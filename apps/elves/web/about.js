// 程序版本和网页资源版本都由原生菜单传入；确认按钮仅调用关于窗口自身的关闭命令。
const params = new URL(location.href).searchParams;
const version = params.get("version") || "未知";
const resourceRevision = params.get("resourceRevision") || "未知";
document.getElementById("about-version").textContent = `程序版本 ${version.slice(0, 160)}`;
document.getElementById("about-resource-revision").textContent =
  `资源版本 ${resourceRevision.slice(0, 32)}`;
const confirm = document.getElementById("about-confirm");
confirm.addEventListener("click", async () => {
  if (confirm.disabled) return;
  confirm.disabled = true;
  try {
    await window.__TAURI__.core.invoke("client_close_about");
  } catch (error) {
    const message = document.getElementById("about-error");
    message.textContent = `关闭失败：${String(error)}`;
    message.hidden = false;
    confirm.disabled = false;
  }
});
