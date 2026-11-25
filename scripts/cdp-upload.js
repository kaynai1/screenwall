// 通过 CDP 给页面的文件选择框塞入本地文件，用于开发期验收上传流程。
// 用法: node cdp-upload.js <ws-url> <css-selector> <绝对文件路径>

const [url, selector, file] = process.argv.slice(2);
if (!url || !selector || !file) {
  console.error("用法: node cdp-upload.js <ws-url> <selector> <file>");
  process.exit(2);
}

const socket = new WebSocket(url);
let nextId = 1;
const pending = new Map();

function send(method, params = {}) {
  const id = nextId++;
  socket.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

const timer = setTimeout(() => {
  console.error("CDP 超时");
  process.exit(1);
}, 20000);

socket.addEventListener("message", (event) => {
  const message = JSON.parse(String(event.data));
  const entry = pending.get(message.id);
  if (!entry) return;
  pending.delete(message.id);
  if (message.error) entry.reject(new Error(message.error.message));
  else entry.resolve(message.result);
});

socket.addEventListener("error", (event) => {
  console.error("CDP 连接失败:", event.message ?? event.type);
  process.exit(1);
});

socket.addEventListener("open", async () => {
  try {
    await send("DOM.enable");
    const { root } = await send("DOM.getDocument", { depth: -1, pierce: true });
    const { nodeId } = await send("DOM.querySelector", {
      nodeId: root.nodeId,
      selector,
    });
    if (!nodeId) throw new Error(`找不到元素: ${selector}`);
    await send("DOM.setFileInputFiles", { nodeId, files: [file] });
    console.log("已在", selector, "中设置文件", file);
  } catch (error) {
    console.error(String(error));
    process.exitCode = 1;
  } finally {
    clearTimeout(timer);
    socket.close();
  }
});
