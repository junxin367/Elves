const form = document.querySelector("#connection-form");
const server = document.querySelector("#server-url");
const connect = document.querySelector("#connect");
const local = document.querySelector("#start-local");
const status = document.querySelector("#status");
const invoke = (command, payload = {}) => window.__TAURI__.core.invoke(command, payload);
let connecting = false;
let edited = false;
server.addEventListener("input", () => { edited = true; });

async function openConnection() {
  if (connecting || !server.value.trim()) return;
  connecting = true;
  connect.disabled = true;
  if (local) local.disabled = true;
  status.textContent = "正在打开远程服务…";
  status.classList.remove("error");
  try {
    // 连接页只提交服务首页；配对密钥和设备授权仍由原远程页面处理，不传入原生配置。
    await invoke("connect_remote", { serverUrl: server.value.trim() });
  } catch (error) {
    status.textContent = String(error);
    status.classList.add("error");
    server.focus();
  } finally {
    connecting = false;
    connect.disabled = false;
    if (local) local.disabled = false;
  }
}
form.addEventListener("submit", event => {
  event.preventDefault();
  void openConnection();
});
async function startLocalConnection() {
  if (connecting) return;
  connecting = true;
  if (local) local.disabled = true; connect.disabled = true;
  status.textContent = "正在启动本机服务…"; status.classList.remove("error");
  let started = false;
  try {
    server.value = await invoke("remote_start_local_service");
    edited = true; started = true;
  } catch (error) {
    status.textContent = `本机服务启动失败：${error}`; status.classList.add("error");
  } finally {
    connecting = false; if (local) local.disabled = false; connect.disabled = false;
  }
  if (started) await openConnection();
}
local?.addEventListener("click", () => void startLocalConnection());

try {
  const settings = await invoke("remote_client_state");
  // 本地探测异步返回时不能覆盖用户已经开始编辑的地址。
  if (!edited) server.value = settings.serverUrl;
  document.querySelector("#version").textContent = `v${settings.version}`;
  if (!connecting) {
    status.textContent = (!edited && settings.error) || (!edited && settings.detected
      ? "已发现本机远程服务，可直接连接。" : "连接后将使用此客户端独立保存的登录状态。");
    status.classList.toggle("error", !edited && Boolean(settings.error));
    connect.disabled = false;
  }
  if (settings.error && !edited) {
    // 地址错误后直接选中原输入，便于立即修正，不再次自动进入错误页面。
    server.focus();
    server.select();
  }
  // 用户要求独立程序直接打开会话；仅首次启动自动恢复已成功连接的地址。
  if (!edited && !settings.error && settings.autoConnect && settings.serverUrl) {
    if (settings.localService) void startLocalConnection();
    else void openConnection();
  }
} catch (error) {
  status.textContent = `读取设置失败：${error}`;
  status.classList.add("error");
}
