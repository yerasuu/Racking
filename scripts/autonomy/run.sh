#!/usr/bin/env bash
# Autonomy round trip test (board #50): a real Claude Code worker session,
# woken only by `rack listen`, takes tasks from a scripted main agent under
# its local policy. See docs/autonomy-test.md for what it proves.
#
# Usage: scripts/autonomy/run.sh [--keep] [A B C D E]
#   --keep  keep the temp root (logs, transcript, audit) even when all pass.
#   letters the Claude scenarios to run (default all); D2 and F, which need
#           no Claude session, always run.
# Needs: target/release/{rack,racking-server} of this branch, claude on PATH
# (logged in), jq, curl, python3. A full run costs about 0.40 USD of haiku.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BIN=$REPO/target/release
CLAUDE_DIR=$(dirname "$(command -v claude)")
KEEP=""
[ "${1:-}" = --keep ] && KEEP=--keep && shift
WANT=" ${*:-A B C D E} "
want() { [[ $WANT == *" $1 "* ]] && budget_ok; }
ROOT=$(mktemp -d -t racking-autonomy.XXXXXX)
# shellcheck source=lib.sh
. "$HERE/lib.sh"
# shellcheck source=setup.sh
. "$HERE/setup.sh"
# shellcheck source=scenarios.sh
. "$HERE/scenarios.sh"

cleanup() {
  stop_worker 2>/dev/null
  kill_tracked
  if [ "$KEEP" = --keep ] || grep -q $'\tFAIL\t' "$ROOT/results.tsv" 2>/dev/null; then
    echo "kept for inspection: $ROOT"
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT

"$BIN/rack" --version | grep -q '^rack 0\.2\.0' || {
  echo "build this branch first: cargo build --release --locked" >&2
  exit 2
}
say "root $ROOT; $("$BIN/rack" --version); $("$CLAUDE_DIR/claude" --version)"

mkdir -p "$ROOT/evidence"
setup_server
pair w1
pair w2
# Workers reach only main by default; scenario C needs w2 -> w1.
rk boss link --pair w1 w2 | tee -a "$ROOT/run.log"
setup_policy
setup_settings

if start_worker; then
  want A && scenario_a
  want B && scenario_b
  want C && scenario_c
  want D && scenario_d
  want E && scenario_e
  stop_worker
  say "worker usage: $(usage | jq -c .)"
else
  verdict session FAIL "no Claude session; A-E not run with a real session"
fi
scenario_f

say "results:"
column -t -s $'\t' "$ROOT/results.tsv" | tee -a "$ROOT/run.log"
! grep -q $'\tFAIL\t' "$ROOT/results.tsv"
