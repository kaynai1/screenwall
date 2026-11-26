//! 群控广播的服务端行为：同账号两台设备、同一主控、两条会话。
//!
//! 主控端群控管理器为每台设备各开一条 `control` 会话，输入按会话分别下发；
//! 本测试验证服务端允许同一 viewer 同时持有指向不同设备的多条会话，
//! 且上行帧只到达对应设备（互不串台）。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use screenwall_proto::cloud::{encode_tunnel_frame, TunnelKind};
use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::PeerAddr;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: true,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    }
}

async fn start_server() -> String {
    let state = Arc::new(AppState::new(
        Db::open_in_memory().expect("内存数据库"),
        test_config(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<PeerAddr>(),
        )
        .await
        .expect("服务器退出");
    });

    let base = format!("http://{addr}");
    for _ in 0..40 {
        if reqwest::get(format!("{base}/healthz")).await.is_ok() {
            return addr.to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("服务器没有就绪");
}

async fn next_json<S>(ws: &mut WebSocketStream<S>) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等待消息超时")
            .expect("连接已关闭")
            .expect("WebSocket 错误");
        match message {
            WsMessage::Text(text) => return serde_json::from_str(text.as_str()).expect("JSON"),
            WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            other => panic!("意外的消息: {other:?}"),
        }
    }
}

async fn next_json_t<S>(ws: &mut WebSocketStream<S>, kind: &str) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let value = next_json(ws).await;
        if value.get("t").and_then(Value::as_str) == Some(kind) {
            return value;
        }
    }
}

async fn next_binary<S>(ws: &mut WebSocketStream<S>) -> Vec<u8>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等待二进制帧超时")
            .expect("连接已关闭")
            .expect("WebSocket 错误");
        match message {
            WsMessage::Binary(data) => return data.to_vec(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            other => panic!("意外的消息: {other:?}"),
        }
    }
}

/// 在 `wait` 内不应收到任何二进制帧。
async fn expect_no_binary<S>(ws: &mut WebSocketStream<S>, wait: Duration)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let message = tokio::time::timeout(wait, ws.next()).await;
    match message {
        Err(_) => {}
        Ok(Some(Ok(WsMessage::Binary(data)))) => {
            panic!("不应收到二进制帧，却收到 {} 字节", data.len());
        }
        Ok(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))) => {}
        Ok(other) => panic!("意外的消息: {other:?}"),
    }
}

async fn connect(
    addr: &str,
    path: &str,
    token: &str,
) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{addr}{path}")
        .into_client_request()
        .expect("请求");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("令牌头"),
    );
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("连接 WebSocket");
    ws
}

async fn login(client: &reqwest::Client, addr: &str, device: &str, name: &str) -> String {
    let login = client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": "kay",
            "password": "password123",
            "device": {
                "id": device,
                "name": name,
                "os": "Windows 11",
                "version": "0.1.0"
            }
        }))
        .send()
        .await
        .expect("登录请求");
    assert!(
        login.status().is_success(),
        "登录失败: {:?}",
        login.status()
    );
    let body: Value = login.json().await.expect("登录响应");
    body["token"].as_str().expect("token").to_string()
}

/// 主控向 `device` 打开会话并让其接受，返回会话号。
async fn open_session<S>(
    viewer: &mut WebSocketStream<S>,
    device: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    request: u32,
    device_id: &str,
) -> u32
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": request, "deviceId": device_id, "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");

    let open = next_json_t(device, "open").await;
    let session = open["session"].as_u64().expect("session") as u32;
    assert_eq!(open["mode"], "control");
    // 普通账号会话：主控端和被控端同账号，被控端可以免「允许控制」确认。
    assert_eq!(open["sameAccount"], true);
    device
        .send(WsMessage::Text(
            json!({ "t": "open.ok", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open.ok");

    let opened = next_json_t(viewer, "open.ok").await;
    assert_eq!(opened["session"], session);
    assert_eq!(opened["request"], request);
    assert_eq!(opened["deviceId"], device_id);
    session
}

#[tokio::test]
async fn group_control_broadcasts_to_each_device() {
    let addr = start_server().await;
    let client = reqwest::Client::new();

    let register = client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": "kay", "password": "password123" }))
        .send()
        .await
        .expect("注册请求");
    assert!(
        register.status().is_success(),
        "注册失败: {:?}",
        register.status()
    );

    // 同账号两台设备各自登录，各拿一枚绑定本设备的令牌。
    let token_a = login(&client, &addr, "dev-a", "LivingRoom").await;
    let token_b = login(&client, &addr, "dev-b", "Study").await;

    let mut device_a = connect(
        &addr,
        "/ws/device?device_id=dev-a&name=LivingRoom&os=Windows%2011&version=0.1.0",
        &token_a,
    )
    .await;
    assert_eq!(next_json(&mut device_a).await["t"], "ready");
    let mut device_b = connect(
        &addr,
        "/ws/device?device_id=dev-b&name=Study&os=Windows%2011&version=0.1.0",
        &token_b,
    )
    .await;
    assert_eq!(next_json(&mut device_b).await["t"], "ready");

    // 主控就是 dev-a 这台设备上的客户端，与真实客户端一致。
    let mut viewer = connect(&addr, "/ws/view", &token_a).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    let devices = next_json_t(&mut viewer, "devices").await;
    let list = devices["devices"].as_array().expect("设备数组");
    for id in ["dev-a", "dev-b"] {
        let target = list
            .iter()
            .find(|device| device["id"] == id)
            .unwrap_or_else(|| panic!("{id} 应出现在设备列表里"));
        assert_eq!(target["online"], true, "{id} 应在线");
    }

    let session_a = open_session(&mut viewer, &mut device_a, 1, "dev-a").await;
    let session_b = open_session(&mut viewer, &mut device_b, 2, "dev-b").await;
    assert_ne!(session_a, session_b, "每台设备应有独立会话");

    // 一次「广播」在客户端展开为两条按会话下发的输入帧。
    for (session, device) in [(session_a, &mut device_a), (session_b, &mut device_b)] {
        let frame = encode_tunnel_frame(
            TunnelKind::Json,
            session,
            br#"{"t":"input","events":[{"k":"move","abs":[0.25,0.75]}]}"#,
        );
        viewer
            .send(WsMessage::Binary(frame.clone().into()))
            .await
            .expect("发送广播输入");
        assert_eq!(
            next_binary(device).await,
            frame,
            "会话 {session} 的输入应到达本设备"
        );
    }

    // 只发给 dev-a 的帧不能串到 dev-b。
    let only_a = encode_tunnel_frame(TunnelKind::Json, session_a, br#"{"t":"input","events":[]}"#);
    viewer
        .send(WsMessage::Binary(only_a.clone().into()))
        .await
        .expect("发送单设备输入");
    assert_eq!(next_binary(&mut device_a).await, only_a);
    expect_no_binary(&mut device_b, Duration::from_millis(300)).await;

    // 关闭群控：两条会话分别收到 close。
    for (session, device) in [(session_a, &mut device_a), (session_b, &mut device_b)] {
        viewer
            .send(WsMessage::Text(
                json!({ "t": "close", "session": session })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("关闭会话");
        let close = next_json_t(device, "close").await;
        assert_eq!(close["session"], session);
    }
}
