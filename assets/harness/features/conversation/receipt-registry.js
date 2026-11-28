// 每个请求保留自己的记录和只读重查定时器；pending 只是当前输入框的投影，不是整个工作区的写锁。
import { session } from "../../core/state.js";
import { pending, setPending } from "./pending-state.js";
import { privateInputScope } from "../session/capabilities.js";
import { receiptInView, receiptOwnerKey, syncReceiptActivity } from "./receipt-context.js";

const records = new Map(), admissions = new Map(), timers = new Map();
export function allReceipts() {
  return pending && !records.has(pending.id) ? [...records.values(), pending] : [...records.values()];
}
export function receiptById(id) { return records.get(id) || (pending?.id === id ? pending : null); }
export function receiptForPayload(payload) {
  if (!pending && !records.size) return null;
  const key = receiptOwnerKey(payload), scope = privateInputScope(session);
  return allReceipts().find(r => (!r.deviceId || r.deviceId === session?.device?.id)
    && (!(r.operationScope || r.recoveryScope) || (r.operationScope || r.recoveryScope) === scope)
    && key && receiptOwnerKey(r.payload) === key) || null;
}
export function reserveReceipt(payload) {
  const key = JSON.stringify([privateInputScope(session), receiptOwnerKey(payload)]), token = {};
  if (admissions.has(key) || receiptForPayload(payload)) return null;
  admissions.set(key, token);
  return () => { if (admissions.get(key) === token) admissions.delete(key); };
}
export function syncReceiptView() {
  setPending([...records.values()].find(receiptInView) || null);
}
export function syncReceiptSelection() { syncReceiptView(); syncReceiptActivity(); }
export function installReceipt(record) {
  records.set(record.id, record);
  syncReceiptView();
}
export function removeReceipt(record) {
  if (!record) return;
  records.delete(record.id); clearTimeout(timers.get(record.id)); timers.delete(record.id);
  if (pending?.id === record.id) setPending(null);
  syncReceiptView();
}
export function retryReceipt(record, delay, check) {
  clearTimeout(timers.get(record.id));
  timers.set(record.id, setTimeout(() => {
    timers.delete(record.id);
    const current = receiptById(record.id);
    if (current) void check(current);
  }, delay));
}
export function stopReceiptChecks() { for (const timer of timers.values()) clearTimeout(timer); timers.clear(); }
export function clearReceiptRegistry() {
  stopReceiptChecks(); records.clear(); admissions.clear(); setPending(null);
}
