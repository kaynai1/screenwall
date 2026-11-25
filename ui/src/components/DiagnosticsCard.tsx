import { useEffect, useState } from "react";
import { FileText, FolderOpen, Loader2, PackageOpen } from "lucide-react";
import { Switch } from "./ui";
import { errorLine } from "../errors";
import {
  exportDiagnostics,
  getCloseToTray,
  getLogInfo,
  openLogDir,
  setCloseToTray,
  type LogInfo,
} from "../logs";
import { formatBytes } from "../mock";

/**
 * 「诊断与后台」设置组：托盘开关 + 日志占用 + 导出诊断包。
 * 独立成组件，避免设置页各个分组互相牵连。
 */
export default function DiagnosticsCard({
  notify,
}: {
  notify: (text: string, tone?: "info" | "success" | "error") => void;
}) {
  const [closeToTray, setCloseToTrayState] = useState(true);
  const [info, setInfo] = useState<LogInfo | null>(null);
  const [busy, setBusy] = useState<"export" | "open" | null>(null);
  const [exported, setExported] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    void getCloseToTray().then((value) => {
      if (alive) setCloseToTrayState(value);
    });
    void getLogInfo()
      .then((value) => {
        if (alive) setInfo(value);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  async function toggleTray(next: boolean) {
    setCloseToTrayState(next);
    try {
      await setCloseToTray(next);
      notify(
        next ? "关闭窗口时会缩到托盘" : "关闭窗口将直接退出",
        "success",
      );
    } catch (error) {
      setCloseToTrayState(!next);
      notify(errorLine(error), "error");
    }
  }

  async function openLogs() {
    if (busy) return;
    setBusy("open");
    try {
      const dir = await openLogDir();
      notify(`已打开日志目录：${dir}`, "info");
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setBusy(null);
    }
  }

  async function exportLogs() {
    if (busy) return;
    setBusy("export");
    try {
      const dir = await exportDiagnostics();
      setExported(dir);
      notify("诊断包已导出（已脱敏账号令牌）", "success");
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="set-group" aria-labelledby="set-diagnostics">
      <h3 className="set-group-title" id="set-diagnostics">
        后台与诊断
      </h3>
      <div className="set-rows">
        <Switch
          label="关闭窗口时缩到托盘"
          hint="关闭后点窗口的 × 会直接退出，远程会话随之断开"
          checked={closeToTray}
          onChange={(value) => void toggleTray(value)}
        />

        <div className="set-row">
          <div className="set-row-copy">
            <span className="set-row-label">日志</span>
            <span className="set-row-hint">
              {info
                ? `${info.files} 个文件 · ${formatBytes(info.bytes)} · ${info.dir}`
                : "正在读取日志目录…"}
            </span>
          </div>
          <div className="set-inline-control">
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => void openLogs()}
              disabled={busy !== null}
            >
              {busy === "open" ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <FolderOpen size={14} />
              )}
              打开目录
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => void exportLogs()}
              disabled={busy !== null}
            >
              {busy === "export" ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <PackageOpen size={14} />
              )}
              导出诊断包
            </button>
          </div>
        </div>

        {exported && (
          <div className="set-row">
            <span className="set-row-copy">
              <span className="set-row-label">最近导出</span>
            </span>
            <code className="set-code set-code-wide">{exported}</code>
          </div>
        )}

        <div className="set-row">
          <div className="set-row-copy">
            <span className="set-row-label">日志滚动</span>
            <span className="set-row-hint">
              单个文件 2 MiB，保留 3 个历史文件；崩溃原因也会写入日志
            </span>
          </div>
          <span className="set-row-value">
            <FileText size={14} />
            screenwall.log
          </span>
        </div>
      </div>
    </section>
  );
}
