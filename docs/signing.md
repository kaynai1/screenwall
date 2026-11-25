# 代码签名

当前状态：**没有商业代码签名证书**。正式发布用的安装包仍应按「未签名」对待，
Windows SmartScreen 会提示「Windows 已保护你的电脑」。本文给出两套方案：

1. **自签名证书**（现在就能用）：证明「包签名后没被改过」，但**消不掉** SmartScreen；
2. **正式 OV/EV 证书**（以后买了就切）：真正解决 SmartScreen。

脚本：

| 脚本 | 作用 |
| --- | --- |
| `scripts\make-dev-cert.ps1` | 生成自签名代码签名证书，导出 `.pfx`，打印 Thumbprint |
| `scripts\sign.ps1` | 用 `signtool.exe` 签名并 `verify /pa /v` |

---

## 1. 为什么自签名消不掉 SmartScreen

SmartScreen 判断的是**信誉**，不是「有没有签名」：

- 一个**未知发布者**的 exe，SmartScreen 直接弹「Windows 已保护你的电脑」；
- 用了证书但证书**不在微软信誉体系**里（自签名证书 100% 属于此类），SmartScreen 仍然
  当成未知，警告照弹；
- **OV 证书**：签名的包会逐渐积累信誉，下载量起来后警告才慢慢消失，新包首次仍可能提示；
- **EV 证书**：微软对 EV 签名的包给即时信誉，**首次下载也不弹** SmartScreen（但仍可能弹
  UAC 的「是否允许此应用更改设备」——那是提权提示，跟签名信誉无关）。

所以：自签名的价值是**完整性**（签名后改一个字节，验证就会失败），不是**信任**
（SmartScreen 不认）。要在用户机器上不弹警告，只能买 OV/EV 证书。

---

## 2. 自签名方案（可立即使用）

### 2.1 生成证书

```powershell
cd D:\screenwall
$env:SCREENWALL_PFX_PASSWORD = '换成你自己的密码'   # 可选；不设则自动生成并写到 target\certs\pfx-password.txt
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-dev-cert.ps1
```

脚本做的事：

- `New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=ScreenWall Dev"`，
  存到 `Cert:\CurrentUser\My`（**不需要管理员**，`CurrentUser\My` 是每用户的）；
- EKU 设为 Code Signing（`1.3.6.1.5.5.7.3.3`），`BasicConstraints` 非 CA；
- 导出私钥到 `target\certs\screenwall-dev.pfx`；
- 打印 Thumbprint（最后一行也输出一次，方便脚本捕获）。

实测输出（2026-11-24）：

```
Subject    : CN=ScreenWall Dev
Thumbprint : B0C115AC39E2D5F611A714F2AE12299E7DC20655
Store      : Cert:\CurrentUser\My\B0C115AC39E2D5F611A714F2AE12299E7DC20655
NotAfter   : 11/24/2029 6:23:15 PM
PFX        : D:\screenwall\target\certs\screenwall-dev.pfx
```

重复运行会**复用**同 Subject 下最新的带私钥证书，不会无限生成。

### 2.2 （可选）把证书导入「受信任的根证书颁发机构」

**仅用于本机测试**。导入后本机的 `signtool verify` 才会通过；不要把这张根证书发给用户。

```powershell
$thumb = 'B0C115AC39E2D5F611A714F2AE12299E7DC20655'
$tmp = Join-Path $env:TEMP 'sw-dev-cert.cer'
Export-Certificate -Cert "Cert:\CurrentUser\My\$thumb" -FilePath $tmp
Import-Certificate -FilePath $tmp -CertStoreLocation Cert:\CurrentUser\Root
```

撤销（测试完建议撤掉）：

```powershell
Get-ChildItem Cert:\CurrentUser\Root | Where-Object Thumbprint -eq $thumb | Remove-Item -Force
```

### 2.3 签名

```powershell
# 用证书库里已有的证书（推荐）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 `
    -File target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe `
    -Thumbprint B0C115AC39E2D5F611A714F2AE12299E7DC20655

# 或用 PFX（换机器、换用户时）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 `
    -File target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe `
    -PfxPath target\certs\screenwall-dev.pfx `
    -PfxPassword $env:SCREENWALL_PFX_PASSWORD
```

`sign.ps1` 自动在 `C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe`
里找**版本号最大**的那个，执行：

```
signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 <exe>
signtool verify /pa /v <exe>
```

`-TimestampUrl ''` 可跳过时间戳（不建议：签名会随证书过期而失效）。
`-SkipVerify` 只签名不验证。

### 2.4 验证结果怎么读

签名后 `signtool verify /pa /v` 会显示签名有效、时间戳来自 DigiCert，但最后报：

```
SignTool Error: A certificate chain processed, but terminated in a root
    certificate which is not trusted by the trust provider.
```

这是**预期行为**，不是签名失败。本机实测：

| 状态 | `signtool verify /pa /v` 结果 |
| --- | --- |
| 根证书**未**导入受信任根 | 签名/时间戳都正常，最后报「chain ... not trusted」，退出码 1 |
| 根证书**已**导入受信任根 | `Successfully verified`，`Number of errors: 0`，退出码 0 |

两种情况下签名本身都成功（`Successfully signed`，`Number of errors: 0`），
`Get-AuthenticodeSignature` 都能读到 `Signer: CN=ScreenWall Dev` 与
`Timestamp: CN=DigiCert SHA256 RSA4096 Timestamp Responder 2026 1`。
差别只在**本机是否信任这张根证书**。

### 2.5 删除证书

```powershell
Remove-Item Cert:\CurrentUser\My\B0C115AC39E2D5F611A714F2AE12299E7DC20655
Remove-Item target\certs\screenwall-dev.pfx, target\certs\pfx-password.txt
```

（`target\` 在 `.gitignore` 里，PFX 不会进版本库。但**永远不要**把私钥或 PFX 密码提交到仓库。）

---

## 3. 有正式证书后怎么切

### 3.1 让 Tauri 自己签（推荐）

`crates/client/tauri.conf.json` 的 `bundle.windows` 现在是：

```json
"windows": {
  "nsis": {
    "installMode": "perMachine",
    "installerHooks": "windows/installer-hooks.nsh",
    "displayLanguageSelector": false
  }
}
```

买了证书后，在 `bundle.windows` 里加两项（**签名字段本次没有添加**，避免与其他改动冲突）：

```json
"windows": {
  "certificateThumbprint": "你的证书指纹（去掉空格）",
  "digestAlgorithm": "sha256",
  "timestampUrl": "http://timestamp.digicert.com",
  "nsis": { "installMode": "perMachine", "installerHooks": "windows/installer-hooks.nsh", "displayLanguageSelector": false }
}
```

- `certificateThumbprint`：证书指纹。Tauri 从 **当前用户** 的证书库（`Cert:\CurrentUser\My`）
  找这张证书并签名。CI 里需要先把证书 `Import-PfxCertificate` 进去（见 3.3）。
- 如果证书不在当前用户库、或者想用 `signtool` 自定义参数，可以用 `signCommand`
  （Tauri 2 支持自定义签名命令），例如：

  ```json
  "signCommand": "powershell -NoProfile -ExecutionPolicy Bypass -File ../../scripts/sign.ps1 -File %1 -Thumbprint <thumb>"
  ```

  `%1` 是 Tauri 传入的待签文件路径。具体占位符以 Tauri 2 文档为准；本项目**尚未**用
  `signCommand` 实测过，切换时以 `certificateThumbprint` 为主。

### 3.2 别把「更新签名」和「exe 签名」搞混

| 概念 | 配置 | 作用 |
| --- | --- | --- |
| **exe/安装包签名** | `bundle.windows.certificateThumbprint` / `signtool` | 让 Windows 认发布者、过 SmartScreen（配合 OV/EV） |
| **更新签名** | `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（Tauri updater 的 minisign 密钥） | 让**自动更新**校验「下载的更新包确实是官方发的」 |

`TAURI_SIGNING_PRIVATE_KEY` **不是**给 exe 做 Authenticode 签名的，它对 SmartScreen 没作用。
本项目当前用的是**自托管更新**（`docs/server.md` 1.9 节：服务器返回 SHA256，客户端校验），
没有启用 Tauri updater 的签名机制，所以目前不需要 `TAURI_SIGNING_PRIVATE_KEY`。

### 3.3 CI 里注入证书（GitHub Secrets）

把 PFX 转成 base64 存进仓库 Secrets：

```powershell
# 本地一次性执行
$b64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes('target\certs\screenwall.pfx'))
Set-Clipboard $b64        # 粘贴到 GitHub → Settings → Secrets and variables → Actions
```

需要的 Secrets：

| Secret | 内容 |
| --- | --- |
| `WINDOWS_CERTIFICATE_BASE64` | PFX 文件的 base64 |
| `WINDOWS_CERTIFICATE_PASSWORD` | PFX 密码 |

在 `bundle` job 里加一步（**本次没有把它写进 `ci.yml`**，因为仓库还没有证书；买了证书后加）：

```yaml
      - name: Import code-signing certificate
        shell: pwsh
        env:
          CERT_B64: ${{ secrets.WINDOWS_CERTIFICATE_BASE64 }}
          CERT_PWD: ${{ secrets.WINDOWS_CERTIFICATE_PASSWORD }}
        run: |
          $pfx = Join-Path $env:RUNNER_TEMP 'screenwall.pfx'
          [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:CERT_B64))
          $pwd = ConvertTo-SecureString -String $env:CERT_PWD -AsPlainText -Force
          $cert = Import-PfxCertificate -FilePath $pfx -CertStoreLocation Cert:\CurrentUser\My -Password $pwd
          Write-Host "Imported $($cert.Thumbprint)"
          # 交给 tauri.conf.json 的 bundle.windows.certificateThumbprint
```

导入后，`scripts\tauri-build.bat build --bundles nsis` 产出的安装包会带签名。
如果 `certificateThumbprint` 写死的是证书指纹，指纹在换证书时会变，记得同步。

CI 里用 PFX 直接签（不经 Tauri）也可以：

```yaml
      - name: Sign installer
        shell: pwsh
        env:
          SCREENWALL_PFX_PASSWORD: ${{ secrets.WINDOWS_CERTIFICATE_PASSWORD }}
        run: |
          # 先还原 pfx（同上），然后
          powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 `
            -File target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe `
            -PfxPath $env:RUNNER_TEMP\screenwall.pfx
```

---

## 4. 时间戳服务器

签名带 `/tr <url> /td sha256`（RFC 3161）后，即使证书过期，**已经签过的包**在
证书有效期内签的那个时间点仍被视为有效——所以时间戳是必须的，否则证书一过期，
所有旧安装包的签名就都失效了。

备选地址（都在用，任选其一；某个挂了就换）：

| 地址 | 提供方 |
| --- | --- |
| `http://timestamp.digicert.com` | DigiCert（默认，实测可用） |
| `http://timestamp.sectigo.com` | Sectigo |
| `http://timestamp.globalsign.com/tsa/r6advanced1` | GlobalSign |
| `http://timestamp.acs.microsoft.com` | Microsoft |

签名需要**能上网**（访问时间戳服务器）。离线构建时可用 `-TimestampUrl ''` 跳过，
但产物不应作为正式发布。

---

## 5. 实测记录（2026-11-24）

环境：Windows 11，PowerShell 5.1，signtool 10.0.26100.0，安装包
`ScreenWall_0.1.0_x64-setup.exe`（当时 4.16 MiB；2026-11-24 19:25 重打后为 4.20 MiB）。

- `make-dev-cert.ps1`：成功，Thumbprint `B0C115AC39E2D5F611A714F2AE12299E7DC20655`，
  有效期至 2029-11-24，PFX 导出到 `target\certs\`。
- `sign.ps1 -Thumbprint ...` 签安装包：`Successfully signed`，
  `The signature is timestamped: Tue Nov 24 18:23:23 2026`。
- `signtool verify /pa /v`（根证书未信任）：签名链显示 `ScreenWall Dev` → 自身，
  时间戳链到 DigiCert，最后报 `chain ... not trusted`（预期）。
- 临时导入受信任根后重验：`Successfully verified`，`Number of errors: 0`（随后已移除该根）。
- `sign.ps1 -PfxPath ... -SkipVerify` 签一份 exe 副本：`Successfully signed`（PFX 路径可用）。
- `make-dev-cert.ps1` 再跑一次：走「复用已有证书」分支，Thumbprint 不变，PFX 重新导出。

注意：上面签过名的 `target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe` 是
**本机测试产物**，签名后它的哈希已经变了。下次 `scripts\tauri-build.bat` 重新打包会
生成新的未签名 exe。发布前要按本文流程重新签，并同步更新 `server.toml` 里
`[update].sha256`（或让服务器重新计算，见 `docs/server.md` 1.9 节）。
