import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  Activity,
  AlertCircle,
  Camera,
  ChevronLeft,
  Clipboard,
  FolderUp,
  Gauge,
  KeyRound,
  Keyboard,
  Maximize2,
  Minimize2,
  Mic,
  MonitorOff,
  MonitorSpeaker,
  MoreHorizontal,
  MousePointer2,
  PanelRight,
  PowerOff,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Volume2,
  VolumeX,
  Wifi,
} from "lucide-react";
import {
  DISPLAY_ALL,
  disableH264Video,
  getAutoQuality,
  getClipboardSync,
  getConnectionQuality,
  getControlInfo,
  getRemoteVolume,
  setClipboardSync as saveClipboardSync,
  isTauri,
  reconnectDirect,
  reportAudioStats,
  requestVideoKeyframe,
  saveScreenshot,
  sendControlInput,
  sendPower,
  sendSecureAttention,
  setAudioEnabled,
  setAutoQuality as saveAutoQuality,
  setControlVideoMuted,
  setRemoteVolume,
  setStreamOptions,
  setVideoDecoder,
  startControl,
  stopControl,
  subscribeControlAudio,
  subscribeControlAudioError,
  subscribeControlAudioFormat,
  subscribeControlCaps,
  subscribeControlDeviceInfo,
  subscribeControlDisplays,
  subscribeControlFrame,
  subscribeControlInputStatus,
  subscribeControlPowerError,
  subscribeControlPowerOk,
  subscribeControlSas,
  subscribeControlReconnected,
  subscribeControlReconnecting,
  subscribeControlReconnectFailed,
  subscribeControlState,
  subscribeControlStats,
  subscribeControlVolume,
  subscribeControlVolumeError,
  type AudioSource,
  type ConnectionQuality,
  type ControlFrameEvent,
  type ControlReconnectOutcome,
  type ControlStats,
  type InputEvent,
  type PowerAction,
  type ReconnectProgress,
  type RemoteDisplay,
  type StreamQuality,
} from "../api";
import { formatDuration } from "../audit";
import { RemoteAudioPlayer, supportsOpusAudio } from "../audio";
import DesktopPreview from "../components/DesktopPreview";
import { EmptyState, Modal, Progress, Segmented, Switch } from "../components/ui";
import { describeError, errorLine } from "../errors";
import {
  LOCAL_ONLY_KEYS,
  MOUSE_BUTTONS,
  SCAN_CODES,
  isTypingTarget,
  pointToNormalized,
  wheelNotches,
} from "../input";
import { formatLastSeen } from "../mock";
import { isDirectDeviceId, rememberSession } from "../session";
import type { ControlPageProps, DeviceView } from "../types";
import "./ControlPage.css";

type StreamPhase =
  | "offline"
  | "connecting"
  | "waiting"
  | "live"
  | "error"
  | "demo";

let controlTokenSeq = Date.now();

/**
 * 与 client 端 start_control 的默认订阅保持一致。
 *
 * 默认 60：30fps 下拖动窗口、滚动页面会明显发顿，1080p 的硬件编码器
 * 完全带得动 60fps（8 Mbps），没必要把默认档压在半帧率。
 */
const DEFAULT_STREAM_OPTIONS: StreamQuality = {
  fps: 60,
  maxWidth: 1920,
  quality: 65,
  display: 0,
};

const RESOLUTION_OPTIONS = [
  { maxWidth: 1280, hint: "标清" },
  { maxWidth: 1920, hint: "高清" },
  { maxWidth: 2560, hint: "2K" },
  { maxWidth: 3840, hint: "4K" },
];

/**
 * 帧率档位。`0` 是「自动」：不设目标帧率，被控端按显示器的实际刷新率
 * （DXGI 只在桌面变化时出帧）和链路能力尽量跑快，慢下来由自适应负责。
 */
const FPS_OPTIONS = [0, 10, 15, 30, 60, 90, 120, 144];

/**
 * 画面偏好（分辨率 / 帧率 / 清晰度）跨会话保留：4K 屏的用户不该每次进控制页
 * 都重选一遍。显示器索引不保存 —— 它跟某台被控机的屏幕布局绑定，换设备后
 * 那个索引可能根本不存在，所以每次新会话都从主屏开始。
 */
const STREAM_OPTIONS_KEY = "screenwall.stream-options";
/// 偏好结构版本：v2 起默认帧率从 30 提到 60。
const STREAM_OPTIONS_VERSION = 2;

function readStreamOptions(): StreamQuality {
  try {
    const saved = JSON.parse(
      localStorage.getItem(STREAM_OPTIONS_KEY) ?? "null",
    ) as (Partial<StreamQuality> & { v?: number }) | null;
    // 只接受菜单里真实存在的档位：旧版本存下的、被手改过的值一律回默认，
    // 免得界面显示一个后端会钳到别的值的档位。
    if (
      saved &&
      RESOLUTION_OPTIONS.some((item) => item.maxWidth === saved.maxWidth) &&
      FPS_OPTIONS.includes(saved.fps as number) &&
      QUALITY_OPTIONS.some((item) => item.quality === saved.quality)
    ) {
      // 旧版本存的 30 多半就是当年的默认值，跟着新默认升到 60；
      // 用户之后手动选 30 会带 v2 存下来，不会再被改。
      const fps =
        saved.v === STREAM_OPTIONS_VERSION
          ? (saved.fps as number)
          : saved.fps === 30
            ? 60
            : (saved.fps as number);
      return {
        fps,
        maxWidth: saved.maxWidth as number,
        quality: saved.quality as number,
        display: 0,
      };
    }
  } catch {
    // 存储不可用（隐私模式）或 JSON 损坏：用默认值，不影响连接。
  }
  return DEFAULT_STREAM_OPTIONS;
}

function saveStreamOptions(options: StreamQuality): void {
  try {
    localStorage.setItem(
      STREAM_OPTIONS_KEY,
      JSON.stringify({
        v: STREAM_OPTIONS_VERSION,
        fps: options.fps,
        maxWidth: options.maxWidth,
        quality: options.quality,
      }),
    );
  } catch {
    // 存不下就只对本次运行生效。
  }
}

const IPV4 = /^\d{1,3}(\.\d{1,3}){3}$/;

/**
 * 合成设备视图：直连设备的 ID（`direct:192.168.1.20:47900`）不在账号设备列表里，
 * 页面需要名字、在线状态这些字段，这里造一个占位对象。
 * 只在本页使用，绝不写回 `devices` 列表。
 */
function syntheticDeviceView(deviceId: string): DeviceView {
  const address = deviceId.replace(/^direct[:\-]/, "");
  const ip = address
    .split(":")
    .map((part) => part.trim())
    .find((part) => IPV4.test(part));
  return {
    id: deviceId,
    name: "直连设备",
    os: "未知系统",
    version: "",
    online: true,
    lastSeen: Date.now(),
    controllable: true,
    ip,
  };
}

const QUALITY_OPTIONS = [
  { quality: 50, label: "流畅" },
  { quality: 65, label: "均衡" },
  { quality: 75, label: "清晰" },
];

/** 画面四周留白的配色：自动按被控画面明暗在黑白之间切换。 */
type StageTone = "auto" | "dark" | "light";

const STAGE_TONE_KEY = "screenwall.stage-tone";

function readStageTone(): StageTone {
  try {
    const saved = localStorage.getItem(STAGE_TONE_KEY);
    if (saved === "dark" || saved === "light" || saved === "auto") {
      return saved;
    }
  } catch {
    // 隐私模式或存储被禁用时用默认值即可。
  }
  return "auto";
}

/**
 * 画面填充方式：`cover` 铺满舞台（等比放大、裁掉超出部分，没有留白），
 * `contain` 完整显示整块屏幕（可能出现上下或左右留白）。
 */
type FitMode = "cover" | "contain";

const FIT_MODE_KEY = "screenwall.stage-fit";

function readFitMode(): FitMode {
  try {
    const saved = localStorage.getItem(FIT_MODE_KEY);
    if (saved === "cover" || saved === "contain") return saved;
  } catch {
    // 存储不可用时用默认值。
  }
  return "cover";
}

/** base64（后端把 Annex-B 帧编码后随事件发来）→ 字节。 */
function base64ToBytes(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/**
 * 从 Annex-B 码流里读 SPS（NAL 类型 7）的 profile / 约束 / level，
 * 拼成 WebCodecs 需要的 `avc1.PPCCLL`。
 *
 * 硬件编码器每个关键帧前都会重发 SPS/PPS，所以分辨率或编码参数变了也能
 * 拿到新值；读不到就说明这不是我们认识的码流，交给调用方回退。
 */
function spsCodecString(bytes: Uint8Array): string | null {
  let index = 0;
  while (index + 4 <= bytes.length) {
    const sc3 = bytes[index] === 0 && bytes[index + 1] === 0 && bytes[index + 2] === 1;
    const sc4 =
      bytes[index] === 0 &&
      bytes[index + 1] === 0 &&
      bytes[index + 2] === 0 &&
      bytes[index + 3] === 1;
    if (!sc3 && !sc4) {
      index += 1;
      continue;
    }
    const start = index + (sc3 ? 3 : 4);
    if (start + 3 < bytes.length && (bytes[start] & 0x1f) === 7) {
      const hex = (value: number) =>
        value.toString(16).toUpperCase().padStart(2, "0");
      return `avc1.${hex(bytes[start + 1])}${hex(bytes[start + 2])}${hex(
        bytes[start + 3],
      )}`;
    }
    index = start;
  }
  return null;
}

/** 这个运行环境有没有 WebCodecs 的 H.264 解码器（WebView2 / Chromium 内核）。 */
function supportsWebCodecsH264(): boolean {
  return typeof VideoDecoder !== "undefined" && typeof EncodedVideoChunk !== "undefined";
}

/** 一次会话里的 H.264 解码状态；放在 ref 里是因为帧回调是同步的。 */
type H264State = {
  decoder: VideoDecoder | null;
  codec: string | null;
  /** 配置好之后必须先见到关键帧才能解 P 帧。 */
  waitingKey: boolean;
  /** 解码时间戳（微秒）：只要求单调递增，用本地计数避免对端时钟回绕。 */
  timestamp: number;
  /** 上次向被控端要关键帧的时刻，避免丢帧时把请求打成洪水。 */
  lastKeyRequestAt: number;
  errors: number;
  /** 已经回退过 MJPEG：本次会话不再尝试解 H.264。 */
  disabled: boolean;
};

function newH264State(): H264State {
  return {
    decoder: null,
    codec: null,
    waitingKey: true,
    timestamp: 0,
    lastKeyRequestAt: 0,
    errors: 0,
    disabled: false,
  };
}

export default function ControlPage({
  devices,
  deviceId,
  active,
  onBack,
  onFiles,
  notify,
}: ControlPageProps) {
  // 页面被保活：切到别的页面时只是 `display:none`。窗口最小化到托盘时
  // `visibilityState` 也会变 hidden，两种情况下都不需要继续推画面。
  const [windowHidden, setWindowHidden] = useState(
    () => typeof document !== "undefined" && document.visibilityState === "hidden",
  );
  useEffect(() => {
    const onChange = () => setWindowHidden(document.visibilityState === "hidden");
    document.addEventListener("visibilitychange", onChange);
    return () => document.removeEventListener("visibilitychange", onChange);
  }, []);
  /** 页面可见且窗口没被隐藏：只有这时才真的需要画面。 */
  const videoWanted = active && !windowHidden;
  // 用 ref 让定时器读到最新值。
  const liveRef = useRef(videoWanted);
  liveRef.current = videoWanted;
  // 账号设备列表里能查到的设备；直连设备用合成 ID，不在这里。
  const listedDevice = devices.find((item) => item.id === deviceId);
  const direct = isDirectDeviceId(deviceId);
  // 实际链路：后端建会话时会告诉我们这次是中转、局域网直连还是 P2P。
  // `direct` 只看设备 ID，同账号 P2P 的 ID 仍然是账号里的设备，看不出来。
  const [linkMode, setLinkMode] = useState<"relay" | "direct" | "p2p">(
    direct ? "direct" : "relay",
  );
  // 直连设备的合成 ID 不在设备列表里，用占位对象补齐页面需要的字段。
  // 占位对象只在本页使用，绝不写回 devices 列表。
  const placeholderDevice = useMemo(
    () => (listedDevice ? null : syntheticDeviceView(deviceId)),
    [deviceId, listedDevice],
  );
  const device = listedDevice ?? placeholderDevice ?? undefined;
  // 订阅过滤与提示文案都按 deviceId 走：占位对象只是展示用。
  const deviceLabel = device?.name ?? "直连设备";

  const [phase, setPhase] = useState<StreamPhase>("connecting");
  const [streamError, setStreamError] = useState<string | null>(null);
  const [hasFrame, setHasFrame] = useState(false);
  const [retryKey, setRetryKey] = useState(0);
  const [frameStats, setFrameStats] = useState({
    fps: 0,
    mbps: 0,
    width: 0,
    height: 0,
  });
  const [menuOpen, setMenuOpen] = useState(false);
  // 电源操作必须二次确认：重启/关机不可撤销。
  const [powerAction, setPowerAction] = useState<PowerAction | null>(null);
  const [powerBusy, setPowerBusy] = useState(false);
  const [qualityMenuOpen, setQualityMenuOpen] = useState(false);
  const [qualitySwitching, setQualitySwitching] = useState(false);
  const [streamOptions, setStreamOptionsState] =
    useState<StreamQuality>(readStreamOptions);
  const [displays, setDisplays] = useState<RemoteDisplay[]>([]);
  const [fullscreen, setFullscreen] = useState(false);
  const [sideOpen, setSideOpen] = useState(true);
  const [stageTone, setStageTone] = useState<StageTone>(readStageTone);
  const [fitMode, setFitMode] = useState<FitMode>(readFitMode);
  // 当前画面的编码：决定渲染用 <img>（MJPEG）还是 <canvas>（H.264）。
  const [videoCodec, setVideoCodec] = useState<"mjpeg" | "h264">("mjpeg");
  // 自动模式下的判断结果：画面整体偏亮就用白底，偏暗就用黑底。
  const [stageLight, setStageLight] = useState(false);
  const [keyboardOn, setKeyboardOn] = useState(true);
  const [mouseOn, setMouseOn] = useState(true);
  const [clipboardOn, setClipboardOn] = useState(true);
  /**
   * 键鼠捕获：点画面后 Tab / 方向键都发给远端。
   * 按两次 Esc 或点「释放键盘」退出；焦点在输入框里时永不捕获。
   */
  const [captured, setCaptured] = useState(false);
  // 事件处理器里要读最新值：用 ref 同步，避免闭包读到旧的 captured。
  const capturedRef = useRef(false);
  capturedRef.current = captured;
  const escStreakRef = useRef(0);
  const escTimerRef = useRef<number | null>(null);
  const releaseBtnRef = useRef<HTMLButtonElement | null>(null);
  // 系统声音默认关闭：显式点击才订阅，避免一进页面就把带宽吃掉。
  const [audioOn, setAudioOn] = useState(false);
  const [audioBusy, setAudioBusy] = useState(false);
  /** 实际生效的音频编码与码率，由 audio.sub.ok 回报。 */
  const [audioFormat, setAudioFormat] = useState<string | null>(null);
  /** 声音来源：系统回环或被控端麦克风。 */
  const [audioSource, setAudioSource] = useState<AudioSource>("loopback");
  /** 被控端当前音量/静音，由 audio.volume.ok 回报；`null` 表示还没读到。 */
  const [remoteVolume, setRemoteVolumeState] = useState<{
    level: number;
    muted: boolean;
  } | null>(null);
  const [volumeBusy, setVolumeBusy] = useState(false);
  /** 旧被控端不认识音量消息：置灰控件，而不是一直转圈。 */
  const [volumeUnsupported, setVolumeUnsupported] = useState(false);
  /** 被控端前台有提权窗口，输入被 UIPI 拦截（由 `input.status` 推送）。 */
  const [uipiBlocked, setUipiBlocked] = useState(false);
  const uipiBlockedRef = useRef(false);
  /** Ctrl+Alt+Del 正在等待被控端回应。 */
  const [sasBusy, setSasBusy] = useState(false);
  const sasBusyTimerRef = useRef<number | null>(null);
  /**
   * 握手后双方能力交集（`welcome.caps`）；`null` 表示还没握手。
   * 已知能力就直接决定可选功能是否可用，未知时沿用旧的「试了再说」兜底。
   */
  const [peerCaps, setPeerCaps] = useState<string[] | null>(null);
  /** 已知被控端不支持音量联动：不用等 `control-volume-error` 回来才置灰。 */
  const capsBlockVolume = peerCaps !== null && !peerCaps.includes("audio.volume");
  /** 已知被控端不支持 Ctrl+Alt+Del 请求（老版本）。 */
  const capsBlockSas = peerCaps !== null && !peerCaps.includes("input.sas");
  /** 被控端实时设备信息（CPU/内存/显卡/IP），随 `welcome` 与 `status` 推送。 */
  const [deviceInfo, setDeviceInfo] = useState<{
    cpu: number;
    mem: number;
    gpu: string;
    ip: string;
  } | null>(null);
  const volumeTimerRef = useRef<number | null>(null);
  const audioIdRef = useRef(0);
  // 隧道 Ping/Pong 的真实往返时延，以及「自动适应网络」开关。
  const [rttMs, setRttMs] = useState<number | null>(null);
  const [autoQuality, setAutoQuality] = useState(true);
  // 会话累计质量（丢帧/丢包/重连），由 control-stats 推送。
  const [quality, setQuality] = useState<ConnectionQuality | null>(null);
  const [qualityOpen, setQualityOpen] = useState(false);
  const healthMenuRef = useRef<HTMLDivElement | null>(null);
  // 断线自动重连：`null` 表示没有在重连；`failed` 表示 5 次都失败，等用户点重试。
  const [reconnecting, setReconnecting] = useState<ReconnectProgress | null>(
    null,
  );
  const [reconnectFailed, setReconnectFailed] =
    useState<ControlReconnectOutcome | null>(null);
  // 重连成功后短暂显示「已恢复」提示。
  const [recovered, setRecovered] = useState(false);
  const recoveredTimerRef = useRef<number | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const qualityMenuRef = useRef<HTMLDivElement | null>(null);
  const streamOptionsRef = useRef(streamOptions);
  const screenRef = useRef<HTMLImageElement | null>(null);
  const screenCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  // 解码出来的原始画面（远端分辨率），每次重绘按 cover/contain 缩放贴到可见画布。
  const h264SrcRef = useRef<HTMLCanvasElement | null>(null);
  const h264Ref = useRef<H264State>(newH264State());
  const paintH264Ref = useRef<() => void>(() => {});
  const videoCodecRef = useRef<"mjpeg" | "h264">("mjpeg");
  const hasFrameRef = useRef(false);
  const openedRef = useRef(false);
  const errorRef = useRef(false);
  const stalledRef = useRef(false);
  const lastFrameAtRef = useRef(0);
  const lastTickRef = useRef(0);
  const pendingFrameRef = useRef<string | null>(null);
  const frameInfoRef = useRef({ count: 0, bytes: 0, width: 0, height: 0 });

  // ---- 键鼠注入 ----------------------------------------------------
  const inputLayerRef = useRef<HTMLDivElement | null>(null);
  const controlTokenRef = useRef(0);
  const keyboardOnRef = useRef(keyboardOn);
  const mouseOnRef = useRef(mouseOn);
  const audioOnRef = useRef(audioOn);
  const audioSourceRef = useRef(audioSource);
  const audioPlayerRef = useRef<RemoteAudioPlayer | null>(null);
  const frameStatsRef = useRef(frameStats);
  const fitModeRef = useRef<FitMode>(fitMode);
  const pressedKeysRef = useRef<Set<number>>(new Set());
  const pressedButtonsRef = useRef<Set<number>>(new Set());
  const pendingMoveRef = useRef<[number, number] | null>(null);
  const moveRafRef = useRef<number | null>(null);
  const lastMoveSentAtRef = useRef(0);
  const reconnectingRef = useRef(false);
  const reconnectFailedRef = useRef(false);
  // 会话 effect 只跑一次，闭包读不到后续的 peerCaps；用 ref 同步最新能力。
  const peerCapsRef = useRef<string[] | null>(null);

  reconnectingRef.current = reconnecting !== null;
  reconnectFailedRef.current = reconnectFailed !== null;
  peerCapsRef.current = peerCaps;

  // 重连期间与重连失败后都不注入键鼠，避免输入打到不存在的会话上。
  const canInject =
    phase === "live" &&
    hasFrame &&
    !!device?.online &&
    !reconnectingRef.current &&
    !reconnectFailedRef.current;

  keyboardOnRef.current = keyboardOn;
  mouseOnRef.current = mouseOn;
  audioOnRef.current = audioOn;
  audioSourceRef.current = audioSource;
  frameStatsRef.current = frameStats;
  fitModeRef.current = fitMode;
  streamOptionsRef.current = streamOptions;

  const sendInput = useCallback((events: InputEvent[]) => {
    const token = controlTokenRef.current;
    if (token <= 0 || events.length === 0) return;
    // 重连期间/失败后不发送输入：后端会话已经不存在，发了也只会报错。
    if (reconnectingRef.current || reconnectFailedRef.current) return;
    void sendControlInput(token, events);
  }, []);

  const cancelPendingMove = useCallback(() => {
    pendingMoveRef.current = null;
    if (moveRafRef.current !== null) {
      window.cancelAnimationFrame(moveRafRef.current);
      moveRafRef.current = null;
    }
  }, []);

  const releaseAllRemote = useCallback(() => {
    pressedKeysRef.current.clear();
    pressedButtonsRef.current.clear();
    pendingMoveRef.current = null;
    if (moveRafRef.current !== null) {
      window.cancelAnimationFrame(moveRafRef.current);
      moveRafRef.current = null;
    }
    const token = controlTokenRef.current;
    if (token > 0) {
      void sendControlInput(token, [{ k: "releaseAll" }]);
    }
  }, []);

  const flushMove = useCallback(() => {
    moveRafRef.current = null;
    const abs = pendingMoveRef.current;
    pendingMoveRef.current = null;
    if (!abs || !mouseOnRef.current) return;
    // 高刷屏上 rAF 可能超过 60Hz，这里再按 16ms 节流，保证最多约 60 次/秒。
    const now = performance.now();
    if (now - lastMoveSentAtRef.current < 16) {
      pendingMoveRef.current = abs;
      moveRafRef.current = window.requestAnimationFrame(flushMove);
      return;
    }
    lastMoveSentAtRef.current = now;
    sendInput([{ k: "move", abs }]);
  }, [sendInput]);

  /**
   * 把视口坐标映射为画面 0..1 归一化坐标。
   * 画面使用 object-fit: contain，需要按缩放比与居中偏移扣掉黑边。
   */
  const pointToAbs = useCallback(
    (
      clientX: number,
      clientY: number,
      clampToImage = true,
    ): [number, number] | null => {
      const layer = inputLayerRef.current;
      if (!layer) return null;
      const rect = layer.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return null;

      const { width: frameW, height: frameH } = frameStatsRef.current;
      // 铺满按较大的比例放大，超出舞台的部分被裁掉；适应按较小的比例完整显示。
      return pointToNormalized(
        rect,
        clientX - rect.left,
        clientY - rect.top,
        frameW,
        frameH,
        fitModeRef.current,
        clampToImage,
      );
    },
    [],
  );

  useEffect(() => {
    const onFullscreenChange = () => {
      setFullscreen(document.fullscreenElement !== null);
    };
    document.addEventListener("fullscreenchange", onFullscreenChange);
    return () =>
      document.removeEventListener("fullscreenchange", onFullscreenChange);
  }, []);

  // 全屏是「只看被控端画面」：进入后侧栏、顶栏、控制页工具栏和右侧信息栏都收起来，
  // 只留远端画面和底部操作条（方便退出全屏、切声音）。退出时原样恢复。
  useEffect(() => {
    document.body.classList.toggle("is-immersive", fullscreen);
    return () => document.body.classList.remove("is-immersive");
  }, [fullscreen]);

  // 背景偏好按本机记忆，换设备或重开应用都保持。
  useEffect(() => {
    try {
      localStorage.setItem(STAGE_TONE_KEY, stageTone);
    } catch {
      // 存储不可用时只影响记忆，不影响本次使用。
    }
  }, [stageTone]);

  useEffect(() => {
    try {
      localStorage.setItem(FIT_MODE_KEY, fitMode);
    } catch {
      // 同上。
    }
  }, [fitMode]);

  /**
   * 自动模式：每秒采样一次画面四周的像素亮度，整屏偏亮就用白底，偏暗就用黑底，
   * 让留白和画面连成一片。只取外圈，避免被中间的窗口内容带偏。
   */
  useEffect(() => {
    if (stageTone !== "auto" || !hasFrame || phase !== "live") return;
    const canvas = document.createElement("canvas");
    canvas.width = 24;
    canvas.height = 14;
    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (!context) return;

    const sample = () => {
      const image = screenRef.current;
      if (!image || !image.complete || image.naturalWidth === 0) return;
      try {
        context.drawImage(image, 0, 0, canvas.width, canvas.height);
        const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
        let sum = 0;
        let count = 0;
        for (let y = 0; y < canvas.height; y += 1) {
          for (let x = 0; x < canvas.width; x += 1) {
            const edge =
              x < 2 || y < 2 || x >= canvas.width - 2 || y >= canvas.height - 2;
            if (!edge) continue;
            const index = (y * canvas.width + x) * 4;
            sum +=
              0.2126 * data[index] +
              0.7152 * data[index + 1] +
              0.0722 * data[index + 2];
            count += 1;
          }
        }
        if (count > 0) {
          const average = sum / count;
          // 带回滞的阈值，避免画面亮度刚好在临界点时黑白来回跳。
          setStageLight((current) =>
            current ? average > 110 : average > 150,
          );
        }
      } catch {
        // 取像素失败时保持上一次判断。
      }
    };

    sample();
    const timer = window.setInterval(sample, 1000);
    return () => window.clearInterval(timer);
  }, [stageTone, hasFrame, phase]);

  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (event: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [menuOpen]);

  useEffect(() => {
    if (!qualityMenuOpen) return;
    const onPointerDown = (event: MouseEvent) => {
      if (
        qualityMenuRef.current &&
        !qualityMenuRef.current.contains(event.target as Node)
      ) {
        setQualityMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [qualityMenuOpen]);

  useEffect(() => {
    if (!qualityOpen) return;
    const onPointerDown = (event: MouseEvent) => {
      if (
        healthMenuRef.current &&
        !healthMenuRef.current.contains(event.target as Node)
      ) {
        setQualityOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setQualityOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [qualityOpen]);

  // 全屏时按 Esc 只退出全屏，避免外层 App 同时把页面切回屏幕墙。
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && document.fullscreenElement) {
        event.stopImmediatePropagation();
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, []);

  // 只有在实时画面、设备在线、且没有在重连时才允许注入。
  // 离开该状态、切换开关或卸载时主动释放远端所有按键/鼠标，避免被控端「卡键」。
  useEffect(() => {
    if (!canInject) {
      // 进入重连/失败态时本地记录也要清掉，恢复后不会误发重复的 keydown。
      pressedKeysRef.current.clear();
      pressedButtonsRef.current.clear();
      cancelPendingMove();
      return;
    }
    return () => {
      releaseAllRemote();
    };
  }, [canInject, keyboardOn, mouseOn, releaseAllRemote, cancelPendingMove]);

  useEffect(() => {
    if (!canInject) return;
    const onWindowBlur = () => releaseAllRemote();
    const onVisibilityChange = () => {
      if (document.hidden) releaseAllRemote();
    };
    window.addEventListener("blur", onWindowBlur);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      window.removeEventListener("blur", onWindowBlur);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [canInject, releaseAllRemote]);

  // 滚轮需要 non-passive 监听才能 preventDefault 阻止页面滚动。
  useEffect(() => {
    if (!canInject) return;
    const layer = inputLayerRef.current;
    if (!layer) return;
    const onWheel = (event: WheelEvent) => {
      if (!mouseOnRef.current) return;
      event.preventDefault();
      event.stopPropagation();
      const unit =
        event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 100 : 1;
      const delta: [number, number] = [
        wheelNotches(event.deltaX * unit),
        -wheelNotches(event.deltaY * unit),
      ];
      if (delta[0] === 0 && delta[1] === 0) return;
      cancelPendingMove();
      sendInput([{ k: "wheel", delta }]);
    };
    layer.addEventListener("wheel", onWheel, { passive: false });
    return () => layer.removeEventListener("wheel", onWheel);
  }, [canInject, cancelPendingMove, sendInput]);

  // 在画面外松开鼠标时也要补发 up，避免远端按钮卡住。
  useEffect(() => {
    if (!canInject) return;
    const onMouseUp = (event: MouseEvent) => {
      if (!mouseOnRef.current) return;
      if (!pressedButtonsRef.current.delete(event.button)) return;
      const button = MOUSE_BUTTONS[event.button];
      if (!button) return;
      const abs = pointToAbs(event.clientX, event.clientY, false);
      cancelPendingMove();
      sendInput([
        { k: "button", button, down: false, ...(abs ? { abs } : {}) },
      ]);
    };
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, [canInject, cancelPendingMove, pointToAbs, sendInput]);

  useEffect(
    () => () => {
      if (moveRafRef.current !== null) {
        window.cancelAnimationFrame(moveRafRef.current);
      }
    },
    [],
  );

  // 进入控制页时建立（或复用）控制会话，收到首帧后切到实时画面。
  useEffect(() => {
    setHasFrame(false);
    hasFrameRef.current = false;
    openedRef.current = false;
    errorRef.current = false;
    stalledRef.current = false;
    lastFrameAtRef.current = 0;
    lastTickRef.current = performance.now();
    pendingFrameRef.current = null;
    frameInfoRef.current = { count: 0, bytes: 0, width: 0, height: 0 };
    setFrameStats({ fps: 0, mbps: 0, width: 0, height: 0 });
    setStreamError(null);
    setDisplays([]);
    h264Ref.current = newH264State();
    videoCodecRef.current = "mjpeg";
    setVideoCodec("mjpeg");

    if (!device) return;
    if (!device.online) {
      setPhase("offline");
      return;
    }
    if (!isTauri) {
      setPhase("demo");
      return;
    }

    let cancelled = false;
    let guardTimer: number | undefined;
    // 会话令牌：中转路径由前端自增，直连路径必须复用后端建会话时的令牌
    // （后端事件带的就是那个值，自己另起一个会让所有过滤条件失配）。
    let clientToken = 0;
    controlTokenRef.current = 0;
    setPhase("connecting");
    setRemoteVolumeState(null);
    setVolumeBusy(false);
    setVolumeUnsupported(false);
    setPeerCaps(null);
    setDeviceInfo(null);

    const fail = (message: string) => {
      if (cancelled) return;
      errorRef.current = true;
      // 会话已经不可用：本地停止播放，后端那份订阅会随会话关闭一起释放。
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      audioOnRef.current = false;
      setAudioOn(false);
      setPhase("error");
      setStreamError(message);
    };

    // ---- H.264 解码 --------------------------------------------------
    // WebCodecs 的 VideoDecoder 是异步的，而帧事件回调是同步的：状态一律放
    // 在 `h264Ref` 里，React 只负责在编码切换时重渲染一次。

    if (!h264SrcRef.current) {
      h264SrcRef.current = document.createElement("canvas");
    }

    const closeDecoder = () => {
      const state = h264Ref.current;
      const decoder = state.decoder;
      state.decoder = null;
      state.codec = null;
      state.waitingKey = true;
      if (decoder) {
        try {
          decoder.close();
        } catch {
          // 解码器已经因错误自行关闭（再 close 会抛 InvalidStateError）。
        }
      }
    };

    /** 把远端原始画面按 cover/contain 贴到可见画布上。 */
    const paintH264 = () => {
      const canvas = screenCanvasRef.current;
      const source = h264SrcRef.current;
      if (!canvas || !source || source.width === 0 || source.height === 0) return;
      const dpr = window.devicePixelRatio || 1;
      const width = Math.max(1, Math.round(canvas.clientWidth * dpr));
      const height = Math.max(1, Math.round(canvas.clientHeight * dpr));
      if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      const context = canvas.getContext("2d");
      if (!context) return;
      context.clearRect(0, 0, width, height);
      const scale = (fitModeRef.current === "cover" ? Math.max : Math.min)(
        width / source.width,
        height / source.height,
      );
      const drawWidth = source.width * scale;
      const drawHeight = source.height * scale;
      context.drawImage(
        source,
        (width - drawWidth) / 2,
        (height - drawHeight) / 2,
        drawWidth,
        drawHeight,
      );
    };
    paintH264Ref.current = paintH264;

    const onDecoderError = () => {
      if (cancelled) return;
      const state = h264Ref.current;
      closeDecoder();
      state.errors += 1;
      // 连续失败说明这条机器的 WebCodecs 解不了我们的码流：别再让画面
      // 在两种编码之间来回闪，让后端改订 MJPEG（后端会记住本次运行的回退）。
      if (state.errors >= 3) {
        state.disabled = true;
        void disableH264Video(clientToken).catch(() => {});
        return;
      }
      // 多半只是丢了关键帧：请被控端立刻补一个。
      void requestVideoKeyframe(clientToken).catch(() => {});
    };

    const drawDecoded = (frame: VideoFrame) => {
      const source = h264SrcRef.current;
      if (source) {
        if (source.width !== frame.displayWidth || source.height !== frame.displayHeight) {
          source.width = frame.displayWidth;
          source.height = frame.displayHeight;
        }
        source.getContext("2d")?.drawImage(frame, 0, 0);
      }
      frame.close();
      paintH264();
      if (!hasFrameRef.current) {
        hasFrameRef.current = true;
        setHasFrame(true);
      }
    };

    const ensureDecoder = (codec: string): VideoDecoder | null => {
      const state = h264Ref.current;
      if (state.disabled) return null;
      if (state.decoder && state.codec === codec && state.decoder.state === "configured") {
        return state.decoder;
      }
      closeDecoder();
      try {
        const decoder = new VideoDecoder({
          output: drawDecoded,
          error: onDecoderError,
        });
        // Annex-B 输入：不传 description，让解码器自己从码流里取 SPS/PPS。
        // 1080p60 的软解会吃掉大量主线程时间，优先交给显卡硬解。
        decoder.configure({
          codec,
          optimizeForLatency: true,
          hardwareAcceleration: "prefer-hardware",
        });
        state.decoder = decoder;
        state.codec = codec;
        state.waitingKey = true;
        return decoder;
      } catch {
        onDecoderError();
        return null;
      }
    };

    const decodeH264Frame = (frame: ControlFrameEvent) => {
      const state = h264Ref.current;
      if (state.disabled) return;
      const bytes = base64ToBytes(frame.data);
      // 用采集端的真实时间戳（微秒），只保证本机单调递增。之前每帧加 1ms
      // 的假时间戳会让解码器把 60fps 的帧当成 1000fps，调度/统计都不对。
      const timestamp = Math.max(frame.timestampUs, state.timestamp + 1);
      state.timestamp = timestamp;
      if (frame.keyframe) {
        const codec = spsCodecString(bytes);
        const decoder = codec ? ensureDecoder(codec) : null;
        if (!decoder) {
          if (!codec) onDecoderError();
          return;
        }
        state.waitingKey = false;
        try {
          decoder.decode(
            new EncodedVideoChunk({
              type: "key",
              timestamp,
              data: bytes,
            }),
          );
        } catch {
          onDecoderError();
        }
        return;
      }
      // 配好之前、或等关键帧期间的 P 帧一律丢弃：硬解不会替我们补参考帧。
      // 首帧关键帧可能在 `video.sub.ok` 之前就到了（那时宽高还是 0，被后端丢弃），
      // 所以这里主动补要一个，否则要等到 10 秒的周期关键帧才出画面。
      if (!state.decoder || state.waitingKey) {
        const now = performance.now();
        if (now - state.lastKeyRequestAt > 800) {
          state.lastKeyRequestAt = now;
          void requestVideoKeyframe(clientToken).catch(() => {});
        }
        return;
      }
      // 解码跟不上时不能随手丢 P 帧：H.264 的 P 帧依赖前面的参考帧，
      // 单独丢一帧会让后续画面花到下一个 IDR（周期关键帧 10 秒才一个）。
      // 这里直接重建解码器、清空排队，丢到下一个关键帧，并让被控端马上补一个：
      // 短暂停一下，比花屏拖十秒好，也不会让排队延迟越积越高。
      if (state.decoder.decodeQueueSize > 4) {
        const codec = state.codec;
        closeDecoder();
        if (codec) ensureDecoder(codec);
        const now = performance.now();
        if (now - state.lastKeyRequestAt > 500) {
          state.lastKeyRequestAt = now;
          void requestVideoKeyframe(clientToken).catch(() => {});
        }
        return;
      }
      try {
        state.decoder.decode(
          new EncodedVideoChunk({
            type: "delta",
            timestamp,
            data: bytes,
          }),
        );
      } catch {
        onDecoderError();
      }
    };

    const offFrame = subscribeControlFrame((frame) => {
      if (cancelled || frame.clientToken !== clientToken) return;
      if (frame.deviceId !== deviceId) return;

      const info = frameInfoRef.current;
      info.count += 1;
      info.bytes += frame.bytes;
      if (frame.width > 0) info.width = frame.width;
      if (frame.height > 0) info.height = frame.height;

      openedRef.current = true;
      stalledRef.current = false;
      lastFrameAtRef.current = performance.now();

      if (frame.codec === "h264") {
        if (videoCodecRef.current !== "h264") {
          videoCodecRef.current = "h264";
          setVideoCodec("h264");
        }
        // 只有解出画面才算「有帧」：首帧由 `drawDecoded` 点亮。
        decodeH264Frame(frame);
      } else {
        pendingFrameRef.current = frame.dataUrl;
        if (videoCodecRef.current !== "mjpeg") {
          videoCodecRef.current = "mjpeg";
          setVideoCodec("mjpeg");
        }
        if (screenRef.current) {
          screenRef.current.src = frame.dataUrl;
        }
        if (!hasFrameRef.current) {
          hasFrameRef.current = true;
          setHasFrame(true);
        }
      }
      setFrameStats((current) =>
        current.width === frame.width && current.height === frame.height
          ? current
          : {
              ...current,
              width: frame.width || current.width,
              height: frame.height || current.height,
            },
      );
      // 已经报错时保留错误态，避免在途帧把错误提示吞掉。
      setPhase((current) => (current === "error" ? current : "live"));
    });

    const offState = subscribeControlState((state) => {
      if (cancelled || state.clientToken !== clientToken) return;
      if (state.deviceId !== deviceId) return;
      // 正在重连时后端会先把旧会话置为 closed，这条不能当成最终失败。
      if (reconnectingRef.current && state.phase === "closed") return;

      if (state.phase === "streaming") {
        if (state.width && state.height) {
          frameInfoRef.current.width = state.width;
          frameInfoRef.current.height = state.height;
          setFrameStats((current) => ({
            ...current,
            width: state.width ?? current.width,
            height: state.height ?? current.height,
            fps: state.fps ?? current.fps,
          }));
        }
        setPhase((current) => (current === "connecting" ? "waiting" : current));
      } else if (state.phase === "error") {
        fail(state.message ?? "画面接入失败");
      } else if (state.phase === "closed") {
        // 既不是直连、又不在账号设备列表里的会话没有可重连的去处，
        // 只能给错误态让用户返回。
        if (!direct && !listedDevice) {
          setReconnecting(null);
          setReconnectFailed(null);
          fail("连接已断开，请返回后重新连接");
          return;
        }
        // 主动停止/离开页面时前端已经取消订阅，这里只可能是意外断开：
        // 中转与直连会话后端都会自动重连（直连是重建整条 TLS + WS），
        // 先显示「正在重连」，等后端推进度或直接给结果。
        setReconnecting((current) =>
          current ?? { attempt: 0, maxAttempts: 5, delayMs: 0 },
        );
      }
    });

    // 握手结果里的能力交集：比旧逻辑（发了消息等报错）更早、更准。
    const offCaps = subscribeControlCaps((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      setPeerCaps(event.caps);
    });

    const offDisplays = subscribeControlDisplays((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      setDisplays(event.displays);
    });

    // 被控端设备信息：握手时带来显卡/IP，之后每 5 秒的 `status` 更新 CPU/内存。
    const offDeviceInfo = subscribeControlDeviceInfo((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      setDeviceInfo({
        cpu: event.cpu,
        mem: event.mem,
        gpu: event.gpu,
        ip: event.ip,
      });
    });

    const offAudio = subscribeControlAudio((audio) => {
      if (cancelled || audio.clientToken !== clientToken) return;
      if (audio.deviceId !== deviceId) return;
      if (audio.codec === "opus") {
        audioPlayerRef.current?.pushOpus(audio.data, audio.sampleRate, audio.channels);
      } else {
        audioPlayerRef.current?.pushPcm(audio.data, audio.sampleRate, audio.channels);
      }
    });

    const offAudioFormat = subscribeControlAudioFormat((format) => {
      if (cancelled || format.clientToken !== clientToken) return;
      if (format.deviceId !== deviceId) return;
      audioIdRef.current = format.stream;
      setAudioFormat(
        format.codec === "opus"
          ? `Opus ${Math.round(format.bitrate / 1000)} kbps · ${format.sampleRate / 1000} kHz`
          : `PCM ${format.sampleRate / 1000} kHz`,
      );
      if (format.source === "microphone") setAudioSource("microphone");
      else if (format.source === "loopback") setAudioSource("loopback");
    });

    const offVolume = subscribeControlVolume((volume) => {
      if (cancelled || volume.clientToken !== clientToken) return;
      if (volume.deviceId !== deviceId) return;
      setRemoteVolumeState({ level: volume.level, muted: volume.muted });
      setVolumeBusy(false);
    });

    const offVolumeError = subscribeControlVolumeError(() => {
      if (cancelled) return;
      // 进入会话时会自动读一次音量，旧被控端会回 UNKNOWN_MESSAGE；只置灰控件不弹提示。
      setVolumeBusy(false);
      setVolumeUnsupported(true);
    });

    const offAudioError = subscribeControlAudioError((message) => {
      if (cancelled) return;
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      audioOnRef.current = false;
      setAudioOn(false);
      const { title, detail } = describeError(message);
      notify(`${title}：${detail}`, "error");
    });

    const offPowerOk = subscribeControlPowerOk((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      notify(
        `「${deviceLabel}」将在 3 秒后${event.action === "restart" ? "重启" : "关机"}`,
        "success",
      );
    });

    const offPowerError = subscribeControlPowerError((message) => {
      if (cancelled) return;
      const { title, detail } = describeError(message);
      notify(`${title}：${detail}`, "error");
    });

    const offSas = subscribeControlSas((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      if (sasBusyTimerRef.current !== null) {
        window.clearTimeout(sasBusyTimerRef.current);
        sasBusyTimerRef.current = null;
      }
      setSasBusy(false);
      if (event.method === "keys") {
        // 降级只能作用于普通应用，进不了安全桌面；把原因原样告诉用户。
        notify(
          event.detail
            ? `已回退为普通 Ctrl+Alt+Del 按键：${event.detail}`
            : "已回退为普通 Ctrl+Alt+Del 按键：被控端不是服务/管理员模式，无法进入安全桌面",
          "error",
        );
      } else {
        notify("已向被控端发送 Ctrl+Alt+Del", "success");
      }
    });

    const offInputStatus = subscribeControlInputStatus((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      const wasBlocked = uipiBlockedRef.current;
      uipiBlockedRef.current = event.uipi;
      setUipiBlocked(event.uipi);
      // 被控端会在持续拦截时定期重报，这里只在状态翻转时弹一次。
      if (event.uipi && !wasBlocked) {
        notify(
          "被控端有提权窗口，输入被系统拦截；需要以管理员身份运行被控端",
          "error",
        );
      } else if (!event.uipi && wasBlocked) {
        notify("提权窗口已关闭，远程输入恢复正常", "info");
      }
    });

    const offStats = subscribeControlStats((stats: ControlStats) => {
      if (cancelled) return;
      setRttMs(stats.rttMs);
      setAutoQuality(stats.auto);
      if (stats.quality) setQuality(stats.quality);
      // 后端每一轮也会带一次重连进度，作为事件丢失时的兜底。
      if (stats.reconnecting) setReconnecting(stats.reconnecting);
    });

    // 每 2 秒主动拉一次质量快照；页面不可见时停掉，切回来立刻补一次。
    // 后端事件推送（control-stats）仍然生效，这里只保证「数字一定是新的」。
    let qualityTimer: number | null = null;
    const pollQuality = () => {
      // 会话令牌还没定下来（正在复用直连会话）时不查，等下一轮。
      if (cancelled || document.hidden || clientToken <= 0) return;
      void getConnectionQuality(clientToken)
        .then((next) => {
          if (!cancelled) setQuality(next);
        })
        .catch(() => {
          // 会话正在重连/已结束：保留上一次的数字，不弹提示。
        });
    };
    const startQualityPolling = () => {
      if (qualityTimer !== null) return;
      pollQuality();
      qualityTimer = window.setInterval(pollQuality, 2000);
    };
    const stopQualityPolling = () => {
      if (qualityTimer === null) return;
      window.clearInterval(qualityTimer);
      qualityTimer = null;
    };
    const onVisibilityChange = () => {
      if (document.hidden) stopQualityPolling();
      else startQualityPolling();
    };
    if (!document.hidden) startQualityPolling();
    document.addEventListener("visibilitychange", onVisibilityChange);
    // 断线自动重连：后端按 1s/2s/4s/8s/8s 重试 5 次。
    const offReconnecting = subscribeControlReconnecting((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      // 断线瞬间可能已经走过一次 fail()，把阶段拉回「等待画面」，
      // 否则横幅下还压着一块「画面接入失败」。
      setPhase((current) => (current === "error" ? "waiting" : current));
      setReconnectFailed(null);
      setReconnecting({
        attempt: event.attempt,
        maxAttempts: event.maxAttempts,
        delayMs: event.delayMs,
      });
      setRecovered(false);
      // 断开时可能已经走过一次 fail()：重连期间不算「画面接入失败」。
      errorRef.current = false;
      stalledRef.current = false;
      setStreamError(null);
    });
    const offReconnected = subscribeControlReconnected((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      setReconnecting(null);
      setReconnectFailed(null);
      setRecovered(true);
      errorRef.current = false;
      stalledRef.current = false;
      lastFrameAtRef.current = performance.now();
      // 恢复后重新登记会话：新会话号要交给页面外的登记表，
      // 跳去「文件传输」时才能带着正确的会话走。
      rememberSession(deviceId, clientToken);
      void getRemoteVolume(clientToken).catch(() => {});
      // 后端恢复订阅后会把 audio.sub.ok 再发一遍；这里把本地播放器按需重建，
      // 否则「声音开关是开的、却听不到」。
      if (audioOnRef.current) {
        if (!audioPlayerRef.current) {
          audioPlayerRef.current = new RemoteAudioPlayer((message) =>
            notify(`系统声音：${message}`, "error"),
          );
        }
        audioPlayerRef.current.open();
        audioIdRef.current = 0;
      }
      setPhase((current) => (current === "error" ? "waiting" : current));
      notify("连接已恢复", "success");
      if (recoveredTimerRef.current !== null) {
        window.clearTimeout(recoveredTimerRef.current);
      }
      recoveredTimerRef.current = window.setTimeout(() => {
        recoveredTimerRef.current = null;
        setRecovered(false);
      }, 2600);
    });
    const offReconnectFailed = subscribeControlReconnectFailed((event) => {
      if (cancelled || event.clientToken !== clientToken) return;
      if (event.deviceId !== deviceId) return;
      setReconnecting(null);
      setReconnectFailed(event);
      setRecovered(false);
      // 重连失败后画面停在最后一帧，提示用户手动重试；声音一并停掉。
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      audioIdRef.current = 0;
      audioOnRef.current = false;
      setAudioOn(false);
      setAudioFormat(null);
      const { title, detail } = describeError(event.message ?? "");
      setStreamError(`${title}：${detail}`);
      setPhase("error");
    });
    void getAutoQuality().then((enabled) => {
      if (!cancelled) setAutoQuality(enabled);
    });
    void getClipboardSync().then((enabled) => {
      if (!cancelled) setClipboardOn(enabled);
    });

    const statsTimer = window.setInterval(() => {
      const info = frameInfoRef.current;
      const now = performance.now();
      const elapsed = Math.max(0.25, (now - lastTickRef.current) / 1000);
      lastTickRef.current = now;
      setFrameStats((current) => ({
        fps: Math.round(info.count / elapsed),
        mbps: (info.bytes * 8) / elapsed / 1_000_000,
        width: info.width || current.width,
        height: info.height || current.height,
      }));
      info.count = 0;
      info.bytes = 0;

      // 断流判定只在正常会话里生效：重连中/失败态由后端状态机驱动，
      // 否则会把「正在重连」覆盖成「画面停止更新」。页面被切走时帧是
      // 主动暂停的，也不能算断流。
      if (
        liveRef.current &&
        hasFrameRef.current &&
        !errorRef.current &&
        !stalledRef.current &&
        !reconnectingRef.current &&
        !reconnectFailedRef.current &&
        now - lastFrameAtRef.current > 5000
      ) {
        stalledRef.current = true;
        fail("画面已停止更新，请重新连接");
      }
    }, 1000);

    /** 会话已建立后的公共收尾：登记、读音量、等首帧。 */
    const onSessionReady = () => {
      if (cancelled) return;
      // 会话已建立：交给页面外登记，跳去「文件传输」时不会被断开。
      rememberSession(deviceId, clientToken);
      openedRef.current = true;
      setReconnecting(null);
      setReconnectFailed(null);
      // 顺带读一次被控端音量，侧栏滑块才有初始值。
      // 已知被控端没有 `audio.volume` 能力就不发这条：发了只会换回一个
      // UNKNOWN_MESSAGE，白白把能力未知的兜底路径也点亮。
      const caps = peerCapsRef.current;
      if (caps === null || caps.includes("audio.volume")) {
        void getRemoteVolume(clientToken).catch(() => {});
      }
      setPhase((current) => (current === "connecting" ? "waiting" : current));
      // 新会话在后端按默认档（1080p / 60 / 均衡）起订阅，这里补推用户上次
      // 选的档位；否则界面显示「4K」而实际画面还是 1080p。
      // 显示器一律回到主屏：记住的索引属于上次那台机器，换设备后可能不存在。
      const wanted: StreamQuality = { ...streamOptionsRef.current, display: 0 };
      if (wanted.display !== streamOptionsRef.current.display) {
        streamOptionsRef.current = wanted;
        setStreamOptionsState(wanted);
      }
      if (
        wanted.fps !== DEFAULT_STREAM_OPTIONS.fps ||
        wanted.maxWidth !== DEFAULT_STREAM_OPTIONS.maxWidth ||
        wanted.quality !== DEFAULT_STREAM_OPTIONS.quality
      ) {
        setStreamOptions(clientToken, wanted).catch(() => {
          // 推不上去就回到默认档，宁可界面朴素也不要显示的档位与实际不符。
          streamOptionsRef.current = DEFAULT_STREAM_OPTIONS;
          setStreamOptionsState(DEFAULT_STREAM_OPTIONS);
          notify("恢复上次的画质设置失败，已回到默认 1080p", "error");
        });
      }
      guardTimer = window.setTimeout(() => {
        if (
          !cancelled &&
          liveRef.current &&
          !hasFrameRef.current &&
          !errorRef.current
        ) {
          fail("已建立会话，但 12 秒内没有收到画面");
        }
      }, 12000);
    };

    /** 原有中转路径：每次进入控制页都新开一条会话。 */
    const startRelay = () => {
      clientToken = ++controlTokenSeq;
      controlTokenRef.current = clientToken;
      // 会话握手时就要声明 `video.h264`：探测结果必须先落进后端。
      // 探测本身不失败；后端调用失败也只是退回 MJPEG，不要中断会话。
      void setVideoDecoder(supportsWebCodecsH264())
        .catch(() => {})
        .then(() => startControl(deviceId, clientToken))
        .then((info) => {
          // 后端可能已经升级成 P2P 直连（两端打洞成功时不走服务器）。
          if (info?.link === "p2p") setLinkMode("p2p");
          onSessionReady();
        })
        .catch((error) => {
          // 启动失败（设备离线、被拒、服务器没连上）：不是「断线」，不要重连，
          // 直接给错误态，用户点「重新连接」再走一遍。
          fail(errorLine(error));
        });
    };

    // 直连（或列表里查不到的合成 ID）：会话已经由 `connect_direct` 建好，
    // 必须复用而不是再建一条（再建必然报「设备不存在」）。
    // 中转路径保持原样：不查旧会话，直接新开一条。
    if (!direct && listedDevice) {
      startRelay();
    } else {
      // 直连重连成功后 `shared.control` 已经换成新会话，本页 effect 被
      // `retryKey` 重跑，这里读到的就是新会话的令牌与会话号。
      void getControlInfo()
        .then((active) => {
          if (cancelled) return;
          if (active && active.deviceId === deviceId) {
            // 复用已有会话：令牌必须是建会话时那个，不能自增，
            // 否则后端事件的 clientToken 过滤会全部失配。
            clientToken = active.clientToken;
            controlTokenRef.current = clientToken;
            if (active.link) setLinkMode(active.link);
            // 直连的 welcome 在建会话时就到了，事件早于本页订阅：
            // 从会话信息里补一次能力交集，避免能力永远未知。
            if (active.caps) {
              peerCapsRef.current = active.caps;
              setPeerCaps(active.caps);
            }
            // 直连 welcome 事件可能早于订阅：从会话信息补一次设备信息。
            if (active.gpu || active.ip) {
              setDeviceInfo({
                cpu: active.cpu ?? 0,
                mem: active.mem ?? 0,
                gpu: active.gpu ?? "",
                ip: active.ip ?? "",
              });
            }
            onSessionReady();
            return;
          }
          fail("会话已结束，请返回后重新连接");
        })
        .catch(() => {
          if (!cancelled) fail("无法读取当前会话，请返回后重新连接");
        });
    }

    return () => {
      cancelled = true;
      // 解码器必须显式关掉：它持有 GPU 侧的硬解会话。
      try {
        h264Ref.current.decoder?.close();
      } catch {
        // 已经因错误关闭。
      }
      h264Ref.current = newH264State();
      offFrame();
      offState();
      offCaps();
      offDeviceInfo();
      offDisplays();
      offAudio();
      offAudioFormat();
      offVolume();
      offVolumeError();
      offAudioError();
      offPowerOk();
      offPowerError();
      offSas();
      offInputStatus();
      offStats();
      offReconnecting();
      offReconnected();
      offReconnectFailed();
      stopQualityPolling();
      document.removeEventListener("visibilitychange", onVisibilityChange);
      setReconnecting(null);
      setReconnectFailed(null);
      setQuality(null);
      uipiBlockedRef.current = false;
      setUipiBlocked(false);
      setSasBusy(false);
      if (sasBusyTimerRef.current !== null) {
        window.clearTimeout(sasBusyTimerRef.current);
        sasBusyTimerRef.current = null;
      }
      if (recoveredTimerRef.current !== null) {
        window.clearTimeout(recoveredTimerRef.current);
        recoveredTimerRef.current = null;
      }
      setRecovered(false);
      // 离开画面页或重连：停止本地播放，并通知被控端关掉采集。
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      audioIdRef.current = 0;
      setAudioFormat(null);
      if (audioOnRef.current) {
        audioOnRef.current = false;
        setAudioOn(false);
        void setAudioEnabled(clientToken, false).catch(() => {});
      }
      window.clearInterval(statsTimer);
      if (volumeTimerRef.current !== null) {
        window.clearTimeout(volumeTimerRef.current);
        volumeTimerRef.current = null;
      }
      if (guardTimer !== undefined) window.clearTimeout(guardTimer);
      // 这里不断开会话：离开页面去「文件传输」时还要继续用这条会话。
      // 真正的断开由 App 的页面跳转（stopActiveSession）或后端换会话时完成。
    };
  }, [device?.id, device?.online, retryKey]);

  // 页面被切走时暂停后端推流：保活的页面只是 `display:none`，4K 帧继续经
  // Tauri 事件送进 WebView，主线程解析 base64 会让整窗点击卡好几秒。
  useEffect(() => {
    void setControlVideoMuted(!videoWanted).catch(() => {});
  }, [videoWanted]);

  // 组件卸载时一定要解除暂停：否则下一次进来（可能换了设备）会一直没有画面。
  useEffect(
    () => () => {
      void setControlVideoMuted(false).catch(() => {});
    },
    [],
  );

  // 回到页面：暂停期间没有帧，断流计时要重新开始；H.264 还要补一个关键帧，
  // 否则解码器会一直等参考帧，画面停在切走前那一帧。
  useEffect(() => {
    if (!videoWanted) return;
    lastFrameAtRef.current = performance.now();
    stalledRef.current = false;
    const token = controlTokenRef.current;
    if (token > 0 && videoCodecRef.current === "h264") {
      void requestVideoKeyframe(token).catch(() => {});
    }
  }, [videoWanted]);

  // H.264 画在 canvas 上，缩放与留白由代码算：舞台尺寸一变就得重绘最后一帧。
  useEffect(() => {
    const stage = stageRef.current;
    if (!stage || videoCodec !== "h264") return;
    const observer = new ResizeObserver(() => paintH264Ref.current());
    observer.observe(stage);
    return () => observer.disconnect();
  }, [videoCodec]);

  // 切换 cover / contain 立即生效，不必等下一帧到达。
  useEffect(() => {
    paintH264Ref.current();
  }, [fitMode, videoCodec]);

  // 声音开着时每 2 秒回报一次播放缓冲，被控端按它调整 Opus 码率。
  useEffect(() => {
    if (!audioOn) return;
    const timer = window.setInterval(() => {
      const player = audioPlayerRef.current;
      const token = controlTokenRef.current;
      if (player && token > 0 && audioIdRef.current > 0) {
        void reportAudioStats(token, audioIdRef.current, player.bufferMs);
      }
    }, 2000);
    return () => window.clearInterval(timer);
  }, [audioOn]);

  // 第一帧到达时组件才渲染 <img>，这里把缓存的首帧补上。
  useLayoutEffect(() => {
    if (hasFrame && screenRef.current && pendingFrameRef.current) {
      screenRef.current.src = pendingFrameRef.current;
    }
  }, [hasFrame]);

  if (!device) {
    return (
      <div className="ctrl-empty">
        <EmptyState
          icon={<MonitorOff size={18} strokeWidth={1.75} />}
          title="找不到这台设备"
          hint="设备可能已被移除，或尚未同步到当前账号。返回屏幕墙重新选择一台设备吧。"
          action={
            <button type="button" className="btn btn-primary" onClick={onBack}>
              返回屏幕墙
            </button>
          }
        />
      </div>
    );
  }

  const offline = !device.online;
  // 重连期间/失败后不算「实时」：舞台显示最后一帧 + 横幅，指标回落到设备数据。
  const live = phase === "live" && hasFrame;
  // 重连期间画面停在最后一帧：舞台角标显示「重连中」而不是「实时」。
  const streaming = live && !reconnecting && !reconnectFailed;
  const connectingPhase = phase === "connecting" || phase === "waiting";
  // 优先用隧道 Ping 的实测往返时延，退出会话后回落到设备列表里的延迟。
  const measuredLatency = live && rttMs !== null ? rttMs : device.latency;
  const latency = measuredLatency ?? 0;
  const latencyKnown = measuredLatency !== undefined;
  const fpsTop = live && frameStats.fps > 0 ? String(frameStats.fps) : "—";
  const fpsDetail = live && frameStats.fps > 0 ? `${frameStats.fps} FPS` : "—";
  const bandwidthText =
    live && frameStats.mbps > 0 ? `${frameStats.mbps.toFixed(1)} Mbps` : "—";
  // 丢帧率：丢失 ÷（收到 + 丢失）。会话刚开始没有帧时按 0 处理，避免误报警示色。
  const totalVideo = quality ? quality.video.frames + quality.video.lost : 0;
  const lossRate =
    quality && totalVideo > 0 ? (quality.video.lost / totalVideo) * 100 : 0;
  const sessionDuration = quality
    ? formatDuration(Math.max(0, Date.now() - quality.since))
    : "—";
  // 侧栏「连接质量」：真实丢帧率优先，其次看实测帧率。
  const healthLabel = offline
    ? "离线"
    : reconnecting
      ? "重连中"
      : reconnectFailed
        ? "已断开"
        : live
          ? lossRate > 2
            ? "较差"
            : frameStats.fps >= 6
              ? "流畅"
              : frameStats.fps > 0
                ? "一般"
                : "连接中"
          : connectingPhase
            ? "连接中"
            : phase === "error"
              ? "异常"
              : "—";
  const qualityTone: "mint" | "amber" | "red" | "neutral" = offline
    ? "neutral"
    : reconnecting || reconnectFailed || phase === "error"
      ? "red"
      : !live
        ? "neutral"
        : lossRate > 2 || frameStats.fps < 6
          ? "amber"
          : "mint";
  // 0–100 的健康度：丢帧率占大头，帧率补足剩余部分。
  const qualityScore = !live
    ? 0
    : Math.max(
        0,
        Math.min(
          100,
          Math.round(
            100 -
              Math.min(100, lossRate * 12) -
              Math.max(0, 30 - frameStats.fps) * 1.5,
          ),
        ),
      );
  const resolutionText =
    live && frameStats.width > 0
      ? `${frameStats.width} × ${frameStats.height}`
      : device.resolution ?? "—";
  const actualStreamText =
    frameStats.width > 0
      ? `实际 ${frameStats.width} × ${frameStats.height} · ${
          frameStats.fps > 0 ? `${frameStats.fps} FPS` : "—"
        }`
      : "等待画面…";
  /** 舞台留白配色：自动跟随画面明暗，也可以手动固定黑/白。 */
  const stageBackground =
    stageTone === "auto"
      ? stageLight
        ? "#f2f3f5"
        : "#050607"
      : stageTone === "light"
        ? "#f2f3f5"
        : "#050607";
  const stageStyle = { "--stage-bg": stageBackground } as CSSProperties;

  const bothInput = keyboardOn && mouseOn;
  const inputBadge = bothInput
    ? "键鼠控制已开启"
    : keyboardOn
      ? "键盘控制已开启"
      : mouseOn
        ? "鼠标控制已开启"
        : null;
  // 握手前不知道被控端能力，先别发：老被控端会回 4100 把整页判成错误。
  const sasPendingCaps = isTauri && peerCaps === null;
  // 直连会话明确不提供键鼠注入，Ctrl+Alt+Del 也不例外。
  const sasDisabled =
    offline || direct || capsBlockSas || sasPendingCaps || sasBusy;

  function queueMove(clientX: number, clientY: number) {
    if (!mouseOnRef.current) return;
    const abs = pointToAbs(clientX, clientY);
    if (!abs) return;
    // 只保留最新坐标，下一帧统一发送，最多约 60 次/秒。
    pendingMoveRef.current = abs;
    if (moveRafRef.current === null) {
      moveRafRef.current = window.requestAnimationFrame(flushMove);
    }
  }

  function handleInputMouseDown(event: ReactMouseEvent<HTMLDivElement>) {
    event.preventDefault();
    inputLayerRef.current?.focus({ preventScroll: true });
    // 点画面即进入键鼠捕获：之后 Tab / 方向键发给远端。
    setCaptured(true);
    escStreakRef.current = 0;
    if (!mouseOnRef.current) return;
    const button = MOUSE_BUTTONS[event.button];
    if (!button) return;
    // 按下位置直接随 button 事件带上，用户不必先移动鼠标。
    const abs = pointToAbs(event.clientX, event.clientY);
    cancelPendingMove();
    pressedButtonsRef.current.add(event.button);
    sendInput([
      { k: "button", button, down: true, ...(abs ? { abs } : {}) },
    ]);
  }

  function handleInputMouseMove(event: ReactMouseEvent<HTMLDivElement>) {
    if (!mouseOnRef.current) return;
    queueMove(event.clientX, event.clientY);
  }

  /**
   * 释放键鼠捕获：把焦点交还给工具栏的「释放键盘」按钮（键盘用户继续 Tab 走），
   * 并主动释放远端所有按下的键，避免卡键。
   */
  function releaseCapture(announce = true) {
    escStreakRef.current = 0;
    setCaptured(false);
    releaseAllRemote();
    releaseBtnRef.current?.focus({ preventScroll: true });
    if (announce) {
      notify("已释放键盘，Tab 与方向键恢复本机操作", "info");
    }
  }

  function handleInputKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (!keyboardOnRef.current) return;
    if (LOCAL_ONLY_KEYS.has(event.code)) return;
    // 焦点在输入框/文本域里时绝不捕获，避免抢走用户正在打的字。
    if (isTypingTarget(document.activeElement)) return;
    // Tab：捕获中发给远端（远端按 Tab 切焦点），未捕获时留给本地焦点切换。
    // 这样键盘用户永远能 Tab 出画面区，不会被困住。
    if (event.code === "Tab" && !capturedRef.current) return;
    // Esc 连按两次退出捕获：第一次照常发给远端，第二次留给本机。
    if (event.code === "Escape" && capturedRef.current) {
      escStreakRef.current += 1;
      if (escTimerRef.current !== null) window.clearTimeout(escTimerRef.current);
      escTimerRef.current = window.setTimeout(() => {
        escStreakRef.current = 0;
        escTimerRef.current = null;
      }, 1500);
      if (escStreakRef.current >= 2) {
        event.preventDefault();
        event.stopPropagation();
        releaseCapture();
        return;
      }
    }
    const mapping = SCAN_CODES[event.code];
    if (!mapping) return;
    event.preventDefault();
    event.stopPropagation();
    // autoRepeat 的 keydown 也要转发：SendInput 不会自行产生按键重复，
    // 由浏览器的重复频率驱动远端连发。Set 只做状态记录，不会重复释放。
    pressedKeysRef.current.add(mapping.scan);
    sendInput([
      { k: "key", scan: mapping.scan, down: true, ext: mapping.ext },
    ]);
  }

  function handleInputKeyUp(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (!keyboardOnRef.current) return;
    if (LOCAL_ONLY_KEYS.has(event.code)) return;
    if (event.code === "Tab" && !captured) return;
    const mapping = SCAN_CODES[event.code];
    if (!mapping) return;
    event.preventDefault();
    event.stopPropagation();
    pressedKeysRef.current.delete(mapping.scan);
    sendInput([
      { k: "key", scan: mapping.scan, down: false, ext: mapping.ext },
    ]);
  }

  function handleInputBlur() {
    releaseAllRemote();
    setCaptured(false);
    escStreakRef.current = 0;
  }

  async function requestScreenshot() {
    setMenuOpen(false);
    // H.264 没有现成的 JPEG：从保存原始画面的离屏画布导出 PNG。
    const source = h264SrcRef.current;
    const dataUrl =
      videoCodecRef.current === "h264" && source && source.width > 0
        ? source.toDataURL("image/png")
        : pendingFrameRef.current;
    if (!dataUrl) {
      notify("还没有可用画面，等画面出来再试", "error");
      return;
    }
    try {
      const path = await saveScreenshot(dataUrl);
      notify(`截图已保存到 ${path}`, "success");
    } catch (error) {
      notify(
        `保存截图失败：${errorLine(error)}`,
        "error",
      );
    }
  }

  async function confirmPower() {
    if (!powerAction || powerBusy) return;
    const token = controlTokenRef.current;
    if (token <= 0) {
      notify("控制会话尚未建立", "error");
      return;
    }
    setPowerBusy(true);
    try {
      await sendPower(token, powerAction);
      setPowerAction(null);
    } catch (error) {
      notify(
        `电源操作失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setPowerBusy(false);
    }
  }

  async function requestSecureAttention() {
    const token = controlTokenRef.current;
    if (token <= 0) {
      notify("控制会话尚未建立", "error");
      return;
    }
    if (sasBusy) return;
    setSasBusy(true);
    // 被控端会回 `sas.ok` 清除忙碌态；旧版本/丢包时兜底解锁，避免按钮一直转圈。
    if (sasBusyTimerRef.current !== null) {
      window.clearTimeout(sasBusyTimerRef.current);
    }
    sasBusyTimerRef.current = window.setTimeout(() => {
      sasBusyTimerRef.current = null;
      setSasBusy(false);
    }, 5000);
    try {
      await sendSecureAttention(token);
    } catch (error) {
      if (sasBusyTimerRef.current !== null) {
        window.clearTimeout(sasBusyTimerRef.current);
        sasBusyTimerRef.current = null;
      }
      setSasBusy(false);
      notify(`发送 Ctrl+Alt+Del 失败：${errorLine(error)}`, "error");
    }
  }

  async function toggleFullscreen() {
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
      } else {
        await document.documentElement.requestFullscreen();
      }
    } catch {
      notify("当前环境不支持全屏模式", "error");
    }
  }

  async function applyStreamOptions(patch: Partial<StreamQuality>) {
    const previous = streamOptionsRef.current;
    const next: StreamQuality = { ...previous, ...patch };
    if (
      next.fps === previous.fps &&
      next.maxWidth === previous.maxWidth &&
      next.quality === previous.quality &&
      next.display === previous.display
    ) {
      return;
    }
    streamOptionsRef.current = next;
    setStreamOptionsState(next);
    setQualitySwitching(true);
    try {
      await setStreamOptions(controlTokenRef.current, next);
      // 切换成功后记住这组偏好，下一台设备 / 下次启动沿用。
      saveStreamOptions(next);
    } catch (error) {
      // 失败时回到切换前的选择，画面保持旧帧不变。
      streamOptionsRef.current = previous;
      setStreamOptionsState(previous);
      notify(
        `切换画面参数失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setQualitySwitching(false);
    }
  }

  function toggleKeyboard() {
    setKeyboardOn((value) => !value);
    notify(keyboardOn ? "键盘控制已关闭" : "键盘控制已开启", "info");
  }

  function toggleMouse() {
    setMouseOn((value) => !value);
    notify(mouseOn ? "鼠标控制已关闭" : "鼠标控制已开启", "info");
  }

  function toggleClipboard() {
    void applyClipboard(!clipboardOn);
  }

  async function toggleAudio() {
    const token = controlTokenRef.current;
    if (token <= 0 || audioBusy) return;
    const next = !audioOn;
    if (next) {
      // AudioContext 必须由用户手势创建，否则会被 WebView 的自动播放策略静音。
      if (!audioPlayerRef.current) {
        audioPlayerRef.current = new RemoteAudioPlayer((message) =>
          notify(`系统声音：${message}`, "error"),
        );
      }
      audioPlayerRef.current.open();
    }
    setAudioBusy(true);
    try {
      // WebView2 支持 WebCodecs 时用 Opus（64 kbps 左右），否则回退原始 PCM。
      const codec = next && (await supportsOpusAudio()) ? "opus" : "pcm16";
      await setAudioEnabled(token, next, codec, audioSourceRef.current);
      setAudioOn(next);
      audioOnRef.current = next;
      if (!next) {
        audioPlayerRef.current?.dispose();
        audioPlayerRef.current = null;
        audioIdRef.current = 0;
        setAudioFormat(null);
      }
      notify(
        next
          ? audioSourceRef.current === "microphone"
            ? "麦克风已开启"
            : "系统声音已开启"
          : "远程声音已关闭",
        "info",
      );
    } catch (error) {
      if (next) {
        audioPlayerRef.current?.dispose();
        audioPlayerRef.current = null;
      }
      notify(
        `切换系统声音失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setAudioBusy(false);
    }
  }

  /** 切换系统声音/麦克风；正在播放时立刻按新来源重新订阅。 */
  async function changeAudioSource(next: AudioSource) {
    if (next === audioSource) return;
    setAudioSource(next);
    audioSourceRef.current = next;
    const token = controlTokenRef.current;
    if (!audioOnRef.current || token <= 0) {
      notify(
        next === "microphone" ? "已切换为麦克风（开启后生效）" : "已切换为系统声音",
        "info",
      );
      return;
    }
    setAudioBusy(true);
    try {
      const codec = (await supportsOpusAudio()) ? "opus" : "pcm16";
      await setAudioEnabled(token, true, codec, next);
      audioIdRef.current = 0;
      setAudioFormat(null);
      notify(next === "microphone" ? "已切换到麦克风" : "已切换到系统声音", "info");
    } catch (error) {
      setAudioSource(audioSource);
      audioSourceRef.current = audioSource;
      notify(
        `切换声音来源失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setAudioBusy(false);
    }
  }

  /** 滑块拖动时合并请求，停下 180 ms 后才真正下发。 */
  function applyVolume(level: number, muted?: boolean) {
    const token = controlTokenRef.current;
    if (token <= 0) return;
    setRemoteVolumeState((current) => ({
      level,
      muted: muted ?? current?.muted ?? false,
    }));
    setVolumeBusy(true);
    if (volumeTimerRef.current !== null) window.clearTimeout(volumeTimerRef.current);
    volumeTimerRef.current = window.setTimeout(() => {
      volumeTimerRef.current = null;
      void setRemoteVolume(token, { level, muted })
        .catch((error) =>
          notify(
            `调整音量失败：${errorLine(error)}`,
            "error",
          ),
        )
        .finally(() => setVolumeBusy(false));
    }, 180);
  }

  function toggleRemoteMute() {
    const token = controlTokenRef.current;
    if (token <= 0) return;
    const muted = !(remoteVolume?.muted ?? false);
    setVolumeBusy(true);
    void setRemoteVolume(token, { muted })
      .catch((error) => {
        setVolumeBusy(false);
        notify(
          `切换静音失败：${errorLine(error)}`,
          "error",
        );
      });
  }

  async function toggleAutoQuality(next: boolean) {
    setAutoQuality(next);
    try {
      const applied = await saveAutoQuality(next);
      setAutoQuality(applied);
      notify(
        applied
          ? "已开启自动适应：按实测帧率调整清晰度"
          : "已关闭自动适应：使用手动画质",
        "info",
      );
    } catch (error) {
      setAutoQuality(!next);
      notify(errorLine(error), "error");
    }
  }

  function setKeyMouse(next: boolean) {
    setKeyboardOn(next);
    setMouseOn(next);
    notify(next ? "键鼠同步已开启" : "键鼠同步已关闭", "info");
  }

  async function applyClipboard(next: boolean) {
    setClipboardOn(next);
    try {
      const applied = await saveClipboardSync(next);
      setClipboardOn(applied);
      notify(applied ? "剪贴板同步已开启" : "剪贴板同步已关闭", "info");
    } catch (error) {
      setClipboardOn(!next);
      notify(errorLine(error), "error");
    }
  }

  function setClipboardSync(next: boolean) {
    void applyClipboard(next);
  }

  return (
    <div className="ctrl-page">
      <header className="ctrl-toolbar">
        <div className="ctrl-tb-left">
          <button
            type="button"
            className="btn btn-ghost btn-sm ctrl-back"
            onClick={onBack}
            title="返回屏幕墙"
          >
            <ChevronLeft size={15} strokeWidth={1.75} />
            屏幕墙
          </button>
          <div className="ctrl-device">
            <div className="ctrl-device-top">
              <h3 title={device.name}>{device.name}</h3>
              <span
                className={`ctrl-status ${offline ? "is-offline" : "is-online"}`}
              >
                <i aria-hidden="true" />
                {offline ? "离线" : "在线"}
              </span>
            </div>
          </div>
        </div>

        <div className="ctrl-metrics" aria-label="连接指标">
          <div className="ctrl-metric">
            <span>延迟</span>
            <strong>{offline || !latencyKnown ? "—" : `${latency} ms`}</strong>
          </div>
          <span className="ctrl-metric-sep" aria-hidden="true" />
          <div className="ctrl-metric">
            <span>FPS</span>
            <strong>{fpsTop}</strong>
          </div>
          <span className="ctrl-metric-sep is-last" aria-hidden="true" />
          <div className="ctrl-metric is-bandwidth">
            <span>码率</span>
            <strong>{bandwidthText}</strong>
          </div>
        </div>

        <div className="ctrl-tb-actions">
          <div className="ctrl-quality-wrap" ref={healthMenuRef}>
            <button
              type="button"
              className={`btn btn-ghost btn-sm ctrl-quality-trigger ctrl-health-trigger ${
                qualityOpen ? "is-active" : ""
              } ${lossRate > 2 ? "is-warning" : ""}`}
              onClick={() => setQualityOpen((value) => !value)}
              title="连接质量"
              aria-label="连接质量"
              aria-haspopup="dialog"
              aria-expanded={qualityOpen}
              disabled={offline}
            >
              <Activity size={15} strokeWidth={1.75} />
              <span>质量</span>
              {lossRate > 2 && (
                <em className="ctrl-health-badge">{lossRate.toFixed(1)}%</em>
              )}
            </button>
            {qualityOpen && (
              <div
                className="ctrl-menu ctrl-quality-panel ctrl-health-panel"
                role="dialog"
                aria-label="连接质量"
              >
                <div className="ctrl-quality-head">
                  <strong>连接质量</strong>
                  <span>
                    {reconnecting
                      ? `重连中（${reconnecting.attempt}/${reconnecting.maxAttempts}）`
                      : quality
                        ? "本次会话累计"
                        : "等待统计…"}
                  </span>
                </div>
                {quality ? (
                  <dl className="ctrl-health-grid">
                    <div>
                      <dt>视频帧</dt>
                      <dd className="mono">{quality.video.frames}</dd>
                    </div>
                    <div>
                      <dt>丢帧</dt>
                      <dd
                        className={`mono ${
                          quality.video.lost > 0 ? "is-warn" : ""
                        }`}
                      >
                        {quality.video.lost}
                      </dd>
                    </div>
                    <div>
                      <dt>丢帧率</dt>
                      <dd className={`mono ${lossRate > 2 ? "is-danger" : ""}`}>
                        {lossRate.toFixed(1)}%
                      </dd>
                    </div>
                    <div>
                      <dt>帧率</dt>
                      <dd className="mono">{quality.video.fps.toFixed(1)} FPS</dd>
                    </div>
                    <div>
                      <dt>码率</dt>
                      <dd className="mono">
                        {quality.video.kbps > 0
                          ? `${Math.round(quality.video.kbps)} kbps`
                          : "—"}
                      </dd>
                    </div>
                    <div>
                      <dt>最大帧间隔</dt>
                      <dd className="mono">{quality.video.maxGapMs} ms</dd>
                    </div>
                    <div>
                      <dt>音频包</dt>
                      <dd className="mono">{quality.audio.packets}</dd>
                    </div>
                    <div>
                      <dt>音频丢包</dt>
                      <dd
                        className={`mono ${
                          quality.audio.lost > 0 ? "is-warn" : ""
                        }`}
                      >
                        {quality.audio.lost}
                      </dd>
                    </div>
                    <div>
                      <dt>音频空档</dt>
                      <dd
                        className={`mono ${
                          quality.audio.gaps > 0 ? "is-warn" : ""
                        }`}
                      >
                        {quality.audio.gaps}
                      </dd>
                    </div>
                    <div>
                      <dt>重连次数</dt>
                      <dd className="mono">{quality.net.reconnects}</dd>
                    </div>
                    <div>
                      <dt>往返时延</dt>
                      <dd className="mono">
                        {quality.net.rttMs === null
                          ? "—"
                          : `${quality.net.rttMs} ms`}
                      </dd>
                    </div>
                    <div>
                      <dt>已持续</dt>
                      <dd className="mono">{sessionDuration}</dd>
                    </div>
                  </dl>
                ) : (
                  <p className="ctrl-health-empty">
                    画面开始后这里会显示真实的丢帧、丢包与往返时延。
                  </p>
                )}
                <p className="ctrl-quality-tip">
                  丢帧按画面帧序号缺口计算，重复或迟到的帧不会误报；
                  丢包率超过 2% 时入口会变警示色。
                </p>
              </div>
            )}
          </div>
          <div className="ctrl-quality-wrap" ref={qualityMenuRef}>
            <button
              type="button"
              className={`btn btn-ghost btn-sm ctrl-quality-trigger ${
                qualityMenuOpen ? "is-active" : ""
              }`}
              onClick={() => setQualityMenuOpen((value) => !value)}
              title="画面设置"
              aria-label="画面设置"
              aria-haspopup="dialog"
              aria-expanded={qualityMenuOpen}
              disabled={offline}
            >
              <Gauge size={15} strokeWidth={1.75} />
              <span>画面</span>
            </button>
            {qualityMenuOpen && (
              <div
                className="ctrl-menu ctrl-quality-panel"
                role="dialog"
                aria-label="画面设置"
                aria-busy={qualitySwitching}
              >
                <div className="ctrl-quality-head">
                  <strong>画面设置</strong>
                  <span className={qualitySwitching ? "is-switching" : ""}>
                    {qualitySwitching ? "正在切换…" : actualStreamText}
                  </span>
                </div>

                {displays.length > 0 && (
                  <div className="ctrl-quality-group">
                    <span className="ctrl-quality-label">显示器</span>
                    <div className="ctrl-quality-grid is-display">
                      {displays.map((item) => (
                        <button
                          key={item.index}
                          type="button"
                          className={`ctrl-quality-option ${
                            streamOptions.display === item.index
                              ? "is-active"
                              : ""
                          }`}
                          aria-pressed={streamOptions.display === item.index}
                          disabled={qualitySwitching || offline}
                          onClick={() =>
                            void applyStreamOptions({ display: item.index })
                          }
                        >
                          <span>显示器 {item.index + 1}</span>
                          <small>
                            {item.primary ? "主屏 · " : ""}
                            {item.w}×{item.h}
                          </small>
                        </button>
                      ))}
                      {displays.length > 1 && (
                        <button
                          type="button"
                          className={`ctrl-quality-option ${
                            streamOptions.display === DISPLAY_ALL
                              ? "is-active"
                              : ""
                          }`}
                          aria-pressed={streamOptions.display === DISPLAY_ALL}
                          disabled={qualitySwitching || offline}
                          onClick={() =>
                            void applyStreamOptions({
                              display: DISPLAY_ALL,
                            })
                          }
                        >
                          <span>全部屏幕</span>
                          <small>多屏拼接</small>
                        </button>
                      )}
                    </div>
                  </div>
                )}

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">自适应</span>
                  <div className="ctrl-quality-auto">
                    <Switch
                      checked={autoQuality}
                      onChange={(next) => void toggleAutoQuality(next)}
                      label="自动适应网络"
                    />
                    <p className="ctrl-quality-auto-hint">
                      按实测帧率在清晰度之间自动取舍，跑不满目标帧率时先降清晰度。
                    </p>
                  </div>
                </div>

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">分辨率</span>
                  <div className="ctrl-quality-grid is-res">
                    {RESOLUTION_OPTIONS.map((option) => (
                      <button
                        key={option.maxWidth}
                        type="button"
                        className={`ctrl-quality-option ${
                          streamOptions.maxWidth === option.maxWidth
                            ? "is-active"
                            : ""
                        }`}
                        aria-pressed={
                          streamOptions.maxWidth === option.maxWidth
                        }
                        disabled={qualitySwitching || offline}
                        onClick={() =>
                          void applyStreamOptions({
                            maxWidth: option.maxWidth,
                          })
                        }
                      >
                        <span>{option.maxWidth}</span>
                        <small>{option.hint}</small>
                      </button>
                    ))}
                  </div>
                </div>

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">帧率</span>
                  <div className="ctrl-quality-grid is-fps">
                    {FPS_OPTIONS.map((option) => (
                      <button
                        key={option}
                        type="button"
                        className={`ctrl-quality-option ${
                          streamOptions.fps === option ? "is-active" : ""
                        }`}
                        aria-pressed={streamOptions.fps === option}
                        disabled={qualitySwitching || offline}
                        onClick={() =>
                          void applyStreamOptions({ fps: option })
                        }
                      >
                        <span>{option === 0 ? "自动" : option}</span>
                        <small>{option === 0 ? "不限" : "FPS"}</small>
                      </button>
                    ))}
                  </div>
                </div>

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">画质</span>
                  <div className="ctrl-quality-grid is-quality">
                    {QUALITY_OPTIONS.map((option) => (
                      <button
                        key={option.quality}
                        type="button"
                        className={`ctrl-quality-option ${
                          streamOptions.quality === option.quality
                            ? "is-active"
                            : ""
                        }`}
                        aria-pressed={streamOptions.quality === option.quality}
                        disabled={qualitySwitching || offline}
                        onClick={() =>
                          void applyStreamOptions({
                            quality: option.quality,
                          })
                        }
                      >
                        <span>{option.label}</span>
                        <small>Q{option.quality}</small>
                      </button>
                    ))}
                  </div>
                </div>

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">画面填充</span>
                  <div className="ctrl-quality-grid is-fit">
                    {(
                      [
                        { mode: "cover", label: "铺满", hint: "裁边" },
                        { mode: "contain", label: "适应", hint: "完整" },
                      ] as const
                    ).map((option) => (
                      <button
                        key={option.mode}
                        type="button"
                        className={`ctrl-quality-option ${
                          fitMode === option.mode ? "is-active" : ""
                        }`}
                        aria-pressed={fitMode === option.mode}
                        onClick={() => setFitMode(option.mode)}
                      >
                        <span>{option.label}</span>
                        <small>{option.hint}</small>
                      </button>
                    ))}
                  </div>
                </div>

                <div className="ctrl-quality-group">
                  <span className="ctrl-quality-label">画面留白</span>
                  <div className="ctrl-quality-grid is-tone">
                    {(
                      [
                        { tone: "auto", label: "自动" },
                        { tone: "dark", label: "黑色" },
                        { tone: "light", label: "白色" },
                      ] as const
                    ).map((option) => (
                      <button
                        key={option.tone}
                        type="button"
                        className={`ctrl-quality-option ${
                          stageTone === option.tone ? "is-active" : ""
                        }`}
                        aria-pressed={stageTone === option.tone}
                        onClick={() => setStageTone(option.tone)}
                      >
                        <span>{option.label}</span>
                      </button>
                    ))}
                  </div>
                </div>

                <p className="ctrl-quality-tip">
                  自动会跟随被控画面明暗切换黑底或白底。4K 或 60 FPS
                  会显著增加带宽与 CPU，公网建议先用 1080p/30。
                </p>
              </div>
            )}
          </div>

          <span className="ctrl-tb-sep" />
          <button
            type="button"
            className="icon-btn ctrl-icon"
            onClick={toggleFullscreen}
            title={fullscreen ? "退出全屏" : "全屏显示"}
            aria-label={fullscreen ? "退出全屏" : "全屏显示"}
          >
            {fullscreen ? (
              <Minimize2 size={16} strokeWidth={1.75} />
            ) : (
              <Maximize2 size={16} strokeWidth={1.75} />
            )}
          </button>

          <div className="ctrl-more" ref={menuRef}>
            <button
              type="button"
              className={`icon-btn ctrl-icon ${menuOpen ? "is-active" : ""}`}
              onClick={() => setMenuOpen((value) => !value)}
              title="更多操作"
              aria-label="更多操作"
              aria-expanded={menuOpen}
            >
              <MoreHorizontal size={17} strokeWidth={1.75} />
            </button>
            {menuOpen && (
              <div className="ctrl-menu" role="menu">
                <button
                  type="button"
                  role="menuitem"
                  disabled={offline}
                  onClick={() => void requestScreenshot()}
                >
                  <Camera size={14} strokeWidth={1.75} />
                  屏幕截图
                </button>
                <div className="ctrl-menu-sep" />
                <button
                  type="button"
                  role="menuitem"
                  disabled={offline}
                  onClick={() => {
                    setMenuOpen(false);
                    setPowerAction("restart");
                  }}
                >
                  <RotateCcw size={14} strokeWidth={1.75} />
                  重启设备
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="is-danger"
                  disabled={offline}
                  onClick={() => {
                    setMenuOpen(false);
                    setPowerAction("shutdown");
                  }}
                >
                  <PowerOff size={14} strokeWidth={1.75} />
                  关闭设备
                </button>
              </div>
            )}
          </div>

          <button
            type="button"
            className={`icon-btn ctrl-icon ctrl-tb-side ${sideOpen ? "is-active" : ""}`}
            onClick={() => setSideOpen((value) => !value)}
            title={sideOpen ? "收起信息面板" : "展开信息面板"}
            aria-label={sideOpen ? "收起信息面板" : "展开信息面板"}
            aria-pressed={sideOpen}
          >
            <PanelRight size={16} strokeWidth={1.75} />
          </button>
        </div>
      </header>

      {/* 断线自动重连：重连中与失败后各一条横幅，成功后短暂提示「已恢复」。 */}
      {reconnecting && (
        <div className="ctrl-banner is-reconnecting" role="status" aria-live="polite">
          <RefreshCw size={15} strokeWidth={1.75} className="spin" />
          <span>
            连接已断开，正在重连
            {reconnecting.attempt > 0
              ? `（${reconnecting.attempt}/${reconnecting.maxAttempts}）`
              : ""}
            …
          </span>
          <span className="ctrl-banner-hint">
            {reconnecting.delayMs > 0
              ? `下次尝试还有约 ${Math.max(1, Math.round(reconnecting.delayMs / 1000))} 秒`
              : "等待被控端响应"}
          </span>
        </div>
      )}
      {!reconnecting && reconnectFailed && (
        <div className="ctrl-banner is-failed" role="alert">
          <AlertCircle size={15} strokeWidth={1.75} />
          <span>
            重连失败：{describeError(reconnectFailed.message ?? "").title}
          </span>
          <button
            type="button"
            className="btn btn-primary btn-sm"
            onClick={() => {
              setReconnectFailed(null);
              setReconnecting(null);
              // 直连会话：后端重连已经放弃，这里用长期 token 重新直连一次；
              // 中转会话：重跑本页的会话 effect（重新向服务器发 open）。
              if (direct) {
                void reconnectDirect(deviceId)
                  .then(() => setRetryKey((value) => value + 1))
                  .catch((error) => notify(errorLine(error), "error"));
              } else {
                setRetryKey((value) => value + 1);
              }
            }}
          >
            <RefreshCw size={14} strokeWidth={1.75} />
            重试
          </button>
        </div>
      )}
      {!reconnecting && !reconnectFailed && recovered && (
        <div className="ctrl-banner is-recovered" role="status" aria-live="polite">
          <ShieldCheck size={15} strokeWidth={1.75} />
          <span>连接已恢复</span>
        </div>
      )}

      <div className="ctrl-body">
        <main className="ctrl-main">
          <div className="ctrl-stage-wrap">
            <div
              ref={stageRef}
              className={`ctrl-screen ctrl-stage ${live ? "is-live" : ""} ${
                stageTone === "light" || (stageTone === "auto" && stageLight)
                  ? "is-light"
                  : ""
              }`}
              style={stageStyle}
            >
              {hasFrame ? (
                videoCodec === "h264" ? (
                  <canvas
                    ref={screenCanvasRef}
                    className="ctrl-live-frame ctrl-video"
                    role="img"
                    aria-label={`${device.name} 实时画面`}
                  />
                ) : (
                  <img
                    ref={screenRef}
                    className="ctrl-live-frame ctrl-video"
                    style={{ objectFit: fitMode }}
                    alt={`${device.name} 实时画面`}
                    draggable={false}
                  />
                )
              ) : (
                <DesktopPreview device={device} large live={!offline} />
              )}
              {canInject && (
                <div
                  ref={inputLayerRef}
                  className="ctrl-input-layer"
                  tabIndex={0}
                  role="application"
                  aria-label="远程键鼠输入区域"
                  onMouseDown={handleInputMouseDown}
                  onMouseMove={handleInputMouseMove}
                  onContextMenu={(event) => event.preventDefault()}
                  onAuxClick={(event) => event.preventDefault()}
                  onDragStart={(event) => event.preventDefault()}
                  onKeyDown={handleInputKeyDown}
                  onKeyUp={handleInputKeyUp}
                  onBlur={handleInputBlur}
                />
              )}
              <span className={`ctrl-stage-badge ${streaming ? "is-live" : ""}`}>
                <i />
                {streaming ? "实时" : reconnecting ? "重连中" : "预览"}
              </span>
              {live && (
                <span className="ctrl-stage-meta">
                  {frameStats.width > 0
                    ? `${frameStats.width} × ${frameStats.height}`
                    : resolutionText}
                  <span className="ctrl-stage-meta-dot" aria-hidden="true">
                    ·
                  </span>
                  {frameStats.fps > 0 ? `${frameStats.fps} FPS` : "—"}
                </span>
              )}
              {/* 重连/失败时键鼠注入已关闭，这里明确告诉用户原因。 */}
              {!canInject && (reconnecting || reconnectFailed) && live && (
                <span className="ctrl-input-hint">
                  {reconnecting ? "重连期间键鼠已暂停" : "重连失败，键鼠已暂停"}
                </span>
              )}
              {canInject &&
                (inputBadge ? (
                  <span className="ctrl-input-badge">
                    <i />
                    {inputBadge}
                    {captured ? " · 捕获中" : ""}
                  </span>
                ) : (
                  <span className="ctrl-input-hint">
                    打开下方键鼠开关后可控制
                  </span>
                ))}
              {canInject && captured && (
                <button
                  ref={releaseBtnRef}
                  type="button"
                  className="ctrl-release-btn"
                  onClick={() => releaseCapture()}
                  title="释放键盘捕获（连按两次 Esc 也可以）"
                  aria-label="释放键盘捕获"
                >
                  释放键盘
                </button>
              )}
              {!offline && phase === "error" && !reconnecting && (
                <div className="ctrl-stream-error ctrl-state-block">
                  <AlertCircle
                    size={18}
                    strokeWidth={1.75}
                    className="ctrl-state-icon is-danger"
                  />
                  <h4>画面接入失败</h4>
                  <p>{streamError ?? "请稍后重试"}</p>
                  {/* 重试：中转会话重新开一条；直连会话重新走一次 connect_direct
                      （用长期 token 免配对码，地址就在设备 ID 里）。 */}
                  <button
                    type="button"
                    className="btn btn-primary btn-sm"
                    onClick={() => {
                      if (direct) {
                        void reconnectDirect(deviceId)
                          .then(() => setRetryKey((value) => value + 1))
                          .catch((error) => notify(errorLine(error), "error"));
                      } else {
                        setRetryKey((value) => value + 1);
                      }
                    }}
                  >
                    <RefreshCw size={14} strokeWidth={1.75} />
                    重新连接
                  </button>
                  <button
                    type="button"
                    className="btn btn-ghost btn-sm"
                    onClick={onBack}
                  >
                    <ChevronLeft size={14} strokeWidth={1.75} />
                    返回屏幕墙
                  </button>
                </div>
              )}
              {offline && (
                <div className="ctrl-offline ctrl-state-block">
                  <MonitorOff
                    size={18}
                    strokeWidth={1.75}
                    className="ctrl-state-icon"
                  />
                  <h4>设备离线</h4>
                  <p>
                    最后上线：{formatLastSeen(device.lastSeen)}。可在「设备」页对离线设备发送
                    Wake-on-LAN 唤醒包。
                  </p>
                </div>
              )}
              {!offline && connectingPhase && (
                <div className="ctrl-connecting ctrl-state-block" role="status">
                  <ShieldCheck
                    size={18}
                    strokeWidth={1.75}
                    className="ctrl-state-icon"
                  />
                  <strong>
                    {phase === "waiting"
                      ? "正在获取实时画面…"
                      : "正在等待被控端确认…"}
                  </strong>
                  <p>
                    {device.name} ·{" "}
                    {phase === "waiting"
                      ? "已建立会话，等待首帧画面"
                      : "请在目标设备上点击「允许控制」"}
                  </p>
                </div>
              )}
            </div>
          </div>

          <div className="ctrl-dock" role="toolbar" aria-label="远程操作">
            <button
              type="button"
              className={`ctrl-dock-btn ${keyboardOn ? "is-on" : ""}`}
              disabled={offline}
              onClick={toggleKeyboard}
              title="键盘控制"
              aria-label="键盘控制"
              aria-pressed={keyboardOn}
            >
              <Keyboard size={16} strokeWidth={1.75} />
              <span className="ctrl-dock-label">键盘</span>
            </button>
            <button
              type="button"
              className={`ctrl-dock-btn ${mouseOn ? "is-on" : ""}`}
              disabled={offline}
              onClick={toggleMouse}
              title="鼠标控制"
              aria-label="鼠标控制"
              aria-pressed={mouseOn}
            >
              <MousePointer2 size={16} strokeWidth={1.75} />
              <span className="ctrl-dock-label">鼠标</span>
            </button>
            <button
              type="button"
              className={`ctrl-dock-btn ${clipboardOn ? "is-on" : ""}`}
              disabled={offline}
              onClick={toggleClipboard}
              title="剪贴板同步"
              aria-label="剪贴板同步"
              aria-pressed={clipboardOn}
            >
              <Clipboard size={16} strokeWidth={1.75} />
              <span className="ctrl-dock-label">剪贴板</span>
            </button>
            <button
              type="button"
              className={`ctrl-dock-btn ${audioOn ? "is-on" : ""}`}
              disabled={offline || audioBusy || (!audioOn && phase !== "live")}
              onClick={() => void toggleAudio()}
              title={
                audioOn
                  ? `关闭远程声音${audioFormat ? `（${audioFormat}）` : ""}`
                  : "开启远程声音（优先 Opus 压缩）"
              }
              aria-label={audioOn ? "关闭远程声音" : "开启远程声音"}
              aria-pressed={audioOn}
            >
              {audioOn ? (
                <Volume2 size={16} strokeWidth={1.75} />
              ) : (
                <VolumeX size={16} strokeWidth={1.75} />
              )}
              <span className="ctrl-dock-label">声音</span>
            </button>
            <span className="ctrl-dock-sep" />
            <button
              type="button"
              className="ctrl-dock-btn"
              disabled={offline}
              onClick={requestScreenshot}
              title="屏幕截图"
              aria-label="屏幕截图"
            >
              <Camera size={16} strokeWidth={1.75} />
              <span className="ctrl-dock-label">截图</span>
            </button>
            <button
              type="button"
              className="ctrl-dock-btn"
              onClick={onFiles}
              title="文件传输"
              aria-label="文件传输"
            >
              <FolderUp size={16} strokeWidth={1.75} />
              <span className="ctrl-dock-label">文件</span>
            </button>
            <span className="ctrl-dock-sep" />
            <button
              type="button"
              className={`ctrl-dock-btn ${fullscreen ? "is-on" : ""}`}
              onClick={toggleFullscreen}
              title={fullscreen ? "退出全屏" : "全屏显示"}
              aria-label={fullscreen ? "退出全屏" : "全屏显示"}
            >
              {fullscreen ? (
                <Minimize2 size={16} strokeWidth={1.75} />
              ) : (
                <Maximize2 size={16} strokeWidth={1.75} />
              )}
              <span className="ctrl-dock-label">全屏</span>
            </button>
          </div>
        </main>

        <aside className={`ctrl-side ${sideOpen ? "" : "is-hidden"}`}>
          <section className="ctrl-section">
            <h4 className="ctrl-section-title">设备信息</h4>
            <dl className="ctrl-info-list">
              <div>
                <dt>系统</dt>
                <dd>{device.os || "—"}</dd>
              </div>
              <div>
                <dt>版本</dt>
                <dd className="mono">{device.version || "—"}</dd>
              </div>
              {device.location && (
                <div>
                  <dt>位置</dt>
                  <dd title={device.location}>{device.location}</dd>
                </div>
              )}
              <div>
                <dt>IP 地址</dt>
                <dd className="mono">
                  {deviceInfo?.ip || device.ip || "—"}
                </dd>
              </div>
              <div>
                <dt>分辨率</dt>
                <dd className="mono">{resolutionText}</dd>
              </div>
              <div>
                <dt>显卡</dt>
                <dd title={deviceInfo?.gpu || device.gpu}>
                  {deviceInfo?.gpu || device.gpu || "—"}
                </dd>
              </div>
              <div>
                <dt>最后在线</dt>
                <dd>{offline ? formatLastSeen(device.lastSeen) : "当前在线"}</dd>
              </div>
            </dl>
          </section>

          <section className="ctrl-section">
            <h4 className="ctrl-section-title">会话统计</h4>
            <div className="ctrl-quality-meter">
              <div className="ctrl-quality-meter-top">
                <span>连接质量</span>
                <strong className={`is-${qualityTone}`}>
                  {healthLabel}
                  {offline || !latencyKnown ? "" : ` · ${latency} ms`}
                </strong>
              </div>
              <Progress value={qualityScore} tone={qualityTone === "red" ? "red" : "neutral"} />
            </div>
            <dl className="ctrl-info-list">
              <div>
                <dt>连接方式</dt>
                <dd>
                  <Wifi size={14} strokeWidth={1.75} aria-hidden="true" />
                  {linkMode === "direct"
                    ? "局域网直连"
                    : linkMode === "p2p"
                      ? "P2P 直连"
                      : "服务器中转"}
                </dd>
              </div>
              <div>
                <dt>延迟</dt>
                <dd>{offline || !latencyKnown ? "—" : `${latency} ms`}</dd>
              </div>
              <div>
                <dt>丢帧率</dt>
                <dd className={lossRate > 2 ? "is-warn" : ""}>
                  {quality ? `${lossRate.toFixed(1)}%` : "—"}
                </dd>
              </div>
              <div>
                <dt>重连</dt>
                <dd>
                  {quality && quality.net.reconnects > 0
                    ? `${quality.net.reconnects} 次`
                    : "未发生"}
                </dd>
              </div>
              <div>
                <dt>带宽</dt>
                <dd>{bandwidthText}</dd>
              </div>
              <div>
                <dt>帧率</dt>
                <dd>{fpsDetail}</dd>
              </div>
              <div>
                <dt>编码</dt>
                <dd>{live ? "MJPEG" : "—"}</dd>
              </div>
              <div>
                <dt>CPU</dt>
                <dd>{deviceInfo ? `${deviceInfo.cpu.toFixed(0)}%` : "—"}</dd>
              </div>
              <div>
                <dt>内存</dt>
                <dd>{deviceInfo ? `${deviceInfo.mem.toFixed(0)}%` : "—"}</dd>
              </div>
            </dl>
          </section>

          <section className="ctrl-section">
            <h4 className="ctrl-section-title">快捷操作</h4>
            <div className="ctrl-quick-grid">
              <button
                type="button"
                className="ctrl-quick-btn"
                onClick={onFiles}
                title="文件传输"
              >
                <FolderUp size={14} strokeWidth={1.75} />
                文件传输
              </button>
              <button
                type="button"
                className="ctrl-quick-btn"
                disabled={offline}
                onClick={() => void requestScreenshot()}
                title="屏幕截图"
              >
                <Camera size={14} strokeWidth={1.75} />
                屏幕截图
              </button>
              <button
                type="button"
                className="ctrl-quick-btn"
                disabled={sasDisabled}
                onClick={() => void requestSecureAttention()}
                title={
                  capsBlockSas
                    ? "被控端版本较旧，不支持 Ctrl+Alt+Del"
                    : sasPendingCaps
                      ? "正在确认被控端能力，稍后再试"
                      : "向被控端发送 Ctrl+Alt+Del"
                }
              >
                <KeyRound size={14} strokeWidth={1.75} />
                {sasBusy ? "发送中…" : "Ctrl+Alt+Del"}
              </button>
              <button
                type="button"
                className="ctrl-quick-btn"
                disabled={offline}
                onClick={() => setPowerAction("restart")}
                title="重启设备"
              >
                <RotateCcw size={14} strokeWidth={1.75} />
                重启
              </button>
              <button
                type="button"
                className="ctrl-quick-btn is-danger is-wide"
                disabled={offline}
                onClick={() => setPowerAction("shutdown")}
                title="关闭设备"
              >
                <PowerOff size={14} strokeWidth={1.75} />
                关机
              </button>
            </div>
            <p className="ctrl-section-hint">
              {direct
                ? "局域网直连不提供键鼠控制，Ctrl+Alt+Del 不可用。"
                : capsBlockSas
                  ? "被控端版本较旧，不支持 Ctrl+Alt+Del 请求；升级被控端后可用。"
                  : sasPendingCaps
                    ? "正在确认被控端能力，握手完成后可用。"
                    : uipiBlocked
                      ? "被控端有提权窗口，输入已被系统拦截；需要以管理员身份运行被控端（或安装为服务）。"
                      : "Ctrl+Alt+Del 需要被控端以服务/SYSTEM 模式运行，或由系统策略允许；否则只会作为普通按键发送，无法进入安全桌面。"}
            </p>
          </section>

          <section className="ctrl-section">
            <h4 className="ctrl-section-title">远程声音</h4>
            <Segmented
              size="sm"
              value={audioSource}
              options={[
                { value: "loopback", label: "系统声音", icon: <MonitorSpeaker size={14} strokeWidth={1.75} /> },
                { value: "microphone", label: "麦克风", icon: <Mic size={14} strokeWidth={1.75} /> },
              ]}
              onChange={(value) => void changeAudioSource(value)}
            />
            <div className="ctrl-volume">
              <button
                type="button"
                className={`ctrl-volume-mute ${remoteVolume?.muted ? "is-on" : ""}`}
                disabled={offline || volumeBusy || volumeUnsupported || capsBlockVolume}
                onClick={toggleRemoteMute}
                title={remoteVolume?.muted ? "取消静音" : "静音"}
                aria-label={remoteVolume?.muted ? "取消静音" : "静音"}
                aria-pressed={remoteVolume?.muted ?? false}
              >
                {remoteVolume?.muted ? (
                  <VolumeX size={16} strokeWidth={1.75} />
                ) : (
                  <Volume2 size={16} strokeWidth={1.75} />
                )}
              </button>
              <input
                type="range"
                className="ctrl-volume-range"
                min={0}
                max={100}
                step={1}
                disabled={
                  offline ||
                  volumeBusy ||
                  volumeUnsupported ||
                  capsBlockVolume ||
                  remoteVolume === null
                }
                value={Math.round((remoteVolume?.level ?? 0) * 100)}
                onChange={(event) => applyVolume(Number(event.target.value) / 100)}
                aria-label="被控端音量"
              />
              <span className="ctrl-volume-value mono">
                {remoteVolume === null ? "—" : `${Math.round(remoteVolume.level * 100)}%`}
              </span>
            </div>
            <p className="ctrl-section-hint">
              {capsBlockVolume
                ? "被控端未声明音量联动能力，本次会话不可用。"
                : volumeUnsupported
                  ? "被控端版本较旧，暂不支持音量联动；升级被控端后可用。"
                  : "音量与被控端扬声器同步；切换来源会重新建立声音订阅。"}
            </p>
          </section>

          <section className="ctrl-section">
            <h4 className="ctrl-section-title">输入与剪贴板</h4>
            <div className="ctrl-switch-list">
              <Switch
                checked={bothInput}
                disabled={offline}
                onChange={setKeyMouse}
                label="键鼠同步"
              />
              <Switch
                checked={clipboardOn}
                disabled={offline}
                onChange={setClipboardSync}
                label="剪贴板同步"
              />
            </div>
            {uipiBlocked && (
              <p className="ctrl-section-hint">
                被控端有提权窗口，键鼠输入被系统拦截；需要以管理员身份运行被控端（或安装为服务）。
              </p>
            )}
          </section>

          <div className="ctrl-secure">
            <ShieldCheck size={14} strokeWidth={1.75} />
            {linkMode === "direct"
              ? "画面与输入走局域网直连，不经服务器"
              : linkMode === "p2p"
                ? "画面与输入走 P2P 直连（UDP 打洞），不经服务器中转"
                : "画面与输入经服务器中转"}
          </div>
        </aside>
      </div>

      <Modal
        open={powerAction !== null}
        onClose={() => {
          if (!powerBusy) setPowerAction(null);
        }}
        title={powerAction === "shutdown" ? "关闭设备" : "重启设备"}
        subtitle={`目标：${device.name}`}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              disabled={powerBusy}
              onClick={() => setPowerAction(null)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={powerBusy}
              onClick={() => void confirmPower()}
            >
              {powerBusy
                ? "发送中…"
                : powerAction === "shutdown"
                  ? "确认关机"
                  : "确认重启"}
            </button>
          </>
        }
      >
        <p className="ctrl-power-hint">
          {powerAction === "shutdown"
            ? "被控端会在 3 秒后关机，正在运行的程序可能被中断。"
            : "被控端会在 3 秒后重启，未保存的工作可能丢失。"}
          确认后会话会随设备离线而断开。
        </p>
      </Modal>
    </div>
  );
}
