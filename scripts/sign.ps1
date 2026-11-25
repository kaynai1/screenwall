<#
.SYNOPSIS
    Sign a Windows executable with signtool.exe, then verify the signature.

.DESCRIPTION
    Locates the newest x64 signtool.exe under
    C:\Program Files (x86)\Windows Kits\10\bin\*\x64\ and runs

        signtool sign /fd sha256 /tr <TimestampUrl> /td sha256 ...

    The certificate can be given either as a thumbprint (cert in
    Cert:\CurrentUser\My, e.g. from scripts\make-dev-cert.ps1) or as a PFX file
    plus password. Afterwards `signtool verify /pa /v` runs.

    Exit code 0 means the file was signed AND verified. A self-signed
    certificate makes verification fail with a chain-trust error (0x800B0109);
    that is expected and reported clearly, not hidden. See docs/signing.md.

.PARAMETER File
    Path to the .exe / .msi / .dll to sign. Required.

.PARAMETER Thumbprint
    Certificate thumbprint in Cert:\CurrentUser\My. Required unless -PfxPath.

.PARAMETER PfxPath
    Path to a .pfx. Required unless -Thumbprint.

.PARAMETER PfxPassword
    PFX password. Falls back to $env:SCREENWALL_PFX_PASSWORD.

.PARAMETER TimestampUrl
    RFC 3161 timestamp server. Default http://timestamp.digicert.com.
    Pass '' to skip timestamping (signature then expires with the cert).

.PARAMETER SkipVerify
    Sign only; do not run signtool verify.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 `
        -File target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe `
        -Thumbprint 0123456789ABCDEF0123456789ABCDEF01234567

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 `
        -File target\release\screenwall-client.exe `
        -PfxPath target\certs\screenwall-dev.pfx -PfxPassword $env:SCREENWALL_PFX_PASSWORD
#>
[CmdletBinding(DefaultParameterSetName = 'Thumbprint')]
param(
    [Parameter(Mandatory = $true)]
    [string]$File,

    [Parameter(Mandatory = $true, ParameterSetName = 'Thumbprint')]
    [string]$Thumbprint,

    [Parameter(Mandatory = $true, ParameterSetName = 'Pfx')]
    [string]$PfxPath,

    [Parameter(ParameterSetName = 'Pfx')]
    [string]$PfxPassword,

    [string]$TimestampUrl = 'http://timestamp.digicert.com',

    [switch]$SkipVerify
)

$ErrorActionPreference = 'Stop'

# --- resolve target ----------------------------------------------------------
if (-not (Test-Path -LiteralPath $File)) {
    throw "File not found: $File"
}
$File = (Resolve-Path -LiteralPath $File).Path
Write-Host "Signing: $File"

# --- locate signtool ---------------------------------------------------------
function Find-SignTool {
    $base = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (-not (Test-Path $base)) { return $null }

    $candidates = Get-ChildItem -Path $base -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Name } -Descending

    foreach ($dir in $candidates) {
        $exe = Join-Path $dir.FullName 'x64\signtool.exe'
        if (Test-Path $exe) { return $exe }
    }
    return $null
}

$signtool = Find-SignTool
if (-not $signtool) {
    throw 'signtool.exe not found under C:\Program Files (x86)\Windows Kits\10\bin\*\x64. Install the Windows SDK (or "Windows SDK Signing Tools for Desktop Apps").'
}
Write-Host "signtool: $signtool"

# --- build sign arguments ----------------------------------------------------
$signArgs = @('sign', '/fd', 'sha256', '/v')

if ($PSCmdlet.ParameterSetName -eq 'Thumbprint') {
    $normalized = ($Thumbprint -replace '\s', '').ToUpperInvariant()
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Thumbprint -eq $normalized } | Select-Object -First 1
    if (-not $cert) {
        throw "Certificate $normalized not found in Cert:\CurrentUser\My. Run scripts\make-dev-cert.ps1 first, or pass -PfxPath."
    }
    Write-Host "Certificate: $($cert.Subject) [$normalized]"
    $signArgs += @('/sha1', $normalized)
}
else {
    if (-not (Test-Path -LiteralPath $PfxPath)) { throw "PFX not found: $PfxPath" }
    if (-not $PfxPassword) { $PfxPassword = $env:SCREENWALL_PFX_PASSWORD }
    if (-not $PfxPassword) { throw 'No PFX password. Pass -PfxPassword or set $env:SCREENWALL_PFX_PASSWORD.' }
    Write-Host "Certificate: $PfxPath"
    $signArgs += @('/f', (Resolve-Path -LiteralPath $PfxPath).Path, '/p', $PfxPassword)
}

if ($TimestampUrl) {
    $signArgs += @('/tr', $TimestampUrl, '/td', 'sha256')
}
$signArgs += $File

# --- sign --------------------------------------------------------------------
Write-Host ''
Write-Host "Running: signtool $($signArgs -join ' ')" -ForegroundColor Cyan
& $signtool @signArgs
$signExit = $LASTEXITCODE
if ($signExit -ne 0) {
    Write-Host "SIGN FAILED (exit $signExit)" -ForegroundColor Red
    exit $signExit
}
Write-Host 'SIGN OK' -ForegroundColor Green

if ($SkipVerify) {
    Write-Host 'Verification skipped (-SkipVerify).'
    exit 0
}

# --- verify ------------------------------------------------------------------
Write-Host ''
Write-Host 'Running: signtool verify /pa /v' -ForegroundColor Cyan
& $signtool verify /pa /v $File
$verifyExit = $LASTEXITCODE

Write-Host ''
if ($verifyExit -eq 0) {
    Write-Host 'VERIFY OK (chain trusted, signature intact)' -ForegroundColor Green
    exit 0
}

Write-Host "VERIFY FAILED (exit $verifyExit)" -ForegroundColor Yellow
Write-Host @'
The file IS signed. Verification failed on certificate trust, which is the
normal result for a self-signed certificate: the root is not in the machine's
trusted root store, so SmartScreen still warns.

  - "A certificate chain processed, but terminated in a root certificate which
     is not trusted by the trust provider" (0x800B0109) -> expected for the dev cert.
  - To make verify pass on THIS machine only, import the certificate into
    "Trusted Root Certification Authorities" (see docs/signing.md). Do not ship
    that root to users.
  - To clear SmartScreen for end users you need a real OV/EV code-signing
    certificate; an EV certificate avoids the initial warning.
'@
exit 1
