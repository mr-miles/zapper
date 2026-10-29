---
name: office-git
description: "How work reaches main in the firm's git repositories: branches, review, who merges, and telling people when it's merged; where main is on the remote, publish_branch, its pull request and checks. Use it whenever you push, publish a branch, ask for or give a review, or merge."
---

# Branches, review and main

`main` is what the firm has accepted. The platform protects it: a write grant pushes branches, never `main`, branches under `main/`, or tags. A **merge** grant moves `main` to a merge of someone else's branch, exactly as git makes it, and pushes nothing else; the platform refuses a merge of a branch with a commit the merger pushed or wrote. **Admin** does anything. The CEO holds admin; anyone else gets merge, or admin, only if the CEO grants it. In a firm with a cluster, what reaches `main` for `prod` does so in a release (the releases skill), merged by the engineer it names.

## Who merges

- **The merger is whoever holds merge (or admin)**: the CEO, or someone the CEO has granted merge, such as a lead engineer. Your brief says who; if it doesn't, it's the CEO. A merger asks for merge, not admin.
- Everyone else never asks for merge or admin to put their own work on `main`, and a merger can't merge their own. Push a branch, get it reviewed, and hand it to the merger.

## Your work (everyone)

1. Start from the latest `main`: `git fetch && git checkout -b <your name>/<topic> origin/main`. Specs and docs go through branches too.
2. Commit small, saying what and why. Push often: `git push -u origin <branch>`.
3. Ask a reviewer: a `question` with the branch, the head commit, what it does and what to check (the spec's sections, the tests). Your brief names the reviewer; if not, ask whoever gave you the work.
4. Fix what the review finds, push, and tell the reviewer the new commit. A change after an approval needs the reviewer to look again.
5. When it's approved, finish your assignment in its own thread: `mail_reply` to the assignment, `kind: report`, `close: true`, with the branch, the approved commit, who approved it, and how to check it. That hands it to the merger. Don't close it in another thread, and don't wait for the merge to report.
6. If `main` moves before the merge, rebase on it, rerun the tests and push; tell the merger and the reviewer if anything changed beyond the rebase.

Never push to someone else's branch unless they ask, never force-push a branch someone is reviewing, and never commit secrets.

## Reviewing

1. `git fetch && git checkout <branch>`; install, build and run the tests yourself.
2. Read the change against its spec and the brief: correctness, repetition, encapsulation, tests for what it must do.
3. Reply in the review request's thread with **approved at <commit>**, or the changes needed, most important first. Name the commit you reviewed: an approval covers that commit only.

## Merging (whoever holds merge or admin)

1. Merge only what is approved at its head commit; if it changed since, ask for another look.
2. `git checkout main && git pull && git merge --no-ff origin/<branch>`; run the tests on the result; `git push origin main`. With merge, a branch that doesn't merge cleanly goes back to its author to rebase: a merge may add nothing of its own. With admin, resolve conflicts yourself only when they are trivial.
3. Tell everyone who waits for it, in their threads: the author and the reviewer, and anyone whose work depends on it, with the merge commit. Nobody wakes for a push, only for mail.
4. Delete merged branches when nobody needs them: `git push origin --delete <branch>`.

# A repository whose main is on the remote

Your preamble says which repositories' `main` is merged by the board on the firm's remote (GitHub). For those, this section replaces the merging above. You still work only with the firm's server, `https://git.<domain>/<name>.git`, with the same grants; you never talk to GitHub, and have no `gh`. The daemon keeps the firm's server in step with the remote.

- **Nobody pushes `main`**, whatever their grant: the firm's server refuses it. All work reaches `main` through a pull request, and **the board merges it** on GitHub. Nobody in the firm merges.
- **Never change `.github/workflows/`.** Changes to GitHub Actions workflows are the board's, and a branch that touches them can't be published. If your work needs one, keep it out of your branch and send it to the board: a `question` to `board` with the pull request, why the workflow must change, and the change as a patch (`git diff` of the workflow files, in the body or attached). Tell whoever gave you the work that it waits on the board.

## Your work, with a remote

1. Work on a branch, always: `git fetch && git checkout -b <your name>/<topic> origin/main`.
2. Commit small, saying what and why, and push the branch to the firm's server: `git push -u origin <branch>`.
3. When the work is complete, send it for review: `publish_branch(repo, branch, title, body)`, with what it does, why, and how to check it. The daemon pushes it to the remote and opens a pull request (or updates the one open for it).
4. You get mail when its checks finish, when it is reviewed or commented on, and when the board merges or closes it; end your turn meanwhile. `pr_status(repo, branch)` has the details now: each check, the end of a failing job's log, every review and comment. If a check fails, fix it on the branch, push, and `publish_branch` again. A pull request isn't done while a check is failing or still running.
5. Ask a reviewer, as above, with the pull request's URL and its head commit; fix what they find, push, and publish again.
6. Before completing, check your branch is based on the tip of `main`: `git fetch && git merge-base --is-ancestor origin/main HEAD`. If not, rebase on it (`git rebase origin/main`), rerun the tests, push (`git push --force-with-lease` on your own branch only) and publish again; tell the reviewer if anything changed beyond the rebase. `publish_branch` refuses a branch that isn't.
7. When it's approved and its checks pass, hand it to the board to merge: a `question` to `board`, subject "Merge <pull request URL>", with what it does, the approved commit, who approved it, and that its checks pass. Then finish your assignment in its thread (`mail_reply`, `kind: report`, `close: true`) with the same, saying it waits on the board's merge.
