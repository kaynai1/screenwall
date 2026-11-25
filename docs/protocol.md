# ScreenWall 协议规格 v1

> 配套文档：[design.md](design.md)
> 状态：草案，随实现调整

## 0. 约定

- 所有多字节整数为**小端序**
- 文本帧：UTF-8 JSON，`t` 字段标识消息类型；单帧上限 1 MiB（`max_frame_bytes`，文件块 base64 后最大约 350 KiB）
- 二进制帧：只承载视频数据；单帧上限 4 MiB
- 时间戳统一用发送方单调时钟，单位毫秒或微秒，仅用于测量延迟，不做跨机对齐
- 未知字段必须忽略；未知消息类型回 `error` 后继续（`hello` 阶段除外）

## 1. 连接

本文件描述**应用层协议**，它可以跑在两种承载上：

1. **局域网直连**：`wss://<agent-host>:47900/ws`，Agent 自签证书，客户端固定 SHA-256 指纹。
2. **账号中转**：主控端与被控端各自连接服务器，经 `/ws/view` + `/ws/device` 的会话帧承载，帧格式见 [server.md](server.md) 第 5 节。

两种承载里的消息完全一致；区别只在底层连接和初始鉴权。

| 项 | 值 |
|---|---|
| 直连 URL | `wss://<agent-host>:47900/ws` |
| 直连底层 | TCP + TLS 1.3（rustls），Agent 自签证书，客户端固定指纹 |
| 中转底层 | 两端各一条到服务器的 WSS；应用消息装进 8 字节会话帧 |
| 鉴权 | 直连用配对码派生的 `token`；中转用账号令牌，`hello.token` 仍校验以防串会话 |
| 会话 | 每个被控端可同时接受多个会话（屏幕墙 + 单独控制） |
| 握手 | 会话建立后 5 秒内必须发送 `hello`，否则断开 |

直连的完整设计（配对码、证书指纹、会话流程、与中转的对比）见 [direct.md](direct.md)。
注意：直连的**每次**会话都必须经被控端本机授权，不受「允许被控」总开关影响 ——
那个开关只管账号中转会话。
| 心跳 | 每 5 秒一个 WebSocket ping；15 秒无 pong 断开；中转时服务器另有 45 秒空闲超时 |
| 状态推送 | Agent 每 5 秒推一次 `status`（每个会话独立） |

## 2. 握手

### 2.1 `hello`（client → agent）

```json
{
  "t": "hello",
  "proto": 1,
  "token": "<base64, 32 字节>",
  "client": { "name": "DESKTOP-CTRL", "version": "0.1.0" },
  "caps": ["audio.opus", "audio.volume", "file.resume", "wall"]
}
```

`caps` 可选，缺省视为 v1 基线（见 [2.4 能力协商](#24-能力协商caps)）。

### 2.2 `welcome`（agent → client）

```json
{
  "t": "welcome",
  "proto": 1,
  "agent": {
    "name": "DESKTOP-A",
    "version": "0.1.0",
    "os": "Windows 11 26200"
  },
  "limits": {
    "maxStreams": 4,
    "maxChunk": 1048576,
    "allowControl": true
  },
  "caps": ["audio.opus", "audio.pcm16", "audio.source.mic", "audio.volume", "audio.aec", "audio.stats", "display.multi", "display.all", "file.resume", "file.directory", "file.sha256", "clipboard.text", "power", "wol", "wall"]
}
```

### 2.3 握手失败

Agent 回复 `error` 后以对应关闭码断开：

```json
{ "t": "error", "code": 4001, "msg": "auth failed" }
```

| 关闭码 | 含义 |
|---|---|
| 4001 | token 错误 |
| 4002 | 协议版本不支持 |
| 4003 | 超过连接或订阅上限 |
| 4004 | 对端关闭了「允许被控」 |
| 4005 | 握手超时 |
| 4006 | 鉴权失败次数过多，临时封禁 |

### 2.4 能力协商（`caps`）

`hello` 与 `welcome` 各自声明一份能力列表，用于**新旧版本互通**：老版本不认识
新能力就忽略，新版本也不会因为对端没有某项能力而用错消息。

规则：

1. 列表是字符串数组，标识用「点分小写」，**发布后不要改名**（改名等于静默关闭该能力）。
2. 未知标识一律忽略，解析不得失败。
3. 字段缺失 = 对端是 v1 基线（`Capabilities::legacy()`），不是「什么都不支持」。
   因此**不要把新能力加进基线**。
4. 判断某个可选功能能不能用，只看双方交集（`Capabilities::supports`），
   不要用本机常量直接判定。**现状（2026-11-24 核对）**：Agent 三个入口（中转 / 独立服务 / 直连）
   都按这条算交集并把 `welcome.caps` 回成交集，但 `supports()` 在生产代码里零调用；
   前端直接看 `control-caps` / `get_control_info` 给的字符串数组（目前 gate 了 `audio.volume`
   与 `input.sas`），其余可选功能仍是「收到 `UNKNOWN_MESSAGE` 再降级」。
5. 新增能力**不需要**升 `proto`；`proto` 只在报文结构不兼容时升。

当前标识（`crates/proto/src/caps.rs`）：

| 标识 | 含义 |
|---|---|
| `audio.opus` | 音频 Opus 编码 |
| `audio.pcm16` | 音频 PCM16 编码 |
| `audio.source.mic` | 声音来源可切换到麦克风 |
| `audio.volume` | `audio.volume` / `audio.volume.get` 音量读写 |
| `audio.aec` | 麦克风回声消除 |
| `audio.stats` | `audio.stats` 缓冲反馈（码率自适应） |
| `display.multi` | 多显示器枚举与切换 |
| `display.all` | `display = DISPLAY_ALL` 虚拟桌面拼接 |
| `file.resume` | 上传/下载断点续传 |
| `file.directory` | 整目录递归下载 |
| `file.sha256` | `file.done.sha256` 端到端校验 |
| `clipboard.text` | 文本剪贴板双向同步 |
| `input.sas` | Ctrl+Alt+Del 请求（`sas` / `sas.ok`） |
| `input.uipi` | UIPI 输入拦截状态上报（`input.status`） |
| `power` | 远程重启/关机 |
| `wol` | 远程开机 |
| `wall` | 屏幕墙只读会话 |

基线（`BASELINE`，对端未声明 `caps` 时假定具备）**不包含** `audio.aec`、`input.sas`、
`input.uipi` 三项；它们是后续新增能力，只在本端 `CURRENT` 里出现。

## 3. 状态与连通性

### 3.1 `status`（agent → client，每 5 秒）

```json
{
  "t": "status",
  "screen": { "w": 1920, "h": 1080, "scale": 1.0 },
  "subs": [ { "id": 7, "stream": "wall", "fps": 2 } ],
  "stats": { "cpu": 3.2, "kbps": 420, "mem": 42.5 }
}
```

`stats` 的字段都是真实采样，不是占位值：

- `cpu`：**整机** CPU 占用百分比（`GetSystemTimes` 差分，0–100）。首次推送
  还没有基准，报 `0`；之后的每次推送是「上一次推送以来」的平均占用。
- `mem`：**整机**内存占用百分比（`GlobalMemoryStatusEx` 的 `dwMemoryLoad`）。
  旧被控端不带这个字段，主控端按 `0` 处理。
- `kbps`：**本会话**的出流码率（视频帧 + 音频包 + 文件块字节数，按推送间隔差分）。
  局域网直连走的是同一套 `session_task`，所以同样有真实值，不是恒为 `0`。

`subs` 是当前**视频**订阅的实时快照（音频订阅不在里面，它的格式由 `audio.sub.ok`
一次性给出）：每推一次 `status`，被控端把当前 `video.sub` 的 `id` / `stream` / 实际
`fps` 放进来；没有订阅时是空数组。切换显示器或改画质后，下一次 `status` 就会反映
新的 `fps`。

### 3.2 `ping` / `pong`

```json
{ "t": "ping", "ts": 1732450000123 }
{ "t": "pong", "ts": 1732450000123 }
```

`ts` 原样回传，客户端用 RTT / 2 估算单程延迟并显示在墙上。

## 4. 显示器

`welcome` 之后 Agent 主动发送；显示器变化时重发：

```json
{
  "t": "display.list",
  "displays": [
    { "index": 0, "w": 1920, "h": 1080, "primary": true, "x": 0, "y": 0 },
    { "index": 1, "w": 2560, "h": 1440, "primary": false, "x": 1920, "y": 0 }
  ]
}
```

`index` 从 0 开始、主屏排第一；`x` / `y` 是这块屏在虚拟桌面里的左上角（可为负），
用于按显示器切换画面和映射输入坐标。每次会话开始时重新枚举，热插拔后客户端能立即看到。

## 5. 媒体流（视频与音频）

### 5.1 订阅

```json
{
  "t": "video.sub",
  "stream": "wall",
  "display": 0,
  "fps": 2,
  "maxWidth": 480,
  "quality": 70,
  "codec": "mjpeg",
  "cursor": false
}
```

```json
{
  "t": "video.sub",
  "stream": "focus",
  "display": 0,
  "fps": 30,
  "maxWidth": 1920,
  "maxBitrate": 8000,
  "codec": "h264",
  "cursor": true
}
```

| 字段 | 说明 |
|---|---|
| `stream` | `wall` / `focus`，每个连接各一路 |
| `display` | 显示器索引（0 起）；`4294967295` = 整块虚拟桌面拼接，默认 0 |
| `fps` | wall 1–5，focus 5–60 |
| `maxWidth` | 输出最大宽度，高度按比例 |
| `quality` | 仅 mjpeg，1–100 |
| `maxBitrate` | 目标码率上限，单位 kbps。**现状（2026-11-25）**：已实现。Agent 按 `kbps * 1000 / 8 / fps` 折算单帧字节预算，编码时选第一个不超预算的质量档（MJPEG 没有 CBR，这是能做到的最接近的形式）；三档都超预算时保留最低质量帧而不是丢帧。低于 50 kbps 或高于 200 Mbps 的值按「不限制」处理。屏幕墙缩略流忽略此字段。客户端从 `client.json` 的 `maxBitrate` 读，默认不限制。|
| `codec` | `mjpeg` / `h264`。**现状（2026-11-24 核对）**：Agent 只实现了 `mjpeg`，订阅 `h264` 会直接回 `4104`「当前只支持 MJPEG 编码」（`crates/agent/src/cloud.rs`）；主控端也只发 `mjpeg` |
| `cursor` | 协议字段：是否把系统光标画进画面。Agent 按 `sub.cursor && !view_only` 决定（`crates/agent/src/cloud.rs`）：DXGI 路径用 `GetFramePointerShape` 取形状、`PointerPosition` 取位置，再由 `crates/agent/src/cursor.rs` 合成；GDI 回退路径用 `GetCursorInfo` + `DrawIconEx`。墙上流客户端固定发 `false`，聚焦流固定发 `true`（见 [design.md](design.md) 5.2 的现状说明） |

屏幕墙缩略图会话（服务器转发的 `open` 带 `"mode": "wall"`）免逐次授权，
Agent 会把 `fps` 夹到 ≤2、`maxWidth` 夹到 ≤480，并忽略输入、文件、剪贴板等控制类消息，
只接受 `video.sub`。

### 5.2 确认、取消、关键帧

```json
{ "t": "video.sub.ok", "id": 7, "stream": "wall",
  "actual": { "codec": "mjpeg", "w": 480, "h": 270, "fps": 2 } }
{ "t": "video.unsub", "id": 7 }
{ "t": "video.keyframe", "id": 8 }
```

`id` 由 Agent 分配（u16），用作二进制帧的 `streamId`。

### 5.3 二进制帧头（24 字节）

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 1 | `version` | `0x01` |
| 1 | 1 | `type` | 1=video，2=audio，3=fileChunk（文件块当前走 JSON `file.chunk`） |
| 2 | 2 | `streamId` | 订阅返回的 `id` |
| 4 | 4 | `seq` | 帧序号 |
| 8 | 8 | `tsUs` | 采集时刻（微秒） |
| 16 | 4 | `payloadLen` | 负载字节数 |
| 20 | 1 | `codec` | 1=JPEG，2=H.264 Annex-B，3=PCM16，4=Opus，0=原始字节 |
| 21 | 1 | `flags` | bit0=关键帧 |
| 22 | 2 | `reserved` | 0 |
| 24 | N | `payload` | |

说明：

- `seq` 从 0 递增，客户端可据此发现丢帧；不要求重传
- `flags` 的关键帧标记用于 H.264 的 IDR，解码器可据此重置
- 客户端渲染队列满时丢最旧的帧，不阻塞接收

### 5.4 系统声音与麦克风

主控端显式请求才开启一路声音。默认采集「正在播放到默认扬声器」的内容（WASAPI 回环），
也可以改采被控端默认麦克风：

```json
{ "t": "audio.sub", "codec": "opus", "source": "loopback" }
{ "t": "audio.sub.ok", "id": 3, "actual": { "codec": "opus", "sampleRate": 48000, "channels": 2, "bitrate": 64000, "source": "loopback" } }
{ "t": "audio.stats", "id": 3, "bufferedMs": 120 }
{ "t": "audio.unsub", "id": 3 }
```

- `audio.sub.codec` 可选：`opus`（默认）/ `pcm16`。不带该字段的旧客户端按 `opus` 处理；
  不支持 WebCodecs 的 WebView 由主控端显式指定 `pcm16` 回退
- `audio.sub.source` 可选：`loopback`（默认，系统声音）/ `microphone`。不带该字段按
  `loopback` 处理，旧被控端会忽略它并继续采系统声音
- 重复发 `audio.sub` 等于换一路新订阅（换编码或换来源都走这里），Agent 直接替换旧采集，
  不需要先 `audio.unsub`
- Agent 用默认设备的混音格式打开采集，重采样/下混成 48 kHz 立体声 Opus，按 20 ms 一包
  放进 `type=2`、`codec=4` 的二进制帧，`streamId` = `audio.sub.ok.id`，`tsUs` 为 Unix 纪元微秒
- Opus 初始码率 64 kbps（单声道 48 kbps），约为原始 PCM 的 1/24。
  编码器初始化失败时 Agent 自动回退 PCM16，并在 `audio.sub.ok.actual.codec` 中如实告知
- PCM 回退路径同 v1：交错小端 16 位、48 kHz 立体声、约 1.5 Mbps，`type=2`、`codec=3`，
  `actual.bitrate` 为 0
- `audio.sub.ok` 发出前 Agent 不会推帧；主控端拿到 `sampleRate` / `channels` 后再初始化播放器
- 主控端每 2 秒发一次 `audio.stats` 上报播放缓冲深度（毫秒）。Agent 最多每 3 秒按缓冲调整一次
  Opus 码率：`bufferedMs > 250` 时降到 3/4（下限 24 kbps），`< 80` 时升到 5/4（上限 96 kbps）；
  PCM 路径忽略该消息
- 被控端没有默认输出设备 / 麦克风、或格式不受支持时返回 `4110`；这是局部失败，画面不受影响
- 缩略图（wall）会话忽略 `audio.sub` / `audio.stats`
- 麦克风不做回声消除（AEC）：被控端扬声器外放时会有回授，建议对端戴耳机

### 5.5 被控端音量

```json
{ "t": "audio.volume.get" }
{ "t": "audio.volume", "level": 0.42, "muted": false }
{ "t": "audio.volume.ok", "level": 0.42, "muted": false }
```

- 调整默认输出设备的主音量与静音（`IAudioEndpointVolume`）；`level` 为 0.0–1.0
- `audio.volume` 的 `level` / `muted` 都可省略：只带 `muted` 就是切换静音，只带 `level` 就是改音量
- `audio.volume.get` 只读当前状态，回同一个 `audio.volume.ok`
- 没有输出设备时返回 `4110`；缩略图（wall）会话忽略这两条消息
- 主控端侧栏的滑块拖动时合并请求，停下约 180 ms 才下发，避免高频写系统音量
- 主控端用 Web Audio 按 `AudioBuffer` 顺序排程播放：Opus 先经 WebCodecs `AudioDecoder` 解成
  `AudioData`，再与 PCM 走同一条时间线。队列落后就跳过、积压超过 300 ms 就重置时间线

## 6. 输入

### 6.1 批量消息

```json
{
  "t": "input",
  "events": [
    { "k": "move", "abs": [0.5123, 0.3401] },
    { "k": "button", "button": "left", "down": true, "abs": [0.5123, 0.3401] }
  ]
}
```

客户端按 8 ms 或 64 条事件为一批发送。单条连接内严格按顺序处理。

### 6.2 事件类型

| `k` | 字段 | 说明 |
|---|---|---|
| `move` | `abs`: [x, y] 或 `rel`: [dx, dy] | 绝对坐标按当前订阅的显示器归一化；整块桌面模式则相对整个虚拟桌面。相对位移用于 3D 视角 |
| `button` | `button`: left/right/middle/x1/x2；`down`；可选 `abs` | 绝对点击时同时带坐标 |
| `wheel` | `delta`: [水平, 垂直]，一格 120 | |
| `key` | `scan`: Windows 扫描码；`down`；`ext`: 是否扩展键 | 不区分字符，布局由被控端决定 |
| `text` | `text`: 字符串 | 以 `KEYEVENTF_UNICODE` 直接注入，支持中文 |
| `releaseAll` | 无 | 立即松开所有已按下的键鼠 |

### 6.3 语义约定

- **移动可合并**：客户端 60 Hz 取样，覆盖未发出的旧移动事件
- **按键/点击/滚轮必达且有序**：不合并、不重传
- **群控**：主控端向每个目标各自发送同一批事件，坐标保持归一化，由各 Agent 映射到自己的分辨率
- **显示器切换**：换 `display` 只重新发一次 `video.sub`（Agent 换流，会话不变）；之后输入坐标随之映射到新显示器
- **状态恢复**：任何连接中断，Agent 在会话清理时执行 `release_all`；客户端重新开始时先发一次 `releaseAll`

### 6.4 群控广播（当前实现）

- 群控不是共享会话：主控端为每台选中设备各开一条普通 `control` 会话，被控端照常逐台授权。
- 每张卡片从自己的会话订 `stream: "wall"` 的低帧率视频做预览（默认 2 FPS / 宽 640 / Q60）；切画质重发 `video.sub`，Agent 直接换流，会话号保持不变。
- 一次操作把同一批 `events` 原样复制到每条已就绪的会话；`abs` 始终是 0..1 相对坐标，所以不同分辨率的设备落在各自画面的同一相对位置。
- 定向操作（关闭「广播输入」）时只发往目标设备那一条会话；关闭群控会对每条会话逐个 `close`。

### 6.5 Ctrl+Alt+Del（安全注意序列）

```json
{ "t": "sas", "asUser": false }
{ "t": "sas.ok", "method": "sas" }
{ "t": "sas.ok", "method": "keys", "detail": "系统策略 SoftwareSASGeneration 未允许当前身份发送 SAS…" }
```

- 只在双方都声明 `input.sas` 时由主控端发送；老被控端会回 `4109`（部分旧版本回 `4100`），
  主控端按能力交集置灰按钮，不把这条错误当成整页失败
- 被控端优先 `LoadLibraryW("sas.dll")` + `GetProcAddress("SendSAS")` 调用 `SendSAS(asUser)`；
  能否真正生成 SAS 取决于进程身份（SYSTEM / 服务）与
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`
  （0 = 不允许，1 = 仅服务，2 = 仅应用，3 = 两者；缺失按 0 处理）
- 不满足条件时降级为 `SendInput` 注入 `VK_CONTROL` + `VK_MENU` + `VK_DELETE`，
  回 `method: "keys"` 和原因；这种降级只能作用于普通应用，无法进入安全桌面
- `asUser` 省略按 `false`；直连会话拒绝该消息（`4108`），与其它键鼠消息一致
- `SendSAS` 没有返回值，被控端只能确认「已调用」，无法确认系统一定接受

### 6.6 UIPI 拦截上报（`input.status`）

```json
{ "t": "input.status", "uipi": true, "detail": "前台窗口的完整性级别高于被控端…" }
```

- 被控端包装 `SendInput`：返回数量不足且 `GetLastError == ERROR_ACCESS_DENIED (5)` 时判定为 UIPI 拦截
- 只在双方都声明 `input.uipi` 时发送；状态翻转时发一次，持续拦截每 10 秒重报一次，恢复时发 `uipi: false`
- 主控端收到后提示「被控端有提权窗口，需要以管理员身份运行被控端」
- UIPI 是系统安全边界：普通权限 Agent 控制不了提权窗口，只能通过以管理员运行或安装为服务解决

## 7. 文件传输

### 7.1 ID 分配

`tid` 为 u16：发送方在 `1..=32767` 分配，接收方回传时沿用；不复用未完成的 `tid`。

### 7.2 清单

```json
{
  "t": "file.offer",
  "tid": 12,
  "items": [
    { "path": "logs/a.txt", "size": 1234, "sha256": "…" },
    { "path": "logs/sub/b.txt", "size": 5678, "sha256": "…" }
  ],
  "totalSize": 6912
}
```

### 7.3 应答

```json
{
  "t": "file.accept",
  "tid": 12,
  "root": "C:\\Users\\me\\Downloads\\ScreenWall",
  "resume": [{ "item": 0, "offset": 1048576 }]
}
{ "t": "file.deny", "tid": 12, "msg": "not enough space" }
```

接收方在 `accept` 前完成路径安全校验（拒绝绝对路径与 `..`）。

`resume` 是可选字段（上传方向）：接收方按 `sha256` 在 `Downloads\ScreenWall\.screenwall-parts\<sha256>.part` 里找到上次中断留下的前缀时，把每个文件的已收字节数带回来；发送端必须从该偏移继续发，而不是从 0 开始。没有断点时省略或为空数组。

`offset` 是可选字段（下载方向，见 7.6）：主控端把本地已有分片的字节数带回来，被控端校验 `offset <= size` 后从该偏移 seek 发送，超过文件大小或读不出前缀时从 0 重发。`0` 时省略；旧版接收方忽略该字段（等于 0），旧版发送方不发送时接收方按 0 处理，双方都能自动回退到整文件重传。

### 7.4 数据块

```json
{ "t": "file.chunk", "tid": 12, "item": 0, "offset": 262144, "data": "<base64>" }
```

- `item` 是 `file.offer.items` 的下标；`offset` 必须是该文件已接收的字节数
- 单块原始字节上限 256 KiB（`FILE_CHUNK_BYTES`），JSON 里用 base64 承载
- 发送端最多保留 8 MiB 未确认字节（`FILE_WINDOW_BYTES`），超过就等待 `file.progress`；30 秒没有确认视为对端失联
- 接收方每收满 2 MiB 回一条 `file.progress`，它同时是流控确认

### 7.5 完成与进度

```json
{ "t": "file.progress", "tid": 12, "done": 1048576, "total": 6912 }
{ "t": "file.done", "tid": 12, "path": "logs/a.txt", "ok": true, "sha256": "…" }
{ "t": "file.done", "tid": 12, "path": "logs/a.txt", "ok": false, "msg": "SHA256 校验失败（实际 …）" }
{ "t": "file.cancel", "tid": 12, "msg": "user canceled" }
```

`file.done.sha256` 是发送方算出的整文件哈希：上传时由被控端在落盘后计算并与 `file.offer` 里的值比对，不一致就 `ok=false` 并删掉坏分片；下载时由被控端边发边算，主控端用自己收到的内容再算一遍，不一致就删掉临时文件并报错。

每个文件单独 `file.done`；全部文件结束后发送 `file.complete`：

```json
{ "t": "file.complete", "tid": 12, "files": 2, "bytes": 6912 }
```

### 7.6 浏览与下载（被控端 → 主控端）

```json
{ "t": "file.list", "tid": 21, "path": "C:\\Users\\me" }
{ "t": "file.list.ok", "tid": 21, "path": "C:\\Users\\me", "parent": "C:\\Users", "entries": [
  { "name": "Documents", "path": "C:\\Users\\me\\Documents", "dir": true, "size": 0 },
  { "name": "a.txt", "path": "C:\\Users\\me\\a.txt", "dir": false, "size": 1234, "modified": 1763900000 }
] }
{ "t": "file.get", "tid": 22, "path": "C:\\Users\\me\\a.txt" }
```

`file.get` 之后被控端先回 `file.offer`（单文件），**收到主控端的 `file.accept` 才开始推数据块**，避免数据块早于接收方准备好。

下载续传：

- 主控端把已收前缀留在 `Downloads\ScreenWall\.screenwall-downloads\<key>.part`，`key` 是「设备 ID + 规范化远程路径 + 文件大小」的 SHA256（十六进制），不用原始文件名，避免路径注入；大小变化视为另一个文件，不会错续
- 再次下载同一文件时主控端在 `file.accept` 里带 `offset`，被控端从该偏移 seek 发送；首个数据块的 `offset` 即续传起点
- 被控端在发送前重算前缀 SHA256，`file.done.sha256` 始终是整文件哈希；主控端续传时也从分片重算前缀哈希，收尾时用整文件哈希比对
- 取消、断线、关闭客户端都保留分片；超过 7 天未续传的分片在下次下载时清理
- 旧版被控端忽略 `offset`、从 0 重发时，主控端收到偏移 0 的数据块会清掉前缀并整文件重收，不会卡死
- 校验失败删除分片；成功时若同名文件已存在则加 `(1)`、`(2)` 后缀，不覆盖

### 7.7 约定

- 一条会话同时只有 1 个上传批次（被控端只保留一个接收会话）；下载由主控端自己排队，最多 3 个并发，其余在本地队列等待
- 整目录下载是主控端的本地行为：它逐层发 `file.list` 展开目录，再为每个文件各发一次 `file.get`（独立 `tid`），协议层没有「目录」消息
- 上传落盘目录：被控端 `%USERPROFILE%\Downloads\ScreenWall`，同名文件加 `(1)`、`(2)` 后缀
- 被控端只接受相对路径；绝对路径、`..`、盘符都会被拒绝
- 上传断点续传：被控端把已收内容留在 `.screenwall-parts\<sha256>.part`，断线/取消不删除；下次 `file.offer` 带同一个 `sha256` 时回 `resume` 偏移。收尾时先校验 SHA256 再改名到目标文件，失败则删掉分片。超过 7 天的分片会在新批次开始时清理
- 下载断点续传：主控端把已收前缀留在 `Downloads\ScreenWall\.screenwall-downloads\<key>.part`（`key` = 设备 + 远程路径 + 大小的 SHA256），再次下载时在 `file.accept.offset` 里带回起点；被控端校验后 seek 发送。取消/断线/退出都保留分片，超过 7 天未续传的在下次下载时清理；旧版任一端不支持时自动回退到从 0 重传。完成后按 SHA256 校验，失败删分片，同名文件加序号
- 同一批清单里出现重复哈希（同内容多份）时，只有第一份参与续传
- 默认上限：**目前没有实现**。Agent 的 `safe_join` 只做路径安全校验，不限制单文件大小、单次总量或条目数；客户端整目录下载只限制了递归规模（20000 个文件 / 64 层），上传没有对应的总量上限。早期文档写的「单文件 20 GiB、单次总量 200 GiB、条目数 10000」在代码里找不到。

## 8. 剪贴板同步

文本剪贴板双向同步，超过 64K 字符不同步。

```json
{ "t": "clipboard", "text": "复制的内容" }
```

双方各自轮询本机剪贴板序列号，变化才发送；自己刚写入的内容不会再回传（避免两端互相刷）。被控端在会话期间持续监听；主控端仅在控制会话存在且「剪贴板同步」开启时生效。

## 9. 电源操作与远程开机

### 9.1 重启 / 关机（已实现）

```json
{ "t": "power", "action": "restart" }
{ "t": "power", "action": "shutdown" }
{ "t": "power.ok", "action": "restart" }
{ "t": "error", "code": 4111, "msg": "电源操作失败: ..." }
```

- 只在已授权的控制会话里可用；缩略图（wall）会话忽略
- Agent 用系统自带 `shutdown.exe /r|/s /t 3` 执行，延迟 3 秒给 `power.ok` 留出回传时间
- 执行失败返回 `4111`，会话本身保持；成功后被控端会随关机/重启离线
- 主控端必须先二次确认，不能一键触发

### 9.2 远程开机（Wake-on-LAN）

```json
{ "t": "wol.request", "mac": "AA:BB:CC:DD:EE:FF", "broadcast": "192.168.1.255" }
{ "t": "wol.ok", "mac": "AA:BB:CC:DD:EE:FF" }
{ "t": "error", "code": 4100, "msg": "MAC 地址无效: ..." }
```

约束：

- MAC 必须是单播地址（首字节最低位为 0），不能全 0 或全 `FF`；接受 `AA:BB:CC:DD:EE:FF`、
  `AA-BB-CC-DD-EE-FF` 与 12 位无分隔写法
- `broadcast` 只接受 `255.255.255.255` 或私有网段地址（10/8、172.16/12、192.168/16），
  端口统一用 9
- Agent 侧每 10 秒最多发送一次，超出返回 `4100`
- 只发送标准 magic packet：6 × `0xFF` + 16 × MAC，UDP 目的端口 9
- 只在已授权的控制会话里可用；缩略图（wall）会话忽略

实现位置：Agent 的 `crates/agent/src/wol.rs`，由 `cloud.rs` 处理 `wol.request`。

**实际使用方式**：目标离线时根本建立不了会话，所以 Agent 的 `wol.request` 分支只适用于
「还有另一台在线设备」的场景。日常唤醒走的是主控端本机的 `wake_on_lan` 命令——
由主控电脑直接向目标网段广播，前提是两者在同一局域网（或路由器配置了定向广播）。

## 10. 错误

### 10.1 通用错误消息

```json
{ "t": "error", "code": 4101, "msg": "stream limit reached", "tid": 12 }
```

`tid` / `id` 可选，用于关联具体任务。

### 10.2 应用错误码

| 码 | 含义 |
|---|---|
| 4100 | 请求参数非法 |
| 4101 | 超过订阅或并发上限 |
| 4102 | 目标地址或 MAC 不允许 |
| 4103 | 显示器索引不存在 |
| 4104 | 编码器不可用 |
| 4105 | 文件路径非法 |
| 4106 | 磁盘空间不足 |
| 4107 | 校验失败 |
| 4108 | 对端关闭了控制功能 |
| 4109 | 未识别的消息类型 |
| 4110 | 系统声音不可用（无输出设备或格式不支持） |
| 4111 | 电源操作失败（无法启动 shutdown.exe） |

## 11. 版本策略

- `proto` 为整数，当前为 `1`
- Agent 支持的最小版本写在 `welcome.proto`；客户端高于 Agent 支持范围时断开
- 新增字段向后兼容：发送方可以多带，接收方必须忽略
- **新增可选功能走 `caps`**（[2.4](#24-能力协商caps)），不升 `proto`；
  只有报文结构本身不兼容（例如枚举值语义变化、必填字段新增）才需要升 `proto`
- 新增消息类型：老对端会回 `4109 UNKNOWN_MESSAGE`，调用方必须把它当作
  「该功能不可用」而不是致命错误（主控端据此降级，不把整页画面判成出错）

## 12. 典型会话

```
client                                  agent
  │ ── wss 连接 ────────────────────────▶ │
  │ ── hello {token} ───────────────────▶ │
  │ ◀──────────────── welcome + limits ── │
  │ ◀──────────────── display.list ────── │
  │ ◀──────────────── status (每 5s) ───── │
  │ ── video.sub {wall} ────────────────▶ │
  │ ◀──────────────── video.sub.ok {id:7} │
  │ ◀════ 二进制帧 type=1 id=7 ═══════════ │
  │ ── video.sub {focus} ───────────────▶ │
  │ ◀──────────────── video.sub.ok {id:8} │
  │ ◀════ H.264 帧 ═══════════════════════ │
  │ ── audio.sub ───────────────────────▶ │
  │ ◀──────────────── audio.sub.ok {id:9} │
  │ ◀════ 二进制帧 type=2 codec=3 ════════ │
  │ ── input {move, button} ────────────▶ │
  │ ── file.offer {tid:12} ─────────────▶ │
  │ ◀──────────────── file.accept ──────── │
  │ ── file.chunk {item,offset,data} ───▶ │
  │ ◀──────────────── file.progress ────── │
  │ ◀──────────────── file.done ────────── │
  │ ── file.get {tid:22,path} ──────────▶ │
  │ ◀──────────────── file.offer (单文件) │
  │ ── file.accept {tid:22} ────────────▶ │
  │ ◀── file.chunk {item,offset,data} ─── │
  │ ── clipboard {text} ◀───────────────▶ │
  │ ── wol.request ─────────────────────▶ │
  │ ◀──────────────── wol.ok ───────────── │
```

控制类消息（`open` / `input` / `clipboard`）需要被控端在授权弹窗里同意后才生效，见 [server.md](server.md)。

## 13. 客户端连接质量与重连（客户端实现，不改协议）

这一节只描述主控端（`crates/client`）如何使用既有字段，**不新增任何协议消息**。

### 13.1 丢帧 / 丢包

- 视频：`FrameHeader.seq` 在同一 `streamId` 内单调递增，客户端按缺口计数丢帧。
  `seq` 是 `u32`，用 `wrapping_sub` 同时判断前向/后向距离：前向距离为 0 是重复帧、很小是正常前进（含回绕）、后向距离很小是迟到帧、两个方向都很大说明换了新流（重新订阅）只重定基准。
  重复/迟到帧既不计入收到帧数，也不计为丢失。
- 音频：同样按 `FrameHeader.seq` 计丢包；另外用相邻两包 `tsUs` 之差 > 40 ms 统计「音频空档」（Opus 单包 20 ms）。
- 重新订阅（`video.sub` / `video.unsub` 往返、重连）后 `seq` 从 0 重新开始，客户端以 `streamId` 变化或跳变过大作为重基准信号。

### 13.2 客户端事件（非协议）

以下 Tauri 事件只在主控端界面内使用，不经过服务器：

| 事件 | 负载 |
| --- | --- |
| `control-reconnecting` | `{ clientToken, deviceId, attempt, maxAttempts, delayMs }` |
| `control-reconnected` | `{ clientToken, deviceId, session, attempt, message: null }` |
| `control-reconnect-failed` | `{ clientToken, deviceId, session: 0, attempt, message }` |
| `control-stats` | 原有字段 + `quality`（`{ video, audio, net, since }`）+ `reconnecting`（`{ attempt, maxAttempts, delayMs }` 或 `null`） |

### 13.3 错误文案所需的字段

客户端把服务器 / 被控端的错误码拼成 `"{code}: {msg}"` 后交给界面映射（`ui/src/errors.ts`）。
`open.err` 与 `t = "error"` 已经带 `code`，无需新增字段。
