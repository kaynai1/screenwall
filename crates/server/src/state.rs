//! 服务器运行状态：在线设备、主控连接与会话表。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

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
}

#[derive(Debug, Clone)]
pub struct ViewerConn {
    pub user_id: i64,
    /// 主控端设备 ID（来自登录/连接令牌），用于审计与按设备吊销。
    pub device_id: String,
    /// 连接码令牌只允许看见/打开这一台设备。
    pub restricted: Option<String>,
    pub tx: mpsc::UnboundedSender<WsOut>,
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
    pub metrics: Metrics,
    /// 自托管更新源（`[update]` 段）；未配置时相关接口返回 404。
    pub update: UpdateState,
    next_session: AtomicU32,
    next_viewer: AtomicU64,
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
            metrics: Metrics::default(),
            update: UpdateState::default(),
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
        let online: Vec<String> = {
            let devices = self.devices.read().await;
            devices
                .values()
                .filter(|conn| conn.user_id == user_id)
                .map(|conn| conn.device_id.clone())
                .collect()
        };
        rows.into_iter()
            .map(|row| DeviceView {
                online: online.iter().any(|id| id == &row.id),
                id: row.id,
                name: row.name,
                os: row.os,
                version: row.version,
                last_seen: row.last_seen,
                controllable: row.controllable,
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
