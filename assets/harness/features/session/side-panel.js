// PC 侧边会话/子智能体面板与独立会话窗口的页面模式，以及向外层标签同步执行与未读状态。

import { connected, creating, pageLeaving, selectedId, selectionVersion, session, thread } from "../../core/state.js";
import { reloadThread, updateComposer } from "../../core/actions.js";
import { randomId } from "../../core/util.js";
import { desktopWindowBridge } from "../../platform.js";
import { $ } from "../../ui/dom.js";
import { api } from "../api.js";
import { privateInputScope, threadReadOnly } from "./capabilities.js";
import { markVisibleThreadRead } from "../messages/user-content.js";
import { sidebarAttentionState } from "../sidebar/list-state.js";

// 用户要求 PC 子会话参照侧边会话使用右侧标签内的独立页面；外层标签已提供名称，
// 因此面板内隐藏整段顶部信息、主侧栏与返回入口，子智能体仍沿用原生只读限制。
// 文件工具在本面板内打开，避免嵌套第二个标签面板；手机普通关联会话页不使用此标记。
export const workspaceSidePanel = location.pathname === "/side-thread";
// 按页面地址标记独立会话窗口或 PC 侧边面板模式，样式据此隐藏侧栏等区域。
export function applyWorkspaceWindowMode() {
  // 用户要求独立窗口保留完整会话/标签页，仅隐藏左侧列表；不能复用会禁用顶部与标签功能的侧边 iframe 模式。
  // 远程旧链接仍可进入原会话，但 sessionWindow 参数不再让网页进入独立窗口布局。
  if (!workspaceSidePanel && session?.transport === "desktop" && desktopWindowBridge("open")
      && new URLSearchParams(location.search).get("sessionWindow") === "1")
    document.documentElement.dataset.sessionWindow = "true";
  else delete document.documentElement.dataset.sessionWindow;
  // 侧边 iframe 可在鉴权完成前收到外层显隐通知，二次应用窗口模式不能重置它。
  if (workspaceSidePanel && document.documentElement.dataset.workspaceSidePanel !== "true") {
    document.documentElement.dataset.workspaceSidePanel = "true";
    document.documentElement.dataset.workspaceSidePanelVisible = "false";
  }
}
// 用户要求独立会话窗口在任务栏显示会话标题（如会话窗口显示会话标题、文件预览窗口显示文件标题）：
// 页面标题跟随顶部会话标题，Windows 客户端监听页面标题变化后更新原生窗口标题，不向网页开放窗口命令。
// 顶部标题在多处更新，且 shell() 会重建整个 #app，因此只监听 #app 子节点替换并重新绑定标题元素；
// 默认标题“远程会话”及无标题时回退页面原标题。文件预览页由 workspace-preview.js 自行设置文件标题。
let sessionWindowTitleSyncInstalled = false;
export function installSessionWindowTitleSync() {
  if (sessionWindowTitleSyncInstalled || document.documentElement.dataset.sessionWindow !== "true") return;
  const host = document.getElementById("app");
  if (!host) return;
  sessionWindowTitleSyncInstalled = true;
  const fallback = document.title;
  let heading = null, applied = fallback;
  const sync = () => {
    const name = (heading?.textContent || "").trim();
    const next = name && name !== "远程会话" ? `${name} · Elves` : fallback;
    if (next !== applied) document.title = applied = next;
  };
  const headingObserver = new MutationObserver(sync);
  const bind = () => {
    const current = document.getElementById("thread-title");
    if (current !== heading) {
      headingObserver.disconnect();
      heading = current;
      if (heading) headingObserver.observe(heading, { childList: true, characterData: true, subtree: true });
    }
    sync();
  };
  new MutationObserver(bind).observe(host, { childList: true });
  bind();
}
let workspaceSidePanelStateSignature = "";
function sideComposerEditor(id) {
  if (!session || !id || id !== selectedId || thread?.id !== id || thread.viewKind !== "side"
      || creating || pageLeaving || document.hidden || threadReadOnly(id)
      || thread.sideThreadExpired || thread.sideThreadClosed) return null;
  const prompt = $("#prompt");
  return prompt?.isConnected && !prompt.disabled && prompt.isContentEditable
    && !prompt.closest("[hidden],[inert]") && !document.querySelector("dialog[open]") ? prompt : null;
}
function focusSideComposer(id) {
  // 用户要求主动打开可写侧边后就能输入；不改变草稿内容、选区或正文滚动位置。
  if (workspaceSidePanel && document.documentElement.dataset.workspaceSidePanelVisible !== "true") return;
  sideComposerEditor(id)?.focus({ preventScroll: true });
}
let requestedSideInput = null;
export function requestSideComposerFocus(id) {
  const scope = privateInputScope(session);
  if (scope && !workspaceSidePanel && id === selectedId)
    requestedSideInput = { id, scope, version: selectionVersion };
}
export function syncRequestedSideInput() {
  const request = requestedSideInput;
  if (!request) return;
  if (request.id !== selectedId || request.version !== selectionVersion || request.scope !== privateInputScope(session)
      || pageLeaving || document.hidden || document.hasFocus?.() === false
      || thread?.id === selectedId && (thread.viewKind !== "side" || threadReadOnly() || thread.sideThreadExpired || thread.sideThreadClosed)) {
    requestedSideInput = null;
    return;
  }
  // 手机详情与连接补读可能并行；由真正启用当前输入框的那次更新消费请求。
  const prompt = sideComposerEditor(request.id);
  if (!prompt) return;
  requestedSideInput = null;
  prompt.focus({ preventScroll: true });
}
export function syncWorkspaceSidePanelState(force = false) {
  if (!workspaceSidePanel || window.parent === window || creating || !selectedId || thread?.id !== selectedId) return;
  const status = thread.status?.status || thread.status;
  const type = typeof status === "string" ? status : status?.type;
  const attention = sidebarAttentionState(thread);
  // 用户要求侧边有审批或问答时显示未读蓝点；待处理请求不能因查看正文的已读回执而消失。
  const unread = attention === "waiting"
    || (typeof thread.hasUnreadTurn === "boolean" ? thread.hasUnreadTurn : attention === "unread");
  // 用户要求侧边标签显示真实执行状态：沿用原生状态及等待判断，不把发送中、排队或待确认占位当成执行中。
  const running = connected && !pageLeaving && ["active", "inProgress", "running"].includes(type)
    && attention !== "waiting";
  const composerReady = !!sideComposerEditor(selectedId);
  const signature = JSON.stringify([selectedId, running, unread, composerReady]);
  if (!force && signature === workspaceSidePanelStateSignature) return;
  workspaceSidePanelStateSignature = signature;
  window.parent.postMessage({ type: "codex-elves-side-panel-state", threadId: selectedId, running, unread, composerReady }, location.origin);
}
// PC 侧边面板接收外层标签的显隐通知，只对实际显示的标签确认已读。
export function installSidePanelMessageListener() {
  for (const type of ["pointerdown", "keydown", "visibilitychange"])
    document.addEventListener(type, () => { requestedSideInput = null; }, true);
  // 页面后台与外层标签隐藏都结束查看租约；失联或直接关页由服务端 90 秒租约兜底，不派发模型操作。
  document.addEventListener("visibilitychange", () => syncSideThreadPresence());
  window.addEventListener("pagehide", () => syncSideThreadPresence(true));
  window.addEventListener("pageshow", () => syncSideThreadPresence());
  window.addEventListener("message", event => {
    if (!workspaceSidePanel || window.parent === window || event.source !== window.parent || event.origin !== location.origin) return;
    if (event.data?.type === "codex-elves-side-panel-focus") {
      focusSideComposer(event.data.threadId);
      return;
    }
    if (event.data?.type !== "codex-elves-side-panel-visibility" || typeof event.data.visible !== "boolean") return;
    // iframe 的 document.hidden 不随标签隐藏改变；只对实际显示的侧边标签确认已读，后台标签继续保留未读状态。
    document.documentElement.dataset.workspaceSidePanelVisible = String(event.data.visible);
    syncSideThreadPresence();
    updateComposer(); // 切换可见标签立即开始或停止该侧边自己的统计读取。
    syncWorkspaceSidePanelState(true);
    if (event.data.visible) {
      // 标签从隐藏恢复时重新开启原只读补齐；隐藏期间不会轮询或为了查看而 resume 原生线程。
      void reloadThread().catch(() => {});
      void markVisibleThreadRead().catch(() => {});
    }
  });
}

let sidePresenceViewId = null, sidePresenceSequence = 0, sidePresenceRecord = null, sidePresenceTimer = null;
// 与服务端 side_idle::VIEW_LEASE 的 90 秒同步：30 秒续租只代表当前页面实际可见，历史轮询不续租。
const SIDE_PRESENCE_INTERVAL = 30000;
export function syncSideThreadPresence(leaving = false) {
  clearTimeout(sidePresenceTimer); sidePresenceTimer = null;
  const scope = privateInputScope(session);
  const eligible = !!scope && session?.capabilities?.sideIdleRelease === true && thread?.id === selectedId
    && thread.serviceSide === true && !thread.sideThreadClosed && !thread.sideThreadExpired;
  const visible = eligible && connected && !leaving && !pageLeaving && !document.hidden
    && (!workspaceSidePanel || document.documentElement.dataset.workspaceSidePanelVisible === "true");
  const id = eligible ? selectedId : null;
  const previous = sidePresenceRecord, now = Date.now();
  const send = (threadId, isVisible) => {
    sidePresenceViewId ||= randomId();
    void api("/api/read/sidePresence", { threadId, viewId: sidePresenceViewId,
      sequence: ++sidePresenceSequence, visible: isVisible }, undefined, undefined,
    { resourcePriority: "background" }).catch(() => {});
  };
  if (previous?.scope === scope && previous.visible && (previous.id !== id || !visible))
    send(previous.id, false);
  const changed = !previous || previous.scope !== scope || previous.id !== id || previous.visible !== visible;
  if (!id) { sidePresenceRecord = null; return; }
  if (visible && (changed || now - previous.sentAt >= SIDE_PRESENCE_INTERVAL)) send(id, true);
  sidePresenceRecord = { scope, id, visible,
    sentAt: visible && (changed || now - previous.sentAt >= SIDE_PRESENCE_INTERVAL) ? now : previous?.sentAt ?? now };
  if (visible) sidePresenceTimer = setTimeout(() => syncSideThreadPresence(),
    Math.max(1, SIDE_PRESENCE_INTERVAL - (Date.now() - sidePresenceRecord.sentAt)));
}
