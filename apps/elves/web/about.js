// 版本由原生菜单传入，只写入文本；确认按钮仅调用关于窗口自身的关闭命令。
const version = new URL(location.href).searchParams.get("version") || "未知";
document.getElementById("about-version").textContent = `版本 ${version.slice(0, 160)}`;
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
