# Codex 会话未读标记：选中会话的蓝点不消失（2026-11-22）

使用者反馈两个现象：从 A 会话点进 B 会话，B 的蓝点不消失，要从 B 再点别的会话才消失；在 B 里等它执行，完成后 B 也留着蓝点，同样要离开 B 才消失。只有 Codex 实例出现，Claude 实例正常。产品规则见[会话正文阅读与未读](../product-standards/core/product-behavior.md#会话正文阅读与未读)。

## 先排除原生已读状态

用真实 Codex CLI（0.167.0-alpha.4.1）的 app-server 做了协议探针：隔离 `CODEX_HOME`，上游换成本地模拟的 Responses 接口，经 stdio 发 JSON-RPC。探针和隔离目录验证后已删除，没有遗留 `codex.exe`。

- 新建、尚无轮次的会话：`thread/read` 和 `thread/start` 的 `readState` 都是 `null`（协议说明“持久已读状态不可用”）。第一轮完成后才有值。
- 一轮完成时的事件顺序固定：`thread/readState/changed`（`firstUnread` 为 `{type:"turn", turnId}`）→ `thread/status/changed`（idle）→ `turn/completed`。也就是说，完成事件到达前，前端已经先收到“未读”。
- `thread/readState/update`（`operation: read`）成功后先推送 `thread/readState/changed`（`firstUnread` 为 `null`），再返回回执；`thread/read`、`thread/list.readStates` 随即一致。用旧的 `revision` 再提交会返回 `readStateConflict`。

再核对使用者本机服务端日志（`codex-elves.log`，2026-11-21 起共 152 条 `markRead`）：Codex 会话 60 次，其中 54 次真的调用了 `thread/readState/update` 并完成；5 次是空操作（对应没有 rollout 的新建会话，`readState` 为 `null`，没有未读可清）；1 次失败是 stdio 兜底路径上的探针会话。服务端提交已读是正常的，问题在前端显示。

## 根因

`renderSidebar` 给选中会话算行状态时，把会话详情里的 `thread.priority` 盖在侧栏条目的 `priority` 上（原意是让审批、问答立即提醒）。`thread.priority` 只是上一次读取会话详情时的快照：

- Codex 会话详情经服务端投影，自带 `priority`（读取时若有未读，就是 `attentionState: "unread"`）；Claude 会话详情没有这个字段，所以 Claude 一直正常。
- 点进 B：详情读到的是未读快照 → 已读回执成功后，前端清除了侧栏条目和 `thread.hasUnreadTurn`，但没有也不会清 `thread.priority` → B 这一行仍按旧快照显示蓝点。切到 C 后，B 不再是选中会话，这一行改用已被清除的侧栏条目，蓝点才消失。与使用者描述完全一致。
- 停在 B 等执行完成：执行期间读到的快照是“执行中”，完成事件、已读回执都只更新侧栏条目，这一行停在旧快照上；完成后重新读取详情若落在已读回执被处理之前，又会拿到“未读”快照。两种情况都要离开 B 才消失。

附带发现：同一个覆盖会让选中会话在新一轮开始后不显示执行圈（详情里仍是读取时的空闲快照），选中会话也会因此一直被算作需要放进“优先事项”、不能被“清除已读会话”清除。

## 修复

`attentionFor` 只借用选中会话的 `thread.requests`（审批、问答仍立即提醒），其余状态一律以侧栏条目为准——它随目录刷新、`turn/started`、`turn/completed`、`thread/readState/changed` 事件和已读回执实时更新。修改位置 `assets/harness/features/sidebar/list.js`，已递增网页资源版本。对 Claude 没有行为变化（它的详情没有 `priority`，原来就回退到侧栏条目）。

## 验证

- 新增 3 条网页状态测试，用真实的侧栏渲染代码和状态函数：从其他会话点进未读会话后蓝点随已读回执消失；执行中停留在原会话，按真实协议顺序（未读 → 完成）走完后不残留执行圈、已读后不残留蓝点；选中会话收到新一轮开始事件后立即显示执行圈。临时恢复旧逻辑时这 3 条全部失败，恢复修复后通过。
- 网页状态测试全量 670 项，665 通过；失败的 5 项是“Claude 额度底栏”相关，来自另一个会话尚未提交的 `claude-usage.js` 改动，在 HEAD 基线上全部通过，与本次无关。网页资源静态检查通过。

## 未覆盖

- 没有在真实 Windows 客户端或浏览器里点击验证，需要重新编译客户端后确认。
- 原生先发“未读”再发完成事件，所以正在查看并跟随底部的会话，在已读回执返回前这一行可能短暂出现蓝点，回执成功后消失；窗口隐藏、最小化或滚离底部时完成，仍按规范保留未读，回到底部或恢复可见后清除。
