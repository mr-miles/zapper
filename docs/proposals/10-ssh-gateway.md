# SSH into a session: a host-side gateway

An operator reaches a running session with `ssh`, `rsync` and VS Code Remote-SSH, as if it were a machine called `ID.zapper`, and moves files with `scp` and `sftp` to and from a drive on the host, `drive.zapper`, which guests reach under policy ([proposal 11](11-core-drive.md)). There is no sshd in the guest and no new listener on the network: the SSH protocol ends in the daemon, which carries each SSH channel to the guest over the vsock channels that `zapper attach` already uses. Every shell is recorded, every program it starts is checked against the session's `exec` rules, and nothing the SSH client asks for opens a new way out of the VM.

## Today

- The host reaches a guest only over vsock. The guest agent accepts terminal connections on `SHELL_PORT` (1024) from the host's CID only (`zapper-agent/src/shell.rs`). A connection starts with a `ShellRequest` line (`argv`, optional `tty` size, `env`, `system`), then carries `Frame`s: `Data`, `Stderr` (without a tty), `Resize`, `Eof`, and `Exit` as the last frame (`zapper-proto/src/proto.rs`).
- `GET /v1/sessions/{id}/attach` on the CLI's unix socket upgrades to that frame stream (`zapper-api/src/lib.rs`, `attach`), and the daemon pumps it, recording a terminal as `shell-N.cast` and emitting `shell` events (`daemon/session/terminal.rs`). `zapper attach` and `zapper exec` are built on it. The workload's programs, a shell's among them, run as uid 1000 under seccomp exec supervision.
- `zapper cp ID:/path DIR` copies out, as a `system` shell running `tar`. Nothing copies in.
- The agent forwards host vsock connections to fixed guest TCP ports listed at boot (`Forward`, `engine.ports`; `zapper-agent/src/forward.rs`, `inbound`), from the host's CID only. These ports serve [services](../network.md#services) to other sessions; nothing reaches them from the operator's machine.

So the operator has a terminal, one-shot commands and copying out, but no SSH: VS Code Remote-SSH, `scp`/`sftp` and `rsync` don't work.

## What the clients need

| Client | SSH features it uses |
|---|---|
| `ssh ID.zapper` | `session` channel, `pty-req`, `shell`, `window-change`, `exit-status` |
| `ssh ID.zapper CMD`, `rsync`, `scp -O` (legacy) | `session` channel, `exec` with binary-safe stdin and stdout, stdin EOF, `exit-status` |
| `scp` (OpenSSH 9+), `sftp` | `subsystem sftp`, served here by the drive, not a session |
| VS Code Remote-SSH | `exec` (`bash` with its install script); downloading its server, in the guest or uploaded (`remote.SSH.localServerDownload`); then `direct-tcpip` to the server's port on the guest's loopback (also as a SOCKS forward, `-D`). Linux x86-64 with glibc 2.28 or later: the zapper images qualify |
| Rarely wanted, refused | `auth-agent-req` (agent forwarding), `x11-req`, `tcpip-forward` (`-R`), `direct-tcpip` to anything but the guest's loopback |

## Options

### A. Without changing zapper: VS Code tunnels or code-server

`code tunnel` in the guest connects out to Microsoft's relay, and the editor connects through it, with a GitHub or Microsoft sign-in. It works today with net rules for the relay and the server download. But it hands control of the session to a third-party service and an outside account: input reaches the guest through the relay, not through zapper, so nothing is recorded as a terminal, and anyone with that account can drive the session. It also needs a broad allow to Microsoft's hosts for the session's whole life. code-server (VS Code in a browser) runs in the guest, but the operator's browser can't reach it, since nothing forwards a guest port to the operator. It still needs part of option C.

Not recommended for untrusted workloads. It's worth a line in the docs as what *not* to do.

### B. sshd in the image, reached through a vsock pipe

Install `openssh-server` in the image, start it at boot, list port 22 in `engine.ports`, and add `zapper connect ID:22`, a stdio-to-vsock pipe, for `ProxyCommand`. Small to build: one API route and a CLI command over the existing `Forward`.

Against it:

- **It escapes exec supervision.** sshd would be started at boot as root, outside the workload. The agent installs the seccomp filter only on the workload's process tree, so programs started from SSH logins are neither checked nor counted. Making sshd the workload, or filtering it, would turn exec checks on sshd's own privilege separation.
- **zapper sees nothing.** SSH is encrypted end to end between the client and the guest, so the host can't record terminals or know what a channel is for. Agent forwarding and `-R` would be up to sshd's config inside a VM the workload may control as root.
- Every image needs sshd, host keys and an authorized key. That goes against "any Linux image works unchanged", and with snapshots one day, against unique host keys.

Not recommended.

### C. A host-side SSH gateway (recommended)

The daemon speaks SSH itself, on a stream the CLI opens over its unix socket, and maps each channel the client opens onto what zapper already has: a shell channel becomes a guest terminal, `exec` a guest command without a tty, and `direct-tcpip` a new, narrow forward to the guest's own loopback. SFTP goes to the core drive on the host, not to a session. The SSH client sees one ordinary server per session, and one more, `drive.zapper`, for files. The only guest change is the loopback forward.

The rest of this proposal designs C.

### D. C, plus a TCP listener

The same gateway, also listening on `127.0.0.1:2222` with the session ID as the user name (`ssh -p 2222 ID@localhost`). This only matters for clients that can't run a `ProxyCommand`, which Remote-SSH, OpenSSH, `scp` and `rsync` all can. It adds a listener and its own client authentication. Leave it out unless a client needs it.

## Design

### Reaching the gateway: `ProxyCommand`, no listener

```
zapper ssh-proxy ID                # stdio ↔ the session's SSH gateway
zapper ssh-config                  # prints the block below, for ~/.ssh/config
```

```
Host *.zapper
    ProxyCommand zapper ssh-proxy %h
    HostKeyAlias zapper
    User user
    StrictHostKeyChecking yes
    UserKnownHostsFile ~/.config/zapper/known_hosts
```

`zapper ssh-proxy` opens `GET /v1/sessions/{id}/ssh` on the daemon's unix socket and upgrades it to a raw byte stream, as `attach` upgrades to frames. The daemon runs the SSH server's side of the protocol on that stream. `%h` is `ID.zapper`; the CLI drops the suffix. `zapper ssh-proxy drive.zapper` opens `GET /v1/drive/ssh` instead, where the gateway serves only `subsystem sftp`, on the drive.

- **No new listener.** Who may use the gateway is who may open the CLI's socket, which is the daemon's user, exactly as for `zapper attach`. The gateway grants nothing `attach` doesn't already.
- **Client authentication** is therefore the socket's. The gateway accepts the `none` method, and also any `publickey` the client offers, since some clients insist on one. It never reads the operator's keys.
- **Host key.** One Ed25519 key for the daemon, generated on first use into `~/.config/zapper/ssh_host_ed25519` (mode 0600). `zapper ssh-config` writes its public half to `~/.config/zapper/known_hosts` under the alias `zapper`, so every session matches one entry and VS Code doesn't ask about each new ID. Nothing secret depends on it: the transport is a local socket.
- **A remote zapper host** chains over the operator's existing SSH: `ProxyCommand ssh user@zapper-host zapper ssh-proxy %h`. The outer SSH authenticates the operator; zapper still opens nothing.

### Library

`russh` (server side; Apache-2.0, tokio, pure Rust, used in production) is the candidate. SSH parsing is in the daemon, the process that holds the secrets. The input comes from the operator, not a guest, which keeps it out of the threat model's adversary. It still gets the usual bounds, a fuzz target and a pinned version. A hand-written SSH server is out of the question.

### Channels

| SSH request | In the guest | Recorded |
|---|---|---|
| `session` + `pty-req` + `shell` | A terminal: `ShellRequest { argv: [], tty: Some(size) }`, the profile's command, as `attach` | `shell` events and `shell-N.cast`, as `attach` |
| `session` + `pty-req` + `exec CMD` | A terminal running `["/bin/sh", "-c", CMD]` | As above, with argv |
| `session` + `exec CMD` (no pty) | `ShellRequest { argv: ["/bin/sh", "-c", CMD], tty: None }`: stdout as `Data`, stderr as `Stderr` | A `shell` event with argv and exit code; no cast (binary data). Each program it starts is an `exec` event as always |
| `session` + `exec "scp -t …"` / `"scp -f …"` (legacy `scp -O`) | Refused, with a message pointing at `drive.zapper` | |
| `window-change` | `Frame::Resize` | |
| Channel EOF / close | `Frame::Eof` / close the guest connection | |
| `Frame::Exit(code)` | `exit-status` (a signal as `exit-signal`), then close | |
| `env` | Kept in `ShellRequest.env` for `LANG`, `LC_*` and `TERM` only; others dropped | |
| `subsystem sftp` | Refused on a session; served by the drive on `drive.zapper` (below) | |
| `direct-tcpip` to `127.0.0.1`, `::1` or `localhost`, port P | A new guest forward to loopback port P (below) | A `forward` event: port, bytes each way, duration |
| `direct-tcpip` elsewhere | Refused (`administratively prohibited`) | A `forward` event, refused |
| `tcpip-forward` (`-R`), `auth-agent-req`, `x11-req`, `keepalive@openssh.com` | Refused, except the keepalive, answered | |

Everything opened from SSH runs as the workload user under exec supervision, never as `system`. `system` stays for the host's own maintenance, such as `zapper cp`.

### Files: the drive, not the guest

`scp` (OpenSSH 9+) and `sftp` speak SFTP. The gateway doesn't serve SFTP into a session. It serves it for one host, `drive.zapper`, straight from the core drive's directory on the host ([proposal 11](11-core-drive.md)): `scp report.pdf drive.zapper:inbox/`, `sftp drive.zapper`. Guests then read and write the drive over HTTPS, as the drive's rules allow.

Two ways of serving SFTP into a guest were weighed and dropped:

- **Passing the subsystem to the image's `sftp-server`**, run as the workload user, as sshd does. It needs no guest change, but it needs `openssh-sftp-server` in every image. The operator's files land inside the VM, where every program in it can read them, and revoking access later doesn't remove them.
- **Serving SFTP on the host by running guest commands** (`dd`, `stat`, `find`, `mv`) per request. This is slow (a process and an exec check per chunk), floods the audit log with `exec` events, and depends on GNU coreutils' flags. Running it as `system` to skip the checks would run as root past `fs` rules.

With the drive, files the operator adds never touch a guest until a rule lets a guest fetch them, and files a guest writes are checked by the host, not extracted from an archive the guest made. Legacy `scp -O`, which runs `scp -t` in the remote shell, is refused for the same reason and pointed at the drive.

### Port forwarding to the guest's loopback

VS Code's server listens on a random port on the guest's loopback, so the fixed `engine.ports` forwards can't serve it. Add one vsock port to the agent, `LOCAL_FORWARD_PORT` (1200). A connection from the host's CID starts with one bounded line, `{"port": P}`. The agent connects to `127.0.0.1:P` and splices, as `forward::inbound` does. It refuses other CIDs, as every agent listener does, so a guest process can't use it over vsock loopback.

This adds no way out of the VM. The connection starts on the host, ends on the guest's loopback, and never touches the NIC or the proxy. The guest gains nothing: it could already reach its own loopback. `-R` (the guest connecting back to the client) would be a way out that skips the proxy, and is refused. So is `direct-tcpip` to any other address: a forward to the internet "through the guest" would be a way to make the guest look like the source of traffic that zapper's policy never saw.

### VS Code Remote-SSH, end to end

1. The operator adds the `zapper ssh-config` block once and starts a session with `--keep` (Remote-SSH reconnects, and the session must outlive the first connection).
2. "Connect to Host… `ID.zapper`" runs `zapper ssh-proxy ID.zapper`. The gateway answers; the host key matches the `zapper` alias.
3. VS Code runs its install script over `exec`. It downloads the server, either in the guest, which needs net rules for the download hosts, or on the operator's machine with `"remote.SSH.localServerDownload": "always"`, then uploaded over the same channel. The second needs no net rule and is the one to document.
4. The server starts under `~/.vscode-server/`. Every program it and its extensions start (`node`, `git`, `rg`, language servers) is exec-checked. A `vscode` profile in the shipped config allows `~/.vscode-server/**` and the tools it runs, so it doesn't flood the operator with `ask`s. The profile extends the operator's usual one.
5. The editor talks to the server through `direct-tcpip` to its loopback port.
6. The extension marketplace, from the editor's side, needs nothing from the guest. Extensions that run in the guest and fetch things go through the proxy under the session's rules, like anything else.

A `vscode` profile with these rules, and a short page in the docs, are part of the work. Which hosts the in-guest download needs is settled by testing, not guessed here.

### Limits

From the SSH client: at most 16 channels per connection and 8 connections per session; a forward's first line within `MAX_LINE`; SFTP requests within the frame limit, and a packet longer than 256 KiB refused. A gateway connection ends when its session ends. A session doesn't end when its SSH connections do: that is still `--keep` and `zapper stop`.

## Invariants

The gateway keeps every invariant in [AGENTS.md](../../AGENTS.md). In particular:

- The agent still accepts terminal and forward connections only from CID 2; the gateway is on the host.
- Guest traffic still goes through zapper: the only new forward goes from the host to the guest's loopback, and the ways an SSH client could open a path out of the VM (`-R`, agent forwarding, `direct-tcpip` elsewhere) are refused.
- The VMs still can't reach the operator's interfaces: there is no new listener, and the gateway is on the CLI's socket.

New invariant: **SSH channels map only onto host-initiated guest connections run as the workload user; the gateway refuses every request that would let the guest reach the operator's machine or the network.** `tests/e2e.sh` covers it: `-R`, `-A` and `direct-tcpip` to a public address are refused, and a shell over SSH is recorded and exec-checked like one over `attach`.

## Phases

1. **Shell and exec.** `zapper ssh-proxy`, `zapper ssh-config`, the `/ssh` route, the host key, `session` channels with and without a pty, `env`, exit status, refusals. `ssh`, `rsync` and `scp -O` work.
2. **Files.** The core drive and `drive.zapper` over the gateway ([proposal 11](11-core-drive.md)). `scp` and `sftp` work, to and from the drive.
3. **Forwarding and VS Code.** `LOCAL_FORWARD_PORT`, `direct-tcpip` to loopback, `forward` events, the `vscode` profile, the docs page. VS Code Remote-SSH works.
4. **The UI.** SSH shells already appear as terminals; show forwards and SFTP writes in the session view.

## Open questions

- Whether `ask` should apply to the gateway itself: an operator's own SSH needs no approval, but in a firm, the board's SSH into a worker could be an `ssh` event the security officers see.
- Whether `exec` channels without a tty should keep a capped record of their stdin and stdout, as casts do for terminals. Useful for audits, but VS Code's channels carry a lot of traffic.
- A Windows or macOS VS Code that can't run `zapper` locally still works through the remote-host `ProxyCommand` form, but this needs testing.
