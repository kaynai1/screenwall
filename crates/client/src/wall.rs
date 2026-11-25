//! 屏幕墙缩略图：给每台在线设备开一条只读会话（1 FPS、320px 宽），
//! 把 JPEG 帧以 `wall-frame` 事件推给前端。
//!
//! 和控制会话互不影响：这里用 `mode: "wall"` 打开，被控端不会弹授权，
//! 但会忽略一切输入/文件/剪贴板消息，只允许 `video.sub`。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use screenwall_proto::cloud::TunnelKind;
use screenwall_proto::{
    decode_frame, Capabilities, FrameCodec, FrameType, Hello, Message, PeerInfo, StreamKind,
    VideoCodec, VideoSub, VideoSubOk, PROTO_VERSION,
};
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::{hostname, send_tunnel_json, Shared, ViewOut};

/// 多久收不到帧就认为预览断了。
const STALE_AFTER: Duration = Duration::from_secs(15);
/// 失败后多久重试。
const RETRY_AFTER: Duration = Duration::from_secs(8);
/// 单台设备的缩略图参数。
const WALL_FPS: u16 = 1;
const WALL_WIDTH: u32 = 320;
const WALL_QUALITY: u8 = 60;

#[derive(Default)]
struct Sub {
    session: Option<u32>,
    stream: Option<u16>,
    width: u32,
    height: u32,
    last_frame: Option<Instant>,
    /// 会话建立时刻，用来判断「开了但一直没帧」。
    session_at: Option<Instant>,
    last_attempt: Option<Instant>,
    opening: bool,
    error: Option<String>,
    /// 被控端 `status` 里的整机指标；屏幕墙卡片上的 CPU / 内存条用它。
    metrics: Option<(f32, f32)>,
}

/// 屏幕墙预览状态。
#[derive(Default)]
pub struct Hub {
    desired: Vec<String>,
    subs: HashMap<String, Sub>,
    by_session: HashMap<u32, String>,
}

fn emit_state(app: &AppHandle, device: &str, state: &str, message: Option<&str>) {
    let _ = app.emit(
        "wall-state",
        json!({
            "deviceId": device,
            "state": state,
            "message": message,
        }),
    );
}

async fn close_session(shared: &Shared, session: u32) {
    if let Some(out) = shared.view_tx.read().await.clone() {
        let text = json!({ "t": "close", "session": session }).to_string();
        let _ = out.send(ViewOut::Text(text));
    }
}

fn fail(app: &AppHandle, shared: &Shared, device: &str, message: &str) {
    if let Some(sub) = shared.wall.lock().unwrap().subs.get_mut(device) {
        sub.opening = false;
        sub.session = None;
        sub.stream = None;
        sub.error = Some(message.to_string());
    }
    emit_state(app, device, "error", Some(message));
}

/// 前端告知当前需要预览哪些设备；多退少补。
///
/// 本机自己的 ID 一律剔除：屏幕墙是「看别人」的地方，把自己的桌面推给自己
/// 既没有意义，也意味着本机被控端要白白开一路采集。前端已经过滤过一遍，
/// 这里再挡一道，避免以后换调用方时又漏掉。
pub async fn set_preview(app: &AppHandle, shared: &Arc<Shared>, device_ids: Vec<String>) {
    let self_id = shared.config.read().await.device_id.clone();
    let mut close = Vec::new();
    {
        let mut hub = shared.wall.lock().unwrap();
        let mut desired: Vec<String> = Vec::new();
        for id in device_ids {
            if id.is_empty() || (!self_id.is_empty() && id == self_id) {
                continue;
            }
            if !desired.contains(&id) {
                desired.push(id);
            }
        }
        hub.desired = desired.clone();
        let unwanted: Vec<String> = hub
            .subs
            .keys()
            .filter(|id| !desired.contains(id))
            .cloned()
            .collect();
        for id in unwanted {
            if let Some(sub) = hub.subs.remove(&id) {
                if let Some(session) = sub.session {
                    hub.by_session.remove(&session);
                    close.push(session);
                }
            }
        }
    }
    for session in close {
        close_session(shared, session).await;
    }
    ensure(app, shared).await;
}

/// 离开屏幕墙：断开所有缩略图会话。
pub async fn stop_preview(shared: &Arc<Shared>) {
    let close = {
        let mut hub = shared.wall.lock().unwrap();
        hub.desired.clear();
        let sessions: Vec<u32> = hub.by_session.keys().copied().collect();
        hub.subs.clear();
        hub.by_session.clear();
        sessions
    };
    for session in close {
        close_session(shared, session).await;
    }
}

/// 登录/退出登录时整体清空（视图连接本身也会重建）。
pub fn clear(shared: &Shared) {
    let mut hub = shared.wall.lock().unwrap();
    hub.desired.clear();
    hub.subs.clear();
    hub.by_session.clear();
}

/// 视图连接断开：保留「想看哪些设备」，但清掉会话，重连后由 tick 重新打开。
pub fn reset_sessions(shared: &Shared) {
    let mut hub = shared.wall.lock().unwrap();
    hub.by_session.clear();
    for sub in hub.subs.values_mut() {
        sub.session = None;
        sub.stream = None;
        sub.session_at = None;
        sub.opening = false;
        sub.last_attempt = None;
        sub.error = None;
        sub.metrics = None;
    }
}

/// 设备主动结束会话（或服务器清理）。
pub fn on_closed(app: &AppHandle, shared: &Shared, session: u32) {
    let device = {
        let mut hub = shared.wall.lock().unwrap();
        let Some(device) = hub.by_session.remove(&session) else {
            return;
        };
        if let Some(sub) = hub.subs.get_mut(&device) {
            sub.session = None;
            sub.stream = None;
            sub.session_at = None;
        }
        device
    };
    emit_state(app, &device, "closed", Some("预览会话已结束"));
}

/// 为还没会话的设备发起预览连接。
pub async fn ensure(app: &AppHandle, shared: &Arc<Shared>) {
    let started: Vec<String>;
    {
        let mut hub = shared.wall.lock().unwrap();
        // 想要的设备先占位，下面才能统一按「无会话」批量打开。
        for id in hub.desired.clone() {
            hub.subs.entry(id).or_default();
        }
        let now = Instant::now();
        let open: Vec<String> = hub
            .subs
            .iter_mut()
            .filter(|(_, sub)| {
                sub.session.is_none()
                    && !sub.opening
                    && sub
                        .last_attempt
                        .is_none_or(|at| now.duration_since(at) >= RETRY_AFTER)
            })
            .map(|(id, sub)| {
                sub.opening = true;
                sub.last_attempt = Some(now);
                id.clone()
            })
            .collect();
        started = open;
    }
    for device in started {
        let app = app.clone();
        let shared = shared.clone();
        tauri::async_runtime::spawn(async move {
            open_one(&app, &shared, &device).await;
        });
    }
}

async fn open_one(app: &AppHandle, shared: &Arc<Shared>, device: &str) {
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
        fail(app, shared, device, "主控连接尚未就绪");
        return;
    };
    let text = json!({
        "t": "open",
        "request": request,
        "deviceId": device,
        "viewer": hostname(),
        "mode": "wall"
    })
    .to_string();
    if out.send(ViewOut::Text(text)).is_err() {
        shared.pending_opens.lock().unwrap().remove(&request);
        fail(app, shared, device, "主控连接已断开");
        return;
    }

    let session = match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(Ok(session))) => session,
        Ok(Ok(Err(message))) => {
            fail(app, shared, device, &message);
            return;
        }
        _ => {
            shared.pending_opens.lock().unwrap().remove(&request);
            fail(app, shared, device, "设备未响应");
            return;
        }
    };

    let keep = {
        let mut hub = shared.wall.lock().unwrap();
        let wanted = hub.desired.iter().any(|id| id == device);
        // 并发打开（例如前端重复挂载）时只保留先到的会话，后到的立即关掉。
        let taken = hub
            .subs
            .get(device)
            .is_some_and(|sub| sub.session.is_some());
        if !wanted || taken {
            false
        } else {
            let sub = hub.subs.entry(device.to_string()).or_default();
            sub.session = Some(session);
            sub.stream = None;
            sub.session_at = Some(Instant::now());
            sub.opening = false;
            sub.error = None;
            hub.by_session.insert(session, device.to_string());
            true
        }
    };
    if !keep {
        close_session(shared, session).await;
        return;
    }

    emit_state(app, device, "connecting", None);
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
        fps: WALL_FPS,
        max_width: Some(WALL_WIDTH),
        quality: Some(WALL_QUALITY),
        codec: VideoCodec::Mjpeg,
        cursor: false,
        max_bitrate: None,
    });
    if !send_tunnel_json(&out, session, &hello) || !send_tunnel_json(&out, session, &sub) {
        {
            let mut hub = shared.wall.lock().unwrap();
            hub.by_session.remove(&session);
            if let Some(entry) = hub.subs.get_mut(device) {
                entry.session = None;
                entry.stream = None;
            }
        }
        close_session(shared, session).await;
        fail(app, shared, device, "发送订阅失败");
    }
}

/// 处理会话里的 JSON / 视频帧；返回 true 表示这条消息属于缩略图会话。
pub fn on_binary(
    app: &AppHandle,
    shared: &Shared,
    session: u32,
    kind: TunnelKind,
    payload: &[u8],
) -> bool {
    let device = {
        let hub = shared.wall.lock().unwrap();
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
            Message::VideoSubOk(ok) => apply_sub_ok(app, shared, &device, session, ok),
            Message::Status(status) => {
                // 只更新指标，不动会话状态：屏幕墙卡片靠它显示真实的 CPU / 内存。
                let updated = {
                    let mut hub = shared.wall.lock().unwrap();
                    match hub.subs.get_mut(&device) {
                        Some(sub) => {
                            sub.metrics = Some((status.stats.cpu, status.stats.mem));
                            true
                        }
                        None => false,
                    }
                };
                if updated {
                    let _ = app.emit(
                        "wall-metrics",
                        json!({
                            "deviceId": device,
                            "cpu": status.stats.cpu,
                            "mem": status.stats.mem,
                            "kbps": status.stats.kbps,
                        }),
                    );
                }
            }
            Message::Error(error) => {
                if let Some(sub) = shared.wall.lock().unwrap().subs.get_mut(&device) {
                    sub.error = Some(error.msg.clone());
                }
                emit_state(app, &device, "error", Some(&error.msg));
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

    let (width, height) = {
        let mut hub = shared.wall.lock().unwrap();
        let Some(sub) = hub.subs.get_mut(&device) else {
            return true;
        };
        // 记录流 id 之后，其他流的帧一律忽略。
        if sub.stream != Some(frame.stream_id) {
            return true;
        }
        sub.last_frame = Some(Instant::now());
        sub.error = None;
        (sub.width, sub.height)
    };

    let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let _ = app.emit(
        "wall-frame",
        json!({
            "deviceId": device,
            "width": width,
            "height": height,
            "dataUrl": format!("data:image/jpeg;base64,{encoded}"),
        }),
    );
    true
}

fn apply_sub_ok(app: &AppHandle, shared: &Shared, device: &str, session: u32, ok: VideoSubOk) {
    if ok.stream != StreamKind::Wall {
        return;
    }
    {
        let mut hub = shared.wall.lock().unwrap();
        if hub.by_session.get(&session).map(String::as_str) != Some(device) {
            return;
        }
        if let Some(sub) = hub.subs.get_mut(device) {
            sub.stream = Some(ok.id);
            sub.width = ok.actual.w;
            sub.height = ok.actual.h;
            sub.error = None;
        }
    }
    emit_state(app, device, "live", None);
}

/// 定期检查：断流重开、失败的设备退避重试。
pub async fn tick(app: &AppHandle, shared: &Arc<Shared>) {
    let mut close = Vec::new();
    let mut lost = Vec::new();
    {
        let mut hub = shared.wall.lock().unwrap();
        let now = Instant::now();
        let ids: Vec<String> = hub.subs.keys().cloned().collect();
        for id in ids {
            let Some(sub) = hub.subs.get_mut(&id) else {
                continue;
            };
            let Some(session) = sub.session else { continue };
            let since = sub
                .last_frame
                .or(sub.session_at)
                .map_or(Duration::ZERO, |at| now.duration_since(at));
            if since < STALE_AFTER {
                continue;
            }
            sub.session = None;
            sub.stream = None;
            sub.session_at = None;
            sub.error = Some("画面已停止更新".into());
            hub.by_session.remove(&session);
            close.push(session);
            lost.push(id);
        }
    }
    for session in close {
        close_session(shared, session).await;
    }
    for device in lost {
        emit_state(app, &device, "error", Some("画面已停止更新"));
    }
    ensure(app, shared).await;
}

// ---------------------------------------------------------------- 命令

#[tauri::command]
pub async fn set_wall_preview(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    device_ids: Vec<String>,
) -> Result<(), String> {
    let shared = shared.inner().clone();
    set_preview(&app, &shared, device_ids).await;
    Ok(())
}

#[tauri::command]
pub async fn stop_wall_preview(shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    let shared = shared.inner().clone();
    stop_preview(&shared).await;
    Ok(())
}

/// 主控连接重连后立即恢复预览（不等下一轮 tick）。
pub async fn resume(app: &AppHandle, shared: &Arc<Shared>) {
    ensure(app, shared).await;
}
