# Claude 重装后会话再次显示未读调查记录

日期：2026-11-18（Asia/Shanghai）。

## 调查问题

重新安装并打开 Elves 后，Claude 会话再次显示未读。需要确认：

1. 未读状态是否只是安装前已经持久化的状态；
2. 启动恢复或重连是否把已读会话误判为新内容；
3. 当前代码、安装包和本机状态是否一致。

本轮仅做只读调查，没有清理会话状态、修改代码或批量标记已读。

## 结论

### 已确认

Claude 蓝点来自 Harness 保存的 `Conversation.unread` 字段，不是安装程序临时生成的 UI 状态。

状态文件位于：

```text
C:\Users\junes\.codex-session-delete\remote-claude-sessions.json
```

Windows 安装/卸载只删除安装目录和快捷方式，保留用户目录中的设置、授权、原生会话和 WebView 缓存。因此重新安装不会清除 Claude 的未读字段。

启动时 `Claude::new()` 读取该文件并恢复会话；对共享会话不会把 `unread` 重置为 `false`。随后侧栏直接输出 `hasUnreadTurn`，所以此前持久化为未读的会话会在重新打开后继续显示蓝点。

### 当前本机快照

调查时读取到：

- Claude 会话总数：83；
- `unread=true`：7；
- 7 个未读会话全部为 `shared=true`；
- 7 个未读会话全部为 `discovered=false`；
- 未读会话的 `updatedAt` 分布在 2026-11-14 至 2026-11-18；
- 状态文件大小约 41.2 MiB，最后写入时间为 2026-11-18 21:15:27。

这些数据更符合历史未读状态被保留，而不是安装时统一重新生成未读。

### 尚未完全闭环

调查没有保存重装前的状态文件快照，因此无法证明用户当时看到的每一个具体会话都已经是已读。

日志中有一个当前仍为未读的 Claude 会话，在 2026-11-18 17:19:48 出现过成功的 `markRead` 回执。这个事实留下两个候选解释：

- `markRead` 之后又有真实原生内容到达；
- 后续原生历史快照存在差异，被 `shared_history_has_new_content()` 再次判为新内容，或发生并发状态写回。

当前日志没有保存对应的原生快照前后差异，因此不能把该个案确定为启动误判。

## 代码因果链

### 状态加载和持久化

- `crates/elves-core/src/paths.rs:15-29`
  - 默认状态目录为用户 Home 下的 `.codex-session-delete`。
- `crates/elves-harness/src/claude.rs:162`
  - `Conversation` 持久化 `unread: bool`。
- `crates/elves-harness/src/claude.rs:550-586`
  - `Claude::new()` 读取 `remote-claude-sessions.json`；
  - 共享会话只清理临时预览，不清除 `unread`。
- `crates/elves-harness/src/claude.rs:907-925`
  - 会话变更通过原子写入持续保存。

### 侧栏显示

- `crates/elves-harness/src/claude.rs:384-400`
  - `Conversation::view()` 将 `self.unread` 输出为 `hasUnreadTurn`。
- `crates/elves-harness/src/claude.rs:1414-1463`
  - 侧栏合并时直接使用保存的会话视图。

### 已读操作

- `crates/elves-harness/src/claude.rs:2454-2478`
  - `markRead` 才会将 `c.unread = false` 并持久化。
- `assets/harness/features/messages/user-content.js:164-192`
  - 前端只对当前选中、已加载、可见、连接正常且未被只读/删除/隐藏条件拦截的会话调用 `markRead`；
  - 重新启动不会批量清除其他历史会话的未读。

### 原生快照回放

- `crates/elves-harness/src/claude.rs:2920-3076`
  - 快照历史发生变化时，按 `c.unread || shared_history_has_new_content(...)` 保留或新增未读。
- `crates/elves-harness/src/claude.rs:3507-3615`
  - 比较稳定的原生用户 UUID、消息类型和消息 ID；
  - 忽略状态、连接代次、client ID、发送时间和后台状态等展示元数据；
  - 真正新增正文或正文增长仍会置未读。

### 安装行为

- `scripts/installer/windows/Elves.nsi:121-136`
  - 卸载删除安装目录、快捷方式和注册表项；
  - 明确保留用户目录中的设置、密钥、原生会话和 WebView 缓存。

## 验证结果

当前代码已包含提交 `4344749b`：

```text
fix(会话): 修复 Claude 已读会话的未读标记回弹
```

该提交已包含在当前 `HEAD db1b9f14` 的祖先中。

已运行以下回归测试，均通过：

```text
claude::tests::shared_unread_tracks_new_content_not_previews_status_or_reconnections ... ok
claude::tests::shared_unread_background_state_changes_remain_read_after_restart ... ok
```

每项结果均为 `1 passed; 0 failed`。

测试覆盖：

- 已读状态持久化后重新创建 `Claude`；
- 重连和连接代次变化；
- 临时预览删除；
- 后台任务状态变化；
- 真正新增回复、工具输出和任务完成通知。

## 已排除解释

### 安装包没有包含最新未读修复

未支持该解释。当前分支已经包含 `4344749b`，对应重启和重连回归测试通过。

### 普通重启必然把已读会话重新标记为未读

未复现。现有测试中的已读会话经过 `Claude::new()`、重连和状态恢复后仍保持已读。

### 重新安装会清除并重建会话状态

未支持该解释。安装脚本不清理用户状态目录，当前磁盘仍存在完整的 `remote-claude-sessions.json`。

## 下次复查清单

如果再次出现“重装前已读、重装后未读”，应优先保存以下证据，再进行任何清理：

1. 重装前复制并计算 `remote-claude-sessions.json` 的 SHA-256；
2. 记录目标会话的 `id`、`unread`、`updatedAt`、`turns` 数量和最后一轮 ID；
3. 记录重装前最后一次 `markRead` 的时间和回执；
4. 记录新进程启动时间；
5. 启动后第一次侧栏快照和第一次原生历史快照；
6. 对比第一次出现 `unread=true` 的具体状态写入；
7. 若历史投影发生变化，保存 `shared_history_has_new_content()` 比较前后的摘要。

在没有上述前后快照前，不应直接删除状态文件或批量强制标记已读，因为这样会丢失判断“状态保留”与“启动误判”所需的证据。

## 本次交付边界

- 调查记录已保存为本文件；
- 未修改产品代码；
- 未清理用户本机 Claude 会话状态；
- 未批量标记会话已读；
- 本文记录的是 2026-11-18 的本机快照，后续状态数量和日志时间可能变化。
