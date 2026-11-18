# Windows 本机工作区

Elves 是 Windows x64 桌面程序，需要 Microsoft Edge WebView2 Runtime。安装包只部署一个 `elves.exe` 及卸载程序，桌面和开始菜单只提供“Elves”。同一程序内置 Harness 后台，不需要单独安装管理器、启动器或工具程序。

## 启动与本机工作区

启动 Elves 后，客户端自动启动本机 Harness，并直接打开工作区。客户端不读取或恢复远程地址，不显示连接页，不要求访问密钥，也不自动开启远程会话。只有本机初始化失败时才显示实际原因和“重试”。

Harness 本机工作区默认只监听 `127.0.0.1:45223`，与网页远程入口的默认端口 `45222` 分开。客户端通过本机私有管理接口取得 30 秒有效的一次性证明，原生端完成授权并写入仅供本机使用的 HttpOnly Cookie；证明和 Cookie 不进入 URL 或网页脚本。该身份不加入远程设备列表，远程会话关闭、只读、换密钥、登录锁定和撤销设备均不影响 Elves。

当前已启用供应商打开“启用本地代理”后，Harness 随 Elves 启动代理，默认监听 `45221`。保存代理开关后运行中的后台也会同步状态；远程会话开关不控制代理。启动失败在代理设置中显示原因。固定工具内嵌客户端，按 SHA-256 校验后释放到用户状态目录，安装目录不另放工具程序。

## 工作区与设置

从左侧[主导航](../core/product-behavior.md#统一左侧主导航)进入会话、代理设置、会话管理或远程会话。底部“设置”直接打开设置页；[设置规范](../core/product-behavior.md#远程设置分类与实例配置)区分本机外观与同一 Harness 共享的设置。

文件、变更和关联会话可在[右侧标签面板](../sessions/workspace-tools-and-side-sessions.md#菜单入口与页面层级)打开；独立会话窗口与文件预览窗口共用主窗口的 WebView2 Environment 和本机授权，关闭窗口只关闭自身。需要启动宿主机应用时使用[本机 Open in](../core/product-behavior.md#open-in-的应用识别与前台显示)。

## 原生窗口与生命周期

原生顶栏高 36 个逻辑像素，随 DPI 缩放，背景、文字及分隔线跟随工作区实际颜色。窗口支持拖动、缩放、最小化、最大化和还原，并恢复上次位置和大小。主窗口关闭收进托盘，再次启动或点击托盘唤回原窗口。托盘提供打开、收起、最大化或还原、刷新和退出，不提供连接设置。

主窗口标题固定为“Elves”；独立会话窗口标题为“<会话标题> · Elves”，文件窗口显示文件名及可选的“变更”。所有窗口和原生顶栏在主界面线程创建，页面不获得窗口控制命令。

任务栏角标按未读主会话 ID 去重，侧边会话、子代理和归档不另计，搜索和折叠不改变总数，超过 99 显示 `99+`。仅主窗口上报数量。

托盘退出安全回收本客户端拥有的 Harness、代理和隧道；原生会话仍沿用既有安全停止规则。升级先请求安全退出，再替换程序并移除旧程序和快捷方式；卸载保留设置、历史、授权及缓存。

## 数据与资源

共享数据保持在 `~/.codex-session-delete/`。客户端默认使用 `%LOCALAPPDATA%\com.elves.app\`；若新目录不存在且旧 `%LOCALAPPDATA%\com.codexelves.remote\` 已存在，则沿用旧目录以保留窗口和 WebView 数据。`settings.json` 只保存窗口状态，不保存服务地址。

开发验收可用绝对路径环境变量 `ELVES_DATA_DIR` 隔离客户端数据，用 `ELVES_STATE_DIR` 隔离 Harness 状态。两者须同时设置，不能用隔离客户端目录代替隔离后台设置。

客户端内置完整公开网页包，按 Origin 保存通过清单、大小和 SHA-256 验证的当前及上一版资源。完整更新在下次导航生效，不混换当前页面的资源。API、上传、授权和原生脚本不进入缓存；接管 Service Worker 时不清除 Cookie、外观、草稿或 IndexedDB。

## 构建

```powershell
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run vite:build
.\scripts\build-windows-installer.ps1
```

安装包产物为 `dist/windows/Elves-<版本>-windows-x64-setup.exe`；同一程序的便携副本为 `dist/windows/Elves-<版本>-windows-x64.exe`。仅构建程序可运行 `scripts/build-windows-client.ps1`，不会附带 Android 构建或安装。版本取根 `Cargo.toml`。

源码位于 `apps/elves/`，Harness 位于 `crates/elves-harness/`。项目不再包含 Codex App 注入、CDP、renderer 脚本或桌面执行桥接。
