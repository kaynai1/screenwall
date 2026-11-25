//! 连接质量统计：丢帧/丢包、实时帧率与码率、音频空档、重连次数与 RTT。
//!
//! 全部数据来自真实到达的帧头（`FrameHeader.seq` / `ts_us`）与到达时刻，
//! 不做任何估算或模拟。时间来源由调用方传入 `Instant`，方便单元测试。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

/// 帧率/码率的统计窗口。
pub const WINDOW: Duration = Duration::from_secs(5);
/// 单次跳变超过这么多帧就认为换了一路新流（重新订阅/重连），不按丢帧计。
const RESET_THRESHOLD: u32 = 1000;
/// 相邻两包解码时间戳超过 40 ms 视为一次音频空档（Opus 单包 20 ms）。
const AUDIO_GAP_US: u64 = 40_000;

/// 一帧序号的处理结果，主要给单测断言用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOutcome {
    /// 本轮第一帧，只建立基准。
    First,
    /// 紧接上一帧。
    Next,
    /// 中间缺了 `n` 帧。
    Gap(u32),
    /// 重复或迟到的旧帧：不计入收到帧数，也不计丢失。
    Duplicate,
    /// 跳变过大（重新订阅/重连后从 0 开始），只重定基准。
    Reset,
}

/// 按 `seq`（u32 回绕）统计收到与丢失的数量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqTracker {
    last: Option<u32>,
    received: u64,
    lost: u64,
    duplicates: u64,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空基准但保留累计计数：重新订阅后 `seq` 会从头开始。
    pub fn rebase(&mut self) {
        self.last = None;
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// 重复/迟到帧的数量（只用于测试与排查，不进快照）。
    #[cfg(test)]
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// 喂入一个帧序号。
    ///
    /// 用 `wrapping_sub` 同时判断前向/后向距离：
    /// - 前向距离为 0：重复帧；
    /// - 前向距离很小：正常前进，缺的算丢失（含 u32 回绕，例如 0xFFFFFFFF → 0）；
    /// - 后向距离很小：迟到/乱序的旧帧，直接忽略；
    /// - 两个方向都很大：换了一路新流，重定基准。
    pub fn observe(&mut self, seq: u32) -> SeqOutcome {
        let Some(last) = self.last else {
            self.last = Some(seq);
            self.received += 1;
            return SeqOutcome::First;
        };
        let forward = seq.wrapping_sub(last);
        let backward = last.wrapping_sub(seq);
        if forward == 0 {
            self.duplicates += 1;
            return SeqOutcome::Duplicate;
        }
        if forward <= RESET_THRESHOLD {
            self.lost += u64::from(forward - 1);
            self.last = Some(seq);
            self.received += 1;
            if forward == 1 {
                SeqOutcome::Next
            } else {
                SeqOutcome::Gap(forward - 1)
            }
        } else if backward <= RESET_THRESHOLD {
            // 迟到的旧帧：不推进基准，避免把已经数过的帧重复计入。
            self.duplicates += 1;
            SeqOutcome::Duplicate
        } else {
            self.last = Some(seq);
            self.received += 1;
            SeqOutcome::Reset
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoQuality {
    /// 收到的视频帧数（重复/迟到帧不计）。
    pub frames: u64,
    /// 按 seq 缺口推算的丢帧数。
    pub lost: u64,
    /// 最近 5 秒平均帧率。
    pub fps: f64,
    /// 最近 5 秒平均码率（kbps）。
    pub kbps: f64,
    /// 本次会话内相邻两帧的最大到达间隔（毫秒）。
    pub max_gap_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioQuality {
    /// 收到的音频包数。
    pub packets: u64,
    /// 按包序号缺口推算的丢包数。
    pub lost: u64,
    /// 相邻两包解码时间戳之差 > 40 ms 的次数（采集或链路空档）。
    pub gaps: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetQuality {
    /// 本次控制会话内自动重连成功的次数。
    pub reconnects: u32,
    /// 最近一次隧道 Ping 的往返时延；`None` 表示还没测到。
    pub rtt_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub video: VideoQuality,
    pub audio: AudioQuality,
    pub net: NetQuality,
    /// 会话开始时刻（Unix 毫秒），前端用来显示「已持续」。
    pub since: i64,
}

/// 一次控制会话的累计统计。
#[derive(Debug)]
pub struct Tracker {
    since: Instant,
    since_unix_ms: i64,
    video: SeqTracker,
    /// 最近 5 秒内到达的视频帧（到达时刻, 字节数）。
    window: VecDeque<(Instant, u32)>,
    video_stream: Option<u16>,
    last_video_at: Option<Instant>,
    max_gap: Duration,
    audio: SeqTracker,
    audio_last_ts: Option<u64>,
    audio_gaps: u64,
    reconnects: u32,
}

impl Tracker {
    pub fn new(now: Instant, since_unix_ms: i64) -> Self {
        Self {
            since: now,
            since_unix_ms,
            video: SeqTracker::new(),
            window: VecDeque::new(),
            video_stream: None,
            last_video_at: None,
            max_gap: Duration::ZERO,
            audio: SeqTracker::new(),
            audio_last_ts: None,
            audio_gaps: 0,
            reconnects: 0,
        }
    }

    /// 更新会话开始时刻（重连恢复后沿用最初的会话开始时间）。
    pub fn set_since(&mut self, since_unix_ms: i64) {
        self.since_unix_ms = since_unix_ms;
    }

    /// 收到一帧视频。
    pub fn on_video(&mut self, stream: u16, seq: u32, bytes: u32, at: Instant) {
        if self.video_stream != Some(stream) {
            // 换了一路订阅：序号从 0 重新开始，旧的基准作废。
            self.video_stream = Some(stream);
            self.video.rebase();
            self.last_video_at = None;
        }
        if let Some(last) = self.last_video_at {
            let gap = at.saturating_duration_since(last);
            if gap > self.max_gap {
                self.max_gap = gap;
            }
        }
        self.last_video_at = Some(at);
        self.video.observe(seq);
        self.window.push_back((at, bytes));
        self.prune(at);
    }

    /// 收到一个音频包。
    pub fn on_audio(&mut self, seq: u32, ts_us: u64) {
        self.audio.observe(seq);
        if let Some(last) = self.audio_last_ts {
            if ts_us > last && ts_us - last > AUDIO_GAP_US && ts_us - last < 1_000_000 {
                self.audio_gaps += 1;
            }
        }
        if self.audio_last_ts.is_none_or(|last| ts_us > last) {
            self.audio_last_ts = Some(ts_us);
        }
    }

    /// 重连成功：新会话的 seq 从 0 开始，清掉流基准与窗口，累计计数保留。
    pub fn on_reconnect(&mut self) {
        self.reconnects = self.reconnects.saturating_add(1);
        self.video.rebase();
        self.audio.rebase();
        self.video_stream = None;
        self.last_video_at = None;
        self.audio_last_ts = None;
        self.window.clear();
    }

    fn prune(&mut self, now: Instant) {
        while let Some((at, _)) = self.window.front() {
            if now.saturating_duration_since(*at) > WINDOW {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// 生成给前端的快照。`now` 用来裁剪 5 秒窗口。
    pub fn snapshot(&mut self, now: Instant, rtt_ms: u32) -> Snapshot {
        self.prune(now);
        let span = now
            .saturating_duration_since(self.since)
            .min(WINDOW)
            .as_secs_f64();
        // 会话刚开始时窗口很短，用 0.5 秒下限避免一两帧被放大成超高帧率。
        let span = span.max(0.5);
        let frames = self.window.len() as u64;
        let bytes: u64 = self.window.iter().map(|(_, size)| u64::from(*size)).sum();
        Snapshot {
            video: VideoQuality {
                frames: self.video.received(),
                lost: self.video.lost(),
                fps: round1(frames as f64 / span),
                kbps: round1(bytes as f64 * 8.0 / span / 1000.0),
                max_gap_ms: self.max_gap.as_millis().min(u128::from(u64::MAX)) as u64,
            },
            audio: AudioQuality {
                packets: self.audio.received(),
                lost: self.audio.lost(),
                gaps: self.audio_gaps,
            },
            net: NetQuality {
                reconnects: self.reconnects,
                rtt_ms: (rtt_ms > 0).then_some(rtt_ms),
            },
            since: self.since_unix_ms,
        }
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn seq_counts_forward_gaps() {
        let mut seq = SeqTracker::new();
        assert_eq!(seq.observe(0), SeqOutcome::First);
        assert_eq!(seq.observe(1), SeqOutcome::Next);
        assert_eq!(seq.observe(4), SeqOutcome::Gap(2));
        assert_eq!(seq.received(), 3);
        assert_eq!(seq.lost(), 2);
    }

    #[test]
    fn seq_handles_u32_wrap() {
        let mut seq = SeqTracker::new();
        seq.observe(u32::MAX - 1);
        assert_eq!(seq.observe(u32::MAX), SeqOutcome::Next);
        assert_eq!(seq.observe(0), SeqOutcome::Next);
        assert_eq!(seq.observe(1), SeqOutcome::Next);
        assert_eq!(seq.received(), 4);
        assert_eq!(seq.lost(), 0);
        // 回绕处缺一帧也要算出来。
        assert_eq!(seq.observe(3), SeqOutcome::Gap(1));
        assert_eq!(seq.lost(), 1);
    }

    #[test]
    fn seq_ignores_duplicates_and_stale_frames() {
        let mut seq = SeqTracker::new();
        seq.observe(10);
        seq.observe(11);
        assert_eq!(seq.observe(11), SeqOutcome::Duplicate);
        assert_eq!(seq.observe(9), SeqOutcome::Duplicate);
        assert_eq!(seq.observe(12), SeqOutcome::Next);
        assert_eq!(seq.received(), 3);
        assert_eq!(seq.lost(), 0);
        assert_eq!(seq.duplicates(), 2);
    }

    #[test]
    fn seq_rebases_on_large_jump() {
        let mut seq = SeqTracker::new();
        seq.observe(5000);
        assert_eq!(seq.observe(0), SeqOutcome::Reset);
        assert_eq!(seq.observe(1), SeqOutcome::Next);
        assert_eq!(seq.received(), 3);
        assert_eq!(seq.lost(), 0);
    }

    #[test]
    fn video_window_reports_fps_and_bitrate() {
        let base = Instant::now();
        let mut tracker = Tracker::new(base, 1_700_000_000_000);
        // 5 秒内每秒 20 帧、每帧 10 KB。
        for second in 0..5 {
            for frame in 0..20 {
                let ms = second * 1000 + frame * 50;
                tracker.on_video(7, (second * 20 + frame) as u32, 10_240, at(base, ms));
            }
        }
        let snap = tracker.snapshot(at(base, 4_999), 37);
        assert_eq!(snap.video.frames, 100);
        assert_eq!(snap.video.lost, 0);
        assert_eq!(snap.video.fps, 20.0);
        // 100 帧 × 10 KB × 8 bit / 5 秒 ≈ 1638 kbps。
        assert!(
            (snap.video.kbps - 1638.7).abs() < 2.0,
            "{}",
            snap.video.kbps
        );
        assert_eq!(snap.net.rtt_ms, Some(37));
        assert_eq!(snap.since, 1_700_000_000_000);
    }

    #[test]
    fn video_window_drops_old_frames() {
        let base = Instant::now();
        let mut tracker = Tracker::new(base, 0);
        tracker.on_video(1, 0, 1000, base);
        tracker.on_video(1, 1, 1000, at(base, 6_000));
        let snap = tracker.snapshot(at(base, 6_000), 0);
        // 只有最近 5 秒内的一帧进入窗口，但累计帧数仍是 2。
        assert_eq!(snap.video.frames, 2);
        // 窗口固定为 5 秒，只算得进最近这一帧。
        assert_eq!(snap.video.fps, 0.2);
        assert_eq!(snap.video.max_gap_ms, 6_000);
        assert_eq!(snap.net.rtt_ms, None);
    }

    #[test]
    fn video_stream_change_rebases_without_loss() {
        let base = Instant::now();
        let mut tracker = Tracker::new(base, 0);
        tracker.on_video(3, 900, 100, base);
        tracker.on_video(4, 0, 100, at(base, 50));
        tracker.on_video(4, 1, 100, at(base, 100));
        let snap = tracker.snapshot(at(base, 100), 0);
        assert_eq!(snap.video.frames, 3);
        assert_eq!(snap.video.lost, 0);
    }

    #[test]
    fn audio_gaps_and_loss() {
        let mut tracker = Tracker::new(Instant::now(), 0);
        tracker.on_audio(0, 0);
        tracker.on_audio(1, 20_000);
        // 40 ms 整不算空档（严格大于）。
        tracker.on_audio(2, 60_000);
        // 缺一包且间隔 80 ms：丢 1、空档 1。
        tracker.on_audio(4, 140_000);
        let snap = tracker.snapshot(Instant::now(), 0);
        assert_eq!(snap.audio.packets, 4);
        assert_eq!(snap.audio.lost, 1);
        assert_eq!(snap.audio.gaps, 1);
    }

    #[test]
    fn audio_ignores_huge_timestamp_jumps() {
        let mut tracker = Tracker::new(Instant::now(), 0);
        tracker.on_audio(0, 1_000_000);
        // 超过 1 秒的跳变视为重新开始，不计空档。
        tracker.on_audio(1, 9_000_000);
        let snap = tracker.snapshot(Instant::now(), 0);
        assert_eq!(snap.audio.gaps, 0);
    }

    #[test]
    fn set_since_keeps_start_time_across_reconnect() {
        let mut tracker = Tracker::new(Instant::now(), 1_000);
        tracker.set_since(500);
        let snap = tracker.snapshot(Instant::now(), 0);
        assert_eq!(snap.since, 500);
    }

    #[test]
    fn reconnect_resets_stream_baseline_and_counts() {
        let base = Instant::now();
        let mut tracker = Tracker::new(base, 0);
        tracker.on_video(1, 40, 500, base);
        tracker.on_audio(40, 800_000);
        tracker.on_reconnect();
        // 新会话从 0 开始，不应被记成丢 40 帧。
        tracker.on_video(1, 0, 500, at(base, 2_000));
        tracker.on_audio(0, 0);
        let snap = tracker.snapshot(at(base, 2_000), 0);
        assert_eq!(snap.video.lost, 0);
        assert_eq!(snap.video.frames, 2);
        assert_eq!(snap.audio.lost, 0);
        assert_eq!(snap.net.reconnects, 1);
    }
}
