import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import {
  AlertCircle,
  Check,
  CheckCircle2,
  Copy,
  FolderUp,
  Hash,
  KeyRound,
  Layers,
  LayoutGrid,
  Link2,
  List,
  Loader2,
  Monitor,
  MonitorPlay,
  MonitorSmartphone,
  Plus,
  RefreshCw,
  Search,
  SearchX,
  UploadCloud,
  X,
} from "lucide-react";
import { sha256 } from "js-sha256";
import DesktopPreview from "../components/DesktopPreview";
import { EmptyState, Modal, Progress, Segmented } from "../components/ui";
import {
  cancelFileTransfer,
  createConnectLink,
  getConnectCode,
  getControlInfo,
  isTauri,
  offerFiles,
  rotateConnectCode,
  setWallPreview,
  startControl,
  stopControl,
  stopWallPreview,
  subscribeFileTransfer,
  subscribeWallFrame,
  subscribeWallMetrics,
  subscribeWallState,
  uploadChunk,
  type ConnectCodeView,
  type ConnectLinkView,
  type FileTransferEvent,
} from "../api";
import { formatConnectCode } from "../connect";
import { errorLine } from "../errors";
import { formatBytes, formatLastSeen, statusLabel } from "../mock";
import type { DeviceView, WallPageProps } from "../types";
import "./WallPage.css";

type WallFilter = "all" | "online" | "offline";
type WallView = "grid" | "list";
type Tone = "ok" | "warn" | "danger";

/** 每个上传数据块的原始字节数（后端要求不超过 256 KiB）。 */
const CHUNK_BYTES = 256 * 1024;
/** 浏览器侧计算 SHA256 的切片大小。 */
const HASH_SLICE_BYTES = 4 * 1024 * 1024;
/** 从屏幕墙发起会话时用的令牌基数，和文件页 / 控制页的计数器错开。 */
let wallTokenSeq = 900_000;

type DropStatus =
  | "queued"
  | "connecting"
  | "hashing"
  | "sending"
  | "waiting"
  | "done"
  | "error"
  | "cancelled";

/** 屏幕墙上一台设备当前的发送状态；同一设备只保留最近一次。 */
type WallTransfer = {
  deviceId: string;
  status: DropStatus;
  /** 被控端分配的传输号；0 表示还没走到 `offer_files`。 */
  tid: number;
  files: number;
  total: number;
  done: number;
  completed: number;
  message: string | null;
};

/** 拖入文件后等待用户确认「建立控制会话并发送」。 */
type DropRequest = {
  device: DeviceView;
  files: File[];
  /** 当前已连着另一台设备时的设备 ID；仅用于提示。 */
  switchFrom: string | null;
};

/** 一次进行中的发送；`run` 由事件回调和发送循环共享，避免闭包读到旧状态。 */
type UploadRun = {
  tid: number;
  deviceId: string;
  deviceName: string;
  files: number;
  cancelled: boolean;
  settled: boolean;
  watchdog: number | null;
  /** 这条会话是屏幕墙自己建的（收尾后要断开）；复用控制页的会话为 false。 */
  ownsSession: boolean;
  /** 本次发送使用的会话令牌；没有会话时为 null。 */
  token: number | null;
};

const DROP_STATUS_LABEL: Record<DropStatus, string> = {
  queued: "排队等待上一台设备…",
  connecting: "正在连接设备…",
  hashing: "正在计算校验值…",
  sending: "发送中",
  waiting: "等待被控端确认…",
  done: "已发送",
  error: "发送失败",
  cancelled: "已取消",
};

/** 列表视图空间小，用更短的文案。 */
const DROP_STATUS_SHORT: Record<DropStatus, string> = {
  queued: "排队中",
  connecting: "连接中",
  hashing: "校验中",
  sending: "发送中",
  waiting: "确认中",
  done: "已发送",
  error: "失败",
  cancelled: "已取消",
};

function isActiveDrop(status: DropStatus): boolean {
  return (
    status === "queued" ||
    status === "connecting" ||
    status === "hashing" ||
    status === "sending" ||
    status === "waiting"
  );
}

/** 拖拽内容里是否带文件；拖选文字时不触发发送。 */
function hasFilePayload(event: DragEvent<HTMLElement>): boolean {
  const types = event.dataTransfer?.types;
  return types ? Array.from(types).includes("Files") : false;
}

/** 为什么这台设备不能接收文件；能接收时返回 null。 */
function dropBlockReason(device: DeviceView): string | null {
  if (!device.online) return "设备离线，无法发送";
  if (!device.controllable) return "未允许远程控制，无法发送";
  return null;
}

function droppedRelativePath(file: File): string {
  const withPath = file as File & { webkitRelativePath?: string };
  return withPath.webkitRelativePath || file.name;
}

/** 浏览器侧流式算 SHA256：WebView 拿不到本地绝对路径，只能读 File 内容。 */
async function hashLocalFile(file: File): Promise<string> {
  const hasher = sha256.create();
  for (let offset = 0; offset < file.size; offset += HASH_SLICE_BYTES) {
    const buffer = await file
      .slice(offset, offset + HASH_SLICE_BYTES)
      .arrayBuffer();
    hasher.update(new Uint8Array(buffer));
  }
  return hasher.hex();
}

/** 读取一个文件切片并转成 base64（不带 data URL 前缀）。 */
function readChunkBase64(
  file: File,
  offset: number,
  length: number,
): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = typeof reader.result === "string" ? reader.result : "";
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.onerror = () => {
      reject(reader.error ?? new Error("读取本地文件失败"));
    };
    reader.readAsDataURL(file.slice(offset, offset + length));
  });
}

function usageTone(value: number): Tone {
  if (value >= 80) return "danger";
  if (value >= 60) return "warn";
  return "ok";
}

function latencyTone(value: number): Tone {
  if (value >= 40) return "danger";
  if (value >= 20) return "warn";
  return "ok";
}

function statusClass(device: DeviceView): string {
  if (!device.online) return "is-offline";
  if (!device.controllable) return "is-view";
  return "is-online";
}

function latencyClass(device: DeviceView): string {
  if (!device.online || device.latency === undefined) return "is-muted";
  return `is-${latencyTone(device.latency)}`;
}

function latencyText(device: DeviceView): string {
  return device.online && device.latency !== undefined
    ? `${device.latency} ms`
    : "—";
}

function activateCard(event: KeyboardEvent<HTMLElement>, action: () => void) {
  if (event.target !== event.currentTarget) return;
  if (event.key !== "Enter" && event.key !== " ") return;
  event.preventDefault();
  action();
}

function Metric({ label, value }: { label: string; value: number | null }) {
  const tone = value === null ? "ok" : usageTone(value);

  return (
    <div className="wall-metric">
      <span className="wall-metric-label">{label}</span>
      <span
        className={`wall-bar is-${tone} ${value === null ? "is-empty" : ""}`}
        aria-hidden="true"
      >
        {value !== null && (
          <span style={{ width: `${Math.min(100, Math.max(0, value))}%` }} />
        )}
      </span>
      <span className="wall-metric-value">
        {value === null ? "—" : `${value}%`}
      </span>
    </div>
  );
}

/** 卡片底部的发送状态：进度条 + 百分比 + 取消；结束态显示成功/失败原因。 */
function TransferStatus({
  transfer,
  compact = false,
  onCancel,
}: {
  transfer: WallTransfer;
  compact?: boolean;
  onCancel: () => void;
}) {
  const active = isActiveDrop(transfer.status);
  const failed = transfer.status === "error";
  const percent =
    transfer.total > 0
      ? Math.min(100, Math.round((transfer.done / transfer.total) * 100))
      : 0;
  const tone = failed ? "red" : transfer.status === "done" ? "mint" : "blue";
  const label = compact
    ? DROP_STATUS_SHORT[transfer.status]
    : DROP_STATUS_LABEL[transfer.status];

  return (
    <div
      className={`wall-transfer is-${transfer.status}`}
      role="status"
      aria-live="polite"
    >
      <div className="wall-transfer-head">
        {active ? (
          <Loader2 size={13} strokeWidth={2} className="spin" aria-hidden="true" />
        ) : failed ? (
          <AlertCircle size={13} strokeWidth={2} aria-hidden="true" />
        ) : (
          <CheckCircle2 size={13} strokeWidth={2} aria-hidden="true" />
        )}
        <span className="wall-transfer-label" title={transfer.message ?? undefined}>
          {label}
        </span>
        {active && transfer.total > 0 && (
          <span className="wall-transfer-percent">{percent}%</span>
        )}
        {active && (
          <button
            type="button"
            className="wall-transfer-cancel"
            aria-label="取消发送"
            title="取消发送"
            onClick={(event) => {
              event.stopPropagation();
              onCancel();
            }}
          >
            <X size={12} strokeWidth={2} />
          </button>
        )}
      </div>

      {active && transfer.total > 0 && (
        <Progress value={percent} tone={tone} thin />
      )}

      {active && transfer.total > 0 && !compact && (
        <span className="wall-transfer-bytes">
          {formatBytes(Math.min(transfer.done, transfer.total))} /{" "}
          {formatBytes(transfer.total)} · {transfer.files} 个文件
        </span>
      )}

      {transfer.status === "done" && !compact && (
        <span className="wall-transfer-bytes">
          共 {transfer.files} 个文件 · {formatBytes(transfer.total)}
        </span>
      )}

      {failed && (
        <span className="wall-transfer-error" title={transfer.message ?? undefined}>
          {transfer.message ?? "未知原因"}
        </span>
      )}
    </div>
  );
}

function SkeletonGrid() {
  return (
    <div className="wall-grid" aria-hidden="true">
      {[0, 1, 2].map((key) => (
        <div className="wall-skeleton-card" key={key}>
          <span className="skeleton wall-skeleton-preview" />
          <span className="skeleton wall-skeleton-line is-title" />
          <span className="skeleton wall-skeleton-line" />
          <span className="skeleton wall-skeleton-line is-bar" />
          <span className="skeleton wall-skeleton-line is-bar" />
        </div>
      ))}
    </div>
  );
}

function SkeletonList() {
  return (
    <div className="wall-table" aria-hidden="true">
      <div className="wall-table-head">
        <span />
        <span>设备</span>
        <span>状态</span>
        <span className="wall-col-system">系统</span>
        <span className="wall-col-ip">IP</span>
        <span>延迟</span>
        <span className="wall-col-seen">最后在线</span>
        <span className="wall-col-action">操作</span>
      </div>
      {[0, 1, 2, 3, 4].map((key) => (
        <div className="wall-row wall-skeleton-row" key={key}>
          <span className="skeleton wall-skeleton-dot" />
          <span className="skeleton wall-skeleton-line is-wide" />
          <span className="skeleton wall-skeleton-line is-short" />
          <span className="skeleton wall-skeleton-line is-short wall-col-system" />
          <span className="skeleton wall-skeleton-line is-short wall-col-ip" />
          <span className="skeleton wall-skeleton-line is-tiny" />
          <span className="skeleton wall-skeleton-line is-short wall-col-seen" />
          <span className="skeleton wall-skeleton-line is-tiny wall-col-action" />
        </div>
      ))}
    </div>
  );
}

export default function WallPage(props: WallPageProps) {
  const {
    account,
    devices,
    notify,
    selectedIds,
    onToggleSelect,
    onClearSelection,
    onOpenDevice,
    onGroup,
    onFiles,
    onDevices,
  } = props;

  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<WallFilter>("all");
  const [view, setView] = useState<WallView>("grid");
  const [addOpen, setAddOpen] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [frames, setFrames] = useState<Record<string, string>>({});
  const [previewHints, setPreviewHints] = useState<Record<string, string>>({});
  // 被控端 status 推来的真实整机指标（CPU / 内存百分比）。
  const [metrics, setMetrics] = useState<
    Record<string, { cpu: number; mem: number }>
  >({});

  /* ------------------------------------------------- 本机连接码 / 连接链接 */

  // 被控端把本机连接码直接放在工作台顶部，不用再进设置里找。
  const [connect, setConnect] = useState<ConnectCodeView | null>(null);
  const [connectBusy, setConnectBusy] = useState(false);
  const [link, setLink] = useState<ConnectLinkView | null>(null);
  const [linkBusy, setLinkBusy] = useState(false);
  const [linkCopied, setLinkCopied] = useState(false);

  useEffect(() => {
    if (account.quick) return;
    let alive = true;
    getConnectCode()
      .then((value) => {
        if (alive) setConnect(value);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [account.quick, account.deviceId]);

  async function rotateCode() {
    if (connectBusy) return;
    setConnectBusy(true);
    try {
      const next = await rotateConnectCode();
      setConnect(next);
      // 换码后旧链接也随之失效，避免界面上留着一条用不了的链接。
      setLink(null);
      notify("已换新的连接码，旧码立即失效", "success");
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setConnectBusy(false);
    }
  }

  async function generateLink() {
    if (linkBusy) return;
    setLinkBusy(true);
    try {
      const next = await createConnectLink(account.deviceId || undefined);
      setLink(next);
      try {
        await navigator.clipboard.writeText(next.url);
        setLinkCopied(true);
        notify("连接链接已生成并复制", "success");
      } catch {
        notify("连接链接已生成，请手动复制", "success");
      }
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setLinkBusy(false);
    }
  }

  /* ------------------------------------------------------------ 拖拽发送 */

  // 拖拽经过的卡片：null 表示没有悬停在任何卡片上。
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  // 有文件在页面里拖动（用于给所有卡片显示可投放状态）。
  const [dragging, setDragging] = useState(false);
  // 页面级 dragenter/dragleave 计数器：子元素冒泡会让 leave 误触发。
  const dragDepth = useRef(0);
  // 卡片自己的 enter/leave 计数器，按设备 ID 分开记。
  const cardDepth = useRef<Record<string, number>>({});
  // 每台设备最近一次发送状态。
  const [transfers, setTransfers] = useState<Record<string, WallTransfer>>({});
  // 拖到离线 / 不可控设备时的即时提示（卡片上的短暂红框）。
  const [dropRejectId, setDropRejectId] = useState<string | null>(null);
  const rejectTimer = useRef<number | null>(null);
  // 已拖入文件、等用户确认是否建立会话。
  const [pendingDrop, setPendingDrop] = useState<DropRequest | null>(null);
  const [dropConfirmError, setDropConfirmError] = useState<string | null>(null);
  const [sessionDeviceId, setSessionDeviceId] = useState<string | null>(null);

  // 事件回调和发送循环共享的运行时状态，全部走 ref，避免读到旧闭包。
  const transfersRef = useRef<Record<string, WallTransfer>>({});
  const runsRef = useRef<Map<number, UploadRun>>(new Map());
  // tid 还没分配时的发送也要能取消，按设备另存一份。
  const deviceRuns = useRef<Map<string, UploadRun>>(new Map());
  const busyDevices = useRef<Set<string>>(new Set());
  const mountedRef = useRef(true);
  // 屏幕墙自己建立的会话在离开页面时要断开；复用控制页的会话不能碰。
  const ownTokenRef = useRef<number | null>(null);
  // 后台只有一个控制会话：不同设备的发送串行执行，避免互相顶掉会话。
  const sendQueue = useRef<Promise<void>>(Promise.resolve());

  function commitTransfer(deviceId: string, patch: Partial<WallTransfer>) {
    // 只覆盖真正给出的字段：调用方常写 `x: cond ? value : undefined`。
    const clean: Partial<WallTransfer> = {};
    for (const [key, value] of Object.entries(patch)) {
      if (value !== undefined) {
        (clean as Record<string, unknown>)[key] = value;
      }
    }
    setTransfers((current) => {
      const base: WallTransfer = current[deviceId] ?? {
        deviceId,
        status: "sending",
        tid: 0,
        files: 0,
        total: 0,
        done: 0,
        completed: 0,
        message: null,
      };
      const next = { ...base, ...clean };
      transfersRef.current = { ...transfersRef.current, [deviceId]: next };
      return { ...current, [deviceId]: next };
    });
  }

  function flashReject(deviceId: string) {
    setDropRejectId(deviceId);
    if (rejectTimer.current !== null) window.clearTimeout(rejectTimer.current);
    rejectTimer.current = window.setTimeout(() => {
      rejectTimer.current = null;
      if (mountedRef.current) setDropRejectId(null);
    }, 1800);
  }

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (rejectTimer.current !== null) window.clearTimeout(rejectTimer.current);
      for (const run of runsRef.current.values()) {
        if (run.watchdog !== null) window.clearTimeout(run.watchdog);
      }
      runsRef.current.clear();
      // 离开屏幕墙时断开本页建立的会话；控制页留下的会话不归这里管。
      const token = ownTokenRef.current;
      ownTokenRef.current = null;
      if (token !== null) void stopControl(token).catch(() => {});
    };
  }, []);

  // 控制页留下的会话可能已经存在：记录它，决定 drop 时能否直接复用。
  useEffect(() => {
    if (!isTauri) return;
    let alive = true;
    const refresh = () => {
      getControlInfo()
        .then((info) => {
          if (alive) setSessionDeviceId(info?.deviceId ?? null);
        })
        .catch(() => {});
    };
    refresh();
    return () => {
      alive = false;
    };
  }, []);

  const findRun = (tid: number) => runsRef.current.get(tid) ?? null;

  /** 收尾一次发送：清掉运行态、更新界面、给出提示。 */
  function settleRun(
    run: UploadRun,
    status: Extract<DropStatus, "done" | "error" | "cancelled">,
    message: string | null,
  ) {
    if (run.settled) return;
    run.settled = true;
    if (run.watchdog !== null) {
      window.clearTimeout(run.watchdog);
      run.watchdog = null;
    }
    if (run.tid) runsRef.current.delete(run.tid);
    if (deviceRuns.current.get(run.deviceId) === run) {
      deviceRuns.current.delete(run.deviceId);
    }
    busyDevices.current.delete(run.deviceId);
    releaseOwnSession(run);
    if (!mountedRef.current) return;
    commitTransfer(run.deviceId, {
      status,
      message,
      done: status === "done" ? transfersRef.current[run.deviceId]?.total ?? 0 : undefined,
      completed: status === "done" ? run.files : undefined,
    });
    if (status === "done") {
      notify(`已发送到「${run.deviceName}」：${run.files} 个文件`, "success");
    } else if (status === "error") {
      notify(
        `发送到「${run.deviceName}」失败：${message ?? "未知原因"}`,
        "error",
      );
    }
  }

  // 被控端推送的传输事件：按 tid 关联到具体设备，多台并发互不干扰。
  useEffect(() => {
    const off = subscribeFileTransfer((event: FileTransferEvent) => {
      const run = findRun(event.tid);
      if (!run) return;
      if (event.direction && event.direction !== "upload") return;

      if (event.kind === "progress") {
        if (!mountedRef.current) return;
        const total = event.total ?? transfersRef.current[run.deviceId]?.total ?? 0;
        commitTransfer(run.deviceId, {
          status: "sending",
          done: Math.min(event.done ?? 0, total),
          total,
        });
        return;
      }

      if (event.kind === "file-done") {
        if (event.ok === false) {
          settleRun(run, "error", event.msg ?? "被控端保存文件失败");
          void cancelFileTransfer(run.tid).catch(() => {});
          return;
        }
        if (!mountedRef.current) return;
        const current = transfersRef.current[run.deviceId];
        commitTransfer(run.deviceId, {
          completed: Math.min((current?.completed ?? 0) + 1, run.files),
        });
        return;
      }

      if (event.kind === "complete") {
        settleRun(run, "done", null);
        return;
      }

      if (event.kind === "cancelled") {
        // 带 msg 是被控端报告失败，没有则是本端/对端主动取消。
        const message = event.msg ?? "";
        if (message) settleRun(run, "error", message);
        else settleRun(run, "cancelled", null);
        return;
      }

      if (event.kind === "error") {
        settleRun(run, "error", event.msg ?? "被控端返回错误");
      }
    });
    return off;
  }, []);

  /** 真正执行一次发送：必要时先建立控制会话，再逐个文件分块上传。 */
  async function startWallUpload(
    device: DeviceView,
    files: File[],
    token: number,
    ownsSession: boolean,
  ) {
    const deviceName = device.name;
    const run: UploadRun = {
      tid: 0,
      deviceId: device.id,
      deviceName,
      files: files.length,
      cancelled: false,
      settled: false,
      watchdog: null,
      ownsSession,
      token,
    };
    busyDevices.current.add(device.id);
    deviceRuns.current.set(device.id, run);
    commitTransfer(device.id, {
      status: "hashing",
      tid: 0,
      files: files.length,
      total: files.reduce((sum, file) => sum + file.size, 0),
      done: 0,
      completed: 0,
      message: null,
    });

    let tid = 0;
    // 只有在「数据已全部交出、等被控端确认」阶段才启用超时兜底；
    // 传输本身可能有进度事件，不能把大文件当成卡死。
    let awaitingConfirm = false;
    try {
      // 先算 SHA256：被控端用它校验完整性，也用它匹配上次没传完的前缀。
      const hashed: { file: File; path: string; size: number; sha256: string }[] =
        [];
      for (const file of files) {
        hashed.push({
          file,
          path: droppedRelativePath(file),
          size: file.size,
          sha256: await hashLocalFile(file),
        });
      }
      if (run.cancelled || run.settled) return;

      const offered = await offerFiles(
        hashed.map((item) => ({
          path: item.path,
          size: item.size,
          sha256: item.sha256,
        })),
      );
      tid = offered.tid;
      run.tid = tid;
      runsRef.current.set(tid, run);

      const resumeOffsets = new Map<number, number>(
        offered.resume.map((entry) => [entry.item, entry.offset]),
      );
      const resumedTotal = offered.resume.reduce(
        (sum, entry) => sum + entry.offset,
        0,
      );
      const total = offered.total || hashed.reduce((sum, item) => sum + item.size, 0);
      if (mountedRef.current) {
        commitTransfer(device.id, {
          status: "sending",
          tid,
          total,
          done: Math.min(resumedTotal, total),
        });
      }

      let sent = Math.min(resumedTotal, total);
      for (let index = 0; index < hashed.length; index += 1) {
        if (run.cancelled || run.settled) return;
        const item = hashed[index];
        const startOffset = Math.min(resumeOffsets.get(index) ?? 0, item.size);
        for (let offset = startOffset; offset < item.size; offset += CHUNK_BYTES) {
          if (run.cancelled || run.settled) return;
          const length = Math.min(CHUNK_BYTES, item.size - offset);
          const data = await readChunkBase64(item.file, offset, length);
          if (run.cancelled || run.settled) return;
          await uploadChunk(tid, index, offset, data);
          sent += length;
          if (mountedRef.current) {
            commitTransfer(device.id, { done: Math.max(0, Math.min(sent, total)) });
          }
        }
      }

      if (run.cancelled || run.settled) return;
      if (mountedRef.current) commitTransfer(device.id, { status: "waiting" });

      if (!isTauri) {
        // 浏览器预览没有真实事件：所有数据块都已被接受后直接完成。
        settleRun(run, "done", null);
        return;
      }
      // 数据已全部交给后端，等被控端的 file.done / file.complete。
      // 兜底：若确认一直不来（被控端卡住 / 会话断开），超时给出明确失败。
      awaitingConfirm = true;
      run.watchdog = window.setTimeout(() => {
        settleRun(run, "error", "被控端长时间没有确认，传输可能已中断");
      }, 60_000);
    } catch (error) {
      if (run.cancelled) {
        settleRun(run, "cancelled", null);
        return;
      }
      const message = errorLine(error);
      settleRun(run, "error", message);
      if (tid !== 0) void cancelFileTransfer(tid).catch(() => {});
    } finally {
      // 出错/取消时上面已经收尾；演示模式也不会来事件，这里补一次收尾，
      // 免得进度卡在「等待被控端确认」。成功路径靠事件收尾，不能在这里断会话
      // —— 被控端要在数据收齐后才回 file.done / file.complete。
      if (!awaitingConfirm && !run.settled) {
        releaseOwnSession(run);
      }
    }
  }

  /** 自己建立的会话用完即断；复用控制页的会话不动。 */
  function releaseOwnSession(run: UploadRun) {
    if (!run.ownsSession || run.token === null) return;
    const token = run.token;
    run.ownsSession = false;
    if (ownTokenRef.current === token) {
      ownTokenRef.current = null;
      void stopControl(token).catch(() => {});
    }
  }

  /**
   * 后台只有一个控制会话，所以「连接 + 发送」串行排队：
   * 多台设备同时拖文件时不会互相顶掉会话，也不会把文件发错设备。
   */
  function enqueueSend(device: DeviceView, files: File[]) {
    const total = files.reduce((sum, file) => sum + file.size, 0);
    // 从排队开始就占住这台设备，避免同一台设备连拖两次互相覆盖状态。
    busyDevices.current.add(device.id);
    commitTransfer(device.id, {
      status: "queued",
      tid: 0,
      files: files.length,
      total,
      done: 0,
      completed: 0,
      message: null,
    });

    const task = async () => {
      const pending = transfersRef.current[device.id];
      // 排队期间被取消了：不再发起。
      if (!pending || pending.status !== "queued") {
        busyDevices.current.delete(device.id);
        return;
      }
      let token = 0;
      let ownsSession = false;
      try {
        const active = isTauri ? await getControlInfo() : null;
        if (active && active.deviceId === device.id) {
          // 已经有到这台设备的会话（控制页留下的或本页建立的）：直接复用。
          token = active.clientToken;
        } else {
          // 需要控制会话才能传文件：没有就现建一条，用完断开。
          const nextToken = ++wallTokenSeq;
          if (mountedRef.current) {
            commitTransfer(device.id, {
              status: "connecting",
              tid: 0,
              files: files.length,
              total,
              done: 0,
              completed: 0,
              message: null,
            });
          }
          const info = await startControl(device.id, nextToken);
          token = info.clientToken ?? nextToken;
          ownTokenRef.current = token;
          ownsSession = true;
          if (mountedRef.current) setSessionDeviceId(device.id);
        }
      } catch (error) {
        if (ownsSession && ownTokenRef.current === token) {
          ownTokenRef.current = null;
          void stopControl(token).catch(() => {});
        }
        const message = errorLine(error);
        if (mountedRef.current) {
          commitTransfer(device.id, {
            status: "error",
            tid: 0,
            files: files.length,
            total,
            done: 0,
            completed: 0,
            message,
          });
          notify(
            `发送到「${device.name}」失败：${message}。被控端需要点「允许」才能传文件。`,
            "error",
          );
        }
        return;
      }

      // 连接期间被取消：别浪费这条刚建好的会话。
      const latest = transfersRef.current[device.id];
      if (!latest || latest.status === "cancelled") {
        if (ownsSession && ownTokenRef.current === token) {
          ownTokenRef.current = null;
          void stopControl(token).catch(() => {});
        }
        return;
      }
      await startWallUpload(device, files, token, ownsSession);
    };

    sendQueue.current = sendQueue.current.then(task, task);
  }

  function confirmDrop() {
    const request = pendingDrop;
    if (!request) return;
    const blocked = dropBlockReason(request.device);
    if (blocked) {
      setDropConfirmError(blocked);
      return;
    }
    setPendingDrop(null);
    setDropConfirmError(null);
    enqueueSend(request.device, request.files);
  }

  function cancelDrop() {
    setPendingDrop(null);
    setDropConfirmError(null);
  }

  /** 取消某台设备的发送：排队中、连接中、传输中都能取消。 */
  async function cancelWallUpload(deviceId: string) {
    const transfer = transfersRef.current[deviceId];
    if (!transfer || !isActiveDrop(transfer.status)) return;
    const run = deviceRuns.current.get(deviceId) ?? null;
    if (run) {
      run.cancelled = true;
      settleRun(run, "cancelled", null);
      if (run.tid) await cancelFileTransfer(run.tid).catch(() => {});
    } else {
      // 还在排队 / 连接中：只改状态，队列轮到时会自己跳过。
      commitTransfer(deviceId, { status: "cancelled", message: null });
      busyDevices.current.delete(deviceId);
    }
  }

  /* ------------------------------------------------------- 拖拽事件处理 */

  function resetDragState() {
    dragDepth.current = 0;
    cardDepth.current = {};
    setDragging(false);
    setDragOverId(null);
  }

  // 页面级只负责「有文件在拖动」和兜底 preventDefault；
  // 卡片的事件继续冒泡上来，两边状态都能保持准确。
  function handlePageDragEnter(event: DragEvent<HTMLDivElement>) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    dragDepth.current += 1;
    setDragging(true);
  }

  function handlePageDragOver(event: DragEvent<HTMLDivElement>) {
    // 不 preventDefault 浏览器会拒绝 drop —— 这是最常见的坑。
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    if (event.dataTransfer) {
      // 卡片自己会再覆盖成 none（离线设备）；这里给默认的可投放光标。
      event.dataTransfer.dropEffect = "copy";
    }
  }

  function handlePageDragLeave(event: DragEvent<HTMLDivElement>) {
    if (!hasFilePayload(event)) return;
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) {
      setDragging(false);
      setDragOverId(null);
      cardDepth.current = {};
    }
  }

  function handlePageDrop(event: DragEvent<HTMLDivElement>) {
    // 落在卡片以外：清掉高亮，不做任何传输。
    event.preventDefault();
    resetDragState();
  }

  function handleCardDragEnter(device: DeviceView, event: DragEvent<HTMLElement>) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    // 不 stopPropagation：页面级的计数器也要收到 enter/leave 才不会错位。
    cardDepth.current[device.id] = (cardDepth.current[device.id] ?? 0) + 1;
    setDragOverId(device.id);
  }

  function handleCardDragOver(device: DeviceView, event: DragEvent<HTMLElement>) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    if (event.dataTransfer) {
      // 离线/不可控设备显示禁止光标，松手也不会发出去。
      event.dataTransfer.dropEffect = dropBlockReason(device) ? "none" : "copy";
    }
  }

  function handleCardDragLeave(device: DeviceView, event: DragEvent<HTMLElement>) {
    if (!hasFilePayload(event)) return;
    // 用每张卡片自己的计数器抵消子元素冒泡产生的 leave，避免高亮闪烁。
    const next = Math.max(0, (cardDepth.current[device.id] ?? 0) - 1);
    cardDepth.current[device.id] = next;
    if (next === 0) {
      setDragOverId((current) => (current === device.id ? null : current));
    }
  }

  function handleCardDrop(device: DeviceView, event: DragEvent<HTMLElement>) {
    event.preventDefault();
    // 停止冒泡：页面级的 onDrop 只处理「落在空白处」，不应该再跑一遍。
    event.stopPropagation();
    const files = Array.from(event.dataTransfer?.files ?? []);
    resetDragState();
    if (files.length === 0) {
      notify("没有检测到可发送的文件；拖拽文件夹请先压缩或到文件页发送", "error");
      return;
    }
    if (busyDevices.current.has(device.id)) {
      notify(`「${device.name}」正在发送中，请等待完成或取消`, "info");
      return;
    }
    const blocked = dropBlockReason(device);
    if (blocked) {
      flashReject(device.id);
      notify(`无法发送到「${device.name}」：${blocked}`, "error");
      return;
    }
    if (files.some((file) => file.size === 0)) {
      notify("所拖文件里有 0 字节的空文件，请去掉后重试", "error");
      return;
    }
    // 文件传输必须有一条控制会话：先问一次，避免屏幕墙只读时静默失败。
    const other =
      sessionDeviceId && sessionDeviceId !== device.id
        ? devices.find((item) => item.id === sessionDeviceId)?.name ?? null
        : null;
    setDropConfirmError(null);
    setPendingDrop({ device, files, switchFrom: other });
  }


  const onlinePreviewKey = useMemo(
    () =>
      devices
        .filter((device) => device.online)
        .map((device) => device.id)
        .join(","),
    [devices],
  );

  // 在线设备各开一条只读缩略图会话（1 FPS / 320px 宽），离开页面时全部断开。
  useEffect(() => {
    if (!isTauri) return;
    const ids = onlinePreviewKey ? onlinePreviewKey.split(",") : [];
    void setWallPreview(ids).catch(() => {});
  }, [onlinePreviewKey]);

  useEffect(() => {
    if (!isTauri) return;
    const offFrame = subscribeWallFrame((frame) => {
      setFrames((prev) =>
        prev[frame.deviceId] === frame.dataUrl
          ? prev
          : { ...prev, [frame.deviceId]: frame.dataUrl },
      );
    });
    const offState = subscribeWallState((event) => {
      if (event.state === "error" || event.state === "closed") {
        setFrames((prev) => {
          if (!(event.deviceId in prev)) return prev;
          const next = { ...prev };
          delete next[event.deviceId];
          return next;
        });
        setPreviewHints((prev) => ({
          ...prev,
          [event.deviceId]: event.message ?? "缩略图不可用",
        }));
      } else if (event.state === "live") {
        setPreviewHints((prev) => {
          if (!(event.deviceId in prev)) return prev;
          const next = { ...prev };
          delete next[event.deviceId];
          return next;
        });
      }
    });
    return () => {
      offFrame();
      offState();
      void stopWallPreview().catch(() => {});
    };
  }, []);

  // 指标和帧分开订阅：帧是 1 FPS，指标是 5 秒一次，别让两边互相触发重渲染。
  useEffect(() => {
    if (!isTauri) return;
    const offMetrics = subscribeWallMetrics((event) => {
      setMetrics((prev) => {
        const current = prev[event.deviceId];
        if (
          current &&
          current.cpu === event.cpu &&
          current.mem === event.mem
        ) {
          return prev;
        }
        return {
          ...prev,
          [event.deviceId]: { cpu: event.cpu, mem: event.mem },
        };
      });
    });
    return offMetrics;
  }, []);

  useEffect(() => {
    if (!refreshing) return;
    const timer = window.setTimeout(() => {
      setRefreshing(false);
      notify(`设备列表已刷新，共 ${devices.length} 台设备`, "success");
    }, 700);
    return () => window.clearTimeout(timer);
  }, [refreshing, devices.length, notify]);

  const onlineCount = useMemo(
    () => devices.filter((device) => device.online).length,
    [devices],
  );
  const selectedSet = useMemo(() => new Set(selectedIds), [selectedIds]);

  const filtered = useMemo(() => {
    const keyword = query.trim().toLowerCase();
    return devices.filter((device) => {
      if (filter === "online" && !device.online) return false;
      if (filter === "offline" && device.online) return false;
      if (!keyword) return true;
      return [
        device.name,
        device.os,
        device.version,
        device.location ?? "",
        device.ip ?? "",
      ].some((field) => field.toLowerCase().includes(keyword));
    });
  }, [devices, filter, query]);

  const filterOptions: { value: WallFilter; label: string; count: number }[] = [
    { value: "all", label: "全部", count: devices.length },
    { value: "online", label: "在线", count: onlineCount },
    { value: "offline", label: "离线", count: devices.length - onlineCount },
  ];

  const hasSelection = selectedIds.length > 0;

  function handleRefresh() {
    if (refreshing) return;
    setRefreshing(true);
    notify("正在刷新设备状态…");
  }

  function clearFilters() {
    setQuery("");
    setFilter("all");
  }

  async function copyValue(label: string, value: string) {
    if (!value) {
      notify(`暂无可复制的${label}`, "error");
      return;
    }
    try {
      if (!navigator.clipboard?.writeText) {
        notify("当前环境不支持自动复制，请手动选择后复制", "error");
        return;
      }
      await navigator.clipboard.writeText(value);
      notify(`${label}已复制`, "success");
    } catch {
      notify(`复制失败，请手动选择${label}后复制`, "error");
    }
  }

  function renderCard(device: DeviceView) {
    const selected = selectedSet.has(device.id);
    const isLocal = device.id === account.deviceId;
    // 优先用被控端 status 推来的真实值；没有会话时退回设备目录里的字段。
    const live = device.online ? metrics[device.id] : undefined;
    const cpu = live ? Math.round(live.cpu) : device.online ? device.cpu ?? null : null;
    const mem = live ? Math.round(live.mem) : device.online ? device.mem ?? null : null;
    const systemTitle = `${device.os} ${device.version}${
      device.location ? ` · ${device.location}` : ""
    }`;

    const blocked = dropBlockReason(device);
    const isDragOver = dragOverId === device.id;
    const rejecting = dropRejectId === device.id;
    const transfer = transfers[device.id];
    const showTransfer = transfer && transfer.status !== "cancelled";
    const cardClass = [
      "wall-card",
      selected ? "is-selected" : "",
      dragging && !blocked ? "is-drop-ready" : "",
      isDragOver && !blocked ? "is-drop-over" : "",
      isDragOver && blocked ? "is-drop-blocked" : "",
      rejecting ? "is-drop-reject" : "",
    ]
      .filter(Boolean)
      .join(" ");

    return (
      <article
        key={device.id}
        className={cardClass}
        role="button"
        tabIndex={0}
        aria-label={`打开 ${device.name}`}
        onClick={() => onOpenDevice(device.id)}
        onKeyDown={(event) =>
          activateCard(event, () => onOpenDevice(device.id))
        }
        onDragEnter={(event) => handleCardDragEnter(device, event)}
        onDragOver={(event) => handleCardDragOver(device, event)}
        onDragLeave={(event) => handleCardDragLeave(device, event)}
        onDrop={(event) => handleCardDrop(device, event)}
      >
        <div className="wall-card-preview">
          <DesktopPreview
            device={device}
            live={device.online}
            showName={false}
            frame={frames[device.id]}
            hint={previewHints[device.id]}
          />
          <button
            type="button"
            role="checkbox"
            aria-checked={selected}
            aria-label={
              selected ? `取消选择 ${device.name}` : `选择 ${device.name}`
            }
            className={`wall-check ${selected ? "is-checked" : ""}`}
            onClick={(event) => {
              event.stopPropagation();
              onToggleSelect(device.id);
            }}
          >
            {selected && <Check size={14} strokeWidth={1.75} />}
          </button>

          {isDragOver && (
            <div
              className={`wall-drop-overlay ${blocked ? "is-blocked" : ""}`}
              aria-hidden="true"
            >
              {blocked ? (
                <AlertCircle size={20} strokeWidth={1.75} />
              ) : (
                <UploadCloud size={20} strokeWidth={1.75} />
              )}
              <span>
                {blocked
                  ? blocked
                  : `松开即发送到「${device.name}」`}
              </span>
            </div>
          )}
        </div>

        <div className="wall-card-body">
          <div className="wall-card-head">
            <strong className="wall-card-name" title={device.name}>
              {device.name}
            </strong>
            {isLocal && <span className="badge badge-muted wall-local">本机</span>}
          </div>

          <div className="wall-card-meta">
            <span className={`wall-status ${statusClass(device)}`}>
              <i aria-hidden="true" />
              {statusLabel(device)}
            </span>
            <span className="wall-meta-sep" aria-hidden="true" />
            <span className={`wall-latency ${latencyClass(device)}`}>
              {latencyText(device)}
            </span>
            <span className="wall-meta-sep" aria-hidden="true" />
            <span className="wall-card-os" title={systemTitle}>
              {device.os}
            </span>
          </div>

          <div className="wall-metrics">
            <Metric label="CPU" value={cpu} />
            <Metric label="内存" value={mem} />
          </div>

          {showTransfer && (
            <TransferStatus
              transfer={transfer}
              onCancel={() => void cancelWallUpload(device.id)}
            />
          )}
        </div>

        <div className="wall-card-foot">
          <span className="wall-seen">
            {device.online
              ? "当前在线"
              : `最后在线 ${formatLastSeen(device.lastSeen)}`}
          </span>
          <div className="wall-card-actions">
            <button
              type="button"
              className="btn btn-sm btn-ghost"
              onClick={(event) => {
                event.stopPropagation();
                onOpenDevice(device.id);
              }}
            >
              <MonitorPlay size={14} strokeWidth={1.75} />
              控制
            </button>
            <button
              type="button"
              className="btn btn-sm btn-ghost"
              onClick={(event) => {
                event.stopPropagation();
                onFiles(device.id);
              }}
            >
              <FolderUp size={14} strokeWidth={1.75} />
              文件
            </button>
          </div>
        </div>
      </article>
    );
  }

  function renderRow(device: DeviceView) {
    const selected = selectedSet.has(device.id);
    const isLocal = device.id === account.deviceId;
    const blocked = dropBlockReason(device);
    const isDragOver = dragOverId === device.id;
    const rejecting = dropRejectId === device.id;
    const transfer = transfers[device.id];
    const showTransfer = transfer && transfer.status !== "cancelled";
    const rowClass = [
      "wall-row",
      selected ? "is-selected" : "",
      dragging && !blocked ? "is-drop-ready" : "",
      isDragOver && !blocked ? "is-drop-over" : "",
      isDragOver && blocked ? "is-drop-blocked" : "",
      rejecting ? "is-drop-reject" : "",
      showTransfer ? "has-transfer" : "",
    ]
      .filter(Boolean)
      .join(" ");

    return (
      <div
        key={device.id}
        className={rowClass}
        role="button"
        tabIndex={0}
        aria-label={`打开 ${device.name}`}
        onClick={() => onOpenDevice(device.id)}
        onKeyDown={(event) => {
          // 行内的按钮自己处理键盘，不要在冒泡时重复触发「打开」。
          if (event.target !== event.currentTarget) return;
          if (event.key !== "Enter" && event.key !== " ") return;
          event.preventDefault();
          onOpenDevice(device.id);
        }}
        onDragEnter={(event) => handleCardDragEnter(device, event)}
        onDragOver={(event) => handleCardDragOver(device, event)}
        onDragLeave={(event) => handleCardDragLeave(device, event)}
        onDrop={(event) => handleCardDrop(device, event)}
      >
        <button
          type="button"
          role="checkbox"
          aria-checked={selected}
          aria-label={
            selected ? `取消选择 ${device.name}` : `选择 ${device.name}`
          }
          className={`wall-check wall-check-row ${selected ? "is-checked" : ""}`}
          onClick={(event) => {
            event.stopPropagation();
            onToggleSelect(device.id);
          }}
        >
          {selected && <Check size={14} strokeWidth={1.75} />}
        </button>

        <div className="wall-cell-device">
          <span className="wall-device-icon" aria-hidden="true">
            <Monitor size={16} strokeWidth={1.75} />
          </span>
          <strong title={device.name}>{device.name}</strong>
          {isLocal && <span className="badge badge-muted wall-local">本机</span>}
          {isDragOver && (
            <span
              className={`wall-row-drop-hint ${blocked ? "is-blocked" : ""}`}
              aria-hidden="true"
            >
              {blocked ? blocked : "松开即发送"}
            </span>
          )}
        </div>

        <div className="wall-cell-status">
          <span className={`wall-status ${statusClass(device)}`}>
            <i aria-hidden="true" />
            {statusLabel(device)}
          </span>
        </div>

        <span
          className="wall-cell-system wall-col-system"
          title={`${device.os} ${device.version}`}
        >
          {device.os || "—"}
        </span>

        <span className="wall-cell-ip wall-col-ip">{device.ip ?? "—"}</span>

        <span className={`wall-cell-latency ${latencyClass(device)}`}>
          {latencyText(device)}
        </span>

        <span className="wall-cell-seen wall-col-seen">
          {device.online ? "当前在线" : formatLastSeen(device.lastSeen)}
        </span>

        <div className="wall-cell-action wall-col-action">
          {showTransfer ? (
            <TransferStatus
              transfer={transfer}
              compact
              onCancel={() => void cancelWallUpload(device.id)}
            />
          ) : (
            <button
              type="button"
              className="btn btn-sm btn-ghost"
              onClick={(event) => {
                event.stopPropagation();
                onOpenDevice(device.id);
              }}
            >
              <MonitorPlay size={14} strokeWidth={1.75} />
              控制
            </button>
          )}
        </div>
      </div>
    );
  }

  return (
    <div
      className={`wall ${dragging ? "is-dragging" : ""}`}
      aria-busy={refreshing}
      onDragEnter={handlePageDragEnter}
      onDragOver={handlePageDragOver}
      onDragLeave={handlePageDragLeave}
      onDrop={handlePageDrop}
    >
      {dragging && (
        <div className="wall-drop-banner" aria-hidden="true">
          <UploadCloud size={15} strokeWidth={1.75} />
          <span>松开到任意在线设备卡片上，即可把文件发到那台电脑</span>
        </div>
      )}
      {!account.quick && (
        <div className="wall-connect" aria-label="本机连接码">
          <div className="wall-connect-main">
            <span className="wall-connect-label">
              <Hash size={14} strokeWidth={1.75} aria-hidden="true" />
              本机连接码
            </span>
            <code className="wall-connect-code mono">
              {connect ? formatConnectCode(connect.code) : "—"}
            </code>
            <span
              className={`wall-connect-state ${
                connect?.enabled ? "is-on" : ""
              }`}
            >
              <i aria-hidden="true" />
              {connect?.enabled ? "可连接" : "未开启"}
            </span>
            <button
              type="button"
              className="icon-btn"
              onClick={() => void copyValue("连接码", connect?.code ?? "")}
              disabled={!connect?.code}
              title="复制连接码"
              aria-label="复制连接码"
            >
              <Copy size={14} strokeWidth={1.75} />
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => void rotateCode()}
              disabled={connectBusy || !connect}
              title="换一个新的连接码，旧码立即失效"
            >
              <RefreshCw
                size={13}
                strokeWidth={1.75}
                className={connectBusy ? "spin" : undefined}
              />
              换一个
            </button>
            {connect?.hasPassword && (
              <span className="wall-connect-pass" title="连接密码（在设置 → 安全 里修改）">
                <KeyRound size={13} strokeWidth={1.75} aria-hidden="true" />
                <span className="mono">{connect.password ?? "已设置"}</span>
              </span>
            )}
          </div>

          <div className="wall-connect-link">
            <div className="field-control wall-connect-link-field">
              <Link2 size={14} strokeWidth={1.75} aria-hidden="true" />
              <input
                value={link?.url ?? ""}
                readOnly
                spellCheck={false}
                placeholder="生成一条只能用一次的链接，点开即连"
                aria-label="一次性连接链接"
                onFocus={(event) => event.currentTarget.select()}
              />
            </div>
            {link && (
              <button
                type="button"
                className="icon-btn"
                onClick={() => void copyValue("连接链接", link.url)}
                title="复制连接链接"
                aria-label="复制连接链接"
              >
                {linkCopied ? (
                  <Check size={14} strokeWidth={1.75} />
                ) : (
                  <Copy size={14} strokeWidth={1.75} />
                )}
              </button>
            )}
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => void generateLink()}
              disabled={linkBusy || !connect?.enabled}
              title={
                connect?.enabled
                  ? "生成 10 分钟有效、只能用一次的连接链接"
                  : "先在设置 → 安全 里开启「允许连接码连接」"
              }
            >
              <Link2
                size={13}
                strokeWidth={1.75}
                className={linkBusy ? "spin" : undefined}
              />
              生成链接
            </button>
          </div>
        </div>
      )}

      <div className="wall-toolbar">
        <div className="wall-toolbar-main">
          <div className="wall-filters" role="group" aria-label="设备筛选">
            {filterOptions.map((option) => (
              <button
                key={option.value}
                type="button"
                className={`chip wall-filter ${
                  filter === option.value ? "is-active" : ""
                }`}
                aria-pressed={filter === option.value}
                onClick={() => setFilter(option.value)}
              >
                {option.label}
                <span className="wall-filter-count">{option.count}</span>
              </button>
            ))}
          </div>

          <div className="field-control wall-search">
            <Search size={15} strokeWidth={1.75} aria-hidden="true" />
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="搜索设备、系统、位置或 IP"
              aria-label="搜索设备"
              spellCheck={false}
            />
            {query && (
              <button
                type="button"
                className="field-eye"
                onClick={() => setQuery("")}
                aria-label="清空搜索"
              >
                <X size={14} strokeWidth={1.75} />
              </button>
            )}
          </div>
        </div>

        <div className="wall-toolbar-actions">
          <button
            type="button"
            className="icon-btn wall-refresh"
            onClick={handleRefresh}
            disabled={refreshing}
            aria-label="刷新设备状态"
            title="刷新设备状态"
          >
            <RefreshCw
              size={16}
              strokeWidth={1.75}
              className={refreshing ? "spin" : undefined}
            />
          </button>

          <Segmented<WallView>
            value={view}
            options={[
              {
                value: "grid",
                label: "网格",
                icon: <LayoutGrid size={14} strokeWidth={1.75} />,
              },
              {
                value: "list",
                label: "列表",
                icon: <List size={14} strokeWidth={1.75} />,
              },
            ]}
            onChange={setView}
          />

          <button
            type="button"
            className="btn btn-primary"
            onClick={() => setAddOpen(true)}
          >
            <Plus size={15} strokeWidth={1.75} />
            添加设备
          </button>
        </div>
      </div>

      {refreshing ? (
        view === "grid" ? (
          <SkeletonGrid />
        ) : (
          <SkeletonList />
        )
      ) : devices.length === 0 ? (
        <div className="wall-empty">
          <EmptyState
            icon={<MonitorSmartphone size={18} strokeWidth={1.75} />}
            title="还没有任何设备"
            hint="同一账号的电脑安装客户端并登录后，设备会自动出现在这里。"
            action={
              <div className="wall-empty-actions">
                <button
                  type="button"
                  className="btn btn-primary"
                  onClick={() => setAddOpen(true)}
                >
                  <Plus size={15} strokeWidth={1.75} />
                  添加设备
                </button>
                <button
                  type="button"
                  className="btn btn-ghost"
                  onClick={onDevices}
                >
                  设备管理
                </button>
              </div>
            }
          />
        </div>
      ) : filtered.length === 0 ? (
        <div className="wall-empty">
          <EmptyState
            icon={<SearchX size={18} strokeWidth={1.75} />}
            title="没有匹配的设备"
            hint="换个关键词，或调整上方的在线状态筛选。"
            action={
              <button
                type="button"
                className="btn btn-soft"
                onClick={clearFilters}
              >
                清除筛选
              </button>
            }
          />
        </div>
      ) : view === "grid" ? (
        <div className="wall-grid">{filtered.map(renderCard)}</div>
      ) : (
        <div className="wall-table">
          <div className="wall-table-head" aria-hidden="true">
            <span />
            <span>设备</span>
            <span>状态</span>
            <span className="wall-col-system">系统</span>
            <span className="wall-col-ip">IP</span>
            <span>延迟</span>
            <span className="wall-col-seen">最后在线</span>
            <span className="wall-col-action">操作</span>
          </div>
          {filtered.map(renderRow)}
        </div>
      )}

      {hasSelection && (
        <div
          className="wall-selection-bar"
          role="group"
          aria-label="已选设备的批量操作"
        >
          <span className="wall-selection-info">
            已选 <strong>{selectedIds.length}</strong> 台设备
          </span>
          <div className="wall-selection-actions">
            <button type="button" className="btn btn-soft btn-sm" onClick={onGroup}>
              <Layers size={14} strokeWidth={1.75} />
              群控
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => onFiles()}
            >
              <FolderUp size={14} strokeWidth={1.75} />
              传文件
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={onClearSelection}
            >
              <X size={14} strokeWidth={1.75} />
              取消选择
            </button>
          </div>
        </div>
      )}

      <Modal
        open={pendingDrop !== null}
        onClose={cancelDrop}
        title="发送文件到远程设备"
        width={480}
        footer={
          <>
            <button type="button" className="btn btn-ghost" onClick={cancelDrop}>
              取消
            </button>
            <button
              type="button"
              className="btn btn-primary"
              onClick={confirmDrop}
            >
              <UploadCloud size={15} strokeWidth={1.75} />
              连接并发送
            </button>
          </>
        }
      >
        <div className="wall-drop-modal">
          <p className="wall-drop-summary">
            将发送 <strong>{pendingDrop?.files.length ?? 0}</strong> 个文件（
            {formatBytes(
              (pendingDrop?.files ?? []).reduce(
                (sum, file) => sum + file.size,
                0,
              ),
            )}
            ）到：
          </p>
          <div className="wall-drop-target">
            <span className="wall-device-icon" aria-hidden="true">
              <Monitor size={16} strokeWidth={1.75} />
            </span>
            <div className="wall-drop-target-copy">
              <strong>{pendingDrop?.device.name ?? ""}</strong>
              <small>
                {pendingDrop?.device.os}
                {pendingDrop?.device.ip ? ` · ${pendingDrop.device.ip}` : ""}
              </small>
            </div>
          </div>

          <ul className="wall-drop-filelist">
            {(pendingDrop?.files ?? []).slice(0, 5).map((file, index) => (
              <li key={`${file.name}-${index}`}>
                <span className="wall-drop-filename" title={droppedRelativePath(file)}>
                  {droppedRelativePath(file)}
                </span>
                <span className="wall-drop-filesize">{formatBytes(file.size)}</span>
              </li>
            ))}
            {(pendingDrop?.files.length ?? 0) > 5 && (
              <li className="wall-drop-more">
                还有 {(pendingDrop?.files.length ?? 0) - 5} 个文件…
              </li>
            )}
          </ul>

          <p className="notice wall-drop-note">
            文件传输走远程控制会话，被控端需要点「允许」才会接收。
            {pendingDrop?.switchFrom
              ? `当前已连着「${pendingDrop.switchFrom}」，确认后会先断开它再连接这台设备。`
              : sessionDeviceId === pendingDrop?.device.id
                ? "将复用当前已有的控制会话，无需再次授权。"
                : "连接成功后自动开始发送，完成后自动断开。文件保存在被控端「下载\ScreenWall」。"}
          </p>

          {dropConfirmError && (
            <p className="notice notice-error wall-drop-error">{dropConfirmError}</p>
          )}
        </div>
      </Modal>

      <Modal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        title="添加设备"
        width={460}
        footer={
          <button
            type="button"
            className="btn btn-ghost"
            onClick={() => setAddOpen(false)}
          >
            关闭
          </button>
        }
      >
        <div className="wall-modal">
          <p className="wall-modal-intro">
            在另一台 Windows 电脑安装 ScreenWall 客户端，用同一账号登录，设备会自动出现在屏幕墙。
          </p>

          <div className="wall-fields">
            <div className="field">
              <span>当前账号</span>
              <div className="field-control wall-copy-control">
                <span className="wall-copy-value" title={account.username}>
                  {account.username || "未登录"}
                </span>
                <button
                  type="button"
                  className="btn btn-sm btn-ghost wall-copy-btn"
                  onClick={() => copyValue("账号", account.username)}
                  disabled={!account.username}
                >
                  <Copy size={14} strokeWidth={1.75} />
                  复制
                </button>
              </div>
            </div>

            <div className="field">
              <span>本机设备 ID</span>
              <div className="field-control wall-copy-control">
                <code className="wall-copy-value" title={account.deviceId}>
                  {account.deviceId || "—"}
                </code>
                <button
                  type="button"
                  className="btn btn-sm btn-ghost wall-copy-btn"
                  onClick={() => copyValue("设备 ID", account.deviceId)}
                  disabled={!account.deviceId}
                >
                  <Copy size={14} strokeWidth={1.75} />
                  复制
                </button>
              </div>
            </div>
          </div>

          <p className="notice wall-modal-note">
            登录后设备会在中转服务器上自动上线，不需要记 IP，也不依赖额外组网软件。
          </p>
        </div>
      </Modal>
    </div>
  );
}
