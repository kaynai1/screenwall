<#
.SYNOPSIS
    Local equivalent of the GitHub Actions "build-and-test" job.

.DESCRIPTION
    Runs the same checks that .github/workflows/ci.yml runs on windows-latest:
      1. cargo fmt --all -- --check
      2. cargo clippy --workspace --all-targets -- -D warnings
      3. cargo test --workspace
      4. ui: npm ci (only when node_modules is missing) -> tsc --noEmit -> npm run build

    Cargo is always invoked through scripts\cargo.bat. That wrapper enters the MSVC
    environment first; running plain `cargo` from Git Bash picks up the coreutils
    link.exe from /usr/bin and fails to link.

    Every step prints its duration and result. Any failing step makes the script
    exit non-zero. A summary table is printed at the end.

.PARAMETER SkipClippy
    Skip the clippy step.

.PARAMETER SkipUi
    Skip the whole UI section (npm ci / tsc / build).

.PARAMETER SkipFmt
    Skip the rustfmt check.

.PARAMETER SkipTest
    Skip cargo test.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ci.ps1

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ci.ps1 -SkipUi -SkipClippy
#>
[CmdletBinding()]
param(
    [switch]$SkipFmt,
    [switch]$SkipClippy,
    [switch]$SkipTest,
    [switch]$SkipUi
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = Split-Path -Parent $PSScriptRoot
$CargoBat = Join-Path $PSScriptRoot 'cargo.bat'
$UiDir = Join-Path $Root 'ui'

$Results = New-Object System.Collections.Generic.List[object]

function Write-Step([string]$Text) {
    Write-Host ''
    Write-Host "==> $Text" -ForegroundColor Cyan
}

function Invoke-Step {
    param(
        [string]$Name,
        [scriptblock]$Action
    )
    Write-Step $Name
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $ok = $true
    $detail = ''
    try {
        & $Action
        if ($LASTEXITCODE -ne 0) { $ok = $false; $detail = "exit code $LASTEXITCODE" }
    }
    catch {
        $ok = $false
        $detail = $_.Exception.Message
    }
    $sw.Stop()
    $status = if ($ok) { 'PASS' } else { 'FAIL' }
    $color = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("{0,-28} {1}  ({2:N1}s) {3}" -f $Name, $status, $sw.Elapsed.TotalSeconds, $detail) -ForegroundColor $color
    $Results.Add([pscustomobject]@{
            Step     = $Name
            Status   = $status
            Seconds  = [math]::Round($sw.Elapsed.TotalSeconds, 1)
            Detail   = $detail
        })
    # Intentionally returns nothing: the caller must not pipe this to Out-Null,
    # otherwise the native command's stdout (test results, fmt diffs) is lost.
}

function Invoke-Cargo {
    param([string[]]$CargoArgs)
    # cmd /c keeps the .bat wrapper in charge of the MSVC environment.
    & cmd /c "`"$CargoBat`" $($CargoArgs -join ' ')"
}

function Invoke-Npm {
    param([string[]]$NpmArgs)
    Push-Location $UiDir
    try {
        & npm @NpmArgs
    }
    finally {
        Pop-Location
    }
}

Write-Host "ScreenWall CI (local)" -ForegroundColor White
Write-Host "root: $Root"
Write-Host "time: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"

# --- 1. rustfmt ---------------------------------------------------------------
if ($SkipFmt) {
    $Results.Add([pscustomobject]@{ Step = 'cargo fmt --check'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipFmt' })
}
else {
    Invoke-Step 'cargo fmt --all -- --check' { Invoke-Cargo @('fmt', '--all', '--', '--check') }
}

# --- 2. clippy ----------------------------------------------------------------
# -D warnings: the workspace is clippy-clean as of 2026-11-24, and CI enforces the
# same. A new lint should fail this step rather than accumulate.
if ($SkipClippy) {
    $Results.Add([pscustomobject]@{ Step = 'cargo clippy'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipClippy' })
}
else {
    Invoke-Step 'cargo clippy --workspace --all-targets -- -D warnings' { Invoke-Cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') }
}

# --- 3. tests -----------------------------------------------------------------
if ($SkipTest) {
    $Results.Add([pscustomobject]@{ Step = 'cargo test --workspace'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipTest' })
}
else {
    Invoke-Step 'cargo test --workspace' { Invoke-Cargo @('test', '--workspace') }
}

# --- 4. UI --------------------------------------------------------------------
if ($SkipUi) {
    $Results.Add([pscustomobject]@{ Step = 'ui npm ci'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipUi' })
    $Results.Add([pscustomobject]@{ Step = 'ui tsc --noEmit'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipUi' })
    $Results.Add([pscustomobject]@{ Step = 'ui npm run build'; Status = 'SKIP'; Seconds = 0; Detail = '-SkipUi' })
}
else {
    if (Test-Path (Join-Path $UiDir 'node_modules')) {
        Write-Step 'ui npm ci (skipped: node_modules exists)'
        $Results.Add([pscustomobject]@{ Step = 'ui npm ci'; Status = 'SKIP'; Seconds = 0; Detail = 'node_modules present' })
    }
    else {
        Invoke-Step 'ui npm ci' { Invoke-Npm @('ci') }
    }

    Invoke-Step 'ui tsc --noEmit' { Invoke-Npm @('run', 'typecheck') }
    Invoke-Step 'ui npm run build' { Invoke-Npm @('run', 'build') }
}

# --- summary ------------------------------------------------------------------
Write-Host ''
Write-Host '================ summary ================' -ForegroundColor White
$Results | Format-Table -AutoSize Step, Status, Seconds, Detail | Out-String | Write-Host

$failed = @($Results | Where-Object { $_.Status -eq 'FAIL' })
if ($failed.Count -gt 0) {
    Write-Host ("FAILED: {0} step(s)" -f $failed.Count) -ForegroundColor Red
    exit 1
}
Write-Host 'ALL CHECKS PASSED' -ForegroundColor Green
exit 0
