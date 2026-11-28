import type { CapacitorConfig } from "@capacitor/cli";

// 安卓壳：前端资源打包进 APK，页面源用 http://localhost。
// 用 http 而不是默认的 https：局域网里服务器多半是 http/ws，https 源会触发混内容拦截。
const config: CapacitorConfig = {
  appId: "com.screenwall.client",
  appName: "ScreenWall",
  webDir: "dist",
  server: {
    androidScheme: "http",
    cleartext: true,
  },
  android: {
    allowMixedContent: true,
  },
  plugins: {
    // 壳里的页面源是 `http://localhost`，直接 fetch 服务器属于跨域；交给原生层
    // 发请求可以绕开 WebView 的 CORS 与混内容限制（浏览器里的网页端不走这里）。
    CapacitorHttp: { enabled: true },
  },
};

export default config;
