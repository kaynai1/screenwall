import { LogOut, Monitor, Settings2, Wifi } from "lucide-react";
import { NAV_SECTIONS } from "../nav";
import type { AccountView, DeviceView, PageKey } from "../types";

export default function Sidebar({
  page,
  onNavigate,
  account,
  devices,
  selectedCount,
  onLogout,
  onOpenSettings,
  directOn,
  directBusy,
  onToggleDirect,
}: {
  page: PageKey;
  onNavigate: (page: PageKey) => void;
  account: AccountView;
  devices: DeviceView[];
  selectedCount: number;
  onLogout: () => void;
  onOpenSettings: () => void;
  /** 局域网直连当前是否开启（只在本机模式下有意义）。 */
  directOn: boolean;
  directBusy: boolean;
  onToggleDirect: () => void;
}) {
  const onlineCount = devices.filter((device) => device.online).length;
  const local = devices.find((device) => device.id === account.deviceId);
  const serverHost = account.local
    ? "无服务器"
    : account.serverUrl.replace(/^https?:\/\//, "").replace(/\/$/, "");
  const serverHint = account.local ? "局域网直连 · 无服务器" : serverHost;
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
          {/* 这里显示的永远是「当前用什么方式连」：账号模式是服务器中转，
              局域网模式是直连（绿色=直连开着，灰色=关着），连接码会话单独标出来。 */}
          <span
            className={`brand-direct ${
              account.local ? (directOn ? "" : "is-off") : account.connected ? "" : "is-off"
            }`}
            title={
              account.quick
                ? "通过连接码连接到这台设备"
                : account.local
                  ? directOn
                    ? "局域网直连已开启"
                    : "局域网直连已关闭"
                  : `画面与控制经服务器中转：${serverHost}`
            }
          >
            <Wifi size={12} strokeWidth={2} aria-hidden="true" />
            {account.quick ? "连接码" : account.local ? "局域网直连" : "服务器中转"}
          </span>
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
        {/* 本机状态 + 直连开关。原来「开启直连」是顶栏上一个不可点的胶囊，
            点了没反应；现在做成可点的开关，直接控制本机是否被直连。 */}
        <button
          type="button"
          className="local-device"
          onClick={() => onToggleDirect()}
          disabled={directBusy || account.quick}
          title={
            account.local
              ? directOn
                ? "点击关闭局域网直连"
                : "点击开启局域网直连"
              : "直连只在局域网模式下可切换"
          }
        >
          <div className="local-device-icon">
            <Monitor size={15} strokeWidth={1.75} />
          </div>
          <div className="local-device-copy">
            <strong>{local?.name ?? (account.quick ? "远程设备" : "这台电脑")}</strong>
            <small>
              <i
                className={
                  account.local ? (directOn ? "is-ok" : "") : account.connected ? "is-ok" : ""
                }
              />
              <span className="side-copy-text">
                {account.local
                  ? directOn
                    ? "直连已开启"
                    : "直连已关闭"
                  : account.connected
                    ? account.quick
                      ? "连接码会话"
                      : "本机被控在线"
                    : "连接服务器中"}
              </span>
            </small>
          </div>
          {account.local && (
            <span className={`local-switch ${directOn ? "is-on" : ""}`} aria-hidden="true" />
          )}
        </button>

        <div className="account-mini">
          <div className="avatar">
            {account.local ? (
              <Wifi size={14} strokeWidth={1.75} />
            ) : (
              account.username.slice(0, 1).toUpperCase()
            )}
          </div>
          <div className="account-mini-copy">
            <strong>{account.local ? "局域网模式" : account.username}</strong>
            <small title={serverHint}>
              <span className="side-copy-text">{serverHost || "未配置服务器"}</span>
            </small>
          </div>
          <button
            type="button"
            className="icon-btn"
            onClick={onOpenSettings}
            title="账号设置"
            aria-label="账号设置"
          >
            <Settings2 size={15} strokeWidth={1.75} />
          </button>
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
