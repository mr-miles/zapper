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

## Caveat on paperclip.ai

Paperclip.ai launched after the model's January 2026 knowledge cutoff, so proposal 02 is built from third-party write-ups (some vendor-authored), not first-hand knowledge of its internals. Its adapter interface and API must be confirmed against the real Paperclip docs/repo before that proposal is acted on.
