import { activePopup, closePopup, togglePopup } from "./ui/popup.js";

export function nativeMessageSentAtMs(item) {
  const value = item?.sentAtMs ?? item?.messageMetadata?.sentAtMs;
  return Number.isFinite(value) && value > 0 ? value : null;
}

export function messageActionState({
  item,
  isLastUserMessage,
  readOnly = false,
  turnInProgress = false,
  editSupported = true,
  completed = false,
  forkSupported = false,
  forkDisabled = false,
  turnId = null
}) {
  const user = item?.type === "userMessage";
  const assistant = item?.type === "agentMessage";
  const metadata = item?.messageMetadata;
  const editable = isLastUserMessage ?? metadata?.editable === true;
  return {
    canCopy: user || assistant && (completed || item?.completed === true || item?.status === "completed"),
    canEdit: user && editable && !readOnly && !turnInProgress && editSupported,
    canFork: assistant && forkSupported && !!turnId,
    forkDisabled,
    forkTurnId: turnId,
    sentAtMs: nativeMessageSentAtMs(item),
    messageId: typeof metadata?.messageId === "string" ? metadata.messageId : null,
    turnId: typeof metadata?.turnId === "string" ? metadata.turnId : null
  };
}

export function renderMessageActions(root, item, options, callbacks) {
  const state = messageActionState({ item, ...options });
  const user = item.type === "userMessage";
  const trigger = user ? root.querySelector(".user-bubble") : null;
  const signature = JSON.stringify([state, options.threadId, options.inActivity]);
  const previous = root.messageActionBinding;
  if (previous?.signature === signature && previous.trigger === trigger && previous.space.isConnected) {
    previous.item = item;
    return;
  }
  previous?.dispose();
  if (options.inActivity && !user || !state.canCopy && state.sentAtMs == null) return;

  const document = root.ownerDocument;
  const element = (tag, className, text) => {
    const node = document.createElement(tag);
    node.className = className;
    if (text != null) node.textContent = text;
    return node;
  };
  const space = element("div", "message-action-space");
  const panel = element("div", `message-actions${user ? " user-message-action" : ""}`);
  panel.setAttribute("aria-label", "消息操作");
  const binding = {
    item, signature, trigger, space, panel,
    dispose() {
      if (activePopup?.trigger === binding.more) closePopup();
      space.remove();
      if (root.messageActionBinding === binding) delete root.messageActionBinding;
    }
  };
  root.messageActionBinding = binding;
  const action = (name, label, handler) => {
    const button = element("button", "message-action-button");
    button.type = "button"; button.title = label; button.setAttribute("aria-label", label);
    button.append(callbacks.icon(name));
    button.onclick = () => handler(binding.item);
    return button;
  };
  const date = state.sentAtMs == null ? null : new Date(state.sentAtMs);
  const now = date == null ? null : new Date();
  const sameDay = date != null
    && date.getFullYear() === now.getFullYear()
    && date.getMonth() === now.getMonth()
    && date.getDate() === now.getDate();
  const clock = date == null ? null
    : new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false }).format(date);
  const time = date == null ? null : element("time", "message-time", sameDay ? clock
    : `${new Intl.DateTimeFormat("zh-CN", { weekday: "long" }).format(date)}${clock}`);
  if (time) {
    time.dateTime = date.toISOString();
    time.title = date.toLocaleString("zh-CN");
  }
  if (user && time) panel.append(time);
  if (state.canCopy) panel.append(action("copy", user ? "复制用户消息" : "复制回复", callbacks.copy));
  if (state.canEdit) panel.append(action("edit", "编辑并再次发送", callbacks.edit));
  if (state.canFork) {
    // 用户要求回复下方 ⋯ 菜单与 /fork 同名为“分支到新聊天”；不支持的侧边/子智能体不创建按钮。
    const more = action("more", "更多操作", () => togglePopup(more));
    more.classList.add("message-more-button");
    more.setAttribute("aria-haspopup", "menu"); more.setAttribute("aria-expanded", "false");
    more.menuOptions = [{ value: "fork", label: "分支到新聊天", icon: "fork",
      disabled: state.forkDisabled, action: () => callbacks.fork(state.forkTurnId) }];
    binding.more = more;
    panel.append(more);
  }
  if (!user && time) panel.append(time);
  space.append(panel); root.append(space);
}
