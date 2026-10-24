[← Examples](../README.md)

# A firm that researches

A question goes in, a pack of cited findings comes out. **Ada**, the CEO, gets a question from you and hires a **research manager**, who scopes it with web search and writes a research plan: the sub-questions, the sources for each, and the sites they are on. The research manager hires one or more **researchers**, each with a strand of the plan. Each asks for access to their strand's sites, writes a pack of Markdown documents with references, and suggests sources the plan missed, which the research manager approves or turns down. With more than one researcher, the research manager collates their packs into one, reviews it and reports to Ada, who reviews it and reports to you.

- [zapper.yaml](zapper.yaml): a complete config: the `claude` profile and the firm `lab`.
- [task.md](task.md): the board's question for Ada, as a sample: what it would take to start a repair café (a monthly event where volunteers fix residents' broken things) in a small UK town, and what the evidence says about repair cafés' impact and why some last and others close. Any question will do; the first line is the subject.

There is no git repository and no cluster: the plan and the packs are on the shared drive.

## How access works here

- **Searching needs none.** Claude Code's web search runs on Anthropic's side, through the Claude API the profile already allows, so the research manager can scope the question before anyone asks for anything.
- **Fetching a page does.** Every other host is asked about. The research plan lists the hosts each strand needs, and researchers ask for theirs at once, naming the plan. Ada holds security in a firm without officers, so she decides their requests, approving what the plan names and asking the research manager about anything it doesn't.
- **You decide Ada's own.** Her own requests and the background check on the research manager, her hire, come to you. The research manager's hires are checked by Ada.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office`. You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how. Then, from the repository's root:

```
zapper --config examples/research-firm/zapper.yaml check
```

Every line should be `ok` (or a `note`). With Ada, a research manager and two researchers the host needs 16 GiB of memory free. The run uses your Claude subscription, on Sonnet: expect one to two hundred turns.

## Run it

1. Start the daemon with this config. Stop any other `zapper serve` first.

   ```
   zapper --config examples/research-firm/zapper.yaml serve
   ```

2. Start the firm with the question, in another terminal:

   ```
   zapper firm up lab --task examples/research-firm/task.md
   ```

3. Answer Ada, as the board:

   - **Her budget request** (`b-1`), in the Kickoff tab, or `zapper firm budget lab approve b-1`.
   - **The research manager's background check**: approve it in the Security tab, or `zapper firm screen lab c-1 --approve --note "ok"`.
   - **Ada's own access requests**, if she makes any: `zapper firm security lab` lists what waits.

4. Watch them work:

   - **Shared drive**: the research plan, then each researcher's pack, then the collated one, with their versions as they are reviewed.
   - **Messages**: the briefs, researchers' suggested sources and the research manager's answers, security asking the research manager about hosts the plan didn't name.
   - **Security**: the researchers' requests and Ada's decisions.
   - **The terminal**: `zapper firm journal lab -f` for everything, `zapper firm transcript lab rosa -f` for one worker.

5. Ada reports to you with the pack attached. `zapper drive ls lab` lists the drive, and `zapper drive get lab a-N ./out` copies the pack out.

## Afterwards

```
zapper firm down lab      # stop the run; its journal, transcripts and drive stay
zapper firm replay lab RUN
```

To research something else, change [task.md](task.md): the question, the sub-questions you care about, and the size of the team you'll pay for.
