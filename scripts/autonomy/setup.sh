# Setup for run.sh (sourced): a throwaway server, boss from the config file,
# w1 and w2 through the real pairing flow, w1's local policy and the
# --settings file of w1's Claude Code session.

setup_server() {
  PORT=$(free_port)
  URL="http://127.0.0.1:$PORT"
  local token
  token=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
  mkdir -p "$ROOT/server/data" "$ROOT/boss/xdg/rack"
  # boss is the only file agent: admin (it makes the invites) and main (its
  # tasks carry delegated authority). Setting a role turns roles on. The
  # server refuses a config with fewer than two agents, so `idle` exists
  # only for that: its token is random and never used.
  cat >"$ROOT/server/racking.toml" <<EOF
bind = "127.0.0.1:$PORT"
data_dir = "$ROOT/server/data"

[agents.boss]
token = "$token"
admin = true
role = "main"

[agents.idle]
token = "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"

# Scenario C needs w2 -> w1. A link (rack link --pair) is not enough: the
# role default still lets a worker send only to main, so this opens it.
[permissions]
send = { w2 = ["w1", "boss"] }
EOF
  printf 'url = "%s"\ntoken = "%s"\n' "$URL" "$token" >"$ROOT/boss/xdg/rack/rack.conf"
  printf 'Authorization: Bearer %s\n' "$token" >"$ROOT/boss/auth.hdr"
  chmod 600 "$ROOT/server/racking.toml" "$ROOT/boss/xdg/rack/rack.conf" "$ROOT/boss/auth.hdr"
  clean server "$BIN/racking-server" --config "$ROOT/server/racking.toml" >"$ROOT/server/log" 2>&1 &
  local pid=""
  for _ in $(seq 50); do
    pid=$(pgrep -f -- "--config $ROOT/server/racking.toml")
    [ -n "$pid" ] && rk boss whoami >/dev/null 2>&1 && break
    sleep 0.2
  done
  track "$pid" racking-server
  say "server: $URL (pid $pid); boss: $(rk boss whoami | head -1)"
}

# rack link on boss, rack join on the agent, then boss confirms with the
# fingerprint the joiner printed. --yes-if-fingerprint is the one scripted
# concession: it stands in for the owner reading both screens.
pair() {
  local name=$1 out code id words
  out=$(rk boss link "$name" --role worker --no-wait)
  code=$(sed -n 's/^code: //p' <<<"$out")
  id=$(sed -n 's/^invite \([0-9]*\) .*/\1/p' <<<"$out")
  mkdir -p "$ROOT/$name/xdg"
  rk "$name" join "$code" --server "$URL" >"$ROOT/$name/join.log" 2>&1 &
  local join=$!
  for _ in $(seq 50); do
    words=$(sed -n 's/^fingerprint: //p' "$ROOT/$name/join.log")
    [ -n "$words" ] && break
    sleep 0.2
  done
  rk boss link --resume "$id" --yes-if-fingerprint "$words" >"$ROOT/$name/link.log" 2>&1
  wait "$join"
  say "pair $name: invite $id, fingerprint '$words'; $(grep -h -E '^(confirmed|denied|linked)' "$ROOT/$name/link.log" "$ROOT/$name/join.log" | tr '\n' ';')"
}

setup_policy() {
  local work=$ROOT/w1/work pol=$ROOT/w1/xdg/rack/policy.toml
  rk w1 policy init --workdir "$work" >/dev/null
  # Replace the template's diagnostics profile (it is the last section) with
  # the one under test: two fixed metric commands, nothing else. write is
  # the workdir's out/ only, so the worker can save the output it attaches.
  sed -i '/^\[profiles\.diagnostics\]/,$d' "$pol"
  cat >>"$pol" <<'EOF'
[profiles.diagnostics]
description = "Run a fixed set of read-only metric commands; save their output under out/."
enabled = true
allow_run = [
  { cmd = "/usr/bin/uptime" },
  { cmd = "/usr/bin/df", args = ["-h"] },
]
read = ["{workdir}"]
write = ["{workdir}/out"]
timeout_secs = 30
returns = ["stdout", "exit_code", "files:out/*"]
EOF
  mkdir -p "$work/out"
  rk w1 policy check | tee -a "$ROOT/run.log"
}

# The worker's --settings: what `rack policy settings` prints (hook on every
# tool, permissions, crossSessionInbound accept) plus a SessionStart hook that
# starts `rack listen` in the background, as docs/hooks/listen-research.md
# tested. The hook writes the listener's pid so cleanup can stop it.
setup_settings() {
  local listen="\"$BIN/rack\" listen </dev/null >/dev/null 2>>\"$ROOT/w1/listen.log\" & echo \$! >\"$ROOT/w1/listen.pid\""
  rk w1 policy settings |
    jq --arg cmd "$listen" '.hooks.SessionStart = [{hooks: [{type: "command", command: $cmd}]}]' \
      >"$ROOT/w1/settings.json"
  cp "$HERE/worker-CLAUDE.md" "$ROOT/w1/work/CLAUDE.md"
}

# The worker session: claude -p kept open on a fifo with no prompt at all,
# so every turn it takes is started by a message rack listen injects.
# --setting-sources project and --strict-mcp-config keep the owner's own
# settings, hooks, plugins and MCP servers out of it.
start_worker() {
  mkfifo "$ROOT/w1/stdin"
  (cd "$ROOT/w1/work" && exec env -i HOME="$HOME" LANG="${LANG:-C.UTF-8}" TERM=dumb \
    PATH="$BIN:$CLAUDE_DIR:/usr/bin:/bin" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" \
    XDG_CONFIG_HOME="$ROOT/w1/xdg" \
    "$CLAUDE_DIR/claude" -p --input-format stream-json --output-format stream-json --verbose \
    --model haiku --setting-sources project --settings "$ROOT/w1/settings.json" \
    --strict-mcp-config --tools Bash,Read,Write --debug-file "$ROOT/w1/debug.log" \
    <"$ROOT/w1/stdin" >"$ROOT/w1/session.jsonl" 2>"$ROOT/w1/session.err") &
  track $! claude
  # Holding the fifo open is what keeps the -p session alive between turns.
  exec 7>"$ROOT/w1/stdin"
  for _ in $(seq 60); do
    [ -s "$ROOT/w1/listen.pid" ] && break
    sleep 0.5
  done
  if [ -s "$ROOT/w1/listen.pid" ]; then
    track "$(cat "$ROOT/w1/listen.pid")" "rack listen"
    say "worker session up; rack listen pid $(cat "$ROOT/w1/listen.pid")"
    return 0
  fi
  say "worker session did not start rack listen; see $ROOT/w1/session.err"
  return 1
}

stop_worker() {
  exec 7>&-
  for _ in $(seq 20); do
    pgrep -f -- "--settings $ROOT/w1/settings.json" >/dev/null || break
    sleep 0.5
  done
}
