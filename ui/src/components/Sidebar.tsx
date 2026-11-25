import { LogOut, Monitor } from "lucide-react";
import { NAV_SECTIONS } from "../nav";
import type { AccountView, DeviceView, PageKey } from "../types";

export default function Sidebar({
  page,
  onNavigate,
  account,
  devices,
  selectedCount,
  onLogout,
}: {
  page: PageKey;
  onNavigate: (page: PageKey) => void;
  account: AccountView;
  devices: DeviceView[];
  selectedCount: number;
  onLogout: () => void;
}) {
  const onlineCount = devices.filter((device) => device.online).length;
  const local = devices.find((device) => device.id === account.deviceId);
  const serverHost = account.serverUrl.replace(/^https?:\/\//, "").replace(/\/$/, "");
  // 连接码会话只有一台设备，只保留设置入口。
  const sections = account.quick
    ? NAV_SECTIONS.map((section) => ({
        ...section,
        items: section.items.filter((item) => item.key === "settings"),
      })).filter((section) => section.items.length > 0)
    : NAV_SECTIONS;

  return (
    <aside className="sidebar">
      <div className="brand" data-tauri-drag-region>
        <div className="brand-mark" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <div className="brand-text">
          <strong>ScreenWall</strong>
          <small>远程工作台</small>
        </div>
      </div>

      <nav className="nav">
        {sections.map((section) => (
          <div className="nav-section" key={section.label}>
            <div className="nav-label">{section.label}</div>
            {section.items.map((item) => {
              const Icon = item.icon;
              const active =
                page === item.key || (page === "control" && item.key === "wall");
              const badge =
                item.key === "wall"
                  ? onlineCount
                  : item.key === "group" && selectedCount > 0
                    ? selectedCount
                    : null;

              return (
                <button
                  key={item.key}
                  type="button"
                  className={`nav-item ${active ? "is-active" : ""}`}
                  aria-current={active ? "page" : undefined}
                  onClick={() => onNavigate(item.key)}
                >
                  <Icon size={16} strokeWidth={1.75} />
                  <span>{item.label}</span>
                  {badge !== null && <em>{badge}</em>}
                </button>
              );
            })}
          </div>
        ))}
      </nav>

      <div className="sidebar-foot">
        <div className="local-device">
          <div className="local-device-icon">
            <Monitor size={15} strokeWidth={1.75} />
          </div>
          <div className="local-device-copy">
            <strong>{local?.name ?? (account.quick ? "远程设备" : "这台电脑")}</strong>
            <small>
              <i className={account.connected ? "is-ok" : ""} />
              {account.connected
                ? account.quick
                  ? "连接码会话"
                  : "本机被控在线"
                : "正在连接服务器"}
            </small>
          </div>
        </div>

        <div className="account-mini">
          <div className="avatar">{account.username.slice(0, 1).toUpperCase()}</div>
          <div className="account-mini-copy">
            <strong>{account.username}</strong>
            <small title={serverHost}>{serverHost || "未配置服务器"}</small>
          </div>
          <button
            type="button"
            className="icon-btn"
            onClick={onLogout}
            title={account.quick ? "断开连接" : "退出登录"}
            aria-label={account.quick ? "断开连接" : "退出登录"}
          >
            <LogOut size={15} strokeWidth={1.75} />
          </button>
        </div>
      </div>
    </aside>
  );
}
