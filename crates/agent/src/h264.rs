//! Media Foundation 硬件 H.264 编码。
//!
//! 采集链路输出 BGRA，这里转成 NV12 后交给显卡的 H.264 编码 MFT（NVIDIA NVENC /
//! Intel Quick Sync / AMD VCE 都通过 Media Foundation 暴露），输出 Annex-B 字节流。
//! 相比 MJPEG：1080p 每帧编码从 ~12ms CPU 降到 GPU 上 1–3ms，4K60 才跑得起来。
//!
//! 几个关键点：
//!
//! - **只认硬件 MFT**：`MFTEnumEx` 用 `MFT_ENUM_FLAG_HARDWARE`，找不到就返回
//!   错误，上层回退 MJPEG。软件编码器不在本模块的职责里。
//! - **异步 MFT**：绝大多数显卡编码器是 async MFT，必须先用
//!   `MF_TRANSFORM_ASYNC_UNLOCK` 解锁，再按 `METransformNeedInput` /
//!   `METransformHaveOutput` 事件驱动。同步 MFT 走直接 `ProcessOutput` 路径。
//! - **低延迟**：CBR + 无 B 帧 + `CODECAPI_AVEncCommonLowLatency`，编码器内部
//!   不做前向缓冲；首帧与重订阅时用 `CODECAPI_AVEncVideoForceKeyFrame` 强制 IDR。
//! - **输入格式**：NV12，BT.709 limited range（见 [`crate::nv12`]）。宽高补齐到
//!   偶数，实际编码尺寸以 [`Encoder::size`] 为准。

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Error as WinError, Interface, GUID};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncH264CABACEnable, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFAttributes,
    IMFMediaEventGenerator, IMFMediaType, IMFTransform, METransformDrainComplete,
    METransformHaveOutput, METransformNeedInput, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::nv12::{bgra_to_nv12, nv12_len, padded_dims};

/// 被控端声明硬件 H.264 编码能力的能力标识。
pub const CAP: &str = screenwall_proto::caps::VIDEO_H264;

/// H.264 Main profile（77）。Main 在浏览器与所有硬件解码器上都可用，
/// High 的额外工具对屏幕内容没有必要。
const AVC_PROFILE_MAIN: u32 = 77;
/// 帧率分母。
const FPS_DEN: u32 = 1;
/// 等待异步 MFT 事件的轮询间隔。
const EVENT_POLL: Duration = Duration::from_micros(150);
/// 等编码器吐出当前帧输出的上限。超时就先返回已有的数据，输出会在下一次
/// `encode` 等待输入槽时补收 —— 不能为了等输出把采集线程卡住。
const OUTPUT_WAIT: Duration = Duration::from_millis(60);
/// 等编码器接受输入的时长。编码器内部最多缓冲一两帧，正常不会等这么久。
const INPUT_WAIT: Duration = Duration::from_millis(250);
/// 输出样本的默认分配大小；MFT 声明了 cbSize 时取更大者。
/// 4K 关键帧在 25 Mbps 下约几百 KB，留足余量避免 `MF_E_BUFFERTOOSMALL`。
const OUTPUT_MIN_BYTES: u32 = 4 * 1024 * 1024;

static AVAILABLE: OnceLock<bool> = OnceLock::new();

/// 本机是否有可用的硬件 H.264 编码 MFT。
///
/// 结果缓存：探测需要初始化 Media Foundation 并枚举 MFT，握手与订阅时都会问，
/// 不能每次都重新枚举。探测在独立线程上跑，避免把 COM 状态留在异步运行时的
/// 工作线程上。
pub fn hardware_available() -> bool {
    *AVAILABLE.get_or_init(|| {
        std::thread::Builder::new()
            .name("mf-h264-probe".into())
            .spawn(|| {
                with_mf(|| {
                    let list = enumerate_hardware()?;
                    Ok(!list.is_empty())
                })
                .unwrap_or(false)
            })
            .map(|handle| handle.join().unwrap_or(false))
            .unwrap_or(false)
    })
}

/// 供能力协商使用：硬件可用时返回 `["video.h264"]`，否则空切片。
pub fn capabilities() -> &'static [&'static str] {
    if hardware_available() {
        &[CAP]
    } else {
        &[]
    }
}

/// 一帧编码结果。
pub struct EncodedFrame {
    /// Annex-B 字节流（关键帧会带 SPS/PPS + IDR）。
    pub data: Vec<u8>,
    /// 是否关键帧（含 IDR）。客户端在解出关键帧前要丢弃非关键帧。
    pub keyframe: bool,
}

/// 硬件编码器。
///
/// 必须在同一个线程上创建、编码、销毁：Media Foundation 的 MFT 与 COM
/// 单元都绑定线程。采集链路把它放在专门的编码线程上。
pub struct Encoder {
    inner: Option<Inner>,
    /// 采集给的原始尺寸（可能为奇数），NV12 转换按它读 BGRA。
    src_width: usize,
    src_height: usize,
    /// 编码尺寸（偶数补齐）。
    width: u32,
    height: u32,
    nv12: Vec<u8>,
    /// 本线程的 COM 单元是否由我们初始化（Drop 时要配对 CoUninitialize）。
    com_owned: bool,
}

struct Inner {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    /// 异步 MFT 是否已经发出 `METransformNeedInput`。
    can_input: bool,
    /// 已经取出、等调用方来拿的输出帧。每项对应一帧（有的编码器一帧拆成
    /// 多个样本，`drain_output` 会把它们拼成一项）。
    ready: VecDeque<Vec<u8>>,
    /// 编码器自己分配输出样本（`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`）。
    provides_samples: bool,
    /// 提供给 `ProcessOutput` 的样本容量。
    output_bytes: u32,
    /// 输入时间戳（100ns 为单位）。
    sample_time: i64,
    frame_duration: i64,
    width: u32,
    height: u32,
}

impl Encoder {
    /// 创建一个硬件编码器；找不到硬件 MFT 时返回错误（上层据此回退 MJPEG）。
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        let (enc_w, enc_h) = padded_dims(width as usize, height as usize);
        let (enc_w, enc_h) = (enc_w as u32, enc_h as u32);
        let fps = fps.clamp(1, 240);
        let bitrate = bitrate_bps.clamp(500_000, 100_000_000);

        // 编码器要活过整个采集会话，所以 COM/MF 不能像探测那样用完就关：
        // 这里只初始化，等 Drop 时再配对关闭。
        let com_owned = start_mf()?;
        let built = (|| -> Result<Inner> {
            let activates = enumerate_hardware()?;
            if activates.is_empty() {
                bail!("没有找到硬件 H.264 编码器（需要支持 Media Foundation 的显卡驱动）");
            }
            let mut errors = Vec::new();
            for activate in &activates {
                let name = mft_name(activate);
                match unsafe { build(activate, enc_w, enc_h, fps, bitrate) } {
                    Ok(inner) => {
                        // 日志里带上具体是哪个 MFT：独显/核显/采集卡驱动各有一个，
                        // 性能差异很大，排查掉帧时第一个要看的就是它。
                        tracing::info!(encoder = %name, "选用硬件 H.264 编码器");
                        return Ok(inner);
                    }
                    Err(error) => errors.push(format!("{name}: {error:#}")),
                }
            }
            bail!("硬件编码器初始化失败: {}", errors.join("；"))
        })();

        match built {
            Ok(inner) => {
                tracing::info!(
                    width = enc_w,
                    height = enc_h,
                    fps,
                    bitrate,
                    "硬件 H.264 编码器已初始化"
                );
                Ok(Self {
                    inner: Some(inner),
                    src_width: width as usize,
                    src_height: height as usize,
                    width: enc_w,
                    height: enc_h,
                    nv12: Vec::new(),
                    com_owned,
                })
            }
            Err(error) => {
                stop_mf(com_owned);
                Err(error)
            }
        }
    }

    /// 编码器实际输出的尺寸（宽高已补齐到偶数）。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 提交一帧：BGRA → NV12 后送进编码器，GPU 在后台编码。
    ///
    /// 输出用 [`Encoder::collect`] 取。把提交和收取拆开，调用方就能在等上一帧
    /// 输出的时候转换下一帧，让 CPU 转换与硬件编码重叠（4K 每帧省下约 9ms）。
    pub fn submit_bgra(&mut self, bgra: &[u8], force_key: bool) -> Result<()> {
        let expected = self.src_width * self.src_height * 4;
        if bgra.len() < expected {
            bail!(
                "BGRA 帧长度不足: {} < {}（{}x{}）",
                bgra.len(),
                expected,
                self.src_width,
                self.src_height
            );
        }
        let (src_width, src_height) = (self.src_width, self.src_height);
        let size = nv12_len(self.width as usize, self.height as usize);
        self.nv12.resize(size, 0);
        // 拆开字段借用：转换写 self.nv12，提交只碰 self.inner。
        let Encoder { inner, nv12, .. } = self;
        let inner = inner.as_mut().ok_or_else(|| anyhow!("编码器已关闭"))?;
        bgra_to_nv12(bgra, src_width, src_height, nv12);
        inner.submit(nv12, force_key)
    }

    /// 直接提交 NV12（调用方已转好）。
    pub fn submit_nv12(&mut self, nv12: &[u8], force_key: bool) -> Result<()> {
        let inner = self.inner.as_mut().ok_or_else(|| anyhow!("编码器已关闭"))?;
        inner.submit(nv12, force_key)
    }

    /// 取一帧输出；`timeout` 内没有就绪的输出时返回 `None`。
    ///
    /// 超时不是错误：数据还在编码器里，下一次 `collect` 会取到。异步 MFT 的
    /// 输出可能比输入晚一两帧，调用方按「提交了几帧就收几帧」记账即可。
    pub fn collect(&mut self, timeout: Duration) -> Result<Option<EncodedFrame>> {
        let inner = self.inner.as_mut().ok_or_else(|| anyhow!("编码器已关闭"))?;
        inner.collect(timeout)
    }

    /// 提交并立即等这一帧的输出（老接口，基准测试和一次性调用用）。
    pub fn encode_bgra(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        self.submit_bgra(bgra, force_key)?;
        self.collect(OUTPUT_WAIT)
    }

    /// 直接编码 NV12（调用方已转好），供基准测试把转换与编码分开计时。
    pub fn encode_nv12(&mut self, nv12: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        self.submit_nv12(nv12, force_key)?;
        self.collect(OUTPUT_WAIT)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            // 先让编码器把内部缓冲吐干净，再释放接口，最后才 MFShutdown。
            unsafe {
                let _ = inner.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                let _ = inner
                    .mft
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            inner.events = None;
            inner.codec = None;
            drop(inner);
        }
        stop_mf(self.com_owned);
    }
}

impl Inner {
    /// 异步 MFT 必须等到 `METransformNeedInput` 才能 `ProcessInput`。
    fn wait_for_input(&mut self) -> Result<()> {
        if self.events.is_none() {
            return Ok(());
        }
        let deadline = Instant::now() + INPUT_WAIT;
        while !self.can_input {
            if self.pump_event()? {
                continue;
            }
            if Instant::now() >= deadline {
                bail!("等待编码器输入槽超时");
            }
            std::thread::sleep(EVENT_POLL);
        }
        self.can_input = false;
        Ok(())
    }

    fn submit(&mut self, nv12: &[u8], force_key: bool) -> Result<()> {
        self.wait_for_input()?;
        if force_key {
            self.force_keyframe();
        }
        let len = nv12.len() as u32;
        if len as usize != nv12_len(self.width as usize, self.height as usize) {
            bail!("NV12 帧长度不对: {len} 字节");
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(len).context("MFCreateMemoryBuffer 失败")?;
            let mut ptr = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .context("锁输入缓冲区失败")?;
            if ptr.is_null() {
                bail!("输入缓冲区地址为空");
            }
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(len)
                .context("设置输入缓冲区长度失败")?;

            let sample = MFCreateSample().context("MFCreateSample 失败")?;
            sample.AddBuffer(&buffer).context("AddBuffer 失败")?;
            sample
                .SetSampleTime(self.sample_time)
                .context("设置时间戳失败")?;
            sample
                .SetSampleDuration(self.frame_duration)
                .context("设置帧时长失败")?;
            self.mft
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput 失败")?;
        }
        self.sample_time += self.frame_duration;
        Ok(())
    }

    fn force_keyframe(&self) {
        let Some(codec) = &self.codec else { return };
        let value = variant_u32(1);
        if let Err(error) = unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value) } {
            tracing::debug!("请求关键帧失败（编码器不支持）: {error}");
        }
    }

    /// 取一帧输出，最多等 `timeout`。
    ///
    /// 返回 `None` 只表示「这段时间里还没好」，输出仍留在编码器里，下次再取。
    fn collect(&mut self, timeout: Duration) -> Result<Option<EncodedFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(data) = self.ready.pop_front() {
                return Ok(Some(encoded_frame(data)));
            }

            if self.events.is_none() {
                // 同步 MFT：ProcessOutput 直接抽，抽不到就是还没好。
                while let Some(bytes) = self.drain_output()? {
                    self.ready.push_back(bytes);
                }
                return Ok(self.ready.pop_front().map(encoded_frame));
            }

            // 异步 MFT：把手头的事件取空，输出会进 ready，输入槽标记进 can_input。
            while self.pump_event()? {}
            if let Some(data) = self.ready.pop_front() {
                return Ok(Some(encoded_frame(data)));
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(EVENT_POLL.min(deadline - now));
        }
    }

    /// 处理一个异步事件；返回是否真的取到了事件。
    fn pump_event(&mut self) -> Result<bool> {
        let Some(events) = &self.events else {
            return Ok(false);
        };
        let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(error) => return Err(error).context("GetEvent 失败"),
        };
        let kind = unsafe { event.GetType() }.context("读取 MFT 事件类型失败")?;
        let status = unsafe { event.GetStatus() }.context("读取 MFT 事件状态失败")?;
        if status.is_err() {
            return Err(anyhow::Error::new(WinError::from_hresult(status)))
                .context("编码器报告事件失败（可能是驱动或会话问题）");
        }
        if kind == METransformNeedInput.0 as u32 {
            self.can_input = true;
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Some(bytes) = self.drain_output()? {
                self.ready.push_back(bytes);
            }
        } else if kind == METransformDrainComplete.0 as u32 {
            // Drain 结束，没有更多输出。
        }
        Ok(true)
    }

    /// 从 MFT 取一个输出样本；`None` 表示当前没有更多输出。
    fn drain_output(&mut self) -> Result<Option<Vec<u8>>> {
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];

        if !self.provides_samples {
            unsafe {
                let buffer = MFCreateMemoryBuffer(self.output_bytes)
                    .context("MFCreateMemoryBuffer（输出）失败")?;
                let sample = MFCreateSample().context("MFCreateSample（输出）失败")?;
                sample.AddBuffer(&buffer).context("输出 AddBuffer 失败")?;
                output[0].pSample = ManuallyDrop::new(Some(sample));
            }
        }

        let mut flags = 0u32;
        let result = unsafe { self.mft.ProcessOutput(0, &mut output, &mut flags) };
        // 不管成功失败，样本与事件都要交回 COM 管理，避免泄漏。
        let sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
        let events = unsafe { ManuallyDrop::take(&mut output[0].pEvents) };
        drop(events);

        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(error) => return Err(error).context("ProcessOutput 失败"),
        }

        let Some(sample) = sample else {
            return Ok(None);
        };
        let mut data = Vec::new();
        unsafe {
            let count = sample.GetBufferCount().context("读取输出缓冲区数量失败")?;
            for index in 0..count {
                let buffer = sample
                    .GetBufferByIndex(index)
                    .context("读取输出缓冲区失败")?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut ptr, None, Some(&mut len))
                    .context("锁输出缓冲区失败")?;
                if !ptr.is_null() && len > 0 {
                    data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                }
                let _ = buffer.Unlock();
            }
        }
        if data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(data))
        }
    }
}

/// 把一帧 Annex-B 数据包装成输出，顺带识别关键帧。
fn encoded_frame(data: Vec<u8>) -> EncodedFrame {
    EncodedFrame {
        keyframe: is_keyframe(&data),
        data,
    }
}

/// 枚举硬件 H.264 编码 MFT（输入 NV12、输出 H.264）。
fn enumerate_hardware() -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE
        | MFT_ENUM_FLAG_SYNCMFT
        | MFT_ENUM_FLAG_ASYNCMFT
        | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx 失败")?;
    }
    let mut list = Vec::new();
    if !activates.is_null() {
        unsafe {
            let slice = std::slice::from_raw_parts(activates, count as usize);
            for activate in slice.iter().flatten() {
                list.push(activate.clone());
            }
            CoTaskMemFree(Some(activates as *const core::ffi::c_void));
        }
    }
    Ok(list)
}

/// MFT 的友好名称（`MFT_FRIENDLY_NAME_Attribute`），只用于日志。
fn mft_name(activate: &IMFActivate) -> String {
    // SAFETY: 只读属性；缓冲区按 GetStringLength 给出的长度分配。
    unsafe {
        let key = &MFT_FRIENDLY_NAME_Attribute as *const _;
        let Ok(length) = activate.GetStringLength(key) else {
            return "未知编码器".to_string();
        };
        let mut buffer = vec![0u16; length as usize + 1];
        let mut written = 0u32;
        if activate
            .GetString(key, &mut buffer, Some(&mut written))
            .is_err()
        {
            return "未知编码器".to_string();
        }
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }
}

/// 配置一个 MFT。失败时调用方会尝试列表里的下一个。
unsafe fn build(
    activate: &IMFActivate,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<Inner> {
    let mft: IMFTransform = activate.ActivateObject().context("ActivateObject 失败")?;
    let attributes: IMFAttributes = mft.GetAttributes().context("读取 MFT 属性失败")?;
    let asynchronous = attributes
        .GetUINT32(&MF_TRANSFORM_ASYNC)
        .map(|value| value != 0)
        .unwrap_or(false);
    if asynchronous {
        attributes
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .context("解锁异步 MFT 失败")?;
    }

    // 编码器必须先设输出类型：输出类型会约束输入类型（分辨率、帧率）。
    let output_type = video_type(MFVideoFormat_H264, width, height, fps)?;
    output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
    output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, AVC_PROFILE_MAIN)?;
    // 告诉解码器/浏览器色彩空间，避免它们按 BT.601 解释导致偏色。
    let _ = output_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = output_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
    let _ = output_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    let _ = output_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 0);
    mft.SetOutputType(0, &output_type, 0)
        .context("设置输出类型（H.264）失败")?;

    let input_type = video_type(MFVideoFormat_NV12, width, height, fps)?;
    mft.SetInputType(0, &input_type, 0)
        .context("设置输入类型（NV12）失败")?;

    let codec: Option<ICodecAPI> = mft.cast().ok();
    if let Some(codec) = &codec {
        // CBR：码率预算是按「每秒多少字节」算的，VBR 会把瞬时帧撑爆单帧上限。
        set_u32(codec, &CODECAPI_AVEncCommonRateControlMode, 0);
        set_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate);
        set_u32(
            codec,
            &CODECAPI_AVEncCommonMaxBitRate,
            bitrate.saturating_mul(3) / 2,
        );
        set_u32(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        set_bool(codec, &CODECAPI_AVEncCommonLowLatency, true);
        set_bool(codec, &CODECAPI_AVEncH264CABACEnable, true);
    }

    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .context("BEGIN_STREAMING 失败")?;
    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .context("START_OF_STREAM 失败")?;

    let info = mft.GetOutputStreamInfo(0).context("读取输出流信息失败")?;
    let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

    let events = if asynchronous {
        Some(
            mft.cast::<IMFMediaEventGenerator>()
                .context("异步 MFT 缺少事件接口")?,
        )
    } else {
        None
    };

    Ok(Inner {
        mft,
        events,
        codec,
        can_input: !asynchronous,
        ready: VecDeque::new(),
        provides_samples,
        output_bytes: info.cbSize.max(OUTPUT_MIN_BYTES),
        sample_time: 0,
        frame_duration: 10_000_000 / i64::from(fps),
        width,
        height,
    })
}

unsafe fn video_type(subtype: GUID, width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    let media_type = MFCreateMediaType().context("MFCreateMediaType 失败")?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
    media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(u64::from(width), u64::from(height)))?;
    media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(u64::from(fps), u64::from(FPS_DEN)))?;
    media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    Ok(media_type)
}

fn pack(high: u64, low: u64) -> u64 {
    (high << 32) | (low & 0xffff_ffff)
}

fn set_u32(codec: &ICodecAPI, key: &GUID, value: u32) {
    let variant = variant_u32(value);
    if let Err(error) = unsafe { codec.SetValue(key, &variant) } {
        tracing::debug!("编码器参数 {key:?} = {value} 不受支持: {error}");
    }
}

fn set_bool(codec: &ICodecAPI, key: &GUID, value: bool) {
    let variant = variant_bool(value);
    if let Err(error) = unsafe { codec.SetValue(key, &variant) } {
        tracing::debug!("编码器参数 {key:?} = {value} 不受支持: {error}");
    }
}

fn variant_u32(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

fn variant_bool(value: bool) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    }
    variant
}

/// 在初始化好 COM/MF 的线程上执行一段代码，结束后清理。
fn with_mf<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let com_owned = start_mf()?;
    let result = f();
    stop_mf(com_owned);
    result
}

/// 初始化当前线程的 COM 单元与 Media Foundation。
///
/// 返回 `true` 表示 COM 单元是本次调用创建的，关闭时需要配对 `CoUninitialize`。
fn start_mf() -> Result<bool> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // RPC_E_CHANGED_MODE：本线程已经以别的单元模型初始化过 COM，仍可继续用 MF。
    let com_owned = hr.is_ok();
    if !com_owned && hr.0 != 0x8001_0106u32 as i32 {
        return Err(anyhow::Error::new(WinError::from_hresult(hr))).context("CoInitializeEx 失败");
    }
    if let Err(error) = unsafe { MFStartup(MF_VERSION, 0) } {
        if com_owned {
            unsafe { CoUninitialize() };
        }
        return Err(error).context("MFStartup 失败");
    }
    Ok(com_owned)
}

fn stop_mf(com_owned: bool) {
    unsafe {
        let _ = MFShutdown();
        if com_owned {
            CoUninitialize();
        }
    }
}

/// Annex-B 里是否含 IDR（类型 5）或 SPS（类型 7）。
///
/// 硬件编码器在关键帧前会带 SPS/PPS；客户端用 SPS 构造 `avc1.xxxxxx` 配置串。
pub fn is_keyframe(data: &[u8]) -> bool {
    walk_nals(data, |nal_type| nal_type == 5 || nal_type == 7)
}

/// 从 Annex-B 关键帧里读 SPS 的 profile/level，拼出 WebCodecs 需要的
/// `avc1.PPCCLL` 配置串。没有 SPS 时返回 `None`。
pub fn codec_string(data: &[u8]) -> Option<String> {
    let mut index = 0usize;
    while index + 3 < data.len() {
        let Some(start) = start_code(&data[index..]) else {
            index += 1;
            continue;
        };
        let nal = index + start;
        if nal + 3 < data.len() && data[nal] & 0x1f == 7 {
            return Some(format!(
                "avc1.{:02X}{:02X}{:02X}",
                data[nal + 1],
                data[nal + 2],
                data[nal + 3]
            ));
        }
        index = nal + 1;
    }
    None
}

fn walk_nals(data: &[u8], mut predicate: impl FnMut(u8) -> bool) -> bool {
    let mut index = 0usize;
    while index + 3 < data.len() {
        let Some(start) = start_code(&data[index..]) else {
            index += 1;
            continue;
        };
        let nal = index + start;
        if nal < data.len() && predicate(data[nal] & 0x1f) {
            return true;
        }
        index = nal + 1;
    }
    false
}

/// 起始码长度（0、0、1 或 0、0、0、1）。
fn start_code(data: &[u8]) -> Option<usize> {
    if data.len() >= 4 && data[0..4] == [0, 0, 0, 1] {
        Some(4)
    } else if data.len() >= 3 && data[0..3] == [0, 0, 1] {
        Some(3)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_detection() {
        let data = [
            0u8, 0, 0, 1, 0x67, 0x4d, 0x40, 0x2a, 0x01, // SPS
            0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, // IDR
        ];
        assert!(is_keyframe(&data));
        assert_eq!(codec_string(&data).as_deref(), Some("avc1.4D402A"));
        // 只剩非 IDR 的 P 帧（类型 1）。
        let p = [0u8, 0, 0, 1, 0x41, 0x9a, 0x00];
        assert!(!is_keyframe(&p));
        assert_eq!(codec_string(&p), None);
    }

    #[test]
    fn three_byte_start_codes() {
        let data = [0u8, 0, 1, 0x65, 0x88];
        assert!(is_keyframe(&data));
        assert_eq!(codec_string(&data), None);
        let sps = [0u8, 0, 1, 0x67, 0x42, 0xc0, 0x1f];
        assert_eq!(codec_string(&sps).as_deref(), Some("avc1.42C01F"));
    }

    /// 真机基准：`cargo test -p screenwall-agent --release h264_bench -- --ignored --nocapture`
    ///
    /// 打印 1080p / 4K 每帧的 BGRA→NV12 转换与硬件编码耗时。没有硬件编码器的
    /// 机器上会跳过（打印原因），不会让普通 `cargo test` 变红。
    #[test]
    #[ignore = "需要显卡硬件编码器；用 --ignored 手动跑"]
    fn h264_bench() {
        // 让 `Encoder::new` 的日志（尤其是"选了哪个 MFT"）能打出来。
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        if !hardware_available() {
            println!("跳过：本机没有可用的硬件 H.264 编码器");
            return;
        }
        for (name, width, height, fps, bitrate) in [
            ("1080p", 1920u32, 1080u32, 60u32, 8_000_000u32),
            ("4K", 3840, 2160, 60, 25_000_000),
        ] {
            let frames = 90usize;
            // 渐变底 + 一条移动的亮带，避免编码器把静止画面优化成近零码率。
            // 预先造好几帧，计时循环里不再做逐像素填充。
            let sources: Vec<Vec<u8>> = (0..3)
                .map(|frame| {
                    let mut bgra = vec![0u8; width as usize * height as usize * 4];
                    fill_test_frame(&mut bgra, width as usize, height as usize, frame);
                    bgra
                })
                .collect();

            // ---- 串行：提交后立刻等这一帧的输出（旧行为）。
            let mut encoder = match Encoder::new(width, height, fps, bitrate) {
                Ok(encoder) => encoder,
                Err(error) => {
                    println!("{name}: 编码器初始化失败: {error:#}");
                    continue;
                }
            };
            let mut nv12 = Vec::new();
            let mut convert = Vec::with_capacity(frames);
            let mut encode = Vec::with_capacity(frames);
            let mut total_bytes = 0usize;
            let mut keyframes = 0usize;
            for frame in 0..frames {
                let bgra = &sources[frame % sources.len()];
                let started = Instant::now();
                bgra_to_nv12(bgra, width as usize, height as usize, &mut nv12);
                convert.push(started.elapsed());
                let started = Instant::now();
                let encoded = encoder
                    .encode_nv12(&nv12, frame == 0)
                    .expect("硬件编码失败");
                encode.push(started.elapsed());
                if let Some(encoded) = encoded {
                    total_bytes += encoded.data.len();
                    if encoded.keyframe {
                        keyframes += 1;
                    }
                }
            }
            // 前几帧包含编码器预热与首帧关键帧，统计时跳过。
            let skip = 5.min(frames / 2);
            let convert_avg = average(&convert[skip..]);
            let encode_avg = average(&encode[skip..]);
            println!(
                "{name} {width}x{height}@{fps}: 转换 {convert_avg:.2} ms/帧, 编码 {encode_avg:.2} ms/帧, \
                 合计 {:.2} ms/帧 (≈ {:.0} fps), 平均 {} KB/帧, 关键帧 {keyframes}",
                convert_avg + encode_avg,
                1000.0 / (convert_avg + encode_avg).max(0.001),
                total_bytes / frames.max(1) / 1024,
            );

            // ---- 流水线：提交本帧后不等，先转换下一帧，再收上一帧的输出。
            // 这正是采集线程现在的跑法，量的是端到端吞吐（含转换）。
            let mut pipelined = match Encoder::new(width, height, fps, bitrate) {
                Ok(encoder) => encoder,
                Err(error) => {
                    println!("{name}: 流水线编码器初始化失败: {error:#}");
                    continue;
                }
            };
            let nv12_len = nv12.len();
            let mut front = vec![0u8; nv12_len];
            let mut back = vec![0u8; nv12_len];
            bgra_to_nv12(&sources[0], width as usize, height as usize, &mut front);
            let started = Instant::now();
            pipelined.submit_nv12(&front, true).expect("流水线提交失败");
            for frame in 1..frames {
                bgra_to_nv12(
                    &sources[frame % sources.len()],
                    width as usize,
                    height as usize,
                    &mut back,
                );
                pipelined.submit_nv12(&back, false).expect("流水线提交失败");
                std::mem::swap(&mut front, &mut back);
                let _ = pipelined.collect(OUTPUT_WAIT).expect("流水线收帧失败");
            }
            let _ = pipelined.collect(OUTPUT_WAIT).expect("流水线收帧失败");
            let per_frame = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
            println!(
                "{name} {width}x{height}@{fps} 流水线（含转换）: {per_frame:.2} ms/帧 \
                 (≈ {:.0} fps)",
                1000.0 / per_frame.max(0.001)
            );
        }
    }

    fn average(values: &[Duration]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.iter().map(|value| value.as_secs_f64()).sum::<f64>() * 1000.0 / values.len() as f64
    }

    fn fill_test_frame(bgra: &mut [u8], width: usize, height: usize, frame: usize) {
        let bar_x = (frame * 17) % width.max(1);
        for y in 0..height {
            for x in 0..width {
                let index = (y * width + x) * 4;
                let in_bar = x >= bar_x && x < bar_x + 240;
                bgra[index] = if in_bar { 40 } else { (x % 256) as u8 };
                bgra[index + 1] = if in_bar { 200 } else { (y % 256) as u8 };
                bgra[index + 2] = if in_bar { 255 } else { ((x + y) % 256) as u8 };
                bgra[index + 3] = 255;
            }
        }
    }
}
