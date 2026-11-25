//! 自托管更新端到端测试：版本信息、SHA256、流式下载与路径穿越防护。
//!
//! 每个测试用独立的临时目录与假安装包（几十字节），不触碰仓库里的真实产物。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use screenwall_server::config::{Config, UpdateConfig};
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::PeerAddr;
use screenwall_server::update::UpdateState;
use serde_json::Value;
use sha2::{Digest, Sha256};

const FILE: &str = "ScreenWall_0.1.0_x64-setup.exe";
const VERSION: &str = "0.1.0";
const NOTES: &str = "测试更新说明";
const PAYLOAD: &[u8] = b"ScreenWall fake installer payload for tests (not a real exe).";

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: false,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    }
}

/// 唯一临时目录，测试结束时删除。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "screenwall-update-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

async fn start_server(update: UpdateState) -> String {
    let mut state = AppState::new(Db::open_in_memory().expect("内存数据库"), test_config());
    state.update = update;
    let state = Arc::new(state);
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

fn configured(dir: &std::path::Path) -> UpdateState {
    UpdateState::from_config(&UpdateConfig {
        dir: Some(dir.to_path_buf()),
        version: Some(VERSION.to_string()),
        file: Some(FILE.to_string()),
        notes: Some(NOTES.to_string()),
    })
}

#[tokio::test]
async fn version_reports_cached_sha256() {
    let tmp = TempDir::new("version");
    std::fs::write(tmp.path().join(FILE), PAYLOAD).expect("写入假安装包");
    let addr = start_server(configured(tmp.path())).await;

    let client = reqwest::Client::new();
    // 连查两次，确认缓存路径也不会改变结果。
    for _ in 0..2 {
        let response = client
            .get(format!("http://{addr}/api/v1/version"))
            .send()
            .await
            .expect("版本请求");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("版本响应");
        assert_eq!(body["version"], VERSION);
        assert_eq!(body["sha256"], sha256_hex(PAYLOAD));
        assert_eq!(body["notes"], NOTES);
        assert_eq!(body["url"], format!("/downloads/{FILE}"));
        let published = body["publishedAt"].as_i64().expect("publishedAt");
        assert!(published > 0, "publishedAt 应为正数: {published}");
    }
}

#[tokio::test]
async fn download_streams_exact_file() {
    let tmp = TempDir::new("download");
    std::fs::write(tmp.path().join(FILE), PAYLOAD).expect("写入假安装包");
    let addr = start_server(configured(tmp.path())).await;

    let response = reqwest::get(format!("http://{addr}/downloads/{FILE}"))
        .await
        .expect("下载请求");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/octet-stream")
    );
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("Content-Disposition");
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(disposition.contains(FILE), "{disposition}");

    let bytes = response.bytes().await.expect("下载内容");
    assert_eq!(&bytes[..], PAYLOAD);
    assert_eq!(sha256_hex(&bytes), sha256_hex(PAYLOAD));
}

#[tokio::test]
async fn unconfigured_returns_404() {
    let addr = start_server(UpdateState::default()).await;

    let version = reqwest::get(format!("http://{addr}/api/v1/version"))
        .await
        .expect("版本请求");
    assert_eq!(version.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = version.json().await.expect("版本响应");
    assert_eq!(body["msg"], "未配置更新");

    let download = reqwest::get(format!("http://{addr}/downloads/{FILE}"))
        .await
        .expect("下载请求");
    assert_eq!(download.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_file_returns_404() {
    let tmp = TempDir::new("missing");
    let addr = start_server(configured(tmp.path())).await;

    let version = reqwest::get(format!("http://{addr}/api/v1/version"))
        .await
        .expect("版本请求");
    assert_eq!(version.status(), reqwest::StatusCode::NOT_FOUND);

    let download = reqwest::get(format!("http://{addr}/downloads/{FILE}"))
        .await
        .expect("下载请求");
    assert_eq!(download.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn path_traversal_is_rejected() {
    let tmp = TempDir::new("traversal");
    // 白名单文件在子目录里，同时把敏感内容放在上级目录；
    // 任何穿越尝试都必须拿不到上级目录里的内容。
    let secret_dir = tmp.path().join("secret");
    std::fs::create_dir_all(&secret_dir).expect("创建目录");
    std::fs::write(secret_dir.join("secret.txt"), b"TOP-SECRET").expect("写入敏感文件");
    std::fs::write(secret_dir.join(FILE), PAYLOAD).expect("写入假安装包");
    let addr = start_server(configured(&secret_dir)).await;

    for path in [
        "/downloads/..%2Fsecret.txt".to_string(),
        "/downloads/../secret.txt".to_string(),
        "/downloads/..%5Csecret.txt".to_string(),
        "/downloads/%2E%2E%2Fsecret.txt".to_string(),
        format!("/downloads/{FILE}%00.txt"),
        "/downloads/other.exe".to_string(),
    ] {
        let response = reqwest::get(format!("http://{addr}{path}"))
            .await
            .expect("穿越请求");
        assert!(
            response.status() == reqwest::StatusCode::BAD_REQUEST
                || response.status() == reqwest::StatusCode::NOT_FOUND,
            "{path} 返回了 {:?}",
            response.status()
        );
        let body = response.bytes().await.unwrap_or_default();
        assert!(
            !body
                .windows(b"TOP-SECRET".len())
                .any(|w| w == b"TOP-SECRET"),
            "{path} 泄漏了上级目录内容"
        );
    }

    // 正常白名单文件仍可下载，确认拒绝的是路径而不是整个接口。
    let ok = reqwest::get(format!("http://{addr}/downloads/{FILE}"))
        .await
        .expect("正常下载");
    assert_eq!(ok.status(), reqwest::StatusCode::OK);
}
