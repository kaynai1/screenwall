import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  Check,
  ChevronDown,
  Gauge,
  MonitorOff,
  MousePointer2,
  Pause,
  Play,
  Trash2,
  Users,
  WifiOff,
  X,
} from "lucide-react";
import {
  groupInput,
  isTauri,
  setGroupOptions,
  startGroup,
  stopGroup,
  subscribeGroupFrame,
  subscribeGroupState,
  type GroupQuality,
  type GroupStateEvent,
  type InputEvent,
} from "../api";
import DesktopPreview from "../components/DesktopPreview";
import { EmptyState, Segmented, Switch } from "../components/ui";
import { errorLine } from "../errors";
import {
  LOCAL_ONLY_KEYS,
  MOUSE_BUTTONS,
  SCAN_CODES,
  pointToNormalized,
  wheelNotches,
} from "../input";
import { formatLastSeen } from "../mock";
import type { DeviceView, GroupPageProps } from "../types";
import "./GroupPage.css";

type Quality = "smooth" | "balanced" | "hd";

const QUALITY_LABEL: Record<Quality, string> = {
  smooth: "流畅",
  balanced: "均衡",
  hd: "高清",
};

/** 群控画面都是低帧率缩略流，键鼠输入另走同一会话。 */
const QUALITY_OPTIONS: Record<Quality, GroupQuality> = {
  smooth: { fps: 1, maxWidth: 480, quality: 50 },
  balanced: { fps: 2, maxWidth: 640, quality: 60 },
  hd: { fps: 3, maxWidth: 960, quality: 70 },
};

const COLUMN_OPTIONS: { value: "2" | "3"; label: string }[] = [
  { value: "2", label: "2 列" },
  { value: "3", label: "3 列" },
];

const MEMBER_STATE_LABEL: Record<GroupStateEvent["state"], string> = {
  connecting: "等待授权",
  live: "已连接",
  closed: "已断开",
  error: "连接失败",
};

function latencyText(device: DeviceView): string {
  if (device.latency === undefined) return "在线";
  return `${device.latency} ms`;
}

function removeKey<T>(record: Record<string, T>, key: string): Record<string, T> {
  if (!(key in record)) return record;
  const next = { ...record };
  delete next[key];
  return next;
}

/**
 * 群控输入层：盖在一张实时画面上，鼠标/键盘事件按归一化坐标广播出去。
 * 每台设备各有一个，焦点在哪台就以哪台为「主控画面」。
 */
function GroupInputOverlay({
  deviceId,
  width,
  height,
  sendRemote,
}: {
  deviceId: string;
  width: number;
  height: number;
  sendRemote: (deviceId: string, events: InputEvent[]) => void;
}) {
  const layerRef = useRef<HTMLDivElement | null>(null);
  const pressedKeysRef = useRef<Set<number>>(new Set());
  const pressedButtonsRef = useRef<Set<number>>(new Set());
  const pendingMoveRef = useRef<[number, number] | null>(null);
  const moveRafRef = useRef<number | null>(null);
  const lastMoveSentAtRef = useRef(0);
  const sendRef = useRef(sendRemote);
  sendRef.current = sendRemote;

  const send = useCallback(
    (events: InputEvent[]) => {
      if (events.length > 0) sendRef.current(deviceId, events);
    },
    [deviceId],
  );

  const cancelPendingMove = useCallback(() => {
    pendingMoveRef.current = null;
    if (moveRafRef.current !== null) {
      window.cancelAnimationFrame(moveRafRef.current);
      moveRafRef.current = null;
    }
  }, []);

  const releaseAll = useCallback(() => {
    pressedKeysRef.current.clear();
    pressedButtonsRef.current.clear();
    cancelPendingMove();
    send([{ k: "releaseAll" }]);
  }, [cancelPendingMove, send]);

  const flushMove = useCallback(() => {
    moveRafRef.current = null;
    const abs = pendingMoveRef.current;
    pendingMoveRef.current = null;
    if (!abs) return;
    // 高刷屏上 rAF 可能超过 60Hz，这里再按 16ms 节流。
    const now = performance.now();
    if (now - lastMoveSentAtRef.current < 16) {
      pendingMoveRef.current = abs;
      moveRafRef.current = window.requestAnimationFrame(flushMove);
      return;
    }
    lastMoveSentAtRef.current = now;
    send([{ k: "move", abs }]);
  }, [send]);

  const pointToAbs = useCallback(
    (
      clientX: number,
      clientY: number,
      clampToImage = true,
    ): [number, number] | null => {
      const layer = layerRef.current;
      if (!layer) return null;
      const rect = layer.getBoundingClientRect();
      return pointToNormalized(
        rect,
        clientX - rect.left,
        clientY - rect.top,
        width,
        height,
        "contain",
        clampToImage,
      );
    },
    [height, width],
  );

  function queueMove(clientX: number, clientY: number) {
    const abs = pointToAbs(clientX, clientY);
    if (!abs) return;
    pendingMoveRef.current = abs;
    if (moveRafRef.current === null) {
      moveRafRef.current = window.requestAnimationFrame(flushMove);
    }
  }

  useEffect(() => {
    layerRef.current?.focus({ preventScroll: true });
    return () => {
      releaseAll();
    };
  }, [releaseAll]);

  useEffect(() => {
    const onBlur = () => releaseAll();
    const onVisibility = () => {
      if (document.hidden) releaseAll();
    };
    window.addEventListener("blur", onBlur);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.removeEventListener("blur", onBlur);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [releaseAll]);

  // 画面外松开鼠标也要补发 up，避免远端按钮卡住。
  useEffect(() => {
    const onMouseUp = (event: MouseEvent) => {
      if (!pressedButtonsRef.current.delete(event.button)) return;
      const button = MOUSE_BUTTONS[event.button];
      if (!button) return;
      const abs = pointToAbs(event.clientX, event.clientY, false);
      cancelPendingMove();
      send([{ k: "button", button, down: false, ...(abs ? { abs } : {}) }]);
    };
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, [cancelPendingMove, pointToAbs, send]);

  // 滚轮要 non-passive 才能阻止页面滚动。
  useEffect(() => {
    const layer = layerRef.current;
    if (!layer) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      event.stopPropagation();
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 100 : 1;
      const delta: [number, number] = [
        wheelNotches(event.deltaX * unit),
        -wheelNotches(event.deltaY * unit),
      ];
      if (delta[0] === 0 && delta[1] === 0) return;
      cancelPendingMove();
      send([{ k: "wheel", delta }]);
    };
    layer.addEventListener("wheel", onWheel, { passive: false });
    return () => layer.removeEventListener("wheel", onWheel);
  }, [cancelPendingMove, send]);

  function handleMouseDown(event: ReactMouseEvent<HTMLDivElement>) {
    event.preventDefault();
    layerRef.current?.focus({ preventScroll: true });
    const button = MOUSE_BUTTONS[event.button];
    if (!button) return;
    const abs = pointToAbs(event.clientX, event.clientY);
    cancelPendingMove();
    pressedButtonsRef.current.add(event.button);
    send([{ k: "button", button, down: true, ...(abs ? { abs } : {}) }]);
  }

  function handleMouseMove(event: ReactMouseEvent<HTMLDivElement>) {
    queueMove(event.clientX, event.clientY);
  }

  function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (LOCAL_ONLY_KEYS.has(event.code)) return;
    const mapping = SCAN_CODES[event.code];
    if (!mapping) return;
    event.preventDefault();
    event.stopPropagation();
    pressedKeysRef.current.add(mapping.scan);
    send([{ k: "key", scan: mapping.scan, down: true, ext: mapping.ext }]);
  }

  function handleKeyUp(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (LOCAL_ONLY_KEYS.has(event.code)) return;
    const mapping = SCAN_CODES[event.code];
    if (!mapping) return;
    event.preventDefault();
    event.stopPropagation();
    pressedKeysRef.current.delete(mapping.scan);
    send([{ k: "key", scan: mapping.scan, down: false, ext: mapping.ext }]);
  }

  return (
    <div
      ref={layerRef}
      className="group-input-layer"
      tabIndex={0}
      role="application"
      aria-label={`操作 ${deviceId} 的画面`}
      onMouseDown={handleMouseDown}
      onMouseMove={handleMouseMove}
      onKeyDown={handleKeyDown}
      onKeyUp={handleKeyUp}
    />
  );
}

export default function GroupPage({
  devices,
  selectedIds,
  setSelectedIds,
  onOpenDevice,
  notify,
}: GroupPageProps) {
  const [running, setRunning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [columns, setColumns] = useState<"2" | "3">("2");
  const [quality, setQuality] = useState<Quality>("balanced");
  const [syncKeyboard, setSyncKeyboard] = useState(true);
  const [broadcastInput, setBroadcastInput] = useState(true);
  const [memberStates, setMemberStates] = useState<
    Record<string, GroupStateEvent>
  >({});
  const [frames, setFrames] = useState<Record<string, string>>({});

  const tokenRef = useRef(0);
  const broadcastRef = useRef(broadcastInput);
  const qualityRef = useRef(quality);
  const selectionRef = useRef<string | null>(null);
  broadcastRef.current = broadcastInput;
  qualityRef.current = quality;

  const onlineDevices = devices.filter((device) => device.online);
  const selectedDevices = devices.filter((device) =>
    selectedIds.includes(device.id),
  );
  const onlineSelected = selectedDevices.filter((device) => device.online);
  const onlineSelectedKey = onlineSelected.map((device) => device.id).join(",");

  // 后端的群控状态与画面帧；只接受当前批次，避免旧页面的残留事件。
  useEffect(() => {
    if (!isTauri) return;
    const offState = subscribeGroupState((event) => {
      if (event.token !== tokenRef.current) return;
      setMemberStates((prev) => ({ ...prev, [event.deviceId]: event }));
      if (event.state !== "live") {
        setFrames((prev) => removeKey(prev, event.deviceId));
      }
    });
    const offFrame = subscribeGroupFrame((event) => {
      if (event.token !== tokenRef.current) return;
      setFrames((prev) =>
        prev[event.deviceId] === event.dataUrl
          ? prev
          : { ...prev, [event.deviceId]: event.dataUrl },
      );
    });
    return () => {
      offState();
      offFrame();
    };
  }, []);

  // 离开页面即停止群控，被控端会话一并关闭。
  useEffect(
    () => () => {
      const current = tokenRef.current;
      tokenRef.current = 0;
      if (current > 0) void stopGroup(current).catch(() => {});
    },
    [],
  );

  const sendRemote = useCallback((deviceId: string, events: InputEvent[]) => {
    const current = tokenRef.current;
    if (current <= 0) return;
    void groupInput(
      current,
      events,
      broadcastRef.current ? null : deviceId,
    );
  }, []);

  function resetRunState() {
    tokenRef.current = 0;
    selectionRef.current = null;
    setRunning(false);
    setMemberStates({});
    setFrames({});
  }

  async function handleStart() {
    if (onlineSelected.length === 0) {
      notify("请先选择至少一台在线设备", "error");
      return;
    }
    if (!isTauri) {
      notify("浏览器演示模式无法建立群控会话", "error");
      return;
    }
    setBusy(true);
    try {
      const options = QUALITY_OPTIONS[qualityRef.current];
      const next = await startGroup(
        onlineSelected.map((device) => device.id),
        options,
      );
      if (next <= 0) {
        notify("没有可群控的在线设备", "error");
        return;
      }
      tokenRef.current = next;
      selectionRef.current = onlineSelectedKey;
      setRunning(true);
      // 后端在 open 请求发出前就推送了「等待授权」，那时 token 还没回到前端，
      // 这里补一份初始状态，避免画面先显示「暂无预览」。
      setMemberStates(
        Object.fromEntries(
          onlineSelected.map((device) => [
            device.id,
            {
              token: next,
              deviceId: device.id,
              state: "connecting" as const,
              message: "等待被控端授权…",
              width: 0,
              height: 0,
            },
          ]),
        ),
      );
      setFrames({});
      notify(
        `已向 ${onlineSelected.length} 台设备发起群控，请在被控端确认授权`,
        "info",
      );
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setBusy(false);
    }
  }

  async function handleStop(silent = false) {
    const current = tokenRef.current;
    resetRunState();
    if (current > 0) {
      try {
        await stopGroup(current);
      } catch {
        // 会话已经断开时关闭命令失败可以忽略。
      }
    }
    if (!silent) notify("已停止群控广播", "info");
  }

  // 运行中增删设备：让后端多退少补，新建的会话会重新弹授权。
  useEffect(() => {
    if (!running) return;
    if (selectionRef.current === onlineSelectedKey) return;
    selectionRef.current = onlineSelectedKey;
    const ids = onlineSelectedKey ? onlineSelectedKey.split(",") : [];
    if (ids.length === 0) {
      void handleStop(true);
      return;
    }
    let cancelled = false;
    void startGroup(ids, QUALITY_OPTIONS[qualityRef.current])
      .then((next) => {
        if (cancelled || next <= 0) return;
        tokenRef.current = next;
      })
      .catch((error) => {
        if (!cancelled) notify(errorLine(error), "error");
      });
    return () => {
      cancelled = true;
    };
    // handleStop 只依赖 ref 与状态，不需要进入依赖数组。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onlineSelectedKey, running, notify]);

  function toggleSelect(id: string) {
    const target = devices.find((device) => device.id === id);
    if (!target || !target.online) return;
    setSelectedIds(
      selectedIds.includes(id)
        ? selectedIds.filter((item) => item !== id)
        : [...selectedIds, id],
    );
  }

  function selectAllOnline() {
    setSelectedIds(onlineDevices.map((device) => device.id));
    if (onlineDevices.length === 0) {
      notify("当前没有在线设备", "info");
      return;
    }
    notify(`已选择 ${onlineDevices.length} 台在线设备`, "success");
  }

  function clearSelection() {
    if (selectedIds.length === 0) {
      notify("当前没有选中的设备", "info");
      return;
    }
    setSelectedIds([]);
    notify("已清空群控选择", "info");
  }

  function toggleRunning() {
    if (running) {
      void handleStop();
      return;
    }
    void handleStart();
  }

  function handleQuality(next: Quality) {
    setQuality(next);
    notify(`画面画质已切换为「${QUALITY_LABEL[next]}」`, "info");
    if (running && tokenRef.current > 0) {
      void setGroupOptions(tokenRef.current, QUALITY_OPTIONS[next]).catch(
        (error) => notify(errorLine(error), "error"),
      );
    }
  }

  function removeSelection(id: string, name: string) {
    setSelectedIds(selectedIds.filter((item) => item !== id));
    notify(`已将「${name}」移出群控`, "info");
  }

  const liveCount = onlineSelected.filter(
    (device) => memberStates[device.id]?.state === "live",
  ).length;
  const failedCount = onlineSelected.filter(
    (device) =>
      memberStates[device.id]?.state === "error" ||
      memberStates[device.id]?.state === "closed",
  ).length;

  const statusText =
    onlineSelected.length === 0
      ? "等待选择要群控的设备"
      : running
        ? `广播中 · ${liveCount}/${onlineSelected.length} 台画面就绪${
            failedCount > 0 ? `，${failedCount} 台异常` : ""
          }`
        : `广播已停止 · ${onlineSelected.length} 台设备待命`;

  return (
    <div className="group-page">
      <header className="group-toolbar">
        <div className="group-toolbar-group">
          <label className="group-quality">
            <Gauge size={14} strokeWidth={1.75} aria-hidden="true" />
            <span className="group-quality-label">画质</span>
            <span className="group-quality-select">
              <select
                className="group-select"
                value={quality}
                onChange={(event) =>
                  handleQuality(event.target.value as Quality)
                }
                aria-label="群控画质"
              >
                <option value="smooth">流畅</option>
                <option value="balanced">均衡</option>
                <option value="hd">高清</option>
              </select>
              <ChevronDown size={14} strokeWidth={1.75} aria-hidden="true" />
            </span>
          </label>

          <span className="group-toolbar-divider" aria-hidden="true" />

          <span className="group-toolbar-label">画面排列</span>
          <Segmented
            value={columns}
            options={COLUMN_OPTIONS}
            onChange={setColumns}
            size="sm"
          />
        </div>

        <div className="group-toolbar-group">
          <span
            className={`group-state ${running ? "is-live" : ""}`}
            role="status"
          >
            <i aria-hidden="true" />
            {running ? "广播中" : "已停止"}
          </span>
        </div>
      </header>

      <div className="group-body">
        <aside className="group-picker" aria-label="设备选择">
          <div className="group-picker-head">
            <div className="group-picker-title">
              <h2>设备</h2>
              <span>
                {onlineDevices.length} 在线 / {devices.length} 台
              </span>
            </div>
            <div className="group-picker-tools">
              <button
                type="button"
                className="chip group-picker-chip"
                onClick={selectAllOnline}
              >
                <Users size={14} strokeWidth={1.75} />
                全选在线
              </button>
              <button
                type="button"
                className="chip group-picker-chip"
                onClick={clearSelection}
              >
                清空
              </button>
            </div>
          </div>

          <div className="group-list" role="group" aria-label="可群控设备">
            {devices.map((device) => {
              const selected = selectedIds.includes(device.id);
              const disabled = !device.online;
              return (
                <button
                  key={device.id}
                  type="button"
                  className={`group-row ${selected ? "is-selected" : ""}`}
                  onClick={() => toggleSelect(device.id)}
                  disabled={disabled}
                  aria-pressed={selected}
                  aria-label={`${
                    selected ? "移出群控" : "加入群控"
                  } ${device.name}`}
                  title={disabled ? "设备离线，无法加入群控" : device.name}
                >
                  <span className="group-check" aria-hidden="true">
                    {selected && <Check size={14} strokeWidth={1.75} />}
                  </span>

                  <span className="group-row-copy">
                    <span className="group-row-name">
                      <i
                        className={`group-dot ${
                          device.online ? "is-on" : "is-off"
                        }`}
                        aria-hidden="true"
                      />
                      {device.name}
                    </span>
                    <span className="group-row-meta">
                      {device.online
                        ? device.os
                        : `离线 · ${formatLastSeen(device.lastSeen)}`}
                    </span>
                  </span>

                  <span className="group-row-latency">
                    {device.online ? (
                      latencyText(device)
                    ) : (
                      <WifiOff size={14} strokeWidth={1.75} aria-hidden="true" />
                    )}
                  </span>
                </button>
              );
            })}
          </div>

          <div className="group-picker-foot">
            点击设备加入或移出群控，离线设备不可选择
          </div>
        </aside>

        <section className="group-console" aria-label="群控同步台">
          <header className="group-console-head">
            <div>
              <h2>同步台</h2>
              <p>
                已选 <strong>{selectedDevices.length}</strong> 台 · 在线{" "}
                <strong>{onlineSelected.length}</strong> 台
              </p>
            </div>
            <span className={`badge ${running ? "" : "badge-muted"}`}>
              {running ? "实时广播" : "待命"}
            </span>
          </header>

          <section className="group-section group-section-previews">
            <div className="group-section-head">
              <h3>已选画面</h3>
              <span>{selectedDevices.length} 台</span>
            </div>

            {selectedDevices.length === 0 ? (
              <div className="group-preview-empty">
                <EmptyState
                  icon={<MonitorOff size={20} strokeWidth={1.75} />}
                  title="还没有选择设备"
                  hint="在左侧勾选在线设备，或用「全选在线」把全部电脑放进同步台。"
                  action={
                    <button
                      type="button"
                      className="btn btn-soft"
                      onClick={selectAllOnline}
                    >
                      <Users size={15} strokeWidth={1.75} />
                      全选在线设备
                    </button>
                  }
                />
              </div>
            ) : (
              <div className="group-preview-row" data-cols={columns}>
                {selectedDevices.map((device) => {
                  const member = memberStates[device.id];
                  const frame = device.online ? frames[device.id] : undefined;
                  const canInject =
                    running &&
                    syncKeyboard &&
                    member?.state === "live" &&
                    frame &&
                    member.width > 0 &&
                    member.height > 0;
                  return (
                    <article
                      className={`group-preview ${
                        device.online ? "" : "is-offline"
                      }`}
                      key={device.id}
                    >
                      <div className="group-preview-stage">
                        <DesktopPreview
                          device={device}
                          live={member?.state === "live"}
                          frame={frame}
                          hint={
                            member && member.state !== "live"
                              ? (member.message ?? undefined)
                              : undefined
                          }
                        />
                        {canInject && (
                          <GroupInputOverlay
                            deviceId={device.id}
                            width={member.width}
                            height={member.height}
                            sendRemote={sendRemote}
                          />
                        )}
                      </div>
                      <div className="group-preview-foot">
                        <div className="group-preview-copy">
                          <strong title={device.name}>{device.name}</strong>
                          <small>
                            {device.online
                              ? member
                                ? `${MEMBER_STATE_LABEL[member.state]}${
                                    member.state === "live"
                                      ? ` · ${member.width}×${member.height}`
                                      : member.message
                                        ? ` · ${member.message}`
                                        : ""
                                  }`
                                : `${device.os}${
                                    device.latency !== undefined
                                      ? ` · ${device.latency} ms`
                                      : ""
                                  }`
                              : `离线 · ${formatLastSeen(device.lastSeen)}`}
                          </small>
                        </div>
                        <div className="group-preview-actions">
                          <button
                            type="button"
                            className="icon-btn group-preview-btn"
                            disabled={!device.online}
                            onClick={() => onOpenDevice(device.id)}
                            title="单独控制"
                            aria-label={`单独控制 ${device.name}`}
                          >
                            <MousePointer2 size={14} strokeWidth={1.75} />
                          </button>
                          <button
                            type="button"
                            className="icon-btn group-preview-btn is-danger"
                            onClick={() =>
                              removeSelection(device.id, device.name)
                            }
                            title="移出群控"
                            aria-label={`将 ${device.name} 移出群控`}
                          >
                            <X size={14} strokeWidth={1.75} />
                          </button>
                        </div>
                      </div>
                    </article>
                  );
                })}
              </div>
            )}
          </section>

          <div className="group-console-body">
            <section className="group-section">
              <div className="group-section-head">
                <h3>同步选项</h3>
                <span>对已选的在线设备生效</span>
              </div>

              <div className="group-switch-list">
                <Switch
                  checked={syncKeyboard}
                  onChange={(next) => {
                    setSyncKeyboard(next);
                    notify(
                      next
                        ? "键鼠同步已开启：点击任意画面即可操作"
                        : "键鼠同步已关闭",
                      "info",
                    );
                  }}
                  label="键鼠同步"
                  hint="点击任意设备画面获得焦点，键鼠输入按比例落到各设备"
                />
                <Switch
                  checked={broadcastInput}
                  onChange={(next) => {
                    setBroadcastInput(next);
                    notify(
                      next
                        ? "广播输入已开启：一次操作同步到所有设备"
                        : "广播输入已关闭：只操作当前聚焦的设备",
                      "info",
                    );
                  }}
                  label="广播输入"
                  hint="关闭后只向当前有焦点的设备发送键鼠"
                />
              </div>
            </section>

            <section className="group-section">
              <div className="group-section-head">
                <h3>选择操作</h3>
                <span>按在线状态批量处理</span>
              </div>

              <div className="group-actions">
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={selectAllOnline}
                >
                  <Users size={14} strokeWidth={1.75} />
                  全选在线
                </button>
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={clearSelection}
                >
                  <Trash2 size={14} strokeWidth={1.75} />
                  清空选择
                </button>
              </div>

              <p className="group-actions-hint">
                离线设备不会加入群控。当前画质为「{QUALITY_LABEL[quality]}」，
                键鼠与画面会按此画质下发；运行中增减设备会自动建立或关闭对应会话。
              </p>
            </section>
          </div>

          <footer className="group-exec">
            <div className="group-exec-copy">
              <strong>{running ? "群控进行中" : "准备开始群控"}</strong>
              <span>{statusText}</span>
            </div>
            <button
              type="button"
              className={`btn ${running ? "btn-danger" : "btn-primary"}`}
              onClick={toggleRunning}
              disabled={busy || (!running && onlineSelected.length === 0)}
            >
              {running ? (
                <>
                  <Pause size={15} strokeWidth={1.75} />
                  停止群控
                </>
              ) : (
                <>
                  <Play size={15} strokeWidth={1.75} />
                  {busy ? "正在发起…" : "开始群控"}
                </>
              )}
            </button>
          </footer>
        </section>
      </div>
    </div>
  );
}
