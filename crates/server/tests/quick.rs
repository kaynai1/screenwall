//! 连接码（ID + 密码）：不需要账号也能换到一台设备的受限令牌。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
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

/// 注册并登录，返回（账号令牌, 设备 ID）。
async fn login(base: &str, username: &str) -> (String, String) {
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/api/v1/register"))
        .json(&json!({ "username": username, "password": "password123" }))
        .send()
        .await
        .expect("注册请求");
    let value: Value = client
        .post(format!("{base}/api/v1/login"))
        .json(&json!({
            "username": username,
            "password": "password123",
            "device": { "name": "测试机", "os": "Windows", "version": "0.1.0" }
        }))
        .send()
        .await
        .expect("登录请求")
        .json()
        .await
        .expect("登录响应");
    let token = value["token"].as_str().expect("登录令牌").to_string();
    let device_id = value["device"]["id"].as_str().expect("设备 ID").to_string();
    (token, device_id)
}

async fn connect_ws(
    base: &str,
    path: &str,
    token: &str,
) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_base = if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        format!("ws://{}", base.replacen("http://", "", 1))
    };
    let mut request = format!("{ws_base}{path}")
        .into_client_request()
        .expect("构造请求");
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("WebSocket 连接");
    socket
}

#[tokio::test]
async fn quick_connect_issues_scoped_token() {
    let addr = start_server().await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let (token, owner_device) = login(&base, "quick-owner").await;

    // 开启连接码连接（顺便拿到码）。
    let state: Value = client
        .post(format!("{base}/api/v1/quick"))
        .bearer_auth(&token)
        .json(&json!({ "enabled": true, "password": "connect123" }))
        .send()
        .await
        .expect("设置连接码")
        .json()
        .await
        .expect("设置响应");
    assert_eq!(state["enabled"], true);

    let code: Value = client
        .get(format!("{base}/api/v1/quick"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("读取连接码")
        .json()
        .await
        .expect("读取响应");
    let code = code["code"].as_str().expect("连接码").to_string();
    assert_eq!(code.len(), 9);

    // 设备不在线时拒绝：连接码不是「排队等你上线」的凭据。
    let offline = client
        .post(format!("{base}/api/v1/quick/connect"))
        .json(&json!({ "code": code, "password": "connect123" }))
        .send()
        .await
        .expect("连接尝试");
    assert_eq!(offline.status(), reqwest::StatusCode::CONFLICT);

    // 让设备上线。
    let device_path =
        format!("/ws/device?device_id={owner_device}&name=QuickOwner&os=Windows&version=0.1.0");
    let mut device = connect_ws(&addr, &device_path, &token).await;
    assert_eq!(next_json(&mut device).await["t"], "ready");

    // 密码错误：401，提示不区分「码不存在」和「密码错」。
    let wrong = client
        .post(format!("{base}/api/v1/quick/connect"))
        .json(&json!({ "code": code, "password": "wrong-pass" }))
        .send()
        .await
        .expect("错误密码");
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

    let ok: Value = client
        .post(format!("{base}/api/v1/quick/connect"))
        .json(&json!({ "code": code, "password": "connect123", "name": "访客" }))
        .send()
        .await
        .expect("连接成功")
        .json()
        .await
        .expect("连接响应");
    let quick_token = ok["token"].as_str().expect("受限令牌").to_string();
    let device_id = ok["device"]["id"].as_str().expect("设备 ID").to_string();

    // 受限令牌不能访问账号接口。
    let devices = client
        .get(format!("{base}/api/v1/devices"))
        .bearer_auth(&quick_token)
        .send()
        .await
        .expect("设备列表");
    assert_eq!(devices.status(), reqwest::StatusCode::FORBIDDEN);

    // 连 /ws/view 只能看到这一台设备。
    let mut view = connect_ws(&addr, "/ws/view", &quick_token).await;
    assert_eq!(next_json(&mut view).await["t"], "ready");
    let list = next_json(&mut view).await;
    assert_eq!(list["t"], "devices");
    let devices = list["devices"].as_array().expect("设备数组");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"], device_id.as_str());

    // 打开会话时被控端收到 trusted 标记（不需要再弹确认框）。
    view.send(WsMessage::Text(
        json!({ "t": "open", "request": 1, "deviceId": device_id, "viewer": "访客" })
            .to_string()
            .into(),
    ))
    .await
    .expect("发送 open");
    let opened = next_json(&mut device).await;
    assert_eq!(opened["t"], "open");
    assert_eq!(opened["trusted"], true);
}

#[tokio::test]
async fn quick_connect_rejects_disabled_device() {
    let addr = start_server().await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let (token, _device_id) = login(&base, "quick-off").await;

    let code: Value = client
        .get(format!("{base}/api/v1/quick"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("读取连接码")
        .json()
        .await
        .expect("读取响应");
    let code = code["code"].as_str().expect("连接码").to_string();

    let response = client
        .post(format!("{base}/api/v1/quick/connect"))
        .json(&json!({ "code": code, "password": "anything" }))
        .send()
        .await
        .expect("连接尝试");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("响应体");
    assert_eq!(body["error"], "quick_disabled");
}
