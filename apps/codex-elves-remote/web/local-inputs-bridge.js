(() => {
  if (window.top !== window) return;
  // 系统选择器和 CF_HDROP 只在独立客户端提供；浏览器拖放/粘贴另用本次操作授予的 File/目录对象。
  // 原生命令只处理系统选择器或 CF_HDROP，不接收任意本机路径、脚本或目标地址。
  Object.defineProperty(window, "__CODEX_ELVES_LOCAL_INPUTS__", {
    value: Object.freeze({
      pick: folders => window.__TAURI__.core.invoke("remote_choose_inputs", { folders: folders === true }),
      paste: () => window.__TAURI__.core.invoke("remote_paste_inputs")
    })
  });
})();
