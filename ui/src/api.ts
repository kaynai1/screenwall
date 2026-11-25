import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AccountView,
  DeviceView,
  DirectFirewallState,
  DirectState,
} from "./types";
import { mockAccount, mockDevices } from "./mock";

export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const DEMO_KEY = "screenwall.demo.loggedOut";
/** 浏览器演示模式：记住当前是不是连接码会话。 */
let demoQuick = false;

export type ControlInfo = {
  session: number;
  deviceId: string;
  request: number;
  /** 直连会回传本次会话的令牌；中转路径不带这个字段。 */
  clientToken?: number;
  /**
   * 握手后双方能力交集（`welcome.caps`）；`null` / 缺省表示尚未握手、能力未知。
   * 标识与 `crates/proto/src/caps.rs` 一致，例如 `audio.volume`。
   */
  caps?: string[] | null;
};

export type ControlPhase = "opening" | "streaming" | "closed" | "error";

/** 视频订阅参数（对应 VideoSub 的 fps / maxWidth / quality / display）。 */
export type StreamQuality = {
  fps: number;
  maxWidth: number;
  quality: number;
  /** 显示器索引；`DISPLAY_ALL` 表示整块虚拟桌面。 */
  display: number;
};

/** 与被控端 DisplayInfo 一一对应。 */
export type RemoteDisplay = {
  index: number;
  w: number;
  h: number;
  primary: boolean;
  x: number;
  y: number;
};

/** 多屏拼接（整块虚拟桌面）的哨兵值，与 Rust 端 DISPLAY_ALL 一致。 */
export const DISPLAY_ALL = 0xffffffff;

export type ControlDisplaysEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  displays: RemoteDisplay[];
};

/** 与 crates/proto 的 `InputEvent` 一一对应（tag = "k"，camelCase）。 */
export type InputEvent =
  | { k: "move"; abs?: [number, number]; rel?: [number, number] }
  | {
      k: "button";
      button: "left" | "right" | "middle" | "x1" | "x2";
      down: boolean;
      abs?: [number, number];
    }
  | { k: "wheel"; delta: [number, number] }
  | { k: "key"; scan: number; down: boolean; ext?: boolean }
  | { k: "text"; text: string }
  | { k: "releaseAll" };

export type ControlStateEvent = {
  phase: ControlPhase;
  deviceId: string;
  session: number;
  clientToken: number;
  message: string | null;
  width: number | null;
  height: number | null;
  fps: number | null;
  codec: string | null;
};

/** 握手完成时下发的双方能力交集；比控制页的订阅可能更早到达。 */
export type ControlCapsEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  caps: string[];
};

export type ControlFrameEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  stream: number;
  seq: number;
  timestampUs: number;
  width: number;
  height: number;
  fps: number;
  bytes: number;
  dataUrl: string;
};

export type ControlPowerEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  action: PowerAction;
};

/** SAS 实际发送方式：`sas` 为真正的安全注意序列，`keys` 为普通按键降级。 */
export type SasMethod = "sas" | "keys";

/** 被控端对 Ctrl+Alt+Del 的处理结果（`sas.ok`）。 */
export type ControlSasEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  method: SasMethod;
  detail?: string | null;
};

/** 被控端上报的 UIPI 输入拦截状态（`input.status`）。 */
export type ControlInputStatusEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  uipi: boolean;
  detail?: string | null;
};

export type ControlAudioEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  stream: number;
  seq: number;
  timestampUs: number;
  /** `opus` 是单个 20 ms Opus 包，`pcm16` 是交错小端 i16 PCM。 */
  codec: "opus" | "pcm16";
  sampleRate: number;
  channels: number;
  /** base64 编码的音频数据。 */
  data: string;
};

export type AudioSource = "loopback" | "microphone";

export type ControlAudioFormat = {
  deviceId: string;
  session: number;
  clientToken: number;
  stream: number;
  codec: "opus" | "pcm16";
  sampleRate: number;
  channels: number;
  bitrate: number;
  /** 实际采集来源；旧被控端缺省按系统声音处理。 */
  source?: AudioSource;
};

/** `audio.volume.ok`：被控端调整/读取后的实际音量与静音状态。 */
export type ControlVolumeEvent = {
  deviceId: string;
  session: number;
  clientToken: number;
  /** 0.0–1.0 */
  level: number;
  muted: boolean;
};

/**
 * StrictMode 会重复执行 effect。把 start/stop 串行化，
 * 保证「开始 → 停止 → 再开始」的顺序稳定，不会互相覆盖。
 */
let controlQueue: Promise<unknown> = Promise.resolve();

function enqueueControl<T>(task: () => Promise<T>): Promise<T> {
  const run = controlQueue.then(task, task);
  controlQueue = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 浏览器预览时默认停留在主界面，方便直接查看设计。 */
function demoLoggedIn(): boolean {
  if (typeof localStorage === "undefined") return true;
  return localStorage.getItem(DEMO_KEY) !== "1";
}

export async function getAccount(): Promise<AccountView> {
  if (isTauri) return invoke<AccountView>("get_account");
  await delay(120);
  // 演示模式用 localStorage 记住「本机模式」，刷新后不回登录页。
  const local =
    typeof localStorage !== "undefined" &&
    localStorage.getItem(DEMO_KEY) === "local";
  if (local) {
    return {
      ...mockAccount,
      serverUrl: "",
      username: "局域网模式",
      loggedIn: true,
      quick: false,
      local: true,
    };
  }
  return { ...mockAccount, loggedIn: demoLoggedIn(), quick: demoQuick };
}

export async function listDevices(): Promise<DeviceView[]> {
  if (isTauri) return invoke<DeviceView[]>("list_devices");
  await delay(120);
  // 连接码会话只有一台设备，演示模式也保持同样的形状。
  return demoQuick ? [mockDevices[0]] : mockDevices;
}

export async function login(input: {
  serverUrl: string;
  username: string;
  password: string;
  register: boolean;
}): Promise<AccountView> {
  if (isTauri) {
    return invoke<AccountView>("login", {
      serverUrl: input.serverUrl,
      username: input.username,
      password: input.password,
      register: input.register,
    });
  }
  await delay(650);
  demoQuick = false;
  if (typeof localStorage !== "undefined") {
    localStorage.removeItem(DEMO_KEY);
  }
  return {
    ...mockAccount,
    serverUrl: input.serverUrl || mockAccount.serverUrl,
    username: input.username || mockAccount.username,
    loggedIn: true,
    quick: false,
  };
}

// ---------------------------------------------------------------- 本机模式（无服务器）

/**
 * 不连服务器，只用局域网直连。
 *
 * 这是「点对点」的默认形态：两台机器在同一网段，一台显示配对码，
 * 另一台输地址和码就能连，全程没有账号、没有云端中转。
 */
export async function enterLocalMode(): Promise<AccountView> {
  if (isTauri) return invoke<AccountView>("enter_local_mode");
  await delay(400);
  demoQuick = false;
  if (typeof localStorage !== "undefined") {
    localStorage.setItem(DEMO_KEY, "local");
  }
  return {
    ...mockAccount,
    serverUrl: "",
    username: "本机模式",
    loggedIn: true,
    quick: false,
    local: true,
  };
}

// ---------------------------------------------------------------- 连接码（ID + 密码）

export type ConnectCodeView = {
  code: string;
  enabled: boolean;
  hasPassword: boolean;
  /** 本机保存的明文密码；别的机器登录时可能为空。 */
  password: string | null;
};

/** 不登录账号，直接用 9 位连接码 + 密码连接一台设备。 */
export async function quickConnect(input: {
  serverUrl: string;
  code: string;
  password: string;
}): Promise<AccountView> {
  if (isTauri) {
    return invoke<AccountView>("quick_connect", input);
  }
  await delay(600);
  demoQuick = true;
  if (typeof localStorage !== "undefined") {
    localStorage.removeItem(DEMO_KEY);
  }
  return {
    ...mockAccount,
    serverUrl: input.serverUrl || mockAccount.serverUrl,
    username: "连接码会话",
    loggedIn: true,
    quick: true,
  };
}

export async function quickDisconnect(): Promise<AccountView> {
  if (isTauri) return invoke<AccountView>("quick_disconnect");
  demoQuick = false;
  return { ...mockAccount, loggedIn: false, quick: false };
}

export async function getConnectCode(): Promise<ConnectCodeView> {
  if (isTauri) return invoke<ConnectCodeView>("get_connect_code");
  return {
    code: "572955600",
    enabled: true,
    hasPassword: true,
    password: "926454",
  };
}

export async function setConnectCode(
  enabled: boolean,
  password?: string,
): Promise<ConnectCodeView> {
  if (isTauri) {
    return invoke<ConnectCodeView>("set_connect_code", { enabled, password });
  }
  return {
    code: "572955600",
    enabled,
    hasPassword: true,
    password: password || "926454",
  };
}

export async function rotateConnectCode(): Promise<ConnectCodeView> {
  if (isTauri) return invoke<ConnectCodeView>("rotate_connect_code");
  return {
    code: String(Math.floor(100000000 + Math.random() * 900000000)),
    enabled: true,
    hasPassword: true,
    password: "926454",
  };
}

// ---------------------------------------------------------------- 一次性连接链接

export type ConnectLinkView = {
  /** 完整链接，形如 `http://host/l/<token>`。 */
  url: string;
  expiresAt: number;
  expiresInSec: number;
};

export type ConnectLinkInput = {
  server: string;
  token: string;
};

/** 本机生成一条一次性连接链接（10 分钟有效、只能用一次）。 */
export async function createConnectLink(
  deviceId?: string,
): Promise<ConnectLinkView> {
  if (isTauri) return invoke<ConnectLinkView>("create_connect_link", { deviceId });
  await delay(300);
  return {
    url: `${mockAccount.serverUrl}/l/${Math.random().toString(16).slice(2, 34)}`,
    expiresAt: Date.now() + 10 * 60 * 1000,
    expiresInSec: 600,
  };
}

/** 用一次性链接令牌直接连入（无需密码）。 */
export async function connectByLink(
  input: ConnectLinkInput,
): Promise<AccountView> {
  if (isTauri) {
    return invoke<AccountView>("connect_by_link", {
      serverUrl: input.server,
      token: input.token,
    });
  }
  await delay(600);
  demoQuick = true;
  if (typeof localStorage !== "undefined") {
    localStorage.removeItem(DEMO_KEY);
  }
  return {
    ...mockAccount,
    serverUrl: input.server || mockAccount.serverUrl,
    username: "连接码会话",
    loggedIn: true,
    quick: true,
  };
}

/** 启动时通过 `screenwall://` 唤起：取走待处理的链接。 */
export async function takeConnectLink(): Promise<ConnectLinkInput | null> {
  if (!isTauri) return null;
  return invoke<ConnectLinkInput | null>("take_connect_link");
}

/** 运行中再次点开链接：由 Rust 侧 emit。 */
export function subscribeConnectLink(
  handler: (link: ConnectLinkInput) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ConnectLinkInput>("connect-link", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export async function logout(): Promise<AccountView> {
  if (isTauri) return invoke<AccountView>("logout");
  await delay(260);
  demoQuick = false;
  if (typeof localStorage !== "undefined") {
    localStorage.setItem(DEMO_KEY, "1");
  }
  return { ...mockAccount, loggedIn: false, quick: false };
}

export function subscribeAccount(
  handler: (account: AccountView) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<AccountView>("account", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeDevices(
  handler: (devices: DeviceView[]) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<DeviceView[]>("devices", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function startControl(
  deviceId: string,
  clientToken: number,
): Promise<ControlInfo> {
  if (!isTauri) {
    return enqueueControl(async () => {
      await delay(420);
      return { session: 1, deviceId, request: 1 };
    });
  }
  return enqueueControl(() =>
    invoke<ControlInfo>("start_control", { deviceId, clientToken }),
  );
}

export function stopControl(clientToken: number): Promise<void> {
  if (!isTauri) return Promise.resolve();
  return enqueueControl(() =>
    invoke<void>("stop_control", { clientToken }),
  );
}

/**
 * 在现有控制会话内切换视频订阅参数（不重开会话）。
 * 失败时 reject，由调用方回滚 UI 选择。
 */
export function setStreamOptions(
  clientToken: number,
  options: StreamQuality,
): Promise<void> {
  if (!isTauri) return delay(360);
  return invoke<void>("set_stream_options", {
    clientToken,
    fps: options.fps,
    maxWidth: options.maxWidth,
    quality: options.quality,
    display: options.display,
  });
}

export type PowerAction = "restart" | "shutdown";

/** 让被控端重启或关机；被控端会在 3 秒后执行。 */
export function sendPower(
  clientToken: number,
  action: PowerAction,
): Promise<void> {
  if (!isTauri) return delay(240);
  return invoke<void>("send_power", { clientToken, action });
}

/**
 * 请求被控端发送 Ctrl+Alt+Del。
 * 结果从 `control-sas` 事件回来（可能降级为普通按键）。
 */
export function sendSecureAttention(clientToken: number): Promise<void> {
  if (!isTauri) return delay(160);
  return invoke<void>("send_secure_attention", { clientToken });
}

/** 把当前画面保存到本机 Downloads\ScreenWall，返回落盘路径。 */
export async function saveScreenshot(dataUrl: string): Promise<string> {
  if (!isTauri) return "Downloads\\ScreenWall\\ScreenShot-demo.jpg";
  return invoke<string>("save_screenshot", { dataUrl });
}

/** 修改设备名（服务器持久化，所有端同步）。 */
export function renameDevice(deviceId: string, name: string): Promise<void> {
  if (!isTauri) return delay(200);
  return invoke<void>("rename_device", { deviceId, name });
}

/** 允许/禁止别人控制这台设备。 */
export function setDeviceControllable(
  deviceId: string,
  controllable: boolean,
): Promise<void> {
  if (!isTauri) return delay(200);
  return invoke<void>("set_device_controllable", { deviceId, controllable });
}

/** 主动向服务器拉一次设备列表。 */
export function refreshDevices(): Promise<DeviceView[]> {
  if (!isTauri) return delay(240).then(() => mockDevices);
  return invoke<DeviceView[]>("refresh_devices");
}

/** 从账号里移除设备（同时吊销它的令牌）。 */
export function deleteDevice(deviceId: string): Promise<void> {
  if (!isTauri) return delay(260);
  return invoke<void>("delete_device", { deviceId });
}

/** 打开/关闭被控端声音；`source` 选系统声音或麦克风。 */
export function setAudioEnabled(
  clientToken: number,
  enabled: boolean,
  codec?: "opus" | "pcm16",
  source?: AudioSource,
): Promise<void> {
  if (!isTauri) return delay(180);
  return invoke<void>("set_audio_enabled", {
    clientToken,
    enabled,
    codec,
    source,
  });
}

/**
 * 由本机网卡发一次 WOL magic packet。
 * 被控端离线时没有会话可用，只能由同网段的主控机广播。
 */
export function wakeOnLan(mac: string, broadcast?: string): Promise<void> {
  if (!isTauri) return delay(300);
  return invoke<void>("wake_on_lan", { mac, broadcast });
}

/**
 * 请另一台在线设备代发 WOL magic packet（异地开机）。
 * 代发设备会弹出授权请求，需要在那台机器上点「允许」。
 */
export function wakeViaDevice(
  helperDeviceId: string,
  mac: string,
  broadcast?: string,
): Promise<void> {
  if (!isTauri) return delay(600);
  return invoke<void>("wake_via_device", {
    helperDeviceId,
    mac,
    broadcast,
  });
}

/** 读取被控端当前音量，结果从 `control-volume` 事件回来。 */
export function getRemoteVolume(clientToken: number): Promise<void> {
  if (!isTauri) return delay(80);
  return invoke<void>("get_remote_volume", { clientToken });
}

/** 调整被控端音量 / 静音，结果从 `control-volume` 事件回来。 */
export function setRemoteVolume(
  clientToken: number,
  volume: { level?: number; muted?: boolean },
): Promise<void> {
  if (!isTauri) return delay(80);
  return invoke<void>("set_remote_volume", {
    clientToken,
    level: volume.level,
    muted: volume.muted,
  });
}

/** 把播放缓冲深度回报给被控端，驱动 Opus 码率自适应。 */
export async function reportAudioStats(
  clientToken: number,
  id: number,
  bufferedMs: number,
): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("report_audio_stats", {
      clientToken,
      id,
      bufferedMs: Math.max(0, Math.round(bufferedMs)),
    });
  } catch {
    // 统计是尽力而为，失败不影响播放。
  }
}

/**
 * 把一批键鼠事件交给主控端转发给被控端。
 * 预览模式下直接忽略；发送失败只记录日志，不打断界面。
 */
export async function sendControlInput(
  clientToken: number,
  events: InputEvent[],
): Promise<void> {
  if (events.length === 0) return;
  if (!isTauri) return;
  try {
    await invoke<void>("send_control_input", { clientToken, events });
  } catch (error) {
    console.warn("发送远程输入事件失败", error);
  }
}

export function subscribeControlState(
  handler: (state: ControlStateEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlStateEvent>("control-state", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/**
 * 订阅握手结果里的能力交集。
 * 控制页据此直接决定可选功能是否可用，不必先发一条消息等报错。
 */
export function subscribeControlCaps(
  handler: (event: ControlCapsEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlCapsEvent>("control-caps", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlDisplays(
  handler: (event: ControlDisplaysEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlDisplaysEvent>("control-displays", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlFrame(
  handler: (frame: ControlFrameEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlFrameEvent>("control-frame", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlAudio(
  handler: (audio: ControlAudioEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlAudioEvent>("control-audio", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** `audio.sub.ok` 的实际编码、采样率与码率。 */
export function subscribeControlAudioFormat(
  handler: (format: ControlAudioFormat) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlAudioFormat>("control-audio-format", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 被控端没有可用的扬声器/回环设备时收到；不影响画面。 */
export function subscribeControlAudioError(
  handler: (message: string) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<string>("control-audio-error", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 被控端不支持音量消息（旧版本）时收到；不影响画面。 */
export function subscribeControlVolumeError(
  handler: (message: string) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<string>("control-volume-error", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 被控端回报的音量/静音状态。 */
export function subscribeControlVolume(
  handler: (event: ControlVolumeEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlVolumeEvent>("control-volume", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlPowerOk(
  handler: (event: ControlPowerEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlPowerEvent>("control-power-ok", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlPowerError(
  handler: (message: string) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<string>("control-power-error", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 被控端对 Ctrl+Alt+Del 的处理结果。 */
export function subscribeControlSas(
  handler: (event: ControlSasEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlSasEvent>("control-sas", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 被控端输入被 UIPI 拦截/恢复时推送。 */
export function subscribeControlInputStatus(
  handler: (event: ControlInputStatusEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlInputStatusEvent>("control-input-status", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 会话连接质量（`get_connection_quality` 与 `control-stats.quality` 共用）。 */
export type ConnectionQuality = {
  video: {
    /** 收到的视频帧数（重复/迟到帧不计）。 */
    frames: number;
    /** 按 seq 缺口推算的丢帧数。 */
    lost: number;
    /** 最近 5 秒平均帧率。 */
    fps: number;
    /** 最近 5 秒平均码率（kbps）。 */
    kbps: number;
    /** 本次会话内相邻两帧的最大到达间隔（毫秒）。 */
    maxGapMs: number;
  };
  audio: {
    /** 收到的音频包数。 */
    packets: number;
    /** 按包序号缺口推算的丢包数。 */
    lost: number;
    /** 相邻两包解码时间戳之差 > 40 ms 的次数。 */
    gaps: number;
  };
  net: {
    /** 本次控制会话内自动重连成功的次数。 */
    reconnects: number;
    /** 最近一次隧道 Ping 的往返时延。 */
    rttMs: number | null;
  };
  /** 会话开始时刻（Unix 毫秒）。 */
  since: number;
};

/** 重连进度（`control-stats.reconnecting` 与 `get_reconnect_state` 共用）。 */
export type ReconnectProgress = {
  attempt: number;
  maxAttempts: number;
  delayMs: number;
};

/** 采集端每秒推送一次的链路统计：实测帧率、隧道往返时延、码率、自适应开关。 */
export type ControlStats = {
  measuredFps: number;
  targetFps: number;
  rttMs: number | null;
  kbps: number;
  auto: boolean;
  /** 会话累计质量；旧后端不返回时为 undefined。 */
  quality?: ConnectionQuality;
  /** 正在自动重连时的进度；未在重连时为 null/undefined。 */
  reconnecting?: ReconnectProgress | null;
};

/** 断线自动重连：每轮尝试前推送（`control-reconnecting`）。 */
export type ControlReconnectingEvent = {
  clientToken: number;
  deviceId: string;
  attempt: number;
  maxAttempts: number;
  delayMs: number;
};

/** 重连成功（`control-reconnected`）或放弃（`control-reconnect-failed`）。 */
export type ControlReconnectOutcome = {
  clientToken: number;
  deviceId: string;
  /** 成功时为恢复后的新会话号；失败时为 0。 */
  session: number;
  /** 成功时为第几次尝试成功；失败时为总尝试次数。 */
  attempt: number;
  /** 失败原因（成功时为 null）。 */
  message: string | null;
};

export function subscribeControlReconnecting(
  handler: (event: ControlReconnectingEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlReconnectingEvent>("control-reconnecting", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeControlReconnected(
  handler: (event: ControlReconnectOutcome) => void,
): () => void {
  return listenOutcome("control-reconnected", handler);
}

export function subscribeControlReconnectFailed(
  handler: (event: ControlReconnectOutcome) => void,
): () => void {
  return listenOutcome("control-reconnect-failed", handler);
}

function listenOutcome(
  name: string,
  handler: (event: ControlReconnectOutcome) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlReconnectOutcome>(name, (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 当前会话的连接质量；失败说明会话已经不存在。 */
export async function getConnectionQuality(
  clientToken: number,
): Promise<ConnectionQuality> {
  if (!isTauri) {
    // 演示模式：给一份自洽的假数据，界面结构不变。
    return {
      video: { frames: 0, lost: 0, fps: 0, kbps: 0, maxGapMs: 0 },
      audio: { packets: 0, lost: 0, gaps: 0 },
      net: { reconnects: 0, rttMs: null },
      since: Date.now(),
    };
  }
  return invoke<ConnectionQuality>("get_connection_quality", { clientToken });
}

export function subscribeControlStats(
  handler: (stats: ControlStats) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlStats>("control-stats", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export async function setAutoQuality(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_auto_quality", { enabled });
  return enabled;
}

export async function getAutoQuality(): Promise<boolean> {
  if (isTauri) return invoke<boolean>("get_auto_quality");
  return true;
}

/** 被控端收到的一条「请求控制」：需要在 30 秒内本机确认。 */
export type ControlRequest = {
  session: number;
  viewer: string;
  timeoutSec: number;
};

export function subscribeControlRequest(
  handler: (request: ControlRequest) => void,
): () => void {
  if (!isTauri) {
    // 浏览器预览没有后端：用 window 事件模拟一条被控请求，方便看弹窗。
    const onDemo = (event: Event) => {
      const detail = (event as CustomEvent<ControlRequest>).detail;
      if (detail) handler(detail);
    };
    window.addEventListener("screenwall:control-request", onDemo);
    return () => window.removeEventListener("screenwall:control-request", onDemo);
  }
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<ControlRequest>("control-request", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 窗口挂载前可能已经错过事件，启动时补一次待确认列表。 */
export async function listControlRequests(): Promise<ControlRequest[]> {
  if (!isTauri) return [];
  try {
    return await invoke<ControlRequest[]>("list_control_requests");
  } catch {
    return [];
  }
}

export async function answerControlRequest(
  session: number,
  allow: boolean,
): Promise<void> {
  if (!isTauri) return;
  await invoke("answer_control_request", { session, allow });
}

/** 本机偏好（保存在 client.json）。 */
export type LocalSettings = {
  allowControl: boolean;
  uiZoom: number;
  closeToTray?: boolean;
  /** 自定义 CA 根证书路径；null 表示使用系统默认信任库。 */
  caPath?: string | null;
  /** 无头机器上是否允许临时接入虚拟显示器。 */
  virtualDisplay?: boolean;
};

export async function getLocalSettings(): Promise<LocalSettings> {
  if (!isTauri) return { allowControl: true, uiZoom: 1 };
  try {
    return await invoke<LocalSettings>("get_local_settings");
  } catch {
    return { allowControl: true, uiZoom: 1 };
  }
}

export async function setAllowControl(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_allow_control", { enabled });
  return enabled;
}

/** 无头机器上临时接入虚拟显示器（首次需要 UAC 安装驱动）。 */
export async function setVirtualDisplay(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_virtual_display", { enabled });
  return enabled;
}

export async function setUiZoom(zoom: number): Promise<number> {
  if (isTauri) return invoke<number>("set_ui_zoom", { zoom });
  return zoom;
}

/** 真请求 `/healthz`，返回往返毫秒。 */
export async function testServer(url: string): Promise<number> {
  if (isTauri) return invoke<number>("test_server", { url });
  return 12;
}

export async function getAutostart(): Promise<boolean> {
  if (!isTauri) return false;
  try {
    return await invoke<boolean>("get_autostart");
  } catch {
    return false;
  }
}

export async function setAutostart(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_autostart", { enabled });
  return enabled;
}

// ---------------------------------------------------------------- 文件传输

/** 被控端推送的文件传输事件（上传/下载共用）。 */
export type FileTransferEvent = {
  tid: number;
  kind:
    | "start"
    | "accepted"
    | "progress"
    | "file-done"
    | "complete"
    | "cancelled"
    | "error"
    /** 整目录下载：一组任务已展开完毕（files 为文件总数）。 */
    | "directory"
    /** 单个下载任务进入队列，等待并发空位。 */
    | "queued"
    /** 下载数据已收齐，正在比对 SHA256 并落盘。 */
    | "verifying";
  direction?: "upload" | "download";
  name?: string;
  path?: string;
  root?: string;
  done?: number;
  total?: number;
  files?: number;
  ok?: boolean;
  msg?: string | null;
  /** 上传时被控端已持有的续传位置；下载 start 事件里是续传起点字节数。 */
  resume?: FileResume[] | number;
  resumed?: number;
  /** 下载完成时本机算出的 SHA256，以及是否与发送端一致。 */
  sha256?: string;
  verified?: boolean;
  /** 整目录下载的分组号；该组每个文件的任务事件都带同一个 group。 */
  group?: number;
  /** `directory` 事件里展开出的空目录数量。 */
  emptyDirs?: number;
  /** `directory` 事件里目录为空（没有文件也没有子目录）。 */
  empty?: boolean;
  /** `complete` 事件里的 `directory: true` 表示这是整目录收尾（此时 tid 可能为 0）。 */
  directory?: boolean;
  /** `queued` 事件里的文件大小（与 total 同值，保留兼容）。 */
  size?: number;
};

export function subscribeFileTransfer(
  handler: (event: FileTransferEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<FileTransferEvent>("file-transfer", (event) =>
    handler(event.payload),
  )
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export type FileResume = { item: number; offset: number };

export type UploadResult = {
  tid: number;
  root: string;
  total: number;
  resume: FileResume[];
};

export async function offerFiles(
  items: { path: string; size: number; sha256: string }[],
): Promise<UploadResult> {
  if (isTauri) return invoke<UploadResult>("offer_files", { items });
  return { tid: 1, root: "C:\Users\demo\Downloads\ScreenWall", total: 0, resume: [] };
}

/** `data` 是 base64 切片，每块不超过 256 KiB 原始字节。 */
export async function uploadChunk(
  tid: number,
  item: number,
  offset: number,
  data: string,
): Promise<void> {
  if (isTauri) {
    await invoke("upload_chunk", { tid, item, offset, data });
  }
}

export type RemoteEntry = {
  name: string;
  path: string;
  dir: boolean;
  size: number;
  modified?: number | null;
};

export type RemoteListing = {
  tid: number;
  path: string;
  parent?: string | null;
  entries: RemoteEntry[];
};

export async function listRemoteFiles(path: string): Promise<RemoteListing> {
  if (isTauri) return invoke<RemoteListing>("list_remote_files", { path });
  const root = path || "C:\Users\demo";
  return {
    tid: 1,
    path: root,
    parent: root === "C:\Users\demo" ? null : "C:\Users\demo",
    entries: [
      { name: "Documents", path: `${root}\Documents`, dir: true, size: 0 },
      { name: "report.pdf", path: `${root}\report.pdf`, dir: false, size: 2411724 },
      { name: "design.png", path: `${root}\design.png`, dir: false, size: 918273 },
    ],
  };
}

export type DownloadResult = {
  tid: number;
  name: string;
  /** 任务刚入队时后端还不知道文件大小，可能为 0，以 `start` 事件为准。 */
  size: number;
  path: string;
  /** 本次从哪个字节开始；大于 0 表示命中了断点续传。 */
  resume?: number;
};

/** 开始一个下载任务；并发已满时后端会先排队，`start` 事件在真正开始时才推送。 */
export async function downloadRemoteFile(
  path: string,
): Promise<DownloadResult> {
  if (isTauri) return invoke<DownloadResult>("download_remote_file", { path });
  const name = path.split(/[\/]/).pop() || "file";
  return {
    tid: 2,
    name,
    size: 2411724,
    path: `C:\Users\demo\Downloads\ScreenWall\${name}`,
  };
}

export type DirectoryDownloadResult = {
  /** 整组下载的分组号；该组每个文件的事件都带同一个 group。 */
  group: number;
  name: string;
  /** 展开得到的文件数（空目录不算）。 */
  files: number;
  emptyDirs: number;
  /** 目录里没有文件也没有子目录。 */
  empty: boolean;
};

/**
 * 下载整个远程目录：后端递归展开后逐个排队下载，
 * 文件落在 `Downloads\ScreenWall\<目录名>\...` 并保留目录结构。
 */
export async function downloadRemoteDirectory(
  path: string,
): Promise<DirectoryDownloadResult> {
  if (isTauri) return invoke<DirectoryDownloadResult>("download_remote_directory", { path });
  const name = path.split(/[\/\\]/).filter(Boolean).pop() || "folder";
  return { group: 3, name, files: 2, emptyDirs: 1, empty: false };
}

export async function cancelFileTransfer(tid: number): Promise<void> {
  if (isTauri) await invoke("cancel_file_transfer", { tid });
}

/** 当前远程会话；没有会话时返回 null。 */
export type ActiveControl = {
  deviceId: string;
  session: number;
  clientToken: number;
  /** 握手后的能力交集；尚未握手时为 `null`。 */
  caps?: string[] | null;
};

export async function getControlInfo(): Promise<ActiveControl | null> {
  if (!isTauri) return { deviceId: "demo-device", session: 1, clientToken: 1 };
  try {
    const info = await invoke<ActiveControl | null>("get_control_info");
    return info ?? null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------- 局域网直连

/** 演示模式下直连的默认状态：已开启、正在监听。 */
const DEMO_DIRECT: DirectState = {
  enabled: true,
  listening: true,
  port: 47900,
  addresses: ["192.168.1.20"],
  localId: "572955600",
  pairingCode: "428913",
  pairingExpiresIn: 512,
  activeSessions: 0,
  trustedPeers: 2,
};

/**
 * 演示模式下开关的状态。
 *
 * 必须记在模块里：远程协助页每 5 秒轮询一次 `getDirectState`，如果这里
 * 永远返回默认值，用户刚关掉的开关会在下一次轮询时自己跳回去。
 */
let demoDirect: DirectState = { ...DEMO_DIRECT };

export async function getDirectState(): Promise<DirectState> {
  if (isTauri) return invoke<DirectState>("get_direct_state");
  await delay(120);
  return demoDirect;
}

export async function setDirectEnabled(enabled: boolean): Promise<DirectState> {
  if (isTauri) return invoke<DirectState>("set_direct_enabled", { enabled });
  await delay(160);
  demoDirect = {
    ...demoDirect,
    enabled,
    listening: enabled,
    pairingCode: enabled ? DEMO_DIRECT.pairingCode : null,
    pairingExpiresIn: enabled ? DEMO_DIRECT.pairingExpiresIn : 0,
  };
  return demoDirect;
}

export async function regeneratePairingCode(): Promise<DirectState> {
  if (isTauri) return invoke<DirectState>("regenerate_pairing_code");
  await delay(160);
  // 每次生成一个新码，演示「用过即换」的行为。
  const code = String(Math.floor(100000 + Math.random() * 900000));
  demoDirect = { ...demoDirect, pairingCode: code, pairingExpiresIn: 600 };
  return demoDirect;
}

/** 清空「允许直连本机」的长期授权，所有配对过的设备都要重新输配对码。 */
export async function forgetDirectPeers(): Promise<DirectState> {
  if (isTauri) return invoke<DirectState>("forget_direct_peers");
  await delay(160);
  demoDirect = { ...demoDirect, trustedPeers: 0 };
  return demoDirect;
}

/** 演示模式：假装安装器已经加好规则。 */
const DEMO_FIREWALL: DirectFirewallState = {
  rulePresent: true,
  ruleMatches: true,
  ruleName: "ScreenWall Direct",
  port: 47900,
};

/** 读 Windows 防火墙里 `ScreenWall Direct` 规则的状态（只读）。 */
export async function getDirectFirewallState(): Promise<DirectFirewallState> {
  if (isTauri) return invoke<DirectFirewallState>("get_direct_firewall_state");
  await delay(80);
  return DEMO_FIREWALL;
}

/**
 * 补防火墙规则：规则缺失或指向旧路径时调用，会弹一次管理员授权（UAC）。
 * 不用每次开直连都点——安装器已经加好规则，这里只是兜底。
 */
export async function allowDirectFirewall(): Promise<DirectFirewallState> {
  if (isTauri) return invoke<DirectFirewallState>("allow_direct_firewall");
  await delay(600);
  return DEMO_FIREWALL;
}

/** 同网段发现到的一台开启了直连的设备。 */
export type DiscoveredDevice = {
  /** 形如 `192.168.1.20:47900`。 */
  address: string;
  name: string;
  port: number;
  fingerprint: string;
  /** 对方识别码（9 位数字）；老版本设备为空。 */
  id?: string;
};

/**
 * 按识别码在局域网里找设备。
 *
 * 只发一次 UDP 广播、等约 1.2 秒；找不到会抛错，不会退回到「随便连一台」。
 */
export async function resolveDirectId(id: string): Promise<DiscoveredDevice> {
  if (!isTauri) {
    await delay(700);
    return { address: "192.168.1.31:47900", name: "DESKTOP-7Q2", port: 47900, fingerprint: "", id };
  }
  return invoke<DiscoveredDevice>("resolve_direct_id", { id });
}

/**
 * 扫描同网段开启了局域网直连的设备。
 * 走 UDP 广播，约 1.2 秒后返回；没有设备时返回空数组。
 */
export async function scanDirectDevices(): Promise<DiscoveredDevice[]> {
  if (!isTauri) {
    await delay(600);
    return [
      { address: "192.168.1.31:47900", name: "DESKTOP-7Q2", port: 47900, fingerprint: "" },
    ];
  }
  return invoke<DiscoveredDevice[]>("scan_direct_devices");
}

/** 设置自定义 CA 根证书路径（文件或目录）；`null` 表示恢复默认信任库。 */
export async function setCaPath(path: string | null): Promise<string | null> {
  if (!isTauri) return path;
  return invoke<string | null>("set_ca_path", { path });
}

/** 直连一台同网段设备；返回与控制页共用的 ControlInfo。 */
export async function connectDirect(input: {
  host: string;
  port?: number;
  pairingCode?: string;
}): Promise<ControlInfo> {
  if (isTauri) return invoke<ControlInfo>("connect_direct", { input });
  await delay(520);
  return { session: 1, deviceId: "direct-peer", request: 1, clientToken: 1 };
}

/**
 * 把直连设备的合成 ID（`direct:<地址>:<端口>`）拆回地址。
 * 与控制页的重连按钮共用；解析不了返回 `null`。
 */
export function parseDirectDeviceId(
  deviceId: string,
): { host: string; port?: number } | null {
  // 与 `isDirectDeviceId` 保持一致：`direct:` 是后端合成 ID，`direct-` 是演示模式。
  const rest = deviceId.startsWith("direct:")
    ? deviceId.slice("direct:".length)
    : deviceId.startsWith("direct-")
      ? deviceId.slice("direct-".length)
      : null;
  if (!rest) return null;
  // 地址里可能有 IPv6 方括号，按最后一个冒号切端口。
  const cut = rest.lastIndexOf(":");
  if (cut <= 0) return { host: rest };
  const port = Number(rest.slice(cut + 1));
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null;
  return { host: rest.slice(0, cut), port };
}

/**
 * 重连一台直连设备：地址从设备 ID 解析，不带配对码。
 *
 * 配对码是一次性的、10 分钟过期，重连必须复用后端 `known_peers` 里的
 * 长期 token —— 后端 `connect_direct` 在 `pairingCode` 为空时正是这么做的。
 */
export function reconnectDirect(deviceId: string): Promise<ControlInfo> {
  const target = parseDirectDeviceId(deviceId);
  if (!target) return Promise.reject(new Error("直连地址无效，请返回后重新连接"));
  return connectDirect(target);
}

export async function getClipboardSync(): Promise<boolean> {
  if (!isTauri) return true;
  try {
    return await invoke<boolean>("get_clipboard_sync");
  } catch {
    return true;
  }
}

export async function setClipboardSync(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_clipboard_sync", { enabled });
  return enabled;
}

// ---------------------------------------------------------------- 屏幕墙缩略图

/** 单台设备最 1 FPS 的缩略图帧（JPEG data URL）。 */
export type WallFrameEvent = {
  deviceId: string;
  width: number;
  height: number;
  dataUrl: string;
};

export type WallPreviewState = "connecting" | "live" | "closed" | "error";

export type WallStateEvent = {
  deviceId: string;
  state: WallPreviewState;
  message: string | null;
};

/** 被控端 `status.stats` 的真实整机指标，随屏幕墙预览一起推。 */
export type WallMetricsEvent = {
  deviceId: string;
  /** 整机 CPU 占用百分比。 */
  cpu: number;
  /** 整机内存占用百分比。 */
  mem: number;
  /** 该预览会话的出流码率（kbps）。 */
  kbps: number;
};

/**
 * 缩略图订阅同样要串行：StrictMode 会「挂载 → 清理 → 再挂载」，
 * 并发执行会让 stop 抢在最后的 set 之后，导致预览整页空白。
 */
let wallQueue: Promise<unknown> = Promise.resolve();

function enqueueWall<T>(task: () => Promise<T>): Promise<T> {
  const run = wallQueue.then(task, task);
  wallQueue = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

/** 告知后端屏幕墙当前需要预览哪些设备（多退少补）。 */
export function setWallPreview(deviceIds: string[]): Promise<void> {
  if (!isTauri) return Promise.resolve();
  return enqueueWall(() =>
    invoke<void>("set_wall_preview", { deviceIds }),
  );
}

/** 离开屏幕墙：断开全部缩略图会话。 */
export function stopWallPreview(): Promise<void> {
  if (!isTauri) return Promise.resolve();
  return enqueueWall(() => invoke<void>("stop_wall_preview"));
}

export function subscribeWallFrame(
  handler: (frame: WallFrameEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<WallFrameEvent>("wall-frame", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** 订阅屏幕墙预览会话的实时指标（CPU / 内存 / 码率）。 */
export function subscribeWallMetrics(
  handler: (event: WallMetricsEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<WallMetricsEvent>("wall-metrics", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeWallState(
  handler: (state: WallStateEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<WallStateEvent>("wall-state", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

// ---------------------------------------------------------------- 群控

/** 群控画面档位：低帧率缩略流，键鼠输入另走同一会话。 */
export type GroupQuality = { fps: number; maxWidth: number; quality: number };

export type GroupMemberPhase = "connecting" | "live" | "closed" | "error";

export type GroupStateEvent = {
  token: number;
  deviceId: string;
  state: GroupMemberPhase;
  message: string | null;
  width: number;
  height: number;
};

export type GroupFrameEvent = {
  token: number;
  deviceId: string;
  session: number;
  seq: number;
  width: number;
  height: number;
  dataUrl: string;
};

/**
 * 开始 / 调整群控：`deviceIds` 是当前全部群控设备，后端多退少补。
 * 被控端会为每台设备弹一次授权，全部同意后才有画面。
 */
let groupQueue: Promise<unknown> = Promise.resolve();

function enqueueGroup<T>(task: () => Promise<T>): Promise<T> {
  const run = groupQueue.then(task, task);
  groupQueue = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

export function startGroup(
  deviceIds: string[],
  options: GroupQuality,
): Promise<number> {
  if (!isTauri) return Promise.resolve(deviceIds.length > 0 ? 1 : 0);
  return enqueueGroup(() =>
    invoke<number>("start_group", {
      deviceIds,
      fps: options.fps,
      maxWidth: options.maxWidth,
      quality: options.quality,
    }),
  );
}

/** 运行中切换画质：后端让所有已建立会话重新订阅。 */
export function setGroupOptions(
  token: number,
  options: GroupQuality,
): Promise<void> {
  if (!isTauri) return Promise.resolve();
  return enqueueGroup(() =>
    invoke<void>("set_group_options", {
      token,
      fps: options.fps,
      maxWidth: options.maxWidth,
      quality: options.quality,
    }),
  );
}

export function stopGroup(token?: number | null): Promise<void> {
  if (!isTauri) return Promise.resolve();
  return enqueueGroup(() => invoke<void>("stop_group", { token: token ?? null }));
}

/**
 * 广播一批键鼠事件。`target` 为空时发给所有已连接设备，
 * 指定设备时只发给那一台（关闭「广播输入」时使用）。
 */
export async function groupInput(
  token: number,
  events: InputEvent[],
  target?: string | null,
): Promise<void> {
  if (events.length === 0 || !isTauri) return;
  try {
    await invoke<void>("group_input", {
      token,
      events,
      target: target ?? null,
    });
  } catch (error) {
    console.warn("发送群控输入失败", error);
  }
}

export function subscribeGroupState(
  handler: (state: GroupStateEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<GroupStateEvent>("group-state", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function subscribeGroupFrame(
  handler: (frame: GroupFrameEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<GroupFrameEvent>("group-frame", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}

// ---------------------------------------------------------------- 服务器管理面板

export type PanelStatus = {
  available: boolean;
  running: boolean;
  error: string | null;
};

export function getPanelStatus(): Promise<PanelStatus> {
  return invoke<PanelStatus>("get_panel_status");
}

/**
 * 后台预启动服务器管理面板（kaypanel），不显示。
 *
 * 用户真正点「服务器管理」时 backend 复用已启动的进程直接返回握手，
 * 省掉首次点进去时 spawn 子进程 + 读握手的等待。
 */
export function prewarmPanel(): Promise<void> {
  return invoke<void>("prewarm_panel");
}

export type PanelBounds = {
  x: number;
  y: number;
  width: number;
  height: number;
  /** 前端视口宽度（CSS 像素），Rust 侧据此换算 DIP。 */
  viewportWidth: number;
};

/** 在内容区显示内嵌面板（kaypanel）。bounds 是 CSS 像素。 */
export function showServerPanel(bounds: PanelBounds): Promise<void> {
  return invoke<void>("show_server_panel", { bounds });
}

/** 隐藏内嵌面板；webview 与后台进程都保留，下次秒开。 */
export function hideServerPanel(): Promise<void> {
  return invoke<void>("hide_server_panel");
}

/** 窗口尺寸变化时同步面板几何位置。 */
export function resizeServerPanel(bounds: PanelBounds): Promise<void> {
  return invoke<void>("resize_server_panel", { bounds });
}
