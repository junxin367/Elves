// 页面生命周期：前后台切换时暂停与恢复，以及连接恢复重试。

import { notice } from "../../ui/notice.js";
import { $ } from "../../ui/dom.js";
import { selectThread, showWelcome } from "../../core/actions.js";
import { pageLeaving, selectedId, selectionToken, session, setPageLeaving } from "../../core/state.js";
import { recoverQuestionReceipts } from "../approval-requests.js";
import { resetClaudeUsage, syncClaudeUsage } from "../composer/claude-usage.js";
import { syncComposerMetrics } from "../composer/composer.js";
import { closeContextUsageTip, resetComposerMetrics } from "../composer/metrics.js";
import { promptSelection, setPromptSelection } from "../composer/prompt-editor.js";
import { stopCatalogRefresh } from "../composer/skill-catalog.js";
import { refreshCachedCatalogs } from "../composer/skills.js";
import { rememberTranscript } from "../conversation/transcript.js";
import { stopGoalLiveTimer, syncGoalLiveTimer } from "../goal/state.js";
import { stopActivityTimer, syncActivityTimer } from "../messages/activity-state.js";
import { drainPendingNotificationOpen } from "../notifications.js";
import { finishQueueDrag } from "../queue/queue.js";
import { normalizeRelatedView, privateInputScope } from "./capabilities.js";
import {
  cancelConnectionProbe, connecting, initialAuthenticationRequired, recordRuntimeDiagnostic,
  setStreamRetry, streamRetry, withRecoveryErrorSilence
} from "./connection-state.js";
import { refreshConnectedSession, setConnection } from "./connection.js";
import {
  connect, ensureStream, probeConnectionRecovery, rememberPageInputs, retireForegroundReads,
  startConnectionPoll, stopConnectionPoll, suspendPage
} from "./lifecycle.js";
import { handleInitialConnectionError, showConnectionRecovery } from "./login.js";
import { relatedExitTransition, relatedHistoryKey, relatedSessionView } from "./related-state.js";
import { scheduleRelatedOperationCheck, scheduleRelatedSessionCheck } from "./related.js";
import { rememberReadingPosition } from "./thread-cache.js";
import { startClientUnreadBadgePolling, stopClientUnreadBadgePolling, syncClientUnreadBadge } from "../sidebar/list.js";

let promptFocusSnapshot = null;
function rememberPromptFocus(target = document.activeElement) {
  const prompt = $("#prompt");
  if (prompt && target === prompt) promptFocusSnapshot = {
    prompt, selection: promptSelection(prompt), threadId: selectedId, selectionToken, scope: privateInputScope(session)
  };
}
function restorePromptFocus() {
  const snapshot = promptFocusSnapshot;
  if (!snapshot || document.hidden || document.hasFocus?.() === false) return;
  const prompt = $("#prompt");
  const active = document.activeElement;
  if (snapshot.prompt !== prompt || snapshot.threadId !== selectedId || snapshot.selectionToken !== selectionToken
      || snapshot.scope !== privateInputScope(session) || !prompt?.isConnected || prompt.disabled
      || prompt.closest?.("[inert]") || document.querySelector?.("dialog[open]")
      || active && active !== prompt && active !== document.body && active !== document.documentElement) {
    promptFocusSnapshot = null;
    return;
  }
  // 用户要求独立客户端切回前台后继续编辑；只恢复后台前确实位于编辑框的焦点和选区。
  prompt.focus({ preventScroll: true });
  // WebView 尚未拿到原生键盘焦点时 focus 可能无效；不能先消费快照，后续真实 focus 仍须恢复。
  if (document.activeElement !== prompt) return;
  promptFocusSnapshot = null;
  const selection = snapshot.selection;
  setPromptSelection(prompt, selection.start, selection.end, selection);
}
function installPromptFocusTracking() {
  // 用户反馈 Alt+Tab、系统复制后无法续写：编辑框 focusout 可能早于 window.blur，须在目标丢失前保存选区。
  document.addEventListener("focusout", event => {
    if (event.target !== $("#prompt")) return;
    if (event.relatedTarget) promptFocusSnapshot = null;
    else rememberPromptFocus(event.target);
  }, true);
  document.addEventListener("focusin", event => {
    if (event.target !== $("#prompt") && event.target !== document.body && event.target !== document.documentElement)
      promptFocusSnapshot = null;
  }, true);
  // 主动点击页面其他位置属于新的焦点意图，不能让旧的后台快照在稍后重新抢回编辑框。
  document.addEventListener("pointerdown", event => {
    if (!$("#prompt")?.contains(event.target)) promptFocusSnapshot = null;
  }, true);
}
function resumePage() {
  const wasLeaving = pageLeaving;
  setPageLeaving(false);
  recordRuntimeDiagnostic("page.resume");
  if (!session) { retryConnectionRecovery(); return; }
  if (!wasLeaving) return;
  refreshCachedCatalogs();
  syncGoalLiveTimer();
  startConnectionPoll(session.device.id);
  scheduleRelatedOperationCheck();
  ensureStream({ suppressErrors: wasLeaving });
  void withRecoveryErrorSilence(() => refreshConnectedSession())
    .catch(() => {}).finally(drainPendingNotificationOpen);
}
export function retryConnectionRecovery({ suppressStreamErrors = false, recoveryProbe = false } = {}) {
  if (pageLeaving || document.hidden || navigator.onLine === false || initialAuthenticationRequired || connecting) return;
  if (!session) { void connect().catch(handleInitialConnectionError); return; }
  if (recoveryProbe) {
    void probeConnectionRecovery().catch(() => {});
    return;
  }
  void recoverQuestionReceipts();
  refreshCachedCatalogs();
  ensureStream({ suppressErrors: suppressStreamErrors });
  void withRecoveryErrorSilence(() => refreshConnectedSession())
    .catch(() => {});
}
// 网络、可见性与页面离开/返回等页面生命周期事件。
export function installPageLifecycle() {
  installPromptFocusTracking();
  window.addEventListener("online", () => {
    retryConnectionRecovery({ recoveryProbe: true });
  });
  window.addEventListener("offline", () => {
    stopCatalogRefresh();
    retireForegroundReads("网络已离线");
    if (session) setConnection(false);
    else if (!initialAuthenticationRequired) showConnectionRecovery(new TypeError("网络已离线"));
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      stopCatalogRefresh();
      rememberPromptFocus();
      resetClaudeUsage();
      resetComposerMetrics();
      if (session) { rememberTranscript(); rememberReadingPosition(); }
      retireForegroundReads();
      stopActivityTimer();
      stopGoalLiveTimer();
      finishQueueDrag();
      closeContextUsageTip();
      stopConnectionPoll();
      cancelConnectionProbe();
      clearTimeout(streamRetry); setStreamRetry(null);
      startClientUnreadBadgePolling();
      return;
    }
    stopClientUnreadBadgePolling();
    syncClientUnreadBadge();
    recordRuntimeDiagnostic("page.visible");
    restorePromptFocus();
    if (pageLeaving) { resumePage(); return; }
    syncClaudeUsage();
    syncComposerMetrics();
    syncActivityTimer();
    syncGoalLiveTimer();
    scheduleRelatedSessionCheck();
    if (session) startConnectionPoll(session.device.id);
    retryConnectionRecovery({ suppressStreamErrors: true });
  });
  window.addEventListener("pagehide", suspendPage);
  window.addEventListener("blur", () => { rememberPromptFocus(); finishQueueDrag(); });
  window.addEventListener("focus", restorePromptFocus);
  window.addEventListener("beforeunload", () => {
    recordRuntimeDiagnostic("page.beforeunload");
    // 导航可能被取消或交给 App 内置浏览器；只有 pagehide 才表示当前文档真正离开。
    rememberPageInputs();
  });
  window.addEventListener("pageshow", resumePage);
  window.addEventListener("popstate", event => {
    if (!session) return;
    const id = new URLSearchParams(location.hash.slice(1)).get("thread");
    const view = normalizeRelatedView(event.state?.[relatedHistoryKey], id);
    if (id && (relatedExitTransition || id !== selectedId || JSON.stringify(view) !== JSON.stringify(relatedSessionView)))
      void selectThread(id, { closeSidebar: false, relatedView: view }).catch(error => notice(error.message, true));
    else if (!id && selectedId) showWelcome();
  });
}
