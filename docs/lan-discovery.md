# 局域网自动发现

同网段的主控端不用再手抄对方 IP：被控端开启「局域网直连」后会在 UDP 上回答探测，主控端在设备页「直连」弹窗里点「扫描同网段设备」就能列出来。

## 协议

自定义的极简 UDP 协议，没有引入 mDNS/Avahi 依赖。

| 项 | 值 |
| --- | --- |
| 端口 | `47901`（`crates/agent/src/discovery.rs` 的 `DISCOVERY_PORT`） |
| 探测报文 | `SCREENWALL-DISCOVER-1`（精确匹配，其它内容静默丢弃） |
| 应答 | JSON，camelCase |
| 传输 | IPv4 广播 `255.255.255.255` 或单播 |

应答内容（`Announcement`）：

```json
{
  "app": "screenwall",
  "name": "DESKTOP-7Q2",
  "port": 47900,
  "fingerprint": "AB:CD:…",
  "proto": 1
}
```

- `app` 固定为 `screenwall`，主控端据此过滤无关报文。
- `port` 是**直连 TCP 端口**，主控端据此拼出 `192.168.1.20:47900`。
- `fingerprint` 是直连证书的 SHA-256，用来在界面上展示/将来做「已配对」判定。
- `proto` 当前取主协议版本 `screenwall_proto::PROTO_VERSION`。

## 生命周期

被控端：

- `direct::listen_loop` 每次成功建立 TCP 监听后，同时拉起 `discovery::spawn(47901, …)`；
- 关闭直连、端口被占用、或这次监听结束（`wait_disabled`）时，通过 `watch` 通道取消并 `abort` 应答任务；
- UDP 47901 绑定失败**不影响直连本身**，只打一条 `warn`。

主控端：

- `scan_direct_devices` 命令调用 `discovery::scan(Duration::from_millis(1200))`；
- 只发一次探测，在 1.2 秒窗口里收应答，按「来源 IP + 应答里的端口」去重；
- 同一网段有多台时全部列出；只有一台时弹窗自动填好地址。

## 防火墙

自动发现需要放行**入站 UDP 47901**，与直连的 TCP 47900 是两条独立规则：

| 规则名 | 协议 / 端口 | 定义位置 |
| --- | --- | --- |
| `ScreenWall Direct` | TCP 47900 | `firewall.rs` 的 `RULE_NAME` + `installer-hooks.nsh` |
| `ScreenWall Discovery` | UDP 47901 | `firewall.rs` 的 `DISCOVERY_RULE_NAME` + `installer-hooks.nsh` |

两条规则都只放行 ScreenWall 自己的 exe（`program=`），不是对整个端口开放。安装器装完即加，卸载即删；设置页的「允许局域网直连」会同时修复两条（缺 UDP 规则时 `ruleMatches` 为 `false`，界面会提示）。

> 旧版本升级上来的机器只有 TCP 规则，设置页会显示「规则不完整」，点一次「允许局域网直连」即可补齐。

## 已知限制

- 广播只发到 `255.255.255.255`，多网卡时可能只走默认出口网卡；需要覆盖多网段时可用 `scan_with_targets` 指定广播地址（目前界面未暴露）。
- 不做主机名解析：目标只接受裸 IP 或 `IP:端口`，避免在异步路径里做阻塞 DNS。
- 没有认证：扫描只列出「开启了直连」的设备，连上去仍然要配对码或长期 token。
