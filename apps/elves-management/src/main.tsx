import { createRoot } from "react-dom/client";
import { App } from "./ManagementApp";

const app = document.getElementById("app");

if (app instanceof HTMLElement) {
  // 该前端只由独立 Elves 代理管理器的 Tauri 壳加载，不由 Harness 工作区嵌入。
  createRoot(app).render(<App />);
}
