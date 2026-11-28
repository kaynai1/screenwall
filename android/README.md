# ScreenWall 安卓主控端（原生）

Kotlin + Jetpack Compose 写的原生安卓客户端，只做**主控端**：登录、看设备、远程控制。
不注册为被控端，也不会被别的设备控制。

网络协议与网页端完全一致（`web/src/protocol.ts`、`crates/proto`）：
HTTP 登录 + `/ws/view` WebSocket，会话里的 JSON 与媒体帧都包在 8 字节隧道帧里。

## 功能

- 登录：服务器地址 / 用户名 / 密码，记住上次地址与账号；`POST /api/v1/login`。
- 设备列表：`GET /api/v1/devices`，在线状态、系统、最后在线时间，下拉刷新；
  服务器推送的 `devices` 消息会实时刷新。
- 控制会话：`open`（control 模式）→ `hello` → `video.sub`（MJPEG；请求 H.264
  时被控端返回 4104 会自动回退 MJPEG）。
- 画面：MJPEG 用 `BitmapFactory` 解码到自定义 View；H.264 用 `MediaCodec`
  硬件解码输出到 `TextureView`（等关键帧、解析 SPS/PPS）。
- 输入：单击左键、拖动按住移动、长按右键、双指上下滚轮、双指捏合 1–4 倍缩放
  （放大后单指平移）；绝对定位 / 触控板两种模式。
- 键盘：系统输入法文字注入（`text` 事件，带退格差异计算），Esc/Tab/回车/方向键/
  Win/Ctrl+Alt+Del 快捷键。
- 会话设置：画质、分辨率、帧率、H.264 开关、声音（Opus/PCM16）、显示器切换、断开。
- iOS 风格界面：分组卡片、#007AFF 强调色、系统字体、跟随系统深浅色。

中继直连（WebRTC P2P）留待后续版本；当前走服务器中转，不影响登录与控制。

## 构建

需要 JDK 17、Android SDK（compileSdk 34）、Gradle 8.2.1。

```powershell
$env:JAVA_HOME = "D:\tools\jdk17\jdk-17.0.17+10"
D:\tools\gradle-8.2.1\bin\gradle assembleDebug
```

产物：`android/app/build/outputs/apk/debug/app-debug.apk`。

`android/local.properties` 里的 `sdk.dir` 指向本机 SDK，已被 git 忽略。

## 本地联调（不依赖 Windows 被控端）

`scripts/fake-server.mjs` 是一个只有账号接口、一台假设备和假画面/声音的服务器：

```powershell
node android/scripts/fake-server.mjs
```

然后把 App 的服务器地址填 `http://127.0.0.1:8788`，账号密码随便填。模拟器 / 真机
上先做端口反向：

```powershell
D:\android-sdk\platform-tools\adb.exe reverse tcp:8788 tcp:8788
```

测试画面默认读取系统临时目录 `sw-fake-frames` 下的 `frame*.jpg`；没有图片时
`video.sub` 会返回 4104，和当前 main 分支真被控端的行为一致。

## 目录

- `app/src/main/java/com/screenwall/android/net/` — 隧道帧 / 媒体帧编解码、`/ws/view` 客户端
- `app/src/main/java/com/screenwall/android/session/` — 控制会话状态机
- `app/src/main/java/com/screenwall/android/video/` — MJPEG / H.264 解码
- `app/src/main/java/com/screenwall/android/input/` — 输入事件构建与 16 ms 合批
- `app/src/main/java/com/screenwall/android/audio/` — Opus / PCM16 播放
- `app/src/main/java/com/screenwall/android/ui/` — Compose 界面
- `scripts/make-icons.ps1` — 生成三色竖柱图标
