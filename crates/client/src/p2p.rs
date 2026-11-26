//! 同账号会话的 P2P 打洞（被控端这一半）。
//!
//! 账号会话默认走服务器中转。为了绕开中转带宽，主控端在建立会话前先经服务器
//! 递一份「我想直连」的请求过来（`p2p.offer`），这里负责：
//!
//! 1. 绑定 UDP 端口 + STUN 探出公网映射，把候选地址和一次性 token 回给主控端；
//! 2. 与主控端双向打洞，把打通的 socket 交给 QUIC 服务端；
//! 3. 在 QUIC 流上跑**和局域网直连完全一样**的 WebSocket 会话协议
//!    （[`direct::serve_stream`]），采集、输入、文件、音频、剪贴板全部复用。
//!
//! 失败就静默回退：主控端收不到 `answer` 或打洞超时后会走服务器中转，
//! 功能不受影响，只是慢一点、糊一点。
//!
//! token 是这里签发、经服务器递回主控端的一次性凭据，加进本服务私有的
//! [`DirectAuth`] 白名单（不会混进局域网直连的白名单），并标记成
//! `mark_auto`：同账号会话免掉「允许控制」弹窗，但仍受「允许被控」总开关约束。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use screenwall_agent::cloud::P2pProvider;
use screenwall_agent::direct::{self, AuthLimiter, DirectAuth};
use screenwall_agent::nat;
use screenwall_agent::quic;
use screenwall_agent::server::AgentState;
use screenwall_agent::tls;
use serde_json::json;
use tauri::AppHandle;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::Shared;

/// 一次性 token 的有效期。够覆盖打洞 + QUIC 握手 + 建会话的最坏耗时，
/// 又不能长到「会话早就结束了还被拿着用」。
const TOKEN_TTL: Duration = Duration::from_secs(180);
/// 与主控端互相打洞的上限。超时说明两边 NAT 打不通，交给中转。
const PUNCH_TIMEOUT: Duration = Duration::from_secs(6);
/// 打洞成功后等待 QUIC 连接/流的时长。
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// 主控端候选地址的条数上限（服务器侧也会截断一次）。
const MAX_CANDIDATES: usize = 8;

/// 被控端的 P2P 服务，挂到 [`screenwall_agent::cloud::CloudOptions`] 上。
pub struct P2pHost {
    app: AppHandle,
    shared: Arc<Shared>,
    /// 正在进行的打洞数。上限 2 条：正常只会有一条，多一点余量给
    /// 「上一次刚失败、主控端立刻重试」；再多的请求直接拒绝，
    /// 免得有人拿同一个账号刷出成百上千个 UDP socket 与任务。
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl P2pHost {
    pub fn new(app: AppHandle, shared: Arc<Shared>) -> Self {
        Self {
            app,
            shared,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl P2pProvider for P2pHost {
    fn handle_offer(
        &self,
        request: u32,
        viewer_id: u64,
        viewer: String,
        candidates: Vec<String>,
        out: mpsc::UnboundedSender<WsMessage>,
    ) {
        use std::sync::atomic::Ordering;
        if self.active.fetch_add(1, Ordering::Relaxed) >= 2 {
            self.active.fetch_sub(1, Ordering::Relaxed);
            let _ = out.send(WsMessage::Text(
                json!({
                    "t": "p2p",
                    "request": request,
                    "viewerId": viewer_id,
                    "event": "reject",
                    "msg": "本机正在处理其他直连请求"
                })
                .to_string()
                .into(),
            ));
            return;
        }
        let app = self.app.clone();
        let shared = self.shared.clone();
        let active = self.active.clone();
        tauri::async_runtime::spawn(async move {
            // 任务结束（无论成功失败）都要把计数还回去。
            struct Guard(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let _guard = Guard(active);
            if let Err(error) =
                answer(app, shared, request, viewer_id, viewer, candidates, out.clone()).await
            {
                tracing::info!("同账号 P2P 未能建立，主控端将走中转: {error:#}");
                // 明确告诉主控端别等了，它收到会立刻回退中转。
                let _ = out.send(WsMessage::Text(
                    json!({
                        "t": "p2p",
                        "request": request,
                        "viewerId": viewer_id,
                        "event": "reject",
                        "msg": "打洞失败"
                    })
                    .to_string()
                    .into(),
                ));
            }
        });
    }
}

/// 处理一次 P2P 请求：回 answer → 打洞 → QUIC → 跑会话。
///
/// 注意顺序：**先发 answer**，两边才能同时打洞。主控端收到 answer 才开始
/// 向我们的候选地址发包，我们这边则马上去打它，NAT 的两个洞才会同时开。
#[allow(clippy::too_many_arguments)]
async fn answer(
    app: AppHandle,
    shared: Arc<Shared>,
    request: u32,
    viewer_id: u64,
    viewer: String,
    candidates: Vec<String>,
    out: mpsc::UnboundedSender<WsMessage>,
) -> Result<()> {
    let config = shared.config.read().await.clone();
    if !config.punch_through {
        return Err(anyhow!("本机已关闭打洞"));
    }
    let peers: Vec<SocketAddr> = candidates
        .iter()
        .filter_map(|addr| addr.trim().parse::<SocketAddr>().ok())
        .take(MAX_CANDIDATES)
        .collect();
    if peers.is_empty() {
        return Err(anyhow!("主控端没有给出可用地址"));
    }

    // 绑定与 STUN 都是同步阻塞的（每个 STUN 服务器最多等 2 秒），
    // 放到阻塞线程池，别把 Tauri 的异步运行时卡住。
    let (punch, mapping) = tokio::task::spawn_blocking(|| {
        let mut punch = nat::PunchSocket::bind(0)?;
        // STUN 失败不算致命：同网段时内网地址直接就能打通。
        // 预算压到 3 秒：主控端正等着 answer，不能在这里慢慢试。
        let mapping = punch.discover_within(Duration::from_secs(3)).ok();
        Ok::<_, std::io::Error>((punch, mapping))
    })
    .await
    .context("打洞任务异常结束")?
    .context("绑定 UDP 端口失败")?;

    let local_port = punch.local_addr().context("读取本地端口失败")?.port();
    let mut offers: Vec<String> = Vec::new();
    if let Some(mapping) = mapping {
        tracing::info!(public = %mapping.public, local = %mapping.local, "P2P 打洞前的公网映射");
        offers.push(mapping.public.to_string());
    }
    for ip in crate::direct::local_addresses() {
        offers.push(format!("{ip}:{local_port}"));
    }
    if offers.is_empty() {
        return Err(anyhow!("没有可用候选地址"));
    }

    let material = tls::ensure_cert(&crate::base_dir()).context("加载 QUIC 证书失败")?;
    let token = random_token();
    if token.is_empty() {
        return Err(anyhow!("无法生成随机 token"));
    }
    // 白名单先放行再回 answer：主控端可能在 answer 到达的同一瞬间就开始握手。
    let auth = Arc::new(DirectAuth::new());
    auth.allow_for(token.clone(), TOKEN_TTL).await;
    auth.mark_auto(token.clone()).await;

    let payload = json!({
        "t": "p2p",
        "request": request,
        "viewerId": viewer_id,
        "event": "answer",
        "candidates": offers,
        "fingerprint": material.fingerprint,
        "token": token
    })
    .to_string();
    out.send(WsMessage::Text(payload.into()))
        .map_err(|_| anyhow!("中转控制通道已关闭"))?;
    tracing::info!(request, %viewer, "已回 P2P 应答，开始打洞");

    let (punch, reached) = tokio::task::spawn_blocking(move || {
        let reached = punch.punch(&peers, PUNCH_TIMEOUT);
        (punch, reached)
    })
    .await
    .context("打洞任务异常结束")?;
    let reached = reached.map_err(|error| anyhow!("打洞失败: {error}"))?;

    let socket = punch.into_socket();
    // PunchSocket 为了 `recv_from` 带超时开了读超时；QUIC 端点要的是
    // 纯非阻塞 socket，超时设置留着会和 quinn 的 poll 打架。
    let _ = socket.set_read_timeout(None);
    let _ = socket.set_write_timeout(None);
    socket
        .set_nonblocking(true)
        .context("切换非阻塞失败")?;
    let endpoint = quic::server_endpoint(socket, &material).context("QUIC 端点创建失败")?;
    tracing::info!(peer = %reached, "P2P 已打通，等待 QUIC 连接");

    let incoming = tokio::time::timeout(ACCEPT_TIMEOUT, endpoint.accept())
        .await
        .map_err(|_| anyhow!("等待 QUIC 连接超时"))?
        .ok_or_else(|| anyhow!("QUIC 端点已关闭"))?;
    let connection = incoming.await.context("QUIC 握手失败")?;
    let peer_ip = connection.remote_address().ip();
    let stream = tokio::time::timeout(ACCEPT_TIMEOUT, quic::QuicStream::accept(&connection))
        .await
        .map_err(|_| anyhow!("等待 QUIC 流超时"))?
        .context("接受 QUIC 流失败")?;

    // 先在锁外把配置取成独立值：读锁的临时守卫不能跨进下面的结构体字面量。
    let host_config = shared.config.read().await.clone();
    let state = Arc::new(AgentState {
        config: crate::direct::agent_config(&host_config),
        displays: screenwall_agent::display::list_displays(),
        approval: Some(crate::host_approval(app, shared)),
    });
    // 限流器每条连接一份：P2P 的 token 是一次性的，不该和局域网直连
    // 共用一个失败计数（互相殃及）。
    let limiter = Arc::new(AuthLimiter::new());
    let result = direct::serve_stream(stream, peer_ip, state, auth, limiter).await;
    if let Err(error) = result {
        tracing::debug!("P2P 会话结束: {error:#}");
    }
    // 连接对象要活到流结束，drop 掉会把流重置。
    let _ = connection.closed().await;
    Ok(())
}

/// 32 字节随机 token 的十六进制表示。
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 拿不到系统随机数时不能退化成弱 token，直接换一条更长的路径
        // （调用方会看到空 token 并放弃 P2P）。
        return String::new();
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_32_bytes_of_hex() {
        let token = random_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        // 两次生成不应相同。
        assert_ne!(token, random_token());
    }
}
