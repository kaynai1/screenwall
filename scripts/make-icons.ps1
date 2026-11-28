# 生成 ScreenWall 的三端图标（网页 PWA / 安卓 / Windows）。
#
# 标志：三根竖向圆角柱——蓝 `#0079FD`、浅灰 `#ADADAF`、深灰 `#727276`，
# 白底。比例取自设计稿（42px 版）：蓝柱宽 11、高 27；两根灰柱宽 7、高 25，
# 柱间留 1.5 个单位的缝，整体 28×27。
#
# 用法：powershell -ExecutionPolicy Bypass -File scripts/make-icons.ps1
# 只依赖 .NET 的 System.Drawing，不需要额外装工具。

param(
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$Blue = [System.Drawing.ColorTranslator]::FromHtml("#0079FD")
$Gray = [System.Drawing.ColorTranslator]::FromHtml("#ADADAF")
$Dark = [System.Drawing.ColorTranslator]::FromHtml("#727276")
$White = [System.Drawing.Color]::White

# 标志内容区（单位坐标）宽 28、高 27。
$UnitW = 28.0
$UnitH = 27.0

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $d = [Math]::Min($r, [Math]::Min($w, $h) / 2) * 2
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    return $path
}

function Draw-Mark([System.Drawing.Graphics]$g, [float]$size, [float]$contentWidth) {
    $u = $contentWidth / $UnitW
    $totalH = $UnitH * $u
    $left = ($size - $contentWidth) / 2
    $top = ($size - $totalH) / 2

    $bars = @(
        @{ X = 0.0;  Y = 0.0; W = 11.0; H = 27.0; Color = $Blue },
        @{ X = 12.5; Y = 1.0; W = 7.0;  H = 25.0; Color = $Gray },
        @{ X = 21.0; Y = 1.0; W = 7.0;  H = 25.0; Color = $Dark }
    )
    foreach ($bar in $bars) {
        $x = $left + $bar.X * $u
        $y = $top + $bar.Y * $u
        $w = $bar.W * $u
        $h = $bar.H * $u
        $path = New-RoundedPath $x $y $w $h ($w / 2)
        $brush = New-Object System.Drawing.SolidBrush($bar.Color)
        $g.FillPath($brush, $path)
        $brush.Dispose()
        $path.Dispose()
    }
}

function New-Icon([int]$size, [float]$scale, [string]$path, [bool]$round, [bool]$transparent) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    if ($round) {
        $g.Clear([System.Drawing.Color]::Transparent)
        $circle = New-Object System.Drawing.Drawing2D.GraphicsPath
        $circle.AddEllipse(0, 0, $size, $size)
        $whiteBrush = New-Object System.Drawing.SolidBrush($White)
        $g.FillPath($whiteBrush, $circle)
        $whiteBrush.Dispose()
        $g.SetClip($circle)
    } else {
        $g.Clear($(if ($transparent) { [System.Drawing.Color]::Transparent } else { $White }))
    }

    Draw-Mark $g $size ($size * $scale)
    $g.Dispose()
    $dir = Split-Path -Parent $path
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "  $path"
}

function New-Splash([int]$w, [int]$h, [string]$path) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear($White)
    $markWidth = [Math]::Min($w, $h) * 0.26
    # Draw-Mark 以正方形画布居中，这里把原点挪到屏幕中心。
    $g.TranslateTransform(($w - [Math]::Min($w, $h)) / 2, ($h - [Math]::Min($w, $h)) / 2)
    Draw-Mark $g ([double][Math]::Min($w, $h)) $markWidth
    $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "  $path"
}

function Get-IconPngBytes([int]$size, [float]$scale) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear($White)
    Draw-Mark $g $size ($size * $scale)
    $g.Dispose()
    $stream = New-Object System.IO.MemoryStream
    $bmp.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return $stream.ToArray()
}

# ICO 里的 BMP 帧（DIB）：32 位 BGRA、自下而上，附一张全零的 AND 掩码。
# 小尺寸用 BMP 而不是 PNG，旧版 Windows 与部分打包工具只认 BMP 帧。
function Get-IconDibBytes([int]$size, [float]$scale) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear($White)
    Draw-Mark $g $size ($size * $scale)
    $g.Dispose()

    $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
    $locked = $bmp.LockBits(
        $rect,
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $stride = $locked.Stride
    $pixels = New-Object byte[] ($stride * $size)
    [System.Runtime.InteropServices.Marshal]::Copy($locked.Scan0, $pixels, 0, $pixels.Length)
    $bmp.UnlockBits($locked)
    $bmp.Dispose()

    $maskStride = [int]([Math]::Ceiling($size / 32.0) * 4)
    $mask = New-Object byte[] ($maskStride * $size)

    $stream = New-Object System.IO.MemoryStream
    $writer = New-Object System.IO.BinaryWriter($stream)
    $writer.Write([UInt32]40)                # biSize
    $writer.Write([Int32]$size)              # biWidth
    $writer.Write([Int32]($size * 2))        # biHeight（XOR + AND）
    $writer.Write([UInt16]1)                 # biPlanes
    $writer.Write([UInt16]32)                # biBitCount
    $writer.Write([UInt32]0)                 # BI_RGB
    $writer.Write([UInt32]($size * $size * 4))
    $writer.Write([Int32]0)
    $writer.Write([Int32]0)
    $writer.Write([UInt32]0)
    $writer.Write([UInt32]0)
    for ($y = $size - 1; $y -ge 0; $y--) {
        $writer.Write($pixels, $y * $stride, $size * 4)
    }
    $writer.Write($mask)
    $writer.Flush()
    $bytes = $stream.ToArray()
    $writer.Dispose()
    $stream.Dispose()
    return , $bytes
}

function New-Ico([string]$path) {
    $sizes = @(16, 24, 32, 48, 64, 128, 256)
    $images = @()
    foreach ($size in $sizes) {
        if ($size -ge 256) {
            # 256 用 PNG 压缩，文件小得多（Windows Vista+ 支持）。
            $images += , @{ Data = [byte[]](Get-IconPngBytes $size 0.68); Png = $true }
        } else {
            $images += , @{ Data = [byte[]](Get-IconDibBytes $size 0.68); Png = $false }
        }
    }

    $stream = New-Object System.IO.MemoryStream
    $writer = New-Object System.IO.BinaryWriter($stream)
    $writer.Write([UInt16]0)                 # reserved
    $writer.Write([UInt16]1)                 # type: icon
    $writer.Write([UInt16]$sizes.Count)

    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $size = $sizes[$i]
        $data = $images[$i].Data
        $writer.Write([Byte]$(if ($size -ge 256) { 0 } else { $size }))
        $writer.Write([Byte]$(if ($size -ge 256) { 0 } else { $size }))
        $writer.Write([Byte]0)               # palette
        $writer.Write([Byte]0)               # reserved
        $writer.Write([UInt16]1)             # color planes
        $writer.Write([UInt16]32)            # bits per pixel
        $writer.Write([UInt32]$data.Length)
        $writer.Write([UInt32]$offset)
        $offset += $data.Length
    }
    foreach ($image in $images) { $writer.Write($image.Data) }
    $writer.Flush()
    [System.IO.File]::WriteAllBytes($path, $stream.ToArray())
    $writer.Dispose()
    $stream.Dispose()
    Write-Host "  $path"
}

Write-Host "网页 PWA"
New-Icon 192 0.62 (Join-Path $RepoRoot "web/public/icon-192.png") $false $false
New-Icon 512 0.62 (Join-Path $RepoRoot "web/public/icon-512.png") $false $false
New-Icon 512 0.42 (Join-Path $RepoRoot "web/public/icon-maskable-512.png") $false $false

Write-Host "安卓 (Capacitor)"
$res = Join-Path $RepoRoot "web/android/app/src/main/res"
$density = @{ "mdpi" = 1.0; "hdpi" = 1.5; "xhdpi" = 2.0; "xxhdpi" = 3.0; "xxxhdpi" = 4.0 }
foreach ($name in $density.Keys) {
    $scale = $density[$name]
    $dir = Join-Path $res "mipmap-$name"
    New-Icon ([int](48 * $scale)) 0.60 (Join-Path $dir "ic_launcher.png") $false $false
    New-Icon ([int](48 * $scale)) 0.56 (Join-Path $dir "ic_launcher_round.png") $true $false
    # 自适应图标前景是 108dp，内容要落在中间 66dp 安全区里。
    New-Icon ([int](108 * $scale)) 0.42 (Join-Path $dir "ic_launcher_foreground.png") $false $true
}

$splashDirs = @{
    "drawable-port-mdpi" = @(320, 480); "drawable-port-hdpi" = @(480, 800);
    "drawable-port-xhdpi" = @(720, 1280); "drawable-port-xxhdpi" = @(960, 1600);
    "drawable-port-xxxhdpi" = @(1280, 1920);
    "drawable-land-mdpi" = @(480, 320); "drawable-land-hdpi" = @(800, 480);
    "drawable-land-xhdpi" = @(1280, 720); "drawable-land-xxhdpi" = @(1600, 960);
    "drawable-land-xxxhdpi" = @(1920, 1280);
    "drawable" = @(480, 320)
}
foreach ($name in $splashDirs.Keys) {
    $size = $splashDirs[$name]
    New-Splash $size[0] $size[1] (Join-Path $res "$name/splash.png")
}

Write-Host "Windows (Tauri)"
New-Ico (Join-Path $RepoRoot "crates/client/icons/icon.ico")

Write-Host "完成"
