[← README](../README.md) · [Docs](README.md) · **Security model**

# Security model

## Overview

Each office worker runs in a zapper session, and zapper assumes that whatever runs in a session may be hostile, up to a root shell in the guest. The agent inside the guest is treated the same way. Controls that have to hold are enforced on the host. Controls enforced inside the guest are listed separately, as guardrails.

## Threat model

| | |
|---|---|
| Adversary | The session's workload: a worker's agent (Claude Code or pi) following injected instructions, from a web page or another worker's mail, or anything trying to break out, possibly with root in the guest. |
| Assets | The host, where the daemon starts VMs and reads secrets. Other sessions, including the other workers of a firm. Networks the host can reach, such as management networks, cloud metadata and the host's own services. The values of `proxy` secrets. |
| Goal | The session can do what its policy grants for as long as it lives. Its actions are recorded on the host. |
| Out of scope | A malicious operator. Compromise of the host by other means. KVM and Firecracker vulnerabilities, which are handled by keeping them up to date. What a credential grants and for how long: `guest` secrets are given to the workload by design, so the issuer scopes them and sets their expiry. |

## What is enforced where

| Control | Enforced by | Holds with root in the guest |
|---|---|---|
| The VMM runs in its own user, network and mount namespaces, in a read-only root holding its own files: the session's `vm/` directory, the kernel, the initramfs, its binary and its devices (three, four with a NIC) ([the VMM](#the-vmm)) | `zapper __vmm` on the host | Yes |
| Guest traffic goes through zapper. With a NIC, the VMM runs in a network namespace whose only interfaces are loopback and the guest's tap, and nftables there sends TCP to the daemon and DNS to its resolver and drops the rest. In vsock mode the guest has loopback only ([network](network.md)) | Network namespace and nftables outside the VM; Firecracker's device model | Yes |
| DNS queries are validated (standard query, one question, class IN, a well-formed name) and rate-limited. A name is looked up on the host only if some request to it could be allowed, outright or after an `ask`; others are refused. Answers are never made up. With a broad `allow` or `ask` (`*`, or such a default), any name reaches the host's resolver, so queries could carry data out ([network: DNS](network.md#dns)) | Host resolver | Yes |
| Host names from the guest (`CONNECT` targets, `Host` headers, TLS server names) are validated before any rule sees them | Host proxy | Yes |
| A socket service reaches only a socket in the daemon's `state/services/` directory, named by file name; the program behind it learns the calling session from a line the daemon sends before the guest's bytes ([network: socket services](network.md#socket-services)) | Host | Yes |
| Policy for each connection and, when inspected, each request | Host proxy | Yes |
| Address checks: loopback, link-local, metadata and host addresses are unreachable, private ranges only if listed. The host resolves names and connects to the address it checked | Host proxy | Yes |
| A TLS client's server name must equal the name the connection was decided on | Host proxy | Yes |
| `proxy` secrets reach the guest as placeholders. Their values go into allowed requests to the secret's `hosts`, over TLS, or over plain HTTP if the secret sets `plaintext: true`, and are masked in the responses to them. `aws` secrets' keys sign allowed requests on the host; the guest holds a fake key | Host proxy | Yes |
| An inspected request is judged on the path the upstream will act on (decoded and with dot segments resolved, and forwarded so), a path that upstreams may read differently is refused, and so is a request whose override header or parameter names another method ([network: connections](network.md#connections)) | Host proxy | Yes |
| Each session has its own CA, kept in memory and trusted by that session's guest | Host | Yes |
| Host directories are copied into per-session disks, so host files stay unchanged | Host | Yes |
| Lifetime (`ttl_secs`), CPU, memory and disk size | Host, Firecracker | Yes |
| Limits on input from the guest: line and frame sizes, connections per session, concurrent exec checks, pending approvals | Host | Yes |
| The host accepts one control connection, which the agent makes before the workload starts, then removes the socket | Host | Yes |
| Audit log, terminal recordings, console log (capped at 1 MiB) | Host | Yes |
| With a keeper ([the record](#the-record)), the record can only be appended to by the daemon's user: firms' journals and transcripts, audit logs and terminal recordings belong to `zapper-keeper`, a separate user | `zapper-keeper`, OS users | Yes |
| Each `execve` and `execveat` of the workload waits for the host's decision | Guest agent (seccomp user notification) | No |
| The 32-bit and x32 syscall tables are blocked, so exec checks can't be bypassed through them | Guest agent (seccomp filter) | No |
| The workload runs as uid 1000 with `no_new_privs`, so setuid binaries keep its privileges | Guest agent | No |
| Terminal connections are accepted from the host's CID only | Guest agent | No |
| `fs` rules (`hidden`, `ro`) | Guest agent (bind mounts at boot) | No |
| Process exit accounting | Guest kernel (`acct`) | No |

The guest-side controls hold while the workload is unprivileged. A profile with `agent.root: true` gives them up, and the UI marks its sessions. A guest kernel exploit also defeats them. The host-side controls hold in both cases.

## The record

What happened is kept in the record: firms' journals and transcripts, sessions' audit logs and terminal recordings. Without a keeper, the daemon's user writes it under `state`, and whoever controls that user (a compromise of the daemon included) can change it.

With `daemon.keeper` set, the record is kept by `zapper-keeper`, a small service running as its own user (`scripts/install-keeper.sh` sets it up, as root, once):

- **One way.** The daemon reaches the keeper over a unix socket with one operation: append a line to a stream. The keeper accepts it only from the configured user (checked with `SO_PEERCRED`), answers with an acknowledgement and never with data, and has no operation to read, rewrite or remove. Stream names are plain relative paths ending in `.jsonl` or `.cast`.
- **Owned by the keeper.** The files belong to the keeper's user, readable by the daemon user's group (mode 0640) and nobody else. A compromised daemon can add lines; what is already there stays.
- **Durable.** Each line is on disk (`fdatasync`) before the daemon hears back; new files and directories are flushed too. After a crash, lines the chain doesn't cover yet are chained on restart.
- **Tamper evident.** Next to each file, `<file>.chain` holds a hash per line, each over the line and the hash before it, and every 64 lines (or 30 s) a checkpoint signed with an Ed25519 key only the keeper can read. `zapper record verify FIRM RUN`, the keeper's UI and `zapper-keeper verify` check a file against its chain and the public key in `keeper.pub`, so changes made around the keeper (by root, or on a copy) show.
- **Fail closed.** The daemon doesn't start if its keeper doesn't answer; if writing the record fails while it runs, firms take no turns until it works again. The keeper refuses appends when its disk has less than 1 GiB free, so a flood of appends can't fill it.
- **Confined.** The keeper runs under systemd with no capabilities, a read-only system except its own directories, and a system call filter; its binary is root-owned in `/usr/local/libexec`, so the daemon's user can't replace it. Its UI is read-only, on loopback, behind a token readable by the daemon user's group.

If the daemon's user is compromised, everything recorded before stays as it was, and the signed, timestamped checkpoints mark when; what is appended after is the attacker's word. If the keeper's own user or root is compromised, the record can be changed, though `verify` catches changes to anything checkpointed against a saved public key.

## The office

In a firm ([office](office.md)), each worker is a session, so everything above holds for it. The office adds:

| Control | Enforced by | Holds with root in the guest |
|---|---|---|
| A worker's calls of the office's tools come on its VM's own MCP port; the daemon's gateway takes the caller from the VM, never from the message, and serves a session only the servers registered for it | Host | Yes |
| Every office request is checked against the worker's role (officer tools) and the chart (assignments only down it); mail is readable only by the thread's participants | Host | Yes |
| Every hire waits for a security officer's (or the board's) background check before taking a turn; an objection cancels the hire | Host | Yes |
| An `ask` in a firm is refused at once and becomes an access request; officers' decisions add rules only at a worker's or role's layer, under `final` rules; the firm's own services, secrets, private networks and global rules stay with the board; an officer can't decide its own request | Host | Yes |
| The firm's service names are routed by the daemon and never looked up on the host; a VM never reaches another VM or a host address | Host | Yes |
| Facility ports (the cluster's API and ingress) accept connections from the host's CID only | Guest agent of the cluster VM | No |
| Published files: the archive from the guest is parsed on the host, keeping regular files with clean relative paths; links and devices are dropped; sizes are bounded | Host | Yes |
| Published files are served to the UI with their own content type and `Content-Security-Policy: sandbox`; mail and documents are rendered from text | Host, UI | Yes |
| Bounds on what workers send: request line, body, recipients, attachments, mail per hour and per run, fan-out, thread length, artifact and drive size, pending access requests | Host | Yes |
| Skills are attached read-only | Guest kernel (read-only disk) | No |

What the office does not defend: workers are free to persuade, mislead or instruct each other through mail and access-request reasons. That is deliberate ([workers manipulating workers](office.md#workers-manipulating-workers)); the controls above are what holds regardless.

Workers hold credentials for the firm's services only as grants: a git identity whose rights the git server checks on every request, and, with a cluster grant, a token for their own service account with RoleBindings in `dev` (and `prod` for admin). Grants are decided by the policy owners (the security officers and the CEO, never the requester), expire, and are revoked when the worker leaves; revoking deletes the service account, which voids the token. Nobody holds cluster-admin but the daemon (and the board's dashboard). The UI's writes also cover the board's actions: the board's mail and marking it read, firm start/stop/pause, security decisions and promoting them into the running config, background checks, credential grants and decisions, budget decisions, funding and releasing workers, drive uploads, deleting past runs and notes on runs, under the same guards as answering approvals.

## Secrets given to a session can be used by it

Every secret a session receives, in any mode, gives the session the authority that secret carries, for as long as the session runs. zapper can't prevent that, so treat each secret you give a session as obtainable by it.

| Mode | The session can read the value | The session can use the authority |
|---|---|---|
| `guest` (env or file) | Yes | Anywhere the network rules let it connect, during the session and afterwards, if it sends the value out |
| `proxy` | No, unless a destination reflects it back | Through requests that zapper attaches it to: allowed by a rule and sent to one of the secret's `hosts`, during the session only |

`proxy` mode protects the value, so the session can't take the raw token elsewhere or keep it after it ends. It doesn't limit what the token can do at those hosts. Choose the token's own scope (repository, permissions, lifetime) as if the session held it.

## The VMM

A guest that escapes Firecracker gets the Firecracker process. zapper starts each VMM through `zapper __vmm`, which, before Firecracker runs:

1. enters new user, network and mount namespaces, and becomes root inside them without host privileges;
2. for a NIC session, builds the guest's network in the new network namespace ([network](network.md)); in vsock mode, that namespace stays empty;
3. builds a root on a tmpfs, holding bind mounts at their host paths: the session's `vm/` directory (disks, VM config, metrics, sockets), the kernel, the initramfs and the Firecracker binary read-only, `/dev/kvm`, `/dev/null`, `/dev/urandom`, `/dev/net/tun` for a NIC, and a private `/tmp`;
4. `pivot_root`s into it and makes it read-only;
5. with subordinate ids (`daemon.vmm_subids`), starts Firecracker as the session's own subordinate uid, without capabilities and with `no_new_privs`. The daemon maps two ids into the namespace with `newuidmap` and `newgidmap`: 0, the daemon user, who does the steps above and owns the session's files, and 1, a subordinate uid no other running session holds. The VM directory is shared with it through the daemon user's group, and the tap is its own.

From there, the VMM sees none of the daemon user's other files: the config, secret files, the session's audit log and recordings, and other sessions all stay outside its view. `tests/e2e.sh` checks this through `/proc/<pid>/root` of a running VMM.

## Usage

- Run workers, and anything else you don't trust, with `root: false`, the default.
- Use `proxy` secrets for bearer tokens and API keys: the workload can use them but can't read them. Use `guest` secrets for credentials the client has to hold, such as S3 signing keys, client certificates and kubeconfigs, and issue them scoped and short-lived.
- Bind each `proxy` secret to the hosts that need it. A destination that echoes request headers back, such as an echo service, returns the value to the guest; `tests/e2e.sh` shows this with a test secret.
- Use a `final: true` deny in the global layer for destinations that must stay unreachable from every profile.
- Leave `daemon.private_networks` empty unless a session needs a private network, and then list that network only.
- Keep the UI (`daemon.ui`) and a firm's dashboard (`dashboard_port`) on loopback, and reach a remote host through an SSH tunnel. Listening on the network, they are plain HTTP and whoever has the token controls zapper ([a remote host](configuration.md#a-remote-host)). The VMs can't reach them either way: the host's own addresses are closed to them.
- Use inspection rules (`methods`, `path`) when a host-level rule is too broad. A passed-through TLS connection to an allowed CDN host can still carry requests for another site on that CDN in its `Host` header.

## Verification

```
make test-e2e      # ends with "all e2e checks passed"
```

`tests/e2e.sh` checks the host-side controls from a guest, with unit tests for the parsers and address checks, and `tests/e2e-office.sh` the office's: from inside a worker it tries to give its manager an assignment, send as another worker, read another worker's mail, use officer tools, publish a symlink to a host file, and reach the office's tools from outside a firm; a worker without a grant tries the cluster API and git. As root in the guest, it tries to get around the NIC capture by using another DNS server, connecting to a real address, and connecting to other ports on the gateway. With `tests/attack.c` as the workload, it tries to:

- impersonate the agent on the control port
- open a root shell over vsock loopback and through the guest's own CID
- exec through the 32-bit and x32 syscall tables
- exec a denied binary

## Persistent sessions

A persistent session (`zapper run --persistent`, `persistent: true`) keeps its disks when it stops, in its `vm/` directory, and can be started again (`zapper start`), also after the daemon restarts; `zapper rm` removes them. Its disks are files the daemon's user can change, so they get tamper evidence in the record:

- **When it is made**, its image's SHA-256 is compared with the one recorded when the image was built (`images/NAME.json`), in a `disks` event (`image`): an image changed since is refused unless `--accept-changed-image` (`accept_changed_image`), recorded too; one built before images were hashed is recorded as unverified, and used.
- **When it stops**, once its VM is off, each disk's SHA-256 is recorded in a `disks` event (`stopped`). The record is the only copy: nothing beside the disks holds their hashes.
- **When it starts**, each disk is hashed again before Firecracker opens it, and compared with the last `disks stopped` event in the session's record: `checked` if the same; `changed` if not, naming what differs, and the start is refused; `unverified` if there is nothing to compare with, or the record shows the session running after that stop (the daemon ended while it ran, so its disks weren't hashed). `--accept-changed-disks` starts it anyway, recorded as `accepted`.
- **`zapper record session ID`** (`GET /v1/sessions/{id}/verify`) checks the session's files against the keeper's chains, then the record's disks from beginning to end: a start that checked disks unlike the stop before it, a `disks stopped` event not following the session's stop (hashes appended later to cover a change), and every change, unverified start and accepted image. It exits 1 on any finding.

What this does and doesn't establish:

- **With a keeper**, the record is append-only, so a stop's hashes stay as they were written: a disk changed while stopped is refused at the start, and, if the start was made to pass anyway, shows in `zapper record session` as a start unlike its stop. Hashes appended to cover a change show as a stop without a session stop before it.
- **Without a keeper**, the record is the daemon user's file like any other, and these checks catch accidents, not someone who edits the record too.
- **Against the operator**, the daemon's checks show honest operation, not more: the operator is the daemon's user, and can run a `zapper` that hashes nothing and records what it likes. What it can't do, with a keeper, is change what was recorded before; checking a disk against those hashes with something the operator doesn't run (the keeper, or an auditor's own `sha256sum` against the record) is what holds against them.
- **While the session runs**, no change to its disks is caught ([limitations](#limitations)). Its own secrets (`Session::secrets`, such as a firm worker's token) aren't kept across a daemon restart. A persistent session can't share policy layers with other sessions, so a firm's workers aren't persistent.

## Limitations

- **Exec checks may race.** The host decides on the path the process passed. A multithreaded process written for it may change that memory between the check and the kernel's own read; the kernel documents this limit of seccomp user notification. The tests haven't shown it, but treat exec policy as a guardrail and rely on network and secrets for the boundary.
- **`fs` rules apply at boot.** Files created later aren't covered, and root in the guest can undo the rules.
- **Firecracker's uid needs subordinate ids.** With ranges for the daemon user in `/etc/subuid` and `/etc/subgid` and the `uidmap` helpers (`daemon.vmm_subids`, on by default), each session's Firecracker runs as a subordinate uid of its own, without capabilities: it owns nothing on the host and can't signal or trace the daemon or another session's VMM. The VMM process that sets up its namespaces keeps the daemon user's uid, as does Firecracker on a host without the ranges or helpers (`zapper check` says which). Either way the kernel interfaces it keeps (`/dev/kvm`, and `/dev/net/tun` with a NIC) are shared with the host, and Firecracker keeps the daemon user's groups, `kvm` among them.
- **The daemon is one process that holds the secrets and runs the proxy.** It parses HTTP, TLS, JSON and frames from the guest with memory-safe libraries, and a bug in that code would be a bug in the boundary.
- **Rate and volume limits** for network traffic are planned: see [the README](../README.md#next).
- **Sessions end when the daemon restarts.** Their audit logs stay on disk. So does a firm's run, which can be replayed but not resumed. A persistent session comes back stopped, with its disks, to be started again.
- **The operator can change a running sandbox's disk without a trace.** A persistent session's disks changed while it is stopped show in the record ([persistent sessions](#persistent-sessions)); while it runs, Firecracker writes them constantly, and a write by the daemon's user looks no different.
- **A firm tool's effect can outlive a failed record write.** The "no turns while the record can't be written" gate ([the record](#the-record)) is checked when a turn starts, but a worker's tool call appends to the journal *during* the turn. If that append fails (a full disk), the in-memory effect — a funding, a hire, an access decision — has already happened, while the event is missing from the journal the record, replay and the board pack are rebuilt from. So a run can act on a state the record doesn't show. `mail_send` is the exception: a message body lives only in the journal, so a failed write there fails the send rather than deliver a message that reads back empty. Making every state-changing tool fail the same way (append before the effect, or roll back) is not yet done.

Network-specific limitations are on the [network](network.md#limitations) page.

## References

- Firecracker design and threat model: https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md
- seccomp user notification and its TOCTOU caveat: `man 2 seccomp_unotify`
- Sources: `crates/zapper-core/src/vmm.rs`, `crates/zapper-core/src/netns.rs`, `crates/zapper-core/src/daemon/{proxy,dns,nic,addr,session,tls}.rs`, `crates/zapper-office/src/`, `crates/zapper-agent/src/{shell,sys,init,office,forward}.rs`
