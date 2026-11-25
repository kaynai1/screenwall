//! HTTP API：注册、登录、设备管理。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
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
    })
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
        .upsert_device(user.id, &device_id, &name, &os, &version)
        .map_err(internal)?;

    let token = new_token();
    let expires_at = now_ms() + state.config.token_ttl_days * 86_400_000;
    state
        .db
        .create_token(&hash_token(&token), user.id, &device_id, expires_at)
        .map_err(internal)?;

    state.broadcast_devices(user.id).await;

    Ok(Json(json!({
        "token": token,
        "expiresAt": expires_at,
        "user": { "id": user.id, "username": user.username },
        "device": { "id": device_id, "name": name }
    })))
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;
    state.db.revoke_token(&auth.token_hash).map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;
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
    let auth = authorize(&state, &headers)?;

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
    authorize(&state, &headers)?;
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
    authorize(&state, &headers)?;
    let (active_sessions, online_devices) = state.live_counts().await;
    Ok(Json(json!({
        "onlineDevices": online_devices,
        "activeSessions": active_sessions,
        "totalBytes": state.metrics.total_bytes(),
        "errorCount": state.metrics.error_count(),
    })))
}

pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}
