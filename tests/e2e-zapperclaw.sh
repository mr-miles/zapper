#!/usr/bin/env bash
# zapperclaw's end-to-end test on a real gateway (docs/zapperclaw.md): the
# warden makes the office sandbox and runs zapperclaw in it; zapperclaw has
# the warden make its roster's worker (a fake Claude Code that answers the
# board) and drives its agent; the board mails it through the board's
# forward, and the worker's answer comes back through its tools at the
# office's edge, through the workers' forward.
#
# Needs what tests/e2e-warden.sh needs (a gateway with the Firecracker
# driver and supervisor identity on, config/openshell.yaml pointing at it,
# the `office-edge` and `warden` providers), and, built:
#
#   make build
#   make image IMAGE=office-fake
#   make image IMAGE=zapperclaw-office
#
# Then: tests/e2e-zapperclaw.sh. KEEP=1 leaves the sandboxes for a look.
# EXEC overrides how a command runs in a sandbox (default `openshell
# sandbox exec NAME --`).
set -euo pipefail
cd "$(dirname "$0")/.."

WARDEN=(target/release/zapper-warden --config config/warden.yaml)
LOG=${LOG:-/tmp/zapperclaw-e2e.log}
EXEC=${EXEC:-"openshell sandbox exec"}
STATE=~/.local/state/zapperclaw
BOARD=http://127.0.0.1:8974
SERVE=

fail() { echo "FAIL: $*" >&2; echo "(the warden's log, with the office's: $LOG)" >&2; exit 1; }
ok() { echo "ok: $*"; }
in_office() { $EXEC zc-office -- "$@"; }

cleanup() {
  [ -n "$SERVE" ] && kill "$SERVE" 2>/dev/null || true
  if [ "${KEEP:-}" != 1 ]; then
    openshell sandbox delete zcw-echo >/dev/null 2>&1 || true
    openshell sandbox delete zc-office >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

[ -x "${WARDEN[0]}" ] || fail "no ${WARDEN[0]}: make build"
[ -x target/release/zapperclaw ] || fail "no target/release/zapperclaw: make build"

# 1. The templates, the office's with zapperclaw's config.
"${WARDEN[@]}" check | grep -q '"template": "zcp-zapperclaw-echo"' || fail "check: no zcp-zapperclaw-echo"
"${WARDEN[@]}" apply >/dev/null || fail "apply"
ok "templates zc-office, zcp-zapperclaw-echo"

# 2. The warden serves and runs zapperclaw in the office sandbox.
"${WARDEN[@]}" serve >"$LOG" 2>&1 &
SERVE=$!
for _ in $(seq 1 360); do
  grep -q 'zapperclaw: 1 workers' "$LOG" && break
  kill -0 "$SERVE" 2>/dev/null || fail "the warden stopped"
  sleep 1
done
grep -q 'zapperclaw: 1 workers' "$LOG" || fail "zapperclaw didn't start in 6 minutes"
ok "zapperclaw runs in zc-office"

# 3. The board: its token, from the warden's state, never printed.
TOKEN=$(cat "$STATE/board-token")
board() { # METHOD PATH [BODY]: the answer's body
  local method=$1 path=$2 body=${3:-}
  curl -sS -X "$method" -H "Authorization: Bearer $TOKEN" -H 'X-Zapper-UI: 1' \
    -H 'content-type: application/json' ${body:+--data "$body"} "$BOARD$path"
}
code=$(curl -s -o /dev/null -w '%{http_code}' "$BOARD/api/workers")
[ "$code" = 401 ] || fail "the board without its token: $code, not 401"
for _ in $(seq 1 360); do
  board GET /api/workers | grep -q '"state":"idle"' && break
  sleep 1
done
board GET /api/workers | grep -q '"state":"idle"' || fail "echo isn't idle: $(board GET /api/workers)"
ok "the board: its token only; echo's sandbox made, echo idle"

# 4. Mail to echo, and its answer in the thread.
sent=$(board POST /api/mail '{"to":["echo"],"subject":"Ping","body":"Are you there?"}')
thread=$(echo "$sent" | sed -n 's/.*"thread":"\(t-[0-9]*\)".*/\1/p')
[ -n "$thread" ] || fail "sending: $sent"
for _ in $(seq 1 180); do
  board GET /api/inbox | grep -q 'Re: Ping' && break
  sleep 1
done
board GET /api/inbox | grep -q 'Re: Ping' || fail "no answer from echo: $(board GET /api/workers)"
board GET /api/thread/$thread | grep -q 'echo: Ping' || fail "the answer isn't echo's: $(board GET /api/thread/$thread)"
ok "the board mailed echo; echo answered in the thread, through the edge"

# 5. The drive over the edge: echo publishes a file with `office drive`,
#    fetches it back from the shared drive, and says what it read.
sent=$(board POST /api/mail '{"to":["echo"],"subject":"Publish","body":"Put a note on the drive."}')
pub=$(echo "$sent" | sed -n 's/.*"thread":"\(t-[0-9]*\)".*/\1/p')
[ -n "$pub" ] || fail "sending: $sent"
for _ in $(seq 1 180); do
  board GET /api/inbox | grep -q 'Re: Publish' && break
  sleep 1
done
board GET /api/thread/$pub | grep -q 'published and fetched: from echo' || fail "echo didn't publish and fetch: $(board GET /api/thread/$pub)"
board GET /api/drive/echo/notes | grep -q "\"$pub.txt\"" || fail "no echo/notes/$pub.txt on the shared drive: $(board GET /api/drive/echo/notes)"
ok "echo published to the shared drive and fetched it back, over the edge"

# 6. The office's record, in its sandbox; the warden's, on the host.
in_office grep -q '"event":"end"' /home/agent/.local/state/zapperclaw/journal.jsonl || fail "no turn's end in the office's journal"
for op in office_started worker_made agent_started; do
  grep -q "\"op\":\"$op\"" "$STATE/warden.jsonl" || fail "no $op in the warden's record"
done
ok "the office's journal has the turn; the warden's record the office, echo and its agent"

echo "all ok"
