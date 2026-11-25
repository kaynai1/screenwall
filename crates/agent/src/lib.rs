//! ScreenWall 被控端。

pub mod aec;
pub mod audio;
pub mod ca;
pub mod capture;
pub mod clipboard;
pub mod cloud;
pub mod config;
pub mod cursor;
pub mod direct;
pub mod discovery;
pub mod display;
pub mod dxgi;
pub mod files;
pub mod input;
pub mod nat;
pub mod power;
pub mod quic;
pub mod server;
pub mod session;
pub mod stats;
pub mod tls;
pub mod virtual_display;
pub mod wol;

pub use server::{run_server, AgentState};
