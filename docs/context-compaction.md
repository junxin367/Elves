# 协议代理上下文压缩与历史恢复

本文只说明已启用的本地协议代理如何处理压缩及压缩后的请求。工作区的压缩入口、实例配置及可用能力见 [Elves 产品规范](product-standards/core/product-behavior.md#实例模型与压缩)。改造前问题和实测见 [2026-08-03 至 2026-08-05 历史验证](validation/context-compaction-20260803-20260805.md)。

## 目标

按最终生成压缩结果的路径区分模式，并保留最近一次真实用户回合：

1. 对外只区分“本地压缩”和“远程压缩”两种模式。
2. 压缩模式按最终由谁生成压缩结果判定，不按最初收到的触发类型判定。
3. 本地压缩时，LLM 只总结“指代锚点”之前的历史。
4. 从最后一条真实 `user` 之前最近的一条可见 `assistant` 消息开始，原样保留该指代锚点、
   最后一条真实 `user` 以及后续消息，不再文本化后拼入摘要。
5. 不为满足上游消息结尾规则而向持久会话写入虚拟用户内容。

## 压缩模式定义

### 本地压缩

最终压缩摘要由 Elves 控制的普通模型请求生成，即归类为本地压缩。

本地压缩包括：

- Codex 发送传统上下文压缩提示词，由 Elves 调用模型生成摘要。
- Codex 触发 Remote Compaction V2，但目标模型或协议不支持原生 V2，Elves 将
  `compaction_trigger` 转换为普通摘要请求并在本地兼容桥中生成压缩结果。

因此，V2 只是触发来源。只要最终经过本地兼容桥生成摘要，运行模式就是本地压缩。

本地压缩可以使用：

- Elves 的压缩提示词或用户配置的自定义压缩提示词。
- 最近回合原始保留策略。
- 使用会话主模型生成摘要；校验失败时用同一模型重试一次。
- HTTP 与 WebSocket 共用的本地响应改写逻辑。

### 远程压缩

最终压缩结果由上游模型服务通过原生 Remote Compaction V2 生成，即归类为远程压缩。

远程压缩必须保持上游原生行为：

- 原样发送 `compaction_trigger`。
- 不替换为本地摘要提示词。
- 不使用本地最近回合保留逻辑。
- 不改写原生压缩项的内容结构。

### 内部诊断字段

内部仍可记录触发来源和兼容桥状态，例如：

- `legacy_prompt`
- `remote_v2_trigger`
- `local_bridge_applied`
- `native_remote_v2`

这些字段只用于日志和排障，不能再作为用户可见的第三种压缩模式。

## 本地压缩的缓存前缀

- GPT、Claude 和其他模型的本地压缩统一保留系统提示、工具定义、动态工具目录及推理配置，
  不因模型是 GPT 或本地缺少有效缓存记录而删除这些字段。
- 首次请求只替换压缩触发项；开启最近回合原始保留时，仍按现有规则截取待摘要历史。
- 原请求强制调用工具时，压缩副本把 `tool_choice` 改为 `none`；原请求要求 JSON 或
  JSON Schema 输出时，压缩副本改回普通文本输出。工具目录、系统提示和其他稳定字段继续保留。
- 同模型重试只在输入末尾追加摘要要求，不替换系统提示，不删除 developer/system 消息或工具定义。
  压缩响应仍须通过摘要格式与无工具调用校验，未通过的工具调用不得传回客户端执行。
- Responses/Chat 风格 usage 的缓存写入和命中都会建立保守的五分钟租约，避免只有
  `cached_tokens` 时把已命中的缓存长期判断为未知。
- 独立压缩模型的选择策略不变；跨模型切换仍需调整推理配置，不能保证复用原模型缓存。
  Responses WebSocket 遇到其他协议的独立压缩模型时，当前轮次自动切换 HTTP 执行。
- 真正由上游执行的原生 Remote Compaction V2 不进入上述处理；V2 降级为本地摘要时遵循上述规则。
- 摘要中的完整会话记录只使用相对 `CODEX_HOME` 的路径，不能写入本机绝对路径。

保留前缀用于避免项目自身破坏缓存匹配条件，不代表上游必然命中缓存。

## 本地压缩数据流

### 1. 切分输入

压缩开始前，从压缩触发项之前的输入中找到最后一条真实 `user` 消息，再向前找到最近一条
可见 `assistant` 文本消息作为“指代锚点”：

```text
待摘要历史 = 输入开头 → 指代锚点之前
原始最近回合 = 指代锚点 → 最后一条真实 user → 压缩触发项之前
```

找不到指代锚点时，原始最近回合仍从最后一条真实 `user` 开始。

“真实 user”不包括：

- developer、system 和内部 reminder。
- 工具输出在协议转换过程中形成的 user 容器。
- 代理生成的兼容标记或控制消息。
- 压缩提示词和 `compaction_trigger`。

指代锚点用于处理以下短回复：

- `按推荐处理`
- `继续`
- `就按这个`
- `同意`

这些回复的具体含义通常位于上一条 assistant 消息中，不能依赖 LLM 摘要一定保留。

### 2. 生成摘要

只把“待摘要历史”和有效压缩提示词发送给摘要模型。最近回合不进入摘要模型输入，避免摘要
再次转述该回合。

### 3. 保存压缩结果

本地压缩结果保存两部分：

```text
summary
  较早历史的 LLM 摘要

retained_tail
  最近回合的原始 Responses items
```

Remote Compaction V2 collector 仍只接收一个 `compaction` item，因此本地兼容载荷需要使用
新的版本化结构保存 `summary` 和 `retained_tail`，不能直接在压缩响应中并列返回多个 output
item。

本地 V3 结构化载荷使用 `codex-elves-compaction-v3:` 前缀，正文为 JSON：

```text
{
  "summary": "较早历史摘要",
  "retained_tail": [原始 Responses items]
}
```

V2 本地兼容桥把该载荷写入单个 `compaction.encrypted_content`。传统本地压缩把同一载荷作为
摘要文本返回；Codex 将其包装成压缩摘要 user 消息后，代理在下一次请求中识别 v3 前缀并恢复。

### 4. 恢复历史

后续请求遇到本地合成压缩项时：

1. 按上游协议把 `summary` 恢复为历史 assistant context 或带来源说明的 user 历史上下文。
2. 按原始顺序追加 `retained_tail` 中的消息和工具记录。
3. 不改变原始 role、item ID、call ID、工具名称、顺序和工具调用配对关系。尾部未超预算时
   内容块保持原样；触发容量裁剪时只替换过大的工具结果或动态工具描述。
4. 删除 Codex 压缩器已经原样保留、同时又出现在 `retained_tail` 中的 user/developer/system
   重复项；优先按 item ID 匹配，其次检查 turn metadata，最后按 role 与完整 content 匹配。
   非 GPT 路径的协议锚点按下述限制处理；GPT 保留已有兼容逻辑。
5. 不把兼容层生成的传输内容写回持久会话。

非 GPT 模型发往上游时，普通请求与再次压缩使用同一套历史展开规则：最新本地合成压缩项
之前的旧会话不再重放，只保留必要指令、有效工具声明，以及可选的纯文本协议锚点；
摘要、`retained_tail` 和压缩后的新输入仍按顺序恢复。GPT 沿用已有兼容逻辑；仅有非本项目
生成的原生密文时，不会触发本项裁剪。

- 旧版 AGENTS、`<INSTRUCTIONS>`、环境上下文与 metadata 标记的指令都需保留；混合
  user 消息按文本块提取指令，不因同一消息含有指令就把旧图片、文件一起带回。
- `additional_tools` 和 `tool_search_output.tools` 中的目录继续有效；后者只恢复为工具声明，
  不恢复旧工具执行结果。请求转换和 JSON/SSE 响应回映射使用同一目录与别名顺序。
- 协议锚点必须是原始、纯文本且序列化 content 不超过 4 KiB 的 user；不截短用户消息，
  不接受文件、未知多模态块或旧摘要包装。找不到时把摘要作为带来源说明的 user 历史上下文。
- HTTP 与 WebSocket 的首次压缩、重试、回退均遵循上述规则，避免再次压缩时重新引入已被
  摘要覆盖的内容，破坏与普通请求共有的缓存前缀。
- 开启最近回合保留时，只摘除尾部对话；尾部的 system/developer 和动态工具目录在协议
  转换前保留，防止强制工具选择失效或缓存字段变化。GPT 本地摘要及校验失败重试则明确
  移除所有工具声明，包括 v3 载荷中的目录，但保留历史工具调用/结果配对。
- 非 GPT 会话切换到 GPT 摘要模型时，仍先按来源会话裁剪已摘要历史；WebSocket 的等待用户
  判断保留原始压缩标识，且不会拦截明确的压缩请求。

### 5. 容量处理

原始最近回合必须在 item 和调用配对层面保持原子性：

- user / assistant 消息原文不裁剪。
- 函数调用参数不裁剪。
- 不删除工具调用或工具输出的一半，不改变 `id`、`call_id`、工具名称和顺序。
- 尾部不超过 `retain_tokens` 时，所有 Responses items 原样保留。

尾部超过 `retain_tokens` 时，将该配置视为裁剪目标：

1. 一旦确认超目标，先扫描全部可裁剪工具结果和动态工具描述，无条件移除其中的图片、音频和视频
   Data URL；该扫描不因中途达到目标而提前结束，避免较小或靠后的媒体载荷原样残留。同一个
   `tool_search_output` 的 `output` 与 `tools` 独立处理。
2. 再优先处理占用最大的工具结果。文本结果保留头尾各一部分并裁掉中间，插入仅含类型和
   估算 Token 数的短标记；短标记格式为 `<truncated:<kind>;~<tokens>t>`。结构化媒体载荷替换
   为 `<truncated:media;~<tokens>t>`。
3. 多个工具结果保留预览后仍超目标时，将最大的结果继续缩成仅保留标记。
4. legacy `tool_result` 只裁剪 `content.content`，保留 `content.tool_use_id`；legacy
   `tool_call` 的 `tool_use.input` 原样保留。
5. `tool_search_output` 只沿嵌套 `tools` 列表裁剪 namespace / function 自身的描述，不进入
   `parameters` 或 `input_schema`；媒体清理同样不进入 schema。工具名称和参数 schema 保持
   不变，保证恢复后仍能注册动态工具。
6. 如果剩余超量来自不可裁剪的 user / assistant 原文或调用参数，则允许软超配置目标，不返回
   `local_compaction_retained_tail_too_large`。

2 MiB 合成载荷上限仍是物理硬限制；超过该上限时明确失败，避免生成无法解码的压缩记录。

## 自动续接规则

这里的“续接”仅指处理 Codex 原生发来的压缩后续请求，不是代理按 GPT 推理 Token 主动追加模型请求。恢复原始最近回合后，按目标上游最终收到的有效尾部角色处理：

- 尾部是原始 user 或合法 tool result：允许自动续接。
- 尾部是 assistant，且上游支持 assistant prefill：允许自动续接。
- 尾部是 assistant，且上游禁止 assistant prefill：停止自动续接，等待用户发送真实消息。

默认不生成虚拟 user 消息。严格上游遇到 assistant 尾部时，代理返回一个没有 output item
的本地 `completed` 响应，结束本次自动续接并等待真实用户消息；不得向持久历史写入空 user、
零宽字符或说明性占位内容。

## 兼容范围

本设计适用于所有最终执行为本地压缩的请求：

- 传统压缩触发。
- Remote Compaction V2 触发后的本地兼容桥。
- HTTP。
- Responses WebSocket。
- Responses、Chat Completions 和 Anthropic 上游协议。

本设计不修改最终执行为远程压缩的原生 Remote Compaction V2 请求。

旧版 `codex-elves-compaction-v1` 和 `codex-elves-compaction-v2` 载荷继续按现有文本摘要方式
读取；新的 `codex-elves-compaction-v3` 结构化载荷使用独立前缀，避免旧会话失效。旧版合成
摘要在禁止 assistant prefill 的模型上无法安全自动续接时，同样停止并等待真实用户消息。

## 测试设计

需要长期保留以下回归测试：

- 模式分类只产生本地压缩和远程压缩两种结果。
- V2 触发经过本地兼容桥后分类为本地压缩。
- 原生 Remote Compaction V2 分类为远程压缩。
- 最近回合从最后一条真实 user 之前最近的可见 assistant 指代锚点开始切分。
- 最近回合不进入摘要模型请求。
- 最近回合以原始 Responses items 保存和恢复。
- Codex 原生保留的 user/developer/system 与 v3 尾部重复时通常只保留一份；若它是摘要前唯一
  的真实 user，则保留原始副本以满足 Anthropic 首条消息约束。
- user、assistant、工具调用和工具输出顺序保持不变。
- 工具调用与输出 ID 配对保持不变。
- 预算内图片和结构化内容块保持原样。
- 超预算文本工具结果保留头尾和裁剪标记，调用配对不变。
- 超预算图片工具结果移除 data URL，保留原工具结果 item 和明确文本标记。
- 超预算函数调用参数仍原样保留，且 `id`、`call_id` 和工具名称不变。
- legacy `tool_call/tool_result` 的嵌套 ID 外壳在裁剪后仍能通过 Chat 与 Anthropic 配对。
- `tool_search_output` 裁剪描述后仍保留工具名称和参数 schema，并能在 Chat 与 Anthropic
  注册动态工具。
- 多个大工具结果会进入第二轮 marker-only 裁剪，直到达到目标。
- 脱敏 25-item 夹具稳定复现裁剪前 45,754 Token，并在裁剪后保留全部 item。
- 不可裁剪的 user / assistant 原文把配置值视为软目标，不因配置值失败。
- 仅超过 2 MiB 物理载荷上限时明确失败。
- 旧版文本压缩载荷仍可恢复。
- 禁止 assistant prefill 的上游不会收到 assistant 结尾的自动续接请求。
- 为避免虚拟用户内容，无法安全续接时停止并等待真实用户消息。
- HTTP 与 WebSocket 得到一致的压缩模式和恢复结果。

## 成功标准

- 用户可见压缩模式只有本地压缩和远程压缩；日志的内部诊断字段不构成第三种模式。
- V2 触发但最终由本地兼容桥生成摘要时，统一显示为本地压缩。
- LLM 摘要不再重复最近一次真实用户回合。
- `按推荐处理`、`继续` 等短回复保留其上一条 assistant 指代对象。
- 最近回合不再被压成 assistant 摘要中的纯文本。
- 本地压缩恢复后保留原始消息和工具协议结构。
- 不再因本地压缩把整个历史收束为 assistant prefill 而触发上游协议错误。
- 不向持久会话写入代理生成的虚拟用户内容。

## 非目标

- 不修改上游原生 Remote Compaction V2 的压缩算法或载荷。
- 不把本地兼容桥作为第三种用户可见压缩模式。
- 本文不定义会话 UI 或代理日志的展示布局。
- 不通过重标角色、空 user、零宽字符或伪造工具调用绕过上游协议校验。
