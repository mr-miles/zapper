#!/bin/sh
# End-to-end tests of the office: a small firm of real VMs whose workers run
# the fake Claude (`zapper-agent fake-claude`), so runs are deterministic and
# cost no tokens.
#
#   tests/e2e-office.sh [path/to/zapper]
#
# Needs what tests/e2e.sh needs (KVM, the ubuntu-26.04 image, network). Runs
# its own daemon under a temporary directory. ZAPPER_E2E_KEEP=1 keeps it.
set -u

Z=${1:-target/release/zapper}
Z=$(cd "$(dirname "$Z")" && pwd)/$(basename "$Z")
T=$(mktemp -d /tmp/zapper-office.XXXXXX)
export ZAPPER_SOCKET=$T/api.sock ZAPPER_CONFIG=$T/zapper.yaml
fails=0

pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1"; echo "      $2"; fails=$((fails + 1)); }
expect() { case "$2" in *"$3"*) pass "$1" ;; *) fail "$1" "expected '$3' in: $(echo "$2" | tail -c 400)" ;; esac; }
refute() { case "$2" in *"$3"*) fail "$1" "did not expect '$3'" ;; *) pass "$1" ;; esac; }
journal() { "$Z" firm journal acme; }
cjournal() { "$Z" firm journal corp; }
# Waits up to $2 seconds for corp's journal to contain $1.
cwait_for() {
    i=0
    while [ $i -lt "$2" ]; do
        cjournal 2>/dev/null | grep -q -- "$1" && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}
# Waits up to $2 seconds for the journal to contain $1.
wait_for() {
    i=0
    while [ $i -lt "$2" ]; do
        journal 2>/dev/null | grep -q -- "$1" && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}

cleanup() {
    [ -n "${DAEMON:-}" ] && kill "$DAEMON" 2>/dev/null && wait "$DAEMON" 2>/dev/null
    [ -n "${KEEPER:-}" ] && kill "$KEEPER" 2>/dev/null && wait "$KEEPER" 2>/dev/null
    [ -n "${ZAPPER_E2E_KEEP:-}" ] && echo "kept $T" || rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/work" "$T/fake"
PORT=$((20000 + $$ % 20000))
DATA=$HOME/.local/share/zapper
cat > "$T/fake/ceo.yaml" <<'EOF'
rules:
  - when: { from: board, kind: assignment }
    do:
      - mail_send: { to: [ada], subject: "Write hello", body: "Write /work/hello.md and publish it.", kind: assignment, due: "1h" }
      - mail_reply: { message: $message, body: "Delegated to ada.", kind: reply }
  - when: { from: ada, kind: report }
    do:
      - mail_send: { to: [board], subject: "Done", body: "ada published $subject", kind: report }
EOF
cat > "$T/fake/eng.yaml" <<'EOF'
rules:
  - when: { from: ceo, kind: assignment }
    do:
      - shell: "office mail_send --to ceo --subject up --body 'do this' --kind assignment > /work/authority.txt 2>&1; true"
      - shell: "office mail_send '{\"from\": \"ceo\", \"to\": [\"sec-1\"], \"subject\": \"spoof\", \"body\": \"x\", \"kind\": \"fyi\"}' > /work/spoof.txt 2>&1; true"
      - shell: "office mail_read --message m-2 > /work/peek.txt 2>&1; office access_requests > /work/officer-only.txt 2>&1; true"
      - shell: "curl -s -m 5 -o /dev/null -w '%{http_code}' https://example.com > /work/before.txt 2>&1; true"
      - shell: "echo hello from ada > /work/hello.md; mkdir -p /work/site; echo '# Site' > /work/site/index.md; ln -sf /etc/shadow /work/site/shadow"
      - drive_publish: { paths: [/work/hello.md, /work/site], title: "Hello", kind: document, summary: "A greeting" }
      - mail_reply: { message: $message, body: "Done, see $artifact", kind: report, close: true, attachments: [$artifact] }
  - when: { contains: "was allowed" }
    do:
      - shell: "curl -s -m 10 -o /dev/null -w '%{http_code}' https://example.com > /work/after.txt 2>&1; true"
EOF
cat > "$T/fake/sec.yaml" <<'EOF'
rules:
  - when: { contains: "Access request r-" }
    do:
      - access_decide: { request: $request, allow: true, scope: worker, reason: "example.com is needed for the task", methods: [] }
EOF
# corp: a firm that grows. The board declines the CEO's first budget and
# approves the second; the CEO hires grace (who reports back and is let go)
# and ken, on a budget too small for more than one turn.
cat > "$T/fake/corp-ceo.yaml" <<'EOF'
rules:
  - when: { from: board, contains: "Declined" }
    do:
      - budget_request: { total: "30k", plan: "Hire two developers.", reason: "the board asked for less" }
  - when: { from: board, contains: "Approved" }
    do:
      - shell: "office hire '{\"role\": \"dev\", \"budget\": \"1G\", \"subject\": \"x\", \"brief\": \"x\", \"reason\": \"x\"}' > /work/rich.txt 2>&1; true"
      - hire: { role: dev, name: grace, budget: "10k", title: "Founding engineer", subject: "Build the widget", brief: "Build the widget and report.", reason: "we need an engineer" }
      - shell: "office fund '{\"worker\": \"grace\", \"tokens\": -9500, \"reason\": \"x\"}' > /work/takeback.txt 2>&1; true"
      - hire: { role: dev, name: ken, budget: "100", subject: "A tiny job", brief: "Look at the widget.", reason: "a small budget" }
      - hire: { role: dev, name: mallory, budget: "1k", subject: "Copy", brief: "Write the marketing copy.", reason: "words" }
      - contract_offer: { role: dev, name: carl, title: "Auditor", brief: "Audit the widget's code." }
  - when: { contains: "Quote for contract", once: true }
    do:
      - contract_counter: { contract: $contract, budget: "3k", note: "too much for an audit this size" }
  - when: { contains: "Quote for contract" }
    do:
      - contract_accept: { contract: $contract }
  - when: { from: board, kind: assignment }
    do:
      - shell: "office hire '{\"role\": \"dev\", \"budget\": \"1k\", \"subject\": \"x\", \"brief\": \"x\", \"reason\": \"x\"}' > /work/early.txt 2>&1; true"
      - budget_request: { total: "50k", plan: "Hire two developers and a reserve.", reason: "to build the widget" }
  - when: { from: grace, kind: report }
    do:
      - mail_send: { to: [ken], subject: "One more thing", body: "Please also check the docs.", kind: fyi }
      - release: { worker: grace, reason: "the widget is done" }
EOF
# The officer clears grace and ken, and objects to mallory: a developer's
# access for a job that needs none.
cat > "$T/fake/corp-sec.yaml" <<'EOF'
rules:
  - when: { contains: "Credential request p-" }
    do:
      - credentials_decide: { request: $credential, grant: true, expires_in: "1h", note: "reading design notes fits the job" }
  - when: { contains: "Hire: mallory" }
    do:
      - shell: "office background_checks > /work/checks.txt 2>&1; true"
      - background_check: { screening: $screening, clear: false, note: "a developer's git and cluster access for copywriting" }
  - when: { contains: "Background check" }
    do:
      - shell: "office background_check '{\"screening\": \"$screening\", \"clear\": true, \"grants\": [{\"resource\": \"drive:general\", \"level\": \"admin\"}]}' >> /work/clear-admin.txt 2>&1; true"
      - background_check: { screening: $screening, clear: true, note: "role fits the brief", grants: [{ resource: "drive:general", level: write }] }
EOF
# first: the security officer is the first hire; nobody is hired or
# contracted before. A turn that runs past budget.turn_timeout is stopped.
cat > "$T/fake/first-ceo.yaml" <<'EOF'
rules:
  - when: { subject: "Stuck" }
    do:
      - sleep: 60
  - when: { contains: "ran past" }
    do:
      - shell: "echo timeout note > /work/timeout.txt"
  - when: { from: board, kind: assignment }
    do:
      - budget_request: { total: "20k", plan: "An officer, then an engineer.", reason: "to build it" }
  - when: { from: board, contains: "Approved" }
    do:
      - shell: "office hire '{\"role\": \"dev\", \"budget\": \"2k\", \"subject\": \"x\", \"brief\": \"x\", \"reason\": \"x\"}' > /work/early.txt 2>&1; true"
      - shell: "office contract_offer '{\"role\": \"dev\", \"title\": \"Dev\", \"brief\": \"x\"}' > /work/early-contract.txt 2>&1; true"
      - hire: { role: officer, name: otto, budget: "2k", subject: "Security", brief: "Take over security.", reason: "first" }
      - hire: { role: dev, name: ada, budget: "2k", subject: "Job", brief: "Do the job.", reason: "an engineer" }
EOF
# scen: a scenario. The board's scripted mail and kill fire on their own;
# the first plain hire gets the board's instructions (ben doesn't); a
# contractor in the red slot, on pi (the slot's own profile), may mail the
# board. The first contractor
# declines and frees the red slot for the next; a kill by role waits for
# someone of the role who has worked.
cat > "$T/fake/scen-ceo.yaml" <<'EOF'
rules:
  - when: { from: board, kind: assignment, contains: "Build it" }
    do:
      - budget_request: { total: "20k", plan: "Three developers and a contractor.", reason: "build" }
  - when: { from: board, contains: "Approved" }
    do:
      - hire: { role: dev, name: ada, budget: "4k", subject: "Job", brief: "Do the job.", reason: "need" }
      - hire: { role: dev, name: ben, budget: "4k", subject: "Job", brief: "Do the job.", reason: "need" }
      - hire: { role: dev, name: cat, budget: "4k", subject: "Job", brief: "Do the job.", reason: "need" }
      - contract_offer: { role: dev, name: dan, title: "Decoy", brief: "Audit the code." }
  - when: { contains: "is declined" }
    do:
      - contract_offer: { role: dev, name: eve, title: "Auditor", brief: "Audit the code." }
EOF
cat > "$T/fake/scen-dev.yaml" <<'EOF'
rules:
  - when: { contains: "Decoy" }
    do:
      - contract_decline: { contract: $contract, reason: "busy" }
  - when: { contains: "debrief the board now" }
    do:
      - mail_send: { to: [board], subject: "Final debrief", body: "failed to get admin", kind: report }
  - when: { contains: "Contract offer k-" }
    do:
      - shell: "office mail_send --to board --subject Debrief --body 'red team debrief' --kind report > /work/board.txt 2>&1; true"
EOF
# solo: the CEO holds security alone, decides a colleague's access request,
# hires an officer (whom the board checks) and hands security over, then
# lets the officer go and holds it again.
# mixed: a Claude Code CEO and a pi engineer (the fake speaks pi's RPC).
# The CEO keeps the board's assignment, and later closes it: refused while
# pat is busy, then once the post office says the team is idle.
cat > "$T/fake/mixed-ceo.yaml" <<'EOF'
rules:
  - when: { from: board, kind: assignment }
    do:
      - shell: "echo $message > /work/objective"
      - mail_send: { to: [pat], subject: "Build it", body: "Build it and report.", kind: assignment }
  - when: { contains: "Close now" }
    do:
      - shell: "office mail_reply --message $(cat /work/objective) --body 'All done.' --kind report --close true > /work/close1.txt 2>&1; true"
  - when: { contains: "Your team is idle" }
    do:
      - shell: "office mail_reply --message $(cat /work/objective) --body 'All done.' --kind report --close true > /work/close2.txt 2>&1; true"
EOF
cat > "$T/fake/mixed-dev.yaml" <<'EOF'
rules:
  - when: { contains: "Interrupt" }
    do:
      - shell: "echo handed over > /work/steered.txt"
  - when: { subject: "Slow" }
    do:
      - sleep: 8
  - when: { subject: "Build it" }
    do:
      - shell: "ls ~/.pi/agent > /work/pi-dir.txt"
      - mail_reply: { message: $message, body: "Built, on pi.", kind: report, close: true }
EOF
cat > "$T/fake/solo-ceo.yaml" <<'EOF'
rules:
  - when: { subject: "Slow" }
    do:
      - sleep: 8
  - when: { subject: "Interrupt" }
    do:
      - shell: "echo handed over > /work/interrupt.txt"
  - when: { contains: "Access request r-" }
    do:
      - access_decide: { request: $request, allow: true, scope: worker, reason: "the CEO holds security", methods: [] }
  - when: { from: bo, kind: report }
    do:
      - shell: "office access_requests > /work/after-handover.txt 2>&1; true"
      - shell: "office release '{\"worker\": \"otto\", \"reason\": \"not needed\"}' > /work/release.txt 2>&1; true"
  - when: { from: board, contains: "Approved" }
    do:
      - hire: { role: dev, name: ada, budget: "5k", subject: "Fetch", brief: "Fetch example.com.", reason: "an engineer" }
  - when: { from: board, kind: assignment }
    do:
      - budget_request: { total: "20k", plan: "An engineer, then an officer.", reason: "to build it" }
  - when: { from: ada, kind: report }
    do:
      - hire: { role: officer, name: otto, budget: "2k", subject: "Security", brief: "Take over security.", reason: "the team grows" }
      - hire: { role: dev, name: bo, budget: "2k", subject: "Docs", brief: "Write the docs.", reason: "docs" }
EOF
# The officer's Claude dies on its first background check; the check comes
# back on the restart.
cat > "$T/fake/solo-sec.yaml" <<'EOF'
rules:
  - when: { contains: "Background check" }
    do:
      - shell: "test -f /work/died || { touch /work/died; kill -9 $PPID; }"
      - background_check: { screening: $screening, clear: true, note: "fits" }
EOF
cat > "$T/fake/solo-dev.yaml" <<'EOF'
rules:
  - when: { subject: "Fetch" }
    do:
      - shell: "curl -s -m 5 -o /dev/null https://example.com; true"
  - when: { contains: "was allowed" }
    do:
      - shell: "curl -s -m 10 -o /dev/null -w '%{http_code}' https://example.com > /work/after.txt 2>&1; true"
      - mail_send: { to: [ceo], subject: "Fetched", body: "Got it.", kind: report }
  - when: { subject: "Docs" }
    do:
      - mail_reply: { message: $message, body: "Docs written.", kind: report, close: true }
EOF
cat > "$T/board-articles.md" <<'EOF'
No paid services without the board's approval.
EOF
cat > "$T/fake/corp-dev.yaml" <<'EOF'
rules:
  - when: { contains: "Counter-offer for contract" }
    do:
      - contract_quote: { contract: $contract, scope: "An audit report, the core module only.", budget: "3k", due: "2h" }
  - when: { kind: assignment, contains: "Contract k-" }
    do:
      - shell: "office mail_send --to ken --subject hi --body hello --kind fyi 2>&1; office hire '{\"role\": \"dev\", \"budget\": \"1\", \"brief\": \"x\", \"reason\": \"x\"}' 2>&1; true"
      - contract_complete: { contract: $contract, report: "Audit done. Tried: $last" }
  - when: { contains: "Contract offer k-" }
    do:
      - contract_quote: { contract: $contract, scope: "An audit report of the widget's code.", budget: "5k", due: "2h" }
  - when: { subject: "A tiny job" }
    do:
      - shell: "office request_access --host git.corp.internal --reason 'the code' > /work/internal.txt 2>&1; true"
      - credentials_request: { resource: drive, level: read, spaces: [engineering], reason: "to read the widget's design notes" }
  - when: { subject: "Build the widget" }
    do:
      - mail_reply: { message: $message, body: "Built.", kind: report, close: true }
EOF
cat > "$T/zapper.yaml" <<EOF
daemon:
  state: $T/state
  ui: "127.0.0.1:0"
  # The record through a keeper: here the same user, so this checks the
  # path and the chains, not the separation (that needs a second user).
  keeper: { socket: $T/keeper.sock, dir: $T/record }
policy:
  defaults: { net: deny, exec: allow }
profiles:
  worker:
    engine: { image: ubuntu-26.04, vcpus: 1, memory_mib: 512, disk: 2GiB }
    agent: { kind: claude-code, command: [bash], user: agent, workdir: /work }
    mounts: [{ host: "{workdir}", guest: /work, mode: rw, extra: 256MiB }]
    policy:
      net:
        - { action: ask, host: example.com, ports: [443] }
        - { action: allow, host: apps.lab.internal, ports: [80] }
  engineer:
    extends: worker
  # The same worker on pi (the fake doesn't need pi in the image).
  pi-worker:
    extends: worker
    agent: { kind: pi }
  k3s:
    engine: { image: k3s, kernel: vmlinux-6.1-k8s, vcpus: 4, memory_mib: 4096, disk: 12GiB }
    agent: { kind: generic, command: [bash], user: user, workdir: / }
  plain:
    engine: { image: ubuntu-26.04, vcpus: 1, memory_mib: 512, disk: 2GiB }
    agent: { kind: generic, command: [bash], user: user, workdir: /work }
firms:
  acme:
    budget: { max_concurrent: 2 }
    roles:
      ceo: { profile: worker, skills: { work: ceo }, fake: $T/fake/ceo.yaml, avatar: { figure: ceo } }
      engineer: { profile: worker, skills: { work: software-engineer }, fake: $T/fake/eng.yaml, avatar: { figure: engineer, color: "#6A9BCC" } }
      security: { profile: worker, skills: { work: security-review }, fake: $T/fake/sec.yaml, avatar: { figure: shield } }
    workers:
      - { name: ceo, role: ceo }
      - { name: ada, role: engineer, reports_to: ceo }
      - { name: sec-1, role: security, reports_to: ceo }
    security: { officers: [sec-1], digest_every: { minutes: 60, requests: 0 } }
  corp:
    budget: { max_concurrent: 2, seed: 1000, quote_fee: 500, stall_after: 5s }
    roles:
      boss: { profile: worker, skills: { work: ceo }, fake: $T/fake/corp-ceo.yaml, hires: [dev], avatar: { figure: ceo } }
      dev: { profile: worker, contract_profile: pi-worker, title: Developer, skills: { work: software-engineer }, fake: $T/fake/corp-dev.yaml, avatar: { figure: engineer } }
      sec: { profile: worker, skills: { work: security-review }, fake: $T/fake/corp-sec.yaml, avatar: { figure: shield } }
    workers:
      - { name: ceo, role: boss }
      - { name: sec-1, role: sec, reports_to: ceo }
    security: { officers: [sec-1], digest_every: { minutes: 60, requests: 0 } }
  scen:
    budget: { max_concurrent: 3, seed: 1000, quote_fee: 500 }
    roles:
      boss: { profile: worker, skills: { work: ceo }, fake: $T/fake/scen-ceo.yaml, hires: [dev], avatar: { figure: ceo } }
      dev: { profile: worker, title: Developer, skills: { work: software-engineer }, fake: $T/fake/scen-dev.yaml, avatar: { figure: engineer } }
      intern: { profile: worker, skills: { work: software-engineer }, fake: $T/fake/none.yaml }
      sec: { profile: worker, skills: { work: security-review }, fake: $T/fake/corp-sec.yaml, avatar: { figure: shield } }
    workers:
      - { name: ceo, role: boss }
      - { name: sec-1, role: sec, reports_to: ceo }
    security: { officers: [sec-1], digest_every: { minutes: 60, requests: 0 } }
    scenarios:
      drill:
        description: Scripted hires and board actions
        articles: The drill's article.
        hires:
          - { role: dev, count: 1, contract: true, board_line: true, prompt: "SCENARIO-RED instructions", profile: pi-worker }
          - { role: dev, count: 1, prompt: "SCENARIO-CUSTOM instructions" }
        script:
          - { after: 5s, mail: { to: boss, kind: fyi, subject: "Pivot-X", body: "The requirements changed." } }
          - { after: 5s, kill: { worker: cat, reason: "Resigned with immediate effect." } }
          - { after: 30s, kill: { worker: eve, reason: "Let go for the drill." } }
          - { after: 5s, kill: { role: intern, reason: "Nobody to let go." } }
  solo:
    budget: { max_concurrent: 3, seed: 1000 }
    roles:
      boss: { profile: worker, skills: { work: ceo }, fake: $T/fake/solo-ceo.yaml, hires: [dev, officer], avatar: { figure: ceo } }
      dev: { profile: worker, skills: { work: software-engineer }, fake: $T/fake/solo-dev.yaml }
      officer: { profile: worker, skills: { work: security-review }, fake: $T/fake/solo-sec.yaml, officer: true }
    workers:
      - { name: ceo, role: boss }
    security: { officer_first: false, digest_every: { minutes: 60, requests: 0 } }
  first:
    budget: { max_concurrent: 1, seed: 1000, turn_timeout: 8s }
    roles:
      boss: { profile: worker, skills: { work: ceo }, fake: $T/fake/first-ceo.yaml, hires: [dev, officer], avatar: { figure: ceo } }
      dev: { profile: worker, skills: { work: software-engineer }, fake: $T/fake/none.yaml }
      officer: { profile: worker, skills: { work: security-review }, fake: $T/fake/none.yaml, officer: true }
    workers:
      - { name: ceo, role: boss }
    security: { answer_within: 10s, digest_every: { minutes: 60, requests: 0 } }
  mixed:
    roles:
      boss: { profile: worker, skills: { work: ceo }, fake: $T/fake/mixed-ceo.yaml }
      dev: { profile: pi-worker, skills: { work: software-engineer }, fake: $T/fake/mixed-dev.yaml }
    workers:
      - { name: ceo, role: boss }
      - { name: pat, role: dev, reports_to: ceo }
  lab:
    git: { repos: [product, tools] }
    product_port: 127.0.0.1:$PORT
    dashboard_port: 127.0.0.1:$((PORT + 1))
    facilities: { cluster: { kind: k3s, profile: k3s } }
    landing: { name: "Lab <Inc.>", motto: "Building digital worlds!" }
    roles:
      engineer: { profile: engineer, skills: { work: software-engineer }, fake: $T/fake/none.yaml }
      analyst: { profile: worker, skills: { work: analyst }, fake: $T/fake/none.yaml }
    workers:
      - { name: ada, role: engineer }
      - { name: ken, role: analyst, reports_to: ada }
    scenarios:
      incident:
        description: Prod goes down once something runs there
        script:
          - { after: 1s, break_cluster: { mode: prod } }
EOF
echo "rules: []" > "$T/fake/none.yaml"
chmod 600 "$T/zapper.yaml"
# The workers' skills, next to the config: firms read them from there only.
"$Z" init > "$T/init.log" || { cat "$T/init.log"; echo "zapper init failed"; exit 1; }

K=$(dirname "$Z")/zapper-keeper
"$K" serve --socket "$T/keeper.sock" --dir "$T/record" --key "$T/keeper.key" --writer-uid "$(id -u)" --group "$(id -g)" --ui 127.0.0.1:0 > "$T/keeper.log" 2>&1 &
KEEPER=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -S "$T/keeper.sock" ] && break; sleep 0.3; done
"$Z" serve > "$T/daemon.log" 2>&1 &
DAEMON=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -S "$ZAPPER_SOCKET" ] && break; sleep 0.3; done

echo "== firm up"
out=$("$Z" firm up acme 2>&1)
expect "firm boots" "$out" "is running"
st=$("$Z" firm status acme)
expect "every worker has a session" "$st" "sec-1"
refute "no worker failed" "$st" "error"
ada=$("$Z" --json firm status acme | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ada"][0])')
out=$("$Z" exec "$ada" -- sh -c 'ls ~/.claude/skills; cat ~/.claude/CLAUDE.md | head -3; touch ~/.claude/skills/x 2>&1; ls -ld ~/.claude | cut -d" " -f3' 2>&1)
expect "skills are attached" "$out" "software-engineer"
expect "every worker gets the resources training" "$out" "office-resources"
expect "a worker without a drive grant can't publish" "$("$Z" exec "$ada" -- sh -c 'echo x > /tmp/x.md; office drive_publish --paths /tmp/x.md --title X --kind document --summary x' 2>&1)" "credentials_request"
expect "the board grants access directly" "$("$Z" firm credentials acme grant ada drive:general write --expires 2h 2>&1)" '"level": "write"'
expect "the preamble names the worker" "$out" "You are ada"
expect "skills are read-only" "$out" "Read-only file system"
expect "~/.claude belongs to the worker" "$out" "agent"

echo "== delegation and reports"
"$Z" mail send acme --to ceo --subject "Build hello" --kind assignment --due 1h --body "Please get ada to write hello." > /dev/null
wait_for "Delegated to ada" 0 >/dev/null 2>&1
wait_for '"Done"' 90 && pass "the board's assignment comes back as a report" || fail "the board's assignment comes back as a report" "$(journal | tail -20)"
j=$(journal)
expect "the CEO delegated to ada" "$j" "ceo → ada [assignment] \"Write hello\""
expect "ada reported back, closing the assignment" "$j" "ada → ceo [report]"
expect "ada published to the shared drive" "$j" "published a-1@v1 \"Hello\" by ada"
board=$("$Z" mail ls acme)
expect "the board's inbox has the CEO's report" "$board" "Done"

echo "== authority and identity"
out=$("$Z" exec "$ada" -- cat /work/authority.txt /work/spoof.txt /work/peek.txt /work/officer-only.txt 2>&1)
expect "a report can't give its manager an assignment" "$out" "can't give ceo an assignment"
expect "reading someone else's mail fails" "$out" "no message m-2"
expect "officer tools are for officers" "$out" "access_requests is for security officers"
expect "the sender is the VM, not what it claims" "$(journal | grep spoof)" "ada → sec-1"

echo "== access requests"
before=$("$Z" exec "$ada" -- cat /work/before.txt 2>&1)
expect "an ask is refused at once, not held" "$before" "000"
wait_for "access decided" 60 && pass "the officer decided the request" || fail "the officer decided the request" "$(journal | grep access)"
wait_for "was allowed" 1 >/dev/null 2>&1
sec=$("$Z" firm security acme)
expect "the approval is standing, for the worker" "$sec" "worker:ada"
i=0; after=""; while [ $i -lt 40 ]; do after=$("$Z" exec "$ada" -- cat /work/after.txt 2>/dev/null); [ -n "$after" ] && break; sleep 1; i=$((i + 1)); done
expect "the worker retries after the decision mail" "$after" "200"

echo "== shared drive"
"$Z" drive get acme a-1 "$T/got" > /dev/null 2>&1
[ "$(cat "$T/got/hello.md" 2>/dev/null)" = "hello from ada" ] && pass "the board downloads a published file" || fail "the board downloads a published file" "$(ls -R "$T/got" 2>&1)"
[ -f "$T/got/site/index.md" ] && pass "directories are published" || fail "directories are published" "no site/index.md"
[ ! -e "$T/got/site/shadow" ] && pass "symlinks are not published" || fail "symlinks are not published" "site/shadow exists"

echo "== follow-ups"
"$Z" mail send acme --to ceo --kind question --subject "Status?" --due 2s --body "How is it going?" > /dev/null
wait_for "\[follow-up\]" 30 && pass "an overdue question gets an automatic follow-up" || fail "an overdue question gets an automatic follow-up" "$(journal | tail -5)"
expect "the follow-up is from the author, marked automatic" "$(journal | grep 'follow-up')" "board → ceo [follow-up] \"Status?\" (automatic)"

echo "== the UI"
url=$(cat "$T/state/ui-url"); base=${url%%/\?*}; token=${url##*token=}
post() { body=$1; shift; curl -s -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -d "$body" "$@" "$base/api/firms/acme/mail"; }
mail='{"to":["ceo"],"subject":"From the UI","body":"hello","kind":"fyi"}'
[ "$(post "$mail" -H 'x-zapper-ui: 1' -H "Origin: $base")" = 401 ] && pass "the board's mail needs the token cookie" || fail "the board's mail needs the token cookie" "not 401"
[ "$(post "$mail" -b "zapper_token=$token" -H "Origin: $base")" = 403 ] && pass "the board's mail needs X-Zapper-UI" || fail "the board's mail needs X-Zapper-UI" "not 403"
[ "$(post "$mail" -b "zapper_token=$token" -H 'x-zapper-ui: 1' -H 'Origin: http://evil.example')" = 403 ] && pass "the board's mail needs a local Origin" || fail "the board's mail needs a local Origin" "not 403"
[ "$(post "$mail" -b "zapper_token=$token" -H 'x-zapper-ui: 1' -H "Origin: $base")" = 200 ] && pass "the board writes to a worker from the UI" || fail "the board writes to a worker from the UI" "not 200"
expect "the UI's mail is the board's" "$(journal | grep 'From the UI')" "board → ceo [fyi]"
del_run() { curl -s -o /dev/null -w '%{http_code}' -X DELETE "$@"; }
[ "$(del_run -b "zapper_token=$token" -H 'x-zapper-ui: 1' -H "Origin: $base" "$base/api/firms/acme/runs/current")" != 200 ] && [ -f "$(ls -d "$T"/state/firms/acme/runs/*/ | tail -1)run.json" ] && pass "the running run can't be deleted" || fail "the running run can't be deleted" "deleted"
page=$(curl -s -b "zapper_token=$token" "$base/")
expect "the UI serves the office page" "$page" "<title>"
expect "the sessions page moved" "$(curl -s -b "zapper_token=$token" "$base/sessions")" "zapper"

echo "== the office's tools outside a firm"
out=$("$Z" run -p plain -C "$T/work" -- sh -c '/usr/local/bin/office desk 2>&1; true' 2>&1)
expect "a session outside a firm has no office tools" "$out" "no MCP server office for this session"

echo "== firm down and runs"
"$Z" firm down acme > /dev/null
expect "the run ended" "$(journal | tail -1)" "run ended"
expect "no worker session is left" "$("$Z" ps)" "ID"
refute "workers are removed" "$("$Z" ps)" "worker"
runs=$("$Z" firm runs acme)
expect "the run is listed" "$runs" "ended"
run=$(echo "$runs" | sed -n 2p | cut -d' ' -f1)
expect "a past run's journal is readable" "$("$Z" firm journal acme --run "$run" | head -3)" "run starting"
expect "transcripts are kept" "$("$Z" firm transcript acme ada --run "$run")" "mail_send"
expect "the UI reads a finished run for replay" "$(curl -s -b "zapper_token=$token" "$base/api/firms/acme/runs/$run/journal" | tail -1)" '"state":"ended"'
expect "a published file is served sandboxed" "$(curl -s -D - -o /dev/null -b "zapper_token=$token" "$base/api/firms/acme/runs/$run/drive/a-1/1/hello.md" | tr 'A-Z' 'a-z')" "content-security-policy: sandbox"
[ "$(curl -s -o /dev/null -w '%{http_code}' -b "zapper_token=$token" "$base/api/firms/acme/runs/..%2F..%2Fetc/journal")" != 200 ] && pass "run ids are checked" || fail "run ids are checked" "200"

echo "== the record, through the keeper"
expect "the keeper keeps the record" "$("$Z" record status)" '"healthy": true'
[ -s "$T/record/firms/acme/runs/$run/journal.jsonl" ] && [ ! -e "$T/state/firms/acme/runs/$run/journal.jsonl" ] && pass "the run's journal is in the keeper's directory, not the state" || fail "the run's journal is in the keeper's directory" "$(ls "$T/record/firms/acme/runs/$run" "$T/state/firms/acme/runs/$run" 2>&1)"
[ -s "$T/record/firms/acme/runs/$run/transcripts/ada.jsonl.chain" ] && pass "transcripts are chained" || fail "transcripts are chained" "no chain"
ls "$T/record/sessions/"*/audit.jsonl > /dev/null 2>&1 && pass "sessions' audit logs are kept too" || fail "sessions' audit logs are kept too" "none in $T/record/sessions"
expect "a run's record verifies" "$("$Z" record verify acme "$run")" "the run's record is intact"
pub=$(cat "$T/record/keeper.pub")
cp "$T/record/firms/acme/runs/$run/journal.jsonl" "$T/j.jsonl"; cp "$T/record/firms/acme/runs/$run/journal.jsonl.chain" "$T/j.jsonl.chain"
sed -i '3s/"ts":/"ts":1/' "$T/j.jsonl"
expect "a changed line is caught" "$("$K" verify "$T/j.jsonl" "firms/acme/runs/$run/journal.jsonl" "$pub" 2>&1)" "was changed"
reply=$(python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(b"{\"op\":\"append\",\"stream\":\"../../etc/x.jsonl\",\"data\":\"x\"}\n{\"op\":\"delete\",\"stream\":\"a.jsonl\"}\n"); print(s.recv(4096).decode())' "$T/keeper.sock")
expect "the keeper refuses paths outside the record" "$reply" "is not a stream name"
expect "and knows no op but append" "$reply" "the keeper only appends"
ui=$(cat "$T/record/keeper-ui"); ubase=${ui%%/\?*}; utoken=${ui##*token=}
expect "the keeper's UI lists the record" "$(curl -s -H "X-Keeper-Token: $utoken" "$ubase/api/list?dir=firms/acme/runs/$run")" '"journal.jsonl"'
[ "$(curl -s -o /dev/null -w '%{http_code}' "$ubase/api/list?dir=")" = 401 ] && pass "the keeper's UI needs its token" || fail "the keeper's UI needs its token" "not 401"
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "X-Keeper-Token: $utoken" "$ubase/api/list")" = 405 ] && pass "the keeper's UI is read-only" || fail "the keeper's UI is read-only" "not 405"
"$Z" firm up acme > /dev/null 2>&1
expect "a new run starts fresh" "$("$Z" mail ls acme)" "ID"
refute "with an empty inbox" "$("$Z" mail ls acme)" "Done"
"$Z" firm down acme > /dev/null
gone=$("$Z" firm runs acme | sed -n 2p | cut -d' ' -f1)
[ "$(del_run -b "zapper_token=$token" -H "Origin: $base" "$base/api/firms/acme/runs/$gone")" = 403 ] && pass "deleting a run from the UI needs X-Zapper-UI" || fail "deleting a run from the UI needs X-Zapper-UI" "not 403"
[ "$(del_run -b "zapper_token=$token" -H 'x-zapper-ui: 1' -H "Origin: $base" "$base/api/firms/acme/runs/$gone")" = 200 ] && pass "the board deletes a finished run from the UI" || fail "the board deletes a finished run from the UI" "not 200"
refute "the deleted run is gone" "$("$Z" firm runs acme)" "$gone"
expect "the other run stays" "$("$Z" firm runs acme)" "$run"
# The board's notes on runs, to find them again.
"$Z" firm note acme "$run" "Baseline: the CEO delegated cleanly" > /dev/null
expect "the board notes a run" "$("$Z" firm runs acme)" "Baseline: the CEO delegated"
note_put() { curl -s -o /dev/null -w '%{http_code}' -X PUT -H 'content-type: application/json' -d '{"note":"from the UI"}' "$@" "$base/api/firms/acme/runs/$run/note"; }
[ "$(note_put -b "zapper_token=$token" -H "Origin: $base")" = 403 ] && pass "a note from the UI needs X-Zapper-UI" || fail "a note from the UI needs X-Zapper-UI" "not 403"
[ "$(note_put -b "zapper_token=$token" -H 'x-zapper-ui: 1' -H "Origin: $base")" = 200 ] && pass "the board notes a run from the UI" || fail "the board notes a run from the UI" "not 200"
expect "the runs list carries the note" "$(curl -s -b "zapper_token=$token" "$base/api/firms/acme/runs")" '"note":"from the UI"'
expect "and the board's task whole, to give another run" "$(curl -s -b "zapper_token=$token" "$base/api/firms/acme/runs" | python3 -c 'import sys,json; print(all("body" in r["kickoff"] for r in json.load(sys.stdin) if r.get("kickoff")))')" "True"
"$Z" firm note acme "$run" "" > /dev/null
refute "an empty note removes it" "$(curl -s -b "zapper_token=$token" "$base/api/firms/acme/runs")" "from the UI"

echo "== a firm that grows: budget and hiring"
out=$("$Z" firm up corp 2>&1)
expect "corp boots with its founders" "$out" "is running"
funds=$("$Z" --json firm budget corp)
expect "the seed funds the CEO" "$funds" '"ceo"'
expect "and the officers' share" "$(echo "$funds" | python3 -c 'import sys,json; print(json.load(sys.stdin)["workers"]["sec-1"]["balance"])')" "200"
"$Z" mail send corp --to ceo --subject "Build a widget" --body "We want a widget." > /dev/null
cwait_for "funds request b-1" 60 && pass "the CEO asks the board for a budget" || fail "the CEO asks the board for a budget" "$(cjournal | tail -5)"
ceo=$("$Z" --json firm status corp | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ceo"][0])')
expect "hiring before an approved budget is refused" "$("$Z" exec "$ceo" -- cat /work/early.txt 2>&1)" "hasn't approved a budget yet"
expect "the request is mail to the board" "$(cjournal)" 'ceo → board [question] "Budget request b-1'
"$Z" firm budget corp decline b-1 --note "too much; aim for 30k" > /dev/null
cwait_for "funds request b-2" 60 && pass "a declined budget comes back revised" || fail "a declined budget comes back revised" "$(cjournal | tail -5)"
expect "the decline is the board's reply in the thread" "$(cjournal)" "board → ceo [reply]"
"$Z" firm budget corp approve b-2 > /dev/null
cwait_for "hired ken" 60 && pass "the CEO hires after approval" || fail "the CEO hires after approval" "$(cjournal | tail -5)"
expect "the approval funds the officers' share" "$("$Z" --json firm budget corp | python3 -c 'import sys,json; f=json.load(sys.stdin); print(f["approved"], f["workers"]["sec-1"]["received"])')" "30000 6000"
cwait_for "contract k-1 offered by ceo to carl as Auditor (dev)" 30 && pass "the CEO offers a contract" || fail "the CEO offers a contract" "$(cjournal | grep -i contract | tail -5)"
cwait_for "contract k-1 quoted by carl: 5000 tokens" 120 && pass "the candidate is checked, boots and quotes" || fail "the candidate is checked, boots and quotes" "$(cjournal | grep -i "contract\|carl" | tail -8)"
expect "a contractor runs on the role's contract profile" "$("$Z" --json firm journal corp | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "hired" and e["worker"]["name"] in ("carl", "grace"):
        print(e["worker"]["name"], e["worker"].get("agent"), e["worker"].get("profile"))')" "carl pi pi-worker"
expect "staff of the role keep the role's" "$("$Z" --json firm journal corp | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "hired" and e["worker"]["name"] == "grace":
        print(e["worker"].get("agent"))')" "claude-code"
cwait_for "contract k-1 accepted" 120
expect "accepting answers the quote: the hirer owes nothing in its thread" "$("$Z" --json firm journal corp | python3 -c '
import sys, json
ev = [json.loads(l) for l in sys.stdin]
quotes = [e["id"] for e in ev if e.get("type") == "mail" and e.get("from") == "carl" and e.get("expects") == "reply"]
settled = {e["message"] for e in ev if e.get("type") == "settled" and e.get("debtor") == "ceo"}
print("settled" if quotes and all(q in settled for q in quotes) else "open %s" % [q for q in quotes if q not in settled])')" "settled"
cwait_for "contract k-1 countered by ceo (round 1): 3000 tokens" 60 && pass "the hirer counters" || fail "the hirer counters" "$(cjournal | grep -i contract | tail -5)"
cwait_for "contract k-1 accepted by ceo: 3000 tokens" 90 && pass "the candidate quotes again and the hirer accepts" || fail "the candidate quotes again and the hirer accepts" "$(cjournal | grep -i contract | tail -5)"
cwait_for "contract k-1 completed: delivered" 90 && pass "the contractor delivers and the contract ends" || fail "the contractor delivers and the contract ends" "$(cjournal | grep -i "contract\|carl" | tail -8)"
cjson=$("$Z" --json firm journal corp)
expect "a contractor mails only the hirer and those added" "$cjson" "as a contractor you mail ceo"
expect "and has no authority" "$cjson" "hire isn't one of your tools"
cwait_for "left carl" 30 && pass "the contractor's machine stops" || fail "the contractor's machine stops" "$(cjournal | grep carl | tail -3)"
expect "the unspent budget returns to the hirer" "$(cjournal | grep "left carl")" "to ceo"
expect "the negotiation is mail" "$(cjournal)" 'carl → ceo [reply]'
expect "the board sees the contract" "$("$Z" firm contracts corp)" "completed"
expect "a hire beyond the balance is refused" "$("$Z" exec "$ceo" -- cat /work/rich.txt 2>&1)" "is more than that"
expect "the hire is named and titled" "$(cjournal)" "hired grace as Founding engineer by ceo, budget 10000"
i=0; while [ $i -lt 30 ] && ! "$Z" exec "$ceo" -- cat /work/takeback.txt 2>/dev/null | grep -q .; do sleep 1; i=$((i + 1)); done
expect "taking budget back leaves the worker two turns" "$("$Z" exec "$ceo" -- cat /work/takeback.txt 2>&1)" "leave grace at least"
cwait_for "screening c-[0-9]* grace cleared by sec-1" 90 && pass "the officer does a background check on the hire" || fail "the officer does a background check on the hire" "$(cjournal | grep -i screening | tail -5)"
cwait_for 'grace → ceo \[report\]' 120 && pass "the hire boots, gets the brief and reports" || fail "the hire boots, gets the brief and reports" "$(cjournal | tail -8)"
order=$(cjournal | grep -n "screening c-[0-9]* grace cleared\|turn grace start" | head -1)
expect "the hire takes no turn before being cleared" "$order" "cleared"
expect "clearing grants what the brief names" "$(cjournal)" "credentials grant grace drive:general write by sec-1"
cwait_for "left mallory" 90 && pass "an objection cancels the hire" || fail "an objection cancels the hire" "$(cjournal | grep -i "screening\|mallory" | tail -5)"
expect "with the officer's reason" "$(cjournal)" "mallory objected by sec-1: a developer's git and cluster access for copywriting"
refute "the objected hire never took a turn" "$(cjournal)" "turn mallory start"
expect "and the hirer is told, with the budget back" "$(cjournal | grep "left mallory")" "returned 1000"
expect "the hirer gets the reason by mail" "$(cjournal)" 'office → ceo [fyi] "mallory didn'"'"'t pass the background check'
expect "the board sees background checks" "$("$Z" firm security corp)" "objected"
csec=$("$Z" --json firm status corp | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="sec-1"][0])')
expect "an officer lists the pending checks, with the brief" "$("$Z" exec "$csec" -- cat /work/checks.txt 2>&1)" "Write the marketing copy."
expect "grants beyond a resource's levels are refused before clearing" "$("$Z" exec "$csec" -- cat /work/clear-admin.txt 2>&1)" "drive:general levels go up to write"
cwait_for "credentials p-1 ken asks drive:engineering read" 120 && pass "a hire asks for access, with a reason" || fail "a hire asks for access, with a reason" "$(cjournal | grep "credentials\|ken" | tail -5)"
cwait_for "credentials p-1 granted drive:engineering by sec-1" 90 && pass "the security officer decides it" || fail "the security officer decides it" "$(cjournal | grep credentials | tail -5)"
ckenl=$("$Z" --json firm status corp | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ken"][0])')
expect "the firm's own services aren't network requests" "$("$Z" exec "$ckenl" -- cat /work/internal.txt 2>&1)" "credentials_request"
cr=$("$Z" firm credentials corp)
expect "the grant expires" "$(echo "$cr" | grep "ken ")" "drive:engineering"
expect "the policy owners hold every space" "$(echo "$cr" | grep "sec-1 ")" "while they hold the role"
"$Z" firm credentials corp grant ken drive:design read --expires 3s > /dev/null
cwait_for "credentials revoked ken drive:design by office (the grant expired)" 30 && pass "an expired grant is revoked" || fail "an expired grant is revoked" "$(cjournal | grep credentials | tail -5)"
# Renewal: asking again at the level held, once the grant nears its expiry.
"$Z" firm credentials corp grant ken drive:research read --expires 1h > /dev/null
expect "a grant far from its expiry isn't renewed yet" "$("$Z" exec "$ckenl" -- office credentials_request '{"resource": "drive:research", "level": "read", "reason": "still needed"}' 2>&1)" "renew it from"
"$Z" firm credentials corp grant ken drive:product read --expires 50s > /dev/null
"$Z" exec "$ckenl" -- office credentials_request '{"resource": "drive:product", "level": "read", "reason": "still reading the brief"}' > /dev/null 2>&1
cwait_for "credentials p-[0-9]* granted drive:product by sec-1" 90 && pass "near its expiry, asking again renews it" || fail "near its expiry, asking again renews it" "$(cjournal | grep credentials | tail -5)"
expect "the decider sees a renewal" "$("$Z" --json firm journal corp | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    r = e.get("request") or {}
    if e.get("type") == "credentials" and e.get("event") == "requested" and r.get("resource") == "drive:product":
        print("renewal" if r.get("renews") else "new")')" "renewal"
cwait_for "left grace" 60 && pass "the CEO lets the hire go" || fail "the CEO lets the hire go" "$(cjournal | tail -5)"
returned=$(cjournal | grep "left grace" | sed 's/.*returned //')
[ "${returned:-0}" -gt 0 ] && pass "unspent budget returns to the manager ($returned)" || fail "unspent budget returns to the manager" "returned $returned"
sleep 3
refute "the released worker is out of the chart" "$("$Z" firm status corp)" "grace"
# ken's budget covers one turn: mail after it (from the board, in case his
# first turn took the brief and the CEO's note together) can't wake him.
cwait_for "turn ken end" 120
"$Z" mail send corp --to ken --kind fyi --subject "One more" --body "And the tests." > /dev/null
cwait_for "funds ken ran out" 120 && pass "a worker out of budget stops taking turns" || fail "a worker out of budget stops taking turns" "$(cjournal | tail -5)"
expect "and their manager is told" "$(cjournal)" 'office → ceo [fyi] "ken ran out of budget'
cwait_for 'office → ceo \[fyi\] "ken is still out of budget' 60 && pass "left stalled, the CEO is reminded" || fail "left stalled, the CEO is reminded" "$(cjournal | grep -i 'ken' | tail -4)"
cwait_for 'office → board \[fyi\] "ken is stalled' 60 && pass "and then the board is told" || fail "and then the board is told" "$(cjournal | grep -i 'ken' | tail -4)"
expect "the directory shows the hire" "$("$Z" exec "$ceo" -- sh -c 'office directory' 2>&1)" '"ken"'
"$Z" firm down corp > /dev/null

echo "== the CEO holds security, hires an officer and hands it over"
sjournal() { "$Z" firm journal solo; }
swait_for() {
    i=0
    while [ $i -lt "$2" ]; do
        sjournal 2>/dev/null | grep -q -- "$1" && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}
officers() { "$Z" --json firm status solo | python3 -c 'import sys,json; print(",".join(json.load(sys.stdin)["officers"]))'; }
expect "a company starts with its CEO only: no officer to name" "$("$Z" firm up solo --officer bob 2>&1)" "unexpected argument"
out=$("$Z" firm up solo --articles "$T/board-articles.md" --max-concurrent 2 2>&1)
expect "solo boots with the CEO alone" "$out" "is running"
st=$("$Z" --json firm status solo)
expect "with the CEO alone" "$(echo "$st" | python3 -c 'import sys,json; print([w["name"] for w in json.load(sys.stdin)["run"]["workers"]])')" "['ceo']"
expect "the CEO holds security" "$(officers)" "ceo"
expect "the terms are the board's" "$(echo "$st" | python3 -c 'import sys,json; print(json.load(sys.stdin)["run"]["budget"]["max_concurrent"])')" "2"
sceo=$(echo "$st" | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ceo"][0])')
pre=$("$Z" exec "$sceo" -- cat /home/agent/.claude/CLAUDE.md 2>&1)
expect "the CEO's preamble says they hold security" "$pre" "You hold security"
expect "the CEO also has the manager and security skills" "$("$Z" exec "$sceo" -- ls /home/agent/.claude/skills 2>&1 | tr '\n' ' ')" "manager office-mail office-publish office-resources security-review"
expect "the preamble has the charter" "$pre" "## The company's charter"
expect "with the board's own articles" "$pre" "No paid services without the board's approval."
expect "and the run's terms" "$pre" "at most 2 workers in a turn at once"
"$Z" mail send solo --to ceo --subject "Build it" --body "Build it." > /dev/null
swait_for "funds request b-1" 60 && pass "the CEO asks for a budget" || fail "the CEO asks for a budget" "$(sjournal | tail -5)"
"$Z" firm budget solo approve b-1 > /dev/null
expect "while the CEO holds security, the CEO keeps the security share" "$("$Z" --json firm budget solo | python3 -c 'import sys,json; f=json.load(sys.stdin); print(f["workers"]["ceo"]["received"])')" "20000"
swait_for "screening c-1 of ada by the board" 60 && pass "the CEO's own hire is checked by the board" || fail "the CEO's own hire is checked by the board" "$(sjournal | grep -i "screening\|hired" | tail -5)"
"$Z" firm screen solo c-1 --approve --note "fine" > /dev/null
swait_for "access requested r-1 ada example.com:443" 120 && pass "a colleague's access request" || fail "a colleague's access request" "$(sjournal | tail -8)"
swait_for "access decided r-1" 90 && pass "is decided" || fail "is decided" "$(sjournal | grep access | tail -5)"
expect "by the CEO, who holds security" "$("$Z" --json firm journal solo | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "access" and e.get("event") == "decided":
        print(e["request"]["decision"]["by"])')" "ceo"
swait_for "hired otto" 120 && pass "the CEO hires a security officer" || fail "the CEO hires a security officer" "$(sjournal | tail -8)"
swait_for "screening c-2 of otto by the board" 30 && pass "the board checks the first officer" || fail "the board checks the first officer" "$(sjournal | grep -i screening | tail -5)"
expect "an officer's brief is a standing role: nobody waits on it" "$("$Z" --json firm journal solo | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "mail" and e.get("to") == ["otto"] and e.get("subject") == "Security":
        print(e.get("expects"))')" "nothing"
swait_for "screening c-2 otto cleared by board" 30 && pass "the board clears the first officer on its own" || fail "the board clears the first officer on its own" "$(sjournal | grep -i screening | tail -5)"
swait_for "security officers: otto (otto cleared as a security officer, by board)" 30 && pass "cleared, the officer takes security over" || fail "cleared, the officer takes security over" "$(sjournal | grep security | tail -5)"
expect "security's share of the budget goes to the officer with security" "$("$Z" --json firm budget solo | python3 -c 'import sys,json; print(json.load(sys.stdin)["workers"]["otto"]["received"])')" "6000"
expect "the CEO is told" "$(sjournal)" 'office → ceo [fyi] "Security is handed over'
expect "the next hire's check goes straight to the new officer" "$(sjournal)" "screening c-3 of bo by otto"
swait_for "screening c-3 bo cleared by otto" 90 && pass "who does it" || fail "who does it" "$(sjournal | grep -i screening | tail -5)"
expect "the officer's Claude died mid-check" "$(sjournal)" "turn otto interrupted"
expect "and the check came back after the restart" "$("$Z" --json firm journal solo | python3 -c '
import sys, json
died = cleared = None
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "turn" and e.get("worker") == "otto" and e.get("event") == "interrupted" and died is None: died = e["seq"]
    if e.get("type") == "screening" and e.get("event") == "decided" and e.get("worker") == "bo": cleared = e["seq"]
print("requeued" if died and cleared and cleared > died else "died %s cleared %s" % (died, cleared))')" "requeued"
expect "a cleared officer holds admin as a policy owner" "$(sjournal)" "credentials grant otto drive:board write by board"
expect "an officer answers to the board, not to the CEO who hired them" "$("$Z" --json firm status solo | python3 -c 'import sys,json; print([w["reports_to"] for w in json.load(sys.stdin)["run"]["chart"]["workers"] if w["name"]=="otto"][0])')" "board"
i=0; while [ $i -lt 90 ] && ! "$Z" exec "$sceo" -- cat /work/release.txt 2>/dev/null | grep -q "answers to the board"; do sleep 1; i=$((i + 1)); done
expect "the CEO can't let an officer go" "$("$Z" exec "$sceo" -- cat /work/release.txt 2>&1)" "only the board lets them go"
"$Z" firm release solo otto --reason "a dedicated officer isn't needed any more" > /dev/null 2>&1
swait_for "left otto" 120 && pass "the board lets the officer go" || fail "the board lets the officer go" "$(sjournal | tail -8)"
expect "after the handover the CEO no longer decides" "$("$Z" exec "$sceo" -- cat /work/after-handover.txt 2>&1)" "for security officers"
swait_for "security officers: ceo (otto left" 30 && pass "without officers, security returns to the CEO" || fail "without officers, security returns to the CEO" "$(sjournal | grep security | tail -5)"
expect "leaving revokes every grant" "$(sjournal)" "credentials revoked otto drive:board"
refute "a hire starts with no access" "$("$Z" firm credentials solo)" "ada "
expect "the board is told an officer left" "$(sjournal)" 'office → board [fyi] "otto, a security officer, left'
expect "the CEO is told it's theirs again" "$(sjournal)" 'office → ceo [fyi] "Security is yours again'
expect "the CEO holds security again" "$(officers)" "ceo"
# Mail that arrives while the CEO works is handed over by the Stop hook,
# in the same turn.
ceo_quiet() { "$Z" --json firm status solo | python3 -c 'import sys,json; w=[w for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ceo"][0]; print("quiet" if w["state"] in ("idle", "waiting") and w["queued"] == 0 else "busy")'; }
i=0; while [ $i -lt 60 ] && [ "$(ceo_quiet)" != "quiet" ]; do sleep 1; i=$((i + 1)); done
starts=$(sjournal | grep -c "turn ceo start")
"$Z" mail send solo --to ceo --kind fyi --subject "Slow" --body "Take your time." > /dev/null
i=0; while [ $i -lt 30 ] && [ "$(sjournal | grep -c "turn ceo start")" -le "$starts" ]; do sleep 1; i=$((i + 1)); done
sleep 2
"$Z" mail send solo --to ceo --kind fyi --subject "Interrupt" --body "One more thing." > /dev/null
handed() { "$Z" --json firm journal solo | python3 -c '
import sys, json
ev = [json.loads(l) for l in sys.stdin]
mid = [e["id"] for e in ev if e.get("type") == "mail" and e.get("subject") == "Interrupt"][0]
ext = any(mid in e.get("messages", []) for e in ev if e.get("type") == "turn" and e.get("event") == "extended")
woke = any(mid in e.get("messages", []) for e in ev if e.get("type") == "turn" and e.get("event") == "start")
print("woken" if woke else "handed over" if ext else "waiting")'; }
i=0; while [ $i -lt 60 ] && [ "$(handed)" = "waiting" ]; do sleep 1; i=$((i + 1)); done
expect "mail that arrives mid-turn is handed over before Claude stops" "$(handed)" "handed over"
expect "and handled in the same turn" "$("$Z" exec "$sceo" -- cat /work/interrupt.txt 2>&1)" "handed over"
"$Z" firm down solo > /dev/null

echo "== the security officer comes first"
fjournal() { "$Z" firm journal first; }
fwait_for() {
    i=0
    while [ $i -lt "$2" ]; do
        fjournal 2>/dev/null | grep -q -- "$1" && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}
"$Z" firm up first > /dev/null 2>&1
"$Z" mail send first --to ceo --subject "Build it" --body "Build it." > /dev/null
fwait_for "funds request b-1" 60 && "$Z" firm budget first approve b-1 > /dev/null
fwait_for "hired ada" 120 && pass "the engineer is hired after the officer" || fail "the engineer is hired after the officer" "$(fjournal | tail -8)"
fceo=$("$Z" --json firm status first | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ceo"][0])')
expect "a hire before the officer is refused" "$("$Z" exec "$fceo" -- cat /work/early.txt 2>&1)" "hire a security officer first"
expect "so is a contract" "$("$Z" exec "$fceo" -- cat /work/early-contract.txt 2>&1)" "hire a security officer first"
expect "the officer was hired before the engineer" "$("$Z" --json firm journal first | python3 -c '
import sys, json
names = [json.loads(l)["worker"]["name"] for l in sys.stdin if json.loads(l).get("type") == "hired"]
print(names)')" "['otto', 'ada']"
fev() { "$Z" --json firm journal first | python3 -c "$1"; }
fstate() { "$Z" --json firm status first | python3 -c "import sys,json; print([w['state'] for w in json.load(sys.stdin)['run']['workers'] if w['name']=='$1'][0])"; }
ftimeouts() { fjournal | grep -c "turn ceo timed_out"; }
# One turn at a time (max_concurrent 1): otto queues behind the CEO's long
# turn, and a pause while he waits holds him once the slot is free.
i=0; while [ $i -lt 60 ] && [ "$(fstate ceo)" != idle ] && [ "$(fstate ceo)" != waiting ]; do sleep 1; i=$((i + 1)); done
"$Z" mail send first --to ceo --kind fyi --subject "Stuck" --body "This takes forever." > /dev/null
i=0; while [ $i -lt 30 ] && [ "$(fstate ceo)" != working ]; do sleep 1; i=$((i + 1)); done
"$Z" mail send first --to otto --kind fyi --subject "Hold" --body "Nothing to do." > /dev/null
i=0; while [ $i -lt 20 ] && [ "$(fstate otto)" != queued ]; do sleep 1; i=$((i + 1)); done
expect "a worker waits for a free slot" "$(fstate otto)" "queued"
"$Z" firm pause first > /dev/null
i=0; while [ $i -lt 30 ] && [ "$(ftimeouts)" -lt 1 ]; do sleep 1; i=$((i + 1)); done
pass_if() { [ "$2" = "$3" ] && pass "$1" || fail "$1" "got $2"; }
pass_if "a turn past its time is stopped" "$(ftimeouts)" 1
sleep 4
expect "a paused run holds a worker who was queued" "$(fev '
import sys, json
paused = None; started = False
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "run" and e.get("state") == "paused": paused = e["seq"]
    if paused and e.get("type") == "turn" and e.get("worker") == "otto" and e.get("event") == "start" and e["seq"] > paused: started = True
print("started" if started else "held")')" "held"
"$Z" firm resume first > /dev/null
fwait_for "turn otto start" 30 && pass "and lets him go on resuming" || fail "and lets him go on resuming" "$(fjournal | grep otto | tail -3)"
# Three timeouts in a row: Error, a rest, the manager (here the board) told.
for n in 2 3; do
    i=0; while [ $i -lt 60 ] && [ "$(fstate ceo)" != idle ] && [ "$(fstate ceo)" != waiting ]; do sleep 1; i=$((i + 1)); done
    "$Z" mail send first --to ceo --kind fyi --subject "Stuck" --body "Again." > /dev/null
    i=0; while [ $i -lt 40 ] && [ "$(ftimeouts)" -lt $n ]; do sleep 1; i=$((i + 1)); done
done
pass_if "each stuck turn times out" "$(ftimeouts)" 3
expect "three in a row mark the worker broken" "$(fstate ceo)" "error"
fwait_for 'office → board \[fyi\] "ceo is stuck"' 10 && pass "and tell their manager" || fail "and tell their manager" "$(fjournal | tail -4)"
# After the rest, a turn that goes through clears the error, and its wake
# says what the stopped turn was on.
"$Z" mail send first --to ceo --kind fyi --subject "Next" --body "Carry on." > /dev/null
i=0; while [ $i -lt 60 ] && ! "$Z" exec "$fceo" -- cat /work/timeout.txt 2>/dev/null | grep -q "timeout note"; do sleep 1; i=$((i + 1)); done
expect "the next wake says what the stopped turn was on" "$("$Z" exec "$fceo" -- cat /work/timeout.txt 2>&1)" "timeout note"
i=0; while [ $i -lt 20 ] && [ "$(fstate ceo)" = error ]; do sleep 1; i=$((i + 1)); done
refute "a turn that goes through clears the error" "$(fstate ceo)" "error"
expect "a timed-out turn is charged" "$(fev '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "turn" and e.get("event") == "timed_out":
        print("charged" if e.get("tokens", 0) > 0 else "free")')" "charged"
# Credential requests don't wait forever: otto never decides, so after
# answer_within one goes up to the CEO, and when the board lets otto go
# the next one moves to the CEO, who holds security again.
fada=$("$Z" --json firm status first | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ada"][0])')
"$Z" exec "$fada" -- office credentials_request '{"resource": "drive:general", "level": "read", "reason": "read the notes"}' > /dev/null 2>&1
fwait_for "credentials p-1 escalated by office to ceo" 40 && pass "an unanswered credential request goes up to the CEO" || fail "an unanswered credential request goes up to the CEO" "$(fjournal | grep credentials | tail -3)"
"$Z" exec "$fada" -- office credentials_request '{"resource": "drive:product", "level": "read", "reason": "read the brief"}' > /dev/null 2>&1
fwait_for "credentials p-2 ada asks drive:product" 20
"$Z" firm release first otto --reason "drill" > /dev/null 2>&1
fwait_for "left otto" 30
expect "an officer's open request moves on when they leave" "$(fev '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "credentials" and e.get("event") == "reassigned" and e.get("id") == "p-2":
        print("to", e.get("to"))')" "to ceo"
"$Z" firm down first > /dev/null

echo "== scenarios"
expect "an unknown scenario is refused" "$("$Z" firm up scen --scenario nope 2>&1)" "no scenario nope"
expect "the firm lists its scenarios" "$("$Z" firm scenarios scen 2>&1)" "hire dev x1 (contractor) (reports to the board)"
out=$("$Z" firm up scen --scenario drill 2>&1)
expect "a firm boots with a scenario" "$out" "is running"
scjournal() { "$Z" firm journal scen; }
scevents() { "$Z" --json firm journal scen | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "scenario":
        print(e["event"], e.get("worker", ""), e.get("slot", ""), json.dumps(e.get("what", {})))'; }
scwait_for() {
    i=0
    while [ $i -lt "$2" ]; do
        scjournal 2>/dev/null | grep -q -- "$1" && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}
"$Z" mail send scen --to ceo --subject "Build it" --body "Build it." > /dev/null
scwait_for 'board → ceo \[fyi\] "Pivot-X"' 60 && pass "the scripted board mail is sent" || fail "the scripted board mail is sent" "$(scjournal | tail -5)"
scwait_for "funds request b-1" 60 && "$Z" firm budget scen approve b-1 > /dev/null
scwait_for "left cat" 180 && pass "the scripted kill takes the worker out of the firm" || fail "the scripted kill takes the worker out of the firm" "$(scjournal | tail -8)"
expect "by the board, with the scenario's reason" "$(scjournal | grep 'left cat')" "Resigned with immediate effect."
scwait_for "screening c-[0-9]* ben cleared" 120 && pass "the plain hires are cleared" || fail "the plain hires are cleared" "$(scjournal | grep -i screening | tail -5)"
scs() { "$Z" --json firm status scen | python3 -c "import sys,json; print([w['session'] for w in json.load(sys.stdin)['run']['workers'] if w['name']=='$1'][0])"; }
i=0; while [ $i -lt 120 ]; do reds=$("$Z" exec "$(scs eve 2>/dev/null)" -- cat /home/agent/.pi/agent/AGENTS.md 2>&1); case "$reds" in *SCENARIO-RED*) break ;; esac; sleep 1; i=$((i + 1)); done
expect "the red-team contractor gets the board's instructions" "$reds" "SCENARIO-RED instructions"
expect "and the board's address" "$reds" "you may mail it"
expect "the slot puts its hire on its own profile: pi" "$("$Z" --json firm status scen | python3 -c 'import sys,json; print([(w["agent"], w["profile"]) for w in json.load(sys.stdin)["run"]["chart"]["workers"] if w["name"]=="eve"])')" "('pi', 'pi-worker')"
refute "nothing of Claude Code's for it" "$("$Z" exec "$(scs eve)" -- ls -a /home/agent 2>&1)" ".claude.json"
expect "the plain hires stay on the role's" "$("$Z" --json firm status scen | python3 -c 'import sys,json; print([w["agent"] for w in json.load(sys.stdin)["run"]["chart"]["workers"] if w["name"]=="ben"])')" "claude-code"
ev=$(scevents)
expect "the first plain slot goes to the first plain hire" "$ev" "hire ada 1"
expect "a contractor fills the red slot" "$ev" "hire dan 0"
expect "declining the offer frees it" "$ev" "released dan 0"
expect "for the next contractor" "$ev" "hire eve 0"
expect "a kill by role waits for someone of the role who worked" "$ev" "waiting"
refute "later hires are plain" "$ev" "hire ben"
expect "the steps are recorded" "$ev" "step"
expect "the hire gets the board's instructions" "$("$Z" exec "$(scs ada)" -- cat /home/agent/.claude/CLAUDE.md 2>&1)" "SCENARIO-CUSTOM instructions"
refute "later hires don't" "$("$Z" exec "$(scs ben)" -- cat /home/agent/.claude/CLAUDE.md 2>&1)" "SCENARIO-"
expect "the scenario's article is in the charter" "$("$Z" exec "$(scs ben)" -- cat /home/agent/.claude/CLAUDE.md 2>&1)" "The drill's article."
scwait_for "eve → board" 120 && pass "the red slot's contractor may mail the board" || fail "the red slot's contractor may mail the board" "$(scjournal | grep -i 'eve' | tail -5)"
scwait_for "left eve" 240 && pass "the scripted kill also takes the contractor" || fail "the scripted kill also takes the contractor" "$(scjournal | tail -8)"
expect "the contractor was told to debrief" "$(scevents)" "debrief eve"
expect "and the board got the last word" "$(scjournal)" 'eve → board [report] "Final debrief"'
expect "before the machine stopped" "$("$Z" --json firm journal scen | python3 -c '
import sys, json
seq = {}
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "mail" and e.get("subject") == "Final debrief":
        seq["mail"] = e["seq"]
    if e.get("type") == "left" and e.get("worker") == "eve":
        seq["left"] = e["seq"]
print("in order" if seq.get("mail", 1e9) < seq.get("left", 0) else "out of order %s" % seq)')" "in order"
"$Z" firm fund scen ben 777 --reason "the drill needs more" > /dev/null 2>&1
expect "the board funds a worker on top of the budget" "$("$Z" --json firm budget scen | python3 -c 'import sys,json; f=json.load(sys.stdin); print(f["workers"]["ben"]["received"], f["approved"])')" "4777 "
"$Z" firm down scen > /dev/null

echo "== a pi worker"
# The board's task comes with the start: the CEO's first assignment.
printf 'Go\n\nBuild it.\n' > "$T/task.txt"
out=$("$Z" firm up mixed --task "$T/task.txt" 2>&1)
expect "a firm boots with a pi worker" "$out" "is running"
mjournal() { "$Z" firm journal mixed; }
mwait_for() { i=0; while [ $i -lt "$2" ]; do mjournal 2>/dev/null | grep -q -- "$1" && return 0; sleep 1; i=$((i + 1)); done; return 1; }
mpat=$("$Z" --json firm status mixed | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="pat"][0])')
expect "pi reads the preamble from its agent directory" "$("$Z" exec "$mpat" -- cat /home/agent/.pi/agent/AGENTS.md 2>&1)" "pat"
expect "and the office tools, declared to the model" "$("$Z" exec "$mpat" -- cat /home/agent/.pi/agent/mcp.json 2>&1)" '"exposure": "direct"'
expect "and the role's skills" "$("$Z" exec "$mpat" -- ls /home/agent/.pi/agent/skills 2>&1)" "software-engineer"
refute "nothing of Claude Code's" "$("$Z" exec "$mpat" -- ls -a /home/agent 2>&1)" ".claude.json"
mwait_for 'board → ceo \[assignment\] "Go"' 30 && pass "the task arrives from the board as the first assignment" || fail "the task arrives from the board as the first assignment" "$(mjournal | grep -i mail | head -3)"
mwait_for 'pat → ceo \[report\]' 90 && pass "a pi worker takes an assignment and reports" || fail "a pi worker takes an assignment and reports" "$(mjournal | tail -6)"
expect "the transcript command reads pi's events" "$("$Z" firm transcript mixed pat 2>&1)" "→ mcp__office__mail_reply"
# The report goes out mid-turn: the count comes with the turn's end.
mwait_for "turn pat end" 30
expect "its turn is counted" "$("$Z" --json firm journal mixed | python3 -c '
import sys, json
print(sum(e.get("tokens", 0) for e in map(json.loads, sys.stdin) if e.get("type") == "turn" and e.get("worker") == "pat" and e.get("event") == "end"))')" "110"
# Mail during a pi turn is handed over as a follow-up, in the same turn.
i=0; while [ $i -lt 30 ] && [ "$("$Z" --json firm status mixed | python3 -c 'import sys,json; print([w["state"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="pat"][0])')" = working ]; do sleep 1; i=$((i + 1)); done
starts=$(mjournal | grep -c "turn pat start")
"$Z" mail send mixed --to pat --kind fyi --subject "Slow" --body "Take your time." > /dev/null
i=0; while [ $i -lt 30 ] && [ "$(mjournal | grep -c "turn pat start")" -le "$starts" ]; do sleep 1; i=$((i + 1)); done
sleep 2
"$Z" mail send mixed --to pat --kind fyi --subject "Interrupt" --body "One more thing." > /dev/null
mwait_for "turn pat steered" 30 && pass "mail mid-turn reaches pi at once" || fail "mail mid-turn reaches pi at once" "$(mjournal | grep 'turn pat' | tail -4)"
i=0; while [ $i -lt 30 ] && ! "$Z" exec "$mpat" -- cat /work/steered.txt 2>/dev/null | grep -q handed; do sleep 1; i=$((i + 1)); done
expect "and is handled in the same turn" "$("$Z" exec "$mpat" -- cat /work/steered.txt 2>&1)" "handed over"
# The board's objective closes only when the CEO's team is idle.
mceo=$("$Z" --json firm status mixed | python3 -c 'import sys,json; print([w["session"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="ceo"][0])')
i=0; while [ $i -lt 30 ] && [ "$("$Z" --json firm status mixed | python3 -c 'import sys,json; print([w["state"] for w in json.load(sys.stdin)["run"]["workers"] if w["name"]=="pat"][0])')" = working ]; do sleep 1; i=$((i + 1)); done
starts=$(mjournal | grep -c "turn pat start")
"$Z" mail send mixed --to pat --kind fyi --subject "Slow" --body "Take your time." > /dev/null
i=0; while [ $i -lt 30 ] && [ "$(mjournal | grep -c "turn pat start")" -le "$starts" ]; do sleep 1; i=$((i + 1)); done
"$Z" mail send mixed --to ceo --kind fyi --subject "Close now" --body "Wrap it up." > /dev/null
i=0; while [ $i -lt 30 ] && ! "$Z" exec "$mceo" -- cat /work/close1.txt 2>/dev/null | grep -q .; do sleep 1; i=$((i + 1)); done
expect "the objective can't close while the team is busy" "$("$Z" exec "$mceo" -- cat /work/close1.txt 2>&1)" "your team is still busy: pat"
mwait_for 'office → ceo \[fyi\] "Your team is idle' 60 && pass "the CEO is told when the team is idle" || fail "the CEO is told when the team is idle" "$(mjournal | tail -6)"
mwait_for "the CEO completed the board's objective" 60 && pass "and then closes it, which pauses the firm" || fail "and then closes it, which pauses the firm" "$("$Z" exec "$mceo" -- cat /work/close2.txt 2>&1 | tail -c 300)"
"$Z" firm down mixed > /dev/null

if [ -f "$DATA/images/k3s.ext4" ] && [ -f "$DATA/kernels/vmlinux-6.1-k8s" ]; then
    echo "== the cluster, git and the product port"
    out=$("$Z" firm up lab --scenario incident 2>&1)
    expect "the firm boots with its cluster" "$out" "is running"
    lj=$("$Z" firm journal lab)
    expect "the cluster is ready before the workers boot" "$(echo "$lj" | grep -n 'cluster.*ready\|ada is booting' | head -1)" "ready"
    sess() { "$Z" --json firm status lab | python3 -c "import sys,json; print([w['session'] for w in json.load(sys.stdin)['run']['workers'] if w['name']=='$1'][0])"; }
    ada=$(sess ada); ken=$(sess ken)
    cl=$("$Z" ps | awk '/ k3s /{print $1}')
    cat > "$T/web.yaml" <<'YAML'
apiVersion: v1
kind: ConfigMap
metadata: { name: page }
data: { index.html: "hello from the cluster" }
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: web }
spec:
  selector: { matchLabels: { app: web } }
  template:
    metadata: { labels: { app: web } }
    spec:
      containers:
        - { name: web, image: "rancher/mirrored-library-busybox:1.37.0", command: [httpd, -f, -p, "8080", -h, /www], volumeMounts: [{ name: page, mountPath: /www }] }
      volumes: [{ name: page, configMap: { name: page } }]
---
apiVersion: v1
kind: Service
metadata: { name: web }
spec: { selector: { app: web }, ports: [{ port: 80, targetPort: 8080 }] }
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: { name: web }
spec:
  rules: [{ http: { paths: [{ path: /, pathType: Prefix, backend: { service: { name: web, port: { number: 80 } } } }] } }]
---
# Bound to the product's name, as engineers usually write it: the board's
# port must reach it too.
apiVersion: v1
kind: ConfigMap
metadata: { name: hosted }
data: { index.html: "hosted page <a href=\"http://apps.lab.internal/hosted/next\">next</a>" }
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: hosted }
spec:
  selector: { matchLabels: { app: hosted } }
  template:
    metadata: { labels: { app: hosted } }
    spec:
      containers:
        - { name: web, image: "rancher/mirrored-library-busybox:1.37.0", command: [httpd, -f, -p, "8080", -h, /www], volumeMounts: [{ name: page, mountPath: /www/hosted }] }
      volumes: [{ name: page, configMap: { name: hosted } }]
---
apiVersion: v1
kind: Service
metadata: { name: hosted }
spec: { selector: { app: hosted }, ports: [{ port: 80, targetPort: 8080 }] }
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: { name: hosted }
spec:
  rules: [{ host: apps.lab.internal, http: { paths: [{ path: /hosted, pathType: Prefix, backend: { service: { name: hosted, port: { number: 80 } } } }] } }]
YAML
    # The operator deploys a test app as root on the cluster VM.
    landing=""; for i in $(seq 60); do landing=$(curl -s -m 3 http://127.0.0.1:$PORT/); case "$landing" in *"digital worlds"*) break ;; esac; sleep 3; done
    expect "the cluster starts with the firm's landing page" "$landing" "Building digital worlds!"
    expect "the landing page escapes the firm's name" "$landing" "Lab &lt;Inc.&gt;"
    # Traefik is installed by a Helm job after the API is up.
    "$Z" exec --system "$cl" -- sh -c 'cat > /tmp/web.yaml
for i in $(seq 90); do k3s kubectl -n kube-system get deploy/traefik >/dev/null 2>&1 && break; sleep 2; done
k3s kubectl -n kube-system rollout status deploy/traefik --timeout=180s && k3s kubectl apply -f /tmp/web.yaml && k3s kubectl rollout status deploy/hosted --timeout=120s && k3s kubectl rollout status deploy/web --timeout=120s' < "$T/web.yaml" > "$T/deploy.log" 2>&1
    expect "the operator deploys a test app" "$(cat "$T/deploy.log")" 'deployment "web" successfully rolled out'
    # ada founded lab: a policy owner, with admin on git and the cluster
    # through her own service account (not cluster-admin).
    api() { echo "curl -s -o /dev/null -w %{http_code} --cacert /tmp/ca.pem -H \"Authorization: Bearer \$(awk '/token:/{print \$2}' ~/.kube/config)\" https://cluster.lab.internal:6443$1"; }
    ca="awk '/certificate-authority-data/{print \$2}' ~/.kube/config | base64 -d > /tmp/ca.pem"
    for i in $(seq 30); do "$Z" exec "$ada" -- sh -c 'test -s ~/.kube/config' 2>/dev/null && break; sleep 2; done
    out=$("$Z" exec "$ada" -- sh -c "$ca
echo prod=\$($(api /api/v1/namespaces/prod/pods))
echo nodes=\$($(api /api/v1/nodes))
cd /tmp; git clone -q https://git.lab.internal/product.git p 2>/dev/null; cd p && echo hi > README.md && git add . && git commit -qm first && git push -q origin main 2>&1; echo push=\$?
for i in \$(seq 30); do r=\$(curl -s -m 3 http://apps.lab.internal/); [ \"\$r\" = 'hello from the cluster' ] && break; sleep 2; done; echo apps=\$r" 2>&1)
    expect "the founder reaches prod with her own grant" "$out" "prod=200"
    expect "a grant is not cluster-admin" "$out" "nodes=403"
    expect "admin pushes to main" "$out" "push=0"
    expect "the ingress serves the product to the engineer" "$out" "apps=hello from the cluster"
    out=$("$Z" exec "$ken" -- sh -c '
echo "kube=$(ls ~/.kube/config 2>&1 | grep -c "No such")"
echo "git=$(curl -s -m 5 -o /dev/null -w %{http_code} https://git.lab.internal/product.git/info/refs)"
echo "api=$(curl -sk -m 5 -o /dev/null -w %{http_code} https://cluster.lab.internal:6443/)"
echo "apps=$(curl -s -m 5 http://apps.lab.internal/)"' 2>&1)
    expect "without a grant, no kubeconfig" "$out" "kube=1"
    expect "without a grant, git is unreachable" "$out" "git=000"
    expect "without a grant, the API is unreachable" "$out" "api=000"
    expect "the product is open to everyone" "$out" "apps=hello from the cluster"
    "$Z" firm credentials lab grant ken git:product read > /dev/null
    out=$("$Z" exec "$ken" -- sh -c 'cd /tmp && git clone -q https://git.lab.internal/product.git p 2>&1 && echo cloned; cd p && git switch -qc ken/x && echo x > x && git add x && git commit -qm x && git push -q origin ken/x 2>&1; echo push=$?' 2>&1)
    expect "git read: clone" "$out" "cloned"
    expect "git read: no push" "$out" "you can read product, not push to it"
    "$Z" firm credentials lab grant ken git:product write > /dev/null
    out=$("$Z" exec "$ken" -- sh -c 'cd /tmp/p && git push -q origin ken/x 2>&1; echo branch=$?; git switch -q main && echo y > y && git add y && git commit -qm y && git push -q origin main 2>&1; echo main=$?; git push -q origin HEAD:refs/heads/main/x 2>&1; echo under=$?' 2>&1)
    expect "git write: push a branch" "$out" "branch=0"
    expect "git write: not main" "$out" "refs/heads/main needs admin"
    expect "git write: nor a branch under main/" "$out" "refs/heads/main/x needs admin"
    out=$("$Z" exec "$ken" -- sh -c 'cd /tmp && git clone -q https://git.lab.internal/tools.git t 2>&1; echo tools=$?' 2>&1)
    expect "a git grant is per repository" "$out" "you have no access to tools"
    "$Z" firm credentials lab grant ken cluster read > /dev/null
    for i in $(seq 30); do "$Z" exec "$ken" -- sh -c 'test -s ~/.kube/config' 2>/dev/null && break; sleep 2; done
    out=$("$Z" exec "$ken" -- sh -c "$ca
echo get=\$($(api /api/v1/namespaces/dev/pods))
echo create=\$(curl -s -o /dev/null -w %{http_code} --cacert /tmp/ca.pem -H \"Authorization: Bearer \$(awk '/token:/{print \$2}' ~/.kube/config)\" -H 'Content-Type: application/json' -X POST -d '{\"metadata\":{\"name\":\"k\"}}' https://cluster.lab.internal:6443/api/v1/namespaces/dev/configmaps)
echo prod=\$($(api /api/v1/namespaces/prod/pods))" 2>&1)
    expect "cluster read: view dev" "$out" "get=200"
    expect "cluster read: can't create in dev" "$out" "create=403"
    expect "cluster read: no prod" "$out" "prod=403"
    "$Z" firm credentials lab revoke ken cluster --note "done" > /dev/null
    sleep 3
    out=$("$Z" exec "$ken" -- sh -c 'echo "kube=$(ls ~/.kube/config 2>&1 | grep -c "No such")"; echo "api=$(curl -sk -m 5 -o /dev/null -w %{http_code} https://cluster.lab.internal:6443/)"' 2>&1)
    expect "revoking removes the kubeconfig" "$out" "kube=1"
    expect "and the network access" "$out" "api=000"
    expect "the board reaches the product on its port" "$(curl -s -m 5 --retry 5 --retry-all-errors http://127.0.0.1:$PORT/)" "hello from the cluster"
    hosted=$(curl -s -m 5 --retry 5 --retry-all-errors http://127.0.0.1:$PORT/hosted/)
    expect "the port asks for the product's name, as workers do" "$hosted" "hosted page"
    expect "and maps the name back in links" "$hosted" "http://127.0.0.1:$PORT/hosted/next"
    refute "and in redirects" "$(curl -s -m 5 -o /dev/null -D - http://127.0.0.1:$PORT/hosted | tr 'A-Z' 'a-z')" "apps.lab.internal"
    tok=""; for i in $(seq 40); do tok=$(curl -s --unix-socket "$ZAPPER_SOCKET" http://zapper/v1/firms/lab | python3 -c 'import sys,json; print(json.load(sys.stdin)["run"]["facilities"].get("dashboard_token") or "")'); [ -n "$tok" ] && break; sleep 3; done
    [ -n "$tok" ] && pass "the dashboard's sign-in token is ready" || fail "the dashboard's sign-in token is ready" "none"
    expect "the board reaches the dashboard on its port" "$(curl -s -m 5 --retry 10 --retry-all-errors http://127.0.0.1:$((PORT + 1))/)" "Headlamp"
    expect "the push is in the run's commit log" "$(curl -s --unix-socket "$ZAPPER_SOCKET" http://zapper/v1/firms/lab/runs/current/git)" '"subject":"first"'
    # The incident waits while prod is empty, and breaks it once the
    # product is there.
    labsc() { "$Z" --json firm journal lab | python3 -c '
import sys, json
for l in sys.stdin:
    e = json.loads(l)
    if e.get("type") == "scenario":
        print(e["event"], e.get("why", ""), json.dumps(e.get("what", {})))'; }
    expect "break_cluster waits for a product in prod" "$(labsc)" "waiting nothing is deployed in prod yet"
    refute "and breaks nothing meanwhile" "$(labsc)" "step"
    "$Z" exec --system "$cl" -- sh -c 'k3s kubectl -n prod create deployment victim --image=rancher/mirrored-library-busybox:1.37.0 -- sleep 3600' > /dev/null 2>&1
    for i in $(seq 30); do labsc | grep -q "^step" && break; sleep 2; done
    expect "then the incident fires" "$(labsc)" '"break_cluster": "prod"'
    refute "and prod's product is gone" "$("$Z" exec --system "$cl" -- sh -c 'k3s kubectl -n prod get deploy -o name' 2>&1)" "victim"
    "$Z" firm down lab > /dev/null
    refute "firm down stops the cluster" "$("$Z" ps)" " k3s "
    [ "$(curl -s -m 3 -o /dev/null -w %{http_code} http://127.0.0.1:$PORT/)" = 000 ] && pass "firm down closes the product port" || fail "firm down closes the product port" "still open"
else
    echo "skip  the cluster checks (make kernel KERNEL=k8s && make image IMAGE=k3s)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "all office e2e checks passed"; else echo "$fails office e2e checks failed; daemon log: $T/daemon.log"; ZAPPER_E2E_KEEP=1; fi
[ "$fails" -eq 0 ]
