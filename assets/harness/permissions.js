// 实例设置修改原生全局配置；编辑区菜单只选择当前会话的权限，两者不能互相代写。
// 用户要求 Codex 菜单与 App 同名同义，只保留这四项；不新增只读、实例设置或命名配置入口。
const codexModes = [
  { value: "auto", label: "请求批准", icon: "permissionAsk",
    description: "编辑外部文件和使用互联网时始终询问" },
  { value: "guardian-approvals", label: "帮我批准", icon: "shield",
    description: "仅对检测到的风险操作请求批准" },
  { value: "full-access", label: "完全访问权限", shortLabel: "完全访问", icon: "permissionFull",
    description: "可不受限制地访问互联网和你电脑上的任何文件" },
  { value: "configured", label: "自定义 (config.toml)", shortLabel: "自定义", icon: "settings",
    description: "使用 config.toml 中定义的权限" }
];
// Claude 菜单只提供 App 的五种模式；manual/default 共用一项，配置继承和旧 dontAsk 值仅兼容既有记录。
const claudeModes = [
  { value: "default", label: "手动", icon: "claudeManual", description: "每次更改前都先询问" },
  { value: "acceptEdits", label: "接受编辑", icon: "claudeEdits", description: "自动接受所有文件编辑" },
  { value: "plan", label: "计划", icon: "claudePlan", description: "更改前先制定计划" },
  { value: "auto", label: "自动", icon: "claudeAuto", description: "由 Claude 处理权限决策" },
  { value: "bypassPermissions", label: "绕过权限", icon: "claudeBypass", description: "接受所有权限请求" }
];
// 用户要求 Antigravity 权限菜单去除计划模式、增加无限制模式并参考 Claude 绕过权限的菜单效果与警示色；
// 其中 default / accept-edits / auto 经官方 ACP session/set_mode 切换，bypassPermissions 经官方 `agy acp --dangerously-skip-permissions` 生效。
const antigravityModes = [
  { value: "default", label: "询问确认", icon: "claudeManual", description: "按权限设置，在编辑与执行命令前询问" },
  { value: "accept-edits", label: "自动编辑", icon: "claudeEdits", description: "自动接受工作区文件修改，命令仍需确认" },
  { value: "auto", label: "自动执行", icon: "claudeAuto", description: "自动批准文件编辑与安全命令，不可逆操作仍询问" },
  { value: "bypassPermissions", label: "无限制模式", shortLabel: "无限制", icon: "claudeBypass", description: "自动批准所有权限请求，不再询问" }
];

export function permissionOptions(backend, supportedModes = null) {
  const options = backend === "claude" ? claudeModes : backend === "antigravity" ? antigravityModes : codexModes;
  return options.filter(option => !supportedModes || backend === "codex" && option.value === "configured"
      || supportedModes.includes(option.value)
      || backend === "claude" && option.value === "default" && supportedModes.includes("manual"))
    .map(option => ({ ...option, value: backend === "claude" && option.value === "default"
        && supportedModes && !supportedModes.includes("default") && supportedModes.includes("manual")
      ? "manual" : option.value }));
}

export function permissionModeKnown(backend, mode) {
  return permissionOptions(backend).some(option => option.value === mode)
    || (backend === "claude" ? ["manual", "configured", "dontAsk"].includes(mode)
      : backend === "antigravity" ? mode === "configured"
      : mode === "read-only");
}

export function permissionLabel(backend, mode, creating = false) {
  // 旧会话的只读预设仍按原权限执行；与 App 一致归入“请求批准”，只改变展示，不自动迁移权限。
  const value = backend === "claude" ? mode === "manual" ? "default" : mode
    : backend === "antigravity" ? mode || "default"
    : mode === "read-only" ? "auto" : mode || "configured";
  const option = permissionOptions(backend).find(option => option.value === value);
  return option?.shortLabel || option?.label || (backend === "codex" && creating ? "自定义" : "权限");
}

export function mountInstancePermissions({
  page, element, selectControl, setSelectOptions, setSelectValue, api, getInstances,
  readOnly, authorizationScope, onLoaded = () => {}, onSaved = () => {},
  cache = null, confirmReload = async () => true
}) {
  const scope = authorizationScope();
  const current = () => page.isConnected && scope === authorizationScope();
  const instanceSlot = element("div", "permission-instance-slot");
  const path = element("span", "appearance-hint instruction-path");
  const target = element("div", "permission-instance-target");
  const heading = element("div", "appearance-field permission-instance-field");
  target.append(path, instanceSlot);
  heading.append(element("span", "", "会话实例"), target);
  const card = element("div", "appearance-card instance-permissions-card");
  const fields = element("div", "instance-permission-fields");
  const status = element("p", "appearance-hint permission-settings-status");
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  const actions = element("div", "appearance-action-buttons permission-settings-actions");
  const reload = element("button", "", "重新读取"), save = element("button", "primary", "保存权限");
  reload.type = save.type = "button";
  actions.append(reload, save);
  card.append(heading, fields, status, actions);
  page.append(element("p", "appearance-hint",
    "修改所选实例的全局权限，与原生客户端设置共用。已有会话的单独权限选择保持不变。"), card);
  const instances = selectControl("permission-instance", "权限设置的会话实例", [], "");
  instanceSlot.append(instances);
  const records = new Map(), reading = new Map(), writing = new Set();
  const fieldControls = new Set();
  let disposed = false, selected = "";
  const alive = () => !disposed && current();
  const instance = () => getInstances().find(instance => instance.id === selected);
  const ownerKey = id => cache?.identity(id) ?? id;
  const allowed = () => alive() && !readOnly() && records.get(selected)?.readOnly !== true
    && !!instance() && !instance().error && !records.get(selected)?.needsRead
    && (!records.has(selected) || records.get(selected).owner === ownerKey(selected))
    && !cache?.peek(selected)?.blocked;
  const changes = record => Object.fromEntries(Object.entries(record?.draft || {})
    .filter(([key, value]) => value !== record.original[key]));
  const syncButtons = () => {
    const record = records.get(selected), loading = reading.has(selected), busy = writing.has(selected);
    save.disabled = !allowed() || busy || !record || !Object.keys(changes(record)).length;
    save.textContent = busy ? "正在保存…" : "保存权限";
    save.setAttribute("aria-busy", String(busy));
    reload.disabled = loading || busy;
    instances.disabled = !getInstances().length;
    for (const control of fieldControls) control.disabled = !allowed() || busy || control.permissionUnsupported;
  };
  function addField(label, name, options, value, disabled = false) {
    const row = element("label", "appearance-field instance-permission-field");
    row.append(element("span", "", label));
    if (name === "permissionMode" && ["default", "manual"].includes(value))
      value = options.find(option => ["default", "manual"].includes(option.value))?.value || value;
    if (!options.some(option => option.value === value))
      options = [...options, { value, label: "当前自定义配置（保留）", disabled: true }];
    const control = selectControl(`permission-global-${name}`, label, options, value);
    control.permissionUnsupported = disabled; fieldControls.add(control);
    control.disabled = disabled || !allowed() || writing.has(selected);
    const owner = records.get(selected), ownerId = selected;
    control.onchange = () => {
      const record = records.get(selected);
      // 用户要求迟到选择仍绑定编辑时的实例与记录，不能把旧控件的值写入刚切换的权限配置。
      if (!allowed() || !record || writing.has(selected) || record !== owner || selected !== ownerId
          || control.isConnected === false) return;
      record.draft[name] = control.value;
      record.message = "";
      status.textContent = "尚未保存";
      syncButtons();
    };
    row.append(control);
    fields.append(row);
  }
  function render() {
    if (!alive()) return;
    const record = records.get(selected);
    fields.replaceChildren(); fieldControls.clear();
    path.textContent = record?.path || "";
    status.textContent = reading.has(selected) && !record ? "正在读取实例权限…" : record?.message || "";
    if (record) {
      const defaults = { value: "", label: "使用原生默认" };
      if (record.backend === "codex") {
        addField("批准策略", "approvalPolicy",
          [defaults, { value: "on-request", label: "请求批准" }, { value: "never", label: "从不请求批准" }],
          record.draft.approvalPolicy);
        addField("沙盒设置", "sandboxMode", [defaults,
          { value: "read-only", label: "只读" }, { value: "workspace-write", label: "工作区写入" },
          { value: "danger-full-access", label: "完全访问权限" }],
        record.permissionProfile ? "__profile__" : record.draft.sandboxMode, !!record.permissionProfile);
        if (record.permissionProfile)
          fields.append(element("p", "appearance-hint",
            "当前访问范围包含本界面不支持的配置，请在原生 config.toml 中调整。"));
      } else {
        const modes = permissionOptions(record.backend, record.supportedModes).filter(option => option.value !== "configured");
        addField("默认权限模式", "permissionMode", [defaults, ...modes], record.draft.permissionMode,
          record.supportedModes?.length === 0);
        if (record.supportedModes?.length === 0)
          fields.append(element("p", "appearance-hint", "暂时无法确认该 CLI 支持的权限模式，请重新读取后再修改。"));
      }
      if (record.readOnly || readOnly()) status.textContent ||= "当前为只读访问";
    }
    syncButtons();
  }
  function receive(id, result, discard = false) {
    const target = getInstances().find(instance => instance.id === id);
    if (result?.instanceId !== id || result.backend !== target?.backend
        || typeof result.revision !== "string" || !result.settings || typeof result.settings !== "object"
        || Array.isArray(result.settings)) throw new Error("实例权限读取结果不匹配，请重新读取。");
    const previous = records.get(id);
    if (!discard && previous?.source === result && previous.owner === ownerKey(id)) {
      previous.needsRead = !!Object.keys(changes(previous)).length && previous.revision !== result.revision;
      return false;
    }
    const original = target.backend === "codex" ? {
      approvalPolicy: result.settings.customApproval ? "__custom__" : result.settings.approvalPolicy || "",
      sandboxMode: result.settings.sandboxMode || ""
    } : { permissionMode: result.settings.permissionMode || "" };
    const dirty = previous && Object.keys(changes(previous)).length;
    // 后台核对保留未保存权限和原编辑版本；配置已变化时显式重读后合并，不能悄悄借新版本覆盖。
    const preserve = !discard && dirty && previous.owner === ownerKey(id);
    records.set(id, { ...result, ...result.settings, source: result, owner: ownerKey(id),
      original: preserve ? previous.original : original, draft: preserve ? previous.draft : { ...original },
      revision: preserve ? previous.revision : result.revision,
      needsRead: preserve && previous.revision !== result.revision,
      message: preserve && previous.revision !== result.revision
        ? "实例权限已变化，未保存的修改已保留；请重新读取后合并。" : discard ? "" : previous?.message || "" });
    onLoaded(id, result);
    return true;
  }
  async function load(id, force = false) {
    if (!id || !alive() || writing.has(id)) return;
    const cached = cache?.peek(id);
    let changed = false;
    if (cached) changed = receive(id, cached.value, force);
    if (!force && (cache ? cached?.fresh : records.has(id))) {
      if (changed) render(); else syncButtons();
      return;
    }
    if (reading.has(id)) return reading.get(id);
    const identity = ownerKey(id);
    if (records.has(id)) records.get(id).needsRead = force || cached?.blocked === true;
    const operation = (cache ? cache.read(id, { force, background: records.has(id) })
      : api("/api/settings/permissions", { instanceId: id })).then(result => {
      if (!alive() || ownerKey(id) !== identity || reading.get(id) !== operation) return;
      changed = receive(id, result, force) || changed;
    }).catch(error => {
      if (!alive()) return;
      const record = records.get(id);
      if (error.name === "AbortError") return;
      if (record) { record.message = error.message; record.needsRead = true; }
      if (selected === id) status.textContent = error.message;
    }).finally(() => {
      if (reading.get(id) !== operation) return;
      reading.delete(id);
      if (alive() && selected === id) {
        if (changed) render();
        else syncButtons();
      }
    });
    reading.set(id, operation);
    if (!records.has(id) || changed) render(); else syncButtons();
    return operation;
  }
  instances.onchange = () => {
    selected = instances.value;
    render();
    void load(selected);
  };
  reload.onclick = async () => {
    if (Object.keys(changes(records.get(selected))).length && !await confirmReload()) return;
    if (alive()) await load(selected, true);
  };
  save.onclick = async () => {
    const id = selected, record = records.get(id), settings = changes(record);
    if (!allowed() || !record || !Object.keys(settings).length || writing.has(id)) return;
    const identity = ownerKey(id);
    reading.delete(id); cache?.invalidate(id);
    writing.add(id);
    record.message = "";
    render();
    try {
      const result = await api("/api/settings/permissions", {
        operation: "save", instanceId: id, expectedRevision: record.revision, settings
      });
      if (scope !== authorizationScope()) return;
      if (ownerKey(id) !== identity) throw new Error("实例或服务已变化，请重新读取核对权限保存结果。");
      if (result?.instanceId !== id || typeof result.revision !== "string"
          || !result.settings || result.backend !== record.backend)
        throw new Error("保存结果尚未确认，请重新读取实例权限。");
      for (const [key, value] of Object.entries(settings)) {
        const actual = result.settings[key] || "";
        if (actual !== value && !(key === "permissionMode"
            && ["manual", "default"].includes(actual) && ["manual", "default"].includes(value)))
          throw new Error("权限保存回执与提交不一致，请重新读取核对。");
      }
      const confirmed = cache?.store(id, result) || result;
      if (!alive()) return;
      const original = record.backend === "codex" ? {
        approvalPolicy: result.settings.customApproval ? "__custom__" : result.settings.approvalPolicy || "",
        sandboxMode: result.settings.sandboxMode || ""
      } : { permissionMode: result.settings.permissionMode || "" };
      records.set(id, { ...record, ...result, ...result.settings, source: confirmed,
        needsRead: cache?.peek(id)?.blocked === true, original, draft: { ...original },
        message: "已保存至实例全局配置。新会话及后续按实例配置加载的操作使用新设置。" });
      onSaved(id, result);
    } catch (error) {
      if (alive()) { record.needsRead = true; record.message = `${error.message} 未自动重复保存，可重新读取核对。`; }
    } finally {
      writing.delete(id);
      if (alive() && selected === id) {
        render();
        if (cache?.peek(id)?.blocked) void load(id);
      }
    }
  };
  return {
    async show(id = null) {
      if (!alive()) return;
      const previous = selected;
      const choices = getInstances().map(instance => ({ value: instance.id, label: instance.label, icon: instance.backend,
        disabled: !!instance.error }));
      selected = id || selected || choices.find(option => !option.disabled)?.value || "";
      if (selected && !choices.some(option => option.value === selected))
        choices.push({ value: selected, label: selected, disabled: true });
      setSelectOptions(instances, choices, selected);
      setSelectValue(instances, selected);
      if (previous !== selected || !records.has(selected)) render(); else syncButtons();
      if (!selected) status.textContent = "尚未检测到可配置的实例";
      else if (!instance() || instance().error) status.textContent = "所选实例暂不可用，请重新检测或明确选择其他实例。";
      else await load(selected);
    },
    refreshInstance(id) {
      if (!alive() || id && id !== selected || writing.has(selected)) return;
      if (selected) void load(selected);
    },
    dispose() { disposed = true; }
  };
}
