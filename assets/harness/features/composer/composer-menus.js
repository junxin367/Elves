// 输入区浮层：模型菜单渲染与新会话所选模型的记忆、浮层与审批区的视口适配，以及统一收起输入区菜单；模型菜单搜索词在 options-state.js。

import { $ } from "../../ui/dom.js";
import { createMenuEmpty, createMenuGroup, createMenuItem, createMenuSearch, shouldShowMenuSearch } from "../../ui/menu-parts.js";
import { activePopup, fitPopup } from "../../ui/popup.js";
import { menuHorizontalLeft } from "../../ui/position.js";
import { updateComposer } from "../../core/actions.js";
import { creating } from "../../core/state.js";
import { fitClaudeUsageTip } from "./claude-usage.js";
import { closeContextUsageTip, fitContextUsageTip } from "./metrics.js";
import { currentEffort, currentModel, currentModelId } from "./model-choice.js";
import { rememberNewChatSettings } from "./new-chat-settings.js";
import {
  defaultModelEffort, modelChoiceVersion, modelQuery, models, selectedEffort, setModelChoiceVersion,
  setModelQuery, setSelectedEffort, setSelectedModel
} from "./options-state.js";
import { effortLabels } from "./prompt-editor.js";
import { fitGoalEditor } from "../goal/state.js";

export function closeSkillMenu() {
  const menu = $("#skill-menu"); if (menu) menu.hidden = true;
  $("#prompt")?.setAttribute("aria-expanded", "false");
  $("#prompt")?.removeAttribute("aria-activedescendant");
}
export function closeModelMenu() {
  const menu = $("#model-settings");
  if (menu) { menu.hidden = true; menu.menuSearchVisible = false; }
  $("#model-button")?.setAttribute("aria-expanded", "false");
}
export function closeComposerMenus({ keepContextUsage = false } = {}) {
  closeModelMenu();
  closeSkillMenu();
  if (!keepContextUsage) closeContextUsageTip();
}
export function renderModelMenu() {
  const menu = $("#model-settings");
  if (!menu) return;
  const focusIndex = [...menu.querySelectorAll("button")].indexOf(document.activeElement);
  const focusKey = menu.contains(document.activeElement) ? document.activeElement.dataset.choiceKey : null;
  const modelScroll = $(".model-options", menu)?.scrollTop || 0;
  const effortScroll = $(".effort-options", menu)?.scrollTop || 0;
  const searchFocused = document.activeElement === $("#model-search");
  const searchSelection = searchFocused ? [document.activeElement.selectionStart, document.activeElement.selectionEnd] : null;
  // 模型入口明确使用自动模式：只统计未隐藏的完整模型候选，不计思考深度或当前值提示占位。
  const availableModels = models.filter(model => model.hiddenByInstance !== true);
  menu.menuSearchVisible = shouldShowMenuSearch($("#model-button")?.searchMode, availableModels.length, menu.menuSearchVisible);
  if (!menu.menuSearchVisible && modelQuery) setModelQuery("");
  menu.replaceChildren();
  if (menu.menuSearchVisible) {
    const input = createMenuSearch({ id: "model-search", className: "model-search",
      label: "搜索模型", value: modelQuery, onQuery(value) {
        if ($("#model-search") !== input) return;
        setModelQuery(value); renderModelMenu();
      } });
    menu.append(input);
  }
  const choice = (text, checked, action, description, key = text) => {
    const node = createMenuItem({ label: text, description, selected: checked, action,
      role: "menuitemradio", className: "model-option", labelClassName: "model-option-label" });
    node.dataset.choiceKey = key;
    node.title = text;
    return node;
  };
  const applyModelMenuChoice = () => {
    setModelChoiceVersion(modelChoiceVersion + 1);
    renderModelMenu();
    updateComposer();
    closeComposerMenus();
    $("#prompt").focus();
  };
  // 用户要求共用菜单项，但模型与思考深度的选择、独立滚动和焦点恢复仍归本模块管理。
  const group = createMenuGroup("模型", "model-options");
  const modelId = currentModelId(), matchedModel = currentModel(), selectedReasoning = currentEffort();
  // 用户要求 Claude 可按实例隐藏模型；完整目录保留用于当前模型和能力判断，仅过滤可重新选择的条目。
  const visibleModels = availableModels.filter(model =>
    `${model.displayName || ""} ${model.model}`.toLowerCase().includes(modelQuery.trim().toLowerCase()));
  for (const model of visibleModels) group.append(choice(model.displayName || model.model, matchedModel === model, () => {
    if (currentModel() !== model) {
      // 用户要求切换模型不丢弃思考深度，Claude 和 Codex 的已有及新建会话共用此规则。
      // 必须先读取当前深度再更新模型；仅在目标模型未报告支持原深度时才回退到其可用默认值。
      const previousEffort = currentEffort();
      setSelectedModel(model.model);
      setSelectedEffort(defaultModelEffort(model, previousEffort));
    }
    if (creating) rememberNewChatModel(model.model);
    applyModelMenuChoice();
  }, null, `model:${model.model}`));
  if (modelId && (!matchedModel || matchedModel.hiddenByInstance === true)
      && `${matchedModel?.displayName || ""} ${modelId}`.toLowerCase().includes(modelQuery.trim().toLowerCase())) {
    const current = choice(matchedModel?.displayName || modelId, true, () => {},
      matchedModel?.hiddenByInstance === true ? "当前所选模型，已在实例设置中隐藏" : "当前会话模型，原生模型列表暂未提供",
      `model:${modelId}`);
    current.disabled = true; group.prepend(current);
  }
  if (!group.children.length) group.append(createMenuEmpty(
    models.length && models.every(model => model.hiddenByInstance === true)
      ? "当前实例的模型均已隐藏，可在会话实例设置中恢复显示"
      : models.length ? "没有匹配的模型，请修改搜索条件" : "暂无可选模型，请稍后重试", "popover-empty"));
  menu.append(group);
  const efforts = createMenuGroup("思考深度", "effort-options");
  const supportedEfforts = (currentModel()?.supportedReasoningEfforts || []).filter(effort => effort.reasoningEffort !== "minimal");
  for (const effort of supportedEfforts) {
    efforts.append(choice(effortLabels[effort.reasoningEffort] || effort.reasoningEffort,
      selectedReasoning === effort.reasoningEffort,
      () => {
        setSelectedEffort(effort.reasoningEffort);
        if (creating) rememberNewChatSettings({ model: currentModelId(), reasoningEffort: selectedEffort });
        applyModelMenuChoice();
      }, null, `effort:${effort.reasoningEffort}`));
  }
  if (!efforts.children.length) efforts.append(createMenuEmpty("此模型暂无可选思考深度", "popover-empty"));
  menu.append(efforts);
  fitComposerMenu(menu);
  group.scrollTop = modelScroll; efforts.scrollTop = effortScroll;
  if (searchFocused && $("#model-search")) {
    $("#model-search").focus({ preventScroll: true });
    $("#model-search").setSelectionRange(...searchSelection);
  } else if (focusIndex >= 0 || searchFocused) {
    const buttons = [...menu.querySelectorAll("button")];
    (buttons.find(node => node.dataset.choiceKey === focusKey && !node.disabled)
      || (buttons[focusIndex]?.disabled ? null : buttons[focusIndex])
      || buttons.find(node => !node.disabled))?.focus({ preventScroll: true });
  }
}
export function fitComposerMenu(menu) {
  if (menu.id === "model-settings") {
    const anchor = $("#model-button");
    if (!anchor || menu.hidden) return;
    const viewport = window.visualViewport;
    const top = viewport?.offsetTop || 0, left = viewport?.offsetLeft || 0;
    const width = viewport?.width || window.innerWidth, height = viewport?.height || window.innerHeight;
    const box = anchor.getBoundingClientRect(), gap = 4, padding = 8;
    const above = Math.max(0, box.top - top - gap - padding);
    const below = Math.max(0, top + height - box.bottom - gap - padding);
    const limit = Math.min(560, height * .8, Math.max(above, below));
    const compact = width <= 760 || window.matchMedia("(pointer:coarse)").matches;
    menu.style.width = "max-content";
    menu.style.minWidth = `${Math.min(compact ? 220 : 300, width - padding * 2)}px`;
    menu.style.maxWidth = `${Math.min(compact ? 280 : 350, width - padding * 2)}px`;
    menu.style.maxHeight = `${limit}px`;
    $(".effort-options", menu)?.style.setProperty("max-height", `${Math.max(0, (limit - 80) * .45)}px`);
    const upwards = below < menu.getBoundingClientRect().height && above > below;
    const available = Math.min(560, height * .8, upwards ? above : below);
    menu.style.maxHeight = `${available}px`;
    $(".effort-options", menu)?.style.setProperty("max-height", `${Math.max(0, (available - 80) * .45)}px`);
    const actual = menu.getBoundingClientRect();
    menu.style.left = `${menuHorizontalLeft(box, actual.width, { left, width }, padding)}px`;
    menu.style.top = `${upwards ? box.top - gap - actual.height : box.bottom + gap}px`;
    menu.dataset.direction = upwards ? "up" : "down";
    return;
  }
  // 命令/Skill 建议属于整个输入框的补全层，保留输入框对齐，不套用按钮菜单的左右展开规则。
  const top = $("#composer")?.getBoundingClientRect().top || 300;
  const viewport = window.visualViewport, height = viewport?.height || window.innerHeight;
  menu.style.maxHeight = `${Math.max(0, Math.min(480, height * .8, top - (viewport?.offsetTop || 0) - 16))}px`;
}
export function fitApprovalArea() {
  const root = $("#approval-area"), anchor = root?.parentElement;
  if (!root?.childElementCount || !anchor) return;
  const viewport = window.visualViewport, padding = 8, gap = 8;
  const top = viewport?.offsetTop || 0, height = viewport?.height || window.innerHeight;
  const anchorBottom = anchor.getBoundingClientRect().top - gap;
  let contentBottom = anchorBottom;
  // 用户要求问答卡贴在输入栏上方，可直接覆盖排队、目标等附属信息；普通权限审批仍避让这些内容。
  if (!root.querySelector(".question-card")) {
    for (let previous = anchor.previousElementSibling; previous; previous = previous.previousElementSibling) {
      if (previous.hidden) continue;
      const box = previous.getBoundingClientRect();
      if (box.width > 0 && box.height > 0) contentBottom = Math.min(contentBottom, box.top - gap);
    }
  }
  const bottom = Math.min(contentBottom, top + height - padding);
  root.style.setProperty("--approval-available-height", `${Math.max(0, bottom - top - padding)}px`);
  root.style.setProperty("--approval-lift", `${Math.max(0, anchorBottom - bottom)}px`);
}
export function refitComposerMenus() {
  fitPopup();
  fitContextUsageTip();
  fitClaudeUsageTip();
  fitApprovalArea();
  fitGoalEditor();
  for (const id of ["model-settings", "skill-menu"]) {
    const menu = $(`#${id}`); if (menu && !menu.hidden) fitComposerMenu(menu);
  }
}
// 视口尺寸或滚动变化时重新适配输入区浮层。
export function installComposerMenuViewportListeners() {
  window.addEventListener("resize", refitComposerMenus);
  window.visualViewport?.addEventListener("resize", refitComposerMenus);
  window.visualViewport?.addEventListener("scroll", refitComposerMenus);
  document.addEventListener("scroll", event => {
    fitApprovalArea();
    fitContextUsageTip();
    fitClaudeUsageTip();
    if (activePopup && !activePopup.node.contains(event.target)) fitPopup();
    const menu = $("#model-settings");
    if (menu && !menu.hidden && !menu.contains(event.target)) fitComposerMenu(menu);
  }, true);
}

export function rememberNewChatModel(model) {
  rememberNewChatSettings({ model, reasoningEffort: currentEffort() });
}
