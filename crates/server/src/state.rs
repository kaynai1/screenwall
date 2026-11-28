//! 服务器运行状态：在线设备、主控连接与会话表。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, RwLock};

use crate::auth::now_ms;
use crate::config::{Config, LimitsConfig};
use crate::db::Db;
use crate::metrics::Metrics;
use crate::ratelimit::{LoginLimiter, RateLimiter};
use crate::update::UpdateState;

#[derive(Debug, Clone)]
pub enum WsOut {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

#[derive(Debug, Clone)]
pub struct DeviceConn {
    pub user_id: i64,
    pub device_id: String,
    pub tx: mpsc::UnboundedSender<WsOut>,
    /// 被控端支持网页端 WebRTC P2P（连接时带 `webrtc=1`）。
    pub webrtc: bool,
}

/// 主控端待发的最新画面帧。
///
/// 被控端出帧的速度往往远高于主控端链路的带宽。全塞进无界队列会让画面越积
/// 越旧：延迟无限增长、服务端内存也跟着涨，连 `video.sub.ok` / `pong` 这些
/// 控制消息都被压在后面。这里给每个主控端留一个很短的队列，写线程取走就发：
///
/// - MJPEG 每帧独立，队列里只留最新一帧，来一帧覆盖一帧；
/// - H.264 有关键帧依赖，丢帧后解码链就断了，所以整段丢弃并等下一个关键帧；
/// - 音频等其他二进制帧不进这个队列（丢音频比晚一点更糟），仍走控制通道。
///
/// 只留最新一帧还不够：写线程把帧交给内核缓冲后立刻就能发下一帧，慢链路上
/// 内核 + 中间设备的缓冲会攒出好几秒的延迟。网页端每收到一帧回一条
/// `video.ack`，这里按确认的往返时间做拥塞控制：链路有余量就多放几帧在途
/// 把带宽跑满，延迟涨上来就少放几帧把队列排空。延迟因此有上限，不会越积越多。
/// 老客户端不回确认，那就退回改动前的行为（不拦）。
pub struct MediaSlot {
    inner: Mutex<MediaQueue>,
}

/// H.264 最多排几帧：再多说明主控端已经跟不上，直接丢到下一个关键帧。
const H264_MAX_QUEUED: usize = 3;

/// 一条在途记录最多保留多久。确认只用来算「在途」帧数，来晚了丢掉记录
/// 只是少算一点，不能因此把画面卡死。
const PENDING_MAX_AGE: f64 = 6.0;
/// 「在途」帧数的初始值和上下限。用确认回来的时间做拥塞控制：
/// 延迟低就多发几帧把带宽跑满，延迟高了就少发几帧把队列排空。
const WINDOW_START: f64 = 2.0;
const WINDOW_MIN: f64 = 1.0;
const WINDOW_MAX: f64 = 8.0;
/// 排队延迟（当前往返减去这条链路的基线往返）超过这个值就收窗口。
const WINDOW_SLOW_DELAY: f64 = 0.6;
/// 排队延迟低于这个值说明链路还有余量，可以加窗口。
const WINDOW_FAST_DELAY: f64 = 0.25;
/// 连续这么多次「够快」才加一帧（加性增），减速则一次到位（乘性减）。
const WINDOW_GROW_ACKS: u32 = 5;

struct StoredFrame {
    data: Vec<u8>,
    seq: u32,
}

struct PendingFrame {
    seq: u32,
    sent_at: std::time::Instant,
}

struct MediaQueue {
    jpeg: Option<StoredFrame>,
    h264: VecDeque<StoredFrame>,
    /// 丢过 H.264 帧，下一个关键帧之前送过去的都解不出来。
    wait_keyframe: bool,
    /// 当前画面流。换流（重新订阅）后旧的确认序号作废。
    stream: u16,
    /// 已发出、还没等到 `video.ack` 的帧（按发送顺序）。
    pending: VecDeque<PendingFrame>,
    /// 主控端有没有回过确认（网页端才回）。
    ack_seen: bool,
    /// 当前允许在途的帧数（拥塞窗口）。
    window: f64,
    /// 连续「够快」的确认次数。
    grow: u32,
    /// 上次收窗口的时间，避免一次拥塞被连续罚好几次。
    last_shrink: Option<std::time::Instant>,
    /// 见过的最小「发出 → 确认」耗时，也就是这条链路的基线往返。
    /// 真实的排队情况要看「当前耗时 − 基线」，不能只看绝对值。
    min_age: f64,
}

impl Default for MediaQueue {
    fn default() -> Self {
        Self {
            jpeg: None,
            h264: VecDeque::new(),
            wait_keyframe: false,
            stream: 0,
            pending: VecDeque::new(),
            ack_seen: false,
            window: WINDOW_START,
            grow: 0,
            last_shrink: None,
            min_age: f64::MAX,
        }
    }
}

impl std::fmt::Debug for MediaSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queue = self.lock();
        f.debug_struct("MediaSlot")
            .field("jpeg", &queue.jpeg.as_ref().map(|frame| frame.data.len()))
            .field("h264", &queue.h264.len())
            .field("wait_keyframe", &queue.wait_keyframe)
            .field("pending", &queue.pending.len())
            .field("ack_seen", &queue.ack_seen)
            .field("window", &queue.window)
            .finish()
    }
}

impl Default for MediaSlot {
    fn default() -> Self {
        Self {
            inner: Mutex::new(MediaQueue::default()),
        }
    }
}

impl MediaSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, MediaQueue> {
        // 锁里只有一次替换/取出，不会 panic；真被毒化也继续用，别让画面线程挂掉。
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 投放一帧 JPEG。返回是否挤掉了上一帧（计入丢弃指标）。
    pub fn put_jpeg(&self, data: Vec<u8>, stream: u16, seq: u32) -> bool {
        let mut queue = self.lock();
        queue.switch_stream(stream);
        queue.jpeg.replace(StoredFrame { data, seq }).is_some()
    }

    /// 投放一帧 H.264。返回是否丢了帧（需要主控端等关键帧）。
    pub fn put_h264(&self, data: Vec<u8>, stream: u16, seq: u32, keyframe: bool) -> bool {
        let mut queue = self.lock();
        queue.switch_stream(stream);
        let frame = StoredFrame { data, seq };
        let mut dropped = false;
        if queue.wait_keyframe {
            if !keyframe {
                return true;
            }
            queue.wait_keyframe = false;
            queue.h264.clear();
        } else if queue.h264.len() >= H264_MAX_QUEUED {
            queue.h264.clear();
            dropped = true;
            if !keyframe {
                queue.wait_keyframe = true;
                return true;
            }
        }
        queue.h264.push_back(frame);
        dropped
    }

    /// 现在能不能发下一帧。
    pub fn ready(&self) -> bool {
        let mut queue = self.lock();
        if !queue.ack_seen {
            // 客户端不回确认（老版本 / 非网页端）：不拦，行为与改动前一致。
            return true;
        }
        // 确认一直不回来时只把过老的记录放掉，避免永久卡住。
        while let Some(oldest) = queue.pending.front() {
            if oldest.sent_at.elapsed().as_secs_f64() < PENDING_MAX_AGE {
                break;
            }
            queue.pending.pop_front();
        }
        (queue.pending.len() as f64) < queue.window
    }

    /// 主控端确认已收到 `seq` 那一帧（序号是累计的，之前发的也都到了）。
    pub fn ack(&self, seq: u32) {
        let mut queue = self.lock();
        queue.ack_seen = true;
        let Some(position) = queue.pending.iter().position(|frame| frame.seq == seq) else {
            return;
        };
        let now = std::time::Instant::now();
        let age = now
            .duration_since(queue.pending[position].sent_at)
            .as_secs_f64();
        for _ in 0..=position {
            queue.pending.pop_front();
        }
        // 拥塞控制：一帧「发出 → 主控端收到」的耗时减去这条链路的基线往返，
        // 就是服务端自己攒出来的排队延迟。排队多了收窗口，排队很少就加窗口，
        // 直到带宽跑满或延迟到上限。
        if age < queue.min_age {
            queue.min_age = age;
        }
        let delay = age - queue.min_age;
        if delay > WINDOW_SLOW_DELAY {
            queue.grow = 0;
            let cooldown = queue
                .last_shrink
                .map(|at| now.duration_since(at).as_secs_f64() >= 0.3)
                .unwrap_or(true);
            if cooldown {
                queue.window = (queue.window * 0.6).max(WINDOW_MIN);
                queue.last_shrink = Some(now);
            }
        } else if delay < WINDOW_FAST_DELAY {
            queue.grow += 1;
            if queue.grow >= WINDOW_GROW_ACKS {
                queue.grow = 0;
                queue.window = (queue.window + 1.0).min(WINDOW_MAX);
            }
        } else {
            queue.grow = 0;
        }
    }

    /// 取下一帧：JPEG 优先，其次 H.264 队首；取出即记为一帧在途。
    pub fn take(&self) -> Option<Vec<u8>> {
        let mut queue = self.lock();
        let frame = queue.jpeg.take().or_else(|| queue.h264.pop_front())?;
        // 不回确认的客户端（桌面端 / 旧网页端）不需要记账，否则「在途」列表
        // 会随整场会话一直长下去；收到第一条确认后这里才开始记。
        if queue.ack_seen {
            queue.pending.push_back(PendingFrame {
                seq: frame.seq,
                sent_at: std::time::Instant::now(),
            });
        }
        Some(frame.data)
    }
}

impl MediaQueue {
    fn switch_stream(&mut self, stream: u16) {
        if self.stream != stream {
            self.stream = stream;
            self.pending.clear();
            self.jpeg = None;
            self.h264.clear();
            self.wait_keyframe = false;
            self.min_age = f64::MAX;
            self.grow = 0;
        }
    }
}

#[derive(Debug, Clone)]
pub struct ViewerConn {
    pub user_id: i64,
    /// 主控端设备 ID（来自登录/连接令牌），用于审计与按设备吊销。
    pub device_id: String,
    /// 连接码令牌只允许看见/打开这一台设备。
    pub restricted: Option<String>,
    pub tx: mpsc::UnboundedSender<WsOut>,
    /// 待发的画面帧（慢链路时只保留最新的）。
    pub media: Arc<MediaSlot>,
}

/// 单个会话的累计中转字节数，被会话克隆共享。
#[derive(Debug, Default)]
pub struct SessionStats {
    pub up: AtomicU64,
    pub down: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: i64,
    pub device_key: String,
    pub device_id: String,
    /// 主控端设备 ID，写入审计的「主控设备」。
    pub controller_device_id: String,
    pub viewer_id: u64,
    pub viewer_tx: mpsc::UnboundedSender<WsOut>,
    /// 主控端的画面队列（慢链路只留最新帧）。
    pub media: Arc<MediaSlot>,
    pub request: u32,
    /// `control`（可操作）或 `wall`（只读缩略图，不写审计）。
    pub mode: String,
    /// 会话真正建立（设备回 open.ok）时写入审计并更新时间。
    pub started_at: i64,
    pub audit_id: Option<i64>,
    pub stats: Arc<SessionStats>,
}

/// `Session::new` 的入参：会话的身份与路由信息。
///
/// 用结构体而不是 8 个位置参数，避免调用点把 `device_id` 和
/// `controller_device_id` 这类同类型字段写反。
pub struct SessionInit {
    pub user_id: i64,
    pub device_key: String,
    pub device_id: String,
    pub controller_device_id: String,
    pub viewer_id: u64,
    pub viewer_tx: mpsc::UnboundedSender<WsOut>,
    pub media: Arc<MediaSlot>,
    pub request: u32,
    pub mode: String,
}

impl Session {
    pub fn new(init: SessionInit) -> Self {
        Self {
            user_id: init.user_id,
            device_key: init.device_key,
            device_id: init.device_id,
            controller_device_id: init.controller_device_id,
            viewer_id: init.viewer_id,
            viewer_tx: init.viewer_tx,
            media: init.media,
            request: init.request,
            mode: init.mode,
            started_at: now_ms(),
            audit_id: None,
            stats: Arc::new(SessionStats::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub os: String,
    pub version: String,
    pub online: bool,
    pub last_seen: i64,
    pub controllable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gpu: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ip: String,
    /// 本机识别码（9 位数字）：别人在同一网段或已登录服务器时都能用它找人。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub local_id: String,
    /// 被控端支持网页端 WebRTC P2P；网页端据此决定要不要先试直连。
    #[serde(default)]
    pub webrtc: bool,
}

pub struct AppState {
    pub db: Db,
    pub config: Config,
    /// 设备配额与限流参数（`[limits]` 段），旧配置缺字段时取默认值。
    pub limits: LimitsConfig,
    pub devices: RwLock<HashMap<String, DeviceConn>>,
    pub viewers: RwLock<HashMap<u64, ViewerConn>>,
    pub sessions: RwLock<HashMap<u32, Session>>,
    pub login_limiter: LoginLimiter,
    /// 登录请求频率（按客户端 IP），与失败次数锁定互补。
    pub login_rate: RateLimiter,
    /// 注册请求频率（按客户端 IP）。
    pub register_rate: RateLimiter,
    /// `/ws/view` 建连频率（按账号）。
    pub view_connect_rate: RateLimiter,
    /// 会话建立频率（按主控连接）。
    pub session_open_rate: RateLimiter,
    /// 连接码尝试频率（按客户端 IP）。
    pub quick_connect_rate: RateLimiter,
    /// 建议/BUG 提交频率（按账号）：每小时 5 条，允许一次连发 3 条。
    pub feedback_rate: RateLimiter,
    pub metrics: Metrics,
    /// 自托管更新源（`[update]` 段）；未配置时相关接口返回 404。
    pub update: UpdateState,
    /// 网页端 WebRTC P2P（`[webrtc]` 段）；`enabled = false` 时整条链路关闭。
    pub webrtc: crate::config::WebrtcConfig,
    /// `/api/v1/p2p/ice` 的签发频率（按账号）。
    pub ice_rate: RateLimiter,
    /// WebRTC 信令登记：`(viewer_id, request)` → 目标设备，TTL 60 秒。
    pub webrtc_pending: RwLock<HashMap<(u64, u32), PendingWebrtc>>,
    /// P2P 打洞请求：`(viewer_id, request)` → 发起方与目标设备。
    ///
    /// 只用来把被控端的 `answer` 送回正确的主控端连接，不参与会话。
    /// 条目由超时或应答消费，避免主控端一直等。
    pub p2p_pending: RwLock<HashMap<(u64, u32), PendingP2p>>,
    next_session: AtomicU32,
    next_viewer: AtomicU64,
}

/// 一条还没被应答的 P2P 请求。
pub struct PendingP2p {
    pub device_key: String,
    pub created: std::time::Instant,
}

/// 一段正在协商的网页端 WebRTC 会话。
///
/// 与 [`PendingP2p`] 不同，WebRTC 不是一次问答：offer / answer / trickle ICE
/// 会来回多条消息，所以这里记录有效期与双向消息数，超时或刷屏就整段丢弃。
pub struct PendingWebrtc {
    pub device_key: String,
    pub created: std::time::Instant,
    pub messages: u32,
}

impl PendingWebrtc {
    /// 一段协商最多允许的转发条数（双方合计）：足够 trickle ICE，又能防刷。
    pub const MAX_MESSAGES: u32 = 128;
    /// 协商最长存活时间；超过后新的消息一律丢弃，主控端会回退中继。
    pub const TTL: std::time::Duration = std::time::Duration::from_secs(60);
}

impl AppState {
    /// 使用默认配额与限流参数创建状态。
    pub fn new(db: Db, config: Config) -> Self {
        Self::with_limits(db, config, LimitsConfig::default())
    }

    /// 使用显式配额与限流参数创建状态。
    pub fn with_limits(db: Db, config: Config, limits: LimitsConfig) -> Self {
        let login_rate = RateLimiter::new(
            limits.login_per_minute,
            std::time::Duration::from_secs(60),
            limits.login_burst,
        );
        let register_rate = RateLimiter::new(
            limits.register_per_hour,
            std::time::Duration::from_secs(3600),
            limits.register_burst,
        );
        let view_connect_rate = RateLimiter::new(
            limits.view_connects_per_minute,
            std::time::Duration::from_secs(60),
            limits.view_connect_burst,
        );
        let session_open_rate = RateLimiter::new(
            limits.session_opens_per_minute,
            std::time::Duration::from_secs(60),
            limits.session_open_burst,
        );
        let quick_connect_rate = RateLimiter::new(
            limits.quick_connects_per_minute,
            std::time::Duration::from_secs(60),
            limits.quick_connect_burst,
        );
        // 反馈不走 `[limits]` 配置：它跟业务流量无关，默认值够用。
        let feedback_rate = RateLimiter::new(5, std::time::Duration::from_secs(3600), 3);
        Self {
            db,
            config,
            limits,
            devices: RwLock::new(HashMap::new()),
            viewers: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            login_limiter: LoginLimiter::new(
                crate::ratelimit::MAX_FAILURES,
                crate::ratelimit::LOCKOUT,
            ),
            login_rate,
            register_rate,
            view_connect_rate,
            session_open_rate,
            quick_connect_rate,
            feedback_rate,
            metrics: Metrics::default(),
            update: UpdateState::default(),
            webrtc: crate::config::WebrtcConfig::default(),
            ice_rate: RateLimiter::new(12, std::time::Duration::from_secs(60), 6),
            webrtc_pending: RwLock::new(HashMap::new()),
            p2p_pending: RwLock::new(HashMap::new()),
            next_session: AtomicU32::new(1),
            next_viewer: AtomicU64::new(1),
        }
    }

    /// 单账号设备上限；0 表示不限制。
    pub fn max_devices(&self) -> u32 {
        self.limits.max_devices_per_account
    }

    /// 账号能否再绑定这台设备。
    ///
    /// 已有设备重复登录/重连不计新增，直接放行；只有新设备才检查上限。
    /// 数据库查询失败时按「不允许」处理，避免绕过配额。
    pub fn device_quota_ok(&self, user_id: i64, device_id: &str) -> bool {
        let max = self.max_devices();
        if max == 0 {
            return true;
        }
        match self.db.device(user_id, device_id) {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(error) => {
                tracing::error!("读取设备失败，按配额已满处理: {error:#}");
                self.metrics.add_error();
                return false;
            }
        }
        match self.db.count_devices(user_id) {
            Ok(count) => count < i64::from(max),
            Err(error) => {
                tracing::error!("统计设备数失败，按配额已满处理: {error:#}");
                self.metrics.add_error();
                false
            }
        }
    }

    pub fn device_key(user_id: i64, device_id: &str) -> String {
        format!("{user_id}:{device_id}")
    }

    pub fn alloc_session(&self) -> u32 {
        loop {
            let id = self.next_session.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    pub fn alloc_viewer(&self) -> u64 {
        loop {
            let id = self.next_viewer.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// 登记一条新会话，并更新会话累计数与并发峰值。
    pub async fn insert_session(&self, session_id: u32, session: Session) {
        let active = {
            let mut sessions = self.sessions.write().await;
            sessions.insert(session_id, session);
            sessions.len() as u64
        };
        self.metrics.session_opened(active);
    }

    /// 当前活跃会话数与在线设备数（均为全服务器口径）。
    pub async fn live_counts(&self) -> (u64, u64) {
        let sessions = self.sessions.read().await.len() as u64;
        let devices = self.devices.read().await.len() as u64;
        (sessions, devices)
    }

    pub async fn device_views(&self, user_id: i64) -> Vec<DeviceView> {
        let rows = match self.db.list_devices(user_id) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!("读取设备列表失败: {error:#}");
                return Vec::new();
            }
        };
        let online: Vec<(String, bool)> = {
            let devices = self.devices.read().await;
            devices
                .values()
                .filter(|conn| conn.user_id == user_id)
                .map(|conn| (conn.device_id.clone(), conn.webrtc))
                .collect()
        };
        rows.into_iter()
            .map(|row| {
                let conn = online.iter().find(|(id, _)| id == &row.id);
                DeviceView {
                online: conn.is_some(),
                webrtc: conn.map(|(_, webrtc)| *webrtc).unwrap_or(false),
                id: row.id,
                name: row.name,
                os: row.os,
                version: row.version,
                last_seen: row.last_seen,
                controllable: row.controllable,
                gpu: row.gpu,
                ip: row.ip,
                local_id: row.local_id,
                }
            })
            .collect()
    }

    pub async fn devices_message(&self, user_id: i64) -> String {
        let devices = self.device_views(user_id).await;
        json!({ "t": "devices", "devices": devices }).to_string()
    }

    /// 受限会话（连接码）只看得见它被允许打开的那一台设备。
    pub async fn quick_devices_message(&self, user_id: i64, device_id: &str) -> String {
        let devices: Vec<DeviceView> = self
            .device_views(user_id)
            .await
            .into_iter()
            .filter(|device| device.id == device_id)
            .collect();
        json!({ "t": "devices", "devices": devices }).to_string()
    }

    pub async fn broadcast_devices(&self, user_id: i64) {
        let text = self.devices_message(user_id).await;
        let targets: Vec<(Option<String>, mpsc::UnboundedSender<WsOut>)> = {
            let viewers = self.viewers.read().await;
            viewers
                .values()
                .filter(|viewer| viewer.user_id == user_id)
                .map(|viewer| (viewer.restricted.clone(), viewer.tx.clone()))
                .collect()
        };
        for (restricted, tx) in targets {
            let payload = match restricted {
                Some(device_id) => self.quick_devices_message(user_id, &device_id).await,
                None => text.clone(),
            };
            let _ = tx.send(WsOut::Text(payload));
        }
    }

    pub async fn send_to_device(&self, device_key: &str, out: WsOut) -> bool {
        let devices = self.devices.read().await;
        match devices.get(device_key) {
            Some(conn) => conn.tx.send(out).is_ok(),
            None => false,
        }
    }

    pub async fn send_to_viewer(&self, session: &Session, out: WsOut) {
        let _ = session.viewer_tx.send(out);
    }

    /// 设备断线：只在断开的仍是当前连接时，移除在线记录、结束会话并通知主控端。
    ///
    /// 同一设备重连时，旧连接可能晚于新连接退出；不能让它把新连接删掉。
    pub async fn disconnect_device(
        &self,
        user_id: i64,
        device_id: &str,
        conn_tx: &mpsc::UnboundedSender<WsOut>,
    ) {
        let key = Self::device_key(user_id, device_id);
        let removed = {
            let mut devices = self.devices.write().await;
            let is_current = devices
                .get(&key)
                .map(|conn| conn.tx.same_channel(conn_tx))
                .unwrap_or(false);
            if is_current {
                devices.remove(&key)
            } else {
                None
            }
        };
        if removed.is_some() {
            self.close_sessions_for_device(&key, 4201, "device disconnected")
                .await;
            let _ = self.db.touch_device(user_id, device_id);
            self.broadcast_devices(user_id).await;
        }
    }

    pub async fn close_sessions_for_device(&self, device_key: &str, code: u16, msg: &str) {
        let removed: Vec<(u32, Session)> = {
            let mut sessions = self.sessions.write().await;
            let ids: Vec<u32> = sessions
                .iter()
                .filter(|(_, session)| session.device_key == device_key)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id).map(|session| (id, session)))
                .collect()
        };
        for (id, session) in removed {
            self.finish_session_audit(&session);
            let text =
                json!({ "t": "closed", "session": id, "code": code, "msg": msg }).to_string();
            let _ = session.viewer_tx.send(WsOut::Text(text));
        }
    }

    pub async fn close_sessions_for_viewer(&self, viewer_id: u64) {
        let removed: Vec<(u32, Session)> = {
            let mut sessions = self.sessions.write().await;
            let ids: Vec<u32> = sessions
                .iter()
                .filter(|(_, session)| session.viewer_id == viewer_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id).map(|session| (id, session)))
                .collect()
        };
        for (id, session) in removed {
            self.finish_session_audit(&session);
            let text = json!({ "t": "close", "session": id }).to_string();
            if !self
                .send_to_device(&session.device_key, WsOut::Text(text))
                .await
            {
                tracing::debug!("会话 {id} 对应的设备已离线");
                self.metrics.add_dropped();
            }
        }
    }

    /// 登记一条 P2P 打洞请求。表满（防止刷内存）或已有同键时返回 false。
    pub async fn register_p2p(&self, viewer_id: u64, request: u32, device_key: String) -> bool {
        const TTL: std::time::Duration = std::time::Duration::from_secs(90);
        const CAP: usize = 256;
        let now = std::time::Instant::now();
        let mut pending = self.p2p_pending.write().await;
        pending.retain(|_, entry| now.duration_since(entry.created) < TTL);
        if pending.len() >= CAP {
            return false;
        }
        pending.insert(
            (viewer_id, request),
            PendingP2p {
                device_key,
                created: now,
            },
        );
        true
    }

    /// 取出并删除一条 P2P 请求；设备不匹配时不动它。
    ///
    /// 返回 `true` 表示这条请求归该设备，可以把应答转回主控端。
    pub async fn take_p2p(&self, viewer_id: u64, request: u32, device_key: &str) -> bool {
        let mut pending = self.p2p_pending.write().await;
        match pending.get(&(viewer_id, request)) {
            Some(entry) if entry.device_key == device_key => {
                pending.remove(&(viewer_id, request));
                true
            }
            _ => false,
        }
    }

    /// 主控连接断开：它没等到应答的 P2P 请求全部作废。
    pub async fn clear_p2p_for_viewer(&self, viewer_id: u64) {
        self.p2p_pending
            .write()
            .await
            .retain(|(owner, _), _| *owner != viewer_id);
    }

    /// 登记（或刷新）一段 WebRTC 协商。
    ///
    /// 同一主控连接最多同时协商 3 条；表满返回 false。
    pub async fn register_webrtc(&self, viewer_id: u64, request: u32, device_key: String) -> bool {
        const CAP: usize = 256;
        const PER_VIEWER: usize = 3;
        let now = std::time::Instant::now();
        let mut pending = self.webrtc_pending.write().await;
        pending.retain(|_, entry| now.duration_since(entry.created) < PendingWebrtc::TTL);
        if pending.len() >= CAP {
            return false;
        }
        let mine = pending
            .keys()
            .filter(|(owner, key)| *owner == viewer_id && *key != request)
            .count();
        if mine >= PER_VIEWER {
            return false;
        }
        pending.insert(
            (viewer_id, request),
            PendingWebrtc {
                device_key,
                created: now,
                messages: 0,
            },
        );
        true
    }

    /// 查一段 WebRTC 协商的目标设备；过期或不存在返回 `None`。
    pub async fn webrtc_target(&self, viewer_id: u64, request: u32) -> Option<String> {
        let mut pending = self.webrtc_pending.write().await;
        let now = std::time::Instant::now();
        match pending.get(&(viewer_id, request)) {
            Some(entry) if now.duration_since(entry.created) < PendingWebrtc::TTL => {
                Some(entry.device_key.clone())
            }
            Some(_) => {
                pending.remove(&(viewer_id, request));
                None
            }
            None => None,
        }
    }

    /// 记一条转发；超过上限返回 false（调用方丢弃这条消息）。
    pub async fn bump_webrtc(&self, viewer_id: u64, request: u32) -> bool {
        let mut pending = self.webrtc_pending.write().await;
        match pending.get_mut(&(viewer_id, request)) {
            Some(entry) if entry.messages < PendingWebrtc::MAX_MESSAGES => {
                entry.messages += 1;
                true
            }
            _ => false,
        }
    }

    /// 协商结束（reject / bye）：删掉登记，后续消息不再转发。
    pub async fn finish_webrtc(&self, viewer_id: u64, request: u32) {
        self.webrtc_pending
            .write()
            .await
            .remove(&(viewer_id, request));
    }

    /// 主控连接断开：它正在协商的 WebRTC 会话全部作废。
    pub async fn clear_webrtc_for_viewer(&self, viewer_id: u64) {
        self.webrtc_pending
            .write()
            .await
            .retain(|(owner, _), _| *owner != viewer_id);
    }

    /// 会话真正建立（设备回 `open.ok`）时写入审计开始记录。
    ///
    /// 在会话写锁内做「检查 + 插入 + 标记」，重复的 `open.ok` 不会产生多余记录。
    pub async fn start_session_audit(&self, session_id: u32, device_key: &str) {
        let mut sessions = self.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return;
        };
        if session.device_key != device_key || session.audit_id.is_some() {
            return;
        }
        // 屏幕墙缩略图会话是只读预览，不写审计，避免刷屏。
        if session.mode == "wall" {
            session.started_at = now_ms();
            return;
        }

        let started_at = now_ms();
        match self.db.create_audit(
            session.user_id,
            session_id,
            &session.controller_device_id,
            &session.device_id,
            started_at,
        ) {
            Ok(audit_id) => {
                session.audit_id = Some(audit_id);
                session.started_at = started_at;
            }
            Err(error) => {
                tracing::error!("写入审计开始记录失败: {error:#}");
                self.metrics.add_error();
            }
        }
    }

    /// 补齐会话审计的结束时间、时长与字节数。没有审计记录时静默跳过。
    pub fn finish_session_audit(&self, session: &Session) {
        let Some(audit_id) = session.audit_id else {
            return;
        };
        let ended_at = now_ms();
        let bytes_up = session.stats.up.load(Ordering::Relaxed);
        let bytes_down = session.stats.down.load(Ordering::Relaxed);
        if let Err(error) = self.db.finish_audit(
            audit_id,
            ended_at,
            ended_at - session.started_at,
            bytes_up,
            bytes_down,
        ) {
            tracing::error!("写入审计结束记录失败: {error:#}");
            self.metrics.add_error();
        }
    }

    /// 关闭以某个设备身份作为主控端的长连接（用于令牌吊销）。
    pub async fn close_viewers_for_device(&self, user_id: i64, device_id: &str) {
        let targets: Vec<mpsc::UnboundedSender<WsOut>> = {
            let viewers = self.viewers.read().await;
            viewers
                .values()
                .filter(|viewer| viewer.user_id == user_id && viewer.device_id == device_id)
                .map(|viewer| viewer.tx.clone())
                .collect()
        };
        for tx in targets {
            let _ = tx.send(WsOut::Close);
        }
    }
}
