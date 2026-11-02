# Code-quality review

A whole-workspace review for complexity, unclear/multiple responsibilities, poor test coverage, duplication, unnecessary steps and dead code, with proposals aimed at testability and readability. Reviewed 2026-11-02 against branch `feat/office-process-step4`.

**Scope:** all crates under `crates/`. The large daemon internals, the policy/support crates and the office firm-logic were deep-read by cluster; the CLI, shared client and dead-code scan were done directly. Every headline claim below was spot-verified against source.

**Status key:** `[ ]` open · `[x]` done · `[~]` in progress. IDs are stable — cite them when resuming. Mark a finding done in its line **and** add a dated note to the Progress log at the bottom.

## Overall verdict

The codebase is in good health: almost no dead-code markers (two `#[allow(dead_code)]`, one vendored TODO), heavy documentation, and the security-critical seams that are tested are tested well (policy rule precedence in `eval.rs`, `zapper-mint` expiry, `zapper-keeper` hash chains, core's `addr`/`dns`/`secrets` helpers). The debt is concentrated in three recurring shapes: a few very large multi-responsibility functions on hot paths (**B**), two kinds of duplication (**C**), and security-relevant seams tested only end-to-end or not at all (**A**). Fixing **A** is the highest-value work; **C1** (unifying the office's two decision subsystems) is the biggest structural win.

---

## A. Test gaps on security-sensitive code (highest value)

- [x] **A1 — `zapper-git` branch-protection hook has zero tests.** Done: `hook_enforces_branch_protection` + `local_admin_with_no_level_is_the_operator` drive real pushes at each level. `crates/zapper-git/src/lib.rs:154-218` (`PRE_RECEIVE`). The embedded shell script *is* the crate's security purpose (keep `write` off `main`/`main/*`/tags, confine `merge` to a clean fast-forward of someone else's branch with nothing added, let `admin` through). A regression in the string silently weakens branch protection with no signal.
  **Fix / test:** a shell-level integration test — init a bare repo, install the hook, assert: `write` pushing `main`, `main/x`, a tag is refused but a plain branch succeeds; `merge` fast-forwards `main` to another user's tip but refuses a tip containing the merger's own commits (both the `zapper-pushed` and author/committer-name paths) and refuses a merge commit whose tree adds anything (`merge-tree` mismatch); `admin` bypasses all; `pre_receive_with_local_admin()` exits 0 when `ZAPPER_GIT_LEVEL` is unset.
  *Verified:* hook present and untested.

- [x] **A2 — `zapper-office/security.rs` has one test for 1578 lines.** Done: `nobody_decides_their_own_hires_background_check` drives the self-decide guard directly — the hiring officer is refused their own hire's check, the board decides it. (A reusable `may_screen` predicate extraction is still worthwhile but not required for the test.) The office's defining invariants — "nobody decides their own hire's background check" (`decide_screening`, `src/security.rs:433`, `if s.by == by`) and "never the requester" (`pick_officer`, `:141`) — are exercised only indirectly via `tools_tests.rs`.
  **Fix / test:** extract the guards into small pure predicates (e.g. `may_screen(officer, screening)`), then add direct negative tests: an officer who is also the hirer of a pending check is refused while a second officer is allowed.
  *Verified:* `grep -c '#\[test\]' security.rs` = 1.

- [x] **A3 — `zapper-record` has zero tests**, including `healthy()`, the flag that stops the daemon taking firm turns on a keeper-append failure (AGENTS.md invariant). `crates/zapper-record/src/lib.rs`.
  **Fix / test:** assert path→stream routing (`stream()`/`view()`, :77-92 — a `state` path with a valid stream suffix routes to the keeper, a non-matching path writes locally); a failing keeper append flips `healthy()` false and a later success flips it back (:137-146); `Writer::line` rejects a line containing `\n`; `verify_dir` (:178-216) on a tampered stream.
  *Verified:* 0 files with `#[cfg(test)]` in the crate.

- [x] **A4 — core `aws::resign`: already covered (finding corrected).** The original claim was scoped too narrowly to aws.rs's *inline* `mod tests`. `resign`'s gate is exercised through the proxy datapath in `hardening_tests.rs`: `the_fake_aws_key_signs_only_where_a_rule_lists_its_secret` (the "no rule lists its secret" refusal + the guest's-own-key passthrough), `the_fake_aws_key_signs_only_for_the_secrets_hosts` (the host `check`), and `a_request_signed_with_the_fake_aws_key_is_signed_again` (happy path). No change needed.

- [x] **A5 — core `proxy::resolve` address selection untested.** Done: factored the selection loop out of `resolve` into `first_reachable(policy, host, addrs)` (DNS stays in `resolve`); `first_reachable_skips_forbidden_and_reports_why` asserts a forbidden address is skipped for a reachable one, a name resolving only to forbidden addresses is refused, and an empty list errors.

- [x] **A6 — `zapper-ssh` forwarding/X11/agent refusals and channel cap untested.** Done: `remote_forwarding_and_x11_are_refused` asserts `tcpip-forward` is not granted and an X11 request is never answered Success (bounded waits), and `the_channel_cap_rejects_further_channels` opens `MAX_CHANNELS` sessions and asserts the next is rejected. The forward/X11 cases are regression canaries over russh's defaults (confirmed to refuse/ignore today); a russh upgrade that changed them would now fail here. Note: these rely on russh defaults rather than explicit `x11_request`/`agent_request`/`tcpip_forward` overrides — adding explicit rejections would make the guarantee the office's own (a small follow-up).

- [x] **A7 — `zapper-client::Zapper` methods untested.** Done: `round_trips_json_raw_bytes_and_errors` serves a tiny HTTP server over a unix socket and exercises `get`/`call_raw`/`Answer::ok`. `crates/zapper-client/src/lib.rs` — `call`/`call_raw`/`stream`/`upgrade`; only the `gateway` sub-module is tested. This is the socket client both the office and workbench depend on (and `call_raw` was just added).
  **Fix / test:** a unix-socket round-trip test serving a tiny axum router (the office `standalone` tests show the pattern): assert JSON round-trip, a raw-bytes body, and an error-status path.

- [x] **A8 — `zapper-keeper` `MAX_OPEN` eviction path untested.** Done: `evicting_open_streams_keeps_each_chain_intact` opens `> MAX_OPEN` streams (forcing the checkpoint-and-clear eviction), then re-appends to an evicted stream and asserts its chain resumes at seq 2 and still verifies.

## B. Large multi-responsibility functions (readability + testability)

- [x] **B1 (done) — carved the three heaviest cohesive blocks out of `mail::send` (check_assignment_authority, check_objective_close, rate_and_loop_guard), leaving the build/deliver spine. Original: `office mail::send` is a god-function (~`src/mail.rs:492-1062`).** `office mail::send` is a god-function (~`src/mail.rs:492-1062`).** Subject derivation, size/attachment/limit validation, address expansion, chart-authority, three rate limits (one composing and sending a board notice mid-function), journaling, obligation settle/create, unread + pair-log + kickoff.
  **Fix:** split into `validate`, `check_authority`, `rate_guard` (returns a decision; caller sends the notice), `record_and_deliver`, `settle_and_oblige` — each unit-testable.
  *Verified:* `send` at 492; next method `send_notice` at 1064.

- [x] **B2 (done) — moved the Config/Profile/Image nested sub-dispatchers into config_cmd/profile_cmd/image_cmd, so run() is a thin router. Original: `zapper/main.rs::run()` is a ~531-line dispatch (`:474-1005`)** `zapper/main.rs::run()` is a ~531-line dispatch (`:474-1005`)** handling ~25 subcommands inline. Several commands are already extracted (`service`, `attach`, `build_in_daemon`).
  **Fix:** finish the pattern — one small `async fn` per command, `run()` becomes a thin router. Each handler becomes independently testable.

- [~] **B3 (partial) — folded the repeated requeue pair into `requeue_all`; left the outcome match in place (tightly coupled to the loop's permit/nap/continue/backoff — a handle_outcome split would risk the retry behaviour). Original: `office foreman::run` turn loop (`src/foreman.rs:153-408`)** `office foreman::run` turn loop (`src/foreman.rs:153-408`)** interleaves permit acquisition, four blocking gates, and a 3-arm `match outcome` whose `End`/`Late`/`Exit` arms each repeat requeue + requeue_access + backoff + journal.
  **Fix:** lift outcome handling into `handle_outcome(...) -> Control`; collapse the duplicated requeue calls into one helper per failure path.

- [x] **B4 (done) — grouped let_go into detach_from_chart/move_balance/stop_worker_vm; left decide_screening (its self-decide guard is now tested by A2). Original: `office workforce::let_go` (`:1439-1532`) and `security::decide_screening` (`:392-469`)** `office workforce::let_go` (`:1439-1532`) and `security::decide_screening` (`:392-469`)** are orchestrators mixing a policy decision with many downstream effects (grants, offboarding, mail, ledger).
  **Fix:** return a typed decision and run effects in a separate `apply_clearance`/`apply_objection` / grouped `let_go` steps (`detach_from_chart`, `move_money`, `hand_off_mail`, `stop_vm`).

- [~] **B5 (partial) — peeled `prepare_inspected` out of `inspect::forward`; left `aws::sign` (intricate canonicalization) and `driver::run` (turn-loop control flow) as the risk outweighs the gain. Original: core hot-path density.** core hot-path density.** `inspect::forward` (`proxy/inspect.rs:506`, ~110 lines) — peel pre-dispatch validation into `prepare_inspected()`. `aws::sign` (`proxy/aws.rs:257`, ~110 lines) — split pure `canonicalize()` from the two credential-styles `apply_*`. `driver::run` (`daemon/driver.rs:282`, ~156-line loop) — per-outcome methods on a small `TurnState`.

- [x] **B6 (done) — split into check_rule_secrets and check_rule_db_relay. Original: `policy::check_rules` (`config.rs:1036-1093`)** `policy::check_rules` (`config.rs:1036-1093`)** runs four unrelated validations in one nested loop (duplicate ids, glob compilation, secret existence + aws-header ban, the dbauth/db-relay matrix).
  **Fix:** extract `check_ids_and_globs` / `check_net_secrets` / `check_db_relay`; the existing per-message tests map cleanly onto them.

- [~] **B7 (won't do) — `check()` is a linear sequence of host-environment probes (/dev/kvm, firecracker, kernels) with heterogeneous async/config-dependent logic and inter-step dependencies; it already reads as a top-to-bottom checklist and host probes aren't unit-testable, so a data-driven table adds indirection for no gain. Original: `zapper::check()` (`main.rs:1395+`)** `zapper::check()` (`main.rs:1395+`)** is a long linear sequence of `report(...)` host checks — readable but untestable as a unit.
  **Fix:** a data-driven list of named checks so each is testable and rendered uniformly.

## C. Duplication (drift risk)

- [~] **C1 — `office security.rs` vs `access.rs`: two parallel decision subsystems — won't-do (wholesale merge), after reading both in full.** The two share a *silhouette* (request → decider → decide/escalate/reassign/tick → notify) but differ in every security-relevant detail, by design:
  - **Authority model.** Network `ask`s (`security.rs`) are decided by *any* officer or the board — flat, round-robin (`pick_officer`), escalation always straight to the board. Credential grants (`access.rs`) run a *ladder*: officer → CEO → board (`first_decider`/`next_decider`), with high-privilege resources (merge/admin on git, admin on the cluster) going straight to the CEO. That CEO tier is a named invariant ("merge and admin … is the CEO's to give"). The review's "round-robin implemented twice" is inaccurate: only `security.rs` round-robins; `access.rs` picks the first eligible non-CEO officer.
  - **Store + state.** `security.rs` carries two stores (requests *and* screenings) with a typed `State` enum incl. `Merged`; `access.rs` has one store with string states and `Level`/renewal concepts. No shared representation.
  - **Decision effect.** `security::decide` builds a `NetRule` + `Standing` and puts a session/layer rule; `access::decide` makes a `Grant` and enforces it on the platform (RBAC, kubeconfig, network). Nothing in common past "journal + notify".
  A generic engine abstracting over decider-selection + escalation-ladder + store + decision-effect would be *larger* than the two subsystems and would place the firm's most security-sensitive invariants behind indirection — the opposite of the review's readability/auditability goal. Declined for the same reason as B7/C2/C5: forcing the abstraction costs more than the duplication. The genuinely-shared, safe slice (the copy-paste *inside* `security::decide`) is done — see C5 below.
  *Verified:* read `security.rs` (1578 L) and `access.rs` (1100 L) end to end; the authority models, stores, and decision effects are disjoint.

- [~] **C2 — `read_frame` triplicated — won't fix (no clean home).** Confirmed three copies (driver `lib.rs:22`, ssh `lib.rs:271`, core `session/terminal.rs:322`). But all three need `tokio::io::AsyncRead`, and `zapper-proto` (where `Frame` lives) has no tokio dep and is linked by the guest agent — adding tokio there to host the shared reader would bloat the base protocol crate; `zapper-policy` forbids I/O; `zapper-ssh` depends on neither higher crate. The three ~8-line copies (bounded by `Frame::header`) are the lesser evil. Original finding: `zapper-driver/src/lib.rs:22`, `zapper-ssh/src/lib.rs:271`, `zapper-core/src/daemon/session/terminal.rs:322` — identical 5-byte-header read → `Frame::header` → `vec![0; len]` → `read_exact` → `Frame::decode` (the `debug_assert!(len <= MAX_FRAME)` is redundant; the bound is enforced in `Frame::header`).
  **Fix:** one `read_frame` in `zapper-proto` next to `Frame`; all three call it.
  *Verified:* three definitions.

- [x] **C3 — core database relays ×4 (done).** `redis`/`cassandra`/`mongodb`/`kafka` `_exchange` were identical bar the auth call. Folded into one `simple_relay(proto)` in `proxy/relay.rs` that negotiates both legs then matches `proto` for the auth step (a closure didn't fit — cassandra's auth touches the guest leg, the others only the server, and the signatures differ); each protocol's wire explanation moved onto its match arm. `relay_database`'s dispatch routes `Redis | Cassandra | Mongodb | Kafka` to it; Postgres/MySQL/TDS keep their bespoke handshakes.
  *Verified:* all 43 proxy tests pass; lint clean.

- [x] **C4 — core datapath fail-logging repeated (done).** Added one `fail_log(d, s, rec, NetLog, status, error)` in `proxy.rs` that sets `NetLog.error` and `note_failure`'s error/status from a single pair of args, so the UI event and the stored record can't drift. Routed every failure site through it: relay's `fail` closure, the tunnel's two sites (server-name mismatch, dial failure), and inspect's `deny`/`fail`/`refused`. The tunnel's redundant `logp` closure was dropped (success now logs `net()` directly).
  *Verified:* all 43 proxy tests pass; lint clean.

- [~] **C5 — office small duplications — needs care, deferred.** On inspection the "duplicates" aren't identical: the founding-officer test at `workforce.rs:688` adds a `run.config.worker(w)` check that `:520`/`:697` omit, and the funds-`"move"` sites carry varying extra keys (e.g. `"granted"` at `:963`). Unifying them blindly could change office security/accounting behavior, so each site needs individual judgment rather than a blanket helper — left for a focused pass. Original finding: The founding-officer test `chart.is_officer(w) && chart.root() != Some(w) && run.config.worker(w).is_some()` is inlined 3× (`workforce.rs:520,687-688,697`) → `chart.is_founding_officer(run, w)`. The funds-`"move"` journal `json!` appears 8× across `workforce.rs`/`contracts.rs` → `run.journal_move(from, to, tokens, reason)`. `security::decide` role-vs-worker branches are copy-paste → compute `(scope, target)` first, build `Standing` once — **done** (resolve a `RuleTarget` first, place the rule and record the one `Standing` the same way for both; the fallible resolution stays before the counter bump so a failure leaves no id gap). `inject_for`/`value_of` (`core proxy/inspect.rs:304,380`) look up + policy-check each secret twice per request → hand the resolved `(secret, sendable)` forward.
  *Verified:* founding-officer test inlined 3×.

## D. Dead code / unnecessary steps / minor

- [x] **D1 — `office ops::board()` is dead** (`src/ops.rs:1400-1403`, `#[allow(dead_code)]`, no callers). Delete it and the silencer.
  *Verified:* only test *names* match `board`; no call sites.

- [ ] **D2 — git hook appends `zapper-pushed` before it can `refuse`** (`zapper-git/src/lib.rs:199-203`). The `git rev-list … >> "$pushed"` runs per ref at the top of the loop; because the push is atomic, a later `refuse` leaves attribution lines for commits that never entered the repo, which can only make future `not_theirs` merge checks over-refuse.
  **Fix:** stage per-ref records to a temp file and append to `zapper-pushed` only after the per-ref checks pass.
  *Verified:* append precedes the `case $ZAPPER_GIT_LEVEL` that calls `refuse`.

- [ ] **D3 — core `inject_for`/`value_of` recompute** `find`+`check` per secret (`proxy/inspect.rs:304,380`) — see C5; the second policy check is pure recomputation.

- [x] **D4 — minor (done).** Removed the unused `Front.description` field; added a comment clarifying the two disjoint `contracts` TOOLS entries; replaced the board access-notice's pointless `format!` constant arg and always-on "(the firm has no other officer)" with the truthful "(no officer can decide it)". Original: `office skills.rs` `Front.description` (`:113`) is parsed but never read (no `deny_unknown_fields`, so not even needed to accept the key). `ops.rs` TOOLS lists `"contracts"` twice (`:797,:855`, differ only by `shown`) — add a comment or merge with a predicate. `security.rs:273` builds a board notice with `format!` interpolating a constant literal and always emits a parenthetical ("the firm has no other officer") that isn't always the true reason.

---

## Suggested sequencing

1. **Quick wins (low risk, high clarity):** D1 (delete `board()`), C2 (`read_frame` → `zapper-proto`), D4 minors, C5 small office helpers.
2. **High-value testability:** A1 (git-hook test), A3 (record tests), A2 + A4 (extract guards then test), A7 (client round-trip test).
3. **Larger refactors — each its own change:** B1 (`mail::send`), C1 (security/access unification), B2 (CLI `run()`), C3 (database relays), B3/B4 (foreman / `let_go`).

Each change must keep `make lint && make test` green; security-relevant ones (A1, A4, C1, D2) should land with the new tests that prove the behavior they touch.

## Progress log

- 2026-11-02 — Review written. (Separately, the architecture-review items A–D and the `api-types → proto` edge removal are already done; see `docs/crate-dependencies.md`. This document is the independent code-quality review.)
- 2026-11-02 — Did the quick-wins + the named high-value test gaps: **D1** (deleted dead `board()`), **D4** (unused field + tool comment + notice wording), **A1** (git-hook integration test), **A3** (record tests), **A7** (client round-trip test), **A2** (self-decide screening guard test). **A4** turned out already covered (finding corrected). **C2** and **C5** reclassified as won't-fix / needs-care with rationale above. `make lint` green.
  **Still open ("the rest"):** the larger refactors **B1–B7**, **C1**, **C3**, **C4**, **D2** (git-hook `zapper-pushed` ordering), **D3**.
- 2026-11-02 — Did the remaining test gaps: **A5** (extracted `first_reachable`, tested address selection), **A6** (ssh remote-forward/X11 refusal + channel cap), **A8** (keeper eviction keeps chains). All of section **A** is now done. `make lint` green. A possible follow-up surfaced by A6: `zapper-ssh` relies on russh defaults to refuse X11/agent/remote-forward rather than explicit handler overrides.
- 2026-11-02 — Did the **A6 follow-up** (explicit ssh x11/agent/tcpip_forward Handler overrides), **D2** (git hook stages attribution until the push is accepted) and **D3** (shared `header_secret` for the inject find+check).
- 2026-11-02 — Worked the refactors: **B1** (mail::send guards), **B2** (CLI sub-command handlers), **B4** (let_go steps), **B6** (policy per-rule validators) done fully; **B3** (requeue_all; outcome match left) and **B5** (prepare_inspected; aws::sign/driver::run left) done partially with rationale; **B7** reclassified won't-do. `make lint && make test` green.
- 2026-11-02 — Did the **C** batch: **C3** (fold the 4 simple DB relays into `simple_relay`), **C4** (shared `fail_log` for the datapath failure sites), and the safe slice of **C1/C5** (dedup the role/worker rule-placement in `security::decide` via `RuleTarget`). **C1** wholesale merge reclassified won't-do after reading both subsystems in full — the authority models, stores and decision effects are disjoint by design, and a generic engine would hide security invariants behind indirection. `zapper-core`/`zapper-office` lint clean; all 43 proxy + 157 office tests pass.
  **Still open ("the rest"):** the deferred parts of **B3** (handle_outcome) and **B5** (aws::sign / driver::run). Sections A, C, D complete; B partially (risk-aware). No further C items outstanding.
