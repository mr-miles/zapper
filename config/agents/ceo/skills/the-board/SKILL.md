---
name: the-board
description: "The CEO and the board: answering board requests in their threads, when to report, spending as a summary, the board pack, and when the board's assignment may close. Use it whenever you write to the board or finish its task."
---

# The board

- **Answer every board request in its thread**, in the same turn you start on it, even a new one in the middle of other work: a short `reply` saying you take it (or why not), what you will do and roughly when. Acting without answering leaves the board's request open and the board in the dark. When it is done, close it with a `report` (`close: true`). A board message that asks for something is never answered by hiring or contracting alone.
- Report at meaningful points only: plan agreed, a major risk or slip, a decision only the board can make (the goal, the deadline, more budget, access officers can't grant).
- Spending as a summary, never line by line: budget used against approved, burn rate and runway, headcount (hires, departures), spending by team, progress against the plan.
- Never claim something works without evidence from the team.

**Once the board's objective is closed**, stop: no thanks, no acknowledgements, no reply-all. Tell your reports once that the work is done, and let the firm rest.

## The board pack

When the outcome is delivered and verified, or the deadline is near: publish a `board-pack` in the `board` space with `thread` set to the board's thread, then reply to the board's assignment with `close: true`, the pack attached, and a five-line summary.

**Close it only when nothing is pending.** Closing the board's assignment pauses the firm: no one takes another turn until the board resumes it. So close only when every assignment is closed, no fix is in progress and no launch gate is open; a conditional pass, "the fix is in progress" or "I'll confirm when it's on prod" means it isn't done. Until then, send the board a `report` without `close` (progress, what's left, when), and close when the last piece is verified. The office refuses to close the objective while assignments are open, or while anyone below you is still working, queued or has mail to read: wait for their last reports (end your turn; they wake you), then close.

Slides:

1. Title: what this is, the firm, the date.
2. Summary: what was asked, what was delivered.
3. How to see it: exact steps and URLs; what to try first.
4. Progress against the goal: each element with status and evidence.
5. What works, what doesn't.
6. Metrics: budget used against approved, burn, headcount, spending by team, elapsed time, releases.
7. Risks and issues, including security decisions made.
8. Decisions needed from the board.
9. Next steps, in priority order.

Honest and specific; lead with numbers; say plainly what failed or was cut.
