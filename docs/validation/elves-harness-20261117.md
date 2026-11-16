# Elves 与 Harness 改造验收

日期：2026-11-17。桌面版本保持 `0.8.0`，Android 独立版本未调整。

## 实现范围

- 桌面产品统一为 Elves，唯一程序为 `elves.exe`；Harness 独立承担原生执行、会话及管理，远程会话只提供网页、PWA、Android 接入。
- Elves 自动启动本机 Harness，通过一次性证明及独立 HttpOnly Cookie 进入本机工作区，不读取旧远程地址或要求访问密钥。
- 已启用供应商的本地代理开关控制代理随程序启动，远程开关不控制本机代理、执行或授权。
- 删除 Codex App 注入、CDP、renderer 脚本、旧 launcher、独立管理器、任务看板及相应配置和构建入口。
- 保留旧设置、历史、协议字段、授权及 Android 标识的兼容；安装和卸载保留用户数据。

## 代码与构建检查

| 检查 | 结果 |
| --- | --- |
| 前端类型、模块语法、导入导出、分层及资源检查 | 通过，146 个公开资源、138 个 JS 文件 |
| 共享管理前端构建 | 通过，产物随源码提交 |
| `cargo fmt --all -- --check`、`cargo check --workspace` | 通过 |
| 客户端、协议代理及数据层 Rust 回归 | 1215 项通过，2 项按已有条件跳过 |
| Harness 完整首轮回归 | 461 项通过，14 项因真实环境或子进程入口条件跳过；两项沿用旧架构的断言随后修正 |
| Harness 定向复测 | 两项旧断言复测通过；5 项本机授权测试复测通过，包含关闭远程监听的实际生命周期 |
| Node 回归 | 773 项通过 |
| Claude 共享历史 Python 回归 | 在 `claude-us` 的 Linux Python 环境中执行，190 项通过；未调用模型或读取官方账号信息 |
| 变更的 PowerShell 脚本 | 8 个文件语法检查通过 |
| Windows 安装包 | 专用构建脚本退出码为 0，资源、内嵌工具及许可证检查通过 |

两项 Harness 断言分别涉及只有 Claude 的环境样例错误要求出现 Codex，以及把关闭远程监听等同于禁止 Harness 保留安全读取引擎。修正后分别核对真实实例范围与执行派发开关。

WebSocket 取消回归改为通过原生只读快照核对传输状态，不恢复已删除的 `/transport/sessions` 桌面管理路由。

## 隔离运行验收

使用独立客户端数据、Harness 状态及 Codex Home。测试设置关闭远程会话、开启远程只读，并保留一个不可达的旧远程地址占位值。

默认客户端进程能够启动自己的 Harness。最终程序的 `--harness-service` 模式进一步实测通过：

- 管理协议为 10，管理端点归属本次启动的进程。
- 一次性本机证明完成授权，`/api/session` 返回 `transport: "desktop"`、`readOnly: false` 及可用管理能力。
- 远程会话仍关闭，保存的远程只读设置仍为真；启动未改写这些设置。
- 本机通道不返回公网主入口或第二入口，公开资源清单包含 146 个文件。
- 安全退出返回 `stopped`，本次后台正常退出，测试客户端已关闭。

当前 `45221` 已有监听，本次没有替换或中断它。代理自启动选择条件和协议监听分别由单元及协议回归验证。

Windows 桌面锁定，未完成实际页面、底部设置交互和截图验收。后台实测不能替代这部分界面验收。

## 产物与交付边界

安装输入只有 `dist/windows/app/elves.exe`。`target/release-fast/`、`target/release/`、安装输入及便携副本四份程序的 SHA-256 相同：

`f01c05697aeb32439b41d09ec6e17f8f4309d2f9784aabd76c31c3a4aa32b9eb`

安装包：`dist/windows/Elves-0.8.0-windows-x64-setup.exe`，21,682,912 字节，SHA-256：

`cfa1d63893bb1fc0957707dfa1f1ddf3605fb36f63504812b59b4d9ea8ec8339`

便携程序：`dist/windows/Elves-0.8.0-windows-x64.exe`。

本次未安装、未切换正式服务、未创建发布标签或公开 Release，未执行 Android 构建。
