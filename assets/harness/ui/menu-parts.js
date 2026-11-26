// 公共菜单内容：Popup、模型和 Slash 共享结构；会话状态、选择提交、焦点与定位仍由调用方负责。
// 用户要求排除常驻分组菜单：仅显式接入 ui-menu / menu-* 的弹出内容采用统一字号、图标与状态。
import { button, element } from "./dom.js";
import { icon } from "./icons.js";

export function createMenuSurface(className = "") {
  return element("div", `ui-menu ${className}`.trim());
}

function appendMenuIcon(node, name, renderIcon) {
  const image = renderIcon ? renderIcon() : name ? icon(name) : null;
  if (!image) return;
  image.classList?.add("menu-icon");
  node.append(image);
}

export function createMenuDescription(text, className = "", tag = "small") {
  return element(tag, `menu-description muted ${className}`.trim(), text);
}

export function createMenuTitle(text, className = "") {
  return element("div", `menu-title ${className}`.trim(), text);
}

export function createMenuEmpty(text, className = "") {
  return createMenuDescription(text, `menu-empty ${className}`.trim(), "p");
}

export function createMenuGroup(label = "", className = "") {
  const node = element("div", `menu-group ${className}`.trim());
  if (label) { node.setAttribute("role", "group"); node.setAttribute("aria-label", label); }
  return node;
}

export function createMenuSeparator(className = "") {
  const node = element("div", `menu-separator ${className}`.trim());
  node.setAttribute("role", "separator");
  return node;
}

export function shouldShowMenuSearch(mode = "off", candidateCount = 0, wasVisible = false) {
  // 用户要求搜索参数只有关闭/自动/显示三档，默认关闭；自动按搜索前候选数判断，9 项起显示。
  // 本次展开中已显示则保留到关闭，避免筛选或异步刷新减少条目后输入框消失；显式关闭仍优先。
  return mode === "on" || mode === "auto" && (wasVisible || candidateCount > 8);
}

export function createMenuItem({
  label, accessibleLabel = label, description, descriptionPlacement = "below",
  icon: iconName, renderIcon, role = "menuitem", selected = null, showCheck = selected !== null,
  checkmark = "✓", disabled = false, danger = false, submenu = false, action,
  className = "", labelClassName = "", descriptionClassName = "", labelTag = "span"
}) {
  const node = button("", accessibleLabel, event => {
    if (!node.disabled) action?.(event);
  }, `menu-item${danger ? " menu-item-danger" : ""} ${className}`.trim());
  node.disabled = !!disabled;
  node.setAttribute("role", role);
  if (selected !== null) node.setAttribute(role === "option" ? "aria-selected" : "aria-checked", String(selected));
  appendMenuIcon(node, iconName, renderIcon);
  const labels = element("span", `menu-item-label${descriptionPlacement === "inline" ? " menu-item-label-inline" : ""} ${labelClassName}`.trim());
  labels.append(element(labelTag, "menu-item-name", label));
  if (description) {
    const detail = createMenuDescription(description, descriptionClassName);
    detail.title = description;
    labels.append(detail);
  }
  node.append(labels);
  if (showCheck) {
    // 用户要求勾号与文字等大，并保留未选中时的占位；优先事项仍可传入既有“√”符号。
    const check = element("span", "choice-check menu-check", selected ? checkmark || "✓" : "");
    check.setAttribute("aria-hidden", "true");
    node.append(check);
  }
  if (submenu) {
    node.setAttribute("aria-haspopup", "menu"); node.setAttribute("aria-expanded", "false");
    const arrow = icon("chevron"); arrow.classList.add("menu-icon", "menu-submenu-arrow");
    node.append(arrow);
  }
  return node;
}

export function createMenuSearch({ id, label, placeholder = label, value = "", onQuery, className = "" }) {
  const input = element("input", `menu-search ${className}`.trim());
  if (id) input.id = id;
  input.type = "search"; input.autocomplete = "off"; input.value = value;
  input.placeholder = placeholder; input.setAttribute("aria-label", label);
  let applied = value;
  const update = () => {
    if (input.value === applied) return;
    applied = input.value; onQuery?.(input.value);
  };
  input.oninput = event => { if (!event.isComposing) update(); };
  input.oncompositionend = update;
  return input;
}

export function createMenuButton({ label, accessibleLabel = label, icon: iconName, action, className = "" }) {
  const node = button("", accessibleLabel, event => {
    if (!node.disabled) action?.(event);
  }, `menu-button ${className}`.trim());
  appendMenuIcon(node, iconName);
  if (label) node.append(element("span", "menu-button-label", label));
  return node;
}

export function createMenuHelp({ label, href, className = "" }) {
  const node = element("a", `menu-help menu-description ${className}`.trim());
  node.href = href; node.target = "_blank"; node.rel = "noopener noreferrer";
  node.append(element("span", "", label));
  appendMenuIcon(node, "window");
  return node;
}
