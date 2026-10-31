[← Docs](README.md) · **Workbench**

# The workbench

A layer for one person's sandboxes ([proposal 12, part 2](proposals/12-single-user-layer.md#part-2-the-workbench)): `zapper-workbench`, a process of its own beside the daemon. It is a client of zapper's API on zapper's socket, so it is zapper's operator, and it serves an API and a UI of its own through a **gateway** that decides who may do what. Its facilities (secrets, the drive, the configuration's repository, SSH keys, the wizard) come in later phases; this is phase 1: the gateway, its tokens, the session views and the facility interface.

```
zapper-workbench serve                 # the API (/w/v1) and the UI, on 127.0.0.1:7880; prints the UI's address
zapper-workbench url                   # that address again
zapper-workbench token show            # the owner's token, alone on stdout: sign in with it at /login
zapper-workbench token add ci -l project=x --rights read,create,control
zapper-workbench token ls ; zapper-workbench token rm ci
```

Run it as the same user as `zapper serve`: it finds zapper's socket the way the `zapper` CLI does (`$ZAPPER_SOCKET`, else under `$XDG_RUNTIME_DIR`).

## Tokens

- **The owner's**, in `~/.config/zapper-workbench/token` (mode 0600 in a 0700 directory), made on first use: every right, every session. `serve` prints it when it makes it, the first time, on stderr with the address; after that `token show` prints it (alone, on stdout), so a service's log doesn't get it at every start.
- **Others**, made with `token add`: each reaches only sessions that have all its labels (`-l key=value`, repeatable; none for every session), with only its rights. The token is printed once and kept only as its SHA-256, in `tokens.json`. Added and removed tokens apply at once.

| Right | Lets the token |
|---|---|
| `read` | see its sessions: info, events, audit, requests, terminals (read-only), rules, approvals |
| `create` | create sessions, which get its labels |
| `control` | stop, start and remove its sessions |
| `terminal` | open terminals in its sessions |
| `answer` | answer its sessions' approvals |
| `rules` | add and remove rules in its sessions' own layers |

A token never travels in an address, where it would be kept in shell and browser histories and in logs. Automation sends it as `Authorization: Bearer TOKEN`. In the UI you sign in at `/login`: the token goes in a password field, and the page sends it in a request's body; the answer sets it as a strict, `HttpOnly` cookie, and **Sign out** removes it. The browser's requests are checked as zapper's UI's are: a local `Host` on loopback (no DNS rebinding), and for a write (signing in and out among them) the `X-Workbench-UI: 1` header and an `Origin` naming the UI. Without the cookie, the page sends you to `/login`, and the API answers `401`. Responses aren't cached.

## What the gateway does for a client

It never passes a request through as it came:

- **Lists, events and approvals** are filtered to the token's sessions; a session it doesn't reach is answered as one that isn't there. Events no session has (config changes, services) go only to a token that reaches every session. Events resume through zapper's own cursors (`after=EPOCH:SEQ`, `Last-Event-ID`), passed through, so a restart of the workbench loses no client's place.
- **A session it creates** gets the token's labels, and `workbench=1`. Its `{workdir}` is a directory of the workbench's (`~/.local/share/zapper-workbench/work/NAME`, or a new empty one), never a path a client names on the host. Rules at creation need `rules`.
- **A terminal** runs as the workload user, never as `system`, and its `shell` events name the token (`workbench:NAME`).
- **Rules** go only to the session's own layer: a rule of another layer can't be removed through a session.
- **A start with changed disks** (`accept_changed_disks`) is the owner's decision only.

## API

Under `/w/v1`, each needing the right in brackets:

| Route | |
|---|---|
| `GET /status` | zapper's version and epoch, and the token's name, labels and rights |
| `GET /profiles` | profile names [read] |
| `GET /sessions`, `GET /sessions/{id}` | [read] |
| `POST /sessions` | `{profile, workdir?, labels?, persistent?, ssh?, rules?}` [create] |
| `POST /sessions/{id}/stop`, `POST /sessions/{id}/start`, `DELETE /sessions/{id}` | [control] |
| `GET /sessions/{id}/audit\|verify\|requests\|terminals`, `GET /sessions/{id}/requests/{n}`, `GET /sessions/{id}/terminals/{n}` (its output, server-sent events) | [read] |
| `GET /sessions/{id}/rules`; `POST /sessions/{id}/rules` `{rule, position?}`, `DELETE /sessions/{id}/rules/{rule}` | [read]; [rules] |
| `GET /sessions/{id}/attach?tty=RxC&argv=JSON&persist=bool&terminal=N` | upgrade to a terminal, frames as zapper's [terminal] |
| `GET /approvals`; `POST /approvals/{id}` `{allow, remember?}` | [read]; [answer] |
| `GET /events[?session=ID&history=true&after=EPOCH:SEQ]` | [read] |
| `GET /facilities` | the facilities and their panels |
| `/w/v1/<facility>/…` | each facility's own routes |

## Facilities

A facility implements `zapper_workbench::facility::Facility`: its name (its routes are under `/w/v1/<name>/`, behind the gateway, each request's grant in its extensions), its routes, its panel in the UI (a title, and a script served as a file of its own, `/panels/<name>.js`, which builds its elements in `[data-facility="<name>"]` with text as text, as the page's own scripts do), the services it registers in zapper at start (`POST /v1/services`), and what it does with each of zapper's events, which the workbench follows for it, resuming where it left off. None ships yet; the next phases add them.

## Listening beyond loopback

`--listen 0.0.0.0:7880` serves the API and UI to the network, in plain HTTP: for a trusted network, or behind a TLS proxy. Whoever has a token has its rights. The VMs can't reach it: the host's own addresses are closed to them.
