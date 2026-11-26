//! 自托管自动更新：检查版本、下载安装包、启动安装程序。
//!
//! 这里不依赖 `crate::Shared`，只接收 `reqwest::Client`、服务器地址与令牌，
//! 进度通过回调上报，便于在 `main.rs` 里用 `#[tauri::command]` 包装并 emit 事件。
//!
//! - 检查更新优先走 `GET /api/v1/version`（服务器不要求登录）。
//! - 没有自建服务器时（局域网模式）回退到 GitHub Releases 的 latest 接口，
//!   这样「不用服务器」的用户也能收到新版本。
//! - 下载到 `%TEMP%\ScreenWall\updates\<文件名>`，边下边算 SHA256，校验失败即删除。
//! - 安装只负责启动安装包；是否退出主程序由调用方决定。

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// 当前编译版本，与 `Cargo.toml` 保持一致。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 开源仓库的 Releases 接口；局域网模式（没有自建服务器）用它检查更新。
const GITHUB_RELEASES: &str = "https://api.github.com/repos/kaynai1/screenwall/releases/latest";

/// 发现的新版本。字段用 camelCase 直接返回给前端。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 当前运行的版本。
    pub current: String,
    /// 服务器发布的最新版本。
    pub version: String,
    /// 更新说明（可含换行）。
    pub notes: String,
    /// 安装包的绝对下载地址。
    pub url: String,
    /// 安装包 SHA256（64 位小写十六进制）。
    pub sha256: String,
    /// 发布时间（Unix 毫秒）。
    pub published_at: i64,
}

/// 比较两个版本号：`a > b` 时返回 `true`。
///
/// 忽略前导 `v`，按 `.` 分段比较前导数字（`1.2.10 > 1.2.9`），
/// 缺省段视为 0（`2.0 == 2.0.0`），预发布后缀（`-beta`）不参与比较。
pub fn version_gt(a: &str, b: &str) -> bool {
    let left = parse_version(a);
    let right = parse_version(b);
    let len = left.len().max(right.len());
    for index in 0..len {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        if l != r {
            return l > r;
        }
    }
    false
}

fn parse_version(raw: &str) -> Vec<u64> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    // 只取主版本段，丢掉 `-beta.1` / `+build` 之类后缀。
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    core.split('.')
        .map(|segment| {
            let digits: String = segment
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

/// 检查服务器是否发布了新版本；`Ok(None)` 表示已是最新。
///
/// **不带账号令牌**：`/api/v1/version` 本来就不需要登录，带上一个过期的令牌
/// 只会让「检查更新」莫名报「登录已失效」（v0.2.9 之前就是这样，用户看到
/// 红字却和更新毫无关系），也可能让口径更严的反向代理直接拒绝请求。
/// 服务器还没发布过更新时返回 404，这里按「这一源没有更新」处理，
/// 与 GitHub 仓库没有 Release 的语义一致。
pub async fn check_update(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<Option<UpdateInfo>, String> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/api/v1/version");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("检查更新失败: {error}"))?;
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
    ) {
        // 这一源回答不了版本：404 是服务器没配置 `[update]`，401/403 往往是
        // 服务器前面挂了反向代理 / 鉴权（版本接口本身免登录）。都不算错误，
        // 交给 GitHub 那一路下结论——否则「检查更新」会冒出一条
        // 「登录已失效」，看着像账号问题，其实跟更新无关。
        return Ok(None);
    }
    let body: Value = response
        .json()
        .await
        .unwrap_or_else(|_| Value::Object(Default::default()));
    if !status.is_success() {
        let message = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("服务器未提供更新");
        return Err(format!("{message}（HTTP {status}）"));
    }

    let version = body
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "服务器返回的版本号无效".to_string())?
        .to_string();
    let raw_url = body
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "服务器返回的下载地址无效".to_string())?;
    let sha256 = body
        .get("sha256")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or_else(|| "服务器返回的校验值无效".to_string())?;
    let notes = body
        .get("notes")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let published_at = body.get("publishedAt").and_then(Value::as_i64).unwrap_or(0);

    if !version_gt(&version, CURRENT_VERSION) {
        return Ok(None);
    }

    Ok(Some(UpdateInfo {
        current: CURRENT_VERSION.to_string(),
        version,
        notes,
        url: absolute_url(base, raw_url),
        sha256,
        published_at,
    }))
}

/// 从 GitHub Releases 检查新版本；`Ok(None)` 表示已是最新。
///
/// 局域网模式没有自建服务器，但用户同样需要更新。GitHub 的 latest 接口
/// 返回 tag_name、正文和附件列表；我们取第一个 `*-setup.exe` 作为安装包，
/// 校验值用附件自带的 `digest`（GitHub 现在会返回 `sha256:<hex>`）。
///
/// 网络不可达时返回 Err，由界面显示原因 —— 不静默当成「已是最新」。
pub async fn check_update_github(client: &reqwest::Client) -> Result<Option<UpdateInfo>, String> {
    // 全局 HTTP 超时只有 10s，国内访问 api.github.com 常常不够；这里单独
    // 给 30s，而且把失败原因说成「网络问题」而不是让上层静默当成「已是最新」。
    let response = client
        .get(GITHUB_RELEASES)
        .timeout(std::time::Duration::from_secs(30))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "检查更新超时：无法访问 GitHub（国内网络可能需要代理），请稍后重试或手动下载"
                    .to_string()
            } else {
                format!("检查更新失败：无法访问 GitHub（国内网络可能需要代理）: {error}")
            }
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // 仓库还没有发布任何 Release：按「已是最新」处理，不当作错误。
        return Ok(None);
    }
    let body: Value = response
        .json()
        .await
        .unwrap_or_else(|_| Value::Object(Default::default()));
    if !status.is_success() {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("GitHub 未返回可用的更新信息");
        return Err(format!("{message}（HTTP {status}）"));
    }

    let version = body
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "GitHub 返回的版本号无效".to_string())?
        .to_string();
    if !version_gt(&version, CURRENT_VERSION) {
        return Ok(None);
    }

    let assets = body
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let installer = assets
        .iter()
        .find(|asset| {
            asset
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.to_ascii_lowercase().ends_with("-setup.exe"))
        })
        .ok_or_else(|| format!("{version} 的 Release 里没有 setup.exe 安装包"))?;

    let url = installer
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "安装包下载地址无效".to_string())?
        .to_string();
    // digest 形如 `sha256:abcd…`；没有就拒绝，宁可不更新也不能不校验。
    let sha256 = installer
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("sha256:"))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or_else(|| "Release 附件缺少 SHA256 校验值，已取消更新".to_string())?;

    let notes = body
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let published_at = body
        .get("published_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .unwrap_or(0);

    Ok(Some(UpdateInfo {
        current: CURRENT_VERSION.to_string(),
        version,
        notes,
        url,
        sha256,
        published_at,
    }))
}

/// `2026-11-25T07:58:38Z` → Unix 毫秒。
///
/// 只为「发布于 …」这一行显示服务，手写解析比多引一个日期库划算；
/// 解析不了就返回 None，界面按「发布时间未知」处理。
fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    let date = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    Some(date.timestamp_millis())
}

/// 相对下载地址拼成绝对地址；已是 `http(s)://` 的原样返回。
fn absolute_url(base: &str, raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return raw.to_string();
    }
    if raw.starts_with('/') {
        format!("{base}{raw}")
    } else {
        format!("{base}/{raw}")
    }
}

/// 更新包存放目录：`%TEMP%\ScreenWall\updates`。
pub fn update_dir() -> PathBuf {
    std::env::temp_dir().join("ScreenWall").join("updates")
}

/// 下载安装包到本地并校验 SHA256，返回本地绝对路径。
///
/// `on_progress(received, total)` 会在下载过程中被反复调用，`total` 未知时为 0。
/// 校验失败会删除已下载文件并返回错误。
pub async fn download_update<F>(
    client: &reqwest::Client,
    url: &str,
    sha256: &str,
    mut on_progress: F,
) -> Result<String, String>
where
    F: FnMut(u64, u64),
{
    let expected = sha256.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("校验值格式无效".into());
    }

    let dir = update_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| format!("创建更新目录失败: {error}"))?;
    let name = file_name_from_url(url);
    let dest = dir.join(&name);
    let part = dir.join(format!("{name}.part"));

    // 覆盖客户端默认的短超时：安装包较大，整段下载给足时间。
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(30 * 60))
        .send()
        .await
        .map_err(|error| format!("下载安装包失败: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载安装包失败（HTTP {status}）"));
    }
    let total = response.content_length().unwrap_or(0);

    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|error| format!("创建下载文件失败: {error}"))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut last_emit: u64 = 0;
    let mut response = response;
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&part).await;
                return Err(format!("下载中断: {error}"));
            }
        };
        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("写入安装包失败: {error}"));
        }
        hasher.update(&chunk);
        received += chunk.len() as u64;
        // 每 256 KiB 或下载结束时回报一次，避免事件刷屏。
        if received - last_emit >= 256 * 1024 || total > 0 && received >= total {
            last_emit = received;
            on_progress(received, total);
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("写入安装包失败: {error}"))?;
    drop(file);
    on_progress(received, total);

    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!(
            "安装包校验失败（期望 {expected}，实际 {actual}），已删除下载文件"
        ));
    }

    if tokio::fs::rename(&part, &dest).await.is_err() {
        // 目标已存在时 Windows 上 rename 会失败，删掉旧文件再试一次。
        let _ = tokio::fs::remove_file(&dest).await;
        tokio::fs::rename(&part, &dest)
            .await
            .map_err(|error| format!("保存安装包失败: {error}"))?;
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// 启动安装包（NSIS 静默安装 `/S`），成功启动即返回。
///
/// perMachine 安装包的清单要求管理员权限：`Command::spawn` 在非提权进程里
/// 会直接报 ERROR_ELEVATION_REQUIRED（os error 740），所以这里必须走
/// `ShellExecuteW(runas)`，由用户确认一次 UAC。
///
/// 不在这里退出进程：主程序可以在返回 `Ok` 后自行 `exit(0)`。
pub fn install_update(path: &str) -> Result<(), String> {
    let file = Path::new(path);
    if !file.is_file() {
        return Err(format!("安装包不存在: {path}"));
    }
    crate::firewall::shell_execute_runas(file, "/S", false)
        .map_err(|error| format!("启动安装程序失败: {error}"))?;
    Ok(())
}

/// 从下载地址推导本地文件名：去掉查询/锚点，取最后一段并做 Windows 安全清洗。
pub fn file_name_from_url(url: &str) -> String {
    let no_fragment = url.split('#').next().unwrap_or(url);
    let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);
    let last = no_query.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(last);

    let mut cleaned = String::with_capacity(decoded.len());
    for ch in decoded.chars() {
        let safe =
            !matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !ch.is_control();
        cleaned.push(if safe { ch } else { '_' });
    }
    let cleaned = cleaned
        .trim_end_matches(['.', ' '])
        .trim_start_matches(' ')
        .to_string();
    let cleaned = if cleaned.len() > 180 {
        cleaned[..180].to_string()
    } else {
        cleaned
    };
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "ScreenWall-update.exe".to_string();
    }
    cleaned
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(!version_gt("1.2.9", "1.2.9"));
        assert!(!version_gt("1.2.10", "1.2.10"));
        assert!(version_gt("1.2.10", "1.2.9"));
        assert!(!version_gt("1.2.9", "1.2.10"));
        assert!(version_gt("2.0.0", "1.99.99"));
        assert!(version_gt("1.10.0", "1.9.0"));
        assert!(!version_gt("2.0", "2.0.0"));
        assert!(!version_gt("2.0.0", "2.0"));
        assert!(version_gt("v1.0.0", "0.9.9"));
        assert!(version_gt("V1.0.1", "v1.0.0"));
        assert!(!version_gt("1.2.3-beta", "1.2.3"));
        assert!(version_gt("1.2.4-beta", "1.2.3"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(version_gt("0.1.1", "0.1.0"));
    }

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(
            file_name_from_url("http://host/downloads/ScreenWall_0.1.0_x64-setup.exe"),
            "ScreenWall_0.1.0_x64-setup.exe"
        );
        assert_eq!(
            file_name_from_url("https://host/downloads/a%20b.exe?token=1#frag"),
            "a b.exe"
        );
        assert_eq!(
            file_name_from_url("http://host/downloads/a:b*c.exe"),
            "a_b_c.exe"
        );
        // `?` 之后是查询串，不进入文件名。
        assert_eq!(
            file_name_from_url("http://host/downloads/setup.exe?x=1"),
            "setup.exe"
        );
        assert_eq!(file_name_from_url("http://host/"), "ScreenWall-update.exe");
        assert_eq!(file_name_from_url(""), "ScreenWall-update.exe");
        assert_eq!(
            file_name_from_url("http://host/x/.."),
            "ScreenWall-update.exe"
        );
        assert_eq!(file_name_from_url("http://host/x/a.exe."), "a.exe");
        // 路径分隔符会被替换，不会逃出更新目录。
        assert!(!file_name_from_url("http://host/..%2F..%2Fevil.exe").contains('/'));
        assert!(!file_name_from_url("http://host/..%5Cevil.exe").contains('\\'));
    }

    #[test]
    fn absolute_url_joining() {
        assert_eq!(
            absolute_url("http://127.0.0.1:8787", "/downloads/a.exe"),
            "http://127.0.0.1:8787/downloads/a.exe"
        );
        assert_eq!(
            absolute_url("http://127.0.0.1:8787", "downloads/a.exe"),
            "http://127.0.0.1:8787/downloads/a.exe"
        );
        assert_eq!(
            absolute_url("http://127.0.0.1:8787", "https://cdn/a.exe"),
            "https://cdn/a.exe"
        );
    }
}
