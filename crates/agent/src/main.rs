use std::sync::Arc;

use anyhow::{Context, Result};
use screenwall_agent::config;
use screenwall_agent::display;
use screenwall_agent::server::{run_server, AgentState};
use screenwall_agent::tls;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let dir = config::config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
    let config = config::load_or_create(&dir)?;

    let material = tls::ensure_cert(&dir)?;
    let acceptor = TlsAcceptor::from(tls::server_config(&material)?);

    let displays = display::list_displays();
    let state = Arc::new(AgentState {
        config: config.clone(),
        displays,
        approval: None,
    });

    let listener = TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("无法监听端口 {}", config.port))?;

    println!("ScreenWall Agent {}", env!("CARGO_PKG_VERSION"));
    println!("  设备名 : {}", config.name);
    println!("  设备 ID: {}", config.device_id);
    println!("  监听   : 0.0.0.0:{}", config.port);
    println!("  Token  : {}", config.token);
    println!("  指纹   : {}", material.fingerprint);
    println!(
        "  显示器 : {}",
        state
            .displays
            .iter()
            .map(|display| format!("{}x{}", display.w, display.h))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let cloud_options = screenwall_agent::cloud::CloudOptions::from_state(&state);
    if let Some(options) = cloud_options {
        println!("  服务器 : {}", options.server_url);
        println!("  Ctrl+C 退出");
        let cloud_state = state.clone();
        tokio::select! {
            result = run_server(listener, state, acceptor) => result,
            result = screenwall_agent::cloud::run(cloud_state, options) => result,
            _ = tokio::signal::ctrl_c() => {
                println!("\n已退出");
                Ok(())
            }
        }
    } else {
        println!("  服务器 : 未配置（只接受局域网直连）");
        println!("  Ctrl+C 退出");
        tokio::select! {
            result = run_server(listener, state, acceptor) => result,
            _ = tokio::signal::ctrl_c() => {
                println!("\n已退出");
                Ok(())
            }
        }
    }
}
