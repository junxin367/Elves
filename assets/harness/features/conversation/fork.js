// 持久主会话分支：两个入口共用原生分支和可靠回执，不自动发送消息。
import { $ } from "../../ui/dom.js";
import { notice } from "../../ui/notice.js";
import { closeComposerMenus, loadSidebar, mutate, selectThread, updateComposer } from "../../core/actions.js";
import { busy, connected, selectedId, selectionToken, session, stopping, thread } from "../../core/state.js";
import { pending } from "./pending-state.js";
import { privateInputScope, threadForkAllowed, validRelatedThreadId } from "../session/capabilities.js";

export function forkAvailable(id = selectedId) {
  return threadForkAllowed(id) && connected && !busy && !stopping && !pending;
}
export function validForkResult(result, sourceId) {
  return result?.ok === true && result.status === "created" && result.sourceThreadId === sourceId
    && validRelatedThreadId(result.threadId) && result.threadId !== sourceId
    && result.conversationId === result.threadId;
}
export async function openForkResult(record, result) {
  if (!validForkResult(result, record.payload.threadId))
    throw new Error("分支回执身份不匹配，请核对原请求，不要重复创建");
  const scope = privateInputScope(session);
  // 迟到的创建结果可刷新列表，但不能把用户从后来选择的会话或新授权强行带走。
  if (record.deviceId !== session?.device.id || record.operationScope !== scope) return;
  void Promise.resolve(loadSidebar()).catch(error => {
    if (scope === privateInputScope(session)) notice(`分支已创建，会话列表刷新失败：${error.message}`, true);
  });
  if (selectedId === record.payload.threadId && selectionToken === record.selectionToken) {
    await selectThread(result.threadId);
    if (scope === privateInputScope(session) && selectedId === result.threadId)
      $("#prompt")?.focus({ preventScroll: true });
  }
  if (scope === privateInputScope(session)) {
    const warning = [result.warning, result.permissionWarning].filter(value => typeof value === "string" && value).join("；");
    notice(warning || "已在新聊天中分支", !!warning);
  }
}
export async function forkThread(turnId = null, sourceId = selectedId) {
  if (sourceId !== selectedId || thread?.id !== sourceId || !forkAvailable(sourceId)) return;
  const record = { payload: { threadId: sourceId, ...(turnId ? { turnId } : {}) },
    selectionToken, deviceId: session.device.id, operationScope: privateInputScope(session) };
  closeComposerMenus();
  try {
    // 创建前保存原回执；断网或超时只查询此请求，不能重新 fork。
    const request = mutate("forkThread", record.payload, { reliable: true });
    updateComposer();
    const result = await request;
    await openForkResult(record, result);
  } catch (error) {
    if (record.operationScope === privateInputScope(session)) notice(error.message, true);
  } finally {
    if (record.operationScope === privateInputScope(session)) updateComposer();
  }
}
