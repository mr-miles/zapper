# Real GitHub repositories through a GitHub App

A firm works in the GitHub repository the board names, and each worker reaches it with a short-lived token that the host mints from a GitHub App, scoped to what that worker's approved grant allows. The token lives in the host's secret store; the guest sees only a placeholder. This narrows [proposal 01](01-persistent-outputs.md)'s "real GitHub repos" section into something buildable, adds what it left open (setting the App up, naming the repository, permission profiles, expiry, changing a grant), and fixes a flaw in how it scoped tokens.

The minting code exists in Go in the `agentctl` repository (`internal/ghsetup`, `internal/ghtoken`). This proposal ports it to Rust inside zapper; nothing at runtime depends on `agentctl`.

## Today

- A firm with `git: {}` gets an empty bare repository per run, served by the daemon at `git.<domain>`. Access is a `git:<repo>` grant at level `read`, `write` or `admin`, checked on every request, with a `pre-receive` hook that keeps `main` for `admin` (`facility.rs`, `access.rs`, [office](../office.md#facilities-the-cluster-git-and-the-product)).
- Secrets are `Env | File | Value`, all static (`zapper-policy/src/config.rs`). The proxy injects a `proxy` secret into an inspected request that a rule listing it allows, to a host in the secret's `hosts`.
- Nothing in zapper talks to GitHub.

## Decisions

| | |
|---|---|
| Language | Rust, in zapper. `ring` (already a dependency) signs the App JWT (RS256, PKCS#1 key as GitHub issues it); the existing hyper + rustls client does the HTTP. No new crypto or HTTP dependency. |
| Who names the repository | The board, in the kickoff instructions (`owner/name`). zapper does not offer a list to pick from. It uses the App to **check** the name (is the App installed there?) and to suggest near matches on a typo. |
| Access model | **Permission profiles** replace `read`/`write`/`admin` for GitHub repositories. The drive and the cluster keep their levels. |
| Token lifetime | Installation tokens last one hour. The host refreshes them; the guest never knows. |
| Changing a grant | A token cannot be edited. Zapper mints a new one for the new permissions and retires the old one. |
| Token granularity | One token per **(worker, repository)**, not one per worker; see [Why not one superset token](#why-not-one-superset-token-per-worker). |

## 1. Setting the App up: `zapper github`

Ported from `agentctl gh-setup`, with installation added.

```
zapper github setup --org acme            # create the App if there isn't one, then make sure it is installed
zapper github setup --user me             # same, under a personal account
zapper github status [--org acme]         # App, installation, the permissions it has, the repositories it can see
```

`setup`:

1. **App present?** Look for `~/.config/zapper/github-apps/<owner>.yaml`. If it exists, skip creation (the App might already be made: reuse it, never overwrite; `--force` is not offered, because GitHub shows the private key once).
2. **Create** (no file): serve the manifest form on `localhost:<port>`, open the browser, take the redirect code (or a pasted URL, for a remote host), convert it with `POST /app-manifests/{code}/conversions`, and write the credentials file `0600`. Same flow and same checks as `ghsetup` (the owner GitHub reports must match `--org`/`--user`).
3. **Install**: with the App's JWT call `GET /orgs/{org}/installation` (`/users/{user}/installation`). If it is installed, report the installation id and its permissions and repository selection. If not, print `https://github.com/apps/<slug>/installations/new`, open it, and poll that endpoint until it appears or the user cancels.
4. **Report** which repositories the installation can reach (`GET /installation/repositories` with a throwaway installation token) and warn if it is installed on *all* repositories: choose "only selected repositories" so the App's reach is a deliberate ceiling.

The manifest asks for the permissions the profiles may need (the union, below). Anything missing from the installed App's permissions is reported by `status` and by config validation, so a profile that can never be minted fails early.

The credentials file keeps `agentctl`'s layout (`github.appId`, `clientId`, `appSlug`, `privateKey` or an absolute `privateKeyPath`; client and webhook secrets are not stored). It sits under the daemon user's config directory, which no VM mounts. The key is read only by the minting process (below) and is never logged.

## 2. The board names the repository

The firm's config says where repositories come from, not which:

```yaml
firms:
  acme:
    git:
      github: { app: acme }          # the owner's credentials file; the repositories come from the kickoff
```

`git: { repos: [...] }` (the internal server) and `git: { github: ... }` are mutually exclusive.

The kickoff names the repository: a `repos` field in the Kickoff tab and `zapper firm up acme --repo acme/api` (repeatable). The board may also just say it in the instructions to the CEO; since the grants need a canonical name, the CEO's first `credentials_request` for `git:acme/api` is validated the same way. Either way, zapper checks the name at the point it first appears:

- `GET /repos/{owner}/{name}/installation` must succeed (the App is installed there); otherwise the request is refused with "the App is not installed on acme/api" plus up to three near names from `GET /installation/repositories`.
- All of a firm's repositories must have one owner, since a token belongs to one installation (as in `agentctl`).
- The resource becomes `git:acme/api` (`resource_in` accepts `owner/name`; plain `git` still works when the firm has exactly one).

Nobody picks from a list of everything the App can see, so the CEO cannot widen a firm to a repository the board did not name; the App's installation is the outer limit, the firm's named repositories the next, the profile the next.

## 3. Permission profiles

```yaml
    credentials:
      github:
        profiles:
          read:        { contents: read, issues: read, pull_requests: read, actions: read, workflows: read }
          contributor: { contents: write, pull_requests: write, issues: write }
          maintainer:  { contents: write, pull_requests: write, issues: write, workflows: write, approver: ceo }
```

- A profile is a named map of GitHub App permission names to `read|write|admin`, as the token endpoint takes them. `metadata: read` is always added. Names and levels are checked at config load, and each profile must be within what the installed App has (`GET /app/installations/{id}` returns its `permissions`).
- `approver: ceo` (optional) says who may decide a request for the profile. It replaces the rule that `admin` on git is the CEO's: the profiles that can change the repository's workflows or administration are the ones to mark.
- `credentials_request(resource: "git:acme/api", profile: "contributor", reason)`; `credentials_decide` allows or denies that profile, or a lower one. Officers and the CEO see the profile's full permission list in the request, not just its name. The Security tab's matrix shows the profile per worker and repository.
- A worker holds at most one profile per repository. Asking for another is an **extension** (below).
- The policy owners hold a profile named by `credentials.github.owner_profile` (default: the broadest), as they hold admin today. Leaving the role revokes it.
- Profile names are a firm-level vocabulary the board and CEO already use in kickoff and mail; the `office-resources` and `office-git` skills describe them instead of `read`/`write`/`admin`.

`main` protection moves to GitHub. The internal server's `pre-receive` hook has no equivalent on github.com; the operator sets a branch ruleset on `main` (require a pull request, restrict who can push), and the profiles then separate "can push branches" from "can administer rulesets and workflows". `zapper github status` warns when a named repository's default branch is unprotected. The Level-based `office-git` rules ("pushing `main` needs admin") are rewritten around the ruleset: workers push branches and open pull requests; the merger is whoever holds a profile that can merge. This is a change of the product's behaviour, not just plumbing, and is called out in Open questions.

## 4. A grant mints a token and mounts it

### The minting source

Proposal 01's generic hook, as specified there, plus the GitHub minting as its first user:

```
SecretSource::Command { run: [String], timeout, ttl_fallback }   // the command prints {"value": "...", "expires_at": "<rfc3339>"} or a bare value
zapper mint github-app --app acme --repo acme/api --perm contents=write --perm pull_requests=write
```

- `zapper mint` is a thin wrapper over a library (`zapper-github`): load the credentials file, sign the JWT (`iat` backdated 60 s, `exp` 9 min), `GET /repos/{o}/{r}/installation`, `POST /app/installations/{id}/access_tokens {repositories, permissions}`, print `{value, expires_at}`. It is what `agentctl gh-token-helper` does, minus the on-disk cache (the daemon caches).
- The daemon runs it as the daemon user with a clean environment and a timeout; stdout is never logged; failure messages never contain the token (the `agentctl` rule). The arguments are composed by the daemon from the grant, never from guest input.
- The App's private key is read only by this process. The daemon itself never holds it, which keeps a compromised proxy from learning it.

### The per-grant secret

On approval, the office registers, through `Core`, a secret scoped to that worker's session and not part of the saved startup config (`config save` does not write it; it ends with the session):

```yaml
gh-<worker>-<owner>-<repo>:
  command: [zapper, mint, github-app, --app, acme, --repo, acme/api, --perm, contents=write, ...]
  mode: proxy
  hosts: [github.com, api.github.com]
```

and puts rules on the worker's layer (replacing `network_follows_grant`'s internal git rule), id `credentials-git-acme-api`:

- `github.com:443`, `path: /acme/api.git/**` and `/acme/api/**`, `set_headers: { Authorization: "Bearer {{secret:gh-…}}" }`;
- `api.github.com:443`, `path: /repos/acme/api/**`, same header.

Both rules inspect the connection (headers need it). The guest's git is configured with `http.https://github.com/.extraHeader = "Authorization: Bearer zapper-secret-gh-…"` so it sends the placeholder, and the proxy swaps in the real token. Git over HTTPS accepts an App token as a Bearer; if a git endpoint insists on Basic `x-access-token:<token>`, a base64 render helper is needed (the token would sit inside a base64 blob the substring matcher cannot see). That is the one thing to verify against real GitHub early.

A worker's rules name only that worker's secrets, and a secret is injected only where a rule of the sending session lists it, so one worker cannot spend another's token. GraphQL (`/graphql`) cannot be restricted to a repository by path and is not allowed; REST under `/repos/{o}/{r}/` and git smart-HTTP are.

### Expiry: the host refreshes, the guest never notices

Tokens last an hour; grants last `credentials.expiry` (24 h by default). They are independent clocks.

- The daemon caches the command's `value` with its `expires_at`, and re-runs the command when less than a **refresh margin** remains (default 10 minutes). Refresh is lazy (on the next request that needs the secret) and single-flight, so a burst of git requests mints once. It is not on a timer, so an idle worker mints nothing.
- The margin exceeds the longest single request the proxy will carry, so a token does not lapse in the middle of a push. A request that outlasts its token fails; the retry gets a fresh one. This is the same reasoning as `agentctl`'s "refresh + 5 minutes".
- If a refresh fails but the cached token still has life, the request proceeds with it and a `credentials` event records the failure. If the token has lapsed, the request fails closed with the secret's name in `zapper events` and the audit log (existing behaviour for a missing secret), and the worker's git reports a 401 or 403 that the `office-git` skill tells it to treat as "ask for the grant to be renewed, or check with security."
- The grant is the authority: when it expires or is revoked the rule and the secret are removed, so no new token is minted, and the token already issued lives out at most one hour. Revocation also tells GitHub: `DELETE /installation/token` authenticated with that token, which the host holds. A grant revoked is a token revoked, not a token left to lapse.
- The App's key being rotated or the installation removed shows up as every mint failing; the board is notified once per firm, not per worker.

### Extending or changing a grant

GitHub cannot widen or narrow an existing token. When a worker's profile on a repository changes (an approved extension, a renewal at a new level, or a partial revoke):

1. Compute the permissions of the new profile.
2. Mint a new token for it (a new command line, so a new cache entry).
3. Replace the secret behind the same name, so the rule and the guest's placeholder do not change; in-flight requests finish on the old token.
4. Revoke the old token at GitHub (`DELETE /installation/token`).

An extension is a single approval of "profile B instead of A", where B must include everything A had (an extension never silently narrows); narrowing is a revocation plus a new grant.

### Why not one superset token per worker

Proposal 01 suggested minting one token per worker scoped to "the repos and permission level its grant allows". With several repositories and profiles that is wrong in one direction: a token has one `repositories` list and one `permissions` map applied to **all** of them. A worker with `contributor` on `acme/api` and `read` on `acme/web`, given one token for both, would hold `contributor` on `acme/web` as well. The superset the token carries is the union, and the union grants more than any single approval did.

So the unit is the **(worker, repository)** pair: a worker with grants on two repositories gets two tokens and two secrets, each exactly its profile. "A completely new token containing the superset" applies per repository, when the profile on that repository is extended. An optimisation merges repositories into one token only when their profiles are identical. If you would rather have one token per worker and accept the union, say so; the rest is unchanged.

## 5. What changes in the code

| Where | Change |
|---|---|
| `zapper-github` (new crate) | JWT, installation lookup, token mint and revoke, repository check/list, manifest flow, credentials file. Tests against a fake GitHub (`httptest`-style, as in `agentctl`'s tests). |
| `zapper` CLI | `zapper github setup\|status`, `zapper mint github-app` |
| `zapper-policy` config | `SecretSource::Command`; profile and `git.github` config in the firm schema (`firm.rs`) with validation |
| `zapper-core` proxy | resolve a `Command` secret with the cache, margin, single-flight and fail-closed rules; session-scoped secrets registered and dropped with the session |
| `zapper-office` `access.rs` | profiles instead of `Level` for `git:`; extension; per-(worker, repo) secrets; revoke at GitHub; `resource_in` accepts `owner/name`; `approver` replaces "admin is the CEO's" |
| `facility.rs` | skip bare repositories, `pre-receive` hook, per-worker git passwords and the `git.<domain>` service for GitHub firms; `credentials_for` writes the `extraHeader`; services advertise `https://github.com/<owner>/<repo>.git` |
| Kickoff UI, `firm up` | `repos` / `--repo`, checked against the installation |
| Office UI Git tab, `git.rs` | needs a new source: a host-side mirror clone, or the GitHub API |
| Skills and prompts | `office-resources`, `office-git`, `security-review`: profiles, branch + pull-request flow, 401 handling |
| Docs | `docs/network.md` and `docs/security.md`: `github.com` is resolved on the host, leaving the "firm services are never resolved on the host" model for these firms |

## 6. Security

- The App's private key is read by one short-lived process; the daemon holds only tokens, in memory. Tokens never reach the guest, a log, or the journal (the journal records the secret's name, profile, repository and expiry).
- Reach is the intersection of the installation (the operator's choice of repositories), the board's named repositories, the grant's profile, and the rule's path. Each can only narrow the one before.
- The proxy invariants stand: a secret is injected only into TLS requests to its `hosts`, by a rule that lists it, in the sending session.
- Attribution: GitHub's own logs show the App, not the worker. zapper's record stays authoritative for who did what (the one limitation proposal 01 already accepted).
- `repository` and `permission` strings that come from a worker's request are matched against the config's profiles and the named repositories before they reach a command line; nothing a worker types is passed to `zapper mint` unchecked.

## Phases

1. **`zapper-github` + `zapper github` + `zapper mint`**: against a fake GitHub, then a real test App. Verify git over HTTPS with a Bearer token and `gh`/REST through the proxy first, since everything depends on it.
2. **`SecretSource::Command`** with cache, margin and fail-closed behaviour, tested with fake commands (expiring tokens, failing refresh, burst of requests).
3. **Firm config, profiles, kickoff repository check; grants mint and mount tokens; extension and revocation.** Retire the internal-git paths for GitHub firms.
4. **UI and skills**: kickoff field, profile display in the Security tab, rewritten `office-git`.
5. **Git tab history** from a mirror.

## Open questions

- **`main` protection.** Without the `pre-receive` hook, "only the merger pushes `main`" rests on GitHub rulesets. Do you want zapper to verify and require one (refuse a firm whose default branch is unprotected), or only warn?
- **One token per (worker, repository) or per worker.** See above; the per-pair design is safer, the per-worker one simpler.
- **Several owners.** A firm with repositories under two owners would need two installations and two sets of tokens; this proposal refuses it, as `agentctl` does. Is that acceptable?
- **GraphQL.** Blocked here. `gh pr create` and similar use it; workers may need REST-only flows or an explicit, wider rule for that host.
