// 后台任务详情和新标签页。只接受所属标签的会话身份；页面关闭或隐藏只取消读取，不停止任务。
import { icon } from "./ui/icons.js";
import { element, button } from "./ui/dom.js";

const states = { running:"执行中",completed:"已完成",failed:"失败",stopped:"已停止",cancelled:"已取消",unknown:"状态待核对" };
const ended = s => ["completed","failed","stopped","cancelled"].includes(s);
const title = r => r?.name || r?.command || (/shell|bash|command/.test(r?.kind) ? "后台命令" : /agent/.test(r?.kind) ? "后台子代理" : /monitor/.test(r?.kind) ? "监控" : "后台任务");
const seconds = ms => { const n=Math.floor(ms/1000); return n<60?`${n}秒`:n<3600?`${Math.floor(n/60)}分 ${n%60}秒`:`${Math.floor(n/3600)}时 ${Math.floor(n%3600/60)}分`; };
function facts(row, status, started) {
  const line=element("div","task-facts");
  line.append(element("span",`task-state task-${row.status}`,status || states[row.status] || states.unknown));
  const elapsed=Number.isFinite(row.elapsedMs)?row.elapsedMs:Number.isFinite(row.completedAtMs)&&Number.isFinite(row.startedAtMs)?row.completedAtMs-row.startedAtMs:null;
  if (elapsed!==null && elapsed>=0) line.append(element("span","",seconds(elapsed)));
  const stamp=started?row.startedAtMs:row.completedAtMs;
  if (Number.isFinite(stamp) && stamp>0) line.append(element("time","",`${new Date(stamp).toLocaleString()}${started?" 开始":""}`));
  if (Number.isInteger(row.exitCode)) line.append(element("span","",`退出码 ${row.exitCode}`));
  return line;
}

export function mountTaskLauncher(host, context, choose) {
  const section=element("div","task-launcher"), grid=element("div","task-launcher-grid");
  section.append(element("h2","","打开"),grid);host.append(section);
  const actions=[["changes","查看变更","fileChange"],["files","浏览文件","folder"],["side","创建侧边会话","chat"],["records","任务记录","recent"],["terminal","终端","terminal"]];
  const buttons=actions.map(([id,name,symbol])=>{
    const b=button("",name,()=>{if(!b.disabled)choose(id);},"task-launcher-action");
    const status=element("small",""); const head=element("span","task-launcher-action-head");
    head.append(icon(symbol),status);b.append(head,element("span","",name));grid.append(b);return {id,name,b,status};
  });
  function update(){
    const c=context();
    for(const {id,name,b,status} of buttons){
      const reason=id==="terminal"?"待接入":id==="side"?(c.canCreateSide?"":"当前会话不可创建侧边会话"):
        id==="records"?(!c.threadId || c.taskScopeSupported===false?"当前没有可查看的会话":""):
        (!c.threadId&&!c.projectId?"请先选择项目或会话":"");
      b.disabled=!!reason;b.title=reason?`${name}：${reason}`:name;
      status.textContent=id==="terminal"?reason:"";
    }
  }
  return {open:update, updateContext:update, dispose(){section.remove();}};
}

export function mountTaskView(host,{item,getContext,read,stop,pending,openTask}) {
  const section=element("div","task-view"), heading=element("div","task-view-heading");
  const h=element("h2","",item.title || "任务记录"), body=element("div","task-view-body");
  const note=element("p","task-view-note muted"); note.setAttribute("role","status");
  const action=button("","停止任务",()=>void stopOne(),"task-stop");action.append(icon("stop"),element("span","","停止任务"));action.hidden=true;
  heading.append(h,action);section.append(heading,note,body);host.append(section);
  const id=getContext().threadId, task=item.taskId, isDetail=!!task;
  let visible=false,disposed=false,controller=null,timer=null,epoch=0,latest=null,signature=null,inFlight=false;
  let liveOutput=null,lastScope=null,lastIdentity=null,readFailed=false;
  const scope=()=>{const c=getContext();return JSON.stringify([c.deviceId,c.cacheScope,c.threadId,c.connectionVersion]);};
  function owned(){return !disposed && visible && !document.hidden && getContext().threadId===id;}
  function writable(){const c=getContext();return owned()&&c.connected===true&&!c.readOnly&&!readFailed&&latest?.status==="running"&&latest?.canStop===true&&!!latest?.token;}
  function key(){return JSON.stringify([id,task,latest?.token]);}
  function noteText(s){note.textContent=s||"";note.hidden=!s;}
  function schedule(ms){clearTimeout(timer);if(owned())timer=setTimeout(refresh,ms);}
  function message(text){body.replaceChildren(element("p","task-view-note",text));}
  function field(label,value) {
    if(typeof value!=="string"||!value.length)return;
    const wrap=element("section","task-field");wrap.append(element("h3","",label),element("pre","tool-output",value));body.append(wrap);
  }
  function paint(row){
    const state=pending.get(key());
    const stateLabel=ended(row.status)?null:state?.label || (row.stopRequested?"已请求停止":null);
    h.textContent=title(row);action.hidden=!writable()||!!state;
    body.replaceChildren(facts(row,stateLabel,true));
    field("命令",row.command);
    // 原始输出与摘要独立；实时读取失败或任务刚结束时保留最后成功读到的输出，绝不把它标成最终 stdout。
    if(row.outputKind==="live"&&typeof row.output==="string")liveOutput={text:row.output,truncated:row.outputTruncated};
    const hasOutput=typeof row.output==="string";
    const output=hasOutput?row.output:liveOutput?.text;
    const live=row.status==="running"&&row.outputKind==="live";
    const caption=live?"实时输出":row.outputKind==="final"?"输出":"最近输出";
    field(`${caption}${row.outputTruncated || (!hasOutput && liveOutput?.truncated)?"（已截取）":""}`,output);
    if(live&&!output)body.append(element("p","task-view-note","暂无输出"));
    if(row.summary!==output)field("结果摘要",row.summary);
    noteText([row.outputError,state?.error].filter(Boolean).join("\n"));
  }
  function unavailable(reason){
    readFailed=true;signature=null;action.hidden=true;
    if(latest&&!ended(latest.status)){
      latest={...latest,status:"unknown",canStop:false,outputKind:"recent"};
      paint(latest);
    }
    noteText(reason);
  }
  function records(rows,reason) {
    body.replaceChildren();noteText(reason);
    if(!rows.length){if(!reason)message("暂无已确认的结束记录");return;}
    for(const r of rows){
      const b=button("",title(r),()=>{if(owned())openTask({id:r.id,name:title(r)});},"task-record");
      const line=element("span","task-record-title");line.append(icon("terminal"),element("span","",title(r)));
      b.append(line,facts(r,null,false));body.append(b);
    }
  }
  async function refresh(){
    if(!owned()||inFlight)return;
    if(getContext().connected===false){unavailable("连接已断开，任务状态待核对");return;}
    if(!id || typeof read!=="function"){noteText("当前会话无法读取后台任务");return;}
    const seq=++epoch,identity=scope();const abort=controller=new AbortController();inFlight=true;
    if(!latest && signature===null)noteText("正在读取…");
    try{
      const r=await read(isDetail?"backgroundTaskDetail":"backgroundTaskRecords",{threadId:id,...(task?{taskId:task}:{})},abort.signal);
      if(!owned()||epoch!==seq||scope()!==identity)return;
      if(r?.ok!==true||r.threadId!==id||isDetail&&(r.taskId!==task||r.task!=null&&r.task.id!==task)
          ||!isDetail&&(!Array.isArray(r.records)||r.records.some(x=>!x?.id||!ended(x.status))))throw Error("后台任务响应身份或内容无效");
      if(isDetail){
        const row=r.task;
        if(!row){unavailable(r.reason || "未取得此任务的原生状态");schedule(4500);return;}
        readFailed=false;
        if(latest?.token && row.token && latest.token!==row.token)liveOutput=null;
        const oldKey=key();latest=row;
        if(ended(row.status))pending.delete(oldKey);
        const sig=JSON.stringify([row,pending.get(key()),getContext().readOnly,r.reason]);
        if(signature!==sig){signature=sig;paint(row);}
        if(r.reason && !ended(row.status)){action.hidden=true;noteText(r.reason);}
        if(!ended(row.status)||row.outputPending)schedule(row.outputPending?1000:row.status==="running"?2000:4500);
      }else{
        const sig=JSON.stringify([r.records,r.reason]);
        if(sig!==signature){signature=sig;records(r.records,r.reason);}
        schedule(6000);
      }
    }catch(error){
      if(!owned()||seq!==epoch||abort.signal.aborted)return;
      unavailable(error.message||"任务读取失败");schedule(5500);
    }finally{if(seq===epoch){inFlight=false;controller=null;}}
  }
  async function stopOne(){
    if(!writable()||!stop||pending.has(key()))return;
    const k=key(),token=latest.token, marker={label:"正在请求停止"}; pending.set(k,marker);
    signature=null;paint(latest);
    try{
      const r=await stop({threadId:id,taskId:task,token});
      if(r?.ok!==true||r.threadId!==id||r.taskId!==task||r.status!=="requested")throw Error("停止结果待核对");
      marker.label="已请求停止";
    }catch(e){
      if(e.notSubmitted===true || !e.outcomeUnknown && (e.operationState==="failed" || [400,403,404,409].includes(e.code))){pending.delete(k);marker.error=e.message;}
      else {marker.label="停止结果待核对";marker.error=e.message;}
    }
    if(owned()&&latest?.token===token){signature=null;paint(latest);if(marker.error)noteText(marker.error);void refresh();}
  }
  function pause(){++epoch;controller?.abort();controller=null;inFlight=false;clearTimeout(timer);timer=null;}
  function updateContext(){
    const s=scope(),c=getContext();
    action.hidden=!writable()||pending.has(key());
    const next=JSON.stringify([s,c.connected,c.readOnly]);
    if(lastScope!==next){
      lastScope=next;pause();readFailed=true;signature=null;action.hidden=true;
      if(lastIdentity!==null && lastIdentity!==s){liveOutput=null;latest=null;body.replaceChildren();}
      lastIdentity=s;if(owned())void refresh();
    }
  }
  function visibility(){pause();if(owned())void refresh();}
  document.addEventListener("visibilitychange",visibility);
  return {open(){if(owned()&&!inFlight)void refresh();},updateContext,setVisible(v){if(v===visible)return;visible=v;pause();if(v){lastScope=null;updateContext();}},
    dispose(){disposed=true;pause();document.removeEventListener("visibilitychange",visibility);section.remove();}};
}

export function mountMobileTaskView(workspace,options) {
  // 手机沿用整页与系统返回；返回记录列表或关闭页面不发送任何任务控制。
  const owner=crypto.randomUUID?.() || `task-${Math.random()}`, route="elvesTaskView";
  const panel=element("section","session-tools-screen session-tools-visible task-mobile");
  const head=element("header","session-tools-heading"), label=element("h1","");
  const backButton=button("返回","返回",back,"text-button session-tools-back");
  head.append(label,backButton);const body=element("div","task-mobile-body");panel.append(head,body);
  const inert=[...workspace.children].map(n=>[n,n.inert]);inert.forEach(([n])=>{n.inert=true;});workspace.append(panel);
  let stack=[],view=null,gone=false;
  function paint(){const item=stack.at(-1);if(!item){dispose();return;}
    view?.dispose();body.replaceChildren();label.textContent=item.title || "任务记录";
    view=mountTaskView(body,{...options,item,openTask:t=>open({kind:"task",taskId:t.id,title:t.name})});
    view.setVisible(true);backButton.focus({preventScroll:true});
  }
  function open(item){if(gone)return; stack.push(item);history.pushState({...history.state,[route]:{owner,depth:stack.length}},"");paint();}
  function back(){if(history.state?.[route]?.owner===owner)history.back();else dispose();}
  function pop(e){if(gone)return;
    const target=e.state?.[route];if(target?.owner!==owner)stack=[];else stack=stack.slice(0,target.depth);
    paint();
  }
  function native(e){if(!gone){e.preventDefault();back();}}
  function escape(e){if(!gone&&e.key==="Escape"&&!e.defaultPrevented){e.preventDefault();e.stopPropagation();back();}}
  function dispose(){if(gone)return;gone=true;view?.dispose();panel.remove();inert.forEach(([n,old])=>{n.inert=old;});
    window.removeEventListener("popstate",pop);window.removeEventListener("codex-elves-native-back",native);document.removeEventListener("keydown",escape,true);
    options.onClose?.();}
  window.addEventListener("popstate",pop);window.addEventListener("codex-elves-native-back",native);document.addEventListener("keydown",escape,true);
  return {open,back,dispose,updateContext(){view?.updateContext();}};
}
