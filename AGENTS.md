# Instructions for coding agents

People should start with the [README](README.md). The project is The Office: firms of AI workers (CEO, product manager, managers, engineers, designers, researchers, analysts, security officers), each Claude Code (or pi) in its own sandbox of an [OpenShell](https://github.com/NVIDIA/OpenShell) gateway with the openshell-firecracker driver. They mail each other, wake on mail, publish to a shared drive, push to the firm's git repository and deploy to the firm's cluster; security officers decide access; the operator is the board ([docs/the-office.md](docs/the-office.md), [docs/office.md](docs/office.md)). The office moved off zapper's own Firecracker daemon, which is gone; [docs/openshell-migration.md](docs/openshell-migration.md) records how, and what is still to do.

## Setting The Office up for a user

### 1. Check the host (no changes)

- An OpenShell gateway with the Firecracker driver: `openshell gateway info` shows the `firecracker` driver. If there is none, the openshell-firecracker README has the install; it needs root for its prerequisites (KVM access, sub-uids), which the user runs themselves.
- `cargo`, the static target (`rustup target list --installed | grep musl`), and `podman`.

Report what is missing and let the user decide. Never run `sudo` without the user's explicit consent, and run only what you listed.

### 2. Build

```
make build                    # zapper-office, zapper-inspect, and the sandboxes' `office` command
make image                    # the workers' image (IMAGE=office-fake: a fake Claude Code and pi, for tests)
```

Run the office from the repository's root: it reads `config/zapper.yaml` (profiles and image recipes), `config/office.yaml` (firms) and `config/openshell.yaml` (the gateway), tracked in the repository. Outside a checkout, `zapper-office init` writes all three to `~/.config/zapper`, with the workers' agents and skills. Run image builds in the background with a log (`make image > /tmp/zapper-image.log 2>&1`) and give the user `tail -f /tmp/zapper-image.log`.

### 3. The gateway's side

The user creates the credentials themselves; never ask for a token, read it or print it.

1. The Claude token as the gateway's `anthropic` provider (`claude setup-token` on any machine with a browser prints a long-lived OAuth token; OpenShell's docs say how to create the provider from it).
2. The workers' identity at the office: `openshell profile import -f config/openshell-office-edge.yaml` and `openshell provider create --name office-edge --type office-edge --runtime-credentials`.
3. The driver's supervisor identity on: `OPENSHELL_VM_SUPERVISOR_IDENTITY=true` in its environment, then restart it.
4. `proposal_approval_mode` and `agent_policy_proposals_enabled` left unset at the gateway: the office decides proposals through its officers and refuses to start otherwise.

### 4. Hand over a running office

Set `gateway:` and its TLS files in `config/openshell.yaml`, then start `zapper-office serve` (or tell the user to) and give them the dashboard URL it prints. On a remote host, an SSH tunnel to its port (`ssh -L 8099:127.0.0.1:8099 user@host`).

With the user's go-ahead (a firm spends their Claude usage), the user starts the company in the dashboard's Kickoff tab, or `zapper-office firm up acme` starts the example firm with its CEO idle; the user gives the CEO its first task there, or with `zapper-office mail send acme --to barbara --kind assignment …`. The CEO then asks for a budget, which only the user approves (`zapper-office firm budget acme approve b-1`, or Kickoff); never approve budgets, background checks or access requests on the user's behalf.

### Rules

- Firms may be in use: before `zapper-office firm down`, `zapper-office config reload` or restarting the office, check `zapper-office firm ls` and say what you are about to change. A restart or `firm down` ends a firm's run, which can't be resumed.
- The office's `state` directory holds firms' runs (mail, transcripts, published files, journals): treat them as sensitive and do not delete them unasked.

## Working on the repository

```
crates/zapper-office/    The Office: post office, foreman (headless Claude or pi), shared drive, security officers, facilities; the firms config (`firm.rs`), validated against the profiles of zapper.yaml
  src/openshell.rs         the OfficeClient over an OpenShell gateway: sandboxes from profiles, exec, policy and drafted rules
  src/openshell_rules.rs   zapper's rules as a sandbox's network policy, and drafted rules as the officers see them
  src/edge.rs              the office edge: /oauth/token (SVID → token), MCP over Streamable HTTP at /mcp, git at /git/
  src/client.rs            OfficeClient: what the firm logic needs from where its workers run
  src/fake.rs              an in-memory OfficeClient for the tests
  src/image.rs             `zapper-office image`: a recipe of zapper.yaml as the sandboxes' image
  src/office.html          the office UI; its stylesheet and scripts, one per tab or part, in src/ui/
crates/zapper-agent/     the sandboxes' `office` command: the workers' CLI, MCP bridge and Stop hook, over HTTP to the edge
crates/zapper-policy/    config model (profiles, image recipes), layered rule evaluation (no I/O)
crates/zapper-driver/    the harnesses (Claude Code and pi): their files, and driving them headless
crates/zapper-git/       git over HTTP: `git http-backend` per request, and the pre-receive hook that keeps branches apart from main
crates/zapper-record/    the journal's writer, optionally through zapper-keeper (crates/zapper-keeper: a separate user's append-only store)
crates/zapper-inspect/   `zapper-inspect`: past runs read from disk, their people and a scenario's operatives
crates/zapper-runs/      the files a firm's run leaves and what its journal says, read-only
crates/zapper-github/    the firm's GitHub remote; zapper-mint mints its tokens
crates/zapper-fake/      a fake Claude Code and pi that play a script: the office-fake image's `claude` and `pi`
crates/zapper-workbench/, crates/zapper-ssh/   out of the workspace: they ran on the daemon, and wait to be ported onto OpenShell
config/                  zapper.yaml (profiles, image recipes), office.yaml (firms), openshell.yaml (the gateway), openshell-office-edge.yaml (the edge's provider profile)
  agents/, skills/         office roles' agent definitions and skills
tests/fake               the scripts the fake workers play; tests/e2e-office.sh is the daemon-era suite, to be ported
docs/                    one page per topic; index in docs/README.md
examples/                configs to run as they are; each loads in a test
```

Build and test:

```
make build        # release: the office, zapper-inspect, the static guest command
make test         # unit tests, no gateway, seconds
make lint         # rustfmt (defaults, rustfmt.toml) and clippy with warnings as errors; `make fmt` formats
```

Invariants (a change that breaks one is wrong even if it compiles):

- A worker's credentials stay outside its sandbox: the Claude token is the gateway's provider, and a worker's identity at the office is a token the supervisor gets from the edge (`/oauth/token`, the sandbox's JWT-SVID verified against the driver's keys) and adds to its requests.
- Every request to the edge but the token grant needs a current token the edge issued, for a sandbox the office still runs; the caller is that sandbox's worker, never what the request says. Requests with an `Origin` are refused.
- In a firm, the sender of every office request is the sandbox it came from; roles and the chart are checked on the office's side; a request no rule allows is refused at once and decided by officers or the board.
- Nothing approves a worker's drafted rule but an officer or the board: the office refuses a gateway with automatic approval, and `access_decide` is what approves, narrows or rejects at the gateway.
- In a firm, security is always held (by officers, else the CEO, else the board), nobody decides their own access request or their own hire's background check, a hire takes no turn before being cleared, and a hired officer decides nothing before being cleared. The charter's articles are the same for every run; only their terms and the board's own articles change. A contractor mails only their hirer and those the hirer added, has no authority, and a contract moves money only from the hirer's balance. Access to the firm's resources (git, the cluster, drive spaces) is a grant decided by the policy owners (officers and the CEO, never the requester), enforced by the office (git through the edge) or by the cluster's RBAC, expiring, and revoked when the worker leaves; no role gets access by default.
- Anything from a sandbox is bounded (message and body limits) and rendered as text in the UI.
- The dashboard's writes require its token cookie and `X-Zapper-UI: 1`, and, on loopback, a local `Host` and `Origin`; listening elsewhere, an `Origin` naming the `Host` the request came to.
- A config change is validated as a whole and applied atomically, or not at all.
- With a keeper, the journal is appended through it and nothing else.

Code style: rustfmt's defaults and a clean clippy. `make hooks` turns on the pre-commit hook in `.githooks/`, which runs both; keep `make lint` passing.

Markdown: never hard-wrap prose; one line per paragraph or list item.
