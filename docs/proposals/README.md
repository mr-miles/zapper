# Proposals

Forward-looking design proposals for zapper and The Office. These are **plans, not current documentation** — the implemented behaviour is described in [docs/](../README.md). Each proposal states the current mechanism (with file references), a proposed design, concrete change points, trade-offs, and a recommendation.

Drafted 2026-10-25 on branch `feat/require-config` (01–04); proposal 05 added 2026-10-26 on branch `docs/firm-separation`.

| Proposal | Question it answers |
|---|---|
| [01 — Persistent outputs](01-persistent-outputs.md) | Make firm outputs durable: real GitHub repos via a pre-installed GitHub App, an S3-backed shared drive (and whether a Squid cache is worth it), and access control that persists and is controllable across runs. |
| [02 — Paperclip.ai on zapper](02-paperclip-integration.md) | Run paperclip.ai "companies" on zapper as the sandbox / network / credential / security substrate, keeping zapper's credential requests, access control, and security review. |
| [03 — Interactive agent dialog](03-interactive-dialog.md) | Give an agent a chat-like way to talk to the operator for exploratory work, instead of clunky async mail. |
| [04 — The verifiable record](04-verifiable-record.md) | Keep the bespoke keeper, or re-base the tamper-evident record on transparency-log standards (C2SP `tlog-tiles`, signed checkpoints, witnesses) and crates like `siglog`; where a Rust library helps and where it doesn't. |
| [05 — Legion Ascended mapping](05-legion-ascended-mapping.md) | Map Legion Ascended's governed `start→ship` workflow (oracles, evidence, risk-tiered gates, approvals, attestation) onto The Office's roles and record, and borrow its risk-tier → gate policy as per-assignment governance. |
| [06 — Plan and issue model](06-plan-and-issue-model.md) | Small additions to the Office's ticket model and a `plan` tool for multi-step plans (decomposition, `blocks` ordering, staged release, slots, optional completion contracts), modelled on Paperclip's schema (so it can be swapped for Paperclip) and covering Legion's needs (so its skills/roles reuse). |
| [07 — Database credential injection](07-db-credential-injection.md) | Extend the proxy's host-held-secret model to databases: an agent uses a database without holding its password — the proxy authenticates to the real DB with a host-held (optionally short-lived) credential and relays, as an auth-terminating relay. Query audit/gating out of scope (the DB does its own; authorisation is DB-native roles scoped to the injected credential). |
| [08 — Real GitHub repositories through a GitHub App](08-github-app-repositories.md) | Port agentctl's GitHub App code to Rust: `zapper github setup` creates and installs the App, the board names the repository at kickoff, permission profiles replace read/write/admin, an approved grant mints a scoped token that the host mounts as a proxy secret, refreshes before it expires and re-mints when the grant changes. |

## Caveat on Paperclip

Paperclip launched after the model's January 2026 knowledge cutoff. Proposal 02 was first sketched from third-party write-ups but is now grounded in Paperclip's own source (`github.com/paperclipai/paperclip`, read at commit `1fbbef4`), with file references throughout. It is a fast-moving monorepo, so re-check the cited symbols and paths before building on them; anything the proposal marks *(inferred)* comes from its docs rather than its code.
