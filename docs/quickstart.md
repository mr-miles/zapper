[← README](../README.md) · [Docs](README.md) · **Quickstart**

# Quickstart

From a fresh clone and an OpenShell gateway to a running firm: the example firm `acme`, a CEO who hires the team.

## 1. What you need

- Linux on x86_64 with KVM, and an [OpenShell](https://github.com/NVIDIA/OpenShell) gateway (v0.1.7) with the [openshell-firecracker](https://github.com/NVIDIA/openshell-firecracker) driver installed and running (its README's install, configure, check and start steps). `openshell gateway info` should show the `firecracker` driver.
- Rust, with the static target: `rustup target add x86_64-unknown-linux-musl`
- `podman`, to build the workers' image
- a Claude subscription, for the workers
- room for the example firm: each worker is a microVM with its profile's memory ([office: resources](office.md#resources))

## 2. Build

```
git clone <this repository> && cd zapper
make build
```

That builds `zapper-office` (the office: its dashboard, the firm CLI, image builds), `zapper-inspect` (past runs) and `zapper-agent`, the static `office` command the workers' image carries. Run the office from the repository's root: it reads `config/` there (`zapper.yaml`: the profiles and image recipes; `office.yaml`: the firms; `openshell.yaml`: the gateway). Outside a checkout, `zapper-office init` writes all three to `~/.config/zapper`, with the workers' agents and skills beside them.

## 3. The workers' image

```
make image                    # localhost/zapper-openshell-office: Claude Code and pi (pinned), git, kubectl, podman, a headless browser
```

`zapper-office image NAME` builds any recipe of `zapper.yaml`'s `images:` the same way: the recipe, then the workers' account at uid 1000, `/work`, and the `office` command. The OpenShell client uses `localhost/zapper-openshell-<image>` for a profile's image unless `images:` in `openshell.yaml` says otherwise.

## 4. The gateway's side

The driver must issue each sandbox's identity: set `OPENSHELL_VM_SUPERVISOR_IDENTITY=true` in its environment (`~/.config/openshell/firecracker-driver.env`) and restart it.

The Claude token is the gateway's `anthropic` provider: create it as OpenShell's docs say (`openshell provider create --type anthropic ...`). Nothing in a VM sees it; the supervisor adds it to the workers' requests to the Claude API.

The workers' identity at the office is the `office-edge` provider:

```
openshell profile import -f config/openshell-office-edge.yaml
openshell provider create --name office-edge --type office-edge --runtime-credentials
```

The office turns OpenShell's policy advisor on for each sandbox it makes and its security officers decide every proposal, so leave `proposal_approval_mode` and `agent_policy_proposals_enabled` unset at the gateway: the office refuses to start against a gateway that approves on its own.

## 5. Point the office at the gateway

In `config/openshell.yaml`, set `gateway:` to the gateway's URL (`openshell gateway info` prints it) and its TLS files. `providers: [anthropic, office-edge]` attaches both to every sandbox. The `edge:` defaults suit the driver on this host.

## 6. Start the office

```
target/release/zapper-office serve
```

It connects to the gateway, starts the office edge on `127.0.0.1:8970` (where the sandboxes' supervisors reach it, as `host.openshell.internal`), and prints the dashboard's URL with its one-time token. On a remote host, reach it through an SSH tunnel: `ssh -L 8099:127.0.0.1:8099 user@host`.

## 7. Start a firm

A firm spends your Claude usage. In the dashboard's Kickoff tab, start the company: its name, its CEO and charter, the terms you set, and your own articles. Or start the example firm with its CEO idle, and give the CEO its first task:

```
target/release/zapper-office firm up acme
target/release/zapper-office mail send acme --to barbara --kind assignment --subject 'Build the widget' --body '...'
```

The CEO scopes the work and asks you for a budget, which only you approve (Kickoff, or `zapper-office firm budget acme approve b-1`). With it they hire; nobody starts before their background check clears.

## 8. Watch it

The dashboard's tabs follow the firm: the org chart, mail, the drive, git, security (access requests, standing approvals, background checks), VMs and statistics. A worker's network access is the security officers' call: what no rule allows is refused at once, the worker proposes a narrow rule through OpenShell's `policy.local`, and an officer approves, narrows or rejects it.

`zapper-office firm down acme` ends the run; it can be replayed in the Runs tab, or read with `zapper-inspect`.
