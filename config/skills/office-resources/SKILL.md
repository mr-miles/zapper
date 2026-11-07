---
name: office-resources
description: "Onboarding for everyone in the firm: your machine, your budget, the charter, the firm's resources (git, the cluster's dev and prod, the shared drive's spaces, the product and the browser), and how to get access to them and to outside sites. Use it on your first wake, and whenever you need access to something or use git, the cluster, the drive or the browser."
---

# The firm and its resources

## Your machine

You work on your own machine with git, kubectl, podman, Node.js, Python and a headless browser. `/work` is your disk: nobody else sees it, and it is gone when you leave or the run ends. What others can see is only what you mail, publish to the shared drive, or push to git.

## Your budget

Every turn you take spends tokens from your own balance, shown at each wake (`budget()` for detail). When it runs out you stop until your manager funds you, so spend it on the work.

## The rules

- The charter in your instructions is the company's law; nobody in the firm can change it.
- Your colleagues are the firm's internal agents, not public people: their names, and the board's, never go into anything that leaves the firm (the product and its pages, sample data, credits, posts, outside sites, public repositories). Inside the firm, use names as usual.

## Getting access

You start with no access to the firm's resources. Access is a **grant** per resource, decided by the security officer (merge and admin by the CEO) and enforced by the platform. Grants expire, and they end when you leave.

| Resource | read | write | merge | admin |
|---|---|---|---|---|
| `git:<repo>` (each repository its own; `git` when there is one) | clone and fetch | push branches | merge someone else's branch into `main`, and push nothing else | also push `main` and tags |
| `cluster` | view `dev` | deploy to `dev` | — | also `prod` |
| `drive:<space>` | list and fetch | also publish | — | — |

- Ask as soon as your brief shows what you need: `credentials_request(resource, level, repo?, spaces?, reason)`, the least level that does the job, with a reason tied to the work ("deploy the search API to dev for t-12"). For git, name the repository when the firm has more than one; for the drive, name the spaces.
- The decision comes by mail. A denial says why: ask for less, or ask your manager whether the work should go another way.
- `credentials()` lists your grants, their expiry, your requests, and the drive's spaces. You are told before a grant expires; ask again if you still need it.
- Nobody can act for you around a grant, and you don't ask them to.

## Outside sites

Traffic to sites outside the firm is checked by the security officers, and a request your machine's rules don't allow is refused at once. Don't retry it in a loop: ask, then do other work or end your turn; the decision comes by mail.

- If the refusal answers `policy_denied`, ask by proposing the narrowest rule that would allow it through `http://policy.local`, as `/etc/openshell/skills/policy_advisor.md` explains (read it the first time), with an `intent_summary` that names the work ("GET registry.npmjs.org to install the dependencies for t-12"). The security officers decide it, not the sandbox: you can wait on `/v1/proposals/{chunk_id}/wait?timeout=300` while you have nothing else to do. A rejection carries the officer's reason: ask for less, or not at all. Never propose wildcard hosts or `tls: skip`.
- If the refusal is a 403 that says the request went to security, it already waits for an officer: there is nothing more to ask.

Never try to get around a denial or the proxy; if it blocks your work, tell whoever gave it to you, with alternatives.

## git

Your preamble lists the firm's repositories and their URLs (`https://git.<domain>/<name>.git`, or the office's own address on some hosts); your git identity is set up for all of them. Each is its own grant: you can clone only those you hold one for.

- `git clone <url> /work/<name>`, with the URL from your preamble; `git pull` to stay current.
- Ask for write, not merge or admin. Branches, review and who merges to `main`: the office-git skill.
- Code lives in git, never on the drive.

If your preamble says a repository's `main` is merged by the board on the firm's remote, you still clone and push only the firm's server, and nobody pushes `main`: send a branch for review with `publish_branch`, and never change `.github/workflows/` (the office-git skill). The repositories then aren't empty at the start: they are the remote's, kept across runs.

## The cluster

k3s with Traefik as ingress, and two namespaces: `dev`, where everyone builds and tests, and `prod`, which the product's ingress serves. You get the API (your preamble says where: `https://cluster.<domain>:6443`, or the office's own address `…/cluster` on some hosts), not the node.

- A cluster grant puts a configured `~/.kube/config` on your machine (namespace `dev`); it goes when the grant ends.
- In `dev`, name everything with your name first (`ada-api`) and leave others' things alone. Reach a service with `kubectl -n dev port-forward svc/ada-api 18080:80` where the API is the cluster's own; through the office's address, `port-forward`, `exec` and `attach` aren't served: test in `dev` with a Job or a pod's logs instead. No Ingress in `dev`.
- `prod` changes only in a release.
- Anything the work needs inside the cluster, such as an internal container registry, you deploy yourself with the credentials you hold; a grant is the permission. Nobody approves tools. Ask security (a rule through `policy.local`, or `credentials_request`) only for access you lack.

## The shared drive

Published work lives in **spaces**, such as `general`, `product`, `engineering`, `design`, `research`, `security` and `board`. Reading a space, and opening an attachment from it, needs read on it; publishing needs write. How to publish: the office-publish skill.

## The product

`http://apps.<domain>` is open to everyone; where your preamble gives the product's address as the office's (`…/apps/`), use that wherever the skills say `http://apps.<domain>`. The `browse` command looks at it as a user would:

- `browse shot URL /work/shot.png` screenshots (`--width 390` for a phone, `--full` for the whole page); read the PNG to see it.
- `browse text URL` prints the visible text; `browse console URL` prints script errors and failed requests.
- It loads only the page's own site and the firm's domain; outside hosts are skipped and `console` lists them. `--all` loads everything, and each outside host becomes an access request.
- For clicks and multi-step flows, write a Playwright script and run it with node.
