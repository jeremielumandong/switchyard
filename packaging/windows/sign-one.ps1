#Requires -Version 5.1
# Sign files with Azure Trusted Signing. makensis runs this through !uninstfinalize and
# !finalize, for the uninstaller it embeds and for the finished installer.
[CmdletBinding()]
param([Parameter(Mandatory, Position = 0, ValueFromRemainingArguments)][string[]]$Path)
$ErrorActionPreference = 'Stop'
try {
    . "$PSScriptRoot/trusted-signing.ps1"
    Invoke-TrustedSign -Context (New-TrustedSigningContext) -Path $Path
} catch {
    Write-Error $_
    exit 1
}
exit 0
