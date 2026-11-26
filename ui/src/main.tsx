import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { setVideoDecoder } from "./api";
import "./styles.css";
import { bootstrapAppearance } from "./theme";

// 主题与动效偏好在首屏就生效，避免启动后再闪一下。
bootstrapAppearance();

// 界面一启动就把 WebCodecs 能力告诉后端：之后每次握手（中转、直连、重连）
// 的 hello 里都会按它决定要不要声明 `video.h264`。直连会话在建会话前就
// 完成握手，等控制页加载再报就晚了。
void setVideoDecoder(
  typeof VideoDecoder !== "undefined" && typeof EncodedVideoChunk !== "undefined",
).catch(() => {});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
