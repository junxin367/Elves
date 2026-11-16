import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { setImmediate as nextTurn } from "node:timers/promises";

const client = await readFile(new URL("../apps/elves/web/app.js", import.meta.url), "utf8");
const pwa = await readFile(new URL("../assets/harness/pwa.js", import.meta.url), "utf8");

function startupPage() {
  const calls=[],nodes=new Map(),timers=[];
  for(const id of ["retry","status","version"]) {
    const classes=new Set();nodes.set(`#${id}`,{textContent:"",hidden:false,disabled:false,events:{},addEventListener(name,callback){this.events[name]=callback;},classList:{add:name=>classes.add(name),toggle:(name,on)=>on?classes.add(name):classes.delete(name),contains:name=>classes.has(name)}});
  }
  let resolveState;const pending=new Promise(resolve=>resolveState=resolve);
  const context=vm.createContext({document:{querySelector:selector=>nodes.get(selector)},setInterval:callback=>timers.push(callback),clearInterval(){},window:{addEventListener(){},__TAURI__:{core:{invoke:(command,payload)=>{calls.push({command,payload});return command==="client_startup_state"?pending:Promise.resolve();}}}}});
  vm.runInContext(client,context);
  return {nodes,calls,timers,async load(value){resolveState({version:"0.8.0",starting:false,error:null,...value});await nextTurn();}};
}
test("Elves 启动页不读取旧远程地址，也不要求连接或访问密钥",async()=>{
  const page=startupPage();await page.load({serverUrl:"https://old.example/",autoConnect:true});
  assert.deepEqual(page.calls.map(call=>call.command),["client_startup_state"]);
  assert.equal(page.nodes.get("#retry").hidden,true);
  const html=await readFile(new URL("../apps/elves/web/index.html",import.meta.url),"utf8");
  assert.doesNotMatch(html,/server-url|connection-form|access-key/);
});
test("远程会话关闭不阻挡 Elves 打开本机 Harness",async()=>{
  const page=startupPage();await page.load({remoteAccessEnabled:false,starting:true});
  assert.match(page.nodes.get("#status").textContent,/Elves 工作区/);
  assert.equal(page.nodes.get("#retry").hidden,true);
  assert.ok(page.calls.every(call=>call.command!=="connect_remote"));
});
test("本机初始化失败只显示具体原因和重试，不自动重复启动",async()=>{
  const page=startupPage();await page.load({error:"Harness 端口被占用"});
  assert.equal(page.nodes.get("#status").textContent,"Harness 端口被占用");
  assert.equal(page.nodes.get("#retry").hidden,false);
  assert.equal(page.calls.filter(call=>call.command==="client_retry_startup").length,0);
  await page.nodes.get("#retry").events.click();
  assert.equal(page.calls.filter(call=>call.command==="client_retry_startup").length,1);
});

test("Windows 普通 HTTP 使用原生资源缓存，并在前台请求结束后恢复后台检查", async () => {
  const calls = [], windowEvents = new Map(), timers = [];
  let now = 1000;
  const document = {
    visibilityState: "visible", querySelector: () => null, getElementById: () => null,
    addEventListener() {},
  };
  const host = {
    addEventListener: (name, callback) => windowEvents.set(name, callback),
    dispatchEvent() {},
    __TAURI__: { core: { invoke: (command, payload) => {
      calls.push({ command, payload });
      return Promise.resolve({
        status: command === "client_cache_refresh" ? "waiting" : "ready",
        version: "a".repeat(64), message: "客户端网页资源已缓存",
      });
    } } },
  };
  const context = vm.createContext({
    __CODEX_ELVES_NATIVE_RESOURCES__: { version: 1 }, window: host, document,
    navigator: { onLine: true }, isSecureContext: false,
    location: { protocol: "http:", origin: "http://127.0.0.1:49123", hostname: "127.0.0.1" },
    isAndroidContainer: () => false, Date: class extends Date { static now() { return now; } },
    CustomEvent: class { constructor(type, options) { this.type = type; this.detail = options?.detail; } },
    setTimeout: (callback, delay) => { const timer = { callback, delay }; timers.push(timer); return timer; },
    clearTimeout: timer => { if (timer) timer.canceled = true; },
  });
  vm.runInContext(pwa.replace(/^import .*;\r?\n/gm, "").replace(/^export /gm, ""), context);
  assert.equal(context.getPwaCacheState().status, "ready");
  windowEvents.get("codex-elves-app-ready")();
  await nextTurn();
  assert.equal(calls[0].command, "client_cache_status");
  const release = context.beginPwaForegroundWork();
  assert.equal(calls.at(-1).command, "client_cache_activity");
  assert.equal(calls.at(-1).payload.busy, true);
  release();
  now += 2100;
  timers.findLast(timer => !timer.canceled).callback();
  assert.equal(calls.at(-1).payload.busy, false);
  await context.retryPwaCache();
  assert.equal(context.getPwaCacheState().status, "waiting");
  assert.ok(calls.some(call => call.command === "client_cache_refresh"));
});
