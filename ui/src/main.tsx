import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";
import { bootstrapAppearance } from "./theme";

// 主题与动效偏好在首屏就生效，避免启动后再闪一下。
bootstrapAppearance();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
