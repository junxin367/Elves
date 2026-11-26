// 输入区状态与发送：发送按钮/禁用状态更新、失败输入恢复提示、发送与停止生成、输入区滚动挂载与统计同步；发送中标记与反馈计时在 send-state.js。

import {
  chooseComposerImages, chooseComposerLocalInputs, composerAttachmentSnapshot, composerAttachmentsLoading,
  composerAttachmentsPayload, composerCanAddImages, composerCanAddLocalInputs, composerHasAttachments,
  composerLocalInputsPayload, composerLocalInputsSupported, setComposerAttachmentsDisabled
} from "../../attachments.js";
import { validateRemoteReceipt } from "../../channels.js";
import { recordDiagnostic } from "../../diagnostics.js";
import { recoveryStorageMessage } from "../../recovery-store.js";
import { confirmAction } from "../../ui/dialog.js";
import { $, button, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { notice } from "../../ui/notice.js";
import { activePopup, fitPopup, renderPopup } from "../../ui/popup.js";
import { setSelectOptions } from "../../ui/select.js";
import {
  applyQueuedSendResult, closeComposerMenus, loadModels, loadSidebar, mutate, reloadThread,
  renderApprovals, renderGoalStatus, renderQueueArea, renderQueuedEditRecoveryControls,
  resolvePending, scheduleMessages, selectThread
} from "../../core/actions.js";
import {
  busy, connected, creating, nativeConnectionVersion, pageLeaving, selectedId, selectionToken,
  selectionVersion, session, setBusy, setStopping, sidebar, stopping, thread
} from "../../core/state.js";
import { recoveryStore } from "../../core/stores.js";
import { randomId } from "../../core/util.js";
import { api } from "../api.js";
import { appearance } from "../appearance/theme.js";
import { approvalReplies, questionReplyReceipts, setApprovalSignature } from "../approval-state.js";
import { renderClaudeControlNotices } from "../claude-notices.js";
import { syncClaudeUsage } from "./claude-usage.js";
import { refitComposerMenus } from "./composer-menus.js";
import {
  codexCreationDefaultsState, creationInstanceId, creationProject, syncNewChatProjectPicker
} from "./creation-state.js";
import { clearSubmittedDraft, recoverFailedInput, restoreDraft } from "./draft-restore.js";
import {
  draftKey, failedInputRecoveries, inputDraftEmpty, newDraftKey, persistInputDraft,
  recoveryGeneration, snapshotDraft, storeDraft, submittedDrafts
} from "./drafts.js";
import {
  compactUsageDescription, composerMetricsContext, composerMetricsHasTurn,
  composerMetricsLatestTurn, composerMetricsState, contextUsage, normalizeComposerMetrics,
  renderComposerMetrics, resetComposerMetrics, setComposerMetricsState, updateContextUsage
} from "./metrics.js";
import { claudePendingSetting, syncBackendModels } from "./model-catalog.js";
import { currentEffort, currentModel, currentModelId, remoteFastKey, updateRemoteFast } from "./model-choice.js";
import {
  codexCreationDefaultsKey, loadCodexCreationDefaults, newChatPreferenceScope,
  newClaudePermissionMode, newCodexPermissionMode, rememberedNewChatModel,
  rememberedNewChatSettings, selectCreatedThread
} from "./new-chat-settings.js";
import {
  instancePermissionCatalogs, modelChoiceVersion, modelLoadError, modelsLoadedAt, modelsPromise,
  remoteFastState, remoteFastWriting, selectedDraftFast, selectedEffort, selectedModel
} from "./options-state.js";
import { loadInstancePermissionCatalog, updateComposerPermissions } from "./permissions.js";
import { composerPrompt, effortLabels, resizeComposerInput } from "./prompt-editor.js";
import { updatePromptOptimization } from "./prompt-optimization.js";
import {
  claudeGiveUpDelayMs, clearPendingFeedbackTimer, composerDiagnosticState, pendingFeedbackDelay, pendingFeedbackReady,
  prepareClaudeProjectCreation, queuedEditHasOriginalImages, sending, setComposerDiagnosticState,
  setPendingFeedbackTimer, setSending, setThreadOwnershipRetry, threadOwnershipRetry
} from "./send-state.js";
import { selectedSkills } from "./skill-catalog.js";
import { scheduledTimeLabel, selectedScheduledAt } from "./scheduled-state.js";
import {
  openScheduledMessagePicker, reloadScheduledMessages, renderScheduledMessages, scheduledMessagesSupported,
  submitScheduledMessage
} from "./scheduled-message.js";
import { compactCommandAvailable, renderSkillMenu } from "./skills.js";
import {
  compactContext, compactionInProgress, compactionLocked, compactionQueueSendReason,
  currentCompaction, nativeCompactionStartBaseline, renderCompactionBanner
} from "../conversation/compaction.js";
import { historyInitialized, setFollowMessages, updateScrollControl } from "../conversation/history-state.js";
import { pending, pendingTimer, setPendingTimer } from "../conversation/pending-state.js";
import { savePending } from "../conversation/pending-store.js";
import {
  applyGoalComposerReceipt, manuallyResolveGoalComposer, renderGoalComposerRecovery
} from "../goal/composer-submission.js";
import { goalComposerReason, goalPrefix, selectGoalCommand } from "../goal/composer.js";
import {
  goalComposerAction, goalComposerPlaceholder, goalComposerRecoveries, goalEditPending,
  goalRecordThreadId, goalVersion, selectedGoal
} from "../goal/state.js";
import { editingMessage, queuedEditSubmittingAction, updateMessageEditor } from "../messages/message-edit.js";
import { restoreUnfinishedQueuedMessageEdit, submitQueuedMessageEdit } from "../queue/queued-edit.js";
import { finishSendDisplay, rememberSendDisplay } from "../queue/sent-display.js";
import { editingQueuedMessage, queueVersion, queuedEditActions } from "../queue/state.js";
import { claudeCommandSupported } from "../session/action-capabilities.js";
import {
  activeBackend, backendCapability, claudeNativeQueue, claudeThreadId, isActiveThread, isClaude, privateInputScope,
  queueCapability, serviceSideThread, temporarySideThread, threadOwnedElsewhere, threadReadOnly, threadReadOnlyMessage,
  threadReleasableElsewhere
} from "../session/capabilities.js";
import { recordRuntimeDiagnostic, stream } from "../session/connection-state.js";
import {
  activeInstanceId, availableSessionInstances, newChatBackendOptions, sessionInstanceEnabled
} from "../session/instances.js";
import { relatedThreadOperations, sideCreationView } from "../session/related-state.js";
import {
  ownsRelatedOperation, renderRelatedOperationStatus, syncRelatedSessionLayout,
  updateSideCreationStatus
} from "../session/related.js";
import { syncRequestedSideInput, syncWorkspaceSidePanelState } from "../session/side-panel.js";
import { currentThreadDisplay } from "../session/thread-cache.js";
import { rememberThreadDisplay, syncRelatedModelDisplay } from "../session/thread-display.js";
import { sessionTools, workspaceOpenIn } from "../shell-state.js";
import { sidebarBusy } from "../sidebar/actions.js";
import {
  threadDeleted, threadDeletionChecks, threadDeletionKey, threadDeletionLocked, threadDeletions
} from "../sidebar/deletion-state.js";
import { checkThreadDeletion } from "../sidebar/thread-deletion.js";

function currentThreadOwnershipRetry(record = threadOwnershipRetry) {
  return !!record && !pageLeaving && record.threadId === selectedId && record.selectionToken === selectionToken
    && record.scope === privateInputScope(session) && record.connectionVersion === nativeConnectionVersion;
}
function renderThreadOwnership() {
  const retry = $("#thread-ownership-retry");
  if (!retry) return;
  // 重试与“关闭其他占用”共用一个进行中记录：任一操作进行时两个按钮都禁用，只有发起的那个显示处理中状态。
  const working = currentThreadOwnershipRetry();
  const releasing = working && threadOwnershipRetry?.action === "release";
  retry.disabled = !connected || working;
  retry.textContent = working && !releasing ? "重试中…" : "重试";
  retry.setAttribute("aria-busy", String(working && !releasing));
  const release = $("#thread-ownership-release");
  if (!release) return;
  release.hidden = !threadReleasableElsewhere();
  release.disabled = !connected || working;
  release.textContent = releasing ? "关闭中…" : "关闭其他占用";
  release.setAttribute("aria-busy", String(releasing));
}
export async function retryThreadOwnership() {
  if (!threadOwnedElsewhere() || !connected || currentThreadOwnershipRetry()) return;
  const record = { threadId: selectedId, selectionToken, scope: privateInputScope(session),
    connectionVersion: nativeConnectionVersion };
  setThreadOwnershipRetry(record);
  renderThreadOwnership();
  try {
    // 用户要求这里只重新核对占用；不恢复其他客户端的会话、不创建分支，也不重发草稿或排队消息。
    await reloadThread();
    if (currentThreadOwnershipRetry(record) && !document.hidden && !threadOwnedElsewhere()
        && !threadReadOnly() && !$("#composer")?.hidden && !$("#prompt")?.disabled)
      $("#prompt")?.focus({ preventScroll: true });
  } catch (error) {
    if (currentThreadOwnershipRetry(record)) notice(
      `检查会话占用失败：${error.message}`, true);
  } finally {
    if (threadOwnershipRetry === record) {
      setThreadOwnershipRetry(null);
      renderThreadOwnership();
    }
  }
}

// 用户要求提示里能看出是什么在占用：只按服务端核对出的进程类型和 PID 描述，不含命令行、目录或环境。
function externalHolderLabel(holder) {
  const entrypoint = typeof holder?.entrypoint === "string" ? holder.entrypoint : "";
  const background = holder?.kind === "bg";
  const what = background ? "后台任务"
    : entrypoint.startsWith("sdk") ? "其他应用里的 Claude 会话"
    : holder?.kind === "interactive" && entrypoint === "cli" ? "终端里打开的 claude"
    : "其他 Claude 进程";
  const detail = [background ? "旧版 Elves 或 claude --bg 启动" : "",
    Number.isSafeInteger(holder?.pid) && holder.pid > 0 ? `PID ${holder.pid}` : ""].filter(Boolean).join("，");
  return detail ? `${what}（${detail}）` : what;
}
export async function releaseThreadHolders() {
  if (!threadReleasableElsewhere() || !connected || currentThreadOwnershipRetry()) return;
  const record = { threadId: selectedId, selectionToken, scope: privateInputScope(session),
    connectionVersion: nativeConnectionVersion, action: "release" };
  const holders = thread.externalHolders.map(externalHolderLabel);
  const scope = holders.length ? ` ${holders.length} 个进程：${holders.join("、")}` : "其他进程";
  // 用户要求关闭其他占用前必须确认：它会结束别的应用的进程，弹窗写明影响范围；Elves 自己的会话进程不受影响。
  if (!await confirmAction("关闭其他占用",
      `将结束正在使用这个会话的${scope}。它们正在执行的任务会被中断，未保存的内容可能丢失；`
      + "Elves 自己的会话进程不受影响。结束后可在这里继续使用该会话。",
      "关闭其他占用", true)) return;
  // 弹窗期间会话、授权或连接可能已变化；确认后重新核对，不能把旧确认用于另一个会话。
  if (!currentThreadOwnershipRetry(record) || !threadReleasableElsewhere() || currentThreadOwnershipRetry()) return;
  setThreadOwnershipRetry(record);
  renderThreadOwnership();
  try {
    const requestId = randomId();
    const receipt = validateRemoteReceipt(
      await api("/api/action/releaseExternalHolders", { threadId: record.threadId }, requestId), requestId);
    if (receipt.state === "failed")
      throw Object.assign(new Error(receipt.result?.message || "无法关闭其他占用"), { code: receipt.result?.code || 422 });
    if (receipt.state !== "completed") throw new Error("未能确认关闭结果，请点“重试”核对当前占用");
    try { await reloadThread(); }
    catch (error) {
      // 进程已经关闭，只是重新读取失败：不能报成“关闭失败”，让用户重试核对即可。
      if (currentThreadOwnershipRetry(record)) notice(`已关闭其他占用，但重新读取会话失败：${error.message}，请点“重试”核对。`, true);
      return;
    }
    if (currentThreadOwnershipRetry(record)) {
      if (threadOwnedElsewhere()) notice("已结束其他进程，但这个会话仍被占用，请点“重试”再核对。", true);
      else {
        notice("已关闭其他占用，可以继续使用这个会话。");
        if (!document.hidden && !threadReadOnly() && !$("#composer")?.hidden && !$("#prompt")?.disabled)
          $("#prompt")?.focus({ preventScroll: true });
      }
    }
  } catch (error) {
    if (currentThreadOwnershipRetry(record)) notice(`关闭其他占用失败：${error.message}`, true);
    // 失败时可能已有部分进程退出，重新读取一次让提示反映实际占用；读取失败不覆盖上面的原因。
    if (currentThreadOwnershipRetry(record)) void reloadThread().catch(() => {});
  } finally {
    if (threadOwnershipRetry === record) {
      setThreadOwnershipRetry(null);
      renderThreadOwnership();
    }
  }
}

export function syncPendingFeedbackTimer() {
  clearPendingFeedbackTimer();
  if (!session || pageLeaving || document.hidden) return;
  const compaction = currentCompaction();
  const awaitingCompaction = compaction.state === "unknown"
    || compaction.state === "pending" && !compaction.acknowledged && !compaction.nativeConfirmed;
  // Claude 压缩受理后迟迟不开始才提示：会话执行中的排队不计时，空闲后到点再刷新输入区提示。
  const nativeStartSince = nativeCompactionStartBaseline(selectedId, compaction);
  const records = [
    pending, threadDeletions.get(threadDeletionKey(selectedId)),
    goalEditPending?.payload.threadId === selectedId ? goalEditPending : null,
    awaitingCompaction ? compaction : null,
    nativeStartSince === null ? null : { action: "nativeCompactionStart", time: nativeStartSince },
    ...[...relatedThreadOperations.values()].filter(record =>
      ownsRelatedOperation(record) && record.parentThreadId === selectedId),
    ...[...goalComposerRecoveries.values()].filter(record =>
      record.recoveryState === "unknown" && (goalRecordThreadId(record) === selectedId
        || creating && newDraftKey(record.payload.projectId, record.payload.backend, record.payload.instanceId) === draftKey())),
    ...[...approvalReplies.values(), ...questionReplyReceipts.values()].filter(record =>
      record.deviceId === session.device.id && record.threadId === selectedId)
  ];
  const remaining = records.filter(record => record && Number.isFinite(record.time)
      && (!record.deviceId || record.deviceId === session.device.id)
      && !["confirming", "deleted"].includes(record.state))
    .map(record => record.time + pendingFeedbackDelay(record) - Date.now()).filter(delay => delay > 0);
  if (!remaining.length) return;
  const version = selectionVersion, scope = privateInputScope(session);
  setPendingFeedbackTimer(setTimeout(() => {
    setPendingFeedbackTimer(null);
    if (pageLeaving || document.hidden || version !== selectionVersion || scope !== privateInputScope(session)) return;
    if (sideCreationView) updateSideCreationStatus(sideCreationView.record);
    setApprovalSignature(null);
    renderApprovals();
    updateComposer();
  }, Math.min(...remaining)));
}
export function updateComposer({ followEnd = false } = {}) {
  workspaceOpenIn?.updateContext();
  sessionTools?.updateContext();
  syncRelatedSessionLayout();
  if (!$("#composer")) return;
  renderThreadOwnership();
  if (typeof restoreUnfinishedQueuedMessageEdit === "function") restoreUnfinishedQueuedMessageEdit();
  // 先恢复当前会话的缓存及保存状态，再决定发送按钮是否等待该会话的 Fast 写入。
  updateRemoteFast();
  if ($("#permission-button")) updateComposerPermissions();
  const sessionTypeEnabled = sessionInstanceEnabled(activeInstanceId());
  const backendSelect = $("#new-chat-backend");
  const distribution = JSON.stringify([availableSessionInstances(), appearance.sessionInstances, creationInstanceId]);
  if (backendSelect && backendSelect.dataset.wslDistribution !== distribution) {
    backendSelect.dataset.wslDistribution = distribution;
    setSelectOptions(backendSelect, newChatBackendOptions(), creationInstanceId);
  }
  // 用户要求已选会话顶部显示对应品牌；图标独立于标题文字，标题更新不会清掉图标，新建/欢迎页不残留旧品牌。
  const backendMark = $("#thread-backend-icon");
  if (backendMark) {
    const backend = selectedId ? activeBackend() : "";
    if (backendMark.dataset.backend !== backend) {
      backendMark.dataset.backend = backend;
      backendMark.hidden = !backend;
      backendMark.replaceChildren(...(backend ? [icon(backend)] : []));
    }
  }
  syncBackendModels();
  resizeComposerInput({ followEnd });
  renderGoalStatus();
  renderQueueArea();
  renderScheduledMessages();
  updateScrollControl();
  const typedGoal = goalPrefix($("#prompt").value);
  const goalMode = !!selectedGoal || !!typedGoal;
  const active = isActiveThread(), prompt = composerPrompt(typedGoal && !selectedGoal
    ? $("#prompt").value.slice(typedGoal.end) : $("#prompt").value, selectedSkills, goalMode),
    hasAttachments = composerHasAttachments(), send = $("#send");
  const goalReason = goalMode ? goalComposerReason({ submitting: true }) : "";
  const deleting = threadDeletionLocked();
  const unsupportedProjectless = creating && !$("#project-select").value && !backendCapability("projectlessCreation");
  const compaction = currentCompaction(), compacting = compactionLocked();
  const editing = !!editingMessage || !!editingQueuedMessage;
  // 定时发送只是保存消息、到期才走普通发送：执行中或压缩中都能设置，不受下面的即时发送限制。
  const scheduling = !!selectedScheduledAt && !editing;
  const compactionSendReason = compacting && !goalMode && !editing && !scheduling ? compactionQueueSendReason() : "";
  const queueOnly = !scheduling && !isClaude() && compacting && !goalMode && !editing && !compactionSendReason;
  const unsupportedFiles = composerLocalInputsPayload().length > 0
    && session?.capabilities?.localFileInput !== true;
  const inputImages = composerAttachmentsPayload();
  const unsupportedStagedImages = inputImages.some(image => image.staged)
    && (isClaude() || session?.capabilities?.desktopCodexImageInput !== true);
  const unsupportedImages = unsupportedStagedImages
    || !editing && inputImages.length > 0 && !backendCapability("imageInput")
    || !!editingQueuedMessage?.imageEditing && inputImages.length > 0
      && !queueCapability("queuedImageEditing");
  const queueImageNote = editingQueuedMessage?.state === "editing"
    && queuedEditHasOriginalImages(editingQueuedMessage) && !editingQueuedMessage.imageEditing
    ? editingQueuedMessage.imageRemoval
      ? "当前运行的服务仅支持删除原图；更新程序后可添加图片。"
      : queueCapability("queuedImageEditing")
        ? "部分原图片无法安全编辑，本次仅修改文字，原图片会保留。"
        : "当前运行的服务仅支持修改文字，原图片会保留；更新程序后可增删图片。"
    : "";
  let attachmentNote = $("#attachment-support-note");
  if (!attachmentNote && $("#composer-attachments")) {
    attachmentNote = element("small", "attachment-support-note", "当前运行的服务尚不支持图片发送，请更新程序后再发送。");
    attachmentNote.id = "attachment-support-note";
    $("#composer-attachments").after(attachmentNote);
  }
  if (attachmentNote) {
    attachmentNote.textContent = unsupportedFiles
      ? "当前远程服务尚不支持文件和文件夹发送，请更新服务；附件已保留。"
      : unsupportedStagedImages
      ? "原图仅支持在具备此能力的 Elves 本机 Codex 会话发送，图片已保留。"
      : unsupportedImages
      ? editingQueuedMessage
        ? "当前运行的服务尚不支持编辑队列图片，修改已保留，请更新程序后再发送。"
        : "当前运行的服务尚不支持图片发送，请更新程序后再发送。"
      : queueImageNote;
    attachmentNote.hidden = !unsupportedFiles && !unsupportedImages && !queueImageNote;
  }
  // 用户要求压缩期间队列编辑与普通草稿一样可修改和发送，不能联动锁住输入框、附件或发送按钮。
  setComposerAttachmentsDisabled(threadReadOnly() || deleting || busy || stopping
    || !!editingMessage || editing && !!pending || editingQueuedMessage?.state === "starting" || (!creating && !selectedId));
  const add = $("#composer-add");
  const usage = contextUsage(currentThreadDisplay()?.tokenUsage);
  // 用户指定左侧“+”菜单从上到下为定时发送、压缩、目标、文件和文件夹、图片；本机文件入口仍仅在支持原生选择器的独立客户端显示。
  add.menuOptions = [
    ...(scheduledMessagesSupported() ? [{
      value: "scheduled", label: "定时发送", icon: "recent",
      description: selectedScheduledAt ? scheduledTimeLabel(selectedScheduledAt) : "设置未来 24 小时内的发送时间",
      disabled: creating || !!selectedGoal || !!editingMessage || !!editingQueuedMessage || threadReadOnly()
        || !selectedId || !connected,
      action: openScheduledMessagePicker
    }] : []),
    { value: "compact", label: "压缩", icon: "compactContext",
      description: compactUsageDescription(usage),
      disabled: !compactCommandAvailable(), action: compactContext },
    { value: "goal", label: "目标", icon: "target",
      description: goalComposerReason() || "设置要持续追求的目标",
      disabled: !!goalComposerReason(), action: () => selectGoalCommand(null) },
    ...(composerLocalInputsSupported() ? [{
      value: "files", label: "文件和文件夹", icon: "folderClosed",
      disabled: deleting || !composerCanAddLocalInputs() || session?.capabilities?.localFileInput !== true,
      // 用户要求点击一级菜单直接打开系统选择窗口，文件和文件夹无需先选类型。
      action: chooseComposerLocalInputs
    }] : []),
    { value: "images", label: "图片", icon: "image", disabled: deleting || !composerCanAddImages(), action: chooseComposerImages }
  // 用户要求临时侧边直接移除“目标”按钮，/ 菜单需同步隐藏，不能仅保留禁用项。
  ].filter(option => (option.value !== "goal" || !temporarySideThread())
    && (!isClaude() || !["compact", "goal"].includes(option.value) || claudeCommandSupported(option.value)));
  add.disabled = add.menuOptions.every(option => option.disabled);
  if (activePopup?.trigger === add) renderPopup();
  const queuedEditAction = queuedEditSubmittingAction();
  const sendingLabel = sending ? sending.phase === "syncing" ? "同步中…" : "发送中…"
    : queuedEditAction === "cancelQueuedEdit" ? "处理中…" : queuedEditAction === "editQueued" ? "发送中…" : "";
  if ($("#send-status").textContent !== sendingLabel) $("#send-status").textContent = sendingLabel;
  // 用户要求压缩执行中空输入显示停止；有正文或附件时仍可加入原生队列，旧 print 会话保留原停止行为。
  // 用户要求排队取回后沿用普通发送图标和文案；空草稿不能误显示停止当前任务。
  // 只有定时标记、还没有正文时仍按空输入处理，执行中保留停止按钮。
  const action = sending ? "sending" : editingQueuedMessage || scheduling && (prompt || hasAttachments) ? "send"
    : isClaude() && !claudeNativeQueue() && active && selectedId ? "stop"
    : !goalMode && !editing && (active || compactionInProgress(compaction))
      && ((!prompt && !hasAttachments) || pending) && selectedId ? "stop" : "send";
  send.dataset.queuedEditState = editingQueuedMessage?.state || "";
  const stopLabel = compacting ? "停止压缩" : "停止生成";
  const sendLoading = action === "sending" || !!queuedEditAction;
  if (send.dataset.action !== action || send.dataset.loading !== String(sendLoading)) {
    send.dataset.action = action;
    send.dataset.loading = String(sendLoading);
    const indicator = sendLoading ? element("span", "spinner")
      : icon(action === "stop" ? "stop" : "arrowUp");
    indicator.setAttribute("aria-hidden", "true");
    send.replaceChildren(indicator);
  }
  send.setAttribute("aria-label", sendingLabel || (goalMode ? "创建目标"
    : action === "stop" ? stopLabel : scheduling ? "设置定时发送" : queueOnly ? "发送到原生消息队列" : "发送消息"));
  send.title = sendingLabel || (editingQueuedMessage ? "发送消息" : editingMessage ? "请先完成消息编辑" : goalMode ? goalReason || "创建目标"
    : action === "stop" ? stopLabel : scheduling ? `设置定时发送：${scheduledTimeLabel(selectedScheduledAt)}`
    : queueOnly ? "加入原生消息队列，压缩完成后执行" : compactionSendReason || (active ? "发送补充消息" : "发送消息"));
  if (unsupportedStagedImages) send.title = "原图仅支持在具备此能力的 Elves 本机 Codex 会话发送";
  else if (unsupportedImages) send.title = "当前服务版本尚不支持图片发送，请先更新程序";
  if (unsupportedFiles) send.title = "请更新远程服务后发送文件和文件夹";
  if (unsupportedProjectless) send.title = "当前服务版本尚不支持无项目会话，请先更新程序或选择已有项目";
  send.setAttribute("aria-busy", String(busy || stopping));
  const blockedBy = !sessionTypeEnabled && action !== "stop" ? "backend-disabled"
    : editingMessage ? "message-edit" : editingQueuedMessage?.state === "starting" ? "queue-edit-loading"
    : deleting ? "delete" : busy ? "busy" : stopping ? "stopping"
    // 保存档位仅防止新消息抢先提交，停止当前任务无需等待 Fast 请求。
    : action !== "stop" && $("#permission-button")?.dataset?.saving === "true" ? "permissions-saving"
    : action !== "stop" && remoteFastWriting?.key === remoteFastKey() ? "fast-saving"
    : action !== "stop" && compactionSendReason ? compaction.deviceId && compaction.deviceId !== session?.device.id ? "compaction-scope"
      : compaction.state === "unknown" ? "compaction-unknown" : compaction.state === "pending" && !compaction.acknowledged && !compaction.nativeConfirmed
        ? "compaction-pending" : "compaction-unsupported"
    : !connected ? "offline" : !scheduling && isClaude() && !claudeNativeQueue() && active && action !== "stop" ? "claude-running"
    // 历史会话已保存当前模型时可直接续发；没有完整目录也不能强制用户先打开模型选择器。
    : isClaude() && !modelsLoadedAt && !currentThreadDisplay()?.model && action !== "stop" ? "claude-models" : threadReadOnly() ? "read-only" : unsupportedImages ? "images-unsupported"
    : unsupportedFiles ? "files-unsupported" : unsupportedProjectless ? "project-required" : goalReason ? "goal" : composerAttachmentsLoading() ? "images-loading"
    : action !== "stop" && pending ? "pending-receipt" : action !== "stop" && !prompt && !hasAttachments ? "empty"
    : !creating && (!selectedId || thread?.id !== selectedId) ? "thread-not-loaded" : "ready";
  send.disabled = blockedBy !== "ready";
  const diagnosticState = JSON.stringify([privateInputScope(session), selectedId, blockedBy, pageLeaving, connected, stream?.readyState]);
  if (diagnosticState !== composerDiagnosticState) {
    setComposerDiagnosticState(diagnosticState);
    recordRuntimeDiagnostic("composer.state", { blockedBy, draftChars: prompt.length,
      attachmentCount: composerAttachmentSnapshot().length });
  }
  $("#prompt").disabled = !sessionTypeEnabled || threadReadOnly() || deleting || !!editingMessage || editing && (busy || !!pending)
    || editingQueuedMessage?.state === "starting" || (!creating && !selectedId);
  syncRequestedSideInput();
  syncWorkspaceSidePanelState();
  // 外部占用只限制当前会话，仍允许新建其他聊天及使用侧栏菜单。
  $("#new-chat").disabled = session?.readOnly || threadReadOnly() && !threadOwnedElsewhere() || !connected && !(session?.capabilities?.claudeSessions && stream?.readyState === EventSource.OPEN);
  // 用户要求主会话和侧边队列编辑时仍可选择后续模型；执行中选择不写进排队消息，后续新发送才应用。
  $("#model-button").disabled = !sessionTypeEnabled || threadReadOnly() || deleting
    || !!(editingMessage || editingQueuedMessage) && (busy || stopping || !!pending || !connected);
  $("#sidebar-view-toggle").disabled = !sidebar;
  syncNewChatProjectPicker();
  if (backendSelect) backendSelect.disabled = !creating || threadReadOnly() || busy || stopping || !!pending;
  updatePromptOptimization();
  for (const control of document.querySelectorAll(".row-menu-button")) control.disabled = session?.readOnly || threadReadOnly() && !threadOwnedElsewhere() || sidebarBusy || !connected;
  fitPopup();
  $("#prompt").placeholder = threadReadOnly() ? threadReadOnlyMessage() : compacting && !editing && !goalMode
    ? queueOnly ? "继续输入，发送后会排队等待压缩完成…" : "可继续编辑草稿，压缩期间暂不能发送…"
    : goalMode ? goalComposerPlaceholder
    : isClaude() ? active ? claudeNativeQueue()
      ? thread?.capabilities?.remoteQueue === true ? "继续输入，消息会排队等待发送…" : "继续输入，消息会进入 Claude 原生队列…"
      : "Claude 正在执行，可停止后发送新消息…" : "给 Claude 发送消息…"
    : active ? serviceSideThread() ? "继续输入，消息会排队等待当前任务完成…" : "补充要求，会使用原生消息队列…"
    : "给 Codex 发送消息，输入 / 选择命令或 Skill…";
  const model = currentModel(), effort = currentEffort();
  const name = model?.displayName || currentModelId()
    || (modelsPromise || selectedId && !thread ? "正在读取模型…" : "模型信息暂不可用");
  $("#model-button").replaceChildren(element("span", "model-name", name));
  if (effort) $("#model-button").append(document.createTextNode(" "), element("span", "model-effort", effortLabels[effort] || effort));
  $("#model-button").title = modelLoadError || `模型：${name}${effort ? ` · 思考深度：${effortLabels[effort] || effort}` : ""}`;
  const modelChange = claudePendingSetting("model"), effortChange = claudePendingSetting("effort");
  const nativeChange = !selectedModel && modelChange || !selectedEffort && effortChange;
  if (nativeChange) {
    const state = nativeChange.state === "confirmation" ? "待确认"
      : nativeChange.state === "confirming" ? "确认中" : "待生效";
    $("#model-button").append(document.createTextNode(" "), element("span", "model-effort muted", state));
    $("#model-button").title += ` · ${state}，原生确认后生效`;
  }
  syncRelatedModelDisplay();
  updateContextUsage();
  syncClaudeUsage();
  syncComposerMetrics();
  if ($("#skill-menu") && !$("#skill-menu").hidden) renderSkillMenu();
  if (threadReadOnly()) closeComposerMenus({ keepContextUsage: !threadOwnedElsewhere() });
  refitComposerMenus();
  $("#pending-area").replaceChildren();
  if (!sessionTypeEnabled) $("#pending-area").append(element("div", "pending-banner",
    "此会话实例尚未启用。可在「设置 → 会话实例」中启用对应运行环境。"));
  const deletion = threadDeletions.get(threadDeletionKey(selectedId));
  if (deletion && !["confirming", "deleted"].includes(deletion.state) && pendingFeedbackReady(deletion)) {
    const box = element("div", "pending-banner", "正在确认会话删除结果。");
    if (deletion.state !== "submitting") {
      const check = button("核对删除结果", "只查询删除回执，不会再次删除会话",
        () => void checkThreadDeletion(deletion), "text-button");
      check.disabled = !connected || threadDeletionChecks.has(threadDeletionKey(selectedId));
      box.append(check);
    }
    $("#pending-area").append(box);
  }
  if (unsupportedProjectless && !threadReadOnly()) $("#pending-area").append(element("div", "pending-banner",
    "当前运行的服务尚不支持无项目会话；更新程序后可直接发送，或先选择已有项目。"));
  if (goalMode && goalReason && !busy && !pending && !threadReadOnly())
    $("#pending-area").append(element("div", "pending-banner goal-composer-note", goalReason));
  renderClaudeControlNotices($("#pending-area"));
  if (pending && (!sending || pending.payload !== sending.payload)
      && (pending.invalid || pending.deviceId && pending.deviceId !== session.device.id || pendingFeedbackReady(pending))) {
    const unavailable = pending.invalid || (pending.deviceId && pending.deviceId !== session.device.id);
    const pendingGoal = goalComposerAction(pending.action, pending.payload);
    const pendingQueueItem = ["steerQueued", "removeQueued"].includes(pending.action);
    // 用户要求（2026-11-25）：Claude 会话的发送/历史编辑由 Harness 自己判定送达结果，不再让用户“查回执”或“已人工确认”；
    // 页面自动轮询，服务端按原生记录结案（见 pending.js 的 resolvePending 与 claude_queue.rs 的 abandon_stale）。
    const claudeMessage = !unavailable && ["send", "editMessage"].includes(pending.action)
      && claudeThreadId(pending.payload?.threadId);
    const box = element("div", "pending-banner", unavailable ? "回执损坏或属于旧授权。请检查原生会话后人工解除锁定，不会自动重发。"
      : claudeMessage ? "正在确认消息送达，确认后会自动更新，无需操作。"
      : pending.action === "reorderQueued" ? "正在确认队列排序结果。"
      : pending.action === "forkThread" ? "正在确认新聊天分支创建结果；不会重复创建。"
      : pendingGoal ? "正在确认目标提交结果。"
      : pendingQueueItem && pending.receiptMissing ? "未找到队列操作回执，请核对原生会话；不会自动重发。"
      : pendingQueueItem ? pending.action === "steerQueued"
        ? "正在确认队列引导结果。"
        : "正在确认队列删除结果。"
      : queuedEditActions.has(pending.action) ? "正在确认队列编辑结果。" : "正在确认消息送达结果。");
    if (!unavailable && queuedEditActions.has(pending.action))
      renderQueuedEditRecoveryControls(box, pending.payload.threadId, pending.payload.editId);
    else if (!unavailable && !claudeMessage) box.append(button("查回执", pendingGoal ? "查询原目标提交回执，不会再次提交"
      : pendingQueueItem ? "只查询原队列操作回执，不会重复引导或删除"
      : pending.action === "forkThread" ? "查询原分支回执，不会重复创建" : "查询此消息的送达回执", () => void resolvePending(), "text-button"));
    const createdGoalThreadId = pendingGoal && goalRecordThreadId(pending);
    if (!unavailable && createdGoalThreadId && selectedId !== createdGoalThreadId)
      box.append(button("打开已创建会话", "打开已创建的会话，只读取数据",
        () => void selectThread(createdGoalThreadId, { closeSidebar: false }), "text-button"));
    if (pendingGoal && !unavailable) box.append(button("已人工核对", "在本机核对后解除目标提交锁定",
      () => void manuallyResolveGoalComposer(pending), "text-button"));
    else if (!queuedEditActions.has(pending.action) && !pendingQueueItem && !claudeMessage) box.append(button("已人工确认", "确认检查会话后解除锁定", async () => {
      const current = pending;
      if (await confirmAction("解除发送锁定", "请先检查本机或网页会话。解除锁定不会重发，但再次发送可能重复执行；仅在已核对结果后继续。", "已核对，解除锁定", true)
          && pending === current) { savePending(null); updateComposer(); }
    }, "text-button"));
    // Claude 的自动结案仍无结论（例如会话一直处于执行中）超过 3 分钟，才提供放弃等待；
    // 放弃时把输入恢复到输入框，不重发，也不要求用户去原生客户端核对。
    if (claudeMessage && Number.isFinite(pending.time) && Date.now() - pending.time >= claudeGiveUpDelayMs)
      box.append(button("放弃等待", "放弃等待并把输入恢复到输入框，不会重发；若消息其实已送达，再次发送会重复", async () => {
        const current = pending;
        if (!await confirmAction("放弃等待送达结果", "等待较久仍无法确认这条消息是否送达。放弃后内容会恢复到输入框，不会自动重发；"
            + "如果它其实已经送达，再次发送会重复。", "放弃并恢复输入", true) || pending !== current) return;
        try { await recoverFailedInput(current); }
        catch (error) { notice(error.message || "输入恢复失败，原请求回执已保留，请勿重新发送。", true); return; }
        if (pending === current) savePending(null);
        updateComposer();
      }, "text-button"));
    $("#pending-area").append(box);
  }
  renderFailedInputRecovery($("#pending-area"));
  renderGoalComposerRecovery($("#pending-area"));
  renderRelatedOperationStatus($("#pending-area"));
  renderCompactionBanner($("#pending-area"), selectedId, compaction);
  if (historyInitialized && selectedId) scheduleMessages();
  updateMessageEditor();
  syncPendingFeedbackTimer();
}
function renderFailedInputRecovery(root) {
  const key = draftKey(), scope = privateInputScope(session);
  const record = [...failedInputRecoveries.values()].find(record => record.deviceId === session?.device.id
    && record.recoveryScope === scope && (record.payload.threadId || newDraftKey(record.payload.projectId, record.payload.backend, record.payload.instanceId)) === key);
  if (!record) return;
  const empty = !editingQueuedMessage && !editingMessage && inputDraftEmpty(snapshotDraft());
  const box = element("div", "pending-banner", record.action === "editMessage"
    ? empty ? "上次历史编辑未完成，修改文字已保留；恢复后请核对当前历史和原附件。"
      : "上次历史编辑未完成，修改文字已保留；清空当前输入后可恢复，当前草稿不会被覆盖。"
    : empty
    ? "上次发送失败，原文字、Skill 和图片已保留，可恢复到输入框。"
    : "上次发送失败，原输入已保留；清空当前输入后可恢复，当前内容不会被覆盖。");
  const restore = button("恢复输入", "恢复上次失败的输入，不会自动发送", async () => {
    if (threadReadOnly() || draftKey() !== key || privateInputScope(session) !== scope || editingQueuedMessage || editingMessage
        || !inputDraftEmpty(snapshotDraft())) return;
    storeDraft(key, record.draft); restoreDraft(key); resizeComposerInput();
    if (!record.sessionRecovery) {
      try {
        await persistInputDraft(key);
        await recoveryStore.remove(scope, "failed", record.id);
        if (privateInputScope(session) === scope) failedInputRecoveries.delete(record.id);
      } catch (error) { notice(recoveryStorageMessage(error), true); }
    }
    updateComposer();
  }, "text-button");
  restore.disabled = threadReadOnly() || !empty || busy || stopping || !!pending;
  box.append(restore); root.append(box);
}
export async function sendMessage() {
  // 用户要求未启用类型不可发起新输入；打开旧深链仍可查看历史，不会因此停止原生正在执行的任务。
  if (!sessionInstanceEnabled(activeInstanceId())) return;
  if (threadDeletionLocked() || threadDeleted(selectedId)) return;
  if (editingQueuedMessage) return submitQueuedMessageEdit();
  if (editingMessage) return;
  if (busy || stopping || pending || threadReadOnly() || !connected) return;
  if (remoteFastWriting?.key === remoteFastKey()) { notice("正在保存 Fast 设置，请稍后发送"); return; }
  if ($("#permission-button")?.dataset?.saving === "true") { notice("正在保存权限，请稍后发送"); return; }
  if (composerAttachmentsLoading()) { notice("附件正在读取，请稍后发送"); return; }
  const nativeCodexDefaults = creating && !isClaude() && session.capabilities?.codexSessionDefaults === true;
  if (nativeCodexDefaults) {
    const key = codexCreationDefaultsKey();
    try {
      // 用户要求新建沿用有效的模型和深度；目录未就绪时不能把已记住的模型误判为失效。
      const [defaults] = await Promise.all([loadCodexCreationDefaults(true), loadModels()]);
      if (!defaults) return;
    } catch (error) {
      if (codexCreationDefaultsKey() === key && !codexCreationDefaultsState?.error)
        notice(`无法确认新会话模型：${error.message}`, true);
      return;
    }
    // 默认值读取期间切换项目、授权或已有会话时，本次发送失效；不能将旧项目配置带给新目标。
    if (codexCreationDefaultsKey() !== key || busy || stopping || pending || !connected) return;
  }
  const claudePreferences = creating && isClaude() ? rememberedNewChatSettings() : null;
  if (claudePreferences && (claudePreferences.model || claudePreferences.reasoningEffort || claudePreferences.permissionMode)) {
    const preferenceScope = newChatPreferenceScope(), project = creationProject,
      version = selectionVersion, connection = nativeConnectionVersion, instanceId = activeInstanceId();
    const current = () => creating && isClaude() && newChatPreferenceScope() === preferenceScope
      && creationProject === project && selectionVersion === version && nativeConnectionVersion === connection;
    try {
      // 用户要求 Claude 新建恢复三项记忆；模型目录及权限能力读取完成后再构造请求，不能把加载中误判为已失效。
      await Promise.all([
        claudePreferences.model || claudePreferences.reasoningEffort ? loadModels(false, { includeAvailable: true }) : null,
        claudePreferences.permissionMode && session.capabilities?.sessionPermissions === true ? loadInstancePermissionCatalog(instanceId) : null
      ]);
    } catch (error) {
      if (current()) notice(`无法确认 Claude 新会话设置：${error.message}`, true);
      return;
    }
    if (!current() || busy || stopping || pending || !connected) return;
    if (claudePreferences.permissionMode && session.capabilities?.sessionPermissions === true) {
      const catalog = instancePermissionCatalogs.get(instanceId);
      if (catalog?.scope !== privateInputScope(session) || catalog?.error) return;
    }
  }
  let draft = snapshotDraft();
  const prefix = goalPrefix(draft.text, draft.skills);
  if (prefix) {
    if (!selectGoalCommand(prefix)) return;
    draft = snapshotDraft();
  }
  const goalMode = !!draft.goalMode;
  if (goalMode) {
    const reason = goalComposerReason({ submitting: true });
    if (reason) { notice(reason, true); return; }
  }
  const scheduledAt = Number.isSafeInteger(draft.scheduledAtMs) ? draft.scheduledAtMs : null;
  if (scheduledAt && (creating || selectedGoal)) {
    notice("定时发送只支持已有会话，且不能与目标模式同时使用。", true);
    return;
  }
  if (scheduledAt && (scheduledAt <= Date.now() || scheduledAt > Date.now() + 24 * 60 * 60 * 1000)) {
    notice("定时发送时间已失效，请重新选择未来 24 小时内的时间。", true);
    return;
  }
  const prompt = composerPrompt(draft.text, draft.skills, goalMode);
  const images = composerAttachmentsPayload();
  const files = composerLocalInputsPayload();
  if (images.some(image => image.staged) && (isClaude() || session.capabilities?.desktopCodexImageInput !== true)) {
    notice("草稿中的原图只能在支持原图发送的本机 Codex 会话使用，附件已保留。", true);
    return;
  }
  // 用户要求浏览器也能发送拖入/粘贴的文件；这里只检查服务能力，不再要求本机路径读取桥。
  if (files.length && session.capabilities?.localFileInput !== true) {
    notice("当前远程服务尚不支持文件和文件夹发送，请更新服务；附件已保留。", true);
    return;
  }
  if (images.length && !backendCapability("imageInput")) {
    notice("当前服务版本尚不支持图片发送，附件已保留，请先更新程序。", true);
    return;
  }
  if (!prompt && !images.length && !files.length) { notice(goalMode ? "请描述目标或添加附件" : "请输入消息或添加附件"); return; }
  if (!creating && (!selectedId || thread?.id !== selectedId)) return;
  const queueOnly = !scheduledAt && !isClaude() && !creating && !goalMode && compactionLocked();
  if (queueOnly) {
    const reason = compactionQueueSendReason();
    if (reason) { notice(reason, true); return; }
  }
  const id = selectedId;
  const version = selectionVersion;
  const scope = privateInputScope(session), generation = recoveryGeneration;
  const currentAuthorization = () => scope === privateInputScope(session) && generation === recoveryGeneration;
  const wasCreating = creating;
  const queueReadVersion = queueVersion;
  const rememberedModel = creating && !selectedModel ? rememberedNewChatModel() : null;
  const model = selectedModel || rememberedModel?.model
    || (selectedEffort || nativeCodexDefaults || creating && selectedDraftFast ? currentModelId() : ""),
    reasoningEffort = selectedEffort || (rememberedModel || nativeCodexDefaults ? currentEffort() : "");
  // 用户要求 Codex 执行或压缩期间仅保留模型/深度选择，排队消息不携带新设置；停止或完成后的新发送才应用。
  const modelChoiceApplies = creating || isClaude() || !isActiveThread() && !queueOnly;
  const payload = {
    prompt, instanceId: activeInstanceId(), ...(creating ? { projectId: $("#project-select").value, backend: activeBackend() } : {
      threadId: id,
      ...(!goalMode && session.capabilities?.queueSendRecovery === true ? { clientUserMessageId: randomId() } : {})
    }),
    ...(goalMode ? { goalMode: true } : {}),
    ...(scheduledAt ? { sendAtMs: scheduledAt, scheduledAtMs: scheduledAt } : {}),
    ...(queueOnly ? { queueOnly: true } : {}),
    ...(modelChoiceApplies && model ? { model } : {}),
    // 显式草稿优先；Claude 有有效记忆时恢复原选择，否则仍继承实例配置，不能把展示值固化为覆盖。
    ...(creating && session.capabilities?.sessionPermissions === true
      && (draft.permissionMode || (isClaude() && newClaudePermissionMode()) || $("#permission-button")?.value)
      ? { permissionMode: draft.permissionMode || (isClaude() ? newClaudePermissionMode() || "configured"
        : nativeCodexDefaults ? newCodexPermissionMode() : $("#permission-button").value) } : {}),
    // 新建只传本次草稿的显式选择；已有会话由原生会话偏好控制，发送不增加额外设置请求。
    ...(creating && session.capabilities?.fastMode && !isClaude() ? {
      fastMode: remoteFastState?.key === remoteFastKey() && remoteFastState.supported && selectedDraftFast
    } : {}),
    ...(modelChoiceApplies && reasoningEffort ? { reasoningEffort } : {}),
    ...(draft.skills.length ? { skills: draft.skills.map(({ name, path }) => ({ name, path })) } : {}),
    ...(images.length ? { images } : {}),
    ...(files.length ? { files } : {})
  };
  if (creating && !payload.projectId && !backendCapability("projectlessCreation")) {
    notice("当前服务版本尚不支持无项目会话，草稿已保留，请先更新程序或选择已有项目。", true);
    return;
  }
  submittedDrafts.set(payload, { ...draft, modelChoice: {
    version: modelChoiceVersion, model: selectedModel, effort: selectedEffort
  } });
  const action = wasCreating ? "create" : goalMode ? "setGoal" : scheduledAt ? "createScheduledMessage" : "send";
  const goalRecord = goalMode ? {
    id: randomId(), action, payload, draft, goalReadVersion: goalVersion,
    deviceId: session.device.id, selectionToken, time: Date.now()
  } : null;
  const submission = { payload, phase: "sending" };
  const sendDisplay = action === "send" ? rememberSendDisplay(payload) : null;
  setSending(submission);
  setBusy(true); updateComposer();
  let requestStarted = false;
  try {
    if (wasCreating && payload.backend === "claude" && session.capabilities?.claudeProjectTrust === true) {
      const currentCreation = () => currentAuthorization() && creating && selectionVersion === version
        && activeInstanceId() === payload.instanceId && $("#project-select").value === payload.projectId;
      if (!await prepareClaudeProjectCreation(payload, currentCreation)) {
        submittedDrafts.delete(payload);
        return;
      }
    }
    requestStarted = true;
    const result = action === "createScheduledMessage"
      ? await submitScheduledMessage(payload)
      : await mutate(action, payload, { reliable: true, receiptId: goalRecord?.id });
    if (!currentAuthorization()) return;
    if (result.permissionWarning) notice(result.permissionWarning, true);
    if (goalMode) {
      await applyGoalComposerReceipt(pending?.id === goalRecord.id ? pending : goalRecord,
        { state: "completed", result }, { allowSelection: true });
      return;
    }
    if (action === "createScheduledMessage") {
      clearSubmittedDraft(payload, selectionVersion === version);
      await reloadScheduledMessages(id);
      notice(`已设置定时发送：${scheduledTimeLabel(scheduledAt)}`);
      return;
    }
    const queued = !wasCreating && applyQueuedSendResult(id, result, queueReadVersion, payload);
    clearSubmittedDraft(payload, selectionVersion === version);
    if (queued) {
      return;
    }
    submission.phase = "syncing"; updateComposer();
    void loadSidebar(null, { fresh: true }).catch(error => {
      if (currentAuthorization()) recordDiagnostic("remote.send.sidebar", {
        phase: "failure", errorType: error?.name || "Error"
      });
    });
    if (selectionVersion === version) {
      if (result.threadId && result.threadId !== selectedId) {
        if (wasCreating) await selectCreatedThread(result.threadId);
        else await selectThread(result.threadId);
      }
      else await reloadThread();
    }
  } catch (error) {
    if (!currentAuthorization()) return;
    if (!requestStarted) submittedDrafts.delete(payload);
    finishSendDisplay(sendDisplay, !pending);
    if (goalMode) {
      const record = pending?.id === goalRecord.id ? pending : goalRecord;
      const outcome = await applyGoalComposerReceipt(record, error.goalReceipt || {
        state: "unknown", result: error.result || { ok: false, outcomeUnknown: true, message: error.message }
      }, { allowSelection: true });
      if (outcome === "pending" && pending?.id === goalRecord.id) {
        clearTimeout(pendingTimer);
        setPendingTimer(setTimeout(() => { if (pending?.id === goalRecord.id) void resolvePending(); }, 2000));
      }
      return;
    }
    if (error.pendingFeedback) updateComposer();
    else if (error.deliveryPending) notice("正在自动核对原生接收状态，不会重发消息。");
    else if (submission.phase === "syncing") recordDiagnostic("remote.send.reload", {
      phase: "failure", errorType: error?.name || "Error"
    });
    else notice(error.message, true);
  } finally {
    finishSendDisplay(sendDisplay);
    if (sending === submission) { setSending(null); setBusy(false); updateComposer(); }
  }
}
export async function stopGeneration() {
  // 压缩的实时状态可能先于线程 active 到达；与按钮共用已确认压缩判断，点击后才真正派发停止。
  if (stopping || busy || threadReadOnly() || !connected || !selectedId
      || !isActiveThread() && !compactionInProgress()) return;
  const id = selectedId, version = selectionVersion, claude = isClaude();
  setStopping(true); updateComposer();
  try {
    const result = await mutate("stop", { threadId: id });
    // Claude 的停止只中断当前执行，没有后台终端清理；原生 CLI 一直不响应停止请求时服务端会强制结束它的进程，
    // 必须明确告诉用户（不能静默成功），后续发送会自动重新接上原会话。
    notice(result?.cleanupPending ? result.message || "会话已停止，后台终端清理尚未确认"
      : result?.forced ? "Claude 没有响应停止请求，已强制结束它的进程；会话已保留，可以继续发送。"
      : claude ? "已停止" : "已请求停止并清理此会话的后台终端");
    if (version === selectionVersion && selectedId === id) await reloadThread();
  } catch (error) { notice(error.message, true); }
  finally { setStopping(false); updateComposer(); }
}

export function mountComposerScroll() {
  const form = $("#composer"), input = $(".composer-input", form), messages = $("#messages");
  const mobile = matchMedia("(max-width: 760px)");
  let reveal = 1, touch = null;
  const setReveal = value => {
    reveal = Math.max(0, Math.min(1, value));
    if (reveal === 1) input.style.removeProperty("--composer-rows");
    else input.style.setProperty("--composer-rows", `${reveal}fr`);
    input.style.setProperty("--composer-reveal", String(reveal));
    form.style.setProperty("--composer-toolbar-gap", `${Math.round(6 * reveal)}px`);
    input.style.pointerEvents = reveal === 0 ? "none" : "";
  };
  const expand = () => setReveal(1);
  const scroll = (delta, target) => {
    if (!mobile.matches || !delta || !selectedId || creating || editingMessage || editingQueuedMessage
        || document.activeElement === $("#prompt") || !$("#skill-menu").hidden || !$("#model-settings").hidden
        || activePopup || document.querySelector("dialog[open]") || $(".workspace").classList.contains("sidebar-open")) return;
    for (let node = target instanceof Element ? target : target.parentElement; node && node !== messages; node = node.parentElement) {
      if (node.scrollHeight > node.clientHeight + 1 && /auto|scroll/.test(getComputedStyle(node).overflowY)
          && (delta < 0 ? node.scrollTop > 0 : node.scrollTop + node.clientHeight < node.scrollHeight - 1)) return;
    }
    if (delta < 0) {
      if (messages.scrollTop <= 0 || messages.scrollHeight <= messages.clientHeight + 1) return;
      setFollowMessages(false);
    }
    const distance = Math.max(160, input.firstElementChild.scrollHeight * 2);
    setReveal(reveal + delta / distance);
  };
  const onWheel = event => {
    if (event.ctrlKey || event.defaultPrevented || Math.abs(event.deltaX) > Math.abs(event.deltaY)) return;
    const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? messages.clientHeight : 1;
    scroll(event.deltaY * unit, event.target);
  };
  const onTouchStart = event => {
    touch = event.touches.length === 1 ? { x: event.touches[0].clientX, y: event.touches[0].clientY } : null;
  };
  const onTouchMove = event => {
    const previous = touch;
    onTouchStart(event);
    if (!previous || !touch || event.defaultPrevented) return;
    const delta = previous.y - touch.y;
    if (Math.abs(delta) >= Math.abs(previous.x - touch.x)) scroll(delta, event.target);
  };
  const onTouchEnd = () => { touch = null; };
  const onFormClick = event => {
    if (reveal < 1 && event.target === form) {
      expand(); $("#prompt").focus({ preventScroll: true });
    }
  };
  const onMediaChange = () => { touch = null; expand(); resizeComposerInput(); };
  messages.addEventListener("wheel", onWheel, { passive: true });
  messages.addEventListener("touchstart", onTouchStart, { passive: true });
  messages.addEventListener("touchmove", onTouchMove, { passive: true });
  messages.addEventListener("touchend", onTouchEnd, { passive: true });
  messages.addEventListener("touchcancel", onTouchEnd, { passive: true });
  form.addEventListener("focusin", expand);
  form.addEventListener("click", onFormClick);
  mobile.addEventListener("change", onMediaChange);
  return {
    expand,
    get collapsed() { return mobile.matches && reveal < 1; },
    dispose() {
      messages.removeEventListener("wheel", onWheel);
      messages.removeEventListener("touchstart", onTouchStart);
      messages.removeEventListener("touchmove", onTouchMove);
      messages.removeEventListener("touchend", onTouchEnd);
      messages.removeEventListener("touchcancel", onTouchEnd);
      form.removeEventListener("focusin", expand);
      form.removeEventListener("click", onFormClick);
      mobile.removeEventListener("change", onMediaChange);
    }
  };
}

export function syncComposerMetrics() {
  if (!$("#session-metrics")) return;
  const context = composerMetricsContext();
  if (!context) { resetComposerMetrics(); return; }
  if (composerMetricsState?.key !== context.key) {
    resetComposerMetrics();
    const saved = currentThreadDisplay()?.sessionMetrics;
    setComposerMetricsState({ key: context.key, value: saved?.source === context.source ? normalizeComposerMetrics(saved) : null,
      checkedAt: 0, pending: false, controller: null, timer: null, fingerprint: "", retryAt: 0 });
  }
  const state = composerMetricsState, active = isActiveThread();
  const fingerprint = JSON.stringify([composerMetricsLatestTurn(state)?.id, active]);
  if (state.fingerprint !== fingerprint) { state.fingerprint = fingerprint; state.checkedAt = 0; }
  renderComposerMetrics();
  clearTimeout(state.timer); state.timer = null;
  if (!connected) { state.controller?.abort(); return; }
  if (state.pending) return;
  // 统计只在可见的当前会话后台读取；执行中每 2 秒、空闲每 15 秒，发送与停止均不等待统计。
  const interval = active ? 2000 : 15000;
  const remaining = Math.max(state.retryAt - Date.now(), state.checkedAt + interval - Date.now());
  if (remaining > 0) {
    state.timer = setTimeout(() => { if (composerMetricsState === state) syncComposerMetrics(); }, remaining);
    return;
  }
  state.pending = true;
  const controller = new AbortController();
  state.controller = controller;
  void api("/api/read/sessionMetrics", context.payload, undefined, controller.signal, { resourcePriority: "background" })
    .then(result => {
      if (composerMetricsState !== state || state.controller !== controller || controller.signal.aborted
          || composerMetricsContext()?.key !== state.key
          || result?.ok !== true || result.threadId !== context.payload.threadId || result.backend !== context.payload.backend
          || result.metrics?.source !== context.source) return;
      const value = normalizeComposerMetrics(result.metrics, state.value);
      // 新轮次可能先只返回累计或空统计；这时保留上一轮快照，等本轮速度、步数或 Token 实测值到达再替换。
      if (value && (value.totalIncomplete || !composerMetricsHasTurn(state.value) || composerMetricsHasTurn(value))) {
        state.value = value;
        if (thread?.id === result.threadId) thread.sessionMetrics = value;
        rememberThreadDisplay({ id: result.threadId, sessionMetrics: value }, { persist: !isActiveThread() });
      }
      state.retryAt = 0;
    })
    .catch(() => {
      if (composerMetricsState === state && state.controller === controller && !controller.signal.aborted)
        state.retryAt = Date.now() + 15000;
    })
    .finally(() => {
      if (composerMetricsState !== state || state.controller !== controller) return;
      state.checkedAt = !controller.signal.aborted && state.fingerprint === fingerprint ? Date.now() : 0;
      state.pending = false; state.controller = null;
      syncComposerMetrics();
    });
}
