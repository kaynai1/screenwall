import {
  ArrowRight,
  Check,
  Copy,
  Eye,
  EyeOff,
  Hash,
  Link2,
  Loader2,
  Monitor,
  RefreshCw,
  ShieldCheck,
  Wifi,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  createConnectLink,
  connectDirect,
  getConnectCode,
  getDirectState,
  regeneratePairingCode,
  resolvePeerId,
  rotateConnectCode,
  setConnectCode,
  setDirectEnabled,
  type ConnectCodeView,
} from "../api";
import { formatConnectCode } from "../connect";
import type { DirectState, RemotePageProps } from "../types";
import "./RemotePage.css";

/** 临时密码的有效期选项。 */
type PasswordTtl = "once" | "hour" | "day" | "forever";

const TTL_OPTIONS: { value: PasswordTtl; label: string }[] = [
  { value: "once", label: "一次性" },
  { value: "hour", label: "1 小时" },
  { value: "day", label: "1 天" },
  { value: "forever", label: "长期" },
];

/**
 * 远程协助：把「别人连我」和「我连别人」放在一页。
 *
 * 这一页取代了散落在设置里的连接码与局域网直连 —— 它们都是**日常要用**的，
 * 埋在设置里每次都要点三层才能拿到码，不合适。
 */
export default function RemotePage({
  account,
  notify,
  onOpenDevice,
  active,
}: RemotePageProps) {
  const [connect, setConnect] = useState<ConnectCodeView | null>(null);
  const [direct, setDirect] = useState<DirectState | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  /** 临时密码是否明文显示。 */
  const [showPassword, setShowPassword] = useState(false);
  /** 用户自定义的固定密码；空表示随机生成。 */
  const [fixedPassword, setFixedPassword] = useState("");
  const [ttl, setTtl] = useState<PasswordTtl>("once");
  const [copied, setCopied] = useState<"all" | "code" | null>(null);

  /** 连接别人：对方识别码 + 密码。 */
  const [peerCode, setPeerCode] = useState("");
  const [peerPassword, setPeerPassword] = useState("");
  const [peerBusy, setPeerBusy] = useState(false);
  /** 连接过程中的进度提示（查找识别码 → 连接）。 */
  const [peerStatus, setPeerStatus] = useState<string | null>(null);

  const copyTimer = useRef<number | null>(null);

  const load = useCallback(async () => {
    try {
      const [nextConnect, nextDirect] = await Promise.all([
        getConnectCode().catch(() => null),
        getDirectState().catch(() => null),
      ]);
      setConnect(nextConnect);
      setDirect(nextDirect);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => {
    if (!active) return;
    void load();
    // 配对码 10 分钟一换，界面上要跟着走。
    // 隐藏时停轮询（见 PageProps.active 的说明）。回显时先 load 一次再续。
    const timer = window.setInterval(() => void load(), 5000);
    return () => window.clearInterval(timer);
  }, [load, active]);

  useEffect(
    () => () => {
      if (copyTimer.current !== null) window.clearTimeout(copyTimer.current);
    },
    [],
  );

  async function copy(text: string, what: "all" | "code") {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(what);
      if (copyTimer.current !== null) window.clearTimeout(copyTimer.current);
      copyTimer.current = window.setTimeout(() => setCopied(null), 2000);
      notify("已复制到剪贴板", "success");
    } catch {
      notify("复制失败，请手动选择", "error");
    }
  }

  async function toggleDirect(next: boolean) {
    setBusy(true);
    try {
      setDirect(await setDirectEnabled(next));
      notify(next ? "已开启局域网直连" : "已关闭局域网直连", "success");
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
    } finally {
      setBusy(false);
    }
  }

  async function regeneratePairing() {
    setBusy(true);
    try {
      setDirect(await regeneratePairingCode());
      notify("配对码已更新", "success");
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
    } finally {
      setBusy(false);
    }
  }

  async function rotateCode() {
    setBusy(true);
    try {
      setConnect(await rotateConnectCode());
      notify("识别码已更新，旧码立即失效", "success");
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
    } finally {
      setBusy(false);
    }
  }

  async function savePassword() {
    if (!serverAccount) return;
    setBusy(true);
    try {
      // 还没有读到服务器状态（connect 为空）时也要能用：开启状态以本地
      // 推断为准，设置成功后 getConnectCode 会把真实值带回来。
      const next = await setConnectCode(
        connect?.enabled ?? true,
        fixedPassword || undefined,
      );
      setConnect(next);
      notify(fixedPassword ? "已设置固定密码" : "已换新的临时密码", "success");
      setFixedPassword("");
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
    } finally {
      setBusy(false);
    }
  }

  async function generateLink() {
    setBusy(true);
    try {
      const link = await createConnectLink();
      await copy(link.url, "code");
      notify("链接已复制，10 分钟内有效且只能用一次", "success");
      // 签发链接会顺手把连接码连接打开（服务端行为），刷新一下让开关状态跟上。
      void load();
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
    } finally {
      setBusy(false);
    }
  }

  async function connectPeer() {
    if (!peerCode.trim()) {
      notify("先填对方的识别码", "error");
      return;
    }
    setPeerBusy(true);
    try {
      const raw = peerCode.trim();
      // 9 位数字是本机识别码：先在同一网段广播找，找不到再问服务器
      // （同账号的跨网段设备）。其余按 IP / 主机名处理。
      const digits = raw.replace(/[\s-]/g, "");
      let host = raw.replace(/\s/g, "");
      if (/^\d{9}$/.test(digits)) {
        setPeerStatus("正在局域网里查找这个识别码…");
        const found = await resolvePeerId(digits);
        if (found.kind === "relay") {
          // 跨网段：走账号中转会话。服务器已确认设备属于本账号且在线，
          // 但设备列表是服务端推送的，可能还没到 —— 补一条最小信息让
          // 控制页认得它（`online: true` 是服务器刚确认过的）。
          setPeerStatus(`找到 ${found.name}（跨网段），正在中转连接…`);
          notify("已找到设备，正在打开控制页", "success");
          setPeerStatus(null);
          onOpenDevice(found.deviceId, {
            id: found.deviceId,
            name: found.name,
            os: "Windows",
            version: "",
            online: true,
            lastSeen: Date.now(),
            controllable: true,
          });
          return;
        }
        host = found.address;
        setPeerStatus(`找到 ${found.name}（${found.address}），正在连接…`);
      } else {
        setPeerStatus("正在连接…");
      }
      const info = await connectDirect({
        host,
        pairingCode: peerPassword || undefined,
      });
      notify("已连接，正在打开控制页", "success");
      setPeerStatus(null);
      // 直接进控制页：会话已经建好，控制页会复用这一条。
      onOpenDevice(info.deviceId);
    } catch (cause) {
      notify(cause instanceof Error ? cause.message : String(cause), "error");
      setPeerStatus(null);
    } finally {
      setPeerBusy(false);
    }
  }

  // 识别码优先用本机生成的：不登录、不连服务器也有，安装后就固定不变。
  // 服务器连接码只有登录后才有，作为跨网段中转时用的备用码显示。
  const localId = direct?.localId ?? "";
  const code = localId
    ? formatConnectCode(localId)
    : connect
      ? formatConnectCode(connect.code)
      : "";
  const serverCode =
    localId && connect?.code ? formatConnectCode(connect.code) : "";
  // 密码取哪一份取决于怎么连：登录了就用服务器临时密码，纯局域网下
  // 服务器根本没参与，真正要输的是直连配对码。不这样回退的话，
  // 「复制识别码和密码」复制出去的是一个对方根本用不了的串。
  const serverPassword = connect?.password ?? "";
  const password = serverPassword || (direct?.pairingCode ?? "");
  const passwordIsPairing = !serverPassword && Boolean(direct?.pairingCode);
  const directOn = direct?.enabled ?? false;
  // 固定密码和一次性链接都由服务器签发：账号登录（非连接码会话、非本机模式）
  // 才有。以前判断的是 `connect` 有没有读到 —— 首次登录时服务器还没配密码，
  // 整块会直接消失，用户看到的就是「登录了却什么都没法设置」。
  const serverAccount = account.loggedIn && !account.quick && !account.local;
  // 服务器上是否已开启连接码连接；还没读到状态时按「登录后自动开启」的
  // 实际行为推断成已开启，避免闪一下「未开启」。
  const connectEnabled = connect?.enabled ?? serverAccount;
  const address = direct?.addresses[0]
    ? `${direct.addresses[0]}:${direct.port}`
    : "";

  return (
    <div className="remote-page">
      <div className="remote-head">
        <div>
          <h2>远程协助</h2>
          <p>把本机交给别人控制，或者连到另一台电脑</p>
        </div>
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          onClick={() => void load()}
        >
          <RefreshCw size={14} />
          刷新
        </button>
      </div>

      <div className="remote-grid">
        {/* ---------------- 别人连我 ---------------- */}
        <section className="remote-card">
          <header className="remote-card-head">
            <div className="remote-card-title">
              <span className="remote-card-icon">
                <Monitor size={17} strokeWidth={1.75} />
              </span>
              <div>
                <strong>别人用识别码连我</strong>
                <small>对方输入识别码和密码就能控制本机</small>
              </div>
            </div>
            <label className="remote-switch">
              <input
                type="checkbox"
                checked={directOn}
                disabled={direct === null || busy}
                onChange={(event) => void toggleDirect(event.target.checked)}
              />
              <span />
            </label>
          </header>

          <div className="remote-pair">
            <div className="remote-field">
              <div className="remote-field-head">
                <span>本机识别码</span>
                {/* 直连关着时局域网扫不到，但登录了服务器的人仍能凭它
                    跨网段找到本机 —— 两种情况下提示要说清楚。 */}
                {!directOn ? (
                  <em className="remote-field-note is-off">
                    {serverAccount
                      ? "局域网直连未开启，仅同账号可跨网段找到"
                      : "未开启，别人连不上"}
                  </em>
                ) : (
                  serverCode && (
                    <em className="remote-field-note">联网码 {serverCode}</em>
                  )
                )}
              </div>
              <div className="remote-field-body">
                <span className="remote-code mono">{code || "---------"}</span>
                <button
                  type="button"
                  className="remote-mini"
                  onClick={() => void copy(code, "code")}
                  disabled={!code}
                  title="复制识别码"
                  aria-label="复制识别码"
                >
                  {copied === "code" ? <Check size={14} /> : <Copy size={14} />}
                </button>
              </div>
            </div>

            <div className="remote-field">
              <div className="remote-field-head">
                <span>{passwordIsPairing ? "配对码" : "临时密码"}</span>
                <button
                  type="button"
                  className="remote-link"
                  onClick={() =>
                    void (passwordIsPairing ? regeneratePairing() : rotateCode())
                  }
                  disabled={busy}
                >
                  <RefreshCw size={12} />
                  换一个
                </button>
              </div>
              <div className="remote-field-body">
                <span className="remote-code mono">
                  {/* 配对码本来就明文显示（同一网段临时码），没必要再遮一次；
                      服务器临时密码默认遮住。没有密码时显示破折号，别显示
                      一排圆点让人以为有密码。 */}
                  {!password
                    ? "——"
                    : showPassword || passwordIsPairing
                      ? password
                      : "••••••••"}
                </span>
                {/* 配对码没什么可遮的，隐藏/显示按钮对它没有意义。 */}
                {!passwordIsPairing && (
                  <button
                    type="button"
                    className="remote-mini"
                    onClick={() => setShowPassword((value) => !value)}
                    title={showPassword ? "隐藏密码" : "显示密码"}
                    aria-label={showPassword ? "隐藏密码" : "显示密码"}
                  >
                    {showPassword ? <EyeOff size={14} /> : <Eye size={14} />}
                  </button>
                )}
              </div>
            </div>
          </div>

          <button
            type="button"
            className="btn btn-primary remote-copy-all"
            onClick={() =>
              void copy(
                `识别码：${code}\n${passwordIsPairing ? "配对码" : "密码"}：${password}`,
                "all",
              )
            }
            disabled={!code}
          >
            {copied === "all" ? <Check size={15} /> : <Copy size={15} />}
            复制识别码和{passwordIsPairing ? "配对码" : "密码"}
          </button>

          {/* 密码有效期只对服务器临时密码有意义：配对码固定 10 分钟一换，
              不是这里能调的。没登录时整组不显示，免得给出一个改不动的选项。 */}
          {serverPassword && (
            <div className="remote-ttl">
              <span className="remote-ttl-label">密码有效期</span>
              <div className="remote-ttl-options">
                {TTL_OPTIONS.map((option) => (
                  <button
                    key={option.value}
                    type="button"
                    className={`remote-ttl-btn ${ttl === option.value ? "is-on" : ""}`}
                    onClick={() => setTtl(option.value)}
                  >
                    <i />
                    {option.label}
                  </button>
                ))}
              </div>
              <p className="remote-ttl-hint">
                {ttl === "once"
                  ? "连上一次就换"
                  : ttl === "forever"
                    ? "长期有效，不会自动更换"
                    : `连接后 ${TTL_OPTIONS.find((o) => o.value === ttl)?.label}内可重复连接`}
              </p>
            </div>
          )}

          {/* 固定密码是服务器上的设置（要登录才写得了）。纯局域网/连接码会话
              没有账号可写，藏起来，不留一个点了必然报「尚未登录」的入口。 */}
          {serverAccount && (
            <div className="remote-fixed">
              <div className="remote-fixed-head">
                <span>
                  固定密码
                  <em>设置后不会自动更换</em>
                </span>
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={() => void savePassword()}
                  disabled={busy}
                >
                  {busy ? <Loader2 size={13} className="spin" /> : null}
                  {connect?.hasPassword ? "修改" : "设置"}
                </button>
              </div>
              <input
                type="text"
                className="remote-input"
                value={fixedPassword}
                onChange={(event) => setFixedPassword(event.target.value)}
                placeholder={connect?.hasPassword ? "已设置，输入新密码可修改" : "未设置"}
                spellCheck={false}
                autoComplete="off"
                aria-label="固定密码"
              />
              {!connectEnabled && (
                <p className="remote-tip">
                  连接码连接当前是关闭的，生成一次性链接时会自动打开。
                </p>
              )}
            </div>
          )}

          {directOn && (
            <div className="remote-local">
              <Wifi size={14} />
              <span>
                局域网地址
                <code>{address || "未检测到"}</code>
              </span>
              <em className="remote-local-hint">
                {direct?.pairingExpiresIn
                  ? `配对码 ${Math.ceil(direct.pairingExpiresIn / 60)} 分钟后自动更换`
                  : "配对码已过期，点上方「换一个」"}
              </em>
            </div>
          )}

          {/* 一次性链接由服务器签发，没登录时没有可用的东西可生成。 */}
          {serverAccount && (
            <button
              type="button"
              className="remote-link remote-link-wide"
              onClick={() => void generateLink()}
              disabled={busy}
            >
              <Link2 size={13} />
              生成一次性链接（10 分钟，只能用一次）
              <ArrowRight size={13} />
            </button>
          )}

          {error && (
            <p className="notice notice-error remote-error" role="alert">
              {error}
            </p>
          )}
        </section>

        {/* ---------------- 我连别人 ---------------- */}
        <section className="remote-card">
          <header className="remote-card-head">
            <div className="remote-card-title">
              <span className="remote-card-icon">
                <Link2 size={17} strokeWidth={1.75} />
              </span>
              <div>
                <strong>控制别人的电脑</strong>
                <small>填对方的识别码和密码</small>
              </div>
            </div>
          </header>

          <div className="remote-form">
            <label className="remote-label">
              <span>对方的识别码</span>
              <input
                type="text"
                className="remote-input"
                value={peerCode}
                onChange={(event) => setPeerCode(event.target.value)}
                placeholder="9 位识别码，或 192.168.1.20:47900"
                spellCheck={false}
                autoComplete="off"
                inputMode="numeric"
              />
            </label>

            <label className="remote-label">
              <span>密码</span>
              <input
                type="password"
                className="remote-input"
                value={peerPassword}
                onChange={(event) => setPeerPassword(event.target.value)}
                placeholder="临时密码或固定密码"
                spellCheck={false}
                autoComplete="off"
              />
            </label>

            <button
              type="button"
              className="btn btn-primary remote-connect"
              onClick={() => void connectPeer()}
              disabled={peerBusy || !peerCode.trim()}
            >
              {peerBusy ? (
                <Loader2 size={15} className="spin" />
              ) : (
                <Link2 size={15} />
              )}
              {peerBusy ? "正在连接…" : "连接"}
            </button>

            {peerStatus && (
              <p className="remote-status" role="status">
                <Loader2 size={13} className="spin" />
                {peerStatus}
              </p>
            )}

            <p className="remote-tip">
              <ShieldCheck size={13} />
              同一网段会自动走直连，画面不经过任何服务器；登录同一账号时，
              跨网段也能用识别码找到设备并走中转。
            </p>
          </div>

          <div className="remote-foot">
            <span>
              <Hash size={12} />
              识别码在对方「远程协助」页顶部
            </span>
          </div>
        </section>
      </div>
    </div>
  );
}
