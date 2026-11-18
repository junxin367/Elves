(() => {
  if (window.top !== window || window.__ELVES_HARNESS_DIAGNOSTICS__) return;

  const snapshot = async (options = {}) => {
    const threadId = typeof options.threadId === "string" && options.threadId.length > 0
      ? options.threadId : null;
    const response = await fetch("/api/read/harnessDiagnostics", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(threadId ? { threadId } : {})
    });
    const value = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(value.message || `Harness 诊断读取失败（HTTP ${response.status}）`);
    }
    window.__ELVES_HARNESS_LAST_DIAGNOSTICS__ = value;
    if (options.log !== false) {
      console.info("[Elves] Harness 诊断快照", value);
    }
    return value;
  };

  Object.defineProperty(window, "__ELVES_HARNESS_DIAGNOSTICS__", {
    configurable: false,
    enumerable: false,
    value: snapshot,
    writable: false
  });
  console.info(
    "[Elves] 本机 Harness 诊断：await window.__ELVES_HARNESS_DIAGNOSTICS__({threadId: '<会话ID>'})"
  );
})();
