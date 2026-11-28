# 生成 PWA 图标（192 / 512 / maskable 512），与网页端品牌标记一致。
# 用法：powershell -ExecutionPolicy Bypass -File web\scripts\make-icons.ps1
Add-Type -AssemblyName System.Drawing

$outDir = Join-Path $PSScriptRoot "..\public"
$outDir = [System.IO.Path]::GetFullPath($outDir)
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    return $path
}

function New-Icon([int]$size, [string]$file, [double]$contentScale) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::FromArgb(255, 11, 11, 13))

    # 背景圆角块（maskable 版本留出安全边距，由 contentScale 控制）
    $pad = $size * (1 - $contentScale) / 2
    $bg = New-RoundedPath $pad $pad ($size - 2 * $pad) ($size - 2 * $pad) ($size * 0.18 * $contentScale)
    $bgBrush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 20, 22, 28))
    $g.FillPath($bgBrush, $bg)

    # 三条横杠：与网页端 .brand-mark 一致
    $accent = [System.Drawing.Color]::FromArgb(255, 0, 122, 255)
    $inner = $size * $contentScale
    $left = $size / 2 - $inner * 0.3
    $barH = $inner * 0.11
    $gap = $inner * 0.1
    $top = $size / 2 - ($barH * 3 + $gap * 2) / 2
    $rows = @(
        @{ width = 0.6; alpha = 150 },
        @{ width = 1.0; alpha = 255 },
        @{ width = 0.78; alpha = 200 }
    )
    for ($i = 0; $i -lt $rows.Count; $i++) {
        $w = $inner * 0.6 * $rows[$i].width
        $y = $top + $i * ($barH + $gap)
        $path = New-RoundedPath $left $y $w $barH ($barH / 2)
        $color = [System.Drawing.Color]::FromArgb($rows[$i].alpha, $accent.R, $accent.G, $accent.B)
        $brush = New-Object System.Drawing.SolidBrush $color
        $g.FillPath($brush, $path)
        $brush.Dispose()
        $path.Dispose()
    }

    $bgBrush.Dispose()
    $bg.Dispose()
    $g.Dispose()
    $target = Join-Path $outDir $file
    $bmp.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "wrote $target"
}

New-Icon 192 "icon-192.png" 1.0
New-Icon 512 "icon-512.png" 1.0
New-Icon 512 "icon-maskable-512.png" 0.78
