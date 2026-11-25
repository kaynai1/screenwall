import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ChangeEvent, DragEvent } from "react";
import {
  ArrowDownLeft,
  ArrowDownToLine,
  ArrowUp,
  ArrowUpRight,
  ChevronRight,
  Copy,
  FileText,
  Folder,
  FolderDown,
  FolderUp,
  Home,
  Inbox,
  Loader2,
  Monitor,
  RefreshCw,
  Upload,
  UploadCloud,
  X,
} from "lucide-react";
import {
  cancelFileTransfer,
  downloadRemoteDirectory,
  downloadRemoteFile,
  getControlInfo,
  isTauri,
  listRemoteFiles,
  offerFiles,
  startControl,
  stopControl,
  subscribeControlReconnected,
  subscribeControlState,
  subscribeFileTransfer,
  uploadChunk,
  type FileTransferEvent,
  type RemoteEntry,
  type RemoteListing,
} from "../api";
import { sha256 } from "js-sha256";
import { EmptyState, Progress } from "../components/ui";
import { errorLine } from "../errors";
import { formatBytes } from "../mock";
import { isDirectDeviceId } from "../session";
import type { FilesPageProps } from "../types";
import "./FilesPage.css";

/** 每个上传数据块的原始字节数（后端要求不超过 256 KiB）。 */
const CHUNK_BYTES = 256 * 1024;
/** 下载保存目录（仅用于界面提示，实际路径由后端事件返回）。 */
const DOWNLOAD_DIR_LABEL = "Downloads\\ScreenWall";
/** 浏览器预览没有真实后端，用它补一条可读的演示路径。 */
const DEMO_SAVE_ROOT = "C:\\Users\\demo\\Downloads\\ScreenWall";

type SessionInfo = { deviceId: string; session: number };

type UploadItem = { file: File; path: string; size: number; sha256: string };

type UploadStatus = "sending" | "waiting" | "done" | "cancelled" | "error";

type UploadTask = {
  tid: number;
  root: string;
  items: UploadItem[];
  total: number;
  done: number;
  currentIndex: number;
  completed: number;
  status: UploadStatus;
  speed: number;
  message: string | null;
  startedAt: number;
};

type DownloadStatus =
  | "queued"
  | "downloading"
  | "verifying"
  | "done"
  | "cancelled"
  | "error";

type DownloadTask = {
  tid: number;
  /** 整目录下载的分组号；单独下载为 0。 */
  group: number;
  name: string;
  path: string;
  total: number;
  done: number;
  status: DownloadStatus;
  speed: number;
  message: string | null;
  startedAt: number;
  /** 本次续传起点；大于 0 时界面提示「已从断点继续」。 */
  resume: number;
};

/** 一次整目录下载：成员任务通过 `group` 关联。 */
type DirectoryTask = {
  group: number;
  name: string;
  path: string;
  /** 展开得到的文件总数（空目录不算）。 */
  files: number;
  emptyDirs: number;
  empty: boolean;
  /** 展开时统计的总字节数，用于总进度。 */
  totalBytes: number;
  startedAt: number;
  /** 分组收尾事件已到达（没有文件时也会收到）。 */
  finished: boolean;
};

type DownloadTone = "blue" | "mint" | "red" | "neutral" | "amber";

type TransferRecord = {
  id: number;
  direction: "upload" | "download";
  name: string;
  size: number;
  location: string;
  time: number;
  ok: boolean;
  message?: string;
};

type RateMeter = { lastDone: number; lastAt: number; rate: number };

type UploadRun = { tid: number; cancelled: boolean; failure: string | null };

const UPLOAD_STATUS_LABEL: Record<UploadStatus, string> = {
  sending: "传输中",
  waiting: "等待确认",
  done: "已完成",
  cancelled: "已取消",
  error: "失败",
};

const DOWNLOAD_STATUS_LABEL: Record<DownloadStatus, string> = {
  queued: "排队中",
  downloading: "下载中",
  verifying: "校验中",
  done: "已完成",
  cancelled: "已取消",
  error: "失败",
};

const DOWNLOAD_STATUS_TONE: Record<DownloadStatus, DownloadTone> = {
  queued: "neutral",
  downloading: "blue",
  verifying: "amber",
  done: "mint",
  cancelled: "neutral",
  error: "red",
};

function isDownloadActive(status: DownloadStatus): boolean {
  return status === "queued" || status === "downloading" || status === "verifying";
}

function basename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

function normalizePath(path: string): string {
  return path.replace(/\\/g, "/");
}

function joinRemote(root: string, relative: string): string {
  if (!root) return relative;
  const sep = root.includes("\\") ? "\\" : "/";
  const base = root.replace(/[\\/]+$/, "");
  const parts = relative.split(/[\\/]+/).filter(Boolean);
  return [base, ...parts].join(sep) || base;
}

/** 返回 done 字节落在哪一个文件上；done 达到总量时返回 items.length。 */
function indexAtDone(items: { size: number }[], done: number): number {
  let acc = 0;
  for (let index = 0; index < items.length; index += 1) {
    acc += items[index].size;
    if (done < acc) return index;
  }
  return items.length;
}

type Crumb = { name: string; path: string };

function buildCrumbs(path: string): Crumb[] {
  if (!path) return [];
  const sep = path.includes("\\") ? "\\" : "/";
  const crumbs: Crumb[] = [];
  const drive = /^([A-Za-z]:)[\\/]?/.exec(path);

  if (drive) {
    crumbs.push({ name: drive[1], path: drive[1] + sep });
    let acc = drive[1] + sep;
    for (const part of path.slice(drive[0].length).split(/[\\/]+/).filter(Boolean)) {
      acc += part;
      crumbs.push({ name: part, path: acc });
      acc += sep;
    }
    return crumbs;
  }

  if (path.startsWith("/")) {
    crumbs.push({ name: "/", path: "/" });
    let acc = "";
    for (const part of path.split("/").filter(Boolean)) {
      acc += `/${part}`;
      crumbs.push({ name: part, path: acc });
    }
    return crumbs;
  }

  let acc = "";
  for (const part of path.split(/[\\/]+/).filter(Boolean)) {
    acc = acc ? `${acc}${sep}${part}` : part;
    crumbs.push({ name: part, path: acc });
  }
  return crumbs;
}

function pad2(value: number): string {
  return String(value).padStart(2, "0");
}

function formatClock(timestamp: number): string {
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return "—";
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

function formatModified(seconds: number | null | undefined): string {
  if (!seconds) return "—";
  const date = new Date(seconds * 1000);
  if (Number.isNaN(date.getTime())) return "—";
  return `${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(
    date.getHours(),
  )}:${pad2(date.getMinutes())}`;
}

function speedLabel(speed: number, active: boolean): string {
  if (!active) return "—";
  if (speed <= 0) return "计算中…";
  return `${formatBytes(speed)}/s`;
}

function progressPercent(done: number, total: number, finished: boolean): number {
  if (total <= 0) return finished ? 100 : 0;
  return Math.max(0, Math.min(100, Math.round((done / total) * 100)));
}

function resetMeter(meter: { current: RateMeter }): void {
  meter.current = { lastDone: 0, lastAt: performance.now(), rate: 0 };
}

function tickRate(meter: { current: RateMeter }, done: number): number {
  const now = performance.now();
  const state = meter.current;
  const elapsed = now - state.lastAt;
  if (elapsed < 300) return state.rate;
  const instant = (Math.max(0, done - state.lastDone) * 1000) / elapsed;
  state.rate = state.rate > 0 ? state.rate * 0.6 + instant * 0.4 : instant;
  state.lastDone = done;
  state.lastAt = now;
  return state.rate;
}

/** 读取一个文件切片并转成 base64（不带 data URL 前缀）。 */
function readChunkBase64(file: File, offset: number, length: number): Promise<string> {
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

/** 浏览器侧流式算 SHA256：WebView 拿不到本地绝对路径，只能读 File 内容。 */
async function hashLocalFile(file: File): Promise<string> {
  const hasher = sha256.create();
  const HASH_SLICE = 4 * 1024 * 1024;
  for (let offset = 0; offset < file.size; offset += HASH_SLICE) {
    const buffer = await file.slice(offset, offset + HASH_SLICE).arrayBuffer();
    hasher.update(new Uint8Array(buffer));
  }
  return hasher.hex();
}

function relativePath(file: File): string {
  const withPath = file as File & { webkitRelativePath?: string };
  return withPath.webkitRelativePath || file.name;
}

/** 从本页直接发起会话时用的令牌，和控制页的计数器错开。 */
let transferTokenSeq = 100_000;

export default function FilesPage({
  devices,
  initialDeviceId,
  notify,
  onOpenWall,
}: FilesPageProps) {
  /* ---------------------------------------------------------------- 会话 */

  const [controlInfo, setControlInfo] = useState<SessionInfo | null>(null);
  const [checkingSession, setCheckingSession] = useState(true);
  const [connecting, setConnecting] = useState(false);
  // 本页自己发起的会话在离开时必须由本页断开。
  const ownTokenRef = useRef<number | null>(null);

  useEffect(() => {
    let alive = true;

    const refresh = () => {
      getControlInfo()
        .then((info) => {
          if (!alive) return;
          setControlInfo(
            info ? { deviceId: info.deviceId, session: info.session } : null,
          );
          setCheckingSession(false);
        })
        .catch(() => {
          if (!alive) return;
          setControlInfo(null);
          setCheckingSession(false);
        });
    };

    refresh();
    const off = subscribeControlState((state) => {
      if (!alive) return;
      if (state.phase === "closed" || state.phase === "error") {
        // 控制页会在断线后自动重连（会话号会变），这里先清空再拉一次，
        // 避免文件页拿着已经失效的会话号继续发命令。
        setControlInfo(null);
        setCheckingSession(false);
        window.setTimeout(() => {
          if (alive) refresh();
        }, 1200);
      } else {
        refresh();
      }
    });

    // 重连成功后会话号会变，立刻刷新，避免文件操作打到旧会话上。
    const offReconnected = subscribeControlReconnected(() => {
      if (alive) refresh();
    });

    return () => {
      alive = false;
      off();
      offReconnected();
    };
  }, []);

  // 离开本页时断开由本页发起的会话；控制页留下的会话不归这里管。
  useEffect(
    () => () => {
      const token = ownTokenRef.current;
      ownTokenRef.current = null;
      if (token != null) void stopControl(token).catch(() => {});
    },
    [],
  );

  /** 从本页直接连一台设备（被控端会弹授权），连上后就能传文件。 */
  async function connectForTransfer() {
    const device = devices.find((item) => item.id === initialDeviceId);
    if (!device || connecting) return;
    if (!device.online) {
      notify(`「${device.name}」当前离线，无法传输文件`, "error");
      return;
    }
    const token = ++transferTokenSeq;
    ownTokenRef.current = token;
    setConnecting(true);
    try {
      await startControl(device.id, token);
      const info = await getControlInfo();
      if (info) {
        setControlInfo({ deviceId: info.deviceId, session: info.session });
        notify(`已连接 ${device.name}`, "success");
      } else {
        notify("会话已建立，但读取会话信息失败", "error");
      }
    } catch (error) {
      ownTokenRef.current = null;
      notify(`连接失败：${errorLine(error)}`, "error");
    } finally {
      setConnecting(false);
    }
  }

  const sessionKey = controlInfo
    ? `${controlInfo.deviceId}:${controlInfo.session}`
    : null;
  const sessionDevice = controlInfo
    ? devices.find((device) => device.id === controlInfo.deviceId) ?? null
    : null;
  // 直连设备的 ID 是合成值（`direct:ip:port`），不在设备列表里，给它一个可读名字。
  const sessionName =
    sessionDevice?.name ??
    (controlInfo && isDirectDeviceId(controlInfo.deviceId)
      ? "直连设备"
      : controlInfo?.deviceId) ??
    "远程设备";
  const targetDevice = devices.find((device) => device.id === initialDeviceId) ?? null;

  /* -------------------------------------------------------------- 目录浏览 */

  const [remoteListing, setRemoteListing] = useState<RemoteListing | null>(null);
  const [remotePath, setRemotePath] = useState("");
  const [remoteLoading, setRemoteLoading] = useState(false);
  const [remoteError, setRemoteError] = useState<string | null>(null);
  const remoteSeq = useRef(0);

  const loadRemote = useCallback(
    async (path: string) => {
      const seq = remoteSeq.current + 1;
      remoteSeq.current = seq;
      setRemoteLoading(true);
      setRemoteError(null);
      try {
        const listing = await listRemoteFiles(path);
        if (seq !== remoteSeq.current) return;
        setRemoteListing(listing);
        setRemotePath(listing.path || path);
        setRemoteLoading(false);
      } catch (error) {
        if (seq !== remoteSeq.current) return;
        const message = errorLine(error);
        setRemoteError(message);
        setRemoteLoading(false);
        notify(`读取被控端目录失败：${message}`, "error");
      }
    },
    [notify],
  );

  const refreshRemote = () => {
    if (!controlInfo) return;
    void loadRemote(remotePath);
  };

  const openRemote = (path: string) => {
    if (!controlInfo) return;
    void loadRemote(path);
  };

  /* -------------------------------------------------------------- 上传状态 */

  const [uploadTask, setUploadTaskState] = useState<UploadTask | null>(null);
  const [uploadStarting, setUploadStarting] = useState(false);
  const uploadTaskRef = useRef<UploadTask | null>(null);
  const uploadStartingRef = useRef(false);
  const uploadRunRef = useRef<UploadRun | null>(null);
  const uploadRecorded = useRef<Set<number>>(new Set());
  const uploadMeter = useRef<RateMeter>({
    lastDone: 0,
    lastAt: 0,
    rate: 0,
  });

  /* -------------------------------------------------------------- 下载状态 */

  // 所有下载任务按 tid 索引，后端最多同时跑 3 个，其余显示为排队中。
  const [downloadTasks, setDownloadTasksState] = useState<DownloadTask[]>([]);
  const downloadTasksRef = useRef<Map<number, DownloadTask>>(new Map());
  const [directoryTasks, setDirectoryTasksState] = useState<DirectoryTask[]>([]);
  const directoryTasksRef = useRef<Map<number, DirectoryTask>>(new Map());
  // 每个分组只汇总提示一次。
  const directoryNotified = useRef<Set<number>>(new Set());
  const [downloadStarting, setDownloadStarting] = useState(false);
  const downloadStartingRef = useRef(false);
  // 每个任务各自算速度，避免并发任务互相干扰。
  const downloadMeters = useRef<Map<number, RateMeter>>(new Map());

  /* ---------------------------------------------------------------- 记录 */

  const [history, setHistory] = useState<TransferRecord[]>([]);
  const historySeq = useRef(0);

  const addRecord = useCallback((record: Omit<TransferRecord, "id" | "time">) => {
    const entry: TransferRecord = {
      ...record,
      id: historySeq.current + 1,
      time: Date.now(),
    };
    historySeq.current = entry.id;
    setHistory((current) => [entry, ...current]);
  }, []);

  /* ---------------------------------------------------------------- 工具 */

  const commitUpload = (next: UploadTask | null) => {
    uploadTaskRef.current = next;
    setUploadTaskState(next);
  };

  const updateUpload = (updater: (task: UploadTask) => Partial<UploadTask>) => {
    const current = uploadTaskRef.current;
    if (!current) return;
    const next = { ...current, ...updater(current) };
    uploadTaskRef.current = next;
    setUploadTaskState(next);
  };

  const publishDownloads = () => {
    const tasks = Array.from(downloadTasksRef.current.values());
    // 新任务在前，方便看到刚加入的排队任务。
    tasks.sort((left, right) => right.startedAt - left.startedAt);
    setDownloadTasksState(tasks);
  };

  const commitDownload = (task: DownloadTask) => {
    downloadTasksRef.current.set(task.tid, task);
    downloadMeters.current.set(task.tid, {
      lastDone: task.done,
      lastAt: performance.now(),
      rate: 0,
    });
    publishDownloads();
  };

  const updateDownload = (
    tid: number,
    updater: (task: DownloadTask) => Partial<DownloadTask>,
  ) => {
    const current = downloadTasksRef.current.get(tid);
    if (!current) return;
    downloadTasksRef.current.set(tid, { ...current, ...updater(current) });
    publishDownloads();
  };

  const removeDownload = (tid: number) => {
    downloadTasksRef.current.delete(tid);
    downloadMeters.current.delete(tid);
    publishDownloads();
  };

  const publishDirectories = () => {
    const groups = Array.from(directoryTasksRef.current.values());
    groups.sort((left, right) => right.startedAt - left.startedAt);
    setDirectoryTasksState(groups);
  };

  const commitDirectory = (task: DirectoryTask) => {
    directoryTasksRef.current.set(task.group, task);
    publishDirectories();
  };

  const updateDirectory = (
    group: number,
    updater: (task: DirectoryTask) => Partial<DirectoryTask>,
  ) => {
    const current = directoryTasksRef.current.get(group);
    if (!current) return;
    directoryTasksRef.current.set(group, { ...current, ...updater(current) });
    publishDirectories();
  };

  const finishUpload = (tid: number) => {
    const task = uploadTaskRef.current;
    if (!task || task.tid !== tid) return;
    if (
      task.status === "done" ||
      task.status === "cancelled" ||
      task.status === "error"
    ) {
      return;
    }
    updateUpload((current) => ({
      status: "done",
      done: current.total,
      currentIndex: current.items.length,
      completed: Math.max(current.completed, current.items.length),
      speed: 0,
      message: null,
    }));
    uploadRunRef.current = null;
    notify(`上传完成，共 ${task.items.length} 个文件`, "success");
  };

  const finishDownload = (data: {
    tid: number;
    name: string;
    path: string;
    size: number;
    done?: number;
  }) => {
    const task = downloadTasksRef.current.get(data.tid);
    if (!task) return;
    if (!isDownloadActive(task.status)) return;
    const size = data.size > 0 ? data.size : task.total;
    const name = data.name || task.name;
    const path = data.path || task.path;
    updateDownload(task.tid, () => ({
      status: "done",
      done: size,
      total: size,
      speed: 0,
      name,
      path,
    }));
    addRecord({
      direction: "download",
      name,
      size,
      location: path,
      ok: true,
    });
    // 目录下载里的单个文件不弹提示，避免一次几十条；整组完成时再汇总。
    if (!task.group) {
      notify(`已下载「${name}」到 ${path}`, "success");
    } else {
      notifyDirectoryProgress(task.group);
    }
  };

  /** 整组下载收尾时给一条汇总提示（每个分组只提示一次）。 */
  const notifyDirectoryProgress = (group: number) => {
    if (group === 0 || directoryNotified.current.has(group)) return;
    const directory = directoryTasksRef.current.get(group);
    if (!directory) return;
    const members = Array.from(downloadTasksRef.current.values()).filter(
      (task) => task.group === group,
    );
    if (members.length === 0) return;
    if (members.some((task) => isDownloadActive(task.status))) return;
    const done = members.filter((task) => task.status === "done").length;
    const failed = members.filter((task) => task.status === "error").length;
    const cancelled = members.filter((task) => task.status === "cancelled").length;
    if (done + failed + cancelled < members.length) return;
    directoryNotified.current.add(group);
    updateDirectory(group, () => ({ finished: true }));
    if (failed > 0) {
      notify(`文件夹「${directory.name}」下载结束：${done} 个成功、${failed} 个失败`, "error");
    } else if (cancelled > 0) {
      notify(`文件夹「${directory.name}」已取消，完成 ${done}/${members.length} 个文件`, "info");
    } else {
      notify(`文件夹「${directory.name}」下载完成，共 ${done} 个文件`, "success");
    }
  };

  /* ------------------------------------------------------------ 事件处理 */

  const findUploadItemIndex = (task: UploadTask, eventPath: string): number => {
    const recorded = uploadRecorded.current;
    const target = normalizePath(eventPath);
    if (target) {
      for (let index = 0; index < task.items.length; index += 1) {
        if (recorded.has(index)) continue;
        const relative = normalizePath(task.items[index].path);
        if (target === relative || target.endsWith(`/${relative}`)) return index;
      }
    }
    for (let index = 0; index < task.items.length; index += 1) {
      if (!recorded.has(index)) return index;
    }
    return -1;
  };

  const handleFileEvent = (event: FileTransferEvent) => {
    /* 上传进度：done/total 是整个批次的字节数。 */
    if (event.kind === "progress" && event.direction === "upload") {
      const task = uploadTaskRef.current;
      if (!task || task.tid !== event.tid) return;
      if (task.status !== "sending" && task.status !== "waiting") return;
      const done = Math.max(task.done, event.done ?? task.done);
      const total = event.total ?? task.total;
      const speed = tickRate(uploadMeter, done);
      updateUpload((current) => ({
        done,
        total,
        currentIndex: indexAtDone(current.items, done),
        speed,
      }));
      return;
    }

    /* 单个文件在被控端落盘的结果。 */
    if (event.kind === "file-done" && event.direction === "upload") {
      const task = uploadTaskRef.current;
      if (!task || task.tid !== event.tid) return;
      const ok = event.ok !== false;
      const eventPath = event.path ?? "";
      const index = findUploadItemIndex(task, eventPath);
      if (index >= 0) uploadRecorded.current.add(index);
      const item = index >= 0 ? task.items[index] : undefined;
      const name = item?.path ?? (eventPath ? basename(eventPath) : "文件");
      const location = eventPath || (item ? joinRemote(task.root, item.path) : task.root);
      addRecord({
        direction: "upload",
        name,
        size: item?.size ?? 0,
        location,
        ok,
        message: event.msg ?? undefined,
      });

      if (!ok) {
        const message = event.msg ?? "被控端保存文件失败";
        const run = uploadRunRef.current;
        if (run && run.tid === task.tid) run.failure = message;
        updateUpload(() => ({
          status: "error",
          message,
          speed: 0,
        }));
        notify(`「${basename(name)}」上传失败：${message}`, "error");
        cancelFileTransfer(task.tid).catch(() => {});
        return;
      }

      const completed = task.completed + 1;
      updateUpload(() => ({ completed }));
      if (completed >= task.items.length) {
        finishUpload(task.tid);
      }
      return;
    }

    /* 整批上传结束。 */
    if (event.kind === "complete" && event.direction === "upload") {
      const task = uploadTaskRef.current;
      if (!task || task.tid !== event.tid) return;
      if (task.status === "done" || task.status === "cancelled" || task.status === "error") {
        return;
      }
      const total = event.total ?? task.total;
      updateUpload((current) => ({
        total,
        done: total,
        currentIndex: current.items.length,
      }));
      finishUpload(task.tid);
      return;
    }

    /* 整目录下载：展开完成，先登记整组任务，再接收逐个文件的 queued 事件。 */
    if (event.kind === "directory" && event.direction === "download") {
      const group = event.group ?? 0;
      if (group === 0) return;
      const existing = directoryTasksRef.current.get(group);
      if (existing) {
        updateDirectory(group, () => ({
          files: event.files ?? existing.files,
          emptyDirs: event.emptyDirs ?? existing.emptyDirs,
          totalBytes: event.total ?? existing.totalBytes,
          path: event.path ?? existing.path,
        }));
      } else {
        commitDirectory({
          group,
          name: event.name ?? "文件夹",
          path: event.path ?? "",
          files: event.files ?? 0,
          emptyDirs: event.emptyDirs ?? 0,
          empty: event.empty === true,
          totalBytes: event.total ?? 0,
          startedAt: Date.now(),
          finished: false,
        });
        notify(
          event.empty
            ? `「${event.name ?? "文件夹"}」是空文件夹`
            : `开始下载文件夹「${event.name ?? "文件夹"}」，共 ${event.files ?? 0} 个文件`,
          "info",
        );
      }
      return;
    }

    /* 单个下载任务入队（目录展开时一次性收到，单独下载时也走这里）。 */
    if (event.kind === "queued" && event.direction === "download") {
      if (downloadTasksRef.current.has(event.tid)) return;
      const group = event.group ?? 0;
      const size = event.total ?? event.size ?? 0;
      commitDownload({
        tid: event.tid,
        group,
        name: event.name ?? "下载文件",
        path: "",
        total: size,
        done: 0,
        status: "queued",
        speed: 0,
        message: null,
        startedAt: Date.now(),
        resume: 0,
      });
      return;
    }

    /* 下载开始（被控端可能先于命令返回推送）。 */
    if (event.kind === "start" && event.direction === "download") {
      const resume =
        typeof event.resume === "number"
          ? Math.max(0, event.resume)
          : Math.max(0, event.done ?? 0);
      const current = downloadTasksRef.current.get(event.tid);
      if (current) {
        // 从排队转入下载：速度表从续传起点重新开始。
        downloadMeters.current.set(event.tid, {
          lastDone: Math.max(resume, event.done ?? 0),
          lastAt: performance.now(),
          rate: 0,
        });
        updateDownload(event.tid, () => ({
          name: event.name ?? current.name,
          path: event.path ?? current.path,
          total: event.total ?? current.total,
          done: Math.max(resume, event.done ?? 0),
          resume,
          status: "downloading",
        }));
        return;
      }
      const task: DownloadTask = {
        tid: event.tid,
        group: event.group ?? 0,
        name: event.name ?? "下载文件",
        path: event.path ?? "",
        total: event.total ?? 0,
        done: resume,
        resume,
        status: "downloading",
        speed: 0,
        message: null,
        startedAt: Date.now(),
      };
      commitDownload(task);
      notify(
        resume > 0
          ? `已从断点继续下载「${task.name}」（${formatBytes(resume)} 无需重传）`
          : `开始下载「${task.name}」`,
        "info",
      );
      return;
    }

    /* 下载进度。 */
    if (event.kind === "progress" && event.direction === "download") {
      const task = downloadTasksRef.current.get(event.tid);
      if (!task) return;
      if (task.status !== "downloading") return;
      const done = Math.max(task.done, event.done ?? task.done);
      const total = event.total ?? task.total;
      const meter = downloadMeters.current.get(event.tid);
      const speed = meter ? tickRate({ current: meter }, done) : 0;
      updateDownload(event.tid, () => ({
        done,
        total,
        speed,
        name: event.name ?? task.name,
      }));
      return;
    }

    /* 数据收齐，进入 SHA256 校验与落盘。 */
    if (event.kind === "verifying" && event.direction === "download") {
      const task = downloadTasksRef.current.get(event.tid);
      if (!task || task.status !== "downloading") return;
      updateDownload(event.tid, () => ({
        status: "verifying",
        done: Math.max(task.done, event.done ?? task.total),
        total: event.total ?? task.total,
        speed: 0,
      }));
      return;
    }

    /* 下载结束（校验通过、改名完成）。 */
    if (event.kind === "complete" && event.direction === "download") {
      // 没有文件的目录下载也会走这里（tid = 0），只收尾整组任务。
      if (event.directory) {
        const group = event.group ?? 0;
        if (group !== 0) updateDirectory(group, () => ({ finished: true }));
        return;
      }
      const task = downloadTasksRef.current.get(event.tid);
      if (!task) return;
      finishDownload({
        tid: event.tid,
        name: event.name ?? task.name,
        path: event.path ?? task.path,
        size: event.total ?? task.total,
        done: event.done,
      });
      if (event.verified) {
        notify(`「${event.name ?? task.name}」下载完成，SHA256 校验通过`, "success");
      }
      return;
    }

    /* 取消：上传和下载共用 tid；带 msg 的是对端报告失败而非用户取消。 */
    if (event.kind === "cancelled") {
      const upload = uploadTaskRef.current;
      if (
        upload &&
        upload.tid === event.tid &&
        (upload.status === "sending" || upload.status === "waiting")
      ) {
        const message = event.msg ?? "";
        const run = uploadRunRef.current;
        if (message) {
          if (run && run.tid === upload.tid) run.failure = message;
          updateUpload(() => ({ status: "error", message, speed: 0 }));
          notify(`上传中断：${message}`, "error");
        } else {
          if (run && run.tid === upload.tid) run.cancelled = true;
          updateUpload(() => ({ status: "cancelled", speed: 0, message: null }));
          uploadRunRef.current = null;
        }
      }
      const download = downloadTasksRef.current.get(event.tid);
      if (download && isDownloadActive(download.status)) {
        const message = event.msg ?? "";
        if (message) {
          updateDownload(event.tid, () => ({ status: "error", message, speed: 0 }));
          addRecord({
            direction: "download",
            name: download.name,
            size: download.total,
            location: download.path,
            ok: false,
            message,
          });
          notify(`下载「${download.name}」失败：${message}`, "error");
        } else {
          updateDownload(event.tid, () => ({
            status: "cancelled",
            speed: 0,
            message: null,
          }));
          if (!download.group) notify(`已取消下载「${download.name}」`, "info");
        }
        if (download.group) notifyDirectoryProgress(download.group);
      }
      return;
    }

    /* 错误：事件里不一定带 direction，按 tid 匹配当前任务。 */
    if (event.kind === "error") {
      const upload = uploadTaskRef.current;
      if (
        upload &&
        upload.tid === event.tid &&
        (upload.status === "sending" || upload.status === "waiting")
      ) {
        const message = event.msg ?? "上传失败";
        const run = uploadRunRef.current;
        if (run && run.tid === upload.tid) run.failure = message;
        updateUpload(() => ({
          status: "error",
          message,
          speed: 0,
        }));
        notify(`上传失败：${message}`, "error");
      }
      const download = downloadTasksRef.current.get(event.tid);
      if (download && isDownloadActive(download.status)) {
        const message = event.msg ?? "下载失败";
        updateDownload(event.tid, () => ({
          status: "error",
          message,
          speed: 0,
        }));
        addRecord({
          direction: "download",
          name: download.name,
          size: download.total,
          location: download.path,
          ok: false,
          message,
        });
        notify(`下载「${download.name}」失败：${message}`, "error");
        if (download.group) notifyDirectoryProgress(download.group);
      }
    }
  };

  useEffect(() => {
    const off = subscribeFileTransfer((event) => handleFileEvent(event));
    return off;
    // handleFileEvent 只通过 ref 读取最新任务状态，订阅一次即可。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ------------------------------------------------ 会话变化时重置页面状态 */

  useEffect(() => {
    remoteSeq.current += 1;
    setRemoteListing(null);
    setRemotePath("");
    setRemoteError(null);
    setRemoteLoading(false);

    uploadRunRef.current = null;
    uploadRecorded.current = new Set();
    uploadTaskRef.current = null;
    uploadStartingRef.current = false;
    setUploadTaskState(null);
    setUploadStarting(false);
    resetMeter(uploadMeter);

    downloadTasksRef.current = new Map();
    downloadMeters.current = new Map();
    downloadStartingRef.current = false;
    setDownloadTasksState([]);
    directoryTasksRef.current = new Map();
    directoryNotified.current = new Set();
    setDirectoryTasksState([]);
    setDownloadStarting(false);

    if (!sessionKey) return;
    void loadRemote("");
  }, [sessionKey, loadRemote]);

  /* ------------------------------------------------------------ 上传入口 */

  const [dragging, setDragging] = useState(false);
  const dragDepth = useRef(0);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const input = folderInputRef.current;
    if (!input) return;
    input.setAttribute("webkitdirectory", "");
    input.setAttribute("directory", "");
  }, []);

  const uploadBusy =
    uploadTask !== null &&
    (uploadTask.status === "sending" || uploadTask.status === "waiting");
  const uploadLocked = uploadStarting || uploadBusy;

  /** 用 ref 判断，避免命令尚未返回时界面状态还没刷新。 */
  const uploadLockedNow = () => {
    const task = uploadTaskRef.current;
    return (
      uploadStartingRef.current ||
      (task !== null && (task.status === "sending" || task.status === "waiting"))
    );
  };

  const pickFiles = () => {
    if (!controlInfo) {
      notify("请先在「屏幕墙」连接一台设备", "error");
      return;
    }
    if (uploadLockedNow()) {
      notify("正在上传，请等待当前任务结束或取消", "info");
      return;
    }
    fileInputRef.current?.click();
  };

  const pickFolder = () => {
    if (!controlInfo) {
      notify("请先在「屏幕墙」连接一台设备", "error");
      return;
    }
    if (uploadLockedNow()) {
      notify("正在上传，请等待当前任务结束或取消", "info");
      return;
    }
    folderInputRef.current?.click();
  };

  const startUpload = async (picked: File[]) => {
    if (!controlInfo) {
      notify("请先在「屏幕墙」连接一台设备", "error");
      return;
    }
    if (uploadLockedNow()) {
      notify("当前已有上传任务，请等待完成或取消", "error");
      return;
    }

    const usable = picked.filter((file) => file.size > 0);
    const skipped = picked.length - usable.length;
    if (usable.length === 0) {
      notify("所选文件都是 0 字节的空文件，无法上传", "error");
      return;
    }
    if (skipped > 0) {
      notify(`已跳过 ${skipped} 个空文件`, "info");
    }

    let items: UploadItem[] = usable.map((file) => ({
      file,
      path: relativePath(file),
      size: file.size,
      sha256: "",
    }));
    const computedTotal = items.reduce((sum, item) => sum + item.size, 0);

    uploadStartingRef.current = true;
    setUploadStarting(true);
    let offered;
    try {
      // 先算 SHA256：被控端用它校验完整性，也用它匹配上次没传完的前缀。
      notify("正在计算文件校验值…", "info");
      const hashed: UploadItem[] = [];
      for (const item of items) {
        hashed.push({ ...item, sha256: await hashLocalFile(item.file) });
      }
      items = hashed;
      offered = await offerFiles(
        items.map((item) => ({
          path: item.path,
          size: item.size,
          sha256: item.sha256,
        })),
      );
    } catch (error) {
      uploadStartingRef.current = false;
      setUploadStarting(false);
      notify(`无法开始上传：${errorLine(error)}`, "error");
      return;
    }

    const resumeOffsets = new Map<number, number>(
      offered.resume.map((entry) => [entry.item, entry.offset]),
    );
    const resumedTotal = offered.resume.reduce((sum, entry) => sum + entry.offset, 0);
    const total = offered.total || computedTotal;
    const root = isTauri ? offered.root : DEMO_SAVE_ROOT;
    const task: UploadTask = {
      tid: offered.tid,
      root,
      items,
      total,
      done: Math.min(resumedTotal, total),
      currentIndex: 0,
      completed: 0,
      status: "sending",
      speed: 0,
      message: null,
      startedAt: Date.now(),
    };
    const run: UploadRun = { tid: task.tid, cancelled: false, failure: null };
    uploadRunRef.current = run;
    uploadRecorded.current = new Set();
    resetMeter(uploadMeter);
    commitUpload(task);
    uploadStartingRef.current = false;
    setUploadStarting(false);
    notify(
      resumedTotal > 0
        ? `已从断点继续：${formatBytes(resumedTotal)} 无需重传`
        : `开始上传 ${items.length} 个文件`,
      "info",
    );

    const runActive = () => {
      const current = uploadTaskRef.current;
      return (
        !run.cancelled &&
        !run.failure &&
        current !== null &&
        current.tid === task.tid &&
        (current.status === "sending" || current.status === "waiting")
      );
    };

    let sent = Math.min(resumedTotal, total);
    try {
      for (let itemIndex = 0; itemIndex < items.length; itemIndex += 1) {
        if (!runActive()) break;
        const item = items[itemIndex];
        const startOffset = Math.min(resumeOffsets.get(itemIndex) ?? 0, item.size);
        updateUpload((current) =>
          current.tid === task.tid ? { currentIndex: itemIndex } : {},
        );
        for (let offset = startOffset; offset < item.size; offset += CHUNK_BYTES) {
          if (!runActive()) break;
          const length = Math.min(CHUNK_BYTES, item.size - offset);
          const data = await readChunkBase64(item.file, offset, length);
          if (!runActive()) break;
          await uploadChunk(task.tid, itemIndex, offset, data);
          sent += length;
          const speed = tickRate(uploadMeter, sent);
          updateUpload((current) =>
            current.tid === task.tid && current.status === "sending"
              ? {
                  done: Math.max(current.done, sent),
                  currentIndex: indexAtDone(current.items, sent),
                  speed,
                }
              : {},
          );
        }
      }
    } catch (error) {
      // 取消后仍在途的块可能报错，不能覆盖“已取消”状态。
      if (run.cancelled || run.failure) return;
      const current = uploadTaskRef.current;
      if (!current || current.tid !== task.tid) return;
      const message = errorLine(error);
      run.failure = message;
      updateUpload(() => ({ status: "error", message, speed: 0 }));
      notify(`上传失败：${message}`, "error");
      cancelFileTransfer(task.tid).catch(() => {});
      return;
    }

    if (!runActive()) return;

    updateUpload((current) =>
      current.tid === task.tid
        ? {
            status: "waiting",
            done: Math.max(current.done, total),
            currentIndex: current.items.length,
            speed: 0,
          }
        : {},
    );

    if (!isTauri) {
      // 浏览器预览没有真实事件：所有数据块都已被接受后直接完成。
      items.forEach((item, index) => {
        if (uploadRecorded.current.has(index)) return;
        uploadRecorded.current.add(index);
        addRecord({
          direction: "upload",
          name: item.path,
          size: item.size,
          location: joinRemote(root, item.path),
          ok: true,
        });
      });
      updateUpload((current) =>
        current.tid === task.tid
          ? {
              status: "done",
              done: total,
              currentIndex: current.items.length,
              completed: current.items.length,
            }
          : {},
      );
      uploadRunRef.current = null;
      notify(`上传完成，共 ${items.length} 个文件`, "success");
    }
  };

  const handlePicked = (event: ChangeEvent<HTMLInputElement>) => {
    const picked = Array.from(event.target.files ?? []);
    event.target.value = "";
    if (picked.length === 0) return;
    void startUpload(picked);
  };

  const cancelUpload = async () => {
    const task = uploadTaskRef.current;
    if (!task || (task.status !== "sending" && task.status !== "waiting")) return;
    const run = uploadRunRef.current;
    if (run && run.tid === task.tid) run.cancelled = true;
    uploadRunRef.current = null;
    updateUpload(() => ({ status: "cancelled", speed: 0 }));
    notify(`已取消上传「${task.items.length} 个文件」`, "info");
    try {
      await cancelFileTransfer(task.tid);
    } catch (error) {
      notify(`取消上传失败：${errorLine(error)}`, "error");
    }
  };

  const closeUploadCard = () => {
    if (uploadBusy) return;
    uploadTaskRef.current = null;
    setUploadTaskState(null);
  };

  /* ------------------------------------------------------------ 拖拽上传 */

  const handleDragEnter = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    if (uploadLockedNow()) return;
    dragDepth.current += 1;
    setDragging(true);
  };

  const handleDragOver = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    if (event.dataTransfer) {
      event.dataTransfer.dropEffect = uploadLockedNow() ? "none" : "copy";
    }
  };

  const handleDragLeave = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragging(false);
  };

  const handleDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    dragDepth.current = 0;
    setDragging(false);
    if (uploadLockedNow()) {
      notify("正在上传，请等待当前任务结束或取消", "info");
      return;
    }
    const dropped = Array.from(event.dataTransfer.files ?? []);
    if (dropped.length === 0) {
      notify("没有检测到可上传的文件；拖拽文件夹请使用「选择文件夹」", "error");
      return;
    }
    void startUpload(dropped);
  };

  /* ------------------------------------------------------------ 下载入口 */

  const startDownload = async (entry: RemoteEntry) => {
    if (!controlInfo) {
      notify("请先在「屏幕墙」连接一台设备", "error");
      return;
    }
    try {
      const info = await downloadRemoteFile(entry.path);
      // 浏览器预览返回的假路径不可读，直接用目录项补一条正常路径。
      const name = isTauri ? info.name : entry.name;
      const savedPath = isTauri ? info.path : joinRemote(DEMO_SAVE_ROOT, entry.name);
      const existing = downloadTasksRef.current.get(info.tid);
      if (existing) {
        // 后端可能先推送 queued/start，这里只补名称与总量。
        updateDownload(info.tid, () => ({
          name: existing.name || name,
          path: existing.path || savedPath,
          total: existing.total > 0 ? existing.total : info.size,
        }));
      } else {
        commitDownload({
          tid: info.tid,
          group: 0,
          name,
          path: savedPath,
          total: info.size,
          done: 0,
          resume: 0,
          status: "queued",
          speed: 0,
          message: null,
          startedAt: Date.now(),
        });
        notify(`已加入下载队列「${name}」`, "info");
      }

      if (!isTauri) {
        // 浏览器预览没有真实事件：命令成功即视为已保存。
        updateDownload(info.tid, () => ({ status: "downloading" }));
        finishDownload({
          tid: info.tid,
          name,
          path: savedPath,
          size: info.size,
          done: info.size,
        });
      }
    } catch (error) {
      notify(`下载失败：${errorLine(error)}`, "error");
    }
  };

  /** 下载整个远程文件夹：后端递归展开后逐个排队，保留目录结构。 */
  const startDirectoryDownload = async (entry: RemoteEntry) => {
    if (!controlInfo) {
      notify("请先在「屏幕墙」连接一台设备", "error");
      return;
    }
    if (downloadStartingRef.current) {
      notify("正在展开上一个文件夹，请稍候", "info");
      return;
    }
    downloadStartingRef.current = true;
    setDownloadStarting(true);
    try {
      const info = await downloadRemoteDirectory(entry.path);
      const savedPath = isTauri
        ? joinRemote(DOWNLOAD_DIR_LABEL, info.name)
        : joinRemote(DEMO_SAVE_ROOT, info.name);
      const existing = directoryTasksRef.current.get(info.group);
      if (existing) {
        updateDirectory(info.group, () => ({
          files: info.files,
          emptyDirs: info.emptyDirs,
          path: savedPath,
        }));
      } else {
        commitDirectory({
          group: info.group,
          name: info.name,
          path: savedPath,
          files: info.files,
          emptyDirs: info.emptyDirs,
          empty: info.empty,
          totalBytes: 0,
          startedAt: Date.now(),
          finished: info.files === 0,
        });
      }
      if (info.files === 0) {
        notify(
          info.empty
            ? `「${info.name}」是空文件夹，已创建目录`
            : `「${info.name}」没有文件，已创建 ${info.emptyDirs} 个空目录`,
          "info",
        );
      }

      if (!isTauri) {
        // 浏览器预览：造两个已完成任务，方便看队列与总进度。
        const demoPath = joinRemote(savedPath, "demo");
        for (let index = 0; index < info.files; index += 1) {
          const tid = 10_000 + index;
          commitDownload({
            tid,
            group: info.group,
            name: `${info.name}\\demo-${index + 1}.bin`,
            path: demoPath,
            total: 1024 * 1024,
            done: 0,
            resume: 0,
            status: "downloading",
            speed: 0,
            message: null,
            startedAt: Date.now() + index,
          });
          finishDownload({
            tid,
            name: `${info.name}\\demo-${index + 1}.bin`,
            path: demoPath,
            size: 1024 * 1024,
            done: 1024 * 1024,
          });
        }
      }
    } catch (error) {
      notify(`下载文件夹失败：${errorLine(error)}`, "error");
    } finally {
      downloadStartingRef.current = false;
      setDownloadStarting(false);
    }
  };

  const cancelDownload = async (task: DownloadTask) => {
    if (!isDownloadActive(task.status)) return;
    updateDownload(task.tid, () => ({ status: "cancelled", speed: 0 }));
    notify(`已取消下载「${task.name}」`, "info");
    try {
      await cancelFileTransfer(task.tid);
    } catch (error) {
      notify(`取消下载失败：${errorLine(error)}`, "error");
    }
  };

  /** 取消整组里还没结束的任务；已完成的保持原样。 */
  const cancelDirectory = async (directory: DirectoryTask) => {
    const members = downloadTasks.filter(
      (task) => task.group === directory.group && isDownloadActive(task.status),
    );
    if (members.length === 0) {
      updateDirectory(directory.group, () => ({ finished: true }));
      return;
    }
    for (const task of members) {
      updateDownload(task.tid, () => ({ status: "cancelled", speed: 0 }));
    }
    notify(`已取消文件夹「${directory.name}」剩余的 ${members.length} 个文件`, "info");
    await Promise.all(
      members.map((task) => cancelFileTransfer(task.tid).catch(() => {})),
    );
  };

  const closeDownloadCard = (task: DownloadTask) => {
    if (isDownloadActive(task.status)) return;
    removeDownload(task.tid);
  };

  const closeDirectoryCard = (directory: DirectoryTask) => {
    const busy = downloadTasks.some(
      (task) => task.group === directory.group && isDownloadActive(task.status),
    );
    if (busy) return;
    directoryTasksRef.current.delete(directory.group);
    publishDirectories();
  };

  const copyPath = async (path: string) => {
    try {
      if (!navigator.clipboard?.writeText) {
        throw new Error("当前环境不支持剪贴板");
      }
      await navigator.clipboard.writeText(path);
      notify("已复制保存路径", "success");
    } catch (error) {
      notify(`复制失败：${errorLine(error)}`, "error");
    }
  };

  /* ------------------------------------------------------------ 派生数据 */

  const currentPath = remoteListing?.path || remotePath;
  const crumbs = useMemo(() => buildCrumbs(currentPath), [currentPath]);
  const entries = useMemo(() => {
    const list = remoteListing?.entries ?? [];
    return [...list].sort((left, right) => {
      if (left.dir !== right.dir) return left.dir ? -1 : 1;
      return left.name.localeCompare(right.name, "zh-CN");
    });
  }, [remoteListing]);

  const uploadPercent = uploadTask
    ? progressPercent(
        uploadTask.done,
        uploadTask.total,
        uploadTask.status === "done" || uploadTask.status === "waiting",
      )
    : 0;

  const uploadTone =
    uploadTask?.status === "error"
      ? "red"
      : uploadTask?.status === "cancelled"
        ? "neutral"
        : uploadTask?.status === "done"
          ? "mint"
          : "blue";

  const activeDownloads = downloadTasks.filter((task) =>
    isDownloadActive(task.status),
  ).length;

  /** 目录总进度：按已完成文件数 + 字节数各给一份。 */
  const directoryProgress = useMemo(() => {
    const map = new Map<
      number,
      {
        total: number;
        done: number;
        failed: number;
        cancelled: number;
        doneBytes: number;
        totalBytes: number;
        active: number;
      }
    >();
    for (const task of downloadTasks) {
      if (!task.group) continue;
      const entry =
        map.get(task.group) ??
        {
          total: 0,
          done: 0,
          failed: 0,
          cancelled: 0,
          doneBytes: 0,
          totalBytes: 0,
          active: 0,
        };
      entry.total += 1;
      entry.totalBytes += task.total;
      if (task.status === "done") {
        entry.done += 1;
        entry.doneBytes += task.total > 0 ? task.total : task.done;
      } else if (task.status === "error") {
        entry.failed += 1;
      } else if (task.status === "cancelled") {
        entry.cancelled += 1;
      } else {
        entry.active += 1;
        entry.doneBytes += task.done;
      }
      map.set(task.group, entry);
    }
    return map;
  }, [downloadTasks]);

  const uploadCurrent =
    uploadTask && uploadTask.currentIndex < uploadTask.items.length
      ? uploadTask.items[uploadTask.currentIndex]
      : null;
  const uploadTitle = uploadTask
    ? uploadTask.status === "done"
      ? `${uploadTask.items.length} 个文件上传完成`
      : uploadTask.status === "cancelled"
        ? "上传已取消"
        : uploadTask.status === "error"
          ? "上传失败"
          : uploadTask.status === "waiting"
            ? "正在等待被控端确认…"
            : uploadCurrent
              ? basename(uploadCurrent.path)
              : "上传中"
    : "";

  /* ---------------------------------------------------------------- 渲染 */

  const historyPanel = (
    <section className="panel files-history" aria-label="传输记录">
      <header className="files-history-head">
        <div>
          <h2>传输记录</h2>
          <p>本次会话完成的上传与下载</p>
        </div>
        <span className="badge badge-muted">{history.length} 条</span>
      </header>

      {history.length === 0 ? (
        <EmptyState
          icon={<Inbox size={20} strokeWidth={1.75} />}
          title="还没有传输记录"
          hint="上传或下载完成后，文件名、大小、保存位置和时间会显示在这里。"
        />
      ) : (
        <div className="files-history-scroll">
          <div className="files-history-table" role="table" aria-label="传输记录">
            <div className="files-history-row is-head" role="row">
              <span role="columnheader">方向</span>
              <span role="columnheader">文件</span>
              <span role="columnheader">大小</span>
              <span role="columnheader">保存位置</span>
              <span role="columnheader">时间</span>
              <span role="columnheader">状态</span>
            </div>
            {history.map((record) => (
              <div
                className={`files-history-row ${record.ok ? "" : "is-failed"}`}
                role="row"
                key={record.id}
              >
                <span role="cell">
                  <span
                    className={`badge ${
                      record.direction === "upload" ? "badge-blue" : "badge-muted"
                    }`}
                  >
                    {record.direction === "upload" ? (
                      <ArrowUpRight size={12} strokeWidth={1.75} aria-hidden="true" />
                    ) : (
                      <ArrowDownLeft size={12} strokeWidth={1.75} aria-hidden="true" />
                    )}
                    {record.direction === "upload" ? "上传" : "下载"}
                  </span>
                </span>
                <span className="files-history-name" role="cell" title={record.name}>
                  <span>{record.name}</span>
                  {!record.ok && record.message && <small>{record.message}</small>}
                </span>
                <span className="files-cell-muted mono" role="cell">
                  {formatBytes(record.size)}
                </span>
                <span className="files-history-location mono" role="cell" title={record.location}>
                  {record.location || "—"}
                </span>
                <span className="files-cell-muted mono" role="cell">
                  {formatClock(record.time)}
                </span>
                <span className="files-cell-status" role="cell">
                  <span className={`badge ${record.ok ? "" : "badge-red"}`}>
                    {record.ok ? "完成" : "失败"}
                  </span>
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
    </section>
  );

  if (checkingSession) {
    return (
      <div className="files-page">
        <div className="panel files-session-check">
          <Loader2 size={17} className="spin" aria-hidden="true" />
          <span>正在检查远程会话…</span>
        </div>
      </div>
    );
  }

  if (!controlInfo) {
    return (
      <div className="files-page">
        <section className="panel files-session-empty">
          <EmptyState
            icon={<Monitor size={21} strokeWidth={1.75} />}
            title={
              !targetDevice
                ? "还没有可用的远程会话"
                : targetDevice.online
                  ? `连接「${targetDevice.name}」后传输文件`
                  : `「${targetDevice.name}」当前离线`
            }
            hint={
              !targetDevice
                ? "文件传输通过当前远程控制会话进行。请先在「屏幕墙」连接一台设备，再回到这里浏览被控端文件、上传或下载文件。"
                : targetDevice.online
                  ? "文件传输走远程控制会话。点「连接并传输」，在被控端确认授权后就能浏览、上传和下载文件。"
                  : "设备上线后才能建立传输会话。可以稍后再来，或在屏幕墙查看其他在线设备。"
            }
            action={
              <div className="files-empty-actions">
                {targetDevice && (
                  <button
                    type="button"
                    className="btn btn-primary"
                    onClick={() => void connectForTransfer()}
                    disabled={connecting || !targetDevice.online}
                  >
                    {connecting ? (
                      <Loader2 size={15} className="spin" aria-hidden="true" />
                    ) : (
                      <Monitor size={15} strokeWidth={1.75} />
                    )}
                    {connecting ? "正在等待被控端确认…" : "连接并传输"}
                  </button>
                )}
                <button type="button" className="btn btn-ghost" onClick={onOpenWall}>
                  前往屏幕墙
                </button>
              </div>
            }
          />
        </section>
        {historyPanel}
      </div>
    );
  }

  return (
    <div className="files-page">
      <header className="files-toolbar">
        <div className="files-toolbar-group">
          <span className="files-session-pill">
            <span className="files-dot" aria-hidden="true" />
            <Monitor size={15} strokeWidth={1.75} aria-hidden="true" />
            <span className="files-session-copy">
              <strong title={sessionName}>{sessionName}</strong>
              <small>已连接 · 会话 #{controlInfo.session}</small>
            </span>
          </span>
          <span className="files-toolbar-meta">文件传输使用当前远程控制会话</span>
        </div>
        <div className="files-toolbar-group">
          <button
            type="button"
            className="btn btn-ghost"
            onClick={onOpenWall}
            aria-label="前往屏幕墙"
          >
            <Monitor size={15} strokeWidth={1.75} />
            屏幕墙
          </button>
        </div>
      </header>

      <div className="files-split panel">
        <section className="files-browser" aria-label="被控端文件">
          <header className="files-browser-head">
            <div>
              <h2>被控端文件</h2>
              <p>浏览 {sessionName} 的目录，选择文件下载到本机</p>
            </div>
            <span className="badge badge-muted">{entries.length} 项</span>
          </header>

          <div className="files-browser-nav">
            <div className="files-breadcrumbs" aria-label="当前目录">
              <button
                type="button"
                className="files-crumb"
                onClick={() => openRemote("")}
                title="用户主目录"
                aria-label="回到用户主目录"
              >
                <Home size={14} strokeWidth={1.75} aria-hidden="true" />
              </button>
              {crumbs.map((crumb, index) => (
                <span className="files-crumb-group" key={crumb.path}>
                  <ChevronRight
                    size={13}
                    strokeWidth={1.75}
                    className="files-crumb-sep"
                    aria-hidden="true"
                  />
                  <button
                    type="button"
                    className={`files-crumb ${
                      index === crumbs.length - 1 ? "is-current" : ""
                    }`}
                    onClick={() => openRemote(crumb.path)}
                    title={crumb.path}
                    aria-current={index === crumbs.length - 1 ? "page" : undefined}
                  >
                    {crumb.name}
                  </button>
                </span>
              ))}
            </div>
            <div className="files-browser-actions">
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={() => openRemote(remoteListing?.parent ?? "")}
                disabled={!remoteListing?.parent}
                aria-label="上一级目录"
              >
                <ArrowUp size={14} strokeWidth={1.75} />
                上一级
              </button>
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={refreshRemote}
                disabled={remoteLoading}
                aria-label="刷新被控端目录"
              >
                <RefreshCw
                  size={14}
                  strokeWidth={1.75}
                  className={remoteLoading ? "spin" : undefined}
                />
                刷新
              </button>
            </div>
          </div>

          <div className="files-path-line mono" title={currentPath || "用户主目录"}>
            {currentPath || "用户主目录"}
          </div>

          {remoteLoading && !remoteListing ? (
            <div className="files-loading">
              <Loader2 size={16} className="spin" aria-hidden="true" />
              正在读取被控端目录…
            </div>
          ) : remoteError && !remoteListing ? (
            <div className="files-browser-error">
              <div className="notice notice-error">{remoteError}</div>
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={refreshRemote}
              >
                <RefreshCw size={14} strokeWidth={1.75} />
                重试
              </button>
            </div>
          ) : remoteListing && entries.length === 0 ? (
            <EmptyState
              icon={<Folder size={20} strokeWidth={1.75} />}
              title="这个文件夹是空的"
              hint="被控端的这个目录中没有可显示的文件或子文件夹。"
              action={
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={refreshRemote}
                >
                  <RefreshCw size={14} strokeWidth={1.75} />
                  刷新
                </button>
              }
            />
          ) : remoteListing ? (
            <div className="files-table-scroll" aria-busy={remoteLoading}>
              <div className="files-table" role="table" aria-label="被控端目录内容">
                <div className="files-tr is-head" role="row">
                  <span role="columnheader">名称</span>
                  <span role="columnheader">大小</span>
                  <span role="columnheader" className="files-col-modified">
                    修改时间
                  </span>
                  <span role="columnheader" className="files-right">
                    操作
                  </span>
                </div>
                {entries.map((entry) => (
                  <div
                    className={`files-tr ${entry.dir ? "is-dir" : ""}`}
                    role="row"
                    key={entry.path}
                  >
                    <span className="files-name" role="cell">
                      {entry.dir ? (
                        <Folder size={15} strokeWidth={1.75} aria-hidden="true" />
                      ) : (
                        <FileText size={15} strokeWidth={1.75} aria-hidden="true" />
                      )}
                      {entry.dir ? (
                        <button
                          type="button"
                          className="files-entry-button"
                          onClick={() => openRemote(entry.path)}
                          title={`打开 ${entry.name}`}
                          aria-label={`打开文件夹 ${entry.name}`}
                        >
                          <span className="files-name-text">{entry.name}</span>
                          <ChevronRight
                            size={13}
                            strokeWidth={1.75}
                            aria-hidden="true"
                          />
                        </button>
                      ) : (
                        <span className="files-name-text" title={entry.name}>
                          {entry.name}
                        </span>
                      )}
                    </span>
                    <span className="files-cell-muted mono" role="cell">
                      {entry.dir ? "—" : formatBytes(entry.size)}
                    </span>
                    <span
                      className="files-cell-muted files-col-modified mono"
                      role="cell"
                    >
                      {formatModified(entry.modified)}
                    </span>
                    <span className="files-row-actions" role="cell">
                      {entry.dir ? (
                        <>
                          <button
                            type="button"
                            className="files-row-action"
                            onClick={() => void startDirectoryDownload(entry)}
                            disabled={downloadStarting}
                            title={downloadStarting ? "正在展开文件夹" : "下载整个文件夹"}
                            aria-label={`下载整个文件夹 ${entry.name}`}
                          >
                            <FolderDown size={14} strokeWidth={1.75} />
                          </button>
                          <button
                            type="button"
                            className="files-row-action"
                            onClick={() => openRemote(entry.path)}
                            title="打开文件夹"
                            aria-label={`打开文件夹 ${entry.name}`}
                          >
                            <ChevronRight size={14} strokeWidth={1.75} />
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          className="files-row-action"
                          onClick={() => void startDownload(entry)}
                          title="下载到本机"
                          aria-label={`下载 ${entry.name}`}
                        >
                          <ArrowDownToLine size={14} strokeWidth={1.75} />
                        </button>
                      )}
                    </span>
                  </div>
                ))}
              </div>
            </div>
          ) : null}
        </section>

        <aside className="files-upload" aria-label="上传文件">
          <header className="files-target-head">
            <div>
              <h2>上传到被控端</h2>
              <p>文件将发送到 {sessionName}</p>
            </div>
            {uploadLocked && (
              <span className="badge badge-blue">
                {uploadStarting ? "准备中" : "传输中"}
              </span>
            )}
            {downloadStarting && !uploadLocked && (
              <span className="badge badge-amber">展开文件夹中</span>
            )}
            {activeDownloads > 0 && !uploadLocked && (
              <span className="badge badge-blue">下载中 {activeDownloads}</span>
            )}
          </header>

          <input
            ref={fileInputRef}
            className="files-hidden-input"
            type="file"
            multiple
            tabIndex={-1}
            aria-hidden="true"
            onChange={handlePicked}
          />
          <input
            ref={folderInputRef}
            className="files-hidden-input"
            type="file"
            multiple
            tabIndex={-1}
            aria-hidden="true"
            onChange={handlePicked}
          />

          <div
            className={`files-drop ${dragging ? "is-dragging" : ""} ${
              uploadLocked ? "is-disabled" : ""
            }`}
            role="button"
            tabIndex={uploadLocked ? -1 : 0}
            aria-disabled={uploadLocked}
            aria-label="拖拽文件到此处，或按回车选择上传文件"
            onClick={pickFiles}
            onKeyDown={(event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                if (!uploadLocked) pickFiles();
              }
            }}
            onDragEnter={handleDragEnter}
            onDragOver={handleDragOver}
            onDragLeave={handleDragLeave}
            onDrop={handleDrop}
          >
            <UploadCloud size={21} strokeWidth={1.75} aria-hidden="true" />
            <span className="files-drop-copy">
              <strong>
                {uploadStarting
                  ? "正在准备上传任务…"
                  : uploadBusy
                    ? "正在上传，暂不能添加文件"
                    : "拖拽文件到这里"}
              </strong>
              <small>
                {uploadLocked
                  ? "完成后可继续发送下一批文件"
                  : "或点击此处选择文件；文件夹请使用下方按钮"}
              </small>
            </span>
          </div>

          <div className="files-pick-row">
            <button
              type="button"
              className="btn btn-primary"
              onClick={pickFiles}
              disabled={uploadLocked}
              aria-label="选择要上传的文件"
            >
              <Upload size={15} strokeWidth={1.75} />
              选择文件
            </button>
            <button
              type="button"
              className="btn btn-ghost"
              onClick={pickFolder}
              disabled={uploadLocked}
              aria-label="选择要上传的文件夹"
            >
              <FolderUp size={15} strokeWidth={1.75} />
              选择文件夹
            </button>
          </div>

          {uploadTask && (
            <div className="files-transfer-card is-active" aria-live="polite">
              <div className="files-transfer-head">
                <span className="files-task-dir is-up" aria-hidden="true">
                  <ArrowUpRight size={15} strokeWidth={1.75} />
                </span>
                <div className="files-transfer-copy">
                  <strong title={uploadCurrent?.path ?? uploadTask.root}>
                    {uploadTitle}
                  </strong>
                  <small>
                    {UPLOAD_STATUS_LABEL[uploadTask.status]} · 第{" "}
                    {Math.min(uploadTask.currentIndex + 1, uploadTask.items.length)}/
                    {uploadTask.items.length} 个 · 保存到{" "}
                    <span className="files-inline-path mono" title={uploadTask.root}>
                      {uploadTask.root}
                    </span>
                  </small>
                </div>
                {uploadBusy ? (
                  <button
                    type="button"
                    className="btn btn-ghost btn-sm"
                    onClick={() => void cancelUpload()}
                    aria-label="取消当前上传"
                  >
                    <X size={14} strokeWidth={1.75} />
                    取消
                  </button>
                ) : (
                  <button
                    type="button"
                    className="files-row-action"
                    onClick={closeUploadCard}
                    title="关闭"
                    aria-label="关闭上传任务卡片"
                  >
                    <X size={14} strokeWidth={1.75} />
                  </button>
                )}
              </div>

              <div className="files-transfer-progress">
                <Progress value={uploadPercent} tone={uploadTone} />
                <span className="files-task-pct mono">{uploadPercent}%</span>
              </div>

              <div className="files-transfer-meta">
                <span className="mono">
                  {formatBytes(uploadTask.done)} / {formatBytes(uploadTask.total)}
                </span>
                <span className="mono">
                  {speedLabel(uploadTask.speed, uploadBusy)}
                </span>
                <span className="mono">
                  已落盘 {uploadTask.completed}/{uploadTask.items.length}
                </span>
              </div>

              {uploadTask.status === "error" && (
                <div className="notice notice-error">
                  {uploadTask.message ?? "上传失败"}
                </div>
              )}
            </div>
          )}

          {directoryTasks.map((directory) => {
            const stats = directoryProgress.get(directory.group);
            const totalFiles = Math.max(stats?.total ?? directory.files, directory.files);
            const doneFiles = stats?.done ?? 0;
            const finishedFiles = doneFiles + (stats?.failed ?? 0) + (stats?.cancelled ?? 0);
            const busy = (stats?.active ?? 0) > 0;
            const percent = progressPercent(
              doneFiles,
              totalFiles,
              !busy && finishedFiles >= totalFiles,
            );
            const tone: DownloadTone =
              (stats?.failed ?? 0) > 0
                ? "red"
                : busy
                  ? "blue"
                  : totalFiles > 0 && doneFiles >= totalFiles
                    ? "mint"
                    : "neutral";
            return (
              <div className="files-transfer-card is-active" aria-live="polite" key={`dir-${directory.group}`}>
                <div className="files-transfer-head">
                  <span className="files-task-dir is-down" aria-hidden="true">
                    <FolderDown size={15} strokeWidth={1.75} />
                  </span>
                  <div className="files-transfer-copy">
                    <strong title={directory.path || directory.name}>{directory.name}</strong>
                    <small>
                      {totalFiles === 0
                        ? "空文件夹"
                        : busy
                          ? `文件夹下载中 · 已完成 ${doneFiles}/${totalFiles} 个文件`
                          : `已完成 ${doneFiles}/${totalFiles} 个文件`}
                      {directory.emptyDirs > 0 && ` · ${directory.emptyDirs} 个空目录`}
                      {" · 保存到 "}
                      <span className="files-inline-path mono" title={directory.path}>
                        {directory.path || DOWNLOAD_DIR_LABEL}
                      </span>
                    </small>
                  </div>
                  {busy ? (
                    <button
                      type="button"
                      className="btn btn-ghost btn-sm"
                      onClick={() => void cancelDirectory(directory)}
                      aria-label={`取消文件夹 ${directory.name} 的下载`}
                    >
                      <X size={14} strokeWidth={1.75} />
                      取消
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="files-row-action"
                      onClick={() => closeDirectoryCard(directory)}
                      title="关闭"
                      aria-label="关闭文件夹下载卡片"
                    >
                      <X size={14} strokeWidth={1.75} />
                    </button>
                  )}
                </div>

                <div className="files-transfer-progress">
                  <Progress value={percent} tone={tone} />
                  <span className="files-task-pct mono">{percent}%</span>
                </div>

                <div className="files-transfer-meta">
                  <span className="mono">
                    {doneFiles}/{totalFiles} 个文件
                  </span>
                  <span className="mono">
                    {formatBytes(stats?.doneBytes ?? 0)} / {formatBytes(stats?.totalBytes ?? 0)}
                  </span>
                  {(stats?.failed ?? 0) > 0 && (
                    <span className="mono">{stats?.failed} 个失败</span>
                  )}
                  {directory.path && (
                    <button
                      type="button"
                      className="files-copy-button"
                      onClick={() => void copyPath(directory.path)}
                      aria-label="复制文件夹保存路径"
                    >
                      <Copy size={12} strokeWidth={1.75} />
                      复制路径
                    </button>
                  )}
                </div>
              </div>
            );
          })}

          {downloadTasks.map((task) => {
            const active = isDownloadActive(task.status);
            const percent = progressPercent(
              task.done,
              task.total,
              task.status === "done" || task.status === "verifying",
            );
            const tone = DOWNLOAD_STATUS_TONE[task.status];
            return (
              <div
                className={`files-transfer-card ${active ? "is-active" : ""}`}
                aria-live="polite"
                key={`dl-${task.tid}`}
              >
                <div className="files-transfer-head">
                  <span className="files-task-dir is-down" aria-hidden="true">
                    <ArrowDownLeft size={15} strokeWidth={1.75} />
                  </span>
                  <div className="files-transfer-copy">
                    <strong title={task.name}>{basename(task.name)}</strong>
                    <small>
                      <span className={`badge badge-${tone === "blue" ? "blue" : tone === "red" ? "red" : tone === "amber" ? "amber" : "muted"}`}>
                        {DOWNLOAD_STATUS_LABEL[task.status]}
                      </span>
                      {task.status === "queued" && " · 等待空闲下载位"}
                      {task.resume > 0 && task.status !== "done" && (
                        <> · 已从断点继续（{formatBytes(task.resume)}）</>
                      )}
                      {task.status === "cancelled" &&
                        task.done > 0 &&
                        " · 分片已保留，再次下载可续传"}
                      {" · 保存到 "}
                      <span
                        className="files-inline-path mono"
                        title={task.path || DOWNLOAD_DIR_LABEL}
                      >
                        {task.path || DOWNLOAD_DIR_LABEL}
                      </span>
                    </small>
                  </div>
                  {active ? (
                    <button
                      type="button"
                      className="btn btn-ghost btn-sm"
                      onClick={() => void cancelDownload(task)}
                      aria-label={`取消下载 ${basename(task.name)}`}
                    >
                      <X size={14} strokeWidth={1.75} />
                      取消
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="files-row-action"
                      onClick={() => closeDownloadCard(task)}
                      title="关闭"
                      aria-label="关闭下载任务卡片"
                    >
                      <X size={14} strokeWidth={1.75} />
                    </button>
                  )}
                </div>

                <div className="files-transfer-progress">
                  <Progress value={percent} tone={tone} />
                  <span className="files-task-pct mono">{percent}%</span>
                </div>

                <div className="files-transfer-meta">
                  <span className="mono">
                    {formatBytes(task.done)} / {formatBytes(task.total)}
                  </span>
                  <span className="mono">
                    {speedLabel(task.speed, task.status === "downloading")}
                  </span>
                  {task.path && (
                    <button
                      type="button"
                      className="files-copy-button"
                      onClick={() => void copyPath(task.path)}
                      aria-label="复制下载保存路径"
                    >
                      <Copy size={12} strokeWidth={1.75} />
                      复制路径
                    </button>
                  )}
                </div>

                {task.status === "error" && (
                  <div className="notice notice-error">
                    {task.message ?? "下载失败"}
                  </div>
                )}
              </div>
            );
          })}

          <div className="files-save-hint">
            <Folder size={14} strokeWidth={1.75} aria-hidden="true" />
            <span>
              下载文件保存到本机{" "}
              <span className="mono">{DOWNLOAD_DIR_LABEL}</span>
            </span>
          </div>
        </aside>
      </div>

      {historyPanel}
    </div>
  );
}
