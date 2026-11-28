// 定时发送：时间选择与当前会话的独立定时消息列表。

import { $, button, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { confirmAction } from "../../ui/dialog.js";
import { activePopup, closePopup, openPopup, togglePopup } from "../../ui/popup.js";
import { notice } from "../../ui/notice.js";
import { closeComposerMenus, updateComposer } from "../../core/actions.js";
import { connected, nativeConnectionVersion, pageLeaving, selectedId, selectionVersion, session, thread } from "../../core/state.js";
import { randomId } from "../../core/util.js";
import { api, operationErrorState, receiptResultState } from "../api.js";
import { rememberDraft } from "./drafts.js";
import { promptSelection, setPromptSelection } from "./prompt-editor.js";
import { renderSelectedSkills } from "./selected-skills.js";
import { scheduledTimeLabel, selectedScheduledAt, setSelectedScheduledAt } from "./scheduled-state.js";
import { selectedGoal } from "../goal/state.js";
import { privateInputScope, threadReadOnly } from "../session/capabilities.js";

let scheduledMessages = [], scheduledRequest = null, scheduledLoadedThread = null, scheduledLoadedScope = null;
let scheduledReadScope = null;
let scheduledDialog = null, scheduledEditor = null, dialogScope = null, dialogTrigger = null;
let dialogPromptSelection = null, dialogRestorePrompt = false;
// 已渲染状态按根元素记录：updateComposer 每次输入都会进入渲染，只比较数组引用和少量状态，
// 不对全部定时消息（含最长 128 KiB 的正文）做 JSON 序列化；重新读取会换新数组，必然重新渲染。
const renderedByRoot = new WeakMap(), noScheduledMessages = [];
// 输入区上下文从上到下固定为普通队列、定时消息、目标，视觉上从底部向上即目标、定时消息、队列。
const statusLabels = { running: "发送中", error: "发送失败", unknown: "结果待确认" };
function scheduledScope() {
  return JSON.stringify([privateInputScope(session), nativeConnectionVersion, selectedId, selectionVersion]);
}
function editingSupported() { return session?.capabilities?.scheduledMessageEditing === true; }
function editBlock(message) {
  if (!editingSupported()) return "当前服务版本不支持编辑定时消息";
  if (!connected || threadReadOnly()) return "当前会话不可编辑";
  if (!message || message.threadId !== selectedId) return "定时消息已被删除或已发送";
  if (message.canEdit !== true) return message.editDisabledReason || "定时消息已进入发送流程，无法编辑";
  return "";
}

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
  const close = button("", "关闭定时发送设置", () => void closeScheduledDialog(), "icon-button");
  close.append(icon("close"));
  heading.append(title, close);
  const body = element("div", "scheduled-send-body");
  const content = element("label", "scheduled-send-field scheduled-send-content");
  content.append(element("span", "scheduled-send-label", "消息内容"));
  const prompt = element("textarea", "scheduled-send-prompt");
  prompt.setAttribute("aria-label", "消息内容");
  content.append(prompt);
  const attachments = element("p", "scheduled-send-attachments");
  const label = element("label", "scheduled-send-field");
  label.append(element("span", "scheduled-send-label", "发送时间"));
  const input = element("input", "scheduled-send-input");
  input.type = "datetime-local";
  input.setAttribute("aria-label", "发送时间");
  label.append(input);
  const hint = element("p", "scheduled-send-hint", "可设置当前时间起未来 24 小时内的时间");
  const error = element("p", "scheduled-send-error");
  error.setAttribute("role", "status");
  error.setAttribute("aria-live", "polite");
  const actions = element("div", "scheduled-send-actions");
  const cancel = button("取消", "取消定时发送设置", () => void closeScheduledDialog(), "text-button");
  const confirm = button("确定", "确认定时发送时间", () => {
    if (scheduledEditor) return saveScheduledEdit();
    if (dialogScope !== scheduledScope() || !connected || threadReadOnly()) return dialog.close();
    const value = inputValueMs(input.value);
    if (!value || value <= Date.now() || value > maxScheduledAt()) {
      error.textContent = "请选择未来 24 小时内的时间";
      return;
    }
    setSelectedScheduledAt(value);
    // 必须先画出编辑框标记再记录草稿：草稿快照以编辑框里的实际标记为准，没画出来时刚选的时间会被清掉。
    renderSelectedSkills();
    rememberDraft();
    updateComposer();
    dialogRestorePrompt = true;
    dialog.close();
  }, "primary");
  actions.append(cancel, confirm);
  body.append(content, attachments, label, hint, error);
  dialog.append(heading, body, actions);
  dialog.scheduleFields = { title, content, prompt, attachments, input, error, close, cancel, confirm };
  dialog.oncancel = event => { event.preventDefault(); void closeScheduledDialog(); };
  window.addEventListener("codex-elves-native-back", event => {
    if (event.defaultPrevented || !dialog.open || [...document.querySelectorAll("dialog[open]")].at(-1) !== dialog) return;
    event.preventDefault(); void closeScheduledDialog();
  });
  dialog.addEventListener("close", () => {
    const sameScope = dialogScope === scheduledScope();
    const restore = dialogRestorePrompt ? dialogPromptSelection : null;
    scheduledEditor = null; dialogScope = null; error.textContent = ""; prompt.value = "";
    dialogPromptSelection = null; dialogRestorePrompt = false;
    document.body.classList?.toggle("modal-open", !!document.querySelector?.("dialog[open]"));
    if (!sameScope) return;
    if (restore?.prompt === $("#prompt") && restore?.prompt?.isConnected && !restore.prompt.disabled) {
      // 用户要求空输入时光标在定时标签后，有内容时标签置顶且保留原编辑位置；复用目标的正文/Skill 边界换算。
      // 等原生 dialog 关闭并完成默认焦点恢复后再还原选区，避免 focus 把光标重新放到标签前。
      restore.prompt.focus({ preventScroll: true });
      const selection = restore.selection;
      setPromptSelection(restore.prompt, selection.start, selection.end, selection);
    } else (dialogTrigger?.isConnected ? dialogTrigger : $("#prompt"))?.focus({ preventScroll: true });
  });
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
  // 时间输入框会接管焦点，必须在打开弹窗前保存正文位置，而不是确认时间后重新读取。
  const prompt = $("#prompt"), selection = prompt ? promptSelection(prompt) : null;
  closeComposerMenus();
  closePopup();
  const dialog = ensureDialog();
  scheduledEditor = null; dialogScope = scheduledScope(); dialogTrigger = document.activeElement;
  dialogPromptSelection = prompt ? { prompt, selection } : null; dialogRestorePrompt = false;
  const { input, content, attachments, title, confirm, close, cancel, error } = dialog.scheduleFields;
  content.hidden = attachments.hidden = true;
  title.textContent = "定时发送"; confirm.textContent = "确定";
  confirm.setAttribute("aria-label", "确认定时发送时间");
  input.disabled = confirm.disabled = close.disabled = cancel.disabled = false; error.textContent = "";
  const min = minScheduledAt(), max = maxScheduledAt();
  input.min = localInputValue(min);
  input.max = localInputValue(max);
  input.value = localInputValue(selectedScheduledAt && selectedScheduledAt >= min ? selectedScheduledAt : min);
  document.body.classList?.add("modal-open");
  dialog.showModal();
  queueMicrotask(() => input.focus({ preventScroll: true }));
}

export function openScheduledMessageEditor(message, trigger) {
  const blocked = editBlock(message);
  if (blocked) { notice(blocked, true); return; }
  closePopup(); closeComposerMenus();
  const dialog = ensureDialog(), fields = dialog.scheduleFields;
  dialogScope = scheduledScope(); dialogTrigger = trigger || document.activeElement;
  dialogPromptSelection = null; dialogRestorePrompt = false;
  scheduledEditor = { message, scope: dialogScope, saving: false, uncertain: false, error: "" };
  fields.title.textContent = "编辑定时发送";
  fields.content.hidden = false;
  fields.prompt.value = message.prompt || "";
  fields.attachments.hidden = !(message.attachmentCount > 0);
  fields.attachments.textContent = message.attachmentCount > 0 ? `已附加 ${message.attachmentCount} 个附件` : "";
  fields.input.min = localInputValue(minScheduledAt());
  fields.input.max = localInputValue(maxScheduledAt());
  // 已创建任务的原时间先完整回显；过去时间由保存校验处理，不能打开编辑就悄悄改期。
  fields.input.value = localInputValue(message.sendAtMs);
  scheduledEditor.initialTime = fields.input.value;
  fields.confirm.textContent = "保存";
  fields.confirm.setAttribute("aria-label", "保存定时发送");
  fields.prompt.oninput = fields.input.oninput = () => {
    if (scheduledEditor) { scheduledEditor.error = ""; syncScheduledEditor(); }
  };
  syncScheduledEditor();
  document.body.classList?.add("modal-open");
  dialog.showModal();
  queueMicrotask(() => fields.prompt.focus({ preventScroll: true }));
}

function syncScheduledEditor() {
  if (!scheduledEditor || !scheduledDialog) return;
  if (scheduledEditor.scope !== scheduledScope()) { scheduledDialog.close(); return; }
  const editor = scheduledEditor, fields = scheduledDialog.scheduleFields;
  const current = scheduledMessages.find(message => message.id === editor.message.id);
  const blocked = editBlock(current) || (current.updatedAtMs !== editor.message.updatedAtMs
    ? "定时消息已变化，请关闭后重新打开编辑" : "");
  const locked = editor.saving || editor.uncertain || !!blocked;
  fields.prompt.readOnly = locked; fields.input.disabled = locked;
  fields.confirm.disabled = locked;
  fields.close.disabled = fields.cancel.disabled = editor.saving;
  fields.error.textContent = editor.saving ? "正在保存…" : editor.error || blocked;
}

async function closeScheduledDialog() {
  const dialog = scheduledDialog, editor = scheduledEditor;
  if (!dialog?.open || editor?.saving) return;
  const fields = dialog.scheduleFields;
  if (editor && (fields.prompt.value !== (editor.message.prompt || "")
      || fields.input.value !== editor.initialTime)) {
    if (!await confirmAction(editor.uncertain ? "关闭编辑？" : "放弃修改？",
      editor.uncertain ? "保存结果待确认，关闭不会撤销已提交的修改，请重新打开任务核对。"
        : "尚未保存的消息内容和发送时间将丢失。", editor.uncertain ? "关闭编辑" : "放弃修改")) return;
    if (scheduledEditor !== editor) return;
  }
  dialog.close();
}

async function saveScheduledEdit() {
  syncScheduledEditor();
  const editor = scheduledEditor, dialog = scheduledDialog, fields = dialog?.scheduleFields;
  if (!editor || fields.confirm.disabled || !dialog.open) return;
  const sendAtMs = fields.input.value === editor.initialTime
    ? editor.message.sendAtMs : inputValueMs(fields.input.value), prompt = fields.prompt.value;
  if (!sendAtMs || sendAtMs <= Date.now() || sendAtMs > maxScheduledAt()) {
    editor.error = "请选择未来 24 小时内的时间"; syncScheduledEditor(); return;
  }
  if ((!prompt.trim() && !editor.message.attachmentCount) || new TextEncoder().encode(prompt).length > 128 * 1024) {
    editor.error = "消息不能为空或超过 128 KiB"; syncScheduledEditor(); return;
  }
  // 用户要求编辑原任务：仅提交内容和时间，不取回/删除后重建，不改动当前输入框草稿。
  editor.saving = true; syncScheduledEditor();
  try {
    const receipt = await api("/api/action/updateScheduledMessage", {
      threadId: editor.message.threadId, messageId: editor.message.id,
      expectedUpdatedAtMs: editor.message.updatedAtMs, prompt, sendAtMs
    }, randomId());
    if (scheduledEditor !== editor || editor.scope !== scheduledScope()) return;
    if (receipt?.state !== "completed" || receipt.result?.ok !== true) {
      editor.uncertain = receiptResultState(receipt) !== "failed";
      editor.error = receipt?.result?.message || (editor.uncertain
        ? "保存结果待确认，请关闭后重新打开核对" : "定时消息保存失败");
      return;
    }
    dialog.close();
    notice("定时发送已更新");
    await reloadScheduledMessages(editor.message.threadId);
  } catch (error) {
    if (scheduledEditor !== editor || editor.scope !== scheduledScope()) return;
    editor.uncertain = operationErrorState(error) !== "failed";
    editor.error = editor.uncertain ? "保存结果待确认，请关闭后重新打开核对" : error.message;
  } finally {
    editor.saving = false;
    if (scheduledEditor === editor) syncScheduledEditor();
  }
}

export async function submitScheduledMessage(payload) {
  const receipt = await api("/api/action/createScheduledMessage", payload, randomId());
  if (receipt?.state !== "completed") {
    const state = receiptResultState(receipt);
    throw Object.assign(new Error(receipt?.result?.message || (state === "failed" ? "定时消息设置失败" : "定时消息设置结果待确认")), {
      operationState: state, outcomeUnknown: state === "unknown",
      pendingFeedback: state === "pending"
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

export function retireScheduledMessageRead() {
  // 退后台或断线只取消列表读取，保留已显示的列表、编辑内容及所有写回执。
  const previous = scheduledRequest;
  scheduledRequest = null;
  scheduledReadScope = null;
  previous?.controller.abort();
}

export async function reloadScheduledMessages(threadId = selectedId, { onlyIfNeeded = false } = {}) {
  if (!threadId) {
    retireScheduledMessageRead();
    scheduledMessages = []; scheduledLoadedThread = null; renderScheduledMessages(); return;
  }
  if (document.hidden || pageLeaving || !connected || threadId !== selectedId || !scheduledMessagesSupported()) return;
  const scope = scheduledScope();
  // 同一轮前台恢复、选中会话和连接探测只补读一次；真实变更或写回执仍需要比在途快照更新的列表。
  if (scheduledRequest?.scope === scope) {
    if (!onlyIfNeeded) scheduledRequest.again = true;
    return scheduledRequest.promise;
  }
  if (onlyIfNeeded && scheduledReadScope === scope) return;
  retireScheduledMessageRead();
  const request = { scope, controller: new AbortController(), again: false, promise: null };
  scheduledReadScope = scope;
  scheduledRequest = request;
  const current = () => scheduledRequest === request && !document.hidden && !pageLeaving && scope === scheduledScope();
  request.promise = (async () => {
    try {
      do {
        request.again = false;
        try {
          const result = await api("/api/read/scheduledMessages", { threadId }, undefined, request.controller.signal);
          if (!current()) return;
          if (request.again) continue;
          scheduledMessages = Array.isArray(result?.messages) ? result.messages : [];
          scheduledLoadedThread = threadId;
          scheduledLoadedScope = scope;
          renderScheduledMessages();
        } catch (error) {
          if (!current()) return;
          // 在途期间已收到真实变更时，旧快照及其错误都不展示，改以补读结果为准。
          if (!request.again) notice(`定时消息读取失败：${error.message}`, true);
        }
      } while (request.again);
    } finally {
      if (scheduledRequest === request) scheduledRequest = null;
    }
  })();
  return request.promise;
}

export function renderScheduledMessages() {
  const root = $("#scheduled-area");
  if (!root) return;
  if (scheduledDialog?.open && dialogScope !== scheduledScope()) scheduledDialog.close();
  syncScheduledEditor();
  const current = scheduledLoadedThread === selectedId && scheduledLoadedScope === scheduledScope()
    ? scheduledMessages : noScheduledMessages;
  const readOnly = threadReadOnly(), previous = renderedByRoot.get(root);
  if (previous && previous.current === current && previous.selectedId === selectedId
      && previous.connected === connected && previous.readOnly === readOnly) return;
  renderedByRoot.set(root, { current, selectedId, connected, readOnly });
  root.hidden = !current.length;
  if (activePopup?.trigger?.dataset.scheduledMessageMenu) closePopup();
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
    // 用户要求与排队消息一致：更多图标展开“编辑 / 删除”，不再把删除作为单击入口。
    const menu = button("", "定时消息操作", () => togglePopup(menu), "queued-menu-button scheduled-message-menu");
    menu.append(icon("more"));
    menu.dataset.scheduledMessageMenu = message.id;
    menu.setAttribute("aria-haspopup", "menu"); menu.setAttribute("aria-expanded", "false");
    menu.popupClassName = "queued-message-popup";
    menu.disabled = !connected || readOnly;
    const scope = scheduledScope(), blocked = editBlock(message);
    menu.menuOptions = [
      { label: "编辑", icon: "edit", disabled: !!blocked, description: blocked,
        action: () => { if (scope === scheduledScope()) openScheduledMessageEditor(message, menu); } },
      { label: "删除", value: "remove", icon: "trash",
        disabled: message.canCancel === false || message.status === "running",
        action: () => { if (scope === scheduledScope()) void cancelScheduledMessage(message); } }
    ];
    menu.onkeydown = event => {
      if (["ArrowDown", "ArrowUp"].includes(event.key)) {
        event.preventDefault(); openPopup(menu, { edge: event.key === "ArrowUp" ? "last" : "first" });
      }
    };
    row.append(glyph, copy, menu);
    root.append(row);
  }
}
