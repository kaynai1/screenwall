//! 跨域：安卓壳（页面源 `http://localhost`）等跨域页面要能直接调 `/api`。

use std::sync::Arc;
use std::time::Duration;

use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;

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
        axum::serve(listener, router(state).into_make_service())
            .await
            .expect("服务器退出");
    });

    let base = format!("http://{addr}");
    for _ in 0..40 {
        if reqwest::get(format!("{base}/healthz")).await.is_ok() {
            return base;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("服务器没有就绪");
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn cross_origin_preflight_and_response_are_allowed() {
    let base = start_server().await;
    let client = reqwest::Client::new();

    // 预检：浏览器在带 JSON 体 / Authorization 头之前先发 OPTIONS。
    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/login"))
        .header("Origin", "http://localhost")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,authorization")
        .send()
        .await
        .expect("预检请求失败");
    assert!(
        preflight.status().is_success(),
        "预检被拒绝：{}",
        preflight.status()
    );
    assert_eq!(
        header(&preflight, "access-control-allow-origin"),
        "*",
        "预检缺少 Access-Control-Allow-Origin"
    );
    let allowed_headers = header(&preflight, "access-control-allow-headers").to_ascii_lowercase();
    assert!(
        allowed_headers.contains("authorization") && allowed_headers.contains("content-type"),
        "预检没放行所需请求头：{allowed_headers}"
    );

    // 真正的请求（这里故意用错密码）也要带 CORS 头，浏览器才读得到错误信息。
    let response = client
        .post(format!("{base}/api/v1/login"))
        .header("Origin", "http://localhost")
        .json(&serde_json::json!({ "username": "cors-probe", "password": "wrong-password" }))
        .send()
        .await
        .expect("登录请求失败");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        "*",
        "普通响应缺少 Access-Control-Allow-Origin"
    );
}
