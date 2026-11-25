import { Cloud, CloudOff, FlaskConical, Wifi } from "lucide-react";
import { PAGE_META } from "../nav";
import type { AccountView, PageKey } from "../types";
import WindowChrome from "./WindowChrome";

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

        <span className={`pill ${account.connected ? "pill-ok" : "pill-warn"}`}>
          {account.local ? (
            <Wifi size={12} />
          ) : account.connected ? (
            <Cloud size={12} />
          ) : (
            <CloudOff size={12} />
          )}
          {account.local
            ? account.connected
              ? "局域网直连"
              : "开启直连"
            : account.connected
              ? "已连接"
              : "连接中"}
        </span>

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
