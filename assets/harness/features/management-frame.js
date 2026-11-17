// 管理页面由同一远程服务提供；各页面只创建一次，返回会话时保留聊天工作区。
import { element } from "../ui/dom.js";
import { requireAuthentication } from "../core/actions.js";
import { session } from "../core/state.js";
import { deleteSidebarThread } from "./sidebar/thread-deletion.js";
import { canManageHarness } from "./main-navigation.js";

export function createManagementPages() {
  const deviceId = session?.device.id;
  let frame = null, current = "proxy", visible = true, loading = null;
  const requests = new AbortController();
  const send = value => {
    if (frame?.isConnected && session?.device.id === deviceId)
      frame.contentWindow?.postMessage(value, location.origin);
  };
  const syncTheme = () => {
    const style = getComputedStyle(document.documentElement);
    send({ type: "management-theme", theme: style.colorScheme === "dark" ? "dark" : "light",
      // 用户要求管理页沿用同一套外观语义：主按钮、强调色与焦点分开传递，不能再把所有主操作染成原始 HEX。
      colors: Object.fromEntries(["bg", "text", "text-secondary", "side", "hover", "line", "muted", "accent",
        "accent-text", "accent-solid", "accent-soft", "on-accent", "focus-ring", "primary-bg", "primary-text",
        "composer-bg", "control-bg", "code", "danger", "on-danger", "selection-bg", "selection-text"]
        .map(name => [name, style.getPropertyValue(`--${name}`).trim()])),
      font: style.getPropertyValue("--ui-font-family").trim(), fontSize: parseFloat(style.fontSize),
      codeFont: style.getPropertyValue("--code-font-family").trim() });
  };
  const receive = async event => {
    if (event.origin !== location.origin || event.source !== frame?.contentWindow
      || !frame?.isConnected || session?.device.id !== deviceId || !canManageHarness()) return;
    const value = event.data;
    if (value?.type === "management-ready") {
      loading?.remove(); loading = null;
      send({ type: "management-page", page: current });
      send({ type: "management-visibility", visible }); syncTheme();
    } else if (value?.type === "management-unauthorized") requireAuthentication();
    else if (value?.type === "management-delete"
      && typeof value.requestId === "string" && /^management-delete-[0-9]+-[0-9]+$/.test(value.requestId)
      && typeof value.id === "string" && /^(?:local:)?[0-9a-f-]{36}$/i.test(value.id)) {
      try {
        const result = await deleteSidebarThread({ id: value.id.replace(/^local:/, ""),
          title: typeof value.title === "string" ? value.title.slice(0, 500) : "" },
        { confirmed: true, propagate: true });
        if (!result?.deleted) throw new Error("会话当前不可删除，请核对执行状态及原删除回执。");
        send({ type: "management-delete-result", requestId: value.requestId, ok: true });
      } catch (error) {
        send({ type: "management-delete-result", requestId: value.requestId, ok: false, message: error.message });
      }
    }
  };
  window.addEventListener("message", receive);
  const configurationChanged = () => send({ type: "management-config-changed" });
  window.addEventListener("codex-elves-management-updated", configurationChanged);
  const themeObserver = new MutationObserver(syncTheme);
  themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ["class", "style", "data-theme"] });
  const open = (page, host) => {
    current = page;
    if (!canManageHarness()) {
      host.replaceChildren(element("p", "management-unavailable",
        "管理功能仅可在 Elves 独立客户端使用。"));
      return;
    }
    if (!frame) {
      frame = element("iframe", "management-frame");
      frame.title = "Elves 管理工作区";
      // 旧 Windows/Android 容器会为所有缓存文档附加禁止嵌入的响应头。
      // 仅取固定同源管理页作为 srcdoc，继承原页面的 Origin、CSP 和授权，不扩大任何原生命令权限。
      loading = element("p", "management-unavailable", "正在加载管理界面…");
      host.append(loading);
      void fetch("/management/index.html", { credentials: "same-origin", signal: requests.signal })
        .then(async response => {
          if (!response.ok) throw new Error(`管理界面加载失败（HTTP ${response.status}）`);
          const html = await response.text();
          if (!frame?.isConnected || session?.device.id !== deviceId) return;
          const parsed = new DOMParser().parseFromString(html, "text/html");
          parsed.documentElement.dataset.managementPage = current;
          frame.srcdoc = `<!doctype html>${parsed.documentElement.outerHTML}`;
        }).catch(error => {
          if (!host.isConnected || session?.device.id !== deviceId || requests.signal.aborted) return;
          if (loading) loading.textContent = error.message;
        });
      // 与会话同源并沿用设备 Cookie，不把访问密钥、供应商凭据或原生命令放入 URL。
      frame.referrerPolicy = "same-origin";
      host.append(frame);
    } else send({ type: "management-page", page });
  };
  open.setVisible = value => { visible = value; send({ type: "management-visibility", visible }); };
  open.dispose = () => {
    requests.abort();
    window.removeEventListener("message", receive); themeObserver.disconnect();
    window.removeEventListener("codex-elves-management-updated", configurationChanged);
    frame?.remove(); frame = null;
  };
  return open;
}
