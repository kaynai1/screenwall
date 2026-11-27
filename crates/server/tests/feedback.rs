//! 用户反馈（建议与 BUG）：账号令牌提交、内容校验、落库与管理员查看。

use std::sync::Arc;
use std::time::Duration;

use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::PeerAddr;
use serde_json::{json, Value};

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: true,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    }
}

/// 启动服务器，返回监听地址与共享状态（测试里直接查库断言）。
async fn start_server() -> (String, Arc<AppState>) {
    let state = Arc::new(AppState::new(
        Db::open_in_memory().expect("内存数据库"),
        test_config(),
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
            return (base, state);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("服务器没有就绪");
}

/// 注册并登录，返回账号令牌。
async fn login(base: &str, username: &str) -> String {
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
            "device": { "name": "测试机", "os": "Windows", "version": "0.2.20" }
        }))
        .send()
        .await
        .expect("登录请求")
        .json()
        .await
        .expect("登录响应");
    value["token"].as_str().expect("登录令牌").to_string()
}

#[tokio::test]
async fn submit_feedback_saves_row_with_device_snapshot() {
    let (base, state) = start_server().await;
    let client = reqwest::Client::new();
    let token = login(&base, "feedback-user").await;

    let response = client
        .post(format!("{base}/api/v1/feedback"))
        .bearer_auth(&token)
        .json(&json!({
            "category": "bug",
            "content": "  控制页音量滑块拖不动  ",
            "contact": "user@example.com",
            "appVersion": "0.2.20"
        }))
        .send()
        .await
        .expect("提交反馈");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("响应 JSON");
    assert!(body["id"].as_i64().expect("反馈 id") > 0);

    let rows = state.db.list_feedback(10).expect("读取反馈");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.username, "feedback-user");
    // 提交时快照设备名，之后设备改名也能对上。
    assert_eq!(row.device_name, "测试机");
    assert!(!row.device_id.is_empty());
    assert_eq!(row.category, "bug");
    // 内容去掉首尾空白后入库。
    assert_eq!(row.content, "控制页音量滑块拖不动");
    assert_eq!(row.contact, "user@example.com");
    assert_eq!(row.app_version, "0.2.20");
}

#[tokio::test]
async fn submit_feedback_rejects_bad_input() {
    let (base, _state) = start_server().await;
    let client = reqwest::Client::new();
    let token = login(&base, "feedback-bad").await;

    // 未登录不能提交。
    let anonymous = client
        .post(format!("{base}/api/v1/feedback"))
        .json(&json!({ "category": "bug", "content": "没有账号也要发" }))
        .send()
        .await
        .expect("匿名提交");
    assert_eq!(anonymous.status(), 401);

    // 内容太短。
    let short = client
        .post(format!("{base}/api/v1/feedback"))
        .bearer_auth(&token)
        .json(&json!({ "category": "suggestion", "content": "短" }))
        .send()
        .await
        .expect("过短提交");
    assert_eq!(short.status(), 400);

    // 分类白名单之外。
    let category = client
        .post(format!("{base}/api/v1/feedback"))
        .bearer_auth(&token)
        .json(&json!({ "category": "other", "content": "分类写错了也要发" }))
        .send()
        .await
        .expect("错误分类提交");
    assert_eq!(category.status(), 400);
}
