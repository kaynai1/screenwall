/** 视频帧渲染：MJPEG 走 createImageBitmap，H.264 走 WebCodecs。 */

import {
  avcCodecString,
  CODEC_H264,
  CODEC_JPEG,
  FLAG_KEYFRAME,
  type MediaFrame,
} from "./protocol";

export type VideoSample = { fps: number; kbps: number };

export class VideoRenderer {
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;
  /** MJPEG 解码中的帧（只保留最新一帧，旧的直接丢）。 */
  private pendingJpeg: Uint8Array | null = null;
  private decoding = false;
  private bytes = 0;
  private frames = 0;
  private lastSample = performance.now();

  private decoder: VideoDecoder | null = null;
  private decoderCodec = "";
  private h264Broken = false;
  private waitingKeyframe = true;

  /** 画布尺寸变化（用于 UI 调整宽高比）。 */
  onSize: ((width: number, height: number) => void) | null = null;
  /** H.264 解不了时通知外层回退 MJPEG。 */
  onH264Unavailable: (() => void) | null = null;

  attach(canvas: HTMLCanvasElement): void {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  }

  detach(): void {
    this.canvas = null;
    this.ctx = null;
    this.reset();
  }

  push(frame: MediaFrame): void {
    this.bytes += frame.payload.length;
    if (frame.codec === CODEC_JPEG) {
      this.pendingJpeg = frame.payload;
      if (!this.decoding) void this.pumpJpeg();
      return;
    }
    if (frame.codec === CODEC_H264) {
      this.pushH264(frame);
    }
  }

  /** 每秒调用一次，给出这期间的帧率与码率。 */
  sample(): VideoSample {
    const now = performance.now();
    const seconds = Math.max(0.001, (now - this.lastSample) / 1000);
    const sample = {
      fps: Math.round((this.frames / seconds) * 10) / 10,
      kbps: Math.round((this.bytes * 8) / seconds / 1000),
    };
    this.frames = 0;
    this.bytes = 0;
    this.lastSample = now;
    return sample;
  }

  reset(): void {
    this.pendingJpeg = null;
    if (this.decoder) {
      try {
        this.decoder.close();
      } catch {
        // 已经关闭过就忽略。
      }
      this.decoder = null;
    }
    this.decoderCodec = "";
    this.waitingKeyframe = true;
  }

  private async pumpJpeg(): Promise<void> {
    this.decoding = true;
    while (this.pendingJpeg) {
      const bytes = this.pendingJpeg;
      this.pendingJpeg = null;
      try {
        // TS 5.7 把 Uint8Array 的 buffer 宽成 ArrayBufferLike，运行时这里永远是普通
        // ArrayBuffer（WebSocket 给的是 ArrayBuffer），不需要复制。
        const part = bytes as unknown as BlobPart;
        const bitmap = await createImageBitmap(new Blob([part], { type: "image/jpeg" }));
        this.draw(bitmap, bitmap.width, bitmap.height);
        bitmap.close();
      } catch {
        // 半帧/损坏帧：跳过，下一帧继续。
      }
    }
    this.decoding = false;
  }

  private draw(source: CanvasImageSource, width: number, height: number): void {
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!canvas || !ctx) return;
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
      this.onSize?.(width, height);
    }
    ctx.drawImage(source, 0, 0, width, height);
    this.frames += 1;
  }

  private pushH264(frame: MediaFrame): void {
    if (this.h264Broken) return;
    if (typeof VideoDecoder === "undefined") {
      this.markH264Broken();
      return;
    }
    const keyframe = (frame.flags & FLAG_KEYFRAME) !== 0;

    if (!this.decoder) {
      // 等关键帧，顺便从 SPS 里拼 codec 字符串。
      if (!keyframe) return;
      const codec = avcCodecString(frame.payload);
      if (!codec) return;
      if (!this.decoderCodec) this.decoderCodec = codec;
      try {
        this.decoder = new VideoDecoder({
          output: (videoFrame) => {
            try {
              this.draw(videoFrame, videoFrame.displayWidth, videoFrame.displayHeight);
            } finally {
              videoFrame.close();
            }
          },
          error: () => {
            this.markH264Broken();
          },
        });
        this.decoder.configure({
          codec,
          optimizeForLatency: true,
          hardwareAcceleration: "prefer-hardware",
        });
      } catch {
        this.markH264Broken();
        return;
      }
      this.waitingKeyframe = false;
    }

    if (this.waitingKeyframe && !keyframe) return;
    this.waitingKeyframe = false;

    try {
      this.decoder.decode(
        new EncodedVideoChunk({
          type: keyframe ? "key" : "delta",
          timestamp: frame.tsUs,
          data: frame.payload,
        }),
      );
    } catch {
      this.markH264Broken();
    }
  }

  private markH264Broken(): void {
    if (this.h264Broken) return;
    this.h264Broken = true;
    this.reset();
    this.onH264Unavailable?.();
  }
}
