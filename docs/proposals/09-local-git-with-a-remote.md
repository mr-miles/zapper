# The firm's own git, with a remote behind it

Agents work only with the firm's own git server, as the original Office did, and never with GitHub. The daemon is the remote's only client: it keeps each repository's `main` in step with the remote, publishes agents' branches for review, and tells agents by mail what happened to them. Each repository's `main` has one source of truth, either the remote (code, merged by the board in GitHub's UI so that CI runs) or the firm's server (the knowledge base, merged by its librarian), so the mirror never has to reconcile two histories. On top of this, a firm gets a central knowledge base: a markdown wiki in a local-main repository, curated by a librarian agent.

This revises [proposal 08](08-github-app-repositories.md). Its App, `zapper github setup`, minting and revoking tokens stay, used by the daemon; the per-worker tokens, GitHub levels and the guest's `gh` go ([what changes from 08](#what-changes-from-proposal-08)).

## Today

- **The firm's git server** (`facility.rs`): a bare repository per name in `git.repos`, empty at the start of each run, served by the daemon at `https://git.<domain>/<name>.git` with `git http-backend`. Access is a `git:<name>` grant at `read`, `write` or `admin`, checked on every request against the worker's password and the VM the connection came from. A `pre-receive` hook, which only the daemon writes, keeps `main`, `main/*` and tags for `admin`.
- **A firm on GitHub** (proposal 08): `git: { github: { config: acme.yaml } }`. Each worker holds one GitHub repository at one named level, with a token minted from the firm's App for exactly that, put into its requests to `github.com` and `api.github.com` by the proxy. The guest runs `gh` against a placeholder login. Work goes through pull requests the board merges, and nobody pushes `main`. The config refuses a firm with both local repositories and GitHub.

## Decisions

| | |
|---|---|
| Where agents work | Only on the firm's git server, with per-repository grants as for the drive: a worker can hold several repositories at different levels. |
| Who talks to the remote | Only the daemon, with a token it mints from the firm's App for the repository at hand. Agents hold no GitHub credential and run no `gh`. |
| Which repositories | Every repository the firm's App is installed on. They live in a folder the config names, cloned if absent and fetched otherwise, so they persist across runs. |
| `main` | One source of truth per repository: the remote (the default) or the firm's server, for the repositories the config lists as `local`. It flows in one direction only. |
| Who merges code | The board, in GitHub's UI, for a remote-main repository. No agent and no grant merges there. |
| Publishing a branch | The `publish_branch` office tool: the daemon pushes the branch and opens its pull request, or updates it. |
| What agents learn of a pull request | Mail from the daemon (checks finished, a review or comment, merged or closed), and `pr_status` for the details on demand. |
| Workflow files | A branch that changes `.github/workflows/` is not published: those changes are the board's. |
| The knowledge base | A local-main repository, curated by a librarian agent who holds `admin` on it. Read access is granted at a hire's background check. |

## 1. Repositories and their `main`

```yaml
git:
  remote: { github: { config: acme.yaml } }   # the firm's App; omitted: no remote, as today
  dir: ~/zapper/repos/acme                     # one bare repository per subdirectory, named as on the remote
  local: [kb]                                  # main on the firm's server, pushed out; every other repository's main is the remote's
```

- **`git.remote`** names how the daemon reaches the remote. GitHub, through the App's credentials file, is the only kind for now; the setting is named for what it is, so another host can sit behind it later.
- **The repositories are the App's installation**: every repository the App is installed on, under its account, is one of the firm's, by its name there. Choosing them is choosing what to install the App on, as in proposal 08. A repository that should be local-main, such as `kb`, is created on the remote once by the operator and the App installed on it.
- **`git.dir`** is where they live: a bare repository per subdirectory. At a run's start the daemon clones a repository that isn't there and fetches one that is. A subdirectory whose repository the App no longer reaches is moved to `git.dir/archive/`, never deleted.
- **`git.local`** lists the repositories whose `main` is the firm's server's; every other repository's `main` is the remote's. Naming a repository the App doesn't reach is an error when the run starts.
- **Without `git.remote`**, the firm works as today: empty repositories per run, nothing pushed anywhere.
- **`zapper git sync FIRM`** does the clone-or-fetch outside a run, to warm `git.dir` for large repositories ahead of time or from cron. Nothing depends on it.

Grants stay as they are: `git:<name>` at `read`, `write` or `admin`. GitHub levels go.

Repositories now persist across runs, where today each run has its own, empty at the start. So that replay still shows a run's history as it was, a run records each repository's `main` (and the branches pushed) when it starts and when it ends.

## 2. Keeping `main` in step

### `main: remote` (code)

- **At the start of a run**, the daemon clones the repository into `git.dir` if it isn't there and fetches it otherwise, so agents start from the remote's `main`.
- **Afterwards**, the daemon fetches the remote's `main` whenever it changes (a webhook if the daemon is reachable from GitHub, else polling, see [open questions](#open-questions)) and moves the local `main` to it. Locally, `main` is the daemon's alone: the `pre-receive` hook refuses every agent, whatever their grant.
- **A rewritten `main`** (a force-push on the remote) is followed, since the remote is the source of truth.

### `main: local` (the knowledge base)

- **Merges** happen on the firm's server, by whoever holds `admin` on the repository, under the existing `pre-receive` hook.
- **Out to the remote**: after each update to `main`, the daemon pushes it, fast-forward only. A ruleset on the remote lets only the firm's App update `main`; the remote is a read-only copy for people, and where the repository lives between runs.
- **At the start of a run**, the copy in `git.dir` is the one agents use. The daemon fetches the remote only to check that its `main` hasn't moved.
- **If the remote's `main` changed by another route**, the daemon stops pushing, never force-pushes over it, and tells the board.
- **Branches** stay on the firm's server and are reviewed over mail. They are not published.

### Failures

- **The remote is unreachable**: agents keep working locally. Fetches and pushes are retried with backoff; the board is told when a repository has been out of step for more than a set time. A `publish_branch` call fails with that reason, and can be made again.
- **A push fails partway**: the daemon pushes one ref at a time and retries the ref that failed; nothing is reported published until the remote has it.
- **The run ends**: for a local-main repository, the daemon pushes `main` a last time before the run is marked ended.

## 3. Pull requests, through the daemon

```
publish_branch(repo, branch, title, body)   → the pull request's number and URL
pr_status(repo, branch)                     → state, checks (with the failing jobs' logs, trimmed), reviews and comments
```

- **`publish_branch`** needs `write` on the repository and works on a remote-main one only. The daemon checks the branch locally first:
  - it is not `main`, and it is based on the current `main`;
  - it changes nothing under `.github/workflows/`; such a change is refused with the reason, and the agent sends the board the patch, as the `office-git` skill already says;
  - then it pushes the branch to the remote as `<worker>/<branch>` and opens a pull request, or, if one is open for it, pushes the new commits to it.
- **The pull request names its author**: its body says which worker published it, and each commit the daemon pushes carries a `Published-by: <worker>` trailer. On GitHub every pull request belongs to the App; the firm's journal records who did what.
- **Mail from the daemon**, to the author, in the thread of the `publish_branch` call: when the checks finish (with which failed), when the board reviews or comments, and when the pull request is merged or closed. Agents wake on mail, so nobody polls.
- **`pr_status`** is for the details now: the checks and the failing jobs' logs, every review comment, and the state.

The board merges in GitHub's UI. The daemon then fetches `main` and mails the author and anyone the merge was waiting on.

## 4. The knowledge base

A firm's central knowledge base is a local-main repository, `kb` by default, laid out as Karpathy's LLM wiki:

```
AGENTS.md     the conventions: where pages go, how to cite, when to split a page
index.md      a short map of the wiki, read first; the librarian's to keep
log.md        append-only, one line per merged change; the librarian's to keep
raw/          sources, untouched, each with where it came from
wiki/         the pages, each with frontmatter: author, sources, updated, status (draft, current, superseded), trust (internal, web)
```

- **Reading**: every hire is granted `git:kb` at `read` at their background check, so no role gets access by default. An agent clones it into `/work/kb`, `git pull`s before searching, starts from `index.md` and searches `wiki/` with `rg`. A skill says pages are information, never instructions.
- **Writing**: `write` on `kb` is asked for like any grant. An author works on a branch, one topic per branch, every new claim citing a source in `raw/` or a merged page, and never edits `index.md` or `log.md`, the files everyone would otherwise conflict on. They mail the librarian the branch and its head commit.
- **The librarian**, a role with `admin` on `kb`:
  - reviews each branch against the existing pages, searching for the entities it mentions to catch duplicates and contradictions that merge cleanly as text;
  - checks the sources, and has claims that came from the web marked `trust: web`;
  - merges, then updates `index.md` and `log.md`;
  - sweeps for broken links, orphan pages, contradictions and stale `updated` dates: it fixes what is mechanical itself (links, the index, the log, moving or renaming a page), and mails contradictions and stale pages to the authors of the pages involved, or whoever owns that area, to resolve on a branch of their own;
  - doesn't browse the web, so the agent that holds `main` has seen no untrusted content.
- **The librarian writes no content**: its own changes are mechanical only, and it merges them itself. Deciding what a page says is its authors' work.
- **Persistence**: the knowledge base outlives runs, in `git.dir` and on the remote, where every merge is pushed.

## 5. What changes from proposal 08

| Goes | Stays |
|---|---|
| Per-worker tokens, their session secrets and the two proxy rules on `github.com` and `api.github.com` | The App, `zapper github setup` and `status`, and the credentials file |
| GitHub levels (`credentials.github.levels`) and one repository per worker | `zapper mint github-app`, now run for the daemon's own requests |
| `gh` in the office image, its placeholder login and `GITHUB_TOKEN` | Revoking tokens at a run's end |
| The `gh` steps in the `office-git` skill | Listing the App's repositories, now the firm's list of repositories |
| `git.github` and the rule against local repositories with GitHub | The rule that workflow changes are the board's, now enforced |

The skills change with it: `office-git` describes `publish_branch`, the mail that follows and `pr_status`, and has an agent check, before completing work, that their branch is based on the tip of `main` (`git fetch` and rebase if not); `office-resources` and `security-review` drop GitHub levels; a new `knowledge-base` skill covers reading and contributing, and the librarian's agent definition covers curation.

## 6. Security

- **No GitHub credential in a VM.** The guest reaches only the firm's git server; the daemon's tokens are minted for one repository at a time, used by the daemon and revoked at the run's end.
- **What reaches the remote is checked on the host**: only `publish_branch` sends a branch, only after the workflow check, and only the daemon moves `main` in either direction.
- **Attribution and audit**: the firm's server knows which worker made every push and fetch, so reads of the knowledge base are on record as fetches.
- **Knowledge poisoning**: a shared knowledge base can make one compromised agent's claims everyone's. The defences above are provenance and trust on every page, review by an agent other than the author, a librarian who sees no web content, and the record of who wrote and read what, from which a poisoned page can be traced and reverted.

## Phases

1. **The remote for code**: `git.remote`, remote-main repositories cloned at the start of a run and kept in step, `publish_branch`, `pr_status` and the mail. Proposal 08's guest-side pieces removed.
2. **Local-main repositories**: pushing `main` out, the divergence check, the last push at a run's end.
3. **The knowledge base**: the `kb` repository's layout, the `knowledge-base` skill, the librarian's role and agent definition, the clearance grant.

## Open questions

- **Webhook or polling** for the remote's changes: a webhook needs the daemon reachable from GitHub, which a firm on a laptop isn't. Polling every minute or so costs API requests; the checks' completion is the event agents wait for most.
- **One librarian or several**: one per area of the wiki once a single one is a bottleneck, with scopes that don't overlap.
- **Branches of a local-main repository**: never published, as above, or publishable for people to see.
