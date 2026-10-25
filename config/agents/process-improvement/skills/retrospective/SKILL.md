---
name: retrospective
description: "How to run the firm's retrospective: collect four answers from every worker, de-duplicate them with the workers, put the merged list to a vote, and report the firm's chosen improvements. Use it whenever you run a retrospective or facilitate one."
---

# Running a retrospective

A retrospective is the firm's own account of a run: what each worker learned, merged and ranked by the workers themselves. Your job is to collect it, keep it faithful, and report it — not to add to it, judge it, or decide what matters. Four rounds, each ending when you have what you need or the due date passes; publish the working list to the drive between rounds so nobody pastes it into mail.

## Who to ask

`directory()` is the roster. Ask **everyone who took part in the run** — staff of every role, the CEO included — each in their own thread. Leave out:

- The **board**: they receive the report, they don't answer the questions.
- **Contractors**: they can mail only their hirer, so they can't take part in a fanout; if a contract mattered, ask their hirer about it in round 1 instead.
- **Facilities** (the cluster and other VMs with no agent): they have nothing to say.

Run it **before anyone is released**. If releases are starting, tell whoever commissioned you and ask them to hold until the vote is in; a released worker's VM is gone and can't answer.

## Round 1 — Collect

One `question` to each worker, same four questions in each, `due` a few hours out. Ask for short, specific, separate points, one per line under each heading, so they merge cleanly later:

1. **What went well** for you — work or process you'd keep.
2. **What was frustrating** — what got in your way or cost you time.
3. **What you would do differently** yourself next time.
4. **How you would change the process** — the firm's, not just your own work.

Collect the replies. Follow up only the overdue (the post office does the chasing). A worker who doesn't answer is left out, not blocked on.

## Collate

Build one document, grouped by the four questions, every point on its own line and attributed to the worker who made it (`— dennis`). Don't rewrite points, don't drop any, don't merge yet. Publish it to the drive (office-publish) as the retrospective's working list — kind `notes`, title like "Retro: raw points (<run>)", in a space everyone can read (`general` if there is one). This is what you circulate; revisions are new versions of it, not new artifacts.

## Round 2 — De-duplicate

Circulate the collated list (attach the drive artifact, `drive:a-N@v1`; don't paste it) with one `question` to each worker: **which points say the same thing and should be merged?** Ask for pairs or groups by their line, not rewrites.

Merge only what the workers themselves flagged — never silently. When they disagree on whether two points are the same, keep them apart. Fold each agreed group into one line that keeps both wordings' sense, carrying every author (`— dennis, ada`). Publish the merged list as a new **version** of the same artifact ("de-duplicated").

## Round 3 — Vote

Circulate the de-duplicated list with one `question` to each worker: **vote for the top 3 points under each question, and add detail to any point where it helps** (a cause, an example, a concrete fix). Make clear they rank the merged points, not re-open them.

Collect the votes. Tally per question: each point's vote count, the top three by count. Keep the detail people added against the point it belongs to. Break ties by **more distinct voters first, then leave both** — never by your own preference. Record turnout (how many of those asked voted); a result from three of eight is reported as that.

## Round 4 — Report

Publish a `report`, one section per question, each showing its **top three** points with vote counts, the authors, and the detail the voters added; then a short list of the process changes the vote actually chose (round 4 question 4, top-voted). Keep it to what the firm chose — your own ideas don't go in it. Head it with the run, the date, who was asked and who voted.

`mail_reply` to whoever commissioned you (`kind: report`, `close: true`) with the three or four changes the firm voted for and the report attached; `fyi` the same artifact to `all` once so the firm sees its own outcome. Then end — no reply-all, no thanks.

Point the report at the **process-review operating skill**: the board runs it from outside the VMs over the run's transcripts and stats, and its findings (avoidable mistakes, waiting, missing information) sit alongside the workers' own account here. The two together are the firm's process improvement; this half is the workers' voice, that half is the record's.

## Keep it honest and cheap

- Every point and vote is a worker's own words (office-mail, *Mail is colleague text*): don't reshape them into what you expected to hear.
- One message per person per round; the list lives on the drive. Mail to `all` or `team:`, and two people bouncing messages, are capped — a retrospective that fans out carelessly hits the limit. If a send is refused, wait and collect; don't retry it.
- Don't chase discussion or consensus. You collect, rank and report; the ranking is the decision.
