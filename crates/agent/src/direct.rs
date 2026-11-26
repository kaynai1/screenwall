//! 局域网直连：在本机放一个「服务器桩」。
//!
//! 中转服务器只做一件事：按 `session` 号转发 tunnel 帧。这里省掉账号与中转，
//! 把主控端的 tunnel 帧直接交给 `cloud::session_task`，把它的输出直接发回去，
//! 于是采集、输入、文件、音频、剪贴板全部原样复用。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use screenwall_proto::cloud::TUNNEL_MAX_PAYLOAD;
use screenwall_proto::{
    codes, Capabilities, DisplayList, Limits, Message, PeerInfo, Welcome, PROTO_VERSION,
};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use crate::cloud;
use crate::server::AgentState;
use crate::session::constant_time_eq;

/// TLS / WebSocket 握手上限。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 5 秒内必须收到 hello，和中转握手保持一致。
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// 空闲超过这个时间就发 WebSocket Ping 保活。
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 同一 IP 在窗口期内允许的认证失败次数。超过就退避，挡暴力枚举 6 位配对码。
const AUTH_FAIL_LIMIT: u32 = 5;
/// 失败计数的滑动窗口。窗口内失败满 `AUTH_FAIL_LIMIT` 次后按倍数退避。
const AUTH_FAIL_WINDOW: Duration = Duration::from_secs(60);
/// 退避上限：别让攻击者把正常用户永久锁在外面。
const AUTH_BACKOFF_CAP: Duration = Duration::from_secs(30);

type Sink<S> = SplitSink<WebSocketStream<S>, WsMessage>;

/// 长期 token 新增时的落盘回调。
pub type PromoteHook = Arc<dyn Fn(&str) + Send + Sync>;

/// 直连白名单：当前允许连进来的 token 集合。
///
/// 分两类：配对 token 随配对码轮换、作废即失效；长期 token 在首次配对
/// 被授权后由 [`DirectAuth::promote`] 从配对 token 升上来，重启后从配置恢复。
#[derive(Default)]
pub struct DirectAuth {
    /// 配对 token → 过期时刻（`None` 表示不过期，仅测试用）。
    pairing: RwLock<HashMap<String, Option<Instant>>>,
    promoted: RwLock<HashSet<String>>,
    /// 当前活跃的直连连接数，界面用来显示「正在被控」。
    active: std::sync::atomic::AtomicUsize,
    /// 新提升一个长期 token 时通知宿主落盘。宿主重启后靠它恢复白名单，
    /// 否则「免配对码重连」只在被控端不重启的前提下成立。
    on_promote: StdMutex<Option<PromoteHook>>,
}

impl DirectAuth {
    /// 当前活跃连接数。
    pub fn active(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 连接建立 / 断开时增减计数。用守卫保证异常退出也会减回去。
    fn guard_active(self: &Arc<Self>) -> ActiveGuard {
        self.active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ActiveGuard(self.clone())
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// 放行一个配对 token，`ttl` 之后自动失效。
    ///
    /// 过期必须由这里管，不能只靠界面上的倒计时：界面只是不再显示这个码，
    /// 白名单里留着它的话，10 分钟后照样能连进来。
    pub async fn allow_for(&self, token: String, ttl: Duration) {
        self.pairing
            .write()
            .await
            .insert(token, Some(Instant::now() + ttl));
    }

    /// 放行一个配对 token，不过期（仅测试与内部使用）。
    pub async fn allow(&self, token: String) {
        self.pairing.write().await.insert(token, None);
    }

    /// 吊销一个 token；两类集合都清，避免「换码后长期 token 还留着」。
    pub async fn revoke(&self, token: &str) {
        self.pairing.write().await.remove(token);
        self.promoted.write().await.remove(token);
    }

    /// 只从「配对 token」集合里撤掉，长期集合保留。
    ///
    /// 配对成功后配对码要立刻作废（否则这个码永久有效，谁拿到都能连），
    /// 但用这个码连上来的那台设备已经升级成长期凭据，不能跟着一起吊销。
    pub async fn revoke_pairing(&self, token: &str) {
        self.pairing.write().await.remove(token);
    }

    /// 整体替换长期 token 集合（配对码轮换时用；配对 token 不受影响）。
    pub async fn replace_all(&self, tokens: impl IntoIterator<Item = String>) {
        let fresh: HashSet<String> = tokens.into_iter().collect();
        *self.promoted.write().await = fresh;
    }

    /// 注册「长期 token 新增」的落盘回调。宿主在启动时调一次。
    pub fn on_promote(&self, callback: PromoteHook) {
        *self.on_promote.lock().unwrap() = Some(callback);
    }

    /// 把一次性的配对 token 升成长期 token（授权通过时调用）。
    pub async fn promote(&self, token: &str) {
        // 配对码作废后这条 token 仍要能连，所以不能只留在 pairing 集合里。
        self.pairing.write().await.remove(token);
        let added = self.promoted.write().await.insert(token.to_string());
        if added {
            let callback = self.on_promote.lock().unwrap().clone();
            if let Some(callback) = callback {
                callback(token);
            }
        }
    }

    /// 长期 token 是否还有效（重启后从配置恢复用）。
    pub async fn is_promoted(&self, token: &str) -> bool {
        self.promoted.read().await.contains(token)
    }

    /// 当前放行的长期 token 条数（界面显示「已授权 N 台设备」）。
    pub async fn promoted_count(&self) -> usize {
        self.promoted.read().await.len()
    }

    /// 常量时间比较，防时序侧信道。过期的配对 token 一律拒绝。
    pub async fn is_allowed(&self, token: &str) -> bool {
        let now = Instant::now();
        let pairing = self.pairing.read().await;
        let promoted = self.promoted.read().await;
        let probe = token.as_bytes();
        // 不短路：两个集合都完整比一遍，避免用耗时泄露「命中了几个」。
        let mut allowed = false;
        for (candidate, expires) in pairing.iter() {
            let live = expires.is_none_or(|deadline| deadline > now);
            allowed |= live & constant_time_eq(probe, candidate.as_bytes());
        }
        for candidate in promoted.iter() {
            allowed |= constant_time_eq(probe, candidate.as_bytes());
        }
        allowed
    }
}

/// 按来源 IP 统计认证失败，挡住对 6 位配对码的暴力枚举。
///
/// 没有它的话，`000000`–`999999` 一百万次尝试在一个局域网里跑得完，
/// 而且每次失败都会 spawn 一个连接任务，本身也是个廉价的拒绝服务入口。
#[derive(Default)]
pub struct AuthLimiter {
    /// IP → (连续失败次数, **最后一次**失败的时刻)。
    ///
    /// 记「最后一次」而不是「第一次」：退避要按「距上次尝试过了多久」算，
    /// 否则一旦超限，整个窗口内都恒定拒绝，指数退避永远升不到第二级。
    failures: StdMutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl AuthLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这个 IP 现在是否该被拒绝。命中限流返回还需要等待的时长。
    pub fn blocked_for(&self, peer: IpAddr) -> Option<Duration> {
        let mut failures = self.failures.lock().unwrap();
        let (count, last) = failures.get(&peer).copied()?;
        let since_last = last.elapsed();
        if since_last >= AUTH_FAIL_WINDOW {
            // 很久没再失败，记录作废。
            failures.remove(&peer);
            return None;
        }
        if count < AUTH_FAIL_LIMIT {
            return None;
        }
        // 超限后按倍数退避：第 5 次失败后冷却 1 秒，之后每次翻倍，封顶 30 秒。
        let extra = count - AUTH_FAIL_LIMIT;
        let cooldown = Duration::from_secs(1u64 << extra.min(5)).min(AUTH_BACKOFF_CAP);
        let left = cooldown.saturating_sub(since_last);
        // 冷却已过就放行 —— 让对方再试一次，失败则计数继续上涨、下次冷却更长。
        (!left.is_zero()).then_some(left)
    }

    /// 记一次认证失败。
    pub fn record_failure(&self, peer: IpAddr) {
        let mut failures = self.failures.lock().unwrap();
        let entry = failures.entry(peer).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= AUTH_FAIL_WINDOW {
            // 隔了一个窗口才再失败，重新计数。
            *entry = (1, Instant::now());
        } else {
            entry.0 = entry.0.saturating_add(1);
            entry.1 = Instant::now();
        }
    }

    /// 认证成功后清掉这个 IP 的失败记录。
    pub fn record_success(&self, peer: IpAddr) {
        self.failures.lock().unwrap().remove(&peer);
    }

    /// 清空全部记录（关闭直连时调用，避免旧记录影响下次开启）。
    pub fn clear(&self) {
        self.failures.lock().unwrap().clear();
    }
}

/// 这个来源地址算不算「局域网」。
///
/// 拒绝一切公网地址（IPv4 全球可路由、IPv6 全球单播）。放行的包括：
/// 回环、RFC1918 私网、链路本地（IPv4 `169.254/16`、IPv6 `fe80::/10`）、
/// IPv6 唯一本地地址（`fc00::/7`）、以及 IPv6 内嵌 IPv4 的映射地址
/// （双栈监听时内核可能给出 `::ffff:192.168.1.5`，要先还原成 IPv4 再判）。
pub fn is_lan_peer(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local()
            // 0.0.0.0 只可能出现在「不知道来源」的兜底路径上，不当局域网处理。
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_private() || v4.is_link_local();
            }
            // 唯一本地地址 fc00::/7 与链路本地 fe80::/10。
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// 活跃连接计数的守卫：无论 `serve` 正常结束还是提前返回，计数都会减回去。
struct ActiveGuard(Arc<DirectAuth>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 监听直连连接。每条连接一个任务，错误只记日志不中断监听。
pub async fn run(
    listener: TcpListener,
    state: Arc<AgentState>,
    tls: TlsAcceptor,
    auth: Arc<DirectAuth>,
    limiter: Arc<AuthLimiter>,
) -> Result<()> {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                // accept 失败通常只是瞬时资源问题，退让一下再继续，别把监听拖死。
                tracing::warn!(%error, "直连接口 accept 失败");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // 只接受局域网来源。
        //
        // 端口绑在 `0.0.0.0` 上是为了多网卡（有线 + 无线 + 虚拟网卡都能听），
        // 但这不等于愿意接受公网连接。路由器做了端口映射、或者机器直接挂在
        // 公网时，`0.0.0.0` 会让外面的人也连进来 —— 而直连的信任模型是
        // 「同一个局域网里的人」，不打算承受来自公网的爆破（哪怕有限流）。
        //
        // 判据用「不是公网」而不是「是私网」：IPv6 链路本地（fe80::/10）
        // 和 IPv4 链路本地（169.254.0.0/16）也算局域网，`is_private()` 不覆盖它们。
        if !is_lan_peer(&peer.ip()) {
            tracing::warn!(%peer, "直连只接受局域网来源，已拒绝");
            drop(tcp);
            continue;
        }
        // 被限流的 IP 直接断开，连 TLS 握手都不做 —— 否则每次尝试仍要
        // 付一次握手成本，等于免费给攻击者一个 CPU 消耗入口。
        if let Some(wait) = limiter.blocked_for(peer.ip()) {
            tracing::warn!(
                %peer,
                wait_ms = wait.as_millis() as u64,
                "直连认证失败次数过多，暂时拒绝"
            );
            drop(tcp);
            continue;
        }
        let state = state.clone();
        let tls = tls.clone();
        let auth = auth.clone();
        let limiter = limiter.clone();
        tokio::spawn(async move {
            // 计数守卫跟着连接走：提前返回也会减回去。
            let _active = auth.guard_active();
            match serve(tcp, tls, state, auth, limiter).await {
                Ok(()) => tracing::debug!(%peer, "直连连接正常结束"),
                Err(error) => tracing::warn!(%peer, error = %error, "直连连接中断"),
            }
        });
    }
}

async fn serve(
    tcp: TcpStream,
    tls: TlsAcceptor,
    state: Arc<AgentState>,
    auth: Arc<DirectAuth>,
    limiter: Arc<AuthLimiter>,
) -> Result<()> {
    let peer_ip = tcp
        .peer_addr()
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let _ = tcp.set_nodelay(true);
    let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
        Ok(result) => result?,
        Err(_) => bail!("直连 TLS 握手超时"),
    };
    serve_stream(stream, peer_ip, state, auth, limiter).await
}

/// 在一条**已经加密**的流上跑直连协议。
///
/// 传输无关：TCP + TLS（同网段）和 QUIC（跨网段打洞）都走这里，
/// 所以握手、授权、会话派发、缓存补投只有一份实现。
pub async fn serve_stream<S>(
    stream: S,
    peer_ip: IpAddr,
    state: Arc<AgentState>,
    auth: Arc<DirectAuth>,
    limiter: Arc<AuthLimiter>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ws = match tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::accept_async(stream))
        .await
    {
        Ok(result) => result?,
        Err(_) => bail!("直连 WebSocket 握手超时"),
    };
    let (mut sink, mut incoming) = ws.split();

    // ---- 握手：5 秒内必须收到 hello
    let first = match tokio::time::timeout(HELLO_TIMEOUT, incoming.next()).await {
        Ok(Some(Ok(frame))) => frame,
        Ok(Some(Err(error))) => return Err(error.into()),
        Ok(None) => bail!("握手前连接关闭"),
        Err(_) => {
            close_with(&mut sink, codes::HANDSHAKE_TIMEOUT, "handshake timeout").await;
            bail!("直连握手超时");
        }
    };
    let hello = match first {
        WsMessage::Text(text) => match Message::parse(text.as_str()) {
            Ok(Message::Hello(hello)) => hello,
            _ => {
                close_with(&mut sink, codes::BAD_REQUEST, "first message must be hello").await;
                bail!("首条消息不是 hello");
            }
        },
        _ => {
            close_with(&mut sink, codes::BAD_REQUEST, "first message must be text").await;
            bail!("首条消息不是文本帧");
        }
    };

    if hello.proto != PROTO_VERSION {
        close_with(&mut sink, codes::PROTO_UNSUPPORTED, "unsupported protocol").await;
        bail!("协议版本不匹配: {}", hello.proto);
    }
    if !auth.is_allowed(&hello.token).await {
        limiter.record_failure(peer_ip);
        close_with(&mut sink, codes::AUTH_FAILED, "auth failed").await;
        bail!("直连 token 不在白名单");
    }
    limiter.record_success(peer_ip);
    // 直连绕过了账号服务器，安全边界要求每次会话都必须有人在本机点「允许」，
    // 所以没有授权回调就整条连接拒绝，而不是退化成「直接放行」。
    let Some(inner_approval) = state.approval.clone() else {
        close_with(&mut sink, codes::AUTH_FAILED, "approval required").await;
        bail!("直连必须经过本机授权（宿主未提供授权回调）");
    };

    // 输出统一走通道：会话任务和读循环都能往里写，由写任务独占 sink。
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    // 本连接的授权回调外包一层：本机点「允许」之后把配对 token 升成长期 token，
    // 这样配对码作废/被控端重启后，主控端存的 token 仍然有效。
    // 拒绝时补一条 `closed`：直连是先回 open.ok 再走授权，主控端已经拿到会话号，
    // 不告诉它会话没建起来，它会一直等 welcome。
    let state = Arc::new(AgentState {
        config: state.config.clone(),
        displays: state.displays.clone(),
        approval: Some(promoting_approval(
            inner_approval,
            auth.clone(),
            hello.token.clone(),
            out_tx.clone(),
        )),
    });

    // 对端没声明 caps 时按 v1 基线处理；Welcome 只回双方交集，和中转入口一致。
    let agreed = Capabilities::with_peer_and(hello.caps.clone(), crate::h264::capabilities());
    tracing::info!(
        client = %hello.client.name,
        caps = %agreed,
        "局域网直连客户端已连接"
    );

    // ---- 欢迎与初始状态：与中转的握手层一致，先让主控端拿到设备信息
    if !send_text(
        &out_tx,
        &Message::Welcome(Welcome {
            proto: PROTO_VERSION,
            agent: PeerInfo {
                name: state.config.name.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            limits: Limits::defaults(),
            caps: agreed,
        }),
    ) {
        writer.abort();
        return Ok(());
    }
    if !send_text(
        &out_tx,
        &Message::DisplayList(DisplayList {
            displays: state.displays.clone(),
        }),
    ) {
        writer.abort();
        return Ok(());
    }
    if !send_text(&out_tx, &cloud::build_status(&state)) {
        writer.abort();
        return Ok(());
    }

    let sessions = cloud::new_sessions();
    // 直连先回 open.ok 再走授权（和服务器顺序一致），主控端拿到会话号后
    // 会立刻发 hello / video.sub；那时 `open_session` 可能还在等人点「允许」，
    // 会话通道尚未登记。这些帧先按会话号缓存，登记后原序补投，否则 hello 会丢，
    // 会话任务 5 秒后握手超时。表里「有键」就代表这个会话还在授权中。
    let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
    // 每条连接的 session 号独立计数：`sessions` 是本连接私有的，
    // 和服务器按账号分配会话号互不干扰。
    let mut next_session: u32 = 1;

    loop {
        let item = tokio::time::timeout(IDLE_TIMEOUT, incoming.next()).await;
        let message = match item {
            Err(_) => {
                let _ = out_tx.send(WsMessage::Ping(Vec::new().into()));
                continue;
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(Some(Ok(message))) => message,
        };

        match message {
            WsMessage::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let session = value
                    .get("session")
                    .and_then(Value::as_u64)
                    .map(|id| id as u32);
                match value.get("t").and_then(Value::as_str).unwrap_or_default() {
                    "open" => {
                        let session = next_session;
                        next_session = next_session.wrapping_add(1).max(1);
                        let request = value.get("request").and_then(Value::as_u64).unwrap_or(0);
                        let viewer = value
                            .get("viewer")
                            .and_then(Value::as_str)
                            .unwrap_or("未知设备")
                            .to_string();
                        // 先回带 request 的 open.ok，主控端据此解析会话号；
                        // `open_session` 自己发的那条不带 request，只是服务器协议的确认。
                        send_control(
                            &out_tx,
                            json!({
                                "t": "open.ok",
                                "request": request,
                                "session": session,
                                "deviceId": "local",
                            }),
                        );
                        // 先占住「授权中」标记，保证 open.ok 之后立刻到达的帧
                        // 一定进缓存，而不是赶在会话登记前被直接派发丢掉。
                        pending.lock().unwrap().entry(session).or_default();
                        // 授权要等人点按钮，不能卡住读循环。
                        let task_state = state.clone();
                        let task_out = out_tx.clone();
                        let task_sessions = sessions.clone();
                        tokio::spawn(async move {
                            cloud::open_session(
                                &task_state,
                                session,
                                viewer,
                                false,
                                // 直连保持「每次确认」，不做免弹窗。
                                false,
                                &task_out,
                                &task_sessions,
                            )
                            .await;
                        });
                        spawn_flush(session, sessions.clone(), pending.clone());
                    }
                    "close" => {
                        if let Some(session) = session {
                            cloud::close_session(&sessions, session);
                            pending.lock().unwrap().remove(&session);
                            tracing::info!(session, "主控端关闭直连会话");
                        }
                    }
                    "ping" => send_control(&out_tx, json!({ "t": "pong", "ts": now_ms() })),
                    _ => {}
                }
            }
            WsMessage::Binary(data) => {
                let Ok((header, _)) = screenwall_proto::cloud::decode_tunnel_frame(&data) else {
                    continue;
                };
                // 锁住缓存表期间判断并写入，避免补投线程刚好在这一刻清表，
                // 造成「既没补投、也没直接派发」的丢帧窗口。
                let mut pending = pending.lock().unwrap();
                if let Some(buffer) = pending.get_mut(&header.session) {
                    // 授权中：先缓存，登记后原序补投。真正的握手只有 hello 和
                    // 一两条订阅，这里给足余量，同时防止对端在授权弹窗期间
                    // 灌满内存。
                    if data.len() <= TUNNEL_MAX_PAYLOAD
                        && buffer.bytes + data.len() <= PENDING_MAX_BYTES
                    {
                        buffer.bytes += data.len();
                        buffer.frames.push(data.to_vec());
                    }
                    continue;
                }
                drop(pending);
                cloud::dispatch(&data, &sessions);
            }
            WsMessage::Ping(payload) => {
                let _ = out_tx.send(WsMessage::Pong(payload));
            }
            WsMessage::Pong(_) => {}
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    // 连接结束：本连接的所有会话一起收掉，避免采集线程和输入状态残留。
    let open_sessions: Vec<u32> = {
        let sessions = sessions.lock().unwrap();
        sessions.keys().copied().collect()
    };
    for session in open_sessions {
        cloud::close_session(&sessions, session);
    }
    pending.lock().unwrap().clear();
    writer.abort();
    Ok(())
}

/// 单个会话在授权期间最多缓存多少字节。
const PENDING_MAX_BYTES: usize = 4 * 1024 * 1024;

/// 授权期间缓存下来的会话帧，按会话号分桶。
type Pending = Arc<StdMutex<HashMap<u32, PendingBucket>>>;

#[derive(Default)]
struct PendingBucket {
    frames: Vec<Vec<u8>>,
    bytes: usize,
}

/// 等 `open_session` 把会话登记进 `sessions` 后，把授权期间缓存的帧按原序补投。
fn spawn_flush(session: u32, sessions: cloud::Sessions, pending: Pending) {
    tokio::spawn(async move {
        // 授权最长 30 秒，留足余量；被拒时没人登记，超时后直接丢掉缓存。
        for _ in 0..7000 {
            // 补投期间一直持有缓存表的锁：读循环此刻要么等锁，要么在补投结束后
            // 才看到表里没有这个会话而直接派发，不会插到缓存帧前面。
            let mut pending = pending.lock().unwrap();
            if sessions.lock().unwrap().contains_key(&session) {
                let buffered = pending.remove(&session).unwrap_or_default();
                for frame in buffered.frames {
                    cloud::dispatch(&frame, &sessions);
                }
                return;
            }
            drop(pending);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pending.lock().unwrap().remove(&session);
    });
}

/// 包一层授权回调：宿主点「允许」后把本连接用的配对 token 升成长期 token。
///
/// 不能等 `open_session` 返回——那时会话已经跑完，所以这里自己接住 oneshot，
/// 先回给 `open_session` 再异步提升。拒绝时额外通知主控端会话没建起来。
fn promoting_approval(
    inner: crate::server::ApprovalFn,
    auth: Arc<DirectAuth>,
    token: String,
    out: mpsc::UnboundedSender<WsMessage>,
) -> crate::server::ApprovalFn {
    Arc::new(move |request: crate::server::ApprovalRequest| {
        let session = request.session;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        inner(crate::server::ApprovalRequest {
            session,
            viewer: request.viewer,
            reply: reply_tx,
        });
        let auth = auth.clone();
        let token = token.clone();
        let out = out.clone();
        let reply = request.reply;
        // 授权可能等几十秒，不能阻塞调用方。
        tokio::spawn(async move {
            let allowed = reply_rx.await.unwrap_or(false);
            if allowed {
                auth.promote(&token).await;
            } else {
                // 直连先回 open.ok 再走授权，主控端已经拿到会话号；
                // 不补这条它会一直等 welcome。
                send_control(
                    &out,
                    json!({
                        "t": "closed",
                        "session": session,
                        "code": codes::CONTROL_DISABLED,
                        "msg": "被控端拒绝了本次请求"
                    }),
                );
            }
            let _ = reply.send(allowed);
        });
    })
}

async fn close_with<S>(sink: &mut Sink<S>, code: u16, reason: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = WsMessage::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    }));
    if let Err(error) = sink.send(frame).await {
        tracing::debug!(%error, "发送关闭帧失败");
    }
    sink.close().await.ok();
}

/// 控制类文本消息（握手层的 welcome / status / open.ok / pong）。
fn send_text(out: &mpsc::UnboundedSender<WsMessage>, message: &Message) -> bool {
    out.send(WsMessage::Text(message.to_json().into())).is_ok()
}

fn send_control(out: &mpsc::UnboundedSender<WsMessage>, value: Value) {
    let _ = out.send(WsMessage::Text(value.to_string().into()));
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))
    }

    #[test]
    fn lan_peer_accepts_local_ranges() {
        for text in [
            "127.0.0.1",
            "192.168.1.20",
            "10.0.0.5",
            "172.16.3.4",
            "169.254.10.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:192.168.1.7",
        ] {
            let ip: IpAddr = text.parse().unwrap();
            assert!(is_lan_peer(&ip), "{text} 应算局域网");
        }
    }

    #[test]
    fn lan_peer_rejects_public_addresses() {
        for text in [
            "8.8.8.8",
            "1.1.1.1",
            "203.0.113.9",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            let ip: IpAddr = text.parse().unwrap();
            assert!(!is_lan_peer(&ip), "{text} 是公网地址，必须拒绝");
        }
    }

    #[test]
    fn limiter_ignores_below_threshold() {
        let limiter = AuthLimiter::new();
        for _ in 0..AUTH_FAIL_LIMIT - 1 {
            limiter.record_failure(peer());
        }
        assert!(limiter.blocked_for(peer()).is_none(), "未达上限不该拦");
    }

    #[test]
    fn limiter_only_blocks_offending_ip() {
        let limiter = AuthLimiter::new();
        for _ in 0..AUTH_FAIL_LIMIT {
            limiter.record_failure(peer());
        }
        let other = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 51));
        assert!(limiter.blocked_for(peer()).is_some());
        assert!(limiter.blocked_for(other).is_none(), "别的 IP 不该被连坐");
    }

    #[test]
    fn limiter_clears_on_success() {
        let limiter = AuthLimiter::new();
        for _ in 0..AUTH_FAIL_LIMIT {
            limiter.record_failure(peer());
        }
        assert!(limiter.blocked_for(peer()).is_some());
        limiter.record_success(peer());
        assert!(limiter.blocked_for(peer()).is_none(), "成功一次应清零");
    }

    #[test]
    fn limiter_backoff_grows_then_releases() {
        let limiter = AuthLimiter::new();
        for _ in 0..AUTH_FAIL_LIMIT {
            limiter.record_failure(peer());
        }
        // 第 5 次失败后冷却 1 秒。
        let first = limiter.blocked_for(peer()).expect("应处于冷却");
        assert!(first <= Duration::from_secs(1), "首档冷却不超过 1 秒");

        // 冷却过了必须放行 —— 这是原实现的 bug：退避算出来了但永不生效，
        // 整个 60 秒窗口内恒定拒绝，指数退避根本升不到第二级。
        std::thread::sleep(Duration::from_millis(1100));
        assert!(limiter.blocked_for(peer()).is_none(), "冷却结束应放行一次");

        // 再失败一次，冷却翻倍。
        limiter.record_failure(peer());
        let second = limiter.blocked_for(peer()).expect("再次进入冷却");
        assert!(second > Duration::from_millis(900), "第二档冷却应接近 2 秒");
    }

    #[test]
    fn limiter_cooldown_is_capped() {
        let limiter = AuthLimiter::new();
        // 远超上限的失败次数。
        for _ in 0..AUTH_FAIL_LIMIT + 20 {
            limiter.record_failure(peer());
        }
        let wait = limiter.blocked_for(peer()).expect("应处于冷却");
        assert!(
            wait <= AUTH_BACKOFF_CAP,
            "冷却不得超过 {AUTH_BACKOFF_CAP:?}"
        );
    }

    #[test]
    fn limiter_window_expiry_resets() {
        let limiter = AuthLimiter::new();
        for _ in 0..AUTH_FAIL_LIMIT {
            limiter.record_failure(peer());
        }
        assert!(limiter.blocked_for(peer()).is_some());
        // 直接把时间戳往前推，模拟窗口过期，不用真等 60 秒。
        {
            let mut failures = limiter.failures.lock().unwrap();
            let entry = failures.get_mut(&peer()).unwrap();
            entry.1 = Instant::now() - AUTH_FAIL_WINDOW - Duration::from_secs(1);
        }
        assert!(limiter.blocked_for(peer()).is_none(), "窗口过期应作废记录");
        assert!(
            limiter.failures.lock().unwrap().get(&peer()).is_none(),
            "记录应被清掉"
        );
    }
}
