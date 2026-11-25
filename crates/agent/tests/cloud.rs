//! 端到端：Agent 主动连服务器，主控端通过中转建立会话。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use screenwall_agent::cloud::{self, CloudOptions};
use screenwall_agent::config::AgentConfig;
use screenwall_agent::server::{AgentState, ApprovalFn, ApprovalRequest};
use screenwall_proto::cloud::{decode_tunnel_frame, encode_tunnel_frame, TunnelKind};
use screenwall_proto::{codes, Capabilities, DisplayInfo, Hello, Message, PeerInfo, Ping};
use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

async fn start_server() -> String {
    let config = Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: true,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    };
    let state = Arc::new(AppState::new(
        Db::open_in_memory().expect("内存数据库"),
        config,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        axum::serve(listener, router(state))
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

async fn login(addr: &str) -> String {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": "kay", "password": "password123" }))
        .send()
        .await
        .expect("注册");
    assert!(response.status().is_success());
    let response = client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": "kay",
            "password": "password123",
            "device": { "id": "dev-a", "name": "LivingRoom", "os": "Windows 11", "version": "0.1.0" }
        }))
        .send()
        .await
        .expect("登录");
    assert!(response.status().is_success());
    let body: Value = response.json().await.expect("登录响应");
    body["token"].as_str().expect("token").to_string()
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

async fn next_inner<S>(ws: &mut WebSocketStream<S>, session: u32) -> Message
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等待中转帧超时")
            .expect("连接已关闭")
            .expect("WebSocket 错误");
        let data = match message {
            WsMessage::Binary(data) => data,
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("意外的消息: {other:?}"),
        };
        let (header, payload) = decode_tunnel_frame(&data).expect("会话帧");
        if header.session != session {
            continue;
        }
        assert_eq!(header.kind, TunnelKind::Json);
        return Message::parse(std::str::from_utf8(payload).expect("UTF-8")).expect("消息");
    }
}

#[tokio::test]
async fn agent_connects_and_serves_session() {
    let addr = start_server().await;
    let token = login(&addr).await;

    let config = AgentConfig {
        name: "TestAgent".to_string(),
        port: 0,
        token: "local-token".to_string(),
        allow_control: true,
        clipboard_sync: true,
        server_url: format!("http://{addr}"),
        account_token: token.clone(),
        device_id: "dev-a".to_string(),
        ca_path: String::new(),
        virtual_display: false,
    };
    let state = Arc::new(AgentState {
        config,
        displays: vec![DisplayInfo {
            index: 0,
            w: 1920,
            h: 1080,
            primary: true,
            x: 0,
            y: 0,
        }],
        approval: None,
    });
    let options = CloudOptions::from_state(&state).expect("中转配置");
    let agent = tokio::spawn(cloud::run(state, options));

    let mut viewer = connect(&addr, "/ws/view", &token).await;
    let ready = next_json(&mut viewer).await;
    assert_eq!(ready["t"], "ready");

    let devices = loop {
        let value = next_json_t(&mut viewer, "devices").await;
        let online = value["devices"]
            .as_array()
            .map(|list| {
                list.iter()
                    .any(|device| device["id"] == "dev-a" && device["online"] == true)
            })
            .unwrap_or(false);
        if online {
            break value;
        }
    };
    assert!(devices["devices"].as_array().unwrap().len() == 1);

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 9, "deviceId": "dev-a", "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let opened = next_json_t(&mut viewer, "open.ok").await;
    assert_eq!(opened["request"], 9);
    let session = opened["session"].as_u64().expect("session") as u32;

    let hello = Message::Hello(Hello {
        proto: 1,
        token: String::new(),
        client: PeerInfo {
            name: "Office".to_string(),
            version: "0.1.0".to_string(),
        },
        caps: Capabilities::current(),
    });
    let frame = encode_tunnel_frame(TunnelKind::Json, session, hello.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("发送 hello");

    match next_inner(&mut viewer, session).await {
        Message::Welcome(welcome) => {
            assert_eq!(welcome.agent.name, "TestAgent");
            // 主控端声明了全量能力，welcome 回的交集就是本机全量。
            assert!(
                welcome.caps.contains(screenwall_proto::caps::WALL),
                "welcome 应声明 wall 能力：{}",
                welcome.caps
            );
        }
        other => panic!("期望 welcome，收到 {other:?}"),
    }
    match next_inner(&mut viewer, session).await {
        Message::DisplayList(list) => assert_eq!(list.displays[0].w, 1920),
        other => panic!("期望 display.list，收到 {other:?}"),
    }
    match next_inner(&mut viewer, session).await {
        Message::Status(status) => assert_eq!(status.screen.w, 1920),
        other => panic!("期望 status，收到 {other:?}"),
    }

    let ping = Message::Ping(Ping { ts: 12345 });
    let frame = encode_tunnel_frame(TunnelKind::Json, session, ping.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("发送 ping");
    loop {
        match next_inner(&mut viewer, session).await {
            Message::Pong(pong) => {
                assert_eq!(pong.ts, 12345);
                break;
            }
            // 5 秒状态推送可能先到，跳过。
            Message::Status(_) => continue,
            other => panic!("期望 pong，收到 {other:?}"),
        }
    }

    agent.abort();
}

/// 被控端授权：拒绝要回 open.err，允许才建会话。
#[tokio::test]
async fn control_requires_host_approval() {
    let addr = start_server().await;
    let token = login(&addr).await;

    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let approval: ApprovalFn = Arc::new(move |request: ApprovalRequest| {
        let _ = request_tx.send((request.session, request.viewer, request.reply));
    });

    let config = AgentConfig {
        name: "TestAgent".to_string(),
        port: 0,
        token: "local-token".to_string(),
        allow_control: true,
        clipboard_sync: true,
        server_url: format!("http://{addr}"),
        account_token: token.clone(),
        device_id: "dev-a".to_string(),
        ca_path: String::new(),
        virtual_display: false,
    };
    let state = Arc::new(AgentState {
        config,
        displays: vec![DisplayInfo {
            index: 0,
            w: 1920,
            h: 1080,
            primary: true,
            x: 0,
            y: 0,
        }],
        approval: Some(approval),
    });
    let options = CloudOptions::from_state(&state).expect("中转配置");
    let agent = tokio::spawn(cloud::run(state, options));

    let mut viewer = connect(&addr, "/ws/view", &token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    loop {
        let value = next_json_t(&mut viewer, "devices").await;
        let online = value["devices"]
            .as_array()
            .map(|list| {
                list.iter()
                    .any(|device| device["id"] == "dev-a" && device["online"] == true)
            })
            .unwrap_or(false);
        if online {
            break;
        }
    }

    // 1) 拒绝：主控端应拿到 open.err，而不是会话。
    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 9, "deviceId": "dev-a", "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let (first_session, viewer_name, reply) = request_rx.recv().await.expect("授权请求");
    assert_eq!(viewer_name, "Office");
    reply.send(false).expect("答复拒绝");
    let rejected = next_json_t(&mut viewer, "open.err").await;
    assert_eq!(rejected["request"], 9);
    assert_eq!(rejected["code"], codes::CONTROL_DISABLED);
    assert_eq!(rejected["session"].as_u64(), None, "拒绝不应暴露会话号");
    let _ = first_session;

    // 2) 允许：建立会话后能走完 hello 握手。
    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 10, "deviceId": "dev-a", "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let (approved_session, _, reply) = request_rx.recv().await.expect("第二次授权请求");
    reply.send(true).expect("答复允许");
    let opened = next_json_t(&mut viewer, "open.ok").await;
    assert_eq!(opened["request"], 10);
    let session = opened["session"].as_u64().expect("session") as u32;
    assert_eq!(session, approved_session);

    // 老主控端：hello 不带 caps，被控端按 v1 基线协商。
    let hello = Message::Hello(Hello {
        proto: 1,
        token: String::new(),
        client: PeerInfo {
            name: "Office".to_string(),
            version: "0.1.0".to_string(),
        },
        caps: Capabilities::default(),
    });
    let frame = encode_tunnel_frame(TunnelKind::Json, session, hello.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("发送 hello");
    match next_inner(&mut viewer, session).await {
        Message::Welcome(welcome) => {
            assert_eq!(welcome.agent.name, "TestAgent");
            // 中转入口同样回交集：老主控端没声明 caps，不能被告知有 AEC。
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
        }
        other => panic!("期望 welcome，收到 {other:?}"),
    }

    agent.abort();
}

/// 异地开机代发：控制会话里的 `wol.request` 真的发 magic packet 并回 `wol.ok`；
/// 紧接着的第二次请求会被 10 秒限流挡下（错误码 `BAD_REQUEST`）。
///
/// MAC 用一个不存在的本地管理地址，只有 102 字节广播出去，不会唤醒任何机器。
#[tokio::test]
async fn wol_request_is_relayed_and_rate_limited() {
    let addr = start_server().await;
    let token = login(&addr).await;

    let approval: ApprovalFn = Arc::new(|request: ApprovalRequest| {
        let _ = request.reply.send(true);
    });
    let config = AgentConfig {
        name: "TestAgent".to_string(),
        port: 0,
        token: "local-token".to_string(),
        allow_control: true,
        clipboard_sync: false,
        server_url: format!("http://{addr}"),
        account_token: token.clone(),
        device_id: "dev-a".to_string(),
        ca_path: String::new(),
        virtual_display: false,
    };
    let state = Arc::new(AgentState {
        config,
        displays: vec![DisplayInfo {
            index: 0,
            w: 1920,
            h: 1080,
            primary: true,
            x: 0,
            y: 0,
        }],
        approval: Some(approval),
    });
    let options = CloudOptions::from_state(&state).expect("中转配置");
    let agent = tokio::spawn(cloud::run(state, options));

    let mut viewer = connect(&addr, "/ws/view", &token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    loop {
        let value = next_json_t(&mut viewer, "devices").await;
        let online = value["devices"]
            .as_array()
            .map(|list| {
                list.iter()
                    .any(|device| device["id"] == "dev-a" && device["online"] == true)
            })
            .unwrap_or(false);
        if online {
            break;
        }
    }

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 21, "deviceId": "dev-a", "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let opened = next_json_t(&mut viewer, "open.ok").await;
    let session = opened["session"].as_u64().expect("session") as u32;

    let hello = Message::Hello(Hello {
        proto: 1,
        token: String::new(),
        client: PeerInfo {
            name: "Office".to_string(),
            version: "0.1.0".to_string(),
        },
        caps: Capabilities::default(),
    });
    let frame = encode_tunnel_frame(TunnelKind::Json, session, hello.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("发送 hello");
    loop {
        match next_inner(&mut viewer, session).await {
            Message::Welcome(_) => break,
            Message::Status(_) => continue,
            other => panic!("期望 welcome，收到 {other:?}"),
        }
    }

    let mac = "02:00:00:00:00:01".to_string();
    let wol = Message::WolRequest(screenwall_proto::WolRequest {
        mac: mac.clone(),
        broadcast: "255.255.255.255".to_string(),
    });
    let frame = encode_tunnel_frame(TunnelKind::Json, session, wol.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("发送 wol.request");
    loop {
        match next_inner(&mut viewer, session).await {
            Message::WolOk(ok) => {
                assert_eq!(ok.mac, mac);
                break;
            }
            Message::Status(_) | Message::DisplayList(_) => continue,
            other => panic!("期望 wol.ok，收到 {other:?}"),
        }
    }

    // 10 秒内再发一次：被限流挡下，错误直接回到主控端。
    let frame = encode_tunnel_frame(TunnelKind::Json, session, wol.to_json().as_bytes());
    viewer
        .send(WsMessage::Binary(frame.into()))
        .await
        .expect("再次发送 wol.request");
    loop {
        match next_inner(&mut viewer, session).await {
            Message::Error(error) => {
                assert_eq!(error.code, screenwall_proto::codes::BAD_REQUEST);
                assert!(
                    error.msg.contains("频繁"),
                    "限流文案应说明频率：{}",
                    error.msg
                );
                break;
            }
            Message::Status(_) | Message::DisplayList(_) => continue,
            other => panic!("期望限流错误，收到 {other:?}"),
        }
    }

    agent.abort();
}
