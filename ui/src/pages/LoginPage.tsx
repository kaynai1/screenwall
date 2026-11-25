import { useEffect, useState, type FormEvent } from "react";
import {
  ArrowRight,
  Eye,
  EyeOff,
  FolderSync,
  Hash,
  KeyRound,
  LayoutGrid,
  Loader2,
  LockKeyhole,
  MousePointerClick,
  Server,
  UserRound,
  Wifi,
} from "lucide-react";
import type { AccountView } from "../types";
import { isTauri } from "../api";
import WindowChrome from "../components/WindowChrome";

const DEFAULT_SERVER = "http://127.0.0.1:8787";

const FACTS = [
  {
    icon: LayoutGrid,
    title: "屏幕墙",
    text: "局域网里的设备直接连，实时预览每一块屏幕。",
  },
  {
    icon: MousePointerClick,
    title: "键鼠直通",
    text: "远程画面与本地操作同步，支持键盘、鼠标和组合键。",
  },
  {
    icon: FolderSync,
    title: "文件传输",
    text: "在两台设备之间收发文件，不需要额外的网盘。",
  },
];

export default function LoginPage({
  account,
  busy,
  error,
  onLogin,
  onQuickConnect,
  onLocalMode,
}: {
  account: AccountView;
  busy: boolean;
  error: string | null;
  onLogin: (input: {
    serverUrl: string;
    username: string;
    password: string;
    register: boolean;
  }) => void;
  onQuickConnect: (input: {
    serverUrl: string;
    code: string;
    password: string;
  }) => void;
  onLocalMode: () => void;
}) {
  const [mode, setMode] = useState<"login" | "register" | "code">("login");
  const [serverUrl, setServerUrl] = useState(account.serverUrl || DEFAULT_SERVER);
  const [username, setUsername] = useState(account.username || "");
  const [password, setPassword] = useState("");
  const [showPassword, setShowPassword] = useState(false);
  const [connectCode, setConnectCode] = useState("");
  const [connectPassword, setConnectPassword] = useState("");

  useEffect(() => {
    if (account.serverUrl) setServerUrl(account.serverUrl);
    if (account.username) setUsername(account.username);
  }, [account.serverUrl, account.username]);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    if (mode === "code") {
      onQuickConnect({
        serverUrl,
        code: connectCode,
        password: connectPassword,
      });
      return;
    }
    onLogin({
      serverUrl,
      username,
      password,
      register: mode === "register",
    });
  }

  return (
    <div className="login-screen">
      <WindowChrome />
      <aside className="login-aside" data-tauri-drag-region>
        <div className="login-brand">
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

        <div className="login-intro">
          <h1>把每台电脑，放在同一张桌上。</h1>
          <p>
            同一网段的两台机器直接互连，不经过任何中转服务器。屏幕墙、群控、
            文件传输和远程控制，都收在一个窗口里。
          </p>

          <ul className="login-facts">
            {FACTS.map((fact) => {
              const Icon = fact.icon;
              return (
                <li key={fact.title}>
                  <Icon size={16} strokeWidth={1.75} />
                  <div>
                    <strong>{fact.title}</strong>
                    <span>{fact.text}</span>
                  </div>
                </li>
              );
            })}
          </ul>
        </div>

        <div className="login-aside-foot">
          局域网直连不需要服务器 · 走服务器时数据只在你和自建服务器之间
        </div>
      </aside>

      <main className="login-main">
        <div className="login-local">
          <div className="login-local-copy">
            <strong>
              <Wifi size={16} strokeWidth={1.75} />
              不用服务器，直接连
            </strong>
            <p>
              两台电脑在同一网段时，一台开直连、另一台输地址和 6 位配对码就能互连。
              没有账号，画面不经过任何中转服务器。
            </p>
          </div>
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy}
            onClick={onLocalMode}
          >
            进入局域网模式
            <ArrowRight size={15} strokeWidth={1.75} />
          </button>
        </div>

        <div className="login-or">
          <span>或者用账号 / 连接码走服务器</span>
        </div>

        <form className="login-form" onSubmit={submit}>
          <div className="login-form-head">
            <h2>
              {mode === "code"
                ? "连接码连接"
                : mode === "register"
                  ? "创建账号"
                  : "登录"}
            </h2>
            <p>
              {mode === "code"
                ? "输入对方设备显示的 9 位连接码和密码，不需要同一个账号。"
                : mode === "register"
                  ? "注册后，这台电脑会自动加入你的设备列表。"
                  : "使用账号连接同服务器下的所有设备。"}
            </p>
          </div>

          <div className="login-tabs" role="tablist" aria-label="连接方式">
            <button
              type="button"
              role="tab"
              aria-selected={mode === "login"}
              className={mode === "login" ? "is-active" : ""}
              onClick={() => setMode("login")}
            >
              登录
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={mode === "register"}
              className={mode === "register" ? "is-active" : ""}
              onClick={() => setMode("register")}
            >
              注册
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={mode === "code"}
              className={mode === "code" ? "is-active" : ""}
              onClick={() => setMode("code")}
            >
              连接码连接
            </button>
          </div>

          <details className="login-server">
            <summary>
              <Server size={14} strokeWidth={1.75} />
              服务器地址
              <span className="login-server-value">{serverUrl || DEFAULT_SERVER}</span>
            </summary>
            <div className="login-server-body">
              <div className="field-control">
                <Server size={15} strokeWidth={1.75} />
                <input
                  className="mono"
                  value={serverUrl}
                  onChange={(event) => setServerUrl(event.target.value)}
                  placeholder={DEFAULT_SERVER}
                  spellCheck={false}
                  autoComplete="off"
                  aria-label="服务器地址"
                />
              </div>
              <span className="field-hint">
                只有走账号或连接码时才需要。局域网直连不用填。
              </span>
            </div>
          </details>

          {mode === "code" ? (
            <>
              <label className="field">
                <span>连接码或连接链接</span>
                <div className="field-control">
                  <Hash size={15} strokeWidth={1.75} />
                  <input
                    className="mono"
                    value={connectCode}
                    onChange={(event) => setConnectCode(event.target.value)}
                    placeholder="9 位数字，或直接粘贴连接链接"
                    spellCheck={false}
                    autoComplete="off"
                  />
                </div>
                <span className="field-hint">
                  粘贴对方发来的链接（形如 http://…/l/…）可以跳过密码。
                </span>
              </label>

              <label className="field">
                <span>连接密码</span>
                <div className="field-control">
                  <KeyRound size={15} strokeWidth={1.75} />
                  <input
                    type={showPassword ? "text" : "password"}
                    value={connectPassword}
                    onChange={(event) => setConnectPassword(event.target.value)}
                    placeholder="对方设备设置的连接密码"
                    autoComplete="off"
                  />
                  <button
                    type="button"
                    className="field-eye"
                    onClick={() => setShowPassword((value) => !value)}
                    aria-label={showPassword ? "隐藏密码" : "显示密码"}
                  >
                    {showPassword ? (
                      <EyeOff size={15} strokeWidth={1.75} />
                    ) : (
                      <Eye size={15} strokeWidth={1.75} />
                    )}
                  </button>
                </div>
              </label>
            </>
          ) : (
            <>
              <label className="field">
                <span>用户名</span>
                <div className="field-control">
                  <UserRound size={15} strokeWidth={1.75} />
                  <input
                    value={username}
                    onChange={(event) => setUsername(event.target.value)}
                    placeholder="3–32 位字母、数字、下划线"
                    spellCheck={false}
                    autoComplete="username"
                  />
                </div>
              </label>

              <label className="field">
                <span>密码</span>
                <div className="field-control">
                  <LockKeyhole size={15} strokeWidth={1.75} />
                  <input
                    type={showPassword ? "text" : "password"}
                    value={password}
                    onChange={(event) => setPassword(event.target.value)}
                    placeholder="至少 8 位"
                    autoComplete={
                      mode === "register" ? "new-password" : "current-password"
                    }
                  />
                  <button
                    type="button"
                    className="field-eye"
                    onClick={() => setShowPassword((value) => !value)}
                    aria-label={showPassword ? "隐藏密码" : "显示密码"}
                  >
                    {showPassword ? (
                      <EyeOff size={15} strokeWidth={1.75} />
                    ) : (
                      <Eye size={15} strokeWidth={1.75} />
                    )}
                  </button>
                </div>
              </label>
            </>
          )}

          {error && <div className="notice notice-error">{error}</div>}

          {!isTauri && (
            <div className="login-demo-hint">
              浏览器预览模式：任意账号密码都可以进入演示界面。
            </div>
          )}

          <button type="submit" className="btn btn-primary login-submit" disabled={busy}>
            {busy ? (
              <>
                <Loader2 size={16} className="spin" />
                正在连接…
              </>
            ) : (
              <>
                {mode === "code"
                  ? "连接"
                  : mode === "register"
                    ? "注册并登录"
                    : "登录"}
                <ArrowRight size={15} strokeWidth={1.75} />
              </>
            )}
          </button>

          <p className="login-foot">
            {mode === "code" ? (
              <>
                连接码和密码由被控端在「设置 → 安全」里开启并查看。
                <br />
                密码验证通过后直接进入远程桌面，不需要账号。
              </>
            ) : (
              <>
                登录后，本机会在后台作为被控端保持在线。
                <br />
                不需要记 IP，也不需要额外安装组网软件。
              </>
            )}
          </p>
        </form>
      </main>
    </div>
  );
}
