//! 系统声音采集：WASAPI 回环（loopback）。
//!
//! 只采集「正在播放到默认扬声器」的声音，不需要虚拟声卡，也不会把麦克风内容带出去。
//! 采集线程把设备混音格式（通常是 32 位浮点 48 kHz 立体声）转换成
//! 交错小端 16 位 PCM；默认再用 Opus 压成 20 ms 一个包，主控端不支持时回退 PCM。
//!
//! 线程内部自己初始化 COM（MTA），失败或没有音频设备时通过 ready 通道返回错误。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels as OpusChannels, SampleRate as OpusRate};
use screenwall_proto::{AudioCodec, AudioSource};

use crate::aec::{self, Aec};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

/// 每次从回环缓冲取数据的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// 单个上行音频块的时长。
const CHUNK_MS: u64 = 20;
/// 混音格式最多支持到 8 声道，再大就不再往外推。
const MAX_CHANNELS: u16 = 8;
/// Opus 固定 48 kHz，20 ms 一帧。
const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME_SAMPLES: usize = (OPUS_RATE as usize * CHUNK_MS as usize) / 1000;
/// 立体声默认 64 kbps、单声道 48 kbps；自适应范围 24–96 kbps。
const OPUS_STEREO_BITRATE: u32 = 64_000;
const OPUS_MONO_BITRATE: u32 = 48_000;

const WAVE_FORMAT_PCM: u32 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;
const WAVE_FORMAT_EXTENSIBLE: u32 = 0xFFFE;

/// 实际采集格式，交给上层写进 `audio.sub.ok`。
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// Opus 的初始码率；PCM 为 0。
    pub bitrate: u32,
    /// 实际采集来源（系统回环 / 麦克风）。
    pub source: AudioSource,
}

/// 采集线程句柄。Drop 时通知线程退出。
pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
}

#[derive(Clone, Copy)]
struct MixFormat {
    sample_rate: u32,
    channels: u16,
    kind: SampleKind,
}

impl MixFormat {
    fn bytes_per_sample(self) -> usize {
        match self.kind {
            SampleKind::F32 => 4,
            SampleKind::I16 => 2,
        }
    }

    fn chunk_bytes(self) -> usize {
        let samples = self.sample_rate as usize * self.channels as usize * CHUNK_MS as usize / 1000;
        (samples * 2).max(640)
    }
}

/// 启动回环采集。返回句柄与格式；没有默认输出设备或格式不支持时返回错误。
///
/// `wanted` 是主控端希望的编码；Opus 编码器初始化失败会自动回退 PCM，
/// 实际的编码通过返回的 `AudioFormat` 和每一块回调告知。
pub fn spawn(
    wanted: AudioCodec,
    source: AudioSource,
    bitrate: Arc<AtomicU32>,
    sink: impl Fn(Vec<u8>, AudioCodec) + Send + 'static,
) -> Result<(AudioHandle, AudioFormat)> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<AudioFormat, String>>();

    let thread = std::thread::Builder::new()
        .name("audio-capture".to_string())
        .spawn(move || {
            let mut ready_tx = Some(ready_tx);
            match run(&thread_stop, &sink, &mut ready_tx, wanted, source, &bitrate) {
                Ok(()) => {}
                Err(error) => {
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("{error:#}")));
                    }
                    tracing::warn!("系统声音采集结束: {error:#}");
                }
            }
        })
        .context("创建声音采集线程失败")?;

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(format)) => Ok((
            AudioHandle {
                stop,
                _thread: Some(thread),
            },
            format,
        )),
        Ok(Err(message)) => {
            let _ = thread.join();
            Err(anyhow::anyhow!(message))
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(anyhow::anyhow!("声音采集启动超时"))
        }
    }
}

fn run(
    stop: &AtomicBool,
    sink: &impl Fn(Vec<u8>, AudioCodec),
    ready: &mut Option<std::sync::mpsc::Sender<Result<AudioFormat, String>>>,
    wanted: AudioCodec,
    source: AudioSource,
    bitrate: &Arc<AtomicU32>,
) -> Result<()> {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    com.ok().context("初始化 COM 失败")?;

    let result = capture_loop(stop, sink, ready, wanted, source, bitrate);
    unsafe { CoUninitialize() };
    result
}

fn capture_loop(
    stop: &AtomicBool,
    sink: &impl Fn(Vec<u8>, AudioCodec),
    ready: &mut Option<std::sync::mpsc::Sender<Result<AudioFormat, String>>>,
    wanted: AudioCodec,
    source: AudioSource,
    bitrate: &Arc<AtomicU32>,
) -> Result<()> {
    // 开关由环境变量初始化一次；日志里会打印最终状态。
    aec::init_from_env();

    let (client, format) = open_client(source)?;
    let capture: IAudioCaptureClient =
        unsafe { client.GetService() }.context("获取音频采集接口失败")?;

    let mix = format;
    if mix.channels == 0 || mix.channels > MAX_CHANNELS || mix.sample_rate == 0 {
        bail!(
            "不支持的音频格式: {} Hz / {} 声道",
            mix.sample_rate,
            mix.channels
        );
    }
    let mut opus = if wanted == AudioCodec::Opus {
        match OpusEncoder::new(mix, bitrate) {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                tracing::warn!(%error, "初始化 Opus 编码器失败，回退 PCM");
                None
            }
        }
    } else {
        None
    };
    let public = match opus.as_ref() {
        Some(encoder) => encoder.format(source),
        None => AudioFormat {
            codec: AudioCodec::Pcm16,
            sample_rate: mix.sample_rate,
            channels: mix.channels as u8,
            bitrate: 0,
            source,
        },
    };
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok(public));
    }
    tracing::info!(
        codec = ?public.codec,
        source = ?public.source,
        sample_rate = public.sample_rate,
        channels = public.channels,
        bitrate = public.bitrate,
        "声音采集已启动"
    );

    unsafe { client.Start() }.context("启动音频采集失败")?;

    // 麦克风 + 开关打开时，额外开一路系统回环作为 AEC 的远端参考。
    // 回环打不开只降级为直通（aec 保持 None），绝不影响麦克风本身。
    let mut echo: Option<EchoReference> = if source == AudioSource::Microphone && aec::enabled() {
        match EchoReference::open(mix) {
            Ok(reference) => Some(reference),
            Err(error) => {
                tracing::warn!("打开回环参考失败，AEC 降级为直通: {error:#}");
                None
            }
        }
    } else {
        None
    };

    let chunk_bytes = mix.chunk_bytes();
    let mut pending: Vec<u8> = Vec::with_capacity(chunk_bytes * 2);
    let mut source: Vec<i16> = Vec::new();
    let result = 'capture: loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);

        loop {
            let packet_frames = match unsafe { capture.GetNextPacketSize() } {
                Ok(frames) => frames,
                Err(error) => break 'capture Err(anyhow::anyhow!("读取音频包大小失败: {error}")),
            };
            if packet_frames == 0 {
                break;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if let Err(error) =
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            {
                break 'capture Err(anyhow::anyhow!("读取音频数据失败: {error}"));
            }

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            source.clear();
            if silent || data.is_null() || frames == 0 {
                source.resize(frames as usize * mix.channels as usize, 0);
            } else {
                let len = frames as usize * mix.channels as usize * mix.bytes_per_sample();
                let bytes = unsafe { std::slice::from_raw_parts(data, len) };
                decode_samples(&mut source, bytes, mix);
            }
            let _ = unsafe { capture.ReleaseBuffer(frames) };

            // 回声消除：回环参考与麦克风按块对齐后处理，参考只喂 AEC，不编码。
            // 处理结果是交错格式，声道数与麦克风混音格式一致，直接走原编码路径。
            if let Some(reference) = echo.as_mut() {
                reference.push_mic(&source, mix);
                let processed = reference.process();
                if !processed.is_empty() {
                    if let Some(encoder) = opus.as_mut() {
                        if let Err(error) = encoder.feed(&processed, mix.channels as usize, sink) {
                            break 'capture Err(error);
                        }
                    } else {
                        for sample in &processed {
                            pending.extend_from_slice(&sample.to_le_bytes());
                        }
                    }
                }
            } else if let Some(encoder) = opus.as_mut() {
                if let Err(error) = encoder.feed(&source, mix.channels as usize, sink) {
                    break 'capture Err(error);
                }
            } else {
                for sample in &source {
                    pending.extend_from_slice(&sample.to_le_bytes());
                }
            }
        }

        if opus.is_none() {
            while pending.len() >= chunk_bytes {
                let rest = pending.split_off(chunk_bytes);
                let chunk = std::mem::replace(&mut pending, rest);
                sink(chunk, AudioCodec::Pcm16);
            }
        }
    };

    let _ = unsafe { client.Stop() };
    tracing::info!("系统声音采集已停止");
    result
}

/// AEC 的远端参考：一路系统回环采集，加一个自适应消除器。
///
/// 回环和麦克风的混音格式可能不同（采样率、声道数），这里统一下混成单声道，
/// 再把回环重采样到麦克风的采样率。处理后的单声道结果会按麦克风的原声道数
/// 复制回交错格式，保证编码器的输入布局和不开 AEC 时完全一致。
struct EchoReference {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// 回环设备的混音格式（解码用）。
    mix: MixFormat,
    /// 麦克风的声道数（输出交错格式）。
    channels: usize,
    /// 下混后的单声道缓冲。
    mono: Vec<i16>,
    /// 重采样到麦克风采样率后的参考。
    resampler: Option<LinearResampler>,
    reference: Vec<i16>,
    /// 攒够一块才一起喂给 AEC，保持和麦克风数据同粒度。
    mic_pending: Vec<i16>,
    /// 每次处理的麦克风样本数（单声道）。
    block: usize,
    aec: Aec,
    /// 参考队列的软上限，超过就丢弃最旧的，避免时钟漂移导致无限积压。
    cap: usize,
    /// 回环读取失败后是否已经打过日志，避免每 10 ms 刷屏。
    pull_warned: bool,
}

impl Drop for EchoReference {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

impl EchoReference {
    fn open(mic: MixFormat) -> Result<Self> {
        let (client, mix) = open_client(AudioSource::Loopback).context("打开回环参考")?;
        if mix.channels == 0 || mix.channels > MAX_CHANNELS || mix.sample_rate == 0 {
            bail!(
                "回环参考格式不支持: {} Hz / {} 声道",
                mix.sample_rate,
                mix.channels
            );
        }
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("获取回环参考采集接口失败")?;
        unsafe { client.Start() }.context("启动回环参考采集失败")?;

        let rate = mic.sample_rate;
        let resampler = if mix.sample_rate != rate {
            Some(LinearResampler::new(mix.sample_rate, rate, 1))
        } else {
            None
        };
        // 5 ms 一块，兼顾实时性和开销；AEC 内部还会再按 64 样本分块。
        let block = (rate as usize / 200).max(64);
        let aec = Aec::new(rate);
        tracing::info!(
            ref_rate = mix.sample_rate,
            ref_channels = mix.channels,
            mic_rate = rate,
            mic_channels = mic.channels,
            block,
            "AEC 参考信号已接入（回环 → 麦克风）"
        );
        Ok(Self {
            client,
            capture,
            mix,
            channels: mic.channels as usize,
            mono: Vec::new(),
            resampler,
            reference: Vec::new(),
            mic_pending: Vec::with_capacity(block * 4),
            block,
            aec,
            cap: rate as usize / 2,
            pull_warned: false,
        })
    }

    /// 读走回环里当前所有包，转成与麦克风同采样率的单声道参考。
    fn pull(&mut self) -> Result<()> {
        loop {
            let frames = match unsafe { self.capture.GetNextPacketSize() } {
                Ok(frames) => frames,
                Err(error) => bail!("读取回环参考包大小失败: {error}"),
            };
            if frames == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut count = 0u32;
            let mut flags = 0u32;
            if let Err(error) = unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut count, &mut flags, None, None)
            } {
                bail!("读取回环参考数据失败: {error}");
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let mut samples = Vec::new();
            if silent || data.is_null() || count == 0 {
                samples.resize(count as usize * self.mix.channels as usize, 0);
            } else {
                let len = count as usize * self.mix.channels as usize * self.mix.bytes_per_sample();
                let bytes = unsafe { std::slice::from_raw_parts(data, len) };
                decode_samples(&mut samples, bytes, self.mix);
            }
            let _ = unsafe { self.capture.ReleaseBuffer(count) };

            self.mono.clear();
            let channels = self.mix.channels as usize;
            for frame in samples.chunks_exact(channels) {
                let sum: i32 = frame.iter().map(|&sample| sample as i32).sum();
                self.mono.push((sum / channels as i32) as i16);
            }
            match self.resampler.as_mut() {
                Some(resampler) => resampler.push(&self.mono, &mut self.reference),
                None => self.reference.extend_from_slice(&self.mono),
            }
        }
    }

    /// 麦克风这一块到达时，先把参考读到同一时刻，再凑够一块处理。
    fn push_mic(&mut self, mic: &[i16], mix: MixFormat) {
        // 参考读取失败不致命：这一块用静音补，下一轮继续尝试。
        match self.pull() {
            Ok(()) => self.pull_warned = false,
            Err(error) => {
                if !self.pull_warned {
                    self.pull_warned = true;
                    tracing::warn!("读取回环参考失败（本块按静音处理）: {error:#}");
                }
            }
        }
        if self.reference.len() > self.cap {
            let drop = self.reference.len() - self.cap;
            self.reference.drain(..drop);
        }

        let channels = mix.channels as usize;
        self.mic_pending.reserve(mic.len() / channels.max(1));
        for frame in mic.chunks_exact(channels) {
            let sum: i32 = frame.iter().map(|&sample| sample as i32).sum();
            self.mic_pending.push((sum / channels as i32) as i16);
        }
    }

    /// 凑够一块就处理一块，返回交错格式（与原麦克风声道数一致）的样本。
    fn process(&mut self) -> Vec<i16> {
        let channels = self.channels;
        let mut out = Vec::with_capacity(self.mic_pending.len() * channels);
        let mut block = vec![0i16; self.block];
        while self.mic_pending.len() >= self.block {
            // 参考一般和麦克风同步到达；短暂落后时先等一下，别急着补 0，
            // 否则一次错位会一直错下去。积压超过 200 ms 才补 0 强行追上。
            if self.reference.len() < self.block && self.mic_pending.len() < self.cap {
                break;
            }
            let rest = self.mic_pending.split_off(self.block);
            let chunk = std::mem::replace(&mut self.mic_pending, rest);

            // 参考与麦克风等长；不够就尾部补 0（回环空闲或已经落后太多）。
            let have = self.reference.len().min(self.block);
            block[..have].copy_from_slice(&self.reference[..have]);
            block[have..].fill(0);
            self.reference.drain(..have);

            // AEC 内部按自己的分块节奏处理，返回长度可能略少于输入；
            // 少掉的部分会在下一次调用继续，不丢样本。
            for sample in self.aec.process(&block, &chunk) {
                for _ in 0..channels {
                    out.push(sample);
                }
            }
        }
        out
    }
}

/// 把采集到的 PCM 编码成 20 ms 一个 Opus 包。
struct OpusEncoder {
    encoder: Encoder,
    channels: u16,
    resampler: Option<LinearResampler>,
    samples: Vec<i16>,
    target: Arc<AtomicU32>,
    applied: u32,
    packet: Vec<u8>,
}

impl OpusEncoder {
    fn new(mix: MixFormat, target: &Arc<AtomicU32>) -> Result<Self> {
        let channels = mix.channels.min(2);
        let initial = if channels == 2 {
            OPUS_STEREO_BITRATE
        } else {
            OPUS_MONO_BITRATE
        };
        target.store(initial, Ordering::Relaxed);
        let mut encoder = Encoder::new(
            OpusRate::Hz48000,
            if channels == 2 {
                OpusChannels::Stereo
            } else {
                OpusChannels::Mono
            },
            Application::Audio,
        )
        .context("创建 Opus 编码器失败")?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(initial as i32))
            .context("设置 Opus 码率失败")?;
        let resampler = if mix.sample_rate != OPUS_RATE {
            Some(LinearResampler::new(
                mix.sample_rate,
                OPUS_RATE,
                channels as usize,
            ))
        } else {
            None
        };
        Ok(Self {
            encoder,
            channels,
            resampler,
            samples: Vec::with_capacity(OPUS_FRAME_SAMPLES * 4),
            target: target.clone(),
            applied: initial,
            // 960 个立体声样本最多 4 字节/样本的保守上限。
            packet: vec![0u8; OPUS_FRAME_SAMPLES * 4],
        })
    }

    fn format(&self, source: AudioSource) -> AudioFormat {
        AudioFormat {
            codec: AudioCodec::Opus,
            sample_rate: OPUS_RATE,
            channels: self.channels as u8,
            bitrate: self.applied,
            source,
        }
    }

    /// 输入源格式的交错 i16，必要时下混到 1/2 声道并重采样到 48 kHz。
    fn feed(
        &mut self,
        input: &[i16],
        source_channels: usize,
        sink: &impl Fn(Vec<u8>, AudioCodec),
    ) -> Result<()> {
        let mixed = downmix(input, source_channels, self.channels as usize);
        match self.resampler.as_mut() {
            Some(resampler) => resampler.push(&mixed, &mut self.samples),
            None => self.samples.extend_from_slice(&mixed),
        }
        let frame_len = OPUS_FRAME_SAMPLES * self.channels as usize;
        while self.samples.len() >= frame_len {
            let rest = self.samples.split_off(frame_len);
            let frame = std::mem::replace(&mut self.samples, rest);
            let target = self.target.load(Ordering::Relaxed);
            if target > 0 && target != self.applied {
                self.encoder
                    .set_bitrate(Bitrate::BitsPerSecond(target as i32))
                    .context("调整 Opus 码率失败")?;
                self.applied = target;
            }
            let len = self
                .encoder
                .encode(&frame, &mut self.packet)
                .context("Opus 编码失败")?;
            sink(self.packet[..len].to_vec(), AudioCodec::Opus);
        }
        Ok(())
    }
}

/// 线性插值重采样。WASAPI 混音通常是 48 kHz，只有 44.1 kHz 等设备会走到这里。
struct LinearResampler {
    step: f64,
    pos: f64,
    pending: Vec<i16>,
    channels: usize,
}

impl LinearResampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            pending: Vec::new(),
            channels,
        }
    }

    fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let channels = self.channels;
        self.pending.extend_from_slice(input);
        let frames = self.pending.len() / channels;
        let mut pos = self.pos;
        while pos + 1.0 < frames as f64 {
            let index = pos.floor() as usize;
            let frac = (pos - index as f64) as f32;
            for channel in 0..channels {
                let a = self.pending[index * channels + channel] as f32;
                let b = self.pending[(index + 1) * channels + channel] as f32;
                out.push((a + (b - a) * frac).round() as i16);
            }
            pos += self.step;
        }
        let consumed = pos.floor() as usize;
        if consumed > 0 {
            self.pending.drain(..consumed * channels);
            pos -= consumed as f64;
        }
        self.pos = pos;
    }
}

/// 声道下混：单声道取所有声道平均，双声道取前两个声道。
fn downmix(input: &[i16], source_channels: usize, target_channels: usize) -> Vec<i16> {
    if source_channels == 0 {
        return Vec::new();
    }
    if source_channels == target_channels {
        return input.to_vec();
    }
    let frames = input.len() / source_channels;
    let mut out = Vec::with_capacity(frames * target_channels);
    for frame in 0..frames {
        let base = frame * source_channels;
        if target_channels == 1 {
            let sum: i32 = (0..source_channels)
                .map(|channel| input[base + channel] as i32)
                .sum();
            out.push((sum / source_channels as i32) as i16);
        } else {
            out.push(input[base]);
            out.push(input[base + 1]);
        }
    }
    out
}

/// 打开默认输出设备的回环采集或默认麦克风，返回客户端和混音格式。
/// 格式指针读完立即释放。
fn open_client(source: AudioSource) -> Result<(IAudioClient, MixFormat)> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("创建音频设备枚举器失败")?;
    let (device, flags, what) = match source {
        AudioSource::Loopback => (
            unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .context("没有可用的默认扬声器")?,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            "系统声音",
        ),
        AudioSource::Microphone => (
            unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }
                .context("没有可用的默认麦克风")?,
            0,
            "麦克风",
        ),
    };
    let client: IAudioClient =
        unsafe { device.Activate(CLSCTX_ALL, None) }.context("激活音频客户端失败")?;

    let wave: *mut WAVEFORMATEX = unsafe { client.GetMixFormat() }.context("读取混音格式失败")?;
    if wave.is_null() {
        bail!("{what}混音格式为空");
    }
    let mix = unsafe { read_format(wave) };

    // 共享模式必须用设备混音格式；缓冲区给 200ms，块大小交给系统决定。
    // Initialize 需要读这块内存，必须在 CoTaskMemFree 之前调用。
    let init =
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, wave, None) };
    unsafe { CoTaskMemFree(Some(wave as *const _)) };

    init.with_context(|| format!("初始化{what}采集失败"))?;
    Ok((client, mix?))
}

/// 读取默认输出设备的音量与静音状态。
pub fn read_volume() -> Result<(f32, bool)> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .ok();
    let result = (|| {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("创建音频设备枚举器失败")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("没有可用的默认扬声器")?;
        let volume: IAudioEndpointVolume =
            unsafe { device.Activate(CLSCTX_ALL, None) }.context("打开音量控制失败")?;
        let level = unsafe { volume.GetMasterVolumeLevelScalar() }.context("读取音量失败")?;
        let muted = unsafe { volume.GetMute() }.context("读取静音状态失败")?;
        Ok((level, muted.as_bool()))
    })();
    unsafe { CoUninitialize() };
    result
}

/// 设置默认输出设备的音量（0.0–1.0）和/或静音。
pub fn write_volume(level: Option<f32>, muted: Option<bool>) -> Result<(f32, bool)> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .ok();
    let result = (|| {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("创建音频设备枚举器失败")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("没有可用的默认扬声器")?;
        let volume: IAudioEndpointVolume =
            unsafe { device.Activate(CLSCTX_ALL, None) }.context("打开音量控制失败")?;
        if let Some(level) = level {
            let level = level.clamp(0.0, 1.0);
            unsafe { volume.SetMasterVolumeLevelScalar(level, std::ptr::null()) }
                .context("设置音量失败")?;
        }
        if let Some(muted) = muted {
            unsafe { volume.SetMute(muted, std::ptr::null()) }.context("设置静音失败")?;
        }
        let level = unsafe { volume.GetMasterVolumeLevelScalar() }.context("读取音量失败")?;
        let muted = unsafe { volume.GetMute() }.context("读取静音状态失败")?;
        Ok((level, muted.as_bool()))
    })();
    unsafe { CoUninitialize() };
    result
}

/// 解析 `WAVEFORMATEX`；`WAVE_FORMAT_EXTENSIBLE` 看子格式 GUID 的 data1。
unsafe fn read_format(wave: *const WAVEFORMATEX) -> Result<MixFormat> {
    let base = unsafe { *wave };
    let tag = base.wFormatTag as u32;
    let kind = match tag {
        WAVE_FORMAT_IEEE_FLOAT => SampleKind::F32,
        WAVE_FORMAT_PCM => SampleKind::I16,
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = unsafe { &*(wave as *const WAVEFORMATEXTENSIBLE) };
            match ext.SubFormat.data1 {
                3 => SampleKind::F32,
                1 => SampleKind::I16,
                other => bail!("不支持的音频子格式: {other:#x}"),
            }
        }
        other => bail!("不支持的音频格式: {other:#x}"),
    };
    let bits = base.wBitsPerSample;
    match (kind, bits) {
        (SampleKind::F32, 32) | (SampleKind::I16, 16) => {}
        _ => bail!("不支持的音频位宽: {bits}"),
    }
    if base.nChannels == 0 {
        bail!("音频声道数为 0");
    }
    Ok(MixFormat {
        sample_rate: base.nSamplesPerSec,
        channels: base.nChannels,
        kind,
    })
}

/// 把混音数据转成交错 i16；多余字节忽略。
fn decode_samples(out: &mut Vec<i16>, data: &[u8], format: MixFormat) {
    let samples = data.len() / format.bytes_per_sample();
    match format.kind {
        SampleKind::I16 => {
            out.extend(
                data[..samples * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|chunk| i16::from_le_bytes(*chunk)),
            );
        }
        SampleKind::F32 => {
            out.reserve(samples);
            for chunk in data[..samples * 4].as_chunks::<4>().0 {
                let value = f32::from_le_bytes(*chunk);
                let clamped = value.clamp(-1.0, 1.0);
                let sample = if clamped.is_finite() {
                    (clamped * 32767.0).round() as i16
                } else {
                    0
                };
                out.push(sample);
            }
        }
    }
}

/// 把混音数据转成交错小端 i16 字节；单测用它模拟旧 PCM 路径。
#[cfg(test)]
fn append_pcm(out: &mut Vec<u8>, data: &[u8], format: MixFormat) {
    let mut samples = Vec::new();
    decode_samples(&mut samples, data, format);
    out.reserve(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(kind: SampleKind) -> MixFormat {
        MixFormat {
            sample_rate: 48_000,
            channels: 2,
            kind,
        }
    }

    #[test]
    fn converts_float_to_i16() {
        let mut out = Vec::new();
        let mut data = Vec::new();
        for value in [-1.5f32, -1.0, 0.0, 0.5, 1.0, 2.0] {
            data.extend_from_slice(&value.to_le_bytes());
            data.extend_from_slice(&0f32.to_le_bytes());
        }
        append_pcm(&mut out, &data, format(SampleKind::F32));
        let samples: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| i16::from_le_bytes(*chunk))
            .collect();
        assert_eq!(
            samples,
            vec![-32767, 0, -32767, 0, 0, 0, 16384, 0, 32767, 0, 32767, 0]
        );
    }

    #[test]
    fn passes_pcm16_through() {
        let mut out = Vec::new();
        append_pcm(&mut out, &[0x01, 0x00, 0xFF, 0xFF], format(SampleKind::I16));
        assert_eq!(out, vec![0x01, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn chunk_size_is_twenty_ms() {
        let mix = format(SampleKind::I16);
        assert_eq!(mix.chunk_bytes(), 48_000 * 2 * 2 * 20 / 1000);
    }

    #[test]
    fn downmix_averages_mono_and_keeps_first_stereo() {
        let stereo = [100i16, 300, -100, -300];
        assert_eq!(downmix(&stereo, 2, 1), vec![200, -200]);
        let surround = [1i16, 2, 3, 4, 5, 6];
        assert_eq!(downmix(&surround, 6, 2), vec![1, 2]);
    }

    #[test]
    fn resampler_44100_to_48000_keeps_length() {
        let mut resampler = LinearResampler::new(44_100, 48_000, 2);
        let input: Vec<i16> = (0..4410).flat_map(|i| [i as i16, i as i16]).collect();
        let mut out = Vec::new();
        resampler.push(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 4800).abs() <= 2, "frames={frames}");
    }

    #[test]
    fn opus_encodes_twenty_ms_and_decodes_back() {
        let target = Arc::new(AtomicU32::new(0));
        let mut encoder = OpusEncoder::new(format(SampleKind::I16), &target).unwrap();
        let mut frame = Vec::with_capacity(OPUS_FRAME_SAMPLES * 2);
        for index in 0..OPUS_FRAME_SAMPLES {
            let value = ((index as f32 / OPUS_RATE as f32 * 440.0 * std::f32::consts::TAU).sin()
                * 8_000.0) as i16;
            frame.push(value);
            frame.push(value);
        }
        let packets = std::sync::Mutex::new(Vec::new());
        encoder
            .feed(&frame, 2, &|packet, codec| {
                assert_eq!(codec, AudioCodec::Opus);
                packets.lock().unwrap().push(packet);
            })
            .unwrap();
        let packets = packets.into_inner().unwrap();
        assert_eq!(packets.len(), 1);
        assert!(
            packets[0].len() < OPUS_FRAME_SAMPLES * 4,
            "packet={}",
            packets[0].len()
        );

        let mut decoder =
            audiopus::coder::Decoder::new(OpusRate::Hz48000, OpusChannels::Stereo).unwrap();
        let mut pcm = vec![0i16; OPUS_FRAME_SAMPLES * 2];
        let decoded = decoder
            .decode(Some(&packets[0][..]), &mut pcm[..], false)
            .unwrap();
        assert_eq!(decoded, OPUS_FRAME_SAMPLES);
    }
}
