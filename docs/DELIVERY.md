# ScreenWall 交付计划

> 目标：单安装包、同账号互控、自建账号与 WSS 中转的完整远程控制产品。
> 本文件是「完整交付」的清单与验收口径，按阶段推进，每阶段结束都能跑起来。

## 原则

- 不保留假 UI：按钮要么真的生效，要么删掉。
- 每一步都有可观测证据（日志、截图、脚本），不靠"应该没问题"。
- 协议先行：新增能力先落到 `crates/proto`，再实现两端。

---

## P0 —— 画面链路（已完成）

| 事项 | 结果 |
|---|---|
| DXGI 采集 + GDI 回退 | 单帧 blit ≈ 4ms |
| JPEG 编码（jpeg-encoder，BGRA 直编） | 1080p release ≈ 16ms |
| 采集/编码线程流水线 + 取最新帧丢旧帧 | 60fps 档 44 → **58.5 fps**（release）；30fps 档 30.0 |
| 隧道 Ping/Pong | 「延迟」显示真实往返（1–12ms） |
| 自适应画质 | 按实测帧率在 Q75→Q42 阶梯滑动；手动选画质即退出自动 |
| 闲时补帧 | 桌面静止每 500ms 补发缓存帧，帧率/心跳不归零 |

验收：`RUST_LOG=screenwall_agent=debug` 下「采集统计」fps 与目标一致；控制页显示实测延迟。

---

## P1 —— 安全与可信（已完成）

1. 被控端授权：**已完成**。`AgentState.approval` 回调 → 客户端弹窗「允许控制 / 拒绝」，30 秒未确认自动拒绝，答复通道断开按拒绝处理；「设置 → 安全 → 允许被控」可整体关闭（运行中立即生效，等待中的请求一并拒绝）。**同一账号的普通会话免确认**：服务器在 `open` 里带 `sameAccount: true`，客户端直接放行，不弹窗；局域网直连和连接码会话不带这个标记，仍然逐次确认。
2. 服务端 TLS/WSS：**服务端已完成**（`tls.rs`、证书配置、`wss://` 全流程测试）。**自定义 CA 已完成（2026-11-25）**：设置 → 连接 → 中转连接里新增「自定义 CA（可选）」，填 PEM/DER 文件或证书目录，保存前会真的解析一遍（路径写错当场报错）。语义同 `curl --cacert` —— 配置了**只信任这个 CA**，留空恢复系统默认信任库；客户端主控连接与被控端的 `cloud` 连接都走 `screenwall_agent::ca`（`load_roots` / `client_config` / `connector_from_path`），因此自签名 CA 或企业内网 CA 现在能连。改完需要重新连接服务器才生效。
3. 账号安全：**已完成**。登录失败限流（5 次锁 60 秒）、设备令牌吊销接口、审计表；密码哈希参数见 `docs/server-security.md`。
4. 设置页收口：**已完成**。删掉会话锁屏、双重验证、新设备提醒、文档/反馈等假控件；剩下的是真实现：「测试连接」请求 `/healthz` 并显示往返毫秒、「界面缩放」走 `WebviewWindow::set_zoom` 并持久化、「开机自启动」写 `HKCU\...\Run`、「动画效果」写 `data-motion` 并持久化、「允许被控」即时生效；托盘与日志相关项也已接真实命令（见 P3 第 14、15 项）。**主题切换**当时删掉的是假控件，后来界面重做（见下方「界面重做」一节）时以真实现的方式加回来了：浅色/深色双主题，存 `localStorage`，无存储偏好时跟随系统。
5. 会话审计：**已完成**。服务端 `audits` 表 + `GET /api/v1/audits?limit=&offset=&deviceId=`（返回 `{audits,total}`，按 `started_at DESC, id DESC` 稳定分页，`deviceId` 匹配主控端或被控端）；客户端命令 `list_audits`，侧边栏「管理 → 审计」页显示当前页会话数/进行中/累计时长/累计流量/匹配总数，支持按设备下拉过滤、关键词搜索、全部/进行中/已结束筛选、上一页/下一页；进行中的会话实时计算时长。审计只含会话元数据，不含画面、键鼠与文件内容。**直连审计已完成（2026-11-25）**：新增 `mode` 列（`relay` / `direct`，旧库打开时自动 `ALTER TABLE` 补列，历史记录默认 `relay`）与 `POST /api/v1/audits/direct`（Bearer 鉴权，校验设备 ID 非空/≤128、`endedAt >= startedAt`、时长上限 30 天）；主控端在直连会话结束时上报一条完整记录（`session_id = 0`、被控端写 `direct:<ip>:<port>`、流量暂按 0），审计页对直连显示「直连」标记与「直连 192.168.1.20」而不是原始合成 ID。

验收：陌生设备请求控制会被本机拦住（`crates/agent/tests/cloud.rs::control_requires_host_approval` 覆盖拒绝/允许两条链路）；`wss://` 下全流程可用；设置页不存在无效控件。

---

## P2 —— 核心功能兑现

6. 文件传输：**已完成（含断点续传与 SHA256）**。
   - 上传：拖拽 / 选择文件 / 选择文件夹，前端用 `js-sha256` 按 4 MiB 切片流式计算 SHA256，再按 256 KiB 分块 base64 走 `file.chunk`，8 MiB 流控窗口，`file.progress` 兼作确认；被控端落盘到 `%USERPROFILE%\Downloads\ScreenWall`，同名文件加 `(1)` 后缀，收满字节数且 SHA256 匹配才改名。
   - 断点续传：上传中断（取消、断线、关闭页面）后被控端保留 `.screenwall-parts\<sha256>.part`，下次上传同一文件时 `file.accept` 带回 `resume` 偏移，主控端从断点继续，界面提示「已从断点继续」。分片超过 7 天自动清理。
   - 下载：浏览被控端目录（`file.list`），点文件下载到本机 `Downloads\ScreenWall`，先 `file.offer` → 主控端 `file.accept` 再推数据；被控端边发边算 SHA256，主控端落盘后再算一遍，不一致就删文件并报错。
   - 空文件、路径穿越（`..`、绝对路径、盘符）都有处理；无哈希的旧客户端仍可按字节数落盘。
   - 页面：`文件传输` 页已全部接真实命令，无假数据；从屏幕墙卡片点「文件」或控制页点「文件」进入；没有会话时可直接「连接并传输」（被控端照常弹授权）。
   - 下载断点续传：`file.accept.offset` 由主控端带回本地分片已有字节数，被控端校验（超过文件大小按 0）后 seek 发送并补算前缀哈希；分片在 `Downloads\ScreenWall\.screenwall-downloads\<key>.part`，`key = SHA256(设备 ID + 规范化路径 + 大小)`，文件被替换（大小变化）不会错续；超过 7 天未续传的分片在下次下载时清理。旧 Agent 忽略 `offset` 时主控端收到首块即清空前缀、从 0 重收，不会卡死。
   - 整目录下载：目录行的「下载整个文件夹」由客户端递归 `file.list` 展开（BFS，上限 20000 个文件 / 64 层），保留相对目录层级落到 `Downloads\ScreenWall\<目录名>\...`，空目录也会建出来；子目录读取失败只跳过并告警，根目录失败才整组报错。
   - 多文件并发：下载改为队列 + 最多 3 个并发（`MAX_CONCURRENT_DOWNLOADS`），排队任务可取消，每个文件仍是独立 tid、独立进度、独立断点续传与 SHA256 校验。上传侧沿用原有「一次一个批次」的限制。
7. 群控：**已完成**。
   - 每台选中设备各开一条普通 `control` 会话（同账号免确认；直连/连接码仍要授权，30 秒超时规则不变）；卡片从对应会话订一路 `wall` 低帧率视频做预览（画质档：流畅 1 FPS / 480px、均衡 2 FPS / 640px、高清 3 FPS / 960px）。
   - 键鼠广播：同一批事件原样发到每条就绪会话，坐标保持 0..1 归一化，各设备按自己的分辨率落点；键盘以扫描码广播。关掉「广播输入」后可在单张卡片上定向操作。
   - 改画质只重发 `video.sub`（Agent 换流），不重开会话；移除设备、离开页面或主控断线会逐条 `close`。
   - 双机扇出由 `crates/server/tests/group.rs` 覆盖（同账号两台设备、两条会话、广播各达其设备且不串台）；本机只有一台物理设备，未做双机真机实测。
8. 屏幕墙实时缩略图：**已完成**（1 FPS / 320px 宽）。
   - 主控端为每台在线设备开一条 `mode: "wall"` 只读会话；被控端免逐次授权、忽略输入/文件/剪贴板，只接受一路 `video.sub`（fps 夹到 ≤2、宽夹到 ≤480）。
   - 帧以 `wall-frame` 事件推到前端，卡片直接显示实时画面；离开屏幕墙页、设备离线或退出登录即关闭会话；断流 15 秒自动重开，失败 8 秒退避。
   - 缩略图会话不写审计，避免每次进页面刷出 N 条记录。
9. 多显示器切换：**已完成**。
   - `display.list` 带每块屏在虚拟桌面里的坐标；`video.sub.display` 选 0 起的显示器，`4294967295` 表示整块虚拟桌面拼接。
   - Agent 按显示器裁剪采集：选中某块屏时优先用 DXGI 按输出坐标匹配（GPU 速度快）；多屏拼接或 DXGI 不可用时 GDI `StretchBlt` 直接裁剪缩放。
   - 输入坐标跟随选中显示器：归一化坐标先落在该屏上，再换算成虚拟桌面的绝对坐标。
   - 控制页「画面」面板新增显示器组，切换只重发 `video.sub`（会退出自动画质），`video.sub.ok` 更新实际分辨率。
   - 本机只有一块屏，双屏真机未实测；跨屏坐标换算有单测（`input::tests`）。
10. 剪贴板与系统声音：**文本双向同步、系统声音传输已完成**。
    - 剪贴板：两端各自轮询 `GetClipboardSequenceNumber`，400ms 一次，自己刚写入的内容不会再回传（文本 `LAST_WRITE`、图片 `image_sequence` 各自抑制），超过 64K 字符不同步；被控端受「允许被控 / 剪贴板同步」开关控制，主控端受控制页开关控制。**图片剪贴板已完成（2026-11-25）**：`clipboard.rs` 支持 CF_DIB ↔ PNG（24/32bpp `BI_RGB`，支持 top-down/bottom-up），单张 PNG ≤ 8 MiB、原始 DIB ≤ 32 MiB；`ClipboardSync.image` 走 base64(PNG)。**有文本时优先发文本**（Excel/Word 复制会同时放位图，优先发图会把表格变成一张图），只有纯图片时才发图。能力 `clipboard.image` 只进 `CURRENT`，对端没声明时只发文本；老版本收到带 `image` 的消息会忽略该字段，仍能收文本。文件剪贴板（复制粘贴文件）仍未做。
    - 声音：WASAPI 采集，默认采「播放到默认扬声器」的回环内容（不需要虚拟声卡），控制页侧栏可切换成被控端**麦克风**（`audio.sub.source`）。下混/重采样成 48 kHz 立体声后走 **Opus**（20 ms 一包，默认 64 kbps，约为原始 PCM 的 1/24），放进 `type=2`、`codec=4` 的二进制帧；`audio.sub` / `audio.sub.ok` / `audio.stats` / `audio.unsub` 显式订阅，主控端拿到实际编码/采样率/声道后才开始播放。WebView2 用 WebCodecs `AudioDecoder` 解码，不支持时自动改订 `pcm16`（交错小端 i16，`codec=3`）回退；Agent 编码器初始化失败也会自动回退并在 `audio.sub.ok.actual.codec` 中告知。主控端每 2 秒上报播放缓冲深度，Agent 每 ≥3 秒按缓冲调整 Opus 码率（>250 ms 降 1/4，最低 24 kbps；<80 ms 升 1/4，最高 96 kbps）。控制页「声音」按钮默认关闭，离开画面页或断开会话自动退订；播放端用 Web Audio 顺序排程 + 300 ms 抖动上限，落后跳过、积压重置。切换音源会重发 `audio.sub`（Agent 直接替换采集，不用先退订）。
    - 音量联动：`audio.volume` / `audio.volume.get` / `audio.volume.ok`，Agent 用 `IAudioEndpointVolume` 读写默认输出设备的主音量与静音。控制页侧栏有音量滑块 + 静音按钮，拖动时本地即时反馈、停下约 180 ms 才下发，避免高频写系统音量；进入会话时先读一次当前值。没有输出设备时返回 `4110`，不影响画面。
    - **回声消除（AEC）已完成（2026-11-24）**：麦克风作为音源时，Agent 额外开一路系统回环当远端参考信号，做纯 Rust 的 NLMS 自适应回声消除（`crates/agent/src/aec.rs`，`#![deny(unsafe_code)]`）。延迟估计用 8 kHz 抽取后的归一化互相关粗搜 + 全速率局部细化，延迟对齐后自适应滤波器覆盖约 100 ms；双讲检测（ERLE > 6 dB 且麦克风能量 > 4× 估计回声）冻结自适应但继续减除；残留回声按功率域增益抑制并钳到 −20 dB。回环参考打不开时整体旁路、麦克风照常直通；`SCREENWALL_AEC=0`（或 `false` / `off` / `no`）可整体关闭。日志会打「AEC 已对齐扬声器到麦克风的延迟」（延迟毫秒/采样点/滤波器长度）与约 10 秒一条的 ERLE/双讲比例（`debug` 级）。
11. 真实带宽/丢包指标：**已完成（2026-11-24 核对）**。控制页显示实测 FPS、码率、RTT（来自真实帧与 ping），以及按 `FrameHeader.seq` 缺口推算的**丢帧率**与音频丢包数（`crates/client/src/quality.rs`，质量浮层「连接质量」）。
    - 码率上限（2026-11-25 已实现）：`VideoSub.max_bitrate`（kbps）不再是被忽略的字段。Agent 在 `CaptureOptions::frame_budget()` 里按 `kbps * 1000 / 8 / fps` 折算单帧字节预算，编码时选第一个不超预算的质量档；三档都超时保留最低质量帧而不是丢帧。低于 50 kbps 或高于 200 Mbps 按不限制处理。客户端从 `client.json` 的 `maxBitrate` 读，默认 `null`（不限制）。屏幕墙缩略流忽略此字段。

验收：本机自控实测 4 MiB 文件上传中断后从 1 MiB 处续传、SHA256 一致；声明错误哈希时被控端拒绝落盘；下载副本 SHA256 一致且无残留 `.part`。剪贴板抑制逻辑有单测覆盖，跨机验证需两台机器。
屏幕墙缩略图（1 FPS / 320px）与群控（一次广播到 N 台，每台一条独立会话）均已可用。

---

## P2.5 —— 假 UI 收口（已完成，2026-11-24）

- 设备管理接服务器真接口：重命名走 `PATCH /api/v1/devices/{id}`、允许控制开关走同一接口的 `controllable`、移除走 `DELETE`（同时吊销目标设备令牌）；本机设备不允许删除，界面也不显示「移除」。重命名/开关刷新后仍保持，服务器接口有集成测试（`tunnel.rs::device_manage_rename_toggle_and_delete`）。
- 「刷新设备列表」改为真正 `GET /api/v1/devices`，不再是 700ms 假动画。
- 控制页「屏幕截图」把当前帧保存为 JPEG 到 `Downloads\ScreenWall\ScreenShot-<时间戳>.jpg`，返回真实路径。
- 「重启 / 关机」新增 `power` / `power.ok` 消息，Agent 执行 `shutdown.exe /r|/s /t 3`；主控端必须二次确认（显示目标设备名与后果）。执行过程未真机触发（会重启本机），命令构造有单测。
- 删掉无法真实实现的假按钮：
  - 「远程开机 / 唤醒设备」：原先是假按钮，现已接真实 WOL（见 P4）。
- `Ctrl+Alt+Del`：新增 `sas` / `sas.ok` 与 `input.sas` 能力。Agent 优先调用 `sas.dll!SendSAS`，
  并按 `SoftwareSASGeneration` 策略 / SYSTEM 身份判断是否可能成功；不满足时降级为普通
  `Ctrl+Alt+Del` 按键，回传 `method: "keys"` 和原因，界面明确提示「无法进入安全桌面」。

---

## P3 —— 交付形态

12. 单安装包：**已完成**。`bundle.active = true`，目标 NSIS，`installMode: perMachine`（安装需要管理员，安装/卸载时自动维护 `ScreenWall Direct` 防火墙规则），多尺寸 `.ico` 图标，标识 `dev.screenwall.client`。
    构建命令：`scripts\tauri-build.bat build --bundles nsis`（自动进 MSVC 环境）；产物 `target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe`（4.50 MiB，SHA256 `6C287894DFB38ABC5B68A455B354F06E79E6D53AF0197EFEDD79464710AA5D43`，2026-11-25 01:45 含直连自动重连、屏幕墙拖拽分发、码率上限、远程画面光标、能力协商回交集、局域网来源校验、直连认证失败限流、局域网直连、Opus 声音与麦克风 AEC、被控端音量联动、远程开机 WOL、整目录下载与 3 路并发下载、上传/下载断点续传、审计页、托盘看门狗、诊断日志、自托管自动更新、真实运行指标）。
    **最新重打（2026-11-25 12:42）**：7.69 MiB（8,064,841 B），SHA256 `44D93FD85717D25DB5B883E68AB21D5799DBB6ADCE43605855BD5B748B67C39F`，新增异地开机代发（WOL 经在线设备转发）与代发授权提示。
    **没有商业代码签名证书**（安装时 SmartScreen 会提示）；自签名方案与 CI Secrets 注入见 [签名文档](signing.md)。
13. 自动更新：**已完成**（自托管，不依赖第三方更新服务）。
    - 服务器：`GET /api/v1/version`（免登录）返回 `{version,url,sha256,notes,publishedAt}`，数据来自 `server.toml` 的 `[update]` 段；SHA256 首次请求计算后缓存。`GET /downloads/{file}` 只放行 `[update].file` 白名单，流式返回，`..`、`%2F`、`%00` 等穿越尝试一律 404。未配置时 404 `{"msg":"未配置更新"}`。
    - 客户端：`crates/client/src/update.rs` 做语义化版本比较（`1.2.10 > 1.2.9`）、下载到 `%TEMP%\ScreenWall\updates`、边下边算 SHA256（不匹配删除并报错）、通过 `update-progress` 事件报进度；`install_update` 用 `ShellExecuteW(runas)` 启动 NSIS `/S` 后退出程序，由安装包完成升级（perMachine 安装器需要管理员，升级时会弹一次 UAC）。
    - 前端：设置 → 关于 → 「软件更新」显示当前版本、「检查更新」、新版本号与说明、「下载并安装」带进度条；浏览器预览显示「已是最新版本」，下载按钮会明确提示预览模式不支持，不是假控件。
    - 发布流程见 `docs/server.md` 1.9 节：把新安装包放进 `[update].dir`、改 `[update].version`/`file`/`notes`、重启服务器（SHA256 缓存按进程计算）。
14. 托盘常驻与看门狗：**已完成**。`crates/client/src/tray.rs` 建系统托盘（打开主界面 / 允许远程控制 / 退出），关闭窗口默认缩到托盘（设置页「后台与诊断 → 关闭窗口时缩到托盘」可关掉）；连接看门狗每 30 秒检查一次，主控/被控连续约 2 分钟未连上就重启两条会话任务（不动用户正在用的群控/文件会话）。开机自启仍写 `HKCU\...\Run`。
15. 崩溃与诊断日志：**已完成**。`crates/client/src/logging.rs` 把 `tracing` 写成滚动文件 `%APPDATA%\ScreenWall\logs\screenwall.log`（单文件 2 MiB，保留 3 个历史文件），`panic` hook 把崩溃位置与原因写进同一份日志；设置页可打开日志目录、导出诊断包（日志 + 脱敏后的 `client.json`，`token` 替换为 `<已脱敏>` + `system.json`），并显示日志占用。

验收：干净机器上安装→登录→被控→升级 全流程无需命令行。

---

## P4 —— 工程化

16. `cargo test` 覆盖坐标映射、协议编解码、能力协商、音频消息（含音源与音量）编解码与 Opus 编解码/下混/重采样、AEC（延迟估计/自适应/双讲/残留抑制）、运行指标采样、连接质量统计、WOL MAC/广播地址校验与 magic packet 构造、缩放、授权、文件路径校验、上传与下载断点续传、SHA256 校验、目录展开与相对路径落盘、审计分页/设备过滤/稳定排序、更新接口与路径穿越、服务端指标与限流、日志滚动与令牌脱敏、剪贴板抑制、群控多设备扇出、跨屏坐标换算；`cargo test --workspace` 共 **271 个测试通过**（2026-11-25 实测；含局域网直连集成用例、限流与来源校验单测、`Welcome.caps` 交集断言、远程光标合成、码率预算、直连重连与屏幕墙拖拽的单测；本轮新增 31 个，含异地开机的 `wol.request → wol.ok` 与 10 秒限流集成用例：UDP 自动发现的 JSON 往返/端到端/去重/垃圾报文、剪贴板图片的 PNG↔DIB 往返与 24bpp 通道顺序/压缩格式拒绝/超大图拒绝/`poll` 回传抑制、自定义 CA 的 PEM/DER/目录/错误路径、`ClipboardSync.image` 编解码与老格式兼容、`clipboard.image` 不进基线、直连审计写入与 `mode` 迁移、直连审计接口的鉴权/时间倒挂/空 ID 校验）。
    本地 CI 脚本 `scripts\ci.ps1` 与 GitHub Actions `.github/workflows/ci.yml` 四个 job（fmt / build-and-test / clippy / bundle）全部阻塞且实测全绿：`cargo fmt --all -- --check` 0 diff、`cargo clippy --workspace --all-targets -- -D warnings` 0 警告、`npx tsc --noEmit` 无输出、`npm run build` 通过。详见 [CI 文档](ci.md)。
17. **服务端可观测性已完成（2026-11-24）**：`crates/server/src/metrics.rs` 在真实处理路径上累加原子计数，`GET /api/v1/metrics`（需登录）返回一份完整快照：
    - `sessions`：`active`（内存表现算）、`peak`、`total`；
    - `bytes`：`toDevices` / `toViewers`（中转负载字节数，双向分列）；
    - `errors`：`total`、`rate5m`（30 个 10 秒桶的滑动窗口，输出最近 5 分钟的平均每分钟错误数）、`byCode`（按协议错误码分组）；
    - `devices`：`online`（内存表现算）、`accounts`（数据库现算）；
    - `handshakes`：`ok` / `failed`；`backpressure.droppedFrames`；`uptimeSecs`。

    旧 `/metrics`（精简 4 字段）保留以兼容已有监控脚本。单账号设备上限与限流写在 `server.toml` 的 `[limits]` 段（9 项，均可写 0 关闭）：`max_devices_per_account`（默认 20）、登录/注册/`/ws/view` 建连/会话建立四条令牌桶限流，超限分别返回 HTTP 429 或协议码 4003/4006/4205。实现与测试见 `crates/server/tests/observability.rs`（8 个集成测试：真实隧道流量反映到 `bytes`、握手失败与错误码计数、设备上限拒绝新设备但允许重登、上限 0 表示不限、注册限流 429 后恢复、建连限流、会话建立限流回 `open.err`、登录限流）。
18. **断线重连体验、错误文案统一、焦点与键盘可达性已完成（2026-11-24）**：
    - 重连：控制会话断开后按 `1s / 2s / 4s / 8s / 8s` 五轮退避自动重连（`ReconnectGuard` + `control_wanted` 标记，用户主动停止、离开页面、退出登录、换账号都会取消；主控 WebSocket 整体断开时计划挂起到服务器恢复再执行）。每次尝试推 `control-reconnecting`，成功推 `control-reconnected`，五轮全失败推 `control-reconnect-failed`；控制页顶部内联横幅（`.ctrl-banner`，`role="status"` / `role="alert"`）显示「正在重连（第 N/5 次）」「已恢复」「重连失败」。
    - 错误文案：`ui/src/errors.ts` 把 18 个协议码（4001–4006 共 6 个、4100–4111 共 12 个）、6 个服务器中转码（4200–4205）、HTTP 错误名与 401/403/404/409/429/5xx 统一映射成一句中文说明，并剥掉 `invoke failed:` / `Error:` 等前缀噪音；Rust 侧 `crates/client/src/errors.rs` 对 `anyhow` 错误链做同样处理。
    - 连接质量：`crates/client/src/quality.rs` 用既有 `FrameHeader.seq` / `ts_us` / `stream_id` 统计帧数、丢帧、实测 FPS、kbps、最大帧间隔、音频包数与 RTT，`get_connection_quality` 命令 + 控制页质量浮层展示。
    - 可达性：全局 `:focus-visible` 焦点环改用 `var(--accent-strong)` 并覆盖全部可聚焦控件；共用 `Modal` 做焦点陷阱（打开聚焦首个可操作元素、Tab/Shift+Tab 循环、Esc 关闭、关闭后焦点回到触发按钮）；图标按钮补 `aria-label`；控制页捕获键盘时 Tab/方向键发远端，连按两次 Esc 释放；列表行补 `role` / `tabIndex` / 回车与空格响应。
19. **协议能力协商（capabilities）已完成（2026-11-24）**：`hello` / `welcome` 增加可选 `caps: string[]`，`crates/proto/src/caps.rs` 定义 15 个能力标识与「v1 基线」；字段缺失按基线处理，未知标识忽略，新增能力不升 `proto`。`Capabilities::with_peer` 负责算双方交集；它**不是**判断可选功能能否启用的唯一入口（见下方现状）。新增单测 9 个（能力集合去重排序、序列化、未知项、缺字段回退、交集、双侧判定、基线是 CURRENT 子集、Display；此后又补了 `welcome_contract_is_intersection_not_current`，`caps.rs` 现有 10 个测试，以代码为准），另加 `hello_without_caps_parses_as_legacy` / `welcome_without_caps_parses_as_legacy` / `legacy_hello_without_caps_is_accepted`（真起 TLS 服务、用不带 `caps` 的 hello 握手）三个互通用例。局域网直连路径的 `wol.request` 也从「M0 尚未实现」改成真的发 magic packet（Agent 侧处理已接通；**主控端仍不发这条消息**，界面的「唤醒」走客户端本机 `wake_on_lan`，见 P4「已知约束」与 design.md 5.6 的现状说明）。
    - **现状（截至本次核对）**：Agent 的三个入口——中转会话（`crates/agent/src/cloud.rs`）、独立 Agent 服务的 `session::handle`（`crates/agent/src/session.rs`）、局域网直连（`crates/agent/src/direct.rs`）——都在握手时调 `Capabilities::with_peer` 算交集、打 caps 日志，并把 `Welcome.caps` 回成这份交集。主控端收到 `welcome` 后同样算一遍交集（`crates/client/src/main.rs` 的 `Message::Welcome` 分支、`crates/client/src/direct.rs`），通过 `control-caps` 事件与 `get_control_info` 交给前端。
    - 前端据此 gate 的功能目前只有一项：控制页用 `caps.includes("audio.volume")` 判断被控端是否支持音量联动（`ui/src/pages/ControlPage.tsx` 的 `capsBlockVolume`），不支持就置灰滑块。其余可选功能仍按「试了再说」处理（收到 `UNKNOWN_MESSAGE` 再降级）。
    - `Capabilities::supports()` 在生产代码里仍零调用（只有 `crates/proto/src/caps.rs` 的测试在用），前端是直接看字符串数组，不走这个方法。所以「`with_peer` 算交集」已经真正接上了，「`supports` 是唯一入口」这句仍不成立。
20. **真实运行指标（2026-11-24）**：`crates/agent/src/stats.rs` 用 `GetSystemTimes` 差分报**整机 CPU**、`GlobalMemoryStatusEx` 报**整机内存**，并按会话累计视频帧 / 音频包 / 文件块字节数差分出 **kbps**，填进 `status.stats`（此前恒为 `cpu:0, kbps:0` 的假值）。`status.subs` 也从恒空改为当前 `video.sub` 的真实快照（`id` / `stream` / 实际 `fps`）。屏幕墙把 `status` 转成 `wall-metrics` 事件，卡片上的 CPU / 内存条现在显示被控端真实占用，不再是设备目录里的空字段。`Stats` 新增 `mem` 字段（`#[serde(default)]`，旧被控端不带时按 0）。新增 8 个单测。

验收：CI 全绿；升级协议时旧客户端给出明确提示而不是黑屏（不带 `caps` 的旧 hello 由 `legacy_hello_without_caps_is_accepted` 覆盖）；屏幕墙卡片显示被控端真实 CPU / 内存；`GET /api/v1/metrics` 的会话数、字节数随真实推流增长；控制会话断开后五轮退避能自动恢复。

21. **远程画面光标（2026-11-25）**：之前 `VideoSub.cursor` 字段只在协议里存在，全仓无读取点、生产代码一律传 `false`，所以主控端看到的画面里没有鼠标，远程操作时找不准位置。现在补上：
    - 新增 `crates/agent/src/cursor.rs`：纯像素合成，不碰 COM，便于单测。支持 DXGI 的三种光标形状——`MONOCHROME`（`(dst & and) ^ xor` 经典掩码规则）、`COLOR`（alpha 混合）、`MASKED_COLOR`（alpha=0 替换、`0xFF` 异或）；含越界裁剪、隐藏不画、形状数据不足时跳过。
    - DXGI 路径（`crates/agent/src/dxgi.rs`）：`AcquireNextFrame` 后读 `LastMouseUpdateTime` → `PointerPosition`（可见性与位置），`PointerShapeBufferSize > 0` 时才调 `GetFramePointerShape` 取形状并缓存，缓冲复用、`DXGI_ERROR_MORE_DATA` 时扩一次重取。**只有光标移动、没有新画面**的帧（`AccumulatedFrames == 0`）不重拷 8 MB 纹理，改用 `CursorBackup` 还原上一帧被盖住的像素再画新光标，避免残影；位置与形状都没变则返回 `None` 不重复发帧（顺带修掉了原先「无变化也重发同一帧」的问题）。
    - GDI 回退路径（`crates/agent/src/capture.rs`）：`GetCursorInfo` 取可见性与热点，`GetIconInfo` 取位图尺寸（句柄不变时缓存，且始终 `DeleteObject`），`DrawIconEx` 按采集缩放比换算位置与尺寸。
    - 开关：`CaptureOptions.cursor` ← `cloud.rs` 的 `cursor: sub.cursor && !view_only`；聚焦流默认 `true`，屏幕墙与群控保持 `false`。
    - 前端：删掉控制页原先本地绘制的 8 px 圆点光标（被控端画面现在自带真实光标，两层会重影）。
    - **需要人工确认**（无法在自动化测试里验证）：光标位置在多显示器负原点 / 非 100% DPI 下是否贴合、三种形状的观感、快速移动时有无残影、GDI 回退路径的光标位置与大小、光标隐藏场景（全屏视频/游戏）切换瞬间旧光标是否被擦掉。

---

## P5 —— 局域网直连（已完成，2026-11-24）

同网段两台机器不经过账号服务器直接控制。协议零改动：`cloud.rs` 的 `session_task` 本来就是传输无关的（输入 `mpsc<Inner>`、输出 `mpsc<WsMessage>`），被控端本地放一个「服务器桩」把 tunnel 帧直接喂进去，采集 / 输入 / 文件 / 音频 / 剪贴板全部原样复用。详见 [局域网直连](direct.md)。

1. 被控端本地桩 `crates/agent/src/direct.rs`：`DirectAuth` 白名单（配对 token / 长期 token 两个集合 + 活动会话计数）、TLS + WS 握手、授权回调包装（允许时把配对 token 提升为长期 token，拒绝时补发 `closed`）、授权期间到达的 tunnel 帧按会话号缓存（单会话上限 4 MiB）后原序补投。
2. 客户端 `crates/client/src/direct.rs`：监听循环（独立任务，**不能放 `host_loop`**——那里未登录直接 return）、配对码生成与 10 分钟倒计时、主控接入、证书指纹固定（`PinnedCert` 比对 `screenwall_agent::tls::fingerprint()`）。
3. 凭据分两个方向存：`known_peers` 是「我能连谁」（主控端），`direct_tokens` 是「谁能连我」（被控端）。长期 token 在授权通过时由 `on_promote` 回调落盘，下次启动 `replace_all` 灌回白名单。
4. 输出路由 `crates/client/src/main.rs`：`control_out(shared, session)` 按会话号决定走直连 WebSocket 还是服务器；`wall.rs` / `group.rs` 保持走服务器（屏幕墙与群控是账号功能）。
5. 命令：`get_direct_state` / `set_direct_enabled` / `regenerate_pairing_code` / `forget_direct_peers` / `connect_direct`。
6. 前端：设置 → 连接 → 局域网直连（开关、地址、端口、会话数、6 位配对码 + 倒计时 + 重新生成、已授权设备数与「清除授权」）；设备页「直连」按钮弹窗填地址与可选配对码；控制页复用已有会话（直连 deviceId 形如 `direct:<ip>:<port>`，不在设备列表里）。
7. 安全：默认关闭、只接受局域网来源（`is_lan_peer` 在 accept 时校验，公网地址直接断开）、每次会话必须本机点「允许」（不受「允许被控」总开关影响）、配对码 10 分钟过期且**配对成功即换码**、证书 SHA-256 指纹固定、认证失败限流、长期授权可撤销、诊断包脱敏两个方向的凭据。直连会话**不上报服务器审计**；本地日志只有被控端的两条 `info`（连接建立、会话关闭），主控端只有异常时的 `warn`，没有审计式记录（查看者、起止、时长）。
8. 测试（截至本次核对，数量以代码为准）：`crates/agent/tests/direct.rs` 的集成用例覆盖 token 提升与整体替换、提升通知落盘且不重复、换码不影响长期凭据、配对码到期失效、认证失败限流与成功清零、无授权被拒、错误 token 被拒、端到端会话、`Welcome.caps` 为交集、拒绝授权补 `closed`；`crates/agent/src/direct.rs` 的 `tests` 模块另有 6 个限流单测和 2 个来源校验单测；`crates/client/src/logging.rs` 的 `export_scrubs_peer_tokens`。

验收：`cargo test --workspace` **214 passed / 0 failed**；`scripts\ci.ps1` 六步全 PASS；本机双实例（第二个实例用 `APPDATA` + `WEBVIEW2_USER_DATA_FOLDER` 隔离）实测 1920×1080 / 30 FPS / 37 Mbps / 零丢帧 / 最大帧间隔 65 ms，`activeSessions` 0→1→0，免配对码重连成功。

**收口时修掉八个缺陷**（详见 [direct.md](direct.md)）：白名单灌错方向（A 连过 B 后 B 能反向免码连 A）、长期 token 只存内存（重启即丢）、诊断包泄露直连凭据、配对码熵不足（时间戳可预测）、配对成功后不换码、配对码过期只在界面上（白名单里永久放行）、**认证失败限流只写在文档里**（`AuthLimiter` 根本不存在，6 位配对码可无限爆破且每次连接都 spawn 任务）、退避算出来却永不生效（记的是第一次失败时刻，整个窗口恒定拒绝，指数退避升不到第二级）。

**已知**：只在本机双实例验证，没有两台物理机实测。perMachine 安装器会自动添加 `ScreenWall Direct` 入站规则（`program=` 限定为 `screenwall-client.exe`，只放行专用/域网络）；从旧 `currentUser` 版本升级或规则被删时，设置页会提示并提供一次 UAC 的修复按钮。尚未在真实两块网卡上验证规则是否覆盖所有网络配置文件 —— 排障命令见 [direct.md](direct.md)「连不上怎么查」。

---

## P6 —— 服务器管理（内嵌 KayPanel）（已完成）

需求：在 ScreenWall 里直接管理多台 Linux 服务器，不另开应用、不切浏览器。屏幕墙管的是 Windows 机器的画面与键鼠，服务器管理管的是 Linux 主机的 SSH 运维，两者在同一个窗口里并列。

### 方案：sidecar，不重写

KayPanel 是 Go 写的 Linux 服务器管理面板，自带 SSH/SFTP/PTY、Docker/Compose 调用、告警推送、AI 运维助手、前端静态资源与登录/CSRF/审计逻辑。把它整体重写成 Rust 是数周工作量，而且要把鉴权、命令执行、文件操作这些高风险路径重新验证一遍；后续上游更新也无法合并。

因此采用 **sidecar**：

- 上游源码 vendor 在 `third_party/kaypanel/`（Go 1.23，MIT，上游 v0.14.1），`go.sum` 一并保留，构建可复现。
- 只给 `main.go` 增加 `-sidecar` 模式：强制监听 `127.0.0.1:0`（回环随机端口），启动后向 stdout 输出一行握手 JSON，stdin 读到 EOF 时优雅退出。
- 未修改上游的业务逻辑、鉴权逻辑与前端资源。
- 许可与本地改动声明在 `third_party/kaypanel/THIRD-PARTY-NOTICE.md`。

代价是安装包多一个 `kaypanel.exe`（约 9.4 MiB，未压缩）和多一套 Go 构建工具链；换来的是零重写风险、与上游同步的能力，以及面板内部行为完全保持原样。

### 架构

```
侧边栏「管理 → 服务器管理」
        │  PageKey = "server"
        ▼
ui/src/pages/ServerPage.tsx ── 量出内容区矩形（CSS 像素）
        │  invoke show_server_panel({x, y, width, height})
        ▼
crates/client/src/panels.rs
        │  首次进入：spawn kaypanel.exe -sidecar -config %APPDATA%\ScreenWall\kaypanel\config.yaml
        │  ← stdout 第一行：{"url":"http://127.0.0.1:<随机端口>","token":"<一次性令牌>","pid":<pid>}
        │  Window::add_child(WebviewUrl::External("<url>/api/desktop/login?token=<一次性令牌>"))
        ▼
子 WebView2（label `kaypanel-embed`，独立 WebView2 数据目录）
        │  HTTP，仅回环
        ▼
kaypanel.exe（sidecar）── SSH ──> Linux 主机（目标机无需 Agent）
```

关键实现：

| 位置 | 作用 |
|---|---|
| `third_party/kaypanel/main.go` | `-sidecar` 模式：回环随机端口、stdout 单行握手、stdin EOF 退出 |
| `scripts/build-kaypanel.bat` | `go mod verify` + `go build -trimpath -ldflags "-s -w -H windowsgui -X main.version=0.14.1"`，产物 `third_party\kaypanel\dist\kaypanel.exe` |
| `crates/client/tauri.conf.json` | `bundle.resources` 把 `kaypanel.exe` 打进安装包 |
| `crates/client/src/panels.rs` | 拉起/复用子进程、读握手、创建并摆位子 webview、切页隐藏、退出清理 |
| `ui/src/pages/ServerPage.tsx` | 量内容区位置、窗口缩放时同步、离开页面时隐藏；页面本身只放加载/错误态 |
| `ui/src/nav.ts` | 「管理」分区新增「服务器管理」，`PageKey` 增加 `"server"` |

两个实现上的硬约束：

1. **不用 iframe。** kaypanel 响应带 `X-Frame-Options: DENY` 与 `SameSite=Strict`，iframe 必然白屏。子 webview 是独立的浏览上下文，不受这两条限制，也不会与主界面的 CSS/JS 互相污染。
2. **子 webview 用独立 WebView2 数据目录。** 主窗口通过 `additionalBrowserArgs` 建了共享 user data folder 的 environment；第二个 webview 用不同参数复用它时，WebView2 返回 `HRESULT(0x8007139F)`（`ERROR_INVALID_STATE`）。因此给面板单独一个数据目录 `%APPDATA%\ScreenWall\kaypanel\webview2`，顺便把面板的登录 cookie 与主窗口隔离。Tauri 的 `Window::add_child` 需要 `unstable` feature（`crates/client/Cargo.toml` 已启用）。

### 握手与鉴权

sidecar 模式下：

- 只监听 `127.0.0.1:0`，由操作系统分配空闲端口；配置文件里的 `-listen` 被忽略，避免面板误暴露到局域网。
- 一次性登录令牌由 kaypanel 进程内部随机生成，只经父子进程的私有 stdout 管道交给 Rust，不出现在命令行参数或环境变量里；Rust 用它拼出 `/api/desktop/login?token=...` 让内嵌 webview 免密进入。
- 日志全部走 stderr；Rust 侧把 stderr 接到 `Stdio::null()`，避免管道写满卡住子进程（需要看子进程日志时应手动运行，见 [server-panel.md](server-panel.md) 的排障一节）。

### 构建与打包

- 依赖 **Go 1.23+**。
- 先跑 `scripts\build-kaypanel.bat` 生成 `third_party\kaypanel\dist\kaypanel.exe`，再跑 `scripts\tauri-build.bat build --bundles nsis`。
- `scripts\tauri-build.bat` 在发现 `dist\kaypanel.exe` 不存在时会自动调用 `build-kaypanel.bat`；Go 缺失时整次打包失败，不会静默产出一个缺面板的安装包。
- 首次构建需要联网拉 Go 模块；脚本在未设置 `GOPROXY` 时默认 `https://goproxy.cn,direct`，Go 不在 `PATH` 时可用 `GO_BIN` 指定其 `bin` 目录。
- 运行时定位顺序（`panels.rs` 的 `sidecar_path`）：Tauri `resource_dir()` → 可执行文件同目录 → 开发态仓库内 `third_party/kaypanel/dist/kaypanel.exe`。

### 进程生命周期与清理

- Rust 的 `Hub` 持有 `Child`、`ChildStdin` 与握手信息；重复进入页面复用同一个进程，不会起第二个实例。
- **正常退出**：应用退出时先关 webview，再 drop `stdin`。子进程读到 EOF，走 kaypanel 自己的 5 秒优雅关闭流程。
- **兜底**：drop `stdin` 后最多等 1.5 秒，仍未退出就 `kill` 并 `wait`。
- **父进程崩溃**：操作系统关闭管道句柄，子进程同样读到 EOF 自行退出，不产生孤儿进程（已实测）。
- **子进程先挂**：`ensure_started` 用 `try_wait` 检测，下次进入页面时清理旧状态并重新拉起。
- **切页不销毁**：离开「服务器管理」只调用 `hide`，webview 与进程都保留，下次进入秒开；窗口缩放时前端重新量位置并调用 `resize_server_panel`。

### 数据位置

| 路径 | 内容 |
|---|---|
| `%APPDATA%\ScreenWall\kaypanel\config.yaml` | 面板配置：节点、SSH 凭据、告警与 AI 设置等 |
| `%APPDATA%\ScreenWall\kaypanel\webview2\` | 内嵌面板独立的 WebView2 用户数据目录（cookie、缓存） |
| `%APPDATA%\ScreenWall\logs\screenwall.log` | 主程序日志，包含「服务器管理面板已启动 / 已内嵌」等事件 |

面板数据保存在本机，不走 ScreenWall 的账号与中转；它管理的是 SSH 可达的 Linux 主机。

### 已知限制

- **只能管理 Linux 主机（通过 SSH）**，不能管理 Windows 主机。Windows 机器归屏幕墙 / 远程控制模块管，两者不互通。
- 目标机需要开启 SSH（默认 22 端口）并可被本机访问；这是面板直接发起的连接，不经过 ScreenWall 中转，也不使用 ScreenWall 的账号体系。
- **首次使用需要在面板里「添加节点」**：填写地址、SSH 端口、用户名与密钥/密码，首次连接确认主机指纹（TOFU），之后指纹变化会拒绝连接。
- 面板是独立的浅色主题，不跟随 ScreenWall 的浅色/深色主题；主界面与面板的 CSS/JS 互不影响。
- 安装包因此需要附带 `kaypanel.exe`，打包机需要 Go 1.23+；CI 的处理见 [CI 文档](ci.md) 第 5 节。
- 面板版本随 ScreenWall 安装包走（当前内置上游 v0.14.1），没有独立于 ScreenWall 的更新通道。

---

## 已知约束

- 1080p 60fps 的编码在 16ms 左右，60fps 档实测 58–59；要稳定 60 需要硬件编码（Media Foundation / NVENC），列在 P2 后期评估。
- CPU 双线性缩放 1920→1600 要 30ms 以上，所以自适应只调画质不调分辨率；要改分辨率需要 GPU 缩放。
- 选中单块显示器时 DXGI 按输出复制；「全部屏幕」拼接和 RDP 会话仍回退 GDI（约 40ms/帧，仅保证功能）。
- 文件数据走 JSON + base64，体积膨胀约 33%；换来的是不需要改中转帧格式。实测 700 KB 上传约 0.16 秒。
- 文件传输依赖当前控制会话：上传同一时间只支持 1 个批次，下载最多 3 个并发（其余排队，可取消）。上传断点续传按整文件 SHA256 匹配，只有重新选择同一个文件才会命中；下载续传按「设备 + 远程路径 + 大小」匹配，远程文件被替换（大小变化）会从头下载。上传分片在 `Downloads\ScreenWall\.screenwall-parts`，下载分片在 `.screenwall-downloads`，超过 7 天未续传会在下次传输时清理。
- 整目录下载是逐个文件串行排队完成的，目录很大（上千个文件）时整体耗时按单文件带宽累加；递归上限 20000 个文件 / 64 层，超出会报错而不是静默截断。
- 剪贴板支持文本（≤64K 字符）与图片（PNG ≤ 8 MiB），**文件剪贴板（复制粘贴文件）未做**；图片仅支持 24/32bpp `BI_RGB` 的 CF_DIB，16bpp / `BI_BITFIELDS` 的位图会跳过（避免错色），部分浏览器/截图工具可能落在这类格式上。同机自控只能验证「不会回传自己写入内容」的逻辑，跨机同步需要两台机器实测。
- 群控是 N 条独立会话：同账号设备免确认，预览带宽随设备数线性增长（直连/连接码会话仍要逐台授权）。
- 声音默认走 Opus（64 kbps 起，24–96 kbps 自适应），与画面共享同一条 WSS 隧道；码率只按主控端播放缓冲调整，不感知链路丢包。WebView2 不支持 WebCodecs（或 Agent 编码器初始化失败）时回退到未压缩 PCM（48 kHz 立体声约 1.5 Mbps），弱网下仍可能挤占画质。只采默认输出设备的回环，不能只采某个应用；同机自控会把本机正在放的声音再播一遍（自测时的正常现象）。
- 声音已支持系统回环与麦克风两种来源，麦克风来源带 AEC（NLMS + 延迟对齐 + 双讲检测 + 残留抑制，约 100 ms 滤波器）。AEC 的效果取决于外放音量、房间混响与设备延迟：强烈外放 + 大混响下仍会有残留回声，极端情况建议对端戴耳机。回环参考打不开时自动旁路为直通（日志会警告），`SCREENWALL_AEC=0` 可整体关闭。「没有输出设备 / 没有麦克风」分支返回 4110，但本机有设备，未在无声卡机器上实测。Opus 重采样为线性插值，采样率不是 48 kHz 的设备会有轻微音质损失。
- 音量联动只作用于默认输出设备的主音量与静音，不区分单个应用；同一时间只有一路声音订阅，切换来源会重建采集。
- 远程开机（WOL）有两条路：① 主控端本机广播，要求目标与主控在同一局域网（或路由器配置定向广播）；② **异地代发（2026-11-25 已实现）**：设备页唤醒弹窗里选一台在线的同账号设备，由它向目标网段广播 —— 主控端开一条一次性控制会话（`mode=control`，不订阅画面、不进控制页），代发设备**仍需本机用户点「允许」**（和普通远程控制一样，不削弱「每次会话必须本机确认」的边界），拿到会话后只发 `hello` + `wol.request`，收到 `wol.ok` 或错误就立刻关闭；6 秒无回应按超时处理。代发设备必须与目标在同一局域网，且其日志里会留下一条普通的控制会话审计（时长约 1 秒）。两条路都要求目标 BIOS 与网卡属性已开启 Wake on Magic Packet；**都未在真实断电设备上验证过唤醒效果**。
- 远程重启/关机只做了二次确认与命令构造，没有真机执行过（会重启本机）；被控端是普通用户进程，遇到会阻止关机的程序时 `shutdown /t 3` 可能超时失败，v1 不加 `/f` 强制结束。
- 删除其它设备、双设备改名等管理操作由服务器集成测试覆盖（两台虚拟设备），没有第二台物理机做界面实测。
- 同机自控（本机既主控又被控）时 `SendInput` 会移动真实光标，WebView 再产生原生 mousemove，形成轻微坐标反馈；跨机没有这个问题。
- dev 模式下 React StrictMode 会让控制页发两次 `open`，第二次会顶掉第一次（账号会话免确认，表现为刚连上就断开重连一次）；release 构建没有这个问题。
- **服务端未在 Linux 上编译/运行过**（2026-11-24）。代码与依赖层面已核查通过：`crates/server` / `crates/proto` 无 `cfg(windows)` 或 `windows::` 引用，内部依赖只有 `screenwall-proto`，无 `openssl`/`native-tls`（TLS 走 `rustls` + `ring` 纯 Rust），无硬编码 Windows 路径。但本机没有 WSL 发行版与 Docker，`rustup target add x86_64-unknown-linux-gnu` 因网络问题未完成，所以**没有真实编译证据**。部署步骤见 [server.md 6.4](server.md)。注意只能 `cargo build -p screenwall-server`，`--workspace` 会因为 `screenwall-agent`（windows crate）与 `screenwall-client`（tauri）而失败。
- 安装包已构建并通过 Tauri 的 NSIS 校验，但本机没有实际执行安装/卸载（避免动到现有登录配置和 WebView2 数据目录）；在干净机器上的安装流程仍需实机走一遍。
- CI 与代码签名：workflow 与本地脚本都已就绪并实测全绿，但**目录不是 git 仓库**，GitHub Actions 从未真正跑过；也没有商业代码签名证书，安装时 SmartScreen 仍会提示。
- 重连与限流在单机（本机自控）验证：五轮退避与「主控 WS 断开→挂起→恢复后重连」已实测，但真实公网弱网（丢包、长 RTT、NAT 超时）下的表现未验证。
- 局域网直连只在本机双实例（`APPDATA` + `WEBVIEW2_USER_DATA_FOLDER` 隔离）验证，**没有两台物理机实测**。两个实例走 loopback，绕过了防火墙与真实网卡，所以「物理机能否连通」这件事从未被验证过 —— 这是当前最该先补的一项。
- **防火墙规则（已实现，未真机验证）**。安装包改为 `installMode: perMachine`，NSIS 钩子 `crates/client/windows/installer-hooks.nsh` 在安装后添加 `ScreenWall Direct` 入站 TCP 规则（`program=` 限定当前 exe，`profile=domain,private`），卸载时删除。旧 `currentUser` 安装升级、规则被删或端口改动时，设置页会检测到并在用户点击后弹一次 UAC，以管理员身份重跑自身补规则。尚未实测：网络配置文件被 Windows 划为 `Public` 时规则不生效；从旧版本升级会留下旧的 HKCU 卸载项，建议先卸载旧版再装新版。`docs/direct.md` 的「连不上怎么查」给了手动补规则与逐层排查的命令。
- ~~直连会话不写服务器审计~~ **已补（2026-11-25）**：主控端在直连会话结束时 `POST /api/v1/audits/direct`，审计页能看到起止、时长与被控端地址（`mode=direct`）。**但这是客户端自述**：被控端完全不知道这条记录被上报，断网时也不会上报（只在 `end_session` 那一层补一次，失败只记 debug 日志）；流量恒为 0。
- **局域网自动发现已完成（2026-11-25）**：被控端开启直连后在 UDP 47901 上回答探测（`crates/agent/src/discovery.rs`），主控端在设备页「直连」弹窗里点「扫描同网段设备」即可列出同网段开启了直连的机器（显示设备名 + 地址，点一下自动填好），不用再手抄 IP。实现是自定义 UDP 广播协议（探测报文 `SCREENWALL-DISCOVER-1`，应答为 JSON），没有引入 mDNS 依赖；广播只发到 `255.255.255.255`，多网卡环境下可能只走默认出口网卡。防火墙安装钩子与设置页的「允许局域网直连」现在会同时放行 TCP 47900（直连）与 UDP 47901（发现），只针对 ScreenWall 自己的 exe。
- **直连自动重连（2026-11-25 已实现）**：直连会话现在 `reconnectable: true`，走与中转相同的五轮退避（1s/2s/4s/8s/8s）与三个重连事件，控制页横幅对直连同样生效。重连用 `known_peers` 里的长期 token（不是配对码），TLS `PinnedCert` + 连接后显式指纹复核双重校验，指纹变化明确失败。**但被控端每次直连连接都会弹授权窗**，所以无人值守时每轮重连都要等人点「允许」，5 轮全超时就放弃。要做到真正的无人值守重连，需要在 agent 侧对已配对的长期 token 放行、不再弹窗——这会削弱「每次会话必须本机确认」这条安全边界，**未做，待决定**。
- **无 UDP 打洞**。直连与中转都只走 TCP，跨 NAT 的 P2P 直连没有做（按用户要求放到最后）。直连监听绑 `0.0.0.0`，来源校验 `is_lan_peer` 认识 IPv6 地址（回环 / `fc00::/7` / `fe80::/10` / IPv4 映射地址），但连接地址、WOL 与其余网络代码仍是 IPv4（`Ipv4Addr`），没有验证过 IPv6 端到端连通。
- 只有 MJPEG，没有 H.264 或硬件编码：1080p 30fps 在 16 ms 左右勉强够用，60 fps 档实测 58–59 帧，稳定 60 需要 Media Foundation / NVENC（列在 P2 后期评估）。
- **屏幕墙「文件拖到哪块屏就发给哪台」（2026-11-25 已实现）**：屏幕墙卡片成为 drop 目标，拖入时高亮并提示「松开即发送到 <设备名>」，分别处理设备离线、正在发送、0 字节空文件、已有其它设备会话（二次确认后切换）几种情况，发送中/成功/失败都有卡片级反馈。文件传输依赖控制会话，所以 drop 时会先确认会话状态。
- **UIPI 限制**：被控端是普通用户进程，目标程序以管理员权限运行时 `SendInput` 会被拦掉，控制不了那类窗口（如任务管理器、UAC 提权后的程序）。现在 Agent 会把 `SendInput` 的 `ERROR_ACCESS_DENIED` 判定为 UIPI 拦截，通过 `input.status` 告知主控端，界面提示「需要以管理员身份运行被控端」；以管理员运行 / 安装为服务仍是唯一根治办法。
- **服务器管理内嵌 KayPanel（sidecar）**：只通过 SSH 管理 Linux 主机，不能管理 Windows 主机；面板固定浅色、与主界面主题独立；首次使用要手动「添加节点」。`kaypanel.exe` 随安装包分发，本地打包需要 Go 1.23+，CI 目前没有显式安装 Go（见 [CI 文档](ci.md) 第 5 节）。面板数据保存在 `%APPDATA%\ScreenWall\kaypanel\`，不走 ScreenWall 账号同步。

---

## 验收记录（2026-11-24，本机自控）

| 项 | 命令 / 操作 | 结果 |
|---|---|---|
| 被控端授权 | 屏幕墙点「控制」→ 本机弹窗 → 允许 | 会话建立并 30 FPS 推流；拒绝/超时两条分支见 `cloud.rs` 集成测试 |
| 会话跨页复用 | 控制页点「文件传输」 | 会话保持（日志无 close），文件页显示「已连接 · 会话 #134」 |
| 离开即断开 | 文件页点「屏幕墙」 | 日志出现「主控端关闭中转会话 #134」 |
| 文件上传 | 文件页选择 700 KB 测试文件 | 0.16 秒完成，落盘 `Downloads\ScreenWall\upload-test.bin`，SHA256 与源一致 |
| 文件下载 | 远程目录点下载 | 保存为 `upload-test (1).bin`（同名自动加序号），SHA256 与源一致，无残留 `.part` |
| 上传断点续传 | 通过 Tauri 命令：4 MiB 文件先传 1 MiB 后 `cancel`，再用同哈希重新 `offer_files` | 第二次 `file.accept` 返回 `resume: [{item:0, offset:1048576}]`，从 1 MiB 续传后落盘 SHA256 `bb9f8df6…3af8` 与源一致，`.screenwall-parts` 清空 |
| SHA256 拒绝 | 通过 Tauri 命令：`file.offer` 声明 64 个 `f` 的假哈希后传完 512 KiB | 被控端日志「上传接收完成」但 `file.done.ok=false`（SHA256 校验失败），目标文件未出现，坏分片被删除 |
| 下载校验 | 下载上述 4 MiB 文件 | 保存为 `resume-test (1).bin`，本机 SHA256 与源一致，无残留 `.part`，界面提示校验通过 |
| 前端哈希 | `js-sha256` 流式切片对 4 MiB 文件计算 | 结果 `bb9f8df61474d25e71fa00722318cd387396ca1736605e1248821cc0de3d3af8`，与 `Get-FileHash` 一致 |
| 从文件页连接 | 屏幕墙卡片点「文件」→「连接并传输」 | 被控端弹授权，允许后直接进入可传输状态（会话 #135） |
| 剪贴板 | `cargo test -p screenwall-agent --lib` | 12 个测试含 4 个剪贴板用例通过；同机自测无回声报错，跨机待两台机器 |
| 屏幕墙缩略图 | 重启后进入屏幕墙页 | 卡片显示本机实时画面（320×180 / 1 FPS，`wall-frame` 持续更新），**没有**授权弹窗；页面停留期间只有一条缩略图会话 |
| 缩略图会话隔离 | 从卡片点「控制」→ 允许 | 离开屏幕墙时缩略图会话立即关闭；控制会话独立建立并 30 FPS 推流，两者互不干扰 |
| 缩略图免审计 | 打开屏幕墙后再查 `/api/v1/audits` | 审计表仍只有控制会话记录，`wall` 会话未写入 |
| 群控单台 | 群控页选中本机 → 开始群控 → 被控端允许 | 会话建立，卡片实时画面 640×360@2，日志「群控会话已建立」 |
| 群控改画质 | 运行中切「流畅 / 高清」 | 重发 `video.sub`，画面变 480×270@1 / 960×540@3，会话号不变 |
| 群控广播坐标 | 在卡片上点击，抓 `group-input` | 归一化坐标经 `SendInput` 落到对应像素（0.8,0.2 → 1535,215），多机按各自分辨率映射 |
| 群控多机扇出 | `cargo test -p screenwall-server --test group` | 2 台设备 2 条会话，广播帧分别到达、互不串台，`close` 逐条送达 |
| 群控清理 | 移除唯一设备 / 离开群控页 | 群控停止并关闭对应会话 |
| 显示器列表 | 建立控制会话 | 日志「收到被控端显示器列表 count=1」，「画面」面板出现「显示器 1 · 主屏 · 1920×1080」 |
| 整块桌面订阅 | `set_stream_options` 传 `display=4294967295` | Agent 日志 `display=4294967295`，DXGI 1920×1080 重新出流，会话号不变 |
| 越界显示器 | `set_stream_options` 传 `display=5` | 立即返回「显示器 6 不存在」，原画面不受影响 |
| 显示器坐标映射 | 广播 `abs [0.8, 0.2]` | 光标落到 (1535,215)，与切换前一致（单屏等价；跨屏换算见单测） |
| 声音订阅 | 控制页点「声音」 | Agent 日志「系统声音采集已启动 codec=Opus 48000/2 bitrate=64000」，主控端收到 `audio.sub.ok`，按钮标题显示 `Opus 64 kbps · 48 kHz` |
| 声音链路 | 播约 13 秒系统提示音，页面里统计 `pushOpus` 包 | 672 包 / 163,332 B（≈97 kbps，已从 64 kbps 自适应上调），48 kHz 立体声，`seq` 连续 |
| 码率自适应（降） | CDP 强制 `bufferMs=420` | Agent 日志逐级 96000→72000→54000→40500→30375→24000（下限） |
| 码率自适应（升） | 恢复真实缓冲（约 40–60 ms） | Agent 日志逐级 24000→30000→37500→46875→58593→73241→91551→96000（上限） |
| PCM 回退 | `set_audio_enabled` 传 `codec:'pcm16'` | Agent 日志 `codec=Pcm16`，按钮标题 `PCM 48 kHz`；播 6 秒提示音收到 450 个 `pushPcm` 块（2,304,000 字符），正常播放 |
| 旧客户端兼容 | `set_audio_enabled` 不传 `codec` | 按默认 Opus 订阅（`codec=Opus bitrate=64000`），按钮标题 `Opus 64 kbps · 48 kHz` |
| 声音播放 | 检查 `window.__audioPlayer`（dev 构建） | `AudioContext.state = "running"`，`codec = "opus"`，缓冲约 40–60 ms，Opus 包按 20 ms 排程 |
| 关闭声音 | 再点「声音」/ 离开画面页 | 日志「声音订阅已停止」「系统声音采集已停止」，画面与会话不受影响 |
| 设备重命名 | 设备页展开本机 → 重命名 → 保存后点刷新 | 名称变 `KAYNAI-Renamed`，服务器 `GET /devices` 返回新名字，再改回 `KAYNAI` |
| 允许控制开关 | 设备页关闭「允许远程控制」→ 刷新 | 开关保持关闭；重新打开后恢复，服务器接口由集成测试覆盖 |
| 删除本机 | 展开本机设备卡片 | 不显示「移除」按钮（后端也会拒绝）；删除他人设备由 `tunnel.rs` 集成测试覆盖 |
| 屏幕截图 | 控制页点「截图」 | 落盘 `Downloads\ScreenWall\ScreenShot-20261124-151919.jpg`（237 KB，JPEG 头 `FF D8 FF`），提示真实路径 |
| 电源确认 | 控制页点「重启」 | 弹出二次确认（目标设备、3 秒后重启、会话会断开）；点取消不发送。**未点确认**，避免真机重启 |
| 会话审计 | 侧边栏「管理 → 审计」 | 概览显示当前页会话数 / 进行中 / 累计时长 / 累计流量 / 匹配总数；表格按开始时间倒序，进行中会话时长实时计算 |
| 审计过滤 | 审计页选设备、切「进行中/已结束」、输入关键词 | 记录数与底部「共 N 条」同步变化；无结果时出现「清除筛选」；服务端 `total` 与过滤条件一致 |
| 审计分页 | 记录超过 20 条时点「下一页」 | 翻页不重不漏（服务端单测 `audit_page_paginates_without_duplicates_or_gaps` 覆盖），页码与总数同步 |
| 托盘常驻 | 点窗口 ×，再从托盘左键/菜单「打开主界面」 | 进程不退出，窗口隐藏后可重新显示；设置里关掉「关闭窗口时缩到托盘」后 × 直接退出 |
| 托盘允许被控 | 托盘菜单点「允许远程控制」 | 与设置页开关同步（写 `client.json` 并发 `tray-allow-control`），关闭时待确认请求一并拒绝 |
| 滚动日志 | 运行一段时间后看 `%APPDATA%\ScreenWall\logs` | `screenwall.log` 持续写入；超过 2 MiB 滚成 `.1`，最多保留 3 个历史文件（单测 `writer_rotates_after_limit`） |
| 导出诊断包 | 设置 → 连接 → 后台与诊断 → 导出诊断包 | 生成 `diagnostics-<时间戳>` 目录，含日志、`system.json` 与 `client.json`，其中 `token` 被替换为 `<已脱敏>`（单测 `export_scrubs_token`） |
| 假按钮移除 | 控制页与屏幕墙检查 DOM | 「开机/唤醒」假按钮不存在；`Ctrl+Alt+Del` 现在是真实按钮（`sas` 请求，未在真机验证 `SendSAS` 是否被系统策略接受） |
| 下载续传 | 200 MiB 远程文件下载到 118,489,088 B 时取消，再点下载 | 返回 `resume: 118489088`，完成后 209,715,200 B、SHA256 `1B338CBA…A29A` 与源一致，`.screenwall-downloads` 清空 |
| 下载续传（旧端兼容） | `file.accept` 不带 `offset`（旧 Agent 路径） | 主控端收到首块即清空前缀从 0 重收，任务正常完成（单测 `resume_offset_uses_existing_part` / `hash_prefix_matches_full_prefix`） |
| 审计页（真机） | 侧边栏「管理 → 审计」 | 真实服务器 18 条记录，概览与表格一致；进行中会话时长每秒递增 |
| 审计分页（真机） | `list_audits` limit=2 offset=0/2 | `total=16`，page1 `[16,15]`、page2 `[14,13]`，不重不漏；`deviceId=nope-device` 返回 `total=0` |
| 托盘常驻 | 向窗口发 `WM_CLOSE` | 进程仍在（`running=1`），窗口 `visible=False`，重新显示后恢复 |
| 诊断导出（真机） | `export_diagnostics` | 生成 `diagnostics-20261124-165706`，含 `screenwall.log`、`system.json`、`client.json`；`client.json` 中 `token` 为 `<已脱敏>` |
| 更新检查 | 设置 → 关于 → 检查更新（服务器 `[update].version` 改为 0.1.1） | 显示「发现新版本 0.1.1 / 当前 0.1.0 / 可更新」；改回 0.1.0 后显示「已是最新版本」 |
| 更新下载校验 | `download_update` 正确哈希 / 错误哈希 / `..%2Fserver.toml` | 正确哈希落盘 3.95 MiB 且 SHA256 `be50163c…9c2c`；错误哈希报「安装包校验失败…已删除下载文件」；穿越 URL 返回 404 |
| 音量读取 | 建立会话后 `get_remote_volume` | `control-volume` 事件回来 `level=0.4 muted=false`，侧栏滑块显示 40% 且可拖动 |
| 音量写入 | `set_remote_volume {level:0.35}` / `{muted:true}` / `{muted:false, level:0.6}` | 三次都回 `audio.volume.ok`，事件依次 0.35/false、0.35/true、0.6/false；系统音量确实随之变化 |
| 音量 UI | 侧栏拖动滑块到 25%、点静音/取消静音 | 滑块即时显示 25%，180 ms 后下发；静音按钮变 `is-on` 且 `aria-label` 变「取消静音」，再点恢复 |
| 音量参数校验 | `set_remote_volume {level:1.5}` / 空参数 / 错误 clientToken | 分别返回「音量需在 0.0–1.0 之间」「需要提供音量或静音状态」「控制会话已切换，请重新连接」 |
| 麦克风来源 | `set_audio_enabled {source:'microphone'}` | `audio.sub.ok.actual` 为 `codec=opus channels=1 bitrate=48000 source=microphone`；切回 `loopback` 得到 `channels=2 bitrate=64000 source=loopback` |
| 音源切换 UI | 侧栏「远程声音」点「麦克风」 | Segmented 选中态切换，提示「已切换为麦克风（开启后生效）」；播放中切换会重发 `audio.sub` 并清空旧 stream |
| 音量联动旧端兼容 | 模拟 `UNKNOWN_MESSAGE` | 只置灰滑块与静音按钮并显示「被控端版本较旧，暂不支持音量联动」，不把整页判成出错 |
| WOL 发送 | `wake_on_lan {mac:'AA:BB:CC:DD:EE:FF', broadcast:'255.255.255.255'}` | 返回成功，magic packet 实际发出 |
| WOL 校验 | `mac:'zz'` / `broadcast:'8.8.8.8'` / 组播 MAC | 分别返回「MAC 地址无效: zz」「广播地址无效: 8.8.8.8」；`normalize_mac` 拒绝首字节最低位为 1 的组播地址 |
| WOL 界面 | 设备页离线设备「唤醒」按钮 | 弹窗预填 `192.168.1.255` 广播地址与 MAC 输入，格式错误就地提示；**未在真实断电设备上验证唤醒效果** |
| WOL 异地代发（2026-11-25 双实例） | 主控 A 对离线设备点「唤醒」→ 发送方式选「通过 KAYNAI 代发」→ 输入假 MAC `02:00:00:00:00:02`；代发实例 B 弹授权 → 点「允许控制」 | B 日志 `已发送远程开机 magic packet session=10 mac=02:00:00:00:00:02 broadcast=255.255.255.255`，回 `wol.ok` 后会话 10 ms 内关闭；A 提示「已请「KAYNAI」代发唤醒包」并记 `异地开机代发结束 ok=true`。B 的授权弹窗显示「KAYNAI（开机代发） 请求控制这台设备」，明确这是开机请求而非看画面 |
| WOL 异地代发（协议层） | `crates/agent/tests/cloud.rs::wol_request_is_relayed_and_rate_limited` | 控制会话里发 `wol.request` 回 `wol.ok`；紧接着第二次请求被 10 秒限流挡下，回 `BAD_REQUEST`「发送过于频繁」 |
| 整目录下载 | `download_remote_directory` 指向含 `sub/empty` 的测试目录 | 返回 `files=2 emptyDirs=2`；落盘 `Downloads\ScreenWall\dltest\a.txt`、`sub\b.txt`，`sub\empty` 空目录也被创建，内容与源一致 |
| 整目录下载 UI | 文件页目录行「下载整个文件夹」 | 右侧出现目录卡片「已完成 2/2 个文件 · 2 个空目录」，下方逐文件进度 |
| 并发下载 | 5 个 8 MB 文件整目录下载，抓 `file-transfer` 事件 | `queued` 5 条、`start` 5 条，首个 500 ms 内同时只有 3 条 `start`（上限生效），5 个文件 SHA256 全对，分片目录清空 |
| 并发下载（续） | 6 个 12 MB 文件 + 单文件大文件同时进行 | 大文件下载期间其他目录下载照常完成；6 个文件 SHA256 全对，总计 68.7 MB |
| 取消排队任务 | 对排队中的 `tid` 调 `cancel_file_transfer` | 返回成功，该任务不再启动，其他任务继续 |
| 全量测试 | `cargo test --workspace` | 当时 **108 passed / 0 failed**（含授权拒绝、文件路径穿越、上传/下载断点续传、SHA256 校验、目录展开与相对路径、审计分页与设备过滤、更新接口与路径穿越、群控扇出、跨屏坐标、Opus 编解码与重采样、WOL 地址校验与 magic packet、音量消息编解码、日志滚动与脱敏、电源参数、设备管理接口）；本批新增用例后为 **172 passed**，见下表 |
| 前端类型 | `cd ui && npx tsc --noEmit` | 零错误 |
| 安装包 | `scripts\tauri-build.bat build --bundles nsis` | 当时的 NSIS 产物 `ScreenWall_0.1.0_x64-setup.exe`（4.16 MiB，SHA256 `904F6B9E…B2661`）；**已被下表 19:25 重打的 4.20 MiB 版本取代** |

### 补充验收（2026-11-24 19:00–19:30，能力协商 / 指标 / AEC / 重连 / CI）

| 项 | 命令 / 操作 | 结果 |
|---|---|---|
| 能力协商（中转） | 建立控制会话后读 Agent 日志 | `中转会话已建立 … caps=audio.aec,audio.opus,…,wall,wol`（15 项，含 `audio.aec`）。当时记录「`welcome.caps` 与交集一致」时，代码回的是 `Capabilities::current()`；现在 `cloud.rs` / `session.rs` / `direct.rs` 三处都已改成回交集（截至本次核对） |
| 真实 CPU / 内存 | 屏幕墙卡片 | 卡片显示 `CPU 38%` / `内存 54%`，多次刷新在 17%–50% 间变化，不再恒为 0 |
| 服务端指标（真实流量） | 控制会话运行中 `GET /api/v1/metrics` | `sessions.active=2`、`peak=2`、`total=4`；`bytes.toViewers=141570101` 随推流持续增长；`handshakes.ok=2`、`failed=0`；`devices.online=1`、`accounts=2`；`errors.rate5m=0.2` |
| 指标鉴权 | 不带令牌请求 `/api/v1/metrics` | 返回 `invalid_token`（不是匿名可读） |
| AEC 启用 | 控制页 `set_audio_enabled(source='mic')` | 日志「麦克风回声消除(AEC)已启用 enabled=true」「AEC 参考信号已接入（回环 → 麦克风） ref_rate=48000 ref_channels=2 mic_rate=44100 mic_channels=1 block=220」 |
| 连接质量 | `get_connection_quality(clientToken)` | `video.frames=3213, lost=0, fps=21.4, kbps=22665.6, maxGapMs=74`；`audio.packets=2839, gaps=1`；`net.reconnects=1, rttMs=1` |
| 主控断线 | 杀掉服务器进程 4 秒后重启 | 客户端顶栏变「正在连接服务器」、屏幕墙显示「连接中」；服务器恢复后自动重连，控制会话恢复需被控端重新授权（本机自动允许），`net.reconnects` 变 1 |
| 重连后指标 | 恢复后再读质量 | 帧数从 167 继续增长到 3213，`since` 不变（同一会话统计），无丢帧 |
| CI（本地） | `powershell -File scripts\ci.ps1` | fmt 4.4s / clippy 7.5s / test 30.3s / tsc 3.1s / build 2.4s 全部 PASS，退出码 0 |
| 全量测试（重跑） | `cargo test --workspace` | **172 passed / 0 failed** |
| 安装包（重打） | `scripts\tauri-build.bat build --bundles nsis` | `ScreenWall_0.1.0_x64-setup.exe`（4.20 MiB，SHA256 `C3D794766026A52408DD729300F3EF303D47947A6FC1473BAD6A16187023663D`）；**已被 21:52 含局域网直连的 4.49 MiB 版本取代** |

### 界面重做（2026-11-24 19:40–19:55，对齐 KayPanel）

参照仓库 `kaynai1/kaypanel` 的 `web/assets/style.css`，把界面配色、密度与圆角对齐到同一套视觉语言。字体栈两边本来就完全一致（`Segoe UI` / `Microsoft YaHei UI` / …），不需要改。

| 项 | 改前 | 改后 |
|---|---|---|
| 主题 | 只有深色 | 浅色 + 深色双主题，设置 → 连接 → 外观 → 主题切换，存 `localStorage`（`screenwall.theme`），无存储偏好时跟随系统 |
| 内容区底色 | `#0b0c0e` | 浅 `#f4f5f7` / 深 `#0a0f17` |
| 卡片与侧栏 | `#131417` / `#0e0f12` | 浅 `#ffffff` / 深 `#121a26` |
| 强调色 | `#8ea2ff` 淡紫蓝 | 浅 `#2563eb` / 深 `#60a5fa` |
| 成功 / 危险 / 警告 | `#4cc38a` / `#e56767` / `#e0b15c` | 浅 `#16a34a` / `#dc2626` / `#b45309`；深 `#34d399` / `#f87171` / `#fbbf24` |
| 小圆角 / 模态框 | 6px / 16px | 7px / 14px |
| 侧栏宽 / 顶栏高 | 236px / 56px | 212px / 58px |
| 字号阶梯 | 15px 整数（已是整数） | 保持 15px 整数，把残留的 13.5 / 12.5 半像素改成 14 / 13 |

**token 化**：规则层原本散着 67 处字面色值与 69 处 `rgba()`。现在 `styles.css` 的 `:root` 与 `:root[data-theme="dark"]` 定义两套 token，页面与组件样式只引用变量，`grep` 复查规则层已无字面色值（唯一例外 `--stage-bg`，视频舞台两套主题都必须近黑）。旧 token 名保留为别名（`--surface` → `--card` 等），页面样式不用一次性改。

**实测**（Chrome DevTools 计算样式 + 对比度换算）：

| 检查 | 结果 |
|---|---|
| 浅色 token | `--bg #f4f5f7`、`--card #ffffff`、`--text #0f172a`、`--accent #2563eb` |
| 深色 token | `--bg #0a0f17`、`--card #121a26`、`--text #e8eef7`、`--accent #60a5fa` |
| 正文对比度 | 浅色 16.37:1、深色 16.46:1 |
| muted 对比度 | 浅色 6.44:1、深色 6.66:1（均过 AA 4.5:1） |
| faint 对比度 | 浅色 4.97:1、深色 5.05:1 |
| 主题切换 | 设置页点「深色」→ `documentElement.dataset.theme` 变 `dark`，卡片底色由 `rgb(255,255,255)` 变 `rgb(18,26,38)`，无需刷新 |
| 未定义 token | 顺带修掉 `--bg-raise` / `--text-1` 两处引用了不存在变量的规则（原本静默失效） |
| 分段控件 | 主题开关被 flex 压扁成 28px，加 `flex: none` + `white-space: nowrap` 后恢复 48px |
| 构建 | `npm run typecheck` 无输出；`npm run build` 1620 modules，CSS 88.59 kB（gzip 14.26 kB） |

**已知**：浅色主题下视频舞台仍是近黑（`--stage-bg`），这是刻意的——画面周围一圈亮底会让缩略图看起来发灰。

### 局域网直连（2026-11-24 21:00–21:40，本机双实例）

两个实例都用同一份 `screenwall-client.exe`，第二个实例通过 `APPDATA` 与 `WEBVIEW2_USER_DATA_FOLDER` 环境变量隔离配置目录，模拟两台机器。

| 项 | 命令 / 操作 | 结果 |
|---|---|---|
| 监听 | 设置 → 连接 → 局域网直连 打开开关 | `47900` 进入 LISTENING，界面显示本机局域网 IPv4、端口、6 位配对码与 10 分钟倒计时 |
| 端口占用 | 先占用 `47900` 再打开开关 | 界面显示 `enabled && !listening` 警告，不崩溃，释放端口后自动开始监听 |
| 配对连接 | 实例 B 设备页「直连」填 `127.0.0.1` + 配对码 | 实例 A 弹授权窗，允许后建立会话，`activeSessions` 0→1 |
| 画面 | 直连会话运行中 | 1920×1080 / 30 FPS / 37–40 Mbps / 零丢帧 / 最大帧间隔 54–65 ms（与中转模式一致，DXGI 采集） |
| 拒绝授权 | 实例 A 授权窗点「拒绝」 | 实例 B 控制页报「连接已断开，请返回后重新连接」；被控端补发 `closed`，会话号回收，`activeSessions` 回到 0 |
| 错误配对码 | 填 `000000` | 被控端回 `AUTH_FAILED`，连接断开，不进入授权流程 |
| 免配对码重连 | 断开后再次「直连」只填地址 | 直接用 `known_peers` 里的长期 token 连上，无需配对码，不弹授权以外的额外步骤 |
| 证书指纹固定 | 删掉被控端 `direct` 证书目录后重启再连 | 主控端拒绝，日志「直连证书指纹不匹配，已拒绝连接」，界面报 `无法连接 <地址>: …`（含 rustls 的「证书指纹不匹配」原文），不会静默接受新证书 |
| 会话计数 | 连接 / 断开 | `activeSessions` 0→1→0，界面数字与实际会话一致 |
| 中转与直连并存 | 直连会话运行中同时用账号中转控制同一台 | 两条会话互不干扰，中转画面照常；`control_out()` 按会话号正确分流 |
| 配对码到期 | 开启直连后等 10 分钟 | 界面自动换成新码（`keep_pairing_fresh`），旧码派生的 token 立刻被拒；不会卡在「已失效」 |
| 已授权设备 | 设置页「局域网直连 → 已授权设备」 | 配对成功后显示 `1 台`；点「清除授权」二次确认后归零，对端再连提示需要配对码 |
| 单元 / 集成测试 | `cargo test -p screenwall-agent --test direct` | 当时 9 passed：`auth_promote_and_replace`、`promote_notifies_hook_once`、`revoke_pairing_keeps_promoted_token`、`pairing_token_expires`、`auth_limiter_backs_off_after_repeated_failures`、`direct_without_approval_is_rejected`、`direct_wrong_token_is_rejected`、`direct_end_to_end`、拒绝授权补 `closed`。此后新增 `direct_welcome_caps_are_intersection` 等用例，当前用例数见代码 |
| 诊断脱敏 | `cargo test -p screenwall-client logging` | `export_scrubs_peer_tokens` 通过：`knownPeers[].token` 与 `directTokens[]` 都变成 `<已脱敏>`，地址与设备名保留 |
| 全量测试 | `cargo test --workspace` | 214 passed / 0 failed（2026-11-25 实测） |
| CI | `powershell -File scripts\ci.ps1` | 六步全 PASS（fmt / clippy / test / tsc / build） |

## v0.2.7（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 控制页空白（回归） | v0.2.4 的页面保活只渲染 `visited` 集合，`openDevice()` / 登录进控制页走裸 `setPage("control")` 没登记，点设备进控制页整块空白（顶栏「远程控制」、内容区空）。修法：任何切页都登记 `visited`，且当前页始终在渲染集合里。浏览器演示模式 A/B 复现：回退 `App.tsx` 必现空白，恢复后正常渲染 |
| 局域网直连画面 | 直连本来就支持画面与键鼠（`crates/client/src/direct.rs` 监听 → `screenwall_agent::direct` → `cloud::session_task`）；`crates/agent/src/session.rs` 里「直连不提供画面/键鼠」只属于独立 Agent 服务入口，不是客户端直连。`direct_end_to_end` 10/10 通过（含 `video.sub` 回包） |
| 检查更新 | 局域网模式（无服务器）不再把 GitHub 失败静默成「已是最新」；GitHub 请求单独 30s 超时并如实报网络原因 |
| 侧栏裁切 | 「这台电脑」「局域网模式」副标题在 212px 侧栏里硬切；文案缩短 + `.side-copy-text` 省略号层，完整说明进 `title` |
| 缩放性能 | `fast_image_resize`（SIMD）替换纯 Rust 浮点双线性；release 实测 4K→1080p 由 32–48 ms 降到 9–15 ms，SIMD 路径失败自动回退。JPEG 编码 1080p Q65 约 12.5 ms/帧（`jpeg-encoder` SIMD 4:2:0） |
| 服务端一键安装 | `scripts/deploy-server.sh` 重写：自动修复异常 `/dev/null`、依赖安装 3 次重试、tarball 优先（git 兜底、镜像切换）、支持 `--yes --account --password [--port] [--allow-register]` 无交互安装 |

## v0.2.8（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 帧率自动档 | `CaptureOptions.fps = 0` 表示不设上限（`MAX_FPS = 240`）：DXGI 用 4ms 抓帧超时，实际节奏跟显示器刷新率走；GDI 无变化检测，自动档兜底 60fps。节奏每轮重算，DXGI→GDI 回退后自动收回。UI 档位 `自动/10/15/30/60/90/120/144`，`set_stream_options_impl` 允许 `0..=240` |
| 网络自适应 | `adapt_loop` 改为 RTT + 帧率双信号：以本会话最小 RTT 为基线，`> 1.8× + 25ms` 判拥塞；先沿画质 `75→65→55→48→42` 降，到底再降分辨率 `3840→2560→1920→1280`；连续 6 个健康窗口（约 12s）才回升，且不超过用户手选的分辨率（`Shared.user_max_width`）。三条链路都按真实路径时延判定 |
| 画质偏好记忆 | 分辨率/帧率/画质存 `localStorage["screenwall.stream-options"]`；会话建立后前端补推一次（后端新会话按默认 1080p/30 起订阅），推失败回退默认并提示。显示器索引每次新会话回主屏 |
| P2P 传输 | 三处 WebSocket 打开 `disable_nagle`；QUIC `stream_receive_window` 1.25MB→8MB、`send_window` 32MB、保活 10s→5s、拥塞控制 Cubic→BBR；`connect_direct` 改为 TCP 与打洞并行（打洞延迟 1.2s 起步，谁先成功用谁），跨网段不必先等 TCP SYN 重试 |
| 测试 | 全工作区 313 passed / 0 failed；UI `tsc --noEmit` + `vite build` 通过 |

## v0.2.9（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 同账号免确认 | 服务器转发 `open` 时带 `"sameAccount": restricted.is_none()`；客户端授权回调在「允许被控」总开关之后按它直接放行。连接码会话为 `false`、局域网直连不带该字段，仍然逐次弹窗。「允许被控」关闭时任何会话照样拒绝。群控不再逐台点确认 |
| 连接方式胶囊 | 左上角显示当前真实链路：账号模式「服务器中转」、局域网模式「局域网直连」（开=绿/关=灰）、连接码会话「连接码」，悬停显示服务器地址或开关状态 |
| 沉浸式全屏 | 全屏只留被控端画面：`body.is-immersive` 收起侧栏、顶栏、控制页工具栏与右侧信息栏，底部操作条绝对定位到右下角、半透明，悬停恢复；Esc 或操作条「全屏」退出 |
| 窗口拖动 | 顶栏与侧栏品牌区改回 `data-tauri-drag-region`，子元素 `pointer-events: none`，修掉「左键长按拖不动窗口」 |
| 注册重名提示 | `POST /api/v1/register` 的 409 之前被统一映射成「设备刚被其他端修改过」；新增 `username_taken` 文案优先匹配「用户名已存在」，并自动切到登录 tab |
| 服务端管理脚本 | `scripts/deploy-server.sh` 改为六趣式横幅菜单（安装/更新/状态/日志/重启/备份/卸载），交互一律从 `/dev/tty` 读，`curl \| bash` 下也能提问 |
| 工具链 | `rust-toolchain.toml` 的 `channel` 不再钉死 `stable-x86_64-pc-windows-msvc`（Linux 服务器上会报 `target tuple in channel name`），安装脚本另加 `normalize_toolchain` 兜底修正 `~/.rustup/settings.toml` |
| 测试 | `group.rs` 断言 `sameAccount=true`、`quick.rs` 断言 `false`；`cargo check` 三个 crate、集成测试 3 passed、`tsc --noEmit`、`vite build` 通过 |

## v0.2.10（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 检查更新误报登录失效 | 版本接口 `/api/v1/version` 本免登录，旧代码却带账号令牌，令牌过期后服务器回 401、界面映射成「登录已失效」。修复：检查更新不再带令牌；服务器 404（未配置 `[update]`）/401/403（前置反向代理或鉴权）按「这一源没有信息」处理，交给 GitHub 那一路；两个源同时失败时把两个原因都显示 |
| 测试 | `cargo test -p screenwall-client` 60 passed（含 `merge_update_sources` 新增断言） |

## v0.2.11（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 同账号 P2P 直连 | 账号会话不再固定走服务器中转：主控端建立会话前先经服务器递一份 `p2p.offer`（候选地址），被控端回 `p2p.answer`（候选 + QUIC 证书指纹 + 一次性 token），两端同时 UDP 打洞，打通后把打通的 socket 交给 QUIC，在 QUIC 流上跑**和局域网直连完全相同**的 WebSocket 会话协议（`direct::serve_stream`），采集/输入/文件/音频/剪贴板/剪贴板图片全部原样复用。失败（旧版本、对称 NAT、对端关闭）静默回退中转。协议见 `docs/server.md` 3.1 |
| 服务端信令 | `p2p` 文本消息由服务器转发：主控端 → 服务器 → 被控端 → 服务器 → 主控端。校验与 `open` 相同（同账号、允许被控、在线、连接码只能打自己那台），应答只能对应「登记过的 `(viewerId, request)` + 同一台设备」且只能应答一次，90 秒未应答自动作废，主控连接断开时清空；字段逐项重建，不原样透传 |
| 免确认与安全 | P2P token 只进被控端为 P2P 单独维护的临时白名单（3 分钟、`mark_auto`），因此同账号会话仍然免「允许控制」弹窗，但「允许被控」总开关照旧优先；局域网直连与连接码会话不受影响，仍然逐次确认。主控端按 `answer` 里的证书指纹固定校验，不是首次信任 |
| 打洞体验 | 新增 `nat::discover_within` / `PunchSocket::discover_within`：STUN 探测带总预算（P2P 路径 3 秒），静默丢 UDP 的网络不会再把「点连接」拖十几秒；`STUN_SOCKET` 读超时按剩余预算收紧。被控端对同一时刻的打洞请求限 2 条 |
| 界面 | 控制页「连接方式」新增「P2P 直连」，副标题写「画面与输入走 P2P 直连（UDP 打洞），不经服务器中转」；`ControlInfo` / `get_control_info` 新增 `link`（`relay` / `direct` / `p2p`），重连后也能显示真实链路 |
| 路由修正 | 控制帧的路由改为按「会话有没有链路登记」判断，而不是按设备 ID 前缀：同账号 P2P 的设备 ID 仍是账号里的设备，但帧必须走链路。服务器发来的 `closed` 不再误收链路会话，服务器断线也不会收掉 P2P 会话 |
| 版本门槛 | 主控端按设备列表里的版本号判断（≥ 0.2.11 才发 `p2p.offer`），旧被控端完全不受影响，也不会白等一轮超时 |

## v0.2.12（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 检查更新永远「已是最新」 | 两个原因叠加：① 客户端 HTTP 客户端没设 User-Agent（reqwest 默认不发），GitHub REST API 直接回 403 `Request forbidden by administrative rules`，GitHub 这一路其实一直失败；② 服务器没配置 `[update]` 时 `/api/v1/version` 回 404，被当成「这一源说已是最新」，于是 GitHub 那路的失败被静默吞掉。哪怕 GitHub 上已经发了新版，界面也一直显示「已是最新版本」 |
| 修法 | ① 客户端 HTTP 客户端统一加 `User-Agent: ScreenWall/<版本>`；② 更新检查改成三种结论 `Newer` / `Latest` / `Unknown`：「没有更新」和「这一源回答不了」分开，只有至少一源明确说「已是最新」时才按已是最新处理，否则把失败原因如实显示出来 |
| 测试 | `cargo test -p screenwall-client --bins` 62 passed，新增 `unknown_source_does_not_hide_the_other_failure` |

## v0.2.13（2026-11-26）

| 项目 | 内容 |
| --- | --- |
| 切页卡顿 | 控制页被保活在 DOM 里（`display:none`）时，4K MJPEG 仍每秒几十帧经 Tauri 事件送进 WebView，base64 编码与主线程解析把点击卡住好几秒。新增 `set_control_video_muted`：页面不可见时后端在帧循环里直接跳过编码与 `emit`，回到页面立即恢复并重置断流计时；H.264 恢复时补一个关键帧。屏幕墙缩略图同样在页面隐藏时停订阅 |
| 点控制要等十几秒 | 旧服务器（< 0.2.11）不认识 `p2p` 信令，主控端发出 `p2p` 后白等 8 秒才回退中转，被控端的「允许控制」弹窗也就晚十几秒才出现。客户端连接后请求 `/healthz` 缓存服务器版本，低于门槛时直接跳过打洞 |
| 同账号仍弹窗 | 根因在服务器：`sameAccount` 是 v0.2.9 起服务器才转发的字段。客户端把服务器版本显示在设置 → 关于与侧栏提示里，低于 0.2.11 时明确提示「升级服务器后才能 P2P 直连、同账号免确认」 |
| 侧栏链路胶囊 | 账号模式下原来固定写「服务器中转」。现在订阅 `control-state` 的 `link`（`relay` / `direct` / `p2p`）：有会话时显示真实链路（P2P 直连为绿色、中转琥珀色），没有会话时按默认路径显示，提示里写清原因 |
| 错误文案 | 4004 原来写「对方关闭了被控 / 已关闭允许被控开关」，但被控端拒绝或超时也用同一个码，文案与实际原因对不上。改成「被控端拒绝了请求」，同时覆盖开关关闭与人工拒绝 |

## v0.2.14（2026-11-27）

| 项目 | 内容 |
| --- | --- |
| 画面糊 | 硬件 H.264 的码率单位接错：`h264_bitrate` 返回 kbps（1080p30 = 4000），`Encoder::new` 收的是 bps 并钳到 ≥ 500_000，于是实际一直按 500 kbps 编码（日志里「重建…bitrate=4000」后面跟着「已初始化 bitrate=500000」）。现在按 ×1000 传入，1080p30 默认 4 Mbps，4K 按帧率折算到 12.5 Mbps |
| STUN 永远失败 | `resolve` 只取 DNS 第一个地址，Cloudflare / Google 的 STUN 域名都先返回 IPv6，而打洞 socket 绑的是 IPv4，`send_to` 直接报地址族不支持；轮到 stun.qq.com 又超时，最后只剩「STUN 响应里没有映射地址」。改为只取 IPv4、逐个尝试解析结果，并补一台 stun.miwifi.com；错误文案区分「UDP 被拦截」和「解析不出映射」 |
| 打洞秒失败（10040） | 被控端还在 64 字节缓冲里等探测包时，主控端已经打通并直接发来约 1.2 KB 的 QUIC Initial，Windows 返回 `WSAEMSGSIZE`（10040），被当成致命错误。接收缓冲扩到 4 KB，超长包按「已打通」处理；`await_peer` 收到 QUIC 包也直接放行（quinn 会重传 Initial） |
| 测试 | `nat::stun_discovers_public_mapping`（联网，手动 `--ignored` 跑）；`cargo check --workspace --all-targets` 通过 |
