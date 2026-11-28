//! 服务器配置。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    #[serde(default)]
    pub allow_registration: bool,
    #[serde(default = "default_ttl_days")]
    pub token_ttl_days: i64,
    #[serde(default = "default_max_frame")]
    pub max_frame_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            db_path: default_db_path(),
            allow_registration: false,
            token_ttl_days: default_ttl_days(),
            max_frame_bytes: default_max_frame(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;
        if config.db_path.is_relative() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    config.db_path = parent.join(&config.db_path);
                }
            }
        }
        Ok(config)
    }
}

/// 可选的 TLS 配置，单独从同一份 TOML 读取。
///
/// 故意不放进 [`Config`]：`Config` 的字段已被其他 crate 的结构体字面量使用，
/// 新增字段会破坏它们的编译；TLS 只有服务器自己的 `run` 需要。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TlsConfig {
    /// PEM 证书链路径（可包含中间证书）。
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    /// PEM 私钥路径（PKCS#8 / PKCS#1 / SEC1 均可）。
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
}

impl TlsConfig {
    /// 读取 TLS 设置并解析相对路径。
    ///
    /// - 两个字段都没配置：返回 `None`，服务器保持明文 HTTP/WS（本机开发）。
    /// - 只配置了其中一个：直接报错，避免意外退化成明文。
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut tls: TlsConfig =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;

        match (&tls.tls_cert, &tls.tls_key) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => {
                let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
                if let Some(parent) = parent {
                    if let Some(cert) = tls.tls_cert.as_mut() {
                        if cert.is_relative() {
                            *cert = parent.join(&*cert);
                        }
                    }
                    if let Some(key) = tls.tls_key.as_mut() {
                        if key.is_relative() {
                            *key = parent.join(&*key);
                        }
                    }
                }
                Ok(Some(tls))
            }
            _ => bail!("tls_cert 与 tls_key 必须同时配置"),
        }
    }
}

/// 自托管更新配置，从同一份 TOML 的 `[update]` 段读取。
///
/// 与 [`TlsConfig`] 一样不放进 [`Config`]：`Config` 的字段已被测试里的结构体字面量使用。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateConfig {
    /// 安装包所在目录；相对路径按配置文件所在目录解析。
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// 对外发布的最新版本号，例如 `0.1.0`。
    #[serde(default)]
    pub version: Option<String>,
    /// 安装包文件名（必须位于 `dir` 下，下载时按此名精确匹配）。
    #[serde(default)]
    pub file: Option<String>,
    /// 更新说明，可含换行。
    #[serde(default)]
    pub notes: Option<String>,
}

/// 只读取 `[update]` 段，其余顶层字段忽略。
#[derive(Debug, Deserialize)]
struct UpdateDoc {
    #[serde(default)]
    update: Option<UpdateConfig>,
}

impl UpdateConfig {
    /// 读取 `[update]` 段；配置文件不存在或没有该段时返回空配置（更新未启用）。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let doc: UpdateDoc =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;
        let mut config = doc.update.unwrap_or_default();
        if let Some(dir) = config.dir.as_mut() {
            if dir.is_relative() {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    *dir = parent.join(&*dir);
                }
            }
        }
        Ok(config)
    }
}

/// 配额与限流配置，从同一份 TOML 的 `[limits]` 段读取。
///
/// 与 [`TlsConfig`] / [`UpdateConfig`] 一样不放进 [`Config`]：`Config` 的字段
/// 已被其他 crate 的结构体字面量使用，新增字段会破坏它们的编译。
///
/// 所有字段都有默认值，旧配置（没有 `[limits]` 段）按默认值启动。
/// 速率写 0 表示关闭该条限流（`max_devices_per_account = 0` 表示不限设备数）。
#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    /// 单账号最多可绑定的设备数；0 表示不限制。重复登录已有设备不计新增。
    #[serde(default = "default_max_devices")]
    pub max_devices_per_account: u32,
    /// `POST /api/v1/login` 每个客户端 IP 每分钟允许的请求数。
    #[serde(default = "default_login_per_minute")]
    pub login_per_minute: u32,
    /// 登录请求限流的突发容量（桶大小；默认等于每分钟速率）。
    #[serde(default = "default_login_burst")]
    pub login_burst: u32,
    /// `POST /api/v1/register` 每个客户端 IP 每小时允许的注册数。
    #[serde(default = "default_register_per_hour")]
    pub register_per_hour: u32,
    /// 注册限流的突发容量（桶大小；默认等于每小时速率）。
    #[serde(default = "default_register_burst")]
    pub register_burst: u32,
    /// 每个账号每分钟允许建立的 `/ws/view` 长连接数。
    #[serde(default = "default_view_connects_per_minute")]
    pub view_connects_per_minute: u32,
    /// `/ws/view` 建连限流的突发容量（桶大小；默认等于每分钟速率）。
    #[serde(default = "default_view_connect_burst")]
    pub view_connect_burst: u32,
    /// 单条主控连接每分钟允许发起的会话数（`t = open`）。
    #[serde(default = "default_session_opens_per_minute")]
    pub session_opens_per_minute: u32,
    /// 会话建立限流的突发容量（桶大小；默认等于每分钟速率）。
    #[serde(default = "default_session_open_burst")]
    pub session_open_burst: u32,
    /// `POST /api/v1/quick/connect` 每个客户端 IP 每分钟允许的尝试次数。
    #[serde(default = "default_quick_connects_per_minute")]
    pub quick_connects_per_minute: u32,
    /// 连接码尝试限流的突发容量（桶大小；默认等于每分钟速率）。
    #[serde(default = "default_quick_connect_burst")]
    pub quick_connect_burst: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_devices_per_account: default_max_devices(),
            login_per_minute: default_login_per_minute(),
            login_burst: default_login_burst(),
            register_per_hour: default_register_per_hour(),
            register_burst: default_register_burst(),
            view_connects_per_minute: default_view_connects_per_minute(),
            view_connect_burst: default_view_connect_burst(),
            session_opens_per_minute: default_session_opens_per_minute(),
            session_open_burst: default_session_open_burst(),
            quick_connects_per_minute: default_quick_connects_per_minute(),
            quick_connect_burst: default_quick_connect_burst(),
        }
    }
}

/// 只读取 `[limits]` 段，其余顶层字段忽略。
#[derive(Debug, Deserialize)]
struct LimitsDoc {
    #[serde(default)]
    limits: Option<LimitsConfig>,
}

impl LimitsConfig {
    /// 读取 `[limits]` 段；配置文件不存在或没有该段时使用默认值。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let doc: LimitsDoc =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;
        Ok(doc.limits.unwrap_or_default())
    }
}

/// 网页端 WebRTC P2P 的 `[webrtc]` 段；没有这一段时功能关闭，行为与旧版一致。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebrtcConfig {
    /// 打开后 `/api/v1/p2p/ice` 才可用，`webrtc` 信令才会转发。
    #[serde(default)]
    pub enabled: bool,
    /// STUN 服务器，例如 `stun:stun.example.com:3478`；可写多个。
    #[serde(default)]
    pub stun: Vec<String>,
    /// TURN 服务器地址，例如 `turn:turn.example.com:3478?transport=udp`。
    #[serde(default)]
    pub turn: Option<String>,
    /// coturn 的 `static-auth-secret`；只放服务器配置，绝不能下发给客户端。
    #[serde(default)]
    pub turn_secret: Option<String>,
    /// TURN 临时凭据有效期（秒）。
    #[serde(default = "default_turn_ttl")]
    pub turn_ttl_secs: u64,
}

impl WebrtcConfig {
    /// 读取 `[webrtc]` 段；配置文件或该段不存在时使用默认值（关闭）。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let doc: WebrtcDoc =
            toml::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))?;
        let mut config = doc.webrtc.unwrap_or_default();
        if config.turn.is_some() && config.turn_secret.as_deref().unwrap_or("").is_empty() {
            bail!("[webrtc] 配置了 turn 但没有 turn_secret，TURN 凭据无法签发");
        }
        if config.turn_ttl_secs == 0 {
            config.turn_ttl_secs = default_turn_ttl();
        }
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
struct WebrtcDoc {
    #[serde(default)]
    webrtc: Option<WebrtcConfig>,
}

fn default_turn_ttl() -> u64 {
    3600
}

fn default_bind() -> String {
    "127.0.0.1:8787".to_string()
}

fn default_db_path() -> PathBuf {
    PathBuf::from("screenwall.db")
}

fn default_ttl_days() -> i64 {
    30
}

fn default_max_frame() -> usize {
    1024 * 1024
}

fn default_max_devices() -> u32 {
    20
}

fn default_login_per_minute() -> u32 {
    30
}

fn default_login_burst() -> u32 {
    30
}

fn default_register_per_hour() -> u32 {
    10
}

fn default_register_burst() -> u32 {
    10
}

fn default_view_connects_per_minute() -> u32 {
    30
}

fn default_view_connect_burst() -> u32 {
    30
}

fn default_session_opens_per_minute() -> u32 {
    60
}

fn default_session_open_burst() -> u32 {
    60
}

fn default_quick_connects_per_minute() -> u32 {
    10
}

fn default_quick_connect_burst() -> u32 {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_default_when_section_missing() {
        let dir = std::env::temp_dir().join(format!("screenwall-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.toml");
        std::fs::write(
            &path,
            "bind = \"127.0.0.1:8787\"\nallow_registration = true\n",
        )
        .unwrap();

        let limits = LimitsConfig::load(&path).expect("旧配置应能加载");
        assert_eq!(limits.max_devices_per_account, 20);
        assert_eq!(limits.login_per_minute, 30);
        assert_eq!(limits.register_per_hour, 10);
        assert_eq!(limits.view_connects_per_minute, 30);
        assert_eq!(limits.session_opens_per_minute, 60);

        // 顶层配置照常解析。
        let config = Config::load(&path).expect("Config 应能加载");
        assert!(config.allow_registration);
        assert_eq!(config.bind, "127.0.0.1:8787");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits_section_overrides_defaults() {
        let dir =
            std::env::temp_dir().join(format!("screenwall-config-lim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.toml");
        std::fs::write(
            &path,
            "bind = \"127.0.0.1:8787\"\n\n[limits]\nmax_devices_per_account = 5\nlogin_per_minute = 7\n",
        )
        .unwrap();

        let limits = LimitsConfig::load(&path).expect("配置应能加载");
        assert_eq!(limits.max_devices_per_account, 5);
        assert_eq!(limits.login_per_minute, 7);
        // 未写的字段仍取默认值。
        assert_eq!(limits.register_per_hour, 10);
        assert_eq!(limits.session_open_burst, 60);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits_missing_file_uses_defaults() {
        let path = std::env::temp_dir().join("screenwall-config-does-not-exist.toml");
        let limits = LimitsConfig::load(&path).expect("缺失文件应回退默认值");
        assert_eq!(limits.max_devices_per_account, 20);
    }
}
