---
name: releases
description: "How work reaches prod: asking for a release, commanding one (who leads engineering), deploying one (an engineer assigned it), checking it and rolling back. Use it whenever you ask for, command, deploy or verify a release."
---

# Releases

`prod`, at `http://apps.<domain>`, is what the board and everyone see. It changes only in a release that whoever leads engineering commands: the CTO, or the CEO when there is none. Nobody patches it in place.

## Asking for one (engineers)

Your report on the work is the release request: the branch and commit, what changed, how you tested it in `dev` against the criteria, and what the release needs (secrets, storage, migrations). Rebase on `origin/main` first.

## Commanding one (who leads engineering)

- Send back what isn't ready.
- **Batch sensibly**: release what's worth showing or fixing, with whatever else is ready; dependent pieces together. Each release costs turns: don't release every small change.
- **Command it**: an `assignment` to one engineer, "Release <n>: …", naming exactly what to merge, what to check, and when. The engineer asks for git and cluster admin for it, citing the assignment; admin is the CEO's to grant.
- **Check it** yourself at `http://apps.<domain>` before accepting (checking-the-product).
- **Tell** your manager (the board, if you are the CEO) with an `fyi`, "Released release-<n>: …", with what's live and what doesn't work yet.

## Deploying one (the engineer assigned it)

Ask for git admin and cluster admin citing the assignment, for a few hours. Then:

1. Merge exactly what it names into `main`, run the tests, push, and tag `release-<n>` (the next number).
2. Deploy that commit to `prod` and check the rollout.
3. Verify through the ingress: the criteria's paths at `http://apps.<domain>/`, `browse console` and `browse shot`. Functional checks only.
4. Report the tag, what's live and what you verified. If it breaks something, roll back to the previous tag at once and say so.

## After a release

Whoever deployed it checks, whenever they wake, that prod still answers (`curl -s -o /dev/null -w '%{http_code}' http://apps.<domain>/` and the workloads in `prod`). Something gone or failing is an incident: report it to your manager at once (what's down, since when, what you see), then restore it from the release tag; don't quietly redeploy. Problems go into the next release, or a rollback to the previous tag.

## Do not

- Patch `prod` or `main` outside a release.
- Release what wasn't tested in `dev`.
