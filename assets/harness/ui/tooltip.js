// 主题化悬停提示：替代浏览器原生 title，使输入区、关联菜单及正文压缩提示跟随网页配色。

import { element } from "./dom.js";

export function installComposerTooltips() {
  // 用户要求 hover 信息跟随 Codex 配色；浏览器原生 title 无法应用网页主题，编辑区改用可访问的普通提示层。
  // 压缩信息沿用同一提示层并支持点击固定；其他提示和已有独立上下文提示的触屏交互不变。
  const scope = ".composer-area,.composer-popover,.composer-add-popup,.queued-message-popup,.context-compaction";
  let pending = null, timer = null, closeTimer = null, node = null, observer = null;
  const contents = new WeakMap();
  const available = trigger => trigger.isConnected && !trigger.closest("[hidden],[inert]")
    && document.documentElement.dataset.workspaceSidePanelVisible !== "false";
  function position() {
    if (!pending || !node) return;
    const view = window.visualViewport, left = view?.offsetLeft || 0, top = view?.offsetTop || 0;
    const width = view?.width || window.innerWidth, height = view?.height || window.innerHeight;
    const { trigger, point } = pending;
    if (pending.content) {
      node.style.maxWidth = `${Math.max(0, width - 16)}px`;
      node.style.maxHeight = `${Math.max(0, height - 16)}px`;
    }
    const anchor = trigger.getBoundingClientRect(), box = node.getBoundingClientRect();
    // 用户要求侧边 hover 以悬停点为锚；沿用 6px 上下间距和 8px 视口留白，键盘及固定详情仍按控件定位。
    const x = point?.x ?? anchor.left + anchor.width / 2;
    const above = (point?.y ?? anchor.top) - box.height - 6;
    const below = (point?.y ?? anchor.bottom) + 6;
    node.style.left = `${Math.max(left + 8, Math.min(x - box.width / 2, left + width - box.width - 8))}px`;
    node.style.top = `${Math.max(top + 8, Math.min(above >= top + 8 ? above : below, top + height - box.height - 8))}px`;
  }
  function renderContent() {
    if (!node || !pending?.content) return;
    const scroll = node.scrollTop;
    const focusClose = document.activeElement?.classList?.contains("composer-metrics-detail-close");
    node.replaceChildren();
    pending.content.render(node, pending.pinned, () => {
      pending?.trigger.focus?.(); close();
    });
    node.setAttribute("role", pending.pinned ? "dialog" : "tooltip");
    node.setAttribute("aria-label", "Token 统计");
    if (pending.pinned) node.setAttribute("aria-modal", "false");
    else node.removeAttribute("aria-modal");
    node.scrollTop = scroll;
    if (focusClose) node.querySelector(".composer-metrics-detail-close")?.focus();
  }
  function update(trigger, title, content) {
    if (content) contents.set(trigger, content);
    else if (content === null) contents.delete(trigger);
    if (pending?.trigger !== trigger) return;
    // Fast 等异步状态变化时更新同一提示，含 200ms 等待期间；移开鼠标后也恢复最新文案。
    pending.title = String(title || "").trim();
    const nextContent = contents.get(trigger);
    const contentChanged = nextContent?.key !== pending.content?.key;
    pending.content = nextContent;
    trigger.removeAttribute("title");
    if (!pending.title && !pending.content) { close(); return; }
    if (node && pending.content) {
      if (contentChanged) renderContent();
      position();
    } else if (node && node.textContent !== pending.title) {
      node.textContent = pending.title;
      position();
    }
  }
  function close() {
    clearTimeout(timer); timer = null;
    clearTimeout(closeTimer); closeTimer = null;
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
    const content = contents.get(trigger);
    if (!title && !content) return false;
    pending = { trigger, title: title || "", content, pinned: false, point: null };
    trigger.removeAttribute("title");
    return true;
  }
  function show() {
    clearTimeout(timer); timer = null;
    if (!pending || node) return;
    const { trigger, title } = pending;
    if (!available(trigger)) { close(); return; }
    // Token 统计宽度不足时自然换行；压缩正文提示仍按模型、前后 Token、缩减及耗时分行。
    const className = trigger.id === "session-metrics" ? "composer-tooltip composer-metrics-tooltip"
      : trigger.closest(".context-compaction") ? "composer-tooltip compaction-tooltip" : "composer-tooltip";
    node = element("div", className + (pending.content ? " composer-metrics-detail" : ""), pending.content ? "" : title);
    node.id = "composer-hover-tooltip"; node.setAttribute("role", "tooltip");
    renderContent();
    const descriptions = (trigger.getAttribute("aria-describedby") || "").split(/\s+/).filter(Boolean);
    trigger.setAttribute("aria-describedby", [...new Set([...descriptions, node.id])].join(" "));
    if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
    document.body.append(node);
    if (node.hasAttribute("popover")) node.showPopover();
    position();
    observer = new MutationObserver(() => {
      if (!available(trigger)) close();
    });
    observer.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden", "inert"] });
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-workspace-side-panel-visible"] });
  }
  function schedule(trigger, point = null) {
    if (pending?.trigger === trigger || pending?.pinned || !prepare(trigger)) return;
    pending.point = point;
    // 原生 Codex 普通提示默认延迟 200ms，避免移动鼠标经过工具栏时连续闪烁。
    timer = setTimeout(show, 200);
  }
  function toggleCompaction(trigger) {
    togglePinned(trigger);
  }
  function togglePinned(trigger) {
    clearTimeout(closeTimer); closeTimer = null;
    if (pending?.trigger === trigger && pending.pinned) { close(); return; }
    if (!prepare(trigger)) return;
    // 点击或轻触立即显示同一提示；已经悬停时固定原浮层，不另建一份。
    pending.pinned = true;
    pending.point = null;
    if (node) { renderContent(); position(); }
    else show();
    if (node) trigger.setAttribute("aria-expanded", "true");
  }
  document.addEventListener("pointerover", event => {
    if (event.pointerType !== "mouse") return;
    if (pending?.content && (node?.contains(event.target) || pending.trigger.contains(event.target))) {
      clearTimeout(closeTimer); closeTimer = null;
    }
    const trigger = target(event);
    if (trigger) schedule(trigger, sideTooltipPoint(event, trigger));
  });
  document.addEventListener("pointermove", event => {
    // 200ms 等待期间采用最后的悬停点；弹出后保留位置，避免小幅移动造成气泡抖动。
    if (pending?.point && !node && pending.trigger.contains(event.target))
      pending.point = sideTooltipPoint(event, pending.trigger) ?? pending.point;
  });
  document.addEventListener("pointerout", event => {
    if (!pending || pending.pinned) return;
    if (pending.trigger.contains(event.relatedTarget) || pending.content && node?.contains(event.relatedTarget)) return;
    if (pending.trigger.contains(event.target) || pending.content && node?.contains(event.target)) {
      // 用户要求详细统计可移入阅读、选择和复制；给跨越 6px 间隙保留 150ms，不影响普通文字提示。
      if (pending.content) { clearTimeout(closeTimer); closeTimer = setTimeout(close, 150); }
      else close();
    }
  });
  document.addEventListener("focusin", event => {
    if (document.documentElement.dataset.inputModality !== "keyboard") return;
    const trigger = target(event);
    if (trigger) schedule(trigger);
  });
  document.addEventListener("focusout", event => {
    if (!pending) return;
    // 点击固定后的统计允许选择文本；点到卡片内普通文字导致按钮失焦时不能把卡片关闭。
    if (pending.pinned && pending.content) return;
    if (pending.content && (node?.contains(event.relatedTarget) || pending.trigger.contains(event.relatedTarget))) return;
    if (pending.trigger.contains(event.target) || pending.content && node?.contains(event.target)) close();
  });
  document.addEventListener("click", event => {
    const trigger = event.target?.closest?.("#session-metrics");
    if (!trigger || !trigger.closest(scope)) return;
    // Token 统计没有自身业务点击动作；点击时固定当前 hover，再次点击收起，手机端也可查看同一份详情。
    togglePinned(trigger);
  });
  document.addEventListener("pointerdown", event => {
    if (event.target?.closest?.("#session-metrics")) return;
    if (pending?.content && node?.contains(event.target)) return;
    if (pending?.trigger.closest(".context-compaction") && pending.trigger.contains(event.target)) return;
    close();
  }, true);
  document.addEventListener("keydown", event => {
    const trigger = event.target?.closest?.("#session-metrics");
    if (trigger && ["Enter", " "].includes(event.key) && !event.isComposing) {
      togglePinned(trigger); event.preventDefault(); event.stopPropagation(); return;
    }
    if (pending?.pinned && event.key === "Escape" && !event.isComposing) {
      // 先收起已点击展开的详情，不让同一次 Esc 退出侧边会话。
      close(); event.preventDefault(); event.stopPropagation();
    }
  }, true);
  document.addEventListener("keydown", () => { if (!pending?.pinned) close(); });
  document.addEventListener("scroll", event => {
    if (pending?.content && node?.contains(event.target)) return;
    close();
  }, true);
  window.addEventListener("resize", close);
  window.addEventListener("blur", close);
  window.visualViewport?.addEventListener("resize", close);
  window.visualViewport?.addEventListener("scroll", close);
  return { close, update, toggleCompaction, togglePinned,
    clearContent(trigger) { contents.delete(trigger); if (pending?.trigger === trigger) close(); } };
}

export function sideTooltipPoint(event, trigger) {
  if (event.pointerType !== "mouse" || !Number.isFinite(event.clientX) || !Number.isFinite(event.clientY)) return null;
  if (document.documentElement.dataset.workspaceSidePanel !== "true"
      && !trigger.closest(".workspace.related-session-view:not(.related-session-readonly)")) return null;
  // 取当前页面视口坐标；右侧 iframe 已自带坐标原点，不叠加父页偏移或屏幕坐标。
  return { x: event.clientX, y: event.clientY };
}
export let composerTooltips = null;
export function setComposerTooltips(value) { return composerTooltips = value; }
