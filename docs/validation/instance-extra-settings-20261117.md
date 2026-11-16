# 实例额外配置核对与设置接入

核对日期：2026-11-17。范围为本机 `$CODEX_HOME/config.toml` 及 `claude-us`、用户 `junes` 的 `~/.claude/settings.json`。只盘点设置及验证新增入口，没有切换现有配置值，也没有更新正式远程服务。

## 配置与原生能力证据

- 本机 `codex --version` 返回 `codex-cli 0.164.0`。`codex features list` 及独立 CLI app-server 的 `experimentalFeature/list` 确认 Chronicle、上下文管理、会话存储压缩处于 `underDevelopment`，旧 `js_repl` 标记为 `removed`。未连接或读取 Codex App 的运行状态。
- Codex 标量、记忆和并发字段按 [官方配置 schema](https://developers.openai.com/codex/config-schema.json) 核对；旧 `personality` 的风格选择已弃用。配置 schema 中存在某键，不单独作为原生特性仍有效的证据。
- `claude-us` 内 `/home/junes/.local/bin/claude --version` 返回 `2.1.325`。查询均从 `/home/junes` 的登录环境启动；有效 `TZ` 为 `America/Los_Angeles`，原生 `date` 返回 PST、`-0800`。没有用宿主机时区替代目标 WSL 时区。
- 同一 Claude CLI 的 `--help` 确认 `--autocompact`、`--settings` 和 `doctor`。通过给额外布尔字段临时传入错误类型，`doctor` 分别返回对应字段的 `Expected boolean, but received string`，确认该 CLI 实际识别这些字段。临时覆盖不改写用户配置，不创建会话或模型请求。
- `modelSettings.<model>.effortLevel`、缓存及压缩环境变量没有取得同等的原生持久化或能力证据，因此只展示配置，不认定实际生效，不开放修改。没有直接请求 Claude / Anthropic 官方接口，也未提取或复用登录凭据。

## 新增可写项

| 实例 | 页面 | 设置及原生字段 |
| --- | --- | --- |
| Codex | 会话实例 | Chronicle：`features.chronicle`；实验性上下文管理：`features.context_management.experimental_mode`；存储压缩：`features.local_thread_store_compression` |
| Codex | 会话实例 | 生成记忆：`memories.generate_memories`；使用记忆：`memories.use_memories`；子智能体并发上限：`agents.max_concurrent_threads_per_session` |
| Codex | 模型与思考 | 自动压缩阈值：`model_auto_compact_token_limit`；思考摘要：`model_reasoning_summary`；默认服务档位：`service_tier` |
| Claude | 会话实例 | `syncClaudeAiSkills`、`syncClaudeAiPlugins`、`disableBundledSkills`、`skipDangerousModePermissionPrompt`、`switchModelsOnFlag` |

共增加 14 个可写项；它们仍需通过实际实例的能力、设备写权限及配置版本检查。支持删除覆盖以恢复原生默认，并沿用共享更新事件。

## 只读诊断与保留边界

- 已配置的旧 JavaScript REPL、旧回答风格分别显示原值和当前无法作为可用设置管理的原因。
- Claude 的 `CLAUDE_CODE_PROMPT_CACHE_TTL`、`CLAUDE_CODE_AUTO_COMPACT_WINDOW`、`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` 只展示固定字段中的安全值，保留原配置。可写的远程压缩窗口继续使用已有的唯一入口和原生 CLI 参数。
- 已配置的逐模型 `effortLevel` 只返回受限模型 ID 与已知思考深度，不返回其他模型配置内容。
- 已有 Todo、记忆总开关、子智能体、目标、回顾、权限、默认模型、Skills、MCP 和钩子继续使用已有入口。
- 不将项目授权记录、账号或密钥、工具信任环境、第三方源、状态栏或通知命令、Codex App 的 `desktop.*` 偏好复制成远程可写项；不向网页传递它们的原文。

## 验证

- 远程网页完整回归：556/556，包含额外项默认状态、只读显示、非法数字、`badInput`、迟到失焦和返回字段校验。
- Rust 实例设置专项：6/6，覆盖原配置与注释保留、BOM/CRLF、恢复默认、布尔及表形式的上下文配置、类型与范围、原生能力拒绝、只读项和凭据脱敏。
- 在 `claude-us` 的临时 Linux 配置文件上直接运行生产写入程序：104 项断言通过，覆盖 5 个布尔项的开启/关闭/恢复默认、版本冲突、只读文件、未知键、原权限、扩展属性、BOM/CRLF 和其他配置保留；测试前后真实 Claude 配置摘要一致。
- 管理器 `check` 通过；140 个网页资源、134 个 JavaScript 文件的语法、导入导出、分层、循环依赖及预加载检查通过。

上述结果确认源码读写接入、返回约束和隔离配置上的保存行为；未宣称所有实验功能已经在实际任务中执行，也未构建安装包、发布新版本或部署正式服务。
