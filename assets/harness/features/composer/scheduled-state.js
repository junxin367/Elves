// 定时发送草稿状态与时间文案：时间选择属于当前会话草稿，实际定时消息由 Harness 独立保存。

export let selectedScheduledAt = null;
export function setSelectedScheduledAt(value) {
  return selectedScheduledAt = Number.isSafeInteger(value) && value > 0 ? value : null;
}
// 编辑框标记、定时消息列表、菜单说明和提示共用：当天只写“今天 HH:mm”，其余写日期；按本机时区显示。
export function scheduledTimeLabel(value) {
  if (!Number.isFinite(value)) return "未设置时间";
  const date = new Date(value), now = new Date();
  const time = new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit" }).format(date);
  return date.toDateString() === now.toDateString()
    ? `今天 ${time}` : `${date.getMonth() + 1}月${date.getDate()}日 ${time}`;
}
