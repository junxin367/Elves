// 通用弹层宿主：Select 与按钮菜单共用的定位、搜索、键盘导航、焦点恢复与 dialog 顶层规则。
// 业务浮层（如输入区模型/Skill 菜单）通过 registerAppMenuCloser 注册，打开弹层或确认框前统一收起。

import { menuHorizontalLeft } from "./position.js";
import { element } from "./dom.js";
import {
  createMenuButton, createMenuDescription, createMenuEmpty, createMenuGroup, createMenuHelp,
  createMenuItem, createMenuSearch, createMenuSeparator, createMenuSurface, createMenuTitle, shouldShowMenuSearch
} from "./menu-parts.js";

export let activePopup = null;
export function setActivePopup(value) { return activePopup = value; }
// 打开通用弹层或确认框前先收起业务浮层（输入区模型/Skill 菜单）；具体实现由入口启动时注册，
// 组件本身不依赖业务模块。注册前为空操作，入口须在任何弹层打开前完成注册。
export let closeAppMenus = () => {};
export function registerAppMenuCloser(close) { closeAppMenus = close; }
// Select 选项的提交由 ui/select.js 在加载时注册；弹层只负责展示、搜索与键盘导航，不反向依赖 Select 实现。
let commitSelection = (_trigger, _value) => false;
export function registerSelectCommitter(commit) { commitSelection = commit; }
export function closePopup(restoreFocus = false) {
  if (!activePopup) return;
  if (activePopup.submenuTimer) clearTimeout(activePopup.submenuTimer);
  if (activePopup.submenu) closePopupSubmenu();
  const { node, trigger, dismiss } = activePopup;
  activePopup = null;
  trigger.setAttribute("aria-expanded", "false");
  trigger.removeAttribute("aria-controls");
  if (node.hasAttribute("popover") && node.matches(":popover-open")) node.hidePopover();
  dismiss?.remove();
  node.remove();
  if (restoreFocus && trigger.isConnected && !trigger.disabled) trigger.focus();
}
export function togglePopup(trigger) {
  if (activePopup?.trigger === trigger) closePopup(true);
  else openPopup(trigger);
}
export function openPopup(trigger, {
  query = "", edge = null, focus = true, focusSearch = true, position = null, host = null
} = {}) {
  if (trigger.disabled || !trigger.isConnected) return;
  closePopup(); closeAppMenus();
  const node = createMenuSurface(`ui-popup${trigger.popupClassName ? ` ${trigger.popupClassName}` : ""}`);
  node.id = "active-popup";
  const selecting = !!trigger.selectOptions;
  node.setAttribute("aria-label", trigger.getAttribute("aria-label"));
  const title = trigger.popupTitle ? createMenuTitle(trigger.popupTitle, "popup-title") : null;
  let help = null;
  if (trigger.popupHelp) {
    help = createMenuHelp({ ...trigger.popupHelp, className: "popup-help" });
  }
  // 用户要求 Codex 顶部标题与“了解更多”同一行；完整标题保留在 DOM 和悬停提示中，窄屏由样式省略。
  if (title && help && trigger.popupHelpAfterTitle) {
    const header = createMenuGroup("", "popup-header");
    title.title = trigger.popupTitle;
    header.append(title, help); node.append(header);
  } else {
    if (title) node.append(title);
    if (help && trigger.popupHelpAfterTitle) node.append(help);
  }
  const list = createMenuGroup("", "popup-list");
  list.setAttribute("role", selecting ? "listbox" : "menu");
  list.setAttribute("aria-label", trigger.getAttribute("aria-label"));
  const note = trigger.popupNote ? createMenuDescription(trigger.popupNote, "popup-note", "div") : null;
  // 用户要求 Claude 权限菜单的说明位于选项上方；其他弹层继续沿用底部说明。
  if (note && trigger.popupNotePosition === "top") node.append(note);
  node.append(list);
  if (note && trigger.popupNotePosition !== "top") node.append(note);
  if (help && !trigger.popupHelpAfterTitle) node.append(help);
  const choices = selecting ? trigger.selectOptions : trigger.menuOptions;
  activePopup = { node, trigger, list, input: null, query, position,
    searchBefore: note && trigger.popupNotePosition === "top" ? note : list,
    index: Math.max(0, choices.findIndex(option => option.value === trigger.value)), choices: [] };
  trigger.setAttribute("aria-expanded", "true"); trigger.setAttribute("aria-controls", node.id);
  const popupHost = host || trigger.closest("dialog,[data-popup-host]") || document.body;
  if (trigger.popupDismissLayer) {
    const dismiss = element("div", "session-tools-menu-dismiss");
    dismiss.onpointerdown = event => { event.preventDefault(); event.stopPropagation(); };
    dismiss.onclick = event => {
      event.preventDefault(); event.stopPropagation(); closePopup(true);
    };
    activePopup.dismiss = dismiss;
    popupHost.append(dismiss);
  }
  if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
  popupHost.append(node);
  renderPopup();
  if (activePopup?.node !== node) return;
  if (node.hasAttribute("popover")) node.showPopover();
  const buttons = [...list.querySelectorAll("button")];
  const enabled = buttons.filter(option => !option.disabled);
  const selected = buttons[activePopup.index];
  const target = edge === "last" ? enabled.at(-1) : edge === "first" ? enabled[0]
    : selected && !selected.disabled ? selected : enabled[0];
  activePopup.index = Math.max(0, buttons.indexOf(target));
  if (focus) {
    const input = activePopup.input;
    if (input && focusSearch && trigger.selectFocusSearch?.()) {
      input.focus({ preventScroll: true });
      input.setSelectionRange(input.value.length, input.value.length);
    } else target?.focus();
  }
  fitPopup();
  node.onkeydown = event => {
    if (event.isComposing || activePopup?.node !== node) return;
    const input = activePopup.input;
    if (activePopup.submenu?.node.contains(event.target)) return;
    if (activePopup.submenu && ["Escape", "ArrowLeft"].includes(event.key)) {
      event.preventDefault(); event.stopPropagation(); closePopupSubmenu(true); return;
    }
    const options = [...list.querySelectorAll("button:not(:disabled)")];
    if (event.key === "ArrowRight" && document.activeElement?.getAttribute("aria-haspopup") === "menu") {
      event.preventDefault(); event.stopPropagation();
      document.activeElement.click();
    } else if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key) && !(event.target === input && ["Home", "End"].includes(event.key))) {
      event.preventDefault();
      if (!options.length) return;
      const index = options.indexOf(document.activeElement);
      const next = event.key === "Home" ? 0 : event.key === "End" ? options.length - 1
        : index < 0 ? event.key === "ArrowDown" ? 0 : options.length - 1
        : (index + (event.key === "ArrowDown" ? 1 : options.length - 1)) % options.length;
      activePopup.index = next;
      options[next]?.focus(); options[next]?.scrollIntoView({ block: "nearest" });
    } else if (event.target === input && event.key === "Enter") {
      event.preventDefault();
      const selected = list.querySelectorAll("button")[activePopup.index];
      (selected && !selected.disabled ? selected : options[0])?.click();
    } else if (event.key === "Tab") {
      if (help && event.target !== help && !event.shiftKey) {
        event.preventDefault(); help.focus();
      } else if (help && event.target === help && event.shiftKey) {
        event.preventDefault(); options.at(-1)?.focus();
      } else closePopup(true);
    } else if (event.target !== input && event.key.length === 1 && !event.ctrlKey && !event.metaKey && event.key !== " ") {
      const start = options.indexOf(document.activeElement) + 1;
      const ordered = [...options.slice(start), ...options.slice(0, start)];
      ordered.find(option => option.textContent.trim().toLowerCase().startsWith(event.key.toLowerCase()))?.focus();
    }
  };
}
function syncPopupSearch(popup) {
  const { trigger, node, list, input } = popup;
  // 调用方已过滤隐藏对象；不把禁用占位或固定底部操作计入门槛，也不使用当前搜索结果数。
  const candidates = (trigger.selectOptions || trigger.menuOptions).filter(option => !option.disabled && !option.footer);
  const visible = shouldShowMenuSearch(trigger.searchMode, candidates.length, !!input);
  const restoreFocus = !!input && document.activeElement === input && !visible;
  if (!visible) {
    input?.remove();
    popup.input = null;
    popup.query = "";
  } else if (!input) {
    const search = createMenuSearch({
      className: "popup-search", value: popup.query,
      label: trigger.selectSearchLabel || "搜索可选项目",
      placeholder: trigger.selectSearchPlaceholder || "搜索项目名称或路径",
      onQuery(value) {
        if (activePopup !== popup || popup.input !== search) return;
        popup.query = value; popup.index = 0;
        list.scrollTop = 0;
        const options = list.querySelector(".popup-options");
        if (options) options.scrollTop = 0;
        renderPopup();
      }
    });
    popup.input = search;
    // 异步加载跨过门槛时在同一弹层内加入搜索，保持原说明顺序，不重新打开弹层或抢走焦点。
    node.insertBefore(search, popup.searchBefore);
  }
  return restoreFocus;
}
export function renderPopup() {
  if (!activePopup) return;
  const searchFocusRemoved = syncPopupSearch(activePopup);
  const { trigger, list, query, submenu } = activePopup;
  const listScroll = list.scrollTop, optionsScroll = list.querySelector(".popup-options")?.scrollTop;
  const restoreFocus = searchFocusRemoved || list.contains(document.activeElement);
  const focused = restoreFocus ? document.activeElement.dataset.value : null;
  const choices = (trigger.selectOptions || trigger.menuOptions).filter(option =>
    option.footer || !query || `${option.label} ${option.description || ""} ${option.searchText || ""}`.toLowerCase().includes(query.trim().toLowerCase()));
  const custom = trigger.selectCustomOption?.(query);
  if (custom && !trigger.selectOptions.some(option => option.value === custom.value)) choices.push(custom);
  activePopup.choices = choices;
  list.replaceChildren();
  const hasFooter = choices.some(option => option.footer);
  list.classList.toggle("has-footer", hasFooter);
  const options = hasFooter ? element("div", "popup-options") : list;
  if (hasFooter) list.append(options);
  if (!choices.some(option => !option.footer)) options.append(createMenuEmpty(
    query ? trigger.selectSearchLabel ? "没有匹配的选项，请修改搜索条件" : "没有匹配的项目，请修改搜索条件"
      : trigger.popupEmptyText || (hasFooter ? "本机暂无可用项目" : "暂无可用选项"), "popover-empty"));
  choices.forEach(option => {
    if (option.separatorBefore) {
      (option.footer ? list : options).append(createMenuSeparator("popup-separator"));
    }
    const node = createMenuItem({
      ...option, selected: trigger.selectOptions ? trigger.value === option.value : option.checkable ? !!option.selected : null,
      role: trigger.selectOptions ? "option" : option.checkable ? "menuitemcheckbox" : "menuitem",
      submenu: !!option.submenu, labelClassName: "popup-option-label",
      className: `popup-option${option.footer ? " popup-option-footer" : ""}${option.danger ? " popup-option-danger" : ""}`,
      action: () => {
        if (trigger.disabled || node.disabled || activePopup?.trigger !== trigger || !list.contains(node)) return;
        if (option.submenu) { openPopupSubmenu(node, option, true); return; }
        // 实例多选菜单允许连续选择；旧菜单节点的迟到点击不能覆盖重绘后的选择状态。
        if (trigger.selectOptions || !option.keepOpen) closePopup(true);
        if (trigger.selectOptions) {
          commitSelection(trigger, option.value);
        } else void option.action();
      }
    });
    node.tabIndex = -1;
    node.onfocus = () => {
      if (!activePopup) return;
      activePopup.index = choices.indexOf(option);
      if (activePopup.submenu && activePopup.submenu.value !== node.dataset.value) closePopupSubmenu();
    };
    node.dataset.value = option.value ?? option.label;
    node.onpointerenter = event => {
      if (event.pointerType === "touch" || node.disabled || activePopup?.trigger !== trigger) return;
      clearTimeout(activePopup.submenuTimer);
      // 子菜单稍延后展开，允许指针从父项斜向移入子菜单，避免掠过其他父项时立即切走。
      activePopup.submenuTimer = setTimeout(() => {
        if (activePopup?.trigger !== trigger || !list.contains(node)) return;
        if (option.submenu) openPopupSubmenu(node, option);
        else if (activePopup.submenu) closePopupSubmenu();
      }, 120);
    };
    node.onpointerleave = () => { if (activePopup?.trigger === trigger) clearTimeout(activePopup.submenuTimer); };
    (option.footer ? list : options).append(node);
    if (focused === node.dataset.value && !node.disabled) node.focus({ preventScroll: true });
    if (submenu && submenu.value === node.dataset.value) {
      if (node.disabled || submenu.key !== option.submenuKey || !option.submenu) closePopupSubmenu();
      else {
        // 用户要求侧栏刷新不重开弹层；同一父项仅换绑新节点，子菜单内容、焦点及在途读取保持不变。
        submenu.source = node; submenu.option = option;
        node.setAttribute("aria-expanded", "true"); node.setAttribute("aria-controls", submenu.node.id);
      }
    }
  });
  if (submenu && activePopup.submenu === submenu && !list.contains(submenu.source)) closePopupSubmenu();
  if (restoreFocus && !list.contains(document.activeElement)) {
    // 动态环境列表变化时只移动键盘焦点，不改选中值，也不启用已失效的目标。
    (list.querySelector('button[aria-selected="true"]:not(:disabled),button[aria-checked="true"]:not(:disabled)')
      || list.querySelector("button:not(:disabled)"))?.focus({ preventScroll: true });
  }
  fitPopup();
  // 用户要求动态重绘保留阅读位置；项目菜单的固定底部操作与选项区分别恢复。
  list.scrollTop = listScroll;
  if (hasFooter && optionsScroll != null) options.scrollTop = optionsScroll;
}
export function fitPopup() {
  if (!activePopup) return;
  const { node, trigger, position } = activePopup;
  if (!trigger.isConnected || trigger.hidden || trigger.disabled) { closePopup(); return; }
  const viewport = window.visualViewport;
  const top = viewport?.offsetTop || 0, left = viewport?.offsetLeft || 0;
  const width = viewport?.width || window.innerWidth, height = viewport?.height || window.innerHeight;
  const box = trigger.getBoundingClientRect(), gap = 4, padding = 8;
  const scroller = trigger.closest(".sidebar-content")?.getBoundingClientRect();
  if (box.bottom <= top || box.top >= top + height || box.right <= left || box.left >= left + width
      || (scroller && (box.bottom <= scroller.top || box.top >= scroller.bottom))) { closePopup(); return; }
  // 用户要求 Open in 固定向左：业务菜单注册定位函数后，所有重绘及滚动/缩放重定位均回调它，不能被通用方向规则覆盖。
  if (typeof trigger.popupFit === "function") {
    trigger.popupFit();
    if (activePopup?.submenu) fitPopupSubmenu();
    return;
  }
  const originTop = position?.y ?? box.top;
  const originBottom = position?.y ?? box.bottom + gap;
  const originLeft = position?.x ?? box.left;
  const availableBelow = Math.max(0, top + height - originBottom - padding);
  const availableAbove = Math.max(0, originTop - (position ? 0 : gap) - top - padding);
  // 宽度按业务声明的搜索能力保持稳定，不随自动模式的输入框显隐反复跳动。
  const preferredWidth = trigger.popupWidth || (["auto", "on"].includes(trigger.searchMode) ? 300 : 200);
  const availableWidth = Math.max(0, width - padding * 2);
  const maxHeight = Math.min(trigger.popupMaxHeight || 360, height * .8);
  if (trigger.popupAutoWidth) {
    // 自适应菜单以配置下限随内容扩展，窄屏保留两侧各 8px；模型类菜单另设 popupMaxWidth（350px）与输入区模型菜单统一上限。
    const minWidth = Math.min(preferredWidth, availableWidth);
    const maxWidth = trigger.popupMaxWidth
      ? Math.min(Math.max(trigger.popupMaxWidth, minWidth), availableWidth)
      : availableWidth;
    node.style.width = "max-content";
    node.style.minWidth = `${minWidth}px`;
    node.style.maxWidth = `${maxWidth}px`;
  } else {
    node.style.width = `${Math.min(Math.max(box.width, preferredWidth), availableWidth)}px`;
  }
  node.style.maxHeight = `${Math.min(maxHeight, Math.max(availableAbove, availableBelow))}px`;
  const menuHeight = node.getBoundingClientRect().height;
  const above = trigger.popupDirection === "up" ? availableAbove > 0
    : availableBelow < menuHeight && availableAbove > availableBelow;
  node.style.maxHeight = `${Math.min(maxHeight, above ? availableAbove : availableBelow)}px`;
  const actualHeight = node.getBoundingClientRect().height;
  const actualWidth = node.getBoundingClientRect().width;
  // 普通 Select、按钮菜单和模型菜单共用水平规则；右键/长按传入的点击坐标继续作为特殊锚点。
  node.style.left = `${position
    ? Math.max(left + padding, Math.min(originLeft, left + width - actualWidth - padding))
    : menuHorizontalLeft(box, actualWidth, { left, width }, padding)}px`;
  node.style.top = `${Math.max(top + padding, Math.min(above ? originTop - (position ? 0 : gap) - actualHeight : originBottom, top + height - padding - actualHeight))}px`;
  node.dataset.direction = above ? "up" : "down";
  if (activePopup.submenu) fitPopupSubmenu();
}

function closePopupSubmenu(restoreFocus = false) {
  const popup = activePopup, submenu = popup?.submenu;
  if (!submenu) return;
  clearTimeout(popup.submenuTimer); popup.submenu = null;
  submenu.controller?.abort();
  submenu.source.setAttribute("aria-expanded", "false");
  submenu.source.removeAttribute("aria-controls");
  if (submenu.node.hasAttribute("popover") && submenu.node.matches(":popover-open")) submenu.node.hidePopover();
  submenu.node.remove(); popup.node.classList.remove("has-submenu");
  if (restoreFocus && submenu.source.isConnected) submenu.source.focus({ preventScroll: true });
}

function openPopupSubmenu(source, option, focus = false) {
  const popup = activePopup;
  if (!popup || source.disabled || !popup.list.contains(source)) return;
  clearTimeout(popup.submenuTimer);
  if (popup.submenu?.value === source.dataset.value) {
    if (focus) {
      popup.submenu.focusWhenLoaded = true;
      popup.submenu.list.querySelector("button:not(:disabled)")?.focus();
    }
    return;
  }
  if (popup.submenu) closePopupSubmenu();
  const node = createMenuSurface(`ui-popup popup-submenu${popup.trigger.popupClassName ? ` ${popup.trigger.popupClassName}` : ""}${option.submenuClassName ? ` ${option.submenuClassName}` : ""}`);
  node.id = `${popup.node.id}-submenu`; node.setAttribute("aria-label", option.label);
  const back = createMenuButton({ label: option.label, accessibleLabel: `返回，${option.label}`,
    icon: "back", action: () => closePopupSubmenu(true), className: "popup-submenu-back" });
  back.hidden = true;
  const list = createMenuGroup("", "popup-list");
  list.setAttribute("role", "menu"); list.setAttribute("aria-label", option.label);
  node.append(back, list);
  const submenu = { node, list, back, source, option, value: source.dataset.value,
    key: option.submenuKey, controller: null, focusWhenLoaded: focus };
  popup.submenu = submenu; popup.node.classList.add("has-submenu");
  source.setAttribute("aria-expanded", "true"); source.setAttribute("aria-controls", node.id);
  // 子菜单保留在父菜单 DOM 内供外部点击判断；独立 popover 使其不被父菜单的滚动边界裁切。
  if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
  popup.node.append(node);
  if (node.hasAttribute("popover")) node.showPopover();
  node.onpointerenter = () => clearTimeout(popup.submenuTimer);
  node.onkeydown = event => {
    if (event.isComposing || activePopup !== popup || popup.submenu !== submenu) return;
    if (["Escape", "ArrowLeft"].includes(event.key)) {
      event.preventDefault(); event.stopPropagation(); closePopupSubmenu(true); return;
    }
    if (event.key === "Tab") { closePopup(true); return; }
    const options = [...list.querySelectorAll("button:not(:disabled)")];
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      event.preventDefault(); event.stopPropagation();
      const index = options.indexOf(document.activeElement);
      const next = event.key === "Home" ? 0 : event.key === "End" ? options.length - 1
        : index < 0 ? event.key === "ArrowDown" ? 0 : options.length - 1
        : (index + (event.key === "ArrowDown" ? 1 : options.length - 1)) % options.length;
      options[next]?.focus(); options[next]?.scrollIntoView({ block: "nearest" });
    }
  };
  if (Array.isArray(option.submenu)) renderPopupSubmenu(popup, submenu, option.submenu);
  else void loadPopupSubmenu(popup, submenu);
}

async function loadPopupSubmenu(popup, submenu) {
  submenu.controller?.abort();
  const controller = new AbortController(); submenu.controller = controller;
  submenu.node.setAttribute("aria-busy", "true");
  renderPopupSubmenu(popup, submenu, [{ label: "正在加载打开方式…", disabled: true }]);
  try {
    const choices = await submenu.option.submenu({ signal: controller.signal });
    if (activePopup !== popup || popup.submenu !== submenu || controller.signal.aborted) return;
    renderPopupSubmenu(popup, submenu, choices);
  } catch (error) {
    if (activePopup !== popup || popup.submenu !== submenu || controller.signal.aborted) return;
    // 文件已不存在时直接显示简短状态，不再把“重新加载打开方式”与删除提示叠加在同一行。
    const message = error?.message || "";
    if (/文件不存在或已(?:被)?删除/u.test(message)) {
      renderPopupSubmenu(popup, submenu, [{ label: "文件不存在或已删除", disabled: true }]);
      return;
    }
    renderPopupSubmenu(popup, submenu, [{ label: "重新加载打开方式",
      description: message || "读取失败，请重试", keepOpen: true,
      action: () => loadPopupSubmenu(popup, submenu) }]);
  } finally {
    if (submenu.controller === controller) submenu.node.removeAttribute("aria-busy");
  }
}

function renderPopupSubmenu(popup, submenu, choices) {
  if (activePopup !== popup || popup.submenu !== submenu) return;
  const scrollTop = submenu.list.scrollTop;
  const focused = submenu.list.contains(document.activeElement) ? document.activeElement.dataset.value : null;
  submenu.list.replaceChildren();
  if (!choices.length) submenu.list.append(createMenuEmpty("暂无可用选项", "popover-empty"));
  for (const option of choices) {
    const node = createMenuItem({ ...option, labelClassName: "popup-option-label",
      className: `popup-option${option.danger ? " popup-option-danger" : ""}`, action: () => {
        if (activePopup !== popup || popup.submenu !== submenu || node.disabled || !submenu.list.contains(node)) return;
        if (!option.keepOpen) closePopup(true);
        void option.action?.();
      }
    });
    node.tabIndex = -1;
    node.dataset.value = option.value ?? option.label; node.setAttribute("role", "menuitem");
    submenu.list.append(node);
    if (focused === node.dataset.value && !node.disabled) node.focus({ preventScroll: true });
  }
  fitPopupSubmenu();
  submenu.list.scrollTop = scrollTop;
  if (focused != null && !submenu.list.contains(document.activeElement))
    submenu.list.querySelector("button:not(:disabled)")?.focus({ preventScroll: true });
  if (submenu.focusWhenLoaded) {
    const first = submenu.list.querySelector("button:not(:disabled)");
    if (first) { first.focus(); submenu.focusWhenLoaded = false; }
    else { submenu.node.tabIndex = -1; submenu.node.focus({ preventScroll: true }); }
  }
}

function fitPopupSubmenu() {
  const popup = activePopup, submenu = popup?.submenu;
  if (!submenu) return;
  if (!submenu.source.isConnected || submenu.source.disabled) { closePopupSubmenu(); return; }
  const viewport = window.visualViewport;
  const left = viewport?.offsetLeft || 0, top = viewport?.offsetTop || 0;
  const width = viewport?.width || window.innerWidth, height = viewport?.height || window.innerHeight;
  const source = submenu.source.getBoundingClientRect(), parent = popup.node.getBoundingClientRect();
  if (source.bottom <= parent.top || source.top >= parent.bottom) { closePopupSubmenu(); return; }
  // 用户要求会话右键使用两级菜单；子菜单贴父菜单优先向右，空间不足向左，窄屏重叠时提供返回入口。
  // 四边沿用通用弹层的 8px 留白，父子间的 3px 间隔配合悬停延迟，便于移入子菜单。
  const padding = 8, gap = 3, menuWidth = Math.max(0, Math.min(submenu.option.submenuWidth || 200, width - padding * 2));
  const rightSpace = left + width - padding - parent.right - gap;
  const leftSpace = parent.left - gap - left - padding;
  const overlap = Math.max(rightSpace, leftSpace) < menuWidth;
  const rightward = rightSpace >= menuWidth || rightSpace >= leftSpace;
  submenu.back.hidden = !overlap;
  submenu.node.style.width = `${menuWidth}px`;
  submenu.node.style.maxHeight = `${Math.max(0, height - padding * 2)}px`;
  submenu.node.dataset.direction = overlap ? "overlap" : rightward ? "right" : "left";
  const x = overlap ? parent.left : rightward ? parent.right + gap : parent.left - gap - menuWidth;
  const menuHeight = submenu.node.getBoundingClientRect().height;
  submenu.node.style.left = `${Math.max(left + padding, Math.min(x, left + width - menuWidth - padding))}px`;
  submenu.node.style.top = `${Math.max(top + padding, Math.min(source.top - 4, top + height - menuHeight - padding))}px`;
}
