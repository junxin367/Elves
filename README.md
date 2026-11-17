# Elves

<p align="center"><img src="assets/harness/icons/elves-logo.svg" alt="Elves 图标" width="128"></p>
<p align="center">中文 · <a href="README_EN.md">English</a></p>

**在一个工作区中使用 Codex、Claude，管理项目、会话和本地协议代理。**

Elves 是 Windows 桌面客户端，打开后自动启动本机 Harness 并进入工作区，无需填写远程地址或访问密钥。Harness 独立负责原生执行与管理：Codex 使用独立 app-server，Claude 使用所选 WSL 实例的原生 CLI。

网页、PWA 和 Android 通过 Harness 的“远程会话”能力接入同一工作区。远程会话可以关闭，不影响 Elves 本机使用或本地代理。项目不再依赖 Codex App，也不提供页面注入、桌面桥接或独立管理器。

代理设置、会话管理和远程访问配置仅在 Elves 独立客户端开放。网页、PWA、Android 使用访问密钥登录后提供会话与普通设置，不能访问管理页面或相应管理接口。

[版本发布](https://github.com/junxin367/Elves/releases) · [使用与开发文档](docs/README.md) · [问题反馈](https://github.com/junxin367/Elves/issues)

## 可以做什么

| 入口 | 功能 |
| --- | --- |
| 会话 | 按项目和实例查找会话，继续对话，选择模型与思考深度，处理队列、审批和问答，查看文件与变更 |
| 代理设置 | 配置供应商、模型、协议和本地代理，查看运行状态、请求日志与请求详情 |
| 会话管理 | 浏览和筛选本地会话，单个或批量删除 |
| 远程会话 | 配置网页服务、公网入口、访问密钥及设备授权，供其他设备访问 |
| 设置 | 管理实例、模型默认值、会话命名、权限、自定义指令、Skills、MCP、钩子和外观 |

会话命名由同一 Harness 全局共用一组执行实例与模型。外观按客户端独立保存，其他设置在同一 Harness 的各客户端间共享；草稿、附件和阅读位置保留在各客户端。模型、思考深度和扩展能力以所属实例实际提供的能力为准。

## 快速开始

### 准备环境

- Windows x64，以及可用的 Microsoft Edge WebView2 Runtime。
- 已安装并完成原生配置的 Codex CLI，或已配置的 WSL Claude 实例；在 Elves 设置中检测并选择需要使用的实例。
- 使用自定义供应商时，准备对应的 Base URL、API Key 和模型信息。

在[版本发布页](https://github.com/junxin367/Elves/releases)查看已发布的安装包，也可以按下文从源码构建：

| 文件 | 用途 |
| --- | --- |
| `Elves-<version>-windows-x64-setup.exe` | Windows 安装包，只安装一个 Elves 程序及卸载程序 |
| `Elves-<version>-windows-x64.exe` | 便携客户端 |

### 打开工作区

1. 安装或运行 **Elves**，等待本机 Harness 就绪后进入会话工作区。
2. 打开底部“设置”，检测并配置会话实例，按需设置默认模型、思考深度和权限。
3. 返回“会话”，选择实例和项目，新建或继续已有会话。
4. 需要使用自定义供应商时，进入“代理设置”，保存供应商与模型配置后点击“使用”。

为当前启用的供应商打开“启用本地代理”后，代理随 Elves / Harness 自动启动，默认端口为 `45221`。Responses API 请求直接发往配置的 Responses 上游；Chat Completions 和 Anthropic 使用对应的协议转换。模型列表分别保留协议归属。

关闭主窗口会收进系统托盘；从托盘选择退出时，Elves 安全回收自己启动的后台。Windows 的底部设置图标直接打开设置界面。

## 从网页或手机访问

在 Elves 左侧“远程会话”中启用网页服务，保存访问密钥，并配置其他设备可访问的地址。浏览器和 PWA 使用该地址与密钥登录；Android 连接同一 Harness，APK 按独立版本发布。

本机工作区默认使用 `127.0.0.1:45223`，远程网页默认端口为 `45222`。本机授权与远程设备授权分开保存；关闭远程会话、调整远程只读策略或撤销远程设备，都不会使 Elves 本机退出。远程使用期间，运行 Harness 的电脑需要保持开机。

详细步骤见[手机连接指南](docs/remote-access-mobile.md)；公网线路与授权规则见[远程会话说明](docs/remote-access.md)。

## 数据与升级

- 共享设置、供应商配置、日志、远程密钥和设备授权保存在 `~/.codex-session-delete/`，沿用旧数据目录以支持升级。
- Codex / Claude 的原生配置和历史仍由所属实例保存；Elves 的窗口状态和 WebView2 缓存使用独立客户端目录。
- 升级使用新的 Windows 安装包。安装程序清理旧程序入口并保留用户数据；卸载同样保留设置、历史和登录缓存。
- 操作结果未知时保留原回执，先核对会话实际结果，避免重复发送或删除。

## 开发与构建

需要 Node.js/npm、Rust MSVC 工具链及 Windows C++ 构建工具；Windows 安装包另需 NSIS。

```powershell
npm --prefix apps/elves-management ci
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run build:frontend
cargo fmt --all -- --check
cargo check --workspace --locked

# 日常编译：检查网页资源、按内容复用管理前端，再增量构建客户端和便携版
.\scripts\build-windows-client.ps1

# 需要完整优化的客户端时
.\scripts\build-windows-client.ps1 -Profile release

# 仅在需要安装包时构建；默认使用 release 完整优化
.\scripts\build-windows-installer.ps1
```

产物位于 `dist/windows/`，以上入口均不附带 Android 构建。日常 `release-fast` 只对项目自身 crate 使用较轻的优化；正式 `release` 保持完整优化。网页资源只在 `elves-assets` 中嵌入一次，供客户端和 Harness 共用。

仅构建共享管理页面时，使用 `npm --prefix apps/elves-management run build:frontend`；它同时核对源码、配置、依赖及生成文件的内容摘要，任一变化都会重建。使用 `run build:frontend -- --force` 或原有 `run vite:build` 可强制重建。构建配置与实测方法见[构建性能说明](docs/build-performance.md)。

管理页生成目录 `assets/harness/management/` 不纳入 Git，包括入口 HTML 和带哈希的 JS/CSS。全新检出、清理产物或切换管理页源码后，应先生成前端再运行 Cargo；缺少入口或入口引用的资源时，Rust 构建会明确失败。普通工作区网页源码、品牌图标、第三方运行库及内置插件包继续纳入 Git。

| 目录 | 责任 |
| --- | --- |
| `apps/elves/` | 唯一 Windows 程序、原生窗口及本机客户端能力 |
| `crates/elves-harness/` | Harness 执行、会话、管理及可选远程接入 |
| `crates/elves-core/`、`crates/elves-data/` | 原生配置、协议代理与会话数据访问 |
| `assets/harness/` | 桌面与远程入口共用的工作区网页 |
| `apps/elves-management/` | 共享管理前端源码，构建到 `assets/harness/management/`，产物不入 Git |
| `apps/elves-android/` | Android 远程接入容器，版本与签名独立维护 |

## 文档

从[文档索引](docs/README.md)选择使用指南、架构说明和构建发布流程。产品行为以[产品规范](docs/product-standards.md)为准；执行边界见[Elves 与 Harness 架构](docs/harness-architecture.md)。
