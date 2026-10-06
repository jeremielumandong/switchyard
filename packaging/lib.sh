# shellcheck shell=bash disable=SC2034
# Shared helpers for the packaging scripts. Source, don't execute.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGING_DIR="$REPO_ROOT/packaging"
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"

APP_NAME="Switchyard"
APP_BIN="switchyard"
CLI_BIN="swy"
APP_ID="io.github.jeremielumandong.switchyard"

# Version from [workspace.package] in the root Cargo.toml (the first top-level `version =`).
VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)}"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' not found. $2"; }
