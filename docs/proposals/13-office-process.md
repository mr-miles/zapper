# The office as a process of its own

Status: part 1 is built (`tidy/office-api`, below), and the office runs on it in-process. Of part 2: §4 (a decider socket), §6 (the driver as a crate), §7 (its own listener and its own config file), and §8 (kept-until-removed, resuming by cursor) are decided; §5 is the last decision open.

The office moves out of the daemon into `zapper-office`, an executable that is a client of zapper's API on zapper's socket in exactly the way the [workbench](12-single-user-layer.md#part-2-the-workbench) is: it depends on the API and on `zapper-proto` and `zapper-policy`, never on `zapper-core`. It replaces [firm separation](../firm-separation.md#later-if-wanted-a-separate-process)'s "later, if wanted" section, which predates the workbench and assumed one process.

The workbench is no longer a plan to follow: it is built — its own executable (`zapper-workbench`), its own gateway that authenticates clients and calls zapper over the socket, a facilities interface, tokens and grants, SSE, the drive, git served as a socket service, and SSH. So the office is not charting a new pattern; it copies a working one, and §7's shared gateway is a concrete extraction, not a sketch.

Why:

- **Guest input out of the process with the secrets.** The office parses what workers send it (MCP tool calls, git pushes, drive files) inside the daemon, which holds every session's secrets. Out of it, a bug there reaches the office's own state, not the proxy's. It is what [proxy separation](../proxy-separation.md) does for the proxy, for the office's part.
- **Lifecycles of their own.** The office can crash, be restarted or upgraded without the daemon, and the daemon without the office.
- **zapper links no office code.** `OFFICE=1` becomes "also run `zapper-office`"; `zapper` is the same binary either way.

## Today: where the office meets the core

As of `feat/workbench`. The office is about 24,000 lines with its tests, and reaches the daemon through the `Core` interface, in Rust, in-process. Most of what it uses crosses a socket as it is; seven things don't.

| The office uses | Where | Over the socket |
|---|---|---|
| Sessions: create, stop, remove; labels; rules on a session; answering approvals; events, with resume; terminals, `system` ones among them (the drive copies files that way, `drive.rs:141`); registering services; the record's status | throughout | Works today |
| The firm's git server, an in-process service (`impl ServiceHandler for GitServer`) | `facility.rs:87` | Works today as a [socket service](12-single-user-layer.md#4-services-served-by-a-facility-outside-the-daemon), which names its caller in a first line |
| Its tools, an in-process MCP server (`impl McpServer for Tools`) | `ops.rs:32` | §1 below |
| Rule changes, read from `config` events' words, and its own layers' rules listed and removed through `RuleKeeper` | `lib.rs:717` | §2 (events); §5 (its layers) |
| Session creation with files, mounts, an agent overlay and forwarded ports | `facility.rs`, `run.rs` | §3 |
| Officers, an in-process `Decider`: a firm's `ask` refused at once, handed to them | `security.rs:1137` | §4 |
| Role policy as live shared layers (`shared_layers: Arc<RwLock<Policy>>`, `Scope::Role`), one per role, read by every worker of it | `run.rs:585`, `run.rs:816` | §5 |
| Driving workers: the foreman opens a terminal and runs the core's driver on it (`driver::start`) | `foreman.rs:73-80` | §6 |
| Its routes and page, mounted in zapper's API and UI behind zapper's guard; `zapper firm …`; the `firms:` config section | `zapper-api`, `firm_cli.rs`, `firm.rs` | §7 |

## Part 1: tidy-ups, each done on its own

Each is useful while the office stays in-process, and the office moves onto it straight away, so `make test-e2e-office` keeps passing. None waits for part 2.

### 1. MCP servers on a socket

*As built (`6dd5d47` on `tidy/office-api`):* as below, with an owner: `Core::add_mcp_socket` takes one, the API registers as the operator's, and `DELETE /v1/mcp/{id}` removes only the operator's (`409` for a client's). The request and reply are `zapper_proto::facility::{McpCall, McpReply}`; the facility has 10 s to list its tools and 120 s for a call. The office serves its tools on `state/services/office-<firm>.sock`, still in-process, and all its tests pass through it.

An MCP server registered through the API, served by a facility's own process: the office's tools, the workbench's later.

- **`POST /v1/mcp`** `{name, sessions, socket}` (and `DELETE /v1/mcp/{id}`), as `POST /v1/services` takes a socket service: `socket` names a file in the daemon's `state/services/` directory, never a path; `sessions` is a label selector; only the operator, on the API's socket, registers one.
- **The gateway stays the MCP endpoint.** It still answers `initialize`, keeps the line bounds and limits, and records each tool call as an `mcp` event, timed. A socket server is one more implementation of the `McpServer` trait inside the core: for each `tools/list` and `tools/call` it connects to the socket and sends one line, `{"session": "ID", "labels": {…}, "server": NAME, "method": "tools/list" | "tools/call", "tool"?, "arguments"?}`, then reads one reply line (`{"result": …}` or `{"error": "…"}`), within `MAX_MCP_LINE` and a timeout.
- **Who calls is the daemon's word.** `session` and `labels` come from the VM the guest's connection came from, as for a socket service's first line; nothing a guest sends sets them. The facility trusts them because only the daemon reaches its socket.
- **The office moves its tools onto it** while still in-process (it serves the socket from a task of its own), so the change is tested on its own before anything else moves.

Invariant, from AGENTS.md's "the sender of every office request is the VM it came from": a socket MCP server is told the caller by the daemon, from the VM the connection came from, and nothing in a message changes it. Tests: the caller named from the VM and not from the guest's arguments; an oversized reply refused; a server that doesn't answer timed out and recorded; `tools/list` per session.

### 2. Rule events

*As built (`fc93eb4`):* as below. The config's layers are compared before and after each change, so every path (`POST`/`DELETE /v1/rules`, `PUT /v1/config`, a profile changed, a reload) is covered; a rule changed under its id is removed, then added. A rule removed through a client's `RuleKeeper` is an event with that layer's scope. Rules a session is created with come with it, not as events. `config` events stay, in words, for the change itself.

Rule changes as events of their own, so a client keeps its records by following the stream instead of being called in-process.

- **A `rule` event** `{state: "added" | "removed", id, scope, kind}` (and `session` when the scope is a session's) for every rule added or removed, by whoever: a client, the operator's CLI or UI, an approval remembered, a config change. Today these are `config` events whose words a reader would have to parse (`rule {id} removed from session:…`, `daemon/session.rs:393`).
- `config` events stay for what isn't a rule.
- The office uses them for what it keeps about rules in the core's layers: an officer's standing approval revoked when its rule goes. `RuleKeeper`'s other job, rules in layers *the office* holds, is §5's.

Tests: each way a rule changes emits one `rule` event, with its scope; a resumed stream gets the ones it missed.

### 3. Session creation through the API

*As built (`488bfae`):* as below, except the checks: the in-process path barely checks these fields (it trusts its clients), so they are checked at the API, for what lets a session boot: guest paths absolute and without `..`, at most 64 files and 128 KiB of them (they go in the guest's first, bounded message), and no forward on the agent's own vsock ports.

`POST /v1/sessions` takes what `NewSession` has and a client needs: `files` (written in the guest), `mounts`, `agent_overlay` (merged over the profile's `agent`) and `forwards`, besides what it takes now (profile, workdir, rules, labels, persistent, ssh).

- Each is validated as the in-process path validates it: mounts' sources under the workdir, files' paths and sizes bounded, forwards' ports.
- `shared_layers` stays in-process until §5 decides what replaces it.
- The office creates workers through it while still in-process; so can the workbench's wizard.

Tests: each field reaches the session as the in-process path gives it; one out of bounds refused.

## Part 2: for discussion

### 4. Deciders: who decides a firm's `ask`

*Decided: (b), a decider socket.*

Today `Decider::ask` is called in-process when a rule asks: the request is refused at once with a `Referral` (where it went, what to do meanwhile), and the officers decide later, the decision coming back as a rule on the session (`deciders.rs`, `proxy.rs:156`). The referral the agent is shown — `access to {what} requested from {to} (request {id}); {then}` — is assembled by the core (`proxy.rs:130`) from the office's `Referral`. What the office puts there matters to the choice: `to` ("security") and `then` are fixed strings, but the **`id` (`r-N`) is office state** — its counter (`security.rs:224`), reused when a worker re-asks the same host (the merge path returns the same id, `security.rs:205`), and the handle the officers' decision, the mail and the request log all carry. Over the socket:

- **(b, chosen) A decider socket**, as for the §1 MCP servers: on an `ask` the core asks the office's socket synchronously, within a short timeout, for the referral; the office runs its merge, `MAX_PENDING` and officer-pick and returns `{id, to, then}`, so the real `r-N` is in the refusal and the request log at once, exactly as today. On no answer the request is denied, no referral. Chosen because the id is the office's to allocate at refusal time, and the socket pattern already exists (one more `state/services/*.sock`), so the synchronous call is short and unexotic.
- **(a) Referrals as events.** The core refuses with a referral it makes and emits a `referral` event; the office allocates `r-N`, merges and decides off the stream. Resumable by cursor and the cleanest separation (the core never touches an office id) — but then `r-N` can't appear in the agent's immediate refusal or the core's request log; it reaches the worker only by mail, and the merge-stable id isn't echoed on retries. Set aside: it changes what the agent sees for a separation the socket doesn't actually cost us.
- **Either way:** `final` rules stay above every decider; and when the decider's client is gone, its sessions' `ask`s are refused as denials (not sent to the operator, which would bypass the officers), until it registers again or the registration is removed.

### 5. Role policy: shared layers, or rules per session

Today each role's policy is a layer the office holds in memory and shares with every worker of that role (`Scope::Role`): an officer's approval for the role applies to all its workers at once, and `zapper rules` lists it as `role:engineer` through `RuleKeeper`.

- **(a) Named shared layers in the core.** `PUT /v1/layers/{name}` (rules, validated as a config change), sessions created with `layers: [name]`, rules added to and removed from a layer through the existing rule routes with `scope: "layer:NAME"`. The core owns them, so listing, removal and `rule` events (§2) cover them, and `RuleKeeper` goes. Closest to today; a new scope in evaluation and in the config's views.
- **(b) Rules per session.** The office writes a role's rules into each worker's own layer, and again into every worker of the role when they change. Nothing new in the core; but a role change is N writes that aren't atomic together, and `zapper rules` shows N copies.
- **(c) A profile per role.** Role rules in the config's profiles, changed by config changes. Atomic and visible, but every officer's approval becomes a config change, with its diff and its save.

### 6. Driving workers

*Decided: (a), the driver as a crate the office uses.* Who may prompt a worker is then who holds its terminal — the office — and the API grows no prompt capability.

Today the foreman opens a terminal in the worker's VM and runs the core's driver on it (`driver::start`, `foreman.rs:73-80`); it keeps when to wake an agent and the budgets. Prompts reach a worker only through it: the operator's `zapper tell` is refused for a firm's sessions.

- **(a, chosen) The driver's code in a crate of its own**, depending on `zapper-proto` only (the harnesses' conversation code and the process driver), used by the core for driven sessions and by the office over `attach`, whose frames are the same as `open_shell`'s. The office keeps its foreman as it is. Who may prompt a worker is then who holds its terminal: the office.
- **(b) The core drives, the office prompts.** Workers are created `driven`, the office prompts through `POST /v1/sessions/{id}/prompt` and reads turns from events. The core's driver needs an **owner** in the API: a session created with a driver claim, whose prompts need the claim's token, so that nothing reaches a worker around the firm's post office and budgets. Less code in the office; a capability in the API that nothing else has. *Set aside: it adds a prompt capability to the API that only the office would use, and keeps the driver in the core.*

### 7. The office's UI, CLI and config

*Decided: its own listener, and its own config file.* The alternative — the office behind the workbench, as a facility it proxies (one sign-in for both, but the office then needs the workbench to run) — is set aside.

- **Its own listener.** The office serves its pages and the board's writes itself, behind its own guard. This is no longer a plan to write but an extraction: the workbench's `gateway.rs` already owns remote security (token or the UI's cookie with `Host`, `Origin` and the header check), creates sessions with the token's labels and a directory of its own, scopes a rule to a session's own layer, never opens a `system` terminal, and filters lists, approvals and events to a token's sessions. The part both need — sign-in, cookie, the origin checks, the per-session filtering — moves into a crate both depend on; each keeps its own facilities and routes. AGENTS.md's list of the UI's writes splits into zapper's and the office's.
- **`zapper firm …`** moves to `zapper-office firm …` (or the `zapper` CLI finds the office's socket), and `zapper-inspect` stays as it is, reading runs from disk.
- **Config.** *Decided: the office's own file.* The `firms:` section moves to `~/.config/zapper/office.yaml` (or beside zapper's in a checkout), parsed and validated by the office — the workbench's own `~/.config/zapper-workbench` and `~/.local/share/zapper-workbench` are the precedent. The alternative (staying in zapper's config, the core storing it without reading and handing it to the office through the API) is set aside: it would keep one `config diff`/`config save`, but at the cost of the core carrying config it does not use, against the point of the split.

### 8. Two lifecycles

Today both stop together, and a run ends with the daemon ([firm separation: restarts](../firm-separation.md#restarts)). The `serve` shutdown that has since landed (`bae773f`) settles the mechanic this builds on: on stop the daemon stops sessions at once (each waits up to 5 s for its guest to power off, not one after another), gives requests under way 3 s, then returns and drops open streams; their clients resume from the next daemon by their cursors, and persistent sessions are left running. So a client that follows events and terminals already survives the daemon going and coming back by its cursor — the office is one such client.

On that footing the direction is **kept-until-removed, not leases**:

- **The daemon after the office goes.** Registrations (socket services, MCP servers, deciders) stay until removed, each with a stated behaviour while the client is away: a socket service and an MCP server refuse, a decider's sessions are refused as denials (never sent to the operator, §4). No lease to lose, so an office restart or upgrade doesn't tear the registrations down — the point of the split.
- **The office after a daemon restart.** It re-finds its sessions by their labels and its registrations by name, and resumes the event and terminal streams from its cursors, exactly as the shutdown leaves room for. Non-persistent workers still end with the daemon, and the run with them, as now; making workers persistent (so a run outlives a daemon restart) waits on §5(a) and is later work.
- **The one open sub-question:** the office's workers while the office is away. Left running, they idle (nothing drives them until the office returns); stopped by the daemon after the office's registrations have been gone for a grace period, they free their VMs but end the run. Leaning *left running with a grace stop*: the office usually comes back, and a short idle is cheaper than a lost run.

### 9. The record

AGENTS.md: "the daemon takes no firm turns while the record can't be written". The office would check the record's state through `/v1/record` before each turn, and write its own journal through the keeper as it does today. The invariant moves from the daemon to the office, worded so.

### 10. Tests

Many of the office's tests run it in-process over a `TestCore` (`test_session`, `shell_vm`, `mcp`, `dial`). They move to the workbench's pattern: zapper's real API on a socket over a `TestCore`, the office as a client of it. The largest part of the work, and mostly mechanical; it can go module by module, behind part 1 and before the office's process starts.

## Steps

1. Part 1, in any order: socket MCP servers, rule events, session creation. Each its own branch, the office moved onto it at once.
2. Decide §5 (§4 and §6–§8 are decided). Each decision gets its own step, building what it needs in the core, with the office moved onto it in-process.
3. The office's tests onto the API (§10), module by module.
4. `zapper-office` as its own executable: its listener and pages (§7), its config, its lifecycle (§8), the record check (§9); `zapper` stops linking it, and `OFFICE` only decides what `make install` installs.

Every step needs a KVM host for `make test-e2e-office`.

## Open questions

One is open:

- §5: named shared layers in the core (a), rules per session (b), or profiles per role (c)?

Decided, left here for the record:

- §4: a decider socket (b) — the core asks the office's socket synchronously for the referral, so the office keeps the `r-N` id and its merge/officer-pick, reached over one more `state/services/*.sock`.
- §6: the driver as a crate the office uses (a) — who holds a worker's terminal prompts it, and the API grows no prompt capability.
- §7: the office's own listener (extracted from the workbench's gateway), and the `firms:` config in the office's own `office.yaml`.
- §8: kept-until-removed, not leases, with the office resuming by cursor (the `serve` shutdown made this the cheap path). The lone remainder is whether an away office's workers are left idling or grace-stopped — leaning left running.
