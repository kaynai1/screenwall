//! TLS 冒烟测试：自签证书 → TlsListener → HTTPS 请求。
//!
//! 只验证监听层与 rustls 配置可用；证书内容由 rcgen 在临时目录生成。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use screenwall_server::config::Config;
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::{load_server_config, PeerAddr, TlsListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: "unused.db".into(),
        allow_registration: false,
        token_ttl_days: 30,
        max_frame_bytes: 1024 * 1024,
    }
}

#[tokio::test]
async fn tls_listener_serves_https() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("生成自签证书");
    let cert_der = certified.cert.der().clone();

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let dir: PathBuf =
        std::env::temp_dir().join(format!("screenwall-tls-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, certified.cert.pem()).expect("写证书");
    std::fs::write(&key_path, certified.key_pair.serialize_pem()).expect("写私钥");

    let server_config = load_server_config(&cert_path, &key_path).expect("加载 TLS 配置");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    let state = Arc::new(AppState::new(
        Db::open_in_memory().expect("内存数据库"),
        test_config(),
    ));
    tokio::spawn(async move {
        axum::serve(
            TlsListener::new(listener, server_config),
            router(state).into_make_service_with_connect_info::<PeerAddr>(),
        )
        .await
        .expect("TLS 服务器退出");
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("加入根证书");
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let mut last_error = None;
    let mut tls = None;
    for _ in 0..40 {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                match connector
                    .connect(ServerName::try_from("localhost").expect("服务器名"), stream)
                    .await
                {
                    Ok(stream) => {
                        tls = Some(stream);
                        break;
                    }
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut tls = tls.unwrap_or_else(|| panic!("TLS 连接失败: {last_error:?}"));

    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("写请求");
    let mut response = Vec::new();
    tls.read_to_end(&mut response).await.expect("读响应");
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200"), "响应: {text}");
    assert!(text.contains("\"ok\":true"), "响应: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}
