/**
 * 中继隧道帧与媒体帧的编解码。
 *
 * 与 `crates/proto/src/cloud.rs`（8 字节会话帧）和
 * `crates/proto/src/frame.rs`（24 字节媒体帧）逐字节对应，改动任何一边都要同步。
 */

export const TUNNEL_HEADER_LEN = 8;
export const MEDIA_HEADER_LEN = 24;

export const TUNNEL_JSON = 1;
export const TUNNEL_BINARY = 2;

export const MEDIA_VIDEO = 1;
export const MEDIA_AUDIO = 2;
export const MEDIA_FILE = 3;

/** `video.sub.display` 取这个值表示整块虚拟桌面（`crates/proto` 的 `DISPLAY_ALL`）。 */
export const DISPLAY_ALL = 0xffffffff;

export const CODEC_JPEG = 1;
export const CODEC_H264 = 2;
export const CODEC_PCM16 = 3;
export const CODEC_OPUS = 4;

export const FLAG_KEYFRAME = 0x01;

export type TunnelFrame = {
  kind: number;
  session: number;
  payload: Uint8Array;
};

export type MediaFrame = {
  type: number;
  streamId: number;
  seq: number;
  tsUs: number;
  payloadLen: number;
  codec: number;
  flags: number;
  payload: Uint8Array;
};

/** 组装「会话帧头 + 负载」。 */
export function encodeTunnel(
  kind: number,
  session: number,
  payload: Uint8Array,
): Uint8Array {
  const out = new Uint8Array(TUNNEL_HEADER_LEN + payload.length);
  const view = new DataView(out.buffer);
  view.setUint8(0, 1);
  view.setUint8(1, kind);
  view.setUint32(2, session >>> 0, true);
  view.setUint16(6, 0, true);
  out.set(payload, TUNNEL_HEADER_LEN);
  return out;
}

export function decodeTunnel(data: Uint8Array): TunnelFrame | null {
  if (data.length < TUNNEL_HEADER_LEN || data[0] !== 1) return null;
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const kind = view.getUint8(1);
  if (kind !== TUNNEL_JSON && kind !== TUNNEL_BINARY) return null;
  return {
    kind,
    session: view.getUint32(2, true),
    payload: data.subarray(TUNNEL_HEADER_LEN),
  };
}

export function decodeMediaFrame(data: Uint8Array): MediaFrame | null {
  if (data.length < MEDIA_HEADER_LEN || data[0] !== 1) return null;
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const payloadLen = view.getUint32(16, true);
  if (payloadLen !== data.length - MEDIA_HEADER_LEN) return null;
  return {
    type: view.getUint8(1),
    streamId: view.getUint16(2, true),
    seq: view.getUint32(4, true),
    tsUs: Number(view.getBigUint64(8, true)),
    payloadLen,
    codec: view.getUint8(20),
    flags: view.getUint8(21),
    payload: data.subarray(MEDIA_HEADER_LEN),
  };
}

/** 把 JSON 消息编码成会话帧。 */
export function tunnelJson(session: number, message: unknown): Uint8Array {
  return encodeTunnel(
    TUNNEL_JSON,
    session,
    new TextEncoder().encode(JSON.stringify(message)),
  );
}

/** 从 Annex-B 码流里取 SPS，拼出 WebCodecs 需要的 `avc1.PPCCLL`。 */
export function avcCodecString(data: Uint8Array): string | null {
  for (let i = 0; i + 4 < data.length; i += 1) {
    // 起始码 00 00 01 或 00 00 00 01。
    if (data[i] !== 0 || data[i + 1] !== 0) continue;
    let header = i + 2;
    if (data[header] === 1) {
      header += 1;
    } else if (data[header] === 0 && data[header + 1] === 1) {
      header += 2;
    } else {
      continue;
    }
    if (header + 4 >= data.length) return null;
    if ((data[header] & 0x1f) !== 7) continue;
    const profile = data[header + 1];
    const constraints = data[header + 2];
    const level = data[header + 3];
    const hex = (value: number) => value.toString(16).padStart(2, "0");
    return `avc1.${hex(profile)}${hex(constraints)}${hex(level)}`;
  }
  return null;
}
