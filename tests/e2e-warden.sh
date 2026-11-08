#!/usr/bin/env bash
# The warden's end-to-end test on a real gateway (docs/warden.md): workers
# made, listed, driven and deleted through the warden only, by the office
# sandbox's probe (curl and socat), as zapperclaw will.
#
# Needs: an OpenShell gateway with the Firecracker driver and supervisor
# identity on; config/openshell.yaml pointing at it; the `office-edge` and
# `warden` providers (config/openshell-office-edge.yaml,
# config/openshell-warden.yaml); and, built:
#
#   make build
#   make image IMAGE=office-fake
#   make image IMAGE=zapperclaw-office
#
# Then: tests/e2e-warden.sh. KEEP=1 leaves the office sandbox (and a failed
# run's worker) for a look. EXEC overrides how a command runs in a sandbox
# (default `openshell sandbox exec NAME --`).
set -euo pipefail
cd "$(dirname "$0")/.."

WARDEN=(target/release/zapper-warden --config config/warden.yaml)
LOG=${LOG:-/tmp/zapper-warden-e2e.log}
EXEC=${EXEC:-"openshell sandbox exec"}
URL=http://host.openshell.internal:8972
STATE=~/.local/state/zapperclaw/warden.jsonl
SERVE=

fail() { echo "FAIL: $*" >&2; echo "(the warden's log: $LOG)" >&2; exit 1; }
ok() { echo "ok: $*"; }
in_sandbox() { local name=$1; shift; $EXEC "$name" -- "$@"; }
in_office() { in_sandbox zc-office "$@"; }
# The office calls the warden: METHOD PATH [BODY]; prints the answer's body.
call() {
  local method=$1 path=$2 body=${3:-}
  in_office curl -sS -X "$method" -H 'content-type: application/json' \
    ${body:+--data "$body"} "$URL$path" || true
}
status_of() { # METHOD PATH [BODY]: the status only
  local method=$1 path=$2 body=${3:-}
  in_office curl -sS -o /dev/null -w '%{http_code}' -X "$method" \
    -H 'content-type: application/json' ${body:+--data "$body"} "$URL$path"
}

cleanup() {
  [ -n "$SERVE" ] && kill "$SERVE" 2>/dev/null || true
  if [ "${KEEP:-}" != 1 ]; then
    openshell sandbox delete zcw-ada >/dev/null 2>&1 || true
    openshell sandbox delete zc-office >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

[ -x "${WARDEN[0]}" ] || fail "no ${WARDEN[0]}: make build"

# 1. The profiles prepare, and the templates are made.
"${WARDEN[@]}" check | grep -q '"template": "zcp-zapperclaw-fake"' || fail "check: no zcp-zapperclaw-fake"
"${WARDEN[@]}" apply >/dev/null || fail "apply"
ok "templates zc-office, zcp-zapperclaw-fake, zcp-claude"

# 2. The warden serves, having made the office sandbox.
"${WARDEN[@]}" serve >"$LOG" 2>&1 &
SERVE=$!
for _ in $(seq 1 360); do
  grep -q 'its routes on' "$LOG" && break
  kill -0 "$SERVE" 2>/dev/null || fail "the warden stopped"
  sleep 1
done
grep -q 'its routes on' "$LOG" || fail "the warden didn't start in 6 minutes"
ok "office sandbox zc-office, routes served"

# 3. Only the office's supervisor's token is served.
code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8972/workers)
[ "$code" = 401 ] || fail "a request without the office's token: $code, not 401"
ok "no token, no routes (401)"

# 4. Workers: made once, of an offered profile only.
[ "$(status_of GET /workers)" = 200 ] || fail "GET /workers"
call GET /workers | grep -q '"workers":\[\]' || fail "a fresh warden lists workers"
[ "$(status_of PUT /workers/ada '{"profile":"zapperclaw-fake"}')" = 201 ] || fail "PUT ada: not made"
call GET /workers | grep -q '"name":"ada"' || fail "ada isn't listed"
[ "$(status_of PUT /workers/ada '{"profile":"zapperclaw-fake"}')" = 200 ] || fail "PUT ada again: not 200"
[ "$(status_of PUT /workers/ada '{"profile":"claude"}')" = 409 ] || fail "ada of another profile: not 409"
[ "$(status_of PUT /workers/eve '{"profile":"k3s"}')" = 400 ] || fail "a profile not offered: not 400"
[ "$(status_of PUT /workers/Eve '{"profile":"zapperclaw-fake"}')" = 400 ] || fail "a bad name: not 400"
ok "ada made once, of an offered profile; the rest refused"

# 5. The workers' edge forward reaches the office sandbox's port 8970.
in_office bash -c 'setsid nohup socat TCP-LISTEN:8970,reuseaddr,fork EXEC:cat >/dev/null 2>&1 < /dev/null &'
sleep 1
exec 3<>/dev/tcp/127.0.0.1/8970 || fail "connecting to the workers' edge forward"
printf 'ping\n' >&3
read -r -t 10 line <&3 || fail "no answer through the forward"
exec 3>&-
[ "$line" = ping ] || fail "the forward answered $line"
ok "workers' edge forward into the office sandbox"

# 6. ada's agent, driven through the warden: a prompt in, a turn's result out.
TICKET=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
DRIVE=$(base64 -w0 <<'EOS'
#!/usr/bin/env bash
# One agent stream: its greeting, then a prompt as a data frame, then what the agent says.
read -r greeting
printf '%s\n' "$greeting" > /tmp/zc-greeting
body='{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello from the warden test"}]}}'$'\n'
n=${#body}
printf '\000'
printf "$(printf '\\%03o\\%03o\\%03o\\%03o' $((n >> 24 & 255)) $((n >> 16 & 255)) $((n >> 8 & 255)) $((n & 255)))"
printf '%s' "$body"
timeout 60 cat > /tmp/zc-agent.out
EOS
)
in_office bash -c "printf '%s' '$DRIVE' | base64 -d > /tmp/zc-drive.sh"
in_office bash -c 'rm -f /tmp/zc-agent.out /tmp/zc-greeting; setsid nohup socat TCP-LISTEN:8973,reuseaddr SYSTEM:"bash /tmp/zc-drive.sh" >/dev/null 2>&1 < /dev/null &'
sleep 1
[ "$(status_of POST /workers/ada/agent "{\"ticket\":\"$TICKET\"}")" = 200 ] || fail "starting ada's agent"
[ "$(status_of POST /workers/ada/agent '{"ticket":"short"}')" = 400 ] || fail "a bad ticket: not 400"
[ "$(status_of POST /workers/ada/agent "{\"ticket\":\"$TICKET\",\"resume\":\"--x\"}")" = 400 ] || fail "a resume flag: not 400"
for _ in $(seq 1 60); do
  in_office grep -aq '"type":"result"' /tmp/zc-agent.out 2>/dev/null && break
  sleep 1
done
in_office grep -aq '"type":"result"' /tmp/zc-agent.out || fail "no turn's result from ada's agent"
[ "$(in_office cat /tmp/zc-greeting)" = "ZCAGENT $TICKET" ] || fail "the stream didn't open with its ticket"
ok "ada's agent driven: its ticket, a prompt, its turn's result"

# 7. Deleted, and gone.
[ "$(status_of DELETE /workers/ada)" = 200 ] || fail "DELETE ada"
call GET /workers | grep -q '"workers":\[\]' || fail "ada is still listed"
[ "$(status_of DELETE /workers/ada)" = 404 ] || fail "DELETE ada again: not 404"
ok "ada deleted"

# 8. The warden's record.
for op in worker_made agent_started worker_deleted refused; do
  grep -q "\"op\":\"$op\"" "$STATE" || fail "no $op in $STATE"
done
ok "the warden's record: made, started, deleted, refused"

echo "all ok"
