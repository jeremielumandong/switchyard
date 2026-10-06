<#
.SYNOPSIS
  Build the Switchyard NSIS installer: dist\Switchyard-<version>-windows-x64-setup.exe

.PARAMETER SkipBuild
  Reuse the existing release binaries in target\release.

.NOTES
  Needs Rust (rustup) and NSIS 3 (`winget install NSIS.NSIS` or `choco install nsis`).
  Optional Authenticode signing (skipped when unset), applied to both exes and the installer:
    WINDOWS_SIGN_THUMBPRINT   SHA-1 thumbprint of a cert in the user/machine store, or
    WINDOWS_SIGN_PFX + WINDOWS_SIGN_PFX_PASSWORD
    WINDOWS_SIGN_TIMESTAMP    RFC 3161 URL (default http://timestamp.digicert.com)
#>
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$DistDir = if ($env:DIST_DIR) { $env:DIST_DIR } else { Join-Path $RepoRoot 'dist' }
$Target = 'x86_64-pc-windows-msvc'

function Log($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# Version from [workspace.package] in the root Cargo.toml.
$Version = $env:VERSION
if (-not $Version) {
  $m = Select-String -Path (Join-Path $RepoRoot 'Cargo.toml') -Pattern '^version = "(.+)"' | Select-Object -First 1
  if (-not $m) { throw 'could not read version from Cargo.toml' }
  $Version = $m.Matches[0].Groups[1].Value
}
# VIProductVersion needs four numeric parts: 1.2.3-beta.1 -> 1.2.3.0
$core = ($Version -split '[-+]')[0].Split('.')
$VersionQuad = (@($core) + @('0', '0', '0', '0'))[0..3] -join '.'

Set-Location $RepoRoot

if (-not $SkipBuild) {
  rustup target add $Target | Out-Null
  Log "cargo build --release --target $Target"
  cargo build --release --locked --target $Target -p switchyard-app -p switchyard-cli
  if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}

$BinDir = Join-Path $RepoRoot "target\$Target\release"
foreach ($exe in 'switchyard.exe', 'swy.exe') {
  if (-not (Test-Path (Join-Path $BinDir $exe))) { throw "missing $BinDir\$exe (run without -SkipBuild)" }
}

# --- signing ------------------------------------------------------------------------------
function Find-SignTool {
  $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
  $found = Get-ChildItem $kits -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1
  if ($found) { return $found.FullName }
  throw 'signtool.exe not found (install the Windows SDK)'
}

function Sign-File($path) {
  if (-not $env:WINDOWS_SIGN_THUMBPRINT -and -not $env:WINDOWS_SIGN_PFX) { return }
  $tool = Find-SignTool
  $ts = if ($env:WINDOWS_SIGN_TIMESTAMP) { $env:WINDOWS_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }
  $signArgs = @('sign', '/fd', 'sha256', '/tr', $ts, '/td', 'sha256')
  if ($env:WINDOWS_SIGN_THUMBPRINT) {
    $signArgs += @('/sha1', $env:WINDOWS_SIGN_THUMBPRINT)
  } else {
    $signArgs += @('/f', $env:WINDOWS_SIGN_PFX)
    if ($env:WINDOWS_SIGN_PFX_PASSWORD) { $signArgs += @('/p', $env:WINDOWS_SIGN_PFX_PASSWORD) }
  }
  Log "signing $(Split-Path $path -Leaf)"
  & $tool @signArgs $path
  if ($LASTEXITCODE -ne 0) { throw "signing $path failed" }
}

# Sign copies in a staging dir so target\ keeps the unsigned build artifacts untouched.
$Stage = Join-Path $RepoRoot 'target\package\windows'
Remove-Item $Stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage, $DistDir | Out-Null
foreach ($exe in 'switchyard.exe', 'swy.exe') {
  Copy-Item (Join-Path $BinDir $exe) $Stage
  Sign-File (Join-Path $Stage $exe)
}

# --- NSIS ---------------------------------------------------------------------------------
$makensis = Get-Command makensis.exe -ErrorAction SilentlyContinue
$makensis = if ($makensis) { $makensis.Source } else {
  @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $makensis) { throw 'makensis not found. Install NSIS 3: winget install NSIS.NSIS' }

$OutFile = Join-Path $DistDir "Switchyard-$Version-windows-x64-setup.exe"
Log "creating $(Split-Path $OutFile -Leaf)"
& $makensis /V2 /INPUTCHARSET UTF8 `
  "/DVERSION=$Version" "/DVERSION_QUAD=$VersionQuad" `
  "/DBIN_DIR=$Stage" "/DREPO_ROOT=$RepoRoot" "/DOUT_FILE=$OutFile" `
  (Join-Path $PSScriptRoot 'switchyard.nsi')
if ($LASTEXITCODE -ne 0) { throw 'makensis failed' }

Sign-File $OutFile
Log "done -> $OutFile"
Get-Item $OutFile | Format-Table Name, Length
