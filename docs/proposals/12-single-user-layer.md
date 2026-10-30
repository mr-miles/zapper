# A layer for single users: sandboxes you provision, keep and reach over SSH

One person starts, provisions and works with one or more of their own agents, or other untrusted code, in zapper sandboxes, keeping every control zapper has, without a folder of homegrown scripts. A separate layer sits on zapper built with `OFFICE=0`. It provides a set of facilities (secrets, a shared drive, a git repository of agent configuration, an SSH connector, a wizard for new sandboxes) and serves the API that a web UI or automation uses, including zapper's own session views. The user provisions a sandbox once, then does their work in it over SSH, and the sandbox is still there tomorrow.

The layer's working name here is **the workbench** (`zapper-workbench`); the name is a placeholder.

This proposal has two parts. [Part 1](#part-1-changes-to-zapper) is what zapper's core needs: sandboxes that persist, stop and start, and survive a restart of the daemon, and a way for facilities outside the daemon to serve guests. These are general changes that land in zapper on their own. [Part 2](#part-2-the-workbench) is the layer itself. It builds on [proposal 10](10-ssh-gateway.md) (SSH) and revises [proposal 11](11-core-drive.md) (the drive).

## Today

- **The sandbox's lifetime is the daemon's.** Each VMM gets `PR_SET_PDEATHSIG, SIGKILL` and `kill_on_drop` (`daemon/vm.rs`), and Firecracker gets the death signal again (`vmm.rs`): VMs die with the daemon, even if it crashes. zapper-agent powers the guest off when its control connection closes (`zapper-agent/src/control.rs:64`). When the host side of a terminal goes away, zapper-agent sends `SIGHUP` to the terminal's process group (`shell.rs:153`). [security](../security.md#limitations) says it plainly: "Sessions end when the daemon restarts."
- **A session's state lives only in the daemon's memory**: its resolved profile, labels and session-layer rules; remembered and pending approvals; the per-session CA, which the guest trusts (`tls.rs`, `SessionTls`); the DNS table of synthetic addresses (`dns.rs`); the subordinate-uid lease (a static in `subids.rs`); the proxied connections.
- **A session that ends loses its disks.** Teardown deletes `vm/`, with the root and data disks (`session/lifecycle.rs`). There is no stopped-but-kept state and no start.
- **The VMM is already a separate, small, confined process.** `zapper __vmm` (`vmm.rs`, `netns.rs`) enters its own user, network and mount namespaces, pivots into a minimal root, starts Firecracker, and relays each captured guest connection to the daemon by connecting to `nic.sock` once per connection (`netns.rs:235`).
- **Facilities that guests reach live in the daemon.** A service goes to an in-process handler, which is handed the calling session (`Target::Handler`, `daemon/services.rs`), to an address, or to another session's vsock port. Through the API, only the last two can be registered, and neither tells the target which session is calling.
- **Images are built from the CLI only** (`zapper image build`); there is no route for it.
- **The office** is the precedent for a layer: a client of `Core` that mounts its routes and page into the daemon's API and UI. It runs in-process.

## Part 1: changes to zapper

### 1. Sandboxes that persist: stop, start, remove

A session gains a **stopped** state, which keeps its disks.

| Operation | What happens |
|---|---|
| `zapper stop ID` | `HostMsg::Shutdown`: the guest powers off cleanly (or is killed after a grace period). The root disk, data disks, session directory and record stay. The session is listed as `stopped`. |
| `zapper start ID` | Boots the kept disks: a fresh boot, like a laptop's reboot. Installed packages, files and home directories are as they were; running processes aren't. A new `Init`, and new minted guest secrets. The same CA ([below](#state-that-must-stay-the-same)). |
| `zapper rm ID` | Deletes the disks. The record (audit log, recordings) stays unless `--purge`. |
| `zapper run` without `--keep` | Unchanged: removed when its command exits. |

- Mounts are copied once, at creation, into data disks, which persist with the sandbox like its root disk. `zapper cp` and the drive are the ways out.
- An image rebuilt later doesn't change existing sandboxes: each keeps its own root disk. `zapper ps` shows the image and when it was built, so a sandbox on an old image is visible.
- `ttl_secs` limits a sandbox's running time from each start. A new `engine.max_age` (unset by default) removes a stopped sandbox after that long.
- `max_sessions` and the memory admission count running sandboxes only. `zapper ps` shows each sandbox's disk use, since stopped sandboxes take space (sparse files, reflinked where the filesystem allows).
- API: `POST /v1/sessions/{id}/start`; `stop` stays as it is, now keeping the disks; `DELETE` removes.

This step alone gives sandboxes the user can stop and come back to. It doesn't yet survive a daemon restart: a restart stops every running sandbox, which can then be started again.

### 2. Sandboxes that survive a restart of the daemon

The daemon can't be made small enough never to need restarting. It is where the code that most needs patching lives (the HTTP and TLS parsers, DNS, the database wire protocols), and upgrades, `daemon.keeper` changes and crashes all restart it. So instead of avoiding restarts, **make them harmless**: the one piece a running VM needs is the process that owns Firecracker, and that piece already exists and is small.

#### The supervisor: `zapper __vmm`, made durable

- No death signal. It starts in its own session (`setsid`), and in its own systemd `--user` scope when the daemon runs under systemd, since `systemctl restart` stops a unit's whole cgroup by default.
- It is Firecracker's parent and waits on it. It writes `vmm.pid` and listens on `vmm.sock` in the session directory, outside `vm/`, through which a daemon adopts it.
- It enforces the sandbox's `ttl_secs` deadline itself, so the deadline holds while no daemon runs.
- It stays what it is: confined in its namespaces and root, and minimal. Its new jobs are waiting, being adopted, and the deadline. Nothing that parses guest input moves into it.

#### While no daemon runs, the sandbox fails closed

- **Network**: `__vmm` connects to `nic.sock` for each captured connection. With no daemon listening, the connection is refused. In vsock mode the proxy's port has no listener either. Nothing leaves the VM.
- **Exec**: zapper-agent can't ask, so an exec waits for the daemon to return, up to a bound (60 s), and then is refused.
- **Terminals** stay alive in the guest ([below](#terminals-that-outlive-their-connection)).
- **Record**: nothing happens that isn't recorded, because nothing can happen that needs a decision. The gap shows as a `daemon` event when the session is adopted, with its start and end.

#### A control channel that resumes

- `Init` carries a **resume token**: 32 random bytes, kept by zapper-agent in PID 1's memory, which the uid 1000 workload can't read.
- On control EOF, zapper-agent no longer powers off. It reconnects to port 1000, retrying with backoff, and sends `Resume { token }` as its first line. An `ExecCheck` waiting for an answer is sent again on the new connection.
- When a daemon adopts a sandbox, it binds `v.sock_1000` again, accepts one connection, and keeps it only if its first line is `Resume` with that session's token. Then it removes the socket, as at boot.
- `HostMsg::Shutdown` still powers the guest off. A guest that gets no daemon back stays up, with nothing allowed, until the supervisor's deadline or a `stop`.

The invariant changes from "the host accepts only the first control connection" to **"the host accepts the first control connection, and afterwards only a resumption that carries the session's token."** A workload that is root in the guest could read the token, but root in the guest already defeats every control enforced in the guest, and the host-side controls hold either way.

#### State that must stay the same

The daemon writes each session's state to its directory, outside `vm/`, so the VMM never sees it:

| State | Kept as |
|---|---|
| Profile, resolved config, labels, workdir, created, TTL, status | `session.json` |
| Session-layer rules and remembered approvals | `rules.jsonl`, appended as they change |
| The DNS table: synthetic address ↔ name | `dns.jsonl`, appended as names are given out. The guest may hold addresses in its resolver cache for as long as it likes |
| The subordinate-uid lease | in `session.json`; leases are rebuilt from the directories at start |
| **The per-session CA** | The supervisor holds its key in memory and gives it to the daemon that adopts it, over `vmm.sock`. It is never written to disk, which keeps [security](../security.md#what-is-enforced-where)'s "kept in memory". A sandbox that is *stopped* and *started* gets the key back the same way only if its supervisor lived, so after a host reboot the sandbox gets a new CA, and zapper-agent installs it at boot as now |

Pending approvals are lost: the connections waiting on them were dropped. They are asked again when the guest retries. In-flight proxied connections drop too, and clients retry, a model API stream among them.

#### Adoption at start

The daemon scans `state/sessions/`:

- A live supervisor (`vmm.pid` alive, `vmm.sock` answering): adopt it. Get the CA, bind `nic.sock` and the vsock listeners again, accept the resumption, reattach terminals, restart the driver for driven sessions (which already resumes a conversation), and emit `daemon` with the gap.
- A dead supervisor, after a host reboot or a crash: mark the sandbox `stopped`, so it can be started.
- A stopped sandbox: listed as `stopped`.

### 3. Terminals that outlive their connection

A terminal keeps running when its host connection closes, and the host can attach to it again: `GET /v1/sessions/{id}/attach?terminal=N`, `zapper attach ID --terminal N`.

- A daemon restart, or the user's SSH or VS Code connection dropping, then leaves Claude Code running, and the user reconnects to the same terminal.
- A terminal ends when its command exits, or when the host closes it on purpose (`DELETE …/terminals/N`, `zapper attach --hangup`).
- The recording continues in the same `shell-N.cast`, without gaps when the guest still holds the bytes, and with a marked gap when it doesn't.
- It gives herdr, and any multiplexer, the missing piece: a pane can attach to an existing terminal.

#### Today

A terminal is tied to the vsock connection that opened it (`zapper-agent/src/shell.rs`, `session`):

1. zapper-agent (PID 1) forks the program: `setsid`, the pty as its controlling terminal (or pipes without a tty), uid 1000, `no_new_privs`, the seccomp exec filter, `execve`.
2. The `supervise` thread answers the filter's notifications over the control channel. It doesn't depend on the terminal's connection.
3. Output pumps read the pty master, or the stdout and stderr pipes, and write frames straight to the connection; they stop when a write fails.
4. The input thread reads frames from the connection; on EOF or an error it sends `SIGHUP` to the process group (`shell.rs:153`).
5. On exit, zapper-agent sends `Exit(code)` and reports `ShellExit` on the control channel.

When the connection drops, the program gets `SIGHUP`. One that ignores it soon blocks, since nothing drains its pty. The scrollback that watchers see is kept on the host (`daemon/session/terminal.rs`, `Terminal`), so it is lost with the daemon.

#### Where it runs: a holder process per terminal, not PID 1

PID 1 keeps what needs privilege and is already reviewed: forking the program, dropping privileges, installing the exec filter, `supervise`, and reaping the child. Everything that only shuffles bytes moves into a **holder**: the same zapper-agent binary, run as `zapper-agent hold`, one process per terminal, started by PID 1, running as root, with `no_new_privs`.

- PID 1 forks the program as today, then passes the pty master (or the three pipes) to the holder with `SCM_RIGHTS`, and passes host connections for that terminal the same way. The holder never forks or execs anything.
- PID 1 reaps the program and tells the holder its exit code over their socket.
- A bug or a panic in a holder ends that terminal, not the VM. A panic in PID 1 powers the guest off.
- The holder's memory is its own, so the output buffer, and a terminal emulator if one is used ([below](#getting-the-screen-back)), never grow PID 1.
- The workload can't reach a holder: it runs as root, its socket to PID 1 is a socketpair, and it accepts nothing else.
- PID 1 keeps a table of terminals (ID → holder, program's process group) and restarts nothing: a holder that dies ends its terminal with `SIGHUP`, as a dropped connection does today.

#### The holder

- **With a tty** (shells, Claude Code's interface): the holder **keeps reading** the pty into a buffer (256 KiB), with a running byte offset, and forwards to the attached connection if there is one. A failed write detaches the connection. The program never blocks, so Claude Code keeps working while nobody is attached; output older than the buffer is lost.
- **Without a tty** (`zapper exec`, SSH `exec` channels, the driver's headless agent): only with `ShellRequest { persist: true }`, which the driver sets. The holder **stops reading** while detached, so the program blocks when its pipe fills and continues on reattach, losing nothing. The driver's stream-json can't have a gap. Without `persist`, a non-tty terminal hangs up as now: an `exec` whose client has gone is of no use.
- **Disconnecting** detaches; it doesn't hang up. A new `Hangup` frame from the host ends the terminal on purpose.
- **Reattaching**: `ShellRequest` gains `attach: Option<u64>` (the terminal) and `from: Option<u64>` (a byte offset). The holder answers with a new frame, `Attached { from, to, rows, cols }` (the oldest and newest bytes it holds, and the size), replays from `from` or from the oldest byte, then streams live output. A second connection **takes over** from the first, as `tmux attach -d` does; read-only watchers stay on the host's stream route.
- **Exit while detached**: the remaining output and `Exit(code)` go to the next attach, then the holder exits. A detached terminal whose program has exited is removed after 24 hours unattached.
- **Recording**: the host knows the offset where `shell-N.cast` stopped, and reattaches with `from` set to it, so the recording continues without duplicates, or with a marked gap if those bytes have left the buffer.

#### Getting the screen back

The buffer holds raw output, so its tail can begin inside an escape sequence and doesn't recreate the screen (that Claude Code switched to the alternate screen, for instance). Two ways, in order:

1. **Redraw** (first): after the replay, the holder applies the new client's window size, so the program gets `SIGWINCH` and full-screen programs redraw; if the size is unchanged, it resizes by one row and back. Plain shells just show the tail. This is what `dtach` and `abduco` do, and it needs no emulator.
2. **An emulator in the holder** (if redraws prove unreliable): the holder keeps an in-memory screen with the `vt100` crate and, on reattach, sends the screen as it is, then live output, as `shpool` does (with its own `shpool_vt100`). In the holder, not PID 1, a parser of the workload's output can't take the VM down.

#### Libraries and tools

| | What it is | Use here |
|---|---|---|
| `dtach`, `abduco` | Small C tools: a process on a pty behind a unix socket, detach and attach, a redraw on attach | The model for the holder's tty mode. Not bundled: they run as the user and know nothing of offsets, recordings or zapper's frames |
| `shpool` | Rust (Apache-2.0): a daemon that keeps named shell sessions, keeps an in-memory render with `shpool_vt100`, and redraws on reattach | The model for the emulator option, and its crate a candidate. Not bundled: it runs as the user, spawns shells itself, and has its own client protocol |
| `vt100` crate | A terminal parser that keeps screen state, and can produce the escape sequences that redraw it | The emulator option, in the holder |
| `tmux`, `screen` | Full multiplexers with emulators | Too much, and they would put the workload in charge of its own terminals |

Running one of these tools in the image would break "any image works", and since they run as the workload's user, the workload could kill, rename or attach to them. Building the holder into zapper-agent keeps the terminal out of the workload's reach, keeps exec supervision where it is, and keeps the byte offsets that make recordings continuous.

#### Limits

At most 16 terminals per sandbox, past which a new one is refused; one buffer per terminal, fixed; `Attached` and replayed frames within `MAX_FRAME`. A holder's memory is bounded by its buffer (and the emulator's screen, if used), whatever the workload writes.

#### What stays the same

- zapper-agent accepts terminal connections from the host's CID only, so attaching, reattaching and taking over stay host-only, and terminal IDs needn't be secret.
- Exec supervision is untouched. While no daemon runs, checks wait for the control channel to resume (§2), then fail closed.
- The existing frames are unchanged. New: `attach`, `from` and `persist` in `ShellRequest`; the `Attached` and `Hangup` frames.

### 4. Services served by a facility outside the daemon

A new service target: **`to: {socket: PATH}`**, a unix socket owned by a facility.

- For a TLS service, the daemon terminates TLS with the session's CA for the name asked for, as it does for in-process handlers, then connects to `PATH`.
- It first sends one header line naming the caller, which the guest can't set: `{"session": "ID", "labels": {…}, "name": "drive.zapper", "port": 443}`. Then it relays the stream.
- The service's name resolves only in sessions its labels pick, as for any service.
- Only the operator, through the API, can register a socket target, and `PATH` must be under the daemon user's runtime directory.

This one change lets every guest-facing facility live in its own process, with the same caller identity the in-process handlers get. It replaces proposal 11's in-daemon drive handler.

### 5. Policy for those services: `net` path rules, with placeholders

Since the daemon terminates TLS for socket services, it can **inspect** their HTTP like any inspected connection, and apply `net` rules with `methods` and `path`, `ask` included, before relaying. A facility then needs no access control of its own for guest requests.

Two additions to path globs: `{session}` (the session's ID) and `{label:KEY}` (one of its labels). A rule whose label the session lacks matches nothing.

```yaml
- { kind: net, action: allow, host: drive.zapper, methods: [GET, HEAD], path: "/inbox/**" }
- { kind: net, action: allow, host: drive.zapper, methods: [GET, HEAD, PUT, DELETE], path: "/outbox/{session}/**" }
```

This replaces proposal 11's new `drive` rule kind: the drive becomes an ordinary inspected service.

### 6. Building images through the API

`POST /v1/images/{name}/build` (recipe arguments in the body) → a build ID; `GET /v1/images/builds/{id}` streams the log as server-sent events; `GET /v1/images` lists images with their source, size and build time. Builds run one at a time, and the CLI's `zapper image build` uses the same route.

### 7. From proposal 10

The SSH gateway's needs from the core: an upgrade to a raw byte stream for the gateway, and the forward to the guest's loopback (`LOCAL_FORWARD_PORT`) for VS Code. Shell and exec over SSH use `attach`.

### 8. Clients outside the daemon: event resume and scoped tokens

The workbench is a client of the API in another process, as [firm separation](../firm-separation.md#later-if-wanted-a-separate-process) foresees for the office. Two things such a client needs that the API lacks:

- **Event resume.** Events carry a `seq`, but `/v1/events` takes only `session` and `history`, and `seq` starts again at 1 when the daemon restarts (`daemon/events.rs`). Add an `epoch` (an ID for the daemon's run, in every event and in `/v1/status`) and `?after=EPOCH:SEQ` (or SSE's `Last-Event-ID`). A client that reconnects gets what it missed from recent history, or a `gap` event saying history no longer reaches back that far. A new epoch tells it the daemon restarted.
- **Scoped tokens.** Today, whoever opens the socket can do everything. Add API tokens, each limited to sessions with given labels and to a set of routes (read, answer approvals, create sessions, change rules at those sessions' layer). The workbench uses the socket with full rights, as the operator; the office out of process, or an automation, gets a token for its own sessions only.

### Invariants

Changed:

- **Control**: the host accepts the first control connection, and afterwards only a resumption carrying the session's token.
- **The VMM** no longer dies with the daemon. While no daemon runs, the guest reaches nothing and starts nothing new, and its supervisor still enforces its deadline.

New:

- A sandbox is adopted only through its own supervisor's socket, and its CA comes only from that supervisor.
- A socket service's caller is named by the daemon, from the VM the connection came from, never by the guest.

`tests/e2e.sh` gains:

- restarting the daemon with a guest running, then checking that during the gap a connection and an exec are refused, and that after it the terminal, rules, DNS answers and TLS trust all hold;
- a resumption with a wrong token refused;
- stop, start and remove;
- a socket service seeing the right caller, and a guest-sent header ignored.

### Steps

Ordered so that what also makes zapper with the office (`OFFICE=1`) better separated, clearer or better encapsulated comes first. Each step is useful without the workbench, and leaves `make test-e2e` and `make test-e2e-office` passing.

| # | Step | What `OFFICE=1` gains | The workbench needs it for |
|---|---|---|---|
| 1 | **Socket services with the caller named** (§4) | The office's facilities, its git server and drive among them, can be served from outside the daemon, behind the same interface: the step [firm separation](../firm-separation.md#later-if-wanted-a-separate-process) lists for a separate office process ("session identity on gateway calls the upstream can check") | Drive and git as facility processes |
| 2 | **Inspected service traffic, `net` path rules with `{session}` and `{label:KEY}`** (§5) | Access to the firm's git and drive can be stated as policy, visible in `zapper rules` and `eval`, instead of grant checks only the office's code makes | The drive's access control |
| 3 | **Event resume and scoped tokens** (§8) | The other two of firm separation's three additions for a separate office process; the office's UI and foreman stop losing events across a reconnect | A gateway that survives either side restarting |
| 4 | **Image builds over the API** (§6) | The office's Kickoff can build a missing image instead of failing; the CLI and the UI share one route | The wizard and the repository |
| 5 | **Persist, stop, start** (§1) | Facilities (a firm's cluster) and workers can be stopped and kept; the cluster's state survives a firm going down | Sandboxes the user comes back to |
| 6 | **Terminals that outlive their connection** (§3) | `persist: true` for the foreman's driver terminals: a worker's agent survives a dropped host connection; terminal handling leaves PID 1 for holder processes | Reconnecting over SSH to the same terminal |
| 7 | **Survive a restart of the daemon** (§2) | Workers' VMs and terminals outlive a daemon restart. The office still ends its run when the daemon stops ([firm separation: restarts](../firm-separation.md#restarts)); resuming a run is the office's own work, after this | Upgrading zapper without losing sandboxes |
| 8 | **SSH pieces** from proposal 10 (§7) | — | The SSH connector |

[Proxy separation](../proxy-separation.md)'s session process (guest input parsed outside the process that holds the secrets) is the other change that most improves `OFFICE=1`'s separation. It is its own track, and fits with step 7 as follows. That process parses guest input, so it must be restartable like the daemon, not the durable piece. The VMM supervisor holds the CA's key and hands it to both, and both are adopted after a restart. Its step 2 (the control channel and terminal frames move into it) comes after this proposal's step 6, so terminals are already held in the guest when their host side moves.

## Part 2: the workbench

### Processes

| Process | Runs as | Does |
|---|---|---|
| **zapper** (`OFFICE=0`) | the user | Unchanged as the boundary: VMs, policy, the proxy and its secrets, DNS, the record |
| **zapper-workbench** | the user | The facilities, as modules, and the gateway: the workbench's API, and the web UI |
| *(optional)* **zapper-sshd** | its own unprivileged user | Only if SSH must work without the host's sshd ([SSH](#ssh-connector)) |

Two processes, not three. Facilities are modules behind one interface in the workbench process, so adding or removing one is a build or config choice, and moving one into a process of its own later is mechanical. A separate gateway process would add authentication between processes and another thing to deploy, for a modularity the module boundary already gives.

**Trust.**

- The workbench holds zapper's API socket, so it is zapper's operator. It runs as the same user.
- Its own API and UI listen on loopback by default, behind a token, with the same `Origin` and `X-Zapper-UI` checks as zapper's UI. Remote use goes through an SSH tunnel, as for zapper's UI.
- Guest input never reaches zapper's write routes through it. Requests from guests reach facilities only through socket services (Part 1 §4), already decided by zapper's policy.

### The facility interface

Each facility provides:

- **Routes** under `/w/v1/<facility>/…` on the workbench's API.
- **A UI panel**: a page fragment and its script.
- **Services** it registers in zapper at start: a name, a port, its own unix socket, and the default `net` rules it suggests.
- **Events** it subscribes to from zapper's `/v1/events` (for example, session start and stop).
- **Its part of the config**, in the workbench's git repository ([below](#git-repository-agent-config-recipes-skills)), validated as a whole with the rest.

Facilities don't talk to each other directly. The wizard calls their routes like any client.

### Session views (the gateway)

The workbench's API proxies zapper's session routes under its own authentication:

- sessions (list, get, create, stop, start, remove);
- events (SSE), audit, requests, terminals and their streams;
- approvals and answering them.

The UI shows what zapper's UI shows today (audit logs, events, approvals, the network view, usage, terminals read-only, stop and remove), plus start, disk use, and which facilities a sandbox uses. Answering approvals stays a write that needs the token, as in zapper's UI.

### Secrets

- **Storage**: values stored by the workbench under `~/.config/zapper-workbench/secrets/`, one file each (mode 0600 in a 0700 directory), or in the OS keyring where there is one.
- **In zapper**: it writes zapper `secrets:` entries that point at those files (a `file` source), never inline values, with the `hosts` each binds to, the mode (`proxy` by default; `guest` only when the user picks it, with the warning from [security](../security.md#secrets-given-to-a-session-can-be-used-by-it)), and the `net` rule that uses it. Then `PUT /v1/config` (validated, atomic) and `config save`.
- **Presets**: Claude (`sk-ant-oat…`, `api.anthropic.com`), OpenRouter, GitHub tokens, AWS (as an `aws` secret, signed on the host), a database (`dbauth`). Each preset knows its hosts, its header and a check that the value works.
- **Never shown back**: the UI shows names, hosts, modes and which profiles use each, never values. Replacing a value is a write; reading one isn't possible through the workbench.

### Shared drive

- Files in a host directory, `~/.local/share/zapper-workbench/drive/`.
- **From outside the host**: `scp` and `sftp` to `drive.zapper` through the SSH connector, SFTP served by the workbench from that directory ([proposal 10](10-ssh-gateway.md#files-the-drive-not-the-guest)).
- **Inside the host**: the service `drive.zapper:443` (Part 1 §4), HTTP `GET`, `HEAD`, `PUT`, `DELETE` and directory listings, decided by `net` path rules (Part 1 §5). The wizard adds the usual rules: read on `inbox/`, write on `outbox/{session}/`.
- Proposal 11's paths, links, bounds and events hold as written, enforced by the facility: normalized paths, `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, writes through a temporary file and a rename, size bounds, a `drive` event per request. The `drive` rule kind and the in-daemon handler are replaced by Part 1 §4–5.

### Git repository: agent config, recipes, skills

The workbench's configuration is a git repository, `~/.local/share/zapper-workbench/config.git`, with a working copy the user edits or pushes to:

```
profiles/<name>.yaml      zapper profiles: engine, agent, mounts, policy
images/<name>.yaml        image recipes (zapper's `images:` entries), with files they copy
agents/<name>/            an agent's definition: AGENT.md, instructions, its own skills/
skills/<name>/            skills any agent may use
secrets.yaml              secret names, hosts, modes, sources: never values
drive.yaml                the drive's default rules and bounds
workbench.yaml            facilities on or off, the UI's address
```

- **On each commit to `main`**, the workbench validates the whole tree, renders the profiles, images and secret entries into zapper's running config (`PUT /v1/config`, all or nothing), rebuilds changed images (Part 1 §6), and records which commit is live. A commit that doesn't validate isn't applied, and the UI shows why.
- **Skills and agent definitions** reach sandboxes read-only, through the harness's existing mounts (`agent.skills`, `config_mounts`), from a checkout of the live commit.
- **Agents may propose changes**: the repository is also served to guests as `git.zapper` (a socket service), and `net` path rules can let a sandbox push to a branch, never to `main`. The user merges in the UI or locally.

### SSH connector

Built on [proposal 10](10-ssh-gateway.md): the gateway carries SSH channels to sandboxes through zapper's `attach` and the loopback forward, and serves SFTP for `drive.zapper` from the drive.

- **Keys**: the user's public keys, each with a name and the sandboxes it reaches (all, by label, or by ID).
- **Through the host's sshd (the default)**: the connector writes restricted lines in the user's `~/.ssh/authorized_keys`, each between markers it owns:
  ```
  restrict,command="zapper-workbench ssh-proxy --key laptop --only label=project=x" ssh-ed25519 AAAA… laptop
  ```
  The key gets no host shell and no forwarding of its own: sshd runs only the gateway, which reads the target from `SSH_ORIGINAL_COMMAND` and refuses sandboxes outside `--only`. The key's name goes into `shell`, `forward` and `drive` events, so the record says who connected. No root is needed, and there's no new listener.
- **Client config**: the UI shows the `~/.ssh/config` block for the user's machine:
  ```
  Host *.zapper
      ProxyCommand ssh -i ~/.ssh/laptop user@zapper-host %h
      HostKeyAlias zapper
  ```
  …plus the VS Code settings (`remote.SSH.localServerDownload: always`).
- **Without the host's sshd** (optional): `zapper-sshd`, a separate process as its own unprivileged user, listening on a port the user picks, authenticating keys itself, then passing the stream to the gateway with the key's name. Loopback by default.

### Boot wizard

1. **What to run**: a profile from the repository (Claude Code, pi, a shell, a custom one), or a new one from a template.
2. **Image**: present, or built now with its log shown.
3. **Work**: an empty directory, a host directory copied in, a repository cloned at boot, or folders on the drive.
4. **Secrets**: which ones, with their hosts. Missing ones are entered here and stored as above.
5. **Network**: a preset (only the model API; the model API plus package registries; ask for everything else), shown as the `net` rules it adds, editable.
6. **Size and lifetime**: vCPUs, memory (checked against the host), disk, `ttl_secs`, `max_age`.
7. **Access**: which SSH keys reach it.
8. **Create**: `POST /v1/sessions` with labels for the workbench (`workbench=1`, the project). Then the result: `ssh ID.zapper`, the VS Code line, the drive's paths for this sandbox, and the approvals view.

The wizard's choices are saved as a profile in the repository, so the next sandbox like it is one click, or one `POST`.

### The workbench's API

Under `/w/v1`:

- `sessions…` (the proxied session routes), `wizard`;
- `secrets`, `drive/{path}`, `repo` (the live commit, validation results, applying a commit), `ssh/keys`, `images` (proxied);
- `events` (zapper's events, plus the facilities' own).

Automation uses the same API with a token from `~/.config/zapper-workbench/token`. The UI is a client of it and nothing else.

### Phases

The workbench starts on the API as it is, and takes each core step as it lands.

1. **Skeleton and session views**, on today's API: the gateway, its token, the UI's session pages; the facility interface. With core step 3, events resume across reconnects.
2. **Secrets** facility: needs nothing new from the core.
3. **The repository**, applying commits to the running config; with core step 4, building images.
4. **Wizard**, with core step 5: sandboxes created to be kept, stopped and started.
5. **Drive**, on core steps 1–2.
6. **SSH connector**, on core step 8 and proposal 10, with core steps 6–7 so the user's work survives dropped connections and daemon restarts.
7. **Git served to guests**, on core steps 1–2, for agents' proposed changes.

## Open questions

- **Pause and resume** (a Firecracker memory snapshot of the same VM, so running processes survive a stop or a host reboot): worth adding after stop and start? It's the same VM, so the clone-uniqueness problems of snapshot-based fork don't apply; the clock does, and the CA would need storing with the snapshot.
- **The CA after a host reboot**: a new CA on start is safe but means every client's pinned trust changes. Storing the key (0600, outside `vm/`) would keep it, at the cost of "never on disk".
- **One sandbox per project, or many?** The wizard and labels allow both; the UI's grouping should follow whichever is common.
- **The office**: whether the office should, later, become a workbench facility rather than an in-process mount. Not part of this proposal.
- **Multi-user**: out of scope. Everything here assumes one user, who is zapper's operator.
