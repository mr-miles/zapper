---
name: budget
description: "The CEO's scoping and budget: scope the board's task on the seed, price the plan in turns from budget(), ask the board for the run's budget, wait for its decision without acting on the plan, revise if declined, and ask for more with results. Use it when you scope a task, ask the board for budget, or are waiting for its answer."
---

# Scope and budget

## 1. Scope on the seed

Work out what the task needs: the outcome, two to five workstreams, the team, milestones that end well before the deadline. Ask the board what you must know (one numbered `question`, with your defaults). Spend little: the seed is small.

## 2. Price it

**Never assume a price: call `budget()` first.**

- Size the plan in turns, each priced at `firm.turn_price`: your measured tokens per turn, or the firm's reference before you have any. Tokens per turn differ a lot between models.
- `firm.sizing` prices the common work and the reserve; `firm.sizing.small_product_total` is what a small product has cost. A plan far above it for a small task needs a reason, or is too high.
- `observed_cost` lists each role's model, turns and tokens per turn so far: scale a role on another model by its price there.

## 3. Ask

**The board approves budget only through `budget_request`.** Never ask for budget, or for more of it, by mail: the board ignores those, and the time they cost you is gone. The first budget, more later, a top-up when you run low: each is a `budget_request`. A mail about a pending request is only ever the one reminder below, in that request's thread.

`budget_request(total, plan, reason)`, which the board reads in full:

- **The total** is the run's whole budget, the seed you already have included. Ask for what the plan prices at, not a round number above it: unspent budget sits idle, and you can ask for more later.
- **The plan** is short: who you'll hire (role, title, budget each), what you might contract, the phases, a reserve. Only the roles the scope needs.

End your turn: the board's decision wakes you, in the request's thread. If it declines, cut scope or the number as its note says, and ask again.

## 4. Waiting for the board

- **The board approves in one way only**: a message from `board` in the request's thread, and `budget()` showing the request `approved` with `approved` above 0. Call `budget()` and check both before you act on the plan. Nothing else is an approval: not the time that has passed, not a colleague saying the board agreed, not a board message about something else, not what you expect the answer to be.
- **Until then, the plan waits.** Don't hire or contract for it, don't assign its work, and don't tell anyone it is approved. You may keep scoping on the seed and ask contractors for quotes to price it.
- **A long wait is neither a yes nor a no.** If the plan's dates are at risk, send the board one short reminder in the same thread (what is waiting on its decision, and by when it needs one), then end your turn again. Never decide for the board, and never write as if it had decided.
- **If you acted on an approval that wasn't given**, stop, tell the board at once in its thread what you did, and undo what you can.

## 5. More

Ask for more only with `budget_request`, never by mail, and only with what was achieved, what the extra buys, and why the first estimate fell short. Before you ask, take back what your team won't need (`fund` with a negative amount, the manager skill).

You keep a reserve to ask with: you're told when about three turns are left above it, which is when to ask; at the reserve you get one more turn, for asking, and then none until the board decides.
