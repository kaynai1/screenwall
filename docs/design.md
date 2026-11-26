# ScreenWall 设计文档

> 多机屏幕墙远程控制 · 自用 · v0.1 草案
> 日期：2026-11-23　技术栈：Rust + Tauri 2　参照产品：GT远程（8dks.com）

> ## ⚠ 这是**计划稿**，不是实现说明
>
> 本文写于动手之前，描述的是当时**打算**怎么做。落地时有多处改成了更简单的方案，
> 以下几处**文档与代码不一致**，以代码为准：
>
> | 主题 | 本文（计划） | 实际实现 |
> | --- | --- | --- |
> | 采集 | Windows.Graphics.Capture（WGC）+ D3D11 | **DXGI Desktop Duplication**，失败回退 **GDI `StretchBlt`**（`crates/agent/src/capture.rs`） |
> | 编码 | FFmpeg（`ffmpeg-next`），mjpeg / h264_nvenc / qsv / amf / libx264 | **`jpeg-encoder`**，只有 MJPEG，无 H.264、无硬件编码 |
> | 解码 | WebCodecs H.264 + canvas | **`<img>` / canvas 直接显示 JPEG**，不走 WebCodecs |
> | 打包 | FFmpeg DLL 随包分发 | **不依赖 FFmpeg**，单 exe 约 4.5 MiB 安装包 |
> | 屏幕墙「文件拖到哪块屏发哪台」 | v1 范围 | **已实现（2026-11-25）**，卡片为 drop 目标 |
>
> 当前真实能力与验收证据见 [DELIVERY.md](DELIVERY.md)，协议见 [protocol.md](protocol.md)。
> 本文只保留作为设计思路的历史记录。
>
> 正文里的计划条目保持原样，凡与代码不符的都在其下用
> `> **现状（2026-11-24 核对）**：…` 标注实际实现。带这个标注的条目一律以代码为准。

## 1. 产品定位

在一台主控电脑上同时看到 3–5 台 Windows 电脑的实时画面（屏幕墙），点开任意一台就能像本地一样操作；可以多选群控；文件拖到哪块屏就发给哪台；异地可用，能唤醒关机的电脑。

### 1.1 v1 范围

| 能力 | 说明 |
|---|---|
| 屏幕墙 | 3–5 台，缩略流 2–5 fps，在线状态与延迟 |
| 单台控制 | 最高 1080p / 30–60 fps，键鼠接管，全屏，3D 视角 |
| 群控 | 选中的设备同步执行同一套键鼠操作 |
| 文件分发 | 单台或多台，目录递归，同名另存，独立进度 |
| 远程开机 | 同网广播；异地由同网 helper 代发 |
| 账号体系 | 自有账号，注册后同一账号下的设备自动出现在列表 |
| 异地访问 | 客户端与服务器都主动出站，经自建中转服务器连接 |

> **现状（2026-11-24 核对）**：本表有三项与实现不符。
> - 屏幕墙缩略流不是 2–5 fps：客户端固定 1 fps / 320px（`crates/client/src/wall.rs` 的 `WALL_FPS`、`WALL_WIDTH`）。
> - 「3D 视角」未实现：没有 Pointer Lock，客户端也不发相对位移（见第 7 节）。
> - 远程开机的「异地由同网 helper 代发」未实现：只有主控端本机广播（见第 9 节）。

### 1.2 非目标（v1 不做）

- 驱动级键鼠（需要签名内核驱动，成本高）
- 微信 / QQ 等第三方登录，v1 只做自有账号
- 自建 UDP 打洞（v1 先全部走服务器中转，v2 再优化为直连优先）
- 被控端支持 Linux / macOS（v1 仅 Windows 10/11 x64）
- 音频、剪贴板同步、摄像头、多人协作、权限体系
- 接管 UAC 安全桌面和登录界面

> **现状（2026-11-24 核对）**：音频与文本剪贴板同步**已经实现**（不在 v1 之外了）：系统回环 / 麦克风声音走 Opus，音量联动、AEC 都有；剪贴板只同步纯文本（≤64K 字符）。摄像头、多人协作、权限体系仍未做。UAC 安全桌面与登录界面仍不支持（被控端是普通用户进程）。

## 2. 总体架构

```
                 ┌──────── 中转 / 账号服务器（公网 VPS）────────┐
                 │ 注册登录 · 设备目录 · 在线状态 · 会话转发      │
                 └──────▲──────────────────────────▲───────────┘
                        │ wss:// 主动出站            │ wss:// 主动出站
        ┌───────────────┴────────────┐   ┌──────────┴─────────────────┐
        │ ScreenWall（电脑 A）        │   │ ScreenWall（电脑 B / C…）   │
        │ 托盘常驻：被控 + 主控        │   │ 同账号登录，看到设备列表     │
        │ 采集 / 编码 / 输入注入       │   │ WebView：屏幕墙 / 控制      │
        └────────────────────────────┘   └────────────────────────────┘
```

关键决策：

1. **一个安装包，双角色。** 每台 Windows 装同一个 `ScreenWall`，登录后既是被控端也是主控端；不需要分别安装 Agent 和客户端，也不需要手动填对方 IP。
2. **账号 + 自建服务器。** 设备目录、在线状态和会话转发由一台公网服务器负责。两端都主动向服务器建立出站连接，因此不需要公网 IP、端口映射或额外组网软件。
3. **v1 先全部走服务器中转，v2 再优化直连。** 中转实现简单、任何 NAT 下都能用；代价是画面经过服务器。等基础功能稳定后，再加入服务器协助的 UDP 打洞，直连失败自动回落到中转。
4. **所有网络连接放在 Tauri 的 Rust 侧。** 浏览器 WebSocket 无法信任自签证书，也无法做证书指纹固定；Rust 侧统一处理 TLS、鉴权、重连后，通过本机回环 WebSocket 把控制消息和视频帧转发给前端。回环流量不出本机，不需要 TLS。
5. **不用 WebRTC。** 中转层是 WebSocket，比 ICE/DTLS/RTP 简单一个数量级；浏览器端只需 WebSocket + WebCodecs。
6. **被控端是交互式托盘进程，不是 Windows 服务。** 服务运行在 session 0，采集不到用户桌面。

代价与退路：服务器带宽承担全部画面流量，需要按实际码率控制画质；服务器能看到转发明文（自用可信，后续可加端到端加密）。中转层保持独立，未来可替换为 QUIC / WebTransport。

> **现状（2026-11-24 核对）**：
> - 第 3 条已过时：局域网直连**已经落地**（同网段可绕过服务器，见 [direct.md](direct.md)），不是留到 v2；UDP 打洞仍未做。
> - 第 4 条的前半段属实（TLS、鉴权、重连都在 Rust 侧），但**没有本机回环 WebSocket**：Rust 侧直接通过 Tauri 事件（`app.emit`，如 `control-frame` / `wall-frame` / `control-*` / `devices` / `account`）把画面与控制消息推给前端，命令走 `invoke`。
> - 第 5 条只对了一半：确实不用 WebRTC，但前端也不用 WebCodecs（视频是 JPEG，直接显示）。

## 3. 部署与运行

| 项 | 说明 |
|---|---|
| 客户端 | 一个 Tauri 桌面应用，托盘常驻；同一进程同时提供被控与主控能力 |
| 服务器 | `screenwall-server`，部署在一台有公网地址的 VPS；也可先跑在家中常开电脑上做开发测试 |
| 自启 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 或启动文件夹 |
| 网络 | 客户端与服务器之间只有出站 WSS，不需要开放入站端口 |
| 设备发现 | 登录同一账号后由服务器下发设备列表，不使用广播发现 |
| 局域网 | 同一局域网内可绕过服务器直连，作为可选加速路径 |
| 配置目录 | `%APPDATA%\ScreenWall\`（`client.json`、登录令牌、证书、日志） |
| 日志 | `%APPDATA%\ScreenWall\logs\`，滚动（**实际按大小滚动**，不是按天，见下方现状），令牌与帧数据脱敏 |

> **现状（2026-11-24 核对）**：
> - 日志**按大小滚动**，不是按天：单文件 2 MiB，保留 3 个历史文件（`crates/client/src/logging.rs` 的 `MAX_BYTES` / `MAX_FILES`）。运行日志不写帧数据；令牌的脱敏发生在导出诊断包时（`client.json` 的 `token`、`knownPeers[].token`、`directTokens[]` 替换为 `<已脱敏>`），日志写入器本身不做字段脱敏。
> - 「不需要开放入站端口」只对中转成立：打开局域网直连后本机会监听 `0.0.0.0:47900`（见 [direct.md](direct.md)）。
> - 配置目录里有两份配置：客户端的 `client.json` 与独立 Agent 的 `agent.toml`，字段见 5.7 节末尾。

## 4. 技术栈

| 层 | 选型 | 说明 |
|---|---|---|
| 语言 | Rust stable（MSVC 工具链） | 单 exe，无运行时依赖 |
| GUI | Tauri 2 + WebView2 | Win10/11 自带 WebView2 |
| 前端 | React 18 + TypeScript + Vite | 屏幕墙用 canvas 与 DOM 组合 |
| 网络 | tokio、tokio-tungstenite、rustls、rcgen | 异步 IO、WebSocket、TLS、自签证书 |
| 服务器 | axum、tokio、rusqlite（bundled）、argon2 | HTTP API、设备在线表、中转、密码哈希 |
| 采集 | `windows` crate → Windows.Graphics.Capture（WGC）+ D3D11 | Win10 1903+ 系统 API |
| 编码 | FFmpeg（`ffmpeg-next`）：mjpeg、h264_nvenc/qsv/amf/libx264 | 一条管线覆盖缩略与高清 |
| 解码 | WebCodecs（H.264）+ canvas（JPEG） | WebView2 基于 Chromium，支持硬件解码 |
| 输入 | `SendInput`；客户端用 Pointer Lock 捕获相对移动 | 见第 7 节 |
| 配置 / 日志 | serde + toml；tracing + tracing-appender | |
| 打包 | Tauri bundler（NSIS） | FFmpeg DLL 随包分发 |

FFmpeg 采用 BtbN 的 shared 构建（含 nvenc / qsv / amf），首次启动探测可用编码器。

> **现状（2026-11-24 核对）**：第 4 章这张表写的是计划选型，落地时换了实现。逐项对照：
>
> | 层 | 本文（计划） | 实际实现 |
> |---|---|---|
> | 采集 | WGC + D3D11 | **DXGI Desktop Duplication**，不可用时回退 **GDI `StretchBlt`**（`crates/agent/src/capture.rs`） |
> | 编码 | FFmpeg，mjpeg / h264_nvenc / qsv / amf / libx264 | **`jpeg-encoder`**，只有 MJPEG，无 H.264、无硬件编码 |
> | 解码 | WebCodecs（H.264）+ canvas | **`<img>` / canvas 直接显示 JPEG**，不走 WebCodecs |
> | 输入 | `SendInput`；Pointer Lock 捕获相对移动 | `SendInput` 属实；**Pointer Lock 未做**，只发绝对坐标（见第 7 节） |
> | 配置 / 日志 | serde + toml；tracing + tracing-appender | 配置：Agent 用 `agent.toml`（toml），客户端用 `client.json`、服务器用 `server.toml`；日志：tracing + 自写的按大小滚动写入器，没有用 tracing-appender |
> | 打包 | FFmpeg DLL 随包分发 | **不依赖 FFmpeg**，单 exe 安装包 |
>
> 本章下方「FFmpeg 采用 BtbN 的 shared 构建…」一句是计划描述，实际不使用 FFmpeg。

## 5. Agent 设计（crates/agent）

### 5.1 任务模型

- 单进程、tokio 多线程 runtime
- 采集线程：WGC 要求专用消息循环，独立线程运行，帧写入有界通道
- 编码任务：每个订阅一个编码器实例
- 会话任务：每条 WS 连接一个，负责订阅、输入、文件、心跳
- 背压：视频通道满时丢最旧帧；输入和文件事件不丢

> **现状（2026-11-24 核对）**：任务模型基本属实，但「采集线程：WGC 要求专用消息循环」不适用——实际没有 WGC，DXGI 采集在独立的抓屏线程里跑，编码在另一条线程（`crates/agent/src/capture.rs` 的 `run_capture`），中间是单槽邮箱「新帧覆盖旧帧」。

### 5.2 采集（`capture.rs`）

- 通过 WGC 采集指定显示器，默认主显示器；返回 BGRA 纹理，拷贝到系统内存
- 只在画面变化时产生帧；按订阅帧率进一步节流
- 光标：墙上流包含光标（便于判断对方是否在用）；聚焦流不包含光标，由客户端在本机绘制，避免光标延迟和重影
- 进程声明 per-monitor v2 DPI 感知，所有坐标使用物理像素
- 已知限制：部分全屏独占游戏用 WGC 会黑屏，v2 用 DXGI Desktop Duplication 兜底

> **现状（2026-11-24 核对）**：
> - 采集后端是 **DXGI Desktop Duplication**（不是 WGC），初始化失败回退 **GDI `StretchBlt`**；WGC 那条「v2 兜底」的路线没有采用。
> - 「只在画面变化时产生帧」只对 DXGI 成立：`AcquireNextFrame` 超时返回 `Ok(None)`，不出帧；**GDI 回退路径每个 tick 都抓一帧**（`crates/agent/src/capture.rs` 的 `GdiCapturer::grab` 直接 `StretchBlt`）。桌面静止时编码线程每 500 ms 补发一帧缓存 JPEG（`IDLE_RESEND_AFTER`），主控端帧率不会归零。
> - 光标按 `VideoSub.cursor` 决定是否画进画面（`crates/agent/src/cloud.rs`：`cursor: sub.cursor && !view_only`）。DXGI 路径用 `GetFramePointerShape` 取形状、`DXGI_OUTDUPL_FRAME_INFO::PointerPosition` 取位置（开流后鼠标一直没动时用 `GetCursorPos` 补一次初始位置），再由 `crates/agent/src/cursor.rs` 的 `composite_cursor` 合成到帧上（单色/彩色/带掩码三种形状，只移动光标时不重拷桌面纹理，用备份还原旧位置）；GDI 回退路径用 `GetCursorInfo` + `DrawIconEx` 直接画进 DIB。这与本条计划**相反**：墙上流订阅 `cursor: false`（且 `view_only` 强制不画），聚焦流客户端订阅 `cursor: true`，光标由 Agent 画进画面，前端没有本地绘制光标的叠加层。

### 5.3 编码（`encoder.rs`）

- 统一走 FFmpeg，进程内调用，不启动子进程
- **墙上流**：BGRA → swscale → YUV420P → MJPEG，`maxWidth` 默认 480，质量 70
- **聚焦流**：BGRA → NV12 → H.264，按顺序探测 `h264_nvenc` → `h264_qsv` → `h264_amf` → `libx264`（ultrafast / zerolatency）
- 启动自检：每种编码器实际编码一帧，失败自动降级，并把最终编码器上报给客户端
- 关键帧：订阅开始时强制一个 IDR；客户端可请求关键帧

> **现状（2026-11-24 核对）**：本章整体是计划，落地只保留了 MJPEG 这一条路，且实现换成了 `jpeg-encoder` 而不是 FFmpeg：
> - 没有 `encoder.rs` 这个模块，也没有 FFmpeg / swscale / NVENC / QSV / AMF / libx264；墙上流与聚焦流都是 `jpeg-encoder` 编的 JPEG（BGRA 直编）。
> - 「按订阅帧率节流」属实；「启动自检探测编码器」不存在。
> - 没有 IDR / 关键帧概念（MJPEG 每帧都是完整图）；`video.keyframe` 消息在协议里有定义，但 JPEG 路径没有对应的编码器状态可重置。
> - 编码器会在单帧超过中转上限时按质量档位逐级降质（`FALLBACK_QUALITY` → `MIN_JPEG_QUALITY`），仍超限就丢帧。

### 5.4 输入注入（`input.rs`）

- 封装 `SendInput`
- 绝对移动：归一化坐标 `0..1` → 目标显示器矩形 → 虚拟桌面 `0..65535`（`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE`）
- 相对移动：`MOUSEEVENTF_MOVE`，用于 3D 视角
- 按键：发送 Windows 扫描码 + extended 标志（方向键、Insert/Delete、小键盘回车等）；文本输入用 `KEYEVENTF_UNICODE`
- 滚轮：`MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL`，一格 120
- 维护当前按下的键和鼠标键集合；会话结束、客户端失焦、收到 `releaseAll` 时全部释放
- 限制：目标前台程序以管理员运行时，普通权限进程的 `SendInput` 会被 UIPI 拦掉；需要在配置里说明，必要时以管理员运行 Agent

> **现状（2026-11-24 核对）**：绝对移动、按键扫描码、滚轮、按下集合与 `release_all` 都属实。**相对移动没有调用方**：`input.rs` 的 `rel` 分支存在，但客户端生产代码只发绝对坐标（见第 7 节的现状说明）。

### 5.5 文件接收（`files.rs`）

- 复用同一条 WS，二进制块帧
- 落盘根目录 `%USERPROFILE%\Downloads\ScreenWall\<设备名>\<yyyyMMdd-HHmmss>\`
- 校验大小与 SHA-256；路径安全检查见 8.3

> **现状（2026-11-24 核对）**：上传**平铺**落到 `%USERPROFILE%\Downloads\ScreenWall`，没有「设备名 / 时间戳」两层子目录（`crates/agent/src/files.rs` 的 `receive_root`）；同名文件自动加 `(1)` 属实。文件块走 `file.chunk` 的 JSON + base64（不是二进制块帧），单块 256 KiB（见 8.1）。

### 5.6 远程开机（`wol.rs`）

- 作为 helper 时接收 `wol.request`，向指定广播地址发送 magic packet
- 只接受私有网段内的广播地址和合法单播 MAC；每 10 秒最多一次

> **现状（2026-11-24 核对）**：
> - Agent 侧确实实现了 `wol.request` 处理（`crates/agent/src/cloud.rs`、`session.rs` 都接了 `WolRequest` → `wol::send_magic_packet`），校验规则与「每 10 秒最多一次」也属实（`crates/agent/src/wol.rs` 的 `MIN_INTERVAL`）。
> - 但**主控端从不发 `wol.request`**：界面上的「唤醒」走客户端本机命令 `wake_on_lan`，直接由主控机广播（`crates/client/src/main.rs` 的 `wake_on_lan`），跨网段无法唤醒。
> - 「作为 helper 代发」这套协作流程（选择同网在线设备、设备设置里指定）没有实现。

### 5.7 服务与安全（`cloud.rs`、`config.rs`）

- 启动后使用登录令牌向 `wss://<server>/ws/device` 建立**出站**长连接，断线指数退避重连
- 不监听公网端口；可选在局域网内监听 `47900/tcp` 供同网直连（端口绑 `0.0.0.0`，但 accept 时只放行局域网来源地址，见 [direct.md](direct.md)）
- 服务器下发 `tunnel.open` 后，Agent 在现有会话逻辑上接一条虚拟流，消息格式与局域网直连一致
- 服务器连接使用 TLS（生产环境由 Caddy/Nginx 提供证书）；局域网直连仍用自签证书 + 指纹固定
- 登录失败与设备令牌失效由服务器限速并通知客户端重新登录
- `client.json` 关键项：`server_url`、`account_token`、`device_id`、`allow_control`、`autostart`、`wall_quality`、`focus_bitrate`

> **现状（2026-11-24 核对）**：配置分两份文件，字段与本节列的不一样。
> - `client.json`（`crates/client/src/main.rs` 的 `ClientConfig`）：`server_url`、`token`、`username`、`device_id`、`allow_control`、`ui_zoom`、`clipboard_sync`、`close_to_tray`、`direct_enabled`、`direct_port`、`known_peers`、`direct_tokens`。没有 `account_token`（这里叫 `token`），没有 `autostart`，也没有 `wall_quality` / `focus_bitrate`（画质是会话内 `video.sub` 参数，不持久化）。
> - `agent.toml`（`crates/agent/src/config.rs` 的 `AgentConfig`，独立 Agent 用）：`name`、`port`、`token`、`allow_control`、`clipboard_sync`、`server_url`、`account_token`、`device_id`。
> - 开机自启不进配置文件，写注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
> - 局域网直连不是「可选监听 47900」这么简单，它有独立的白名单、配对码与来源校验，见 [direct.md](direct.md)。

### 5.8 托盘（`tray.rs`）

- 登录状态（账号、设备名、服务器连通性）
- 「允许被控」开关
- 打开主窗口、开机自启开关、打开配置目录、退出登录、退出

> **现状（2026-11-24 核对）**：托盘菜单实际只有 **3 项**——「打开主界面」「允许远程控制」「退出 ScreenWall」（`crates/client/src/tray.rs` 的 `setup`）。**没有**登录状态、开机自启开关、打开配置目录、退出登录这 4 项；这些能力只存在于设置页。

## 6. 视频管线

### 6.1 墙上缩略流

| 参数 | 默认值 |
|---|---|
| 最大宽度 | 480 px（宽高比保持） |
| 帧率 | 2 fps（可调 1–5） |
| 编码 | MJPEG，质量 70 |
| 光标 | 包含 |

5 台 × 2 fps × 约 40 KB ≈ 400 KB/s（约 3.2 Mbps）。5 Mbps 的中转服务器可以承载整面墙；如果服务器带宽更小，自动降到 1 fps 或更低质量。

> **现状（2026-11-24 核对）**：
> - 屏幕墙实际固定 **1 fps / 320px 宽 / 质量 60**（`crates/client/src/wall.rs` 的 `WALL_FPS`、`WALL_WIDTH`、`WALL_QUALITY`），不是「2 fps 可调 1–5」。Agent 侧把 wall 流夹到 `fps ≤ 2`、`maxWidth ≤ 480`（`crates/agent/src/cloud.rs`）。
> - 「服务器带宽不足自动降帧/降质」**没有实现**，客户端也不根据带宽调整缩略流参数。
> - 「光标：包含」未实现：屏幕墙会话订阅 `cursor: false`，Agent 对 `view_only` 会话也强制不画光标（`crates/agent/src/cloud.rs` 的 `cursor: sub.cursor && !view_only`），见 5.2 的现状说明。

### 6.2 聚焦高清流

| 参数 | 默认值 |
|---|---|
| 分辨率 | 目标显示器原生分辨率，最大宽 1920 |
| 帧率 | 30 fps（可切 60） |
| 编码 | H.264 Annex-B，NVENC 低延迟参数 |
| 码率 | 4–8 Mbps，CBR，按发送队列长度降档 |
| GOP | 2 秒；无 B 帧；订阅时和按需产生 IDR |
| 光标 | 不包含（客户端本地绘制） |

> **现状（2026-11-24 核对）**：这张表是计划。实际聚焦流仍是 **MJPEG**（`jpeg-encoder`），分辨率上限 1920、默认 30 fps、质量 65；没有 H.264 / NVENC / CBR / GOP / IDR，也就谈不上「按发送队列长度降档」。客户端发的是 `codec: mjpeg`（`crates/client/src/main.rs` 的 `default_video_sub` 与 `set_stream_options_impl`）。自适应从 2026-11-26 起是「先画质、后分辨率」：拥塞时（帧率掉到目标的 82% 以下，或 RTT 超过本会话最小值的 1.8 倍 + 25ms）先沿 75→65→55→48→42 降 JPEG 质量，到底再沿 3840→2560→1920→1280 降分辨率；链路连续 6 个窗口（约 12 秒）健康才往回升，且不会超过用户手动选的那一档。帧率支持 `0 = 自动`（不设上限，DXGI 只在桌面变化时出帧，实际跟着显示器刷新率走）。`VideoSub.max_bitrate`（kbps）按 `kbps * 1000 / 8 / fps` 折算单帧字节预算选质量档，默认不限制。光标方向与本条**相反**：聚焦流订阅 `cursor: true`，光标由 Agent 画进画面，客户端没有本地绘制，见 5.2 的现状说明。

### 6.3 解码与渲染（Web）

- `VideoDecoder`（WebCodecs），codec `avc1.42E01E`，`avc.format = 'annexb'`
- 解码输出 `VideoFrame`，绘制到 canvas；`requestAnimationFrame` 节流
- 墙上流用 `<img>` / canvas 显示 JPEG，不解码 H.264，CPU 占用极低
- M1 前先做 WebCodecs spike，确认 WebView2 的 H.264 解码路径（实际没有做 spike，直接走了 JPEG 方案）

> **现状（2026-11-24 核对）**：只有第 3 条落地。墙上流与聚焦流都是 JPEG，前端直接用 `<img>` / canvas 显示，**不走 WebCodecs、不解码 H.264**；6.2 的 H.264 / NVENC / CBR 一整套没有实现。音频解码倒是用了 WebCodecs `AudioDecoder`（Opus），但那属于声音链路，与本章无关。

### 6.4 延迟预算（1080p30）

局域网直连：采集 3–8 ms + 编码 3–8 ms + 网络 < 5 ms + 解码 3–5 ms + 渲染 5–10 ms ≈ 20–35 ms，目标是玻璃到玻璃 < 60 ms。

经中转：在上面基础上加两段客户端到服务器的 RTT（约 20–80 ms），目标是 < 150 ms。服务器选在双方都近的城市会明显更好。

> **现状（2026-11-24 核对）**：这些是设计目标，不是实测值。实际链路是 MJPEG 编码 + JPEG 解码（不是 H.264），延迟构成与本节假设不同；真实测量值见 [DELIVERY.md](DELIVERY.md) 的验收记录。

> **2026-11-26 编码/缩放实测**（release，1080p）：JPEG 编码 Q65 约 12.5 ms/帧（`jpeg-encoder` SIMD，4:2:0，已经是纯 Rust 里较快的路径）。缩放在 4K 屏上更关键：原来的纯 Rust 浮点双线性 4K→1080p 要 32–48 ms/帧，直接卡死 30 fps；改用 `fast_image_resize`（AVX2/SSE SIMD）后降到 9–15 ms/帧，SIMD 不可用时自动回退原实现。抓屏与编码分两条线程（单槽邮箱只保留最新帧），所以 4K 屏的采集线程约 10–17 ms/帧，30 fps 稳，60 fps 视机器而定。

## 7. 输入与群控

- **绝对坐标**：客户端把光标在画面中的位置归一化为 `0..1`，Agent 映射到本机目标显示器。不同分辨率、不同缩放比例下，点击落在相同的相对位置。
- **3D 视角**：客户端 Pointer Lock 捕获 `movementX/Y` 作为相对位移发送，Agent 直接注入相对移动；本机显示本地光标，远端不绘光标。
- **事件语义**：
  - 鼠标移动可合并：客户端约 60 Hz 取样，只保留最新位置
  - 按键、点击、滚轮可靠有序：每条连接内按顺序送达
  - 高频事件不做重传；连接恢复后以当前状态重新同步（`releaseAll` 后继续）
- **群控**：客户端向每个选中目标分别发送同一事件（坐标已归一化），各 Agent 独立换算和注入。某台卡住或断线不影响其它设备。
- **安全退出**：客户端注册全局热键 `Ctrl+Alt+Shift`，触发时立即松开所有键鼠并结束控制；Agent 在会话结束时也执行 `release_all`。

> **现状（2026-11-24 核对）**：
> - **3D 视角未实现**。前端没有 `requestPointerLock`，也不读 `movementX/Y`；生产代码里 `InputEvent::Move.rel` 从不发送（只发绝对坐标 `abs`）。Agent 的 `input.rs` 支持 `rel` 分支，但这条路径没有调用方。
> - **全局热键未实现**。全仓没有 `RegisterHotKey`，也没有引入 Tauri 的 global-shortcut 插件。现有的释放方式是前端在捕获键盘时连按两次 Esc 释放，以及离开页面/会话结束时由前端与 Agent 各自 `release_all`。
> - 「鼠标移动可合并」「按键有序」「`releaseAll`」这三条属实。

## 8. 文件分发

### 8.1 流程

1. 发送方先发清单（相对路径、大小、SHA-256）
2. 接收方校验空间和路径后 `accept`
3. 按固定大小分块传输，每块带序号（计划 1 MiB）
4. 每个文件完成后校验 SHA-256，回报结果

> **现状（2026-11-24 核对）**：第 3 条分块大小是 **256 KiB**（`crates/proto/src/messages.rs` 的 `FILE_CHUNK_BYTES = 256 * 1024`），不是 1 MiB；分块走 `file.chunk` 的 JSON + base64，8 MiB 流控窗口，`file.progress` 兼作确认。清单校验与 SHA-256 回报属实。

### 8.2 落盘与并发

- 目标目录：`<接收根>\<主控设备名>\<时间戳>\`
- 保留子目录结构；同名文件自动加 `(1)`、`(2)`
- 每台设备最多 2 个文件并行，进度独立
- 传输期间连接断开则本次任务失败，不做断点续传（v2）

> **现状（2026-11-24 核对）**：
> - 落盘目录是 `<接收根>\...`，即 `Downloads\ScreenWall`，**没有**「主控设备名 / 时间戳」两层；上传保留客户端传来的相对路径（选择文件夹上传时），同名文件加 `(1)` 属实。
> - 并发方向与本条**相反**：上传一次只跑 **1 个批次**；下载是队列 + **最多 3 个并发**（`crates/client/src/files.rs` 的 `MAX_CONCURRENT_DOWNLOADS = 3`）。
> - **断点续传已实现**（上传与下载都有），不是留到 v2：上传按整文件 SHA-256 匹配 `Downloads\ScreenWall\.screenwall-parts\<sha256>.part`，`file.accept` 带回 `resume` 偏移；下载按「设备 + 远程路径 + 大小」匹配 `.screenwall-downloads\<key>.part`。分片超过 7 天清理。

### 8.3 安全检查

- 拒绝绝对路径、含 `..` 的路径、盘符、NTFS 保留名（CON、PRN、AUX、NUL、COM1–9、LPT1–9）
- 规范化后必须仍位于接收根目录内
- 限制文件数量、单文件大小、总大小（可配置），默认：单文件 20 GiB、总量 200 GiB

> **现状（2026-11-24 核对）**：
> - `safe_join`（`crates/agent/src/files.rs`）只拒绝绝对路径、`..` 以及非 `Normal` 的路径段；**不拒绝 NTFS 保留名**（CON/PRN/AUX/NUL/COM1–9/LPT1–9）。
> - 「规范化后仍位于接收根目录内」由逐段拼接到根目录实现，结果上成立。
> - **没有**文件数量、单文件大小、总大小限制；`codes::DISK_FULL`（4106）只定义了常量，没有生产代码在接收前检查磁盘空间。

## 9. 远程开机

前提条件（写入使用说明）：

- BIOS/UEFI 开启 Wake-on-LAN
- 使用有线网卡（无线 WoL 不稳定）
- Windows 关闭「快速启动」
- 网卡属性允许「魔术包唤醒」

流程：

1. **同网**：主控端直接向目标网段广播 magic packet（UDP 9）
2. **异地**：请求一台同网且在线的 helper Agent 代发（设备设置里指定）
3. 唤醒后轮询 Agent 上线，超时 120 秒提示检查 BIOS / 网络设置

helper 从同一账号下、与目标处于同一局域网的在线设备中选择；用户也可以手动指定。

> **现状（2026-11-24 核对）**：只有第 1 步实现——主控端本机调用 `wake_on_lan` 命令，由 `screenwall_agent::wol::send_magic_packet` 向指定广播地址发 magic packet（`crates/client/src/main.rs`）。**第 2 步「异地由同网 helper 代发」未实现**：没有 helper 选择逻辑，客户端也从不发 `wol.request`（Agent 侧虽有处理函数，但无调用方）。第 3 步「唤醒后轮询上线、120 秒超时提示」也没有专门的实现，只能靠设备列表自行刷新观察。详见 5.6 的现状说明。

## 10. 安全模型

| 项 | 做法 |
|---|---|
| 链路加密 | 两端到服务器均为 TLS 1.3（WSS）；局域网直连用自签证书 + SHA-256 指纹固定 |
| 账号 | 用户名 + 密码，Argon2id 哈希；登录按 IP 和账号限速 |
| 令牌 | 32 字节随机会话令牌，数据库只存哈希，默认 30 天过期，可在服务器吊销 |
| 授权 | 同账号设备默认可控；被控端可随时关闭「允许被控」；后续加单次确认 |
| 暴露面 | 客户端只主动出站，不开放入站端口（局域网直连开启时例外：监听 47900，见 [direct.md](direct.md)）；服务器只暴露 443 |
| 服务器可见性 | 中转层能看到转发明文，服务器自持、受信；后续用端到端密钥消除 |
| 文件 | 路径穿越防护；只写入指定根目录 |
| 日志 | 密码、令牌、帧数据不落日志 |

风险说明：服务器被攻破等于账号被攻破。服务器只保存账号哈希、令牌哈希和设备元数据，不保存画面；部署时要求 HTTPS、强密码和最小开放端口。

## 11. 性能目标

| 场景 | 指标 | 目标 |
|---|---|---|
| 墙上 5 路 480p@2fps | 主控解码 CPU | < 10% |
| 聚焦 1080p30 | 局域网玻璃到玻璃延迟 | < 60 ms |
| 聚焦 1080p30 | 经中转服务器（同城） | < 150 ms |
| 键鼠响应 | 按下到远端生效 | < 100 ms |
| 文件传输 | 局域网吞吐 | > 50 MB/s |
| Agent 常驻 | 内存 / CPU（空闲） | < 150 MB / < 1% |
| 启动 | 双击到可被控 | < 3 s |

## 12. 目录结构与构建

```
D:\screenwall\
├─ Cargo.toml              # workspace
├─ crates\
│  ├─ proto\               # 消息类型、二进制帧编解码、错误码
│  ├─ agent\               # 被控端能力：采集、输入、文件、WoL
│  ├─ server\              # 账号、设备目录、在线状态、中转
│  └─ client\              # Tauri 主程序：登录、设备列表、转发、界面
├─ ui\                     # React + Vite + TypeScript
│  ├─ src\
│  └─ package.json
├─ docs\
│  ├─ design.md
│  ├─ protocol.md
│  ├─ server.md
│  └─ m0.md
├─ scripts\                # 构建、开发启动脚本
└─ .gitignore
```

构建：

```powershell
cargo tauri dev          # 开发
cargo build --release    # 发布
```

前端构建由 Tauri 调用 `npm run build`；FFmpeg DLL 在构建脚本中复制到输出目录。

> **现状（2026-11-24 核对）**：目录结构大体属实（另有 `crates/` 下的 `proto` / `agent` / `server` / `client`、`docs/`、`scripts/`、`ui/`）。构建命令实际是 `scripts\tauri-build.bat build --bundles nsis` 或 `cargo tauri`；**没有 FFmpeg DLL 需要复制**，整个应用不依赖 FFmpeg。

## 13. 里程碑

| 阶段 | 内容 | 产出 / 验收 | 预估 |
|---|---|---|---|
| M0 | workspace、proto、TLS/WSS 握手、显示器枚举、WebCodecs spike（实际没做 spike，见下方现状） | 协议与握手测试通过；Agent 能启动 | 已完成 |
| M1 | 账号服务器、注册登录、设备目录、在线状态、会话中转 | 两台电脑登录同账号，互相看到在线并打通一条会话 | 5–7 天 |
| M2 | WGC 采集 → JPEG → 墙上网格 | 5 台同屏 2–5 fps，延迟 < 500 ms | 3–5 天 |
| M3 | 聚焦 H.264 流、键鼠注入、全屏、3D 视角 | 经中转操作延迟 < 150 ms，30 fps | 5–7 天 |
| M4 | 群控：多选、归一化坐标、事件语义、断连松键 | 3 台同步操作结果一致 | 3–4 天 |
| M5 | 文件：清单、分块、目录、并发、冲突、校验 | 1 GiB 目录发 3 台无错 | 2–3 天 |
| M6 | 远程开机、直连优化、自适应码率、安装包、使用文档 | 异地：关机 → 唤醒 → 控制；可长期日常使用 | 持续 |

合计约 4–6 周全职，业余 6–10 周。M1 是新的关键路径，先把它打通，再往上叠画面与控制。

> **现状（2026-11-24 核对）**：M1–M6 的核心能力都已落地（M0 当时只有 workspace/握手，真正画面从 M2 开始），但「内容」一列与实现有出入：M2/M3 没有 WGC 与 H.264（DXGI/GDI + MJPEG，见第 4、6 章）；M3 的 3D 视角未做；M6 的「直连优化」落地为局域网直连 TCP（不是 UDP 打洞），远程开机的异地 helper 未做。逐项验收见 [DELIVERY.md](DELIVERY.md)。

## 14. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 显卡硬编兼容性 | 编码失败或花屏 | 启动自检 + 四级降级，最后 libx264 |
| WebCodecs H.264 在 WebView2 不可用 | M2 返工 | M0 阶段 spike 验证，必要时退到本机回环传 MJPEG |
| WGC 对全屏独占游戏黑屏 | 游戏场景不可用 | v1 记录限制，v2 加 DXGI Desktop Duplication |
| 自签证书在浏览器侧不可用 | 连接失败 | 网络放在 Rust 侧，证书固定也在 Rust 侧 |
| `SendInput` 被 UIPI / 反作弊拦截 | 部分窗口控不了 | 提升权限；反作弊场景明确不支持 |
| 中转服务器带宽不足 | 画面卡顿或码率被迫降低 | 墙上流限帧率与分辨率；自适应码率；v2 加 P2P 直连 |
| 服务器故障 / 欠费 | 登录与设备列表不可用 | 已配对设备保留局域网直连；数据库定期备份，可迁移服务器 |
| 多路采集编码争用 CPU/GPU | 帧率下降 | 墙上流限 `maxWidth` 和帧率；聚焦时才开高清 |
| FFmpeg 许可 | 分发受限 | 自用不涉及；若要分发，改用 LGPL 组件避免 GPL 的 x264 |

> **现状（2026-11-24 核对）**：这张风险表基于计划选型。实际没有 FFmpeg / 硬件编码，所以「显卡硬编兼容性」「FFmpeg 许可」「WebCodecs 不可用」三条不再适用；WGC 黑屏那条也不适用（直接用 DXGI + GDI 兜底）。仍然成立的是：UIPI 拦截 `SendInput`、服务器带宽与故障、多路采集争用 CPU。`SendInput` 被反作弊拦截的情况没有实测。

## 15. 待定问题

1. 是否开源？影响依赖许可和文档详略。
2. 主控端是否需要 macOS（Tauri 可移植，但网络与 UI 需回归）。
3. 多显示器：v1 只做主显示器，是否需要按屏选择。
4. 剪贴板同步、音频、画面录制是否进入 v2。
5. 直连（UDP 打洞）排在 M6 还是更早，取决于中转服务器的实际带宽和延迟。
6. 服务器最终放在哪里：轻量云 VPS，还是家中常开电脑 + 公网 IP / 端口映射。
7. 是否开放注册？自用版建议关闭公开注册，账号由服务器管理命令创建。
8. 手机 / 浏览器只读墙（协议打通后顺带实现）。
