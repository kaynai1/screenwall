//! 服务器运行指标：进程内原子计数、错误码分布与 5 分钟错误率。
//!
//! 所有数据都在真实处理路径上累加，不做采样或估算；进程重启后归零。
//! `GET /api/v1/metrics` 读取这里的快照，在线设备数、活跃会话数、账号数
//! 由调用方从内存表与数据库现算后传入。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

/// 错误率滑动窗口：30 个 10 秒桶，覆盖最近 5 分钟。
const ERROR_BUCKETS: usize = 30;
const ERROR_BUCKET_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, Default)]
struct ErrorBucket {
    /// 该桶对应的 10 秒时间片编号（自进程启动起算）。
    epoch: u64,
    count: u64,
}

/// 服务器级指标计数器，可被多个连接/请求共享。
#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    errors: AtomicU64,
    error_codes: Mutex<BTreeMap<u16, u64>>,
    error_buckets: Mutex<[ErrorBucket; ERROR_BUCKETS]>,
    sessions_total: AtomicU64,
    sessions_peak: AtomicU64,
    handshakes_ok: AtomicU64,
    handshakes_failed: AtomicU64,
    dropped_frames: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            error_codes: Mutex::new(BTreeMap::new()),
            error_buckets: Mutex::new([ErrorBucket::default(); ERROR_BUCKETS]),
            sessions_total: AtomicU64::new(0),
            sessions_peak: AtomicU64::new(0),
            handshakes_ok: AtomicU64::new(0),
            handshakes_failed: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    /// 主控端 → 被控端方向的中转负载字节数。
    pub fn add_up(&self, bytes: u64) {
        self.bytes_up.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 被控端 → 主控端方向的中转负载字节数。
    pub fn add_down(&self, bytes: u64) {
        self.bytes_down.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记一次未分类错误（内部错误、审计写入失败等）。
    pub fn add_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        self.record_error_window();
    }

    /// 记一次带应用层错误码的错误（`open.err` 等），同时计入总数与错误率。
    pub fn add_error_code(&self, code: u16) {
        self.add_error();
        let mut codes = self
            .error_codes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *codes.entry(code).or_insert(0) += 1;
    }

    /// 会话成功建立：累计数 +1，并按当前并发数更新峰值。
    pub fn session_opened(&self, active: u64) {
        self.sessions_total.fetch_add(1, Ordering::Relaxed);
        self.sessions_peak.fetch_max(active, Ordering::Relaxed);
    }

    /// `/ws/device` 或 `/ws/view` 握手成功（升级并发送 `ready`）。
    pub fn handshake_ok(&self) {
        self.handshakes_ok.fetch_add(1, Ordering::Relaxed);
    }

    /// `/ws/device` 或 `/ws/view` 握手被拒（鉴权失败、参数非法、超配额等）。
    pub fn handshake_failed(&self) {
        self.handshakes_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// 转发时目标通道已关闭/发送失败，帧被丢弃。
    pub fn add_dropped(&self) {
        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
    }

    /// 上下行字节合计（兼容旧的 `/metrics` 接口）。
    pub fn total_bytes(&self) -> u64 {
        self.bytes_up.load(Ordering::Relaxed) + self.bytes_down.load(Ordering::Relaxed)
    }

    /// 累计错误数（兼容旧的 `/metrics` 接口）。
    pub fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// 生成 `/api/v1/metrics` 的完整 JSON 快照。
    ///
    /// `active_sessions` / `online_devices` / `accounts` 由调用方现算：
    /// 前两者来自内存表，账号数来自数据库。
    pub fn snapshot(&self, active_sessions: u64, online_devices: u64, accounts: u64) -> Value {
        let by_code: BTreeMap<u16, u64> = self
            .error_codes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        json!({
            "uptimeSecs": self.started.elapsed().as_secs(),
            "sessions": {
                "active": active_sessions,
                "peak": self.sessions_peak.load(Ordering::Relaxed),
                "total": self.sessions_total.load(Ordering::Relaxed),
            },
            "bytes": {
                "toDevices": self.bytes_up.load(Ordering::Relaxed),
                "toViewers": self.bytes_down.load(Ordering::Relaxed),
            },
            "errors": {
                "total": self.errors.load(Ordering::Relaxed),
                "rate5m": self.errors_per_minute_5m(),
                "byCode": by_code,
            },
            "devices": {
                "online": online_devices,
                "accounts": accounts,
            },
            "handshakes": {
                "ok": self.handshakes_ok.load(Ordering::Relaxed),
                "failed": self.handshakes_failed.load(Ordering::Relaxed),
            },
            "backpressure": {
                "droppedFrames": self.dropped_frames.load(Ordering::Relaxed),
            },
        })
    }

    fn record_error_window(&self) {
        let epoch = self.started.elapsed().as_secs() / ERROR_BUCKET_SECS;
        let mut buckets = self
            .error_buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let slot = (epoch as usize) % ERROR_BUCKETS;
        let bucket = &mut buckets[slot];
        if bucket.epoch != epoch {
            bucket.epoch = epoch;
            bucket.count = 0;
        }
        bucket.count = bucket.count.saturating_add(1);
    }

    /// 最近 5 分钟窗口内的平均每分钟错误数，保留两位小数。
    fn errors_per_minute_5m(&self) -> f64 {
        let now_epoch = self.started.elapsed().as_secs() / ERROR_BUCKET_SECS;
        let buckets = self
            .error_buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let oldest = now_epoch.saturating_sub(ERROR_BUCKETS as u64 - 1);
        let total: u64 = buckets
            .iter()
            .filter(|bucket| bucket.epoch <= now_epoch && bucket.epoch >= oldest)
            .map(|bucket| bucket.count)
            .sum();
        let window_minutes = (ERROR_BUCKETS as f64) * (ERROR_BUCKET_SECS as f64) / 60.0;
        ((total as f64 / window_minutes) * 100.0).round() / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_counters() {
        let metrics = Metrics::default();
        metrics.add_up(100);
        metrics.add_down(250);
        metrics.add_error_code(4200);
        metrics.add_error_code(4200);
        metrics.add_error_code(4100);
        metrics.add_error();
        metrics.session_opened(3);
        metrics.session_opened(2);
        metrics.handshake_ok();
        metrics.handshake_failed();
        metrics.add_dropped();

        let snapshot = metrics.snapshot(1, 2, 3);
        assert_eq!(snapshot["sessions"]["active"], 1);
        assert_eq!(snapshot["sessions"]["peak"], 3);
        assert_eq!(snapshot["sessions"]["total"], 2);
        assert_eq!(snapshot["bytes"]["toDevices"], 100);
        assert_eq!(snapshot["bytes"]["toViewers"], 250);
        assert_eq!(snapshot["errors"]["total"], 4);
        assert_eq!(snapshot["errors"]["byCode"]["4200"], 2);
        assert_eq!(snapshot["errors"]["byCode"]["4100"], 1);
        assert!(snapshot["errors"]["rate5m"].as_f64().unwrap() > 0.0);
        assert_eq!(snapshot["devices"]["online"], 2);
        assert_eq!(snapshot["devices"]["accounts"], 3);
        assert_eq!(snapshot["handshakes"]["ok"], 1);
        assert_eq!(snapshot["handshakes"]["failed"], 1);
        assert_eq!(snapshot["backpressure"]["droppedFrames"], 1);
        assert!(snapshot["uptimeSecs"].is_u64());
    }

    #[test]
    fn empty_snapshot_has_zero_rate() {
        let metrics = Metrics::default();
        let snapshot = metrics.snapshot(0, 0, 0);
        assert_eq!(snapshot["errors"]["rate5m"], 0.0);
        assert_eq!(snapshot["errors"]["total"], 0);
        assert_eq!(snapshot["sessions"]["peak"], 0);
    }
}
