// 会话实例：实例目录状态与启用判断、当前与会话所属实例、新建会话的后端选项、会话位置显示与 Claude 回顾默认值提示；目录加载与实例列表渲染在 instance-catalog.js。

import { notice } from "../../ui/notice.js";
import { creating, selectedId, session, sidebar, thread } from "../../core/state.js";
import { appearance } from "../appearance/theme.js";
import { creationInstanceId } from "../composer/creation-state.js";
import { conversationBackend } from "./capabilities.js";
import { currentThreadDisplay } from "./thread-cache.js";

export let sessionInstances = [], sessionInstanceRequest = null;
export function setSessionInstances(value) { return sessionInstances = value; }
export function setSessionInstanceRequest(value) { return sessionInstanceRequest = value; }
export let sessionTypes = [
  { id: "codex", label: "Codex" },
  { id: "claude", label: "Claude" },
  { id: "antigravity", label: "Antigravity" }
];
export function setSessionTypes(value) { return sessionTypes = value; }
function sidebarSummaryForId(id) {
  if (!id || !sidebar) return null;
  return [...(sidebar.recent || []), ...(sidebar.pinnedThreads || []),
    ...(sidebar.projects || []).flatMap(project => project.threads || [])].find(item => item.id === id) || null;
}
export function availableSessionInstances() {
  if (sessionInstances.length) return sessionInstances;
  // 用户要求实例显示名不拼接“本机”“WSL”；目录未返回时的默认名称也遵循同一规则。
  // 启动初期在 /api/settings/environments 返回前，也要把共享设置中已启用的 Antigravity 实例纳入候选，
  // 避免点击历史 Antigravity 会话时误判为“此会话实例尚未启用”并卡在“正在读取模型...”。
  const fallback = [{ id: "codex:local", backend: "codex", label: "Codex", ready: true, legacyDefault: true },
    ...(session?.claudeInstanceId ? [{ id: session.claudeInstanceId, backend: "claude",
      label: ["Claude", session.claudeWslDistribution].filter(Boolean).join(" · "), ready: true, legacyDefault: true }] : [])];
  if (appearance.sessionInstances && typeof appearance.sessionInstances === "object") {
    for (const [id, enabled] of Object.entries(appearance.sessionInstances)) {
      if (enabled !== true || !id.startsWith("antigravity:") || fallback.some(item => item.id === id)) continue;
      fallback.push({
        id,
        backend: "antigravity",
        label: id === "antigravity:local" ? "Antigravity" : "Antigravity · WSL",
        ready: true,
        legacyDefault: false
      });
    }
  }
  return fallback;
}
export function conversationInstanceId(value) {
  if (value?.instanceId) return value.instanceId;
  const backend = conversationBackend(value);
  const instances = availableSessionInstances();
  const matched = instances.find(instance => instance.backend === backend
    && (value?.wslDistribution ? instance.distribution === value.wslDistribution
      && (!value.wslUser || instance.user === value.wslUser) : instance.legacyDefault))?.id;
  if (matched) return matched;
  if (backend === "codex") return "codex:local";
  // Antigravity 等无 legacyDefault 的后端，在会话详情尚未加载且缺少显式 instanceId 时，
  // 优先匹配该后端已启用的实例，避免误判为未启用而阻断模型加载与输入区。
  return instances.find(instance => instance.backend === backend && sessionInstanceEnabled(instance.id))?.id
    || instances.find(instance => instance.backend === backend)?.id
    || "";
}
export function activeInstanceId() {
  const current = thread?.id === selectedId ? thread : null;
  const cached = typeof currentThreadDisplay === "function" ? currentThreadDisplay() : null;
  const display = current || (cached?.id === selectedId ? cached : null) || sidebarSummaryForId(selectedId);
  return creating ? creationInstanceId : conversationInstanceId(display || { id: selectedId });
}
export function sessionInstanceEnabled(id, preferences = appearance) {
  if (preferences.sessionInstances && typeof preferences.sessionInstances === "object")
    return preferences.sessionInstances[id] === true;
  const instance = availableSessionInstances().find(value => value.id === id);
  return !!instance?.legacyDefault && preferences.sessionTypes?.[instance.backend] === true;
}
export function enabledInstanceIds() {
  // 所有环境统一按实例开关决定列表入口，不按发行版名称筛选。
  return availableSessionInstances().filter(instance => sessionInstanceEnabled(instance.id)).map(instance => instance.id);
}
function sessionBackendEnabled(backend) {
  return availableSessionInstances().some(instance => instance.backend === backend && sessionInstanceEnabled(instance.id));
}
export function migrateSessionInstances(preferences, instances) {
  // 旧类型开关仅迁移到原默认实例；新发现的同类型实例保持关闭，各实例允许全部关闭。
  if (preferences.sessionInstances && typeof preferences.sessionInstances === "object") return false;
  preferences.sessionInstances = Object.fromEntries(instances.map(instance =>
    [instance.id, instance.legacyDefault === true && preferences.sessionTypes?.[instance.backend] === true]));
  return true;
}
const claudeRecapDefaultWarnings = new Map();
export function reportClaudeRecapDefaults(result, scope) {
  const warnings = [];
  for (const value of Array.isArray(result.claudeRecapDefaults) ? result.claudeRecapDefaults : []) {
    if (typeof value?.instanceId !== "string") continue;
    const key = JSON.stringify([scope, value.instanceId]);
    const message = typeof value.error === "string" ? value.error : "";
    if (!message) { claudeRecapDefaultWarnings.delete(key); continue; }
    if (claudeRecapDefaultWarnings.get(key) === message) continue;
    claudeRecapDefaultWarnings.set(key, message);
    const label = result.instances?.find(instance => instance.id === value.instanceId)?.label || value.instanceId;
    warnings.push(`${label} 的离开期间回顾未能默认关闭：${message}`);
  }
  if (warnings.length) notice(warnings.join("\n"), true);
}

export function newChatBackendOptions() {
  // 用户要求类型与环境实例分别控制；未接通的执行适配器只在设置中展示，不能悄悄改投默认实例。
  return availableSessionInstances().filter(instance => instance.ready && sessionInstanceEnabled(instance.id))
    .map(instance => ({ value: instance.id, backend: instance.backend, label: instance.label,
      icon: ["codex", "claude", "antigravity"].includes(instance.backend) ? instance.backend : "allSessions" }));
}
export function threadLocation(value) {
  // 用户要求顶部路径前仅显示非本机实例的实际环境名（如发行版名），不拼接类型、“本机”或“WSL”标签。
  const distribution = value?.wslDistribution
    || availableSessionInstances().find(instance => instance.id === value?.instanceId)?.distribution;
  return [distribution, value?.cwd].filter(Boolean).join(" · ");
}
