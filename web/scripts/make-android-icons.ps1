# 用网页端的 512 图标生成安卓启动图标（mipmap 各密度）。
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$source = "D:\screenwall\web\public\icon-512.png"
$root = "D:\screenwall\web\android\app\src\main\res"

$densities = @(
    @{ dir = "mdpi"; legacy = 48; foreground = 108 },
    @{ dir = "hdpi"; legacy = 72; foreground = 162 },
    @{ dir = "xhdpi"; legacy = 96; foreground = 216 },
    @{ dir = "xxhdpi"; legacy = 144; foreground = 324 },
    @{ dir = "xxxhdpi"; legacy = 192; foreground = 432 }
)

$image = [System.Drawing.Image]::FromFile($source)

foreach ($entry in $densities) {
    $folder = Join-Path $root ("mipmap-" + $entry.dir)
    foreach ($pair in @(
            @{ name = "ic_launcher.png"; size = $entry.legacy },
            @{ name = "ic_launcher_round.png"; size = $entry.legacy },
            @{ name = "ic_launcher_foreground.png"; size = $entry.foreground }
        )) {
        $bitmap = New-Object System.Drawing.Bitmap($pair.size, $pair.size)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.DrawImage($image, 0, 0, $pair.size, $pair.size)
        $graphics.Dispose()
        $bitmap.Save((Join-Path $folder $pair.name), [System.Drawing.Imaging.ImageFormat]::Png)
        $bitmap.Dispose()
    }
}

$image.Dispose()
Write-Host "android icons updated"
