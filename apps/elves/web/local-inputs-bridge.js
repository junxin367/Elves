(() => {
  if (window.top !== window) return;
  // 系统选择器和 CF_HDROP 只在独立客户端提供；浏览器拖放/粘贴另用本次操作授予的 File/目录对象。
  // 原生命令只处理系统选择器或 CF_HDROP，不接收任意本机路径、脚本或目标地址。
  Object.defineProperty(window, "__CODEX_ELVES_LOCAL_INPUTS__", {
    value: Object.freeze({
      pick: folders => window.__TAURI__.core.invoke("client_choose_inputs", { folders: folders === true }),
      // 新版直接打开同一个选择窗口；旧网页的文件/文件夹独立调用仍通过 pick 兼容。
      pickAny: () => window.__TAURI__.core.invoke("client_choose_inputs", {}),
      paste: () => window.__TAURI__.core.invoke("client_paste_inputs"),
      // 新页面使用本机暂存引用，不把文件内容通过 IPC 交给 WebView；旧缓存沿用原方法。
      pickStaged: () => window.__TAURI__.core.invoke("client_choose_inputs", { staged: true }),
      pasteStaged: stageImages => window.__TAURI__.core.invoke("client_paste_inputs", { staged: true, stageImages: stageImages === true })
    })
  });
})();
