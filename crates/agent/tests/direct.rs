//! 端到端：局域网直连桩。真实 TLS + WebSocket，走完握手、授权、开会话、订阅画面。
//!
//! 画面采集在测试环境可能失败（没有可用的桌面/编码器），所以订阅只断言
//! 「收到 video.sub.ok 或 error」——两者都证明 tunnel 帧通路是通的。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::direct::{self, DirectAuth};
use screenwall_agent::server::{AgentState, ApprovalFn, ApprovalRequest};
use screenwall_agent::tls;
use screenwall_proto::cloud::{decode_tunnel_frame, encode_tunnel_frame, TunnelKind};
use screenwall_proto::{
    Capabilities, DisplayInfo, Hello, Message, PeerInfo, StreamKind, VideoCodec, VideoSub,
    PROTO_VERSION,
};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

const TOKEN: &str = "pairing-token-for-test";

type ClientWs = WebSocketStream<TlsStream<TcpStream>>;

#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("screenwall-direct-{}-{tag}", std::process::id()))
}

struct Harness {
    port: u16,
    auth: Arc<DirectAuth>,
    requests:
        tokio::sync::mpsc::UnboundedReceiver<(u32, String, tokio::sync::oneshot::Sender<bool>)>,
}

async fn start_direct(tag: &str) -> Result<Harness> {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(&dir)?;
    let (acceptor, _fingerprint) = tls::ensure_acceptor(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let approval: ApprovalFn = Arc::new(move |request: ApprovalRequest| {
        let _ = request_tx.send((request.session, request.viewer, request.reply));
    });

    let state = Arc::new(AgentState {
        config: AgentConfig {
            name: "DIRECT-AGENT".into(),
            port,
            token: String::new(),
            allow_control: true,
            clipboard_sync: false,
            server_url: String::new(),
            account_token: String::new(),
            device_id: "direct-device".into(),
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
        approval: Some(approval),
    });

    let auth = Arc::new(DirectAuth::new());
    auth.allow(TOKEN.to_string()).await;

    let run_auth = auth.clone();
    tokio::spawn(async move {
        if let Err(error) = direct::run(
            listener,
            state,
            acceptor,
            run_auth,
            Arc::new(direct::AuthLimiter::new()),
        )
        .await
        {
            eprintln!("直连桩退出: {error}");
        }
    });

    Ok(Harness {
        port,
        auth,
        requests: request_rx,
    })
}

async fn connect(port: u16) -> Result<ClientWs> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    let tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("127.0.0.1")?, tcp)
        .await?;
    let (ws, _) =
        tokio_tungstenite::client_async(format!("wss://127.0.0.1:{port}/ws"), tls).await?;
    Ok(ws)
}

fn hello(token: &str) -> Message {
    Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: token.into(),
        client: PeerInfo {
            name: "DIRECT-CLIENT".into(),
            version: "0.1.0".into(),
        },
        caps: Capabilities::current(),
    })
}

/// 读下一条 JSON 文本帧，跳过二进制/心跳。
async fn next_json(ws: &mut ClientWs) -> Result<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow!("等待文本帧超时"))?
            .ok_or_else(|| anyhow!("连接已关闭"))??;
        match message {
            WsMessage::Text(text) => return Ok(serde_json::from_str(text.as_str())?),
            WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            // 二进制是会话帧，这个辅助函数不处理。
            WsMessage::Binary(_) => continue,
            other => return Err(anyhow!("意外的消息: {other:?}")),
        }
    }
}

async fn next_json_t(ws: &mut ClientWs, kind: &str) -> Result<Value> {
    loop {
        let value = next_json(ws).await?;
        if value.get("t").and_then(Value::as_str) == Some(kind) {
            return Ok(value);
        }
    }
}

/// 读属于指定会话的下一条 tunnel JSON 消息。
async fn next_inner(ws: &mut ClientWs, session: u32) -> Result<Message> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow!("等待会话帧超时"))?
            .ok_or_else(|| anyhow!("连接已关闭"))??;
        let data = match message {
            WsMessage::Binary(data) => data,
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Text(_) => continue,
            other => return Err(anyhow!("意外的消息: {other:?}")),
        };
        let (header, payload) = decode_tunnel_frame(&data)?;
        if header.session != session {
            continue;
        }
        if header.kind != TunnelKind::Json {
            continue;
        }
        return Ok(Message::parse(std::str::from_utf8(payload)?)?);
    }
}

async fn send_inner(ws: &mut ClientWs, session: u32, message: &Message) -> Result<()> {
    let frame = encode_tunnel_frame(TunnelKind::Json, session, message.to_json().as_bytes());
    ws.send(WsMessage::Binary(frame.into())).await?;
    Ok(())
}

#[tokio::test]
async fn direct_end_to_end() -> Result<()> {
    let mut harness = start_direct("e2e").await?;
    let mut ws = connect(harness.port).await?;

    // ---- 握手：hello → welcome / display.list / status
    ws.send(WsMessage::text(hello(TOKEN).to_json())).await?;

    let welcome = next_json_t(&mut ws, "welcome").await?;
    assert_eq!(welcome["agent"]["name"], "DIRECT-AGENT");
    assert_eq!(welcome["proto"], PROTO_VERSION);

    let displays = next_json_t(&mut ws, "display.list").await?;
    assert_eq!(displays["displays"][0]["w"], 1920);

    let status = next_json_t(&mut ws, "status").await?;
    assert_eq!(status["screen"]["w"], 1920);

    // ---- open：被控端弹授权，回 true 后才建会话
    ws.send(WsMessage::Text(
        json!({ "t": "open", "request": 7, "viewer": "Office" })
            .to_string()
            .into(),
    ))
    .await?;

    let (approved_session, viewer, reply) = harness
        .requests
        .recv()
        .await
        .ok_or_else(|| anyhow!("没有收到授权请求"))?;
    assert_eq!(viewer, "Office");

    // 真实主控端拿到 open.ok 后会立刻发 hello，此时本机还没点「允许」。
    // 先不批准，验证会话通道未登记时这些帧不会丢。
    let opened = next_json_t(&mut ws, "open.ok").await?;
    assert_eq!(opened["request"], 7);
    assert_eq!(opened["deviceId"], "local");
    let session = opened["session"]
        .as_u64()
        .ok_or_else(|| anyhow!("缺 session"))? as u32;
    assert_eq!(session, approved_session);
    send_inner(&mut ws, session, &hello("")).await?;

    // 稍等一下再批准，确保 hello 确实落在「授权中」的窗口里。
    tokio::time::sleep(Duration::from_millis(100)).await;
    reply.send(true).expect("答复允许");

    // 授权通过后配对 token 应被升成长期 token。
    // 带 request 的 open.ok 在读循环里立刻发出，提升发生在授权回调之后，轮询等一下。
    let mut promoted = false;
    for _ in 0..40 {
        if harness.auth.is_promoted(TOKEN).await {
            promoted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(promoted, "授权通过后 token 应被提升为长期 token");

    // ---- 会话内握手：hello 是授权期间发的，应被补投并得到 welcome
    match next_inner(&mut ws, session).await? {
        Message::Welcome(welcome) => assert_eq!(welcome.agent.name, "DIRECT-AGENT"),
        other => return Err(anyhow!("期望 welcome，收到 {other:?}")),
    }
    match next_inner(&mut ws, session).await? {
        Message::DisplayList(list) => assert_eq!(list.displays[0].w, 1920),
        other => return Err(anyhow!("期望 display.list，收到 {other:?}")),
    }
    match next_inner(&mut ws, session).await? {
        Message::Status(status) => assert_eq!(status.screen.w, 1920),
        other => return Err(anyhow!("期望 status，收到 {other:?}")),
    }

    // ---- 订阅画面：采集可能失败，但必须有一条回包证明帧通路是通的
    let sub = Message::VideoSub(VideoSub {
        stream: StreamKind::Focus,
        display: 0,
        fps: 10,
        max_width: Some(640),
        quality: Some(50),
        codec: VideoCodec::Mjpeg,
        cursor: false,
        max_bitrate: None,
    });
    send_inner(&mut ws, session, &sub).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("订阅画面没有收到任何回包"));
        }
        let message = tokio::time::timeout(remaining, next_inner(&mut ws, session)).await??;
        match message {
            Message::VideoSubOk(ok) => {
                assert_eq!(ok.actual.codec, VideoCodec::Mjpeg);
                break;
            }
            // 采集失败是允许的：说明订阅消息已到达会话任务并得到了处理。
            Message::Error(error) => {
                assert_eq!(error.code, screenwall_proto::codes::ENCODER_UNAVAILABLE);
                break;
            }
            // 5 秒状态推送会插队，跳过。
            Message::Status(_) => continue,
            other => return Err(anyhow!("期望 video.sub.ok 或 error，收到 {other:?}")),
        }
    }

    // ---- close 后会话应被移除
    ws.send(WsMessage::Text(
        json!({ "t": "close", "session": session })
            .to_string()
            .into(),
    ))
    .await?;
    // 给读循环一点时间处理关闭。
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

/// 直连的 welcome 也必须回双方交集，而不是本机全量。
///
/// 老主控端不带 `caps` → 按 v1 基线协商，welcome 里不能有 `audio.aec`；
/// 只声明一部分能力时，welcome 只回交集（本机不认识的 `future.thing` 不算数）。
#[tokio::test]
async fn direct_welcome_caps_are_intersection() -> Result<()> {
    let harness = start_direct("caps").await?;

    // 1) 老主控端：hello 没有 caps 字段。
    let mut ws = connect(harness.port).await?;
    let legacy = format!(
        r#"{{"t":"hello","proto":{PROTO_VERSION},"token":"{TOKEN}","client":{{"name":"OLD-CLIENT","version":"0.0.9"}}}}"#
    );
    ws.send(WsMessage::text(legacy)).await?;
    let welcome = next_json_t(&mut ws, "welcome").await?;
    let caps: Vec<String> = serde_json::from_value(welcome["caps"].clone())?;
    assert!(
        caps.iter().any(|cap| cap == "audio.volume"),
        "老主控端也应拿到 v1 基线能力：{caps:?}"
    );
    assert!(
        !caps.iter().any(|cap| cap == "audio.aec"),
        "老主控端不该被告知有 AEC：{caps:?}"
    );

    // 2) 只声明部分能力：welcome 只回交集。
    let mut ws = connect(harness.port).await?;
    let partial = Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: TOKEN.into(),
        client: PeerInfo {
            name: "PARTIAL-CLIENT".into(),
            version: "0.1.0".into(),
        },
        caps: Capabilities::new(["audio.opus", "future.thing"]),
    });
    ws.send(WsMessage::text(partial.to_json())).await?;
    let welcome = next_json_t(&mut ws, "welcome").await?;
    let caps: Vec<String> = serde_json::from_value(welcome["caps"].clone())?;
    assert_eq!(caps, vec!["audio.opus".to_string()]);

    Ok(())
}

/// 没有授权回调时必须拒绝：直连不允许免授权。
#[tokio::test]
async fn direct_without_approval_is_rejected() -> Result<()> {
    let dir = temp_dir("noapproval");
    std::fs::create_dir_all(&dir)?;
    let (acceptor, _) = tls::ensure_acceptor(&dir)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let state = Arc::new(AgentState {
        config: AgentConfig {
            name: "NO-APPROVAL".into(),
            port,
            token: String::new(),
            allow_control: true,
            clipboard_sync: false,
            server_url: String::new(),
            account_token: String::new(),
            device_id: "direct-device".into(),
            ca_path: String::new(),
            virtual_display: false,
        },
        displays: Vec::new(),
        gpu: String::new(),
        ip: String::new(),
        approval: None,
    });
    let auth = Arc::new(DirectAuth::new());
    auth.allow(TOKEN.to_string()).await;
    tokio::spawn(async move {
        let _ = direct::run(
            listener,
            state,
            acceptor,
            auth,
            Arc::new(direct::AuthLimiter::new()),
        )
        .await;
    });

    let mut ws = connect(port).await?;
    ws.send(WsMessage::text(hello(TOKEN).to_json())).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let item = tokio::time::timeout(remaining, ws.next()).await?;
        match item {
            Some(Ok(WsMessage::Close(frame))) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(0);
                assert_eq!(code, screenwall_proto::codes::AUTH_FAILED);
                return Ok(());
            }
            Some(Ok(WsMessage::Text(text))) => {
                if let Message::Error(error) = Message::parse(text.as_str())? {
                    assert_eq!(error.code, screenwall_proto::codes::AUTH_FAILED);
                    return Ok(());
                }
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Err(anyhow!("连接在返回错误前关闭")),
        }
    }
}

/// 不在白名单的 token 必须被拒。
#[tokio::test]
async fn direct_wrong_token_is_rejected() -> Result<()> {
    let harness = start_direct("badtoken").await?;
    let mut ws = connect(harness.port).await?;
    ws.send(WsMessage::text(hello("not-the-token").to_json()))
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let item = tokio::time::timeout(remaining, ws.next()).await?;
        match item {
            Some(Ok(WsMessage::Close(frame))) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(0);
                assert_eq!(code, screenwall_proto::codes::AUTH_FAILED);
                return Ok(());
            }
            Some(Ok(WsMessage::Text(text))) => {
                if let Message::Error(error) = Message::parse(text.as_str())? {
                    assert_eq!(error.code, screenwall_proto::codes::AUTH_FAILED);
                    return Ok(());
                }
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Err(anyhow!("连接在返回错误前关闭")),
        }
    }
}

/// 本机拒绝授权时，直连要补一条 `closed`，否则主控端会一直等 welcome。
#[tokio::test]
async fn direct_rejection_closes_session() -> Result<()> {
    let mut harness = start_direct("reject").await?;
    let mut ws = connect(harness.port).await?;
    ws.send(WsMessage::text(hello(TOKEN).to_json())).await?;
    let _ = next_json_t(&mut ws, "welcome").await?;

    ws.send(WsMessage::Text(
        json!({ "t": "open", "request": 3, "viewer": "Office" })
            .to_string()
            .into(),
    ))
    .await?;
    let (_, _, reply) = harness
        .requests
        .recv()
        .await
        .ok_or_else(|| anyhow!("没有收到授权请求"))?;
    reply.send(false).expect("答复拒绝");

    let opened = next_json_t(&mut ws, "open.ok").await?;
    assert_eq!(opened["request"], 3);
    let closed = next_json_t(&mut ws, "closed").await?;
    assert_eq!(closed["session"], opened["session"]);
    assert_eq!(closed["code"], screenwall_proto::codes::CONTROL_DISABLED);

    // 拒绝不应提升 token。
    assert!(!harness.auth.is_promoted(TOKEN).await);
    Ok(())
}

/// 长期 token 与配对 token 的集合语义。
#[tokio::test]
async fn auth_promote_and_replace() {
    let auth = DirectAuth::new();
    auth.allow("pair".into()).await;
    assert!(auth.is_allowed("pair").await);
    assert!(!auth.is_promoted("pair").await);

    auth.promote("pair").await;
    assert!(auth.is_allowed("pair").await);
    assert!(auth.is_promoted("pair").await);

    // 换配对码只替换长期集合：旧的长期 token 失效，新配对 token 仍在。
    auth.allow("next-pair".into()).await;
    auth.replace_all(["long".to_string()]).await;
    assert!(!auth.is_allowed("pair").await);
    assert!(auth.is_allowed("next-pair").await);
    assert!(auth.is_allowed("long").await);
    assert!(auth.is_promoted("long").await);

    auth.revoke("long").await;
    assert!(!auth.is_allowed("long").await);
}

#[tokio::test]
async fn promote_notifies_hook_once() {
    use std::sync::Mutex as StdMutex;

    let auth = DirectAuth::new();
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    auth.on_promote(Arc::new(move |token: &str| {
        sink.lock().unwrap().push(token.to_string());
    }));

    auth.allow("pair".into()).await;
    // 非长期集合的变化不该触发落盘。
    assert!(seen.lock().unwrap().is_empty());

    auth.promote("pair").await;
    auth.promote("pair").await; // 重复提升不应重复落盘
    assert_eq!(seen.lock().unwrap().as_slice(), ["pair".to_string()]);

    // 被吊销后重新提升，应该再通知一次（否则重启后会丢这条）。
    auth.revoke("pair").await;
    auth.promote("pair").await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn auth_limiter_backs_off_after_repeated_failures() {
    use std::net::{IpAddr, Ipv4Addr};

    let limiter = direct::AuthLimiter::new();
    let peer = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));

    // 未达上限时不该拦。
    for _ in 0..5 {
        assert!(limiter.blocked_for(peer).is_none());
        limiter.record_failure(peer);
    }
    // 第 5 次失败之后进入退避。
    let wait = limiter.blocked_for(peer).expect("超过上限后应被限流");
    assert!(wait > std::time::Duration::ZERO, "退避时长应大于 0");

    // 其他 IP 不受影响。
    let other = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 51));
    assert!(limiter.blocked_for(other).is_none());

    // 认证成功后清掉记录。
    limiter.record_success(peer);
    assert!(limiter.blocked_for(peer).is_none());
}

#[tokio::test]
async fn pairing_token_expires() {
    use std::time::Duration;

    let auth = DirectAuth::new();
    auth.allow_for("short".into(), Duration::from_millis(60))
        .await;
    assert!(auth.is_allowed("short").await, "未到期时应该放行");

    tokio::time::sleep(Duration::from_millis(90)).await;
    assert!(!auth.is_allowed("short").await, "到期后必须拒绝");
}

#[tokio::test]
async fn revoke_pairing_keeps_promoted_token() {
    let auth = DirectAuth::new();
    auth.allow("code-token".into()).await;
    auth.promote("code-token").await;
    assert!(auth.is_promoted("code-token").await);

    // 配对成功后轮换配对码：这个码派生的 token 不该再当「配对码」用，
    // 但已经授权过的那台设备必须还能连。
    auth.revoke_pairing("code-token").await;
    assert!(auth.is_allowed("code-token").await);
    assert!(auth.is_promoted("code-token").await);
    assert_eq!(auth.promoted_count().await, 1);
}
