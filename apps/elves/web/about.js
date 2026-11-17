// 版本由原生菜单传入；只写入文本，不开放窗口、文件或安装权限。
const version = new URL(location.href).searchParams.get("version") || "未知";
document.getElementById("about-version").textContent = `版本 ${version.slice(0, 160)}`;
