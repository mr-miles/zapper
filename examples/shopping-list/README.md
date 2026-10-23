[← Examples](../README.md)

# A company that builds software

A firm that grows to fit its task: **Ada**, the CEO, starts alone. She hires a **product manager** to write the specification of a shopping list app, then a small **engineering team** to build it from that specification in the firm's git repository: a Node.js backend with a JSON API and a React frontend, the list kept in memory. Each of them is Claude Code in a VM of their own. You are the board: you give Ada the task, approve her budget and her hires, and get her report.

- [zapper.yaml](zapper.yaml): a complete config: the `claude` profile, `claude-engineer` (the same, with npm allowed), and the firm `shop` with its git repository.
- [task.md](task.md): the board's task for Ada. The first line is the subject. It ends with what done means: all the code on main in git, a fresh clone that builds, every test passing, a test for each of the spec's acceptance criteria, and every change reviewed by the other engineer for repetition and encapsulation.

There is no cluster: the engineers run and test the app in their own VMs, and you read the code in the UI's Git tab, or clone it and run it yourself.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office`. You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how. Then, from the repository's root:

```
zapper --config examples/shopping-list/zapper.yaml check
```

Every line should be `ok` (or a `note`).

Each worker's VM takes 4 GiB, so with Ada, a product manager and two engineers the host needs 16 GiB free; each hire is refused with the numbers if it doesn't fit. The run uses your Claude subscription, on Sonnet: expect a few hundred turns, and an hour or two. At most three workers take turns at once (`budget.max_concurrent`).

## Run it

1. Start the daemon with this config. Stop any other `zapper serve` first: they share the socket and the state directory.

   ```
   zapper --config examples/shopping-list/zapper.yaml serve
   ```

   It prints the UI's address. Open it.

2. Start the firm with the task, in another terminal:

   ```
   zapper firm up shop --task examples/shopping-list/task.md
   ```

   Ada's VM boots (about half a minute), and the task arrives in her inbox as an assignment from the board.

3. Answer Ada, as the board. Ada holds security in a firm without officers, and nobody decides their own requests or checks their own hires, so these come to you:

   - **Her budget request** (`b-1`). Ada sizes it with `budget()`; approve it in the Kickoff tab, or decline with a note to have her revise it:

     ```
     zapper firm budget shop approve b-1
     ```

   - **A background check for each hire**: the product manager first, the engineers once the spec is done. Each hire's VM boots, but they take no turn until you approve. Approve or object in the Security tab, or:

     ```
     zapper firm screen shop c-1 --approve --note "ok"
     ```

   - **Ada's own access requests**, if she makes any (`zapper firm security shop` lists what waits, `zapper firm decide shop r-1 --allow` answers).

   Ada decides everyone else's: the product manager and the engineers ask her for write access to the repository, and the engineers may ask for a site beyond npm.

4. Watch them work:

   - **The UI**: the Organization tab shows the chart growing as Ada hires, and mail flying between them; Messages has the threads (the brief, the spec's review, the engineers' questions to the product manager); Security the access requests and Ada's decisions; Git the repository as it fills: SPEC.md first, then the code, branch by branch; each worker's panel their transcript.
   - **The terminal**:

     ```
     zapper firm journal shop -f            # everything: hires, mail, grants, pushes
     zapper firm status shop                # who is working, idle or waiting
     zapper mail ls shop                    # your inbox, as the board
     ```

5. Ada reports to you when the app is done, having checked the definition of done herself: a fresh clone, the build, the tests, and the spec's criteria against them.

## Try the app

The repository is on this machine, in the run's directory. To clone it (`zapper firm runs shop` lists the runs):

```
git clone ~/.local/state/zapper/firms/shop/runs/RUN/git/product.git shopping-list
cd shopping-list
```

Then follow its README; it should be `npm install` and a start command or two.

## Afterwards

```
zapper firm down shop     # stop the run; its journal, transcripts, drive and repository stay
zapper firm replay shop RUN
```

`replay` prints the UI address that plays the run back, the Git tab's commits following the timeline. To see the team carry on, give Ada a second task before you stop, like persisting the list to a file. [docs/office.md](../../docs/office.md) has the rest.
