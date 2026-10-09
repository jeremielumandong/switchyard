#!/bin/sh
# Install Switchyard on Linux (x86_64) for the current user: no sudo, no Rust toolchain.
#
#   curl -fsSL https://raw.githubusercontent.com/jeremielumandong/switchyard/main/install.sh | sh
#
# Downloads the published AppImage from GitHub Releases, verifies its SHA-256 checksum, and
# installs:
#   ~/Applications/Switchyard.AppImage            the app
#   ~/.local/bin/switchyard, ~/.local/bin/swy     launcher command and the `swy` CLI
#   ~/.local/share/applications/switchyard.desktop, icons   desktop launcher
#
# Options (after `sh -s --` when piped, e.g. `curl … | sh -s -- --uninstall`):
#   --build            from a source checkout: build the AppImage, then install it
#   PATH.AppImage      install a local AppImage instead of downloading
#   --stop-running     quit a running installed Switchyard first (it is never killed silently)
#   --uninstall        remove the AppImage, commands, desktop entry and icons
#   -h, --help         show this help
#
# Environment:
#   SWITCHYARD_VERSION    release tag to install, e.g. v0.1.7 (default: the latest release)
#   SWITCHYARD_APPIMAGE   install path (default ~/Applications/Switchyard.AppImage)
#
# Your connections, history and settings live in ~/.local/share/switchyard and are never
# touched; secrets stay in the keychain or vault. Rerun to update (quit Switchyard first).
#
# The whole script is one function, parsed before anything runs, so `curl | sh` never
# executes a partly downloaded file.

switchyard_install() (
    set -eu

    repo=jeremielumandong/switchyard
    releases="https://github.com/$repo/releases"
    asset=Switchyard-linux-x86_64.AppImage

    data_home=${XDG_DATA_HOME:-$HOME/.local/share}
    dest=${SWITCHYARD_APPIMAGE:-$HOME/Applications/Switchyard.AppImage}
    bin_dir=$HOME/.local/bin
    wrapper=$bin_dir/switchyard
    cli_link=$bin_dir/swy
    desktop_file=$data_home/applications/switchyard.desktop
    icon_root=$data_home/icons/hicolor
    marker='X-Switchyard-Installer=install.sh'
    wrapper_tag='Launch the Switchyard AppImage'

    if [ -t 2 ]; then
        log() { printf '\033[36m==>\033[0m %s\n' "$*"; }
        warn() { printf '\033[33mwarn:\033[0m %s\n' "$*" >&2; }
        fail() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
    else
        log() { printf '==> %s\n' "$*"; }
        warn() { printf 'warn: %s\n' "$*" >&2; }
        fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
    fi

    build=0 stop=0 uninstall=0 source=''
    for arg in "$@"; do
        case $arg in
            --build) build=1 ;;
            --stop-running) stop=1 ;;
            --uninstall) uninstall=1 ;;
            -h|--help)
                printf '%s\n' \
                    'Install Switchyard on Linux (x86_64) for the current user.' '' \
                    'Usage: install.sh [--build | PATH.AppImage] [--stop-running]' \
                    '       install.sh --uninstall [--stop-running]' '' \
                    'With no argument the latest GitHub release is downloaded and verified.' \
                    'SWITCHYARD_VERSION=vX.Y.Z picks a release; SWITCHYARD_APPIMAGE sets the path.'
                exit 0 ;;
            -*) fail "unknown option '$arg' (try --help)" ;;
            *) source=$arg ;;
        esac
    done

    [ "$(uname -s)" = Linux ] || fail 'This installer supports Linux only. See the README for Windows and macOS.'
    case $dest in /*) ;; *) fail "SWITCHYARD_APPIMAGE must be an absolute path (got '$dest')" ;; esac

    # ---- running instances ------------------------------------------------------------
    # Only a Switchyard started from the AppImage being replaced counts: the AppImage
    # runtime sets APPIMAGE to its path. A development build (target/*/switchyard) or an
    # AppImage elsewhere is never touched.
    installed_pids() {
        for pid in $(pgrep -x switchyard 2>/dev/null || true); do
            exe=$(readlink "/proc/$pid/exe" 2>/dev/null) || continue
            case $exe in /tmp/.mount_*) ;; *) continue ;; esac
            running=$(tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | sed -n 's/^APPIMAGE=//p')
            [ "$running" = "$dest" ] && echo "$pid"
        done
        return 0
    }
    guard_running() {
        pids=$(installed_pids)
        [ -n "$pids" ] || return 0
        if [ "$stop" -eq 0 ]; then
            fail "Switchyard is running from the installed AppImage (pid $(echo $pids)). Quit it first (it may have open transactions or unsaved edits), or rerun with --stop-running."
        fi
        log "Stopping Switchyard (pid $(echo $pids))"
        kill -TERM $pids 2>/dev/null || true
        i=0
        while [ $i -lt 20 ] && [ -n "$(installed_pids)" ]; do sleep 0.5; i=$((i + 1)); done
        pids=$(installed_pids)
        if [ -n "$pids" ]; then
            warn "Switchyard ignored SIGTERM for 10 s; sending SIGKILL"
            kill -KILL $pids 2>/dev/null || true
        fi
    }
    refresh_caches() {
        command -v update-desktop-database >/dev/null 2>&1 &&
            update-desktop-database "$data_home/applications" >/dev/null 2>&1 || true
        command -v gtk-update-icon-cache >/dev/null 2>&1 &&
            gtk-update-icon-cache -q -t "$icon_root" >/dev/null 2>&1 || true
    }

    # ---- uninstall ----------------------------------------------------------------------
    if [ "$uninstall" -eq 1 ]; then
        guard_running
        removed=0
        if [ -f "$dest" ]; then rm -f "$dest" "$dest".bak; log "Removed $dest"; removed=1; fi
        if [ -f "$wrapper" ] && grep -q "$wrapper_tag" "$wrapper"; then
            rm -f "$wrapper"; log "Removed $wrapper"; removed=1
        fi
        if [ -L "$cli_link" ] && [ "$(readlink "$cli_link")" = "$dest" ]; then
            rm -f "$cli_link"; log "Removed $cli_link"; removed=1
        fi
        if [ -f "$desktop_file" ] && grep -qx "$marker" "$desktop_file"; then
            rm -f "$desktop_file"; log "Removed $desktop_file"; removed=1
        fi
        for icon in "$icon_root"/*/apps/switchyard.png; do
            [ -f "$icon" ] && rm -f "$icon" && removed=1
        done
        refresh_caches
        [ "$removed" -eq 1 ] || log 'Nothing was installed'
        log "Your connections, history and settings stay in $data_home/switchyard; delete that folder yourself if you want them gone."
        exit 0
    fi

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    # ---- source ---------------------------------------------------------------------------
    if [ "$build" -eq 1 ]; then
        [ -z "$source" ] || fail 'give either --build or a path, not both'
        # Only when run as a file from a checkout: piped through `sh`, $0 is the shell.
        root=''
        case $0 in
            install.sh|*/install.sh) [ -f "$0" ] && root=$(cd "$(dirname "$0")" && pwd) ;;
        esac
        [ -n "$root" ] && [ -f "$root/packaging/linux/build-appimage.sh" ] ||
            fail '--build needs a source checkout: clone the repository and run ./install.sh --build there.'
        command -v bash >/dev/null 2>&1 || fail 'Required command missing: bash'
        bash "$root/packaging/linux/build-appimage.sh"
        source=$(ls -t "$root"/dist/Switchyard-*-"$(uname -m)".AppImage 2>/dev/null | head -n 1)
        [ -n "$source" ] || fail "the build produced no AppImage in $root/dist"
    elif [ -z "$source" ]; then
        [ "$(uname -m)" = x86_64 ] ||
            fail 'Release AppImages are x86_64 only. On other machines, build from source: ./install.sh --build'
        for tool in curl sha256sum mktemp; do
            command -v "$tool" >/dev/null 2>&1 || fail "Required command missing: $tool"
        done
        tag=${SWITCHYARD_VERSION:-}
        if [ -z "$tag" ]; then
            latest=$(curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 \
                -o /dev/null -w '%{url_effective}' "$releases/latest") ||
                fail 'Could not find a published release.'
            case $latest in
                "$releases/tag/"*) tag=${latest##*/} ;;
                *) fail 'Could not resolve the latest release.' ;;
            esac
        fi
        case $tag in v[0-9]*) ;; *) fail 'SWITCHYARD_VERSION must be a release tag such as v0.1.7.' ;; esac
        case $tag in *[!a-zA-Z0-9._-]*) fail 'Invalid release tag.' ;; esac
        log "Downloading Switchyard $tag"
        for file in "$asset" "$asset.sha256"; do
            curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 \
                "$releases/download/$tag/$file" -o "$work/$file" ||
                fail "Release $tag has no Linux AppImage ($file). Pick another release with SWITCHYARD_VERSION, or build from source: ./install.sh --build"
        done
        # Compare against the expected file only; never take paths from the checksum file.
        expected=$(cut -d ' ' -f 1 "$work/$asset.sha256")
        [ "${#expected}" -eq 64 ] || fail 'Invalid release checksum.'
        case $expected in *[!0-9a-f]*) fail 'Invalid release checksum.' ;; esac
        actual=$(sha256sum "$work/$asset" | cut -d ' ' -f 1)
        [ "$actual" = "$expected" ] || fail 'Checksum mismatch; nothing was installed.'
        log 'Checksum verified'
        source=$work/$asset
    fi

    [ -s "$source" ] || fail "$source is missing or empty"
    source=$(readlink -f "$source")
    [ "$(head -c 4 "$source" | od -An -c | tr -d ' \n')" = '177ELF' ] ||
        fail "$source is not an AppImage"
    chmod +x "$source" 2>/dev/null || true

    # Extract the CLI and icons (works without FUSE) to check the AppImage runs here.
    (cd "$work" && "$source" --appimage-extract 'usr/bin/swy' >/dev/null 2>&1) ||
        fail "$source could not be extracted; is it a Switchyard AppImage for $(uname -m)?"
    (cd "$work" && "$source" --appimage-extract 'usr/share/icons/*' >/dev/null 2>&1) || true
    version=$("$work/squashfs-root/usr/bin/swy" --version 2>/dev/null) ||
        fail "$source does not run on this system (swy --version failed). Check glibc and the libraries listed in the README."
    log "Checked: $version"

    unchanged=0
    if [ -f "$dest" ] && cmp -s "$source" "$dest"; then
        log "$dest is already this build"
        unchanged=1
    fi

    # ---- install ------------------------------------------------------------------------
    if [ "$unchanged" -eq 0 ]; then
        guard_running
        mkdir -p "$(dirname "$dest")"
        if [ -f "$dest" ]; then
            mv -f "$dest" "$dest.bak"
            log "Kept the previous version as $(basename "$dest").bak"
        fi
        # Copy beside the destination, then rename: never half-written.
        cp -f "$source" "$dest.new.$$"
        chmod 755 "$dest.new.$$"
        mv -f "$dest.new.$$" "$dest"
        log "Installed $dest"
    fi

    mkdir -p "$bin_dir"
    if [ -e "$wrapper" ] && ! grep -q "$wrapper_tag" "$wrapper" 2>/dev/null; then
        warn "$wrapper exists and was not written by this installer; leaving it alone"
    else
        cat >"$wrapper" <<EOF
#!/bin/sh
# $wrapper_tag. Written by install.sh.
APPIMAGE="\${SWITCHYARD_APPIMAGE:-$dest}"
if [ ! -x "\$APPIMAGE" ]; then
  echo "switchyard: \$APPIMAGE is missing or not executable" >&2
  exit 1
fi
exec "\$APPIMAGE" "\$@"
EOF
        chmod 755 "$wrapper"
        log "Command: $wrapper"
    fi
    # The AppImage runs the `swy` CLI when started through a link named swy.
    if [ -e "$cli_link" ] && ! { [ -L "$cli_link" ] && [ "$(readlink "$cli_link")" = "$dest" ]; }; then
        warn "$cli_link exists and was not written by this installer; leaving it alone"
    else
        ln -sfn "$dest" "$cli_link"
        log "CLI: $cli_link"
    fi

    if [ -e "$desktop_file" ] && ! grep -qx "$marker" "$desktop_file"; then
        warn "$desktop_file was edited by hand; leaving it alone"
    else
        mkdir -p "$(dirname "$desktop_file")"
        cat >"$desktop_file" <<EOF
[Desktop Entry]
Type=Application
Name=Switchyard
GenericName=Database and SSH Client
Comment=Database client, SSH terminals and file transfer behind one connection model
Exec=$wrapper %U
TryExec=$wrapper
Icon=switchyard
Terminal=false
Categories=Development;Database;Network;
Keywords=sql;postgres;postgresql;sqlserver;mssql;oracle;snowflake;ssh;sftp;ftp;database;
StartupWMClass=switchyard
$marker
EOF
        log "Desktop entry: $desktop_file"
    fi

    icons=$work/squashfs-root/usr/share/icons/hicolor
    if [ -d "$icons" ]; then
        (cd "$icons" && find . -type f -name 'switchyard.*') | while IFS= read -r f; do
            mkdir -p "$(dirname "$icon_root/$f")"
            cp -f "$icons/$f" "$icon_root/$f"
        done
        log 'Icons installed'
    else
        warn 'could not extract icons from the AppImage; the launcher may show a generic icon'
    fi
    refresh_caches

    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) warn "$bin_dir is not on your PATH, so 'switchyard' and 'swy' won't resolve in a terminal" ;;
    esac
    log "Done. Start Switchyard from your app launcher, or run 'switchyard'."
)

switchyard_install "$@"
