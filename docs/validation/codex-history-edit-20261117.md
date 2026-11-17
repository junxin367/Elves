# Codex 历史消息编辑验收

日期：2026-11-17。

## Codex App 实现核对

只读检查本机 `OpenAI.Codex 26.1111.2228.0` 的 `app.asar`：

- `webview/assets/async/907538.0572f09d40.js` 中的 `editLastUserTurn` 已接收 `allowEarlierMessages`。启用后可定位较早轮次，不再只允许最后一条；目标及后续轮次执行中时拒绝编辑。
- 分页历史调用 `thread/revert`，参数为 `threadId`、`beforeTurnId`；旧历史使用 `thread/rollback(numTurns)`。
- 回退原轮次及之后的历史，再在同一会话启动新轮次。替换原输入的第一段文字、清空该段文字定位信息，同时保留其余输入。
- 本项目只使用独立 CLI / app-server 的分页接口，不接入 App 的 owner / follower 转发、内部状态或桌面桥接。

官方接口说明参考 Codex App Server 文档的 `thread/revert`、`thread/turns/list`、`thread/items/list`、`turn/start`。具体参数及行为另以本次原生 CLI 实测核对。

## 实现范围

Codex 主会话各历史轮次的原始用户消息使用现有编辑入口，包括分页加载的旧消息。编辑开始时保存原生历史头和新客户端消息 ID；提交时恢复独立引擎写入归属，直读核对历史与空闲状态，分页找到原轮次，再回退并重发。

执行、压缩、队列、未完成队列编辑、审批或未确认发送存在时拒绝回退。回退后清理整个被移除后缀的计划、压缩记录及页面缓存，保留之前的历史；同代次迟到事件不能恢复被移除轮次。

重发意图先落盘，不展示为待发送队列，也不被队列工作线程重放。新客户端消息 ID 用于幂等及送达核对；成功发送保留原图片和 Skill。修改文字在提交前持久保存，失败恢复不覆盖普通草稿；恢复文字不代表原附件也已恢复，页面明确提示重新核对。

不新增分支会话，不恢复工作区文件，不开放临时侧边或只读子智能体的历史编辑。

## 验证结果

- `node --test --test-reporter=dot scripts/harness-ui-state.test.mjs`：603 项通过。
- 新增服务端历史编辑回归：3 项通过，覆盖 105 轮跨页历史、原图片和 Skill 保留、模型与思考深度、重复提交、历史头失效、错误消息身份、执行中拒绝，以及原生接收后回执丢失的核对。
- 既有历史分页、子智能体只读写入保护及 Projection 回归：16 项通过，包含回退后旧计划与迟到压缩事件的过滤。
- `node scripts/check-harness-assets.mjs`：148 个网页资源通过，139 个 JS 文件的语法、导入导出、分层、循环依赖及预加载检查通过。
- `git diff --check` 通过。全仓 `cargo fmt --all -- --check` 存在已有格式差异，本次未扩大为无关格式整理。

原生验证使用独立 `CODEX_HOME`、Codex CLI `0.164.0` 及回环模拟 Responses 上游，不使用真实账号授权或修改现有会话：

1. 通过原生 `thread/start` 和三次 `turn/start` 生成三轮会话。
2. 用 `thread/turns/list` 和 `thread/items/list` 读取原生分页内容。
3. 对第二轮调用 `thread/revert(beforeTurnId)`；原生剩余轮次仅为第一轮。
4. 在同一会话发送修改内容，原生最终保留第一轮及新轮次，共两轮。
5. 核对第四次上游请求：包含修改后的问题，不包含被移除的第二、三轮问题；收到原生 `thread/reverted` 通知。

上述原生验证覆盖独立 CLI 接口和模型上下文；Harness 服务端由隔离引擎回归覆盖。未进行安装版、正式鉴权网页点击或 Android 真机验收，未构建安装包、安装软件或发版。

临时原生验证脚本已删除。自动审批阻止了隔离数据目录的递归及逐项清理，仅返回 `blocked by policy`；剩余数据保留于被 Git 忽略的 `temp/codex-history-edit-20261117/native-home/`，不纳入提交。
