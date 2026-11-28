/**
 * 网页端上传文件：`file.offer` → `file.accept` → 按 256 KiB 切块发 `file.chunk`，
 * 用 `file.progress` 做 8 MiB 流控窗口，最后等被控端回 `file.done` / `file.complete`。
 *
 * 和桌面端不同，网页端不做增量 SHA256（手机上一次把大文件读进内存不现实），
 * `sha256` 传空串，被控端按字节数落盘。下载与目录浏览尚未实现。
 */

import type { Viewer } from "./api";

const CHUNK_BYTES = 256 * 1024;
const WINDOW_BYTES = 8 * 1024 * 1024;

let nextTid = 1;

function takeTid(): number {
  const tid = nextTid;
  nextTid = nextTid >= 0xffff ? 1 : nextTid + 1;
  return tid;
}

function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const step = 0x8000;
  for (let i = 0; i < bytes.length; i += step) {
    binary += String.fromCharCode(...bytes.subarray(i, i + step));
  }
  return btoa(binary);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

/** 轮询等待条件成立；网络回调在别处更新闭包变量。 */
async function until(
  condition: () => boolean,
  timeoutMs: number,
  timeoutText: string,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error(timeoutText);
    await sleep(40);
  }
}

export type UploadResult = { files: number; bytes: number };

export async function uploadFiles(
  viewer: Viewer,
  files: File[],
  onProgress?: (sent: number, total: number) => void,
): Promise<UploadResult> {
  if (files.length === 0) return { files: 0, bytes: 0 };
  const tid = takeTid();
  const total = files.reduce((sum, file) => sum + file.size, 0);
  let sent = 0;
  let acked = 0;
  let accepted: { resume: { item: number; offset: number }[] } | null = null;
  let completed: UploadResult | null = null;
  let denied: string | null = null;
  const results: { ok: boolean; msg?: string }[] = [];

  const off = viewer.subscribe({
    onMessage: (message) => {
      if (Number(message.tid ?? -1) !== tid) return;
      switch (message.t) {
        case "file.accept": {
          const resume = Array.isArray(message.resume)
            ? (message.resume as { item: number; offset: number }[])
            : [];
          accepted = { resume };
          break;
        }
        case "file.progress": {
          acked = Math.max(acked, Number(message.done ?? 0));
          break;
        }
        case "file.done": {
          results.push({ ok: Boolean(message.ok), msg: message.msg as string | undefined });
          break;
        }
        case "file.complete": {
          completed = {
            files: Number(message.files ?? files.length),
            bytes: Number(message.bytes ?? total),
          };
          break;
        }
        case "file.deny": {
          denied = String(message.msg ?? "被控端拒绝了文件");
          break;
        }
        default:
          break;
      }
    },
  });

  try {
    viewer.sendJson({
      t: "file.offer",
      tid,
      items: files.map((file) => ({ path: file.name, size: file.size, sha256: "" })),
      totalSize: total,
    });
    await until(
      () => accepted !== null || denied !== null,
      30_000,
      "等待被控端接收文件超时",
    );
    if (denied) throw new Error(denied);

    const resume = new Map(accepted!.resume.map((item) => [item.item, item.offset]));
    for (let index = 0; index < files.length; index += 1) {
      const file = files[index];
      const start = Math.min(file.size, Math.max(0, resume.get(index) ?? 0));
      if (start > 0) {
        sent += start;
        onProgress?.(sent, total);
      }
      // P2P 下走 DataChannel：webrtc-rs 单条消息默认上限 256 KiB，
      // base64 后要留足余量，所以块大小压到 96 KiB。
      const chunkBytes = viewer.usesP2p ? 96 * 1024 : CHUNK_BYTES;
      let offset = start;
      while (offset < file.size) {
        // 在途字节超过窗口就先等被控端的 file.progress。
        while (sent - acked > WINDOW_BYTES) {
          await until(
            () => sent - acked <= WINDOW_BYTES || denied !== null || completed !== null,
            30_000,
            "文件传输超时，被控端没有确认",
          );
          if (denied) throw new Error(denied);
        }
        const end = Math.min(file.size, offset + chunkBytes);
        const bytes = new Uint8Array(await file.slice(offset, end).arrayBuffer());
        viewer.sendJson({ t: "file.chunk", tid, item: index, offset, data: toBase64(bytes) });
        sent += bytes.length;
        offset = end;
        onProgress?.(sent, total);
      }
    }

    await until(() => completed !== null || denied !== null, 180_000, "等待被控端写入文件超时");
    if (denied) throw new Error(denied);
    const failed = results.find((item) => !item.ok);
    if (failed) throw new Error(failed.msg || "部分文件写入失败");
    return completed!;
  } finally {
    off();
  }
}
