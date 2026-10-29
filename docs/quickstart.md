[← README](../README.md) · [Docs](README.md) · **Quickstart**

# Quickstart

From a fresh clone to a running firm: the example firm `acme`, a CEO who hires the team, and a cluster to deploy to.

## 1. What you need

- Linux on x86_64, with KVM (`ls /dev/kvm`)
- Rust, with the static target: `rustup target add x86_64-unknown-linux-musl`
- `podman` and `e2fsprogs`, to build images
- `iproute2` and `nftables`
- unprivileged user namespaces (on by default on most distributions)
- a Claude subscription, for the workers
- about 40 GiB of free RAM for the example firm ([office: resources](office.md#resources))

`zapper check` in step 5 tells you if something is missing, with the command that fixes it.

## 2. Build and install

```
git clone <this repository> && cd zapper
make deps         # downloads Firecracker and a guest kernel
make install      # builds zapper, zapper-agent and zapper-inspect into ~/.local/bin
```

zapper is the daemon and CLI the office runs on. Run it from the repository's root: it uses `config/zapper.yaml` there, and paths in it are relative to that file or under your home. Outside a checkout, `zapper init` copies it to `~/.config/zapper/zapper.yaml`, with the workers' skills in `~/.config/zapper/skills`. The config defines the firm `acme` and the profiles its workers run with; a profile is a recipe for a VM: which image, which command, which rules.

## 3. Images and the cluster's kernel

```
make image                    # the base Ubuntu image
make image IMAGE=office       # the workers: Claude Code and pi (pinned), git, kubectl, podman, a headless browser
make image IMAGE=k3s          # the firm's cluster
make kernel KERNEL=k8s        # the cluster's kernel, built in a container
```

## 4. The Claude token

Workers use your Claude subscription through a long-lived OAuth token. Create it on any machine with Claude Code and a browser (your laptop will do: the token belongs to your account, not the machine), then store it on the zapper host, pasting it at the prompt so it stays out of your shell history:

```
claude setup-token        # signs in and prints the token, sk-ant-oat…
mkdir -p -m 700 ~/.config/zapper
(umask 077; read -rsp 'Claude token: ' t; printf '%s' "$t" > ~/.config/zapper/claude-token); echo
```

The token stays on the host: zapper adds it to the workers' requests to `api.anthropic.com`, and Claude Code in the VMs only sees a placeholder.

Every worker runs on Claude, contractors included. The red-team scenario's contractor runs on pi with DeepSeek through OpenRouter (Claude declines its covert objective): to play it, create a key at [openrouter.ai/settings/keys](https://openrouter.ai/settings/keys) and store it the same way, `(umask 077; read -rsp 'OpenRouter key: ' t; printf '%s' "$t" > ~/.config/zapper/openrouter-token); echo`, or pick another profile for its slot in Kickoff. [Model credentials](configuration.md#model-credentials-claude-and-openrouter) has both in full.

Usage counts against your subscription, not API billing. Claude Code's banner says "Claude API" anyway, because this kind of token can't read your plan; `/status` shows "Auth token: CLAUDE_CODE_OAUTH_TOKEN", and `/usage` shows your plan's limits. The comments on `secrets` in the config explain the difference, and how to switch to an API key.

## 5. Check

```
zapper check
```

Each line should say `ok`. Expected at this point:

- `note   the daemon isn't running`: you start it next.
- `note   the record is written by this user`: no [keeper](security.md#the-record), which is optional.

## 6. Start the daemon

In a terminal of its own:

```
zapper serve
```

```
zapper: UI at http://127.0.0.1:7878/?token=3evtji2k5mntwur2ijmc5xm2
zapper: API on /run/user/1000/zapper/api.sock
```

Open that URL: the UI shows the firm, still off. Leave the daemon running.

## 7. Check a session

Each worker is a zapper session. Before booting a firm, check that a plain one works:

```
zapper run -C $(mktemp -d)
```

You get a shell inside a VM, in `/work`, which holds a copy of the directory given with `-C` (by default, the current one). Try:

```
curl https://pypi.org               # fails: the default profile allows no network
exit
```

The session and its VM are removed when you exit. `zapper events --history` shows what it did, one line per DNS query, connection or exec, with the rule that decided it.

Claude Code works in a sandbox on its own too, without a firm: `zapper run -p claude --keep`, in a project's directory, starts it on a copy of the project. The Claude API is allowed with the token the host adds, `.env` files are hidden, and every other host waits for your answer (`zapper approvals`, `zapper approve ID`, or the UI's sessions view). `zapper cp ID:/work ./out` copies its work out.

## 8. Start the firm

```
zapper firm up acme                     # boots the cluster, then the CEO, idle
zapper firm status acme                 # workers' states, mail, turns and tokens today
```

Or start it in the UI's Kickoff tab: the company's name, its CEO (who holds security until they hire an officer), and the [charter](office.md#the-charter), the company's law, with the terms you set (the seed, security's share, the pace) and your own articles. `zapper firm up acme --articles law.md` does the same from the command line.

`firm up` checks the firm's memory against the host's before booting anything. In the UI, the org chart fills in as workers boot, and the product port, `http://127.0.0.1:8080`, shows the firm's landing page.

## 9. Give the CEO a task

Write the CEO the firm's first task in the Kickoff tab, or:

```
zapper mail send acme --to barbara --kind assignment --subject "First product" --body "…"
```

Nothing in the config says what the firm builds; the kickoff does. [Office prompts](office-prompts.md#example-kickoff) has an example.

The CEO scopes the task on the seed and asks you for a budget. Approve or decline it in Kickoff, or:

```
zapper firm budget acme                 # the request: total, plan, reason
zapper firm budget acme approve b-1     # or: decline b-1 --note "…"
```

Then the CEO hires the team; each hire waits for a background check (by an officer, or by you in the Security tab or with `zapper firm screen acme c-1 --approve`).

## 10. Watch it work

```
zapper firm journal acme -f             # mail, states, turns, access requests, publishes
zapper firm transcript acme ada         # what a worker's agent did
zapper firm security acme               # access requests and standing approvals
zapper drive ls acme                    # what workers published
```

In the UI: envelopes move along the org chart as workers mail each other, the Messages tab holds every thread, Security shows the officers' decisions and anything escalated to you, and the Shared drive tab opens what was published. The product the engineers deploy replaces the landing page at `http://127.0.0.1:8080`, and the cluster's Kubernetes dashboard is at `http://127.0.0.1:8081` (the UI has its sign-in token).

The budget you approved caps the run, and `budget` in the config limits the turns per day; `zapper firm budget acme` and `zapper firm status acme` show what's used.

## 11. End the run

```
zapper firm down acme
```

The run stays on disk: `zapper firm runs acme` lists runs, and the UI's Runs tab replays one. The next `zapper firm up acme` starts a fresh run.

## Next

- [Office](office.md): firms, mail, the shared drive, security officers, the UI.
- [Policy](policy.md): writing rules, secrets, approvals.
- [Configuration](configuration.md): profiles, firms and the config file.
- [Commands](commands.md): every command.
