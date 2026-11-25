// 测「点服务器管理 → 面板真正显示」的耗时。
// 用法: node cdp-serverpage-timing.js <ws-url>
// 逻辑：点击 nav「服务器管理」按钮后，页面 ServerPage 显示 loading（「正在启动服务器管理…」），
// 等 loading 消失（说明 showServerPanel 命令返回、子 webview 已经在显示）计时。

const url = process.argv[2];
if (!url) {
  console.error("缺少 CDP WebSocket 地址");
  process.exit(2);
}

const ws = new WebSocket(url);
let id = 0;
const pending = new Map();
const T0 = Date.now();
const t = () => Date.now() - T0;

function send(method, params = {}) {
  return new Promise((resolve, reject) => {
    const msgId = ++id;
    pending.set(msgId, { resolve, reject });
    ws.send(JSON.stringify({ id: msgId, method, params }));
  });
}

ws.addEventListener("message", (event) => {
  const msg = JSON.parse(String(event.data));
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
  }
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function evalJS(expression) {
  const r = await send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
  return r.result.value;
}

async function realClick(x, y) {
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
}

async function main() {
  await send("Page.enable");
  await send("Runtime.enable");

  // 回到屏幕墙（可能当前在别的页）
  await evalJS(`(() => {
    const b = Array.from(document.querySelectorAll('.nav-item')).find(x => x.textContent.trim() === '屏幕墙');
    b && b.click();
    return true;
  })()`);
  await sleep(400);

  const before = Date.now();
  await evalJS(`(() => {
    const b = Array.from(document.querySelectorAll('.nav-item')).find(x => x.textContent.trim() === '服务器管理');
    if (!b) return false;
    const r = b.getBoundingClientRect();
    window.__btn = { x: Math.round(r.x + r.width/2), y: Math.round(r.y + r.height/2) };
    return true;
  })()`);
  const btn = await evalJS(`window.__btn`);
  if (!btn) {
    console.log("找不到服务器管理按钮");
    process.exit(1);
  }

  // 记录点前状态
  const state0 = await evalJS(`(() => {
    const l = document.querySelector('.server-page-state');
    return { loading: l ? l.textContent.trim() : null, hasError: !!document.querySelector('.server-page-state.is-error') };
  })()`);

  await realClick(btn.x, btn.y);
  const tClick = Date.now();

  // 轮询：loading「正在启动服务器管理…」出现，然后消失（命令返回 + webview 可见）
  let sawLoading = false;
  let loadingMs = null;
  let goneMs = null;
  const deadline = tClick + 20000;
  while (Date.now() < deadline) {
    const s = await evalJS(`(() => {
      const l = document.querySelector('.server-page-state');
      return { loading: l ? l.textContent.trim() : null, hidden: !!document.querySelector('.page-enter.is-hidden .server-page') };
    })()`);
    if (s.loading && s.loading.includes("正在启动")) {
      if (!sawLoading) { sawLoading = true; loadingMs = Date.now() - tClick; }
    } else if (sawLoading && !s.loading) {
      goneMs = Date.now() - tClick;
      break;
    }
    if (!s.loading && !sawLoading && s.hidden === false) {
      // 没有 loading 直接出现（预启动生效太快，loading 一闪而过）
      // 不再等 loading，直接认为出现了
    }
    await sleep(10);
  }

  // 再确认面板进程里 webview 是否真的显示（等 300ms 内容渲染）
  await sleep(300);
  const later = await evalJS(`(() => {
    const l = document.querySelector('.server-page-state');
    return { loadingAfter: l ? l.textContent.trim() : null };
  })()`);

  console.log(JSON.stringify({
    before: state0,
    sawLoading,
    loadingMs: sawLoading ? loadingMs : null,
    goneMs,
    elapsedMaybeGone: goneMs ?? (Date.now() - tClick),
    later,
  }, null, 2));
  ws.close();
  process.exit(0);
}

ws.addEventListener("open", () => {
  main().catch((err) => {
    console.error("FAIL:", err.message);
    process.exit(1);
  });
});

ws.addEventListener("error", (e) => {
  console.error("WS 错误:", e.message ?? e.type);
  process.exit(1);
});