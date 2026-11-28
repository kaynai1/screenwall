import {
  ChevronLeft,
  Clipboard,
  Download,
  Gauge,
  Keyboard,
  Loader2,
  Maximize,
  Minimize,
  Monitor,
  MousePointer2,
  PictureInPicture2,
  RefreshCw,
  SlidersHorizontal,
  Upload,
  Volume2,
  VolumeX,
  Wifi,
  X,
  ZoomOut,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type * as React from "react";
import type { DeviceView, Viewer } from "../api";
import { RemoteAudioPlayer, supportsOpusAudio } from "../audio";
import { RemoteFiles } from "../download";
import { describeCode } from "../errors";
import { uploadFiles } from "../files";
import {
  InputBatcher,
  pointToNormalized,
  SCAN_CODES,
  wheelNotches,
} from "../input";
import { CODEC_OPUS, CODEC_PCM16, MEDIA_AUDIO, MEDIA_VIDEO, DISPLAY_ALL } from "../protocol";
import { VideoRenderer } from "../video";
import { WEB_VERSION } from "../version";
import FileBrowser from "./FileBrowser";

type Phase = "opening" | "waiting" | "streaming" | "error";

type StreamOptions = { maxWidth: number; fps: number; quality: number; preferH264: boolean };

const OPTIONS_KEY = "screenwall.web.stream";
const RESOLUTIONS = [
  { value: 1280, label: "标清" },
  { value: 1920, label: "高清" },
  { value: 2560, label: "2K" },
  { value: 3840, label: "4K" },
];
const FPS_OPTIONS = [15, 30, 60];
/** MJPEG 的码率几乎只由 JPEG 质量决定，手机上必须让用户能压下来。 */
const QUALITY_OPTIONS = [
  { value: 45, label: "省流量" },
  { value: 65, label: "均衡" },
  { value: 80, label: "清晰" },
];

const DEFAULT_OPTIONS: StreamOptions = {
  maxWidth: 1280,
  fps: 30,
  quality: 65,
  preferH264: true,
};

/** 被控端的显示器信息（与 `DisplayInfo` 对应）。 */
type DisplayInfo = { index: number; w: number; h: number; primary: boolean; x?: number; y?: number };

/** 触摸方式：绝对坐标（点哪里去哪里）或触控板（相对移动）。 */
type TouchMode = "absolute" | "trackpad";

const TOUCH_KEY = "screenwall.web.touch";
/** 双指缩放的上限；再大就只剩马赛克了。 */
const MAX_ZOOM = 4;

function readTouchMode(): TouchMode {
  try {
    return localStorage.getItem(TOUCH_KEY) === "trackpad" ? "trackpad" : "absolute";
  } catch {
    return "absolute";
  }
}

function readOptions(): StreamOptions {
  try {
    const text = localStorage.getItem(OPTIONS_KEY);
    if (!text) return DEFAULT_OPTIONS;
    const value = JSON.parse(text) as Partial<StreamOptions>;
    // 旧版本存过 75 之类的任意值，吸附到最近的档位，保证界面上有一项是选中的。
    const rawQuality = typeof value.quality === "number" ? value.quality : null;
    const quality = QUALITY_OPTIONS.reduce(
      (best, item) =>
        rawQuality !== null && Math.abs(item.value - rawQuality) < Math.abs(best.value - rawQuality)
          ? item
          : best,
      QUALITY_OPTIONS[1],
    ).value;
    return {
      maxWidth: RESOLUTIONS.some((item) => item.value === value.maxWidth)
        ? (value.maxWidth as number)
        : DEFAULT_OPTIONS.maxWidth,
      fps: FPS_OPTIONS.includes(value.fps as number) ? (value.fps as number) : DEFAULT_OPTIONS.fps,
      quality,
      preferH264: value.preferH264 !== false,
    };
  } catch {
    return DEFAULT_OPTIONS;
  }
}

/** 码率上万时换成 Mbps，手机上顶栏才不会挤成一行数字。 */
function formatRate(kbps: number): string {
  if (kbps >= 1000) return `${(kbps / 1000).toFixed(1)} Mbps`;
  return `${Math.round(kbps)} kbps`;
}

function formatBytes(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(1)} GB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`;
  if (bytes >= 1 << 10) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} B`;
}

function saveOptions(options: StreamOptions): void {
  try {
    localStorage.setItem(OPTIONS_KEY, JSON.stringify(options));
  } catch {
    // 忽略。
  }
}

export default function ControlPage({
  viewer,
  device,
  onBack,
  notify,
}: {
  viewer: Viewer;
  device: DeviceView;
  onBack: () => void;
  notify: (text: string) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const [renderer] = useState(() => new VideoRenderer());
  const [phase, setPhase] = useState<Phase>("opening");
  const [errorText, setErrorText] = useState("");
  const [options, setOptions] = useState<StreamOptions>(readOptions);
  const [stats, setStats] = useState({ fps: 0, kbps: 0, rtt: 0 });
  const [actual, setActual] = useState<{ w: number; h: number; fps: number; codec: string } | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const [keyboardOpen, setKeyboardOpen] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  /** 右下角悬浮菜单：触控板模式、显示器、声音、断开。 */
  const [panelOpen, setPanelOpen] = useState(false);
  const [touchMode, setTouchMode] = useState<TouchMode>(readTouchMode);
  const [displays, setDisplays] = useState<DisplayInfo[]>([]);
  const [display, setDisplay] = useState(0);
  const [audioOn, setAudioOn] = useState(false);
  const [audioInfo, setAudioInfo] = useState<{ codec: string; channels: number } | null>(null);
  const [volume, setVolume] = useState<{ level: number; muted: boolean } | null>(null);
  /** 上传进度；`null` 表示当前没有上传。 */
  const [upload, setUpload] = useState<{ sent: number; total: number } | null>(null);
  /** 画面是否被放大/平移过（显示「还原」按钮用）。 */
  const [zoomed, setZoomed] = useState(false);
  const uploadInputRef = useRef<HTMLInputElement | null>(null);
  const [filesOpen, setFilesOpen] = useState(false);
  const [remoteFiles] = useState(() => new RemoteFiles((message) => viewer.sendJson(message)));

  const optionsRef = useRef(options);
  optionsRef.current = options;
  const displayRef = useRef(display);
  displayRef.current = display;
  const touchModeRef = useRef(touchMode);
  touchModeRef.current = touchMode;
  const audioOnRef = useRef(audioOn);
  audioOnRef.current = audioOn;
  const audioPlayerRef = useRef<RemoteAudioPlayer | null>(null);
  const audioIdRef = useRef(0);
  const audioFormatRef = useRef({ sampleRate: 48_000, channels: 2 });
  const viewRef = useRef({ scale: 1, x: 0, y: 0 });
  const subIdRef = useRef(0);
  const phaseRef = useRef<Phase>("opening");
  phaseRef.current = phase;
  const batcherRef = useRef<InputBatcher | null>(null);
  const rttRef = useRef(0);

  const sendStream = useCallback(
    (next: StreamOptions) => {
      // H.264 属于 v1 基线（不走能力协商），只要浏览器有 WebCodecs 就能试；
      // 真解不出来时被控端会回 4104，那时再降回 MJPEG。
      const h264 = next.preferH264 && typeof VideoDecoder !== "undefined";
      viewer.sendJson({
        t: "video.sub",
        stream: "focus",
        display: displayRef.current,
        fps: next.fps,
        maxWidth: next.maxWidth,
        quality: next.quality,
        codec: h264 ? "h264" : "mjpeg",
        cursor: true,
      });
    },
    [viewer],
  );

  const resubscribe = useCallback(
    (next: StreamOptions) => {
      setOptions(next);
      optionsRef.current = next;
      saveOptions(next);
      if (subIdRef.current) {
        viewer.sendJson({ t: "video.unsub", id: subIdRef.current });
        subIdRef.current = 0;
      }
      renderer.reset();
      sendStream(next);
    },
    [renderer, sendStream, viewer],
  );

  const start = useCallback(() => {
    setPhase("opening");
    setErrorText("");
    renderer.reset();
    const run = async () => {
      if (!viewer.isOpen) await viewer.connect();
      await viewer.open(device.id);
    };
    run().catch((error: unknown) => {
      setPhase("error");
      const raw = error instanceof Error ? error.message : String(error);
      const match = raw.match(/^(\d{4})[:：]\s*(.*)$/);
      if (match) {
        const copy = describeCode(Number(match[1]), match[2]);
        setErrorText(`${copy.title}：${copy.detail}`);
      } else {
        setErrorText(raw);
      }
    });
  }, [device.id, renderer, viewer]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas) renderer.attach(canvas);
    renderer.onH264Unavailable = () => {
      notify("H.264 解码不可用，已切回 MJPEG");
      const next = { ...optionsRef.current, preferH264: false };
      resubscribe(next);
    };

    const batcher = new InputBatcher((events) => {
      viewer.sendJson({ t: "input", events });
    });
    batcherRef.current = batcher;

    // React 的 onWheel 挂在根节点且是 passive，preventDefault 不生效；
    // 这里直接给画布挂非 passive 监听，滚动不会带动页面。
    const onWheelNative = (event: WheelEvent) => {
      if (phaseRef.current !== "streaming") return;
      event.preventDefault();
      batcherRef.current?.push({
        k: "wheel",
        delta: [wheelNotches(event.deltaX), -wheelNotches(event.deltaY)],
      });
    };
    canvas?.addEventListener("wheel", onWheelNative, { passive: false });

    const off = viewer.subscribe({
      onOpen: () => {
        setPhase("waiting");
        viewer.sendJson({
          t: "hello",
          proto: 1,
          token: "",
          client: { name: "网页端", version: WEB_VERSION },
          caps: ["clipboard.text"],
        });
      },
      onMessage: (message) => {
        // 目录浏览与下载的消息由 RemoteFiles 消化，别让控制页当成未知消息。
        if (remoteFiles.handle(message)) return;
        switch (message.t) {
          case "welcome": {
            sendStream(optionsRef.current);
            // 被控端在 welcome 之后会主动推 display.list，不需要（协议里也不能）主动请求。
            break;
          }
          case "display.list": {
            const list = Array.isArray(message.displays) ? (message.displays as DisplayInfo[]) : [];
            setDisplays(list);
            break;
          }
          case "audio.sub.ok": {
            const info = message.actual as
              | { codec?: string; sampleRate?: number; channels?: number }
              | undefined;
            const id = Number(message.id ?? 0);
            audioIdRef.current = id;
            audioFormatRef.current = {
              sampleRate: Number(info?.sampleRate ?? 48_000),
              channels: Number(info?.channels ?? 2),
            };
            setAudioInfo({
              codec: String(info?.codec ?? "opus"),
              channels: audioFormatRef.current.channels,
            });
            break;
          }
          case "audio.volume.ok": {
            setVolume({
              level: Number(message.level ?? 0),
              muted: Boolean(message.muted),
            });
            break;
          }
          case "video.sub.ok": {
            const info = message.actual as { codec?: string; w?: number; h?: number; fps?: number } | undefined;
            subIdRef.current = Number(message.id ?? 0);
            setActual({
              w: Number(info?.w ?? 0),
              h: Number(info?.h ?? 0),
              fps: Number(info?.fps ?? 0),
              codec: String(info?.codec ?? "mjpeg"),
            });
            setPhase("streaming");
            break;
          }
          case "pong": {
            const sent = Number(message.ts ?? 0);
            if (sent) rttRef.current = Math.max(0, Date.now() - sent);
            break;
          }
          case "clipboard": {
            const text = typeof message.text === "string" ? message.text : "";
            if (!text) break;
            navigator.clipboard
              ?.writeText(text)
              .then(() => notify("已同步对方剪贴板"))
              .catch(() => notify("对方剪贴板已更新（浏览器未授权读取）"));
            break;
          }
          case "input.status": {
            if (message.uipi) notify("远端窗口权限更高，键鼠可能被系统拦截");
            break;
          }
          case "error": {
            const code = Number(message.code ?? 0);
            const copy = describeCode(code, String(message.msg ?? ""));
            if (code === 4104 && subIdRef.current === 0) {
              // H.264 编码器不可用：退回 MJPEG 再试一次。
              resubscribe({ ...optionsRef.current, preferH264: false });
              break;
            }
            notify(`${copy.title}：${copy.detail}`);
            break;
          }
          case "video.unsub.ok":
            break;
          default:
            break;
        }
      },
      onFrame: (frame) => {
        if (frame.type === MEDIA_VIDEO && frame.codec !== undefined) {
          renderer.push(frame);
          return;
        }
        if (frame.type === MEDIA_AUDIO && audioOnRef.current) {
          const player = audioPlayerRef.current;
          if (!player) return;
          const { sampleRate, channels } = audioFormatRef.current;
          if (frame.codec === CODEC_OPUS) player.pushOpus(frame.payload, sampleRate, channels);
          else if (frame.codec === CODEC_PCM16) player.pushPcm(frame.payload, sampleRate, channels);
        }
      },
      onClosed: (_code, msg) => {
        setPhase("error");
        setErrorText(`会话已断开：${msg}`);
      },
      onSocketClosed: (reason) => {
        setPhase("error");
        setErrorText(reason);
      },
    });

    start();

    const statsTimer = window.setInterval(() => {
      const sample = renderer.sample();
      setStats({ ...sample, rtt: rttRef.current });
    }, 1000);
    const pingTimer = window.setInterval(() => {
      if (subIdRef.current) viewer.sendJson({ t: "ping", ts: Date.now() });
    }, 5000);
    // 每 2 秒报一次播放缓冲，被控端据此调 Opus 码率（协议里的 audio.stats）。
    const audioStatsTimer = window.setInterval(() => {
      const player = audioPlayerRef.current;
      const id = audioIdRef.current;
      if (!audioOnRef.current || !player || !id) return;
      const buffered = player.bufferMs;
      if (buffered > 0) viewer.sendJson({ t: "audio.stats", id, bufferedMs: buffered });
    }, 2000);

    return () => {
      off();
      canvas?.removeEventListener("wheel", onWheelNative);
      window.clearInterval(statsTimer);
      window.clearInterval(pingTimer);
      window.clearInterval(audioStatsTimer);
      if (volumeTimer.current) window.clearTimeout(volumeTimer.current);
      if (audioIdRef.current) viewer.sendJson({ t: "audio.unsub", id: audioIdRef.current });
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      viewer.sendJson({ t: "input", events: [{ k: "releaseAll" }] });
      remoteFiles.dispose();
      viewer.closeSession();
      renderer.detach();
    };
    // 只挂载一次：设备/视图在控制页生命周期内不变。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 键盘：桌面浏览器按扫描码注入，软键盘走 Text。
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (phaseRef.current !== "streaming") return;
      const target = event.target as HTMLElement | null;
      if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return;
      if (event.key === "F11") {
        event.preventDefault();
        void toggleFullscreen();
        return;
      }
      if (event.repeat) return;
      const mapping = SCAN_CODES[event.code];
      if (!mapping) return;
      if (event.ctrlKey && event.code === "KeyV") return;
      event.preventDefault();
      batcherRef.current?.push({ k: "key", scan: mapping.scan, down: true, ext: mapping.ext });
      batcherRef.current?.flush();
    };
    const onKeyUp = (event: KeyboardEvent) => {
      if (phaseRef.current !== "streaming") return;
      const mapping = SCAN_CODES[event.code];
      if (!mapping) return;
      event.preventDefault();
      batcherRef.current?.push({ k: "key", scan: mapping.scan, down: false, ext: mapping.ext });
      batcherRef.current?.flush();
    };
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
    };
  }, []);

  useEffect(() => {
    const onFullscreen = () => setFullscreen(Boolean(document.fullscreenElement));
    document.addEventListener("fullscreenchange", onFullscreen);
    return () => document.removeEventListener("fullscreenchange", onFullscreen);
  }, []);

  async function toggleFullscreen() {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else await (stageRef.current ?? document.documentElement).requestFullscreen();
    } catch {
      notify("浏览器不允许进入全屏");
    }
  }

  // ---------------------------------------------------------------- 指针 / 触摸

  const pointers = useRef(new Map<number, { x: number; y: number }>());
  /** 鼠标按下的是哪个键：抬起时必须回同一个键，否则右键会变成松开左键。 */
  const downButtons = useRef(new Map<number, "left" | "right" | "middle">());
  /**
   * 手势状态机。
   *
   * - `absolute`：点哪里去哪里。
   * - `trackpad`：相对移动（单指拖动 = 移动鼠标，轻点 = 左键）。
   * - `panview`：画面被放大后，单指/双指用来平移画面，不动远端鼠标。
   * - `pinch` / `scroll`：双指缩放 / 双指滚动。
   * - `ignore`：双指抬起一根后，剩下那根不再触发任何动作。
   */
  const gesture = useRef({
    longPress: 0,
    moved: false,
    suppressUp: false,
    twoFinger: false,
    mode: "" as "" | "absolute" | "trackpad" | "panview" | "pinch" | "scroll" | "ignore",
    startDist: 0,
    startScale: 1,
    startView: { x: 0, y: 0 },
    startMid: { x: 0, y: 0 },
    lastMid: { x: 0, y: 0 },
    scrollAccX: 0,
    scrollAccY: 0,
    lastX: 0,
    lastY: 0,
  });

  const TRACKPAD_GAIN = 1.6;

  function normalized(event: React.PointerEvent<HTMLCanvasElement>) {
    const canvas = canvasRef.current;
    if (!canvas) return null;
    return pointToNormalized(
      canvas.getBoundingClientRect(),
      event.clientX,
      event.clientY,
      canvas.width,
      canvas.height,
    );
  }

  function midPoint(): { x: number; y: number } {
    const values = Array.from(pointers.current.values());
    if (values.length < 2) return { x: 0, y: 0 };
    return { x: (values[0].x + values[1].x) / 2, y: (values[0].y + values[1].y) / 2 };
  }

  function pinchDistance(): number {
    const values = Array.from(pointers.current.values());
    if (values.length < 2) return 0;
    return Math.hypot(values[0].x - values[1].x, values[0].y - values[1].y);
  }

  function onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
    if (phaseRef.current !== "streaming") return;
    event.preventDefault();
    const canvas = event.currentTarget;
    canvas.setPointerCapture(event.pointerId);
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    const abs = normalized(event);

    if (event.pointerType === "mouse") {
      const button: "left" | "right" | "middle" =
        event.button === 2 ? "right" : event.button === 1 ? "middle" : "left";
      downButtons.current.set(event.pointerId, button);
      batcherRef.current?.push({ k: "button", button, down: true, abs: abs ?? undefined });
      batcherRef.current?.flush();
      return;
    }

    if (pointers.current.size === 2) {
      // 两指：先不决定是缩放还是滚动，等移动方向明确后再说。
      window.clearTimeout(gesture.current.longPress);
      gesture.current.moved = false;
      gesture.current.twoFinger = true;
      gesture.current.suppressUp = true;
      gesture.current.mode = "";
      gesture.current.startDist = pinchDistance();
      gesture.current.startScale = viewRef.current.scale;
      gesture.current.startView = { ...viewRef.current };
      gesture.current.startMid = midPoint();
      gesture.current.lastMid = gesture.current.startMid;
      gesture.current.scrollAccX = 0;
      gesture.current.scrollAccY = 0;
      // 第一根手指可能已经按下了左键，先松开，避免远端卡在拖拽状态。
      batcherRef.current?.push({ k: "button", button: "left", down: false });
      batcherRef.current?.flush();
      return;
    }

    if (pointers.current.size > 2) return;

    gesture.current.moved = false;
    gesture.current.suppressUp = false;
    gesture.current.lastX = event.clientX;
    gesture.current.lastY = event.clientY;

    // 画面放大后，单指用来拖动查看，不再移动远端鼠标。
    if (viewRef.current.scale > 1.01) {
      gesture.current.mode = "panview";
      return;
    }

    if (touchModeRef.current === "trackpad") {
      gesture.current.mode = "trackpad";
      gesture.current.longPress = window.setTimeout(() => {
        if (gesture.current.moved) return;
        // 触控板模式下长按 = 右键单击（当前位置）。
        gesture.current.suppressUp = true;
        batcherRef.current?.push({ k: "button", button: "right", down: true });
        batcherRef.current?.push({ k: "button", button: "right", down: false });
        batcherRef.current?.flush();
      }, 550);
      return;
    }

    gesture.current.mode = "absolute";
    if (abs) {
      batcherRef.current?.push({ k: "move", abs });
      batcherRef.current?.push({ k: "button", button: "left", down: true, abs });
      batcherRef.current?.flush();
    }
    gesture.current.longPress = window.setTimeout(() => {
      if (gesture.current.moved || gesture.current.twoFinger) return;
      // 长按 = 右键。
      gesture.current.suppressUp = true;
      batcherRef.current?.push({ k: "button", button: "left", down: false });
      batcherRef.current?.push({ k: "button", button: "right", down: true, abs: abs ?? undefined });
      batcherRef.current?.push({ k: "button", button: "right", down: false });
      batcherRef.current?.flush();
    }, 550);
  }

  function onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
    if (phaseRef.current !== "streaming") return;
    event.preventDefault();
    if (gesture.current.mode === "ignore") return;
    const entry = pointers.current.get(event.pointerId);
    if (entry) {
      entry.x = event.clientX;
      entry.y = event.clientY;
    } else if (event.pointerType === "mouse") {
      const abs = normalized(event);
      if (abs) batcherRef.current?.push({ k: "move", abs });
      return;
    }

    if (gesture.current.twoFinger) {
      const mid = midPoint();
      const dist = pinchDistance();
      const state = gesture.current;

      // 第一次出现明显位移时，按「距离变化」还是「中点移动」判定手势。
      if (state.mode === "") {
        const dDist = Math.abs(dist - state.startDist);
        const dMid = Math.hypot(mid.x - state.startMid.x, mid.y - state.startMid.y);
        if (dDist > 8 && dDist > dMid) {
          state.mode = "pinch";
        } else if (dMid > 8) {
          state.mode = state.startScale > 1.01 ? "panview" : "scroll";
        }
      }

      if (state.mode === "pinch") {
        const stage = stageRef.current?.getBoundingClientRect();
        const cx = stage ? stage.left + stage.width / 2 : window.innerWidth / 2;
        const cy = stage ? stage.top + stage.height / 2 : window.innerHeight / 2;
        const scale = Math.min(
          MAX_ZOOM,
          Math.max(1, state.startScale * (dist / Math.max(1, state.startDist))),
        );
        const ratio = scale / state.startScale;
        // 让捏合开始时中点下的画面内容始终跟着手指走。
        const dx0 = state.startMid.x - cx - state.startView.x;
        const dy0 = state.startMid.y - cy - state.startView.y;
        viewRef.current = {
          scale,
          x: mid.x - cx - dx0 * ratio,
          y: mid.y - cy - dy0 * ratio,
        };
        if (scale <= 1.01) viewRef.current = { scale: 1, x: 0, y: 0 };
        applyView();
      } else if (state.mode === "panview") {
        viewRef.current.x += mid.x - state.lastMid.x;
        viewRef.current.y += mid.y - state.lastMid.y;
        applyView();
      } else if (state.mode === "scroll") {
        state.scrollAccX += mid.x - state.lastMid.x;
        state.scrollAccY += mid.y - state.lastMid.y;
        while (Math.abs(state.scrollAccY) >= 30) {
          const direction = state.scrollAccY > 0 ? 1 : -1;
          state.scrollAccY -= direction * 30;
          batcherRef.current?.push({ k: "wheel", delta: [0, -direction * 120] });
        }
        while (Math.abs(state.scrollAccX) >= 40) {
          const direction = state.scrollAccX > 0 ? 1 : -1;
          state.scrollAccX -= direction * 40;
          batcherRef.current?.push({ k: "wheel", delta: [direction * 120, 0] });
        }
      }
      state.lastMid = mid;
      return;
    }

    if (gesture.current.mode === "panview") {
      viewRef.current.x += event.clientX - gesture.current.lastX;
      viewRef.current.y += event.clientY - gesture.current.lastY;
      gesture.current.lastX = event.clientX;
      gesture.current.lastY = event.clientY;
      gesture.current.moved = true;
      applyView();
      return;
    }

    if (gesture.current.mode === "trackpad") {
      const dx = (event.clientX - gesture.current.lastX) * TRACKPAD_GAIN;
      const dy = (event.clientY - gesture.current.lastY) * TRACKPAD_GAIN;
      gesture.current.lastX = event.clientX;
      gesture.current.lastY = event.clientY;
      if (dx || dy) {
        batcherRef.current?.push({ k: "move", rel: [dx, dy] });
        if (Math.abs(dx) + Math.abs(dy) > 2) {
          gesture.current.moved = true;
          window.clearTimeout(gesture.current.longPress);
        }
      }
      return;
    }

    const abs = normalized(event);
    if (!abs) return;
    const start = pointers.current.get(event.pointerId);
    if (start) {
      // 有位移就算拖拽，不再触发长按右键。
      if (Math.abs(start.x - event.clientX) + Math.abs(start.y - event.clientY) > 6) {
        gesture.current.moved = true;
        window.clearTimeout(gesture.current.longPress);
      }
    }
    batcherRef.current?.push({ k: "move", abs });
  }

  function endPointer(event: React.PointerEvent<HTMLCanvasElement>) {
    pointers.current.delete(event.pointerId);
    window.clearTimeout(gesture.current.longPress);
    if (phaseRef.current !== "streaming") return;
    const state = gesture.current;

    if (state.twoFinger) {
      if (pointers.current.size < 2) {
        state.twoFinger = false;
        state.suppressUp = false;
        state.mode = pointers.current.size > 0 ? "ignore" : "";
        // 缩放回到 1 倍时清掉平移偏移，画面重新居中。
        if (viewRef.current.scale <= 1.01 && (viewRef.current.x || viewRef.current.y)) {
          viewRef.current = { scale: 1, x: 0, y: 0 };
          applyView();
        }
      }
      return;
    }

    if (state.mode === "ignore") {
      if (pointers.current.size === 0) state.mode = "";
      return;
    }

    if (state.mode === "panview") {
      // 放大状态下轻点仍然是一次左键单击，按下位置按当前缩放映射。
      if (!state.moved) {
        const abs = normalized(event);
        if (abs) {
          batcherRef.current?.push({ k: "button", button: "left", down: true, abs });
          batcherRef.current?.push({ k: "button", button: "left", down: false, abs });
          batcherRef.current?.flush();
        }
      }
      state.mode = pointers.current.size > 0 ? "ignore" : "";
      return;
    }

    if (state.suppressUp) {
      state.suppressUp = false;
      state.mode = "";
      return;
    }

    if (state.mode === "trackpad") {
      // 轻点 = 左键单击（当前光标位置）。
      if (!state.moved) {
        batcherRef.current?.push({ k: "button", button: "left", down: true });
        batcherRef.current?.push({ k: "button", button: "left", down: false });
        batcherRef.current?.flush();
      }
      state.mode = "";
      return;
    }

    const abs = normalized(event);
    const button = downButtons.current.get(event.pointerId) ?? "left";
    downButtons.current.delete(event.pointerId);
    batcherRef.current?.push({ k: "button", button, down: false, abs: abs ?? undefined });
    batcherRef.current?.flush();
    state.mode = "";
  }

  function onContextMenu(event: React.MouseEvent<HTMLCanvasElement>) {
    event.preventDefault();
  }

  // ---------------------------------------------------------------- 软键盘

  function onBeforeInput(event: React.FormEvent<HTMLTextAreaElement>) {
    const native = event.nativeEvent as InputEvent;
    if (native.inputType === "insertText" && native.data) {
      event.preventDefault();
      batcherRef.current?.push({ k: "text", text: native.data });
      batcherRef.current?.flush();
    } else if (native.inputType === "deleteContentBackward") {
      event.preventDefault();
      batcherRef.current?.push({ k: "key", scan: 0x0e, down: true });
      batcherRef.current?.push({ k: "key", scan: 0x0e, down: false });
      batcherRef.current?.flush();
    } else if (native.inputType === "insertLineBreak") {
      event.preventDefault();
      batcherRef.current?.push({ k: "key", scan: 0x1c, down: true });
      batcherRef.current?.push({ k: "key", scan: 0x1c, down: false });
      batcherRef.current?.flush();
    }
  }

  function sendComposition(event: React.CompositionEvent<HTMLTextAreaElement>) {
    if (event.data) {
      batcherRef.current?.push({ k: "text", text: event.data });
      batcherRef.current?.flush();
    }
  }

  function toggleKeyboard() {
    setKeyboardOpen((current) => {
      const next = !current;
      if (next) window.setTimeout(() => inputRef.current?.focus(), 30);
      return next;
    });
  }

  async function sendClipboard() {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) {
        notify("本机剪贴板为空");
        return;
      }
      viewer.sendJson({ t: "clipboard", text });
      notify("已发送到对方剪贴板");
    } catch {
      notify("浏览器未授权读取剪贴板");
    }
  }

  function quickKey(scan: number, ext = false) {
    batcherRef.current?.push({ k: "key", scan, down: true, ext });
    batcherRef.current?.push({ k: "key", scan, down: false, ext });
    batcherRef.current?.flush();
  }

  // ---------------------------------------------------------------- 声音 / 显示器 / 缩放

  async function toggleAudio() {
    if (audioOnRef.current) {
      setAudioOn(false);
      audioOnRef.current = false;
      if (audioIdRef.current) {
        viewer.sendJson({ t: "audio.unsub", id: audioIdRef.current });
        audioIdRef.current = 0;
      }
      audioPlayerRef.current?.dispose();
      audioPlayerRef.current = null;
      setAudioInfo(null);
      return;
    }
    // AudioContext 必须在用户手势里创建/恢复，否则自动播放策略会静音。
    if (!audioPlayerRef.current) {
      audioPlayerRef.current = new RemoteAudioPlayer((text) => notify(text));
    }
    audioPlayerRef.current.open();
    const opus = await supportsOpusAudio();
    viewer.sendJson({
      t: "audio.sub",
      codec: opus ? "opus" : "pcm16",
      source: "loopback",
    });
    viewer.sendJson({ t: "audio.volume.get" });
    setAudioOn(true);
    audioOnRef.current = true;
  }

  const volumeTimer = useRef(0);

  function pushVolume(next: { level: number; muted: boolean }) {
    setVolume(next);
    // 滑块拖动时先本地即时反馈，停下约 150 ms 再下发，避免高频写系统音量。
    if (volumeTimer.current) window.clearTimeout(volumeTimer.current);
    volumeTimer.current = window.setTimeout(() => {
      viewer.sendJson({ t: "audio.volume", level: next.level, muted: next.muted });
    }, 150);
  }

  async function sendFiles(files: File[]) {
    if (files.length === 0) return;
    const total = files.reduce((sum, file) => sum + file.size, 0);
    setUpload({ sent: 0, total });
    try {
      const result = await uploadFiles(viewer, files, (sent, all) =>
        setUpload({ sent, total: all }),
      );
      notify(`已发送 ${result.files} 个文件（${formatBytes(result.bytes)}）`);
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    } finally {
      setUpload(null);
    }
  }

  function chooseDisplay(next: number) {
    if (next === displayRef.current) return;
    setDisplay(next);
    displayRef.current = next;
    // 切换显示器会换一路流，和改画质一样重新订阅。
    resubscribe(optionsRef.current);
  }

  function chooseTouchMode(next: TouchMode) {
    setTouchMode(next);
    touchModeRef.current = next;
    try {
      localStorage.setItem(TOUCH_KEY, next);
    } catch {
      // 隐私模式下忽略。
    }
  }

  function applyView() {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const { scale, x, y } = viewRef.current;
    canvas.style.transform =
      scale === 1 && x === 0 && y === 0 ? "" : `translate(${x}px, ${y}px) scale(${scale})`;
    setZoomed(scale > 1.01);
  }

  function resetZoom() {
    viewRef.current = { scale: 1, x: 0, y: 0 };
    applyView();
  }

  const pipRef = useRef<HTMLVideoElement | null>(null);

  async function togglePip() {
    if (document.pictureInPictureElement) {
      await document.exitPictureInPicture().catch(() => undefined);
      return;
    }
    const canvas = canvasRef.current;
    const capture = canvas?.captureStream?.bind(canvas);
    if (!canvas || !capture || !document.pictureInPictureEnabled) {
      notify("当前浏览器不支持画中画");
      return;
    }
    try {
      let video = pipRef.current;
      if (!video) {
        video = document.createElement("video");
        video.muted = true;
        video.playsInline = true;
        pipRef.current = video;
      }
      video.srcObject = capture(30);
      await video.play();
      await video.requestPictureInPicture();
    } catch {
      notify("当前浏览器不支持画中画");
    }
  }

  const codecLabel = actual?.codec === "h264" ? "H.264" : "MJPEG";

  return (
    <div className="control-page" ref={stageRef}>
      <div className="stage">
        <canvas
          ref={canvasRef}
          className="screen"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={endPointer}
          onPointerCancel={endPointer}
          onContextMenu={onContextMenu}
        />

        {phase !== "streaming" && (
          <div className="stage-overlay">
            {phase === "error" ? (
              <>
                <strong>连不上这台设备</strong>
                <span>{errorText}</span>
                <div className="stage-actions">
                  <button type="button" className="ghost-btn" onClick={start}>
                    <RefreshCw size={15} /> 重试
                  </button>
                  <button type="button" className="ghost-btn" onClick={onBack}>
                    返回
                  </button>
                </div>
              </>
            ) : (
              <>
                <Loader2 size={22} className="spin" />
                <span>
                  {phase === "opening" ? "正在请求连接…" : "等待被控端确认 / 首帧…"}
                </span>
              </>
            )}
          </div>
        )}
      </div>

      <header className="control-bar">
        <button type="button" className="icon-btn" onClick={onBack} aria-label="返回">
          <ChevronLeft size={20} />
        </button>
        <div className="control-title">
          <strong>{device.name}</strong>
          <small>
            <Wifi size={11} />
            {stats.fps} fps · {formatRate(stats.kbps)}
            {stats.rtt ? ` · ${stats.rtt}ms` : ""}
            {actual ? ` · ${actual.w}×${actual.h} ${codecLabel}` : ""}
          </small>
        </div>
        <button
          type="button"
          className={`icon-btn ${menuOpen ? "is-on" : ""}`}
          onClick={() => setMenuOpen((current) => !current)}
          aria-label="画质"
        >
          <Gauge size={19} />
        </button>
      </header>

      {menuOpen && (
        <div className="quality-sheet">
          <div className="sheet-title">画质</div>
          <div className="chip-row">
            {RESOLUTIONS.map((item) => (
              <button
                type="button"
                key={item.value}
                className={`chip ${options.maxWidth === item.value ? "is-on" : ""}`}
                onClick={() => resubscribe({ ...options, maxWidth: item.value })}
              >
                {item.label}
              </button>
            ))}
          </div>
          <div className="sheet-title">帧率</div>
          <div className="chip-row">
            {FPS_OPTIONS.map((fps) => (
              <button
                type="button"
                key={fps}
                className={`chip ${options.fps === fps ? "is-on" : ""}`}
                onClick={() => resubscribe({ ...options, fps })}
              >
                {fps} FPS
              </button>
            ))}
          </div>
          <div className="sheet-title">压缩</div>
          <div className="chip-row">
            {QUALITY_OPTIONS.map((item) => (
              <button
                type="button"
                key={item.value}
                className={`chip ${options.quality === item.value ? "is-on" : ""}`}
                onClick={() => resubscribe({ ...options, quality: item.value })}
              >
                {item.label}
              </button>
            ))}
          </div>
          <label className="toggle-row">
            <span>可用时使用 H.264</span>
            <input
              type="checkbox"
              checked={options.preferH264}
              onChange={(event) => resubscribe({ ...options, preferH264: event.target.checked })}
            />
          </label>
        </div>
      )}

      <footer className="control-dock">
        <button type="button" className="dock-btn" onClick={toggleKeyboard}>
          <Keyboard size={20} />
          <span>键盘</span>
        </button>
        <button type="button" className="dock-btn" onClick={sendClipboard}>
          <Clipboard size={20} />
          <span>粘贴</span>
        </button>
        <button type="button" className="dock-btn" onClick={toggleFullscreen}>
          {fullscreen ? <Minimize size={20} /> : <Maximize size={20} />}
          <span>全屏</span>
        </button>
        <button type="button" className="dock-btn" onClick={() => quickKey(0x01)} aria-label="Esc">
          <span className="dock-key">Esc</span>
        </button>
        <button type="button" className="dock-btn" onClick={() => quickKey(0x1c)}>
          <span className="dock-key">↵</span>
          <span>回车</span>
        </button>
      </footer>

      {keyboardOpen && (
        <div className="keyboard-dock">
          <textarea
            ref={inputRef}
            rows={1}
            className="keyboard-input"
            placeholder="在这里输入，实时发送到远端…"
            onBeforeInput={onBeforeInput}
            onCompositionEnd={sendComposition}
            onChange={() => {}}
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
          />
          <button type="button" className="ghost-btn" onClick={() => setKeyboardOpen(false)}>
            收起
          </button>
        </div>
      )}

      {zoomed && (
        <button type="button" className="zoom-reset" onClick={resetZoom}>
          <ZoomOut size={15} />
          还原
        </button>
      )}

      <button
        type="button"
        className={`fab ${panelOpen ? "is-on" : ""}`}
        onClick={() => setPanelOpen((current) => !current)}
        aria-label="更多操作"
      >
        {panelOpen ? <X size={20} /> : <SlidersHorizontal size={20} />}
      </button>

      {panelOpen && (
        <div className="control-panel">
          <div className="sheet-title">
            <MousePointer2 size={13} /> 触摸模式
          </div>
          <div className="chip-row">
            <button
              type="button"
              className={`chip ${touchMode === "absolute" ? "is-on" : ""}`}
              onClick={() => chooseTouchMode("absolute")}
            >
              绝对
            </button>
            <button
              type="button"
              className={`chip ${touchMode === "trackpad" ? "is-on" : ""}`}
              onClick={() => chooseTouchMode("trackpad")}
            >
              触控板
            </button>
          </div>

          {displays.length > 0 && (
            <>
              <div className="sheet-title">
                <Monitor size={13} /> 显示器
              </div>
              <div className="chip-row">
                {displays.map((item) => (
                  <button
                    type="button"
                    key={item.index}
                    className={`chip ${display === item.index ? "is-on" : ""}`}
                    onClick={() => chooseDisplay(item.index)}
                  >
                    {item.primary ? "主屏" : `屏幕 ${item.index + 1}`}
                  </button>
                ))}
                {displays.length > 1 && (
                  <button
                    type="button"
                    className={`chip ${display === DISPLAY_ALL ? "is-on" : ""}`}
                    onClick={() => chooseDisplay(DISPLAY_ALL)}
                  >
                    全部
                  </button>
                )}
              </div>
            </>
          )}

          <div className="sheet-title">
            <Upload size={13} /> 文件
          </div>
          <div className="chip-row">
            <button
              type="button"
              className="chip"
              disabled={upload !== null}
              onClick={() => uploadInputRef.current?.click()}
            >
              <Upload size={13} />
              {upload ? "发送中…" : "发送文件到电脑"}
            </button>
            <button
              type="button"
              className="chip"
              onClick={() => {
                setPanelOpen(false);
                setFilesOpen(true);
              }}
            >
              <Download size={13} />
              从电脑下载
            </button>
          </div>
          {upload && (
            <div className="upload-bar" aria-label="上传进度">
              <span
                style={{
                  width: `${upload.total ? Math.min(100, (upload.sent / upload.total) * 100) : 0}%`,
                }}
              />
            </div>
          )}
          <input
            ref={uploadInputRef}
            type="file"
            multiple
            hidden
            onChange={(event) => {
              const files = Array.from(event.target.files ?? []);
              event.target.value = "";
              void sendFiles(files);
            }}
          />

          <div className="sheet-title">
            <Volume2 size={13} /> 声音
          </div>
          <div className="chip-row">
            <button
              type="button"
              className={`chip ${audioOn ? "is-on" : ""}`}
              onClick={() => void toggleAudio()}
            >
              {audioOn ? "关闭声音" : "开启声音"}
            </button>
            {audioOn && (
              <span className="panel-note">
                {audioInfo ? `${audioInfo.codec.toUpperCase()} · ${audioInfo.channels} 声道` : "正在连接…"}
              </span>
            )}
          </div>

          {audioOn && volume && (
            <label className="volume-row">
              <button
                type="button"
                className="icon-btn"
                onClick={() => pushVolume({ ...volume, muted: !volume.muted })}
                aria-label={volume.muted ? "取消静音" : "静音"}
              >
                {volume.muted ? <VolumeX size={16} /> : <Volume2 size={16} />}
              </button>
              <input
                type="range"
                min={0}
                max={100}
                value={Math.round(volume.level * 100)}
                onChange={(event) =>
                  pushVolume({ ...volume, level: Number(event.target.value) / 100 })
                }
              />
              <span>{Math.round(volume.level * 100)}%</span>
            </label>
          )}

          <div className="chip-row panel-actions">
            <button type="button" className="chip" onClick={() => void togglePip()}>
              <PictureInPicture2 size={13} /> 画中画
            </button>
            <button
              type="button"
              className="chip is-danger"
              onClick={() => {
                setPanelOpen(false);
                onBack();
              }}
            >
              <X size={13} /> 断开
            </button>
          </div>
        </div>
      )}

      {filesOpen && <FileBrowser files={remoteFiles} onClose={() => setFilesOpen(false)} />}
    </div>
  );
}
