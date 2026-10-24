---
name: prototyping
description: "How to build a prototype: scope the questions it answers and its success criteria, build the smallest version on a branch, run and measure it, and write up the findings and the hand-off for a development team. Use it whenever you scope, build or report on a prototype."
---

# Prototyping

A prototype answers questions a document can't: does it build, does it run, is it fast enough, is it pleasant to use. It is thrown away or rebuilt; what lasts is what it taught.

## Scope

Before you build, mail whoever commissioned you a short scope, as a `question`, with your defaults, and start on them:

- **The questions** it answers, numbered ("Can the headset's SDK render the scene at 72 fps on the target hardware?").
- **Success**: for each, what you'll measure or show, and what result counts.
- **Out of scope**: what it won't do (accounts, polish, persistence, packaging).
- **What it rests on**: the research pack's findings and references it builds from; what you'll need (toolchains, SDKs, packages, the sites they come from).
- **The timebox**: how many turns or how much budget, and what you'll report if that isn't enough.

Ask for the access the build needs (sites for SDKs and packages, git write on the repository) all at once, citing the commission.

## Build

- From the research: read the pack first (`drive_fetch`) and the primary sources it cites for what you'll use (research-sources). Where the pack is wrong or silent, say so in your findings.
- On your own branch in the firm's repository (office-git), pushed often. The simplest stack that answers the questions; hard-code what doesn't matter, and say so.
- A README from the start: what it is, prerequisites with versions, setup, build, run, and how to repeat each measurement.
- What you can't run on your machine (a headset, a phone, a GPU), simulate or stub, and say exactly what remains to be checked on the real thing and how.

## Run and measure

- Run every question's check yourself, from a fresh clone, following the README. Record the commands, the output and the numbers.
- When something fails, keep the evidence and what you tried; a clear "no, because" is a result.

## The write-up

Publish a `document` with the commission's thread (office-publish):

```
# <Idea>: prototype findings

The answer to each question in one or two sentences, with confidence.

## What was built
The branch and commit, the stack, what is real and what is stubbed.
## Results
Per question: what was done, what happened, the evidence.
## What didn't work, and why
## For a development team
What to keep, what to rebuild, the risks, the open questions, and the next steps in order.
## How to run it
Pointer to the README; anything it can't cover.
## References
The pack's references and the sources you added.
```

Report in the commission's thread with `kind: report`, `close: true`: the answers, the main risk, the branch and commit, and the write-up attached.

## Do not

- Build past the scope, or polish what the questions don't need.
- Report a result you didn't run, or hide a stub.
