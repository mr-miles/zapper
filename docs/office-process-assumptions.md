# office-process build: assumptions to review

Working notes from the unattended build of [proposal 13](proposals/13-office-process.md)
on `feat/office-process`. Each is a choice made where the proposal or the code left
something open; review and correct as needed. Delete this file before the PR merges.

## Status

Done, in-process, each its own commit(s), `make test` green (647) and `make lint` clean:

- **§4** decider socket — trait async, core socket mechanism, office moved onto it.
- **§5** named layers in the core, attached by a `firm-role` label; office off `shared_layers`.
- **§6** `zapper-driver` crate (harnesses' conversation code + process driver).
- **§9** firm turns gated on the record through `Core::record_writable()`.

Not done (need a KVM host for `make test-e2e-office`, and better reviewed attended):

- **§10** migrate the office's in-process `TestCore` tests to the API-over-socket pattern
  (unit-level, no VMs, but large and mechanical — ~24k lines of tests).
- **Step 4** `zapper-office` as its own executable: its listener/pages (§7), config
  (`office.yaml`), lifecycle (§8, kept-until-removed + resume-by-cursor), and `zapper` no
  longer linking it. This is the actual process split; validate with `make test-e2e-office`.

Note: `make test-e2e-office` was **not run** here (no KVM); even the done steps should pass
it before merge. The `target/` dir had filled the disk mid-session — cleared stale
`target/debug/incremental` (15G); nothing in the repo was touched.

## §4 — decider socket

- **Timeout 10s**, on no answer or an error the request is refused as a denial (per the
  proposal). Constant `ASK_TIMEOUT` in `deciders/socket.rs`.
- **`allowed` notes are fire-and-forget**: `SocketDecider::allowed` spawns a task to send
  the note and does not wait, so a slow/absent decider never holds a request its rule
  already allowed. Cost: if the send fails (socket gone mid-run), a *once-only* standing
  approval might be used more than once. Low severity, only on socket failure.
- `MAX_MCP_LINE` (128 KiB) reused as the decider reply bound (it is the generic line cap
  in `zapper-proto`).

## §5 — named layers attached by labels

- **One active run per firm at a time.** A role's layer is the core named layer
  `<firm>/<role>`, reset to empty at run start. Two concurrent runs of the *same* firm
  would share the layer. If concurrent same-firm runs are possible, key the layer (and the
  `firm-role` label value) by run id instead.
- **`firm-role` label value = `<firm>/<role>` = the layer's name.** Attachment is a single
  key lookup per the fixed precedence slot (session own → firm-role → profile → global).
- **Only the `firm-role` slot is implemented.** The proposal also notes a firm-wide layer
  (`firm` key) falls out "for free"; not added (the office does not use one today). Adding
  it needs a scope for the firm slot (`Scope::Firm`, or a generic `Scope::Layer`).
- **`RuleKeeper` is kept**, now reading the same Arcs the core vends, so `zapper rules`
  still lists role layers. The proposal has it "go" in favour of the core listing named
  layers natively (with §2 rule events); left as a later cleanup to keep the migration
  behaviour-preserving.

## §6 — the driver as its own crate

- **Scope taken = the proposal's literal §6(a):** "the harnesses' conversation code and
  the process driver" moved to `zapper-driver` (deps: `zapper-policy`, tokio, serde, rand).
  The `Driver` orchestration struct and `drive()` glue (Weak<Daemon>/Session, events,
  record) stay in `zapper-core/daemon/driver.rs` as the core's user of the crate. The
  office still calls the core's driver in-process; it only *links* the crate directly once
  it becomes a separate process (step 4). So no office change in this step.
- **`read_frame` duplicated.** `zapper-driver` has its own 12-line `read_frame` (proto
  Frame reader); the core keeps its copy in `session/terminal.rs`. Alternative: hoist one
  copy into `zapper-proto`. Chose duplication to avoid touching proto + terminal.
- **`MANAGED_SETTINGS` is now `pub`** (was `pub(crate)` + `#[cfg(test)]` re-export) so the
  core's `spec.rs` test can read it across the crate boundary. It is part of the harness's
  public surface now.
- Core re-exports the crate as `daemon::harness` (`pub use zapper_driver::harness;`) so the
  existing `crate::daemon::harness::…` paths resolve unchanged.

## §9 — record check through the interface

- The foreman gates on **`Core::record_writable()` = `record::healthy()`** (the cheap
  atomic), not `record_status()["healthy"]` (which, with a keeper, also does a `hello()`
  round-trip). Chosen to avoid a keeper round-trip on every turn-gate / idle loop; matches
  the prior office behaviour exactly. If the stricter "keeper answers now" check is wanted
  at the gate, switch to parsing `record_status()` (accepting the per-gate round-trip).
