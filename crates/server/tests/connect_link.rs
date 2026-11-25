//! 一次性连接链接：签发、消费、单次有效、落地页。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::PeerAddr;
use serde_json::{json, Value};
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
            "device": { "name": "测试机", "os": "Windows", "version": "0.2.0" }
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

async fn connect_device(
    base: &str,
    token: &str,
    device_id: &str,
) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_base = format!("ws://{}", base.replacen("http://", "", 1));
    let path = format!("/ws/device?device_id={device_id}&name=Owner&os=Windows&version=0.2.0");
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
async fn connect_link_is_single_use_and_scoped() {
    let addr = start_server().await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let (token, device_id) = login(&base, "link-owner").await;

    // 未开启连接码连接时，不允许签发链接。
    let denied = client
        .post(format!("{base}/api/v1/connect-link"))
        .bearer_auth(&token)
        .json(&json!({ "deviceId": device_id }))
        .send()
        .await
        .expect("签发尝试");
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);

    client
        .post(format!("{base}/api/v1/quick"))
        .bearer_auth(&token)
        .json(&json!({ "enabled": true, "password": "connect123" }))
        .send()
        .await
        .expect("开启连接码");

    let created: Value = client
        .post(format!("{base}/api/v1/connect-link"))
        .bearer_auth(&token)
        .json(&json!({ "deviceId": device_id }))
        .send()
        .await
        .expect("签发链接")
        .json()
        .await
        .expect("签发响应");
    let url = created["url"].as_str().expect("链接").to_string();
    let link_token = created["token"].as_str().expect("链接令牌").to_string();
    assert!(url.contains("/l/"));
    assert!(!link_token.is_empty());

    // 设备离线时拒绝，且不消耗链接。
    let offline = client
        .post(format!("{base}/api/v1/connect-link/use"))
        .json(&json!({ "token": link_token }))
        .send()
        .await
        .expect("离线消费");
    assert_eq!(offline.status(), reqwest::StatusCode::CONFLICT);

    // 落地页在有效期内返回唤起页面。
    let page = client
        .get(format!("{base}/l/{link_token}"))
        .send()
        .await
        .expect("落地页");
    assert_eq!(page.status(), reqwest::StatusCode::OK);
    let html = page.text().await.expect("页面内容");
    assert!(html.contains("screenwall://connect"));

    // 设备上线后可以换到受限令牌。
    let mut device = connect_device(&addr, &token, &device_id).await;
    let ready = device.next().await.expect("首条消息").expect("消息");
    assert!(matches!(ready, WsMessage::Text(_)));

    let ok: Value = client
        .post(format!("{base}/api/v1/connect-link/use"))
        .json(&json!({ "token": link_token }))
        .send()
        .await
        .expect("消费链接")
        .json()
        .await
        .expect("消费响应");
    let session_token = ok["token"].as_str().expect("会话令牌").to_string();
    assert_eq!(ok["device"]["id"], device_id.as_str());

    // 第二次使用同一条链接必须失败（单次消费）。
    let again = client
        .post(format!("{base}/api/v1/connect-link/use"))
        .json(&json!({ "token": link_token }))
        .send()
        .await
        .expect("二次消费");
    assert_eq!(again.status(), reqwest::StatusCode::NOT_FOUND);

    // 受限令牌不能访问账号接口。
    let devices = client
        .get(format!("{base}/api/v1/devices"))
        .bearer_auth(&session_token)
        .send()
        .await
        .expect("设备列表");
    assert_eq!(devices.status(), reqwest::StatusCode::FORBIDDEN);

    // 落地页在消费后显示失效。
    let page = client
        .get(format!("{base}/l/{link_token}"))
        .send()
        .await
        .expect("失效页");
    let html = page.text().await.expect("页面内容");
    assert!(html.contains("连接链接已失效"));
}

#[tokio::test]
async fn connect_link_requires_account_token() {
    let addr = start_server().await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v1/connect-link"))
        .json(&json!({}))
        .send()
        .await
        .expect("匿名签发");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 不存在的令牌直接判失效。
    let bad = client
        .get(format!("{base}/l/not-a-real-token"))
        .send()
        .await
        .expect("落地页");
    let html = bad.text().await.expect("页面内容");
    assert!(html.contains("连接链接已失效"));
}
