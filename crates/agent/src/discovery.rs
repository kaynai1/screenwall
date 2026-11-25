//! 局域网直连自动发现（UDP 广播）。
//!
//! 采用「谁问谁答」：主控端广播一个固定探测报文，被控端收到后把自己的
//! 设备名、直连端口和证书指纹用 JSON 回给来源地址。相比让被控端周期组播
//! 在线状态，这种方式平时不占局域网带宽，也不用维护在线表；代价是每次
//! 打开「发现设备」都要等一轮超时。
//!
//! 报文格式故意保持极简：探测报文是固定字节串，应答是
//! [`Announcement`] 的 JSON。发现端口与直连 TCP 端口分开，便于防火墙
//! 只放行 UDP 探测而不暴露更多入口。

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use screenwall_proto::PROTO_VERSION;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::time::Instant;

/// 发现服务使用的 UDP 端口。
pub const DISCOVERY_PORT: u16 = 47901;

/// 探测报文固定内容。
pub const PROBE: &[u8] = b"SCREENWALL-DISCOVER-1";

/// 公告里 `app` 字段的固定值，用来过滤同一端口上的无关 UDP 报文。
const APP_ID: &str = "screenwall";

/// 单个 UDP 报文的最大长度。公告只有几百字节，留足余量即可，
/// 避免异常大包反复撑大接收缓冲。
const MAX_DATAGRAM: usize = 64 * 1024;

/// 被控端广播的自身信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    /// 固定为 "screenwall"，用来过滤无关报文。
    pub app: String,
    /// 设备名（主机名），界面显示用。
    pub name: String,
    /// 直连 TCP 端口。
    pub port: u16,
    /// 直连证书指纹（大写十六进制、冒号分隔）。
    pub fingerprint: String,
    /// 协议版本。
    pub proto: u32,
    /// 本机识别码（9 位数字）。
    ///
    /// 主控端输入识别码后广播一次，只有识别码对得上的那台会连过来 ——
    /// 这样「输识别码就能连」在纯局域网模式下也成立，不需要服务器。
    /// 老版本没有这个字段，`#[serde(default)]` 保证仍能互相发现。
    #[serde(default)]
    pub id: String,
}

impl Announcement {
    /// 构造一个 app 字段已填好的公告。
    ///
    /// `proto` 直接取主协议版本，主控端据此判断要不要提示版本不匹配；
    /// 该字段不参与过滤，老版本仍能被发现。
    pub fn new(name: impl Into<String>, port: u16, fingerprint: impl Into<String>) -> Self {
        Self {
            app: APP_ID.to_string(),
            name: name.into(),
            port,
            fingerprint: fingerprint.into(),
            proto: u32::from(PROTO_VERSION),
            id: String::new(),
        }
    }

    /// 带上本机识别码。
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
}

/// 主控端扫描到的一台设备。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    /// 直连地址，形如 "192.168.1.20:47900"（IPv4）。
    pub address: String,
    /// 设备名。
    pub name: String,
    /// 直连 TCP 端口。
    pub port: u16,
    /// 直连证书指纹。
    pub fingerprint: String,
    /// 对方识别码；老版本为空。
    #[serde(default)]
    pub id: String,
}

/// 被控端应答器。
pub struct Responder {
    socket: UdpSocket,
    info: Announcement,
}

impl Responder {
    /// 绑定 UDP 端口。`port` 传 0 表示由系统分配（测试用）。
    ///
    /// 打开广播权限是为了与主控端保持同一条代码路径：即使某些平台上
    /// 应答偶尔走广播地址，也能发出去。
    pub async fn bind(port: u16, info: Announcement) -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
        socket.set_broadcast(true)?;
        Ok(Self { socket, info })
    }

    /// 实际绑定的本地地址。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// 循环应答探测，直到 `cancel` 变成 true。
    ///
    /// 收到非探测报文一律静默丢弃；单个收包/发包失败只记日志，不能让
    /// 一次瞬时错误（例如 Windows 上的 ICMP 端口不可达）终止整个应答器，
    /// 否则用户得重新打开开关才能再被发现。
    pub async fn run(self, mut cancel: watch::Receiver<bool>) {
        // 公告内容在运行期不变，先序列化一次，避免每个探测都重复编码。
        let payload = match serde_json::to_vec(&self.info) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(error = %error, "发现公告序列化失败，应答器退出");
                return;
            }
        };

        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            // `changed()` 只会在下一次发送后返回；如果启动时取消位已经置位，
            // 必须先查当前值，否则会一直等到进程退出。
            if *cancel.borrow() {
                break;
            }

            tokio::select! {
                changed = cancel.changed() => {
                    // 发送端被丢弃也视为取消，避免任务永久挂着。
                    if changed.is_err() || *cancel.borrow() {
                        break;
                    }
                }
                received = self.socket.recv_from(&mut buf) => {
                    match received {
                        Ok((len, from)) => {
                            if &buf[..len] != PROBE {
                                continue;
                            }
                            if let Err(error) = self.socket.send_to(&payload, from).await {
                                tracing::debug!(%from, error = %error, "发现应答发送失败");
                            }
                        }
                        Err(error) => {
                            tracing::debug!(error = %error, "发现应答器收包失败");
                        }
                    }
                }
            }
        }
    }
}

/// 绑定并后台启动应答器；绑定失败返回错误（调用方打日志即可，不要 panic）。
pub async fn spawn(
    port: u16,
    info: Announcement,
    cancel: watch::Receiver<bool>,
) -> io::Result<tokio::task::JoinHandle<()>> {
    let responder = Responder::bind(port, info).await?;
    Ok(tokio::spawn(responder.run(cancel)))
}

/// 向 255.255.255.255:DISCOVERY_PORT 广播探测，收集 `timeout` 内的应答。
///
/// 只发广播、不扫网段，是为了不产生「端口扫描」式的流量；跨网段场景
/// 需要调用方用 [`scan_with_targets`] 自己指定目标。
pub async fn scan(timeout: Duration) -> Vec<Discovered> {
    let targets = [format!("255.255.255.255:{DISCOVERY_PORT}")];
    scan_with_targets(&targets, timeout).await
}

/// 向指定目标发探测（单播或广播地址均可），主要给测试与多网卡场景用。
///
/// 目标可以是裸 IP（自动补 [`DISCOVERY_PORT`]）或完整的 `IP:端口`。
/// 结果按直连地址去重，因此同一目标重复出现也只返回一台设备。
pub async fn scan_with_targets(targets: &[String], timeout: Duration) -> Vec<Discovered> {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(socket) => socket,
        Err(error) => {
            tracing::debug!(error = %error, "发现扫描绑定本地端口失败");
            return Vec::new();
        }
    };
    // 单播目标不依赖广播权限；即使平台禁止广播，也继续尝试发送。
    if let Err(error) = socket.set_broadcast(true) {
        tracing::debug!(error = %error, "发现扫描打开广播权限失败");
    }

    for target in targets {
        let Some(addr) = probe_target(target) else {
            tracing::debug!(%target, "发现目标格式无效，已跳过");
            continue;
        };
        if let Err(error) = socket.send_to(PROBE, addr).await {
            tracing::debug!(%target, error = %error, "发现探测发送失败");
        }
    }

    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            // 到点没收到更多应答，扫描结束。
            Err(_) => break,
            Ok(Err(error)) => {
                tracing::debug!(error = %error, "发现扫描收包失败");
                // Windows 上 ICMP 端口不可达会把错误抛到 UDP recv；
                // 立刻重试会空转，短暂让出线程再继续等应答。
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(Ok((len, from))) => {
                let Ok(announcement) = serde_json::from_slice::<Announcement>(&buf[..len]) else {
                    // 同一端口上的无关 UDP 流量很常见，静默忽略。
                    continue;
                };
                if announcement.app != APP_ID {
                    continue;
                }

                // 应答里的 port 是直连 TCP 端口，不能拿发现端口的来源端口拼地址。
                let address = SocketAddr::new(from.ip(), announcement.port).to_string();
                if !seen.insert(address.clone()) {
                    continue;
                }
                found.push(Discovered {
                    address,
                    name: announcement.name,
                    port: announcement.port,
                    fingerprint: announcement.fingerprint,
                    id: announcement.id,
                });
            }
        }
    }

    found
}

/// 把扫描目标补成可直接 `send_to` 的地址。
///
/// 主机名可能触发阻塞 DNS，`scan` 是异步函数，这里只接受 IP 和 `IP:端口`，
/// 需要解析主机的场景交给调用方先自己处理。
fn probe_target(target: &str) -> Option<SocketAddr> {
    let target = target.trim();
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, DISCOVERY_PORT));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcement() -> Announcement {
        Announcement::new("测试机", 47900, "AA:BB:CC:DD:EE:FF")
    }

    #[test]
    fn announcement_json_roundtrip_uses_camel_case() {
        let info = announcement();
        assert_eq!(info.app, "screenwall");
        assert_eq!(info.proto, u32::from(PROTO_VERSION));

        let value = serde_json::to_value(&info).expect("公告应能序列化");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("公告 JSON 应是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["app", "fingerprint", "id", "name", "port", "proto"],
            "字段名应是 camelCase"
        );

        let decoded: Announcement = serde_json::from_value(value).expect("公告应能反序列化");
        assert_eq!(decoded, info);
    }

    #[test]
    fn old_announcement_without_id_still_decodes() {
        // 老版本被控端没有 `id` 字段。主控端升级后必须还能发现它们，
        // 否则「输 IP 直连」这条路会跟着一起坏掉。
        let json = r#"{"app":"screenwall","name":"老机器","port":47900,
                      "fingerprint":"AA:BB","proto":1}"#;
        let decoded: Announcement = serde_json::from_str(json).expect("缺 id 也应能解析");
        assert_eq!(decoded.id, "");
        assert_eq!(decoded.name, "老机器");
    }

    #[test]
    fn announcement_carries_local_id() {
        let info = announcement().with_id("123456789");
        assert_eq!(info.id, "123456789");
        let value = serde_json::to_value(&info).expect("公告应能序列化");
        assert_eq!(value["id"], "123456789");
    }

    #[tokio::test]
    async fn scan_returns_responder_id() {
        let info = announcement().with_id("987654321");
        let responder = Responder::bind(0, info).await.expect("绑定应答器");
        let port = responder.local_addr().expect("取本地地址").port();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(responder.run(cancel_rx));

        let found =
            scan_with_targets(&[format!("127.0.0.1:{port}")], Duration::from_millis(800)).await;

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "987654321", "识别码要原样带回主控端");

        let _ = cancel_tx.send(true);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn scan_finds_local_responder() {
        let info = announcement();
        let responder = Responder::bind(0, info.clone()).await.expect("绑定应答器");
        let port = responder.local_addr().expect("取本地地址").port();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(responder.run(cancel_rx));

        let found =
            scan_with_targets(&[format!("127.0.0.1:{port}")], Duration::from_millis(800)).await;

        assert_eq!(found.len(), 1, "应恰好发现一台设备");
        let device = &found[0];
        // address 里的端口是公告的直连端口，不是本次探测发现的 ephemeral 端口。
        assert_eq!(device.address, format!("127.0.0.1:{}", info.port));
        assert_eq!(device.name, info.name);
        assert_eq!(device.port, info.port);
        assert_eq!(device.fingerprint, info.fingerprint);

        let _ = cancel_tx.send(true);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn duplicate_targets_are_deduplicated() {
        let info = announcement();
        let responder = Responder::bind(0, info).await.expect("绑定应答器");
        let port = responder.local_addr().expect("取本地地址").port();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(responder.run(cancel_rx));

        let target = format!("127.0.0.1:{port}");
        let found = scan_with_targets(&[target.clone(), target], Duration::from_millis(800)).await;

        assert_eq!(found.len(), 1, "同一地址重复应答只保留一条");

        let _ = cancel_tx.send(true);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn non_probe_packets_get_no_reply() {
        let responder = Responder::bind(0, announcement())
            .await
            .expect("绑定应答器");
        let port = responder.local_addr().expect("取本地地址").port();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(responder.run(cancel_rx));

        let prober = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("绑定探测端");
        let responder_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        prober
            .send_to(b"SCREENWALL-DISCOVER-2", responder_addr)
            .await
            .expect("发送垃圾报文");

        let mut buf = [0u8; 256];
        let reply =
            tokio::time::timeout(Duration::from_millis(300), prober.recv_from(&mut buf)).await;
        assert!(reply.is_err(), "非探测报文不应有回应");

        // 确认垃圾报文没有把应答器弄坏，真正的探测仍然有结果。
        let found =
            scan_with_targets(&[format!("127.0.0.1:{port}")], Duration::from_millis(500)).await;
        assert_eq!(found.len(), 1, "应答器应继续工作");

        let _ = cancel_tx.send(true);
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn probe_target_adds_default_port_only_for_bare_ip() {
        assert_eq!(
            probe_target("192.168.1.20"),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
                DISCOVERY_PORT
            ))
        );
        assert_eq!(
            probe_target("127.0.0.1:1234"),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234))
        );
        assert_eq!(probe_target("not-an-ip"), None);
    }
}
