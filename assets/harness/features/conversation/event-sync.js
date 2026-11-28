// 实时事件同步状态：已通知的状态与 Token 用量版本，以及延迟重载与侧栏刷新调度。

import { notice } from "../../ui/notice.js";
import { loadSidebar, reloadThread } from "../../core/actions.js";
import { pageLeaving, selectedId, selectionVersion, session, turns } from "../../core/state.js";
import { privateInputScope } from "../session/capabilities.js";
import { foregroundReadGeneration, refreshTimer, setRefreshTimer, setSidebarTimer, sidebarTimer } from "../session/connection-state.js";
import { threadDeleted } from "../sidebar/deletion-state.js";

export let tokenUsageVersion = 0, notifiedTokenUsage = null;
export function setTokenUsageVersion(value) { return tokenUsageVersion = value; }
export function setNotifiedTokenUsage(value) { return notifiedTokenUsage = value; }
export let statusVersion = 0, notifiedStatus = null;
export function setStatusVersion(value) { return statusVersion = value; }
export function setNotifiedStatus(value) { return notifiedStatus = value; }
let emptyTurnReload = null;
export function scheduleReload(emptyTurn = null) {
  if (refreshTimer) {
    // 停止、补缺、重连等其他理由也要求重读时，不能被压缩小事件取消。
    if (!emptyTurn || emptyTurn.threadId !== emptyTurnReload?.threadId
        || emptyTurn.turnId !== emptyTurnReload?.turnId) emptyTurnReload = null;
    return;
  }
  emptyTurnReload = emptyTurn;
  setRefreshTimer(setTimeout(() => {
    setRefreshTimer(null);
    const check = emptyTurnReload;
    emptyTurnReload = null;
    // Codex 可能先发空 turn/started，再发原生压缩 item；在原有刷新时间点检查，不为完整的压缩行重读历史。
    if (check && check.threadId === selectedId) {
      const items = turns.get(check.turnId)?.items;
      if (items?.length === 1 && items[0].type === "contextCompaction") return;
    }
    const id = selectedId, version = selectionVersion, scope = privateInputScope(session),
      generation = foregroundReadGeneration;
    void reloadThread().catch(error => {
      // 用户要求删除成功提示不被迟到错误覆盖；错误传播期间也可能切换会话或授权，展示前须再次核对。
      if (!pageLeaving && id && selectedId === id && version === selectionVersion
          && generation === foregroundReadGeneration && scope === privateInputScope(session) && !threadDeleted(id))
        notice(error.message, true);
    });
  }, 450));
}
export function scheduleSidebarRefresh() {
  clearTimeout(sidebarTimer);
  setSidebarTimer(setTimeout(() => void loadSidebar(null, { fresh: true }).catch(() => {}), 1200));
}
