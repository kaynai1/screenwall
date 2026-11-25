//! 群控：一次操作广播到 N 台设备。
//!
//! 每台被选中的设备各开一条正常控制会话（被控端仍会弹一次授权），
//! 画面用低帧率的 `wall` 流（2–3 FPS、640–960 宽）铺在群控台的卡片里；
//! 键鼠输入按归一化坐标广播给所有会话，各设备按自己的分辨率落点。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use screenwall_proto::cloud::TunnelKind;
use screenwall_proto::{
    decode_frame, Capabilities, FrameCodec, FrameType, Hello, InputBatch, InputEvent, Message,
    PeerInfo, StreamKind, VideoCodec, VideoSub, PROTO_VERSION,
};
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::{hostname, send_tunnel_json, Shared, ViewOut, APPROVAL_TIMEOUT_SECS, MAX_INPUT_BATCH};

/// 打开会话最多等多久：被控端授权上限 30 秒，再加传输余量。
const OPEN_TIMEOUT: Duration = Duration::from_secs(APPROVAL_TIMEOUT_SECS + 10);
/// 单帧过大直接丢弃，避免把 WebView 拖死。
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 群控画质档位，和前端「流畅 / 均衡 / 高清」一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub fps: u16,
    pub max_width: u32,
    pub quality: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: 2,
            max_width: 640,
            quality: 60,
        }
    }
}

impl Options {
    pub fn normalized(self) -> Self {
        Self {
            fps: self.fps.clamp(1, 5),
            max_width: self.max_width.clamp(320, 960),
            quality: self.quality.clamp(30, 85),
        }
    }
}

#[derive(Default)]
struct Member {
    session: Option<u32>,
    stream: Option<u16>,
    /// 会话已握手，可以接收输入。
    ready: bool,
    opening: bool,
    state: String,
    message: Option<String>,
    width: u32,
    height: u32,
    last_frame: Option<Instant>,
    last_attempt: Option<Instant>,
}

#[derive(Default)]
pub struct Hub {
    /// 正在运行的群控批次；0 表示未开始。
    pub token: u64,
    options: Options,
    desired: Vec<String>,
    members: HashMap<String, Member>,
    by_session: HashMap<u32, String>,
}

fn emit_state(app: &AppHandle, token: u64, device: &str, member: &Member) {
    let _ = app.emit(
        "group-state",
        json!({
            "token": token,
            "deviceId": device,
            "state": member.state,
            "message": member.message,
            "width": member.width,
            "height": member.height,
        }),
    );
}

fn set_state(app: &AppHandle, shared: &Shared, device: &str, state: &str, message: Option<String>) {
    let (token, member) = {
        let mut hub = shared.group.lock().unwrap();
        let token = hub.token;
        let member = hub.members.get_mut(device);
        match member {
            Some(member) => {
                member.state = state.to_string();
                member.message = message;
                (
                    token,
                    Member {
                        session: member.session,
                        stream: member.stream,
                        ready: member.ready,
                        opening: member.opening,
                        state: member.state.clone(),
                        message: member.message.clone(),
                        width: member.width,
                        height: member.height,
                        last_frame: member.last_frame,
                        last_attempt: member.last_attempt,
                    },
                )
            }
            None => return,
        }
    };
    if token != 0 {
        emit_state(app, token, device, &member);
    }
}

async fn close_session(shared: &Shared, session: u32) {
    if let Some(out) = shared.view_tx.read().await.clone() {
        let text = json!({ "t": "close", "session": session }).to_string();
        let _ = out.send(ViewOut::Text(text));
    }
}

/// 开始或调整群控：`device_ids` 是当前要群控的全部设备，多退少补。
pub async fn start(
    app: &AppHandle,
    shared: &Arc<Shared>,
    device_ids: Vec<String>,
    options: Options,
) -> Result<u64, String> {
    if shared.config.read().await.token.is_empty() {
        return Err("尚未登录".into());
    }
    if !*shared.connected.read().await {
        return Err("服务器尚未连接".into());
    }

    let mut close = Vec::new();
    let mut open = Vec::new();
    let token;
    {
        let mut hub = shared.group.lock().unwrap();
        let mut desired: Vec<String> = Vec::new();
        for id in device_ids {
            if !id.is_empty() && !desired.contains(&id) {
                desired.push(id);
            }
        }
        if hub.token == 0 {
            if desired.is_empty() {
                return Ok(0);
            }
            hub.token = shared.next_group.fetch_add(1, Ordering::Relaxed).max(1);
        }
        let token_now = hub.token;
        token = token_now;
        hub.options = options.normalized();
        hub.desired = desired.clone();

        let unwanted: Vec<String> = hub
            .members
            .keys()
            .filter(|id| !desired.contains(id))
            .cloned()
            .collect();
        for id in unwanted {
            if let Some(member) = hub.members.remove(&id) {
                if let Some(session) = member.session {
                    hub.by_session.remove(&session);
                    close.push(session);
                }
            }
        }
        for id in &desired {
            let member = hub.members.entry(id.clone()).or_default();
            if member.session.is_none() && !member.opening {
                member.opening = true;
                member.last_attempt = Some(Instant::now());
                member.state = "connecting".into();
                member.message = Some("等待被控端授权…".into());
                open.push(id.clone());
            }
        }
    }
    for session in close {
        close_session(shared, session).await;
    }

    let (token, state) = {
        let hub = shared.group.lock().unwrap();
        (
            token,
            hub.desired
                .iter()
                .filter_map(|id| hub.members.get(id).map(|m| (id.clone(), snapshot(m))))
                .collect::<Vec<_>>(),
        )
    };
    for (device, member) in state {
        emit_state(app, token, &device, &member);
    }
    tracing::info!(token, opening = open.len(), "群控批次更新");
    for device in open {
        let app = app.clone();
        let shared = shared.clone();
        tauri::async_runtime::spawn(async move {
            open_one(&app, &shared, &device).await;
        });
    }
    Ok(token)
}

fn snapshot(member: &Member) -> Member {
    Member {
        session: member.session,
        stream: member.stream,
        ready: member.ready,
        opening: member.opening,
        state: member.state.clone(),
        message: member.message.clone(),
        width: member.width,
        height: member.height,
        last_frame: member.last_frame,
        last_attempt: member.last_attempt,
    }
}

async fn open_one(app: &AppHandle, shared: &Arc<Shared>, device: &str) {
    let (token, options) = {
        let hub = shared.group.lock().unwrap();
        (hub.token, hub.options)
    };
    if token == 0 {
        return;
    }

    let request = shared
        .next_request
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    let (tx, rx) = tokio::sync::oneshot::channel();
    shared.pending_opens.lock().unwrap().insert(request, tx);

    let out = shared.view_tx.read().await.clone();
    let Some(out) = out else {
        shared.pending_opens.lock().unwrap().remove(&request);
        fail(app, shared, device, "主控连接已断开");
        return;
    };
    let text = json!({
        "t": "open",
        "request": request,
        "deviceId": device,
        "viewer": hostname(),
        "mode": "control"
    })
    .to_string();
    if out.send(ViewOut::Text(text)).is_err() {
        shared.pending_opens.lock().unwrap().remove(&request);
        fail(app, shared, device, "主控连接已断开");
        return;
    }

    let session = match tokio::time::timeout(OPEN_TIMEOUT, rx).await {
        Ok(Ok(Ok(session))) => session,
        Ok(Ok(Err(message))) => {
            fail(app, shared, device, &message);
            return;
        }
        _ => {
            shared.pending_opens.lock().unwrap().remove(&request);
            fail(app, shared, device, "被控端未确认（等待超时）");
            return;
        }
    };

    let keep = {
        let mut hub = shared.group.lock().unwrap();
        if hub.token != token || !hub.desired.iter().any(|id| id == device) {
            false
        } else {
            let member = hub.members.entry(device.to_string()).or_default();
            if member.session.is_some() {
                // 并发打开时只保留先到的会话。
                false
            } else {
                member.session = Some(session);
                member.stream = None;
                member.ready = false;
                member.opening = false;
                member.state = "connecting".into();
                member.message = Some("已授权，正在建立画面…".into());
                member.last_frame = Some(Instant::now());
                hub.by_session.insert(session, device.to_string());
                true
            }
        }
    };
    if !keep {
        tracing::info!(session, device, "群控会话不再需要，立即关闭");
        close_session(shared, session).await;
        return;
    }
    tracing::info!(session, device, token, "群控会话已建立");
    set_state(
        app,
        shared,
        device,
        "connecting",
        Some("已授权，正在建立画面…".into()),
    );

    let hello = Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: String::new(),
        client: PeerInfo {
            name: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        caps: Capabilities::current(),
    });
    let sub = Message::VideoSub(VideoSub {
        stream: StreamKind::Wall,
        display: 0,
        fps: options.fps,
        max_width: Some(options.max_width),
        quality: Some(options.quality),
        codec: VideoCodec::Mjpeg,
        cursor: false,
        max_bitrate: None,
    });
    if !send_tunnel_json(&out, session, &hello) || !send_tunnel_json(&out, session, &sub) {
        {
            let mut hub = shared.group.lock().unwrap();
            hub.by_session.remove(&session);
            if let Some(member) = hub.members.get_mut(device) {
                member.session = None;
                member.stream = None;
                member.ready = false;
            }
        }
        close_session(shared, session).await;
        fail(app, shared, device, "发送订阅失败");
        return;
    }
    let mut hub = shared.group.lock().unwrap();
    if let Some(member) = hub.members.get_mut(device) {
        member.ready = true;
    }
}

fn fail(app: &AppHandle, shared: &Shared, device: &str, message: &str) {
    {
        let mut hub = shared.group.lock().unwrap();
        if let Some(member) = hub.members.get_mut(device) {
            member.opening = false;
            member.ready = false;
            member.session = None;
            member.stream = None;
            member.state = "error".into();
            member.message = Some(message.to_string());
        }
    }
    set_state(app, shared, device, "error", Some(message.to_string()));
}

/// 处理群控会话里的 JSON / 视频帧；返回 true 表示这条消息属于群控会话。
pub fn on_binary(
    app: &AppHandle,
    shared: &Shared,
    session: u32,
    kind: TunnelKind,
    payload: &[u8],
) -> bool {
    let device = {
        let hub = shared.group.lock().unwrap();
        match hub.by_session.get(&session) {
            Some(device) => device.clone(),
            None => return false,
        }
    };

    if kind == TunnelKind::Json {
        let Ok(text) = std::str::from_utf8(payload) else {
            return true;
        };
        let Ok(message) = Message::parse(text) else {
            return true;
        };
        match message {
            Message::VideoSubOk(ok) => {
                if ok.stream != StreamKind::Wall {
                    return true;
                }
                let changed = {
                    let mut hub = shared.group.lock().unwrap();
                    if hub.by_session.get(&session).map(String::as_str) != Some(device.as_str()) {
                        return true;
                    }
                    match hub.members.get_mut(&device) {
                        Some(member) => {
                            member.stream = Some(ok.id);
                            member.width = ok.actual.w;
                            member.height = ok.actual.h;
                            member.state = "live".into();
                            member.message = None;
                            member.last_frame = Some(Instant::now());
                            true
                        }
                        None => false,
                    }
                };
                if changed {
                    set_state(app, shared, &device, "live", None);
                }
            }
            Message::Error(error) => {
                set_state(app, shared, &device, "error", Some(error.msg));
            }
            _ => {}
        }
        return true;
    }

    let Ok((frame, jpeg)) = decode_frame(payload) else {
        return true;
    };
    if frame.kind != FrameType::Video || frame.codec != FrameCodec::Jpeg {
        return true;
    }
    if jpeg.len() > MAX_FRAME_BYTES {
        return true;
    }

    let (token, width, height) = {
        let mut hub = shared.group.lock().unwrap();
        let token = hub.token;
        let Some(member) = hub.members.get_mut(&device) else {
            return true;
        };
        if member.stream != Some(frame.stream_id) {
            return true;
        }
        member.last_frame = Some(Instant::now());
        (token, member.width, member.height)
    };

    let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let _ = app.emit(
        "group-frame",
        json!({
            "token": token,
            "deviceId": device,
            "session": session,
            "seq": frame.seq,
            "width": width,
            "height": height,
            "dataUrl": format!("data:image/jpeg;base64,{encoded}"),
        }),
    );
    true
}

/// 会话被对端结束：不自动重开（重开需要重新授权），交给界面处理。
pub fn on_closed(app: &AppHandle, shared: &Shared, session: u32) {
    let device = {
        let mut hub = shared.group.lock().unwrap();
        let Some(device) = hub.by_session.remove(&session) else {
            return;
        };
        if let Some(member) = hub.members.get_mut(&device) {
            member.session = None;
            member.stream = None;
            member.ready = false;
            member.state = "closed".into();
            member.message = Some("会话已结束".into());
        }
        device
    };
    set_state(app, shared, &device, "closed", Some("会话已结束".into()));
}

/// 定期检查：画面长时间不更新时标记为异常（不自动重开，重开需要重新授权）。
pub fn tick(app: &AppHandle, shared: &Shared, stale_after: Duration) {
    let stale: Vec<String> = {
        let mut hub = shared.group.lock().unwrap();
        let now = Instant::now();
        hub.members
            .iter_mut()
            .filter(|(_, member)| member.state == "live")
            .filter(|(_, member)| {
                member
                    .last_frame
                    .is_none_or(|at| now.duration_since(at) >= stale_after)
            })
            .map(|(id, member)| {
                member.state = "error".into();
                member.message = Some("画面已停止更新".into());
                member.stream = None;
                id.clone()
            })
            .collect()
    };
    for device in stale {
        set_state(app, shared, &device, "error", Some("画面已停止更新".into()));
    }
}

/// 主控连接断开：清掉会话占位并通知界面；重新连上后由界面重新开始。
pub fn reset_sessions(app: &AppHandle, shared: &Shared) {
    let (token, devices) = {
        let mut hub = shared.group.lock().unwrap();
        let token = hub.token;
        let mut devices = Vec::new();
        for (id, member) in hub.members.iter_mut() {
            member.session = None;
            member.stream = None;
            member.ready = false;
            member.opening = false;
            member.last_attempt = None;
            if member.state == "live" || member.state == "connecting" {
                member.state = "closed".into();
                member.message = Some("主控连接已断开".into());
                devices.push(id.clone());
            }
        }
        hub.by_session.clear();
        hub.token = 0;
        (token, devices)
    };
    if token == 0 {
        return;
    }
    for device in devices {
        let _ = app.emit(
            "group-state",
            json!({
                "token": token,
                "deviceId": device,
                "state": "closed",
                "message": "主控连接已断开",
                "width": 0,
                "height": 0,
            }),
        );
    }
}

/// 登录 / 退出登录时整体清空。
pub fn clear(shared: &Shared) {
    let mut hub = shared.group.lock().unwrap();
    hub.token = 0;
    hub.desired.clear();
    hub.members.clear();
    hub.by_session.clear();
}

pub async fn stop(app: &AppHandle, shared: &Arc<Shared>, only_token: Option<u64>) {
    let (token, sessions, devices) = {
        let mut hub = shared.group.lock().unwrap();
        if hub.token == 0 {
            return;
        }
        if let Some(token) = only_token {
            if token != hub.token {
                return;
            }
        }
        let token = hub.token;
        hub.token = 0;
        hub.desired.clear();
        let sessions: Vec<u32> = hub.by_session.keys().copied().collect();
        let devices: Vec<String> = hub
            .members
            .iter()
            .map(|(id, member)| (id.clone(), snapshot(member)))
            .filter(|(_, member)| member.session.is_some())
            .map(|(id, _)| id)
            .collect();
        hub.members.clear();
        hub.by_session.clear();
        (token, sessions, devices)
    };
    for session in sessions {
        close_session(shared, session).await;
    }
    for device in devices {
        let _ = app.emit(
            "group-state",
            json!({
                "token": token,
                "deviceId": device,
                "state": "closed",
                "message": "群控已停止",
                "width": 0,
                "height": 0,
            }),
        );
    }
}

/// 修改画质档位并让所有在线会话重新订阅。
pub async fn set_options(
    app: &AppHandle,
    shared: &Arc<Shared>,
    token: u64,
    options: Options,
) -> Result<(), String> {
    let (sessions, options) = {
        let mut hub = shared.group.lock().unwrap();
        if hub.token == 0 || hub.token != token {
            return Err("群控尚未开始或已停止".into());
        }
        hub.options = options.normalized();
        let sessions: Vec<u32> = hub
            .members
            .values()
            .filter(|member| member.ready && member.session.is_some())
            .filter_map(|member| member.session)
            .collect();
        (sessions, hub.options)
    };
    let out = shared
        .view_tx
        .read()
        .await
        .clone()
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    for session in sessions {
        let sub = Message::VideoSub(VideoSub {
            stream: StreamKind::Wall,
            display: 0,
            fps: options.fps,
            max_width: Some(options.max_width),
            quality: Some(options.quality),
            codec: VideoCodec::Mjpeg,
            cursor: false,
            max_bitrate: None,
        });
        let _ = send_tunnel_json(&out, session, &sub);
    }
    let _ = app;
    Ok(())
}

/// 广播键鼠输入；`target` 为空时发给所有已握手会话。
pub async fn send_input(
    shared: &Arc<Shared>,
    token: u64,
    events: Vec<InputEvent>,
    target: Option<String>,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    if events.len() > MAX_INPUT_BATCH {
        return Err(format!(
            "单次输入事件过多（{} > {MAX_INPUT_BATCH}）",
            events.len()
        ));
    }
    let sessions: Vec<u32> = {
        let hub = shared.group.lock().unwrap();
        if hub.token == 0 || hub.token != token {
            return Err("群控尚未开始或已停止".into());
        }
        hub.members
            .iter()
            .filter(|(id, member)| {
                target.as_ref().is_none_or(|wanted| wanted == *id)
                    && member.ready
                    && member.session.is_some()
            })
            .filter_map(|(_, member)| member.session)
            .collect()
    };
    if sessions.is_empty() {
        return Ok(());
    }
    let out = shared
        .view_tx
        .read()
        .await
        .clone()
        .ok_or_else(|| "主控连接尚未就绪".to_string())?;
    for session in sessions {
        let message = Message::Input(InputBatch {
            events: events.clone(),
        });
        let _ = send_tunnel_json(&out, session, &message);
    }
    Ok(())
}

// ---------------------------------------------------------------- 命令

#[tauri::command]
pub async fn start_group(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    device_ids: Vec<String>,
    fps: u16,
    max_width: u32,
    quality: u8,
) -> Result<u64, String> {
    let shared = shared.inner().clone();
    start(
        &app,
        &shared,
        device_ids,
        Options {
            fps,
            max_width,
            quality,
        },
    )
    .await
}

#[tauri::command]
pub async fn set_group_options(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    token: u64,
    fps: u16,
    max_width: u32,
    quality: u8,
) -> Result<(), String> {
    let shared = shared.inner().clone();
    set_options(
        &app,
        &shared,
        token,
        Options {
            fps,
            max_width,
            quality,
        },
    )
    .await
}

#[tauri::command]
pub async fn stop_group(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    token: Option<u64>,
) -> Result<(), String> {
    let shared = shared.inner().clone();
    stop(&app, &shared, token).await;
    Ok(())
}

#[tauri::command]
pub async fn group_input(
    shared: State<'_, Arc<Shared>>,
    token: u64,
    events: Vec<InputEvent>,
    target: Option<String>,
) -> Result<(), String> {
    let shared = shared.inner().clone();
    send_input(&shared, token, events, target).await
}
