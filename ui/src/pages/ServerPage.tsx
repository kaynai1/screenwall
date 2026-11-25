import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import {
  getPanelStatus,
  hideServerPanel,
  resizeServerPanel,
  showServerPanel,
  type PanelBounds,
} from "../api";
import { errorLine } from "../errors";

/**
 * 服务器管理页（kaypanel）。
 *
 * 面板本身不是 HTML —— 它是一个独立的原生子 webview（kaypanel 自己带
 * `X-Frame-Options: DENY`，iframe 会白屏）。这个组件只做三件事：
 *   1. 量出内容区在窗口里的位置，交给 Rust 把子 webview 摆上去；
 *   2. 窗口缩放时同步位置；
 *   3. 离开本页时把子 webview 藏起来。
 *
 * 所以页面本身留空，只放一个加载态与错误态，等子 webview 盖上来。
 */
export default function ServerPage() {
  const anchor = useRef<HTMLDivElement | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);

  /** 量出锚点元素在窗口坐标系里的矩形（CSS 像素）。 */
  function measure(): PanelBounds | null {
    const node = anchor.current;
    if (!node) return null;
    const rect = node.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return null;
    return {
      x: rect.left,
      y: rect.top,
      width: rect.width,
      height: rect.height,
      // Rust 侧用「窗口 DIP 宽度 ÷ 视口 CSS 宽度」求换算系数，
      // 同时兼容 DPI 缩放与界面缩放（set_zoom）。
      viewportWidth: window.innerWidth,
    };
  }

  // 首次进入：量位置 → 拉起面板 → 显示。
  useEffect(() => {
    let alive = true;

    (async () => {
      try {
        const status = await getPanelStatus();
        if (!alive) return;
        if (!status.available) {
          setError(
            status.error ?? "服务器管理组件不可用，请重新安装 ScreenWall",
          );
          return;
        }
        // 等一帧，确保布局已经完成再量。
        await new Promise((resolve) => requestAnimationFrame(resolve));
        if (!alive) return;
        const bounds = measure();
        if (!bounds) {
          setError("无法定位内容区，请调整窗口大小后重试");
          return;
        }
        await showServerPanel(bounds);
        if (!alive) return;
        setReady(true);
      } catch (failure) {
        if (alive) setError(errorLine(failure));
      }
    })();

    return () => {
      alive = false;
      // 离开本页：把子 webview 藏起来，别盖在别的页面上。
      void hideServerPanel().catch(() => {});
    };
  }, []);

  // 内容区尺寸/位置变化：重新量并同步位置。
  // 不能只听 window.resize —— 侧边栏折叠、顶栏高度变化都不触发它，
  // 但内容区确实变了。ResizeObserver 覆盖这两种情况。
  useLayoutEffect(() => {
    if (!ready) return;
    const node = anchor.current;
    if (!node) return;

    let frame = 0;
    const sync = () => {
      const bounds = measure();
      if (bounds) void resizeServerPanel(bounds).catch(() => {});
    };
    const schedule = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(sync);
    };

    const observer = new ResizeObserver(schedule);
    observer.observe(node);
    window.addEventListener("resize", schedule);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", schedule);
    };
  }, [ready]);

  return (
    <div className="server-page" ref={anchor}>
      {error ? (
        <div className="server-page-state is-error">
          <strong>服务器管理不可用</strong>
          <p>{error}</p>
        </div>
      ) : (
        <div className="server-page-state">
          <Loader2 size={18} className="spin" />
          <p>正在启动服务器管理…</p>
        </div>
      )}
    </div>
  );
}
