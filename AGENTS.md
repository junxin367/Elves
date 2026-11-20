# Elves 项目工作规则

## 适用范围

本文件适用于整个仓库，覆盖协议代理、Elves 工作区与独立 Harness、安装包构建、GitHub 发版及发布验收。

## 核心原则

保留 Elves 项目命名、本地代理及 Elves 工作区与独立 Harness。管理功能从 Elves 主导航进入，不恢复独立管理器窗口或安装入口。

## 需求提交与推送

- 只有用户在当前需求中明确要求提交或推送时，才执行 `git commit` 或 `git push`；未明确要求时保留工作区改动，不主动提交或推送。
- 每次提交只包含对应需求的改动，不将多个需求合并到同一提交；保留用户已有的无关改动。
- 提交前核对当前分支、远端、有效 Git 作者和提交者身份及暂存差异，沿用中文提交规范，不使用 AI 身份或署名。
- 用户明确要求推送时，推送后核对远端分支与本地提交一致；未执行提交或推送时，准确报告已完成步骤和工作区状态。

## 产品规范

- 设计、开发、修改、审查或验收界面布局、视觉样式、控件或交互行为前，必须先读根目录 [Web UI 与交互设计规范](DESIGN.md)，再按 [产品规范索引](docs/product-standards/README.md) 阅读 Elves 主规范及相关专题的对应章节。仅打开索引不算完成阅读。
- 新增或调整产品交互约定时，更新索引所指的对应主规范章节，并核对实现处的需求注释；其他文档引用该章节，不另写一套规则。

## 远程会话禁止桥接 Codex App（强制）

- 本约束适用于远程会话网页、Elves Windows 客户端、Android 及其服务端的全部能力，包括只读查询、实时订阅、发送、审批、停止、删除和异常恢复。
- 远程会话必须独立于 Codex App 运行。允许独立服务启动和使用 Codex 原生 CLI / app-server 及其受支持接口，这不属于桌面桥接；不得复用或接管 Codex App 持有的连接、内部对象或执行上下文。
- 禁止通过 CDP、浏览器或页面注入、React / renderer 状态访问、桌面 owner / follower IPC、转发到本地管理器等方式，调用或驱动 Codex App 来提供远程能力。只读、单独封装接口或仅作为兜底，也不能绕过此约束。
- 不得以“修复删除占用”“让持有者代删或释放”“已有本地原生入口”“兼容旧版本”等理由恢复桌面桥接。独立服务缺少安全可用的原生能力时，明确向用户说明已验证的限制、未解决的场景及缺失接口；不得把错误提示或回执加固报告为功能问题已解决。
- 排查、设计、实现和验收均须遵守本约束。发现历史代码、文档或记录仍描述远程 Codex App 桥接时，以本条为准，将其作为需要核对的遗留内容，不得据此恢复桥接，也不得仅凭规则已写入就声称现有实现已经全部移除桥接。

## 用户特定要求的代码注释

- 实现或修改用户明确指定的行为、尺寸、布局、能力优先级、兼容或降级边界时，必须在对应代码、样式或配置附近添加简体中文注释，说明用户要求、适用条件和必须保留的约束；不能只在项目文档或提交信息中记录。
- 涉及固定数值、计算公式或多个实现位置联动时，注释必须说明数值含义、计算依据及需要同步核对的位置。例如，过期提示块与完整输入框等高时，须说明输入区、工具栏、内边距、边框和手机断点的关系。
- 重构、迁移或优化相关代码时，须保留这些需求注释并核对原要求；需求经用户调整后，同步更新实现和注释，避免留下过期说明。
- 注释应简短并解释需求意图，不逐行复述代码，不写入密钥、用户消息等敏感内容；交付前检查本次涉及的用户特定要求是否已在对应实现处注明。

## 临时截图与效果稿

- 仅用于调试、效果确认或临时验收的截图、效果图和录屏保留在本机 `temp/`，不纳入 Git 提交或推送；“提交所有代码”也不包含这些临时材料。
- 提交前检查新增图片及录屏，排除临时预览产物。正式产品资源及用户明确要求入库的文档配图按其用途处理。
- 验收文档保留文字结果，不引用未入库的临时图片；已有本地截图不因排除提交而删除。

## 产品与架构命名

- 产品名统一为 `Elves`，仓库为 `https://github.com/junxin367/Elves`。
- Windows 只安装一个 `elves.exe`，桌面、开始菜单和安装完成页只提供 Elves 程序入口；后台由同一程序的 `--harness-service` 模式承载。
- `Harness` 是独立执行与管理产品，代码位于 `crates/elves-harness/`；远程会话只是它的网页和 Android 访问能力，不得用它称呼 Elves 或整体架构。
- `crates/elves-core/` 提供配置与协议代理基础能力，`crates/elves-data/` 提供会话数据访问；`apps/elves/` 为原生客户端，`apps/elves-management/` 仅为共享管理前端，`assets/harness/` 为工作区资源。
- Elves 启动必须直接打开本机 Harness，不显示远程地址或配对页，不读取上次远程地址，不自动修改远程会话开关。只有本机初始化失败时显示原因和重试。
- Harness 本机入口默认回环端口 `45223`，本地代理默认 `45221`，网页默认 `45222`。禁止远程监听占用本机或代理端口。远程关闭、只读、换密钥、撤销设备及公网变化不能影响 Elves 本机授权、执行或代理。
- 本地代理由当前已启用供应商的开关控制；开关已开启时必须随 Elves / Harness 启动，不能依赖远程会话已开启。
- 项目不再提供 Codex App 注入。禁止恢复 launcher、CDP、renderer 脚本、桌面桥接或其功能开关；允许通过独立原生 CLI / app-server 执行、只读解析项目元数据和检查会话占用。
- 旧数据目录、已发布协议字段、设备 Cookie、Android applicationId 与签名材料可保留兼容名称，不作为当前产品名；升级清理旧程序和快捷方式，保留设置、历史、授权和缓存。
- Windows 产物为 `target/release/elves.exe`、`dist/windows/app/elves.exe`、`dist/windows/Elves-<version>-windows-x64-setup.exe` 和同一程序的便携副本 `dist/windows/Elves-<version>-windows-x64.exe`。
- 历史发布与验收记录按当时事实保留，不批量重写历史。

## 产品行为约束

开发和维护时必须保持以下行为：

- Windows 客户端及共享管理库版本号以根 `Cargo.toml` 的 `workspace.package.version` 为准。Android 版本独立维护于 `apps/elves-android/version.properties`，不随 Windows 版本号同步。普通编译不调整 Android 版本；桌面发版需要同步发布 Android 时，按下方发布规则递增 Android 独立版本和 `versionCode`，并保持现有正式签名证书。
- 默认本地协议代理端口保持 `45221`。
- Windows 客户端关闭主窗口时收进托盘，再次启动唤回原窗口，并保持上次窗口大小、位置及显示器。
- Elves 本机主导航固定为会话、代理设置、会话管理、远程会话；网页、PWA、Android 仅显示会话入口，不开放任何管理页面、供应商/API 配置、本地代理、请求日志或相应管理接口。服务端必须核对本机监听入口及真实本机授权，不能只隐藏按钮或信任客户端标记。导航宽 52px，按钮 36px，左右各 8px，设置固定底部。
- 会话管理只保留浏览、筛选及单个/批量删除，不恢复历史会话修复和上下文压缩入口。
- 各端设置与退出入口遵循[产品规范的主导航约定](docs/product-standards/core/product-behavior.md#统一左侧主导航)。
- 快捷入口中文不能乱码；NSIS 编译必须使用 `/INPUTCHARSET UTF8`。
- 产品 logo 以 `assets/harness/icons/elves-logo.svg` 为唯一源，网页和文档优先直接使用 SVG；原生图标用 `npm --prefix apps/elves-management run assets:brand` 生成，资源按实际用途命名。项目及 Codex、Claude 实例图标继续使用原有资源。

## 协议代理和模型配置约束

不得破坏当前协议代理设计：

- 用户通过“启用本地代理”显式开关控制代理，是否走代理以该开关为准。
- 模型列表按协议分别维护：Responses API、Chat Completions、Anthropic。
- 展示模型列表时聚合排序，但配置仍保留协议归属。
- Responses API 模型请求不得被翻译成 Chat Completions；必须直接调用 Responses 上游 Base URL。
- Chat Completions 和 Anthropic 转换逻辑必须保留，并用协议测试覆盖。
- 显式模型协议配置优先，不得批量改写已有模型的显式协议；缺少归属时按模型名称推断：GPT/OpenAI 系列使用 Responses API，Claude 和其他非 GPT 模型默认使用 Anthropic。空模型名不能用于协议推断。
- 思考深度按模型能力处理：GPT 类最高 `xhigh`，Claude 类最高 `max`，其他模型按能力表处理，不能用单一固定等级覆盖全部模型。
- 保留常见模型上下文大小表；模型列表新增或加载模型时优先使用已填写值和已知表值，仍缺少上下文大小时默认填入 `1000000`，不得覆盖已有手动值。
- 模型配置 UI 保持表格交互：请求模型、协议、上下文大小、删除 icon；协议为下拉选择。

## Claude 实例测试规则

- 测试 Claude 实例时，使用本项目的 `codex-elves-claude-test` 测试实例，并选择 `gpt 6.1 sol` 模型。
- 使用其他 Claude 实例复现测试时，默认选择 `haiku 5.5` 模型。

## Claude 官方信息获取约束

- 本项目获取、展示或缓存任何 Claude 官方信息时，包括账号状态、订阅类型与等级、额度、重置时间、限流、模型及能力、Fast 状态和官方计费信息，必须通过目标会话所属 WSL 发行版、用户及配置的原生 Claude CLI 获取。新建会话尚无会话身份时，使用用户明确选中的 Claude 实例；不得借用本机 Claude、其他 WSL 实例或其他用户。
- 官方接口访问必须由该 WSL 内的 Claude CLI 自行完成。禁止项目代码、脚本、SDK、浏览器自动化或第三方服务绕过 CLI，直接请求 Claude / Anthropic 的官方接口或抓取官方账号页面；即使在 WSL 中执行 `curl`、Python HTTP 请求或复刻 CLI 网络请求，也属于绕过，禁止使用。
- 禁止提取、复制或复用 Claude 的 Cookie、OAuth Token、API Key、登录缓存及其他凭据来查询官方信息。SDK 或桥接器只有在实际调用仍经过上述同一 WSL Claude CLI 时才可使用，不能以“SDK 官方提供”或“只读查询”为由绕过。
- 通过 WSL 启动 Claude 或查询 Claude 官方信息时，必须让 Claude 进程继承目标 WSL 用户的有效时区；优先使用目标发行版、用户和 Home 目录的登录环境启动，例如 `wsl.exe --distribution <distribution> --user <user> --cd "~" --exec /bin/bash -lc 'exec "$@"' -- <claude-executable> ...`，或使用等效的显式 `TZ` 传递方案。仅使用 `wsl.exe --exec <claude-executable>` 不算满足要求，因为它可能绕过 `/etc/profile` 中的时区设置。
- 查询或展示重置时间前，必须在同一发行版、同一用户、同一启动路径中核对 `TZ`、`date '+%Y-%m-%d %H:%M:%S %Z %z'` 及必要时的 `/etc/localtime`；原始 CLI 返回的 UTC/带偏移时间只能按该 WSL 的已验证时区转换。禁止使用 Windows 宿主机时区、Agent 进程时区或其他 WSL 实例的时区替代目标 WSL 时区。
- 官方信息查询必须将工作目录固定在目标 WSL 用户的 Home 目录，不得把本机项目目录、宿主机环境变量、Hooks、MCP 或会话上下文带入查询；使用 `--no-session-persistence`、禁用 Hooks/MCP 等隔离参数时，仍须保留同一 WSL CLI、用户和授权范围。
- 如果目标 WSL 的有效时区无法核对，或登录环境与系统时区不一致且启动路径未明确采用哪一个，停止转换并将时区标为未知/受阻；不得默认写成 `Asia/Shanghai`、UTC 或任何固定时区。
- CLI 不支持查询、没有返回目标字段、不可用或读取失败时，将对应信息标为未知或不可获取，并主动向用户说明缺失的信息、已确认的失败原因及功能影响；原因尚未确认时如实说明，不得静默失败、只写日志或仅显示“未知”。不得改走其他来源补齐，也不得根据使用量、百分比、文档映射或本地估算推断为官方等级、额度或计费结果。
- 官方信息缓存只能复用同一实例、同一授权范围内且仍有效的 CLI 原始结果，保留原采集时间和额度窗口，不得通过重新缓存延长有效期。新增或修改读取逻辑时，核对实际 WSL 发行版、用户、CLI 可执行文件、原生查询方法和返回字段；未完成真实 CLI 验证时，明确区分“代码已接入”与“已实际获取”。

## 远程会话发送与显示约束

- 普通消息只有在明确确认当前会话正在执行时才允许入队；空闲、未知状态或仅有旧队列残留时应走直接发送。异步准备后再次核对真实执行状态，不能把本次发送自身的启动占位误认为另一条正在执行的任务。
- 发送中、引导中以及仅确认入队的消息不得提前插入会话正文；正文展示必须来自成功回执或同一原生消息的已接收证据。结果未知时保留原回执，不展示假成功，也不自动重发。
- 已确认并展示的消息不得因空队列、迟到快照或历史暂未同步而消失。原生消息到达后按稳定消息身份接替并去重；只有明确的编辑回退、会话删除等操作可以移除对应内容。
- PC 网页和 Elves Windows 客户端 的文件、变更在右侧可调宽的多标签面板打开，不能覆盖或禁用聊天。列表与各详情可同时保留，重复目标复用标签；按会话/新建项目分别保留标签内容、选中项、显隐、宽度及阅读位置，切换时隐藏并在切回时恢复，不重载侧边 iframe。更换授权或关闭页面时释放全部标签与请求，后台读取不得借用当前会话身份。手机和 Android 容器沿用原整页导航。

## 构建约定

### Harness 与网页资源更新

Harness 与工作区资源内嵌于唯一的 Elves 程序。更新采用新的 Windows 安装包，升级会短暂关闭 Elves 及其拥有的后台，不重建独立管理器或旧启动器。仅编译或改源码时不安装、不切换正式服务。

工作区资源变更后递增 `assets/harness/resource-revision.txt`。共享管理页面变更后先构建前端，再构建客户端；核对程序内嵌资源与当前源码相同。远程网页入口更新与 Elves 本机入口分别验收，不能将网页可用当作本机客户端可用。

### Windows 安装包编译

- 只有用户明确要求生成或编译安装包时，才执行安装包脚本。日常修改、修复、检查、提交和推送不得自动附带安装包构建，避免重复消耗构建时间。
- 用户只要求“编译”时，按要求构建相关程序或前端，不默认生成安装包；要求仅检查或改代码时不以“交付完整”为由额外打包。
- 用户明确要求 Windows 安装包时执行：

```powershell
.\scripts\build-windows-installer.ps1
```

该脚本必须完整生成：

- `target/release/elves.exe`
- `dist/windows/app/elves.exe`
- `dist/windows/Elves-<version>-windows-x64-setup.exe`（`<version>` 取自根 `Cargo.toml` 的 `workspace.package.version`）
- `dist/windows/Elves-<version>-windows-x64.exe`（另行提供的便携客户端）

安装包只安装 `elves.exe`（另含卸载程序），不打包 launcher、管理器或独立工具 EXE。后台与固定工具内嵌客户端，工具按需释放到用户状态目录。桌面和开始菜单只创建“Elves”程序快捷方式；完成页运行选项指向该客户端。升级清理旧程序及旧快捷方式，卸载保留设置、密钥、授权和登录缓存。

不要改回手工 NSIS 命令作为默认流程。根 `build.ps1` 当前会自动调用 Android 同步构建；普通 Windows 编译使用上述专用入口，不能默认调用根 `build.ps1` 或 `scripts/sync-android-build.ps1`，以免附带构建 Android。需要修改构建流程时，优先更新 `build.ps1` 和 `scripts/build-windows-installer.ps1`。

### Elves Windows 客户端

- 用户只要求编译 Elves 客户端时，执行 `.\scripts\build-windows-client.ps1`，只生成独立 EXE 和对应构建信息，不附带管理器或 Android 构建。
- 客户端使用独立窗口配置和 WebView2 缓存；启动自动准备本机 Harness，原生写入仅用于本机的 HttpOnly Cookie 后打开工作区，不出现远程连接或密钥输入。
- 本机授权失效时通过受限原生命令恢复，仅保留本机会话页来源及服务端确认的回环地址；不得索取远程密码、清理草稿及待确认回执或重发写操作。历史远程登录标记不能阻止独立客户端启动，多窗口复用仍有效的本机凭据。
- 独立程序最顶部使用 Rust / Win32 实现的原生应用顶栏，显示程序标题，固定为 36 个逻辑像素，随 DPI 缩放。背景、文字和分隔线逐帧跟随会话页实际配色及渐变，不在原生端另启一套动画。文件、帮助菜单放在同一顶栏：文件仅提供“添加文件夹”；帮助提供“打开项目地址”“关于 Elves”，不提供更新入口。保留窗口拖动、缩放、最小化、最大化及还原，主窗口关闭收进托盘，独立会话窗口关闭自身，再次启动唤回原窗口。不恢复“连接”“窗口”菜单栏，刷新及退出保留在托盘，不提供连接设置。
- “添加文件夹”只由原生系统选择器取得目录，登记到 Harness 自有项目记录；取消不写入，重复目录复用项目，不写 Codex App 状态，也不自动发送会话。网页仅接收并确认原生已登记的项目 ID，不能提供任意目录或调用选择器。
- 应用顶栏不创建 WebView，窗口操作仅由原生控件处理；远程内容仅使用已登记的配色、未读角标、公开资源缓存、独立会话及本机拖贴接口。本机启动状态和重试仅授权内置启动页，不向管理 iframe 扩大原生命令权限。保留 `client_titlebar_theme` 命令名以兼容既有网页缓存，不恢复标题栏网页及其窗口控制授权。
- Windows 任务栏角标按当前启用实例的未读主会话 ID 去重，置顶、最近和项目中重复出现只算一次；侧边子会话、子代理、归档会话不单独计数。搜索、折叠和快捷筛选不改变总数，读完后减少、为零时清除，超过 99 仅在图标上显示 `99+`。只由主窗口上报，不按窗口累加；后台沿用实时事件并以 15 秒只读查询补齐，发送不等待角标结果，更换授权或连接地址时清除旧计数。
- 本机工作区加载失败时显示实际原因与重试，不回到远程地址输入页，也不把异常回执显示为已进入工作区。
- 客户端内置公开网页包，按服务 Origin 保存经过清单及 SHA-256 校验的当前/上一版资源；完整更新只在下次导航启用，不在使用中混换资源或自动刷新。缓存不能拦截 API、上传、鉴权或原生注入脚本。接管 Service Worker 时不得清除 Cookie、localStorage 或 IndexedDB；前台会话请求优先于资源更新。
- Elves 不保存或恢复远程连接地址；内置启动页只允许查看本机启动状态和重试，普通页面不能调用该权限。

### Android 显式编译

- 只有用户明确要求编译 Android，或管理端发版按下方规则判定需要同步发布 Android 时，才执行 Android 构建。
- 普通 Windows 编译不因 Android 相关代码变化、成功构建基线缺失或旧 APK 校验失败而自动附加 Android 构建。
- 用户明确要求编译 Android 时，执行 `.\scripts\build-android.ps1 -Configuration Release`，完成正式构建及 lint，生成 `dist/android/Elves-<androidVersion>-android.apk`。`<androidVersion>` 取自 Android 独立版本配置；仅编译不递增版本、不创建发布标签。
- 复用既有正式签名材料；缺少签名配置时明确报告 Android 构建失败，不得创建替代证书或以 Debug APK 交付。Debug 仅用于明确的开发验证或不持有发布密钥的 PR 检查。
- 只有正式 APK 构建及校验成功后才能更新本机 Android 构建基线；失败不得记录为已构建。

### GitHub 发版与 Android 发布验收

- 用户要求“发布”“发版”或“推送发布标签”时，必须分别检查桌面和 Android 的交付状态；用户明确限定发布平台时遵守指定范围。
- 发布管理端新版本前，以上一个**已公开的桌面正式 Release 对应标签**为起点，以本次待发布的目标提交为终点，检查期间所有会影响 Android 的改动。不得用未提交差异、上次本机构建或 Actions 构建代替此版本区间；Android 独立 Release、草稿和预发布不作为桌面比较基线。
- Android 影响检查必须覆盖：
  - `apps/elves-android/` 下的原生源码、资源、独立版本、依赖和构建配置。
  - APK 内置的 WebView 网页代码及资源。以 `apps/elves-android/app/build.gradle` 的 `prepareBundledWebAssets` 和 `bundledWebResources` 为准，检查源目录 `assets/harness/`，包括 `app.js`、`recovery-store.js`、页面样式、Markdown、附件及平台适配代码；这些文件会进入 APK 的 `assets/web/files/`。即使 Android 原生目录没有变化，内置网页代码变化也必须判定为影响 Android。
  - Android 构建、签名、资源打包和发布相关脚本及工作流；共享脚本或远程接口、资源更新协议的改动，也须核对是否改变 Android 的运行或更新行为。
  - 排除构建产物、工具链、缓存和本机私有配置；不影响上述内容的管理器专属界面或普通文档改动，不触发 Android 发布。
- 决定跳过 Android 前，还须以上一次**已公开且 APK、更新清单校验有效的 Android Release**为基线核对 Android 相关输入，防止遗漏此前尚未发布的 Android 改动。尚无有效 Android Release、存在未发布改动，或远端附件缺失、校验失败时，按需要同步发布处理。
- 缺少上一个桌面发布基线时，检查全部 Android 相关输入；仍无法确认是否影响 Android 时，报告缺失证据及 `失败/受阻` 状态，不得直接宣告跳过或完成。

| 管理端发版检查结果 | Android 处理 |
| --- | --- |
| 改动影响 Android，或存在上述未完成的 Android 交付 | 递增 Android 独立版本，正式构建及 lint 后同步发布 Android 安装包 |
| 改动不影响 Android，且既有 Android Release 及附件校验有效 | 跳过 Android 发布，说明桌面比较基线、检查范围和跳过依据 |

- 需要同步发布 Android 时，首次准备本次发布须将 `versionName` 的修订号增加 `1`，即递增 `0.0.1`，例如 `0.0.1` → `0.0.2`；不得改成管理端版本号。同时将 `versionCode` 设置为“当前值与已公开 Android 版本最大值中的较大值加 `1`”，保证可覆盖升级。失败重试时复用本次已经确定的目标版本，不重复递增。
- 按下表区分构建和发布，不得因 Actions 总体成功或桌面 Release 已发布就认定 Android 已发布：

| 触发方式 | Android 实际交付 | 可报告状态 |
| --- | --- | --- |
| `main` 推送 Android 相关变更 | 正式签名构建、lint，上传 Actions Artifacts | 已构建未发布 |
| 推送桌面版本标签，如 `v<desktopVersion>` | 触发桌面发布，不触发 Android 独立发布 | 单独核对 Android 状态 |
| 手动运行 Android Release，`tag` 留空 | 仅上传 Actions Artifacts | 已构建未发布 |
| 推送 `android-v<versionName>`，或手动运行 Release 并填写该标签 | 正式构建后执行独立 Android 发布任务 | 完成下述验收后才可报告已发布 |

- 本次发布已获用户授权且 Android 需要发布时，必须完成 Android **Release** 构建、lint、既有正式签名及 APK 校验，并创建、推送对应的 `android-v<versionName>` 标签，或对已存在的正确标签触发 Release 工作流，同步发布 Android 安装包和更新清单。不得只生成本地 APK 或 Actions Artifacts 后报告已发布，也不得移动已发布标签或覆盖不同内容的已发布 APK。具体步骤遵循 `docs/android-release.md`。
- 等待 Android 工作流结束，单独确认 `Publish independent Android Release` 任务成功；该任务为 `skipped`、失败或仍在运行时，不得报告发布完成。
- 发布后确认对应 Android Release 已公开，包含 `Elves-<versionName>-android.apk` 和 `android-update.json`；下载两项附件，核对版本、`versionCode`、APK 大小和 SHA-256 与更新清单及本次正式构建一致，并确认桌面 Latest 标记和 `latest.json` 未被 Android 发布覆盖。
- 最终分别报告桌面和 Android 状态：`已发布`、`已构建未发布`、`进行中`、`失败/受阻` 或 `跳过（附依据）`，并提供对应运行记录、Release 或产物位置。只要求检查、诊断或本地编译时，不得擅自创建、推送发布标签或触发远程发布；发现未发布应明确列为待完成项。

## 共享管理前端检查入口

在仓库根目录执行共享管理前端检查时，使用 `npm --prefix apps/elves-management run check`；构建使用 `npm --prefix apps/elves-management run vite:build`，输出为 `assets/harness/management/`，由独立 Harness 提供。
