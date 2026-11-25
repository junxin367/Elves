// 输入时提前启动 Claude CLI：用户要求缩短首条消息的等待（CLI 冷启动实测 6–29 秒，带 MCP 服务时更久）。

import { $ } from "../../ui/dom.js";
import { mutate } from "../../core/actions.js";
import { busy, connected, creating, selectedId, session, thread } from "../../core/state.js";
import { isActiveThread, isClaude, threadReadOnly } from "../session/capabilities.js";
import { promptDomText } from "./prompt-editor.js";

// 服务端把没有被使用的预热 CLI 在 2 分钟后收回，所以同一会话 90 秒最多提交一次：
// 输入持续时在收回前续一次，停下来就不再占用；不是每个按键一次请求。
const warmedAt = new Map(), warmIntervalMs = 90 * 1000;
export function warmSessionOnInput() {
  const id = selectedId;
  // 只对已打开、当前空闲且可编辑的 Claude 会话、编辑框里确有文字时提交；旧版服务端没有声明能力就不发请求。
  // 是否已有原生会话、是否被别的应用占用由服务端核对，不满足时静默跳过；这里的失败也不提示，发送时照常启动并再次核对。
  if (!id || !session || !connected || creating || busy || !isClaude() || thread?.id !== id
      || session.capabilities?.warmSession !== true || threadReadOnly(id) || isActiveThread()
      || !promptDomText($("#prompt")).trim()) return;
  const now = Date.now();
  if (now - (warmedAt.get(id) || 0) < warmIntervalMs) return;
  warmedAt.set(id, now);
  if (warmedAt.size > 32) warmedAt.delete(warmedAt.keys().next().value);
  void mutate("warmSession", { threadId: id }).catch(() => {});
}
