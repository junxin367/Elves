// 用户要求主入口固定排序，“定时任务”紧跟会话之后；设置固定底部。
// 切页保留当前会话、草稿、侧边标签及阅读位置。
import { button, element } from "../ui/dom.js";
import { icon } from "../ui/icons.js";
import { closePopup, openPopup, togglePopup } from "../ui/popup.js";
import { session } from "../core/state.js";
import { isAndroidContainer } from "../platform.js";

export const mainNavigationItems = Object.freeze([
  { id: "conversation", label: "会话", icon: "chat" },
  { id: "scheduled", label: "定时任务", icon: "recent" },
  { id: "proxy", label: "代理设置", icon: "network" },
  { id: "sessions", label: "会话管理", icon: "recent" },
  { id: "remoteAccess", label: "远程会话", icon: "globe" }
]);

export function isElvesClient() {
  return typeof globalThis.__ELVES_CLIENT__?.open === "function";
}

export function canManageHarness() {
  // 网页、PWA、Android 只显示会话；管理页要求原生容器和服务端确认的本机传输及管理能力同时成立。
  return isElvesClient() && session?.transport === "desktop" && session?.capabilities?.management === true;
}

let disposeNavigation = null;
let chooseNavigation = null;
export function showConversationPage() { chooseNavigation?.("conversation"); }
export function disposeMainNavigation() {
  disposeNavigation?.();
  disposeNavigation = null;
  chooseNavigation = null;
}

export function mountMainNavigation({ workspace, openSettings, openScheduledTasks, logout, openPage }) {
  disposeMainNavigation();
  const shell = element("div", "remote-shell");
  const navigation = element("nav", "main-navigation");
  navigation.setAttribute("aria-label", "主导航");
  const pages = element("div", "main-navigation-pages");
  const settingsArea = element("div", "main-navigation-settings");
  const sidebar = workspace.querySelector("#session-sidebar");
  const footer = element("div", "sidebar-footer mobile-sidebar-footer");
  const footerActions = element("div", "footer-actions");
  let mobileNavigation = false;
  const management = element("main", "management-workspace");
  management.id = "management-workspace";
  management.hidden = true;
  const scheduledPage = element("main", "scheduled-workspace");
  scheduledPage.id = "scheduled-workspace";
  scheduledPage.hidden = true;
  const settingsPage = element("main", "appearance-screen");
  settingsPage.id = "settings-workspace";
  settingsPage.hidden = true;
  let current = "conversation", savedPosition = null;
  let settingsFrom = "conversation", disposeSettings = null, disposeScheduled = null;
  const items = mainNavigationItems.filter(item => item.id === "conversation" || canManageHarness());
  const controls = new Map();
  const canChoose = id => id === "conversation" || id === "settings"
    || canManageHarness() && items.some(item => item.id === id);
  const choose = id => {
    if (current === id || !canChoose(id)) return;
    closePopup();
    if (current === "settings") {
      // 数值框失焦时保存有效值；主动切页时先失焦，再卸载设置，不能吞掉最后一次编辑。
      if (settingsPage.contains(document.activeElement)) document.activeElement.blur();
      disposeSettings?.();
      disposeSettings = null;
    }
    if (current === "scheduled") {
      disposeScheduled?.();
      disposeScheduled = null;
    }
    if (current === "conversation") {
      const messages = workspace.querySelector("#messages");
      savedPosition = messages ? { node: messages, top: messages.scrollTop } : null;
    }
    if (id === "settings") settingsFrom = current;
    current = id;
    for (const [page, control] of controls) {
      control.classList.toggle("selected", page === current);
      if (page === current) control.setAttribute("aria-current", "page");
      else control.removeAttribute("aria-current");
    }
    // 隐藏而不销毁会话 DOM，实时消息和会话操作继续使用原来的实例与授权。
    workspace.hidden = current !== "conversation";
    workspace.inert = workspace.hidden;
    // 设置、定时任务与会话管理占同一内容列并共享选中状态；只隐藏页面，不用弹窗锁住主导航。
    settingsPage.hidden = current !== "settings";
    scheduledPage.hidden = current !== "scheduled";
    management.hidden = current === "conversation" || current === "settings" || current === "scheduled";
    openPage.setVisible?.(!management.hidden);
    if (current === "conversation") {
      if (savedPosition?.node.isConnected) savedPosition.node.scrollTop = savedPosition.top;
      requestAnimationFrame(() => {
        if (current === "conversation" && savedPosition?.node.isConnected)
          savedPosition.node.scrollTop = savedPosition.top;
      });
    } else if (current === "settings") {
      disposeSettings = openSettings(settingsPage, {
        onBack: () => {
          const destination = canChoose(settingsFrom) ? settingsFrom : "conversation";
          choose(destination);
          (mobileNavigation ? controls.get("settings") : controls.get(destination))?.focus({ preventScroll: true });
        }
      });
    } else if (current === "scheduled") {
      scheduledPage.setAttribute("aria-label", "定时任务");
      disposeScheduled = openScheduledTasks?.(scheduledPage) || null;
    } else {
      management.setAttribute("aria-label", items.find(page => page.id === current).label);
      openPage(current, management);
    }
  };
  for (const item of items) {
    const control = button("", item.label, () => choose(item.id), "main-navigation-button");
    control.dataset.page = item.id;
    control.setAttribute("aria-controls", item.id === "conversation" ? "session-sidebar"
      : item.id === "scheduled" ? scheduledPage.id : management.id);
    control.append(icon(item.icon));
    if (item.id === current) {
      control.classList.add("selected");
      control.setAttribute("aria-current", "page");
    }
    controls.set(item.id, control);
    pages.append(control);
  }
  const settings = button("", "设置", () => {
    // 用户要求手机恢复旧版底部两个独立按钮；PC 网页保留菜单，Windows 仍直接打开设置。
    if (isElvesClient() || mobileNavigation) choose("settings");
    else togglePopup(settings);
  }, "main-navigation-button");
  settings.id = "appearance-button";
  settings.dataset.page = "settings";
  controls.set("settings", settings);
  settings.append(icon("settings"));
  if (!isElvesClient()) {
    settings.setAttribute("aria-haspopup", "menu");
    settings.setAttribute("aria-expanded", "false");
    settings.popupDirection = "up";
    settings.popupWidth = 160;
    settings.menuOptions = [
      { label: "设置", icon: "settings", action: () => choose("settings") },
      { label: "退出", icon: "logout", action: logout }
    ];
    settings.onkeydown = event => {
      if (mobileNavigation || !["ArrowDown", "ArrowUp"].includes(event.key)) return;
      event.preventDefault();
      openPopup(settings, { edge: event.key === "ArrowUp" ? "last" : "first" });
    };
  }
  const logoutButton = button("", "退出", logout, "footer-button");
  logoutButton.id = "logout";
  logoutButton.append(icon("logout"), element("span", "", "退出"));
  footerActions.append(logoutButton);
  footer.append(footerActions);
  footer.hidden = true;
  if (!isElvesClient()) sidebar?.append(footer);
  // 用户要求手机网页与 Android 无主导航：760px 沿用会话抽屉断点；粗指针无悬停及 Android 容器兼顾横屏。
  // 只移动同一个设置按钮，保留 appearance-button 的退出锁定与设置入口，不创建重复 ID。
  const mobileMedia = matchMedia("(max-width: 760px), (hover: none) and (pointer: coarse)");
  const syncMobileNavigation = () => {
    const mobile = !isElvesClient() && !!sidebar && (mobileMedia.matches || isAndroidContainer());
    if (mobile !== mobileNavigation) closePopup();
    mobileNavigation = mobile;
    shell.dataset.mobileNavigation = String(mobile);
    settingsPage.dataset.settingsMobile = String(mobile);
    footer.hidden = !mobile;
    settings.className = mobile ? "footer-button" : "main-navigation-button";
    settings.classList.toggle("selected", current === "settings");
    settings.replaceChildren(icon("settings"), ...(mobile ? [element("span", "", "设置")] : []));
    if (mobile || isElvesClient()) {
      settings.removeAttribute("aria-haspopup");
      settings.removeAttribute("aria-expanded");
      settings.setAttribute("aria-controls", settingsPage.id);
    } else {
      settings.removeAttribute("aria-controls");
      settings.setAttribute("aria-haspopup", "menu");
      settings.setAttribute("aria-expanded", "false");
    }
    if (mobile) footerActions.prepend(settings);
    else settingsArea.append(settings);
  };
  mobileMedia.addEventListener("change", syncMobileNavigation);
  syncMobileNavigation();
  chooseNavigation = choose;
  navigation.append(pages, settingsArea);
  workspace.before(shell);
  shell.append(navigation, workspace, scheduledPage, management, settingsPage);
  disposeNavigation = () => {
    mobileMedia.removeEventListener("change", syncMobileNavigation);
    footer.remove();
    disposeSettings?.();
    disposeScheduled?.();
    openPage.dispose?.();
  };
  return { choose, shell, management, settingsPage };
}
