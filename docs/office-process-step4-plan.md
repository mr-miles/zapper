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
- **P1-c** done: the foreman drives over `run.client.attach(id, req)` and holds no
  `Arc<Session>` — `foreman::start` takes the session id + harness kind from
  `WorkerRt::driven()`, `driver_of`/`use zapper_core::Session` are gone. The `Arc<Session>`
  field stays on `WorkerRt` as the transitional store the other readers (facility/security/
  drive) still use; `driven()` derives id+kind from it until P2-b migrates those onto the
  client, at which point the `Arc` is replaced by stored id+info. Foreman tests (full turns
  over `shell_vm`) pass through the client now.
- **P1 complete.**
- **P2-a** done for the socket facilities: `FirmRun.socket_dir` field + a `serve_facility`
  RAII guard (`client.rs`); `ops` (tools MCP socket) and `security` (officers' decider socket)
  register through `run.client`. Trait grew `add_mcp_socket`/`remove_mcp_server`/
  `add_decider_socket`/`remove_decider` (Local real, Remote stubbed for P3-a). Facility's own
  zone/git/cluster registration is a different shape and rides with facility logic in P2-b.
- **P2-b (facility services + guest commands)** done: zone/service registration
  (`add_service`/`add_zone`/`remove_service`) and `facility::stop` on `run.client`; facility
  guest commands over `run.client.run_command` (built on `attach`).
- **P2-c (started)**: `Active` extractor; 8 single-`{f}` firm handlers shed the preamble.
- **Git on a socket** (resolved the P3 handler-serialize finding): the office serves git on a
  socket like its tools/decider (`serve_git` + `serve_facility`, a TLS `Socket` service), so
  `add_service` carries only serializable targets and a standalone office serves git unchanged.
- **P3 complete**: every `Remote` method is implemented over the socket (session CRUD, service/
  mcp/decider/zone registration, and `attach` via a hyper upgrade that's frame-transparent
  through `pump_shell`); P3-b added the `/v1/deciders` and `/v1/zones` API routes. `Remote` is
  not exercised end-to-end until the P4 binary — the serde round-trip and the API routes are the
  testable parts; the socket transport is covered by the KVM office e2e.
- **Next: P4** — own listener + the split (relocate `http.rs`, the office holds its own `Office`
  state not `d.office()`, drop the `zapper-core` prod dep, `OFFICE=1` runs `zapper-office`),
  ending at the operator's `make test-e2e-office` KVM gate. Also still open: the transitional
  `Arc<Session>` removal (P4) and the remaining P2 `d`-plumbing it gates.

### P2-b FINDING — the rest of "drop `d`" is gated on guest commands / the `Arc<Session>`
The cleanly Arc-independent conversions are done (services). What's left divides by what it
actually needs from `d`:
- **Session *boot*** (`facility::start`'s `d.create_session` for the cluster; `boot_worker`'s
  for workers) keeps `d` **by design, for now**: it populates the transitional `Arc<Session>`
  store (`facility.cluster`, `WorkerRt.session`) that P1-c deliberately kept. These drop `d`
  only when that Arc is removed — the **P4** end-state (the office holds no `Session`).
- **Guest commands** (`facility::sh`/`drive::guest_command` → `start`'s k3s/kubectl deploys,
  `cluster_access`'s RBAC + kubeconfig write) and **session reads** (`security`'s `s.requests()`,
  `s.remove_rule()`) run **on the `Arc<Session>`**, not on `d`. To take them off the Arc the
  trait needs new methods — a **`run_command(id, argv, stdin, cap)` guest-command runner**
  (streaming today; would simplify to buffered bytes), plus `requests(id)` /
  `remove_session_rule(id, rule)`. `access::cluster_access`/`scenario` etc. that *plumb* `d`
  into these are gated on them.
- So **most remaining P2-b "drop `d`" is coupled to either `run_command` (a sizeable new
  streaming trait method) or the P4 Arc-removal** — it is not the clean mechanical fan-out the
  earlier plan assumed. **Decision needed** (see below) before continuing: add `run_command`
  now and push the guest-command migration through, or defer the Arc-bound paths to P4 and take
  P2-b only as far as the Arc-independent conversions (e.g. workforce's `stop_session`/
  `forget_session` by id, already on the trait) allow.
- **P2-c** (the http extractor) is independent of all this and is a clean lead task.

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
- [x] **P1-c** The session-handle rework — **the care point**. The foreman drives via
      `run.client.attach(id, req)` + `driver::start` and holds no `Arc<Session>`:
      `foreman::start` takes the session id + harness kind (from `WorkerRt::driven()`), not a
      `Session`; `driver_of` is gone (`harness::headless(kind)` inline). The `Arc<Session>`
      field stays on `WorkerRt` as the transitional store for the facility/security/drive
      readers (so `driven()` derives id+kind from it); those move onto the client in P2-b, after
      which the field becomes stored id+info and the `Arc` goes. Driving correctness is covered
      by the existing `foreman_tests` (full turns over `shell_vm`, now through the client).

## P2 — downstream conversions (unblocked by P1; parallelisable — see map)
Drop the `(d, run)` pair; use `run.client`. Grow the trait (`resolve`, `layers_for`,
`subscribe`, `add_session_rule`, `remove_rule`) as a site needs it; `Local` now, `Remote` in P3.
- [x] **P2-a** Registration leaves → `run.client` (the two **socket** facilities). Added
      `FirmRun.socket_dir` (P1-a; `d.socket_dir()` at run creation) and a
      `serve_facility(...) -> FacilityGuard` RAII helper (bind + serve-loop + register +
      cleanup; `Drop` = remove registration + abort task + unlink). `ops::register` (the tools'
      MCP socket) and `security::register` (the officers' decider socket) now register through
      `run.client` (`add_mcp_socket`/`add_decider_socket` + `remove_*` on the trait, Local real
      / Remote stubbed) and bind under `run.socket_dir`; their `Served`/`DeciderServed` structs
      and hand-rolled teardown are gone, `unregister` just drops the guard. `ops::register`
      keeps `&Core` only to build the in-process `Tools` serve loop (rehomed in P4).
      **Facility's own registration** (the domain zone + git-service handler, and the cluster's
      session-target services) is **not** this shape — it tracks service ids and bottoms out in
      session ops (cluster stop/forget) — so it moves with the facility logic in **P2-b**.
- [~] **P2-b** Logic/plumbing files → drop `d`. **Done:** facility service registration (P2-a
      note) and facility **guest commands** (`sh`/`cluster_access`/deploy over
      `run.client.run_command`, built on `attach` — the cluster is a sandbox terminal, not an
      agent). **Left:** session *boot* keeps `d` until the transitional `Arc<Session>` goes
      (P4); the `access`/`contracts`/`workforce`/`scenario` `d`-plumbing drop is coupled to the
      http handlers (P2-c) that pass `d`, and to the Arc removal.
- [~] **P2-c** http handlers: an `Active` axum extractor resolving `{f}` → `Arc<FirmRun>`
      (running firm), so handlers drop the `State(d)` + `office().active(f)?` preamble. **Done:**
      the 8 single-`{f}` `active` write handlers (`down`, `pause`, `send`, `upload`,
      `credentials_state`/`_grant`, `board_fund`/`_release`); those not needing the core also
      dropped `State(d)`. **Left:** multi-path-param handlers (keep `Path((f,id))` to avoid
      double-`Path` extraction) and the `office().run(&f)` (not-necessarily-running) handlers,
      which want a sibling extractor.
- [ ] **P2-d** `run.rs` run management (`up_with`/`boot`/`down`/`ticker`) + worker creation →
      office state + client.

## P3 — `Remote` impls + API routes
- [x] **P3-a** `Remote` fleshed for **every** trait method (no stubs). Session CRUD:
      `create_session` (POST `/v1/sessions`, mapping `NewSession`→the API's `CreateSession`),
      `stop_session` (POST `…/stop`), `forget_session` (DELETE), `session` (GET) — needing
      **`SessionInfo: Deserialize`** (+ `Status`) so `Remote` can read responses, covered by a
      JSON round-trip in `client::tests`. Registration: `add_service`→`/v1/services`,
      `remove_service`→DELETE, `add_mcp_socket`→`/v1/mcp`, `remove_mcp_server`→DELETE,
      `add_decider_socket`/`remove_decider`→`/v1/deciders`, `add_zone`→`/v1/zones`. `attach`:
      GET `…/attach` hyper-upgraded to a raw stream (new `Zapper::upgrade`), frame-transparent
      through the daemon's `pump_shell`, so `driver::start` drives over it unchanged. The async
      methods `.await`; the sync ones use `block`. **Not unit-tested end-to-end** (no live socket
      until the P4 binary); the serde round-trip + the new API routes are the testable parts.
- [x] **P3-b** Added the `zapper-api` routes the registration `Remote` methods call: `POST
      /v1/deciders` + `DELETE /v1/deciders/{id}`, and `POST /v1/zones`, each as the operator's.
      Covered by `tests::zones_and_socket_deciders_added_and_removed`.
- [x] **P3 verified without KVM**: `client::tests::remote_over_the_socket` serves the real
      `zapper-api` router on a unix socket (as the daemon does, via `axum::serve`) backed by a
      `TestCore`, and drives `Remote` end to end — `session`, `add_zone`/`add_service` (Socket
      target)/`add_mcp_socket`/`add_decider_socket` + their removes, and `attach` (the hyper
      upgrade reaching the guest shell via `shell_vm`). Only `create_session` is left to the KVM
      e2e (it boots a VM at the daemon). `zapper-api` is an office dev-dep (no cycle; it mounts
      the office's routes, never depends on it).

### P3 FINDING — RESOLVED: git now served on a socket
The gap was that `add_service` carried an in-process `Arc<dyn ServiceHandler>` (the firm's git
server), which cannot cross the socket. **Fixed:** the office now serves git on a socket, like
its tools and decider — `serve_git` reads the daemon's caller line (the workbench's
`serve_socket` pattern) and answers the guest's HTTP/1.1; it is registered as a TLS
`ServiceTarget::Socket` via `serve_facility` (a `Registered::Service` guard, dropped in
`facility::stop`). The `GitServer` handler is gone. So `add_service` now carries only
serializable targets and `Remote::add_service` works; a standalone office (P4) serves git with
no code change.

## P4 — own listener + the split (D3 = A)

### P4 PROGRESS — `Session`-method decoupling (the real prerequisite for dropping the core dep)
All verifiable without KVM (office↔core only). Done so far:
- **Remote verified** over a real socket (`remote_over_the_socket`).
- **Firm logic off `Session` methods**: security (`requests`/rules), facility
  (`network_follows_grant`), and drive (guest commands) all go through `run.client` by session
  id (new trait methods `requests`/`put_session_rule`/`remove_session_rule`; drive's
  `guest_command` runs over `attach`+`pump_command`, still streaming).

**The office now holds no `Session` in production.** Done:
- ops MCP tools run from the `McpCall` + `run.client` (no `d.session()`/`&Session`).
- `connect_port` on the client (`facility::forward_port`, `product`) + the `/v1/sessions/{id}/
  connect` route — the last `Session` *method* is gone.
- **`Arc<Session>` removed**: `WorkerRt.session`/`facility.cluster` store a `SessionInfo` (+ a
  worker's harness kind); boot/teardown go through `run.client` by id. A `#[cfg(test)]`
  `WorkerRt.session_arc` keeps the real `Session` for the test helpers only. `facility::start`
  and `down` dropped `&Core`; the http `down` handler dropped `State(d)`.

The only `Session` use left in office prod code is **`client::Local`** — the in-process adapter
that wraps `Core`; it becomes dev-only when the core dep is dropped (P4-c).

**Remaining (the structural split — `c.d`/`d.office()` is the live thread):**
- **`c.d` cascade** — the ops `Call` still holds `&Core`, fanning out to
  `workforce::hire`/`contracts::offer`/`access`/`layers_for`, bottoming out at **`d.office()`**.
  So **P4-b (office holds its own `Office`)** is the keystone that lets `c.d` become `run.client`.
- Then **P4-a** (relocate `http.rs` + shared gateway crate), **P4-c** (drop the `zapper-core`
  prod dep; `Local` becomes dev-only), **P4-d** (`OFFICE=1`).

- [~] **P4-a** The shared crate is extracted — **`zapper-client`** holds the socket `Zapper`
      client, `serve_socket` (the accept + read-the-daemon's-first-line loop the office's three
      socket facilities shared), and `gateway` (the `Host`/`Origin`/cookie auth, parameterised by
      the UI marker). The workbench and the office both use the client + `serve_socket`; the
      workbench uses the gateway auth and the office will when it serves its own listener.
      **Office-state swap done:** the firm routes (`http.rs`) now take `State<Arc<Office>>`, not
      `State<Core>` — the `Active` extractor and every handler reach firms/runs/config through the
      office (`run`/`active`/`firm_dir`/`client().config()`); `promote`'s running-config mutation
      moved behind a new `OfficeClient::promote_rule` (Local changes the daemon's config; Remote
      stubbed for P4-d). `routes()` takes `Arc<Office>`; the co-host mount passes
      `OfficeOf::office(d)` (both now `pub` so `zapper` can build the office state). The only
      `Core` the firm routes still name is `record::view`/`verify_state_dir` for reading run files
      off disk (sourced elsewhere in P4-c). **Left:** bind the router on the office's own listener
      — coupled to the bin (P4-d).
- [~] **P4-b** The office holds its own `Office` (`FirmRun.office()`, `Office.state_dir`); firm
      logic and the disk helpers reach the registry/run files without `Core`. Run management now
      off `Core` too: `create_run`, `up_with`, `booter` and `check_resources` take `&Arc<Office>`
      and use `run.client` (`session_count` added to the trait) — the http `up` handler passes
      `d.office()`, and only the `#[cfg(test)]` helpers still hold a `Core` (for `test_session`).
      **Left:** `boot`/`lib::start` and the http read endpoints still take `&Core` in-process —
      they move to the bin in P4-d.
- [ ] **P4-c** Drop `zapper-core` from the office's prod deps (dev-only, as the workbench).
- [~] **P4-d** Wire the bin to run firms; `zapper` stops linking the office; `OFFICE=1` runs
      `zapper-office`.
      **P4-d-1 done:** the last two stubbed `Remote` methods are real — `reset_layer` clears a
      role layer through a new daemon route (`DELETE /v1/layers/{name}` → `clear_layer`), and
      `promote_rule` posts to the existing `/v1/rules` (which assigns the `net-N` id and inserts
      at the profile scope; the answer's extra `scope` key is stripped before the rule
      deserializes). Both are exercised end-to-end in `remote_over_the_socket`, so `Remote` now
      has no `unimplemented!`. **Left:** the bin builds an `Office` with a `Remote` client (state/
      socket dirs from config, rule-follow over the API event stream, serve the firm routes +
      office page + gateway), then `zapper serve` spawns it under `OFFICE=1` instead of running
      the office in-process.
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
