[← Docs](README.md) · **Workbench**

# The workbench

A layer for one person's sandboxes ([proposal 12, part 2](proposals/12-single-user-layer.md#part-2-the-workbench)): `zapper-workbench`, a process of its own beside the daemon. It is a client of zapper's API on zapper's socket, so it is zapper's operator, and it serves an API and a UI of its own through a **gateway** that decides who may do what. Built so far: phase 1 (the gateway, its tokens, the session views and the facility interface), phase 2 (the [secrets](#secrets) facility), phase 5 (the [drive](#the-drive)) and phase 6 (the [SSH connector](#ssh-keys)). The configuration's repository and the wizard come in later phases.

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
| `secrets` | manage the [secrets](#secrets); and add a rule that uses one (with `rules`), which has a session use it |

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
| `GET /config/diff`, `POST /config/save` | what saving would change in zapper's startup file; save zapper's running config there (the owner's token only: it writes everything running) |
| `GET /secrets`, `GET /secrets/presets`, `PUT /secrets/{name}`, `DELETE /secrets/{name}` | the [secrets](#secrets) facility [secrets] |
| `GET /ssh/keys`, `PUT /ssh/keys/{name}` `{key, reach}`, `DELETE /ssh/keys/{name}`, `GET /ssh/config?host=USER@HOST` | the [SSH keys](#ssh-keys) (the owner's token only) |
| `GET\|PUT\|DELETE /drive/files/{path}`; `GET /drive/rules/usual`, `POST /drive/rules` | the [drive](#the-drive)'s files, and its usual rules added (the owner's token only) |
| `/w/v1/<facility>/…` | each facility's own routes |

## Secrets

The first facility: credentials the workbench keeps for zapper, so they never sit in zapper's config file.

- **A value is a file of its own** in `~/.config/zapper-workbench/secrets/` (mode 0600, in a 0700 directory), trimmed of the whitespace a paste brings, at most 64 KiB.
- **zapper's entry names the file** (`file:` source) with the hosts it may go to and how it reaches a session, set in zapper's running config through `PUT /v1/secrets/{name}`, one secret at a time. zapper reads the file on each use, so replacing a value needs no restart and no config change.
- **Never shown back.** The list shows each secret's name, mode, hosts, where its value comes from, and what uses it (profiles that write `{{secret:NAME}}`, rules that list it); never a value. Replacing one is a write; nothing reads one.
- **Only its own.** It changes and removes only secrets whose file is in its directory; one written in zapper's config by hand is listed, but stays the operator's (`409`).
- **Nothing half done.** A value is written aside, and put in place only once zapper has taken the entry: a secret zapper refuses (a `proxy` secret with no hosts, say) leaves no file behind. A secret something still uses (a rule in the config, a profile, a session's own rule) isn't removed, and keeps its value.
- **Running, not saved.** Changes go to zapper's running config. **Save zapper's running config…** (the owner's, `POST /w/v1/config/save`) shows the diff and writes it to zapper's startup file: everything running, the secrets' entries among it, never their values.

Presets fill in the hosts and mode, and check what a value looks like (its form, not a call to the service):

| Preset | Hosts | Mode | Value | A profile gives it as |
|---|---|---|---|---|
| `claude` | `api.anthropic.com` | `proxy` | `sk-ant-…` (`claude setup-token`) | `CLAUDE_CODE_OAUTH_TOKEN` |
| `openrouter` | `openrouter.ai` | `proxy` | `sk-or-…` | `OPENROUTER_API_KEY` |
| `github` | `api.github.com`, `github.com` | `proxy` | `github_pat_…`, `ghp_…` and the like | `GITHUB_TOKEN` |
| `aws` | `*.amazonaws.com` | `aws` (signed on the host) | credential_process JSON | `AWS_ACCESS_KEY_ID` |
| `database` | the database's, given | `dbauth` | the password; and `user` | (none: a database `protocol:` rule) |

Without a preset, give `hosts` and `mode` (`proxy`, `guest`, `aws`, `dbauth`); a `guest` secret is readable by everything in the session ([security](security.md#secrets-given-to-a-session-can-be-used-by-it)), and the panel says so. `PUT /w/v1/secrets/{name}` takes `{preset?, value?, hosts?, mode?, plaintext?, user?, database?, mechanism?}`: a value for a new secret, any of them to change one.

A secret reaches a session through a rule that lists it (and, for a profile's agent, `{{secret:NAME}}` in its `env`), which the wizard (phase 4) will write; until then, through the session's rules (`POST /w/v1/sessions/{id}/rules` with `secrets: [NAME]`, which takes the `secrets` right) or the config.

## The drive

Files in `~/.local/share/zapper-workbench/drive/` (0700, with `inbox/` and `outbox/`), which you reach through the workbench and sessions reach as `drive.zapper:443`.

- **For sessions it is a [socket service](network.md#socket-services)** for the workbench's sessions (label `workbench=1`): the workbench serves `workbench-drive.sock` in zapper's `state/services/` (`socket_dir` in zapper's status), and zapper terminates the guest's TLS with the session's CA, names the caller in the first line, and decides each request against `net` rules with methods and paths before passing it on. The drive decides no access itself. It registers the service at start and again whenever zapper has lost it (a restart).
- **The usual rules**, which **Let workbench sessions use it…** adds to zapper's global layer (`POST /w/v1/drive/rules`; running config, saved when you save it): read the inbox, and read and write the session's own outbox.

  ```yaml
  - { id: drive-inbox, kind: net, action: allow, host: drive.zapper, ports: [443], methods: [GET, HEAD], path: "/inbox/**" }
  - { id: drive-outbox, kind: net, action: allow, host: drive.zapper, ports: [443], methods: [GET, HEAD, PUT, DELETE], path: "/outbox/{session}/**" }
  ```

  Only the workbench's sessions resolve `drive.zapper`, so these reach no other.
- **What a session does**: `GET` a directory for a JSON listing (`{path, entries: [{name, kind, size, modified}]}`), `GET` or `HEAD` a file, `PUT` a file (written whole or not at all, at most 256 MiB, its directories made as needed), `DELETE` a file or an empty directory: `curl https://drive.zapper/inbox/`, `curl -T report.pdf https://drive.zapper/outbox/$SESSION/`.
- **Every path stays in the drive.** A path is cleaned to its components (relative, no `.`, `..`, empty or hidden ones, at most 32 deep; as the office's drive cleans archive paths) and walked from the drive's root a component at a time without following a link, so a symbolic link anywhere, whoever made it, fails the request instead of leading out. Only regular files and directories are served or listed (at most 2,000 entries); a write goes to a hidden temporary file, renamed into place once whole.
- **Nothing runs.** A session gets text as text (HTML and scripts as `text/plain`, SVG as bytes), with `nosniff`. Your downloads through the workbench are always `application/octet-stream` attachments with `Content-Security-Policy: sandbox`, so a file a session wrote can't run in the workbench's page with your cookie.
- **The owner's token only** reaches it through the workbench (the panel: folders, upload, download, remove).

Not yet: `scp` and `sftp` to `drive.zapper` through the SSH connector, which needs an SFTP server in the gateway.

## SSH keys

The SSH connector ([SSH](ssh.md) has how SSH reaches a sandbox): your public keys, each with a name and the sandboxes it reaches, which the host's sshd checks.

```
zapper-workbench ssh-proxy --key NAME [ID.zapper]   # what sshd runs for a key; the sandbox from $SSH_ORIGINAL_COMMAND
```

- **A key reaches** every sandbox, those with given labels, or given IDs (`reach`: `"all"`, `{"labels": {…}}`, `{"sessions": [ID…]}`); and of those, only one whose SSH is on (`zapper ssh on ID`, or created with `ssh`).
- **Each key is a line of `~/.ssh/authorized_keys`**, between markers the workbench owns; every other line is left as it is. The line is `restrict,command="ZAPPER_SOCKET=… zapper-workbench ssh-proxy --key NAME" KEY NAME`: the key gets no shell on the host and no forwarding, and sshd runs only the proxy. The workbench rewrites the block as keys are added and removed (the file stays 0600, `~/.ssh` 0700).
- **The proxy decides at each connection**: the key still the workbench's, the sandbox one it reaches (another is answered as one that isn't there), and SSH on for it; then it carries SSH as `zapper ssh proxy` does, with the key's name in the terminal's events. A key's reach changed applies at its next connection, without rewriting `authorized_keys`.
- **The owner's token only** manages keys: a key is a shell in the sandboxes it reaches, which a token limited to some must not hand out for more.
- **Your machine's lines**: `GET /w/v1/ssh/config?host=you@host` (and the panel) give the `~/.ssh/config` block, `ProxyCommand ssh -T -i ~/.ssh/YOUR_KEY you@host %h` with `HostKeyAlias zapper`, and the `known_hosts` line for the gateway's host key, which is zapper's own (`~/.config/zapper/ssh_host_ed25519`), so one line serves `zapper ssh` and the workbench.

## Facilities

A facility implements `zapper_workbench::facility::Facility`: its name (its routes are under `/w/v1/<name>/`, behind the gateway, each request's grant in its extensions), its routes, its panel in the UI (a title, and a script served as a file of its own, `/panels/<name>.js`, which builds its elements in `[data-facility="<name>"]` with text as text, as the page's own scripts do), the services it registers in zapper at start (`POST /v1/services`), and what it does with each of zapper's events, which the workbench follows for it, resuming where it left off. None ships yet; the next phases add them.

## Listening beyond loopback

`--listen 0.0.0.0:7880` serves the API and UI to the network, in plain HTTP: for a trusted network, or behind a TLS proxy. Whoever has a token has its rights. The VMs can't reach it: the host's own addresses are closed to them.
