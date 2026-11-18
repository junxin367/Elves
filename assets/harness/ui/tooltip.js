// 主题化悬停提示：替代浏览器原生 title，使输入区、关联菜单及正文压缩提示跟随网页配色。

import { element } from "./dom.js";

export function installComposerTooltips() {
  // 用户要求 hover 信息跟随 Codex 配色；浏览器原生 title 无法应用网页主题，编辑区改用可访问的普通提示层。
  // 压缩信息沿用同一提示层并支持点击固定；其他提示和已有独立上下文提示的触屏交互不变。
  const scope = ".composer-area,.composer-popover,.composer-add-popup,.queued-message-popup,.context-compaction";
  let pending = null, timer = null, node = null, observer = null;
  function position() {
    if (!pending || !node) return;
    const view = window.visualViewport, left = view?.offsetLeft || 0, top = view?.offsetTop || 0;
    const width = view?.width || window.innerWidth, height = view?.height || window.innerHeight;
    const anchor = pending.trigger.getBoundingClientRect(), box = node.getBoundingClientRect();
    node.style.left = `${Math.max(left + 8, Math.min(anchor.left + (anchor.width - box.width) / 2, left + width - box.width - 8))}px`;
    const above = anchor.top - box.height - 6;
    node.style.top = `${Math.max(top + 8, Math.min(above >= top + 8 ? above : anchor.bottom + 6, top + height - box.height - 8))}px`;
  }
  function update(trigger, title) {
    if (pending?.trigger !== trigger) return;
    // Fast 等异步状态变化时更新同一提示，含 200ms 等待期间；移开鼠标后也恢复最新文案。
    pending.title = String(title || "").trim();
    trigger.removeAttribute("title");
    if (!pending.title) { close(); return; }
    if (node && node.textContent !== pending.title) {
      node.textContent = pending.title;
      position();
    }
  }
  function close() {
    clearTimeout(timer); timer = null;
    observer?.disconnect(); observer = null;
    if (pending) {
      const { trigger, title, pinned } = pending;
      if (!trigger.hasAttribute("title")) trigger.setAttribute("title", title);
      if (pinned) trigger.setAttribute("aria-expanded", "false");
      const descriptions = (trigger.getAttribute("aria-describedby") || "").split(/\s+/)
        .filter(id => id && id !== "composer-hover-tooltip");
      if (descriptions.length) trigger.setAttribute("aria-describedby", descriptions.join(" "));
      else trigger.removeAttribute("aria-describedby");
    }
    node?.remove(); node = null; pending = null;
  }
  function target(event) {
    const trigger = event.target?.closest?.("[title]") || pending?.trigger;
    return trigger && trigger.contains(event.target) && trigger.closest(scope)
      && trigger.id !== "context-info" ? trigger : null;
  }
  function prepare(trigger) {
    if (pending?.trigger === trigger) return true;
    close();
    const title = trigger.getAttribute("title")?.trim();
    if (!title) return false;
    pending = { trigger, title, pinned: false };
    trigger.removeAttribute("title");
    return true;
  }
  function show() {
    clearTimeout(timer); timer = null;
    if (!pending || node) return;
    const { trigger, title } = pending;
    if (!trigger.isConnected || trigger.closest("[hidden],[inert]")) { close(); return; }
    // Token 统计仍为单行；用户要求压缩提示按模型、前后 Token、缩减及耗时分行，不能再显示成长条原生 title。
    const className = trigger.id === "session-metrics" ? "composer-tooltip composer-metrics-tooltip"
      : trigger.closest(".context-compaction") ? "composer-tooltip compaction-tooltip" : "composer-tooltip";
    node = element("div", className, title);
    node.id = "composer-hover-tooltip"; node.setAttribute("role", "tooltip");
    const descriptions = (trigger.getAttribute("aria-describedby") || "").split(/\s+/).filter(Boolean);
    trigger.setAttribute("aria-describedby", [...new Set([...descriptions, node.id])].join(" "));
    if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
    document.body.append(node);
    if (node.hasAttribute("popover")) node.showPopover();
    position();
    observer = new MutationObserver(() => {
      if (!trigger.isConnected || trigger.closest("[hidden],[inert]")) close();
    });
    observer.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden", "inert"] });
  }
  function schedule(trigger) {
    if (pending?.trigger === trigger || pending?.pinned || !prepare(trigger)) return;
    // 原生 Codex 普通提示默认延迟 200ms，避免移动鼠标经过工具栏时连续闪烁。
    timer = setTimeout(show, 200);
  }
  function toggleCompaction(trigger) {
    if (pending?.trigger === trigger && pending.pinned) { close(); return; }
    if (!prepare(trigger)) return;
    // 点击或轻触立即显示同一提示；已经悬停时固定原浮层，不另建一份。
    pending.pinned = true;
    show();
    if (node) trigger.setAttribute("aria-expanded", "true");
  }
  document.addEventListener("pointerover", event => {
    if (event.pointerType !== "mouse") return;
    const trigger = target(event);
    if (trigger) schedule(trigger);
  });
  document.addEventListener("pointerout", event => {
    if (pending && !pending.pinned && pending.trigger.contains(event.target) && !pending.trigger.contains(event.relatedTarget)) close();
  });
  document.addEventListener("focusin", event => {
    if (document.documentElement.dataset.inputModality !== "keyboard") return;
    const trigger = target(event);
    if (trigger) schedule(trigger);
  });
  document.addEventListener("focusout", event => {
    if (pending?.trigger.contains(event.target) && !pending.trigger.contains(event.relatedTarget)) close();
  });
  document.addEventListener("pointerdown", event => {
    if (pending?.trigger.closest(".context-compaction") && pending.trigger.contains(event.target)) return;
    close();
  }, true);
  document.addEventListener("keydown", event => {
    if (pending?.pinned && event.key === "Escape" && !event.isComposing) {
      // 先收起已点击展开的详情，不让同一次 Esc 退出侧边会话。
      close(); event.preventDefault(); event.stopPropagation();
    }
  }, true);
  document.addEventListener("keydown", () => { if (!pending?.pinned) close(); });
  document.addEventListener("scroll", close, true);
  window.addEventListener("resize", close);
  window.addEventListener("blur", close);
  window.visualViewport?.addEventListener("resize", close);
  window.visualViewport?.addEventListener("scroll", close);
  return { close, update, toggleCompaction };
}
export let composerTooltips = null;
export function setComposerTooltips(value) { return composerTooltips = value; }
