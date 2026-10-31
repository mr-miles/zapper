[← README](../README.md) · [Docs](README.md) · **Firm separation (design)**

# Firm separation: The Office as a client of zapper's core

Status: proposal, for review. Nothing here is built yet.

## Goal

zapper's core creates and configures sandboxes, the MCP gateway, logging and metrics, and drives agents (the foreman). The Office provides the firm's capabilities (org chart, mail, drive, git, budgets, security officers, replay) and configures the core to use them, through an interface the core offers to any client, and nothing else.

Both stay in one process and one binary. The office moves into its own crate, `zapper-office`, which depends on the core's interface and can't reach anything else in the daemon: the compiler enforces the separation. A process boundary (a REST client with scoped tokens) stays possible later, but nothing here needs it.

Today the office is a module of the daemon. The `office` cargo feature can leave it out of the build, but the core still knows what a firm is: a session can belong to one, the proxy routes firm names and firm `ask`s, and the foreman is office code, so leaving it out needs a stand-in module (`office_off.rs`). After the split the core knows nothing about firms; anything a firm needs from it is a general feature, and the `office` feature only decides whether the binary links the office crate.

Non-goals: changing what a firm does, or how it looks to the board.

## Where zapper and The Office meet today

Every place the core calls into the office, and the general feature that replaces it:

| Today | Where | Replaced by |
|---|---|---|
| A session belongs to a firm (`FirmLink`: firm, member, role policy) | `daemon/session.rs` (`Session.firm`, `NewSession.firm`, `SessionInfo.firm/member`) | [Labels](#1-the-core-interface-and-labels) |
| A firm role's policy layer in a session's layers | `session.rs` (`layers()`), `api.rs` (rule listing) | Rules on the session, set by the office through the interface, or a profile per role |
| The office port: one JSON request per tool call, the sender is the VM | `session.rs` (vsock port 1100, `office::ops::serve`), `zapper-agent office` | [The office's tools in the gateway](#4-the-offices-tools-in-the-gateway) |
| Firm service names routed by the daemon (git, the cluster, apps) | `proxy.rs` (`Target::Firm`, `facility::route`, `facility::connect`), `nic.rs` (`facility::serves_name`) | [Services](#3-services) |
| A firm's `ask` refused at once and filed with the officers | `proxy.rs` (`Action::Ask if firm.is_some()`, `security::ask`, `security::used`) | [Approval routing](#5-approval-routing) |
| Deleting a rule revokes an officer's standing approval | `api.rs` (`delete_rule`, `security::revoke`) | The office hears rule events and keeps its records |
| The foreman: one task per worker, driving Claude Code or pi headless | `office/foreman.rs`, using `daemon/harness.rs` (`Driver`, `Conversation`) | [The driver](#6-the-driver) |
| The office page, its routes and the board's writes | `daemon/ui.rs` (`office.html`), `api.rs` (`office::http`) | The office's routers and pages, mounted by [`zapper-api`](#the-api-and-ui-server) in `main` |
| Firm commands, and firms stopped at daemon shutdown | `main.rs` (`firm`, `mail`, `drive`, `record verify`; `office::down`) | The office's commands, added by the binary with the `office` feature; the core tells clients it is stopping |
| Firm config in zapper's config | `zapper-policy/src/firm.rs`, `Config.firms` | The office's own section, parsed and validated by the office |
| Files, mounts and forwarded ports at session creation | `NewSession.files`, `.forwards`, `.agent_overlay` | Already general; part of the interface |

## Target architecture

```
  board, CLI ──▶ ┌──────────────────────────── zapper (one process) ─────────────────────────────┐
                 │  zapper-api: REST API (unix socket), UI server, guard, sessions pages          │
                 │        │ mounts the office's routers and pages                                 │
                 │        ▼                                                                       │
                 │  zapper-office: firms, org chart, mail, drive, git, budgets, officers, replay  │
                 │        │                                    │                                  │
                 │        ▼ the Core interface (Rust)          ▼                                  │
                 │  zapper-core: sessions · policy & approvals · proxy & secrets · MCP gateway    │
                 │        services · driver (the foreman) · events, metrics                       │
                 └────────────────────────────────────────────────────────────────────────────────┘
```

`zapper-api` reaches the core through the same interface as the office.

The core keeps no firm state; the office keeps all of it, in its own directory under the daemon's state, as today.

## Crates

| Crate | Kind | Holds | Depends on |
|---|---|---|---|
| `zapper-policy` | library (exists) | Config model, rule evaluation, host↔guest protocol; loses `firm.rs` | — |
| `zapper-core` | library (from `crates/zapper`) | The daemon: sessions, proxy, DNS, gateway, services, driver, events. No HTTP API. Its public API is the `Core` interface and its types; everything else is `pub(crate)` | `zapper-policy`, `zapper-keeper` |
| `zapper-api` | library (from `daemon/api.rs`, `ui.rs` and the pages) | The REST API on the unix socket, the UI server (listener, token, guard), the sessions pages and terminal viewer; mounts routers and pages it is given | `zapper-core` (its interface), axum |
| `zapper-office` | library (from `daemon/office/`) | Firms, mail, drive, git, budgets, officers, replay, the office's routers and pages, the firm config (`firm.rs` moves here) | `zapper-core` (its interface), `zapper-policy`, axum |
| `zapper` | binary | The CLI, `main`, the VMM shim; builds the core and `zapper-api` and, with the `office` feature, the office, handing its routers and pages to `zapper-api` | `zapper-core`, `zapper-api`, `zapper-office` (optional) |
| `zapper-inspect` | binary (exists) | Past runs, read from disk | `zapper-office`, for the run's types |
| `zapper-agent`, `zapper-keeper` | binary, library (exist) | Unchanged, but the agent's `office` relay goes | as now |

Making the core's internals crate-private is what keeps the office and `zapper-api` to the interface: depending on `zapper-core` gives them `Core` and nothing else. A separate crate of traits only (`zapper-core-api`, implemented by the core and wired in `main`) would be stricter, but adds a crate and boilerplate for no second implementation; not worth it now.

## What the core provides

Six features. Each is useful without a firm.

### 1. The core interface and labels

A `Core` handle the office (or any other client compiled in) holds, with the operations it needs and no access to the daemon's internals:

- Sessions: create (profile, extra policy, files, mounts, forwarded ports, agent overrides, labels, [secrets](#2-per-session-secrets)), stop, find by label, run a command in one (as `zapper exec` does; the drive copies files in and out with it).
- Rules: add to and remove from a session's layer.
- Approvals: answer one, optionally remembering it as a rule.
- Config: read, change (validated as a whole, applied atomically), diff, save, reload, the redacted view.
- Registration: [services](#3-services), [gateway servers](#4-the-offices-tools-in-the-gateway), [approval deciders](#5-approval-routing).
- The [driver](#6-the-driver): prompt a session.
- [Events](#7-events-and-metrics): subscribe, filtered by label.

Labels are key/value pairs on a session, `{firm: acme, worker: pat, role: engineer}`, set at creation and shown in `zapper ps`, the UI and every event. They replace `FirmLink`.

The interface is the contract between the crates: the office and `zapper-api` compile against it alone, so a feature that needs something else from the core has to add it to the interface, in the open. `zapper-api` needs the most of it, everything the CLI and UI do, including two streams: a terminal's frames (attach, the terminal viewer) and the event feed.

### The API and UI server

`zapper-api` is the REST API (the CLI's, on the unix socket) and the UI server (`daemon.ui`), built on `Core`. It owns the listener, the UI token and its cookie, and the guard every UI request goes through (`Host`, the token, and for writes `X-Zapper-UI: 1` and `Origin`). It serves the sessions pages and the terminal viewer itself, and mounts what it is given at start: with the office, its routers under `/v1/firms` and `/api/firms`, and its page at `/`. The guard applies to everything mounted, so a client's routes can't bypass it; without the office, `/` is the sessions page.

The core then has no HTTP API of its own and no way for clients to register pages: `main` wires the office's routers into `zapper-api`.

### 2. Per-session secrets

A secret declared `session: true` has no value in the config; each session that uses it is given one when it is created, held in memory for that session and dropped when it ends. The office would use it for each worker's git password, which it can write as a file at session creation until then. Outside firms, it carries a token minted for one agent and one repository (and, with a `mint` command, the core can mint it itself).

### 3. Services

A service maps a name and port that sessions use to something the core connects them to: an in-process handler (the office's git server), an address on the host, or a port of another session (over vsock, as the cluster is reached today).

- Registered through the interface, with the sessions that may use it (a label selector). The name resolves only in those sessions, is never looked up on the host, and still needs a net rule, as today.
- TLS: the core terminates it with the session's CA for the service's name, as the firm's git server does today, or passes it through.
- The office registers `git.acme.internal:443`, `cluster.acme.internal:6443` and `apps.acme.internal:80/443`. Outside firms, a `k3s` session's API can be a service other sessions reach.

### 4. The office's tools in the gateway

The office's tools (`mail_send`, `drive_publish`, `hire`, ...) become a server in the MCP gateway, registered in-process and attached to the firm's sessions by label.

- The gateway tells the server which session each call comes from (its id and labels). In-process, nothing in the guest can set or change that, so "the sender is the VM it came from" holds without the vsock port.
- The tool list stays per worker: the office answers `tools/list` for that session, filtered by role, as `ops.rs` does now.
- The office port, `zapper-agent office mcp` and `ops::serve` go. The agent knows one address, the gateway's.

### 5. Approval routing

A profile (or a session) names who decides its `ask`s:

- The operator (today's default): the request waits for an answer, up to `ask_timeout_secs`.
- A registered decider, e.g. the office: the request is refused at once with a message saying it went to review, the decider is called with it, and it answers through the interface, optionally remembering the decision as a rule on the session. This is today's firm behaviour, without the proxy knowing about firms.
- `final` rules stay above every decider, as now.

### 6. The driver

The foreman, moved into the core and made general. `agent.driven` on a profile, `false` by default: with `true`, the session starts its agent headless through the harness (`claude -p` with stream-json, `pi --mode rpc`) instead of waiting for a terminal. Only harnesses with a driver can be driven (`claude-code`, `pi`); validation refuses `driven` on others.

- Prompting a session: queued if the agent is mid-turn, or handed over at once for harnesses that take one (pi). Through the API (`POST /v1/sessions/{id}/prompt`), the CLI (`zapper tell ID "..."`; `zapper run -p P "..."` starts a driven session with its first task and follows it) and a prompt box on the UI's session page.
- Events: `turn_start`, `activity` ("Edit pricing.md"), `turn_end` with tokens, outcome (done, rate-limited, failed) and duration.
- It keeps the conversation id and resumes after a crash or a turn timeout; restarts are bounded and reported.
- Transcripts: each output line goes to the session's directory.
- State: idle or working, the current activity, turns and tokens so far, in the session info and the UI.
- Ownership: a client creating a driven session through `Core` can claim its driver, and gets a handle; prompting that session then needs the handle. The API, CLI and UI refuse to prompt an owned session and say who owns it ("pat is a worker of firm acme: mail them instead"), so nothing reaches a firm's worker around the firm's post office, journal and budgets. Sessions nobody claimed (`zapper run`, a profile with `driven: true`) are prompted by the operator freely; in `OFFICE=0` builds, that is every session. The owner gets the session's turn events and transcript.
- `attach` and `exec` stay the operator's escape hatch on any session, owned or not, and each is an event with the session's labels, so the office can journal "the board opened a terminal on pat's VM".
- What it doesn't do: decide when to wake an agent, or budgets. The office does, from mail and its ledger.

Outside firms (including `OFFICE=0` builds): an agent that runs unattended, given tasks through the API, the CLI or the UI, with its progress and replies as events and in its transcript. `zapper attach` still works on a driven session, as a read-only view of the conversation.

#### Turning the driver off

`daemon.driver: false` (default `true`) turns the driver off for the whole daemon. zapper then only sandboxes and monitors: it starts no agent on its own and gives none any input; people work in sessions through terminals, as before the driver existed. With `OFFICE=0`, that is all the build does.

- Validation refuses a config that needs the driver: a profile with `driven: true`, or a `firms` section (the office drives its workers).
- Everywhere says so: the prompt endpoint answers `409` with "the driver is disabled (daemon.driver: false)", `zapper tell` prints the same, the session info shows `driver: disabled`, the UI's session page shows a note where the prompt box would be, and `zapper check` lists it.
- Turning it off in the running config is refused while driven sessions exist ("stop them first"), so no agent is left half driven; turning it on applies at once.

### 7. Events and metrics

- The event stream gains the driver's events, rule changes, approval decisions and service registrations, each with the session's labels.
- In-process subscribers get them from the same broadcast as the UI's live feed; usage per session (tokens, turns, CPU, memory) is in the session info.

## What the office does

It owns everything about a company: firm config, the chart, mail and the journal, the drive, git repositories, budgets and contracts, security officers and grants, the office UI, replay and `zapper-inspect`. Its flows, all through the interface:

| When | It |
|---|---|
| `firm up` | Registers its git server as a service; creates a session per worker (labels, its git password, `driven: true` through the agent overrides with the driver claimed, approval routing to itself); attaches its tools to `firm=acme` in the gateway; boots the cluster session and registers its ports as services |
| Mail arrives | Records it, then prompts the recipient's session through the driver |
| A turn ends | Charges the budget from the event's tokens, updates the worker's state |
| A worker's `ask` | Is called as the decider; files it with an officer, and answers it through the interface when decided |
| A grant | Adds the net rule for its service to the worker's session (git and the cluster), and checks the grant itself on each git request; the cluster's RBAC as now |
| A hire | Creates a session; a departure stops it and revokes its grants |
| `firm down`, or the core stopping | Stops the firm's sessions, unregisters its services, gateway server and decider |

Its record (journal, transcripts, drive, repositories) stays in its own directory, through the keeper if configured, as today.

## Invariants after the split

The office's invariants ([AGENTS.md](../AGENTS.md)) hold, on general mechanisms:

| Invariant | Rests on |
|---|---|
| The sender of every office request is the VM it came from | The gateway's session identity on calls to a registered server |
| An `ask` is refused at once and decided by officers or the board, never past `final` rules | Approval routing; `final` rules checked before any decider |
| The firm's service names are routed by the daemon and never resolved on the host | Services |
| Nobody decides their own access request or their own hire's check; security is always held | The office, unchanged |
| A worker hears only through the firm (mail, notices), and every turn is in the run's record | Driver ownership: only the office's handle prompts its workers; operator terminals are events it journals |
| Access to the firm's resources is a grant, enforced on the host or by the cluster's RBAC | The office (git server, drive) and rules on the session (network) |

New ones for the core: a service is reachable only from the sessions it is registered for; session identity on gateway calls can't be set or changed by a guest; an owned session's driver is prompted only through its owner's handle; the office crate depends on the core's interface and nothing else in the daemon.

## Restarts

One process, as today: when the daemon stops, the office stops its firms first (the core tells registered clients), sessions end, and a run is not resumed.

## Steps

Each step is useful without firms, and the office moves onto it before the next, so `make test-e2e-office` keeps passing throughout.

The driver comes first: it needs nothing from the other steps (the harness code it builds on is already in the core, unused in `OFFICE=0` builds), and it is the step that gives `OFFICE=0` builds the most: unattended agents with `driven: true`. Until the `Core` interface exists, the office calls it directly, as it calls the rest of the daemon today. Ownership comes with it, not later: the office uses the driver from that step on, and the operator's new prompt must not reach its workers.

| # | Step | Useful without firms | The office moves to it | Rough size |
|---|---|---|---|---|
| 1 | [The driver](#6-the-driver) in the core: `agent.driven`, prompting through the API, CLI and UI, turn events, transcripts, ownership (the office claims its workers' drivers, so the operator's prompt is refused for them), and `daemon.driver` to turn it all off | Unattended agents with tasks, in `OFFICE=0` builds; or, with the driver off, sandboxing and monitoring only | `foreman.rs` keeps when to wake and budgets; the conversation is the driver's, claimed by the office | 700–1,100, mostly moved |
| 2 | Split `crates/zapper` into `zapper-core` (library), [`zapper-api`](#the-api-and-ui-server) (library) and `zapper` (binary); [the `Core` interface](#1-the-core-interface-and-labels), covering everything the API uses (sessions, terminals, config, rules, approvals, events, the driver) and labels | Grouping sessions; one interface for the CLI, the UI and any in-process client | `FirmLink` → labels; the office calls `Core`, not daemon internals; its routers are mounted by `zapper-api` | 900–1,300, much of it moved |
| 3 | [Services](#3-services) | The k3s test cluster reachable from other sessions | `facility::route`, firm DNS | 400–600 |
| 4 | [Registered gateway servers](#4-the-offices-tools-in-the-gateway), with session identity | Built-in tools for any client | The office port and relay go | 300–500, mostly moved |
| 5 | [Approval routing](#5-approval-routing) | External approvers (a chat bot, a policy service) | `security::ask` in the proxy goes | 300–400 |
| 6 | Move the office into the `zapper-office` crate, `firm.rs` with it; the binary hands its routers and pages to `zapper-api` and adds its commands; make the core's internals crate-private | — | `office.html`, `office::http`, `firm_cli.rs` | Mostly moving files |
| 7 | [Per-session secrets](#2-per-session-secrets) (and minting), when wanted | A token per agent and repository | Git passwords, which until then it writes as files at session creation, as today | 300–500 |

After step 6: the core has no firm code; `office_off.rs` and the dead-code allowances go; `OFFICE=0` builds the binary without linking `zapper-office`, with no stand-in needed. The office's ~13,500 lines change little: it calls `Core` where it reached into the daemon.

Every step needs a KVM host for its end-to-end tests.

## Later, if wanted: a separate process

> **Superseded by [proposal 13](proposals/13-office-process.md)**, written after the workbench: the office as a process of its own, a client of zapper's socket as the workbench is.

The same features work across a process boundary with three additions: the REST API of `zapper-api` as the client's interface, with tokens scoped to its sessions; session identity on gateway calls the upstream can check (a signed header, or a unix socket only the core reaches); and event resume by sequence number, so a client restart loses nothing. None of it is needed while the office is compiled in. A token is worth having only if the office runs as another user and reaches the core through a socket that requires one; as the operator's own user it can open the full socket anyway. [Proposal 12](proposals/12-single-user-layer.md#processes) puts remote clients' security in a gateway in front of the core instead, and the core's event resume is in place.

## Open questions

- Firm config: a `firms` section the office parses and validates (the core checks only that a client claimed it), or the office's own file. The first keeps one config and one `config diff`.
- Per-role policy: rules on each worker's session (simple, and the office already knows the roles), or a general "group" layer in the core's evaluation, shared by sessions with a label (closer to today's `Scope::Role`, and visible in `zapper rules`).
- How much of the UI is the office's: whole pages mounted by `zapper-api` (as `office.html` is now), or panels the sessions page shows for sessions with its labels.
