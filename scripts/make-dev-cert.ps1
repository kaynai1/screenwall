<#
.SYNOPSIS
    Create a self-signed code-signing certificate for local ScreenWall builds.

.DESCRIPTION
    Creates a CodeSigning certificate "CN=ScreenWall Dev" in Cert:\CurrentUser\My,
    exports it (with the private key) to target\certs\screenwall-dev.pfx, and
    prints the thumbprint. Works in Windows PowerShell 5.1 and needs no admin
    rights: CurrentUser\My is per-user.

    This certificate does NOT remove the SmartScreen warning. A self-signed cert
    is not in the Microsoft reputation system, so SmartScreen still treats the
    installer as unknown. It only proves the file was not modified after signing
    to anyone who chooses to trust the cert. See docs/signing.md.

    The PFX password comes from the SCREENWALL_PFX_PASSWORD environment variable.
    If it is unset, a random password is generated and written to
    target\certs\pfx-password.txt so the export step can be reproduced.

.PARAMETER Subject
    Certificate subject. Default "CN=ScreenWall Dev".

.PARAMETER OutDir
    Where to write the .pfx. Default <repo>\target\certs.

.PARAMETER Years
    Validity in years. Default 3.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-dev-cert.ps1

.EXAMPLE
    $env:SCREENWALL_PFX_PASSWORD = 'hunter2'
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-dev-cert.ps1
#>
[CmdletBinding()]
param(
    [string]$Subject = 'CN=ScreenWall Dev',
    [string]$OutDir,
    [int]$Years = 3
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $Root 'target\certs' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$pfxPath = Join-Path $OutDir 'screenwall-dev.pfx'
$pwdPath = Join-Path $OutDir 'pfx-password.txt'

# --- password ----------------------------------------------------------------
$password = $env:SCREENWALL_PFX_PASSWORD
$generated = $false
if ([string]::IsNullOrEmpty($password)) {
    $bytes = New-Object byte[] 24
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $password = [Convert]::ToBase64String($bytes)
    $generated = $true
}
$securePwd = ConvertTo-SecureString -String $password -AsPlainText -Force

# --- existing cert? ----------------------------------------------------------
$existing = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $Subject -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

if ($existing) {
    Write-Host "Reusing existing certificate:" -ForegroundColor Yellow
    $cert = $existing
}
else {
    Write-Host "Creating self-signed code-signing certificate '$Subject'..."
    $cert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $Subject `
        -CertStoreLocation Cert:\CurrentUser\My `
        -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable `
        -KeySpec Signature `
        -NotAfter (Get-Date).AddYears($Years) `
        -TextExtension @(
            '2.5.29.37={text}1.3.6.1.5.5.7.3.3',   # EKU: Code Signing
            '2.5.29.19={text}'                      # BasicConstraints: not a CA
        )
}

# --- export ------------------------------------------------------------------
Export-PfxCertificate -Cert $cert -FilePath $pfxPath -Password $securePwd | Out-Null
if ($generated) {
    Set-Content -Path $pwdPath -Value $password -Encoding ASCII -NoNewline
}

# --- report ------------------------------------------------------------------
Write-Host ''
Write-Host '================ dev code-signing certificate ================' -ForegroundColor White
Write-Host ("Subject    : {0}" -f $cert.Subject)
Write-Host ("Thumbprint : {0}" -f $cert.Thumbprint) -ForegroundColor Green
Write-Host ("Store      : Cert:\CurrentUser\My\{0}" -f $cert.Thumbprint)
Write-Host ("NotAfter   : {0}" -f $cert.NotAfter)
Write-Host ("PFX        : {0}" -f $pfxPath)
if ($generated) {
    Write-Host ("Password   : written to {0}" -f $pwdPath) -ForegroundColor Yellow
}
else {
    Write-Host "Password   : from `$env:SCREENWALL_PFX_PASSWORD"
}
Write-Host ''
Write-Host 'Sign an exe:' -ForegroundColor Cyan
Write-Host ("  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\sign.ps1 -File target\release\bundle\nsis\ScreenWall_0.1.0_x64-setup.exe -Thumbprint {0}" -f $cert.Thumbprint)
Write-Host ''
Write-Host 'Remove the certificate when done (optional):' -ForegroundColor Cyan
Write-Host ("  Remove-Item Cert:\CurrentUser\My\{0}" -f $cert.Thumbprint)
Write-Host '  Remove-Item target\certs\screenwall-dev.pfx, target\certs\pfx-password.txt'
Write-Host ''
Write-Host 'Reminder: a self-signed cert does NOT clear the SmartScreen warning.' -ForegroundColor Yellow

# Emit the thumbprint on the last line so callers can capture it.
Write-Output $cert.Thumbprint
