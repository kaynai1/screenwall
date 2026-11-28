# 网页端 / 手机端

网页端是一套纯浏览器应用：不用安装，打开 `https://<服务器>/app` 登录即可远程控制
已被控的电脑；在安卓手机 Chrome / Edge 里「添加到主屏幕」后以全屏 PWA 运行，体验接近
原生 App。它走**账号中转**，不参与局域网直连与 UDP 打洞。

> 安卓 APK 需要 JDK + Android SDK 才能打包，本仓库暂不提供独立安装包；
> 网页端的 PWA 已覆盖手机使用场景（见下文「装到安卓手机」），要独立 APK 的打包路线
> 见「打包成安卓 APK」。

## 能力

| 功能 | 状态 |
|---|---|
| 画面（MJPEG） | 支持，当前被控端的唯一编码 |
| 画面（H.264 / WebCodecs） | 支持：浏览器有 `VideoDecoder` 就请求 `codec:"h264"`，被控端用 Media Foundation 硬编（本分支已合入）；解不出时回退 MJPEG |
| 画质（分辨率 / 帧率 / 压缩档） | 支持，选择会记在本机 |
| 鼠标 / 键盘 | 支持（桌面浏览器按扫描码注入） |
| 触摸 | 支持：单指点击/拖动、长按右键、双指滚动、双指捏合缩放（1–4 倍，放大后单指平移画面）、触控板/绝对两种触摸模式、屏幕键盘 |
| 剪贴板文本 | 双向（取决于浏览器授权） |
| 声音 | 支持：Opus（WebCodecs）优先，不支持时回退 PCM16；可调被控端音量与静音 |
| 多显示器 | 支持：`display.list` 枚举，悬浮菜单切换单屏或全部拼接 |
| 画中画 | 桌面 Chrome/Edge 支持（canvas 捕获流）；iOS Safari 不支持 |
| 全屏 | 支持 |
| 文件传输（上传） | 支持：悬浮菜单里选文件发到电脑，256 KiB 分块 + 8 MiB 流控窗口；网页端不做增量 SHA256，按字节数落盘。下载与目录浏览暂未实现 |
| 屏幕墙 | 支持：设备页右上角入口，每台在线设备一条 `wall` 只读会话（1 FPS / 480px），点卡片进入控制 |
| 群控 | 支持：设备页右上角入口（≥2 台在线设备时出现），逐台确认后各开一条控制会话，缩略流 2 FPS / 640px；任意画面上的触摸、拖动、打字、滚动都按归一化坐标广播到全部会话，长按为右键、双指滚动；停止后统一 `releaseAll`。画面仍是缩略流，精确操作请点进单台控制 |

## 构建与部署

```powershell
cd web
npm install
npm run build
```

产物在 `web/dist`。账号服务器启动时用 `--web-dir` 指过去：

```powershell
.\screenwall-server.exe --config server.toml run --web-dir web\dist
```

然后访问 `https://<服务器>/`（会 307 跳转到 `/app/`）。服务器把 `/app/*` 当静态站点提供，
找不到的路径回落到 `index.html`（SPA 路由）。不传 `--web-dir` 时服务器行为与旧版完全一致。

默认订阅参数是 1280×720 / 30fps / 均衡压缩（MJPEG 不走码率自适应，手机流量下建议在
右上角画质面板里换「省流量」或 15fps）。

**必须用 HTTPS**：WebCodecs、剪贴板读取、全屏、PWA 安装都要求安全上下文（`localhost` 除外）。

## 开发

```powershell
cd web
npm run dev
```

开发服务器在 `1421`，把 `/api` 与 `/ws` 代理到本机 `127.0.0.1:8787`，所以先起一个本地
服务器再打开 `http://localhost:1421/app/`。

## 装到安卓手机

1. 手机 Chrome 打开 `https://<服务器>/app` 并登录一次；
2. 设备页顶部会提示「安装到桌面」：Chrome 支持自动安装时点「安装」，否则按提示在
   浏览器菜单里选「添加到主屏幕」；
3. 从桌面图标启动即为全屏独立窗口（`display: standalone`），没有地址栏。

触摸操作：

- 轻点 = 左键单击；按住拖动 = 拖拽（左键按住移动）
- 长按 0.55 秒 = 右键
- 双指上下滑动 = 滚轮，左右滑动 = 横向滚轮
- 双指捏合 = 放大/缩小画面（放大后单指拖动平移，右上角「还原」回到 1 倍）
- 右下角悬浮菜单 = 切换「绝对 / 触控板」、选显示器、开关声音与音量、画中画、断开
- 触控板模式下：单指拖动移动鼠标、轻点左键、长按右键
- 底部「键盘」打开输入框，输入内容实时以文本注入远端（支持中文输入法）
- 「Esc / 回车」按钮补上手机键盘没有的键

群控：

- 设备页右上角「群控」入口（≥2 台在线且允许被控时出现），勾选设备后开始；
- 每台设备各开一条控制会话，网格里是 2 FPS 缩略流，左上角「操作中」是你当前操作的画面；
- 在任意画面上轻点 / 拖动 / 长按右键 / 双指滚动 / 打字，都会同步到全部设备；
- 顶部键盘按钮打开输入框，输入实时注入所有设备；
- 底部「停止群控」统一释放按键并结束全部会话；进入单台精确操作请先用「屏幕墙」或设备列表点开。

## 装到 iPhone / iPad

1. Safari 打开 `https://<服务器>/app` 并登录一次（必须是 HTTPS，PWA 与 WebCodecs 都要求安全上下文）；
2. 点底部「分享」→「添加到主屏幕」→「添加」；
3. 从桌面图标启动即为全屏运行（`apple-mobile-web-app-capable`），没有地址栏。

iOS 上的差异（都有回退，不影响使用）：

- **H.264**：iOS 16.4+ 的 Safari 有 WebCodecs `VideoDecoder`；解不出来时自动回退 MJPEG，流量与 CPU 会高一些。
- **声音**：`AudioDecoder` 不支持 Opus 时自动改订 PCM16，码率更高但能出声。静音开关在侧边，注意 iPhone 的静音拨片也会让网页静音。
- **画中画 / 全屏**：iOS 的 PWA 不支持 `requestPictureInPicture` 与 `element.requestFullscreen`，按钮会提示不支持；用 Safari 的「添加到主屏幕」本身就是全屏。
- **剪贴板**：读取本机剪贴板需要用户手势（点「粘贴」按钮），iOS 首次会询问。
- **后台**：切到后台或锁屏后 WebSocket 可能被系统挂起；回到页面若显示断开，点设备页的「重连」或控制页的「重试」。

## 打包成安卓 APK

仓库自带 Capacitor 壳（`web/android/`），前端资源打进 APK，WebView 里跑的就是网页端本身。
构建需要 **JDK 17** 和 **Android SDK**（`platforms/android-34` + `build-tools/34.0.0`）：

```powershell
cd web
npm install
npm run build:cap              # base 改成相对路径，资源跟着 APK 走
npx cap sync android
cd android
.\gradlew.bat assembleDebug    # 首次会下载 Gradle 与 AGP 依赖
```

产物 `web/android/app/build/outputs/apk/debug/app-debug.apk`，直接传到手机点安装即可
（debug 签名）。`local.properties` 里写 `sdk.dir=<SDK 路径>`；`ANDROID_HOME` / `JAVA_HOME`
指对也能构建。

壳里的登录页没有默认服务器地址（WebView 的 `https://localhost` 不是服务器），需要手填，
地址会记在本机。`AndroidManifest.xml` 已开 `usesCleartextTraffic`，局域网里用
`http://<电脑 IP>:8787` 也能连；正式部署建议上 HTTPS。

上架要换成自己的 release keystore（`android/app/build.gradle` 的 `signingConfigs`）。
另一条路是 TWA / Bubblewrap：站点能 HTTPS 访问后用
`npx @bubblewrap/cli init --manifest https://<服务器>/app/manifest.webmanifest` 生成。
两条路都是 WebView/Chrome 壳；要真正的本地渲染（不走 WebView）得把解码和输入重写成原生，
像 CrossDesk 那样，工作量大得多，目前不做。

## 实现要点

- `/ws/view` 支持 `?token=` 查询串鉴权：浏览器的 `WebSocket` 构造器不能设置
  `Authorization` 头。HTTP 接口仍然只认 Bearer 头。
- 会话建立后所有应用消息都是 8 字节会话帧（`crates/proto/src/cloud.rs`）包着的 JSON 或
  24 字节媒体帧（`crates/proto/src/frame.rs`），网页端在 `web/src/protocol.ts` 里按同样
  的字节序解码。
- MJPEG 用 `createImageBitmap` 解码，只保留最新一帧（解码跟不上时丢旧帧）；
  H.264 用 `VideoDecoder`（Annex-B，codec 字符串从 SPS 里解析），解不出时自动回退。
- 输入在 `web/src/input.ts` 里按 16ms 批量发送，连续移动只发最后一个。

## 安全提示

- 令牌放在 WebSocket 查询串里，容易被反向代理的访问日志记录。部署时请在 Nginx /
  Caddy 里对 `/ws/view` 的查询串做脱敏，或关闭该路径的访问日志。
- 网页端只持有服务器令牌，不接触被控端的证书指纹；被控端仍然按账号会话规则决定是否需要
  本机确认（`allow_control` / 「允许被控」）。
