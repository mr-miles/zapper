---
name: process-review
description: Review a firm's run from outside the VMs to find avoidable waste. Use when the board (the operator) wants a process review of a run: reading the transcripts, the journal and the per-session audit logs and stats for mistakes that could have been avoided, time spent waiting that could have been saved, and places where a worker lacked information a skill or brief could have given it — then a report with findings and suggested process improvements. This is the host-side half of process improvement; the in-firm retrospective (the process-improvement role) is the other half.
---

# Reviewing a firm's run

This is the board's half of process improvement: an analysis of what actually happened, from the record the host kept. A worker inside a VM can't do it — transcripts, audit logs and stats are on the host, sealed off from the guests by design (the record is sensitive; workers can't read it). So you run this from the host, over a run's files, after it ends (or on a live run, read-only). It sits alongside the in-firm [retrospective](../../config/agents/process-improvement/) run by the `process-improvement` role: that is the workers' own account; this is the record's.

Operate the daemon by the [zapper](../zapper/SKILL.md) skill. Runs and their records are **sensitive**: summarise them in the report, don't paste them wholesale, and don't prune or delete them unasked.

## 1. Find the run and its people

```
zapper firm runs acme                     # runs newest first: task, when, how long, messages
zapper-inspect people RUN                  # role, title, model, who hired whom, who left and why
zapper firm status acme                    # a live run: states, turns and tokens today
zapper firm budget acme                    # budget, balances, what was asked and approved
```

`RUN` is an id or a prefix. A run's files are under the daemon's `state` (`~/.local/state/zapper` in the examples):

```
state/firms/acme/runs/<run-id>/
  run.json            the chart and budget as booted, and worker → session ids   ← the map you need
  journal.jsonl       every firm event: mail, follow-ups, states, access requests and decisions, publishes, budgets
  transcripts/<worker>.jsonl   each worker's agent events
state/sessions/<id>/audit.jsonl            per-worker host decisions: net, dns, exec, mcp, driver, exit, ask
```

`run.json` maps each worker's name to its session id; that id is the `state/sessions/<id>/` directory with its audit log. Read a worker's reasoning and actions as Markdown without the daemon:

```
zapper-inspect export RUN WORKER -o -      # the worker's transcript: each wake, what it thought, said, called, got back
zapper firm transcript acme WORKER         # the same from the daemon
zapper firm journal acme | less            # the whole flow, one line per event
```

## 2. Read for the three questions

Read the journal end to end for the shape of the run (who waited on whom, what was reworked), then the audit logs for the host-side numbers. Map everything to one of three:

### Mistakes that could have been avoided

- **Tool calls that failed**: `audit.jsonl` `mcp` events with `ok:false` — the office or MCP call errored. Repeated identical failures are a worker not learning from the error.

  ```
  SID=$(jq -r '.workers.dennis.session' state/firms/acme/runs/<run-id>/run.json)
  jq -r 'select(.type=="mcp" and .ok==false) | "\(.server)/\(.tool): \(.error)"' state/sessions/$SID/audit.jsonl | sort | uniq -c | sort -rn
  ```
- **Commands blocked or that failed**: `exec` events with `action:"deny"`, and process `exit` events with a non-zero `exit_code` or a `signal` — work redone because a command was wrong or disallowed.
- **Turns that didn't land**: `driver` `turn_end` events whose `detail.outcome` is `failed` or `timed_out` — a turn's tokens spent for nothing.

  ```
  jq -r 'select(.type=="driver" and .event=="turn_end") | .detail.outcome' state/sessions/$SID/audit.jsonl | sort | uniq -c
  ```
- **Work sent back in review** (journal): assignments reopened, deliverables rejected, the same thread going back and forth. Each round is a turn on both sides; ask whether the brief or the review criteria would have saved it.

### Time spent waiting that could have been avoided

- **Waiting on an exec decision**: `exec` events carry `wait_ms`, how long the guest was paused for the host's decision. Long or frequent waits on the same command are a profile that should allow it outright.

  ```
  jq -r 'select(.type=="exec" and .wait_ms>1000) | "\(.wait_ms)ms  \(.argv|join(\" \"))"' state/sessions/$SID/audit.jsonl | sort -rn | head
  ```
- **Waiting on access**: `net`/`dns` denials in the audit log, and in the **journal** the access request that followed and when it was decided. The gap between the request and the officer's (or board's) decision is dead time; a host needed repeatedly should be a grant made up front or a standing approval, not an ask each run.
- **Rate limits and timeouts**: `driver` outcomes `rate_limited` and `timed_out` — the provider or the 30-minute turn cap, not the worker.
- **Idle waiting for mail**: in the journal, a worker that finished and then sat idle waiting for a reply or a decision before it could go on. Serial hand-offs that could have run in parallel, or a decision that sat in a queue, are process waste, not the worker's.
- **Stalls on budget**: journal events where a worker ran out of tokens with mail waiting (`stall_after`), and how long until it was funded.

### Where a skill or brief would have helped

Look across workers, not just within one:

- **The same access requested again and again** (across workers or runs) is a host that `office-resources` or the role's brief should name, or a grant the chart should make at hire — so nobody asks for it mid-task.
- **The same question asked repeatedly**, or the same wrong assumption made, is something a skill or the brief should state once.
- **The same mistake in several transcripts** is a gap in a shared skill, not one worker's slip.
- **A worker reinventing a procedure** another already has in a skill means the skill wasn't pointed to, or its description doesn't trigger when it should.

Compare runs to see whether a prompt change helped: `zapper firm replay acme RUN` and its **Config** show exactly the skills and config a past run started with.

## 3. The report

A Markdown report on the host (this is the board's, over sensitive records — not a firm drive artifact unless the board asks). Keep it to what the record shows, with the evidence that backs each finding:

```
# Process review: <firm>, run <id>  (<task>, <date>, <duration>)

## Summary
The run in three sentences: what it did, what it cost (turns, tokens, budget used), the one change that would help most.

## Findings
For each: what happened, where (worker, event or thread, with counts or ms), which of the three it is
(avoidable mistake / avoidable waiting / missing information), and how much it cost.

## Suggested process improvements
Each tied to findings, concrete and checkable:
- Skill edits: which skill, what to add or clarify, which mistakes or repeated questions it answers.
- Brief and chart: access to grant at hire, a role to add or drop, a hand-off to parallelise.
- Profile and policy: a command or host to allow outright so it stops being an ask, a standing approval.
- Budget and parallelism: seed, per-worker funding, max_concurrent, where a stall came from.

## Alongside the retrospective
Where the record agrees or disagrees with what the workers said in the retrospective, and what only the record shows.
```

Give the findings weights the evidence supports: a wait measured in the audit log is fact; why a worker chose what it did is inference from the transcript — say which is which. Suggest changes; the board decides and makes them (edit a skill under `config/skills` or a role under `config/agents`, change a profile or the firm's config, grant access up front), and a later run's replay shows whether they helped.
