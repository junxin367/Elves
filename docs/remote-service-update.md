# Elves 与 Harness 更新

Elves 客户端、本机 Harness 和工作区资源由 `elves.exe` 提供；代理管理器由独立的 `elves-manager.exe` 提供，协议代理可由 Harness 或管理器启动为独立服务。当前升级入口是 Windows 安装包，旧 CodexElves 管理器和 launcher 的服务替换脚本已删除。

## 准备与构建

工作区资源变化时递增 `assets/harness/resource-revision.txt`。代理管理器界面变化时先构建独立管理器前端，再构建管理器 Rust 壳：

```powershell
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run build:manager
cargo fmt --all -- --check
cargo check --workspace
.\scripts\build-windows-manager.ps1
```

核对 `target/release/elves.exe`、`target/release/elves-manager.exe` 及 `dist/windows/app/` 中两份程序存在。构建脚本核对 Elves 内嵌工作区资源及工具的完整性，安装包同时安装客户端与独立代理管理器。

仅改代码或构建不执行安装，不切换正在使用的服务，也不发布标签或 Android 安装包。构建成功、安装成功和发布成功分别记录。

## 安装与升级

获得明确的升级要求后运行 `dist/windows/Elves-<版本>-windows-x64-setup.exe`。安装包通过同一程序的退出模式关闭 Harness 后再替换程序，一律放行：Codex 仍在执行时，任务留在 Elves 私有 Codex 服务里继续，新版本启动后自动接上；没有执行中的会话时 Codex 一并关闭。等待审批的轮次、随退出中断的任务和已保存待续的操作逐条写入安装日志。共享设置、密钥、设备授权及 WebView 数据保留。判定口径及安装日志的逐条提示，以[产品行为与交互规范](product-standards/core/product-behavior.md#elves-安装启动与-harness)中托盘退出与安装升级的约定为准。

关停判定由正在运行的旧版 Harness 做出，但不再决定能否安装：安装程序记下结论后一律自己关闭 Elves，不要求用户先退出。从尚未使用私有服务的旧版升级时，旧版的 Codex 是 stdio 子进程，执行中的任务无法交接，会随旧版一起中断；旧版也可能把没有执行的会话误判为执行中，安装日志会列出它的判断。完成这次升级后，执行中的 Codex 由 Elves 私有服务承载。本地代理与客户端是同一个 `elves.exe`，安装期间必须结束，经本地代理调用模型的任务在这段时间可能报错结束。

升级后启动 Elves，应直接进入本机工作区。本机入口默认 `45223`，代理默认 `45221`，远程网页默认 `45222`。远程会话可以保持关闭；验证 Elves 和代理时不要求开启网页或公网入口。

## 验收与回退

本机验收核对工作区、代理开关和状态、共享设置、会话及退出回收。启用了网页或 Android 远程会话时，再独立核对实际访问地址、资源版本、原设备授权和会话读取，不能用网页可用代替桌面启动验收。

需要回退时使用已确认兼容的原安装包，保留现有用户数据，重新检查本机和已启用的远程能力。旧版可能不认识新设置；不能为了回退恢复 Codex App 注入或桌面桥接。Android 不接受网页资源号倒退，相关修复使用更高资源号，并按独立发布规则处理原生版本。
