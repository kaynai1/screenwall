# Verify GDI screen capture while the monitor is powered off (DPMS).
# Capture baseline -> broadcast SC_MONITORPOWER off -> wait 3s -> capture again -> restore.
# PASS means the desktop keeps composing while the monitor is off.

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class DisplayPower {
    [DllImport("user32.dll", CharSet = CharSet.Auto)]
    public static extern IntPtr SendMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
}
"@

$HWND_BROADCAST = [IntPtr]::new(0xffff)
$WM_SYSCOMMAND = 0x0112
$SC_MONITORPOWER = 0xF170
$MONITOR_OFF = [IntPtr]::new(2)
$MONITOR_ON = [IntPtr]::new(-1)

function Get-ScreenBitmap {
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    } finally {
        $g.Dispose()
    }
    return $bmp
}

function Get-Stats([System.Drawing.Bitmap]$bmp) {
    $sum = 0.0
    $sumSq = 0.0
    $count = 0
    $min = 255
    $max = 0
    for ($y = 0; $y -lt $bmp.Height; $y += 16) {
        for ($x = 0; $x -lt $bmp.Width; $x += 16) {
            $c = $bmp.GetPixel($x, $y)
            $lum = 0.2126 * $c.R + 0.7152 * $c.G + 0.0722 * $c.B
            $sum += $lum
            $sumSq += $lum * $lum
            $count++
            if ($lum -lt $min) { $min = $lum }
            if ($lum -gt $max) { $max = $lum }
        }
    }
    $mean = $sum / $count
    $variance = [Math]::Max(0, ($sumSq / $count) - ($mean * $mean))
    [pscustomobject]@{
        Mean = [Math]::Round($mean, 2)
        StdDev = [Math]::Round([Math]::Sqrt($variance), 2)
        Min = [Math]::Round($min, 0)
        Max = [Math]::Round($max, 0)
    }
}

function Get-Diff([System.Drawing.Bitmap]$a, [System.Drawing.Bitmap]$b) {
    $sum = 0.0
    $count = 0
    for ($y = 0; $y -lt $a.Height; $y += 16) {
        for ($x = 0; $x -lt $a.Width; $x += 16) {
            $ca = $a.GetPixel($x, $y)
            $cb = $b.GetPixel($x, $y)
            $sum += [Math]::Abs($ca.R - $cb.R) + [Math]::Abs($ca.G - $cb.G) + [Math]::Abs($ca.B - $cb.B)
            $count += 3
        }
    }
    return [Math]::Round($sum / $count, 2)
}

$outDir = Join-Path $PSScriptRoot "..\ui\screenshots"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$beforePath = Join-Path $outDir "monitor-on.png"
$afterPath = Join-Path $outDir "monitor-off.png"

$before = Get-ScreenBitmap
$beforeStats = Get-Stats $before
$before.Save($beforePath, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host ("[monitor-off] baseline: mean={0} stddev={1} min={2} max={3}" -f $beforeStats.Mean, $beforeStats.StdDev, $beforeStats.Min, $beforeStats.Max)
Write-Host ("[monitor-off] saved: {0}" -f $beforePath)

try {
    [void][DisplayPower]::SendMessage($HWND_BROADCAST, $WM_SYSCOMMAND, [IntPtr]$SC_MONITORPOWER, $MONITOR_OFF)
    Write-Host "[monitor-off] monitor off requested, waiting 3s..."
    Start-Sleep -Seconds 3
    $after = Get-ScreenBitmap
    $afterStats = Get-Stats $after
    $after.Save($afterPath, [System.Drawing.Imaging.ImageFormat]::Png)
    Write-Host ("[monitor-off] while off: mean={0} stddev={1} min={2} max={3}" -f $afterStats.Mean, $afterStats.StdDev, $afterStats.Min, $afterStats.Max)
    Write-Host ("[monitor-off] saved: {0}" -f $afterPath)

    $diff = Get-Diff $before $after
    Write-Host ("[monitor-off] mean pixel diff (0-255): {0}" -f $diff)

    if ($afterStats.StdDev -ge 5 -and $afterStats.Mean -ge 5 -and $diff -le 30) {
        Write-Host "[monitor-off] RESULT: PASS - desktop still composes, GDI capture works with monitor off"
    } elseif ($afterStats.StdDev -lt 5 -or $afterStats.Mean -lt 5) {
        Write-Host "[monitor-off] RESULT: BLACK - capture is black while monitor off"
    } else {
        Write-Host "[monitor-off] RESULT: CHANGED - image differs, inspect screenshots"
    }
} finally {
    [void][DisplayPower]::SendMessage($HWND_BROADCAST, $WM_SYSCOMMAND, [IntPtr]$SC_MONITORPOWER, $MONITOR_ON)
    [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point 10, 10
    Write-Host "[monitor-off] monitor restore requested"
    $before.Dispose()
}
