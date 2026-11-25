//! 麦克风回声消除（AEC）。
//!
//! 用系统回环（扬声器实际播放的内容）作为远端参考信号，对麦克风信号做自适应
//! 回声消除。纯 Rust 实现，不用 unsafe，也不额外引入依赖：
//!
//! * 延迟估计：抽取到约 8 kHz 做归一化互相关粗搜，再回全速率局部细化；
//! * 延迟对齐：参考信号整体延后估计出的延迟，自适应滤波器只建模
//!   扬声器→麦克风的短残余路径；
//! * 自适应滤波：时域分块 NLMS（归一化最小均方），覆盖约 100 ms；
//! * 双讲检测：麦克风能量显著高于估计回声时冻结自适应，但仍做减除；
//! * 残留回声抑制：按估计回声能量对残差做功率域增益，双讲时旁路。
//!
//! 回环参考打不开时由调用方把 AEC 整体旁路，麦克风继续直通工作。

#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

/// 进程级开关：默认开启，由 [`init_from_env`] 从 `SCREENWALL_AEC` 初始化一次。
static ENABLED: AtomicBool = AtomicBool::new(true);
static INIT: Once = Once::new();

/// 读取环境变量 `SCREENWALL_AEC` 初始化开关；`0` / `false` / `off` / `no` 视为关闭。
/// 重复调用只有第一次生效。
pub fn init_from_env() {
    INIT.call_once(|| {
        let value = std::env::var("SCREENWALL_AEC")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase());
        let on = !matches!(value.as_deref(), Some("0" | "false" | "off" | "no"));
        ENABLED.store(on, Ordering::Relaxed);
        tracing::info!(
            enabled = on,
            "麦克风回声消除(AEC){}",
            if on {
                "已启用"
            } else {
                "已关闭（环境变量 SCREENWALL_AEC=0）"
            }
        );
    });
}

/// 运行时开关，主要给测试和调试用。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 滤波器覆盖的回声拖尾时长（毫秒）。48 kHz 下即 4800 taps。
const FILTER_MS: usize = 100;
/// 延迟搜索上限（毫秒）。
pub const MAX_DELAY_MS: usize = 300;
/// 双讲判定：麦克风能量超过估计回声的倍数。
const DTD_RATIO: f32 = 4.0;
/// 双讲判定：ERLE 超过这个值才认为滤波器已收敛，避免启动阶段误判。
const DTD_ERLE_DB: f32 = 6.0;
/// 双讲判定用的能量下限（归一化幅度，均方）。
const DTD_FLOOR: f32 = 1e-6;
/// 双讲解除后保持的块数（约 13 ms），避免判决来回抖动。
const DT_HOLD_BLOCKS: u8 = 10;
/// 残留回声抑制的泄漏系数：认为估计回声里还有这么多比例没被抵消。
const RES_LEAK: f32 = 0.5;
/// 残留抑制的最大衰减（-20 dB）。
const GAIN_MIN: f32 = 0.1;
/// NLMS 分块大小（约 1.3 ms @ 48 kHz）。
const BLOCK: usize = 64;
/// NLMS 步长。
const STEP: f32 = 0.5;
/// 归一化：i16 → [-1, 1]。
const INV_I16: f32 = 1.0 / 32768.0;
/// ERLE 日志间隔。
const LOG_INTERVAL: Duration = Duration::from_secs(10);
const EPS: f32 = 1e-9;

/// 用互相关估计「麦克风相对参考信号」的延迟，返回采样点数。
///
/// `reference` 是远端参考（回环），`mic` 是麦克风；两者从同一时刻开始、长度相近。
/// 返回 `None` 表示参考太安静或相关性不足，暂时无法确定延迟。
pub fn estimate_delay(
    reference: &[i16],
    mic: &[i16],
    sample_rate: u32,
    max_delay_ms: u32,
) -> Option<usize> {
    let rate = sample_rate.max(8_000) as usize;
    // 抽取到约 8 kHz 做粗搜，精度由后面的全速率细化保证。
    let dec = (rate / 8_000).max(1);
    let dec_rate = rate / dec;
    let max_lag = max_delay_ms.max(1) as usize * dec_rate / 1000;
    let min_window = dec_rate / 8;
    if max_lag == 0 || mic.len() <= dec * (max_lag + min_window) {
        return None;
    }

    // 抽取前先做分块平均（简易抗混叠低通）：直接跳采样会把非整数延迟
    // 变成完全不相干的噪声，粗搜就失效了。平均带来的群延迟两边相同，不影响相对延迟。
    let ref_dec = decimate_mean(reference, dec);
    let mic_dec = decimate_mean(mic, dec);
    if mic_dec.len() <= max_lag + min_window {
        return None;
    }

    // 相关窗口最多 1.5 秒，且两边都要覆盖到最大延迟。
    let window = (dec_rate * 3 / 2)
        .min(ref_dec.len())
        .min(mic_dec.len() - max_lag);
    if window < min_window {
        return None;
    }

    // 去直流：参考取窗口均值，麦克风取整个搜索范围的均值。
    let ref_mean = ref_dec[..window].iter().sum::<f64>() / window as f64;
    let mic_span = window + max_lag;
    let mic_mean = mic_dec[..mic_span].iter().sum::<f64>() / mic_span as f64;
    let r: Vec<f64> = ref_dec[..window].iter().map(|v| v - ref_mean).collect();
    let m: Vec<f64> = mic_dec[..mic_span].iter().map(|v| v - mic_mean).collect();

    let energy_ref: f64 = r.iter().map(|v| v * v).sum();
    // 参考太安静（均方不到 4，约 -78 dBFS）就别估了，否则容易锁到噪声上。
    if energy_ref < window as f64 * 4.0 {
        return None;
    }
    let mut energy_mic: f64 = m[..window].iter().map(|v| v * v).sum();
    if energy_mic < window as f64 * 4.0 {
        return None;
    }

    let mut norms = vec![0.0f64; max_lag + 1];
    let mut best = 0usize;
    let mut best_norm = 0.0f64;
    for lag in 0..=max_lag {
        let mut dot = 0.0f64;
        for i in 0..window {
            dot += r[i] * m[i + lag];
        }
        let norm = if energy_ref > 0.0 && energy_mic > 0.0 {
            (dot / (energy_ref * energy_mic).sqrt()).abs()
        } else {
            0.0
        };
        norms[lag] = norm;
        if norm > best_norm {
            best_norm = norm;
            best = lag;
        }
        if lag < max_lag {
            // 滑动窗口能量：去掉最旧的、加上最新的。
            energy_mic += m[lag + window] * m[lag + window] - m[lag] * m[lag];
            energy_mic = energy_mic.max(1e-9);
        }
    }

    // 相关性太低说明没有回声，或者参考根本不是回声来源。
    if best_norm < 0.3 {
        return None;
    }

    // 抛物线插值取亚采样峰值。
    let mut refined = best as f64;
    if best > 0 && best < max_lag {
        let (a, b, c) = (norms[best - 1], norms[best], norms[best + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-12 {
            refined += 0.5 * (a - c) / denom;
        }
    }
    let center = (refined * dec as f64).round().max(0.0) as usize;

    // 回到全速率在粗估附近逐采样细化，保证误差远小于 2 ms。
    let span = 2 * dec + 2;
    let low = center.saturating_sub(span);
    let high = center + span;
    let full_window = (rate / 2).min(reference.len());
    if high + full_window > mic.len() || full_window < rate / 10 {
        return Some(center);
    }

    let ref_mean = reference[..full_window]
        .iter()
        .map(|&v| v as f64)
        .sum::<f64>()
        / full_window as f64;
    let mic_mean = mic[low..low + full_window]
        .iter()
        .map(|&v| v as f64)
        .sum::<f64>()
        / full_window as f64;
    let mut best_full = center;
    let mut best_full_norm = 0.0f64;
    for lag in low..=high {
        let mut dot = 0.0f64;
        let mut energy_ref = 0.0f64;
        let mut energy_mic = 0.0f64;
        for i in 0..full_window {
            let r = reference[i] as f64 - ref_mean;
            let m = mic[i + lag] as f64 - mic_mean;
            dot += r * m;
            energy_ref += r * r;
            energy_mic += m * m;
        }
        let norm = if energy_ref > 0.0 && energy_mic > 0.0 {
            (dot / (energy_ref * energy_mic).sqrt()).abs()
        } else {
            0.0
        };
        if norm > best_full_norm {
            best_full_norm = norm;
            best_full = lag;
        }
    }
    Some(best_full)
}

/// 按 `dec` 个样本一组取均值再抽取，得到约 `rate / dec` 的低速信号。
fn decimate_mean(samples: &[i16], dec: usize) -> Vec<f64> {
    if dec <= 1 {
        return samples.iter().map(|&s| s as f64).collect();
    }
    let mut out = Vec::with_capacity(samples.len() / dec);
    let mut acc = 0f64;
    for (index, &sample) in samples.iter().enumerate() {
        acc += sample as f64;
        if index % dec == dec - 1 {
            out.push(acc / dec as f64);
            acc = 0.0;
        }
    }
    out
}

/// 启动阶段累积数据、估计一次延迟。
///
/// 参考和麦克风按到达顺序成对对齐（长度不齐时余下的留到下次），
/// 估计成功后清空缓冲并停止工作。
struct DelayEstimator {
    rate: u32,
    ref_pending: Vec<i16>,
    mic_pending: Vec<i16>,
    reference: Vec<i16>,
    mic: Vec<i16>,
    tried_at: usize,
}

impl DelayEstimator {
    fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(8_000),
            ref_pending: Vec::new(),
            mic_pending: Vec::new(),
            reference: Vec::new(),
            mic: Vec::new(),
            tried_at: 0,
        }
    }

    /// 喂入新数据；锁定成功时返回（延迟采样点，覆盖本次麦克风块的已对齐参考）。
    ///
    /// 返回的参考窗口从「本次麦克风块起点往前 `lag` 个样本」开始，长度是
    /// `mic.len() + lag`，正好让滤波器在锁定瞬间就处于正确对齐状态。
    fn push(&mut self, reference: &[i16], mic: &[i16]) -> Option<(usize, Vec<i16>)> {
        self.ref_pending.extend_from_slice(reference);
        self.mic_pending.extend_from_slice(mic);
        let paired = self.ref_pending.len().min(self.mic_pending.len());
        if paired > 0 {
            self.reference.extend(self.ref_pending.drain(..paired));
            self.mic.extend(self.mic_pending.drain(..paired));
        }

        // 最多保留 4 秒，避免长时间锁不上时内存一直涨。
        let cap = self.rate as usize * 4;
        if self.reference.len() > cap {
            let drop = self.reference.len() - cap;
            self.reference.drain(..drop);
            self.mic.drain(..drop);
            self.tried_at = self.tried_at.saturating_sub(drop);
        }
        if self.ref_pending.len() > cap {
            let drop = self.ref_pending.len() - cap;
            self.ref_pending.drain(..drop);
        }

        // 至少攒够 1.2 秒，之后每秒尝试一次。
        let want = self.rate as usize * 12 / 10;
        if self.reference.len() < want
            || self.reference.len().saturating_sub(self.tried_at) < self.rate as usize
        {
            return None;
        }
        self.tried_at = self.reference.len();

        let start = self.reference.len() - want;
        let lag = estimate_delay(
            &self.reference[start..],
            &self.mic[start..],
            self.rate,
            MAX_DELAY_MS as u32,
        )?;

        // 覆盖本次麦克风块的已对齐参考：从块起点前 lag 个样本，到当前参考末尾。
        let len = self.reference.len();
        let m = mic.len();
        let aligned_start = len.saturating_sub(lag + m);
        let aligned_end = len;
        let mut aligned = self.reference[aligned_start..aligned_end].to_vec();
        // 数据不够时在前面补 0（更早的参考未知），保证与麦克风块等长 + lag。
        let need = m + lag;
        if aligned.len() < need {
            let mut padded = vec![0i16; need - aligned.len()];
            padded.extend(aligned);
            aligned = padded;
        }

        self.reference = Vec::new();
        self.mic = Vec::new();
        self.ref_pending = Vec::new();
        self.mic_pending = Vec::new();
        Some((lag, aligned))
    }
}

/// 一路麦克风的回声消除器。参考信号是系统回环（单声道，且已重采样到麦克风采样率）。
///
/// 前 1.2 秒左右只做直通，同时累积数据估计延迟；锁定后按块做 NLMS 与残留抑制。
pub struct Aec {
    rate: u32,
    taps: usize,
    weights: Vec<f32>,
    /// 时间顺序的输入窗口：`[taps]` 个历史 + 当前块。
    hist: Vec<f32>,
    /// 已按延迟对齐、待处理的参考样本。
    ref_queue: VecDeque<i16>,
    /// 尚未凑满一块的麦克风样本。
    mic_pending: Vec<i16>,
    estimator: Option<DelayEstimator>,
    delay: Option<usize>,
    /// 锁定时参考窗口已经包含本次参考数据，这一轮不要再追加。
    skip_ref_once: bool,
    /// 双讲状态（上一块的判决，用于决定本块是否自适应）。
    double_talk: bool,
    dt_hold: u8,
    /// 平滑后的估计回声功率与 ERLE。
    echo_power: f32,
    erle_db: f32,
    last_log: Instant,
    /// 统计：本轮日志周期内的总块数与双讲块数。
    blocks: u64,
    dt_blocks: u64,
}

impl Aec {
    /// `sample_rate` 是麦克风那一路的采样率；参考信号必须已重采样到同一速率。
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(8_000);
        // 滤波器长度按 100 ms，向上取整到分块大小。
        let taps = (rate as usize * FILTER_MS / 1000).div_ceil(BLOCK) * BLOCK;
        Self {
            rate,
            taps,
            weights: vec![0.0; taps],
            hist: vec![0.0; taps + BLOCK],
            ref_queue: VecDeque::new(),
            mic_pending: Vec::with_capacity(BLOCK * 4),
            estimator: Some(DelayEstimator::new(rate)),
            delay: None,
            skip_ref_once: false,
            double_talk: false,
            dt_hold: 0,
            echo_power: 0.0,
            erle_db: 0.0,
            last_log: Instant::now(),
            blocks: 0,
            dt_blocks: 0,
        }
    }

    /// 滤波器长度（tap 数）。
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// 是否已锁定延迟。
    pub fn is_locked(&self) -> bool {
        self.delay.is_some()
    }

    /// 估计出的延迟（采样点）；未锁定为 `None`。
    pub fn delay_samples(&self) -> Option<usize> {
        self.delay
    }

    /// 平滑后的 ERLE（dB）。
    pub fn erle_db(&self) -> f32 {
        self.erle_db
    }

    /// 当前是否处于双讲（冻结自适应）。
    pub fn is_double_talk(&self) -> bool {
        self.double_talk
    }

    /// 喂入同一时间片的两路单声道数据，返回处理后的麦克风数据。
    ///
    /// 延迟未锁定前原样返回 `mic`，避免启动阶段误消近端声音；锁定后按分块
    /// 处理，返回的样本数可能比 `mic` 少不到一块（剩余的留到下次调用）。
    pub fn process(&mut self, reference: &[i16], mic: &[i16]) -> Vec<i16> {
        if self.delay.is_none() {
            let locked = self
                .estimator
                .as_mut()
                .and_then(|estimator| estimator.push(reference, mic));
            match locked {
                Some((lag, aligned)) => self.lock(lag, aligned),
                None => return mic.to_vec(),
            }
        }

        if self.skip_ref_once {
            self.skip_ref_once = false;
        } else {
            // 参考整体延后 lag 个样本后，才是与麦克风对齐的「远端信号」。
            self.ref_queue.extend(reference.iter().copied());
            // 两路同一时钟，正常不会长期积压；超过 500 ms 说明异常，丢掉最旧的。
            let cap = self.rate as usize / 2;
            while self.ref_queue.len() > cap {
                self.ref_queue.pop_front();
            }
        }
        self.mic_pending.extend_from_slice(mic);

        let mut out = Vec::with_capacity(mic.len());
        self.drain_blocks(&mut out);
        self.maybe_log();
        out
    }

    /// 把不足一块的尾巴用静音补齐后处理完，返回剩余输出。
    ///
    /// 实时采集用不到（尾巴留到下一次），离线一次性处理时用来保证输出等长。
    pub fn flush(&mut self) -> Vec<i16> {
        if self.delay.is_none() || self.mic_pending.is_empty() {
            return Vec::new();
        }
        let pad = (BLOCK - self.mic_pending.len() % BLOCK) % BLOCK;
        self.mic_pending.extend(std::iter::repeat_n(0i16, pad));
        let mut out = Vec::with_capacity(self.mic_pending.len());
        self.drain_blocks(&mut out);
        out
    }

    /// 把 `mic_pending` 里凑满的块全部处理掉。
    fn drain_blocks(&mut self, out: &mut Vec<i16>) {
        let mut far = [0f32; BLOCK];
        let mut near = [0f32; BLOCK];
        while self.mic_pending.len() >= BLOCK {
            let rest = self.mic_pending.split_off(BLOCK);
            let chunk = std::mem::replace(&mut self.mic_pending, rest);
            for (slot, &sample) in near.iter_mut().zip(chunk.iter()) {
                *slot = sample as f32 * INV_I16;
            }
            for slot in far.iter_mut() {
                // 参考没跟上时补 0：回环空闲本来就没有声音。
                *slot = self.ref_queue.pop_front().unwrap_or(0) as f32 * INV_I16;
            }
            self.process_block(&far, &near, out);
        }
    }

    /// 内部状态是否全部有限（单测用）。
    #[cfg(test)]
    fn state_is_finite(&self) -> bool {
        self.echo_power.is_finite()
            && self.erle_db.is_finite()
            && self.weights.iter().all(|weight| weight.is_finite())
            && self.hist.iter().all(|sample| sample.is_finite())
    }

    /// 锁定延迟：安装已对齐的参考窗口，并清空待处理数据。
    fn lock(&mut self, lag: usize, aligned: Vec<i16>) {
        self.delay = Some(lag);
        self.ref_queue = VecDeque::from(aligned);
        self.mic_pending.clear();
        self.estimator = None;
        self.skip_ref_once = true;
        tracing::info!(
            delay_ms = lag * 1000 / self.rate as usize,
            delay_samples = lag,
            taps = self.taps,
            filter_ms = self.taps * 1000 / self.rate as usize,
            "AEC 已对齐扬声器到麦克风的延迟"
        );
    }

    /// 处理一个块：NLMS + 双讲 + 残留抑制。
    fn process_block(&mut self, far: &[f32; BLOCK], mic: &[f32; BLOCK], out: &mut Vec<i16>) {
        let taps = self.taps;
        // 当前块接到窗口末尾，窗口保持「最旧 … 最新」的顺序。
        self.hist[taps..taps + BLOCK].copy_from_slice(far);

        // 上一块的判决决定这一块是否自适应（1 块 ≈ 1.3 ms，可以忽略）。
        let adapt = !self.double_talk;

        let mut mic_pow = 0f32;
        let mut echo_pow = 0f32;
        let mut err_pow = 0f32;
        let mut errors = [0f32; BLOCK];

        for i in 0..BLOCK {
            // x 以最新样本结尾：x[k] 对应 t-(taps-1-k)。
            let x = &self.hist[i + 1..i + 1 + taps];
            let mut y = 0f32;
            let mut xpow = 0f32;
            for (weight, &sample) in self.weights.iter().zip(x.iter()) {
                y += weight * sample;
                xpow += sample * sample;
            }
            let m = mic[i];
            let e = m - y;
            errors[i] = e;
            mic_pow += m * m;
            echo_pow += y * y;
            err_pow += e * e;

            if adapt {
                // 归一化步长；xpow 很小时更新量自然趋近 0，不会放大噪声。
                let mu = STEP / (xpow + 1e-3);
                if mu.is_finite() && e.is_finite() {
                    for (weight, &sample) in self.weights.iter_mut().zip(x.iter()) {
                        *weight += mu * e * sample;
                    }
                }
            }
        }

        let inv = 1.0 / BLOCK as f32;
        mic_pow *= inv;
        echo_pow *= inv;
        err_pow *= inv;

        // 数值异常时整体复位，宁可短暂直通也不要输出 NaN。
        if !(mic_pow.is_finite() && err_pow.is_finite() && echo_pow.is_finite()) {
            self.reset();
            out.extend(mic.iter().map(|&sample| normalize_i16(sample)));
            self.hist.copy_within(BLOCK.., 0);
            return;
        }

        // 双讲：麦克风能量远高于估计回声，且滤波器已经收敛。
        let double_talk = self.erle_db > DTD_ERLE_DB
            && mic_pow > DTD_FLOOR
            && mic_pow > DTD_RATIO * self.echo_power.max(DTD_FLOOR);
        if double_talk {
            self.dt_hold = DT_HOLD_BLOCKS;
        } else {
            self.dt_hold = self.dt_hold.saturating_sub(1);
        }
        self.double_talk = double_talk || self.dt_hold > 0;
        self.blocks += 1;
        if self.double_talk {
            self.dt_blocks += 1;
        }

        // 只在非双讲时更新 ERLE，否则近端语音会把它拉低。
        if !self.double_talk && mic_pow > DTD_FLOOR {
            let instant = (10.0 * (mic_pow / err_pow.max(EPS)).log10()).clamp(-10.0, 40.0);
            self.erle_db = 0.9 * self.erle_db + 0.1 * instant;
        }
        self.echo_power = 0.8 * self.echo_power + 0.2 * echo_pow;

        // 残留回声抑制：按估计回声功率做功率域增益。双讲时旁路，保护近端语音。
        let gain = if self.double_talk {
            1.0
        } else {
            let target = RES_LEAK * self.echo_power;
            if err_pow > target && err_pow > EPS {
                ((err_pow - target) / err_pow).sqrt()
            } else {
                0.0
            }
        }
        .clamp(GAIN_MIN, 1.0);

        for &error in errors.iter() {
            out.push(normalize_i16(error * gain));
        }

        // 窗口左移一个块，下一块继续往后接。
        self.hist.copy_within(BLOCK.., 0);
    }

    /// 复位自适应状态（保留延迟锁定）。
    fn reset(&mut self) {
        self.weights.fill(0.0);
        self.hist.fill(0.0);
        self.echo_power = 0.0;
        self.erle_db = 0.0;
        self.double_talk = false;
        self.dt_hold = 0;
    }

    /// 每 10 秒左右打一条 ERLE。
    fn maybe_log(&mut self) {
        if self.last_log.elapsed() < LOG_INTERVAL {
            return;
        }
        self.last_log = Instant::now();
        tracing::debug!(
            erle_db = self.erle_db,
            double_talk = self.double_talk,
            double_talk_pct = if self.blocks == 0 {
                0.0
            } else {
                self.dt_blocks as f32 * 100.0 / self.blocks as f32
            },
            delay_ms = self.delay.map(|lag| lag * 1000 / self.rate as usize),
            "AEC 状态"
        );
        self.blocks = 0;
        self.dt_blocks = 0;
    }
}

/// 一次性处理一整段数据（单测和离线调试用）。
///
/// 传入已知延迟时跳过延迟估计、直接对齐；估不出来就原样返回。
pub fn cancel_echo(
    reference: &[i16],
    mic: &[i16],
    sample_rate: u32,
    delay: Option<usize>,
) -> Vec<i16> {
    let Some(lag) = delay else {
        return mic.to_vec();
    };
    let mut aec = Aec::new(sample_rate);
    // 对齐后的参考：前 lag 个样本补 0（过去未知），其后是原始参考。
    let mut aligned = vec![0i16; lag];
    aligned.extend_from_slice(reference);
    aec.lock(lag, aligned);
    let mut out = aec.process(reference, mic);
    out.extend(aec.flush());
    out.truncate(mic.len());
    out
}

/// 把 [-1, 1] 的浮点样本转回 i16；非有限值归零。
fn normalize_i16(value: f32) -> i16 {
    if value.is_finite() {
        (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// 固定种子的均匀噪声，幅度约 ±amp。
    fn noise(seed: u64, len: usize, amp: f32) -> Vec<i16> {
        let mut state = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let unit = (state >> 40) as f32 / (1u32 << 24) as f32;
            out.push(((unit * 2.0 - 1.0) * amp * 32767.0).round() as i16);
        }
        out
    }

    /// 参考信号经过「延迟 + 若干抽头」的 FIR 后得到的回声。
    fn echo_of(reference: &[i16], delay: usize, taps: &[(usize, f32)]) -> Vec<i16> {
        let mut out = vec![0i16; reference.len()];
        for &(offset, gain) in taps {
            let d = delay + offset;
            for t in d..reference.len() {
                let value = out[t] as f32 + reference[t - d] as f32 * gain;
                out[t] = value.clamp(-32768.0, 32767.0) as i16;
            }
        }
        out
    }

    fn energy(samples: &[i16]) -> f64 {
        samples.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / samples.len().max(1) as f64
    }

    /// 输入相对输出的能量下降（dB）。
    fn reduction_db(input: &[i16], output: &[i16]) -> f64 {
        10.0 * (energy(input) / energy(output).max(1e-12)).log10()
    }

    #[test]
    fn estimates_known_delay() {
        let n = RATE as usize * 3;
        let reference = noise(11, n, 0.3);
        let mic = echo_of(&reference, 800, &[(0, 0.5), (9, 0.2)]);
        let lag =
            estimate_delay(&reference, &mic, RATE, MAX_DELAY_MS as u32).expect("应该能估出延迟");
        // 容差 ±2 ms。
        let tolerance = RATE as i64 * 2 / 1000;
        assert!(
            (lag as i64 - 800).abs() <= tolerance,
            "估计延迟 {lag} 偏离 800 超过 {tolerance}"
        );
    }

    #[test]
    fn cancels_pure_echo() {
        let n = RATE as usize * 4;
        let reference = noise(21, n, 0.3);
        // 回声走一条 3 抽头的短路径；再加一点麦克风本底噪声，避免「完美可建模」。
        let mut mic = echo_of(&reference, 600, &[(0, 0.4), (12, 0.15), (30, -0.08)]);
        let floor = noise(22, n, 0.01);
        for (sample, hiss) in mic.iter_mut().zip(floor.iter()) {
            *sample = (*sample as i32 + *hiss as i32).clamp(-32768, 32767) as i16;
        }
        let out = cancel_echo(&reference, &mic, RATE, Some(600));
        assert_eq!(out.len(), mic.len());
        // 只看收敛后的最后 1.5 秒。
        let start = n - RATE as usize * 3 / 2;
        let db = reduction_db(&mic[start..], &out[start..]);
        eprintln!("纯回声（含本底噪声）下降 {db:.1} dB");
        assert!(db >= 12.0, "纯回声残留只下降了 {db:.1} dB");
    }

    #[test]
    fn keeps_near_end_when_reference_silent() {
        let n = RATE as usize * 2;
        let reference = vec![0i16; n];
        let mic = noise(31, n, 0.3);
        let out = cancel_echo(&reference, &mic, RATE, Some(0));
        assert_eq!(out.len(), mic.len());
        let db = reduction_db(&mic, &out);
        assert!(db < 1.0, "近端语音被削弱了 {db:.2} dB");
    }

    #[test]
    fn double_talk_keeps_near_end() {
        let n = RATE as usize * 5;
        let reference = noise(41, n, 0.25);
        // 回声比较轻，近端语音更响，模拟「边放边说话」。
        let mut mic = echo_of(&reference, 600, &[(0, 0.12), (10, 0.05), (24, -0.03)]);
        let near = noise(42, n, 0.35);
        let near_start = RATE as usize * 3 / 2;
        for (t, sample) in mic.iter_mut().enumerate().skip(near_start) {
            *sample = (*sample as f32 + near[t] as f32).clamp(-32768.0, 32767.0) as i16;
        }
        let out = cancel_echo(&reference, &mic, RATE, Some(600));
        assert_eq!(out.len(), mic.len());
        let start = RATE as usize * 2;
        let input = energy(&mic[start..]);
        let output = energy(&out[start..]);
        assert!(
            output >= input * 0.6,
            "双讲时近端语音被消掉了: 输入 {input:.0}, 输出 {output:.0}"
        );
    }

    #[test]
    fn silence_is_safe() {
        let n = RATE as usize * 2;
        let reference = vec![0i16; n];
        let mic = vec![0i16; n];
        let out = cancel_echo(&reference, &mic, RATE, Some(0));
        assert_eq!(out.len(), mic.len());
        assert!(out.iter().all(|&s| s == 0));
        assert!(estimate_delay(&reference, &mic, RATE, MAX_DELAY_MS as u32).is_none());

        // 空输入也不能 panic。
        let mut aec = Aec::new(RATE);
        assert!(aec.process(&[], &[]).is_empty());
        assert!(aec.process(&[], &[]).is_empty());
    }

    #[test]
    fn streaming_locks_and_cancels() {
        let n = RATE as usize * 3;
        let reference = noise(51, n, 0.3);
        let mic = echo_of(&reference, 900, &[(0, 0.5), (8, 0.1)]);
        let mut aec = Aec::new(RATE);
        let chunk = 960;
        let mut out = Vec::new();
        for start in (0..n).step_by(chunk) {
            let end = (start + chunk).min(n);
            out.extend(aec.process(&reference[start..end], &mic[start..end]));
        }
        assert!(aec.is_locked(), "3 秒数据应该能锁定延迟");
        let lag = aec.delay_samples().expect("已锁定");
        let tolerance = RATE as i64 * 2 / 1000;
        assert!(
            (lag as i64 - 900).abs() <= tolerance,
            "流式估计延迟 {lag} 偏离 900"
        );
        assert_eq!(out.len(), mic.len());
        let start = n - RATE as usize;
        let db = reduction_db(&mic[start..], &out[start..]);
        assert!(db >= 6.0, "流式回声残留只下降了 {db:.1} dB");
    }

    #[test]
    fn cancels_echo_at_44k() {
        // 设备混音不是 48 kHz 时也要能工作。
        let rate = 44_100u32;
        let n = rate as usize * 4;
        let reference = noise(61, n, 0.3);
        let delay = 500;
        let mic = echo_of(&reference, delay, &[(0, 0.4), (11, 0.12)]);
        let out = cancel_echo(&reference, &mic, rate, Some(delay));
        assert_eq!(out.len(), mic.len());
        let start = n - rate as usize * 3 / 2;
        let db = reduction_db(&mic[start..], &out[start..]);
        eprintln!("44.1k 纯回声下降 {db:.1} dB");
        assert!(db >= 12.0, "44.1 kHz 下纯回声只下降了 {db:.1} dB");
    }

    #[test]
    fn rejects_lag_beyond_search_range() {
        // 延迟超过搜索上限时不该误锁到一个错误位置。
        let n = RATE as usize * 3;
        let reference = noise(71, n, 0.3);
        let mic = echo_of(&reference, RATE as usize, &[(0, 0.5)]);
        let lag = estimate_delay(&reference, &mic, RATE, MAX_DELAY_MS as u32);
        if let Some(lag) = lag {
            assert!(
                lag <= RATE as usize * MAX_DELAY_MS / 1000,
                "延迟 {lag} 超出搜索范围"
            );
        }
    }

    #[test]
    fn saturating_input_stays_finite() {
        // 削顶/接近满幅的输入不能把滤波器推成 NaN。
        let n = RATE as usize;
        let reference: Vec<i16> = (0..n)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        let mic: Vec<i16> = (0..n)
            .map(|i| if i % 3 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        let out = cancel_echo(&reference, &mic, RATE, Some(64));
        assert_eq!(out.len(), mic.len());
        // 内部状态必须是有限值：一旦 NaN 就会永远传播下去。
        let mut aec = Aec::new(RATE);
        aec.lock(64, vec![0i16; 64]);
        let processed = aec.process(&reference, &mic);
        assert!(aec.state_is_finite(), "AEC 内部状态出现 NaN/Inf");
        assert!(!processed.is_empty());
    }

    #[test]
    fn no_echo_keeps_passthrough_before_lock() {
        // 参考和麦克风都不相关（纯噪声近端）时，锁不上延迟就直通。
        let n = RATE as usize * 2;
        let reference = noise(81, n, 0.2);
        let mic = noise(82, n, 0.2);
        let mut aec = Aec::new(RATE);
        let chunk = 480;
        let mut out = Vec::new();
        for start in (0..n).step_by(chunk) {
            let end = (start + chunk).min(n);
            out.extend(aec.process(&reference[start..end], &mic[start..end]));
        }
        assert!(!aec.is_locked(), "不相关的两路信号不应该锁定");
        assert_eq!(out.len(), mic.len());
        let db = reduction_db(&mic, &out);
        assert!(db.abs() < 0.5, "未锁定时应当是直通，实际变化 {db:.2} dB");
    }

    #[test]
    fn switch_round_trips() {
        set_enabled(false);
        assert!(!enabled());
        set_enabled(true);
        assert!(enabled());
    }
}
