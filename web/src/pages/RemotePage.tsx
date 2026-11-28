/**
 * 远程协助：控制别人的电脑。
 *
 * 网页端是纯浏览器，既不能被别人控制，也做不了 UDP 广播和局域网直连，
 * 所以这里只保留「我连别人」：9 位识别码先在账号里查（跨网段走中转），
 * 查不到再当连接码用密码换一条 15 分钟的受限会话。
 */

import {
  ArrowLeft,
  Hash,
  Link2,
  Loader2,
  Lock,
  Monitor,
  ShieldCheck,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  ApiError,
  quickConnect,
  resolvePeerId,
  Viewer,
  type Account,
  type DeviceView,
} from "../api";
import { describeHttp, describeThrown, type ErrorCopy } from "../errors";
import ControlPage from "./ControlPage";

/** 按 `436 910 774` 分组显示，方便照着对方屏幕上念。 */
function formatCode(digits: string): string {
  return digits.replace(/^(\d{3})(\d{3})(\d{3})$/, "$1 $2 $3");
}

export default function RemotePage({
  account,
  onBack,
  onOpenDevice,
  notify,
}: {
  account: Account;
  onBack: () => void;
  onOpenDevice: (device: DeviceView) => void;
  notify: (text: string) => void;
}) {
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<ErrorCopy | null>(null);
  /** 连接码换来的独立会话：不替换当前账号，只临时打开一条 Viewer。 */
  const [quick, setQuick] = useState<{ viewer: Viewer; device: DeviceView } | null>(null);
  const quickRef = useRef<Viewer | null>(null);

  // 页面被卸载（返回、退出登录）时把临时会话关掉。
  useEffect(
    () => () => {
      quickRef.current?.close();
      quickRef.current = null;
    },
    [],
  );

  async function connect() {
    if (busy) return;
    const digits = code.replace(/\D/g, "");
    if (digits.length !== 9) {
      setError({ title: "识别码不对", detail: "识别码是 9 位数字，在对方「远程协助」页顶部。" });
      return;
    }
    setBusy(true);
    setError(null);
    try {
      // 同一账号里先按识别码找（跨网段也能命中，会话走中转）。
      if (!account.quick) {
        setStatus("正在账号里查找这个识别码…");
        try {
          const found = await resolvePeerId(account, digits);
          setStatus(null);
          notify(`已找到 ${found.name}，正在打开控制页`);
          onOpenDevice({
            id: found.deviceId,
            name: found.name,
            os: "Windows",
            version: "",
            online: true,
            lastSeen: Date.now(),
            controllable: true,
          });
          return;
        } catch (cause) {
          // 账号里没有这个识别码时退到「连接码 + 密码」这条路。
          if (!(cause instanceof ApiError) || cause.code !== "id_not_found") throw cause;
        }
      }
      if (!password) {
        setStatus(null);
        setError({
          title: "这个识别码不在你的账号里",
          detail: "如果对方给你的是连接码，请填写对方显示的连接密码；否则请确认两台电脑登录的是同一个账号。",
        });
        return;
      }
      setStatus("正在用连接码建立会话…");
      const session = await quickConnect(account.serverUrl, digits, password);
      const viewer = new Viewer(session);
      quickRef.current = viewer;
      setStatus(null);
      setQuick({
        viewer,
        device: {
          id: session.deviceId,
          name: session.username || "远程电脑",
          os: "Windows",
          version: "",
          online: true,
          lastSeen: Date.now(),
          controllable: true,
        },
      });
    } catch (cause) {
      setStatus(null);
      if (cause instanceof ApiError) setError(describeHttp(cause.code, cause.message));
      else setError(describeThrown(cause));
    } finally {
      setBusy(false);
    }
  }

  if (quick) {
    return (
      <ControlPage
        viewer={quick.viewer}
        device={quick.device}
        notify={notify}
        onBack={() => {
          quick.viewer.close();
          quickRef.current = null;
          setQuick(null);
          setPassword("");
        }}
      />
    );
  }

  return (
    <div className="page remote-page">
      <header className="app-bar">
        <button type="button" className="icon-btn" onClick={onBack} aria-label="返回">
          <ArrowLeft size={18} />
        </button>
        <div>
          <h1>远程协助</h1>
          <p className="app-bar-sub">用识别码连另一台电脑</p>
        </div>
      </header>

      <div className="remote-card">
        <div className="remote-card-head">
          <div className="remote-icon">
            <Monitor size={20} />
          </div>
          <div>
            <strong>控制别人的电脑</strong>
            <small>填对方的识别码和密码</small>
          </div>
        </div>

        <label className="field">
          <span>对方的识别码</span>
          <div className="input-wrap">
            <Hash size={16} />
            <input
              value={formatCode(code)}
              onChange={(event) => setCode(event.target.value.replace(/\D/g, "").slice(0, 9))}
              placeholder="436 910 774"
              inputMode="numeric"
              autoComplete="off"
            />
          </div>
        </label>

        <label className="field">
          <span>连接密码</span>
          <div className="input-wrap">
            <Lock size={16} />
            <input
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              placeholder="对方用连接码时填这里"
              autoComplete="off"
            />
          </div>
        </label>

        {status && <p className="remote-status">{status}</p>}

        {error && (
          <div className="error-box">
            <strong>{error.title}</strong>
            <span>{error.detail}</span>
          </div>
        )}

        <button
          type="button"
          className="primary-btn"
          onClick={connect}
          disabled={busy || code.replace(/\D/g, "").length !== 9}
        >
          {busy ? <Loader2 size={18} className="spin" /> : <Link2 size={18} />}
          连接
        </button>

        <p className="remote-note">
          <ShieldCheck size={15} />
          网页端只能控制别人，不能被别人控制。识别码在对方「远程协助」页顶部；两台电脑登录同一账号时，
          跨网段也能直接连上（画面走服务器中转）。
        </p>
      </div>
    </div>
  );
}
