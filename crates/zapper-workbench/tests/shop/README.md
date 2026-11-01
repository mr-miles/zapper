# The shop, as a workbench repository

`examples/shopping-list/zapper.yaml` (the office firm `shop`) rewritten as the workbench's configuration repository (proposal 12, phase 3). In the workbench there is no firm and no CEO: there is you, and the sandboxes running Claude Code you start from these profiles and work with directly. A draft for agreeing the layout; nothing reads it yet.

```
policy.yaml                       zapper's global policy layer
secrets.yaml                      each secret's hosts and mode; never a value (the secrets facility keeps those)
images/office.yaml                zapper's image recipe, as config/zapper.yaml has it
profiles/claude.yaml              Claude Code: the Claude API allowed, everything else asked
profiles/product-manager.yaml     extends claude, as the product-manager agent
profiles/engineer.yaml            extends claude, with npm, as the software-engineer agent
agents/<name>/AGENT.md            an agent definition: the CLAUDE.md of a profile that names it
agents/<name>/skills/<skill>/     that agent's own skills
skills/<skill>/SKILL.md           skills every profile gets
```

## Applying a commit

The workbench renders the repository into one zapper config, `PUT`s it (zapper validates it as a whole and works out what changed), and saves it:

- **Each file under its section**, in zapper's own format: `profiles/engineer.yaml` is `profiles.engineer`, `images/office.yaml` is `images.office`, `policy.yaml` is `policy`, `secrets.yaml` is `secrets`.
- **Each secret** gets the secrets facility's `file:` source; you set its value there.
- **A profile's agent**: `agent.definition: NAME` is the one key that isn't zapper's, and the render consumes it. `agents/NAME/AGENT.md`, without its front matter, becomes the profile's `agent.instructions`, which zapper writes as the sandbox's `~/.claude/CLAUDE.md`, read-only. No other agent goes into the sandbox.
- **Skills, merged**: for every profile, `skills/` is copied into a skills directory of its own in the live checkout, and its agent's `agents/NAME/skills/` laid over it (a skill of the same name replaces the shared one); `agent.skills` names that directory, which zapper mounts read-only where Claude Code reads skills. The agent reads both and changes neither.
- **`daemon:`** isn't in the repository: host paths and the UI's address are the host's, in the workbench's own global config (with what `workbench.yaml` and `drive.yaml` held).

`../workbench-shop.rendered.yaml` is the config rendered from this directory, and `../workbench-shop.live/` the merged skills; zapper's parser takes the config, and all three profiles resolve:

```
profile claude: skills: checking-the-product, reviewing-work
profile engineer: CLAUDE.md "You work with the user, in this session: you build what they…"; skills: checking-the-product, reviewing-work
profile product-manager: CLAUDE.md "You work with the user, in this session: you decide with the…"; skills: checking-the-product, product-brief, reviewing-work
```

## From the firm

| The firm (`examples/shopping-list`) | Here |
|---|---|
| `daemon:` | The workbench's global config |
| `secrets.claude: { file: ~/.config/zapper/claude-token, … }` | `secrets.yaml` names it and its hosts; its value goes in through the secrets facility |
| `profiles.claude`, `claude-engineer` | `profiles/claude.yaml`, `profiles/engineer.yaml`; the `deny git.shop.internal` rule goes (no firm repository) |
| `firms.shop.roles` (ceo, product-manager, engineer) | A profile per role you start yourself, `product-manager` and `engineer`. No CEO |
| `agents_dir: config/agents` (each role's `AGENT.md` and own `skills/`) | `agents/<name>/`, the same layout; its `AGENT.md` becomes the profile's CLAUDE.md |
| `skills_dir: config/skills` | `skills/`, merged with the agent's own for each profile |
| Mail, the board, reports to the CEO | The conversation: you work with each sandbox directly, and it reports to you there. The drive's inbox and outbox are only for moving files in and out |

Dropped, as the firm's alone: the CEO and its skills (`budget`, `building-the-team`, `the-board`), budgets and hiring, officers and access requests, mail, the board pack, the `office-*` skills, and the cluster's `releases` and `build-and-deploy`.

Adapted (drafts): both agents' definitions and `product-brief` to work with you in the session, the brief in `/work/brief.md`; `checking-the-product` without officers or release clusters, reporting to you; `reviewing-work` as it was, but for what named the firm's drive and knowledge base.

## The product's code

Not on the host: each sandbox clones it from GitHub over HTTPS, through zapper's proxy, with a `github` secret (the preset) and a profile rule that allows GitHub with it. The workbench's `git.zapper` is only its configuration.
