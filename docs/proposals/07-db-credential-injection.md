# Database credential injection

Extend the proxy's host-held-secret model from HTTP headers to database connections: an agent uses a database without ever holding its password — the proxy authenticates to the real database with a host-held (optionally short-lived) credential and relays the session. Query audit and content gating are deliberately **out of scope**: the database does its own audit, and authorisation is the database's own roles/grants scoped to the injected credential, not SQL-sniffing on the host.

## Today

The proxy captures all TCP; a database connection is a passthrough **tunnel** — byte-forwarded, gated only by a host/port net rule (`crates/zapper-core/src/daemon/proxy.rs`, `tunnel`). The guest holds the database password, and secrets are injected only into *inspected HTTP* (into headers). Database TLS is also not HTTP TLS: Postgres sends a plaintext `SSLRequest` before the handshake, MySQL upgrades mid-handshake, and neither starts with a TLS ClientHello or carries SNI — so the HTTP MITM path does not apply to them.

## Mechanism: an auth-terminating relay

Model it on a connection pooler (pgbouncer-style), not on query inspection:

1. The guest connects to the database host — which the capture points at the proxy — as the configured user, with **no password** (the profile gives it host/user/db and no secret, mirroring the HTTP placeholder).
2. The proxy speaks just enough of the wire protocol to read the startup (requested user/database), **present the client side as authenticated** (it impersonates the server's post-auth state to the guest), and separately open its own connection to the real database and **authenticate with the host-held secret** (Postgres SCRAM/MD5; MySQL auth).
3. Once both sides are up it **splices and relays bytes** — it does not parse the query stream.

So the only protocol work is the startup/auth handshake, never SQL.

### TLS — the simplest safe wiring

Prefer **guest↔proxy plaintext, proxy↔server TLS**. The guest↔proxy leg is captured inside the VM boundary (it never leaves the host) and carries **no secret** (the guest has no password), so it needs neither TLS nor a per-session CA; the proxy↔server leg uses real TLS with the real credential. That sidesteps MITM on the guest side entirely. If the guest insists on TLS to the "database" (`sslmode=require`), the proxy terminates it with the per-session CA (the guest must trust it), and `sslmode=verify-full` against the real server's certificate still breaks it — the same cert-pinning limit as HTTPS. So steer the guest to a plaintext local leg.

## Which databases, and what needs special treatment

The hard part per database is the **TLS negotiation** (several databases do not start with a TLS ClientHello) and the **auth exchange**; once past auth the relay is protocol-agnostic byte-splicing.

| Database | TLS negotiation | Auth | First-cut difficulty |
|---|---|---|---|
| **PostgreSQL** | plaintext `SSLRequest` preamble, then TLS | SCRAM-SHA-256 (md5 legacy) | **Reference — do first** |
| **MySQL / MariaDB** | TLS upgrade *mid-handshake* (server greeting → client sets `CLIENT_SSL` → TLS → auth) | `caching_sha2_password` (8.0 default) / `mysql_native_password`; caching_sha2 over a non-TLS leg needs an RSA public-key exchange | Harder: mid-handshake TLS + the caching_sha2 RSA dance + auth-switch |
| **MS SQL Server (TDS)** | TLS handshake **tunnelled inside TDS PRELOGIN packets** | SQL auth (tractable) or Windows/Kerberos/NTLM (hard) | Hard: must wrap the TLS handshake in TDS framing; restrict to SQL auth |
| **MongoDB** | standard TLS | SASL (SCRAM-SHA-256/1) over `OP_MSG` | Moderate: normal TLS, but a full wire-protocol + SASL framer |
| **Redis / Valkey** | standard TLS (or plaintext) | `AUTH` / `HELLO AUTH` command | Easiest: the "credential" is just an `AUTH` the proxy issues |
| **Cassandra (CQL)** | standard TLS | SASL/PLAIN via `AUTHENTICATE`/`AUTH_RESPONSE` | Moderate |
| **Oracle (TNS/TTC)** | proprietary negotiation | proprietary | Defer — out of scope for a first cut |

The databases needing genuinely *special* treatment are the ones whose TLS is not "ClientHello first": **Postgres** (`SSLRequest`), **MySQL** (mid-handshake upgrade), and **TDS** (TLS-in-TDS). The rest are "standard TLS, then a SASL/`AUTH` exchange" and differ only in framing. Each still needs its own auth implementation, so add them one at a time behind the same relay; Postgres first.

## The secret model

- A new secret kind: a database credential bound to `(host, user, database)`, injected by **completing the server-side auth** rather than by setting a header — a `DbAuth` mode alongside the HTTP `proxy` mode in the secret model (`crates/zapper-policy/src/config.rs`). A net rule for the database `host:port` names the protocol + the secret, which makes the connection inspected (routed to the auth-relay instead of tunnelled).
- It composes with the minted/dynamic secret source from [proposal 01](01-persistent-outputs.md): the credential can be **short-lived** — RDS IAM auth tokens, Vault dynamic database roles — minted host-side, rotated, and never in the guest.
- The existing **`plaintext` flag** (today it gates injecting a `proxy` secret over plain HTTP) carries straight over, now governing the **server leg**: the real credential may be sent to the database over a **non-TLS** connection only if `plaintext: true` (anything on the path can read it — lab/internal only); otherwise TLS to the server is required and the connection **fails closed** if the server won't negotiate it. The guest↔proxy leg carries no secret (the guest has no password), so it is unaffected — which is exactly why the plaintext guest leg is safe. This keeps the HTTP invariant intact: *"the proxy adds a secret only when the connection uses TLS or the secret sets `plaintext: true`."*

## Authorisation stays in the database

No SQL-content gating on the host: a WAF-for-SQL is a weak boundary — any disagreement between the proxy's parser and the database's (comments, dialect, multi-statement, encodings) is a bypass. Instead the injected credential **is a database role scoped by the database's own grants** (which tables, read vs write), and the database enforces it. Give each profile/agent a least-privilege role; the proxy just supplies that role's credential. Audit is the database's own log; zapper records only the **connection** (who connected to which database, as which secret) in its keeper-backed record — not the queries.

## What's new, and the attack surface

- A per-database **startup/auth state machine** (Postgres first — clean SCRAM; MySQL analogous). It is the only new parsing and is limited to the handshake, not the query stream, so the surface is small. It is still guest-influenced parsing in the daemon → give it a **fuzz target** (a DB-handshake target, per [fuzz/](../../fuzz/README.md)) and isolate it per session (see [proxy-separation.md](../proxy-separation.md)).
- A **protocol dimension** on inspection (a rule's protocol / `inject` kind) and a `stream()` dispatch to the relay alongside `tunnel`/`mitm`.

## Scope and recommendation

Target **Postgres credential injection** first: a plaintext guest leg, TLS + real SCRAM auth to the server, least-privilege database roles for authorisation, and the database's own audit. MySQL next. Skip query audit/gating. Go in aware of three caveats: `verify-full`/cert-pinning breaks any guest-side TLS termination (prefer the plaintext leg); the handshake parser is new host-side guest-input parsing (fuzz + isolate); and the credential should be short-lived where the database supports it (compose with proposal 01's minted secrets).

## Open questions

- **Guest user vs secret user:** does the proxy use the secret's user (ignoring the guest's requested user) or require they match? Recommend: the secret defines the user; the guest's requested database is honoured or pinned.
- **Protocol completeness:** `BackendKeyData` / query-cancel and `ParameterStatus` handling in the Postgres relay (synthesise vs relay the server's).
- **Server-connection pooling** per secret (reuse) — a later optimisation, not needed for a first cut.
