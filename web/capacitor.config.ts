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
};

export default config;
