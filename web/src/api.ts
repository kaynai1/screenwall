/** 账号接口与主控 WebSocket（`/ws/view`）。 */

import { decodeMediaFrame, decodeTunnel, tunnelJson, TUNNEL_BINARY, TUNNEL_JSON, type MediaFrame } from "./protocol";

export type DeviceView = {
  id: string;
  name: string;
  os: string;
  version: string;
  online: boolean;
  lastSeen: number;
  controllable: boolean;
  /** 被控端支持网页端 WebRTC P2P（连接时带 `webrtc=1`）。 */
  webrtc?: boolean;
};

export type Account = {
  serverUrl: string;
  token: string;
  username: string;
  deviceId: string;
  /** 连接码换来的受限会话：只有一台设备。 */
  quick: boolean;
  expiresAt: number;
};

/** 网页端自己的名字，出现在被控端的授权弹窗里。 */
const VIEWER_NAME = "网页端";

/** 连上 WebSocket 后等 `ready` 的上限；超时按失败处理，外层会退避重连。 */
const CONNECT_TIMEOUT_MS = 10_000;

/** WebRTC 从 offer 到 DataChannel 打开的上限；超时直接回退中继。 */
const P2P_TIMEOUT_MS = 3_000;

/** 网页端在这台浏览器上的固定设备 ID（存在 localStorage）。 */
const DEVICE_KEY = "screenwall.web.deviceId";

/**
 * 每次登录都换一个新设备，会在账号里堆一堆「网页端」，还会吃掉设备配额。
 * 同一浏览器固定用同一个 ID：重复登录只是刷新这台设备的记录。
 */
function webDeviceId(): string {
  try {
    const saved = localStorage.getItem(DEVICE_KEY);
    if (saved && /^[A-Za-z0-9_-]{1,64}$/.test(saved)) return saved;
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    const id = `web-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
    localStorage.setItem(DEVICE_KEY, id);
    return id;
  } catch {
    // 隐私模式下存不了，就让服务器发一个（只是这台浏览器的设备会重复）。
    return "";
  }
}

export function normalizeServerUrl(input: string): string {
  const value = input.trim();
  if (!value) return window.location.origin;
  const withScheme = /^https?:\/\//i.test(value)
    ? value
    : `${window.location.protocol === "https:" ? "https" : "http"}://${value}`;
  return withScheme.replace(/\/+$/, "");
}

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(code: string, message: string, status: number) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

async function postJson<T>(
  serverUrl: string,
  path: string,
  body: unknown,
  headers?: Record<string, string>,
): Promise<T> {
  const response = await fetch(`${serverUrl}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const data = (await response.json().catch(() => null)) as
    | { error?: string; msg?: string }
    | null;
  if (!response.ok) {
    throw new ApiError(data?.error ?? "http_error", data?.msg ?? `HTTP ${response.status}`, response.status);
  }
  return data as T;
}

type LoginResponse = {
  token: string;
  expiresAt: number;
  user: { id: number; username: string };
  device?: { id: string; name: string; connectCode?: string };
};

export async function login(
  serverUrl: string,
  username: string,
  password: string,
): Promise<Account> {
  const data = await postJson<LoginResponse>(serverUrl, "/api/v1/login", {
    username,
    password,
    device: {
      id: webDeviceId() || undefined,
      name: "网页端",
      os: "Web",
      version: "web",
    },
  });
  return {
    serverUrl,
    token: data.token,
    username: data.user.username,
    deviceId: data.device?.id ?? "",
    quick: false,
    expiresAt: data.expiresAt,
  };
}

export async function register(
  serverUrl: string,
  username: string,
  password: string,
): Promise<Account> {
  const data = await postJson<LoginResponse>(serverUrl, "/api/v1/register", {
    username,
    password,
  });
  return {
    serverUrl,
    token: data.token,
    username: data.user.username,
    deviceId: "",
    quick: false,
    expiresAt: data.expiresAt,
  };
}

/** 识别码 + 密码直接连一台设备，不需要账号。 */
export async function quickConnect(
  serverUrl: string,
  code: string,
  password: string,
): Promise<Account> {
  const data = await postJson<{ token: string; expiresAt: number; device: { id: string; name: string } }>(
    serverUrl,
    "/api/v1/quick/connect",
    { code: code.trim(), password, name: "网页端" },
  );
  return {
    serverUrl,
    token: data.token,
    username: data.device.name,
    deviceId: data.device.id,
    quick: true,
    expiresAt: data.expiresAt,
  };
}

/**
 * 按 9 位识别码在本账号里找在线设备（服务器 `/api/v1/resolve-id`）。
 *
 * 浏览器做不了 UDP 广播和局域网直连，这是网页端唯一能「输识别码就连」的路径：
 * 两台机器都登录了同一个账号，且不在同一网段时也能走中转。
 */
export async function resolvePeerId(
  account: Account,
  id: string,
): Promise<{ deviceId: string; name: string }> {
  const data = await postJson<{ id: string; name: string; online: boolean }>(
    account.serverUrl,
    "/api/v1/resolve-id",
    { id },
    { authorization: `Bearer ${account.token}` },
  );
  return { deviceId: data.id, name: data.name };
}

export async function logout(account: Account): Promise<void> {
  await fetch(`${account.serverUrl}/api/v1/logout`, {
    method: "POST",
    headers: { authorization: `Bearer ${account.token}` },
  }).catch(() => {});
}

export type ViewerHandlers = {
  onReady?: () => void;
  onDevices?: (devices: DeviceView[]) => void;
  onOpen?: (session: number) => void;
  onOpenError?: (code: number, msg: string) => void;
  onClosed?: (code: number, msg: string) => void;
  /** 隧道里的应用消息（welcome / status / video.sub.ok / clipboard …）。 */
  onMessage?: (message: Record<string, unknown>) => void;
  onFrame?: (frame: MediaFrame) => void;
  onSocketClosed?: (reason: string) => void;
  /** P2P 建立成功后的远端视频 / 声音轨道。 */
  onP2pVideo?: (stream: MediaStream) => void;
  onP2pAudio?: (stream: MediaStream) => void;
  /** 已经走 P2P 后连接断开（和服务器 WebSocket 无关）。 */
  onP2pLost?: (reason: string) => void;
  /** P2P 会话建立完成（链路已经可以传控制消息和媒体）。 */
  onP2pActive?: () => void;
};

/** WebRTC 链路的每秒统计（`RTCPeerConnection.getStats()` 取）。 */
export type P2pStats = { fps: number; kbps: number; rtt: number; w: number; h: number };

type P2pHandles = {
  pc: RTCPeerConnection;
  dc: RTCDataChannel;
  request: number;
  active: boolean;
};

/**
 * 主控端 WebSocket。
 *
 * 浏览器不能给 WebSocket 设 `Authorization` 头，所以令牌走查询串；
 * 会话建立后所有应用消息都是 8 字节会话帧包着的 JSON 或媒体帧。
 */
export class Viewer {
  private socket: WebSocket | null = null;
  private session = 0;
  private request = 0;
  private pendingOpen: {
    request: number;
    resolve: (session: number) => void;
    reject: (error: Error) => void;
  } | null = null;
  private closedByUs = false;
  private readonly subscribers = new Set<ViewerHandlers>();

  /** 当前会话走哪条链路：中继（服务器 WebSocket）还是 P2P（DataChannel）。 */
  private transport: "relay" | "p2p" = "relay";
  private p2p: P2pHandles | null = null;
  private p2pRequest = 0;
  private p2pRemoteSet = false;
  private p2pIceQueue: RTCIceCandidateInit[] = [];
  /** 等 DataChannel 打开 / 等 `open.ok` 的挂起回调，超时或失败时一起收尾。 */
  private p2pWait: ((channel: RTCDataChannel | null) => void) | null = null;
  private p2pPendingOpen: { request: number; settle: (session: number | null) => void } | null = null;
  private p2pStatsTimer = 0;
  private p2pSampleValue: P2pStats = { fps: 0, kbps: 0, rtt: 0, w: 0, h: 0 };
  private p2pBytes = 0;
  private p2pSampleAt = 0;

  constructor(private readonly account: Account) {}

  /** 当前会话是否走 P2P。 */
  get usesP2p(): boolean {
    return this.transport === "p2p";
  }

  /** P2P 链路的最近一秒统计；中转链路返回全 0（调用方继续用解码器统计）。 */
  get p2pStats(): P2pStats {
    return this.p2pSampleValue;
  }

  /**
   * P2P 轨道只承载 H.264；设备编不出来时（`error 4104`）切回中继重新开会话。
   * 调用方拿到 `onOpen` 后会按现有流程重新握手、重新订阅。
   */
  async switchToRelay(deviceId: string): Promise<number> {
    this.session = 0;
    this.dropP2p();
    return this.open(deviceId, "control", false);
  }

  /** 订阅事件（App 和设备页/控制页各订阅一份，互不覆盖）。 */
  subscribe(handlers: ViewerHandlers): () => void {
    this.subscribers.add(handlers);
    return () => {
      this.subscribers.delete(handlers);
    };
  }

  private emit<K extends keyof ViewerHandlers>(
    key: K,
    ...args: Parameters<NonNullable<ViewerHandlers[K]>>
  ): void {
    for (const subscriber of this.subscribers) {
      const handler = subscriber[key] as ((...values: typeof args) => void) | undefined;
      handler?.(...args);
    }
  }

  get sessionId(): number {
    return this.session;
  }

  get isOpen(): boolean {
    return this.socket?.readyState === WebSocket.OPEN;
  }

  connect(): Promise<void> {
    this.closedByUs = false;
    this.session = 0;
    this.dropP2p();
    const base = this.account.serverUrl.replace(/^http/i, "ws");
    const url = `${base}/ws/view?token=${encodeURIComponent(this.account.token)}`;
    return new Promise((resolve, reject) => {
      let settled = false;
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      this.socket = socket;
      // 握手没拿到 `ready`（服务器刚重启、中间设备把连接挂住）时不能一直
      // 停在「正在连接服务器…」：超时就断开重来，外层会按退避重连。
      const timer = window.setTimeout(() => {
        if (settled) return;
        settled = true;
        try {
          socket.close();
        } catch {
          // 已经关了就算了。
        }
        reject(new Error("连接服务器超时"));
      }, CONNECT_TIMEOUT_MS);
      const settle = () => window.clearTimeout(timer);

      socket.onopen = () => {};
      socket.onerror = () => {
        settle();
        if (!settled) {
          settled = true;
          reject(new Error("无法连接服务器"));
        }
      };
      socket.onclose = (event) => {
        settle();
        if (!settled) {
          settled = true;
          reject(new Error(`连接被服务器关闭（${event.code}）`));
        }
        this.pendingOpen?.reject(new Error("连接已断开"));
        this.pendingOpen = null;
        this.dropP2p();
        if (!this.closedByUs) this.emit("onSocketClosed", event.reason || `连接已断开（${event.code}）`);
      };
      socket.onmessage = (event) => {
        if (typeof event.data === "string") {
          this.handleControl(event.data);
          if (!settled) {
            const parsed = safeJson(event.data);
            if (parsed?.t === "ready") {
              settled = true;
              settle();
              resolve();
            }
          }
          return;
        }
        void this.handleBinary(event.data as ArrayBuffer);
      };
    });
  }

  /**
   * 建立会话。`p2p` 为真时先试 WebRTC（3 秒连不上就关掉），失败再走中继，
   * 用户只会多看到一两秒「正在连接」。
   */
  async open(
    deviceId: string,
    mode: "control" | "wall" = "control",
    p2p = false,
  ): Promise<number> {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new Error("连接尚未就绪");
    }
    if (p2p && mode === "control" && (await this.openP2p(deviceId))) {
      this.emit("onOpen", this.session);
      return this.session;
    }
    this.request = (this.request + 1) >>> 0;
    const request = this.request;
    return new Promise((resolve, reject) => {
      this.pendingOpen = { request, resolve, reject };
      socket.send(
        JSON.stringify({
          t: "open",
          request,
          deviceId,
          viewer: VIEWER_NAME,
          mode,
        }),
      );
      window.setTimeout(() => {
        if (this.pendingOpen?.request === request) {
          this.pendingOpen = null;
          reject(new Error("连接超时，被控端没有响应"));
        }
      }, 30_000);
    });
  }

  sendJson(message: Record<string, unknown>): void {
    if (!this.session) return;
    if (this.transport === "p2p") {
      const dc = this.p2p?.dc;
      if (dc?.readyState === "open") {
        // 复制一份到普通 ArrayBuffer：DataChannel 的 `send` 不接受 SharedArrayBuffer 视图。
        dc.send(new Uint8Array(tunnelJson(this.session, message)).buffer);
      }
      return;
    }
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(tunnelJson(this.session, message));
  }

  /** 请求被控端立刻出一个关键帧（切换 H.264 / 分辨率后用）。 */
  requestKeyframe(id: number): void {
    this.sendJson({ t: "video.keyframe", id });
  }

  /** 只关会话，保留到服务器的 WebSocket（从控制页返回设备列表时用）。 */
  closeSession(): void {
    if (this.transport === "p2p") {
      const dc = this.p2p?.dc;
      if (dc?.readyState === "open" && this.session) {
        dc.send(JSON.stringify({ t: "close", session: this.session }));
      }
      this.session = 0;
      this.dropP2p();
      return;
    }
    const socket = this.socket;
    if (socket?.readyState === WebSocket.OPEN && this.session) {
      socket.send(JSON.stringify({ t: "close", session: this.session }));
    }
    this.session = 0;
  }

  close(): void {
    this.closedByUs = true;
    this.closeSession();
    this.dropP2p();
    const socket = this.socket;
    this.socket = null;
    socket?.close();
  }

  // ------------------------------------------------------------ WebRTC P2P

  /** ICE 服务来自服务器；接口不可用（未启用 WebRTC）就不尝试 P2P。 */
  private async fetchIceServers(): Promise<RTCIceServer[] | null> {
    try {
      const response = await fetch(`${this.account.serverUrl}/api/v1/p2p/ice`, {
        method: "POST",
        headers: { authorization: `Bearer ${this.account.token}` },
      });
      if (!response.ok) return null;
      const data = (await response.json()) as { iceServers?: RTCIceServer[] };
      return Array.isArray(data.iceServers) ? data.iceServers : [];
    } catch {
      return null;
    }
  }

  private sendP2pSignal(
    request: number,
    deviceId: string,
    payload: Record<string, unknown>,
  ): boolean {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return false;
    socket.send(
      JSON.stringify({ t: "webrtc", request, deviceId, viewer: VIEWER_NAME, ...payload }),
    );
    return true;
  }

  /** 试建 WebRTC；任何一步失败都返回 false，外层回退中继。 */
  private async openP2p(deviceId: string): Promise<boolean> {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return false;
    if (typeof RTCPeerConnection === "undefined") return false;
    const iceServers = await this.fetchIceServers();
    if (!iceServers) return false;

    this.dropP2p();
    this.p2pRemoteSet = false;
    this.p2pIceQueue = [];
    const request = (this.p2pRequest = (this.p2pRequest + 1) >>> 0);
    const pc = new RTCPeerConnection({ iceServers });
    // 两条 recvonly 的 m-line：被控端把 answer 里的轨道挂到这两路上。
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
    const dc = pc.createDataChannel("ctrl", { ordered: true });
    dc.binaryType = "arraybuffer";
    const handles: P2pHandles = { pc, dc, request, active: false };
    this.p2p = handles;

    pc.ontrack = (event) => {
      const stream = event.streams[0] ?? new MediaStream([event.track]);
      if (event.track.kind === "video") this.emit("onP2pVideo", stream);
      else if (event.track.kind === "audio") this.emit("onP2pAudio", stream);
    };
    pc.onicecandidate = (event) => {
      if (!event.candidate) return;
      this.sendP2pSignal(request, deviceId, { event: "ice", ice: event.candidate.toJSON() });
    };
    pc.onconnectionstatechange = () => {
      if (this.p2p !== handles) return;
      if (pc.connectionState === "failed") this.p2pFailed(handles, "直连已断开");
      else if (pc.connectionState === "connected") handles.active = true;
    };
    dc.onmessage = (event) => {
      if (typeof event.data === "string") this.handleDcControl(event.data);
      else if (event.data instanceof ArrayBuffer) void this.handleBinary(event.data);
    };
    dc.onclose = () => {
      if (this.p2p === handles) this.p2pFailed(handles, "直连通道已关闭");
    };

    const opened = new Promise<RTCDataChannel | null>((resolve) => {
      const timer = window.setTimeout(() => {
        this.p2pWait = null;
        resolve(null);
      }, P2P_TIMEOUT_MS);
      this.p2pWait = (channel) => {
        window.clearTimeout(timer);
        this.p2pWait = null;
        resolve(channel);
      };
    });
    dc.onopen = () => {
      if (this.p2p !== handles) return;
      this.p2pWait?.(dc);
    };

    try {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      const sdp = pc.localDescription?.sdp ?? "";
      if (!sdp || !this.sendP2pSignal(request, deviceId, { event: "offer", sdp })) {
        throw new Error("发送 offer 失败");
      }
    } catch {
      this.dropP2p();
      return false;
    }

    const channel = await opened;
    if (!channel || this.p2p !== handles) {
      this.dropP2p();
      return false;
    }
    const session = await this.openDcSession(channel);
    if (!session || this.p2p !== handles) {
      this.dropP2p();
      return false;
    }
    handles.active = true;
    this.transport = "p2p";
    this.session = session;
    this.startP2pStats(pc);
    this.emit("onP2pActive");
    return true;
  }

  /** DataChannel 上用直连协议发 `open`，拿被控端分配的会话号。 */
  private openDcSession(channel: RTCDataChannel): Promise<number | null> {
    return new Promise((resolve) => {
      const request = (this.request = (this.request + 1) >>> 0);
      const timer = window.setTimeout(() => {
        if (this.p2pPendingOpen?.request === request) {
          this.p2pPendingOpen = null;
          resolve(null);
        }
      }, 10_000);
      this.p2pPendingOpen = {
        request,
        settle: (session) => {
          window.clearTimeout(timer);
          this.p2pPendingOpen = null;
          resolve(session);
        },
      };
      try {
        channel.send(JSON.stringify({ t: "open", request, viewer: VIEWER_NAME }));
      } catch {
        this.p2pPendingOpen?.settle(null);
      }
    });
  }

  private handleP2pSignal(value: Record<string, unknown>): void {
    const p2p = this.p2p;
    if (!p2p || Number(value.request ?? 0) !== p2p.request) return;
    switch (value.event) {
      case "answer":
        void this.applyP2pAnswer(String(value.sdp ?? ""));
        break;
      case "ice":
        void this.applyP2pIce(value.ice);
        break;
      case "reject":
      case "bye":
        this.dropP2p();
        break;
      default:
        break;
    }
  }

  private async applyP2pAnswer(sdp: string): Promise<void> {
    const p2p = this.p2p;
    if (!p2p || !sdp) return;
    try {
      await p2p.pc.setRemoteDescription({ type: "answer", sdp });
      this.p2pRemoteSet = true;
      for (const ice of this.p2pIceQueue.splice(0)) {
        await p2p.pc.addIceCandidate(ice).catch(() => undefined);
      }
    } catch {
      this.dropP2p();
    }
  }

  private async applyP2pIce(ice: unknown): Promise<void> {
    const p2p = this.p2p;
    if (!p2p || !ice || typeof ice !== "object") return;
    const init = ice as RTCIceCandidateInit;
    if (!this.p2pRemoteSet) {
      if (this.p2pIceQueue.length < 64) this.p2pIceQueue.push(init);
      return;
    }
    await p2p.pc.addIceCandidate(init).catch(() => undefined);
  }

  /** DataChannel 上的握手消息（`open.ok` 之外的都是会话层的 `closed` 之类）。 */
  private handleDcControl(text: string): void {
    const value = safeJson(text);
    if (!value) return;
    if (value.t === "open.ok") {
      const pending = this.p2pPendingOpen;
      if (pending && Number(value.request ?? 0) === pending.request) {
        pending.settle(Number(value.session ?? 0) || null);
      }
      return;
    }
    if (value.t === "open.err") {
      this.p2pPendingOpen?.settle(null);
      return;
    }
    this.handleControl(text);
  }

  private p2pFailed(handles: P2pHandles, reason: string): void {
    if (this.p2p !== handles) return;
    const active = handles.active;
    this.dropP2p();
    if (active) this.emit("onP2pLost", reason);
  }

  private dropP2p(): void {
    const p2p = this.p2p;
    this.p2p = null;
    this.transport = "relay";
    if (this.p2pStatsTimer) {
      window.clearInterval(this.p2pStatsTimer);
      this.p2pStatsTimer = 0;
    }
    this.p2pSampleValue = { fps: 0, kbps: 0, rtt: 0, w: 0, h: 0 };
    const wait = this.p2pWait;
    this.p2pWait = null;
    wait?.(null);
    const pending = this.p2pPendingOpen;
    this.p2pPendingOpen = null;
    pending?.settle(null);
    if (p2p) {
      try {
        p2p.dc.close();
      } catch {
        // 已经关闭就忽略。
      }
      try {
        p2p.pc.close();
      } catch {
        // 已经关闭就忽略。
      }
    }
  }

  /** 每秒从 `getStats()` 取 P2P 的帧率 / 码率 / 往返延迟。 */
  private startP2pStats(pc: RTCPeerConnection): void {
    this.p2pBytes = 0;
    this.p2pSampleAt = performance.now();
    this.p2pStatsTimer = window.setInterval(() => {
      void pc
        .getStats()
        .then((stats) => {
          let bytes = 0;
          let fps = 0;
          let rtt = 0;
          let w = 0;
          let h = 0;
          stats.forEach((entry) => {
            const report = entry as RTCStats & {
              kind?: string;
              bytesReceived?: number;
              framesPerSecond?: number;
              frameWidth?: number;
              frameHeight?: number;
              state?: string;
              currentRoundTripTime?: number;
            };
            if (report.type === "inbound-rtp" && report.kind === "video") {
              bytes = report.bytesReceived ?? 0;
              fps = report.framesPerSecond ?? 0;
              w = report.frameWidth ?? 0;
              h = report.frameHeight ?? 0;
            } else if (
              report.type === "candidate-pair" &&
              report.state === "succeeded" &&
              report.currentRoundTripTime
            ) {
              rtt = Math.round(report.currentRoundTripTime * 1000);
            }
          });
          const now = performance.now();
          const seconds = Math.max(0.001, (now - this.p2pSampleAt) / 1000);
          const kbps = Math.round(((bytes - this.p2pBytes) * 8) / seconds / 1000);
          this.p2pBytes = bytes;
          this.p2pSampleAt = now;
          this.p2pSampleValue = {
            fps: Math.round(fps),
            kbps: Math.max(0, kbps),
            rtt: rtt || this.p2pSampleValue.rtt,
            w,
            h,
          };
        })
        .catch(() => undefined);
    }, 1000);
  }

  private handleControl(text: string): void {
    const value = safeJson(text);
    if (!value) return;
    switch (value.t) {
      case "devices": {
        const list = Array.isArray(value.devices) ? (value.devices as DeviceView[]) : [];
        this.emit("onDevices", list);
        break;
      }
      case "open.ok": {
        const session = Number(value.session ?? 0);
        const request = Number(value.request ?? 0);
        const pending = this.pendingOpen;
        if (pending && pending.request === request) {
          this.pendingOpen = null;
          this.session = session;
          this.emit("onOpen", session);
          pending.resolve(session);
        }
        break;
      }
      case "open.err": {
        const request = Number(value.request ?? 0);
        const code = Number(value.code ?? 0);
        const msg = String(value.msg ?? "设备拒绝了会话");
        if (this.pendingOpen?.request === request) {
          const pending = this.pendingOpen;
          this.pendingOpen = null;
          pending.reject(new Error(`${code}: ${msg}`));
        }
        this.emit("onOpenError", code, msg);
        break;
      }
      case "closed": {
        const code = Number(value.code ?? 4201);
        const msg = String(value.msg ?? "会话已关闭");
        if (code !== 0) this.emit("onClosed", code, msg);
        this.session = 0;
        break;
      }
      case "webrtc":
        this.handleP2pSignal(value);
        break;
      default:
        break;
    }
  }

  private async handleBinary(buffer: ArrayBuffer): Promise<void> {
    const bytes = new Uint8Array(buffer);
    const tunnel = decodeTunnel(bytes);
    if (!tunnel) return;
    if (tunnel.kind === TUNNEL_JSON) {
      const text = new TextDecoder().decode(tunnel.payload);
      const value = safeJson(text);
      if (value) this.emit("onMessage", value as Record<string, unknown>);
      return;
    }
    if (tunnel.kind !== TUNNEL_BINARY) return;
    const frame = decodeMediaFrame(tunnel.payload);
    if (frame) this.emit("onFrame", frame);
  }
}

function safeJson(text: string): Record<string, unknown> | null {
  try {
    const value = JSON.parse(text);
    return value && typeof value === "object" ? (value as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}
