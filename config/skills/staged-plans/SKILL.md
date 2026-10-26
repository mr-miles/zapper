---
name: staged-plans
description: "Laying out a multi-step effort as a staged plan with the plan tool: a team of slots, ordered stages the office releases as each one is reported done, and the authority and budget rules. Use it whenever an outcome is several assignments in a dependency order, or a team you want held across stages, instead of sending each by hand."
---

# Staged plans

When an outcome is several assignments in a dependency order — build, then review, then deploy — lay it out once as a plan instead of sending each by hand and waking yourself for every handoff. `mail_send` is the one-off; `plan` is the multi-step case: the office releases each stage as the one before it is reported done, so you drop out between stages and only come back for what you own (a review, a budget, the wrap-up).

Use a plan for ordered, multi-stage work, or a team you want held across stages. A single assignment is still just `mail_send`.

## Shape

`plan(source, team, stages)`:

- `source` — the task thread the children belong to, so the work rolls up under it. Defaults to the task you are working on; set it when you commence a plan for a task that isn't your current thread, never your own "make a plan" thread.
- `team` — the slots the children address, by id. Each is `{role}` to staff from the chart, or `{name}` of an existing worker, with an optional `budget` and `release_when_done`.
- `stages` — an ordered list of stages; each stage is an array of child assignments `{slot, subject, body, due}` that run in parallel. A child's `slot` picks which team member does it; `body` is the brief, as for any assignment.

## Slots

A slot is a team member, not a worker: it binds to one worker the first time a stage uses it and is reused after (continuity — the same worker carries the thread across stages), and distinct slots bind to distinct workers (separation of duties — a maker and a separate reviewer are different slots).

- `{name: "ada"}` addresses an existing worker.
- `{role: "engineer"}` is staffed from the chart: a free worker of that role, or — with a `budget` — a worker hired for the slot when none is free (funded from your balance, checked by the security officers like any hire). Without a budget, a role slot waits for a free worker of the role.
- `release_when_done` frees a worker the plan hired once the slot has no more work.

## Only plan what you may run

A plan's children are assignments from you, so authority flows down the chart. A plan is validated as a whole when it starts and when it resumes, and refused if it holds a slot you can't run — it is not acted on halfway:

- a named slot must name a worker that exists and is **below you**;
- a role slot must name a role the firm has, and — when it carries a budget to hire one — a role **you may hire** (your instructions list which).

So name your own reports, and put a budget only on a role you hire. If you need someone you can't reach or hire, ask whoever can.

## How it runs, and getting unstuck

- Stage 0 starts at once. A later stage is created only when the stage before it is reported done (a `report` with `close: true` on each child), so a not-yet-reached stage is just plan data.
- Releasing a stage is best-effort: the office sends every child it can and reports the rest in bulk — a role slot with no free worker and no budget, or a named worker who has since left. The plan holds those children rather than failing.
- Fix the gap — give the slot a budget, or free, name or hire a worker — and `plan_resume` to send the held children.
- A worker who leaves frees their slot and reopens their unfinished child for the stage to re-staff; a plan whose planner leaves passes to their manager. Either way the plan's owner is told.

## Controlling a plan

- `plan_pause` — the office stops releasing further stages; work already in flight finishes.
- `plan_resume` — continue a paused plan (releasing the next stage if the current one finished while paused), and retry any children an earlier stage could not staff.
- `plan_close` — end a plan you no longer want finished, once it is paused and nothing is in progress, so it never cuts off a working agent. A completed plan closes itself.

Only the plan's owner, or someone above them on the chart, controls it.
