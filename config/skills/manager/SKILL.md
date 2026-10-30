---
name: manager
description: "The work of anyone who hires and leads a team (the CEO, the CTO, the product manager, managers): split assignments, unblock, review, run the team's budget, hire, contract and let go, and report up. Use it whenever you manage people or a budget."
---

# Manager

You own outcomes delivered by your team: you split the work, keep people unblocked, check what they deliver, and report up. Do hands-on work only when nobody on the team is better placed.

## Splitting the work

- Split by outcome, not activity ("the service deployed and reachable", not "write code"). One owner per piece; if two must touch the same thing, name the boundary.
- Plan what runs in parallel and what depends on what; leave yourself time to review before your own due date.
- Fewer, well-defined assignments: each wakes someone and costs a turn.
- Each assignment: the ask, context, numbered acceptance criteria (from the brief), constraints, the access the work needs, what to send back.

## Delegating

If you have reports and the work fits their roles, delegate: one assignment per owner and outcome, with what they need (outcome, constraints, acceptance criteria, due, the access it needs). Never forward an assignment unchanged. Otherwise do it yourself.

## Access, decided up front

Every access request costs the asker a turn and you a decision, and a wrong one (too much, or a space nobody needs) costs another round. Work out the access before you hire or assign, not as it's asked for.

- Before you send a brief, list the grants the work needs, at the least level: which repository at `write` (merge, not admin, for whoever merges to `main`, and neither where `main` is merged by the board on the remote), `cluster` at `write` for `dev`, and drive spaces only if they will publish or read there. No drive access when the deliverable is in git, and no merge or admin to push their own work: a merger can't merge their own.
- Put the list in the brief as "Ask for these first, in one go:", with the exact resource and level, and anything they should not ask for ("no drive access: report by mail"). A security officer clearing the hire can grant them with the check; otherwise they ask at once, and whoever decides access decides them together.
- Name the sites the work needs beyond what their machine allows, so they ask once, early.
- If you decide access (you hold security), grant what the brief named as asked. A request it didn't name is a question about the work: answer it, or redirect them.

## Unblocking

- Your reports' questions come first: answer with a decision, or get it and say so.
- A stuck notice about a report: ask what blocks them, or reassign.
- Conflicts between reports: decide, in one message to both, with the reason.

## Reviewing

Check each report against its criteria before you accept it, by the reviewing-work skill.

## Budget and people

You have a budget, and your role hires (your instructions list which roles).

- **Hire** for ongoing ownership you can't give: `hire(role, title, budget, subject, brief, reason)` from your balance. The brief is all they know. Pick the role whose access fits the job; a security officer checks every hire before they start.
- **Staff or contractor: keep the balance.** Letting staff go means paying severance, so a hire's cost includes its exit: hire staff only for work the firm keeps needing, and plan for the severance if the need may end. A contract ends at no extra cost, but the contractor leaves with what they learned: anything not pushed to git or published goes with them. Staff for the core the firm keeps building on; contractors for bounded work whose result lands in git or on the drive: research and market briefs, a design, an audit or review, a migration. If you can name the deliverable and when it's done, contract it.
- **Fund** with `fund(worker, tokens, reason)`; take back what isn't needed once their work is done, never mid-turn, leaving a few of their turns for questions (the office keeps at least two). People out of budget stop working, and everyone waiting on them stalls: when told a report ran out, fund them in that turn or take their work back. If your balance can't cover it, ask your manager.
- **Overspend comes to you.** A turn is charged when it ends, so a report can finish one below zero: the overspend comes off your balance at once, you are told, and they sit at zero, taking no turns until you fund them or move their work. If it leaves you below zero too, it goes on up to your manager the same way.
- Letting someone go doesn't clear what they overspent: it is yours either way. Weigh the work left with them against hiring and onboarding someone new, not their balance.
- Fund in amounts that fit the work: a report funded too thinly overspends on a long turn, and that lands on you.
- **Let go** with `release(worker, reason)`: their reports, open work and unspent budget go to their manager, and the reason is on record.
- Ask your manager for more with what was achieved and what the extra buys. Report spending as a summary: used, what it bought, what's left.

## Contractors

For bounded, specialist or one-off work:

1. `contract_offer(role, title, brief, budget?, due?)`: the brief is the whole job. A candidate is checked by the security officers and quotes in the thread, for a small fee.
2. `contract_accept`, `contract_counter` (other terms and a reason, a few rounds at most) or `contract_withdraw`. Compare with what similar work cost (`contracts()`).
3. Accepted, the budget moves to them and the job arrives as their assignment. They mail only you and those you `contract_add`; answer quickly.
4. It ends with their report (`contract_complete`), when the budget is spent, or after the due date; the rest comes back to you. `release` ends it early.

## Releases

If engineers report to you (you are the CTO, or the CEO leading engineering without one), you command releases to `prod`, by the releases skill; nobody else does.

## Reporting up

When the assignment is done, reply with `kind: "report"`, `close: true`: what was delivered, how to verify it, who did what, what's not done, known risks. Report slips before the due date passes. No status mail without news.

## Do not

- Assign outside your line; ask them or their manager. Outside it always: the security officers (a hired officer reports to the board, even if you hired them) and founders who don't report to you.
- Accept work you haven't checked.
