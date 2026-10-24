[← README](../README.md) · [Docs](README.md) · **Network**

# Network

## Overview

A session (an office worker, a firm's cluster, or a plain `zapper run` session) reaches the network in one of two modes, set per profile with `engine.network`. The example firm's profiles use `nic`.

| Mode | The guest has | Which programs get out | Host needs |
|---|---|---|---|
| `nic` (default) | `eth0` with an address, a default route and a DNS server | Any program, over TCP | `ip` and `nft` |
| `vsock` | Loopback, and `HTTP(S)_PROXY` set to a local forwarder | Programs that use the proxy variables | Nothing beyond what every session needs |

Every session needs unprivileged user namespaces on the host, because the VMM runs confined in its own namespaces ([security](security.md#the-vmm)). In vsock mode the VMM's network namespace is empty.

In both modes, each connection is decided by the session's `net` rules on the host ([policy](policy.md)). Allowed connections are made by the host. In a firm, a connection that hits an `ask` rule is refused at once and becomes an access request for the security officers ([office](office.md#deciding-requests)).

## How it works

### NIC mode

The daemon starts the VMM through `zapper __vmm`. This shim:

1. enters a new user namespace and network namespace (and a mount namespace, for [confinement](security.md#the-vmm)), and becomes root inside them without any host privileges;
2. creates `tap0` with `10.0.2.1/30`, plus loopback. These are the namespace's only interfaces, and it has no routes to the host's network;
3. loads nftables rules that redirect every TCP connection arriving on `tap0` to a local listener, and every UDP packet to port 53 to a local DNS listener, and drop the remaining traffic;
4. passes each captured connection to the daemon as `TCP <destination> <port>`, and each query as `DNS`, over `nic.sock` in the session's VMM directory;
5. starts Firecracker, which attaches `tap0` to the guest as `eth0`.

The guest kernel configures `eth0` as `10.0.2.2/30`, with gateway and DNS server `10.0.2.1`. The capture runs in the namespace, outside the VM, so changing routes or DNS settings inside the guest, even as root, still leaves all traffic going through it.

### DNS

The daemon answers the guest's DNS queries. A query comes from the guest, so it's validated first. It must be a standard query (opcode 0, class IN) with exactly one question. Each label must be 1–63 letters, digits, `-` or `_`, not starting or ending with `-`, and the name at most 253 bytes. Compressed names, empty labels and other characters are rejected:

| Query | Answer |
|---|---|
| Malformed (a response, several questions, a compressed name, truncated) | FORMERR |
| An opcode other than a standard query | NOTIMP |
| A class other than IN, or an invalid name | REFUSED |
| More than 50 a second from the session (bursts of 100) | SERVFAIL |

A valid name is then answered by policy. Answers are never made up:

| The policy for the name | Answer |
|---|---|
| Some request to it could be allowed, outright or after an `ask` | The host looks the name up. If it doesn't exist: NXDOMAIN. If it does: an address from `198.18.0.0/15` that stands for the name within the session |
| Nothing could allow it | REFUSED |

"Some request" counts every port, method and path. The rules are taken in evaluation order: final rules from the global layer down, then the layers, then the default. A matching `allow` rule makes the name reachable, even if it only covers some ports or requests. A matching rule that covers all requests (no `ports`, `methods` or `path`) settles the rest, so `deny host: x` stops the search, while `deny host: x, path: /admin/**` doesn't. In short: if a session can reach a name in any way, the name resolves. `ask` decides access, not resolution, so a name under an `ask` rule resolves without a prompt, and the approval comes when the session connects.

Names are looked up on the host's resolver, so a query for a name the rules cover leaves the host. With a narrow rule (`*.wikipedia.org`) that only reaches the domain's own DNS servers. With a broad `allow` or `ask` (`host: "*"`, or an `allow` or `ask` default), any name a session makes up reaches the resolver, and a program can encode data in names (`data.attacker.example`) without connecting anywhere. Keep broad rules out of profiles for untrusted sessions. Lookups are cached for a minute, with at most 8 at once per session. AAAA and other query types for existing names get an empty answer, so clients use IPv4. Each A query, refusal and rejection is a `dns` event with its response code and reason.

### Connections

When the guest connects to one of these synthetic addresses, the daemon knows which name it stands for and decides on that name and the port. This works for any TCP protocol, such as HTTPS, SSH, git or a database. A connection to any other address is decided on the address itself.

### Services

A service is a name and port that the daemon connects itself, never looked up on the host: to an in-process handler (a firm's git server), an address on the host's network (still subject to the [address checks](#address-checks)), or a port of another session's VM over vsock (a firm's cluster, or any session's). A service is for the sessions its labels pick (`zapper run --label team=a`), or for every session. Its name resolves only in those sessions, to a synthetic address like any other, and reaching it still needs a net rule; rules with methods, paths or secrets don't apply to a service, since its traffic isn't inspected. A name with `*.` in front stands for every name under it; an exact name wins over `*.`. A service's name on a port it doesn't serve goes nowhere ("nothing serves NAME:PORT").

A zone is a domain whose names resolve only to services, for the sessions it is for: a name in it that nothing serves doesn't exist (`NXDOMAIN`), and nothing about it is looked up on the host. A firm's domain is one: the office registers the zone and its services (`git.`, `cluster.` and `apps.` under the domain) for the run's sessions ([office](office.md#names-and-who-reaches-what)).

The operator adds services with `zapper service add` (or `POST /v1/services`), for example a test cluster's API in one session for others to use:

```
zapper run -p k3s --keep --label team=a                 # session ID, its API on vsock port 6443
zapper service add k3s.internal:6443 --to-session ID:6443 --for team=a
zapper rule add profile:default '{kind: net, action: allow, host: k3s.internal, ports: [6443]}'
zapper service ls ; zapper service rm 1
```

Adding and removing a service are `service` events.

For an allowed connection, the daemon:

1. resolves the name on the host and checks the address ([address checks](#address-checks));
2. connects to the address it checked;
3. if the client starts with a TLS ClientHello, requires its server name to equal the decided name, and refuses a ClientHello it can't read (malformed, cut short, or split across records), whose name it can't check;
4. relays the bytes.

If a rule for the host has `methods`, `path`, `secrets` or `set_headers`, the connection is inspected instead. For TLS, the daemon completes the handshake with a certificate from the session's CA, which the guest trusts. It then decides each request on its method and path, sets `Host` to the decided name, adds secrets, and forwards the request over a verified TLS connection. Plain HTTP is inspected the same way, without the TLS step.

### vsock mode

The guest has loopback only. `HTTP_PROXY` and `HTTPS_PROXY` point at `127.0.0.1:3128`, which the agent forwards over vsock to the daemon. The daemon serves the HTTP proxy protocol: a `CONNECT host:port` is handled like a captured connection to that name, and an absolute `http://` request is decided and forwarded on its own.

### Address checks

The daemon connects only to addresses that pass these checks, whatever the rules say:

| Address | Reachable |
|---|---|
| Loopback, link-local (including cloud metadata `169.254.169.254`), multicast, broadcast, unspecified, `0/8` | Never |
| The host's own addresses | Never |
| Private and unique-local ranges (`10/8`, `172.16/12`, `192.168/16`, `100.64/10`, `198.18/15`, `fc00::/7`, `fec0::/10`) | If listed in `daemon.private_networks` |
| Public addresses | Yes |

## Usage

Rules name hosts and ports. These are examples for a profile's `policy.net`:

```
- { action: allow, host: github.com, ports: [22] }                     # git over SSH
- { action: allow, host: "*.githubusercontent.com", ports: [443] }     # raw files, passed through
- { action: allow, host: pypi.org, ports: [443], methods: [GET] }      # inspected: GET only
- { action: allow, host: db.corp.example, ports: [5432] }              # needs its private range in private_networks
- { action: ask, host: "*" }                                            # anything else: an approval, or in a firm an access request
```

To see what a session does and why:

```
zapper events -s ID                          # dns and net events, with the deciding rule
zapper eval -s ID net github.com:22          # which rule would decide, before changing anything
```

From inside a guest, a refused name fails to resolve (`getent hosts` prints nothing; curl says "Could not resolve host"), and `zapper events` shows `refused NAME (no rule allows this name)`. A denied connection is closed as soon as it's made. An inspected HTTP request that is denied gets a `403` with the rule's id.

## Configuration

| Key | Where | Meaning |
|---|---|---|
| `engine.network` | profile | `nic` or `vsock` |
| `daemon.private_networks` | daemon | CIDRs of private networks sessions may reach |
| `net` rules | any policy layer | [policy](policy.md) |

## Verification

```
zapper check                  # "unprivileged user and mount namespaces", "tap and nftables in the VMM's network namespace"
make test-e2e                 # the NIC, root and vsock groups in docs/testing.md
```

## Limitations

- TCP and DNS are the only traffic that leaves a NIC session. QUIC clients fall back to TCP. IPv6 isn't routed.
- A program that connects to an address without looking up a name is decided on that address. Its TLS server name then has to equal the address.
- A server-first protocol such as SSH waits up to 300 ms for its first bytes, while the daemon checks whether the client is starting TLS.
- A NIC adds about 0.4 s to session start, for the namespace and its nftables rules.

## References

- Sources: `crates/zapper-core/src/netns.rs`, `crates/zapper-core/src/daemon/{nic,dns,proxy,addr}.rs`, `crates/zapper-agent/src/forward.rs`
