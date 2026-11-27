// 用户确认的实例设置：共享的显示开关与原生常规配置分开；模型、思考和扩展页面共用实例选择及请求隔离。
import { button, element } from "../../ui/dom.js";
import { openSettingsPrompt } from "../../ui/dialog.js";
import { icon } from "../../ui/icons.js";
import { configureModelMenuWidth } from "../../ui/menu-parts.js";
import { selectControl, setSelectOptions } from "../../ui/select.js";
import { activePopup, closePopup, openPopup, renderPopup, togglePopup } from "../../ui/popup.js";
import { connected, session } from "../../core/state.js";
import { readCache } from "../../core/stores.js";
import { loadModels, refreshSkillCatalog, updateComposer } from "../../core/actions.js";
import { api } from "../api.js";
import { privateInputScope } from "../session/capabilities.js";
import { activeInstanceId, availableSessionInstances, sessionInstanceEnabled, sessionInstances } from "../session/instances.js";
import { loadSessionInstances, setInstanceDisplay } from "../session/instance-catalog.js";
import { invalidateModelSettings, selectedEffort, setSelectedEffort } from "../composer/options-state.js";
import { effortLabels } from "../composer/prompt-editor.js";
import { sharedSettingsWritable, subscribeSharedSettings } from "./shared-state.js";
import {
  cachedInstanceSettings, instanceSettingsIdentity, invalidateInstanceSettings, readInstanceSettings, storeInstanceSettings
} from "./instance-settings-cache.js";
import { notice } from "../../ui/notice.js";

const sections = {
  general: "会话实例", models: "模型与思考", skills: "Skills", mcp: "MCP", hooks: "钩子"
};
// 用户要求后续可扩展设置；导航分组、名称和图标集中登记，新增页面不再复制整段侧栏模板。
export const settingsNavigation = [
  { id: "personal", label: "个人", items: [
    { id: "general", label: "常规", icon: "settings" },
    { id: "customization", label: "外观", icon: "sun" },
    { id: "shortcuts", label: "快捷键", icon: "keyboard" }
  ] },
  { id: "agents", label: "智能体", items: [
    // 用户要求模型、思考及压缩配置统一从“会话实例”进入，共用同一个实例选择和保存版本。
    { id: "types", label: "会话实例", icon: "allSessions" },
    // 用户要求该设置入口统一称“会话命名”，与页面标题、提示及产品规范保持一致。
    { id: "naming", label: "会话命名", icon: "edit" },
    { id: "permissions", label: "权限", icon: "shield" },
    { id: "instructions", label: "自定义指令", icon: "edit" }
  ] },
  { id: "extensions", label: "扩展", items: [
    { id: "skills", label: "Skills", icon: "puzzle" },
    { id: "mcp", label: "MCP", icon: "network" },
    { id: "hooks", label: "钩子", icon: "hook" }
  ] },
  { id: "tools", label: "设备工具", androidOnly: true, items: [
    { id: "notifications", label: "通知事件", icon: "globe" },
    { id: "diagnostics", label: "诊断日志", icon: "tools" }
  ] }
];
export const instanceSettingPages = [
  { id: "types", label: "会话实例", section: "general", description: "按实例管理显示模型、思考、缓存压缩和原生能力。" },
  { id: "skills", label: "Skills", section: "skills", description: "查看和管理所选实例的 Skills。" },
  { id: "mcp", label: "MCP", section: "mcp", description: "查看和管理所选实例的 MCP 服务。" },
  { id: "hooks", label: "钩子", section: "hooks", description: "查看和管理所选实例的原生钩子。" }
];
function brand(backend) {
  // 用户要求保留现有 Codex 结形、Claude 橙色星芒；实例选择器和页标题均直接复用现有 icon()。
  const node = icon(backend);
  node.classList.add("instance-settings-brand");
  node.dataset.backend = backend;
  return node;
}
function fieldCopy(label, description) {
  const copy = element("span", "appearance-field-copy");
  copy.append(element("span", "", label));
  if (description) copy.append(element("small", "", description));
  return copy;
}
function switchControl(label, value) {
  const control = element("input", "appearance-notification-switch");
  // 缺少显式值时使用原生默认；使用原生混合态及对应无障碍语义，不能读成“关闭”。
  control.type = "checkbox"; control.setAttribute("role", value == null ? "checkbox" : "switch");
  control.setAttribute("aria-label", value == null ? `${label}，使用原生默认` : label);
  control.checked = value === true; control.indeterminate = value == null;
  return control;
}
function section(parent, label, backend = null) {
  const node = element("section", "appearance-section");
  if (label) {
    const heading = element("h4", "instance-settings-section-heading");
    if (backend) heading.append(brand(backend));
    heading.append(document.createTextNode(label));
    node.append(heading);
  }
  const card = element("div", "appearance-card");
  node.append(card); parent.append(node);
  return card;
}
// 用户要求默认状态统一口径；实例原生配置的空值称“使用原生默认”，模型能力的默认值单独注明来源。
const inherit = { value: "", label: "使用原生默认" };
const booleanOptions = [inherit, { value: "true", label: "开启" }, { value: "false", label: "关闭" }];
const boolValue = value => value == null ? "" : String(value);
const apiValue = value => value === "" ? null : value === "true" ? true : value === "false" ? false : value;

export function mountInstanceSettingsPage({ page, section: name, getSelectedInstance = () => "",
  setSelectedInstance = () => {} }) {
  const scope = privateInputScope(session);
  let disposed = false, selected = "", record = null, detecting = false, loading = false, saving = false, needsRead = false;
  let savingHidden = false, pendingHidden = null, queuedHidden = null, recordIdentity = "", promptEditor = null;
  let generation = 0, message = "", externalRefresh = false, inventoryQuery = "";
  const inventoryControls = new Set();
  const alive = () => !disposed && page.isConnected && scope === privateInputScope(session);
  // 过期缓存后台核对时可继续编辑；已知跨端变化或服务、CLI 身份变化后，旧记录只供查看。
  // 拆分身份权限校验与缓存阻断校验：显示模型连续勾选保存期间，本次 save() 自身会先将缓存标为 blocked，
  // 仅放行本次保存导致的缓存失效，身份、权限、只读与外部刷新阻断仍严格生效。
  const identityWritable = () => alive() && connected && session && !session.readOnly && !record?.readOnly
    && !detecting && !needsRead && !!record
    && recordIdentity === instanceSettingsIdentity(selected, name)
    && !!currentInstance() && !currentInstance().error && record.backend === currentInstance().backend;
  const baseWritable = () => identityWritable() && !cachedInstanceSettings(selected, name)?.blocked;
  const writable = () => baseWritable() && !saving;
  const currentInstance = () => availableSessionInstances().find(instance => instance.id === selected);
  const promptReadable = () => alive() && connected && !!record
    && recordIdentity === instanceSettingsIdentity(selected, name);
  function retireRead() {
    generation++; loading = false;
    return generation;
  }
  const head = element("div", "appearance-card instance-settings-target");
  const targetRow = element("div", "appearance-field");
  const instances = selectControl(`settings-${name}-instance`,
    name === "general" ? "当前会话实例" : `${sections[name]}的会话实例`, [], "", "",
    option => [option?.icon ? brand(option.icon) : icon("allSessions"),
      element("span", "select-label", option?.label || "请选择会话实例"), icon("chevron")]);
  targetRow.append(element("span", "", "会话实例"), instances); head.append(targetRow);
  const display = switchControl("显示此实例", false);
  if (name === "general") {
    const row = element("label", "appearance-field");
    row.append(fieldCopy("显示此实例", "所有客户端共用，不影响运行中的任务。"), display);
    head.append(row);
  }
  const content = element("div", "instance-settings-content");
  const status = element("p", "appearance-hint instance-settings-status");
  status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const reload = button("重新读取", "重新读取当前实例设置", () => void refresh(true), "text-button");
  const actions = element("div", "appearance-action-buttons instance-settings-actions");
  actions.append(reload);
  // 用户要求设置页移除“重新检测实例”按钮；实例目录仍由进入设置及服务状态变化自动检测，当前配置保留“重新读取”。
  page.append(head, content, status, actions);
  // 用户要求所有会话实例统一提供「显示模型」且改用多选菜单，避免长开关列表占用页面；
  // 持久复用同一触发器并在连续勾选保存期间保持弹层不关闭，保留搜索词、焦点与滚动位置。
  const visibilityTrigger = button("", "显示模型", () => togglePopup(visibilityTrigger),
    "select-trigger instance-model-visibility-trigger");
  visibilityTrigger.id = `settings-${name}-hiddenModelIds`;
  visibilityTrigger.setAttribute("aria-haspopup", "menu");
  visibilityTrigger.setAttribute("aria-expanded", "false");
  visibilityTrigger.searchMode = "auto";
  configureModelMenuWidth(visibilityTrigger);
  visibilityTrigger.selectSearchLabel = "搜索模型";
  visibilityTrigger.selectSearchPlaceholder = "搜索模型";
  visibilityTrigger.popupEmptyText = "尚未取得当前实例的完整模型目录";
  visibilityTrigger.menuOptions = [];
  visibilityTrigger.onkeydown = event => {
    if (visibilityTrigger.disabled || event.isComposing) return;
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      event.preventDefault();
      openPopup(visibilityTrigger, { edge: event.key === "Home" ? "first" : event.key === "End" ? "last" : null, focusSearch: false });
    }
  };
  const controls = new Set(), promptControls = new Set();
  function sync() {
    instances.disabled = !alive() || detecting || saving || !availableSessionInstances().length;
    reload.disabled = !alive() || detecting || saving || loading || !session || !connected
      || !currentInstance() || !!currentInstance().error;
    display.checked = sessionInstanceEnabled(selected);
    display.indeterminate = !currentInstance();
    // 显示开关来自服务端共享偏好，不等待原生配置读取；实例检测或保存期间仍锁定目标。
    display.disabled = !alive() || detecting || saving || !currentInstance()
      || !connected || !sharedSettingsWritable(session);
    // 用户要求已有配置在后台核对时继续显示，不再反复进入整页读取状态；首次无缓存时才显示读取提示。
    status.textContent = detecting ? "正在检测会话实例…" : loading && !record ? "正在读取实例设置…" : saving ? "正在保存…" : message;
    const canWrite = writable();
    // 仅本次显示模型保存（savingHidden）飞行中忽略自身 invalidateInstanceSettings() 导致的 blocked，
    // 保留全部身份、授权、只读与错误状态校验；非本保存期或保存其他设置时仍与 canWrite 一致。
    const canWriteVisibility = savingHidden ? identityWritable() : canWrite;
    for (const control of controls) {
      control.disabled = (control === visibilityTrigger ? !canWriteVisibility : !canWrite)
        || control.settingsUnsupported === true;
    }
    if (activePopup?.trigger && (!activePopup.trigger.isConnected || activePopup.trigger.disabled)
        && (activePopup.trigger === visibilityTrigger || controls.has(activePopup.trigger)))
      closePopup();
    const canReadPrompt = promptReadable() && !detecting && !saving;
    for (const control of promptControls) control.disabled = !canReadPrompt;
    if (promptEditor) alive() ? promptEditor.sync() : promptEditor.close();
  }
  function addSwitch(card, key, label, description, value, unsupported = false) {
    const row = element("label", "appearance-field");
    const control = switchControl(label, value);
    control.id = `settings-${name}-${key}`; control.settingsUnsupported = unsupported;
    const inherited = value == null ? "当前使用原生默认设置。" : "";
    const copyText = [description, inherited].filter(Boolean).join(" ");
    row.append(fieldCopy(label, copyText), control);
    const owner = record, ownerId = selected;
    control.onchange = () => {
      if (record === owner && selected === ownerId && control.isConnected)
        void save({ [key]: control.checked });
    };
    controls.add(control); card.append(row);
  }
  function settingSelect(key, label, options, value, transform = apiValue, unsupported = false) {
    if (!options.some(option => option.value === value))
      options = [...options, { value, label: `${value}（当前配置）`, disabled: true }];
    const control = selectControl(`settings-${name}-${key}`, label, options, value);
    control.settingsUnsupported = unsupported;
    const owner = record, ownerId = selected;
    control.onchange = () => {
      if (record !== owner || selected !== ownerId || !control.isConnected) return;
      void save({ [key]: transform(control.value) });
    };
    controls.add(control);
    return control;
  }
  function addSelect(card, key, label, description, options, value, transform = apiValue, unsupported = false) {
    const row = element("div", "appearance-field");
    row.append(fieldCopy(label, description), settingSelect(key, label, options, value, transform, unsupported));
    card.append(row);
  }
  function renderGeneral() {
    const backend = record.backend, values = record.settings;
    if (backend === "antigravity") {
      // 用户要求移除 Antigravity 实例页的只读常规说明卡片（执行协议/默认权限/全局自定义指令），保留目录异常与底部提示。
      if (record.capabilityError)
        content.append(element("p", "appearance-hint error-text", `无法读取当前 Antigravity 实例模型目录：${record.capabilityError}`));
      content.append(element("p", "appearance-hint", "项目设置、启动参数和组织策略仍按原生优先级生效。"));
      return;
    }
    const label = backend === "claude" ? "Claude" : "Codex";
    const card = section(content, `${label} 常规设置`, backend);
    if (backend === "codex") {
      // 用户要求精简设置项描述：标题与控件已自解释的开关不再重复介绍，仅保留影响范围、依赖或副作用说明。
      for (const [key, label, description] of [
        ["todo", "任务清单（Todo）", ""],
        ["memories", "记忆", ""],
        ["multiAgent", "子智能体", ""],
        ["goals", "目标模式", ""]
      ]) addSwitch(card, key, label, description, values[key]);
      content.append(element("p", "appearance-hint",
        "仅修改所选实例的原生配置，新会话或重新加载后生效。"));
      // 用户要求按 Codex 实例提供默认关闭的持久侧边开关；旧服务缺字段时禁用，不能假装已支持。
      const sides = section(content, "侧边会话");
      addSwitch(sides, "persistentSideThreads", "持久侧边会话",
        "仅影响新建侧边，关闭标签或重启后保留历史。",
        values.persistentSideThreads === true, typeof values.persistentSideThreads !== "boolean");
    } else {
      addSwitch(card, "awaySummaryEnabled", "离开期间回顾",
        "关闭后保留已有回顾。", values.awaySummaryEnabled);
      addSelect(card, "promptSuggestionEnabled", "输入建议", "",
        booleanOptions, boolValue(values.promptSuggestionEnabled), apiValue,
        record.capabilities?.promptSuggestions !== true);
      content.append(element("p", "appearance-hint",
        "回顾和输入建议保存在所选实例的原生配置，新会话生效。"));
      if (record.capabilities?.promptSuggestions !== true) {
        content.append(element("p", "appearance-hint",
          "该实例的 Claude CLI 尚未确认支持输入建议，对应控件暂不可用。"));
      }
      // 兼容没有额外设置返回值的旧服务；新服务在下方统一展示实际失败原因，避免漏报 doctor 校验失败。
      if (record.capabilityError && !record.extraSettings?.length)
        content.append(element("p", "appearance-hint", `无法核对当前 Claude CLI 能力：${record.capabilityError}`));
    }
    content.append(element("p", "appearance-hint", "项目设置、启动参数和组织策略仍按原生优先级生效。"));
  }
  // 用户要求移除设置页「实例默认值」：新会话模型与思考深度由新建页输入区按实例自动记住并跨端共享，未记忆时沿用引擎原生默认。
  function renderCodexReasoning() {
    if (record.backend !== "codex") return;
    const reasoning = section(content, "思考深度");
    addSwitch(reasoning, "ultraReasoningEnabled", "开放 Ultra 思考深度",
      "仅支持的模型可见。", record.settings.ultraReasoningEnabled);
  }
  function currentHiddenModelIds() {
    return pendingHidden ?? (Array.isArray(record?.settings?.hiddenModelIds) ? record.settings.hiddenModelIds : []);
  }
  function syncModelVisibilityTrigger() {
    const owner = record, ownerId = selected, ownerIdentity = recordIdentity;
    if (!owner || !Array.isArray(owner.settings?.hiddenModelIds)) {
      visibilityTrigger.menuOptions = [];
      return;
    }
    const hidden = currentHiddenModelIds(), hiddenSet = new Set(hidden), models = owner.models || [];
    const hiddenInCatalog = models.filter(model => hiddenSet.has(model.model)).length;
    const visibleCount = models.length - hiddenInCatalog;
    const summary = !models.length
      ? (hidden.length ? `已隐藏 ${hidden.length} 个模型` : "暂无可选模型")
      : !hiddenInCatalog
        ? `全部显示（${models.length}）`
        : visibleCount === 0
          ? `已全部隐藏（0/${models.length}）`
          : `已显示 ${visibleCount}/${models.length} 个模型`;
    visibilityTrigger.setAttribute("aria-description", `当前状态：${summary}`);
    visibilityTrigger.title = `显示模型：${summary}`;
    visibilityTrigger.replaceChildren(element("span", "select-label", summary), icon("chevron"));
    visibilityTrigger.popupEmptyText = owner.capabilityError
      ? `未能返回模型列表（${owner.capabilityError}）`
      : "尚未取得当前实例的完整模型目录";
    // 无目录且无已隐藏项时禁用触发器并在下方说明原因；存在历史隐藏项时仍允许打开菜单执行“全部显示”。
    visibilityTrigger.settingsUnsupported = !models.length && !hidden.length;
    const applyHidden = next => {
      if (selected !== ownerId || recordIdentity !== ownerIdentity || !visibilityTrigger.isConnected) return;
      if (savingHidden) {
        if (!identityWritable()) { sync(); return; }
        pendingHidden = next;
        queuedHidden = next;
        syncModelVisibilityTrigger();
        if (activePopup?.trigger === visibilityTrigger) renderPopup();
        return;
      }
      if (!writable()) return;
      pendingHidden = next;
      queuedHidden = null;
      savingHidden = true;
      syncModelVisibilityTrigger();
      if (activePopup?.trigger === visibilityTrigger) renderPopup();
      void save({ hiddenModelIds: next });
    };
    // 完整原生目录仍保留在多选菜单中，隐藏名单只按当前实例保存，不能变成全局模型白名单或修改原生配置文件。
    visibilityTrigger.menuOptions = [
      ...models.map(model => {
        const label = model.displayName || model.model;
        return {
          value: model.model,
          label,
          description: label === model.model ? "" : model.model,
          searchText: model.model,
          checkable: true,
          selected: !hiddenSet.has(model.model),
          keepOpen: true,
          action: () => {
            const next = new Set(currentHiddenModelIds());
            if (next.has(model.model)) next.delete(model.model);
            else next.add(model.model);
            applyHidden([...next]);
          }
        };
      }),
      {
        value: "__show_all__",
        label: "全部显示",
        footer: true,
        separatorBefore: true,
        keepOpen: true,
        disabled: !hidden.length,
        action: () => {
          if (currentHiddenModelIds().length) applyHidden([]);
        }
      }
    ];
  }
  function renderModelVisibility() {
    // 用户要求所有会话实例统一提供「显示模型」且使用多选菜单，避免长列表占满页面；
    // 手机端（<=760px）与数值/手动压缩模型行一致改为纵向排列，防止 240px 触发器将左侧文案挤成单字竖排。
    if (!Array.isArray(record.settings.hiddenModelIds)) return;
    const models = record.models || [];
    const card = section(content, "显示模型");
    syncModelVisibilityTrigger();
    const row = element("div", "appearance-field instance-model-visibility-row");
    row.append(fieldCopy("可选模型", "仅隐藏选择列表，已有会话保留当前模型。"), visibilityTrigger);
    controls.add(visibilityTrigger);
    card.append(row);
    if (!models.length) content.append(element("p", "appearance-hint",
      record.capabilityError
        ? `当前实例未能返回模型列表（${record.capabilityError}），请确认登录状态后点击重新读取。`
        : "尚未取得当前实例的完整模型目录；可以先在菜单中恢复全部显示，再重新读取。"));
  }
  function renderInventory() {
    const inventory = record.inventory || {};
    // Antigravity 的 hooks.json 按具名钩子单独声明 enabled，无全局总开关。
    if (name === "hooks" && record.backend !== "antigravity") {
      const card = section(content, "钩子设置");
      addSwitch(card, "enabled", "开启钩子", "按所选实例的原生钩子配置执行。", inventory.enabled);
    }
    if (inventory.message) content.append(element("p", "appearance-hint", inventory.message));
    const rows = Array.isArray(inventory.data) ? inventory.data : [];
    if (name === "skills" || name === "mcp") {
      // 用户要求参考 Codex App：Skills 和 MCP 列表提供本地搜索，按已读取的目录字段筛选，
      // 不改变后端目录、实例配置或开关状态；搜索框统一 240px，清空后保留焦点。
      const label = name === "mcp" ? "MCP 服务" : "Skill";
      const searchControl = element("div", "instance-settings-search-control");
      const search = element("input", "instance-settings-search");
      search.type = "search"; search.placeholder = `搜索${label}`; search.autocomplete = "off";
      search.setAttribute("aria-label", `搜索${label}`); search.value = inventoryQuery;
      const clear = button("", `清除${label}搜索`, () => {
        inventoryQuery = ""; search.value = ""; renderRows();
        search.focus({ preventScroll: true });
      }, "icon-button instance-settings-search-clear");
      clear.append(icon("close")); clear.hidden = search.value.length === 0;
      searchControl.append(search, clear); content.append(searchControl);
      // 用户要求 Skills 与 MCP 搜索框上下间距统一；两页共用无重复分组标题的列表布局。
      const card = section(content, ["skills", "mcp"].includes(name) ? "" : `所选实例的 ${sections[name]}`);
      const list = element("div", "instance-settings-inventory-list");
      card.append(list);
      const renderRows = () => {
        for (const control of inventoryControls) controls.delete(control);
        inventoryControls.clear(); list.replaceChildren();
        const query = inventoryQuery.trim().toLocaleLowerCase();
        const filtered = rows.filter(value => !query || [
          value.name, value.id, value.path, value.transport, value.description
        ].filter(Boolean).join(" ").toLocaleLowerCase().includes(query));
        if (!filtered.length) {
          list.append(element("p", "appearance-hint instance-settings-inventory-empty",
            rows.length ? "没有匹配的结果。" : inventory.error || `所选实例尚未返回${sections[name]}。`));
          return;
        }
        appendInventoryRows(list, filtered);
      };
      const updateClear = () => { clear.hidden = search.value.length === 0; };
      search.oninput = () => { inventoryQuery = search.value; updateClear(); renderRows(); };
      renderRows();
      return;
    }
    if (!rows.length) {
      content.append(element("p", "appearance-hint", inventory.error || `所选实例尚未返回${sections[name]}。`));
      return;
    }
    const card = section(content, ["skills", "mcp"].includes(name) ? "" : `所选实例的 ${sections[name]}`);
    appendInventoryRows(card, rows);
  }
  function appendInventoryRows(card, rows) {
    for (const value of rows) {
      const row = element("div", "appearance-field");
      const codexMcp = name === "mcp" && record.backend === "codex";
      const inherited = value.enabled == null && (value.canToggle === true || Object.hasOwn(value, "enabled"));
      // 用户要求 Codex MCP 保留原生默认值语义；默认值只决定开关初始位置，不额外展示“已开启/已关闭”文字。
      const enabled = codexMcp && inherited ? value.defaultEnabled : value.enabled;
      // 用户要求 MCP 列表右侧只保留开关，不重复显示“已开启/已关闭”状态文案。
      const description = value.description || value.transport || "";
      row.append(fieldCopy(value.name || value.id || value.path, description));
      if (value.canToggle === true) {
        const control = switchControl(value.name || value.id, enabled);
        const owner = record, ownerId = selected;
        control.onchange = () => {
          if (record === owner && selected === ownerId && control.isConnected) {
            const actionKey = name === "skills" ? "skill" : name === "hooks" ? "hook" : "server";
            void save({ [actionKey]: { id: value.id, enabled: control.checked } });
          }
        };
        controls.add(control); inventoryControls.add(control);
        row.append(control);
      } else if (value.readOnlyReason) {
        row.append(element("span", "muted", value.readOnlyReason));
      } else if (typeof value.enabled === "boolean" && name !== "mcp") {
        row.append(element("span", "muted", value.enabled ? "已开启" : "已关闭"));
      } else if (inherited) {
        row.append(element("span", "muted", "使用原生默认"));
      }
      card.append(row);
    }
  }
  function addPromptSetting(card, option, value) {
    const row = element("div", "appearance-field");
    const owner = record, ownerId = selected;
    const control = button("设置", `设置${option.label}`, () => {
      if (control.disabled || !control.isConnected || record !== owner || selected !== ownerId || promptEditor) return;
      const identity = recordIdentity;
      const expected = { revision: record.revision, preferenceRevision: record.preferenceRevision };
      const getBlock = () => {
        if (!promptReadable() || selected !== ownerId || recordIdentity !== identity)
          return "实例、服务或授权已变化，请重新打开设置。";
        if (record.revision !== expected.revision || record.preferenceRevision !== expected.preferenceRevision
            || record.settings[option.id] !== value)
          return "实例设置已更新，请关闭后重新打开核对。";
        return writable() ? "" : "当前无法修改，请核对连接和写入权限，必要时重新读取。";
      };
      // 用户要求按实例在弹窗设置压缩提示词。恢复默认待保存后才移除覆盖，写入使用打开时的两个版本。
      const defaultValue = option.defaultValue || (owner.backend === "claude" ? null : undefined);
      promptEditor = openSettingsPrompt({ title: option.label, value, defaultValue, max: option.max, getBlock,
        returnFocus: () => selected === ownerId ? [...promptControls].find(item => item.id === control.id) : null,
        onClose() { promptEditor = null; },
        async onSave(value) {
          const blocked = getBlock();
          if (blocked) throw new Error(blocked);
          const error = await save({ [option.id]: value }, expected);
          if (error) throw new Error(error);
        }
      });
    });
    control.id = `settings-${name}-${option.id}-prompt`;
    control.setAttribute("aria-haspopup", "dialog");
    row.append(fieldCopy(option.label, option.description), control);
    promptControls.add(control); card.append(row);
  }
  function renderExtraSettings() {
    const options = arguments[0] || {};
    const allRows = record.extraSettings || [];
    if (!allRows.length) return;
    const rows = allRows.filter(option =>
      (options.onlyGroup ? option.group === options.onlyGroup : true)
      && (options.excludeGroup ? option.group !== options.excludeGroup : true));
    if (options.showCapabilityError !== false && record.capabilityError)
      content.append(element("p", "appearance-hint", `部分原生设置能力核对失败：${record.capabilityError}`));
    if (!rows.length) return;
    const groups = new Map();
    for (const option of rows) {
      if (!groups.has(option.group)) groups.set(option.group, section(content, option.group));
      const card = groups.get(option.group), value = record.settings[option.id];
      // 用户要求额外配置也按真实实例保存；失效、能力未确认或只读诊断值只展示，不伪装成可用开关。
      if (option.readOnly) {
        const row = element("div", "appearance-field");
        const label = option.options?.find(choice => choice.value === value)?.label;
        const configured = label || (option.id.startsWith("nativeModelEffort:") ? effortLabels[value] : "") || String(value);
        // 诊断项只说明文件中的值；不能把已移除或未确认的原生能力显示成“已开启”。
        // 用户要求所有实例均展示压缩提示词，不支持时右侧改为纯文字“不支持”，不渲染设置按钮。
        const text = option.id === "compactionModel"
          ? (option.enabledKey && record.settings[option.enabledKey] === false ? "已关闭" : configured)
          : option.cliPreview ? option.cliPreview
          : option.id === "compactionPrompt" ? "不支持"
          : option.valueInvalid ? "配置值无效" : value == null ? "使用原生默认" : `配置值：${configured}`;
        row.append(fieldCopy(option.label, option.readOnlyReason || option.description),
          element("span", "muted", text));
        card.append(row); continue;
      }
      if (option.kind === "boolean") {
        addSelect(card, option.id, option.label, option.description, booleanOptions, boolValue(value));
      } else if (option.kind === "enum" && option.modeKey && Array.isArray(option.modeOptions)) {
        // 用户要求「手动压缩模型」增加独立开关（默认关闭，避免默认就生效）：
        // 关闭时禁用模型与策略选择并保留已选配置；开启后右侧按 [模型 Select] [策略菜单按钮] [开关] 排列（手机端开关置于首行右侧）。
        const row = element("div", "appearance-field instance-setting-compaction-model-row");
        const enabledKey = option.enabledKey || "";
        const enabled = enabledKey ? record.settings[enabledKey] === true : true;
        const modeKey = option.modeKey;
        const modeValue = record.settings[modeKey] ?? option.modeValue ?? option.modeOptions[0]?.value ?? "";
        const currentMode = option.modeOptions.find(choice => choice.value === modeValue) || option.modeOptions[0];
        const owner = record, ownerId = selected;
        let toggle = null;
        if (enabledKey) {
          toggle = switchControl(`启用${option.label}`, enabled);
          toggle.id = `settings-${name}-${enabledKey}`;
          toggle.classList.add("instance-setting-compaction-model-switch");
          toggle.onchange = () => {
            if (record === owner && selected === ownerId && toggle.isConnected)
              void save({ [enabledKey]: toggle.checked });
          };
          controls.add(toggle);
        }
        const modeTrigger = button("", `${option.label}使用策略`, () => togglePopup(modeTrigger),
          "select-trigger instance-setting-mode-trigger");
        modeTrigger.id = `settings-${name}-${modeKey}`;
        modeTrigger.settingsUnsupported = !enabled;
        modeTrigger.setAttribute("aria-haspopup", "menu");
        modeTrigger.setAttribute("aria-expanded", "false");
        modeTrigger.setAttribute("aria-description", `当前策略：${currentMode?.label || ""}`);
        modeTrigger.title = `${option.label}策略：${currentMode?.label || ""}${currentMode?.description ? ` · ${currentMode.description}` : ""}`;
        modeTrigger.replaceChildren(element("span", "select-label", currentMode?.label || "选择策略"), icon("chevron"));
        modeTrigger.menuOptions = option.modeOptions.map(choice => ({
          value: choice.value,
          label: choice.label,
          description: choice.description || "",
          checkable: true,
          selected: choice.value === modeValue,
          action: () => {
            if (record !== owner || selected !== ownerId || !modeTrigger.isConnected) return;
            if (choice.value !== modeValue) void save({ [modeKey]: choice.value });
          }
        }));
        modeTrigger.onkeydown = event => {
          if (modeTrigger.disabled || event.isComposing) return;
          if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
            event.preventDefault();
            openPopup(modeTrigger, { edge: event.key === "Home" ? "first" : event.key === "End" ? "last" : null, focusSearch: false });
          }
        };
        const modelOptions = option.options.length ? option.options : [{ value: "session", label: "跟随会话模型" }];
        const modelValue = value || "session";
        // 选择「跟随会话模型」提交 "session"，与服务端返回的 settings.compactionModel 一致（服务端落盘时仍移除偏好键，不改动策略）。
        const modelSelect = settingSelect(
          option.id,
          option.label,
          modelOptions,
          modelValue,
          next => next,
          !enabled
        );
        modelSelect.classList.add("instance-setting-compaction-model-select");
        modelSelect.searchMode = "auto";
        configureModelMenuWidth(modelSelect);
        modelSelect.selectSearchLabel = "搜索手动压缩模型";
        modelSelect.selectSearchPlaceholder = "搜索手动压缩模型";
        controls.add(modeTrigger);
        const box = element("span", "instance-setting-compaction-model-control");
        box.append(modelSelect, modeTrigger);
        if (toggle) row.append(fieldCopy(option.label, option.description), box, toggle);
        else row.append(fieldCopy(option.label, option.description), box);
        card.append(row);
      } else if (option.kind === "enum") {
        addSelect(card, option.id, option.label, option.description,
          [inherit, ...option.options], value || "", value => value || null);
      } else if (option.kind === "text") {
        addPromptSetting(card, option, value);
      } else if (option.kind === "integer") {
        // 用户要求自动压缩阈值三端统一用百分比输入（50～98%），右侧紧邻输入框展示对应数值与未设置默认值。
        const unit = option.unit || "";
        const hasMeta = Boolean(option.cliPreview || unit);
        const row = element("label", hasMeta ? "appearance-field instance-setting-number-row" : "appearance-field");
        const input = element("input");
        input.type = "number"; input.step = "1";
        input.id = `settings-${name}-${option.id}`; input.setAttribute("aria-label", option.label);
        input.min = String(option.min); input.max = String(option.max);
        input.placeholder = option.defaultLabel || "使用原生默认"; input.value = value == null ? "" : String(value);
        const owner = record, ownerId = selected;
        input.onchange = () => {
          // 数字控件失焦可能晚于切换实例或重新读取；旧控件不能借用新目标及新版本保存。
          if (record !== owner || selected !== ownerId || !input.isConnected) return;
          const raw = input.value.trim(), next = raw === "" ? null : Number(raw);
          if (input.validity?.badInput || raw !== "" && (!Number.isSafeInteger(next) || next < option.min || next > option.max)) {
            input.value = value == null ? "" : String(value);
            input.setAttribute("aria-invalid", "true");
            const unitSuffix = unit ? (unit === "%" ? unit : ` ${unit}`) : "";
            message = `${option.label}需要填写 ${option.min}～${option.max}${unitSuffix} 的整数，或留空使用原生默认。`;
            sync(); return;
          }
          if (next !== value) void save({ [option.id]: next });
        };
        input.oninput = () => input.removeAttribute("aria-invalid");
        input.onkeydown = event => { if (event.key === "Enter") { event.preventDefault(); input.blur(); } };
        // 与现有数字设置共用容器；带对应 CLI 数值预览或单位时在右侧并排展示，窄屏自动折行避免挤占说明列。
        const box = element("span", option.cliPreview || unit
          ? "appearance-number instance-setting-number-control"
          : "appearance-number");
        if (option.cliPreview) {
          const preview = element("span", "instance-setting-cli-value", option.cliPreview);
          preview.title = option.cliPreview;
          box.append(preview);
        }
        if (unit) {
          // 大整数单位（如 TK）保持 100px/82px 宽度，百分比沿用 76px/68px 紧凑宽度。
          const wrap = element("span", option.max > 999
            ? "instance-setting-unit-input is-wide"
            : "instance-setting-unit-input");
          wrap.append(input, element("span", "instance-setting-unit", unit));
          box.append(wrap);
        } else {
          box.append(input);
        }
        row.append(fieldCopy(option.label, option.description), box);
        controls.add(input); card.append(row);
      }
    }
  }
  function render() {
    content.replaceChildren(); controls.clear(); inventoryControls.clear(); promptControls.clear();
    if (record) {
      if (name === "general") {
        // 合并页面只读取当前实例的一份设置；旧服务未返回模型目录时仍可显示已有常规配置。
        // 用户要求按「显示模型 → 缓存与压缩」排列，其余扩展分组继续留在常规设置下方。
        renderModelVisibility();
        renderExtraSettings({ onlyGroup: "缓存与压缩", showCapabilityError: false });
        if (Array.isArray(record.models)) renderCodexReasoning();
        renderGeneral();
      }
      else if (name === "models") {
        renderModelVisibility();
        renderCodexReasoning();
      }
      else renderInventory();
      renderExtraSettings(name === "general" ? { excludeGroup: "缓存与压缩" } : {});
      if (record.readOnly || session?.readOnly) message ||= record.readOnlyReason || "当前为只读访问";
    }
    sync();
    if (activePopup?.trigger === visibilityTrigger) {
      if (visibilityTrigger.isConnected && !visibilityTrigger.disabled) renderPopup();
      else closePopup();
    }
  }
  function checkedResult(result, id) {
    if (!result || result.ok === false || result.instanceId !== id || result.backend !== currentInstance()?.backend
        || typeof result.readOnly !== "boolean"
        || typeof result.revision !== "string" || typeof result.preferenceRevision !== "string"
        || !result.settings || Array.isArray(result.settings) || typeof result.settings !== "object")
      throw new Error("实例设置返回结果无法确认，请重新读取。");
    const values = result.settings;
    if (values.hiddenModelIds != null
        && (!Array.isArray(values.hiddenModelIds) || values.hiddenModelIds.length > 512
          || new Set(values.hiddenModelIds).size !== values.hiddenModelIds.length
          || values.hiddenModelIds.some(id => typeof id !== "string" || !id.length || id.length > 200
            || /[\u0000-\u001f\u007f]/u.test(id))))
      throw new Error("实例模型显示设置返回格式无效，请重新读取。");
    if (result.extraSettings != null && (!Array.isArray(result.extraSettings)
        || result.extraSettings.length > 100 || result.extraSettings.some(option =>
          !option || typeof option.id !== "string" || typeof option.label !== "string"
          || typeof option.group !== "string" || typeof option.description !== "string"
          || typeof option.readOnly !== "boolean" || !Object.hasOwn(values, option.id)
          || option.valueInvalid != null && typeof option.valueInvalid !== "boolean"
          || option.unit != null && typeof option.unit !== "string"
          || option.cliPreview != null && typeof option.cliPreview !== "string"
          || option.defaultLabel != null && typeof option.defaultLabel !== "string"
          || option.enabledKey != null && (typeof option.enabledKey !== "string" || !option.enabledKey
            || !Object.hasOwn(values, option.enabledKey) || typeof values[option.enabledKey] !== "boolean")
          || option.modeKey != null && (typeof option.modeKey !== "string" || !option.modeKey
            || !Object.hasOwn(values, option.modeKey)
            || !Array.isArray(option.modeOptions) || !option.modeOptions.length
            || option.modeOptions.some(choice => !choice || typeof choice.value !== "string" || typeof choice.label !== "string")
            || !option.modeOptions.some(choice => choice.value === values[option.modeKey]))
          || !["boolean", "integer", "integerString", "enum", "text"].includes(option.kind)
          || !Array.isArray(option.options) || option.options.some(choice =>
            !choice || typeof choice.value !== "string" || typeof choice.label !== "string")
          || option.readOnly && typeof option.readOnlyReason !== "string"
          || !option.readOnly && (option.kind === "boolean"
            ? values[option.id] !== null && typeof values[option.id] !== "boolean"
            : option.kind === "integer"
              ? !Number.isSafeInteger(option.min) || !Number.isSafeInteger(option.max)
                || option.min < 1 || option.max < option.min
                || values[option.id] !== null && (!Number.isSafeInteger(values[option.id])
                  || values[option.id] < option.min || values[option.id] > option.max)
              : option.kind === "enum"
                ? values[option.id] !== null && !option.options.some(choice => choice.value === values[option.id])
                : option.kind === "text"
                  ? !Number.isSafeInteger(option.max) || option.max < 1 || option.max > 20000
                    || values[option.id] !== null && typeof values[option.id] !== "string"
                    || option.defaultValue != null && typeof option.defaultValue !== "string"
                : true))))
      throw new Error("实例额外设置字段无法确认，请重新读取。");
    if (name === "general") {
      const keys = result.backend === "codex" ? ["todo", "memories", "multiAgent", "goals"]
        : result.backend === "claude" ? ["awaySummaryEnabled", "promptSuggestionEnabled"] : [];
      if (keys.some(key => !Object.hasOwn(values, key) || values[key] !== null && typeof values[key] !== "boolean"))
        throw new Error("实例常规设置字段不完整，请重新读取。");
      if (result.backend === "codex" && Object.hasOwn(values, "persistentSideThreads")
          && typeof values.persistentSideThreads !== "boolean")
        throw new Error("持久侧边开关字段无效，请重新读取。");
    }
    if (name === "models" || name === "general" && result.models != null) {
      if (!Array.isArray(result.models) || result.models.some(model => !model || typeof model.model !== "string"
            || model.hiddenByInstance != null && typeof model.hiddenByInstance !== "boolean"
            || model.displayName != null && typeof model.displayName !== "string"
            || model.supportedReasoningEfforts != null && (!Array.isArray(model.supportedReasoningEfforts)
              || model.supportedReasoningEfforts.some(effort => !effort || typeof effort.reasoningEffort !== "string")))
          || result.backend === "codex" && typeof values.ultraReasoningEnabled !== "boolean")
        throw new Error("实例模型设置字段不完整，请重新读取。");
    } else if (name !== "general" && (!result.inventory || !Array.isArray(result.inventory.data)
        || result.inventory.data.some(row => !row || typeof row !== "object"
          || typeof (row.name || row.id || row.path) !== "string"
          || row.canToggle === true && typeof row.id !== "string"
          || row.enabled != null && typeof row.enabled !== "boolean"
          || row.defaultEnabled != null && typeof row.defaultEnabled !== "boolean")
        || name === "hooks" && (!Object.hasOwn(result.inventory, "enabled")
          || result.inventory.enabled !== null && typeof result.inventory.enabled !== "boolean"))) {
      throw new Error("实例扩展列表字段不完整，请重新读取。");
    }
    return result;
  }
  async function read() {
    return refresh();
  }
  async function refresh(force = false) {
    if (!alive() || saving || detecting) return;
    const id = selected, version = retireRead(), previous = record;
    needsRead = false;
    if (!id || !currentInstance() || currentInstance().error) {
      record = null;
      message = "所选实例暂不可用，请等待实例目录更新或明确选择其他实例。"; render(); return;
    }
    if (!session || !connected) {
      record = null;
      message = "连接远程服务后可读取实例设置。"; render(); return;
    }
    const cached = cachedInstanceSettings(id, name);
    try { record = cached ? checkedResult(cached.value, id) : null; }
    catch { invalidateInstanceSettings(id); record = null; }
    recordIdentity = instanceSettingsIdentity(id, name);
    needsRead = force || cached?.blocked === true;
    if (!force && cached?.fresh && record) {
      if (record !== previous) render(); else sync();
      return;
    }
    // 缓存显示与请求生命周期分开；切页或关窗只撤销页面接收，其他入口可继续复用同一在途读取。
    loading = true; message = "";
    if (record !== previous) render(); else sync();
    let changed = false;
    try {
      const result = await readInstanceSettings(id, name, { force, background: !!record
        || page.closest?.(".appearance-settings-page")?.hidden === true });
      if (!alive() || selected !== id || generation !== version) return;
      changed = record !== result;
      record = checkedResult(result, id);
      recordIdentity = instanceSettingsIdentity(id, name);
      needsRead = false;
    } catch (error) {
      if (alive() && selected === id && generation === version && error.name !== "AbortError") {
        needsRead = true;
        message = record ? `后台核对失败，保留上次设置；重新读取后可继续修改。${error.message}` : error.message;
      }
    } finally {
      if (alive() && selected === id && generation === version) {
        loading = false;
        if (changed) render(); else sync();
        if (externalRefresh) { externalRefresh = false; void read(); }
      }
    }
  }
  async function save(settings, expected = record) {
    const isHiddenOnly = Object.keys(settings).length === 1 && Object.hasOwn(settings, "hiddenModelIds");
    if (!writable()) {
      if (isHiddenOnly) { savingHidden = false; pendingHidden = null; queuedHidden = null; }
      render();
      return "当前无法保存，请重新读取核对。";
    }
    const id = selected, before = record, identity = instanceSettingsIdentity(selected, name);
    // 保存先撤销同一实例旧读取；旧快照不能在成功回执之后回填，也不能带着旧版本继续写入其他分类。
    retireRead(); invalidateInstanceSettings(id);
    saving = true; message = "";
    if (isHiddenOnly) {
      savingHidden = true;
      sync();
    } else {
      render();
    }
    let keepHiddenSaving = false;
    try {
      const result = await api("/api/settings/instance", { instanceId: id, section: name,
        operation: "save", settings, expectedRevision: expected.revision,
        expectedPreferenceRevision: expected.preferenceRevision });
      if (scope !== privateInputScope(session) || selected !== id) return "保存结果不属于当前实例或授权。";
      if (identity !== instanceSettingsIdentity(id, name))
        throw new Error("实例或服务已变化，请重新读取核对保存结果。");
      record = checkedResult(result, id);
      for (const [key, value] of Object.entries(settings)) {
        const actual = key === "enabled" ? record.inventory?.enabled
          : ["skill", "server", "hook"].includes(key) ? record.inventory?.data?.find(row => row.id === value.id)?.enabled
            : record.settings[key];
        if (key === "hiddenModelIds" ? JSON.stringify(actual) !== JSON.stringify(value)
          : actual !== (["skill", "server", "hook"].includes(key) ? value.enabled : value))
          throw new Error("保存结果与所选设置不一致，请重新读取核对。");
      }
      record = storeInstanceSettings(id, name, record);
      if (cachedInstanceSettings(id, name)?.blocked) {
        needsRead = true; externalRefresh = true;
      }
      if (name === "models" || name === "general"
          && ["ultraReasoningEnabled", "defaultServiceTier", "hiddenModelIds"]
            .some(key => Object.hasOwn(settings, key))) {
        // 已确认的配置变更即使设置窗口已关闭也须失效缓存；其他实例的旧磁盘目录同样不能在下次启动恢复。
        const removed = readCache.remove(scope, "models", id === "codex:local" ? "global" : id).catch(() => {});
        if (id === activeInstanceId()) {
          // 关闭 Ultra 后清理尚未提交的 Ultra 选择，避免下一次发送继续携带禁用等级；已有原生会话深度保持原值。
          if (record.backend === "codex" && record.settings.ultraReasoningEnabled === false && selectedEffort === "ultra")
            setSelectedEffort("");
          invalidateModelSettings(); updateComposer();
          void removed.then(() => {
            if (scope === privateInputScope(session) && id === activeInstanceId())
              return loadModels(!Object.hasOwn(settings, "hiddenModelIds"));
          }).catch(() => {});
        }
      } else if (name === "skills") {
        // 用户修改原生 Skill 后及时撤销旧补全缓存；只为当前实例主动重读，不借用其他会话的环境探测。
        void refreshSkillCatalog({ reload: id === activeInstanceId() }).catch(() => {});
      }
      if (isHiddenOnly && queuedHidden !== null && alive() && selected === id && !needsRead
          && identity === instanceSettingsIdentity(id, name)) {
        const followUp = queuedHidden;
        queuedHidden = null;
        if (JSON.stringify(followUp) !== JSON.stringify(record.settings.hiddenModelIds)) {
          keepHiddenSaving = true;
          saving = false;
          syncModelVisibilityTrigger();
          return await save({ hiddenModelIds: followUp }, record);
        }
      }
      if (alive()) message = Object.hasOwn(settings, "hiddenModelIds")
        ? "模型显示设置已保存，已有会话继续使用当前模型。"
        : Object.hasOwn(settings, "compactionModelEnabled")
            || Object.hasOwn(settings, "compactionModel")
            || Object.hasOwn(settings, "compactionModelMode")
          ? "已保存，下次手动压缩时生效。"
          : "已保存，新会话或重新加载后生效。";
      return "";
    } catch (error) {
      queuedHidden = null;
      const failure = `${error.message} 未自动重复保存，请重新读取核对。`;
      if (alive() && selected === id) {
        record = before; needsRead = true;
        message = failure;
      }
      return failure;
    } finally {
      if (!keepHiddenSaving) {
        if (isHiddenOnly) { savingHidden = false; pendingHidden = null; queuedHidden = null; }
        if (alive() && selected === id) {
          saving = false; render();
          if (externalRefresh) { externalRefresh = false; void read(); }
        }
      }
    }
  }
  async function show(force = false) {
    if (!alive() || saving) return;
    if (activePopup?.trigger && (activePopup.trigger === visibilityTrigger || controls.has(activePopup.trigger))) closePopup();
    pendingHidden = null; queuedHidden = null;
    // 重新检测开始即撤销旧读取和写入口；迟到的旧回包不能盖掉“实例不可用”，也不能恢复旧控件。
    const version = retireRead();
    // 已检测的目录可直接准备目标；重新检测才等待环境扫描，避免每次切回设置都重复启动检测。
    detecting = force || !sessionInstances.length; message = ""; sync();
    if (!session || !connected) {
      detecting = false; record = null; message = "连接远程服务后可读取实例设置。"; render(); return;
    }
    try {
      if (detecting) {
        if (force) invalidateInstanceSettings(selected || getSelectedInstance() || activeInstanceId());
        await loadSessionInstances(force);
      }
      if (!alive() || generation !== version) return;
      const choices = availableSessionInstances().map(instance =>
        ({ value: instance.id, label: instance.label, icon: instance.backend, disabled: !!instance.error }));
      // 用户要求按真实实例配置；页面间保留同一选择，实例消失时保留目标，不能悄悄改写其他实例。
      const preferred = getSelectedInstance() || selected || activeInstanceId();
      const target = choices.find(option => option.value === preferred);
      detecting = false;
      if (preferred && (!target || target.disabled)) {
        if (!target) {
          const previous = instances.selectOptions.find(option => option.value === preferred);
          choices.push({ value: preferred, label: previous?.label || preferred,
            icon: previous?.icon || preferred.split(":")[0], disabled: true });
        }
        selected = preferred; setSelectOptions(instances, choices, selected);
        record = null; message = "所选实例暂不可用，请等待实例目录更新或明确选择其他实例。"; render(); return;
      }
      selected = preferred || choices.find(option => !option.disabled)?.value || "";
      setSelectedInstance(selected);
      setSelectOptions(instances, choices, selected);
      if (selected) await refresh(force);
      else { record = null; message = "尚未检测到可配置的会话实例。"; render(); }
    } catch (error) {
      if (alive() && generation === version) { record = null; message = error.message; render(); }
    } finally {
      if (alive() && generation === version) { detecting = false; sync(); }
    }
  }
  instances.onchange = () => {
    if (!alive() || saving || detecting) { sync(); return; }
    if (activePopup?.trigger && (activePopup.trigger === visibilityTrigger || controls.has(activePopup.trigger))) closePopup();
    pendingHidden = null; queuedHidden = null;
    selected = instances.value; setSelectedInstance(selected); void read();
  };
  display.onchange = () => {
    if (alive() && !detecting && !saving && sharedSettingsWritable(session))
      void setInstanceDisplay(selected, display.checked).catch(error => { notice(error.message, true); sync(); });
    sync();
  };
  const unsubscribe = subscribeSharedSettings(change => {
    if (!alive()) return;
    sync();
    if (change.instance && (!change.instanceId || change.instanceId === selected)
        && !page.closest?.(".appearance-settings-page")?.hidden) {
      if (saving || detecting || loading) externalRefresh = true;
      else void read();
    }
  });
  render();
  return {
    show,
    dispose() {
      disposed = true;
      pendingHidden = null; queuedHidden = null;
      if (activePopup?.trigger && (activePopup.trigger === visibilityTrigger || controls.has(activePopup.trigger))) closePopup();
      promptEditor?.close(); retireRead(); unsubscribe();
    }
  };
}

export function mountShortcuts(page) {
  const mac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const modifier = mac ? "⌘" : "Ctrl";
  const card = section(page, "会话编辑区");
  // 用户要求只显示快捷键；列表逐项对应 shell.js 已有按键处理，不增加未实现的全局快捷键。
  for (const [label, keys] of [
    ["发送消息", `${modifier} + Enter`], ["换行", "Enter"],
    ["撤销", `${modifier} + Z`], ["重做", `${modifier} + Y / ${modifier} + Shift + Z`],
    ["选择命令或 Skill", "↑ / ↓"], ["确认命令或 Skill", "Enter / Tab"],
    ["关闭菜单或设置", "Esc"]
  ]) {
    const row = element("div", "appearance-field shortcut-setting-row");
    row.append(element("span", "", label), element("kbd", "", keys)); card.append(row);
  }
  page.append(element("p", "appearance-hint", "编辑快捷键在输入框获得焦点时生效；命令菜单中的 Enter / Tab 用于选择当前项。"));
}
