//! 客户端日志：滚动文件 + 崩溃记录 + 一键导出诊断包。
//!
//! `tracing` 默认只写 stderr；安装成桌面程序后没有控制台，出问题什么都看不到。
//! 这里加一个按大小滚动的文件写入器（`%APPDATA%\ScreenWall\logs`），
//! 并接管 panic hook 把崩溃原因也写进去。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// 单个日志文件上限 2 MiB，超过就滚成 `.1`、`.2`，最多保留 3 个历史文件。
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 3;

pub fn log_dir(base: &Path) -> PathBuf {
    base.join("logs")
}

struct State {
    file: Option<File>,
    written: u64,
}

/// `MakeWriter` 实现：每次写入都锁一下，日志量很小，够用。
#[derive(Clone)]
pub struct RollingWriter {
    dir: PathBuf,
    state: Arc<Mutex<State>>,
}

impl RollingWriter {
    pub fn new(dir: PathBuf) -> Self {
        if let Err(error) = fs::create_dir_all(&dir) {
            eprintln!("创建日志目录失败: {error}");
        }
        let mut writer = Self {
            dir,
            state: Arc::new(Mutex::new(State {
                file: None,
                written: 0,
            })),
        };
        writer.open();
        writer
    }

    fn active_path(&self) -> PathBuf {
        self.dir.join("screenwall.log")
    }

    fn open(&mut self) {
        let path = self.active_path();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                let mut state = self.state.lock().unwrap();
                state.file = Some(file);
                state.written = written;
            }
            Err(error) => eprintln!("打开日志文件失败: {error}"),
        }
    }

    /// 把当前文件滚成 `.1`，旧的依次后移，超出 `MAX_FILES` 的删掉。
    fn rotate(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.file = None;
        state.written = 0;
        drop(state);

        let _ = fs::remove_file(self.dir.join(format!("screenwall.log.{MAX_FILES}")));
        for index in (1..MAX_FILES).rev() {
            let from = self.dir.join(format!("screenwall.log.{index}"));
            let to = self.dir.join(format!("screenwall.log.{}", index + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let _ = fs::rename(self.active_path(), self.dir.join("screenwall.log.1"));
        self.open();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RollingWriter {
    type Writer = RollingHandle;

    fn make_writer(&'a self) -> Self::Writer {
        RollingHandle {
            writer: self.clone(),
        }
    }
}

pub struct RollingHandle {
    writer: RollingWriter,
}

impl Write for RollingHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let overflow = {
            let state = self.writer.state.lock().unwrap();
            state.written + buf.len() as u64 > MAX_BYTES
        };
        if overflow {
            self.writer.rotate();
        }
        let mut state = self.writer.state.lock().unwrap();
        if let Some(file) = state.file.as_mut() {
            file.write_all(buf)?;
            file.flush()?;
            state.written += buf.len() as u64;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.writer.state.lock().unwrap();
        if let Some(file) = state.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

/// 初始化日志：文件 + stderr 双写，并接管 panic。
pub fn init(base: &Path) -> RollingWriter {
    let writer = RollingWriter::new(log_dir(base));
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer.clone())
        .with_ansi(false)
        .try_init();

    let panic_writer = writer.clone();
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_else(|| "未知位置".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知错误".into());
        let line = format!("PANIC {location}: {payload}\n");
        {
            use tracing_subscriber::fmt::MakeWriter as _;
            let mut handle = panic_writer.make_writer();
            let _ = handle.write_all(line.as_bytes());
        }
        eprintln!("{line}");
        default_hook(info);
    }));

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        dir = %log_dir(base).display(),
        "客户端日志已启动"
    );
    writer
}

/// 日志文件列表（新的在前），供设置页展示。
pub fn log_files(base: &Path) -> Vec<PathBuf> {
    let dir = log_dir(base);
    let mut files: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("screenwall.log"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // 按「当前文件 → .1 → .2 …」排序；直接按字符串排会把 `.1` 排到最前面。
    files.sort_by_key(|path| {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        name.rsplit_once('.')
            .and_then(|(_, suffix)| suffix.parse::<u32>().ok())
            .unwrap_or(0)
    });
    files
}

/// 导出诊断包：日志 + 脱敏后的配置，返回目录路径。
///
/// 配置里的 `token` 是账号令牌，导出前必须清空；`device_id`、`server_url`
/// 对排查有必要，保留。直连设备列表里的 `known_peers[].token` 是长期直连
/// 令牌，同样能直接连上别人的机器，必须一起脱敏。
pub fn export(base: &Path) -> Result<PathBuf, String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let target = base.join(format!("diagnostics-{stamp}"));
    fs::create_dir_all(&target).map_err(|error| format!("创建导出目录失败: {error}"))?;

    for file in log_files(base) {
        if let Some(name) = file.file_name() {
            fs::copy(&file, target.join(name)).map_err(|error| format!("复制日志失败: {error}"))?;
        }
    }

    let config_path = base.join("client.json");
    let mut config: Value = fs::read_to_string(&config_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(object) = config.as_object_mut() {
        if object.contains_key("token") {
            object.insert("token".into(), json!("<已脱敏>"));
        }
    }
    scrub_peer_tokens(&mut config);
    let sanitized = serde_json::to_string_pretty(&config).unwrap_or_else(|_| "{}".into());
    fs::write(target.join("client.json"), sanitized)
        .map_err(|error| format!("写入配置副本失败: {error}"))?;

    let summary = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "exportedAt": chrono::Local::now().to_rfc3339(),
    });
    fs::write(
        target.join("system.json"),
        serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|error| format!("写入系统信息失败: {error}"))?;

    Ok(target)
}

/// 把 `knownPeers[].token` 与 `directTokens[]` 换成占位符。
///
/// 两者都是能直接连上某台机器的凭据，不能进诊断包；地址与设备名保留，
/// 否则排查时不知道是哪台机器。
fn scrub_peer_tokens(config: &mut Value) {
    if let Some(peers) = config.get_mut("knownPeers").and_then(Value::as_array_mut) {
        for peer in peers {
            if let Some(object) = peer.as_object_mut() {
                if object.contains_key("token") {
                    object.insert("token".into(), json!("<已脱敏>"));
                }
            }
        }
    }
    if let Some(tokens) = config.get_mut("directTokens").and_then(Value::as_array_mut) {
        // 条数对排查有意义（「我授权过几台」），内容没有。
        let count = tokens.len();
        *tokens = (0..count).map(|_| json!("<已脱敏>")).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_scrubs_token() {
        let dir = std::env::temp_dir().join(format!("sw-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("client.json"),
            r#"{"serverUrl":"http://127.0.0.1:8787","token":"secret-token","username":"kay","deviceId":"studio-main"}"#,
        )
        .unwrap();
        fs::create_dir_all(log_dir(&dir)).unwrap();
        fs::write(log_dir(&dir).join("screenwall.log"), "hello\n").unwrap();

        let target = export(&dir).unwrap();
        let config = fs::read_to_string(target.join("client.json")).unwrap();
        assert!(!config.contains("secret-token"), "令牌必须脱敏: {config}");
        assert!(config.contains("studio-main"));
        assert!(target.join("system.json").exists());
        assert!(target.join("screenwall.log").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_scrubs_peer_tokens() {
        let dir = std::env::temp_dir().join(format!("sw-log-peer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("client.json"),
            r#"{"token":"acct","knownPeers":[{"address":"192.168.1.20:47900","fingerprint":"AB:CD","token":"peer-secret-token","name":"书房"}],"directTokens":["inbound-secret-a","inbound-secret-b"]}"#,
        )
        .unwrap();
        fs::create_dir_all(log_dir(&dir)).unwrap();

        let target = export(&dir).unwrap();
        let config = fs::read_to_string(target.join("client.json")).unwrap();
        assert!(
            !config.contains("peer-secret-token"),
            "直连长期令牌必须脱敏: {config}"
        );
        assert!(
            !config.contains("inbound-secret"),
            "允许连进来的长期令牌也必须脱敏: {config}"
        );
        // 地址与设备名要留着，否则排查时不知道是哪台机器。
        assert!(config.contains("192.168.1.20:47900"));
        assert!(config.contains("书房"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_files_lists_newest_first() {
        let dir = std::env::temp_dir().join(format!("sw-log-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(log_dir(&dir)).unwrap();
        fs::write(log_dir(&dir).join("screenwall.log"), "a").unwrap();
        fs::write(log_dir(&dir).join("screenwall.log.1"), "b").unwrap();
        fs::write(log_dir(&dir).join("other.txt"), "c").unwrap();

        let files = log_files(&dir);
        assert_eq!(files.len(), 2);
        assert!(files[0].to_string_lossy().ends_with("screenwall.log"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_rotates_after_limit() {
        use tracing_subscriber::fmt::MakeWriter as _;

        let dir = std::env::temp_dir().join(format!("sw-log-rot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let writer = RollingWriter::new(log_dir(&dir));
        // 先塞满超过上限的内容，触发滚动。
        {
            let mut handle = writer.make_writer();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..3 {
                handle.write_all(&chunk).unwrap();
            }
        }
        assert!(log_dir(&dir).join("screenwall.log.1").exists());
        assert!(log_dir(&dir).join("screenwall.log").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
