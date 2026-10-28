[← Examples](../README.md)

# A firm on GitHub

The [shopping list](../shopping-list/README.md) company, working in a repository of yours on GitHub instead of a repository of its own. **Ada**, the CEO, starts alone, hires a product manager to write a spec, then engineers to build a Node and React app from it. Everyone is Claude Code in a VM of their own, and nobody holds a GitHub credential: the host mints each worker a token from your GitHub App, for one repository at one level, and puts it into their requests to GitHub. All work goes through pull requests, whose checks the engineers watch with `gh`; you, the board, merge them.

- [zapper.yaml](zapper.yaml): a complete config, with the firm `octo` and its two GitHub levels, `read` and `write`.
- [task.md](task.md): the board's task for Ada. The first line is the subject.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office` (it has `gh`). You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how.

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

Then the App, from the repository's root on the zapper host:

```
zapper github setup --org <account>        # or --user <your login>
```

It opens GitHub to create the App (or finds the one there is) and to install it: install it on the `shopping-list` repository only, since the firm works in every repository the App reaches. It writes the App's credentials to `~/.config/zapper/github-apps/<account>.yaml`. Put that file's name in [zapper.yaml](zapper.yaml), in place of `acme.yaml`:

```yaml
    git:
      github: { config: <account>.yaml }
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

   The run asks GitHub for the App's account and the repositories it reaches; if it reaches none, or the App is installed on more than one account, it stops and says so. Ada's VM boots and the task arrives in her inbox.

3. Answer Ada, as the board, in the UI or the terminal:

   - **Her budget request**: `zapper firm budget octo approve b-1`, or the Kickoff tab.
   - **Background checks** of her hires, which come to you because she holds security: `zapper firm screen octo c-1 --approve --note "ok"`, or the Security tab.
   - **GitHub access**: each hire asks for the repository at `write`; Ada decides those. She holds `read` on it herself from the start, and nothing more.

4. **Merge the pull requests**, on GitHub, when the author mails you that one is reviewed and its checks pass. A workflow change comes to you by mail as a patch: apply it yourself if you agree.

5. Watch them work: the Organization, Messages and Security tabs show the people, their mail and who holds which level; the pull requests are on GitHub. There is no Git tab, since the firm has no repository of its own. In the terminal, `zapper firm journal octo -f` shows everything, including each token's minting.

## Afterwards

```
zapper firm down octo     # stop the run; its journal, transcripts and drive stay; the code is on GitHub
```

A worker leaving, or losing their grant, has their token revoked at GitHub, and so does every token when the run stops. [docs/office.md](../../docs/office.md) has the rest: a firm on GitHub is under "Facilities".
