import {
  ChevronRight,
  Download,
  LayoutGrid,
  LogOut,
  Monitor,
  RefreshCw,
  Smartphone,
  Wifi,
  WifiOff,
} from "lucide-react";
import { useEffect, useState } from "react";
import type { Account, DeviceView } from "../api";

/** Chrome/Edge 的安装事件，TS 的 DOM 类型里还没有。 */
type InstallPromptEvent = Event & {
  prompt: () => Promise<void>;
  userChoice: Promise<{ outcome: "accepted" | "dismissed" }>;
};

const INSTALL_HINT_KEY = "screenwall.web.installHintDismissed";

function isMobileBrowser(): boolean {
  return /android|iphone|ipad|ipod|mobile/i.test(navigator.userAgent);
}

function isStandalone(): boolean {
  return (
    window.matchMedia("(display-mode: standalone)").matches ||
    (navigator as Navigator & { standalone?: boolean }).standalone === true
  );
}

function relativeTime(ms: number): string {
  if (!ms) return "从未上线";
  const diff = Date.now() - ms;
  if (diff < 60_000) return "刚刚在线";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

export default function DevicesPage({
  account,
  devices,
  connected,
  connectionError,
  onOpen,
  onOpenWall,
  onLogout,
  onReconnect,
}: {
  account: Account;
  devices: DeviceView[];
  connected: boolean;
  connectionError: string | null;
  onOpen: (device: DeviceView) => void;
  onOpenWall: () => void;
  onLogout: () => void;
  onReconnect: () => void;
}) {
  const online = devices.filter((device) => device.online).length;
  const previewable = devices.filter((device) => device.online && device.controllable).length;
  const host = account.serverUrl.replace(/^https?:\/\//, "");
  const [installEvent, setInstallEvent] = useState<InstallPromptEvent | null>(null);
  const [installed, setInstalled] = useState(isStandalone);
  const [installDismissed, setInstallDismissed] = useState(() => {
    try {
      return localStorage.getItem(INSTALL_HINT_KEY) === "1";
    } catch {
      return false;
    }
  });

  // 手机上「添加到主屏幕」后就没有地址栏，和原生 App 差不多。
  useEffect(() => {
    const onPrompt = (event: Event) => {
      event.preventDefault();
      setInstallEvent(event as InstallPromptEvent);
    };
    const onInstalled = () => setInstalled(true);
    window.addEventListener("beforeinstallprompt", onPrompt);
    window.addEventListener("appinstalled", onInstalled);
    return () => {
      window.removeEventListener("beforeinstallprompt", onPrompt);
      window.removeEventListener("appinstalled", onInstalled);
    };
  }, []);

  const dismissInstall = () => {
    setInstallDismissed(true);
    try {
      localStorage.setItem(INSTALL_HINT_KEY, "1");
    } catch {
      // 忽略。
    }
  };

  const installNow = async () => {
    if (!installEvent) return;
    await installEvent.prompt();
    setInstallEvent(null);
  };

  const showInstall = isMobileBrowser() && !installed && !installDismissed;

  return (
    <div className="page">
      <header className="app-bar">
        <div>
          <h1>{account.quick ? "连接码会话" : "设备"}</h1>
          <p className="app-bar-sub">
            <i className={`dot ${connected ? "is-on" : "is-off"}`} />
            {connected ? `已连接 ${host}` : "正在连接服务器…"}
          </p>
        </div>
        <div className="app-bar-actions">
          {previewable > 0 && (
            <button type="button" className="icon-btn" onClick={onOpenWall} aria-label="屏幕墙">
              <LayoutGrid size={18} />
            </button>
          )}
          <button type="button" className="icon-btn" onClick={onLogout} aria-label="退出登录">
            <LogOut size={18} />
          </button>
        </div>
      </header>

      {connectionError && (
        <div className="banner banner-error">
          <div>
            <strong>连接已断开</strong>
            <span>{connectionError}</span>
          </div>
          <button type="button" className="ghost-btn" onClick={onReconnect}>
            <RefreshCw size={15} />
            重连
          </button>
        </div>
      )}

      {showInstall && (
        <div className="banner banner-tip">
          <div>
            <strong>安装到桌面</strong>
            <span>
              {installEvent
                ? "装到桌面后全屏打开，和 App 一样用。"
                : "在浏览器菜单里选「添加到主屏幕」，就能像 App 一样全屏使用。"}
            </span>
          </div>
          {installEvent ? (
            <button type="button" className="ghost-btn" onClick={installNow}>
              <Download size={15} />
              安装
            </button>
          ) : (
            <button type="button" className="ghost-btn" onClick={dismissInstall}>
              知道了
            </button>
          )}
        </div>
      )}

      <div className="list-head">
        <span>{account.quick ? "这台设备" : "我的设备"}</span>
        <span className="muted">
          {online} 在线 / {devices.length} 台
        </span>
      </div>

      <div className="device-list">
        {devices.map((device) => {
          const usable = device.online && device.controllable;
          return (
            <button
              type="button"
              key={device.id}
              className={`device-card ${usable ? "" : "is-dim"}`}
              onClick={() => usable && onOpen(device)}
              disabled={!usable}
            >
              <div className="device-icon">
                {/android|iphone|ipad|mobile/i.test(device.os) ? (
                  <Smartphone size={20} />
                ) : (
                  <Monitor size={20} />
                )}
              </div>
              <div className="device-copy">
                <strong>{device.name || "未命名设备"}</strong>
                <small>
                  {device.online ? (
                    <>
                      <Wifi size={12} /> 在线
                    </>
                  ) : (
                    <>
                      <WifiOff size={12} /> {relativeTime(device.lastSeen)}
                    </>
                  )}
                  {device.os ? ` · ${device.os}` : ""}
                  {!device.controllable ? " · 已关闭被控" : ""}
                </small>
              </div>
              <ChevronRight size={18} className="chevron" />
            </button>
          );
        })}

        {devices.length === 0 && (
          <div className="empty">
            <Monitor size={28} />
            <strong>还没有设备</strong>
            <span>在被控电脑上登录同一账号，它会出现在这里。</span>
          </div>
        )}
      </div>

      <footer className="page-foot">
        {account.quick ? "连接码会话 15 分钟后失效" : `当前账号 ${account.username}`}
      </footer>
    </div>
  );
}
