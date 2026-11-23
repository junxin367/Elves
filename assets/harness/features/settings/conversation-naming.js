// 用户要求所有实例共用一组命名实例与模型；配置由服务保存，页面草稿按授权隔离。
import { button, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { selectControl, setSelectOptions } from "../../ui/select.js";
import { notice } from "../../ui/notice.js";
import { confirmAction, openSettingsPrompt } from "../../ui/dialog.js";
import { saveSharedSettings } from "../../core/actions.js";
import { connected, session } from "../../core/state.js";
import { privateInputScope } from "../session/capabilities.js";
import { availableSessionInstances } from "../session/instances.js";
import { sharedSettingsState, sharedSettingsWritable, subscribeSharedSettings } from "./shared-state.js";
import { settingsEnvironments, settingsPageCache } from "./instance-settings-cache.js";

const namingListeners = new Set();
const namingModelCache = settingsPageCache("naming-models"), namingInfoCache = settingsPageCache("naming-info");
let latestNamingStatus = null;
// 用户要求可见名称与无障碍名称共用，提示词统一称为“会话命名提示词”；自动更新标题仍是自动命名的子设置。
const namingLabels = {
  enabled: "自动命名", refresh: "自动更新标题", instance: "命名实例", model: "命名模型",
  maxTitleChars: "标题长度上限", refreshEveryTurns: "标题更新间隔", systemPrompt: "会话命名提示词"
};
export function receiveConversationNaming(value) {
  latestNamingStatus = { owner: namingInfoCache.identity(""), value };
  for (const listener of namingListeners) listener(value);
  if (value?.state === "failed") notice(value.message || "自动命名未完成，请检查会话命名设置。", true);
}

export function mountConversationNaming(page) {
  page.classList.add("instance-settings-content");
  const scope = privateInputScope(session);
  let disposed = false, loading = false, saving = false, defaults = null, base = null, draft = null;
  let defaultsOwner = "", promptEditor = null;
  let revision = -1, epoch = "", generation = 0, loadedModels = [], catalogOwner = "", showing = null, modelReading = null;
  const alive = () => !disposed && page.isConnected && privateInputScope(session) === scope;
  const supported = () => session?.capabilities?.conversationNaming === true && sharedSettingsState.ready;
  const dirty = () => draft && base && JSON.stringify(draft) !== JSON.stringify(base.conversationNaming);
  const writable = () => alive() && connected && supported() && sharedSettingsWritable(session) && !saving;
  const catalogReady = () => !!draft && catalogOwner === namingModelCache.identity(draft.instanceId)
    && !namingModelCache.peek(draft.instanceId)?.blocked && loadedModels.length > 0;
  const card = element("div", "appearance-card");
  const switchControl = label => {
    const input = element("input", "appearance-notification-switch");
    input.type = "checkbox"; input.setAttribute("role", "switch"); input.setAttribute("aria-label", label);
    return input;
  };
  const enabled = switchControl(namingLabels.enabled), refresh = switchControl(namingLabels.refresh);
  const instances = selectControl("conversation-naming-instance", namingLabels.instance, [], "", "instance-settings-select",
    option => {
      const glyph = icon(option?.icon || "allSessions");
      glyph.classList.add("instance-settings-brand"); glyph.dataset.backend = option?.icon || "";
      return [glyph, element("span", "select-label", option?.label || "请选择命名实例"), icon("chevron")];
    });
  const models = selectControl("conversation-naming-model", namingLabels.model, [], "", "instance-settings-select");
  models.selectSearchable = true; models.selectSearchPlaceholder = "搜索命名模型";
  const numberControl = (label, min, max) => {
    const input = element("input"); input.type = "number"; input.min = String(min);
    input.max = String(max); input.step = "1"; input.setAttribute("aria-label", label); return input;
  };
  const length = numberControl(namingLabels.maxTitleChars, 10, 120);
  const interval = numberControl(namingLabels.refreshEveryTurns, 1, 50);
  const row = (label, description, control, tag = "label") => {
    const field = element(tag, "appearance-field"), copy = element("span", "appearance-field-copy");
    copy.append(element("span", "", label));
    if (description) copy.append(element("small", "", description));
    field.append(copy, control); card.append(field);
    return field;
  };
  row(namingLabels.enabled, "首条普通消息成功接收后生成标题，适用于同一 Harness 的 Codex、Claude 主会话及 Codex 侧边会话。", enabled);
  // 用户要求实例与模型是关联选择：同一行从左到右先实例、后模型；切换实例后清空旧模型并重新读取目录。
  const targetSelectors = element("div", "conversation-naming-target-selectors");
  targetSelectors.append(instances, models);
  row("命名实例与模型", "先选择命名实例，再选择其命名模型；所有主会话共用此组合，不改变原会话的模型。",
    targetSelectors, "div").classList.add("conversation-naming-target-row");
  const lengthBox = element("span", "appearance-number"); lengthBox.append(length);
  const intervalBox = element("span", "appearance-number"); intervalBox.append(interval);
  row(namingLabels.maxTitleChars, "默认 36 个字符，可设置为 10～120 个字符。", lengthBox);
  row(namingLabels.refresh, "仅在自动命名开启后生效；每次成功压缩后最多检查一次，主题未变时保留原标题。手动重命名后停止更新。", refresh);
  // 标题更新间隔只累计最新一次成功压缩之后的成功对话，压缩所在轮不计入默认的 5 轮。
  row(namingLabels.refreshEveryTurns, "从最近一次成功压缩后开始计数，默认满 5 个成功轮次检查标题。压缩所在轮、失败、中断和重复轮次不计。", intervalBox);
  const promptSettings = button("设置", `设置${namingLabels.systemPrompt}`, openNamingPrompt);
  promptSettings.setAttribute("aria-haspopup", "dialog");
  row(namingLabels.systemPrompt, "", promptSettings, "div");
  const status = element("p", "appearance-hint");
  status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const recent = element("p", "appearance-hint");
  const actions = element("div", "appearance-action-buttons");
  const reload = button("重新读取", "重新读取会话命名设置", () => void show(true), "text-button");
  const save = button("保存", "保存会话命名设置", () => void persist(), "primary");
  actions.append(reload, save);
  page.append(card, status, actions);

  function sync() {
    const canWrite = writable();
    for (const control of [enabled, instances, models, length, refresh, interval])
      control.disabled = !canWrite || !draft;
    models.disabled ||= !catalogReady();
    promptSettings.disabled = !alive() || !connected || !supported() || !defaults?.systemPrompt
      || defaultsOwner !== namingInfoCache.identity("") || saving;
    // 主开关关闭时子设置不生效，禁用控件并保留原选择，避免呈现为两个互相独立的命名开关。
    refresh.disabled ||= !draft?.enabled;
    interval.disabled ||= !draft?.enabled || !draft?.refreshEnabled;
    save.disabled = !canWrite || draft?.enabled && !catalogReady() || !dirty();
    reload.disabled = saving || loading || !alive();
    if (promptEditor) alive() ? promptEditor.sync() : promptEditor.close();
  }
  function restore() {
    base = structuredClone(sharedSettingsState.settings); draft = structuredClone(base.conversationNaming);
    revision = sharedSettingsState.revision; epoch = sharedSettingsState.epoch;
    enabled.checked = draft.enabled; refresh.checked = draft.refreshEnabled;
    length.value = String(draft.maxTitleChars); interval.value = String(draft.refreshEveryTurns);
    renderInstances(); renderModels(); sync();
  }
  function openNamingPrompt() {
    if (promptSettings.disabled || !alive() || !supported() || !defaults?.systemPrompt || promptEditor) return;
    const editBase = structuredClone(sharedSettingsState.settings);
    const editRevision = sharedSettingsState.revision, editEpoch = sharedSettingsState.epoch;
    const initial = editBase.conversationNaming.systemPrompt, defaultValue = defaults.systemPrompt;
    const getBlock = () => {
      if (!alive() || editEpoch !== sharedSettingsState.epoch)
        return "服务或授权已变化，请重新打开设置。";
      if (sharedSettingsState.settings.conversationNaming.systemPrompt !== initial)
        return "会话命名提示词已在其他客户端更新，请关闭后重新打开核对。";
      return writable() ? "" : "当前无法修改，请核对连接和写入权限。";
    };
    // 用户要求弹窗独立保存提示词：沿用打开时的版本，只提交 systemPrompt，不带上页面其他未保存的字段。
    promptEditor = openSettingsPrompt({ title: namingLabels.systemPrompt,
      value: !initial || initial === defaultValue ? null : initial, defaultValue, max: 10000, getBlock,
      returnFocus: () => promptSettings,
      onClose() { promptEditor = null; },
      async onSave(value) {
        const blocked = getBlock();
        if (blocked) throw new Error(blocked);
        const systemPrompt = value ?? defaultValue, hadDraft = dirty();
        saving = true; sync();
        try {
          const snapshot = await saveSharedSettings({ conversationNaming: { systemPrompt } },
            { base: editBase, expectedRevision: editRevision });
          if (!alive() || editEpoch !== sharedSettingsState.epoch
              || snapshot?.settings?.conversationNaming?.systemPrompt !== systemPrompt)
            throw new Error("会话命名提示词保存结果未确认，请重新读取核对。");
          if (hadDraft) {
            // 保留其他字段的草稿和基线；不能借此吸收另一个客户端的改动。
            base.conversationNaming.systemPrompt = draft.systemPrompt = systemPrompt;
          } else restore();
          status.textContent = "会话命名提示词已保存。";
        } finally { saving = false; if (alive()) sync(); }
      }
    });
  }
  function renderInstances() {
    const choices = availableSessionInstances().map(instance => ({ value: instance.id,
      label: instance.label, icon: instance.backend, disabled: !instance.ready || !!instance.error }));
    if (draft?.instanceId && !choices.some(option => option.value === draft.instanceId)) {
      const previous = instances.selectOptions.find(option => option.value === draft.instanceId);
      choices.push({ value: draft.instanceId, label: previous?.label || draft.instanceId,
        icon: previous?.icon || draft.instanceId.split(":")[0], disabled: true });
    }
    setSelectOptions(instances, choices, draft?.instanceId || "");
  }
  function renderModels() {
    // 模型隐藏偏好只限制选择列表；已有命名配置继续保留，不因隐藏而停用或改投其他模型。
    const available = catalogOwner === namingModelCache.identity(draft?.instanceId) ? loadedModels : [];
    const choices = available.filter(model => model.hiddenByInstance !== true)
      .map(model => ({ value: model.model, label: model.displayName || model.model }));
    if (draft?.model && !choices.some(option => option.value === draft.model)) {
      const current = available.find(model => model.model === draft.model);
      choices.unshift({ value: draft.model, label: current?.hiddenByInstance === true
        ? `${current.displayName || draft.model}（已隐藏，保留当前配置）`
        : `${draft.model}（请核对可用状态）`, disabled: true });
    }
    if (!draft?.model || !choices.length) choices.unshift({ value: "", label: loading ? "正在读取命名模型…" : "请选择命名模型" });
    setSelectOptions(models, choices, draft?.model || "");
  }
  async function readNamingModels(force = false) {
    const id = draft?.instanceId, owner = namingModelCache.identity(id);
    if (modelReading?.owner === owner && !force) return modelReading.promise;
    const version = ++generation, cached = namingModelCache.peek(id);
    const changed = catalogOwner !== owner || cached && loadedModels !== cached.value.data;
    // 用户要求切回命名页先保留已有模型，只有切换实际实例时清除另一实例的目录。
    if (catalogOwner !== owner) loadedModels = cached?.value.data || [];
    else if (cached) loadedModels = cached.value.data;
    catalogOwner = owner;
    if (changed) renderModels();
    if (!force && cached?.fresh) { loading = false; sync(); return; }
    loading = true; sync();
    const request = { owner, promise: null };
    modelReading = request;
    request.promise = (async () => {
      try {
        const instance = availableSessionInstances().find(instance => instance.id === id);
        if (!instance?.ready || instance.error) throw new Error("所选命名实例暂不可用，请重新选择命名实例。");
        const result = await namingModelCache.read(id, { force, background: loadedModels.length > 0 });
        if (!alive() || generation !== version || draft.instanceId !== id) return;
        if (!Array.isArray(result.data) || result.data.some(model => typeof model.model !== "string"))
          throw new Error("命名模型目录返回无效。");
        const changed = JSON.stringify(loadedModels) !== JSON.stringify(result.data);
        loadedModels = result.data;
        if (changed) renderModels();
        if (!loadedModels.length) status.textContent = "所选命名实例未返回可用模型，请检查该实例的原生 CLI。";
      } catch (error) {
        if (alive() && generation === version && error.name !== "AbortError") status.textContent = error.message;
      } finally {
        if (modelReading === request) modelReading = null;
        if (alive() && generation === version) { loading = false; sync(); }
      }
    })();
    return request.promise;
  }
  function renderRecent(value) {
    recent.textContent = value?.state === "failed" ? value.message
      : value?.state === "completed" ? `最近自动命名结果：${value.title}` : "";
  }
  async function show(force = false) {
    if (!alive() || saving) return;
    if (showing && !force) return showing;
    if (force && dirty() && !await confirmAction("重新读取会话命名设置", "当前未保存的修改将被服务端设置替换。", "重新读取")) return;
    if (!alive()) return;
    if (!supported()) { status.textContent = "当前服务尚未支持会话命名，请更新服务后设置。"; sync(); return; }
    status.textContent = "";
    // 共享配置已经就绪即可显示表单；状态、默认提示词和目录并行准备，重复进入复用同一份请求。
    if (!draft || force || !dirty()) restore();
    const operation = (async () => {
      try {
        const preloaded = availableSessionInstances().some(instance => instance.id === draft.instanceId && instance.ready && !instance.error)
          ? readNamingModels(force) : null;
        const infoOwner = namingInfoCache.identity("");
        const readInfo = namingInfoCache.read("", { force, background: !!defaults }).then(data => {
          if (!alive() || infoOwner !== namingInfoCache.identity("")) return;
          defaults = data.defaults; defaultsOwner = infoOwner;
          const latest = latestNamingStatus?.owner === infoOwner ? latestNamingStatus.value : null;
          renderRecent(latest && (!data.status?.updatedAt || latest.updatedAt >= data.status.updatedAt) ? latest : data.status);
          sync();
        });
        await Promise.all([readInfo, settingsEnvironments(force), preloaded]);
        if (!alive()) return;
        renderInstances();
        if (!preloaded) await readNamingModels(force);
        sync();
      } catch (error) { if (alive() && error.name !== "AbortError") { status.textContent = error.message; sync(); } }
      finally { if (showing === operation) showing = null; }
    })();
    showing = operation;
    return operation;
  }
  async function persist() {
    if (!writable() || save.disabled || !draft || !base) return;
    if (epoch !== sharedSettingsState.epoch) { status.textContent = "服务已重新连接，请重新打开设置后核对。"; return; }
    if (draft.enabled && (!availableSessionInstances().some(instance => instance.id === draft.instanceId && instance.ready)
        || !loadedModels.some(model => model.model === draft.model))) {
      status.textContent = "请选择可用的命名实例和命名模型后开启自动命名。"; return;
    }
    if (!Number.isInteger(draft.maxTitleChars) || draft.maxTitleChars < 10 || draft.maxTitleChars > 120
        || !Number.isInteger(draft.refreshEveryTurns) || draft.refreshEveryTurns < 1 || draft.refreshEveryTurns > 50
        || !draft.systemPrompt.trim() || Array.from(draft.systemPrompt).length > 10000) {
      status.textContent = `请核对标题长度上限、标题更新间隔和${namingLabels.systemPrompt}。`; return;
    }
    const patch = Object.fromEntries(Object.entries(draft).filter(([key, value]) => value !== base.conversationNaming[key]));
    saving = true; status.textContent = "正在同步到所有客户端…"; sync();
    try {
      await saveSharedSettings({ conversationNaming: patch }, { base, expectedRevision: revision });
      if (!alive()) return;
      restore(); status.textContent = "会话命名设置已保存，同一 Harness的各客户端共用。";
    } catch (error) {
      if (alive()) status.textContent = `${error.message} 修改内容已保留，请核对其他客户端的设置。`;
    } finally { if (alive()) { saving = false; sync(); } }
  }
  enabled.onchange = () => { draft.enabled = enabled.checked; sync(); };
  refresh.onchange = () => { draft.refreshEnabled = refresh.checked; sync(); };
  instances.onchange = () => {
    draft.instanceId = instances.value; draft.model = ""; status.textContent = ""; void readNamingModels(); sync();
  };
  models.onchange = () => { draft.model = models.value; sync(); };
  length.oninput = () => { draft.maxTitleChars = Number(length.value); sync(); };
  interval.oninput = () => { draft.refreshEveryTurns = Number(interval.value); sync(); };
  const unsubscribe = subscribeSharedSettings(change => {
    if (!alive() || !supported()) { sync(); return; }
    if (draft && !saving) {
      const changed = JSON.stringify(base?.conversationNaming) !== JSON.stringify(sharedSettingsState.settings.conversationNaming);
      if (dirty() && changed) status.textContent = "其他客户端已修改会话命名设置，当前未保存内容已保留；保存时将核对冲突。";
      else if (!dirty() && changed) { restore(); if (!page.closest(".appearance-settings-page")?.hidden) void readNamingModels(); }
    }
    if (change.instance && change.modelCatalogChanged !== false && (!change.instanceId || change.instanceId === draft?.instanceId)
        && !page.closest(".appearance-settings-page")?.hidden && !saving) void readNamingModels();
    sync();
  });
  const receive = value => { if (alive()) renderRecent(value); };
  namingListeners.add(receive); sync();
  return { show, dispose() {
    disposed = true; promptEditor?.close(); generation++; unsubscribe(); namingListeners.delete(receive);
  } };
}
