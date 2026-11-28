//! 主动连接账号服务器，接收并处理中转会话。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use screenwall_proto::cloud::{
    decode_tunnel_frame, encode_tunnel_frame, TunnelKind, TUNNEL_MAX_PAYLOAD,
};
use screenwall_proto::{
    codes, encode_frame, ActualAudio, ActualVideo, AudioCodec, AudioStats, AudioSubOk,
    AudioVolumeOk, Capabilities, ClipboardSync, DisplayList, ErrorMsg, FileAccept, FileCancel,
    FileChunk, FileComplete, FileDone, FileGet, FileItem, FileListOk, FileOffer, FileProgress,
    FrameCodec, FrameHeader, FrameType, InputStatus, Limits, Message, PeerInfo, Pong, PowerOk,
    SasMethod, ScreenInfo, SecureAttentionOk, Stats, Status, SubInfo, VideoBitrate, VideoCodec,
    VideoSub, VideoSubOk, Welcome, WolOk, CLIPBOARD_IMAGE, DISPLAY_ALL, FILE_WINDOW_BYTES,
    INPUT_UIPI, PROTO_VERSION,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::audio::{self, AudioHandle};
use crate::capture::{self, CaptureOptions};
use crate::clipboard;
use crate::display::{self, MonitorRect};
use crate::files::{self, RecvSession};
use crate::input;
use crate::power;
use crate::server::{AgentState, ApprovalRequest};
use crate::stats;
use crate::webrtc_host::WebrtcHub;
use crate::wol;

/// 被控端授权等待时长：宿主界面不答复就拒绝。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// 会话内收到的应用层消息。
/// 直连通过 `Sessions` 别名间接引用它，所以这里必须是 `pub(crate)`。
pub(crate) enum Inner {
    Text(String),
    Binary(Vec<u8>),
}

/// 一条服务器/直连连接上挂着所有会话的输入通道。
pub(crate) type Sessions = Arc<StdMutex<HashMap<u32, mpsc::UnboundedSender<Inner>>>>;

pub(crate) fn new_sessions() -> Sessions {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// 关掉一个会话：丢掉输入通道，`session_task` 的 `recv` 随即返回 None 并自行收尾。
pub(crate) fn close_session(sessions: &Sessions, session: u32) {
    sessions.lock().unwrap().remove(&session);
}

/// 文本消息处理完后需要执行的画面操作。
enum VideoCommand {
    Subscribe(VideoSub),
    Unsubscribe(u16),
    /// 主控端解码器丢了参考帧，要求下一帧强制关键帧（H.264）。
    Keyframe(u16),
    /// 主控端按拥塞情况实时调整目标码率（`video.bitrate`）。
    Bitrate(VideoBitrate),
}

/// 同账号会话的 P2P 打洞服务（被控端侧）。
///
/// 中转连接只负责把「主控端想直连」的请求转过来；真正的打洞、QUIC 服务端
/// 与会话派发由宿主实现。做成 trait 而不是写死在这里，是因为只有客户端
/// （Tauri 应用）具备证书、直连白名单和授权回调，纯 agent 二进制没有。
pub trait P2pProvider: Send + Sync {
    /// 主控端请求 P2P 直连。
    ///
    /// `request` 由主控端分配、原样带回；`candidates` 是主控端的候选地址
    /// （公网映射 + 内网地址）。实现方应尽快通过 `out` 回一条
    /// `{"t":"p2p","request":…,"viewerId":…,"event":"answer"|"reject",…}`，
    /// 否则主控端会在超时后回退到服务器中转。
    fn handle_offer(
        &self,
        request: u32,
        viewer_id: u64,
        viewer: String,
        candidates: Vec<String>,
        out: mpsc::UnboundedSender<WsMessage>,
    );
}

pub struct CloudOptions {
    pub server_url: String,
    pub token: String,
    pub device_id: String,
    pub name: String,
    /// 本机识别码（9 位数字）；空串表示还没生成，不上报。
    pub local_id: String,
    /// 自定义 CA 根证书路径；`None` 表示用系统默认信任库。
    pub ca_path: Option<std::path::PathBuf>,
    /// P2P 打洞服务；`None` 表示只走中转。
    pub p2p: Option<Arc<dyn P2pProvider>>,
}

impl std::fmt::Debug for CloudOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudOptions")
            .field("server_url", &self.server_url)
            .field("device_id", &self.device_id)
            .field("name", &self.name)
            .field("p2p", &self.p2p.is_some())
            .finish()
    }
}

impl Clone for CloudOptions {
    fn clone(&self) -> Self {
        Self {
            server_url: self.server_url.clone(),
            token: self.token.clone(),
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            local_id: self.local_id.clone(),
            ca_path: self.ca_path.clone(),
            p2p: self.p2p.clone(),
        }
    }
}

impl CloudOptions {
    /// 只有同时配置了服务器地址和账号令牌，才启用中转模式。
    pub fn from_state(state: &AgentState) -> Option<Self> {
        let server_url = state.config.server_url.trim();
        let token = state.config.account_token.trim();
        if server_url.is_empty() || token.is_empty() {
            return None;
        }
        let ca_path = state.config.ca_path.trim();
        Some(Self {
            server_url: server_url.to_string(),
            token: token.to_string(),
            device_id: state.config.device_id.clone(),
            name: state.config.name.clone(),
            local_id: state.config.local_id.clone(),
            ca_path: if ca_path.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(ca_path))
            },
            // 纯 agent 二进制没有证书与直连白名单，不提供 P2P；
            // 客户端会把它替换成真正的实现。
            p2p: None,
        })
    }
}

pub async fn run(state: Arc<AgentState>, options: CloudOptions) -> Result<()> {
    // WebRTC 信令和账号连接同生命周期：重连后旧的 PeerConnection 已被丢弃，
    // 主控端会重新发起协商。
    let webrtc = WebrtcHub::new(state.clone());
    let mut backoff = 2u64;
    loop {
        let started = Instant::now();
        match connect_once(state.clone(), &options, &webrtc).await {
            Ok(()) => tracing::warn!("与服务器的连接已关闭"),
            Err(error) => tracing::warn!("连接服务器失败: {error:#}"),
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = 2;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn connect_once(
    state: Arc<AgentState>,
    options: &CloudOptions,
    webrtc: &Arc<WebrtcHub>,
) -> Result<()> {
    let url = device_url(options, &state.gpu, &state.ip)?;
    let mut request = url
        .clone()
        .into_client_request()
        .with_context(|| format!("服务器地址无效: {url}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", options.token)
            .parse()
            .context("令牌格式无效")?,
    );

    // 配置了自定义 CA 就只信任它（同 curl --cacert），否则走系统默认信任库。
    let connector =
        crate::ca::connector_from_path(options.ca_path.as_deref()).context("加载自定义 CA 失败")?;
    // 关掉 Nagle：被控端上行是持续的视频帧，下行是键鼠和心跳；Nagle 会把
    // 小包压到有 ACK 才发，交互延迟最坏多出 40ms。
    let (ws, _) = match connector {
        Some(connector) => {
            tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector))
                .await
        }
        None => tokio_tungstenite::connect_async_with_config(request, None, true).await,
    }
    .context("WebSocket 连接失败")?;
    tracing::info!("已连接账号服务器 {}", options.server_url);

    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let sessions = new_sessions();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(60), stream.next()).await;
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
                handle_control(&state, options, text.as_str(), &out_tx, &sessions, webrtc).await;
            }
            WsMessage::Binary(data) => dispatch(&data, &sessions),
            WsMessage::Ping(payload) => {
                let _ = out_tx.send(WsMessage::Pong(payload));
            }
            WsMessage::Pong(_) => {}
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    sessions.lock().unwrap().clear();
    writer.abort();
    Ok(())
}

fn device_url(options: &CloudOptions, gpu: &str, ip: &str) -> Result<String> {
    let base = options.server_url.trim().trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    if !(base.starts_with("ws://") || base.starts_with("wss://")) {
        bail!("server_url 需要 http(s):// 或 ws(s):// 前缀");
    }
    Ok(format!(
        "{base}/ws/device?device_id={}&name={}&os=Windows&version={}&gpu={}&ip={}&local_id={}&webrtc=1",
        urlencoding::encode(&options.device_id),
        urlencoding::encode(&options.name),
        urlencoding::encode(env!("CARGO_PKG_VERSION")),
        urlencoding::encode(gpu),
        urlencoding::encode(ip),
        urlencoding::encode(&options.local_id)
    ))
}

async fn handle_control(
    state: &Arc<AgentState>,
    options: &CloudOptions,
    text: &str,
    out: &mpsc::UnboundedSender<WsMessage>,
    sessions: &Sessions,
    webrtc: &Arc<WebrtcHub>,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let session = value
        .get("session")
        .and_then(Value::as_u64)
        .map(|id| id as u32);

    match value.get("t").and_then(Value::as_str).unwrap_or_default() {
        "ready" => tracing::info!("服务器握手完成，等待主控端连接"),
        "open" => {
            let Some(session) = session else { return };
            if !state.config.allow_control {
                send_control(
                    out,
                    json!({
                        "t": "open.err",
                        "session": session,
                        "code": codes::CONTROL_DISABLED,
                        "msg": "本机已关闭被控"
                    }),
                );
                return;
            }

            // wall 是屏幕墙上的只读缩略图，不需要逐次授权；控制会话仍然要人点「允许」。
            let view_only = value.get("mode").and_then(Value::as_str) == Some("wall");
            // 连接码会话（trusted）已经用密码验证过身份，不再逐次弹窗。
            let trusted = value
                .get("trusted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // 同账号会话：主控端登录的是这台设备的账号主人，宿主可以免确认。
            let same_account = value
                .get("sameAccount")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // 授权要等人点按钮，必须放到独立任务里，否则会卡住整条服务器连接。
            let viewer = value
                .get("viewer")
                .and_then(Value::as_str)
                .unwrap_or("未知设备")
                .to_string();
            let state = state.clone();
            let out = out.clone();
            let sessions = sessions.clone();
            tokio::spawn(async move {
                open_session(
                    &state,
                    session,
                    viewer,
                    view_only,
                    trusted,
                    same_account,
                    &out,
                    &sessions,
                )
                .await;
            });
        }
        "close" => {
            if let Some(session) = session {
                close_session(sessions, session);
                tracing::info!("主控端关闭中转会话 {session}");
            }
        }
        "p2p" => {
            // 主控端希望绕过中转直连。这里只做「交给宿主去建链路」，
            // 真正的打洞在 `P2pProvider` 里；没有实现就静默忽略，
            // 主控端等不到 answer 会自己回退到中转。
            let (Some(provider), Some(request)) = (
                options.p2p.as_ref(),
                value
                    .get("request")
                    .and_then(Value::as_u64)
                    .map(|r| r as u32),
            ) else {
                return;
            };
            let viewer_id = value
                .get("viewerId")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let candidates: Vec<String> = value
                .get("candidates")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|addr| !addr.is_empty() && addr.len() <= 64)
                        .take(8)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if candidates.is_empty() {
                return;
            }
            // 「允许被控」总开关关闭时连中转会话都不建，P2P 当然也不能建。
            if !state.config.allow_control {
                send_control(
                    out,
                    json!({
                        "t": "p2p",
                        "request": request,
                        "viewerId": viewer_id,
                        "event": "reject",
                        "msg": "本机已关闭被控"
                    }),
                );
                return;
            }
            let viewer = value
                .get("viewer")
                .and_then(Value::as_str)
                .unwrap_or("未知设备")
                .to_string();
            provider.handle_offer(request, viewer_id, viewer, candidates, out.clone());
        }
        "webrtc" => {
            // 网页端 P2P 协商：浏览器发 offer，这里回 answer；后续 trickle ICE
            // 和挂断都按 request 路由到对应的连接任务。
            let viewer_id = value
                .get("viewerId")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            if viewer_id != 0 {
                webrtc.handle(viewer_id, &value, out);
            }
        }
        "ping" => send_control(out, json!({ "t": "pong", "ts": now_ms() })),
        _ => {}
    }
}

/// 从 `handle_control` 的 `"open"` 分支抽出来的会话启动逻辑：
/// 走授权 → 登记会话 → 发 `open.ok` → 跑 `session_task`。
/// 中转和直连共用；授权等待会阻塞当前任务，调用方必须放在 `tokio::spawn` 里。
// 授权相关的布尔量已经拆开命名，再并成一个枚举只会让调用点更难读。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_session(
    state: &Arc<AgentState>,
    session: u32,
    viewer: String,
    view_only: bool,
    auto_approve: bool,
    same_account: bool,
    out: &mpsc::UnboundedSender<WsMessage>,
    sessions: &Sessions,
) {
    if !view_only && !auto_approve {
        if let Some(approval) = &state.approval {
            let (tx, rx) = tokio::sync::oneshot::channel();
            approval(ApprovalRequest {
                session,
                viewer: viewer.clone(),
                same_account,
                // 中转会话没有长期凭据的概念：连接码已经由服务器验证过
                // （`trusted` 直接放行），其余情况都该由人来确认。
                authorized_before: false,
                reply: tx,
            });
            let allowed = match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
                Ok(Ok(allowed)) => allowed,
                Ok(Err(_)) => {
                    tracing::info!("授权请求 {session} 的答复通道已关闭，按拒绝处理");
                    false
                }
                Err(_) => {
                    tracing::info!("授权请求 {session} 超时未答复，按拒绝处理");
                    false
                }
            };
            if !allowed {
                tracing::info!("已拒绝 {viewer} 的控制请求（会话 {session}）");
                send_control(
                    out,
                    json!({
                        "t": "open.err",
                        "session": session,
                        "code": codes::CONTROL_DISABLED,
                        "msg": "被控端拒绝了本次请求"
                    }),
                );
                return;
            }
            tracing::info!("已允许 {viewer} 控制本机（会话 {session}）");
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<Inner>();
    sessions.lock().unwrap().insert(session, tx);
    // 直连还会额外发一条带 request 的 open.ok；这里这条是中转服务器约定的确认。
    send_control(out, json!({ "t": "open.ok", "session": session }));
    if view_only {
        tracing::info!(session, "主控端打开缩略图会话");
    } else {
        tracing::info!("主控端打开中转会话 {session}");
    }
    session_task(session, state.clone(), out.clone(), rx, view_only).await;
}

pub(crate) fn dispatch(data: &[u8], sessions: &Sessions) {
    let Ok((header, payload)) = decode_tunnel_frame(data) else {
        return;
    };
    if payload.len() > TUNNEL_MAX_PAYLOAD {
        return;
    }
    let tx = {
        let sessions = sessions.lock().unwrap();
        sessions.get(&header.session).cloned()
    };
    let Some(tx) = tx else {
        return;
    };
    let inner = match header.kind {
        TunnelKind::Json => match std::str::from_utf8(payload) {
            Ok(text) => Inner::Text(text.to_string()),
            Err(_) => return,
        },
        TunnelKind::Binary => Inner::Binary(payload.to_vec()),
    };
    let _ = tx.send(inner);
}

/// 把本机剪贴板的一次变化发给主控端；发送失败返回 false（调用方退出会话）。
///
/// `allow_image` 表示对端声明了 `clipboard.image`。没有声明时即使本机复制的是
/// 纯图片也不发——老主控端只认文本，发了它也会当成空文本丢掉，白白占带宽。
fn dispatch_clipboard(
    session: &u32,
    out: &mpsc::UnboundedSender<WsMessage>,
    clip_seq: &mut u32,
    allow_image: bool,
) -> bool {
    use crate::clipboard::Clip;
    let message = match clipboard::poll(clip_seq) {
        Some(Clip::Text(text)) => ClipboardSync::text(text),
        Some(Clip::Image(png)) if allow_image => {
            ClipboardSync::image(base64::engine::general_purpose::STANDARD.encode(&png))
        }
        // 对端不认图片：跳过这一轮，等下一次变化。
        Some(Clip::Image(_)) => return true,
        None => return true,
    };
    send_message(session, out, &Message::Clipboard(message))
}

/// 把对端发来的剪贴板内容写到本机剪贴板。
///
/// 优先按图片处理；图片解码或写入失败时退回文本，避免整条消息白收。
pub fn apply_clipboard(sync: &ClipboardSync) {
    if let Some(image) = sync.image.as_deref() {
        match base64::engine::general_purpose::STANDARD.decode(image) {
            Ok(bytes) => {
                if clipboard::write_image_png(&bytes) {
                    return;
                }
                tracing::debug!("剪贴板图片写入失败，尝试文本");
            }
            Err(error) => tracing::debug!("剪贴板图片 base64 解码失败: {error}"),
        }
    }
    if !sync.text.is_empty() {
        clipboard::write_text(&sync.text);
    }
}

async fn session_task(
    session: u32,
    state: Arc<AgentState>,
    out: mpsc::UnboundedSender<WsMessage>,
    mut rx: mpsc::UnboundedReceiver<Inner>,
    view_only: bool,
) {
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    let hello = match first {
        Ok(Some(Inner::Text(text))) => match Message::parse(&text) {
            Ok(Message::Hello(hello)) => hello,
            _ => {
                send_error(
                    &session,
                    &out,
                    codes::BAD_REQUEST,
                    "first message must be hello",
                );
                return;
            }
        },
        _ => {
            send_error(&session, &out, codes::BAD_REQUEST, "handshake timeout");
            return;
        }
    };

    if hello.proto != PROTO_VERSION {
        send_error(
            &session,
            &out,
            codes::PROTO_UNSUPPORTED,
            "unsupported protocol",
        );
        return;
    }

    // 对端没声明 caps 时按 v1 基线处理，这样老主控端仍然能用全部 v1 功能。
    let agreed = Capabilities::with_peer_and(hello.caps.clone(), crate::h264::capabilities());
    // 新主控端才认识 `input.status`；老主控端连解析都会失败，直接不发。
    let report_uipi = agreed.contains(INPUT_UIPI);
    tracing::info!(
        client = %hello.client.name,
        session,
        caps = %agreed,
        "中转会话已建立"
    );

    if !send_message(
        &session,
        &out,
        &Message::Welcome(Welcome {
            proto: PROTO_VERSION,
            agent: PeerInfo {
                name: state.config.name.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            limits: Limits::defaults(),
            // 回交集而不是本机全量：主控端据此直接判断可选功能，不用再猜。
            // 老主控端没声明 caps 时交集就是 v1 基线（不含 audio.aec）。
            caps: agreed.clone(),
            ip: state.ip.clone(),
            gpu: state.gpu.clone(),
        }),
    ) {
        return;
    }
    // 每次会话都重新枚举：显示器热插拔后主控端能立刻看到最新列表。
    if !send_message(
        &session,
        &out,
        &Message::DisplayList(DisplayList {
            displays: display::list_displays(),
        }),
    ) {
        return;
    }
    if !send_message(&session, &out, &build_status(&state)) {
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clipboard_tick = tokio::time::interval(Duration::from_millis(400));
    clipboard_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clip_seq = clipboard::sequence();

    // 本会话的出流量（视频 + 音频 + 文件块），用于 status.stats.kbps。
    let traffic = Arc::new(stats::ByteCounter::default());
    let reporter = stats::StatusReporter::new(traffic.clone());

    let mut video: Option<(u16, capture::CaptureHandle)> = None;
    // 当前视频订阅的对外描述，进 `status.subs`；没有订阅时为 None。
    let mut video_sub: Option<SubInfo> = None;
    // 一路系统声音（WASAPI 回环）；显式订阅才启动，退订或断开即停。
    let mut audio: Option<AudioOut> = None;
    // 主控端当前看的显示器；默认主屏，与首次 video.sub 的 display=0 一致。
    let mut input_region: Option<MonitorRect> = display::monitor_rect(0);
    let mut next_stream_id: u16 = 1;
    let mut next_audio_id: u16 = 1;
    // 上传接收状态与下载流控确认值，都按会话隔离。
    let mut recv: Option<RecvSession> = None;
    let mut sends: HashMap<u16, Arc<SendCtl>> = HashMap::new();
    // 下载：已发 offer，等主控端 accept 后再开始推数据。
    let mut pending: HashMap<u16, PendingSend> = HashMap::new();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let stats = reporter.stats();
                let subs = video_sub.iter().cloned().collect();
                if !send_message(&session, &out, &build_status_with(&state, stats, subs)) {
                    break;
                }
            }
            _ = clipboard_tick.tick() => {
                if !view_only && state.config.clipboard_sync {
                    if !dispatch_clipboard(
                        &session,
                        &out,
                        &mut clip_seq,
                        agreed.contains(CLIPBOARD_IMAGE),
                    ) {
                        break;
                    }
                } else {
                    clip_seq = clipboard::sequence();
                }
            }
            item = rx.recv() => {
                let Some(item) = item else { break };
                match item {
                    Inner::Text(text) => {
                        let message = match Message::parse(&text) {
                            Ok(message) => message,
                            Err(error) => {
                                send_error(
                                    &session,
                                    &out,
                                    codes::BAD_REQUEST,
                                    &format!("消息解析失败: {error}"),
                                );
                                continue;
                            }
                        };
                        // 缩略图会话只读：输入、文件、剪贴板、开机一律不执行。
                        if view_only && is_control_message(&message) {
                            tracing::debug!(session, "缩略图会话忽略了控制类消息");
                            continue;
                        }
                        let command = match message {
                            Message::FileOffer(offer) => {
                                if let Some(mut old) = recv.take() {
                                    old.abort();
                                }
                                match RecvSession::new(offer.tid, offer.items) {
                                    Ok(mut fresh) => {
                                        let resume = fresh.resume_list();
                                        tracing::info!(
                                            session,
                                            tid = offer.tid,
                                            files = fresh.root_label(),
                                            resumed = resume.len(),
                                            "开始接收上传文件"
                                        );
                                        send_message(
                                            &session,
                                            &out,
                                            &Message::FileAccept(FileAccept {
                                                tid: offer.tid,
                                                root: fresh.root_label(),
                                                resume,
                                                offset: 0,
                                            }),
                                        );
                                        if fresh.done >= fresh.total {
                                            // 空文件，或上次已经把最后一块写盘：直接收尾。
                                            finish_recv(&session, &out, &mut fresh);
                                        } else {
                                            recv = Some(fresh);
                                        }
                                    }
                                    Err(error) => send_error(
                                        &session,
                                        &out,
                                        codes::PATH_NOT_ALLOWED,
                                        &format!("无法接收文件: {error}"),
                                    ),
                                }
                                continue;
                            }
                            Message::FileChunk(chunk) => {
                                let Some(current) = recv.as_mut() else { continue };
                                if current.tid != chunk.tid {
                                    continue;
                                }
                                match current.write(&chunk) {
                                    Ok(due) => {
                                        if due {
                                            send_message(
                                                &session,
                                                &out,
                                                &Message::FileProgress(FileProgress {
                                                    tid: current.tid,
                                                    done: current.done,
                                                    total: current.total,
                                                }),
                                            );
                                        }
                                        if current.done >= current.total {
                                            finish_recv(&session, &out, current);
                                            recv = None;
                                        }
                                    }
                                    Err(error) => {
                                        let tid = chunk.tid;
                                        if let Some(mut current) = recv.take() {
                                            current.abort();
                                        }
                                        send_message(
                                            &session,
                                            &out,
                                            &Message::FileCancel(FileCancel {
                                                tid,
                                                msg: format!("接收失败: {error}"),
                                            }),
                                        );
                                        tracing::warn!(session, tid, %error, "上传接收失败");
                                    }
                                }
                                continue;
                            }
                            Message::FileListReq(request) => {
                                match files::list_dir(&request.path) {
                                    Ok((path, parent, entries)) => {
                                        send_message(
                                            &session,
                                            &out,
                                            &Message::FileListOk(FileListOk {
                                                tid: request.tid,
                                                path,
                                                parent,
                                                entries,
                                            }),
                                        );
                                    }
                                    Err(error) => send_error(
                                        &session,
                                        &out,
                                        codes::PATH_NOT_ALLOWED,
                                        &format!("读取目录失败: {error}"),
                                    ),
                                }
                                continue;
                            }
                            Message::FileGet(get) => {
                                prepare_file_send(&session, &out, &mut pending, get);
                                continue;
                            }
                            Message::FileAccept(accept) => {
                                if let Some(send) = pending.remove(&accept.tid) {
                                    spawn_file_stream(
                                        &session,
                                        &out,
                                        &mut sends,
                                        accept.tid,
                                        send,
                                        accept.offset,
                                        traffic.clone(),
                                    );
                                }
                                continue;
                            }
                            Message::FileCancel(cancel) => {
                                if let Some(mut current) = recv.take() {
                                    if current.tid == cancel.tid {
                                        current.abort();
                                        tracing::info!(
                                            session,
                                            tid = cancel.tid,
                                            "上传已被主控端取消"
                                        );
                                    } else {
                                        recv = Some(current);
                                    }
                                }
                                if let Some(ctl) = sends.remove(&cancel.tid) {
                                    ctl.cancelled.store(true, Ordering::Relaxed);
                                }
                                pending.remove(&cancel.tid);
                                continue;
                            }
                            Message::FileProgress(progress) => {
                                if let Some(ctl) = sends.get(&progress.tid) {
                                    ctl.confirmed.fetch_max(progress.done, Ordering::Relaxed);
                                }
                                continue;
                            }
                            Message::Clipboard(sync) => {
                                if state.config.clipboard_sync {
                                    apply_clipboard(&sync);
                                    clip_seq = clipboard::sequence();
                                }
                                continue;
                            }
                            Message::AudioSub(sub) => {
                                if view_only {
                                    tracing::debug!(session, "缩略图会话忽略了声音订阅");
                                    continue;
                                }
                                // 重复订阅就换一路新的；旧采集线程随句柄释放停止。
                                audio = None;
                                let id = next_audio_id;
                                next_audio_id = next_audio_id.wrapping_add(1).max(1);
                                let session_id = session;
                                let sender = out.clone();
                                let seq = Arc::new(AtomicU32::new(0));
                                // AudioSubOk 发出前不推帧，主控端拿到格式后再开始播放。
                                let gate = Arc::new(AtomicBool::new(false));
                                let sink_gate = gate.clone();
                                let bitrate = Arc::new(AtomicU32::new(0));
                                let capture_bitrate = bitrate.clone();
                                let wanted = sub.codec.unwrap_or(AudioCodec::Opus);
                                let source = sub.source.unwrap_or_default();
                                let audio_traffic = traffic.clone();
                                let started = tokio::task::spawn_blocking(move || {
                                    audio::spawn(wanted, source, capture_bitrate, move |chunk, codec| {
                                        if !sink_gate.load(Ordering::Relaxed) {
                                            return;
                                        }
                                        let header = FrameHeader {
                                            kind: FrameType::Audio,
                                            stream_id: id,
                                            seq: seq.fetch_add(1, Ordering::Relaxed),
                                            ts_us: unix_micros(),
                                            payload_len: 0,
                                            codec: match codec {
                                                AudioCodec::Opus => FrameCodec::Opus,
                                                AudioCodec::Pcm16 => FrameCodec::Pcm16,
                                            },
                                            flags: 0,
                                        };
                                        let frame = encode_frame(&header, &chunk);
                                        let tunnel = encode_tunnel_frame(
                                            TunnelKind::Binary,
                                            session_id,
                                            &frame,
                                        );
                                        if sender.send(WsMessage::Binary(tunnel.into())).is_ok() {
                                            audio_traffic.add(chunk.len());
                                        }
                                    })
                                })
                                .await;
                                match started {
                                    Ok(Ok((handle, format))) => {
                                        let initial_bitrate = format.bitrate;
                                        audio = Some(AudioOut {
                                            id,
                                            _handle: handle,
                                            bitrate,
                                            last_adjust: Instant::now(),
                                        });
                                        tracing::info!(
                                            session,
                                            stream = id,
                                            codec = ?format.codec,
                                            source = ?format.source,
                                            sample_rate = format.sample_rate,
                                            channels = format.channels,
                                            bitrate = format.bitrate,
                                            "声音订阅已启动"
                                        );
                                        if !send_message(
                                            &session,
                                            &out,
                                            &Message::AudioSubOk(AudioSubOk {
                                                id,
                                                actual: ActualAudio {
                                                    codec: format.codec,
                                                    sample_rate: format.sample_rate,
                                                    channels: format.channels,
                                                    bitrate: initial_bitrate,
                                                    source: format.source,
                                                },
                                            }),
                                        ) {
                                            break;
                                        }
                                        gate.store(true, Ordering::Relaxed);
                                    }
                                    Ok(Err(error)) => {
                                        tracing::warn!(session, %error, "启动系统声音采集失败");
                                        send_error(
                                            &session,
                                            &out,
                                            codes::AUDIO_UNAVAILABLE,
                                            &format!("无法采集系统声音: {error}"),
                                        );
                                    }
                                    Err(error) => {
                                        tracing::warn!(session, %error, "声音采集任务异常");
                                        send_error(
                                            &session,
                                            &out,
                                            codes::AUDIO_UNAVAILABLE,
                                            "系统声音采集任务异常",
                                        );
                                    }
                                }
                                continue;
                            }
                            Message::AudioUnsub(unsub) => {
                                if audio.as_ref().map(|current| current.id) == Some(unsub.id) {
                                    audio = None;
                                    tracing::info!(session, stream = unsub.id, "声音订阅已停止");
                                }
                                continue;
                            }
                            Message::AudioStats(stats) => {
                                if let Some(current) = audio.as_mut() {
                                    adjust_audio_bitrate(session, current, stats);
                                }
                                continue;
                            }
                            Message::AudioVolume(volume) => {
                                if view_only {
                                    continue;
                                }
                                let level = volume.level;
                                let muted = volume.muted;
                                let result = tokio::task::spawn_blocking(move || {
                                    audio::write_volume(level, muted)
                                })
                                .await;
                                match result {
                                    Ok(Ok((level, muted))) => {
                                        tracing::info!(session, level, muted, "已调整被控端音量");
                                        if !send_message(
                                            &session,
                                            &out,
                                            &Message::AudioVolumeOk(AudioVolumeOk { level, muted }),
                                        ) {
                                            break;
                                        }
                                    }
                                    Ok(Err(error)) => send_error(
                                        &session,
                                        &out,
                                        codes::AUDIO_UNAVAILABLE,
                                        &format!("调整音量失败: {error:#}"),
                                    ),
                                    Err(error) => send_error(
                                        &session,
                                        &out,
                                        codes::AUDIO_UNAVAILABLE,
                                        &format!("调整音量任务异常: {error}"),
                                    ),
                                }
                                continue;
                            }
                            Message::AudioVolumeGet(_) => {
                                if view_only {
                                    continue;
                                }
                                let result = tokio::task::spawn_blocking(audio::read_volume).await;
                                match result {
                                    Ok(Ok((level, muted))) => {
                                        tracing::debug!(session, level, muted, "读取被控端音量");
                                        if !send_message(
                                            &session,
                                            &out,
                                            &Message::AudioVolumeOk(AudioVolumeOk { level, muted }),
                                        ) {
                                            break;
                                        }
                                    }
                                    Ok(Err(error)) => send_error(
                                        &session,
                                        &out,
                                        codes::AUDIO_UNAVAILABLE,
                                        &format!("读取音量失败: {error:#}"),
                                    ),
                                    Err(error) => send_error(
                                        &session,
                                        &out,
                                        codes::AUDIO_UNAVAILABLE,
                                        &format!("读取音量任务异常: {error}"),
                                    ),
                                }
                                continue;
                            }
                            message => handle_message(
                                &session,
                                &out,
                                message,
                                input_region,
                                report_uipi,
                            ),
                        };
                        match command {
                            Some(VideoCommand::Subscribe(sub)) => {
                                // H.264 只在硬件编码器确实可用时才接受；否则明确
                                // 报错，主控端据此回退 MJPEG 并记住这次回退。
                                if sub.codec == VideoCodec::H264
                                    && !crate::h264::hardware_available()
                                {
                                    send_error(
                                        &session,
                                        &out,
                                        codes::ENCODER_UNAVAILABLE,
                                        "被控端没有可用的硬件 H.264 编码器",
                                    );
                                    continue;
                                }
                                // display = DISPLAY_ALL 时采集整块虚拟桌面，否则只采集那块屏。
                                let region = if sub.display == DISPLAY_ALL {
                                    None
                                } else {
                                    match display::monitor_rect(sub.display) {
                                        Some(rect) => Some(rect),
                                        None => {
                                            send_error(
                                                &session,
                                                &out,
                                                codes::DISPLAY_NOT_FOUND,
                                                &format!(
                                                    "显示器 {} 不存在",
                                                    sub.display.saturating_add(1)
                                                ),
                                            );
                                            continue;
                                        }
                                    }
                                };

                                // 控制流跟随主控端的帧率（0 = 自动，不设上限）；
                                // 缩略图会话强制低帧率、小尺寸，别让屏幕墙把 CPU 吃光。
                                let (fps_cap, width_cap) = if view_only {
                                    (2, 480)
                                } else {
                                    (capture::MAX_FPS, u32::MAX)
                                };
                                let options = CaptureOptions {
                                    fps: sub.fps.min(fps_cap),
                                    // 优先使用主控端请求的 maxWidth，未指定时才用默认档位。
                                    max_width: sub
                                        .max_width
                                        .unwrap_or(capture::DEFAULT_MAX_WIDTH)
                                        .min(width_cap),
                                    quality: sub.quality.unwrap_or(65),
                                    region,
                                    // 屏幕墙缩略图不画光标：既省带宽也避免预览里全是光标。
                                    cursor: sub.cursor && !view_only,
                                    // 屏幕墙不给码率上限（尺寸与帧率已经压到很低，
                                    // 再加预算只会让缩略图糊掉）。
                                    max_bitrate: if view_only { None } else { sub.max_bitrate },
                                }
                                .normalized();
                                // 无头机器：先确保有一块可采集的虚拟屏，否则
                                // DXGI/GDI 只能给出黑帧。已开物理屏时是空操作。
                                if state.config.virtual_display
                                    || crate::virtual_display::enabled()
                                {
                                    match tokio::task::spawn_blocking(|| {
                                        crate::virtual_display::ensure_display(Duration::from_secs(
                                            15,
                                        ))
                                    })
                                    .await
                                    {
                                        Ok(Ok(true)) => {}
                                        Ok(Ok(false)) => {}
                                        Ok(Err(error)) => {
                                            tracing::warn!("无头虚拟显示器不可用: {error}");
                                        }
                                        Err(error) => {
                                            tracing::warn!("无头虚拟显示器任务失败: {error}");
                                        }
                                    }
                                }
                                let id = next_stream_id;
                                next_stream_id = next_stream_id.wrapping_add(1).max(1);
                                let session_id = session;
                                let sender = out.clone();
                                let video_traffic = traffic.clone();
                                // 编码器创建失败时，编码线程会自己回退 MJPEG；
                                // 这里只按主控端选择的编码回报 `video.sub.ok`。
                                let codec = sub.codec;
                                let (handle, ready) =
                                    capture::spawn(id, options, codec, move |frame| {
                                        let bytes = frame.len();
                                        let tunnel = encode_tunnel_frame(
                                            TunnelKind::Binary,
                                            session_id,
                                            &frame,
                                        );
                                        if sender.send(WsMessage::Binary(tunnel.into())).is_ok() {
                                            video_traffic.add(bytes);
                                        }
                                    });

                                let started = tokio::time::timeout(
                                    Duration::from_secs(3),
                                    ready,
                                )
                                .await;
                                match started {
                                    Ok(Ok(Ok((w, h)))) => {
                                        // H.264 的宽高会补齐到偶数，`video.sub.ok`
                                        // 报编码后的实际尺寸，免得界面按差 1 像素算坐标。
                                        let (w, h) = if codec == VideoCodec::H264 {
                                            let (pw, ph) = crate::nv12::padded_dims(
                                                w as usize, h as usize,
                                            );
                                            (pw as u32, ph as u32)
                                        } else {
                                            (w, h)
                                        };
                                        video = Some((id, handle));
                                        video_sub = Some(SubInfo {
                                            id,
                                            stream: sub.stream,
                                            fps: options.fps,
                                        });
                                        // 输入坐标跟随画面：切换显示器后点击仍落在同一相对位置。
                                        input_region = options.region;
                                        tracing::info!(
                                            session,
                                            stream = id,
                                            display = sub.display,
                                            w,
                                            h,
                                            fps = options.fps,
                                            "画面订阅已启动"
                                        );
                                        if !send_message(
                                            &session,
                                            &out,
                                            &Message::VideoSubOk(VideoSubOk {
                                                id,
                                                stream: sub.stream,
                                                actual: ActualVideo {
                                                    codec,
                                                    w,
                                                    h,
                                                    fps: options.fps,
                                                },
                                            }),
                                        ) {
                                            break;
                                        }
                                    }
                                    Ok(Ok(Err(message))) => {
                                        send_error(
                                            &session,
                                            &out,
                                            codes::ENCODER_UNAVAILABLE,
                                            &message,
                                        );
                                    }
                                    _ => {
                                        send_error(
                                            &session,
                                            &out,
                                            codes::ENCODER_UNAVAILABLE,
                                            "屏幕采集启动超时",
                                        );
                                    }
                                }
                            }
                            Some(VideoCommand::Unsubscribe(id)) => {
                                // 退订的 id 与当前流不一致时不动（可能是旧流的迟到消息）。
                                let current_is = matches!(
                                    video.as_ref(),
                                    Some((current, _)) if *current == id
                                );
                                if current_is {
                                    video = None;
                                    video_sub = None;
                                    tracing::info!(session, stream = id, "画面订阅已停止");
                                }
                            }
                            Some(VideoCommand::Keyframe(id)) => {
                                // 只在请求的是当前流时处理；旧流的迟到请求忽略。
                                if let Some((current, handle)) = video.as_ref() {
                                    if *current == id {
                                        handle.request_keyframe();
                                        tracing::debug!(session, stream = id, "主控端请求关键帧");
                                    }
                                }
                            }
                            Some(VideoCommand::Bitrate(bitrate)) => {
                                // 只认当前流的 id；旧流的迟到消息忽略。
                                if let Some((current, handle)) = video.as_ref() {
                                    if *current == bitrate.id {
                                        handle.set_bitrate(bitrate.kbps);
                                        tracing::debug!(
                                            session,
                                            stream = bitrate.id,
                                            kbps = bitrate.kbps,
                                            "主控端实时调整码率"
                                        );
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                    Inner::Binary(data) => {
                        tracing::trace!(session, bytes = data.len(), "暂不处理二进制负载");
                    }
                }
            }
        }
    }

    // 对端可能没有发送 releaseAll 就断开，退出前释放所有按下的键。
    input::release_all();
    // 显式丢弃会通知采集线程退出。
    drop(video);
    drop(audio);
    tracing::info!(session, "中转会话结束");
}

/// 只读缩略图会话里会被忽略的消息：任何能改变被控端状态或泄露内容的请求。
fn is_control_message(message: &Message) -> bool {
    matches!(
        message,
        Message::Input(_)
            | Message::SecureAttention(_)
            | Message::InputStatus(_)
            | Message::Clipboard(_)
            | Message::Power(_)
            | Message::PowerOk(_)
            | Message::FileOffer(_)
            | Message::FileChunk(_)
            | Message::FileAccept(_)
            | Message::FileDeny(_)
            | Message::FileProgress(_)
            | Message::FileCancel(_)
            | Message::FileListReq(_)
            | Message::FileGet(_)
            | Message::AudioStats(_)
            | Message::AudioVolume(_)
            | Message::AudioVolumeGet(_)
            | Message::WolRequest(_)
            | Message::VideoBitrate(_)
    )
}

fn handle_message(
    session: &u32,
    out: &mpsc::UnboundedSender<WsMessage>,
    message: Message,
    input_region: Option<MonitorRect>,
    report_uipi: bool,
) -> Option<VideoCommand> {
    match message {
        Message::Ping(ping) => {
            send_message(session, out, &Message::Pong(Pong { ts: ping.ts }));
            None
        }
        Message::Input(batch) => {
            // SendInput 是同步调用，这里直接注入；事件解析与限流在 input 模块内完成。
            // 注入返回 Some 表示 UIPI 状态刚变化（或持续拦截到达重报间隔）。
            if let Some(update) = input::inject(&batch.events, input_region) {
                if report_uipi {
                    let uipi = update == input::UipiReport::Blocked;
                    send_message(
                        session,
                        out,
                        &Message::InputStatus(InputStatus {
                            uipi,
                            detail: uipi.then(|| {
                                "前台窗口的完整性级别高于被控端，SendInput 被 UIPI 拦截".to_string()
                            }),
                        }),
                    );
                }
            }
            None
        }
        Message::SecureAttention(request) => {
            let (method, detail) = input::secure_attention(request.as_user);
            match method {
                SasMethod::Sas => tracing::info!(session = *session, "已请求 SendSAS"),
                SasMethod::Keys => tracing::warn!(
                    session = *session,
                    detail = detail.as_deref().unwrap_or(""),
                    "SendSAS 不可用，已降级为普通按键"
                ),
            }
            send_message(
                session,
                out,
                &Message::SecureAttentionOk(SecureAttentionOk { method, detail }),
            );
            None
        }
        Message::Power(request) => {
            match power::schedule(request.action) {
                Ok(()) => {
                    tracing::warn!(action = ?request.action, "已安排远程电源操作");
                    send_message(
                        session,
                        out,
                        &Message::PowerOk(PowerOk {
                            action: request.action,
                        }),
                    );
                }
                Err(error) => send_error(
                    session,
                    out,
                    codes::POWER_FAILED,
                    &format!("电源操作失败: {error:#}"),
                ),
            }
            None
        }
        Message::VideoSub(sub) => Some(VideoCommand::Subscribe(sub)),
        Message::VideoUnsub(unsub) => Some(VideoCommand::Unsubscribe(unsub.id)),
        // MJPEG 每帧都是完整图像；H.264 下由视频订阅分支处理这个请求。
        Message::VideoKeyframe(keyframe) => Some(VideoCommand::Keyframe(keyframe.id)),
        Message::VideoBitrate(bitrate) => Some(VideoCommand::Bitrate(bitrate)),
        Message::WolRequest(request) => {
            let mac = &request.mac;
            if wol::normalize_mac(mac).is_none() {
                send_error(
                    session,
                    out,
                    codes::BAD_REQUEST,
                    &format!("MAC 地址无效: {mac}"),
                );
                return None;
            }
            let Some(broadcast) = wol::normalize_broadcast(&request.broadcast) else {
                let requested = &request.broadcast;
                send_error(
                    session,
                    out,
                    codes::BAD_REQUEST,
                    &format!("广播地址无效: {requested}"),
                );
                return None;
            };
            match wol::send_magic_packet(&request.mac, &broadcast) {
                Ok(()) => {
                    tracing::info!(
                        session = *session,
                        mac = %request.mac,
                        broadcast = %broadcast,
                        "已发送远程开机 magic packet"
                    );
                    send_message(session, out, &Message::WolOk(WolOk { mac: request.mac }));
                }
                Err(error) => send_error(
                    session,
                    out,
                    codes::BAD_REQUEST,
                    &format!("发送 magic packet 失败: {error:#}"),
                ),
            }
            None
        }
        Message::Hello(_) => {
            send_error(session, out, codes::BAD_REQUEST, "重复的 hello");
            None
        }
        Message::Error(error) => {
            tracing::warn!(code = error.code, msg = %error.msg, "客户端报告错误");
            None
        }
        _ => {
            send_error(session, out, codes::UNKNOWN_MESSAGE, "暂不支持该消息");
            None
        }
    }
}

/// 收完一次上传：逐文件回 done，最后回 complete。
fn finish_recv(session: &u32, out: &mpsc::UnboundedSender<WsMessage>, recv: &mut RecvSession) {
    let tid = recv.tid;
    let (results, bytes) = recv.finish();
    let files = results.len() as u32;
    for result in results {
        send_message(session, out, &Message::FileDone(result));
    }
    if !send_message(
        session,
        out,
        &Message::FileComplete(FileComplete { tid, files, bytes }),
    ) {
        return;
    }
    tracing::info!(session, tid, bytes, files, "上传接收完成");
}

/// 下载任务的流控状态：已确认字节 + 取消标记。
struct SendCtl {
    confirmed: AtomicU64,
    cancelled: AtomicBool,
}

impl SendCtl {
    fn new(confirmed: u64) -> Self {
        Self {
            confirmed: AtomicU64::new(confirmed),
            cancelled: AtomicBool::new(false),
        }
    }
}

/// 当前正在推送的系统声音：句柄释放即停止采集。
struct AudioOut {
    id: u16,
    _handle: AudioHandle,
    bitrate: Arc<AtomicU32>,
    last_adjust: Instant,
}

/// 按主控端播放缓冲调整 Opus 码率：积压就降，缓冲很浅就升回去。
fn adjust_audio_bitrate(session: u32, current: &mut AudioOut, stats: AudioStats) {
    if current.id != stats.id || current.last_adjust.elapsed() < Duration::from_secs(3) {
        return;
    }
    let from = current.bitrate.load(Ordering::Relaxed).max(1);
    let next = if stats.buffered_ms > 250 {
        (from * 3 / 4).max(24_000)
    } else if stats.buffered_ms < 80 {
        (from * 5 / 4).min(96_000)
    } else {
        from
    };
    if next != from {
        current.bitrate.store(next, Ordering::Relaxed);
        current.last_adjust = Instant::now();
        tracing::info!(
            session,
            buffered_ms = stats.buffered_ms,
            from,
            to = next,
            "调整系统声音码率"
        );
    }
}

/// 下载第一步：校验路径、发 offer，等主控端 accept 后再推数据。
struct PendingSend {
    file: std::fs::File,
    path: String,
    size: u64,
    hasher: Sha256,
}

fn prepare_file_send(
    session: &u32,
    out: &mpsc::UnboundedSender<WsMessage>,
    pending: &mut HashMap<u16, PendingSend>,
    get: FileGet,
) {
    let (file, size) = match files::open_for_send(&get.path) {
        Ok(value) => value,
        Err(error) => {
            send_error(
                session,
                out,
                codes::PATH_NOT_ALLOWED,
                &format!("无法发送文件: {error}"),
            );
            return;
        }
    };
    let name = Path::new(&get.path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| get.path.clone());
    if !send_message(
        session,
        out,
        &Message::FileOffer(FileOffer {
            tid: get.tid,
            items: vec![FileItem {
                path: name,
                size,
                sha256: String::new(),
            }],
            total_size: size,
        }),
    ) {
        return;
    }
    pending.insert(
        get.tid,
        PendingSend {
            file,
            path: get.path,
            size,
            hasher: Sha256::new(),
        },
    );
}

/// 收到 accept 后从续传偏移按 8MB 窗口推数据块，直到发完或对端取消。
///
/// `requested` 是主控端已持有的前缀；超过文件大小或读不出前缀时回退到 0。
/// 前缀也要计入哈希，`file.done.sha256` 才是整文件的。
fn spawn_file_stream(
    session: &u32,
    out: &mpsc::UnboundedSender<WsMessage>,
    sends: &mut HashMap<u16, Arc<SendCtl>>,
    tid: u16,
    send: PendingSend,
    requested: u64,
    traffic: Arc<stats::ByteCounter>,
) {
    let PendingSend {
        mut file,
        path,
        size,
        mut hasher,
    } = send;
    let start = files::valid_offset(requested, size);
    let ctl = Arc::new(SendCtl::new(start));
    sends.insert(tid, ctl.clone());
    let session_id = *session;
    let out = out.clone();
    tokio::spawn(async move {
        let mut offset = start;
        // 前缀哈希要读一遍已有字节，放到任务里做，别阻塞会话循环。
        // 读不出前缀（文件被截断/替换）就退回从头发送，主控端收到 0 偏移会重收。
        if offset > 0 {
            match files::hash_prefix(&mut file, offset) {
                Ok(prefix) => hasher = prefix,
                Err(error) => {
                    tracing::warn!(
                        session = session_id,
                        tid,
                        offset,
                        %error,
                        "下载续传起点无效，改为从 0 发送"
                    );
                    offset = 0;
                    // 改成从头发，流控基线也归零，避免把主控端已收字节当成确认。
                    ctl.confirmed.store(0, Ordering::Relaxed);
                }
            }
        }
        let mut last_seen = offset;
        let mut last_ack = Instant::now();
        while offset < size {
            // 主控端取消后立刻停下，不再继续推数据。
            if ctl.cancelled.load(Ordering::Relaxed) {
                return;
            }
            // 等接收端把在途字节压到窗口以内；30 秒没有确认就放弃。
            loop {
                if ctl.cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let acknowledged = ctl.confirmed.load(Ordering::Relaxed);
                if acknowledged != last_seen {
                    last_seen = acknowledged;
                    last_ack = Instant::now();
                }
                if offset.saturating_sub(acknowledged) <= FILE_WINDOW_BYTES {
                    break;
                }
                if last_ack.elapsed() > Duration::from_secs(30) {
                    let _ = send_message(
                        &session_id,
                        &out,
                        &Message::FileCancel(FileCancel {
                            tid,
                            msg: "等待接收端确认超时".into(),
                        }),
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let data = match files::read_chunk(&mut file, offset) {
                Ok(data) if !data.is_empty() => data,
                Ok(_) => break,
                Err(error) => {
                    let _ = send_message(
                        &session_id,
                        &out,
                        &Message::FileCancel(FileCancel {
                            tid,
                            msg: format!("读取失败: {error}"),
                        }),
                    );
                    return;
                }
            };
            hasher.update(&data);
            let chunk = FileChunk {
                tid,
                item: 0,
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            };
            let chunk_bytes = data.len();
            offset += chunk_bytes as u64;
            if !send_message(&session_id, &out, &Message::FileChunk(chunk)) {
                return;
            }
            traffic.add(chunk_bytes);
        }
        if offset < size {
            let _ = send_message(
                &session_id,
                &out,
                &Message::FileCancel(FileCancel {
                    tid,
                    msg: "文件在传输过程中被修改或截断".into(),
                }),
            );
            return;
        }
        let _ = send_message(
            &session_id,
            &out,
            &Message::FileDone(FileDone {
                tid,
                path,
                ok: true,
                msg: None,
                sha256: Some(hex::encode(hasher.finalize())),
            }),
        );
        let _ = send_message(
            &session_id,
            &out,
            &Message::FileComplete(FileComplete {
                tid,
                files: 1,
                bytes: size,
            }),
        );
    });
}

pub(crate) fn build_status(state: &AgentState) -> Message {
    build_status_with(state, Stats::default(), Vec::new())
}

/// 组装 `status`；`stats` 与 `subs` 由调用方按会话状态给出。
fn build_status_with(state: &AgentState, stats: Stats, subs: Vec<SubInfo>) -> Message {
    let primary = state
        .displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| state.displays.first());
    Message::Status(Status {
        screen: ScreenInfo {
            w: primary.map(|display| display.w).unwrap_or(0),
            h: primary.map(|display| display.h).unwrap_or(0),
            scale: 1.0,
        },
        subs,
        stats,
    })
}

fn send_message(session: &u32, out: &mpsc::UnboundedSender<WsMessage>, message: &Message) -> bool {
    let payload = message.to_json();
    let frame = encode_tunnel_frame(TunnelKind::Json, *session, payload.as_bytes());
    out.send(WsMessage::Binary(frame.into())).is_ok()
}

/// 音频帧时间戳：Unix 纪元起算的微秒数。
fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as u64)
        .unwrap_or(0)
}

fn send_error(session: &u32, out: &mpsc::UnboundedSender<WsMessage>, code: u16, msg: &str) {
    send_message(
        session,
        out,
        &Message::Error(ErrorMsg {
            code,
            msg: msg.to_string(),
            tid: None,
            id: None,
        }),
    );
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
