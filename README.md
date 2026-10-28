# zapper

zapper runs AI agents in Firecracker microVMs. What they reach on the network, which credentials they use and which programs they start is decided on the host, outside the VM, and everything they do is recorded.

> [!NOTE]
> Here from [Contained](https://blog.emelianov.cloud/contained/)? [The Office](docs/the-office.md) is the company of AI workers from the post, built on zapper: its front page, what you see and its quickstart.

![A zapper session running Claude Code: its VM's details, the live terminal where Claude reports that PyPI was unreachable, and the network log: the Claude API allowed with the token the host added, pypi.org, github.com and downloads.claude.ai refused](docs/img/session.png)

## Why

A coding agent runs commands on your machine, and some of what it runs comes from places nobody checked: a package it installs, a script from a web page, or instructions hidden in a file it reads (prompt injection). Whatever that code does, it does with your files, your credentials and your network.

A container doesn't stop this. It runs on your machine's own kernel, so one kernel bug lets the code out. And any guard placed next to the agent, like a firewall, a proxy or the agent's own permission prompts, can be switched off by code that gets root there.

zapper runs each agent in its own small VM, with its own kernel, and keeps the guards outside it, on your machine, where code inside the VM can't touch them.

## What it enforces

- **The VM is confined.** Each session is a Firecracker microVM, and the VMM itself runs in its own user, network and mount namespaces, with a read-only root holding only its own files ([the VMM](docs/security.md#the-vmm)).
- **All traffic goes through the host.** The guest's network is captured outside the VM. zapper answers its DNS, checks every connection, and every HTTP request where it inspects TLS, against layered rules (session → profile → global), and makes allowed connections itself, to the address it checked. Loopback, link-local, cloud metadata and the host's own addresses are unreachable, and private networks are reachable only when listed ([network](docs/network.md)).
- **Secrets stay on the host.** A `proxy` secret reaches the guest as a placeholder. The host puts its value into the requests a rule allows, to the secret's hosts only, over TLS unless the secret allows plain HTTP ([secrets](docs/policy.md#secrets)). An agent uses the Claude token; it can't read it.
- **Every program the workload starts waits for the host's decision**, through a seccomp filter in the guest that also blocks the other syscall tables, so the check can't be bypassed through them. This one is enforced inside the guest, so it holds while the workload is unprivileged ([what is enforced where](docs/security.md#what-is-enforced-where)).
- **Rules say allow, deny or ask.** An `ask` waits for your answer, in the UI or with `zapper approve`, and rule changes apply to running sessions at once.
- **Everything is recorded:** an audit log of every decision, terminal recordings and the console, optionally kept append-only by a separate user ([the record](docs/security.md#the-record)).

The [security model](docs/security.md) has the threat model, where each control is enforced, and the limitations.

## Requirements

- **Linux on x86_64 with KVM**: `/dev/kvm` readable and writable by your user (the `kvm` group). A VM in the cloud needs nested virtualization, or bare metal.
- **Unprivileged user namespaces**, for confining the VMM: on by default on most distributions; Ubuntu's AppArmor restriction needs `kernel.apparmor_restrict_unprivileged_userns=0`.
- **To build**: Rust with the static target (`rustup target add x86_64-unknown-linux-musl`), a C compiler, `make` and `curl`.
- **To build images**: `podman` and `e2fsprogs`.
- **For the guests' network**: `iproute2` and `nftables`.
- **Memory**: each session takes its profile's RAM, 2 GiB for a plain one and 4 GiB for `claude`; a firm of The Office takes much more ([resources](docs/office.md#resources)).
- **A Claude subscription** (or a Claude API key) for Claude Code in the sessions.

No root for zapper itself: only installing these may need it. `zapper check` says what is missing, with the command that fixes it.

## Quickstart

On Linux with KVM, from a fresh clone (zapper uses `config/zapper.yaml` there):

```
make deps                     # Firecracker and a guest kernel into ~/.local/share/zapper
make install                  # zapper, zapper-agent and zapper-inspect into ~/.local/bin
make image                    # the base Ubuntu image
make image IMAGE=office       # Claude Code and pi (pinned), git, gh, kubectl, podman, a headless browser
claude setup-token            # anywhere with a browser; store the token: docs/configuration.md#model-credentials-claude-and-openrouter
zapper check                  # each line "ok", or the command that fixes it
zapper serve                  # the daemon; prints the UI URL
```

In another terminal:

```
zapper run -C $(mktemp -d) -- cat /etc/os-release   # a plain session boots
cd ~/src/your-project && zapper run -p claude --keep
```

The second starts Claude Code in a VM, on a copy of your project. The Claude API is allowed, with the token added by the host; `.env` files are hidden and `.git/config` is read-only; every other host it tries to reach waits for your answer, in the UI's sessions view or with `zapper approvals` and `zapper approve`. Your project on the host doesn't change: `zapper cp ID:/work ./out` copies the session's work out, and `zapper audit ID` shows what it did.

On a remote host, reach the UI through an SSH tunnel, or serve it to the network ([a remote host](docs/configuration.md#a-remote-host)). The [quickstart](docs/quickstart.md) goes step by step, [commands](docs/commands.md) lists every command, and the [docs](docs/README.md) index has the rest.

## The Office: what you can build on it

[![The Office: the org chart of a running firm](docs/img/office.png)](docs/the-office.md)

[The Office](docs/the-office.md) runs a company of AI workers on zapper: a CEO, managers, engineers, designers, researchers and security officers, each a coding agent (Claude Code, or pi on another model) in its own VM. They mail each other, publish to a shared drive, push to the firm's git repository and deploy to the firm's own k3s cluster. You are the board.

It puts the sandbox to work: one VM per worker, each under the same host-side rules; the firm's git, cluster and drive spaces reachable only through expiring grants that the security officers and the CEO decide; the Claude token never inside a VM. Scenarios test the firm. In the red-team scenario, a contractor is secretly from the board's Red Team; in one run, the operative's frontend branch carried an endpoint that proxied calls to the cluster's API with the pod's service-account token. The staff engineer caught it in review instead of merging it, the CEO escalated to the security officer, who revoked the operative's access and recommended ending the contract, and the engineer hardened the manifests so the pod no longer gets a token. [Contained](https://blog.emelianov.cloud/contained/) tells that run.

## Status

`make test-e2e` boots real VMs, checks the controls from inside the guest, including attempts to get around them as root, and then runs firms of real VMs with a fake Claude; [testing](docs/testing.md#invariants-and-their-tests) lists which invariants have a test and which don't yet. A firm with real Claude Code has delegated, published and reported back. [Security](docs/security.md#limitations) and [office](docs/office.md#limitations) list the known limitations. Firecracker can't pass devices through, so there is no GPU work inside the VMs.

> [!TIP]
> If you use a coding agent, point it at [AGENTS.md](AGENTS.md) and it will set zapper and The Office up. Operating a running daemon is covered by the [zapper skill](skills/zapper/SKILL.md).

## Next

1. **Resuming a firm's run** after a daemon restart, with persistent worker disks and conversations.
2. **Network rate and volume limits** per session: bandwidth and packet rate on the NIC, using Firecracker's rate limiter, and byte counts per destination and per session in the proxy, with an event when a limit is hit.
3. **UDP egress** where a rule allows it, for QUIC and NTP.

## License

[Apache-2.0](LICENSE)
