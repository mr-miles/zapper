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

## Step 4 — the `zapper-office` binary (scaffold only)

On branch `feat/office-process-step4` (off `feat/office-process`). Follows the workbench's
proven pattern (its own `main.rs`, a `Zapper` client over the daemon's unix socket, its own
listener). **Scaffold only — not a working separation yet:**

- Added `crates/zapper-office/src/client.rs` (the `Zapper` API client, same as the
  workbench's — a candidate for a shared crate per §7) and `src/main.rs` + a `[[bin]]`
  target. `zapper-office serve` connects to the daemon, confirms it, and serves a
  placeholder dashboard on its own port; `zapper-office url` prints the address.
- The **lib is untouched**, so the office still runs in-process via `zapper serve`
  (feature `office`). The binary does **not** yet run firms.

Decision (you chose, unattended): **§5(a) — removal follows the core's `rule` event.**
`zapper` without the office must still remove/action approvals, and in an office run the
office drives its own layers (nobody goes round them). Built on this branch:

- **`RuleKeeper` is retired.** The core owns the firm role layers (named layers):
  `rule_layers()` lists them and `remove_rule()` takes a rule out of one and emits a `rule`
  removed event. The trait, its registry, `add_rule_keeper`, and the office's `Keeper` are
  gone.
- **The office follows the event** (`follow_rules` → `security::forget_standing`) to drop
  its own `Standing` record when a role rule is removed through the core, instead of the old
  synchronous in-process callback.
- **Behaviour change to confirm:** operator-revoke-while-the-office-runs is now
  *eventually* consistent (the office reconciles off the event stream), where it used to be
  synchronous. Enforcement (the rule leaving the layer) is still synchronous. The test
  `an_officers_approval_for_a_role_is_a_rule_the_operator_can_revoke` now polls for the
  office's reconciliation. In practice nobody revokes office rules out-of-band mid-run, so
  this window is not a live concern — but confirm you're happy with it.

The real conversion that remains (the bulk of step 4), from the `&Core` method survey
(≈40 methods over 63 sites):

- **Office-own state — not a socket call:** `office()` (32 uses) returns the office's own
  `Office` state; in a standalone process the office holds it directly. Rehome it off `Core`.
- **Daemon operations → `client::Zapper` calls:** `create_session`, `stop_session`,
  `forget_session`, `sessions`, `session`, `add_session_rule`, `remove_rule`, `services`,
  `add_service`/`add_zone`, `add_mcp_socket`/`remove_mcp_server`, `add_decider_socket`/
  `remove_decider`, `config`/`with_config`/`change`/`check_config`/`add_config_check`,
  `record_writable`, `layers_for`, `resolve`, `socket_dir`/`state_dir`/`startup_path`.
  Most already have HTTP routes; any that don't need one added.
- **In-process couplings to replace before the split:**
  - `add_rule_keeper` / `RuleKeeper` — **done** (retired; the office follows `rule` events,
    see the §5(a) section above).
  - `named_layer()` returns a shared `Arc<RwLock<Policy>>` the office still mutates for its
    *own* add/remove (`run.roles` are the core's named-layer Arcs). Removal via the core is
    done; the office's adds (`security.rs:952`) and its own revokes (`security.rs` →
    `remove_rule`) must become `add_layer_rule` / `remove_layer_rule` API calls so the
    office stops sharing the Arc. **This is now the main remaining coupling.**
  - `on_shutdown` — the office manages its own lifecycle in its process (§8).
  - `test_session` — test-only; the office's tests move to the API-over-socket pattern (§10).
- Then `zapper-office` drops `zapper-core` from its production deps (dev-only, as the
  workbench has it), `zapper` stops linking the office, and `OFFICE=1` means "also run
  `zapper-office`". Validate with `make test-e2e-office` on a KVM host.

### The `OfficeClient` abstraction — started

Decided (you approved: a trait with two implementations). Built on this branch:

- `trait OfficeClient` in `office/src/client.rs`, with its operations so far: `add_layer_rule`
  / `remove_layer_rule` (backed by new `Core` methods that emit `rule` events) and
  `record_writable` (the foreman's turn gate; `Local → Core`, `Remote → GET /v1/record`). The
  foreman no longer holds a `Core`.
- `Local(Core)` — in-process, delegates to `Core` (used now).
- `Remote(Zapper)` — over the API socket (`POST /v1/rules` with a role scope, `DELETE
  /v1/rules/{id}`); the office's sync firm logic blocks on the binary's multi-threaded
  runtime. Not yet exercised (the binary doesn't run firms until the rest of the surface
  moves onto the trait), but it compiles and the layer ops work over the socket.
- `FirmRun` holds `Arc<dyn OfficeClient>` instead of the shared role-layer `Arc`s
  (`run.roles` is gone); standing approvals and revocation go through `run.client`. The
  office no longer shares policy memory with the core for role layers.

**Note — reference cycle:** `Local` holds a `Core` (→ `Arc<Daemon>`), and the daemon reaches
`FirmRun` (`Daemon → Office → runs → FirmRun → client → Core → Daemon`). Benign for the
long-lived daemon; per-test it leaks a `Daemon` (tests are short-lived). The `Remote` path
has no cycle (it holds a socket path). If the in-process leak ever matters, give `Local` a
`Weak` core handle.

**Remaining, in two buckets:**

1. *Handle-free* methods (small, like the three done): config reads, and the id-returning
   registrations (`add_service`/`add_zone`, `add_mcp_socket`/`remove_mcp_server`,
   `add_decider_socket`/`remove_decider`, config checks). These extend the trait the same way
   but are called from `facility.rs`/`ops.rs` with a `&Core`, so each needs the client
   threaded to its site.
2. *The session-handle rework* (the bulk, **e2e-gated, attended**): `create_session`,
   `session`, `stop_session`, `forget_session` return `Arc<Session>`, which the foreman,
   terminals and driver hold throughout. These can't cross a socket; the office must move to
   **session ids + API calls** (and the driver crate, §6, over `attach`). This is the large,
   behaviour-sensitive change that touches worker driving and terminals — do it with
   `make test-e2e-office` on a KVM host, not unattended.

Then rehome `office()`'s 32 uses to the office's own state, drop `zapper-core` from the
office's prod deps, and wire the binary to run firms through `Remote`.

### Original recommendation (kept for context)

The conversion keeps wanting the office to reach the daemon from places that hold a
`FirmRun` (e.g. `security.rs:952` add, `:1128` revoke) but not a `&Core`. Two dead-ends
show the shape of the right answer:

- storing a `Core` in `FirmRun` fails `#[derive(Debug)]` (`Core` isn't `Debug`) and makes a
  `Daemon → Office → FirmRun → Core → Daemon` reference cycle; and
- threading `&Core` through `decide`/`revoke` and their callers is wide, churny, and still
  core-typed (no path to the separate process).

So the clean move — a **design decision to confirm** — is a narrow trait, say
`OfficeClient`, with the handful of daemon operations the office needs (create/stop/forget
session, add/remove a session rule, `add_layer_rule`/`remove_layer_rule`, services, MCP,
deciders, config, record), held by `FirmRun`/`Office` as `Arc<dyn OfficeClient>`. Two impls:
one wrapping `Core` (in-process, now) and one over the `client::Zapper` socket (the separate
process). Then the office's `&Core` sites convert to `self.client`/`run.client` calls one at
a time, each behaviour-preserving and unit-testable, and `office()`'s 32 uses rehome to the
office's own state. When the last `&Core` is gone, `zapper-core` leaves the prod deps.

This is the crux of step 4 and sets the abstraction everything else hangs off, so it's worth
deciding/reviewing attended rather than guessing its shape unattended. The core now has the
pieces it needs (`named_layer`, the decider socket, `record_writable`, rule events); the
`add_layer_rule`/`remove_layer_rule` methods are the small remaining core additions.

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
