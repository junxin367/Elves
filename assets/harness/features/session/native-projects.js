// 原生“添加文件夹”只交付已登记的项目 ID；页面确认展示后才退役原生待处理选择。
import { loadSidebar, showWelcome, startNewChat } from "../../core/actions.js";
import { selectionVersion, session, sidebar } from "../../core/state.js";
import { notice } from "../../ui/notice.js";
import { showConversationPage } from "../main-navigation.js";
import { privateInputScope } from "./capabilities.js";

let running = null;
export function openPendingNativeProject() {
  const bridge = globalThis.__ELVES_CLIENT__;
  if (typeof bridge?.projectNavigation !== "function" || !session || session.readOnly
      || document.documentElement.dataset.sessionWindow === "true") return Promise.resolve();
  if (running) return running;
  const scope = privateInputScope(session);
  const current = () => !!session && privateInputScope(session) === scope && !session.readOnly;
  let navigation = selectionVersion;
  running = (async () => {
    let pending = await bridge.projectNavigation();
    while (pending && current()) {
      if (!/^elves-project-[0-9a-f]{64}$/.test(pending.projectId)
          || typeof pending.requestId !== "string" || !pending.requestId || pending.requestId.length > 100) {
        throw new Error("原生项目选择无效，请重新添加文件夹。");
      }
      await loadSidebar(null, { fresh: true });
      if (!current()) return;
      if (selectionVersion !== navigation) {
        // 文件夹已经保存；用户后续手动切换会话时取消旧的自动跳转，不能迟到覆盖当前会话。
        await bridge.projectNavigation(pending.requestId);
        return;
      }
      if (!sidebar?.projects?.some(project => project.id === pending.projectId)) {
        throw new Error("所选文件夹尚未出现在项目列表，请刷新后重试。");
      }
      showConversationPage();
      showWelcome({ startNew: false });
      const selected = startNewChat(pending.projectId);
      navigation = selectionVersion;
      await selected;
      if (!current()) return;
      pending = await bridge.projectNavigation(pending.requestId);
    }
  })().catch(error => { if (current()) notice(error.message || "无法打开所选文件夹", true); })
    .finally(() => { running = null; });
  return running;
}

export function installNativeProjectListener() {
  window.addEventListener("elves:project-added", () => void openPendingNativeProject());
  window.addEventListener("focus", () => void openPendingNativeProject());
}
