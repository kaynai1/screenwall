/**
 * 下载被控端文件：底部弹出目录浏览，点文件即开始下载到本机。
 *
 * 只读操作：列目录用 `file.list`，取文件用 `file.get`；
 * 大文件按块累加，结束时交给浏览器存盘（手机上是系统下载）。
 */

import {
  ChevronLeft,
  Download,
  File as FileIcon,
  Folder,
  Loader2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import type { DownloadState, RemoteEntry, RemoteFiles, RemoteListing } from "../download";

function formatSize(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(1)} GB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`;
  if (bytes >= 1 << 10) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} B`;
}

function sortEntries(entries: RemoteEntry[]): RemoteEntry[] {
  return [...entries].sort((a, b) => {
    if (a.dir !== b.dir) return a.dir ? -1 : 1;
    return a.name.localeCompare(b.name, "zh-CN");
  });
}

export default function FileBrowser({
  files,
  onClose,
}: {
  files: RemoteFiles;
  onClose: () => void;
}) {
  const [listing, setListing] = useState<RemoteListing | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [downloads, setDownloads] = useState<Record<number, DownloadState>>({});

  const load = useCallback(
    (path: string) => {
      setLoading(true);
      setError("");
      files
        .list(path)
        .then((next) => setListing(next))
        .catch((thrown: unknown) =>
          setError(thrown instanceof Error ? thrown.message : String(thrown)),
        )
        .finally(() => setLoading(false));
    },
    [files],
  );

  useEffect(() => {
    load("");
  }, [load]);

  function startDownload(entry: RemoteEntry) {
    files.download(entry.path, (state) => {
      setDownloads((current) => ({ ...current, [state.tid]: state }));
    });
  }

  const rows = Object.values(downloads).sort((a, b) => a.tid - b.tid);
  const entries = listing ? sortEntries(listing.entries) : [];

  return (
    <div className="file-overlay" role="dialog" aria-label="下载电脑文件">
      <div className="file-sheet">
        <header className="file-head">
          <button
            type="button"
            className="icon-btn"
            disabled={!listing?.parent}
            onClick={() => listing?.parent && load(listing.parent)}
            aria-label="上一级"
          >
            <ChevronLeft size={18} />
          </button>
          <div className="file-head-copy">
            <strong>电脑文件</strong>
            <small>{listing?.path || "读取中…"}</small>
          </div>
          <button type="button" className="icon-btn" onClick={onClose} aria-label="关闭">
            <X size={18} />
          </button>
        </header>

        <div className="file-list">
          {loading && (
            <div className="file-state">
              <Loader2 size={16} className="spin" />
              <span>正在读取目录…</span>
            </div>
          )}
          {!loading && error && (
            <div className="file-state is-error">
              <span>{error}</span>
              <button type="button" className="ghost-btn" onClick={() => load(listing?.path ?? "")}>
                重试
              </button>
            </div>
          )}
          {!loading && !error && entries.length === 0 && (
            <div className="file-state">
              <span>这个目录是空的</span>
            </div>
          )}
          {!loading &&
            !error &&
            entries.map((entry) => (
              <button
                type="button"
                key={entry.path}
                className="file-row"
                onClick={() => (entry.dir ? load(entry.path) : startDownload(entry))}
              >
                {entry.dir ? <Folder size={18} /> : <FileIcon size={18} />}
                <span className="file-name">{entry.name}</span>
                <small>{entry.dir ? "" : formatSize(entry.size)}</small>
                {!entry.dir && <Download size={14} className="file-get" />}
              </button>
            ))}
        </div>

        {rows.length > 0 && (
          <div className="file-downloads">
            {rows.map((item) => {
              const percent = item.total > 0 ? Math.min(100, (item.done / item.total) * 100) : 0;
              return (
                <div className={`dl-row is-${item.state}`} key={item.tid}>
                  <div className="dl-copy">
                    <span className="file-name">{item.name}</span>
                    <small>
                      {item.state === "error"
                        ? item.msg || "下载失败"
                        : item.state === "done"
                          ? `已保存 · ${formatSize(item.done)}`
                          : `${formatSize(item.done)} / ${item.total ? formatSize(item.total) : "…"}`}
                    </small>
                  </div>
                  {item.state === "running" && (
                    <>
                      <div className="dl-bar" aria-label="下载进度">
                        <span style={{ width: `${percent}%` }} />
                      </div>
                      <button
                        type="button"
                        className="ghost-btn"
                        onClick={() => files.cancel(item.tid)}
                      >
                        取消
                      </button>
                    </>
                  )}
                  {item.state !== "running" && (
                    <button
                      type="button"
                      className="ghost-btn"
                      onClick={() =>
                        setDownloads((current) => {
                          const next = { ...current };
                          delete next[item.tid];
                          return next;
                        })
                      }
                    >
                      关闭
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
