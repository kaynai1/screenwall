// 本地假服务器：只有账号接口 + 一个假的在线设备 + MJPEG/PCM 画面声音。
//
// 用途：不依赖 Windows 被控端、也不需要真实账号，就能在模拟器 / 真机上把
// 原生主控端的登录、设备列表、控制会话整条链路过一遍。
//
// 用法：
//   node android/scripts/fake-server.mjs
//   （可选）SW_FRAMES=某目录，目录里放 frame0.jpg、frame1.jpg… 轮流发送；
//          不设置时读取系统临时目录下的 sw-fake-frames。
//
// 然后用 App 登录：服务器 http://127.0.0.1:8788（先 adb reverse tcp:8788 tcp:8788），
// 账号密码随便填。它和真被控端一样：H.264 订阅会回 4104，用来验证客户端的回退。

import http from 'node:http';
import crypto from 'node:crypto';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const PORT = Number(process.env.PORT || 8788);
const FRAME_DIR = process.env.SW_FRAMES || path.join(os.tmpdir(), 'sw-fake-frames');
const FAKE_TOKEN = 'fake-dev-token';
const QUICK_TOKEN = 'fake-quick-token';
const QUICK_CODE = '123456789';
/** SW_WEBRTC=1 时假装被控端支持直连，用来验证「先试 P2P、失败回中继」。 */
const FAKE_WEBRTC = process.env.SW_WEBRTC === '1';
/**
 * 浏览器里的假被控端（`scripts/fake-agent.html`）。
 *
 * 打开后连 `/ws/agent`，这里把主控端的 `t:"webrtc"` 信令转发给它，
 * 再把它的 answer / trickle ICE 原样转回主控端——手机端就能真的跑通
 * WebRTC 直连（H.264 + Opus 轨道、`ctrl` DataChannel）。
 */
const agent = { socket: null, viewers: new Map() };
const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const SESSION_ID = 7;
const VIDEO_ID = 1;
const AUDIO_ID = 2;

const JPEGS = (() => {
  if (!existsSync(FRAME_DIR)) return [];
  return readdirSync(FRAME_DIR)
    .filter((name) => /\.jpe?g$/i.test(name))
    .sort()
    .map((name) => readFileSync(path.join(FRAME_DIR, name)));
})();

const nowMs = () => Date.now();

function devicesPayload() {
  return [
    {
      id: 'dev-1',
      name: '测试电脑（假设备）',
      os: 'windows',
      version: '0.2.20',
      online: true,
      lastSeen: nowMs(),
      controllable: true,
      webrtc: FAKE_WEBRTC,
    },
    {
      id: 'dev-2',
      name: '离线的那台',
      os: 'windows',
      version: '0.2.19',
      online: false,
      lastSeen: nowMs() - 8 * 60 * 1000,
      controllable: true,
    },
    // 查看端也会出现在账号设备表里，客户端应当把它们隐藏。
    {
      id: 'web-1',
      name: '网页端',
      os: 'web',
      version: 'vweb',
      online: true,
      lastSeen: nowMs(),
      controllable: true,
    },
    {
      id: 'android-1',
      name: '安卓端',
      os: 'android',
      version: 'v15',
      online: false,
      lastSeen: nowMs() - 3 * 60 * 1000,
      controllable: true,
    },
  ];
}

/** 识别码会话只能看到自己那台设备。 */
function quickDevicesPayload() {
  return devicesPayload().filter((device) => device.id === 'dev-1');
}

// ---------------------------------------------------------------- WebSocket 帧

const OP_TEXT = 0x1;
const OP_BINARY = 0x2;
const OP_CLOSE = 0x8;
const OP_PING = 0x9;
const OP_PONG = 0xa;

function wsSend(socket, opcode, payload) {
  const length = payload.length;
  let header;
  if (length < 126) {
    header = Buffer.alloc(2);
    header[1] = length;
  } else if (length < 65536) {
    header = Buffer.alloc(4);
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  header[0] = 0x80 | opcode;
  socket.write(Buffer.concat([header, payload]));
}

const sendText = (socket, value) =>
  wsSend(socket, OP_TEXT, Buffer.from(typeof value === 'string' ? value : JSON.stringify(value)));
const sendBinary = (socket, bytes) => wsSend(socket, OP_BINARY, bytes);

/** 解析客户端（带掩码）发来的帧，回调 (opcode, payload)。 */
function feedFrames(state, chunk, onFrame) {
  state.buffer = Buffer.concat([state.buffer, chunk]);
  for (;;) {
    const buf = state.buffer;
    if (buf.length < 2) return;
    const opcode = buf[0] & 0x0f;
    const masked = (buf[1] & 0x80) !== 0;
    let length = buf[1] & 0x7f;
    let offset = 2;
    if (length === 126) {
      if (buf.length < 4) return;
      length = buf.readUInt16BE(2);
      offset = 4;
    } else if (length === 127) {
      if (buf.length < 10) return;
      length = Number(buf.readBigUInt64BE(2));
      offset = 10;
    }
    let mask = null;
    if (masked) {
      if (buf.length < offset + 4) return;
      mask = buf.subarray(offset, offset + 4);
      offset += 4;
    }
    if (buf.length < offset + length) return;
    let payload = buf.subarray(offset, offset + length);
    if (mask) {
      payload = Buffer.from(payload);
      for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
    }
    state.buffer = buf.subarray(offset + length);
    onFrame(opcode, payload);
  }
}

// ---------------------------------------------------------------- 隧道 / 媒体帧

const TUNNEL_JSON = 1;
const TUNNEL_BINARY = 2;

function tunnel(kind, session, payload) {
  const header = Buffer.alloc(8);
  header[0] = 1;
  header[1] = kind;
  header.writeUInt32LE(session, 2);
  header.writeUInt16LE(0, 6);
  return Buffer.concat([header, payload]);
}

const tunnelJson = (session, value) =>
  tunnel(TUNNEL_JSON, session, Buffer.from(JSON.stringify(value)));

function mediaFrame(kind, streamId, seq, codec, flags, payload) {
  const header = Buffer.alloc(24);
  header[0] = 1;
  header[1] = kind;
  header.writeUInt16LE(streamId, 2);
  header.writeUInt32LE(seq >>> 0, 4);
  header.writeBigUInt64LE(BigInt(Date.now()) * 1000n, 8);
  header.writeUInt32LE(payload.length, 16);
  header[20] = codec;
  header[21] = flags;
  return Buffer.concat([header, payload]);
}

// ---------------------------------------------------------------- 会话

function startSession(socket) {
  const timers = new Set();
  let videoTimer = null;
  let audioTimer = null;
  let videoSeq = 0;
  let audioSeq = 0;
  let audioPhase = 0;
  let frameIndex = 0;
  let inputBatches = 0;
  let inputEvents = 0;
  let acks = 0;

  const sessionJson = (value) => sendBinary(socket, tunnelJson(SESSION_ID, value));

  function stopVideo() {
    if (videoTimer) clearInterval(videoTimer);
    videoTimer = null;
  }

  function stopAudio() {
    if (audioTimer) clearInterval(audioTimer);
    audioTimer = null;
  }

  function startVideo(fps) {
    stopVideo();
    // 允许 60/90 fps，测帧率上限时别被假服务器自己卡住。
    const interval = Math.max(8, Math.round(1000 / Math.max(1, fps)));
    videoTimer = setInterval(() => {
      if (JPEGS.length === 0) return;
      const jpeg = JPEGS[frameIndex % JPEGS.length];
      frameIndex += 1;
      const frame = mediaFrame(1, VIDEO_ID, videoSeq, 1, 0x01, jpeg);
      videoSeq += 1;
      sendBinary(socket, tunnel(TUNNEL_BINARY, SESSION_ID, frame));
    }, interval);
    timers.add(videoTimer);
  }

  function startAudio() {
    stopAudio();
    const sampleRate = 48000;
    const samples = 960; // 20 ms
    audioTimer = setInterval(() => {
      const pcm = Buffer.alloc(samples * 2);
      for (let i = 0; i < samples; i += 1) {
        const value = Math.round(Math.sin(audioPhase) * 800); // 很轻的 440 Hz 提示音
        audioPhase += (2 * Math.PI * 440) / sampleRate;
        pcm.writeInt16LE(value, i * 2);
      }
      const frame = mediaFrame(2, AUDIO_ID, audioSeq, 3, 0, pcm);
      audioSeq += 1;
      sendBinary(socket, tunnel(TUNNEL_BINARY, SESSION_ID, frame));
    }, 20);
    timers.add(audioTimer);
  }

  function handleSessionMessage(message) {
    switch (message.t) {
      case 'hello':
        sessionJson({
          t: 'welcome',
          proto: 1,
          agent: { name: '测试电脑（假设备）', version: '0.2.20' },
          limits: { maxStreams: 4, maxChunk: 1048576, allowControl: true },
          caps: [],
        });
        sessionJson({
          t: 'display.list',
          displays: [
            { index: 0, w: 1280, h: 720, primary: true, x: 0, y: 0 },
            { index: 1, w: 1024, h: 768, primary: false, x: 1280, y: 0 },
          ],
        });
        break;
      case 'video.sub': {
        const fps = Number(message.fps) || 30;
        console.log(`[video] 订阅 codec=${message.codec} ${message.maxWidth}px ${fps}fps`);
        if (message.codec !== 'mjpeg') {
          // 和 main 分支的真被控端一致：目前只有 MJPEG。
          sessionJson({ t: 'error', code: 4104, msg: '当前只支持 MJPEG 编码' });
          break;
        }
        if (JPEGS.length === 0) {
          sessionJson({ t: 'error', code: 4104, msg: `没有测试画面：请先往 ${FRAME_DIR} 放几张 jpg` });
          break;
        }
        sessionJson({
          t: 'video.sub.ok',
          id: VIDEO_ID,
          stream: 'focus',
          actual: { codec: 'mjpeg', w: 1280, h: 720, fps },
        });
        startVideo(fps);
        break;
      }
      case 'video.unsub':
        if (Number(message.id) === VIDEO_ID) stopVideo();
        break;
      case 'audio.sub':
        console.log(`[audio] 订阅 codec=${message.codec || 'auto'}`);
        sessionJson({
          t: 'audio.sub.ok',
          id: AUDIO_ID,
          actual: {
            codec: 'pcm16',
            sampleRate: 48000,
            channels: 1,
            bitrate: 0,
            source: 'loopback',
          },
        });
        startAudio();
        break;
      case 'audio.unsub':
        if (Number(message.id) === AUDIO_ID) stopAudio();
        break;
      case 'video.ack':
        acks += 1;
        if (acks % 120 === 1) console.log(`[ack] 已确认 ${acks} 帧`);
        break;
      case 'ping':
        sessionJson({ t: 'pong', ts: message.ts });
        break;
      case 'sas':
        sessionJson({ t: 'sas.ok', method: 'sas' });
        break;
      case 'input': {
        const events = Array.isArray(message.events) ? message.events : [];
        inputBatches += 1;
        inputEvents += events.length;
        if (inputBatches <= 8 || inputBatches % 50 === 0) {
          console.log(`[input] 收到 ${inputBatches} 批 / ${inputEvents} 个事件`);
        }
        break;
      }
      default:
        break;
    }
  }

  function handleText(text) {
    let message;
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    switch (message.t) {
      case 'open':
        console.log(`[open] 设备 ${message.deviceId}，mode=${message.mode}`);
        setTimeout(() => {
          sendText(socket, { t: 'open.ok', request: message.request, session: SESSION_ID });
        }, 200);
        break;
      case 'close':
        stopVideo();
        stopAudio();
        break;
      case 'webrtc':
        // 有假被控端在线就转发信令，跑真正的直连；没有就静默丢弃，
        // 主控端等 4 秒后回退中继（和「被控端不支持 WebRTC」一致）。
        console.log(`[webrtc] ${message.event} request=${message.request}`);
        forwardViewerWebrtc(socket, message);
        break;
      default:
        break;
    }
  }

  function handleBinary(bytes) {
    if (bytes.length < 8 || bytes[0] !== 1) return;
    if (bytes[1] !== TUNNEL_JSON) return;
    let message;
    try {
      message = JSON.parse(bytes.subarray(8).toString('utf8'));
    } catch {
      return;
    }
    handleSessionMessage(message);
  }

  function cleanup() {
    stopVideo();
    stopAudio();
    for (const timer of timers) clearTimeout(timer);
    timers.clear();
  }

  return { handleText, handleBinary, cleanup };
}

// ---------------------------------------------------------------- HTTP

function json(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(body),
  });
  res.end(body);
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url || '/', `http://${req.headers.host || 'localhost'}`);
  console.log(`[http] ${req.method} ${url.pathname}`);
  if (req.method === 'POST' && url.pathname === '/api/v1/login') {
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => {
      body += chunk;
      if (body.length > 16 * 1024) req.destroy();
    });
    req.on('end', () => {
      let name = '';
      try {
        name = JSON.parse(body).username || '';
      } catch {
        // 随便填也能登录
      }
      json(res, 200, {
        token: FAKE_TOKEN,
        expiresAt: '2099-01-01T00:00:00Z',
        user: { id: 1, username: name || 'tester' },
        device: { id: 'mobile-1', name: '安卓端', connectCode: '000000' },
      });
    });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/api/v1/quick/connect') {
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => {
      body += chunk;
      if (body.length > 16 * 1024) req.destroy();
    });
    req.on('end', () => {
      let code = '';
      let hasPassword = false;
      try {
        const parsed = JSON.parse(body);
        code = String(parsed.code || '');
        hasPassword = Boolean(parsed.password);
      } catch {
        // 参数坏掉按校验失败处理
      }
      console.log(`[quick] 识别码=${code} 密码=${hasPassword ? '已填' : '空'}`);
      if (code !== QUICK_CODE || !hasPassword) {
        json(res, 401, { error: 'quick_auth_failed', msg: '连接码或密码不正确' });
        return;
      }
      json(res, 200, {
        token: QUICK_TOKEN,
        expiresAt: nowMs() + 15 * 60 * 1000,
        device: { id: 'dev-1', name: '测试电脑（假设备）' },
      });
    });
    return;
  }
  if (req.method === 'GET' && url.pathname === '/api/v1/devices') {
    if ((req.headers.authorization || '').includes(QUICK_TOKEN)) {
      json(res, 403, { error: 'quick_token', msg: '连接码会话只能访问一台设备' });
      return;
    }
    json(res, 200, { devices: devicesPayload() });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/api/v1/p2p/ice') {
    if (!FAKE_WEBRTC) {
      json(res, 404, { error: 'webrtc_disabled', msg: '假服务器没有开启 WebRTC' });
      return;
    }
    json(res, 200, {
      iceServers: [{ urls: ['stun:127.0.0.1:3478'] }],
      ttl: 3600,
    });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/api/v1/logout') {
    json(res, 200, { ok: true });
    return;
  }
  if (req.method === 'GET' && url.pathname === '/agent.html') {
    const file = path.join(SCRIPT_DIR, 'fake-agent.html');
    if (!existsSync(file)) {
      res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8' });
      res.end('缺少 scripts/fake-agent.html');
      return;
    }
    const body = readFileSync(file);
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'content-length': body.length });
    res.end(body);
    return;
  }
  json(res, 404, { error: 'not_found', msg: '假服务器只有 login / devices / logout / agent.html' });
});

// ---------------------------------------------------------------- 假被控端（WebRTC）

/** 完成 WebSocket 握手；缺 key 直接断开并返回 false。 */
function acceptUpgrade(req, socket) {
  const key = req.headers['sec-websocket-key'];
  if (!key) {
    socket.destroy();
    return false;
  }
  const accept = crypto
    .createHash('sha1')
    .update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
    .digest('base64');
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n' +
      'Upgrade: websocket\r\n' +
      'Connection: Upgrade\r\n' +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  return true;
}

/** 主控端的 `t:"webrtc"` 信令转给浏览器假被控端；没有假被控端就丢弃。 */
function forwardViewerWebrtc(viewerSocket, message) {
  const target = agent.socket;
  if (!target) return;
  const request = Number(message.request) || 0;
  if (message.event === 'offer') {
    agent.viewers.set(request, viewerSocket);
    sendText(target, {
      t: 'webrtc',
      request,
      viewerId: 1,
      event: 'offer',
      viewer: message.viewer || '手机端',
      sdp: message.sdp,
      iceServers: [{ urls: ['stun:127.0.0.1:3478'] }],
    });
  } else if (message.event === 'ice') {
    sendText(target, { t: 'webrtc', request, viewerId: 1, event: 'ice', ice: message.ice });
  } else if (message.event === 'bye') {
    agent.viewers.delete(request);
    sendText(target, { t: 'webrtc', request, viewerId: 1, event: 'bye' });
  }
}

/** 假被控端的 answer / trickle ICE 转回发起 offer 的那个主控端。 */
function handleAgentText(socket, text) {
  let message;
  try {
    message = JSON.parse(text);
  } catch {
    return;
  }
  if (message.t !== 'webrtc') return;
  const request = Number(message.request) || 0;
  const viewer = agent.viewers.get(request);
  if (!viewer) return;
  console.log(`[webrtc] agent ${message.event} request=${request}`);
  const out = { t: 'webrtc', request, event: message.event };
  if (typeof message.sdp === 'string') out.sdp = message.sdp;
  if (message.ice) out.ice = message.ice;
  if (typeof message.msg === 'string') out.msg = message.msg;
  sendText(viewer, out);
  if (message.event === 'bye' || message.event === 'reject') agent.viewers.delete(request);
}

function attachAgent(req, socket) {
  if (!acceptUpgrade(req, socket)) return;
  if (agent.socket) {
    // 同一时间只保留一个假被控端，新的顶掉旧的。
    try {
      wsSend(agent.socket, OP_CLOSE, Buffer.alloc(0));
      agent.socket.end();
    } catch {
      // 旧连接已经坏了，不用管
    }
  }
  agent.socket = socket;
  agent.viewers.clear();
  console.log('[agent] 假被控端已连接');
  const state = { buffer: Buffer.alloc(0) };
  socket.on('data', (chunk) => {
    feedFrames(state, chunk, (opcode, payload) => {
      if (opcode === OP_TEXT) handleAgentText(socket, payload.toString('utf8'));
      else if (opcode === OP_PING) wsSend(socket, OP_PONG, payload);
      else if (opcode === OP_CLOSE) {
        wsSend(socket, OP_CLOSE, Buffer.alloc(0));
        socket.end();
      }
    });
  });
  socket.on('close', () => {
    if (agent.socket === socket) {
      agent.socket = null;
      agent.viewers.clear();
    }
    console.log('[agent] 假被控端断开');
  });
  socket.on('error', () => socket.destroy());
}

server.on('upgrade', (req, socket) => {
  const url = new URL(req.url || '/', 'http://localhost');
  if (url.pathname === '/ws/agent') {
    attachAgent(req, socket);
    return;
  }
  if (url.pathname !== '/ws/view') {
    socket.destroy();
    return;
  }
  if (!acceptUpgrade(req, socket)) return;

  const state = { buffer: Buffer.alloc(0) };
  const session = startSession(socket);
  const quick = url.searchParams.get('token') === QUICK_TOKEN;
  const list = () => (quick ? quickDevicesPayload() : devicesPayload());

  sendText(socket, { t: 'ready' });
  sendText(socket, { t: 'devices', devices: list() });
  console.log(quick ? '[ws] 识别码会话已连接' : '[ws] 主控端已连接');

  // 模拟设备上下线推送，让设备列表的实时刷新也能看到效果。
  const devicesTimer = setInterval(() => {
    sendText(socket, { t: 'devices', devices: list() });
  }, 30_000);

  socket.on('data', (chunk) => {
    feedFrames(state, chunk, (opcode, payload) => {
      if (opcode === OP_TEXT) session.handleText(payload.toString('utf8'));
      else if (opcode === OP_BINARY) session.handleBinary(payload);
      else if (opcode === OP_PING) wsSend(socket, OP_PONG, payload);
      else if (opcode === OP_CLOSE) {
        wsSend(socket, OP_CLOSE, Buffer.alloc(0));
        socket.end();
      }
    });
  });

  socket.on('close', () => {
    clearInterval(devicesTimer);
    session.cleanup();
    for (const [request, viewer] of agent.viewers) {
      if (viewer === socket) agent.viewers.delete(request);
    }
    console.log('[ws] 主控端断开');
  });
  socket.on('error', () => socket.destroy());
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[fake] 监听 http://127.0.0.1:${PORT}`);
  console.log(`[fake] 画面目录：${FRAME_DIR}（${JPEGS.length} 张 jpg）`);
  if (JPEGS.length === 0) console.log('[fake] 没有 jpg，video.sub 会返回 4104');
});
