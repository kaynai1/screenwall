# ScreenWall 安卓主控端（原生）

Kotlin + Jetpack Compose 写的原生安卓客户端，只做**主控端**：登录、看设备、远程控制。
不注册为被控端，也不会被别的设备控制。

网络协议与网页端完全一致（`web/src/protocol.ts`、`crates/proto`）：
HTTP 登录 + `/ws/view` WebSocket，会话里的 JSON 与媒体帧都包在 8 字节隧道帧里。

## 功能

- 登录：服务器地址 / 用户名 / 密码，记住上次地址与账号；`POST /api/v1/login`。
  也可以在登录页切到「识别码」，用 9 位识别码 + 连接密码直接连（`POST /api/v1/quick/connect`，
  不需要账号，令牌 15 分钟有效）。
- 首页（对齐 CrossDesk 的手机端形态）：顶部「已连接服务器」状态条、远程桌面直连框、
  最近连接（本地保存，带退出时的画面缩略图）、右上角「全部设备」与设置入口。
- 设备列表：`GET /api/v1/devices`，在线状态、系统、最后在线时间，下拉刷新；
  服务器推送的 `devices` 消息会实时刷新。隐藏安卓端 / 网页端这类查看端，
  可被控的在线电脑排在前面。
- 全局设置页：鼠标模式（相对 / 绝对）、画面偏好（帧率优先 / 画质优先 / 平衡）、
  连接后锁定横屏、服务器切换、本机 ID、隐私与授权、保存 / 清除画面预览、
  本机配对信息、关于、退出登录。
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
首页的「设备 ID 或 IP 地址」目前只支持 9 位识别码，IP 局域网直连（配对码 + 证书指纹）
还没在安卓端实现。

## 构建

需要 JDK 17、Android SDK（compileSdk 34）、Gradle 8.2.1。

```powershell
$env:JAVA_HOME = "D:\tools\jdk17\jdk-17.0.17+10"
D:\tools\gradle-8.2.1\bin\gradle assembleDebug
```

产物：`android/app/build/outputs/apk/debug/app-debug.apk`。

`android/local.properties` 里的 `sdk.dir` 指向本机 SDK，已被 git 忽略。

### 打 Release 包（发到 GitHub Release）

Release 包必须用固定密钥签名，否则已装用户装不上更新。密钥和口令不进仓库：

1. 生成一次密钥（已有就跳过）：

```powershell
keytool -genkeypair -keystore D:\tools\screenwall-android\screenwall-release.jks `
  -storetype PKCS12 -alias screenwall -keyalg RSA -keysize 4096 -validity 10000 `
  -dname "CN=ScreenWall, OU=ScreenWall, O=ScreenWall, C=CN"
```

2. 在 `android/keystore.properties` 写密钥路径与口令（该文件已 gitignore）：

```properties
storeFile=D:/tools/screenwall-android/screenwall-release.jks
storePassword=...
keyAlias=screenwall
keyPassword=...
```

3. 构建并改名：

```powershell
$env:JAVA_HOME = "D:\tools\jdk17\jdk-17.0.17+10"
D:\tools\gradle-8.2.1\bin\gradle assembleRelease
copy app\build\outputs\apk\release\app-release.apk ..\ScreenWall_<版本>_android.apk
```

没有 `keystore.properties` 时构建仍然通过，但产物是 `app-release-unsigned.apk`，
只能本地自测，不能分发。发布时把这个 APK 作为附件传到对应 Release，App 首页
启动就会自己检查并提示更新（只认 Release 里名字以 `.apk` 结尾的附件）。

## 本地联调（不依赖 Windows 被控端）

`scripts/fake-server.mjs` 是一个只有账号接口、一台假设备和假画面/声音的服务器：

```powershell
node android/scripts/fake-server.mjs
```

然后把 App 的服务器地址填 `http://127.0.0.1:8788`，账号密码随便填。
识别码连接固定用 `123456789` + 任意非空密码；设备列表里还会带两台查看端
（安卓端 / 网页端），用来验证列表过滤。模拟器 / 真机
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
