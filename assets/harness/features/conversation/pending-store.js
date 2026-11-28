// 待发送持久化：把待发送消息保存到本地，页面重开后恢复。

import { recoveryStorageMessage } from "../../recovery-store.js";
import { recoveryStore } from "../../core/stores.js";
import { recoveryScope } from "../composer/drafts.js";
import { pending, validSkillSelection } from "./pending-state.js";
import { allReceipts, installReceipt, removeReceipt } from "./receipt-registry.js";
import { receiptBelongsHere, receiptNotice, receiptStorageKey } from "./receipt-context.js";
import { goalComposerStorageRecord, validGoalComposerDraft } from "../goal/composer.js";
import { goalComposerAction, goalComposerRecoveries, hydrateGoalComposerRecord } from "../goal/state.js";
import { queuedEditActions } from "../queue/state.js";

// 从会话存储恢复未完成的写操作回执；校验失败时记为无效，交由恢复流程提示用户。
export function restoreStoredPending() {
  try {
    let raw = JSON.parse(sessionStorage.getItem(receiptStorageKey) || "null");
    if (!raw && receiptStorageKey !== "codex-elves-pending") {
      // 早期同源 iframe 共用单键；只迁移未标来源、线程完全相同的旧记录，保留其它窗口的数据。
      const legacy = JSON.parse(sessionStorage.getItem("codex-elves-pending") || "null");
      const entries = Array.isArray(legacy) ? legacy : legacy ? [legacy] : [];
      raw = entries.filter(r => !r?.receiptSurface && receiptBelongsHere(r));
      if (raw.length) {
        sessionStorage.setItem(receiptStorageKey, JSON.stringify(raw));
        const rest = entries.filter(r => !raw.includes(r));
        if (rest.length) sessionStorage.setItem("codex-elves-pending", JSON.stringify(rest.length === 1 ? rest[0] : rest));
        else sessionStorage.removeItem("codex-elves-pending");
      }
    }
    for (const entry of Array.isArray(raw) ? raw : raw ? [raw] : []) {
      const stored = hydrateGoalComposerRecord(entry);
      if (stored && receiptBelongsHere(stored)) installReceipt(typeof stored.id === "string" && /^[a-zA-Z0-9-]{1,100}$/.test(stored.id)
      && (stored.action === "forkThread"
        ? typeof stored.payload?.threadId === "string"
          && (stored.payload.turnId == null || typeof stored.payload.turnId === "string")
        : stored.action === "reorderQueued"
        ? typeof stored.payload?.threadId === "string" && Array.isArray(stored.payload.messageIds)
          && Array.isArray(stored.payload.expectedMessageIds)
          && stored.payload.messageIds.length > 0
          && stored.payload.messageIds.length === stored.payload.expectedMessageIds.length
          && new Set(stored.payload.messageIds).size === stored.payload.messageIds.length
          && stored.payload.messageIds.every(id => typeof id === "string" && stored.payload.expectedMessageIds.includes(id))
        : queuedEditActions.has(stored.action)
        ? typeof stored.payload?.editId === "string" && typeof stored.payload?.threadId === "string"
        : ["steerQueued", "removeQueued"].includes(stored.action)
        ? typeof stored.payload?.threadId === "string" && typeof stored.payload?.messageId === "string"
        : stored.action === "setPermissions" ? typeof stored.payload?.threadId === "string"
        : ["send", "create", "editMessage", "setGoal"].includes(stored.action)
          && typeof stored.payload?.prompt === "string" && validSkillSelection(stored.payload.skills)
          && (stored.action !== "setGoal" || stored.payload.goalMode === true && typeof stored.payload.threadId === "string")
          && (stored.payload.goalMode !== true || goalComposerAction(stored.action, stored.payload)
            && (validGoalComposerDraft(stored.draft) || typeof stored.recoveryRef === "string")))
      ? stored : { id: stored.id, payload: stored.payload, deviceId: stored.deviceId, invalid: true });
    }
  } catch { installReceipt({ invalid: true }); }
}
export function savePending(value, previous = pending) {
  if (value && !Number.isFinite(value.time)) value.time = Date.now();
  // 只改动指定请求。保留单条旧格式；同页多会话并发时保存所有回执，不让后一条覆盖前一条。
  const id = value ? value.id : previous?.id;
  const remaining = allReceipts().filter(r => r.id !== id);
  if (value) remaining.push(value);
  try {
    const entries = remaining.map(goalComposerStorageRecord);
    if (entries.length) sessionStorage.setItem(receiptStorageKey, JSON.stringify(entries.length === 1 ? entries[0] : entries));
    else sessionStorage.removeItem(receiptStorageKey);
  } catch (error) {
    if (value && !value.recoveryRef) throw new Error(recoveryStorageMessage(error, true));
    if (!value && previous) receiptNotice(previous, "无法清理浏览器回执记录，重新打开页面后可能仍需人工核对。", true);
  }
  if (value) installReceipt(value);
  else {
    const scope = previous?.recoveryScope || recoveryScope;
    if (previous?.recoveryRef && scope)
      void recoveryStore.remove(scope, "active", previous.recoveryRef).catch(() => {});
    if (previous?.recoveryRef && scope
        && ![...goalComposerRecoveries.values()].some(record => record.id === previous.id))
      void recoveryStore.remove(scope, "pending", previous.recoveryRef).catch(() => {});
    removeReceipt(previous);
  }
}
