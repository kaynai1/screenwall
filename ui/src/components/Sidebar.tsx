import { CircleArrowUp, LogOut, Monitor, Settings2 } from "lucide-react";
import { NAV_SECTIONS } from "../nav";
import { LinkCodeIcon, P2pIcon, RelayIcon, RouterIcon } from "./QingIcons";
import type { AccountView, DeviceView, PageKey } from "../types";
import { versionBelow } from "../version";

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
  link,
  appVersion,
  updateVersion,
}: {
  page: PageKey;
  onNavigate: (page: PageKey) => void;
  account: AccountView;
  devices: DeviceView[];
  /** 当前客户端版本；空字符串表示还没取到。 */
  appVersion: string;
  /** 启动时检查到的新版客户端版本；`null` 表示没有或检查失败。 */
  updateVersion: string | null;
  selectedCount: number;
  /** 当前远程会话实际走的链路；没有会话时为 null。 */
  link: "relay" | "direct" | "p2p" | null;
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
  // 链路胶囊只说「现在实际怎么连」：有控制会话就按后端回报的真实链路，
  // 没有会话时按默认路径（账号模式走服务器，P2P 是会话建立时才可能升级的）。
  const linkLabel = account.quick
    ? "连接码"
    : account.local
      ? "局域网直连"
      : link === "p2p"
        ? "P2P 直连"
        : link === "direct"
          ? "局域网直连"
          : "服务器中转";
  // 胶囊图标跟着链路走：P2P 用节点连线、直连用路由器、中转用云服务器。
  const LinkIcon = account.quick
    ? LinkCodeIcon
    : account.local
      ? RouterIcon
      : link === "p2p"
        ? P2pIcon
        : link === "direct"
          ? RouterIcon
          : RelayIcon;
  // 低于 0.2.11 的服务器不认识 `p2p` 信令，也不发 `sameAccount`：
  // 把这件事写在提示里，用户才知道「弹确认框」是服务器旧，不是客户端的问题。
  const serverOutdated =
    !account.local &&
    !account.quick &&
    versionBelow(account.serverVersion, "0.2.11");
  const linkHint = account.quick
    ? "通过连接码连接到这台设备"
    : account.local
      ? directOn
        ? "局域网直连已开启"
        : "局域网直连已关闭"
      : link === "p2p"
        ? "画面与控制走 P2P 直连（UDP 打洞成功，不经过服务器中转）"
        : link === "direct"
          ? "画面与控制走局域网直连"
          : serverOutdated
            ? `服务器 ${account.serverVersion} 版本较旧：升级服务器后可用 P2P 直连与同账号免确认`
            : `画面与控制经服务器中转：${serverHost}`;
  // 客户端有新版，或服务器版本落后于当前客户端时，在左下角挂一条提示。
  // 网络失败/没配置更新源时后端返回 null，这里就什么都不显示。
  const serverBehind =
    !account.local &&
    !account.quick &&
    appVersion !== "" &&
    versionBelow(account.serverVersion, appVersion);
  const updateNote = updateVersion
    ? { title: `发现新版本 ${updateVersion}`, sub: "点击前往设置更新客户端" }
    : serverBehind
      ? {
          title: "服务器版本需要更新",
          sub: `${account.serverVersion} → ${appVersion}`,
        }
      : null;
  // 连接码会话只有一台设备，只保留设置入口；局域网模式连不上服务器，反馈提交不了。
  const sections = NAV_SECTIONS.map((section) => ({
    ...section,
    items: section.items.filter((item) =>
      account.quick
        ? item.key === "settings"
        : account.local
          ? item.key !== "feedback"
          : true,
    ),
  })).filter((section) => section.items.length > 0);

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
              account.local
                ? directOn
                  ? ""
                  : "is-off"
                : account.connected
                  ? link === "p2p" || link === "direct"
                    ? ""
                    : "is-relay"
                  : "is-off"
            }`}
            title={linkHint}
          >
            <LinkIcon size={12} strokeWidth={2} />
            {linkLabel}
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

      {updateNote && (
        <button
          type="button"
          className="update-note"
          onClick={onOpenSettings}
          title={`${updateNote.title}：${updateNote.sub}`}
        >
          <span className="update-note-icon" aria-hidden="true">
            <CircleArrowUp size={15} strokeWidth={1.75} />
          </span>
          <span className="update-note-copy">
            <strong>{updateNote.title}</strong>
            <small>{updateNote.sub}</small>
          </span>
        </button>
      )}

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
              <RouterIcon size={14} />
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
