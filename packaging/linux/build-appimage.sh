#!/usr/bin/env bash
# Build dist/Switchyard-<version>-<arch>.AppImage for the host architecture.
#
# Usage: packaging/linux/build-appimage.sh [--skip-build]
#
# Uses `appimagetool` from PATH, or downloads it into target/package/tools. Set
# APPIMAGETOOL=/path/to/appimagetool to pin a specific copy. Works without FUSE
# (APPIMAGE_EXTRACT_AND_RUN=1), so it runs in containers and CI.
#
# The AppImage expects the host to provide GPUI's system libraries (libxkbcommon(-x11),
# libwayland-client, libxcb, libfontconfig, libfreetype, the Vulkan loader). Build on the
# oldest distro you want to support, since glibc is not bundled.
source "$(dirname "$0")/../lib.sh"

SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == "Linux" ]] || die "AppImage packaging must run on Linux"
need cargo "Install Rust from https://rustup.rs"

ARCH="$(uname -m)"
cd "$REPO_ROOT"

if [[ $SKIP_BUILD -eq 0 ]]; then
  log "cargo build --release"
  cargo build --release --locked -p switchyard-app -p switchyard-cli
fi

WORK="$REPO_ROOT/target/package/linux"
APPDIR="$WORK/$APP_NAME.AppDir"
rm -rf "$APPDIR" && mkdir -p "$APPDIR" "$DIST_DIR"

log "assembling AppDir"
install -Dm755 "target/release/$APP_BIN" "$APPDIR/usr/bin/$APP_BIN"
install -Dm755 "target/release/$CLI_BIN" "$APPDIR/usr/bin/$CLI_BIN"
install -Dm755 "$PACKAGING_DIR/linux/AppRun" "$APPDIR/AppRun"
install -Dm644 "$PACKAGING_DIR/linux/switchyard.desktop" \
  "$APPDIR/usr/share/applications/switchyard.desktop"
install -Dm644 "$PACKAGING_DIR/icons/switchyard-256.png" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps/switchyard.png"
install -Dm644 "$REPO_ROOT/crates/app/assets/fonts/OFL-Geist.txt" \
  "$APPDIR/usr/share/doc/switchyard/OFL-Geist.txt"
ln -s usr/share/applications/switchyard.desktop "$APPDIR/switchyard.desktop"
ln -s usr/share/icons/hicolor/256x256/apps/switchyard.png "$APPDIR/switchyard.png"
ln -s switchyard.png "$APPDIR/.DirIcon"

# --- appimagetool --------------------------------------------------------------------------
TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [[ -z "$TOOL" ]]; then
  need curl "Install curl, or put appimagetool on PATH."
  TOOL="$REPO_ROOT/target/package/tools/appimagetool-$ARCH.AppImage"
  if [[ ! -x "$TOOL" ]]; then
    log "downloading appimagetool ($ARCH)"
    mkdir -p "$(dirname "$TOOL")"
    curl -fsSL -o "$TOOL" \
      "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
  fi
fi

OUT="$DIST_DIR/$APP_NAME-$VERSION-$ARCH.AppImage"
log "creating $(basename "$OUT")"
rm -f "$OUT"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" VERSION="$VERSION" "$TOOL" --no-appstream "$APPDIR" "$OUT"
chmod +x "$OUT"

log "done -> $OUT"
ls -lh "$OUT"
