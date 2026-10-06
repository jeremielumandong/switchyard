# Add or remove a directory on the user PATH (HKCU\Environment; no administrator rights
# needed). Installed next to swy.exe and run by the NSIS installer / uninstaller. Reads and
# writes the raw registry value so %VAR% entries and the REG_EXPAND_SZ type survive (NSIS
# strings are capped at 1024 chars; PATH often isn't).
param(
  [Parameter(Mandatory)][ValidateSet('add', 'remove')][string]$Action,
  [Parameter(Mandatory)][string]$Dir
)
$ErrorActionPreference = 'Stop'
$key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
try {
  $current = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  $norm = $Dir.TrimEnd('\')
  $parts = @($current -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $norm })
  if ($Action -eq 'add') { $parts += $norm }
  if ($parts.Count -eq 0) {
    # The user PATH held only this directory: drop the value rather than leave it empty.
    $key.DeleteValue('Path', $false)
  } else {
    $key.SetValue('Path', ($parts -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
  }
} finally {
  $key.Close()
}
