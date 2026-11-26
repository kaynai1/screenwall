# ScreenWall 账号与中转服务器规格 v1

> 配套文档：[design.md](design.md) · [protocol.md](protocol.md)
> 状态：草案，随实现调整

服务器负责三件事：

1. **账号**：注册、登录、令牌校验。
2. **设备目录**：同一账号下的设备列表与在线状态。
3. **会话中转**：把主控端和被控端之间的字节流对接起来。两端都主动连接服务器，因此不需要公网 IP 或端口映射。

服务器**不保存画面内容**，只转发；数据库保存账号哈希、令牌哈希和设备元数据。

## 0. 部署形态

```
客户端 A ──┐
           ├── wss://screenwall.example.com ── Caddy（TLS）── screenwall-server ── SQLite
客户端 B ──┘
```

- `screenwall-server` 默认监听 `127.0.0.1:8787`，由 Caddy / Nginx 反向代理并提供证书。
- 生产环境只暴露 443；服务器本机不需要开放 8787。
- 也可以用 `wss://` 直连，只需给服务器配置证书；自用推荐 Caddy，自动申请 Let's Encrypt。
- 开发阶段可以跑在 `http://127.0.0.1:8787`，客户端配置里允许 `ws://`。

## 1. HTTP API

所有请求与响应均为 JSON，`Content-Type: application/json`。需要登录的接口带：

```
Authorization: Bearer <token>
```

### 1.1 注册

`POST /api/v1/register`

```json
{ "username": "kay", "password": "至少 8 位" }
```

成功 `200`：

```json
{
  "token": "<64 位十六进制>",
  "user": { "id": 1, "username": "kay" }
}
```

失败：

| 状态码 | 错误 | 说明 |
|---|---|---|
| 400 | `invalid_input` | 用户名或密码不符合要求 |
| 403 | `registration_closed` | 服务器关闭了公开注册 |
| 409 | `user_exists` | 用户名已存在 |
| 429 | `rate_limited` | 注册过于频繁（默认每 IP 每小时 10 次，见 6.2 节） |

规则：

- 用户名 3–32 字符，只允许字母、数字、下划线、连字符。
- 密码 8–128 字符。
- 密码用 Argon2id 哈希，绝不存明文。
- 自用部署建议 `allow_registration = false`，用命令行创建账号（见第 6 节）。

### 1.2 登录

`POST /api/v1/login`

```json
{
  "username": "kay",
  "password": "……",
  "device": {
    "id": "客户端生成的稳定 UUID",
    "name": "客厅电脑",
    "os": "Windows 11 26200",
    "version": "0.1.0"
  }
}
```

成功：

```json
{
  "token": "……",
  "expiresAt": 1790000000000,
  "user": { "id": 1, "username": "kay" },
  "device": { "id": "……", "name": "客厅电脑", "connectCode": "572955600" }
}
```

- `device.connectCode` 是 9 位连接码，登录时按需分配，之后保持不变（换码走 1.8 节）。
- `device.id` 首次登录时由客户端生成并持久化，之后不变；服务器按 `(user_id, device_id)` 更新设备信息。
- 如果客户端没有 `device.id`，服务器会生成一个并在响应里返回。
- 登录有两道限流（见 6.2 节）：按 IP 的请求频率令牌桶，以及「用户名 + IP」连续失败
  5 次锁定 60 秒；超限返回 `429` 并带 `Retry-After`。
- 单账号设备数受 `[limits].max_devices_per_account` 约束，新设备超限返回
  `403 device_limit_reached`。

### 1.3 当前账号

`GET /api/v1/me`

```json
{ "user": { "id": 1, "username": "kay" } }
```

### 1.4 设备列表

`GET /api/v1/devices`

```json
{
  "devices": [
    {
      "id": "……",
      "name": "客厅电脑",
      "os": "Windows 11 26200",
      "version": "0.1.0",
      "online": true,
      "lastSeen": 1790000000000,
      "controllable": true
    }
  ]
}
```

- `online` 由服务器内存中的连接表决定；进程重启后由设备重连恢复。
- `controllable` 对应被控端的「允许被控」开关。

### 1.5 修改设备

`PATCH /api/v1/devices/{device_id}`

```json
{ "name": "书房电脑" }
```

只允许改自己账号下的设备。改名后向该账号的所有在线主控端推送新的设备列表。

### 1.6 删除设备

`DELETE /api/v1/devices/{device_id}`

删除后该设备令牌失效，必须重新登录。用于设备丢失或重装。

### 1.7 退出登录

`POST /api/v1/logout`

吊销当前令牌。客户端同时清除本地 `account_token`。

### 1.8 连接码（ID + 密码）

以下三个接口需要账号令牌，操作的是**当前登录设备**（令牌里的 `device_id`）：

`GET /api/v1/quick` —— 读连接码与开关状态：

```json
{ "code": "572955600", "enabled": true, "hasPassword": true }
```

`POST /api/v1/quick` —— 设置开关，可选同时改密码：

```json
{ "enabled": true, "password": "至少 6 位；省略表示不改" }
```

密码用 Argon2 哈希存 `devices.access_hash`。开启时如果服务器上还没有密码，返回
`400 invalid_input`。

`POST /api/v1/quick/rotate` —— 换一个新的 9 位连接码，旧码立即失效：

```json
{ "code": "903118245" }
```

`POST /api/v1/quick/connect`（**不需要登录**）—— 用连接码 + 密码换受限令牌：

```json
{ "code": "572955600", "password": "……", "name": "主控端显示名" }
```

成功：

```json
{
  "token": "……",
  "expiresAt": 1790000900000,
  "device": { "id": "……", "name": "客厅电脑" }
}
```

- 令牌 15 分钟有效，`kind = 'quick'`，只能连这一台设备的 `/ws/view`；调账号接口返回
  `403 quick_token`。
- 设备必须在线（`409 device_offline`）、已开启连接码（`403 quick_disabled`）、且没有
  关闭被控（`403 control_disabled`）。
- 密码错误、连接码不存在统一返回 `401 quick_auth_failed`，不区分原因。
- 限流：按 IP 每分钟 `[limits].quick_connects_per_minute`（默认 10）次；同一连接码 +
  IP 连续失败 5 次锁定 60 秒。
- 服务器发给被控端的 `open` 消息带 `"trusted": true`，被控端据此跳过「允许控制」弹窗。

#### 一次性连接链接

`POST /api/v1/connect-link`（需要账号令牌）—— 为当前设备签发一条一次性链接：

```json
{ "deviceId": "可选；省略时用该账号第一台设备" }
```

```json
{
  "url": "http://host/l/<token>",
  "token": "<token>",
  "expiresAt": 1790000600000,
  "expiresInSec": 600,
  "device": { "id": "……", "name": "客厅电脑" }
}
```

链接地址按请求的 `Host` 构造，反向代理下优先读 `x-forwarded-host` / `x-forwarded-proto`。
未开启连接码连接时返回 `403 quick_disabled`，被控关闭时返回 `403 control_disabled`。

`GET /l/{token}`（不需要登录）—— 链接落地页（HTML）。有效时渲染「在 ScreenWall 中打开」
按钮，跳转 `screenwall://connect?server=…&token=…`；失效时渲染「连接链接已失效」。
页面本身不消费令牌。

`POST /api/v1/connect-link/use`（**不需要登录**）—— 消费令牌换受限令牌：

```json
{ "token": "<token>" }
```

- 链接 **10 分钟有效、单次消费**；`connect_links` 表只存令牌的 SHA256。
- 消费前先只读校验并检查设备在线：设备离线返回 `409 device_offline`，链接仍可用。
- 不存在、过期、已使用统一返回 `404 link_invalid`。
- 换到的令牌与连接码相同（`kind = 'quick'`，15 分钟，只能连这一台设备）。
- 按 IP 复用 `quick_connect_rate` 限流。

### 1.9 版本检查（不需要登录）

`GET /api/v1/version`

登录前也要能检查更新，所以这个接口不要求 `Authorization`。数据来自 `server.toml` 的 `[update]` 段：

```json
{
  "version": "0.1.0",
  "url": "/downloads/ScreenWall_0.1.0_x64-setup.exe",
  "sha256": "<64 位小写十六进制>",
  "notes": "更新说明，可含换行",
  "publishedAt": 1790000000000
}
```

- `url` 是相对路径，客户端会拼到自己的服务器地址上；也可以直接填绝对 `http(s)` 地址（例如放到 CDN）。
- `sha256` 在服务器首次请求时计算一次并缓存，不会每次请求都重读整个安装包。
- `publishedAt` 取安装包文件的修改时间（Unix 毫秒）。
- 未配置 `[update]`、目录或文件不存在时返回 `404`：

```json
{ "msg": "未配置更新" }
```

> **没有自建服务器的客户端（局域网模式）不走这个接口**：它会退到 GitHub Releases 的
> latest 接口取版本与安装包，校验值用附件自带的 `sha256:` digest。自建服务器不可达时
> 也会退到 GitHub。两条更新路线的完整说明见 [更新与分发](update.md)。

### 1.10 下载安装包

`GET /downloads/{file}`

只允许下载 `[update].dir` 下 `[update].file` 指定的那一个文件：文件名必须精确匹配，不接受子路径、`..`、编码后的分隔符等，任何其它请求都返回 `404`。响应为：

```
Content-Type: application/octet-stream
Content-Disposition: attachment; filename="ScreenWall_0.1.0_x64-setup.exe"
```

内容用 `tokio::fs` 流式返回，不把整个安装包读进内存。

## 2. WebSocket 端点

两个端点都用 `Authorization: Bearer <token>` 鉴权；不接受把令牌放在 URL 里，避免进入访问日志。

| 端点 | 角色 | 说明 |
|---|---|---|
| `/ws/device` | 被控端 | 每个设备一条长连接，接收会话请求并转发数据 |
| `/ws/view` | 主控端 | 每个客户端一条长连接，可同时打开多个设备会话 |

连接建立后，服务器立即发送一条 JSON 文本帧：

```json
{ "t": "ready", "serverTime": 1790000000000 }
```

### 2.1 心跳

- 服务器每 15 秒发 WebSocket ping，45 秒内没有任何 pong 或数据则断开。
- 客户端也可以主动发 ping；服务器原样回 pong。
- 设备断线后服务器标记离线，并通知同账号的主控端。

## 3. 主控端协议（`/ws/view`）

服务器 → 主控端：

```json
{ "t": "devices", "devices": [ … ] }
```

连接建立时发一次；之后设备上线、下线、改名、删除时再推送完整列表。

主控端 → 服务器，打开一个设备会话：

```json
{ "t": "open", "request": 7, "deviceId": "……", "viewer": "办公室电脑", "mode": "wall" }
```

- `request` 是主控端自己生成的序号，用于把响应和请求对上。
- 同一台设备可以同时开多个会话（例如屏幕墙 + 单独控制）；同一个主控连接也可以同时持有指向不同设备的多条会话，群控就是按设备逐条 `open`，输入分别下发。
- `mode` 省略或为 `control` 时是正常控制会话；为 `wall` 时是屏幕墙缩略图会话：
  服务器把 `mode` 原样转给被控端，被控端免逐次授权、忽略输入/文件/剪贴板消息，
  只接受 `video.sub`（`fps ≤ 2`、`maxWidth ≤ 480`）。`wall` 会话不写审计。

服务器 → 主控端，成功：

```json
{ "t": "open.ok", "request": 7, "session": 123, "deviceId": "……" }
```

失败：

```json
{ "t": "open.err", "request": 7, "deviceId": "……", "code": 4200, "msg": "device offline" }
```

`session` 由服务器分配，全局唯一，后续二进制帧用它路由。

主控端 → 服务器，关闭会话：

```json
{ "t": "close", "session": 123 }
```

服务器 → 主控端，设备主动关闭或断线：

```json
{ "t": "closed", "session": 123, "code": 4201, "msg": "device disconnected" }
```

## 4. 被控端协议（`/ws/device`）

服务器 → 被控端，新会话：

```json
{
  "t": "open",
  "session": 123,
  "viewer": "办公室电脑",
  "viewerVersion": "0.1.0",
  "mode": "wall"
}
```

`mode` 为 `wall` 时表示屏幕墙缩略图：被控端不弹授权窗口，也忽略除 `video.sub`
以外的控制类消息。

被控端接受：

```json
{ "t": "open.ok", "session": 123 }
```

拒绝（例如「允许被控」已关闭）：

```json
{ "t": "open.err", "session": 123, "code": 4004, "msg": "control disabled" }
```

服务器 → 被控端，会话结束：

```json
{ "t": "close", "session": 123 }
```

被控端 → 服务器，主动结束：

```json
{ "t": "close", "session": 123 }
```

## 5. 会话帧

会话建立后，两端通过服务器转发**二进制帧**。每帧 8 字节头 + 负载：

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 1 | `version` | 固定 `1` |
| 1 | 1 | `kind` | `1` = JSON，`2` = 媒体 / 文件二进制 |
| 2 | 4 | `session` | 服务器分配的会话号，小端 u32 |
| 6 | 2 | `flags` | 保留，填 `0` |
| 8 | … | `payload` | 负载 |

- `kind = 1`：负载是 UTF-8 JSON，内容就是 [protocol.md](protocol.md) 里的消息（`hello`、`welcome`、`status`、`video.sub`、`input` 等）。
- `kind = 2`：负载是 [protocol.md](protocol.md) 第 5.3 节的 24 字节帧头 + 媒体数据。
- 单帧上限 1 MiB；超过则服务器关闭该会话并回 `4202`。
- 服务器不解析负载，只按 `session` 转发，因此新增应用层消息不需要改服务器。

服务器转发规则：

```
主控端 --(session=123, frame)--> 服务器 --(session=123, frame)--> 被控端
被控端 --(session=123, frame)--> 服务器 --(session=123, frame)--> 主控端
```

同一会话的两个方向共用 `session`，服务器按会话记录对应的两条连接。

## 6. 配置与运维

`server.toml`：

```toml
bind = "127.0.0.1:8787"
db_path = "screenwall.db"
allow_registration = false
token_ttl_days = 30
max_frame_bytes = 1048576

# 自托管更新（可选）；省略本段则 /api/v1/version 返回 404。
[update]
dir = "target/release/bundle/nsis"          # 相对路径按配置文件所在目录解析
version = "0.1.0"
file = "ScreenWall_0.1.0_x64-setup.exe"     # 必须位于 dir 下
notes = "修复若干问题，提升稳定性。"
```

发布新版本的流程：

1. 用 `scripts\tauri-build.bat` 打出新的 NSIS 安装包。
2. 把安装包放到 `[update].dir`（默认指向 `target/release/bundle/nsis`）。
3. 更新 `server.toml` 里的 `version` 与 `file`，重启服务器（或让客户端下次请求时重新计算哈希）。
4. 客户端在设置页「软件更新」里检查、下载并按 SHA256 校验后启动安装程序。

服务器重启后哈希会重新计算；同一个进程内文件变化不会自动失效，发布新包后需要重启服务器。

### 6.1 可观测性

`GET /api/v1/metrics`（需要登录）返回当前进程内的真实运行数据，全部在真实处理路径上累加，
进程重启后归零：

```json
{
  "uptimeSecs": 1234,
  "sessions": { "active": 3, "peak": 11, "total": 42 },
  "bytes": { "toDevices": 12345678, "toViewers": 8765432 },
  "errors": { "total": 4, "rate5m": 0.8, "byCode": { "4200": 2, "4100": 2 } },
  "devices": { "online": 2, "accounts": 1 },
  "handshakes": { "ok": 42, "failed": 3 },
  "backpressure": { "droppedFrames": 0 }
}
```

| 字段 | 含义 |
|---|---|
| `uptimeSecs` | 本进程已运行秒数 |
| `sessions.active` | 当前活跃中转会话数（内存会话表实时长度） |
| `sessions.peak` | 进程启动以来的并发会话峰值 |
| `sessions.total` | 累计建立过的会话数（含 `wall` 缩略图会话） |
| `bytes.toDevices` | 主控端 → 被控端方向的中转 payload 字节累计 |
| `bytes.toViewers` | 被控端 → 主控端方向的中转 payload 字节累计 |
| `errors.total` | 累计错误数（鉴权失败、登录/注册限流、非法帧、`open.err`、内部错误等） |
| `errors.rate5m` | 最近 5 分钟的平均每分钟错误数（30 个 10 秒桶滑动窗口） |
| `errors.byCode` | 按应用层错误码分组的累计次数（键为十进制字符串，如 `4200`） |
| `devices.online` | 当前建立 `/ws/device` 长连接的设备数（全服务器口径） |
| `devices.accounts` | 数据库中的账号总数 |
| `handshakes.ok` | `/ws/device` 与 `/ws/view` 握手成功次数 |
| `handshakes.failed` | 握手被拒次数（鉴权失败、参数非法、设备配额、建连限流） |
| `backpressure.droppedFrames` | 转发时目标通道已关闭/发送失败而丢弃的帧数 |

字节口径与审计一致：只算隧道帧的 payload，不含 8 字节帧头。

怎么用：

- **并发会话**：看 `sessions.active` 与 `sessions.peak`。`active` 长时间贴近 `peak`、
  且 `peak` 持续被刷新，说明服务器接近容量上限，需要扩容或收紧
  `session_opens_per_minute`。
- **带宽**：对 `bytes.toDevices` / `bytes.toViewers` 连续两次采样求差，除以间隔秒数
  即得上下行速率（字节/秒）。两个方向不对称是正常的（视频下行远大于控制上行）。
- **错误率**：`errors.rate5m` 上升时先看 `errors.byCode`：`4200`（设备离线）、
  `4204`（设备拒绝）、`4205`（会话建立被限流）各自指向不同问题。配合
  `handshakes.failed` 可区分「连不上」与「连上后开不了会话」。
- **丢帧**：`backpressure.droppedFrames` 增长说明主控端读得慢或已断开，
  服务器无法把帧投递出去。

旧接口 `GET /metrics` 仍保留，返回 `onlineDevices` / `activeSessions` /
`totalBytes` / `errorCount` 四个字段，供已有监控脚本使用；新接入请用
`/api/v1/metrics`。

### 6.2 设备上限与限流配置

`server.toml` 的 `[limits]` 段（整段省略时使用下列默认值，旧配置无需修改即可启动）：

```toml
[limits]
max_devices_per_account = 20     # 单账号最多绑定的设备数，0 = 不限制
login_per_minute = 30            # 每个 IP 每分钟允许的登录请求数
login_burst = 30                 # 登录限流的桶容量
register_per_hour = 10           # 每个 IP 每小时允许的注册数
register_burst = 10              # 注册限流的桶容量
view_connects_per_minute = 30    # 每个账号每分钟允许建立的 /ws/view 连接数
view_connect_burst = 30          # 建连限流的桶容量
session_opens_per_minute = 60    # 每个账号每分钟允许发起的会话数
session_open_burst = 60          # 会话建立限流的桶容量
quick_connects_per_minute = 10   # 每个 IP 每分钟允许的连接码尝试次数
quick_connect_burst = 10         # 连接码尝试限流的桶容量
```

- **设备上限**：登录绑定设备与 `/ws/device` 上线都受 `max_devices_per_account` 约束。
  已登记的设备重复登录/重连不占用新名额；新设备超限返回 `403 device_limit_reached`
  与中文提示，不会静默失败。删除设备后名额立即释放。
- **限流**：按 key 的令牌桶，每 `per` 时间补充 `rate` 个令牌，桶容量为 `burst`。
  超限时 HTTP 接口返回 `429` + `Retry-After`，`/ws/view` 建连在握手阶段直接返回 `429`，
  会话建立（`t = open`）则回 `open.err` 且 `code = 4205`。速率写 `0` 关闭该条限流。
- 触发限流时只打一条 `warn` 日志（含 key 与剩余秒数），不会每个被拒请求都刷屏。
- 限流状态在内存里，重启后清零；多实例部署时各实例独立计数。

### 6.3 命令行

```powershell
screenwall-server run --config server.toml
screenwall-server user add kay          # 交互式输入密码
screenwall-server user list
screenwall-server user reset kay
screenwall-server token revoke <token>  # 应急吊销
```

`--config` 是全局参数（`global = true`），写在子命令前后都行。完整部署步骤见 6.4。

### 6.4 部署到 Linux（含沙盒 / VPS / 容器面板）

服务端**没有一行 Windows 专属代码**，依赖也全部跨平台（`rusqlite` 用 `bundled`
自带 SQLite C 源码，TLS 走 `rustls` + `ring`，不需要系统 OpenSSL）。所以可以直接在
Linux 上编译运行。

> **验证状态（2026-11-24）**：下面这套流程**没有在真实 Linux 上跑过**。
> 本机只有 Windows，WSL 未安装发行版、没有 Docker，`rustup target add
> x86_64-unknown-linux-gnu` 因网络问题未完成。以下结论来自**代码与依赖层面的核查**：
> `crates/server` 与 `crates/proto` 中 `grep -rn "cfg(windows)|windows::"` 无命中；
> `cargo tree -p screenwall-server --edges normal --depth 1` 的内部依赖只有
> `screenwall-proto`；无 `openssl` / `native-tls` 依赖（TLS 是 `rustls` + `ring`，
> 纯 Rust）；无硬编码的 Windows 路径。**首次部署时请把编译结果反馈回来**，
> 尤其是 `ring` 与 `rusqlite`（`bundled`）在目标机器上的 C 编译是否顺利。

**关键：只能编译 `screenwall-server`，不能用 `--workspace`。**
工作区里的 `screenwall-agent` 依赖 `windows` crate（DXGI / WASAPI / GDI），
`screenwall-client` 依赖 `tauri`，这两个在 Linux 上编不过。`-p screenwall-server`
只会拉进 `screenwall-proto`（纯 serde），依赖闭包是干净的。

#### 0. 一键部署（Ubuntu / 无域名 / 直绑公网，最快路径）

如果只要一台 Ubuntu 云主机当中转、不需要域名和 Caddy，用仓库里的
`scripts/deploy-server.sh`。**一条命令、无需交互**。

一台全新的 Ubuntu / Debian 服务器，用 SSH 登录后执行这一条：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/deploy-server.sh \
  | bash
```

它会依次提示：

```text
请输入账号（登录用，如 kay）: kay
请输入密码（至少 8 位，输入不显示）:
请输入端口（直接回车用默认 8787）:
允许客户端自助注册账号? (y/N):
确认开始部署? (Y/n):
```

端口直接回车就用 8787。装完之后客户端登录页填 `http://<公网IP>:8787`、
账号和刚才输入的密码。

不想交互就把答案写在命令里：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/deploy-server.sh \
  | bash -s -- --yes --account kay --password '至少8位密码' --port 8787
```

`api.github.com` 不通时换 jsDelivr（内容取自 main，可能有缓存延迟）：

```bash
curl -fsSL https://cdn.jsdelivr.net/gh/kaynai1/screenwall@main/scripts/deploy-server.sh \
  | bash -s -- --yes --account kay --password '至少8位密码'
```

两个都不通，就用镜像克隆，再在本地执行：

```bash
git clone --depth 1 https://gitclone.com/github.com/kaynai1/screenwall.git /tmp/sw \
  && bash /tmp/sw/scripts/deploy-server.sh --yes --account kay --password '至少8位密码'
```

脚本会自己处理这些容易失败的环节：

- **自动修复 `/dev/null`**：它被换成普通文件/软链接时，`apt`、`gpgv`、`cargo`
  会全部报 `cannot create /dev/null: Permission denied`。脚本检测到就直接重建
  字符设备（`chattr -i` → `rm` → `mknod`），不再让你手工修。
- **依赖安装带重试**：`apt-get update` 有失败项不阻断，`install` 失败自动重试 3 次；
  缺什么装什么（curl/tar/git/编译工具/pkg-config/gpgv）。
- **源码用 tarball 优先**：不依赖 git，GitHub 不通时自动切镜像，最后才退回 `git clone`。
- **Rust 与 crates 走 rsproxy.cn 镜像**，国内编译快。
- 编译、写配置、建账号、装 systemd、放行 ufw，最后打印中转地址并做健康检查。

可选参数：`--port 8787`、`--allow-register`。**已经装过**之后再运行同一条命令，
就不会再走安装向导，而是显示管理菜单（更新 / 状态 / 日志 / 重启 / 备份 / 卸载）：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/deploy-server.sh \
  | bash
```

> 交互问答是从 `/dev/tty` 读的，所以要用 SSH 登录进服务器再执行（分配了终端）。
> 像 `ssh root@host 'curl ... | bash'` 这种不分配终端的写法会提示没有交互环境，
> 那种场景请改用 `--yes --account ... --password ...` 的无交互形式。

装完记得去云控制台 **安全组** 放行 TCP 端口 `8787`。需要域名 + HTTPS / WSS 的
完整分步（含 Caddy）见下文。

#### 1. 装工具链（Arch 为例）

```bash
pacman -Syu --needed base-devel git
pacman -S --needed rust          # 或者用 rustup：curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

`base-devel` 是必需的：`ring` 和 `rusqlite` 都要调 C 编译器（`cc`）。
不需要 OpenSSL 开发包，也不需要 Node.js / npm —— 那些只在打客户端安装包时用。

#### 2. 取代码并编译

```bash
git clone <你的仓库地址> /opt/screenwall-src
cd /opt/screenwall-src
cargo build --release -p screenwall-server
```

只编服务端，产物在 `target/release/screenwall-server`（Windows 实测 7.2 MiB，
Linux 通常略大一点）。
**不要**跑 `cargo build --release --workspace` 或 `cargo test --workspace`，
会在 agent / client 上失败。

#### 3. 安装与配置

```bash
install -Dm755 target/release/screenwall-server /opt/screenwall/screenwall-server
mkdir -p /opt/screenwall

cat > /opt/screenwall/server.toml <<'EOF'
bind = "127.0.0.1:8787"
db_path = "/opt/screenwall/screenwall.db"
allow_registration = false
token_ttl_days = 30
max_frame_bytes = 1048576

[limits]
max_devices_per_account = 20
login_per_minute = 30
register_per_hour = 10
EOF

useradd --system --no-create-home --shell /usr/sbin/nologin screenwall
chown -R screenwall:screenwall /opt/screenwall
```

`db_path` 一定要写**绝对路径**：systemd 的工作目录不是 `/opt/screenwall`，
写相对路径会落到别处（默认 `WorkingDirectory` 是 `/`）。

#### 4. systemd 单元

```ini
# /etc/systemd/system/screenwall.service
[Unit]
Description=ScreenWall server
After=network.target

[Service]
Type=simple
User=screenwall
Group=screenwall
WorkingDirectory=/opt/screenwall
ExecStart=/opt/screenwall/screenwall-server run --config /opt/screenwall/server.toml
Restart=always
RestartSec=3

# 服务端只监听 127.0.0.1，对外由 Caddy 提供 TLS。
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/opt/screenwall

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now screenwall
systemctl status screenwall
journalctl -u screenwall -f          # 看日志
```

#### 5. Caddy 反向代理与证书

```bash
pacman -S --needed caddy
```

```caddyfile
# /etc/caddy/Caddyfile
screenwall.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

```bash
systemctl enable --now caddy
```

Caddy 会自动申请并续期 Let's Encrypt 证书，**前提是域名已解析到这台机器且 80 / 443
端口能从公网访问**。

沙盒 / 容器面板如果只给了一个 IP、没有域名，就没有证书可用，只能临时跑
`http://<ip>:8787` 做验证。客户端能连，但**画面与密码都是明文传输**，不要长期这么用。
这种情况下至少把端口改成非默认的，并且只在自己的网络里试。

用 Caddy 时还要确认它转发的 WebSocket 连接不会被缓冲——Caddy v2 的
`reverse_proxy` 默认就支持 WebSocket 升级，不需要额外配置。

**检查项**：`curl -s https://你的域名/healthz` 能返回 JSON，就说明 Caddy、TLS
和上游服务都通了。

#### 6. 建账号

```bash
# 交互式输入密码（推荐）
sudo -u screenwall /opt/screenwall/screenwall-server \
  user add kay --config /opt/screenwall/server.toml

# 或非交互（密码会进 shell 历史，仅限首次初始化）
sudo -u screenwall /opt/screenwall/screenwall-server \
  user add kay --password '<密码>' --config /opt/screenwall/server.toml
```

`--config` 是 `global = true` 的全局参数，写在子命令前后都可以（上面写成子命令之后，
和 `--help` 里的展示一致）。`user add` 不带 `--password` 时会从终端交互读取密码并
要求二次确认；这个读取走 `rpassword`，**不能从管道喂**，脚本里请用 `--password`。

#### 7. 验证

```bash
curl -s http://127.0.0.1:8787/healthz
# {"ok":true,"version":"0.1.0"}

curl -s https://screenwall.example.com/healthz
# 同上（走 Caddy 与 TLS）
```

然后客户端登录时服务器地址填 `https://screenwall.example.com`（客户端会自动换成
`wss://`）。填 `http://` 也能连，但不推荐。

#### 8. 带宽是真正的瓶颈

服务端本身极省资源：实测（Windows，本机自控）**常驻内存 14.2 MB、CPU 累计 0.9 秒**。
但中转是纯字节转发，一路 1080p / 30fps 控制会话实测 **≈22.7 Mbps 持续**：

| 并发场景 | 服务器出向带宽 |
|---|---|
| 1 路控制 | ≈23 Mbps |
| 3 路控制 | ≈68 Mbps |
| 4 台设备互看（屏幕墙 1 FPS 另算） | ≈90 Mbps |

换算成流量：**1 路控制 ≈10 GB/小时**。所以 1 TB 月流量能撑约 **103 小时
（≈4.3 天）连续控制**；如果每天用 3.5 小时，一个月刚好用完。

这是单程估算。中转会同时往主控端和被控端发，实际计费按「入向 + 出向」，
口径取决于服务商，保守按 2 倍估。

所以选机器看的是**带宽和月流量配额**，不是 CPU / 内存：

- 1 核 1 GB 的轻量 VPS 足够跑几十路（内存 14 MB、CPU 基本闲着）。
- 便宜套餐常给 100 Mbps 共享带宽 + 1 TB 月流量。按上面的换算，**每天用 3–4 小时
  就接近上限**；偶发使用没问题，长期挂着当监控用要选流量包大或按带宽计费的方案。
- 屏幕墙缩略图（1 FPS / 320px）流量小得多，可以常开。
- 提高画质档位会线性增加带宽；低画质档能显著降低流量。

#### 9. 更新服务端

```bash
cd /opt/screenwall-src && git pull
cargo build --release -p screenwall-server
install -Dm755 target/release/screenwall-server /opt/screenwall/screenwall-server
systemctl restart screenwall
```

重启会断开所有在线会话（客户端有自动重连，会按 1s/2s/4s/8s/8s 退避重试），
`/api/v1/metrics` 的计数也会归零（指标全在内存里）。数据库不受影响。

## 7. 错误码

| code | 含义 |
|---|---|
| 4200 | 设备离线或不存在 |
| 4201 | 会话被对端关闭 / 设备断线 |
| 4202 | 帧超过大小上限 |
| 4203 | 会话号无效 |
| 4204 | 设备拒绝会话 |
| 4205 | 会话建立过于频繁（触发限流） |
| 4301 | 令牌无效或过期 |
| 4302 | 账号被禁用 |
| 4303 | 请求过于频繁 |

应用层错误仍使用 [protocol.md](protocol.md) 第 9 节的 4001–4109。

## 8. 典型流程

```
主控端                         服务器                         被控端
  │ 登录 POST /login              │                              │
  │ ─────────────────────────────>│                              │
  │ <──── token + device          │                              │
  │                               │<──── 登录 + /ws/device ──────│
  │                               │──── ready ──────────────────>│
  │ ── /ws/view ─────────────────>│                              │
  │ <──── devices（含被控端）─────│                              │
  │ ── open(device) ─────────────>│                              │
  │                               │──── open(session) ──────────>│
  │                               │<─── open.ok(session) ────────│
  │ <──── open.ok(session) ───────│                              │
  │ ── 帧(session) ──────────────>│──── 帧(session) ────────────>│
  │ <──── 帧(session) ────────────│<─── 帧(session) ─────────────│
```

会话内的第一条 JSON 消息仍是应用层的 `hello`，由被控端校验令牌或直接信任服务器的账号鉴权（v1 信任服务器）。`welcome` 之后进入 `status` / 视频 / 输入流程。

## 9. 安全说明

- 服务器只保存 Argon2id 密码哈希、令牌 SHA-256 哈希、设备元数据；不保存画面与文件内容。
- 客户端与服务器之间必须 TLS；开发环境的 `ws://` 只允许 `127.0.0.1`。
- 令牌默认 30 天过期；退出登录、删除设备会立即吊销。
- 服务器被攻破的影响范围是账号与设备目录；后续版本加入端到端会话密钥，让中转服务器无法解读画面。
- 公开注册默认关闭，避免陌生人注册后占用服务器资源。
