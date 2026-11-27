//! HTTP API：注册、登录、设备管理。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::{
    bearer_token, hash_password, hash_token, new_device_id, new_token, now_ms, valid_device_id,
    valid_password, valid_username, verify_password,
};
use crate::state::{AppState, WsOut};
use crate::tls::PeerAddr;

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    msg: String,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status,
            code,
            msg: msg.into(),
            retry_after: None,
        }
    }

    fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({ "error": self.code, "msg": self.msg })),
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            if let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        response
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_input", msg)
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "登录已失效，请重新登录",
    )
}

fn auth_failed() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "auth_failed", "用户名或密码错误")
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!("服务器内部错误: {error}");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "服务器内部错误",
    )
}

fn conflict(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "conflict", msg)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "设备不存在")
}

fn rate_limited(seconds: u64) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("登录失败次数过多，请在 {seconds} 秒后重试"),
    )
    .with_retry_after(seconds)
}

/// 通用请求频率超限：不同接口用各自的动作名，文案统一带剩余秒数。
fn too_many_requests(action: &str, seconds: u64) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("{action}过于频繁，请在 {seconds} 秒后重试"),
    )
    .with_retry_after(seconds)
}

/// 设备配额超限：明确告诉用户上限与处理方式，不允许静默失败。
fn device_quota_exceeded(max: u32) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "device_limit_reached",
        format!("该账号最多绑定 {max} 台设备，请先在设备列表里删除不再使用的设备"),
    )
}

/// 可选的客户端地址：服务器用 `into_make_service_with_connect_info` 启动时存在；
/// 直接挂路由的测试或反向代理场景下为 `None`。
pub struct ClientIp(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let addr = parts
            .extensions
            .get::<ConnectInfo<PeerAddr>>()
            .map(|info| (info.0).0);
        Ok(ClientIp(addr))
    }
}

#[derive(Debug, Clone)]
pub struct Auth {
    pub user_id: i64,
    pub device_id: String,
    pub token_hash: String,
    /// 连接码换来的受限令牌：只能打开 `target_device_id`。
    pub quick: bool,
    pub target_device_id: String,
}

pub fn authorize(state: &AppState, headers: &HeaderMap) -> Result<Auth, ApiError> {
    let token = bearer_token(headers).ok_or_else(|| {
        state.metrics.add_error();
        unauthorized()
    })?;
    let token_hash = hash_token(token);
    let row = state
        .db
        .validate_token(&token_hash)
        .map_err(internal)?
        .ok_or_else(|| {
            state.metrics.add_error();
            unauthorized()
        })?;
    Ok(Auth {
        user_id: row.user_id,
        device_id: row.device_id,
        token_hash,
        quick: row.quick,
        target_device_id: row.target_device_id,
    })
}

/// 账号接口不接受连接码令牌：受限令牌只用于 `/ws/view`。
pub fn authorize_account(state: &AppState, headers: &HeaderMap) -> Result<Auth, ApiError> {
    let auth = authorize(state, headers)?;
    if auth.quick {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "quick_token",
            "连接码令牌不能访问账号接口",
        ));
    }
    Ok(auth)
}

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub username: String,
    pub password: String,
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    client: ClientIp,
    Json(req): Json<RegisterReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = client
        .0
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(seconds) = state.register_rate.check(&ip) {
        // 只在触发限流时记录一次，避免每个被拒请求都刷日志。
        tracing::warn!("注册请求过于频繁，已限流: ip={ip} retry_after={seconds}s");
        state.metrics.add_error();
        return Err(too_many_requests("注册", seconds));
    }
    if !state.config.allow_registration {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "registration_closed",
            "服务器未开放注册",
        ));
    }
    let username = req.username.trim().to_string();
    if !valid_username(&username) {
        return Err(bad_request("用户名需为 3–32 位字母、数字、下划线或连字符"));
    }
    if !valid_password(&req.password) {
        return Err(bad_request("密码需为 8–128 位"));
    }
    if state.db.find_user(&username).map_err(internal)?.is_some() {
        return Err(conflict("用户名已存在"));
    }

    let password = req.password.clone();
    let pass_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    let user_id = state
        .db
        .create_user(&username, &pass_hash)
        .map_err(|error| {
            if format!("{error:#}").contains("UNIQUE") {
                conflict("用户名已存在")
            } else {
                internal(error)
            }
        })?;

    let token = new_token();
    let expires_at = now_ms() + state.config.token_ttl_days * 86_400_000;
    state
        .db
        .create_token(&hash_token(&token), user_id, "", expires_at)
        .map_err(internal)?;

    Ok(Json(json!({
        "token": token,
        "expiresAt": expires_at,
        "user": { "id": user_id, "username": username }
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub gpu: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    /// 本机识别码（9 位数字）：登录时一并上报，别人才能跨网段用识别码找到本机。
    #[serde(default)]
    pub local_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device: Option<DeviceInfo>,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    client: ClientIp,
    Json(req): Json<LoginReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 没有 ConnectInfo（例如测试直接调用路由）时退化为全局 "unknown" 桶。
    let ip = client
        .0
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let username = req.username.trim().to_string();

    // 第一道：请求频率（按 IP 的令牌桶），每次尝试都计数，防止高频撞库把 CPU 打满。
    if let Some(seconds) = state.login_rate.check(&ip) {
        tracing::warn!("登录请求过于频繁，已限流: ip={ip} retry_after={seconds}s");
        state.metrics.add_error();
        return Err(too_many_requests("登录", seconds));
    }
    // 第二道：同一用户名 + IP 的连续失败锁定。
    if let Some(seconds) = state.login_limiter.locked_for(&username, &ip) {
        state.metrics.add_error();
        return Err(rate_limited(seconds));
    }

    let Some(user) = state.db.find_user(&username).map_err(internal)? else {
        state.metrics.add_error();
        let locked = state.login_limiter.record_failure(&username, &ip);
        return Err(locked.map(rate_limited).unwrap_or_else(auth_failed));
    };
    if user.disabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "user_disabled",
            "账号已被禁用",
        ));
    }

    let password = req.password.clone();
    let pass_hash = user.pass_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &pass_hash))
        .await
        .map_err(internal)?;
    if !ok {
        state.metrics.add_error();
        let locked = state.login_limiter.record_failure(&username, &ip);
        return Err(locked.map(rate_limited).unwrap_or_else(auth_failed));
    }
    state.login_limiter.record_success(&username, &ip);

    let device = req.device.unwrap_or(DeviceInfo {
        id: None,
        name: None,
        os: None,
        version: None,
        gpu: None,
        ip: None,
        local_id: None,
    });
    let device_id = match device.id {
        Some(id) if valid_device_id(&id) => id,
        Some(_) => return Err(bad_request("设备 ID 格式无效")),
        None => new_device_id(),
    };
    let name = device
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("未命名设备")
        .chars()
        .take(64)
        .collect::<String>();
    let os = device.os.unwrap_or_default();
    let version = device.version.unwrap_or_default();
    let gpu = device.gpu.unwrap_or_default();
    let ip = device.ip.unwrap_or_default();
    let local_id = device
        .local_id
        .as_deref()
        .and_then(normalize_local_id)
        .unwrap_or_default();

    // 单账号设备上限：重复登录已有设备不受影响，新设备超限直接拒绝。
    if !state.device_quota_ok(user.id, &device_id) {
        let max = state.max_devices();
        tracing::warn!(
            "设备配额已满，拒绝绑定新设备: user={} device={device_id} limit={max}",
            user.username
        );
        state.metrics.add_error();
        return Err(device_quota_exceeded(max));
    }

    state
        .db
        .upsert_device(
            user.id, &device_id, &name, &os, &version, &gpu, &ip, &local_id,
        )
        .map_err(internal)?;

    let token = new_token();
    let expires_at = now_ms() + state.config.token_ttl_days * 86_400_000;
    state
        .db
        .create_token(&hash_token(&token), user.id, &device_id, expires_at)
        .map_err(internal)?;

    // 登录时顺手确保连接码存在，客户端可以直接显示「本机 ID」。
    let connect_code = state
        .db
        .ensure_connect_code(user.id, &device_id)
        .unwrap_or_else(|error| {
            tracing::warn!("分配连接码失败: {error:#}");
            String::new()
        });

    state.broadcast_devices(user.id).await;

    Ok(Json(json!({
        "token": token,
        "expiresAt": expires_at,
        "user": { "id": user.id, "username": user.username },
        "device": { "id": device_id, "name": name, "connectCode": connect_code }
    })))
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let user = state
        .db
        .user_by_id(auth.user_id)
        .map_err(internal)?
        .ok_or_else(unauthorized)?;
    Ok(Json(json!({
        "user": { "id": user.id, "username": user.username }
    })))
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    state.db.revoke_token(&auth.token_hash).map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let devices = state.device_views(auth.user_id).await;
    Ok(Json(json!({ "devices": devices })))
}

#[derive(Debug, Deserialize)]
pub struct PatchDeviceReq {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub controllable: Option<bool>,
}

pub async fn patch_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
    Json(req): Json<PatchDeviceReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    if state
        .db
        .device(auth.user_id, &device_id)
        .map_err(internal)?
        .is_none()
    {
        return Err(not_found());
    }

    if let Some(name) = req.name {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(bad_request("设备名需为 1–64 个字符"));
        }
        state
            .db
            .rename_device(auth.user_id, &device_id, name)
            .map_err(internal)?;
    }
    if let Some(controllable) = req.controllable {
        state
            .db
            .set_controllable(auth.user_id, &device_id, controllable)
            .map_err(internal)?;
    }

    state.broadcast_devices(auth.user_id).await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    // 本机设备不允许删除：删掉自己的设备等于把自己从账号里摘出去，
    // 而请求方正是这台设备 —— 它之后再也没法用同一份令牌登录回来。
    // 客户端界面已经隐藏了「移除」按钮，这里必须同样拦住，不能只靠界面。
    if device_id == auth.device_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "cannot_remove_self",
            "不能删除本机设备",
        ));
    }
    if !state
        .db
        .delete_device(auth.user_id, &device_id)
        .map_err(internal)?
    {
        return Err(not_found());
    }
    state
        .db
        .revoke_device_tokens(auth.user_id, &device_id)
        .map_err(internal)?;

    let key = AppState::device_key(auth.user_id, &device_id);
    if let Some(conn) = state.devices.write().await.remove(&key) {
        let _ = conn.tx.send(WsOut::Close);
    }
    state
        .close_sessions_for_device(&key, 4201, "device removed")
        .await;
    state.broadcast_devices(auth.user_id).await;
    Ok(Json(json!({ "ok": true })))
}

/// 吊销某个设备的全部令牌，并断开它当前的中转/主控连接。
///
/// 被吊销后，该设备持有的令牌在 `/api` 与 `/ws` 上都会被拒绝。
pub async fn revoke_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    if !valid_device_id(&device_id) {
        return Err(bad_request("设备 ID 格式无效"));
    }
    if state
        .db
        .device(auth.user_id, &device_id)
        .map_err(internal)?
        .is_none()
    {
        return Err(not_found());
    }

    let revoked = state
        .db
        .revoke_device_tokens_count(auth.user_id, &device_id)
        .map_err(internal)?;

    // 断开该设备作为被控端的长连接，并结束它参与的会话（同时写入审计）。
    let key = AppState::device_key(auth.user_id, &device_id);
    if let Some(conn) = state.devices.write().await.remove(&key) {
        let _ = conn.tx.send(WsOut::Close);
    }
    state
        .close_sessions_for_device(&key, 4201, "token revoked")
        .await;
    // 断开以该设备身份作为主控端的长连接。
    state
        .close_viewers_for_device(auth.user_id, &device_id)
        .await;
    state.broadcast_devices(auth.user_id).await;

    Ok(Json(json!({ "ok": true, "revoked": revoked })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// 设备 ID；同时匹配主控端与被控端，空字符串视为未传。
    #[serde(default)]
    pub device_id: Option<String>,
}

fn default_audit_limit() -> i64 {
    50
}

/// 分页查询会话审计记录（仅当前账号），返回 `audits` 与满足过滤条件的 `total`。
pub async fn audits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let limit = query.limit.clamp(1, 500);
    let offset = query.offset.max(0);
    // 空字符串按未传处理，避免把 `deviceId=` 当成过滤空设备。
    let device = query.device_id.as_deref().filter(|value| !value.is_empty());
    let (rows, total) = state
        .db
        .audit_page(auth.user_id, limit, offset, device)
        .map_err(internal)?;
    Ok(Json(json!({ "audits": rows, "total": total })))
}

/// 客户端上报的一次局域网直连会话（已结束）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectAuditReq {
    pub controller_device_id: String,
    pub controlled_device_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    #[serde(default)]
    pub bytes_up: u64,
    #[serde(default)]
    pub bytes_down: u64,
}

/// 写入一条直连审计。
///
/// 直连不经过服务器，服务器无从自行记录，只能由主控端在会话结束后上报。
/// 内容是客户端自述的，所以要严格做边界检查（长度、时间顺序、时长上限），
/// 不让一条脏请求把审计页搞乱。
pub async fn report_direct_audit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DirectAuditReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;

    let controller = body.controller_device_id.trim();
    let controlled = body.controlled_device_id.trim();
    if controller.is_empty() || controlled.is_empty() {
        return Err(bad_request("设备 ID 不能为空"));
    }
    if controller.len() > 128 || controlled.len() > 128 {
        return Err(bad_request("设备 ID 过长"));
    }
    if body.started_at <= 0 || body.ended_at < body.started_at {
        return Err(bad_request("会话起止时间无效"));
    }
    // 上限 30 天：明显是坏数据（比如设备时钟错乱）时截断，不拒绝整条记录。
    let duration = body.duration_ms.clamp(0, 30 * 24 * 3600 * 1000);

    state
        .db
        .insert_direct_audit(
            auth.user_id,
            controller,
            controlled,
            body.started_at,
            body.ended_at,
            duration,
            body.bytes_up,
            body.bytes_down,
        )
        .map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

/// 服务器运行指标（需要登录）。返回 JSON 而不是 Prometheus 文本。
///
/// 在线设备数、活跃会话数来自内存表，账号数来自数据库，其余为进程内累计计数。
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize_account(&state, &headers)?;
    let (active_sessions, online_devices) = state.live_counts().await;
    let accounts = state.db.count_users().map_err(internal)?.max(0) as u64;
    Ok(Json(state.metrics.snapshot(
        active_sessions,
        online_devices,
        accounts,
    )))
}

/// 旧版 `/metrics` 的精简指标（需要登录）。
///
/// 保留原字段名以兼容已有监控脚本；新接入请使用 `/api/v1/metrics`。
pub async fn metrics_legacy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize_account(&state, &headers)?;
    let (active_sessions, online_devices) = state.live_counts().await;
    Ok(Json(json!({
        "onlineDevices": online_devices,
        "activeSessions": active_sessions,
        "totalBytes": state.metrics.total_bytes(),
        "errorCount": state.metrics.error_count(),
    })))
}

// ---------------------------------------------------------------- 连接码（ID + 密码）

/// 归一化连接码：只保留数字，允许用户按 `123 456 789` 分组输入。
fn normalize_connect_code(raw: &str) -> Option<String> {
    if !raw
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == ' ' || ch == '-')
    {
        return None;
    }
    let digits: String = raw.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.len() != 9 || digits.starts_with('0') {
        return None;
    }
    Some(digits)
}

/// 归一化本机识别码：和连接码同样的 9 位规则（客户端生成的码首位也不为 0）。
fn normalize_local_id(raw: &str) -> Option<String> {
    normalize_connect_code(raw)
}

#[derive(Debug, Deserialize)]
pub struct ResolveIdReq {
    pub id: String,
}

/// 按识别码在本账号的设备里找在线的那台。
///
/// 识别码原本只靠 UDP 广播在同网段解析；登录了服务器的人自然期望
/// 「输对方的识别码就能连」，哪怕两台机器不在一个网段。这里补上服务端查找：
/// 命中后返回设备 ID，主控端接着走中转会话。
///
/// **只在本账号内查**：识别码是显示在界面上的公开数字，不是凭据；
/// 用它跨账号找人等于让任何人凭一个公开数字连进别人电脑。同账号本来
/// 就是已授权关系（服务器会话带 `sameAccount`），用它跨网段连自己的另一台机器
/// 才是这条链路真正的用途。
pub async fn resolve_local_id(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ResolveIdReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let Some(id) = normalize_local_id(&req.id) else {
        return Err(bad_request("识别码是 9 位数字"));
    };
    let rows = state.db.list_devices(auth.user_id).map_err(internal)?;
    let matches: Vec<_> = rows.into_iter().filter(|row| row.local_id == id).collect();
    if matches.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "id_not_found",
            "你的账号里没有这个识别码的设备",
        ));
    }
    // 在线判断要走内存表：数据库里的 last_seen 可能是断线前的旧值。
    // 识别码理论上可能撞（客户端各自随机生成），所以优先挑在线的那台，
    // 而不是按列表顺序撞大运。
    let online_id = {
        let devices = state.devices.read().await;
        matches
            .iter()
            .find(|row| {
                devices
                    .values()
                    .any(|conn| conn.device_id == row.id && conn.user_id == auth.user_id)
            })
            .map(|row| row.id.clone())
    };
    // 重码时优先用在线那台的信息；都不在线就报第一台的「已关闭被控」。
    let row = match online_id.as_ref() {
        Some(id) => matches.iter().find(|row| &row.id == id),
        None => matches.first(),
    }
    .expect("matches 非空");
    if !row.controllable {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "control_disabled",
            "这台设备已关闭被控",
        ));
    }
    if online_id.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "device_offline",
            "这台设备当前不在线",
        ));
    }
    Ok(Json(json!({
        "id": row.id,
        "name": row.name,
        "online": true,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackReq {
    /// `suggestion`（建议）或 `bug`（问题）。
    pub category: String,
    pub content: String,
    /// 联系方式；用户愿意留才留。
    #[serde(default)]
    pub contact: String,
    /// 客户端版本，便于定位问题。
    #[serde(default)]
    pub app_version: String,
}

/// 提交一条建议或 BUG。
///
/// 反馈存在服务器本地库里，管理员用 `screenwall-server feedback list` 查看。
/// 只允许账号令牌提交（连接码会话没有账号归属，不参与反馈）。
pub async fn submit_feedback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<FeedbackReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let category = match req.category.trim() {
        "bug" => "bug",
        "suggestion" => "suggestion",
        _ => return Err(bad_request("分类只能是 suggestion 或 bug")),
    };
    let content = req.content.trim();
    let length = content.chars().count();
    if length < 5 {
        return Err(bad_request("内容太短，至少写 5 个字"));
    }
    if length > 4000 {
        return Err(bad_request("内容太长，最多 4000 字"));
    }
    // 按账号限流：正常用户不会一小时发五条以上。放在参数校验之后，
    // 格式错误的请求不消耗令牌，否则连续几次写错就会把配额耗光。
    if let Some(seconds) = state.feedback_rate.check(&auth.user_id.to_string()) {
        state.metrics.add_error();
        return Err(too_many_requests("提交反馈", seconds));
    }
    let contact: String = req.contact.trim().chars().take(120).collect();
    let app_version: String = req.app_version.trim().chars().take(32).collect();
    let username = state
        .db
        .user_by_id(auth.user_id)
        .map_err(internal)?
        .map(|user| user.username)
        .unwrap_or_default();
    let device_name = state
        .db
        .device(auth.user_id, &auth.device_id)
        .map_err(internal)?
        .map(|device| device.name)
        .unwrap_or_default();
    let id = state
        .db
        .add_feedback(
            auth.user_id,
            &username,
            &auth.device_id,
            &device_name,
            category,
            content,
            &contact,
            &app_version,
        )
        .map_err(internal)?;
    tracing::info!(
        "收到用户反馈: id={} user={} device={} category={}",
        id,
        username,
        auth.device_id,
        category
    );
    Ok(Json(json!({ "id": id })))
}

/// 读本机的连接码与开关状态（被控端设置页用）。
pub async fn quick_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let code = state
        .db
        .ensure_connect_code(auth.user_id, &auth.device_id)
        .map_err(internal)?;
    let (_, enabled, hash) = state
        .db
        .device_access(auth.user_id, &auth.device_id)
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok(Json(json!({
        "code": code,
        "enabled": enabled,
        "hasPassword": !hash.is_empty()
    })))
}

#[derive(Debug, Deserialize)]
pub struct QuickUpdateReq {
    /// 新密码；省略或空字符串表示只改开关，不动原密码。
    #[serde(default)]
    pub password: Option<String>,
    pub enabled: bool,
}

/// 打开/关闭连接码连接，可选地同时更新密码。
pub async fn quick_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QuickUpdateReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let password = req
        .password
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let hash = match password {
        Some(password) => {
            let length = password.chars().count();
            if !(6..=64).contains(&length) {
                return Err(bad_request("连接密码需要 6–64 位"));
            }
            let password = password.to_string();
            Some(
                tokio::task::spawn_blocking(move || hash_password(&password))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?,
            )
        }
        None => None,
    };

    if req.enabled && hash.is_none() {
        let (_, _, existing) = state
            .db
            .device_access(auth.user_id, &auth.device_id)
            .map_err(internal)?
            .ok_or_else(not_found)?;
        if existing.is_empty() {
            return Err(bad_request("请先设置连接密码"));
        }
    }

    let changed = state
        .db
        .set_device_access(auth.user_id, &auth.device_id, hash.as_deref(), req.enabled)
        .map_err(internal)?;
    if !changed {
        return Err(not_found());
    }
    tracing::info!(
        "连接码连接: user_id={} device={} enabled={}",
        auth.user_id,
        auth.device_id,
        req.enabled
    );
    Ok(Json(json!({ "enabled": req.enabled })))
}

/// 换一个新的连接码，旧码立即失效。
pub async fn quick_rotate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let code = state
        .db
        .rotate_connect_code(auth.user_id, &auth.device_id)
        .map_err(internal)?;
    tracing::info!(
        "连接码已更换: user_id={} device={}",
        auth.user_id,
        auth.device_id
    );
    Ok(Json(json!({ "code": code })))
}

#[derive(Debug, Deserialize)]
pub struct QuickConnectReq {
    pub code: String,
    pub password: String,
    /// 主控端显示名，写进被控端日志与审计。
    #[serde(default)]
    pub name: Option<String>,
}

/// 用「连接码 + 密码」换一个只能打开这台设备的受限令牌。
///
/// 不需要账号：这正是它和账号登录的区别。密码错误与连接码不存在返回同一句
/// 提示，避免把「这个码存在」泄露给扫描者。
pub async fn quick_connect(
    State(state): State<Arc<AppState>>,
    client: ClientIp,
    Json(req): Json<QuickConnectReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = client
        .0
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if let Some(seconds) = state.quick_connect_rate.check(&ip) {
        tracing::warn!("连接码尝试过于频繁，已限流: ip={ip} retry_after={seconds}s");
        state.metrics.add_error();
        return Err(too_many_requests("连接尝试", seconds));
    }

    let Some(code) = normalize_connect_code(&req.code) else {
        return Err(bad_request("连接码是 9 位数字"));
    };
    // 同一连接码 + IP 连续输错也锁一段时间，抑制逐个猜密码。
    let lock_key = format!("quick:{code}");
    if let Some(seconds) = state.login_limiter.locked_for(&lock_key, &ip) {
        state.metrics.add_error();
        return Err(rate_limited(seconds));
    }

    let Some(device) = state.db.device_by_connect_code(&code).map_err(internal)? else {
        state.metrics.add_error();
        let locked = state.login_limiter.record_failure(&lock_key, &ip);
        return Err(locked.map(rate_limited).unwrap_or_else(quick_auth_failed));
    };

    if !device.access_enabled || device.access_hash.is_empty() {
        state.metrics.add_error();
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "quick_disabled",
            "这台设备没有开启连接码连接",
        ));
    }
    if !device.controllable {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "control_disabled",
            "这台设备已关闭被控",
        ));
    }

    let password = req.password.clone();
    let hash = device.access_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(internal)?;
    if !ok {
        state.metrics.add_error();
        let locked = state.login_limiter.record_failure(&lock_key, &ip);
        return Err(locked.map(rate_limited).unwrap_or_else(quick_auth_failed));
    }
    state.login_limiter.record_success(&lock_key, &ip);

    let device_key = AppState::device_key(device.user_id, &device.id);
    if !state.devices.read().await.contains_key(&device_key) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "device_offline",
            "这台设备当前不在线",
        ));
    }

    // 受限令牌只活 15 分钟：连接本身是临时的，不需要长期凭据。
    let token = new_token();
    let expires_at = now_ms() + 15 * 60 * 1000;
    state
        .db
        .create_quick_token(&hash_token(&token), device.user_id, &device.id, expires_at)
        .map_err(internal)?;
    tracing::info!(
        "连接码会话签发: device={} ip={} name={}",
        device.id,
        ip,
        req.name.as_deref().unwrap_or("")
    );

    Ok(Json(json!({
        "token": token,
        "expiresAt": expires_at,
        "device": { "id": device.id, "name": device.name }
    })))
}

fn quick_auth_failed() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "quick_auth_failed",
        "连接码或密码不正确",
    )
}

/// 一次性连接链接的有效期（秒）：链接本身是凭据，短时效 + 单次消费。
const CONNECT_LINK_TTL_SECS: i64 = 10 * 60;

#[derive(Debug, Deserialize)]
pub struct ConnectLinkReq {
    /// 目标设备 ID；留空时用该账号的第一台设备。
    #[serde(default)]
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UseConnectLinkReq {
    pub token: String,
}

/// 为被控设备签发一条一次性连接链接：点开链接即唤起客户端，无需输入密码。
pub async fn create_connect_link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ConnectLinkReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize_account(&state, &headers)?;
    let device_id = match req.device_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            let devices = state.db.list_devices(auth.user_id).map_err(internal)?;
            let Some(first) = devices.first() else {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_device",
                    "该账号下没有设备",
                ));
            };
            first.id.clone()
        }
    };
    let Some(device) = state
        .db
        .device(auth.user_id, &device_id)
        .map_err(internal)?
    else {
        return Err(not_found());
    };
    let Some((_, access_enabled, access_hash)) = state
        .db
        .device_access(auth.user_id, &device_id)
        .map_err(internal)?
    else {
        return Err(not_found());
    };
    if access_hash.is_empty() {
        // 没有密码就开不了连接码连接；链接本身虽然是凭据，但被控端会把它
        // 当成「已授权的会话」，没有密码的设备不该有这条路径。
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "quick_no_password",
            "这台设备还没有连接密码，请先在「远程协助」里设置",
        ));
    }
    if !device.controllable {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "control_disabled",
            "这台设备已关闭被控",
        ));
    }
    if !access_enabled {
        // 用户点「生成链接」就是在说「现在让人进来」，只是开关还关着。
        // 这里顺手打开，免得为了发一条链接先跑去设置页点一遍开关。
        // 放在「是否允许被控」之后：被控关掉的设备不该因为点了一次链接
        // 就把连接码连接打开。
        state
            .db
            .set_device_access(auth.user_id, &device_id, None, true)
            .map_err(internal)?;
        tracing::info!(
            "生成连接链接时自动开启连接码连接: user_id={} device={}",
            auth.user_id,
            device_id
        );
    }

    let token = new_token();
    let expires_at = now_ms() + CONNECT_LINK_TTL_SECS * 1000;
    state
        .db
        .create_connect_link(auth.user_id, &device.id, &hash_token(&token), expires_at)
        .map_err(internal)?;

    // 链接地址用请求的 Host 构造；走反向代理时优先 x-forwarded-*。
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:8787")
        .trim()
        .to_string();
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .trim()
        .to_string();
    let url = format!("{proto}://{host}/l/{token}");

    tracing::info!(
        "一次性连接链接已签发: device={} user={}",
        device.id,
        auth.user_id
    );
    Ok(Json(json!({
        "url": url,
        "token": token,
        "expiresAt": expires_at,
        "expiresInSec": CONNECT_LINK_TTL_SECS,
        "device": { "id": device.id, "name": device.name },
    })))
}

/// 用一次性链接令牌换受限令牌：匿名、单次消费、只连这一台设备。
pub async fn use_connect_link(
    State(state): State<Arc<AppState>>,
    client: ClientIp,
    Json(req): Json<UseConnectLinkReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = client
        .0
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(seconds) = state.quick_connect_rate.check(&ip) {
        tracing::warn!("一次性链接消费过于频繁，已限流: ip={ip} retry_after={seconds}s");
        state.metrics.add_error();
        return Err(too_many_requests("链接使用", seconds));
    }
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return Err(bad_request("链接令牌为空"));
    }
    let token_hash = hash_token(&token);
    // 先只读校验：设备离线是暂时的，不能让一次「离线」把链接烧掉。
    let Some(row) = state
        .db
        .connect_link_target(&token_hash)
        .map_err(internal)?
    else {
        state.metrics.add_error();
        return Err(link_invalid());
    };

    let device_key = AppState::device_key(row.user_id, &row.device_id);
    if !state.devices.read().await.contains_key(&device_key) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "device_offline",
            "这台设备当前不在线",
        ));
    }
    // 消费是原子的：并发下同一条链接只会换到一个令牌。
    let Some(row) = state
        .db
        .consume_connect_link(&token_hash)
        .map_err(internal)?
    else {
        state.metrics.add_error();
        return Err(link_invalid());
    };
    let device_name = state
        .db
        .device(row.user_id, &row.device_id)
        .map_err(internal)?
        .map(|device| device.name)
        .unwrap_or_else(|| "远程设备".to_string());

    let session_token = new_token();
    let expires_at = now_ms() + 15 * 60 * 1000;
    state
        .db
        .create_quick_token(
            &hash_token(&session_token),
            row.user_id,
            &row.device_id,
            expires_at,
        )
        .map_err(internal)?;
    tracing::info!("一次性链接会话签发: device={} ip={ip}", row.device_id);
    Ok(Json(json!({
        "token": session_token,
        "expiresAt": expires_at,
        "device": { "id": row.device_id, "name": device_name },
    })))
}

fn link_invalid() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "link_invalid",
        "连接链接不存在、已过期或已被使用",
    )
}

/// 链接落地页 `GET /l/{token}`：唤起桌面客户端，或提示链接已失效。
///
/// 页面本身不消费令牌，只负责把服务器地址和令牌交给 `screenwall://` 协议；
/// 真正换令牌由客户端调用 `POST /api/v1/connect-link/use` 完成。
pub async fn link_page(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    let token = token.trim().to_string();
    let valid = state
        .db
        .connect_link_valid(&hash_token(&token))
        .unwrap_or(false);
    if valid {
        Html(LINK_PAGE_OK.replace("__TOKEN__", &token)).into_response()
    } else {
        Html(LINK_PAGE_BAD).into_response()
    }
}

/// 有效链接的落地页：唤起客户端（不自动跳转，避免协议未注册时浏览器报错）。
const LINK_PAGE_OK: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ScreenWall 远程连接</title>
<style>
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
         font-family: system-ui, "Segoe UI", "Microsoft YaHei", sans-serif;
         background: #0b0c0e; color: #e6e8eb; }
  .card { max-width: 460px; padding: 32px; background: #161a21; border: 1px solid #262c36;
          border-radius: 12px; text-align: center; }
  h1 { font-size: 18px; margin: 0 0 8px; }
  p { color: #9aa3b0; line-height: 1.6; margin: 8px 0; font-size: 14px; }
  button { margin-top: 16px; padding: 10px 22px; border: 0; border-radius: 8px;
           background: #2563eb; color: #fff; font-size: 15px; cursor: pointer; }
  button:hover { background: #1d4ed8; }
</style>
</head>
<body>
<div class="card">
  <h1>ScreenWall 远程连接</h1>
  <p>已准备好连接链接，点击下面的按钮在 ScreenWall 客户端中打开。</p>
  <button id="open" type="button">在 ScreenWall 中打开</button>
  <p>如果客户端没有自动打开，请点击上面的按钮；尚未安装的话，请先下载 ScreenWall 客户端，再用连接码连接。</p>
</div>
<script>
  const token = "__TOKEN__";
  const server = location.origin;
  document.getElementById("open").addEventListener("click", function () {
    window.location.href =
      "screenwall://connect?server=" + encodeURIComponent(server) +
      "&token=" + encodeURIComponent(token);
  });
</script>
</body>
</html>"#;

/// 链接失效的落地页。
const LINK_PAGE_BAD: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ScreenWall 连接链接已失效</title>
<style>
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
         font-family: system-ui, "Segoe UI", "Microsoft YaHei", sans-serif;
         background: #0b0c0e; color: #e6e8eb; }
  .card { max-width: 460px; padding: 32px; background: #161a21; border: 1px solid #262c36;
          border-radius: 12px; text-align: center; }
  h1 { font-size: 18px; margin: 0 0 8px; }
  p { color: #9aa3b0; line-height: 1.6; margin: 8px 0; font-size: 14px; }
</style>
</head>
<body>
<div class="card">
  <h1>连接链接已失效</h1>
  <p>这条连接链接不存在、已过期或已经被使用过。</p>
  <p>请向对方重新生成一条新的连接链接，或改用连接码 + 密码连接。</p>
</div>
</body>
</html>"#;

pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

#[cfg(test)]
mod tests {
    use super::normalize_connect_code;

    #[test]
    fn connect_code_accepts_grouped_digits() {
        assert_eq!(
            normalize_connect_code("572 955 600").as_deref(),
            Some("572955600")
        );
        assert_eq!(
            normalize_connect_code("572-955-600").as_deref(),
            Some("572955600")
        );
        assert_eq!(
            normalize_connect_code("572955600").as_deref(),
            Some("572955600")
        );
        assert!(normalize_connect_code("57295560").is_none());
        assert!(normalize_connect_code("5729556001").is_none());
        assert!(normalize_connect_code("057295560").is_none());
        assert!(normalize_connect_code("57295560a").is_none());
    }
}
