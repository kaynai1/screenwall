/// <reference types="vite/client" />

/**
 * 远程声音播放器：PCM 直接建 AudioBuffer，Opus 用 WebCodecs 解码后同样排进
 * Web Audio 时间线。队列落后就跳过，积压过多就重置，保证延迟不会越拖越大。
 */

/** 最多提前排 300 ms；超过说明播放端跟不上或网络抖动堆积，直接重置时间线。 */
const MAX_LEAD_SECONDS = 0.3;
/** 从「现在」开始播放时留的余量，避免首块因调度延迟被截断。 */
const START_DELAY_SECONDS = 0.06;
/** Opus 每包固定 20 ms，WebCodecs 需要递增的微秒时间戳。 */
const OPUS_PACKET_US = 20_000;

function decodeBase64(data: string): Uint8Array {
  const binary = atob(data);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

/** 当前 WebView 是否支持 Opus（WebCodecs）。不支持就退回 PCM 订阅。 */
export async function supportsOpusAudio(): Promise<boolean> {
  if (typeof AudioDecoder === "undefined") return false;
  try {
    const support = await AudioDecoder.isConfigSupported({
      codec: "opus",
      sampleRate: 48_000,
      numberOfChannels: 2,
    });
    return support.supported === true;
  } catch {
    return false;
  }
}

export class RemoteAudioPlayer {
  private ctx: AudioContext | null = null;
  private nextTime = 0;
  private queued = 0;
  private disposed = false;
  private decoder: AudioDecoder | null = null;
  private decoderRate = 0;
  private decoderChannels = 0;
  private opusTimestamp = 0;
  private onError: (message: string) => void;

  constructor(onError?: (message: string) => void) {
    this.onError = onError ?? (() => {});
  }

  /**
   * 用户点击开关时调用：AudioContext 需要在用户手势里创建/恢复，
   * 否则 WebView 的自动播放策略会把声音静音。
   */
  open(): void {
    if (this.disposed) return;
    if (!this.ctx) {
      this.ctx = new AudioContext();
    }
    if (this.ctx.state === "suspended") {
      void this.ctx.resume();
    }
    if (import.meta.env.DEV) {
      // 开发期验收用：在控制台能查缓冲深度、编码和 AudioContext 状态。
      (window as unknown as Record<string, unknown>).__audioPlayer = this;
    }
  }

  /** 喂一块 base64 交错 i16 PCM。格式变化时重建时间线。 */
  pushPcm(data: string, sampleRate: number, channels: number): void {
    if (this.disposed || sampleRate <= 0 || channels <= 0) return;
    this.open();
    const ctx = this.ctx;
    if (!ctx) return;

    const bytes = decodeBase64(data);
    const frames = Math.floor(bytes.length / 2 / channels);
    if (frames === 0) return;

    const buffer = ctx.createBuffer(channels, frames, sampleRate);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let channel = 0; channel < channels; channel += 1) {
      const target = buffer.getChannelData(channel);
      for (let frame = 0; frame < frames; frame += 1) {
        const offset = (frame * channels + channel) * 2;
        target[frame] = view.getInt16(offset, true) / 32768;
      }
    }
    this.schedule(buffer);
  }

  /** 喂一个 base64 Opus 包（20 ms）；解码完成后自动排进时间线。 */
  pushOpus(data: string, sampleRate: number, channels: number): void {
    if (this.disposed || sampleRate <= 0 || channels <= 0) return;
    this.open();
    if (!this.ctx) return;
    if (!this.ensureDecoder(sampleRate, channels)) return;

    const decoder = this.decoder;
    if (!decoder || decoder.state !== "configured") return;
    const packet = decodeBase64(data);
    try {
      decoder.decode(
        new EncodedAudioChunk({
          type: "key",
          timestamp: this.opusTimestamp,
          duration: OPUS_PACKET_US,
          data: packet,
        }),
      );
      this.opusTimestamp += OPUS_PACKET_US;
    } catch {
      // 单个坏包不打断整路声音。
    }
  }

  /** 当前估算的缓冲深度（毫秒），0 表示没有待播数据。 */
  get bufferMs(): number {
    const ctx = this.ctx;
    if (!ctx || this.nextTime <= 0) return 0;
    return Math.max(0, Math.round((this.nextTime - ctx.currentTime) * 1000));
  }

  get codec(): "opus" | "pcm16" | null {
    return this.decoder ? "opus" : this.ctx ? "pcm16" : null;
  }

  private ensureDecoder(sampleRate: number, channels: number): boolean {
    if (
      this.decoder &&
      this.decoder.state === "configured" &&
      this.decoderRate === sampleRate &&
      this.decoderChannels === channels
    ) {
      return true;
    }
    this.decoder?.close();
    this.decoder = null;
    if (typeof AudioDecoder === "undefined") {
      this.onError("当前环境不支持 Opus 解码");
      return false;
    }
    try {
      const decoder = new AudioDecoder({
        output: (audio) => this.onDecoded(audio),
        error: () => {
          this.decoder = null;
          this.onError("Opus 解码失败，请重新开启声音");
        },
      });
      decoder.configure({
        codec: "opus",
        sampleRate,
        numberOfChannels: channels,
      });
      this.decoder = decoder;
      this.decoderRate = sampleRate;
      this.decoderChannels = channels;
      this.opusTimestamp = 0;
      return true;
    } catch (error) {
      this.onError(
        `无法启用 Opus 解码：${error instanceof Error ? error.message : String(error)}`,
      );
      return false;
    }
  }

  private onDecoded(audio: AudioData): void {
    const ctx = this.ctx;
    if (!ctx || this.disposed) {
      audio.close();
      return;
    }
    const frames = audio.numberOfFrames;
    const channels = audio.numberOfChannels;
    if (frames <= 0 || channels <= 0) {
      audio.close();
      return;
    }
    try {
      const buffer = ctx.createBuffer(channels, frames, audio.sampleRate);
      for (let channel = 0; channel < channels; channel += 1) {
        audio.copyTo(buffer.getChannelData(channel), {
          planeIndex: channel,
          format: "f32-planar",
        });
      }
      this.schedule(buffer);
    } catch {
      // 解码器在关闭过程中仍可能回调，忽略即可。
    } finally {
      audio.close();
    }
  }

  private schedule(buffer: AudioBuffer): void {
    const ctx = this.ctx;
    if (!ctx) return;
    const now = ctx.currentTime;
    if (this.nextTime < now || this.nextTime > now + MAX_LEAD_SECONDS) {
      this.nextTime = now + START_DELAY_SECONDS;
      this.queued = 0;
    }
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    source.start(this.nextTime);
    this.nextTime += buffer.duration;
    this.queued += 1;
    source.onended = () => {
      this.queued = Math.max(0, this.queued - 1);
    };
  }

  dispose(): void {
    this.disposed = true;
    this.queued = 0;
    const decoder = this.decoder;
    this.decoder = null;
    if (decoder && decoder.state !== "closed") {
      try {
        decoder.close();
      } catch {
        // 已经关闭就算了。
      }
    }
    const ctx = this.ctx;
    this.ctx = null;
    if (ctx) {
      void ctx.close().catch(() => {});
    }
  }
}
