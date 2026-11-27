//! Agent 配置与 token。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 设备名，默认取 COMPUTERNAME。
    pub name: String,
    /// 监听端口。
    pub port: u16,
    /// 预共享 token（base64），用于局域网直连。
    pub token: String,
    /// 是否允许被控。
    pub allow_control: bool,
    /// 远程会话中同步文本剪贴板。
    #[serde(default = "default_true")]
    pub clipboard_sync: bool,
    /// 账号服务器地址，例如 `https://screenwall.example.com`；为空则只跑局域网直连。
    #[serde(default)]
    pub server_url: String,
    /// 账号登录令牌；由客户端登录后写入。
    #[serde(default)]
    pub account_token: String,
    /// 稳定设备 ID；首次启动生成。
    #[serde(default)]
    pub device_id: String,
    /// 本机识别码（9 位数字）：随设备上线报给服务器，别人登录后也能凭它找人。
    #[serde(default)]
    pub local_id: String,
    /// 自定义 CA 根证书路径（文件或目录）；为空表示用系统默认信任库。
    #[serde(default)]
    pub ca_path: String,
    /// 无头机器上是否允许临时接入虚拟显示器（需要管理员安装第三方 IDD 驱动）。
    #[serde(default)]
    pub virtual_display: bool,
}

impl AgentConfig {
    pub fn with_new_token() -> Self {
        Self {
            name: hostname(),
            port: 47900,
            token: generate_token(),
            allow_control: true,
            clipboard_sync: true,
            server_url: String::new(),
            account_token: String::new(),
            device_id: generate_device_id(),
            local_id: String::new(),
            ca_path: String::new(),
            virtual_display: false,
        }
    }
}

/// `%APPDATA%\ScreenWall`
pub fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ScreenWall")
}

pub fn load_or_create(dir: &Path) -> Result<AgentConfig> {
    fs::create_dir_all(dir)?;
    let path = dir.join("agent.toml");
    if path.exists() {
        let text =
            fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut config: AgentConfig =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;
        if config.token.trim().is_empty() {
            config.token = generate_token();
            save(dir, &config)?;
            tracing::warn!("配置里的 token 为空，已重新生成");
        }
        if config.device_id.trim().is_empty() {
            config.device_id = generate_device_id();
            save(dir, &config)?;
            tracing::info!("已生成设备 ID {}", config.device_id);
        }
        Ok(config)
    } else {
        let config = AgentConfig::with_new_token();
        save(dir, &config)?;
        tracing::info!("已创建配置 {}", path.display());
        Ok(config)
    }
}

pub fn save(dir: &Path, config: &AgentConfig) -> Result<()> {
    fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(config)?;
    fs::write(dir.join("agent.toml"), text)?;
    Ok(())
}

fn default_true() -> bool {
    true
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "screenwall-agent".to_string())
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("系统随机数不可用");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn generate_device_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("系统随机数不可用");
    hex::encode(bytes)
}
