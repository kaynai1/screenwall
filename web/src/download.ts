/**
 * 从被控端下载文件：`file.list` 浏览目录，`file.get` → `file.offer` →
 * `file.accept` → `file.chunk` 取数据，凑齐后交给浏览器保存。
 *
 * 被控端按 8 MB 窗口推数据，接收方每收满 2 MB 回一条 `file.progress`
 * 作为流控确认；30 秒没有进展就放弃，避免半开连接一直挂着。
 */

export type RemoteEntry = {
  name: string;
  path: string;
  dir: boolean;
  size: number;
  modified?: number;
};

export type RemoteListing = {
  path: string;
  parent: string | null;
  entries: RemoteEntry[];
};

export type DownloadState = {
  tid: number;
  name: string;
  done: number;
  total: number;
  state: "running" | "done" | "error";
  msg?: string;
};

const PROGRESS_STEP = 2 * 1024 * 1024;
const LIST_TIMEOUT = 10_000;
const OFFER_TIMEOUT = 15_000;
const STALL_TIMEOUT = 30_000;

type PendingList = {
  resolve: (listing: RemoteListing) => void;
  reject: (error: Error) => void;
  timer: number;
};

type Job = {
  tid: number;
  name: string;
  total: number;
  chunks: Uint8Array[];
  received: number;
  acked: number;
  timer: number;
  onUpdate: (state: DownloadState) => void;
};

function decodeBase64(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function baseName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || "文件";
}

function saveBlob(name: string, chunks: Uint8Array[]): void {
  const blob = new Blob(chunks as BlobPart[], { type: "application/octet-stream" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = name;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // 移动端下载要等浏览器把 Blob 交接完，别立刻释放。
  window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

export class RemoteFiles {
  private nextTid = 1;
  private lists = new Map<number, PendingList>();
  private jobs = new Map<number, Job>();

  constructor(private readonly send: (message: Record<string, unknown>) => void) {}

  private tid(): number {
    if (this.nextTid > 60_000) this.nextTid = 1;
    return this.nextTid++;
  }

  list(path: string): Promise<RemoteListing> {
    const tid = this.tid();
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.lists.delete(tid);
        reject(new Error("被控端没有响应目录请求"));
      }, LIST_TIMEOUT);
      this.lists.set(tid, { resolve, reject, timer });
      this.send({ t: "file.list", tid, path });
    });
  }

  /** 开始下载单个文件；进度与结果都通过 `onUpdate` 回调。 */
  download(path: string, onUpdate: (state: DownloadState) => void): number {
    const tid = this.tid();
    const job: Job = {
      tid,
      name: baseName(path),
      total: 0,
      chunks: [],
      received: 0,
      acked: 0,
      timer: 0,
      onUpdate,
    };
    this.jobs.set(tid, job);
    onUpdate({ tid, name: job.name, done: 0, total: 0, state: "running" });
    job.timer = window.setTimeout(() => this.fail(tid, "被控端没有响应下载请求"), OFFER_TIMEOUT);
    this.send({ t: "file.get", tid, path });
    return tid;
  }

  cancel(tid: number): void {
    this.send({ t: "file.cancel", tid, msg: "用户取消" });
    this.fail(tid, "已取消");
  }

  /** 处理与下载相关的消息；返回 true 表示这条消息已被消费。 */
  handle(message: Record<string, unknown>): boolean {
    switch (message.t) {
      case "file.list.ok": {
        const tid = Number(message.tid ?? 0);
        const pending = this.lists.get(tid);
        if (!pending) return false;
        this.lists.delete(tid);
        window.clearTimeout(pending.timer);
        pending.resolve({
          path: String(message.path ?? ""),
          parent: message.parent ? String(message.parent) : null,
          entries: Array.isArray(message.entries) ? (message.entries as RemoteEntry[]) : [],
        });
        return true;
      }
      case "file.offer": {
        const tid = Number(message.tid ?? 0);
        const job = this.jobs.get(tid);
        if (!job) return false;
        window.clearTimeout(job.timer);
        const items = Array.isArray(message.items)
          ? (message.items as Array<{ path?: string; size?: number }>)
          : [];
        const total = Number(message.totalSize ?? items[0]?.size ?? 0);
        job.total = total;
        if (items[0]?.path) job.name = baseName(items[0].path);
        job.onUpdate({ tid, name: job.name, done: 0, total, state: "running" });
        job.timer = window.setTimeout(() => this.fail(tid, "下载停滞"), STALL_TIMEOUT);
        this.send({ t: "file.accept", tid, root: "", offset: 0 });
        return true;
      }
      case "file.chunk": {
        const tid = Number(message.tid ?? 0);
        const job = this.jobs.get(tid);
        if (!job) return false;
        const data = decodeBase64(String(message.data ?? ""));
        job.chunks.push(data);
        job.received += data.length;
        job.onUpdate({
          tid,
          name: job.name,
          done: job.received,
          total: Math.max(job.total, job.received),
          state: "running",
        });
        if (job.received - job.acked >= PROGRESS_STEP) {
          job.acked = job.received;
          window.clearTimeout(job.timer);
          job.timer = window.setTimeout(() => this.fail(tid, "下载停滞"), STALL_TIMEOUT);
          this.send({ t: "file.progress", tid, done: job.received, total: job.total });
        }
        return true;
      }
      case "file.done": {
        const tid = Number(message.tid ?? 0);
        const job = this.jobs.get(tid);
        if (!job) return false;
        window.clearTimeout(job.timer);
        this.jobs.delete(tid);
        if (message.ok === false) {
          job.onUpdate({
            tid,
            name: job.name,
            done: job.received,
            total: job.total,
            state: "error",
            msg: String(message.msg ?? "下载失败"),
          });
          return true;
        }
        saveBlob(job.name, job.chunks);
        job.onUpdate({
          tid,
          name: job.name,
          done: job.received,
          total: job.received,
          state: "done",
        });
        return true;
      }
      case "file.cancel": {
        const tid = Number(message.tid ?? 0);
        const job = this.jobs.get(tid);
        if (!job) return false;
        this.fail(tid, String(message.msg ?? "下载已取消"));
        return true;
      }
      default:
        return false;
    }
  }

  /** 页面离开时调用：清掉计时器，别让回调打到已卸载的组件。 */
  dispose(): void {
    for (const pending of this.lists.values()) window.clearTimeout(pending.timer);
    this.lists.clear();
    for (const job of this.jobs.values()) window.clearTimeout(job.timer);
    this.jobs.clear();
  }

  private fail(tid: number, msg: string): void {
    const job = this.jobs.get(tid);
    if (!job) return;
    window.clearTimeout(job.timer);
    this.jobs.delete(tid);
    job.onUpdate({
      tid,
      name: job.name,
      done: job.received,
      total: job.total,
      state: "error",
      msg,
    });
  }
}
