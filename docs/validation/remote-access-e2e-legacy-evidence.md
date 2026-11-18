# 远程会话 E2E 历史记录

本文保留退役用例、旧版证据约定和当时的验证结果，不作为当前 UI 步骤或验收通过凭证。现行规则见[远程会话真实 E2E 回归](../remote-access-e2e.md)。

## 源码回归对照

2026-10-31，`remote_access` 授权夹具有效期为 60/120 秒，随机密钥共享单项 KDF 缓存。原样并行运行得到 27 passed、9 failed、1 ignored；串行得到 36 passed、0 failed、1 ignored，耗时 167.53 秒。CI 将该组从普通并行测试排除，改为单独串行运行；未修改产品授权期限。当时新增的原生 `outcomeUnknown` 禁止 CLI 重发回归，先复现失败，修复后通过。此处只记录当时源码结果，不代表真实 Chrome 全流程或 Android 真机通过。

## 历史验收范围

2026-11-02 的恢复范围不包含文件审批。该轮新增的文件审批展示及专用测试已撤回，`scenario.approval` 继续验命令审批。更早依赖桌面 `tab.title` 的六个侧边用例也已移除。

## 历史队列取消合同

旧版 `queue.edit_cancel` 使用真实 `fill` 填写“原文＋取消编辑的临时文字”；runner `2.5.4` 与文件证据门禁 `1.3.2` 补充实际值留证、原生编辑事务和完整恢复条件。不能仅凭 DOM 摘要没有临时文字、队列预览相同或 `canceled` 布尔值判通过。

执行旧用例需开启 `allowQueueChanges` 和 `allowDrafts`，main/control 均归属该轮。分别保存两端原普通草稿；仅从空稿通过 UI 填该轮不同标记，不覆盖已有同轮稿；其他来源草稿使案例受阻。取消后比较原稿，返回主会话后须 `loading=false`、编辑器可用、没有编辑或待确认锁且发送按钮可用。只清空该案例由空稿生成的测试稿。

只读原生驱动 `observe-queue-edit-cancel` 提供三份不同读取 ID 的原始观察，`rawFacts` 指向实际文件字段。缺少驱动时在创建活动轮、发送或入队前受阻：

| phase | 必需的原始事实 |
| --- | --- |
| `before` | 当前 `threadId` 及原生顺序的 `queuedMessages`；每项含 `id/clientUserMessageId/input`，`input` 为原生完整序列化输入 |
| `editing` | `editId/editState=editing/originalMessageId/originalClientUserMessageId/originalInput/originalIndex`；与 `before` 的原对象、完整输入和索引匹配才继续 |
| `after` | 同一事务 `editState=canceled`、真实 `cancelReceipt`（外层 `requestId/state` 与原 `result`）、最新完整 `queuedMessages`、`nativeTurns/historyComplete=true`；回执绑定原会话、事务及恢复物理 ID |

每份另有 `runId/caseId/origin/sourceFingerprint/operation/phase/capturedAt/readId`。恢复物理 ID 可不同，但须对应相同客户端逻辑 ID 及完整输入；未编辑项的物理 ID、完整输入及相对位置保持。原生历史中已消费或引导的同客户端逻辑 ID 不能算作恢复队列通过。

取消前单独保存 `*-queue-cancel-editor.json`，字段 `threadId/editId/capturedAt/value` 中的 `value` 取自实际 `contenteditable`，须等于真实填写后的不同正文，事务 ID 来自该阶段原生读取，时间早于取消派发；另外保存带哈希的 observation、DOM 和截图。门禁按固定用例 ID 检查三阶段及编辑器原件；删字段或只报成功不算通过。

旧用例只测原对象仍在忙时队列中恢复；原对象自然消费时保留原现场并记受阻，不重建、取消或重发补过。空闲取消后立即消费、unknown、出队竞争、暂停/send-only、附件及隐藏输入另验。自 runner `2.5.10` 起，`queue.edit_draft` 取代旧 UI 用例，当前流程不依赖“取消编辑”按钮。

## 问答回执刷新历史取证

历史报告使用协议 `2.0`、runner `2.5.3` 和文件证据门禁 `1.3.1`。当时的浏览器观察中，响应派发时间和底层完成时间存在 1.917ms 的反序，不以该样本作容差。原记录引用为 `R3-original-question-recovery/timing-contract-source.json`；这是当时的本机证据名，不在仓库中。

当时分段真实操作验证了既有原题的刷新恢复，原件另经只读审核。此结果不将中断的自动处理器标为通过，不覆盖失败回执、权限变化、竞争或完整套件的其他分支。
