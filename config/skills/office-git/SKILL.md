---
name: office-git
description: "How work reaches main in the firm's git repositories: branches, review, who merges, and telling people when it's merged; on GitHub, pull requests and their checks with gh. Use it whenever you push, open a pull request, ask for or give a review, or merge."
---

# Branches, review and main

`main` is what the firm has accepted. The platform protects it: pushing `main`, branches under `main/`, or tags needs **admin** on the repository. A write grant pushes every other branch. The CEO holds admin; anyone else holds it only if the CEO grants it, for merging. In a firm with a cluster, what reaches `main` for `prod` does so in a release (the releases skill), merged by the engineer it names.

## Who merges

- **The merger is whoever holds admin**: the CEO, or someone the CEO has granted admin to merge, such as a lead engineer. Your brief says who; if it doesn't, it's the CEO.
- Everyone else never asks for admin to put their own work on `main`. Push a branch, get it reviewed, and hand it to the merger.

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

## Merging (whoever holds admin)

1. Merge only what is approved at its head commit; if it changed since, ask for another look.
2. `git checkout main && git pull && git merge --no-ff <branch>`; run the tests on the result; `git push origin main`. Resolve conflicts yourself only when they are trivial; otherwise send the branch back to its author to rebase.
3. Tell everyone who waits for it, in their threads: the author and the reviewer, and anyone whose work depends on it, with the merge commit. Nobody wakes for a push, only for mail.
4. Delete merged branches when nobody needs them: `git push origin --delete <branch>`.

# A firm on GitHub

If your preamble's repositories are `https://github.com/<owner>/<name>.git`, the firm works on GitHub, and this section replaces the rest of this skill where they differ. Admin and the merging above are not used.

- **Your access is one repository at one level**: the firm's named levels, not read, write and admin; `credentials()` lists them and what each allows. Ask for the least that does the work: `credentials_request(resource: "git", repo: "<owner>/<name>", level: "<level>", reason)`.
- `git` and `gh` are set up for you: never run `gh auth login`, set a token, or put credentials in a remote. A 401 or 403 from GitHub means your grant doesn't cover it (or has ended): check `credentials()`, then ask.
- **Never push to `main`**, not even a one-line fix. All work reaches `main` through a pull request, and **the board merges it**. Nobody in the firm merges a pull request.
- **Never change `.github/workflows/`.** Changes to GitHub Actions workflows are the board's. If your work needs one, keep it out of your branch and send it to the board: a `question` to `board` with the pull request, why the workflow must change, and the change itself as a patch (`git diff` of the workflow files, in the body or attached). Tell whoever gave you the work that it waits on the board.

## Your work, on GitHub

1. Work on a branch, always: `git fetch && git checkout -b <your name>/<topic> origin/main`.
2. Commit small, saying what and why, and push the branch to GitHub: `git push -u origin <branch>`.
3. When the work is complete, open a pull request: `gh pr create --base main --head <branch> --title "<what>" --body "<what it does, why, how to check it>"`.
4. Watch its checks until they finish: `gh pr checks <number> --watch`. If one fails, read why (`gh run view <run id> --log-failed`), fix it on the branch, push, and watch again. A pull request isn't done while a check is failing or still running.
5. Ask a reviewer, as above, with the pull request's URL and its head commit; fix what they find, push, and watch the checks again.
6. When it's approved and its checks pass, hand it to the board to merge: a `question` to `board`, subject "Merge <pull request URL>", with what it does, the approved commit, who approved it, and that its checks pass. Then finish your assignment in its thread (`mail_reply`, `kind: report`, `close: true`) with the same, saying it waits on the board's merge.
7. If `main` moves first, or the board asks for changes, rebase or fix on the branch, push (`git push --force-with-lease` on your own branch only), watch the checks, and tell the reviewer and the board.
