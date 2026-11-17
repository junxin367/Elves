// 用户要求四个主入口固定排序，切换管理页面时保留当前会话、草稿、侧边标签及阅读位置。
import { button, element } from "../ui/dom.js";
import { icon } from "../ui/icons.js";
import { closePopup, openPopup, togglePopup } from "../ui/popup.js";

export const mainNavigationItems = Object.freeze([
  { id: "conversation", label: "会话", icon: "chat" },
  { id: "proxy", label: "代理设置", icon: "network" },
  { id: "sessions", label: "会话管理", icon: "recent" },
  { id: "remoteAccess", label: "远程会话", icon: "globe" }
]);

export function isElvesClient() {
  return typeof globalThis.__ELVES_CLIENT__?.open === "function";
}

let disposeNavigation = null;
let chooseNavigation = null;
export function showConversationPage() { chooseNavigation?.("conversation"); }
export function disposeMainNavigation() {
  disposeNavigation?.();
  disposeNavigation = null;
  chooseNavigation = null;
}

export function mountMainNavigation({ workspace, openSettings, logout, openPage }) {
  disposeMainNavigation();
  const shell = element("div", "remote-shell");
  const navigation = element("nav", "main-navigation");
  navigation.setAttribute("aria-label", "主导航");
  const pages = element("div", "main-navigation-pages");
  const settingsArea = element("div", "main-navigation-settings");
  const management = element("main", "management-workspace");
  management.id = "management-workspace";
  management.hidden = true;
  let current = "conversation", savedPosition = null;
  const controls = new Map();
  const choose = id => {
    if (current === id) return;
    closePopup();
    if (current === "conversation") {
      const messages = workspace.querySelector("#messages");
      savedPosition = messages ? { node: messages, top: messages.scrollTop } : null;
    }
    current = id;
    for (const [page, control] of controls) {
      control.classList.toggle("selected", page === current);
      if (page === current) control.setAttribute("aria-current", "page");
      else control.removeAttribute("aria-current");
    }
    // 隐藏而不销毁会话 DOM，实时消息和会话操作继续使用原来的实例与授权。
    workspace.hidden = current !== "conversation";
    workspace.inert = workspace.hidden;
    management.hidden = current === "conversation";
    openPage.setVisible?.(!management.hidden);
    if (current === "conversation") {
      if (savedPosition?.node.isConnected) savedPosition.node.scrollTop = savedPosition.top;
      requestAnimationFrame(() => {
        if (current === "conversation" && savedPosition?.node.isConnected)
          savedPosition.node.scrollTop = savedPosition.top;
      });
    } else {
      management.setAttribute("aria-label", mainNavigationItems.find(page => page.id === current).label);
      openPage(current, management);
    }
  };
  for (const item of mainNavigationItems) {
    const control = button("", item.label, () => choose(item.id), "main-navigation-button");
    control.dataset.page = item.id;
    control.setAttribute("aria-controls", item.id === "conversation" ? "session-sidebar" : management.id);
    control.append(icon(item.icon));
    if (item.id === current) {
      control.classList.add("selected");
      control.setAttribute("aria-current", "page");
    }
    controls.set(item.id, control);
    pages.append(control);
  }
  const settings = button("", "设置", () => {
    // Windows 独立客户端直接进入设置；网页、PWA 与 Android 只提供设置和原有退出操作。
    if (isElvesClient()) openSettings();
    else togglePopup(settings);
  }, "main-navigation-button");
  settings.id = "appearance-button";
  settings.append(icon("settings"));
  if (!isElvesClient()) {
    settings.setAttribute("aria-haspopup", "menu");
    settings.setAttribute("aria-expanded", "false");
    settings.popupDirection = "up";
    settings.popupWidth = 160;
    settings.menuOptions = [
      { label: "设置", icon: "settings", action: openSettings },
      { label: "退出", icon: "logout", action: logout }
    ];
    settings.onkeydown = event => {
      if (!["ArrowDown", "ArrowUp"].includes(event.key)) return;
      event.preventDefault();
      openPopup(settings, { edge: event.key === "ArrowUp" ? "last" : "first" });
    };
  }
  chooseNavigation = choose;
  settingsArea.append(settings);
  navigation.append(pages, settingsArea);
  workspace.before(shell);
  shell.append(navigation, workspace, management);
  disposeNavigation = () => openPage.dispose?.();
  return { choose, shell, management };
}
