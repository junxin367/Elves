// 发送状态：发送按钮、异常反馈条件、未知请求的退出等待计时，以及队列图片和 Claude 项目信任确认。

import { confirmAction } from "../../ui/dialog.js";
import { api } from "../api.js";

export let threadOwnershipRetry = null;
export function setThreadOwnershipRetry(value) { return threadOwnershipRetry = value; }
export let sending = null;
export function setSending(value) { return sending = value; }
export let composerDiagnosticState = "";
export function setComposerDiagnosticState(value) { return composerDiagnosticState = value; }
// 仅在 Claude / AGY 已明确送达不明时，沿用 3 分钟后的“放弃等待”；正常在途多久都不弹回执。
export const claudeGiveUpDelayMs = 180000;
let pendingFeedbackTimer = null;
export function setPendingFeedbackTimer(value) { return pendingFeedbackTimer = value; }
export function pendingFeedbackReady(record) {
  // 用户要求：计时不作为送达证据。普通在途、服务仍在处理或恢复时缺少时间都不显示核对入口。
  if (["failed", "completed"].includes(record?.receiptState ?? record?.state)) return false;
  return !!record && (record.invalid === true || record.recoveryUnavailable === true || record.receiptMissing === true
    || (record.receiptState ?? record.state ?? record.recoveryState) === "unknown");
}
export function clearPendingFeedbackTimer() {
  clearTimeout(pendingFeedbackTimer);
  pendingFeedbackTimer = null;
}
export function queuedEditHasOriginalImages(edit) {
  // 用户要求普通队列消息恢复到输入框后按普通消息编辑；只有原消息确实带图片时，
  // 才显示“原图片会保留”提示，不能因为处于队列编辑态就误报图片限制。
  return edit?.imageRemoval === true || edit?.imageEditing === true
    || Array.isArray(edit?.originalImageSources) && edit.originalImageSources.length > 0
    || Array.isArray(edit?.attachments) && edit.attachments.length > 0;
}
export async function prepareClaudeProjectCreation(payload, stillCurrent) {
  const result = await api("/api/read/projectTrust", {
    backend: "claude", instanceId: payload.instanceId, projectId: payload.projectId
  });
  if (!stillCurrent()) return false;
  if (result?.instanceId !== payload.instanceId || typeof result?.trustPath !== "string" || !result.trustPath)
    throw new Error("项目目录信任检查结果不匹配，草稿已保留，请重新选择项目后重试。");
  if (result.trusted === true) return true;
  // 用户要求在远程完成一次目录信任；明确显示实例和真实信任边界，取消和切换项目均不写入或发送。
  const confirmed = await confirmAction("信任 Claude 项目目录",
    `实例：${result.instanceLabel || payload.instanceId}\n目录：${result.trustPath}\n`
      + "允许 Claude 加载此项目的规则和工具配置。仅确认你信任的项目；确认后继续发送当前消息。",
    "信任并继续", false, "project-trust-confirmation");
  if (!confirmed || !stillCurrent()) return false;
  payload.trustProject = true;
  payload.expectedProjectPath = result.trustPath;
  return true;
}
