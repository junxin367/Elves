// 5h/7d 额度与消耗预测提示：
// - Claude 实例经所属 WSL Claude CLI 读取 5h/7d 剩余额度、账号、订阅与重置时间，展开时统计该 WSL 本地日志消耗与预估总额度；
// - Antigravity 实例经目标环境官方 `agy -p "/usage" --output-format json` 读取 `Gemini Models` 与 `Claude and GPT models` 分组额度，
//   结合本地 `~/.gemini/google_accounts.json` 展示邮箱、ACP 订阅等级与本地转录日志分组统计展示底部 `5h xx% 7d xx%` 及 320px 弹层。

import { $, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { closePopup } from "../../ui/popup.js";
import { closeComposerMenus } from "../../core/actions.js";
import { connected, creating, pageLeaving, selectedId, session, sidebar, thread } from "../../core/state.js";
import { api } from "../api.js";
import { fitComposerMetrics } from "./metrics.js";
import { modelDefaults, selectedModel } from "./options-state.js";
import { isAntigravity, isClaude, privateInputScope } from "../session/capabilities.js";
import { activeInstanceId, sessionInstanceEnabled } from "../session/instances.js";
import { currentThreadDisplay } from "../session/thread-cache.js";

export let claudeUsageState = null;
function claudeUsageContext() {
  const owner = privateInputScope(session);
  if (!owner || pageLeaving || document.hidden || !creating && !selectedId) return null;
  const current = !creating && thread?.id === selectedId ? thread : null;
  const cached = !creating && !current ? currentThreadDisplay() : null;
  const display = current || (cached?.id === selectedId ? cached : null) || (!creating
    ? [...(sidebar?.recent || []), ...(sidebar?.pinnedThreads || []),
      ...(sidebar?.projects || []).flatMap(project => project.threads || [])].find(item => item.id === selectedId) : null);
  // 用户要求切换时先显示目标实例的缓存；详情未到时只认目标会话缓存/侧栏明确提供的实例，不借用旧会话或默认实例。
  const instanceId = creating || current ? activeInstanceId() : display?.instanceId;
  const antigravityActive = typeof isAntigravity === "function" && isAntigravity();
  const backend = creating || current ? (isClaude() ? "claude" : antigravityActive ? "antigravity" : "")
    : ["claude", "antigravity"].includes(display?.backend) ? display.backend : "";
  const supported = backend === "claude" ? session?.capabilities?.claudeSubscriptionUsage === true
    : backend === "antigravity" ? session?.capabilities?.antigravityUsage === true : false;
  if (!backend || !supported || !instanceId || !sessionInstanceEnabled(instanceId) || display?.viewKind === "subagent") return null;
  return {
    key: JSON.stringify([owner, instanceId]),
    owner,
    backend,
    instanceId,
    canRead: connected && (creating || !!current),
    payload: { backend, instanceId, ...(!creating ? { threadId: selectedId } : {}) }
  };
}
function claudeUsageCurrentModelGroup() {
  const model = ((typeof selectedModel === "string" && selectedModel)
    || currentThreadDisplay()?.model
    || (typeof thread === "object" && thread?.model)
    || (typeof modelDefaults === "object" && modelDefaults?.model)
    || "").trim().toLowerCase();
  return /^(?:claude-|gpt-|o1|o3|o4)/.test(model) ? "thirdParty" : "gemini";
}
function claudeUsageGroups(value) {
  return value?.source === "antigravity-native-usage" && Array.isArray(value.groups)
    ? value.groups.filter(group => group && typeof group.id === "string" && group.id) : [];
}
function claudeUsageResolveGroup(value, preferred = "") {
  const groups = claudeUsageGroups(value);
  if (!groups.length) return null;
  const modelGroup = claudeUsageCurrentModelGroup();
  return groups.find(group => group.id === preferred)
    || groups.find(group => group.id === modelGroup)
    || groups[0];
}
// 用户要求打开 Claude 会话时底部额度先显示缓存、数据又要及时刷新：每 60 秒读取一次，电脑端距上次读取超过 60 秒即经 CLI 重新读取；
// 72 小时只是原采集时间的显示上限，不是刷新周期。电脑端先返回保留结果并在后台刷新（refreshing）时，5 秒后复查以尽快换上新数据。
// 电脑端 crates/elves-harness/src/claude_usage.rs 的 USAGE_REFRESH / USAGE_RETENTION_MS 须同步核对。
const claudeUsageRefreshMs = 60000, claudeUsageFollowUpMs = 5000, claudeUsageMaxAgeMs = 72 * 60 * 60 * 1000;
// 用户要求（临时处理）：官方 `agy -p "/usage"` 频繁调用容易触发风控，Antigravity 额度不再自动轮询，
// 只在用户手动点击底部额度按钮展开弹窗时读取一次（请求带 manualRefresh，距上次读取不足 60 秒则直接用缓存）。
// 打开前没有有效缓存时按钮显示“5h --  7d --”，不伪造满额；读取失败后按钮保留，可重新点开重试。
// 弹窗保持打开期间，仅当电脑端返回 refreshing（后台仍在刷新）时才 5 秒复查，属于同一次手动点击的后续。
// 官方开放后将 antigravityUsageManualOnly 改为 false 恢复自动刷新；电脑端 antigravity/usage.rs 的 QUOTA_CLI_MANUAL_ONLY 须同步核对。
// Claude 实例不受此开关影响，仍按 60 秒周期刷新。
const antigravityUsageManualOnly = true;
function claudeUsageManualOnly(backend) {
  return backend === "antigravity" && antigravityUsageManualOnly;
}
function claudeUsageRefreshDelay(state) {
  return state?.refreshing ? claudeUsageFollowUpMs : claudeUsageRefreshMs;
}
const claudeUsageCache = new Map();
function rememberClaudeUsage(state = claudeUsageState) {
  const owner = privateInputScope(session);
  for (const [key, cached] of claudeUsageCache) {
    if (cached.owner !== owner || !claudeUsageWindows(cached.value).length) claudeUsageCache.delete(key);
  }
  if (!state) return;
  claudeUsageCache.delete(state.key);
  if (state.owner === owner && claudeUsageWindows(state.value).length) {
    // 用户要求打开时先显示缓存；仅在内存中按授权和实例保存最近有效数据，切会话/前后台不丢失。
    claudeUsageCache.set(state.key, { owner, value: state.value, checkedAt: state.checkedAt,
      refreshing: state.refreshing === true, details: state.details, detailsRead: state.detailsRead });
    // 最多保留 16 个最近使用实例，且仍受 72 小时真实采集时间约束，不保存 DOM、请求或凭据。
    while (claudeUsageCache.size > 16) claudeUsageCache.delete(claudeUsageCache.keys().next().value);
  }
}
function claudeUsageWindows(value, now = Date.now(), preferredGroup = "") {
  // 用户要求真实账号额度；缺失、过期或非法字段不能显示成 0%，窗口到期后也不推测已恢复的额度。
  if (!["claude-native-usage", "antigravity-native-usage"].includes(value?.source) || typeof value.observedAtMs !== "number"
      || !Number.isFinite(value.observedAtMs) || value.observedAtMs <= 0
      || value.observedAtMs > now + 30000 || now - value.observedAtMs >= claudeUsageMaxAgeMs) return [];
  const activeGroup = claudeUsageResolveGroup(value, preferredGroup);
  const bucketSource = activeGroup || value;
  const groupPrefix = activeGroup ? `${activeGroup.id}:` : "";
  return [["fiveHour", "5h", "5 小时"], ["sevenDay", "7d", "7 天"]].flatMap(([key, short, label]) => {
    const data = bucketSource[key], used = data?.usedPercentage;
    if (typeof used !== "number" || !Number.isFinite(used) || used < 0 || used > 100) return [];
    const reset = data.resetsAt == null ? null : typeof data.resetsAt === "string" ? Date.parse(data.resetsAt)
      : typeof data.resetsAt === "number" ? data.resetsAt * (data.resetsAt < 1e12 ? 1000 : 1) : NaN;
    if (reset != null && (!Number.isFinite(reset) || reset <= now || reset > 8640000000000000)) return [];
    // 用户要求底部和账号信息统一显示剩余比例，直接按 100 - 原生已用比例取整；预测仍使用未取整的原生已用值。
    return [{ key, detailKey: `${groupPrefix}${key}`, groupId: activeGroup?.id || "", short, label,
      used: Math.round(used), remaining: Math.round(100 - used), usedPercentage: used, resetAt: reset }];
  });
}
function claudeUsageAllWindows(value, now = Date.now()) {
  const groups = claudeUsageGroups(value);
  if (!groups.length) return claudeUsageWindows(value, now);
  return groups.flatMap(group => claudeUsageWindows(value, now, group.id));
}
function claudeUsageDefaultWindows() {
  // 用户要求真实额度返回前先显示满额占位；占位只用于当前可读取实例，不写入缓存或本地统计。
  return [["fiveHour", "5h", "5 小时"], ["sevenDay", "7d", "7 天"]].map(([key, short, label]) =>
    ({ key, short, label, used: 0, remaining: 100, usedPercentage: 0, resetAt: null, default: true }));
}
function claudeUsageDisplayWindows(state, now = Date.now()) {
  const windows = claudeUsageWindows(state?.value, now);
  if (windows.length) return windows;
  // 手动刷新模式下没有有效额度（未读取、读取中或读取失败）统一显示“--”，保证按钮可点击重试且不显示假满额。
  if (state && claudeUsageManualOnly(state.backend)) {
    return claudeUsageDefaultWindows().map(window => ({ ...window, unread: true }));
  }
  // 首次查询、缓存刷新在途或会话详情尚未完成时先显示满额；已完成的失败/权限响应不伪造额度。
  return state && (state.checkedAt === 0 || state.pending) ? claudeUsageDefaultWindows() : [];
}
function claudeUsageSubscription(value) {
  const raw = value?.subscriptionType;
  const type = typeof raw === "string" ? raw.trim() : "";
  if (/^[A-Za-z0-9][A-Za-z0-9 _+()./-]{0,63}$/u.test(type)) {
    // 用户要求等级只来自所属 WSL Claude CLI；仅统一首字母显示，不从额度比例推断等级或倍率。
    // 用户要求 AGY 额度标签省略 Google AI 前缀，只显示 Pro/Ultra 等等级；原始类型仍完整保留在缓存中。
    const display = value?.source === "antigravity-native-usage"
      ? type.replace(/^Google\s+AI(?:\s+|$)/iu, "") : type;
    return { type, label: display ? display[0].toUpperCase() + display.slice(1) : "" };
  }
  return { type: null, label: "" };
}
function claudeUsageAccount(value) {
  const email = typeof value?.accountEmail === "string" ? value.accountEmail.trim() : "";
  // 用户要求弹窗顶部显示对应 Claude 实例已登录的邮箱；缓存或旧客户端返回异常值时不直接展示。
  return email && email.length <= 320 && email.includes("@") && !/[\s\u0000-\u001f\u007f]/u.test(email) ? email : "";
}
function claudeUsageMaskAccount(account) {
  const at = account.lastIndexOf("@");
  // 用户指定默认脱敏：@ 前仅保留前 3 个字符并固定追加 3 个 *，域名保持完整。
  return at < 0 ? account : `${[...account.slice(0, at)].slice(0, 3).join("")}***${account.slice(at)}`;
}
function claudeUsageResetText(resetAt, now = Date.now()) {
  if (resetAt == null) return "时间未知";
  const totalMinutes = Math.max(1, Math.ceil(Math.max(0, resetAt - now) / 60000));
  // 用户要求将 Claude 额度重置倒计时从“x天/小时/分钟后”改为紧凑英文单位并固定在统计行右侧：
  // 满 1 天显示“xd xh”（如 1d 2h，整天无剩余小时时显示“xd”）；
  // 满 1 小时显示“xh xm”（如 3h 5m，整小时无剩余分钟时显示“xh”）；不足 1 小时显示“xm”。
  if (totalMinutes >= 1440) {
    const days = Math.floor(totalMinutes / 1440);
    const hours = Math.floor((totalMinutes % 1440) / 60);
    return hours ? `${days}d ${hours}h` : `${days}d`;
  }
  if (totalMinutes >= 60) {
    const hours = Math.floor(totalMinutes / 60);
    const minutes = totalMinutes % 60;
    return minutes ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  return `${totalMinutes}m`;
}
function claudeUsageResetCountdownText(resetAt, now = Date.now()) {
  if (resetAt == null) return "时间未知";
  const totalMinutes = Math.max(1, Math.ceil(Math.max(0, resetAt - now) / 60000));
  const days = Math.floor(totalMinutes / 1440);
  const hours = Math.floor((totalMinutes % 1440) / 60);
  const minutes = totalMinutes % 60;
  const parts = [];
  if (days) parts.push(`${days}天`);
  if (days || hours) parts.push(`${hours}小时`);
  parts.push(`${minutes}分钟`);
  return parts.join("");
}
function claudeUsageTimeZone(value) {
  const timeZone = typeof value?.timeZone === "string" ? value.timeZone.trim() : "";
  if (!timeZone || timeZone.length > 128 || !/^[A-Za-z0-9._+-]+(?:\/[A-Za-z0-9._+-]+)*$/u.test(timeZone)
      || timeZone !== "UTC" && !timeZone.includes("/")) return "";
  try {
    new Intl.DateTimeFormat("en-US", { timeZone }).format();
    return timeZone;
  } catch {
    return "";
  }
}
function claudeUsageEnvironmentTimeZone() {
  try {
    return claudeUsageTimeZone({ timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone });
  } catch {
    return "";
  }
}
function claudeUsageResetTitle(value, resetAt) {
  if (resetAt == null) return "";
  const wslTimeZone = claudeUsageTimeZone(value);
  const environmentTimeZone = claudeUsageEnvironmentTimeZone();
  if (!wslTimeZone && !(value?.source === "antigravity-native-usage" && value?.hostLocal === true && environmentTimeZone)) return "";
  try {
    const format = timeZone => new Intl.DateTimeFormat("zh-CN", {
      year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
      hour12: false, timeZone, timeZoneName: "short"
    }).format(new Date(resetAt));
    if (!wslTimeZone) {
      return `${claudeUsageResetCountdownText(resetAt)}\n${format(environmentTimeZone)}（${environmentTimeZone}）`;
    }
    // 用户要求保留 WSL 原始时区，同时补充转换到打开 Harness 的当前环境时区；两者都使用 24 小时制且不显示秒。
    const wslFormatted = format(wslTimeZone);
    const environment = environmentTimeZone
      ? `\n${format(environmentTimeZone)}（${environmentTimeZone}）` : "";
    // 用户要求 hover 在两个绝对时间上方补充精确到小时和分钟的重置倒计时。
    return `${claudeUsageResetCountdownText(resetAt)}\n${wslFormatted}（${wslTimeZone}）${environment}`;
  } catch {
    return "";
  }
}
function claudeUsageNumber(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= Number.MAX_SAFE_INTEGER;
}
function claudeUsageCompact(value, money = false) {
  if (!claudeUsageNumber(value)) return "—";
  const unit = [[1e12, "T"], [1e9, "B"], [1e6, "M"], [1e3, "k"]].find(([limit]) => value >= limit);
  const text = unit ? `${(value / unit[0]).toFixed(money || unit[0] >= 1e6 ? 2 : 1)}${unit[1]}`
    : money ? value.toFixed(2) : String(Math.round(value));
  return money ? `$${text}` : text;
}
function claudeUsageEstimatedTotal(used, usedPercentage) {
  // 用户要求预估整个窗口的总额度：已消耗 ÷ 原生已用比例；0% 或未知时不推测，不按经过时间外推。
  if (!claudeUsageNumber(used) || !claudeUsageNumber(usedPercentage) || usedPercentage <= 0 || usedPercentage > 100) return null;
  const total = used / (usedPercentage / 100);
  return claudeUsageNumber(total) ? total : null;
}
function claudeUsageMetricsMatch(metrics, window, now = Date.now()) {
  const duration = window.key === "fiveHour" ? 18000000 : 604800000;
  return ["claude-wsl-local-journal", "antigravity-local-journal"].includes(metrics?.source) && claudeUsageNumber(metrics.observedAtMs)
    && metrics.observedAtMs > 0 && metrics.observedAtMs <= now + 30000 && now - metrics.observedAtMs < claudeUsageMaxAgeMs
    && window.resetAt != null && metrics.windowEndMs === window.resetAt
    && metrics.windowStartMs === window.resetAt - duration;
}
function claudeUsageMetrics(window, now = Date.now()) {
  const detailKey = window.detailKey || window.key;
  const metrics = claudeUsageState?.details?.value?.[detailKey] ?? claudeUsageState?.details?.value?.[window.key];
  return claudeUsageMetricsMatch(metrics, window, now) ? { ...metrics,
    usedCostUsd: claudeUsageMetricValue(metrics, "usedCostUsd", "costObservedAtMs", now),
    usedTokens: claudeUsageMetricValue(metrics, "usedTokens", "tokensObservedAtMs", now) } : null;
}
function claudeUsageMetricValue(metrics, key, timeKey, now = Date.now()) {
  const at = metrics?.[timeKey] ?? metrics?.observedAtMs;
  return claudeUsageNumber(metrics?.[key]) && claudeUsageNumber(at) && at > 0
    && at <= now + 30000 && now - at < claudeUsageMaxAgeMs ? metrics[key] : null;
}
function claudeUsageSignature(windows) {
  return JSON.stringify(windows.map(window => [window.detailKey || window.key, window.resetAt]));
}
function rememberClaudeUsageDetails(state, usage, windows) {
  let received = false;
  const groups = claudeUsageGroups(usage);
  const value = Object.fromEntries(windows.map(window => {
    const detailKey = window.detailKey || window.key;
    const groupData = window.groupId ? groups.find(item => item.id === window.groupId) : null;
    const incoming = (groupData || usage)?.[window.key]?.metrics;
    const previous = state.details?.value?.[detailKey];
    const retained = claudeUsageMetricsMatch(previous, window) ? previous : null;
    const usable = claudeUsageMetricsMatch(incoming, window);
    const source = incoming?.source === "antigravity-local-journal" || retained?.source === "antigravity-local-journal"
      ? "antigravity-local-journal" : "claude-wsl-local-journal";
    const metrics = { source, windowEndMs: window.resetAt,
      windowStartMs: window.resetAt - (window.key === "fiveHour" ? 18000000 : 604800000) };
    // 用户要求使用和预测先展示缓存：金额与 Token 各自接替，单项缺失不覆盖另一项已有数字，且不延长旧值的采集时间。
    for (const [key, timeKey] of [["usedCostUsd", "costObservedAtMs"], ["usedTokens", "tokensObservedAtMs"]]) {
      const old = claudeUsageMetricValue(retained, key, timeKey);
      const oldAt = retained?.[timeKey] ?? retained?.observedAtMs;
      if (usable && claudeUsageNumber(incoming[key]) && (!claudeUsageNumber(old) || incoming.observedAtMs >= oldAt)) {
        metrics[key] = incoming[key]; metrics[timeKey] = incoming.observedAtMs; received = true;
      } else { metrics[key] = old; metrics[timeKey] = claudeUsageNumber(old) ? oldAt : null; }
    }
    metrics.observedAtMs = Math.max(metrics.costObservedAtMs || 0, metrics.tokensObservedAtMs || 0);
    return [detailKey, metrics.observedAtMs ? metrics : null];
  }));
  // 刷新失败或缺少数字时保留同窗口的有效旧值；新窗口、过期数据和迟到的较旧统计不能覆盖当前值。
  state.details = Object.values(value).some(Boolean)
    ? { signature: claudeUsageSignature(windows), value } : null;
  return received;
}
function loadClaudeUsageDetails() {
  const state = claudeUsageState, context = claudeUsageContext();
  if (!state?.tip || state.key !== context?.key || !context.canRead || state.detailsPending || state.pending) return;
  const allWindows = claudeUsageAllWindows(state.value);
  const resettableWindows = allWindows.filter(window => window.resetAt != null);
  const signature = claudeUsageSignature(resettableWindows);
  if (!resettableWindows.length || state.detailsRead?.signature === signature
      && Date.now() - state.detailsRead.checkedAt < claudeUsageRefreshMs) return;
  const tip = state.tip, controller = new AbortController();
  state.detailsPending = true; state.detailsController = controller;
  // 用户要求仅展开时扫描本地日志；关闭、切换授权/实例后丢弃迟到结果，详情失败不影响百分比。
  void api("/api/read/claudeUsage", { ...context.payload, includeLocal: true }, undefined, controller.signal,
    { resourcePriority: "background" })
    .then(result => {
      if (claudeUsageState !== state || state.tip !== tip || state.detailsController !== controller) return;
      if (result?.ok !== true || result.backend !== context.backend || result.instanceId !== context.instanceId)
        throw new Error("额度详情响应无效");
      rememberClaudeUsageDetails(state, result.usage, claudeUsageAllWindows(state.value));
    })
    .catch(error => {
      if (claudeUsageState !== state || state.tip !== tip || state.detailsController !== controller) return;
      if ([401, 403].includes(error.code)) {
        state.value = null; state.details = null; claudeUsageCache.delete(state.key);
      }
    })
    .finally(() => {
      if (claudeUsageState !== state || state.detailsController !== controller) return;
      state.detailsPending = false; state.detailsController = null;
      state.detailsRead = { signature, checkedAt: Date.now() };
      rememberClaudeUsage(state);
      if (state.tip === tip) {
        const current = claudeUsageAllWindows(state.value).filter(window => window.resetAt != null);
        renderClaudeUsage();
        if (claudeUsageSignature(current) !== signature) loadClaudeUsageDetails();
      }
      scheduleClaudeUsageUpdate();
    });
}
export function closeClaudeUsageTip() {
  const tip = claudeUsageState?.tip;
  if (!tip) return;
  claudeUsageState.tip = null;
  claudeUsageState.detailsController?.abort();
  claudeUsageState.detailsController = null; claudeUsageState.detailsPending = false;
  tip.trigger.setAttribute("aria-expanded", "false");
  tip.trigger.removeAttribute("aria-describedby");
  if (tip.node.hasAttribute("popover") && tip.node.matches(":popover-open")) tip.node.hidePopover();
  tip.node.remove();
}
function pauseClaudeUsage(state) {
  clearTimeout(state.timer);
  state.controller?.abort(); state.detailsController?.abort();
  state.timer = state.controller = state.detailsController = null;
  state.pending = state.detailsPending = false;
}
export function resetClaudeUsage({ clearCache = false, hideTrigger = true } = {}) {
  if (clearCache) claudeUsageCache.clear();
  else rememberClaudeUsage();
  closeClaudeUsageTip();
  if (claudeUsageState) pauseClaudeUsage(claudeUsageState);
  claudeUsageState = null;
  const trigger = $("#claude-usage");
  if (trigger && hideTrigger) trigger.hidden = true;
}
export function fitClaudeUsageTip() {
  const tip = claudeUsageState?.tip;
  if (!tip) return;
  if (!tip.trigger.isConnected || tip.trigger.hidden) { closeClaudeUsageTip(); return; }
  const viewport = window.visualViewport, padding = 8, gap = 4;
  const left = viewport?.offsetLeft || 0, top = viewport?.offsetTop || 0;
  const width = viewport?.width || window.innerWidth, height = viewport?.height || window.innerHeight;
  const box = tip.trigger.getBoundingClientRect();
  if (box.bottom <= top || box.top >= top + height || box.right <= left || box.left >= left + width) {
    closeClaudeUsageTip(); return;
  }
  // 用户要求弹层总宽固定 320px（含两侧各 14px 内边距与 1px 边框），不随内容扩展；
  // 仅在可视区域不足 336px 时由 maxWidth 收窄，保留两侧各 8px 防越界间距。
  tip.node.style.width = "320px";
  tip.node.style.maxWidth = `${Math.max(0, width - padding * 2)}px`;
  const above = Math.max(0, box.top - top - gap - padding);
  const below = Math.max(0, top + height - box.bottom - gap - padding);
  const upwards = above >= tip.node.getBoundingClientRect().height || above >= below;
  tip.node.style.maxHeight = `${Math.min(height * .8, upwards ? above : below)}px`;
  const actual = tip.node.getBoundingClientRect();
  tip.node.style.left = `${Math.max(left + padding, Math.min(box.right - actual.width, left + width - actual.width - padding))}px`;
  tip.node.style.top = `${Math.max(top + padding, Math.min(upwards ? box.top - gap - actual.height : box.bottom + gap,
    top + height - actual.height - padding))}px`;
}
function renderClaudeUsageTip(defaultOrBarWindows) {
  const tip = claudeUsageState?.tip;
  if (!tip) return;
  const value = claudeUsageState.value;
  const isAntigravityUsage = value?.source === "antigravity-native-usage";
  const groups = claudeUsageGroups(value);
  const activeGroup = groups.length ? claudeUsageResolveGroup(value, tip.selectedGroup) : null;
  const windows = activeGroup ? claudeUsageWindows(value, Date.now(), activeGroup.id) : defaultOrBarWindows;
  if (!windows.length) { closeClaudeUsageTip(); return; }
  const subscription = claudeUsageSubscription(value);
  const account = claudeUsageAccount(value);
  const title = element("div", "claude-usage-title");
  // 用户要求邮箱默认脱敏；同次弹层刷新保留眼睛状态，账号变化或重新打开时恢复脱敏。
  if (tip.account !== account) { tip.account = account; tip.accountVisible = false; }
  // 没有邮箱时继续显示“账号信息”，不增加无作用的大眼睛按钮。
  const titleText = element("span", "claude-usage-title-text", account ? "" : "账号信息");
  title.append(titleText);
  let reveal = null;
  if (account) {
    titleText.id = "claude-usage-account-email";
    reveal = element("button", "claude-usage-account-toggle");
    reveal.type = "button";
    reveal.setAttribute("aria-controls", titleText.id);
    const updateAccount = () => {
      const visible = tip.accountVisible === true;
      const displayed = visible ? account : claudeUsageMaskAccount(account);
      const action = visible ? "隐藏完整邮箱" : "显示完整邮箱";
      titleText.textContent = displayed; titleText.title = displayed;
      titleText.setAttribute("aria-label", `登录账号：${displayed}`);
      reveal.title = action; reveal.setAttribute("aria-label", action);
      reveal.setAttribute("aria-pressed", visible ? "true" : "false");
      reveal.replaceChildren(icon(visible ? "eyeOff" : "eye"));
    };
    reveal.onclick = () => {
      if (claudeUsageState?.tip !== tip) return;
      tip.accountVisible = !tip.accountVisible; updateAccount();
    };
    updateAccount();
  }
  // 用户要求眼睛紧跟账号文本；订阅等级继续显示在账号行右侧。
  if (reveal) title.append(reveal);
  // 用户确认缺少等级时直接不显示，不添加未知标签、占位或说明，也不使用效果图里的示例等级补齐。
  if (subscription.label) {
    const badge = element("span", "claude-usage-subscription", subscription.label);
    badge.title = isAntigravityUsage
      ? `所属 Antigravity 实例官方返回的订阅等级：${subscription.label}`
      : `所属 WSL Claude CLI 返回的订阅类型：${subscription.type}`;
    badge.setAttribute("aria-label", `订阅等级：${subscription.label}`); title.append(badge);
  }
  const content = [title];
  if (groups.length > 1 && activeGroup) {
    const tabs = element("div", "claude-usage-groups");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "模型额度分组");
    for (const group of groups) {
      const selected = group.id === activeGroup.id;
      const tab = element("button", "claude-usage-group-tab", group.label || group.name || group.id);
      tab.type = "button";
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-selected", selected ? "true" : "false");
      if (group.description) tab.title = group.description;
      tab.onclick = () => {
        if (claudeUsageState?.tip !== tip) return;
        tip.selectedGroup = group.id;
        renderClaudeUsageTip(defaultOrBarWindows);
      };
      tabs.append(tab);
    }
    content.push(tabs);
  }
  const localLabel = isAntigravityUsage ? "仅此实例本地原生转录日志" : "仅此 WSL 本地原生日志";
  for (const window of windows) {
    const section = element("section", "claude-usage-section");
    const row = element("div", "claude-usage-detail-row");
    row.append(element("span", "", `${window.label}额度`),
      element("span", "claude-usage-remaining", window.unread ? "剩余 --" : `剩余 ${window.remaining}%`));
    const track = element("div", "claude-usage-track"), fill = element("div", "claude-usage-fill");
    track.setAttribute("aria-hidden", "true"); fill.style.width = window.unread ? "0%" : `${window.remaining}%`; track.append(fill);
    const metrics = claudeUsageMetrics(window), numbers = element("div", "claude-usage-metrics");
    const stats = element("div", "claude-usage-stats");
    for (const estimated of [false, true]) {
      const item = element("span", `claude-usage-metric ${estimated ? "claude-usage-forecast" : "claude-usage-consumed"}`);
      const cost = estimated ? claudeUsageEstimatedTotal(metrics?.usedCostUsd, window.usedPercentage) : metrics?.usedCostUsd;
      const tokens = estimated ? claudeUsageEstimatedTotal(metrics?.usedTokens, window.usedPercentage) : metrics?.usedTokens;
      item.title = `${estimated ? "预估总额度（已消耗 ÷ 原生已用比例）" : "本窗口已消耗"}；${localLabel}；金额按公开模型价格估算`;
      item.setAttribute("aria-label", `${estimated ? "预估总额度" : "已消耗"} ${claudeUsageCompact(cost, true)} / ${claudeUsageCompact(tokens)} Token`);
      item.append(icon(estimated ? "usageForecast" : "usageConsumed"),
        element("span", "", claudeUsageCompact(cost, true)), element("span", "claude-usage-slash", "/"),
        element("span", "", claudeUsageCompact(tokens)));
      stats.append(item);
    }
    const unstarted = isAntigravityUsage && window.resetAt == null && window.usedPercentage === 0 && !window.default;
    const reset = element("span", "claude-usage-reset", window.unread ? (claudeUsageState?.pending ? "读取中" : "读取失败，可重新点开重试")
      : unstarted ? "未开始" : claudeUsageResetText(window.resetAt));
    const resetTitle = claudeUsageResetTitle(value, window.resetAt);
    if (resetTitle) {
      reset.title = resetTitle;
      reset.setAttribute("aria-label", `重置时间：${resetTitle}`);
    } else if (unstarted) {
      reset.title = "当前分组尚未消耗额度，滚动计时窗口将在首次使用后启动";
      reset.setAttribute("aria-label", "重置时间：尚未开始计时");
    } else if (window.resetAt != null) {
      // 未能核对目标 WSL 时区时不能退回浏览器本地时间，避免显示看似精确但来源错误的时间。
      reset.title = "目标 WSL 时区未知";
      reset.setAttribute("aria-label", "重置时间：目标 WSL 时区未知");
    }
    numbers.append(stats, reset); section.append(row, track, numbers); content.push(section);
  }
  tip.node.replaceChildren(...content);
  fitClaudeUsageTip();
}
function renderClaudeUsage() {
  const trigger = $("#claude-usage");
  if (!trigger) return;
  const context = claudeUsageContext();
  const windows = claudeUsageDisplayWindows(claudeUsageState);
  trigger.hidden = !windows.length;
  fitComposerMetrics();
  if (!windows.length) { closeClaudeUsageTip(); return; }
  const isDefault = windows[0]?.default === true;
  const isUnread = windows[0]?.unread === true;
  const activeGroup = claudeUsageResolveGroup(claudeUsageState?.value);
  const signature = JSON.stringify([isUnread ? "unread" : isDefault ? "default" : "remaining", activeGroup?.id || "",
    ...windows.map(window => [window.key, window.remaining])]);
  if (trigger.dataset.usage !== signature) {
    trigger.dataset.usage = signature;
    // 用户要求底部额度按“5h xx%  7d xx%”紧凑展示，去掉“剩余”前缀和窗口之间的圆点分隔符。
    const nodes = [];
    for (const window of windows) {
      const row = element("span", "claude-usage-window");
      row.append(element("span", "", window.short), element("span", "claude-usage-percent", window.unread ? "--" : `${window.remaining}%`));
      nodes.push(row);
    }
    trigger.replaceChildren(...nodes);
    const account = claudeUsageAccount(claudeUsageState.value);
    const brandName = context?.backend === "antigravity" ? "Antigravity" : "Claude";
    const groupText = activeGroup ? `${activeGroup.label} 分组，` : "";
    // 用户要求默认脱敏同时覆盖底部按钮的读屏文本，不能只在弹层视觉上隐藏完整邮箱。
    trigger.setAttribute("aria-label", isUnread ? `${brandName} 额度尚未读取，点击读取`
      : `${brandName} ${account ? `登录账号 ${claudeUsageMaskAccount(account)}，` : "账号信息，"}${groupText}${isDefault ? "真实额度读取中，当前按满额显示，" : ""}${windows.map(window => `${window.short} ${window.remaining}%`).join("，")}，查看重置时间`);
  }
  renderClaudeUsageTip(windows);
  fitComposerMetrics();
}
function scheduleClaudeUsageUpdate() {
  const state = claudeUsageState;
  if (!state) return;
  clearTimeout(state.timer);
  const context = claudeUsageContext();
  if (context?.key !== state.key) return;
  const now = Date.now();
  const windows = claudeUsageAllWindows(state.value, now);
  const expiry = [state.value?.observedAtMs, ...(state.tip
    ? Object.values(state.details?.value || {}).flatMap(metrics =>
      [metrics?.observedAtMs, metrics?.costObservedAtMs, metrics?.tokensObservedAtMs]) : [])]
    .filter(claudeUsageNumber).map(at => at + claudeUsageMaxAgeMs).filter(at => at > now);
  // 刷新和 72 小时数据失效分别计时；失败退避不能延长真实数据寿命，窗口到期也不能继续显示旧额度。
  // 手动刷新模式下不排自动刷新；仅弹窗打开且电脑端仍在后台刷新时才复查（见文件顶部 antigravityUsageManualOnly 说明）。
  const autoRefresh = !claudeUsageManualOnly(state.backend) || (state.refreshing === true && !!state.tip);
  const deadlines = [...(context.canRead && !state.pending && autoRefresh ? [state.checkedAt + claudeUsageRefreshDelay(state)] : []), ...expiry,
    ...windows.filter(window => window.resetAt != null).map(window => window.resetAt)];
  if (!deadlines.length) { state.timer = null; return; }
  state.timer = setTimeout(() => {
    if (claudeUsageState !== state) return;
    state.timer = null; syncClaudeUsage();
  }, Math.max(250, Math.min(...deadlines) - now + 5));
}
export function syncClaudeUsage({ manual = false } = {}) {
  if (!$("#claude-usage")) return;
  const context = claudeUsageContext();
  if (!context) { resetClaudeUsage(); return; }
  if (claudeUsageState?.key !== context.key) {
    // 切换实例时原子替换数据并直接渲染目标缓存，不能先 hidden=true 再等待会话详情。
    resetClaudeUsage({ hideTrigger: false });
    const cached = claudeUsageCache.get(context.key);
    claudeUsageState = { key: context.key, owner: context.owner, backend: context.backend, value: cached?.value || null,
      checkedAt: cached?.checkedAt || 0, refreshing: cached?.refreshing === true, pending: false, controller: null, timer: null, tip: null,
      details: cached?.details || null, detailsRead: cached?.detailsRead || null, detailsPending: false, detailsController: null };
  }
  const state = claudeUsageState;
  if (!context.canRead) pauseClaudeUsage(state);
  renderClaudeUsage();
  if (!context.canRead) { scheduleClaudeUsageUpdate(); return; }
  // 手动刷新模式：只有用户点击额度按钮（manual）或同一次点击触发的后台刷新复查（refreshing 且弹窗打开）才读取。
  const manualOnly = claudeUsageManualOnly(context.backend);
  const mayRead = !manualOnly || manual || (state.refreshing === true && !!state.tip);
  if (mayRead && !state.pending && Date.now() - state.checkedAt >= claudeUsageRefreshDelay(state)) {
    state.pending = true;
    const controller = new AbortController();
    state.controller = controller;
    // 用户要求额度按账号所属实例显示；授权或实例切换后丢弃迟到响应，后台页不轮询。
    void api("/api/read/claudeUsage", manualOnly ? { ...context.payload, manualRefresh: true } : context.payload,
      undefined, controller.signal, { resourcePriority: "background" })
      .then(result => {
        if (claudeUsageState !== state || state.controller !== controller || controller.signal.aborted) return;
        if (result?.ok !== true || result.backend !== context.backend || result.instanceId !== context.instanceId)
          throw new Error("额度响应无效");
        // 电脑端先返回保留结果并在后台经 CLI 刷新时按 5 秒复查，新结果到达后恢复 60 秒周期。
        state.refreshing = result.refreshing === true;
        const value = result.usage, windows = claudeUsageAllWindows(value);
        // 读取暂未返回有效窗口时保留该实例仍有效的快照；采集时间不更新，超时/重置仍按原边界失效。
        if (!windows.length && claudeUsageAllWindows(state.value).length) return;
        const account = claudeUsageAccount(value);
        const timeZone = claudeUsageTimeZone(value);
        const rawGroups = claudeUsageGroups(value);
        const normalizedGroups = rawGroups.map(group => {
          const gWindows = claudeUsageWindows(value, Date.now(), group.id);
          if (!gWindows.length) return null;
          return {
            id: group.id,
            label: typeof group.label === "string" ? group.label.slice(0, 32) : group.id,
            name: typeof group.name === "string" ? group.name.slice(0, 64) : "",
            description: typeof group.description === "string" ? group.description.slice(0, 240) : "",
            ...Object.fromEntries(gWindows.map(window => [window.key,
              { usedPercentage: window.usedPercentage, resetsAt: window.resetAt }]))
          };
        }).filter(Boolean);
        const primaryWindows = claudeUsageWindows(value);
        state.value = windows.length ? { source: value.source, observedAtMs: value.observedAtMs,
          ...(value?.hostLocal === true ? { hostLocal: true } : {}),
          ...(timeZone ? { timeZone } : {}),
          ...(account ? { accountEmail: account } : {}),
          ...(Object.hasOwn(value, "subscriptionType") ? { subscriptionType: claudeUsageSubscription(value).type } : {}),
          ...(normalizedGroups.length ? { groups: normalizedGroups } : {}),
          ...Object.fromEntries(primaryWindows.map(window => [window.key,
            { usedPercentage: window.usedPercentage, resetsAt: window.resetAt }])) } : null;
        const resettableWindows = windows.filter(window => window.resetAt != null);
        const received = rememberClaudeUsageDetails(state, value, resettableWindows);
        if (received && resettableWindows.length && resettableWindows.every(window =>
            claudeUsageMetricsMatch(state.details?.value?.[window.detailKey || window.key], window))) {
          const observed = Math.min(Date.now(), ...Object.values(state.details.value).filter(Boolean).map(metrics => metrics.observedAtMs));
          state.detailsRead = { signature: claudeUsageSignature(resettableWindows),
            checkedAt: Math.max(observed, state.detailsRead?.signature === claudeUsageSignature(resettableWindows) ? state.detailsRead.checkedAt : 0) };
        }
      })
      .catch(error => {
        if (claudeUsageState !== state || state.controller !== controller) return;
        // 请求失败或响应无效时不按刷新在途提前复查，回到 60 秒退避。
        state.refreshing = false;
        if ([401, 403].includes(error.code)) {
          state.value = null; state.details = null; claudeUsageCache.delete(state.key);
        }
      })
      .finally(() => {
        if (claudeUsageState !== state || state.controller !== controller) return;
        state.checkedAt = Date.now(); state.pending = false; state.controller = null;
        rememberClaudeUsage(state);
        renderClaudeUsage(); loadClaudeUsageDetails(); scheduleClaudeUsageUpdate();
      });
  }
  loadClaudeUsageDetails();
  scheduleClaudeUsageUpdate();
}
function showClaudeUsageTip() {
  // 用户点击额度按钮是 Antigravity 额度的唯一手动刷新入口。
  syncClaudeUsage({ manual: true });
  const state = claudeUsageState, trigger = $("#claude-usage");
  if (!trigger?.isConnected || trigger.hidden || !state) return;
  const windows = claudeUsageDisplayWindows(state);
  if (!windows.length) return;
  if (state.tip) {
    renderClaudeUsageTip(windows); loadClaudeUsageDetails(); return;
  }
  closePopup(); closeComposerMenus();
  const node = element("div", "ui-popup claude-usage-tooltip");
  node.id = "claude-usage-tooltip"; node.setAttribute("role", "tooltip");
  if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
  state.tip = { trigger, node, account: "", accountVisible: false, selectedGroup: "" };
  trigger.setAttribute("aria-expanded", "true"); trigger.setAttribute("aria-describedby", node.id);
  document.body.append(node);
  // 用户要求缓存优先：先构造完整额度与已缓存数字，再展示弹层，刷新在后台进行。
  renderClaudeUsageTip(windows);
  if (node.hasAttribute("popover")) node.showPopover();
  fitClaudeUsageTip();
  loadClaudeUsageDetails();
}
export function mountClaudeUsage() {
  const trigger = $("#claude-usage");
  // 用户要求仅点击展开/收起，悬停和聚焦不打开；键盘通过按钮原生 Enter/空格激活。
  trigger.onclick = () => {
    if (claudeUsageState?.tip) closeClaudeUsageTip();
    else showClaudeUsageTip();
  };
  trigger.onpointerdown = event => {
    if (event.isPrimary && event.button === 0 && document.activeElement === $("#prompt")) event.preventDefault();
  };
}
