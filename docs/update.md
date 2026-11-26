# 更新与分发

这篇回答两个问题：**用户在哪里更新**，**你自己在哪里发新版**。

---

## 一、用户在哪里更新

### 客户端界面

打开客户端 → 左下角头像或「管理 → 设置」→ 左侧选 **关于** → 找到 **软件更新** 分组：

- 显示当前版本；
- 点 **检查更新**；
- 有新版本时显示版本号、发布日期和更新说明，点 **下载并安装**；
- 下载有进度条，边下边算 SHA256；校验不过会删掉文件并报错，不会装上一个坏包；
- 校验通过后启动 NSIS 安装包（`/S` 静默安装），会弹一次 UAC，装完自动替换。

**「已是最新版本」不是没连上**，而是源里的版本号不高于当前客户端版本。

### 两个更新源

客户端按下面的顺序找新版本，两种模式都能收到更新：

| 场景 | 更新源 | 说明 |
|---|---|---|
| 配了自建服务器（账号 / 连接码模式） | `GET {服务器}/api/v1/version` | 数据来自 `server.toml` 的 `[update]` 段，安装包由你自己托管 |
| 没配服务器（局域网模式） | GitHub Releases `kaynai1/screenwall` 的 latest | 取第一个 `*-setup.exe`，用附件自带的 `sha256:` digest 校验 |

自建服务器连不上时也会自动退到 GitHub，所以哪怕服务器挂了，用户仍然能知道有没有新版。

### 手动更新

不想用客户端里的按钮，就自己下载安装包覆盖安装：

- GitHub：<https://github.com/kaynai1/screenwall/releases>
- 自建服务器：`http://<你的服务器>/downloads/ScreenWall_<版本>_x64-setup.exe`

安装包是 perMachine 安装，**直接双击新版本覆盖安装即可**，配置（`%APPDATA%\ScreenWall\client.json`）和已配对的直连设备都会保留。

---

## 二、你自己在哪里发新版

### 路线 A：发到 GitHub（局域网模式用户走这条）

前提：本机装了 GitHub CLI 并登录（`gh auth login`）。

```powershell
# 1. 升版本号（三处必须一致）
#    根 Cargo.toml 的 workspace.package.version
#    ui/package.json
#    crates/client/tauri.conf.json

# 2. 打包
scripts\tauri-build.bat build --bundles nsis

# 3. 建 Release 并上传安装包（$v 换成这次要发的版本）
$v = "0.2.11"
gh release create "v$v" `
  "target\release\bundle\nsis\ScreenWall_${v}_x64-setup.exe" `
  --title "ScreenWall $v" `
  --notes "本次更新说明"
```

`gh` 会自动算附件的 digest，客户端检查更新时就是靠这个值校验安装包，所以**不要手动改附件**。

### 路线 B：发到自建服务器（账号 / 连接码模式用户走这条）

先看清楚发布脚本跑在**哪台机器**上 —— 它改的是那台机器上的 `server.toml`：

| 服务器跑在 | 用哪个脚本 | 在哪执行 |
|---|---|---|
| Linux（绝大多数情况） | `scripts/publish-update.sh` | 服务器上，用 SSH 过去执行 |
| Windows | `scripts\publish-update.ps1` | 服务器本机（通常也是打包机） |

#### B1. 服务器是 Linux（一条命令）

登录服务器（root，或能 sudo 的账号），直接执行：

```bash
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/publish-update.sh | bash
```

不需要先传任何文件：脚本自己去 GitHub 找最新的正式版，下载 `ScreenWall_<版本>_x64-setup.exe`，
再把服务器配置好。`publish-update.ps1` **不能**在这儿用（`powershell: command not found`）。

要发指定版本，或服务器上已经有安装包：

```bash
# 指定版本
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/publish-update.sh | bash -s -- -v 0.2.11

# 用服务器上已有的安装包
curl -fsSL -H 'Accept: application/vnd.github.raw' \
  https://api.github.com/repos/kaynai1/screenwall/contents/scripts/publish-update.sh \
  | bash -s -- -v 0.2.11 -f /root/ScreenWall_0.2.11_x64-setup.exe
```

脚本做的事：

1. 定出版本号：`-v` 指定 → 文件名推断 → 否则取 GitHub 最新正式版；
2. 下载安装包（先试 `github.com` 直链，不通再走 `api.github.com` 的附件接口）；
3. 把安装包复制到 `[update].dir`（默认 `/opt/screenwall/downloads`）；
4. 删掉旧的 `[update]` 段、追加新的（`dir` / `version` / `file` / `notes`），旧配置留一份 `.bak.<时间>`；
5. 重启 `screenwall` 服务（`--no-restart` 可跳过）；
6. 请求一次 `http://127.0.0.1:<端口>/api/v1/version`，确认真的返回了新版本。

SHA256 不用手填，服务器首次被请求时会自己算并缓存。想先看看脚本内容再跑：
`curl -fsSL ... | less`，或直接从仓库拿 `scripts/publish-update.sh`。

#### B2. 服务器是 Windows（少见）

在服务器本机执行：

```powershell
# $v 与路线 A 同一版本
$v = "0.2.11"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\publish-update.ps1 -Version $v
```

它会把安装包复制到 `dist\update\`、算出 SHA256、改写 `server.toml` 的 `[update]` 段。

然后客户端点「检查更新」就能看到新版本。

### 两条都发

最稳妥的做法是两条都走：GitHub 给局域网模式的用户，自建服务器给你自己那几台走账号的机器。两者互不影响，客户端会按当前模式自动选。

---

## 三、版本号要改哪三处

```text
Cargo.toml                        →  [workspace.package] version = "<版本>"
ui/package.json                   →  "version": "<版本>"
crates/client/tauri.conf.json     →  "version": "<版本>"
```

三处必须写成同一个版本号（当前是 `0.2.11`）。

漏改 `tauri.conf.json` 会导致安装包文件名和内部版本对不上；漏改 `Cargo.toml` 会让客户端报的版本号还是旧的（`CURRENT_VERSION` 取自 `CARGO_PKG_VERSION`）。

---

## 四、注意事项

- **没有商业代码签名证书**。安装包首次运行仍会弹 SmartScreen 警告，这是正常的，要消除必须买 OV/EV 证书。详见 [签名文档](signing.md)。
- **校验值不能省**。客户端拿到没有 `sha256` 的更新信息会直接报错拒绝安装，宁可不让用户更新，也不装一个无法校验的包。
- **GitHub API 有速率限制**。未认证请求每小时 60 次，个人使用完全够；如果出现 `HTTP 403`，等一会儿再试。
- **`dist/` 不进仓库**（`.gitignore` 已忽略），安装包只存在于本机或 Release 上。
