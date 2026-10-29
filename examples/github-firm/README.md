[← Examples](../README.md)

# A firm on GitHub

The [shopping list](../shopping-list/README.md) company, working in a repository of yours on GitHub. **Ada**, the CEO, starts with **Lin**, the librarian, hires a product manager to write a spec, then engineers to build a Node and React app from it. What the team learns goes into the firm's knowledge base, a markdown wiki in a second repository, `kb`, which Lin curates. Everyone is Claude Code in a VM of their own, working only with the firm's own git server: nobody in the VMs holds a GitHub credential. The daemon keeps that server in step with GitHub through your GitHub App, turns the branches the engineers publish into pull requests, and mails them their checks and reviews; you, the board, merge on GitHub.

- [zapper.yaml](zapper.yaml): a complete config, with the firm `octo`, whose repositories are on GitHub and kept in `~/.local/share/zapper/repos/octo`, and whose knowledge base `kb` has its `main` on the firm's server (`git.local`).
- [task.md](task.md): the board's task for Ada. The first line is the subject.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office`. You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how.

Then, on GitHub, in an account of yours (an organisation, or your own login):

1. **A repository for the app**, say `shopping-list`, with a README so it has a `main`.
2. **A workflow that tests pull requests**, committed by you, since workflows are the board's: `.github/workflows/test.yml`:

   ```yaml
   name: test
   on: [pull_request]
   jobs:
     test:
       runs-on: ubuntu-latest
       steps:
         - uses: actions/checkout@v4
         - uses: actions/setup-node@v4
           with: { node-version: 22 }
         - run: if [ -f package.json ]; then npm ci && npm test; fi
   ```

3. **`main` protected**: in the repository's settings, a branch ruleset on `main` that requires a pull request and blocks force pushes, so only a merged pull request changes it.
4. **A repository for the knowledge base**, `kb`, left empty: the daemon gives it its layout (`AGENTS.md`, `index.md`, `log.md`, `wiki/`) when it is first used. Its `main` is merged on the firm's server by Lin and pushed out by the daemon, so protect it with a ruleset that lets only your App update `main` (add the App to the ruleset's bypass list); GitHub's copy is for you to read.

Then the App, from the repository's root on the zapper host:

```
zapper github setup --org <account>        # or --user <your login>
```

It opens GitHub to create the App (or finds the one there is) and to install it: install it on the `shopping-list` and `kb` repositories only, since the firm's repositories are every one the App is installed on. It writes the App's credentials to `~/.config/zapper/github-apps/<account>.yaml`. Put that file's name in [zapper.yaml](zapper.yaml), in place of `acme.yaml`:

```yaml
    git:
      remote: { github: { config: <account>.yaml } }
```

Check it:

```
zapper github status --org <account>       # its installation, permissions and repositories
zapper --config examples/github-firm/zapper.yaml check
```

## Run it

1. Start the daemon with this config. Stop any other `zapper serve` first: they share the socket and the state directory.

   ```
   zapper --config examples/github-firm/zapper.yaml serve
   ```

2. Start the firm with the task, in another terminal:

   ```
   zapper firm up octo --task examples/github-firm/task.md
   ```

   The run asks GitHub for the App's account and the repositories it is installed on (if it is installed on more than one account, it stops and says so), and clones each into `git.dir` when someone first uses it in the run, or fetches it if it is there from an earlier run. Ada's VM boots and the task arrives in her inbox.

3. Answer Ada, as the board, in the UI or the terminal:

   - **Her budget request**: `zapper firm budget octo approve b-1`, or the Kickoff tab.
   - **Background checks** of her hires, which come to you because she holds security: `zapper firm screen octo c-1 --approve --note "ok"`, or the Security tab.
   - **Git access**: each hire asks for `git:shopping-list` at `write`, to push branches to the firm's server and publish them; Ada decides those. Nobody pushes `main`: it comes from GitHub.
   - **The knowledge base**: every hire reads `kb` from being cleared, and asks Ada for `write` to contribute. Lin asks for `merge` on `kb`, which lets her merge others' branches and nothing else; that goes to Ada as CEO. A contribution is a branch Lin reviews and merges; she writes nothing in it herself, so every page has a second pair of eyes.

4. **Merge the pull requests**, on GitHub, when the author mails you that one is reviewed and its checks pass. The daemon fetches `main` back and tells the author. A workflow change comes to you by mail as a patch, since a branch that changes `.github/workflows/` can't be published: apply it yourself if you agree.

5. Watch them work: the Organization, Messages and Security tabs show the people, their mail and their grants; the Git tab shows the firm's server's copy of the repositories and the commits of this run, the knowledge base's pages among them; the pull requests are on GitHub, and so is a copy of `kb`. In the terminal, `zapper firm journal octo -f` shows everything, including what the daemon fetched, pushed and published.

## Afterwards

```
zapper firm down octo     # stop the run; its journal, transcripts and drive stay; the code is on GitHub
```

The repositories stay in `git.dir` for the next run, which fetches what changed on GitHub, and the knowledge base with them: the next company starts from what this one recorded; the daemon's tokens are revoked at GitHub when the run stops. [docs/office.md](../../docs/office.md) has the rest: a firm on GitHub is under "Facilities".
