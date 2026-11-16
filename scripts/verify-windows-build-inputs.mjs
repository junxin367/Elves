import {readFileSync,existsSync} from "node:fs";
import {resolve} from "node:path";
import {listHarnessUiResources} from "./harness-resources.mjs";
const root=resolve(process.argv[2]||resolve(import.meta.dirname,".."));
const sourceOnly=process.argv[3]==="--source-only";
const binaryDirectory=resolve(sourceOnly?resolve(root,"target/release-fast"):process.argv[3]||resolve(root,"target/release-fast"));
const read=path=>readFileSync(resolve(root,path),"utf8");
const backend=read("crates/elves-harness/src/lib.rs"),service=read("crates/elves-harness/src/service.rs"),client=read("apps/elves/src-tauri/src/desktop.rs");
const protocol=Number(service.match(/const PROTOCOL_VERSION:\s*u32\s*=\s*(\d+);/)?.[1]);
if(protocol<10||!/\bcontroller\s*\.start_desktop\s*\(/.test(service)||!service.includes("start_monitor"))throw Error("Harness 必须提供独立本机入口和代理生命周期。");
for(const file of ["assets/inject/renderer-features.js","crates/elves-core/src/bridge.rs","crates/elves-core/src/cdp.rs","crates/elves-core/src/launcher.rs"])
 if(existsSync(resolve(root,file)))throw Error(`不应恢复 Codex App 注入代码：${file}`);
if(!client.includes('open_workspace(startup_app)')||client.includes('async fn connect_remote(')||client.includes('saved.remote_access.enabled'))throw Error("Elves 启动不能依赖远程会话连接或开关。");
if(!/\bself\s*\.harness\s*\.call\s*\(action,\s*payload,\s*Some\(authorization\)\)/.test(backend))throw Error("Harness 必须通过独立原生执行引擎处理会话。");
const resources=await listHarnessUiResources(resolve(root,"assets/harness"));
const id=read("crates/elves-core/src/assets.rs").match(/DIAGNOSTIC_BUILD_ID:\s*&str\s*=\s*"([^"]+)"/)?.[1];
if(!id)throw Error("Elves 构建标识缺失。");
if(!sourceOnly){
 const binary=readFileSync(resolve(binaryDirectory,"elves.exe"));
 if(!binary.includes(Buffer.from(id)))throw Error("Elves 程序缺少当前 Harness 构建标识。");
 for(const resource of resources)if(!binary.includes(readFileSync(resolve(root,"assets/harness",resource.file))))throw Error(`Elves 内嵌资源不一致：${resource.file}`);
}
console.log(sourceOnly?`Elves 源码基线核对通过：独立 Harness 协议 ${protocol}，无 Codex App 注入。`:`Elves 安装输入核对通过：${resources.length} 个 Harness 资源和构建标识一致。`);
