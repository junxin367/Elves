// 远程会话页面：用户要求远程会话配置保留在 Elves 本机 Harness 中，不放进独立代理管理器。
// 该页面只在 Elves 本机客户端主导航显示，调用本机独占的 /api/desktop/remote-access/{action} 接口；
// 网页、PWA 与 Android 不显示入口，也不开放管理接口。

import { copyText } from "../ui/clipboard.js";
import { confirmAction } from "../ui/dialog.js";
import { button, element } from "../ui/dom.js";
import { icon } from "../ui/icons.js";
import { notice } from "../ui/notice.js";
import { api } from "./api.js";

const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const FIXED_NAME_PATTERN = /^[a-z0-9](?:[a-z0-9-]{1,61}[a-z0-9])?$/;
const ACCESS_KEY_PATTERN = /^[\x21-\x7e]{8,64}$/;
const SERVICE_WAITING_GUIDANCE = "Codex 会话服务正在准备；公网入口仍可测试和切换，远程网页会在服务连接后就绪。";

const defaultRemoteAccessSettings = Object.freeze({
  enabled: false,
  ngrokEnabled: false,
  bindHost: "127.0.0.1",
  port: 45222,
  zrokSetupValue: "",
  deviceSessionHours: 0,
  readOnly: false,
  allowApprovals: true
});

const defaultFrpConfig = Object.freeze({
  serverAddr: "",
  serverPort: 7000,
  remotePort: 0,
  publicOrigin: "",
  httpsMode: "provider"
});

export function normalizeFrpServerAddress(value) {
  const trimmed = String(value || "").trim();
  try {
    const url = new URL(trimmed.includes("://") ? trimmed : `http://${trimmed}`);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) return trimmed;
    return url.hostname;
  } catch {
    return trimmed;
  }
}

export function frpPublicOrigin(config) {
  try {
    const host = normalizeFrpServerAddress(config?.serverAddr || "");
    const port = Number(config?.remotePort);
    if (!/^[a-z0-9][a-z0-9.-]*$/i.test(host) || !Number.isInteger(port) || port < 1 || port > 65535) return "";
    const url = new URL(String(config?.publicOrigin || "").trim() || `http://${host}:${port}`);
    return ["http:", "https:"].includes(url.protocol) && url.pathname === "/" && !url.search && !url.hash
      && !url.username && !url.password ? url.origin : "";
  } catch {
    return "";
  }
}

export function normalizeFrpConfig(config) {
  return {
    ...defaultFrpConfig,
    ...config,
    serverAddr: normalizeFrpServerAddress(config?.serverAddr || ""),
    serverPort: Number(config?.serverPort) || 0,
    remotePort: Number(config?.remotePort) || 0,
    publicOrigin: frpPublicOrigin(config),
    httpsMode: "provider"
  };
}

export function remoteAccessValidation(settings) {
  if (!Number.isInteger(settings.port) || settings.port < 1 || settings.port > 65535) {
    return "端口必须在 1–65535 之间。";
  }
  if (settings.deviceSessionHours !== 0
    && (!Number.isInteger(settings.deviceSessionHours) || settings.deviceSessionHours < 1 || settings.deviceSessionHours > 720)) {
    return "设备授权有效期必须在 1–720 小时之间。";
  }
  const bindHost = String(settings.bindHost || "").trim();
  const ipv4 = bindHost.split(".");
  let loopback = false;
  if (ipv4.length === 4 && ipv4.every(part => /^(0|[1-9]\d{0,2})$/.test(part) && Number(part) <= 255)) {
    loopback = ipv4[0] === "127";
  } else if (/^[\da-f:.]+$/i.test(bindHost) && bindHost.includes(":")) {
    try {
      const url = new URL(`http://[${bindHost}]/`);
      loopback = url.hostname === "[::1]";
    } catch {
      return "请填写有效的监听 IP 地址。";
    }
  } else {
    return "请填写有效的监听 IP 地址。";
  }
  if (settings.enabled && !loopback) return "远程会话网页仅允许监听本机回环地址。";
  return null;
}

function downloadSize(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function formatError(error) {
  return error instanceof Error ? error.message : String(error || "操作失败");
}

function createSwitchControl(label, onChange) {
  const wrap = element("label", "remote-switch-label");
  const input = element("input", "appearance-notification-switch");
  input.type = "checkbox";
  input.setAttribute("role", "switch");
  input.setAttribute("aria-label", label);
  const text = element("span", "remote-switch-text", "已停用");
  input.addEventListener("change", () => onChange(input.checked));
  wrap.append(input, text);
  return {
    wrap,
    input,
    setState(checked, disabled, activeLabel = "已启用", inactiveLabel = "已停用") {
      input.checked = Boolean(checked);
      input.setAttribute("aria-checked", String(Boolean(checked)));
      input.disabled = Boolean(disabled);
      text.textContent = checked ? activeLabel : inactiveLabel;
    }
  };
}

function createSecretInput({ id, label, minLength, maxLength, onInput, onCopy }) {
  let visible = false;
  const wrap = element("div", "remote-access-key-input");
  const input = element("input", "remote-input");
  input.id = id;
  input.type = "password";
  input.autocomplete = "off";
  input.spellcheck = false;
  if (minLength) input.minLength = minLength;
  if (maxLength) input.maxLength = maxLength;

  const actions = element("div", "remote-access-key-actions");
  const toggleButton = button("", `显示${label}`, () => {
    visible = !visible;
    sync();
  }, "icon-button remote-key-action");
  toggleButton.setAttribute("aria-pressed", "false");

  const copyButton = button("", `复制${label}`, () => void onCopy(input.value), "icon-button remote-key-action");
  copyButton.append(icon("copy"));

  actions.append(toggleButton, copyButton);
  wrap.append(input, actions);

  const sync = () => {
    input.type = visible ? "text" : "password";
    toggleButton.setAttribute("aria-label", `${visible ? "隐藏" : "显示"}${label}`);
    toggleButton.setAttribute("aria-pressed", String(visible));
    toggleButton.replaceChildren(icon(visible ? "eyeOff" : "eye"));
    const disabled = input.disabled || !input.value;
    toggleButton.disabled = disabled;
    copyButton.disabled = disabled;
  };

  input.addEventListener("input", () => {
    onInput(input.value);
    sync();
  });

  sync();

  return {
    wrap,
    input,
    setVisible(next) {
      visible = Boolean(next);
      sync();
    },
    setValue(value) {
      if (input.value !== value) input.value = value;
      sync();
    },
    setDisabled(disabled) {
      input.disabled = Boolean(disabled);
      sync();
    },
    setPlaceholder(placeholder) {
      input.placeholder = placeholder;
    },
    sync
  };
}

function zrokBadgeInfo(zrok, serviceReady) {
  switch (zrok?.state) {
    case "ready":
      return serviceReady
        ? { className: "connected", label: "可访问", title: "" }
        : { className: "connecting", label: "等待服务就绪", title: "公网已连通，等待 Codex 会话服务连接后即可读取会话。" };
    case "checking": return { className: "connecting", label: "正在检查", title: "" };
    case "downloading": return { className: "connecting", label: "正在下载", title: "" };
    case "starting": return { className: "connecting", label: "正在启动", title: "" };
    case "connecting": return { className: "connecting", label: "正在验证", title: "" };
    case "retry_wait": return { className: "connecting", label: "等待重试", title: "" };
    case "error": return { className: "error", label: "异常", title: zrok.error || "连接异常" };
    case "unbound": return { className: "", label: "未绑定", title: "" };
    case "name_required": return { className: "", label: "待设置名称", title: "" };
    case "missing": return { className: "", label: "未安装", title: "" };
    default: return { className: "", label: "已停止", title: "" };
  }
}

function frpStateLabel(state) {
  if (state === "ready") return "可访问";
  if (state === "downloading") return "正在下载";
  if (state === "starting") return "正在启动";
  if (state === "error") return "异常";
  return "已停止";
}

export function mountRemoteAccessPage(root) {
  let disposed = false;
  let refreshing = false;
  let busy = false;
  let pollTimer = null;
  let pollIntervalMs = 5000;
  let abortController = new AbortController();

  let settings = { ...defaultRemoteAccessSettings };
  let settingsDirty = false;
  let status = null;
  let credentials = null;
  let loginLocked = false;
  let statusMessage = "";

  let editingPrimary = "zrok";
  let initializedPrimary = false;

  let zrokValue = "";
  let loadedZrokSetupValue = "";
  let zrokInputDirty = false;
  let fixedNameValue = "";
  let fixedNameDirty = false;

  let frpConfig = { ...defaultFrpConfig };
  let frpInitialized = false;
  let customPublicOrigin = false;
  let frpToken = "";
  let frpTokenDirty = false;
  let loadedFrpToken = undefined;
  let primaryOperation = null; // "test" | "apply" | null
  let primaryFeedback = null; // { text, kind } | null
  let openingDirectory = false;

  let ngrokToken = "";
  let ngrokTokenConfigured = false;
  let ngrokTokenDirty = false;
  let loadedNgrokToken = null;
  let ngrokTokenRevision = 0;

  let keyValue = "";
  let keyDirty = false;
  let loadedKeyValue = null;
  let loginLockRevision = 0;

  const command = async (action, payload = {}) =>
    api(`/api/desktop/remote-access/${encodeURIComponent(action)}`, payload, undefined, abortController.signal);

  root.className = "remote-access-workspace";
  root.setAttribute("aria-label", "远程会话");
  root.replaceChildren();

  // 页头
  const heading = element("header", "remote-access-heading");
  const titleWrap = element("div", "remote-access-title");
  titleWrap.append(element("h1", "", "远程会话"));
  const refreshButton = button("", "刷新远程会话状态", () => void refresh(), "icon-button");
  refreshButton.append(icon("retry"));
  titleWrap.append(refreshButton);
  heading.append(titleWrap);

  // 主卡片（沿用原三段式分区：远程会话网页、公网入口、访问密钥与设备授权）
  const panel = element("section", "remote-access-panel");

  // --- 分区 1：远程会话网页 ---
  const serviceSection = element("div", "remote-access-section");
  const serviceHead = element("div", "remote-panel-head");
  const serviceCopy = element("div", "remote-panel-head-copy");
  serviceCopy.append(
    element("h2", "", "远程会话网页"),
    element("p", "remote-panel-desc", "通过手机浏览器安全访问本机会话；修改监听地址或端口会断开当前连接。")
  );
  const serviceActions = element("div", "remote-panel-head-actions");
  const webSwitch = createSwitchControl("启用远程会话网页", checked => {
    void toggleWebService(checked);
  });
  const saveButton = button("保存并应用", "保存并应用远程会话设置", () => void saveServiceSettings(), "primary remote-action-button");
  serviceActions.append(webSwitch.wrap, saveButton);
  serviceHead.append(serviceCopy, serviceActions);

  const serviceError = element("p", "remote-access-error");
  serviceError.setAttribute("role", "alert");
  serviceError.hidden = true;

  const serviceFields = element("div", "remote-access-fields remote-access-fields-top remote-access-service-fields");

  const bindField = element("div", "remote-field");
  const bindLabel = element("label", "", "监听 IP 地址");
  bindLabel.htmlFor = "remote-bind";
  const bindInput = element("input", "remote-input");
  bindInput.id = "remote-bind";
  bindInput.type = "text";
  bindInput.spellcheck = false;
  bindInput.addEventListener("input", () => {
    settingsDirty = true;
    settings = { ...settings, bindHost: bindInput.value.trim() };
  });
  bindField.append(bindLabel, bindInput, element("small", "", "仅支持本机回环地址；公网访问由所选主链路转发。"));

  const portField = element("div", "remote-field");
  const portLabel = element("label", "", "独立端口");
  portLabel.htmlFor = "remote-port";
  const portInput = element("input", "remote-input");
  portInput.id = "remote-port";
  portInput.type = "number";
  portInput.min = "1";
  portInput.max = "65535";
  portInput.addEventListener("input", () => {
    settingsDirty = true;
    settings = { ...settings, port: Number(portInput.value) };
  });
  portField.append(portLabel, portInput);

  const expiryField = element("div", "remote-field");
  const expiryHeading = element("div", "remote-access-expiry-heading");
  const expiryLabel = element("label", "", "设备授权有效期（小时）");
  expiryLabel.htmlFor = "remote-expiry";
  const expirySwitch = createSwitchControl("限制设备授权有效期", checked => {
    settingsDirty = true;
    settings = { ...settings, deviceSessionHours: checked ? 168 : 0 };
    render();
  });
  expiryHeading.append(expiryLabel, expirySwitch.wrap);
  const expiryInput = element("input", "remote-input");
  expiryInput.id = "remote-expiry";
  expiryInput.addEventListener("input", () => {
    if (settings.deviceSessionHours <= 0) return;
    settingsDirty = true;
    const value = Number(expiryInput.value);
    settings = { ...settings, deviceSessionHours: Number.isNaN(value) ? 1 : Math.max(1, Math.min(720, value)) };
  });
  const expiryHint = element("small", "", "默认关闭，已授权设备长期有效。");
  expiryField.append(expiryHeading, expiryInput, expiryHint);

  serviceFields.append(bindField, portField, expiryField);
  serviceSection.append(serviceHead, serviceError, serviceFields);

  // --- 分区 2：公网入口（zrok / frp + ngrok 第二线路） ---
  const publicSection = element("div", "remote-access-section");
  const publicHead = element("div", "remote-panel-head");
  const publicCopy = element("div", "remote-panel-head-copy");
  const publicTitleRow = element("div", "remote-public-title-row");
  publicTitleRow.append(element("h2", "", "公网入口"));

  // zrok / frp 面包屑切换（遵循产品规范：查看表单不改变运行中主链路）
  const primaryTabs = element("div", "remote-primary-breadcrumbs");
  primaryTabs.setAttribute("role", "tablist");
  primaryTabs.setAttribute("aria-label", "主链路配置");
  const tabButtons = {};
  for (const [index, [provider, badgeText]] of [["zrok", "免费"], ["frp", "⚡️推荐"]].entries()) {
    const crumb = element("span", "remote-primary-crumb");
    if (index > 0) {
      const slash = element("span", "", "/");
      slash.setAttribute("aria-hidden", "true");
      crumb.append(slash);
    }
    const tab = element("button", "remote-primary-tab");
    tab.type = "button";
    tab.id = `remote-primary-${provider}`;
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-controls", `remote-primary-panel-${provider}`);
    tab.append(document.createTextNode(provider), element("span", `remote-primary-label remote-primary-label-${provider}`, badgeText));
    tab.addEventListener("click", () => {
      if (tab.disabled || editingPrimary === provider) return;
      initializedPrimary = true;
      editingPrimary = provider;
      primaryFeedback = null;
      render();
    });
    tab.addEventListener("keydown", event => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
      event.preventDefault();
      const next = event.key === "Home" ? "zrok" : event.key === "End" ? "frp" : provider === "zrok" ? "frp" : "zrok";
      initializedPrimary = true;
      editingPrimary = next;
      primaryFeedback = null;
      render();
      tabButtons[next]?.focus();
    });
    tabButtons[provider] = tab;
    crumb.append(tab);
    primaryTabs.append(crumb);
  }
  publicTitleRow.append(primaryTabs);
  const publicDesc = element("p", "remote-panel-desc");
  publicCopy.append(publicTitleRow, publicDesc);

  const publicStatusWrap = element("div", "remote-panel-head-actions");
  publicHead.append(publicCopy, publicStatusWrap);

  const primaryUnsupportedHint = element("p", "remote-access-hint", "当前运行的远程后台尚不支持主链路切换，请更新并重新启动后台。");
  primaryUnsupportedHint.hidden = true;

  // zrok panel
  const zrokPanel = element("div", "remote-primary-panel");
  zrokPanel.id = "remote-primary-panel-zrok";
  zrokPanel.setAttribute("role", "tabpanel");
  zrokPanel.setAttribute("aria-labelledby", "remote-primary-zrok");

  const zrokFields = element("div", "remote-access-fields remote-access-fields-flush");
  const zrokAccountWrap = element("div", "remote-field remote-access-wide");
  const zrokHeading = element("div", "remote-access-field-heading");
  const zrokHeadingLeft = element("div", "remote-heading-inline");
  const zrokLabel = element("label", "", "zrok token 或注册邮箱");
  zrokLabel.htmlFor = "remote-zrok-account";
  const zrokSiteLink = element("a", "remote-external-link", "访问官网");
  zrokSiteLink.href = "https://api-v2.zrok.io/";
  zrokSiteLink.target = "_blank";
  zrokSiteLink.rel = "noopener noreferrer";
  zrokSiteLink.prepend(icon("window"));
  zrokHeadingLeft.append(zrokLabel, zrokSiteLink);

  const zrokProgressWrap = element("div", "zrok-download-progress");
  zrokProgressWrap.hidden = true;
  const zrokProgressText = element("span", "");
  const zrokProgressTrack = element("div", "zrok-download-track");
  zrokProgressTrack.setAttribute("role", "progressbar");
  zrokProgressTrack.setAttribute("aria-label", "下载 zrok 程序");
  zrokProgressTrack.setAttribute("aria-valuemin", "0");
  zrokProgressTrack.setAttribute("aria-valuemax", "100");
  const zrokProgressFill = element("div", "zrok-download-fill");
  zrokProgressTrack.append(zrokProgressFill);
  zrokProgressWrap.append(zrokProgressText, zrokProgressTrack);
  zrokHeading.append(zrokHeadingLeft, zrokProgressWrap);

  const zrokSetupRow = element("div", "remote-access-input-row zrok-setup-row");
  const zrokInput = element("input", "remote-input");
  zrokInput.id = "remote-zrok-account";
  zrokInput.type = "text";
  zrokInput.spellcheck = false;
  zrokInput.autocomplete = "off";
  zrokInput.addEventListener("input", () => {
    zrokInputDirty = true;
    zrokValue = zrokInput.value;
    render();
  });

  const fixedNameInput = element("input", "remote-input");
  fixedNameInput.id = "remote-zrok-fixed-name";
  fixedNameInput.type = "text";
  fixedNameInput.spellcheck = false;
  fixedNameInput.autocomplete = "off";
  fixedNameInput.maxLength = 63;
  fixedNameInput.setAttribute("aria-label", "固定公网名称");
  fixedNameInput.placeholder = "固定公网名称";
  fixedNameInput.hidden = true;
  fixedNameInput.addEventListener("input", () => {
    fixedNameDirty = true;
    fixedNameValue = fixedNameInput.value.toLowerCase();
    if (fixedNameInput.value !== fixedNameValue) fixedNameInput.value = fixedNameValue;
    render();
  });

  const zrokActionButton = button("检查中…", "zrok 操作", () => void handleZrokAction(), "remote-secondary-button");
  zrokActionButton.setAttribute("aria-describedby", "remote-service-guidance");
  zrokSetupRow.append(zrokInput, fixedNameInput, zrokActionButton);

  const serviceGuidanceEl = element("p", "remote-access-hint");
  serviceGuidanceEl.id = "remote-service-guidance";
  serviceGuidanceEl.setAttribute("role", "status");
  serviceGuidanceEl.setAttribute("aria-live", "polite");

  const fixedNameErrorEl = element("small", "remote-access-inline-error",
    "固定公网名称需为 3–63 位小写字母、数字或连字符，且首尾必须是字母或数字。");
  fixedNameErrorEl.hidden = true;

  const zrokSetupHintEl = element("small", "",
    "填写邮箱接收注册邮件，或填写 token 和固定公网名称；提交后保存至本机并回显。");

  zrokAccountWrap.append(zrokHeading, zrokSetupRow, serviceGuidanceEl, fixedNameErrorEl, zrokSetupHintEl);

  const zrokUrlWrap = element("div", "remote-field remote-access-wide");
  const zrokUrlLabel = element("label", "", "固定公网地址");
  zrokUrlLabel.htmlFor = "remote-zrok-url";
  const zrokUrlRow = element("div", "remote-access-input-row remote-access-public-row");
  const zrokUrlBox = element("div", "remote-access-key-input remote-access-public-input");
  const zrokUrlInput = element("input", "remote-input");
  zrokUrlInput.id = "remote-zrok-url";
  zrokUrlInput.readOnly = true;
  const zrokUrlActions = element("div", "remote-access-key-actions");
  const zrokUrlCopyButton = button("", "复制固定公网地址", () => void copyValue(zrokUrlInput.value), "icon-button remote-key-action");
  zrokUrlCopyButton.append(icon("copy"));
  zrokUrlActions.append(zrokUrlCopyButton);
  zrokUrlBox.append(zrokUrlInput, zrokUrlActions);
  const zrokTestButton = button("测试访问", "测试公网访问", () => void testPublicUrl(), "remote-secondary-button");
  zrokUrlRow.append(zrokUrlBox, zrokTestButton);
  zrokUrlWrap.append(
    zrokUrlLabel,
    zrokUrlRow,
    element("small", "", "关闭并重新启动 Elves 后复用同一地址；首次访问若出现 zrok 提示页，请点击「Visit Share」。")
  );

  zrokFields.append(zrokAccountWrap, zrokUrlWrap);

  const zrokErrorEl = element("p", "remote-access-error");
  zrokErrorEl.setAttribute("role", "alert");
  zrokErrorEl.hidden = true;
  const zrokRetryEl = element("p", "remote-access-hint");
  zrokRetryEl.hidden = true;

  zrokPanel.append(zrokFields, zrokErrorEl, zrokRetryEl);

  // frp panel（遵循产品规范：第一行按 服务器地址 → Token → 服务端口(80px) → 公网映射端口(80px) 排列）
  const frpPanel = element("div", "remote-primary-panel");
  frpPanel.id = "remote-primary-panel-frp";
  frpPanel.setAttribute("role", "tabpanel");
  frpPanel.setAttribute("aria-labelledby", "remote-primary-frp");
  frpPanel.hidden = true;

  const frpFields = element("div", "remote-frp-fields");

  const frpServerField = element("div", "remote-field remote-frp-server");
  const frpServerLabel = element("label", "", "服务器地址");
  frpServerLabel.htmlFor = "remote-frp-server";
  const frpServerInput = element("input", "remote-input");
  frpServerInput.id = "remote-frp-server";
  frpServerInput.type = "text";
  frpServerInput.spellcheck = false;
  frpServerInput.placeholder = "商家提供的 server_addr";
  frpServerInput.addEventListener("input", () => {
    frpConfig = { ...frpConfig, serverAddr: frpServerInput.value, ...(customPublicOrigin ? {} : { publicOrigin: "" }) };
    primaryFeedback = null;
    render();
  });
  frpServerInput.addEventListener("blur", () => {
    const normalized = normalizeFrpServerAddress(frpConfig.serverAddr);
    if (normalized !== frpConfig.serverAddr) {
      frpConfig = { ...frpConfig, serverAddr: normalized };
      frpServerInput.value = normalized;
      render();
    }
  });
  frpServerField.append(frpServerLabel, frpServerInput);

  const frpTokenField = element("div", "remote-field remote-frp-token");
  const frpTokenLabel = element("label", "", "Token");
  frpTokenLabel.htmlFor = "remote-frp-token";
  const frpTokenControl = createSecretInput({
    id: "remote-frp-token",
    label: "FRP Token",
    onInput: value => {
      frpTokenDirty = true;
      frpToken = value;
      primaryFeedback = null;
      render();
    },
    onCopy: value => copyPrimaryValue(value)
  });
  frpTokenField.append(frpTokenLabel, frpTokenControl.wrap);

  const frpServerPortField = element("div", "remote-field remote-frp-port");
  const frpServerPortLabel = element("label", "", "服务端口");
  frpServerPortLabel.htmlFor = "remote-frp-server-port";
  const frpServerPortInput = element("input", "remote-input");
  frpServerPortInput.id = "remote-frp-server-port";
  frpServerPortInput.type = "number";
  frpServerPortInput.min = "1";
  frpServerPortInput.max = "65535";
  frpServerPortInput.addEventListener("input", () => {
    frpConfig = { ...frpConfig, serverPort: Number(frpServerPortInput.value) || 0 };
    primaryFeedback = null;
    render();
  });
  frpServerPortField.append(frpServerPortLabel, frpServerPortInput);

  const frpRemotePortField = element("div", "remote-field remote-frp-port");
  const frpRemotePortLabel = element("label", "", "公网映射端口");
  frpRemotePortLabel.htmlFor = "remote-frp-remote-port";
  const frpRemotePortInput = element("input", "remote-input");
  frpRemotePortInput.id = "remote-frp-remote-port";
  frpRemotePortInput.type = "number";
  frpRemotePortInput.min = "1";
  frpRemotePortInput.max = "65535";
  frpRemotePortInput.addEventListener("input", () => {
    frpConfig = {
      ...frpConfig,
      remotePort: Number(frpRemotePortInput.value) || 0,
      ...(customPublicOrigin ? {} : { publicOrigin: "" })
    };
    primaryFeedback = null;
    render();
  });
  frpRemotePortField.append(frpRemotePortLabel, frpRemotePortInput);

  const frpOriginField = element("div", "remote-field remote-access-wide");
  const frpOriginLabel = element("label", "", "固定公网地址");
  frpOriginLabel.htmlFor = "remote-frp-url";
  const frpOriginBox = element("div", "remote-access-key-input remote-access-public-input");
  const frpOriginInput = element("input", "remote-input");
  frpOriginInput.id = "remote-frp-url";
  frpOriginInput.type = "text";
  frpOriginInput.spellcheck = false;
  frpOriginInput.setAttribute("aria-describedby", "remote-frp-url-hint");
  frpOriginInput.placeholder = "填写服务商提供的 HTTP/HTTPS 根地址；留空时自动生成";
  frpOriginInput.addEventListener("input", () => {
    const value = frpOriginInput.value;
    customPublicOrigin = Boolean(value.trim());
    frpConfig = { ...frpConfig, publicOrigin: value };
    primaryFeedback = null;
    render();
  });
  const frpOriginActions = element("div", "remote-access-key-actions");
  const frpOriginCopyButton = button("", "复制固定公网地址", () => void copyPrimaryValue(frpPublicOrigin(frpConfig)), "icon-button remote-key-action");
  frpOriginCopyButton.append(icon("copy"));
  frpOriginActions.append(frpOriginCopyButton);
  frpOriginBox.append(frpOriginInput, frpOriginActions);
  const frpOriginHint = element("small", "", "可填写服务商提供的 HTTP/HTTPS 根地址；清空后按服务器地址和公网映射端口自动生成。");
  frpOriginHint.id = "remote-frp-url-hint";
  frpOriginField.append(frpOriginLabel, frpOriginBox, frpOriginHint);

  frpFields.append(frpServerField, frpTokenField, frpServerPortField, frpRemotePortField, frpOriginField);
  frpPanel.append(frpFields);

  // 主链路操作栏（下载进度/提示在左，按钮在右）
  const primaryActionsRow = element("div", "remote-primary-actions");
  primaryActionsRow.hidden = true;

  const frpDownloadWrap = element("div", "remote-frp-download");
  frpDownloadWrap.hidden = true;
  const frpDownloadTitle = element("span", "");
  const frpDownloadBytes = element("span", "");
  const frpDownloadTrack = element("div", "remote-frp-download-track");
  frpDownloadTrack.setAttribute("role", "progressbar");
  frpDownloadTrack.setAttribute("aria-label", "下载 frp 程序");
  frpDownloadTrack.setAttribute("aria-valuemin", "0");
  frpDownloadTrack.setAttribute("aria-valuemax", "100");
  const frpDownloadFill = element("div", "remote-frp-download-fill");
  frpDownloadTrack.append(frpDownloadFill);
  frpDownloadWrap.append(frpDownloadTitle, frpDownloadBytes, frpDownloadTrack);

  const primaryFeedbackEl = element("p", "remote-access-hint");
  primaryFeedbackEl.setAttribute("role", "status");
  primaryFeedbackEl.setAttribute("aria-live", "polite");
  primaryFeedbackEl.setAttribute("aria-atomic", "true");

  const primaryButtons = element("div", "remote-primary-buttons");
  const openFrpDirButton = button("打开服务端目录", "打开 frp 程序目录", () => void openProgramDirectory(), "remote-secondary-button");
  openFrpDirButton.prepend(icon("folderClosed"));
  const primaryTestButton = button("保存并测试", "保存并测试主入口", () => void executePrimary("test"), "remote-secondary-button");
  const primaryApplyButton = button("应用为主链路", "应用为主链路", () => void executePrimary("apply"), "primary remote-action-button");
  primaryButtons.append(openFrpDirButton, primaryTestButton, primaryApplyButton);
  primaryActionsRow.append(frpDownloadWrap, primaryFeedbackEl, primaryButtons);

  // ngrok 第二线路（位于 zrok 视图下，开关独立控制）
  const ngrokFields = element("div", "remote-access-fields remote-ngrok-block");
  const ngrokWide = element("div", "remote-field remote-access-wide");
  const ngrokHeading = element("div", "remote-access-expiry-heading");
  ngrokHeading.append(element("span", "remote-section-subtitle", "ngrok 第二线路"));
  const ngrokSwitch = createSwitchControl("启用 ngrok 第二线路", checked => {
    void toggleNgrok(checked);
  });
  ngrokHeading.append(ngrokSwitch.wrap);

  const ngrokBody = element("div", "remote-ngrok-body");
  const ngrokRow = element("div", "remote-access-input-row remote-access-key-row");
  const ngrokTokenControl = createSecretInput({
    id: "remote-ngrok-token",
    label: "ngrok Token",
    minLength: 20,
    maxLength: 512,
    onInput: value => {
      ngrokTokenDirty = true;
      ngrokToken = value;
      render();
    },
    onCopy: value => copyValue(value)
  });
  const ngrokSaveButton = button("保存并测试", "保存并测试 ngrok Token", () => void saveNgrokToken(), "remote-secondary-button");
  ngrokRow.append(ngrokTokenControl.wrap, ngrokSaveButton);
  const ngrokHint = element("small", "");
  ngrokBody.append(ngrokRow, ngrokHint);

  ngrokWide.append(ngrokHeading, ngrokBody);
  ngrokFields.append(ngrokWide);

  publicSection.append(publicHead, primaryUnsupportedHint, zrokPanel, frpPanel, primaryActionsRow, ngrokFields);

  // --- 分区 3：访问密钥与设备授权 ---
  const authSection = element("div", "remote-access-section");
  const authHead = element("div", "remote-panel-head");
  const authCopy = element("div", "remote-panel-head-copy");
  const authDesc = element("p", "remote-panel-desc", "访问密钥状态尚未确认。 手机端输入此密钥即可连接，更换后旧设备需重新登录。");
  authCopy.append(element("h2", "", "访问密钥与设备授权"), authDesc);
  authHead.append(authCopy);

  const loginLockedAlert = element("p", "remote-access-error",
    "访问密钥连续输错超过3次，新设备登录已锁定。请点击「生成」，再点击右侧「保存」；只有不同的新密钥保存成功后才会解锁。已登录设备当前不受影响。");
  loginLockedAlert.setAttribute("role", "alert");
  loginLockedAlert.hidden = true;

  const keyFields = element("div", "remote-access-fields remote-access-fields-top");
  const keyWide = element("div", "remote-field remote-access-wide");
  const keyLabel = element("label", "", "设置访问密钥");
  keyLabel.htmlFor = "remote-access-key";
  const keyRow = element("div", "remote-access-input-row remote-access-key-row");
  const keyControl = createSecretInput({
    id: "remote-access-key",
    label: "访问密钥",
    minLength: 8,
    maxLength: 64,
    onInput: value => {
      keyDirty = true;
      keyValue = value;
      render();
    },
    onCopy: value => copyValue(value)
  });
  const generateKeyButton = button("生成", "生成随机访问密钥", () => void generateAccessKey(), "remote-secondary-button");
  const saveKeyButton = button("保存", "保存访问密钥", () => void saveAccessKey(), "primary remote-action-button");
  keyRow.append(keyControl.wrap, generateKeyButton, saveKeyButton);
  keyWide.append(keyLabel, keyRow, element("small", "", "8–64 位非空格 ASCII 字符；保存后以明文存于本机。"));
  keyFields.append(keyWide);

  const devicesList = element("div", "remote-access-devices");
  authSection.append(authHead, loginLockedAlert, keyFields, devicesList);

  panel.append(serviceSection, publicSection, authSection);

  const messageEl = element("p", "remote-access-message");
  messageEl.setAttribute("role", "status");
  messageEl.setAttribute("aria-live", "polite");
  messageEl.hidden = true;

  root.append(heading, panel, messageEl);

  function setStatusMessage(text = "") {
    statusMessage = text;
    messageEl.textContent = text;
    messageEl.hidden = !text;
  }

  const copyValue = async value => {
    if (!value) return;
    try {
      await copyText(value);
      setStatusMessage("已复制到剪贴板");
      notice("已复制到剪贴板");
    } catch {
      setStatusMessage("复制失败，请手动选择文本复制");
      notice("复制失败，请手动选择文本复制", true);
    }
  };

  const copyPrimaryValue = async value => {
    if (!value) return;
    try {
      await copyText(value);
      primaryFeedback = { text: "已复制到剪贴板。", kind: "success" };
      render();
    } catch {
      primaryFeedback = { text: "复制失败，请手动选择文本复制。", kind: "error" };
      render();
    }
  };

  function updatePolling() {
    if (disposed) return;
    const active = busy
      || Boolean(status?.primary?.switching)
      || ["starting", "downloading"].includes(status?.primary?.frp?.state || "")
      || ["checking", "downloading", "starting", "connecting"].includes(status?.zrok?.state || "");
    const nextInterval = active ? 500 : 5000;
    if (pollTimer && pollIntervalMs === nextInterval) return;
    if (pollTimer) clearInterval(pollTimer);
    pollIntervalMs = nextInterval;
    pollTimer = window.setInterval(() => void refresh(), pollIntervalMs);
  }

  const run = async operation => {
    busy = true;
    setStatusMessage("");
    render();
    try {
      await operation();
    } catch (error) {
      if (!disposed) {
        const text = formatError(error);
        setStatusMessage(text);
        notice(text, true);
      }
    } finally {
      if (!disposed) {
        busy = false;
        render();
      }
    }
  };

  function adoptStatus(nextStatus) {
    status = nextStatus;
    if (nextStatus?.settings && !settingsDirty) {
      settings = { ...defaultRemoteAccessSettings, ...nextStatus.settings };
      if (!zrokInputDirty && settings.zrokSetupValue !== loadedZrokSetupValue) {
        loadedZrokSetupValue = settings.zrokSetupValue;
        zrokValue = settings.zrokSetupValue;
      }
    }
    if (nextStatus?.primary) {
      if (!initializedPrimary) {
        initializedPrimary = true;
        editingPrimary = nextStatus.primary.provider || "zrok";
      }
      if (!frpInitialized && nextStatus.primary.frp?.config) {
        frpInitialized = true;
        frpConfig = { ...defaultFrpConfig, ...nextStatus.primary.frp.config, httpsMode: "provider" };
        const savedOrigin = String(nextStatus.primary.frp.config.publicOrigin || "").trim();
        const automaticOrigin = frpPublicOrigin({ ...nextStatus.primary.frp.config, publicOrigin: "" });
        customPublicOrigin = Boolean(savedOrigin) && savedOrigin !== automaticOrigin;
      }
    }
    if (!fixedNameDirty && nextStatus?.zrok?.fixedName) {
      fixedNameValue = nextStatus.zrok.fixedName;
    }
  }

  const refresh = async () => {
    if (disposed || refreshing) return;
    refreshing = true;
    const lockRev = loginLockRevision;
    const ngrokRev = ngrokTokenRevision;
    try {
      const saved = await command("credentials");
      if (disposed || lockRev !== loginLockRevision) return;
      if (saved?.status === "ok") {
        credentials = saved;
        if (typeof saved.loginLocked === "boolean") loginLocked = saved.loginLocked;
        const savedKey = saved.key || "";
        if (!keyDirty && loadedKeyValue !== savedKey) {
          loadedKeyValue = savedKey;
          keyValue = savedKey;
          keyControl.setVisible(false);
        }
        if (ngrokRev === ngrokTokenRevision && typeof saved.ngrokTokenConfigured === "boolean") {
          ngrokTokenConfigured = saved.ngrokTokenConfigured;
          const savedToken = saved.ngrokToken || "";
          if (!ngrokTokenDirty && loadedNgrokToken !== savedToken) {
            loadedNgrokToken = savedToken;
            ngrokToken = savedToken;
            ngrokTokenControl.setVisible(false);
          }
        }
        if (!frpTokenDirty && saved.frpToken !== undefined && saved.frpToken !== loadedFrpToken) {
          loadedFrpToken = saved.frpToken;
          frpToken = saved.frpToken;
          frpTokenControl.setVisible(false);
        }
      } else {
        credentials = null;
        setStatusMessage(saved?.message || "凭据状态读取失败");
      }

      const result = await command("status");
      if (disposed) return;
      adoptStatus(result);
      if (ngrokRev === ngrokTokenRevision && typeof result?.ngrok?.configured === "boolean") {
        ngrokTokenConfigured = result.ngrok.configured;
      }
      if (lockRev === loginLockRevision && result?.status === "ok" && typeof result.loginLocked === "boolean") {
        loginLocked = result.loginLocked;
      }
      if (result?.status !== "ok") setStatusMessage(result?.message || "本机服务未连接");
    } catch (error) {
      if (!disposed && error?.name !== "AbortError") {
        status = null;
        setStatusMessage(formatError(error));
      }
    } finally {
      refreshing = false;
      if (!disposed) render();
    }
  };

  const saveZrokSetupValue = async value => {
    if (settings.zrokSetupValue === value) return;
    const next = { ...settings, zrokSetupValue: value };
    const result = await command("saveSettings", { settings: next, apply: false });
    if (disposed) return;
    if (result?.status !== "ok") throw new Error(result?.message || "zrok 配置保存失败");
    settings = { ...next, ...(result.settings || {}) };
    loadedZrokSetupValue = settings.zrokSetupValue;
    zrokInputDirty = false;
  };

  const commitRemoteAccessSettings = async (next, successMessage = "") => {
    const validationError = remoteAccessValidation(next);
    if (validationError) throw new Error(validationError);
    const previous = { ...settings };
    settings = { ...next };
    render();
    let result;
    try {
      result = await command("saveSettings", { settings: next, apply: true });
    } catch (error) {
      settings = previous;
      render();
      throw error;
    }
    if (disposed) return;
    settingsDirty = false;
    adoptStatus(result);
    if (result?.status !== "ok") {
      settings = previous;
      throw new Error(result?.message || "配置未应用");
    }
    if (result.error) throw new Error(result.error);
    if (result.settings && Object.keys(next).some(key => result.settings[key] !== next[key])) {
      setStatusMessage("请核对保存结果。服务正在使用已保存的配置。");
      return;
    }
    const zrokNotice = result.zrok?.state === "ready"
      ? "zrok 固定公网可访问。"
      : result.zrok?.bound && result.zrok?.fixedName
        ? "zrok 固定公网正在启动并验证访问。"
        : "";
    const keyConfigured = result.accessKeyConfigured ?? credentials?.accessKeyConfigured ?? false;
    const authorizationNotice = keyConfigured
      ? ""
      : "访问密钥尚未设置，网页只显示登录页，无法进入会话。";
    const okMessage = successMessage || (result.running
      ? `设置已保存，远程网页服务已启动。${zrokNotice}${authorizationNotice}`
      : "设置已保存，远程访问已关闭。");
    notice(okMessage);
  };

  const toggleWebService = async checked => {
    primaryFeedback = null;
    await run(() => commitRemoteAccessSettings({ ...settings, enabled: Boolean(checked) }));
  };

  const saveServiceSettings = async () => {
    await run(() => commitRemoteAccessSettings({ ...settings }));
  };

  const toggleNgrok = async checked => {
    await run(() => {
      const next = { ...settings, ngrokEnabled: Boolean(checked) };
      return commitRemoteAccessSettings(next, next.ngrokEnabled ? "ngrok 第二线路已启用。" : "ngrok 第二线路已停用，Token 已保留。");
    });
  };

  const handleZrokAction = async () => {
    await run(async () => {
      const zrok = status?.zrok;
      if (!zrok) return;
      const zrokTrimmed = zrokValue.trim();
      const isEmail = EMAIL_PATTERN.test(zrokTrimmed);
      const isToken = Boolean(zrokTrimmed) && !isEmail;
      const normalizedName = fixedNameValue.trim().toLowerCase();

      if (zrok.installed && !isEmail) {
        const latest = await command("status");
        if (disposed) return;
        adoptStatus(latest);
        if (latest?.status !== "ok") throw new Error(latest?.message || "无法确认远程服务状态，请稍后重试。");
        if (!latest.enabled || !latest.running) throw new Error("请先启用上方「远程会话网页」。");
      }
      if (zrokTrimmed) await saveZrokSetupValue(zrokTrimmed);
      if (!zrok.installed) {
        const result = await command("downloadZrok");
        if (disposed) return;
        adoptStatus(result);
        if (result?.status !== "ok") throw new Error(result?.message || "zrok 下载任务启动失败");
        notice("已开始下载 zrok 程序。");
        return;
      }
      if (isEmail) {
        const result = await command("registerZrok", { email: zrokTrimmed });
        if (disposed) return;
        if (result?.status !== "ok") throw new Error(result?.message || "zrok 注册失败");
        notice(result.message || "注册邮件已发送，请检查邮箱。");
        return;
      }
      const targetFixedName = isToken ? normalizedName : zrok.fixedName;
      const result = await command("startZrok", {
        token: isToken ? zrokTrimmed : "",
        fixedName: targetFixedName
      });
      if (disposed) return;
      adoptStatus(result);
      if (result?.status !== "ok") throw new Error(result?.message || "zrok 公网启动失败");
      fixedNameDirty = false;
      fixedNameValue = result.zrok?.fixedName || targetFixedName;
      notice(result.primary?.provider === "frp"
        ? "zrok 绑定已保存，可验证并应用为主链路。"
        : result.zrok?.state === "ready" ? "zrok 固定公网已启动。" : "zrok 固定公网正在启动并验证访问。");
    });
  };

  const testPublicUrl = async () => {
    await run(async () => {
      if (!status?.zrok?.publicUrl) return;
      const result = await command("testZrok");
      if (disposed) return;
      await refresh();
      if (disposed) return;
      if (result?.status !== "ok") throw new Error(result?.message || "公网访问验证失败，请检查网络连接后重试。");
      notice(result.message || "公网访问验证通过。");
    });
  };

  const preparePrimaryAction = async provider => {
    const zrok = status?.zrok;
    if (provider !== "zrok" || !zrok) return;
    const zrokTrimmed = zrokValue.trim();
    const isEmail = EMAIL_PATTERN.test(zrokTrimmed);
    const isToken = Boolean(zrokTrimmed) && !isEmail;
    const normalizedName = fixedNameValue.trim().toLowerCase();
    const fixedValid = normalizedName.length >= 3 && normalizedName.length <= 63 && FIXED_NAME_PATTERN.test(normalizedName);
    const setupChanged = zrokTrimmed !== String(settings.zrokSetupValue || "").trim() || fixedNameDirty;
    if (!setupChanged) return;
    if (zrokTrimmed) await saveZrokSetupValue(zrokTrimmed);
    if (!settings.enabled) return;
    if (isEmail) throw new Error("当前输入是注册邮箱，请先点击右侧「注册公网」，收到 token 后再测试主入口。");
    if (!zrok.installed) throw new Error("请先下载 zrok 程序。");
    if (!isToken) throw new Error("请输入 zrok token。");
    if (!fixedValid) throw new Error("固定公网名称需为 3–63 位小写字母、数字或连字符，且首尾必须是字母或数字。");
    const result = await command("startZrok", { token: zrokTrimmed, fixedName: normalizedName });
    if (disposed) return;
    adoptStatus(result);
    if (result?.status !== "ok") throw new Error(result?.message || "zrok 配置保存失败");
    fixedNameDirty = false;
    fixedNameValue = result.zrok?.fixedName || normalizedName;
  };

  const saveFrpConfig = async () => {
    const normalized = normalizeFrpConfig(frpConfig);
    if (!normalized.serverAddr || !frpToken.trim()) throw new Error("请填写 FRP 服务器地址和 Token。");
    if (![normalized.serverPort, normalized.remotePort].every(value => Number.isInteger(value) && value >= 1 && value <= 65535)) {
      throw new Error("服务端口与公网映射端口必须在 1–65535 之间。");
    }
    if (!frpPublicOrigin(frpConfig)) throw new Error("请填写有效的服务器地址和公网映射端口。");
    const result = await command("saveFrp", { config: normalized, token: frpToken.trim() });
    if (result?.status !== "ok") throw new Error(result?.message || "FRP 配置保存失败");
    frpConfig = normalized;
    frpTokenDirty = true;
    loadedFrpToken = frpToken.trim();
    frpToken = frpToken.trim();
    frpTokenControl.setVisible(false);
  };

  const executePrimary = async action => {
    await run(async () => {
      primaryOperation = action;
      primaryFeedback = null;
      render();
      try {
        if (editingPrimary === "frp") await saveFrpConfig();
        else await preparePrimaryAction(editingPrimary);
        if (!settings.enabled && action === "test") {
          primaryFeedback = { text: "FRP 候选配置已保存；启用远程会话网页后可测试入口。", kind: "success" };
          return;
        }
        const result = await command(action === "apply" ? "applyPrimaryRoute" : "testPrimaryRoute", { provider: editingPrimary });
        if (result?.status !== "ok") throw new Error(result?.message || "主入口验证失败");
        primaryFeedback = { text: result.message || "操作完成。", kind: result.httpsReady === false ? "warning" : "success" };
      } catch (error) {
        primaryFeedback = { text: formatError(error), kind: "error" };
      } finally {
        try {
          await refresh();
        } finally {
          primaryOperation = null;
          if (!disposed) render();
        }
      }
    });
  };

  const openProgramDirectory = async () => {
    openingDirectory = true;
    render();
    try {
      const result = await command("openFrpDirectory");
      if (result?.status !== "ok") throw new Error(result?.message || "打开 frp 程序目录失败");
      if (result.message) primaryFeedback = { text: result.message, kind: "success" };
    } catch (error) {
      primaryFeedback = { text: formatError(error), kind: "error" };
    } finally {
      if (!disposed) {
        openingDirectory = false;
        render();
      }
    }
  };

  const saveNgrokToken = async () => {
    await run(async () => {
      const nextToken = ngrokToken.trim();
      const result = await command("saveNgrok", { token: nextToken });
      if (disposed) return;
      if (result?.status !== "ok") throw new Error(result?.message || "ngrok 验证失败");
      ngrokTokenRevision++;
      loadedNgrokToken = nextToken;
      ngrokTokenDirty = false;
      ngrokTokenConfigured = true;
      ngrokToken = nextToken;
      ngrokTokenControl.setVisible(false);
      await refresh();
      if (!disposed) notice(result.message || "ngrok Token 已保存并通过测试。");
    });
  };

  const generateAccessKey = async () => {
    await run(async () => {
      const result = await command("generateKey");
      if (disposed) return;
      if (result?.status !== "ok" || !result.key) throw new Error(result?.message || "密钥生成失败");
      keyDirty = true;
      keyValue = result.key;
      keyControl.setVisible(false);
      setStatusMessage(loginLocked
        ? "随机密钥已生成，尚未保存。请点击右侧「保存」；只有不同的新密钥保存成功后才会解除登录锁定，并使原有设备重新登录。"
        : "随机密钥已生成，尚未保存。点击右侧「保存」后生效，当前设备授权不受影响。");
    });
  };

  const saveAccessKey = async () => {
    const nextKey = keyValue.trim();
    if (!ACCESS_KEY_PATTERN.test(nextKey)) {
      notice("密钥需为 8–64 个非空格 ASCII 字符。", true);
      return;
    }
    if (credentials?.accessKeyConfigured !== false) {
      const confirmed = await confirmAction(
        "更换访问密钥",
        "更换访问密钥会使所有已授权设备失效，需要重新输入新密钥。继续？",
        "保存新密钥",
        true
      );
      if (!confirmed || disposed) return;
    }
    await run(async () => {
      const result = await command("setKey", { key: nextKey });
      if (disposed) return;
      if (result?.status !== "ok") throw new Error(result?.message || "访问密钥保存失败");
      loginLockRevision++;
      const savedKey = result.key || nextKey;
      loadedKeyValue = savedKey;
      keyDirty = false;
      keyValue = savedKey;
      keyControl.setVisible(false);
      credentials = { ...credentials, ...result, key: savedKey };
      if (typeof result.loginLocked === "boolean") loginLocked = result.loginLocked;
      await refresh();
      if (!disposed) {
        notice(result.loginLocked === true
          ? "访问密钥已保存，登录仍处于锁定状态。请生成不同的新密钥并保存。"
          : "访问密钥已保存并生效；更换密钥后，原有设备需使用新密钥重新登录。");
      }
    });
  };

  const revokeDevice = async device => {
    const confirmed = await confirmAction(
      "撤销设备授权",
      `撤销「${device.name}」的授权？此设备将立即失去会话访问权限。`,
      "撤销",
      true
    );
    if (!confirmed || disposed) return;
    await run(async () => {
      const result = await command("revoke", { deviceId: device.id });
      if (disposed) return;
      if (result?.status !== "ok") throw new Error(result?.message || "撤销失败");
      await refresh();
      if (!disposed) {
        setStatusMessage("设备授权已撤销。");
        notice("设备授权已撤销。");
      }
    });
  };

  function render() {
    if (disposed) return;
    const zrok = status?.zrok;
    const primary = status?.primary;
    const serviceReady = status?.status === "ok" && status.serviceConnected === true;

    // 1. 远程会话网页
    refreshButton.disabled = refreshing;
    webSwitch.setState(settings.enabled, busy, "已启用", "已停用");
    saveButton.disabled = busy;
    saveButton.textContent = busy ? "处理中…" : "保存并应用";

    serviceError.textContent = status?.error || "";
    serviceError.hidden = !status?.error;

    if (document.activeElement !== bindInput) bindInput.value = settings.bindHost;
    bindInput.disabled = busy;
    if (document.activeElement !== portInput) portInput.value = String(settings.port || "");
    portInput.disabled = busy;

    const expiryEnabled = settings.deviceSessionHours > 0;
    expirySwitch.setState(expiryEnabled, busy, "已开启", "已关闭");
    if (expiryEnabled) {
      expiryInput.type = "number";
      expiryInput.min = "1";
      expiryInput.max = "720";
      expiryInput.disabled = busy;
      if (document.activeElement !== expiryInput) expiryInput.value = String(settings.deviceSessionHours);
      expiryHint.textContent = "对新登录设备生效，到期需重新输入密钥。";
    } else {
      expiryInput.type = "text";
      expiryInput.removeAttribute("min");
      expiryInput.removeAttribute("max");
      expiryInput.disabled = true;
      expiryInput.value = "未开启有效期限制（长期有效）";
      expiryHint.textContent = "默认关闭，已授权设备长期有效。";
    }

    // 2. 公网入口头部 & 面包屑
    const tabsDisabled = busy || Boolean(primary?.switching);
    for (const provider of ["zrok", "frp"]) {
      const selected = editingPrimary === provider;
      tabButtons[provider].setAttribute("aria-selected", String(selected));
      tabButtons[provider].tabIndex = selected ? 0 : -1;
      tabButtons[provider].disabled = tabsDisabled;
    }
    publicDesc.replaceChildren(
      ...(editingPrimary === "zrok"
        ? [document.createTextNode("免费，但是稳定性欠佳，建议开启 "), element("strong", "", "ngrok 第二线路")]
        : [document.createTextNode("自行搭建或者购买，速度快稳定性好，"), element("strong", "", "frp程序会被误杀需要自行加白")])
    );

    publicStatusWrap.replaceChildren();
    if (primary?.provider === "frp") {
      const frpState = primary.frp?.state || "stopped";
      const badgeClass = frpState === "ready" ? "connected" : frpState === "error" ? "error" : "connecting";
      const badge = element("span", `remote-status-badge ${badgeClass}`);
      badge.append(element("span", "remote-status-dot"), document.createTextNode(`当前主链路：FRP · ${frpStateLabel(frpState)}`));
      publicStatusWrap.append(badge);
    } else {
      const badgeInfo = zrokBadgeInfo(zrok, serviceReady);
      const badge = element("span", `remote-status-badge ${badgeInfo.className}`.trim());
      if (badgeInfo.title) badge.title = badgeInfo.title;
      badge.append(element("span", "remote-status-dot"), document.createTextNode(badgeInfo.label));
      publicStatusWrap.append(element("span", "remote-current-primary-label", "当前主链路：zrok"), badge);
    }

    primaryUnsupportedHint.hidden = Boolean(primary) || status?.status !== "ok";
    zrokPanel.hidden = editingPrimary !== "zrok";
    frpPanel.hidden = editingPrimary !== "frp";

    // zrok 表单
    const zrokTrimmed = zrokValue.trim();
    const isEmail = EMAIL_PATTERN.test(zrokTrimmed);
    const isToken = Boolean(zrokTrimmed) && !isEmail;
    const normalizedName = fixedNameValue.trim().toLowerCase();
    const fixedNameValid = normalizedName.length >= 3 && normalizedName.length <= 63 && FIXED_NAME_PATTERN.test(normalizedName);
    const zrokDownloading = zrok?.state === "downloading";
    const zrokTransitioning = ["checking", "downloading", "starting", "connecting"].includes(zrok?.state || "");
    const zrokDownloadPercent = Math.max(0, Math.min(100, Math.round(zrok?.downloadPercent || 0)));

    zrokProgressWrap.hidden = !zrokDownloading;
    if (zrokDownloading) {
      zrokProgressText.textContent = zrok?.downloadTotalBytes ? `下载程序 ${zrokDownloadPercent}%` : "正在下载程序";
      if (zrok?.downloadTotalBytes) {
        zrokProgressTrack.setAttribute("aria-valuenow", String(zrokDownloadPercent));
        zrokProgressFill.classList.remove("indeterminate");
        zrokProgressFill.style.width = `${zrokDownloadPercent}%`;
      } else {
        zrokProgressTrack.removeAttribute("aria-valuenow");
        zrokProgressFill.classList.add("indeterminate");
        zrokProgressFill.style.width = "";
      }
    }

    zrokSetupRow.classList.toggle("with-fixed-name", isToken);
    if (document.activeElement !== zrokInput) zrokInput.value = zrokValue;
    zrokInput.disabled = busy || zrokTransitioning;
    zrokInput.placeholder = zrok?.bound ? "已绑定；也可输入邮箱重新注册" : "输入 zrok token 或邮箱";

    fixedNameInput.hidden = !isToken;
    if (document.activeElement !== fixedNameInput) fixedNameInput.value = fixedNameValue;
    fixedNameInput.disabled = busy || zrokTransitioning;

    const zrokActionLabel = !zrok
      ? "检查中…"
      : !zrok.installed
        ? "下载程序"
        : isEmail
          ? "注册公网"
          : primary?.provider === "frp" ? "保存绑定" : "启动公网";
    zrokActionButton.textContent = busy ? "处理中…" : zrokActionLabel;
    zrokActionButton.disabled = busy
      || !zrok
      || zrokTransitioning
      || (zrok.installed && isToken && (!settings.enabled || !fixedNameValid))
      || (zrok.installed && !zrokTrimmed && (!settings.enabled || !zrok.bound || !zrok.fixedName || zrok.running));

    serviceGuidanceEl.textContent = !settings.enabled
      ? "请先启用上方「远程会话网页」，等待会话服务就绪后启动公网。"
      : serviceReady
        ? "Codex 会话服务已连接，可以启动公网。会话、侧边会话和审批均由服务处理。"
        : SERVICE_WAITING_GUIDANCE;
    fixedNameErrorEl.hidden = !(isToken && fixedNameValue && !fixedNameValid);
    zrokSetupHintEl.hidden = Boolean(isToken || normalizedName);

    const publicUrl = zrok?.publicUrl || "";
    zrokUrlInput.value = publicUrl;
    zrokUrlInput.placeholder = zrokTransitioning ? "正在准备 zrok 固定公网地址…" : "绑定并启动后显示固定公网地址";
    zrokUrlCopyButton.disabled = !publicUrl;
    zrokTestButton.disabled = busy || !settings.enabled || !publicUrl || !zrok?.running;
    zrokTestButton.textContent = busy ? "处理中…" : "测试访问";

    const showZrokError = !primary && ["connecting", "retry_wait", "error"].includes(zrok?.state || "") && Boolean(zrok?.error);
    zrokErrorEl.hidden = !showZrokError;
    zrokErrorEl.textContent = showZrokError ? zrok.error : "";
    const showRetryWait = zrok?.state === "retry_wait" && Boolean(zrok?.retryAtMs);
    zrokRetryEl.hidden = !showRetryWait;
    zrokRetryEl.textContent = showRetryWait ? `将在 ${new Date(zrok.retryAtMs).toLocaleTimeString()} 自动重试。` : "";

    // FRP 表单 & 主链路操作栏
    const frpDownloading = primary?.frp?.state === "downloading";
    const primaryPending = busy || Boolean(primaryOperation) || Boolean(primary?.switching) || frpDownloading;
    const computedOrigin = frpPublicOrigin(frpConfig);
    const normalizedFrp = normalizeFrpConfig(frpConfig);
    const draftFrp = primary?.frp?.config;
    const localFrpChanged = draftFrp ? (
      draftFrp.serverAddr !== normalizedFrp.serverAddr
      || draftFrp.serverPort !== normalizedFrp.serverPort
      || draftFrp.remotePort !== normalizedFrp.remotePort
      || draftFrp.publicOrigin !== normalizedFrp.publicOrigin
    ) : false;
    const localTokenChanged = frpToken.trim() !== String(loadedFrpToken || "").trim();
    const frpConfigChanged = editingPrimary === "frp"
      && (primary?.frp?.configMatchesActive === false || localFrpChanged || localTokenChanged);

    if (document.activeElement !== frpServerInput) frpServerInput.value = frpConfig.serverAddr;
    frpServerInput.disabled = primaryPending;
    if (document.activeElement !== frpTokenControl.input) frpTokenControl.setValue(frpToken);
    frpTokenControl.setDisabled(primaryPending);
    if (document.activeElement !== frpServerPortInput) frpServerPortInput.value = frpConfig.serverPort ? String(frpConfig.serverPort) : "";
    frpServerPortInput.disabled = primaryPending;
    if (document.activeElement !== frpRemotePortInput) frpRemotePortInput.value = frpConfig.remotePort ? String(frpConfig.remotePort) : "";
    frpRemotePortInput.disabled = primaryPending;
    if (document.activeElement !== frpOriginInput) frpOriginInput.value = frpConfig.publicOrigin || computedOrigin;
    frpOriginInput.disabled = primaryPending;
    if (frpConfig.publicOrigin.trim() && !computedOrigin) frpOriginInput.setAttribute("aria-invalid", "true");
    else frpOriginInput.removeAttribute("aria-invalid");
    frpOriginCopyButton.disabled = primaryPending || !computedOrigin;

    const knownPrimary = Boolean(primary);
    primaryActionsRow.hidden = !knownPrimary;
    if (knownPrimary) {
      const currentProvider = primary.provider || "zrok";
      const currentApplied = currentProvider === editingPrimary && !(editingPrimary === "frp" && frpConfigChanged);
      const webReady = !settings.enabled || status?.running === true;
      const downloadTotal = primary.frp?.downloadTotalBytes || 0;
      const downloaded = primary.frp?.downloadedBytes || 0;
      const downloadPercent = downloadTotal > 0 ? Math.max(0, Math.min(100, Math.floor(downloaded / downloadTotal * 100))) : null;

      frpDownloadWrap.hidden = !frpDownloading;
      primaryFeedbackEl.hidden = frpDownloading;
      if (frpDownloading) {
        frpDownloadTitle.textContent = `下载 frp${downloadPercent !== null ? ` ${downloadPercent}%` : "…"}`;
        frpDownloadBytes.textContent = downloadTotal > 0
          ? `${downloadSize(downloaded)} / ${downloadSize(downloadTotal)}`
          : downloaded > 0 ? downloadSize(downloaded) : "正在连接…";
        if (downloadPercent !== null) {
          frpDownloadTrack.setAttribute("aria-valuenow", String(downloadPercent));
          frpDownloadFill.classList.remove("indeterminate");
          frpDownloadFill.style.width = `${downloadPercent}%`;
        } else {
          frpDownloadTrack.removeAttribute("aria-valuenow");
          frpDownloadFill.classList.add("indeterminate");
          frpDownloadFill.style.width = "";
        }
      } else {
        const providerError = editingPrimary === "frp"
          ? primary.frp?.error
          : ["connecting", "retry_wait", "error"].includes(zrok?.state || "") ? zrok?.error : null;
        const activeFeedback = primaryFeedback || (primaryOperation
          ? { text: primaryOperation === "apply" ? "正在验证并切换主链路…" : settings.enabled ? "正在保存并测试入口…" : "正在保存配置…", kind: "success" }
          : providerError ? { text: providerError, kind: "error" } : null);
        primaryFeedbackEl.className = activeFeedback ? `remote-primary-feedback ${activeFeedback.kind}` : "remote-access-hint";
        primaryFeedbackEl.textContent = activeFeedback?.text || (
          editingPrimary === "frp" && computedOrigin.startsWith("http:")
            ? "HTTP 公网传输不加密；验证通过后可应用为主链路。"
            : currentProvider === editingPrimary
              ? (editingPrimary === "frp" && frpConfigChanged
                ? "测试或应用新配置时会重启当前 FRP 连接；验证失败后旧入口不会继续使用。"
                : "当前主链路的配置已保留，可查看另一份配置。")
              : "切换公网地址后，远程设备需用新地址重新登录。"
        );
      }

      openFrpDirButton.hidden = editingPrimary !== "frp";
      openFrpDirButton.disabled = openingDirectory;
      openFrpDirButton.replaceChildren(icon("folderClosed"), document.createTextNode(openingDirectory ? "正在打开…" : "打开服务端目录"));

      primaryTestButton.disabled = primaryPending || (!settings.enabled && editingPrimary !== "frp");
      primaryTestButton.textContent = frpDownloading ? "下载中…" : primaryOperation === "test" ? "正在处理…" : settings.enabled ? "保存并测试" : "保存配置";

      primaryApplyButton.disabled = primaryPending || !webReady || currentApplied || (editingPrimary === "frp" && !computedOrigin);
      primaryApplyButton.textContent = primaryOperation === "apply"
        ? "正在切换…"
        : currentApplied ? "当前主链路" : settings.enabled ? "应用为主链路" : "保存为主链路";
    }

    // ngrok 第二线路
    ngrokFields.hidden = editingPrimary !== "zrok";
    ngrokSwitch.setState(settings.ngrokEnabled, busy, "已启用", "已停用");
    ngrokBody.hidden = !settings.ngrokEnabled;
    if (settings.ngrokEnabled) {
      if (document.activeElement !== ngrokTokenControl.input) ngrokTokenControl.setValue(ngrokToken);
      ngrokTokenControl.setDisabled(busy);
      ngrokTokenControl.setPlaceholder(ngrokTokenConfigured
        ? loadedNgrokToken ? "输入新 Token 替换" : "已保存 Token 无法读取，请重新输入并保存"
        : "输入 ngrok Token");
      ngrokSaveButton.disabled = busy || !settings.enabled || !settings.ngrokEnabled || !ngrokToken.trim();
      ngrokSaveButton.textContent = busy ? "处理中…" : "保存并测试";
      ngrokHint.textContent = !settings.enabled
        ? "远程会话网页未启用，第二线路将在网页服务启动后连接。"
        : status?.ngrok?.state === "ready"
          ? "第二通道已连接，安卓端将自动发现并同时使用。"
          : status?.ngrok?.state === "starting"
            ? "第二通道正在启动…"
            : status?.ngrok?.error || "请保存并测试 Token，第二线路启用后安卓端会自动发现，无需填写第二个域名。";
    }

    // 3. 访问密钥与设备授权
    authDesc.textContent = `${credentials === null
      ? "访问密钥状态尚未确认。"
      : credentials.accessKeyConfigured ? "访问密钥已设置。" : "尚未设置访问密钥。"} 手机端输入此密钥即可连接，更换后旧设备需重新登录。`;
    loginLockedAlert.hidden = !loginLocked;

    if (document.activeElement !== keyControl.input) keyControl.setValue(keyValue);
    keyControl.setDisabled(busy);
    keyControl.setPlaceholder(credentials?.accessKeyConfigured
      ? credentials.key ? "输入新密钥替换" : "旧密钥未保存明文，请重新输入并保存"
      : "8–64 个非空格 ASCII 字符，建议随机生成");
    generateKeyButton.disabled = busy;
    saveKeyButton.disabled = busy || !keyDirty || !keyValue.trim();

    // 设备列表
    const devices = Array.isArray(status?.devices) ? status.devices : [];
    devicesList.replaceChildren();
    if (!devices.length) {
      devicesList.append(element("p", "remote-access-hint", "尚无已授权设备。"));
    } else {
      const oneYearLater = Date.now() + 365 * 24 * 3600 * 1000;
      for (const device of devices) {
        const row = element("div", "remote-access-device");
        const deviceIcon = icon("smartphone");
        const info = element("div", "remote-access-device-info");
        const expiryText = device.expiresAtMs > oneYearLater ? "长期有效" : new Date(device.expiresAtMs).toLocaleString();
        info.append(
          element("strong", "", device.name || "未命名设备"),
          element("small", "", `到期：${expiryText} · 最近访问：${new Date(device.lastSeenAtMs).toLocaleString()}`)
        );
        const revokeButton = button("撤销", `撤销 ${device.name || "设备"} 授权`, () => void revokeDevice(device), "text-button remote-device-revoke");
        revokeButton.prepend(icon("trash"));
        revokeButton.disabled = busy;
        row.append(deviceIcon, info, revokeButton);
        devicesList.append(row);
      }
    }

    updatePolling();
  }

  render();
  void refresh();

  return () => {
    if (disposed) return;
    disposed = true;
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
    abortController.abort();
    root.replaceChildren();
  };
}
