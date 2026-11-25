//! 主控端文件传输：上传到被控端、从被控端下载、浏览被控端目录。
//!
//! 上传的数据块由前端 `FileReader` 切好、base64 后调用 `upload_chunk`，
//! 这里只负责流控和转发；下载由这里直接落盘到 `Downloads\ScreenWall`。

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use screenwall_proto::{
    FileAccept, FileCancel, FileChunk, FileComplete, FileDone, FileEntry, FileGet, FileItem,
    FileListOk, FileListReq, FileOffer, FileProgress, FileResume, Message, FILE_PROGRESS_STEP,
    FILE_WINDOW_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::oneshot;

use crate::{send_tunnel_json, Shared};

/// 同时进行的下载任务上限；超出的任务排队，等前面的任务结束后自动开始。
pub const MAX_CONCURRENT_DOWNLOADS: usize = 3;
/// 单个目录递归展开的层数上限，防止异常目录结构把栈和会话拖垮。
pub const MAX_DIRECTORY_DEPTH: usize = 64;
/// 单次整目录下载最多展开的文件数，防止误选盘符根目录时把队列撑爆。
pub const MAX_DIRECTORY_FILES: usize = 20_000;

/// 一次上传/下载的客户端状态。
#[derive(Default)]
pub struct Hub {
    /// 等被控端回包：上传等 `file.accept`，下载等 `file.offer`。
    pending: HashMap<u16, PendingReply>,
    sends: HashMap<u16, SendState>,
    /// 进行中的下载，按 tid 索引；最多 `MAX_CONCURRENT_DOWNLOADS` 个。
    recv: HashMap<u16, RecvState>,
    /// 排队中的下载，按先进先出开始。
    queue: VecDeque<QueuedDownload>,
    /// 已占位、正在等 `file.offer` 的下载 tid；与 `recv` 一起算并发。
    starting: std::collections::HashSet<u16>,
    /// 开始阶段就被取消的 tid，用来抑制随后到达的错误事件。
    cancelled: std::collections::HashSet<u16>,
    lists: HashMap<u16, oneshot::Sender<FileListOk>>,
    tid_seq: u16,
    group_seq: u64,
}

/// 一个已排队的下载请求，等空位时再由客户端发起 `file.get`。
struct QueuedDownload {
    tid: u16,
    path: String,
    /// 相对下载根目录的目标路径（目录下载会保留层级）；`None` 表示用文件名。
    target: Option<String>,
    /// 目录下载的分组号；单独下载为 0。
    group: u64,
    /// 目录展开时已知的文件大小，用于排队时展示。
    size: u64,
}

enum PendingReply {
    Accept(oneshot::Sender<FileAccept>),
    Offer(oneshot::Sender<FileOffer>),
}

struct SendState {
    confirmed: std::sync::Arc<AtomicU64>,
}

struct RecvState {
    tid: u16,
    /// 展示用文件名（目录下载时是相对路径）。
    name: String,
    dest: PathBuf,
    /// 分片文件：`Downloads\ScreenWall\.screenwall-downloads\<key>.part`。
    part: PathBuf,
    file: std::fs::File,
    size: u64,
    written: u64,
    last_progress: u64,
    hasher: Sha256,
    /// 本次请求的续传起点；被控端忽略该字段时会在收到 0 偏移块后清零。
    resume: u64,
    /// 被控端在 `file.done` 里给出的整文件 SHA256。
    expected_sha: Option<String>,
    /// 目录下载的分组号；单独下载为 0。
    group: u64,
}

impl Hub {
    fn alloc(&mut self) -> u16 {
        loop {
            self.tid_seq = self.tid_seq.wrapping_add(1).max(1);
            let tid = self.tid_seq;
            if !self.pending.contains_key(&tid)
                && !self.sends.contains_key(&tid)
                && !self.lists.contains_key(&tid)
                && !self.recv.contains_key(&tid)
                && !self.cancelled.contains(&tid)
                && !self.queue.iter().any(|queued| queued.tid == tid)
            {
                return tid;
            }
        }
    }

    /// 会话结束或退出登录时清空所有状态。没下完的分片保留，下次下载续传。
    pub fn reset(&mut self) {
        self.pending.clear();
        self.sends.clear();
        self.lists.clear();
        self.queue.clear();
        self.cancelled.clear();
        self.starting.clear();
        for (_, recv) in self.recv.drain() {
            let _ = recv.file.sync_all();
            drop(recv.file);
        }
    }

    /// 取消下载：保留分片供下次续传，只释放文件句柄。
    fn abort_download(&mut self, tid: u16) -> Option<String> {
        let recv = self.recv.remove(&tid)?;
        let _ = recv.file.sync_all();
        drop(recv.file);
        Some(recv.name)
    }

    /// 开始阶段的占位结束（转入接收或失败回收）。
    fn end_starting(&mut self, tid: u16) {
        self.starting.remove(&tid);
    }

    /// 并发占用：正在接收的任务 + 已占位等待 offer 的任务。
    fn slots_used(&self) -> usize {
        self.recv.len() + self.starting.len()
    }

    fn has_capacity(&self) -> bool {
        self.slots_used() < MAX_CONCURRENT_DOWNLOADS
    }

    /// 还有空位时取出下一个排队任务并占位。
    fn dequeue(&mut self) -> Option<QueuedDownload> {
        if !self.has_capacity() {
            return None;
        }
        let queued = self.queue.pop_front()?;
        self.starting.insert(queued.tid);
        Some(queued)
    }
}

/// 下载分片目录：`Downloads\ScreenWall\.screenwall-downloads`。
fn parts_dir() -> PathBuf {
    download_root().join(".screenwall-downloads")
}

/// 分片键：同一设备 + 同一远程路径 + 同一大小才复用，文件被替换（大小变化）不会错续。
///
/// 用 SHA256 哈希做文件名，避免远程路径里的特殊字符或 `..` 注入。
fn part_key(device: &str, path: &str, size: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device.as_bytes());
    hasher.update([0u8]);
    // 路径分隔符统一，`C:\a\b` 与 `C:/a/b` 视为同一文件。
    let normalized = path.replace('\\', "/");
    hasher.update(normalized.as_bytes());
    hasher.update([0u8]);
    hasher.update(size.to_le_bytes());
    hex::encode(hasher.finalize())
}

/// 清掉超过 7 天没续传的分片，避免长期占盘。
fn cleanup_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|ext| ext != "part").unwrap_or(true) {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age > Duration::from_secs(7 * 24 * 3600))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 校验已有分片并确定续传起点；分片比文件还长（被替换/损坏）时删除重来。
fn resume_offset(part: &Path, size: u64) -> u64 {
    match std::fs::metadata(part) {
        Ok(meta) if meta.len() > 0 && meta.len() < size => meta.len(),
        Ok(meta) if meta.len() == size => size,
        Ok(_) => {
            let _ = std::fs::remove_file(part);
            0
        }
        Err(_) => 0,
    }
}

/// 从分片已有的前缀重算 SHA256，续传时最终校验才能覆盖整个文件。
fn hash_prefix(part: &Path, offset: u64) -> Result<Sha256, String> {
    let mut hasher = Sha256::new();
    if offset == 0 {
        return Ok(hasher);
    }
    let mut file = std::fs::File::open(part).map_err(|error| format!("打开分片失败: {error}"))?;
    let mut remaining = offset;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let read = file
            .read(&mut buffer[..want])
            .map_err(|error| format!("读取分片失败: {error}"))?;
        if read == 0 {
            return Err("分片长度不足".to_string());
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher)
}

fn emit(app: &AppHandle, value: Value) {
    let _ = app.emit("file-transfer", value);
}

async fn tunnel(shared: &Shared, message: &Message) -> Result<(), String> {
    let control = shared
        .control
        .read()
        .await
        .clone()
        .ok_or_else(|| "控制会话尚未建立".to_string())?;
    let out = crate::control_out(shared, control.session)
        .await
        .ok_or_else(|| "主控连接已断开".to_string())?;
    if send_tunnel_json(&out, control.session, message) {
        Ok(())
    } else {
        Err("发送失败，主控连接可能已断开".to_string())
    }
}

pub(crate) fn download_root() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads").join("ScreenWall")
}

fn safe_name(raw: &str) -> String {
    let name = Path::new(raw)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let cleaned: String = name
        .chars()
        .map(|ch| if "<>:\"/\\|?*".contains(ch) { '_' } else { ch })
        .collect();
    if cleaned.trim().is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

fn unique_download(name: &str) -> Result<PathBuf, String> {
    let root = download_root();
    std::fs::create_dir_all(&root).map_err(|error| format!("创建下载目录失败: {error}"))?;
    let path = root.join(name);
    if !path.exists() {
        return Ok(path);
    }
    unique_in(&root, &path)
}

/// 在 `root` 下给 `path` 找一个不冲突的名字：`a.txt` → `a (1).txt`。
fn unique_in(root: &Path, path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let ext = path
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..1000 {
        let candidate = path.with_file_name(format!("{stem} ({index}){ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    let _ = root;
    Err("同名文件过多".to_string())
}

/// 把相对路径拼到下载根目录下，拒绝绝对路径、`..` 和盘符逃逸。
///
/// 返回 `Downloads\ScreenWall\<root>\<rel_path>` 形式的绝对路径；
/// `rel_path` 用 `/` 或 `\` 分隔都可以，空路径表示 `root` 本身。
pub fn relative_target(root: &Path, rel_path: &str) -> Result<PathBuf, String> {
    let cleaned = rel_path.replace('\\', "/");
    let mut out = root.to_path_buf();
    if cleaned.trim().is_empty() {
        return Ok(out);
    }
    let path = Path::new(&cleaned);
    if path.is_absolute() || cleaned.starts_with('/') {
        return Err(format!("拒绝绝对路径: {rel_path}"));
    }
    // Windows 盘符或 UNC 前缀即使在 Path::is_absolute 判定之外也要拒绝。
    if cleaned.len() >= 2 && cleaned.as_bytes()[1] == b':' {
        return Err(format!("拒绝盘符路径: {rel_path}"));
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => {
                // 中间段里的 `:` 可能是盘符相对路径或 NTFS 数据流，不允许。
                if name.to_string_lossy().contains(':') {
                    return Err(format!("路径包含非法片段: {rel_path}"));
                }
                out.push(name);
            }
            std::path::Component::CurDir => {}
            _ => return Err(format!("路径包含非法片段: {rel_path}")),
        }
    }
    if out == root {
        return Err(format!("路径为空: {rel_path}"));
    }
    Ok(out)
}

/// 一次目录展开得到的待下载文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// 被控端上的绝对路径，下载时原样发给 `file.get`。
    pub remote: String,
    /// 相对下载根目录的目标路径（`/` 分隔，保留目录层级）。
    pub relative: String,
    pub size: u64,
}

/// 目录展开结果：文件清单 + 需要提前建出来的空目录（相对路径）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryPlan {
    pub files: Vec<PlannedFile>,
    /// 空目录（含只含空目录的父目录），相对下载根目录，`/` 分隔。
    pub dirs: Vec<String>,
}

/// 把一层目录项展开成待下载清单：`list` 是当前目录的条目，`base` 是它们在下载根目录下的相对前缀。
///
/// 子目录只作为「需要建出来的目录」记录，递归展开由调用方逐层 `file.list` 后再次调用本函数。
pub fn plan_directory(entries: &[FileEntry], base: &str) -> Result<DirectoryPlan, String> {
    let base = base.trim_matches(|ch| ch == '/' || ch == '\\');
    if !base.is_empty() {
        // 前缀本身也必须是安全的相对路径。
        relative_target(Path::new("."), base)?;
    }
    let mut plan = DirectoryPlan::default();
    for entry in entries {
        let name = entry.name.trim();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(format!("目录项名称不合法: {}", entry.name));
        }
        let relative = if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}/{name}")
        };
        // 名称里可能带 `:` 之类的字符，按目标路径规则再校验一遍。
        relative_target(Path::new("."), &relative)?;
        if entry.dir {
            plan.dirs.push(relative);
        } else {
            plan.files.push(PlannedFile {
                remote: entry.path.clone(),
                relative,
                size: entry.size,
            });
        }
    }
    Ok(plan)
}

/// 目录递归展开的中间状态：BFS 队列 + 已收集的文件。
#[derive(Default)]
struct DirectoryWalk {
    queue: VecDeque<(String, String)>,
    files: Vec<PlannedFile>,
    dirs: Vec<String>,
}

impl DirectoryWalk {
    fn new(root_path: &str, root_name: &str) -> Self {
        let mut walk = Self::default();
        walk.queue
            .push_back((root_path.to_string(), root_name.to_string()));
        walk
    }

    /// 记录一层目录的结果；返回需要继续展开的子目录。
    fn absorb(
        &mut self,
        base: &str,
        entries: &[FileEntry],
    ) -> Result<Vec<(String, String)>, String> {
        let plan = plan_directory(entries, base)?;
        self.files.extend(plan.files);
        self.dirs.extend(plan.dirs.iter().cloned());
        if self.files.len() > MAX_DIRECTORY_FILES {
            return Err(format!("目录文件数超过 {MAX_DIRECTORY_FILES}，请分批下载"));
        }
        Ok(entries
            .iter()
            .filter(|entry| entry.dir)
            .map(|entry| {
                let child_base = if base.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{base}/{}", entry.name)
                };
                (entry.path.clone(), child_base)
            })
            .collect())
    }
}

// ---------------------------------------------------------------- 命令

#[derive(Debug, Deserialize)]
pub struct UploadItem {
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub tid: u16,
    pub root: String,
    pub total: u64,
    /// 被控端已持有的前缀，`[{ item, offset }]`；前端从这里继续发送。
    pub resume: Vec<FileResume>,
}

/// 上传第一步：把文件清单发给被控端，等它确认接收目录。
#[tauri::command]
pub async fn offer_files(
    shared: State<'_, std::sync::Arc<Shared>>,
    items: Vec<UploadItem>,
) -> Result<UploadSession, String> {
    if items.is_empty() {
        return Err("没有选择文件".to_string());
    }
    let total: u64 = items.iter().map(|item| item.size).sum();
    let payload: Vec<FileItem> = items
        .into_iter()
        .map(|item| FileItem {
            path: item.path,
            size: item.size,
            sha256: item.sha256,
        })
        .collect();

    let (tx, rx) = oneshot::channel();
    let tid = {
        let mut hub = shared.files.lock().unwrap();
        let tid = hub.alloc();
        hub.pending.insert(tid, PendingReply::Accept(tx));
        tid
    };
    if let Err(error) = tunnel(
        &shared,
        &Message::FileOffer(FileOffer {
            tid,
            items: payload,
            total_size: total,
        }),
    )
    .await
    {
        shared.files.lock().unwrap().pending.remove(&tid);
        return Err(error);
    }

    let accept = match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(accept)) => accept,
        Ok(Err(_)) => {
            shared.files.lock().unwrap().pending.remove(&tid);
            return Err("请求已失效".to_string());
        }
        Err(_) => {
            shared.files.lock().unwrap().pending.remove(&tid);
            return Err("被控端未响应".to_string());
        }
    };
    // 被控端可能已经有前缀：流控基线从续传位置算起，否则会误判窗口。
    let resumed: u64 = accept.resume.iter().map(|item| item.offset).sum();
    shared.files.lock().unwrap().sends.insert(
        tid,
        SendState {
            confirmed: std::sync::Arc::new(AtomicU64::new(resumed)),
        },
    );
    Ok(UploadSession {
        tid,
        root: accept.root,
        total,
        resume: accept.resume,
    })
}

/// 上传数据块：`data` 是前端切好的 base64，原样转发，不在 Rust 侧解码。
#[tauri::command]
pub async fn upload_chunk(
    shared: State<'_, std::sync::Arc<Shared>>,
    tid: u16,
    item: u16,
    offset: u64,
    data: String,
) -> Result<(), String> {
    let confirmed = {
        let hub = shared.files.lock().unwrap();
        hub.sends.get(&tid).map(|send| send.confirmed.clone())
    }
    .ok_or_else(|| "传输已结束".to_string())?;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if offset.saturating_sub(confirmed.load(Ordering::Relaxed)) <= FILE_WINDOW_BYTES {
            break;
        }
        if Instant::now() > deadline {
            return Err("被控端确认超时".to_string());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tunnel(
        &shared,
        &Message::FileChunk(FileChunk {
            tid,
            item,
            offset,
            data,
        }),
    )
    .await
}

/// 浏览被控端目录。
#[tauri::command]
pub async fn list_remote_files(
    shared: State<'_, std::sync::Arc<Shared>>,
    path: String,
) -> Result<FileListOk, String> {
    list_remote_path(&shared, &path).await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadInfo {
    pub tid: u16,
    pub name: String,
    pub size: u64,
    pub path: String,
    /// 本次从哪个字节开始；大于 0 表示命中了断点。
    pub resume: u64,
}

/// 下载被控端的单个文件到本机下载目录；没有空位时排队等待。
#[tauri::command]
pub async fn download_remote_file(
    app: AppHandle,
    shared: State<'_, std::sync::Arc<Shared>>,
    path: String,
) -> Result<DownloadInfo, String> {
    let tid = {
        let mut hub = shared.files.lock().unwrap();
        let tid = hub.alloc();
        hub.queue.push_back(QueuedDownload {
            tid,
            path: path.clone(),
            target: None,
            group: 0,
            size: 0,
        });
        tid
    };
    schedule_downloads(&app, &shared);
    // 立刻返回排队位置；真正开始时再推送 start 事件。
    let name = safe_name(Path::new(&path).to_string_lossy().as_ref());
    Ok(DownloadInfo {
        tid,
        name,
        size: 0,
        path: download_root().display().to_string(),
        resume: 0,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryDownloadInfo {
    /// 整组下载的分组号；该组每个文件的 `file-transfer` 事件都会带 `group`。
    pub group: u64,
    /// 根目录名（落在 `Downloads\ScreenWall\<name>` 下）。
    pub name: String,
    /// 展开得到的文件数（空目录不算）。
    pub files: usize,
    /// 展开得到的空目录数。
    pub empty_dirs: usize,
    /// 目录里没有任何文件也没有子目录时为 true。
    pub empty: bool,
}

/// 下载被控端的整个目录：客户端递归 `file.list` 展开，再逐个排队下载。
///
/// 相对目录结构保留在 `Downloads\ScreenWall\<目录名>\...` 下，空目录也会建出来；
/// 每个文件仍走单文件下载（独立 tid、断点续传与 SHA256 校验）。
#[tauri::command]
pub async fn download_remote_directory(
    app: AppHandle,
    shared: State<'_, std::sync::Arc<Shared>>,
    path: String,
) -> Result<DirectoryDownloadInfo, String> {
    let name = safe_name(
        Path::new(path.trim_end_matches(['\\', '/']))
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone())
            .as_str(),
    );
    let plan = expand_directory(&shared, &path, &name).await?;
    let group = {
        let mut hub = shared.files.lock().unwrap();
        hub.group_seq = hub.group_seq.wrapping_add(1).max(1);
        hub.group_seq
    };

    // 空目录先建出来：目录下载里没有任何文件时也能看到结果。
    let root = download_root();
    for relative in &plan.dirs {
        if let Ok(target) = relative_target(&root, relative) {
            let _ = std::fs::create_dir_all(target);
        }
    }

    let files = plan.files.len();
    let dirs = plan.dirs.len();
    if files == 0 {
        emit(
            &app,
            json!({
                "tid": 0,
                "kind": "complete",
                "direction": "download",
                "group": group,
                "name": name,
                "path": relative_target(&root, &name).map(|p| p.display().to_string()).unwrap_or_default(),
                "done": 0,
                "total": 0,
                "files": 0,
                "emptyDirs": dirs,
                "directory": true,
                "empty": plan.dirs.is_empty(),
            }),
        );
        return Ok(DirectoryDownloadInfo {
            group,
            name,
            files: 0,
            empty_dirs: dirs,
            empty: dirs == 0,
        });
    }

    let queued: Vec<(u16, String, u64)> = {
        let mut hub = shared.files.lock().unwrap();
        plan.files
            .iter()
            .map(|file| {
                let tid = hub.alloc();
                hub.queue.push_back(QueuedDownload {
                    tid,
                    path: file.remote.clone(),
                    target: Some(file.relative.clone()),
                    group,
                    size: file.size,
                });
                (tid, file.relative.clone(), file.size)
            })
            .collect()
    };
    // 先让前端把整组任务画出来（排队中），再按并发上限逐个开始。
    let total: u64 = queued.iter().map(|(_, _, size)| *size).sum();
    emit(
        &app,
        json!({
            "tid": 0,
            "kind": "directory",
            "direction": "download",
            "group": group,
            "name": name,
            "path": relative_target(&root, &name).map(|p| p.display().to_string()).unwrap_or_default(),
            "files": files,
            "emptyDirs": dirs,
            "total": total,
        }),
    );
    for (tid, relative, size) in &queued {
        emit(
            &app,
            json!({
                "tid": tid,
                "kind": "queued",
                "direction": "download",
                "group": group,
                "name": relative,
                "total": size,
                "size": size,
            }),
        );
    }
    schedule_downloads(&app, &shared);
    Ok(DirectoryDownloadInfo {
        group,
        name,
        files,
        empty_dirs: dirs,
        empty: false,
    })
}

/// 递归展开远程目录（BFS，一层一个 `file.list`），返回待下载清单。
async fn expand_directory(
    shared: &Shared,
    path: &str,
    root_name: &str,
) -> Result<DirectoryPlan, String> {
    let mut walk = DirectoryWalk::new(path, root_name);
    while let Some((dir, base)) = walk.queue.pop_front() {
        if base.matches('/').count() + 1 > MAX_DIRECTORY_DEPTH {
            return Err(format!("目录层级超过 {MAX_DIRECTORY_DEPTH}，请分批下载"));
        }
        let listing = match list_remote_path(shared, &dir).await {
            Ok(listing) => listing,
            Err(error) => {
                // 根目录读不到是硬失败；子目录失败只记日志，继续展开其他目录。
                if base == root_name {
                    return Err(error);
                }
                tracing::warn!(dir = %dir, %error, "目录下载时跳过一个子目录");
                continue;
            }
        };
        let children = walk.absorb(&base, &listing.entries)?;
        for child in children {
            walk.queue.push_back(child);
        }
    }
    Ok(DirectoryPlan {
        files: walk.files,
        dirs: walk.dirs,
    })
}

/// 发一条 `file.list` 并等回包；`list_remote_files` 与目录展开共用。
async fn list_remote_path(shared: &Shared, path: &str) -> Result<FileListOk, String> {
    let (tx, rx) = oneshot::channel();
    let tid = {
        let mut hub = shared.files.lock().unwrap();
        let tid = hub.alloc();
        hub.lists.insert(tid, tx);
        tid
    };
    if let Err(error) = tunnel(
        shared,
        &Message::FileListReq(FileListReq {
            tid,
            path: path.to_string(),
        }),
    )
    .await
    {
        shared.files.lock().unwrap().lists.remove(&tid);
        return Err(error);
    }
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(ok)) => Ok(ok),
        _ => {
            shared.files.lock().unwrap().lists.remove(&tid);
            Err("读取目录超时".to_string())
        }
    }
}

/// 有空位就按 FIFO 开始排队中的下载。
///
/// 这里只做「领取 + 后台启动」，不在调用方的任务里等 `file.offer`，
/// 否则会卡住收包循环（画面/输入也会跟着停）。`dequeue` 会先占位，
/// 所以多个调用方同时调度也不会超过并发上限。
fn schedule_downloads(app: &AppHandle, shared: &std::sync::Arc<Shared>) {
    loop {
        let Some(queued) = shared.files.lock().unwrap().dequeue() else {
            return;
        };
        let app = app.clone();
        let shared = shared.clone();
        tauri::async_runtime::spawn(async move {
            let tid = queued.tid;
            if let Err(error) = start_download(&app, &shared, queued).await {
                tracing::warn!(tid, %error, "下载任务启动失败");
            }
            // 启动失败会释放名额，成功则占着名额；两种情况都再补一次队列。
            schedule_downloads(&app, &shared);
        });
    }
}

/// 开始一个排队中的下载：`file.get` → `file.offer` → 就绪 → `file.accept`。
///
/// 失败时释放占位并把错误事件推给前端；调用方继续调度后面的任务。
async fn start_download(
    app: &AppHandle,
    shared: &Shared,
    queued: QueuedDownload,
) -> Result<(), String> {
    let QueuedDownload {
        tid,
        path,
        target,
        group,
        size,
    } = queued;
    let result = run_download_start(app, shared, tid, &path, target.as_deref(), group, size).await;
    if let Err(error) = &result {
        // 成功时 `starting` 已经转成 `recv` 名额；失败（或取消）时才在这里归还占位。
        let suppress = {
            let mut hub = shared.files.lock().unwrap();
            if hub.recv.contains_key(&tid) {
                // 已经进入接收、但在发 accept 时失败：一并清掉，别占着名额。
                hub.abort_download(tid);
            } else {
                hub.end_starting(tid);
            }
            hub.cancelled.remove(&tid)
        };
        if !suppress {
            emit(
                app,
                json!({
                    "tid": tid,
                    "kind": "error",
                    "direction": "download",
                    "group": group,
                    "name": target.clone().unwrap_or_else(|| safe_name(&path)),
                    "msg": error,
                }),
            );
        }
    }
    result
}

async fn run_download_start(
    app: &AppHandle,
    shared: &Shared,
    tid: u16,
    path: &str,
    target: Option<&str>,
    group: u64,
    known_size: u64,
) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    {
        let mut hub = shared.files.lock().unwrap();
        hub.pending.insert(tid, PendingReply::Offer(tx));
    }
    if let Err(error) = tunnel(
        shared,
        &Message::FileGet(FileGet {
            tid,
            path: path.to_string(),
        }),
    )
    .await
    {
        shared.files.lock().unwrap().pending.remove(&tid);
        return Err(error);
    }
    // 从这里开始占位已经转成正式的接收状态：失败时由调用方统一回收。
    let offer = match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(offer)) => offer,
        _ => {
            let mut hub = shared.files.lock().unwrap();
            // 取消会移除 pending；标记说明是用户取消，留给调用方回收并抑制错误事件。
            let cancelled = hub.cancelled.contains(&tid);
            hub.pending.remove(&tid);
            return Err(if cancelled {
                "已取消".to_string()
            } else {
                "被控端没有响应下载请求".to_string()
            });
        }
    };
    let item = offer
        .items
        .into_iter()
        .next()
        .ok_or_else(|| "被控端没有返回文件".to_string())?;
    let _ = known_size;
    let fallback = safe_name(&item.path);
    let (name, dest) = match target {
        Some(relative) => {
            let root = download_root();
            let dest = relative_target(&root, relative)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("创建下载目录失败: {error}"))?;
            }
            (relative.to_string(), dest)
        }
        None => {
            let dest = unique_download(&fallback)?;
            (fallback, dest)
        }
    };
    let root = dest
        .parent()
        .map(|parent| parent.display().to_string())
        .unwrap_or_default();

    // 分片键要能稳定识别「同一设备 + 同一远程路径 + 同一大小」。
    let device = shared
        .control
        .read()
        .await
        .as_ref()
        .map(|control| control.device_id.clone())
        .unwrap_or_default();
    let key = part_key(&device, path, item.size);
    let part = parts_dir().join(format!("{key}.part"));
    if let Some(dir) = part.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建下载分片目录失败: {error}"))?;
    }
    cleanup_stale_parts(&parts_dir());
    let resume = resume_offset(&part, item.size);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&part)
        .map_err(|error| format!("创建下载分片失败: {error}"))?;
    if file.metadata().map(|meta| meta.len()).unwrap_or(0) != resume {
        file.set_len(resume)
            .map_err(|error| format!("截断下载分片失败: {error}"))?;
    }
    let hasher = hash_prefix(&part, resume)?;
    if resume == item.size {
        // 分片已经完整（上次收完但没来得及收尾），直接校验改名，不再要数据。
        file.seek(SeekFrom::End(0))
            .map_err(|error| format!("定位下载分片失败: {error}"))?;
    } else {
        file.seek(SeekFrom::Start(resume))
            .map_err(|error| format!("定位下载分片失败: {error}"))?;
    }

    {
        let mut hub = shared.files.lock().unwrap();
        // 等待 offer 期间被取消：别再开始接收。标记留给调用方回收并抑制错误事件。
        if hub.cancelled.contains(&tid) {
            return Err("已取消".to_string());
        }
        hub.end_starting(tid);
        hub.recv.insert(
            tid,
            RecvState {
                tid,
                name: name.clone(),
                dest: dest.clone(),
                part: part.clone(),
                file,
                size: item.size,
                written: resume,
                last_progress: resume,
                hasher,
                resume,
                expected_sha: None,
                group,
            },
        );
    }
    // 告诉被控端「本地已就绪」并带上续传起点；旧版被控端会忽略 offset，从 0 发。
    if let Err(error) = tunnel(
        shared,
        &Message::FileAccept(FileAccept {
            tid,
            root: root.clone(),
            resume: Vec::new(),
            offset: resume,
        }),
    )
    .await
    {
        // 没发出去就别留着半截下载状态，否则会一直占着并发名额。
        shared.files.lock().unwrap().abort_download(tid);
        return Err(error);
    }

    emit(
        app,
        json!({
            "tid": tid,
            "kind": "start",
            "direction": "download",
            "group": group,
            "name": name,
            "path": dest.display().to_string(),
            "done": resume,
            "total": item.size,
            "resume": resume,
        }),
    );
    Ok(())
}

/// 取消上传或下载；排队中的下载会直接移出队列并推一条 `cancelled`。
#[tauri::command]
pub async fn cancel_file_transfer(
    app: AppHandle,
    shared: State<'_, std::sync::Arc<Shared>>,
    tid: u16,
) -> Result<(), String> {
    let (recv, queued, cancelled) = {
        let mut hub = shared.files.lock().unwrap();
        hub.sends.remove(&tid);
        hub.lists.remove(&tid);
        let queued = hub
            .queue
            .iter()
            .position(|item| item.tid == tid)
            .and_then(|index| hub.queue.remove(index));
        let recv = hub.abort_download(tid);
        // 已发出 file.get、还在等 offer（或刚拿到、正在建分片）：记下来，开始阶段看到标记就放弃。
        let starting = hub.starting.contains(&tid);
        hub.pending.remove(&tid);
        let known = recv.is_some() || queued.is_some();
        if !known && starting {
            hub.cancelled.insert(tid);
        }
        (recv, queued, known)
    };
    if cancelled {
        emit(
            &app,
            json!({
                "tid": tid,
                "kind": "cancelled",
                "group": queued.as_ref().map_or(0, |item| item.group),
                "name": recv.or_else(|| queued.map(|item| item.target.unwrap_or(item.path))),
            }),
        );
        // 腾出的名额立刻给排队任务。
        schedule_downloads(&app, &shared);
    }
    tunnel(
        &shared,
        &Message::FileCancel(FileCancel {
            tid,
            msg: "主控端已取消".to_string(),
        }),
    )
    .await
}

// ---------------------------------------------------------------- 收包

/// 处理被控端发来的文件消息；返回 true 表示已经消费。
pub async fn on_message(
    app: &AppHandle,
    shared: &std::sync::Arc<Shared>,
    message: Message,
) -> bool {
    match message {
        Message::FileAccept(accept) => {
            let reply = shared.files.lock().unwrap().pending.remove(&accept.tid);
            match reply {
                Some(PendingReply::Accept(sender)) => {
                    let tid = accept.tid;
                    let root = accept.root.clone();
                    let resumed: u64 = accept.resume.iter().map(|item| item.offset).sum();
                    let resume = accept.resume.clone();
                    let _ = sender.send(accept);
                    emit(
                        app,
                        json!({
                            "tid": tid,
                            "kind": "accepted",
                            "direction": "upload",
                            "root": root,
                            "resume": resume,
                            "resumed": resumed,
                        }),
                    );
                    true
                }
                other => {
                    if let Some(reply) = other {
                        shared
                            .files
                            .lock()
                            .unwrap()
                            .pending
                            .insert(accept.tid, reply);
                    }
                    false
                }
            }
        }
        Message::FileOffer(offer) => {
            let reply = shared.files.lock().unwrap().pending.remove(&offer.tid);
            match reply {
                Some(PendingReply::Offer(sender)) => {
                    let _ = sender.send(offer);
                    true
                }
                other => {
                    if let Some(reply) = other {
                        shared
                            .files
                            .lock()
                            .unwrap()
                            .pending
                            .insert(offer.tid, reply);
                    }
                    false
                }
            }
        }
        Message::FileListOk(ok) => {
            let sender = shared.files.lock().unwrap().lists.remove(&ok.tid);
            match sender {
                Some(sender) => {
                    let _ = sender.send(ok);
                    true
                }
                None => false,
            }
        }
        Message::FileProgress(progress) => {
            let known = {
                let hub = shared.files.lock().unwrap();
                match hub.sends.get(&progress.tid) {
                    Some(send) => {
                        send.confirmed.fetch_max(progress.done, Ordering::Relaxed);
                        true
                    }
                    None => false,
                }
            };
            if known {
                emit(
                    app,
                    json!({
                        "tid": progress.tid,
                        "kind": "progress",
                        "direction": "upload",
                        "done": progress.done,
                        "total": progress.total,
                    }),
                );
            }
            known
        }
        Message::FileChunk(chunk) => {
            write_download_chunk(app, shared, chunk).await;
            true
        }
        Message::FileDone(done) => {
            let freed = handle_file_done(app, shared, done);
            if freed {
                schedule_downloads(app, shared);
            }
            true
        }
        Message::FileComplete(complete) => {
            let freed = finalize_or_finish(app, shared, complete);
            if freed {
                schedule_downloads(app, shared);
            }
            true
        }
        Message::FileCancel(cancel) => {
            let (recv, group) = {
                let mut hub = shared.files.lock().unwrap();
                hub.sends.remove(&cancel.tid);
                hub.pending.remove(&cancel.tid);
                let group = hub
                    .queue
                    .iter()
                    .position(|item| item.tid == cancel.tid)
                    .and_then(|index| hub.queue.remove(index))
                    .map(|item| item.group)
                    .unwrap_or(0);
                (hub.abort_download(cancel.tid), group)
            };
            emit(
                app,
                json!({
                    "tid": cancel.tid,
                    "kind": "cancelled",
                    "group": group,
                    "name": recv,
                    "msg": cancel.msg,
                }),
            );
            schedule_downloads(app, shared);
            true
        }
        _ => false,
    }
}

async fn write_download_chunk(app: &AppHandle, shared: &Shared, chunk: FileChunk) {
    let ack = {
        let mut hub = shared.files.lock().unwrap();
        let Some(recv) = hub.recv.get_mut(&chunk.tid) else {
            return;
        };
        let data = match base64::engine::general_purpose::STANDARD.decode(chunk.data.as_bytes()) {
            Ok(data) => data,
            Err(_) => return,
        };
        // 旧版被控端不支持续传，会无视 accept.offset 从 0 重发：清掉前缀后重收。
        if recv.resume > 0 && chunk.offset == 0 {
            recv.file.set_len(0).ok();
            if recv.file.seek(SeekFrom::Start(0)).is_err() {
                return;
            }
            recv.hasher = Sha256::new();
            recv.written = 0;
            recv.last_progress = 0;
            recv.resume = 0;
            tracing::warn!(tid = chunk.tid, "被控端不支持下载续传，改为从 0 接收");
        }
        if chunk.offset != recv.written {
            tracing::warn!(
                tid = chunk.tid,
                expected = recv.written,
                got = chunk.offset,
                "下载数据块偏移不连续"
            );
            return;
        }
        if let Err(error) = recv.file.write_all(&data) {
            tracing::warn!(tid = chunk.tid, %error, "写下载文件失败");
            return;
        }
        recv.hasher.update(&data);
        recv.written += data.len() as u64;
        let done = recv.written;
        let total = recv.size;
        let name = recv.name.clone();
        let tid = recv.tid;
        let group = recv.group;
        let due = recv.written - recv.last_progress >= FILE_PROGRESS_STEP;
        if due {
            recv.last_progress = recv.written;
        }
        emit(
            app,
            json!({
                "tid": tid,
                "kind": "progress",
                "direction": "download",
                "group": group,
                "name": name,
                "done": done,
                "total": total,
            }),
        );
        due.then_some((tid, done, total))
    };
    if let Some((tid, done, total)) = ack {
        let _ = tunnel(
            shared,
            &Message::FileProgress(FileProgress { tid, done, total }),
        )
        .await;
    }
}

/// 处理 `file.done`；返回 true 表示释放了一个下载并发名额。
fn handle_file_done(app: &AppHandle, shared: &Shared, done: FileDone) -> bool {
    let (download, group, progress) = {
        let hub = shared.files.lock().unwrap();
        match hub.recv.get(&done.tid) {
            Some(recv) => (
                true,
                recv.group,
                Some((recv.name.clone(), recv.written, recv.size)),
            ),
            None => (false, 0, None),
        }
    };
    if download {
        if !done.ok {
            let recv = shared.files.lock().unwrap().abort_download(done.tid);
            emit(
                app,
                json!({
                    "tid": done.tid,
                    "kind": "error",
                    "direction": "download",
                    "group": group,
                    "name": recv,
                    "msg": done.msg,
                }),
            );
            return true;
        } else if let Some(expected) = done.sha256 {
            // 记下发送端哈希，等 file.complete 收尾时比对。
            if let Some(recv) = shared.files.lock().unwrap().recv.get_mut(&done.tid) {
                recv.expected_sha = Some(expected);
            }
            // 数据已经收齐，正在比对 SHA256 并改名：界面显示「校验中」。
            if let Some((name, written, size)) = progress {
                emit(
                    app,
                    json!({
                        "tid": done.tid,
                        "kind": "verifying",
                        "direction": "download",
                        "group": group,
                        "name": name,
                        "done": written,
                        "total": size,
                    }),
                );
            }
        }
        // 成功时等 file.complete 收尾，避免早于最后一个数据块。
        return false;
    }
    emit(
        app,
        json!({
            "tid": done.tid,
            "kind": "file-done",
            "direction": "upload",
            "path": done.path,
            "ok": done.ok,
            "msg": done.msg,
        }),
    );
    false
}

/// 处理 `file.complete`；返回 true 表示释放了一个下载并发名额。
fn finalize_or_finish(app: &AppHandle, shared: &Shared, complete: FileComplete) -> bool {
    let is_download = {
        let hub = shared.files.lock().unwrap();
        hub.recv.contains_key(&complete.tid)
    };
    if !is_download {
        let known = shared
            .files
            .lock()
            .unwrap()
            .sends
            .remove(&complete.tid)
            .is_some();
        if known {
            emit(
                app,
                json!({
                    "tid": complete.tid,
                    "kind": "complete",
                    "direction": "upload",
                    "files": complete.files,
                    "total": complete.bytes,
                    "done": complete.bytes,
                }),
            );
        }
        return false;
    }

    let recv = {
        let mut hub = shared.files.lock().unwrap();
        match hub.recv.get(&complete.tid) {
            Some(recv) if recv.written >= recv.size => hub.recv.remove(&complete.tid),
            _ => None,
        }
    };
    let Some(recv) = recv else {
        // 字节数不对，按失败处理；分片留着，下次还能续。
        let recv = shared.files.lock().unwrap().abort_download(complete.tid);
        emit(
            app,
            json!({
                "tid": complete.tid,
                "kind": "error",
                "direction": "download",
                "name": recv,
                "msg": "接收的字节数与文件大小不一致",
            }),
        );
        return true;
    };
    let _ = recv.file.sync_all();
    drop(recv.file);
    let actual = hex::encode(recv.hasher.finalize());
    if let Some(expected) = &recv.expected_sha {
        if expected != &actual {
            // 内容对不上，坏分片不能留给下次续传。
            let _ = std::fs::remove_file(&recv.part);
            emit(
                app,
                json!({
                    "tid": recv.tid,
                    "kind": "error",
                    "direction": "download",
                    "group": recv.group,
                    "name": recv.name,
                    "msg": format!("SHA256 校验失败（实际 {actual}）"),
                }),
            );
            return true;
        }
    }
    let verified = recv.expected_sha.is_some();
    // 下载期间可能有同名文件落盘；改名时再挑一次，绝不覆盖已有文件。
    let dest = if recv.dest.exists() {
        match relative_target(&download_root(), &recv.name) {
            Ok(target) => {
                unique_in(&download_root(), &target).unwrap_or_else(|_| recv.dest.clone())
            }
            Err(_) => recv.dest.clone(),
        }
    } else {
        recv.dest.clone()
    };
    match std::fs::rename(&recv.part, &dest) {
        Ok(()) => emit(
            app,
            json!({
                "tid": recv.tid,
                "kind": "complete",
                "direction": "download",
                "group": recv.group,
                "name": recv.name,
                "path": dest.display().to_string(),
                "done": recv.written,
                "total": recv.size,
                "sha256": actual,
                "verified": verified,
            }),
        ),
        Err(error) => {
            let _ = std::fs::remove_file(&recv.part);
            emit(
                app,
                json!({
                    "tid": recv.tid,
                    "kind": "error",
                    "direction": "download",
                    "group": recv.group,
                    "name": recv.name,
                    "msg": format!("保存失败: {error}"),
                }),
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "screenwall-client-{tag}-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn part_key_identifies_device_path_and_size() {
        let base = part_key("device-a", r"C:\dir\a.bin", 100);
        assert_eq!(base.len(), 64, "应为十六进制 SHA256");
        assert!(base.chars().all(|ch| ch.is_ascii_hexdigit()));

        // 同一设备 + 同一路径 + 同一大小 → 同一个键。
        assert_eq!(base, part_key("device-a", r"C:\dir\a.bin", 100));
        // 分隔符差异不影响。
        assert_eq!(base, part_key("device-a", "C:/dir/a.bin", 100));
        // 换设备、换路径、换大小都必须换键，避免错续。
        assert_ne!(base, part_key("device-b", r"C:\dir\a.bin", 100));
        assert_ne!(base, part_key("device-a", r"C:\dir\b.bin", 100));
        assert_ne!(base, part_key("device-a", r"C:\dir\a.bin", 101));
    }

    #[test]
    fn part_key_is_not_the_raw_name() {
        // 路径里带 .. 或特殊字符时也只得到纯哈希文件名，不会参与路径拼接。
        let key = part_key("dev", "../../evil/name?.bin", 5);
        assert_eq!(key.len(), 64);
        assert!(!key.contains(".."));
        assert!(!key.contains('/'));
        assert!(!key.contains('\\'));
        assert!(!key.contains('?'));
    }

    #[test]
    fn resume_offset_uses_existing_part() {
        let dir = temp_dir("resume");
        let part = dir.join("x.part");
        // 没有分片：从 0 开始。
        assert_eq!(resume_offset(&part, 100), 0);
        // 部分前缀：接着下。
        std::fs::write(&part, b"12345").unwrap();
        assert_eq!(resume_offset(&part, 100), 5);
        // 完整分片：直接收尾。
        std::fs::write(&part, vec![0u8; 100]).unwrap();
        assert_eq!(resume_offset(&part, 100), 100);
        // 比分片还小（被替换）：删掉重来。
        std::fs::write(&part, vec![0u8; 200]).unwrap();
        assert_eq!(resume_offset(&part, 100), 0);
        assert!(!part.exists(), "大小不符的分片应删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_target_keeps_hierarchy_and_rejects_escape() {
        let root = Path::new(r"C:\Users\u\Downloads\ScreenWall");
        // 单层与多层都保留层级，分隔符统一。
        assert_eq!(relative_target(root, "a.txt").unwrap(), root.join("a.txt"));
        assert_eq!(
            relative_target(root, "docs/2024/report.pdf").unwrap(),
            root.join("docs").join("2024").join("report.pdf")
        );
        assert_eq!(
            relative_target(root, r"docs\2024\report.pdf").unwrap(),
            root.join("docs").join("2024").join("report.pdf")
        );
        // `.` 被忽略，空路径就是根目录本身。
        assert_eq!(
            relative_target(root, "./a/b").unwrap(),
            root.join("a").join("b")
        );
        assert_eq!(relative_target(root, "").unwrap(), root.to_path_buf());
        assert_eq!(relative_target(root, "  ").unwrap(), root.to_path_buf());

        // 各种逃逸形式都必须拒绝。
        for bad in [
            "../evil.txt",
            "a/../../evil.txt",
            r"..\evil.txt",
            "/etc/passwd",
            r"\Windows\system32",
            "C:/Windows/evil.txt",
            "C:evil.txt",
            r"\\server\share\evil.txt",
            "a/./../b",
        ] {
            assert!(relative_target(root, bad).is_err(), "应拒绝 {bad}");
        }
    }

    fn entry(name: &str, dir: bool, size: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!(r"C:\remote\{name}"),
            dir,
            size,
            modified: None,
        }
    }

    #[test]
    fn plan_directory_keeps_relative_layout() {
        let entries = vec![
            entry("docs", true, 0),
            entry("empty", true, 0),
            entry("a.txt", false, 10),
            entry("b.bin", false, 2048),
        ];
        let plan = plan_directory(&entries, "proj").unwrap();
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].relative, "proj/a.txt");
        assert_eq!(plan.files[0].size, 10);
        assert_eq!(plan.files[0].remote, r"C:\remote\a.txt");
        assert_eq!(plan.files[1].relative, "proj/b.bin");
        // 子目录作为待建目录，空目录也在里面。
        assert_eq!(
            plan.dirs,
            vec!["proj/docs".to_string(), "proj/empty".to_string()]
        );

        // 空 base 时相对路径就是名字本身。
        let plan = plan_directory(&entries, "").unwrap();
        assert_eq!(plan.files[0].relative, "a.txt");
        assert_eq!(plan.dirs[0], "docs");

        // base 首尾的分隔符会被规整。
        let plan = plan_directory(&entries, "/proj/").unwrap();
        assert_eq!(plan.files[0].relative, "proj/a.txt");
    }

    #[test]
    fn plan_directory_rejects_unsafe_names() {
        for bad in ["..", ".", "", "a/b", r"a\b"] {
            let entries = vec![entry(bad, false, 1)];
            assert!(plan_directory(&entries, "root").is_err(), "应拒绝 {bad:?}");
        }
        // 带盘符的名字在 Windows 目标路径里不安全。
        let entries = vec![entry("C:evil", false, 1)];
        assert!(plan_directory(&entries, "root").is_err());
        // base 自身也不能逃逸。
        let entries = vec![entry("a.txt", false, 1)];
        assert!(plan_directory(&entries, "../outside").is_err());
    }

    #[test]
    fn plan_directory_detects_empty_directory() {
        let plan = plan_directory(&[], "only").unwrap();
        assert!(plan.files.is_empty());
        assert!(plan.dirs.is_empty());
    }

    #[test]
    fn directory_walk_absorbs_layers() {
        let mut walk = DirectoryWalk::new(r"C:\remote\proj", "proj");
        let children = walk
            .absorb("proj", &[entry("sub", true, 0), entry("top.txt", false, 3)])
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0, r"C:\remote\sub");
        assert_eq!(children[0].1, "proj/sub");
        assert_eq!(walk.files[0].relative, "proj/top.txt");
        assert_eq!(walk.dirs, vec!["proj/sub".to_string()]);

        // 第二层继续保留前缀。
        let children = walk
            .absorb("proj/sub", &[entry("deep.txt", false, 1)])
            .unwrap();
        assert!(children.is_empty());
        assert_eq!(walk.files[1].relative, "proj/sub/deep.txt");
    }

    #[test]
    fn unique_in_avoids_overwrite_in_subdirectory() {
        let dir = temp_dir("unique-sub");
        let sub = dir.join("docs");
        std::fs::create_dir_all(&sub).unwrap();
        let first = sub.join("a.txt");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_in(&dir, &first).unwrap();
        assert_eq!(second, sub.join("a (1).txt"));
        // 不存在的路径原样返回。
        let fresh = sub.join("b.txt");
        assert_eq!(unique_in(&dir, &fresh).unwrap(), fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_prefix_matches_full_prefix() {
        let dir = temp_dir("prefix");
        let part = dir.join("p.part");
        std::fs::write(&part, b"hello world").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello");
        let expected = hex::encode(hasher.finalize());
        let actual = hex::encode(hash_prefix(&part, 5).unwrap().finalize());
        assert_eq!(actual, expected);

        let empty = hex::encode(hash_prefix(&part, 0).unwrap().finalize());
        let mut hasher = Sha256::new();
        hasher.update(b"");
        assert_eq!(empty, hex::encode(hasher.finalize()));

        assert!(hash_prefix(&part, 12).is_err(), "前缀超过分片长度应报错");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
