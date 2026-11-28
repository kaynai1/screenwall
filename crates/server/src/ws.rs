//! WebSocket：被控端长连接、主控端长连接与会话中转。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use screenwall_proto::cloud::{decode_tunnel_frame, TunnelKind};
use screenwall_proto::frame::{decode_frame, FrameCodec};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::api::authorize_token;
use crate::auth::{now_ms, valid_device_id};
use crate::state::{AppState, DeviceConn, MediaSlot, Session, SessionInit, ViewerConn, WsOut};

const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const PING_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
pub struct DeviceParams {
    pub device_id: String,
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
    /// 本机识别码（9 位数字）：别人登录后也能凭它跨网段找到本机。
    #[serde(default)]
    pub local_id: Option<String>,
    /// `webrtc=1` 表示这台被控端支持网页端 WebRTC P2P。
    #[serde(default)]
    pub webrtc: Option<String>,
}

pub async fn device_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<DeviceParams>,
) -> Response {
    let auth = match authorize_token(&state, &headers, None) {
        Ok(auth) => auth,
        Err(error) => {
            state.metrics.handshake_failed();
            return error.into_response();
        }
    };
    if !valid_device_id(&params.device_id) {
        state.metrics.handshake_failed();
        return bad_request("设备 ID 格式无效");
    }
    if !auth.device_id.is_empty() && auth.device_id != params.device_id {
        state.metrics.handshake_failed();
        return forbidden("令牌与设备不匹配");
    }

    let name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("未命名设备")
        .chars()
        .take(64)
        .collect::<String>();
    let os = params.os.unwrap_or_default();
    let version = params.version.unwrap_or_default();
    let gpu = params.gpu.unwrap_or_default();
    let ip = params.ip.unwrap_or_default();
    let local_id = params.local_id.unwrap_or_default();

    // 与登录路径共用设备配额：新设备超限时明确拒绝，重复上线不受影响。
    if !state.device_quota_ok(auth.user_id, &params.device_id) {
        let max = state.max_devices();
        tracing::warn!(
            "设备配额已满，拒绝设备上线: user_id={} device={} limit={max}",
            auth.user_id,
            params.device_id
        );
        state.metrics.handshake_failed();
        state.metrics.add_error();
        return quota_exceeded(max);
    }

    if let Err(error) = state.db.upsert_device(
        auth.user_id,
        &params.device_id,
        &name,
        &os,
        &version,
        &gpu,
        &ip,
        &local_id,
    ) {
        tracing::error!("登记设备失败: {error:#}");
        state.metrics.handshake_failed();
        state.metrics.add_error();
        return internal_error();
    }

    state.metrics.handshake_ok();
    let webrtc = params.webrtc.as_deref() == Some("1");
    ws.on_upgrade(move |socket| {
        handle_device(socket, state, auth.user_id, params.device_id, name, webrtc)
    })
}

/// `/ws/view` 的查询参数：网页端无法给 WebSocket 设请求头，令牌只能放这里。
#[derive(Debug, Default, Deserialize)]
pub struct ViewParams {
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn view_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<ViewParams>,
) -> Response {
    let auth = match authorize_token(&state, &headers, params.token.as_deref()) {
        Ok(auth) => auth,
        Err(error) => {
            state.metrics.handshake_failed();
            return error.into_response();
        }
    };
    // 按账号限制建连频率，防止疯狂建会话拖垮服务器。
    let rate_key = format!("view:{}", auth.user_id);
    if let Some(seconds) = state.view_connect_rate.check(&rate_key) {
        tracing::warn!(
            "主控端建连过于频繁，已限流: user_id={} retry_after={seconds}s",
            auth.user_id
        );
        state.metrics.handshake_failed();
        state.metrics.add_error();
        return too_many_requests(seconds);
    }
    state.metrics.handshake_ok();
    let restricted = auth.quick.then(|| auth.target_device_id.clone());
    ws.on_upgrade(move |socket| {
        handle_view(socket, state, auth.user_id, auth.device_id, restricted)
    })
}

async fn handle_device(
    socket: WebSocket,
    state: Arc<AppState>,
    user_id: i64,
    device_id: String,
    name: String,
    webrtc: bool,
) {
    let key = AppState::device_key(user_id, &device_id);
    let (tx, rx) = mpsc::unbounded_channel::<WsOut>();
    let conn = DeviceConn {
        user_id,
        device_id: device_id.clone(),
        tx: tx.clone(),
        webrtc,
    };

    // 同一设备重连时，旧连接让位。
    if let Some(old) = state.devices.write().await.insert(key.clone(), conn) {
        let _ = old.tx.send(WsOut::Close);
        state
            .close_sessions_for_device(&key, 4201, "device reconnected")
            .await;
    }
    let _ = state.db.touch_device(user_id, &device_id);
    tracing::info!("设备上线: {name} ({device_id})");
    state.broadcast_devices(user_id).await;

    let _ = tx.send(WsOut::Text(
        json!({ "t": "ready", "serverTime": now_ms() }).to_string(),
    ));

    let (sink, stream) = socket.split();
    // 设备连接不往画面队列里放东西，给一个用不到的槽位即可。
    let writer = tokio::spawn(write_loop(sink, rx, Arc::new(MediaSlot::default())));
    let heartbeat = spawn_ping(tx.clone());

    let mut stream = stream;
    loop {
        let item = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        match item {
            Err(_) | Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Message::Text(text)))) => {
                handle_device_text(&state, user_id, &key, &tx, text.as_str()).await;
            }
            Ok(Some(Ok(Message::Binary(data)))) => {
                handle_device_binary(&state, &key, data.as_ref()).await;
            }
            Ok(Some(Ok(Message::Ping(payload)))) => {
                let _ = tx.send(WsOut::Pong(payload.to_vec()));
            }
            Ok(Some(Ok(Message::Pong(_)))) => {}
            Ok(Some(Ok(Message::Close(_)))) => break,
        }
    }

    heartbeat.abort();
    writer.abort();
    state.disconnect_device(user_id, &device_id, &tx).await;
}

async fn handle_device_text(
    state: &Arc<AppState>,
    user_id: i64,
    key: &str,
    tx: &mpsc::UnboundedSender<WsOut>,
    text: &str,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let kind = value.get("t").and_then(Value::as_str).unwrap_or_default();
    let session_id = value
        .get("session")
        .and_then(Value::as_u64)
        .map(|id| id as u32);

    match kind {
        "open.ok" => {
            let Some(session_id) = session_id else { return };
            let session = state.sessions.read().await.get(&session_id).cloned();
            let Some(session) = session else { return };
            if session.device_key != key {
                return;
            }
            // 设备接受会话，审计从此刻开始计时。
            state.start_session_audit(session_id, key).await;
            let message = json!({
                "t": "open.ok",
                "request": session.request,
                "session": session_id,
                "deviceId": session.device_id
            })
            .to_string();
            state.send_to_viewer(&session, WsOut::Text(message)).await;
        }
        "open.err" => {
            let Some(session_id) = session_id else { return };
            let code = value.get("code").and_then(Value::as_u64).unwrap_or(4204) as u16;
            let msg = value
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("device rejected session");
            let removed = {
                let mut sessions = state.sessions.write().await;
                let allowed = sessions
                    .get(&session_id)
                    .map(|session| session.device_key == key)
                    .unwrap_or(false);
                if allowed {
                    sessions.remove(&session_id)
                } else {
                    None
                }
            };
            if let Some(session) = removed {
                state.finish_session_audit(&session);
                let message = json!({
                    "t": "open.err",
                    "request": session.request,
                    "deviceId": session.device_id,
                    "code": code,
                    "msg": msg
                })
                .to_string();
                state.send_to_viewer(&session, WsOut::Text(message)).await;
            }
        }
        "close" => {
            let Some(session_id) = session_id else { return };
            let removed = {
                let mut sessions = state.sessions.write().await;
                let allowed = sessions
                    .get(&session_id)
                    .map(|session| session.device_key == key)
                    .unwrap_or(false);
                if allowed {
                    sessions.remove(&session_id)
                } else {
                    None
                }
            };
            if let Some(session) = removed {
                state.finish_session_audit(&session);
                let message = json!({
                    "t": "closed",
                    "session": session_id,
                    "code": 4201,
                    "msg": "session closed by device"
                })
                .to_string();
                state.send_to_viewer(&session, WsOut::Text(message)).await;
            }
        }
        "p2p" => {
            handle_device_p2p(state, user_id, key, &value).await;
        }
        "webrtc" => {
            handle_device_webrtc(state, user_id, key, &value).await;
        }
        "ping" => {
            let _ = tx.send(WsOut::Text(
                json!({ "t": "pong", "ts": now_ms() }).to_string(),
            ));
        }
        _ => {}
    }
}

/// 把被控端的 P2P 应答转回发起请求的那条主控连接。
///
/// 只在登记表里查得到、且登记的设备就是这台时才转发 —— 被控端不能凭空
/// 往别人的主控连接里塞消息。字段逐项重建，不原样透传。
async fn handle_device_p2p(state: &Arc<AppState>, user_id: i64, key: &str, value: &Value) {
    let (Some(request), Some(viewer_id)) = (
        value
            .get("request")
            .and_then(Value::as_u64)
            .map(|r| r as u32),
        value.get("viewerId").and_then(Value::as_u64),
    ) else {
        return;
    };
    let event = match value.get("event").and_then(Value::as_str) {
        Some("answer") => "answer",
        Some("reject") => "reject",
        _ => return,
    };
    if !state.take_p2p(viewer_id, request, key).await {
        return;
    }
    let viewer = state.viewers.read().await.get(&viewer_id).cloned();
    let Some(viewer) = viewer else { return };
    if viewer.user_id != user_id {
        return;
    }
    let message = if event == "answer" {
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
        let fingerprint = value
            .get("fingerprint")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if candidates.is_empty() || fingerprint.len() > 200 || token.is_empty() || token.len() > 200
        {
            return;
        }
        json!({
            "t": "p2p",
            "request": request,
            "event": "answer",
            "candidates": candidates,
            "fingerprint": fingerprint,
            "token": token
        })
    } else {
        let msg = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("device rejected p2p");
        json!({ "t": "p2p", "request": request, "event": "reject", "msg": msg })
    };
    let _ = viewer.tx.send(WsOut::Text(message.to_string()));
}

async fn handle_device_binary(state: &Arc<AppState>, key: &str, data: &[u8]) {
    let Ok((header, payload)) = decode_tunnel_frame(data) else {
        state.metrics.add_error();
        return;
    };
    if payload.len() > state.config.max_frame_bytes {
        state.metrics.add_error();
        return;
    }
    if header.kind != TunnelKind::Json && header.kind != TunnelKind::Binary {
        state.metrics.add_error();
        return;
    }
    let session = state.sessions.read().await.get(&header.session).cloned();
    if let Some(session) = session {
        if session.device_key == key {
            let bytes = payload.len() as u64;
            session.stats.down.fetch_add(bytes, Ordering::Relaxed);
            state.metrics.add_down(bytes);
            // 画面帧进「只留最新」的队列：主控端带宽不够时宁可丢帧，也别把
            // 延迟越积越大。应用消息（JSON）和音频等其余二进制帧仍按原样排队。
            let dropped = if header.kind == TunnelKind::Json {
                session
                    .viewer_tx
                    .send(WsOut::Binary(data.to_vec()))
                    .is_err()
            } else {
                match decode_frame(payload) {
                    Ok((frame, _)) => match frame.codec {
                        FrameCodec::Jpeg => {
                            session
                                .media
                                .put_jpeg(data.to_vec(), frame.stream_id, frame.seq)
                        }
                        FrameCodec::H264 => session.media.put_h264(
                            data.to_vec(),
                            frame.stream_id,
                            frame.seq,
                            frame.is_keyframe(),
                        ),
                        _ => session
                            .viewer_tx
                            .send(WsOut::Binary(data.to_vec()))
                            .is_err(),
                    },
                    // 认不出媒体头就按普通二进制帧原样转发：协议允许上层带着
                    // 自己的负载走隧道，不能因为服务器不认识就把它吞掉。
                    Err(_) => session
                        .viewer_tx
                        .send(WsOut::Binary(data.to_vec()))
                        .is_err(),
                }
            };
            if dropped {
                state.metrics.add_dropped();
            }
        }
    }
}

async fn handle_view(
    socket: WebSocket,
    state: Arc<AppState>,
    user_id: i64,
    viewer_device_id: String,
    restricted: Option<String>,
) {
    let viewer_id = state.alloc_viewer();
    let (tx, rx) = mpsc::unbounded_channel::<WsOut>();
    let media = Arc::new(MediaSlot::default());
    state.viewers.write().await.insert(
        viewer_id,
        ViewerConn {
            user_id,
            device_id: viewer_device_id.clone(),
            restricted: restricted.clone(),
            tx: tx.clone(),
            media: media.clone(),
        },
    );

    let _ = tx.send(WsOut::Text(
        json!({ "t": "ready", "serverTime": now_ms() }).to_string(),
    ));
    let devices = match &restricted {
        Some(device_id) => state.quick_devices_message(user_id, device_id).await,
        None => state.devices_message(user_id).await,
    };
    let _ = tx.send(WsOut::Text(devices));

    let (sink, stream) = socket.split();
    let writer = tokio::spawn(write_loop(sink, rx, media.clone()));
    let heartbeat = spawn_ping(tx.clone());

    let mut stream = stream;
    loop {
        let item = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        match item {
            Err(_) | Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Message::Text(text)))) => {
                handle_view_text(
                    &state,
                    viewer_id,
                    user_id,
                    &viewer_device_id,
                    restricted.as_deref(),
                    &tx,
                    text.as_str(),
                )
                .await;
            }
            Ok(Some(Ok(Message::Binary(data)))) => {
                handle_view_binary(&state, viewer_id, user_id, data.as_ref()).await;
            }
            Ok(Some(Ok(Message::Ping(payload)))) => {
                let _ = tx.send(WsOut::Pong(payload.to_vec()));
            }
            Ok(Some(Ok(Message::Pong(_)))) => {}
            Ok(Some(Ok(Message::Close(_)))) => break,
        }
    }

    heartbeat.abort();
    writer.abort();
    state.viewers.write().await.remove(&viewer_id);
    // 主控端已经走了，它没等到应答的 P2P 请求没有意义，别让被控端白打洞。
    state.clear_p2p_for_viewer(viewer_id).await;
    state.clear_webrtc_for_viewer(viewer_id).await;
    state.close_sessions_for_viewer(viewer_id).await;
}

async fn handle_view_text(
    state: &Arc<AppState>,
    viewer_id: u64,
    user_id: i64,
    viewer_device_id: &str,
    restricted: Option<&str>,
    tx: &mpsc::UnboundedSender<WsOut>,
    text: &str,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match value.get("t").and_then(Value::as_str).unwrap_or_default() {
        "open" => {
            let request = value.get("request").and_then(Value::as_u64).unwrap_or(0) as u32;
            // 按账号限制建会话频率（不按连接，避免重连绕过）：超限时回 open.err。
            let rate_key = format!("open:{user_id}");
            if let Some(seconds) = state.session_open_rate.check(&rate_key) {
                tracing::warn!(
                    "会话建立过于频繁，已限流: user_id={user_id} retry_after={seconds}s"
                );
                let device_id = value
                    .get("deviceId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                send_open_err(
                    state,
                    tx,
                    request,
                    device_id,
                    4205,
                    &format!("会话建立过于频繁，请 {seconds} 秒后重试"),
                );
                return;
            }
            let device_id = value
                .get("deviceId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let viewer_name = value
                .get("viewer")
                .and_then(Value::as_str)
                .unwrap_or("ScreenWall")
                .to_string();
            // wall：屏幕墙上的只读缩略图会话，不需要被控端逐次授权。
            let mode = match value.get("mode").and_then(Value::as_str) {
                Some("wall") => "wall",
                _ => "control",
            }
            .to_string();

            if !valid_device_id(&device_id) {
                send_open_err(state, tx, request, &device_id, 4200, "invalid device id");
                return;
            }
            // 连接码令牌只能打开自己换来的那一台设备。
            if let Some(allowed) = restricted {
                if device_id != allowed {
                    send_open_err(state, tx, request, &device_id, 4200, "device not allowed");
                    return;
                }
            }
            match state.db.device(user_id, &device_id) {
                Ok(Some(device)) => {
                    if !device.controllable {
                        send_open_err(state, tx, request, &device_id, 4204, "control disabled");
                        return;
                    }
                }
                Ok(None) => {
                    send_open_err(state, tx, request, &device_id, 4200, "device not found");
                    return;
                }
                Err(error) => {
                    tracing::error!("读取设备失败: {error:#}");
                    send_open_err(state, tx, request, &device_id, 4200, "device lookup failed");
                    return;
                }
            }

            let key = AppState::device_key(user_id, &device_id);
            let online = state.devices.read().await.contains_key(&key);
            if !online {
                send_open_err(state, tx, request, &device_id, 4200, "device offline");
                return;
            }

            let session_id = state.alloc_session();
            let media = {
                let viewers = state.viewers.read().await;
                viewers
                    .get(&viewer_id)
                    .map(|viewer| viewer.media.clone())
                    .unwrap_or_default()
            };
            let session = Session::new(SessionInit {
                user_id,
                device_key: key.clone(),
                device_id: device_id.clone(),
                controller_device_id: viewer_device_id.to_string(),
                viewer_id,
                viewer_tx: tx.clone(),
                media,
                request,
                mode: mode.clone(),
            });
            state.insert_session(session_id, session).await;

            let message = json!({
                "t": "open",
                "session": session_id,
                "viewer": viewer_name,
                "mode": mode,
                // 连接码 + 密码已经验证过身份，被控端不再逐次弹窗。
                "trusted": restricted.is_some(),
                // 普通账号会话：主控端登录的就是这台设备所属的账号，
                // 被控端可以跳过「允许控制」确认（总开关「允许被控」仍然优先）。
                "sameAccount": restricted.is_none()
            })
            .to_string();
            if !state.send_to_device(&key, WsOut::Text(message)).await {
                state.metrics.add_dropped();
                state.sessions.write().await.remove(&session_id);
                send_open_err(state, tx, request, &device_id, 4200, "device offline");
            }
        }
        "close" => {
            let Some(session_id) = value.get("session").and_then(Value::as_u64) else {
                return;
            };
            let session_id = session_id as u32;
            let removed = {
                let mut sessions = state.sessions.write().await;
                let allowed = sessions
                    .get(&session_id)
                    .map(|session| session.viewer_id == viewer_id)
                    .unwrap_or(false);
                if allowed {
                    sessions.remove(&session_id)
                } else {
                    None
                }
            };
            if let Some(session) = removed {
                state.finish_session_audit(&session);
                let message = json!({ "t": "close", "session": session_id }).to_string();
                state
                    .send_to_device(&session.device_key, WsOut::Text(message))
                    .await;
            }
        }
        "p2p" => {
            // 主控端想绕过中转直连被控端：只把候选地址转给设备，
            // 真正的打洞与传输都在两端之间完成，服务器不碰这条链路。
            handle_view_p2p(state, viewer_id, user_id, restricted, tx, &value).await;
        }
        "webrtc" => {
            handle_view_webrtc(state, viewer_id, user_id, restricted, tx, &value).await;
        }
        "ping" => {
            let _ = tx.send(WsOut::Text(
                json!({ "t": "pong", "ts": now_ms() }).to_string(),
            ));
        }
        _ => {}
    }
}

/// 转发一条 P2P 打洞请求到目标设备。
///
/// 校验和 `open` 一样严：设备要在本账号下、允许被控、在线，连接码会话
/// 还只能打自己那一台。通过了才登记 (主控连接, request)，设备回
/// `answer` 时凭这份登记把结果送回同一个主控连接。
async fn handle_view_p2p(
    state: &Arc<AppState>,
    viewer_id: u64,
    user_id: i64,
    restricted: Option<&str>,
    tx: &mpsc::UnboundedSender<WsOut>,
    value: &Value,
) {
    let Some(request) = value
        .get("request")
        .and_then(Value::as_u64)
        .map(|id| id as u32)
        .filter(|id| *id != 0)
    else {
        return;
    };
    let device_id = value
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_device_id(device_id) {
        return;
    }
    if let Some(allowed) = restricted {
        if device_id != allowed {
            return;
        }
    }
    match state.db.device(user_id, device_id) {
        Ok(Some(device)) if device.controllable => {}
        _ => return,
    }
    let key = AppState::device_key(user_id, device_id);
    if !state.devices.read().await.contains_key(&key) {
        return;
    }
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
    if !state.register_p2p(viewer_id, request, key.clone()).await {
        return;
    }
    let viewer_name = value
        .get("viewer")
        .and_then(Value::as_str)
        .unwrap_or("ScreenWall");
    let message = json!({
        "t": "p2p",
        "request": request,
        // 设备把这两个字段原样带回，服务器据此找回这条主控连接。
        "viewerId": viewer_id,
        "event": "offer",
        "viewer": viewer_name,
        "candidates": candidates
    })
    .to_string();
    if !state.send_to_device(&key, WsOut::Text(message)).await {
        // 设备刚好掉线：撤掉登记，明确告诉主控端走中转，别让它干等。
        state.take_p2p(viewer_id, request, &key).await;
        state.metrics.add_dropped();
        let _ = tx.send(WsOut::Text(
            json!({
                "t": "p2p",
                "request": request,
                "event": "reject",
                "msg": "device offline"
            })
            .to_string(),
        ));
    }
}

/// 网页端发起的 WebRTC 协商。
///
/// 与 QUIC 打洞不同，WebRTC 的 offer / answer / ICE 会来回多条消息：
/// 第一次 `offer` 时登记 `(viewer_id, request)`，之后只转发给登记过的设备。
async fn handle_view_webrtc(
    state: &Arc<AppState>,
    viewer_id: u64,
    user_id: i64,
    restricted: Option<&str>,
    tx: &mpsc::UnboundedSender<WsOut>,
    value: &Value,
) {
    if !state.webrtc.enabled {
        return;
    }
    let Some(request) = value
        .get("request")
        .and_then(Value::as_u64)
        .map(|id| id as u32)
        .filter(|id| *id != 0)
    else {
        return;
    };
    match value.get("event").and_then(Value::as_str).unwrap_or_default() {
        "offer" => {
            let device_id = value
                .get("deviceId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_device_id(device_id) {
                return;
            }
            if let Some(allowed) = restricted {
                if device_id != allowed {
                    return;
                }
            }
            match state.db.device(user_id, device_id) {
                Ok(Some(device)) if device.controllable => {}
                _ => return,
            }
            let key = AppState::device_key(user_id, device_id);
            if !state.devices.read().await.contains_key(&key) {
                return;
            }
            let Some(sdp) = webrtc_sdp(value.get("sdp")) else {
                return;
            };
            let viewer = value
                .get("viewer")
                .and_then(Value::as_str)
                .unwrap_or("网页端")
                .chars()
                .take(64)
                .collect::<String>();
            if !state
                .register_webrtc(viewer_id, request, key.clone())
                .await
            {
                return;
            }
            let message = json!({
                "t": "webrtc",
                "request": request,
                "viewerId": viewer_id,
                "event": "offer",
                "viewer": viewer,
                "sdp": sdp,
                "iceServers": crate::webrtc::ice_servers(&state.webrtc, user_id),
            })
            .to_string();
            if !state.send_to_device(&key, WsOut::Text(message)).await {
                state.finish_webrtc(viewer_id, request).await;
                let _ = tx.send(WsOut::Text(
                    json!({
                        "t": "webrtc",
                        "request": request,
                        "event": "reject",
                        "msg": "device offline"
                    })
                    .to_string(),
                ));
            }
        }
        "ice" => {
            let Some(ice) = webrtc_ice(value.get("ice")) else {
                return;
            };
            let Some(key) = state.webrtc_target(viewer_id, request).await else {
                return;
            };
            if !state.bump_webrtc(viewer_id, request).await {
                return;
            }
            let message = json!({
                "t": "webrtc",
                "request": request,
                "viewerId": viewer_id,
                "event": "ice",
                "ice": ice,
            })
            .to_string();
            let _ = state.send_to_device(&key, WsOut::Text(message)).await;
        }
        "bye" => {
            if let Some(key) = state.webrtc_target(viewer_id, request).await {
                let message = json!({
                    "t": "webrtc",
                    "request": request,
                    "viewerId": viewer_id,
                    "event": "bye",
                })
                .to_string();
                let _ = state.send_to_device(&key, WsOut::Text(message)).await;
            }
            state.finish_webrtc(viewer_id, request).await;
        }
        _ => {}
    }
}

/// 被控端回的 WebRTC 消息：answer / trickle ICE / reject / bye。
async fn handle_device_webrtc(state: &Arc<AppState>, user_id: i64, key: &str, value: &Value) {
    if !state.webrtc.enabled {
        return;
    }
    let (Some(request), Some(viewer_id)) = (
        value
            .get("request")
            .and_then(Value::as_u64)
            .map(|id| id as u32),
        value.get("viewerId").and_then(Value::as_u64),
    ) else {
        return;
    };
    let Some(target) = state.webrtc_target(viewer_id, request).await else {
        return;
    };
    if target != key {
        return;
    }
    let viewer = state.viewers.read().await.get(&viewer_id).cloned();
    let Some(viewer) = viewer else {
        state.finish_webrtc(viewer_id, request).await;
        return;
    };
    if viewer.user_id != user_id {
        return;
    }
    if !state.bump_webrtc(viewer_id, request).await {
        return;
    }
    let message = match value.get("event").and_then(Value::as_str).unwrap_or_default() {
        "answer" => {
            let Some(sdp) = webrtc_sdp(value.get("sdp")) else {
                return;
            };
            json!({
                "t": "webrtc",
                "request": request,
                "event": "answer",
                "sdp": sdp,
            })
        }
        "ice" => {
            let Some(ice) = webrtc_ice(value.get("ice")) else {
                return;
            };
            json!({
                "t": "webrtc",
                "request": request,
                "event": "ice",
                "ice": ice,
            })
        }
        "reject" => {
            state.finish_webrtc(viewer_id, request).await;
            let msg = value
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("device rejected webrtc")
                .chars()
                .take(128)
                .collect::<String>();
            json!({
                "t": "webrtc",
                "request": request,
                "event": "reject",
                "msg": msg,
            })
        }
        "bye" => {
            state.finish_webrtc(viewer_id, request).await;
            json!({
                "t": "webrtc",
                "request": request,
                "event": "bye",
            })
        }
        _ => return,
    };
    let _ = viewer.tx.send(WsOut::Text(message.to_string()));
}

/// SDP 只接受字符串、必须像一份 SDP；限制 16 KiB 防止被当成通用信使。
fn webrtc_sdp(value: Option<&Value>) -> Option<String> {
    let sdp = value?.as_str()?.trim();
    if sdp.len() < 8 || sdp.len() > 16 * 1024 {
        return None;
    }
    if !sdp.starts_with("v=0") {
        return None;
    }
    Some(sdp.to_string())
}

/// trickle ICE 的单个候选。
///
/// 浏览器给的是 `{candidate, sdpMid, sdpMLineIndex, usernameFragment}` 对象
/// （`addIceCandidate` 需要后两个字段），也兼容只有 `candidate` 字符串的老写法。
/// 逐字段重建，丢弃任何多余内容，并以 640 字节封顶。
fn webrtc_ice(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    let candidate = match value {
        Value::String(text) => text.trim(),
        Value::Object(map) => map.get("candidate")?.as_str()?.trim(),
        _ => return None,
    };
    if candidate.is_empty() || candidate.len() > 512 || !candidate.starts_with("candidate:") {
        return None;
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "candidate".to_string(),
        Value::String(candidate.to_string()),
    );
    if let Value::Object(map) = value {
        if let Some(mid) = map.get("sdpMid").and_then(Value::as_str) {
            if mid.len() <= 64 {
                out.insert("sdpMid".to_string(), Value::String(mid.to_string()));
            }
        }
        if let Some(index) = map.get("sdpMLineIndex").and_then(Value::as_u64) {
            if index < 16 {
                out.insert("sdpMLineIndex".to_string(), Value::from(index));
            }
        }
        if let Some(fragment) = map.get("usernameFragment").and_then(Value::as_str) {
            if fragment.len() <= 256 {
                out.insert(
                    "usernameFragment".to_string(),
                    Value::String(fragment.to_string()),
                );
            }
        }
    }
    Some(Value::Object(out))
}

async fn handle_view_binary(state: &Arc<AppState>, viewer_id: u64, user_id: i64, data: &[u8]) {
    let Ok((header, payload)) = decode_tunnel_frame(data) else {
        state.metrics.add_error();
        return;
    };
    if payload.len() > state.config.max_frame_bytes {
        state.metrics.add_error();
        return;
    }
    let session = state.sessions.read().await.get(&header.session).cloned();
    if let Some(session) = session {
        if session.viewer_id == viewer_id && session.user_id == user_id {
            // 网页端每渲染一帧回一条 `video.ack`：画面队列据此控制「在途」帧数。
            // 这条消息只给服务器用，不转给被控端。
            if header.kind == TunnelKind::Json {
                if let Some(seq) = video_ack_seq(payload) {
                    session.media.ack(seq);
                    return;
                }
            }
            let bytes = payload.len() as u64;
            session.stats.up.fetch_add(bytes, Ordering::Relaxed);
            state.metrics.add_up(bytes);
            if !state
                .send_to_device(&session.device_key, WsOut::Binary(data.to_vec()))
                .await
            {
                state.metrics.add_dropped();
            }
        }
    }
}

/// 解析网页端的 `video.ack`（`{ "t": "video.ack", "seq": n }`）。
///
/// 不是确认消息时返回 `None`；是确认但没带序号时返回 `Some(0)`（照旧吞掉，
/// 不转给被控端）。
fn video_ack_seq(payload: &[u8]) -> Option<u32> {
    // 先粗看内容，避免给每条输入消息都做一次 JSON 解析。
    if !payload.windows(9).any(|window| window == b"video.ack") {
        return None;
    }
    let value: Value = serde_json::from_slice(payload).ok()?;
    if value.get("t").and_then(Value::as_str) != Some("video.ack") {
        return None;
    }
    Some(value.get("seq").and_then(Value::as_u64).unwrap_or(0) as u32)
}

/// 回一条 `open.err`，并把错误码计入指标。
fn send_open_err(
    state: &AppState,
    tx: &mpsc::UnboundedSender<WsOut>,
    request: u32,
    device_id: &str,
    code: u16,
    msg: &str,
) {
    state.metrics.add_error_code(code);
    let message = json!({
        "t": "open.err",
        "request": request,
        "deviceId": device_id,
        "code": code,
        "msg": msg
    })
    .to_string();
    let _ = tx.send(WsOut::Text(message));
}

/// 一个连接只有一个写循环：控制消息和画面帧都从这里出。
///
/// 画面帧走 `MediaSlot`（只保留最新的，见 [`MediaSlot`] 的说明），控制消息
/// 走无界队列 —— 主控端慢的时候画面可以丢，`video.sub.ok` / `pong` 不能。
async fn write_loop<S>(mut sink: S, mut rx: mpsc::UnboundedReceiver<WsOut>, media: Arc<MediaSlot>)
where
    S: futures_util::Sink<Message> + Unpin,
{
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(out) = out else { break };
                let message = match out {
                    WsOut::Text(text) => Message::Text(text.into()),
                    WsOut::Binary(data) => Message::Binary(data.into()),
                    WsOut::Ping(data) => Message::Ping(data.into()),
                    WsOut::Pong(data) => Message::Pong(data.into()),
                    WsOut::Close => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                };
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            // 画面队列用短轮询而不是 Notify：丢帧/合并都发生在这个队列上，
            // 轮询只有「有新帧就发」一个语义，不会漏信号；5ms 的唤醒开销
            // 对一台服务器上同时观看的几路主控端可以忽略。
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                // 上一帧没被确认之前不再发下一帧：慢链路上只丢帧不够，
                // 内核和中间设备的缓冲照样会攒出好几秒的延迟。
                if media.ready() {
                    if let Some(frame) = media.take() {
                        if sink.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn spawn_ping(tx: mpsc::UnboundedSender<WsOut>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PING_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if tx.send(WsOut::Ping(Vec::new())).is_err() {
                break;
            }
        }
    })
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid_input", "msg": msg })),
    )
        .into_response()
}

fn forbidden(msg: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "forbidden", "msg": msg })),
    )
        .into_response()
}

/// 建连限流：429 + `Retry-After`。
fn too_many_requests(seconds: u64) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": "rate_limited",
            "msg": format!("建立连接过于频繁，请在 {seconds} 秒后重试")
        })),
    )
        .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

/// 设备配额已满：403 + 明确中文提示。
fn quota_exceeded(max: u32) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "device_limit_reached",
            "msg": format!("该账号最多绑定 {max} 台设备，请先在设备列表里删除不再使用的设备")
        })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "msg": "服务器内部错误" })),
    )
        .into_response()
}
