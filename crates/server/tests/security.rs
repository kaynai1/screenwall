//! 安全加固端到端测试：登录限流、按设备吊销令牌、审计与指标。
//!
//! 这些测试各自启动一个内存数据库服务器，互不影响。

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

async fn register(client: &reqwest::Client, addr: &str, username: &str) {
    let response = client
        .post(format!("http://{addr}/api/v1/register"))
        .json(&json!({ "username": username, "password": "password123" }))
        .send()
        .await
        .expect("注册请求");
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
    password: &str,
    device_id: &str,
) -> reqwest::Response {
    client
        .post(format!("http://{addr}/api/v1/login"))
        .json(&json!({
            "username": username,
            "password": password,
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

async fn login_token(
    client: &reqwest::Client,
    addr: &str,
    username: &str,
    device_id: &str,
) -> String {
    let response = login(client, addr, username, "password123", device_id).await;
    assert!(
        response.status().is_success(),
        "登录失败: {:?}",
        response.status()
    );
    let body: Value = response.json().await.expect("登录响应");
    body["token"].as_str().expect("token").to_string()
}

async fn connect_result(
    addr: &str,
    path: &str,
    token: &str,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let mut request = format!("ws://{addr}{path}")
        .into_client_request()
        .expect("请求");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("令牌头"),
    );
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
        .map_err(|error| error.to_string())
}

async fn connect(
    addr: &str,
    path: &str,
    token: &str,
) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
    connect_result(addr, path, token)
        .await
        .expect("连接 WebSocket")
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

/// 建立一次完整会话（被控端上线 → 主控打开 → 被控确认 → 主控关闭），返回会话编号。
///
/// 关闭后设备收到 `close`，确保审计已落库。
async fn run_control_session(
    addr: &str,
    ctrl_token: &str,
    agent_device: &str,
    agent_token: &str,
    request: u32,
) -> u32 {
    let mut agent = connect(
        addr,
        &format!(
            "/ws/device?device_id={agent_device}&name={agent_device}&os=Windows&version=0.1.0"
        ),
        agent_token,
    )
    .await;
    assert_eq!(next_json(&mut agent).await["t"], "ready");

    let mut viewer = connect(addr, "/ws/view", ctrl_token).await;
    assert_eq!(next_json(&mut viewer).await["t"], "ready");
    next_json_t(&mut viewer, "devices").await;

    viewer
        .send(WsMessage::Text(
            json!({ "t": "open", "request": request, "deviceId": agent_device, "viewer": "Desk" })
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

    viewer
        .send(WsMessage::Text(
            json!({ "t": "close", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("关闭会话");
    assert_eq!(next_json_t(&mut agent, "close").await["session"], session);
    session
}

async fn get_audits(client: &reqwest::Client, addr: &str, token: &str, query: &str) -> Value {
    let response = client
        .get(format!("http://{addr}/api/v1/audits?{query}"))
        .bearer_auth(token)
        .send()
        .await
        .expect("audits 请求");
    assert_eq!(response.status(), reqwest::StatusCode::OK, "audits 应成功");
    response.json().await.expect("audits JSON")
}

fn audit_ids(body: &Value) -> Vec<u64> {
    body["audits"]
        .as_array()
        .expect("audits 数组")
        .iter()
        .map(|row| row["id"].as_u64().expect("审计 id"))
        .collect()
}

#[tokio::test]
async fn audits_paginate_and_filter_by_device() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register(&client, &addr, "kay").await;

    let ctrl_token = login_token(&client, &addr, "kay", "dev-ctrl").await;
    let agent_a = login_token(&client, &addr, "kay", "dev-a").await;
    let agent_b = login_token(&client, &addr, "kay", "dev-b").await;

    // 三次会话：dev-ctrl 分别控制 dev-a、dev-b、dev-a。
    let s1 = run_control_session(&addr, &ctrl_token, "dev-a", &agent_a, 1).await;
    let s2 = run_control_session(&addr, &ctrl_token, "dev-b", &agent_b, 2).await;
    let s3 = run_control_session(&addr, &ctrl_token, "dev-a", &agent_a, 3).await;

    // 第一页：total 为总数，按开始时间倒序（新会话在前）。
    let page1 = get_audits(&client, &addr, &ctrl_token, "limit=2&offset=0").await;
    assert_eq!(page1["total"], 3, "total 应为全部记录数: {page1}");
    let page1_ids = audit_ids(&page1);
    assert_eq!(page1_ids.len(), 2);
    assert_eq!(
        page1["audits"][0]["sessionId"], s3,
        "最新会话应排在最前: {page1}"
    );
    assert_eq!(page1["audits"][1]["sessionId"], s2);
    assert_eq!(page1["audits"][0]["controllerDeviceId"], "dev-ctrl");
    assert_eq!(page1["audits"][0]["controlledDeviceId"], "dev-a");

    // 第二页：与第一页不重复，覆盖剩余记录。
    let page2 = get_audits(&client, &addr, &ctrl_token, "limit=2&offset=2").await;
    assert_eq!(page2["total"], 3);
    let page2_ids = audit_ids(&page2);
    assert_eq!(page2_ids.len(), 1);
    assert_eq!(page2["audits"][0]["sessionId"], s1);
    assert!(
        page1_ids.iter().all(|id| !page2_ids.contains(id)),
        "两页不应重复: {page1_ids:?} vs {page2_ids:?}"
    );

    // offset 超过总数：空页，total 不变。
    let beyond = get_audits(&client, &addr, &ctrl_token, "limit=2&offset=10").await;
    assert_eq!(beyond["total"], 3);
    assert!(audit_ids(&beyond).is_empty());

    // deviceId 按主控端过滤：dev-ctrl 命中全部三条。
    let by_controller = get_audits(&client, &addr, &ctrl_token, "deviceId=dev-ctrl").await;
    assert_eq!(by_controller["total"], 3);
    assert_eq!(audit_ids(&by_controller).len(), 3);

    // deviceId 按被控端过滤：dev-a 命中两条，total 也是过滤后的总数。
    let by_agent = get_audits(&client, &addr, &ctrl_token, "deviceId=dev-a").await;
    assert_eq!(by_agent["total"], 2, "过滤后 total 应为 2: {by_agent}");
    assert_eq!(
        by_agent["audits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["sessionId"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![s3 as u64, s1 as u64]
    );

    // 过滤 + 分页：offset 在过滤结果内生效。
    let filtered_page2 = get_audits(
        &client,
        &addr,
        &ctrl_token,
        "limit=1&offset=1&deviceId=dev-a",
    )
    .await;
    assert_eq!(filtered_page2["total"], 2);
    assert_eq!(filtered_page2["audits"][0]["sessionId"], s1);

    // 空 deviceId 等价于不过滤。
    let empty_device = get_audits(&client, &addr, &ctrl_token, "deviceId=&limit=10").await;
    assert_eq!(empty_device["total"], 3);
    assert_eq!(audit_ids(&empty_device).len(), 3);

    // 不存在的设备：空结果、total 为 0。
    let missing = get_audits(&client, &addr, &ctrl_token, "deviceId=dev-missing").await;
    assert_eq!(missing["total"], 0);
    assert!(audit_ids(&missing).is_empty());

    // 未鉴权仍返回 401。
    let unauthorized = client
        .get(format!("http://{addr}/api/v1/audits?limit=10"))
        .send()
        .await
        .expect("未鉴权 audits");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_locks_after_five_failures() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register(&client, &addr, "kay").await;
    register(&client, &addr, "other").await;

    for attempt in 1..=4 {
        let response = login(&client, &addr, "kay", "wrong-password", "dev-kay").await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "第 {attempt} 次失败应返回 401"
        );
    }

    // 第 5 次失败触发锁定，直接返回 429。
    let response = login(&client, &addr, "kay", "wrong-password", "dev-kay").await;
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response
            .headers()
            .contains_key(reqwest::header::RETRY_AFTER),
        "应带 Retry-After"
    );
    let body: Value = response.json().await.expect("限流响应");
    let msg = body["msg"].as_str().expect("msg");
    assert!(msg.contains("秒后重试"), "中文提示: {msg}");

    // 锁定期内即使密码正确也拒绝。
    let response = login(&client, &addr, "kay", "password123", "dev-kay").await;
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    // 其他用户名不受影响。
    let response = login(&client, &addr, "other", "password123", "dev-other").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn revoked_device_token_rejects_api_and_ws() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register(&client, &addr, "kay").await;

    let ctrl_token = login_token(&client, &addr, "kay", "dev-ctrl").await;
    let agent_token = login_token(&client, &addr, "kay", "dev-a").await;

    let me = client
        .get(format!("http://{addr}/api/v1/me"))
        .bearer_auth(&agent_token)
        .send()
        .await
        .expect("me 请求");
    assert_eq!(me.status(), reqwest::StatusCode::OK);

    let revoke = client
        .post(format!("http://{addr}/api/v1/devices/dev-a/revoke"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("吊销请求");
    assert_eq!(revoke.status(), reqwest::StatusCode::OK);
    let body: Value = revoke.json().await.expect("吊销响应");
    assert_eq!(body["ok"], true);
    assert!(body["revoked"].as_u64().unwrap_or(0) >= 1);

    // 被吊销令牌的 /api 立即失效。
    let me = client
        .get(format!("http://{addr}/api/v1/me"))
        .bearer_auth(&agent_token)
        .send()
        .await
        .expect("me 请求");
    assert_eq!(me.status(), reqwest::StatusCode::UNAUTHORIZED);

    // /ws/device 与 /ws/view 都应拒绝该令牌。
    assert!(
        connect_result(&addr, "/ws/device?device_id=dev-a", &agent_token)
            .await
            .is_err(),
        "被吊销令牌不应能建立设备长连接"
    );
    assert!(
        connect_result(&addr, "/ws/view", &agent_token)
            .await
            .is_err(),
        "被吊销令牌不应能建立主控长连接"
    );

    // 其他设备的令牌不受影响。
    let me = client
        .get(format!("http://{addr}/api/v1/me"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("me 请求");
    assert_eq!(me.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn audit_and_metrics_cover_session_lifecycle() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register(&client, &addr, "kay").await;

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
            json!({ "t": "open", "request": 7, "deviceId": "dev-target", "viewer": "Desk" })
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

    let up_payload = b"up-payload";
    let up = encode_tunnel_frame(TunnelKind::Json, session, up_payload);
    viewer
        .send(WsMessage::Binary(up.clone().into()))
        .await
        .expect("上行帧");
    assert_eq!(next_binary(&mut agent).await, up);

    let down_payload = b"down-payload!!";
    let down = encode_tunnel_frame(TunnelKind::Binary, session, down_payload);
    agent
        .send(WsMessage::Binary(down.clone().into()))
        .await
        .expect("下行帧");
    assert_eq!(next_binary(&mut viewer).await, down);

    // 会话进行中：指标可见。
    let metrics = client
        .get(format!("http://{addr}/metrics"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("metrics 请求");
    assert_eq!(metrics.status(), reqwest::StatusCode::OK);
    let metrics: Value = metrics.json().await.expect("metrics JSON");
    // 只有被控端建立 /ws/device 长连接，主控端不计入在线设备。
    assert_eq!(metrics["onlineDevices"], 1);
    assert_eq!(metrics["activeSessions"], 1);
    let expected = (up_payload.len() + down_payload.len()) as u64;
    assert!(metrics["totalBytes"].as_u64().unwrap_or(0) >= expected);
    assert!(metrics.get("errorCount").is_some());

    // 关闭会话：等设备收到 close，确保审计已落库。
    viewer
        .send(WsMessage::Text(
            json!({ "t": "close", "session": session })
                .to_string()
                .into(),
        ))
        .await
        .expect("关闭会话");
    assert_eq!(next_json_t(&mut agent, "close").await["session"], session);

    let audits = client
        .get(format!("http://{addr}/api/v1/audits?limit=10"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("audits 请求");
    assert_eq!(audits.status(), reqwest::StatusCode::OK);
    let body: Value = audits.json().await.expect("audits JSON");
    assert_eq!(body["total"], 1, "应报告总数: {body}");
    let rows = body["audits"].as_array().expect("audits 数组");
    assert_eq!(rows.len(), 1, "应有一条审计记录: {body}");
    let row = &rows[0];
    assert_eq!(row["sessionId"], session);
    assert_eq!(row["controllerDeviceId"], "dev-ctrl");
    assert_eq!(row["controlledDeviceId"], "dev-target");
    assert!(row["endedAt"].as_i64().is_some(), "结束时间应已补齐");
    assert!(row["durationMs"].as_i64().unwrap_or(-1) >= 0);
    assert_eq!(row["bytesUp"], up_payload.len() as u64);
    assert_eq!(row["bytesDown"], down_payload.len() as u64);

    // 会话结束后活跃会话归零。
    let metrics = client
        .get(format!("http://{addr}/metrics"))
        .bearer_auth(&ctrl_token)
        .send()
        .await
        .expect("metrics 请求");
    let metrics: Value = metrics.json().await.expect("metrics JSON");
    assert_eq!(metrics["activeSessions"], 0);

    // 两个接口都要求鉴权。
    let unauthorized = client
        .get(format!("http://{addr}/api/v1/audits"))
        .send()
        .await
        .expect("未鉴权 audits");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unauthorized = client
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("未鉴权 metrics");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// 直连审计：客户端上报后能查到一条 `mode=direct`、`sessionId=0` 的记录；
/// 台账边界由服务端校验，脏数据不许落库。
#[tokio::test]
async fn direct_audit_report_is_recorded_and_validated() {
    let addr = start_server().await;
    let client = reqwest::Client::new();
    register(&client, &addr, "kay").await;
    let token = login_token(&client, &addr, "kay", "dev-ctrl").await;

    let started = 1_700_000_000_000i64;
    let response = client
        .post(format!("http://{addr}/api/v1/audits/direct"))
        .bearer_auth(&token)
        .json(&json!({
            "controllerDeviceId": "dev-ctrl",
            "controlledDeviceId": "direct:192.168.1.20:47900",
            "startedAt": started,
            "endedAt": started + 60_000,
            "durationMs": 60_000,
            "bytesUp": 1024,
            "bytesDown": 2048
        }))
        .send()
        .await
        .expect("上报直连审计");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let page = get_audits(&client, &addr, &token, "limit=10").await;
    assert_eq!(page["total"], 1, "{page}");
    let row = &page["audits"][0];
    assert_eq!(row["mode"], "direct");
    assert_eq!(row["sessionId"], 0);
    assert_eq!(row["controlledDeviceId"], "direct:192.168.1.20:47900");
    assert_eq!(row["durationMs"], 60_000);
    assert_eq!(row["bytesUp"], 1024);
    assert_eq!(row["bytesDown"], 2048);

    // 时间倒挂：拒绝。
    let bad = client
        .post(format!("http://{addr}/api/v1/audits/direct"))
        .bearer_auth(&token)
        .json(&json!({
            "controllerDeviceId": "dev-ctrl",
            "controlledDeviceId": "direct:1.2.3.4:47900",
            "startedAt": started + 10,
            "endedAt": started,
            "durationMs": 1
        }))
        .send()
        .await
        .expect("时间倒挂请求");
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // 空设备 ID：拒绝。
    let empty = client
        .post(format!("http://{addr}/api/v1/audits/direct"))
        .bearer_auth(&token)
        .json(&json!({
            "controllerDeviceId": "",
            "controlledDeviceId": "direct:1.2.3.4:47900",
            "startedAt": started,
            "endedAt": started + 1,
            "durationMs": 1
        }))
        .send()
        .await
        .expect("空 ID 请求");
    assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);

    // 未鉴权：拒绝，且不落库。
    let anonymous = client
        .post(format!("http://{addr}/api/v1/audits/direct"))
        .json(&json!({
            "controllerDeviceId": "dev-ctrl",
            "controlledDeviceId": "direct:1.2.3.4:47900",
            "startedAt": started,
            "endedAt": started + 1,
            "durationMs": 1
        }))
        .send()
        .await
        .expect("未鉴权请求");
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
    let page = get_audits(&client, &addr, &token, "limit=10").await;
    assert_eq!(page["total"], 1, "被拒的请求不应落库: {page}");
}
