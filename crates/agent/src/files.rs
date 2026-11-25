//! 被控端文件接收与发送：上传落盘到 `Downloads\ScreenWall`，下载按需读盘。
//!
//! 数据块走 `file.chunk`（base64），流控用 `file.progress` 确认；
//! 大文件不会一次性读进内存，也不会一次性塞满发送队列。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use screenwall_proto::{
    FileChunk, FileDone, FileItem, FileResume, FILE_CHUNK_BYTES, FILE_PROGRESS_STEP,
};
use sha2::{Digest, Sha256};

/// 上传落盘目录：`%USERPROFILE%\Downloads\ScreenWall`。
pub fn receive_root() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads").join("ScreenWall")
}

/// 只接受相对路径，逐段校验，防止 `..` 或盘符逃出接收目录。
fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let cleaned = relative.replace('\\', "/");
    let path = Path::new(&cleaned);
    if path.is_absolute() {
        bail!("拒绝绝对路径: {relative}");
    }
    let mut out = root.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => bail!("路径包含非法片段: {relative}"),
        }
    }
    if out == root {
        bail!("路径为空: {relative}");
    }
    Ok(out)
}

/// 断点续传的临时文件目录：`Downloads\ScreenWall\.screenwall-parts`。
fn parts_dir(root: &Path) -> PathBuf {
    root.join(".screenwall-parts")
}

/// 只接受完整的 64 位十六进制 SHA256，避免用文件名做路径拼接时被注入。
fn valid_sha(sha: &str) -> Option<String> {
    let normalized = sha.trim().to_ascii_lowercase();
    if normalized.len() == 64 && normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Some(normalized)
    } else {
        None
    }
}

/// 从文件开头读到 `offset` 算前缀 SHA256；下载续传时先补上前缀，收尾哈希才是整文件的。
pub fn hash_prefix(file: &mut File, offset: u64) -> Result<Sha256> {
    let mut hasher = Sha256::new();
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = offset;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            bail!("文件长度不足，无法从 {offset} 续传");
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher)
}

/// 读取整文件算 SHA256（分块读，不把大文件塞进内存）。
fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("打开 {} 失败", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 每个文件的续传计划：已持有的前缀字节数和复用的临时文件。
struct ItemPlan {
    resume: u64,
    part: Option<PathBuf>,
    /// 期望的整文件哈希（用于收尾校验，也用于判断能否跨会话续传）。
    expected_sha: Option<String>,
}

fn plan_item(root: &Path, item: &FileItem, seen: &mut Vec<String>) -> ItemPlan {
    let Some(sha) = valid_sha(&item.sha256) else {
        return ItemPlan {
            resume: 0,
            part: None,
            expected_sha: None,
        };
    };
    // 同一批里出现重复内容（同哈希）时不共用同一个 part，避免互相覆盖。
    if seen.iter().any(|value| value == &sha) {
        return ItemPlan {
            resume: 0,
            part: None,
            expected_sha: Some(sha),
        };
    }
    seen.push(sha.clone());
    let part = parts_dir(root).join(format!("{sha}.part"));
    // 已有前缀比目标文件还长，说明是坏数据，丢掉重来。
    let resume = match fs::metadata(&part) {
        Ok(meta) if meta.len() > 0 && meta.len() <= item.size => meta.len(),
        Ok(_) => {
            let _ = fs::remove_file(&part);
            0
        }
        Err(_) => 0,
    };
    ItemPlan {
        resume,
        part: Some(part),
        expected_sha: Some(sha),
    }
}

/// 清掉超过 7 天没人续传的临时文件，避免长期占盘。
fn cleanup_stale_parts(root: &Path) {
    let dir = parts_dir(root);
    let Ok(entries) = fs::read_dir(&dir) else {
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
            let _ = fs::remove_file(&path);
        }
    }
}

struct RecvFile {
    /// 无哈希时在打开阶段就确定目标名；有哈希时等收尾校验通过再决定。
    target: Option<PathBuf>,
    tmp: PathBuf,
    file: File,
    size: u64,
    written: u64,
    expected_sha: Option<String>,
    /// 能否保留前缀供下次续传（有独立 part 文件才算）。
    resumable: bool,
    relative: String,
}

/// 一次上传会话（一条 `file.offer` 对应多个文件）。
pub struct RecvSession {
    pub tid: u16,
    pub root: PathBuf,
    items: Vec<FileItem>,
    plans: Vec<ItemPlan>,
    files: Vec<Option<RecvFile>>,
    pub total: u64,
    pub done: u64,
    last_progress: u64,
}

impl RecvSession {
    /// 先校验路径再落临时文件；同名文件自动加序号，避免覆盖。
    pub fn new(tid: u16, items: Vec<FileItem>) -> Result<Self> {
        Self::new_in(receive_root(), tid, items)
    }

    fn new_in(root: PathBuf, tid: u16, items: Vec<FileItem>) -> Result<Self> {
        if items.is_empty() {
            bail!("没有待接收的文件");
        }
        fs::create_dir_all(&root).with_context(|| format!("创建 {} 失败", root.display()))?;
        // 全部路径在回 accept 之前校验，避免传到一半才发现非法。
        let mut total = 0u64;
        for item in &items {
            safe_join(&root, &item.path)
                .with_context(|| format!("文件路径不合法: {}", item.path))?;
            total = total
                .checked_add(item.size)
                .context("待接收文件总大小溢出")?;
        }
        cleanup_stale_parts(&root);
        let mut seen = Vec::new();
        let plans: Vec<ItemPlan> = items
            .iter()
            .map(|item| plan_item(&root, item, &mut seen))
            .collect();
        let done = plans.iter().map(|plan| plan.resume).sum();
        let files = items.iter().map(|_| None).collect();
        Ok(Self {
            tid,
            root,
            items,
            plans,
            files,
            total,
            done,
            last_progress: done,
        })
    }

    pub fn root_label(&self) -> String {
        self.root.display().to_string()
    }

    /// 告诉主控端每个文件从哪里续传；没有断点时返回空列表。
    pub fn resume_list(&self) -> Vec<FileResume> {
        self.plans
            .iter()
            .enumerate()
            .filter(|(_, plan)| plan.resume > 0)
            .map(|(index, plan)| FileResume {
                item: index as u16,
                offset: plan.resume,
            })
            .collect()
    }

    fn open_item(&mut self, index: usize) -> Result<&mut RecvFile> {
        if self.files[index].is_none() {
            let item = self
                .items
                .get(index)
                .with_context(|| format!("文件序号越界: {index}"))?
                .clone();
            let plan = self
                .plans
                .get(index)
                .with_context(|| format!("文件序号越界: {index}"))?;
            let (target, tmp, file, written, resumable) = match &plan.part {
                Some(part) => {
                    if let Some(parent) = part.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("创建 {} 失败", parent.display()))?;
                    }
                    let mut file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(part)
                        .with_context(|| format!("创建 {} 失败", part.display()))?;
                    let existing = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                    if existing < plan.resume || existing > item.size {
                        bail!(
                            "续传文件大小异常（已有 {}，断点 {}，应为 {}）",
                            existing,
                            plan.resume,
                            item.size
                        );
                    }
                    file.seek(SeekFrom::Start(plan.resume))?;
                    (None, part.clone(), file, plan.resume, true)
                }
                None => {
                    let target = unique_path(&safe_join(&self.root, &item.path)?);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("创建 {} 失败", parent.display()))?;
                    }
                    let tmp = target.with_extension(format!(
                        "{}part",
                        target
                            .extension()
                            .map(|ext| format!("{}.", ext.to_string_lossy()))
                            .unwrap_or_default()
                    ));
                    let file = File::create(&tmp)
                        .with_context(|| format!("创建 {} 失败", tmp.display()))?;
                    (Some(target), tmp, file, 0, false)
                }
            };
            self.files[index] = Some(RecvFile {
                target,
                tmp,
                file,
                size: item.size,
                written,
                expected_sha: plan.expected_sha.clone(),
                resumable,
                relative: item.path,
            });
        }
        Ok(self.files[index].as_mut().expect("刚写入"))
    }

    /// 写入一个数据块，返回本次是否需要回报进度。
    pub fn write(&mut self, chunk: &FileChunk) -> Result<bool> {
        let index = chunk.item as usize;
        let data = base64::engine::general_purpose::STANDARD
            .decode(chunk.data.as_bytes())
            .context("文件块 base64 解码失败")?;
        let file = self.open_item(index)?;
        if chunk.offset != file.written {
            bail!(
                "文件块偏移不连续（期望 {}，收到 {}）",
                file.written,
                chunk.offset
            );
        }
        if file.written + data.len() as u64 > file.size {
            bail!("文件块超出声明大小（{} 字节）", file.size);
        }
        file.file
            .write_all(&data)
            .with_context(|| format!("写入 {} 失败", file.tmp.display()))?;
        file.written += data.len() as u64;
        self.done += data.len() as u64;
        if self.done - self.last_progress >= FILE_PROGRESS_STEP {
            self.last_progress = self.done;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 收尾：校验字节数与 SHA256、改名、返回每个文件的结果。
    pub fn finish(&mut self) -> (Vec<FileDone>, u64) {
        let mut results = Vec::new();
        // 空文件不会有数据块；整文件已在断点里的，也要打开才能校验和改名。
        let pending: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(index, slot)| {
                slot.is_none() && (self.items[*index].size == 0 || self.plans[*index].resume > 0)
            })
            .map(|(index, _)| index)
            .collect();
        for index in pending {
            if let Err(error) = self.open_item(index) {
                results.push(FileDone {
                    tid: self.tid,
                    path: self.items[index].path.clone(),
                    ok: false,
                    msg: Some(format!("创建空文件失败: {error}")),
                    sha256: None,
                });
            }
        }
        for (index, slot) in self.files.iter_mut().enumerate() {
            let Some(received) = slot.take() else {
                continue;
            };
            let name = self.items[index].path.clone();
            if let Err(error) = received.file.sync_all() {
                results.push(FileDone {
                    tid: self.tid,
                    path: name,
                    ok: false,
                    msg: Some(format!("落盘失败: {error}")),
                    sha256: None,
                });
                continue;
            }
            if received.written != received.size {
                // 字节数不够：保留可续传的前缀，等下次同一文件接着传。
                if !received.resumable {
                    let _ = fs::remove_file(&received.tmp);
                }
                results.push(FileDone {
                    tid: self.tid,
                    path: name,
                    ok: false,
                    msg: Some(format!(
                        "字节数不一致（收到 {}，应为 {}）",
                        received.written, received.size
                    )),
                    sha256: None,
                });
                continue;
            }
            drop(received.file);
            let actual = match file_sha256(&received.tmp) {
                Ok(value) => value,
                Err(error) => {
                    if !received.resumable {
                        let _ = fs::remove_file(&received.tmp);
                    }
                    results.push(FileDone {
                        tid: self.tid,
                        path: name,
                        ok: false,
                        msg: Some(format!("计算校验值失败: {error}")),
                        sha256: None,
                    });
                    continue;
                }
            };
            if let Some(expected) = &received.expected_sha {
                if &actual != expected {
                    // 内容对不上，坏前缀不能留给下次续传。
                    let _ = fs::remove_file(&received.tmp);
                    results.push(FileDone {
                        tid: self.tid,
                        path: name,
                        ok: false,
                        msg: Some(format!("SHA256 校验失败（实际 {actual}）")),
                        sha256: Some(actual),
                    });
                    continue;
                }
            }
            let target = match &received.target {
                Some(target) => target.clone(),
                None => match safe_join(&self.root, &received.relative) {
                    Ok(path) => unique_path(&path),
                    Err(error) => {
                        if !received.resumable {
                            let _ = fs::remove_file(&received.tmp);
                        }
                        results.push(FileDone {
                            tid: self.tid,
                            path: name,
                            ok: false,
                            msg: Some(format!("目标路径不合法: {error}")),
                            sha256: None,
                        });
                        continue;
                    }
                },
            };
            if let Some(parent) = target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::rename(&received.tmp, &target) {
                Ok(()) => results.push(FileDone {
                    tid: self.tid,
                    path: target.display().to_string(),
                    ok: true,
                    msg: None,
                    sha256: received.expected_sha.as_ref().map(|_| actual),
                }),
                Err(error) => results.push(FileDone {
                    tid: self.tid,
                    path: name,
                    ok: false,
                    msg: Some(format!("重命名失败: {error}")),
                    sha256: None,
                }),
            }
        }
        (results, self.done)
    }

    /// 取消：保留可续传的 part，删掉其余临时文件。
    pub fn abort(&mut self) {
        for slot in self.files.iter_mut() {
            if let Some(received) = slot.take() {
                drop(received.file);
                if !received.resumable {
                    let _ = fs::remove_file(&received.tmp);
                }
            }
        }
    }
}

/// 同名文件不覆盖：`a.txt` → `a (1).txt`。
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let ext = path
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..1000 {
        let candidate = parent.join(format!("{stem} ({index}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// 列目录：目录在前、按名称排序；拒绝非 UTF-8 文件名。
pub fn list_dir(path: &str) -> Result<(String, Option<String>, Vec<screenwall_proto::FileEntry>)> {
    let target = if path.trim().is_empty() {
        // 空路径默认打开被控端用户目录，方便从「下载」等常见位置挑文件。
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(receive_root)
    } else {
        PathBuf::from(path)
    };
    if !target.is_absolute() {
        bail!("需要绝对路径");
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&target).with_context(|| format!("读取 {} 失败", target.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64);
        entries.push(screenwall_proto::FileEntry {
            name,
            path: entry.path().display().to_string(),
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified,
        });
    }
    entries.sort_by(|left, right| {
        right
            .dir
            .cmp(&left.dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let parent = target.parent().map(|path| path.display().to_string());
    Ok((target.display().to_string(), parent, entries))
}

/// 打开待发送的文件，返回（文件, 大小）。
pub fn open_for_send(path: &str) -> Result<(File, u64)> {
    let file_path = PathBuf::from(path);
    if !file_path.is_absolute() {
        bail!("需要绝对路径");
    }
    let meta =
        fs::metadata(&file_path).with_context(|| format!("读取 {} 失败", file_path.display()))?;
    if !meta.is_file() {
        bail!("只能下载文件");
    }
    let file =
        File::open(&file_path).with_context(|| format!("打开 {} 失败", file_path.display()))?;
    Ok((file, meta.len()))
}

/// 校验主控端请求的续传偏移：超过文件大小说明对端记错了，必须从 0 重发。
pub fn valid_offset(requested: u64, size: u64) -> u64 {
    if requested <= size {
        requested
    } else {
        0
    }
}

/// 从 `offset` 读一块数据。
pub fn read_chunk(file: &mut File, offset: u64) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0u8; FILE_CHUNK_BYTES];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        let root = std::env::temp_dir();
        assert!(safe_join(&root, "../evil.txt").is_err());
        assert!(safe_join(&root, "C:/Windows/evil.txt").is_err());
        assert!(safe_join(&root, "a/../../b.txt").is_err());
        assert!(safe_join(&root, "folder/ok.txt").is_ok());
    }

    #[test]
    fn unique_path_adds_suffix() {
        let dir = std::env::temp_dir().join(format!("screenwall-unique-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("a.txt");
        fs::write(&first, b"x").unwrap();
        let second = unique_path(&first);
        assert_eq!(second.file_name().unwrap(), "a (1).txt");
        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "screenwall-{tag}-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha_of(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn chunk(tid: u16, offset: u64, data: &[u8]) -> FileChunk {
        FileChunk {
            tid,
            item: 0,
            offset,
            data: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    #[test]
    fn resumes_from_existing_part() {
        let root = temp_root("resume");
        let content = b"hello world, resume me".to_vec();
        let sha = sha_of(&content);
        let part = parts_dir(&root).join(format!("{sha}.part"));
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(&part, &content[..10]).unwrap();

        let item = FileItem {
            path: "sub/resume.bin".into(),
            size: content.len() as u64,
            sha256: sha.clone(),
        };
        let mut session = RecvSession::new_in(root.clone(), 7, vec![item]).unwrap();
        assert_eq!(session.done, 10);
        let resume = session.resume_list();
        assert_eq!(resume.len(), 1);
        assert_eq!(resume[0].offset, 10);

        assert!(!session.write(&chunk(7, 10, &content[10..])).unwrap());
        let (results, bytes) = session.finish();
        assert_eq!(bytes, content.len() as u64);
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "{:?}", results[0]);
        assert_eq!(results[0].sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(fs::read(root.join("sub/resume.bin")).unwrap(), content);
        assert!(!part.exists(), "续传完成后 part 应被改名移走");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_part_whose_hash_does_not_match() {
        let root = temp_root("mismatch");
        let content = b"real content".to_vec();
        let sha = sha_of(&content);
        let part = parts_dir(&root).join(format!("{sha}.part"));
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        // part 的长度已经等于整文件，但内容是错的。
        fs::write(&part, b"fake content").unwrap();

        let item = FileItem {
            path: "bad.bin".into(),
            size: content.len() as u64,
            sha256: sha,
        };
        let mut session = RecvSession::new_in(root.clone(), 9, vec![item]).unwrap();
        assert_eq!(session.resume_list()[0].offset, content.len() as u64);
        let (results, _) = session.finish();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        assert!(results[0].msg.as_deref().unwrap().contains("SHA256"));
        assert!(!root.join("bad.bin").exists());
        assert!(!part.exists(), "校验失败必须删掉坏前缀");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn valid_offset_falls_back_when_beyond_size() {
        assert_eq!(valid_offset(0, 100), 0);
        assert_eq!(valid_offset(40, 100), 40);
        assert_eq!(valid_offset(100, 100), 100);
        // 对端记错了（文件被替换成更小的版本），必须从头发。
        assert_eq!(valid_offset(101, 100), 0);
    }

    #[test]
    fn hash_prefix_covers_exact_prefix() {
        let root = temp_root("prefix");
        let path = root.join("a.bin");
        let content = b"0123456789abcdef".to_vec();
        fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        let hasher = hash_prefix(&mut file, 6).unwrap();
        assert_eq!(hex::encode(hasher.finalize()), sha_of(&content[..6]));

        let mut empty = File::open(&path).unwrap();
        let hasher = hash_prefix(&mut empty, 0).unwrap();
        assert_eq!(hex::encode(hasher.finalize()), sha_of(&[]));

        let mut over = File::open(&path).unwrap();
        assert!(hash_prefix(&mut over, content.len() as u64 + 1).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_stale_parts_only_removes_old_parts() {
        let root = temp_root("cleanup");
        let dir = parts_dir(&root);
        fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.part");
        let other = dir.join("keep.txt");
        fs::write(&fresh, b"x").unwrap();
        fs::write(&other, b"x").unwrap();
        cleanup_stale_parts(&root);
        assert!(fresh.exists(), "新分片不应被清理");
        assert!(other.exists(), "非 .part 文件不应被清理");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn abort_keeps_resumable_part() {
        let root = temp_root("abort");
        let content = b"0123456789abcdef".to_vec();
        let sha = sha_of(&content);
        let item = FileItem {
            path: "keep.bin".into(),
            size: content.len() as u64,
            sha256: sha.clone(),
        };
        let mut session = RecvSession::new_in(root.clone(), 3, vec![item]).unwrap();
        session.write(&chunk(3, 0, &content[..6])).unwrap();
        session.abort();
        let part = parts_dir(&root).join(format!("{sha}.part"));
        assert_eq!(fs::read(&part).unwrap(), &content[..6]);
        let _ = fs::remove_dir_all(&root);
    }
}
