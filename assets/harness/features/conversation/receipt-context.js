// 回执的界面归属：已有会话用原线程，新建首条还没有线程时用原项目和实例。授权变化不能借用旧记录。
import { creating, selectedId, session, setBusy, setStopping } from "../../core/state.js";
import { notice, syncNoticeOwner } from "../../ui/notice.js";
import { creationBackend, creationInstanceId, creationProject } from "../composer/creation-state.js";
import { setSending } from "../composer/send-state.js";
import { privateInputScope } from "../session/capabilities.js";

export function receiptSurfaceForLocation(address) {
  return address?.pathname === "/side-thread"
    ? `side:${new URLSearchParams(address.hash.slice(1)).get("thread") || ""}` : "main";
}
// 同源侧边 iframe 与主页面共享 sessionStorage 和 IndexedDB；固定入口线程，不随父会话、显隐或选择改变。
export const receiptSurface = receiptSurfaceForLocation(globalThis.location);
export const receiptStorageKey = `codex-elves-pending${receiptSurface === "main" ? "" : `:${receiptSurface}`}`;
export const goalReceiptStorageKey = `codex-elves-goal-composer-recoveries${receiptSurface === "main" ? "" : `:${receiptSurface}`}`;
export function clearStoredReceipts(scope) {
  // 只有显式退出或换授权时才清理同源主/侧边存档；别的窗口已写入的新授权记录必须留下。
  const bases = ["codex-elves-pending", "codex-elves-goal-composer-recoveries"];
  try {
    const keys = new Set([...bases, receiptStorageKey, goalReceiptStorageKey]);
    for (let i = 0; i < sessionStorage.length; i++) {
      const key = sessionStorage.key(i);
      if (bases.some(base => key?.startsWith(`${base}:side:`))) keys.add(key);
    }
    for (const key of keys) {
      const raw = sessionStorage.getItem(key);
      if (!raw) continue;
      let values;
      try { values = JSON.parse(raw); } catch { sessionStorage.removeItem(key); continue; }
      const kept = (Array.isArray(values) ? values : [values]).filter(r =>
        scope && (r?.operationScope || r?.recoveryScope) && (r.operationScope || r.recoveryScope) !== scope);
      if (!kept.length) sessionStorage.removeItem(key);
      else sessionStorage.setItem(key, JSON.stringify(Array.isArray(values) ? kept : kept[0]));
    }
  } catch { /* 无存储权限时仍清理内存与当前授权的 IndexedDB。 */ }
}
export function receiptBelongsHere(record) {
  return record?.receiptSurface ? record.receiptSurface === receiptSurface
    : receiptSurface === "main" || receiptSurface === `side:${record?.payload?.threadId}`;
}
export function receiptOwnerKey(payload) {
  if (typeof payload?.threadId === "string" && payload.threadId) return JSON.stringify(["thread", payload.threadId]);
  if (!payload || !("projectId" in payload || "backend" in payload)) return null;
  const backend = payload.backend || "codex";
  return JSON.stringify(["new", backend, payload.instanceId || (backend === "claude" ? session?.claudeInstanceId : null)
    || `${backend}:local`, payload.projectId || ""]);
}
export function receiptInView(record) {
  if (!record || !receiptBelongsHere(record) || (record.deviceId && record.deviceId !== session?.device?.id)
      || (record.operationScope || record.recoveryScope) && (record.operationScope || record.recoveryScope) !== privateInputScope(session)) return false;
  const owner = receiptOwnerKey(record.payload);
  if (!owner) return false; // 不猜损坏记录属于当前会话；可恢复的原件按请求 ID 从 IndexedDB 补回。
  return owner === receiptOwnerKey(selectedId ? { threadId: selectedId } : creating
    ? { backend: creationBackend, instanceId: creationInstanceId, projectId: creationProject } : null);
}
const activities = new Set(), deferredErrors = new Map();
function canShowReceipt(record) {
  // 隐藏 iframe 的 document.hidden 不会改变；等外层真的显示这个标签再提示其失败。
  return receiptInView(record) && !globalThis.document?.hidden && !globalThis.document?.querySelector?.(".workspace")?.hidden && (receiptSurface === "main"
    || globalThis.document?.documentElement?.dataset?.workspaceSidePanelVisible === "true");
}
export function captureReceiptOwner(payload) {
  return { payload, deviceId: session?.device?.id, operationScope: privateInputScope(session), receiptSurface };
}
export function receiptNotice(record, message, error = false) {
  if (!record || !session || !receiptBelongsHere(record) || record.deviceId && record.deviceId !== session.device.id
      || (record.operationScope || record.recoveryScope) && (record.operationScope || record.recoveryScope) !== privateInputScope(session)) return;
  if (canShowReceipt(record)) { notice(message, error, () => canShowReceipt(record)); return; }
  // 不把后台成功提示带到另一会话；失败保留原文，回到来源会话再提示。
  const key = receiptOwnerKey(record.payload);
  if (error && key) deferredErrors.set(key, { record: { payload: { threadId: record.payload?.threadId, projectId: record.payload?.projectId,
    backend: record.payload?.backend, instanceId: record.payload?.instanceId }, operationScope: record.operationScope || record.recoveryScope,
    deviceId: record.deviceId, receiptSurface: record.receiptSurface }, message });
}
export function syncReceiptActivity() {
  const current = [...activities].filter(receiptInView);
  setBusy(current.some(a => a.kind !== "stop"));
  setStopping(current.some(a => a.kind === "stop"));
  setSending(current.find(a => a.submission)?.submission || null);
  syncReceiptNotices();
}
export function syncReceiptNotices() {
  syncNoticeOwner();
  for (const [key, entry] of deferredErrors) if (canShowReceipt(entry.record)) {
    deferredErrors.delete(key);
    receiptNotice(entry.record, entry.message, true);
  }
}
export function beginReceiptActivity(payload, kind = "write", submission = null) {
  const token = { ...captureReceiptOwner(payload), kind, submission };
  activities.add(token); syncReceiptActivity();
  return () => { if (activities.delete(token)) syncReceiptActivity(); };
}
export function clearReceiptActivity() {
  activities.clear(); deferredErrors.clear(); syncReceiptActivity();
}
