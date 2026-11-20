// 通用确认与设置对话框，关闭后恢复触发前焦点。

import { $, button, element } from "./dom.js";
import { icon } from "./icons.js";
import { closeAppMenus, closePopup } from "./popup.js";

export function confirmAction(title, message, action, danger = false, className = "") {
  closePopup(); closeAppMenus();
  if ($("#confirmation-dialog")) return Promise.resolve(false);
  const trigger = document.activeElement;
  const dialog = element("dialog", `confirmation-dialog${className ? ` ${className}` : ""}`);
  dialog.id = "confirmation-dialog";
  dialog.setAttribute("aria-labelledby", "confirmation-title"); dialog.setAttribute("aria-describedby", "confirmation-message");
  const heading = element("h2", "", title); heading.id = "confirmation-title";
  const description = element("p", "muted", message); description.id = "confirmation-message";
  const actions = element("div", "confirmation-actions");
  actions.append(button("取消", "取消操作", () => dialog.close("cancel")),
    button(action, action, () => dialog.close("confirm"), danger ? "danger-button" : "primary"));
  const content = element("div", "confirmation-content"), text = element("div");
  text.append(heading, description); content.append(icon("warning"), text);
  dialog.append(content, actions); document.body.append(dialog);
  return new Promise(resolve => {
    dialog.onclose = () => {
      const confirmed = dialog.returnValue === "confirm";
      dialog.remove(); document.body.classList.toggle("modal-open", !!document.querySelector("dialog[open]"));
      if (trigger?.isConnected && document.activeElement === document.body) trigger.focus();
      resolve(confirmed);
    };
    document.body.classList.add("modal-open");
    dialog.showModal(); $("button", actions).focus();
  });
}

export function openSettingsPrompt({
  title, value, defaultValue, max, getBlock, onSave, onClose, returnFocus
}) {
  if ($("#settings-prompt-dialog")) return null;
  closePopup(); closeAppMenus();
  const trigger = document.activeElement;
  const dialog = element("dialog", "confirmation-dialog settings-prompt-dialog");
  dialog.id = "settings-prompt-dialog"; dialog.setAttribute("aria-labelledby", "settings-prompt-title");
  const heading = element("div", "settings-prompt-heading");
  const name = element("h2", "", title); name.id = "settings-prompt-title";
  const closeButton = button("", "关闭", cancel, "icon-button");
  closeButton.append(icon("close")); closeButton.autofocus = true;
  heading.append(name, closeButton);

  const body = element("div", "settings-prompt-body"), input = element("textarea", "instruction-content");
  const status = element("p", "settings-prompt-status");
  status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const preview = element("details", "settings-prompt-default");
  const defaultText = defaultValue ?? (defaultValue === null ? "使用原生默认（无附加提示词）" : "暂未取得默认提示词");
  preview.append(element("summary", "", "默认提示词"), element("pre", "", defaultText));
  input.setAttribute("aria-label", title); input.spellcheck = false;
  input.maxLength = max * 2;
  input.placeholder = defaultValue == null ? defaultText : "";
  input.value = defaultValue ?? "";
  const fallback = input.value;
  const initiallyDefault = value == null;
  if (!initiallyDefault) input.value = value;
  const initial = input.value;
  let useDefault = initiallyDefault, saving = false, errorMessage = "";
  body.append(input, preview, status);

  // 用户要求恢复默认只修改弹窗草稿；取消不写入，保存才提交当前文本或移除实例覆盖。
  // 操作区沿用设置页层级：恢复默认、取消为文字按钮，保存为唯一主按钮。
  const actions = element("div", "confirmation-actions settings-prompt-actions");
  const restore = button("恢复默认", `恢复默认${title}`, () => {
    if (restore.disabled) return;
    input.value = fallback; input.scrollTop = 0; useDefault = true; errorMessage = ""; sync();
  }, "text-button");
  const cancelButton = button("取消", "取消编辑", cancel, "text-button");
  const save = button("保存", `保存${title}`, async () => {
    sync();
    if (!dialog.open || save.disabled) return;
    saving = true; errorMessage = ""; sync();
    try {
      await onSave(useDefault ? null : input.value);
      if (dialog.open) dialog.close();
    } catch (error) {
      errorMessage = error.message || "保存未能确认，请重新打开设置核对。";
    } finally {
      saving = false;
      if (dialog.open) sync();
    }
  }, "primary");
  actions.append(restore, cancelButton, save);
  dialog.append(heading, body, actions); document.body.append(dialog);

  function sync() {
    const blocked = getBlock(), dirty = input.value !== initial || useDefault !== initiallyDefault;
    const invalid = !useDefault && (!input.value.trim() || Array.from(input.value).length > max
      || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/u.test(input.value));
    input.readOnly = saving || !!blocked;
    input.setAttribute("aria-invalid", String(dirty && invalid));
    preview.hidden = useDefault || input.value === fallback || defaultValue === undefined;
    restore.disabled = input.readOnly || useDefault || defaultValue === undefined;
    cancelButton.disabled = closeButton.disabled = saving;
    save.disabled = input.readOnly || !dirty || invalid;
    status.textContent = saving ? "正在保存…" : errorMessage || blocked
      || (dirty && invalid ? `请输入 1～${max} 个字符，不包含控制字符。` : "");
    status.hidden = !status.textContent;
  }
  function cancel() { if (dialog.open && !saving) dialog.close(); }
  input.oninput = () => { useDefault = initiallyDefault && input.value === initial; errorMessage = ""; sync(); };
  dialog.oncancel = event => { event.preventDefault(); cancel(); };
  const onNativeBack = event => {
    if (event.defaultPrevented || [...document.querySelectorAll("dialog[open]")].at(-1) !== dialog) return;
    event.preventDefault(); cancel();
  };
  const viewport = window.visualViewport;
  const fit = () => {
    dialog.style.setProperty("--settings-prompt-viewport-height", `${viewport.height}px`);
    dialog.style.setProperty("--settings-prompt-viewport-top", `${viewport.offsetTop}px`);
  };
  dialog.onclose = () => {
    window.removeEventListener("codex-elves-native-back", onNativeBack);
    viewport?.removeEventListener("resize", fit);
    viewport?.removeEventListener("scroll", fit);
    dialog.remove(); document.body.classList.toggle("modal-open", !!document.querySelector("dialog[open]"));
    onClose?.();
    if (document.activeElement === document.body) {
      const target = returnFocus ? returnFocus() : trigger;
      if (target?.isConnected && !target.disabled) target.focus({ preventScroll: true });
    }
  };
  window.addEventListener("codex-elves-native-back", onNativeBack);
  if (viewport) { viewport.addEventListener("resize", fit); viewport.addEventListener("scroll", fit); fit(); }
  document.body.classList.add("modal-open");
  sync(); dialog.showModal();
  return { sync, close() { if (dialog.open) dialog.close(); } };
}
