// 通过 CDP Input 域做真实输入事件，测「点击侧栏 → 页面切换可见」的端到端延迟。
// 用法: node cdp-click-timing.js <ws-url>
// 输出每步: 点击前经 elapsed(ms), 切换后经 elapsed(ms), 差(ms), 目标隐藏类是否翻转。

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

// 页面内：读侧栏按钮坐标、当前页、切换前后时间戳。
async function evalJS(expression) {
  const r = await send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
  return r.result.value;
}

// 真实鼠标点击 (mousedown+mouseup+pressed) 触发 React onClick。
async function realClick(x, y) {
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
}

async function main() {
  await send("Page.enable");
  await send("Runtime.enable");

  // 让 longtask 走 PerformanceObserver 记录下来（时序差异我们自己在外部量，这里只管记录长任务）
  await evalJS(`
    window.__lts = [];
    new PerformanceObserver((list) => {
      list.getEntries().forEach((e) => window.__lts.push(Math.round(e.duration)));
    }).observe({ type: "longtask", buffered: true });
    true;
  `);

  // 拿到侧栏按钮的中心坐标：设置 / 群控 / 屏幕墙 / 文件传输
  const items = await evalJS(`
    (() => {
      const out = {};
      document.querySelectorAll(".nav-item").forEach((b) => {
        const r = b.getBoundingClientRect();
        out[b.textContent.trim().replace(/\\d+$/, "")] = { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) };
      });
      return out;
    })()
  `);
  console.log("侧栏按钮坐标:", JSON.stringify(items));

  // 每步: 读当前活动页 → 真实点击 → 等一小段 → 读隐藏类翻转后的活动页
  const currentPage = () =>
    evalJS(`(() => {
      const el = document.querySelector(".page-enter:not(.is-hidden) h1");
      return el ? el.textContent : "?";
    })()`);

  // h1 在 Topbar，不在 page-enter 内；页面切换的标志是「另一个 nav-item 变 is-active」。
  const readState = (label) =>
    evalJS(`(() => {
      const mark = ${Date.now() - T0};
      const activeNav = () => {
        const el = document.querySelector(".nav-item.is-active span");
        return el ? el.textContent.trim() : "?";
      };
      const hidden = document.querySelectorAll(".page-enter.is-hidden").length;
      const shown = document.querySelectorAll(".page-enter:not(.is-hidden)").length;
      const h1 = document.querySelector(".topbar h1")?.textContent ?? "";
      return { mark, activeNav: activeNav(), h1, hiddenCount: hidden, shownCount: shown };
    })()`);

  // 高频轮询检测切换：点击后每 8ms 读一次 nav-active，直到切换或超时。
  async function waitSwitch(coord, name, maxWait = 4000) {
    const before = await readState(`before-${name}`);
    await realClick(coord.x, coord.y);
    const tClick = Date.now();
    const deadline = tClick + maxWait;
    let last = before;
    while (Date.now() < deadline) {
      await sleep(8);
      const now = await readState(`poll-${name}`);
      if (now.activeNav !== before.activeNav || now.hiddenCount !== before.hiddenCount) {
        now.switchMs = now.mark - before.mark;
        return { before, after: now };
      }
      last = now;
    }
    return { before, after: last, timeout: true };
  }

  const targets = ["群控", "设置", "屏幕墙", "文件传输", "远程协助", "服务器管理"];
  for (const name of targets) {
    const coord = items[name];
    if (!coord) {
      console.log(`[${name}] 无坐标，跳过`);
      continue;
    }
    const { before, after, timeout } = await waitSwitch(coord, name);
    const changed =
      before.activeNav !== after.activeNav || before.hiddenCount !== after.hiddenCount;
    console.log(
      `[${name}] ${changed ? (timeout ? "TIMEOUT" : "OK") : "未切换"} | ` +
        `switchMs=${after.switchMs ?? "-"} | ${before.activeNav} → ${after.activeNav} ` +
        `(hidden ${before.hiddenCount}→${after.hiddenCount}, shown ${before.shownCount}→${after.shownCount})`,
    );
  }

  const lt = await evalJS(`window.__lts`);
  console.log("期间长任务(duration ms):", JSON.stringify(lt));
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