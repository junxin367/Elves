# Elves 与 Harness 架构

Elves 是用户安装的桌面产品。Harness 是独立执行产品，提供原生会话、实例设置、共享偏好及可选远程接入。代理设置、供应商切换、代理日志、本地会话管理和上下文压缩配置由独立的 Elves 代理管理器提供。“远程会话”只表示网页、PWA、Android 访问 Harness 的能力，不表示整个项目或后台架构。

## 代码边界

| 目录 | 责任 |
| --- | --- |
| `apps/elves/` | 唯一 Windows 程序、原生窗口、启动和本机文件能力 |
| `crates/elves-assets/` | 客户端与 Harness 共用的内嵌网页资源，不包含执行或授权逻辑 |
| `crates/elves-harness/` | Harness 服务、Codex app-server、Claude CLI、会话执行及远程授权 |
| `crates/elves-core/` | 设置、模型能力、协议代理及原生供应商配置 |
| `crates/elves-data/` | 会话目录、SQLite、Markdown 和供应商数据同步 |
| `apps/elves-management/` | 代理管理器的 React/Vite 前端，构建为独立 Tauri 管理器资源 |
| `apps/elves-manager/` | 独立代理管理器的 Tauri 壳、管理命令桥和代理服务启动入口 |
| `assets/harness/` | Elves 与 Harness 网页共用的工作区资源 |
| `apps/elves-android/` | Android 远程接入容器，保留正式签名和 applicationId |

依赖方向为客户端调用 Harness，Harness 使用 core 与 data，data 使用 core；客户端与 Harness 共用 assets。core 不依赖 Harness 或桌面窗口。管理接口使用固定白名单；所有执行仍经过目标原生实例，禁止桥接 Codex App。

## 启动与代理

默认运行 `elves.exe` 创建原生窗口，自动启动同一程序的 `--harness-service` 后台模式。后台先启动本机回环工作区，再启动会话执行和可选远程接入。代理服务由 Elves 客户端按启动策略单独拉起，也可以由代理管理器单独启动；代理管理器只通过核心设置管理代理，不接管 Harness 会话请求。

当前已启用供应商的本地代理开关和“随 Elves 启动”开关同时为真时，Elves 客户端会启动独立的 `--proxy-service` 子进程；代理管理器也可以按同一启动策略启动该进程。Harness 只负责本机工作区和会话执行，不再嵌入代理监视器。关闭启动策略只阻止自动拉起，不改变供应商请求是否走代理；管理器或 Harness 退出不会回收已运行的独立代理，代理默认端口 `45221`，与远程开关无关。供应商和显式协议配置按既有规则读取，不更改 Responses、Chat Completions、Anthropic 或 Gemini Interactions 的路由语义。

## 本机与远程隔离

| 入口 | 默认监听 | 授权与策略 |
| --- | --- | --- |
| Elves 本机工作区 | `127.0.0.1:45223` | 独立本机 Cookie，一次性证明，不加入远程设备清单 |
| 远程网页、PWA、Android | 配置地址，默认 `45222` | 访问密钥、设备授权、只读和审批策略、公网入口 |
| 本地协议代理 | 默认 `45221` | 供应商请求路由与“随 Elves 启动”策略分别控制，局域网访问沿用显式开关与网段校验 |

远程监听不能占用本机工作区或代理端口。本机入口只接纳真实回环 Host，远程访问不能提交本机证明。远程关闭、只读、换密钥、撤销设备或公网故障不影响 Elves 授权、执行或代理。本机不发现或切换公网通道。

Harness 不提供代理设置和会话管理页面或管理 API；代理管理器使用本机 Tauri 命令桥读取核心设置和本地会话数据。远程网页、PWA、Android 只提供会话和普通设置；旧 `/management/` 资源不再进入 Harness 清单，旧缓存入口继续拒绝该路径。

本机客户端不使用远程密码登录。Cookie 失效后，受限原生命令重新建立或恢复本机 Cookie，随后只恢复会话读取；草稿与待确认回执保留，不重发写操作。本机恢复命令不接受目标地址，也不向网页返回令牌。

## Codex 执行引擎

Harness 优先使用 Elves 自己拉起的 Codex app-server（私有服务）：用当前选中的 Codex CLI 执行 `app-server --listen unix://<状态目录>\codex-app-server\<Codex Home 摘要>\app-server.sock`，注入与 stdio 相同的工具环境，进程脱离 Harness 存活。套接字所在目录由 Codex 以私有权限创建，预先建好的普通目录会被拒绝。Harness 每次连接启动 `codex app-server proxy --sock <套接字>`，在 proxy 的标准输入输出上完成 WebSocket 握手，用文本帧收发 JSON-RPC；超过 8 MiB 的请求分片发送。服务身份（PID、进程创建时间、CLI 路径与版本、服务代次）记录在同目录的 `<摘要>.json`，复用和停止前都按身份核对。不使用 Codex 终端版共享的托管 daemon，它的版本自行更新，更新可能中断执行中的任务。

私有服务只在确认没有可继续的任务时停止：引擎空闲释放，或托盘退出、安装升级时没有执行中、等待中的会话与后台终端。当前没有连上的服务（连接断开，或上次留下的服务）退出前先经 proxy 只读核对再决定，读取失败才按状态未知保留。CLI 更新后，服务里没有已加载会话才重启换新，否则沿用旧版本直到下次空闲。私有服务不可用（脚本包装器 CLI、启动失败、无响应）时退回 stdio 子进程，此时 Codex 随 Harness 结束，退出与升级照常放行，执行中的任务随之中断。会话命名等短时推理固定使用独立 stdio 进程，不进入私有服务，私有服务里只有 Harness 自己的会话。

Harness 接上同一代次的私有服务后，对执行中或有后台终端的会话执行 `thread/resume`，恢复流式事件。后台终端只能按命令条目的开始与结束事件跟踪，交接时随服务代次写入 Harness 状态，新连接用会话历史核对是否已结束。当前 Codex 协议不会向新连接补发待处理的审批或用户输入请求，所以等待中的轮次退出后会留在服务里继续等待，重新打开 Elves 后只能停止这一轮再重新发送。

## 数据与兼容

共享设置、历史和授权继续使用既有用户状态目录。已发布协议字段、Cookie、缓存键和 Android 标识按兼容需要保留旧名称；它们不作为新产品名。Windows 安装包含 `elves.exe` 与 `elves-manager.exe`，固定工具只由 Elves 客户端内嵌并按需释放。旧 launcher、注入脚本及任务看板入口删除。

桌面主窗口关闭收进托盘，明确退出才执行安全回收。回收按原生任务状态和未确认操作判断，结果未知时保留原始回执，不自动重发或把强制终止视为成功。
