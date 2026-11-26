import { FlaskConical } from "lucide-react";
import { PAGE_META } from "../nav";
import type { PageKey } from "../types";
import WindowChrome from "./WindowChrome";

/**
 * 无边框窗口的拖动区。
 *
 * 用 Tauri 的 `data-tauri-drag-region`：它只对带属性的元素本身生效，
 * 所以标题文字这些子元素在 `styles.css` 里设了 `pointer-events: none`，
 * 让 mousedown 落到顶栏本身（按钮不设，仍然可点）。
 * 之前这里走 `startDragging()` 的异步调用，用户反馈长按拖不动窗口，
 * 改回属性方式（和登录页的拖动区一致）。
 */
export default function Topbar({ page }: { page: PageKey }) {
  const meta = PAGE_META[page];

  return (
    <header className="topbar" data-tauri-drag-region>
      <div className="topbar-titles">
        <h1>{meta.title}</h1>
        <span className="topbar-sub">{meta.sub}</span>
      </div>

      <div className="topbar-actions">
        {!(
          typeof window !== "undefined" && "__TAURI_INTERNALS__" in window
        ) && (
          <span className="pill pill-demo" title="浏览器预览模式">
            <FlaskConical size={12} />
            预览
          </span>
        )}
      </div>

      <WindowChrome />
    </header>
  );
}