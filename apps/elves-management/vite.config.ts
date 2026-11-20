import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";
import { defineConfig } from "vite";

export default defineConfig(({ mode }) => {
  const standaloneManager = mode === "manager";
  return {
  base: standaloneManager ? "./" : "/management/",
  build: {
    outDir: standaloneManager ? "dist/manager" : "../../assets/harness/management",
    emptyOutDir: true,
  },
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  };
});
