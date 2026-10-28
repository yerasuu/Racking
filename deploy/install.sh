#!/bin/sh
# Install racking-server as a systemd user service (see docs/deploy.md).
#
#   deploy/install.sh [--bin PATH] [--bind ADDR] [--dry-run]
#   deploy/install.sh --uninstall [--dry-run]
#
# Safe to run again: an existing server.toml is never touched.
set -eu

usage() {
    cat <<'EOF'
usage: install.sh [--bin PATH] [--bind ADDR] [--dry-run]
       install.sh --uninstall [--dry-run]

  --bin PATH    racking-server binary to install (default ./racking-server)
  --bind ADDR   bind address for a NEW server.toml (default 127.0.0.1:7878)
  --uninstall   stop and remove the unit and the binary; config and data stay
  --dry-run     print what would be done and change nothing
EOF
}

BIN=./racking-server
BIND=127.0.0.1:7878
UNINSTALL=0
DRY=0

while [ $# -gt 0 ]; do
    case $1 in
        --bin) [ $# -ge 2 ] || { usage >&2; exit 2; }; BIN=$2; shift 2 ;;
        --bind) [ $# -ge 2 ] || { usage >&2; exit 2; }; BIND=$2; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        --dry-run) DRY=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

: "${HOME:?HOME is not set}"
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
BIN_DIR=$HOME/.local/bin
CONF_DIR=$HOME/.config/racking
DATA_DIR=$HOME/.local/share/racking
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
CONF=$CONF_DIR/server.toml
UNIT=racking-server.service

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# run CMD ARGS...: execute, or only print under --dry-run.
run() {
    if [ "$DRY" = 1 ]; then
        say "[dry-run] $*"
    else
        "$@"
    fi
}

# The user manager must answer, or systemctl calls only warn.
HAVE_SYSTEMD=0
if command -v systemctl >/dev/null 2>&1; then
    case $(systemctl --user is-system-running 2>/dev/null || true) in
        running | degraded | starting) HAVE_SYSTEMD=1 ;;
    esac
fi
[ "$HAVE_SYSTEMD" = 1 ] || warn "no systemd user manager reachable: systemctl steps are skipped"

# sctl ARGS...: systemctl --user, tolerated when no user manager is reachable.
sctl() {
    if [ "$HAVE_SYSTEMD" = 1 ]; then
        run systemctl --user "$@"
    else
        say "[skipped, no user manager] systemctl --user $*"
    fi
}

if [ "$UNINSTALL" = 1 ]; then
    sctl disable --now "$UNIT" || warn "could not disable $UNIT (not installed?)"
    run rm -f "$UNIT_DIR/$UNIT" "$BIN_DIR/racking-server"
    sctl daemon-reload
    say "Removed the unit and $BIN_DIR/racking-server."
    say "Config and data were left in place. To delete them by hand:"
    say "  rm -r '$CONF_DIR' '$DATA_DIR'"
    say "To stop starting at boot for all your services: loginctl disable-linger \"\$USER\""
    exit 0
fi

[ -f "$BIN" ] || die "binary not found: $BIN (use --bin PATH)"
[ -x "$BIN" ] || die "binary is not executable: $BIN"
[ -f "$SCRIPT_DIR/$UNIT" ] || die "missing $SCRIPT_DIR/$UNIT"
[ -f "$SCRIPT_DIR/../config.example.toml" ] || die "missing $SCRIPT_DIR/../config.example.toml"
case $BIND in
    *[!0-9A-Za-z.:\[\]-]* | '') die "bad --bind value: $BIND" ;;
esac
case $DATA_DIR in
    *[\"\\]*) die "HOME contains a quote or backslash; cannot write server.toml" ;;
esac

# Directories: private, because they hold the tokens and the database.
run mkdir -p "$BIN_DIR" "$UNIT_DIR"
run mkdir -p -m 700 "$CONF_DIR" "$DATA_DIR"
run chmod 700 "$CONF_DIR" "$DATA_DIR"

# Binary: copy next to the target, then rename, so a running server never
# sees a half-written file.
TMP_BIN=$BIN_DIR/.racking-server.new.$$
run cp "$BIN" "$TMP_BIN"
run chmod 755 "$TMP_BIN"
run mv -f "$TMP_BIN" "$BIN_DIR/racking-server"
say "Binary: $BIN_DIR/racking-server"

# Config: only when absent.
gen_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 32
    else
        od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
    fi
}
if [ -e "$CONF" ]; then
    say "Config exists, left untouched: $CONF"
elif [ "$DRY" = 1 ]; then
    say "[dry-run] would create $CONF (mode 600) from config.example.toml with fresh tokens, bind $BIND"
else
    TMP_CONF=$CONF_DIR/.server.toml.new.$$
    trap 'rm -f "$TMP_CONF"' EXIT
    (
        umask 077
        while IFS= read -r line; do
            case $line in
                bind\ =*) printf 'bind = "%s"\n' "$BIND" ;;
                data_dir\ =*) printf 'data_dir = "%s"\n' "$DATA_DIR" ;;
                token\ =*) printf 'token = "%s"\n' "$(gen_token)" ;;
                *) printf '%s\n' "$line" ;;
            esac
        done <"$SCRIPT_DIR/../config.example.toml" >"$TMP_CONF"
    )
    chmod 600 "$TMP_CONF"
    mv "$TMP_CONF" "$CONF"
    trap - EXIT
    say "Config created (mode 600) with new random tokens: $CONF"
    say "Read the tokens there when you configure each agent; they are not printed."
fi

# Unit.
WAS_ACTIVE=0
if [ "$HAVE_SYSTEMD" = 1 ] && systemctl --user is-active --quiet "$UNIT" 2>/dev/null; then
    WAS_ACTIVE=1
fi
run cp "$SCRIPT_DIR/$UNIT" "$UNIT_DIR/$UNIT.new.$$"
run mv -f "$UNIT_DIR/$UNIT.new.$$" "$UNIT_DIR/$UNIT"
sctl daemon-reload
sctl enable --now "$UNIT" || warn "could not start $UNIT: see journalctl --user -u racking-server"
if [ "$WAS_ACTIVE" = 1 ]; then
    sctl restart "$UNIT" || warn "could not restart $UNIT"
fi

# Linger: lets the user manager (and this service) start at boot and keep
# running without anyone logging in.
if [ "$HAVE_SYSTEMD" = 0 ]; then
    warn "skipping loginctl enable-linger (no user manager); run it by hand to start at boot"
elif command -v loginctl >/dev/null 2>&1; then
    say "Enabling linger so the service starts at boot, without a login."
    run loginctl enable-linger "${USER:-$(id -un)}" ||
        warn "could not enable linger; the service will only run while you are logged in"
else
    warn "loginctl not found: the service will only run while you are logged in"
fi

say "Done. Check: systemctl --user status racking-server"
