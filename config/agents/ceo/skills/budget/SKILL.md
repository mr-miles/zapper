---
name: budget
description: "The CEO's scoping and budget: scope the board's task on the seed, price the plan in turns from budget(), ask the board for the run's budget, revise if declined, and ask for more with results. Use it when you scope a task or ask the board for budget."
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

`budget_request(total, plan, reason)`, which the board reads in full:

- **The total** is the run's whole budget, the seed you already have included. Ask for what the plan prices at, not a round number above it: unspent budget sits idle, and you can ask for more later.
- **The plan** is short: who you'll hire (role, title, budget each), what you might contract, the phases, a reserve. Only the roles the scope needs.

End your turn; the board answers in the thread. If it declines, cut scope or the number as its note says, and ask again.

## 4. More

Ask for more only with what was achieved, what the extra buys, and why the first estimate fell short.
