// Claude 与 Antigravity 账号/额度提示：Claude 经所属 WSL CLI 展示 5h/7d 额度、订阅与重置时间；
// Antigravity 严格经官方 ACP 协议展示官方订阅等级、CLI 状态与 ACP 会话 Token 统计（不绕过 CLI）。

import { $, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { closePopup } from "../../ui/popup.js";
import { closeComposerMenus } from "../../core/actions.js";
import { connected, creating, pageLeaving, selectedId, session, sidebar, thread } from "../../core/state.js";
import { api } from "../api.js";
import { fitComposerMetrics } from "./metrics.js";
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
  const backend = creating || current
    ? (isClaude() ? "claude" : isAntigravity() ? "antigravity" : "")
    : (display?.backend === "claude" || display?.backend === "antigravity" ? display.backend : "");
  if (!backend || !instanceId || !sessionInstanceEnabled(instanceId) || display?.viewKind === "subagent") return null;
  if (backend === "claude" && session?.capabilities?.claudeSubscriptionUsage !== true) return null;
  if (backend === "antigravity" && session?.capabilities?.antigravityUsage !== true) return null;
  return {
    key: JSON.stringify([owner, backend, instanceId]),
    backend,
    owner,
    instanceId,
    canRead: connected && (creating || !!current),
    payload: { backend, instanceId, ...(!creating ? { threadId: selectedId } : {}) }
  };
}
// 用户要求打开 Claude 会话时底部额度先显示缓存、数据又要及时刷新：每 60 秒读取一次，电脑端距上次读取超过 60 秒即经 CLI 重新读取；
// 72 小时只是原采集时间的显示上限，不是刷新周期。电脑端先返回保留结果并在后台刷新（refreshing）时，5 秒后复查以尽快换上新数据。
// 电脑端 crates/elves-harness/src/claude_usage.rs 的 USAGE_REFRESH / USAGE_RETENTION_MS 须同步核对。
const claudeUsageRefreshMs = 60000, claudeUsageFollowUpMs = 5000, claudeUsageMaxAgeMs = 72 * 60 * 60 * 1000;
function claudeUsageRefreshDelay(state) {
  return state?.refreshing ? claudeUsageFollowUpMs : claudeUsageRefreshMs;
}
const claudeUsageCache = new Map();
function antigravityUsageUsable(value, now = Date.now()) {
  return value?.source === "antigravity-acp" && typeof value.observedAtMs === "number"
    && Number.isFinite(value.observedAtMs) && value.observedAtMs > 0
    && value.observedAtMs <= now + 30000 && now - value.observedAtMs < claudeUsageMaxAgeMs;
}
function hasCachedUsage(entry, now = Date.now()) {
  return entry?.backend === "antigravity" ? antigravityUsageUsable(entry.value, now) : claudeUsageWindows(entry?.value, now).length > 0;
}
function rememberClaudeUsage(state = claudeUsageState) {
  const owner = privateInputScope(session);
  for (const [key, cached] of claudeUsageCache) {
    if (cached.owner !== owner || !hasCachedUsage(cached)) claudeUsageCache.delete(key);
  }
  if (!state) return;
  claudeUsageCache.delete(state.key);
  if (state.owner === owner && hasCachedUsage(state)) {
    // 用户要求打开时先显示缓存；仅在内存中按授权和实例保存最近有效数据，切会话/前后台不丢失。
    claudeUsageCache.set(state.key, { owner, backend: state.backend, value: state.value, checkedAt: state.checkedAt,
      refreshing: state.refreshing === true, details: state.details, detailsRead: state.detailsRead });
    // 最多保留 16 个最近使用实例，且仍受 72 小时真实采集时间约束，不保存 DOM、请求或凭据。
    while (claudeUsageCache.size > 16) claudeUsageCache.delete(claudeUsageCache.keys().next().value);
  }
}
function claudeUsageWindows(value, now = Date.now()) {
  // 用户要求真实账号额度；缺失、过期或非法字段不能显示成 0%，窗口到期后也不推测已恢复的额度。
  if (value?.source !== "claude-native-usage" || typeof value.observedAtMs !== "number"
      || !Number.isFinite(value.observedAtMs) || value.observedAtMs <= 0
      || value.observedAtMs > now + 30000 || now - value.observedAtMs >= claudeUsageMaxAgeMs) return [];
  return [["fiveHour", "5h", "5 小时"], ["sevenDay", "7d", "7 天"]].flatMap(([key, short, label]) => {
    const data = value[key], used = data?.usedPercentage;
    if (typeof used !== "number" || !Number.isFinite(used) || used < 0 || used > 100) return [];
    const reset = data.resetsAt == null ? null : typeof data.resetsAt === "string" ? Date.parse(data.resetsAt)
      : typeof data.resetsAt === "number" ? data.resetsAt * (data.resetsAt < 1e12 ? 1000 : 1) : NaN;
    if (reset != null && (!Number.isFinite(reset) || reset <= now || reset > 8640000000000000)) return [];
    // 用户要求底部和账号信息统一显示剩余比例，直接按 100 - 原生已用比例取整；预测仍使用未取整的原生已用值。
    return [{ key, short, label, used: Math.round(used), remaining: Math.round(100 - used), usedPercentage: used, resetAt: reset }];
  });
}
function claudeUsageDefaultWindows() {
  // 用户要求真实额度返回前先显示满额占位；占位只用于当前可读取实例，不写入缓存或本地统计。
  return [["fiveHour", "5h", "5 小时"], ["sevenDay", "7d", "7 天"]].map(([key, short, label]) =>
    ({ key, short, label, used: 0, remaining: 100, usedPercentage: 0, resetAt: null, default: true }));
}
function claudeUsageDisplayWindows(state, now = Date.now()) {
  const windows = claudeUsageWindows(state?.value, now);
  if (windows.length) return windows;
  // 首次查询、缓存刷新在途或会话详情尚未完成时先显示满额；已完成的失败/权限响应不伪造额度。
  return state && (state.checkedAt === 0 || state.pending) ? claudeUsageDefaultWindows() : [];
}
function claudeUsageSubscription(value) {
  const raw = value?.subscriptionType;
  const type = typeof raw === "string" ? raw.trim() : "";
  if (/^[A-Za-z0-9][A-Za-z0-9 _+()./-]{0,63}$/u.test(type)) {
    // 用户要求等级只来自所属 WSL Claude CLI；仅统一首字母显示，不从额度比例推断等级或倍率。
    return { type, label: type[0].toUpperCase() + type.slice(1) };
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
  if (!wslTimeZone) return "";
  try {
    const format = timeZone => new Intl.DateTimeFormat("zh-CN", {
      year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
      hour12: false, timeZone, timeZoneName: "short"
    }).format(new Date(resetAt));
    // 用户要求保留 WSL 原始时区，同时补充转换到打开 Harness 的当前环境时区；两者都使用 24 小时制且不显示秒。
    const wslFormatted = format(wslTimeZone);
    const environmentTimeZone = claudeUsageEnvironmentTimeZone();
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
  return metrics?.source === "claude-wsl-local-journal" && claudeUsageNumber(metrics.observedAtMs)
    && metrics.observedAtMs > 0 && metrics.observedAtMs <= now + 30000 && now - metrics.observedAtMs < claudeUsageMaxAgeMs
    && window.resetAt != null && metrics.windowEndMs === window.resetAt
    && metrics.windowStartMs === window.resetAt - duration;
}
function claudeUsageMetrics(window, now = Date.now()) {
  const metrics = claudeUsageState?.details?.value?.[window.key];
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
  return JSON.stringify(windows.map(window => [window.key, window.resetAt]));
}
function rememberClaudeUsageDetails(state, usage, windows) {
  let received = false;
  const value = Object.fromEntries(windows.map(window => {
    const incoming = usage?.[window.key]?.metrics, previous = state.details?.value?.[window.key];
    const retained = claudeUsageMetricsMatch(previous, window) ? previous : null;
    const usable = claudeUsageMetricsMatch(incoming, window);
    const metrics = { source: "claude-wsl-local-journal", windowEndMs: window.resetAt,
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
    return [window.key, metrics.observedAtMs ? metrics : null];
  }));
  // 刷新失败或缺少数字时保留同窗口的有效旧值；新窗口、过期数据和迟到的较旧统计不能覆盖当前值。
  state.details = Object.values(value).some(Boolean)
    ? { signature: claudeUsageSignature(windows), value } : null;
  return received;
}
function loadClaudeUsageDetails() {
  const state = claudeUsageState, context = claudeUsageContext();
  if (!state?.tip || state.backend !== "claude" || state.key !== context?.key || !context.canRead || state.detailsPending || state.pending) return;
  const windows = claudeUsageWindows(state.value);
  const signature = claudeUsageSignature(windows);
  if (!windows.length || state.detailsRead?.signature === signature
      && Date.now() - state.detailsRead.checkedAt < claudeUsageRefreshMs) return;
  const tip = state.tip, controller = new AbortController();
  state.detailsPending = true; state.detailsController = controller;
  // 用户要求仅展开时扫描 WSL 日志；关闭、切换授权/实例后丢弃迟到结果，详情失败不影响百分比。
  void api("/api/read/claudeUsage", { ...context.payload, includeLocal: true }, undefined, controller.signal,
    { resourcePriority: "background" })
    .then(result => {
      if (claudeUsageState !== state || state.tip !== tip || state.detailsController !== controller) return;
      if (result?.ok !== true || result.backend !== "claude" || result.instanceId !== context.instanceId)
        throw new Error("Claude 额度详情响应无效");
      rememberClaudeUsageDetails(state, result.usage, claudeUsageWindows(state.value));
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
        const current = claudeUsageWindows(state.value);
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
function renderClaudeUsageTip(windows) {
  const tip = claudeUsageState?.tip;
  if (!tip) return;
  if (!windows.length) { closeClaudeUsageTip(); return; }
  const subscription = claudeUsageSubscription(claudeUsageState.value);
  const account = claudeUsageAccount(claudeUsageState.value);
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
  if (subscription.type) {
    const badge = element("span", "claude-usage-subscription", subscription.label);
    badge.title = `所属 WSL Claude CLI 返回的订阅类型：${subscription.type}`;
    badge.setAttribute("aria-label", `订阅等级：${subscription.label}`); title.append(badge);
  }
  const content = [title];
  for (const window of windows) {
    const section = element("section", "claude-usage-section");
    const row = element("div", "claude-usage-detail-row");
    row.append(element("span", "", `${window.label}额度`),
      element("span", "claude-usage-remaining", `剩余 ${window.remaining}%`));
    const track = element("div", "claude-usage-track"), fill = element("div", "claude-usage-fill");
    track.setAttribute("aria-hidden", "true"); fill.style.width = `${window.remaining}%`; track.append(fill);
    const metrics = claudeUsageMetrics(window), numbers = element("div", "claude-usage-metrics");
    const stats = element("div", "claude-usage-stats");
    for (const estimated of [false, true]) {
      const item = element("span", `claude-usage-metric ${estimated ? "claude-usage-forecast" : "claude-usage-consumed"}`);
      const cost = estimated ? claudeUsageEstimatedTotal(metrics?.usedCostUsd, window.usedPercentage) : metrics?.usedCostUsd;
      const tokens = estimated ? claudeUsageEstimatedTotal(metrics?.usedTokens, window.usedPercentage) : metrics?.usedTokens;
      item.title = `${estimated ? "预估总额度（已消耗 ÷ 原生已用比例）" : "本窗口已消耗"}；仅此 WSL 本地原生日志；金额按公开模型价格估算`;
      item.setAttribute("aria-label", `${estimated ? "预估总额度" : "已消耗"} ${claudeUsageCompact(cost, true)} / ${claudeUsageCompact(tokens)} Token`);
      item.append(icon(estimated ? "usageForecast" : "usageConsumed"),
        element("span", "", claudeUsageCompact(cost, true)), element("span", "claude-usage-slash", "/"),
        element("span", "", claudeUsageCompact(tokens)));
      stats.append(item);
    }
    const reset = element("span", "claude-usage-reset", claudeUsageResetText(window.resetAt));
    const resetTitle = claudeUsageResetTitle(claudeUsageState.value, window.resetAt);
    if (resetTitle) {
      reset.title = resetTitle;
      reset.setAttribute("aria-label", `重置时间：${resetTitle}`);
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
function antigravityUsageTriggerView(state, now = Date.now()) {
  if (!state || state.backend !== "antigravity") return null;
  const value = antigravityUsageUsable(state.value, now) ? state.value : null;
  if (value) {
    if (value.authRequired === true) {
      return { text: "未登录", warning: true, label: "Antigravity 未登录，点击查看 ACP 账号与登录说明" };
    }
    const tier = typeof (value.tierName || value.subscriptionType || value.subscriptionTier?.name) === "string"
      ? (value.tierName || value.subscriptionType || value.subscriptionTier?.name).trim() : "";
    if (tier) {
      return { text: tier, warning: false, label: `Antigravity 订阅等级 ${tier}，点击查看 ACP 账号与统计` };
    }
    if (value.authenticated === true) {
      return { text: "已登录", warning: false, label: "Antigravity 已通过 ACP 登录，点击查看统计" };
    }
  }
  if (state.checkedAt === 0 || state.pending) {
    return { text: "Antigravity", warning: false, label: "正在通过 ACP 读取 Antigravity 账号状态" };
  }
  return null;
}
function renderAntigravityUsageTip() {
  const tip = claudeUsageState?.tip;
  if (!tip) return;
  const view = antigravityUsageTriggerView(claudeUsageState);
  if (!view) { closeClaudeUsageTip(); return; }
  const value = antigravityUsageUsable(claudeUsageState.value) ? claudeUsageState.value : null;
  const title = element("div", "claude-usage-title");
  const heading = typeof value?.instanceLabel === "string" && value.instanceLabel.trim()
    ? value.instanceLabel.trim() : "Antigravity";
  const titleText = element("span", "claude-usage-title-text", heading);
  title.append(titleText);
  const rawTier = value?.tierName || value?.subscriptionType || value?.subscriptionTier?.name || "";
  const tier = typeof rawTier === "string" ? rawTier.trim() : "";
  if (tier) {
    const badge = element("span", "claude-usage-subscription", tier);
    badge.title = `Antigravity 官方 ACP 返回的订阅等级：${tier}`;
    badge.setAttribute("aria-label", `订阅等级：${tier}`);
    title.append(badge);
  } else if (value?.authRequired === true) {
    const badge = element("span", "claude-usage-subscription is-warning", "未登录");
    badge.title = "目标 Antigravity 实例尚未通过 ACP 登录";
    title.append(badge);
  }
  const content = [title];
  // 区块 1：ACP 连接与账号状态
  const statusSection = element("section", "claude-usage-section");
  const statusRow = element("div", "claude-usage-detail-row");
  const rawVersion = value?.agentVersion || value?.agentInfo?.version || "";
  const version = typeof rawVersion === "string" && rawVersion.trim() ? `v${rawVersion.trim()}` : "";
  const statusValue = !value ? "正在读取 ACP 状态…"
    : value.authRequired === true ? "未登录"
    : value.authenticated === true ? (version ? `已登录 · ${version}` : "已登录")
    : "暂不可用";
  statusRow.append(element("span", "", "ACP 连接状态"), element("span", "claude-usage-remaining", statusValue));
  statusSection.append(statusRow);
  const authNote = value?.authMessage || value?.error || "";
  if (value?.authRequired === true && typeof authNote === "string" && authNote.trim()) {
    statusSection.append(element("p", "antigravity-usage-note", authNote.trim()));
  }
  content.push(statusSection);
  // 区块 2：ACP 实测会话 Token 统计
  const tokens = value?.sessionTokens || value?.acpSessionTokens;
  const tokenSection = element("section", "claude-usage-section");
  const tokenRow = element("div", "claude-usage-detail-row");
  tokenRow.append(element("span", "", "ACP 会话 Token"),
    element("span", "claude-usage-remaining", claudeUsageCompact(tokens?.totalTokens)));
  const tokenMetrics = element("div", "claude-usage-metrics");
  const stats = element("div", "claude-usage-stats");
  stats.append(
    element("span", "claude-usage-metric", `输入 ${claudeUsageCompact(tokens?.inputTokens)}`),
    element("span", "claude-usage-metric", `输出 ${claudeUsageCompact(tokens?.outputTokens)}`)
  );
  if (claudeUsageNumber(tokens?.cachedInputTokens) && tokens.cachedInputTokens > 0) {
    stats.append(element("span", "claude-usage-metric", `缓存 ${claudeUsageCompact(tokens.cachedInputTokens)}`));
  }
  const rawSessionsCount = tokens?.sessionCount ?? tokens?.sessions;
  const sessionsCount = Number.isSafeInteger(rawSessionsCount) ? rawSessionsCount : 0;
  tokenMetrics.append(stats, element("span", "claude-usage-reset", `${sessionsCount} 个会话`));
  tokenSection.append(tokenRow, tokenMetrics);
  content.push(tokenSection);
  // 区块 3：诚实说明 ACP 协议边界（遵循 AGENTS.md，不绕过官方 CLI 读取凭据或直连私有接口）
  const limitSection = element("section", "claude-usage-section");
  limitSection.append(element("p", "antigravity-usage-note",
    "官方 ACP 协议暂未提供 5h/7d 剩余额度百分比、重置时间与 Google 账号邮箱；Elves 严格通过官方 agy acp 读取状态，不绕过 CLI。"));
  content.push(limitSection);
  tip.node.replaceChildren(...content);
  fitClaudeUsageTip();
}
function renderClaudeUsage() {
  const trigger = $("#claude-usage");
  if (!trigger) return;
  if (claudeUsageState?.backend === "antigravity") {
    const view = antigravityUsageTriggerView(claudeUsageState);
    trigger.hidden = !view;
    fitComposerMetrics();
    if (!view) { closeClaudeUsageTip(); return; }
    const signature = JSON.stringify(["antigravity", view.text, view.warning]);
    if (trigger.dataset.usage !== signature) {
      trigger.dataset.usage = signature;
      const row = element("span", "claude-usage-window");
      row.append(element("span", view.warning ? "error-text" : "claude-usage-percent", view.text));
      trigger.replaceChildren(row);
      trigger.setAttribute("aria-label", view.label);
    }
    renderAntigravityUsageTip();
    fitComposerMetrics();
    return;
  }
  const windows = claudeUsageDisplayWindows(claudeUsageState);
  trigger.hidden = !windows.length;
  fitComposerMetrics();
  if (!windows.length) { closeClaudeUsageTip(); return; }
  const isDefault = windows[0]?.default === true;
  const signature = JSON.stringify([isDefault ? "default" : "remaining",
    ...windows.map(window => [window.key, window.remaining])]);
  if (trigger.dataset.usage !== signature) {
    trigger.dataset.usage = signature;
    // 用户要求底部额度按“5h xx%  7d xx%”紧凑展示，去掉“剩余”前缀和窗口之间的圆点分隔符。
    const nodes = [];
    for (const window of windows) {
      const row = element("span", "claude-usage-window");
      row.append(element("span", "", window.short), element("span", "claude-usage-percent", `${window.remaining}%`));
      nodes.push(row);
    }
    trigger.replaceChildren(...nodes);
    const account = claudeUsageAccount(claudeUsageState.value);
    // 用户要求默认脱敏同时覆盖底部按钮的读屏文本，不能只在弹层视觉上隐藏完整邮箱。
    trigger.setAttribute("aria-label", `Claude ${account ? `登录账号 ${claudeUsageMaskAccount(account)}，` : "账号信息，"}${isDefault ? "真实额度读取中，当前按满额显示，" : ""}${windows.map(window => `${window.short} ${window.remaining}%`).join("，")}，查看重置时间`);
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
  const windows = state.backend === "claude" ? claudeUsageWindows(state.value, now) : [];
  const expiry = [state.value?.observedAtMs, ...(state.backend === "claude" && state.tip
    ? Object.values(state.details?.value || {}).flatMap(metrics =>
      [metrics?.observedAtMs, metrics?.costObservedAtMs, metrics?.tokensObservedAtMs]) : [])]
    .filter(claudeUsageNumber).map(at => at + claudeUsageMaxAgeMs).filter(at => at > now);
  // 刷新和 72 小时数据失效分别计时；失败退避不能延长真实数据寿命，窗口到期也不能继续显示旧额度。
  const deadlines = [...(context.canRead && !state.pending ? [state.checkedAt + claudeUsageRefreshDelay(state)] : []), ...expiry,
    ...windows.filter(window => window.resetAt != null).map(window => window.resetAt)];
  if (!deadlines.length) { state.timer = null; return; }
  state.timer = setTimeout(() => {
    if (claudeUsageState !== state) return;
    state.timer = null; syncClaudeUsage();
  }, Math.max(250, Math.min(...deadlines) - now + 5));
}
export function syncClaudeUsage() {
  if (!$("#claude-usage")) return;
  const context = claudeUsageContext();
  if (!context) { resetClaudeUsage(); return; }
  if (claudeUsageState?.key !== context.key) {
    // 切换实例时原子替换数据并直接渲染目标缓存，不能先 hidden=true 再等待会话详情。
    resetClaudeUsage({ hideTrigger: false });
    const cached = claudeUsageCache.get(context.key);
    claudeUsageState = { key: context.key, backend: context.backend, owner: context.owner, value: cached?.value || null,
      checkedAt: cached?.checkedAt || 0, refreshing: cached?.refreshing === true, pending: false, controller: null, timer: null, tip: null,
      details: cached?.details || null, detailsRead: cached?.detailsRead || null, detailsPending: false, detailsController: null };
  }
  const state = claudeUsageState;
  if (!context.canRead) pauseClaudeUsage(state);
  renderClaudeUsage();
  if (!context.canRead) { scheduleClaudeUsageUpdate(); return; }
  if (!state.pending && Date.now() - state.checkedAt >= claudeUsageRefreshDelay(state)) {
    state.pending = true;
    const controller = new AbortController();
    state.controller = controller;
    if (context.backend === "antigravity") {
      // 用户要求 Antigravity 必须经官方 ACP 协议读取账号等级与会话统计，不绕过 CLI。
      void api("/api/read/antigravityUsage", context.payload, undefined, controller.signal, { resourcePriority: "background" })
        .then(result => {
          if (claudeUsageState !== state || state.controller !== controller || controller.signal.aborted) return;
          if (result?.ok !== true || result.backend !== "antigravity" || result.instanceId !== context.instanceId)
            throw new Error("Antigravity ACP 状态响应无效");
          state.refreshing = false;
          if (antigravityUsageUsable(result.usage)) {
            state.value = {
              ...(typeof result.instanceLabel === "string" && result.instanceLabel ? { instanceLabel: result.instanceLabel } : {}),
              ...result.usage
            };
          }
        })
        .catch(error => {
          if (claudeUsageState !== state || state.controller !== controller) return;
          state.refreshing = false;
          if ([401, 403].includes(error.code)) {
            state.value = null; claudeUsageCache.delete(state.key);
          }
        })
        .finally(() => {
          if (claudeUsageState !== state || state.controller !== controller) return;
          state.checkedAt = Date.now(); state.pending = false; state.controller = null;
          rememberClaudeUsage(state);
          renderClaudeUsage(); scheduleClaudeUsageUpdate();
        });
    } else {
      // 用户要求额度按账号所属实例显示；授权或实例切换后丢弃迟到响应，后台页不轮询。
      void api("/api/read/claudeUsage", context.payload, undefined, controller.signal, { resourcePriority: "background" })
        .then(result => {
          if (claudeUsageState !== state || state.controller !== controller || controller.signal.aborted) return;
          if (result?.ok !== true || result.backend !== "claude" || result.instanceId !== context.instanceId)
            throw new Error("Claude 额度响应无效");
          // 电脑端先返回保留结果并在后台经 CLI 刷新时按 5 秒复查，新结果到达后恢复 60 秒周期。
          state.refreshing = result.refreshing === true;
          const value = result.usage, windows = claudeUsageWindows(value);
          // 读取暂未返回有效窗口时保留该实例仍有效的快照；采集时间不更新，超时/重置仍按原边界失效。
          if (!windows.length && claudeUsageWindows(state.value).length) return;
          const account = claudeUsageAccount(value);
          const timeZone = claudeUsageTimeZone(value);
          state.value = windows.length ? { source: value.source, observedAtMs: value.observedAtMs,
            ...(timeZone ? { timeZone } : {}),
            ...(account ? { accountEmail: account } : {}),
            ...(Object.hasOwn(value, "subscriptionType") ? { subscriptionType: claudeUsageSubscription(value).type } : {}),
            ...Object.fromEntries(windows.map(window => [window.key,
              { usedPercentage: window.usedPercentage, resetsAt: window.resetAt }])) } : null;
          const received = rememberClaudeUsageDetails(state, value, windows);
          if (received && windows.every(window => claudeUsageMetricsMatch(state.details?.value?.[window.key], window))) {
            const observed = Math.min(Date.now(), ...Object.values(state.details.value).filter(Boolean).map(metrics => metrics.observedAtMs));
            state.detailsRead = { signature: claudeUsageSignature(windows),
              checkedAt: Math.max(observed, state.detailsRead?.signature === claudeUsageSignature(windows) ? state.detailsRead.checkedAt : 0) };
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
  }
  loadClaudeUsageDetails();
  scheduleClaudeUsageUpdate();
}
function showClaudeUsageTip() {
  syncClaudeUsage();
  const state = claudeUsageState, trigger = $("#claude-usage");
  if (!trigger?.isConnected || trigger.hidden || !state) return;
  if (state.backend === "antigravity") {
    if (!antigravityUsageTriggerView(state)) return;
    if (state.tip) { renderAntigravityUsageTip(); return; }
    closePopup(); closeComposerMenus();
    const node = element("div", "ui-popup claude-usage-tooltip");
    node.id = "claude-usage-tooltip"; node.setAttribute("role", "tooltip");
    if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
    state.tip = { trigger, node, account: "", accountVisible: false };
    trigger.setAttribute("aria-expanded", "true"); trigger.setAttribute("aria-describedby", node.id);
    document.body.append(node);
    renderAntigravityUsageTip();
    if (node.hasAttribute("popover")) node.showPopover();
    fitClaudeUsageTip();
    return;
  }
  const windows = claudeUsageDisplayWindows(state);
  if (!windows.length) return;
  if (state.tip) {
    renderClaudeUsageTip(windows); loadClaudeUsageDetails(); return;
  }
  closePopup(); closeComposerMenus();
  const node = element("div", "ui-popup claude-usage-tooltip");
  node.id = "claude-usage-tooltip"; node.setAttribute("role", "tooltip");
  if (typeof node.showPopover === "function") node.setAttribute("popover", "manual");
  state.tip = { trigger, node, account: "", accountVisible: false };
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
