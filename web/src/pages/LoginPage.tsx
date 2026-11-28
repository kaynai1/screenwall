import { Loader2, Lock, Server, User, Wifi } from "lucide-react";
import { useState } from "react";
import type * as React from "react";
import {
  ApiError,
  login,
  normalizeServerUrl,
  quickConnect,
  register,
  type Account,
} from "../api";
import { describeHttp, describeThrown, type ErrorCopy } from "../errors";

type Mode = "login" | "register" | "code";

const SERVER_KEY = "screenwall.web.server";
const USER_KEY = "screenwall.web.username";

/** 安卓壳（Capacitor）里页面源是 `https://localhost`，不能拿它当服务器地址。 */
function isNativeShell(): boolean {
  const cap = (window as Window & { Capacitor?: { isNativePlatform?: () => boolean } }).Capacitor;
  return Boolean(cap?.isNativePlatform?.());
}

function defaultServer(): string {
  const saved = localStorage.getItem(SERVER_KEY);
  if (saved) return saved;
  if (isNativeShell()) return "";
  // 永远用页面自己的源：正式环境由账号服务器托管，开发环境由 Vite
  // 代理 `/api` 与 `/ws` 到本机服务器；写死 127.0.0.1:8787 会变成跨域请求，
  // 而服务器没有 CORS，登录会直接失败。
  return window.location.origin;
}

export default function LoginPage({ onAccount }: { onAccount: (account: Account) => void }) {
  const [mode, setMode] = useState<Mode>("login");
  const [server, setServer] = useState(defaultServer);
  const [username, setUsername] = useState(() => localStorage.getItem(USER_KEY) ?? "");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ErrorCopy | null>(null);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    if (!server.trim()) {
      setBusy(false);
      setError({ title: "请先填服务器地址", detail: "例如 https://你的服务器:8787" });
      return;
    }
    const serverUrl = normalizeServerUrl(server);
    try {
      if (mode === "code") {
        const account = await quickConnect(serverUrl, code, password);
        remember(serverUrl, "");
        onAccount(account);
      } else if (mode === "register") {
        await register(serverUrl, username.trim(), password);
        // 注册返回的令牌没有绑定设备，再登录一次拿到完整会话。
        const account = await login(serverUrl, username.trim(), password);
        remember(serverUrl, username.trim());
        onAccount(account);
      } else {
        const account = await login(serverUrl, username.trim(), password);
        remember(serverUrl, username.trim());
        onAccount(account);
      }
    } catch (thrown) {
      if (thrown instanceof ApiError) setError(describeHttp(thrown.code, thrown.message));
      else setError(describeThrown(thrown));
    } finally {
      setBusy(false);
    }
  }

  function remember(serverUrl: string, name: string) {
    try {
      localStorage.setItem(SERVER_KEY, serverUrl);
      if (name) localStorage.setItem(USER_KEY, name);
    } catch {
      // 忽略存储失败。
    }
  }

  return (
    <div className="login-page">
      <div className="login-card">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true">
            <span />
            <span />
            <span />
          </div>
          <div>
            <h1>ScreenWall</h1>
            <p>网页 / 手机远程控制</p>
          </div>
        </div>

        <div className="segmented" role="tablist">
          <button
            type="button"
            className={mode === "login" ? "is-active" : ""}
            onClick={() => setMode("login")}
          >
            账号登录
          </button>
          <button
            type="button"
            className={mode === "code" ? "is-active" : ""}
            onClick={() => setMode("code")}
          >
            识别码
          </button>
          <button
            type="button"
            className={mode === "register" ? "is-active" : ""}
            onClick={() => setMode("register")}
          >
            注册
          </button>
        </div>

        <form onSubmit={submit} className="login-form">
          <label className="field">
            <span>服务器</span>
            <div className="input-wrap">
              <Server size={16} />
              <input
                value={server}
                onChange={(event) => setServer(event.target.value)}
                placeholder="https://screenwall.example.com"
                inputMode="url"
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
              />
            </div>
          </label>

          {mode === "code" ? (
            <label className="field">
              <span>9 位识别码</span>
              <div className="input-wrap">
                <Wifi size={16} />
                <input
                  value={code}
                  onChange={(event) => setCode(event.target.value.replace(/\D/g, "").slice(0, 9))}
                  placeholder="123456789"
                  inputMode="numeric"
                  autoComplete="off"
                />
              </div>
            </label>
          ) : (
            <label className="field">
              <span>用户名</span>
              <div className="input-wrap">
                <User size={16} />
                <input
                  value={username}
                  onChange={(event) => setUsername(event.target.value)}
                  placeholder="用户名"
                  autoCapitalize="off"
                  autoCorrect="off"
                  autoComplete="username"
                />
              </div>
            </label>
          )}

          <label className="field">
            <span>{mode === "code" ? "连接密码" : "密码"}</span>
            <div className="input-wrap">
              <Lock size={16} />
              <input
                type="password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                placeholder={mode === "register" ? "至少 8 位" : "密码"}
                autoComplete={mode === "register" ? "new-password" : "current-password"}
              />
            </div>
          </label>

          {error && (
            <div className="error-box">
              <strong>{error.title}</strong>
              <span>{error.detail}</span>
            </div>
          )}

          <button
            type="submit"
            className="primary-btn"
            disabled={busy || !password || (mode !== "code" && !username.trim()) || (mode === "code" && code.length !== 9)}
          >
            {busy ? <Loader2 size={18} className="spin" /> : null}
            {mode === "code" ? "直接连接" : mode === "register" ? "注册并登录" : "登录"}
          </button>
        </form>

        <p className="login-hint">
          服务器填部署 ScreenWall 账号服务的地址；被控电脑需要先登录同一账号并保持在线。
        </p>
      </div>
    </div>
  );
}
