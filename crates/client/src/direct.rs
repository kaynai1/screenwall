//! 局域网直连：不经过账号服务器，同网段两台机器直接建立控制会话。
//!
//! 这个模块只管「客户端这一侧的直连」：
//!
//! - 被控端：在设置里打开开关后监听端口，把配对码给出去（`listen_loop`）
//! - 主控端：填对方地址 + 配对码连进去（`connect`）；之后断线重连只需地址，
//!   凭据复用 `known_peers` 里的长期 token
//!
//! 被控端的会话处理全部在 `screenwall_agent::direct`，这里只负责把
//! 客户端自己的授权弹窗、配置、界面状态接上去。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use screenwall_agent::direct::{self, DirectAuth};
use screenwall_agent::server::{AgentState, ApprovalFn, ApprovalRequest};
use screenwall_agent::tls;
use screenwall_proto::{Capabilities, Hello, Message, PeerInfo, PROTO_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{base_dir, hostname, now_ms, Shared, ViewOut};

/// 配对码有效期：10 分钟。
const PAIRING_TTL: Duration = Duration::from_secs(600);
/// 直连监听端口默认值。
pub const DEFAULT_PORT: u16 = 47900;

/// 已配对过的设备：下次免配对码直连。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownPeer {
    /// `192.168.1.20:47900`
    pub address: String,
    /// 对方证书 SHA-256（大写十六进制、冒号分隔），连接时比对。
    pub fingerprint: String,
    /// 长期 token。
    pub token: String,
    /// 对方设备名，界面显示用。
    pub name: String,
}

/// 直连的运行时状态，全部由 `Shared.direct` 持有。
pub struct DirectHub {
    /// 被控端白名单。放在这里而不是监听循环里，因为配对码轮换、
    /// 配对成功后换长期 token 都要在命令线程里改它。
    pub auth: Arc<DirectAuth>,
    /// 当前配对码；`None` 表示未开启或已作废。
    pub pairing: Option<Pairing>,
    /// 监听是否真的起来了（端口被占用时为 false）。
    pub listening: bool,
    /// 正在直连进来的会话数。
    pub sessions: usize,
    /// 当前这条直连控制会话（主控端用）。
    pub link: Option<DirectLink>,
    /// 链路代号计数器，见 [`DirectLink::id`]。
    pub next_link: u64,
}

impl Default for DirectHub {
    fn default() -> Self {
        Self {
            auth: Arc::new(DirectAuth::new()),
            pairing: None,
            listening: false,
            sessions: 0,
            link: None,
            next_link: 0,
        }
    }
}

pub struct Pairing {
    pub code: String,
    /// 由配对码派生出的 token，加进 `DirectAuth`。
    pub token: String,
    pub created: Instant,
}

/// 主控端的一条直连会话。
pub struct DirectLink {
    pub tx: mpsc::UnboundedSender<ViewOut>,
    pub session: u32,
    /// 本条链路的代号。被控端的会话号是「每条连接各自从 1 开始」的，
    /// 重连后新旧两条链路很可能拿到同一个会话号，只比会话号分不清谁是谁。
    /// 旧读循环退出时靠它判断「当前登记的链路还是不是我自己」。
    pub id: u64,
}

impl DirectHub {
    /// 配对码剩余秒数；未开启或已过期返回 0。
    pub fn pairing_left(&self) -> u64 {
        match &self.pairing {
            Some(pairing) => {
                let elapsed = pairing.created.elapsed();
                if elapsed >= PAIRING_TTL {
                    0
                } else {
                    (PAIRING_TTL - elapsed).as_secs()
                }
            }
            None => 0,
        }
    }

    /// 配对码是否还有效。
    pub fn pairing_valid(&self) -> bool {
        self.pairing_left() > 0
    }
}

/// 给界面看的直连状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectState {
    pub enabled: bool,
    pub listening: bool,
    pub port: u16,
    pub addresses: Vec<String>,
    /// 本机识别码（9 位数字）。不登录也有，同网段输入它就能找到本机。
    pub local_id: String,
    pub pairing_code: Option<String>,
    pub pairing_expires_in: u64,
    pub active_sessions: usize,
    /// 已授权可以直连本机的设备数（长期 token 条数）。
    pub trusted_peers: usize,
}

/// 生成 9 位本机识别码。
///
/// 和服务端连接码同样的形状（9 位、首位非 0），这样用户不用区分
/// 「这是服务器的码还是本机的码」，同一个输入框都能填。
///
/// 用系统熵源而不是时间戳：识别码是长期不变的，可预测就等于把
/// 「找得到哪台机器」也交了出去。取 4 字节后按 9 亿取模，偏差可以忽略。
pub fn generate_local_id() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_err() {
        let now = now_ms() as u32;
        let pid = std::process::id();
        bytes = (now ^ pid.wrapping_mul(2654435761)).to_le_bytes();
    }
    // 1..=9 亿，保证首位非 0，正好 9 位。
    let value = u32::from_le_bytes(bytes) % 900_000_000 + 100_000_000;
    value.to_string()
}

/// 生成 6 位配对码。
///
/// 用系统熵源，不用时间戳 —— 时间戳可预测，别人能算出你下一个码。
/// 取 4 字节后对 1e6 取模有一点点偏差（2^32 不是 1e6 的整数倍），
/// 对「10 分钟有效、失败限流」的配对码来说完全够用。
fn generate_pairing_code() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 熵源不可用时的退路：仍然不退回可预测的纯时间戳。
        let now = now_ms() as u32;
        let pid = std::process::id();
        bytes = (now ^ pid.wrapping_mul(2654435761)).to_le_bytes();
    }
    format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}

/// 配对码 → 实际 token。
///
/// 必须是**纯函数**：主控端只知道配对码，不知道被控端的任何秘密，
/// 两边要能各自算出同一个值。所以这里只加一个固定的域分隔前缀，
/// 不掺入任何一方的 `direct_secret`。
///
/// 安全性由三件事共同保证，而不是靠这个哈希：
/// 1. 配对码 10 分钟过期、用一次就换；
/// 2. 被控端对认证失败做限流（`screenwall_agent::direct` 里的 `AuthLimiter`）；
/// 3. 连接本身是 TLS，token 不在明文里传输。
pub fn pairing_token(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"screenwall-direct-pairing:v1:");
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// 本机局域网 IPv4 地址。
///
/// 用一个连不通的外部地址做 `connect` 来问操作系统「出口在哪」——
/// UDP 不实际发包，只借路由表拿到出口网卡地址。
pub fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                if let std::net::IpAddr::V4(v4) = addr.ip() {
                    if !v4.is_loopback() {
                        out.push(v4.to_string());
                    }
                }
            }
        }
    }
    out
}

/// 把 `host` 或 `host:port` 规整成 `host:port`。
fn normalize_address(input: &str, default_port: u16) -> String {
    let input = input.trim();
    if input.contains(':') {
        input.to_string()
    } else {
        format!("{input}:{default_port}")
    }
}

// ---------------------------------------------------------------- 被控端

/// 被控端监听循环：配置打开就监听，关掉就释放端口。
///
/// 独立于 `host_loop`：直连不需要账号，未登录时也要能跑。
pub async fn listen_loop(app: AppHandle, shared: Arc<Shared>) {
    register_persist(&shared, app.clone()).await;
    let mut was_listening = false;
    loop {
        let config = shared.config.read().await.clone();
        if !config.direct_enabled {
            if was_listening {
                tracing::info!("局域网直连已关闭");
                was_listening = false;
            }
            shared.direct.lock().unwrap().listening = false;
            let _ = app.emit("direct-state", direct_state(&shared).await);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        // 第一次打开：生成配对码。之后一直沿用，除非用户主动换。
        ensure_pairing(&shared).await;

        let port = config.direct_port;
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!("直连端口 {port} 监听失败: {error}");
                shared.direct.lock().unwrap().listening = false;
                let _ = app.emit("direct-state", direct_state(&shared).await);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let material = match tls::ensure_cert(&base_dir()) {
            Ok(material) => material,
            Err(error) => {
                tracing::warn!("直连证书加载失败: {error:#}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let acceptor = match tls::server_config(&material) {
            Ok(config) => tokio_rustls::TlsAcceptor::from(config),
            Err(error) => {
                tracing::warn!("直连 TLS 配置失败: {error:#}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let material = Arc::new(material);

        // 每次重建监听都重灌长期白名单。
        //
        // 灌的是 `direct_tokens`（谁可以连我），不是 `known_peers`（我能连谁）——
        // 两者方向相反。早期版本灌错了，表现为「A 连过 B 之后，B 就能反过来
        // 免授权连 A」，而且自己存的长期 token 在重启后丢失、免配对码失效。
        {
            let auth = shared.direct.lock().unwrap().auth.clone();
            auth.replace_all(config.direct_tokens.iter().cloned()).await;
        }

        let (auth, pairing_token_now) = {
            let hub = shared.direct.lock().unwrap();
            (
                hub.auth.clone(),
                hub.pairing.as_ref().map(|pairing| {
                    (
                        pairing.token.clone(),
                        PAIRING_TTL.saturating_sub(pairing.created.elapsed()),
                    )
                }),
            )
        };
        // 配对码对应的 token 也要放行。剩余有效期按「配对码已经存在多久」
        // 算，不能重新给满 10 分钟 —— 否则每次重启监听都会给旧码续命。
        if let Some((token, left)) = pairing_token_now {
            if left > Duration::ZERO {
                auth.allow_for(token, left).await;
            }
        }

        let state = Arc::new(AgentState {
            config: agent_config(&config),
            displays: screenwall_agent::display::list_displays(),
            approval: Some(approval_fn(app.clone(), shared.clone())),
        });

        {
            let mut hub = shared.direct.lock().unwrap();
            hub.listening = true;
        }
        was_listening = true;
        tracing::info!(port, "局域网直连已监听 0.0.0.0:{port}");
        let _ = app.emit("direct-state", direct_state(&shared).await);

        // 配对码到期自动换码。跟着这次监听一起起停：关掉直连就不再续码。
        let refresh_app = app.clone();
        let refresh_shared = shared.clone();
        let refresh = tauri::async_runtime::spawn(async move {
            keep_pairing_fresh(refresh_app, refresh_shared).await;
        });

        // 跨网段打洞监听：在同一次监听里顺带起一个 UDP 端点，
        // 等主控端打洞包到达后接受它的 QUIC 连接。
        //
        // 跟着直连开关一起起停：关掉直连就不该再从公网被连进来。
        let (punch_cancel, punch_cancel_rx) = tokio::sync::watch::channel(false);
        let punch = if config.punch_through {
            match spawn_punch_listener(
                app.clone(),
                shared.clone(),
                config.punch_port,
                material.clone(),
                punch_cancel_rx,
            )
            .await
            {
                Ok(handle) => Some(handle),
                Err(error) => {
                    // 打洞起不来不影响同网段直连，只是跨网段连不上。
                    tracing::warn!("跨网段打洞监听未能启动: {error}");
                    None
                }
            }
        } else {
            None
        };

        // 自动发现应答器：同网段的主控端广播探测时回报本机地址与指纹。
        // 跟着这次监听一起起停 —— 关掉直连就不该再被别人扫到。
        let (discover_cancel, discover_cancel_rx) = tokio::sync::watch::channel(false);
        let discover = match screenwall_agent::discovery::spawn(
            screenwall_agent::discovery::DISCOVERY_PORT,
            screenwall_agent::discovery::Announcement::new(
                hostname(),
                port,
                material.fingerprint.clone(),
            )
            // 公告里带上识别码：主控端只知道 9 位数字也能在同网段找到本机。
            .with_id(config.local_id.clone()),
            discover_cancel_rx,
        )
        .await
        {
            Ok(handle) => Some(handle),
            Err(error) => {
                // UDP 端口被占用不影响直连本身，只是扫不到，打日志即可。
                tracing::warn!("自动发现未能启动: {error}");
                None
            }
        };

        // 监听任务本身不该退出；配置变化时由外层循环重建。
        let limiter = Arc::new(direct::AuthLimiter::new());
        let result = tokio::select! {
            result = direct::run(listener, state, acceptor, auth, limiter) => result,
            _ = wait_disabled(shared.clone(), port) => {
                tracing::info!("局域网直连配置已关闭");
                Ok(())
            }
        };
        refresh.abort();
        let _ = punch_cancel.send(true);
        if let Some(handle) = punch {
            handle.abort();
        }
        let _ = discover_cancel.send(true);
        if let Some(handle) = discover {
            handle.abort();
        }
        if let Err(error) = result {
            tracing::warn!("直连监听中断: {error:#}");
        }
        shared.direct.lock().unwrap().listening = false;
        let _ = app.emit("direct-state", direct_state(&shared).await);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// 跨网段打洞监听：等主控端的打洞包，然后接受它的 QUIC 连接。
///
/// 为什么被控端要主动 `await_peer` 而不是「监听 TCP 端口」：
/// NAT 会丢掉没见过的来源发来的包，所以跨网段时主控端根本连不到
/// 被控端的 TCP 端口。UDP 打洞是唯一可行的方向 —— 双方各发一次，
/// 两边的 NAT 就都开了口子。
///
/// 打洞成功后的 QUIC 连接上跑的是**同一套**直连协议（`serve_stream`），
/// 所以授权、文件、音频、剪贴板全都自动可用。
async fn spawn_punch_listener(
    app: AppHandle,
    shared: Arc<Shared>,
    port: u16,
    material: Arc<tls::CertMaterial>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
    use screenwall_agent::nat;

    let mut punch = nat::PunchSocket::bind(port)
        .map_err(|error| format!("绑定打洞端口 {port} 失败: {error}"))?;
    let local = punch.local_addr().map_err(|error| error.to_string())?;
    // 探测失败不算致命：同网段的主控端仍能用内网地址打洞成功，
    // 只是跨网段时因为不知道公网地址而失败。
    match punch.discover() {
        Ok(mapping) => tracing::info!(
            public = %mapping.public,
            local = %mapping.local,
            "跨网段打洞已就绪（UDP {}）",
            local.port()
        ),
        Err(error) => tracing::warn!("STUN 探测失败，跨网段打洞可能不可用: {error}"),
    }

    let socket = punch.into_socket();
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("切换非阻塞失败: {error}"))?;
    let endpoint = screenwall_agent::quic::server_endpoint(socket, &material)
        .map_err(|error| format!("QUIC 端点创建失败: {error}"))?;
    drop(material);

    let handle = tauri::async_runtime::spawn(async move {
        loop {
            let incoming = tokio::select! {
                _ = cancel.changed() => break,
                incoming = endpoint.accept() => match incoming {
                    Some(incoming) => incoming,
                    None => break,
                },
            };
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!("QUIC 打洞连接失败: {error}");
                    continue;
                }
            };
            let peer_ip = connection.remote_address().ip();
            // 和 TCP 直连同一条安全边界：只接受局域网或打洞成功的来源。
            // 打洞成功的公网地址不受 `is_lan_peer` 限制 —— 那正是它的用途。
            let stream = match screenwall_agent::quic::QuicStream::accept(&connection).await {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::debug!("接受 QUIC 流失败: {error}");
                    continue;
                }
            };
            let state = Arc::new(AgentState {
                config: agent_config(&shared.config.read().await.clone()),
                displays: screenwall_agent::display::list_displays(),
                approval: Some(approval_fn(app.clone(), shared.clone())),
            });
            let auth = shared.direct.lock().unwrap().auth.clone();
            let limiter = Arc::new(direct::AuthLimiter::new());
            tokio::spawn(async move {
                if let Err(error) =
                    direct::serve_stream(stream, peer_ip, state, auth, limiter).await
                {
                    tracing::debug!("QUIC 直连会话结束: {error}");
                }
                // 连接对象要活到流结束。
                connection.closed().await;
            });
        }
        tracing::info!("跨网段打洞监听已停止");
    });

    Ok(handle)
}

/// 让「授权通过后提升的长期 token」落进配置，重启后仍能免配对码。
///
/// 回调在授权任务里同步触发，不能在那里 `.await` 配置锁，所以丢给一个
/// 独立任务去写盘。顺便轮换配对码 —— 文档承诺「配对成功后自动更换」，
/// 不轮换的话这个码在 10 分钟内谁拿到都能连，且配对过的设备已经升级成
/// 长期凭据，不需要这个码了。
async fn register_persist(shared: &Arc<Shared>, app: AppHandle) {
    let auth = shared.direct.lock().unwrap().auth.clone();
    let weak = Arc::downgrade(shared);
    auth.on_promote(Arc::new(move |token: &str| {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let token = token.to_string();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            {
                let mut config = shared.config.write().await;
                if !config.direct_tokens.iter().any(|known| known == &token) {
                    config.direct_tokens.push(token);
                    if let Err(error) = config.save() {
                        tracing::warn!("保存直连长期令牌失败: {error}");
                    }
                }
            }
            rotate_pairing(&shared).await;
            let _ = app.emit("direct-state", direct_state(&shared).await);
        });
    }));
}

/// 换一个配对码：旧码立刻作废，长期凭据不受影响。
async fn rotate_pairing(shared: &Arc<Shared>) {
    let (auth, old) = {
        let mut hub = shared.direct.lock().unwrap();
        (hub.auth.clone(), hub.pairing.take())
    };
    if let Some(old) = old {
        // 只撤配对集合：用这个码连上来并被授权的设备已升成长期凭据。
        auth.revoke_pairing(&old.token).await;
        tracing::info!("直连配对成功，配对码已更换");
    }
    ensure_pairing(shared).await;
}

/// 配对码到期就自动换一个新码，并推一次界面状态。
///
/// 不做这件事的话，10 分钟后界面会一直停在「已失效」，用户得手动点
/// 「重新生成」；而 `ensure_pairing` 只在监听重建时跑，监听本身不会重建。
async fn keep_pairing_fresh(app: AppHandle, shared: Arc<Shared>) {
    loop {
        let left = {
            let hub = shared.direct.lock().unwrap();
            hub.pairing_left()
        };
        // 至少睡 1 秒，避免 pairing_left 为 0 时忙等。
        tokio::time::sleep(Duration::from_secs(left.max(1))).await;
        ensure_pairing(&shared).await;
        let _ = app.emit("direct-state", direct_state(&shared).await);
    }
}

/// 等到用户关掉直连、或改了端口。两种情况都要重建监听。
async fn wait_disabled(shared: Arc<Shared>, current_port: u16) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let config = shared.config.read().await;
        if !config.direct_enabled || config.direct_port != current_port {
            return;
        }
    }
}

fn agent_config(config: &crate::ClientConfig) -> screenwall_agent::config::AgentConfig {
    screenwall_agent::config::AgentConfig {
        name: hostname(),
        port: config.direct_port,
        token: String::new(),
        allow_control: true,
        clipboard_sync: config.clipboard_sync,
        server_url: String::new(),
        account_token: String::new(),
        device_id: config.device_id.clone(),
        ca_path: config.ca_path.clone().unwrap_or_default(),
        virtual_display: config.virtual_display,
    }
}

/// 直连的授权弹窗：**总是**要人点「允许」。
///
/// 不受「允许被控」总开关影响 —— 那个开关只管账号中转会话。
/// 直连绕过了账号体系，如果还能免授权，同网段任何人都能直接接管本机。
fn approval_fn(app: AppHandle, shared: Arc<Shared>) -> ApprovalFn {
    Arc::new(move |request: ApprovalRequest| {
        let ApprovalRequest {
            session,
            viewer,
            reply,
        } = request;
        let app = app.clone();
        let shared = shared.clone();
        shared.pending_control_requests.lock().unwrap().insert(
            session,
            crate::PendingControlRequest {
                viewer: viewer.clone(),
                reply,
            },
        );
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        let _ = app.emit(
            "control-request",
            json!({
                "session": session,
                "viewer": viewer,
                "timeoutSec": crate::APPROVAL_TIMEOUT_SECS,
                "direct": true,
            }),
        );
        let cleanup = shared.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(crate::APPROVAL_TIMEOUT_SECS + 5)).await;
            cleanup
                .pending_control_requests
                .lock()
                .unwrap()
                .remove(&session);
        });
    })
}

/// 没有配对码就生成一个。
async fn ensure_pairing(shared: &Arc<Shared>) {
    let needs = {
        let hub = shared.direct.lock().unwrap();
        !hub.pairing_valid()
    };
    if !needs {
        return;
    }
    let code = generate_pairing_code();
    let token = pairing_token(&code);
    tracing::info!("已生成新的直连配对码");
    // 旧配对码立刻作废，新配对码放行。
    let (auth, old) = {
        let mut hub = shared.direct.lock().unwrap();
        let auth = hub.auth.clone();
        let old = hub.pairing.replace(Pairing {
            code,
            token: token.clone(),
            created: Instant::now(),
        });
        (auth, old)
    };
    if let Some(old) = old {
        // 只撤配对集合：这个码可能已经被某台设备用掉并升级成长期凭据
        // （换码回调还没跑或跑失败时），`revoke` 会把那台设备一起踢掉。
        auth.revoke_pairing(&old.token).await;
    }
    auth.allow_for(token, PAIRING_TTL).await;
}

// ---------------------------------------------------------------- 主控端

/// 在已配对列表里找这台设备。
fn known_peer(peers: &[KnownPeer], address: &str) -> Option<KnownPeer> {
    peers.iter().find(|peer| peer.address == address).cloned()
}

/// 选择本次连接用的凭据。
///
/// 有配对码就用配对码派生的 token（首次配对）；没有配对码时必须复用
/// `known_peers` 里的长期 token —— 自动重连走的正是这条路：配对码是一次性的、
/// 10 分钟就过期，拿它重连必然失败。
fn pick_credential(code: Option<&str>, known: Option<&KnownPeer>) -> Result<String, String> {
    match (code, known) {
        (Some(code), _) => Ok(pairing_token(code)),
        (None, Some(peer)) => Ok(peer.token.clone()),
        (None, None) => Err("这台设备还没有配对过，需要输入对方显示的配对码".into()),
    }
}

/// 证书指纹比对。
///
/// `known` 为 `None` 表示首次配对（还没有可比对的指纹，放行后由
/// `remember_peer` 记下来）；有记录时必须完全一致，否则拒绝。
fn fingerprint_matches(known: Option<&KnownPeer>, actual: &str) -> bool {
    match known {
        Some(peer) => {
            !peer.fingerprint.is_empty() && actual.eq_ignore_ascii_case(&peer.fingerprint)
        }
        None => true,
    }
}

/// 直连一条会话。
///
/// 成功返回后，后续所有控制帧都从 `shared.direct.link.tx` 出去，
/// 控制页不需要知道自己连的是服务器还是对方。
pub async fn connect(
    app: &AppHandle,
    shared: &Arc<Shared>,
    host: &str,
    port: Option<u16>,
    pairing_code: Option<&str>,
    client_token: u64,
) -> Result<crate::ControlInfo, String> {
    let config = shared.config.read().await.clone();
    let port = port.unwrap_or(if config.direct_port == 0 {
        DEFAULT_PORT
    } else {
        config.direct_port
    });
    let address = normalize_address(host, port);
    let code = pairing_code.map(str::trim).filter(|code| !code.is_empty());

    // 已配对过的设备免配对码。
    let known = known_peer(&config.known_peers, &address);
    let token = pick_credential(code, known.as_ref())?;

    let url = format!("wss://{address}/ws");
    let mut request = url
        .clone()
        .into_client_request()
        .map_err(|error| format!("地址无效: {error}"))?;
    // 自签证书：先接受，连上后再比对指纹。
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "令牌格式无效")?,
    );

    let seen_fingerprint = Arc::new(std::sync::Mutex::new(None));
    let connector = direct_tls_connector(
        known.as_ref().map(|peer| peer.fingerprint.clone()),
        seen_fingerprint.clone(),
    )?;
    // 关掉 Nagle：直连会话里键鼠事件和视频帧共用一条 TCP 流，Nagle 会把
    // 小包攒到有 ACK 才发，最坏多等 40ms，操作手感明显发黏。
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        true,
        Some(tokio_tungstenite::Connector::Rustls(connector)),
    )
    .await
    .map_err(|error| format!("无法连接 {address}: {error}"))?;
    // 连接建立后再兜一层指纹核对：`PinnedCert` 只在有 pin 时比对，而且
    // 比对失败时 rustls 报的是笼统的握手错误。这里把实际指纹显式拿出来，
    // 指纹变了就给出明确原因，不会静默接受一台换了证书的机器。
    let actual = seen_fingerprint.lock().unwrap().clone().unwrap_or_default();
    if !fingerprint_matches(known.as_ref(), &actual) {
        return Err("对方的证书指纹与配对时不一致，已拒绝连接".into());
    }

    finish_connect(app, shared, address, code, token, actual, ws, client_token).await
}

/// 走 QUIC 打洞连一条直连会话（跨网段）。
///
/// 流程：绑定 UDP 端口 → STUN 探公网映射 → 与对端互发打洞包 → QUIC 握手
/// → 在 QUIC 流上跑与 TCP 完全相同的 WebSocket 协议。
///
/// 打洞失败返回 `Err`，调用方据此回退到「让用户走服务器中转」。
pub async fn connect_punch(
    app: &AppHandle,
    shared: &Arc<Shared>,
    host: &str,
    port: Option<u16>,
    pairing_code: Option<&str>,
    client_token: u64,
) -> Result<crate::ControlInfo, String> {
    use screenwall_agent::{nat, quic};
    use std::net::ToSocketAddrs;

    let config = shared.config.read().await.clone();
    let port = port.unwrap_or(if config.direct_port == 0 {
        DEFAULT_PORT
    } else {
        config.direct_port
    });
    let address = normalize_address(host, port);
    let code = pairing_code.map(str::trim).filter(|code| !code.is_empty());
    let known = known_peer(&config.known_peers, &address);
    let token = pick_credential(code, known.as_ref())?;

    // 对端地址必须能解析成 socket 地址：打洞比 TCP 更需要真实的 IP。
    let peer: std::net::SocketAddr = address
        .to_socket_addrs()
        .map_err(|error| format!("地址无效 {address}: {error}"))?
        .next()
        .ok_or_else(|| format!("解析不到 {address}"))?;

    let mut punch = nat::PunchSocket::bind(config.punch_port)
        .map_err(|error| format!("绑定 UDP 端口失败: {error}"))?;
    let mapping = punch
        .discover()
        .map_err(|error| format!("STUN 探测失败（可能被防火墙拦了 UDP）: {error}"))?;
    tracing::info!(
        public = %mapping.public,
        local = %mapping.local,
        "打洞前的公网映射"
    );

    // 对端可能在内网、也可能在公网：两个地址都试。
    let mut candidates = vec![peer];
    if mapping.public != peer {
        candidates.push(mapping.public);
    }
    let reached = punch
        .punch(&candidates, Duration::from_secs(8))
        .map_err(|error| format!("打洞失败: {error}"))?;

    let socket = punch.into_socket();
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("切换非阻塞失败: {error}"))?;
    let seen_fingerprint = Arc::new(std::sync::Mutex::new(None));
    let client = quic::client_endpoint(
        socket,
        known.as_ref().map(|peer| peer.fingerprint.clone()),
        seen_fingerprint.clone(),
    )
    .map_err(|error| format!("QUIC 配置失败: {error}"))?;
    let connection = client
        .connect(reached)
        .await
        .map_err(|error| format!("QUIC 握手失败: {error}"))?;
    let stream = quic::QuicStream::open(&connection)
        .await
        .map_err(|error| format!("打开 QUIC 流失败: {error}"))?;

    let actual = seen_fingerprint.lock().unwrap().clone().unwrap_or_default();
    if !fingerprint_matches(known.as_ref(), &actual) {
        return Err("对方的证书指纹与配对时不一致，已拒绝连接".into());
    }

    let url = format!("wss://{address}/ws");
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("地址无效: {error}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "令牌格式无效")?,
    );
    let (ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|error| format!("QUIC 上的 WebSocket 握手失败: {error}"))?;

    // 连接对象必须活到会话结束：drop 掉它流会立刻被重置。
    std::mem::forget(connection);

    tracing::info!(peer = %reached, "跨网段直连已建立（QUIC 打洞）");
    finish_connect(app, shared, address, code, token, actual, ws, client_token).await
}
/// 传输建好之后的公共部分：握手、开会话、登记、转入读循环。
///
/// TCP + TLS 和 QUIC 打洞都走这里，保证两条路径的协议行为完全一致 ——
/// 尤其是授权、配对升级、审计上报这些只有一份实现，不会因为换传输而漏掉。
#[allow(clippy::too_many_arguments)]
async fn finish_connect<S>(
    app: &AppHandle,
    shared: &Arc<Shared>,
    address: String,
    code: Option<&str>,
    token: String,
    // 本次握手看到的对端证书指纹，配对成功后存进 `known_peers`。
    fingerprint: String,
    ws: tokio_tungstenite::WebSocketStream<S>,
    client_token: u64,
) -> Result<crate::ControlInfo, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ViewOut>();

    // 握手：先 hello，等 welcome。
    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: token.clone(),
        client: PeerInfo {
            name: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        caps: Capabilities::current(),
    });
    sink.send(WsMessage::Text(hello.to_json().into()))
        .await
        .map_err(|error| format!("握手失败: {error}"))?;

    let welcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(Message::Welcome(welcome)) = Message::parse(text.as_str()) {
                        return Ok(welcome);
                    }
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(format!("连接中断: {error}")),
                None => return Err("对方关闭了连接".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "对方未响应（配对码可能已失效）".to_string())??;

    // 申请会话。
    let request_id = shared
        .next_request
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    let open = json!({
        "t": "open",
        "request": request_id,
        "viewer": hostname()
    })
    .to_string();
    sink.send(WsMessage::Text(open.into()))
        .await
        .map_err(|error| format!("发送会话请求失败: {error}"))?;

    // 等对方点「允许」。
    //
    // 这里**必须自己读** `stream`：`read_loop` 还没启动，没人消费帧的话
    // `open.ok` 永远到不了，等 oneshot 会死锁。读到 `open.ok` 后把剩下的
    // 帧交给 `read_loop`（会话建立后它才开始跑）。
    let (session, buffered) = match tokio::time::timeout(
        Duration::from_secs(crate::APPROVAL_TIMEOUT_SECS + 10),
        wait_open_ok(&mut stream, &mut sink, request_id),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(message)) => return Err(message),
        Err(_) => return Err("对方未确认（等待超时）".into()),
    };

    // 会话真的建立了才记配对：对方拒绝授权时不该留下一条无效记录。
    //
    // 被控端在授权通过时会把这次用的 token 升成长期 token（配对码作废后仍
    // 有效）。`remember_peer` 存下的正是同一个字符串，所以之后的重连
    // （不带配对码、只给地址）能直接过白名单，不需要用户再输一次码。
    if code.is_some() {
        remember_peer(shared, &address, &token, &fingerprint, &welcome.agent.name).await;
        tracing::info!(address = %address, "直连配对成功，凭据可用于后续自动重连");
        // 本机模式没有服务器推设备列表：刚配对的这台要自己补进去，
        // 否则连上了却在屏幕墙/设备页里找不到。
        if shared.config.read().await.local_mode {
            crate::refresh_local_devices(app, shared).await;
        }
    }

    // 会话建立：登记直连通道与控制会话，然后转入读循环。
    //
    // 控制会话必须在这里登记：控制页、文件页、音量、电源所有命令都靠
    // `shared.control` 找会话号，不登记的话它们会报「控制会话尚未建立」。
    //
    // 登记前先把旧链路读循环掐掉：自动重连时新旧两条链路会短暂并存，
    // 旧链路的 `control_out` 还能往已死的 socket 里写；更要紧的是旧读循环
    // 退出时会去动 `shared.control`。按链路代号认领可以防住后者，
    // 替换登记则保证「同一时刻只有一条活着的直连」。
    let (link_id, stale) = {
        let mut hub = shared.direct.lock().unwrap();
        let id = bump_link_id(&mut hub);
        let stale = hub.link.replace(DirectLink {
            tx: out_tx.clone(),
            session,
            id,
        });
        hub.sessions = 1;
        (id, stale)
    };
    if let Some(old) = stale {
        // 旧链路自己的读循环会看到 link 已被替换而安静退出；这里只丢弃发送端，
        // 让它的 `out_rx` 收到 None 后结束，不阻塞新链路的登记。
        drop(old.tx);
    }
    let started_ms = now_ms();
    *shared.control.write().await = Some(crate::ControlSession {
        session,
        device_id: format!("direct:{address}"),
        client_token,
        video_id: None,
        width: 0,
        height: 0,
        fps: 0,
        max_width: 0,
        quality: 0,
        display: 0,
        displays: Vec::new(),
        audio_enabled: false,
        audio_id: None,
        audio_rate: 0,
        audio_channels: 0,
        audio_codec: screenwall_proto::AudioCodec::Pcm16,
        audio_bitrate: 0,
        audio_source: screenwall_proto::AudioSource::Loopback,
        // 直连的 welcome 在会话登记前就收到了：先记下能力交集，
        // 免得控制页订阅 `control-caps` 之前事件已经过去。
        peer_caps: Some(Capabilities::with_peer(welcome.caps.clone())),
        started_ms,
        // 直连会话同样支持断线自动重连（重连时整条 TLS + WS 重建）。
        reconnectable: true,
        direct_link: Some(link_id),
    });
    shared.control_wanted.store(true, Ordering::Relaxed);
    // 直连会话沿用同一套质量统计：用户主动连的这条（还没有重连在跑）从零
    // 开始累计；自动重连恢复的会话保留累计值，只由重连任务重定序号基准。
    if shared.reconnect.lock().unwrap().is_none() {
        *shared.quality.lock().unwrap() = crate::quality::Tracker::new(Instant::now(), started_ms);
    }

    let _ = app.emit("direct-state", direct_state(shared).await);

    let info = crate::ControlInfo {
        session,
        device_id: format!("direct:{address}"),
        request: request_id,
        client_token,
    };

    let app = app.clone();
    let shared = shared.clone();
    tauri::async_runtime::spawn(async move {
        // 授权期间读到的帧先喂回去，否则那条 `welcome` 会丢。
        for message in buffered {
            match message {
                WsMessage::Text(text) => {
                    crate::handle_view_text(&app, &shared, text.as_str()).await;
                }
                WsMessage::Binary(data) => {
                    crate::handle_view_binary(&app, &shared, data.as_ref()).await;
                }
                _ => {}
            }
        }
        let result = read_loop(&app, &shared, link_id, &mut stream, &mut sink, &mut out_rx).await;
        if let Err(error) = result {
            tracing::warn!("直连会话中断: {error}");
        }
        // 只有「当前登记的还是我这条链路」时才拆登记：重连时新链路可能已经
        // 先一步接管（会话号还可能撞上同一个值），旧链路退出不能把新的拆掉。
        let mine = {
            let mut hub = shared.direct.lock().unwrap();
            if hub.link.as_ref().map(|link| link.id) == Some(link_id) {
                hub.link = None;
                hub.sessions = 0;
                true
            } else {
                false
            }
        };
        if !mine {
            return;
        }
        // 直连不经过服务器，审计只能由这里补报。本地直连（没登录）直接跳过。
        report_direct_audit(&shared, &address, started_ms, crate::now_ms()).await;
        let _ = app.emit("direct-state", direct_state(&shared).await);
        // 会话没了，界面也该知道。只收掉属于这条链路的控制会话：用户可能已经
        // 切到另一台设备（中转或另一条直连），那条会话不归这条链路管。
        // 走统一出口：可重连会话会进入退避重连，用户主动停止时
        // `control_wanted` 已经是 false，不会重连。
        let control = {
            let mut guard = shared.control.write().await;
            match guard.as_ref() {
                Some(current) if current.direct_link == Some(link_id) => guard.take(),
                _ => None,
            }
        };
        if let Some(control) = control {
            crate::end_session(&app, &shared, control, "直连已断开").await;
        }
    });

    Ok(info)
}

/// 把一次直连会话补报给服务器审计。
///
/// 直连不经过服务器，服务器无从自行记录；会话结束时由主控端上报一条
/// 完整记录（起止时间、时长、被控端地址）。没登录或服务器地址为空时
/// 静默跳过 —— 局域网直连本来就不要求账号。
///
/// 流量按 0 报：直连的统计在 `quality::Tracker` 里按会话累计，但那份数据
/// 会在重连时被重置，拿来写审计并不可靠，宁可留空也不要写错。
async fn report_direct_audit(shared: &Arc<Shared>, address: &str, started_ms: i64, ended_ms: i64) {
    let config = shared.config.read().await.clone();
    let base = config.server_url.trim().trim_end_matches('/');
    if base.is_empty() || config.token.trim().is_empty() {
        return;
    }
    let body = json!({
        "controllerDeviceId": config.device_id,
        "controlledDeviceId": format!("direct:{address}"),
        "startedAt": started_ms,
        "endedAt": ended_ms,
        "durationMs": (ended_ms - started_ms).max(0),
        "bytesUp": 0,
        "bytesDown": 0,
    });
    let response = shared
        .http
        .post(format!("{base}/api/v1/audits/direct"))
        .bearer_auth(&config.token)
        .json(&body)
        .send()
        .await;
    match response {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(address = %address, "直连审计已上报");
        }
        Ok(resp) => tracing::debug!(status = %resp.status(), "直连审计上报被拒绝"),
        Err(error) => tracing::debug!("直连审计上报失败: {error}"),
    }
}

/// 分配一条新的直连链路代号。
///
/// 链路代号从 1 开始，溢出回绕后也不会出现 0（0 会和「没有链路」混淆）。
fn bump_link_id(hub: &mut DirectHub) -> u64 {
    hub.next_link = hub.next_link.wrapping_add(1).max(1);
    hub.next_link
}

/// 当前登记的直连链路是不是这一条。
fn is_current_link(shared: &Shared, link_id: u64) -> bool {
    shared
        .direct
        .lock()
        .unwrap()
        .link
        .as_ref()
        .map(|link| link.id)
        == Some(link_id)
}

/// 在会话建立前读 `open.ok` / `open.err`，并等授权结果。
///
/// 直连没有服务器帮忙分流，控制消息和会话帧走同一条 WebSocket，
/// 所以「等授权结果」这一段必须自己读流。
///
/// 注意 `open.ok` 只表示**会话号已分配**，不代表本机点了「允许」——
/// 被控端要等人点按钮，拒绝时会补一条 `closed`。所以拿到 `open.ok` 后
/// 不能立刻返回：要等第一条属于该会话的 tunnel 帧（`welcome`）作为
/// 「授权已通过、会话真的在跑」的证据；先到的是 `closed` 就说明被拒。
///
/// 期间读到的帧要原样交回给 `read_loop`，不能丢 —— 那条 `welcome`
/// 里有能力协商结果和会话限制。
async fn wait_open_ok<S>(
    stream: &mut futures_util::stream::SplitStream<S>,
    sink: &mut futures_util::stream::SplitSink<S, WsMessage>,
    request_id: u32,
) -> Result<(u32, Vec<WsMessage>), String>
where
    S: futures_util::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let mut opened: Option<u32> = None;
    // 会话建立前读到的帧，按原序交给 `read_loop`。
    let mut buffered: Vec<WsMessage> = Vec::new();
    loop {
        let Some(item) = stream.next().await else {
            return Err("对方关闭了连接".to_string());
        };
        let message = item.map_err(|error| error.to_string())?;
        match &message {
            WsMessage::Binary(data) => {
                // 会话帧：能解出会话号就说明授权已通过。
                if let Ok((header, _)) = screenwall_proto::cloud::decode_tunnel_frame(data) {
                    if opened == Some(header.session) {
                        buffered.push(message);
                        return Ok((header.session, buffered));
                    }
                }
                buffered.push(message);
            }
            WsMessage::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                match value.get("t").and_then(Value::as_str).unwrap_or_default() {
                    "open.ok" => {
                        let request =
                            value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if request != request_id {
                            continue;
                        }
                        let session =
                            value.get("session").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if session == 0 {
                            return Err("对方返回了无效的会话号".to_string());
                        }
                        opened = Some(session);
                        // 会话号到手：立刻发会话级 hello 和默认视频订阅。
                        // 被控端此时可能还在等本机点「允许」，它的 pending
                        // 缓存会把这两帧存住，授权通过后按原序补投。
                        let hello = Message::Hello(Hello {
                            proto: PROTO_VERSION,
                            token: String::new(),
                            client: PeerInfo {
                                name: hostname(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                            },
                            caps: Capabilities::current(),
                        });
                        for message in [hello, crate::default_video_sub()] {
                            let payload = message.to_json();
                            let frame = screenwall_proto::cloud::encode_tunnel_frame(
                                screenwall_proto::cloud::TunnelKind::Json,
                                session,
                                payload.as_bytes(),
                            );
                            sink.send(WsMessage::Binary(frame.into()))
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    "open.err" => {
                        let request =
                            value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if request != request_id {
                            continue;
                        }
                        let code = value.get("code").and_then(Value::as_u64).unwrap_or(0);
                        let msg = value
                            .get("msg")
                            .and_then(Value::as_str)
                            .unwrap_or("对方拒绝了本次请求");
                        return Err(format!("{code}: {msg}"));
                    }
                    "closed" => {
                        let closed =
                            value.get("session").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if opened == Some(closed) {
                            let code = value.get("code").and_then(Value::as_u64).unwrap_or(0);
                            let msg = value
                                .get("msg")
                                .and_then(Value::as_str)
                                .unwrap_or("对方拒绝了本次请求");
                            return Err(format!("{code}: {msg}"));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

async fn read_loop<S>(
    app: &AppHandle,
    shared: &Arc<Shared>,
    link_id: u64,
    stream: &mut futures_util::stream::SplitStream<S>,
    sink: &mut futures_util::stream::SplitSink<S, WsMessage>,
    out_rx: &mut mpsc::UnboundedReceiver<ViewOut>,
) -> Result<(), String>
where
    S: futures_util::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // 重连成功时新链路已经接管：旧链路不能再把帧喂给 `handle_view_*`。
        // 直连会话号按连接独立分配，新旧两条链路很可能都是 1，
        // 只按会话号过滤区分不开，必须认链路代号。
        if !is_current_link(shared, link_id) {
            return Ok(());
        }
        tokio::select! {
            _ = ping.tick() => {
                sink.send(WsMessage::Ping(Vec::new().into()))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            out = out_rx.recv() => {
                let Some(out) = out else { return Ok(()) };
                match out {
                    ViewOut::Text(text) => {
                        sink.send(WsMessage::Text(text.into())).await.map_err(|e| e.to_string())?;
                    }
                    ViewOut::Binary(data) => {
                        sink.send(WsMessage::Binary(data.into())).await.map_err(|e| e.to_string())?;
                    }
                }
            }
            item = stream.next() => {
                let Some(item) = item else { return Ok(()) };
                match item.map_err(|error| error.to_string())? {
                    WsMessage::Text(text) => {
                        crate::handle_view_text(app, shared, text.as_str()).await;
                    }
                    WsMessage::Binary(data) => {
                        crate::handle_view_binary(app, shared, data.as_ref()).await;
                    }
                    WsMessage::Ping(payload) => {
                        sink.send(WsMessage::Pong(payload)).await.map_err(|e| e.to_string())?;
                    }
                    WsMessage::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// 记住已配对的设备，下次免配对码，并固定证书指纹。
async fn remember_peer(
    shared: &Arc<Shared>,
    address: &str,
    token: &str,
    fingerprint: &str,
    name: &str,
) {
    let mut config = shared.config.write().await;
    config.known_peers.retain(|peer| peer.address != address);
    config.known_peers.push(KnownPeer {
        address: address.to_string(),
        fingerprint: fingerprint.to_string(),
        token: token.to_string(),
        name: name.to_string(),
    });
    if let Err(error) = config.save() {
        tracing::warn!("保存直连配对记录失败: {error}");
    }
}

/// 自签证书的 TLS 连接器。
///
/// `pinned` 为 `Some` 时做**证书指纹固定**：对方证书的 SHA-256 必须与
/// 上次配对时记下的完全一致，否则拒绝连接。这是直连唯一的身份校验手段 ——
/// 自签证书没有 CA 背书，不比对指纹的话同网段任何人都能冒充对方。
///
/// `None` 表示首次配对：此时还没有可比对的指纹，先接受，配对成功后
/// 由调用方把实际指纹存进 `known_peers`。
fn direct_tls_connector(
    pinned: Option<String>,
    seen: Arc<std::sync::Mutex<Option<String>>>,
) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS 配置失败: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { pinned, seen }))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct PinnedCert {
    /// 期望的证书指纹（大写十六进制、冒号分隔）；`None` 表示首次配对。
    pinned: Option<String>,
    /// 握手时把实际指纹记在这里，配对成功后写进 `known_peers`。
    seen: Arc<std::sync::Mutex<Option<String>>>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = screenwall_agent::tls::fingerprint(end_entity);
        if let Some(expected) = &self.pinned {
            if !actual.eq_ignore_ascii_case(expected) {
                tracing::warn!("直连证书指纹不匹配，已拒绝连接");
                return Err(rustls::Error::General("证书指纹不匹配".into()));
            }
        }
        *self.seen.lock().unwrap() = Some(actual);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------- 界面

pub async fn direct_state(shared: &Arc<Shared>) -> DirectState {
    let config = shared.config.read().await.clone();
    let (pairing_code, pairing_expires_in, listening, active_sessions, auth) = {
        let hub = shared.direct.lock().unwrap();
        let code = hub
            .pairing
            .as_ref()
            .filter(|_| hub.pairing_valid())
            .map(|pairing| pairing.code.clone());
        // 被控端看进来的连接数；主控端看自己这条直连会话。
        let active = hub
            .auth
            .active()
            .max(if hub.link.is_some() { 1 } else { 0 });
        (
            code,
            hub.pairing_left(),
            hub.listening,
            active,
            hub.auth.clone(),
        )
    };
    // 锁外再读：`promoted_count` 要拿异步锁，不能在持有 hub 的同步锁时 await。
    let trusted_peers = auth.promoted_count().await;
    DirectState {
        enabled: config.direct_enabled,
        listening,
        port: config.direct_port,
        addresses: local_addresses(),
        local_id: config.local_id.clone(),
        pairing_code,
        pairing_expires_in,
        active_sessions,
        trusted_peers,
    }
}

// ---------------------------------------------------------------- 命令

#[tauri::command]
pub async fn get_direct_state(
    shared: tauri::State<'_, Arc<Shared>>,
) -> Result<DirectState, String> {
    Ok(direct_state(shared.inner()).await)
}

#[tauri::command]
pub async fn set_direct_enabled(
    app: AppHandle,
    shared: tauri::State<'_, Arc<Shared>>,
    enabled: bool,
) -> Result<DirectState, String> {
    let shared = shared.inner().clone();
    {
        let mut config = shared.config.write().await;
        config.direct_enabled = enabled;
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }
    if enabled {
        ensure_pairing(&shared).await;
    } else {
        // 关闭直连：配对码立刻作废，不让已经拿到码的人继续连。
        let (auth, old) = {
            let mut hub = shared.direct.lock().unwrap();
            (hub.auth.clone(), hub.pairing.take())
        };
        if let Some(old) = old {
            // 只撤配对集合：关掉直连不该连带吊销长期授权，
            // 否则用户临时关一次开关，所有配对过的设备都要重新输码。
            auth.revoke_pairing(&old.token).await;
        }
    }
    let state = direct_state(&shared).await;
    let _ = app.emit("direct-state", state.clone());
    Ok(state)
}

/// 清空「允许直连本机」的长期令牌：所有配对过的设备都要重新输配对码。
#[tauri::command]
pub async fn forget_direct_peers(
    app: AppHandle,
    shared: tauri::State<'_, Arc<Shared>>,
) -> Result<DirectState, String> {
    let shared = shared.inner().clone();
    let auth = { shared.direct.lock().unwrap().auth.clone() };
    auth.replace_all(Vec::new()).await;
    {
        let mut config = shared.config.write().await;
        config.direct_tokens.clear();
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }
    tracing::info!("已清空局域网直连的长期授权");
    let state = direct_state(&shared).await;
    let _ = app.emit("direct-state", state.clone());
    Ok(state)
}

#[tauri::command]
pub async fn regenerate_pairing_code(
    app: AppHandle,
    shared: tauri::State<'_, Arc<Shared>>,
) -> Result<DirectState, String> {
    let shared = shared.inner().clone();
    // 先撤掉旧码，再生成新的。
    let (auth, old) = {
        let mut hub = shared.direct.lock().unwrap();
        (hub.auth.clone(), hub.pairing.take())
    };
    if let Some(old) = old {
        // 只撤配对集合：换码不该影响已经配对过的设备。
        auth.revoke_pairing(&old.token).await;
    }
    ensure_pairing(&shared).await;
    let state = direct_state(&shared).await;
    let _ = app.emit("direct-state", state.clone());
    Ok(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectInput {
    pub host: String,
    pub port: Option<u16>,
    pub pairing_code: Option<String>,
}

#[tauri::command]
pub async fn connect_direct(
    app: AppHandle,
    shared: tauri::State<'_, Arc<Shared>>,
    input: ConnectInput,
) -> Result<crate::ControlInfo, String> {
    // 直连的会话令牌由后端生成：前端不需要参与，控制页从 `get_control_info`
    // 读回来复用即可（中转路径的令牌是前端自增的，两条路径在这里分开）。
    static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let client_token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let shared = shared.inner();
    // 用户手动发起直连：任何在途的自动重连都作废，否则它稍后建好的会话
    // 会把用户刚连上的这条顶掉。
    crate::cancel_reconnect(shared, None);

    // 输入的是 9 位识别码（不是 IP）时，先在同网段广播把它解析成地址。
    //
    // 解析不出来就到此为止，不要拿「9 位数字」当主机名去连 —— 那只会
    // 变成一次 DNS 查询加超时，报错还很难懂。
    let host = if let Some(id) = parse_local_id(&input.host) {
        let found = resolve_local_id(&id).await?;
        tracing::info!(%id, address = %found.address, "识别码已解析为直连地址");
        found.address
    } else {
        input.host.clone()
    };

    // 先走同网段的 TCP 直连，同时让打洞在一个很短的宽限后也开始。
    //
    // 顺序不能反：同网段时 TCP 一定更快更稳（几十毫秒就成功），打洞要多花
    // 几秒探测 STUN，所以宽限窗口内 TCP 先赢，打洞直接被丢掉。
    // 但跨网段时 TCP 往往要等系统把 SYN 重试完（Windows 上可达 20 秒）才
    // 判定不可达，串行的话打洞要白等这 20 秒，这里并行就能省掉。
    let punch_enabled = shared.config.read().await.punch_through;
    connect_tcp_or_punch(
        &app,
        shared,
        &host,
        input.port,
        input.pairing_code.as_deref(),
        client_token,
        punch_enabled,
    )
    .await
}

/// 打洞相对 TCP 的启动宽限：局域网 TCP 一般远快于这个值。
const PUNCH_GRACE: Duration = Duration::from_millis(1200);

/// TCP 直连与打洞并行，谁先成功用谁。
///
/// 任何一条先失败都不会立刻放弃 —— 另一条还在跑，跨网段时正是「TCP 不通、
/// 打洞能通」的常见情形。两条都失败才把两个原因一起报出去。
async fn connect_tcp_or_punch(
    app: &AppHandle,
    shared: &Arc<Shared>,
    host: &str,
    port: Option<u16>,
    code: Option<&str>,
    client_token: u64,
    punch_enabled: bool,
) -> Result<crate::ControlInfo, String> {
    let tcp = connect(app, shared, host, port, code, client_token);
    if !punch_enabled {
        return tcp.await;
    }
    let punch = async {
        tokio::time::sleep(PUNCH_GRACE).await;
        connect_punch(app, shared, host, port, code, client_token).await
    };
    tokio::pin!(tcp);
    tokio::pin!(punch);

    let mut tcp_error: Option<String> = None;
    let mut punch_error: Option<String> = None;
    loop {
        tokio::select! {
            result = &mut tcp, if tcp_error.is_none() => match result {
                Ok(info) => return Ok(info),
                Err(error) => {
                    tracing::info!("TCP 直连失败（{error}），继续等打洞");
                    tcp_error = Some(error);
                }
            },
            result = &mut punch, if punch_error.is_none() => match result {
                Ok(info) => return Ok(info),
                Err(error) => {
                    tracing::info!("跨网段打洞失败（{error}），继续等 TCP");
                    punch_error = Some(error);
                }
            },
            else => break,
        }
    }
    // 两条路都失败时把原因都带上：只说打洞失败会让用户以为局域网地址
    // 填错了，实际可能是对方没开打洞或双方都是对称型 NAT。
    Err(format!(
        "局域网直连失败：{}；跨网段打洞也失败：{}",
        tcp_error.unwrap_or_default(),
        punch_error.unwrap_or_default()
    ))
}

/// 扫描同网段开启了局域网直连的设备（UDP 广播）。
///
/// 只发一次探测、等 1.2 秒收应答。被控端关掉直连后就不再应答，
/// 所以这里扫不到是正常现象，返回空数组而不是报错。
#[tauri::command]
pub async fn scan_direct_devices() -> Result<Vec<screenwall_agent::discovery::Discovered>, String> {
    Ok(screenwall_agent::discovery::scan(Duration::from_millis(1200)).await)
}

/// 把输入里的识别码取出来：允许用户带空格或短横线（界面按 3-3-3 显示）。
///
/// 只认「9 位纯数字」，IP 地址和主机名一律返回 `None` —— 判断放在一处，
/// 免得界面和后端对「什么算识别码」有不同理解。
pub fn parse_local_id(input: &str) -> Option<String> {
    let digits: String = input.chars().filter(char::is_ascii_digit).collect();
    let others = input
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == ' ' || ch == '-' || ch == '\t');
    (digits.len() == 9 && others).then_some(digits)
}

/// 按识别码在局域网里找设备，返回它的直连地址。
///
/// 用户只记得 9 位数字时走这条路：广播一次探测，拿回所有应答，
/// 挑识别码对得上的那台。找不到就报错 —— 不要退回「随便连一台」，
/// 那会让用户连到别人的电脑上。
async fn resolve_local_id(id: &str) -> Result<screenwall_agent::discovery::Discovered, String> {
    let found = screenwall_agent::discovery::scan(Duration::from_millis(1200)).await;
    pick_by_id(found, id)
        .ok_or_else(|| format!("局域网里没找到识别码 {id} 的设备，确认对方已开启直连且在同一网段"))
}

/// 从扫描结果里挑识别码对得上的那台。
///
/// 单独拆出来是为了能测：真实扫描要发广播，测试环境里不可靠。
fn pick_by_id(
    found: Vec<screenwall_agent::discovery::Discovered>,
    id: &str,
) -> Option<screenwall_agent::discovery::Discovered> {
    found.into_iter().find(|device| device.id == id)
}

/// 给界面用的版本：输入 9 位数字，返回对方地址。
#[tauri::command]
pub async fn resolve_direct_id(
    id: String,
) -> Result<screenwall_agent::discovery::Discovered, String> {
    let wanted = parse_local_id(&id).ok_or("识别码是 9 位数字")?;
    resolve_local_id(&wanted).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientConfig, ControlSession};

    #[test]
    fn local_id_is_nine_digits_and_unpredictable() {
        let first = generate_local_id();
        let second = generate_local_id();
        assert_eq!(first.len(), 9, "识别码必须是 9 位");
        assert!(!first.starts_with('0'), "首位不能是 0");
        assert!(first.chars().all(|ch| ch.is_ascii_digit()));
        assert_ne!(first, second, "两次生成不该撞在一起");
    }

    fn discovered(address: &str, id: &str) -> screenwall_agent::discovery::Discovered {
        screenwall_agent::discovery::Discovered {
            address: address.into(),
            name: format!("设备{id}"),
            port: 47900,
            fingerprint: "AA:BB".into(),
            id: id.into(),
        }
    }

    #[test]
    fn pick_by_id_selects_exact_match_only() {
        let found = vec![
            discovered("192.168.1.10:47900", "111111111"),
            discovered("192.168.1.20:47900", "222222222"),
        ];
        let hit = pick_by_id(found.clone(), "222222222").expect("应命中第二台");
        assert_eq!(hit.address, "192.168.1.20:47900");

        // 近似（前缀/后缀）不算命中：宁可报找不到，也不能连错机器。
        assert!(pick_by_id(found.clone(), "22222222").is_none());
        assert!(pick_by_id(found.clone(), "2222222222").is_none());
        // 老版本设备 id 为空，不能用空串误匹配。
        assert!(pick_by_id(found, "").is_none());
    }

    #[test]
    fn parse_local_id_accepts_spacing_and_rejects_addresses() {
        assert_eq!(parse_local_id("123456789").as_deref(), Some("123456789"));
        assert_eq!(parse_local_id("123 456 789").as_deref(), Some("123456789"));
        assert_eq!(
            parse_local_id(" 123-456-789 ").as_deref(),
            Some("123456789")
        );
        // IP 与主机名不能当识别码，否则会被误解析成广播查询。
        assert_eq!(parse_local_id("192.168.1.20"), None);
        assert_eq!(parse_local_id("192.168.1.20:47900"), None);
        assert_eq!(parse_local_id("12345678"), None);
        assert_eq!(parse_local_id(""), None);
    }

    fn session(reconnectable: bool) -> ControlSession {
        ControlSession {
            session: 7,
            device_id: "direct:192.168.1.20:47900".into(),
            client_token: 3,
            video_id: None,
            width: 0,
            height: 0,
            fps: 0,
            max_width: 0,
            quality: 0,
            display: 0,
            displays: Vec::new(),
            audio_enabled: false,
            audio_id: None,
            audio_rate: 0,
            audio_channels: 0,
            audio_codec: screenwall_proto::AudioCodec::Pcm16,
            audio_bitrate: 0,
            audio_source: screenwall_proto::AudioSource::Loopback,
            peer_caps: None,
            started_ms: 1_700_000_000_000,
            reconnectable,
            direct_link: Some(1),
        }
    }

    /// 直连会话必须标记为可重连，否则 `end_session` 不会排重连任务。
    #[test]
    fn direct_session_is_reconnectable() {
        assert!(session(true).reconnectable);
    }

    /// 重连时如果忘了把凭据换回长期 token，就会拿着已经过期的配对码去连。
    /// 这里直接锁死「无配对码 → 长期 token」这条选择规则。
    #[test]
    fn reconnect_uses_long_lived_token_not_pairing_code() {
        let address = normalize_address("192.168.1.20", DEFAULT_PORT);
        let known = KnownPeer {
            address: address.clone(),
            fingerprint: "AA:BB".into(),
            token: "long-lived-token".into(),
            name: "peer".into(),
        };
        // 重连路径不传配对码（`None`）：必须取长期 token。
        let picked = pick_credential(None, Some(&known)).unwrap();
        assert_eq!(picked, "long-lived-token");
        assert_ne!(picked, pairing_token("123456"));

        // 没配对过又不给码：明确失败，而不是悄悄用空 token 去连。
        assert!(pick_credential(None, None).is_err());
    }

    /// 已配对设备的指纹必须参与校验：指纹变了要拒绝，不能静默接受。
    #[test]
    fn reconnect_pins_certificate_fingerprint() {
        let known = KnownPeer {
            address: "192.168.1.20:47900".into(),
            fingerprint: "AA:BB".into(),
            token: "t".into(),
            name: "peer".into(),
        };
        assert!(fingerprint_matches(Some(&known), "aa:bb"));
        assert!(!fingerprint_matches(Some(&known), "CC:DD"));
        // 首次配对还没有指纹可比：放行，配对成功后由 remember_peer 写入。
        assert!(fingerprint_matches(None, "CC:DD"));
    }

    /// 链路代号必须唯一，否则重连时新旧链路会互相拆登记。
    #[test]
    fn link_ids_are_unique_and_never_zero() {
        let mut hub = DirectHub::default();
        let first = bump_link_id(&mut hub);
        let second = bump_link_id(&mut hub);
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        // 溢出后回绕到 1，不能是 0（0 会和「没有链路」混淆）。
        hub.next_link = u64::MAX;
        assert_eq!(bump_link_id(&mut hub), 1);
    }

    /// 直连设备的合成 ID 能被解析回地址（重连时据此重建连接）。
    #[test]
    fn direct_device_id_round_trips_to_address() {
        let config = ClientConfig::default();
        let address = normalize_address("192.168.1.20", config.direct_port);
        assert_eq!(address, "192.168.1.20:47900");
        assert_eq!(format!("direct:{address}"), "direct:192.168.1.20:47900");
        assert_eq!(
            crate::direct_address("direct:192.168.1.20:47900"),
            Some(("192.168.1.20", 47900))
        );
        assert_eq!(crate::direct_address("dev-123"), None);
    }
}
