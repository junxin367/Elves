// 定时发送：时间选择与当前会话的独立定时消息列表。

import { $, button, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { notice } from "../../ui/notice.js";
import { closeComposerMenus, updateComposer } from "../../core/actions.js";
import { connected, selectedId, selectionVersion, session, thread } from "../../core/state.js";
import { randomId } from "../../core/util.js";
import { api } from "../api.js";
import { rememberDraft } from "./drafts.js";
import { renderSelectedSkills } from "./selected-skills.js";
import { scheduledTimeLabel, selectedScheduledAt, setSelectedScheduledAt } from "./scheduled-state.js";
import { selectedGoal } from "../goal/state.js";
import { threadReadOnly } from "../session/capabilities.js";

let scheduledMessages = [], scheduledRequest = null, scheduledLoadedThread = null;
let scheduledDialog = null;
// 已渲染状态按根元素记录：updateComposer 每次输入都会进入渲染，只比较数组引用和少量状态，
// 不对全部定时消息（含最长 128 KiB 的正文）做 JSON 序列化；重新读取会换新数组，必然重新渲染。
const renderedByRoot = new WeakMap(), noScheduledMessages = [];
// 输入区上下文从上到下固定为普通队列、定时消息、目标，视觉上从底部向上即目标、定时消息、队列。
const statusLabels = { running: "发送中", error: "发送失败", unknown: "结果待确认" };

// 内置网页包可能比服务端新（例如 Android 先升级）：服务端未声明能力时不显示入口、不发起读取。
export function scheduledMessagesSupported() {
  return session?.capabilities?.scheduledMessages === true;
}
function pad(value) { return String(value).padStart(2, "0"); }
function localInputValue(value) {
  const date = new Date(value);
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`
    + `T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
function inputValueMs(value) {
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) ? timestamp : 0;
}
function minScheduledAt() {
  return Math.ceil((Date.now() + 60_000) / 60_000) * 60_000;
}
function maxScheduledAt() {
  return Date.now() + 24 * 60 * 60 * 1000;
}

function ensureDialog() {
  if (scheduledDialog?.isConnected) return scheduledDialog;
  const dialog = element("dialog", "scheduled-send-dialog");
  dialog.id = "scheduled-send-dialog";
  dialog.setAttribute("aria-labelledby", "scheduled-send-title");
  const heading = element("div", "scheduled-send-heading");
  const title = element("h2", "", "定时发送");
  title.id = "scheduled-send-title";
  const close = button("", "关闭定时发送设置", () => dialog.close(), "icon-button");
  close.append(icon("close"));
  heading.append(title, close);
  const body = element("div", "scheduled-send-body");
  const label = element("label", "scheduled-send-field");
  label.append(element("span", "scheduled-send-label", "发送时间"));
  const input = element("input", "scheduled-send-input");
  input.type = "datetime-local";
  input.setAttribute("aria-label", "发送时间");
  label.append(input);
  const hint = element("p", "scheduled-send-hint", "可设置当前时间起未来 24 小时内的时间");
  const error = element("p", "scheduled-send-error");
  const actions = element("div", "scheduled-send-actions");
  const cancel = button("取消", "取消定时发送设置", () => dialog.close(), "text-button");
  const confirm = button("确定", "确认定时发送时间", () => {
    const value = inputValueMs(input.value);
    if (!value || value < inputValueMs(input.min) || value > inputValueMs(input.max)) {
      error.textContent = "请选择未来 24 小时内的时间";
      return;
    }
    setSelectedScheduledAt(value);
    dialog.close();
    // 必须先画出编辑框标记再记录草稿：草稿快照以编辑框里的实际标记为准，没画出来时刚选的时间会被清掉。
    renderSelectedSkills();
    rememberDraft();
    updateComposer();
    $("#prompt")?.focus({ preventScroll: true });
  }, "primary");
  actions.append(cancel, confirm);
  body.append(label, hint, error, actions);
  dialog.append(heading, body);
  dialog.addEventListener("close", () => { error.textContent = ""; });
  document.body.append(dialog);
  scheduledDialog = dialog;
  return dialog;
}

export function openScheduledMessagePicker() {
  if (!scheduledMessagesSupported()) return;
  if (selectedGoal) {
    notice("目标模式不支持定时发送，请先取消目标模式。", true);
    return;
  }
  if (!selectedId || !thread || threadReadOnly() || !connected) {
    notice("请先打开可编辑的已有会话。", true);
    return;
  }
  closeComposerMenus();
  const dialog = ensureDialog();
  const input = $(".scheduled-send-input", dialog);
  const min = minScheduledAt(), max = maxScheduledAt();
  input.min = localInputValue(min);
  input.max = localInputValue(max);
  input.value = localInputValue(selectedScheduledAt && selectedScheduledAt >= min ? selectedScheduledAt : min);
  dialog.showModal();
  queueMicrotask(() => input.focus({ preventScroll: true }));
}

export async function submitScheduledMessage(payload) {
  const receipt = await api("/api/action/createScheduledMessage", payload, randomId());
  if (receipt?.state !== "completed") {
    throw Object.assign(new Error(receipt?.result?.message || "定时消息设置结果待确认"), {
      outcomeUnknown: receipt?.state === "unknown",
      pendingFeedback: receipt?.state === "pending"
    });
  }
  if (receipt.result?.ok !== true) throw new Error(receipt.result?.message || "定时消息设置失败");
  return receipt.result;
}

export async function cancelScheduledMessage(message) {
  if (!message?.id || !message.threadId || !connected || threadReadOnly(message.threadId)) return;
  try {
    const receipt = await api("/api/action/cancelScheduledMessage", {
      threadId: message.threadId, messageId: message.id
    }, randomId());
    // 服务端拒绝（例如已在发送中）以失败回执返回，不能当作取消成功。
    if (receipt?.state === "failed") notice(receipt.result?.message || "定时消息取消失败", true);
    await reloadScheduledMessages(message.threadId);
  } catch (error) {
    notice(error.message, true);
  }
}

export async function reloadScheduledMessages(threadId = selectedId) {
  if (!threadId) {
    scheduledMessages = []; scheduledLoadedThread = null; renderScheduledMessages(); return;
  }
  if (!scheduledMessagesSupported()) return;
  const version = selectionVersion;
  // 进行中的读取可能早于刚收到的变更事件：记下需要再读一次。切走又切回时旧请求的结果会被丢弃，必须另起一次。
  if (scheduledRequest?.threadId === threadId && scheduledRequest.version === version) {
    scheduledRequest.again = true;
    return scheduledRequest.promise;
  }
  const request = { threadId, version, again: false, promise: null };
  scheduledRequest = request;
  request.promise = (async () => {
    try {
      do {
        request.again = false;
        const result = await api("/api/read/scheduledMessages", { threadId });
        if (version !== selectionVersion || selectedId !== threadId) return;
        scheduledMessages = Array.isArray(result?.messages) ? result.messages : [];
        scheduledLoadedThread = threadId;
        renderScheduledMessages();
      } while (request.again);
    } catch (error) {
      if (version === selectionVersion && selectedId === threadId) notice(`定时消息读取失败：${error.message}`, true);
    } finally {
      if (scheduledRequest === request) scheduledRequest = null;
    }
  })();
  return request.promise;
}

export function renderScheduledMessages() {
  const root = $("#scheduled-area");
  if (!root) return;
  const current = scheduledLoadedThread === selectedId ? scheduledMessages : noScheduledMessages;
  const readOnly = threadReadOnly(), previous = renderedByRoot.get(root);
  if (previous && previous.current === current && previous.selectedId === selectedId
      && previous.connected === connected && previous.readOnly === readOnly) return;
  renderedByRoot.set(root, { current, selectedId, connected, readOnly });
  root.hidden = !current.length;
  root.replaceChildren();
  for (const message of current) {
    const row = element("div", "scheduled-message");
    row.dataset.messageId = message.id || "";
    row.dataset.status = message.status || "scheduled";
    row.setAttribute("role", "listitem");
    const glyph = icon("recent");
    glyph.classList.add("scheduled-message-icon");
    const copy = element("div", "scheduled-message-copy");
    const heading = element("div", "scheduled-message-heading-line");
    heading.append(element("strong", "", `定时发送 · ${scheduledTimeLabel(message.sendAtMs)}`));
    if (statusLabels[message.status])
      heading.append(element("span", "scheduled-message-status", statusLabels[message.status]));
    copy.append(heading, element("p", "scheduled-message-text", message.prompt || "定时消息"));
    if (message.lastError) copy.append(element("small", "scheduled-message-reason", message.lastError));
    const remove = button("", "取消定时发送", () => void cancelScheduledMessage(message), "icon-button scheduled-message-remove");
    remove.append(icon("trash"));
    remove.disabled = !connected || readOnly || message.status === "running";
    row.append(glyph, copy, remove);
    root.append(row);
  }
}
