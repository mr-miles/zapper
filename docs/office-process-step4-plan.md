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

- The **registration leaves** (`security`/`ops`/`facility` register) need the co-host office
  to place a socket in the **daemon's** `state/services` dir → they need `socket_dir` as
  office config (a Phase-5 bin concern), and each needs a `Remote` API route (Phase 4).
- The **logic/plumbing files** (`contracts`, `access`, `scenario`, most of `workforce`) barely
  touch `d` directly — they plumb it down to the **session ops** (`create_session`,
  `stop_session`, …) which are the **Phase-3 session-handle rework**.

So Phase 2 Local-first in isolation accumulates `Remote` stubs + `socket_dir` placeholders
that only resolve in Phase 3/5. **Revised order:** do the prerequisites first —
**(i)** `socket_dir` as office config (small slice of Phase 5), **(ii)** the Phase-3 session
surface on the trait (`create_session→id`, `stop`, `forget`, `attach`) with the `WorkerRt`
id rework — *then* the registration + logic-file conversions fall out cleanly, *then* flesh
`Remote` + routes (Phase 4), *then* the listener relocation (rest of Phase 5). Sessions and
`socket_dir` are the real load-bearing work; the file conversions are downstream of them.

## STATUS (update on each commit)
- Phase 0 (foundation): **done** — see commits `39d6026`…`d76a7aa`.
- 1.1 (`driver::start` generic): **done** (`9bbe94c`).
- 1.4 (`admit`): **done** — `workforce::room` is now one `client.admit(profile)` call
  (session-limit + memory on the daemon side); `session_count`/office memory pre-check gone.
- 1.5 (`serve_facility`): folded into Phase 2 (the `ops`/`security` register trios convert
  there; the RAII helper is office-internal polish, not a trait-shape change).
- **Next: convert Phase 2 files sequentially, growing the trait per file** (fan-out needs a
  locked complete trait, which emerges from conversion, so sequential is lower-risk here).
- **1.2 (`attach`) is deferred into 3.2:** its shape (the `ShellRequest`, pulling the
  conversation/driver into the office) is only clear when wiring the foreman; defining it
  abstractly now would be revised there. 1.1 is the part that had to come first.
- 1.3 (session-by-id) is entangled with Phase 3 (session storage) — do alongside 3.1.

Anchors: `WorkerRt.session: Arc<Session>` at `office/src/lib.rs:142`; `driver::start` at
`driver/src/process.rs:107` (write half `Proc.stdin: OwnedWriteHalf` at :45); foreman driving
at `office/src/foreman.rs:72` (`open_shell().stream` → `driver::start`); office client at
`office/src/client.rs`; the 24 handlers in `office/src/http.rs`.

---

## Phase 0 — foundation (DONE)
- [x] `zapper-office` bin scaffold + `client::Zapper` (API-socket client).
- [x] `RuleKeeper` retired; core owns role layers; office follows `rule` events.
- [x] `OfficeClient` trait + `Local`/`Remote`; `add_layer_rule`/`remove_layer_rule`,
      `record_writable`, `config`, `session_count`; `room` converted; `Office` holds the client.

## Phase 1 — chunky trait surface + `Local` impls
Each task: add to the trait + `Local`, convert the office call site(s), `cargo test -p
zapper-office` green, commit. `Remote` for these is Phase 4.
- [x] **1.1** Generalise `driver::start` to `start<S: AsyncRead+AsyncWrite+Unpin+Send+'static>`
      (use `tokio::io::split`; box `Proc.stdin` as `Box<dyn AsyncWrite+Unpin+Send>`). Done;
      driver/core/office build, driver tests + clippy green.
- [ ] **1.2** `OfficeClient::attach(session_id, ShellRequest) -> Result<impl stream>`; `Local`
      = `session(id)?.open_shell(req).stream`. (No office call site yet; exercised in 3.2.)
- [ ] **1.3** Session-by-id: `create_session(NewSession) -> SessionInfo`, `stop_session(id)`,
      `forget_session(id)`, `session(id) -> SessionInfo`. `Local` delegates to `Core`.
- [x] **1.4** `admit(profile) -> Result<Config>` (core `has_room`: session-limit + memory in
      one call); `workforce::room` now calls it. Done; office 147 + core session green.
- [ ] **1.5** Registrations + `serve_facility(name, sessions, handler) -> FacilityGuard`
      (RAII). Convert `facility::register` (service+zone), `ops::register` (mcp socket),
      `security::register` (decider socket) + their `unregister`s.
- [ ] **1.6** Remaining reads: `resolve(profile)`, `layers_for(...)`, `subscribe()` events,
      `add_session_rule(id, rule)`, `remove_rule(id)`.

## Phase 2 — convert office logic off `&Core`
Drop the `(d, run)` pair; use `run.client`. One file per task; green + commit each.
- [ ] **2.1** `contracts.rs` (offer/decline/withdraw/end/finish/tick).
- [ ] **2.2** `access.rs` (enforce/reapply/owner_access/revoke_all/tick).
- [ ] **2.3** `security.rs` (appoint; register/unregister already socket-based).
- [ ] **2.4** `scenario.rs` (play/tick), `workforce.rs` (hire), `facility.rs` logic.
- [ ] **2.5** http handlers: an axum extractor resolving `Path<firm>` → `Arc<FirmRun>`, so the
      ~24 handlers drop the `State(d)` + `office().active(f)?` preamble.
- [ ] **2.6** `run.rs` run management (`up_with`/`boot`/`down`/`ticker`) + creation.

## Phase 3 — session-handle rework
- [ ] **3.1** `WorkerRt.session: Arc<Session>` → `session: Mutex<Option<SessionInfo>>` (id +
      info). Update readers (`w.session()`), `start_worker_session` stores the id.
- [ ] **3.2** Foreman drives via `run.client.attach(id, req)` + `driver::start` (uses 1.1/1.2);
      `foreman::start` no longer needs an `Arc<Session>`.
- [ ] **3.3** Other session-handle uses → by id over the client (drive file copies via a
      `system` terminal, human/board terminal views).

## Phase 4 — `Remote` impls + API routes
- [ ] **4.1** Flesh out `Remote` for every Phase-1 method (hyper-upgrade for `attach`; GET/POST
      for sessions/admit/registrations/resolve/layers_for; SSE for `subscribe`).
- [ ] **4.2** Add any missing API routes in `zapper-api` the `Remote` methods call.

## Parallelism & subagents

It's a single crate (compiles as a whole) and the hard phases are design-bearing, so
parallelism helps only in the wide mechanical middle. Isolate parallel work in git
**worktrees** (`isolation: "worktree"`), each on disjoint files, merged by the lead.

**One hand / sequential (lead does these — design-bearing or risky):**
- 1.1 (`driver::start`), **all of Phase 1** (the trait is the keystone; one coherent shape),
  2.5 (the http extractor — cross-cutting), **Phase 3** (driving path — the one place fakes
  can hide a real bug), **Phase 5** (structural split). The trait surface is owned by the
  lead; subagents must not add trait methods (flag a gap instead — avoids conflicts).

**Parallelisable after Phase 1 is fixed — mechanical, disjoint files, mergeable:**
- Phase 2 logic conversions: **2.1 `contracts.rs`, 2.2 `access.rs`, 2.3 `security.rs`,
  2.4 `scenario.rs`/`workforce.rs`/`facility.rs`, 2.6 `run.rs`** — one worktree subagent per
  file/cluster. Brief: "drop the `(d: &Core, run)` pair; use `run.client` for daemon ops;
  keep `cargo test -p zapper-office` green; do **not** change `OfficeClient` — if a method is
  missing, stop and report it." A **fork** (inherits this context/pattern) suits these better
  than a fresh agent.

**Parallelisable after the API routes exist (4.2):**
- Phase 4 `Remote` impls by method-group — sessions / `admit` / registrations / `resolve` /
  events — each a worktree subagent writing the `Remote` method + exercising its route.

**Caveats:** worktrees multiply build cost (each recompiles deps); merges must stay on
disjoint files; anything touching `client.rs` (the trait), `http.rs` (2.5), or the foreman
(Phase 3) is lead-only. Net: parallelism is a ~2–4× fan-out on the Phase-2/4 middle, not on
the hard ends — spawn it only when Phase 1 is locked, else the subagents thrash on a moving
trait.

## Phase 5 — own listener + the split (D3 = A)
- [ ] **5.1** Relocate `http.rs` to the `zapper-office` binary's listener; extract the
      workbench's gateway/auth (sign-in, cookie, `Host`/`Origin`) into a crate both use (§7).
- [ ] **5.2** The office holds its own `Office` state (its own registry), not `d.office()`;
      rehome the 32 `office()` uses.
- [ ] **5.3** Drop `zapper-core` from the office's prod deps (dev-only, as the workbench).
- [ ] **5.4** Wire the bin to run firms; `zapper` stops linking the office; `OFFICE=1` runs
      `zapper-office`.
- [ ] **5.5** Operator: `make test-e2e-office` on a KVM host — the final gate.
