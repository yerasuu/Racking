# Helpers for run.sh (sourced, not run). Everything lives under $ROOT, made by
# run.sh with mktemp -d, so nothing here reads or writes the owner's real
# ~/.config/rack, ~/.claude settings or real racking-server.

# Every agent runs with a clean environment: the session running this script
# may export CLAUDE_CODE_MESSAGING_SOCKET and friends, and a child that saw
# them would talk to the wrong inbox. XDG_CONFIG_HOME moves rack's config
# directory (rack.conf, state, policy, audit) into the agent's temp home.
clean() {
  local who=$1
  shift
  env -i HOME="$HOME" LANG="${LANG:-C.UTF-8}" TERM=dumb \
    PATH="$BIN:$CLAUDE_DIR:/usr/bin:/bin" \
    XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" \
    XDG_CONFIG_HOME="$ROOT/$who/xdg" "$@"
}

# rack as one agent, e.g. `rk boss send w1 ...`.
rk() {
  local who=$1
  shift
  clean "$who" "$BIN/rack" "$@"
}

# Raw API read as boss, for fields rack prints only in prose (kind, hops,
# reply_to, from_role). The token goes through a header file so it never
# shows on a command line.
api() {
  curl -sS -H @"$ROOT/boss/auth.hdr" "$URL$1"
}

say() { printf '%s\n' "$*" | tee -a "$ROOT/run.log"; }

# A result line both on screen and in results.tsv, which the report is
# built from.
verdict() {
  local scenario=$1 result=$2
  shift 2
  printf '%s\t%s\t%s\n' "$scenario" "$result" "$*" >>"$ROOT/results.tsv"
  say "== $scenario: $result - $*"
}

# Remember a pid we started together with a word its cmdline must contain,
# so cleanup never kills a recycled pid or someone else's process.
track() { printf '%s %s\n' "$1" "$2" >>"$ROOT/pids"; }

kill_tracked() {
  [ -f "$ROOT/pids" ] || return 0
  local pid word
  while read -r pid word; do
    if [ -r "/proc/$pid/cmdline" ] && tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q -- "$word"; then
      kill "$pid" 2>/dev/null && say "stopped pid $pid ($word)"
    fi
  done <"$ROOT/pids"
}

free_port() {
  python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}

# Waits until a task leaves `received`/`accepted`, or SECS pass; prints
# the final state. Polls the API, since a busy worker may take a while.
wait_state() {
  local job=$1 secs=$2 state=""
  local end=$((SECONDS + secs))
  while [ $SECONDS -lt $end ]; do
    state=$(api "/jobs/$job" | jq -r '.state // .job.state // empty')
    case $state in done | failed | rejected | cancelled) break ;; esac
    sleep 2
  done
  printf '%s\n' "$state"
}

# The Claude session's usage so far, from the stream-json result events
# (one per woken turn). `usage` is per turn, so it is summed, but
# total_cost_usd already counts the whole session, so the last one is kept.
usage() {
  jq -s '[.[] | select(.type=="result")] | {wakes: length, api_turns: (map(.num_turns) | add // 0),
    cost_usd: (last.total_cost_usd // 0),
    input: (map(.usage.input_tokens) | add // 0), output: (map(.usage.output_tokens) | add // 0),
    cache_read: (map(.usage.cache_read_input_tokens) | add // 0),
    cache_write: (map(.usage.cache_creation_input_tokens) | add // 0)}' "$ROOT/w1/session.jsonl"
}

# Bash commands the worker session asked to run since line N of its
# transcript (allowed or not: the hook decides after the request).
bash_calls_since() {
  tail -n +"$1" "$ROOT/w1/session.jsonl" |
    jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") |
      "\(.name): \(.input.command // .input.file_path // .input | tostring)"'
}
