[← README](../README.md) · [Docs](README.md) · **Proxy separation (design)**

# Proxy separation: guest input parsed outside the process that holds the secrets

Status: proposal, for review. Nothing here is built yet. It records a design discussion: the options considered, why the proposal was chosen, and what it gives up.

## The problem

The daemon is one process. It holds the secrets and the config, decides policy, answers DNS, and parses everything a guest sends: DNS queries, the TLS ClientHello, inspected TLS and the HTTP inside it, the control channel's JSON lines, terminal frames and MCP lines. A bug in any of that parsing, triggered by bytes the guest chose, is a bug in the boundary, in the process that can read every secret, every session's traffic and the record.

The parsing is in memory-safe Rust, mostly in libraries that are themselves fuzzed (hyper, rustls, serde), so what remains is mainly logic bugs and panics. The aim is that such a bug costs as little as possible: the guest's own session at most, never the secrets of others, the config or the record.

## Already done

These narrow the risk without changing the process structure:

- Fuzz targets for every parser that reads guest input, and for the code around hyper that decides and changes what goes upstream ([fuzz/](../fuzz/README.md)). Fuzzing found one bypass, since fixed: a ClientHello the parser couldn't read, such as one split across TLS records, skipped the server-name check.
- Each session's Firecracker runs as a subordinate uid of its own, without capabilities (`daemon.vmm_subids`), so a guest that escapes it can't signal or trace the daemon or another session's VMM ([security](security.md#the-vmm)).

## Options considered

### A. A guest-facing process and a secret-holding egress process

The guest-facing process parses everything from the guest and sends each request, as a structured message (method, host, port, path, headers, then the body as a stream), to an egress process that holds the secrets, checks policy again, adds the secrets, makes the upstream TLS connection and checks its certificate.

- Strongest: no secret value ever enters a process the guest can influence.
- Costliest: requests and responses cross the boundary as a framed protocol, so streaming, HTTP/2, WebSockets and the request log's body capture all have to be carried across it. Weeks of work, and the egress process must trust nothing in the messages, or the split buys nothing.

### B. A session process that holds no secrets and asks for them per request

A process per session parses the guest's input and relays its traffic. It has no network, files or secrets of its own. For each request it asks the daemon, which decides it and answers with the header edits to make (the secret values), and the process applies them. The daemon opens every upstream connection to the address it checked, so bodies never cross a framed protocol.

- Much cheaper than A: only small messages cross ("this request", "these edits").
- Gives up one thing: a compromised session process can write any secret value it is handed back to its guest. See [what a compromise costs](#what-a-compromise-costs).

### C. Who does the upstream TLS

In B, someone must speak TLS to the upstream server and check its certificate. If the session process does, a compromised one can skip the check, and a guest could then expose its own session's secrets to someone tampering with the network between the host and the upstream. So the daemon should. Ways for it to do so and still let the process write the requests:

| Way | How | For | Against |
|---|---|---|---|
| Pass the plain socket after the handshake | | | Doesn't work: the TLS keys and record state live in the daemon's TLS library; the descriptor alone carries ciphertext the process can't continue |
| kTLS, socket passed | The daemon does the handshake, installs the keys in the kernel (`setsockopt(SOL_TLS, TLS_TX/TLS_RX)`), passes the socket; the kernel encrypts and decrypts | The daemon is out of the data path | The process holds the socket: it can read the keys back (harmless) and the daemon can't revoke the connection; needs the kernel's `tls` module; key updates can't be answered |
| rustls relay over a socket pair | The daemon keeps the TLS connection and passes one end of a socket pair; it copies plaintext from the pair into TLS and back | Works on any kernel; no keys leave the daemon; the daemon can cut the connection; an opaque byte copy, so streaming, HTTP/2 and WebSockets just work | Every byte passes through the daemon twice |
| kTLS and `splice` | As the relay, but the daemon installs kTLS on the upstream socket and `splice`s between it and the pair | The kernel moves and encrypts the bytes; the daemon keeps the socket (revocable, keys unreadable by the process) | Needs the `tls` module; control records (session tickets, key updates) stop `splice` and must be read with `recvmsg` |
| BPF socket map | eBPF redirects between the sockets in the kernel | No process involved at all | Needs `CAP_BPF` and `CAP_NET_ADMIN`: a privileged helper, which zapper avoids. Ruled out |

The relay is opaque bytes, unlike option A: the daemon doesn't know where one request ends, so carrying it is a copy, not a protocol.

## Proposal

Option B, with a process per session, the daemon owning every upstream connection, and the daemon doing the upstream TLS: first as a rustls relay, later with kTLS and `splice` where the kernel supports it.

```
daemon: secrets, config, policy, approvals, DNS answers, upstream connections and their TLS, the record
 │
 ├── session a1b2
 │     ├── VMM: Firecracker, as the session's subordinate uid
 │     └── session process: parses the guest's input and upstream's responses; relays
 │           ├── guest side: the proxy port, DNS, control channel, terminals, MCP (vsock and the NIC capture)
 │           ├── upstream side: one end of a socket pair per connection, plaintext
 │           └── one channel to the daemon: requests, edits, connections, log records
 └── session c3d4 …
```

### The session process

- One per session, started with the VM and stopped with it, supervised by the daemon as the VMM is.
- Runs as a uid of its own (the session's second subordinate uid) in its own user, network and mount namespaces, with an empty network namespace, a root holding nothing but its binary, and seccomp denying `socket`, `connect`, `open` of anything new and `execve`. It has no network, no files and no secrets of its own.
- Holds only its session's state: the session's CA key, to decrypt the guest's TLS; its connections; and the secret values of requests in flight.
- Does all the parsing: DNS queries, ClientHellos, the guest's HTTP (decrypted with the session's CA), the control channel, terminal frames, MCP lines, and upstream's HTTP responses.

### A request

1. The guest sends a request. The session process parses it and sends the daemon its description: method, host, port, path. It names no connection.
2. The daemon decides it as today: rules, approvals, `final` rules, and for each secret the rule adds, the secret's `hosts` and `plaintext`. If it is allowed, the daemon picks the upstream connection: one already open in the session's pool for that host and port, or a new one, which it opens to the address it checked, completing TLS itself (server name, certificate). It answers with a reference to the connection (a new one comes as one end of a socket pair, over the channel with `SCM_RIGHTS`) and the header edits: the secret values for this request.
3. The session process applies the edits, writes the request into the connection and relays the response to the guest. It drops the values when the request is done, and reports the outcome: status, sizes, timing.

The daemon knows the session from the channel the message came on, never from anything in it: a compromised process could claim another session.

### Secrets, per use

The session process starts with no secret values. It gets a value only in the answer to a request the daemon allowed, for a connection the daemon opened to one of the secret's `hosts`.

- What a compromised process holds is the values of requests in flight, not everything its rules could add.
- Rule changes, expired approvals, rotated secrets and revoked sessions apply to the next request, as today.
- The daemon records each handout ("secret `claude` added for `api.anthropic.com`").
- It never gets a secret's source: the file, the environment variable or the config.

A value per connection instead (reused for every request on it, dropped when it closes) would save the round trip. That is a local socket, tens of microseconds, small next to an upstream request; start per request, and change only if measurements say so.

### What a compromise costs

A bug in the session process's parsing, exploited by its guest, gets the attacker the process. What that exposes:

| | Today | This proposal | Option A |
|---|---|---|---|
| Other sessions' traffic and secrets | All | None | None |
| The config, secret files, the record | All | None | None |
| Arbitrary network access | The daemon's | None: no `socket`, no `connect` | None |
| This session's secrets | All, and everyone's | Those its rules add, as it asks for them, to its own guest | None |
| The upstream certificate check | Can be bypassed in-process | Done by the daemon | Done by the egress process |

The proposal gives up one invariant: "`proxy` secret values stay on the host" holds unless the session process is compromised, and then the secrets its rules add can reach its guest. Writing such a value onto another of the session's connections adds nothing to that: those connections go to hosts the policy allows, which the guest could reach anyway.

Two things narrow it:

- Short-lived, narrowly scoped values where a service supports them: the daemon exchanges the long-lived secret for a token per session, so a leaked value is worth little.
- `split: true` on a secret that must never leave the daemon: requests that add it are done by the daemon itself, as in option A, for those requests only. Their bodies cross the boundary; everyone else's don't.

### The record

Today the daemon records what it observes itself. With the split, the HTTP-level details (method, path, status, the request log's headers and bodies) come from the session process, which a compromise could falsify. The daemon still records, from its own observation, every decision, every connection it opened (host, port, bytes each way, duration) and every secret it handed out; those stay authoritative. The request log marks what came from the session process, and the UI says so.

Redaction moves with the bodies: the session process redacts the values it was handed before it sends a log record; the daemon checks again against the values it handed out for that request.

## Costs and catches

- A process per session: a few MB, beside the VMM. The daemon starts, watches and stops it, as it does the VMM.
- The relay copies every upstream byte through the daemon: little for agent traffic (API calls, package installs, git); a large download costs CPU, as encrypting it does today. kTLS and `splice` remove the copy.
- kTLS needs the kernel's `tls` module (loading it may need root once, or `/etc/modules`); without it, the relay.
- The daemon picks HTTP/2 or HTTP/1.1 (ALPN) when it opens a connection; the session process speaks whichever it chose.
- Inspection rules (methods, paths) are decided by the daemon on the description the session process sends. A compromised process can lie about a request's method or path, but only on connections to hosts the session may reach, and it gets secrets only for what the description claims and the rules allow: the same as the guest asking for that.

## Steps

Each leaves zapper working.

1. The session process and its channel, carrying only DNS: the process parses queries and the daemon answers them. Supervision, the uid, namespaces and seccomp.
2. The control channel, terminal frames and MCP lines move to the session process.
3. Passthrough connections: the process reads the ClientHello and checks the server name; the daemon opens and relays the upstream connection.
4. Inspected connections: guest TLS terminated in the process; requests described to the daemon, upstream TLS in the daemon (the rustls relay), secret values handed out per request.
5. The request log from the session process, marked as such; the daemon's own connection records beside it.
6. `split: true` secrets, done by the daemon.
7. kTLS and `splice` where the kernel supports it.

## Open questions

- Whether the session process should share the VMM's subordinate uid or have a second one. A second keeps a Firecracker escape from reaching the session process's secret values.
- Whether per-session token exchange is worth building for the common secrets (the Claude token), or `split: true` is enough.
- How the UI shows a request log the daemon didn't observe itself.
