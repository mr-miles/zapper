# The Office

The Office runs a company of AI workers: a CEO, managers, engineers, designers, researchers, analysts and security officers, each a coding agent (Claude Code, or pi on another model) in its own sandbox. They mail each other, wake on mail, publish to a shared drive, push to the firm's git repository and deploy to the firm's own cluster. You are the board. [The Office](docs/the-office.md) is its front page: what it is, what you see, and its scenarios.

> [!NOTE]
> Here from [Contained](https://blog.emelianov.cloud/contained/)? That post's runs were on zapper's own Firecracker daemon. The Office now runs its workers on an [OpenShell](https://github.com/NVIDIA/OpenShell) gateway with the [openshell-firecracker](https://github.com/NVIDIA/openshell-firecracker) driver: one microVM per worker, its network and credentials decided outside the VM by OpenShell's supervisor ([the migration](docs/openshell-migration.md)).

[![The Office: the org chart of a running firm](docs/img/office.png)](docs/the-office.md)

## How it holds together

- **One sandbox per worker.** Each worker is an OpenShell sandbox, a Firecracker microVM with no network interface of its own: everything it reaches goes through the supervisor on the host.
- **Credentials stay outside.** The Claude token is the gateway's `anthropic` provider, injected on the way out. A worker's identity at the office is a short-lived token the supervisor gets for it (its sandbox's SPIFFE ID, checked by the office) and adds to its requests; nothing in the VM can present another worker's.
- **The office's tools and git** are its edge: MCP over Streamable HTTP and git over HTTP, every request checked for the caller's token, their role and their grants.
- **Network access is the officers' call.** The office writes each worker's rules (their own, their role's) into their sandbox's policy. What no rule allows is refused at once; the worker proposes a narrow rule through OpenShell's `policy.local`, or the sandbox drafts one from the refusal, and a security officer (never the requester) approves, narrows or rejects it. Approvals expire and are revoked.
- **Everything is journaled**, so a run can be replayed afterwards.

## Requirements

- **An OpenShell gateway** (v0.1.7) with the Firecracker driver, on Linux with KVM, with supervisor identity on (`OPENSHELL_VM_SUPERVISOR_IDENTITY=true`).
- **To build**: Rust with the static target (`rustup target add x86_64-unknown-linux-musl`), a C compiler and `make`.
- **To build images**: `podman`.
- **A Claude subscription** (or a Claude API key), as the gateway's `anthropic` provider.

## Quickstart

From a fresh clone (the office uses `config/` there):

```
make build                    # zapper-office, zapper-inspect, and the sandboxes' `office` command
make image                    # the workers' image: Claude Code and pi (pinned), git, kubectl, podman, a headless browser
openshell profile import -f config/openshell-office-edge.yaml
openshell provider create --name office-edge --type office-edge --runtime-credentials
# edit config/openshell.yaml: the gateway's URL and TLS files
target/release/zapper-office serve   # prints the dashboard's URL
```

Then start the example firm in the dashboard's Kickoff tab, or with `zapper-office firm up acme`. The [quickstart](docs/quickstart.md) goes step by step, and the [docs](docs/README.md) index has the rest.

> [!TIP]
> If you use a coding agent, point it at [AGENTS.md](AGENTS.md) and it will set The Office up.

## Status

The office's unit tests run on an in-memory backend (`make test`). On a real gateway, phases 1 to 3 of [the migration](docs/openshell-migration.md) are built (sandboxes, the office edge and identity, the officers on drafted rules) and their gates are still to run; the cluster (phase 4: k3s in a sandbox of its own) and the end-to-end suite's port (phase 5) are next. The workbench, which ran on zapper's daemon, is out of the build until it is ported onto OpenShell ([workbench](docs/workbench.md)).

## License

[Apache-2.0](LICENSE)
