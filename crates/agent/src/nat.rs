//! NAT 穿透：STUN 探测公网映射 + UDP 打洞。
//!
//! 局域网直连（`direct.rs`）走 TCP，同网段没问题；跨网段时 TCP 的
//! `IP:端口` 在 NAT 后面根本连不到，必须先把两边的公网映射探出来、
//! 再让双方同时向对方发包把 NAT 的洞打通。
//!
//! 这个模块只做两件不依赖业务协议的事：
//!
//! 1. [`discover`]：向公共 STUN 服务器发 Binding Request，拿到本机的
//!    公网 `IP:端口`（RFC 5389）。
//! 2. [`PunchSocket`]：一个绑定在固定端口的 UDP socket，先由双方互发
//!    「打洞包」打通 NAT，打通后原样交给 QUIC 端点继续用。
//!
//! **不引入任何自建服务器**：STUN 用公共的（Google / Cloudflare / 腾讯），
//! 打洞之后数据直接在两端之间走。只有对称型 NAT 才需要 TURN 中继，
//! 那种情况继续走原来的服务器中转，不影响这里的路径。
//!
//! ## 为什么打洞要「同时」
//!
//! NAT 只放行「本机先发出去过」的回包。A 先发到 B 的公网地址，包会在 B 的
//! NAT 上被丢；但 A 的 NAT 因此记下了一条映射。等 B 也向 A 发一次，B 的
//! NAT 同样记下映射，此时 A 再发的包就能穿过 B 的 NAT。所以两边都要在
//! 一段时间内持续向对方发包，直到收到对方的包为止 —— 这就是 [`punch`]。

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::time::{Duration, Instant};

/// 公共 STUN 服务器。按顺序尝试，第一个成功的就用。
///
/// 都是公开的免费服务，只用来探测映射，不传输任何业务数据。
pub const STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.qq.com:3478",
    "stun.miwifi.com:3478",
];

/// STUN magic cookie（RFC 5389 固定值）。
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Binding Request。
const BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response。
const BINDING_SUCCESS: u16 = 0x0101;
/// XOR-MAPPED-ADDRESS 属性。
const ATTR_XOR_MAPPED: u16 = 0x0020;
/// 老式 MAPPED-ADDRESS 属性（部分服务器仍只回这个）。
const ATTR_MAPPED: u16 = 0x0001;

/// 一次 STUN 探测的超时。公共服务器通常几十毫秒就回。
const STUN_TIMEOUT: Duration = Duration::from_secs(2);
/// 默认探测预算：依次试完 [`STUN_SERVERS`] 的上限。
const STUN_BUDGET: Duration = Duration::from_secs(12);

/// 打洞 socket 的接收缓冲。
///
/// 打洞成功后对端会在同一个 socket 上直接开始 QUIC 握手，Initial 约 1.2 KB；
/// 缓冲小于它就收不到「来源地址」，Windows 还会报 WSAEMSGSIZE（10040）。
const PUNCH_BUF: usize = 4096;

/// 打洞循环里单次 `recv_from` 的等待时间。
///
/// 要短：每等一轮就补发一次探测包，双方都在持续发包，NAT 的洞才开得快。
const PUNCH_POLL: Duration = Duration::from_millis(200);

/// 本机在公网上的映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    /// 公网地址（STUN 服务器看到的来源地址）。
    pub public: SocketAddr,
    /// 本机内网地址（同一个 socket 的本地地址）。
    pub local: SocketAddr,
}

impl Mapping {
    /// NAT 是否「锥形」（cone）：公网端口和内网端口一致通常说明是
    /// full-cone / restricted-cone，打洞成功率高。
    ///
    /// 不一致不能断定是对称型 —— 端口也可能只是恰好被映射到别的号，
    /// 所以这里只用来给界面提示，不用来决定走不走打洞。
    pub fn port_preserved(&self) -> bool {
        self.public.port() == self.local.port()
    }
}

/// 向公共 STUN 服务器探测本机公网映射。
///
/// 复用传入的 `socket`（这样探测到的映射端口就是打洞要用的那个端口），
/// 依次尝试 [`STUN_SERVERS`]，全失败返回最后一个错误。
///
/// 全程用 `send_to` / `recv_from`，**不调用 `connect`**：Windows 上把 UDP
/// socket `connect` 到未指定地址并不能可靠地解除绑定，那样后续打洞会发不出去。
pub fn discover(socket: &UdpSocket) -> io::Result<Mapping> {
    discover_within(socket, STUN_BUDGET)
}

/// 带总预算的 STUN 探测。
///
/// 有的网络会把 UDP 直接吞掉（不回包也不报错），这时每个服务器都要等满
/// 读超时，一轮探测能拖到十几秒。会话建立前只愿意花几秒钟试 P2P 的场景
/// （同账号直连）用这个版本，超了就放弃、走中转。
pub fn discover_within(socket: &UdpSocket, budget: Duration) -> io::Result<Mapping> {
    let local = socket.local_addr()?;
    let deadline = Instant::now() + budget;
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "没有可用的 STUN 服务器");

    for server in STUN_SERVERS {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(addrs) = resolve(server) else {
            continue;
        };
        for addr in addrs {
            if Instant::now() >= deadline {
                break;
            }
            match query(socket, addr, deadline) {
                Ok(public) => {
                    // 探测期间读超时被调到了最多 2 秒；打洞要勤发包，恢复短超时。
                    let _ = socket.set_read_timeout(Some(PUNCH_POLL));
                    return Ok(Mapping { public, local });
                }
                Err(error) => {
                    tracing::debug!("STUN {server}（{addr}）探测失败: {error}");
                    last_error = error;
                }
            }
        }
    }
    let _ = socket.set_read_timeout(Some(PUNCH_POLL));
    Err(last_error)
}

/// 解析 `host:port`，只返回 IPv4 地址。
///
/// 打洞 socket 绑的是 `0.0.0.0`（IPv4）。DNS 往往把 IPv6 排在前面
/// （Cloudflare、Google 的 STUN 域名都是），拿 IPv6 地址去 `send_to`
/// 会直接报「地址族不支持」，整轮探测就白跑了 —— 这正是之前日志里
/// 「STUN 响应里没有映射地址」的根因。
fn resolve(target: &str) -> io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = target.to_socket_addrs()?.collect();
    let v4: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|addr| addr.is_ipv4())
        .collect();
    if v4.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "解析不到 IPv4 地址",
        ));
    }
    Ok(v4)
}

/// 发一次 Binding Request 并解析响应里的映射地址。
///
/// `deadline` 是整轮探测的截止时刻：每次等待都按剩余预算收紧，
/// 静默丢包的服务器不会把后面的候选服务器一起拖死。
fn query(socket: &UdpSocket, server: SocketAddr, deadline: Instant) -> io::Result<SocketAddr> {
    let mut txid = [0u8; 12];
    getrandom::getrandom(&mut txid)
        .map_err(|error| io::Error::other(format!("随机数失败: {error}")))?;

    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    request.extend_from_slice(&0u16.to_be_bytes()); // 没有属性
    request.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    request.extend_from_slice(&txid);

    socket.send_to(&request, server)?;

    let mut buf = [0u8; 512];
    // 有没有收到过这个服务器的包：区分「完全没回」和「回了但解析不出地址」，
    // 排查时这两种情况含义完全不同。
    let mut saw_response = false;
    // 可能收到杂包（别的进程的探测回包、无关的 UDP 流量），重试几次。
    for _ in 0..4 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // 单次最多等 `STUN_TIMEOUT`，但不超过总预算：每轮都很短，
        // 好让外层的 deadline 及时生效。
        socket.set_read_timeout(Some(left.min(STUN_TIMEOUT)))?;
        match socket.recv_from(&mut buf) {
            Ok((read, from)) => {
                // 只认我们发过去的那个服务器回包。
                if from.ip() != server.ip() {
                    continue;
                }
                saw_response = true;
                if let Some(mapped) = parse_response(&buf[..read], &txid) {
                    return Ok(mapped);
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        if saw_response {
            "STUN 响应里没有映射地址"
        } else {
            "STUN 服务器无响应（UDP 出站可能被拦截）"
        },
    ))
}

/// 解析 Binding Success Response，取出映射地址。
///
/// 校验事务 ID，避免把别的探测的回包当成自己的。
fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 {
        return None;
    }
    let kind = u16::from_be_bytes([buf[0], buf[1]]);
    if kind != BINDING_SUCCESS {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC_COOKIE {
        return None;
    }
    if &buf[8..20] != txid {
        return None;
    }
    let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (20 + length).min(buf.len());

    let mut offset = 20;
    let mut plain: Option<SocketAddr> = None;
    while offset + 4 <= end {
        let attr = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start + len;
        if value_end > end {
            break;
        }
        let value = &buf[value_start..value_end];
        match attr {
            ATTR_XOR_MAPPED => {
                if let Some(addr) = decode_address(value, txid, true) {
                    // XOR-MAPPED 优先，直接返回。
                    return Some(addr);
                }
            }
            ATTR_MAPPED => {
                if let Some(addr) = decode_address(value, txid, false) {
                    plain = Some(addr);
                }
            }
            _ => {}
        }
        // 属性按 4 字节对齐。
        offset = value_start + len.div_ceil(4) * 4;
    }
    plain
}

/// 解析 MAPPED-ADDRESS / XOR-MAPPED-ADDRESS 的属性值。
///
/// 格式：1 字节保留 + 1 字节 family + 2 字节端口 + 地址。
/// `xor` 为真时端口和地址都要和 magic cookie（IPv6 还要拼上事务 ID）异或。
fn decode_address(value: &[u8], txid: &[u8; 12], xor: bool) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    match family {
        0x01 => {
            if value.len() < 8 {
                return None;
            }
            let mut octets = [value[4], value[5], value[6], value[7]];
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (index, byte) in octets.iter_mut().enumerate() {
                    *byte ^= cookie[index];
                }
            }
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(octets),
                port,
            )))
        }
        0x02 => {
            if value.len() < 20 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (index, byte) in octets.iter_mut().take(4).enumerate() {
                    *byte ^= cookie[index];
                }
                for (index, byte) in octets.iter_mut().skip(4).enumerate() {
                    *byte ^= txid[index];
                }
            }
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(octets),
                port,
                0,
                0,
            )))
        }
        _ => None,
    }
}

/// 打洞用的 UDP socket。
///
/// 生命周期：绑定固定端口 → 探测映射 → 与对端互发打洞包 → 成功后把
/// 底层 socket 交给 QUIC（[`PunchSocket::into_socket`]）。
pub struct PunchSocket {
    socket: UdpSocket,
    mapping: Option<Mapping>,
}

impl PunchSocket {
    /// 绑定本机端口。端口为 0 时由系统分配。
    ///
    /// 绑定 `0.0.0.0` 是为了多网卡都能收；真正的可达性由 NAT 决定。
    pub fn bind(port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
        socket.set_read_timeout(Some(PUNCH_POLL))?;
        socket.set_write_timeout(Some(PUNCH_POLL))?;
        Ok(Self {
            socket,
            mapping: None,
        })
    }

    /// 本地地址。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// 探测并缓存公网映射。
    pub fn discover(&mut self) -> io::Result<Mapping> {
        let mapping = discover(&self.socket)?;
        self.mapping = Some(mapping);
        Ok(mapping)
    }

    /// 带总预算的探测：到点就放弃，不为了一个公网映射把会话建立拖住。
    pub fn discover_within(&mut self, budget: Duration) -> io::Result<Mapping> {
        let mapping = discover_within(&self.socket, budget)?;
        self.mapping = Some(mapping);
        Ok(mapping)
    }

    /// 已探测到的映射（没探测过返回 None）。
    pub fn mapping(&self) -> Option<Mapping> {
        self.mapping
    }

    /// 与对端互相打洞，直到收到对方的包或超时。
    ///
    /// `peers` 是对端可能的地址（公网映射 + 内网地址都试一遍，
    /// 同一网段时内网地址直接就能通）。
    ///
    /// 返回实际收到响应的那个地址 —— 后续 QUIC 就认这个地址。
    pub fn punch(&self, peers: &[SocketAddr], timeout: Duration) -> io::Result<SocketAddr> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; PUNCH_BUF];
        // 打洞包内容随意，只要不是空包（空包在部分 NAT 上会被丢）。
        let probe = b"screenwall-punch-v1";
        let mut peers: Vec<SocketAddr> = peers.to_vec();
        let mut last_error = None;

        while Instant::now() < deadline {
            for peer in &peers {
                // 发不出去不算失败：NAT 可能还在等对方的包。
                if let Err(error) = self.socket.send_to(probe, peer) {
                    last_error = Some(error);
                }
            }
            // 等一小会儿再发下一轮，避免打爆 NAT 的会话表。
            match self.socket.recv_from(&mut buf) {
                Ok((read, from)) => {
                    if read == 0 {
                        continue;
                    }
                    // 收到对方的任何包就说明双向都通了。对端打洞成功后会在
                    // 同一个 socket 上立刻开始 QUIC 握手，先到的可能是它
                    // 1.2 KB 的 Initial —— 这也算打通。
                    if peers.iter().any(|peer| same_host(peer, &from)) {
                        tracing::info!(peer = %from, bytes = read, "UDP 打洞成功");
                        return Ok(from);
                    }
                    // 来自未知地址：可能是对方换了端口，加入候选继续试。
                    if !peers.iter().any(|peer| peer == &from) {
                        tracing::debug!(from = %from, "打洞收到未知来源，加入候选");
                        peers.push(from);
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) if is_oversized(&error) => {
                    // 缓冲区仍装不下的数据报（Windows 报 WSAEMSGSIZE 10040，
                    // 而且拿不到来源地址）。能收到这么大的包说明洞已经通了：
                    // 对端多半已经开始 QUIC 握手，其 Initial 会自己重传。
                    // 按第一个候选继续，调用方拿它建 QUIC 端点/连接。
                    if let Some(peer) = peers.first().copied() {
                        tracing::warn!(%peer, "打洞收到超长数据报，按已打通处理");
                        return Ok(peer);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "打洞超时：双方 NAT 可能都是对称型，需要走服务器中转",
            )
        }))
    }

    /// 被控端用：等到主控端的打洞包，然后回打一轮确认。
    ///
    /// 为什么被控端不能只「监听」：NAT 会丢弃没见过的来源发来的包，
    /// 主控端的第一发包很可能到不了。但只要它到了（锥形 NAT 或路由器
    /// 做了 UDP 端口映射），这里立刻回包 —— 回包会在本机 NAT 上留下
    /// 一条到主控端的映射，之后 QUIC 就能双向跑。
    ///
    /// 返回主控端的地址，调用方拿它建立 QUIC 连接（作为服务端接受来自
    /// 这个地址的连接）。
    pub fn await_peer(&self, timeout: Duration) -> io::Result<SocketAddr> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; PUNCH_BUF];
        let probe = b"screenwall-punch-v1";
        while Instant::now() < deadline {
            match self.socket.recv_from(&mut buf) {
                Ok((read, from)) => {
                    if read == 0 {
                        continue;
                    }
                    // 只认我们的打洞包，别把无关的 UDP 流量当成对端。
                    if &buf[..read] != probe {
                        // 比探测包大的只可能是 QUIC：主控端已经打洞成功、
                        // 直接开始握手了。这个 Initial 被我们读掉了，quinn
                        // 会重传，返回地址让调用方起 QUIC 端点即可。
                        if read > probe.len() {
                            tracing::info!(%from, bytes = read, "收到 QUIC 握手包，跳过等待打洞包");
                            return Ok(from);
                        }
                        tracing::debug!(%from, "忽略非打洞包");
                        continue;
                    }
                    // 连回三发，保证至少一发能穿过对方的 NAT。
                    for _ in 0..3 {
                        let _ = self.socket.send_to(b"screenwall-punch-v1", from);
                    }
                    tracing::info!(peer = %from, "收到打洞包，已回打确认");
                    return Ok(from);
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "等待主控端打洞超时",
        ))
    }

    /// 取出底层 socket 交给 QUIC。
    pub fn into_socket(self) -> UdpSocket {
        self.socket
    }
}

/// 是否是「数据报比接收缓冲还大」的错误。
///
/// Windows 是 `WSAEMSGSIZE`（10040），Linux/macOS 是 `EMSGSIZE`（90）。
/// 遇到它时 `recv_from` 拿不到来源地址，只能当作「对端已经通了」处理。
fn is_oversized(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(10040) | Some(90))
}

/// 是否同一个 IP（端口可能因为 NAT 映射而不同）。
fn same_host(a: &SocketAddr, b: &SocketAddr) -> bool {
    match (a.ip(), b.ip()) {
        (IpAddr::V4(x), IpAddr::V4(y)) => x == y,
        (IpAddr::V6(x), IpAddr::V6(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_response(txid: &[u8; 12], attr: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        let padded = value.len().div_ceil(4) * 4;
        out.extend_from_slice(&((4 + padded) as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(txid);
        out.extend_from_slice(&attr.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
        out.resize(out.len() + (padded - value.len()), 0);
        out
    }

    #[test]
    fn parses_xor_mapped_ipv4() {
        let txid = [7u8; 12];
        // 192.0.2.1:32853 异或 magic cookie 后的编码。
        let ip = Ipv4Addr::new(192, 0, 2, 1);
        let port: u16 = 32853;
        let mut value = vec![0x00, 0x01];
        value.extend_from_slice(&(port ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
        let cookie = MAGIC_COOKIE.to_be_bytes();
        for (index, byte) in ip.octets().iter().enumerate() {
            value.push(byte ^ cookie[index]);
        }
        let response = build_response(&txid, ATTR_XOR_MAPPED, &value);
        let parsed = parse_response(&response, &txid).unwrap();
        assert_eq!(parsed, SocketAddr::from((ip, port)));
    }

    #[test]
    fn parses_plain_mapped_ipv4() {
        let txid = [3u8; 12];
        let mut value = vec![0x00, 0x01, 0x1F, 0x90]; // 8080
        value.extend_from_slice(&[10, 0, 0, 5]);
        let response = build_response(&txid, ATTR_MAPPED, &value);
        let parsed = parse_response(&response, &txid).unwrap();
        assert_eq!(parsed, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 8080)));
    }

    #[test]
    fn rejects_wrong_transaction_id() {
        let txid = [1u8; 12];
        let mut value = vec![0x00, 0x01, 0x00, 0x50];
        value.extend_from_slice(&[10, 0, 0, 5]);
        let response = build_response(&txid, ATTR_MAPPED, &value);
        // 换个事务 ID 再解析：必须被拒，否则会把别人的回包当成自己的。
        assert!(parse_response(&response, &[2u8; 12]).is_none());
    }

    #[test]
    fn rejects_truncated_and_non_success() {
        assert!(parse_response(&[], &[0u8; 12]).is_none());
        assert!(parse_response(&[0u8; 8], &[0u8; 12]).is_none());
        // Binding Request（0x0001）不是响应。
        let mut request = Vec::new();
        request.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        request.extend_from_slice(&0u16.to_be_bytes());
        request.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        request.extend_from_slice(&[0u8; 12]);
        assert!(parse_response(&request, &[0u8; 12]).is_none());
    }

    #[test]
    fn skips_unknown_attributes_before_mapped() {
        let txid = [9u8; 12];
        // 手工拼一个带前置属性的响应：SOFTWARE(0x8022) 然后 MAPPED。
        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        let software = b"stun-test";
        let mapped = {
            let mut value = vec![0x00, 0x01, 0x1F, 0x90]; // 8080
            value.extend_from_slice(&[10, 0, 0, 7]);
            value
        };
        let padded_software = software.len().div_ceil(4) * 4;
        let length = 4 + padded_software + 4 + mapped.len();
        out.extend_from_slice(&(length as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&txid);
        out.extend_from_slice(&0x8022u16.to_be_bytes());
        out.extend_from_slice(&(software.len() as u16).to_be_bytes());
        out.extend_from_slice(software);
        out.resize(out.len() + (padded_software - software.len()), 0);
        out.extend_from_slice(&ATTR_MAPPED.to_be_bytes());
        out.extend_from_slice(&(mapped.len() as u16).to_be_bytes());
        out.extend_from_slice(&mapped);

        let parsed = parse_response(&out, &txid).unwrap();
        assert_eq!(parsed, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 7), 8080)));
    }

    #[test]
    fn port_preserved_flag() {
        let mapping = Mapping {
            public: SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), 47902)),
            local: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 2), 47902)),
        };
        assert!(mapping.port_preserved());
        let mapped = Mapping {
            public: SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), 54321)),
            local: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 2), 47902)),
        };
        assert!(!mapped.port_preserved());
    }

    /// 真机联网验证：`cargo test -p screenwall-agent --release nat::stun_discovers -- --ignored --nocapture`
    ///
    /// 之前 DNS 返回的 IPv6 地址排在前面，`send_to` 直接报地址族不支持，
    /// 公网映射永远探测不到，跨网段 P2P 就全落回中转。
    #[test]
    #[ignore = "需要网络；验证 STUN 能拿到公网映射"]
    fn stun_discovers_public_mapping() {
        let mut punch = PunchSocket::bind(0).unwrap();
        match punch.discover_within(Duration::from_secs(8)) {
            Ok(mapping) => println!(
                "公网映射 {}（内网 {}，端口保持 {}）",
                mapping.public,
                mapping.local,
                mapping.port_preserved()
            ),
            Err(error) => panic!("STUN 探测失败: {error}"),
        }
    }

    #[test]
    fn punch_socket_binds_and_reports_local() {
        let socket = PunchSocket::bind(0).unwrap();
        let addr = socket.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        assert!(socket.mapping().is_none());
    }

    #[test]
    fn same_host_ignores_port() {
        let a: SocketAddr = "1.2.3.4:1000".parse().unwrap();
        let b: SocketAddr = "1.2.3.4:2000".parse().unwrap();
        let c: SocketAddr = "1.2.3.5:1000".parse().unwrap();
        assert!(same_host(&a, &b));
        assert!(!same_host(&a, &c));
    }
}
