//! Windows 屏幕采集 + JPEG 编码。
//!
//! 首选 DXGI 桌面复制（`crate::dxgi`）：单帧 1–3ms，1080p 60fps 才跑得动。
//! 多显示器、远程桌面会话或复制被占用时自动回退到 GDI `StretchBlt`：
//! 它单帧约 40ms（DWM 回读），只保证功能，不保证帧率。
//!
//! GDI 路径把虚拟桌面（所有显示器拼成的包围盒）按目标宽度等比缩放；
//! DXGI 路径拿单屏原生分辨率，需要缩小时在 CPU 上做双线性缩放。
//! 两条路径都输出紧凑 BGRA，再交给 jpeg-encoder 编码。
//!
//! 光标：DXGI 的桌面纹理不含硬件光标，由 `crate::cursor` 在 CPU 上合成
//! （位置来自 `PointerPosition`，形状来自 `GetFramePointerShape`）；GDI
//! 路径改用 `DrawIconEx` 直接画进 DIB。两条路径都受 `CaptureOptions::cursor`
//! 控制，屏幕墙缩略图始终关闭。

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use jpeg_encoder::{ColorType, Encoder};
use screenwall_proto::cloud::TUNNEL_MAX_PAYLOAD;
use screenwall_proto::{
    encode_frame, FrameCodec, FrameHeader, FrameType, FLAG_KEYFRAME, FRAME_HEADER_LEN,
};
use tokio::sync::oneshot;

use crate::display::MonitorRect;
use crate::dxgi::{DxgiCapturer, DxgiError};

const SRCCOPY: u32 = 0x00CC_0020;
const CAPTUREBLT: u32 = 0x4000_0000;
const DIB_RGB_COLORS: u32 = 0;
const BI_RGB: u32 = 0;
const HALFTONE: i32 = 4;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
// 虚拟桌面：所有显示器按 Windows 排列合并后的包围盒。
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

/// 通过 `SetThreadExecutionState` 请求系统与显示器保持唤醒。
const ES_CONTINUOUS: u32 = 0x8000_0000;
const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

/// 预留给会话帧头（8 字节）和媒体帧头（24 字节）后的最大 JPEG 负载。
const MAX_JPEG_BYTES: usize = TUNNEL_MAX_PAYLOAD - 8 - FRAME_HEADER_LEN - 1024;
/// JPEG 超限时的降级质量阶梯：请求质量 → 40 → 30；30 仍超限则丢弃该帧。
const FALLBACK_QUALITY: u8 = 40;
const MIN_JPEG_QUALITY: u8 = 30;
const MAX_JPEG_QUALITY: u8 = 90;
/// 设了码率上限时，单帧预算再小也不低于这个值（约一张 320×180 的 JPEG）。
/// 低于它基本编码不出可辨认的画面，与其发糊图不如保持原有质量。
const MIN_FRAME_BYTES: usize = 4 * 1024;

/// 协议未指定 maxWidth 时的输出宽度默认值（1080p 档位）。
pub const DEFAULT_MAX_WIDTH: u32 = 1920;

/// 把 Windows 定时器精度提升到 1ms，保证 60fps 的帧间隔不被 15.6ms 粒度拖慢。
const TIMER_PERIOD_MS: u32 = 1;

/// 采集出错后重建后端的最小间隔，避免锁屏等持续失败场景反复初始化。
const RECOVER_INTERVAL: Duration = Duration::from_secs(2);

/// DXGI 非致命错误连续出现这个次数就回退 GDI；ACCESS_LOST 则立即回退。
const DXGI_MAX_CONSECUTIVE_FAILURES: u32 = 2;

/// 回退到 GDI/黑帧后，每隔这么久探测一次 DXGI。
/// 显示器重新打开、会话解锁或 RDP 重连后，能自动切回 GPU 快路径。
const DXGI_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// DXGI 启动后这么久还没取到第一帧就回退 GDI。
/// 显示器关闭时桌面复制可能只超时、不报错，回退后才能给出稳定首帧。
const DXGI_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// 所有显示器都断开时系统指标可能全是 0，用这个固定虚拟分辨率保证画面流不断。
const FALLBACK_DESKTOP_W: u32 = 1024;
const FALLBACK_DESKTOP_H: u32 = 768;

/// 桌面静止时的补帧间隔：既保持主控端帧率统计，又不浪费带宽。
const IDLE_RESEND_AFTER: Duration = Duration::from_millis(500);

/// 抓屏线程最多持有的原始帧缓冲区数量（每块 1080p 约 8MB）。
const BUFFER_POOL: usize = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetCursorInfo(info: *mut CursorInfo) -> i32;
    fn GetIconInfo(icon: isize, info: *mut IconInfo) -> i32;
    fn DrawIconEx(
        hdc: isize,
        x: i32,
        y: i32,
        icon: isize,
        cx: i32,
        cy: i32,
        step: u32,
        brush: isize,
        flags: u32,
    ) -> i32;
}

/// winuser.h 的 CURSORINFO。`cb_size` 必须先填结构大小。
#[repr(C)]
#[derive(Default)]
struct CursorInfo {
    cb_size: u32,
    flags: u32,
    cursor: isize,
    x: i32,
    y: i32,
}

/// winuser.h 的 ICONINFO，只用到热点和两个位图句柄。
#[repr(C)]
#[derive(Default)]
struct IconInfo {
    is_icon: i32,
    hot_x: u32,
    hot_y: u32,
    mask: isize,
    color: isize,
}

/// wingdi.h 的 BITMAP，用于读光标位图的真实尺寸。
#[repr(C)]
#[derive(Default)]
struct Bitmap {
    kind: i32,
    width: i32,
    height: i32,
    width_bytes: i32,
    planes: u16,
    bits_pixel: u16,
    bits: *mut c_void,
}

/// winuser.h 的 CURSOR_SHOWING。
const CURSOR_SHOWING: u32 = 1;
/// winuser.h 的 DI_NORMAL。
const DI_NORMAL: u32 = 3;

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn DeleteDC(hdc: isize) -> i32;
    fn CreateDIBSection(
        hdc: isize,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: isize,
        offset: u32,
    ) -> isize;
    fn SelectObject(hdc: isize, object: isize) -> isize;
    fn DeleteObject(object: isize) -> i32;
    fn GetObjectW(object: isize, size: i32, out: *mut c_void) -> i32;
    fn SetStretchBltMode(hdc: isize, mode: i32) -> i32;
    fn StretchBlt(
        hdc_dest: isize,
        x_dest: i32,
        y_dest: i32,
        w_dest: i32,
        h_dest: i32,
        hdc_src: isize,
        x_src: i32,
        y_src: i32,
        w_src: i32,
        h_src: i32,
        rop: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

/// 采集线程存活期间把系统定时器精度提升到 1ms，Drop 时归还。
struct TimerResolution;

impl TimerResolution {
    fn acquire() -> Self {
        let result = unsafe { timeBeginPeriod(TIMER_PERIOD_MS) };
        if result != 0 {
            tracing::warn!(result, "timeBeginPeriod(1) 失败，60fps 可能达不到");
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        let result = unsafe { timeEndPeriod(TIMER_PERIOD_MS) };
        if result != 0 {
            tracing::warn!(result, "timeEndPeriod(1) 失败");
        }
    }
}

/// 采集线程存活期间阻止显示器休眠，Drop 时恢复原有电源行为。
struct KeepAwake;

impl KeepAwake {
    fn acquire() -> Self {
        let flags = ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED;
        if unsafe { SetThreadExecutionState(flags) } == 0 {
            tracing::warn!("开启屏幕保活失败: {}", std::io::Error::last_os_error());
        }
        Self
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if unsafe { SetThreadExecutionState(ES_CONTINUOUS) } == 0 {
            tracing::warn!("恢复电源计划失败: {}", std::io::Error::last_os_error());
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureOptions {
    pub fps: u16,
    pub max_width: u32,
    pub quality: u8,
    /// 只采集这一块屏幕；`None` 表示整块虚拟桌面（多屏拼接）。
    pub region: Option<MonitorRect>,
    /// 是否把系统光标画进画面（仅 DXGI 路径支持；GDI 回退没有光标）。
    pub cursor: bool,
    /// 目标码率上限（kbps）。`None` 表示不限制，只受隧道单帧上限约束。
    ///
    /// MJPEG 没有 CBR 概念，能做的是「每帧字节预算」：按 `kbps * 1000 / 8 / fps`
    /// 算出单帧允许的字节数，编码时选第一个不超预算的质量档。这比只调 quality
    /// 更接近真实码率控制 —— quality 对帧大小的影响随画面内容剧烈变化。
    pub max_bitrate: Option<u32>,
}

impl CaptureOptions {
    pub fn normalized(self) -> Self {
        Self {
            fps: self.fps.clamp(1, 60),
            max_width: self.max_width.clamp(320, 3840),
            quality: self.quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY),
            region: self.region.filter(|region| !region.is_empty()),
            cursor: self.cursor,
            // 低于 50 kbps 连一张小缩略图都放不下，按不限速处理；上限压到
            // 200 Mbps，避免 u32 溢出后在别处算出荒谬的预算。
            max_bitrate: self
                .max_bitrate
                .filter(|kbps| (50..=200_000).contains(kbps)),
        }
    }

    /// 单帧字节预算；没设上限时返回隧道允许的最大帧长。
    pub fn frame_budget(&self) -> usize {
        let Some(kbps) = self.max_bitrate else {
            return MAX_JPEG_BYTES;
        };
        let per_second = u64::from(kbps) * 1000 / 8;
        let per_frame = per_second / u64::from(self.fps.max(1));
        // 太小会一帧都发不出去；给一个下限，宁可略超也不断流。
        (per_frame.max(MIN_FRAME_BYTES as u64) as usize).min(MAX_JPEG_BYTES)
    }
}

/// 采集线程句柄。Drop 时通知线程退出，不阻塞当前异步任务。
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 采集线程就绪时回传的尺寸；失败时是给用户看的中文原因。
pub type ReadySize = Result<(u32, u32), String>;
/// `spawn` 的返回值：句柄 + 就绪通道。
pub type SpawnedCapture = (CaptureHandle, oneshot::Receiver<ReadySize>);

/// 启动采集线程。
///
/// 返回句柄和一个 `ready` 通道：线程创建采集后端后回传实际输出尺寸，
/// 尺寸收集失败时回传错误字符串。
pub fn spawn(
    stream_id: u16,
    options: CaptureOptions,
    sink: impl Fn(Vec<u8>) + Send + 'static,
) -> SpawnedCapture {
    let options = options.normalized();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (ready_tx, ready_rx) = oneshot::channel();

    let thread = std::thread::Builder::new()
        .name("screen-capture".to_string())
        .spawn(move || {
            // 线程级保活：显示器因电源计划休眠会导致画面黑屏/停止更新。
            let _keep_awake = KeepAwake::acquire();
            // 1ms 定时器精度，否则 60fps 会被 Windows 的 15.6ms 粒度压到约 30fps。
            let _timer = TimerResolution::acquire();
            let mut ready_tx = Some(ready_tx);
            let mut capturer = match Capturer::new(options) {
                Ok(capturer) => capturer,
                Err(error) => {
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("{error:#}")));
                    }
                    return;
                }
            };

            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(capturer.size()));
            }
            run_capture(&mut capturer, stream_id, options, &thread_stop, sink);

            // 采集停止时不再假设主控端还能发送 releaseAll。
            crate::input::release_all();
        });

    if let Err(error) = &thread {
        tracing::warn!("创建屏幕采集线程失败: {error}");
    }

    (
        CaptureHandle {
            stop,
            _thread: thread.ok(),
        },
        ready_rx,
    )
}

fn run_capture(
    capturer: &mut Capturer,
    stream_id: u16,
    options: CaptureOptions,
    stop: &AtomicBool,
    sink: impl Fn(Vec<u8>) + Send + 'static,
) {
    // 用秒换算，60fps 得到 16.67ms 而不是整数截断的 16ms。
    let interval = Duration::from_secs_f64(1.0 / options.fps.max(1) as f64);
    let latest = Arc::new(Latest::<FrameJob>::new());
    let (recycle_tx, recycle_rx) = mpsc::channel::<Vec<u8>>();
    let blit_us = Arc::new(AtomicU64::new(0));
    let convert_us = Arc::new(AtomicU64::new(0));

    let encoder = {
        let latest = Arc::clone(&latest);
        let blit_us = Arc::clone(&blit_us);
        let convert_us = Arc::clone(&convert_us);
        std::thread::Builder::new()
            .name("jpeg-encode".to_string())
            .spawn(move || {
                encode_loop(
                    stream_id,
                    options,
                    &latest,
                    recycle_tx,
                    &blit_us,
                    &convert_us,
                    sink,
                );
            })
    };
    if let Err(error) = &encoder {
        tracing::error!("创建编码线程失败: {error}");
    }

    // 抓屏线程只负责「取帧 → 放进邮箱」，编码和发送在另一条线程上跑，
    // 抓屏不必等 JPEG 编码，60fps 才不会被编码耗时拖到 45fps。
    let mut pool: Vec<Vec<u8>> = Vec::with_capacity(BUFFER_POOL);
    let mut last_error_log: Option<Instant> = None;
    let mut last_recover: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        let frame_start = Instant::now();
        // 回收编码线程用完的缓冲区，稳态下这条路径不再分配内存。
        while let Ok(buffer) = recycle_rx.try_recv() {
            if pool.len() < BUFFER_POOL {
                pool.push(buffer);
            }
        }
        let mut buffer = pool.pop().unwrap_or_default();

        match capturer.capture_into(&mut buffer, interval) {
            Ok(Some((width, height))) => {
                if let Some(stale) = latest.set(FrameJob {
                    pixels: buffer,
                    width,
                    height,
                }) {
                    // 编码还没跟上：丢掉旧帧，只保留最新画面，延迟不累积。
                    if pool.len() < BUFFER_POOL {
                        pool.push(stale.pixels);
                    }
                }
            }
            Ok(None) => {
                // 桌面这几毫秒没有更新：编码线程会在空闲时补发缓存帧。
                if pool.len() < BUFFER_POOL {
                    pool.push(buffer);
                }
            }
            Err(error) => {
                if log_allowed(&mut last_error_log, Instant::now()) {
                    tracing::warn!("屏幕采集失败: {error:#}");
                }
                if pool.len() < BUFFER_POOL {
                    pool.push(buffer);
                }
                // 分辨率切换、锁屏、独占全屏都会让桌面复制失效，限频重建。
                let should_recover = last_recover.is_none_or(|at| at.elapsed() >= RECOVER_INTERVAL);
                if should_recover {
                    last_recover = Some(Instant::now());
                    match Capturer::new(options) {
                        Ok(fresh) => {
                            tracing::info!("采集后端已重建");
                            *capturer = fresh;
                        }
                        Err(error) => tracing::warn!("重建采集后端失败: {error:#}"),
                    }
                }
            }
        }

        blit_us.store(capturer.last_blit_us(), Ordering::Relaxed);
        convert_us.store(capturer.last_convert_us(), Ordering::Relaxed);

        let elapsed = frame_start.elapsed();
        if elapsed < interval {
            wait_for(interval - elapsed);
        } else {
            // 已经落后，只让出极短时间，避免无谓地压低帧率或打满 CPU。
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    latest.close();
    if let Ok(handle) = encoder {
        let _ = handle.join();
    }
}

/// 抓屏线程交给编码线程的一帧原始 BGRA。
struct FrameJob {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
}

/// 单槽邮箱：新帧覆盖旧帧，取帧方永远拿最新画面。
struct Latest<T> {
    slot: Mutex<Option<T>>,
    ready: Condvar,
    closed: AtomicBool,
}

impl<T> Latest<T> {
    fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// 放入新值，返回被挤掉的旧值，由调用方回收缓冲区。
    fn set(&self, value: T) -> Option<T> {
        let mut slot = self.slot.lock().unwrap();
        let previous = slot.replace(value);
        drop(slot);
        self.ready.notify_one();
        previous
    }

    /// 等待并取走一帧；超时或关闭时返回 None。
    fn take(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.slot.lock().unwrap();
        loop {
            if let Some(value) = slot.take() {
                return Some(value);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self.ready.wait_timeout(slot, deadline - now).ok()?;
            slot = guard;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// 编码线程：从邮箱取最新帧 → JPEG → 投递。
///
/// 桌面静止时每 IDLE_RESEND_AFTER 补发一次缓存帧：主控端的帧率、
/// 心跳和「画面还在」的状态不会因为没人动鼠标就归零。
fn encode_loop(
    stream_id: u16,
    options: CaptureOptions,
    latest: &Latest<FrameJob>,
    recycle: mpsc::Sender<Vec<u8>>,
    blit_us: &AtomicU64,
    convert_us: &AtomicU64,
    sink: impl Fn(Vec<u8>),
) {
    let started = Instant::now();
    let mut seq: u32 = 0;
    let mut last_jpeg: Option<Vec<u8>> = None;
    let mut last_sent = started - IDLE_RESEND_AFTER;
    let mut stats = EncodeStats::default();

    loop {
        let Some(job) = latest.take(IDLE_RESEND_AFTER) else {
            if latest.is_closed() {
                break;
            }
            if let Some(jpeg) = last_jpeg.as_ref() {
                if last_sent.elapsed() >= IDLE_RESEND_AFTER {
                    let frame = build_video_frame(stream_id, seq, started, jpeg);
                    if frame.len() <= TUNNEL_MAX_PAYLOAD {
                        sink(frame);
                        seq = seq.wrapping_add(1);
                        last_sent = Instant::now();
                        stats.record(jpeg.len());
                    }
                }
            }
            stats.maybe_log(stream_id, blit_us, convert_us, options);
            continue;
        };

        let (out_w, out_h) = (job.width, job.height);
        let encode_started = Instant::now();
        let requested = options.quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY);
        // 设了码率上限时，单帧允许的字节数按目标码率与帧率折算；
        // 没设时就是隧道单帧上限，行为与以前完全一致。
        let budget = options.frame_budget();
        let mut jpeg = Vec::with_capacity(256 * 1024);
        let mut chosen: Option<Vec<u8>> = None;
        let mut previous = MAX_JPEG_QUALITY + 1;
        for candidate in [requested, FALLBACK_QUALITY, MIN_JPEG_QUALITY] {
            // 请求质量已经低于兜底档位时跳过重复或更高的质量。
            if candidate >= previous {
                continue;
            }
            previous = candidate;
            jpeg.clear();
            if let Err(error) = encode_jpeg(&job.pixels, out_w, out_h, candidate, &mut jpeg) {
                tracing::warn!("JPEG 编码失败: {error:#}");
                break;
            }
            if jpeg.len() <= budget {
                chosen = Some(std::mem::take(&mut jpeg));
                break;
            }
        }
        // 三档都超预算时：若设了码率上限，宁可用最低质量那一帧也别断流
        // （画面糊但能动，比整帧丢弃好）；没设上限时保持原行为丢帧。
        if chosen.is_none()
            && options.max_bitrate.is_some()
            && !jpeg.is_empty()
            && jpeg.len() <= MAX_JPEG_BYTES
        {
            chosen = Some(std::mem::take(&mut jpeg));
        }
        stats.encode_us += encode_started.elapsed().as_micros() as u64;
        stats.samples += 1;

        match chosen {
            Some(jpeg) => {
                let frame = build_video_frame(stream_id, seq, started, &jpeg);
                if frame.len() <= TUNNEL_MAX_PAYLOAD {
                    sink(frame);
                    seq = seq.wrapping_add(1);
                    last_sent = Instant::now();
                    stats.record(jpeg.len());
                    last_jpeg = Some(jpeg);
                } else {
                    stats.dropped += 1;
                }
            }
            None => {
                stats.dropped += 1;
                tracing::warn!(out_w, out_h, "画面帧降到最低质量仍超过中转上限，已丢弃");
            }
        }

        // 把像素缓冲区还给抓屏线程；编码线程自己不持有大内存。
        let _ = recycle.send(job.pixels);
        stats.maybe_log(stream_id, blit_us, convert_us, options);
    }

    tracing::debug!(stream_id, frames = seq, "屏幕采集线程结束");
}

fn build_video_frame(stream_id: u16, seq: u32, started: Instant, jpeg: &[u8]) -> Vec<u8> {
    let header = FrameHeader {
        kind: FrameType::Video,
        stream_id,
        seq,
        ts_us: started.elapsed().as_micros() as u64,
        payload_len: 0,
        codec: FrameCodec::Jpeg,
        flags: FLAG_KEYFRAME,
    };
    encode_frame(&header, jpeg)
}

/// 编码线程的 5 秒统计窗口。
#[derive(Default)]
struct EncodeStats {
    window_started: Option<Instant>,
    frames: u64,
    bytes: u64,
    dropped: u64,
    encode_us: u64,
    samples: u64,
}

impl EncodeStats {
    fn record(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
        if self.window_started.is_none() {
            self.window_started = Some(Instant::now());
        }
    }

    fn maybe_log(
        &mut self,
        stream_id: u16,
        blit_us: &AtomicU64,
        convert_us: &AtomicU64,
        options: CaptureOptions,
    ) {
        let started = *self.window_started.get_or_insert_with(Instant::now);
        let elapsed = started.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        let seconds = elapsed.as_secs_f64().max(0.001);
        let samples = self.samples.max(1);
        tracing::debug!(
            stream_id,
            frames = self.frames,
            fps = self.frames as f64 / seconds,
            fps_target = options.fps,
            out_w = options.max_width,
            avg_jpeg_bytes = self.bytes.checked_div(self.frames).unwrap_or(0),
            dropped = self.dropped,
            blit_us = blit_us.load(Ordering::Relaxed),
            convert_us = convert_us.load(Ordering::Relaxed),
            encode_us = self.encode_us / samples,
            "采集统计"
        );
        self.window_started = Some(Instant::now());
        self.frames = 0;
        self.bytes = 0;
        self.dropped = 0;
        self.encode_us = 0;
        self.samples = 0;
    }
}

/// 睡眠到接近 deadline，最后 1ms 轻量自旋抵消 Sleep 的调度抖动。
fn wait_for(remaining: Duration) {
    const SPIN: Duration = Duration::from_millis(1);
    let deadline = Instant::now() + remaining;
    if remaining > SPIN {
        std::thread::sleep(remaining - SPIN);
    }
    // Sleep 可能已经睡过头，此时 deadline 已过，不会额外自旋。
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

/// 同一类日志每秒最多输出一次。
fn log_allowed(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_none_or(|previous| now.saturating_duration_since(previous) >= Duration::from_secs(1))
    {
        *last = Some(now);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------- 后端

enum Backend {
    Dxgi(DxgiCapturer),
    Gdi(GdiCapturer),
    /// GDI 也不可用时的最后兜底，稳定输出黑帧。
    Blank(BlankCapturer),
}

impl Backend {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Dxgi(capturer) => capturer.size(),
            Self::Gdi(capturer) => capturer.size(),
            Self::Blank(capturer) => capturer.size(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Dxgi(_) => "dxgi",
            Self::Gdi(_) => "gdi",
            Self::Blank(_) => "blank",
        }
    }
}

/// 对外统一的采集器：优先 DXGI，失败后自动回退 GDI/黑帧，并负责缩放与编码。
struct Capturer {
    options: CaptureOptions,
    backend: Backend,
    out_w: usize,
    out_h: usize,
    last_blit_us: u64,
    last_convert_us: u64,
    /// DXGI 连续失败次数；成功取到一帧（含「桌面无更新」）就清零。
    dxgi_failures: u32,
    /// 当前 DXGI 后端是否已经取到过画面。显示器关闭时桌面复制可能一直
    /// 只超时不出帧，启动后迟迟没有首帧就回退 GDI。
    dxgi_has_frame: bool,
    /// 本轮 DXGI 后端启用的时间，用于判断「启动后一直没画面」。
    dxgi_since: Instant,
    /// 当前不是 DXGI 时，下一次尝试重建 DXGI 的时间。
    dxgi_retry_at: Option<Instant>,
    /// 回退日志的限频时间戳，避免显示器反复抖动时刷屏。
    last_fallback_log: Option<Instant>,
}

impl Capturer {
    fn new(options: CaptureOptions) -> Result<Self> {
        let max_width = options.max_width.clamp(320, 3840);
        let (backend, dxgi_retry_at) = Self::build_backend(options);
        let (src_w, src_h) = backend.size();
        let (out_w, out_h) = scaled_output_size(src_w, src_h, max_width);

        tracing::info!(
            backend = backend.name(),
            src_w,
            src_h,
            out_w,
            out_h,
            "屏幕采集已初始化"
        );

        let dxgi_has_frame = !matches!(&backend, Backend::Dxgi(_));
        Ok(Self {
            options,
            backend,
            out_w,
            out_h,
            last_blit_us: 0,
            last_convert_us: 0,
            dxgi_failures: 0,
            dxgi_has_frame,
            dxgi_since: Instant::now(),
            dxgi_retry_at,
            last_fallback_log: None,
        })
    }

    /// 选择采集后端。无论显示器是否可用都保证返回一个后端：
    /// DXGI → GDI → 黑帧，启动阶段不再因采集失败而整体退出。
    fn build_backend(options: CaptureOptions) -> (Backend, Option<Instant>) {
        match DxgiCapturer::new(options.region, options.cursor) {
            Ok(capturer) => {
                tracing::info!("采集后端：DXGI 桌面复制");
                (Backend::Dxgi(capturer), None)
            }
            Err(error) => {
                tracing::info!("DXGI 采集不可用，回退 GDI: {error:#}");
                let retry_at = Instant::now() + DXGI_RETRY_INTERVAL;
                let backend =
                    match GdiCapturer::new(options.max_width, options.region, options.cursor) {
                        Ok(capturer) => Backend::Gdi(capturer),
                        Err(error) => {
                            tracing::warn!("GDI 采集不可用，改用稳定黑帧: {error:#}");
                            Backend::Blank(BlankCapturer::new(options.region, options.max_width))
                        }
                    };
                (backend, Some(retry_at))
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.out_w as u32, self.out_h as u32)
    }

    /// 抓一帧到 dst（紧凑 BGRA）。返回 Ok(None) 表示桌面没有更新。
    ///
    /// DXGI 失败不会向外抛错：连续失败或 ACCESS_LOST 时内部切到 GDI/黑帧，
    /// 本帧返回 None 让编码线程补发上一帧，下一次抓屏就走新后端。
    ///
    /// dst 由调用方从缓冲池取，编码线程用完再还回来，避免每帧 8MB 分配。
    fn capture_into(
        &mut self,
        dst: &mut Vec<u8>,
        timeout: Duration,
    ) -> Result<Option<(usize, usize)>> {
        // 回退期间每 DXGI_RETRY_INTERVAL 探测一次，显示器重新打开后自动切回。
        self.maybe_recover_backend();

        let (src_w, src_h) = self.backend.size();
        let (out_w, out_h) = (self.out_w, self.out_h);
        let dxgi_active = matches!(&self.backend, Backend::Dxgi(_));
        let convert_started = Instant::now();

        // 先在局部作用域里用完对后端的可变借用，再把结果拿出来处理，
        // 否则切换后端时会和仍存活的帧引用冲突。
        enum Step {
            Frame,
            Idle,
            Failed(anyhow::Error),
        }
        let step = {
            let grabbed = match &mut self.backend {
                Backend::Dxgi(capturer) => capturer.grab(timeout),
                Backend::Gdi(capturer) => Ok(Some(capturer.grab())),
                Backend::Blank(capturer) => Ok(Some(capturer.grab())),
            };
            match grabbed {
                Ok(Some(data)) => {
                    if src_w as usize == out_w && src_h as usize == out_h {
                        dst.clear();
                        dst.extend_from_slice(data);
                    } else {
                        scale_bgra(data, src_w as usize, src_h as usize, dst, out_w, out_h);
                    }
                    Step::Frame
                }
                Ok(None) => Step::Idle,
                Err(error) => Step::Failed(error),
            }
        };

        match step {
            Step::Frame => {
                self.dxgi_failures = 0;
                if dxgi_active {
                    self.dxgi_has_frame = true;
                }
                self.last_convert_us = convert_started.elapsed().as_micros() as u64;
                self.last_blit_us = self.backend_blit_us();
                Ok(Some((out_w, out_h)))
            }
            Step::Idle => {
                // 能走到「桌面无更新」说明后端本身是健康的。
                self.dxgi_failures = 0;
                // 显示器关闭时桌面复制可能成功创建但一直不出帧；启动阶段
                // 迟迟拿不到首帧就回退 GDI，避免主控端一直空白。
                if dxgi_active
                    && !self.dxgi_has_frame
                    && self.dxgi_since.elapsed() >= DXGI_FIRST_FRAME_TIMEOUT
                {
                    self.fallback_from_dxgi("DXGI 启用后迟迟没有取到首帧");
                }
                self.last_blit_us = self.backend_blit_us();
                self.last_convert_us = 0;
                Ok(None)
            }
            Step::Failed(error) => {
                if !dxgi_active {
                    // GDI/黑帧路径不返回错误；保留兜底，交给会话层重建。
                    return Err(error);
                }
                self.dxgi_failures = self.dxgi_failures.saturating_add(1);
                let fatal = error
                    .downcast_ref::<DxgiError>()
                    .is_some_and(DxgiError::requires_immediate_fallback);
                if fatal || self.dxgi_failures >= DXGI_MAX_CONSECUTIVE_FAILURES {
                    self.fallback_from_dxgi(&format!("{error:#}"));
                } else {
                    tracing::debug!(failures = self.dxgi_failures, "DXGI 抓帧失败: {error:#}");
                }
                self.last_blit_us = self.backend_blit_us();
                self.last_convert_us = 0;
                Ok(None)
            }
        }
    }

    /// DXGI 运行时失效：重建 GDI。GDI 也不可用时用黑帧保底，保证不断流。
    fn fallback_from_dxgi(&mut self, reason: &str) {
        if log_allowed(&mut self.last_fallback_log, Instant::now()) {
            tracing::warn!("DXGI 采集失效，切换到 GDI 回退: {reason}");
        }
        let backend = match GdiCapturer::new(
            self.options.max_width,
            self.options.region,
            self.options.cursor,
        ) {
            Ok(capturer) => Backend::Gdi(capturer),
            Err(error) => {
                tracing::warn!("GDI 回退初始化失败，改用稳定黑帧: {error:#}");
                Backend::Blank(BlankCapturer::new(
                    self.options.region,
                    self.options.max_width,
                ))
            }
        };
        self.backend = backend;
        self.dxgi_failures = 0;
        self.dxgi_retry_at = Some(Instant::now() + DXGI_RETRY_INTERVAL);
        self.refresh_output_size();
    }

    /// 非 DXGI 后端下周期性探测 DXGI；成功就切回 GPU 快路径。
    fn maybe_recover_backend(&mut self) {
        let Some(due) = self.dxgi_retry_at else {
            return;
        };
        if Instant::now() < due {
            return;
        }

        // 用单次尝试：探测器不能把抓屏线程卡在几百毫秒的重试里。
        match DxgiCapturer::new_with_attempts(self.options.region, self.options.cursor, 1) {
            Ok(capturer) => {
                tracing::info!("DXGI 桌面复制已恢复，切回 GPU 采集");
                self.backend = Backend::Dxgi(capturer);
                self.dxgi_failures = 0;
                self.dxgi_has_frame = false;
                self.dxgi_since = Instant::now();
                self.dxgi_retry_at = None;
                self.refresh_output_size();
                return;
            }
            Err(error) => tracing::debug!("DXGI 恢复探测失败，稍后重试: {error:#}"),
        }

        // 当前是黑帧保底时顺带再试一次 GDI：可能只是初始化时桌面还没就绪。
        if matches!(&self.backend, Backend::Blank(_)) {
            match GdiCapturer::new(
                self.options.max_width,
                self.options.region,
                self.options.cursor,
            ) {
                Ok(capturer) => {
                    tracing::info!("GDI 采集已恢复，从黑帧保底切回");
                    self.backend = Backend::Gdi(capturer);
                    self.refresh_output_size();
                }
                Err(error) => tracing::debug!("GDI 恢复探测失败: {error:#}"),
            }
        }
        self.dxgi_retry_at = Some(Instant::now() + DXGI_RETRY_INTERVAL);
    }

    /// 后端切换后重新计算输出尺寸，保证缩放目标与源尺寸匹配。
    fn refresh_output_size(&mut self) {
        let (src_w, src_h) = self.backend.size();
        let (out_w, out_h) = scaled_output_size(src_w, src_h, self.options.max_width);
        if (out_w, out_h) != (self.out_w, self.out_h) {
            tracing::info!(
                old_w = self.out_w,
                old_h = self.out_h,
                out_w,
                out_h,
                "采集输出尺寸已更新"
            );
            self.out_w = out_w;
            self.out_h = out_h;
        }
    }

    fn last_blit_us(&self) -> u64 {
        self.last_blit_us
    }

    fn last_convert_us(&self) -> u64 {
        self.last_convert_us
    }

    fn backend_blit_us(&self) -> u64 {
        match &self.backend {
            Backend::Dxgi(capturer) => capturer.last_blit_us(),
            Backend::Gdi(capturer) => capturer.last_blit_us(),
            Backend::Blank(_) => 0,
        }
    }
}

// ---------------------------------------------------------------- 尺寸兜底

/// 读虚拟桌面指标；宽高必须为正才有效。
fn virtual_rect_metrics() -> Option<MonitorRect> {
    // 安全：GetSystemMetrics 只按常量索引读系统指标，不涉及指针。
    unsafe {
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w > 0 && h > 0 {
            Some(MonitorRect {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: w as u32,
                h: h as u32,
            })
        } else {
            None
        }
    }
}

/// 读主屏指标；原点固定为 (0, 0)。
fn primary_rect_metrics() -> Option<MonitorRect> {
    // 安全：同 virtual_rect_metrics，只读系统指标。
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        (w > 0 && h > 0).then_some(MonitorRect {
            x: 0,
            y: 0,
            w: w as u32,
            h: h as u32,
        })
    }
}

/// 显示器全部断开时 Windows 可能把虚拟桌面指标报成 0。按
/// 虚拟桌面 → 主屏 → 固定 1024×768 的顺序兜底，保证源尺寸永远非零。
fn pick_desktop_rect(
    virtual_rect: Option<MonitorRect>,
    primary_rect: Option<MonitorRect>,
) -> MonitorRect {
    virtual_rect
        .filter(|rect| !rect.is_empty())
        .or_else(|| primary_rect.filter(|rect| !rect.is_empty()))
        .unwrap_or(MonitorRect {
            x: 0,
            y: 0,
            w: FALLBACK_DESKTOP_W,
            h: FALLBACK_DESKTOP_H,
        })
}

/// 查询当前桌面矩形；拿不到任何显示器时返回固定的虚拟分辨率。
fn query_desktop_rect() -> MonitorRect {
    let virtual_rect = virtual_rect_metrics();
    let primary_rect = primary_rect_metrics();
    let rect = pick_desktop_rect(virtual_rect, primary_rect);
    if virtual_rect.is_none() && primary_rect.is_none() {
        tracing::warn!(
            w = rect.w,
            h = rect.h,
            "未检测到可用显示器，使用固定虚拟桌面继续出流"
        );
    }
    rect
}

/// 按最大宽度等比计算输出尺寸，任何输入都至少 1 像素。
fn scaled_output_size(src_w: u32, src_h: u32, max_width: u32) -> (usize, usize) {
    let src_w = src_w.max(1);
    let src_h = src_h.max(1);
    let max_width = max_width.clamp(320, 3840);
    let out_w = (src_w.min(max_width) as usize).max(1);
    let out_h = ((src_h as u64 * out_w as u64 + src_w as u64 / 2) / src_w as u64).max(1) as usize;
    (out_w, out_h)
}

// ---------------------------------------------------------------- GDI

struct GdiCapturer {
    screen_dc: isize,
    mem_dc: isize,
    dib: isize,
    old_object: isize,
    bits: *mut u8,
    src_x: i32,
    src_y: i32,
    src_w: i32,
    src_h: i32,
    out_w: i32,
    out_h: i32,
    last_blit_us: u64,
    /// 上一次 BitBlt 是否失败。失败后 DIB 里是黑帧，不必每帧重复清零。
    blank: bool,
    /// 是否把系统光标画进画面。
    cursor: bool,
    /// 上次画过的光标句柄；句柄不变时不必重新查热点与尺寸。
    cursor_handle: isize,
    /// 缓存的绘制尺寸（屏幕像素，未缩放）。
    cursor_w: i32,
    cursor_h: i32,
    /// 缓存的热点偏移。
    cursor_hot_x: i32,
    cursor_hot_y: i32,
}

impl GdiCapturer {
    fn new(max_width: u32, region: Option<MonitorRect>, cursor: bool) -> Result<Self> {
        // 源矩形默认覆盖整个虚拟桌面；多显示器时原点可能为负。
        // 指定了显示器时只 BitBlt 这块屏幕，缩放交给 StretchBlt 一次完成。
        // 显示器全部断开时 query_desktop_rect 会给出固定的 1024×768 虚拟桌面。
        let (src_x, src_y, src_w, src_h) = match region.filter(|rect| !rect.is_empty()) {
            Some(region) => (
                region.x,
                region.y,
                region.w.min(i32::MAX as u32) as i32,
                region.h.min(i32::MAX as u32) as i32,
            ),
            None => {
                let rect = query_desktop_rect();
                (
                    rect.x,
                    rect.y,
                    rect.w.min(i32::MAX as u32) as i32,
                    rect.h.min(i32::MAX as u32) as i32,
                )
            }
        };
        // 上面两条路径都保证非零；显式区域若被调用方传成 0，这里再兜一次。
        let src_w = src_w.max(1);
        let src_h = src_h.max(1);

        let (out_w, out_h) = scaled_output_size(src_w as u32, src_h as u32, max_width);
        let out_w = out_w as i32;
        let out_h = out_h as i32;

        let screen_dc = unsafe { GetDC(0) };
        if screen_dc == 0 {
            bail!("GetDC 失败: {}", std::io::Error::last_os_error());
        }

        let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
        if mem_dc == 0 {
            unsafe {
                ReleaseDC(0, screen_dc);
            }
            bail!(
                "CreateCompatibleDC 失败: {}",
                std::io::Error::last_os_error()
            );
        }

        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: out_w,
                // 负高度表示自上而下的 DIB，读出的像素行序与屏幕一致。
                height: -out_h,
                planes: 1,
                bit_count: 32,
                compression: BI_RGB,
                size_image: (out_w as u32)
                    .saturating_mul(out_h as u32)
                    .saturating_mul(4),
                x_pels_per_meter: 0,
                y_pels_per_meter: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };

        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = unsafe { CreateDIBSection(mem_dc, &info, DIB_RGB_COLORS, &mut bits, 0, 0) };
        if dib == 0 || bits.is_null() {
            unsafe {
                DeleteDC(mem_dc);
                ReleaseDC(0, screen_dc);
            }
            bail!("CreateDIBSection 失败: {}", std::io::Error::last_os_error());
        }

        let old_object = unsafe { SelectObject(mem_dc, dib) };
        if old_object == 0 {
            unsafe {
                DeleteObject(dib);
                DeleteDC(mem_dc);
                ReleaseDC(0, screen_dc);
            }
            bail!("SelectObject 失败: {}", std::io::Error::last_os_error());
        }

        // HALFTONE 让缩小时文字边缘更平滑。
        unsafe {
            SetStretchBltMode(mem_dc, HALFTONE);
        }

        tracing::info!(src_x, src_y, src_w, src_h, out_w, out_h, "GDI 采集已初始化");

        Ok(Self {
            screen_dc,
            mem_dc,
            dib,
            old_object,
            bits: bits as *mut u8,
            src_x,
            src_y,
            src_w,
            src_h,
            out_w,
            out_h,
            last_blit_us: 0,
            blank: false,
            cursor,
            cursor_handle: 0,
            cursor_w: 32,
            cursor_h: 32,
            cursor_hot_x: 0,
            cursor_hot_y: 0,
        })
    }

    fn size(&self) -> (u32, u32) {
        (self.out_w.max(1) as u32, self.out_h.max(1) as u32)
    }

    fn last_blit_us(&self) -> u64 {
        self.last_blit_us
    }

    /// 抓屏到 DIB，返回紧凑 BGRA（stride = out_w * 4）。
    ///
    /// 显示器关闭/断开、远程会话没有桌面合成时 `StretchBlt` 会失败。
    /// 此时交出稳定黑帧而不是报错：主控端画面流不断，显示器恢复后
    /// 下一帧自然回到真实画面。
    fn grab(&mut self) -> &[u8] {
        let started = Instant::now();
        let mut ok = unsafe {
            StretchBlt(
                self.mem_dc,
                0,
                0,
                self.out_w,
                self.out_h,
                self.screen_dc,
                self.src_x,
                self.src_y,
                self.src_w,
                self.src_h,
                SRCCOPY | CAPTUREBLT,
            )
        };
        if ok == 0 {
            // 部分显卡/远程桌面环境不支持 CAPTUREBLT，退回普通拷贝。
            ok = unsafe {
                StretchBlt(
                    self.mem_dc,
                    0,
                    0,
                    self.out_w,
                    self.out_h,
                    self.screen_dc,
                    self.src_x,
                    self.src_y,
                    self.src_w,
                    self.src_h,
                    SRCCOPY,
                )
            };
        }
        if ok == 0 {
            // 只在首次失败时清零：之后 DIB 本来就是黑帧，不必每帧写 8MB。
            if !self.blank {
                self.blank = true;
                let byte_len = self.frame_len();
                // 安全：bits 指向 CreateDIBSection 分配的 byte_len 字节，
                // 在本结构存活期间有效；这里只清空自己持有的 DIB 内存。
                unsafe { std::ptr::write_bytes(self.bits, 0, byte_len) };
                tracing::warn!(
                    "GDI 抓屏失败，改为输出黑帧: {}",
                    std::io::Error::last_os_error()
                );
            }
            self.last_blit_us = started.elapsed().as_micros() as u64;
            return self.frame_bytes();
        }
        self.blank = false;
        if self.cursor {
            // 光标直接画进 DIB，读出的像素已经带光标，不需要额外合成。
            self.draw_cursor();
        }
        self.last_blit_us = started.elapsed().as_micros() as u64;
        self.frame_bytes()
    }

    fn frame_len(&self) -> usize {
        (self.out_w as usize)
            .saturating_mul(self.out_h as usize)
            .saturating_mul(4)
    }

    fn frame_bytes(&self) -> &[u8] {
        // 安全：bits 指向 CreateDIBSection 分配的 frame_len 字节，结构存活期间有效。
        unsafe { std::slice::from_raw_parts(self.bits, self.frame_len()) }
    }

    /// 把系统光标直接画进 DIB（已经缩放好的输出坐标）。
    ///
    /// 用 `DrawIconEx` 让系统处理单色/彩色/带掩码三种光标格式，比自己解析
    /// 图标位图省事且更完整。坐标要按采集缩放比换算：`CURSORINFO` 给的是
    /// 屏幕坐标，先减源矩形原点，再乘输出/源尺寸。
    fn draw_cursor(&mut self) {
        let mut info = CursorInfo {
            cb_size: std::mem::size_of::<CursorInfo>() as u32,
            ..CursorInfo::default()
        };
        // 安全：info 是栈上按 CURSORINFO 布局定义的结构，cbSize 已填对。
        if unsafe { GetCursorInfo(&mut info) } == 0 {
            return;
        }
        if info.flags & CURSOR_SHOWING == 0 || info.cursor == 0 {
            return;
        }
        // 热点和尺寸只在光标句柄变化时查一次：GetIconInfo 会新分配两张
        // 位图，每帧调用等于每帧多分配两个 GDI 对象。
        if info.cursor != self.cursor_handle {
            let mut icon = IconInfo::default();
            // 安全：icon 是栈上按 ICONINFO 布局定义的结构；成功时 mask/color
            // 是新分配的位图句柄，读完必须 DeleteObject。
            if unsafe { GetIconInfo(info.cursor, &mut icon) } == 0 {
                return;
            }
            let (w, h) = unsafe {
                let size = read_bitmap_size(if icon.color != 0 {
                    icon.color
                } else {
                    icon.mask
                });
                if icon.mask != 0 {
                    DeleteObject(icon.mask);
                }
                if icon.color != 0 {
                    DeleteObject(icon.color);
                }
                size
            };
            self.cursor_handle = info.cursor;
            self.cursor_w = w;
            self.cursor_h = h;
            self.cursor_hot_x = icon.hot_x as i32;
            self.cursor_hot_y = icon.hot_y as i32;
        }

        // 光标左上角 = 热点屏幕坐标 - 热点偏移，再映射到输出坐标。
        let left = info.x - self.cursor_hot_x - self.src_x;
        let top = info.y - self.cursor_hot_y - self.src_y;
        let scale_x = self.out_w as f32 / self.src_w.max(1) as f32;
        let scale_y = self.out_h as f32 / self.src_h.max(1) as f32;
        let x = (left as f32 * scale_x).round() as i32;
        let y = (top as f32 * scale_y).round() as i32;
        let cx = (self.cursor_w as f32 * scale_x).round() as i32;
        let cy = (self.cursor_h as f32 * scale_y).round() as i32;
        // 安全：mem_dc 由本结构持有、位图已选入；info.cursor 句柄有效且在
        // 本函数内只读使用。绘制失败不影响画面，静默跳过即可。
        unsafe {
            DrawIconEx(
                self.mem_dc,
                x,
                y,
                info.cursor,
                cx.max(1),
                cy.max(1),
                0,
                0,
                DI_NORMAL,
            );
        }
    }
}

/// 读 HBITMAP 的像素尺寸；读不到时退回 32×32（Windows 默认光标大小）。
///
/// 安全：调用方保证 `bitmap` 是有效的 HBITMAP；`GetObjectW` 只写栈上的
/// `Bitmap`，读取失败返回零值，不影响调用方。
unsafe fn read_bitmap_size(bitmap: isize) -> (i32, i32) {
    let mut info = Bitmap::default();
    let copied = unsafe {
        GetObjectW(
            bitmap,
            std::mem::size_of::<Bitmap>() as i32,
            &mut info as *mut Bitmap as *mut c_void,
        )
    };
    if copied == 0 || info.width <= 0 || info.height <= 0 {
        return (32, 32);
    }
    // 单色光标只有 mask，此时 GetObject 得到的高度是两倍（AND+XOR 叠在一起）。
    if info.bits_pixel == 1 {
        return (info.width, (info.height / 2).max(1));
    }
    (info.width, info.height)
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        unsafe {
            if self.old_object != 0 {
                SelectObject(self.mem_dc, self.old_object);
            }
            if self.dib != 0 {
                DeleteObject(self.dib);
            }
            if self.mem_dc != 0 {
                DeleteDC(self.mem_dc);
            }
            if self.screen_dc != 0 {
                ReleaseDC(0, self.screen_dc);
            }
        }
    }
}

// ---------------------------------------------------------------- 黑帧兜底

/// 最后兜底后端：GDI 也不可用时输出稳定黑帧。
/// 不依赖屏幕 DC，因此显示器全部断开、无头会话里也能保证画面流不断。
struct BlankCapturer {
    width: u32,
    height: u32,
    frame: Vec<u8>,
}

impl BlankCapturer {
    fn new(region: Option<MonitorRect>, max_width: u32) -> Self {
        let src = region
            .filter(|rect| !rect.is_empty())
            .unwrap_or(MonitorRect {
                x: 0,
                y: 0,
                w: FALLBACK_DESKTOP_W,
                h: FALLBACK_DESKTOP_H,
            });
        let (width, height) = scaled_output_size(src.w, src.h, max_width);
        let mut frame = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
        // BGRA 黑色，alpha 填满；JPEG 忽略 alpha，但保持缓冲区规范。
        for pixel in frame.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Self {
            width: width as u32,
            height: height as u32,
            frame,
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width.max(1), self.height.max(1))
    }

    fn grab(&self) -> &[u8] {
        &self.frame
    }
}

// ---------------------------------------------------------------- 编码

fn encode_jpeg(
    bgra: &[u8],
    width: usize,
    height: usize,
    quality: u8,
    out: &mut Vec<u8>,
) -> Result<()> {
    let encoder = Encoder::new(out, quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY));
    encoder
        .encode(bgra, width as u16, height as u16, ColorType::Bgra)
        .context("JPEG 编码失败")?;
    Ok(())
}

/// 双线性缩放 BGRA。源和目标都是紧凑行（stride = width * 4）。
///
/// 走 `fast_image_resize` 的 SIMD 实现：4K→1080p 从纯 Rust 的 ~35ms 降到
/// 几毫秒，这是高分辨率机器能不能跑满 30/60 fps 的关键。它要求缓冲区按
/// 像素对齐；万一构造失败（极端尺寸/对齐），退回下面的标量实现，画面不能丢。
fn scale_bgra(src: &[u8], sw: usize, sh: usize, dst: &mut Vec<u8>, dw: usize, dh: usize) {
    if sw > 0 && sh > 0 && dw > 0 && dh > 0 {
        let dst_len = dw.saturating_mul(dh).saturating_mul(4);
        dst.clear();
        dst.resize(dst_len, 0);
        let src_view = ImageRef::new(sw as u32, sh as u32, src, PixelType::U8x4);
        if let Ok(src_view) = src_view {
            if let Ok(mut dst_view) =
                Image::from_slice_u8(dw as u32, dh as u32, dst.as_mut_slice(), PixelType::U8x4)
            {
                // 双线性与原来的实现一致；alpha 恒为 255，关掉 alpha 预乘省时间。
                let mut options = ResizeOptions::default();
                options.algorithm = ResizeAlg::Convolution(FilterType::Bilinear);
                options.mul_div_alpha = false;
                let done = RESIZER.with(|cell| {
                    cell.borrow_mut()
                        .resize(&src_view, &mut dst_view, &options)
                        .is_ok()
                });
                if done {
                    return;
                }
            }
        }
    }
    scale_bgra_scalar(src, sw, sh, dst, dw, dh);
}

thread_local! {
    /// `Resizer` 内部带可复用缓冲区，按线程缓存，避免每帧重新分配。
    static RESIZER: std::cell::RefCell<Resizer> = std::cell::RefCell::new(Resizer::new());
}

/// 标量双线性：SIMD 路径不可用时的兜底。
fn scale_bgra_scalar(src: &[u8], sw: usize, sh: usize, dst: &mut Vec<u8>, dw: usize, dh: usize) {
    let dst_len = dw.saturating_mul(dh).saturating_mul(4);
    dst.clear();
    dst.resize(dst_len, 0);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }

    // 预计算每列的源下标与权重，内层循环只做乘加。
    let mut columns = Vec::with_capacity(dw);
    for x in 0..dw {
        let pos = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5)
            .max(0.0)
            .min((sw - 1) as f32);
        let x0 = pos.floor() as usize;
        let x1 = (x0 + 1).min(sw - 1);
        columns.push((x0, x1, pos - x0 as f32));
    }

    for y in 0..dh {
        let pos = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5)
            .max(0.0)
            .min((sh - 1) as f32);
        let y0 = pos.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = pos - y0 as f32;
        let row0 = &src[y0 * sw * 4..];
        let row1 = &src[y1 * sw * 4..];
        let out_row = &mut dst[y * dw * 4..(y + 1) * dw * 4];
        for (x, &(x0, x1, wx)) in columns.iter().enumerate() {
            let i0 = x0 * 4;
            let i1 = x1 * 4;
            let o = x * 4;
            for c in 0..4 {
                let top = row0[i0 + c] as f32 * (1.0 - wx) + row0[i1 + c] as f32 * wx;
                let bottom = row1[i0 + c] as f32 * (1.0 - wx) + row1[i1 + c] as f32 * wx;
                out_row[o + c] = (top * (1.0 - wy) + bottom * wy).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_options_clamp_high_values() {
        let options = CaptureOptions {
            fps: 120,
            max_width: 8000,
            quality: 200,
            region: None,
            cursor: true,
            max_bitrate: None,
        }
        .normalized();
        assert_eq!(options.fps, 60);
        assert_eq!(options.max_width, 3840);
        assert_eq!(options.quality, MAX_JPEG_QUALITY);
    }

    #[test]
    fn capture_options_clamp_low_values() {
        let options = CaptureOptions {
            fps: 0,
            max_width: 10,
            quality: 1,
            region: None,
            cursor: false,
            max_bitrate: None,
        }
        .normalized();
        assert_eq!(options.fps, 1);
        assert_eq!(options.max_width, 320);
        assert_eq!(options.quality, MIN_JPEG_QUALITY);
    }

    #[test]
    fn frame_budget_without_cap_is_tunnel_limit() {
        let options = CaptureOptions {
            fps: 30,
            max_width: 1920,
            quality: 65,
            region: None,
            cursor: true,
            max_bitrate: None,
        };
        assert_eq!(options.frame_budget(), MAX_JPEG_BYTES);
    }

    #[test]
    fn frame_budget_splits_bitrate_across_fps() {
        // 2 Mbps @ 30 fps = 2000000 / 8 / 30 ≈ 8333 字节/帧。
        let options = CaptureOptions {
            fps: 30,
            max_width: 1920,
            quality: 65,
            region: None,
            cursor: true,
            max_bitrate: Some(2000),
        }
        .normalized();
        assert_eq!(options.frame_budget(), 8333);
    }

    #[test]
    fn frame_budget_has_floor() {
        // 极低码率下预算会被抬到下限，避免一帧都发不出去。
        let options = CaptureOptions {
            fps: 60,
            max_width: 1920,
            quality: 65,
            region: None,
            cursor: true,
            max_bitrate: Some(50),
        }
        .normalized();
        assert_eq!(options.frame_budget(), MIN_FRAME_BYTES);
    }

    #[test]
    fn frame_budget_rejects_absurd_caps() {
        // 0 和超出上限的值都当作不限制（`normalized` 会过滤掉）。
        for cap in [0, 500_000] {
            let options = CaptureOptions {
                fps: 30,
                max_width: 1920,
                quality: 65,
                region: None,
                cursor: true,
                max_bitrate: Some(cap),
            }
            .normalized();
            assert_eq!(options.max_bitrate, None, "{cap} kbps 应被过滤");
            assert_eq!(options.frame_budget(), MAX_JPEG_BYTES);
        }
    }

    /// 手动跑：`cargo test -p screenwall-agent --release bench_encode -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_encode_1080p() {
        let (w, h) = (1920usize, 1080usize);
        let mut frame = vec![0u8; w * h * 4];
        for (index, pixel) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = index % w;
            let y = index / w;
            pixel[0] = (x % 256) as u8;
            pixel[1] = (y % 256) as u8;
            pixel[2] = ((x + y) % 256) as u8;
            pixel[3] = 255;
        }
        let mut out = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            encode_jpeg(&frame, w, h, 65, &mut out).unwrap();
            println!("encode 1080p q65: {:?}", started.elapsed());
        }
    }

    /// 手动跑：`cargo test -p screenwall-agent --release bench_scale -- --ignored --nocapture`
    ///
    /// 4K 屏默认把画面缩到 1920 宽；这一步是纯 CPU 双线性，高分辨率机器上
    /// 往往比 JPEG 编码还贵，直接决定能不能跑满 30/60 fps。
    #[test]
    #[ignore]
    fn bench_scale_4k() {
        let (sw, sh) = (3840usize, 2160usize);
        let src = vec![17u8; sw * sh * 4];
        let mut dst = Vec::new();
        for (dw, dh) in [(1920usize, 1080usize), (1280, 720)] {
            for _ in 0..3 {
                let started = Instant::now();
                scale_bgra(&src, sw, sh, &mut dst, dw, dh);
                println!("scale 4K->{dw}x{dh}: {:?}", started.elapsed());
            }
        }
    }

    #[test]
    fn scale_bgra_keeps_size_and_pixel_order() {
        // 2x2 红蓝棋盘缩到 1x1，四个像素应平均成紫。
        let mut src = Vec::new();
        for value in [
            [0u8, 0, 255, 255],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
            [0, 0, 255, 255],
        ] {
            src.extend_from_slice(&value);
        }
        let mut dst = Vec::new();
        scale_bgra(&src, 2, 2, &mut dst, 1, 1);
        assert_eq!(dst.len(), 4);
        assert!(dst[0] > 100 && dst[0] < 155, "blue={}", dst[0]);
        assert!(dst[2] > 100 && dst[2] < 155, "red={}", dst[2]);
    }

    #[test]
    fn desktop_rect_prefers_virtual_screen() {
        let virtual_rect = MonitorRect {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        };
        let primary_rect = MonitorRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(
            pick_desktop_rect(Some(virtual_rect), Some(primary_rect)),
            virtual_rect
        );
    }

    #[test]
    fn desktop_rect_falls_back_to_primary() {
        let primary_rect = MonitorRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let empty = MonitorRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        assert_eq!(
            pick_desktop_rect(Some(empty), Some(primary_rect)),
            primary_rect
        );
        assert_eq!(pick_desktop_rect(None, Some(primary_rect)), primary_rect);
    }

    #[test]
    fn desktop_rect_is_never_empty_when_all_monitors_are_off() {
        // 所有显示器断开时系统指标可能全为 0，必须退回固定虚拟分辨率。
        let rect = pick_desktop_rect(None, None);
        assert!(!rect.is_empty());
        assert_eq!((rect.w, rect.h), (FALLBACK_DESKTOP_W, FALLBACK_DESKTOP_H));

        let empty = MonitorRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let rect = pick_desktop_rect(Some(empty), Some(empty));
        assert!(!rect.is_empty());
    }

    #[test]
    fn scaled_output_size_keeps_aspect_ratio() {
        assert_eq!(scaled_output_size(1920, 1080, 1920), (1920, 1080));
        assert_eq!(scaled_output_size(1920, 1080, 960), (960, 540));
        // 4K 源在 1080p 档位下等比缩到 1920×1080。
        assert_eq!(scaled_output_size(3840, 2160, 1920), (1920, 1080));
    }

    #[test]
    fn scaled_output_size_has_non_zero_floor() {
        // 无显示器/指标异常时源尺寸可能是 0，输出仍必须至少 1×1，
        // 否则缩放与 JPEG 编码会拿到空画面。
        let (w, h) = scaled_output_size(0, 0, 1920);
        assert!(w >= 1 && h >= 1, "{w}×{h}");
        assert_eq!(scaled_output_size(1024, 768, 1920), (1024, 768));
    }

    #[test]
    fn blank_backend_always_produces_a_frame() {
        // 黑帧兜底：尺寸非零、缓冲区长度正确、像素为不透明黑。
        let blank = BlankCapturer::new(None, 1920);
        let (w, h) = blank.size();
        assert_eq!((w, h), (FALLBACK_DESKTOP_W, FALLBACK_DESKTOP_H));
        let frame = blank.grab();
        assert_eq!(frame.len(), w as usize * h as usize * 4);
        assert!(frame.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
        assert!(frame.iter().step_by(4).all(|&blue| blue == 0));
    }
}
