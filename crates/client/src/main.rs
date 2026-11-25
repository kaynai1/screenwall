#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod direct;
mod errors;
mod files;
mod firewall;
mod group;
mod logging;
mod panels;
mod quality;
mod tray;
mod update;
mod wall;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use screenwall_agent::clipboard;
use screenwall_agent::cloud::{self, CloudOptions};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::display;
use screenwall_agent::server::{AgentState, ApprovalFn, ApprovalRequest};
use screenwall_proto::cloud::{decode_tunnel_frame, encode_tunnel_frame, TunnelKind};
use screenwall_proto::{
    decode_frame, AudioCodec, AudioSource, AudioStats, AudioSub, AudioUnsub, AudioVolume,
    AudioVolumeGet, Capabilities, ClipboardSync, DisplayInfo, FrameCodec, FrameType, Hello,
    InputBatch, InputEvent, Message, PeerInfo, Ping, PowerAction, PowerRequest, SasMethod,
    SecureAttention, StreamKind, VideoCodec, VideoSub, VideoUnsub, DISPLAY_ALL, INPUT_SAS,
    PROTO_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientConfig {
    #[serde(default)]
    server_url: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    device_id: String,
    /// 是否允许别的设备控制本机；关闭后所有请求直接拒绝。
    #[serde(default = "default_true")]
    allow_control: bool,
    /// 界面缩放（0.8–1.5），由 WebView 缩放实现。
    #[serde(default = "default_zoom")]
    ui_zoom: f64,
    /// 远程会话中同步文本剪贴板。
    #[serde(default = "default_true")]
    clipboard_sync: bool,
    /// 关闭窗口时缩到托盘而不是退出（托盘菜单里可退出）。
    #[serde(default = "default_true")]
    close_to_tray: bool,
    /// 是否开启局域网直连（监听本机端口供同网段设备直接接入）。
    #[serde(default)]
    direct_enabled: bool,
    /// 直连监听端口。
    #[serde(default = "default_direct_port")]
    direct_port: u16,
    /// 已配对过的直连设备（我作为主控端连出去时用的凭据）。
    #[serde(default)]
    known_peers: Vec<direct::KnownPeer>,
    /// 允许直连控制本机的长期 token（我作为被控端放行的凭据）。
    ///
    /// 与 `known_peers` 方向相反，不能混用：前者是「我能连谁」，
    /// 后者是「谁能连我」。混用会把别人的机器变成信任自己的机器。
    #[serde(default)]
    direct_tokens: Vec<String>,
    /// 聚焦流的目标码率上限（kbps）；`None` / 0 表示不限制。
    ///
    /// MJPEG 没有 CBR，实现方式是「每帧字节预算」：按这个值和帧率折算
    /// 单帧允许的字节数，编码时选第一个不超预算的质量档。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_bitrate: Option<u32>,
    /// 自定义 CA 根证书文件或目录（PEM/DER）。
    ///
    /// 设置后连接中转服务器只信任这个 CA —— 和 `curl --cacert` 语义一致：
    /// 配置即替代默认信任库。用于自建服务器用私有 CA 签证书的场景。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_path: Option<String>,
}

fn default_direct_port() -> u16 {
    direct::DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

fn default_zoom() -> f64 {
    1.0
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            token: String::new(),
            username: String::new(),
            device_id: String::new(),
            allow_control: true,
            ui_zoom: 1.0,
            clipboard_sync: true,
            close_to_tray: true,
            direct_enabled: false,
            direct_port: direct::DEFAULT_PORT,
            known_peers: Vec::new(),
            direct_tokens: Vec::new(),
            max_bitrate: None,
            ca_path: None,
        }
    }
}

impl ClientConfig {
    fn path() -> PathBuf {
        base_dir().join("client.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

fn base_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ScreenWall")
}

// ---------------------------------------------------------------- 状态

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceView {
    id: String,
    name: String,
    os: String,
    version: String,
    online: bool,
    last_seen: i64,
    controllable: bool,
}

/// 服务器 `GET /api/v1/audits` 返回的一条会话审计记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuditEntry {
    id: i64,
    session_id: u32,
    controller_device_id: String,
    controlled_device_id: String,
    started_at: i64,
    ended_at: Option<i64>,
    duration_ms: i64,
    bytes_up: u64,
    bytes_down: u64,
    /// `relay`（账号中转）或 `direct`（局域网直连）。
    /// 旧服务器不返回这个字段，按中转处理。
    #[serde(default = "default_relay_mode")]
    mode: String,
}

fn default_relay_mode() -> String {
    "relay".to_string()
}

/// 审计分页结果；`total` 为满足过滤条件的总条数（旧服务器不返回时按 0）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuditPageView {
    audits: Vec<AuditEntry>,
    #[serde(default)]
    total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountView {
    server_url: String,
    username: String,
    device_id: String,
    logged_in: bool,
    connected: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlInfo {
    session: u32,
    device_id: String,
    request: u32,
    client_token: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlStateEvent {
    phase: String,
    device_id: String,
    session: u32,
    client_token: u64,
    message: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u16>,
    codec: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlDisplaysEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlFrameEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    stream: u16,
    seq: u32,
    timestamp_us: u64,
    width: u32,
    height: u32,
    fps: u16,
    bytes: usize,
    data_url: String,
}

/// 被控端确认收到电源指令。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlPowerEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    action: PowerAction,
}

/// 被控端对 Ctrl+Alt+Del 的处理结果（`sas.ok`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlSasEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    /// `sas`：已调用 SendSAS；`keys`：降级为普通按键。
    method: SasMethod,
    detail: Option<String>,
}

/// 被控端上报的 UIPI 输入拦截状态（`input.status`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlInputStatusEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    uipi: bool,
    detail: Option<String>,
}

/// 握手完成时推送的双方能力交集（`welcome.caps`）。
///
/// 前端收到后可以直接决定可选功能是否可用；事件本身可能比控制页的订阅更早
/// 到达，所以 `get_control_info` 也会把同一份能力带上。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlCapsEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    /// 能力标识列表，与 `crates/proto/src/caps.rs` 里的常量一一对应。
    caps: Vec<String>,
}

/// 一块系统声音（PCM 或单个 Opus 包，base64）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlAudioEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    stream: u16,
    seq: u32,
    timestamp_us: u64,
    codec: String,
    sample_rate: u32,
    channels: u8,
    data: String,
}

/// `audio.sub.ok` 的格式信息，前端用来显示编码与码率。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlAudioFormatEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    stream: u16,
    codec: String,
    sample_rate: u32,
    channels: u8,
    bitrate: u32,
    /// `loopback`（系统声音）或 `microphone`。
    source: String,
}

/// 被控端回报的当前音量（`audio.volume.ok`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlVolumeEvent {
    device_id: String,
    session: u32,
    client_token: u64,
    level: f32,
    muted: bool,
}

enum ViewOut {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Debug, Clone)]
struct ControlSession {
    session: u32,
    device_id: String,
    client_token: u64,
    video_id: Option<u16>,
    width: u32,
    height: u32,
    fps: u16,
    /// 当前视频订阅的宽度上限与画质，自适应按这两个值做阶梯调整。
    max_width: u32,
    quality: u8,
    /// 当前订阅的显示器；`DISPLAY_ALL` 表示整块虚拟桌面。
    display: u32,
    /// 被控端在会话开始时上报的显示器列表。
    displays: Vec<DisplayInfo>,
    /// 主控端是否想要系统声音（面板开关状态，不是实际订阅状态）。
    audio_enabled: bool,
    /// 实际生效的声音订阅 id；`None` 表示还没收到 `audio.sub.ok`。
    audio_id: Option<u16>,
    /// 声音格式由 `audio.sub.ok` 给出，0 表示未知。
    audio_rate: u32,
    audio_channels: u8,
    /// 实际生效的声音编码与初始码率。
    audio_codec: AudioCodec,
    audio_bitrate: u32,
    /// 当前订阅的声音来源（系统回环 / 麦克风）。
    audio_source: AudioSource,
    /// 握手时 `welcome.caps` 给出的双方能力交集。
    ///
    /// `None` 表示还没收到 `welcome`（能力未知，界面沿用旧的「试了再说」兜底）；
    /// `Some` 之后界面可以直接按能力决定可选功能是否可用。老被控端不带 `caps`
    /// 时按 v1 基线归一，所以这里不会是空集合。
    peer_caps: Option<Capabilities>,
    /// 会话开始时刻（Unix 毫秒），质量统计与「已持续」都用它。
    started_ms: i64,
    /// 是否为可重连会话：`start_control` 发起的中转会话与 `connect_direct`
    /// 建立的直连会话都算（墙/群控的只读会话不算）。
    reconnectable: bool,
    /// 直连会话所属的链路代号（`DirectLink::id`）；中转会话为 `None`。
    ///
    /// 直连的会话号由对方按连接独立分配，不同连接之间会重复，光靠会话号
    /// 分不清「这条控制会话属于哪条 WebSocket」。重连时新旧两条链路可能同时
    /// 存在，收尾必须按这个代号认领，才不会误关掉新会话。
    direct_link: Option<u64>,
}

/// 断线自动重连的进度。只对可重连会话（中转与直连）生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReconnectState {
    /// 当前是第几次尝试（1 起）。
    attempt: u32,
    /// 每次尝试的间隔（毫秒）。
    delay_ms: u64,
}

/// 重连退避表：1s → 2s → 4s → 8s → 8s，最多 5 次。
const RECONNECT_DELAYS_MS: [u64; 5] = [1_000, 2_000, 4_000, 8_000, 8_000];

/// 正在进行的自动重连。用户主动停止/离开页面时整条任务被取消。
struct ReconnectGuard {
    client_token: u64,
    /// 这条重连是否在恢复直连会话。
    ///
    /// 直连不依赖服务器，主控 WebSocket 断线时不能跟着取消它。
    direct: bool,
    state: Option<ReconnectState>,
    /// 取消信号：置位后正在等待的那一轮立即放弃。
    cancel: Arc<AtomicBool>,
    /// 协程句柄（共享槽：注册表被替换时原句柄仍然可安全 abort）。
    task: Option<Arc<StdMutex<Option<tauri::async_runtime::JoinHandle<()>>>>>,
}

/// 画面到达统计：自适应判断「这条链路 + 这台被控机实际能跑多少帧」。
#[derive(Clone)]
struct NetStats {
    window: Instant,
    frames: u64,
    bytes: u64,
}

impl Default for NetStats {
    fn default() -> Self {
        Self {
            window: Instant::now(),
            frames: 0,
            bytes: 0,
        }
    }
}

/// 自适应阶梯：只调 JPEG 画质，不动分辨率。
///
/// 降分辨率会触发 CPU 双线性缩放（实测 1080p→1600 要 30ms 以上），
/// 反而把帧率压得更低；降画质只减少编码字节数，对弱网和编码瓶颈都有效。
const QUALITY_LADDER: [u8; 5] = [75, 65, 55, 48, 42];

#[derive(Default)]
struct Tasks {
    view: Option<tauri::async_runtime::JoinHandle<()>>,
    host: Option<tauri::async_runtime::JoinHandle<()>>,
    adapt: Option<tauri::async_runtime::JoinHandle<()>>,
    clipboard: Option<tauri::async_runtime::JoinHandle<()>>,
    /// 直连监听；独立于 host/view，未登录也要跑。
    direct: Option<tauri::async_runtime::JoinHandle<()>>,
}

struct Shared {
    config: RwLock<ClientConfig>,
    devices: RwLock<Vec<DeviceView>>,
    connected: RwLock<bool>,
    error: RwLock<Option<String>>,
    tasks: StdMutex<Tasks>,
    http: reqwest::Client,
    view_tx: RwLock<Option<mpsc::UnboundedSender<ViewOut>>>,
    control: RwLock<Option<ControlSession>>,
    pending_opens: StdMutex<HashMap<u32, oneshot::Sender<Result<u32, String>>>>,
    /// 异地开机代发的在途会话（最多一条）。
    pending_wake: StdMutex<Option<PendingWake>>,
    next_request: AtomicU32,
    /// 最近 2 秒收到的视频帧数/字节数。
    stats: StdMutex<NetStats>,
    /// 自适应画质开关（默认开）。
    auto_quality: AtomicBool,
    /// 当前处于自适应阶梯的第几级。
    quality_step: AtomicUsize,
    /// 最近一次隧道 Ping 的往返时延，0 表示未知。
    rtt_ms: AtomicU32,
    /// 已发出但还没回 Ping 的时间戳。
    ping_sent: StdMutex<Option<(u64, Instant)>>,
    /// 等待本机用户确认的远程控制请求（被控端授权）。
    pending_control_requests: StdMutex<HashMap<u32, PendingControlRequest>>,
    /// 「允许被控」开关，运行中改立即生效（不用重连）。
    allow_control: AtomicBool,
    /// 文件上传/下载状态。
    files: StdMutex<files::Hub>,
    /// 剪贴板同步开关（运行中立即生效）。
    clipboard_on: AtomicBool,
    /// 屏幕墙缩略图会话。
    wall: StdMutex<wall::Hub>,
    /// 群控广播会话。
    group: StdMutex<group::Hub>,
    /// 群控批次号，保证旧页面的输入不会误发给新批次。
    next_group: AtomicU64,
    /// 当前控制会话的连接质量统计。
    quality: StdMutex<quality::Tracker>,
    /// 断线自动重连的进度与取消信号。
    reconnect: StdMutex<Option<ReconnectGuard>>,
    /// 主控连接（到服务器的 WebSocket）断开时暂存的重连计划，
    /// 等服务器连回来再真正开始重连。
    pending_resume: StdMutex<Option<PendingResume>>,
    /// 界面上是否还有一条「应该活着」的控制会话（控制页/文件页在用）。
    /// 用户主动停止时置 false，自动重连只在 true 时启动。
    control_wanted: AtomicBool,
    /// 局域网直连状态：配对码、监听、当前直连会话。
    direct: StdMutex<direct::DirectHub>,
}

/// 主控连接断开期间暂存的重连计划。
#[derive(Debug, Clone)]
struct PendingResume {
    device_id: String,
    client_token: u64,
    plan: ResumePlan,
}

/// 一条等待本机确认的控制请求。
struct PendingControlRequest {
    viewer: String,
    reply: oneshot::Sender<bool>,
}

/// 异地开机代发：一次性会话只等对端一条 `wol.ok` / `error`。
///
/// 这条会话不进 `shared.control`（画面页/重连逻辑都不该看见它），
/// 回复由 [`resolve_pending_wake`] 按会话号接走。
struct PendingWake {
    session: u32,
    reply: Option<oneshot::Sender<Result<(), String>>>,
}

// ---------------------------------------------------------------- 命令

#[tauri::command]
async fn get_account(shared: State<'_, Arc<Shared>>) -> Result<AccountView, String> {
    Ok(account_view(shared.inner()).await)
}

#[tauri::command]
async fn list_devices(shared: State<'_, Arc<Shared>>) -> Result<Vec<DeviceView>, String> {
    Ok(shared.devices.read().await.clone())
}

#[tauri::command]
async fn login(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    server_url: String,
    username: String,
    password: String,
    register: bool,
) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    let base = normalize_server_url(&server_url)?;
    let username = username.trim().to_string();
    if username.is_empty() {
        return Err("请填写用户名".into());
    }
    if password.is_empty() {
        return Err("请填写密码".into());
    }

    if register {
        post_api(
            &shared.http,
            format!("{base}/api/v1/register"),
            &json!({ "username": username, "password": password }),
            None,
        )
        .await?;
    }

    let existing_device_id = shared.config.read().await.device_id.clone();
    let mut device = json!({
        "name": hostname(),
        "os": "Windows",
        "version": env!("CARGO_PKG_VERSION")
    });
    if !existing_device_id.is_empty() {
        device["id"] = json!(existing_device_id);
    }

    let value = post_api(
        &shared.http,
        format!("{base}/api/v1/login"),
        &json!({ "username": username, "password": password, "device": device }),
        None,
    )
    .await?;

    let token = value["token"]
        .as_str()
        .ok_or_else(|| "服务器没有返回登录令牌".to_string())?
        .to_string();
    let device_id = value["device"]["id"]
        .as_str()
        .unwrap_or(existing_device_id.as_str())
        .to_string();
    if device_id.is_empty() {
        return Err("服务器没有返回设备 ID".into());
    }
    let username = value["user"]["username"]
        .as_str()
        .unwrap_or(username.as_str())
        .to_string();

    {
        let mut config = shared.config.write().await;
        config.server_url = base;
        config.token = token;
        config.username = username;
        config.device_id = device_id;
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }

    shared.devices.write().await.clear();
    *shared.connected.write().await = false;
    *shared.error.write().await = None;

    // 换账号：旧账号的重连任务与挂起的恢复计划都必须清掉，
    // 否则会拿着旧令牌、对着旧设备重试。
    cancel_reconnect(&shared, None);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    stop_tasks(&shared);
    *shared.view_tx.write().await = None;
    *shared.control.write().await = None;
    shared.pending_opens.lock().unwrap().clear();
    shared.files.lock().unwrap().reset();
    wall::clear(&shared);
    group::clear(&shared);
    spawn_tasks(app.clone(), shared.clone());
    emit_devices(&app, &shared).await;
    let view = account_view(&shared).await;
    let _ = app.emit("account", view.clone());
    Ok(view)
}

#[tauri::command]
async fn logout(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if !base.is_empty() && !token.is_empty() {
        let _ = shared
            .http
            .post(format!("{base}/api/v1/logout"))
            .bearer_auth(&token)
            .send()
            .await;
    }

    // 退出登录：任何在途的自动重连都必须停下，不能后台一直重试。
    cancel_reconnect(&shared, None);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    stop_tasks(&shared);
    *shared.view_tx.write().await = None;
    *shared.control.write().await = None;
    shared.pending_opens.lock().unwrap().clear();
    shared.files.lock().unwrap().reset();
    wall::clear(&shared);
    group::clear(&shared);
    {
        let mut config = shared.config.write().await;
        config.token.clear();
        config.username.clear();
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }
    shared.devices.write().await.clear();
    *shared.connected.write().await = false;
    *shared.error.write().await = None;

    emit_devices(&app, &shared).await;
    let view = account_view(&shared).await;
    let _ = app.emit("account", view.clone());
    Ok(view)
}

#[tauri::command]
async fn start_control(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    device_id: String,
    client_token: u64,
) -> Result<ControlInfo, String> {
    start_control_impl(&app, shared.inner(), &device_id, client_token, true).await
}

#[tauri::command]
async fn stop_control(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<(), String> {
    stop_control_impl(&app, shared.inner(), Some(client_token)).await;
    Ok(())
}

#[tauri::command]
async fn set_audio_enabled(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    enabled: bool,
    codec: Option<String>,
    source: Option<String>,
) -> Result<(), String> {
    set_audio_enabled_impl(
        shared.inner(),
        client_token,
        enabled,
        codec.as_deref(),
        source.as_deref(),
    )
    .await
}

/// 前端把播放缓冲深度发给被控端，用于 Opus 码率自适应。
#[tauri::command]
async fn report_audio_stats(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    id: u16,
    buffered_ms: u32,
) -> Result<(), String> {
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token || control.audio_id != Some(id) {
        return Ok(());
    }
    let out = control_out(shared.inner(), control.session)
        .await
        .ok_or_else(|| "主控连接已断开".to_string())?;
    if send_tunnel_json(
        &out,
        control.session,
        &Message::AudioStats(AudioStats { id, buffered_ms }),
    ) {
        Ok(())
    } else {
        Err("主控连接已断开".to_string())
    }
}

/// 读取被控端当前音量；结果通过 `control-volume` 事件回传。
#[tauri::command]
async fn get_remote_volume(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<(), String> {
    get_remote_volume_impl(shared.inner(), client_token).await
}

async fn get_remote_volume_impl(shared: &Arc<Shared>, client_token: u64) -> Result<(), String> {
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    // 握手时已知对端不支持音量：直接拒绝，不必发出去换一个 UNKNOWN_MESSAGE。
    // 能力未知（老被控端 / 还没收到 welcome）时保持原有「试了再说」的行为。
    if let Some(peer) = &control.peer_caps {
        if !Capabilities::current().supports(peer, screenwall_proto::caps::AUDIO_VOLUME) {
            return Err("被控端不支持音量联动".into());
        }
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接已断开".to_string())?;
    if send_tunnel_json(
        &out,
        control.session,
        &Message::AudioVolumeGet(AudioVolumeGet {}),
    ) {
        Ok(())
    } else {
        Err("主控连接已断开".to_string())
    }
}

/// 调整被控端音量 / 静音；`level` 为 0.0–1.0，`None` 表示只改静音。
#[tauri::command]
async fn set_remote_volume(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    level: Option<f32>,
    muted: Option<bool>,
) -> Result<(), String> {
    if let Some(level) = level {
        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
            return Err("音量需在 0.0–1.0 之间".into());
        }
    }
    if level.is_none() && muted.is_none() {
        return Err("需要提供音量或静音状态".into());
    }
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    // 同 get_remote_volume：能力已知就直接拒绝，未知时保留旧兜底。
    if let Some(peer) = &control.peer_caps {
        if !Capabilities::current().supports(peer, screenwall_proto::caps::AUDIO_VOLUME) {
            return Err("被控端不支持音量联动".into());
        }
    }
    let out = control_out(shared.inner(), control.session)
        .await
        .ok_or_else(|| "主控连接已断开".to_string())?;
    if send_tunnel_json(
        &out,
        control.session,
        &Message::AudioVolume(AudioVolume { level, muted }),
    ) {
        Ok(())
    } else {
        Err("主控连接已断开".to_string())
    }
}

#[tauri::command]
async fn send_power(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    action: String,
) -> Result<(), String> {
    let action = parse_power_action(&action)?;
    send_power_impl(shared.inner(), client_token, action).await
}

/// 请求被控端发送 Ctrl+Alt+Del（安全注意序列）。
#[tauri::command]
async fn send_secure_attention(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<(), String> {
    send_secure_attention_impl(shared.inner(), client_token).await
}

/// 由本机网卡广播一次 WOL magic packet。
///
/// 被控端离线时没有会话可用，所以唤醒包只能由和它同网段的主控机发出；
/// 广播地址默认 `255.255.255.255`，跨网段时填目标所在子网的定向广播地址。
#[tauri::command]
async fn wake_on_lan(mac: String, broadcast: Option<String>) -> Result<(), String> {
    let broadcast = broadcast
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "255.255.255.255".to_string());
    let mac = mac.trim().to_string();
    tokio::task::spawn_blocking(move || screenwall_agent::wol::send_magic_packet(&mac, &broadcast))
        .await
        .map_err(|error| format!("唤醒任务异常: {error}"))?
        // 底层是 anyhow 链：只取最后一段人话，别把 `os error 10013` 之类直接抛给界面。
        .map_err(|error| errors::user_message(&error))
}

/// 通过另一台在线设备代发 WOL magic packet（异地开机）。
///
/// 目标机器关机时没有任何会话可用，只能请一台与它同网段的在线设备代发。
/// 这里开一条**一次性控制会话**：代发设备仍需本机用户点「允许」（和其它
/// 远程控制请求一样）；拿到会话后只发 `hello` + `wol.request`，收到
/// `wol.ok` 或报错就立刻关闭 —— 不订阅画面，也不占用控制页和自动重连。
#[tauri::command]
async fn wake_via_device(
    shared: State<'_, Arc<Shared>>,
    helper_device_id: String,
    mac: String,
    broadcast: Option<String>,
) -> Result<(), String> {
    wake_via_device_impl(shared.inner(), &helper_device_id, mac, broadcast).await
}

async fn wake_via_device_impl(
    shared: &Arc<Shared>,
    helper_device_id: &str,
    mac: String,
    broadcast: Option<String>,
) -> Result<(), String> {
    let helper_device_id = helper_device_id.trim();
    if helper_device_id.is_empty() {
        return Err("请选择代发设备".into());
    }
    if is_direct_device(helper_device_id) {
        return Err("代发设备需要先登录账号并保持在线".into());
    }
    let config = shared.config.read().await.clone();
    if config.token.is_empty() {
        return Err("尚未登录".into());
    }
    if !*shared.connected.read().await {
        return Err("服务器尚未连接".into());
    }
    {
        let devices = shared.devices.read().await;
        match devices.iter().find(|device| device.id == helper_device_id) {
            Some(device) if device.online => {}
            Some(_) => return Err("代发设备当前不在线".into()),
            None => return Err("代发设备不存在或不在当前账号下".into()),
        }
    }
    // 参数先在本机校验：对端只会再校一遍，没必要为格式错误去打扰人。
    let mac = screenwall_agent::wol::normalize_mac(&mac)
        .map(|bytes| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        })
        .ok_or_else(|| "MAC 地址格式不正确".to_string())?;
    let broadcast = match broadcast
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(value) => screenwall_agent::wol::normalize_broadcast(&value)
            .ok_or_else(|| format!("广播地址无效: {value}"))?,
        None => "255.255.255.255".to_string(),
    };
    if shared.pending_wake.lock().unwrap().is_some() {
        return Err("上一次开机请求还在处理中".into());
    }

    let request = shared
        .next_request
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    let (tx, rx) = oneshot::channel();
    shared.pending_opens.lock().unwrap().insert(request, tx);

    let Some(out) = shared.view_tx.read().await.clone() else {
        shared.pending_opens.lock().unwrap().remove(&request);
        return Err("主控连接尚未就绪".into());
    };
    // mode 走默认的 control：代发设备必须有人确认，不能悄悄用它发广播。
    // 名称带上标记，好让代发设备那边的人知道这是开机请求，不是有人在偷看画面。
    let open = json!({
        "t": "open",
        "request": request,
        "deviceId": helper_device_id,
        "viewer": format!("{}（开机代发）", hostname()),
        "mode": "control"
    })
    .to_string();
    if out.send(ViewOut::Text(open)).is_err() {
        shared.pending_opens.lock().unwrap().remove(&request);
        return Err("主控连接已断开".into());
    }

    let session =
        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS + 10), rx).await {
            Ok(Ok(Ok(session))) => session,
            Ok(Ok(Err(message))) => return Err(message),
            Ok(Err(_)) => return Err("会话请求已取消".into()),
            Err(_) => {
                shared.pending_opens.lock().unwrap().remove(&request);
                return Err("代发设备未确认（等待超时）".into());
            }
        };

    let (wake_tx, wake_rx) = oneshot::channel();
    *shared.pending_wake.lock().unwrap() = Some(PendingWake {
        session,
        reply: Some(wake_tx),
    });

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: String::new(),
        client: PeerInfo {
            name: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        caps: Capabilities::current(),
    });
    let wol = Message::WolRequest(screenwall_proto::WolRequest {
        mac: mac.clone(),
        broadcast,
    });
    let sent = send_tunnel_json(&out, session, &hello) && send_tunnel_json(&out, session, &wol);
    if !sent {
        clear_pending_wake(shared, session);
        close_relay_session(shared, session).await;
        return Err("主控连接已断开".into());
    }

    // 对端发完包才回 `wol.ok`，给 6 秒足够（真正卡住时也不能一直占着会话）。
    let result = match tokio::time::timeout(Duration::from_secs(6), wake_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("代发设备已断开".into()),
        Err(_) => {
            clear_pending_wake(shared, session);
            Err("代发设备未回应（等待超时）".into())
        }
    };
    close_relay_session(shared, session).await;
    tracing::info!(session, mac = %mac, ok = result.is_ok(), "异地开机代发结束");
    result
}

/// 接走 `wake_via_device_impl` 的等待通道（会话号不匹配时什么也不做）。
fn resolve_pending_wake(shared: &Shared, session: u32, result: Result<(), String>) {
    let mut guard = shared.pending_wake.lock().unwrap();
    if guard
        .as_ref()
        .is_some_and(|pending| pending.session == session)
    {
        if let Some(PendingWake {
            reply: Some(tx), ..
        }) = guard.take()
        {
            let _ = tx.send(result);
        }
    }
}

/// 丢掉在途的开机会话状态，不再关心它的回复。
fn clear_pending_wake(shared: &Shared, session: u32) {
    let mut guard = shared.pending_wake.lock().unwrap();
    if guard
        .as_ref()
        .is_some_and(|pending| pending.session == session)
    {
        *guard = None;
    }
}

/// 让服务器收掉一条中转会话（一次性会话结束时用）。
async fn close_relay_session(shared: &Arc<Shared>, session: u32) {
    if let Some(out) = shared.view_tx.read().await.clone() {
        let text = json!({ "t": "close", "session": session }).to_string();
        let _ = out.send(ViewOut::Text(text));
    }
}

/// 把当前画面存成 JPEG；返回落盘路径。
#[tauri::command]
async fn save_screenshot(data_url: String) -> Result<String, String> {
    save_screenshot_impl(&data_url)
}

#[tauri::command]
async fn rename_device(
    shared: State<'_, Arc<Shared>>,
    device_id: String,
    name: String,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err("设备名需为 1–64 个字符".into());
    }
    device_api(
        shared.inner(),
        &device_id,
        reqwest::Method::PATCH,
        Some(json!({ "name": name })),
    )
    .await
}

/// 主动向服务器拉一次设备列表（平时靠服务端推送）。
#[tauri::command]
async fn refresh_devices(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
) -> Result<Vec<DeviceView>, String> {
    let shared = shared.inner().clone();
    let config = shared.config.read().await.clone();
    if config.token.is_empty() {
        return Err("尚未登录".into());
    }
    let base = normalize_server_url(&config.server_url)?;
    let response = shared
        .http
        .get(format!("{base}/api/v1/devices"))
        .bearer_auth(&config.token)
        .send()
        .await
        .map_err(|error| format!("连接服务器失败: {error}"))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let message = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求失败");
        return Err(format!("{message}（HTTP {status}）"));
    }
    let devices: Vec<DeviceView> =
        serde_json::from_value(value.get("devices").cloned().unwrap_or_else(|| json!([])))
            .map_err(|error| format!("解析设备列表失败: {error}"))?;
    *shared.devices.write().await = devices.clone();
    let _ = app.emit("devices", devices.clone());
    Ok(devices)
}

/// 拉取会话审计记录（仅当前账号），支持分页和按设备过滤。
#[tauri::command]
async fn list_audits(
    shared: State<'_, Arc<Shared>>,
    limit: Option<u32>,
    offset: Option<u32>,
    device_id: Option<String>,
) -> Result<AuditPageView, String> {
    let config = shared.config.read().await.clone();
    if config.token.is_empty() {
        return Err("尚未登录".into());
    }
    let base = normalize_server_url(&config.server_url)?;
    let mut url = reqwest::Url::parse(&format!("{base}/api/v1/audits"))
        .map_err(|error| format!("服务器地址无效: {error}"))?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.unwrap_or(50).clamp(1, 500).to_string())
        .append_pair("offset", &offset.unwrap_or(0).to_string());
    if let Some(device) = device_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        url.query_pairs_mut().append_pair("deviceId", device);
    }
    let response = shared
        .http
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .map_err(|error| format!("连接服务器失败: {error}"))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let message = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求失败");
        return Err(format!("{message}（HTTP {status}）"));
    }
    let audits = value.get("audits").cloned().unwrap_or_else(|| json!([]));
    let total = value.get("total").and_then(Value::as_i64).unwrap_or(0);
    let audits: Vec<AuditEntry> =
        serde_json::from_value(audits).map_err(|error| format!("解析审计记录失败: {error}"))?;
    Ok(AuditPageView { audits, total })
}

#[tauri::command]
async fn set_device_controllable(
    shared: State<'_, Arc<Shared>>,
    device_id: String,
    controllable: bool,
) -> Result<(), String> {
    device_api(
        shared.inner(),
        &device_id,
        reqwest::Method::PATCH,
        Some(json!({ "controllable": controllable })),
    )
    .await
}

#[tauri::command]
async fn delete_device(shared: State<'_, Arc<Shared>>, device_id: String) -> Result<(), String> {
    if shared.config.read().await.device_id == device_id {
        return Err("不能删除本机设备".into());
    }
    device_api(shared.inner(), &device_id, reqwest::Method::DELETE, None).await
}

/// 每次前端调用最多转发 256 条输入事件。
const MAX_INPUT_BATCH: usize = 256;

#[tauri::command]
async fn send_control_input(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    events: Vec<InputEvent>,
) -> Result<(), String> {
    send_control_input_impl(shared.inner(), client_token, events).await
}

#[tauri::command]
async fn set_stream_options(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
    fps: u16,
    max_width: u32,
    quality: u8,
    display: Option<u32>,
) -> Result<(), String> {
    // 用户在画质面板里手动选参数：退出自动档，之后不再被自适应覆盖。
    set_stream_options_impl(
        shared.inner(),
        client_token,
        fps,
        max_width,
        quality,
        display,
        true,
    )
    .await
}

// ---------------------------------------------------------------- 控制会话

async fn start_control_impl(
    app: &AppHandle,
    shared: &Arc<Shared>,
    device_id: &str,
    client_token: u64,
    cancel_pending: bool,
) -> Result<ControlInfo, String> {
    let config = shared.config.read().await.clone();
    if config.token.is_empty() {
        return Err("尚未登录".into());
    }
    if !shared
        .devices
        .read()
        .await
        .iter()
        .any(|device| device.id == device_id)
    {
        return Err("设备不存在或不在当前账号下".into());
    }
    if !*shared.connected.read().await {
        return Err("服务器尚未连接".into());
    }
    // 会话还没建立，只能走服务器通道。
    let out = shared
        .view_tx
        .read()
        .await
        .clone()
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;

    // 同一时间只保留一个远程会话：先关旧会话，再建新会话。
    // 重连任务自己调用时不能走「取消重连」那条路径，否则会把自己掐掉。
    if cancel_pending {
        stop_control_impl(app, shared, None).await;
    } else {
        close_control_session(app, shared, None).await;
    }

    let request = shared
        .next_request
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    let (tx, rx) = oneshot::channel();
    shared.pending_opens.lock().unwrap().insert(request, tx);

    let open = json!({
        "t": "open",
        "request": request,
        "deviceId": device_id,
        "viewer": hostname()
    })
    .to_string();
    if out.send(ViewOut::Text(open)).is_err() {
        shared.pending_opens.lock().unwrap().remove(&request);
        return Err("主控连接已断开".into());
    }

    // 被控端需要人来点「允许」，所以要等得比原来（8 秒）久。
    let session =
        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS + 10), rx).await {
            Ok(Ok(Ok(session))) => session,
            Ok(Ok(Err(message))) => return Err(message),
            Ok(Err(_)) => return Err("会话请求已取消".into()),
            Err(_) => {
                shared.pending_opens.lock().unwrap().remove(&request);
                return Err("被控端未确认（等待超时）".into());
            }
        };

    let Some(out) = control_out(shared, session).await else {
        return Err("主控连接已断开".into());
    };

    // 会话已经从服务器分配好、被控端也同意了：先记下「界面需要这条会话」，
    // 后面任何一步发送失败都要把它清掉，否则会被当成用户主动停止。
    let control = ControlSession {
        session,
        device_id: device_id.to_string(),
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
        audio_codec: AudioCodec::Pcm16,
        audio_bitrate: 0,
        audio_source: AudioSource::Loopback,
        peer_caps: None,
        started_ms: now_ms(),
        reconnectable: true,
        direct_link: None,
    };
    *shared.control.write().await = Some(control.clone());
    shared.control_wanted.store(true, Ordering::Relaxed);
    if cancel_pending {
        // 用户新开一条会话：质量统计从零开始（上一轮的累计值不混进来）。
        *shared.quality.lock().unwrap() = quality::Tracker::new(Instant::now(), control.started_ms);
    }
    // 自动重连不清零：帧/丢包/重连次数按「这次远程会话」累计，由 on_reconnect 只重定序号基准。
    emit_control_state(app, "opening", &control, None);

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: String::new(),
        client: PeerInfo {
            name: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        caps: Capabilities::current(),
    });
    if !send_tunnel_json(&out, session, &hello) {
        abort_pending_control(shared, client_token).await;
        return Err("主控连接已断开".into());
    }

    let subscribe = match default_video_sub() {
        Message::VideoSub(mut sub) => {
            sub.max_bitrate = bitrate_cap(&shared.config.read().await.clone());
            Message::VideoSub(sub)
        }
        other => other,
    };
    if !send_tunnel_json(&out, session, &subscribe) {
        abort_pending_control(shared, client_token).await;
        return Err("主控连接已断开".into());
    }

    Ok(ControlInfo {
        session,
        device_id: device_id.to_string(),
        request,
        client_token,
    })
}

/// 建立会话中途失败：清掉半成品会话，并让服务器也关掉它。
async fn abort_pending_control(shared: &Arc<Shared>, client_token: u64) {
    let control = {
        let mut guard = shared.control.write().await;
        match guard.as_ref() {
            Some(current) if current.client_token == client_token => guard.take(),
            _ => None,
        }
    };
    if let Some(control) = control {
        // 会话已经取走，`control_out` 查不到它是不是直连；这里直接按设备 ID 判断。
        if let Some(out) = route_control_out(
            shared,
            control.session,
            is_direct_device(&control.device_id),
            control.direct_link,
        )
        .await
        {
            let text = json!({ "t": "close", "session": control.session }).to_string();
            let _ = out.send(ViewOut::Text(text));
        }
    }
    shared.control_wanted.store(false, Ordering::Relaxed);
}

/// `start_control` 的默认视频订阅：1080p / 30 FPS / Q65 MJPEG。
fn default_video_sub() -> Message {
    Message::VideoSub(VideoSub {
        stream: StreamKind::Focus,
        display: 0,
        fps: 30,
        max_width: Some(1920),
        quality: Some(65),
        codec: VideoCodec::Mjpeg,
        // 远程操作必须看得到被控端光标；界面暂无开关，默认开启。
        cursor: true,
        max_bitrate: None,
    })
}

/// 把配置里的码率上限规整成协议字段：`None` / 0 / 越界一律当作不限制。
fn bitrate_cap(config: &ClientConfig) -> Option<u32> {
    config.max_bitrate.filter(|kbps| *kbps > 0)
}

async fn stop_control_impl(app: &AppHandle, shared: &Arc<Shared>, only_token: Option<u64>) {
    // 用户主动停止/离开页面：先取消重连，再关会话。
    // 顺序很重要——重连任务可能正在 `start_control_impl` 里，
    // 它建好后会检查取消信号并自己收掉，不会留下一条幽灵会话。
    cancel_reconnect(shared, only_token);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    close_control_session(app, shared, only_token).await;
}

/// 只关闭会话、不动重连状态。重连任务建新会话前用它清理旧会话
/// （旧会话可能已被对端关掉，这时什么都不做）。
async fn close_control_session(app: &AppHandle, shared: &Arc<Shared>, only_token: Option<u64>) {
    let control = {
        let mut guard = shared.control.write().await;
        let matches = guard
            .as_ref()
            .map(|control| only_token.is_none_or(|token| control.client_token == token))
            .unwrap_or(false);
        if matches {
            guard.take()
        } else {
            None
        }
    };
    let Some(control) = control else { return };
    // 会话结束，文件传输一并作废（未下完的临时文件会被删掉）。
    shared.files.lock().unwrap().reset();
    if let Some(out) = route_control_out(
        shared,
        control.session,
        is_direct_device(&control.device_id),
        control.direct_link,
    )
    .await
    {
        let text = json!({ "t": "close", "session": control.session }).to_string();
        let _ = out.send(ViewOut::Text(text));
    }
    // 直连会话关掉后要把通道也撤掉，否则下一条会话会误走旧连接。
    //
    // 按链路代号认领，不能只看会话号：直连的会话号由对方按连接独立分配，
    // 和服务器会话号完全可能撞上同一个值，那样会把一条正常的服务器会话
    // 误判成直连并拆掉通道。
    if let Some(link_id) = control.direct_link {
        let mut hub = shared.direct.lock().unwrap();
        if hub.link.as_ref().map(|link| link.id) == Some(link_id) {
            hub.link = None;
            hub.sessions = 0;
        }
    }
    emit_control_state(app, "closed", &control, Some("会话已结束".into()));
}

// ---------------------------------------------------------------- 自动重连

/// 重连事件：`control-reconnecting` 的负载。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlReconnectingEvent {
    client_token: u64,
    device_id: String,
    attempt: u32,
    max_attempts: u32,
    delay_ms: u64,
}

/// 重连成功/失败事件：`control-reconnected` / `control-reconnect-failed` 的负载。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlReconnectOutcomeEvent {
    client_token: u64,
    device_id: String,
    /// 成功时为恢复后的新会话号；失败时为 0。
    session: u32,
    /// 成功时为第几次尝试成功；失败时为总尝试次数。
    attempt: u32,
    /// 失败原因（成功时为 `None`）。
    message: Option<String>,
}

/// 断开前记下的订阅参数，重连后按原样恢复。
#[derive(Debug, Clone)]
struct ResumePlan {
    /// 最初那条会话的开始时刻（Unix 毫秒）：重连后「已持续」要接着算。
    started_ms: i64,
    fps: u16,
    max_width: u32,
    quality: u8,
    display: u32,
    audio_enabled: bool,
    audio_codec: AudioCodec,
    audio_source: AudioSource,
    clipboard: bool,
}

impl ResumePlan {
    fn from_session(control: &ControlSession, clipboard: bool) -> Self {
        Self {
            started_ms: control.started_ms,
            // 还没收到 VideoSubOk 时按默认参数恢复，避免重连后没有画面。
            fps: if control.fps > 0 { control.fps } else { 30 },
            max_width: if control.max_width > 0 {
                control.max_width
            } else {
                1920
            },
            quality: if control.quality > 0 {
                control.quality
            } else {
                65
            },
            display: control.display,
            audio_enabled: control.audio_enabled,
            audio_codec: control.audio_codec,
            audio_source: control.audio_source,
            clipboard,
        }
    }
}

/// 只取消中转重连（主控 WebSocket 断开时用）。直连重连保留。
fn cancel_relay_reconnect(shared: &Shared) {
    let is_direct = shared
        .reconnect
        .lock()
        .unwrap()
        .as_ref()
        .map(|item| item.direct)
        .unwrap_or(true);
    if is_direct {
        return;
    }
    cancel_reconnect(shared, None);
}

/// 取消当前的重连任务。`only_token` 为 `None` 时无条件取消。
///
/// 同时置位取消信号，正在 `sleep` 等待的那一轮会立刻退出，
/// 不会在用户离开页面后还偷偷建会话。
fn cancel_reconnect(shared: &Shared, only_token: Option<u64>) {
    let mut guard = shared.reconnect.lock().unwrap();
    let matches = guard
        .as_ref()
        .map(|item| only_token.is_none_or(|token| item.client_token == token))
        .unwrap_or(false);
    if !matches {
        return;
    }
    if let Some(item) = guard.as_ref() {
        item.cancel.store(true, Ordering::SeqCst);
    }
    if let Some(item) = guard.as_mut() {
        if let Some(slot) = item.task.take() {
            if let Some(task) = slot.lock().unwrap().take() {
                task.abort();
            }
        }
    }
    *guard = None;
}

/// 当前重连状态（前端「重试中」提示用）。
fn reconnect_state(shared: &Shared, client_token: u64) -> Option<ReconnectState> {
    shared
        .reconnect
        .lock()
        .unwrap()
        .as_ref()
        .filter(|item| item.client_token == client_token)
        .and_then(|item| item.state)
}

fn emit_reconnecting(app: &AppHandle, client_token: u64, device_id: &str, state: ReconnectState) {
    let _ = app.emit(
        "control-reconnecting",
        ControlReconnectingEvent {
            client_token,
            device_id: device_id.to_string(),
            attempt: state.attempt,
            max_attempts: RECONNECT_DELAYS_MS.len() as u32,
            delay_ms: state.delay_ms,
        },
    );
}

/// 会话非用户主动结束时调用：按 1s → 2s → 4s → 8s → 8s 重试，最多 5 次。
///
/// 调用前会话已经从 `shared.control` 里取走（旧会话的隧道关闭消息也已发出），
/// 因此重连过程中不会同时存在两条会话。每一轮都先 `close_control_session`
/// 再建新会话（`start_control_impl(..., cancel_pending = false)`），
/// 不会走「取消重连」那条路径把自己掐掉。
///
/// 直连会话的重连和中转完全不同：不是「向服务器重新发 open」，而是用
/// `known_peers` 里的长期 token 重建整条 TLS + WebSocket（`direct::connect`）。
/// 两条路径共用同一套退避、取消信号与三个事件，界面无需区分。
fn spawn_reconnect(
    app: &AppHandle,
    shared: &Arc<Shared>,
    device_id: &str,
    client_token: u64,
    plan: ResumePlan,
    reason: String,
) {
    cancel_reconnect(shared, None);
    let cancel = Arc::new(AtomicBool::new(false));
    let task_slot: Arc<StdMutex<Option<tauri::async_runtime::JoinHandle<()>>>> =
        Arc::new(StdMutex::new(None));
    let guard = ReconnectGuard {
        client_token,
        direct: direct_address(device_id).is_some(),
        state: None,
        cancel: cancel.clone(),
        task: Some(task_slot.clone()),
    };
    *shared.reconnect.lock().unwrap() = Some(guard);

    let app = app.clone();
    let outer_shared = shared.clone();
    let task_shared = shared.clone();
    let device_id = device_id.to_string();
    let task = tauri::async_runtime::spawn(async move {
        let shared = task_shared;
        tracing::info!(device = %device_id, %reason, "控制会话断开，开始自动重连");
        let mut last_error = reason;
        for (index, delay_ms) in RECONNECT_DELAYS_MS.iter().enumerate() {
            let attempt = index as u32 + 1;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let state = ReconnectState {
                attempt,
                delay_ms: *delay_ms,
            };
            if let Some(item) = shared.reconnect.lock().unwrap().as_mut() {
                if item.client_token == client_token {
                    item.state = Some(state);
                } else {
                    return;
                }
            }
            emit_reconnecting(&app, client_token, &device_id, state);
            // 退避等待期间随时可被取消。
            let deadline = Instant::now() + Duration::from_millis(*delay_ms);
            while Instant::now() < deadline {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if cancel.load(Ordering::SeqCst) {
                return;
            }

            let attempt_result = match direct_address(&device_id) {
                // 直连：整条 TLS + WS 重建，凭据用 `known_peers` 里的长期 token
                // （`pairing_code = None`），证书指纹仍由 `direct::connect` 校验。
                Some((host, port)) => {
                    direct::connect(&app, &shared, host, Some(port), None, client_token).await
                }
                // 中转：向服务器重新发 open。
                None => start_control_impl(&app, &shared, &device_id, client_token, false).await,
            };
            match attempt_result {
                Ok(info) => {
                    // 建立过程中用户可能已经离开：立刻收掉刚建的会话。
                    if cancel.load(Ordering::SeqCst) {
                        stop_control_impl(&app, &shared, Some(client_token)).await;
                        return;
                    }
                    *shared.reconnect.lock().unwrap() = None;
                    {
                        let mut tracker = shared.quality.lock().unwrap();
                        tracker.on_reconnect();
                        tracker.set_since(plan.started_ms);
                    }
                    resume_session(&app, &shared, &info, &plan).await;
                    let _ = app.emit(
                        "control-reconnected",
                        ControlReconnectOutcomeEvent {
                            client_token,
                            device_id: device_id.clone(),
                            session: info.session,
                            attempt,
                            message: None,
                        },
                    );
                    tracing::info!(device = %device_id, attempt, session = info.session, "控制会话已恢复");
                    return;
                }
                Err(error) => {
                    tracing::warn!(device = %device_id, attempt, %error, "重连失败");
                    last_error = error;
                }
            }
        }

        if cancel.load(Ordering::SeqCst) {
            return;
        }
        *shared.reconnect.lock().unwrap() = None;
        let _ = app.emit(
            "control-reconnect-failed",
            ControlReconnectOutcomeEvent {
                client_token,
                device_id: device_id.clone(),
                session: 0,
                attempt: RECONNECT_DELAYS_MS.len() as u32,
                message: Some(last_error),
            },
        );
        tracing::warn!(device = %device_id, "自动重连已放弃");
    });
    // 句柄放在共享槽里：注册表可能已经被取消/替换，这里仍然能安全地收尾。
    *task_slot.lock().unwrap() = Some(task);
    // 注册表在协程启动前就被取消/替换：立刻掐掉，避免留下没人管的协程。
    let alive = outer_shared
        .reconnect
        .lock()
        .unwrap()
        .as_ref()
        .map(|item| item.client_token == client_token)
        .unwrap_or(false);
    if !alive {
        if let Some(task) = task_slot.lock().unwrap().take() {
            task.abort();
        }
    }
}

/// 重连成功后按断开前的参数重新订阅画面/声音，并拉一次音量、打开剪贴板同步。
async fn resume_session(
    app: &AppHandle,
    shared: &Arc<Shared>,
    info: &ControlInfo,
    plan: &ResumePlan,
) {
    // 画面：沿用断开前的档位与显示器。start_control 已经按默认参数订阅过一次，
    // 这里先退订再按原参数订阅，保证和用户看到的一致。
    let _ = set_stream_options_impl(
        shared,
        info.client_token,
        plan.fps,
        plan.max_width,
        plan.quality,
        Some(plan.display),
        false,
    )
    .await;

    // 声音：沿用 codec/source；重连前关着就保持关闭。
    if plan.audio_enabled {
        let codec = match plan.audio_codec {
            AudioCodec::Opus => "opus",
            AudioCodec::Pcm16 => "pcm16",
        };
        let source = match plan.audio_source {
            AudioSource::Loopback => "loopback",
            AudioSource::Microphone => "microphone",
        };
        if let Err(error) =
            set_audio_enabled_impl(shared, info.client_token, true, Some(codec), Some(source)).await
        {
            tracing::warn!(%error, "重连后恢复声音订阅失败");
        }
    }

    // 音量：重新读一次，侧栏滑块回到真实值。
    let _ = get_remote_volume_impl(shared, info.client_token).await;

    // 剪贴板：被控端随会话重建采集状态，这里把本机开关同步过去。
    // 开关本来就没变时 `set_clipboard_sync` 只写内存，不会动配置文件。
    if shared.clipboard_on.load(Ordering::Relaxed) != plan.clipboard {
        shared.clipboard_on.store(plan.clipboard, Ordering::Relaxed);
    }
    let _ = app.emit(
        "control-resumed",
        json!({
            "clientToken": info.client_token,
            "clipboard": plan.clipboard,
        }),
    );
}

/// 主控连接恢复后，把断线前挂起的控制会话重新拉起来。
async fn resume_pending_session(app: &AppHandle, shared: &Arc<Shared>) {
    let Some(pending) = shared.pending_resume.lock().unwrap().take() else {
        return;
    };
    // 已经有一个更新会话或重连在跑：旧的挂起计划作废。
    if shared.control.read().await.is_some() || shared.reconnect.lock().unwrap().is_some() {
        return;
    }
    spawn_reconnect(
        app,
        shared,
        &pending.device_id,
        pending.client_token,
        pending.plan,
        "主控连接已断开".to_string(),
    );
}

/// 一条控制会话的输出通道：直连会话走直连 WebSocket，其余走服务器。
///
/// 直连会话的 `session` 号由对方本地分配，服务器根本不知道它的存在，
/// 所以发错通道就会静默丢失。所有「按当前控制会话发帧」的地方都要经过这里。
///
/// 路由按**当前控制会话**判断，而不是先看直连链路：直连会话号按连接独立
/// 分配，和服务器会话号完全可能撞上同一个值。只要控制会话是直连，就绝不能
/// 回落到服务器通道（未登录时直连也要能用，回落等于把帧发丢）。
pub(crate) async fn control_out(
    shared: &Shared,
    session: u32,
) -> Option<mpsc::UnboundedSender<ViewOut>> {
    let (is_direct, link_id) = {
        let control = shared.control.read().await;
        match control.as_ref() {
            Some(control) if control.session == session => {
                (is_direct_device(&control.device_id), control.direct_link)
            }
            _ => (false, None),
        }
    };
    route_control_out(shared, session, is_direct, link_id).await
}

/// [`control_out`] 的实际路由。会话已经从 `shared.control` 取走时（关闭会话、
/// 建会话失败回滚）调用方自己知道它是不是直连，直接把结论传进来。
async fn route_control_out(
    shared: &Shared,
    session: u32,
    is_direct: bool,
    link_id: Option<u64>,
) -> Option<mpsc::UnboundedSender<ViewOut>> {
    if is_direct {
        let hub = shared.direct.lock().unwrap();
        return hub
            .link
            .as_ref()
            .filter(|link| link.session == session)
            .filter(|link| link_id.is_none_or(|id| id == link.id))
            .map(|link| link.tx.clone());
    }
    shared.view_tx.read().await.clone()
}

/// `device_id` 是否是直连会话（`direct:<地址>:<端口>`）。
pub(crate) fn is_direct_device(device_id: &str) -> bool {
    device_id.starts_with("direct:")
}

/// 控制类命令的前置检查：账号令牌为空时，只有直连会话能继续。
///
/// 直连不需要账号，所以「尚未登录」不能一刀切拒绝 —— 但它仍然必须
/// 有一条活着的控制会话，否则就是无的放矢。
async fn require_session(shared: &Arc<Shared>) -> Result<(), String> {
    if !shared.config.read().await.token.is_empty() {
        return Ok(());
    }
    if shared.direct.lock().unwrap().link.is_some() {
        return Ok(());
    }
    Err("尚未登录".into())
}

fn send_tunnel_json(out: &mpsc::UnboundedSender<ViewOut>, session: u32, message: &Message) -> bool {
    let payload = message.to_json();
    let frame = encode_tunnel_frame(TunnelKind::Json, session, payload.as_bytes());
    out.send(ViewOut::Binary(frame)).is_ok()
}

/// 打开/关闭系统声音。打开后等被控端回 `audio.sub.ok` 才能收到音频帧。
async fn set_audio_enabled_impl(
    shared: &Arc<Shared>,
    client_token: u64,
    enabled: bool,
    codec: Option<&str>,
    source: Option<&str>,
) -> Result<(), String> {
    require_session(shared).await?;
    let wanted = match codec {
        Some("opus") => AudioCodec::Opus,
        Some("pcm16") | Some("pcm") => AudioCodec::Pcm16,
        Some(other) => return Err(format!("不支持的音频编码: {other}")),
        None => AudioCodec::Opus,
    };
    let wanted_source = match source {
        Some("loopback") | Some("system") => AudioSource::Loopback,
        Some("microphone") | Some("mic") => AudioSource::Microphone,
        Some(other) => return Err(format!("不支持的声音来源: {other}")),
        None => AudioSource::Loopback,
    };
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;

    if enabled {
        let unchanged = control.audio_enabled
            && control.audio_codec == wanted
            && control.audio_source == wanted_source;
        if unchanged {
            return Ok(());
        }
        {
            let mut guard = shared.control.write().await;
            match guard.as_mut() {
                Some(current) if current.client_token == client_token => {
                    current.audio_enabled = true;
                    current.audio_codec = wanted;
                    current.audio_source = wanted_source;
                    // 换编码/音源等于换一路新订阅，旧 id 作废。
                    current.audio_id = None;
                    current.audio_rate = 0;
                    current.audio_channels = 0;
                    current.audio_bitrate = 0;
                }
                _ => return Err("控制会话已切换，请重新连接".into()),
            }
        }
        // 被控端收到新订阅会替换旧采集，无需先退订。
        if !send_tunnel_json(
            &out,
            control.session,
            &Message::AudioSub(AudioSub {
                codec: Some(wanted),
                source: Some(wanted_source),
            }),
        ) {
            return Err("主控连接已断开".into());
        }
        Ok(())
    } else {
        let id = {
            let mut guard = shared.control.write().await;
            match guard.as_mut() {
                Some(current) if current.client_token == client_token => {
                    current.audio_enabled = false;
                    current.audio_rate = 0;
                    current.audio_channels = 0;
                    current.audio_bitrate = 0;
                    current.audio_id.take()
                }
                _ => None,
            }
        };
        // 还没收到 audio.sub.ok 时没有 id；等到确认时由处理逻辑补发退订。
        if let Some(id) = id {
            let _ = send_tunnel_json(
                &out,
                control.session,
                &Message::AudioUnsub(AudioUnsub { id }),
            );
        }
        Ok(())
    }
}

fn parse_power_action(raw: &str) -> Result<PowerAction, String> {
    match raw {
        "restart" => Ok(PowerAction::Restart),
        "shutdown" => Ok(PowerAction::Shutdown),
        other => Err(format!("不支持的电源操作: {other}")),
    }
}

async fn send_power_impl(
    shared: &Arc<Shared>,
    client_token: u64,
    action: PowerAction,
) -> Result<(), String> {
    require_session(shared).await?;
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    if !send_tunnel_json(
        &out,
        control.session,
        &Message::Power(PowerRequest { action }),
    ) {
        return Err("主控连接已断开".into());
    }
    Ok(())
}

async fn send_secure_attention_impl(shared: &Arc<Shared>, client_token: u64) -> Result<(), String> {
    require_session(shared).await?;
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    // 直连会话不提供键鼠注入，Ctrl+Alt+Del 同样拒绝。
    if control.direct_link.is_some() {
        return Err("局域网直连不提供键鼠控制".into());
    }
    // 老被控端没有这个能力：直接给出可读错误，别让它回一条 BAD_REQUEST。
    if let Some(caps) = &control.peer_caps {
        if !caps.contains(INPUT_SAS) {
            return Err("被控端版本较旧，不支持 Ctrl+Alt+Del；请升级被控端".into());
        }
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    // as_user = false：以系统身份请求；被控端会按策略决定是否成功。
    let message = Message::SecureAttention(SecureAttention::default());
    if !send_tunnel_json(&out, control.session, &message) {
        return Err("主控连接已断开".into());
    }
    Ok(())
}

async fn send_control_input_impl(
    shared: &Arc<Shared>,
    client_token: u64,
    events: Vec<InputEvent>,
) -> Result<(), String> {
    require_session(shared).await?;
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    if events.is_empty() {
        return Ok(());
    }
    if events.len() > MAX_INPUT_BATCH {
        return Err(format!(
            "单次输入事件过多（{} > {MAX_INPUT_BATCH}）",
            events.len()
        ));
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    let message = Message::Input(InputBatch { events });
    if !send_tunnel_json(&out, control.session, &message) {
        return Err("主控连接已断开".into());
    }
    Ok(())
}

async fn set_stream_options_impl(
    shared: &Arc<Shared>,
    client_token: u64,
    fps: u16,
    max_width: u32,
    quality: u8,
    display: Option<u32>,
    manual: bool,
) -> Result<(), String> {
    if !(1..=60).contains(&fps) {
        return Err(format!("帧率超出范围（{fps}，允许 1-60）"));
    }
    if !(320..=3840).contains(&max_width) {
        return Err(format!("宽度超出范围（{max_width}，允许 320-3840）"));
    }
    if !(30..=90).contains(&quality) {
        return Err(format!("画质超出范围（{quality}，允许 30-90）"));
    }
    require_session(shared).await?;
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    let display = display.unwrap_or(control.display);
    // 被控端列表已到达时先在本地挡住越界索引；列表为空则交给被控端校验。
    if !control.displays.is_empty()
        && display != DISPLAY_ALL
        && !control.displays.iter().any(|item| item.index == display)
    {
        return Err(format!("显示器 {} 不存在", display.saturating_add(1)));
    }
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;

    // 先退订旧视频流，再按新参数重新订阅；整个过程不重开会话。
    if let Some(id) = control.video_id {
        let unsub = Message::VideoUnsub(VideoUnsub { id });
        if !send_tunnel_json(&out, control.session, &unsub) {
            return Err("主控连接已断开".into());
        }
    }
    let subscribe = Message::VideoSub(VideoSub {
        stream: StreamKind::Focus,
        display,
        fps,
        max_width: Some(max_width),
        quality: Some(quality),
        codec: VideoCodec::Mjpeg,
        // 与 default_video_sub 保持一致：重新订阅时不能把光标弄丢。
        cursor: true,
        // 自适应调画质时也要带着码率上限，否则调一档就把上限丢了。
        max_bitrate: bitrate_cap(&shared.config.read().await.clone()),
    });
    if !send_tunnel_json(&out, control.session, &subscribe) {
        return Err("主控连接已断开".into());
    }

    // 清掉旧尺寸，直到新的 VideoSubOk 到达；期间在途的旧帧会被丢弃。
    let mut guard = shared.control.write().await;
    if let Some(current) = guard.as_mut() {
        if current.session == control.session && current.client_token == client_token {
            current.video_id = None;
            current.width = 0;
            current.height = 0;
            current.fps = fps;
            current.max_width = max_width;
            current.quality = quality;
            current.display = display;
        }
    }
    drop(guard);

    if manual {
        // 手动选参数即视为退出自动档：用户的选择优先于自适应。
        shared.auto_quality.store(false, Ordering::Relaxed);
        let step = QUALITY_LADDER
            .iter()
            .position(|candidate| *candidate == quality)
            .unwrap_or(0);
        shared.quality_step.store(step, Ordering::Relaxed);
    }
    Ok(())
}

/// 主控端剪贴板：本机复制的内容发给被控端，被控端的变更写回本机。
async fn clipboard_loop(shared: Arc<Shared>) {
    let mut seq = clipboard::sequence();
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if !shared.clipboard_on.load(Ordering::Relaxed) {
            seq = clipboard::sequence();
            continue;
        }
        let control = shared.control.read().await.clone();
        let Some(control) = control else {
            seq = clipboard::sequence();
            continue;
        };
        // 对端没有声明 `clipboard.image` 时只发文本：老被控端只认文本，
        // 发图片过去它只会当成空文本丢掉，还白占带宽。
        let allow_image = control
            .peer_caps
            .as_ref()
            .is_some_and(|caps| caps.contains(screenwall_proto::caps::CLIPBOARD_IMAGE));
        let message = match clipboard::poll(&mut seq) {
            Some(screenwall_agent::clipboard::Clip::Text(text)) => ClipboardSync::text(text),
            Some(screenwall_agent::clipboard::Clip::Image(png)) if allow_image => {
                use base64::Engine as _;
                ClipboardSync::image(base64::engine::general_purpose::STANDARD.encode(&png))
            }
            // 对端不支持图片：跳过这一轮，等下一次变化。
            Some(screenwall_agent::clipboard::Clip::Image(_)) | None => continue,
        };
        if let Some(out) = control_out(&shared, control.session).await {
            let _ = send_tunnel_json(&out, control.session, &Message::Clipboard(message));
        }
    }
}

/// 自动适应网络：按实测帧率在画质阶梯上滑动，兼顾流畅和清晰。
async fn adapt_loop(app: AppHandle, shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut good_windows = 0u32;
    // 刚调整过参数时跳过一轮，避免 VideoSubOk 到达前的空窗被误判成掉帧。
    let mut cooldown = 0u32;
    // 新会话开始的几个窗口只观察不决策：起流、握手、首帧都在这一段里。
    let mut warmup = 0u32;
    let mut last_session = 0u32;

    loop {
        ticker.tick().await;
        let control = shared.control.read().await.clone();
        let Some(control) = control else {
            good_windows = 0;
            cooldown = 0;
            continue;
        };
        if control.session != last_session {
            last_session = control.session;
            warmup = 2;
            good_windows = 0;
        }

        let (fps, kbps, frames) = {
            let mut stats = shared.stats.lock().unwrap();
            let seconds = stats.window.elapsed().as_secs_f64().max(0.001);
            let fps = stats.frames as f64 / seconds;
            let kbps = stats.bytes as f64 * 8.0 / seconds / 1000.0;
            let frames = stats.frames;
            *stats = NetStats::default();
            (fps, kbps, frames)
        };
        let rtt = shared.rtt_ms.load(Ordering::Relaxed);
        // 质量快照（丢帧/丢包/重连）跟着同一轮 tick 推给前端，前端不必再轮询。
        let quality = shared.quality.lock().unwrap().snapshot(Instant::now(), rtt);
        let reconnecting = reconnect_state(&shared, control.client_token);
        let _ = app.emit(
            "control-stats",
            json!({
                "measuredFps": (fps * 10.0).round() / 10.0,
                "targetFps": control.fps,
                "rttMs": if rtt == 0 { Value::Null } else { json!(rtt) },
                "kbps": kbps.round(),
                "auto": shared.auto_quality.load(Ordering::Relaxed),
                "quality": quality,
                "reconnecting": reconnecting.map(|state| json!({
                    "attempt": state.attempt,
                    "maxAttempts": RECONNECT_DELAYS_MS.len() as u32,
                    "delayMs": state.delay_ms,
                })),
            }),
        );

        if !shared.auto_quality.load(Ordering::Relaxed) || control.fps == 0 {
            good_windows = 0;
            continue;
        }
        if warmup > 0 {
            warmup -= 1;
            continue;
        }
        if cooldown > 0 {
            cooldown -= 1;
            continue;
        }
        // 窗口里几乎没有帧（刚重订阅/断流）时不下判断，那测到的是空窗不是卡顿。
        if frames < 3 {
            continue;
        }

        let target = control.fps as f64;
        let step = shared.quality_step.load(Ordering::Relaxed);
        // 用户手选的画质不在阶梯上（退出自动档后又手动选了「清晰」），
        // 自适应不能拿它当基准继续下滑，等下一次手动选择或重启自适应。
        if QUALITY_LADDER.get(step) != Some(&control.quality) {
            good_windows = 0;
            continue;
        }
        let mut next = step;
        if fps < target * 0.82 {
            // 跑不到目标帧率：降一档画质，减少每帧字节数。
            if step + 1 < QUALITY_LADDER.len() {
                next = step + 1;
            }
            good_windows = 0;
        } else if fps > target * 0.95 {
            // 连续 4 个窗口（约 8 秒）都跑满：尝试恢复一档。
            good_windows += 1;
            if good_windows >= 4 && step > 0 {
                next = step - 1;
                good_windows = 0;
            }
        } else {
            good_windows = 0;
        }

        if next != step {
            let quality = QUALITY_LADDER[next];
            let width = if control.max_width > 0 {
                control.max_width
            } else {
                screenwall_agent::capture::DEFAULT_MAX_WIDTH
            };
            tracing::info!(
                from = step,
                to = next,
                width,
                quality,
                fps,
                "自适应调整画面参数"
            );
            if set_stream_options_impl(
                &shared,
                control.client_token,
                control.fps,
                width,
                quality,
                None,
                false,
            )
            .await
            .is_ok()
            {
                shared.quality_step.store(next, Ordering::Relaxed);
                cooldown = 1;
            }
        }
    }
}

#[tauri::command]
async fn set_auto_quality(shared: State<'_, Arc<Shared>>, enabled: bool) -> Result<bool, String> {
    shared.auto_quality.store(enabled, Ordering::Relaxed);
    Ok(enabled)
}

#[tauri::command]
async fn get_clipboard_sync(shared: State<'_, Arc<Shared>>) -> Result<bool, String> {
    Ok(shared.clipboard_on.load(Ordering::Relaxed))
}

#[tauri::command]
async fn set_clipboard_sync(shared: State<'_, Arc<Shared>>, enabled: bool) -> Result<bool, String> {
    shared.clipboard_on.store(enabled, Ordering::Relaxed);
    let mut config = shared.config.write().await;
    if config.clipboard_sync != enabled {
        config.clipboard_sync = enabled;
        config
            .save()
            .map_err(|error| format!("保存设置失败: {error}"))?;
    }
    Ok(enabled)
}

/// 当前远程会话（文件传输页要用它判断能不能传文件）。
#[tauri::command]
async fn get_control_info(shared: State<'_, Arc<Shared>>) -> Result<Value, String> {
    let control = shared.control.read().await.clone();
    Ok(match control {
        Some(control) => json!({
            "deviceId": control.device_id,
            "session": control.session,
            "clientToken": control.client_token,
            // 握手已完成才带；`null` 表示能力未知，前端沿用旧的兜底逻辑。
            "caps": control
                .peer_caps
                .as_ref()
                .map(|caps| caps.iter().collect::<Vec<_>>()),
        }),
        None => Value::Null,
    })
}

/// 当前会话的连接质量（丢帧/丢包、实时帧率与码率、最大帧间隔、重连与 RTT）。
///
/// 前端每 2 秒轮询一次；页面不可见时前端会停掉轮询。
#[tauri::command]
async fn get_connection_quality(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<quality::Snapshot, String> {
    let shared = shared.inner();
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    if control.client_token != client_token {
        return Err("控制会话已切换，请重新连接".into());
    }
    let rtt = shared.rtt_ms.load(Ordering::Relaxed);
    let snapshot = shared.quality.lock().unwrap().snapshot(Instant::now(), rtt);
    Ok(snapshot)
}

#[tauri::command]
async fn get_auto_quality(shared: State<'_, Arc<Shared>>) -> Result<bool, String> {
    Ok(shared.auto_quality.load(Ordering::Relaxed))
}

/// 本机作为被控端时，用户对「请求控制」弹窗的答复。
#[tauri::command]
fn answer_control_request(
    shared: State<'_, Arc<Shared>>,
    session: u32,
    allow: bool,
) -> Result<(), String> {
    let pending = shared
        .pending_control_requests
        .lock()
        .unwrap()
        .remove(&session);
    match pending {
        Some(pending) => {
            let _ = pending.reply.send(allow);
            Ok(())
        }
        None => Err("该请求已失效".into()),
    }
}

/// 窗口可能在事件到达前才挂载，启动时用它补一次待确认请求。
#[tauri::command]
fn list_control_requests(shared: State<'_, Arc<Shared>>) -> Vec<Value> {
    let pending = shared.pending_control_requests.lock().unwrap();
    let mut list: Vec<Value> = pending
        .iter()
        .map(|(session, request)| {
            json!({
                "session": session,
                "viewer": request.viewer,
                "timeoutSec": APPROVAL_TIMEOUT_SECS,
            })
        })
        .collect();
    list.sort_by_key(|item| item["session"].as_u64().unwrap_or(0));
    list
}

// ---------------------------------------------------------------- 本机设置

/// 本机偏好：被控授权开关 + 界面缩放。读取时一并给默认值。
#[tauri::command]
async fn get_local_settings(shared: State<'_, Arc<Shared>>) -> Result<Value, String> {
    let config = shared.config.read().await;
    Ok(json!({
        "allowControl": config.allow_control,
        "uiZoom": config.ui_zoom,
        "closeToTray": config.close_to_tray,
        "caPath": config.ca_path,
    }))
}

/// 设置自定义 CA 根证书（文件或目录）。传空字符串 / null 表示清除。
///
/// 存之前先真读一遍：路径写错要当场报错，否则用户会以为设置成功、
/// 然后在连接服务器时看到一个莫名其妙的证书校验失败。
#[tauri::command]
async fn set_ca_path(
    shared: State<'_, Arc<Shared>>,
    path: Option<String>,
) -> Result<Option<String>, String> {
    let normalized = path
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(value) = &normalized {
        screenwall_agent::ca::load_roots(std::path::Path::new(value))
            .map_err(|error| format!("读取 CA 证书失败: {error:#}"))?;
    }
    {
        let mut config = shared.config.write().await;
        config.ca_path = normalized.clone();
        config
            .save()
            .map_err(|error| format!("保存设置失败: {error}"))?;
    }
    tracing::info!(ca = ?normalized, "自定义 CA 已更新");
    Ok(normalized)
}

/// 关闭窗口时是缩到托盘还是直接退出。
#[tauri::command]
async fn set_close_to_tray(shared: State<'_, Arc<Shared>>, enabled: bool) -> Result<bool, String> {
    let mut config = shared.config.write().await;
    if config.close_to_tray != enabled {
        config.close_to_tray = enabled;
        config
            .save()
            .map_err(|error| format!("保存设置失败: {error}"))?;
    }
    tracing::info!("关闭窗口缩到托盘: {enabled}");
    Ok(enabled)
}

#[tauri::command]
async fn set_allow_control(shared: State<'_, Arc<Shared>>, enabled: bool) -> Result<bool, String> {
    shared.allow_control.store(enabled, Ordering::Relaxed);
    {
        let mut config = shared.config.write().await;
        if config.allow_control != enabled {
            config.allow_control = enabled;
            config
                .save()
                .map_err(|error| format!("保存设置失败: {error}"))?;
        }
    }
    if !enabled {
        // 关掉开关时，把正在等待确认的请求一并拒绝。
        let mut pending = shared.pending_control_requests.lock().unwrap();
        for (_, request) in pending.drain() {
            let _ = request.reply.send(false);
        }
    }
    tracing::info!("允许被控: {enabled}");
    Ok(enabled)
}

#[tauri::command]
async fn set_ui_zoom(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    zoom: f64,
) -> Result<f64, String> {
    let zoom = zoom.clamp(0.8, 1.5);
    if let Some(window) = app.get_webview_window("main") {
        window
            .set_zoom(zoom)
            .map_err(|error| format!("设置界面缩放失败: {error}"))?;
    }
    {
        let mut config = shared.config.write().await;
        if (config.ui_zoom - zoom).abs() > f64::EPSILON {
            config.ui_zoom = zoom;
            config
                .save()
                .map_err(|error| format!("保存设置失败: {error}"))?;
        }
    }
    Ok(zoom)
}

/// 真去请求 `/healthz`，返回往返毫秒数。
#[tauri::command]
async fn test_server(shared: State<'_, Arc<Shared>>, url: String) -> Result<u64, String> {
    let base = normalize_server_url(&url)?;
    let started = Instant::now();
    let response = shared
        .http
        .get(format!("{base}/healthz"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|error| {
            errors::user_message(&anyhow::Error::new(error).context("连接服务器失败"))
        })?;
    if !response.status().is_success() {
        return Err(format!("服务器健康检查失败（HTTP {}）", response.status()));
    }
    Ok(started.elapsed().as_millis() as u64)
}

/// 是否已写入当前用户的启动项（HKCU\...\Run）。
#[tauri::command]
fn get_autostart() -> bool {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "ScreenWall",
        ])
        .output();
    matches!(output, Ok(output) if output.status.success())
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|error| format!("定位程序失败: {error}"))?;
        let value = format!("\"{}\"", exe.display());
        let status = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "ScreenWall",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()
            .map_err(|error| format!("写入启动项失败: {error}"))?;
        if !status.success() {
            return Err("写入启动项失败（reg add 返回错误）".into());
        }
    } else {
        if !get_autostart() {
            return Ok(false);
        }
        let status = std::process::Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "ScreenWall",
                "/f",
            ])
            .status()
            .map_err(|error| format!("删除启动项失败: {error}"))?;
        if !status.success() {
            return Err("删除启动项失败（reg delete 返回错误）".into());
        }
    }
    Ok(enabled)
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as u64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn emit_control_state(
    app: &AppHandle,
    phase: &str,
    control: &ControlSession,
    message: Option<String>,
) {
    let event = ControlStateEvent {
        phase: phase.to_string(),
        device_id: control.device_id.clone(),
        session: control.session,
        client_token: control.client_token,
        message,
        width: (control.width > 0).then_some(control.width),
        height: (control.height > 0).then_some(control.height),
        fps: (control.fps > 0).then_some(control.fps),
        codec: (control.width > 0).then_some("mjpeg".to_string()),
    };
    let _ = app.emit("control-state", event);
}

/// 会话非用户主动结束时统一走这里：发 `closed` 状态，再决定是否自动重连。
///
/// `reconnectable` 会话会立刻进入退避重连，界面收到 `closed` 后
/// 马上会被 `control-reconnecting` 覆盖成「正在重连」横幅。
async fn end_session(
    app: &AppHandle,
    shared: &Arc<Shared>,
    control: ControlSession,
    message: &str,
) {
    emit_control_state(app, "closed", &control, Some(message.to_string()));
    if control.reconnectable {
        schedule_reconnect(app, shared, control, message).await;
    }
}

// ---------------------------------------------------------------- 连接管理

fn spawn_tasks(app: AppHandle, shared: Arc<Shared>) {
    shared.pending_control_requests.lock().unwrap().clear();
    let host_shared = shared.clone();
    let host_app = app.clone();
    let host = tauri::async_runtime::spawn(async move {
        host_loop(host_app, host_shared).await;
    });
    let view_shared = shared.clone();
    let view_app = app.clone();
    let view = tauri::async_runtime::spawn(async move {
        view_loop(view_app, view_shared).await;
    });
    let adapt_shared = shared.clone();
    let adapt_app = app.clone();
    let adapt = tauri::async_runtime::spawn(async move {
        adapt_loop(adapt_app, adapt_shared).await;
    });
    let clipboard_shared = shared.clone();
    let clipboard_task = tauri::async_runtime::spawn(async move {
        clipboard_loop(clipboard_shared).await;
    });
    // 直连监听独立于上面四条：它不依赖账号，未登录时也要跑。
    let direct_shared = shared.clone();
    let direct_app = app.clone();
    let direct_task = tauri::async_runtime::spawn(async move {
        direct::listen_loop(direct_app, direct_shared).await;
    });
    let mut tasks = shared.tasks.lock().unwrap();
    tasks.host = Some(host);
    tasks.view = Some(view);
    tasks.adapt = Some(adapt);
    tasks.clipboard = Some(clipboard_task);
    tasks.direct = Some(direct_task);
}

/// 看门狗用：先停掉主控/被控任务，再按当前配置重新拉起。
pub(crate) fn restart_tasks(app: AppHandle, shared: Arc<Shared>) {
    if let Some(handle) = shared.tasks.lock().unwrap().view.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().host.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().direct.take() {
        handle.abort();
    }
    spawn_tasks(app, shared);
}

fn stop_tasks(shared: &Shared) {
    shared.pending_control_requests.lock().unwrap().clear();
    if let Some(handle) = shared.tasks.lock().unwrap().view.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().host.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().adapt.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().clipboard.take() {
        handle.abort();
    }
    if let Some(handle) = shared.tasks.lock().unwrap().direct.take() {
        handle.abort();
    }
}

async fn view_loop(app: AppHandle, shared: Arc<Shared>) {
    // 先启动主控连接；host 任务在调用方单独启动。
    let mut backoff = 1u64;
    loop {
        let config = shared.config.read().await.clone();
        if config.token.is_empty() || config.server_url.is_empty() {
            return;
        }

        let started = std::time::Instant::now();
        let result = view_once(&app, &shared, &config).await;
        if started.elapsed() > Duration::from_secs(10) {
            backoff = 1;
        }
        *shared.connected.write().await = false;
        match result {
            Ok(()) => {
                *shared.error.write().await = None;
            }
            Err(error) => {
                // 只保留最后一段人话，别把 anyhow 的上下文链原样塞给界面。
                *shared.error.write().await = Some(errors::user_message(&error));
            }
        }
        let _ = app.emit("account", account_view(&shared).await);

        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// 缩略图看护：断流重开、失败退避。主控连接断开时不动，等重连后恢复。
async fn wall_loop(app: AppHandle, shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if !*shared.connected.read().await {
            continue;
        }
        wall::tick(&app, &shared).await;
        group::tick(&app, &shared, Duration::from_secs(15));
    }
}

async fn view_once(app: &AppHandle, shared: &Arc<Shared>, config: &ClientConfig) -> Result<()> {
    let url = format!("{}/ws/view", to_ws_base(&config.server_url));
    let mut request = url
        .clone()
        .into_client_request()
        .with_context(|| format!("服务器地址无效: {url}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.token)
            .parse()
            .context("令牌格式无效")?,
    );

    // 配置了自定义 CA 就只信任它（同 curl --cacert），否则走系统默认信任库。
    let connector = screenwall_agent::ca::connector_from_path(
        config.ca_path.as_deref().map(std::path::Path::new),
    )
    .context("加载自定义 CA 失败")?;
    let (ws, _) = match connector {
        Some(connector) => {
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
                .await
        }
        None => tokio_tungstenite::connect_async(request).await,
    }
    .context("无法连接服务器")?;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ViewOut>();
    *shared.view_tx.write().await = Some(out_tx);
    *shared.connected.write().await = true;
    *shared.error.write().await = None;
    let _ = app.emit("account", account_view(shared).await);
    // 主控连接（重）建立后立刻恢复屏幕墙预览，不必等下一轮 tick。
    wall::resume(app, shared).await;
    // 服务器断线前还有一个控制会话等着恢复：现在才真正开始重连。
    resume_pending_session(app, shared).await;

    let result: Result<()> = async {
        let (mut sink, mut stream) = ws.split();
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 隧道内 Ping：2 秒一次，用往返时间给「延迟」和自适应提供真实样本。
        let mut tunnel_ping = tokio::time::interval(Duration::from_secs(2));
        tunnel_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ping.tick() => {
                    sink.send(WsMessage::Ping(Vec::new().into()))
                        .await
                        .context("发送心跳失败")?;
                }
                _ = tunnel_ping.tick() => {
                    if let Some(control) = shared.control.read().await.clone() {
                        if let Some(out) = control_out(shared, control.session).await {
                            let ts = now_us();
                            *shared.ping_sent.lock().unwrap() = Some((ts, Instant::now()));
                            let _ = send_tunnel_json(
                                &out,
                                control.session,
                                &Message::Ping(Ping { ts }),
                            );
                        }
                    }
                }
                out = out_rx.recv() => {
                    let Some(out) = out else { return Ok(()) };
                    match out {
                        ViewOut::Text(text) => {
                            sink.send(WsMessage::Text(text.into())).await?;
                        }
                        ViewOut::Binary(data) => {
                            sink.send(WsMessage::Binary(data.into())).await?;
                        }
                    }
                }
                item = stream.next() => {
                    let Some(item) = item else { return Ok(()) };
                    match item? {
                        WsMessage::Text(text) => {
                            handle_view_text(app, shared, text.as_str()).await;
                        }
                        WsMessage::Binary(data) => {
                            handle_view_binary(app, shared, data.as_ref()).await;
                        }
                        WsMessage::Ping(payload) => {
                            sink.send(WsMessage::Pong(payload)).await?;
                        }
                        WsMessage::Close(_) => return Ok(()),
                        _ => {}
                    }
                }
            }
        }
    }
    .await;

    cleanup_view(app, shared).await;
    result
}

async fn handle_view_text(app: &AppHandle, shared: &Arc<Shared>, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match value.get("t").and_then(Value::as_str).unwrap_or_default() {
        "ready" => {
            *shared.connected.write().await = true;
            let _ = app.emit("account", account_view(shared).await);
        }
        "devices" => {
            if let Some(list) = value.get("devices") {
                if let Ok(devices) = serde_json::from_value::<Vec<DeviceView>>(list.clone()) {
                    *shared.devices.write().await = devices;
                    emit_devices(app, shared).await;
                }
            }
        }
        "open.ok" => {
            let request = value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
            let session = value.get("session").and_then(Value::as_u64).unwrap_or(0) as u32;
            if request != 0 && session != 0 {
                resolve_pending_open(shared, request, Ok(session));
            }
        }
        "open.err" => {
            let request = value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
            let code = value.get("code").and_then(Value::as_u64).unwrap_or(0) as u16;
            let message = value
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("被控端拒绝了会话")
                .to_string();
            if request != 0 {
                // 带上服务器错误码，前端 errors.ts 按码给出统一中文文案。
                resolve_pending_open(shared, request, Err(format!("{code}: {message}")));
            }
        }
        "closed" => {
            let session = value.get("session").and_then(Value::as_u64).unwrap_or(0) as u32;
            // 服务器会带上关闭原因（4201 = 对端关闭/设备掉线），日志里留一份。
            if session != 0 {
                let code = value.get("code").and_then(Value::as_u64).unwrap_or(0);
                let reason = value
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("会话已关闭");
                tracing::info!(session, code, reason, "会话被服务器关闭");
            }
            // 一次性开机会话被服务器收掉（代发设备掉线等）：立刻告诉等在那边的调用。
            resolve_pending_wake(shared, session, Err("代发设备已断开".into()));
            // 缩略图会话结束：保留「想看哪些设备」，由 tick 退避重开。
            wall::on_closed(app, shared, session);
            group::on_closed(app, shared, session);
            // 这条 `closed` 来自服务器连接，只可能关掉中转会话：直连会话号
            // 由对方按连接独立分配，和服务器会话号会撞车，不能按号误收。
            let control = {
                let mut guard = shared.control.write().await;
                match guard.as_ref() {
                    Some(current)
                        if current.session == session && !is_direct_device(&current.device_id) =>
                    {
                        guard.take()
                    }
                    _ => None,
                }
            };
            if let Some(control) = control {
                // 被控端主动结束不算用户主动关闭：可重连会话按退避自动恢复。
                end_session(app, shared, control, "被控端已结束会话").await;
            }
        }
        _ => {}
    }
}

/// 会话从 `shared.control` 移除后调用：决定是否自动重连。
///
/// 服务器主动踢线（设备重连、令牌吊销、被控端断开）、直连链路中断和心跳超时
/// 都会走到这里；用户在界面上点「停止」或离开页面时，前端会先调 `stop_control`，
/// 那条路径会先取消重连并置 `control_wanted = false`，所以这里不会误触发。
async fn schedule_reconnect(
    app: &AppHandle,
    shared: &Arc<Shared>,
    control: ControlSession,
    reason: &str,
) {
    // 只有界面还在用这条会话时才自动重连：用户已经点了「停止」/离开页面，
    // 后端会话会先被移除，这里再建一条就变成后台幽灵会话。
    if !reconnect_allowed(&control, shared.control_wanted.load(Ordering::Relaxed)) {
        return;
    }
    let plan = ResumePlan::from_session(&control, shared.clipboard_on.load(Ordering::Relaxed));
    // 直连不依赖服务器：主控 WebSocket 断没断都不影响重连，直接重建链路。
    // 判断依据是设备 ID，不能看 `connected` —— 直连可以在未登录状态下使用。
    if is_direct_device(&control.device_id) {
        spawn_reconnect(
            app,
            shared,
            &control.device_id,
            control.client_token,
            plan,
            reason.to_string(),
        );
        return;
    }
    // 主控 WebSocket 本身断了：先等 `view_loop` 把服务器连回来，
    // 否则 5 次重连都会以「服务器尚未连接」失败，白等 23 秒。
    if !*shared.connected.read().await {
        *shared.pending_resume.lock().unwrap() = Some(PendingResume {
            device_id: control.device_id,
            client_token: control.client_token,
            plan,
        });
        return;
    }
    spawn_reconnect(
        app,
        shared,
        &control.device_id,
        control.client_token,
        plan,
        reason.to_string(),
    );
}

/// 把直连设备的合成 ID（`direct:<地址>:<端口>`）拆回 `(host, port)`。
///
/// 重连时用不到配对码，地址就是重连要连回去的目标。解析失败返回 `None`，
/// 调用方会按中转路径处理（随后以「设备不存在」明确失败）。
pub(crate) fn direct_address(device_id: &str) -> Option<(&str, u16)> {
    let rest = device_id.strip_prefix("direct:")?;
    let (host, port) = rest.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// 是否该为这条会话自动重连。
///
/// 两个条件缺一不可：会话本身可重连，且界面还想要它。用户点「停止」或离开
/// 页面时 `control_wanted` 会被置 false，重连任务随后被取消，不会再连回来。
fn reconnect_allowed(control: &ControlSession, wanted: bool) -> bool {
    control.reconnectable && wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_session(reconnectable: bool) -> ControlSession {
        ControlSession {
            session: 1,
            device_id: "direct:192.168.1.20:47900".into(),
            client_token: 9,
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
            audio_codec: AudioCodec::Pcm16,
            audio_bitrate: 0,
            audio_source: AudioSource::Loopback,
            peer_caps: None,
            started_ms: 0,
            reconnectable,
            direct_link: Some(3),
        }
    }

    /// 直连 ID 必须能被拆回地址，否则重连会退化成「设备不存在」。
    #[test]
    fn direct_address_parses_direct_device_id() {
        assert_eq!(
            direct_address("direct:192.168.1.20:47900"),
            Some(("192.168.1.20", 47900))
        );
        // 普通账号设备 ID 不是直连。
        assert_eq!(direct_address("7f3a9c1e-device"), None);
        assert_eq!(direct_address("direct:192.168.1.20"), None);
        assert_eq!(direct_address("direct::47900"), None);
        assert_eq!(direct_address("direct:host:notaport"), None);
    }

    /// 直连会话与中转会话的判定互斥：只有 `direct:` 前缀算直连。
    #[test]
    fn only_direct_prefix_counts_as_direct() {
        assert!(is_direct_device("direct:10.0.0.2:47900"));
        assert!(!is_direct_device("10.0.0.2:47900"));
        assert!(!is_direct_device("direct-standalone"));
    }

    /// 用户主动断开后（`control_wanted = false`）直连会话不能再自动重连。
    #[test]
    fn direct_reconnect_requires_wanted_flag() {
        let control = direct_session(true);
        assert!(reconnect_allowed(&control, true));
        assert!(!reconnect_allowed(&control, false));
        // 只读会话（墙/群控）无论界面想不想要都不重连。
        assert!(!reconnect_allowed(&direct_session(false), true));
    }
}

async fn handle_view_binary(app: &AppHandle, shared: &Arc<Shared>, data: &[u8]) {
    let Ok((header, payload)) = decode_tunnel_frame(data) else {
        return;
    };
    // 屏幕墙缩略图会话和控制会话各自独立，先按会话号分流。
    if wall::on_binary(app, shared, header.session, header.kind, payload) {
        return;
    }
    if group::on_binary(app, shared, header.session, header.kind, payload) {
        return;
    }
    // 异地开机代发会话不在 `shared.control` 里，按会话号单独接它的回复。
    let wake_session = shared
        .pending_wake
        .lock()
        .unwrap()
        .as_ref()
        .map(|pending| pending.session);
    if wake_session == Some(header.session) {
        if header.kind == TunnelKind::Json {
            let parsed = std::str::from_utf8(payload)
                .ok()
                .and_then(|text| Message::parse(text).ok());
            match parsed {
                Some(Message::WolOk(ok)) => {
                    tracing::info!(session = header.session, mac = %ok.mac, "代发设备已发送唤醒包");
                    resolve_pending_wake(shared, header.session, Ok(()));
                }
                Some(Message::Error(error)) => resolve_pending_wake(
                    shared,
                    header.session,
                    Err(format!("{}: {}", error.code, error.msg)),
                ),
                _ => {}
            }
        }
        // 这条会话只有开机回复，别掉进下面的控制会话分支。
        return;
    }
    let Some(control) = shared.control.read().await.clone() else {
        return;
    };
    if header.session != control.session {
        return;
    }

    match header.kind {
        TunnelKind::Json => {
            let Ok(text) = std::str::from_utf8(payload) else {
                return;
            };
            let Ok(message) = Message::parse(text) else {
                return;
            };
            if files::on_message(app, shared, message.clone()).await {
                return;
            }
            match message {
                Message::VideoSubOk(ok) => {
                    let updated = {
                        let mut guard = shared.control.write().await;
                        if let Some(current) = guard.as_mut() {
                            if current.session == header.session {
                                current.video_id = Some(ok.id);
                                current.width = ok.actual.w;
                                current.height = ok.actual.h;
                                current.fps = ok.actual.fps;
                                Some(current.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(current) = updated {
                        tracing::info!(
                            session = header.session,
                            stream = ok.id,
                            w = ok.actual.w,
                            h = ok.actual.h,
                            fps = ok.actual.fps,
                            "收到画面订阅确认"
                        );
                        emit_control_state(app, "streaming", &current, None);
                    }
                }
                Message::AudioSubOk(ok) => {
                    // 先记下订阅 id/格式；若用户已在确认到达前关掉声音，补发退订。
                    let (updated, enabled) = {
                        let mut guard = shared.control.write().await;
                        match guard.as_mut() {
                            Some(current)
                                if current.session == header.session
                                    && current.client_token == control.client_token =>
                            {
                                current.audio_id = Some(ok.id);
                                current.audio_rate = ok.actual.sample_rate;
                                current.audio_channels = ok.actual.channels;
                                current.audio_codec = ok.actual.codec;
                                current.audio_bitrate = ok.actual.bitrate;
                                current.audio_source = ok.actual.source;
                                (Some(current.clone()), current.audio_enabled)
                            }
                            _ => (None, false),
                        }
                    };
                    if updated.is_some() {
                        if enabled {
                            let codec = match ok.actual.codec {
                                AudioCodec::Opus => "opus",
                                AudioCodec::Pcm16 => "pcm16",
                            };
                            let source = match ok.actual.source {
                                AudioSource::Loopback => "loopback",
                                AudioSource::Microphone => "microphone",
                            };
                            let _ = app.emit(
                                "control-audio-format",
                                ControlAudioFormatEvent {
                                    device_id: control.device_id.clone(),
                                    session: header.session,
                                    client_token: control.client_token,
                                    stream: ok.id,
                                    codec: codec.to_string(),
                                    sample_rate: ok.actual.sample_rate,
                                    channels: ok.actual.channels,
                                    bitrate: ok.actual.bitrate,
                                    source: source.to_string(),
                                },
                            );
                            tracing::info!(
                                session = header.session,
                                stream = ok.id,
                                codec,
                                rate = ok.actual.sample_rate,
                                channels = ok.actual.channels,
                                bitrate = ok.actual.bitrate,
                                "声音订阅已就绪"
                            );
                        } else if let Some(out) = control_out(shared, header.session).await {
                            let _ = send_tunnel_json(
                                &out,
                                header.session,
                                &Message::AudioUnsub(AudioUnsub { id: ok.id }),
                            );
                            tracing::info!(session = header.session, "声音已关闭，补发退订");
                        }
                    }
                }
                Message::AudioVolumeOk(ok) => {
                    let _ = app.emit(
                        "control-volume",
                        ControlVolumeEvent {
                            device_id: control.device_id.clone(),
                            session: header.session,
                            client_token: control.client_token,
                            level: ok.level,
                            muted: ok.muted,
                        },
                    );
                }
                Message::PowerOk(ok) => {
                    tracing::warn!(
                        session = header.session,
                        action = ?ok.action,
                        "被控端已接受电源操作"
                    );
                    let _ = app.emit(
                        "control-power-ok",
                        ControlPowerEvent {
                            device_id: control.device_id.clone(),
                            session: header.session,
                            client_token: control.client_token,
                            action: ok.action,
                        },
                    );
                }
                Message::SecureAttentionOk(ok) => {
                    match ok.method {
                        SasMethod::Sas => tracing::info!(
                            session = header.session,
                            "被控端已调用 SendSAS 发送 Ctrl+Alt+Del"
                        ),
                        SasMethod::Keys => tracing::warn!(
                            session = header.session,
                            detail = ok.detail.as_deref().unwrap_or(""),
                            "被控端已降级为普通 Ctrl+Alt+Del 按键"
                        ),
                    }
                    let _ = app.emit(
                        "control-sas",
                        ControlSasEvent {
                            device_id: control.device_id.clone(),
                            session: header.session,
                            client_token: control.client_token,
                            method: ok.method,
                            detail: ok.detail,
                        },
                    );
                }
                Message::InputStatus(status) => {
                    // 只在状态变化时由被控端上报；前端据此弹提示并显示常驻警告。
                    if status.uipi {
                        tracing::warn!(
                            session = header.session,
                            detail = status.detail.as_deref().unwrap_or(""),
                            "被控端输入被 UIPI 拦截"
                        );
                    } else {
                        tracing::info!(session = header.session, "被控端输入拦截已解除");
                    }
                    let _ = app.emit(
                        "control-input-status",
                        ControlInputStatusEvent {
                            device_id: control.device_id.clone(),
                            session: header.session,
                            client_token: control.client_token,
                            uipi: status.uipi,
                            detail: status.detail,
                        },
                    );
                }
                Message::Error(error) => {
                    // 事件里带上错误码，前端 errors.ts 按码给统一中文文案。
                    let text = format!("{}: {}", error.code, error.msg);
                    if error.code == screenwall_proto::codes::POWER_FAILED {
                        tracing::warn!(msg = %error.msg, "电源操作失败");
                        let _ = app.emit("control-power-error", text);
                    } else if error.code == screenwall_proto::codes::AUDIO_UNAVAILABLE {
                        // 没有声音设备是局部失败，别把整页画面判成出错。
                        tracing::warn!(msg = %error.msg, "系统声音不可用");
                        {
                            let mut guard = shared.control.write().await;
                            if let Some(current) = guard.as_mut() {
                                if current.session == header.session {
                                    current.audio_enabled = false;
                                }
                            }
                        }
                        let _ = app.emit("control-audio-error", text);
                    } else if error.code == screenwall_proto::codes::UNKNOWN_MESSAGE {
                        // 旧被控端不认识音量消息：只提示一下，别把整页画面判成出错。
                        tracing::debug!(msg = %error.msg, "被控端不支持该消息");
                        let _ = app.emit("control-volume-error", text);
                    } else {
                        tracing::warn!(code = error.code, msg = %error.msg, "被控端返回错误");
                        emit_control_state(app, "error", &control, Some(text));
                    }
                }
                Message::Welcome(welcome) => {
                    // 记录双方交集：界面据此直接判断可选功能（如被控端音量），
                    // 不必先发一条消息等 `UNKNOWN_MESSAGE` 回来才知道不支持。
                    let caps = Capabilities::with_peer(welcome.caps.clone());
                    let updated = {
                        let mut guard = shared.control.write().await;
                        match guard.as_mut() {
                            Some(current)
                                if current.session == header.session
                                    && current.client_token == control.client_token =>
                            {
                                current.peer_caps = Some(caps.clone());
                                Some(current.clone())
                            }
                            _ => None,
                        }
                    };
                    tracing::info!(
                        agent = %welcome.agent.name,
                        session = header.session,
                        caps = %caps,
                        "远程会话已握手"
                    );
                    if let Some(current) = updated {
                        let _ = app.emit(
                            "control-caps",
                            ControlCapsEvent {
                                device_id: current.device_id,
                                session: current.session,
                                client_token: current.client_token,
                                caps: caps.iter().map(str::to_string).collect(),
                            },
                        );
                    }
                }
                Message::DisplayList(list) => {
                    let updated = {
                        let mut guard = shared.control.write().await;
                        if let Some(current) = guard.as_mut() {
                            if current.session == header.session
                                && current.client_token == control.client_token
                            {
                                current.displays = list.displays.clone();
                                Some(current.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(current) = updated {
                        tracing::info!(
                            session = header.session,
                            count = list.displays.len(),
                            "收到被控端显示器列表"
                        );
                        let _ = app.emit(
                            "control-displays",
                            ControlDisplaysEvent {
                                device_id: current.device_id,
                                session: current.session,
                                client_token: current.client_token,
                                displays: list.displays,
                            },
                        );
                    }
                }
                Message::Clipboard(sync) => {
                    if shared.clipboard_on.load(Ordering::Relaxed) {
                        screenwall_agent::cloud::apply_clipboard(&sync);
                    }
                }
                Message::Pong(pong) => {
                    let sent = shared.ping_sent.lock().unwrap().take();
                    if let Some((ts, at)) = sent {
                        if ts == pong.ts {
                            let rtt = at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                            shared.rtt_ms.store(rtt.max(1), Ordering::Relaxed);
                        }
                    }
                }
                _ => {}
            }
        }
        TunnelKind::Binary => {
            let Ok((frame, body)) = decode_frame(payload) else {
                return;
            };
            if frame.kind == FrameType::Audio {
                let codec = match frame.codec {
                    FrameCodec::Pcm16 => "pcm16",
                    FrameCodec::Opus => "opus",
                    _ => return,
                };
                if control.audio_rate == 0 || control.audio_channels == 0 || body.len() > 256 * 1024
                {
                    return;
                }
                // 音频包序号与解码时间戳（`ts_us`）同样来自帧头。
                shared
                    .quality
                    .lock()
                    .unwrap()
                    .on_audio(frame.seq, frame.ts_us);
                let encoded = base64::engine::general_purpose::STANDARD.encode(body);
                let event = ControlAudioEvent {
                    device_id: control.device_id.clone(),
                    session: header.session,
                    client_token: control.client_token,
                    stream: frame.stream_id,
                    seq: frame.seq,
                    timestamp_us: frame.ts_us,
                    codec: codec.to_string(),
                    sample_rate: control.audio_rate,
                    channels: control.audio_channels,
                    data: encoded,
                };
                let _ = app.emit("control-audio", event);
                return;
            }
            let jpeg = body;
            if frame.kind != FrameType::Video || frame.codec != FrameCodec::Jpeg {
                return;
            }
            if control.width == 0 || control.height == 0 {
                return;
            }
            // 单帧过大通常是异常数据，直接丢弃，避免把 WebView 拖死。
            if jpeg.len() > 2 * 1024 * 1024 {
                return;
            }
            {
                let mut stats = shared.stats.lock().unwrap();
                stats.frames += 1;
                stats.bytes += jpeg.len() as u64;
            }
            // 真实帧头（stream/seq/到达时刻）喂给质量统计器。
            shared.quality.lock().unwrap().on_video(
                frame.stream_id,
                frame.seq,
                jpeg.len() as u32,
                Instant::now(),
            );
            let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
            let event = ControlFrameEvent {
                device_id: control.device_id.clone(),
                session: header.session,
                client_token: control.client_token,
                stream: frame.stream_id,
                seq: frame.seq,
                timestamp_us: frame.ts_us,
                width: control.width,
                height: control.height,
                fps: control.fps,
                bytes: jpeg.len(),
                data_url: format!("data:image/jpeg;base64,{encoded}"),
            };
            let _ = app.emit("control-frame", event);
        }
    }
}

fn resolve_pending_open(shared: &Shared, request: u32, result: Result<u32, String>) {
    if let Some(tx) = shared.pending_opens.lock().unwrap().remove(&request) {
        let _ = tx.send(result);
    }
}

async fn cleanup_view(app: &AppHandle, shared: &Arc<Shared>) {
    *shared.view_tx.write().await = None;
    // 到服务器的 WebSocket 已经断了：先标记未连接，避免 `schedule_reconnect`
    // 在服务器离线期间空转重试（计划会先挂起，等连回来再跑）。
    *shared.connected.write().await = false;

    let pending = {
        let mut pending = shared.pending_opens.lock().unwrap();
        std::mem::take(&mut *pending)
    };
    for (_, tx) in pending {
        let _ = tx.send(Err("主控连接已断开".into()));
    }
    // 在途的开机代发一次性会话随主控连接一起作废。
    let wake = shared.pending_wake.lock().unwrap().take();
    if let Some(PendingWake {
        reply: Some(tx), ..
    }) = wake
    {
        let _ = tx.send(Err("主控连接已断开".into()));
    }

    // 主控连接断了：在途的中转重连先停掉（它现在只会失败），
    // 会话的恢复计划交给 `schedule_reconnect` 暂存，等服务器连回来再跑。
    // 直连重连不受影响：它走的是局域网，跟服务器连接无关。
    cancel_relay_reconnect(shared);
    // 缩略图会话随视图连接一起失效，重连后由 wall_loop 重新打开。
    wall::reset_sessions(shared);
    // 群控会话无法自动恢复（需要被控端重新授权），交给界面重新开始。
    group::reset_sessions(app, shared);

    // 直连会话不经过服务器：服务器连接断了不该把它一起收掉，
    // 它的链路自己会走 `direct::connect` 里的收尾逻辑。
    let control = {
        let mut guard = shared.control.write().await;
        match guard.as_ref() {
            Some(current) if is_direct_device(&current.device_id) => None,
            _ => guard.take(),
        }
    };
    if let Some(control) = control {
        end_session(app, shared, control, "主控连接已断开").await;
    }
}

/// 被控端授权请求在 Agent 侧的等待上限，和 UI 倒计时保持一致。
const APPROVAL_TIMEOUT_SECS: u64 = 30;

async fn host_loop(app: AppHandle, shared: Arc<Shared>) {
    loop {
        let config = shared.config.read().await.clone();
        if config.token.is_empty() || config.server_url.is_empty() {
            return;
        }

        let agent_config = AgentConfig {
            name: hostname(),
            port: 47900,
            token: String::new(),
            allow_control: true,
            clipboard_sync: shared.clipboard_on.load(Ordering::Relaxed),
            server_url: config.server_url.clone(),
            account_token: config.token.clone(),
            device_id: config.device_id.clone(),
            ca_path: config.ca_path.clone().unwrap_or_default(),
        };
        let approval: ApprovalFn = {
            let app = app.clone();
            let shared = shared.clone();
            Arc::new(move |request: ApprovalRequest| {
                let ApprovalRequest {
                    session,
                    viewer,
                    reply,
                } = request;
                let app = app.clone();
                let shared = shared.clone();
                // 「允许被控」关闭时静默拒绝，不打扰本机用户。
                if !shared.allow_control.load(Ordering::Relaxed) {
                    tracing::info!("已关闭允许被控，拒绝 {viewer} 的请求（会话 {session}）");
                    let _ = reply.send(false);
                    return;
                }
                shared.pending_control_requests.lock().unwrap().insert(
                    session,
                    PendingControlRequest {
                        viewer: viewer.clone(),
                        reply,
                    },
                );
                // 被控请求必须让人看见：把窗口拉回前台。
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
                let _ = app.emit(
                    "control-request",
                    json!({
                        "session": session,
                        "viewer": viewer,
                        "timeoutSec": APPROVAL_TIMEOUT_SECS,
                    }),
                );
                // Agent 30 秒后自动拒绝；这里晚一点清掉残留的答复通道。
                let cleanup = shared.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(APPROVAL_TIMEOUT_SECS + 5)).await;
                    cleanup
                        .pending_control_requests
                        .lock()
                        .unwrap()
                        .remove(&session);
                });
            })
        };
        let state = Arc::new(AgentState {
            config: agent_config,
            displays: display::list_displays(),
            approval: Some(approval),
        });
        let Some(options) = CloudOptions::from_state(&state) else {
            return;
        };
        if let Err(error) = cloud::run(state, options).await {
            tracing::warn!("被控端连接中断: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ---------------------------------------------------------------- 工具

async fn post_api(
    client: &reqwest::Client,
    url: String,
    body: &Value,
    token: Option<&str>,
) -> Result<Value, String> {
    let mut request = client.post(url).json(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("连接服务器失败: {error}"))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if status.is_success() {
        Ok(value)
    } else {
        let message = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求失败");
        Err(format!("{message}（HTTP {status}）"))
    }
}

/// 对当前账号下的设备执行管理操作（改名、允许控制、删除）。
async fn device_api(
    shared: &Arc<Shared>,
    device_id: &str,
    method: reqwest::Method,
    body: Option<Value>,
) -> Result<(), String> {
    let config = shared.config.read().await.clone();
    if config.token.is_empty() {
        return Err("尚未登录".into());
    }
    if device_id.trim().is_empty() {
        return Err("设备 ID 为空".into());
    }
    let base = normalize_server_url(&config.server_url)?;
    let url = format!("{base}/api/v1/devices/{device_id}");
    let mut request = shared.http.request(method, url).bearer_auth(&config.token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("连接服务器失败: {error}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    let message = value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("请求失败");
    Err(format!("{message}（HTTP {status}）"))
}

/// 前端传来的画面是 `data:image/jpeg;base64,...`，直接落盘成 .jpg。
const SCREENSHOT_PREFIX: &str = "data:image/jpeg;base64,";
/// 单张截图的上限，防止异常数据把磁盘写满。
const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;

fn save_screenshot_impl(data_url: &str) -> Result<String, String> {
    let encoded = data_url
        .strip_prefix(SCREENSHOT_PREFIX)
        .ok_or_else(|| "截图数据格式不支持".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("截图数据损坏: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_SCREENSHOT_BYTES {
        return Err("截图大小异常".into());
    }
    let dir = files::download_root();
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建截图目录失败: {error}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut path = dir.join(format!("ScreenShot-{stamp}.jpg"));
    let mut index = 1;
    while path.exists() {
        path = dir.join(format!("ScreenShot-{stamp} ({index}).jpg"));
        index += 1;
    }
    std::fs::write(&path, bytes).map_err(|error| format!("写入截图失败: {error}"))?;
    tracing::info!(path = %path.display(), "远程截图已保存");
    Ok(path.to_string_lossy().to_string())
}

fn normalize_server_url(raw: &str) -> Result<String, String> {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        url = "http://127.0.0.1:8787".to_string();
    }
    if let Some(rest) = url.strip_prefix("ws://") {
        url = format!("http://{rest}");
    } else if let Some(rest) = url.strip_prefix("wss://") {
        url = format!("https://{rest}");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("服务器地址需要 http:// 或 https:// 前缀".into());
    }
    Ok(url)
}

fn to_ws_base(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    }
}

async fn account_view(shared: &Shared) -> AccountView {
    let config = shared.config.read().await.clone();
    AccountView {
        server_url: config.server_url.clone(),
        username: config.username.clone(),
        device_id: config.device_id.clone(),
        logged_in: !config.token.is_empty(),
        connected: *shared.connected.read().await,
        error: shared.error.read().await.clone(),
    }
}

async fn emit_devices(app: &AppHandle, shared: &Shared) {
    let devices = shared.devices.read().await.clone();
    let _ = app.emit("devices", devices);
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "screenwall-client".to_string())
}

/// 客户端自身没有日志输出时，采集/会话里的诊断信息全部丢失。
/// 日志写滚动文件（`%APPDATA%\ScreenWall\logs`），`RUST_LOG` 控制级别。
fn init_tracing() {
    logging::init(&base_dir());
}

/// 打开日志目录（资源管理器）。
#[tauri::command]
fn open_log_dir() -> Result<String, String> {
    let dir = logging::log_dir(&base_dir());
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建日志目录失败: {error}"))?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|error| format!("打开目录失败: {error}"))?;
    Ok(dir.display().to_string())
}

/// 导出诊断包（日志 + 脱敏配置），返回目录路径。
#[tauri::command]
fn export_diagnostics() -> Result<String, String> {
    let path = logging::export(&base_dir())?;
    tracing::info!(path = %path.display(), "已导出诊断包");
    Ok(path.display().to_string())
}

/// 检查服务器上是否有新版本（`GET /api/v1/version`）。
#[tauri::command]
async fn check_update(
    shared: State<'_, Arc<Shared>>,
) -> Result<Option<update::UpdateInfo>, String> {
    let shared = shared.inner().clone();
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if base.is_empty() {
        return Err("尚未配置服务器地址".into());
    }
    update::check_update(&shared.http, &base, &token).await
}

/// 下载新版本安装包并校验 SHA256，返回本地路径。
#[tauri::command]
async fn download_update(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    url: String,
    sha256: String,
) -> Result<String, String> {
    let shared = shared.inner().clone();
    let app = app.clone();
    update::download_update(&shared.http, &url, &sha256, move |received, total| {
        let _ = app.emit(
            "update-progress",
            json!({ "received": received, "total": total }),
        );
    })
    .await
}

/// 启动安装包并退出本程序；NSIS 静默安装，装完由用户从开始菜单打开。
#[tauri::command]
fn install_update(app: AppHandle, path: String) -> Result<(), String> {
    update::install_update(&path)?;
    tracing::info!(%path, "已启动新版本安装包，准备退出");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        tray::exit_app(&app);
    });
    Ok(())
}

/// 当前日志占用空间与文件数，设置页用来提示。
#[tauri::command]
fn get_log_info() -> Value {
    let files = logging::log_files(&base_dir());
    let bytes: u64 = files
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum();
    json!({
        "dir": logging::log_dir(&base_dir()).display().to_string(),
        "files": files.len(),
        "bytes": bytes,
    })
}

fn main() {
    // 以管理员身份重跑自身时只修防火墙规则，不初始化 Tauri。
    // 由设置页的「允许局域网直连」通过 ShellExecuteW(runas) 拉起。
    if let Some(code) = firewall::run_helper_from_args() {
        std::process::exit(code);
    }
    init_tracing();
    let mut config = ClientConfig::load();
    let mut changed = false;
    // 旧版本 client.json 里的 token 是 Agent 的共享 Token，不是账号令牌。
    if config.username.is_empty() && !config.token.is_empty() {
        config.token.clear();
        changed = true;
    }
    if changed {
        let _ = config.save();
    }
    let start_now = !config.token.is_empty() && !config.server_url.is_empty();

    let allow_control = config.allow_control;
    let ui_zoom = config.ui_zoom;
    let clipboard_on = config.clipboard_sync;
    let shared = Arc::new(Shared {
        config: RwLock::new(config),
        devices: RwLock::new(Vec::new()),
        connected: RwLock::new(false),
        error: RwLock::new(None),
        tasks: StdMutex::new(Tasks::default()),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("HTTP 客户端"),
        view_tx: RwLock::new(None),
        control: RwLock::new(None),
        pending_opens: StdMutex::new(HashMap::new()),
        pending_wake: StdMutex::new(None),
        next_request: AtomicU32::new(0),
        stats: StdMutex::new(NetStats::default()),
        auto_quality: AtomicBool::new(true),
        quality_step: AtomicUsize::new(0),
        rtt_ms: AtomicU32::new(0),
        ping_sent: StdMutex::new(None),
        pending_control_requests: StdMutex::new(HashMap::new()),
        allow_control: AtomicBool::new(allow_control),
        files: StdMutex::new(files::Hub::default()),
        clipboard_on: AtomicBool::new(clipboard_on),
        wall: StdMutex::new(wall::Hub::default()),
        group: StdMutex::new(group::Hub::default()),
        next_group: AtomicU64::new(0),
        quality: StdMutex::new(quality::Tracker::new(Instant::now(), now_ms())),
        reconnect: StdMutex::new(None),
        pending_resume: StdMutex::new(None),
        control_wanted: AtomicBool::new(false),
        direct: StdMutex::new(direct::DirectHub::default()),
    });

    let initial = shared.clone();
    let panels = Arc::new(panels::Hub::default());
    let panel_state = panels.clone();
    tauri::Builder::default()
        .manage(shared)
        .manage(panels)
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_zoom(ui_zoom);
                // 关闭窗口默认缩到托盘；用户可在设置里关掉，或在托盘菜单退出。
                let close_shared = initial.clone();
                let win = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        let to_tray = close_shared
                            .config
                            .try_read()
                            .map(|config| config.close_to_tray)
                            .unwrap_or(true);
                        if to_tray {
                            api.prevent_close();
                            let _ = win.hide();
                        }
                    }
                });
            }
            if start_now {
                spawn_tasks(app.handle().clone(), initial.clone());
            }
            // 缩略图看护：断流重开、失败退避，跟随登录状态由 wall::clear 清空。
            let wall_shared = initial.clone();
            let wall_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                wall_loop(wall_app, wall_shared).await;
            });
            // 托盘 + 连接看门狗。托盘建不起来（例如无桌面会话）不影响主功能。
            if let Err(error) = tray::setup(app.handle()) {
                tracing::warn!(%error, "创建系统托盘失败");
            }
            let watchdog_shared = initial.clone();
            let watchdog_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tray::watchdog_loop(watchdog_app, watchdog_shared).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_account,
            list_devices,
            login,
            logout,
            start_control,
            stop_control,
            send_control_input,
            set_stream_options,
            set_audio_enabled,
            get_remote_volume,
            set_remote_volume,
            wake_on_lan,
            wake_via_device,
            report_audio_stats,
            send_power,
            send_secure_attention,
            save_screenshot,
            rename_device,
            refresh_devices,
            list_audits,
            set_device_controllable,
            delete_device,
            set_auto_quality,
            get_auto_quality,
            get_connection_quality,
            get_control_info,
            get_clipboard_sync,
            set_clipboard_sync,
            answer_control_request,
            list_control_requests,
            files::offer_files,
            files::upload_chunk,
            files::list_remote_files,
            files::download_remote_file,
            files::download_remote_directory,
            files::cancel_file_transfer,
            wall::set_wall_preview,
            wall::stop_wall_preview,
            group::start_group,
            group::set_group_options,
            group::stop_group,
            group::group_input,
            get_local_settings,
            set_allow_control,
            set_close_to_tray,
            open_log_dir,
            export_diagnostics,
            get_log_info,
            check_update,
            download_update,
            install_update,
            set_ui_zoom,
            test_server,
            get_autostart,
            set_autostart,
            direct::get_direct_state,
            direct::set_direct_enabled,
            direct::regenerate_pairing_code,
            direct::forget_direct_peers,
            direct::connect_direct,
            direct::scan_direct_devices,
            set_ca_path,
            firewall::get_direct_firewall_state,
            firewall::allow_direct_firewall,
            panels::get_panel_status,
            panels::show_server_panel,
            panels::hide_server_panel,
            panels::resize_server_panel
        ])
        .build(tauri::generate_context!())
        .expect("ScreenWall 启动失败")
        .run(move |app, event| {
            // 应用真正退出时收拾面板子进程；主窗口「关闭到托盘」不会走到这里。
            if let tauri::RunEvent::Exit = event {
                panels::shutdown(app, &panel_state);
            }
        });
}
