[← README](../README.md) · [Docs](README.md) · **Instrumentation**

# Instrumentation

## Overview

zapper records what each session does, an office worker's included, as events. Each event goes to the session's audit log, to the live stream (`zapper events`, `GET /v1/events`), and to an in-memory buffer of the latest 2000 events that the UI reads, and that a client reconnecting to the stream resumes from ([API](api.md#sessions-rules-and-config)). Terminals are recorded as asciicast files. All of this is written on the host. A firm's run adds its own journal and each worker's agent transcript ([office: state](office.md#state)).

## Events

Each event has `seq`, `epoch` (the daemon run it was emitted in: `seq` counts from 1 in each, so in a persistent session's record, written by several runs, the two together name one line), `ts` (Unix seconds), `session` and `type`:

| `type` | Fields | Source |
|---|---|---|
| `net` | `host`, `port`, `method`, `path`, `action`, `rule`, `scope`, `inspected`, `status`, `secrets` (names injected), `error` | Host proxy: one per passed-through connection, or one per request when inspected |
| `dns` | `name` (empty for an invalid query), `qtype`, `allowed`, `rcode` (`noerror`, `nxdomain`, `refused`, `formerr`, `notimp`, `servfail`), `answer` (the synthetic address), `reason` | Host resolver: A queries, refusals and rejected queries |
| `exec` | `pid`, `uid`, `path`, `argv`, `cwd`, `action`, `rule`, `scope`, `wait_ms` | Host decision on a guest exec |
| `guest` / `exit` | `pid`, `ppid`, `uid`, `comm`, `exit_code`, `signal`, `elapsed_ms`, `user_ms`, `sys_ms`, `mem_kib`, `io_bytes` | Guest kernel process accounting |
| `guest` / `setup` | `step`, `ok`, `output` (last 8 KiB) | Profile setup commands |
| `ask` / `answer` | `id`, `kind`, `subject` / `allow`, `remember` | Approvals |
| `shell` | `id`, `state` (`open`, `closed`; for a persistent terminal also `detached`, when its client goes and its program keeps running, and `attached`, when one attaches again), `argv`, `exit_code`, `client` (who opened or attached it, e.g. an SSH key's name) | Terminals |
| `session` | `state` (`starting`, `running`, `failed`, `stopped`), `detail` | Lifecycle, including start time |
| `config` | `change` | Running config changes, in words |
| `rule` | `state` (`added`, `removed`), `id`, `scope` (`global`, `profile:NAME`, `session:ID`, or a client's layer such as `role:NAME`), `kind` | Every rule added to or removed from a layer, by whoever: a client, the operator, an approval remembered, a config change (a rule changed under its id is removed, then added). A session's own are its events; rules it is created with come with it, not as these |
| `ssh` | `state` (`enabled`, `disabled`, `refused`), `client`, `reason` | A session's SSH turned on or off, or a terminal the SSH gateway asked for while it was off ([SSH](ssh.md)) |
| `driver` | `event` (`started`, `turn_start`, `activity`, `steered`, `turn_end`, `exited`, `error`), `detail` (`prompts`; `activity`; `outcome` `done`, `rate_limited`, `failed` or `timed_out`, `tokens`, `ms`, `error`; `code`; `resumed`) | [The driver](#the-driver) of a driven session |
| `disks` | `state` (`image`, `stopped`, `checked`, `changed`, `unverified`, `accepted`), `disks` (each disk's SHA-256, by file name; for `image`, the image's), `detail` (what changed, or how the image compares with its build) | A persistent session's disks, hashed when it stops and checked when it starts ([security: persistent sessions](security.md#persistent-sessions)) |
| `image` | `name`, `build`, `state` (`started`, `built`, `failed`), `detail` (the image's file, or why it failed) | An image build the daemon runs ([images](images.md)) |
| `mcp` | `server`, `tool`, `ok`, `ms`, `error` (the first 512 characters) | A tool called through the [MCP gateway](architecture.md), an office tool included; a call to a server the session hasn't is one too, refused. Names are kept to 128 characters; arguments and results aren't recorded |

The live stream adds `summary`, the event as one line, and `verdict` (`allow`, `deny` or `ask`) when the event decided something; `zapper events` and the UI's events view show those, and the audit log keeps the fields alone.

The host writes `net`, `dns`, `exec` and `mcp` events from its own decisions, so the guest can't alter them. `exit` events come from the guest kernel, and are as reliable as the guest ([security](security.md#what-is-enforced-where)).

## The driver

A session whose profile sets `agent.driven: true` runs its agent headless (`claude -p` with stream-json in and out, or `pi --mode rpc`) instead of waiting for a terminal, and gets its tasks from `zapper tell`, the API (`POST /v1/sessions/{id}/prompt`) or the prompt box on the UI's session page. It is the office's foreman without the firm: one conversation per session, kept between turns.

- Tasks that arrive while the agent works wait, and go together as its next turn; pi takes them at once.
- A turn the provider rate-limits, or that fails before the agent could work, keeps its tasks and is tried again a minute later. A turn without an end in 30 minutes drops the process; three in a row rest the driver. An agent that exits takes its turn's tasks back and the next process resumes the conversation; more than five exits in ten minutes rest it for two minutes.
- The session's info has `driver`: `status` (`idle`, `starting`, `working`, `backoff`, `error`, `stopped`), `activity`, `error`, `queued`, `turns`, `tokens` and `conversation`; or `"disabled"` with `daemon.driver: false`. The UI shows it with the session's details.
- A firm's workers are driven by the office's foreman, not by this driver, and take work by mail: the prompt route refuses them.

## Files

Each session directory, `state/sessions/<id>/`, holds:

| File | Content |
|---|---|
| `audit.jsonl` | The session's events, one JSON object per line |
| `driver.jsonl` | A driven session's transcript: every line the agent wrote (Claude Code's stream-json, pi's events), and `{zapper: stderr|raw|truncated}` for the rest |
| `shell-<n>.cast` | asciicast v2 recording of terminal *n*, with output (`o`), input (`i`) and resizes (`r`); replay it with `asciinema play shell-1.cast` |
| `console.log` | Serial console and Firecracker messages, up to 1 MiB |
| `metrics.json` | Firecracker metrics: vCPU exits, block, network and vsock counters (in `vm/` while the session runs) |
| `vm.json` | The Firecracker configuration the session started with (in `vm/` while the session runs) |

These files stay after the session ends. The `vm/` directory, with the disks and sockets, is deleted.

## The UI

`zapper serve` prints `http://127.0.0.1:7878/?token=…` and writes the URL to `state/ui-url` (0600). The UI opens on the office: firms' org charts, mail, security, shared drives, statistics and replays, where the board writes to workers and decides access requests ([office: web UI](office.md#web-ui)). A link in its footer opens `/sessions`, which shows the daemon's sessions; there, the one action is answering approvals. Its views:

- **sessions**, with counters
- **session detail**: the session's details and terminals, its network traffic, exec history with decisions and wait times, process exits with CPU and memory
- **network**: every request and connection of the session, newest first, like a browser's developer tools: `POST api.github.com:443/repos/acme/x/pulls`, its status, type, size, time and deciding rule, with a filter box. Clicking one shows its headers, request and response bodies, and timing. Requests are listed in detail when the connection is inspected (a rule with `methods`, `path` or `secrets`, or plain HTTP). Passed-through connections show as one `CONNECT host:port` row with bytes and duration, since they're encrypted end to end
- **terminal viewer**: "Watch" on a terminal opens it in its own browser window (`/terminal?session=ID&n=N`), as you'd see it, through xterm.js: what it has printed so far, then live output, until it closes. Clicking "Watch" again brings that window forward. It's read-only: keystrokes in the viewer go nowhere. The daemon keeps up to 2 MiB of output per terminal for this; the full recording stays in `shell-<n>.cast`
- **events**: the live stream, filtered by session, type, action and text
- **approvals**: pending requests, each with Allow and Deny buttons and a choice of answering this request only, or the rest of the session. The tab shows how many are waiting
- **rules**, grouped by layer
- **config**: running, startup and the diff between them

The network view keeps up to 1000 requests per session in memory, with the first 64 KiB of each body and up to 16 MiB of bodies in total; gzip, deflate and brotli bodies are decoded for display. Request headers are recorded as the guest sent them, before the proxy adds secrets, and secret values the proxy added are shown as `***` wherever they appear, including in a response that echoes them back. `guest`-mode secret values aren't masked yet. None of it is written to disk.

The UI exchanges the token for a strict cookie and, on loopback (the default), requires a local `Host` header, which defeats DNS rebinding; listening on the network for a remote host ([configuration: a remote host](configuration.md#a-remote-host)), it takes any `Host`, and the cookie, set for that name only, is what keeps others out. Its writes (answering an approval, and the board's mail and uploads, firm controls, security and credential decisions, background checks, budget decisions, funding and releasing workers, deleting past runs, notes on runs) also need an `X-Zapper-UI` header and an `Origin` naming the UI: a local one on loopback, else the `Host` the request came to. A page on another site can't send that header without a CORS preflight, which the UI never grants, so it can't write through your browser. Rules, profiles, sessions and the config are changed with the CLI or the API on the unix socket; the one exception is promoting a firm's standing approval into its role's profile in the running config. The UI renders session and firm data as text; files workers publish are served with their own type and a sandboxing CSP.

## Usage

```
zapper events --history                         # one line per event
zapper events --json -s tusn49 | jq 'select(.type=="exec") | .argv | join(" ")'
zapper audit tusn49 | jq -r 'select(.type=="net" and .action=="deny") | .host' | sort | uniq -c
```

## Limitations

- Exit accounting reports the kernel's 15-character `comm`. For the full command line, join with `exec` events on `pid`.
- Process accounting counts time in 10 ms ticks and reports average memory.

## References

- `man 5 acct`; asciicast v2: https://docs.asciinema.org/manual/asciicast/v2/
- Sources: `crates/zapper-core/src/daemon/{mod,session,nic}.rs`, `crates/zapper-agent/src/acct.rs`
