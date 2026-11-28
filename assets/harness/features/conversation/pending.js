// 写操作与回执：发送/创建/编辑等写入的幂等提交与待确认回执的核对；待发送状态在 pending-state.js，本地保存与恢复在 pending-store.js。

import { recoveryStorageMessage } from "../../recovery-store.js";
import {
  applyQueuedSendResult, reloadThread, renderMessages, selectThread, updateComposer
} from "../../core/actions.js";
import { creating, selectedId, selectionToken, selectionVersion, session, thread } from "../../core/state.js";
import { recoveryStore } from "../../core/stores.js";
import { randomId } from "../../core/util.js";
import { api, operationErrorState, receiptResultState } from "../api.js";
import { openForkResult, validForkResult } from "./fork.js";
import { clearSubmittedDraft, recoverFailedInput, restorePrivateInputs } from "../composer/draft-restore.js";
import {
  recoveryGeneration, setRecoveryScope, smallInputStorageRecord, snapshotDraft, submittedDrafts
} from "../composer/drafts.js";
import { selectCreatedThread } from "../composer/new-chat-settings.js";
import { confirmPendingSend, pending, sendNotDelivered } from "./pending-state.js";
import { receiptById, reserveReceipt, retryReceipt } from "./receipt-registry.js";
import { receiptNotice, receiptSurface } from "./receipt-context.js";
import { savePending } from "./pending-store.js";
import { applyGoalComposerReceipt, updateGoalComposerPending } from "../goal/composer-submission.js";
import { goalComposerAction, goalReceiptOutcome, goalSubmissionState, goalVersion } from "../goal/state.js";
import { cancelMessageEdit, editingMessage } from "../messages/message-edit.js";
import { applyQueueReorderResult, applyQueueSnapshot, confirmQueuedSteerDisplay, reloadQueue } from "../queue/queue.js";
import { checkQueuedMessageEdit } from "../queue/queued-edit-recovery.js";
import { completeQueuedEditAction, finishQueuedMessageEdit } from "../queue/queued-edit.js";
import { clearSentMessageDisplays } from "../queue/sent-display.js";
import {
  pendingSteer, queueSnapshotIsVersioned, queueVersion, queuedEditActions, queuedEdits,
  setPendingSteer
} from "../queue/state.js";
import { antigravityThreadId, claudeThreadId, privateInputScope, threadActionReadOnly } from "../session/capabilities.js";

export async function mutate(action, payload, { reliable = false, receiptId = null } = {}) {
  if (payload?.threadId && threadActionReadOnly(action, payload.threadId))
    throw Object.assign(new Error("此会话仅供查看，不能提交操作。"), { code: 403, notSubmitted: true });
  const queueAction = ["steerQueued", "removeQueued"].includes(action);
  const trackedOperation = queueAction || ["deleteThread", "createSideThread", "closeSideThread", "forkThread"].includes(action);
  const version = selectionVersion;
  const deviceId = session?.device.id, generation = recoveryGeneration;
  const operationScope = privateInputScope(session);
  const currentAuthorization = () => generation === recoveryGeneration && deviceId === session?.device.id
    && operationScope === privateInputScope(session);
  const id = receiptId || randomId();
  const goalAction = goalComposerAction(action, payload);
  if (reliable) {
    const release = reserveReceipt(payload);
    if (!release) throw Object.assign(new Error(queueAction ? "队列操作仍待确认，请先查询原回执，不要重复引导或删除。"
      : "此会话还有待确认的操作，请勿重复提交。"), { notSubmitted: true });
    try {
      const record = {
        id, action, payload, deviceId: session.device.id, selectionToken, operationScope, receiptSurface, time: Date.now(),
        // AGY 普通消息受理期间只复用发送按钮，不把“等待开始/响应”做成回执提示；传输或送达未知另行处理。
        ...(!goalAction && (["send", "editMessage"].includes(action) && antigravityThreadId(payload.threadId)
          || action === "create" && payload.backend === "antigravity") ? { agySending: true } : {}),
        ...(["send", "create", "setGoal", "editMessage"].includes(action) ? {
          // 历史回退事件会重新加载正文；先保存编辑文字，失败或刷新后不能随旧消息节点一起丢失。
          draft: action === "editMessage"
            ? { text: payload.prompt, skills: [], attachments: [], goalMode: null }
            : submittedDrafts.get(payload) || snapshotDraft(),
          ...(goalAction ? { goalReadVersion: goalVersion } : {})
        } : {})
      };
      if (record.draft) {
        const currentScope = privateInputScope(session);
        try {
          await recoveryStore.activate(currentScope, { replace: false });
          if (generation !== recoveryGeneration || deviceId !== session?.device.id)
            throw new Error("设备授权已更换，操作未提交。");
          setRecoveryScope(currentScope);
          record.recoveryScope = currentScope;
          await recoveryStore.put(currentScope, [{ kind: "pending", id, value: record },
            { kind: "active", id, value: { id } }]);
          record.recoveryRef = id;
        } catch (error) {
          if (generation !== recoveryGeneration || deviceId !== session?.device.id || error.name === "RecoveryAuthorizationError") throw error;
          record.recoveryScope = currentScope;
          if (!smallInputStorageRecord(record)) throw error;
          // 大小受控的记录仍可使用会话存储回执；大图片必须先可靠存入 IndexedDB。
          receiptNotice(record, "浏览器本地草稿存储不可用；本次回执将保存在会话存储，未发送草稿仅保留在本页。", true);
        }
        if (generation !== recoveryGeneration || deviceId !== session?.device.id)
          throw new Error("设备授权已更换，操作未提交。");
      }
      savePending(record);
    } catch (error) {
      error.message = recoveryStorageMessage(error, true);
      error.notSubmitted = true;
      if (goalAction) Object.assign(error, { notSubmitted: true, goalReceipt: {
        state: "failed", result: { ok: false, message: error.message, code: "receipt-storage-unavailable" }
      } });
      throw error;
    } finally { release(); }
  }
  const own = () => reliable && currentAuthorization() ? receiptById(id) : null;
  const ownsPending = () => !!own();
  let result;
  const posting = own();
  if (posting) postingReceipts.add(posting);
  try { result = await api(`/api/action/${action}`, payload, id); }
  catch (error) {
    if (!currentAuthorization()) throw error;
    const failureState = operationErrorState(error);
    if (error.notSubmitted && failureState === "failed") {
      if (goalAction) Object.assign(error, { receiptId: id, goalReceipt: {
        state: "failed", result: { ok: false, notSent: true, message: error.message }
      } });
      if (ownsPending()) {
        const unsent = own();
        markReceiptFeedback(unsent, "failed", { ok: false, notSent: true, message: error.message, code: error.code });
        try {
          await recoverFailedInput(unsent);
          if (own() === unsent) savePending(null, unsent);
        } catch (restoreError) {
          receiptNotice(unsent, restoreError.message, true);
          retryReceipt(unsent, 5000, resolvePending);
        }
      }
      throw error;
    }
    if (goalAction && ownsPending()) {
      const receipt = { state: failureState,
        result: { ok: false, message: error.message, code: error.code, outcomeUnknown: failureState === "unknown" } };
      Object.assign(error, { goalReceipt: receipt, receiptId: id, result: receipt.result, outcomeUnknown: failureState === "unknown" });
      updateGoalComposerPending({ ...own(), result: receipt.result, receiptState: receipt.state });
      throw error;
    }
    if (trackedOperation) {
      Object.assign(error, { receiptId: id, operationState: failureState, outcomeUnknown: failureState === "unknown" });
    }
    if (reliable) {
      if (failureState === "failed" && ownsPending()) {
        const rejected = own();
        markReceiptFeedback(rejected, "failed", { ok: false, message: error.message, code: error.code });
        try {
          await recoverFailedInput(rejected);
          if (own() === rejected) savePending(null, rejected);
        } catch (restoreError) {
          receiptNotice(rejected, restoreError.message, true);
          retryReceipt(rejected, 5000, resolvePending);
        }
      } else {
        if (own()?.agySending) own().agySending = false;
        if (ownsPending() && error.operationState !== "failed") {
          // 先核对原结果；能够立即查明的断线不短暂显示人工核对横幅。
          markReceiptFeedback(own(), "checking");
          error.pendingFeedback = true;
        }
        if (ownsPending()) void resolvePending(own());
        if (action === "send" && failureState !== "failed") error.deliveryPending = true;
      }
    }
    throw error;
  } finally { if (posting) postingReceipts.delete(posting); }
  if (!currentAuthorization()) throw Object.assign(new Error("设备授权已更换，已忽略旧回执。"), { staleAuthorization: true });
  result = { ...result, state: receiptResultState(result) };
  // 删除回执必须匹配原请求且具有明确状态；缺字段或串回执不能被当成成功并清理会话。
  if (action === "deleteThread" && (result?.requestId !== id
      || !["pending", "unknown", "failed", "completed"].includes(result?.state))) {
    throw Object.assign(new Error("删除回执不完整或与原请求不匹配，请核对原回执，勿重复删除。"),
      { receiptId: id, operationState: "unknown", outcomeUnknown: true });
  }
  if (goalAction) {
    const data = result.result || {};
    const goalSubmission = goalSubmissionState(data.goalSubmission, action);
    if (ownsPending()) updateGoalComposerPending({ ...own(), result: data, receiptState: goalReceiptOutcome(result, action) === "unknown" ? "unknown" : result.state,
      ...(goalSubmission ? { goalSubmission } : {}) });
    if (goalReceiptOutcome(result, action) === "succeeded") return data;
    throw Object.assign(new Error(data.message || (result.state === "pending"
      ? "目标提交仍在处理，请稍后查回执" : "目标未确认完成，草稿与回执已保留")), {
      goalReceipt: result, receiptId: id, result: data, goalSubmission,
      code: data.code, outcomeUnknown: ["pending", "unknown"].includes(goalReceiptOutcome(result, action))
    });
  }
  if (result.state === "pending") {
    markReceiptFeedback(own(), "pending");
    const confirmed = (action === "send" || action === "editMessage") && ownsPending()
      ? await confirmPendingSend(own(), result.result).catch(() => null) : null;
    if (confirmed && ownsPending()) result = { state: confirmed.ok === false ? "failed" : "completed", result: confirmed };
    else {
      if (ownsPending()) void resolvePending(own());
      throw Object.assign(new Error("操作仍在处理，请稍后查看会话或回执"), { deliveryPending: action === "send",
        pendingFeedback: ownsPending(),
        ...(trackedOperation ? { receiptId: id, operationState: "pending", outcomeUnknown: true } : {}),
        ...(["approve", "answer"].includes(action) ? { approvalState: "pending" } : {}) });
    }
  }
  if (result.state === "unknown") {
    if (own()?.agySending) own().agySending = false;
    const confirmed = (action === "send" || action === "editMessage") && ownsPending()
      ? await confirmPendingSend(own(), result.result).catch(() => null) : null;
    if (confirmed && ownsPending()) result = { state: confirmed.ok === false ? "failed" : "completed", result: confirmed };
    else {
      markReceiptFeedback(own(), "checking");
      if (ownsPending()) void resolvePending(own());
      throw Object.assign(new Error(result.result?.message || "送达状态未知，请在会话中确认，勿重复发送"), {
        pendingFeedback: ownsPending(),
        ...(trackedOperation ? { receiptId: id, operationState: "unknown", outcomeUnknown: true } : {}),
        ...(["approve", "answer"].includes(action) ? { approvalState: "unknown" } : {}),
        deliveryPending: action === "send"
          && typeof (result.result?.clientUserMessageId ?? payload.clientUserMessageId) === "string"
      });
    }
  }
  if (action === "forkThread" && result.state === "completed" && !validForkResult(result.result, payload.threadId)) {
    markReceiptFeedback(own(), "unknown");
    if (ownsPending()) void resolvePending(own());
    throw Object.assign(new Error("分支回执不完整或身份不匹配，请核对原请求，不要重复创建"),
      { receiptId: id, outcomeUnknown: true });
  }
  let recovered = true;
  if (ownsPending() && result.state === "failed") {
    const rejected = own();
    markReceiptFeedback(rejected, "failed", result.result);
    try { await recoverFailedInput(rejected); }
    catch (restoreError) {
      recovered = false;
      receiptNotice(rejected, restoreError.message, true);
      retryReceipt(rejected, 5000, resolvePending);
    }
  }
  if (ownsPending() && recovered) savePending(null, own());
  if (result.state === "failed") {
    if (action === "create" && result.result?.threadId && creating && selectionVersion === version) {
      await selectCreatedThread(result.result.threadId);
    }
    throw Object.assign(new Error(result.result?.message || "原生操作未完成"), {
      result: result.result, goalSubmission: result.result?.goalSubmission, code: result.result?.code,
      receiptId: id, operationState: "failed",
      ...(result.result?.notSent === true ? { notSent: true } : {}),
      ...(["approve", "answer"].includes(action) ? { approvalState: "failed" } : {})
    });
  }
  return result.result;
}
const postingReceipts = new WeakSet();
function markReceiptFeedback(record, state, result) {
  if (!record || receiptById(record.id) !== record || record.receiptState === state && !record.receiptMissing) return;
  if (record.receiptState === "failed" && state !== "failed") return;
  // 只记同一请求的实际查询/派发结果。保持对象身份，以免打断正在按此记录只读核对的任务。
  record.receiptState = state;
  if (state === "failed") record.result = result;
  delete record.receiptMissing;
  try { savePending(record); }
  catch { receiptNotice(record, "未能更新本地回执状态，原请求仍保留，请保持页面打开。", true); }
}
export async function resolvePending(current = pending, nativeEvidence = false) {
  if (!current || !session) return;
  if (!nativeEvidence && checkingReceipts.has(current)) return checkingReceipts.get(current);
  const check = checkOriginalReceipt(current);
  checkingReceipts.set(current, check);
  try { await check; } finally { if (checkingReceipts.get(current) === check) checkingReceipts.delete(current); }
}
const checkingReceipts = new WeakMap();
async function checkOriginalReceipt(current) {
  const notice = (message, error = false) => receiptNotice(current, message, error);
  if ((current.operationScope || current.recoveryScope) && (current.operationScope || current.recoveryScope) !== privateInputScope(session)) return;
  if (current.recoveryUnavailable) {
    try { await restorePrivateInputs(session); }
    catch (error) { notice(recoveryStorageMessage(error), true); return; }
    const restored = receiptById(current.id);
    if (!restored || restored === current || restored.recoveryUnavailable) { notice("本地恢复记录仍不可读取，原请求标识已保留，请先核对本机会话。", true); return; }
    return resolvePending(restored);
  }
  const deviceId = session.device.id;
  const queueAction = ["steerQueued", "removeQueued"].includes(current.action);
  const operationScope = privateInputScope(session), generation = recoveryGeneration;
  const version = selectionVersion;
  const queueReadVersion = queueVersion;
  if (current.invalid || (current.deviceId && current.deviceId !== deviceId)) {
    notice("回执记录损坏或属于旧设备授权。请先检查原生会话，再人工解除锁定；不会自动重发。", true);
    updateComposer(); return;
  }
  const stillCurrent = () => receiptById(current.id) === current && session?.device.id === deviceId
    && generation === recoveryGeneration && operationScope === privateInputScope(session);
  if (!stillCurrent()) return;
  const retry = delay => retryReceipt(current, delay, resolvePending);
  try {
    // 已有明确失败结果时，只重试本地输入恢复。存储/读取失败不能把失败重新解释为“送达未知”。
    let receipt = current.receiptState === "failed" ? { state: "failed", result: current.result }
      : await api(`/api/receipts/${current.id}`);
    if (!stillCurrent()) return;
    receipt = { ...receipt, state: receiptResultState(receipt) };
    if (typeof current.agySending === "boolean") current.agySending = receipt.state === "pending";
    if (goalComposerAction(current.action, current.payload)) {
      const outcome = await applyGoalComposerReceipt(current, receipt, { allowSelection: true });
      if (outcome === "pending" && receiptById(current.id)) retry(2000);
      return;
    }
    if ((current.action === "send" || current.action === "editMessage")
        && ["pending", "unknown"].includes(receipt.state)) {
      const confirmed = await confirmPendingSend(current, receipt.result);
      if (!stillCurrent()) return;
      if (confirmed) receipt = { state: confirmed.ok === false ? "failed" : "completed", result: confirmed };
      else if (receipt.state === "unknown") {
        // 服务端已按原生证据结案为“没有送达”：与明确失败同样处理，恢复输入并解除锁定，不重发。
        const abandoned = await sendNotDelivered(current).catch(() => false);
        if (!stillCurrent()) return;
        if (abandoned) receipt = { state: "failed", result: { ok: false, notSent: true, code: "not-delivered",
          message: "消息没有送达，内容已保留，可以恢复后重新发送。" } };
        else {
          markReceiptFeedback(current, "unknown");
          if (typeof (receipt.result?.clientUserMessageId ?? current.payload?.clientUserMessageId) === "string") retry(5000);
          else notice("回执缺少客户端消息标识，无法自动核对。请在会话中确认后再解除锁定。", true);
          updateComposer(); return;
        }
      }
    }
    if (receipt.state === "pending") {
      markReceiptFeedback(current, "pending");
      retry(2000); if (pending === current) updateComposer(); return;
    }
    if (receipt.state === "unknown") {
      markReceiptFeedback(current, "unknown");
      if (queuedEditActions.has(current.action)) {
        if (session.capabilities?.queueEditRecovery === true) {
          const state = await checkQueuedMessageEdit(current.payload.threadId, current.payload.editId);
          if (stillCurrent() && state?.operationPending) retry(2000);
        } else notice("队列编辑结果尚未确认，请更新远程服务后核对；不会重复保存或恢复消息。", true);
      } else if (queueAction || current.action === "create" && typeof current.agySending === "boolean") retry(5000);
      updateComposer(); return;
    }
    if (receipt.state === "failed") {
      markReceiptFeedback(current, "failed", receipt.result);
      try { await recoverFailedInput(current); }
      catch (error) {
        notice(error.message || "输入恢复失败，原请求回执已保留，请勿重新发送。", true);
        retry(5000); updateComposer(); return;
      }
    }
    if (!stillCurrent()) return;
    if (queueAction && receipt.state === "completed"
        && (receipt.result?.threadId !== current.payload.threadId
          || receipt.result?.messageId !== current.payload.messageId)) {
      markReceiptFeedback(current, "unknown");
      notice("队列操作回执的会话或消息不匹配，原请求标识已保留，请继续核对原回执。", true);
      updateComposer(); return;
    }
    if (current.action === "forkThread") {
      if (receipt.state === "completed" && !validForkResult(receipt.result, current.payload.threadId)) {
        markReceiptFeedback(current, "unknown");
        notice("分支回执身份不匹配，原请求已保留，不会重复创建", true);
        updateComposer(); return;
      }
      savePending(null, current);
      if (receipt.state === "completed") await openForkResult(current, receipt.result);
      else notice(receipt.result?.message || "分支未创建", true);
      updateComposer(); return;
    }
    savePending(null, current);
    if (queueAction) {
      const id = current.payload.threadId;
      if (receipt.state === "completed") {
        if (current.action === "steerQueued")
          confirmQueuedSteerDisplay(id, current.payload.messageId, receipt.result);
        if (selectedId === id && thread?.id === id) {
          const result = receipt.result;
          if (current.selectionToken === selectionToken && queueReadVersion === queueVersion || queueSnapshotIsVersioned(result))
            applyQueueSnapshot(id, Array.isArray(result.queuedMessages) ? result.queuedMessages
              : (thread.queuedMessages || []).filter(message => message?.id !== current.payload.messageId),
            result.queuedEdits, result);
          if (pendingSteer?.threadId === id && pendingSteer.messageId === current.payload.messageId) {
            setPendingSteer(null); renderMessages();
          }
          const refresh = current.action === "steerQueued" ? reloadThread() : reloadQueue();
          void refresh.catch(error => {
            if (deviceId === session?.device.id && generation === recoveryGeneration
                && operationScope === privateInputScope(session)) notice(error.message, true);
          });
        }
      } else notice(receipt.result?.message || "队列操作未完成，请核对原消息", true);
      updateComposer(); return;
    }
    if (current.action === "reorderQueued") {
      if (receipt.state === "completed") {
        applyQueueReorderResult(current.payload.threadId, receipt.result,
          current.selectionToken === selectionToken ? queueReadVersion : -1);
      } else {
        if (selectedId === current.payload.threadId) void reloadQueue().catch(error => notice(error.message, true));
        notice(receipt.result?.message || "队列排序未完成，请重新核对顺序", true);
      }
      updateComposer();
      return;
    }
    if (queuedEditActions.has(current.action)) {
      if (receipt.state === "completed") {
        completeQueuedEditAction(current.action, current.payload, receipt.result,
          current.selectionToken === selectionToken ? queueReadVersion : -1);
        // 结果已反映在队列或编辑器里，自动恢复不额外发“回执已确认”通知。
      } else {
        if (current.action === "beginQueuedEdit")
          finishQueuedMessageEdit(queuedEdits.get(current.payload.threadId));
        notice(receipt.result?.message || "队列编辑操作未完成，草稿已保留", true);
      }
      updateComposer();
      return;
    }
    const queued = receipt.state === "completed" && current.action === "send"
      && applyQueuedSendResult(current.payload.threadId, receipt.result,
        current.selectionToken === selectionToken ? queueReadVersion : -1, current.payload);
    if (receipt.state === "completed") {
      if (current.action === "editMessage") {
        clearSentMessageDisplays(current.payload.threadId, { retire: true });
        if (editingMessage?.threadId === current.payload.threadId) cancelMessageEdit({ focus: false });
      } else clearSubmittedDraft(current.payload,
        (selectedId === current.payload.threadId || (creating && current.action === "create")) && current.selectionToken === selectionToken);
    } else notice(receipt.result?.message || "上一条消息未发送", true);
    if (current.action === "create" && receipt.result?.threadId && creating
        && current.selectionToken === selectionToken && version === selectionVersion) await selectCreatedThread(receipt.result.threadId);
    else if (selectedId === current.payload.threadId) {
      if (current.action === "editMessage" && receipt.state === "completed")
        await selectThread(selectedId, { closeSidebar: false, force: true });
      else if (!queued) await reloadThread();
    }
    updateComposer();
  } catch (error) {
    if (!stillCurrent()) return;
    // 建立写请求和只读恢复可能并行。POST 尚未返回时，读不到回执不能凭空制造“送达未知”。
    if (postingReceipts.has(current)) { retry(2000); return; }
    if (!current.receiptMissing) markReceiptFeedback(current, "unknown");
    if (current.agySending) { current.agySending = false; updateComposer(); }
    if (error.code === 404) {
      if (goalComposerAction(current.action, current.payload)) {
        updateGoalComposerPending({ ...current, receiptState: "unknown" });
        notice("未找到原目标回执。草稿和原请求标识已保留，请在本机核对；不会自动重复设置或重发首条。", true);
        updateComposer();
        return;
      }
      if (queueAction) {
        // 旧引导可能根本未派发；不能凭 404 判断成功，只在原生历史确认已接收且队列已移除后结束等待。
        if (claudeThreadId(current.payload.threadId) || antigravityThreadId(current.payload.threadId)) {
          const queue = await api("/api/read/queue", { threadId: current.payload.threadId }).catch(() => null);
          if (!stillCurrent()) return;
          if (queue?.threadId === current.payload.threadId && Array.isArray(queue.queuedMessages)
              && !queue.queuedMessages.some(message => message.id === current.payload.messageId)) {
            const history = await api("/api/read/history", { threadId: current.payload.threadId }).catch(() => null);
            if (!stillCurrent()) return;
            const received = history?.threadId === current.payload.threadId && Array.isArray(history.data)
              && history.data.some(turn => turn.items?.some(item => item.type === "userMessage"
                && item.clientId === current.payload.messageId && !item.remotePendingInput));
            if (received) {
              savePending(null, current);
              confirmQueuedSteerDisplay(current.payload.threadId, current.payload.messageId,
                { ok: true, threadId: current.payload.threadId, messageId: current.payload.messageId });
              if (pendingSteer?.threadId === current.payload.threadId
                  && pendingSteer.messageId === current.payload.messageId) setPendingSteer(null);
              if (selectedId === current.payload.threadId && thread?.id === selectedId) {
                if (queueReadVersion === queueVersion || queueSnapshotIsVersioned(queue))
                  applyQueueSnapshot(selectedId, queue.queuedMessages, queue.queuedEdits, queue);
                else void reloadQueue().catch(() => {});
                renderMessages();
              }
              updateComposer();
              return;
            }
          }
        }
        // 未确认的原请求继续保留为页面内提示，避免每次恢复连接都弹同一条红色错误。
        if (!current.receiptMissing) savePending({ ...current, receiptMissing: true });
        updateComposer(); return;
      }
      if (queuedEditActions.has(current.action) && session.capabilities?.queueEditRecovery === true)
        await checkQueuedMessageEdit(current.payload.threadId, current.payload.editId);
      else if ((current.action === "send" || current.action === "editMessage" && antigravityThreadId(current.payload?.threadId))
          && current.payload?.clientUserMessageId) {
        const confirmed = await confirmPendingSend(current).catch(() => null);
        if (confirmed?.ok === false && stillCurrent()) {
          try { await recoverFailedInput(current); }
          catch (failure) { notice(failure.message, true); retry(5000); return; }
          if (!stillCurrent()) return;
          savePending(null, current);
          notice(confirmed.message, true);
        } else if (confirmed && stillCurrent()) {
          savePending(null, current);
          applyQueuedSendResult(current.payload.threadId, confirmed,
            current.selectionToken === selectionToken ? queueReadVersion : -1, current.payload);
          clearSubmittedDraft(current.payload,
            selectedId === current.payload.threadId && current.selectionToken === selectionToken);
          updateComposer();
          if (selectedId === current.payload.threadId) await reloadThread();
        } else if (stillCurrent()) {
          // 回执已丢失时同样只认服务端按原生证据的结案；否则继续只读核对。
          if (await sendNotDelivered(current).catch(() => false) && stillCurrent()) {
            try { await recoverFailedInput(current); }
            catch (failure) {
              notice(failure.message || "输入恢复失败，原请求回执已保留，请勿重新发送。", true);
              retry(5000); updateComposer(); return;
            }
            if (!stillCurrent()) return;
            savePending(null, current);
            notice("消息没有送达，内容已保留，可以恢复后重新发送。", true);
          } else retry(5000);
        }
      }
      else notice("未找到上一条消息的回执。不会自动重发；请先检查会话。", true);
      updateComposer();
    } else { if (pending === current) updateComposer(); retry(5000); }
  }
}
