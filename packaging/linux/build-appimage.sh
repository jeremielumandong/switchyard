#!/usr/bin/env bash
# Build dist/Switchyard-<version>-<arch>.AppImage for the host architecture.
#
# Usage: packaging/linux/build-appimage.sh [--skip-build]
#
# Downloads appimagetool and the AppImage runtime once into target/package/tools, pinned by
# version and verified by SHA-256 (x86_64), or set APPIMAGETOOL=/path/to/appimagetool.
# Works without FUSE (APPIMAGE_EXTRACT_AND_RUN=1), so it runs in containers and CI.
#
# The AppImage expects the host to provide GPUI's system libraries (libxkbcommon(-x11),
# libwayland-client, libxcb, libfontconfig, libfreetype, the Vulkan loader). Build on the
# oldest distro you want to support, since glibc is not bundled.
source "$(dirname "$0")/../lib.sh"

SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
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
APPIMAGETOOL_VERSION=1.9.1
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_VERSION=20251108
RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
TOOLS="$REPO_ROOT/target/package/tools"

# Download $2 to $1 unless present, and check its SHA-256 either way.
fetch() {
  local dest="$1" url="$2" sha="$3"
  if [[ ! -f "$dest" ]]; then
    need curl "Install curl, or set APPIMAGETOOL."
    log "downloading $(basename "$dest")"
    mkdir -p "$(dirname "$dest")"
    curl -fL --retry 3 -o "$dest.part" "$url"
    mv -f "$dest.part" "$dest"
  fi
  if [[ "$(sha256sum "$dest" | cut -d' ' -f1)" != "$sha" ]]; then
    rm -f "$dest"
    die "checksum mismatch for $(basename "$dest"); the download was removed, run again"
  fi
  chmod 755 "$dest"
}

RUNTIME_ARGS=()
TOOL="${APPIMAGETOOL:-}"
if [[ -z "$TOOL" ]]; then
  [[ "$ARCH" == x86_64 ]] || die "pinned tools are x86_64 only; set APPIMAGETOOL for $ARCH"
  TOOL="$TOOLS/appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage"
  RUNTIME="$TOOLS/runtime-$RUNTIME_VERSION-$ARCH"
  fetch "$TOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$ARCH.AppImage" \
    "$APPIMAGETOOL_SHA256"
  fetch "$RUNTIME" \
    "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
    "$RUNTIME_SHA256"
  # Passed explicitly so appimagetool doesn't download an unpinned runtime.
  RUNTIME_ARGS=(--runtime-file "$RUNTIME")
fi

OUT="$DIST_DIR/$APP_NAME-$VERSION-$ARCH.AppImage"
log "creating $(basename "$OUT")"
rm -f "$OUT"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" VERSION="$VERSION" "$TOOL" --no-appstream \
  "${RUNTIME_ARGS[@]}" "$APPDIR" "$OUT"
chmod +x "$OUT"

log "done -> $OUT"
ls -lh "$OUT"
