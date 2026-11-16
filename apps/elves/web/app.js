const status = document.querySelector("#status");
const retry = document.querySelector("#retry");
const invoke = (command) => window.__TAURI__.core.invoke(command);
let pending = false;
async function refresh() {
  try {
    const state = await invoke("client_startup_state");
    document.querySelector("#version").textContent = `v${state.version}`;
    status.textContent = state.error || "正在打开 Elves 工作区…";
    status.classList.toggle("error", Boolean(state.error));
    retry.hidden = !state.error;
    retry.disabled = state.starting || pending;
  } catch (error) { status.textContent = String(error); status.classList.add("error"); }
}
retry.addEventListener("click", async () => {
  if (pending) return;
  pending = true; retry.disabled = true; status.textContent = "正在启动 Harness…";
  try { await invoke("client_retry_startup"); } catch (error) { status.textContent = String(error); }
  finally { pending = false; void refresh(); }
});
void refresh();
const timer = setInterval(() => void refresh(), 1000);
window.addEventListener("pagehide", () => clearInterval(timer), {once:true});
