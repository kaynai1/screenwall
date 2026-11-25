//! 可选的 TLS 监听：把 rustls 接在现有 TCP 监听层，路由与 WS 升级逻辑保持不变。
//!
//! 未配置证书时服务器仍然直接使用 `tokio::net::TcpListener` 的明文实现。

use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// 读取 PEM 证书链与私钥，构造 rustls 服务端配置。
///
/// 同时启用 rustls 的 ring 后端，避免依赖 aws-lc-rs 的本地编译链。
pub fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<Arc<ServerConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut cert_reader = BufReader::new(
        File::open(cert_path)
            .with_context(|| format!("打开 TLS 证书 {} 失败", cert_path.display()))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("解析 TLS 证书 {} 失败", cert_path.display()))?;
    if certs.is_empty() {
        bail!("TLS 证书 {} 中没有找到证书", cert_path.display());
    }

    let mut key_reader = BufReader::new(
        File::open(key_path)
            .with_context(|| format!("打开 TLS 私钥 {} 失败", key_path.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("解析 TLS 私钥 {} 失败", key_path.display()))?
        .with_context(|| format!("TLS 私钥 {} 中没有找到私钥", key_path.display()))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("TLS 证书与私钥不匹配")?;
    Ok(Arc::new(config))
}

/// 在 TCP 监听层完成 TLS 握手；握手失败只丢弃该连接，不影响服务器。
pub struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    pub fn new(inner: TcpListener, config: Arc<ServerConfig>) -> Self {
        Self {
            inner,
            acceptor: TlsAcceptor::from(config),
        }
    }
}

/// 客户端地址的 ConnectInfo 载体。
///
/// 用本地 newtype 而不是直接 `SocketAddr`：孤儿规则不允许为外来类型
/// `SocketAddr` 实现 axum 的 `Connected`，而 TLS 监听需要自定义实现。
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(conn) => conn,
                Err(error) => {
                    tracing::warn!("接受 TCP 连接失败: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(error) => {
                    tracing::debug!("TLS 握手失败（{addr}）: {error}");
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
