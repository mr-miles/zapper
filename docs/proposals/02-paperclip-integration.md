# Paperclip companies on zapper

Run [Paperclip](https://github.com/paperclipai/paperclip) "companies" with zapper underneath as the sandbox / network / credential / security substrate, keeping zapper's enforced isolation and its credential-and-security review — but *reusing* Paperclip's own orchestration (foreman, coordination, turn control) and its approval surfaces rather than duplicating them.

> **Grounding.** This is written against the Paperclip source at `github.com/paperclipai/paperclip`, read at commit `1fbbef4` (file paths below are from there). Paperclip is a fast-moving TypeScript monorepo, so treat the specific symbol/paths as "as of that commit" and re-check before building. Points marked *(inferred)* come from its docs, not code.

## The key realisation: zapper is a Paperclip *sandbox provider*, not a new brain

Paperclip already separates *what runs an agent* from *where it runs*. An adapter (`ServerAdapterModule.execute(ctx)`, `packages/adapter-utils/src/types.ts`) is handed an `AdapterExecutionTarget` — `local`, `remote`/ssh, or `remote`/**sandbox** (`packages/adapter-utils/src/execution-target.ts`) — and the built-in adapters (claude, codex, cursor, gemini, …) already know how to **sync the workspace in, run the agent CLI against the target, and sync changes back** (`packages/adapters/claude-local/src/server/execute.ts`, `packages/adapters/AUTHORING.md`). A sandbox target is backed by a **provider plugin** (`packages/plugins/sandbox-providers/*`: cloudflare, daytona, e2b, kubernetes, modal, …) that implements a documented set of `onEnvironment*` lease/exec/sync hooks (`packages/plugins/sdk/src/define-plugin.ts`).

So the integration seam is clear and already exists: **zapper is a Paperclip sandbox provider** (a plugin declaring `kind: "sandbox_provider"`, capability `environment.drivers.register`), and each agent turn runs inside a zapper session. This is your shape (b) — Paperclip owns the agent process and its orchestration; zapper wraps it — and it is the grain of Paperclip's own design, not something bolted on.

Paperclip's sandbox contract makes the case for zapper outright. `packages/plugins/sandbox-providers/SANDBOX-REQUIREMENTS.md`:

> "A sandbox is an untrusted execution environment… Paperclip protects two authorities across the boundary: the authority to write host files and the authority to call the Paperclip API… The provider and the operator set the policy for general internet access. **This repository does not enforce these provider rules today, and Paperclip cannot verify them for an externally supplied sandbox.**"

That last sentence is zapper's whole value proposition: it is a provider that *actually enforces* the boundary — VM isolation, all egress through host-side policy, host-held secret injection, and a tamper-evident record.

### Why not the earlier "zapper drives via its own turn loop" shape

An earlier draft of this proposal leaned the other way — zapper owning the agent lifecycle and driving turns through its own harness, with Paperclip calling a REST API. The source shows that would **duplicate Paperclip's brain**: Paperclip has its own scheduler and turn control (below), its own adapters that drive the agent CLIs, and its own approval surfaces. Running zapper's foreman/harness alongside Paperclip's heartbeat would mean two turn loops fighting over one agent. The provider shape avoids all of that: zapper runs no turn loop and drives no agent; it leases a sandbox, executes what Paperclip hands it, and enforces policy around it.

## What Paperclip already provides (so zapper must not duplicate)

- **Foreman → "heartbeat".** `server/src/services/work-scheduler.ts` (a single-timer deadline scheduler) + `server/src/services/heartbeat/*` + a durable wake-queue (`server/src/modules/wake-queue/`, table `agent_wakeup_requests`). A "run" is one agent turn (`heartbeat_runs`), woken by schedule, issue assignment, dependency completion, chat, or **approval resolution**. zapper adds no turn control.
- **Coordination → org chart + issues, not a mailbox.** `agents.reportsTo` is the reporting line (`packages/db/src/schema/agents.ts`); agents coordinate by creating/assigning/commenting on `issues` and escalating via `cases`; a wake signals the target. Human↔agent chat/email/inbox are separate channels. zapper's old per-firm mail concept maps onto this; don't bring a second messaging system.
- **Approvals → two real-time surfaces** (see next section).
- **Some isolation already.** Even local runs can be filesystem- and network-scoped (`packages/adapter-utils/src/local-process-sandbox.ts`: `access: "ro"|"rw"`, `networkScope: "deny"|"allowlist"`), and the Claude adapter only passes its skip-permissions flag "inside an identified sandbox" (`packages/adapters/claude-local/src/server/permissions.ts`). zapper replaces this soft, in-process scoping with a hard VM boundary — but note the overlap so network allowlists aren't configured in two places (open question below).

## The seam: a zapper sandbox-provider plugin

Map the provider lifecycle hooks (`define-plugin.ts`) onto zapper's `Core`:

| Paperclip hook | zapper |
|---|---|
| `onEnvironmentAcquireLease` / `ResumeLease` | `Core::create_session` (a session = a lease); resume maps to a kept session |
| `onEnvironmentRealizeWorkspace`, `onEnvironmentSyncIn` / `SyncOut` | copy the workspace into / out of the VM (`zapper cp`) |
| `onEnvironmentExecute` | run the agent CLI in the session (the guest harness), returning output |
| `onEnvironmentReleaseLease` / `Stop` / `Destroy` | `Core::stop_session` / `forget_session` |
| `onEnvironmentRunnerIngressEndpoint`, duplex-stream hooks | the session's control/terminal channel |

Paperclip then selects zapper per run through the `AdapterExecutionTarget` (`kind: "remote"`, `transport: "sandbox"`), and its existing adapters do the sync/execute/sync. All agent egress is captured by zapper's proxy regardless of what the agent attempts — which is precisely the enforcement Paperclip's contract wants but does not provide.

## Approvals without duplication — the important part

Paperclip has **two** human-in-the-loop surfaces, both pushed to the operator in real time over one websocket (`server/src/realtime/live-events-ws.ts`) and mirrored to an immutable `activity_log`. zapper should route its access/credential/network decisions through these rather than stand up a second approval UI.

- **Durable board queue** (`server/src/routes/approvals.ts`, table `approvals`). Types include `request_board_approval`, `budget_override_required`, `hire_agent`; the `payload` is an **open record** (`packages/shared/src/validators/approval.ts`), so a credential/network/grant ask fits with no schema change. An agent (or the system) `POST`s an approval; the **board** decides (`assertBoard`); and on decision the server **auto-wakes the requesting agent with the verdict** (`heartbeat.wakeup(..., reason: "approval_approved")`). This is a near-exact match for zapper's ask→decide→grant model, including closing the loop back to the agent.
- **Live in-run "ask" tools** (DESIGN.md "Live tool reviews keep their own approval flow"; `request_human_input`, `request_confirmation`, `request_item_verdicts`, `request_board_approval`). These render as a card above the task composer and are answered synchronously mid-turn — the live/interactive approvals you asked about.

**Recommended division of labour:**

- A zapper **network `ask`** (a guest hit an `ask` rule mid-turn) → surface as a **live** `request_human_input`/`request_confirmation` so the operator answers in the same place they watch the run; zapper still *enforces* the outcome at the proxy. For a decision that should be durable and auditable (a standing grant), file a `request_board_approval` instead — it persists, requires the board, and auto-wakes the agent.
- A zapper **credential / resource grant** (git, cluster, a drive space) → a `request_board_approval` with the grant detail in `payload`; Paperclip owns the human decision and the record of it, zapper owns minting/scoping/expiry/revocation and enforcement.
- zapper's **traffic review** → split sight from judgement. zapper is the only thing that can *see* all egress, so it emits the traffic (a per-session log, and a digest) and each `ask` to Paperclip, and it *enforces* whatever is decided. But the review itself — someone looking at that traffic and deciding — is orchestration, so it belongs to Paperclip: push the report to a Paperclip **security-officer role** (an agent woken with it) or to the board, as an ordinary wake/approval. zapper should **not** run the officer or the review logic; The Office only did because it was the orchestrator, and that reason is gone here. The judgement is Paperclip's; the sight and the enforcement are zapper's.

The guiding rule: **Paperclip owns the human decision and its UI; zapper owns enforcement and the tamper-evident record.** Each decision has exactly one approval surface (Paperclip's) and exactly one enforcement point (zapper's). Budgets stay entirely Paperclip's (`budget_override_required`).

### "Nobody decides their own", and how far Paperclip is trusted

The Office enforced that no one decides their own access request. In this split the two identities have very different trust, which shapes how the rule is kept:

- **The requester is anchored on zapper** — authoritatively and unspoofably, it is the VM the `ask` came from (the "sender is the VM" invariant); zapper, not the agent, sets `requestedByAgentId`.
- **The decider comes from Paperclip.** zapper can check `decider ≠ requester` before it applies a grant, but that is only as good as Paperclip's report: it guards Paperclip *mistakes* (a self-approval slipping through its logic), not a Paperclip that lies. That is acceptable only because **Paperclip is already the access-decision authority here** — trusting its `decider_id` adds no trust beyond trusting it to decide at all.

So the robustness does not come from the check; it comes from where the decision is anchored and from what zapper refuses to delegate:

- **Default — board-decided, which satisfies the rule structurally.** The board is the human operators (`assertBoard`); an agent can never *be* the board, so `decider ≠ requester` holds with no check, however `decider_id` is set. Keep security-critical asks (network, credentials) board-decided and the constraint is free and robust.
- **Delegating to an agent officer** needs the check *and* a trustworthy decision channel: either Paperclip authenticating the officer (trusted as part of trusting Paperclip to decide), or — to not trust the relay — **signed approvals that zapper verifies**, so a forged decider simply fails to validate.
- **The hard floor, regardless of a buggy or compromised Paperclip:** an operator-set, zapper-side **ceiling** of what is grantable at all (which hosts, which credentials, which resources), configured in zapper's own config, not through Paperclip. Paperclip only ever grants *within* that ceiling. With zapper's enforcement (secret values never leave the host, egress always captured, the address policy, the record), a wrong or dishonest decision cannot exceed what the operator already allowed, exfiltrate a secret, reach a forbidden address, or forge the audit trail.

In short: *Paperclip decides which allowed things happen; zapper decides what is possible at all.* The anti-self check is best-effort integrity layered on the board/signature anchor and the operator's ceiling — not the boundary itself.

## What each side keeps

- **Paperclip:** the company/org chart, roles (including a security-officer role, if used), the heartbeat scheduler and turn control, issues/coordination, chat/inbox, budgets, both approval surfaces + their UI, and the **traffic review itself** — reviewing the reports zapper pushes and deciding.
- **zapper:** the VM sandbox (as a provider plugin), all network/DNS/exec policy and egress capture, host-held secret injection, credential-grant minting/scoping/expiry/revocation, **emitting traffic reports + `ask`s for Paperclip to review** (zapper sees and enforces; Paperclip judges), and the keeper-backed record (which Paperclip's `activity_log` is not — it is mutable).

## Open questions / to confirm against the source

- **Provider-plugin API stability.** `adapter-plugin.md` describes external adapters as "phase-1"; confirm the sandbox-provider plugin surface (`onEnvironment*`) is loadable out-of-tree and stable enough to target, or whether a first cut lives in-tree like the bundled providers.
- **Network policy in two places.** Paperclip's `local-process-sandbox` carries a `networkAllowlist`; zapper has the authoritative policy. Decide that zapper's policy is the source of truth and the Paperclip-side allowlist is left open (zapper enforces), to avoid split-brain config.
- **The runner / HTTP bridge + credential injection.** Paperclip syncs a workspace and exposes a "Paperclip API bridge" and `secretsSvc` into the sandbox. Map which credentials Paperclip injects vs. which zapper injects at the proxy, so a secret isn't handled by both (prefer zapper for anything that must stay host-side).
- **Identity mapping.** Paperclip `agentId`/`companyId` ↔ a zapper principal, for durable grants across runs (ties into [proposal 01](01-persistent-outputs.md)).
- **Delegated-approver support / signed approvals.** The default (board-decided) needs nothing from Paperclip and keeps "nobody decides their own" structurally (see the trust note above). A delegated agent *security-officer* that may decide asks but never its own needs either a Paperclip authority model for a non-board approver, or signed approvals zapper can verify — confirm which Paperclip supports before relying on an agent officer. Until then the board is the safe decider, and zapper enforces "an ask is refused until a decision arrives" and the operator-set grant ceiling regardless.
- **Reference implementation.** The Daytona/e2b/kubernetes providers (`packages/plugins/sandbox-providers/*`) are the templates to copy for the zapper provider; the Daytona `onEnvironmentExecute` is the closest shape to "run the CLI in a leased remote sandbox".

## Phased plan

- **Phase 0 — spike the provider.** A minimal zapper sandbox-provider plugin implementing `acquireLease` (create a session), `realizeWorkspace`/`syncIn`, `execute` (run `claude -p` in the VM), `syncOut`, `releaseLease`. Drive one company agent through it with all egress behind the proxy. Validates the seam against the real plugin API.
- **Phase 1 — map companies to sessions.** Per-agent sessions with zapper profiles/rule layers; confirm sync-in/out and the control/terminal ingress hooks.
- **Phase 2 — approvals.** Route zapper network `ask`s to live `request_human_input` and credential grants to `request_board_approval`; verify the auto-wake closes the loop and the operator sees one timeline.
- **Phase 3 — trust + identity.** Push zapper's traffic reports + `ask`s to a Paperclip security-officer role (or the board) for review, with zapper enforcing the verdict; durable grants keyed to a stable principal ([proposal 01](01-persistent-outputs.md)).
