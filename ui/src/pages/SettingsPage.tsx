import { useEffect, useRef, useState } from "react";
import {
  Check,
  ChevronDown,
  Copy,
  Download,
  Info,
  Loader2,
  LogOut,
  Palette,
  RefreshCw,
  Server,
  ShieldCheck,
  TriangleAlert,
  UserRound,
} from "lucide-react";
import {
  allowDirectFirewall,
  forgetDirectPeers,
  getAutostart,
  getDirectFirewallState,
  getDirectState,
  getLocalSettings,
  regeneratePairingCode,
  setAllowControl,
  setAutostart,
  setCaPath,
  setDirectEnabled,
  setUiZoom,
  setVirtualDisplay,
  testServer,
} from "../api";
import { Modal, Progress, Segmented, Switch } from "../components/ui";
import {
  checkUpdate,
  currentVersion,
  downloadUpdate,
  installUpdate,
  subscribeUpdateProgress,
  type UpdateInfo,
} from "../update";
import DiagnosticsCard from "../components/DiagnosticsCard";
import { errorLine } from "../errors";
import { applyMotion, applyTheme, storedMotion, storedTheme, type Theme } from "../theme";
import type {
  DirectFirewallState,
  DirectState,
  SettingsPageProps,
} from "../types";
import "./SettingsPage.css";

type TestState =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "ok"; ms: number }
  | { kind: "error"; message: string };

type SectionKey = "account" | "connection" | "security" | "about";

const ZOOM_OPTIONS = [
  { value: "90", label: "90%" },
  { value: "100", label: "100%（推荐）" },
  { value: "110", label: "110%" },
  { value: "125", label: "125%" },
];

const SECTIONS = [
  { key: "account", label: "账号", icon: UserRound },
  { key: "connection", label: "连接", icon: Server },
  { key: "security", label: "安全", icon: ShieldCheck },
  { key: "about", label: "关于", icon: Info },
] as const;

/** 主题与动画偏好都只影响本机界面，存在 localStorage 里就够了。 */
type SavedSettings = {
  allowControl: boolean;
  autoStart: boolean;
  zoom: number;
  animations: boolean;
  theme: Theme;
};

/** 更新检查状态。 */
type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "latest" }
  | { kind: "available"; info: UpdateInfo }
  | { kind: "error"; message: string };

/** 下载/安装状态。 */
type DownloadPhase =
  | { kind: "idle" }
  | { kind: "downloading"; received: number; total: number }
  | { kind: "installing" }
  | { kind: "error"; message: string };

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** 配对码剩余有效期：mm:ss。 */
function formatPairingTime(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  return `${mm}:${ss}`;
}

function formatPublished(ms: number): string {
  if (!ms || !Number.isFinite(ms)) return "";
  try {
    return new Date(ms).toLocaleDateString("zh-CN", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
    });
  } catch {
    return "";
  }
}

export default function SettingsPage({
  account,
  notify,
  onLogout,
  busy,
}: SettingsPageProps) {
  const [activeSection, setActiveSection] = useState<SectionKey>("account");
  const [serverUrl, setServerUrl] = useState(account.serverUrl);
  const [test, setTest] = useState<TestState>({ kind: "idle" });
  const [copied, setCopied] = useState(false);
  const [logoutOpen, setLogoutOpen] = useState(false);
  const [saving, setSaving] = useState(false);

  // 本机设置：改动先留在界面，点「保存更改」才落地。
  const [allowControl, setAllowControlState] = useState(true);
  // 无头虚拟显示器：切换即时生效（像直连开关一样，不参与保存按钮）。
  const [virtualDisplay, setVirtualDisplayState] = useState(false);
  const [virtualDisplayBusy, setVirtualDisplayBusy] = useState(false);
  const [autoStart, setAutoStartState] = useState(false);
  const [zoom, setZoom] = useState("100");
  const [animations, setAnimationsState] = useState(true);
  const [theme, setThemeState] = useState<Theme>(storedTheme);
  const [saved, setSaved] = useState<SavedSettings>({
    allowControl: true,
    autoStart: false,
    zoom: 100,
    animations: true,
    theme: "light",
  });

  // 软件更新：检查结果与下载/安装状态分开维护。
  const [version, setVersion] = useState("0.1.0");
  const [update, setUpdate] = useState<UpdatePhase>({ kind: "idle" });
  const [download, setDownload] = useState<DownloadPhase>({ kind: "idle" });
  const installRef = useRef(false);

  // 局域网直连：开关即时生效，不参与下面的 dirty / saved 机制。
  const [direct, setDirect] = useState<DirectState | null>(null);
  const [directBusy, setDirectBusy] = useState(false);
  const [pairingBusy, setPairingBusy] = useState(false);
  const [forgetBusy, setForgetBusy] = useState(false);
  const [forgetOpen, setForgetOpen] = useState(false);
  const [directError, setDirectError] = useState<string | null>(null);
  // 防火墙规则状态：安装器理应先加好；缺失时提示用户一键补规则。
  const [firewall, setFirewall] = useState<DirectFirewallState | null>(null);
  const [firewallBusy, setFirewallBusy] = useState(false);
  // 自定义 CA：改完点「应用」才保存并校验。
  const [caPath, setCaPathState] = useState("");
  const [caBusy, setCaBusy] = useState(false);
  const [caError, setCaError] = useState<string | null>(null);

  const copiedTimer = useRef<number | null>(null);
  const loadSeq = useRef(0);

  useEffect(() => {
    setServerUrl(account.serverUrl);
  }, [account.serverUrl]);

  useEffect(() => {
    let alive = true;
    currentVersion()
      .then((value) => {
        if (alive) setVersion(value);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  // 下载进度由后端事件推送；安装开始后不再更新进度。
  useEffect(() => {
    return subscribeUpdateProgress((progress) => {
      if (installRef.current) return;
      setDownload((current) =>
        current.kind === "downloading"
          ? {
              kind: "downloading",
              received: progress.received,
              total: progress.total || current.total,
            }
          : current,
      );
    });
  }, []);

  useEffect(() => {
    setTest({ kind: "idle" });
  }, [serverUrl]);

  useEffect(() => {
    const seq = ++loadSeq.current;
    (async () => {
      const [local, autostart] = await Promise.all([
        getLocalSettings(),
        getAutostart(),
      ]);
      if (seq !== loadSeq.current) return;
      const zoomValue = String(Math.round(local.uiZoom * 100));
      const nextZoom = ZOOM_OPTIONS.some((item) => item.value === zoomValue)
        ? zoomValue
        : "100";
      const motion = storedMotion();
      const currentTheme = storedTheme();
      setAllowControlState(local.allowControl);
      setVirtualDisplayState(local.virtualDisplay ?? false);
      setCaPathState(local.caPath ?? "");
      setAutoStartState(autostart);
      setZoom(nextZoom);
      setAnimationsState(motion);
      setThemeState(currentTheme);
      applyMotion(motion);
      applyTheme(currentTheme);
      setSaved({
        allowControl: local.allowControl,
        autoStart: autostart,
        zoom: Number(nextZoom),
        animations: motion,
        theme: currentTheme,
      });
    })().catch(() => {});
  }, []);

  // 直连状态由后端持有：进页面读一次，之后只按命令返回值更新。
  useEffect(() => {
    let alive = true;
    getDirectState()
      .then((state) => {
        if (alive) setDirect(state);
      })
      .catch((error) => {
        if (alive) setDirectError(errorLine(error));
      });
    return () => {
      alive = false;
    };
  }, []);

  // 直连开启后查一次防火墙规则；currentUser 安装、规则被删或换了端口
  // 都会落在这里，由用户点「允许局域网直连」提权修复。
  useEffect(() => {
    if (!direct?.enabled) {
      setFirewall(null);
      return;
    }
    let alive = true;
    getDirectFirewallState()
      .then((state) => {
        if (alive) setFirewall(state);
      })
      .catch(() => {
        if (alive) setFirewall(null);
      });
    return () => {
      alive = false;
    };
  }, [direct?.enabled, direct?.port]);

  // 配对码倒计时：本地每秒递减，归零后向后端要一次最新状态。
  useEffect(() => {
    if (!direct?.enabled || !direct.pairingCode) return;
    if (direct.pairingExpiresIn <= 0) {
      let alive = true;
      getDirectState()
        .then((state) => {
          if (alive) setDirect(state);
        })
        .catch(() => {});
      return () => {
        alive = false;
      };
    }
    const timer = window.setTimeout(() => {
      setDirect((current) =>
        current
          ? {
              ...current,
              pairingExpiresIn: Math.max(0, current.pairingExpiresIn - 1),
            }
          : current,
      );
    }, 1000);
    return () => window.clearTimeout(timer);
  }, [direct?.enabled, direct?.pairingCode, direct?.pairingExpiresIn]);

  useEffect(() => {
    return () => {
      if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    };
  }, []);

  const dirty =
    allowControl !== saved.allowControl ||
    autoStart !== saved.autoStart ||
    Number(zoom) !== saved.zoom ||
    animations !== saved.animations ||
    theme !== saved.theme;

  async function saveChanges() {
    if (saving) return;
    setSaving(true);
    try {
      const [nextAllow, nextZoom] = await Promise.all([
        setAllowControl(allowControl),
        setUiZoom(Number(zoom) / 100),
      ]);
      await setAutostart(autoStart);
      applyMotion(animations);
      applyTheme(theme);
      setSaved({
        allowControl: nextAllow,
        autoStart,
        zoom: Math.round(nextZoom * 100),
        animations,
        theme,
      });
      notify("设置已保存", "success");
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setSaving(false);
    }
  }

  /** 无头虚拟显示器：即时生效，失败时回滚开关。 */
  async function toggleVirtualDisplay(next: boolean) {
    if (virtualDisplayBusy) return;
    const previous = virtualDisplay;
    setVirtualDisplayState(next);
    setVirtualDisplayBusy(true);
    try {
      setVirtualDisplayState(await setVirtualDisplay(next));
      notify(
        next ? "无头时会自动接入虚拟显示器" : "已关闭无头虚拟显示器",
        "success",
      );
    } catch (error) {
      setVirtualDisplayState(previous);
      notify(errorLine(error), "error");
    } finally {
      setVirtualDisplayBusy(false);
    }
  }

  /**
   * 应用自定义 CA。与直连开关一样不经过「保存更改」：路径写错要当场报错，
   * 而不是等连接服务器时才冒出一个看不懂的证书错误。
   */
  async function applyCa() {
    if (caBusy) return;
    setCaBusy(true);
    setCaError(null);
    try {
      const saved = await setCaPath(caPath.trim() || null);
      setCaPathState(saved ?? "");
      notify(
        saved
          ? "已应用自定义 CA，重新连接服务器后生效"
          : "已恢复默认信任库，重新连接服务器后生效",
        "success",
      );
    } catch (error) {
      setCaError(errorLine(error));
    } finally {
      setCaBusy(false);
    }
  }

  async function testConnection() {
    if (test.kind === "testing") return;
    setTest({ kind: "testing" });
    try {
      const ms = await testServer(serverUrl);
      setTest({ kind: "ok", ms });
      notify(`服务器可用，往返 ${ms} ms`, "success");
    } catch (error) {
      const message = errorLine(error);
      setTest({ kind: "error", message });
      notify(message, "error");
    }
  }

  /**
   * 直连开关不经过「保存更改」：它会立刻开始/停止监听端口，
   * 与外观这类只影响本机界面的偏好不同，拖到点保存才生效会让用户困惑。
   * 因此这里直接调后端，并用返回值覆盖本地状态。
   */
  async function toggleDirect(enabled: boolean) {
    if (directBusy) return;
    setDirectBusy(true);
    setDirectError(null);
    try {
      const state = await setDirectEnabled(enabled);
      setDirect(state);
      notify(
        enabled
          ? state.listening
            ? "局域网直连已开启"
            : "局域网直连已开启，但端口监听失败"
          : "局域网直连已关闭",
        enabled && !state.listening ? "error" : "success",
      );
    } catch (error) {
      const message = errorLine(error);
      setDirectError(message);
      notify(message, "error");
    } finally {
      setDirectBusy(false);
    }
  }

  /**
   * 补防火墙规则。perMachine 安装时安装器已经加好；这里覆盖 currentUser
   * 安装、规则被删、程序换过目录三种情况，点击后会弹一次 UAC。
   */
  async function repairFirewall() {
    if (firewallBusy) return;
    setFirewallBusy(true);
    setDirectError(null);
    try {
      const state = await allowDirectFirewall();
      setFirewall(state);
      notify(
        state.ruleMatches
          ? "防火墙规则已添加，同网段设备可以直连了"
          : "仍未检测到防火墙规则，请检查系统防火墙或组策略",
        state.ruleMatches ? "success" : "error",
      );
    } catch (error) {
      const message = errorLine(error);
      setDirectError(message);
      notify(message, "error");
    } finally {
      setFirewallBusy(false);
    }
  }

  async function regeneratePairing() {
    if (pairingBusy) return;
    setPairingBusy(true);
    setDirectError(null);
    try {
      const state = await regeneratePairingCode();
      setDirect(state);
      notify("已生成新的配对码", "success");
    } catch (error) {
      const message = errorLine(error);
      setDirectError(message);
      notify(message, "error");
    } finally {
      setPairingBusy(false);
    }
  }

  async function forgetPeers() {
    if (forgetBusy) return;
    setForgetBusy(true);
    setDirectError(null);
    try {
      const state = await forgetDirectPeers();
      setDirect(state);
      setForgetOpen(false);
      notify("已清除全部直连授权，对方需要重新输入配对码", "success");
    } catch (error) {
      const message = errorLine(error);
      setDirectError(message);
      notify(message, "error");
    } finally {
      setForgetBusy(false);
    }
  }

  async function copyDeviceId() {
    const value = account.deviceId;
    if (!value) return;
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      notify("本机设备 ID 已复制", "success");
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
      copiedTimer.current = window.setTimeout(() => {
        setCopied(false);
        copiedTimer.current = null;
      }, 2000);
    } catch {
      notify("复制失败，请手动选择设备 ID", "error");
    }
  }

  function confirmLogout() {
    setLogoutOpen(false);
    onLogout();
  }

  async function runCheckUpdate() {
    if (update.kind === "checking") return;
    setUpdate({ kind: "checking" });
    try {
      const info = await checkUpdate();
      if (info) {
        setUpdate({ kind: "available", info });
      } else {
        setUpdate({ kind: "latest" });
        notify("已是最新版本", "success");
      }
    } catch (error) {
      const message = errorLine(error);
      setUpdate({ kind: "error", message });
      notify(message, "error");
    }
  }

  async function runDownloadAndInstall(info: UpdateInfo) {
    if (download.kind === "downloading" || download.kind === "installing") return;
    installRef.current = false;
    setDownload({ kind: "downloading", received: 0, total: 0 });
    let path: string;
    try {
      path = await downloadUpdate(info.url, info.sha256);
    } catch (error) {
      const message = errorLine(error);
      setDownload({ kind: "error", message });
      notify(message, "error");
      return;
    }
    installRef.current = true;
    setDownload({ kind: "installing" });
    try {
      await installUpdate(path);
      notify("安装程序已启动，应用即将退出", "success");
    } catch (error) {
      installRef.current = false;
      const message = errorLine(error);
      setDownload({ kind: "error", message });
      notify(message, "error");
    }
  }

  return (
    <div className="settings-page">
      <div className="set-layout">
        <nav className="set-nav" aria-label="设置分区">
          {SECTIONS.map((section) => {
            const Icon = section.icon;
            const active = activeSection === section.key;
            return (
              <button
                key={section.key}
                type="button"
                className={`set-nav-item ${active ? "is-active" : ""}`}
                aria-current={active ? "page" : undefined}
                onClick={() => setActiveSection(section.key)}
              >
                <Icon size={16} />
                <span>{section.label}</span>
              </button>
            );
          })}
        </nav>

        <div className="set-content">
          {activeSection === "account" && (
            <div className="set-section">
              <section className="set-group" aria-labelledby="set-account">
                <h3 className="set-group-title" id="set-account">
                  账号信息
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">用户名</span>
                      <span className="set-row-hint">
                        登录同一账号的设备会自动加入设备列表
                      </span>
                    </div>
                    <div className="set-row-value">
                      <span>{account.username || "未登录"}</span>
                      <span
                        className={`badge ${
                          account.connected ? "" : "badge-amber"
                        }`}
                      >
                        {account.connected ? "已登录" : "连接中"}
                      </span>
                    </div>
                  </div>

                  <div className="set-row">
                    <span className="set-row-copy">
                      <span className="set-row-label">服务器地址</span>
                    </span>
                    <code className="set-code">{account.serverUrl}</code>
                  </div>

                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">设备 ID</span>
                      <span className="set-row-hint">
                        用于在另一台设备上核对账号归属，请勿公开分享
                      </span>
                    </div>
                    <div className="set-inline-control">
                      <code className="set-code set-code-wide">
                        {account.deviceId || "—"}
                      </code>
                      <button
                        type="button"
                        className="icon-btn"
                        onClick={copyDeviceId}
                        disabled={!account.deviceId}
                        aria-label="复制设备 ID"
                        title={copied ? "已复制" : "复制设备 ID"}
                      >
                        {copied ? <Check size={15} /> : <Copy size={15} />}
                      </button>
                    </div>
                  </div>
                </div>
              </section>

              <section className="set-group" aria-labelledby="set-session">
                <h3 className="set-group-title" id="set-session">
                  登录状态
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">退出登录</span>
                      <span className="set-row-hint">
                        退出后本机立即断开连接，远程会话随之结束
                      </span>
                    </div>
                    <button
                      type="button"
                      className="btn-text-danger"
                      onClick={() => setLogoutOpen(true)}
                      disabled={busy}
                    >
                      {busy ? (
                        <Loader2 size={15} className="spin" />
                      ) : (
                        <LogOut size={15} />
                      )}
                      {busy ? "正在退出…" : "退出登录"}
                    </button>
                  </div>
                </div>
              </section>
            </div>
          )}

          {activeSection === "connection" && (
            <div className="set-section">
              {account.error && (
                <p className="notice notice-error set-error" role="alert">
                  {account.error}
                </p>
              )}

              <section className="set-group" aria-labelledby="set-relay">
                <h3 className="set-group-title" id="set-relay">
                  中转连接
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">中转状态</span>
                      <span className="set-row-hint">
                        所有设备通过这台服务器保持在线
                      </span>
                    </div>
                    <span
                      className={`set-status ${
                        account.connected ? "is-on" : ""
                      }`}
                    >
                      <i aria-hidden="true" />
                      {account.connected ? "已连接" : "连接中"}
                    </span>
                  </div>

                  <div className="set-field">
                    <span className="set-field-label">服务器地址</span>
                    <div className="set-server-row">
                      <div className="field-control">
                        <Server size={15} />
                        <input
                          value={serverUrl}
                          onChange={(event) =>
                            setServerUrl(event.target.value)
                          }
                          placeholder="http://127.0.0.1:8787"
                          spellCheck={false}
                          autoComplete="off"
                          aria-label="服务器地址"
                        />
                      </div>
                      <button
                        type="button"
                        className={`btn set-test ${
                          test.kind === "ok" ? "btn-soft" : "btn-ghost"
                        }`}
                        onClick={testConnection}
                        disabled={test.kind === "testing"}
                      >
                        {test.kind === "testing" ? (
                          <>
                            <Loader2 size={14} className="spin" />
                            测试中…
                          </>
                        ) : test.kind === "ok" ? (
                          <>
                            <Check size={14} />
                            {test.ms} ms
                          </>
                        ) : (
                          <>
                            <RefreshCw size={14} />
                            测试连接
                          </>
                        )}
                      </button>
                    </div>
                    {test.kind === "error" ? (
                      <p className="set-field-hint set-field-error" role="alert">
                        {test.message}
                      </p>
                    ) : (
                      <p className="set-field-hint">
                        改地址需要重新登录才生效，当前会话不会自动迁移。
                      </p>
                    )}
                  </div>

                  <div className="set-field">
                    <span className="set-field-label">自定义 CA（可选）</span>
                    <div className="set-server-row">
                      <div className="field-control">
                        <ShieldCheck size={15} />
                        <input
                          value={caPath}
                          onChange={(event) => {
                            setCaPathState(event.target.value);
                            if (caError) setCaError(null);
                          }}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") void applyCa();
                          }}
                          placeholder="C:\certs\ca.pem 或证书目录"
                          spellCheck={false}
                          autoComplete="off"
                          aria-label="自定义 CA 路径"
                        />
                      </div>
                      <button
                        type="button"
                        className="btn btn-ghost set-test"
                        onClick={() => void applyCa()}
                        disabled={caBusy}
                      >
                        {caBusy ? (
                          <>
                            <Loader2 size={14} className="spin" />
                            应用中…
                          </>
                        ) : (
                          "应用"
                        )}
                      </button>
                    </div>
                    {caError ? (
                      <p className="set-field-hint set-field-error" role="alert">
                        {caError}
                      </p>
                    ) : (
                      <p className="set-field-hint">
                        自建服务器用私有 CA 签证书时填这里；配置后只信任这个 CA，
                        留空恢复系统默认。保存后重新连接服务器才生效。
                      </p>
                    )}
                  </div>
                </div>
              </section>

              <section className="set-group" aria-labelledby="set-direct">
                <h3 className="set-group-title" id="set-direct">
                  局域网直连
                </h3>
                <div className="set-rows">
                  <Switch
                    label="开启局域网直连"
                    hint="同网段的电脑输入这个配对码即可直连本机，不需要登录账号"
                    checked={direct?.enabled ?? false}
                    disabled={direct === null || directBusy}
                    onChange={(value) => void toggleDirect(value)}
                  />

                  {direct?.enabled && (
                    <>
                      <div className="set-row">
                        <div className="set-row-copy">
                          <span className="set-row-label">本机地址</span>
                          <span className="set-row-hint">
                            对方在「设备」页点「直连」后填这里的地址
                          </span>
                        </div>
                        <div className="set-direct-addresses">
                          {direct.addresses.length > 0 ? (
                            direct.addresses.map((address) => (
                              <code className="set-code" key={address}>
                                {address}:{direct.port}
                              </code>
                            ))
                          ) : (
                            <span className="set-row-value">未检测到局域网地址</span>
                          )}
                        </div>
                      </div>

                      <div className="set-row">
                        <div className="set-row-copy">
                          <span className="set-row-label">监听端口</span>
                          <span className="set-row-hint">
                            默认端口，防火墙只对专用网络放行
                          </span>
                        </div>
                        <span className="set-row-value mono">{direct.port}</span>
                      </div>

                      <div className="set-row">
                        <div className="set-row-copy">
                          <span className="set-row-label">当前会话</span>
                          <span className="set-row-hint">
                            正在通过直连控制本机的会话数
                          </span>
                        </div>
                        <span className="set-row-value">
                          {direct.activeSessions} 个
                        </span>
                      </div>

                      <div className="set-row">
                        <div className="set-row-copy">
                          <span className="set-row-label">已授权设备</span>
                          <span className="set-row-hint">
                            配对成功过的设备可以免配对码直连本机；清除后都要重新输码
                          </span>
                        </div>
                        <div className="set-direct-pair-body">
                          <span className="set-row-value">
                            {direct.trustedPeers} 台
                          </span>
                          <button
                            type="button"
                            className="btn btn-ghost btn-sm"
                            onClick={() => setForgetOpen(true)}
                            disabled={direct.trustedPeers === 0 || forgetBusy}
                          >
                            <ShieldCheck size={14} />
                            清除授权
                          </button>
                        </div>
                      </div>

                      {!direct.listening && (
                        <div className="set-direct-alert">
                          <p className="notice notice-warn" role="alert">
                            <TriangleAlert size={15} />
                            <span>
                              端口被占用或监听失败，局域网直连暂时不可用。
                              关闭占用 {direct.port} 端口的程序，或关闭后重新开启直连。
                            </span>
                          </p>
                        </div>
                      )}

                      {firewall && !firewall.ruleMatches && (
                        <div className="set-direct-alert set-direct-firewall">
                          <p className="notice notice-warn" role="alert">
                            <TriangleAlert size={15} />
                            <span>
                              Windows 防火墙还没有放行本程序（{firewall.ruleName}，
                              TCP {firewall.port}）。此时端口虽然开着，同网段的电脑却会连不上。
                              点右侧按钮补充规则，会弹出一次管理员授权（UAC）。
                            </span>
                          </p>
                          <button
                            type="button"
                            className="btn btn-ghost btn-sm"
                            onClick={() => void repairFirewall()}
                            disabled={firewallBusy}
                          >
                            {firewallBusy ? (
                              <Loader2 size={14} className="spin" />
                            ) : (
                              <ShieldCheck size={14} />
                            )}
                            {firewallBusy ? "等待授权…" : "允许局域网直连"}
                          </button>
                        </div>
                      )}

                      <div className="set-direct-pair">
                        <div className="set-row-copy">
                          <span className="set-row-label">配对码</span>
                          <span className="set-row-hint">
                            对方首次连接时输入，10 分钟内有效，配对成功后自动更换
                          </span>
                        </div>
                        <div className="set-direct-pair-body">
                          <div className="set-pairing">
                            <code className="set-pairing-code" aria-label="配对码">
                              {direct.pairingCode ?? "------"}
                            </code>
                            <span className="set-pairing-time">
                              {direct.pairingCode
                                ? `剩余 ${formatPairingTime(
                                    direct.pairingExpiresIn,
                                  )}`
                                : "已失效"}
                            </span>
                          </div>
                          <button
                            type="button"
                            className="btn btn-ghost btn-sm"
                            onClick={() => void regeneratePairing()}
                            disabled={pairingBusy}
                          >
                            {pairingBusy ? (
                              <Loader2 size={14} className="spin" />
                            ) : (
                              <RefreshCw size={14} />
                            )}
                            {pairingBusy ? "生成中…" : "重新生成"}
                          </button>
                        </div>
                      </div>

                    </>
                  )}

                  {directError && (
                    <div className="set-field">
                      <p className="set-field-hint set-field-error" role="alert">
                        {directError}
                      </p>
                    </div>
                  )}
                </div>
              </section>

              <section className="set-group" aria-labelledby="set-startup">
                <h3 className="set-group-title" id="set-startup">
                  后台与启动
                </h3>
                <div className="set-rows">
                  <Switch
                    label="开机自启动"
                    hint="登录 Windows 后自动启动 ScreenWall（写入当前用户启动项）"
                    checked={autoStart}
                    onChange={setAutoStartState}
                  />
                </div>
              </section>

              <DiagnosticsCard notify={notify} />

              <section className="set-group" aria-labelledby="set-appearance">
                <h3 className="set-group-title" id="set-appearance">
                  外观
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">主题</span>
                      <span className="set-row-hint">
                        浅色适合白天，深色适合长时间看画面
                      </span>
                    </div>
                    <Segmented
                      value={theme}
                      options={[
                        { value: "light", label: "浅色" },
                        { value: "dark", label: "深色" },
                      ]}
                      onChange={(value) => {
                        setThemeState(value);
                        // 主题是即时预览的，点「保存更改」才写回存储。
                        applyTheme(value, false);
                      }}
                    />
                  </div>
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">界面缩放</span>
                      <span className="set-row-hint">
                        适合高分辨率屏幕的显示比例，保存后立即生效
                      </span>
                    </div>
                    <span className="set-select">
                      <select
                        value={zoom}
                        onChange={(event) => setZoom(event.target.value)}
                        aria-label="界面缩放"
                      >
                        {ZOOM_OPTIONS.map((option) => (
                          <option key={option.value} value={option.value}>
                            {option.label}
                          </option>
                        ))}
                      </select>
                      <ChevronDown size={14} />
                    </span>
                  </div>
                  <Switch
                    label="动画效果"
                    hint="页面切换与控件过渡动画"
                    checked={animations}
                    onChange={setAnimationsState}
                  />
                </div>
              </section>
            </div>
          )}

          {activeSection === "security" && (
            <div className="set-section">
              <section className="set-group" aria-labelledby="set-approval">
                <h3 className="set-group-title" id="set-approval">
                  被控授权
                </h3>
                <div className="set-rows">
                  <Switch
                    label="允许被控"
                    hint="关闭后任何设备都无法连接本机，包括同账号设备"
                    checked={allowControl}
                    onChange={setAllowControlState}
                  />
                  <Switch
                    label="无头时接入虚拟显示器"
                    hint="本机没有任何显示器时，用 usbmmidd_v2（Amyuni 签名的间接显示驱动）临时接一块 1920×1080 虚拟屏再采集。首次使用需要一次管理员授权，驱动可能弹一次广告页"
                    checked={virtualDisplay}
                    disabled={virtualDisplayBusy}
                    onChange={(next) => void toggleVirtualDisplay(next)}
                  />
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">授权方式</span>
                      <span className="set-row-hint">
                        每次请求都会在本机弹窗，30 秒未确认自动拒绝
                      </span>
                    </div>
                    <span className="set-row-value">每次确认</span>
                  </div>
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">允许来源</span>
                      <span className="set-row-hint">
                        只有同一账号下的设备能发起控制请求，中转服务器不允许跨账号转发
                      </span>
                    </div>
                    <span className="set-row-value">同账号设备</span>
                  </div>
                </div>
              </section>
            </div>
          )}

          {activeSection === "about" && (
            <div className="set-section">
              <section className="set-group" aria-labelledby="set-app-info">
                <h3 className="set-group-title" id="set-app-info">
                  应用信息
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">ScreenWall</span>
                      <span className="set-row-hint">远程工作台</span>
                    </div>
                    <span className="badge">稳定版</span>
                  </div>
                  <div className="set-row">
                    <span className="set-row-copy">
                      <span className="set-row-label">版本</span>
                    </span>
                    <span className="set-row-value">{version}</span>
                  </div>
                  <div className="set-row">
                    <span className="set-row-copy">
                      <span className="set-row-label">构建</span>
                    </span>
                    <span className="set-row-value">2026.11 · Windows x64</span>
                  </div>
                  <div className="set-row">
                    <span className="set-row-copy">
                      <span className="set-row-label">协议版本</span>
                    </span>
                    <span className="set-row-value">SWP/1 · 中转 + P2P</span>
                  </div>
                </div>
              </section>

              <section className="set-group" aria-labelledby="set-update">
                <h3 className="set-group-title" id="set-update">
                  软件更新
                </h3>
                <div className="set-rows">
                  <div className="set-row">
                    <div className="set-row-copy">
                      <span className="set-row-label">当前版本</span>
                      <span className="set-row-hint">
                        {update.kind === "latest"
                          ? "已是最新版本"
                          : "从自建服务器检查并安装新版本"}
                      </span>
                    </div>
                    <div className="set-inline-control">
                      <span className="set-row-value">{version}</span>
                      <button
                        type="button"
                        className="btn btn-ghost set-test"
                        onClick={runCheckUpdate}
                        disabled={
                          update.kind === "checking" ||
                          download.kind === "downloading" ||
                          download.kind === "installing"
                        }
                      >
                        {update.kind === "checking" ? (
                          <>
                            <Loader2 size={14} className="spin" />
                            检查中…
                          </>
                        ) : (
                          <>
                            <RefreshCw size={14} />
                            检查更新
                          </>
                        )}
                      </button>
                    </div>
                  </div>

                  {update.kind === "error" && (
                    <div className="set-field">
                      <p className="set-field-hint set-field-error" role="alert">
                        {update.message}
                      </p>
                    </div>
                  )}

                  {update.kind === "available" && (
                    <div className="set-update-panel">
                      <div className="set-update-head">
                        <div className="set-row-copy">
                          <span className="set-row-label">
                            发现新版本 {update.info.version}
                          </span>
                          <span className="set-row-hint">
                            当前 {version}
                            {formatPublished(update.info.publishedAt)
                              ? ` · 发布于 ${formatPublished(update.info.publishedAt)}`
                              : ""}
                          </span>
                        </div>
                        <span className="badge badge-amber">可更新</span>
                      </div>

                      {update.info.notes.trim() && (
                        <p className="set-update-notes">{update.info.notes}</p>
                      )}

                      {download.kind === "downloading" ? (
                        <div className="set-update-progress">
                          <Progress
                            value={
                              download.total > 0
                                ? (download.received / download.total) * 100
                                : 0
                            }
                            tone="blue"
                          />
                          <span className="set-update-progress-text">
                            {download.total > 0
                              ? `${formatBytes(download.received)} / ${formatBytes(
                                  download.total,
                                )}`
                              : `${formatBytes(download.received)}（大小未知）`}
                          </span>
                        </div>
                      ) : download.kind === "installing" ? (
                        <p className="set-field-hint">
                          安装程序已启动，ScreenWall 即将退出并完成更新。
                        </p>
                      ) : (
                        <div className="set-update-actions">
                          <button
                            type="button"
                            className="btn btn-primary"
                            onClick={() => runDownloadAndInstall(update.info)}
                          >
                            <Download size={15} />
                            下载并安装
                          </button>
                          {download.kind === "error" && (
                            <p
                              className="set-field-hint set-field-error"
                              role="alert"
                            >
                              {download.message}
                            </p>
                          )}
                        </div>
                      )}
                    </div>
                  )}
                </div>
              </section>

              <p className="set-about-foot">
                <Palette size={14} />
                界面遵循 ScreenWall 设计系统，设置仅保存在本机。
              </p>
            </div>
          )}
        </div>
      </div>

      <div className="set-actions">
        <span className="set-dirty" aria-live="polite">
          {dirty ? "有未保存的更改" : "所有更改已保存"}
        </span>
        <button
          type="button"
          className="btn btn-primary"
          onClick={saveChanges}
          disabled={saving}
        >
          {saving ? (
            <Loader2 size={15} className="spin" />
          ) : (
            <Check size={15} />
          )}
          {saving ? "保存中…" : "保存更改"}
        </button>
      </div>

      <Modal
        open={logoutOpen}
        onClose={() => setLogoutOpen(false)}
        title="退出登录"
        subtitle={`账号：${account.username || "未登录"}`}
        width={420}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              onClick={() => setLogoutOpen(false)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={confirmLogout}
              disabled={busy}
            >
              {busy ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <LogOut size={14} />
              )}
              {busy ? "正在退出…" : "退出登录"}
            </button>
          </>
        }
      >
        <p className="set-confirm-text">
          退出后本机将断开中转服务器连接，远程会话立即结束。再次使用需要重新登录账号。
        </p>
      </Modal>

      <Modal
        open={forgetOpen}
        onClose={() => setForgetOpen(false)}
        title="清除直连授权"
        subtitle={`已授权 ${direct?.trustedPeers ?? 0} 台设备`}
        width={420}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              onClick={() => setForgetOpen(false)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={() => void forgetPeers()}
              disabled={forgetBusy}
            >
              {forgetBusy ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <ShieldCheck size={14} />
              )}
              {forgetBusy ? "清除中…" : "清除授权"}
            </button>
          </>
        }
      >
        <p className="set-confirm-text">
          清除后，所有配对过的设备都要重新输入配对码才能直连本机。正在进行的直连会话不受影响。
        </p>
      </Modal>
    </div>
  );
}
