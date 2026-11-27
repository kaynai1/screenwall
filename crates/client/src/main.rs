#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod direct;
mod errors;
mod files;
mod firewall;
mod group;
mod logging;
mod p2p;
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
    SecureAttention, StreamKind, VideoBitrate, VideoCodec, VideoKeyframe, VideoSub, VideoUnsub,
    DISPLAY_ALL, INPUT_SAS, PROTO_VERSION,
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
    /// 已授权过的直连设备再次连接时不再弹「允许控制」。
    ///
    /// 只认长期 token：那是在用户第一次点「允许」时签发的。
    /// 默认开启，关闭后恢复每次确认。
    #[serde(default = "default_true")]
    remember_authorized: bool,
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
    /// 本机没有任何显示器时，临时接入一块虚拟显示器供远程采集。
    ///
    /// 首次开启会在会话开始时弹一次 UAC 安装 Amyuni usbmmidd_v2（微软签名的
    /// 间接显示驱动，第三方组件，可能弹一次广告页）。默认关闭。
    #[serde(default)]
    virtual_display: bool,
    /// 允许别人用「9 位连接码 + 密码」连接本机（不需要同一账号）。
    #[serde(default)]
    connect_enabled: bool,
    /// 本机连接密码：明文只留在本机，登录时把哈希同步给服务器。
    #[serde(default)]
    connect_password: String,
    /// 服务器分配的 9 位连接码（缓存，离线时也能显示）。
    #[serde(default)]
    connect_code: String,
    /// 本机识别码（9 位数字），首次启动生成后固定不变。
    ///
    /// 与 `connect_code` 的区别：那个是服务器分配的，只有登录后才有；
    /// 这个是本机自己算的，**纯局域网、不登录也能用** —— 局域网里
    /// 对方输这 9 位数字，本机通过 UDP 广播应答把地址告诉他。
    #[serde(default)]
    local_id: String,
    /// 本机模式：完全不用服务器，只走局域网直连。
    ///
    /// 打开后界面正常进入，但不会去连 `/ws/view`、`/ws/device`，
    /// 设备列表来自已配对的直连设备。这是默认推荐的使用方式 ——
    /// 两台机器在同一网段时，不需要账号、不需要服务器。
    #[serde(default)]
    local_mode: bool,
    /// 允许跨网段直连：TCP 连不上时，用 STUN 探出公网映射并尝试 UDP 打洞。
    ///
    /// 打洞成功后画面直接在两台机器之间走，不经过任何服务器。打不通
    /// （双方都是对称型 NAT）时自动放弃，不影响原来的 TCP 直连和中转。
    #[serde(default = "default_true")]
    punch_through: bool,
    /// UDP 打洞用的本机端口；0 表示由系统分配。
    ///
    /// 固定端口的好处是路由器上的端口映射重启后仍然有效；不固定也能打通，
    /// 只是每次重启映射都会变。
    #[serde(default)]
    punch_port: u16,
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
            remember_authorized: true,
            ui_zoom: 1.0,
            clipboard_sync: true,
            close_to_tray: true,
            direct_enabled: false,
            direct_port: direct::DEFAULT_PORT,
            known_peers: Vec::new(),
            direct_tokens: Vec::new(),
            max_bitrate: None,
            ca_path: None,
            virtual_display: false,
            connect_enabled: false,
            connect_password: String::new(),
            connect_code: String::new(),
            local_id: String::new(),
            local_mode: false,
            punch_through: true,
            punch_port: 0,
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

pub(crate) fn base_dir() -> PathBuf {
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
    /// 这次是「连接码」临时会话，不是账号登录：界面上隐藏账号相关入口。
    quick: bool,
    /// 本机模式：没连服务器，只用局域网直连。
    local: bool,
    /// 服务器 `/healthz` 报的版本；还没探测到或本机模式时为 `None`。
    /// 界面用它提示「服务器版本较旧」。
    server_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlInfo {
    session: u32,
    device_id: String,
    request: u32,
    client_token: u64,
    /// 这条会话实际走的链路：`relay`（服务器中转）、`direct`（局域网直连）
    /// 或 `p2p`（同账号 UDP 打洞）。界面按它显示「连接方式」。
    link: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlStateEvent {
    phase: String,
    device_id: String,
    session: u32,
    client_token: u64,
    /// 这条会话实际走的链路，侧栏胶囊按它显示「P2P 直连 / 服务器中转」。
    link: &'static str,
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
    /// `"jpeg"` 或 `"h264"`。
    codec: String,
    /// H.264 关键帧（含 SPS/PPS）；界面在解出关键帧前丢弃后续帧。
    keyframe: bool,
    /// JPEG 帧的 data URL（`data:image/jpeg;base64,…`），H.264 时为空串。
    data_url: String,
    /// H.264 Annex-B 帧的 base64；JPEG 时为空串。
    data: String,
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
    /// 当前视频订阅商定的编码（以 `video.sub.ok` 为准）。
    codec: VideoCodec,
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

/// 自适应阶梯：先调 JPEG 画质，降到最低档还跟不上才动分辨率。
///
/// 降画质只减少编码字节数，代价最小；降分辨率要重新订阅并多一次缩放，
/// 放在最后，且最低只到 1280 宽 —— 再低就没法操作了。
const QUALITY_LADDER: [u8; 5] = [75, 65, 55, 48, 42];

/// 自适应可以走的分辨率档位，与界面上的四档一致。
const WIDTH_LADDER: [u32; 4] = [1280, 1920, 2560, 3840];

/// 连续多少个健康窗口才允许往上调一档（每窗口 2 秒）。
const UPGRADE_STREAK: u32 = 6;
/// RTT 超过会话最小值的这个比例 + 余量，就认为链路上已经排上队。
const RTT_CONGESTED_RATIO: f64 = 1.8;
const RTT_CONGESTED_SLACK_MS: f64 = 25.0;
/// RTT 回落到这个范围之内才算链路干净，可以尝试升档。
const RTT_CLEAR_RATIO: f64 = 1.3;
const RTT_CLEAR_SLACK_MS: f64 = 10.0;

/// 实时码率的下限（kbps）。再往下画面只剩色块，这时改为降分辨率。
const VIDEO_KBPS_FLOOR: u32 = 1_500;

/// 会话默认视频码率（kbps），口径与采集端的 `h264_bitrate` 一致。
///
/// 自适应以它为上限，只临时往下调；用户配置了码率上限时一起取小。
fn default_video_kbps(width: u32, fps: u16, cap: Option<u32>) -> u32 {
    let base = match width {
        w if w >= 3000 => 25_000u64,
        w if w >= 2400 => 14_000,
        w if w >= 1600 => 8_000,
        w if w >= 1200 => 5_000,
        _ => 3_000,
    };
    let fps = if fps == 0 { 60 } else { fps.min(240) } as u64;
    let mut kbps = (base * fps / 60).max(2_000);
    if let Some(cap) = cap {
        kbps = kbps.min(u64::from(cap));
    }
    kbps.clamp(u64::from(VIDEO_KBPS_FLOOR), 100_000) as u32
}

/// 画质阶梯上降一档；已经是最后一档返回 `None`。
fn quality_down(current: u8) -> Option<u8> {
    QUALITY_LADDER
        .iter()
        .position(|candidate| *candidate == current)
        .and_then(|index| QUALITY_LADDER.get(index + 1))
        .copied()
}

/// 画质阶梯上升一档；已经是最清晰返回 `None`。
fn quality_up(current: u8) -> Option<u8> {
    QUALITY_LADDER
        .iter()
        .position(|candidate| *candidate == current)
        .filter(|index| *index > 0)
        .map(|index| QUALITY_LADDER[index - 1])
}

/// 分辨率降一档（只走阶梯里比当前小的最大值）。
fn width_down(current: u32) -> Option<u32> {
    WIDTH_LADDER
        .iter()
        .rev()
        .copied()
        .find(|width| *width < current)
}

/// 分辨率升一档，不超过用户选的上限。
fn width_up(current: u32, ceiling: u32) -> Option<u32> {
    WIDTH_LADDER
        .iter()
        .copied()
        .find(|width| *width > current && *width <= ceiling)
}

/// 自适应的一步动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdaptAction {
    Hold,
    Quality(u8),
    Width(u32),
}

/// 根据这一窗口的链路状态决定下一步。
///
/// 下调是即时的：一旦认定拥塞就先降画质，画质到底再降分辨率。
/// 上调是保守的：连续 [`UPGRADE_STREAK`] 个健康窗口才升一档，且先升画质
/// 再升分辨率，避免在临界点上来回抖动。
fn adapt_decision(
    congested: bool,
    healthy: bool,
    good_windows: u32,
    quality: u8,
    width: u32,
    ceiling: u32,
) -> AdaptAction {
    if congested {
        return quality_down(quality)
            .map(AdaptAction::Quality)
            .or_else(|| width_down(width).map(AdaptAction::Width))
            .unwrap_or(AdaptAction::Hold);
    }
    if healthy && good_windows >= UPGRADE_STREAK {
        return quality_up(quality)
            .map(AdaptAction::Quality)
            .or_else(|| width_up(width, ceiling).map(AdaptAction::Width))
            .unwrap_or(AdaptAction::Hold);
    }
    AdaptAction::Hold
}

/// 按编码格式修正自适应动作。
///
/// `quality` 只影响 MJPEG 的 JPEG 质量档；H.264 的码率由分辨率/帧率决定，
/// 改 `quality` 不会改变画面，却会让被控端重新订阅、重建编码器 ——
/// 每降一档就是一次可见的卡顿。所以 H.264 会话里把画质档直接换成宽度档：
/// 拥塞时降分辨率，恢复后先升回分辨率。
fn adapt_for_codec(
    action: AdaptAction,
    h264: bool,
    congested: bool,
    width: u32,
    ceiling: u32,
) -> AdaptAction {
    if !h264 {
        return action;
    }
    match action {
        AdaptAction::Quality(_) if congested => width_down(width)
            .map(AdaptAction::Width)
            .unwrap_or(AdaptAction::Hold),
        AdaptAction::Quality(_) => width_up(width, ceiling)
            .map(AdaptAction::Width)
            .unwrap_or(AdaptAction::Hold),
        other => other,
    }
}

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
    /// 视频订阅的串行化锁：握手后的那次订阅和界面推来的画质参数可能同时到，
    /// 两边都「先看状态再发 `video.sub`」，不串行就会发出两条订阅
    /// （第二条会把第一条的流换掉，`video.sub.ok` 也会错位）。
    /// 加锁顺序固定为 `video_sub` → `control`，任何地方都不要反过来。
    video_sub: tokio::sync::Mutex<()>,
    pending_opens: StdMutex<HashMap<u32, oneshot::Sender<Result<u32, String>>>>,
    /// 等待被控端 P2P 应答的请求（`request` → 应答内容）。
    ///
    /// 和 `pending_opens` 分开：P2P 失败要静默回退到中转，不能当成会话失败。
    p2p_waits: StdMutex<HashMap<u32, oneshot::Sender<serde_json::Value>>>,
    /// 正在进行的 P2P 升级（同一时间只允许一条，失败就回退中转）。
    p2p_busy: AtomicBool,
    /// 异地开机代发的在途会话（最多一条）。
    pending_wake: StdMutex<Option<PendingWake>>,
    next_request: AtomicU32,
    /// 最近 2 秒收到的视频帧数/字节数。
    stats: StdMutex<NetStats>,
    /// 自适应画质开关（默认开）。
    auto_quality: AtomicBool,
    /// 当前处于自适应阶梯的第几级。
    quality_step: AtomicUsize,
    /// 自适应实时调下来的目标码率（kbps）；0 表示按分辨率/帧率算默认值。
    ///
    /// H.264 会话里优先动这个值：被控端直接改编码器参数，不重新订阅，
    /// 不会像改分辨率那样造成一次可见卡顿。
    video_kbps: AtomicU32,
    /// 用户手动选的分辨率上限（自适应降分辨率后仍按它回升，不会越过用户的选择）。
    /// 0 表示未知，按当前请求宽度当作上限。
    user_max_width: AtomicU32,
    /// 最近一次隧道 Ping 的往返时延，0 表示未知。
    rtt_ms: AtomicU32,
    /// 已发出但还没回 Ping 的时间戳。
    ping_sent: StdMutex<Option<(u64, Instant)>>,
    /// 等待本机用户确认的远程控制请求（被控端授权）。
    pending_control_requests: StdMutex<HashMap<u32, PendingControlRequest>>,
    /// 「允许被控」开关，运行中改立即生效（不用重连）。
    allow_control: AtomicBool,
    /// 已授权过的直连设备免重复确认（运行中立即生效）。
    remember_authorized: AtomicBool,
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
    /// 控制页被切走时暂停视频帧推送。
    ///
    /// 保活的页面只是 `display:none`，订阅和会话都还在：4K MJPEG 每秒几十帧
    /// 继续经 Tauri 事件送进 WebView，主线程光解析 base64 就会卡住点击。
    /// 置位后帧循环直接跳过编码与 emit，回到控制页再恢复。
    video_muted: AtomicBool,
    /// 服务器 `/healthz` 报的版本号。旧服务器不认识 `p2p` 信令，客户端据此
    /// 跳过打洞尝试，避免每次点「控制」都白等 8 秒才回退中转。
    server_version: StdMutex<Option<String>>,
    /// 界面里的 WebCodecs `VideoDecoder` 是否可用（前端探测后通过命令告诉我们）。
    /// 只有为 true 时才会在能力声明里加 `video.h264`，否则老被控端会白白
    /// 发出一条浏览器解不了的 H.264 流。
    h264_decode: AtomicBool,
    /// 这次运行里 H.264 已经失败过（解码报错/被控端拒绝），不再尝试。
    h264_fallback: AtomicBool,
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
    /// 连接码模式：只跑主控端（view），不跑被控端（host），令牌也不写盘。
    quick_mode: AtomicBool,
    /// 还没交给前端的 `screenwall://` 链接。
    ///
    /// 应用启动时前端尚未加载，直接 emit 会丢事件，所以先存这里，
    /// 前端挂载后调用 `take_connect_link` 主动取；运行中再由事件推送。
    pending_link: StdMutex<Option<(String, String)>>,
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
    shared.quick_mode.store(false, Ordering::Relaxed);
    spawn_tasks(app.clone(), shared.clone());
    emit_devices(&app, &shared).await;
    // 把本机保存的连接密码/开关补同步给服务器（首次登录时服务器还没有）。
    sync_connect_settings(&shared).await;
    let view = account_view(&shared).await;
    let _ = app.emit("account", view.clone());
    Ok(view)
}

#[tauri::command]
async fn logout(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    // 连接码会话没有账号可退，直接结束临时会话。
    if shared.quick_mode.load(Ordering::Relaxed) {
        return Ok(end_quick_session(&app, &shared).await);
    }
    // 本机模式没有服务器会话可退，直接回登录页。
    if shared.config.read().await.local_mode {
        return Ok(exit_local_mode(&app, &shared).await);
    }
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
    // 换账号/换服务器时旧版本号必须作废，否则会按上一台服务器的能力做判断。
    *shared.server_version.lock().unwrap() = None;

    emit_devices(&app, &shared).await;
    let view = account_view(&shared).await;
    let _ = app.emit("account", view.clone());
    Ok(view)
}

// ---------------------------------------------------------------- 本机模式（无服务器）

/// 进入本机模式：不连服务器，只用局域网直连。
///
/// 这是「点对点」的默认形态 —— 两台机器在同一网段，一台生成配对码，
/// 另一台输地址和码就能连，全程没有账号、没有云端中转。
///
/// 进入时顺手打开直连监听并生成配对码，用户不必再进设置点一次开关。
#[tauri::command]
async fn enter_local_mode(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    if shared.quick_mode.load(Ordering::Relaxed) {
        return Err("请先断开当前的连接码会话".into());
    }

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
        config.local_mode = true;
        // 本机模式不持有任何服务器凭据。
        config.token.clear();
        config.username.clear();
        config.server_url.clear();
        if config.device_id.is_empty() {
            config.device_id = screenwall_agent::config::generate_device_id();
        }
        // 默认打开直连监听：本机模式的全部价值就在这条链路上。
        config.direct_enabled = true;
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }

    *shared.connected.write().await = false;
    *shared.error.write().await = None;
    {
        let config = shared.config.read().await.clone();
        *shared.devices.write().await = local_devices(&config);
    }
    spawn_tasks(app.clone(), shared.clone());
    emit_devices(&app, &shared).await;
    let view = account_view(&shared).await;
    let _ = app.emit("account", view.clone());
    Ok(view)
}

/// 退出本机模式，回登录页。已配对的直连设备保留，下次进来还能用。
async fn exit_local_mode(app: &AppHandle, shared: &Arc<Shared>) -> AccountView {
    cancel_reconnect(shared, None);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    stop_tasks(shared);
    *shared.view_tx.write().await = None;
    *shared.control.write().await = None;
    shared.pending_opens.lock().unwrap().clear();
    shared.files.lock().unwrap().reset();
    wall::clear(shared);
    group::clear(shared);
    {
        let mut config = shared.config.write().await;
        config.local_mode = false;
        config
            .save()
            .map_err(|error| tracing::warn!("保存配置失败: {error}"))
            .ok();
    }
    shared.devices.write().await.clear();
    *shared.connected.write().await = false;
    *shared.error.write().await = None;
    emit_devices(app, shared).await;
    let view = account_view(shared).await;
    let _ = app.emit("account", view.clone());
    view
}

// ---------------------------------------------------------------- 连接码（ID + 密码）

/// 设置页「连接码」卡片的数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectCodeView {
    code: String,
    enabled: bool,
    has_password: bool,
    /// 本机保存的明文密码（只通过本地 IPC 返回给设置页显示）。
    password: Option<String>,
}

async fn get_api(client: &reqwest::Client, url: String, token: &str) -> Result<Value, String> {
    let response = client
        .get(url)
        .bearer_auth(token)
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

/// 登录后把本机保存的连接码设置补同步给服务器，并缓存服务器分配的连接码。
async fn sync_connect_settings(shared: &Arc<Shared>) {
    let (base, token, enabled, password) = {
        let config = shared.config.read().await;
        (
            config.server_url.clone(),
            config.token.clone(),
            config.connect_enabled,
            config.connect_password.clone(),
        )
    };
    if token.is_empty() || base.is_empty() {
        return;
    }
    if !password.is_empty() || enabled {
        let mut body = json!({ "enabled": enabled });
        if !password.is_empty() {
            body["password"] = json!(password);
        }
        if let Err(error) = post_api(
            &shared.http,
            format!("{base}/api/v1/quick"),
            &body,
            Some(&token),
        )
        .await
        {
            tracing::warn!("同步连接码设置失败: {error}");
        }
    }
    match get_api(&shared.http, format!("{base}/api/v1/quick"), &token).await {
        Ok(value) => {
            let code = value["code"].as_str().unwrap_or_default().to_string();
            let mut config = shared.config.write().await;
            config.connect_code = code;
            let _ = config.save();
        }
        Err(error) => tracing::warn!("读取连接码失败: {error}"),
    }
}

#[tauri::command]
async fn get_connect_code(shared: State<'_, Arc<Shared>>) -> Result<ConnectCodeView, String> {
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if token.is_empty() {
        return Err("尚未登录".into());
    }
    let value = get_api(&shared.http, format!("{base}/api/v1/quick"), &token).await?;
    let mut config = shared.config.write().await;
    let view = ConnectCodeView {
        code: value["code"].as_str().unwrap_or_default().to_string(),
        enabled: value["enabled"].as_bool().unwrap_or(false),
        has_password: value["hasPassword"].as_bool().unwrap_or(false),
        password: if config.connect_password.is_empty() {
            None
        } else {
            Some(config.connect_password.clone())
        },
    };
    config.connect_code = view.code.clone();
    let _ = config.save();
    Ok(view)
}

#[tauri::command]
async fn set_connect_code(
    shared: State<'_, Arc<Shared>>,
    enabled: bool,
    password: Option<String>,
) -> Result<ConnectCodeView, String> {
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if token.is_empty() {
        return Err("尚未登录".into());
    }
    let password = password
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    post_api(
        &shared.http,
        format!("{base}/api/v1/quick"),
        &json!({ "enabled": enabled, "password": password.as_deref() }),
        Some(&token),
    )
    .await?;

    {
        let mut config = shared.config.write().await;
        config.connect_enabled = enabled;
        if let Some(password) = password {
            config.connect_password = password;
        }
        config
            .save()
            .map_err(|error| format!("保存配置失败: {error}"))?;
    }
    get_connect_code(shared).await
}

#[tauri::command]
async fn rotate_connect_code(shared: State<'_, Arc<Shared>>) -> Result<ConnectCodeView, String> {
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if token.is_empty() {
        return Err("尚未登录".into());
    }
    post_api(
        &shared.http,
        format!("{base}/api/v1/quick/rotate"),
        &json!({}),
        Some(&token),
    )
    .await?;
    get_connect_code(shared).await
}

/// 本机（被控端）生成一条一次性连接链接。
///
/// 链接 10 分钟有效、只能使用一次；点开后会唤起客户端并直接连入本机。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectLinkView {
    url: String,
    expires_at: i64,
    expires_in_sec: i64,
}

#[tauri::command]
async fn create_connect_link(
    shared: State<'_, Arc<Shared>>,
    device_id: Option<String>,
) -> Result<ConnectLinkView, String> {
    let (base, token) = {
        let config = shared.config.read().await;
        (config.server_url.clone(), config.token.clone())
    };
    if token.is_empty() {
        return Err("尚未登录".into());
    }
    let mut body = json!({});
    if let Some(device_id) = device_id.filter(|value| !value.trim().is_empty()) {
        body["deviceId"] = json!(device_id);
    }
    let value = post_api(
        &shared.http,
        format!("{base}/api/v1/connect-link"),
        &body,
        Some(&token),
    )
    .await?;
    Ok(ConnectLinkView {
        url: value["url"].as_str().unwrap_or_default().to_string(),
        expires_at: value["expiresAt"].as_i64().unwrap_or(0),
        expires_in_sec: value["expiresInSec"].as_i64().unwrap_or(0),
    })
}

/// 用一次性链接令牌换会话：匿名、单次消费，失败时链接就作废了。
///
/// 与连接码不同，这里没有密码：链接本身就是凭据，所以只允许短期、单次。
#[tauri::command]
async fn connect_by_link(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    server_url: String,
    token: String,
) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    connect_by_link_inner(&app, &shared, server_url, token).await
}

async fn connect_by_link_inner(
    app: &AppHandle,
    shared: &Arc<Shared>,
    server_url: String,
    token: String,
) -> Result<AccountView, String> {
    if shared.quick_mode.load(Ordering::Relaxed) {
        return Err("已经在一个远程会话里，请先断开".into());
    }
    {
        let config = shared.config.read().await;
        if !config.token.is_empty() {
            return Err("请先退出当前账号，再使用连接链接".into());
        }
    }
    let base = normalize_server_url(&server_url)?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("连接链接缺少令牌".into());
    }

    let value = post_api(
        &shared.http,
        format!("{base}/api/v1/connect-link/use"),
        &json!({ "token": token }),
        None,
    )
    .await?;
    let session_token = value["token"]
        .as_str()
        .ok_or_else(|| "服务器没有返回连接令牌".to_string())?
        .to_string();
    let device_id = value["device"]["id"]
        .as_str()
        .ok_or_else(|| "服务器没有返回设备 ID".to_string())?
        .to_string();
    let device_name = value["device"]["name"]
        .as_str()
        .unwrap_or("远程设备")
        .to_string();

    Ok(enter_quick_session(app, shared, base, session_token, device_id, device_name).await)
}

/// 解析网页链接 `http(s)://host/l/<token>`，返回（服务器地址, 令牌）。
///
/// 登录页允许直接粘贴链接，省去「先点开网页再点按钮」这一步。
fn parse_web_link(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let scheme_end = raw.find("://")?;
    let scheme = &raw[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &raw[scheme_end + 3..];
    let (host, path) = rest.split_once('/')?;
    let token = path.strip_prefix("l/")?.trim();
    if host.is_empty() || token.is_empty() || token.contains('/') {
        return None;
    }
    Some((format!("{scheme}://{host}"), token.to_string()))
}

/// 解析 `screenwall://connect?server=...&token=...`。
///
/// 手写解析而不引 URL 库：这个协议只有一种形状，字段只有两个。
fn parse_connect_link(raw: &str) -> Option<(String, String)> {
    let rest = raw.trim();
    let rest = rest.strip_prefix("screenwall://")?;
    let rest = rest.strip_prefix("connect")?;
    let query = rest.strip_prefix('?')?;
    let mut server = String::new();
    let mut token = String::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key {
            "server" => server = value,
            "token" => token = value,
            _ => {}
        }
    }
    if server.is_empty() || token.is_empty() {
        return None;
    }
    Some((server, token))
}

/// 只解码 `%XX`，够用了：协议里的值由服务器用 `encodeURIComponent` 生成。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 处理一条 `screenwall://` 链接：交给前端，由前端决定直接连还是先提示。
///
/// 不在这里直接建会话：链接可能是在账号已登录时打开的，前端需要先提示
/// 「会退出当前账号」，用户确认后才调用 `connect_by_link`。
fn dispatch_connect_link(app: &AppHandle, raw: &str) {
    let Some((server, token)) = parse_connect_link(raw) else {
        tracing::warn!("无法解析的连接链接: {raw}");
        return;
    };
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    // 先存一份：前端可能还没挂载监听。
    if let Some(shared) = app.try_state::<Arc<Shared>>() {
        *shared.pending_link.lock().unwrap() = Some((server.clone(), token.clone()));
    }
    let _ = app.emit("connect-link", json!({ "server": server, "token": token }));
}

/// 前端挂载后主动取走待处理的连接链接（启动时通过协议唤起的情况）。
#[tauri::command]
fn take_connect_link(shared: State<'_, Arc<Shared>>) -> Option<ConnectLinkInput> {
    shared
        .pending_link
        .lock()
        .unwrap()
        .take()
        .map(|(server, token)| ConnectLinkInput { server, token })
}

#[derive(Debug, Clone, Serialize)]
struct ConnectLinkInput {
    server: String,
    token: String,
}

/// 用「连接码 + 密码」临时连接一台设备：不登录账号、不写盘、不启动被控端。
#[tauri::command]
async fn quick_connect(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    server_url: String,
    code: String,
    password: String,
) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    if shared.quick_mode.load(Ordering::Relaxed) {
        return Err("已经在一个连接码会话里".into());
    }
    {
        let config = shared.config.read().await;
        if !config.token.is_empty() {
            return Err("请先退出当前账号，再使用连接码连接".into());
        }
    }
    let base = normalize_server_url(&server_url)?;
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err("请填写连接码".into());
    }
    // 直接粘贴一条连接链接（`http://host/l/<token>`）也能连：不必先打开浏览器。
    if let Some((link_base, link_token)) = parse_web_link(&code) {
        return connect_by_link_inner(&app, &shared, link_base, link_token).await;
    }
    if password.is_empty() {
        return Err("请填写连接密码".into());
    }

    let value = post_api(
        &shared.http,
        format!("{base}/api/v1/quick/connect"),
        &json!({ "code": code, "password": password, "name": hostname() }),
        None,
    )
    .await?;
    let token = value["token"]
        .as_str()
        .ok_or_else(|| "服务器没有返回连接令牌".to_string())?
        .to_string();
    let device_id = value["device"]["id"]
        .as_str()
        .ok_or_else(|| "服务器没有返回设备 ID".to_string())?
        .to_string();
    let device_name = value["device"]["name"]
        .as_str()
        .unwrap_or("远程设备")
        .to_string();

    Ok(enter_quick_session(&app, &shared, base, token, device_id, device_name).await)
}

/// 用已经换到的受限令牌进入连接码会话。
///
/// 连接码连接与一次性链接连接共用：两者拿到的都是 `kind = 'quick'` 的受限令牌，
/// 后续清理、置位、起任务的流程完全一致。
async fn enter_quick_session(
    app: &AppHandle,
    shared: &Arc<Shared>,
    base: String,
    token: String,
    device_id: String,
    device_name: String,
) -> AccountView {
    // 与登录共用同一套清理：旧的重连、挂起会话都不能留着。
    cancel_reconnect(shared, None);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    stop_tasks(shared);
    *shared.view_tx.write().await = None;
    *shared.control.write().await = None;
    shared.pending_opens.lock().unwrap().clear();
    shared.files.lock().unwrap().reset();
    wall::clear(shared);
    group::clear(shared);

    shared.quick_mode.store(true, Ordering::Relaxed);
    {
        let mut config = shared.config.write().await;
        config.server_url = base;
        // 受限令牌只放在内存里，不落盘；重启后自然失效。
        config.token = token;
        config.username.clear();
        config.device_id = device_id.clone();
    }
    *shared.connected.write().await = false;
    *shared.error.write().await = None;
    *shared.devices.write().await = vec![DeviceView {
        id: device_id,
        name: device_name,
        os: "Windows".to_string(),
        version: String::new(),
        online: true,
        last_seen: chrono::Utc::now().timestamp_millis(),
        controllable: true,
    }];
    spawn_tasks(app.clone(), shared.clone());
    emit_devices(app, shared).await;
    let view = account_view(shared).await;
    let _ = app.emit("account", view.clone());
    view
}

async fn end_quick_session(app: &AppHandle, shared: &Arc<Shared>) -> AccountView {
    cancel_reconnect(shared, None);
    *shared.pending_resume.lock().unwrap() = None;
    shared.control_wanted.store(false, Ordering::Relaxed);
    stop_tasks(shared);
    *shared.view_tx.write().await = None;
    *shared.control.write().await = None;
    shared.pending_opens.lock().unwrap().clear();
    shared.files.lock().unwrap().reset();
    wall::clear(shared);
    group::clear(shared);
    shared.quick_mode.store(false, Ordering::Relaxed);
    {
        let mut config = shared.config.write().await;
        config.token.clear();
        config.username.clear();
        config.device_id.clear();
    }
    shared.devices.write().await.clear();
    *shared.connected.write().await = false;
    *shared.error.write().await = None;
    emit_devices(app, shared).await;
    let view = account_view(shared).await;
    let _ = app.emit("account", view.clone());
    view
}

/// 结束连接码会话，回到登录页。
#[tauri::command]
async fn quick_disconnect(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
) -> Result<AccountView, String> {
    let shared = shared.inner().clone();
    if !shared.quick_mode.load(Ordering::Relaxed) {
        return Ok(account_view(&shared).await);
    }
    Ok(end_quick_session(&app, &shared).await)
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

/// 支持同账号 P2P 的最低被控端版本。
///
/// 更早的被控端不认识 `p2p` 消息，发了只会白等一轮超时。版本号由服务器
/// 随设备列表下发，所以这里能先判断再决定要不要试。
const P2P_MIN_VERSION: &str = "0.2.10";

/// 同一时间只允许一条 P2P 尝试（用户连续点两台设备时不至于同时打两条洞）。
struct P2pBusyGuard<'a>(&'a AtomicBool);

impl Drop for P2pBusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// 同账号 P2P 直连：经服务器交换候选地址 → 双向打洞 → 在 QUIC 上建会话。
///
/// 返回 `None` 表示这次不走 P2P（版本不够、打洞失败、对端拒绝），
/// 调用方继续用服务器中转，用户看不到错误。
async fn try_p2p(
    app: &AppHandle,
    shared: &Arc<Shared>,
    device_id: &str,
    client_token: u64,
) -> Option<ControlInfo> {
    if shared.quick_mode.load(Ordering::Relaxed) {
        // 连接码会话的目标设备不在本账号下，服务器不会替它转发 P2P 请求。
        return None;
    }
    // 服务器要先认识 `p2p` 信令才会把它转给被控端。服务器版本低于这一档时
    // 发出去没有任何应答，只会让「点控制」白等 8 秒才回退中转——这也正是
    // 旧服务器上「允许控制」弹窗要等十几秒才出现的原因。
    let server_supports_p2p = shared
        .server_version
        .lock()
        .unwrap()
        .as_deref()
        .is_none_or(|version| update::version_gt(version, P2P_MIN_VERSION));
    if !server_supports_p2p {
        tracing::debug!("服务器版本不支持 p2p 信令，直接走中转");
        return None;
    }
    if !shared.config.read().await.punch_through {
        return None;
    }
    let capable = {
        let devices = shared.devices.read().await;
        devices
            .iter()
            .find(|device| device.id == device_id)
            .is_some_and(|device| {
                // 离线的设备连中转都建不起来，更别说打洞：直接走中转，
                // 由服务器回一条明确的「设备离线」。
                device.online
                    && device.controllable
                    && update::version_gt(&device.version, P2P_MIN_VERSION)
            })
    };
    if !capable {
        return None;
    }
    if shared.p2p_busy.swap(true, Ordering::Relaxed) {
        return None;
    }
    let _busy = P2pBusyGuard(&shared.p2p_busy);

    let out = shared.view_tx.read().await.clone()?;
    // 绑定 + STUN 是阻塞的（每个 STUN 服务器上限 2 秒），放阻塞线程池。
    let (punch, mapping) = tokio::task::spawn_blocking(|| {
        let mut punch = screenwall_agent::nat::PunchSocket::bind(0)?;
        // 只给 3 秒：拿不到公网映射还能靠内网地址打通，没必要为它
        // 让「点连接」卡上十几秒。
        let mapping = punch.discover_within(Duration::from_secs(3)).ok();
        Ok::<_, std::io::Error>((punch, mapping))
    })
    .await
    .ok()?
    .ok()?;

    let port = punch.local_addr().ok()?.port();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(mapping) = mapping {
        candidates.push(mapping.public.to_string());
    }
    for ip in direct::local_addresses() {
        candidates.push(format!("{ip}:{port}"));
    }
    if candidates.is_empty() {
        return None;
    }

    let request = shared
        .next_request
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    let (tx, rx) = oneshot::channel();
    shared.p2p_waits.lock().unwrap().insert(request, tx);
    let offer = json!({
        "t": "p2p",
        "request": request,
        "deviceId": device_id,
        "viewer": hostname(),
        "candidates": candidates
    })
    .to_string();
    if out.send(ViewOut::Text(offer)).is_err() {
        shared.p2p_waits.lock().unwrap().remove(&request);
        return None;
    }

    let answer = match tokio::time::timeout(Duration::from_secs(8), rx).await {
        Ok(Ok(answer)) => answer,
        _ => {
            shared.p2p_waits.lock().unwrap().remove(&request);
            tracing::debug!("P2P 请求没有应答，回退服务器中转");
            return None;
        }
    };
    if answer.get("event").and_then(Value::as_str) != Some("answer") {
        let reason = answer
            .get("msg")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        tracing::info!(msg = reason, "被控端拒绝 P2P，回退服务器中转");
        return None;
    }
    let peers: Vec<std::net::SocketAddr> = answer
        .get("candidates")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|addr| addr.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let fingerprint = answer
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let token = answer
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if peers.is_empty() || fingerprint.is_empty() || token.is_empty() {
        return None;
    }

    match direct::connect_offer(
        app,
        shared,
        device_id.to_string(),
        punch,
        peers,
        fingerprint,
        token,
        client_token,
    )
    .await
    {
        Ok(info) => Some(info),
        Err(error) => {
            tracing::info!("P2P 直连失败，回退服务器中转: {error}");
            None
        }
    }
}

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

    // 先试同账号 P2P 直连：两端打洞通了就不经过中转，带宽和延迟都由
    // 实际链路决定。失败（被控端旧版本、NAT 打不通、对端关闭打洞）就
    // 安静地走下面的中转流程，用户无感。
    if let Some(info) = try_p2p(app, shared, device_id, client_token).await {
        return Ok(info);
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
        // 初始订阅参数：1080p / 60 / Q65。
        // `welcome` 到达后会按协商出的编码立即订阅一次。默认 60 而不是 30：
        // 30fps 下拖动窗口会明显发顿，1080p 硬件编码带 60fps 没有压力。
        fps: 60,
        max_width: 1920,
        quality: 65,
        codec: VideoCodec::Mjpeg,
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
        caps: Capabilities::current_with(local_caps(shared)),
    });
    if !send_tunnel_json(&out, session, &hello) {
        abort_pending_control(shared, client_token).await;
        return Err("主控连接已断开".into());
    }

    // 视频订阅推迟到 `welcome`：只有拿到双方能力交集之后才知道该发 MJPEG
    // 还是 H.264，先发一次再切换会多出一段空窗，也会让迟到的 `video.sub.ok`
    // 覆盖新编码。`welcome` 由读循环处理，那里按协商结果订阅。

    Ok(ControlInfo {
        session,
        device_id: device_id.to_string(),
        request,
        client_token,
        link: "relay",
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
        // 会话已经取走，`control_out` 查不到它挂在哪个通道上，直接按链路登记判断：
        // 有链路（局域网直连或同账号 P2P）就走链路，否则走服务器。
        if let Some(out) = route_control_out(
            shared,
            control.session,
            control.direct_link.is_some(),
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

/// 本机（主控端）运行时额外能力。
///
/// 只有界面确认 `window.VideoDecoder` 可用、且这次运行里还没有回退过，
/// 才声明 `video.h264`；否则握手交集里不会出现它，被控端也就不会发 H.264。
pub(crate) fn local_caps(shared: &Shared) -> &'static [&'static str] {
    if shared.h264_decode.load(Ordering::Relaxed) && !shared.h264_fallback.load(Ordering::Relaxed) {
        &[screenwall_proto::caps::VIDEO_H264]
    } else {
        &[]
    }
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Mjpeg => "mjpeg",
        VideoCodec::H264 => "h264",
    }
}

/// 按会话当前参数构造订阅消息。
fn video_sub_for(
    control: &ControlSession,
    codec: VideoCodec,
    max_bitrate: Option<u32>,
) -> VideoSub {
    VideoSub {
        stream: StreamKind::Focus,
        display: control.display,
        fps: control.fps,
        max_width: Some(control.max_width.max(320)),
        quality: Some(control.quality.clamp(30, 90)),
        codec,
        // 远程操作必须看得到被控端光标；界面暂无开关，默认开启。
        cursor: true,
        max_bitrate,
    }
}

/// 用当前会话参数重新订阅视频，可顺带切换编码。
///
/// 会先退订旧流再订阅新流；`video.sub.ok` 到达前宽高清零，在途的旧帧
/// 因为尺寸不符会被丢弃，界面不会闪一下旧编码的画面。
async fn subscribe_video(
    shared: &Arc<Shared>,
    control: &ControlSession,
    codec: VideoCodec,
) -> Result<(), String> {
    let _serial = shared.video_sub.lock().await;
    // 等到锁之后重新读一次会话：排队期间界面可能刚改过画质参数，
    // 用调用方手上的旧快照会把它丢了。
    let control = {
        let guard = shared.control.read().await;
        match guard.as_ref() {
            Some(current)
                if current.session == control.session
                    && current.client_token == control.client_token =>
            {
                current.clone()
            }
            _ => control.clone(),
        }
    };
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    if let Some(id) = control.video_id {
        let unsub = Message::VideoUnsub(VideoUnsub { id });
        if !send_tunnel_json(&out, control.session, &unsub) {
            return Err("主控连接已断开".into());
        }
    }
    let max_bitrate = {
        let config = shared.config.read().await;
        bitrate_cap(&config, shared.video_kbps.load(Ordering::Relaxed))
    };
    let subscribe = Message::VideoSub(video_sub_for(&control, codec, max_bitrate));
    if !send_tunnel_json(&out, control.session, &subscribe) {
        return Err("主控连接已断开".into());
    }
    let mut guard = shared.control.write().await;
    if let Some(current) = guard.as_mut() {
        if current.session == control.session && current.client_token == control.client_token {
            current.codec = codec;
            current.video_id = None;
            current.width = 0;
            current.height = 0;
        }
    }
    tracing::info!(
        session = control.session,
        stream = "focus",
        codec = codec_name(codec),
        "已重新订阅视频"
    );
    Ok(())
}

/// 把配置里的码率上限规整成协议字段：`None` / 0 / 越界一律当作不限制。
///
/// `live_kbps` 是自适应实时调下来的目标；两者都有时取更小的那个。
fn bitrate_cap(config: &ClientConfig, live_kbps: u32) -> Option<u32> {
    let cap = config.max_bitrate.filter(|kbps| *kbps > 0);
    let live = (live_kbps > 0).then_some(live_kbps);
    match (cap, live) {
        (Some(cap), Some(live)) => Some(cap.min(live)),
        (cap, live) => cap.or(live),
    }
}

/// 把新的视频订阅参数记进会话状态（握手订阅与重订阅共用）。
async fn record_stream_options(
    shared: &Shared,
    session: u32,
    client_token: u64,
    fps: u16,
    max_width: u32,
    quality: u8,
    display: u32,
) {
    let mut guard = shared.control.write().await;
    if let Some(current) = guard.as_mut() {
        if current.session == session && current.client_token == client_token {
            current.fps = fps;
            current.max_width = max_width;
            current.quality = quality;
            current.display = display;
        }
    }
}

/// 手动选参数即退出自动档，并记住用户要的分辨率上限与画质档位。
fn remember_manual_quality(shared: &Shared, manual: bool, max_width: u32, quality: u8) {
    if !manual {
        return;
    }
    shared.auto_quality.store(false, Ordering::Relaxed);
    // 记住用户要的分辨率上限：之后即使自适应临时降过，也只会回到这一档。
    shared.user_max_width.store(max_width, Ordering::Relaxed);
    let step = QUALITY_LADDER
        .iter()
        .position(|candidate| *candidate == quality)
        .unwrap_or(0);
    shared.quality_step.store(step, Ordering::Relaxed);
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
        control.direct_link.is_some(),
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
            // 路由看的是「挂在链路还是服务器上」，不是设备 ID：
            // 同账号 P2P 的 device_id 仍然是账号里的那台设备，但帧走链路。
            Some(control) if control.session == session => {
                (control.direct_link.is_some(), control.direct_link)
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
    // 局域网直连不提供键鼠注入，Ctrl+Alt+Del 同样拒绝；同账号 P2P 是
    // 完整会话（走的就是中转那套协议），不受这条限制。
    if is_direct_device(&control.device_id) {
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
    // 0 是「自动」：不设目标帧率，由被控端按显示器和链路能力尽量跑快。
    if fps > screenwall_agent::capture::MAX_FPS {
        return Err(format!(
            "帧率超出范围（{fps}，允许 0-{}，0 表示自动）",
            screenwall_agent::capture::MAX_FPS
        ));
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
    // 和握手后的那次订阅串行：两边都要「先看状态再发 video.sub」。
    let _serial = shared.video_sub.lock().await;
    // 拿到锁之后重读一次会话：排队期间可能刚完成握手订阅（peer_caps 到位），
    // 也可能刚有解码报错把编码退回 MJPEG，用过期的快照会发错编码。
    let control = {
        let guard = shared.control.read().await;
        match guard.as_ref() {
            Some(current)
                if current.session == control.session && current.client_token == client_token =>
            {
                current.clone()
            }
            _ => return Err("控制会话已切换，请重新连接".into()),
        }
    };
    // `welcome` 还没到：握手后的那次订阅还没发出去，参数记进会话状态即可，
    // 由它一并带上。这里再发一条只会换来两次订阅（第一次的 `video.sub.ok`
    // 还会指向随后被替换的流）。
    if control.peer_caps.is_none() {
        record_stream_options(
            shared,
            control.session,
            client_token,
            fps,
            max_width,
            quality,
            display,
        )
        .await;
        remember_manual_quality(shared, manual, max_width, quality);
        return Ok(());
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
        codec: control.codec,
        // 重新订阅时不能把光标弄丢。
        cursor: true,
        // 自适应调画质时也要带着码率上限，否则调一档就把上限丢了。
        max_bitrate: bitrate_cap(
            &shared.config.read().await.clone(),
            shared.video_kbps.load(Ordering::Relaxed),
        ),
    });
    // 用户手选了新分辨率：默认码率跟着变了，把自适应临时下调的码率清回默认
    // （新流建立后会补发一次 `video.bitrate kbps=0`）。自适应自己降分辨率时
    // 必须保留当前码率，否则会在拥塞时一下跳回高码率。
    if manual && max_width != control.max_width {
        shared.video_kbps.store(0, Ordering::Relaxed);
    }
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

    remember_manual_quality(shared, manual, max_width, quality);
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
    // 本会话见过的最小 RTT，作为「链路是否开始排队」的基线；换会话就重测。
    let mut min_rtt: Option<u32> = None;

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
            min_rtt = None;
            // 新会话的分辨率上限以当前请求为准；用户重新选过会覆盖它。
            shared
                .user_max_width
                .store(control.max_width, Ordering::Relaxed);
            // 码率自适应从新会话的默认值重新开始。
            shared.video_kbps.store(0, Ordering::Relaxed);
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

        if !shared.auto_quality.load(Ordering::Relaxed) {
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
        // 固定帧率下窗口里几乎没有帧（刚重订阅/断流）时不下判断，那测到的
        // 是空窗不是卡顿；自动档静止桌面本来就出不了几帧，不能这样判。
        if control.fps > 0 && frames < 3 {
            continue;
        }
        // 用户手选的画质不在阶梯上（退出自动档后又手动选了「清晰」），
        // 自适应不能拿它当基准继续下滑，等下一次手动选择或重启自适应。
        if !QUALITY_LADDER.contains(&control.quality) {
            good_windows = 0;
            continue;
        }

        // RTT 是直连 / 打洞 / 中转三条路都测得到的真实路径时延：链路开始
        // 排队时它比帧率更早变化，用它判断拥塞可以少掉一批帧。
        if rtt > 0 {
            min_rtt = Some(min_rtt.map_or(rtt, |best| best.min(rtt)));
        }
        let congested_rtt = match (rtt, min_rtt) {
            (0, _) | (_, None) => false,
            (now, Some(best)) => {
                now as f64 > best as f64 * RTT_CONGESTED_RATIO + RTT_CONGESTED_SLACK_MS
            }
        };
        let clear_rtt = match (rtt, min_rtt) {
            // 拿不到 RTT 时只看帧率，不因此挡住升档。
            (0, _) | (_, None) => true,
            (now, Some(best)) => now as f64 <= best as f64 * RTT_CLEAR_RATIO + RTT_CLEAR_SLACK_MS,
        };
        let target = control.fps as f64;
        let fps_low = target > 0.0 && fps < target * 0.82;
        // 自动帧率没有目标可比，窗口里只要还有帧就算跟上了。
        let fps_ok = if target > 0.0 {
            fps > target * 0.95
        } else {
            frames > 0
        };
        let congested = congested_rtt || fps_low;
        let healthy = !congested && fps_ok && clear_rtt;
        good_windows = if healthy {
            good_windows.saturating_add(1)
        } else {
            0
        };

        let width = if control.max_width > 0 {
            control.max_width
        } else {
            screenwall_agent::capture::DEFAULT_MAX_WIDTH
        };
        // 用户手动选的那一档是天花板；自适应临时降下来的宽度不算。
        let ceiling = shared.user_max_width.load(Ordering::Relaxed);
        let ceiling = if ceiling == 0 { width } else { ceiling };

        // 实时码率优先：H.264 会话里先动码率（只改编码器参数、不重新订阅，
        // 不会造成可见卡顿）；降到下限仍然拥塞才降分辨率，恢复时先升码率，
        // 回到默认值再升分辨率。
        let configured_cap = {
            let config = shared.config.read().await;
            bitrate_cap(&config, 0)
        };
        let default_kbps = default_video_kbps(width, control.fps, configured_cap);
        let live_kbps = shared.video_kbps.load(Ordering::Relaxed);
        let bitrate_live = control.codec == VideoCodec::H264
            && control.video_id.is_some()
            && control
                .peer_caps
                .as_ref()
                .is_some_and(|caps| caps.contains(screenwall_proto::caps::VIDEO_BITRATE));
        let mut bitrate_adjusted = false;
        if bitrate_live {
            let current = if live_kbps == 0 {
                default_kbps
            } else {
                live_kbps.min(default_kbps)
            };
            let at_floor = current <= VIDEO_KBPS_FLOOR;
            let at_default = live_kbps == 0 || current >= default_kbps;
            let next = if congested && !at_floor {
                Some((current * 3 / 4).max(VIDEO_KBPS_FLOOR))
            } else if healthy && good_windows >= UPGRADE_STREAK && !at_default {
                Some((current + current / 8 + 100).min(default_kbps))
            } else {
                None
            };
            if let Some(next) = next {
                shared.video_kbps.store(next, Ordering::Relaxed);
                if let Some(out) = control_out(&shared, control.session).await {
                    let message = Message::VideoBitrate(VideoBitrate {
                        id: control.video_id.unwrap_or_default(),
                        kbps: next,
                    });
                    let _ = send_tunnel_json(&out, control.session, &message);
                }
                tracing::info!(
                    from_kbps = current,
                    to_kbps = next,
                    fps,
                    rtt,
                    congested,
                    "自适应调整视频码率"
                );
                bitrate_adjusted = true;
            }
        }

        let action = if bitrate_adjusted {
            // 这一轮只动了码率，分辨率先不动；码率降到下限后才轮到降分辨率。
            AdaptAction::Hold
        } else {
            adapt_for_codec(
                adapt_decision(
                    congested,
                    healthy,
                    good_windows,
                    control.quality,
                    width,
                    ceiling,
                ),
                control.codec == VideoCodec::H264,
                congested,
                width,
                ceiling,
            )
        };
        let (next_quality, next_width) = match action {
            AdaptAction::Hold => (control.quality, width),
            AdaptAction::Quality(quality) => (quality, width),
            AdaptAction::Width(width) => (control.quality, width),
        };
        if action != AdaptAction::Hold {
            tracing::info!(
                from_quality = control.quality,
                to_quality = next_quality,
                from_width = width,
                to_width = next_width,
                fps,
                rtt,
                congested,
                "自适应调整画面参数"
            );
            if set_stream_options_impl(
                &shared,
                control.client_token,
                control.fps,
                next_width,
                next_quality,
                None,
                false,
            )
            .await
            .is_ok()
            {
                if let Some(step) = QUALITY_LADDER
                    .iter()
                    .position(|candidate| *candidate == next_quality)
                {
                    shared.quality_step.store(step, Ordering::Relaxed);
                }
                cooldown = 1;
            }
            good_windows = 0;
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
            // 实际链路：有链路登记说明不走服务器，再看是不是局域网直连。
            "link": if control.direct_link.is_none() {
                "relay"
            } else if is_direct_device(&control.device_id) {
                "direct"
            } else {
                "p2p"
            },
            // 当前订阅的视频编码（`mjpeg` / `h264`）。
            "codec": codec_name(control.codec),
            // 握手已完成才带；`null` 表示能力未知，前端沿用旧的兜底逻辑。
            "caps": control
                .peer_caps
                .as_ref()
                .map(|caps| caps.iter().collect::<Vec<_>>()),
        }),
        None => Value::Null,
    })
}

/// 界面探测 `window.VideoDecoder` 之后告诉后端本机能不能解 H.264。
///
/// 必须在 `start_control` 之前调用：握手里的能力声明要用到它。探测不到
/// 就传 `false`，后端行为与加入 H.264 之前完全一致（只订 MJPEG）。
#[tauri::command]
fn set_video_decoder(shared: State<'_, Arc<Shared>>, available: bool) {
    shared.h264_decode.store(available, Ordering::Relaxed);
    tracing::info!(available, "WebCodecs H.264 解码能力");
}

/// 控制页可见性变化时调用：页面被切走就暂停视频帧推送，回来再恢复。
///
/// 保活的页面只是 `display:none`，会话和订阅都还活着。不暂停的话，4K 画面
/// 的 base64 编码、跨进程传输和 WebView 主线程解析会一直占着资源，用户
/// 在别的页面点任何东西都要排队等帧处理完。
#[tauri::command]
fn set_control_video_muted(shared: State<'_, Arc<Shared>>, muted: bool) {
    if shared.video_muted.swap(muted, Ordering::Relaxed) != muted {
        tracing::debug!(muted, "控制页视频帧推送");
    }
}

/// 解码失败（或用户手动关掉）时停用 H.264：本次运行不再尝试，当前会话改订 MJPEG。
///
/// 「记住」是刻意的：解码器报错往往意味着这台机器的 WebCodecs 与硬件编码器
/// 的组合不可用，反复重试只会让画面在两种编码之间来回闪。
#[tauri::command]
async fn disable_h264_video(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<(), String> {
    let shared = shared.inner().clone();
    let control = {
        let mut guard = shared.control.write().await;
        let current = guard
            .as_mut()
            .ok_or_else(|| "控制会话尚未建立".to_string())?;
        if current.client_token != client_token {
            return Err("控制会话已切换，请重新连接".into());
        }
        // 会话核对通过之后才记「本次运行不再用 H.264」：迟到的报错来自上一次
        // 会话时，不该把新会话也一起拖成 MJPEG。
        shared.h264_fallback.store(true, Ordering::Relaxed);
        if current.codec == VideoCodec::Mjpeg {
            return Ok(());
        }
        // 先落下编码再发消息：并发的多次解码错误只会有一次真正重订。
        // `video_id` 留给 `subscribe_video` 退订旧流。
        current.codec = VideoCodec::Mjpeg;
        current.clone()
    };
    tracing::warn!("界面停用 H.264，回退 MJPEG");
    subscribe_video(&shared, &control, VideoCodec::Mjpeg).await
}

/// 请求被控端立刻产生一个关键帧（解码器丢帧、重配之后调用）。
#[tauri::command]
async fn request_video_keyframe(
    shared: State<'_, Arc<Shared>>,
    client_token: u64,
) -> Result<(), String> {
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
    let Some(id) = control.video_id else {
        return Ok(());
    };
    let out = control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    if !send_tunnel_json(
        &out,
        control.session,
        &Message::VideoKeyframe(VideoKeyframe { id }),
    ) {
        return Err("主控连接已断开".into());
    }
    tracing::debug!(session = control.session, stream = id, "已请求关键帧");
    Ok(())
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
        "rememberAuthorized": config.remember_authorized,
        "uiZoom": config.ui_zoom,
        "closeToTray": config.close_to_tray,
        "caPath": config.ca_path,
        "virtualDisplay": config.virtual_display,
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

/// 无头机器上临时接入虚拟显示器（usbmmidd_v2）。切换即时生效。
#[tauri::command]
async fn set_virtual_display(
    shared: State<'_, Arc<Shared>>,
    enabled: bool,
) -> Result<bool, String> {
    screenwall_agent::virtual_display::set_enabled(enabled);
    let mut config = shared.config.write().await;
    if config.virtual_display != enabled {
        config.virtual_display = enabled;
        config
            .save()
            .map_err(|error| format!("保存设置失败: {error}"))?;
    }
    tracing::info!("无头虚拟显示器: {enabled}");
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

/// 已授权过的设备再次连接时是否免确认。
#[tauri::command]
async fn set_remember_authorized(
    shared: State<'_, Arc<Shared>>,
    enabled: bool,
) -> Result<bool, String> {
    shared.remember_authorized.store(enabled, Ordering::Relaxed);
    let mut config = shared.config.write().await;
    if config.remember_authorized != enabled {
        config.remember_authorized = enabled;
        config
            .save()
            .map_err(|error| format!("保存设置失败: {error}"))?;
    }
    tracing::info!("已授权设备免确认: {enabled}");
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

/// 把窗口四角切成 Windows 11 的圆角（`DWMWA_WINDOW_CORNER_PREFERENCE`）。
///
/// `decorations: false` 的无边框窗口默认是直角；KayPanel 是圆角，我们
/// 也切成圆角保持一致。`hwnd` 是主窗口的原生句柄（isize）。
/// 只在 Windows 上有效，非 Windows 直接忽略。
#[cfg(windows)]
fn apply_round_corners(hwnd: isize) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    // SAFETY: DwmSetWindowAttribute 只在该窗口句柄仍有效时调用；
    // app.setup 里窗口刚建好，#[cfg(windows)] 下 hwnd 一定有效。
    unsafe {
        let pref: u32 = DWMWCP_ROUND as u32;
        // 后两个参数是属性值指针与大小。值为枚举，这里转成原生类型。
        let _ = DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &pref as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn apply_round_corners(_hwnd: isize) {}

pub(crate) fn emit_control_state(
    app: &AppHandle,
    phase: &str,
    control: &ControlSession,
    message: Option<String>,
) {
    let link = if control.direct_link.is_none() {
        "relay"
    } else if is_direct_device(&control.device_id) {
        "direct"
    } else {
        "p2p"
    };
    let event = ControlStateEvent {
        phase: phase.to_string(),
        device_id: control.device_id.clone(),
        session: control.session,
        client_token: control.client_token,
        link,
        message,
        width: (control.width > 0).then_some(control.width),
        height: (control.height > 0).then_some(control.height),
        fps: (control.fps > 0).then_some(control.fps),
        codec: (control.width > 0).then_some(codec_name(control.codec).to_string()),
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

    // 本机模式：不碰任何服务器接口，只跑局域网直连监听。
    //
    // view/host 两条循环在没有 token 时会立刻 return（见各自开头），
    // 但显式分开更清楚：本机模式下连「尝试连服务器」这个动作都不该有。
    if shared
        .config
        .try_read()
        .is_ok_and(|config| config.local_mode)
    {
        let direct_shared = shared.clone();
        let direct_app = app.clone();
        let direct_task = tauri::async_runtime::spawn(async move {
            direct::listen_loop(direct_app, direct_shared).await;
        });
        let adapt_shared = shared.clone();
        let adapt_app = app.clone();
        let adapt = tauri::async_runtime::spawn(async move {
            adapt_loop(adapt_app, adapt_shared).await;
        });
        let mut tasks = shared.tasks.lock().unwrap();
        tasks.direct = Some(direct_task);
        tasks.adapt = Some(adapt);
        return;
    }

    // 连接码模式只做「主控端」：绝不能启动 host 任务，否则会拿受限令牌
    // 去连 /ws/device，把目标设备的在线连接顶掉。
    if shared.quick_mode.load(Ordering::Relaxed) {
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
        let mut tasks = shared.tasks.lock().unwrap();
        tasks.view = Some(view);
        tasks.adapt = Some(adapt);
        return;
    }

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
    // 关掉 Nagle：同一条 WebSocket 上既有视频帧也有键鼠/心跳，Nagle 会
    // 把小包压到有 ACK 才发，交互延迟最坏多出 40ms。
    let (ws, _) = match connector {
        Some(connector) => {
            tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector))
                .await
        }
        None => tokio_tungstenite::connect_async_with_config(request, None, true).await,
    }
    .context("无法连接服务器")?;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ViewOut>();
    *shared.view_tx.write().await = Some(out_tx);
    *shared.connected.write().await = true;
    *shared.error.write().await = None;
    // 问一下服务器版本：低于 0.2.11 的服务器不认识 `p2p` 信令，也不发
    // `sameAccount`。客户端据此跳过打洞，别让用户每次点连接都白等一轮超时。
    {
        let http = shared.http.clone();
        let probe = shared.clone();
        let probe_app = app.clone();
        let base = config.server_url.trim_end_matches('/').to_string();
        tokio::spawn(async move {
            let response = match http
                .get(format!("{base}/healthz"))
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => response,
                _ => return,
            };
            let Ok(body) = response.json::<Value>().await else {
                return;
            };
            if let Some(version) = body.get("version").and_then(Value::as_str) {
                *probe.server_version.lock().unwrap() = Some(version.to_string());
                // 让侧栏/设置页及时看到服务器版本，而不是等下次重连。
                let _ = probe_app.emit("account", account_view(&probe).await);
            }
        });
    }
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
        "p2p" => {
            // 被控端对打洞请求的应答（或拒绝）：唤醒正在等它的升级任务。
            // 等不到就走了中转，所以这里只需要把内容交出去。
            let request = value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
            if request == 0 {
                return;
            }
            let waiter = shared.p2p_waits.lock().unwrap().remove(&request);
            if let Some(waiter) = waiter {
                let _ = waiter.send(value.clone());
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
            // 这条 `closed` 来自服务器连接，只可能关掉中转会话：直连/P2P 会话号
            // 由对方按连接独立分配，和服务器会话号会撞车，不能按号误收。
            let control = {
                let mut guard = shared.control.write().await;
                match guard.as_ref() {
                    Some(current)
                        if current.session == session && current.direct_link.is_none() =>
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

    #[test]
    fn adaptive_walks_quality_before_touching_resolution() {
        // 拥塞：先降画质；画质到底才降分辨率；都到底就保持。
        assert_eq!(
            adapt_decision(true, false, 0, 75, 3840, 3840),
            AdaptAction::Quality(65)
        );
        assert_eq!(
            adapt_decision(true, false, 0, 42, 3840, 3840),
            AdaptAction::Width(2560)
        );
        assert_eq!(
            adapt_decision(true, false, 0, 42, 1280, 3840),
            AdaptAction::Hold
        );
    }

    #[test]
    fn adaptive_upgrades_only_after_a_long_healthy_streak() {
        // 健康窗口不够多：保持不动，避免在临界点来回抖。
        assert_eq!(
            adapt_decision(false, true, UPGRADE_STREAK - 1, 42, 1280, 3840),
            AdaptAction::Hold
        );
        // 够长：先把画质升回去。
        assert_eq!(
            adapt_decision(false, true, UPGRADE_STREAK, 42, 1280, 3840),
            AdaptAction::Quality(48)
        );
        // 画质已经最高：再升分辨率，但不越过用户选的上限。
        assert_eq!(
            adapt_decision(false, true, UPGRADE_STREAK, 75, 1280, 3840),
            AdaptAction::Width(1920)
        );
        assert_eq!(
            adapt_decision(false, true, UPGRADE_STREAK, 75, 1920, 1920),
            AdaptAction::Hold
        );
    }

    #[test]
    fn h264_adaptation_skips_the_jpeg_quality_ladder() {
        // H.264：拥塞时直接降分辨率，不再发「改 quality」的重订阅。
        assert_eq!(
            adapt_for_codec(AdaptAction::Quality(55), true, true, 1920, 1920),
            AdaptAction::Width(1280)
        );
        // 恢复时直接升分辨率（画质档对 H.264 没有意义）。
        assert_eq!(
            adapt_for_codec(AdaptAction::Quality(75), true, false, 1280, 1920),
            AdaptAction::Width(1920)
        );
        // 已经到边界就什么都不做。
        assert_eq!(
            adapt_for_codec(AdaptAction::Quality(42), true, true, 1280, 1920),
            AdaptAction::Hold
        );
        assert_eq!(
            adapt_for_codec(AdaptAction::Quality(75), true, false, 1920, 1920),
            AdaptAction::Hold
        );
        // MJPEG 保持原来的阶梯行为。
        assert_eq!(
            adapt_for_codec(AdaptAction::Quality(55), false, true, 1920, 1920),
            AdaptAction::Quality(55)
        );
    }

    #[test]
    fn live_bitrate_defaults_match_the_encoder() {
        // 与采集端 `h264_bitrate` 同一口径：1080p60 8 Mbps、1080p30 4 Mbps。
        assert_eq!(default_video_kbps(1920, 60, None), 8_000);
        assert_eq!(default_video_kbps(1920, 30, None), 4_000);
        assert_eq!(default_video_kbps(1280, 60, None), 5_000);
        assert_eq!(default_video_kbps(3840, 60, None), 25_000);
        // 自动帧率（0）按 60 算；用户设了上限就以更小的为准。
        assert_eq!(default_video_kbps(1920, 0, None), 8_000);
        assert_eq!(default_video_kbps(1920, 60, Some(3_000)), 3_000);
        // 实时下调的码率与配置上限取更小，不会互相覆盖。
        let config = ClientConfig::default();
        assert_eq!(bitrate_cap(&config, 0), None);
        assert_eq!(bitrate_cap(&config, 4_500), Some(4_500));
        let capped = ClientConfig {
            max_bitrate: Some(3_000),
            ..config
        };
        assert_eq!(bitrate_cap(&capped, 4_500), Some(3_000));
        assert_eq!(bitrate_cap(&capped, 0), Some(3_000));
    }

    #[test]
    fn adaptive_ladders_are_monotonic() {
        assert_eq!(quality_up(42), Some(48));
        assert_eq!(quality_up(75), None);
        assert_eq!(quality_down(75), Some(65));
        assert_eq!(quality_down(42), None);
        assert_eq!(width_up(3840, 3840), None);
        assert_eq!(width_down(1280), None);
        // 非阶梯宽度（手改过的配置）也能收敛到阶梯上。
        assert_eq!(width_down(1600), Some(1280));
        assert_eq!(width_up(1600, 1920), Some(1920));
    }

    fn update_info(version: &str) -> update::UpdateInfo {
        update::UpdateInfo {
            current: "0.2.3".into(),
            version: version.into(),
            notes: String::new(),
            url: format!("https://example.com/{version}.exe"),
            sha256: "a".repeat(64),
            published_at: 0,
        }
    }

    fn newer(version: &str) -> update::SourceCheck {
        update::SourceCheck::Newer(update_info(version))
    }

    #[test]
    fn update_takes_newer_of_the_two_sources() {
        // 服务器还是旧版、GitHub 已发新版：必须报 GitHub 那个，
        // 否则用户永远停在服务器滞后时的那一版。
        let merged =
            merge_update_sources(Ok(newer("0.2.4")), Ok(newer("0.2.5"))).expect("两边都成功");
        assert_eq!(merged.map(|info| info.version).as_deref(), Some("0.2.5"));

        // 反过来也一样。
        let merged =
            merge_update_sources(Ok(newer("0.2.6")), Ok(newer("0.2.5"))).expect("两边都成功");
        assert_eq!(merged.map(|info| info.version).as_deref(), Some("0.2.6"));
    }

    #[test]
    fn update_survives_one_source_being_down() {
        // 纯内网：GitHub 不可达，服务器说有新版。
        let merged = merge_update_sources(Ok(newer("0.2.5")), Err("网络不通".into()))
            .expect("一边失败不算错");
        assert_eq!(merged.map(|info| info.version).as_deref(), Some("0.2.5"));

        // 服务器挂了，GitHub 说有新版。
        let merged = merge_update_sources(Err("服务器超时".into()), Ok(newer("0.2.5")))
            .expect("一边失败不算错");
        assert_eq!(merged.map(|info| info.version).as_deref(), Some("0.2.5"));

        // 服务器明确说已是最新、GitHub 连不上：按已是最新处理，不弹错。
        assert!(
            merge_update_sources(Ok(update::SourceCheck::Latest), Err("网络不通".into()))
                .expect("一边成功")
                .is_none()
        );
    }

    #[test]
    fn update_reports_error_only_when_both_fail() {
        let err = merge_update_sources(Err("服务器超时".into()), Err("GitHub 不可达".into()))
            .expect_err("两边都失败应报错");
        assert!(err.contains("服务器超时"), "应保留服务器侧的原始原因");
        // 两个原因都要出现，否则用户只看到服务器那条，会误判成服务器的问题。
        assert!(err.contains("GitHub 不可达"), "应同时带上 GitHub 侧的原因");
    }

    /// 服务器没配 `[update]`（404）时不能当成「已是最新」：
    /// 否则 GitHub 那一路哪怕挂了也显示已是最新，用户永远收不到新版本。
    #[test]
    fn unknown_source_does_not_hide_the_other_failure() {
        let err = merge_update_sources(
            Ok(update::SourceCheck::Unknown),
            Err("检查更新失败：无法访问 GitHub（HTTP 403）".into()),
        )
        .expect_err("没有源给出结论时不能假装已是最新");
        assert!(err.contains("GitHub"), "应带上 GitHub 的失败原因：{err}");

        // 一源没有结论、另一源确实有新版：照常报更新。
        let merged = merge_update_sources(Ok(update::SourceCheck::Unknown), Ok(newer("0.2.11")))
            .expect("有结论的源说了算");
        assert_eq!(merged.map(|info| info.version).as_deref(), Some("0.2.11"));

        // 两源都没有结论：真正无从判断，按已是最新处理。
        assert!(merge_update_sources(
            Ok(update::SourceCheck::Unknown),
            Ok(update::SourceCheck::Unknown)
        )
        .expect("两边都没结论")
        .is_none());
    }

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
            codec: VideoCodec::Mjpeg,
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
                            // 退订和重新订阅之间会有一段在途消息：编码与当前请求
                            // 不符的 `video.sub.ok` 来自上一次订阅，认它会把
                            // `video_id` 指向已经退掉的流。
                            if current.session == header.session && current.codec == ok.actual.codec
                            {
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
                            codec = codec_name(ok.actual.codec),
                            "收到画面订阅确认"
                        );
                        emit_control_state(app, "streaming", &current, None);
                        // 换了分辨率后默认码率也变了：让被控端把实时码率清回
                        // 默认值。不带上这一步的话，上一次自适应留下的低码率
                        // 会被继续套用到新分辨率的编码器上。
                        let supports_bitrate = current.codec == VideoCodec::H264
                            && current.peer_caps.as_ref().is_some_and(|caps| {
                                caps.contains(screenwall_proto::caps::VIDEO_BITRATE)
                            });
                        if supports_bitrate
                            && shared.video_kbps.load(Ordering::Relaxed) == 0
                        {
                            if let Some(out) = control_out(shared, current.session).await {
                                let reset = Message::VideoBitrate(VideoBitrate {
                                    id: ok.id,
                                    kbps: 0,
                                });
                                let _ = send_tunnel_json(&out, current.session, &reset);
                            }
                        }
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
                    } else if error.code == screenwall_proto::codes::ENCODER_UNAVAILABLE
                        && control.codec == VideoCodec::H264
                    {
                        // 当前订阅的是 H.264 却被拒（编码器初始化失败、硬件被占用），
                        // 不是会话错误：记住这次回退，本次运行不再请求 H.264，
                        // 原地改订 MJPEG 把画面接上。
                        shared.h264_fallback.store(true, Ordering::Relaxed);
                        tracing::warn!(msg = %error.msg, "H.264 编码不可用，回退 MJPEG");
                        if let Err(message) =
                            subscribe_video(shared, &control, VideoCodec::Mjpeg).await
                        {
                            emit_control_state(app, "error", &control, Some(message));
                        }
                    } else {
                        tracing::warn!(code = error.code, msg = %error.msg, "被控端返回错误");
                        emit_control_state(app, "error", &control, Some(text));
                    }
                }
                Message::Welcome(welcome) => {
                    // 记录双方交集：界面据此直接判断可选功能（如被控端音量），
                    // 不必先发一条消息等 `UNKNOWN_MESSAGE` 回来才知道不支持。
                    // 本机运行时能力（WebCodecs 可用时才有 `video.h264`）也要算进去。
                    let caps =
                        Capabilities::with_peer_and(welcome.caps.clone(), local_caps(shared));
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
                                device_id: current.device_id.clone(),
                                session: current.session,
                                client_token: current.client_token,
                                caps: caps.iter().map(str::to_string).collect(),
                            },
                        );
                        // 握手完成才知道该用哪种编码：双方都声明了 `video.h264`
                        // 才请求 H.264，否则保持 MJPEG（行为与以前完全一致）。
                        let codec = if caps.contains(screenwall_proto::caps::VIDEO_H264) {
                            VideoCodec::H264
                        } else {
                            VideoCodec::Mjpeg
                        };
                        if let Err(message) = subscribe_video(shared, &current, codec).await {
                            emit_control_state(app, "error", &current, Some(message));
                        }
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
            let payload = body;
            if frame.kind != FrameType::Video {
                return;
            }
            if !matches!(frame.codec, FrameCodec::Jpeg | FrameCodec::H264) {
                return;
            }
            if control.width == 0 || control.height == 0 {
                return;
            }
            // 单帧过大通常是异常数据，直接丢弃，避免把 WebView 拖死。
            if payload.len() > 2 * 1024 * 1024 {
                return;
            }
            {
                let mut stats = shared.stats.lock().unwrap();
                stats.frames += 1;
                stats.bytes += payload.len() as u64;
            }
            // 真实帧头（stream/seq/到达时刻）喂给质量统计器。
            shared.quality.lock().unwrap().on_video(
                frame.stream_id,
                frame.seq,
                payload.len() as u32,
                Instant::now(),
            );
            // 控制页被切走（保活在 DOM 里但不可见）时不往 WebView 送帧：
            // 4K MJPEG 的 base64 编码和事件解析都在主线程上，隐藏页面继续
            // 收帧会让整窗点击卡上好几秒。统计照常累加，回页后立刻恢复。
            if shared.video_muted.load(Ordering::Relaxed) {
                return;
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
            let h264 = frame.codec == FrameCodec::H264;
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
                bytes: payload.len(),
                codec: if h264 {
                    "h264".to_string()
                } else {
                    "jpeg".to_string()
                },
                keyframe: frame.is_keyframe(),
                data_url: if h264 {
                    String::new()
                } else {
                    format!("data:image/jpeg;base64,{encoded}")
                },
                data: if h264 { encoded } else { String::new() },
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

    // 直连 / P2P 会话不经过服务器：服务器连接断了不该把它一起收掉，
    // 它的链路自己会走收尾逻辑。
    let control = {
        let mut guard = shared.control.write().await;
        match guard.as_ref() {
            Some(current) if current.direct_link.is_some() => None,
            _ => guard.take(),
        }
    };
    if let Some(control) = control {
        end_session(app, shared, control, "主控连接已断开").await;
    }
}

/// 被控端授权请求在 Agent 侧的等待上限，和 UI 倒计时保持一致。
const APPROVAL_TIMEOUT_SECS: u64 = 30;

/// 被控端的授权回调（中转会话与同账号 P2P 共用一份）。
///
/// 「允许被控」总开关优先；同账号会话直接放行；其余弹窗等人点。
pub(crate) fn host_approval(app: AppHandle, shared: Arc<Shared>) -> ApprovalFn {
    Arc::new(move |request: ApprovalRequest| {
        let ApprovalRequest {
            session,
            viewer,
            same_account,
            // 中转 / 同账号 P2P 不走长期 token，没有「已授权过」这回事。
            authorized_before: _,
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
        // 同一账号的设备免确认：主控端登录的就是本机这个账号，等于账号
        // 主人自己操作，没必要每台机器点一次「允许控制」。直连和连接码
        // 会话不带 sameAccount，仍然逐次弹窗。
        if same_account {
            tracing::info!("同账号 {viewer} 请求控制，直接允许（会话 {session}）");
            let _ = reply.send(true);
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
}

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
            virtual_display: config.virtual_display,
        };
        let approval = host_approval(app.clone(), shared.clone());
        let state = Arc::new(AgentState {
            config: agent_config,
            displays: display::list_displays(),
            approval: Some(approval),
        });
        let Some(options) = CloudOptions::from_state(&state) else {
            return;
        };
        // 同账号会话优先尝试 P2P 直连（被打洞失败挡回来时主控端会走中转）。
        let options = CloudOptions {
            p2p: Some(Arc::new(p2p::P2pHost::new(app.clone(), shared.clone()))),
            ..options
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
    let quick = shared.quick_mode.load(Ordering::Relaxed);
    AccountView {
        server_url: config.server_url.clone(),
        username: if quick {
            "连接码会话".to_string()
        } else {
            config.username.clone()
        },
        device_id: config.device_id.clone(),
        logged_in: !config.token.is_empty() || config.local_mode,
        connected: if config.local_mode {
            // 本机模式没有服务器可连：直连监听起来就算「就绪」。
            shared.direct.lock().unwrap().listening
        } else {
            *shared.connected.read().await
        },
        error: shared.error.read().await.clone(),
        quick,
        local: config.local_mode,
        server_version: shared.server_version.lock().unwrap().clone(),
    }
}

async fn emit_devices(app: &AppHandle, shared: &Shared) {
    let devices = shared.devices.read().await.clone();
    let _ = app.emit("devices", devices);
}

/// 本机模式下重新计算设备列表并推给前端。
pub(crate) async fn refresh_local_devices(app: &AppHandle, shared: &Arc<Shared>) {
    let config = shared.config.read().await.clone();
    *shared.devices.write().await = local_devices(&config);
    emit_devices(app, shared).await;
}

/// 本机模式下把「已配对的直连设备」摊成设备列表。
///
/// 没有服务器就没有设备目录，屏幕墙/设备页能显示的就是这些配对过的机器。
/// 这里把 `online` 标成 true：它们在同一网段，实际连不上时屏幕墙的缩略图
/// 会话会自己报错并退避重试，不会卡住界面。
fn local_devices(config: &ClientConfig) -> Vec<DeviceView> {
    config
        .known_peers
        .iter()
        .map(|peer| DeviceView {
            id: format!("direct:{}", peer.address),
            name: if peer.name.is_empty() {
                peer.address.clone()
            } else {
                peer.name.clone()
            },
            os: "局域网直连".to_string(),
            version: String::new(),
            online: true,
            last_seen: chrono::Utc::now().timestamp_millis(),
            controllable: true,
        })
        .collect()
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

/// 检查是否有新版本：同时问自建服务器和 GitHub Releases，取较新的那个。
///
/// 为什么不能「服务器说有就没有」：自建服务器上的更新包要手动发布
/// （`scripts/publish-update.ps1`）并重启才生效，只要漏了这一步，服务器
/// 就会一直回「已是最新」，用户明明在 GitHub 上看到发了新版却更新不了。
/// 反过来，纯内网环境连不上 GitHub，也不能因此报错 —— 两边任一给出
/// 好消息就算数。
///
/// 两个请求并发发出，总耗时取较慢的那条，不会叠加；GitHub 不可达时
/// 只是少一个信息源。
#[tauri::command]
async fn check_update(
    shared: State<'_, Arc<Shared>>,
) -> Result<Option<update::UpdateInfo>, String> {
    let shared = shared.inner().clone();
    let base = {
        let config = shared.config.read().await;
        config.server_url.clone()
    };

    if base.is_empty() {
        // 局域网模式没有自建服务器，只依赖 GitHub。这里必须把 GitHub 的失败
        // 如实返回，不能静默成「已是最新」——否则国内网络下 GitHub 一超时，
        // 用户就会一直以为没有新版本（0.2.5 检测不到 0.2.6 就是这个原因）。
        return Ok(match update::check_update_github(&shared.http).await? {
            update::SourceCheck::Newer(info) => Some(info),
            update::SourceCheck::Latest | update::SourceCheck::Unknown => None,
        });
    }
    let (server, github) = tokio::join!(
        update::check_update(&shared.http, &base),
        update::check_update_github(&shared.http)
    );
    merge_update_sources(server, github)
}

/// 合并「自建服务器」和「GitHub」两个信息源的结论。
///
/// 抽成纯函数是为了能测：这里的分支决定了用户到底会不会看到更新，
/// 出错的代价是「明明有新版却一直显示已是最新」。
fn merge_update_sources(
    server: Result<update::SourceCheck, String>,
    github: Result<update::SourceCheck, String>,
) -> Result<Option<update::UpdateInfo>, String> {
    let newest = |a: update::UpdateInfo, b: update::UpdateInfo| {
        if update::version_gt(&b.version, &a.version) {
            b
        } else {
            a
        }
    };

    // 任一源给出新版本就算数，取版本号最大的那个。
    let mut found: Option<update::UpdateInfo> = None;
    let mut saw_latest = false;
    let mut saw_unknown = false;
    let mut errors: Vec<String> = Vec::new();
    for result in [server, github] {
        match result {
            Ok(update::SourceCheck::Newer(info)) => {
                found = Some(match found {
                    Some(current) => newest(current, info),
                    None => info,
                });
            }
            Ok(update::SourceCheck::Latest) => saw_latest = true,
            Ok(update::SourceCheck::Unknown) => saw_unknown = true,
            Err(error) => errors.push(error),
        }
    }

    if let Some(info) = found {
        return Ok(Some(info));
    }
    // 有一源明确说「已是最新」，另一源失败或没结论：按已是最新处理，不打扰
    // 用户（内网环境 GitHub 永远连不上是常态）。
    if saw_latest {
        return Ok(None);
    }
    if !errors.is_empty() {
        // 没有任何源给出「已是最新」的结论，且至少一源报错：如实报出来。
        // 这里若再返回 None，就回到「服务器没配更新 + GitHub 被拦 → 假装已是最新」
        // 的老毛病（0.2.11 之前就是这样，用户永远收不到新版本）。
        let mut all = errors;
        if saw_unknown {
            all.push("另一来源没有提供版本信息".into());
        }
        return Err(all.join("；"));
    }
    // 两个源都「没有结论」（服务器未配置更新、GitHub 仓库没有 Release）：
    // 这是真正无从判断，继续按已是最新处理。
    Ok(None)
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
    // 无头机器装/插 usbmmidd 虚拟显示器：同样是提权重跑自身，
    // 由被控端在会话开始前通过 ShellExecuteExW(runas) 拉起。
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = screenwall_agent::virtual_display::run_helper_from_args(&args) {
        std::process::exit(code);
    }
    init_tracing();
    let mut config = ClientConfig::load();
    screenwall_agent::virtual_display::set_enabled(config.virtual_display);
    let mut changed = false;
    // 旧版本 client.json 里的 token 是 Agent 的共享 Token，不是账号令牌。
    if config.username.is_empty() && !config.token.is_empty() {
        config.token.clear();
        changed = true;
    }
    // 识别码跟着安装走：首次启动生成，之后固定不变，别人记住一次就能一直用。
    if config.local_id.trim().is_empty() {
        config.local_id = direct::generate_local_id();
        changed = true;
    }
    if changed {
        let _ = config.save();
    }
    let start_now =
        config.local_mode || (!config.token.is_empty() && !config.server_url.is_empty());

    // 通过 screenwall:// 协议启动时，命令行里带着链接。
    let startup_link = args
        .iter()
        .skip(1)
        .find(|arg| arg.starts_with("screenwall://"))
        .cloned();

    let allow_control = config.allow_control;
    let remember_authorized = config.remember_authorized;
    let ui_zoom = config.ui_zoom;
    let clipboard_on = config.clipboard_sync;
    // 本机模式重启后直接进工作台，不需要再点一次「不用服务器」。
    let boot_local_devices = if config.local_mode {
        local_devices(&config)
    } else {
        Vec::new()
    };
    let shared = Arc::new(Shared {
        config: RwLock::new(config),
        devices: RwLock::new(boot_local_devices),
        connected: RwLock::new(false),
        error: RwLock::new(None),
        tasks: StdMutex::new(Tasks::default()),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            // GitHub 的 REST API 会拒绝没有 User-Agent 的请求（403
            // "Request forbidden by administrative rules"），而 reqwest
            // 默认不发 UA。没有这一行，「检查更新」的 GitHub 那一路永远失败。
            .user_agent(concat!("ScreenWall/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP 客户端"),
        view_tx: RwLock::new(None),
        control: RwLock::new(None),
        video_sub: tokio::sync::Mutex::new(()),
        pending_opens: StdMutex::new(HashMap::new()),
        p2p_waits: StdMutex::new(HashMap::new()),
        p2p_busy: AtomicBool::new(false),
        pending_wake: StdMutex::new(None),
        next_request: AtomicU32::new(0),
        stats: StdMutex::new(NetStats::default()),
        auto_quality: AtomicBool::new(true),
        quality_step: AtomicUsize::new(0),
        video_kbps: AtomicU32::new(0),
        user_max_width: AtomicU32::new(0),
        rtt_ms: AtomicU32::new(0),
        ping_sent: StdMutex::new(None),
        pending_control_requests: StdMutex::new(HashMap::new()),
        allow_control: AtomicBool::new(allow_control),
        remember_authorized: AtomicBool::new(remember_authorized),
        files: StdMutex::new(files::Hub::default()),
        clipboard_on: AtomicBool::new(clipboard_on),
        wall: StdMutex::new(wall::Hub::default()),
        group: StdMutex::new(group::Hub::default()),
        next_group: AtomicU64::new(0),
        quality: StdMutex::new(quality::Tracker::new(Instant::now(), now_ms())),
        video_muted: AtomicBool::new(false),
        server_version: StdMutex::new(None),
        h264_decode: AtomicBool::new(false),
        h264_fallback: AtomicBool::new(false),
        reconnect: StdMutex::new(None),
        pending_resume: StdMutex::new(None),
        control_wanted: AtomicBool::new(false),
        direct: StdMutex::new(direct::DirectHub::default()),
        quick_mode: AtomicBool::new(false),
        pending_link: StdMutex::new(startup_link.and_then(|raw| parse_connect_link(&raw))),
    });

    let initial = shared.clone();
    let panels = Arc::new(panels::Hub::default());
    let panel_state = panels.clone();
    tauri::Builder::default()
        // 单实例：第二次启动（点链接唤起）把命令行转发给已有窗口，自己退出。
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(link) = argv.iter().find(|arg| arg.starts_with("screenwall://")) {
                dispatch_connect_link(app, link);
            } else if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(shared)
        .manage(panels)
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_zoom(ui_zoom);
                // Windows 11 把窗口四角切成圆角，和 KayPanel 的视觉对齐。
                // decorations:false 的窗口默认是直角，需要显式开 DWM 圆角。
                if let Ok(hwnd) = window.hwnd() {
                    apply_round_corners(hwnd.0 as isize);
                }
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
            enter_local_mode,
            quick_connect,
            quick_disconnect,
            create_connect_link,
            connect_by_link,
            take_connect_link,
            get_connect_code,
            set_connect_code,
            rotate_connect_code,
            start_control,
            stop_control,
            send_control_input,
            set_stream_options,
            set_control_video_muted,
            set_video_decoder,
            disable_h264_video,
            request_video_keyframe,
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
            set_remember_authorized,
            set_close_to_tray,
            set_virtual_display,
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
            direct::resolve_direct_id,
            set_ca_path,
            firewall::get_direct_firewall_state,
            firewall::allow_direct_firewall,
            panels::get_panel_status,
            panels::prewarm_panel,
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
