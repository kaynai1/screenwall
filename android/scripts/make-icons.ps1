# 生成安卓启动图标：三色竖柱新标志（与 App 内 BrandMark 一致）。
# 用法：powershell -ExecutionPolicy Bypass -File android\scripts\make-icons.ps1
Add-Type -AssemblyName System.Drawing

$res = Join-Path $PSScriptRoot "..\app\src\main\res"
$res = [System.IO.Path]::GetFullPath($res)

# 与 Desktop 端网页版品牌色一致的深色底 + 蓝/靛/青三根竖柱。
$bgColor = [System.Drawing.Color]::FromArgb(255, 11, 11, 13)
$barColors = @(
    [System.Drawing.Color]::FromArgb(255, 0, 122, 255),
    [System.Drawing.Color]::FromArgb(255, 88, 86, 214),
    [System.Drawing.Color]::FromArgb(255, 50, 173, 230)
)

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = [Math]::Min($r * 2, [Math]::Min($w, $h))
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    return $path
}

function Draw-Bars($g, [double]$size, [double]$contentScale) {
    $inner = $size * $contentScale
    $left = ($size - $inner) / 2
    $top = ($size - $inner) / 2
    $barW = $inner * 0.16
    $gap = $inner * 0.11
    $total = $barW * 3 + $gap * 2
    $x = $size / 2 - $total / 2
    $heights = @(0.60, 1.0, 0.78)
    for ($i = 0; $i -lt 3; $i++) {
        $h = $inner * $heights[$i]
        $y = $size / 2 - $h / 2
        $path = New-RoundedPath $x $y $barW $h ($barW / 2)
        $brush = [System.Drawing.SolidBrush]::new($barColors[$i])
        $g.FillPath($brush, $path)
        $brush.Dispose()
        $path.Dispose()
        $x += $barW + $gap
    }
}

function New-Icon([int]$size, [string]$dir, [string]$file, [bool]$legacy, [bool]$round) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    if ($legacy) {
        $margin = [Math]::Max(1, $size * 0.04)
        $radius = $size * 0.22
        if ($round) {
            $brush = [System.Drawing.SolidBrush]::new($bgColor)
            $g.FillEllipse($brush, $margin, $margin, $size - 2 * $margin, $size - 2 * $margin)
            $brush.Dispose()
        } else {
            $bg = New-RoundedPath $margin $margin ($size - 2 * $margin) ($size - 2 * $margin) $radius
            $brush = [System.Drawing.SolidBrush]::new($bgColor)
            $g.FillPath($brush, $bg)
            $brush.Dispose()
            $bg.Dispose()
        }
        Draw-Bars $g $size 0.56
    } else {
        # 自适应图标前景：只画竖柱，背景由 ic_launcher_background 提供。
        Draw-Bars $g $size 0.42
    }

    $g.Dispose()
    $targetDir = Join-Path $res $dir
    New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
    $bmp.Save((Join-Path $targetDir $file), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "wrote $dir/$file"
}

$densities = @(
    @{ dir = "mipmap-mdpi"; legacy = 48; foreground = 108 },
    @{ dir = "mipmap-hdpi"; legacy = 72; foreground = 162 },
    @{ dir = "mipmap-xhdpi"; legacy = 96; foreground = 216 },
    @{ dir = "mipmap-xxhdpi"; legacy = 144; foreground = 324 },
    @{ dir = "mipmap-xxxhdpi"; legacy = 192; foreground = 432 }
)

foreach ($d in $densities) {
    New-Icon $d.legacy $d.dir "ic_launcher.png" $true $false
    New-Icon $d.legacy $d.dir "ic_launcher_round.png" $true $true
    New-Icon $d.foreground $d.dir "ic_launcher_foreground.png" $false $false
}
