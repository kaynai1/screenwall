/**
 * 群控：选中的每台在线设备各开一条独立控制会话。
 *
 * 画面用低帧率缩略流铺成网格，任意一张卡上的触摸 / 鼠标 / 键盘
 * 都按归一化坐标广播到全部会话——一次操作，多台同步。
 * 与桌面端群控一致：被控端仍按各自授权策略逐台确认。
 */

import {
  Check,
  ChevronLeft,
  Keyboard,
  Loader2,
  Monitor,
  Square,
  UsersRound,
  WifiOff,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Viewer, type Account, type DeviceView } from "../api";
import { describeCode } from "../errors";
import {
  InputBatcher,
  pointToNormalized,
  wheelNotches,
  type InputEvent as RemoteInput,
} from "../input";
import { MEDIA_VIDEO } from "../protocol";
import { VideoRenderer } from "../video";
import { WEB_VERSION } from "../version";

type CardState = "connecting" | "waiting" | "live" | "error";

type Sink = (message: Record<string, unknown>) => void;

function describeThrown(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error);
  const match = raw.match(/^(\d{4})[:：]\s*(.*)$/);
  if (!match) return raw;
  const copy = describeCode(Number(match[1]), match[2]);
  return `${copy.title}：${copy.detail}`;
}

const STATE_TEXT: Record<CardState, string> = {
  connecting: "连接中…",
  waiting: "等待画面…",
  live: "已同步",
  error: "未加入",
};

function GroupCard({
  account,
  device,
  active,
  onActivate,
  onState,
  registerSink,
  push,
}: {
  account: Account;
  device: DeviceView;
  active: boolean;
  onActivate: () => void;
  onState: (id: string, state: CardState, error: string) => void;
  registerSink: (id: string, sink: Sink | null) => void;
  push: (event: RemoteInput, immediate?: boolean) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const [state, setState] = useState<CardState>("connecting");
  const [error, setError] = useState("");
  const stateRef = useRef(state);
  stateRef.current = state;

  useEffect(() => {
    onState(device.id, state, error);
  }, [device.id, state, error, onState]);

  // 单卡输入状态：双指滚动、长按右键、拖动同步，规则与控制页一致但更轻。
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef({
    timer: 0,
    moved: false,
    dragging: false,
    longPressed: false,
    twoFinger: false,
    scrollAcc: 0,
    startX: 0,
    startY: 0,
    lastMidY: 0,
    startAbs: null as [number, number] | null,
    button: "left" as "left" | "right" | "middle",
  });

  useEffect(() => {
    const canvas = canvasRef.current;
    const viewer = new Viewer(account);
    const renderer = new VideoRenderer();
    if (canvas) renderer.attach(canvas);
    let disposed = false;

    const setCard = (next: CardState, text = "") => {
      if (disposed) return;
      setState(next);
      setError(text);
    };

    registerSink(device.id, (message) => viewer.sendJson(message));

    const off = viewer.subscribe({
      onOpen: () => {
        viewer.sendJson({
          t: "hello",
          proto: 1,
          token: "",
          client: { name: "网页端", version: WEB_VERSION },
          caps: [],
        });
      },
      onMessage: (message) => {
        switch (message.t) {
          case "welcome":
            // 群控吃带宽，缩略流足够看清操作结果；具体画面仍可点卡片进单台控制。
            viewer.sendJson({
              t: "video.sub",
              stream: "focus",
              display: 0,
              fps: 2,
              maxWidth: 640,
              quality: 55,
              codec: "mjpeg",
              cursor: true,
            });
            break;
          case "video.sub.ok":
            setCard("live");
            break;
          case "error":
            setCard("error", String(message.msg ?? "被控端拒绝了画面"));
            break;
          default:
            break;
        }
      },
      onFrame: (frame) => {
        if (frame.type === MEDIA_VIDEO) renderer.push(frame);
      },
      onClosed: (_code, msg) => {
        registerSink(device.id, null);
        setCard("error", msg);
      },
      onSocketClosed: (reason) => {
        registerSink(device.id, null);
        setCard("error", reason);
      },
    });

    const onWheel = (event: WheelEvent) => {
      if (stateRef.current !== "live") return;
      event.preventDefault();
      push({
        k: "wheel",
        delta: [wheelNotches(event.deltaX), -wheelNotches(event.deltaY)],
      });
    };
    canvas?.addEventListener("wheel", onWheel, { passive: false });

    const run = async () => {
      try {
        await viewer.connect();
        await viewer.open(device.id, "control");
        setCard("waiting");
      } catch (thrown) {
        setCard("error", describeThrown(thrown));
      }
    };
    void run();

    return () => {
      disposed = true;
      off();
      registerSink(device.id, null);
      canvas?.removeEventListener("wheel", onWheel);
      viewer.closeSession();
      viewer.close();
      renderer.detach();
      window.clearTimeout(gesture.current.timer);
    };
  }, [account, device.id, push, registerSink]);

  function toAbs(event: React.PointerEvent<HTMLCanvasElement>): [number, number] | null {
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

  function midY(): number {
    const values = Array.from(pointers.current.values());
    if (values.length === 0) return 0;
    return values.reduce((sum, point) => sum + point.y, 0) / values.length;
  }

  function onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
    if (stateRef.current !== "live") return;
    event.preventDefault();
    onActivate();
    event.currentTarget.setPointerCapture(event.pointerId);
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    const g = gesture.current;
    const abs = toAbs(event);

    if (event.pointerType === "mouse") {
      g.button = event.button === 2 ? "right" : event.button === 1 ? "middle" : "left";
      push({ k: "button", button: g.button, down: true, abs: abs ?? undefined }, true);
      return;
    }

    if (pointers.current.size >= 2) {
      // 第二根手指：结束单指拖动，切换成滚动。
      window.clearTimeout(g.timer);
      g.timer = 0;
      if (g.dragging) {
        push({ k: "button", button: "left", down: false }, true);
        g.dragging = false;
      }
      g.twoFinger = true;
      g.scrollAcc = 0;
      g.lastMidY = midY();
      return;
    }

    g.moved = false;
    g.dragging = false;
    g.longPressed = false;
    g.twoFinger = false;
    g.scrollAcc = 0;
    g.startX = event.clientX;
    g.startY = event.clientY;
    g.startAbs = abs;
    window.clearTimeout(g.timer);
    g.timer = window.setTimeout(() => {
      g.timer = 0;
      if (!g.moved && pointers.current.size === 1) g.longPressed = true;
    }, 480);
  }

  function onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
    if (stateRef.current !== "live") return;
    if (!pointers.current.has(event.pointerId)) return;
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    const g = gesture.current;
    const abs = toAbs(event);

    if (event.pointerType === "mouse") {
      push({ k: "move", abs: abs ?? undefined });
      return;
    }

    if (g.twoFinger || pointers.current.size >= 2) {
      const mid = midY();
      const dy = mid - g.lastMidY;
      g.lastMidY = mid;
      g.scrollAcc += dy;
      const step = 30;
      if (Math.abs(g.scrollAcc) >= step) {
        const direction = g.scrollAcc > 0 ? 1 : -1;
        g.scrollAcc -= direction * step;
        push({ k: "wheel", delta: [0, -direction * 120] });
      }
      return;
    }

    if (!g.dragging) {
      // 8px 以内算轻点，先不按下，避免长按右键时留下按住的左键。
      if (Math.hypot(event.clientX - g.startX, event.clientY - g.startY) < 8) return;
      g.moved = true;
      window.clearTimeout(g.timer);
      g.timer = 0;
      g.dragging = true;
      push(
        {
          k: "button",
          button: "left",
          down: true,
          abs: g.startAbs ?? abs ?? undefined,
        },
        true,
      );
    }
    push({ k: "move", abs: abs ?? undefined });
  }

  function onPointerUp(event: React.PointerEvent<HTMLCanvasElement>) {
    pointers.current.delete(event.pointerId);
    const g = gesture.current;
    window.clearTimeout(g.timer);
    g.timer = 0;
    if (stateRef.current !== "live") return;
    const abs = toAbs(event);

    if (event.pointerType === "mouse") {
      const button = g.button;
      g.button = "left";
      push({ k: "button", button, down: false, abs: abs ?? undefined }, true);
      return;
    }

    if (g.twoFinger) {
      if (pointers.current.size === 0) {
        g.twoFinger = false;
        g.scrollAcc = 0;
      }
      return;
    }

    if (g.dragging) {
      g.dragging = false;
      push({ k: "button", button: "left", down: false, abs: abs ?? undefined }, true);
      return;
    }

    if (g.longPressed) {
      g.longPressed = false;
      const point = g.startAbs ?? abs ?? undefined;
      push({ k: "button", button: "right", down: true, abs: point });
      push({ k: "button", button: "right", down: false, abs: point }, true);
      return;
    }

    const point = g.startAbs ?? abs ?? undefined;
    push({ k: "button", button: "left", down: true, abs: point });
    push({ k: "button", button: "left", down: false, abs: point }, true);
  }

  function onPointerCancel(event: React.PointerEvent<HTMLCanvasElement>) {
    pointers.current.delete(event.pointerId);
    const g = gesture.current;
    window.clearTimeout(g.timer);
    g.timer = 0;
    if (g.dragging) {
      g.dragging = false;
      push({ k: "button", button: "left", down: false }, true);
    }
    g.twoFinger = false;
    g.longPressed = false;
    g.moved = false;
  }

  return (
    <div className={`group-card ${active ? "is-active" : ""} is-${state}`}>
      <div className="group-thumb">
        <canvas
          ref={canvasRef}
          className="group-canvas"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          onContextMenu={(event) => event.preventDefault()}
        />
        {state !== "live" && (
          <div className="group-placeholder">
            {state === "connecting" || state === "waiting" ? (
              <>
                <Loader2 size={16} className="spin" />
                <span>{STATE_TEXT[state]}</span>
              </>
            ) : (
              <>
                <WifiOff size={16} />
                <span>{error || "这台设备没连上"}</span>
              </>
            )}
          </div>
        )}
        {active && <span className="group-badge">操作中</span>}
      </div>
      <div className="group-copy">
        <strong>{device.name || "未命名设备"}</strong>
        <small>{state === "live" ? STATE_TEXT.live : state === "error" ? "未同步" : STATE_TEXT[state]}</small>
      </div>
    </div>
  );
}

export default function GroupPage({
  account,
  devices,
  onBack,
}: {
  account: Account;
  devices: DeviceView[];
  onBack: () => void;
}) {
  const usable = useMemo(
    () => devices.filter((device) => device.online && device.controllable),
    [devices],
  );

  const [phase, setPhase] = useState<"select" | "running">("select");
  const [selected, setSelected] = useState<string[]>(() => usable.map((item) => item.id));
  const [activeId, setActiveId] = useState("");
  const [states, setStates] = useState<Record<string, { state: CardState; error: string }>>({});
  const [keyboardOpen, setKeyboardOpen] = useState(false);
  const keyboardRef = useRef<HTMLTextAreaElement | null>(null);

  const sinks = useRef(new Map<string, Sink>());
  const batcherRef = useRef<InputBatcher | null>(null);
  if (!batcherRef.current) {
    batcherRef.current = new InputBatcher((events) => {
      for (const sink of sinks.current.values()) sink({ t: "input", events });
    });
  }

  const push = useCallback((event: RemoteInput, immediate = false) => {
    batcherRef.current?.push(event);
    if (immediate) batcherRef.current?.flush();
  }, []);

  const registerSink = useCallback((id: string, sink: Sink | null) => {
    if (sink) sinks.current.set(id, sink);
    else sinks.current.delete(id);
  }, []);

  const onState = useCallback((id: string, state: CardState, error: string) => {
    setStates((current) => {
      const prev = current[id];
      if (prev && prev.state === state && prev.error === error) return current;
      return { ...current, [id]: { state, error } };
    });
  }, []);

  const chosen = useMemo(
    () => usable.filter((device) => selected.includes(device.id)),
    [usable, selected],
  );

  const liveCount = useMemo(
    () => chosen.filter((device) => states[device.id]?.state === "live").length,
    [chosen, states],
  );

  // 设备列表刷新后，把已离线的设备从选择里去掉。
  useEffect(() => {
    const available = new Set(usable.map((device) => device.id));
    setSelected((current) => {
      const next = current.filter((id) => available.has(id));
      return next.length === current.length ? current : next;
    });
  }, [usable]);

  function toggle(id: string) {
    setSelected((current) =>
      current.includes(id) ? current.filter((item) => item !== id) : [...current, id],
    );
  }

  function start() {
    if (chosen.length === 0) return;
    setStates({});
    setActiveId(chosen[0].id);
    setKeyboardOpen(false);
    setPhase("running");
  }

  function stop() {
    batcherRef.current?.flush();
    for (const sink of sinks.current.values()) {
      sink({ t: "input", events: [{ k: "releaseAll" }] });
    }
    setKeyboardOpen(false);
    setStates({});
    setPhase("select");
  }

  function back() {
    if (phase === "running") stop();
    onBack();
  }

  useEffect(() => {
    if (!keyboardOpen) return;
    const timer = window.setTimeout(() => keyboardRef.current?.focus(), 40);
    return () => window.clearTimeout(timer);
  }, [keyboardOpen]);

  function onBeforeInput(event: React.FormEvent<HTMLTextAreaElement>) {
    const native = event.nativeEvent as InputEvent;
    if (native.inputType === "insertText" && native.data) {
      event.preventDefault();
      push({ k: "text", text: native.data }, true);
    } else if (native.inputType === "deleteContentBackward") {
      event.preventDefault();
      push({ k: "key", scan: 0x0e, down: true });
      push({ k: "key", scan: 0x0e, down: false }, true);
    } else if (native.inputType === "insertLineBreak") {
      event.preventDefault();
      push({ k: "key", scan: 0x1c, down: true });
      push({ k: "key", scan: 0x1c, down: false }, true);
    }
  }

  function onCompositionEnd(event: React.CompositionEvent<HTMLTextAreaElement>) {
    if (event.data) push({ k: "text", text: event.data }, true);
  }

  if (phase === "select") {
    return (
      <div className="page group-page">
        <header className="app-bar">
          <button type="button" className="icon-btn" onClick={back} aria-label="返回">
            <ChevronLeft size={20} />
          </button>
          <div>
            <h1>群控</h1>
            <p className="app-bar-sub">一次操作，多台同步</p>
          </div>
        </header>

        {usable.length === 0 ? (
          <div className="empty">
            <Monitor size={28} />
            <strong>没有可群控的设备</strong>
            <span>被控电脑在线、且开启了「允许被控」才能加入群控。</span>
          </div>
        ) : (
          <>
            <div className="list-head">
              <span>选择设备</span>
              <span className="group-pick-actions">
                <button
                  type="button"
                  className="ghost-btn"
                  onClick={() => setSelected(usable.map((item) => item.id))}
                >
                  全选
                </button>
                <button type="button" className="ghost-btn" onClick={() => setSelected([])}>
                  清空
                </button>
              </span>
            </div>
            <div className="group-pick">
              {usable.map((device) => {
                const on = selected.includes(device.id);
                return (
                  <button
                    type="button"
                    key={device.id}
                    className={`group-pick-row ${on ? "is-on" : ""}`}
                    onClick={() => toggle(device.id)}
                    aria-pressed={on}
                  >
                    <span className="pick-box">{on && <Check size={14} />}</span>
                    <span className="group-pick-copy">
                      <strong>{device.name || "未命名设备"}</strong>
                      <small>{device.os || "在线"}</small>
                    </span>
                  </button>
                );
              })}
            </div>
            <div className="banner banner-tip">
              <div>
                <strong>授权提示</strong>
                <span>
                  每台设备按各自策略确认：已授权设备直接进入，未授权的会在被控端弹窗。
                </span>
              </div>
            </div>
            <button
              type="button"
              className="primary-btn group-start"
              onClick={start}
              disabled={chosen.length === 0}
            >
              <UsersRound size={17} />
              开始群控（{chosen.length} 台）
            </button>
          </>
        )}
        <footer className="page-foot">画面为低帧率缩略流，点任意一张可进入单台控制。</footer>
      </div>
    );
  }

  return (
    <div className="page group-page is-running">
      <header className="app-bar">
        <button type="button" className="icon-btn" onClick={back} aria-label="返回">
          <ChevronLeft size={20} />
        </button>
        <div>
          <h1>群控</h1>
          <p className="app-bar-sub">
            {liveCount} / {chosen.length} 台同步中
          </p>
        </div>
        <div className="app-bar-actions">
          <button
            type="button"
            className={`icon-btn ${keyboardOpen ? "is-on" : ""}`}
            onClick={() => setKeyboardOpen((value) => !value)}
            aria-label="键盘"
          >
            <Keyboard size={18} />
          </button>
        </div>
      </header>

      <div className="group-hint">
        <UsersRound size={14} />
        在任意画面上触摸 / 拖动 / 打字，所有设备同步执行
      </div>

      <div className="group-grid">
        {chosen.map((device) => (
          <GroupCard
            key={device.id}
            account={account}
            device={device}
            active={activeId === device.id}
            onActivate={() => setActiveId(device.id)}
            onState={onState}
            registerSink={registerSink}
            push={push}
          />
        ))}
      </div>

      <div className="group-dock">
        <button type="button" className="chip is-danger" onClick={stop}>
          <Square size={13} />
          停止群控
        </button>
      </div>

      {keyboardOpen && (
        <div className="keyboard-dock group-keyboard">
          <textarea
            ref={keyboardRef}
            className="keyboard-input"
            rows={1}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            placeholder="在这里打字，内容同步到全部设备"
            onBeforeInput={onBeforeInput}
            onCompositionEnd={onCompositionEnd}
          />
          <button type="button" className="ghost-btn" onClick={() => setKeyboardOpen(false)}>
            收起
          </button>
        </div>
      )}
    </div>
  );
}
