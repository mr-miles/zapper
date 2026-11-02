# Code-quality review

A whole-workspace review for complexity, unclear/multiple responsibilities, poor test coverage, duplication, unnecessary steps and dead code, with proposals aimed at testability and readability. Reviewed 2026-11-02 against branch `feat/office-process-step4`.

**Scope:** all crates under `crates/`. The large daemon internals, the policy/support crates and the office firm-logic were deep-read by cluster; the CLI, shared client and dead-code scan were done directly. Every headline claim below was spot-verified against source.

**Status key:** `[ ]` open · `[x]` done · `[~]` in progress. IDs are stable — cite them when resuming. Mark a finding done in its line **and** add a dated note to the Progress log at the bottom.

## Overall verdict

The codebase is in good health: almost no dead-code markers (two `#[allow(dead_code)]`, one vendored TODO), heavy documentation, and the security-critical seams that are tested are tested well (policy rule precedence in `eval.rs`, `zapper-mint` expiry, `zapper-keeper` hash chains, core's `addr`/`dns`/`secrets` helpers). The debt is concentrated in three recurring shapes: a few very large multi-responsibility functions on hot paths (**B**), two kinds of duplication (**C**), and security-relevant seams tested only end-to-end or not at all (**A**). Fixing **A** is the highest-value work; **C1** (unifying the office's two decision subsystems) is the biggest structural win.

---

## A. Test gaps on security-sensitive code (highest value)

- [ ] **A1 — `zapper-git` branch-protection hook has zero tests.** `crates/zapper-git/src/lib.rs:154-218` (`PRE_RECEIVE`). The embedded shell script *is* the crate's security purpose (keep `write` off `main`/`main/*`/tags, confine `merge` to a clean fast-forward of someone else's branch with nothing added, let `admin` through). A regression in the string silently weakens branch protection with no signal.
  **Fix / test:** a shell-level integration test — init a bare repo, install the hook, assert: `write` pushing `main`, `main/x`, a tag is refused but a plain branch succeeds; `merge` fast-forwards `main` to another user's tip but refuses a tip containing the merger's own commits (both the `zapper-pushed` and author/committer-name paths) and refuses a merge commit whose tree adds anything (`merge-tree` mismatch); `admin` bypasses all; `pre_receive_with_local_admin()` exits 0 when `ZAPPER_GIT_LEVEL` is unset.
  *Verified:* hook present and untested.

- [ ] **A2 — `zapper-office/security.rs` has one test for 1578 lines.** The office's defining invariants — "nobody decides their own hire's background check" (`decide_screening`, `src/security.rs:433`, `if s.by == by`) and "never the requester" (`pick_officer`, `:141`) — are exercised only indirectly via `tools_tests.rs`.
  **Fix / test:** extract the guards into small pure predicates (e.g. `may_screen(officer, screening)`), then add direct negative tests: an officer who is also the hirer of a pending check is refused while a second officer is allowed.
  *Verified:* `grep -c '#\[test\]' security.rs` = 1.

- [ ] **A3 — `zapper-record` has zero tests**, including `healthy()`, the flag that stops the daemon taking firm turns on a keeper-append failure (AGENTS.md invariant). `crates/zapper-record/src/lib.rs`.
  **Fix / test:** assert path→stream routing (`stream()`/`view()`, :77-92 — a `state` path with a valid stream suffix routes to the keeper, a non-matching path writes locally); a failing keeper append flips `healthy()` false and a later success flips it back (:137-146); `Writer::line` rejects a line containing `\n`; `verify_dir` (:178-216) on a tampered stream.
  *Verified:* 0 files with `#[cfg(test)]` in the crate.

- [ ] **A4 — core `aws::resign` has no unit test.** `crates/zapper-core/src/daemon/proxy/aws.rs:624`. The fake-key re-signing gate (the named secret must be rule-listed and sendable to `host`, else the request is refused) — a security invariant covered only end-to-end; the `tests` module exercises only `sign()`/`Chunks`.
  **Fix / test:** drive `resign` with a fake-key request against configs where the secret is / isn't listed and `host` is / isn't allowed; assert refuse vs sign.
  *Verified:* `resign(` appears only at its definition.

- [ ] **A5 — core `proxy::resolve` address selection untested.** `crates/zapper-core/src/daemon/proxy.rs:350`. Implements "connect only to the address you checked; reject a name that resolves only to forbidden addresses" — covered only e2e (the underlying `addr::checked` is well unit-tested).
  **Fix:** factor the selection loop to take an iterator of `SocketAddr` (not do DNS itself) so it is unit-testable with synthetic lists.

- [ ] **A6 — `zapper-ssh` forwarding/X11/agent refusals and channel cap untested.** `crates/zapper-ssh/src/lib.rs:285-465`, cap at `:306`. Refusal of remote (`tcpip-forward`), X11 and agent forwarding relies on russh *defaults*, not an explicit override — a library upgrade could weaken it silently.
  **Fix / test:** open a remote-forward global request and an X11 request and assert both fail; open `MAX_CHANNELS+1` sessions and assert the last is rejected with `ResourceShortage`.

- [ ] **A7 — `zapper-client::Zapper` methods untested.** `crates/zapper-client/src/lib.rs` — `call`/`call_raw`/`stream`/`upgrade`; only the `gateway` sub-module is tested. This is the socket client both the office and workbench depend on (and `call_raw` was just added).
  **Fix / test:** a unix-socket round-trip test serving a tiny axum router (the office `standalone` tests show the pattern): assert JSON round-trip, a raw-bytes body, and an error-status path.

- [ ] **A8 — `zapper-keeper` `MAX_OPEN` eviction path untested.** `crates/zapper-keeper/src/lib.rs:440-451`. Reopen-resumes-chain is tested for one stream, not across evict-then-reappend of many.
  **Fix / test:** append to `> MAX_OPEN` distinct streams, then re-append to an early one and `verify` its chain is unbroken.

## B. Large multi-responsibility functions (readability + testability)

- [ ] **B1 — `office mail::send` is a god-function (~`src/mail.rs:492-1062`).** Subject derivation, size/attachment/limit validation, address expansion, chart-authority, three rate limits (one composing and sending a board notice mid-function), journaling, obligation settle/create, unread + pair-log + kickoff.
  **Fix:** split into `validate`, `check_authority`, `rate_guard` (returns a decision; caller sends the notice), `record_and_deliver`, `settle_and_oblige` — each unit-testable.
  *Verified:* `send` at 492; next method `send_notice` at 1064.

- [ ] **B2 — `zapper/main.rs::run()` is a ~531-line dispatch (`:474-1005`)** handling ~25 subcommands inline. Several commands are already extracted (`service`, `attach`, `build_in_daemon`).
  **Fix:** finish the pattern — one small `async fn` per command, `run()` becomes a thin router. Each handler becomes independently testable.

- [ ] **B3 — `office foreman::run` turn loop (`src/foreman.rs:153-408`)** interleaves permit acquisition, four blocking gates, and a 3-arm `match outcome` whose `End`/`Late`/`Exit` arms each repeat requeue + requeue_access + backoff + journal.
  **Fix:** lift outcome handling into `handle_outcome(...) -> Control`; collapse the duplicated requeue calls into one helper per failure path.

- [ ] **B4 — `office workforce::let_go` (`:1439-1532`) and `security::decide_screening` (`:392-469`)** are orchestrators mixing a policy decision with many downstream effects (grants, offboarding, mail, ledger).
  **Fix:** return a typed decision and run effects in a separate `apply_clearance`/`apply_objection` / grouped `let_go` steps (`detach_from_chart`, `move_money`, `hand_off_mail`, `stop_vm`).

- [ ] **B5 — core hot-path density.** `inspect::forward` (`proxy/inspect.rs:506`, ~110 lines) — peel pre-dispatch validation into `prepare_inspected()`. `aws::sign` (`proxy/aws.rs:257`, ~110 lines) — split pure `canonicalize()` from the two credential-styles `apply_*`. `driver::run` (`daemon/driver.rs:282`, ~156-line loop) — per-outcome methods on a small `TurnState`.

- [ ] **B6 — `policy::check_rules` (`config.rs:1036-1093`)** runs four unrelated validations in one nested loop (duplicate ids, glob compilation, secret existence + aws-header ban, the dbauth/db-relay matrix).
  **Fix:** extract `check_ids_and_globs` / `check_net_secrets` / `check_db_relay`; the existing per-message tests map cleanly onto them.

- [ ] **B7 — `zapper::check()` (`main.rs:1395+`)** is a long linear sequence of `report(...)` host checks — readable but untestable as a unit.
  **Fix:** a data-driven list of named checks so each is testable and rendered uniformly.

## C. Duplication (drift risk)

- [ ] **C1 — `office security.rs` vs `access.rs`: two parallel decision subsystems.** Both carry a `Request` struct, `decide`/`decide_screening`, `escalate`, `tick`, reassign/requeue (`security::requeue :594`/`adopt :545` vs `access::reassign :916`), board-fallback and notify. The self-decide, round-robin and board-fallback invariants are implemented twice.
  **Fix:** extract a shared pending-decision-queue (assign/reassign/escalate/notify over a trait for network-request vs screening vs credential-request); each module keeps only its domain-specific rule effects. Lets the queue invariants be unit-tested once. Biggest structural win.
  *Verified:* parallel `Request`/`decide`/`escalate`/`tick`/reassign in both files.

- [ ] **C2 — `read_frame` triplicated.** `zapper-driver/src/lib.rs:22`, `zapper-ssh/src/lib.rs:271`, `zapper-core/src/daemon/session/terminal.rs:322` — identical 5-byte-header read → `Frame::header` → `vec![0; len]` → `read_exact` → `Frame::decode` (the `debug_assert!(len <= MAX_FRAME)` is redundant; the bound is enforced in `Frame::header`).
  **Fix:** one `read_frame` in `zapper-proto` next to `Frame`; all three call it.
  *Verified:* three definitions.

- [ ] **C3 — core database relays ×4.** `proxy/relay.rs` `redis_exchange :294`, `cassandra_exchange :318`, `mongodb_exchange :343`, `kafka_exchange :571` — identical `guest_leg → server_leg → <proto>::authenticate → Relayed{to_guest: vec![]}`, differing only in the auth call.
  **Fix:** a `simple_relay(..., auth_closure)` helper; each protocol passes its `authenticate`.
  *Verified:* `redis_exchange` body matches the skeleton.

- [ ] **C4 — core datapath fail-logging repeated.** `proxy.rs:433,446`, `relay.rs:123`, `inspect.rs:145` — the `log(NetLog{ error: Some(..), .. }) + note_failure(..)` pairing recurs at every failure site; the event and the record must agree and are easy to desync.
  **Fix:** one `fail_request(d, s, rec, net, v, status, error)` helper covering both.

- [ ] **C5 — office small duplications.** The founding-officer test `chart.is_officer(w) && chart.root() != Some(w) && run.config.worker(w).is_some()` is inlined 3× (`workforce.rs:520,687-688,697`) → `chart.is_founding_officer(run, w)`. The funds-`"move"` journal `json!` appears 8× across `workforce.rs`/`contracts.rs` → `run.journal_move(from, to, tokens, reason)`. `security::decide` (`:939-995`) role-vs-worker branches are copy-paste → compute `(scope, target)` first, build `Standing` once. `inject_for`/`value_of` (`core proxy/inspect.rs:304,380`) look up + policy-check each secret twice per request → hand the resolved `(secret, sendable)` forward.
  *Verified:* founding-officer test inlined 3×.

## D. Dead code / unnecessary steps / minor

- [ ] **D1 — `office ops::board()` is dead** (`src/ops.rs:1400-1403`, `#[allow(dead_code)]`, no callers). Delete it and the silencer.
  *Verified:* only test *names* match `board`; no call sites.

- [ ] **D2 — git hook appends `zapper-pushed` before it can `refuse`** (`zapper-git/src/lib.rs:199-203`). The `git rev-list … >> "$pushed"` runs per ref at the top of the loop; because the push is atomic, a later `refuse` leaves attribution lines for commits that never entered the repo, which can only make future `not_theirs` merge checks over-refuse.
  **Fix:** stage per-ref records to a temp file and append to `zapper-pushed` only after the per-ref checks pass.
  *Verified:* append precedes the `case $ZAPPER_GIT_LEVEL` that calls `refuse`.

- [ ] **D3 — core `inject_for`/`value_of` recompute** `find`+`check` per secret (`proxy/inspect.rs:304,380`) — see C5; the second policy check is pure recomputation.

- [ ] **D4 — minor.** `office skills.rs` `Front.description` (`:113`) is parsed but never read (no `deny_unknown_fields`, so not even needed to accept the key). `ops.rs` TOOLS lists `"contracts"` twice (`:797,:855`, differ only by `shown`) — add a comment or merge with a predicate. `security.rs:273` builds a board notice with `format!` interpolating a constant literal and always emits a parenthetical ("the firm has no other officer") that isn't always the true reason.

---

## Suggested sequencing

1. **Quick wins (low risk, high clarity):** D1 (delete `board()`), C2 (`read_frame` → `zapper-proto`), D4 minors, C5 small office helpers.
2. **High-value testability:** A1 (git-hook test), A3 (record tests), A2 + A4 (extract guards then test), A7 (client round-trip test).
3. **Larger refactors — each its own change:** B1 (`mail::send`), C1 (security/access unification), B2 (CLI `run()`), C3 (database relays), B3/B4 (foreman / `let_go`).

Each change must keep `make lint && make test` green; security-relevant ones (A1, A4, C1, D2) should land with the new tests that prove the behavior they touch.

## Progress log

- 2026-11-02 — Review written; nothing from this document implemented yet. (Separately, the architecture-review items A–D and the `api-types → proto` edge removal are already done; see `docs/crate-dependencies.md` and `docs/office-process-step4-plan.md`. This document is the code-quality review and is independent of those.)
