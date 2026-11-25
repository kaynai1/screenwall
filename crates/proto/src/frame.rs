//! 二进制帧格式（视频帧与文件块共用 24 字节头）。

use thiserror::Error;

/// 帧头长度。
pub const FRAME_HEADER_LEN: usize = 24;
/// 当前帧格式版本。
pub const FRAME_VERSION: u8 = 1;

/// 关键帧标记位。
pub const FLAG_KEYFRAME: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Video = 1,
    Audio = 2,
    FileChunk = 3,
}

impl TryFrom<u8> for FrameType {
    type Error = FrameError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Video),
            2 => Ok(Self::Audio),
            3 => Ok(Self::FileChunk),
            other => Err(FrameError::BadType(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameCodec {
    Raw = 0,
    Jpeg = 1,
    H264 = 2,
    /// 交错小端 PCM（音频帧使用；采样率/声道数由 `audio.sub.ok` 给出）。
    Pcm16 = 3,
    /// 单个 Opus 包（20 ms，固定 48 kHz）。
    Opus = 4,
}

impl TryFrom<u8> for FrameCodec {
    type Error = FrameError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Jpeg),
            2 => Ok(Self::H264),
            3 => Ok(Self::Pcm16),
            4 => Ok(Self::Opus),
            other => Err(FrameError::BadCodec(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: FrameType,
    pub stream_id: u16,
    pub seq: u32,
    pub ts_us: u64,
    pub payload_len: u32,
    pub codec: FrameCodec,
    pub flags: u8,
}

impl FrameHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    /// 编码为 24 字节头。
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut out = [0u8; FRAME_HEADER_LEN];
        out[0] = FRAME_VERSION;
        out[1] = self.kind as u8;
        out[2..4].copy_from_slice(&self.stream_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..16].copy_from_slice(&self.ts_us.to_le_bytes());
        out[16..20].copy_from_slice(&self.payload_len.to_le_bytes());
        out[20] = self.codec as u8;
        out[21] = self.flags;
        out
    }

    /// 只解析头，不检查负载。
    pub fn decode(data: &[u8]) -> Result<Self, FrameError> {
        if data.len() < FRAME_HEADER_LEN {
            return Err(FrameError::TooShort);
        }
        let version = data[0];
        if version != FRAME_VERSION {
            return Err(FrameError::BadVersion(version));
        }
        Ok(Self {
            kind: FrameType::try_from(data[1])?,
            stream_id: u16::from_le_bytes([data[2], data[3]]),
            seq: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            ts_us: u64::from_le_bytes([
                data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
            ]),
            payload_len: u32::from_le_bytes([data[16], data[17], data[18], data[19]]),
            codec: FrameCodec::try_from(data[20])?,
            flags: data[21],
        })
    }
}

/// 组装头 + 负载。
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    let mut header = *header;
    header.payload_len = payload.len() as u32;
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out
}

/// 解析完整帧，返回头与负载切片。
pub fn decode_frame(data: &[u8]) -> Result<(FrameHeader, &[u8]), FrameError> {
    let header = FrameHeader::decode(data)?;
    let payload = &data[FRAME_HEADER_LEN..];
    if header.payload_len as usize != payload.len() {
        return Err(FrameError::LengthMismatch);
    }
    Ok((header, payload))
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    #[error("帧长度不足")]
    TooShort,
    #[error("不支持的帧版本: {0}")]
    BadVersion(u8),
    #[error("未知帧类型: {0}")]
    BadType(u8),
    #[error("未知编码: {0}")]
    BadCodec(u8),
    #[error("负载长度不匹配")]
    LengthMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> FrameHeader {
        FrameHeader {
            kind: FrameType::Video,
            stream_id: 7,
            seq: 42,
            ts_us: 1_700_000_000_000_000,
            payload_len: 0,
            codec: FrameCodec::H264,
            flags: FLAG_KEYFRAME,
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = header();
        let encoded = h.encode();
        assert_eq!(encoded.len(), FRAME_HEADER_LEN);
        assert_eq!(FrameHeader::decode(&encoded).unwrap(), h);
        assert!(FrameHeader::decode(&encoded).unwrap().is_keyframe());
    }

    #[test]
    fn full_frame_roundtrip() {
        let payload = b"hello-frame";
        let bytes = encode_frame(&header(), payload);
        let (decoded, body) = decode_frame(&bytes).unwrap();
        assert_eq!(decoded.payload_len as usize, payload.len());
        assert_eq!(decoded.stream_id, 7);
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.codec, FrameCodec::H264);
        assert_eq!(body, payload);
    }

    #[test]
    fn file_chunk_roundtrip() {
        let h = FrameHeader {
            kind: FrameType::FileChunk,
            stream_id: 12,
            seq: 3,
            ts_us: 0,
            payload_len: 0,
            codec: FrameCodec::Raw,
            flags: 0,
        };
        let bytes = encode_frame(&h, &[1, 2, 3]);
        let (decoded, body) = decode_frame(&bytes).unwrap();
        assert_eq!(decoded.kind, FrameType::FileChunk);
        assert_eq!(body, &[1, 2, 3]);
    }

    #[test]
    fn truncated_header_rejected() {
        assert_eq!(FrameHeader::decode(&[1, 1, 0]), Err(FrameError::TooShort));
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut bytes = header().encode();
        bytes[0] = 9;
        assert_eq!(FrameHeader::decode(&bytes), Err(FrameError::BadVersion(9)));
    }

    #[test]
    fn payload_length_mismatch_rejected() {
        let bytes = encode_frame(&header(), b"abc");
        assert_eq!(
            decode_frame(&bytes[..bytes.len() - 1]),
            Err(FrameError::LengthMismatch)
        );
    }
}
