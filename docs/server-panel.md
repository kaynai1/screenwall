# 服务器管理（内嵌 KayPanel）

侧边栏「管理 → 服务器管理」打开的是 [KayPanel](https://github.com/kaynai1/kaypanel) 面板。它**内嵌在 ScreenWall 的主窗口里**，不是另开一个应用、不是浏览器标签，也不是 iframe。

KayPanel 本身是一个轻量级多服务器管理面板：一个二进制，通过 SSH 统一管理多台 **Linux** 主机，目标机器上不需要安装 Agent。功能包括系统监控、Docker / Compose、文件管理、Web 终端、告警推送、AI 运维助手、多节点与审计日志。

本文讲清楚三件事：为什么这样集成、它怎么跑起来、坏了怎么查。构建与打包的总流程见 [交付计划](DELIVERY.md)，CI 相关见 [CI 文档](ci.md)。

---

## 1. 为什么要合并

ScreenWall 原来只解决 Windows 机器之间的画面、键鼠与文件。实际使用中还有另一类需求：手上若干台 Linux 服务器（VPS、家里的小主机、容器宿主机）需要看负载、重启容器、翻日志、改配置文件。过去要单独打开 KayPanel 的桌面版或网页版，等于维护两套入口。

目标是把这件事收进同一个窗口：

- 同一个任务栏条目、同一个托盘、同一个安装包；
- 从「屏幕墙」切到「服务器管理」只是一次页面切换；
- 不改变 KayPanel 已有的功能与操作习惯。

---

## 2. 为什么用 sidecar，而不是用 Rust 重写

KayPanel 是 Go 项目，包含 SSH / SFTP / PTY、Docker 与 Compose 调用、告警推送、AI 接口调用、登录鉴权、CSRF 防护、审计日志以及整套前端静态资源。

| 方案 | 代价 |
|---|---|
| 用 Rust 重写 | 数周工作量；鉴权、命令执行、文件操作等高风险路径要逐条重测；上游更新无法合并；功能容易逐渐分叉 |
| sidecar 运行原二进制 | 安装包多一个约 9.4 MiB（未压缩）的可执行文件；打包机多一套 Go 工具链；需要管理子进程生命周期 |
| iframe 嵌入原网页 | 行不通，见下一节 |

选择 **sidecar**：

- 上游源码 vendor 在 `third_party/kaypanel/`（Go 1.23，MIT，v0.14.1），`go.sum` 一并保留，构建可复现。
- 只改 `main.go`，增加一个 `-sidecar` 模式；业务逻辑、鉴权逻辑、前端资源都不动。
- 将来同步上游版本时，冲突面只有 `main.go` 里那一小段。
- 即使 sidecar 崩了，影响范围也限于服务器管理页，不会拖垮主程序的画面、控制与中转。

代价是两条构建链（Rust + Go）和一次进程管理，但这些都是可控的工程成本；重写则要长期承担功能对齐与安全回归的成本。

---

## 3. 为什么不用 iframe

最初最省事的想法是：把 KayPanel 起在本地端口，用 `<iframe src="http://127.0.0.1:...">` 嵌进 React 页面。实际不可行：

- KayPanel 的响应头带 `X-Frame-Options: DENY`，浏览器会直接拒绝在 iframe 中渲染，表现为白屏。
- 会话 cookie 是 `SameSite=Strict`，跨上下文嵌入时也拿不到登录态。

改上游去掉这两个头会削弱它的安全边界，也增加后续合并成本。因此改用 Tauri 的**子 webview**：

- 子 webview 是独立的浏览上下文，不是被嵌入的 frame，不受 `X-Frame-Options` 与 `SameSite` 的嵌入限制。
- 它与主界面的 CSS / JS 天然隔离，不会互相污染。
- 可以指定独立的 WebView2 数据目录，把面板的 cookie 与主窗口分开。
- 仍然是同一个原生窗口里的一块区域，用户感知上就是一次页面切换。

---

## 4. 架构与数据流

### 4.1 组件

| 组件 | 位置 | 职责 |
|---|---|---|
| KayPanel 源码 | `third_party/kaypanel/` | 上游 v0.14.1，MIT；`main.go` 增加 `-sidecar` |
| 构建脚本 | `scripts/build-kaypanel.bat` | 产出 `third_party/kaypanel/dist/kaypanel.exe` |
| 打包配置 | `crates/client/tauri.conf.json` | `bundle.resources` 把 `kaypanel.exe` 放进安装包 |
| 进程与 webview 管理 | `crates/client/src/panels.rs` | 拉起子进程、读握手、创建/摆位/隐藏子 webview、退出清理 |
| 页面 | `ui/src/pages/ServerPage.tsx` | 量内容区位置、通知 Rust、加载/错误态 |
| 导航 | `ui/src/nav.ts` | 「管理」分区 →「服务器管理」，`PageKey = "server"` |
| 许可声明 | `third_party/kaypanel/THIRD-PARTY-NOTICE.md` | 上游来源、版本、许可与本地改动 |

### 4.2 数据流（文字版）

```
[用户点击侧边栏「服务器管理」]
              │
              ▼
[ServerPage.tsx 挂载]
  量出内容区在窗口中的矩形（CSS 像素，避开侧边栏与顶栏）
              │ invoke("show_server_panel", {bounds})
              ▼
[panels.rs :: ensure_started]
  已有存活实例？──是──> 直接复用之前的握手信息
              │否
              ▼
  spawn kaypanel.exe -sidecar -config %APPDATA%\ScreenWall\kaypanel\config.yaml
  stdin = 管道, stdout = 管道, stderr = null
              │
              ▼
[kaypanel.exe]
  强制监听 127.0.0.1:0（系统分配随机端口）
  生成一次性登录令牌
  stdout 输出唯一一行握手 JSON：
  {"url":"http://127.0.0.1:<port>","token":"<one-time-token>","pid":<pid>}
  日志全部写 stderr
              │
              ▼
[panels.rs 读取第一行]
  拼出 http://127.0.0.1:<port>/api/desktop/login?token=<one-time-token>
              │ Window::add_child(...)
              ▼
[子 WebView2（label = kaypanel-embed）]
  独立数据目录 %APPDATA%\ScreenWall\kaypanel\webview2
  加载面板，自动登录
              │ HTTP，仅回环
              ▼
[kaypanel.exe] ── SSH ──> [Linux 主机，无需 Agent]
```

页面切换与窗口缩放：

```
离开「服务器管理」 ──> invoke("hide_server_panel") ──> webview.hide()（进程与 webview 都保留）
窗口缩放         ──> 重新量矩形 ──> invoke("resize_server_panel", {bounds})
```

### 4.3 握手协议

sidecar 模式下的约定：

- **监听地址**：永远是 `127.0.0.1:0`，忽略配置里的 `-listen`，避免用户误把面板暴露到局域网。
- **握手**：启动后向 stdout 写**且只写一行** JSON：

  ```json
  {"url":"http://127.0.0.1:49152","token":"<一次性令牌>","pid":12345}
  ```

  `token` 是面板内部随机生成的一次性桌面登录令牌，只经父子进程之间的私有管道传递，不出现在命令行参数或环境变量里。
- **日志**：全部走 stderr。ScreenWall 把子进程 stderr 接到空设备，防止日志写满管道把子进程卡住；需要看日志时手动运行（见 7.5）。
- **退出信号**：Rust 持有子进程的 stdin。父进程正常退出、崩溃或被关闭时，管道关闭，子进程读到 EOF，走 KayPanel 自己的 5 秒优雅关闭流程。

### 4.4 子 webview 的两个关键细节

1. **独立 WebView2 数据目录。** 主窗口通过 `additionalBrowserArgs` 创建了共享 user data folder 的 environment；第二个 webview 用不同参数复用它时，WebView2 返回 `HRESULT(0x8007139F)`（`ERROR_INVALID_STATE`）。因此面板单独使用 `%APPDATA%\ScreenWall\kaypanel\webview2`，同时也隔离了登录 cookie。
2. **Tauri `unstable` feature。** 子 webview 通过 `Window::add_child` 创建，这个 API 需要 tauri 的 `unstable` feature（`crates/client/Cargo.toml` 已启用）。

---

## 5. 构建与打包

### 5.1 依赖

- Go **1.23+**
- Rust / MSVC（ScreenWall 本身）
- Node / npm（前端）

### 5.2 命令

```powershell
cd D:\screenwall
scripts\build-kaypanel.bat
```

产物：

```
third_party\kaypanel\dist\kaypanel.exe
```

脚本会先 `go mod verify`，再用 `go build -trimpath -ldflags "-s -w -H windowsgui -X main.version=0.14.1"` 构建。`-H windowsgui` 让它作为后台进程运行、不弹控制台窗口。

然后正常打包：

```powershell
scripts\tauri-build.bat build --bundles nsis
```

`tauri-build.bat` 在发现 `dist\kaypanel.exe` 不存在时会自动调用 `build-kaypanel.bat`。Go 缺失时打包直接失败，不会静默产出缺少面板的安装包。

### 5.3 网络与工具链环境变量

- 脚本在未设置 `GOPROXY` 时默认 `https://goproxy.cn,direct`（`proxy.golang.org` 在部分网络不可达）。
- Go 不在 `PATH` 时，用 `GO_BIN` 指向 Go 的 `bin` 目录。
- 首次构建需要联网拉取模块；之后有模块缓存即可离线。

### 5.4 运行时定位顺序

`panels.rs` 的 `sidecar_path` 依次查找：

1. Tauri `resource_dir()` 下的 `kaypanel.exe`（安装后的正常位置）；
2. 当前可执行文件同目录；
3. 开发态：仓库内 `third_party/kaypanel/dist/kaypanel.exe`。

都找不到时，页面显示错误态并提示先运行 `scripts\build-kaypanel.bat`。

---

## 6. 使用

1. 侧边栏「管理 → 服务器管理」。
2. 首次进入时面板是空的，需要在面板里「设置 → 添加节点」：
   - 地址（IP 或域名）、SSH 端口（默认 22）、用户名；
   - 私钥或密码；需要 Docker 等特权命令时开启 `sudo`；
   - 首次连接会显示主机指纹（TOFU），确认后固定保存，之后指纹变化会拒绝连接。
3. 添加完成后即可查看监控、管理容器、浏览文件、打开终端。

注意：

- **只能管理 Linux 主机（SSH）**，不能管理 Windows 主机。Windows 机器用 ScreenWall 的屏幕墙与远程控制。
- 目标机器不需要安装任何 Agent，但需要运行 SSH 服务且本机能访问到。
- 面板是独立的浅色主题，不跟随 ScreenWall 的浅色/深色主题。
- 面板数据保存在本机 `%APPDATA%\ScreenWall\kaypanel\`，不走 ScreenWall 的账号与中转。

---

## 7. 排障

### 7.1 提示「找不到服务器管理组件 kaypanel.exe」

页面错误态会显示类似：

```
找不到服务器管理组件 kaypanel.exe，请先运行 scripts\build-kaypanel.bat
```

按场景处理：

| 场景 | 处理 |
|---|---|
| 开发环境 | 在仓库根目录运行 `scripts\build-kaypanel.bat`；确认 `Go 1.23+` 可用 |
| 安装包环境 | 正常安装包应在可执行文件同目录带 `kaypanel.exe`。先确认该文件存在；若被杀毒软件隔离，恢复并加白名单；仍缺失则重新安装 |
| 构建失败 | 看脚本输出：Go 不在 `PATH` 时设置 `GO_BIN`；拉模块失败时检查 `GOPROXY` 与网络 |

手动确认产物：

```powershell
Get-ChildItem D:\screenwall\third_party\kaypanel\dist\kaypanel.exe
```

### 7.2 面板白屏，或一直停在「正在启动服务器管理…」

先区分两种状态：

- 页面显示「服务器管理不可用」+ 具体错误：按 7.1 处理。
- 页面显示加载中，但内容区大面积空白：子进程可能已启动，但 webview 没加载出页面。

依次检查：

1. 任务管理器里是否有 `kaypanel.exe` 进程。没有的话，退出并重启 ScreenWall，再进页面。
2. WebView2 Runtime 是否正常（Windows 11 自带；精简系统或旧系统可能缺失）。
3. `%APPDATA%\ScreenWall\kaypanel\webview2` 是否可写、磁盘是否已满。
4. 面板进程崩溃后重新拉起时，已存在的内嵌 webview 可能仍停在旧地址。重启 ScreenWall 可以强制重建子 webview。
5. 安全软件是否拦截了子进程或本地回环访问；临时关闭后复测。

主程序日志（`%APPDATA%\ScreenWall\logs\screenwall.log`）里查找：

```
服务器管理面板已启动
服务器管理面板已内嵌
创建内嵌面板失败
```

### 7.3 端口冲突怎么办

**正常不需要处理。** sidecar 模式固定监听 `127.0.0.1:0`，端口由操作系统从空闲端口里分配，不存在可配置的固定端口，也不会和 ScreenWall 服务器的 `8787` 或局域网直连的 `47900` 冲突。配置里的 `-listen` 在 sidecar 模式下会被忽略。

只有极少数情况会看到绑定失败：

- 本机临时端口耗尽（大量连接未释放）。用 `netstat -ano | findstr 127.0.0.1` 观察，重启应用或系统后恢复。
- 安全软件 / 代理工具拦截回环监听。放行 ScreenWall 与 `kaypanel.exe` 后重试。

不要试图给 ScreenWall 内嵌的面板指定固定端口——它没有这个配置项。

### 7.4 面板能打开，但节点连不上

这属于 KayPanel 与目标 Linux 主机之间的问题，与 ScreenWall 的网络无关：

- 目标机是否运行 SSH、端口是否放行（云厂商安全组、firewalld / ufw）；
- 用户名、密钥或密码是否正确，是否有 `sudo` 权限；
- 主机指纹是否变化（重装系统、换机器后会变），面板会拒绝连接，需要在面板里重新确认；
- 网络是否可达（面板由本机直接发起 SSH，不经过 ScreenWall 中转）。

### 7.5 想看 kaypanel 自己的日志

主程序刻意丢弃了 sidecar 的 stderr。需要排查面板内部问题时，手动运行一份：

```powershell
cd D:\screenwall
third_party\kaypanel\dist\kaypanel.exe -sidecar -config "$env:APPDATA\ScreenWall\kaypanel\config.yaml"
```

它会打印监听地址、握手 JSON 与运行日志；在控制台按 `Ctrl+C` 走优雅退出。注意这是独立实例，不要和 ScreenWall 正在用的面板同时改同一份配置。

---

## 8. 已知限制

- **只支持 Linux 主机管理（SSH）**，不能管理 Windows 主机。
- 目标机需要开启 SSH 并可被本机访问；面板直连目标机，不经过 ScreenWall 中转，也不使用 ScreenWall 账号。
- 首次使用必须手动「添加节点」，没有从 ScreenWall 设备列表自动导入。
- 面板固定浅色主题，与主界面的浅色/深色主题相互独立。
- 安装包多一个约 9.4 MiB（未压缩）的 `kaypanel.exe`；打包机需要 Go 1.23+。
- 面板数据保存在本机 `%APPDATA%\ScreenWall\kaypanel\`，不随 ScreenWall 账号同步。
- 面板版本随 ScreenWall 安装包走，目前内置上游 v0.14.1；没有独立于 ScreenWall 的更新通道。
- 同一时间只有一个内嵌面板实例；重复点击侧边栏只会复用，不会起第二个进程。

---

## 9. 许可与来源

- 上游项目：<https://github.com/kaynai1/kaypanel>
- 内置版本：v0.14.1
- 许可：MIT License
- 本地改动：
  1. `main.go` 增加 `-sidecar` 模式（回环随机端口、stdout 握手 JSON、stdin EOF 退出）；
  2. 桌面版与 Docker 相关文件原样保留，但不参与 ScreenWall 的打包。
- 未修改上游的业务逻辑、鉴权逻辑与前端资源；`go.sum` 一并 vendor，构建可复现。

完整声明见 `third_party/kaypanel/THIRD-PARTY-NOTICE.md`。
