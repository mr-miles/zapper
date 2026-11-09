[← Docs](README.md) · **zapperclaw**

# zapperclaw

`zapperclaw` is the office cut down to what an always-on set of agents needs: a post office, a foreman per worker and a drive ([proposal 09](proposals/09-zapperclaw.md), step 2). It runs in the office sandbox the [warden](warden.md) makes (`zc-office`), with no model credential and no network but the warden. It has no firm logic: no hiring, budgets, contracts, plans, scenarios or officers. The workers are a fixed roster, and anyone on it may mail anyone.

## How it reaches the workers

Only through the warden ([crates/zapperclaw/src/workers.rs](../crates/zapperclaw/src/workers.rs)), in place of the office's `OfficeClient`:

| Call | Warden route |
|---|---|
| `ensure(name, profile)` | `PUT /workers/{name}` |
| `delete(name)` | `DELETE /workers/{name}` |
| `list()` | `GET /workers` |
| `agent(name, resume?)` | `POST /workers/{name}/agent` with a one-time ticket; the agent's stream then arrives at the agent port, opened with `ZCAGENT <ticket>`, and the foreman drives it |

At start it asks the warden for each roster worker's sandbox, and runs a foreman for each. A worker the warden can't make yet (a cap, a gateway error) shows as `error` and is asked for again, with a growing pause.

## What it serves

Three ports in the office sandbox, each reached only through a warden forward:

| Port | For |
|---|---|
| Edge (8970) | The workers' supervisors, through the warden's forward of `127.0.0.1:8970` on the host. The token grant (`zapper_openshell::edge`, SVIDs checked against the driver's keys as the warden's `/jwks` passes them on); a caller is the roster worker whose sandbox the warden made under that id. Tools at `/mcp`, the drive at `/drive/` |
| Board (8099) | You, through `127.0.0.1:8974` on the host. Every request needs the board's token; a write also needs `X-Zapper-UI: 1`, a loopback `Host`, and an `Origin`, if any, naming it |
| Agent (8973) | Workers' agent streams, which the warden connects; a connection without a ticket the office is waiting for is closed |

The tools: `mail_send`, `mail_reply`, `mail_inbox`, `mail_read`, `mail_thread`, `desk` (what you wait for and what is awaited of you), `directory`, `drive_publish`, `drive_list`, `drive_fetch`, and `stop_check` (Claude Code's Stop hook: mail that came during the turn), and `instructions` (the worker's brief). A message may ask for an answer by a time (`due: "in 2h"`); unanswered, the post office reminds its recipients twice, then tells its sender. Limits: 64 KiB a body, 16 recipients, 200 messages a thread, 40 messages among workers in a thread before the board writes in it again, 60 messages a sender in ten minutes.

The drive is the workbench's, over the edge's HTTP: a worker writes to its own outbox (`PUT /drive/mine/<path>`, which is `/drive/outbox/<name>/<path>`), `drive_publish` copies a file to the shared drive (`/drive/inbox/<name>/<path>`), and everyone reads the shared drive. The workers' `office` command does it: `office drive put LOCAL [PATH]`, `office drive publish LOCAL [PATH]` (put, then publish), `office drive get PATH [LOCAL]`, `office drive ls [PATH]`. Nothing goes over exec, and nothing is staged per worker.

The board's API: `GET /api/workers`, `/api/inbox`, `/api/threads`, `/api/thread/{id}`, `/api/desk`, `/api/transcript/{worker}?lines=N`, `/api/drive/{path}`; `POST /api/mail {to, subject, body, in_reply_to?}`. `/` is a page that shows them, and `/?token=…` sets the cookie it uses.

## Config

[config/zapperclaw/zapperclaw.yaml](../config/zapperclaw/zapperclaw.yaml), which the warden copies into the office sandbox at `~/.config/zapperclaw` (`office.config` in [config/warden.yaml](../config/warden.yaml)):

```yaml
workers:
  research: { profile: claude, instructions: research.md, about: finds things out }
  ops: { profile: claude, instructions: ops.md }
max_concurrent: 2   # turns at once
turn_timeout: 30m
relays: []          # address namespaces for relays (wa, tg, cron), later
```

Each profile must be one the warden offers (`profiles:` in `warden.yaml`), which sets its providers and caps. A worker's brief (who they are, their instructions, who else works here) comes from the edge: Claude Code's `SessionStart` hook, which the warden's worker templates set, runs `office instructions` (the `instructions` tool) when a session starts, but not when it resumes. A harness without the hook (pi) gets it in its conversation's first wake instead. What the warden sets is in the environment, and the office can't change it: `ZAPPERCLAW_WARDEN_URL`, the three ports, and `ZAPPERCLAW_BOARD_TOKEN`.

State is in the office sandbox, under `~/.local/state/zapperclaw`, on a disk that outlives restarts: the journal (`journal.jsonl`), the post office's database (`office.db`), each worker's transcript, the drive, and the edge's token key (`edge-key`).

## Restarts

The post office's state changes only by events ([crates/zapperclaw/src/events.rs](../crates/zapperclaw/src/events.rs)): a message sent, read or shown in a turn, a reminder, an answer settled, a turn started or ended, a conversation. It decides them (the rules, the clock, the numbering) and a store applies them and nothing else, so the same events make the same state. Each batch is one journal line (`"type": "events"`), and the database records the last batch it applied: after a crash between the two it catches up from the journal, and deleted it is made again from it. `store:` in `zapperclaw.yaml` picks where the state is kept: `both` (the default: events journaled, and the database), `database` (no events in the journal: nothing to rebuild from), or `journal` (memory, made again from the journal at every start).

Started again, the office takes up where it was:

- The mail is as it was: threads, who has read what, what is awaited and how many reminders went.
- A turn it stopped in is ended, and its mail (and what the Stop hook handed over in it) is unread again, shown in the next wake with a note that some of it may be handled already.
- Each worker's agent resumes its conversation (`--resume`), so it isn't briefed again.
- Workers' tokens are signed with the edge's key, not kept in a table, so they still work. The warden's token for the office is signed with its own key, in its state on the host, and survives the warden's restart the same way.
- When the office's end of an agent's stream closes, the warden tells the agent its input ended and waits for it to exit (up to 15 minutes) before the worker may have another: the resumed agent never runs beside the old one.
- After a host restart the warden waits (up to 10 minutes) for the office's sandbox to be ready.

## Running it

```
make build                          # zapperclaw is built for the host; the image copies it
make image IMAGE=office-fake        # the fake workers' image
make image IMAGE=zapperclaw-office  # the office sandbox's
zapper-warden --config config/warden.yaml apply
zapper-warden --config config/warden.yaml serve
```

Then open `http://127.0.0.1:8974/?token=$(cat ~/.local/state/zapperclaw/board-token)`. The office's output is in the warden's log.

[tests/e2e-zapperclaw.sh](../tests/e2e-zapperclaw.sh) is the end-to-end test: the warden runs zapperclaw, zapperclaw has the warden make its one worker (a fake Claude Code that answers the board, [tests/fake/zc-echo.yaml](../tests/fake/zc-echo.yaml)), the board mails it, and its answer comes back in the thread through its tools at the edge; asked to, it publishes a file with `office drive` and fetches it back. The unit tests (`cargo test -p zapperclaw`) play the warden and the agents in memory: the post office, the foreman (wakes, the Stop hook, an agent that exits or runs too long, the turn limit), the tools, the drive, the edge's callers and the board's checks.

## Not yet

- To check on a gateway: that the supervisor adds the office's token to zapperclaw's own requests to the warden (as it does curl's: `/usr/local/bin/zapperclaw` is in the `warden` provider's binaries) when the warden's exec starts it; and that the host's build of zapperclaw runs on the image's glibc (Ubuntu 26.04).
- Known edges, from review:
  - Anyone in a thread may add recipients when replying, and they can then read the whole thread, the board's messages included.
  - The board's cookie, like any cookie, goes to every port on 127.0.0.1. Writes still need `X-Zapper-UI`, which another origin can't send without the board's leave.
  - A turn that runs too long drops the office's end of the agent's stream: the agent is told its input ended, but finishes what it was doing before it exits, and only then may the worker have another (the office tries again every 30 seconds).
  - The fan-out guard's recent sends aren't kept: a restart resets it.
  - If the warden loses the office's exec stream while the process lives on, the restart's copy can't bind the ports and exits; the first runs on unwatched.
