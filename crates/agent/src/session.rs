//! 单条连接的会话：握手、心跳、状态推送。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use screenwall_proto::codes;
use screenwall_proto::{
    Capabilities, DisplayList, ErrorMsg, Limits, Message, PeerInfo, Pong, ScreenInfo, Stats,
    Status, PROTO_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use crate::server::AgentState;

type Sink<S> = SplitSink<WebSocketStream<S>, WsMessage>;

pub async fn handle<S>(ws: WebSocketStream<S>, state: Arc<AgentState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();

    // ---- 握手：5 秒内必须收到 hello
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
    let first = match first {
        Ok(Some(Ok(frame))) => frame,
        Ok(Some(Err(error))) => return Err(error.into()),
        Ok(None) => bail!("握手前连接关闭"),
        Err(_) => {
            close_with(&mut sink, codes::HANDSHAKE_TIMEOUT, "handshake timeout").await?;
            bail!("握手超时");
        }
    };

    let hello = match first {
        WsMessage::Text(text) => match Message::parse(text.as_str()) {
            Ok(Message::Hello(hello)) => hello,
            _ => {
                close_with(&mut sink, codes::BAD_REQUEST, "first message must be hello").await?;
                bail!("首条消息不是 hello");
            }
        },
        _ => {
            close_with(&mut sink, codes::BAD_REQUEST, "first message must be text").await?;
            bail!("首条消息不是文本帧");
        }
    };

    if hello.proto != PROTO_VERSION {
        close_with(&mut sink, codes::PROTO_UNSUPPORTED, "unsupported protocol").await?;
        bail!("协议版本不匹配: {}", hello.proto);
    }
    // 对端没声明 caps 时按 v1 基线处理，老主控端照旧可用。
    let agreed = Capabilities::with_peer_and(hello.caps.clone(), crate::h264::capabilities());
    tracing::debug!(caps = %agreed, "局域网会话能力协商完成");
    if !state.config.allow_control {
        close_with(&mut sink, codes::CONTROL_DISABLED, "control disabled").await?;
        bail!("本机已关闭允许被控");
    }
    if !constant_time_eq(hello.token.as_bytes(), state.config.token.as_bytes()) {
        close_with(&mut sink, codes::AUTH_FAILED, "auth failed").await?;
        bail!("token 错误");
    }

    tracing::info!(client = %hello.client.name, "客户端已连接");

    // ---- 欢迎与初始状态
    send(
        &mut sink,
        &Message::Welcome(screenwall_proto::Welcome {
            proto: PROTO_VERSION,
            agent: PeerInfo {
                name: state.config.name.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            limits: Limits::defaults(),
            // 与其它握手入口一致：只回双方交集。
            caps: agreed,
        }),
    )
    .await?;
    send(
        &mut sink,
        &Message::DisplayList(DisplayList {
            displays: state.displays.clone(),
        }),
    )
    .await?;
    // CPU 是差分采样：先建基准，后面每次推送才有真实值。
    let mut cpu = crate::stats::CpuSample::default();
    send(&mut sink, &build_status(&state, &mut cpu)).await?;

    // ---- 主循环
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                send(&mut sink, &build_status(&state, &mut cpu)).await?;
            }
            item = stream.next() => {
                let Some(item) = item else { break };
                match item? {
                    WsMessage::Text(text) => {
                        handle_text(&mut sink, text.as_str()).await?;
                    }
                    WsMessage::Ping(payload) => {
                        sink.send(WsMessage::Pong(payload)).await?;
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    tracing::info!(client = %hello.client.name, "会话结束");
    Ok(())
}

async fn handle_text<S>(sink: &mut Sink<S>, text: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match Message::parse(text) {
        Ok(Message::Ping(ping)) => {
            send(sink, &Message::Pong(Pong { ts: ping.ts })).await?;
        }
        Ok(Message::Input(_) | Message::SecureAttention(_)) => {
            // 局域网直连只做「握手 + 状态 + 唤醒」的连通性探测：它没有授权弹窗，
            // 注入键鼠会绕开 P1 的逐会话授权，所以明确拒绝而不是静默丢弃。
            // Ctrl+Alt+Del 同属输入注入，一并拒绝。
            send_error(
                sink,
                codes::CONTROL_OFF,
                "局域网直连不提供键鼠控制，请使用账号登录后的中转会话",
            )
            .await?;
        }
        Ok(Message::VideoSub(_)) => {
            send_error(
                sink,
                codes::ENCODER_UNAVAILABLE,
                "局域网直连暂不提供画面，请使用账号登录后的中转会话",
            )
            .await?;
        }
        Ok(Message::WolRequest(request)) => {
            // 与中转会话保持一致：同一个 wol 模块、同一套校验与限流。
            let mac = request.mac.clone();
            let broadcast = request.broadcast.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::wol::send_magic_packet(&mac, &broadcast)
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    send(
                        sink,
                        &Message::WolOk(screenwall_proto::WolOk { mac: request.mac }),
                    )
                    .await?;
                }
                Ok(Err(error)) => {
                    send_error(
                        sink,
                        codes::BAD_REQUEST,
                        &format!("唤醒包发送失败: {error}"),
                    )
                    .await?;
                }
                Err(error) => {
                    send_error(sink, codes::BAD_REQUEST, &format!("唤醒任务失败: {error}")).await?;
                }
            }
        }
        Ok(Message::Hello(_)) => {
            send_error(sink, codes::BAD_REQUEST, "重复的 hello").await?;
        }
        Ok(Message::Error(error)) => {
            tracing::warn!(code = error.code, msg = %error.msg, "客户端报告错误");
        }
        Ok(_) => {
            send_error(sink, codes::UNKNOWN_MESSAGE, "暂不支持该消息").await?;
        }
        Err(error) => {
            send_error(sink, codes::BAD_REQUEST, &format!("消息解析失败: {error}")).await?;
        }
    }
    Ok(())
}

/// 局域网直连没有媒体流，`kbps` 恒为 0；`cpu` 仍是真实的整机占用。
fn build_status(state: &AgentState, cpu: &mut crate::stats::CpuSample) -> Message {
    let primary = state
        .displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| state.displays.first());

    Message::Status(Status {
        screen: ScreenInfo {
            w: primary.map(|display| display.w).unwrap_or(0),
            h: primary.map(|display| display.h).unwrap_or(0),
            scale: 1.0,
        },
        subs: Vec::new(),
        stats: Stats {
            cpu: cpu.sample().unwrap_or(0.0),
            kbps: 0,
            mem: crate::stats::memory_percent().unwrap_or(0.0),
        },
    })
}

async fn send<S>(sink: &mut Sink<S>, message: &Message) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(WsMessage::text(message.to_json())).await?;
    Ok(())
}

async fn send_error<S>(sink: &mut Sink<S>, code: u16, msg: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(
        sink,
        &Message::Error(ErrorMsg {
            code,
            msg: msg.to_string(),
            tid: None,
            id: None,
        }),
    )
    .await
}

async fn close_with<S>(sink: &mut Sink<S>, code: u16, reason: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = WsMessage::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    }));
    if let Err(error) = sink.send(frame).await {
        tracing::debug!(%error, "发送关闭帧失败");
    }
    sink.close().await.ok();
    Ok(())
}

/// 逐字节异或比较，耗时与内容无关；直连白名单也复用它。
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}
