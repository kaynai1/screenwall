//! 静态系统信息：显卡型号与局域网出口 IPv4，随 `welcome` 一次性上报。
//!
//! 两个采集都是「尽力而为」：拿不到就返回空字符串，由调用方按空处理，
//! 绝不 panic、绝不阻塞。显卡型号从 DXGI 的 `IDXGIAdapter::GetDesc` 读
//! `Description`（就是设备管理器里看到的适配器名）；出口 IP 用 UDP
//! `connect("8.8.8.8:80")` 借路由表拿本机出口地址（不真正发包，同
//! `crates/client/src/direct.rs::local_addresses` 的做法）。

use std::net::UdpSocket;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, DXGI_ADAPTER_DESC};

/// 显卡型号（如 "NVIDIA GeForce RTX 4060"）；采集失败或非 Windows 时为空。
pub fn gpu_name() -> String {
    unsafe { gpu_name_windows() }.unwrap_or_default()
}

unsafe fn gpu_name_windows() -> Option<String> {
    let levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;
    // SAFETY: 参数都是栈上合法指针，调用失败只返回 HRESULT。
    D3D11CreateDevice(
        None::<&IDXGIAdapter>,
        D3D_DRIVER_TYPE_HARDWARE,
        windows::Win32::Foundation::HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut device),
        Some(&mut level),
        Some(&mut context),
    )
    .ok()?;
    let device = device?;
    let dxgi_device: IDXGIDevice = device.cast().ok()?;
    let adapter: IDXGIAdapter = dxgi_device.GetAdapter().ok()?;
    let desc: DXGI_ADAPTER_DESC = adapter.GetDesc().ok()?;
    Some(from_wide(&desc.Description))
}

/// `[u16; N]` 的设备名去尾零转 `String`。
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 本机局域网出口 IPv4；拿不到时为空字符串。
///
/// 与客户端的 `local_addresses` 同思路：bind 任意端口后 `connect` 公网地址，
/// 由系统路由表决定出口网卡，`local_addr` 即该网卡的地址。整个过程不发包。
pub fn local_ipv4() -> String {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return String::new();
    };
    let Ok(()) = socket.connect("8.8.8.8:80") else {
        return String::new();
    };
    socket
        .local_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default()
}
