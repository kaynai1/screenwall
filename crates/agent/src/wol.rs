//! 远程开机（Wake-on-LAN）：向局域网广播标准 magic packet。
//!
//! magic packet 是 6 字节 `0xFF` 加 16 次目标 MAC，共 102 字节，
//! 通过 UDP 发到广播地址的标准 WOL 端口 9。只应由已经通过授权弹窗的控制会话调用。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// 标准 WOL 端口。
pub const WOL_PORT: u16 = 9;

/// 两次广播之间的最小间隔，避免把 Agent 变成广播放大器。
pub const MIN_INTERVAL: Duration = Duration::from_secs(10);

/// 上一次成功发送的时刻；`None` 表示本次进程还没发过。
static LAST_SENT: Mutex<Option<Instant>> = Mutex::new(None);

/// magic packet 长度：6 字节头 + 16 段 MAC。
const MAGIC_PACKET_LEN: usize = 6 + 16 * 6;

/// 解析 MAC 地址，接受 `AA:BB:CC:DD:EE:FF`、`AA-BB-CC-DD-EE-FF` 和无分隔的 12 位十六进制。
///
/// 全 0 和全 `FF` 不是可唤醒的网卡地址，返回 `None`。
pub fn normalize_mac(input: &str) -> Option<[u8; 6]> {
    let input = input.trim();
    let bytes: Vec<u8> = if input.contains(':') || input.contains('-') {
        // 分隔符必须统一，混用视为非法。
        let separator = if input.contains(':') { ':' } else { '-' };
        let parts: Vec<&str> = input.split(separator).collect();
        if parts.len() != 6 || parts.iter().any(|part| part.len() != 2) {
            return None;
        }
        let mut bytes = Vec::with_capacity(6);
        for part in parts {
            bytes.push(u8::from_str_radix(part, 16).ok()?);
        }
        bytes
    } else {
        if input.len() != 12 || !input.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = Vec::with_capacity(6);
        for index in 0..6 {
            let start = index * 2;
            bytes.push(u8::from_str_radix(&input[start..start + 2], 16).ok()?);
        }
        bytes
    };

    let mac: [u8; 6] = bytes.try_into().ok()?;
    if mac == [0x00; 6] || mac == [0xFF; 6] {
        return None;
    }
    // 首字节最低位是组播/广播标志，只有单播地址才对应一块网卡。
    if mac[0] & 0x01 != 0 {
        return None;
    }
    Some(mac)
}

/// 解析广播地址，接受 `192.168.1.255` 和 `192.168.1.255:9`（端口丢弃，统一用 9）。
///
/// 只接受 `255.255.255.255` 或私有网段地址（10/8、172.16/12、192.168/16），
/// 避免被当成向任意公网主机发包的放大器。
pub fn normalize_broadcast(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let address = match input.split_once(':') {
        Some((address, port)) => {
            if port.is_empty() || port.parse::<u16>().is_err() {
                return None;
            }
            address
        }
        None => input,
    };
    let address: Ipv4Addr = address.parse().ok()?;
    if !is_allowed_broadcast(address) {
        return None;
    }
    Some(address.to_string())
}

fn is_allowed_broadcast(address: Ipv4Addr) -> bool {
    if address == Ipv4Addr::BROADCAST {
        return true;
    }
    let octets = address.octets();
    octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
}

/// 向 `broadcast:9` 发送一次 magic packet；两次发送至少间隔 [`MIN_INTERVAL`]。
pub fn send_magic_packet(mac: &str, broadcast: &str) -> Result<()> {
    let mac = normalize_mac(mac).with_context(|| format!("MAC 地址无效: {mac}"))?;
    let broadcast =
        normalize_broadcast(broadcast).with_context(|| format!("广播地址无效: {broadcast}"))?;
    let target = SocketAddr::from((
        broadcast
            .parse::<Ipv4Addr>()
            .with_context(|| format!("广播地址无效: {broadcast}"))?,
        WOL_PORT,
    ));

    let mut last = LAST_SENT.lock().unwrap();
    if let Some(at) = *last {
        let elapsed = at.elapsed();
        if elapsed < MIN_INTERVAL {
            let wait = MIN_INTERVAL - elapsed;
            bail!("发送过于频繁，请 {} 秒后再试", wait.as_secs().max(1));
        }
    }

    let socket = UdpSocket::bind("0.0.0.0:0").context("绑定 UDP 套接字失败")?;
    socket.set_broadcast(true).context("开启 UDP 广播失败")?;
    let packet = magic_packet(&mac);
    socket
        .send_to(&packet, target)
        .with_context(|| format!("向 {target} 发送 magic packet 失败"))?;
    *last = Some(Instant::now());
    Ok(())
}

/// 6 字节 `0xFF` + 16 次 MAC。
fn magic_packet(mac: &[u8; 6]) -> [u8; MAGIC_PACKET_LEN] {
    let mut packet = [0u8; MAGIC_PACKET_LEN];
    packet[..6].fill(0xFF);
    for segment in packet[6..].as_chunks_mut::<6>().0 {
        segment.copy_from_slice(mac);
    }
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_three_mac_forms() {
        let expected = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(normalize_mac("AA:BB:CC:DD:EE:FF"), Some(expected));
        assert_eq!(normalize_mac("aa-bb-cc-dd-ee-ff"), Some(expected));
        assert_eq!(normalize_mac("AABBCCDDEEFF"), Some(expected));
        assert_eq!(normalize_mac("  aA:bB:cC:dD:eE:fF  "), Some(expected));
    }

    #[test]
    fn rejects_invalid_mac() {
        for bad in [
            "",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:FF:00",
            "AA:BB-CC:DD:EE:FF",
            "GG:BB:CC:DD:EE:FF",
            "AABBCCDDEEF",
            "AABBCCDDEEFF00",
            "00:00:00:00:00:00",
            "FF-FF-FF-FF-FF-FF",
            // 首字节最低位为 1 是组播地址，不对应任何一块网卡。
            "01:00:5E:00:00:01",
        ] {
            assert_eq!(normalize_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn builds_standard_magic_packet() {
        let mac = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB];
        let packet = magic_packet(&mac);
        assert_eq!(packet.len(), 102);
        assert!(packet[..6].iter().all(|byte| *byte == 0xFF));
        for segment in packet[6..].as_chunks::<6>().0 {
            assert_eq!(segment, &mac);
        }
    }

    #[test]
    fn normalizes_broadcast_address() {
        assert_eq!(
            normalize_broadcast("192.168.1.255").as_deref(),
            Some("192.168.1.255")
        );
        assert_eq!(
            normalize_broadcast("192.168.1.255:9").as_deref(),
            Some("192.168.1.255")
        );
        assert_eq!(
            normalize_broadcast(" 192.168.1.255:7 ").as_deref(),
            Some("192.168.1.255")
        );
        assert_eq!(
            normalize_broadcast("255.255.255.255").as_deref(),
            Some("255.255.255.255")
        );
    }

    #[test]
    fn rejects_invalid_broadcast() {
        for bad in [
            "",
            "   ",
            "192.168.1.300",
            "192.168.1",
            "not-an-ip",
            "::1",
            "192.168.1.255:",
            "192.168.1.255:abc",
            "192.168.1.255:9:9",
            "192.168.1.255:70000",
            // 公网地址不允许：避免被当成向任意主机发 UDP 的放大器。
            "8.8.8.8",
            "1.1.1.1:9",
            "172.32.0.1",
            "192.169.1.255",
        ] {
            assert_eq!(normalize_broadcast(bad), None, "{bad}");
        }
    }

    #[test]
    fn accepts_private_broadcast_ranges() {
        for good in [
            "10.0.0.255",
            "10.255.255.255:9",
            "172.16.0.255",
            "172.31.255.255",
            "192.168.0.255",
            "255.255.255.255",
        ] {
            assert!(normalize_broadcast(good).is_some(), "{good}");
        }
    }

    #[test]
    fn rejects_invalid_input_before_sending() {
        assert!(send_magic_packet("nope", "192.168.1.255").is_err());
        assert!(send_magic_packet("AA:BB:CC:DD:EE:FF", "nope").is_err());
    }
}
