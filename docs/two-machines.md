# 两台机器试用（同一局域网）

目标：机器 A（这台，先当服务器/被控）与机器 B（另一台，主控）都能登录、看到对方，并各自试一次
「中转控制」和「局域网直连」。

> 只在自己的局域网里试。下面的 `allow_registration = true` 和 `bind = 0.0.0.0` 是为了省事，
> 公网部署请按 [server.md](server.md) 关闭自助注册并放在 TLS 反向代理后面。

## 0. 准备

- 两台机器连同一个路由器（同一网段，`192.168.x.x` 之类）。
- 在 A 上查自己的局域网地址：

  ```powershell
  ipconfig | Select-String "IPv4"
  ```

  记下形如 `192.168.1.4` 的地址，下文用 `<A-IP>` 代替。
- 两台机器都装 ScreenWall（[Releases](https://github.com/kaynai1/screenwall/releases) 里的
  最新安装包，形如 `ScreenWall_<版本>_x64-setup.exe`）。安装包需要管理员权限，并会自动加好直连用的防火墙规则。
  如果只是直接运行 `screenwall-client.exe`（没安装），A 上要到「设置 → 局域网直连」点一次
  「允许局域网直连」来补规则（会弹一次 UAC）。

## 1. 在 A 上起服务器

Release 里附带了 `screenwall-server.exe`（Windows x64），与安装包放同一目录即可。新建
`server.lan.toml`：

```toml
bind = "0.0.0.0:8787"
db_path = "screenwall.db"
allow_registration = true
token_ttl_days = 30
max_frame_bytes = 1048576
```

启动（保持这个窗口开着）：

```powershell
.\screenwall-server.exe --config server.lan.toml
```

放行 8787 端口（管理员 PowerShell，只需一次）：

```powershell
netsh advfirewall firewall add rule name="ScreenWall Server" dir=in action=allow protocol=TCP localport=8787 profile=domain,private
```

在 B 上验证能到服务器：

```powershell
curl http://<A-IP>:8787/healthz
```

返回 200 即可。

## 2. 两台都登录

- A、B 打开 ScreenWall，服务器地址都填 `http://<A-IP>:8787`。
- 同一账号即可（两台机器会各自成为一台设备）；也可以各注册一个账号，但那样互相看不到设备，
  只能在「直连」里连。
- 登录后，「设备」页应能看到对方的设备，在线状态为「当前在线」。

## 3. 试中转控制

B 在「设备」页对 A 点「控制」→ A 弹出授权请求 → 点「允许控制」。
B 应看到 A 的画面并可以操作键鼠。文件传输、声音、剪贴板同理。

## 4. 试局域网直连（不经服务器）

直连和账号无关，但界面需要先登录才能进入（随便登一个就行）。

- A：设置 → 局域网直连 → 打开。开启后同一网段内可被自动发现。
- B：设备页 → 「直连」→ 「扫描同网段设备」→ 列表里选中 A（显示设备名与 `<A-IP>:47900`）
  → 连接；A 会弹出授权请求，点「允许」。
- B 看到画面即成功。断开后再连，A 仍会再问一次——这是刻意的设计（每次连接都要本机确认）。

## 5. 连不上怎么查（按顺序）

1. `ping <A-IP>`：不通就是网络/路由器隔离（很多路由器开了「AP 隔离」）。
2. `Test-NetConnection <A-IP> -Port 8787`：登录都连不上，先查 A 上的服务器是否在跑、端口是否放行。
3. `Test-NetConnection <A-IP> -Port 47900`：直连端口。不通就查 A 的防火墙规则：

   ```powershell
   netsh advfirewall firewall show rule name="ScreenWall Direct"
   ```

   没有这条规则，就在 A 的设置页点「允许局域网直连」重新补。
4. 扫描不到设备：UDP 47901 被挡。检查 `ScreenWall Discovery` 规则（同上，用 `show rule` 看），
   以及网络配置文件是否被 Windows 划成「公用」——规则只对「域/专用」生效。
5. A 的日志在 `%APPDATA%\ScreenWall\logs\screenwall.log`，设置页可一键导出诊断包（令牌已脱敏）。

## 已知限制

- 直连要求两台机器**在同一网段或路由可达**；跨公网没有 UDP 打洞，请走中转（需要一台有公网
  地址的服务器 + Caddy HTTPS）。
- 被控端是普通用户进程，遇到以管理员身份运行的程序（任务管理器、UAC 提权后的窗口）会受
  UIPI 限制，控制不了。
- 安装包未签名，SmartScreen 会提示；点「更多信息 → 仍要运行」。
