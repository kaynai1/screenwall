# 网页端 WebRTC P2P 方案（设计稿）

> 状态：**已按完整方案落地**（2026-11-28）。实现位置：
> `crates/server/src/webrtc.rs` + `ws.rs`、`crates/agent/src/webrtc_host.rs`、
> `web/src/api.ts` 的 `Viewer`、`web/src/pages/ControlPage.tsx`。
>
> 已实现：`t:"webrtc"` 信令（offer/answer/trickle ICE/reject/bye，TTL 与条数上限）、
> `POST /api/v1/p2p/ice`（STUN + coturn `use-auth-secret` 临时凭据）、被控端
> webrtc-rs answerer（H.264 + Opus 轨道、`ctrl` DataChannel 复用中转协议）、
> 网页端 3 秒建连超时后回退中继、`error 4104` 时切回中继、P2P 统计从
> `getStats()` 取、标题栏「直连」标记、P2P 下文件块 96 KiB。
>
> 尚未实现（后续）：ICE 重启（`disconnected` 后自动恢复）、PLI 主动请求关键帧、
> 被控端把 `sender.get_stats()` 回灌 `video.bitrate`、TURN 是否被用到的诊断上报。
>
> 目标读者：实现这套改造的开发者 / 后续会话。

## 1. 结论

浏览器里唯一能做 P2P 的技术是 **WebRTC**（`RTCPeerConnection`、DataChannel、
SRTP 媒体轨道），而 WebRTC 本身只在**安全上下文**里可用。所以：

- **HTTPS 是硬前提**，和「网页端启用 H.264」是同一个前提，不是二选一；
- WebRTC 走 UDP，NAT 打洞失败时需要 **TURN** 兜底，否则对称 NAT / 运营商
  CGNAT 下完全连不上；
- 收益：画面不再经过公网 TCP 中转，延迟和抖动明显下降；打洞成功时服务器
  不承担媒体带宽；浏览器自带抖动缓冲、NACK/PLI、拥塞控制（GCC），比现在
  「WebSocket + 自己做 ack 拥塞窗口」更接近桌面客户端直连的观感。

代价：被控端（Rust）要集成 WebRTC 实现、服务器要加信令与 TURN、网页端要加
传输抽象与另一条播放路径，客户端必须发新版本。**按一个人算，3–5 周**；
只做「DataChannel 先跑通、媒体沿用现有隧道帧」的 MVP 约 1–2 周。

## 2. 现状（改造的基础）

| 组件 | 现状 | 可复用点 |
|---|---|---|
| 中转链路 | `/ws/view` WebSocket；8 字节会话帧 + 24 字节媒体帧 | 网页端帧编解码不变 |
| 桌面客户端打洞 | `crates/client/src/p2p.rs` 的 `P2pHost`：STUN 探公网映射 → 双向打洞 → QUIC → `direct::serve_stream` | 打洞流程、授权回调、同账号免确认 |
| 信令 | 服务器 `p2p` 消息转发（`ws.rs` 的 `handle_view_p2p` / `handle_device_p2p`，`state.rs` 登记 request） | 授权与路由模型 |
| 直连会话协议 | `direct::serve_stream`：`open` / `open.ok` / `ping` / `pong` 是**普通 JSON**；会话建立后是**和网页端完全一样的隧道帧** | 会话逻辑零改动复用 |
| 能力协商 | 会话内 `Welcome` 回双方 caps 交集（H.264 等） | 可扩展 `webrtc` 能力 |
| 拥塞控制 | 中转侧：`MediaSlot` 只留最新帧 + `video.ack` 窗口；网页端自动降档 | P2P 侧要换成发送端限速（见 §6.4） |

**关键发现**：直连会话在 `open` 之后用的就是同一套隧道帧，所以网页端现有的
`protocol.ts`、`ControlPage` 的收发逻辑可以原样复用，只需要换一条「传输」。
这也意味着控制、输入、剪贴板、文件、声音的协议全部不用重写。

## 3. 目标与非目标

**目标**

1. 网页端（桌面浏览器 / 安卓 Chrome / iOS Safari）与被控端之间建立 WebRTC
   直连，画面、声音、输入都走这条链路；
2. 打洞失败或 WebRTC 建立超时，**无感回退**到现有中转链路，功能不降级；
3. 不破坏桌面客户端现有的 QUIC 打洞与中转逻辑。

**非目标**

- 网页端被控（网页端仍然只能控制别人）；
- 无 HTTPS 的部署也能用 P2P（做不到，浏览器不允许）；
- 让纯 `screenwall-agent` 二进制承担 WebRTC 服务端（没有证书、授权回调与
  直连白名单；沿用现有约定，由客户端宿主实现，见 `P2pProvider`）。

## 4. 总体设计

```
浏览器（网页端）                    账号服务器（信令 + 兜底）              被控端（客户端宿主）
┌──────────────┐   ① HTTPS 登录/令牌   ┌──────────────┐   ② 已认证 WS   ┌────────────────┐
│ Viewer       │ ───────────────────▶ │ /ws/view     │ ◀────────────── │ cloud 连接      │
│ Transport    │  ③ offer/answer/ICE  │  p2p 信令转发 │  ③ offer/answer │ P2pHost         │
│  ├ WebSocket │ ◀──────────────────▶ │  (校验+限流)  │ ──────────────▶ │  ├ QUIC(现有)   │
│  └ DataChannel│                     │ /p2p/ice     │                 │  └ WebRTC(新)   │
└──────┬───────┘                      └──────────────┘                 └───────┬────────┘
       │           ④ ICE（STUN 打洞；失败走 TURN）                              │
       └───────────────────────────────────────────────────────────────────────┘
                    视频/音频：SRTP 轨道　控制/文件：DataChannel
```

角色：**浏览器是 offerer，被控端是 answerer**。浏览器侧 API 完整、ICE 收集
由浏览器自己做；被控端只需要 `setRemoteDescription(offer)` → `createAnswer`
→ trickle ICE。

### 4.1 两种媒体承载方式

| | A. 全部走 DataChannel | B. 媒体走 RTP 轨道（推荐） |
|---|---|---|
| 视频 | 现有 24 字节媒体帧塞进 DataChannel | H.264 走 `TrackLocalStaticSample`，浏览器 `<video>` 播放 |
| 声音 | 现有隧道帧 | Opus 轨道 |
| 拥塞控制 | 自己看 `bufferedAmount` + `video.ack` | 浏览器 GCC + `getStats`（更成熟） |
| 抖动/丢包 | 自己处理，或整帧丢弃 | 浏览器 jitter buffer、NACK、PLI 自动关键帧 |
| 改动量 | 小：复用现有隧道协议 | 中：被控端要接 RTP 打包与发送统计 |
| 体验 | 接近现在中继，只是换成 UDP | 最接近桌面客户端/CrossDesk |

**建议**：MVP 用 A 快速验证连通与回退；正式版用 B 承载视频/音频，A 只承载
控制与数据（两条 DataChannel：`ctrl` 可靠有序、`data` 可靠用于文件）。

### 4.2 会话建立时序

```
浏览器                         服务器                        被控端
  │  ① WS 已连接（现有）          │                              │
  │  ② POST /p2p/ice              │                              │
  │ ────────────────────────────▶ │ 返回 STUN/TURN 临时凭据       │
  │  ③ ws: {t:"webrtc",event:"offer",deviceId,request,sdp}      │
  │ ────────────────────────────▶ │ 校验设备/账号/受限会话        │
  │                               │ ── 转发 offer（带 viewerId）─▶│
  │                               │                              │ createAnswer + ICE
  │                               │ ◀── answer / trickle ice ────│
  │ ◀── answer / trickle ice ──── │                              │
  │  ④ ICE 连通，DataChannel open                                 │
  │  ⑤ DataChannel: {"t":"open",...}（普通 JSON，同直连）         │
  │ ────────────────────────────────────────────────────────────▶│ 授权（同账号免确认规则）
  │ ◀──────────────── open.ok / welcome / status（隧道帧）───────│
  │  ⑥ 浏览器挂上视频轨道，开始 video.sub                          │
  │  ⑦ 成功：关闭中转会话（如果有）；失败：走现有中继              │
```

## 5. 信令协议（服务器 + 两端）

### 5.1 服务器新增消息

现有 `p2p` 消息只带 QUIC 候选地址（≤64 字节）与指纹，装不下 SDP。新增
`t: "webrtc"`，字段与限制：

| 字段 | 限制 | 说明 |
|---|---|---|
| `deviceId` | 已有 `valid_device_id` | 首次 offer 必带 |
| `request` | u32，非 0 | 浏览器分配，后续消息原样带回 |
| `event` | `offer` / `answer` / `ice` / `reject` | `ice` 双向 trickle |
| `sdp` | ≤ 16 KiB，仅字符串 | 只在 offer/answer |
| `ice` | ≤ 512 B | 单个候选，`ice` 事件 |
| `viewer` | ≤ 64 B | 首次 offer 带名字 |
| `msg` | ≤ 128 B | reject 原因 |

服务器侧（`crates/server/src/ws.rs`）：

- 新增 `handle_view_webrtc` / `handle_device_webrtc`；复用 `handle_view_p2p`
  的校验：账号下的设备、`controllable`、在线、连接码会话只能打自己那台；
- 新增登记表 `webrtc: HashMap<(viewer_id, request), {device_key, created}>`，
  TTL 60 秒，双方合计最多 64 条消息（防刷），`viewer` 断开时清理；
- 设备回包只认登记过的 `(viewerId, request)`，字段逐项重建，不原样透传；
- 指标：`webrtc_attempts / webrtc_answers / webrtc_rejects`。

> 不复用现有 `register_p2p/take_p2p`：那份是一次性、给「QUIC answer」用的；
> WebRTC 需要整段协商期的双向转发。

### 5.2 ICE 凭据接口

`POST /api/v1/p2p/ice`（Bearer 令牌）返回：

```json
{
  "iceServers": [
    { "urls": ["stun:stun.example.com:3478"] },
    { "urls": ["turn:turn.example.com:3478?transport=udp"],
      "username": "1720000000:alice", "credential": "base64(hmac)" }
  ],
  "ttl": 3600
}
```

TURN 凭据用 coturn 的 `use-auth-secret` 机制：`username = 过期时间戳 + ":" + 用户标识`，
`credential = base64(HMAC-SHA1(secret, username))`。密钥只放服务器配置
（`[webrtc] turn_secret`），接口按账号限流（例如每分钟 6 次）。

### 5.3 能力上报

被控端连上账号服务器时，在现有设备信息里加一项（新消息或复用 hello 字段）：

```json
{ "t": "device.info", "webrtc": ["datachannel", "h264"], "turn": true }
```

服务器存进设备的内存条目，`devices` 列表多一个 `webrtc: true`。网页端据此
决定「先试 P2P」还是直接走中转，避免对老版本被控端白等 2–3 秒。

## 6. 传输层设计

### 6.1 DataChannel 划分

| 通道 | 配置 | 承载 |
|---|---|---|
| `ctrl` | reliable + ordered | `open` / `ping` / 输入 / 剪贴板 / 视频控制（`video.sub`、`video.keyframe`、`video.bitrate`） |
| `data` | reliable + ordered | 文件传输（现有 `file.*` 协议，分块从 256 KiB 收到 64 KiB） |
| `media`（仅方案 A） | unreliable + unordered（`maxRetransmits: 0`） | 24 字节媒体帧；丢了就丢，靠关键帧请求恢复 |

消息边界：DataChannel 只有二进制消息，适配层加 1 字节前缀（`0`=文本 JSON，
`1`=二进制帧），两端共用。**不要**在 DataChannel 上再套一层 WebSocket。

### 6.2 复用现有会话逻辑

被控端把 `direct.rs` 里的「WS 消息循环」抽成不依赖流的函数：

```rust
serve_messages(state, viewer, frames: impl Stream<Item = WsMessage> + Sink<WsMessage>)
```

`direct::serve_stream` 与新的 WebRTC 宿主都调用它：直连喂 QUIC 流上的
tungstenite 消息，WebRTC 喂 DataChannel 适配出来的 `WsMessage`。授权、会话号、
`open.ok`、`welcome`、订阅、输入、文件全部照旧——**不复制会话代码**。

网页端同理：`Viewer` 抽一个 `Transport` 接口（`send(string | ArrayBuffer)`、
`onmessage`、`onopen`、`onclose`），`WebSocketTransport` 是现状，
`DataChannelTransport` 是新的；控制页、输入、文件、自适应逻辑不动。

### 6.3 媒体（方案 B）

- 视频：被控端编码器输出的 H.264 Annex-B 访问单元直接喂
  `TrackLocalStaticSample`（`webrtc` crate），由 RTP 打包器做分片；
  浏览器 `ontrack` → `<video autoplay muted playsinline>`。
- 音频：Opus 用 `TrackLocalStaticSample`；PCM16 需要先编码成 Opus（现有
  `audio` 模块已有编码），否则不提供音频轨道、退回 DataChannel PCM。
- 关键帧：浏览器 PLI → `on_pli` → 现有 `video.keyframe` 路径。
- 授权前不推流：轨道先协商成 `recvonly`，拿到会话授权前不 `write_sample`，
  避免「还没允许就看到画面」。
- 分辨率/帧率/压缩档：仍用现有 `video.sub` 控制；切分辨率会重建编码器，
  RTP 时间戳与 SSRC 保持不变。

### 6.4 拥塞与自适应

- 方案 B：浏览器 GCC 自动调发送码率；被控端每 500 ms 读一次
  `sender.get_stats()`（`available_outgoing_bitrate`、`rtt`、`fraction_lost`），
  通过现有 `video.bitrate` 的 ICodecAPI 热更新，不重建编码器。
- 方案 A：发送端看 `bufferedAmount`（> 512 KiB 丢非关键帧，> 1 MiB 停发视频），
  网页端继续回 `video.ack`；被控端按 ack 窗口限速（把服务器 `MediaSlot` 的
  逻辑搬成被控端的一份小实现）。
- 网页端 UI 的 fps / Mbps / 延迟：改从 `RTCPeerConnection.getStats()`
  （`inbound-rtp.framesPerSecond`、`bytesReceived`、`candidate-pair.currentRoundTripTime`）
  取，不再依赖媒体帧头估算；中转链路保持现状。
- 自动降档：保留，但 P2P 下优先让 WebRTC 自己调码率，连续 3 秒
  `qualityLimitationReason = "bandwidth"` 才降分辨率。

### 6.5 回退

- **建立阶段**：offer 发出后 2.5 秒内没连上（`connectionState` 不是
  `connected`），关闭 PeerConnection，走现有 `open` 中转；用户只看到
  「正在连接」多一两秒。
- **会话中断**：`iceConnectionState` 变 `disconnected` 后给 3 秒 ICE 重启
  （`restartIce`）；仍失败就按现有断线重连逻辑重开会话（先重试 P2P，
  再回退中转）。不做「同一会话中途迁移」，第一版接受一次重连。
- 失败原因写进诊断日志（`p2p: no candidate pair` / `turn unreachable`），
  方便用户反馈。

## 7. 被控端改造

1. 新增依赖 `webrtc`（webrtc-rs）与 `tokio-util`；先做吞吐与稳定性验证
   （Windows + 1080p H.264 + DataChannel 10 Mbps）。
2. `P2pHost` 增加 `webrtc` 分支：
   - 收 `webrtc` offer → 建 `RTCPeerConnection`（ICE 用 `/p2p/ice` 下发的列表）；
   - `on_data_channel` → 包成 `WsMessage` 流，交给 §6.2 的会话函数；
   - 媒体轨道在授权后开始推流；
   - 同一台机器最多 2 条 WebRTC 连接（与 QUIC 打洞并存，互不影响）。
3. 授权：沿用 `mark_auto` / `sameAccount` 规则；观察者名字取信令里的 `viewer`
   （服务器已校验来源）。
4. 统计：`webrtc.connected / failed / turn / rtt` 上报到 `status`，
   控制页能显示「直连（P2P）」还是「中转」。
5. 版本门槛：老客户端不实现该分支，信令无人应答，服务器 60 秒清登记；
   网页端靠 `devices[].webrtc` 判断，老设备不尝试。

## 8. 网页端改造

1. `Transport` 抽象（见 §6.2）：`Viewer` 不再直接 `new WebSocket`，改为
   `TransportFactory`；`connect()` 里按设备能力选传输。
2. `DataChannelTransport`：`ctrl` 通道收发隧道帧；`send` 前加 1 字节前缀。
3. 播放：方案 B 下 `video.srcObject = new MediaStream([track])`；保留现有
   canvas/WebCodecs 路径给中转与方案 A。PiP、全屏对 `<video>` 直接可用，
   iOS 的兼容性也比 canvas `captureStream` 好。
4. 首次连接流程：`GET /p2p/ice` → `new RTCPeerConnection` → `createOffer`
   → 经 WS 发 `webrtc` offer → 收 answer/ICE → `connected` → 发 `open`。
5. 界面：设备页与控制页显示链路类型胶囊（`P2P` / `中转`），沿用桌面端胶囊样式；
   P2P 不可用时完全静默，不打扰用户。
6. 代码位置：`web/src/transport.ts`（新）、`web/src/webrtc.ts`（新）、
   `web/src/api.ts`（Viewer 改造）、`ControlPage.tsx`（播放分支与统计）。

## 9. 服务器改造

1. 信令转发（§5.1）与 ICE 凭据接口（§5.2）；
2. `[webrtc]` 配置段：`enabled`、`stun`（可多个）、`turn`、`turn_secret`、
   `turn_ttl`；不配置时服务器行为与现在完全一致；
3. 设备信息增加 `webrtc` 能力位（内存即可，不落库，避免迁移）；
4. 指标与限流：信令按 `(viewer, device)` 限 1 次/2 秒、失败计数；
   ICE 接口按账号限流；
5. 安全：所有字段长度/格式校验、只转发登记过的配对、`viewer` 断开即清登记。

## 10. 部署（前置条件）

1. **HTTPS**：`docs/server.md` 的 Caddy 分步；域名 A 记录指向服务器，
   80/443 放行。网页端因此自动获得 H.264 / 剪贴板 / PWA。
2. **coturn**（同一台或另一台都行）：
   - 放行 UDP/TCP 3478，以及 `min-port`–`max-port`（建议 49160–49200）UDP；
   - `use-auth-secret` + `static-auth-secret`（只写进服务器配置）；
   - 可选 443/UDP 的 `alt-tls-listening-port`，对付只放行 443 的网络；
   - 云安全组别忘了开这些端口。
3. 带宽评估：打洞成功时媒体不经过服务器；TURN 兜底时和现在中转一样占
   服务器带宽，但走 UDP、延迟更稳。ECS 带宽小的话，TURN 只作为最后手段。

## 11. 安全

- 信令只在已认证的账号连接之间转发；被控端只认服务器发来的、与自己设备
  匹配的 offer（现有模型）。
- DataChannel 走 DTLS，媒体走 SRTP，链路本身加密；相比现在中转明文 TCP
  是净提升。
- 残余风险：账号服务器是信令的中枢，恶意服务器理论上可以替换 SDP 做中间人；
  这与现在「服务器能看到画面明文」同等级，不新增风险。需要更强保证时，
  可像客户端那样在 UI 显示 DTLS 指纹供用户核对（网页端第一版不做）。
- TURN 凭据短时效、按账号签发；`turn_secret` 不进日志。
- 授权流程不变：同账号按「已授权设备免确认」设置放行，否则弹窗。

## 12. 里程碑

| 阶段 | 内容 | 估算 |
|---|---|---|
| P0 | HTTPS + coturn 部署、ICE 接口、配置段 | 2–3 天 |
| P1 | 信令转发 + 网页端 Transport 抽象 + 被控端 WebRTC 骨架；DataChannel 上跑通 `open`/输入/文件 | 1–2 周 |
| P2 | 视频/音频 RTP 轨道、PLI、统计与 UI 胶囊；方案 A 的 ack 限速（作为回退） | 1–2 周 |
| P3 | 回退、ICE 重启、诊断日志、限流、指标、Chrome/Edge/Safari/iOS 回归 | 1 周 |

合计 3–5 周（单人）。P1 结束即可演示「网页端 P2P 控制」，但画面仍是
DataChannel 方案，观感提升有限；真正的体验提升在 P2。

## 13. 测试矩阵

- 浏览器：Chrome/Edge（Windows）、Android Chrome、Safari（macOS）、iOS Safari（PWA）。
- 网络：同局域网、单侧 NAT、双侧对称 NAT（必须落到 TURN）、仅 IPv6、CGNAT。
- 劣化：1/3/5 Mbps 限速、50–200 ms 延迟、1–5% 丢包（用现有 `D:\swtest\proxy.js`
  改造成 UDP 侧工具，或 Linux `tc netem`）。
- 场景：分辨率/帧率切换、关键帧恢复、文件传输、剪贴板、声音、断线重连、
  被控端重启、服务器重启、同账号多主控并发。
- 回归：桌面客户端 QUIC 打洞与中转不受影响；未升级的被控端直接走中转。

## 14. 风险

| 风险 | 影响 | 应对 |
|---|---|---|
| webrtc-rs 在 Windows 上的吞吐/稳定性 | 高码率下丢帧、CPU 高 | P1 先做基准测试；不达标就退回方案 A 或考虑 GStreamer |
| 对称 NAT 比例高 | 大量用户落到 TURN | 提前评估 TURN 带宽；必要时多地域部署 |
| iOS Safari 的 DataChannel/轨道兼容 | 手机端不可用 | 逐项回归；不支持时静默走中转 |
| 版本割裂 | 老被控端无法 P2P | `devices[].webrtc` 门控，不尝试、不报错 |
| 会话中途切换复杂 | 断线体验 | 第一版接受一次重连，不做无缝迁移 |
| 工期 | 数周 | 先交付 P0（HTTPS + H.264），P2P 作为后续增强 |

## 15. 需要拍板的决策

1. 接受 3–5 周工期，还是先做 1–2 周 MVP（DataChannel 版）？
2. TURN 放哪台机器、可接受的带宽成本？
3. 是否接受「WebRTC 只在 HTTPS 下可用、HTTP 部署仍走中继」这一前提？
4. 是否需要第一版就支持 iOS Safari（PWA）？

## 16. 备选方案（结论：不做）

- **WebTransport（HTTP/3）**：浏览器 API 只有 QUIC 传输，不含 ICE/打洞，
  穿 NAT 仍然要靠服务器，不满足 P2P。
- **HTTP 下用 MSE 播 H.264**：能省带宽，但不是 P2P，且 iPhone Safari 支持
  很差；可作为「没有域名、又必须省带宽」的独立小项目。
- **浏览器直连被控端 WebSocket（局域网）**：页面是 HTTP 时可行，但只覆盖
  同网段，且切到 HTTPS 后受混合内容限制，不能替代 WebRTC。
