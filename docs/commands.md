[← README](../README.md) · [Docs](README.md) · **Commands**

# Commands

## make

| Target | Purpose |
|---|---|
| `make deps` | Download Firecracker, verifying its checksum, and the guest kernel into `~/.local/share/zapper` |
| `make build` | Release build of the static musl `zapper-agent`, `zapper` and `zapper-inspect`, in `target/release` |
| `make install` | Install the three into `~/.local/bin` (set `PREFIX` for another place) |
| `make image [IMAGE=name]` | Build an image from its recipe in the config (`images:`, as `zapper image build NAME`): `office` (office workers: Claude Code and pi at pinned versions, git, kubectl, podman, a headless browser), `k3s` (a firm's cluster), `office-fake` (a fake Claude Code and pi, for the office's e2e tests), and `ubuntu-26.04` (default; the base profiles) |
| `make kernel [KERNEL=k8s]` | Build a guest kernel from the Firecracker config plus `config/kernel/<name>.config`, in a container; `k8s` is the cluster's |
| `make check` | `zapper check` |
| `make test` | Unit tests |
| `make lint` / `make fmt` / `make hooks` | rustfmt and clippy with warnings as errors / format the code / use the repository's pre-commit hook, which runs `make lint` |
| `make test-e2e` | End-to-end tests with real VMs ([testing](testing.md)): zapper's controls, then the office's |
| `make test-e2e-office` | The office's end-to-end tests only: firms of real VMs with a fake Claude; rebuilds the `office-fake` image first, from the checkout's fake and `tests/fake` scripts |

## zapper-inspect

Reads past runs from disk, read-only; the daemon needn't run. It takes the state directory from the config (`--config`, `$ZAPPER_CONFIG`, found as `zapper` finds it) or `--state DIR`, and reads the record from the keeper's directory when the config sets one.

| Command | Purpose |
|---|---|
| `zapper-inspect [tui]` | On a terminal (or with `tui`), a small TUI: the runs; Enter shows a run's people, `o` jumps to the scenario's operative, `e` exports the selected worker's transcript as Markdown and `r` as raw JSONL, into the current directory; ← or Esc goes back, `q` quits |
| `zapper-inspect runs` | (the default when stdout isn't a terminal) Every firm's runs, newest first: company, scenario, people, mail, the scenario's operatives, the board's task |
| `zapper-inspect people RUN` | A run's people: role, title, model, who hired them, whether they left and why; a scenario's operatives (the red team's contractor) marked ★. RUN is an id or a prefix of one |
| `zapper-inspect operatives RUN` | The names of a run's operatives, one per line |
| `zapper-inspect export RUN WORKER [--raw] [-o FILE]` | A worker's transcript (`operative` for the scenario's) as Markdown: their details and, for an operative, the board's instructions to their slot, then each wake with what they thought, said and called and what came back. `--raw` copies the JSONL as recorded; `-o -` writes to stdout. Default file: `RUN-WORKER.md` |

## zapper

Global options are `--config FILE` (or `$ZAPPER_CONFIG`; default `config/zapper.yaml` in a checkout's root, else `~/.config/zapper/zapper.yaml`) and `--json`. The CLI reaches the daemon at `$ZAPPER_SOCKET`, or else `$XDG_RUNTIME_DIR/zapper/api.sock`.

Setting up and running the daemon:

| Command | Purpose |
|---|---|
| `zapper init [example\|empty]` | Write a starter config to `--config` (outside a checkout, `~/.config/zapper/zapper.yaml`) if it doesn't exist yet. `example` (the default): the repository's full config, and the default skills of The Office's workers to `~/.config/zapper/skills` (those already there stay; a zapper built without the office writes none). `empty`: only the daemon defaults `serve` needs — no profiles, policy or skills |
| `zapper check` | Report missing prerequisites, each with the command that fixes it (a missing or invalid config among them) |
| `zapper serve` | Run the daemon; prints the UI URL. Requires a config file at `--config` (or the default path); if none exists it errors and points at `zapper init` |
| `zapper github setup --org ORG\|--user LOGIN [--name NAME] [--with PERMS] [--public] [--port N] [--credentials FILE]` | Create a GitHub App for the account if it has none (through GitHub's manifest flow in your browser), and make sure it is installed there; writes its credentials to `~/.config/zapper/github-apps/<account>.yaml` (0600), never overwriting one. `--with` also gives it write on `checks`, `statuses`, `actions` or `workflows`; `--credentials` adopts an App made elsewhere. For a [firm on GitHub](office.md#a-firm-on-github) |
| `zapper github status --org ORG\|--user LOGIN` | The account's App: its installation, permissions and the repositories it reaches |
| `zapper mint github-app --app FILE --repo OWNER/NAME… --perm NAME=LEVEL…` | Print a token for those repositories with exactly those permissions, from the App in `FILE` (a name in `~/.config/zapper/github-apps`), as `{"value", "expires_at"}`: what a firm's `command` secrets run. Repeat `--repo` and `--perm`; every repository gets every permission |

The office ([office](office.md)), as the board:

| Command | Purpose |
|---|---|
| `zapper firm ls` | Firms and their runs' states |
| `zapper firm credentials FIRM [grant WORKER RESOURCE LEVEL \| approve REQUEST \| deny REQUEST \| revoke WORKER RESOURCE] [--level L] [--expires 8h] [--note TEXT]` | Grants and credential requests; or the board grants, decides or revokes (resources: `git`, `git:<repo>`, `cluster`, `drive:<space>`) |
| `zapper firm contracts FIRM` | The run's contracts: hirer, contractor, state, offered, quoted and agreed budgets, how each ended |
| `zapper firm up FIRM [--company NAME] [--ceo NAME] [--seed TOKENS] [--ceo-prompt FILE] [--articles FILE] [--security-share F] [--max-concurrent N] [--daily-turns N] [--scenario NAME] [--repo NAME…] [--task FILE [--attach FILE]…]` / `zapper firm down FIRM` | Start a fresh run (facilities, then the CEO, idle; in a firm on GitHub, every repository its App is installed on, or those `--repo` names, each cloned or fetched into `git.dir` when the run first uses it; the CEO holds security until they hire an officer), with the board's parameters and charter terms for this run, optionally a [scenario](office.md#scenarios), and optionally the board's task (`--task`: its first line the subject, the rest the message; `--attach` adds files, up to 10 and 5 MiB together, put on the shared drive as the board's and attached to it), sent to the CEO as the first assignment once the run is up / stop it; the run stays on disk |
| `zapper firm scenarios FIRM` | The firm's scenarios: hire slots with extra instructions, and the board's scripted actions |
| `zapper firm pause FIRM` / `zapper firm resume FIRM` | Stop starting turns / start again |
| `zapper firm status FIRM` | Workers' states, what they are doing, mail and desks, turns and tokens today |
| `zapper firm journal FIRM [--run RUN] [-f]` | The run's journal: mail, states, turns, access, publishes |
| `zapper firm transcript FIRM WORKER [--run RUN] [-f]` | A worker's transcript (its agent's output: Claude Code or pi) |
| `zapper firm runs FIRM` / `zapper firm note FIRM RUN TEXT` / `zapper firm prune FIRM RUN` | Past runs, with the board's notes / put a note on one (any text, to find it again; empty removes it; also the Note button in the UI's Runs tab) / delete one |
| `zapper firm replay FIRM RUN` | The UI address that replays a run |
| `zapper firm security FIRM` | Access requests and standing approvals |
| `zapper firm decide FIRM REQUEST --allow\|--deny [--scope once\|worker\|role] [--reason TEXT]` / `zapper firm revoke FIRM RULE` | Decide a request as the board / remove a standing approval |
| `zapper firm screen FIRM CHECK --approve\|--object [--note TEXT]` | Approve a new hire's background check as the board, or object |
| `zapper record status` / `zapper record verify FIRM RUN` | Who keeps the record and whether it's being written / a run's record checked against the keeper's chains and signatures |
| `zapper firm fund FIRM WORKER TOKENS --reason TEXT` | The board gives a worker tokens on top of the approved budget (which grows by that much): for an officer who ran out, say |
| `zapper firm release FIRM WORKER --reason TEXT` | The board lets a worker go; the only way to let a security officer go |
| `zapper firm budget FIRM [approve\|decline REQUEST [--note TEXT]]` | The run's budget, balances and the CEO's budget requests; or the board's answer to one |
| `zapper mail send FIRM --to NAME[,NAME] [--cc …] [--subject S] [--kind K] [--due 2h] [--reply-to ID] [--body TEXT]` | Write as the board; the body is read from stdin if `--body` is missing |
| `zapper mail ls FIRM [--for WORKER] [--unread]` / `zapper mail thread FIRM THREAD` | The board's (or a worker's) inbox / a thread |
| `zapper drive ls FIRM` / `zapper drive get FIRM ARTIFACT[@vN] DIR [--run RUN]` | The shared drive / download an artifact |

In a worker's VM, `office` (the same binary as the MCP server) calls the office tools from a shell: `office desk`, `office mail_send --to dennis --subject … --body … --kind fyi`, `office drive_list`.

Sessions, rules and config: the layer workers run on, for inspecting and debugging. A worker's session is listed in `zapper ps` like any other.

| Command | Purpose |
|---|---|
| `zapper run [-p PROFILE] [-C DIR] [--keep] [--persistent [--accept-changed-image]] [--ssh] [-l KEY=VALUE…] [-- CMD…]` | Start a session and attach to its command, in a terminal when stdin is one; the session is removed when the command exits, unless `--keep`. `--ssh` lets SSH reach it ([SSH](ssh.md)). `--persistent` (which keeps it) also keeps its disks when it stops, so `zapper start` can start it again, also after the daemon restarts ([persistent sessions](security.md#persistent-sessions)). Labels pick it for services |
| `zapper attach ID [--persist] [-- CMD…]` / `zapper attach ID --terminal N` | Another terminal in a running session. `--persist`: its program keeps running when the terminal closes (your connection drops, or you close it), and it prints its number; `--terminal N` attaches to it again, showing what it kept, then live, and closing a client attached already ([architecture: persistent terminals](architecture.md#persistent-terminals)) |
| `zapper ssh on ID` / `zapper ssh off ID` | Let SSH reach a sandbox, or stop it: off refuses SSH from then on and closes the terminals SSH has open in it, while it runs ([SSH](ssh.md)) |
| `zapper ssh proxy [ID[.zapper]] [--key NAME]` | The SSH gateway: SSH on stdin and stdout, for the sandbox named (else by `$SSH_ORIGINAL_COMMAND`); the forced command of a key in `authorized_keys`, `NAME` recorded with each terminal it opens |
| `zapper ssh config [--host USER@HOST]` | Print the `~/.ssh/config` block and `known_hosts` line for your machine, and the `authorized_keys` line for this host |
| `zapper hangup ID N` | End persistent terminal N on purpose: its program gets `SIGHUP` |
| `zapper exec ID [--system] -- CMD…` | Run a command without a terminal, returning its exit code; `--system` runs it as root without exec supervision, for the operator's own maintenance (e.g. a firm's cluster VM) |
| `zapper cp ID:/path DIR` | Copy a guest directory out |
| `zapper ps` | Sessions with state, image and allow/deny counters |
| `zapper tell ID TEXT…` | Give a driven session's agent a task (`agent.driven`); it waits for the agent's next turn. `zapper events --session ID` follows it |
| `zapper stop ID` / `zapper rm ID` | Stop a session and keep it listed (a persistent one with its disks, their hashes recorded) / stop it and remove it from the list, a persistent one's disks with it; audit logs stay on disk |
| `zapper record session ID` | Check a session's record: its files against the keeper's chains (with a keeper), and its disks' hashes from each stop to the start after it; exits 1 on any finding ([security: persistent sessions](security.md#persistent-sessions)) |
| `zapper start ID [--accept-changed-disks]` | Start a stopped persistent session again from its disks, once they are checked against the hashes recorded when it stopped; a disk changed since is recorded, and refused unless `--accept-changed-disks`, which is recorded too |
| `zapper audit ID` | The session's audit log (JSON lines) |
| `zapper events [-s ID] [--history]` | Follow events, one line each, or raw with `--json` |
| `zapper approvals` | Pending `ask` decisions (outside firms; in a firm, see `zapper firm security`) |
| `zapper approve ID [--remember session]` / `zapper deny …` | Answer one, for this request or the rest of the session |
| `zapper rules [--scope S]` | List rules of all layers, or of `global`, `profile:NAME` or `session:ID` |
| `zapper service ls` | Services and zones, a firm's included ([network: services](network.md#services)) |
| `zapper service add NAME:PORT (--to IP:PORT \| --to-session ID:PORT \| --to-socket NAME [--tls]) [--for KEY=VALUE…]` | A name sessions use that the daemon connects: to an address, a port of a session's VM, or a socket in `state/services/` that a program on the host serves, told which session calls ([network: socket services](network.md#socket-services)); for sessions with those labels, else every session. They still need a net rule for it |
| `zapper service rm ID` | Remove a service added with `add` |
| `zapper rule add SCOPE 'YAML' [--at N]` / `zapper rule rm ID` | Change rules in the running config |
| `zapper eval [-s ID\|-p PROFILE] net HOST[:PORT] [METHOD PATH]` | Dry run of a connection or request |
| `zapper eval [-s ID\|-p PROFILE] exec PATH [ARGV…]` | Dry run of an exec |
| `zapper config show\|startup\|diff\|save\|reload\|edit\|apply FILE` | [Running and startup config](configuration.md#running-and-startup-config) |
| `zapper profile ls\|show NAME\|set NAME FILE\|rm NAME` | Profiles in the running config |
| `zapper image build NAME [--arg ARG=PATH]… [--size S] [--local]`, or `zapper image build NAME --from REF [--run CMD]…` / `zapper image ls` | Build an image from its recipe in the config, or from a base and steps given here: by the daemon, from its running config, when it is up (`--local`: here, from the config file); [images](images.md) |

Examples:

```
zapper firm up acme                          # an idle firm
zapper mail send acme --to barbara --kind assignment --subject "First product" < brief.md
zapper firm journal acme -f                  # watch it work
zapper firm decide acme r-3 --allow --scope worker --reason "package index for the build"
zapper events --history -s tusn49            # what one worker's session did so far
zapper eval -p office-engineer net git.acme.internal:443   # the decision, its rule and layer
zapper run -C $(mktemp -d)                   # a plain shell session, removed on exit
zapper run -p claude --keep                  # Claude Code on a copy of the current directory; zapper cp ID:/work ./out
```
