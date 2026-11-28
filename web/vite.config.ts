import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// 网页端由账号服务器挂在 `/app` 下提供，构建产物里的资源路径都带这个前缀。
// 安卓壳（`vite build --mode capacitor`）里资源跟着 APK 走，用相对路径。
// 开发时把 `/api` 与 `/ws` 代理到本机服务器（`screenwall-server run`）。
export default defineConfig(({ mode }) => ({
  base: mode === "capacitor" ? "./" : "/app/",
  plugins: [react()],
  server: {
    port: 1421,
    proxy: {
      "/api": "http://127.0.0.1:8787",
      "/ws": { target: "ws://127.0.0.1:8787", ws: true },
    },
  },
  build: {
    target: "es2022",
  },
}));
