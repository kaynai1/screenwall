//! QUIC 传输层：把打洞出来的 UDP socket 变成一条可读写的加密流。
//!
//! 为什么是 QUIC 而不是自己写 UDP 分包：
//!
//! - 现成的可靠有序流、拥塞控制、重传、MTU 探测，不用自己实现；
//! - 自带 TLS 1.3，证书校验能复用直连已有的指纹固定（[`crate::tls`]）；
//! - 流实现了 tokio 的 `AsyncRead` / `AsyncWrite`，可以直接交给
//!   `tokio-tungstenite`，**上层协议一行都不用改**。
//!
//! 这一层只负责「建立连接 + 开一条双向流」，会话、授权、文件、音频、
//! 剪贴板全部沿用原有代码。

use std::net::UdpSocket;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Endpoint, EndpointConfig, RecvStream, SendStream, ServerConfig};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::tls::CertMaterial;

/// QUIC 应用层协议标识。双方不一致会直接握手失败，
/// 避免把别的 QUIC 服务的连接当成直连。
pub const ALPN: &[u8] = b"screenwall-direct/1";

/// 空闲多久算断线。控制会话有心跳，但网络抖动时留足余量。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 保活间隔，小于空闲超时的一半。
const KEEP_ALIVE: Duration = Duration::from_secs(10);

fn transport() -> Result<Arc<quinn::TransportConfig>> {
    let mut config = quinn::TransportConfig::default();
    config
        .keep_alive_interval(Some(KEEP_ALIVE))
        .max_idle_timeout(Some(
            IDLE_TIMEOUT
                .try_into()
                .map_err(|error| anyhow!("空闲超时配置无效: {error}"))?,
        ));
    Ok(Arc::new(config))
}

/// 服务端端点：在被控端复用已经打洞的 UDP socket。
///
/// `socket` 必须是**打过洞的那个**，不能新建 —— 换了端口 NAT 映射就变了，
/// 对端打过来的包会被丢掉。
pub fn server_endpoint(socket: UdpSocket, material: &CertMaterial) -> Result<Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| anyhow!("TLS 版本配置失败: {error}"))?
        .with_no_client_auth()
        .with_single_cert(material.certs.clone(), material.key.clone_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic =
        QuicServerConfig::try_from(tls).map_err(|error| anyhow!("QUIC 服务端配置失败: {error}"))?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport()?);

    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(config),
        socket,
        quinn::default_runtime().ok_or_else(|| anyhow!("没有可用的异步运行时"))?,
    )?;
    // 服务端也允许主动发起流（当前用不到，但保持对称）。
    endpoint.set_default_client_config(client_config(None, Default::default())?);
    Ok(endpoint)
}

/// 客户端端点。
///
/// `pinned` 是配对时记下的证书指纹；`seen` 用来回填本次握手的实际指纹。
/// 首次配对时 `pinned` 为 `None`，握手不做比对，连上后由调用方决定是否信任。
pub struct Client {
    pub endpoint: Endpoint,
    config: ClientConfig,
}

pub fn client_endpoint(
    socket: UdpSocket,
    pinned: Option<String>,
    seen: Arc<std::sync::Mutex<Option<String>>>,
) -> Result<Client> {
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        quinn::default_runtime().ok_or_else(|| anyhow!("没有可用的异步运行时"))?,
    )?;
    let config = client_config(pinned, seen)?;
    Ok(Client { endpoint, config })
}

impl Client {
    /// 连到对端；`addr` 是打洞成功的那个地址。
    ///
    /// QUIC 的 `server_name` 只用于 SNI 校验，自签证书的 CN 是 `screenwall`，
    /// 所以这里固定传它 —— 真正的身份校验靠证书指纹固定。
    pub async fn connect(&self, addr: std::net::SocketAddr) -> Result<quinn::Connection> {
        let connecting = self
            .endpoint
            .connect_with(self.config.clone(), addr, SERVER_NAME)?;
        let connection = connecting
            .await
            .map_err(|error| anyhow!("QUIC 握手失败: {error}"))?;
        Ok(connection)
    }
}

fn client_config(
    pinned: Option<String>,
    seen: Arc<std::sync::Mutex<Option<String>>>,
) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| anyhow!("TLS 版本配置失败: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { pinned, seen }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic =
        QuicClientConfig::try_from(tls).map_err(|error| anyhow!("QUIC 客户端配置失败: {error}"))?;
    let mut config = ClientConfig::new(Arc::new(quic));
    config.transport_config(transport()?);
    Ok(config)
}

/// 自签证书的 CN，同时也是 SNI 里的服务名。
const SERVER_NAME: &str = "screenwall";

/// 一条双向流，当成普通的 `AsyncRead + AsyncWrite` 用。
///
/// 上层（WebSocket 握手、帧收发）不需要知道底下是 QUIC。
pub struct QuicStream {
    send: SendStream,
    recv: RecvStream,
}

impl QuicStream {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self { send, recv }
    }

    /// 从连接上开一条新的双向流（主控端用）。
    pub async fn open(connection: &quinn::Connection) -> Result<Self> {
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| anyhow!("打开 QUIC 流失败: {error}"))?;
        Ok(Self { send, recv })
    }

    /// 接受对端打开的双向流（被控端用）。
    pub async fn accept(connection: &quinn::Connection) -> Result<Self> {
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(|error| anyhow!("接受 QUIC 流失败: {error}"))?;
        Ok(Self { send, recv })
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 必须显式走 trait：`RecvStream` 自己也有同名的固有方法，直接调用会选到它。
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // 同上：`SendStream::poll_write` 固有方法返回的是 `WriteError`，
        // 不是 `io::Error`，必须显式选 trait 实现。
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

/// 证书校验：和 TCP 直连用同一套指纹固定逻辑。
///
/// QUIC 的 TLS 1.3 不允许 TLS 1.2 那套回调，所以只需要实现
/// `verify_server_cert` 与签名校验三项。
#[derive(Debug)]
struct PinnedCert {
    pinned: Option<String>,
    seen: Arc<std::sync::Mutex<Option<String>>>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = crate::tls::fingerprint(end_entity);
        if let Some(expected) = &self.pinned {
            if !actual.eq_ignore_ascii_case(expected) {
                tracing::warn!("QUIC 直连证书指纹不匹配，已拒绝连接");
                return Err(rustls::Error::General("证书指纹不匹配".into()));
            }
        }
        *self.seen.lock().unwrap() = Some(actual);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // QUIC 只跑 TLS 1.3，这个回调不会被调用。
        Err(rustls::Error::General("QUIC 不支持 TLS 1.2".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_is_stable() {
        // 改名等于把新旧版本之间的兼容性切断，必须有测试盯着。
        assert_eq!(ALPN, b"screenwall-direct/1");
    }

    #[test]
    fn transport_config_is_valid() {
        let config = transport().unwrap();
        // 保活必须短于空闲超时，否则连接会先被判死。
        assert!(KEEP_ALIVE < IDLE_TIMEOUT);
        let _ = config;
    }

    #[test]
    fn client_config_builds_without_pin() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let config = client_config(None, seen).unwrap();
        // 能构造出来就说明 TLS 1.3 provider 与 ALPN 都对得上；
        // quinn 的 ClientConfig 不暴露 transport 的读取接口，这里只验构造成功。
        let _ = config;
    }

    #[test]
    fn client_config_builds_with_pin() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let pinned = "AA:BB:CC".to_string();
        let config = client_config(Some(pinned), seen).unwrap();
        let _ = config;
    }
}
