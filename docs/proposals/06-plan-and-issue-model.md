# The plan and issue model for the Office

Small, additive changes to the Office's ticket model and a `plan` tool, so a manager, CEO or product manager can lay out a multi-step plan — decomposition, ordering, staged execution, assignment, optional governance — instead of drip-sending assignments by mail. The stored shape is modelled on Paperclip (source `df5a78c`) so the engine can later be swapped for Paperclip over zapper's sandbox with the plans intact ([proposal 02](02-paperclip-integration.md)), and it covers what Legion needs so its planner/reviewer skills and roles reuse ([proposal 05](05-legion-ascended-mapping.md)). Every object is optional — a firm that uses none behaves exactly as today.

## Current mechanism

Work moves by **mail**: an `assignment` (`mail_send`) creates a thread and an obligation on the recipient; a "task" is emergent — a root thread plus the threads delegated from it by `context` ([`mail.rs`](../../crates/zapper-office/src/mail.rs) `task_threads`), which drives roll-up and the objective-close gate ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs)). The desk lists what a worker owes and waits for; review is the `reviewing-work` skill. There is no way to express ordering, staged execution, or "these in parallel, then those" — an agent delegates one assignment at a time and nothing sequences them but the agents' own mail. This proposal adds that structure while keeping ticket *content* prose.

## The objects

| Office object | Shape it on (Paperclip) | Covers (Legion) |
|---|---|---|
| **Ticket** (an assignment/thread) | `issue`: `parent_id` (= `context`), `assignee_agent_id`, `status`, `priority`, `review_policy` | a task |
| **Ordering** | `issue_relations` `type: "blocks"` — a stage-list lowers to blocks-edges | taskgraph edges |
| **Plan** | `issue_plan_decompositions`: a source ticket + `requested_children` → `child_issue_ids` | a phase / change bundle |
| **Hold** | `issue_tree_holds`: a hold on the subtree with a `release_policy` | gated progression |
| **Slot** *(Office-only)* | — resolves to a concrete `assignee_agent_id` | an executor assignment |
| **Completion contract** *(optional)* | `completion_contracts`: `risk`, `acceptance`, `completion_authority`, `incomplete_criteria_policy`, hashed | oracle + risk + gate + attestation |
| **Assessment** | `work_assessments`: a verdict against the contract, with a digest | review / evidence |

**Hierarchy** already exists as `context` (a single-parent tree driving roll-up); the only possible change is letting it be *declared* up front, not only auto-derived. **Ordering** is stored as `blocks` edges though the planner writes stages — the stage-list is the readable, parent-owned surface, so a cross-subtree dependency later is just another `blocks` row.

## The `plan` tool

A `plan` is authored and materialised in one atomic call (validate the whole plan or reject none), because a ticket must exist *before* it is delivered. It names a **team of slots** (`{role | name, budget?, release_when_done?}`), a **source** ticket (the current task by default), and **ordered stages** of child specs — the fields `mail_send` already takes (`slot`, `subject`, `body`, `due`, `cc`, `attachments`), with each child's `blocked_by` lowered from the stage order.

It creates every child under the source, wires the `blocks` edges, records the decomposition, holds the subtree with a release policy that frees each stage as its blockers are accepted, and binds or defers each slot; stage 0 is activated and the rest held. `mail_send` stays as the degenerate one-ticket plan. When a workflow profile turns governance on ([proposal 05](05-legion-ascended-mapping.md)) a child may carry a **completion contract**; otherwise acceptance is the ordinary `reviewing-work` close.

## Slots and staffing

Tickets name a **slot**, not a worker, so a plan is a schedule of work, not a roster — the office boots with only its CEO and a worker is a 4–6 GiB VM, so late-only staff must not idle from the start.

- A slot is a role to staff or a pinned name, with a budget and a `release_when_done` flag. The office binds it the first stage that uses it — an existing worker for a name, a hire for a role, a **contract** for bounded late work — reuses it, and releases it when done.
- **Same slot = same worker** (continuity); **distinct slots = distinct workers** (separation of duties by construction: `Build A (dev#1)/Test A (dev#2)`, `Build B (dev#2)/Test B (dev#1)`, no exclusion constraint).
- `release_when_done` set frees the VM whenever the slot has no open ticket and re-staffs on next use (reclaims RAM, costs a fresh worker and background check); unset holds it through gaps, keeping context.
- A slot resolves to a concrete `assignee_agent_id` on bind; Paperclip allows a null assignee at creation, so deferred binding maps exactly and slots dissolve into assignments on a swap.
- Provisioning is gated by budget and a security officer's background check; a slot with a budget provisions within the board-approved envelope without waking the planner (over it asks the board; a new worker wakes an officer, not the planner).

Cross-plan separation ("differ from whoever did ticket X") is a later `distinct_from` exclusion, out of scope.

## Execution — the office as executor

Once commenced, a plan needs no resident orchestrator: the office drives it with the foreman's existing wake-on-event loop, not a new scheduler. (Paperclip is the same — `work-scheduler.ts` is only a deadline timer; readiness is event-driven, a status commit enqueuing durable wakes.) Each event wakes exactly the agent whose turn or decision it is:

- **ready** (blockers accepted) → the assignee works it;
- **submitted** → the reviewer (a distinct slot) accepts or rejects — the gate;
- **accepted stage** → release and wake the next stage (an effect of the review-close, not a poll);
- **finished subtree** → the parent/planner wraps up.

The office orchestrates and gates (release-on-accept, review required, authority checked); the woken agent decides. So the planner drops out except for what it owns — a provision, a budget top-up, its own review, the wrap-up. Emergent decisions the office can't infer (a scope change, a new dependency) are raised by the agent and routed to the right decider.

### Indicating a block

No new blocked state or `block` action. A worker that can't proceed replies up its assignment thread to the parent owner — the existing *being stuck* path; the owner's reply wakes it again, so it is parked while waiting and auto-resumed on the answer. **Access** blocks keep `request_access`; a dependency on a **sibling ticket** is a `blocks` edge. The one rough edge: the parent assignment still reads as *owed*, so overdue follow-ups can nag a blocked worker; if noisy, the office can later pause that follow-up clock while the assignee waits on a question it raised — a tweak, not a concept. Under Paperclip this maps onto its typed `blocked` + `unblockDescriptor` for free.

### Desks and accountability

Delegating an assignment puts it on the delegatee's desk as **owed** and the delegator's as **waiting** (the desk is `debtor → owed`, `creditor → waiting`). A plan would otherwise flood desks, so a **held** child is dormant — not a live owed/waiting obligation and not followed-up — until its stage releases, and the planner sees the plan as one item rather than every future child. A **paused** plan stays on the owner's desk under a resume reminder (the existing follow-up/escalation machinery, re-pointed from the held workers to the owner), and its open tickets keep blocking objective-close — so a paused plan cannot be silently forgotten: the owner must resume or close it.

### Pause, resume, close

`plan_pause` / `plan_resume` / `plan_close` are MCP tools for the plan's **owning agent** (and the board via UI/CLI), with the usual ownership authority — the owner or an ancestor, never an arbitrary worker.

- **Pause** places a manual `paused` hold on the subtree: the office stops releasing stages and waking workers, though an in-flight turn finishes (as with `zapper firm pause`). **Resume** removes the hold and re-wakes the ready frontier, picking up where it left off, since all state lives in the tickets, edges and holds (within the run; a daemon restart ends the run, as always).
- **Close** is the one terminal state, reached by **completion** (all tickets done, auto-closed) or by **`plan_close`** — allowed only when the plan is **paused and nothing is in progress**, so it is never closed under a running agent. Closing clears the remaining obligations and holds and releases slots; completed work stays on the drive. A **closed** plan (completed or abandoned) no longer blocks objective-close; an open or paused one does. So pause keeps the owner accountable; close lets them abandon a no-longer-worthwhile plan cleanly instead of grinding it to the end. The Office has no assigner-side cancel today, so this fills a real gap.

## Consistency with Paperclip and Legion

Because the stored objects are Paperclip's, **swapping the engine for Paperclip is a field remap, not a redesign**, and authored plans survive. Because they cover Legion's taskgraph, its oracle/risk/gate/attestation (the completion contract) and its review (the assessment), **Legion's planner and reviewer skills and roles run against the Office**. Slots and `release_when_done` are the only Office-specific additions, and they resolve to a concrete assignee and the worker lifecycle, so they dissolve on a swap.

## Engine changes

- **New:** `blocks` relations; a decomposition record and the `plan` tool; a subtree tree-hold with a release policy; event-driven staged release (wake the next frontier on accept); slot binding (bind, reuse, release per `release_when_done`); optional completion-contract and assessment records; `plan_pause` / `plan_resume` / `plan_close`; the held-ticket dormant state and the resume reminder; atomic plan validation (assignable recipients, one stage per child, acyclic, within limits).
- **Reused:** assignments and obligations, the `context` tree and roll-up, the foreman's event wakes, the `reviewing-work` close as the "accepted" signal, grants and the background-check gate for provisioning, the drive and the record.

Most of the weight is the held-ticket state and the release-on-accept effect — small extensions of obligations and event wakes.

## Scope — smallest first

1. **Minimal:** `plan` creates children + `blocks` edges + a tree-hold, releasing each stage to **existing** workers on accept; no slots, contracts or provisioning.
2. **Slots and just-in-time staffing:** role/name slots, bind-on-release, `release_when_done`, contracts.
3. **Governance:** optional completion contracts and assessments, under a workflow profile.

## Open questions

- **Declarable `context`** — settable and re-parentable up front, or auto-derived with children attached explicitly?
- **Plan acceptance gate** — accept the plan as an artifact before children exist (Paperclip/Legion parity), or materialise on the `plan` call?
- **Ordering storage** — `blocks` edges (chosen, for swap-fidelity) vs a per-child `stage` ordinal (scatters the plan, loses parallelism).
- **Reviewer ≠ author** — does Paperclip's `review_policy` / `work_assessments` enforce it, or only schedule a review? The Office gets separation from distinct slots; `review_policy` should record which.
- **`requested_children` shape** — align the child spec with Paperclip's so a decomposition round-trips.
