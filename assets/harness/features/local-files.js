// 在系统默认浏览器中打开会话工作区内的本机文件。

import { isAndroidContainer, openAndroidBrowser } from "../platform.js";
import { localFileReference } from "../file-reference.js";
import { notice } from "../ui/notice.js";
import { copyText } from "../ui/clipboard.js";
import { creating, pageLeaving, selectedId, session, thread } from "../core/state.js";
import { api } from "./api.js";
import { privateInputScope } from "./session/capabilities.js";
import { applicationIcon, fileOpenInMenu } from "./session/open-in.js";
import { relatedSessionView } from "./session/related-state.js";

function currentFileBrowserThreadId() {
  const parentThreadId = relatedSessionView?.kind === "side"
    ? relatedSessionView.openedFromThreadId || relatedSessionView.parentThreadId : null;
  return !creating ? parentThreadId || selectedId : null;
}
function localFileBrowserUrl(path) {
  const threadId = currentFileBrowserThreadId();
  if (!threadId || typeof path !== "string" || !path.trim()) {
    throw new Error("当前会话没有可用的工作区文件");
  }
  const normalized = path.trim().replaceAll("\\", "/");
  // 正文菜单支持 ./README.md；去掉当前目录段后继续保留对上级目录越界的拒绝。
  const segments = normalized.split("/").filter(segment => segment && segment !== ".");
  if (!segments.length || segments.some(segment => segment === "..")) {
    throw new Error("文件路径无效");
  }
  const url = new URL(location.href);
  // Linux/WSL 绝对路径的前导斜杠也须保留在路由参数中；否则 /home/... 会被当成项目内相对路径。
  const absolutePrefix = normalized.startsWith("//") ? "%2F%2F" : normalized.startsWith("/")
    && !/^\/[a-z]:\//i.test(normalized) ? "%2F" : "";
  url.pathname = `/api/workspace/view/${encodeURIComponent(threadId)}/${
    absolutePrefix + segments.map(segment => encodeURIComponent(segment)).join("/")
  }`;
  url.search = "";
  url.hash = "";
  return url.href;
}
export async function openLocalFileInBrowser(path) {
  try {
    // 用户要求仅 Android 使用内置浏览器，其他端通过已授权服务调用系统默认浏览器打开原文件；
    // 不能因窗口变窄或来自侧边会话而改用网页预览、新窗口或文件的默认编辑器。
    if (!isAndroidContainer()) {
      const threadId = currentFileBrowserThreadId();
      if (!threadId || typeof path !== "string" || !path.trim())
        throw new Error("当前会话没有可用的工作区文件");
      await api("/api/workspace/open-browser", { threadId, path });
      return;
    }
    return openAndroidBrowser(localFileBrowserUrl(path))
      .catch(error => notice(error.message || "无法打开浏览器", true));
  } catch (error) {
    notice(error.message || "无法打开本地文件", true);
  }
}
// Markdown 中的本机文件链接通过页面事件交给系统默认浏览器打开。
export function installLocalFileOpenListener() {
  window.addEventListener("codex-elves-open-local-file", event => {
    openLocalFileInBrowser(event.detail?.path);
  });
}

function localFileMenuOwner() {
  const scope = privateInputScope(session), threadId = currentFileBrowserThreadId();
  if (!scope || !threadId || pageLeaving || thread?.id !== selectedId) return null;
  const item = { ...thread, id: threadId };
  return { item, key: JSON.stringify([scope, selectedId, threadId, thread.cwd || "",
    thread.instanceId || "", thread.wslDistribution || "", thread.wslUser || ""]) };
}

export function localFileReferenceActions({ openFile } = {}) {
  // 用户要求缓存正文恢复后文件菜单正常可用：渲染时只固定授权和所属会话，完整工作区身份在打开菜单或点击时读取。
  // 不能永久捕获尚未返回的 thread，也不能让旧正文借用切换后的会话；每次打开的菜单仍固定自己的完整身份。
  const scope = privateInputScope(session), sourceId = !creating ? selectedId : null;
  const currentOwner = () => scope && sourceId && !creating && selectedId === sourceId
    && privateInputScope(session) === scope ? localFileMenuOwner() : null;
  const owns = owner => !!owner && currentOwner()?.key === owner.key;
  const preview = (path, line, owner = currentOwner()) => {
    if (!owns(owner)) return;
    const reference = localFileReference(path, true);
    if (!reference) return;
    path = reference.path;
    // 文件页原本按 Git 根目录处理相对路径；正文引用必须相对会话 cwd，先附着明确目录，避免子目录会话打开同名根文件。
    const cwd = owner.item.cwd?.replaceAll("\\", "/").replace(/\/+$/, "");
    const target = !/^(?:\/|[a-z]:\/)/i.test(path) && cwd
      ? `${cwd}/${path.split("/").filter(part => part !== ".").join("/")}` : path;
    return openFile?.(target, line ?? reference.line);
  };
  return { openFile: preview, getMenuOptions(path, line) {
    const owner = currentOwner();
    const opening = fileOpenInMenu(path, { getItem: () => owns(owner) ? owner.item : null });
    // 用户要求“浏览器打开”固定为第二项，复用 Android 内置/其他端系统默认浏览器；
    // Explorer 定位所点文件，复制不附加行号，旧会话/授权的操作不得借用当前身份。
    return [
      { value: "openFile", label: "打开文件", icon: "folder", disabled: !owns(owner) || typeof openFile !== "function",
        action: () => preview(path, line, owner) },
      { value: "openBrowser", label: "浏览器打开", icon: "globe", disabled: !owns(owner),
        action: () => { if (owns(owner)) void openLocalFileInBrowser(path); } },
      // 用户要求所有 File Explorer 直达入口与 Open in 共用彩色应用图标，不再使用灰色线框文件夹。
      { value: "revealFile", label: "在 File Explorer 打开", renderIcon: () => applicationIcon("fileManager"), disabled: opening.disabled,
        action: () => opening.openTarget({ target: "fileManager" }) },
      { ...opening, value: "openIn", label: "打开方式", icon: "window" },
      { value: "copyPath", label: "复制路径", icon: "copy", separatorBefore: true, disabled: !owns(owner),
        action: () => { if (owns(owner)) void copyText(path); } }
    ];
  } };
}
