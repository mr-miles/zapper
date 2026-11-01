# office↔daemon API review (post-P4-b)

A review of the office↔daemon boundary now that the firm logic is fully off `Core` and reaches
the daemon only through `OfficeClient` (`office/src/client.rs`). Scope the user asked for: what
crosses the boundary and where; always-together calls; data-shaping that only matched old
shapes; objects/steps to simplify or delete; and which facilities (git, mail, drive, tools,
decider, cluster) can be standardised. Grounded with `file:line`. Delete this file once acted on.

## A. The boundary surface

`OfficeClient` is ~22 methods. Everything that crosses is socket-friendly by construction —
ids, `SessionInfo`, `Config`, `AnyRule`, `ServiceSpec`/ids, `NetRequest`/decision, and the raw
`attach`/`connect_port` streams. No `Arc<Session>`, no `Core`. That part of the design holds up.
Groups: **rules** (`add_layer_rule`/`remove_layer_rule`/`put_session_rule`/`remove_session_rule`/
`allowed`), **sessions** (`create_session`/`stop_session`/`forget_session`/`session`/`attach`/
`connect_port`/`requests`), **registration** (`add_mcp_socket`/`add_decider_socket`/`add_service`/
`add_zone` + their removes), **misc** (`record_writable`, `config`, `admit`).

## B. Always-together calls → collapse

1. **`stop_session` + `forget_session` are *always* paired** — every call site stops then
   immediately forgets: `facility.rs:913-914`, `run.rs:717-718`, `run.rs:916-917`,
   `workforce.rs:1516-1517` (and `Local` `client.rs:169/174`). Collapse into **one
   `stop_session(id)` that also forgets** (or a `discard_session(id)`), removing a boundary
   method and four paired call sites. Note `stop_session`'s `reason` is **already dropped over
   the socket** (`client.rs` Remote: "the route takes none") — so the reason arg is in-process-
   only; drop it or carry it on a query if it matters.
2. **register + bind + serve + cleanup** is already folded into `serve_facility` +
   `FacilityGuard` (`client.rs`). Good — keep.

## C. Data-shaping that only matches old shapes

3. **`Remote::create_session` hand-flattens `NewSession` → the API's `CreateSession` JSON**
   (`client.rs`: profile/workdir/`rules`-from-`policy`/labels/files/…). The office builds a
   `NewSession` only because `Local` calls `Core::create_session(NewSession)`; `Remote` then
   re-shapes it field-by-field to the API DTO. Unify: either have the office build the API's
   request shape directly, or make the API accept `NewSession`. One shape, no hand-map.
4. **`SessionInfo` (20+ fields) crosses on `create_session`/`session`**, but the office reads
   only `id()`/`user()`/`label()`/`profile()`. Not wrong, but a documented "the office uses
   these four" (or a slimmer return) would make the boundary honest.

## D. Objects/steps to delete or simplify

5. **`OfficeClient::config()` has no callers** — superseded by `admit` (which returns the config:
   `workforce.rs:1315`). **Delete it** (+ the `Local`/`Remote` impls).
6. **Dead return values**: `add_layer_rule -> AnyRule` (ignored at `security.rs:950`) and
   `remove_service -> Option<ServiceInfo>` (ignored at every site: `facility.rs:909`,
   the guard `Drop`, tests). Make both `()`.
7. **`run_of(&Office, &Session)` is now `#[cfg(test)]`-only** (`lib.rs`) — its production use
   (ops) moved to the `McpCall` labels. Delete it and inline the firm/run-label check in the one
   test that still calls it.
8. **Six parallel registration methods** — `add_mcp_socket`/`add_decider_socket` +
   `remove_mcp_server`/`remove_decider` (and `add_service`/`remove_service`). The RAII guard
   already keys teardown off a `Registered { Mcp, Decider, Service }` enum (`client.rs`). The two
   *socket* registrations could collapse to one `register_socket(kind, name, sessions, socket) ->
   id` + `remove(kind, id)`, shrinking the boundary. (`add_service` also takes `Session`/`Addr`
   targets, so keep it; it is the general one.)

## E. Facilities — what can be standardised (and what cannot)

**Finding: "git, mail, drive" are three different kinds, not one.**
- **git** is a daemon-routed **socket service** (a TLS `ServiceTarget::Socket` + a domain zone;
  `facility.rs`).
- **tools** (MCP) and the **decider** (ask) are daemon-routed **socket deciders/servers**
  (`ops.rs`, `security.rs`).
- **cluster** is a set of **session-target services** (`facility.rs register_cluster`).
- **mail (`PostOffice`) and drive (`Drive`) are office-internal state**, exposed only through the
  MCP **tools** — they register nothing with the daemon and serve no socket (`mail.rs:199`,
  `drive.rs:77`). They are *not* facilities in the daemon-service sense; a "facility" abstraction
  should **not** be forced onto them.

**The real duplication is three near-identical socket serve-loops**, one per socket facility:
`ops::serve` (reads an `McpCall`, JSON reply), `security::serve` (reads an `AskCall`, JSON reply
for `Ask`), `facility::serve_git` (reads a `ServiceCaller`, then speaks HTTP). Each repeats:
std→tokio listener, `accept` loop, read+parse the daemon's caller line, spawn a per-connection
handler. **The workbench already has exactly this as `serve_socket(ctx, socket, handler)`**
(`workbench/src/facility.rs:239`) + `caller()`; the office reimplements it three times.

**Recommendation (folds into P4-a):** extract the workbench's `serve_socket`/`caller` and a
workbench-style **`Facility` trait** (name, the services it registers, its socket handler,
optional routes/panel) into the shared crate P4-a creates, and have both the workbench and the
office's socket facilities (tools, decider, git) use it. That deletes the three office serve-loops
and gives one facility model. Two caller sub-shapes to support: **JSON request→reply line**
(tools, decider) and **caller-line-then-raw-stream** (git) — a `serve_socket` that reads the
caller line and hands `(caller, stream)` to the handler covers both (the handler chooses JSON vs
HTTP). `cluster` and git's zone/service registration standardise via the existing
`add_service`/`add_zone` helpers (the `Facility::services()` list, as the workbench does it).

## F. Suggested order

Do **E** as part of **P4-a** (the crate extraction is the natural home for `serve_socket` + the
`Facility` trait, and it shrinks the office before relocating it). Do **B/D** (collapse
`stop`+`forget`, delete `config()`, dead returns, `run_of`) as small independent cleanups
anytime — they reduce the boundary before P4-c drops the dep. Keep **C** (DTO unification) in
mind for when `create_session`/`SessionInfo` are touched in P4-a/d.
