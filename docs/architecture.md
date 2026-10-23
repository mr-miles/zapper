[← README](../README.md) · [Docs](README.md) · **Architecture**

# Architecture

## Overview

The Office runs on zapper: each worker, and each firm's cluster, is a zapper session. This page describes zapper's parts and a session's life; the parts the office adds to the daemon are under [the office](#the-office).

Terms used on this page:

- **Host**: your machine, where zapper runs.
- **Session**: one sandbox. Each session is one small virtual machine; each office worker is one.
- **Guest**: the inside of that virtual machine.
- **Workload**: the program that runs in the guest, such as a worker's Claude Code, or a shell.

```
            HOST                                        GUEST (one per session)
 ┌──────────────────────────────────┐            ┌──────────────────────────────┐
 │ zapper CLI ──▶ zapper daemon      │            │ zapper-agent                 │
 │                 │                 │   vsock    │  sets up the guest           │
 │                 ├── starts VM ────┼───────────▶│  runs your program           │
 │                 ├── decides ◀─────┼── "may I?" │  asks before each program    │
 │                 └── proxy ◀───────┼── network ─│  starts                      │
 │                        │          │            │                              │
 └────────────────────────┼─────────┘            │ workload (uid 1000)          │
                          ▼                       └──────────────────────────────┘
                   internet
```

zapper has these parts:

| Part | Runs | Job |
|---|---|---|
| `zapper` CLI | on the host, when you type a command | Talks to the daemon |
| `zapper` daemon | on the host, started by `zapper serve` | Starts sessions, decides what they may do, records what they did |
| `zapper __vmm` | on the host, one per session | Starts Firecracker locked in its own namespaces |
| `zapper-agent` | in the guest, as its first process | Sets the guest up and runs the workload |
| `zapper-keeper` | on the host, as its own user, optional | Keeps the record append-only ([security](security.md)) |
| `zapper-inspect` | on the host, when you type a command | Reads past runs from the state directory, read-only; the daemon needn't run |

All decisions are made in the daemon. The agent in the guest asks; it doesn't decide.

## How it works

### Starting a session

What happens when a session starts, for each worker at `zapper firm up` or for `zapper run`:

1. The daemon is asked for a new session with a profile: by `firm up` for each worker and facility, or by the CLI.
2. The daemon gives the session a directory, `state/sessions/<id>/`.
3. It gives the session its own copy of the image, which becomes the guest's system disk. Folders the profile mounts are copied into disks of their own: the guest works on that copy, and `zapper cp` brings results back.
4. It starts Firecracker through `zapper __vmm`, which hides the rest of the host from it ([security](security.md#the-vmm)).
5. The guest boots. Its first process is `zapper-agent`.
6. The agent connects to the daemon and receives the setup: user, environment, files, disks.
7. The agent sets up the guest and reports ready.
8. A terminal is opened to the workload: for a worker, by the foreman, which starts its agent headless through the agent's harness, Claude Code or pi ([the office](#the-office), [agent harnesses](configuration.md#agent-harnesses)); for `zapper run`, by the CLI.

When the workload exits, the daemon stops the VM and deletes its disks. The audit log and the terminal recordings stay.

### The session directory

```
state/sessions/<id>/
  audit.jsonl        every event of the session
  shell-1.cast       terminal recording
  console.log        boot messages
  vm/                the only part Firecracker can see; deleted at the end
    root.ext4          the guest's disk, a copy of the image
    disk0.ext4         a copy of a mounted host directory
    vm.json            Firecracker's configuration; kept, with metrics.json, next to the audit log at the end
    v.sock, nic.sock   sockets between guest and daemon
```

### How the guest talks to the host

The guest and the daemon use vsock, a direct channel between a VM and its host. It doesn't go through any network.

| Channel | Opened by | Used for |
|---|---|---|
| Control | the agent, once at boot | Setup, "may I run this program?" questions, events, shutdown |
| Terminal | the daemon, once per terminal | Your keystrokes in, the workload's output out |
| Network | the guest, per connection | The guest's internet traffic ([network](network.md)) |

The daemon accepts one control connection per session. After that the socket is removed, so a workload can't pretend to be the agent.

The CLI talks to the daemon over a unix socket that only your user can open. The web UI, for watching firms and sessions, the board's writes and answering approvals, uses `127.0.0.1:7878`. Both are described on the [API](api.md) page.

### Checking each program

Before the workload starts, the agent installs a filter in it that the workload can't remove. Every child process inherits it. Each time the workload starts a program:

1. The kernel pauses the workload.
2. The agent reads which program it wants to run, and sends that to the daemon.
3. The daemon checks the `exec` rules. With `ask`, it waits for your answer.
4. The agent lets the program start, or refuses it.

If the very first command is refused, the agent prints why and exits with code 126.

### The guest's agent

`zapper-agent` doesn't live in the image. The daemon packs it into a small boot file (an initramfs) each time it starts. So any Linux image works without changes, and the agent always matches the daemon's version.

### The office

A firm's workers are sessions with a link to their firm's run ([office](office.md)). The daemon adds to them:

| Part | Where | Job |
|---|---|---|
| Post office | daemon (`office/mail.rs`) | Mail, threads, who owes whom an answer, follow-ups; bodies in the run's journal |
| Foreman | daemon (`office/foreman.rs`), one per worker | Runs the agent headless in the worker's VM over a terminal channel (`claude -p --input-format stream-json`, or `pi --mode rpc`), writes wake prompts, ends a turn when the agent reports it, records the transcript |
| MCP gateway | vsock port 1100 of every VM | The MCP servers registered in the daemon for the session (the office's tools for a firm's workers): a connection names a server on its first line, then carries MCP as on stdio, one JSON-RPC message per line; the caller is the VM it comes from, and each tool call is an `mcp` event ([instrumentation](instrumentation.md#events)) |
| `office` | in the guest: `zapper-agent office mcp` | The MCP server the agent loads: its stdio bridged line by line to the office's server in the gateway; also a command line and Claude Code's stop hook |
| Shared drive | daemon (`office/drive.rs`) | Copies published files out of the VM over a system terminal, by content hash |
| Security | daemon (`office/security.rs`) | Access requests from `ask` rules, officers' decisions as rules at the worker's or role's layer, digests |
| Facilities | daemon (`office/facility.rs`, `product.rs`) | The firm's git repository (`git http-backend`), its cluster VM (k3s), the product port; the proxy routes the firm's names to them |

The cluster VM's agent forwards host vsock connections on ports 6443, 80 and 443 to k3s, and on the dashboard's NodePort (30466) to Headlamp; only the host may connect.

## Configuration

See [configuration](configuration.md) and [policy](policy.md).

## Verification

`make test` checks the rules and the protocols without starting VMs. `make test-e2e` starts real sessions and checks each part, then firms ([testing](testing.md)).

## Limitations

- Firecracker can't share folders with the host. Mounts are copies, and `zapper cp` copies results back out.
- More: [security](security.md#limitations), [network](network.md#limitations).

## References

- Firecracker vsock: https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md
- Sources: `crates/zapper/src/daemon/`, `crates/zapper/src/vmm.rs`, `crates/zapper/src/netns.rs`, `crates/zapper-agent/src/`, `crates/zapper-policy/src/proto.rs`
