# A drive in zapper's core: files shared with sessions under policy

A directory on the host that the operator fills and empties with `scp`, `sftp` or `zapper drive`, and that sessions reach over HTTPS at `drive.zapper`, each path read or written only as the session's `drive` rules allow. Files move between the operator and the sessions without the operator ever copying into a VM, and without the host unpacking an archive a guest made. It is part of the core, so it works with `OFFICE=0`. [Proposal 10](10-ssh-gateway.md)'s SSH gateway serves it as `drive.zapper` for `scp` and `sftp`.

## Today

- **Into a session**: mounts copy host directories into disks at boot (`engine.mounts`); nothing copies in once a session runs. Firecracker can't share folders.
- **Out of a session**: `zapper cp ID:/path DIR` runs the guest's `tar` as root without exec supervision (a `system` shell) and extracts its output on the host with GNU tar (`crates/zapper/src/main.rs`, `Cmd::Cp`). The archive is the guest's. GNU tar's defaults refuse `..` members and absolute paths, but symlinks the guest put in the archive are recreated in `DIR`.
- **The office's shared drive** (`crates/zapper-office/src/drive.rs`) is for firms only: versioned artifacts stored by content hash, in spaces with grants, published and fetched with office tools over the MCP gateway. The daemon copies the named files out of or into the VM over the system terminal channel, keeps regular files with clean relative paths, and drops links and devices. None of it exists without the office.
- **In-process services** exist in the core: `Target::Handler { tls, handler }` (`crates/zapper-core/src/daemon/services.rs`), whose `serve(session, io)` is handed the calling session, taken from the VM and never from the request. With `tls`, the daemon terminates TLS with the session's CA, which the guest trusts. The firm's git server is one (`zapper-office/src/facility.rs`).

## Decisions

| | |
|---|---|
| What it stores | Plain files in a directory tree, on the host, under `daemon.drive.dir` (default `~/.local/state/zapper/drive`). No versions, no content addressing: those stay the office drive's. |
| The operator's way in | SFTP through the SSH gateway as `drive.zapper` ([proposal 10](10-ssh-gateway.md)), and `zapper drive ls\|get\|put\|rm\|mkdir` over the REST API, which work without the gateway. |
| The guests' way in | An in-process service at `drive.zapper:443`, TLS from the session's CA, plain HTTP: `GET`, `HEAD`, `PUT`, `DELETE`. Any image with `curl` works; no `zapper-agent` change; vsock sessions reach it through the proxy variables. |
| Who decides | A new rule kind, `drive`, beside `net`, `exec` and `fs`, in the same layers (session → profile → global, `final`, defaults), evaluated by the handler on every request against the calling session. |
| The default | `deny`. A session with no `drive` rule allowing something can't resolve `drive.zapper` at all. |
| Links | The drive holds regular files and directories only. The host never follows a link inside it, on any side. |

## Rules

```yaml
policy:
  drive:
    - { id: d-in,  path: "inbox/**",             access: read }
    - { id: d-out, path: "outbox/{session}/**",  access: write }   # each session writes only its own folder
    - { id: d-team, path: "teams/{label:team}/**", access: write }  # sessions labelled team=a share teams/a
    - { id: d-sec, path: "secrets/**", access: deny, final: true }
  defaults: { drive: deny }
```

- `access` is `read` (`GET`, `HEAD`, listing), `write` (also `PUT` and `DELETE`), `deny` or `ask`. `ask` waits for an approval as `net` and `exec` asks do, with `--remember session`; in a firm it becomes an access request, like any `ask`.
- `path` is a glob over the normalized path ([paths](#paths)), with `*` within a segment and `**` across segments, as `fs` rules use. `{session}` is the session's ID and `{label:KEY}` the value of one of its labels; a rule whose label the session lacks matches nothing.
- Rules are evaluated per request in the usual order, so a `final` deny in the global layer wins over everything. `zapper eval -s ID drive GET inbox/report.pdf` shows the deciding rule.
- **No `net` rule is needed** for `drive.zapper`, unlike other services: the drive rules are the only rules for it. Its name resolves in a session only if some `drive` rule could allow it something, the same principle as for DNS.

## The guests' side

```
curl -fsS https://drive.zapper/inbox/report.pdf -o report.pdf       # read
curl -fsS https://drive.zapper/inbox/                                # list a directory: JSON
curl -fsS -T results.csv https://drive.zapper/outbox/$ID/results.csv  # write (parents are created)
curl -fsS -X DELETE https://drive.zapper/outbox/$ID/old.csv
```

| Request | Needs | Does |
|---|---|---|
| `GET` / `HEAD` a file | `read` | The file, with `Content-Length`, `Last-Modified` and an `ETag`; `Range` for resuming |
| `GET` a directory (path ending in `/`) | `read` on the directory | `[{name, type, size, modified}]`, only entries the session could `read`, at most 10,000 |
| `PUT` | `write` | Writes to a temporary file beside the target, flushes it, renames it into place, so readers see whole files. Creates missing parent directories. `If-None-Match: *` refuses to replace; `If-Match` refuses unless the `ETag` matches |
| `DELETE` | `write` | Removes a file, or an empty directory |
| Anything else | | `405` |

A session learns its ID from `ZAPPER_SESSION` in its environment (set today) so it can name its own folder. A refused request gets `403` with the rule's ID, as an inspected request does.

## The operator's side

- `sftp drive.zapper`, `scp FILE drive.zapper:inbox/`, `scp -r drive.zapper:outbox/ ./`: SFTP version 3 on the host, served from the directory, through the gateway's `GET /v1/drive/ssh` ([proposal 10](10-ssh-gateway.md#files-the-drive-not-the-guest)). The operator is not subject to `drive` rules, only to the drive's [bounds](#bounds) and [paths](#paths).
- `zapper drive ls [PATH]`, `get PATH [FILE]`, `put FILE PATH`, `rm PATH`, `mkdir PATH` over `/v1/drive/{path}` on the CLI's socket. These work without the gateway, and are what the UI uses.
- Because it is a host directory, the operator can also use it directly. Links placed there are not served (below).

## Paths

Every path, from a guest or the operator, is normalized before any rule sees it, and the normalized path is the one used on disk:

- percent-decoded once (a guest's URL path); then valid UTF-8, relative, at most 4,096 bytes, each segment at most 255;
- refused with `400` if any segment is empty, `.` or `..`, or contains a NUL, a backslash, or a control character; or if the path started with `/` after decoding a guest URL's leading slash;
- names are case-sensitive.

On disk, every open walks the path one segment at a time with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, so a link anywhere in the path, made by the operator or by anything else on the host, fails the request rather than leading out of the drive. A directory listing leaves links, devices, sockets and FIFOs out.

## Bounds

| Bound | Default |
|---|---|
| One file (`daemon.drive.max_file`) | 1 GiB |
| The whole drive (`daemon.drive.max_total`) | 20 GiB, and the host's disk keeps 1 GiB free |
| Written by one session (`daemon.drive.max_session`) | 5 GiB |
| Concurrent requests per session | 8 |
| Listing | 10,000 entries |

A `PUT` past a bound fails as soon as it crosses it and leaves nothing behind.

## Events

Each request is a `drive` event: the session, method, path, bytes moved, the action and the deciding rule. The operator's own changes are `drive` events without a session, so the UI and `zapper events` show both sides. Events, not files beside the drive, record who wrote what.

## Invariants

New, for [AGENTS.md](../../AGENTS.md):

- **The drive is decided on the host, per request**: a session reaches a path only as its `drive` rules allow at that moment, taken from the VM the request came from; a rule change applies to the next request.
- **Paths are normalized before rules see them**, and that is the path opened; the host never follows a link inside the drive.
- **The drive holds what was written to it, no more**: whole files, written through a temporary file and a rename, within its bounds.

`tests/e2e.sh` covers them: a session without a rule can't resolve the name; `read` can't `PUT`; `outbox/{session}` keeps two sessions apart; `..`, `%2e%2e` and an encoded slash are refused; a link the test puts in the host directory isn't served; a file past `max_file` leaves nothing.

## With `zapper cp`, mounts and the office

- **`zapper cp`** stays, for the operator's own maintenance, as a root `tar`. Its host-side `tar` should also stop recreating links (`--no-overwrite-dir`, and dropping symlink members), as the office drive does. The docs point at the drive for moving work out.
- **Mounting part of the drive at boot**, read-only, as a disk like other mounts, so a session needn't fetch its inputs: later, if wanted. It would be a copy at boot, as mounts are.
- **The office's drive** is unchanged. Its spaces could later map onto paths of this one (`spaces/<space>/…`), its grants becoming `drive` rules at a worker's layer, with the artifact metadata and versions staying in the office. That is not part of this proposal.

## Phases

1. **The drive and its rules.** The `drive` rule kind, evaluation and `eval`; the handler at `drive.zapper` with paths, bounds and events; `zapper drive` and `/v1/drive/…`; the e2e checks.
2. **SFTP.** The drive over the SSH gateway as `drive.zapper`, after proposal 10's phase 1.
3. **The UI.** The drive in the web UI: list, upload, download, and its events.
4. **If wanted:** mounting at boot; moving `zapper cp` onto the drive; the office's spaces onto it.

## Open questions

- Whether a session should be able to make a directory with no file in it (WebDAV's `MKCOL`), or whether `PUT`'s created parents are enough.
- Whether `write` should split into `create` and `overwrite`, for an inbox that sessions may add to but not change.
- Whether the operator should see per-session usage against `max_session` in `zapper ps`.
