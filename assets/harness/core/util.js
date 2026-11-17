// 与业务无关的基础工具。

export const randomId = () => globalThis.crypto?.randomUUID?.() || [...crypto.getRandomValues(new Uint8Array(24))].map(byte => byte.toString(16).padStart(2, "0")).join("");

// 用户要求本轮耗时与目标运行时间共用天/时/分/秒格式，避免长任务仍显示数千秒。
export function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return null;
  let remaining = Math.floor(seconds);
  const days = Math.floor(remaining / 86400); remaining %= 86400;
  const hours = Math.floor(remaining / 3600); remaining %= 3600;
  const minutes = Math.floor(remaining / 60); remaining %= 60;
  const parts = [];
  if (days) parts.push(`${days}d`);
  if (hours || days) parts.push(`${hours}h`);
  if (minutes || hours || days) parts.push(`${minutes}m`);
  if (remaining || !parts.length) parts.push(`${remaining}s`);
  return parts.join("");
}
