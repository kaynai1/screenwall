// M3 验收脚本：在控制页触发鼠标/键盘事件，检查被控端（本机）真实光标和按键状态。
// 远程光标现在由被控端合成进视频帧，前端无法直接断言画面里的光标像素；
// 脚本改为断言控制页没有本地光标叠加层，验证「画面上只有一层光标、不会重影」。
// 前提：客户端已用 CDP 端口启动，且已登录、控制页处于实时画面、正在看主屏。
// 用法: node scripts/verify-m3.js
// 成功退出码 0，失败退出码 1。

"use strict";

const http = require("node:http");
const { execFileSync } = require("node:child_process");

const CDP_HOST = process.env.CDP_HOST || "127.0.0.1";
const CDP_PORT = Number(process.env.CDP_PORT || 9222);
const DISCOVERY_TIMEOUT_MS = 5000;
const WS_OPEN_TIMEOUT_MS = 5000;
const CDP_COMMAND_TIMEOUT_MS = 15000;

function httpGetJson(urlPath, timeoutMs) {
  return new Promise((resolve, reject) => {
    const req = http.get(
      { host: CDP_HOST, port: CDP_PORT, path: urlPath, timeout: timeoutMs },
      (res) => {
        let data = "";
        res.setEncoding("utf8");
        res.on("data", (chunk) => {
          data += chunk;
        });
        res.on("end", () => {
          if (res.statusCode !== 200) {
            reject(new Error(`${urlPath} 返回 HTTP ${res.statusCode}`));
            return;
          }
          try {
            resolve(JSON.parse(data));
          } catch (error) {
            reject(new Error(`无法解析 ${urlPath} 响应: ${error.message}`));
          }
        });
      },
    );
    req.on("timeout", () => {
      req.destroy(new Error(`请求 ${urlPath} 超时`));
    });
    req.on("error", reject);
  });
}

function findPageTarget(targets) {
  const pages = (targets || []).filter(
    (target) => target && target.type === "page" && target.webSocketDebuggerUrl,
  );
  const match = pages.find((target) => {
    const url = String(target.url || "");
    return (
      /screenwall/i.test(String(target.title || "")) ||
      url.indexOf("localhost:1420") >= 0 ||
      url.indexOf("127.0.0.1:1420") >= 0 ||
      url.indexOf("tauri://localhost") >= 0
    );
  });
  if (!match) throw new Error("未找到 ScreenWall 页面 target");
  return match;
}

function createCdpClient(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    const state = { closed: false, nextId: 0 };
    let settled = false;

    const openTimer = setTimeout(() => {
      if (settled) return;
      settled = true;
      try {
        ws.close();
      } catch (error) {}
      reject(new Error("CDP WebSocket 连接超时"));
    }, WS_OPEN_TIMEOUT_MS);

    ws.addEventListener("open", () => {
      clearTimeout(openTimer);
      if (settled) return;
      settled = true;
      resolve({
        send(method, params = {}) {
          return new Promise((res, rej) => {
            if (state.closed) {
              rej(new Error("CDP 连接已关闭"));
              return;
            }
            const id = (state.nextId += 1);
            const timer = setTimeout(() => {
              pending.delete(id);
              rej(new Error(`CDP ${method} 超时`));
            }, CDP_COMMAND_TIMEOUT_MS);
            pending.set(id, { resolve: res, reject: rej, timer });
            ws.send(JSON.stringify({ id, method, params }));
          });
        },
        close() {
          try {
            ws.close();
          } catch (error) {}
        },
      });
    });

    ws.addEventListener("message", (event) => {
      let message;
      try {
        message = JSON.parse(String(event.data));
      } catch (error) {
        return;
      }
      if (typeof message.id === "number") {
        const entry = pending.get(message.id);
        if (!entry) return;
        pending.delete(message.id);
        clearTimeout(entry.timer);
        if (message.error) entry.reject(new Error(message.error.message || "CDP 错误"));
        else entry.resolve(message.result || {});
      }
    });

    ws.addEventListener("error", (event) => {
      clearTimeout(openTimer);
      if (settled) return;
      settled = true;
      reject(new Error(event.message || "CDP 连接失败"));
    });

    ws.addEventListener("close", () => {
      state.closed = true;
      for (const [, entry] of pending) {
        clearTimeout(entry.timer);
        entry.reject(new Error("CDP 连接已关闭"));
      }
      pending.clear();
    });
  });
}

function evaluate(cdp, expression) {
  return cdp
    .send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    })
    .then((result) => {
      if (result.exceptionDetails) {
        throw new Error(
          (result.exceptionDetails.exception &&
            result.exceptionDetails.exception.description) ||
            result.exceptionDetails.text ||
            "页面脚本执行失败",
        );
      }
      return result.result ? result.result.value : undefined;
    });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function powershell(script) {
  const output = execFileSync(
    "powershell",
    ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", timeout: 15000 },
  );
  return output.trim();
}

// 预期落点按主屏（display 0）计算：被控端默认采集主显示器，输入坐标也按它映射。
// 如果在控制页切到了副屏或「全部屏幕」，input_region 会变，这里的期望值不再成立，
// 需要先切回主显示器（默认选项）再跑脚本。
function getScreenInfo() {
  const out = powershell(
    'Add-Type -AssemblyName System.Windows.Forms; ' +
      '$b=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; ' +
      '"$($b.X),$($b.Y),$($b.Width),$($b.Height)"',
  );
  const [x, y, width, height] = out.split(",").map(Number);
  return { x, y, width, height };
}

function getCursor() {
  const out = powershell(
    'Add-Type -AssemblyName System.Windows.Forms; ' +
      '$p=[System.Windows.Forms.Cursor]::Position; "$($p.X),$($p.Y)"',
  );
  const [x, y] = out.split(",").map(Number);
  return { x, y };
}

function isKeyDown(vk) {
  const out = powershell(
    "Add-Type -TypeDefinition 'using System;using System.Runtime.InteropServices;" +
      "public class KeyState{[DllImport(\"user32.dll\")]public static extern short GetAsyncKeyState(int vKey);}'; " +
      `if (([KeyState]::GetAsyncKeyState(${vk}) -band 0x8000) -ne 0) { 'down' } else { 'up' }`,
  );
  return out === "down";
}

// 在页面里找到输入层，并按容器的归一化位置派发合成事件。
function dispatchMove(ratioX, ratioY) {
  return `(() => {
    const layer = document.querySelector(".ctrl-input-layer");
    if (!layer) return { ok: false, reason: "no-input-layer" };
    const rect = layer.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return { ok: false, reason: "empty-layer" };
    const clientX = rect.left + rect.width * ${ratioX};
    const clientY = rect.top + rect.height * ${ratioY};
    layer.dispatchEvent(new MouseEvent("mousemove", {
      clientX, clientY, bubbles: true, cancelable: true, view: window,
    }));
    // 本机自测时，被控端注入会移动同一个物理光标，光标落到捕获层上又会
    // 触发真实的 mousemove，形成回授。测量期间临时关掉命中测试，避免这个
    // 只在本机自测中存在的回路干扰坐标校验。
    layer.style.pointerEvents = "none";

    // 页面可能处于「铺满」或「适应」，期望值按同样的换算规则推导。
    const video = document.querySelector(".ctrl-video");
    const fit = video ? getComputedStyle(video).objectFit : "contain";
    const frameW = video ? video.naturalWidth : 0;
    const frameH = video ? video.naturalHeight : 0;
    let absX = ${ratioX};
    let absY = ${ratioY};
    if (frameW > 0 && frameH > 0) {
      const scale = fit === "cover"
        ? Math.max(rect.width / frameW, rect.height / frameH)
        : Math.min(rect.width / frameW, rect.height / frameH);
      const displayW = frameW * scale;
      const displayH = frameH * scale;
      const offsetX = (rect.width - displayW) / 2;
      const offsetY = (rect.height - displayH) / 2;
      absX = (clientX - rect.left - offsetX) / displayW;
      absY = (clientY - rect.top - offsetY) / displayH;
    }
    return {
      ok: true,
      fit,
      abs: {
        x: Math.min(1, Math.max(0, absX)),
        y: Math.min(1, Math.max(0, absY)),
      },
      rect: { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
      clientX, clientY,
    };
  })()`;
}

function dispatchKey(type, code, key) {
  return `(() => {
    const layer = document.querySelector(".ctrl-input-layer");
    if (!layer) return { ok: false, reason: "no-input-layer" };
    layer.focus();
    const event = new KeyboardEvent(${JSON.stringify(type)}, {
      code: ${JSON.stringify(code)},
      key: ${JSON.stringify(key)},
      bubbles: true,
      cancelable: true,
    });
    layer.dispatchEvent(event);
    return { ok: true, focused: document.activeElement === layer, prevented: event.defaultPrevented };
  })()`;
}

function restoreLayerHitTest(cdp) {
  return evaluate(
    cdp,
    `(() => {
      const layer = document.querySelector(".ctrl-input-layer");
      if (layer) layer.style.pointerEvents = "";
      return !!layer;
    })()`,
  ).catch(() => false);
}

async function readPageState(cdp) {
  return evaluate(
    cdp,
    `(() => {
      const layer = document.querySelector(".ctrl-input-layer");
      const hint = document.querySelector(".ctrl-input-badge, .ctrl-input-hint");
      const toggles = Array.from(document.querySelectorAll(".ctrl-dock-btn.is-on")).map(
        (el) => el.getAttribute("aria-label") || el.title || "",
      );
      const img = document.querySelector("img.ctrl-live-frame");
      return {
        live: !!img && !!img.getAttribute("src") && img.getAttribute("src").indexOf("data:image/jpeg") === 0,
        layer: !!layer,
        hint: hint ? hint.textContent.replace(/\\s+/g, " ").trim() : null,
        toggles,
        error: document.querySelector(".ctrl-stream-error p")
          ? document.querySelector(".ctrl-stream-error p").textContent
          : null,
      };
    })()`,
  );
}

async function main() {
  if (typeof WebSocket !== "function") {
    console.error("[verify-m3] 需要 Node 22 或更高版本（内置 WebSocket）");
    return 1;
  }

  let cdp = null;
  try {
    const targets = await httpGetJson("/json", DISCOVERY_TIMEOUT_MS);
    const target = findPageTarget(targets);
    cdp = await createCdpClient(target.webSocketDebuggerUrl);
    await cdp.send("Runtime.enable");

    const state = await readPageState(cdp);
    console.log(
      `[verify-m3] page: live=${state.live} | inputLayer=${state.layer} | hint=${state.hint || "—"} | toggles=${state.toggles.join(",") || "none"}`,
    );
    if (!state.live) {
      console.log(`[verify-m3] 控制页不在实时画面${state.error ? `（${state.error}）` : ""}`);
      return 1;
    }
    if (!state.layer) {
      console.log("[verify-m3] 找不到 .ctrl-input-layer，键鼠接管层未渲染");
      return 1;
    }

    const screen = getScreenInfo();
    const before = getCursor();
    console.log(
      `[verify-m3] screen=${screen.width}x${screen.height} | cursor before=${before.x},${before.y}`,
    );

    // 先检查鼠标开关是否打开；默认应打开。
    if (!state.toggles.some((label) => label.indexOf("鼠标") >= 0)) {
      console.log("[verify-m3] 鼠标控制开关未开启，尝试点击");
      await evaluate(
        cdp,
        `(() => {
          const btn = Array.from(document.querySelectorAll(".ctrl-dock-btn")).find(
            (el) => (el.getAttribute("aria-label") || el.title || "").indexOf("鼠标") >= 0,
          );
          if (btn) { btn.click(); return true; }
          return false;
        })()`,
      );
      await sleep(200);
    }
    if (!state.toggles.some((label) => label.indexOf("键盘") >= 0)) {
      console.log("[verify-m3] 键盘控制开关未开启，尝试点击");
      await evaluate(
        cdp,
        `(() => {
          const btn = Array.from(document.querySelectorAll(".ctrl-dock-btn")).find(
            (el) => (el.getAttribute("aria-label") || el.title || "").indexOf("键盘") >= 0,
          );
          if (btn) { btn.click(); return true; }
          return false;
        })()`,
      );
      await sleep(200);
    }

    const center = await evaluate(cdp, dispatchMove(0.5, 0.5));
    await sleep(500);
    const centerCursor = getCursor();
    const expectedCenter = {
      x: screen.x + Math.round(screen.width * center.abs.x),
      y: screen.y + Math.round(screen.height * center.abs.y),
    };
    const centerDx = Math.abs(centerCursor.x - expectedCenter.x);
    const centerDy = Math.abs(centerCursor.y - expectedCenter.y);
    console.log(
      `[verify-m3] move 50%/50%: cursor=${centerCursor.x},${centerCursor.y} expected≈${expectedCenter.x},${expectedCenter.y} drift=${centerDx},${centerDy}`,
    );

    // 第 5 项：光标层检查。
    // 光标现在由被控端合成进视频帧（GDI 走 GetCursorInfo + DrawIconEx，DXGI 走
    // GetFramePointerShape），前端 DOM 里看不到它，页面无法断言「光标画出来了」。
    // 这里验证的是相反的一面：画面之上不再有本地光标叠加层，因此不会出现两层光标重影。
    // 判定按「舞台内任何 class 或 id 含 cursor 的元素」来查，将来改名重新加回来也能抓到。
    const cursorOverlay = await evaluate(
      cdp,
      `(() => {
        const stage = document.querySelector(".ctrl-screen");
        const legacy = document.querySelector(".ctrl-remote-cursor");
        const describe = (el) =>
          el.getAttribute("class") || el.getAttribute("id") || el.tagName;
        const suspects = stage
          ? Array.from(stage.querySelectorAll("*"))
              .filter((el) => {
                const cls = el.getAttribute("class") || "";
                const id = el.getAttribute("id") || "";
                return /cursor/i.test(cls) || /cursor/i.test(id);
              })
              .map(describe)
          : [];
        const video = document.querySelector("img.ctrl-live-frame");
        return {
          stage: !!stage,
          // 帧已解码：光标由被控端画在这张画面里，不是 DOM 元素。
          frameReady: !!video && video.naturalWidth > 0 && video.naturalHeight > 0,
          legacy: !!legacy,
          suspects,
        };
      })()`,
    );
    // cursorOk 只看「本地没有叠加层」：视频帧是否解码好由前面的 live 检查和日志负责，
    // 这里不把它算进判定，避免首帧解码慢时误报 FAIL。
    const cursorOk =
      cursorOverlay.stage &&
      !cursorOverlay.legacy &&
      cursorOverlay.suspects.length === 0;
    console.log(
      `[verify-m3] cursor overlay: stage=${cursorOverlay.stage} frameReady=${cursorOverlay.frameReady} localOverlay=${
        cursorOverlay.legacy || cursorOverlay.suspects.length > 0 ? "found" : "none"
      }${cursorOverlay.suspects.length ? ` (${cursorOverlay.suspects.join(", ")})` : ""} — 光标应只存在于视频帧内`,
    );

    const right = await evaluate(cdp, dispatchMove(0.75, 0.5));
    await sleep(500);
    const rightCursor = getCursor();
    const expectedRight = {
      x: screen.x + Math.round(screen.width * right.abs.x),
      y: screen.y + Math.round(screen.height * right.abs.y),
    };
    const rightDx = Math.abs(rightCursor.x - expectedRight.x);
    const rightDy = Math.abs(rightCursor.y - expectedRight.y);
    console.log(
      `[verify-m3] move 75%/50%: cursor=${rightCursor.x},${rightCursor.y} expected≈${expectedRight.x},${expectedRight.y} drift=${rightDx},${rightDy}`,
    );

    await restoreLayerHitTest(cdp);

    const keyDownResult = await evaluate(cdp, dispatchKey("keydown", "KeyA", "a"));
    await sleep(400);
    const down = isKeyDown(0x41);
    const keyUpResult = await evaluate(cdp, dispatchKey("keyup", "KeyA", "a"));
    await sleep(400);
    const up = isKeyDown(0x41);
    console.log(
      `[verify-m3] key A: downSent=${!!keyDownResult.ok} pressed=${down} | upSent=${!!keyUpResult.ok} released=${!up}`,
    );

    const escDown = await evaluate(cdp, dispatchKey("keydown", "Escape", "Escape"));
    await sleep(400);
    const escPressed = isKeyDown(0x1b);
    await evaluate(cdp, dispatchKey("keyup", "Escape", "Escape"));
    await sleep(400);
    const escReleased = !isKeyDown(0x1b);
    console.log(
      `[verify-m3] key Esc: downSent=${!!escDown.ok} pressed=${escPressed} | upSent=true released=${escReleased}`,
    );

    const driftOk = centerDx <= 4 && centerDy <= 4 && rightDx <= 4 && rightDy <= 4;
    const keyOk = down && !up;
    const escOk = escPressed && escReleased;
    const passed = driftOk && keyOk && escOk && cursorOk;
    console.log(
      `[verify-m3] checks: pointer=${driftOk ? "OK" : "FAIL"} | keyboard=${keyOk ? "OK" : "FAIL"} | esc=${escOk ? "OK" : "FAIL"} | cursor-overlay=${cursorOk ? "OK" : "FAIL"}`,
    );
    console.log(`[verify-m3] RESULT: ${passed ? "PASS" : "FAIL"}`);
    return passed ? 0 : 1;
  } catch (error) {
    console.error(`[verify-m3] FAIL: ${error.message}`);
    return 1;
  } finally {
    if (cdp) {
      await restoreLayerHitTest(cdp);
      cdp.close();
    }
  }
}

main()
  .then((code) => {
    process.exitCode = code;
  })
  .catch((error) => {
    console.error(`[verify-m3] FAIL: ${(error && error.stack) || error}`);
    process.exitCode = 1;
  });
