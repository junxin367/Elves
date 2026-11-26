// 本机客户端的工作区打开入口；目录、应用检测与启动均由独立 Harness 完成。
import { button, element } from "../../ui/dom.js";
import { icon } from "../../ui/icons.js";
import { notice } from "../../ui/notice.js";
import { confirmAction } from "../../ui/dialog.js";
import { activePopup, closePopup, openPopup, renderPopup } from "../../ui/popup.js";
import { creating, nativeConnectionVersion, pageLeaving, selectedId, session, sidebar, thread } from "../../core/state.js";
import { randomId } from "../../core/util.js";
import { validateRemoteReceipt } from "../../channels.js";
import { reloadThread } from "../../core/actions.js";
import { api } from "../api.js";
import { privateInputScope, threadReadOnlyApartFromOwnership } from "./capabilities.js";
import { relatedSessionView } from "./related-state.js";
import { workspaceSidePanel } from "./side-panel.js";

// 用户要求补齐 IDEA、WSL 和 PowerShell；ID 须与独立服务 open_in.rs 的 TARGETS 同步，不能在前端再次过滤掉已支持应用。
const applicationIds = new Set(["vscode", "visualStudio", "sublimeText", "githubDesktop",
  "fileManager", "gitBash", "cmder", "rider", "intellij", "powershell", "wsl"]);
const applicationPaths = {
  vscode: '<path d="M17.25 2.8 7.57 11.6 3.4 8.43 1.5 9.5v5l1.9 1.08 4.17-3.18 9.68 8.8L22.5 19V5l-5.25-2.2Zm-.2 5.08v8.24L11.75 12l5.3-4.12Z"/>',
  // 用户要求保留浅色圆角底和蓝黄色文件夹图案；菜单图标框现与其他应用统一为对应文字的 1em。
  defaultApp: '<rect x="2" y="2" width="20" height="20" rx="5" fill="#f5f5f5"/><path d="M6 9a1.5 1.5 0 0 1 1.5-1.5h3L12 9h4.5a1.5 1.5 0 0 1 1.5 1.5v5A1.5 1.5 0 0 1 16.5 17h-9A1.5 1.5 0 0 1 6 15.5Z" fill="#eeb23e"/><path d="M8 11h8v4H8z" fill="#287acb"/><path d="M6 13.5h4l1 1.5h2l1-1.5h4v2a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 6 15.5Z" fill="#f7c454"/>',
  visualStudio: '<path d="m17.34 2.5-8.8 6.7-4.42-3.42L1.5 7.1v9.8l2.62 1.32 4.42-3.42 8.8 6.7 5.16-2.06V4.56L17.34 2.5ZM4.65 9.35 7.7 12l-3.05 2.65v-5.3Zm12.5-1.82v8.94L11.28 12l5.87-4.47Z"/>',
  githubDesktop: '<path d="M12 2a10 10 0 0 0-3.16 19.49c.5.09.68-.22.68-.48v-1.86c-2.78.6-3.37-1.18-3.37-1.18-.45-1.16-1.11-1.47-1.11-1.47-.91-.62.07-.61.07-.61 1 .07 1.53 1.03 1.53 1.03.9 1.52 2.34 1.08 2.91.83.09-.64.35-1.08.64-1.33-2.22-.25-4.55-1.11-4.55-4.94 0-1.09.39-1.98 1.03-2.68-.1-.26-.45-1.27.1-2.64 0 0 .84-.27 2.75 1.02A9.6 9.6 0 0 1 12 6.84c.85 0 1.71.11 2.51.34 1.91-1.29 2.75-1.02 2.75-1.02.55 1.37.2 2.38.1 2.64.64.7 1.03 1.59 1.03 2.68 0 3.84-2.34 4.69-4.57 4.94.36.31.68.92.68 1.85v2.74c0 .27.18.58.69.48A10 10 0 0 0 12 2Z"/>',
  fileManager: '<path d="M2 6a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2Z"/>'
};

function workspaceOpenInAvailable() {
  // 用户要求远程 PC、手机、PWA 和 Android 屏蔽宿主机应用入口；本机身份必须来自服务端，不能只判断原生桥或屏幕尺寸。
  return session?.transport === "desktop" && session?.capabilities?.workspaceOpenIn === true;
}

export function clientOpenInContext() {
  // 顶部入口另要求 Windows 独立客户端；关联阅读页及无会话页面不新增入口。
  // 用户要求会话被其他应用占用并不影响 Open in：占用只让输入区只读，这里不能因此隐藏入口（WSL 项在点击时再按占用状态处理）。
  if (typeof window.__TAURI__?.core?.invoke !== "function" || pageLeaving || creating || !selectedId
      || thread?.id !== selectedId || !workspaceOpenInAvailable() || threadReadOnlyApartFromOwnership()
      || relatedSessionView || workspaceSidePanel) return null;
  const scope = privateInputScope(session);
  if (!scope) return null;
  // 用户要求 WSL 只显示于实际执行的 WSL 会话实例；Windows 挂载目录仍可能是 WSL 会话，不能按 cwd 的路径格式推断。
  const wslInstance = typeof thread.instanceId === "string" && thread.instanceId.includes(":wsl:")
    && typeof thread.wslDistribution === "string" && !!thread.wslDistribution;
  return { scope, threadId: selectedId,
    wslInstance,
    key: JSON.stringify([scope, selectedId, nativeConnectionVersion, thread.cwd || "",
      thread.instanceId || "", thread.wslDistribution || "", thread.wslUser || ""]) };
}

export function applicationIcon(target) {
  const id = typeof target === "string" ? target : target?.target;
  const node = element("span", `workspace-open-in-app-icon workspace-open-in-${id || "fallback"}`);
  node.setAttribute("aria-hidden", "true");
  const fallback = () => {
    if (applicationPaths[id])
      node.innerHTML = `<svg viewBox="0 0 24 24" fill="currentColor">${applicationPaths[id]}</svg>`;
    else if (id === "sublimeText") node.textContent = "S";
    else if (["rider", "intellij"].includes(id)) node.append(element("span", "", id === "intellij" ? "IJ" : "RD"));
    else node.append(icon(["gitBash", "cmder", "powershell", "wsl"].includes(id) ? "terminal" : "window"));
  };
  // 用户要求 Git 与 WSL 使用已安装程序图标；仅接受服务端校验过的 PNG 数据，失败时保留原回退。
  if (typeof target?.icon === "string" && target.icon) {
    const image = element("img");
    image.alt = ""; image.src = target.icon;
    image.addEventListener("error", () => { node.replaceChildren(); fallback(); }, { once: true });
    node.append(image);
  } else fallback();
  return node;
}

function normalizeTargets(result, context) {
  if (result?.ok !== true || typeof result.root !== "string" || !result.root
      || result.root.length > 32768 || !Array.isArray(result.targets))
    throw new Error("工作区应用列表无效，请重新打开会话。");
  if (context?.path && result.path !== context.path)
    throw new Error("应用列表与所选文件不一致，请重新打开文件菜单。");
  const seen = new Set();
  // Default app 只属于正文文件的打开方式；即使目录应用列表误返回该项，顶部和会话行也不能显示或选中。
  const targets = result.targets.filter(target => target
      && (target.target === "defaultApp" ? !!context?.path : applicationIds.has(target.target))
      && (target.target !== "wsl" || context?.wslInstance === true)
      && target.available === true && typeof target.label === "string" && target.label.length <= 80
      && target.label.trim() && !seen.has(target.target) && seen.add(target.target))
    .map(({ target, label, icon, occupancy }) => {
      const installedIcon = typeof icon === "string" && icon.length <= 200000
        && /^data:image\/png;base64,[A-Za-z0-9+/]+={0,2}$/.test(icon) ? icon : null;
      // 仅 WSL 项带会话占用状态：服务端启动时会再次核对，这里只决定直接打开、先确认还是直接提示。
      const held = target === "wsl" && ["free", "harnessIdle", "harnessBusy", "external"].includes(occupancy)
        ? occupancy : null;
      return { target, label, ...(installedIcon ? { icon: installedIcon } : {}), ...(held ? { occupancy: held } : {}) };
    });
  return { root: result.root, targets };
}

function preferenceKey(context, root) {
  // 与原逻辑一致按目录记住首选应用；仅保存应用 ID，按当前服务 Origin、设备授权和目录隔离。
  return `codex-elves-open-in-v1:${JSON.stringify([context.scope, root])}`;
}
function preferredTarget(context, result) {
  let preferred = "";
  try { preferred = localStorage.getItem(preferenceKey(context, result.root)) || ""; } catch {}
  return result.targets.find(target => target.target === preferred) || result.targets[0] || null;
}

// 用户要求：在 WSL 打开 Claude 会话时，别的应用已持有就在页面内提示“已在另一个应用中打开”；
// Elves 自己持有且空闲时，弹窗询问是否关闭当前会话占用，确认后才同步关闭再打开 WSL 里的 CLI 会话；
// Elves 自己正在执行任务时不能关闭。返回 null 表示不继续，"release" 表示用户已确认关闭 Elves 的占用。
async function wslOpenDecision(chosen) {
  switch (chosen.occupancy) {
    case "external":
      notice("该会话已在另一个应用中打开，请先在那边关闭会话，或在输入区点“关闭其他占用”。", true);
      return null;
    case "harnessBusy":
      notice("会话正在执行任务，无法关闭占用。请先停止任务或等待完成后再打开 WSL。", true);
      return null;
    case "harnessIdle":
      return await confirmAction("在 WSL 中打开",
        "Elves 正在占用这个会话。继续会先关闭 Elves 对它的占用，再在 WSL 中打开同一个会话；之后要回到 Elves 继续，需要先在 WSL 里关闭该会话。",
        "关闭占用并打开") ? "release" : null;
    default:
      return "open";
  }
}

// WSL 里的 CLI 启动并登记需要一到两秒；稍后重新读取会话，让“已在另一个应用中打开”提示及时出现，用户不必手动重试。
function recheckOwnershipAfterTerminalOpen(threadId) {
  for (const delay of [2500, 7000])
    setTimeout(() => {
      if (!pageLeaving && selectedId === threadId) void reloadThread().catch(() => {});
    }, delay);
}

const workspaceOpenRequests = new Set();
async function performWorkspaceOpenIn(context, chosen, owns) {
  const key = JSON.stringify([context.scope, context.threadId, context.path || ""]);
  if (!owns(context) || workspaceOpenRequests.has(key)) return null;
  workspaceOpenRequests.add(key);
  let releaseHarness = false;
  if (chosen.target === "wsl") {
    // 弹窗期间会话或授权可能已变化；确认后重新核对身份，不能把旧点击派发给新会话。
    const decision = await wslOpenDecision(chosen).catch(() => null);
    if (!decision || !owns(context)) { workspaceOpenRequests.delete(key); return null; }
    releaseHarness = decision === "release";
  }
  let dispatching = false, timer = null, operationKey = null;
  try {
    const controller = new AbortController();
    timer = setTimeout(() => controller.abort(), 45000);
    // 顶部 Open in 与会话右键菜单共用持久回执；结果未知时仅核对同一请求，不能重复启动应用。
    // 文件操作按路径隔离回执；目录操作保留既有存储键，结果未知时不能被另一个文件的点击覆盖。
    operationKey = `codex-elves-open-in-request-v1:${JSON.stringify([context.scope, context.threadId, chosen.target,
      ...(context.path ? [context.path] : [])])}`;
    let requestId;
    try {
      requestId = sessionStorage.getItem(operationKey);
      if (!/^[A-Za-z0-9-]{1,100}$/.test(requestId || "")) {
        requestId = randomId();
        sessionStorage.setItem(operationKey, requestId);
      }
    } catch { throw new Error("无法保留工作区打开请求，请允许客户端存储后重试。"); }
    if (!owns(context)) return null;
    dispatching = true;
    const response = await api("/api/workspace/open-in",
      { threadId: context.threadId, target: chosen.target, ...(context.path ? { path: context.path } : {}),
        ...(releaseHarness ? { releaseHarness: true } : {}) },
      requestId, controller.signal);
    const receipt = validateRemoteReceipt(response, requestId);
    if (receipt.state === "failed") {
      sessionStorage.removeItem(operationKey);
      throw Object.assign(new Error(receipt.result?.message || "无法打开工作区"),
        { code: receipt.result?.code || 422 });
    }
    if (receipt.state !== "completed") throw new Error("无法确认应用启动结果");
    const opened = receipt.result;
    if (opened.target !== chosen.target || typeof opened.root !== "string"
        || context.path && opened.path !== context.path)
      throw new Error("无法确认应用启动结果");
    sessionStorage.removeItem(operationKey);
    if (!owns(context)) return null;
    // 正文选择某个文件的打开程序，不改变顶部按工作目录记忆的首选应用。
    if (!context.path) {
      try { localStorage.setItem(preferenceKey(context, opened.root), chosen.target); }
      catch { notice("应用启动请求已提交，但首选打开方式未能保存。", true); }
    }
    // 启动成功但未确认前台时保留成功回执，只提示在任务栏查看，不能再次启动。
    if (typeof opened.warning === "string" && opened.warning.trim()) notice(opened.warning);
    if (chosen.target === "wsl") recheckOwnershipAfterTerminalOpen(context.threadId);
    return opened;
  } catch (error) {
    if (!owns(context)) return null;
    const unknown = dispatching && (error.outcomeUnknown === true
      || !error.notSubmitted && !Number.isInteger(error.code));
    if (dispatching && !unknown && (error.notSubmitted || Number.isInteger(error.code)))
      try { sessionStorage.removeItem(operationKey); } catch {}
    notice(unknown ? "未能确认应用启动结果，请先查看远程电脑，勿重复点击。"
      : error.message || "无法打开工作区", true);
    return null;
  } finally {
    clearTimeout(timer); workspaceOpenRequests.delete(key);
  }
}

export function mountWorkspaceOpenIn({ anchor, beforeOpen = () => {} }) {
  const group = element("div", "workspace-open-in");
  group.hidden = true; group.setAttribute("role", "group"); group.setAttribute("aria-label", "打开工作区");
  const primary = button("", "用首选应用打开工作区", () => void activate(), "workspace-open-in-primary");
  const arrow = button("", "选择打开方式", () => void showMenu(), "workspace-open-in-arrow");
  primary.type = arrow.type = "button";
  arrow.setAttribute("aria-haspopup", "menu"); arrow.setAttribute("aria-expanded", "false");
  arrow.append(icon("chevron"));
  arrow.popupClassName = "workspace-open-in-menu"; arrow.popupWidth = 178;
  // 用户要求 Open in 固定向左；首次打开、列表刷新和全局重定位均使用自身定位，保留 178px 宽度及 6px 垂直间隔。
  arrow.popupFit = fitMenu;
  group.append(primary, arrow); anchor.before(group);

  let contextKey = null, loaded = null, pending = null, opening = false, disposed = false;
  const current = () => !disposed && group.isConnected ? clientOpenInContext() : null;
  const owns = context => !!context && current()?.key === context.key;
  const ownsPopup = () => activePopup?.trigger === arrow;
  function setPrimary(target) {
    primary.replaceChildren(applicationIcon(target));
    const title = target ? `用 ${target.label} 打开工作区` : "用首选应用打开工作区";
    primary.title = title; primary.setAttribute("aria-label", title);
  }
  function syncDisabled() {
    const valid = !!current();
    primary.disabled = !valid || opening || loaded?.targets.length === 0;
    arrow.disabled = !valid || opening;
    primary.setAttribute("aria-busy", String(opening));
  }
  function close() {
    if (ownsPopup()) closePopup();
  }
  function cancelRead() {
    pending?.controller.abort();
    pending = null;
  }
  async function readTargets(context, force = false) {
    if (!force && loaded && loaded.key === context.key && Date.now() - loaded.at < 5000) return loaded;
    if (pending?.key === context.key) return pending.promise;
    const request = { key: context.key, controller: new AbortController(), promise: null };
    pending = request;
    request.promise = api("/api/workspace/open-in-targets", { threadId: context.threadId },
      undefined, request.controller.signal).then(value => {
      const result = normalizeTargets(value, context);
      if (owns(context) && pending === request) {
        loaded = { ...result, key: context.key, at: Date.now() };
        setPrimary(preferredTarget(context, result)); syncDisabled();
      }
      return result;
    }).finally(() => { if (pending === request) pending = null; });
    return request.promise;
  }
  function fitMenu() {
    if (!ownsPopup()) return;
    const box = group.getBoundingClientRect(), viewport = window.visualViewport;
    const left = viewport?.offsetLeft || 0, top = viewport?.offsetTop || 0;
    const width = viewport?.width || window.innerWidth, height = viewport?.height || window.innerHeight;
    if (group.hidden || !current() || box.bottom <= top || box.top >= top + height) { close(); return; }
    const popup = activePopup.node, below = Math.max(0, top + height - box.bottom - 14);
    const above = Math.max(0, box.top - top - 14);
    popup.style.width = `${Math.min(178, width - 16)}px`;
    popup.style.maxHeight = `${Math.min(360, Math.max(below, above))}px`;
    const menuHeight = popup.getBoundingClientRect().height;
    const upward = menuHeight > below && above > below;
    // 用户要求远程 Open in 固定向左展开，右边缘对齐整个按钮组；两侧保留 8px 留白，空间不足时只平移、不反向展开。
    const menuWidth = popup.getBoundingClientRect().width;
    popup.style.left = `${Math.max(left + 8, Math.min(box.right - menuWidth, left + width - menuWidth - 8))}px`;
    popup.style.top = `${upward ? Math.max(top + 8, box.top - menuHeight - 6) : box.bottom + 6}px`;
  }
  function decorateMenu() {
    if (!ownsPopup()) return;
    for (const option of activePopup.list.querySelectorAll("button[data-value]")) {
      if (!applicationIds.has(option.dataset.value)) continue;
      const target = loaded?.targets.find(value => value.target === option.dataset.value);
      option.prepend(applicationIcon(target || option.dataset.value));
    }
    fitMenu();
  }
  function menuOptions(context, result) {
    return result.targets.length
      ? result.targets.map(target => ({ value: target.target, label: target.label,
        action: () => activate(target, context) }))
      : [{ label: "未检测到可用应用", disabled: true }];
  }
  async function showMenu() {
    if (ownsPopup()) { closePopup(true); return; }
    const context = current();
    if (!context || opening) return;
    beforeOpen();
    // 用户要求去掉弹出菜单顶部的 Open in 描述：只展示应用列表，保留按钮的无障碍名称。
    arrow.menuOptions = loaded?.key === context.key ? menuOptions(context, loaded)
      : [{ label: "正在加载可用应用…", disabled: true }];
    openPopup(arrow);
    decorateMenu();
    try {
      const result = await readTargets(context, true);
      if (!owns(context) || !ownsPopup()) return;
      arrow.menuOptions = menuOptions(context, result);
      renderPopup(); decorateMenu();
      const first = activePopup.list.querySelector("button:not(:disabled)");
      if (document.activeElement === arrow || !activePopup.node.contains(document.activeElement)) first?.focus();
    } catch (error) {
      if (!owns(context) || !ownsPopup() || error.name === "AbortError") return;
      arrow.menuOptions = [{ label: "重新加载应用列表", action: () => showMenu() }];
      renderPopup(); fitMenu(); notice(error.message || "无法读取工作区应用", true);
    }
  }
  async function activate(target = null, expected = null) {
    const context = expected || current();
    if (opening || !owns(context)) return;
    opening = true; close(); syncDisabled();
    try {
      // 首选按钮同样先刷新真实可用应用；异步准备后再次核对会话，旧点击不能打开切换后的会话目录。
      const result = await readTargets(context, true);
      if (!owns(context)) return;
      const chosen = target ? result.targets.find(value => value.target === target.target) : preferredTarget(context, result);
      if (!chosen) throw new Error("未找到可用的打开方式，请重新选择应用。");
      const opened = await performWorkspaceOpenIn(context, chosen, owns);
      if (opened && owns(context)) setPrimary(chosen);
    } catch (error) {
      if (owns(context) && error.name !== "AbortError") notice(error.message || "无法打开工作区", true);
    } finally {
      opening = false;
      if (!disposed) syncDisabled();
    }
  }
  function updateContext() {
    const context = current();
    if ((context?.key || null) === contextKey) { syncDisabled(); return; }
    contextKey = context?.key || null;
    close(); cancelRead(); loaded = null; setPrimary(null);
    group.hidden = !context; syncDisabled();
    if (context) void readTargets(context).catch(error => {
      if (owns(context) && error.name !== "AbortError") primary.title = error.message || "无法读取工作区应用";
    });
  }
  const resize = typeof ResizeObserver === "function" ? new ResizeObserver(fitMenu) : null;
  resize?.observe(anchor.parentElement);
  const visibility = () => { if (document.hidden) { close(); cancelRead(); } };
  window.addEventListener("resize", fitMenu); window.addEventListener("blur", close);
  window.visualViewport?.addEventListener("resize", fitMenu);
  document.addEventListener("visibilitychange", visibility);
  setPrimary(null); updateContext();
  return { updateContext, close, dispose() {
    if (disposed) return;
    close(); cancelRead(); disposed = true; resize?.disconnect(); group.remove();
    window.removeEventListener("resize", fitMenu); window.removeEventListener("blur", close);
    window.visualViewport?.removeEventListener("resize", fitMenu);
    document.removeEventListener("visibilitychange", visibility);
  } };
}

function sidebarOpenInContext(item) {
  // 与顶部入口一致：被其他应用占用的会话，右键菜单里的“打开方式”同样保留。
  if (pageLeaving || !item?.id || !workspaceOpenInAvailable()
      || threadReadOnlyApartFromOwnership(item.id)) return null;
  const scope = privateInputScope(session);
  if (!scope) return null;
  const wslInstance = typeof item.instanceId === "string" && item.instanceId.includes(":wsl:")
    && typeof item.wslDistribution === "string" && !!item.wslDistribution;
  return { scope, threadId: item.id, wslInstance,
    key: JSON.stringify([scope, item.id, nativeConnectionVersion, item.cwd || "",
      item.instanceId || "", item.wslDistribution || "", item.wslUser || ""]) };
}

export function threadOpenInMenu(item, { isAvailable = () => true } = {}) {
  if (!workspaceOpenInAvailable()) return null;
  const context = sidebarOpenInContext(item);
  // 本机“打开方式”跟随被右键的会话；不切换聊天或借用当前选中会话。
  const current = () => isAvailable() ? sidebarOpenInContext([
    ...(sidebar?.recent || []), ...(sidebar?.pinnedThreads || []),
    ...(sidebar?.projects || []).flatMap(project => project.threads || [])
  ].find(value => value.id === item.id)) : null;
  return createWorkspaceOpenInMenu(context, current);
}

export function fileOpenInMenu(path, { getItem } = {}) {
  if (!workspaceOpenInAvailable()) return null;
  // 文件路径只附着于原会话身份；实际相对目录、Windows/WSL 映射及文件权限均由独立服务验证。
  const current = () => {
    const context = sidebarOpenInContext(getItem?.());
    return context && typeof path === "string" && path.trim()
      ? { ...context, path, key: JSON.stringify([context.key, path]) } : null;
  };
  return createWorkspaceOpenInMenu(current(), current);
}

function createWorkspaceOpenInMenu(context, current) {
  const owns = expected => !!expected && current()?.key === expected.key;
  const read = async signal => {
    if (!owns(context)) throw new Error("会话已变化，请重新打开菜单。");
    const value = await api("/api/workspace/open-in-targets",
      { threadId: context.threadId, ...(context.path ? { path: context.path } : {}) }, undefined, signal);
    if (!owns(context)) throw new Error("会话已变化，请重新打开菜单。");
    return normalizeTargets(value, context);
  };
  const activate = async target => {
    try {
      // 应用列表和启动间再次核对真实可用目标；异步准备期间更换授权或删除会话时不再派发。
      const result = await read();
      if (!owns(context)) return;
      const chosen = result.targets.find(value => value.target === target.target);
      if (!chosen) throw new Error("该应用已不可用，请重新选择打开方式。");
      await performWorkspaceOpenIn(context, chosen, owns);
    } catch (error) {
      if (owns(context) && error.name !== "AbortError") notice(error.message || "无法打开工作区", true);
    }
  };
  return { disabled: !context, submenuKey: context?.key, submenuWidth: 178, openTarget: activate,
    submenuClassName: "workspace-open-in-menu",
    submenu: async ({ signal }) => {
      const result = await read(signal);
      return result.targets.length ? result.targets.map(target => ({
        value: target.target, label: target.label, renderIcon: () => applicationIcon(target),
        action: () => activate(target)
      })) : [{ label: "未检测到可用应用", disabled: true }];
    }
  };
}
