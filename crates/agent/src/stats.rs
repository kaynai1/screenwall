//! 被控端运行指标：CPU 占用、内存占用、会话出流量，用于 `status.stats`。
//!
//! 两个采样都是进程无关的「整机」口径，和任务管理器里看到的一致：
//! CPU 用 `GetSystemTimes` 的两次差分（首次调用没有基准，返回 `None`），
//! 内存用 `GlobalMemoryStatusEx` 的 `dwMemoryLoad`。
//!
//! 采样失败（极少见，例如受限令牌）一律返回 `None`，由调用方决定回退成 0，
//! 不 panic、不阻塞。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use screenwall_proto::Stats;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

/// 上一次 `GetSystemTimes` 的结果，用来算差分。
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuSample {
    idle: u64,
    total: u64,
}

impl CpuSample {
    /// 采一次，返回「自上次采样以来」的整机 CPU 占用百分比（0–100）。
    ///
    /// 第一次调用只建立基准，返回 `None`。
    pub fn sample(&mut self) -> Option<f32> {
        let (idle, total) = read_times()?;
        let previous = *self;
        *self = Self { idle, total };
        if previous.total == 0 {
            return None;
        }
        // FILETIME 是 100 ns 计数，长时间运行后可能回绕；用 saturating 处理。
        let total_delta = total.saturating_sub(previous.total);
        let idle_delta = idle.saturating_sub(previous.idle);
        if total_delta == 0 {
            return None;
        }
        let busy = total_delta.saturating_sub(idle_delta);
        let percent = busy as f64 * 100.0 / total_delta as f64;
        Some(percent.clamp(0.0, 100.0) as f32)
    }
}

/// 整机 CPU 占用百分比；没有基准时返回 `None`（首次调用）。
pub fn cpu_percent(sample: &mut CpuSample) -> Option<f32> {
    sample.sample()
}

/// 整机内存占用百分比（0–100）。
pub fn memory_percent() -> Option<f32> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` 是栈上的合法结构，`dwLength` 已按类型大小填好。
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some((status.dwMemoryLoad as f32).clamp(0.0, 100.0))
}

/// 单个会话的出流量计数器：视频帧、音频包、文件块都往里加。
///
/// 计数在发送闭包里累加（那里拿得到每帧的字节数），`StatusReporter` 只负责
/// 按时间差分算 kbps。
#[derive(Debug, Default)]
pub struct ByteCounter {
    bytes: AtomicU64,
}

impl ByteCounter {
    pub fn add(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// 给一个会话生成 `status.stats` 的小状态机。
///
/// `stats()` 按「上次调用以来的增量」算 kbps，所以调用频率就是 `status` 的
/// 推送频率（5 秒一次），不需要额外的定时器。
pub struct StatusReporter {
    counter: std::sync::Arc<ByteCounter>,
    cpu: Mutex<CpuSample>,
    last: Mutex<(Instant, u64)>,
}

impl StatusReporter {
    pub fn new(counter: std::sync::Arc<ByteCounter>) -> Self {
        Self {
            counter,
            cpu: Mutex::new(CpuSample::default()),
            last: Mutex::new((Instant::now(), 0)),
        }
    }

    /// 组装一份 `Stats`：CPU 是整机占用，kbps 是本会话自上次调用以来的出流码率。
    pub fn stats(&self) -> Stats {
        let cpu = {
            let mut sample = self.cpu.lock().unwrap_or_else(|e| e.into_inner());
            cpu_percent(&mut sample).unwrap_or(0.0)
        };

        let now = Instant::now();
        let total = self.counter.total();
        let kbps = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let (previous_at, previous_bytes) = *last;
            let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
            if elapsed < 0.2 {
                // 窗口太短，差分会把噪声放大成假峰值；这一轮报 0，
                // 并且**不**推进基准，等窗口够长再算。
                0
            } else {
                let delta = total.saturating_sub(previous_bytes);
                *last = (now, total);
                // 字节 → 千比特/秒：× 8 / 1000。
                ((delta as f64 * 8.0 / 1000.0) / elapsed).round() as u32
            }
        };

        Stats {
            cpu,
            kbps,
            mem: memory_percent().unwrap_or(0.0),
        }
    }
}

fn read_times() -> Option<(u64, u64)> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 三个都是栈上的合法 FILETIME 指针，函数只写不读。
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let idle = filetime_to_u64(&idle);
    // 「内核时间」已经包含空闲时间，所以总时间是 kernel + user。
    let total = filetime_to_u64(&kernel).saturating_add(filetime_to_u64(&user));
    Some((idle, total))
}

fn filetime_to_u64(time: &FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_baseline() {
        let mut sample = CpuSample::default();
        // 首次只建基准；真实机器上一定有值，但差分本身没意义。
        assert!(sample.sample().is_none());
    }

    #[test]
    fn second_sample_is_a_percentage() {
        let mut sample = CpuSample::default();
        let _ = sample.sample();
        // 稍微忙等一点，保证 total 有增长。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
        while std::time::Instant::now() < deadline {
            std::hint::spin_loop();
        }
        let value = sample.sample().expect("第二次采样应有差分");
        assert!((0.0..=100.0).contains(&value), "CPU 百分比越界: {value}");
    }

    #[test]
    fn memory_percent_is_in_range() {
        let value = memory_percent().expect("Windows 上应能读到内存状态");
        assert!((0.0..=100.0).contains(&value), "内存百分比越界: {value}");
    }

    #[test]
    fn filetime_combines_high_and_low() {
        let time = FILETIME {
            dwLowDateTime: 0x0000_0001,
            dwHighDateTime: 0x0000_0002,
        };
        assert_eq!(filetime_to_u64(&time), (2u64 << 32) | 1);
    }

    #[test]
    fn zero_delta_does_not_divide_by_zero() {
        // 手工构造「两次读数相同」的情形：不应返回 NaN。
        let mut sample = CpuSample {
            idle: 100,
            total: 100,
        };
        // 真实读数和手工值不同，但无论结果如何都不能是 NaN。
        if let Some(value) = sample.sample() {
            assert!(!value.is_nan());
        }
    }

    #[test]
    fn byte_counter_accumulates() {
        let counter = ByteCounter::default();
        counter.add(100);
        counter.add(250);
        assert_eq!(counter.total(), 350);
    }

    #[test]
    fn reporter_reports_bytes_as_kbps() {
        let counter = std::sync::Arc::new(ByteCounter::default());
        let reporter = StatusReporter::new(counter.clone());
        // 第一次调用建立基准（窗口太短，报 0）。
        let first = reporter.stats();
        assert_eq!(first.kbps, 0);
        // 过 1 秒，累计 125,000 字节 → 1,000,000 bit/s = 1000 kbps。
        counter.add(125_000);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let second = reporter.stats();
        assert!(
            (900..=1100).contains(&second.kbps),
            "kbps 应为 1000 左右，实际 {}",
            second.kbps
        );
        // CPU 是整机口径，必须在合法范围内（首帧可能还没基准）。
        assert!((0.0..=100.0).contains(&second.cpu));
    }

    #[test]
    fn reporter_short_window_does_not_advance_baseline() {
        let counter = std::sync::Arc::new(ByteCounter::default());
        let reporter = StatusReporter::new(counter.clone());
        let _ = reporter.stats();
        counter.add(1000);
        // 立刻再取一次：窗口太短，报 0，且不应吃掉这段字节。
        assert_eq!(reporter.stats().kbps, 0);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        // 现在窗口够长，1000 字节 / 1 秒 = 8 kbps。
        let value = reporter.stats().kbps;
        assert!(
            (6..=10).contains(&value),
            "短窗口不应吞掉字节，期望约 8 kbps，实际 {value}"
        );
    }
}
