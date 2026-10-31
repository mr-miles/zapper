# The office as a process of its own

Status: part 1 is built (`tidy/office-api`, below); of part 2, §7 is decided (the office's own listener) and the rest is for discussion.

The office moves out of the daemon into `zapper-office`, an executable that is a client of zapper's API on zapper's socket in exactly the way the [workbench](12-single-user-layer.md#part-2-the-workbench) is (built on `feat/workbench`): it depends on the API and on `zapper-proto` and `zapper-policy`, never on `zapper-core`. It replaces [firm separation](../firm-separation.md#later-if-wanted-a-separate-process)'s "later, if wanted" section, which predates the workbench and assumed one process.

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

Today `Decider::ask` is called in-process when a rule asks: the request is refused at once with a `Referral` (where it went, what to do meanwhile), and the officers decide later, the decision coming back as a rule on the session (`deciders.rs`). Over the socket:

- **(a) Referrals as events.** A client registers a decider for a label selector (`POST /v1/deciders {sessions, to}`); a matching `ask` is refused at once with a referral the core makes (its id, `to`, a fixed "then"), and a `referral` event goes on the stream. The client decides in its own time and answers with rules through the API. Simple, and resumable through the event cursor; but the "then" text, which the office words per request today, becomes fixed or a template.
- **(b) A decider socket**, as for MCP servers: the core asks the client's socket synchronously, within a short timeout, for the referral; on no answer, the request is refused as a denial. Keeps the office's wording; one more socket protocol.
- **Either way:** `final` rules stay above every decider; and when the decider's client is gone, its sessions' `ask`s are refused as denials (not sent to the operator, which would bypass the officers), until it registers again or the registration is removed.

### 5. Role policy: shared layers, or rules per session

Today each role's policy is a layer the office holds in memory and shares with every worker of that role (`Scope::Role`): an officer's approval for the role applies to all its workers at once, and `zapper rules` lists it as `role:engineer` through `RuleKeeper`.

- **(a) Named shared layers in the core.** `PUT /v1/layers/{name}` (rules, validated as a config change), sessions created with `layers: [name]`, rules added to and removed from a layer through the existing rule routes with `scope: "layer:NAME"`. The core owns them, so listing, removal and `rule` events (§2) cover them, and `RuleKeeper` goes. Closest to today; a new scope in evaluation and in the config's views.
- **(b) Rules per session.** The office writes a role's rules into each worker's own layer, and again into every worker of the role when they change. Nothing new in the core; but a role change is N writes that aren't atomic together, and `zapper rules` shows N copies.
- **(c) A profile per role.** Role rules in the config's profiles, changed by config changes. Atomic and visible, but every officer's approval becomes a config change, with its diff and its save.

### 6. Driving workers

Today the foreman opens a terminal in the worker's VM and runs the core's driver on it (`driver::start`, `foreman.rs:73-80`); it keeps when to wake an agent and the budgets. Prompts reach a worker only through it: the operator's `zapper tell` is refused for a firm's sessions.

- **(a) The driver's code in a crate of its own**, depending on `zapper-proto` only (the harnesses' conversation code and the process driver), used by the core for driven sessions and by the office over `attach`, whose frames are the same as `open_shell`'s. The office keeps its foreman as it is. Who may prompt a worker is then who holds its terminal: the office.
- **(b) The core drives, the office prompts.** Workers are created `driven`, the office prompts through `POST /v1/sessions/{id}/prompt` and reads turns from events. The core's driver needs an **owner** in the API: a session created with a driver claim, whose prompts need the claim's token, so that nothing reaches a worker around the firm's post office and budgets. Less code in the office; a capability in the API that nothing else has.

### 7. The office's UI, CLI and config

*Decided: its own listener.* The second option below, behind the workbench, is set aside. Its config and its CLI are still open.

- **Its own listener.** The office serves its pages and the board's writes itself, behind its own guard: the workbench's gateway code (sign-in, cookie, `Host`, `Origin`, the UI's header), moved into a crate both use. AGENTS.md's list of the UI's writes splits into zapper's and the office's.
- **Or behind the workbench**, as a facility the workbench proxies: one sign-in for both, but the office then needs the workbench to run.
- **`zapper firm …`** moves to `zapper-office firm …` (or the `zapper` CLI finds the office's socket), and `zapper-inspect` stays as it is, reading runs from disk.
- **Config.** The `firms:` section moves to the office's own file (`~/.config/zapper/office.yaml`, or beside zapper's in a checkout), parsed and validated by the office; or stays in zapper's, which the core stores without reading and hands to the office through the API. The first is simpler; the second keeps one `config diff` and one `config save`.

### 8. Two lifecycles

Today both stop together, and a run ends with the daemon ([firm separation: restarts](../firm-separation.md#restarts)). Apart:

- **The daemon after the office goes.** Its services, MCP servers and deciders point at a process that isn't there, and its workers run on. Registrations could be **leases** held by a connection the client keeps open (gone with it), or stay until removed, with what they do while the client is away stated (a socket service refuses, a decider's sessions are refused, §4). Workers: stopped by the daemon when the office's lease ends, or left for the office to find again.
- **The office after the daemon restarts.** Workers aren't persistent sessions (they share layers today, which persistent sessions can't), so they end with the daemon, and the run with them, as now. Resuming a run is the office's own, later work; §5(a) or (b) would let workers be persistent.
- **The office restarting alone.** With leases, a restart ends the run; without them, the office could find its sessions again by their labels and its registrations by name.

### 9. The record

AGENTS.md: "the daemon takes no firm turns while the record can't be written". The office would check the record's state through `/v1/record` before each turn, and write its own journal through the keeper as it does today. The invariant moves from the daemon to the office, worded so.

### 10. Tests

Many of the office's tests run it in-process over a `TestCore` (`test_session`, `shell_vm`, `mcp`, `dial`). They move to the workbench's pattern: zapper's real API on a socket over a `TestCore`, the office as a client of it. The largest part of the work, and mostly mechanical; it can go module by module, behind part 1 and before the office's process starts.

## Steps

1. Part 1, in any order: socket MCP servers, rule events, session creation. Each its own branch, the office moved onto it at once.
2. Decide §4–§8. Each decision gets its own step, building what it needs in the core, with the office moved onto it in-process.
3. The office's tests onto the API (§10), module by module.
4. `zapper-office` as its own executable: its listener and pages (§7), its config, its lifecycle (§8), the record check (§9); `zapper` stops linking it, and `OFFICE` only decides what `make install` installs.

Every step needs a KVM host for `make test-e2e-office`.

## Open questions

- §4: referrals as events (a), or a decider socket (b)?
- §5: named shared layers in the core (a), rules per session (b), or profiles per role (c)?
- §6: the driver as a crate the office uses (a), or the core's driver with an owner in the API (b)?
- §7: its config in its own file, or in zapper's? (Its own listener is decided.)
- §8: registrations as leases, or kept until removed? Are workers stopped when the office goes?
