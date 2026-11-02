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
- [x] **P4-c** Drop `zapper-core` from the office's prod deps (dev-only, as the workbench).
      **Done:** the co-host adapter is `#[cfg(test)]` — `client::Local(Core)` + its impl + the
      trait imports; `lib`'s `OfficeOf` trait/impl, `start`, `follow_rules` and the `prelude`
      import; and the helpers only they reach (`history::close_orphans`,
      `security::forget_standing`, and their `Journal` import). `zapper-core` is now in
      `[dev-dependencies]` (features `test-util`); the office's unit tests drive it against a
      `TestCore`. Production reaches the daemon through `client::Remote` and the serializable
      API types alone. The prior detail is kept below for reference.
      **Done — the relocation out of `zapper-core` (each a green commit, core re-exports so the
      CLI/api are unchanged, the office repointed off core):**
      - new bottom crate **`zapper-api-types`** holds the API DTOs: `Status`, `SessionInfo`,
        `NewSession`, and a serializable `ServiceSpec`/`ServiceTarget` twin (core keeps its
        internal `services::Target` with the in-process `Handler` variant + `From` conversions),
        and `ApiError` + the `errors` module (`ErrorKind`/`kind_of`; the crate gains an `axum`
        dep — the error IS an HTTP response). Deps: policy + proto + serde + axum, never core.
      - new bottom crate **`zapper-util`** holds the generic host helpers (`random_id`, `now`,
        `write_private`, `host_memory_mib`, `exposed_warning`, `ID_CHARS`); deps rand + anyhow.
      - `redact`/`REDACTED` → **`zapper-policy`** (pure config work).
      - the driving stack (`driver::{start, DriveStream, …}`, `harness`, `read_frame`) — the
        office depends on **`zapper-driver`** directly (it already held them; core only re-exported).
      - new crate **`zapper-record`** (the keeper-backed record: `view`/`verify_state_dir`/`Writer`).
      - new crate **`zapper-mint`** (the host-command token cache: `resolve`/`peek`/`forget`/
        `secret_value`; tokio+policy, no core). The office mints its own github tokens for its own
        git calls; the daemon mints for secret injection; each process keeps its own `GLOBAL`.
      - `OfficeClient::remove_service` returns `()` (dropped the last `ServiceInfo` use).
      **Left (the office's ONLY remaining prod `zapper_core` use is the co-host adapter):**
      gate `#[cfg(test)]` — in `client.rs`: `Local(Core)` + its impl + the trait imports it needs
      (`ApprovalApi`/`RuleApi`/`ServiceApi`/`SessionApi`/`ConfigApi`); in `lib.rs`: `use
      prelude::*`, `use {Core, EventBody}`, `start`, `follow_rules`, the `OfficeOf` trait + `impl
      OfficeOf for Core`. (`facility.rs`/`run.rs` core uses are already `#[cfg(test)]`; `routes`/
      `PAGE`/`ASSETS` stay prod — `standalone` uses them.) Then move `zapper-core` to
      `[dev-dependencies]` in `crates/zapper-office/Cargo.toml` (keep `features = ["test-util"]`).
      This can only land together with P4-d-3 (gating `start`/`Local` test-only breaks `zapper`'s
      in-process office), so do them in one go.
- [~] **P4-d** Wire the bin to run firms; `zapper` stops linking the office; `OFFICE=1` runs
      `zapper-office`.
      **P4-d-1 done:** the last two stubbed `Remote` methods are real — `reset_layer` clears a
      role layer through a new daemon route (`DELETE /v1/layers/{name}` → `clear_layer`), and
      `promote_rule` posts to the existing `/v1/rules` (which assigns the `net-N` id and inserts
      at the profile scope; the answer's extra `scope` key is stripped before the rule
      deserializes). Both are exercised end-to-end in `remote_over_the_socket`, so `Remote` now
      has no `unimplemented!`.
      **P4-d-2 done (serve path):** new `office::standalone` builds the `Office` over a `Remote`
      client (`bootstrap`) and serves the office page, its assets and the firm routes (nested at
      `/api`, where the page's scripts call them) behind a UI gateway with a token of its own
      (`zapper_client::gateway` for the `Host`/`Origin`/marker checks). The bin's `serve` binds
      the listener and calls it with the daemon's `state_dir`/`socket_dir` (CLI args; shared by
      convention, P1-a). Because office↔core is the daemon's API alone — no VM boundary — the
      whole serve path is unit-tested without KVM: `standalone_app_serves_behind_the_gateway`
      drives the real `zapper-api` router (TestCore) over a unix socket (gateway 401/redirect/403,
      a firm read over `Remote`, a proxied daemon read, asset serving).
      **P4-d-2 leftovers done (follow-up commit):**
      - **Rule-following** — `standalone::follow_rules` follows the daemon's `/v1/events` SSE and
        drops a firm's standing approval when its role-layer rule is removed through the daemon,
        through the same `forget_removed_role_rule` helper the co-host `follow_rules` now uses
        (shared prod logic; `security::forget_standing` un-gated). Reconnects resuming after the
        last event (`?after=EPOCH:SEQ`) so a mid-run operator edit is not missed.
      - **Daemon-backed reads** — `standalone::proxy_routes` proxies the reads the office page
        needs but the office does not serve (`/sessions`, `/sessions/{id}/terminals`,
        `/sessions/{id}/requests`) to the daemon over the socket, nested at `/api` behind the
        gateway alongside `routes()`. Reads only.
      **Deep links — done** (2026-11-02): the office serves `GET /daemon-ui` (the daemon UI's
      URL with its token, from `state/ui-url`); the page's `daemonUrl()` builds the terminal and
      sessions links against the daemon's origin, and the daemon UI's token redirect now preserves
      the rest of the query, so `/terminal?session=…&n=…&token=…` and `/sessions?firm=…&token=…`
      land authenticated on the daemon UI's own pages.
- [x] **P4-d-3** The office becomes a standalone process the **user** runs (`zapper-office
      serve`, like `zapper-workbench`); `zapper serve` does **not** start/spawn it. **Done** (three
      green commits: firm-CLI move → remove office from `zapper` → P4-c gating + core dev-dep);
      `make lint && make test` pass. **The CLI-transport open item is now resolved** (follow-up
      commit, operator-approved): the office serves its firm routes on its **own unix socket**
      (`standalone::office_socket_path` — `$ZAPPER_OFFICE_SOCKET`, else `office.sock` beside the
      daemon's `api.sock` under `$XDG_RUNTIME_DIR`), bound in `standalone::serve` alongside the
      TCP listener, serving the bare `routes()` with **no gateway** — a local socket gated by
      filesystem permissions, exactly as the daemon serves the `zapper` CLI (the browser gateway —
      token, `Host`/`Origin`, the UI marker — is for the page alone). `cli::Client` now connects
      there and the firm CLI calls `/firms/…` (was the daemon mount's `/v1/firms/…`). Two new unit
      tests lock it in (`cli_routes_need_no_gateway`, `office_socket_path_honours_the_env_override`).
      Still deferred to P4-e: runtime validation on KVM. Still open from P4-d-2 (unrelated to the
      CLI): SSE rule-following and proxying the page's daemon-backed session/VM reads.
      **Locked decisions (operator):** (1) the `zapper firm/mail/drive` CLI moves to the
      `zapper-office` binary — `zapper` drops those subcommands and stops linking `zapper-office`
      entirely; (2) firm config is the office's own — the daemon stops validating `firms:`; (3)
      `OFFICE`/the cargo feature governs only whether the `zapper-office` binary is
      **built/included**, no runtime coupling. Concrete checklist:
      - **Move the firm CLI:** `crates/zapper/src/firm_cli.rs` (1518 lines; uses only
        `crate::client::Client` + `crate::util::now`, **no `zapper_office`**) → the `zapper-office`
        crate, adapted to the office's client (`zapper_office::client::Zapper`/`zapper_client`)
        and `zapper_util::now`; wire `Firm`/`Mail`/`Drive` subcommands into `zapper-office`'s
        `main.rs` `Cli`.
      - **Remove the office from `zapper`:** in `crates/zapper/src/main.rs` drop `office::start(&d)`
        and `office::mount(&d)` from `serve` (the daemon no longer serves the office UI/routes),
        drop `office::config`/`office::check` firm validation, and the `Firm/Mail/Drive` `Cmd`s;
        delete/gut `crates/zapper/src/office.rs` and `firm_cli.rs`; remove the `office` feature and
        the `zapper-office` dep from `crates/zapper/Cargo.toml`.
      - **Finish P4-c (couples here):** gate the office's co-host adapter `#[cfg(test)]` (see P4-c
        “Left”), move `zapper-core` to the office's `[dev-dependencies]`.
      - Validate with `make lint && make test` (office↔core is API-only — no KVM needed for this).
      **All four done.** Notes on what the move touched beyond the literal list:
      - `cli.rs` holds the office's own small daemon-socket `Client` (mirrors `zapper_client::Zapper`
        but keeps raw bytes for a drive download) and the `print_json`/`s`/`table` helpers +
        `daemon_state_dir` (for `firm replay`'s `ui-url` lookup) that lived in `zapper`.
      - `zapper record verify <firm> <run>` (the `#[cfg(feature=office)]` variant) was dropped from
        `zapper` when the office left it. **Now re-added** (operator-approved) as `zapper-office firm
        verify <firm> [run]` (default run `current`): GETs the office's `run_verify` route and
        renders it as `zapper record verify` did (per-file ok/CHANGED, a summary line, exit 0/1).
      - `zapper init` no longer installs the office workers' agents/skills. **Now re-added** as
        `zapper-office init`: resolves the config dir (`cli::config_dir`, same `$ZAPPER_CONFIG` /
        checkout `config/` / `~/.config/zapper` resolution the daemon uses) and calls
        `skills::install` beside it, exactly as `zapper init`'s example template did.
      - The firm example-config tests (`the_*_example_loads`) moved from `zapper`'s `main.rs` to
        `zapper-office/src/examples_tests.rs`.
      - **Still a follow-up:** user-facing docs say `zapper firm/mail/drive` and `zapper init`
        installing skills (AGENTS.md, `skills/zapper/SKILL.md`, `docs/commands.md`, `docs/office.md`).
        The CLI transport is now settled, so the `zapper … → zapper-office …` doc sweep is unblocked
        — not done here (out of the approved 1/2/3 scope).
- [ ] **P4-e** Operator: `make test-e2e-office` on a KVM host — the final gate.

## Architecture review (crate structure)

A review of the whole workspace's crate responsibilities, dependency tree, API chattiness, exposed internals and duplication. The dependency diagram and verdict live in `docs/crate-dependencies.md` (the tree is a clean, acyclic, well-layered DAG; the items below are refinements). Changes A–D and the `api-types → proto` edge removal are **done** (`make lint && make test` green); E is **designed and deferred**.

- [x] **A — one socket client, not two.** `zapper_client::Zapper` gained `call_raw` (raw body + content-type → status + bytes); the office's firm-CLI `cli::Client` is now a thin adapter over `Zapper` instead of a second hand-rolled unix-socket/hyper client.
- [x] **B — shared CLI formatters.** `print_json`/`table`/`s` moved to `zapper_util::cli`, used by both the `zapper` and `zapper-office` command lines. Config-path resolution stays per-binary on purpose (needs `zapper-policy` + does I/O + encodes zapper's layout, so fits neither `zapper-util`'s "no zapper-crate deps" nor `zapper-policy`'s "no I/O").
- [x] **C — tighter office surface.** `zapper-office` no longer exposes `pub mod firm`/`pub mod client` (nothing external used them). Surfaced that `firm::check` is prod-dead now the daemon doesn't validate firms — gated `#[cfg(test)]`, to be re-wired by E.
- [x] **D — group the N+1 session reads.** `GET /v1/sessions?terminals=true` embeds terminals (one call for the VMs view, opt-in/additive — the daemon UI is unchanged); the office serves `GET /firms/{f}/requests` aggregating each worker's captured requests (one firm-scoped call for the officers' traffic view). The office's per-session `/terminals` and `/requests` proxies are gone.
- [x] **`zapper-api-types → zapper-proto` edge removed.** api-types only used proto types via `zapper_policy::proto::*` (policy re-exports proto); the direct edge was vestigial.
- [x] **E — firms are the office's own config — DONE** (2026-11-02, branch `feat/office-firms-config`; full writeup in [office-firms-config-plan.md](office-firms-config-plan.md)). `firms` removed from `zapper_policy::Config`; the office reads them from `config/office.yaml`, validates against the daemon's profiles, and reloads with `zapper-office config reload`; `zapper-office init` writes a starter `office.yaml`; the four examples split into `zapper.yaml`+`office.yaml`. The original design, for reference — completing decision 2 meant:
  - New **`config/office.yaml`** holding `firms:`, read by an office-owned loader; the office reads firms from it, not from the daemon.
  - Remove `firms` from `zapper_policy::Config` (it is `deny_unknown_fields`, so this also removes the `firms:` section from `config/zapper.yaml` and the `EXAMPLE` template, and `zapper init` stops writing it).
  - `profiles:` stays the daemon's — the office still reads it via `config()` (profiles define the VMs the daemon enforces); narrow the office's use of `config()` to profiles/daemon fields.
  - Un-gate `firm::check` (C) and have the office validate its own firms against the daemon's profiles on load (a standalone validator; today only the test-gated co-host `start` wired it in).
  - **The reason it is deferred:** every office test builds a firm by feeding `TestCore` one YAML that mixes `profiles:` and `firms:`, so `test_core` and ~18 test files need reworking to feed firms to the office separately — a sizeable, delicate change best done as its own focused piece, not folded into this review. It also splits the operator's single config file in two, which has a live-reload story to settle (firm-config reload becomes the office's concern, not the daemon's `zapper config reload`).

### Continuation note (context may be cleared mid-step)
Branch `feat/office-process-step4` (worktree `~/zapper/zapper-structure`); nothing pushed. New
crates: `zapper-api-types`, `zapper-util`, `zapper-record`, `zapper-mint`. **P4-c and P4-d-3 are
done** — three green commits (`office-process P4-d-3: move the firm/mail/drive CLI into
zapper-office`; `… remove the office from the zapper binary`; `office-process P4-c: gate the
co-host adapter test-only; zapper-core dev-only`). `make lint && make test` pass. The office is no
longer built into `zapper`; `zapper-office` carries the firm CLI and runs the firms. A follow-up
commit (operator-approved) then settled the CLI transport — the office serves the firm routes on
its own unix socket for `zapper-office firm/mail/drive`, with no gateway — and re-added
`zapper-office firm verify` and `zapper-office init`. A further commit settled P4-d-2's two leftovers — SSE rule-following
(`standalone::follow_rules`) and proxying the page's daemon-backed reads
(`standalone::proxy_routes`). `make lint && make test` still pass. **Next:**
**P4-e** (operator's KVM `make test-e2e-office`); the `zapper … → zapper-office …` doc sweep
(now unblocked); and the small UI follow-up for the page's daemon-UI deep links.
**Disk gotcha:** `target/debug` fills the shared volume (~30G); when a build fails with "No space
left on device", run `rm -rf target/debug` (release/`make lint` artifacts are separate) and rebuild.

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
