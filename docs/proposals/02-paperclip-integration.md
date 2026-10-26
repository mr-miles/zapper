# Paperclip.ai companies on zapper

Run paperclip.ai "companies" using zapper purely as the sandbox / network / credential / security substrate, while keeping zapper's credential-request flow, access control, and security review — the parts that are well thought through.

> **Confirm first.** Paperclip.ai launched after the model's January 2026 cutoff, so this is built from third-party write-ups, not first-hand knowledge. Treating Paperclip as "org chart + roles + budgets + approvals + heartbeat scheduling, with agents plugged in via adapters (Claude Code, Codex, Gemini, Cursor)" — verify its real **adapter interface** and API before acting on the seam below.

## The structural match

Paperclip is, functionally, almost exactly what **The Office** already is in zapper: an orchestration brain — org chart, roles, inter-agent coordination/mail, budgets, approval controls, scheduled wakeups. The crucial fact from the zapper side is that **The Office is just one client of `Core`** (`crates/zapper-core/src/interface.rs`). Core is the enforcement substrate; the office is pure orchestration on top of it.

From the research, the division is clean:

- **Core enforces** (behind the VM boundary): VM isolation/boot, all network/DNS/exec policy evaluation, secret injection, the layered rule engine (global → profile → session → role/keeper), approvals, the MCP gateway, services/zones/DNS routing, and the tamper-evident recording.
- **The client orchestrates:** which sessions exist and their labels, the turn loop and prompts (mail → wake), budgets, the org chart, who decides asks (deciders), the extra rule layers, credential grants, and config checks.

So the goal reduces to: **Paperclip replaces the brain; Core stays as the substrate; and the trust-bearing pieces of the office (credential grants + security review) are kept because they are enforcement, not orchestration.**

## The integration seam (Core as a service)

Paperclip runs each agent through an adapter that today spawns a local process (e.g. `claude -p`). The seam is to write a Paperclip adapter that, instead, **creates and drives a zapper session**. Two shapes:

- **(a) zapper owns the agent lifecycle; Paperclip drives via the API.** Map each Paperclip agent to a zapper session (`Core::create_session`, `interface.rs:170`) and reuse zapper's headless turn loop and harness (claude `-p` stream-json parsing, `TurnEnd`/`Outcome`, token accounting in `crates/zapper-core/src/daemon/harness.rs`). Paperclip's heartbeat wake → `POST /sessions/{id}/prompt` (which queues the next turn, `crates/zapper-api/src/lib.rs:494`); agent output streams back as events (`Core::subscribe`). Paperclip keeps the org chart, roles, budgets, and scheduling.
- **(b) Paperclip owns the agent process; zapper wraps the sandbox.** Paperclip's adapter runs the agent via `zapper exec`/attach into a session, or runs the agent inside the VM while Paperclip talks to it. More coupling to Paperclip's process model, less reuse of zapper's harness and recording hooks.

**Recommendation: (a).** Let zapper own the agent lifecycle and turn loop — that is where the enforcement, secret injection, and recording hooks live, and it guarantees *all* agent egress stays behind zapper's proxy, which is the entire point. Paperclip drives via the REST API (prompt / events / approvals).

Concretely under (a):

- A Paperclip **agent/role** → a zapper **profile** (image + command + rule layers).
- A Paperclip **company** → a set of sessions labeled `{company=<x>}` (mirroring `firm=<x>` today) plus a company-scoped role/rule layer.
- Paperclip's **scheduler/heartbeat** → the caller of `create_session` + `prompt`.
- Agent calls to the world (git, drive, external hosts, credentials) go through zapper: agents use zapper's MCP office tools (`request_access`, `drive_*`, …) served over each VM's MCP port, and egress is governed by zapper's proxy regardless of what Paperclip thinks.

## What to reuse vs replace

**Paperclip owns (replaces the office brain):** org chart, role definitions, scheduling/heartbeats, inter-agent coordination, budgets, task assignment, the "company" concept, and the company-running UI.

**Keep on the zapper side:**

- **`Core`** — non-negotiable; it is the substrate.
- **Credential requests + access control** (`crates/zapper-office/src/access.rs`): the ask → owner-decides → grant → enforce model, with expiry and revoke. This is *enforcement* — it gates network rules, cluster RBAC, and git/drive access on the host — so it must stay on the zapper side even though Paperclip owns the org chart. Who the "policy owners" are can be Paperclip roles, but the decision must flow into zapper's grant/enforcement.
- **Security review** (`crates/zapper-office/src/security.rs`): officers registered as the daemon `Decider` for asks, traffic digests, decide → `NetRule` → standing → promote-to-config. Keep this wholesale — it is the neat part and it is enforcement-side.

### The refactor this implies

`access.rs` and `security.rs` are currently woven into the office (`FirmRun`, foreman, org chart). To use them under Paperclip, separate the **trust layer** from the **brain**. Two sub-options:

- **(i) Run zapper-office as a thin trust/resource service.** It stops being the brain (Paperclip drives the agents via Core) but keeps being the trust layer — it already registers deciders, rule keepers, services, and the MCP tools on Core. Least refactor.
- **(ii) Extract `access.rs` + `security.rs` + `facility.rs` into a standalone crate** that the Paperclip adapter consumes directly.

**Recommendation: (i) first.** The seam becomes: *Paperclip decides who runs and when; zapper/Core decides what they may reach, and records it.*

## Open questions / confirmations

- **Paperclip's adapter contract.** Does Paperclip allow an adapter to be a remote/async driver (needed for shape (a)), or must it spawn a local process? This gates the whole design.
- **Approval ownership.** Paperclip has its own "approval controls"; zapper has asks/grants. Decide which system owns which decision to avoid double approval.
- **Identity mapping.** Paperclip agent id ↔ zapper principal — ties directly into durable grants (see [proposal 01, access control](01-persistent-outputs.md#access-control-across-runs)).
- **Coordination/mail.** Bridge Paperclip's coordination into zapper mail, or keep them separate (see [proposal 03](03-interactive-dialog.md)).
- **The record.** Keep zapper's tamper-evident record (keeper) as the source of truth for what agents did — a zapper strength Paperclip does not replicate.

## Phased plan

- **Phase 0 — spike the seam.** A Paperclip adapter that creates one zapper session and drives a single turn via `POST /sessions/{id}/prompt`, egress behind the proxy. Confirms shape (a) is viable against the real Paperclip adapter interface.
- **Phase 1 — company → sessions.** Map companies/roles to labeled sessions + per-company rule layer; all agent egress behind zapper's proxy.
- **Phase 2 — wire the trust layer.** Run zapper-office as the access/approval path (shape (i)): credential grants + security review for Paperclip agents.
- **Phase 3 — durable identity + dialog.** Persist grants/identity across Paperclip runs (proposal 01), and bridge operator↔agent dialogue (proposal 03).
