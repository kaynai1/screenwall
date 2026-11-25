# ScreenWall 界面设计说明

这一版把客户端从「M1 功能验证页」重做成完整的桌面工作台。单台控制已接入真实 MJPEG 画面和键鼠接管，其余页面数据来自 `src/mock.ts` 或 Tauri 后端已有的账号/设备接口；屏幕墙缩略流、文件传输等能力仍待后续接入。

## 设计方向

完整规范见 `ui/DESIGN.md`，这里只记结论：

- Operate 模式：安静、准确、可预期，注意力留给远程画面本身。
- 中性石墨色分层（`--bg` / `--bg-sidebar` / `--surface` / `--surface-2` / `--surface-3`），层级靠边框和第二中性层，不靠阴影。
- 单一强调色 `--accent: #8ea2ff`，只用于焦点、当前项、链接和关键数据；主按钮用近白底深字。
- 语义色仅用于状态：在线 `--ok`、警告 `--warn`、错误 `--danger`。
- 一个字体家族、固定字号阶梯（11/12/13/14/16/20/26），数字使用 `tabular-nums`。
- 圆角 6/8/12/16；只有浮层使用阴影；动效 160ms，不做页面入场编排。
- 禁止玻璃、彩色光晕、渐变文字/按钮/进度条、扫描线、eyebrow 小标签、emoji 图标。

设计变量集中在 `ui/src/styles.css` 的 `:root` 中，页面自己的样式放在各自的 `PageName.css`，统一使用页面前缀，避免互相覆盖。

## 页面结构

```text
登录 / 注册
└─ 应用外壳
   ├─ 侧栏：工作台（屏幕墙、群控、文件传输）/ 管理（设备、审计、设置）
   ├─ 顶栏：页面标题、连接状态、通知、账号
   └─ 内容区
      ├─ 屏幕墙：设备预览卡片、搜索筛选、多选、添加设备
      ├─ 单台控制：远程舞台、会话统计、快捷操作、底部工具栏
      ├─ 群控：设备选择、同步画面网格、键鼠广播开关
      ├─ 文件传输：目标设备、拖拽区、传输队列、历史记录
      ├─ 设备管理：统计、设备列表、重命名/删除、允许控制
      ├─ 审计：会话概览、按设备过滤、搜索、分页
      └─ 设置：账号、连接（含托盘与诊断）、安全、关于
```

## 关键文件

| 文件 | 作用 |
| --- | --- |
| `ui/src/App.tsx` | 应用状态、页面切换、登录/退出、Toast |
| `ui/src/styles.css` | 设计变量、外壳、登录页、共享组件 |
| `ui/src/nav.ts` | 侧栏导航与页面标题 |
| `ui/src/api.ts` | Tauri 命令封装；浏览器下自动进入演示模式 |
| `ui/src/mock.ts` | 演示设备、格式化工具 |
| `ui/src/components/DesktopPreview.tsx` | 无真实帧时的设备画面占位（不模拟桌面内容） |
| `ui/src/components/ui.tsx` | Switch、Segmented、Progress、EmptyState、Modal |
| `ui/src/pages/LoginPage.tsx` | 登录 / 注册 |
| `ui/src/pages/WallPage.tsx` | 屏幕墙 |
| `ui/src/pages/ControlPage.tsx` | 单台控制 |
| `ui/src/pages/GroupPage.tsx` | 群控 |
| `ui/src/pages/FilesPage.tsx` | 文件传输 |
| `ui/src/pages/DevicesPage.tsx` | 设备管理 |
| `ui/src/pages/AuditPage.tsx` | 会话审计（`ui/src/audit.ts` 封装命令与格式化） |
| `ui/src/pages/SettingsPage.tsx` | 设置（诊断卡片见 `components/DiagnosticsCard.tsx`） |

## 浏览器演示模式

直接用 `npm run dev` 打开 `http://localhost:1420` 时没有 Tauri 环境：

- 默认以演示账号 `kay` 登录，直接展示完整界面。
- 登录页的任意用户名/密码都可以进入。
- 退出登录会写入 `localStorage` 的 `screenwall.demo.loggedOut`，刷新后停留在登录页。
- 要回到演示主界面，在控制台执行 `localStorage.removeItem("screenwall.demo.loggedOut")` 后刷新。

Tauri 客户端仍调用原来的 `get_account`、`list_devices`、`login`、`logout` 命令，接口没有变化。

## 验收截图

在真实 Tauri 客户端（1280×820）里截取：

- [登录 / 注册](../ui/screenshots/login.png)
- [屏幕墙](../ui/screenshots/wall.png)
- [单台控制](../ui/screenshots/control.png)
- [群控](../ui/screenshots/group.png)
- [文件传输](../ui/screenshots/files.png)
- [设备管理](../ui/screenshots/devices.png)
- [设置](../ui/screenshots/settings.png)

## 后续接入点

- 屏幕墙卡片：仍是 `DesktopPreview` 预览，尚未接入真实缩略流。
- 单台控制：舞台是经服务器中转的真实 MJPEG 帧（M2），画面已支持键鼠接管与远程光标（M3）；后续再换 WebCodecs / H.264 解码，并接入剪贴板与音频。
- 群控：网格仍是预览，多路解码和广播输入尚未接入。
- 文件传输：仍是演示，后续接 `files` 协议、真实进度和断点续传。
- 设备管理：重命名、删除、Wake-on-LAN 仍是演示，后端命令待接入。
- 设置：本地开关还未接到 `client.json` 的实际配置。

## 连接体验与可访问性（追加）

### 连接质量浮层

控制页顶栏新增「质量」入口（`Activity` 图标）。数据全部来自真实到达的帧头，不做估算：

- 视频：收到帧数、按 `seq` 缺口计算的丢帧数、最近 5 秒 FPS 与码率（kbps）、本次会话内最大帧间隔（ms）。
- 音频：收到包数、按包序号缺口计算的丢包数、相邻两包解码时间戳之差 > 40 ms 的「空档」次数。
- 网络：本次会话内自动重连成功次数、最近一次隧道 Ping 的 RTT。

统计器在 `crates/client/src/quality.rs`（纯 Rust，带单测：`seq` 回绕、重复/迟到帧、重新订阅后重基准）。
Rust 侧同时通过 `control-stats.quality` 每秒推送一份快照；`get_connection_quality(clientToken)` 命令供前端按需拉取。
丢帧率 = 丢帧 ÷（收到 + 丢失），超过 **2%** 时入口变警示色并显示百分比徽标。

### 断线自动重连

`start_control` 建立的会话在**非用户主动关闭**时（网络断开、服务器踢线、被控端结束会话）自动重连：

| 项 | 值 |
| --- | --- |
| 退避 | 1s → 2s → 4s → 8s → 8s |
| 最多尝试 | 5 次 |
| 恢复内容 | 视频订阅（沿用档位/显示器）、声音订阅（沿用 codec/source）、远端音量、剪贴板同步 |
| 取消条件 | `stop_control`、离开控制/文件页、退出登录、换账号、主控 WebSocket 断开 |

重连期间控制页顶部显示「连接已断开，正在重连（n/5）…」横幅并禁用键鼠注入；
成功后横幅换成短暂的「已恢复」，5 次都失败则换成「重连失败」+「重试」按钮（调用 `start_control` 重新开始）。
主控 WebSocket 断开时不会空转重试：计划暂存在客户端，服务器连回来后才真正开始。

### 错误文案

`ui/src/errors.ts` 把协议错误码（`crates/proto/src/lib.rs` 的 `codes`）与服务器中转错误码（42xx）
映射成 `{ title, detail, action? }`，并提供 `describeError(input)` / `errorLine(input)`：

- 纯错误码：`"4100"`；
- 「码: 文案」：`"4200: device offline"`（Rust 侧事件与 `open.err` 都是这种格式）；
- 任意异常对象或字符串（含 `Error: invoke failed: ...`、HTTP 错误体）。

所有页面的 `catch` 都改走这两个函数，界面上不再出现 `Error: invoke failed: 4100`。
Rust 侧 `crates/client/src/errors.rs::user_message` 负责把 `anyhow` 的上下文链收成最后一段人话，
用在 `view_loop` 的账号错误与 `test_server` 这类直接暴露链路的地方。

### 焦点与键盘可达性

- 全局 `:focus-visible` 焦点环统一用 `--accent-strong`，按钮/卡片/分段控件/导航项都有可见焦点态，不使用 `outline: none` 抹掉。
- 共用 `Modal`（`ui/src/components/ui.tsx`）打开时把焦点移进弹窗（第一个可操作元素），`Tab`/`Shift+Tab` 在弹窗内循环，`Esc` 关闭，关闭后焦点回到触发按钮。
- 图标按钮补齐 `aria-label`；列表行仍用 `role="button"` + `tabIndex={0}` + 回车/空格。
- 控制页：点画面进入键鼠捕获，捕获期间 `Tab` 与方向键都发给远端；连按两次 `Esc` 或点「释放键盘」退出捕获，焦点交还给「释放键盘」按钮。
  未捕获时 `Tab` 走本地焦点（键盘用户不会被困在画面区），方向键仍转发；焦点在 `input`/`textarea`/`select`/`contenteditable` 里时永不捕获。
