# Elves 与 Harness 更新

Elves 客户端、本机 Harness、协议代理和工作区资源由同一个 `elves.exe` 提供。当前升级入口是 Windows 安装包，旧管理器和 launcher 的服务替换脚本已删除。

## 准备与构建

工作区资源变化时递增 `assets/harness/resource-revision.txt`。共享管理界面变化时先检查并生成 `assets/harness/management/`，再构建内嵌资源的客户端：

```powershell
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run vite:build
cargo fmt --all -- --check
cargo check --workspace
.\scripts\build-windows-installer.ps1
```

核对安装包及便携程序存在，`target/release/elves.exe` 与 `dist/windows/app/elves.exe` 的 SHA-256 一致。构建脚本核对内嵌工作区资源及工具的完整性，只将一个正式程序交给安装包。

仅改代码或构建不执行安装，不切换正在使用的服务，也不发布标签或 Android 安装包。构建成功、安装成功和发布成功分别记录。

## 安装与升级

获得明确的升级要求后运行 `dist/windows/Elves-<版本>-windows-x64-setup.exe`。安装包先通过同一程序的退出模式请求安全回收，再短暂关闭 Elves 及其后台，替换程序并清理旧管理器、启动器、任务看板和快捷方式。共享设置、密钥、设备授权及 WebView 数据保留。

升级后启动 Elves，应直接进入本机工作区。本机入口默认 `45223`，代理默认 `45221`，远程网页默认 `45222`。远程会话可以保持关闭；验证 Elves 和代理时不要求开启网页或公网入口。

## 验收与回退

本机验收核对工作区、代理开关和状态、共享设置、会话及退出回收。启用了网页或 Android 远程会话时，再独立核对实际访问地址、资源版本、原设备授权和会话读取，不能用网页可用代替桌面启动验收。

需要回退时使用已确认兼容的原安装包，保留现有用户数据，重新检查本机和已启用的远程能力。旧版可能不认识新设置；不能为了回退恢复 Codex App 注入或桌面桥接。Android 不接受网页资源号倒退，相关修复使用更高资源号，并按独立发布规则处理原生版本。
