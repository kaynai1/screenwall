// 通过 WebView2 的 CDP 端点执行一段 JS，用于开发期验收。
// 用法: node cdp-eval.js <ws-url> "<expression>"

const url = process.argv[2];
const expression = process.argv[3] || "document.body.innerText";

if (!url) {
  console.error("缺少 CDP WebSocket 地址");
  process.exit(2);
}

const socket = new WebSocket(url);
const id = 1;
const timer = setTimeout(() => {
  console.error("CDP 超时");
  process.exit(1);
}, 20000);

socket.addEventListener("open", () => {
  socket.send(
    JSON.stringify({
      id,
      method: "Runtime.evaluate",
      params: { expression, returnByValue: true, awaitPromise: true },
    }),
  );
});

socket.addEventListener("message", (event) => {
  const message = JSON.parse(String(event.data));
  if (message.id !== id) return;
  console.log(JSON.stringify(message.result, null, 2));
  clearTimeout(timer);
  socket.close();
});

socket.addEventListener("error", (event) => {
  console.error("CDP 连接失败:", event.message ?? event.type);
  clearTimeout(timer);
  process.exit(1);
});
