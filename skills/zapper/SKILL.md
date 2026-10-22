---
name: zapper
description: Operate zapper, which runs AI agents in Firecracker microVMs with their network, credentials and program starts decided on the host, and The Office, firms of AI workers built on it. Use when the user wants to start, attach to or stop a session (Claude Code in a sandbox with `zapper run -p claude`), inspect what a session did (network, execs, exits, terminal recording), allow or block a host or command for a session or profile, answer a pending approval, change, diff, save or reload the zapper config, or build an image or kernel; or to start, stop, pause or watch a firm, write to its workers as the board, read its mail, journal, transcripts or shared drive, decide or revoke its access requests, or replay a past run.
---

# Operating zapper

zapper runs AI agents in microVMs; The Office runs firms of AI workers on it. The daemon (`zapper serve`) owns the sessions, and the firms whose workers run in them; the `zapper` CLI talks to it over a unix socket. Before acting, find out what is running and whether the config has unsaved changes:

```
zapper ps                 # sessions: id, profile, state, counters
zapper firm ls            # firms and their runs' states
zapper config diff        # unsaved running-config changes ("no unsaved changes")
```

If the CLI says it can't connect, the daemon isn't running: start it with `zapper serve` in the background and a log (`zapper serve > /tmp/zapper.log 2>&1 &`), then give the user `tail -f /tmp/zapper.log`.

## Rules

- A firm's run and its workers' sessions may be in use. Before `firm down`, `stop`, `rm`, `config reload` or restarting the daemon, check `zapper firm status` and `zapper ps` and say what you're about to end. A restart or `firm down` ends the run, which can't be resumed, and a restart loses unsaved config.
- A firm spends the user's Claude usage: check `zapper firm status` (turns and tokens today) and the budget before starting one, and say what you are about to start.
- API changes edit the running config only. Run `zapper config save` when the user wants a change kept, and show `zapper config diff` first.
- Prefer the narrowest change: a session rule (`session:ID`) over a profile rule, and a profile rule over a global one. Prefer `methods`/`path` limits over allowing a whole host.
- On a remote host, the UI and a firm's dashboard are on its loopback: offer an SSH tunnel first, and serving them to the network (`daemon.ui`, `dashboard_port`) only with its warning ([a remote host](../../docs/configuration.md#a-remote-host)).
- Keep secret values out of rules, profiles and commands. Secrets come from `env` or `file` sources in the config: `proxy` secrets are bound to `hosts`, `guest` secrets are delivered into the session. The user creates the files (the Claude token and the OpenRouter key, as [model credentials](../../docs/configuration.md#model-credentials-claude-and-openrouter) describes); don't ask for them, read or print them.
- Don't decide access requests or approve requests on the user's behalf unless they said to. Show the pending request and ask.
- Runs in `state/firms/` (mail, transcripts, published files), audit logs and recordings are sensitive: summarise them rather than pasting them wholesale, and don't prune or delete them unasked.

## Sessions

Every agent runs in a session, a firm's workers included; `zapper ps` lists them. To see what one did:

```
zapper events --history -s ID            # live and recent, one line each
zapper audit ID | jq -r 'select(.type=="net" and .action=="deny") | "\(.host) \(.error // "")"' | sort | uniq -c
zapper audit ID | jq -r 'select(.type=="exec") | .argv | join(" ")'
ls ~/.local/state/zapper/sessions/ID/    # audit.jsonl, shell-N.cast (asciinema play), console.log
zapper exec ID -- git -C /work status    # no terminal; exit code passes through
zapper exec ID --system -- CMD           # as root, unsupervised: the operator's maintenance (e.g. the cluster VM)
```

Starting sessions:

```
zapper run -C $(mktemp -d)               # shell on a copy of an empty directory, removed on exit
zapper run -p claude --keep              # Claude Code on a copy of the current directory; other hosts are asks
zapper run -p PROFILE --keep             # keep it after the command exits
zapper attach ID                         # another terminal (profile command, or -- CMD)
zapper tell ID "a task"                  # a driven session (agent.driven): queued for its next turn; not a firm's worker (mail them)
zapper cp ID:/work ./out                 # results out (mounts are copies)
zapper stop ID ; zapper rm ID
```

## Rules and approvals

```
zapper rules --scope profile:office
zapper rule add session:ID '{kind: net, action: allow, host: pypi.org, ports: [443], methods: [GET]}'
zapper rule add profile:office '{kind: exec, action: deny, path: ssh}'
zapper rule add global '{kind: net, action: deny, host: "*.example.internal", final: true}'
zapper rule rm net-7
zapper eval -s ID net github.com:443 POST /org/repo/pulls      # which rule decides, before changing anything
zapper approvals ; zapper approve ABC --remember session        # outside firms; or: zapper deny ABC
```

Rule changes apply to running workers at once. Firm profiles can't have `exec` rules with `ask`, and in a firm a network `ask` becomes an access request (`zapper firm security`), not an approval.

## Config

```
zapper config show | diff | save | reload | edit
zapper profile ls ; zapper profile show office-engineer
```

## Images

```
zapper image ls
make image IMAGE=office > /tmp/zapper-image.log 2>&1     # background; tail -f the log
make image IMAGE=k3s ; make kernel KERNEL=k8s             # the firm's cluster
```

## Firms (The Office)

A firm is an org chart of AI workers, each Claude Code (or pi) in its own session ([docs/office.md](../../docs/office.md)). You act as the board.

```
zapper firm ls ; zapper firm status acme          # before anything: is a run going, what are workers doing
zapper firm up acme                               # a fresh, idle run (the cluster first, if the firm has one)
zapper firm scenarios acme                        # simulations: hires with extra instructions, scripted board actions
zapper firm up acme --scenario red-team           # play one (Kickoff lets the board change its hires and steps first)
zapper firm up acme --task task.md                # with the board's task (first line the subject), sent to the CEO once it's up
zapper firm up acme --articles law.md               # with the board's own articles; the CEO holds security until they hire an officer
zapper mail send acme --to barbara --kind assignment --subject "…" --body "…"
zapper firm journal acme -f                        # mail, states, turns, access requests, publishes
zapper firm transcript acme ada                    # what a worker's agent did
zapper mail ls acme ; zapper mail thread acme t-1  # the board's inbox
zapper firm security acme                          # access requests, standing approvals
zapper firm fund acme guido 200k --reason "…"     # the board gives tokens to a worker who ran out (officers answer to the board, so only it can)
zapper firm release acme guido --reason "…"        # the board lets a worker go (officers answer only to the board)
zapper firm budget acme                            # the run's budget, balances, the CEO's budget requests
zapper firm budget acme approve b-1                # the board's answer to a budget request (or: decline b-1 --note "…")
zapper firm screen acme c-1 --clear                 # a new hire's background check, when no officer answered (or --object --note "…")
zapper firm decide acme r-3 --allow --scope worker --reason "…"
zapper firm revoke acme RULE                       # remove a standing approval
zapper drive ls acme ; zapper drive get acme a-2 ./out
zapper firm pause acme ; zapper firm resume acme   # stop starting turns / start again
zapper firm down acme                              # ends the run; it stays on disk for replay
zapper firm runs acme ; zapper firm replay acme RUN
zapper firm note acme RUN "baseline: clean delegation"   # the board's note on a run, to find it again ("" removes it)
zapper-inspect people RUN ; zapper-inspect export RUN operative   # past runs from disk, no daemon: people, the scenario's operative, a transcript as Markdown
```

A firm with a run budget (`budget.seed`) starts with its CEO alone, who holds security until they hire an officer: the CEO scopes the board's task, asks for a budget, and hires the team once the board approves it. Approving or declining budget requests is the board's (the user's) decision: ask them, don't decide it for them.

The UI URL is in `~/.local/state/zapper/ui-url`; the user can watch the firm and decide access requests there too. The product is on the firm's `product_port` (`http://127.0.0.1:8080` in the example), the cluster's dashboard on `dashboard_port` (`8081`).

## More

- The office: [docs/office.md](../../docs/office.md)
- Rules and secrets: [docs/policy.md](../../docs/policy.md)
- Config keys and profiles: [docs/configuration.md](../../docs/configuration.md)
- Security model: [docs/security.md](../../docs/security.md)
- Every command: [docs/commands.md](../../docs/commands.md)

## Installing this skill

Claude Code loads it from this repository through `.claude/skills/zapper`. To use it elsewhere, copy `skills/zapper/` to `~/.claude/skills/zapper/`; other agents can read this file directly.
