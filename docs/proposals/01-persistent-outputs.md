# Persistent outputs

How to make a firm's outputs durable and controllable across runs: push to real GitHub repositories (authenticated by a pre-installed GitHub App), back the shared drive with S3, and persist access control beyond a single run.

## What is ephemeral today

A firm run (`FirmRun`) is in-memory and per run. Everything a firm produces or decides is either run-scoped on the host or rebuilt from zero each run:

- **Git.** Each run creates fresh bare repos at `state_dir/firms/<firm>/runs/<id>/git/<name>.git` (`crates/zapper-office/src/facility.rs:546`), served by an in-process `git http-backend` over per-session-CA TLS at `https://git.<firm>.internal/<repo>.git`. Workers authenticate with per-worker Basic-auth passwords (random, in-memory) and rights come from a live `git:<repo>` grant checked on every request (`facility.rs:210`). The bare repos persist on disk as a read-only artifact, but the serving service, identities, and routing are gone at run end or daemon restart. There is no durable upstream.
- **Shared drive.** Content-addressed blobs (SHA-256 key) under `runs/<id>/drive/<sha256>` (`crates/zapper-office/src/drive.rs:438`), with `Artifact → Version → File` metadata reconstructable from `journal.jsonl`. The blobs persist on local disk after a run, but they live under the daemon state dir on one host — not durable, not shared, not off-box.
- **Access / grants.** The entire grant ledger is per-run in-memory: `FirmRun.access` starts empty every run (`crates/zapper-office/src/run.rs:466`), owners are re-granted, everyone else must re-ask. The `credentials` journal events are an audit record only — **never replayed** to reconstruct grant state (`crates/zapper-office/src/access.rs`). Only the daemon config persists (`Credentials { expiry, spaces }`, `Security { officers, answer_within }`).

So "persistence" has two halves: **(1)** outputs land in durable external stores (GitHub, S3), and **(2)** identity and access survive and stay controllable across runs.

## Real GitHub repos via a pre-installed GitHub App

### Token flow

**One GitHub App per organisation, one scoped token per agent.** The org installs a single App (private key + an installation id for that org). The host signs a JWT with the App key and, **per agent**, mints a distinct short-lived installation access token (`POST /app/installations/{id}/access_tokens`) **scoped down** to just the repos and permission level that agent's grant allows (the `repositories` and `permissions` fields on the token request), ~1h TTL. The token is a **bearer** credential — presented as `Authorization: Bearer <token>` for both the REST/GraphQL API and git-over-HTTPS (git sends it via `http.extraHeader`, below). It is not a username/password pair, so no Basic-auth encoding is involved.

Minting a *separate* token per agent is the key to keeping zapper's per-worker isolation (see the security note below): each session only ever carries its own token, scoped to its own grant.

### A general mechanism: a dynamic (minted) secret source

A GitHub App token is one instance of a general need — a short-lived credential minted on demand from a long-lived one. The same shape covers AWS STS (`AssumeRole`), GCP service-account token exchange, Vault leases, and generic OAuth client-credentials. So the mechanism must be **service-agnostic**, not a `GithubApp` variant.

Today `SecretSource` is only `Env | File | Value` (`crates/zapper-policy/src/config.rs:149`) — all static. Add one dynamic variant:

**`SecretSource::Command { run: Vec<String>, timeout, … }`** — the daemon runs a host command that prints the credential. To make refresh correct rather than guessed, the command returns JSON `{ "value": "…", "expires_at": "<rfc3339>" }` (plain value-only output allowed, with a configured TTL as fallback). The daemon caches the value and re-runs the command shortly before `expires_at`. This mirrors well-trodden patterns — git credential helpers, the kube/AWS `exec` credential plugins — and is auditable.

GitHub App minting then becomes *just a command*: a small helper (e.g. a `zapper mint github-app --installation <id> --repos … --perms …` subcommand, or a user script) that signs the App JWT and calls `POST /app/installations/{id}/access_tokens`. Nothing GitHub-specific enters the policy model; a different service is a different command.

Properties to nail down:

- **Caching/refresh** keyed by the resolved command + args, honouring `expires_at`; `resolve()` returns the cached value without re-running until near expiry (unlike `File`, which re-reads every request, `crates/zapper-core/src/daemon/proxy.rs:830`).
- **Parameterisation** so one source can mint *scoped* tokens per use — e.g. a GitHub token scoped to a worker's granted repos/level, by passing the scope as command arguments. See the per-worker wiring under service-routing changes below.
- **Trust:** the command runs on the host as the daemon user, like every other secret source — trusted host config, never guest-controlled. Enforce the timeout, a clean environment, and keep its stdout out of logs.
- **Quick spike alternative:** before building `Command`, a background task that writes a refreshed token to a `0600` `File` secret works with zero engine changes (the file is re-read per request). Use it to validate end-to-end, then land `Command` as the durable, general answer.

### Injection

Declare a `mode: proxy` secret with `hosts: [api.github.com, github.com, codeload.github.com]`. The proxy only injects on an **inspected** (MITM) connection, which requires the matching net rule to carry `methods`/`path`/`secrets`/`set_headers` (`crates/zapper-policy/src/eval.rs`); a passthrough tunnel never receives secrets.

Because the token is a **bearer** credential, both uses go through the existing header path — **no new injector code**:

- **REST/GraphQL API:** a rule with `set_headers: { Authorization: "Bearer {{secret:ghapp}}" }`.
- **Git smart-HTTP:** configure the guest's git for the host with `http.https://github.com/.extraHeader = "Authorization: Bearer zapper-secret-ghapp"` — the guest only ever holds the **placeholder**. Git sends that header on every request, and the proxy swaps the placeholder for the real token on egress via the existing literal substring replacement (`proxy.rs:837`). The token stays on the host; `mode: guest` is never used.

(Fallback: if a git endpoint rejects `Bearer` and demands Basic `x-access-token:<token>`, that — and only that — would need a base64 render helper, since the token would sit inside a base64 blob the substring matcher can't see. Using Bearer avoids it.)

### Service-routing changes (what to disable/rewire)

For a firm pointed at real GitHub:

- `facility.rs:register` (~122) — drop the `git.<domain>` service and `GitServer` handler; keep the zone (it also covers `cluster.`/`apps.`).
- `facility.rs:start` (~546) — skip bare-repo creation, the `pre-receive` hook, and per-worker git passwords.
- `credentials_for` (~971) — stop writing `.git-credentials`; instead set the guest git `http.<github>.extraHeader` to the **placeholder** `Authorization: Bearer zapper-secret-<per-worker>`, so the proxy injects that worker's real token on egress and no real credential touches the guest.
- `services_for` (~954) and the worker prompt — advertise `https://github.com/<org>/<repo>.git` instead of `https://git.<domain>/<repo>.git`.
- `network_follows_grant` (~404) — the grant-driven net rule must now allow `github.com`/`api.github.com:443` **with** inspection + the per-worker token injection (`set_headers`/`secrets`), and mint that worker's scoped token when the grant is made (mirroring today's per-worker `git_passwords`). Revocation drops the rule and the token.
- `git_request`/`authorize`/`pre-receive` become dead for real-GitHub firms; the read-only history UI (`crates/zapper-office/src/git.rs`) needs a new source (GitHub API, or a host-side mirror clone).

### What this preserves, and the one residual limitation

Because each agent gets its **own** token scoped to its **own** grant, most of the internal server's per-worker guarantees survive:

- **Per-worker scope:** a worker's token is minted for only the repos and permission level (read vs write) its `git:<repo>` grant allows — scoped via the token request's `repositories`/`permissions`. A worker cannot borrow another's reach.
- **Per-worker revocation and expiry:** the token is tied to the grant and the session rule; revoking the grant drops the injecting rule (`network_follows_grant` None branch) and the short TTL caps any residual window. Minted tokens can also be revoked at GitHub.
- **Reachability still gated by the grant:** no grant, no net rule, no token.

The **one residual limitation** is GitHub-side **audit attribution**: every agent acts as the single org App, so GitHub's own logs show the App, not the individual worker. Keep per-worker attribution in zapper's tamper-evident record, which stays authoritative. (Per-*user* GitHub attribution would need a real GitHub user per agent — out of scope for an App-based design.)

Also note: using real `github.com` means that host is resolved normally on the host (subject to `addr` checks), leaving the "firm service names are never resolved on the host" model the internal git enjoyed. Acceptable, but a deliberate change to the network story (`docs/network.md`, `docs/security.md`).

## S3-backed shared drive

### Current storage

Blobs are content-addressed (`sha256` key) with automatic dedup via `blob.exists()`; the logical `Artifact/Version/File` metadata is reconstructable from `journal.jsonl` "published" entries. Blobs bypass the keeper — only the journal and transcripts are part of the tamper-evident record. Transfer into a VM is `tar` streamed over the guest shell channel (**the VMs never talk to the store directly**; the daemon does the I/O). The I/O seams:

| Seam | Location | S3 equivalent |
|---|---|---|
| Publish blob put | `drive.rs:438` | `HeadObject` + `PutObject` (key = sha); idempotent, dedup for free |
| Board-upload blob put | `drive.rs:559` | `PutObject` from the in-memory bytes |
| Fetch blob get | `drive.rs:655` | `GetObject` streamed into the host-side `tar` stdin |
| HTTP serve blob get | `http.rs:333` | `GetObject` streamed to the response body |

### Proposal

Introduce a **`BlobStore` trait** — `exists(sha) -> bool`, `put(sha, bytes/reader) -> size`, `get(sha) -> reader` — and route the four call sites through it. Provide a local-FS impl (today's behaviour) and an S3 impl. The metadata layer (`Artifact`/`Version`/`File`, journal reconstruction) is storage-agnostic and unchanged.

- **Keys.** Prefer **global content-addressed keys** `blobs/<sha256>` for cross-run dedup (today blobs are per-run and not shared). The cost is deletion: a per-run prefix can no longer be `rm -rf`'d; add refcounting or lean on cheap storage + lifecycle policy. Call this out as an explicit GC decision. (The conservative fallback is per-run keys `firms/<firm>/runs/<id>/drive/<sha256>`, which keeps deletion trivial but loses cross-run dedup.)
- **Staging.** Publish can't know the key before hashing, so the tar is extracted and hashed on local disk first (streamed in 64 KiB chunks) and then `put`. An S3 backend still needs local scratch for extraction + hashing; large files want multipart upload. HTTP serve currently reads a whole blob into memory — an S3 stream is an improvement.
- **The record stays local.** Only blob bytes move to S3; `journal.jsonl` (keeper-held, tamper-evident) stays on the host. This preserves the record invariant.
- **Concurrency.** Keep the in-memory metadata `Mutex`; blob puts are idempotent (same sha ⇒ same bytes), so S3 last-writer-wins is safe without locking.
- **S3 credentials are host config, not a guest secret.** The daemon (host code) talks to S3, not the guest, so S3 egress is not governed by the guest proxy. Use host AWS creds (env/file, or an IAM role) — not the `secrets:` proxy model.

### Can S3-native versioning replace the version scheme?

Yes — largely — and it removes the monotonic counters. With bucket **versioning** enabled, S3 keeps every version of a key under its own opaque `VersionId`, resolves "latest" on a plain GET, and exposes history via `ListObjectVersions` (timestamps + `IsLatest`), with strong read-after-write consistency. zapper can then stop computing version numbers and stop scanning the journal (`find_file`) to find the newest:

- **The storage model flips from content-addressed to path-addressed.** Each file lives at a stable key (e.g. `firms/<firm>/drive/<space>/<artifact>/<path>`); a publish is a `PutObject` to that key → a new `VersionId`. Both counters disappear — the artifact id (`a-N`) becomes part of the key, and the per-artifact `version: u32` becomes the `VersionId`. A reference is the key (latest) or `key@VersionId` (pinned). The cost is losing automatic content-addressed dedup — acceptable if storage is cheap; recoverable only by keeping a content-addressed layer underneath, which reintroduces the bookkeeping this is trying to delete.
- **The one real mismatch: grouped multi-file versions.** Today an artifact version is a coherent snapshot of several paths published together (`Version.files: Vec<File>`); S3 versions are **per object**, with no native "these `VersionId`s are version N" grouping. Two S3-native ways to keep grouped snapshots:
  - **Manifest object (recommended):** each publish also writes a versioned `manifest.json` listing the member paths and their `VersionId`s; the manifest's own `VersionId` *is* the artifact version. zapper reads the latest manifest to resolve the set — still no counter, still S3-owned.
  - **Per-artifact tarball:** store one object per artifact and version the tarball (publish/fetch already tar over the shell channel, so it is natural). Dead simple and grouped, but coarse — you can't serve one file over HTTP without unpacking, and each version re-uploads the whole artifact.
- **Metadata can partly move to S3 too.** `author`/`summary`/`created` ride as `x-amz-meta-*` on each version (immutable per version — fine for publish-time facts), within the ~2 KB user-metadata limit; large free-text `changes` stays in the journal.
- **But the audit record stays zapper's.** S3 is not a trustworthy audit root — versioning can be suspended, lifecycle can expire versions, and anyone with bucket creds can mutate it. So the keeper-backed journal should still record *that* a publish happened (who, when, which `key@VersionId`); zapper just stops *assigning* versions. Both "zapper doesn't need to know the latest version" (true for retrieval) and "zapper still records what was published" (kept for audit) hold at once.
- **Cache interaction:** cache by `(key, VersionId)` — immutable, so the capacity-only eviction story above still applies. Never cache a bare "latest" GET without first resolving it to a `VersionId`.
- **No sequential version numbers.** `VersionId`s are opaque, not `v1/v2/v3`. If human-friendly numbering matters (UI, `drive:a-3@v2`-style references), keep a *display-only* ordinal map derived from `ListObjectVersions` order — cosmetic, not load-bearing.

**The decision this forces.** Adopting S3 versioning as the drive's *model* (not just its backend) couples the drive to S3 semantics: the local-FS `BlobStore` backend has no native versioning and would have to emulate `VersionId`s. So choose deliberately:

- **(a) S3 as a dumb blob store, zapper owns versions** — the `BlobStore` trait stays backend-agnostic (local or S3) and zapper keeps its version model. Portable, but the counters/bookkeeping stay.
- **(b) S3-native versioning as the model** — least zapper code; S3 tracks latest and history; but S3-coupled, and any other backend must emulate it.

Recommend **(b) for an S3-committed deployment** — it is the direct answer here and deletes real code — using the **manifest-object** variant to preserve grouped artifact versions. Choose **(a)** only if backend portability is a hard requirement. Note this supersedes the content-addressed "Keys" bullet above: S3-native versioning is inherently path-addressed.

## Bandwidth: is a Squid cache worth it?

**Short answer: no — use a local content-addressed blob cache in the daemon. The deployment is fully internal, so there are no egress charges and no role for a CDN; the cache is the whole answer.**

The concern is many sandboxes fetching the same drive items from S3 repeatedly. But the fetch path is **host-side**: the daemon reads a blob from the store and `tar`s it into the VM over vsock. The VMs never issue S3 requests, so there is no VM→S3 traffic for a forward proxy like Squid to sit in front of. Squid would be in the wrong place.

Even for the host→S3 traffic, Squid is a poor fit: S3 GETs are HTTPS and typically SigV4-signed (varying auth headers / presigned URLs), which makes responses effectively uncacheable without MITM and defeats a shared HTTP cache. You'd be fighting the grain.

What actually helps, in order:

1. **A local blob cache in the `BlobStore` layer** (on local disk, keyed by sha), in front of S3. This eliminates repeat S3 GETs of the same blob across sandboxes and runs, is trivial to implement, and sits exactly where the traffic is (the daemon). Two distinct concerns, often conflated:
   - **Coherence** — never a problem: blobs are content-addressed and immutable, so a sha that's present is always correct and the cache is never stale.
   - **Capacity** — a real problem: without eviction the cache grows without bound. So it needs an **eviction policy** (size and/or age cap, evict least-recently-used), which requires **book-keeping of last-accessed time and size per cached sha**. Don't rely on filesystem `atime` — it is frequently disabled (`noatime` mounts) and unreliable; record access time explicitly in a small sidecar index (or the daemon's existing state), updated on each cache hit, and run eviction against it. This is the one piece of state the cache must maintain itself.
2. **Keep the cache and the store close on the network.** Because the deployment is fully internal — an on-prem S3-compatible store (MinIO, Ceph RGW) or AWS in a private VPC — there are no internet-egress charges; the only costs are latency, throughput, and load on the object store, all of which the local cache cuts by serving repeat reads from local disk. If the store is AWS-in-VPC, an S3 Gateway VPC Endpoint keeps the traffic off any NAT; on-prem it is already LAN-local. No CloudFront or other CDN is involved or needed.

Squid: not worth bothering.

## Access control across runs

### What persistence requires

Today grants are per-run ephemeral (above). To carry access across runs you need:

1. **Stable principals.** A "worker" is a per-run name today. Access that spans runs needs a stable identity — e.g. `firm + role`, or a durable worker id. Pick the identity key deliberately.
2. **A durable grant store.** Persist `Grant` records (principal, resource, level, expiry, granted-by) outside the run, and on run start **load/replay** them instead of starting empty.
3. **Model-invariant reconciliation.** Persisted grants must still be owner-decided, expiring, and revoked on leave; at run start they must re-enforce (`network_follows_grant` / `cluster_access`).
4. **Cross-run revocation.** `revoke` must update the durable store, not just in-memory state.

### Options

- **Minimal — reuse the "promote to config" pattern.** Security review already promotes a standing approval into the running config via `Core::change` + `config save` (`crates/zapper-office/src/security.rs` → `http.rs` promote). Extend the same, audited path to credential grants: the board promotes a standing grant into a persistent per-firm grants list in the config, loaded at run start. Reuses the running/startup split, atomic validation, and keeper audit.
- **Fuller — a grant ledger.** An append-only per-firm grant ledger with a current-state projection, actually replayed at run start (unlike today's audit-only journal), with explicit TTL and revocation events. More work, better audit.

**Recommendation:** start with config-layer persistence (promote-grant) because it reuses an existing, atomic, audited mechanism; treat a full replayed ledger as a later step if per-run ledgers prove limiting.

### Policy tension to decide

Persisting grants means a hire can **retain access across runs** — the opposite of today's clean-slate-each-run story. Ensure revoke-on-leave and expiry still bite, and that background-check clearance is re-checked per policy. This is a deliberate policy decision, not just an implementation detail.
