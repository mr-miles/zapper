---
name: budget
description: "The CEO's scoping and budget: scope the board's task on the seed, size the plan in turns from budget(), ask the board for the run's budget, revise if declined, and ask for more only with results. Use it when you scope a task or ask the board for budget."
---

# Scope and budget

1. **Scope on the seed.** Work out what the task needs: the outcome, two to five workstreams, the team, milestones that end well before the deadline. Ask the board what you must know (one numbered `question`, with your defaults). Spend little: the seed is small.
2. **Ask for the budget.** `budget_request(total, plan, reason)`, every word of it mail. The total is the run's whole budget and includes the seed you already have. The plan is short: who you'll hire (role, title, budget each), what you might contract, the phases, a reserve. **Never assume a price: call `budget()` first.** Size the plan in turns, each priced at `firm.turn_price` (your measured tokens per turn, or the firm's reference before you have any; tokens per turn differ a lot between models); `firm.sizing` prices the common work and the reserve, and `firm.sizing.small_product_total` is what a small product has cost. `observed_cost` lists each role's model, turns and tokens per turn so far: scale a role on another model by its price there. A plan far above the small-product total for a small task needs a reason, or is too high. The board pays for what you ask: ask for what the plan prices at, not a round number above it. Unspent budget sits idle, and you can always ask for more with what was achieved; in one run the CEO asked for 2.0M and the firm spent 29% of it. Hire only the roles the scope needs. If you run short, ask for more with what was achieved. End your turn; the board answers in the thread.
3. **If declined**, cut scope or the number as the note says, and ask again.
4. **Ask for more** only with what was achieved and what the extra buys.
