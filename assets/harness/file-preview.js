import { fitMediaPreview, bindMediaPreviewDismiss, openVideoPreview } from "./attachments.js";
// 工作区预览只接受服务端声明的结构；本页不提供原文件下载，也不把远程路径当作宿主机命令。
const tag = (name, text = "") => Object.assign(document.createElement(name), { textContent: text });
const button = (name, act, disabled = false) => {
  const b = tag("button", name); b.type = "button"; b.className = "text-button"; b.disabled = disabled;
  b.addEventListener("click", act); return b;
};
const uint = n => Number.isSafeInteger(n) && n >= 0;
export function validFileWindow(v) {
  return uint(v.size) && uint(v.start) && uint(v.end) && uint(v.first) && v.first <= v.start && v.start <= v.end
    && v.end <= v.size && v.end - v.start <= 256 * 1024 && typeof v.text === "string" && v.text.length <= 256 * 1024;
}

// 每个片段只挂载当前视窗前后各 10 行；超长单行也切成显示行，不生成整文件 DOM 或拼接历史正文。
export function virtualSource(body, text, { absolute = false, wrap = false, line = null, decorate = null } = {}) {
  const pane = tag("div"); pane.className = "session-files-source file-window-source";
  const sample = tag("div", "M"); sample.className = "session-files-source-line";
  pane.append(sample); body.append(pane);
  const height = Math.max(16, sample.getBoundingClientRect().height || 20);
  const width = Math.max(16, Math.floor(((body.clientWidth || 700) - 78) / 10));
  const cap = wrap ? width : 4096;
  // 几百万个短行不能分配几百万个 JS 数组和子字符串。按块保存 Uint32 位置索引，只切出正在绘制的文字。
  const offsets=[], numbers=[], blockSize=4096;
  let total=0, selected=-1;
  const offset=i=>offsets[Math.floor(i/blockSize)][i%blockSize];
  const number=i=>numbers[Math.floor(i/blockSize)][i%blockSize];
  for(let pos=0,n=1;pos<=text.length;n++){
    const nl=text.indexOf("\n",pos), end=nl<0?text.length:nl;
    if(n===line)selected=total;
    let j=pos;
    do {
      const idx=total%blockSize;
      if(idx===0){offsets.push(new Uint32Array(blockSize));numbers.push(new Uint32Array(blockSize));}
      offsets.at(-1)[idx]=j;numbers.at(-1)[idx]=n;total++;
      let next=Math.min(j+cap,end);
      if(next<end && text.charCodeAt(next-1)>=0xd800 && text.charCodeAt(next-1)<=0xdbff)next--;
      j=next;
    }while(j<end);
    if(nl<0)break;
    pos=end+1;
  }
  // Chromium 的布局高度有上限；过多短行按比例映射滚动条，末尾仍可达。
  const extent=Math.min(total*height,8_000_000);
  const visible=()=>Math.max(1,Math.ceil((body.clientHeight||600)/height));
  const topOf=idx=>total<=visible()?0:idx*(extent-(body.clientHeight||600))/(total-visible());
  const paint = () => {
    const shown=visible(), viewTop=Math.min(Math.max(0,total-shown),Math.floor(body.scrollTop/Math.max(1,extent-(body.clientHeight||600))*Math.max(0,total-shown)));
    const first=Math.max(0,viewTop-10), end=Math.min(total,viewTop+shown+11);
    const padding=Math.max(0,topOf(viewTop)-(viewTop-first)*height);
    pane.style.paddingTop=`${padding}px`;pane.style.paddingBottom=`${Math.max(0,extent-padding-(end-first)*height)}px`;
    const doc = document.createDocumentFragment();
    const state={blockComment:false,quote:""};
    for (let i=first;i<end;i++) {
      const n=number(i), head=i===0||number(i-1)!==n;
      let right=i+1<total?offset(i+1):text.length;
      if(right>offset(i)&&text[right-1]==="\n")right--;
      if(right>offset(i)&&text[right-1]==="\r")right--;
      const s=text.slice(offset(i),right),row=tag("div"),label=tag("span",head?`${absolute?"":"+"}${n}`:"");
      row.className = "session-files-source-line"; label.className = "session-tools-line-number";
      row.style.height=`${height}px`; row.style.whiteSpace="pre"; const code=tag("code"); code.style.whiteSpace="pre";
      code.append(decorate && s.length ? decorate(s,state) : s||"\u200b");
      if (absolute && head) row.dataset.line=n;
      row.classList.toggle("session-files-selected-line",absolute && line===n); row.append(label,code);doc.append(row);
    }
    pane.replaceChildren(doc);
  };
  body.addEventListener("scroll",paint,{passive:true});paint();
  if(absolute && selected>=0){body.scrollTop=Math.max(0,topOf(selected));paint();}
  return ()=>body.removeEventListener("scroll",paint);
}

export function renderTextWindow(v, body, toolbar, load, line) {
  const go=(direction,offset)=>load({direction,...(offset==null?{}:{offset})});
  toolbar.hidden=false; toolbar.replaceChildren(
    button("开头",()=>go("head"),v.start===v.first),button("上一段",()=>go("prev",v.start),v.start===v.first),
    tag("span",`${v.start.toLocaleString()}–${v.end.toLocaleString()} 字节`),
    button("下一段",()=>go("next",v.end),v.end===v.size),button("末尾",()=>go("tail"),v.end===v.size));
  return virtualSource(body,v.text,{absolute:v.start===v.first,line});
}

const resourcePattern=/^\/api\/workspace\/media\/[a-f0-9]{64}$/u;
export const validResource=v=>typeof v.resource==="string" && resourcePattern.test(v.resource);
const rasterData=s=>typeof s==="string" && s.length<=12*1024*1024 && /^data:image\/(?:webp|png|jpeg);base64,[a-zA-Z0-9+/=]+$/u.test(s);
export function validFormat(v) {
  if(v.kind==="picture")return validResource(v)&&/^image\/(?:png|jpeg|webp|gif)$/u.test(v.mime)&&
    uint(v.width)&&uint(v.height)&&v.width>0&&v.height>0&&v.width*v.height<=16000000&&
    typeof v.localOriginal==="boolean"&&(v.thumbnail==null||rasterData(v.thumbnail));
  if(v.kind==="pdfPage")return uint(v.page)&&uint(v.pages)&&v.page<v.pages&&rasterData(v.dataUrl);
  if(v.kind==="archive")return uint(v.page)&&uint(v.pages)&&v.page<v.pages&&uint(v.count)&&v.count<=4096&&
    Array.isArray(v.items)&&v.items.length<=50&&v.items.every(e=>e&&typeof e.name==="string"&&e.name.length<=1024&&uint(e.size)&&typeof e.directory==="boolean");
  return false;
}
export function formatNav(v,bar,load) {
  const p=v.paging, pages=v.pages ?? null, index=p?.page??v.page??0, sheet=p?.sheet??0;
  if(!p && !["pdfPage","archive"].includes(v.kind) && !v.pdfTextMode && !v.pdfTextAvailable && !v.tableTextAvailable)return;
  bar.hidden=false;
  if(v.pdfTextMode) {bar.append(button("查看原页",()=>load({page:0})));return;}
  if(v.pdfTextAvailable){bar.append(button("提取文字",()=>load({representation:"text"})));return;}
  if(v.tableTextAvailable){bar.append(button("按文本查看",()=>load({representation:"text"})));return;}
  if(p?.names?.length>1) {
    const select=tag("select");select.setAttribute("aria-label","选择工作表");
    for(let i=0;i<p.names.length;i++){const opt=tag("option",p.names[i]);opt.value=String(i);select.append(opt);}
    select.value=String(sheet);select.onchange=()=>load({sheet:Number(select.value),page:0});bar.append(select);
  }
  bar.append(button("上一页",()=>load({sheet,page:index-1}),index===0),
    tag("span",pages?`${index+1} / ${pages}`:`第 ${index+1} 页`),
    button("下一页",()=>load({sheet,page:index+1}),p ? !p.more : index+1>=pages));
  if(v.kind==="pdfPage"&&v.textAvailable)bar.append(button("提取文字",()=>load({representation:"text"})));
}
export function mountPicture(v,body) {
  const img=tag("img");img.alt=v.path;img.className="file-page-picture";
  let enlarged=null;
  if(v.kind==="pdfPage") {img.src=v.dataUrl;body.append(img);return ()=>{img.removeAttribute("src");};}
  if(v.localOriginal || v.thumbnail){img.src=v.localOriginal?v.resource:v.thumbnail;body.append(img);}
  else body.append(tag("p","没有可用的静态缩略图。"));
  const open=button("查看原图",()=>{
    if(enlarged)return;
    const d=tag("dialog"),panel=tag("div"),full=tag("img"),close=button("关闭",()=>d.close());
    d.className="image-preview-dialog";d.setAttribute("aria-label","原图");panel.className="video-preview-panel";close.className="image-preview-close";
    full.alt=v.path;full.src=v.resource;panel.append(close,full);d.append(panel);document.body.append(d);d.showModal();
    const layout=fitMediaPreview(d,panel,full,()=>({width:v.width,height:v.height}));
    const dismiss=bindMediaPreviewDismiss(d,full);
    enlarged=()=>{layout.dispose();dismiss();full.removeAttribute("src");d.remove();enlarged=null;};
    d.onclose=()=>enlarged?.();full.onerror=()=>{full.alt="原图载入失败，请重新打开文件核对授权";};layout.resize();
  });
  img.onclick=()=>open.click();body.append(open);
  return ()=>{img.removeAttribute("src");enlarged?.();};
}
export function renderArchive(v,body) {
  const list=tag("dl");list.className="file-archive-list";
  for(const item of v.items){list.append(tag("dt",item.name),tag("dd",item.directory?"目录":`${item.size.toLocaleString()} B`));}
  if(!v.count)list.append(tag("dt","空压缩包"));body.append(list);
}
export function mountFileMedia(v, body, view, invalidate) {
  const video=v.mime.startsWith("video/"), caption=video?"播放视频":"播放音频";
  let active=null, retired=false;
  const trigger=button(caption,()=>{
    if(retired || active || !validResource(v))return;
    if(video) {
      const controller=new AbortController();
      const handle=openVideoPreview(trigger,v.resource,null,v.path,()=>{},{
        position:view.mediaVersion===v.fileVersion?view.mediaTime:0,
        onStop: time=>{if(Number.isFinite(time)){view.mediaTime=time;view.mediaVersion=v.fileVersion;}},
        onClose: ()=>{controller.abort();active=null;},
        onFailure: async code=>{
          try {
            const reply=await fetch(v.resource,{method:"HEAD",credentials:"same-origin",cache:"no-store",redirect:"error",signal:controller.signal});
            if(controller.signal.aborted)return "";
            if([401,403,404,409,410].includes(reply.status)){active?.stop();invalidate(reply.status);return "";}
            if(reply.status===429)return "当前设备已有两个播放请求，请暂停其他媒体后重试。";
            return code===4?"当前设备不支持这种视频格式或编码。":code===3?"视频解码失败，可能已损坏或编码不受支持。":"播放中断，请检查线路后重试。";
          } catch {return "播放线路暂不可用，请重试。";}
        }
      });
      if(handle)active={stop:()=>{controller.abort();handle.close();}};
      return;
    }
    const dialog=tag("dialog");dialog.className="image-preview-dialog video-preview-dialog file-media-dialog";dialog.setAttribute("aria-label",caption);
    const panel=tag("div");panel.className="video-preview-panel";
    const close=button("×",()=>dialog.close());close.className="image-preview-close";close.setAttribute("aria-label","关闭预览");
    const media=tag("audio");media.controls=true;media.preload="none";media.playsInline=true;
    media.disablePictureInPicture=true;media.disableRemotePlayback=true;media.setAttribute("controlsList","nodownload noremoteplayback");
    media.className="file-audio-player";
    media.addEventListener("contextmenu",e=>e.preventDefault());
    const status=tag("p","正在加载…");status.setAttribute("role","status");status.className="video-preview-status";
    const controller=new AbortController();
    let unbind=null,alive=true;
    const remember=()=>{if(Number.isFinite(media.currentTime) && media.currentTime>0 && !media.ended){
      view.mediaTime=media.currentTime;view.mediaVersion=v.fileVersion;
    }};
    const stop=()=>{
      if(!alive)return;alive=false;controller.abort();remember();
      media.pause();media.removeAttribute("src");media.load();unbind?.();dialog.remove();active=null;
    };
    active={dialog,stop};
    dialog.addEventListener("close",stop,{once:true});
    panel.append(close,media,status);dialog.append(panel);document.body.append(dialog);dialog.showModal();
    unbind=bindMediaPreviewDismiss(dialog,media);
    media.addEventListener("loadedmetadata",()=>{if(view.mediaVersion===v.fileVersion&&view.mediaTime>0)media.currentTime=Math.min(view.mediaTime,media.duration||view.mediaTime);});
    media.addEventListener("playing",()=>{status.textContent="";});
    media.addEventListener("timeupdate",remember);
    media.addEventListener("ended",()=>{view.mediaTime=0;});
    media.addEventListener("error",async()=>{
      if(!alive)return;
      remember();
      let reason=media.error?.code===4?"当前设备不支持这种媒体格式或编码。":media.error?.code===3?"媒体解码失败，可能已损坏或编码不受支持。":"播放中断，请检查线路后重试。";
      try {
        const check=await fetch(v.resource,{method:"HEAD",credentials:"same-origin",cache:"no-store",redirect:"error",signal:controller.signal});
        if(!alive)return;
        if([401,403,404,409,410].includes(check.status)){stop();invalidate(check.status);return;}
        if(check.status===429)reason="当前设备的播放请求过多，请暂停其他媒体后重试。";
      } catch{if(!alive)return;}
      status.textContent=reason;status.append(button("重试",()=>{stop();trigger.click();}));
    });
    // 只在明确点播放后设置同源资源地址；无自动跨 Origin 兜底或下载代替播放。
    media.src=v.resource;void media.play().catch(()=>{if(alive&&!media.error)status.textContent="请点击播放继续。";});
  });
  body.append(trigger);
  const hidden=()=>{if(document.hidden || !body.isConnected || body.closest("[hidden],[inert]"))active?.stop();};
  const watcher=typeof MutationObserver==="function"?new MutationObserver(hidden):null;
  for(let e=body;e&&watcher;e=e.parentElement)watcher.observe(e,{attributes:true,attributeFilter:["hidden","inert","class"]});
  document.addEventListener("visibilitychange",hidden);
  return ()=>{retired=true;watcher?.disconnect();document.removeEventListener("visibilitychange",hidden);active?.stop();};
}
