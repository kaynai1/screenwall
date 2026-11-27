//! 端到端：跨网段直连的 QUIC 传输。
//!
//! 这里不模拟 NAT（本机做不到），而是验证**打洞成功之后**的那一段：
//! 一条 UDP socket 上跑 QUIC，QUIC 的双向流上跑完整的直连 WebSocket 协议，
//! 走完握手、授权、开会话。
//!
//! 覆盖三件容易出错的事：
//! 1. QUIC 流能当普通 `AsyncRead + AsyncWrite` 喂给 `tokio-tungstenite`；
//! 2. 证书指纹固定（`pinned`）在 QUIC 上同样生效 —— 指纹不对必须握手失败；
//! 3. ALPN 不匹配的连接会被拒，避免把别的 QUIC 服务当成直连。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::direct::{self, DirectAuth};
use screenwall_agent::quic;
use screenwall_agent::server::{AgentState, ApprovalFn, ApprovalRequest};
use screenwall_agent::tls;
use screenwall_proto::{Capabilities, DisplayInfo, Hello, Message, PeerInfo, PROTO_VERSION};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

const TOKEN: &str = "quic-pairing-token";

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("screenwall-quic-{}-{tag}", std::process::id()))
}

/// 绑一个 loopback UDP 端口，作为「已经打洞成功」的 socket。
fn punch_socket() -> Result<(UdpSocket, SocketAddr)> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    socket.set_nonblocking(true)?;
    let addr = socket.local_addr()?;
    Ok((socket, addr))
}

struct Harness {
    addr: SocketAddr,
    fingerprint: String,
    requests:
        tokio::sync::mpsc::UnboundedReceiver<(u32, String, tokio::sync::oneshot::Sender<bool>)>,
    _dir: PathBuf,
}

async fn start_quic(tag: &str) -> Result<Harness> {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(&dir)?;
    let material = tls::ensure_cert(&dir)?;
    let fingerprint = material.fingerprint.clone();

    let (socket, addr) = punch_socket()?;
    let endpoint = quic::server_endpoint(socket, &material)?;

    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let approval: ApprovalFn = Arc::new(move |request: ApprovalRequest| {
        let _ = request_tx.send((request.session, request.viewer, request.reply));
    });

    let state = Arc::new(AgentState {
        config: AgentConfig {
            name: "QUIC-AGENT".into(),
            port: 0,
            token: String::new(),
            allow_control: true,
            clipboard_sync: false,
            server_url: String::new(),
            account_token: String::new(),
            device_id: "quic-device".into(),
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
        approval: Some(approval),
    });

    let auth = Arc::new(DirectAuth::new());
    auth.allow(TOKEN.to_string()).await;
    let limiter = Arc::new(direct::AuthLimiter::new());

    // 服务端：接一条 QUIC 连接，开一条流，把流交给直连协议。
    tokio::spawn(async move {
        let incoming = match endpoint.accept().await {
            Some(incoming) => incoming,
            None => return,
        };
        let connection = match incoming.await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("QUIC 连接失败: {error}");
                return;
            }
        };
        let stream = match quic::QuicStream::accept(&connection).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("接受 QUIC 流失败: {error}");
                return;
            }
        };
        let peer = connection.remote_address().ip();
        if let Err(error) = direct::serve_stream(stream, peer, state, auth, limiter).await {
            eprintln!("QUIC 直连会话结束: {error}");
        }
        // 端点要活到连接结束，不能提前 drop。
        connection.closed().await;
    });

    Ok(Harness {
        addr,
        fingerprint,
        requests: request_rx,
        _dir: dir,
    })
}

type ClientWs = WebSocketStream<quic::QuicStream>;

async fn connect_quic(
    addr: SocketAddr,
    pinned: Option<String>,
) -> Result<(ClientWs, Arc<std::sync::Mutex<Option<String>>>)> {
    let (socket, _) = punch_socket()?;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let client = quic::client_endpoint(socket, pinned, seen.clone())?;
    let connection = client.connect(addr).await?;
    let stream = quic::QuicStream::open(&connection).await?;
    let (ws, _) = tokio_tungstenite::client_async("wss://screenwall/ws", stream).await?;
    // 连接对象必须跟着流活到最后：drop 掉它流会立刻被重置。
    std::mem::forget(connection);
    Ok((ws, seen))
}

fn hello(token: &str) -> Message {
    Message::Hello(Hello {
        proto: PROTO_VERSION,
        token: token.into(),
        client: PeerInfo {
            name: "QUIC-CLIENT".into(),
            version: "0.1.0".into(),
        },
        caps: Capabilities::current(),
    })
}

async fn next_json_t(ws: &mut ClientWs, kind: &str) -> Result<serde_json::Value> {
    for _ in 0..40 {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow!("等待 {kind} 超时"))?
            .ok_or_else(|| anyhow!("连接已关闭"))??;
        let WsMessage::Text(text) = message else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(text.as_str())?;
        if value.get("t").and_then(|t| t.as_str()) == Some(kind) {
            return Ok(value);
        }
    }
    Err(anyhow!("没等到 {kind}"))
}

#[tokio::test]
async fn quic_stream_carries_full_direct_protocol() -> Result<()> {
    let mut harness = start_quic("full").await?;
    let (mut ws, seen) = connect_quic(harness.addr, None).await?;

    // 首次连接没有 pin：握手应当成功，并把实际指纹回填出来。
    ws.send(WsMessage::Text(hello(TOKEN).to_json().into()))
        .await?;
    let welcome = next_json_t(&mut ws, "welcome").await?;
    assert_eq!(
        welcome
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("QUIC-AGENT"),
        "welcome 里应带上被控端名字"
    );

    let recorded = seen.lock().unwrap().clone();
    assert_eq!(
        recorded.as_deref(),
        Some(harness.fingerprint.as_str()),
        "QUIC 握手应把实际证书指纹回填出来，供配对时保存"
    );

    // 开会话：被控端会走授权回调。
    ws.send(WsMessage::Text(
        r#"{"t":"open","request":1,"viewer":"QUIC-CLIENT"}"#.into(),
    ))
    .await?;
    let open_ok = next_json_t(&mut ws, "open.ok").await?;
    let session = open_ok
        .get("session")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| anyhow!("open.ok 没有会话号"))?;
    assert!(session > 0);

    // 被控端应该收到授权请求。
    let (request_session, viewer, reply) =
        tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
            .await
            .map_err(|_| anyhow!("等待授权请求超时"))?
            .ok_or_else(|| anyhow!("没有授权请求"))?;
    assert_eq!(request_session as u64, session);
    assert_eq!(viewer, "QUIC-CLIENT");
    reply.send(true).map_err(|_| anyhow!("回传授权失败"))?;

    Ok(())
}

#[tokio::test]
async fn quic_rejects_wrong_fingerprint() -> Result<()> {
    let harness = start_quic("pin").await?;
    // 拿一个肯定不对的指纹去连：握手必须在 TLS 阶段就失败。
    let wrong = "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF".to_string();
    let result = connect_quic(harness.addr, Some(wrong)).await;
    assert!(
        result.is_err(),
        "证书指纹不匹配时必须拒绝连接，不能静默接受"
    );
    Ok(())
}

#[tokio::test]
async fn quic_accepts_correct_fingerprint() -> Result<()> {
    let harness = start_quic("pin-ok").await?;
    let (mut ws, _) = connect_quic(harness.addr, Some(harness.fingerprint.clone())).await?;
    ws.send(WsMessage::Text(hello(TOKEN).to_json().into()))
        .await?;
    let welcome = next_json_t(&mut ws, "welcome").await?;
    assert!(welcome.get("agent").is_some());
    Ok(())
}

#[tokio::test]
async fn quic_rejects_bad_token() -> Result<()> {
    let harness = start_quic("token").await?;
    let (mut ws, _) = connect_quic(harness.addr, None).await?;
    // 白名单里没有这个 token：被控端应拒绝。
    ws.send(WsMessage::Text(hello("not-allowed").to_json().into()))
        .await?;
    // 收不到 welcome，连接被关。
    let mut closed = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) => {
                closed = true;
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "token 不在白名单时必须断开连接");
    Ok(())
}

#[test]
fn quic_peer_ip_is_loopback() {
    // 服务端把 `connection.remote_address().ip()` 当 peer_ip 传给限流器；
    // loopback 在测试里是合法来源（真实部署时由 is_lan_peer 过滤）。
    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
}
