// 通过 WebView2 的 CDP 端点截取应用界面。
// 用法: node cdp-shot.js <ws-url> <输出文件.png>

const fs = require("node:fs");

const url = process.argv[2];
const output = process.argv[3];
if (!url || !output) {
  console.error("用法: node cdp-shot.js <ws-url> <输出文件.png>");
  process.exit(2);
}

const socket = new WebSocket(url);
const id = 1;
const timer = setTimeout(() => {
  console.error("CDP 超时");
  process.exit(1);
}, 5000);

socket.addEventListener("open", () => {
  socket.send(JSON.stringify({ id, method: "Page.captureScreenshot", params: { format: "png" } }));
});

socket.addEventListener("message", (event) => {
  const message = JSON.parse(String(event.data));
  if (message.id !== id) return;
  if (message.error) {
    console.error(JSON.stringify(message.error));
    process.exit(1);
  }
  fs.writeFileSync(output, Buffer.from(message.result.data, "base64"));
  console.log(output);
  clearTimeout(timer);
  socket.close();
});

socket.addEventListener("error", (event) => {
  console.error("CDP 连接失败:", event.message ?? event.type);
  clearTimeout(timer);
  process.exit(1);
});
