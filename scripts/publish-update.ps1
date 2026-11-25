# 发布一次自托管更新
#
# 用法（在仓库根目录执行）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\publish-update.ps1 -Version 0.2.0
#
# 做的事：
#   1. 把 target\release\bundle\nsis\ScreenWall_<Version>_x64-setup.exe 复制到 dist\update\
#   2. 改写 server.toml 的 [update] 段（dir / version / file / notes）
#   3. 提示重启服务器后再点「检查更新」
#
# 服务器上的目录不要放进仓库：dist\ 已在 .gitignore 里。

param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [string]$Source = "",
    [string]$Dir = "dist/update",
    [string]$Config = "server.toml",
    [string]$Notes = ""
)

$ErrorActionPreference = "Stop"
Set-Location (Split-Path -Parent $PSScriptRoot)

if (-not $Source) {
    $Source = "target\release\bundle\nsis\ScreenWall_${Version}_x64-setup.exe"
}
if (-not (Test-Path $Source)) {
    throw "找不到安装包：$Source（先跑 scripts\tauri-build.bat build --bundles nsis）"
}
if (-not (Test-Path $Config)) {
    throw "找不到服务器配置：$Config"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$file = Split-Path -Leaf $Source
$target = Join-Path $Dir $file
Copy-Item -Force $Source $target
Write-Host "已复制安装包 -> $target"

$hash = (Get-FileHash -Algorithm SHA256 $target).Hash.ToLower()
Write-Host "SHA256: $hash"

if (-not $Notes) {
    $Notes = "ScreenWall $Version"
}

$lines = Get-Content $Config
$inUpdate = $false
for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]
    if ($line -match '^\[update\]') {
        $inUpdate = $true
        continue
    }
    if ($inUpdate -and $line -match '^\[') {
        $inUpdate = $false
    }
    if (-not $inUpdate) {
        continue
    }
    if ($line -match '^dir\s*=') { $lines[$i] = "dir = `"$($Dir -replace '\\', '/')`"" }
    elseif ($line -match '^version\s*=') { $lines[$i] = "version = `"$Version`"" }
    elseif ($line -match '^file\s*=') { $lines[$i] = "file = `"$file`"" }
    elseif ($line -match '^notes\s*=') { $lines[$i] = "notes = `"$Notes`"" }
}
Set-Content -Path $Config -Value $lines -Encoding UTF8

Write-Host "已更新 $Config 的 [update] 段：version=$Version file=$file"
Write-Host "重启服务器（或让它重新读取配置）后，客户端点「检查更新」就能看到 $Version。"
