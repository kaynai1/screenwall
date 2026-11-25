//! 端到端：注册登录 → 设备上线 → 主控打开会话 → 双向中转。

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

#[tokio::test]
async fn register_login_and_tunnel() {
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

    let login = client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": "kay",
            "password": "password123",
            "device": {
                "id": "dev-a",
                "name": "LivingRoom",
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
    let token = body["token"].as_str().expect("token").to_string();

    let mut device = connect(
        &addr,
        "/ws/device?device_id=dev-a&name=LivingRoom&os=Windows%2011&version=0.1.0",
        &token,
    )
    .await;
    let ready = next_json(&mut device).await;
    assert_eq!(ready["t"], "ready");

    let mut viewer = connect(&addr, "/ws/view", &token).await;
    let ready = next_json(&mut viewer).await;
    assert_eq!(ready["t"], "ready");
    let devices = next_json_t(&mut viewer, "devices").await;
    let list = devices["devices"].as_array().expect("设备数组");
    let target = list
        .iter()
        .find(|device| device["id"] == "dev-a")
        .expect("设备出现在列表里");
    assert_eq!(target["online"], true);

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 1, "deviceId": "dev-a", "viewer": "Office" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");

    let open = next_json_t(&mut device, "open").await;
    let session = open["session"].as_u64().expect("session") as u32;
    device
        .send(WsMessage::Text(
            json!({ "t": "open.ok", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open.ok");

    let opened = next_json_t(&mut viewer, "open.ok").await;
    assert_eq!(opened["session"], session);
    assert_eq!(opened["request"], 1);

    let up = encode_tunnel_frame(TunnelKind::Json, session, br#"{"t":"hello","proto":1}"#);
    viewer
        .send(WsMessage::Binary(up.clone().into()))
        .await
        .expect("发送上行帧");
    assert_eq!(next_binary(&mut device).await, up);

    let down = encode_tunnel_frame(TunnelKind::Binary, session, &[0, 1, 2, 3, 4]);
    device
        .send(WsMessage::Binary(down.clone().into()))
        .await
        .expect("发送下行帧");
    assert_eq!(next_binary(&mut viewer).await, down);

    viewer
        .send(WsMessage::Text(
            json!({ "t": "close", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("关闭会话");
    let close = next_json_t(&mut device, "close").await;
    assert_eq!(close["session"], session);
}

/// 设备管理：改名、允许控制开关、删除都落到服务器（用 dev-b 管理 dev-a）。
#[tokio::test]
async fn device_manage_rename_toggle_and_delete() {
    let addr = start_server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": "kay", "password": "password123" }))
        .send()
        .await
        .expect("注册请求");

    let mut tokens = Vec::new();
    for (id, name) in [("dev-a", "OldName"), ("dev-b", "Manager")] {
        let login: Value = client
            .post(format!("http://{addr}/api/v1/login"))
            .json(&json!({
                "username": "kay",
                "password": "password123",
                "device": { "id": id, "name": name, "os": "Windows", "version": "0.1.0" }
            }))
            .send()
            .await
            .expect("登录请求")
            .json()
            .await
            .expect("登录响应");
        tokens.push(login["token"].as_str().expect("token").to_string());
    }

    // 删除会吊销目标设备的令牌，所以用另一台设备的令牌来管理。
    let token = &tokens[1];

    let rename = client
        .patch(format!("http://{addr}/api/v1/devices/dev-a"))
        .bearer_auth(token)
        .json(&json!({ "name": "Studio" }))
        .send()
        .await
        .expect("改名请求");
    assert!(
        rename.status().is_success(),
        "改名失败: {:?}",
        rename.status()
    );

    let toggle = client
        .patch(format!("http://{addr}/api/v1/devices/dev-a"))
        .bearer_auth(token)
        .json(&json!({ "controllable": false }))
        .send()
        .await
        .expect("开关请求");
    assert!(
        toggle.status().is_success(),
        "开关失败: {:?}",
        toggle.status()
    );

    let list: Value = client
        .get(format!("http://{addr}/api/v1/devices"))
        .bearer_auth(token)
        .send()
        .await
        .expect("设备列表")
        .json()
        .await
        .expect("设备列表响应");
    let devices = list["devices"].as_array().expect("设备数组");
    let target = devices
        .iter()
        .find(|device| device["id"] == "dev-a")
        .expect("dev-a 仍在列表里");
    assert_eq!(target["name"], "Studio");
    assert_eq!(target["controllable"], false);

    let remove = client
        .delete(format!("http://{addr}/api/v1/devices/dev-a"))
        .bearer_auth(token)
        .send()
        .await
        .expect("删除请求");
    assert!(
        remove.status().is_success(),
        "删除失败: {:?}",
        remove.status()
    );

    let list: Value = client
        .get(format!("http://{addr}/api/v1/devices"))
        .bearer_auth(token)
        .send()
        .await
        .expect("设备列表")
        .json()
        .await
        .expect("设备列表响应");
    let devices = list["devices"].as_array().expect("设备数组");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"], "dev-b");

    let again = client
        .delete(format!("http://{addr}/api/v1/devices/dev-a"))
        .bearer_auth(token)
        .send()
        .await
        .expect("再次删除");
    assert_eq!(again.status(), reqwest::StatusCode::NOT_FOUND);
}

/// 设备不能删掉自己：删掉之后它持有的令牌会被吊销，等于把自己踢出账号。
/// 客户端界面隐藏了「移除」按钮，但服务端必须自己拦，不能只靠界面。
#[tokio::test]
async fn device_cannot_delete_itself() {
    let addr = start_server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": "kay", "password": "password123" }))
        .send()
        .await
        .expect("注册请求");

    let login: Value = client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": "kay",
            "password": "password123",
            "device": { "id": "self", "name": "ThisOne", "os": "Windows", "version": "0.1.0" }
        }))
        .send()
        .await
        .expect("登录请求")
        .json()
        .await
        .expect("登录响应");
    let token = login["token"].as_str().expect("token").to_string();

    let response = client
        .delete(format!("http://{addr}/api/v1/devices/self"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("删除自己");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "删除本机设备必须被拒绝"
    );

    // 令牌仍可用，设备仍在列表里。
    let list: Value = client
        .get(format!("http://{addr}/api/v1/devices"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("设备列表")
        .json()
        .await
        .expect("设备列表响应");
    let devices = list["devices"].as_array().expect("设备数组");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"], "self");
}
