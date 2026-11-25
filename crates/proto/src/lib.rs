//! ScreenWall 通信协议：消息类型、二进制帧格式、错误码。
//!
//! 规格见 `docs/protocol.md`。

pub mod caps;
pub mod cloud;
pub mod frame;
pub mod messages;

pub use caps::*;
pub use cloud::*;
pub use frame::*;
pub use messages::*;

/// 当前协议版本。
pub const PROTO_VERSION: u16 = 1;

/// 关闭码与应用错误码。
pub mod codes {
    // 握手失败时使用的 WebSocket 关闭码
    pub const AUTH_FAILED: u16 = 4001;
    pub const PROTO_UNSUPPORTED: u16 = 4002;
    pub const LIMIT_EXCEEDED: u16 = 4003;
    pub const CONTROL_DISABLED: u16 = 4004;
    pub const HANDSHAKE_TIMEOUT: u16 = 4005;
    pub const AUTH_RATE_LIMITED: u16 = 4006;

    // 应用层错误码
    pub const BAD_REQUEST: u16 = 4100;
    pub const STREAM_LIMIT: u16 = 4101;
    pub const ADDRESS_NOT_ALLOWED: u16 = 4102;
    pub const DISPLAY_NOT_FOUND: u16 = 4103;
    pub const ENCODER_UNAVAILABLE: u16 = 4104;
    pub const PATH_NOT_ALLOWED: u16 = 4105;
    pub const DISK_FULL: u16 = 4106;
    pub const CHECKSUM_FAILED: u16 = 4107;
    pub const CONTROL_OFF: u16 = 4108;
    pub const UNKNOWN_MESSAGE: u16 = 4109;
    pub const AUDIO_UNAVAILABLE: u16 = 4110;
    pub const POWER_FAILED: u16 = 4111;
}
