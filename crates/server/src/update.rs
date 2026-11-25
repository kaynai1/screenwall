//! 自托管自动更新：版本检查与安装包下载。
//!
//! 安装包只从 `[update].dir` 下按 `[update].file` 精确匹配下载，不接受任何
//! 其它路径；SHA256 在首次请求时计算一次并缓存。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;
use tokio_util::io::ReaderStream;

use crate::config::UpdateConfig;
use crate::state::AppState;

/// 一次成功计算的哈希与发布时间，命中后不再读文件。
#[derive(Debug, Clone)]
struct Cached {
    sha256: String,
    published_at: i64,
}

/// 已校验过的更新源。
#[derive(Debug, Clone)]
struct Source {
    dir: PathBuf,
    file: String,
    version: String,
    notes: String,
}

/// 服务器持有的更新状态；未配置时所有请求都返回 404。
#[derive(Debug, Default)]
pub struct UpdateState {
    source: Option<Source>,
    cached: OnceCell<Cached>,
}

impl UpdateState {
    /// 从 `[update]` 配置构建。缺字段、目录名非法或文件名带路径分隔符时视为未配置。
    pub fn from_config(config: &UpdateConfig) -> Self {
        let (Some(dir), Some(version), Some(file)) = (
            config.dir.clone(),
            config.version.clone(),
            config.file.clone(),
        ) else {
            return Self::default();
        };
        let file = file.trim().to_string();
        let version = version.trim().to_string();
        if file.is_empty() || version.is_empty() || !is_plain_file_name(&file) {
            return Self::default();
        }
        Self {
            source: Some(Source {
                dir,
                file,
                version,
                notes: config.notes.clone().unwrap_or_default(),
            }),
            cached: OnceCell::new(),
        }
    }

    /// 返回版本信息；文件不存在、读取失败或未配置时返回 `None`。
    ///
    /// SHA256 只成功计算一次；失败不缓存，文件稍后出现仍能被发现。
    pub async fn info(&self) -> Option<UpdateInfo> {
        let source = self.source.as_ref()?;
        let cached = self
            .cached
            .get_or_try_init(|| async {
                let path = source.dir.join(&source.file);
                let meta = tokio::fs::metadata(&path).await.map_err(|_| ())?;
                if !meta.is_file() {
                    return Err(());
                }
                let sha256 = hash_file(&path).await.map_err(|_| ())?;
                Ok(Cached {
                    sha256,
                    published_at: modified_ms(&meta),
                })
            })
            .await
            .ok()?;

        Some(UpdateInfo {
            version: source.version.clone(),
            url: format!("/downloads/{}", encode_segment(&source.file)),
            sha256: cached.sha256.clone(),
            notes: source.notes.clone(),
            published_at: cached.published_at,
        })
    }

    /// 精确匹配白名单文件，返回磁盘路径；任何其它名字（含 `..`）都返回 `None`。
    pub fn download_path(&self, requested: &str) -> Option<PathBuf> {
        let source = self.source.as_ref()?;
        if requested != source.file || !is_plain_file_name(requested) {
            return None;
        }
        Some(source.dir.join(&source.file))
    }
}

/// `GET /api/v1/version` 的响应体。
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub notes: String,
    pub published_at: i64,
}

fn not_configured() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "msg": "未配置更新" }))).into_response()
}

/// `GET /api/v1/version`：不需要登录，登录前也能检查更新。
pub async fn version(State(state): State<Arc<AppState>>) -> Response {
    let Some(info) = state.update.info().await else {
        return not_configured();
    };
    Json(json!({
        "version": info.version,
        "url": info.url,
        "sha256": info.sha256,
        "notes": info.notes,
        "publishedAt": info.published_at,
    }))
    .into_response()
}

/// `GET /downloads/{file}`：仅允许白名单里的安装包，流式返回。
pub async fn download(
    State(state): State<Arc<AppState>>,
    AxumPath(file): AxumPath<String>,
) -> Response {
    let Some(path) = state.update.download_path(&file) else {
        return not_configured();
    };
    let handle = match tokio::fs::File::open(&path).await {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!("打开安装包 {} 失败: {error}", path.display());
            return not_configured();
        }
    };
    let len = match handle.metadata().await {
        Ok(meta) => meta.len(),
        Err(error) => {
            tracing::warn!("读取安装包信息 {} 失败: {error}", path.display());
            return not_configured();
        }
    };

    let ascii_name: String = file
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+' | '(' | ')') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let disposition = format!(
        "attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{}",
        encode_segment(&file)
    );

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len);
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        builder = builder.header(header::CONTENT_DISPOSITION, value);
    }

    let body = Body::from_stream(ReaderStream::new(handle));
    match builder.body(body) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!("构造下载响应失败: {error}");
            not_configured()
        }
    }
}

/// 文件名必须只有一个普通路径组件，避免 `..`、子目录或盘符。
fn is_plain_file_name(name: &str) -> bool {
    let path = Path::new(name);
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn modified_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

async fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 对 URL 路径段做最小百分号编码，保留未保留字符。
fn encode_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_names_only() {
        assert!(is_plain_file_name("ScreenWall_0.1.0_x64-setup.exe"));
        assert!(!is_plain_file_name("../secret.txt"));
        assert!(!is_plain_file_name("sub/secret.txt"));
        assert!(!is_plain_file_name(""));
        assert!(!is_plain_file_name(".."));
    }

    #[test]
    fn segment_encoding_keeps_safe_chars() {
        assert_eq!(
            encode_segment("ScreenWall_0.1.0_x64-setup.exe"),
            "ScreenWall_0.1.0_x64-setup.exe"
        );
        assert_eq!(encode_segment("a b.exe"), "a%20b.exe");
    }
}
