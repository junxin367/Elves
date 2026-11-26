// 侧栏列表状态：本地已读、未读角标计时、侧栏读取请求与折叠分组。

import { selectedId, session, sidebar } from "../../core/state.js";
import { antigravityThreadId, claudeThreadId } from "../session/capabilities.js";

export let collapsed = new Set(), search = "";
export function setSearch(value) { return search = value; }
export let sidebarVersion = 0;
export function setSidebarVersion(value) { return sidebarVersion = value; }
export const projectVisibleThreadCounts = new Map();
export const collapsedSidebarSections = new Set();
export let recentLimit = 40;
export function setRecentLimit(value) { return recentLimit = value; }
// Marking a thread read must not turn a selection-only refresh into a list reorder.
export const sidebarReadRefreshSuppressions = new Set();
export let sidebarFilterDeviceId = null;
export function setSidebarFilterDeviceId(value) { return sidebarFilterDeviceId = value; }
export let sidebarRequest = null;
export function setSidebarRequest(value) { return sidebarRequest = value; }
export function mergeIncompleteSidebar(previous, current) {
  if (current.complete !== false || current.nativeBridge !== "harness"
      || previous?.nativeBridge !== "harness") return current;
  // 服务首批目录不完整时继续展示同一后端的已知会话；新数据优先，
  // 完整目录返回后整体核对，不能把部分结果当作删除证据；置顶目录独立判断完整性。
  const freshIds = new Set([...current.recent, ...current.pinnedThreads,
    ...current.projects.flatMap(project => project.threads)].map(item => item.id));
  const oldProjects = new Map(previous.projects.map(project => [project.id, project]));
  const projectIds = new Set(current.projects.map(project => project.id));
  // 用户要求服务模式与桌面置顶一致：原生置顶已完整时立即接受取消，不能被未完成的最近分页补回。
  const completePins = current.pinnedThreadsComplete === true;
  const pinnedIds = new Set(current.pinnedThreads.map(item => item.id));
  const mergeRows = (old, fresh) => [...fresh, ...old.filter(item => !freshIds.has(item.id) && !claudeThreadId(item.id) && !antigravityThreadId(item.id))]
    .map(item => item.projectId && !projectIds.has(item.projectId) ? { ...item, projectId: null } : item)
    .map(item => completePins ? { ...item, pinned: pinnedIds.has(item.id) } : item);
  const oldRecent = completePins
    ? [...new Map([...previous.pinnedThreads, ...previous.recent].map(item => [item.id, item])).values()]
    : previous.recent;
  return { ...current,
    recent: mergeRows(oldRecent, current.recent),
    pinnedThreads: completePins ? current.pinnedThreads : mergeRows(previous.pinnedThreads, current.pinnedThreads),
    // 项目目录始终完整，只有会话分页未完成；不能把旧服务合成的工作目录补回项目列表。
    projects: current.projects.map(project => ({ ...project,
      threads: mergeRows(oldProjects.get(project.id)?.threads || [], project.threads) }))
  };
}
function forEachSidebarThread(value, callback) {
  if (!value) return;
  for (const item of [...(value.recent || []), ...(value.pinnedThreads || [])]) callback(item);
  for (const project of value.projects || [])
    for (const item of project.threads || []) callback(item);
}
function updateSidebarThread(id, callback) {
  if (!sidebar || typeof id !== "string" || !id) return false;
  let changed = false;
  forEachSidebarThread(sidebar, item => {
    if (item.id !== id) return;
    changed = callback(item) || changed;
  });
  return changed;
}
export function normalizeTurnOutcome(turn) {
  const status = turn?.error != null ? "failed" : typeof turn?.status === "string" ? turn.status : "completed";
  const errorText = JSON.stringify(turn?.error ?? "").toLowerCase();
  const kind = status === "failed"
    ? (turn?.error?.statusCode === 429 || turn?.error?.code === 429
      || /rate\s*limit|ratelimit|too many requests|quota/u.test(errorText) ? "rateLimit" : "failed")
    : "completed";
  return {
    turnId: typeof turn?.id === "string" ? turn.id : null,
    status, kind, error: turn?.error ?? null,
    ...(Number.isFinite(turn?.completedAtMs) ? { completedAtMs: turn.completedAtMs } : {})
  };
}
export function applySidebarThreadOutcome(id, turn, { unread = false } = {}) {
  return updateSidebarThread(id, item => {
    const next = normalizeTurnOutcome(turn);
    const previous = item.lastTurnOutcome;
    const previousPriority = item.priority;
    const previousUnread = item.hasUnreadTurn === true;
    const previousStatus = item.status;
    item.lastTurnOutcome = next;
    item.status = turn?.status || item.status;
    // 用户要求完成事件先结束执行态，再按阅读位置决定未读态；展示层只能得到一个互斥状态，
    // 避免旧的 priority.active 与本次完成产生的蓝点同时落到同一个会话节点。
    const nextUnread = unread && ["completed", "failed", "interrupted"].includes(next.status)
      ? true : item.hasUnreadTurn === true;
    item.hasUnreadTurn = nextUnread;
    const failed = next.status === "failed" || ["failed", "rateLimit"].includes(next.kind);
    item.priority = {
      ...(item.priority || {}),
      status: item.status,
      active: false,
      attentionState: failed ? "error" : nextUnread ? "unread" : "idle"
    };
    return previousUnread !== nextUnread
      || JSON.stringify(previousStatus) !== JSON.stringify(item.status)
      || previousPriority?.active !== item.priority.active
      || previousPriority?.attentionState !== item.priority.attentionState
      || JSON.stringify(previous) !== JSON.stringify(next);
  });
}
export function applySidebarThreadStarted(id, turn) {
  return updateSidebarThread(id, item => {
    const turnId = typeof turn?.id === "string" ? turn.id : null;
    // Codex App 的终态投影不能被迟到的同一轮 turn/started 复活；原生事件重放时保留完成结果。
    if (turnId && item.lastTurnOutcome?.turnId === turnId
        && ["completed", "failed", "interrupted"].includes(item.lastTurnOutcome.status)) return false;
    const previous = item.lastTurnOutcome;
    const status = turn?.status || { type: "active" };
    const changed = previous != null || item.priority?.active !== true
      || JSON.stringify(item.status) !== JSON.stringify(status);
    item.lastTurnOutcome = null;
    item.status = status;
    item.priority = {
      ...(item.priority || {}),
      status: item.status,
      active: true,
      attentionState: "active"
    };
    return changed;
  });
}
export function applySidebarThreadReadState(id, readState) {
  if (!readState || typeof readState !== "object" || !Object.hasOwn(readState, "firstUnread")) return false;
  const unread = readState.firstUnread !== null;
  return updateSidebarThread(id, item => {
    const previousPriority = item.priority;
    const changed = item.hasUnreadTurn !== unread || JSON.stringify(item.readState) !== JSON.stringify(readState);
    item.readState = readState;
    item.hasUnreadTurn = unread;
    if (item.priority && ["idle", "unread"].includes(item.priority.attentionState)) {
      item.priority = { ...item.priority,
        attentionState: item.priority.active === true ? "active" : unread ? "unread" : "idle" };
    }
    return changed || JSON.stringify(previousPriority) !== JSON.stringify(item.priority);
  });
}
export function applySidebarThreadTitle(id, title) {
  if (typeof title !== "string") return false;
  return updateSidebarThread(id, item => {
    if (item.title === title) return false;
    item.title = title;
    return true;
  });
}
export let clientUnreadBadgeTimer = null, clientUnreadBadgeRequest = null;
export function setClientUnreadBadgeTimer(value) { return clientUnreadBadgeTimer = value; }
export function setClientUnreadBadgeRequest(value) { return clientUnreadBadgeRequest = value; }
export function sidebarThreadActive(item) {
  const status = item?.status?.status || item?.status;
  const type = typeof status === "string" ? status : status?.type;
  return item?.sideThreadAttention?.active === true
    || (typeof item?.priority?.active === "boolean"
      ? item.priority.active : type === "active" || type === "inProgress");
}
export function clientUnreadThreadCount(value) {
  const unread = new Set(), sideIds = new Set(value?.sideThreadIds || []);
  // 用户要求角标与会话栏实际未读数量一致：执行中的会话即使保留未读标记也不计入；
  // 最近、项目、置顶重复出现仍只算 1，侧边子会话/子代理不单独增加任务栏数字。
  for (const item of [...(value?.recent || []), ...(value?.pinnedThreads || []),
    ...(value?.projects || []).flatMap(project => project.threads || [])]) {
    if (!item?.id || item.archived === true || sideIds.has(item.id)
        || item.parentThreadId && item.parentThreadId !== item.id
        || item.threadSource === "subagent" || item.source?.subAgent || item.source?.subagent) continue;
    const attention = sidebarAttentionState(item);
    // 明确的 hasUnreadTurn=false 是已读抑制，不能被尚未同步的旧 priority 未读标记重新计入。
    const hasUnread = typeof item.hasUnreadTurn === "boolean" ? item.hasUnreadTurn : attention === "unread";
    if (hasUnread && ["unread", "error"].includes(attention)) unread.add(item.id);
  }
  return unread.size;
}
// 用户要求最近视图按优先事项、可选置顶、今天、昨天、前天、近四个星期日历日和历史分组；
// 服务端已统一输出毫秒，
// 这里兼容旧缓存中的 Unix 秒。阈值 1e11 与服务时间戳转换及
// codex_service.rs UNIX_SECONDS_LIMIT 同步。
export function sidebarTimestamp(value) {
  if (!Number.isFinite(value) || value <= 0) return 0;
  return value < 100_000_000_000 ? value * 1000 : value;
}
export function sidebarUpdatedAt(item) {
  // 用户要求区分桌面浏览与真实活动：打开/恢复旧会话可能更新 updatedAt，
  // 排序和日期分组须优先使用原生 recencyAt；仅旧数据缺失时回退。与服务目录的 recency_at 分页保持一致。
  return sidebarTimestamp(item.recencyAt) || sidebarTimestamp(item.updatedAt) || sidebarTimestamp(item.createdAt);
}
export function sidebarRecentDateGroups(items, now = Date.now()) {
  const today = new Date(now);
  today.setHours(0, 0, 0, 0);
  const starts = Array.from({ length: 7 }, (_, offset) => {
    const date = new Date(today);
    date.setDate(date.getDate() - offset);
    return date;
  });
  const weekdays = ["日", "一", "二", "三", "四", "五", "六"];
  const labels = ["今天", "昨天", "前天", ...starts.slice(3).map(date => `星期${weekdays[date.getDay()]}`)];
  const groups = labels.map((label, index) => ({
    key: index < 3 ? ["today", "yesterday", "before-yesterday"][index] : `weekday-${index}`,
    label,
    items: []
  }));
  const history = { key: "history", label: "历史", items: [] };
  for (const item of Array.isArray(items) ? items : []) {
    const timestamp = sidebarUpdatedAt(item);
    const index = starts.findIndex((start, offset) => timestamp >= start.getTime()
      && (offset === 0 || timestamp < starts[offset - 1].getTime()));
    (index >= 0 ? groups[index] : history).items.push(item);
  }
  // 用户要求最近会话先显示优先事项和可选置顶，再显示以下日期分组；
  // 空日期分组由 appendSection 跳过，但固定返回顺序，避免跨周、跨月时改变位置。
  return [...groups, history];
}
export function markSidebarThreadReadLocally(id, value = sidebar) {
  if (!value || typeof id !== "string" || !id) return;
  const parents = new Set((value.unreadSideThreads || [])
    .filter(item => item.id === id).map(item => item.parentThreadId));
  if (Array.isArray(value.unreadSideThreads))
    value.unreadSideThreads = value.unreadSideThreads.filter(item => item.id !== id);
  const update = item => {
    // 用户要求读完侧边后消除所属主会话的蓝点；同一父会话还有其它未读侧边时继续保留。
    if (item && parents.has(item.id) && item.sideThreadAttention)
      item.sideThreadAttention = { ...item.sideThreadAttention,
        unread: value.unreadSideThreads.some(side => side.parentThreadId === item.id) };
    if (!item || item.id !== id) return;
    item.hasUnreadTurn = false;
    if (item.priority?.attentionState === "unread") {
      item.priority = { ...item.priority, attentionState: item.priority.active === true ? "active" : "idle" };
    }
  };
  value.recent.forEach(update);
  value.pinnedThreads.forEach(update);
  value.projects.forEach(project => project.threads.forEach(update));
}
export function sidebarAttentionState(item) {
  const side = item.sideThreadAttention;
  const requests = Array.isArray(item.requests) && item.requests.length > 0;
  // 用户要求主会话和侧边会话的审批、问答均触发未读提示，不能等待下一次目录状态更新。
  // 正文已读不能撤销仍存在的请求。
  const own = item.priority?.attentionState;
  const status = item.status?.status || item.status;
  const type = typeof status === "string" ? status : status?.type;
  if (requests || side?.waiting || own === "waiting" || !own && (type === "waiting" || (status?.activeFlags || []).some(flag =>
    flag === "waitingOnApproval" || flag === "waitingOnUserInput"))) return "waiting";
  // 用户要求参考 Codex App 的侧栏投影：等待、运行、异常、未读、空闲按优先级互斥展示；
  // 执行态优先于旧阅读快照，直到完成事件清除 active 后才允许显示蓝色未读点。
  if (side?.active || own === "active" || item.priority?.active === true
      || !own && (type === "active" || type === "inProgress")) return "active";
  if (item.lastTurnOutcome?.status === "failed"
      || ["failed", "rateLimit"].includes(item.lastTurnOutcome?.kind)) return "error";
  if (side?.unread || own === "unread" || item.hasUnreadTurn) return "unread";
  return "idle";
}
