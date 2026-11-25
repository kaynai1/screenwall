//! 中转会话帧：服务器把主控端与被控端之间的字节流按会话号转发。
//!
//! 规格见 `docs/server.md` 第 5 节。

use thiserror::Error;

/// 会话帧头长度。
pub const TUNNEL_HEADER_LEN: usize = 8;
/// 当前会话帧版本。
pub const TUNNEL_VERSION: u8 = 1;
/// 服务器允许的单帧最大负载。
pub const TUNNEL_MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TunnelKind {
    /// 应用层 JSON 消息。
    Json = 1,
    /// 媒体或文件二进制帧（24 字节帧头 + 负载）。
    Binary = 2,
}

impl TryFrom<u8> for TunnelKind {
    type Error = TunnelError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Json),
            2 => Ok(Self::Binary),
            other => Err(TunnelError::BadKind(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelHeader {
    pub kind: TunnelKind,
    pub session: u32,
    pub flags: u16,
}

impl TunnelHeader {
    /// 编码为 8 字节头。
    pub fn encode(&self) -> [u8; TUNNEL_HEADER_LEN] {
        let mut out = [0u8; TUNNEL_HEADER_LEN];
        out[0] = TUNNEL_VERSION;
        out[1] = self.kind as u8;
        out[2..6].copy_from_slice(&self.session.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, TunnelError> {
        if data.len() < TUNNEL_HEADER_LEN {
            return Err(TunnelError::TooShort);
        }
        let version = data[0];
        if version != TUNNEL_VERSION {
            return Err(TunnelError::BadVersion(version));
        }
        Ok(Self {
            kind: TunnelKind::try_from(data[1])?,
            session: u32::from_le_bytes([data[2], data[3], data[4], data[5]]),
            flags: u16::from_le_bytes([data[6], data[7]]),
        })
    }
}

/// 组装会话帧头 + 负载。
pub fn encode_tunnel_frame(kind: TunnelKind, session: u32, payload: &[u8]) -> Vec<u8> {
    let header = TunnelHeader {
        kind,
        session,
        flags: 0,
    };
    let mut out = Vec::with_capacity(TUNNEL_HEADER_LEN + payload.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out
}

/// 解析完整会话帧，返回头与负载切片。
pub fn decode_tunnel_frame(data: &[u8]) -> Result<(TunnelHeader, &[u8]), TunnelError> {
    let header = TunnelHeader::decode(data)?;
    Ok((header, &data[TUNNEL_HEADER_LEN..]))
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TunnelError {
    #[error("会话帧长度不足")]
    TooShort,
    #[error("不支持的会话帧版本: {0}")]
    BadVersion(u8),
    #[error("未知会话帧类型: {0}")]
    BadKind(u8),
    #[error("会话帧负载超过上限: {0} 字节")]
    TooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = TunnelHeader {
            kind: TunnelKind::Json,
            session: 12345,
            flags: 0,
        };
        let encoded = header.encode();
        assert_eq!(encoded.len(), TUNNEL_HEADER_LEN);
        assert_eq!(TunnelHeader::decode(&encoded).unwrap(), header);
    }

    #[test]
    fn frame_roundtrip() {
        let payload = br#"{"t":"hello"}"#;
        let bytes = encode_tunnel_frame(TunnelKind::Json, 7, payload);
        let (header, body) = decode_tunnel_frame(&bytes).unwrap();
        assert_eq!(header.kind, TunnelKind::Json);
        assert_eq!(header.session, 7);
        assert_eq!(body, payload);
    }

    #[test]
    fn binary_kind_preserved() {
        let bytes = encode_tunnel_frame(TunnelKind::Binary, 99, &[1, 2, 3]);
        let (header, body) = decode_tunnel_frame(&bytes).unwrap();
        assert_eq!(header.kind, TunnelKind::Binary);
        assert_eq!(header.session, 99);
        assert_eq!(body, &[1, 2, 3]);
    }

    #[test]
    fn truncated_header_rejected() {
        assert_eq!(TunnelHeader::decode(&[1, 1, 0]), Err(TunnelError::TooShort));
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut bytes = TunnelHeader {
            kind: TunnelKind::Json,
            session: 1,
            flags: 0,
        }
        .encode();
        bytes[0] = 9;
        assert_eq!(
            TunnelHeader::decode(&bytes),
            Err(TunnelError::BadVersion(9))
        );
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut bytes = TunnelHeader {
            kind: TunnelKind::Json,
            session: 1,
            flags: 0,
        }
        .encode();
        bytes[1] = 42;
        assert_eq!(TunnelHeader::decode(&bytes), Err(TunnelError::BadKind(42)));
    }
}
