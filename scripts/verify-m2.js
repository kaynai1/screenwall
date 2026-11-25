// M2 验收脚本：从屏幕墙点进第一台在线设备的控制页，等待真实 MJPEG 首帧并截图。
// 依赖 Tauri 调试时开放的 WebView2 CDP 端口（默认 127.0.0.1:9222）。
// 用法: node scripts/verify-m2.js
// 可选环境变量: CDP_PORT（默认 9222）、CDP_HOST（默认 127.0.0.1）
// 成功退出码 0，失败退出码 1。

"use strict";

const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const CDP_HOST = process.env.CDP_HOST || "127.0.0.1";
const CDP_PORT = Number(process.env.CDP_PORT || 9222);
const DISCOVERY_TIMEOUT_MS = 5000;
const WS_OPEN_TIMEOUT_MS = 5000;
const CDP_COMMAND_TIMEOUT_MS = 36000;
const OVERALL_TIMEOUT_MS = 60000;
const SCREENSHOT_PATH = path.resolve(
  __dirname,
  "..",
  "ui",
  "screenshots",
  "m2-control.png",
);

// 这段函数会在页面里通过 Runtime.evaluate 执行，必须完全自包含。
// 判定实时帧：src 以 data:image/jpeg;base64, 开头，且长度足够（排除占位小图）。
async function verifyInPage() {
  const startedAt = Date.now();
  const JPEG_PREFIX = "data:image/jpeg;base64,";
  const MIN_FRAME_DATA_URL_LENGTH = 4096;
  const FRAME_WAIT_MS = 20000;
  const STARTUP_WAIT_MS = 5000;
  const NAV_WAIT_MS = 5000;

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const textOf = (el) =>
    el && el.textContent ? el.textContent.replace(/\s+/g, " ").trim() : "";
  const all = (selector) => Array.prototype.slice.call(document.querySelectorAll(selector));

  function getFrame() {
    const img = document.querySelector("img.ctrl-live-frame");
    if (!img) {
      return {
        found: false,
        valid: false,
        srcPrefix: "",
        dataUrlLength: 0,
        complete: false,
        naturalWidth: 0,
        naturalHeight: 0,
      };
    }
    const src = img.getAttribute("src") || "";
    return {
      found: true,
      valid: src.indexOf(JPEG_PREFIX) === 0 && src.length >= MIN_FRAME_DATA_URL_LENGTH,
      srcPrefix: src.slice(0, JPEG_PREFIX.length),
      dataUrlLength: src.length,
      complete: !!img.complete,
      naturalWidth: img.naturalWidth || 0,
      naturalHeight: img.naturalHeight || 0,
    };
  }

  function onControlPage() {
    return !!document.querySelector(".ctrl-page");
  }
  function onWallPage() {
    return !!document.querySelector(".wall");
  }
  function hasStreamError() {
    return !!document.querySelector(".ctrl-stream-error");
  }
  function hasOffline() {
    return !!document.querySelector(".ctrl-offline");
  }

  function detectPhase() {
    if (!onControlPage()) {
      if (onWallPage()) return { phase: "wall", label: "屏幕墙" };
      if (document.querySelector(".splash-screen")) {
        return { phase: "booting", label: "启动中" };
      }
      if (document.querySelector(".login-screen")) {
        return { phase: "login", label: "登录页" };
      }
      const heading = textOf(document.querySelector(".page-head h2")) || "非屏幕墙页面";
      return { phase: "other", label: heading };
    }
    if (hasOffline()) return { phase: "control-offline", label: "设备离线" };
    if (hasStreamError()) return { phase: "control-error", label: "画面接入失败" };

    const frame = getFrame();
    if (frame.valid && frame.naturalWidth > 0) {
      return {
        phase: "control-live",
        label: textOf(document.querySelector(".ctrl-stage-badge")) || "实时画面",
      };
    }
    const connecting = document.querySelector(".ctrl-connecting");
    if (connecting) {
      const label = textOf(connecting.querySelector("strong"));
      return {
        phase:
          label.indexOf("等待首帧") >= 0 || label.indexOf("获取实时画面") >= 0
            ? "control-waiting"
            : "control-connecting",
        label: label || "连接中",
      };
    }
    if (frame.found) return { phase: "control-frame-pending", label: "首帧加载中" };
    return {
      phase: "control-idle",
      label: textOf(document.querySelector(".ctrl-stage-badge")) || "控制页",
    };
  }

  function collectStats() {
    const toolbarItems = all(".ctrl-q-item");
    const itemValue = (label) => {
      for (const item of toolbarItems) {
        if (textOf(item.querySelector("span")) === label) {
          return textOf(item.querySelector("strong")) || null;
        }
      }
      return null;
    };
    const rows = all(".ctrl-info-list > div");
    const rowValue = (label) => {
      for (const row of rows) {
        if (textOf(row.querySelector("dt")) === label) {
          return textOf(row.querySelector("dd")) || null;
        }
      }
      return null;
    };
    return {
      fps: itemValue("FPS") || rowValue("帧率"),
      resolution: rowValue("分辨率"),
      latency: itemValue("延迟") || rowValue("延迟"),
      bandwidth: itemValue("码率") || rowValue("带宽"),
      quality: textOf(document.querySelector(".ctrl-quality-copy strong")) || null,
      stageBadge: textOf(document.querySelector(".ctrl-stage-badge")) || null,
    };
  }

  function collectTexts() {
    const bodyText = document.body ? document.body.innerText || "" : "";
    return {
      pictureFailed: bodyText.indexOf("画面接入失败") >= 0,
      deviceOffline: bodyText.indexOf("设备离线") >= 0,
      sessionClosed: bodyText.indexOf("远程会话已断开") >= 0,
      waitingFirstFrame: bodyText.indexOf("等待首帧") >= 0,
      streamErrorMessage: textOf(document.querySelector(".ctrl-stream-error p")) || null,
      offlineDetail: textOf(document.querySelector(".ctrl-offline p")) || null,
    };
  }

  function readVerifyErrors() {
    const exists = Object.prototype.hasOwnProperty.call(window, "__verifyErrors");
    const value = exists ? window.__verifyErrors : undefined;
    let length = 0;
    if (Array.isArray(value)) length = value.length;
    else if (value && typeof value.length === "number") length = value.length;

    const samples = [];
    if (Array.isArray(value)) {
      for (let i = 0; i < value.length && i < 5; i += 1) {
        try {
          const item = value[i];
          samples.push(
            typeof item === "string"
              ? item.slice(0, 200)
              : String(JSON.stringify(item)).slice(0, 200),
          );
        } catch (error) {
          samples.push(String(value[i]).slice(0, 200));
        }
      }
    }
    return {
      exists,
      type: exists && value === null ? "null" : typeof value,
      length,
      samples,
    };
  }

  const result = {
    title: document.title,
    url: location.href,
    phase: "unknown",
    phaseLabel: null,
    onWallPage: false,
    onControlPage: false,
    action: "none",
    navigatedToWall: false,
    retried: false,
    controlOpened: false,
    live: false,
    reason: null,
    frame: null,
    texts: null,
    stats: null,
    waitedMs: 0,
    verifyErrors: null,
  };

  function appReady() {
    return (
      onWallPage() ||
      onControlPage() ||
      !!document.querySelector(".nav-item") ||
      !!document.querySelector(".page-head") ||
      !!document.querySelector(".login-screen")
    );
  }

  try {
    // 应用刚启动时可能还在 splash，先等界面稳定。
    while (!appReady() && Date.now() - startedAt < STARTUP_WAIT_MS) {
      await sleep(100);
    }

    let control = onControlPage();
    let wall = onWallPage();

    // 不在屏幕墙/控制页时，尽量用侧栏回到屏幕墙（不依赖设备名）。
    if (!control && !wall) {
      const wallNav = all(".nav-item").find(
        (item) => textOf(item).indexOf("屏幕墙") >= 0,
      );
      if (wallNav) {
        wallNav.click();
        result.navigatedToWall = true;
        const navStarted = Date.now();
        while (!onWallPage() && Date.now() - navStarted < NAV_WAIT_MS) {
          await sleep(100);
        }
        wall = onWallPage();
      }
    }

    if (control) {
      result.action = "already-on-control";
      // 上次运行可能停在“画面接入失败”遮罩，先点重新连接再等待。
      const retryButton = document.querySelector(".ctrl-stream-error .btn");
      if (retryButton) {
        retryButton.click();
        result.retried = true;
        result.action = "retried-on-control";
      }
    } else if (wall) {
      // 第一张在线设备卡片（网格或列表视图都支持）。
      const onlineCards = all(".wall-card.is-online, .wall-row.is-online");
      if (onlineCards.length === 0) {
        result.reason = "no-online-device";
      } else {
        const card = onlineCards[0];
        const controlButton =
          card.querySelector(".wall-veil-btn.is-primary") ||
          card.querySelector(".wall-open");
        if (controlButton) {
          result.action =
            controlButton.className.indexOf("wall-veil-btn") >= 0
              ? "clicked-veil-control"
              : "clicked-footer-control";
          controlButton.click();
        } else {
          // 兜底：卡片本身也可打开控制页。
          result.action = "clicked-card-fallback";
          card.click();
        }
      }
    } else {
      result.reason = "not-on-wall-page";
    }

    if (!result.reason) {
      const clicked = result.action.indexOf("clicked") === 0;
      const clickAt = Date.now();

      // 最多等待 FRAME_WAIT_MS，直到出现可用的 JPEG 首帧。
      const frameDeadline = clickAt + FRAME_WAIT_MS;
      while (Date.now() < frameDeadline) {
        const frame = getFrame();
        if (
          frame.valid &&
          frame.naturalWidth > 0 &&
          !hasStreamError() &&
          !hasOffline()
        ) {
          break;
        }
        // 点击后 5 秒还没进控制页，提前失败，避免无意义的等待。
        if (clicked && !onControlPage() && Date.now() - clickAt >= NAV_WAIT_MS) {
          result.reason = "control-page-not-opened";
          break;
        }
        await sleep(200);
      }
    }

    result.controlOpened = onControlPage();
    const phase = detectPhase();
    result.phase = phase.phase;
    result.phaseLabel = phase.label;
    result.onWallPage = onWallPage();
    result.onControlPage = onControlPage();
    result.frame = getFrame();
    result.texts = collectTexts();
    result.stats = collectStats();
    result.verifyErrors = readVerifyErrors();
    result.waitedMs = Date.now() - startedAt;

    result.live =
      result.frame.valid &&
      result.frame.naturalWidth > 0 &&
      !hasStreamError() &&
      !hasOffline();

    if (!result.live && !result.reason) {
      if (!onControlPage()) result.reason = "control-page-not-opened";
      else if (hasOffline()) result.reason = "device-offline";
      else if (hasStreamError()) result.reason = "stream-error";
      else if (!result.frame.found) result.reason = "frame-timeout";
      else if (result.frame.srcPrefix !== JPEG_PREFIX) result.reason = "frame-not-jpeg";
      else if (result.frame.dataUrlLength < MIN_FRAME_DATA_URL_LENGTH) {
        result.reason = "frame-too-small";
      } else result.reason = "frame-not-ready";
    }

    result.title = document.title;
    result.url = location.href;
  } catch (error) {
    result.reason = "evaluate-error";
    result.error = String((error && error.stack) || error);
    result.waitedMs = Date.now() - startedAt;
    try {
      result.title = document.title;
      result.url = location.href;
      result.phase = detectPhase().phase;
      result.frame = getFrame();
      result.texts = collectTexts();
      result.stats = collectStats();
      result.verifyErrors = readVerifyErrors();
    } catch (nested) {
      // 页面可能正在切页，忽略二次读取失败。
    }
  }

  return result;
}

const REASON_TEXT = {
  "no-online-device": "屏幕墙上没有在线设备卡片",
  "not-on-wall-page": "当前不在屏幕墙页，且无法通过侧栏返回",
  "control-page-not-opened": "点击控制按钮后未进入单台控制页",
  "device-offline": "控制页显示设备离线",
  "stream-error": "控制页显示画面接入失败",
  "frame-timeout": "20 秒内没有出现 img.ctrl-live-frame",
  "frame-not-jpeg": "首帧 src 不是 data:image/jpeg;base64 数据",
  "frame-too-small": "首帧 data URL 长度不足，可能是占位图",
  "frame-not-ready": "首帧尚未解码完成",
  "evaluate-error": "页面内验证脚本异常",
};

const ACTION_TEXT = {
  "already-on-control": "已在单台控制页，跳过点击",
  "retried-on-control": "已在控制页，点击了“重新连接”",
  "clicked-veil-control": "点击第一张在线设备卡片的“控制”按钮",
  "clicked-footer-control": "点击第一张在线设备卡片的“控制”按钮",
  "clicked-card-fallback": "未找到控制按钮，改为点击在线设备卡片",
  none: "无操作",
};

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
      req.destroy(
        new Error(`请求 http://${CDP_HOST}:${CDP_PORT}${urlPath} 超时 (${timeoutMs}ms)`),
      );
    });
    req.on("error", reject);
  });
}

function findPageTarget(targets) {
  if (!Array.isArray(targets)) {
    throw new Error("/json 返回的不是 target 数组");
  }
  const pages = targets.filter(
    (target) => target && target.type === "page" && target.webSocketDebuggerUrl,
  );
  const match = pages.find((target) => {
    const title = String(target.title || "");
    const url = String(target.url || "");
    return (
      /screenwall/i.test(title) ||
      url.indexOf("localhost:1420") >= 0 ||
      url.indexOf("127.0.0.1:1420") >= 0 ||
      url.indexOf("tauri://localhost") >= 0
    );
  });
  if (match) return match;

  const available =
    pages.map((target) => `${target.title || "(无标题)"} ${target.url || ""}`.trim()).join("; ") ||
    "(没有 page target)";
  throw new Error(`未找到 ScreenWall 页面 target。可用 target: ${available}`);
}

function createCdpClient(wsUrl, onEvent) {
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
      } catch (error) {
        // 忽略关闭失败。
      }
      reject(new Error(`CDP WebSocket 连接超时 (${WS_OPEN_TIMEOUT_MS}ms)`));
    }, WS_OPEN_TIMEOUT_MS);

    ws.addEventListener("open", () => {
      clearTimeout(openTimer);
      if (settled) return;
      settled = true;

      resolve({
        send(method, params = {}, timeoutMs = CDP_COMMAND_TIMEOUT_MS) {
          return new Promise((res, rej) => {
            if (state.closed) {
              rej(new Error("CDP 连接已关闭"));
              return;
            }
            const id = (state.nextId += 1);
            const timer = setTimeout(() => {
              pending.delete(id);
              rej(new Error(`CDP ${method} 超时 (${timeoutMs}ms)`));
            }, timeoutMs);
            pending.set(id, { resolve: res, reject: rej, timer });
            try {
              ws.send(JSON.stringify({ id, method, params }));
            } catch (error) {
              clearTimeout(timer);
              pending.delete(id);
              rej(error);
            }
          });
        },
        close() {
          try {
            ws.close();
          } catch (error) {
            // 忽略关闭失败。
          }
        },
        get closed() {
          return state.closed;
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
      if (message && typeof message.id === "number") {
        const entry = pending.get(message.id);
        if (!entry) return;
        pending.delete(message.id);
        clearTimeout(entry.timer);
        if (message.error) {
          const code = message.error.code ? ` (code ${message.error.code})` : "";
          entry.reject(new Error(`${message.error.message || "CDP 错误"}${code}`));
        } else {
          entry.resolve(message.result || {});
        }
        return;
      }
      if (message && message.method && onEvent) {
        try {
          onEvent(message.method, message.params || {});
        } catch (error) {
          // 事件处理失败不影响主流程。
        }
      }
    });

    ws.addEventListener("error", (event) => {
      clearTimeout(openTimer);
      if (settled) return;
      settled = true;
      reject(new Error(`无法连接 CDP WebSocket: ${event.message || event.type || "error"}`));
    });

    ws.addEventListener("close", (event) => {
      clearTimeout(openTimer);
      state.closed = true;
      for (const [id, entry] of pending) {
        clearTimeout(entry.timer);
        entry.reject(new Error("CDP 连接已关闭"));
        pending.delete(id);
      }
      if (settled) return;
      settled = true;
      reject(new Error(`CDP WebSocket 已关闭 (code ${event.code || 0})`));
    });
  });
}

function remoteArgText(arg) {
  if (!arg) return "";
  if (Object.prototype.hasOwnProperty.call(arg, "value")) {
    if (typeof arg.value === "string") return arg.value;
    try {
      return JSON.stringify(arg.value);
    } catch (error) {
      return String(arg.value);
    }
  }
  if (arg.unserializableValue) return String(arg.unserializableValue);
  if (arg.description) return String(arg.description);
  return arg.type || "";
}

function oneLine(text, maxLength = 400) {
  const flat = String(text).replace(/\s+/g, " ").trim();
  return flat.length > maxLength ? `${flat.slice(0, maxLength)}…` : flat;
}

function formatError(err) {
  const prefix =
    err.source === "exception" ? "[exception]" : `[console.${err.level || "error"}]`;
  let location = "";
  if (err.url) {
    const line = typeof err.line === "number" ? err.line + 1 : "?";
    location = ` (${err.url}:${line})`;
  }
  return `${prefix} ${err.text}${location}`;
}

async function main() {
  if (typeof WebSocket !== "function") {
    console.error("[verify-m2] 当前 Node 没有内置 WebSocket，请使用 Node 22 或更高版本");
    return 1;
  }

  const watchdog = setTimeout(() => {
    console.error(
      `[verify-m2] 总超时（${Math.round(OVERALL_TIMEOUT_MS / 1000)} 秒），退出`,
    );
    process.exit(1);
  }, OVERALL_TIMEOUT_MS);

  let cdp = null;
  let target = null;
  const errors = [];
  const warnings = [];

  try {
    let targets;
    try {
      targets = await httpGetJson("/json", DISCOVERY_TIMEOUT_MS);
    } catch (error) {
      throw new Error(
        `无法读取 CDP target 列表（确认客户端以 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=${CDP_PORT} 启动）: ${error.message}`,
      );
    }
    target = findPageTarget(targets);

    cdp = await createCdpClient(target.webSocketDebuggerUrl, (method, params) => {
      if (method === "Runtime.consoleAPICalled") {
        const level = params.type || "log";
        const text = (params.args || []).map(remoteArgText).join(" ");
        if (level === "error" || level === "assert") {
          errors.push({ source: "console", level, text: oneLine(text) });
        } else if (level === "warning") {
          warnings.push(oneLine(text));
        }
        return;
      }
      if (method === "Runtime.exceptionThrown") {
        const details = params.exceptionDetails || {};
        const exception = details.exception || {};
        errors.push({
          source: "exception",
          level: "error",
          text: oneLine(
            exception.description || details.text || "未捕获异常",
          ),
          url: details.url || exception.url || "",
          line: typeof details.lineNumber === "number" ? details.lineNumber : undefined,
        });
      }
    });

    await cdp.send("Runtime.enable", {}, 5000);
    await cdp.send("Page.enable", {}, 5000);
    await cdp.send("Page.bringToFront", {}, 5000).catch(() => {});

    let pageResult = null;
    let evalError = null;
    try {
      const evaluation = await cdp.send(
        "Runtime.evaluate",
        {
          expression: `(${verifyInPage.toString()})()`,
          awaitPromise: true,
          returnByValue: true,
          userGesture: true,
        },
        CDP_COMMAND_TIMEOUT_MS,
      );
      if (evaluation.exceptionDetails) {
        const details = evaluation.exceptionDetails;
        evalError = new Error(
          `Runtime.evaluate 异常: ${
            (details.exception && details.exception.description) ||
            details.text ||
            "未知错误"
          }`,
        );
      } else if (!evaluation.result || evaluation.result.value === undefined) {
        evalError = new Error("Runtime.evaluate 没有返回可序列化结果");
      } else {
        pageResult = evaluation.result.value;
      }
    } catch (error) {
      evalError = error;
    }

    let screenshotError = null;
    try {
      fs.mkdirSync(path.dirname(SCREENSHOT_PATH), { recursive: true });
      await cdp.send("Page.bringToFront", {}, 5000).catch(() => {});
      const shot = await cdp.send(
        "Page.captureScreenshot",
        { format: "png", fromSurface: true },
        15000,
      );
      if (!shot || typeof shot.data !== "string" || shot.data.length === 0) {
        throw new Error("截图数据为空");
      }
      fs.writeFileSync(SCREENSHOT_PATH, Buffer.from(shot.data, "base64"));
    } catch (error) {
      screenshotError = error;
    }

    const live = !!(pageResult && pageResult.live);
    const ok = live && !evalError && !screenshotError;
    const frame = (pageResult && pageResult.frame) || {};
    const texts = (pageResult && pageResult.texts) || {};
    const stats = (pageResult && pageResult.stats) || {};
    const verifyErrors = (pageResult && pageResult.verifyErrors) || {};

    const lines = [];
    lines.push(
      `[verify-m2] target: ${target.title || "(无标题)"} — ${target.url || ""}`.trim(),
    );

    if (pageResult) {
      lines.push(
        `[verify-m2] phase: ${pageResult.phase || "unknown"}` +
          `${pageResult.phaseLabel ? ` (${pageResult.phaseLabel})` : ""}` +
          ` | 动作: ${ACTION_TEXT[pageResult.action] || pageResult.action}` +
          ` | 耗时: ${pageResult.waitedMs || 0}ms`,
      );
      lines.push(
        `[verify-m2] frame: ${live ? "OK" : "未取得"}` +
          ` | data URL ${frame.dataUrlLength || 0} 字符` +
          ` | 图片 ${frame.naturalWidth || 0}x${frame.naturalHeight || 0}`,
      );
      lines.push(
        `[verify-m2] FPS=${stats.fps || "—"}` +
          ` | 分辨率=${stats.resolution || "—"}` +
          ` | 延迟=${stats.latency || "—"}` +
          ` | 码率=${stats.bandwidth || "—"}` +
          ` | 质量=${stats.quality || "—"}`,
      );
      lines.push(
        `[verify-m2] page texts: 画面接入失败=${texts.pictureFailed ? "是" : "否"}` +
          ` | 设备离线=${texts.deviceOffline ? "是" : "否"}` +
          ` | 会话断开=${texts.sessionClosed ? "是" : "否"}`,
      );
      lines.push(
        `[verify-m2] window.__verifyErrors: ${
          verifyErrors.exists
            ? `存在 (${verifyErrors.length || 0} 条)`
            : "不存在"
        }`,
      );
      if (!live) {
        lines.push(
          `[verify-m2] 失败原因: ${
            REASON_TEXT[pageResult.reason] || pageResult.reason || "未知"
          }`,
        );
      }
    } else if (evalError) {
      lines.push(`[verify-m2] 页面验证失败: ${evalError.message}`);
    }

    lines.push(`[verify-m2] console errors: ${errors.length}`);
    for (const err of errors.slice(0, 10)) {
      lines.push(`  - ${formatError(err)}`);
    }
    if (errors.length > 10) {
      lines.push(`  ... 另有 ${errors.length - 10} 条`);
    }
    if (warnings.length > 0) {
      lines.push(`[verify-m2] console.warning: ${warnings.length} 条（未计入报错）`);
    }

    lines.push(
      `[verify-m2] screenshot: ${SCREENSHOT_PATH}${
        screenshotError ? ` (失败: ${screenshotError.message})` : ""
      }`,
    );
    lines.push(`[verify-m2] RESULT: ${ok ? "PASS" : "FAIL"}`);

    console.log(lines.join("\n"));
    return ok ? 0 : 1;
  } catch (error) {
    console.error(`[verify-m2] FAIL: ${error.message}`);
    return 1;
  } finally {
    clearTimeout(watchdog);
    if (cdp) {
      try {
        await cdp.send("Runtime.disable", {}, 1000).catch(() => {});
      } catch (error) {
        // 忽略关闭前的清理失败。
      }
      cdp.close();
    }
  }
}

main()
  .then((code) => {
    process.exitCode = code;
  })
  .catch((error) => {
    console.error(`[verify-m2] FAIL: ${(error && error.stack) || error}`);
    process.exitCode = 1;
  });
