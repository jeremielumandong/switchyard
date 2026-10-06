#!/usr/bin/env bash
# Build the release package for the current platform.
#   macOS  -> dist/Switchyard-<ver>-macos-universal.dmg (+ .pkg)
#   Linux  -> dist/Switchyard-<ver>-<arch>.AppImage
#   Windows: run packaging/windows/build-windows.ps1 from PowerShell instead.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
case "$(uname -s)" in
  Darwin) exec "$here/macos/build-macos.sh" "$@" ;;
  Linux)  exec "$here/linux/build-appimage.sh" "$@" ;;
  MINGW*|MSYS*|CYGWIN*)
    exec powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$here/windows/build-windows.ps1" "$@" ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac
