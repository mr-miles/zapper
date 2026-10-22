[← README](../README.md) · [Docs](README.md) · **API**

# API

## Overview

The zapper daemon serves the API, for The Office and the sessions under it, on its unix socket, mode 0600 in a 0700 directory; whoever can open the socket can use the API. Request bodies are YAML or JSON (JSON only for a firm's `up` and `drive/upload`), and responses are JSON unless noted. Errors are `{"error": "…"}` with status 400 or 404. The UI serves the routes marked **ui** under `/api` on its TCP address, behind its token: the reads, answering approvals, and the board's writes to firms ([instrumentation: the UI](instrumentation.md#the-ui)).

```
curl --unix-socket $XDG_RUNTIME_DIR/zapper/api.sock http://zapper/v1/status
```

## Firms

The office's routes ([office](office.md)), for the board. F is a firm, RUN a run id or `current` (the firm's current or last run). The UI serves the reads and the board's writes (marked **ui**) under `/api`.

| Method and path | ui | Purpose |
|---|---|---|
| `GET /v1/firms` | ✓ | Firms with their runs' states |
| `GET /v1/firms/F` | ✓ | The chart (live, while a run goes), `officers` (who holds security now), `start` (the config's founders and terms, and the charter's articles, for the start form), the run's `articles`, and the run: workers' states, usage, budget, `funds` (seed, approved, balances, budget requests), `contracts` (offers, quotes, counters, agreed terms, how each ended) |
| `POST /v1/firms/F/up` · `/down` · `/pause {paused}` | ✓ | Start a fresh run (optionally `{company, ceo, seed, ceo_prompt, articles, security_share, max_concurrent, daily_turns, scenario, hires, script, task}`; the company starts with its CEO for this run), stop it, pause or resume it |
| `GET /v1/firms/F/scenarios` | ✓ | The firm's scenarios as configured |
| `GET /v1/firms/F/credentials` | ✓ | Every grant (worker, resource, level, by, expiry) and credential request of the running firm |
| `POST /v1/firms/F/credentials/requests/ID` | ✓ | The board decides a credential request: `{grant, level?, expires_in?, note}` |
| `POST /v1/firms/F/credentials/grants` | ✓ | The board grants directly: `{worker, resource, level, expires_in?, note?}` (resource `git`, `cluster` or `drive:<space>`) |
| `DELETE /v1/firms/F/credentials/grants/WORKER/RESOURCE?reason=` | ✓ | The board revokes a grant |
| `GET /v1/firms/F/runs` | ✓ | Past runs, newest first |
| `DELETE /v1/firms/F/runs/RUN` | ✓ | Delete a past run (not the running one) |
| `PUT /v1/firms/F/runs/RUN/note` | ✓ | `{note}`: the board's note on a run, any text up to 4000 bytes; empty removes it. `GET …/runs` lists it as `note` |
| `GET /v1/firms/F/runs/RUN/verify` | ✓ | The run's record checked against the keeper's chains and signatures |
| `GET /v1/firms/F/runs/RUN/config` | ✓ | `{config, skills}`: the config the run started with (YAML, secret values hidden; null for older runs) and each role's skills |
| `GET /v1/firms/F/runs/RUN/journal[?follow=true&from=OFFSET]` | ✓ | The journal (`application/x-ndjson`), or followed as server-sent events, one entry per `data:` |
| `GET /v1/firms/F/runs/RUN/transcripts/WORKER[?follow=true]` | ✓ | A worker's agent events (Claude Code's stream-json, or pi's) and wake prompts |
| `GET /v1/firms/F/runs/RUN/drive/ARTIFACT/VERSION/PATH` | ✓ | A published file, with its own content type and `Content-Security-Policy: sandbox` |
| `GET /v1/firms/F/runs/RUN/git` | ✓ | The run's commits on all branches, newest first |
| `GET /v1/firms/F/runs/RUN/git/branches` | ✓ | Branches, `main` first, with their last commit |
| `GET /v1/firms/F/runs/RUN/git/log?ref=R` | ✓ | A branch's commits |
| `GET /v1/firms/F/runs/RUN/git/tree?ref=R&path=P` | ✓ | A directory's entries at a branch |
| `GET /v1/firms/F/runs/RUN/git/blob?ref=R&path=P` | ✓ | A file's text (up to 1 MiB) |
| `GET /v1/firms/F/runs/RUN/git/commits/ID` | ✓ | A commit's message, files changed and diff |
| `GET /v1/firms/F/mail[?for=WORKER&unread=true]` | ✓ | The board's (or a worker's) inbox |
| `POST /v1/firms/F/mail` | ✓ | The board sends `{to, cc?, subject, body, kind, due?, in_reply_to?, attachments?}` |
| `POST /v1/firms/F/mail/read` | ✓ | `{ids}` the board has read |
| `POST /v1/firms/F/security/screenings/CHECK` | ✓ | `{clear, note?}`: the board clears a new hire's background check, or objects (which cancels the hire) |
| `POST /v1/firms/F/budget/REQUEST` | ✓ | `{approve, note?}`: the board's answer to the CEO's budget request, as a reply in its thread |
| `POST /v1/firms/F/fund` · `/release` | ✓ | `{worker, tokens, reason}`: the board gives a worker tokens / `{worker, reason}`: lets a worker go |
| `POST /v1/firms/F/drive/upload` | ✓ | `{files: [{name, data}], title?, thread?}` with `data` in base64: files from the board (10 files, 5 MiB together) stored as one artifact by the board; returns its `ref` to attach to mail |
| `GET /v1/firms/F/threads/T` | ✓ | A thread with its messages |
| `GET /v1/firms/F/drive` | ✓ | The shared drive's artifacts |
| `GET /v1/firms/F/security` | ✓ | Access requests and standing approvals |
| `POST /v1/firms/F/security/requests/R` | ✓ | The board decides `{allow, scope, reason}` |
| `DELETE /v1/firms/F/security/rules/RULE` | ✓ | Revoke a standing approval |
| `POST /v1/firms/F/security/rules/RULE/promote` | ✓ | Copy it into the running config, at the role's profile |

## Sessions, rules and config

The layer workers run on: zapper's sessions, policy and config.

| Method and path | ui | Purpose |
|---|---|---|
| `GET /v1/status` | ✓ | Version, session counts, unsaved changes, pending approvals |
| `GET /v1/record` | ✓ | Who keeps the record and whether it is being written |
| `GET /v1/config[?format=json]` | ✓ | Running config, YAML by default, secret values redacted |
| `PUT /v1/config` | | Replace the running config (validated; `<redacted>` keeps old values) |
| `GET /v1/config/startup` | ✓ | Startup file, normalised and redacted |
| `GET /v1/config/diff` | ✓ | Unified diff, startup → running (text) |
| `POST /v1/config/save` | | Write the running config to the startup file |
| `POST /v1/config/reload` | | Replace the running config with the startup file |
| `GET /v1/rules[?scope=S]` | ✓ | Rules with `scope`, `kind` and fields |
| `POST /v1/rules` | | `{scope, rule: {kind, …}, position?}` → the rule with its id |
| `DELETE /v1/rules/{id}` | | Remove a rule from any layer |
| `GET /v1/profiles` | ✓ | Summaries |
| `GET\|PUT\|DELETE /v1/profiles/{name}` | | Profile and its resolved form / set / remove |
| `GET /v1/sessions` | ✓ | Sessions, each with its `labels` (a firm's: `firm`, `run`, `member` and `office`, `worker` or `facility`) |
| `POST /v1/sessions` | | `{profile, workdir, rules?, labels?}`; returns once the guest is ready. Labels are `{key: value}`, short words; `firm`, `run`, `member` and `office` are the office's |
| `GET /v1/sessions/{id}` | ✓ | One session |
| `POST /v1/sessions/{id}/stop` | | Stop, keep listed |
| `POST /v1/sessions/{id}/prompt` | | `{text}`: a task for a driven session's agent, queued for its next turn → `202` `{queued, driver}`. `409` if the driver is off or the session isn't driven or running, `403` for a firm's worker (mail them instead) |
| `DELETE /v1/sessions/{id}` | | Stop and forget |
| `GET /v1/sessions/{id}/audit` | ✓ | Audit log (`application/x-ndjson`) |
| `GET /v1/sessions/{id}/requests` | ✓ | The session's requests and connections: `id`, `method`, `url`, `status`, `action`, `rule`, `type`, `req_size`, `resp_size`, `ttfb_ms`, `total_ms`, `inspected` |
| `GET /v1/sessions/{id}/requests/{n}` | ✓ | One request with `request_headers`, `response_headers`, `request_body` and `response_body` (`text` or `base64`, `total`, `kept`, `truncated`, `encoding`); added secret values appear as `***` |
| `GET /v1/sessions/{id}/terminals` | ✓ | The session's terminals: `id`, `argv`, `rows`, `cols`, `open` |
| `GET /v1/sessions/{id}/terminals/{n}/stream` | ✓ | Server-sent events, read-only: the terminal's kept output, then live output. Each `data:` is `{"r": [rows, cols]}`, `{"o": "<base64 bytes>"}`, or `{"end": exit_code}` |
| `GET /v1/sessions/{id}/attach?tty=RxC&argv=JSON&system=bool` | | Upgrade (`Upgrade: zapper-shell`) to a terminal; frames below |
| `POST /v1/evaluate` | | `{session?\|profile?, net?: {host, port, method?, path?}, exec?: {path, argv}}` → decision |
| `GET /v1/events[?session=ID&history=true]` | ✓ | Server-sent events, one JSON event per `data:` line |
| `GET /v1/services` | ✓ | Services and zones: `id`, `kind` (`service` or `zone`), `name`, `port`, `sessions` (labels; every session when empty), `to`, `owner` ([network: services](network.md#services)) |
| `POST /v1/services` | | `{name, port, sessions?, to: {addr: "ip:port"} \| {session: ID, port}}` → the service with its `id` |
| `DELETE /v1/services/{id}` | | Remove a service added through the API; `409` for another owner's (a firm's) |
| `GET /v1/approvals` | ✓ | Pending approvals with suggested rules |
| `POST /v1/approvals/{id}` | ✓ | `{allow, remember?: session}`; through the UI, also needs `X-Zapper-UI: 1` and an `Origin` naming the UI |

## Terminal frames

After the upgrade, both directions carry frames: a 1-byte type, a 4-byte big-endian length, then the payload (at most 64 KiB).

| Type | Direction | Payload |
|---|---|---|
| 0 data | both | stdin to the guest; stdout (and stderr with a tty) from it |
| 1 stderr | guest → client | stderr without a tty |
| 2 resize | client → guest | rows u16, cols u16 |
| 3 eof | client → guest | stdin closed |
| 4 exit | guest → client | exit code i32; the last frame |

`system=true` runs as root without exec supervision. The CLI uses it for `zapper cp`.

## References

- Sources: `crates/zapper/src/daemon/api.rs`, `crates/zapper/src/client.rs`, `crates/zapper-policy/src/proto.rs`
