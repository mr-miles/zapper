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

Keep the whole approval lifecycle in zapper and reduce Paperclip to *waking* the decider. This is already how The Office works: its security-officer sees and decides access requests over the authenticated office MCP (`crates/zapper-office/src/ops.rs`), where the caller's identity *is* the VM the call came from (`crates/zapper-core/src/daemon/gateway.rs`), never a self-reported field. Paperclip simply replaces the foreman that wakes the officer.

**Expose zapper's approvals as an MCP server.** Tools: `approvals.list` (the pending approvals assigned to the calling principal/role) and `approvals.decide(id, allow|deny, reason)`. The gateway authenticates the caller as its VM, so the **decider identity is zapper's own, not reported across the boundary** — exactly as the requester already is.

Flow for an agent security-officer:

1. a guest hits an `ask` (or requests a credential/resource grant) → zapper records a **pending approval** assigned to the officer role, and refuses the guest meanwhile;
2. **Paperclip wakes the officer agent** (its heartbeat) — that is Paperclip's whole part;
3. the officer calls `approvals.list` over MCP, reviews (with the traffic/digest zapper attached), calls `approvals.decide`, and ends its turn;
4. zapper authenticates the `decide` by the calling VM, checks the caller holds the officer role and **is not the requester**, then enforces and records it, and lets the waiting guest retry.

This is the strong form of "nobody decides their own": **both** identities are zapper-authenticated (requester = the VM that asked; decider = the VM that called `decide`), so the check runs on identities zapper owns on both sides — there is no `decider_id` to trust. Paperclip's residual power shrinks to *whether* it wakes an officer (declining only leaves the ask refused — fail-closed) and *which* agent it wakes (zapper rejects a caller that does not hold the officer role, or that is the requester; the officer→principal designation is operator-set — see the floor below). Paperclip cannot forge a decision, because it never supplies one.

**Humans decide through Paperclip; agents through zapper.** A human operator has no VM, so board decisions cannot use the MCP path — and need not:

- **Agent security-officer** → zapper's approvals MCP (VM-authenticated, above).
- **Human board** (the right default for security-critical asks) → Paperclip's board surface: `request_board_approval` (durable; `payload` is an open record — `packages/shared/src/validators/approval.ts`; `assertBoard`; auto-wakes the requester on decision — `server/src/routes/approvals.ts`), or the live `request_human_input`/`request_confirmation` tools that render above the task composer. A human board member is structurally not an agent requester, so "nobody decides their own" holds there too, and the decision is authenticated by Paperclip's own operator auth. This is the one path where a decision is Paperclip-authenticated — acceptable precisely because the human↔agent separation is structural, not a trusted id.

**When there is no security-officer**, the same mechanism applies with Paperclip waking a *different* decider — because "who may decide" is a role/authority question that zapper checks against, not a hard-coded officer. The operator-set ceiling names the decider authority and its fallback order (the Office's ladder is officer → CEO → board). With no officer, Paperclip wakes whichever agent next holds the authority (say the CEO), who decides over the same MCP; zapper still checks that the caller actually holds decider authority for *this* approval and is not the requester. So if the only authorised agent decider *is* the requester (the CEO asking for its own access), no agent can satisfy the check and it falls through to the human board. Paperclip chooses whom to wake; zapper decides whether that caller may decide this one.

So each decision is authenticated **where its decider's identity is authoritative** — agents at zapper's gateway, humans at Paperclip's board — and neither path passes a decider id across the boundary. zapper emits its officer decisions into Paperclip's `activity_log` (via the live-events channel, `server/src/services/live-events.ts`) so the operator still sees one timeline.

**The hard floor, regardless of a buggy or compromised Paperclip:** an operator-set, zapper-side **ceiling** — what is grantable at all and which principals may decide (which hosts, credentials, resources; who the officers are) — configured in zapper's own config, not through Paperclip. Paperclip only ever grants, and only ever wakes a decider, *within* it. With zapper's enforcement (secret values never leave the host, egress always captured, the address policy, the record), a wrong or dishonest decision cannot exceed what the operator already allowed, exfiltrate a secret, reach a forbidden address, or forge the audit trail.

The **traffic review** folds straight in: zapper emits the per-session traffic + a digest as part of the pending approval; Paperclip wakes the officer; the officer reviews and decides over the same MCP. zapper sees and enforces; the judgement is the officer's, authenticated by zapper. Budgets stay entirely Paperclip's (`budget_override_required`).

In short: *Paperclip decides which allowed things happen — by waking a decider, or by asking the board; zapper decides what is possible at all, authenticates the decider, and records it.*

## What each side keeps

- **Paperclip:** the company/org chart and roles (including whoever holds decider authority — officer, CEO, board), the heartbeat scheduler and turn control, issues/coordination, chat/inbox, budgets, the **human board approval surface + its UI**, and **waking the right decider** for a pending zapper approval. It schedules the review; it does not author the agent's decision.
- **zapper:** the VM sandbox (as a provider plugin), all network/DNS/exec policy and egress capture, host-held secret injection, credential-grant minting/scoping/expiry/revocation, the **approvals MCP** — the approval lifecycle, authenticating the decider as the calling VM and checking authority + not-the-requester — the operator-set grant ceiling, and the keeper-backed record (which Paperclip's `activity_log` is not — it is mutable). It sees the traffic, authenticates the decider, enforces, and records; the judgement is the woken agent's (or the board's).

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
- **Phase 2 — approvals via MCP.** Expose zapper's `approvals.list`/`approvals.decide` MCP (decider = the calling VM, checked for authority and not-the-requester); have Paperclip's heartbeat wake the decider on a pending approval, and route decisions that need a human to `request_board_approval`/`request_human_input`. Verify the officer path end to end and the human-board fallback, with one operator timeline.
- **Phase 3 — trust + identity.** The operator-set grant ceiling and decider-authority list in zapper's own config; durable grants keyed to a stable principal ([proposal 01](01-persistent-outputs.md)); optional signed board decisions if Paperclip is to be less trusted.
