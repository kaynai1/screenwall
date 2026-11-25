//! 控制通道消息。
//!
//! 所有消息以 JSON 文本帧传输，`t` 字段区分类型。
//! 字段命名与 `docs/protocol.md` 一一对应，多词字段使用 camelCase。

use serde::{Deserialize, Serialize};

use crate::caps::Capabilities;

/// 单个文件数据块的原始字节上限（base64 编码前）。
pub const FILE_CHUNK_BYTES: usize = 256 * 1024;
/// 接收方每收满这么多字节回一次 `file.progress`（同时充当流控确认）。
pub const FILE_PROGRESS_STEP: u64 = 2 * 1024 * 1024;
/// 发送端允许的最大在途未确认字节；超过就等 `file.progress`。
pub const FILE_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// 顶层消息。每个变体对应一种 `t` 值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Message {
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "welcome")]
    Welcome(Welcome),
    #[serde(rename = "status")]
    Status(Status),
    #[serde(rename = "ping")]
    Ping(Ping),
    #[serde(rename = "pong")]
    Pong(Pong),
    #[serde(rename = "display.list")]
    DisplayList(DisplayList),
    #[serde(rename = "video.sub")]
    VideoSub(VideoSub),
    #[serde(rename = "video.sub.ok")]
    VideoSubOk(VideoSubOk),
    #[serde(rename = "video.unsub")]
    VideoUnsub(VideoUnsub),
    #[serde(rename = "video.keyframe")]
    VideoKeyframe(VideoKeyframe),
    #[serde(rename = "audio.sub")]
    AudioSub(AudioSub),
    #[serde(rename = "audio.sub.ok")]
    AudioSubOk(AudioSubOk),
    #[serde(rename = "audio.unsub")]
    AudioUnsub(AudioUnsub),
    #[serde(rename = "audio.stats")]
    AudioStats(AudioStats),
    #[serde(rename = "audio.volume")]
    AudioVolume(AudioVolume),
    #[serde(rename = "audio.volume.ok")]
    AudioVolumeOk(AudioVolumeOk),
    #[serde(rename = "audio.volume.get")]
    AudioVolumeGet(AudioVolumeGet),
    #[serde(rename = "input")]
    Input(InputBatch),
    #[serde(rename = "sas")]
    SecureAttention(SecureAttention),
    #[serde(rename = "sas.ok")]
    SecureAttentionOk(SecureAttentionOk),
    #[serde(rename = "input.status")]
    InputStatus(InputStatus),
    #[serde(rename = "file.offer")]
    FileOffer(FileOffer),
    #[serde(rename = "file.chunk")]
    FileChunk(FileChunk),
    #[serde(rename = "file.list")]
    FileListReq(FileListReq),
    #[serde(rename = "file.list.ok")]
    FileListOk(FileListOk),
    #[serde(rename = "file.get")]
    FileGet(FileGet),
    #[serde(rename = "file.accept")]
    FileAccept(FileAccept),
    #[serde(rename = "file.deny")]
    FileDeny(FileDeny),
    #[serde(rename = "file.progress")]
    FileProgress(FileProgress),
    #[serde(rename = "file.done")]
    FileDone(FileDone),
    #[serde(rename = "file.complete")]
    FileComplete(FileComplete),
    #[serde(rename = "file.cancel")]
    FileCancel(FileCancel),
    #[serde(rename = "clipboard")]
    Clipboard(ClipboardSync),
    #[serde(rename = "power")]
    Power(PowerRequest),
    #[serde(rename = "power.ok")]
    PowerOk(PowerOk),
    #[serde(rename = "wol.request")]
    WolRequest(WolRequest),
    #[serde(rename = "wol.ok")]
    WolOk(WolOk),
    #[serde(rename = "error")]
    Error(ErrorMsg),
}

impl Message {
    /// 解析文本帧。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// 序列化为文本帧内容。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("消息序列化不应失败")
    }
}

// ---------------------------------------------------------------- 握手

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub token: String,
    pub client: PeerInfo,
    /// 主控端支持的可选能力。老版本不带这个字段，解析为空集合，
    /// 用 [`Capabilities::effective`] 归一成 v1 基线。
    #[serde(default)]
    pub caps: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub proto: u16,
    pub agent: PeerInfo,
    pub limits: Limits,
    /// 被控端支持的可选能力；缺省同 [`Hello::caps`]。
    #[serde(default)]
    pub caps: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_streams: u16,
    pub max_chunk: u32,
    pub allow_control: bool,
}

impl Limits {
    pub fn defaults() -> Self {
        Self {
            max_streams: 4,
            max_chunk: 1024 * 1024,
            allow_control: true,
        }
    }
}

// ---------------------------------------------------------------- 状态

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub screen: ScreenInfo,
    pub subs: Vec<SubInfo>,
    pub stats: Stats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub w: u32,
    pub h: u32,
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubInfo {
    pub id: u16,
    pub stream: StreamKind,
    pub fps: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// 整机 CPU 占用百分比（0–100）。
    pub cpu: f32,
    /// 本会话出流码率（kbps）。
    pub kbps: u32,
    /// 整机内存占用百分比（0–100）。旧被控端不带这个字段，按 0 处理。
    #[serde(default)]
    pub mem: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    pub ts: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {
    pub ts: u64,
}

// ---------------------------------------------------------------- 显示器

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayList {
    pub displays: Vec<DisplayInfo>,
}

/// `VideoSub.display` 取这个值表示整块虚拟桌面（所有显示器拼接）。
pub const DISPLAY_ALL: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub index: u32,
    pub w: u32,
    pub h: u32,
    pub primary: bool,
    /// 显示器在虚拟桌面中的左上角坐标（可为负）。
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
}

// ---------------------------------------------------------------- 视频

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Wall,
    Focus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    Mjpeg,
    H264,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSub {
    pub stream: StreamKind,
    #[serde(default)]
    pub display: u32,
    pub fps: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    pub codec: VideoCodec,
    #[serde(default)]
    pub cursor: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSubOk {
    pub id: u16,
    pub stream: StreamKind,
    pub actual: ActualVideo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActualVideo {
    pub codec: VideoCodec,
    pub w: u32,
    pub h: u32,
    pub fps: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoUnsub {
    pub id: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoKeyframe {
    pub id: u16,
}

// ---------------------------------------------------------------- 音频

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    /// 交错小端 16 位有符号 PCM；采样率与声道数由 `audio.sub.ok` 告知。
    Pcm16,
    /// 每个二进制帧是一个独立 Opus 包（20 ms）；固定 48 kHz，声道数见 `audio.sub.ok`。
    Opus,
}

/// 声音来源：系统回环（扬声器）或麦克风。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    /// 默认扬声器的回环采集（听到对方电脑正在播放的声音）。
    #[default]
    Loopback,
    /// 默认麦克风。
    Microphone,
}

/// 请求一路声音（WASAPI 回环或麦克风）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSub {
    /// 主控端希望的编码；不填由被控端选择（优先 Opus）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<AudioCodec>,
    /// 声音来源；不填按系统回环处理（兼容旧客户端）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AudioSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSubOk {
    pub id: u16,
    pub actual: ActualAudio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActualAudio {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// Opus 的初始目标码率（bit/s）；PCM 为 0。
    #[serde(default)]
    pub bitrate: u32,
    /// 实际来源；旧被控端不返回时按系统回环处理。
    #[serde(default)]
    pub source: AudioSource,
}

/// 主控端调整被控端音量 / 静音。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AudioVolume {
    /// 0.0–1.0；不填表示只改静音。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    /// 不填表示只改音量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioVolumeOk {
    pub level: f32,
    pub muted: bool,
}

/// 读取被控端当前音量 / 静音状态。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AudioVolumeGet {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioUnsub {
    pub id: u16,
}

/// 主控端周期性上报播放缓冲，供被控端调整 Opus 码率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStats {
    pub id: u16,
    pub buffered_ms: u32,
}

// ---------------------------------------------------------------- 输入

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputBatch {
    pub events: Vec<InputEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "camelCase")]
pub enum InputEvent {
    Move {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs: Option<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rel: Option<[i32; 2]>,
    },
    Button {
        button: MouseButton,
        down: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs: Option<[f32; 2]>,
    },
    Wheel {
        delta: [i32; 2],
    },
    Key {
        scan: u16,
        down: bool,
        #[serde(default)]
        ext: bool,
    },
    Text {
        text: String,
    },
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// 请求被控端生成安全注意序列（Ctrl+Alt+Del）。
///
/// 这是可选能力，双方都声明 `input.sas` 时才发送；老被控端会回
/// `4109 UNKNOWN_MESSAGE`，主控端按「该功能不可用」处理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecureAttention {
    /// 传给 `sas.dll!SendSAS` 的 `AsUser`；缺省 `false`（以系统身份请求）。
    #[serde(default)]
    pub as_user: bool,
}

/// SAS 实际使用的发送方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SasMethod {
    /// 通过 `sas.dll!SendSAS` 生成真正的安全注意序列。
    Sas,
    /// 降级为普通 `Ctrl+Alt+Del` 按键，只对普通应用有效，无法进入安全桌面。
    Keys,
}

/// 对 [`SecureAttention`] 的处理结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecureAttentionOk {
    pub method: SasMethod,
    /// 降级原因或失败说明；真正调用 `SendSAS` 时省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 输入注入状态，由被控端在 UIPI 拦截状态变化时主动上报。
///
/// 只在双方都声明 `input.uipi` 时发送；老主控端解析失败会静默丢弃该帧。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputStatus {
    /// 最近一次注入是否被 UIPI 拦截（前台窗口的完整性级别高于被控端进程）。
    pub uipi: bool,
    /// 被控端给出的补充说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ---------------------------------------------------------------- 文件

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOffer {
    pub tid: u16,
    pub items: Vec<FileItem>,
    pub total_size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileItem {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// 一个文件数据块。`data` 是 base64，`offset` 为该块在文件内的偏移。
///
/// 两个方向共用：上传时主控发、被控写；下载时被控发、主控写。
/// 接收方每收满 `PROGRESS_STEP` 字节回一条 `file.progress` 作为流控确认。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileChunk {
    pub tid: u16,
    pub item: u16,
    pub offset: u64,
    pub data: String,
}

/// 浏览被控端目录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileListReq {
    pub tid: u16,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileListOk {
    pub tid: u16,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

/// 请求下载被控端的某个文件；被控端回 `file.offer`（单文件）后开始推送数据块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileGet {
    pub tid: u16,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileAccept {
    pub tid: u16,
    pub root: String,
    /// 上传方向：被控端已经持有的前缀字节数；主控端从这些偏移继续发送。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume: Vec<FileResume>,
    /// 下载方向：主控端已经持有的前缀字节数，被控端从这里 seek 后发送。
    ///
    /// 0 表示从头开始，旧版被控端会忽略该字段；旧版主控端不发送时按 0 处理。
    /// 被控端校验偏移不超过文件大小，超出则回退到 0。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// 单个文件的续传位置，`item` 是 `file.offer` 里的下标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileResume {
    pub item: u16,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDeny {
    pub tid: u16,
    pub msg: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileProgress {
    pub tid: u16,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDone {
    pub tid: u16,
    pub path: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// 发送方算出的整文件 SHA256（小写十六进制）；接收方用它做端到端校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileComplete {
    pub tid: u16,
    pub files: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileCancel {
    pub tid: u16,
    pub msg: String,
}

// ---------------------------------------------------------------- 剪贴板

/// 剪贴板同步：两端各自监听本机变化后发给对端。
///
/// `text` 与 `image` 二选一，同一条消息里不会同时有两个：调用方在本地
/// 按「有文本优先发文本、否则发图片」决定发哪个（Excel/Word 复制时会同时
/// 放位图，优先发图片会让用户复制到的表格变成一张图）。
///
/// `image` 是 PNG 编码后的字节再做 base64，老版本收到后不认识这个字段会
/// 直接忽略，所以向后兼容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardSync {
    #[serde(default)]
    pub text: String,
    /// base64(PNG)。没有图片时为 `None`，不写进 JSON。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

impl ClipboardSync {
    /// 文本剪贴板。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image: None,
        }
    }

    /// 图片剪贴板；`image` 是 base64(PNG)。
    pub fn image(image: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            image: Some(image.into()),
        }
    }
}

// ---------------------------------------------------------------- 电源操作

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    Restart,
    Shutdown,
}

/// 让被控端重启或关机。只在已授权的控制会话里允许。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerRequest {
    pub action: PowerAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerOk {
    pub action: PowerAction,
}

// ---------------------------------------------------------------- 远程开机

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WolRequest {
    pub mac: String,
    pub broadcast: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WolOk {
    pub mac: String,
}

// ---------------------------------------------------------------- 错误

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub code: u16,
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tid: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTO_VERSION;

    fn peer(name: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            version: "0.1.0".into(),
        }
    }

    #[test]
    fn hello_roundtrip_uses_tag() {
        let msg = Message::Hello(Hello {
            proto: PROTO_VERSION,
            token: "abc".into(),
            client: peer("DESKTOP-CTRL"),
            caps: Capabilities::current(),
        });
        let text = msg.to_json();
        assert!(text.contains("\"t\":\"hello\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn hello_without_caps_parses_as_legacy() {
        // 老版本主控端不带 caps 字段。
        let text = r#"{"t":"hello","proto":1,"token":"x","client":{"name":"A","version":"0.0.9"}}"#;
        match Message::parse(text).unwrap() {
            Message::Hello(hello) => {
                assert!(hello.caps.is_empty());
                assert!(hello.caps.effective().contains(crate::caps::FILE_RESUME));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hello_with_caps_roundtrip() {
        let msg = Message::Hello(Hello {
            proto: PROTO_VERSION,
            token: String::new(),
            client: peer("DESKTOP-CTRL"),
            caps: Capabilities::new([crate::caps::AUDIO_OPUS, crate::caps::WOL]),
        });
        let text = msg.to_json();
        assert!(text.contains(r#""caps":["audio.opus","wol"]"#), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn file_accept_roundtrip_with_resume() {
        let msg = Message::FileAccept(FileAccept {
            tid: 3,
            root: "C:/Downloads".into(),
            resume: vec![FileResume {
                item: 0,
                offset: 1024,
            }],
            offset: 0,
        });
        let text = msg.to_json();
        assert!(
            text.contains("\"resume\":[{\"item\":0,\"offset\":1024}]"),
            "{text}"
        );
        assert_eq!(Message::parse(&text).unwrap(), msg);

        // 旧版没带 resume 字段时按没有断点处理。
        let plain = r#"{"t":"file.accept","tid":1,"root":"C:/x"}"#;
        match Message::parse(plain).unwrap() {
            Message::FileAccept(accept) => assert!(accept.resume.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn file_accept_download_offset_roundtrip() {
        let msg = Message::FileAccept(FileAccept {
            tid: 4,
            root: "C:/Downloads".into(),
            resume: Vec::new(),
            offset: 4096,
        });
        let text = msg.to_json();
        assert!(text.contains("\"offset\":4096"), "{text}");
        assert!(
            !text.contains("resume"),
            "下载方向的空 resume 不应出现: {text}"
        );
        assert_eq!(Message::parse(&text).unwrap(), msg);

        // 旧版主控端不带 offset：按 0 处理。
        let plain = r#"{"t":"file.accept","tid":2,"root":"C:/x"}"#;
        match Message::parse(plain).unwrap() {
            Message::FileAccept(accept) => assert_eq!(accept.offset, 0),
            other => panic!("unexpected {other:?}"),
        }

        // offset 为 0 时省略，保持与旧版一致的报文。
        let zero = Message::FileAccept(FileAccept {
            tid: 5,
            root: "C:/x".into(),
            resume: Vec::new(),
            offset: 0,
        })
        .to_json();
        assert!(!zero.contains("offset"), "{zero}");
        assert_eq!(
            Message::parse(&zero).unwrap(),
            Message::FileAccept(FileAccept {
                tid: 5,
                root: "C:/x".into(),
                resume: Vec::new(),
                offset: 0,
            })
        );
    }

    #[test]
    fn file_accept_accepts_both_directions() {
        // 上传与下载的字段同时出现也能解析，方向由上下文决定。
        let text = r#"{"t":"file.accept","tid":1,"root":"r","resume":[{"item":0,"offset":10}],"offset":20}"#;
        match Message::parse(text).unwrap() {
            Message::FileAccept(accept) => {
                assert_eq!(accept.resume.len(), 1);
                assert_eq!(accept.offset, 20);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn file_done_without_sha_is_backward_compatible() {
        let text = r#"{"t":"file.done","tid":1,"path":"a","ok":true}"#;
        match Message::parse(text).unwrap() {
            Message::FileDone(done) => assert_eq!(done.sha256, None),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn welcome_roundtrip_camel_case() {
        let msg = Message::Welcome(Welcome {
            proto: PROTO_VERSION,
            agent: peer("DESKTOP-A"),
            limits: Limits::defaults(),
            caps: Capabilities::current(),
        });
        let text = msg.to_json();
        assert!(text.contains("\"maxStreams\":4"), "{text}");
        assert!(text.contains("\"allowControl\":true"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn welcome_without_caps_parses_as_legacy() {
        let text = r#"{"t":"welcome","proto":1,"agent":{"name":"A","version":"0.0.9"},"limits":{"maxStreams":4,"maxChunk":1048576,"allowControl":true}}"#;
        match Message::parse(text).unwrap() {
            Message::Welcome(welcome) => {
                assert!(welcome.caps.is_empty());
                let agreed = welcome.caps.effective().negotiate(&Capabilities::current());
                assert!(!agreed.contains(crate::caps::AUDIO_AEC));
                assert!(agreed.contains(crate::caps::AUDIO_VOLUME));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn audio_messages_roundtrip() {
        let sub = Message::AudioSub(AudioSub {
            codec: Some(AudioCodec::Opus),
            source: Some(AudioSource::Microphone),
        });
        let text = sub.to_json();
        assert!(text.contains("\"t\":\"audio.sub\""), "{text}");
        assert!(text.contains("\"codec\":\"opus\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), sub);

        let ok = Message::AudioSubOk(AudioSubOk {
            id: 3,
            actual: ActualAudio {
                codec: AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 2,
                bitrate: 64_000,
                source: AudioSource::Loopback,
            },
        });
        let text = ok.to_json();
        assert!(text.contains("\"sampleRate\":48000"), "{text}");
        assert!(text.contains("\"codec\":\"opus\""), "{text}");
        assert!(text.contains("\"bitrate\":64000"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), ok);

        // 旧版没有 codec / source 字段也能解析，按系统回环处理。
        match Message::parse(r#"{"t":"audio.sub"}"#).unwrap() {
            Message::AudioSub(sub) => {
                assert_eq!(sub.codec, None);
                assert_eq!(sub.source, None);
            }
            other => panic!("unexpected {other:?}"),
        }

        let vol = Message::AudioVolume(AudioVolume {
            level: Some(0.4),
            muted: Some(false),
        });
        let text = vol.to_json();
        assert!(text.contains("\"t\":\"audio.volume\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), vol);

        // 只改静音时 level 省略。
        let mute_only = Message::AudioVolume(AudioVolume {
            level: None,
            muted: Some(true),
        });
        let text = mute_only.to_json();
        assert!(!text.contains("level"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), mute_only);

        let vol_ok = Message::AudioVolumeOk(AudioVolumeOk {
            level: 0.4,
            muted: false,
        });
        let text = vol_ok.to_json();
        assert!(text.contains("\"t\":\"audio.volume.ok\""), "{text}");
        assert!(text.contains("\"level\":0.4"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), vol_ok);

        let unsub = Message::AudioUnsub(AudioUnsub { id: 3 });
        let text = unsub.to_json();
        assert!(text.contains("\"t\":\"audio.unsub\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), unsub);

        let stats = Message::AudioStats(AudioStats {
            id: 3,
            buffered_ms: 120,
        });
        let text = stats.to_json();
        assert!(text.contains("\"t\":\"audio.stats\""), "{text}");
        assert!(text.contains("\"bufferedMs\":120"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), stats);
    }

    #[test]
    fn power_messages_roundtrip() {
        let request = Message::Power(PowerRequest {
            action: PowerAction::Restart,
        });
        let text = request.to_json();
        assert!(text.contains("\"t\":\"power\""), "{text}");
        assert!(text.contains("\"action\":\"restart\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), request);

        let ok = Message::PowerOk(PowerOk {
            action: PowerAction::Shutdown,
        });
        let text = ok.to_json();
        assert!(text.contains("\"t\":\"power.ok\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), ok);
    }

    #[test]
    fn status_roundtrip() {
        let msg = Message::Status(Status {
            screen: ScreenInfo {
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
            subs: vec![SubInfo {
                id: 7,
                stream: StreamKind::Wall,
                fps: 2,
            }],
            stats: Stats {
                cpu: 3.2,
                kbps: 400,
                mem: 42.5,
            },
        });
        let text = msg.to_json();
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn input_batch_roundtrip() {
        let msg = Message::Input(InputBatch {
            events: vec![
                InputEvent::Move {
                    abs: Some([0.5, 0.25]),
                    rel: None,
                },
                InputEvent::Button {
                    button: MouseButton::Left,
                    down: true,
                    abs: Some([0.5, 0.25]),
                },
                InputEvent::Key {
                    scan: 30,
                    down: true,
                    ext: false,
                },
                InputEvent::Text {
                    text: "中文".into(),
                },
                InputEvent::ReleaseAll,
            ],
        });
        let text = msg.to_json();
        assert!(text.contains("\"k\":\"releaseAll\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn secure_attention_roundtrip_and_defaults() {
        let request = Message::SecureAttention(SecureAttention { as_user: false });
        let text = request.to_json();
        assert!(text.contains("\"t\":\"sas\""), "{text}");
        assert!(text.contains("\"asUser\":false"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), request);

        // 老版本/省略 asUser 时按系统身份处理，不能解析失败。
        match Message::parse(r#"{"t":"sas"}"#).unwrap() {
            Message::SecureAttention(request) => assert!(!request.as_user),
            other => panic!("unexpected {other:?}"),
        }

        let ok = Message::SecureAttentionOk(SecureAttentionOk {
            method: SasMethod::Keys,
            detail: Some("系统策略未允许".into()),
        });
        let text = ok.to_json();
        assert!(text.contains("\"t\":\"sas.ok\""), "{text}");
        assert!(text.contains("\"method\":\"keys\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), ok);

        // 真正调用 SendSAS 成功时 detail 省略。
        let sas = Message::SecureAttentionOk(SecureAttentionOk {
            method: SasMethod::Sas,
            detail: None,
        });
        let text = sas.to_json();
        assert!(!text.contains("detail"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), sas);
    }

    #[test]
    fn input_status_roundtrip() {
        let blocked = Message::InputStatus(InputStatus {
            uipi: true,
            detail: Some("前台窗口权限更高".into()),
        });
        let text = blocked.to_json();
        assert!(text.contains("\"t\":\"input.status\""), "{text}");
        assert!(text.contains("\"uipi\":true"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), blocked);

        let cleared = Message::InputStatus(InputStatus {
            uipi: false,
            detail: None,
        });
        let text = cleared.to_json();
        assert!(!text.contains("detail"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), cleared);
    }

    #[test]
    fn file_offer_roundtrip() {
        let msg = Message::FileOffer(FileOffer {
            tid: 12,
            items: vec![FileItem {
                path: "logs/a.txt".into(),
                size: 1234,
                sha256: "ff".into(),
            }],
            total_size: 1234,
        });
        let text = msg.to_json();
        assert!(text.contains("\"totalSize\":1234"), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn clipboard_roundtrip() {
        let msg = Message::Clipboard(ClipboardSync::text("复制的文字"));
        let text = msg.to_json();
        assert!(text.contains("\"t\":\"clipboard\""), "{text}");
        // 文本消息不应该带 image 字段。
        assert!(!text.contains("\"image\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn clipboard_image_roundtrip() {
        let msg = Message::Clipboard(ClipboardSync::image("iVBORw0KGgo="));
        let text = msg.to_json();
        assert!(text.contains("\"image\":\"iVBORw0KGgo=\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn clipboard_accepts_old_shape_without_image() {
        // 老版本只发 `{"text":"..."}`，新版本必须还能解析。
        let text = r#"{"t":"clipboard","text":"hello"}"#;
        let parsed = Message::parse(text).expect("老格式应该能解析");
        assert_eq!(parsed, Message::Clipboard(ClipboardSync::text("hello")));
    }

    #[test]
    fn error_roundtrip() {
        let msg = Message::Error(ErrorMsg {
            code: crate::codes::AUTH_FAILED,
            msg: "auth failed".into(),
            tid: None,
            id: None,
        });
        let text = msg.to_json();
        assert!(text.contains("\"t\":\"error\""), "{text}");
        assert_eq!(Message::parse(&text).unwrap(), msg);
    }

    #[test]
    fn unknown_tag_fails() {
        assert!(Message::parse(r#"{"t":"nope"}"#).is_err());
    }
}
