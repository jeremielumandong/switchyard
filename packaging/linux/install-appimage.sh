#!/usr/bin/env bash
# Install the Switchyard AppImage for the current user.
#
# Usage: packaging/linux/install-appimage.sh [--build] [--no-cli] [--uninstall] [APPIMAGE]
#
#   --build      run build-appimage.sh first (otherwise uses the newest dist/*.AppImage)
#   --no-cli     do not link `swy` into ~/.local/bin
#   --uninstall  remove the AppImage, desktop entry, icon and `swy` link
#   APPIMAGE     install this file instead of the newest one in dist/
#
# Installs to ~/Applications/Switchyard.AppImage (an existing copy is kept as
# Switchyard.AppImage.bak-<timestamp>), writes ~/.local/share/applications/switchyard.desktop,
# installs the icon into ~/.local/share/icons/hicolor, and links ~/.local/bin/swy to the
# AppImage (AppRun dispatches to the CLI when invoked as `swy`).
source "$(dirname "$0")/../lib.sh"

BUILD=0
CLI=1
UNINSTALL=0
SRC=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --build) BUILD=1; shift ;;
    --no-cli) CLI=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) SRC="$1"; shift ;;
  esac
done

[[ "$(uname -s)" == "Linux" ]] || die "AppImage install must run on Linux"

APPS_DIR="${APPS_DIR:-$HOME/Applications}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
TARGET="$APPS_DIR/$APP_NAME.AppImage"
DESKTOP="$DATA_DIR/applications/$APP_BIN.desktop"
ICON="$DATA_DIR/icons/hicolor/256x256/apps/$APP_BIN.png"
CLI_LINK="$BIN_DIR/$CLI_BIN"

refresh_caches() {
  command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database -q "$DATA_DIR/applications" || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -q -t "$DATA_DIR/icons/hicolor" || true
}

if [[ $UNINSTALL -eq 1 ]]; then
  log "uninstalling"
  rm -f "$TARGET" "$DESKTOP" "$ICON"
  # Only remove the swy link if it points at our AppImage.
  if [[ -L "$CLI_LINK" && "$(readlink "$CLI_LINK")" == "$TARGET" ]]; then
    rm -f "$CLI_LINK"
  fi
  refresh_caches
  log "done (backups in $APPS_DIR left in place)"
  exit 0
fi

if [[ $BUILD -eq 1 ]]; then
  "$PACKAGING_DIR/linux/build-appimage.sh"
fi

if [[ -z "$SRC" ]]; then
  SRC="$(ls -t "$DIST_DIR"/"$APP_NAME"-*-"$(uname -m)".AppImage 2>/dev/null | head -n1 || true)"
  [[ -n "$SRC" ]] || die "no AppImage in $DIST_DIR; run with --build or pass a path"
fi
[[ -f "$SRC" ]] || die "not a file: $SRC"

mkdir -p "$APPS_DIR" "$(dirname "$DESKTOP")" "$(dirname "$ICON")"

if [[ -e "$TARGET" ]]; then
  BACKUP="$TARGET.bak-$(date +%Y%m%d-%H%M%S)"
  log "backing up existing install -> $(basename "$BACKUP")"
  mv "$TARGET" "$BACKUP"
fi

log "installing $(basename "$SRC") -> $TARGET"
install -Dm755 "$SRC" "$TARGET"

log "installing desktop entry and icon"
sed "s|^Exec=.*|Exec=$TARGET %U|" "$PACKAGING_DIR/linux/switchyard.desktop" > "$DESKTOP"
install -Dm644 "$PACKAGING_DIR/icons/switchyard-256.png" "$ICON"

if [[ $CLI -eq 1 ]]; then
  mkdir -p "$BIN_DIR"
  if [[ -e "$CLI_LINK" && ! -L "$CLI_LINK" ]]; then
    log "skipping $CLI_LINK: a non-symlink file already exists"
  else
    log "linking $CLI_LINK -> $TARGET"
    ln -sfn "$TARGET" "$CLI_LINK"
  fi
fi

refresh_caches
log "done -> $TARGET"
