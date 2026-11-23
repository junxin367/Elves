# Claude 会话命令标签显示优化（2026-11-22）

使用者反馈：Claude 会话里出现 `<command-name>/model</command-name><command-message>model</command-message><command-args>haiku</command-args>`、`<local-command-stdout>Set model to haiku (claude-haiku-5-5)</local-command-stdout>`、`<command-name>/compact</command-name>…`、`<local-command-stdout>Compacted </local-command-stdout>` 这类带标签的气泡。要求：该渲染的渲染，不需要显示的隐藏。显示规则见[远程会话手册](../product-standards/sessions/remote-access-guide.md)中“本地斜杠命令”一段。

## 根因

读取器 `claude_shared.py` 识别“CLI 自己写入的本地命令记录”时，来源核对只认终端（`entrypoint=cli`）。会话改走 Claude Agent SDK 后，CLI 以 `entrypoint=sdk-cli` 写同样的记录，全部被当作用户消息，标签原文直接显示。网页切换模型、点“压缩”都会写这组记录，所以 `/model`、`/compact` 最先暴露。

## 真实记录形态

测试发行版 `codex-elves-claude-test`（用户 `elves-test`），Claude Code 2.1.330，真实上游 `gpt 6.1 sol`。用 Elves 相同的 stream-json 参数逐条写入网页形态的用户帧，再解析原生 JSONL；配置目录用隔离副本，验证后已删除。

| 网页发送 | 原生写入的记录 |
| --- | --- |
| `/context`、`/usage` | caveat（user，`isMeta`）→ 命令行（user，只带 `turnAttributionKey`）→ 输出（`system/local_command`，无 `promptId`） |
| `/model X`、`/effort X` | 同上；成功和失败提示（`Model 'x' is not available`、`Unknown option: bogus`）都是 `system/local_command` |
| `/compact` | `compact_boundary` → 压缩摘要（`isCompactSummary`）→ caveat → 命令行 → 输出 `Compacted `（这一条是 user 行）；空会话时输出 `Not enough messages to compact.`（system 行） |
| `/goal`、`/goal clear`（无目标） | 同 `/context`，输出 `No goal set…` 为 system 行 |
| 自定义命令 `/hello world`、技能 `/greet there` | 命令行 `<command-message>` 在前，只带 `turnOrigin: "sdk"`；展开后的提示是 `isMeta` + `turnCompanion` 的伴随记录 |
| 普通提示 | 带 `promptSource`、`turnOrigin`、`permissionMode`（本机 2.1.329 的记录没有 `turnOrigin`，另两项仍在） |

两点关键事实：

- 所有命令行的 `uuid` 都等于网页发送帧的 UUID（`command_lifecycle` 帧的 `command_uuid` 也是它）。读取器按这个 UUID 确认待确认消息，命令行不再算用户消息后仍然能确认，不会残留待确认意图。
- 使用者本机真实会话里，`/model haiku` 的 “Set model to …” 输出是 user 行，与测试实例手打时的 system 行不同；产生路径没有核实（可能来自 `set_model` 控制请求）。两种形态都要处理。

## 修改

- `claude_shared.py`：`native_setting_source` 同时核对终端和 SDK 来源。SDK 记录没有网页提示才有的 `promptSource`、`turnOrigin`、`permissionMode`（三项任一存在即视为用户输入）且没有 `origin`，才判定为 CLI 自己写的；手写或引用同样标签仍保持原文。`native_local_command` 对 SDK 记录不再要求 caveat 父节点（终端记录仍要求），并标记 `via: "sdk"`。新增 `native_sdk_local_record`，让只读原始行、没有父链可查的位置（会话标题、最近时间）也不把命令行和输出行当成用户消息。
- `claude_tools.rs`：本地命令事件条目带上 `via`。
- `item-presentation.js`：经 SDK 执行的命令显示“执行了 /context”，终端执行的仍是“终端执行了 /resume”；已递增 `resource-revision.txt`。
- 不改写原生 JSONL，标注都在读取时按原生记录计算，已有历史会话重新读取即生效，不需要迁移。

## 验证

- Python 读取器测试 87 项全部通过（原 84 项，新增 3 项，覆盖：`/model`、`/compact` 的命令行与 user 形态输出被隐藏，`/context` 的 system 输出并入事件行；自定义命令显示为用户输入的命令、展开提示隐藏、手写的同样标签保持原文；手打命令的待确认消息按 UUID 确认；system 形态的设置命令输出不露出标签、不改变设置状态）。第一条新增用例在修复前的读取器上失败（`KeyError: 'nativeSettingCommand'`）。
- 用使用者本机一个含终端和 SDK 两种写入的真实会话文件喂给读取器（只统计，不输出对话内容）：带标签的 81 行中，54 行（27 条命令及其输出）已被识别，剩余 27 行全部是 `isMeta` 的 caveat 元数据，Rust 投影不把 `isMeta` 行当作用户消息。
- Rust 投影测试 `shared_sdk_local_commands_are_events_and_setting_records_stay_hidden`：条目里不出现 `command-name`、`local-command-stdout`，只剩带 `via: "sdk"` 的 `/context` 事件行及其输出。
- Rust 相关测试组：`claude::`、`claude_sdk::`、`claude_tools::` 三组共 169 项（单线程），168 项通过；失败的 1 项是 `claude::tests::discovered_model_choices_use_the_bound_instance_catalog_without_resuming`，断言没有计入模型目录的 `hiddenByInstance` 字段，在 HEAD 基线副本（独立 target）上同样失败，与本次改动无关。
- 网页状态测试 670 项中 664 项通过，新增断言覆盖“执行了 /context”文案；失败的 6 项都不在本次改动范围：5 项是 Claude 额度底栏（另一个会话尚未提交的 `claude-usage.js` 改动），1 项是任务列表折叠（另一个会话尚未提交的 `history-state.js` 新增了 `turnPlanNodes`，该测试还没有跟着补装配）。HEAD 基线副本上这 6 项全部通过。网页资源静态检查通过。

## 未覆盖

- 没有在真实 Windows 客户端或浏览器里看渲染，需要重新编译客户端后确认。
- 手打 `/model`、`/effort`、`/goal`、`/compact` 失败时的提示（system 形态，例如 `Model 'x' is not available`）仍然不展示，与终端旧格式一致。网页的模型选择器走控制请求，不受影响；若需要展示，可以把这类输出并入事件行。
- 网页手打 `/context` 后，本地已发送的气泡与命令事件行会同时出现，未在页面中核对重进后的最终样子。
