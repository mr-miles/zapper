# office-process step 4: execution plan (office → its own process)

Splitting The Office out of the daemon into its own process, a client of zapper's API
(proposal 13, step 4). This is office↔core, **not** core↔sandbox: the VM boundary and guest
behaviour are untouched, so it is validated with `TestCore` + fakes (`test_session`,
`shell_vm`, `dial`), not KVM. Delete this file when step 4 lands.

Branch: `feat/office-process-step4` (on top of `feat/office-process`, merged to `overlay`).

## Locked decisions
- **D1 = A** — the office drives each worker over an `attach` terminal stream, running the
  `zapper-driver` crate in the office process; the daemon still owns VMs/proxy/policy/record.
- **D2 = a** — chunky, intent-level trait (`admit`, `start_worker_session`, `serve_facility`,
  `attach`, …), not thin per-op methods.
- **D3 = A** — full split: the office gets its own listener, `zapper-core` leaves its prod
  deps, the `zapper-office` binary runs firms, `OFFICE=1` means "also run `zapper-office`".
- **D4** — I validate with `TestCore`/fakes + office unit tests; **operator runs
  `make test-e2e-office` on a KVM host as the final pre-merge gate.**
- Defaults: `create_session` returns `SessionInfo{id,…}`, the office stores the **id** (not
  `Arc<Session>`); config *mutation* stays operator-side (office reads config, validates its
  own `firms`); a co-host office gets `socket_dir`/`state_dir` from config/convention.

## Validated design (keystones)
- `OfficeClient` trait (`office/src/client.rs`): `Local(Core)` in-process, `Remote(Zapper)`
  over the socket. Methods return socket-friendly data (ids/info), never `Arc<Session>`.
- **attach:** `driver::start` runs a conversation loop on a `UnixStream` from
  `Session::open_shell`. Local attach = that stream. Remote attach = the office's API
  connection, hyper-upgraded to a raw stream, which the daemon splices to the guest shell
  (the workbench already does hyper upgrades + terminal proxying — precedent). `driver::start`
  is generalised to any `AsyncRead+AsyncWrite` so both back it (task 1.1).
- **Chatty→chunky (from the review):** collapse always-together calls into intent methods —
  `admit(role)` (= config + session_count + memory room), `start_worker_session(spec)`
  (= create + journal + store + reapply), `serve_facility(...)` (RAII: bind socket + serve
  loop + register, `Drop` = unregister + abort + unlink), and an http extractor for the ~24
  `d.office().active(firm)?` handler preambles.

## ORDERING FINDING (revise the phases)

The clean, standalone chunky wins are **done** (config, admit, record_writable, role-layer
ops, generic `driver::start`). Converting the *rest* of Phase 2 is **not** cleanly ahead of
Phase 3/5 — the dependencies interleave:

- The **registration leaves** (`security`/`ops`/`facility` register) place a socket in the
  `state/services` dir — that path is **plain office config** (`FirmRun.socket_dir`, assumed
  aligned with the daemon; P1-a), not coordinated. Each also needs a `Remote` API route (P3).
- The **logic/plumbing files** (`contracts`, `access`, `scenario`, most of `workforce`) barely
  touch `d` directly — they plumb it down to the **session ops** (`create_session`,
  `stop_session`, …) which are the real **session-handle rework**.

So the file conversions are downstream of two prerequisites. **Revised order:** do them first
— **(i)** `socket_dir` as config on `FirmRun` (P1-a, done/decided), **(ii)** the session
surface on the trait (`create_session→id`, `stop`, `forget`, `attach`) with the `WorkerRt`
id rework (P1-b/c) — *then* the registration + logic-file conversions fall out cleanly (P2),
*then* flesh `Remote` + routes (P3), *then* the listener relocation + split (P4). The session
surface is the real load-bearing work; the file conversions are downstream of it.

## STATUS (update on each commit)
- **P0 foundation** + **standalone chunky wins**: done. `OfficeClient` (`Local`/`Remote`) with
  `add_layer_rule`/`remove_layer_rule`, `record_writable`, `config`, `admit`; `driver::start`
  generic; role layers via the core; `Office` holds the client. (`39d6026`…`f871fb4`.)
- Ordering corrected (above): the rest is **not** Phase-2-first. Load-bearing prerequisites
  come next (P1), then the file conversions fall out (P2), then `Remote`+routes (P3), then
  the listener/split (P4).
- **P1-a** decided: `socket_dir` is plain office config (carried on `FirmRun`, assumed
  aligned with the daemon — no client method).
- **P1-b** done: session surface on the trait — `create_session -> SessionInfo`, `stop_session`,
  `forget_session`, `session -> SessionInfo`, `attach -> Box<dyn DriveStream>`. `Local` impls
  them on `Core`; `Remote` is stubbed `unimplemented!("…P3-a")` (not wired into any running path
  yet). The async methods return `BoxFuture` so the trait stays object-safe (`Arc<dyn>`).
  `DriveStream` (combined `AsyncRead+AsyncWrite+Unpin+Send`, blanket impl) lives in
  `zapper-driver`, re-exported as `zapper_core::driver::DriveStream`, modelled on the daemon's
  `services::Io`. `SessionInfo` grew public `id()`/`label()`/`profile()` accessors (its fields
  are `pub(crate)` to the core). Covered by `client::tests::local_session_surface` (fakes).
- **Next task: P1-c** (the `WorkerRt`/foreman `attach` driving rework — the care point).

Anchors: `WorkerRt.session: Arc<Session>` at `office/src/lib.rs:142`; `driver::start` at
`driver/src/process.rs:107`; foreman driving at `office/src/foreman.rs:72`
(`open_shell().stream` → `driver::start`); office client at `office/src/client.rs`; the 24
handlers in `office/src/http.rs`; `Remote` struct at `office/src/client.rs:67`.

---

## Phase 0 — foundation (DONE)
- [x] `zapper-office` bin scaffold + `client::Zapper` (API-socket client).
- [x] `RuleKeeper` retired; core owns role layers; office follows `rule` events.
- [x] `OfficeClient` trait + `Local`/`Remote`; `add_layer_rule`/`remove_layer_rule`,
      `record_writable`, `config`, `session_count`; `room` converted; `Office` holds the client.

## Standalone chunky wins (DONE, part of P0)
- [x] **1.1** `driver::start<S: AsyncRead+AsyncWrite+Unpin+Send>` (box `Proc.stdin`) — unblocks
      `attach` for both impls (`9bbe94c`).
- [x] **1.4** `admit(profile) -> Config` (core `has_room`: limit + memory in one call);
      `workforce::room` calls it (`f871fb4`).
- [x] `config` on the trait; role layers via the core; `Office` holds the client.

These were the only conversions independent of the two prerequisites below. The rest reorders
per the finding.

## P1 — load-bearing prerequisites (lead / sequential; fake-tested, no KVM)
Do these first; the file conversions (P2) are downstream of them.
- [x] **P1-a** `socket_dir` is **plain office config**, not a client method. The office and
      daemon are configured to the same `state/services` path — assume aligned, no
      coordination. Carry it on `FirmRun` (set at run creation: `d.socket_dir()` in-process,
      `office.yaml` for the separate bin). The registration leaves (P2-a) read `run.socket_dir`.
      (Decided; an earlier attempt to put it on the trait was backed out.)
- [x] **P1-b** Session surface on the trait (**async** methods — called from the foreman's
      async loop): `create_session(NewSession) -> SessionInfo`, `stop_session(id)`,
      `forget_session(id)`, `session(id) -> SessionInfo`, and `attach(id, ShellRequest) ->
      Result<Box<dyn DriveStream>>` (on 1.1's generic `start`; `DriveStream` = a combined
      `AsyncRead+AsyncWrite+Unpin+Send` trait in `zapper-driver` with a blanket impl). `Local`
      only; `Remote` stubbed for P3. Async methods return `BoxFuture` (no `async-trait` dep; the
      trait is held as `Arc<dyn>`). `SessionInfo` gained `id()`/`label()`/`profile()`.
- [ ] **P1-c** The session-handle rework — **the care point** (where fakes can hide a real
      driving bug; do with fresh focus). `WorkerRt.session: Arc<Session>` → id + `SessionInfo`;
      the foreman drives via `run.client.attach(id, req)` + `driver::start`; `foreman::start`
      no longer needs an `Arc<Session>`.

## P2 — downstream conversions (unblocked by P1; parallelisable — see map)
Drop the `(d, run)` pair; use `run.client`. Grow the trait (`resolve`, `layers_for`,
`subscribe`, `add_session_rule`, `remove_rule`) as a site needs it; `Local` now, `Remote` in P3.
- [ ] **P2-a** Registration leaves → `run.client`: `security`/`ops`/`facility` register +
      their `unregister`s. Fold the bind + serve-loop + register + cleanup boilerplate into a
      `serve_facility(...) -> FacilityGuard` (RAII: `Drop` = unregister + abort + unlink).
      Needs P1-a.
- [ ] **P2-b** Logic/plumbing files → drop `d`: `contracts`, `access`, `scenario`,
      `workforce`, `facility` logic. These bottom out in session ops, so need P1-b/c.
- [ ] **P2-c** http handlers: an axum extractor resolving `Path<firm>` → `Arc<FirmRun>`, so the
      ~24 handlers drop the `State(d)` + `office().active(f)?` preamble. (Cross-cutting — lead.)
- [ ] **P2-d** `run.rs` run management (`up_with`/`boot`/`down`/`ticker`) + worker creation →
      office state + client.

## P3 — `Remote` impls + API routes
- [ ] **P3-a** Flesh `Remote` for every trait method: hyper-upgrade for `attach`; GET/POST for
      sessions / `admit` / registrations / `resolve`; SSE for `subscribe`.
- [ ] **P3-b** Add the missing `zapper-api` routes (`/v1/admit`, `/v1/deciders`, open-shell /
      attach upgrade, …) the `Remote` methods call.

## P4 — own listener + the split (D3 = A)
- [ ] **P4-a** Relocate `http.rs` to the `zapper-office` binary's listener; extract the
      workbench's gateway/auth (sign-in, cookie, `Host`/`Origin`) into a crate both use (§7).
- [ ] **P4-b** The office holds its own `Office` state (its registry), not `d.office()`;
      rehome the 32 `office()` uses; the bin configures its state/socket dirs.
- [ ] **P4-c** Drop `zapper-core` from the office's prod deps (dev-only, as the workbench).
- [ ] **P4-d** Wire the bin to run firms; `zapper` stops linking the office; `OFFICE=1` runs
      `zapper-office`.
- [ ] **P4-e** Operator: `make test-e2e-office` on a KVM host — the final gate.

## Parallelism & subagents
Single crate (compiles as a whole) and the hard work is the P1 prerequisites, so parallelism
helps only in the P2 middle and P3 `Remote` groups. Isolate parallel work in git **worktrees**
(`isolation: "worktree"`), disjoint files, merged by the lead.

- **Lead / sequential (design-bearing or cross-cutting):** all of **P1** (esp. P1-c, the
  driving rework — the care point), **P2-c** (the http extractor), **P4** (the structural
  split). The trait surface is the lead's; subagents must **not** add trait methods — flag a
  gap and stop.
- **Fan-out after P1 is locked (mechanical, disjoint files):** **P2-a** (registration leaves)
  and **P2-b** per file (`contracts`, `access`, `scenario`, `workforce`) — one worktree
  subagent each. Brief: "drop `(d: &Core, run)`; use `run.client`; keep `cargo test -p
  zapper-office` green; don't touch `OfficeClient` — report a missing method." A **fork**
  (inherits this context) suits these better than a fresh agent.
- **Fan-out after P3-b routes exist:** **P3-a** `Remote` by method-group (sessions / `admit` /
  registrations / `resolve` / events).
- **Caveats:** worktrees multiply build cost; merges stay on disjoint files; anything touching
  `client.rs`, `http.rs`, or the foreman is lead-only. Net ~2–4× fan-out on P2-b/P3, not on the
  P1 prerequisites — spawn only once the trait is locked, else subagents thrash on a moving
  trait.
