#!/bin/sh
# Install the Racking clients (rack, racking-mcp) for the current user on Linux.
#
#   install/install.sh [--from DIR] [--bin-dir DIR] [--dry-run]
#   install/install.sh --uninstall [--bin-dir DIR] [--dry-run]
#   curl -fsSL https://raw.githubusercontent.com/yerasuu/Racking/main/install/install.sh | sh -s -- [--version X.Y.Z]
#
# From a checkout: uses the binaries in DIR (default target/release), building
# them with cargo first when they are missing. Run from a pipe (no checkout) or
# with --version: downloads the release from GitHub and checks its SHA-256.
# No root needed. Safe to run again.
set -eu

usage() {
    cat <<'EOF2'
usage: install.sh [--from DIR] [--bin-dir DIR] [--dry-run]
       install.sh --uninstall [--bin-dir DIR] [--dry-run]

  --version X.Y.Z download that GitHub release instead of using a checkout
                  (the default when run without a checkout, e.g. from curl)
  --from DIR      directory with the rack and racking-mcp binaries
                  (default: target/release, built with cargo if missing)
  --bin-dir DIR   where to install them (default ~/.local/bin)
  --uninstall     remove both binaries; rack.conf is left in place
  --dry-run       print what would be done and change nothing
EOF2
}

REPO=yerasuu/Racking
VERSION=
CHECKOUT=0
if [ -f "$0" ]; then
    SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
    ROOT=$(dirname -- "$SCRIPT_DIR")
    [ -f "$ROOT/Cargo.toml" ] && CHECKOUT=1
fi
[ "$CHECKOUT" = 1 ] && FROM=$ROOT/target/release || FROM=
BIN_DIR=${HOME:?HOME is not set}/.local/bin
UNINSTALL=0
DRY=0
EXPLICIT_FROM=0

while [ $# -gt 0 ]; do
    case $1 in
        --version) [ $# -ge 2 ] || { usage >&2; exit 2; }; VERSION=${2#v}; shift 2 ;;
        --from) [ $# -ge 2 ] || { usage >&2; exit 2; }; FROM=$2; EXPLICIT_FROM=1; shift 2 ;;
        --bin-dir) [ $# -ge 2 ] || { usage >&2; exit 2; }; BIN_DIR=$2; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        --dry-run) DRY=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

run() {
    if [ "$DRY" = 1 ]; then say "[dry-run] $*"; else "$@"; fi
}

if [ "$UNINSTALL" = 1 ]; then
    run rm -f "$BIN_DIR/rack" "$BIN_DIR/racking-mcp"
    say "Removed rack and racking-mcp from $BIN_DIR."
    say "The config (~/.config/rack/rack.conf) was left in place."
    exit 0
fi

fetch() { # fetch URL OUT
    if command -v curl >/dev/null 2>&1; then curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then wget -q -O "$2" "$1"
    else die "curl or wget is needed to download the release"; fi
}

if [ -n "$VERSION" ] || { [ -z "$FROM" ] && [ "$EXPLICIT_FROM" = 0 ]; }; then
    [ "$EXPLICIT_FROM" = 0 ] || die "--from and --version cannot be combined"
    case $VERSION in *[!0-9.]* ) die "bad --version (stable releases only, X.Y.Z): $VERSION" ;; esac
    [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] ||
        die "no prebuilt release for $(uname -s) $(uname -m); build from source: cargo install --git https://github.com/$REPO rack racking-mcp"
    ASSET=rack-x86_64-unknown-linux-gnu.tar.gz
    if [ -n "${RACKING_RELEASE_URL:-}" ]; then BASE=$RACKING_RELEASE_URL # for tests
    elif [ -n "$VERSION" ]; then BASE=https://github.com/$REPO/releases/download/v$VERSION
    else BASE=https://github.com/$REPO/releases/latest/download; fi
    if [ "$DRY" = 1 ]; then
        say "[dry-run] download $BASE/$ASSET and $BASE/SHA256SUMS, verify, unpack, install"
        FROM=
    else
        WORK=$(mktemp -d) && trap 'rm -rf "$WORK"' EXIT
        say "Downloading $BASE/$ASSET"
        fetch "$BASE/$ASSET" "$WORK/$ASSET" || die "download failed (is there a release for this version?)"
        fetch "$BASE/SHA256SUMS" "$WORK/SHA256SUMS" || die "could not download SHA256SUMS"
        want=$(awk -v a="$ASSET" '$2 == a || $2 == "*" a { print $1 }' "$WORK/SHA256SUMS")
        have=$(sha256sum "$WORK/$ASSET" | awk '{ print $1 }')
        [ -n "$want" ] && [ "$want" = "$have" ] || die "SHA-256 mismatch for $ASSET: nothing installed"
        tar -xzf "$WORK/$ASSET" -C "$WORK" rack racking-mcp
        FROM=$WORK
    fi
elif [ ! -x "$FROM/rack" ] || [ ! -x "$FROM/racking-mcp" ]; then
    [ "$EXPLICIT_FROM" = 0 ] || die "rack and racking-mcp not found in $FROM"
    command -v cargo >/dev/null 2>&1 ||
        die "no binaries in $FROM and cargo is not installed (get Rust from https://rustup.rs, or pass --from DIR)"
    say "Building the clients with cargo (Rust 1.89 or newer)."
    if [ "$DRY" = 1 ]; then
        say "[dry-run] cargo build --release --locked -p rack -p racking-mcp"
    else
        (cd "$ROOT" && cargo build --release --locked -p rack -p racking-mcp)
    fi
fi

run mkdir -p "$BIN_DIR"
for name in rack racking-mcp; do
    # Copy beside the target, then rename: a running binary never sees a half-written file.
    tmp=$BIN_DIR/.$name.new.$$
    run cp "${FROM:-.}/$name" "$tmp"
    run chmod 755 "$tmp"
    run mv -f "$tmp" "$BIN_DIR/$name"
    if [ "$DRY" = 1 ]; then say "Would install: $BIN_DIR/$name"; else say "Installed: $BIN_DIR/$name"; fi
done

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Note: $BIN_DIR is not on your PATH. Add it, e.g. in ~/.profile:"
       say "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

say "Next: rack join CODE --server URL   (see README, \"Link a machine\"). Later updates: rack update."
