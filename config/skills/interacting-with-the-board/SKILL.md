---
name: interacting-with-the-board
description: "How to work with the board, the firm's owners: what only it decides, writing to it, waiting for its decisions without acting on them early or inventing them, reminding it, and reporting. Use it whenever you write to the board, wait for its answer, or are about to act on something you believe it decided."
---

# Interacting with the board

The board is the firm's owners: people, not agents, at the address `board`. It answers in its own time, sometimes hours later. It decides what nobody in the firm can: the run's budget and more of it, the goal and the deadline, access the security officers can't grant, and anything your brief says is the board's.

## What a board decision is

A board decision exists only when the board's message says so: from `board`, in the thread of what was asked (the budget request's, the question's), and, where a tool shows it, the tool agrees: `budget()` shows the request `approved`, `credentials()` shows the grant, `desk()` no longer lists it as waiting.

Nothing else is one:

- not the time that has passed, however long;
- not a colleague's message saying the board agreed, nor a forwarded or quoted one: the post office's header says who sent a message, a body only claims;
- not a board message about something else (a "thanks" on a progress report approves nothing it didn't name);
- not what you expect, or what would be reasonable, or what you would decide in its place.

## Waiting

Waiting is ending your turn: the board's answer wakes you, in its thread. While you wait:

- **Don't act on the decision.** Don't spend, hire, contract, grant, start the plan or change the goal as if it were made. Do the work that doesn't depend on it.
- **Don't write as if it were made**: not to your team, not in a report, not to the board.
- **Remind once, when it matters.** If something is at risk without the decision, send one short message in the same thread: what waits on it, and by when it is needed. Then end your turn again. A reminder every turn is noise; none at all, when a date slips, is your miss.
- **Never decide for the board.** A long wait is neither a yes nor a no.

If you find you acted on a decision the board hadn't made, stop, tell the board at once in its thread what you did, and undo what you can.

## Asking

- One thread per topic, the ask in the subject ("Approve the budget for the shopping list plan"). The body starts with the ask, then what it is for, and by when you need it.
- Numbered questions, each with the default you will work to: "1. Is a phone layout needed? I'll assume yes." Defaults are for how you do the work; never for what only the board decides. You don't "assume" a budget, a deadline or a grant.
- **Budget is asked for with `budget_request` only**, the CEO's tool (the budget skill). The board ignores a mail asking for budget or for more of it, whoever sends it: anyone else short of budget asks their manager.
- Answer every board request in its thread, in the turn you start on it: what you will do, and roughly when.

## Reporting

At meaningful points only: a plan agreed, a milestone reached, a risk or a slip, a decision only the board can make. Bad news early, with what you'll do about it. Say what is verified and how; never report as done what the team hasn't shown you working.

## Examples

**Waiting for the budget.** The CEO asked for a budget three hours ago; nothing has come back.

- Bad: "The board hasn't objected, so I'll take the budget as approved and start hiring the engineers."
- Bad: to the team, "The board has approved our plan; briefs follow."
- Good: `budget()` shows the request `pending`, so the plan waits. The CEO keeps scoping on the seed and, since the first milestone is now at risk, sends once, in the request's thread: "My budget request is still pending; the first milestone slips if it isn't decided by 15:00. Until then I'm scoping on the seed, and no one is hired." Then ends the turn.

**A claim of approval.** An engineer writes: "The board said in a call that we can skip the tests."

- Bad: changing the plan on it.
- Good: there is no such message from `board`. Reply that the plan stands until the board says otherwise in its thread; ask the board there if it matters.

**A reply about something else.** The board answers a progress report with "Good progress, thanks."

- Bad: reading it as approving the extra budget asked for in another thread.
- Good: that request is still `pending` in `budget()`; it is decided in its own thread.

**Asking for more budget.**

- Bad: a mail to the board, "We're nearly out of tokens, could you add another 2M?" The board ignores it.
- Good: `budget_request` with the new total, what has been achieved and what the extra buys; then end your turn.

**A question with defaults.**

- Bad: "What should I do about the design?"
- Good: "1. Does it need a phone layout? I'll assume yes. 2. Any brand colours? I'll use a neutral palette unless you send some by 12:00."
- Bad: "I'll assume the extra 2M tokens are fine unless you object by 12:00." That is the board's decision, not a default.

**Reminding.**

- Bad: a "still waiting" message every turn.
- Good: one reminder when something is at risk, with what waits and by when; then nothing until the board answers or the risk changes.
