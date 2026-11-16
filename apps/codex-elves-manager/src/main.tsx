import { createRoot } from "react-dom/client";
import { App } from "./ManagementApp";

const app = document.getElementById("app");

if (app instanceof HTMLElement) {
  // 原管理器不再提供独立入口；该前端只作为统一远程导航中的同源管理页面使用。
  createRoot(app).render(<App />);
}
