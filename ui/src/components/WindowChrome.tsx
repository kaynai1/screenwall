import { useEffect, useState } from "react";
import { Copy, Minus, Square, X } from "lucide-react";

/**
 * 自绘窗口按钮。
 *
 * 窗口是无边框的（`decorations: false`），系统不再画标题栏，最小化 /
 * 最大化 / 关闭必须由界面自己提供。按钮尺寸和排布对齐 Windows 11：
 * 46×32，关闭悬停变红。
 *
 * 浏览器预览模式下没有 Tauri 运行时，整块不渲染。
 */
export default function WindowChrome() {
  const inTauri =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (!inTauri) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;

    void (async () => {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      const win = getCurrentWindow();
      const sync = async () => {
        const next = await win.isMaximized().catch(() => false);
        if (!disposed) setMaximized(next);
      };
      await sync();
      unlisten = await win.onResized(() => {
        void sync();
      });
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [inTauri]);

  if (!inTauri) return null;

  async function call(action: "minimize" | "toggleMaximize" | "close") {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const win = getCurrentWindow();
    if (action === "minimize") await win.minimize();
    else if (action === "toggleMaximize") await win.toggleMaximize();
    else await win.close();
  }

  return (
    <div className="win-chrome">
      <button
        type="button"
        className="win-btn"
        onClick={() => void call("minimize")}
        title="最小化"
        aria-label="最小化"
      >
        <Minus size={15} strokeWidth={1.5} />
      </button>
      <button
        type="button"
        className="win-btn"
        onClick={() => void call("toggleMaximize")}
        title={maximized ? "向下还原" : "最大化"}
        aria-label={maximized ? "向下还原" : "最大化"}
      >
        {maximized ? (
          <Copy size={12} strokeWidth={1.5} className="win-restore" />
        ) : (
          <Square size={11} strokeWidth={1.5} />
        )}
      </button>
      <button
        type="button"
        className="win-btn win-close"
        onClick={() => void call("close")}
        title="关闭"
        aria-label="关闭"
      >
        <X size={15} strokeWidth={1.5} />
      </button>
    </div>
  );
}
