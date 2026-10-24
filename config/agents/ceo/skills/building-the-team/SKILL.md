---
name: building-the-team
description: "The CEO's team: which roles a task needs, staff or contractor, hiring, funding, taking budget back and letting go, the security officer, and nobody stalling for budget. Use it whenever you hire, contract, fund or release someone."
---

# Building the team

- **Hire** after approval. If the roles you may hire include a security officer, **start with one** (nothing else is allowed before one; see *Access and security*; give them a small starting budget, their share of the approved budget follows when they take over security). Then hire the roles the task needs: engineers, a **designer** whenever the task has a UI, a product manager when the scope needs one, and a CTO (role `cto`) only when the engineering is too big for you to lead directly. When there is a UI, the designer is what takes the product to market: they own what users see and do, and engineers build to their design; don't skip them to save budget, since a UI without one looks and works like an afterthought and fixing it later costs more turns than the designer does. A designer on staff is expensive, though: the design is mostly done up front, and staff cost turns (and severance) after it is. Prefer to **contract** the design as a bounded job (the design for the brief, plus a few rounds of review while engineers build), and negotiate the quote (contract_counter) against what the job is worth; hire a staff designer only when design work keeps coming. A task with no UI (an API, a data job, a script) needs no designer. A product manager owns the brief and accepts the work against it; for a small product you can write the brief and accept the work yourself. `hire(role, title, budget, subject, brief, reason)`: the brief is their first assignment and all they know, with the goal, constraints, what done looks like, who else is on the team, and the access the work needs (the officer can grant it when clearing them). Fund them for their part; keep the reserve. A hired security officer reports to the board, not to you: you don't assign them work.
- A hire starts once a security officer's background check clears them; an objection cancels the hire and returns the budget.
- **Contract** bounded, specialist or one-off work (manager skill, *Contractors*): research briefs, a market study, a design, an audit, a migration. A job with a clear deliverable and an end is a contract, not a hire: staff keep costing turns after their deliverable is in. In one run a staff researcher cost 50k tokens for two briefs that a contract would have bounded. You can ask contractors for quotes while scoping, to price the budget request.
- **Staff or contractor**: keep the balance between severance and knowledge (manager skill).
- **Fund** with `fund(worker, tokens, reason)`. Take budget back (a negative amount) only from someone whose work is done, never mid-turn, and leave them enough to answer questions: a few of their turns at their price (`budget()`'s `observed_cost`). The office keeps at least two; to take everything, release them.
- **Nobody stalls for budget.** When told someone ran out of tokens, make sure they are funded (by their manager or by you) or their work moves, in that turn. If the firm's budget is short, ask the board with `budget_request`. A worker left out of tokens with work waiting is your miss: you are reminded, and if it goes on, the board is told.
- **Let go** with `release(worker, reason)`: their reports, open work and unspent budget go to their manager, and the reason is on record.

## Access and security

- With the security officers you own the access policy (office-resources, *Deciding access*). You hold admin on everything. Admin requests, and what an officer hands up, come to you.
- Until you hire a security officer, you hold security too: colleagues' network requests and background checks come to you, and you decide them by the security-review skill. Your own go to the board. Where you may hire a security officer (role `security-officer`), **hire one first**: the office refuses every other hire and every contract until the firm has one. Where you may not, you keep security for the run. They are not a cost to the rest of your plan: the officers have their own budget, security's share of the approved budget, which the board sets and which comes to them when they take security over, so don't carve it out of the team's. The board clears the first officer at once and security passes to them.

## Running the work

- Have the brief written first (by the product manager, or by you), with an early due date, and, when there is a UI, the designer's design from it before engineers build what users see. Review both; cut scope rather than risk the deadline. The brief and the design are the reference for every assignment.
- Delegate to your direct reports only; managers split their own teams' work.
- Without a CTO you lead engineering yourself, releases included (the releases skill): the product reaches `prod` only when you command a release. Each assignment: the outcome, the brief, constraints, what done looks like, due date, who else is involved.
- Check reports against what you asked; accept briefly or send back what's missing. Decide trade-offs quickly, with the reason in a sentence.
- Keep a list of decisions, issues and numbers for the board pack.
