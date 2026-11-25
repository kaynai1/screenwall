import { FlaskConical, Wifi } from "lucide-react";
import { PAGE_META } from "../nav";
import type { AccountView, PageKey } from "../types";
import WindowChrome from "./WindowChrome";

/**
 * 在无边框窗口里拖动窗口。
 *
 * 不用 `data-tauri-drag-region` 属性：它只对**带属性的那个元素本身**生效，
 * 子元素（标题、副标题、空白块）会把 mousedown 吞掉，表现为「只有标题之间
 * 的缝隙能拖」。这里直接调 `startDragging()`，整条顶栏任意位置都能拖。
 */
async function beginDrag(event: React.MouseEvent) {
  if (event.button !== 0) return;
  const target = event.target as HTMLElement | null;
  // 按钮、输入框、链接这些不该触发拖动，否则点它们会变成拖窗口。
  if (target?.closest("button, a, input, select, textarea")) return;
  if (!("__TAURI_INTERNALS__" in window)) return;
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().startDragging();
  } catch {
    // 浏览器预览或权限不足时忽略。
  }
}

export default function Topbar({
  page,
  account,
  onOpenSettings,
}: {
  page: PageKey;
  account: AccountView;
  onOpenSettings: () => void;
}) {
  const meta = PAGE_META[page];

  return (
    <header className="topbar" onMouseDown={(event) => void beginDrag(event)}>
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

        <button
          type="button"
          className="top-avatar"
          onClick={onOpenSettings}
          title="账号设置"
          aria-label={`账号设置（${account.local ? "局域网模式" : account.username || "未登录"}）`}
        >
          {account.local ? (
            <Wifi size={14} strokeWidth={1.75} />
          ) : (
            account.username.slice(0, 1).toUpperCase()
          )}
        </button>
      </div>

      <WindowChrome />
    </header>
  );
}
