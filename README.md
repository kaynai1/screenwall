# ScreenWall

自用的多机屏幕墙远程控制软件：一屏看全、点开即控、群控同步、拖拽传文件、远程开机；同一窗口内还内置服务器管理面板（通过 SSH 管理 Linux 主机）。

技术栈：Rust + Tauri 2 + React。默认形态是**局域网点对点**——两台机器直接互连，不需要账号，也不需要任何服务器；跨网段时才需要可选的账号 + 自建中转服务器。

## 文档

- [总体设计](docs/design.md) — 架构、模块、视频与输入管线、里程碑、风险
- [通信协议](docs/protocol.md) — 应用层 WSS 消息、二进制帧格式、文件与 WoL 流程
- [服务器规格](docs/server.md) — 账号、设备目录、在线状态与会话中转
- [M0 实现说明](docs/m0.md) — 协议、握手与工具链验证
- [M1 实现说明](docs/m1.md) — 账号、设备列表、中转与运行步骤
- [M2 实现说明](docs/m2.md) — GDI 采集、MJPEG 编码、中转画面与单台控制验收
- [M3 实现说明](docs/m3.md) — 键鼠注入、超级屏（虚拟桌面采集）、显示器保活与验收
- [显示器与虚拟屏调研](docs/display.md) — 关屏/无头/超级屏/IDD 驱动与假负载方案
- [界面设计说明](docs/ui.md) — 新版工作台的设计系统、页面结构与浏览器演示模式
- [两台机器试用](docs/two-machines.md) — 同一局域网内起服务器、两台机器互控与直连的完整步骤
- [局域网直连](docs/direct.md) — 同网段不经过服务器直接控制、配对码与安全边界
- [连接码连接](docs/quick-connect.md) — 9 位 ID + 密码、不用账号的跨网连接与安全边界
- [局域网自动发现](docs/lan-discovery.md) — UDP 广播扫描同网段开启了直连的设备
- [服务器管理](docs/server-panel.md) — 内嵌 KayPanel：sidecar + 子 webview 的架构、构建与排障
- [更新与分发](docs/update.md) — 用户在哪里检查更新、你自己在哪里发新版（GitHub / 自建服务器两条路线）

## 快速开始

```powershell
# 1. 前端开发服务器
cd D:\screenwall\ui
npm install
npm run dev
```

```powershell
# 2. 客户端
cd D:\screenwall
scripts\cargo.bat run -p screenwall-client
```

**两台机器在同一网段（推荐）**：两台都装客户端，打开后点登录页的「进入局域网模式」。被控端在「屏幕墙 → 添加设备」里看到本机地址和 6 位配对码，主控端在「设备」页点「直连」填进去即可。全程没有账号、没有服务器，画面和操作只在这两台机器之间传输。详见 [局域网直连](docs/direct.md)。

**跨网段**才需要服务器：

```powershell
cd D:\screenwall
copy server.dev.toml server.toml
scripts\cargo.bat run -p screenwall-server -- run --config server.toml
```

然后在客户端登录页用「账号」或「连接码」走服务器，服务器地址填 `http://127.0.0.1:8787`。

**服务器一键安装（Linux / Ubuntu / Debian）**：一台全新的服务器，用 SSH 登录后执行这一条：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' https://api.github.com/repos/kaynai1/screenwall/contents/scripts/deploy-server.sh | bash
```

运行后先出现 **ScreenWall 管理脚本** 菜单（安装 / 更新 / 查看状态 / 查看日志 / 重启 / 备份数据 / 卸载），选 `1` 安装，然后依次问你 **请输入账号** → **请输入密码（至少 8 位，输入不显示）** → **请输入端口（直接回车用默认 8787）**。之后它会自动修 `/dev/null`、装依赖和 Rust（走国内镜像）、下载源码、编译、建账号、注册 systemd、放行 ufw，最后打印中转地址。装完到云控制台**安全组**放行该端口；以后再执行同一条命令，就能用菜单更新、备份、卸载。

不想交互就一次给全：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' https://api.github.com/repos/kaynai1/screenwall/contents/scripts/deploy-server.sh | bash -s -- --yes --account kay --password '至少8位密码' --port 8787
```

装过之后再运行同一条命令会显示管理菜单（更新 / 状态 / 日志 / 重启 / 备份 / 卸载）。`api.github.com` 不通时的备用地址和完整分步见 [服务器规格](docs/server.md#0-一键部署ubuntu--无域名--直绑公网最快路径)。

## 当前状态

M0 已完成并验证；M1 的服务器、Agent 中转和客户端账号体系已完成。M2 的单台控制真实画面已接入：被控端用 Windows GDI 采集屏幕，缩放到最大宽度 1280 并编码为 MJPEG（默认 10 FPS、质量 65），经服务器 `/ws/view` + `/ws/device` 中转；客户端控制页用 `<img>` 显示实时 JPEG 帧，并显示真实 FPS、码率和分辨率。

M3 已接入键鼠控制与超级屏：被控端通过 Win32 `SendInput` 注入鼠标、滚轮和键盘事件，采集期间用 `SetThreadExecutionState` 阻止显示器休眠。控制页「画面」面板可以逐块切换显示器（默认主屏），也可以选「全部屏幕」让所有显示器按系统排列拼成一张画面；输入坐标始终跟随当前画面，点击落在对应的那块屏上。选中单块屏时优先走 DXGI 桌面复制，多屏拼接走 GDI。本机实测关闭显示器后 GDI 采集仍然正常，鼠标坐标误差 ≤2 像素，键盘与 Esc 注入通过；双屏真机与跨机场景仍需实机验收。完全无头（未接显示器）可以在设置里打开「无头时接入虚拟显示器」：首次会话弹一次 UAC 安装随包携带的 Amyuni usbmmidd_v2 间接显示驱动，插一块 1920×1080 虚拟屏再采集（第三方组件，可能弹一次广告页，卸载时只删自己装的驱动），详见 [显示器调研 7.5 节](docs/display.md#75-本项目的落地无头时自动接入-usbmmidd-虚拟屏)。

M4/M5 已落地：被控端授权（同一账号下的设备免确认直接放行；局域网直连和连接码会话仍然弹窗确认，30 秒超时自动拒绝，设置里可整体关闭「允许被控」）、画面自适应画质、文件传输（上传 / 下载 / 浏览被控端目录，流控 + 失败回滚 + SHA256 校验 + 上传与下载断点续传 + 整目录下载保留层级 + 下载最多 3 个并发排队）、文本剪贴板双向同步、系统声音传输（WASAPI 回环 → Opus 64 kbps 起、按播放缓冲自动在 24–96 kbps 之间调整，WebCodecs 不可用时回退 PCM，控制页「声音」按钮显式开关）、屏幕墙实时缩略图（每台在线设备 1 FPS / 320px 宽，免逐次授权、不写审计）、群控（每台设备一条会话，键鼠一次广播到全部选中设备，卡片实时预览可切换画质）、远程截图（保存当前帧到 `Downloads\ScreenWall`）、远程重启/关机（二次确认，被控端 3 秒后执行）、远程开机（设备页对离线设备发 WOL magic packet：同网段可直接广播，异地可选一台在线设备代发，代发设备需本机确认）、被控端音量与静音联动（`IAudioEndpointVolume`，控制页侧栏滑块）、声音来源切换（系统回环 / 麦克风）、设备管理（重命名、允许控制开关、移除都写服务器）、会话审计页（分页、按设备过滤、进行中会话实时计时）、系统托盘与连接看门狗（关闭窗口缩到托盘，连接持续失败自动重启会话任务）、滚动日志与一键导出诊断包（令牌脱敏）、自托管自动更新（服务器 `/api/v1/version` + 安装包 SHA256 校验）、单安装包（NSIS）。设置页、文件页、屏幕墙、设备页、审计页与群控页都已接真实命令，没有保留假控件。

M6 已落地：协议能力协商（`hello` / `welcome` 的可选 `caps`，15 项能力、缺字段按 v1 基线处理、新增能力不升 `proto`）、真实运行指标（被控端整机 CPU / 内存与会话 kbps 填进 `status.stats`，屏幕墙卡片显示真实占用）、麦克风回声消除（回环参考 + NLMS 自适应 + 双讲检测 + 残留抑制，`SCREENWALL_AEC=0` 可关）、服务端可观测性（`GET /api/v1/metrics` 的并发会话 / 带宽 / 错误率 / 握手成败，`[limits]` 段做设备上限与四路令牌桶限流）、控制会话断线自动重连（1s/2s/4s/8s/8s 五轮退避 + 控制页重连横幅）、统一错误文案（协议码 / 中转码 / HTTP 状态映射成中文）、键盘可达性与焦点陷阱、CI（`.github/workflows/ci.yml` 四个阻塞 job + 本地等价脚本 `scripts\ci.ps1`，实测 fmt / clippy / 214 个测试 / tsc / build 全绿）。

界面视觉与 [KayPanel](https://github.com/kaynai1/kaypanel) 对齐：同一套字体栈、同一组中性层与语义色、同一档圆角（浅色 `#f4f5f7`/`#ffffff`、深色 `#0a0f17`/`#121a26`、强调色 `#2563eb`/`#60a5fa`），浅色与深色双主题在设置 → 连接 → 外观里切换并持久化。样式规则已全部 token 化，页面里不再出现字面色值。字号保持 15px 整数阶梯（KayPanel 的 14.5px 半像素在 Windows 上会糊边，见 [设计文档](ui/DESIGN.md)）。

连接码连接已落地（0.2.0 起，0.2.1 补齐一次性链接）：被控端在 设置 → 安全 里设置密码并打开开关后，**本机连接码直接显示在工作台（屏幕墙）页面顶部**，连同「换一个」和「生成链接」按钮；主控端在登录页选「连接码连接」，输入码和密码就能进入远程桌面，**不需要登录账号**。服务器给主控端的是 15 分钟、只能打开这一台设备的受限令牌，不能调账号接口，也不写盘；密码用 Argon2 哈希存在服务器，明文只留在被控端本机；连接码尝试按 IP 限流、同一码连错 5 次锁 60 秒。跨网段可用，画面仍走自建服务器中转。

一次性连接链接也已落地：工作台顶部点「生成链接」签发一条 `http://服务器/l/<令牌>`，**10 分钟有效、只能用一次**；对方点开后网页唤起 `screenwall://` 协议，客户端被单实例转发并直接连入，不用输码和密码。设备离线不会烧掉链接（返回 409，链接仍可用），落地页只负责唤起、不消费令牌。链接也能直接粘到登录页的连接码输入框。详见 [连接码连接](docs/quick-connect.md)。

局域网直连已落地：同网段的两台机器可以完全不经过账号服务器直接控制。被控端在设置里打开开关后监听 `47900`，界面显示 6 位配对码（10 分钟有效，到期自动换新码，配对成功也立刻换码）；主控端在设备页点「直连」填地址和配对码即可。配对成功后保存长期令牌与证书指纹，之后免配对码、并靠指纹固定防冒充；设置页能看到「已授权 N 台设备」并一键清除。认证失败按来源 IP 限流（60 秒内 5 次后指数退避，封顶 30 秒，超限连 TLS 握手都不做）。协议、能力协商、授权、文件、音频、剪贴板全部复用中转那套代码——被控端本地放一个「服务器桩」把 tunnel 帧直接交给 `session_task`。真机实测 1920×1080 / 30 FPS / 37 Mbps / 零丢帧。详见 [局域网直连](docs/direct.md)。

服务器管理已落地：侧边栏「管理 → 服务器管理」打开的不是另一个应用，也不是浏览器标签，而是**嵌在同一个主窗口内容区里的 KayPanel 面板**。KayPanel（上游 v0.14.1，MIT 许可）的源码 vendor 在 `third_party/kaypanel/`，构建成 `kaypanel.exe` 后随安装包分发；Rust 侧在首次进入页面时拉起这个 sidecar，读取它写在 stdout 的一行握手 JSON（回环随机端口 + 一次性登录令牌 + PID），再用 Tauri 子 webview 把面板铺在内容区。切到别的页面只隐藏、不销毁，主程序退出时子进程读到 stdin EOF 自行结束。面板通过 SSH 管理 **Linux** 主机（目标机器无需安装 Agent），提供系统监控、Docker / Compose、文件管理、Web 终端、告警推送、AI 运维助手、多节点与审计日志；**不能管理 Windows 主机**，首次使用需要在面板里「添加节点」。来源、改动与构建方式见 [服务器管理](docs/server-panel.md)。

`Ctrl+Alt+Del` 已接入：Agent 优先调用 `sas.dll!SendSAS`，被控端不是服务/SYSTEM 且系统策略不允许时降级为普通按键并明确提示（见 [协议 6.5](docs/protocol.md)）。仍未做：商业代码签名（没有证书，安装时 SmartScreen 会提示；自签名方案见 [签名文档](docs/signing.md)）、H.264 / 硬件编码（现在只有 MJPEG）、IPv6 与 UDP 打洞、文件剪贴板（复制粘贴文件）、屏幕墙文件拖拽分发（拖拽只在文件页）。**已完成**：局域网自动发现（被控端 UDP 47901 应答，主控端在「直连」弹窗里一键扫描同网段设备）、图片剪贴板（PNG 双向同步，文本优先）、自定义 CA（设置里填 PEM/DER 或证书目录，配置后只信任该 CA；见 [局域网自动发现](docs/lan-discovery.md)）。**防火墙已自动配置**：安装包是 `perMachine` 模式，NSIS 钩子在安装/卸载时维护 `ScreenWall Direct` 入站规则（只放行 `screenwall-client.exe`）；规则被删或从旧 `currentUser` 版本升级时，设置页会提示并提供一次 UAC 的修复按钮。连不上时的排查顺序见 [局域网直连](docs/direct.md) 的「连不上怎么查」。进度与验收口径见 [交付计划](docs/DELIVERY.md)。

## 打包

```powershell
cd D:\screenwall
scripts\tauri-build.bat build --bundles nsis
```

产物在 `target\release\bundle\nsis\ScreenWall_<版本>_x64-setup.exe`（`<版本>` 取自 `Cargo.toml`；按机器安装到 `Program Files\ScreenWall`，需要管理员；安装器会自动添加局域网直连的防火墙规则）。这个脚本会先进 MSVC 环境，避免 Git Bash 的 `link.exe` 遮蔽链接器。

安装包里还包含服务器管理 sidecar `kaypanel.exe`，因此打包机需要 **Go 1.23+**。`scripts\tauri-build.bat` 发现 `third_party\kaypanel\dist\kaypanel.exe` 不存在时会自动调用 `scripts\build-kaypanel.bat`；也可以先手动构建：

```powershell
scripts\build-kaypanel.bat
```

> 注意：Tauri 的 build script 会在**编译期**检查 `bundle.resources`，所以任何会编译客户端的命令（`cargo test`、`cargo clippy`、`cargo build`）都要求 sidecar 已存在，不只是打包。新克隆的仓库先跑一次 `scripts\build-kaypanel.bat`（CI 也是这么做的）。

首次构建会联网拉取 Go 模块（脚本在未设置 `GOPROXY` 时默认使用 `https://goproxy.cn,direct`）；Go 不在 `PATH` 时可用 `GO_BIN` 指定其 `bin` 目录。KayPanel 是第三方开源组件（MIT），来源与本地改动见 `third_party\kaypanel\THIRD-PARTY-NOTICE.md`。

本地一键检查（fmt / clippy / test / 前端 tsc + build，与 CI 等价）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ci.ps1
```

CI 与代码签名的完整说明见 [CI 文档](docs/ci.md) 和 [签名文档](docs/signing.md)。目前**没有商业代码签名证书**，安装包发布时仍会被 SmartScreen 提示；自签名证书只能证明包未被篡改，消不掉警告，要消除必须买 OV/EV 证书。

### 发布一次更新

**用户在哪里更新**：客户端 → 设置 → 关于 → 软件更新 → 「检查更新」→「下载并安装」。
**你自己在哪里发**：两条路线，按用户用的模式选。

1. 升版本号（三处必须一致）：根 `Cargo.toml` 的 `workspace.package.version`、`ui/package.json`、`crates/client/tauri.conf.json`。
2. 重新打包：`scripts\tauri-build.bat build --bundles nsis`。
3. 发布，二选一或都做：

```powershell
# A. GitHub：局域网模式（没配服务器）的用户从这里拿更新
$v = "0.2.12"   # ← 换成这次要发的版本
gh release create "v$v" "target\release\bundle\nsis\ScreenWall_${v}_x64-setup.exe" --title "ScreenWall $v" --notes "更新说明"
```

```bash
# B. 自建服务器是 Linux（绝大多数情况）：登录服务器，就这一条命令。
#    脚本自己从 GitHub 下载最新正式版安装包 → 放进 [update].dir → 改 server.toml
#    → 重启服务 → 校验 /api/v1/version 真的是新版本。
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/publish-update.sh | bash

# 要指定版本/用服务器上已有的安装包，在末尾加参数：
#   ... | bash -s -- -v 0.2.12
#   ... | bash -s -- -v 0.2.12 -f /root/ScreenWall_0.2.12_x64-setup.exe
```

```powershell
# B'. 服务器跑在 Windows 上（少见）：在服务器本机执行
$v = "0.2.12"   # ← 同上
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\publish-update.ps1 -Version $v
```

> 两段不能混用：`publish-update.ps1` 是 **Windows** 脚本，在 Linux 服务器上跑会报
> `powershell: command not found`；它改写的也是本机 `server.toml`。Linux 服务器用 B，
> 脚本会下载安装包、放进 `[update].dir`、改写配置、重启并自检。

4. B 默认会自动重启服务，重启后客户端点「检查更新」就能看到新版本（加 `--no-restart` 可跳过重启）。

客户端的更新源选择：配了服务器就读 `GET /api/v1/version`，没配（局域网模式）就回退到 GitHub Releases，服务器不可达时也退到 GitHub。**显示「已是最新版本」不是没接上**，而是那个源的版本号不高于当前客户端。

完整说明（含手动更新、校验规则、注意事项）见 [更新与分发](docs/update.md)。

正式使用还需要一台有公网地址的服务器（轻量云 VPS 即可），由 Caddy 提供 HTTPS；本机开发用 `http://127.0.0.1:8787`。
