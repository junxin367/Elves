// 用户要求实例设置先展示已确认内容、后台更新；只在内存中按授权与真实实例复用，不持久保存原生配置。
import { nativeConnectionVersion, session } from "../../core/state.js";
import { api } from "../api.js";
import { privateInputScope } from "../session/capabilities.js";
import { availableSessionInstances } from "../session/instances.js";
import { loadSessionInstances } from "../session/instance-catalog.js";
import { sharedSettingsState, subscribeSharedSettings } from "./shared-state.js";

// 与原生能力检查的 60 秒窗口一致；命中、失败和重新展示都不能延长原采集时间。
const settingsFreshMs = 60000;
const settingsEntries = new Map();
let settingsOwner = "";
let settingsEnvironmentSnapshot = null, settingsEnvironmentRequest = null;
function settingsCacheOwner() {
  const scope = privateInputScope(session);
  return scope ? JSON.stringify([scope, sharedSettingsState.epoch, nativeConnectionVersion, !!session.readOnly]) : "";
}
function settingsEntryKey(id, section) {
  const owner = settingsCacheOwner();
  if (owner !== settingsOwner) {
    for (const entry of settingsEntries.values()) entry.request?.controller.abort();
    settingsEntries.clear(); settingsOwner = owner;
  }
  const instance = availableSessionInstances().find(instance => instance.id === id);
  if (owner && section === "page:naming-info") return JSON.stringify([owner, section]);
  if (!owner || !instance || instance.error) return "";
  // 相同 ID 的 CLI 路径、WSL 用户或发行版变化后不能恢复旧配置，更不能把它当成可写的新目标。
  return JSON.stringify([owner, id, section, instance.backend, instance.environmentId,
    instance.distribution, instance.user, instance.home, instance.executable]);
}
function settingsEntry(id, section) {
  const key = settingsEntryKey(id, section);
  if (!key) return null;
  let entry = settingsEntries.get(key);
  if (!entry) {
    entry = { id, section, value: null, loadedAt: 0, stale: true, blocked: false, request: null };
    settingsEntries.set(key, entry);
    // 仅保留最近使用的 64 份实例分类，避免长时间切换实例无限积累配置内容。
    if (settingsEntries.size > 64) {
      const oldest = [...settingsEntries].find(([candidate, value]) => candidate !== key && !value.request);
      if (oldest) settingsEntries.delete(oldest[0]);
    }
  } else {
    settingsEntries.delete(key); settingsEntries.set(key, entry);
  }
  return entry;
}
function settingsSignature(value) {
  const { updated, settingsEpoch, instanceRevision, ...content } = value;
  return JSON.stringify(content);
}
function rememberSettingsEntry(entry, value, confirmedRead = false) {
  const signature = settingsSignature(value);
  // 配置未变化时保留控件绑定的记录对象，静默核对不能重建正在编辑的控件、丢失焦点或草稿。
  if (entry.value && entry.signature === signature) Object.assign(entry.value, value);
  else entry.value = value;
  const change = entry.requiredChange;
  const outdated = !confirmedRead && !!change && !(change.epoch && Number.isSafeInteger(change.revision)
    && change.revision >= 0 && value.settingsEpoch === change.epoch && value.instanceRevision >= change.revision);
  // 保存回包之前若已收到更晚变更，不能用较早回执把缓存重新标新；随后读取仍须核对该实例。
  Object.assign(entry, { signature, loadedAt: Date.now(), stale: outdated, blocked: outdated });
  if (!outdated) entry.requiredChange = null;
  return entry.value;
}
export function instanceSettingsIdentity(id, section) {
  return settingsEntryKey(id, section);
}
export function cachedInstanceSettings(id, section) {
  const entry = settingsEntry(id, section);
  return entry?.value ? { value: entry.value, blocked: entry.blocked,
    fresh: !entry.stale && Date.now() - entry.loadedAt < settingsFreshMs } : null;
}
export function invalidateInstanceSettings(id) {
  settingsEntryKey(id, "general");
  for (const entry of settingsEntries.values()) {
    if (entry.section === "page:naming-info") continue;
    if (id && entry.id !== id) continue;
    entry.stale = true; entry.blocked = true;
    entry.request?.controller.abort(); entry.request = null;
  }
}
export function storeInstanceSettings(id, section, value) {
  const entry = settingsEntry(id, section);
  // 保存期间重开窗口也可能启动读取；成功回执再次撤销它，迟到旧快照不能覆盖已确认的新值。
  entry?.request?.controller.abort();
  if (entry) entry.request = null;
  return entry ? rememberSettingsEntry(entry, value) : value;
}
async function readSettingsEntry(id, section, { force = false, background = false } = {}, read, validate) {
  const entry = settingsEntry(id, section), owner = settingsOwner, key = settingsEntryKey(id, section);
  if (!entry) throw new Error("所选实例暂不可用，请重新检测或明确选择其他实例。");
  if (entry.request && (!force || entry.request.force)) return entry.request.promise;
  if (!force && entry.value && !entry.stale && Date.now() - entry.loadedAt < settingsFreshMs) return entry.value;
  entry.request?.controller.abort();
  const request = { force, controller: new AbortController(), promise: null };
  entry.request = request;
  request.promise = read(request.controller.signal, { resourcePriority: background ? "background" : "foreground" }, force)
    .then(result => {
      if (request.controller.signal.aborted || owner !== settingsCacheOwner()
          || settingsEntryKey(id, section) !== key || settingsEntries.get(key) !== entry || entry.request !== request)
        throw new DOMException("实例设置读取已失效", "AbortError");
      const backend = availableSessionInstances().find(instance => instance.id === id)?.backend;
      if (!result || result.ok === false || !validate(result, backend))
        throw new Error("实例设置返回结果无法确认，请重新读取。");
      return rememberSettingsEntry(entry, result, true);
    }).catch(error => {
      if (entry.request === request && !request.controller.signal.aborted) {
        entry.stale = true; entry.blocked = true;
      }
      throw error;
    }).finally(() => { if (entry.request === request) entry.request = null; });
  return request.promise;
}
export function readInstanceSettings(id, section, options = {}) {
  return readSettingsEntry(id, section, options,
    (signal, priority, force) => api("/api/settings/instance",
      { instanceId: id, section, ...(force ? { forceReload: true } : {}) }, undefined, signal, priority),
    (result, backend) => result.instanceId === id && result.backend === backend
      && typeof result.revision === "string" && typeof result.preferenceRevision === "string"
      && typeof result.readOnly === "boolean" && result.settings
      && typeof result.settings === "object" && !Array.isArray(result.settings));
}
// 其他设置页复用相同的授权、实例、版本及在途隔离；权限组件通过注入使用，根目录组件不反向依赖业务模块。
export function settingsPageCache(name) {
  if (!["permissions", "instructions", "naming-models", "naming-info"].includes(name))
    throw new Error("设置缓存分类无效");
  const section = `page:${name}`;
  return {
    identity: id => instanceSettingsIdentity(id, section),
    // 草稿随真实实例和授权隔离，服务重连不删除未保存文本；读取版本仍负责检测文件冲突。
    draftIdentity(id) {
      const instance = availableSessionInstances().find(instance => instance.id === id);
      return JSON.stringify([privateInputScope(session), id, instance?.environmentId,
        instance?.distribution, instance?.user, instance?.executable]);
    },
    peek: id => cachedInstanceSettings(id, section),
    invalidate: id => invalidateInstanceSettings(id),
    store: (id, value) => storeInstanceSettings(id, section, value),
    read(id, options = {}) {
      const backend = availableSessionInstances().find(instance => instance.id === id)?.backend;
      const models = name === "naming-models", info = name === "naming-info";
      const path = models ? "/api/read/models" : info ? "/api/settings/conversation-naming" : `/api/settings/${name}`;
      return readSettingsEntry(id, section, options,
        (signal, priority, force) => api(path, info ? undefined : models
          ? { instanceId: id, backend, includeAvailable: true, ...(force ? { forceReload: true } : {}) }
          : { instanceId: id }, undefined, signal, priority),
        result => models ? result.catalogComplete !== false && Array.isArray(result.data)
            && result.data.every(model => model && typeof model.model === "string")
          : info ? !!result.defaults && typeof result.defaults.systemPrompt === "string"
          : typeof result.revision === "string" && typeof result.path === "string"
            && (name === "instructions" ? typeof result.content === "string" && typeof result.exists === "boolean"
              && (result.instanceId == null || result.instanceId === id)
              : result.instanceId === id && result.backend === backend && result.settings
                && typeof result.settings === "object" && !Array.isArray(result.settings)));
    }
  };
}
export async function settingsEnvironments(force = false) {
  const owner = settingsCacheOwner();
  // 所有设置页共用 60 秒检测结果及在途扫描；命中不续期，重新检测仍等待新的强制扫描完成。
  if (settingsEnvironmentRequest?.owner === owner) {
    if (!force || settingsEnvironmentRequest.force) return settingsEnvironmentRequest.promise;
    await settingsEnvironmentRequest.promise.catch(() => {});
    if (owner !== settingsCacheOwner()) throw new DOMException("实例检测已失效", "AbortError");
    return settingsEnvironments(true);
  }
  if (!force && settingsEnvironmentSnapshot?.owner === owner
      && Date.now() - settingsEnvironmentSnapshot.loadedAt < settingsFreshMs) return settingsEnvironmentSnapshot.value;
  const request = { owner, force, promise: null };
  settingsEnvironmentRequest = request;
  request.promise = loadSessionInstances(force).then(value => {
    if (owner !== settingsCacheOwner()) throw new DOMException("实例检测结果已失效", "AbortError");
    settingsEnvironmentSnapshot = { owner, value, loadedAt: Date.now() };
    return value;
  }).finally(() => { if (settingsEnvironmentRequest === request) settingsEnvironmentRequest = null; });
  return request.promise;
}
subscribeSharedSettings(change => {
  if (!change.instance) return;
  settingsEntryKey(change.instanceId, "general");
  const epoch = change.epoch || sharedSettingsState.epoch;
  const revision = change.instanceRevision ?? sharedSettingsState.instanceRevision;
  for (const entry of settingsEntries.values()) {
    if (entry.section === "page:naming-info") continue;
    if (change.instanceId && entry.id !== change.instanceId) continue;
    if (change.instanceId && (entry.section === "page:naming-models" && change.modelCatalogChanged === false
        || entry.section === "page:instructions" && change.instructionsChanged === false)) continue;
    // 保存回执已含本次服务版本时，随后到达的同版本事件无需再读；更晚的其他客户端变更仍须核对。
    if (epoch && Number.isSafeInteger(revision) && revision >= 0
        && entry.value?.settingsEpoch === epoch && entry.value.instanceRevision >= revision) continue;
    entry.requiredChange = { epoch, revision };
    entry.stale = true; entry.blocked = true;
    entry.request?.controller.abort(); entry.request = null;
  }
});
