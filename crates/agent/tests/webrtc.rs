//! 端到端：WebRTC 信令桩 + DataChannel 会话。
//!
//! 这里用 webrtc-rs 自己扮演浏览器（offerer），不需要真实浏览器：
//! 发 offer（两条 recvonly 媒体 m-line + 控制 DataChannel）→ 收被控端 answer →
//! trickle ICE 双向交换 → DataChannel 打开 → 发 `open` → 收 `open.ok`。
//! 媒体样本不发（没有采集设备），轨道只校验 answer 里的方向和编码。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::server::AgentState;
use screenwall_agent::webrtc_host::WebrtcHub;
use screenwall_proto::DisplayInfo;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use webrtc::data_channel::DataChannelEvent;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCPeerConnectionIceEvent,
};
use webrtc::runtime::default_runtime;

const VIEWER_ID: u64 = 77;
const REQUEST: u32 = 9;

struct Browser {
    ice: mpsc::UnboundedSender<RTCIceCandidateInit>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Browser {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.ice.send(init);
        }
    }
}

fn agent_state() -> Arc<AgentState> {
    Arc::new(AgentState {
        config: AgentConfig {
            name: "WEBRTC-AGENT".into(),
            port: 0,
            token: String::new(),
            allow_control: true,
            clipboard_sync: false,
            server_url: String::new(),
            account_token: String::new(),
            device_id: "webrtc-device".into(),
            local_id: String::new(),
            ca_path: String::new(),
            virtual_display: false,
        },
        displays: vec![DisplayInfo {
            index: 0,
            w: 1920,
            h: 1080,
            primary: true,
            x: 0,
            y: 0,
        }],
        gpu: String::new(),
        ip: String::new(),
        approval: None,
    })
}

/// 等 answer；在此之前收到的 ICE 候选先攒进 `pending`。
async fn wait_answer(
    rx: &mut mpsc::UnboundedReceiver<WsMessage>,
    pending: &mut Vec<RTCIceCandidateInit>,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let message = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .map_err(|_| anyhow!("等待 answer 超时"))?
            .ok_or_else(|| anyhow!("信令通道已关闭"))?;
        let WsMessage::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(text.as_str())?;
        match value.get("event").and_then(Value::as_str) {
            Some("answer") => return Ok(value),
            Some("ice") => {
                if let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(value["ice"].clone())
                {
                    pending.push(init);
                }
            }
            Some("reject") => bail!("被控端拒绝了 WebRTC：{value}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn webrtc_signaling_and_datachannel_session() -> Result<()> {
    let hub = WebrtcHub::new(agent_state());
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();

    // 浏览器侧：一条 video + 一条 audio 的 recvonly m-line，外加控制 DataChannel。
    // 媒体样本不发（测试机没有采集设备），只验证轨道协商到浏览器这边。
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
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
        },
        RtpCodecKind::Video,
    )?;
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RtpCodecKind::Audio,
    )?;
    let registry = register_default_interceptors(
        webrtc::peer_connection::Registry::new(),
        &mut media_engine,
    )?;
    let (ice_tx, mut ice_rx) = mpsc::unbounded_channel();
    let runtime = default_runtime().ok_or_else(|| anyhow!("没有异步运行时"))?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Browser { ice: ice_tx }))
        .with_runtime(runtime)
        // 和被控端一样绑通配地址：两边都只收本机网卡地址，回环地址凑不成候选对。
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .context("创建浏览器 PeerConnection 失败")?;
    let dc = pc
        .create_data_channel("ctrl", None)
        .await
        .context("创建 DataChannel 失败")?;
    for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
        pc.add_transceiver_from_kind(
            kind,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                streams: Vec::new(),
                send_encodings: Vec::new(),
            }),
        )
        .await
        .context("添加 transceiver 失败")?;
    }

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    let sdp = pc
        .local_description()
        .await
        .map(|description| description.sdp)
        .ok_or_else(|| anyhow!("没有本地 offer"))?;
    assert!(sdp.starts_with("v=0"));

    hub.handle(
        VIEWER_ID,
        &json!({
            "t": "webrtc",
            "request": REQUEST,
            "viewerId": VIEWER_ID,
            "event": "offer",
            "viewer": "测试浏览器",
            "sdp": sdp,
        }),
        &out_tx,
    );

    // answer 之前收到的候选先攒着（和网页端同样的处理）。
    let mut pending_ice: Vec<RTCIceCandidateInit> = Vec::new();
    let answer = wait_answer(&mut out_rx, &mut pending_ice).await?;
    let answer_sdp = answer
        .get("sdp")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("answer 没有 sdp"))?;
    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp.to_string())?)
        .await
        .context("设置 answer 失败")?;
    for ice in pending_ice.drain(..) {
        pc.add_ice_candidate(ice).await?;
    }

    // 被控端要把 video/audio 轨道挂回这两条 m-line：方向 sendonly、注册好的编码、
    // 以及轨道所属的 stream id。（webrtc-rs 只在收到 RTP 时才回调 on_track，
    // 本测试没有采集设备不出 RTP，所以这里直接检查 SDP。）
    assert!(answer_sdp.contains("m=video"), "answer 缺少视频 m-line");
    assert!(answer_sdp.contains("m=audio"), "answer 缺少音频 m-line");
    assert!(answer_sdp.contains("a=rtpmap:102 H264/90000"), "视频没绑 H.264: {answer_sdp}");
    assert!(answer_sdp.contains("a=rtpmap:111 opus/48000/2"), "音频没绑 Opus: {answer_sdp}");
    assert!(answer_sdp.contains("a=sendonly"), "媒体 m-line 不是 sendonly: {answer_sdp}");
    assert!(answer_sdp.contains("a=msid:screenwall"), "轨道没有 stream id: {answer_sdp}");

    // 浏览器候选 → 被控端；被控端候选 → 浏览器。
    let dc_events = {
        let dc_poll = dc.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = dc_poll.poll().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        rx
    };
    let mut dc_events = dc_events;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut opened = false;
    while !opened {
        tokio::select! {
            Some(ice) = ice_rx.recv() => {
                let ice = serde_json::to_value(ice)?;
                hub.handle(
                    VIEWER_ID,
                    &json!({
                        "t": "webrtc",
                        "request": REQUEST,
                        "viewerId": VIEWER_ID,
                        "event": "ice",
                        "ice": ice,
                    }),
                    &out_tx,
                );
            }
            Some(item) = out_rx.recv() => {
                let WsMessage::Text(text) = item else { continue };
                let value: Value = serde_json::from_str(text.as_str())?;
                match value.get("event").and_then(Value::as_str) {
                    Some("ice") => {
                        let init: RTCIceCandidateInit =
                            serde_json::from_value(value["ice"].clone())?;
                        pc.add_ice_candidate(init).await?;
                    }
                    Some("reject") => bail!("被控端拒绝了 WebRTC：{value}"),
                    _ => {}
                }
            }
            Some(event) = dc_events.recv() => {
                if matches!(event, DataChannelEvent::OnOpen) {
                    opened = true;
                }
            }
            _ = tokio::time::sleep_until(deadline) => bail!("DataChannel 没有打开"),
        }
    }

    // DataChannel 上走直连协议：open → open.ok（带 request 和会话号）。
    dc.send_text(&json!({ "t": "open", "request": 1, "viewer": "测试浏览器" }).to_string())
        .await?;
    let session = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let event = tokio::time::timeout_at(deadline, dc_events.recv())
                .await
                .map_err(|_| anyhow!("等待 open.ok 超时"))?
                .ok_or_else(|| anyhow!("DataChannel 已关闭"))?;
            let DataChannelEvent::OnMessage(message) = event else {
                continue;
            };
            if !message.is_string {
                continue;
            }
            let value: Value = serde_json::from_slice(&message.data)?;
            if value.get("t").and_then(Value::as_str) == Some("open.ok") {
                assert_eq!(value.get("request").and_then(Value::as_u64), Some(1));
                let session = value
                    .get("session")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("open.ok 没有会话号"))?;
                break session;
            }
            if let Some("open.err") = value.get("t").and_then(Value::as_str) {
                bail!("会话被拒绝：{value}");
            }
        }
    };
    assert!(session > 0, "会话号应该从 1 开始");

    dc.send_text(&json!({ "t": "close", "session": session }).to_string())
        .await?;
    let _ = dc.close().await;
    let _ = pc.close().await;
    Ok(())
}
