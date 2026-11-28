//! 可观测性与配额端到端测试：`/api/v1/metrics`、单账号设备上限、注册/建连限流。
//!
//! 每个测试各自启动一个内存数据库服务器，互不影响。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use screenwall_proto::cloud::{encode_tunnel_frame, TunnelKind};
use screenwall_server::config::{Config, LimitsConfig};
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::PeerAddr;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: true,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    }
}

/// 启动服务器，返回 `(地址, 状态)`。
///
/// 返回 `AppState` 便于测试直接读内部计数或改配额。
async fn start_server_with(limits: LimitsConfig) -> (String, Arc<AppState>) {
    let state = Arc::new(AppState::with_limits(
        Db::open_in_memory().expect("内存数据库"),
        test_config(),
        limits,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    let served = state.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(served).into_make_service_with_connect_info::<PeerAddr>(),
        )
        .await
        .expect("服务器退出");
    });

    let base = format!("http://{addr}");
    for _ in 0..40 {
        if reqwest::get(format!("{base}/healthz")).await.is_ok() {
            return (addr.to_string(), state);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("服务器没有就绪");
}

async fn start_server() -> String {
    start_server_with(LimitsConfig::default()).await.0
}

async fn register(client: &reqwest::Client, addr: &str, username: &str) -> reqwest::Response {
    client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": username, "password": "password123" }))
        .send()
        .await
        .expect("注册请求")
}

async fn register_ok(client: &reqwest::Client, addr: &str, username: &str) {
    let response = register(client, addr, username).await;
    assert!(
        response.status().is_success(),
        "注册失败: {:?}",
        response.status()
    );
}

async fn login(
    client: &reqwest::Client,
    addr: &str,
    username: &str,
    device_id: &str,
) -> reqwest::Response {
    client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": username,
            "password": "password123",
            "device": {
                "id": device_id,
                "name": device_id,
                "os": "Windows 11",
                "version": "0.1.0"
            }
        }))
        .send()
        .await
        .expect("登录请求")
}

async fn login_token(client: &reqwest::Client, addr: &str, username: &str, device: &str) -> String {
    let response = login(client, addr, username, device).await;
    let status = response.status();
    let body: Value = response.json().await.expect("登录响应");
    assert!(status.is_success(), "登录失败 ({device}): {status} {body}");
    body["token"].as_str().expect("token").to_string()
}

async fn metrics(client: &reqwest::Client, addr: &str, token: &str) -> Value {
    let response = client
        .get(format!("http://{addr}/api/v1/metrics"))
        .bearer_auth(token)
        .send()
        .await
        .expect("metrics 请求");
    assert_eq!(response.status(), reqwest::StatusCode::OK, "metrics 应成功");
    response.json().await.expect("metrics JSON")
}

async fn connect(
    addr: &str,
    path: &str,
    token: &str,
) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
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

#[tokio::test]
async fn metrics_reflect_real_tunnel_traffic() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;

    let ctrl_token = login_token(&client, &addr, "kay", "dev-ctrl").await;
    let agent_token = login_token(&client, &addr, "kay", "dev-target").await;

    // 初始：没有会话、没有字节。
    let before = metrics(&client, &addr, &ctrl_token).await;
    assert_eq!(before["sessions"]["total"], 0, "初始应无会话: {before}");
    assert_eq!(before["bytes"]["toDevices"], 0);
    assert_eq!(before["bytes"]["toViewers"], 0);
    assert_eq!(before["devices"]["accounts"], 1);

    let mut agent = connect(
        &addr,
        "/ws/device?device_id=dev-target&name=Target&os=Windows&version=0.1.0",
        &agent_token,
    )
    .await;
    assert_eq!(next_json(&mut agent).await["t"], "ready");

    let mut viewer = connect(&addr, "/ws/view", &ctrl_token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    next_json_t(&mut viewer, "devices").await;

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 1, "deviceId": "dev-target", "viewer": "Desk" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let open = next_json_t(&mut agent, "open").await;
    let session = open["session"].as_u64().expect("session") as u32;
    agent
        .send(WsMessage::Text(
            json!({ "t": "open.ok", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open.ok");
    assert_eq!(
        next_json_t(&mut viewer, "open.ok").await["session"],
        session
    );

    let up_payload = b"metrics-up";
    let up = encode_tunnel_frame(TunnelKind::Json, session, up_payload);
    viewer
        .send(WsMessage::Binary(up.clone().into()))
        .await
        .expect("上行帧");
    assert_eq!(next_binary(&mut agent).await, up);

    let down_payload = b"metrics-down";
    let down = encode_tunnel_frame(TunnelKind::Binary, session, down_payload);
    agent
        .send(WsMessage::Binary(down.clone().into()))
        .await
        .expect("下行帧");
    assert_eq!(next_binary(&mut viewer).await, down);

    let during = metrics(&client, &addr, &ctrl_token).await;
    assert_eq!(
        during["sessions"]["active"], 1,
        "应有一条活跃会话: {during}"
    );
    assert_eq!(during["sessions"]["total"], 1, "累计会话应为 1: {during}");
    assert_eq!(during["sessions"]["peak"], 1, "峰值为 1: {during}");
    assert_eq!(
        during["bytes"]["toDevices"].as_u64().unwrap_or(0),
        up_payload.len() as u64,
        "主控→设备字节: {during}"
    );
    assert_eq!(
        during["bytes"]["toViewers"].as_u64().unwrap_or(0),
        down_payload.len() as u64,
        "设备→主控字节: {during}"
    );
    assert_eq!(during["devices"]["online"], 1, "一台设备在线: {during}");
    assert_eq!(during["devices"]["accounts"], 1);
    assert!(during["handshakes"]["ok"].as_u64().unwrap_or(0) >= 2);
    assert!(during["uptimeSecs"].is_u64());
    assert!(during["errors"]["byCode"].is_object());

    // 关闭会话后活跃数归零，累计数与字节数保留。
    viewer
        .send(WsMessage::Text(
            json!({ "t": "close", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("关闭会话");
    assert_eq!(next_json_t(&mut agent, "close").await["session"], session);

    let after = metrics(&client, &addr, &ctrl_token).await;
    assert_eq!(after["sessions"]["active"], 0);
    assert_eq!(after["sessions"]["total"], 1);
    assert_eq!(
        after["bytes"]["toDevices"].as_u64(),
        Some(up_payload.len() as u64)
    );

    // 未登录不可访问。
    let unauthorized = client
        .get(format!("http://{addr}/api/v1/metrics"))
        .send()
        .await
        .expect("未鉴权 metrics");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 旧接口仍可用（兼容已有监控脚本）。
    let legacy = client
        .get(format!("http://{addr}/metrics"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("legacy metrics");
    assert_eq!(legacy.status(), reqwest::StatusCode::OK);
    let legacy: Value = legacy.json().await.expect("legacy JSON");
    assert_eq!(legacy["onlineDevices"], 1);
    assert_eq!(legacy["activeSessions"], 0);
}

#[tokio::test]
async fn metrics_count_handshake_failures_and_error_codes() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;
    let token = login_token(&client, &addr, "kay", "dev-ctrl").await;

    // 未带令牌的 WS 握手应被拒绝并计入失败。
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/view"))
            .await
            .is_err(),
        "未鉴权的 /ws/view 应被拒绝"
    );

    // 打开一台不存在的设备：产生 open.err 4200。
    let mut viewer = connect(&addr, "/ws/view", &token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    next_json_t(&mut viewer, "devices").await;
    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 9, "deviceId": "dev-missing" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let error = next_json_t(&mut viewer, "open.err").await;
    assert_eq!(error["code"], 4200);

    let snapshot = metrics(&client, &addr, &token).await;
    assert!(
        snapshot["handshakes"]["failed"].as_u64().unwrap_or(0) >= 1,
        "应记录握手失败: {snapshot}"
    );
    assert!(
        snapshot["handshakes"]["ok"].as_u64().unwrap_or(0) >= 1,
        "应记录握手成功: {snapshot}"
    );
    assert_eq!(
        snapshot["errors"]["byCode"]["4200"].as_u64(),
        Some(1),
        "4200 错误码应被计数: {snapshot}"
    );
    assert!(snapshot["errors"]["total"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn device_limit_rejects_new_devices_but_allows_relogin() {
    let (addr, _state) = start_server_with(LimitsConfig {
        max_devices_per_account: 2,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;

    // 上限内：两台设备都能绑定。
    login_token(&client, &addr, "kay", "dev-a").await;
    login_token(&client, &addr, "kay", "dev-b").await;

    // 超限：第三台被明确拒绝（403 + 中文提示），不是静默失败。
    let response = login(&client, &addr, "kay", "dev-c").await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "第三台设备应被拒绝"
    );
    let body: Value = response.json().await.expect("限流响应");
    assert_eq!(body["error"], "device_limit_reached");
    let msg = body["msg"].as_str().expect("msg");
    assert!(msg.contains("最多绑定 2 台设备"), "中文提示: {msg}");

    // 已登记设备重复登录不受影响，也不会占用新的名额。
    let again = login(&client, &addr, "kay", "dev-a").await;
    assert_eq!(
        again.status(),
        reqwest::StatusCode::OK,
        "重复登录已有设备应成功"
    );
    login_token(&client, &addr, "kay", "dev-b").await;

    // 删除一台后可再绑定一台。
    let manager = login_token(&client, &addr, "kay", "dev-b").await;
    let removed = client
        .delete(format!("http://{addr}/api/v1/devices/dev-a"))
        .bearer_auth(&manager)
        .send()
        .await
        .expect("删除设备");
    assert!(removed.status().is_success(), "删除设备失败");
    login_token(&client, &addr, "kay", "dev-c").await;
}

#[tokio::test]
async fn device_limit_zero_means_unlimited() {
    let (addr, _state) = start_server_with(LimitsConfig {
        max_devices_per_account: 0,
        // 本用例只验证设备上限，关闭登录频率限制避免误判。
        login_per_minute: 0,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;

    for index in 0..25 {
        login_token(&client, &addr, "kay", &format!("dev-{index}")).await;
    }
}

#[tokio::test]
async fn register_rate_limit_returns_429_and_recovers() {
    let (addr, _state) = start_server_with(LimitsConfig {
        // 每秒补充 1 个令牌，桶容量 2。
        // 注册要做 argon2 哈希（耗时不定），串行请求之间可能已经补充令牌，
        // 所以这里并发连发 6 个：令牌检查都在最初几毫秒内完成，必然只放行 2 个。
        register_per_hour: 3_600,
        register_burst: 2,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();

    let mut tasks = Vec::new();
    for index in 0..6 {
        let client = client.clone();
        let addr = addr.clone();
        tasks.push(tokio::spawn(async move {
            register(&client, &addr, &format!("user-{index}")).await
        }));
    }

    let mut ok = 0;
    let mut limited = Vec::new();
    for task in tasks {
        let response = task.await.expect("注册任务");
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            limited.push(response);
        } else {
            assert!(
                response.status().is_success(),
                "意外状态: {:?}",
                response.status()
            );
            ok += 1;
        }
    }
    assert_eq!(ok, 2, "桶容量 2，只应放行两次注册");
    assert_eq!(limited.len(), 4, "其余四次应被限流");

    let limited = limited.into_iter().next().expect("应有限流响应");
    assert!(
        limited.headers().contains_key(reqwest::header::RETRY_AFTER),
        "应带 Retry-After"
    );
    let body: Value = limited.json().await.expect("限流响应");
    assert_eq!(body["error"], "rate_limited");
    assert!(
        body["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("秒后重试"),
        "中文提示: {body}"
    );

    // 窗口过后恢复：等 1.1 秒即可补充 1 个令牌（每秒 1 个）。
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let recovered = register(&client, &addr, "user-recovered").await;
    assert!(
        recovered.status().is_success(),
        "窗口过后应恢复: {:?}",
        recovered.status()
    );
}

#[tokio::test]
async fn view_connect_rate_limit_returns_429_and_recovers() {
    let (addr, _state) = start_server_with(LimitsConfig {
        // 每秒补充 1 个令牌，桶容量 1。
        view_connects_per_minute: 60,
        view_connect_burst: 1,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;
    let token = login_token(&client, &addr, "kay", "dev-ctrl").await;

    let mut first = connect(&addr, "/ws/view", &token).await;
    assert_eq!(next_json(&mut first).await["t"], "ready");

    // 第二次建连超限：握手阶段返回 429。
    let error = tokio_tungstenite::connect_async({
        let mut request = format!("ws://{addr}/ws/view")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("令牌头"),
        );
        request
    })
    .await
    .expect_err("第二次建连应被限流");
    let message = error.to_string();
    assert!(message.contains("429"), "应返回 429 状态: {message}");

    // 每秒补充 1 个令牌，等待 1.1 秒后恢复。
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let mut again = connect(&addr, "/ws/view", &token).await;
    assert_eq!(next_json(&mut again).await["t"], "ready");
}

#[tokio::test]
async fn session_open_rate_limit_replies_open_err() {
    let (addr, _state) = start_server_with(LimitsConfig {
        // 每秒补充 1 个令牌，桶容量 1。
        session_opens_per_minute: 60,
        session_open_burst: 1,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;
    let ctrl_token = login_token(&client, &addr, "kay", "dev-ctrl").await;
    let agent_token = login_token(&client, &addr, "kay", "dev-target").await;

    let mut agent = connect(
        &addr,
        "/ws/device?device_id=dev-target&name=Target&os=Windows&version=0.1.0",
        &agent_token,
    )
    .await;
    assert_eq!(next_json(&mut agent).await["t"], "ready");

    let mut viewer = connect(&addr, "/ws/view", &ctrl_token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    next_json_t(&mut viewer, "devices").await;

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 1, "deviceId": "dev-target" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    assert_eq!(next_json_t(&mut agent, "open").await["t"], "open");

    // 第二次 open 超限：回 open.err 4205。
    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": 2, "deviceId": "dev-target" })
                .to_string()
                .into(),
        ))
        .await
        .expect("发送 open");
    let error = next_json_t(&mut viewer, "open.err").await;
    assert_eq!(error["request"], 2);
    assert_eq!(error["code"], 4205);
    assert!(error["msg"]
        .as_str()
        .unwrap_or_default()
        .contains("秒后重试"));

    let snapshot = metrics(&client, &addr, &ctrl_token).await;
    assert_eq!(
        snapshot["errors"]["byCode"]["4205"].as_u64(),
        Some(1),
        "4205 应计入错误码: {snapshot}"
    );
}

#[tokio::test]
async fn login_request_rate_limit_returns_429() {
    let (addr, _state) = start_server_with(LimitsConfig {
        // 每分钟补充 1 个令牌，桶容量 2：两次登录后必然限流。
        // 生产默认值是每分钟 30 个、桶容量 30，不会误伤正常使用。
        login_per_minute: 1,
        login_burst: 2,
        ..LimitsConfig::default()
    })
    .await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "kay").await;

    // 两次成功登录消耗掉桶里的两个令牌。
    assert!(login(&client, &addr, "kay", "dev-a")
        .await
        .status()
        .is_success());
    assert!(login(&client, &addr, "kay", "dev-a")
        .await
        .status()
        .is_success());

    // 第三次：桶已空，直接 429（不进入密码校验）。
    let limited = login(&client, &addr, "kay", "dev-a").await;
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert!(
        limited.headers().contains_key(reqwest::header::RETRY_AFTER),
        "应带 Retry-After"
    );
    let body: Value = limited.json().await.expect("限流响应");
    assert_eq!(body["error"], "rate_limited");
    assert!(
        body["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("秒后重试"),
        "中文提示: {body}"
    );
}

/// 浏览器 `WebSocket` 不能设置请求头，网页端把令牌放进查询串。
/// 这条路径只对 `/ws/view` 生效，HTTP 接口仍然只认 Bearer 头。
#[tokio::test]
async fn ws_view_accepts_token_in_query_for_browsers() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register_ok(&client, &addr, "webuser").await;
    let token = login_token(&client, &addr, "webuser", "WEB-1").await;

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/view?token={token}"))
            .await
            .expect("查询串令牌应能建立连接");
    assert_eq!(next_json(&mut ws).await["t"], "ready");
    assert_eq!(next_json(&mut ws).await["t"], "devices");

    let bad = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/view?token=wrong"))
        .await;
    assert!(bad.is_err(), "错误令牌不应建立连接");

    // 账号接口不接受查询串令牌：仍要求 Bearer 头。
    let me = client
        .get(format!("http://{addr}/api/v1/me?token={token}"))
        .send()
        .await
        .expect("me 请求");
    assert_eq!(me.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `--web-dir` 挂载网页端：根路径跳转到 `/app/`，未知子路径回落 `index.html`。
#[tokio::test]
async fn web_dir_serves_spa_under_app_prefix() {
    let dir = std::env::temp_dir().join(format!("screenwall-web-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    std::fs::write(dir.join("index.html"), "<!doctype html><title>spa</title>").expect("写 index");

    let state = Arc::new(AppState::with_limits(
        Db::open_in_memory().expect("内存数据库"),
        test_config(),
        LimitsConfig::default(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    let served = screenwall_server::router_with_web(state, Some(dir.clone()));
    tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<PeerAddr>(),
        )
        .await
        .expect("服务器退出");
    });

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("客户端");
    let root = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("根路径");
    assert_eq!(root.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(root.headers()["location"], "/app/");

    let app = client
        .get(format!("http://{addr}/app/"))
        .send()
        .await
        .expect("/app/ 请求");
    assert_eq!(app.status(), reqwest::StatusCode::OK);
    assert!(app.text().await.expect("读文本").contains("spa"));

    // 前端自己的路由（例如 /app/control）由 SPA 接管。
    let deep = client
        .get(format!("http://{addr}/app/control"))
        .send()
        .await
        .expect("深路径请求");
    assert_eq!(deep.status(), reqwest::StatusCode::OK);

    let _ = std::fs::remove_dir_all(dir);
}
