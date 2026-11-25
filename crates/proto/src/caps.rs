//! 能力协商：握手时双方各自声明支持的可选特性。
//!
//! 设计目标是**新旧版本互通**：
//!
//! - 能力列表是纯字符串数组，未知项一律忽略，新版本可以随时加新能力；
//! - `hello` / `welcome` 里的 `caps` 字段带 `#[serde(default)]`，老版本不带这个
//!   字段也能被新版本解析（按 [`Capabilities::legacy`] 处理），新版本发出的
//!   `caps` 也不会让老版本解析失败（serde 默认忽略未知字段）；
//! - 因此协议版本号（[`crate::PROTO_VERSION`]）不需要为新增能力而升。
//!
//! 判断某个可选功能能否使用，一律用 [`Capabilities::supports`]（双方都声明了
//! 才算可用），不要直接看本机常量。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 能力标识
//
// 标识用「点分小写」，一旦发布就不要改名——老版本会把它当作未知项忽略，
// 改名等于把该能力从老版本那里静默关掉。

/// 音频 Opus 编码（`audio.sub.codec = "opus"`）。
pub const AUDIO_OPUS: &str = "audio.opus";
/// 音频 PCM16 编码（`audio.sub.codec = "pcm16"`）。
pub const AUDIO_PCM16: &str = "audio.pcm16";
/// 声音来源可切换到麦克风（`audio.sub.source = "microphone"`）。
pub const AUDIO_SOURCE_MIC: &str = "audio.source.mic";
/// 被控端音量读写（`audio.volume` / `audio.volume.get`）。
pub const AUDIO_VOLUME: &str = "audio.volume";
/// 被控端麦克风回声消除（AEC）。
pub const AUDIO_AEC: &str = "audio.aec";
/// 音频播放缓冲反馈（`audio.stats`），用于码率自适应。
pub const AUDIO_STATS: &str = "audio.stats";
/// 多显示器枚举与切换（`display.list` / `video.sub.display`）。
pub const DISPLAY_MULTI: &str = "display.multi";
/// 整块虚拟桌面拼接（`display = DISPLAY_ALL`）。
///
/// 名字刻意与 [`crate::messages::DISPLAY_ALL`]（`u32::MAX` 哨兵值）区分开，
/// 两者都会被 glob 重导出到 crate 根。
pub const CAP_DISPLAY_ALL: &str = "display.all";
/// 文件上传/下载断点续传。
pub const FILE_RESUME: &str = "file.resume";
/// 整目录递归下载。
pub const FILE_DIRECTORY: &str = "file.directory";
/// 文件 SHA256 端到端校验。
pub const FILE_SHA256: &str = "file.sha256";
/// 文本剪贴板双向同步。
pub const CLIPBOARD_TEXT: &str = "clipboard.text";
/// 图片剪贴板双向同步（`ClipboardSync.image`，base64 PNG）。
pub const CLIPBOARD_IMAGE: &str = "clipboard.image";
/// 远程发送安全注意序列（Ctrl+Alt+Del，`sas` / `sas.ok`）。
///
/// 被控端会优先调用 `sas.dll!SendSAS`，失败时降级为普通按键并回 `sas.ok`。
pub const INPUT_SAS: &str = "input.sas";
/// 被控端会主动上报 UIPI 输入拦截状态（`input.status`）。
pub const INPUT_UIPI: &str = "input.uipi";
/// 远程重启/关机。
pub const POWER: &str = "power";
/// 远程开机（Wake-on-LAN）。
pub const WOL: &str = "wol";
/// 屏幕墙只读会话（`open` 带 `mode: "wall"`）。
pub const WALL: &str = "wall";

/// 协议 v1 首发就具备的能力。
///
/// 对端没有声明 `caps` 时按这个集合处理：那时能连通的对端一定是 v1，
/// 而 v1 的这些功能都已经落地。**新增能力不要加进来**，否则等于对所有
/// 老版本谎报支持。
pub const BASELINE: &[&str] = &[
    AUDIO_OPUS,
    AUDIO_PCM16,
    AUDIO_SOURCE_MIC,
    AUDIO_VOLUME,
    AUDIO_STATS,
    DISPLAY_MULTI,
    CAP_DISPLAY_ALL,
    FILE_RESUME,
    FILE_DIRECTORY,
    FILE_SHA256,
    CLIPBOARD_TEXT,
    POWER,
    WOL,
    WALL,
];

/// 本版本支持的全部能力（在 [`BASELINE`] 之上追加新能力）。
pub const CURRENT: &[&str] = &[
    AUDIO_OPUS,
    AUDIO_PCM16,
    AUDIO_SOURCE_MIC,
    AUDIO_VOLUME,
    AUDIO_AEC,
    AUDIO_STATS,
    DISPLAY_MULTI,
    CAP_DISPLAY_ALL,
    FILE_RESUME,
    FILE_DIRECTORY,
    FILE_SHA256,
    CLIPBOARD_TEXT,
    CLIPBOARD_IMAGE,
    INPUT_SAS,
    INPUT_UIPI,
    POWER,
    WOL,
    WALL,
];

// ---------------------------------------------------------------- 能力集合

/// 一组能力标识。
///
/// 序列化后就是一个字符串数组（`#[serde(transparent)]`），
/// 所以 `hello` / `welcome` 里直接写成 `"caps": ["audio.opus", …]`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities {
    items: Vec<String>,
}

impl Capabilities {
    /// 从任意字符串集合构造：去重、排序，保证报文稳定可比。
    pub fn new<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut items: Vec<String> = items.into_iter().map(Into::into).collect();
        items.sort_unstable();
        items.dedup();
        Self { items }
    }

    /// 空集合：什么都没声明（等价于「什么都不可用」）。
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }

    /// 本机完整能力集。
    pub fn current() -> Self {
        Self::new(CURRENT.iter().copied())
    }

    /// 对端未声明 `caps` 时假定的能力集。
    pub fn legacy() -> Self {
        Self::new(BASELINE.iter().copied())
    }

    /// 是否声明了某个能力。
    pub fn contains(&self, cap: &str) -> bool {
        self.items.iter().any(|item| item == cap)
    }

    /// 是否为空（对端一条都没声明）。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 已声明的能力，按字典序。
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(String::as_str)
    }

    /// 数量。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 双方能力交集：这次会话里真正可用的能力。
    pub fn negotiate(&self, peer: &Capabilities) -> Capabilities {
        Self::new(self.items.iter().filter(|cap| peer.contains(cap)).cloned())
    }

    /// 双方是否都支持某个能力。
    ///
    /// 这是判断可选功能能否启用的唯一入口。对端没声明 `caps` 时调用方应先用
    /// [`Capabilities::legacy`] 补位（见 [`Capabilities::effective`]）。
    pub fn supports(&self, peer: &Capabilities, cap: &str) -> bool {
        self.contains(cap) && peer.contains(cap)
    }

    /// 把「对端可能没声明」的两种情况归一：空集合视为老版本。
    ///
    /// 老版本没有 `caps` 字段，解析出来就是空集合；空集合不可能是
    /// 「一个能力都不支持」的新版本（新版本至少会声明 `CURRENT`）。
    pub fn effective(self) -> Capabilities {
        if self.is_empty() {
            Capabilities::legacy()
        } else {
            self
        }
    }

    /// 与对端协商出本次会话可用的能力。
    pub fn with_peer(peer: Capabilities) -> Capabilities {
        Capabilities::current().negotiate(&peer.effective())
    }
}

impl From<&[&str]> for Capabilities {
    fn from(items: &[&str]) -> Self {
        Self::new(items.iter().copied())
    }
}

impl From<Vec<String>> for Capabilities {
    fn from(items: Vec<String>) -> Self {
        Self::new(items)
    }
}

impl std::fmt::Display for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.items.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_dedupes_and_sorts() {
        let caps = Capabilities::new(["b", "a", "b"]);
        assert_eq!(caps.iter().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(caps.len(), 2);
        assert!(!caps.is_empty());
    }

    #[test]
    fn serializes_as_plain_array() {
        let caps = Capabilities::new([AUDIO_OPUS, WOL]);
        let text = serde_json::to_string(&caps).unwrap();
        assert_eq!(text, r#"["audio.opus","wol"]"#);
        let back: Capabilities = serde_json::from_str(&text).unwrap();
        assert_eq!(back, caps);
    }

    #[test]
    fn unknown_capabilities_are_kept_but_ignored() {
        // 新版本对端可能带来本机不认识的能力：解析不能失败，也不影响判断。
        let caps: Capabilities = serde_json::from_str(r#"["audio.opus","future.thing"]"#).unwrap();
        assert!(caps.contains("future.thing"));
        assert!(caps.contains(AUDIO_OPUS));
        assert!(!caps.contains(AUDIO_AEC));
    }

    #[test]
    fn missing_field_falls_back_to_legacy() {
        // 老版本报文里没有 caps：默认空集合，经 effective() 归一成 baseline。
        let caps = Capabilities::default();
        assert!(caps.is_empty());
        let effective = caps.effective();
        assert_eq!(effective, Capabilities::legacy());
        assert!(effective.contains(FILE_RESUME));
        // 新能力不能被老版本「谎报」支持。
        assert!(!effective.contains(AUDIO_AEC));
    }

    #[test]
    fn negotiate_returns_intersection() {
        let mine = Capabilities::current();
        let peer = Capabilities::new([AUDIO_OPUS, WOL, "future.thing"]);
        let agreed = mine.negotiate(&peer);
        assert!(agreed.contains(AUDIO_OPUS));
        assert!(agreed.contains(WOL));
        assert!(
            !agreed.contains("future.thing"),
            "本机不支持的不能算协商成功"
        );
        assert!(!agreed.contains(AUDIO_AEC), "对端没声明 AEC");
        assert_eq!(agreed.len(), 2);
    }

    #[test]
    fn supports_requires_both_sides() {
        let mine = Capabilities::current();
        let old_peer = Capabilities::legacy();
        assert!(mine.supports(&old_peer, AUDIO_OPUS));
        assert!(mine.supports(&old_peer, AUDIO_VOLUME));
        assert!(!mine.supports(&old_peer, AUDIO_AEC), "老版本没有 AEC");
        assert!(mine.supports(&Capabilities::current(), AUDIO_AEC));
    }

    #[test]
    fn with_peer_normalizes_legacy_peers() {
        // 对端没声明 caps：按 baseline 协商，不能因此丢掉 v1 已有能力。
        let agreed = Capabilities::with_peer(Capabilities::empty());
        assert!(agreed.contains(FILE_DIRECTORY));
        assert!(agreed.contains(AUDIO_VOLUME));
        assert!(!agreed.contains(AUDIO_AEC));
    }

    #[test]
    fn current_is_superset_of_baseline() {
        for cap in BASELINE {
            assert!(
                CURRENT.contains(cap),
                "CURRENT 必须包含 baseline 能力 {cap}，否则新版本会把自己关小"
            );
        }
        // 且确实有新增，否则这个机制没意义。
        assert!(CURRENT.len() > BASELINE.len());
        let current = Capabilities::current();
        assert!(current.contains(AUDIO_AEC));
    }

    #[test]
    fn sas_capabilities_are_not_baseline() {
        // 新能力不能进基线：老版本被控端不认识 `sas` / `input.status`。
        let baseline = Capabilities::legacy();
        assert!(!baseline.contains(INPUT_SAS));
        assert!(!baseline.contains(INPUT_UIPI));
        let current = Capabilities::current();
        assert!(current.contains(INPUT_SAS));
        assert!(current.contains(INPUT_UIPI));
    }

    #[test]
    fn clipboard_image_is_not_baseline() {
        // 图片剪贴板是后加的能力：老版本被控端只认文本，不能谎报支持。
        assert!(!Capabilities::legacy().contains(CLIPBOARD_IMAGE));
        assert!(Capabilities::current().contains(CLIPBOARD_IMAGE));
        assert!(Capabilities::current().contains(CLIPBOARD_TEXT));
    }

    #[test]
    fn welcome_contract_is_intersection_not_current() {
        // 握手回给主控端的必须是交集：
        // 老主控端没声明 caps → 交集 = 基线，不能出现本机才有的 audio.aec；
        // 新主控端声明全量 → 交集 = 本机全量，行为不变。
        let old_peer = Capabilities::with_peer(Capabilities::default());
        assert!(!old_peer.contains(AUDIO_AEC), "老主控端不该被告知有 AEC");
        assert!(old_peer.contains(AUDIO_VOLUME));
        assert_ne!(old_peer, Capabilities::current());

        let new_peer = Capabilities::with_peer(Capabilities::current());
        assert_eq!(new_peer, Capabilities::current());

        // 对端只声明了一部分：只回双方都有的，未知项（本机不认识的）不算数。
        let partial = Capabilities::with_peer(Capabilities::new([AUDIO_OPUS, "future.thing"]));
        assert_eq!(partial.iter().collect::<Vec<_>>(), vec![AUDIO_OPUS]);
    }

    #[test]
    fn display_is_comma_separated() {
        let caps = Capabilities::new([WOL, AUDIO_OPUS]);
        assert_eq!(caps.to_string(), "audio.opus,wol");
        assert_eq!(Capabilities::empty().to_string(), "");
    }
}
