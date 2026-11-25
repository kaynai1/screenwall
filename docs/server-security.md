# 服务器安全加固说明

> 配套文档：[server.md](server.md) · [protocol.md](protocol.md)
> 本文描述 `crates/server` 新增的 TLS、登录限流、令牌吊销、审计与指标。

## 1. TLS / WSS

服务器现在可以自己终结 TLS；不配置证书时行为与以前完全一致。

`server.toml`（或 `crates/server/server.example.toml`）：

```toml
tls_cert = "server.crt"   # PEM 证书链
tls_key = "server.key"    # PEM 私钥（PKCS#8 / PKCS#1 / SEC1）
```

- 两个字段同时填写才启用；只填一个会直接报错，避免误以为已加密。
- 相对路径按配置文件所在目录解析。
- 启用后同一端口同时提供 HTTPS 与 WSS，路由和 WebSocket 升级逻辑不变。
- 私钥只用于 `tokio-rustls` 的服务端握手；未启用 TLS 时 rustls 完全不参与。
- 仍然推荐在前面放 Caddy / Nginx 终结 TLS；此时不要配置这两个字段。

自签证书：

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout server.key -out server.crt \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

要点：

- 现代客户端（包括 rustls）要求证书带 **SAN**，只有 CN 不够。
- 访问地址是域名就写 `DNS:域名`，是 IP 就写 `IP:地址`，可同时写多个。
- 自签证书需要客户端操作系统信任，否则客户端会拒绝连接；生产环境建议用
  Let's Encrypt 或内部 CA 签发的证书。
- 证书到期后替换文件并重启服务器即可。

## 2. 登录失败限流

> `server.md` 里「同一 IP 每分钟最多 5 次失败」的旧描述以本节为准。

- 维度：`用户名 + 客户端 IP`。
- 规则：连续失败 **5 次**后锁定 **60 秒**；锁定期内不校验密码，直接拒绝。
- 成功登录立即清零该维度计数。
- 内存实现，重启后计数丢失（可接受）。
- 没有新失败超过一个锁定周期后条目会被清理，避免内存增长。

触发锁定或处于锁定期时返回：

```http
HTTP/1.1 429 Too Many Requests
Retry-After: 60
Content-Type: application/json

{ "error": "rate_limited", "msg": "登录失败次数过多，请在 60 秒后重试" }
```

注意：`ConnectInfo` 取的是 **TCP 对端地址**。如果服务器前面有反向代理，
所有请求的来源都会是代理地址（通常是 `127.0.0.1`），限流会退化成「按用户名」。
若要按真实客户端 IP 限流，需要在代理层限流，或后续显式配置可信代理并解析
`X-Forwarded-For`（目前未实现，避免伪造头绕过限流）。

## 3. 按设备吊销令牌

```http
POST /api/v1/devices/{device_id}/revoke
Authorization: Bearer <token>
```

- `device_id` 直接取 `GET /api/v1/devices` 返回列表中的 `id`。
- 需要登录；只能吊销当前账号自己的设备。
- 吊销该设备的**全部**令牌（包括当前正在使用的令牌，如果调用者本身就是该设备）。
- 响应：

```json
{ "ok": true, "revoked": 2 }
```

`revoked` 是本次 UPDATE 命中的令牌行数（包含此前已吊销的行）。

效果：

- 被吊销令牌后续访问任何 `/api` 接口都会返回 `401 invalid_token`。
- 被吊销令牌无法建立 `/ws/device` 或 `/ws/view` 长连接。
- 服务器会主动断开该设备当前的被控端连接、主控端连接，并结束相关会话
  （结束时会补齐审计记录）。
- 设备本身不会从设备列表删除；重新登录即可获得新令牌。

## 4. 会话审计

SQLite 表 `audits`（服务器启动时自动创建）：

| 字段 | 含义 |
|---|---|
| `user_id` | 账号（审计按账号隔离） |
| `session_id` | 会话编号 |
| `controller_device_id` | 主控端设备（来自主控连接令牌） |
| `controlled_device_id` | 被控端设备 |
| `started_at` | 会话建立时间（设备回 `open.ok` 的瞬间，毫秒时间戳） |
| `ended_at` | 会话结束时间；异常退出时可能为 `NULL` |
| `duration_ms` | 时长（毫秒） |
| `bytes_up` / `bytes_down` | 主控→被控 / 被控→主控的中转负载字节数 |

写入时机：

- 会话建立：被控端回 `open.ok` 后写入开始记录。
- 会话结束：主控关闭、被控关闭、任一端断线、设备重连、令牌吊销、设备被删除，
  都会补齐结束时间、时长与字节数。

字节数的口径是 **隧道帧的 payload 长度**，不含帧头。

分页查询（仅当前账号）：

```http
GET /api/v1/audits?limit=50&offset=0&deviceId=dev-a
Authorization: Bearer <token>
```

- `limit` 默认 50，范围 1–500。
- `offset` 默认 0，负数按 0 处理。
- `deviceId` 可选，同时匹配主控端与被控端（`controller_device_id` 或
  `controlled_device_id`）；空字符串视为未传。
- 按 `started_at DESC, id DESC` 排序，分页稳定。
- `total` 是满足过滤条件的总条数（与当前页无关）。

```json
{
  "audits": [
    {
      "id": 12,
      "sessionId": 3,
      "controllerDeviceId": "ctrl-1",
      "controlledDeviceId": "dev-a",
      "startedAt": 1790000000000,
      "endedAt": 1790000035000,
      "durationMs": 35000,
      "bytesUp": 123456,
      "bytesDown": 9876543
    }
  ],
  "total": 1
}
```

如果服务器在会话进行中崩溃，该行会保留且 `endedAt` 为 `null`、字节数停留在崩溃前
最后一次写入的值；审计只在结束时补齐，不做逐帧持久化。

## 5. 请求限流与设备配额

除了第 2 节的「登录失败锁定」，服务器还有一套通用令牌桶限流与单账号设备配额，
参数都在 `server.toml` 的 `[limits]` 段（省略时用默认值，旧配置照常启动）：

| 配置项 | 默认 | 作用 |
|---|---|---|
| `max_devices_per_account` | 20 | 单账号最多绑定设备数；0 = 不限制 |
| `login_per_minute` / `login_burst` | 30 / 30 | `POST /api/v1/login`，按客户端 IP |
| `register_per_hour` / `register_burst` | 10 / 10 | `POST /api/v1/register`，按客户端 IP |
| `view_connects_per_minute` / `view_connect_burst` | 30 / 30 | `/ws/view` 建连，按账号 |
| `session_opens_per_minute` / `session_open_burst` | 60 / 60 | 会话建立（`t = open`），按账号 |

- 令牌桶：每 `per` 时间补充 `rate` 个令牌，桶容量 `burst`；`rate = 0` 关闭该条限流。
- HTTP 接口超限返回 `429 rate_limited` + `Retry-After`，中文提示带剩余秒数。
- `/ws/view` 超限在握手阶段直接返回 `429`（连接不会升级）。
- 会话建立超限回 `open.err`，`code = 4205`，主控端可提示用户稍后重试。
- 触发限流时只写一条 `warn` 日志（含 key 与剩余秒数），不逐请求刷屏。
- 与登录失败锁定互补：请求频率限制挡高频扫描，失败锁定挡慢速撞库；
  登录成功不会重置请求频率桶，避免「正确密码一次 + 猜很多次」绕过。
- 限流状态是进程内内存，重启清零；反向代理后所有请求来源相同，按 IP 的维度会退化，
  此时应在代理层限流（见第 2 节说明）。

设备配额：

- 登录绑定设备与 `/ws/device` 上线共用同一上限。
- 已登记设备重复登录/重连不计新增，不会被拒。
- 新设备超限返回 `403 device_limit_reached` 与中文提示，客户端应引导用户删除旧设备；
  不会静默丢弃设备。
- 删除设备（`DELETE /api/v1/devices/{id}`）立即释放名额。

## 6. 指标

```http
GET /api/v1/metrics
Authorization: Bearer <token>
```

返回 JSON（不是 Prometheus 文本）：

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

字段含义、如何判断并发/带宽/错误率、配置项说明见 [server.md](server.md) 第 6.1–6.2 节。

要点：

- 所有计数都在真实处理路径上累加（会话建立/关闭、二进制帧转发、`open.err`、握手、
  转发失败），不是采样或占位数据。
- `errors.byCode` 的键是应用层错误码（`4200` 设备离线、`4204` 设备拒绝、
  `4205` 会话建立被限流等），便于区分故障类型。
- `backpressure.droppedFrames` 只在转发目标已关闭/发送失败时增长。
- 在线设备数、活跃会话数、账号数是请求时现算；其余为进程内累计计数，重启归零。
- 未登录访问返回 `401 invalid_token`。

旧接口 `GET /metrics` 保留，返回 `onlineDevices` / `activeSessions` /
`totalBytes` / `errorCount`，供已有监控脚本使用。

## 7. 测试

```bash
# 单元测试：登录锁定、令牌桶、指标快照、按设备吊销、审计读写
# 集成测试：429 与中文文案、吊销后的 /api 与 /ws 拒绝、审计/指标生命周期、
#           设备上限与重复登录、注册/建连/会话建立限流、/api/v1/metrics 真实计数
# TLS 冒烟：rcgen 自签证书 → TlsListener → HTTPS /healthz
./scripts/cargo.bat test -p screenwall-server
```

相关源码：

- `crates/server/src/ratelimit.rs`：登录失败锁定 + 通用令牌桶
- `crates/server/src/metrics.rs`：指标计数器、错误码分布、5 分钟错误率
- `crates/server/src/config.rs`：`[limits]` 段
- `crates/server/src/tls.rs`：rustls 监听与 ConnectInfo
- `crates/server/src/db.rs`：`audits` 表、按设备吊销、设备/账号计数
- `crates/server/src/api.rs`：登录/注册限流、设备配额、`/revoke`、`/audits`、`/metrics`
- `crates/server/src/ws.rs`：握手指标、建连/会话建立限流、转发字节与丢帧
- `crates/server/src/state.rs`：会话统计、审计生命周期、配额检查
- `crates/server/tests/security.rs`、`observability.rs`、`tls.rs`
