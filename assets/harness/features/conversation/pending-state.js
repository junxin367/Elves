// 待发送状态：发送中的消息与计时器、发送请求、入参校验与发送成功确认。

import { notice } from "../../ui/notice.js";
import { updateComposer } from "../../core/actions.js";
import { selectedId, session, turns } from "../../core/state.js";
import { api } from "../api.js";
import { claudeThreadId } from "../session/capabilities.js";

export let pendingTimer = null;
export function setPendingTimer(value) { return pendingTimer = value; }
export function validSkillSelection(value) {
  return value == null || (Array.isArray(value) && value.length <= 16 && value.every(skill =>
    skill && typeof skill.name === "string" && skill.name.length > 0 && skill.name.length <= 256
      && typeof skill.path === "string" && skill.path.length > 0 && skill.path.length <= 4096));
}
export function validQueuedImageSources(value) {
  return Array.isArray(value) && value.length <= 4
    && value.every(source => typeof source === "string" && source.length > 0 && source.length <= 32_767
      && !/[\u0000-\u001f\u007f]/u.test(source))
    && new Set(value).size === value.length;
}
export let pending = null;
export function setPending(value) { return pending = value; }
export let pendingSendRequest = null;
export function setPendingSendRequest(value) { return pendingSendRequest = value; }
export function reportMutationError(error) {
  if (error.pendingFeedback) { updateComposer(); return; }
  notice(error.message, true);
}
export function acceptedSendItem(items, clientId) {
  return items.find(item => (item.type === "userMessage"
    || item.type === "steeringUserMessage" && item.status === "accepted")
    && !item.remotePendingInput && !item.remoteCompactionInput
    && (item.clientId === clientId || item.clientUserMessageId === clientId));
}
export async function confirmPendingSend(current, result = {}) {
  const threadId = current.payload?.threadId;
  const clientId = result?.clientUserMessageId ?? current.payload?.clientUserMessageId;
  // 两种原生实例的历史编辑都必须确认新消息已接收，原消息或排队记录不能充当成功证据。
  const historicalEdit = current.action === "editMessage";
  if (current.action !== "send" && !historicalEdit || typeof threadId !== "string"
      || typeof clientId !== "string" || !/^[a-zA-Z0-9_-]{1,120}$/.test(clientId)
      || result?.threadId && result.threadId !== threadId
      || result?.clientUserMessageId && current.payload?.clientUserMessageId
        && result.clientUserMessageId !== current.payload.clientUserMessageId) return null;
  if (current.deviceId && current.deviceId !== session?.device.id) return null;
  current.confirmationClientUserMessageId = clientId;
  // 原生接受事件已到达时立即收敛回执；不再被慢速或未返回的 sendState 查询挡住。
  // 仅使用稳定轮次下的真实用户消息，排除发送占位和等待准入，保留未知结果不重发。
  const accepted = selectedId === threadId
    ? acceptedSendItem([...turns.values()].filter(turn => (turn.id || turn.turnId)
      && !turn.pendingUserMessageAdmission).flatMap(turn => turn.items || []), clientId) : null;
  if (accepted) return { ok: true, confirmed: true, deliveryState: "accepted",
    threadId, clientUserMessageId: clientId, messageId: accepted.serverUserMessageId || accepted.id };
  if (pendingSendRequest?.current === current) return pendingSendRequest.promise;
  const promise = (async () => {
    if (session.capabilities?.queueSendRecovery === true) {
      const state = await api("/api/read/sendState", { threadId, clientUserMessageId: clientId });
      return state.confirmed === true && state.threadId === threadId
        && state.clientUserMessageId === clientId
        && (historicalEdit ? state.deliveryState === "accepted" : ["queued", "accepted"].includes(state.deliveryState))
        ? state : null;
    }
    const queue = await api("/api/read/queue", { threadId });
    const queued = queue.threadId === threadId && (queue.queuedMessages || []).find(message =>
      (message.clientUserMessageId ?? message.submissionOptions?.clientUserMessageId
        ?? (!message.serverQueuedMessageId ? message.id : null)) === clientId);
    if (queued && !historicalEdit) return { ...queue, ok: true, confirmed: true, deliveryState: "queued",
      clientUserMessageId: clientId, messageId: queued.id };
    const history = await api("/api/read/history", { threadId, cursor: null });
    const delivered = history.threadId === threadId
      && acceptedSendItem((history.data || []).flatMap(turn => turn.items || []), clientId);
    return delivered ? { ok: true, confirmed: true, deliveryState: "accepted",
      threadId, clientUserMessageId: clientId, messageId: delivered.id } : null;
  })();
  const request = { current, promise }; pendingSendRequest = request;
  try { return await promise; }
  finally { if (pendingSendRequest === request) pendingSendRequest = null; }
}
// Claude 服务端按原生证据判定这条消息没有送达（会话已空闲，且始终没有接收记录）。
// 用户要求 Harness 自己判定送达结果，不再让用户去原生客户端核对（2026-11-25）：调用方据此恢复输入并解除锁定，
// 绝不自动重发。只认服务端对同一线程、同一消息标识的明确答复；原生记录晚到时服务端会改回已接收，见 queue_send_state。
export async function sendNotDelivered(current) {
  const threadId = current.payload?.threadId, clientId = current.payload?.clientUserMessageId;
  if (current.action !== "send" && current.action !== "editMessage" || typeof threadId !== "string"
      || typeof clientId !== "string" || !claudeThreadId(threadId)
      || session?.capabilities?.queueSendRecovery !== true
      || current.deviceId && current.deviceId !== session?.device.id) return false;
  const state = await api("/api/read/sendState", { threadId, clientUserMessageId: clientId });
  return state?.threadId === threadId && state.clientUserMessageId === clientId
    && state.deliveryState === "notDelivered" && state.confirmed !== true;
}
