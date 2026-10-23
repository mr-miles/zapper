[← Examples](../README.md)

# A CEO and a contractor

The smallest firm that shows The Office at work: **Ada**, the CEO, starts alone and contracts **Ben**, a writer, for one job; each is Claude Code in a VM of their own. You are the board. You give Ada a task and approve her budget; she offers Ben the job, agrees his quote, reviews his draft, asks for changes, publishes the result to the shared drive and reports back to you. No git repository and no cluster, so it starts in a minute and takes a few dozen turns.

- [zapper.yaml](zapper.yaml): a complete config, the main one cut down to the `claude` profile and the firm `mini`.
- [task.md](task.md): the board's task for Ada. The first line is the subject.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office`. You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how. Then, from the repository's root:

```
zapper --config examples/mini-firm/zapper.yaml check
```

Every line should be `ok` (or a `note`). The run uses your Claude subscription: expect twenty to thirty-five turns on Sonnet. Its budget is the firm's own accounting in tokens, not money: it caps what Ada and Ben may spend.

## Run it

1. Start the daemon with this config. Stop any other `zapper serve` first: they share the socket and the state directory.

   ```
   zapper --config examples/mini-firm/zapper.yaml serve
   ```

   It prints the UI's address. Open it.

2. Start the firm with the task, in another terminal:

   ```
   zapper firm up mini --task examples/mini-firm/task.md
   ```

   Ada's VM boots (about half a minute), and the task arrives in her inbox as an assignment from the board.

3. Answer Ada, as the board. You make two decisions, in the UI or the terminal:

   - **Her budget request.** Ada asks for a budget (`b-1`) before she can accept a quote. Approve it in the Kickoff tab, or:

     ```
     zapper firm budget mini approve b-1
     ```

   - **Ben's background check.** Ada's offer boots Ben's VM, and like every hire he takes no turn until he is cleared. Ada holds security in a firm without officers, and nobody checks their own hire, so the check comes to you. Clear it in the Security tab, or:

     ```
     zapper firm screen mini c-1 --clear --note "ok"
     ```

   Then Ben quotes, Ada accepts (or counters), and the job starts. `zapper firm contracts mini` shows where it stands.

4. Watch them work:

   - **The UI**: the Organization tab shows Ada, then Ben under her with a dashed outline (pale while they negotiate), what each is doing, and envelopes flying between them as they mail. Messages has the threads, People the contract's steps, Shared drive the published note, and each worker's panel their transcript.
   - **The terminal**:

     ```
     zapper firm journal mini -f            # everything: mail, turns, states, publishes
     zapper firm transcript mini ben -f     # what Ben's Claude Code is doing
     zapper mail ls mini                    # your inbox, as the board
     zapper firm status mini                # who is working, idle or waiting
     zapper firm contracts mini             # offer, quote, acceptance, completion
     ```

5. When Ben completes the contract, his VM stops and the unspent budget returns to Ada. Ada reports to you when the note is published. `zapper drive ls mini` lists it, and `zapper drive get mini a-1 ./out` copies it out.

If either of them tries a website, the request waits for a decision. Ada decides Ben's requests, and you decide hers: `zapper firm security mini` lists them, and `zapper firm decide mini r-1 --deny` answers. The task says they won't need any.

## Afterwards

```
zapper firm down mini     # stop the run; its journal, transcripts and drive stay
zapper firm runs mini     # past runs
zapper firm replay mini RUN
```

`replay` prints the UI address that plays a run back. Answer Ada's report with `zapper mail send mini --to ada --kind reply --reply-to m-N --body "Thanks"` to see her pick it up, or give her another task before you stop.

To make it bigger, add roles to Ada's `hires`, or ask her to hire staff instead of contracting. [docs/office.md](../../docs/office.md) has the rest: contractors are under "The workforce".
