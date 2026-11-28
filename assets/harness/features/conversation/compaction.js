// 上下文压缩：压缩状态与回执、生命周期事件、横幅与压缩命令。

import { confirmAction } from "../../ui/dialog.js";
import { button, element } from "../../ui/dom.js";
import { notice } from "../../ui/notice.js";
import { applyQueuedSendResult, reloadThread, updateComposer } from "../../core/actions.js";
import { busy, connected, selectedId, selectionVersion, session, stopping, thread } from "../../core/state.js";
import { randomId } from "../../core/util.js";
import { api, operationErrorState, receiptResultState } from "../api.js";
import { modelIdentityMatches } from "../composer/model-identity.js";
import {
  modelChoiceVersion, selectedEffort, selectedModel, setSelectedEffort, setSelectedModel
} from "../composer/options-state.js";
import { pendingFeedbackReady } from "../composer/send-state.js";
import { pending } from "./pending-state.js";
import { claudeNativeQueue, isActiveThread, isClaude, privateInputScope, threadReadOnly } from "../session/capabilities.js";
import { currentThreadDisplay } from "../session/thread-cache.js";

export let compactionVersion = 0;
export const compactions = new Map();
// 从会话存储恢复进行中或结果未知的上下文压缩记录。
export function restoreCompactions() {
  try {
    const stored = JSON.parse(sessionStorage.getItem("codex-elves-compact-pending") || "[]");
    if (Array.isArray(stored)) for (const value of stored) {
      if (typeof value?.threadId !== "string" || !value.threadId
          || !["pending", "running", "unknown"].includes(value.state)) continue;
      const receiptId = typeof value.receiptId === "string" && /^[a-zA-Z0-9-]{1,100}$/.test(value.receiptId)
        ? value.receiptId : null;
      compactions.set(value.threadId, {
        state: value.state, threadId: value.threadId, receiptId,
        deviceId: typeof value.deviceId === "string" ? value.deviceId : null,
        acknowledged: value.acknowledged === true, checkPendingReceipt: value.state === "pending" && !!receiptId,
        turnId: typeof value.turnId === "string" ? value.turnId : null,
        itemId: typeof value.itemId === "string" ? value.itemId : null,
        baselineTurnId: typeof value.baselineTurnId === "string" ? value.baselineTurnId : null,
        baselineItemId: typeof value.baselineItemId === "string" ? value.baselineItemId : null,
        time: Number.isFinite(value.time) ? value.time : undefined
      });
    }
  } catch {}
}
export function normalizeCompaction(value) {
  const state = ["pending", "running", "unknown"].includes(value?.state) ? value.state : "idle";
  return {
    state,
    turnId: typeof value?.turnId === "string" ? value.turnId : null,
    itemId: typeof value?.itemId === "string" ? value.itemId : null
  };
}
export function currentCompaction(id = selectedId) {
  if (!id) return { state: "idle", turnId: null, itemId: null };
  return compactions.get(id) || (thread?.id === id ? normalizeCompaction(thread.compaction) : { state: "idle", turnId: null, itemId: null });
}
export function compactionLocked(id = selectedId) {
  return ["pending", "running", "unknown"].includes(currentCompaction(id).state);
}
export function compactionInProgress(value = currentCompaction()) {
  if (isClaude()) return value.state === "running";
  // 用户要求压缩时显示可用的停止按钮；未确认送达或结果未知的压缩不能凭本地锁定状态发起停止。
  return value.state === "running" || value.state === "pending" && (value.acknowledged || value.nativeConfirmed);
}
export function compactionQueueSendReason(id = selectedId) {
  // Claude 普通发送继续由原生实际执行状态决定是否排队；压缩回执不能阻塞聊天主流程。
  if (isClaude() && claudeNativeQueue()) return "";
  // 用户要求主会话与临时侧边在已确认压缩期间均可排队；只保留回执、授权及服务能力校验。
  const current = currentCompaction(id);
  if (!compactionLocked(id)) return "";
  if (current.deviceId && current.deviceId !== session?.device.id)
    return "压缩回执属于旧设备授权，可继续编辑草稿；请重新读取或在本机核对后再发送。";
  if (current.state === "unknown")
    return "压缩结果仍待确认，可继续编辑草稿；不会自动重发压缩请求，请查回执、重新读取或在本机核对后再发送。";
  if (current.state === "pending" && !current.acknowledged && !current.nativeConfirmed)
    return "正在提交压缩请求，可继续编辑草稿。";
  if (session?.capabilities?.compactionQueueSend !== true)
    return "当前服务尚不支持压缩期间发送排队消息，可继续编辑草稿；请更新远程服务或等待压缩完成。";
  return "";
}
export function persistCompactions(required = false) {
  const stored = [...compactions.values()].filter(value => compactionLocked(value.threadId)).map(value => ({
    state: value.state, threadId: value.threadId, receiptId: value.receiptId || null,
    deviceId: value.deviceId || null, acknowledged: value.acknowledged === true,
    turnId: value.turnId || null, itemId: value.itemId || null,
    baselineTurnId: value.baselineTurnId || null, baselineItemId: value.baselineItemId || null,
    time: value.time
  }));
  try {
    if (stored.length) sessionStorage.setItem("codex-elves-compact-pending", JSON.stringify(stored));
    else sessionStorage.removeItem("codex-elves-compact-pending");
  } catch {
    if (required) throw new Error("无法保存压缩回执，压缩请求未提交。请允许此网页使用浏览器会话存储。");
    notice("无法保存压缩回执；请保持页面打开，并在本机核对压缩状态。", true);
  }
}
function setCompactionState(id, value, { required = false } = {}) {
  const normalized = {
    ...value, ...normalizeCompaction(value), threadId: id,
    receiptId: typeof value?.receiptId === "string" ? value.receiptId : null,
    deviceId: typeof value?.deviceId === "string" ? value.deviceId : null,
    acknowledged: value?.acknowledged === true,
    nativeConfirmed: value?.nativeConfirmed === true,
    time: Number.isFinite(value?.time) ? value.time : Date.now(),
    baselineTurnId: typeof value?.baselineTurnId === "string" ? value.baselineTurnId : null,
    baselineItemId: typeof value?.baselineItemId === "string" ? value.baselineItemId : null
  };
  const previous = compactions.get(id);
  compactions.set(id, normalized);
  try {
    persistCompactions(required);
  } catch (error) {
    // 回执未能持久保存时请求不会提交，内存占位也必须回退，不能把输入区永久锁在“压缩中”。
    if (previous) compactions.set(id, previous);
    else compactions.delete(id);
    throw error;
  }
  ++compactionVersion;
  if (thread?.id === id) thread.compaction = normalizeCompaction(normalized);
  return normalized;
}
function compactionAdvanced(current, projection) {
  if (!projection.itemId && !projection.turnId) return false;
  return projection.itemId !== current.baselineItemId || projection.turnId !== current.baselineTurnId;
}
export function reconcileCompaction(id, value) {
  const projection = normalizeCompaction(value), current = compactions.get(id);
  if (projection.state !== "idle") {
    if (current?.state === "running" && projection.state === "pending") {
      if (thread?.id === id) thread.compaction = normalizeCompaction(current);
      return current;
    }
    return setCompactionState(id, {
      ...current, ...projection,
      turnId: projection.turnId || (projection.itemId && projection.itemId === current?.itemId ? current.turnId : null),
      // 原生读取确认的 pending 与本页尚未送达的请求分开，不能改写回执确认状态。
      nativeConfirmed: current?.nativeConfirmed === true
        || !current?.receiptId || compactionAdvanced(current, projection)
    });
  }
  if (!current || current.state === "idle" || !current.receiptId || current.state === "running"
      || current.acknowledged || compactionAdvanced(current, projection)) {
    return setCompactionState(id, projection);
  }
  if (thread?.id === id) thread.compaction = normalizeCompaction(current);
  return current;
}
export function renderCompactionBanner(root, id, compaction) {
  if (!id || !compactionLocked(id)) return;
  const unavailable = compaction.deviceId && session?.device.id && compaction.deviceId !== session.device.id;
  // 用户要求：Claude 已受理、尚未开始不是异常，空闲多久都不提示；真实结果未知或授权异常仍保留恢复入口。
  if (!unavailable && isClaude() && compaction.state === "pending"
      && thread?.nativeCommands?.some(command => command.name === "compact" && command.state === "pending")) return;
  const sendReason = compactionQueueSendReason(id);
  if (compaction.state === "running" && !unavailable && !sendReason) return;
  const awaitingReceipt = compaction.state === "unknown"
    || compaction.state === "pending" && !compaction.acknowledged && !compaction.nativeConfirmed;
  if (!unavailable && awaitingReceipt && !pendingFeedbackReady(compaction)) return;
  if (!unavailable && !awaitingReceipt && !sendReason) return;
  const messages = {
    pending: awaitingReceipt ? "正在确认压缩结果。" : sendReason,
    running: sendReason || "正在压缩此会话的上下文；可继续编辑并发送普通消息，消息会加入原生队列，在压缩完成后执行。",
    unknown: "正在确认压缩结果。"
  };
  const box = element("div", "pending-banner", unavailable
    ? `${sendReason}不会自动重发压缩请求。`
    : messages[compaction.state]);
  if (!unavailable && compaction.receiptId && ["pending", "unknown"].includes(compaction.state)) {
    const check = button("查回执", "只查询压缩请求回执，不会重新提交", () => void resolveCompactionReceipt(id), "text-button");
    check.disabled = !connected;
    box.append(check);
  }
  const reload = button("重新读取", "从原生会话重新读取压缩状态", () => void recoverCompaction(id), "text-button");
  reload.disabled = !connected || selectedId !== id;
  box.append(reload);
  if (unavailable || compaction.state === "unknown") {
    box.append(button("已人工确认", "确认在本机核对后解除压缩锁定", async () => {
      const current = compactions.get(id);
      if (await confirmAction("解除压缩锁定", "请先检查本机原生会话。解除锁定不会重新压缩；仅在已确认压缩已完成或未执行后继续。", "已核对，解除锁定", true)
          && compactions.get(id) === current) {
        setCompactionState(id, { state: "idle", turnId: current?.turnId, itemId: current?.itemId });
        updateComposer();
      }
    }, "text-button"));
  }
  root.append(box);
}
async function applyCompactionReceipt(id, receiptId, receipt) {
  const current = compactions.get(id);
  if (!current || current.receiptId !== receiptId || current.state === "idle"
      || current.deviceId && current.deviceId !== session?.device.id) return;
  receipt = { ...receipt, state: receiptResultState(receipt) };
  const scope = privateInputScope(session);
  const refresh = async () => {
    if (selectedId !== id || privateInputScope(session) !== scope) return;
    try { await reloadThread(); }
    catch (error) {
      // 压缩受理与后续读取分开处理；读取失败不能把已确认回执倒退成送达未知。
      if (selectedId === id && privateInputScope(session) === scope)
        notice(`压缩回执已确认，但会话状态读取失败：${error.message}`, true);
    }
  };
  if (receipt.state === "pending") {
    current.checkPendingReceipt = true;
    updateComposer();
    return;
  }
  if (receipt.state === "unknown") {
    // 已看见同次原生开始时，未知的只是送达回执；保留回执身份，不撤销正在执行的事实。
    if (current.state === "running" && current.liveItemId && current.liveItemId === current.itemId) return;
    setCompactionState(id, { ...current, state: "unknown" });
    updateComposer();
    return;
  }
  if (receipt.state === "failed") {
    setCompactionState(id, { state: "idle", turnId: current.turnId, itemId: current.itemId });
    notice(receipt.result?.message || "原生压缩请求未完成", true);
    updateComposer();
    await refresh();
    return;
  }
  if (receipt.state !== "completed") {
    setCompactionState(id, { ...current, state: "unknown" });
    notice("压缩回执状态无法识别。不会自动重发，请在本机核对。", true);
    updateComposer();
    return;
  }
  if (receipt.result?.deliveryState === "queued") {
    // 用户要求执行中手动压缩与消息共用队列；入队只确认等待，不能提前锁定压缩或向正文插入压缩事件。
    if (current.state !== "running" && !current.nativeConfirmed)
      setCompactionState(id, { state: "idle", turnId: current.baselineTurnId, itemId: current.baselineItemId });
    updateComposer();
    if (session?.capabilities?.compactionItemEvents && connected && selectedId === id) {
      // 入队只同步队列，不为一个队列标记重新下载历史；旧服务仍按原方式核对。
      applyQueuedSendResult(id, { ...receipt.result, queueSyncPending: true }, null);
      return;
    }
    await refresh();
    return;
  }
  const acknowledged = setCompactionState(id, { ...current, acknowledged: true });
  if (receipt.result?.compaction) reconcileCompaction(id, receipt.result.compaction);
  updateComposer();
  const latest = compactions.get(id);
  if (latest?.receiptId !== acknowledged.receiptId) return;
  const expected = receipt.result?.nativeCommand?.id || receipt.result?.compaction?.itemId;
  // 只有同次在线条目才证明客户端已看到开始；pending、入队或 running 回执本身都不是正文证据。
  const seen = value => value?.state === "running" && value.liveItemId && value.liveItemId === value.itemId
    && (!expected || expected === "pending-manual-context-compaction" || expected === value.liveItemId);
  if (seen(latest)) return;
  if (session?.capabilities?.compactionItemEvents && connected && selectedId === id
      && ["pending", "running"].includes(latest.state)) {
    if (latest.startReadScheduled) return;
    latest.startReadScheduled = true;
    const selection = selectionVersion;
    // 原生开始事件可能晚于受理。给小事件 3 秒先到，漏事件才补一次只读历史；不延迟渲染、不重发压缩。
    setTimeout(() => {
      const value = compactions.get(id);
      if (value?.receiptId !== receiptId) return;
      delete value.startReadScheduled;
      if (connected && selection === selectionVersion && !seen(value)) void refresh();
    }, 3000);
    return;
  }
  await refresh();
}
const compactionReceiptChecks = new Set();
export function checkPendingCompactionReceipt(id) {
  const current = compactions.get(id);
  // 只在服务已答 pending 或重新打开后借既有连接探测读取原回执；正常的压缩 POST 还在途时不发额外查询。
  if (current?.checkPendingReceipt && current.state === "pending" && !current.nativeConfirmed && !current.acknowledged
      && current.deviceId === session?.device?.id) return resolveCompactionReceipt(id, { background: true });
}
async function resolveCompactionReceipt(id, { background = false } = {}) {
  const current = compactions.get(id);
  if (!current?.receiptId || !session || !connected) return;
  if (current.deviceId && current.deviceId !== session.device.id) {
    notice("压缩回执属于旧设备授权。请在本机核对后人工解除锁定。", true);
    return;
  }
  if (compactionReceiptChecks.has(current.receiptId)) return;
  compactionReceiptChecks.add(current.receiptId);
  const scope = privateInputScope(session);
  try {
    const receipt = await api(`/api/receipts/${current.receiptId}`, undefined, undefined, undefined,
      background ? { resourcePriority: "background" } : undefined);
    if (privateInputScope(session) !== scope || background && selectedId !== id) return;
    await applyCompactionReceipt(id, current.receiptId, receipt);
  } catch (error) {
    const latest = compactions.get(id);
    if (privateInputScope(session) !== scope || latest?.receiptId !== current.receiptId
        || background && (selectedId !== id || latest.state !== "pending" || latest.acknowledged || latest.nativeConfirmed)) return;
    if (latest.state === "pending" && !latest.acknowledged && !latest.nativeConfirmed)
      setCompactionState(id, { ...latest, state: "unknown" });
    if (selectedId !== id) return;
    updateComposer();
    // 真正未知由原压缩区域提示一次，读取失败不再叠加一条“请查回执”通知。
  } finally { compactionReceiptChecks.delete(current.receiptId); }
}
async function recoverCompaction(id) {
  if (!connected || selectedId !== id) return;
  try {
    await reloadThread();
    const reason = compactionQueueSendReason(id);
    notice(compactionLocked(id)
      ? reason || "压缩尚未结束，可继续编辑并发送普通消息到原生队列，压缩完成后执行。"
      : "已确认压缩结束，可以继续发送消息。", !!reason);
  } catch (error) { notice(`重新读取压缩状态失败：${error.message}`, true); }
  updateComposer();
}
// 用户要求压缩与发送消息、/goal 一样触发所选模型切换：菜单里选的模型和思考深度只是本地选择，
// 不随压缩请求带上就不会切到 CLI，压缩会沿用旧模型而界面仍显示新选择。
// 仅 Claude 且选择与会话当前设置不同才带上（同值切换是空操作，不应因模型目录暂不可用而阻断压缩）；
// 切换了模型时思考深度一并带上，避免服务端按“换模型即清空深度”处理而与界面显示不一致。
// 服务端在发 /compact 之前先应用，应用失败则不压缩并返回错误；对应 claude_controls.rs 的 native_command_action
// 与 composer.js 的发送载荷。
export function compactionModelChoice() {
  if (!isClaude()) return null;
  const display = currentThreadDisplay();
  const model = selectedModel && !modelIdentityMatches(selectedModel, display?.model) ? selectedModel : "";
  const reasoningEffort = selectedEffort && (model || selectedEffort !== (display?.reasoningEffort || ""))
    ? selectedEffort : "";
  return model || reasoningEffort ? { model, reasoningEffort } : null;
}
export async function compactContext() {
  if (stopping || busy || pending || threadReadOnly() || !connected || !selectedId
      || thread?.id !== selectedId || compactionLocked(selectedId)) return;
  const scope = privateInputScope(session);
  if (!scope) return;
  const id = selectedId, baseline = normalizeCompaction(thread.compaction), receiptId = randomId();
  const choice = compactionModelChoice(), choiceVersion = modelChoiceVersion;
  const choiceModel = selectedModel, choiceEffort = selectedEffort;
  try {
    setCompactionState(id, {
      state: "pending", receiptId, deviceId: session.device.id, acknowledged: false,
      turnId: null, itemId: null, baselineTurnId: baseline.turnId, baselineItemId: baseline.itemId
    }, { required: true });
  } catch (error) {
    notice(error.message, true);
    return;
  }
  updateComposer();
  try {
    const receipt = await api("/api/action/compact", {
      threadId: id,
      ...(choice?.model ? { model: choice.model } : {}),
      ...(choice?.reasoningEffort ? { reasoningEffort: choice.reasoningEffort } : {})
    }, receiptId);
    // 更换授权后的迟到结果只属于原请求，不能清除新授权的模型选择或重写当前会话状态。
    if (privateInputScope(session) !== scope) return;
    // 仅确认受理或入队后清除覆盖；明确失败保留选择，菜单后来的新选择有独立版本，不能被旧回执清掉。
    if (choice && receipt.state === "completed" && receipt.result?.ok !== false
        && selectedId === id && choiceVersion === modelChoiceVersion) {
      if (selectedModel === choiceModel) setSelectedModel("");
      if (selectedEffort === choiceEffort) setSelectedEffort("");
    }
    await applyCompactionReceipt(id, receiptId, receipt);
  } catch (error) {
    if (privateInputScope(session) !== scope) return;
    const current = compactions.get(id);
    if (current?.receiptId !== receiptId) return;
    // 502/504 等代理错误不能证明请求未到达；明确未知优先保留回执，只有明确未提交或普通 4xx 才解除锁定。
    if (operationErrorState(error) === "failed") {
      setCompactionState(id, { state: "idle", turnId: current.turnId, itemId: current.itemId });
      notice(error.message, true);
    } else {
      if (current.state === "running" && current.liveItemId && current.liveItemId === current.itemId) return;
      setCompactionState(id, { ...current, state: "unknown" });
    }
  } finally { updateComposer(); }
}
function compactionEventMatches(current, item, turnId, completed = false) {
  if (!current || current.state === "idle") return true;
  // 未取得本次原生身份时，任意旧完成事件都不能确认本次请求；等待开始事件、同次回执或重新读取。
  if (completed && current.receiptId && !current.turnId
      && (!current.itemId || current.itemId === "pending-manual-context-compaction")) return false;
  // 新请求尚未取得原生身份时，上一条压缩的迟到完成事件不能代替本次回执或提前解除防重锁。
  if (current.receiptId && !current.itemId && !current.turnId && current.baselineItemId
      && item?.id === current.baselineItemId && turnId === current.baselineTurnId) return false;
  if (current.itemId && current.itemId !== "pending-manual-context-compaction"
      && item?.id && current.itemId !== item.id) return false;
  if (current.turnId && turnId && current.turnId !== turnId) return false;
  return !(current.receiptId && !current.turnId && (!current.itemId || current.itemId === "pending-manual-context-compaction")
    && item?.source === "automatic");
}
export function receiveCompactionLifecycle(event, params) {
  const id = params.threadId;
  if (typeof id !== "string" || !id) return;
  const turnId = params.turnId || params.turn?.id || null;
  const items = params.item?.type === "contextCompaction" ? [params.item]
    : (params.turn?.items || []).filter(item => item?.type === "contextCompaction");
  let current = compactions.get(id), changed = false, completed = false;
  for (const item of items) {
    const itemCompleted = event.method === "item/completed" || item.completed === true
      || ["completed", "failed", "interrupted", "cancelled", "canceled"].includes(item.status);
    if (!itemCompleted && current?.state === "idle" && current.itemId === item.id
        && (!current.turnId || current.turnId === turnId)) continue;
    if (!compactionEventMatches(current, item, turnId, itemCompleted)) continue;
    if (itemCompleted) {
      setCompactionState(id, { state: "idle", turnId, itemId: item.id });
      current = compactions.get(id); changed = true; completed = true;
    } else if (event.method === "item/started" || item.completed === false) {
      current = setCompactionState(id, { ...current, state: "running", nativeConfirmed: true,
        turnId, itemId: item.id, liveItemId: item.id, independentLifecycle: item.remoteCompactionLifecycle === true });
      changed = true;
    }
  }
  // Claude 手动压缩可挂在已经回复完的轮次后；结束正文轮次本身不是独立压缩操作的终态。
  if (!completed && event.method === "turn/completed" && current?.state === "running" && !current.independentLifecycle
      && current.turnId && current.turnId === turnId) {
    setCompactionState(id, { state: "idle", turnId, itemId: current.itemId });
    changed = true; completed = true;
  }
  if (changed && selectedId === id) {
    updateComposer();
    if (completed) {
      const failed = items.some(item => item.status === "failed") || params.turn?.status === "failed";
      // 实时事件与回执保持一致：成功结束静默更新，失败仍显示错误，不能因去掉完成提示而隐藏异常。
      if (failed) notice("上下文压缩失败，请查看原生会话。", true);
    }
  }
}
