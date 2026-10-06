#Requires -Version 5.1
# Fail closed before a Windows release is uploaded: the staged executables and the installer
# must carry a valid, timestamped signature from the expected publisher.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Installer,
    [string]$StageDir,
    [string]$ExpectedSubject = $env:SWITCHYARD_SIGN_EXPECTED_SUBJECT
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security') -ErrorAction Stop
if (-not $ExpectedSubject) { throw 'Set SWITCHYARD_SIGN_EXPECTED_SUBJECT to the certificate profile subject.' }
if (-not $StageDir) { $StageDir = Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path 'target\package\windows' }

$files = @($Installer) + @('switchyard.exe', 'swy.exe' | ForEach-Object { Join-Path $StageDir $_ })
foreach ($path in $files) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing file to verify: $path" }
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate) {
        throw "Invalid or untimestamped signature: $path ($($signature.Status))"
    }
    if ($signature.SignerCertificate.Subject -cne $ExpectedSubject) {
        throw "Unexpected publisher for $path : $($signature.SignerCertificate.Subject)"
    }
    Write-Host "Verified: $path"
}
