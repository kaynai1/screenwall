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

  constructor(private readonly account: Account) {}

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
    const base = this.account.serverUrl.replace(/^http/i, "ws");
    const url = `${base}/ws/view?token=${encodeURIComponent(this.account.token)}`;
    return new Promise((resolve, reject) => {
      let settled = false;
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      this.socket = socket;

      socket.onopen = () => {};
      socket.onerror = () => {
        if (!settled) {
          settled = true;
          reject(new Error("无法连接服务器"));
        }
      };
      socket.onclose = (event) => {
        if (!settled) {
          settled = true;
          reject(new Error(`连接被服务器关闭（${event.code}）`));
        }
        this.pendingOpen?.reject(new Error("连接已断开"));
        this.pendingOpen = null;
        if (!this.closedByUs) this.emit("onSocketClosed", event.reason || `连接已断开（${event.code}）`);
      };
      socket.onmessage = (event) => {
        if (typeof event.data === "string") {
          this.handleControl(event.data);
          if (!settled) {
            const parsed = safeJson(event.data);
            if (parsed?.t === "ready") {
              settled = true;
              resolve();
            }
          }
          return;
        }
        void this.handleBinary(event.data as ArrayBuffer);
      };
    });
  }

  open(deviceId: string, mode: "control" | "wall" = "control"): Promise<number> {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("连接尚未就绪"));
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
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN || !this.session) return;
    socket.send(tunnelJson(this.session, message));
  }

  /** 请求被控端立刻出一个关键帧（切换 H.264 / 分辨率后用）。 */
  requestKeyframe(id: number): void {
    this.sendJson({ t: "video.keyframe", id });
  }

  /** 只关会话，保留到服务器的 WebSocket（从控制页返回设备列表时用）。 */
  closeSession(): void {
    const socket = this.socket;
    if (socket?.readyState === WebSocket.OPEN && this.session) {
      socket.send(JSON.stringify({ t: "close", session: this.session }));
    }
    this.session = 0;
  }

  close(): void {
    this.closedByUs = true;
    this.closeSession();
    const socket = this.socket;
    this.socket = null;
    socket?.close();
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
