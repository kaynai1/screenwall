//! M0 验收：hello → welcome，以及错误 token 被拒绝。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::server::{run_server, AgentState};
use screenwall_agent::tls;
use screenwall_proto::{Capabilities, Hello, Message, PeerInfo, PROTO_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

const TOKEN: &str = "test-token";

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
    std::env::temp_dir().join(format!("screenwall-test-{}-{tag}", std::process::id()))
}

async fn start_agent(tag: &str) -> Result<u16> {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(&dir)?;
    let material = tls::ensure_cert(&dir)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(tls::server_config(&material)?);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(AgentState {
        config: AgentConfig {
            name: "TEST-AGENT".into(),
            port,
            token: TOKEN.into(),
            allow_control: true,
            clipboard_sync: true,
            server_url: String::new(),
            account_token: String::new(),
            device_id: "test-device".into(),
            ca_path: String::new(),
            virtual_display: false,
        },
        displays: Vec::new(),
        approval: None,
    });

    tokio::spawn(async move {
        if let Err(error) = run_server(listener, state, acceptor).await {
            eprintln!("测试服务器退出: {error}");
        }
    });

    Ok(port)
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
            name: "TEST-CLIENT".into(),
            version: "0.1.0".into(),
        },
        caps: Capabilities::current(),
    })
}

#[tokio::test]
async fn valid_token_gets_welcome_and_displays() -> Result<()> {
    let port = start_agent("ok").await?;
    let mut ws = connect(port).await?;

    ws.send(WsMessage::text(hello(TOKEN).to_json())).await?;

    let mut got_welcome = false;
    let mut got_displays = false;
    let mut got_status = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while !(got_welcome && got_displays && got_status) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let item = tokio::time::timeout(remaining, ws.next()).await?;
        let item = match item {
            Some(item) => item?,
            None => return Err(anyhow!("连接提前关闭")),
        };
        if let WsMessage::Text(text) = item {
            match Message::parse(text.as_str())? {
                Message::Welcome(welcome) => {
                    assert_eq!(welcome.agent.name, "TEST-AGENT");
                    assert_eq!(welcome.proto, PROTO_VERSION);
                    // 被控端必须声明完整能力集，主控端据此判断可选功能。
                    assert!(
                        welcome.caps.contains(screenwall_proto::caps::AUDIO_VOLUME),
                        "welcome 应声明 audio.volume：{}",
                        welcome.caps
                    );
                    got_welcome = true;
                }
                Message::DisplayList(displays) => {
                    assert!(displays.displays.is_empty());
                    got_displays = true;
                }
                Message::Status(status) => {
                    assert_eq!(status.screen.w, 0);
                    got_status = true;
                }
                other => panic!("意外的消息: {other:?}"),
            }
        }
    }

    Ok(())
}

/// 老版本主控端的 hello 不带 `caps`，必须照旧握手成功，并被按 v1 基线对待。
#[tokio::test]
async fn legacy_hello_without_caps_is_accepted() -> Result<()> {
    let port = start_agent("legacy").await?;
    let mut ws = connect(port).await?;

    let legacy = format!(
        r#"{{"t":"hello","proto":{PROTO_VERSION},"token":"{TOKEN}","client":{{"name":"OLD-CLIENT","version":"0.0.9"}}}}"#
    );
    ws.send(WsMessage::text(legacy)).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let item = tokio::time::timeout(remaining, ws.next()).await?;
        let item = match item {
            Some(item) => item?,
            None => return Err(anyhow!("连接提前关闭")),
        };
        if let WsMessage::Text(text) = item {
            match Message::parse(text.as_str())? {
                Message::Welcome(welcome) => {
                    assert_eq!(welcome.agent.name, "TEST-AGENT");
                    // welcome 回的是交集：老主控端按 v1 基线协商，不能出现 AEC。
                    assert!(
                        !welcome.caps.contains(screenwall_proto::caps::AUDIO_AEC),
                        "老主控端不该被告知有 AEC：{}",
                        welcome.caps
                    );
                    assert!(
                        welcome.caps.contains(screenwall_proto::caps::AUDIO_VOLUME),
                        "v1 基线能力不能丢：{}",
                        welcome.caps
                    );
                    return Ok(());
                }
                Message::DisplayList(_) | Message::Status(_) => {}
                other => panic!("意外的消息: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn wrong_token_is_rejected() -> Result<()> {
    let port = start_agent("bad").await?;
    let mut ws = connect(port).await?;
    ws.send(WsMessage::text(hello("wrong").to_json())).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let item = tokio::time::timeout(remaining, ws.next()).await?;
        match item {
            Some(Ok(WsMessage::Text(text))) => {
                if let Message::Error(error) = Message::parse(text.as_str())? {
                    assert_eq!(error.code, 4001);
                    return Ok(());
                }
            }
            Some(Ok(WsMessage::Close(frame))) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(0);
                assert_eq!(code, 4001);
                return Ok(());
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Err(anyhow!("连接在返回错误前关闭")),
        }
    }
}
