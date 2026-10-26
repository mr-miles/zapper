# The plan and issue model for the Office

Small additions to the Office's ticket model and a `plan` tool, so a manager, CEO or product manager can lay out a multi-step plan — decomposition, ordering, staged execution, assignment, and optional per-ticket governance — instead of drip-sending assignments by mail. The design is self-contained, but its **stored shape is deliberately modelled on Paperclip** (so a firm could later run Paperclip's orchestrator over zapper's sandbox, [proposal 02](02-paperclip-integration.md), with the plans intact) and **covers what Legion needs** in these areas (so Legion's planner/reviewer skills and roles can be reused, [proposal 05](05-legion-ascended-mapping.md)). Paperclip references are to its source at commit `df5a78c`; the governance tie-in (risk tiers, workflow profiles) lives in proposal 05 and is optional here.

## Current mechanism

Today the Office has no plan or dependency concept. Work moves by **mail**: a worker sends an `assignment` (`mail_send`), which creates a thread and an obligation on the recipient ([`crates/zapper-office/src/mail.rs`](../../crates/zapper-office/src/mail.rs), [`ops.rs`](../../crates/zapper-office/src/ops.rs)). A "task" is emergent — a root thread plus the threads delegated from it, linked by each thread's `context` (`task_threads` in `mail.rs`), which the drive and the objective-close use to roll a task up. The desk lists what a worker owes and waits for; review is by the `reviewing-work` skill; the office refuses to close a task while its sub-assignments are open ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs)).

So decomposition, ordering between pieces of work, staged execution, and "do these in this order, then those in parallel" cannot be expressed: an agent can only delegate one assignment at a time, and nothing sequences them but the agents' own mail. This proposal adds the missing structure while keeping ticket *content* prose.

## The objects

Every object is optional and additive — a firm that uses none behaves exactly as today. Each is shaped on a Paperclip table and covers a Legion concept:

| Office object | Shape it on (Paperclip) | Covers (Legion) |
|---|---|---|
| **Ticket** (an assignment/thread) | `issue`: `parent_id` (= `context`), `assignee_agent_id` (the bound worker), `status`, `priority`, `review_policy` | a task |
| **Ordering** | `issue_relations`, `type: "blocks"` — a stage-list is authoring sugar that lowers to blocks-edges | the taskgraph's edges |
| **Plan** | `issue_plan_decompositions`: a source ticket + `requested_children` specs → `child_issue_ids`, with a status | a phase / change bundle |
| **Hold** | `issue_tree_holds`: a hold on the plan's subtree with a `release_policy` | gated progression |
| **Slot** *(Office-only authoring)* | — (resolves to a concrete `assignee_agent_id`) | an executor assignment |
| **Completion contract** *(optional)* | `completion_contracts`: `risk`, `acceptance`, `completion_authority`, `incomplete_criteria_policy`, hash-pinned | oracle + risk tier + gate + attestation |
| **Assessment** | `work_assessments`: the verdict against the contract, with an input digest | review / evidence |

Two notes. **Hierarchy already exists** as `context` (a single-parent tree that already drives roll-up); the only change worth considering is letting it be *declared*, not only auto-derived, so a plan can be laid out up front. **Ordering is stored as `blocks` edges even though the planner writes stages** — the stage-list is the readable, parent-owned authoring surface; stored, it is Paperclip-shaped edges, so a cross-subtree dependency later is just another `blocks` row.

## The `plan` tool

An agent lays out the whole plan in one atomic call, because a ticket must be able to exist *before* it is delivered — later stages are created held, not pushed to workers yet.

**Input.** A `plan` names:

- a **team of slots** — each `{role | name, budget?, release_when_done?}`; tickets reference a slot rather than a worker;
- the source ticket (the current task by default);
- **ordered stages**, each a list of child specs with the fields `mail_send` already takes — `slot` (who), `subject`, `body`, `due`, `cc`, `attachments` (drive references like `drive:a-3@v2`, up to 16, validated readable) — and, lowered from the stage order, each child's `blocked_by` set.

**What it does, atomically** (validate the whole plan or reject none): create every child under the source (`context`/parent), wire the `blocks` edges from the stages, record the decomposition (source, requested children, child ids), place a **tree-hold** on the subtree whose release policy frees each stage as its blockers are accepted, and bind or defer each slot. It activates stage 0 (delivers and wakes those workers) and holds the rest. `mail_send(assignment)` stays as the degenerate one-ticket, one-stage plan.

**Optional governance.** When the workflow profile turns it on ([proposal 05](05-legion-ascended-mapping.md)), a child spec may carry a **completion contract** (`{risk, acceptance, completion_authority, incomplete_criteria_policy}`, hash-pinned); otherwise acceptance is the ordinary `reviewing-work` close. Off by default.

## Slots, assignment and staffing

Tickets name a **slot**, not a worker, so a plan is a schedule of work, not a fixed roster (the office boots with only its CEO and hires or contracts during the run; a worker is a 4–6 GiB VM).

- A slot is a role to staff (`dev#1: {role: dev}`), optionally pinned to a name (`dev#2: {name: bertie}`), with a budget and a `release_when_done` flag.
- The office binds a slot to one worker the first stage that uses it — an existing worker for a name, a hire for a role, or a **contract** for bounded late work — reuses that worker for the slot's later tickets, and releases it when done.
- **Same slot = same worker** (continuity across stages); **distinct slots = distinct workers** (separation of duties is structural — `Build A (dev#1) / Test A (dev#2)`, `Build B (dev#2) / Test B (dev#1)` tests each feature by the other's developer, no exclusion constraint needed).
- `release_when_done` set frees the worker's VM whenever the slot has no open ticket and re-staffs it when next needed (reclaims RAM, at the cost of a fresh worker and another background check); unset holds it through gaps, keeping its context. Contiguous slots are unaffected.
- A slot resolves to a concrete `assignee_agent_id` when it binds — Paperclip assigns concrete agents and allows a null assignee at creation, so **deferred assignment** (bind on release) maps exactly; on a swap the slot simply dissolves into assignments.
- Provisioning is gated: hiring/contracting needs budget and a security officer's background check before the new worker's first turn. A slot that carries a budget provisions within the board-approved envelope without waking the planner (over the envelope asks the board; a new worker still wakes a security officer, not the planner).

Separation of duties beyond the slot scheme — "differ from whoever did ticket X" across plans — is a later `distinct_from: [ticket-ids]` exclusion, out of scope here.

## Execution — the office as executor

Once a plan is commenced, it needs no resident orchestrator: the office is its executor, and this is the Office's existing wake-on-event loop applied to the plan's events, not a new scheduler. (Paperclip works the same way: its `work-scheduler.ts` is only an in-process deadline timer; readiness and execution are event-driven — a status commit enqueues durable wakes for the newly-unblocked dependent, the parent on children-completed, a reviewer on submission, the unblock owner on a block.)

- A **ready** ticket (its blockers accepted) → wake its assignee to do it.
- A **submitted** ticket → wake its reviewer (distinct slot) to accept or reject — the gate.
- An **accepted stage** → release and wake the next stage's workers (staged release is an *event effect of the review-close*, not a poll).
- A **block** a worker hits → it raises it up its assignment thread; the manager's reply resumes it (see *Indicating a block* below — no new state needed).
- A **finished subtree** → wake the parent/planner for the wrap-up.

The office orchestrates and gates (release-on-accept, review required, authority and assignability checked); the woken agent makes the call. So agents — the planner included — are woken only when a turn or a decision is genuinely theirs; the planner drops out except for what it owns (a provision, a budget top-up, its own review, the wrap-up). Decisions the office cannot infer — an emergent scope change, a newly discovered dependency — are raised by the agent through the existing channels (mail, a new `blocks` edge, an access request) and routed by the office to the right decider.

**Indicating a block.** This needs no new blocked *state* or `block` action. A worker that cannot proceed — a scope question, a missing input only its manager can supply — simply replies up its assignment thread to the parent owner (the manager who assigned it), the existing *being stuck* path. The post office wakes the parent owner with that mail, and the manager's reply wakes the worker again — the ordinary "the reply wakes you" model — so the worker is parked while waiting and auto-resumed on the answer, with nothing added. The two blocks that already have structured channels keep them: **access** goes through `request_access` (refused meanwhile, decided by the security officer, wakes the worker on decision), and a dependency on a **sibling ticket** is a `blocks` edge, not a message. The one rough edge of staying informal is that the parent assignment still reads as *owed*, so its overdue follow-ups can nag a legitimately-blocked worker and its manager; if that proves noisy in real runs the office can later infer blocked-ness — the assignee is waiting on a question it raised within the task — and pause that assignment's follow-up clock, a follow-up tweak rather than a new concept. If Paperclip runs the plans, this informal block maps onto its typed `blocked` status + `unblockDescriptor` (owner = the parent owner), which buys the status accuracy for free; the Office need not adopt that shape itself.

## Consistency with Paperclip and Legion

The point of the shapes above is reuse in both directions. Because the stored objects are Paperclip's (tickets with `parent_id`/`assignee_agent_id`/`status`, `blocks` relations, a plan decomposition, a tree-hold, optional completion contracts, assessments), **replacing the Office's plan engine with Paperclip is a field remap, not a redesign**, and the plans a firm has authored survive the swap. Because the same objects cover Legion's taskgraph (tree + edges), its oracle/risk/gate/attestation (the completion contract) and its review/evidence (the assessment), **Legion's planner and reviewer skills and roles can be run against the Office** — they find the artifacts they expect. Slots and `release_when_done` are the only Office-specific additions; they are authoring conveniences that resolve to a concrete assignee and the worker lifecycle, so they dissolve cleanly on a swap.

## Engine changes

What is new, and what is reused:

- **New:** store `blocks` relations between tickets; a decomposition record and the `plan` tool; a subtree tree-hold with a release policy; **event-driven staged release** (on an accept, wake the next frontier); slot binding (bind at first use, reuse, release per `release_when_done`, distinct slots to distinct workers); optional completion-contract and assessment records; atomic plan validation (recipients assignable, each child in one stage, acyclic, within limits).
- **Reused as-is:** creating assignments and obligations, the `context` tree and roll-up, waking a worker on an event (the foreman), the `reviewing-work` accept as the "accepted" signal, grants and the background-check gate for provisioning, the drive for attachments and the record for evidence.

Most of the weight is the deferred/held ticket state and the release-on-accept effect; both are small extensions of mechanisms the office already has (obligations and event wakes).

## Scope — smallest first

1. **Minimal runnable cut:** `plan` creates children + `blocks` edges + a tree-hold, releasing each stage to **existing** workers on acceptance. Stage-list authoring; no slots, contracts, or provisioning.
2. **Slots and just-in-time staffing:** role/name slots, binding on release, `release_when_done`, contracts for late work.
3. **Governance:** optional completion contracts and assessments, under a workflow profile.

## Open questions

- **Declarable `context`.** Make the hierarchy settable up front (and re-parentable), or keep it auto-derived and attach plan children explicitly?
- **Plan acceptance gate.** Treat the plan as an artifact that is *accepted* before children exist (Paperclip/Legion parity), or materialise on the `plan` call directly?
- **Storage of ordering.** Store `blocks` edges (Paperclip-shaped, chosen here) or a `stage` ordinal per child (more assignment-native, but scatters the plan and loses parallelism) — the edges win for swap-fidelity.
- **Reviewer ≠ author.** Confirm whether Paperclip's `review_policy`/`work_assessments` enforce author ≠ reviewer, or only schedule a review; the Office gets separation structurally from distinct slots, but a stored `review_policy` should say which.
- **`requested_children` shape.** Align the child spec with Paperclip's `requested_children` so a decomposition round-trips.
