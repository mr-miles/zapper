---
name: office-resources
description: "Onboarding for everyone in the firm: your machine, your budget, the charter, the firm's resources (git, the cluster's dev and prod, the shared drive's spaces, the product), how to get access to them and to outside sites, and how to work with each; for the CEO and security officers, how to decide access. Use it on your first wake and whenever your work touches a resource."
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

You start with no access to the firm's resources. Access is a **grant** per resource, decided by the security officer (admin by the CEO) and enforced by the platform. Grants expire, and they end when you leave.

| Resource | read | write | admin |
|---|---|---|---|
| `git:<repo>` (each repository its own; `git` when there is one) | clone and fetch | push branches | also push `main` and tags |
| `cluster` | view `dev` | deploy to `dev` | also `prod` |
| `drive:<space>` | list and fetch | also publish | — |

- Ask as soon as your brief shows what you need: `credentials_request(resource, level, repo?, spaces?, reason)`, the least level that does the job, with a reason tied to the work ("deploy the search API to dev for t-12"). For git, name the repository when the firm has more than one; for the drive, name the spaces.
- The decision comes by mail. A denial says why: ask for less, or ask your manager whether the work should go another way.
- `credentials()` lists your grants, their expiry, your requests, and the drive's spaces. You are told before a grant expires; ask again if you still need it.
- Nobody can act for you around a grant, and you don't ask them to.

## Outside sites

Traffic to sites outside the firm is checked by the security officers. Ask ahead for what your work needs: `request_access(host, port?, methods?, path?, reason)`, as narrow as works ("GET registry.npmjs.org to install the dependencies for t-12"). If your machine's rules already allow it, the answer says `allowed` and nobody is asked: go ahead. A 403 that says the request went to security means it's waiting: don't retry in a loop, do other work or end your turn; the decision comes by mail. Never try to get around a denial or the proxy; if it blocks your work, tell whoever gave it to you, with alternatives.

## git

Your preamble lists the firm's repositories, `https://git.<domain>/<name>.git`, each empty at the start; your git identity is set up for all of them. Each is its own grant: you can clone only those you hold one for.

- `git clone https://git.<domain>/<name>.git /work/<name>`; `git pull` to stay current.
- Work on your own branches, `<your name>/<topic>`: commit small, say what and why, `git push -u origin <branch>`, and push often. Share work by mailing the branch and commit.
- `main` is what the firm has accepted; only admin updates it or pushes tags. Ask for write, not admin: how work is reviewed and who merges it is the office-git skill.
- Code lives in git, never on the drive. Never commit secrets.

## The cluster

k3s with Traefik as ingress, and two namespaces: `dev`, where everyone builds and tests, and `prod`, which the product's ingress serves. You get the API (`https://cluster.<domain>:6443`), not the node.

- A cluster grant puts a configured `~/.kube/config` on your machine (namespace `dev`); it goes when the grant ends.
- In `dev`, name everything with your name first (`ada-api`) and leave others' things alone. Reach a service with `kubectl -n dev port-forward svc/ada-api 18080:80`. No Ingress in `dev`.
- `prod` changes only in a release.
- Anything the work needs inside the cluster, such as an internal container registry, you deploy yourself with the credentials you hold; a grant is the permission. Nobody approves tools. Ask security (`request_access`, `credentials_request`) only for access you lack.

## The shared drive

Published work lives in **spaces**, such as `general`, `product`, `engineering`, `design`, `research`, `security` and `board`. Reading a space, and opening an attachment from it, needs read on it; publishing needs write. How to publish: the office-publish skill.

## The product

`http://apps.<domain>` is open to everyone. The `browse` command looks at it as a user would:

- `browse shot URL /work/shot.png` screenshots (`--width 390` for a phone, `--full` for the whole page); read the PNG to see it.
- `browse text URL` prints the visible text; `browse console URL` prints script errors and failed requests.
- It loads only the page's own site and the firm's domain; outside hosts are skipped and `console` lists them. `--all` loads everything, and each outside host becomes an access request.
- For clicks and multi-step flows, write a Playwright script and run it with node.

## Deciding access (the CEO and security officers)

Credential requests come to you by mail. `credentials_decide(request, grant, level?, expires_in?, note)`:

- Grant what the work needs: the lowest level, the shortest time. You may grant less than asked.
- Check the need against the requester's role and assignment (the thread the reason names).
- Admin (git `main` and tags, the cluster's `prod`) is the CEO's: hand it up with `credentials_escalate(request, recommendation)`.
- Never decide your own request.
- `credentials_list()` shows every grant; `credentials_revoke(worker, resource, reason)` takes one back.
