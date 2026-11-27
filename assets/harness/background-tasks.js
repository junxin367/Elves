// 后台任务只读展示：只接受当前会话的原生任务快照，不从普通工具、旧历史或计时器推断运行状态。

const taskKinds = {
  shell: { label: "后台命令", icon: "terminal" },
  monitor: { label: "监控", icon: "eye" },
  agent: { label: "后台子代理", icon: "model" },
  task: { label: "后台任务", icon: "queue" }
};

function backgroundTaskKind(value) {
  const kind = typeof value === "string" ? value.toLowerCase() : "";
  return /bash|shell/.test(kind) ? "shell" : /monitor/.test(kind) ? "monitor"
    : /agent/.test(kind) ? "agent" : "task";
}

export function normalizeBackgroundTasks(items, summary = "") {
  if (Array.isArray(items)) {
    const seen = new Set();
    return items.flatMap(item => {
      if (!item || typeof item.id !== "string" || !item.id.trim() || seen.has(item.id)
          || item.status != null && item.status !== "running" || item.ambient === true) return [];
      seen.add(item.id);
      const kind = backgroundTaskKind(item.kind), fallback = taskKinds[kind];
      return [{ id: item.id, kind, icon: fallback.icon, count: 1,
        name: typeof item.name === "string" && item.name.trim() ? item.name : fallback.label,
        elapsedMs: Number.isFinite(item.elapsedMs) && item.elapsedMs >= 0 ? item.elapsedMs : null }];
    });
  }
  // 仍在运行的旧 Claude 宿主可能只有数量摘要；保留已知数量与类型，不编造逐项名称或执行时间。
  const text = typeof summary === "string" ? summary.trim() : "";
  if (!text) return [];
  const parts = text.split(/\s*,\s*/).map(part =>
    /^(\d+)\s+(?:local\s+|background\s+)?(shell|monitor|agent|task)s?$/i.exec(part));
  if (!parts.every(Boolean)) return [{ id: "summary", name: text === "background" ? "后台任务" : text,
    kind: "task", icon: "queue", count: null, elapsedMs: null }];
  return parts.flatMap((part, index) => {
    const count = Number(part[1]), kind = part[2].toLowerCase();
    if (!Number.isSafeInteger(count) || count <= 0) return [];
    return [{ id: `summary:${index}:${kind}`, name: `${count} 个${taskKinds[kind].label}`,
      kind, icon: taskKinds[kind].icon, count, elapsedMs: null }];
  });
}

export function backgroundTaskCount(items) {
  if (!items.every(item => Number.isSafeInteger(item.count) && item.count > 0)) return null;
  const count = items.reduce((total, item) => total + item.count, 0);
  return Number.isSafeInteger(count) ? count : null;
}

export function taskNoticeText(item) {
  // 用户只要“成功且触发原生续跑”的正文行。这个条目由适配器核对因果后生成，不能用工具/轮次 completed 代替。
  let label;
  if (item.type === "backgroundTaskContinuation") {
    const backend = { claude: "Claude", antigravity: "AGY", codex: "Codex" }[item.source];
    if (typeof backend !== "string" || item.status !== "completed" || typeof item.taskId !== "string"
        || !item.taskId.trim() || item.statusOnly === true) return null;
    const subject = { command: "后台命令", agent: "后台子代理", monitor: "监控" }[item.kind] || "后台任务";
    label = `${subject}${item.kind === "monitor" ? "已结束" : "已完成"} · ${backend} 自动继续`;
  } else if (item.type === "claudeTaskNotification" && item.source === "claude" && item.statusOnly !== true) {
    // 定时触发、监控收到事件是已有的独立功能；旧缓存里的后台终态不能重新显示成“自动继续”。
    if (item.kind === "schedule") label = "定时任务已触发";
    else if (item.kind === "monitor" && !item.status && item.event) label = "监控收到事件";
  }
  if (!label) return null;
  const detail = [item.summary, item.event].filter(text => typeof text === "string" && text.length).join("\n");
  return { label, description: detail || "原生任务通知没有提供摘要。" };
}

export function visibleTaskTurn(turn) {
  const hidden = item => (item.type === "claudeTaskNotification" || item.type === "backgroundTaskContinuation")
    && !taskNoticeText(item);
  // 仅过滤展示副本，不改变历史归属；分页、重连或旧缓存里的隐藏项也不能切断最终回复或留下空行。
  return turn.items?.some(hidden) ? { ...turn, items: turn.items.filter(item => !hidden(item)) } : turn;
}

export function preserveBackgroundTaskSnapshot(target, current) {
  if (!target || current?.id !== target.id) return;
  // 与原生状态版本共用竞态保护：较慢的 HTTP 读取不能覆盖已经收到的任务结束快照。
  for (const field of ["backgroundTaskItems", "backgroundTasks"]) {
    if (Object.hasOwn(current, field)) target[field] = current[field];
  }
}

export function updateSessionMenuAria(trigger) {
  if (!trigger) return;
  const count = Number(trigger.getAttribute("data-background-task-count"));
  const tasks = trigger.classList.contains("has-background-tasks");
  const unread = trigger.classList.contains("has-unread-side-thread");
  const label = "会话菜单"
    + (tasks ? Number.isSafeInteger(count) && count > 0
      ? `，${count} 个后台任务正在执行` : "，有后台任务正在执行" : "")
    + (unread ? "，有未读的侧边会话" : "");
  trigger.setAttribute("aria-label", label);
  trigger.title = label;
}
