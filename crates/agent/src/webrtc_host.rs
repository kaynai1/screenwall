//! 网页端 WebRTC P2P（被控端这一半）。
//!
//! 浏览器是 offerer：它把 offer 经账号服务器递过来，这里回 answer，并在
//! DataChannel 上用**和中转完全一样的会话协议**跑控制/输入/文件；画面走
//! RTP 轨道（H.264），由浏览器的 `<video>` 直接播放。
//!
//! 设计要点：
//! - 信令走账号服务器（`t = "webrtc"`），只有本账号的主控端能发起，和
//!   中转会话语义一致，所以会话建立时按同账号自动放行；
//! - 没有 WebRTC 的旧客户端不会回 answer，服务器 60 秒后清登记，主控端回退中继；
//! - DataChannel 单条消息在 webrtc-rs 里限 16 KiB，媒体帧不进 DataChannel，
//!   文件块由网页端按小块发送。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::{Bytes, BytesMut};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::default_runtime;

use crate::cloud;
use crate::server::AgentState;

/// 同时协商 / 保持的 WebRTC 连接上限。正常只有一条，留点余量给刚失败的重试。
const MAX_ACTIVE: usize = 2;
/// 从 offer 到 DataChannel 可用的上限；超时当作建立失败，让主控端回退。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 授权等待期间缓存的会话帧上限（和中转/直连同一套思路）。
const PENDING_MAX_BYTES: usize = 512 * 1024;

/// 服务器递过来的后续信令（trickle ICE / 挂断）。
enum Signal {
    Ice(Value),
    Bye,
    Closed,
}

/// 被控端的 WebRTC 会话宿主，挂在账号连接上。
pub struct WebrtcHub {
    state: Arc<AgentState>,
    active: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Signal>>>>,
}

impl WebrtcHub {
    pub fn new(state: Arc<AgentState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            active: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 处理服务器转发来的 `t = "webrtc"` 消息。
    pub fn handle(&self, viewer_id: u64, value: &Value, out: &mpsc::UnboundedSender<WsMessage>) {
        let Some(request) = value
            .get("request")
            .and_then(Value::as_u64)
            .map(|id| id as u32)
            .filter(|id| *id != 0)
        else {
            return;
        };
        match value.get("event").and_then(Value::as_str).unwrap_or_default() {
            "offer" => self.start(viewer_id, request, value, out),
            "ice" => {
                let ice = value.get("ice").cloned().unwrap_or(Value::Null);
                self.signal(request, Signal::Ice(ice));
            }
            "bye" => self.signal(request, Signal::Bye),
            _ => {}
        }
    }

    fn signal(&self, request: u32, signal: Signal) {
        let tx = self.active.lock().unwrap().get(&request).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(signal);
        }
    }

    fn start(
        &self,
        viewer_id: u64,
        request: u32,
        value: &Value,
        out: &mpsc::UnboundedSender<WsMessage>,
    ) {
        if !self.state.config.allow_control {
            self.reject(viewer_id, request, out, "本机已关闭被控");
            return;
        }
        let Some(sdp) = value.get("sdp").and_then(Value::as_str).map(str::trim) else {
            return;
        };
        if !sdp.starts_with("v=0") || sdp.len() > 16 * 1024 {
            return;
        }
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&request) {
                return;
            }
            if active.len() >= MAX_ACTIVE {
                drop(active);
                self.reject(viewer_id, request, out, "本机正在处理其他直连请求");
                return;
            }
            active.insert(request, signal_tx.clone());
        }

        let viewer = value
            .get("viewer")
            .and_then(Value::as_str)
            .unwrap_or("网页端")
            .to_string();
        let ice_servers = value
            .get("iceServers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let state = self.state.clone();
        let active = self.active.clone();
        let out = out.clone();
        let sdp = sdp.to_string();
        tokio::spawn(async move {
            let opened = Arc::new(AtomicBool::new(false));
            let result = run_connection(
                state,
                request,
                viewer_id,
                viewer,
                sdp,
                ice_servers,
                out.clone(),
                signal_tx,
                signal_rx,
                opened.clone(),
            )
            .await;
            active.lock().unwrap().remove(&request);
            if let Err(error) = result {
                tracing::info!(
                    request,
                    viewer_id,
                    "网页端 WebRTC 未能建立，主控端会回退中继: {error:#}"
                );
            }
            if !opened.load(Ordering::Relaxed) {
                let _ = out.send(WsMessage::Text(
                    json!({
                        "t": "webrtc",
                        "request": request,
                        "viewerId": viewer_id,
                        "event": "reject",
                        "msg": "webrtc 未建立"
                    })
                    .to_string()
                    .into(),
                ));
            }
        });
    }

    fn reject(
        &self,
        viewer_id: u64,
        request: u32,
        out: &mpsc::UnboundedSender<WsMessage>,
        msg: &str,
    ) {
        let _ = out.send(WsMessage::Text(
            json!({
                "t": "webrtc",
                "request": request,
                "viewerId": viewer_id,
                "event": "reject",
                "msg": msg
            })
            .to_string()
            .into(),
        ));
    }
}

struct Handler {
    request: u32,
    viewer_id: u64,
    out: mpsc::UnboundedSender<WsMessage>,
    signal: mpsc::UnboundedSender<Signal>,
    sink: Arc<tokio::sync::Mutex<Option<MediaSinks>>>,
    state: Arc<AgentState>,
    opened: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(mut init) = event.candidate.to_json() else {
            return;
        };
        // rtc 的 `to_json()` 把 sdpMid 留成空串，浏览器会当作无效候选拒绝。
        // BUNDLE 下所有候选都挂在第一个 m-line 上，补成 "0"。
        if init.sdp_mid.as_deref().unwrap_or_default().is_empty() {
            init.sdp_mid = Some("0".to_string());
            init.sdp_mline_index = Some(0);
        }
        let Ok(ice) = serde_json::to_value(init) else {
            return;
        };
        let _ = self.out.send(WsMessage::Text(
            json!({
                "t": "webrtc",
                "request": self.request,
                "viewerId": self.viewer_id,
                "event": "ice",
                "ice": ice,
            })
            .to_string()
            .into(),
        ));
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            let _ = self.signal.send(Signal::Closed);
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let Ok(label) = data_channel.label().await else {
            return;
        };
        if label != "ctrl" {
            return;
        }
        self.opened.store(true, Ordering::Relaxed);
        let state = self.state.clone();
        let sink = self.sink.clone();
        let signal = self.signal.clone();
        let request = self.request;
        tokio::spawn(async move {
            tracing::debug!(request, "网页端 DataChannel 已打开");
            run_channel(state, data_channel, sink, signal).await;
        });
    }
}

/// 一条 WebRTC 连接：回 answer、处理 trickle ICE、把 H.264 写进视频轨道。
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    state: Arc<AgentState>,
    request: u32,
    viewer_id: u64,
    _viewer: String,
    sdp: String,
    ice_servers: Vec<Value>,
    out: mpsc::UnboundedSender<WsMessage>,
    signal: mpsc::UnboundedSender<Signal>,
    mut signals: mpsc::UnboundedReceiver<Signal>,
    opened: Arc<AtomicBool>,
) -> Result<()> {
    let mut media_engine = MediaEngine::default();
    // H.264 画面 + Opus 声音：轨道只承载这两种，MJPEG/PCM 继续走 DataChannel。
    let h264 = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_string(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    };
    media_engine.register_codec(h264.clone(), RtpCodecKind::Video)?;
    let opus = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_string(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    };
    media_engine.register_codec(opus.clone(), RtpCodecKind::Audio)?;
    let registry =
        register_default_interceptors(webrtc::peer_connection::Registry::new(), &mut media_engine)?;

    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(parse_ice_servers(&ice_servers)?)
        .build();
    let runtime = default_runtime().ok_or_else(|| anyhow!("没有可用的异步运行时"))?;
    // 绑到本机局域网地址（拿不到就退回通配）——STUN 反射地址要靠它算。
    let bind = if state.ip.parse::<std::net::IpAddr>().is_ok() {
        format!("{}:0", state.ip)
    } else {
        "0.0.0.0:0".to_string()
    };
    let sink: Arc<tokio::sync::Mutex<Option<MediaSinks>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let handler = Arc::new(Handler {
        request,
        viewer_id,
        out: out.clone(),
        signal: signal.clone(),
        sink: sink.clone(),
        state,
        opened: opened.clone(),
    });
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(vec![bind])
        .build()
        .await
        .context("创建 PeerConnection 失败")?;

    // 浏览器 offer 里已有 video / audio 的 m-line，这里只给存在的挂轨道；
    // 没有 m-line 就说明对端这一路不想要（例如只开音频），不能凭空加。
    let has_video = sdp.contains("m=video");
    let has_audio = sdp.contains("m=audio");
    let offer = RTCSessionDescription::offer(sdp).context("解析 offer SDP 失败")?;
    pc.set_remote_description(offer)
        .await
        .context("设置远端 offer 失败")?;

    let mut sinks = MediaSinks {
        video: None,
        audio: None,
    };
    if has_video {
        let track = local_track(&h264.rtp_codec, RtpCodecKind::Video)?;
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .context("挂载视频轨道失败")?;
        sinks.video = Some(TrackSink::new(track, sender, Duration::from_millis(33)));
    }
    if has_audio {
        let track = local_track(&opus.rtp_codec, RtpCodecKind::Audio)?;
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .context("挂载音频轨道失败")?;
        sinks.audio = Some(TrackSink::new(track, sender, Duration::from_millis(20)));
    }

    let answer = pc.create_answer(None).await.context("生成 answer 失败")?;
    pc.set_local_description(answer)
        .await
        .context("设置本地 answer 失败")?;
    if let Some(local) = pc.local_description().await {
        let _ = out.send(WsMessage::Text(
            json!({
                "t": "webrtc",
                "request": request,
                "viewerId": viewer_id,
                "event": "answer",
                "sdp": local.sdp,
            })
            .to_string()
            .into(),
        ));
    }
    *sink.lock().await = Some(sinks);

    // 建立阶段有超时；DataChannel 打开之后就一直等到对端 bye 或断开。
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline), if !opened.load(Ordering::Relaxed) => {
                tracing::debug!(request, "WebRTC 建立超时，关闭连接");
                break;
            }
            item = signals.recv() => match item {
                None | Some(Signal::Bye) | Some(Signal::Closed) => break,
                Some(Signal::Ice(ice)) => {
                    if let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(ice) {
                        if let Err(error) = pc.add_ice_candidate(init).await {
                            tracing::debug!(request, "添加远端 ICE 候选失败: {error}");
                        }
                    }
                }
            },
        }
    }
    *sink.lock().await = None;
    let _ = pc.close().await;
    Ok(())
}

/// 一条已协商的媒体轨道：视频走 H.264，声音走 Opus。
struct TrackSink {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    resolved: Option<(u32, u8)>,
    last_ts_us: Option<u64>,
    default_duration: Duration,
}

impl TrackSink {
    fn new(
        track: Arc<TrackLocalStaticSample>,
        sender: Arc<dyn RtpSender>,
        default_duration: Duration,
    ) -> Self {
        Self {
            track,
            sender,
            resolved: None,
            last_ts_us: None,
            default_duration,
        }
    }

    async fn push(&mut self, payload: &[u8], ts_us: u64) -> Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        if self.resolved.is_none() {
            let ssrc = *self
                .track
                .ssrcs()
                .await
                .first()
                .ok_or_else(|| anyhow!("轨道没有 SSRC（协商还没完成）"))?;
            let params = self.sender.get_parameters().await?;
            let payload_type = params
                .rtp_parameters
                .codecs
                .first()
                .map(|codec| codec.payload_type)
                .ok_or_else(|| anyhow!("轨道没有协商到编码"))?;
            self.resolved = Some((ssrc, payload_type));
        }
        let (ssrc, payload_type) = self.resolved.expect("刚填过");
        // 采样时长只影响 RTP 时间戳；视频用相邻两帧的时间差，丢帧时不漂。
        let duration = match self.last_ts_us {
            Some(prev) if ts_us > prev && ts_us - prev < 500_000 => {
                Duration::from_micros(ts_us - prev)
            }
            _ => self.default_duration,
        };
        self.last_ts_us = Some(ts_us);
        self.track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data: Bytes::copy_from_slice(payload),
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
        Ok(())
    }
}

/// 一条连接上的轨道集合；没有协商到的媒体为 `None`，对应帧继续走 DataChannel。
struct MediaSinks {
    video: Option<TrackSink>,
    audio: Option<TrackSink>,
}

/// 按编码建一条本地轨道（轨道号/流号只用于 SDP，和网页端无关）。
fn local_track(codec: &RTCRtpCodec, kind: RtpCodecKind) -> Result<Arc<TrackLocalStaticSample>> {
    let name = match kind {
        RtpCodecKind::Video => "video",
        RtpCodecKind::Audio => "audio",
        _ => "media",
    };
    Ok(Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "screenwall".to_string(),
            format!("screenwall-{name}"),
            "screenwall".to_string(),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(rand_ssrc()),
                    ..Default::default()
                },
                codec: codec.clone(),
                ..Default::default()
            }],
        ),
    )?))
}

/// DataChannel 上的会话：协议与中转一致（`open` / `close` / `ping` + 隧道帧）。
async fn run_channel(
    state: Arc<AgentState>,
    dc: Arc<dyn DataChannel>,
    sink: Arc<tokio::sync::Mutex<Option<MediaSinks>>>,
    signal: mpsc::UnboundedSender<Signal>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();
    let dc_writer = dc.clone();
    let sink_writer = sink.clone();
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            match message {
                WsMessage::Text(text) => {
                    if dc_writer.send_text(&text).await.is_err() {
                        break;
                    }
                }
                WsMessage::Binary(data) => {
                    // H.264 / Opus 走 RTP 轨道；MJPEG、PCM、文件块仍走
                    // DataChannel，协议和中转完全一致。
                    if let Ok((header, payload)) = screenwall_proto::frame::decode_frame(&data) {
                        use screenwall_proto::frame::{FrameCodec, FrameType};
                        let h264_video =
                            header.kind == FrameType::Video && header.codec == FrameCodec::H264;
                        let opus_audio =
                            header.kind == FrameType::Audio && header.codec == FrameCodec::Opus;
                        if h264_video || opus_audio {
                            let mut guard = sink_writer.lock().await;
                            let sink = guard.as_mut().and_then(|sinks| {
                                if h264_video {
                                    sinks.video.as_mut()
                                } else {
                                    sinks.audio.as_mut()
                                }
                            });
                            if let Some(sink) = sink {
                                if let Err(error) = sink.push(payload, header.ts_us).await {
                                    tracing::debug!("写媒体轨道失败: {error:#}");
                                }
                                continue;
                            }
                        } else if header.kind == FrameType::Video {
                            // 视频轨道已协商时不能把 MJPEG 塞进 DataChannel：
                            // 单帧远超通道单条消息上限，会把发送队列拖垮。
                            let has_video = sink_writer
                                .lock()
                                .await
                                .as_ref()
                                .map(|sinks| sinks.video.is_some())
                                .unwrap_or(false);
                            if has_video {
                                continue;
                            }
                        }
                    }
                    if dc_writer.send(BytesMut::from(&data[..])).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    let sessions = cloud::new_sessions();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let mut next_session: u32 = 1;

    while let Some(event) = dc.poll().await {
        match event {
            DataChannelEvent::OnMessage(message) => {
                if message.is_string {
                    let Ok(text) = String::from_utf8(message.data.to_vec()) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let session = value
                        .get("session")
                        .and_then(Value::as_u64)
                        .map(|id| id as u32);
                    match value.get("t").and_then(Value::as_str).unwrap_or_default() {
                        "open" => {
                            let request =
                                value.get("request").and_then(Value::as_u64).unwrap_or(0);
                            let viewer = value
                                .get("viewer")
                                .and_then(Value::as_str)
                                .unwrap_or("网页端")
                                .to_string();
                            let session = next_session;
                            next_session = next_session.wrapping_add(1).max(1);
                            // 与直连一致：先回带 request 的 open.ok，主控端据此
                            // 解析会话号；`open_session` 自己还会发一条服务器协议
                            // 意义上的确认。
                            let _ = tx.send(WsMessage::Text(
                                json!({
                                    "t": "open.ok",
                                    "request": request,
                                    "session": session,
                                    "deviceId": "local",
                                })
                                .to_string()
                                .into(),
                            ));
                            pending.lock().unwrap().entry(session).or_default();
                            let task_state = state.clone();
                            let task_out = tx.clone();
                            let task_sessions = sessions.clone();
                            tokio::spawn(async move {
                                // WebRTC 只走账号信令，能进来的都是同账号主控端，
                                // 和服务器中转的 sameAccount 语义一致，直接放行。
                                cloud::open_session(
                                    &task_state,
                                    session,
                                    viewer,
                                    false,
                                    true,
                                    true,
                                    &task_out,
                                    &task_sessions,
                                )
                                .await;
                            });
                            spawn_flush(session, sessions.clone(), pending.clone());
                        }
                        "close" => {
                            if let Some(session) = session {
                                cloud::close_session(&sessions, session);
                                pending.lock().unwrap().remove(&session);
                            }
                        }
                        "ping" => {
                            let _ = tx.send(WsMessage::Text(
                                json!({ "t": "pong", "ts": now_ms() }).to_string().into(),
                            ));
                        }
                        _ => {}
                    }
                } else {
                    let data = message.data.to_vec();
                    let Ok((header, _)) = screenwall_proto::cloud::decode_tunnel_frame(&data)
                    else {
                        continue;
                    };
                    let mut pending = pending.lock().unwrap();
                    if let Some(buffer) = pending.get_mut(&header.session) {
                        if data.len() <= screenwall_proto::cloud::TUNNEL_MAX_PAYLOAD
                            && buffer.bytes + data.len() <= PENDING_MAX_BYTES
                        {
                            buffer.bytes += data.len();
                            buffer.frames.push(data);
                        }
                        continue;
                    }
                    drop(pending);
                    cloud::dispatch(&data, &sessions);
                }
            }
            DataChannelEvent::OnClose | DataChannelEvent::OnClosing => break,
            _ => {}
        }
    }

    let open_sessions: Vec<u32> = sessions.lock().unwrap().keys().copied().collect();
    for session in open_sessions {
        cloud::close_session(&sessions, session);
    }
    pending.lock().unwrap().clear();
    let _ = signal.send(Signal::Closed);
}

/// 授权期间缓存下来的会话帧，按会话号分桶。
type Pending = Arc<Mutex<HashMap<u32, PendingBucket>>>;

#[derive(Default)]
struct PendingBucket {
    bytes: usize,
    frames: Vec<Vec<u8>>,
}

/// 会话登记进表之后，把等待期间收到的帧按原序补投。
fn spawn_flush(session: u32, sessions: cloud::Sessions, pending: Pending) {
    tokio::spawn(async move {
        for _ in 0..500 {
            if sessions.lock().unwrap().contains_key(&session) {
                let buffered = {
                    let mut pending = pending.lock().unwrap();
                    pending.remove(&session).unwrap_or_default()
                };
                for frame in buffered.frames {
                    cloud::dispatch(&frame, &sessions);
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pending.lock().unwrap().remove(&session);
    });
}

fn parse_ice_servers(values: &[Value]) -> Result<Vec<RTCIceServer>> {
    let mut servers = Vec::new();
    for value in values {
        let urls: Vec<String> = value
            .get("urls")
            .map(|urls| match urls {
                Value::String(url) => vec![url.clone()],
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        if urls.is_empty() || urls.len() > 8 {
            continue;
        }
        servers.push(RTCIceServer {
            urls,
            username: value
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            credential: value
                .get("credential")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
        if servers.len() >= 4 {
            break;
        }
    }
    // 没有 STUN/TURN 也能靠本机候选在局域网内连通；公网会失败并由上层回退。
    Ok(servers)
}

fn rand_ssrc() -> u32 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::getrandom(&mut bytes);
    u32::from_le_bytes(bytes).max(1)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
