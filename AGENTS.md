# Instructions for coding agents

People should start with the [README](README.md). Operating a running daemon (starting, attaching to and stopping sessions, answering approvals, changing rules, reading audit logs, and running firms of The Office) is covered by the agent skill [skills/zapper/SKILL.md](skills/zapper/SKILL.md); keep it in step with the CLI and docs.

The project is zapper: AI agents in Firecracker microVMs, with what they reach, which credentials they use and which programs they start decided on the host; the daemon, the `zapper` CLI and the sandbox. All guest traffic is captured outside the VM and goes through policy on the host. Credentials are either injected by the host or handed to the guest, per secret. Every exec waits for the host's decision (enforced in the guest, through seccomp), and everything is recorded. Rules are layered (session → profile → global) and managed through a REST API with a running/startup config like a router's.

The Office, built on it, is a separate layer in the same repository ([docs/the-office.md](docs/the-office.md)): firms of AI workers (CEO, product manager, managers, engineers, designers, researchers, analysts, security officers), each Claude Code (or pi) in its own VM, who mail each other, wake on mail, publish to a shared drive, push to the firm's git repository and deploy to the firm's own k3s cluster; security officers watch traffic and decide access requests; the operator is the board ([docs/office.md](docs/office.md)).

## Setting zapper up for a user

### 1. Check the host (no changes)

```
make deps && make build && target/release/zapper check     # every line "ok", or the fix
```

`check` reports `/dev/kvm` access, Firecracker, the guest kernel, the agent binary, images per profile, unprivileged user and mount namespaces (for VMM confinement), `ip` and `nft` (for NICs), podman and e2fsprogs, config permissions and whether the daemon is up. Report what is missing and let the user decide.

### 2. Ask for root once, with reasons

zapper itself never needs root. Only host prerequisites may:

| Command | Why it needs root |
|---|---|
| `sudo usermod -aG kvm $USER` | `/dev/kvm` access, if `check` reports none (then log in again) |
| `sudo apt install podman e2fsprogs` | building images, if missing |
| `sudo apt install iproute2 nftables` | guest NICs, if missing |
| `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` | VMM confinement, if user namespaces are restricted |
| `sudo scripts/install-keeper.sh` | An append-only record kept by a separate user (`zapper-keeper`, a confined systemd service), if the user wants it; then add the `daemon.keeper` lines it prints |

Never run `sudo` without the user's explicit consent, and run only what you listed.

### 3. Build

```
make deps                       # Firecracker (checksum-verified) and kernel
make install                    # zapper, zapper-agent and zapper-inspect into ~/.local/bin
make image                      # ubuntu-26.04: the base profiles, and a plain session to check the setup
make image IMAGE=office         # Claude Code and pi: `zapper run -p claude`, and The Office's workers
```

Run zapper from the repository's root: it uses `config/zapper.yaml`, tracked in the repository, whose paths are relative to it or under `~/`. Downloads, images, kernels and state go under `~/.local`, the Claude token under `~/.config/zapper/`; nothing generated goes into the repository. Outside a checkout, `zapper init` copies the config to `~/.config/zapper/zapper.yaml` and the workers' skills to `~/.config/zapper/skills` (firms have no built-in fallback).

Run image builds in the background with a log (`make image > /tmp/zapper-image.log 2>&1`) and give the user `tail -f /tmp/zapper-image.log`.

### 4. Hand over a running zapper

The agent sessions need the Claude token, which the user creates and stores themselves; never ask for it, read it or print it. Give them these steps ([configuration: model credentials](docs/configuration.md#model-credentials-claude-and-openrouter) has the details):

1. On any machine with Claude Code and a browser (their laptop will do), `claude setup-token`: it signs in and prints a long-lived OAuth token (`sk-ant-oat…`) for their Claude subscription.
2. On the zapper host, without the token reaching shell history: `mkdir -p -m 700 ~/.config/zapper && (umask 077; read -rsp 'Claude token: ' t; printf '%s' "$t" > ~/.config/zapper/claude-token); echo`
3. `zapper check`. No restart is needed: the host reads the file on each request.

Start `zapper serve` (or tell the user to) and give them the UI URL it prints. `zapper run -C $(mktemp -d) -- cat /etc/os-release` checks that a plain session boots (`-C` gives it an empty directory instead of a copy of the checkout). `zapper run -p claude --keep`, in a project's directory, is Claude Code in a sandbox on a copy of it: the Claude API allowed, every other host an `ask` the user answers (`zapper approvals`, `zapper approve`, or the UI's sessions view); `zapper cp ID:/work ./out` copies the work out. Answer approvals only as the user says.

### 5. The Office, if the user wants it

A firm needs the cluster's image and kernel too:

```
make image IMAGE=k3s && make kernel KERNEL=k8s   # the firm's cluster and its kernel
```

The red-team scenario's contractor runs on pi with DeepSeek through OpenRouter, and so do the other `office-*` profiles on pi. If the user wants them, they create a key at https://openrouter.ai/settings/keys (`sk-or-v1-…`; credits, or `:free` models only; if the key is limited to some models, it must allow the ones those profiles name), and store it like the Claude token: `(umask 077; read -rsp 'OpenRouter key: ' t; printf '%s' "$t" > ~/.config/zapper/openrouter-token); echo`.

With the user's go-ahead (a firm spends their Claude usage), the user starts the company in the UI's Kickoff tab (the company, its CEO, charter terms, their own articles), or `zapper firm up acme` starts the example firm with its CEO idle; the user gives the CEO its first task there, or with `zapper mail send acme --to barbara --kind assignment …` (barbara is the CEO). The CEO then asks for a budget, which only the user approves (`zapper firm budget acme approve b-1`, or Kickoff); never approve budgets, background checks or access requests on the user's behalf.

### A remote host

If zapper runs on a machine other than the user's, the UI (`daemon.ui`, `127.0.0.1:7878`) and a firm's dashboard (`dashboard_port`, `127.0.0.1:8081`) are on that machine's loopback. Offer, in this order:

1. An SSH tunnel from the user's machine, no config change: `ssh -L 7878:127.0.0.1:7878 -L 8081:127.0.0.1:8081 user@host`, then the URL `zapper serve` printed.
2. Serving them to the network: `daemon.ui: 0.0.0.0:7878` and the firm's `dashboard_port: 0.0.0.0:8081` in the config, then restart the daemon (see Rules). Say plainly before changing it: whoever has the token controls zapper, and the dashboard's token is cluster-admin; it is plain HTTP, for a trusted network or behind a TLS proxy; zapper isn't built for production. The VMs can't reach either way. [Configuration: a remote host](docs/configuration.md#a-remote-host) has the details.

### Rules

- Sessions and firms may be in use: before `zapper rm`, `zapper stop`, `zapper firm down`, `zapper config reload` or restarting the daemon, check `zapper ps`, `zapper firm ls` and `zapper config diff` and say what you are about to change. Unsaved running config is lost on restart, and a restart or `firm down` ends a firm's run, which can't be resumed.
- `zapper config save` writes the user's startup file: run it when the user asks to keep a change.
- Do not print `zapper config startup` output into chats when it may hold inline secrets; values are redacted, but names and sources are not.
- The daemon's `state` directory holds audit logs and terminal recordings of sessions, and firms' runs (mail, transcripts, published files): treat them as sensitive and do not delete them unasked.

## Working on the repository

Architecture: [docs/architecture.md](docs/architecture.md). Network: [docs/network.md](docs/network.md). Security model and the controls that must hold: [docs/security.md](docs/security.md).

```
crates/zapper-policy/    config model, layered rule evaluation, host↔guest protocol (no I/O)
crates/zapper-core/      the daemon: sessions, proxy, DNS, services, the MCP gateway, approvals, the driver, the record; its interface `Core`, the only way in for the crates below
  src/interface.rs         `Core`: sessions, config, rules, approvals and deciders, services, MCP servers, events, what clients register
  src/vmm.rs               `zapper __vmm`: runs Firecracker confined in its own namespaces and root
  src/netns.rs             the guest's network in the VMM's namespace, and traffic capture
  src/daemon/harness.rs    agent harnesses: Claude Code and pi behind `Harness`/`Driver`/`Conversation`; HARNESS.md for both, and opencode
  src/daemon/session.rs    session lifecycle, guest control channel, exec decisions, recordings
  src/daemon/proxy.rs      egress proxy: tunnels, TLS inspection, secret injection
  src/daemon/services.rs   names the daemon connects itself (a firm's git and cluster), zones
  src/daemon/gateway.rs    the MCP gateway: servers registered in the daemon, over each VM's MCP port
  src/daemon/addr.rs       which destination addresses are reachable at all
  src/daemon/dns.rs        the guest's resolver: synthetic addresses for allowed names
  src/daemon/httplog.rs    per-session request log for the UI's network view
  src/daemon/vm.rs         disks, initramfs, the Firecracker process
crates/zapper-api/       the REST API (the CLI's, on a unix socket) and the web UI, over `Core`; mounts what it is given (the office's routes and page)
  src/ui.html              the web UI: state, terminal viewer, answering approvals (textContent only)
  src/assets/              xterm.js 6.0.0 (MIT) for the terminal viewer; see its README
crates/zapper-office/    The Office, a client of `Core`: post office, foreman (headless Claude), shared drive, security officers, facilities; the `firms` config (`firm.rs`), which it parses and checks
  src/office.html          the office UI (org chart, messages, security, drive, statistics, replay)
crates/zapper/           the CLI and `serve`: starts the core, the office (feature `office`) and the API and UI
crates/zapper-inspect/   `zapper-inspect`: past runs read from disk, their people and a scenario's operatives, transcripts exported; a minimal TUI
crates/zapper-keeper/    the record's keeper: a separate user's append-only store with hash chains and signed checkpoints, its read-only UI
crates/zapper-agent/     guest PID 1: setup, terminals, seccomp exec supervision, accounting; `office` (MCP server, CLI) and the fake Claude and pi
config/                  zapper.yaml: the config, used when zapper runs in the checkout (relative paths resolve against it);
  skills/                  office workers' skills: built in, and read live through the acme firm's `skills_dir: skills`
  kernel/                  kernel config fragments (`make kernel KERNEL=k8s`)
scripts/fetch-deps.sh    Firecracker and kernel download
scripts/check-js.sh      syntax of the pages' scripts (needs node; CI runs it)
tests/e2e.sh             end-to-end tests against real VMs; tests/attack.c probes from inside
tests/e2e-office.sh      end-to-end tests of firms, with the fake Claude
docs/                    one page per topic; index in docs/README.md
skills/zapper/           operating skill, linked from .claude/skills/zapper
```

Build and test:

```
make build        # release: static musl agent + host binary
make test         # unit tests, no VMs, seconds
make lint         # rustfmt (defaults, rustfmt.toml) and clippy with warnings as errors; `make fmt` formats
make test-e2e     # real VMs, network; must end "all e2e checks passed" and "all office e2e checks passed"
```

Invariants (a change that breaks one is wrong even if it compiles; [testing](docs/testing.md#invariants-and-their-tests) lists the test for each, and what no test covers yet, which a change to that code should add):

- The VMM runs in its own user, network and mount namespaces; its root holds the session's `vm/` directory, the kernel, initramfs, its binary and its devices.
- Guest traffic goes through zapper, also with root in the guest. With a NIC, the VMM's network namespace has loopback and the tap as its interfaces and sends all TCP and DNS to the daemon; in vsock mode, vsock is the guest's only channel to the host.
- DNS queries and host names from the guest are validated before policy sees them. DNS looks a name up on the host only if some request to it could be allowed (outright or after an `ask`), refuses other names, and never makes up an answer. `ask` decides access, not resolution.
- `proxy` secret values stay on the host. The proxy adds a value to a request when a rule listing the secret allows it, the destination matches the secret's `hosts`, and the connection uses TLS or the secret sets `plaintext: true`. A `guest` secret goes to sessions whose profile uses it.
- The proxy connects to the address it checked. Loopback, link-local and host addresses are unreachable, and private ranges need `daemon.private_networks`.
- Passthrough connections are bound to the TLS server name; inspected ones present a per-session CA and override `Host`.
- The host accepts only the first control connection; the agent accepts terminal connections only from CID 2.
- The seccomp filter kills foreign-arch and x32 syscalls, so exec checks cannot be bypassed through another syscall table.
- Anything from the guest is bounded (line, frame, connection and approval limits) and rendered as text in the UI.
- The UI's writes are answering approvals and the board's firm actions (mail and marking it read, start/stop/pause, security decisions and promoting them into the running config, background checks, credential grants and decisions, budget decisions, funding and releasing workers, drive uploads, deleting a past run, notes on runs); each requires the token cookie and `X-Zapper-UI: 1`; on loopback (the default) a local `Host` and `Origin`, and listening elsewhere (`daemon.ui`, for a remote host) an `Origin` naming the `Host` the request came to. Whatever the UI and the dashboard listen on, the VMs can't reach them: the host's own addresses are closed to them.
- In a firm, the sender of every office request is the VM it came from; roles and the chart are checked on the host; an `ask` is refused at once and decided by officers or the board, never past `final` rules; the firm's service names are routed by the daemon and never resolved on the host.
- In a firm, security is always held (by officers, else the CEO, else the board), nobody decides their own access request or their own hire's background check, a hire takes no turn before being cleared, and a hired officer decides nothing before being cleared. The charter's articles are the same for every run; only their terms and the board's own articles change. A contractor mails only their hirer and those the hirer added, has no authority, and a contract moves money only from the hirer's balance. Access to the firm's resources (git, the cluster, drive spaces) is a grant decided by the policy owners (officers and the CEO, never the requester), enforced on the host or by the cluster's RBAC, expiring, and revoked when the worker leaves; no role gets access by default.
- A config change is validated as a whole and applied atomically, or not at all.
- With a keeper, the record is appended through it and nothing else: the keeper takes appends from the configured uid only, has no operation to read back, rewrite or remove, flushes each line before acknowledging, and the daemon takes no firm turns while the record can't be written.

Applying changes: rebuild with `make build` and restart the daemon; sessions do not survive a restart (stop them first, see Rules). Rule and profile changes need no restart: they apply to running sessions at once.

Code style: rustfmt's defaults and a clean clippy. `make hooks` turns on the pre-commit hook in `.githooks/`, which runs both; keep `make lint` passing.

Markdown: never hard-wrap prose; one line per paragraph or list item.
