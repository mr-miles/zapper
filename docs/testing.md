[← README](../README.md) · [Docs](README.md) · **Testing**

# Testing

## Tiers

| Tier | Command | Needs | Checks |
|---|---|---|---|
| Unit | `make test` | Rust toolchain | Rule evaluation and layering, final rules, defaults, inspection, DNS answers, config validation and round-trip, rule ids, templates, frame codec, address checks, SNI parsing, certificate minting, sizes |
| End to end | `make test-e2e` | KVM, network access, user namespaces, the `ubuntu-26.04` image, gcc | Below |
| Office | `make test-e2e-office` (also run by `make test-e2e`) | The same, and the `office-fake` image (`make image IMAGE=office-fake`); for the cluster checks, the `k3s` image and the `k8s` kernel | [Office: testing](office.md#testing) |

`tests/e2e.sh` starts its own daemon with a temporary config, socket and state directory, separate from any running daemon. It removes the sessions it starts, and deletes its directory when all checks pass, or keeps it when `ZAPPER_E2E_KEEP=1` is set. It ends with `all e2e checks passed`, or with the number of failures and the path of the daemon log.

## What the end-to-end tier checks

| Group | Checks |
|---|---|
| NIC session | Runs as uid 1000 with `eth0` and without proxy variables. The workdir is copied in, and `fs: hidden` hides `.env`. A `proxy` secret appears as its placeholder and a `guest` secret as its value. DNS answers an allowed name with a synthetic address, gives NXDOMAIN for an allowed name that doesn't exist, and refuses names no rule allows. An allowed TLS connection succeeds, and an allowed request gets its secret. An HTTP/2 request negotiates h2 through the inspecting proxy and still gets its secret injected (the path gRPC rides). POST to a GET-only host, a port without a rule, a raw IP, and a TLS name that differs from the dialled name are denied. An exec rule denies `perl`, and a denied first command prints the reason and exits 126. The exit code reaches the CLI. |
| Root in the guest | With another DNS server configured, names are still answered by policy. A connection to a real address is denied, and other ports on the gateway are closed. |
| VMM confinement | A running VMM has its own root. The config file, the audit log and the daemon user's home directory (apart from zapper's own files) are absent from its view, and its root is read-only. Teardown keeps `vm.json` and removes `vm/`. |
| vsock session | Loopback only, with proxy variables. An allowed connection succeeds through the proxy, and a direct connection fails. |
| Approvals | A request waits for approval, goes through once approved, and `--remember session` adds a rule. Answering for a profile is rejected, and answers leave the config unchanged. The UI answers an approval only with the token cookie, `X-Zapper-UI` and a local `Origin`, has no other writes, and replays a terminal's output only to the token's holder. |
| Request log | Inspected requests and passed-through connections are listed; headers are recorded as the guest sent them; an added secret echoed back by the destination is redacted. A removed session's audit log stays readable, and a malformed session id is refused. |
| Config | A rule change shows in the diff, a global final rule wins, an invalid change is rejected, and reload discards changes. |
| Attacks (`tests/attack.c` as the workload) | Impersonating the agent on the control port, opening a shell over vsock loopback or the guest's own CID, and exec through the 32-bit or x32 tables all fail; the last two are killed with SIGSYS. A denied exec fails. |

A new control needs a check here, including one that attacks it from inside the guest.

## Invariants and their tests

The invariants in [AGENTS.md](../AGENTS.md#working-on-the-repository) and where each is checked. "Not yet" is what no test covers so far: it holds by the code's design, and a test for it is owed.

| Invariant | Checked by | Not yet |
|---|---|---|
| The VMM runs in its own namespaces and root | e2e: VMM confinement | |
| Guest traffic goes through zapper, also with root in the guest | e2e: root in the guest, vsock session | |
| DNS queries and host names are validated; DNS resolves only what could be allowed and never makes up an answer; an A query gets a synthetic `198.18.0.0/15` address and an AAAA query a `2001:2::/48` one, each captured and checked the same way | unit: DNS answers (A and AAAA records, the v4 and v6 synthetic pools), `nic` (A and AAAA for one name are of their family); fuzz: `dns_query` (A and AAAA); e2e: NIC session (NXDOMAIN, refused names, and the IPv6 datapath when the guest kernel has IPv6) | A live IPv6 guest round-trip runs in e2e only on a v6-capable kernel |
| `proxy` secret values stay on the host and are added only where allowed | e2e: NIC session (the placeholder in the guest, the value in an allowed request), request log (an echoed secret is redacted); unit: refused for a host outside the secret's `hosts` and over plain HTTP without `plaintext: true`, placeholders and `set_headers` alike; a `guest` secret only in the profiles that use it; an echoed value masked in the response's headers and body, also split across frames; an `aws` secret signs again only a request signed with its fake key that a rule listing it allows, for its hosts, with signatures matching botocore's and AWS's documented examples (headers, presigned URLs, chunked uploads) | |
| An inspected request is judged on the path and method the upstream will act on | unit: dot segments and encoded spellings can't step past a path `deny`, the judged path is the one forwarded, ambiguous paths are refused; method override headers and parameters naming another method are refused | |
| The proxy connects to the address it checked; loopback, link-local and host addresses are unreachable | unit: address checks; e2e: NIC session (raw IP), root in the guest (a real address) | |
| Passthrough is bound to the TLS server name; inspection presents a per-session CA and overrides `Host` | unit: SNI parsing, an unreadable ClientHello refused (one split across records too), certificate minting, the `Host` override; fuzz: `tls_hello`; e2e: NIC session (a TLS name that differs is denied) | |
| Inspection serves HTTP/2 or HTTP/1.1 by ALPN, pins the authority to the checked host, and injects the same over either | unit: the ALPN offer (h2 then http/1.1), the upstream URI shaping (origin-form for h1, absolute for h2, the authority never taken from the path), and an h2 request served, decided and injected in-process; fuzz: `http2_uri`; e2e: NIC session (HTTP/2 negotiated and the secret injected over it) | A real upstream HTTP/2 server round-trip is only in e2e, not in-process |
| One control connection; terminals only from CID 2 | e2e: attacks | |
| Foreign-arch and x32 syscalls are killed | e2e: attacks | |
| Input from the guest is bounded and rendered as text | unit: control lines, frames, connections through the proxy, approvals waiting, MCP connections; the pages' scripts parse no string as HTML and run none as code | |
| The UI's writes are only approvals and the board's firm actions, each behind the token cookie, `X-Zapper-UI` and an `Origin` naming the UI; on loopback, a local `Host` | e2e: approvals (the cookie, the header and the `Origin`; another `Host` refused; no config save); unit: the `Host` and `Origin` checks on loopback and listening on the network, promoting a decision into the running config (not the startup file); CLI: `zapper serve` with the UI on the network, over TCP | |
| In a firm: the sender is the VM; roles and the chart are checked on the host; an `ask` is refused at once and decided by officers or the board; service names are routed by the daemon | office e2e: identity, authority, officer-only tools, access requests, the firm's services; unit: an officer's approval doesn't get past a `final` rule | |
| In a firm: security is always held; nobody decides their own hire's check or their own access request; no turn before clearance; a hired officer decides nothing before clearance; the charter's articles are the same for every run | office e2e: background checks (the CEO's own hire goes to the board, the first officer is cleared by the board, hires are cleared before working); unit: nobody deciding their own network or credential request, a hired officer deciding nothing before clearance, the charter's articles whatever the run's terms and the board's own | |
| A config change is validated as a whole and applied atomically, or not at all | unit: config validation; e2e: config (an invalid change is rejected, reload discards) | |
| With a keeper, the record is append-only through it | unit: the keeper's store (hash chains, checkpoints, recovery after a crash), no firm turns while the record can't be written; `zapper-keeper serve`: appends from the configured uid only, a line in its file before it is acknowledged | That the line is on the disk, not only in the file, when acknowledged: that takes a power cut |

Controls in the [security model](security.md#what-is-enforced-where) without a test yet: VMs not reaching each other, facility ports accepting only the host's CID, the lifetime, CPU, memory and disk limits, the console cap, and `no_new_privs`.

## Fuzzing

The parsers that read what a guest sends (DNS queries, the TLS ClientHello, CONNECT and Host authorities, the control channel's lines, terminal frames, MCP lines), and the code that decides and changes what goes upstream (rule matching, the headers a request leaves with and the secrets added to them, the upstream request URI whose authority stays the checked host, the request log's redaction and decompression), have fuzz targets in [fuzz/](../fuzz/README.md), run by hand with nightly and cargo-fuzz. A crash found there becomes a unit test next to the parser.

## Manual scenarios

Before a release, walk through these:

| Scenario | Expected |
|---|---|
| `zapper run`, resize the terminal, run `vim`, press Ctrl-C during a command | Full-screen programs redraw, and Ctrl-C stops the command in the guest |
| A raw TCP connection to `github.com:22` from a session that allows it | The SSH banner arrives |
| Kill the daemon with SIGKILL during a session | Its `firecracker` processes exit, and the next `zapper serve` removes the leftover disks |
| SIGTERM to the daemon | Each session stops with `daemon shutdown` |
| A profile with `ttl_secs: 60` | The session stops after a minute with `ttl expired` |
| Open the UI URL, then the same URL with another `Host` header | The UI loads, and the other host gets 403 |
| A two-worker firm on real Claude Code (a small model, a CEO and an engineer founder reporting to them, `budget: { daily_turns: 30 }`, no `seed` so nobody hires), and an assignment to the CEO: "have your engineer write a haiku to /work/haiku.md, publish it and report back" | Within a minute: the CEO delegates by mail, the engineer publishes and reports with `close`, the CEO reports to the board; `zapper drive get` returns the file; `zapper events -s` on a worker's session shows `api.anthropic.com` with `secrets: [claude]` |
| `zapper firm up acme` with the example firm, then a kickoff from the UI | The org chart animates; workers' avatars follow their states; envelopes fly; the board pack arrives in the board's inbox |

## References

- Sources: `tests/e2e.sh`, `tests/e2e-office.sh`, `tests/attack.c`, and the `#[cfg(test)]` modules in each crate
