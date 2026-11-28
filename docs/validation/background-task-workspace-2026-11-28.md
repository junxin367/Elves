# 会话后台任务与工作区标签页验收

产品行为见[后台任务展示](../product-standards/core/product-behavior.md#后台任务展示)。本次验证开发源码，没有安装、切换正式服务或生成安装包。

## 单任务控制验证

- **Claude**：在 `codex-elves-claude-test` 的 `elves-test` 登录环境使用原生 CLI `2.1.330`。新建不持久化的隔离会话，推理回包由本地模拟上游提供，指定 `gpt-6.1-sol`；执行的 A、B 只是打印文本并等待。实际读取到两项各自的 `ready`，停止 A 后收到 A 的 `stopped` 通知，仍能读取 B 的输出。B 随后 `completed`、退出码为 0，原会话继续响应。两项最终输出均由原任务的 `get_task_output` 取得，未把启动回执当作 stdout。
- **Codex**：用独立引擎 `0.167.0-alpha.4.1`、隔离的 Codex Home、临时工作目录和本地模拟上游运行 A、B。原生列表分别返回了两个条目及各自的进程标识。只终止 A 后，B 仍在列表并自然完成，原会话仍可读取。A 的原生结束条目为 `failed`、退出码 `-1`；只有同时收到针对 A 的 `terminated:true` 才展示已停止。没有调用全部终端清理或整轮中断。
- **AGY**：转录回放覆盖正常退出、非零退出、取消、无法确认的结果及任务身份不匹配。只读取可核实的记录，不声称支持实时任务列表或单任务停止；未向真实 AGY 会话发消息。

## 回归结果

| 检查 | 结果 |
| --- | --- |
| `node scripts/check-harness-assets.mjs` | 155 个网页资源、148 个 JS 文件通过 |
| `harness-ui-state.test.mjs`、`workspace-tabs.test.mjs` | 766 项通过 |
| 工作区标签、Windows 缓存与顶栏、Harness 验收脚本的扩展回归 | 226 项通过，包含上述标签测试，不重复计数 |
| 测试 WSL 内运行 `scripts/claude-shared-history.test.py` | 102 项通过 |
| `cargo test -p elves-harness --profile release-fast --lib background_task -- --test-threads=1` | 10 项通过 |
| 同一 Rust 测试程序的 `claude::sdk::` 与 `antigravity::` | 分别 58 项通过，以及 68 项通过、1 项显式真实 ACP 联调用例忽略 |
| Chrome 隔离页面使用工作区实际菜单、标签和任务组件 | 1220px、390px、320px 通过，无页面异常；接口使用可控测试数据 |

浏览器覆盖「＋ → 任务记录」原位打开和复用、桌面和手机查看详情、只读禁停、已请求与已停止的区分、断线后输出不再标为实时、任务归零仍可回看、原始换行、切换会话后丢弃迟到回复，以及返回时保留草稿。交互终端仅为待接入入口。
