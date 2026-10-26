# The verifiable record: build vs adopt

Whether to keep zapper's bespoke keeper or re-base it on the transparency-log standards and crates that now exist, and where an existing Rust library would (and would not) help.

## What zapper has today

Two layers, both append-only:

- **The journal** — per-run `journal.jsonl` (mail, run-state, drive events), and session audit logs / terminal recordings (`.cast`). Written through `record::Writer` (`crates/zapper-core/src/daemon/record.rs:100`), which is either `Local` (a plain fsync'd file) or `Kept` (forwarded to the keeper).
- **The keeper** — a separate confined user/service (`crates/zapper-keeper`) that takes append-only requests over a unix socket (`{op, stream, data}`), flushes before acknowledging, and offers **no** read-back, rewrite, or delete. Beside each data file it maintains a hash chain `<file>.chain`:
  - **Hash chain:** each line's hash covers the previous hash and the line's bytes (`link`, SHA-256 via `ring`, `crates/zapper-keeper/src/lib.rs:92`), from a per-stream genesis (`lib.rs:82`).
  - **Signed checkpoints:** every N lines / after a duration, a checkpoint over `(stream, seq, hash, ts)` signed with an **Ed25519** key only the keeper can read (`checkpoint_message` `lib.rs:100`; `Ed25519KeyPair` `lib.rs:29`).
  - **Verification:** `record::verify_dir` (`record.rs:168`) re-reads a file and checks it against its chain, reporting lines/signed/checkpoints. Crash recovery rebuilds the chain from the data.

Its security rests on two things: process **isolation** (a separate uid, no read API, the daemon takes no firm turns while the record can't be written) and the **hash-chain + signed-checkpoint** integrity that detects any change made "around" the keeper (by root, or on disk).

## What it is — and isn't — relative to the standard

The keeper is, in effect, a **private, single-signer transparency log without the tree**. Compared with the now-standard transparency-log construction (RFC 6962/9162; the C2SP `tlog-tiles` + signed-note "checkpoint" specs; the transparency.dev / Tessera / Sigstore-Rekor ecosystem), it lacks:

- **A Merkle tree.** Verification is a linear re-hash of the whole file (`verify_dir`), and there are no **inclusion** or **consistency** proofs — you cannot prove "entry X is in the log" or "log state B extends A" cheaply; you re-read everything.
- **Standard formats.** The checkpoint is a bespoke string, not a C2SP signed note, so nothing external can verify it without zapper-specific code.
- **Tiled / object-friendly storage.** The chain is a sidecar file; there is no tiled layout of the kind that makes transparency logs scale and sit naturally on object storage.
- **Independent witnesses.** Trust is the single keeper Ed25519 key. There is no witness co-signing, so a holder of that key (or the keeper itself) is a single point of trust; the standard's witness quorum gives split-trust that the current design does not.

None of these are bugs — they are the scope the keeper was built to. The question is whether the gaps are worth closing.

## The ecosystem and the Rust options

> Grounding caveat: most Rust crates in this space are young and low-adoption, and several that search surfaces read as auto-generated. Verify authorship, downloads, and release history before depending on any of them.

- **[`siglog`](https://github.com/prefix-dev/siglog)** — by prefix-dev (the pixi/rattler team, reputable). A Tessera-compatible transparency log: RFC 6962 Merkle tree, signed checkpoints, witness co-signing, C2SP `tlog-tiles`, with **filesystem and S3** storage backends. The closest fit and the first to evaluate — and its S3 backend dovetails with [proposal 01](01-persistent-outputs.md).
- **[`witness-log`](https://github.com/0xAndersen/witness-log)** — RFC 6962 Merkle, witness quorums, offline-verifiable "dossiers"; named author, looks real; smaller.
- **`rs-merkle`** (from knowledge, verify currency) — just the Merkle-tree primitive with inclusion proofs, if you want to build the tree yourself and keep everything else bespoke.
- Lower-provenance / verify carefully: `auths-transparency`, `metamorphic_log`, `confium_transparency`, `qs_transparency` — young, possibly generated; do not adopt without scrutiny.
- **Plain WAL crates** (for the journal half): `okaywal` (from knowledge), or the young `open-wal` / `walogs` / `wal-db` / `rewal`. See the recommendation below — the journal probably does not need a dependency at all.

## Build vs adopt

**What adopting the transparency-log standard would buy:**

- **Cheap, standard verification** — inclusion/consistency proofs instead of a full-file re-hash; an external auditor (or the operator offline) can verify with standard tooling, not zapper-specific code.
- **Split trust via witnesses** — the record's integrity no longer rests on a single keeper key; independent witnesses co-sign checkpoints. This is the one genuinely new *security* property, not just ergonomics.
- **Object-storage alignment** — a tiled log on S3 fits the same direction as the drive move in proposal 01 (and `siglog` already has an S3 backend).

**What it would cost / risk:**

- **Maturity.** Outside `siglog`, the crates are immature; you would be an early adopter or a fork maintainer. Even `siglog` warrants a hard look at stability before it holds the audit record.
- **It does not replace the keeper's isolation.** The keeper's strongest property — a separate uid with no read/rewrite/delete API, append-only over a socket — is an *operational* control a library does not provide. A transparency-log crate gives you the data structure, not the confinement. Any adoption should keep the isolated append-only service and swap only its internal format.
- **Witnesses need other parties.** The witness model only pays off if the witnesses are independently operated; a single operator co-signing with themselves adds format, not trust.
- **S3 changes the trust base.** Today the record is local behind the keeper's uid. A transparency log on S3 is mutable by anyone with bucket creds — the Merkle proofs + witness checkpoints are what compensate, so you cannot adopt the S3 storage without also adopting the proof/witness machinery.

## Recommendation

- **Journal half:** keep the fsync'd JSONL as-is. It is simple, already keeper-compatible, and a WAL crate buys little; do not add a dependency here.
- **Keeper half:** keep the bespoke keeper as the default. It is sound for the current threat model (integrity detection + isolation), and the alternatives do not yet clearly beat it.
- **Treat the transparency-log standard as a deliberate upgrade, not a refactor** — pursue it only if a concrete goal appears: **offline/third-party verifiability** or **independent witnesses** (split trust). If it is pursued:
  - Evaluate **`siglog`** first (C2SP `tlog-tiles`, signed checkpoints, witnesses, S3 backend).
  - Keep the keeper's **isolated append-only service** and replace only its internal chain format with a Merkle/`tlog-tiles` structure — so you gain proofs, standard checkpoints, and interop while retaining the confinement that the library cannot provide.
  - Sequence it with the [proposal 01](01-persistent-outputs.md) S3 work, since the storage backends and the S3 trust-base question overlap.

## Open questions

- Is **third-party / offline verifiability** actually a requirement, or is internal integrity-detection enough? This single answer decides the whole proposal.
- Are there **independent parties** who could run witnesses? Without them, the witness model is cosmetic.
- Does the record need to **scale** beyond what linear-chain verification handles comfortably? If not, the Merkle tree's main practical win (cheap proofs) matters less.
