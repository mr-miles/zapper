[← Examples](../README.md)

# A firm that researches

A question goes in, a pack of cited findings comes out. **Ada**, the CEO, gets a question from you and hires a **research manager**, who scopes it with web search and writes a research plan: the sub-questions, the sources for each, and the sites they are on. The research manager hires one or more **researchers**, each with a strand of the plan. Each asks for access to their strand's sites, writes a pack of Markdown documents with references, and suggests sources the plan missed, which the research manager approves or turns down. With more than one researcher, the research manager collates their packs into one, reviews it and reports to Ada, who reviews it and reports to you.

Then it's your call. If the research says the idea is worth trying, ask Ada for a prototype, and she commissions a **prototype engineer** (staff, or on contract for a bounded job), who scopes it from the pack, builds it in the firm's git repository and writes up what works, what doesn't and what a development team needs next.

- [zapper.yaml](zapper.yaml): a complete config: the `claude` profile and the firm `lab`.
- [task.md](task.md): the board's question for Ada, as a sample: what it would take to start a repair café (a monthly event where volunteers fix residents' broken things) in a small UK town, and what the evidence says about repair cafés' impact and why some last and others close. Any question will do; the first line is the subject.

The plan, the packs and the prototype's write-up are on the shared drive; the prototype's code is in the firm's repository, `prototypes`, which the Git tab shows. There is no cluster.

## How access works here

- **Searching needs none.** Claude Code's web search runs on Anthropic's side, through the Claude API the profile already allows, so the research manager can scope the question before anyone asks for anything.
- **Fetching a page does.** Every other host is asked about. The research plan lists the hosts each strand needs, and researchers ask for theirs at once, naming the plan. Ada holds security in a firm without officers, so she decides their requests, approving what the plan names and asking the research manager about anything it doesn't.
- **You decide Ada's own.** Her own requests and the background check on the research manager, her hire, come to you. The research manager's hires are checked by Ada.

## Before you start

You need a zapper set up as in the [README](../../README.md): `make deps`, `make install`, and the Claude Code image, `make image IMAGE=office`. You also need your Claude token in `~/.config/zapper/claude-token`; [config/zapper.yaml](../../config/zapper.yaml) says how. Then, from the repository's root:

```
zapper --config examples/research-firm/zapper.yaml check
```

Every line should be `ok` (or a `note`). With Ada, a research manager and two researchers the host needs 16 GiB of memory free; the prototype engineer needs 4 GiB more, or what the researchers free once Ada lets them go. The run uses your Claude subscription, on Sonnet: expect one to two hundred turns.

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
   - **Git**: the prototype, branch by branch.
   - **Messages**: the briefs, researchers' suggested sources and the research manager's answers, security asking the research manager about hosts the plan didn't name.
   - **Security**: the researchers' requests and Ada's decisions.
   - **The terminal**: `zapper firm journal lab -f` for everything, `zapper firm transcript lab rosa -f` for one worker.

5. Ada reports to you with the pack attached. `zapper drive ls lab` lists the drive, and `zapper drive get lab a-N ./out` copies the pack out.

6. To commission a prototype: closing your task paused the firm, so resume it, then give Ada the next task:

   ```
   zapper firm resume lab
   zapper mail send lab --to ada --kind assignment --subject "Prototype it" --body "Build a prototype of …, from the research pack: what it must show, by when."
   ```

   Ada may ask for more budget for it (`zapper firm budget lab approve b-2`), and the prototype engineer's background check comes to you as before. They mail Ada a scope first, then build; Ada reports with the write-up.

## Afterwards

```
zapper firm down lab      # stop the run; its journal, transcripts and drive stay
zapper firm replay lab RUN
```

To research something else, change [task.md](task.md): the question, the sub-questions you care about, and the size of the team you'll pay for.
