//! TLS + WebSocket 监听。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use screenwall_proto::DisplayInfo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::config::AgentConfig;
use crate::session;

/// 被控端授权请求：主控端要打开会话时，由宿主界面决定是否放行。
pub struct ApprovalRequest {
    pub session: u32,
    pub viewer: String,
    /// 主控端和这台被控端在同一个账号下（中转服务器在 `open` 里标
    /// `sameAccount: true`）。意思是「主控端就是账号主人」，宿主可以跳过
    /// 确认直接放行；直连和连接码会话不带这个标记，仍然逐次弹窗。
    pub same_account: bool,
    /// 这台主控端的长期凭据之前已经通过一次本机授权（直连白名单里的长期
    /// token）。宿主可以选择「记住已授权设备」，之后不再弹窗。
    pub authorized_before: bool,
    /// 宿主界面的答复；`false` 或通道被丢弃都按拒绝处理。
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

pub type ApprovalFn = Arc<dyn Fn(ApprovalRequest) + Send + Sync>;

pub struct AgentState {
    pub config: AgentConfig,
    pub displays: Vec<DisplayInfo>,
    /// 显卡型号，随 `welcome` 上报；采集失败为空字符串。
    pub gpu: String,
    /// 本机局域网出口 IPv4，随 `welcome` 上报；拿不到为空字符串。
    pub ip: String,
    /// 被控端授权回调；`None` 表示按 `config.allow_control` 直接放行。
    pub approval: Option<ApprovalFn>,
}

pub async fn run_server(
    listener: TcpListener,
    state: Arc<AgentState>,
    tls: TlsAcceptor,
) -> Result<()> {
    loop {
        let (tcp, peer) = listener.accept().await?;
        let state = state.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match serve_connection(tcp, tls, state).await {
                Ok(()) => tracing::debug!(%peer, "连接正常结束"),
                Err(error) => tracing::warn!(%peer, error = %error, "连接中断"),
            }
        });
    }
}

async fn serve_connection(tcp: TcpStream, tls: TlsAcceptor, state: Arc<AgentState>) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let stream = tokio::time::timeout(Duration::from_secs(10), tls.accept(tcp)).await??;
    let ws = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::accept_async(stream),
    )
    .await??;
    session::handle(ws, state).await
}
