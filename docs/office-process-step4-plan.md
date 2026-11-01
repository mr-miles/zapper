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

## STATUS (update on each commit)
- Phase 0 (foundation): **done** — see commits `39d6026`…`d76a7aa`.
- **Next task: 1.1** (generalise `driver::start`).
- Nothing else started.

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
- [ ] **1.1** Generalise `driver::start` to `start<S: AsyncRead+AsyncWrite+Unpin+Send+'static>`
      (use `tokio::io::split`; box `Proc.stdin` as `Box<dyn AsyncWrite+Unpin+Send>`). Validate:
      `cargo test -p zapper-driver`.
- [ ] **1.2** `OfficeClient::attach(session_id, ShellRequest) -> Result<impl stream>`; `Local`
      = `session(id)?.open_shell(req).stream`. (No office call site yet; exercised in 3.2.)
- [ ] **1.3** Session-by-id: `create_session(NewSession) -> SessionInfo`, `stop_session(id)`,
      `forget_session(id)`, `session(id) -> SessionInfo`. `Local` delegates to `Core`.
- [ ] **1.4** `admit(role) -> Result<Config>` (groups config + session_count + memory room);
      replace `workforce::room`. Validate: `room`/hire tests.
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

## Phase 5 — own listener + the split (D3 = A)
- [ ] **5.1** Relocate `http.rs` to the `zapper-office` binary's listener; extract the
      workbench's gateway/auth (sign-in, cookie, `Host`/`Origin`) into a crate both use (§7).
- [ ] **5.2** The office holds its own `Office` state (its own registry), not `d.office()`;
      rehome the 32 `office()` uses.
- [ ] **5.3** Drop `zapper-core` from the office's prod deps (dev-only, as the workbench).
- [ ] **5.4** Wire the bin to run firms; `zapper` stops linking the office; `OFFICE=1` runs
      `zapper-office`.
- [ ] **5.5** Operator: `make test-e2e-office` on a KVM host — the final gate.
