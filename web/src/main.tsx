import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

// 不套 StrictMode：它会在开发模式双挂载，控制页的副作用会重复建会话。
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);

// 离线时也至少能打开界面（真正的控制当然要联网）。
// 安卓壳（Capacitor）里的资源来自 APK 自己，不注册 Service Worker。
const nativeShell = Boolean(
  (window as Window & { Capacitor?: { isNativePlatform?: () => boolean } }).Capacitor?.isNativePlatform?.(),
);
if ("serviceWorker" in navigator && import.meta.env.PROD && !nativeShell) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/app/sw.js").catch(() => {});
  });
}
