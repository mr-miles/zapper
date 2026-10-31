# Agent harnesses

A harness is the coding agent a session runs: Claude Code and pi today, opencode later. zapper sees an agent only through the interfaces in `crates/zapper/src/daemon/harness.rs`, so adding one is an implementation of those interfaces plus a name in `agent.kind` (`zapper_policy::AGENT_KINDS`). This page describes the interfaces, how Claude Code and pi implement them, and how opencode would; opencode isn't implemented yet. The pi and opencode details come from their repositories and documentation as of 2026-10-17 (pi `@earendil-works/pi-coding-agent` 1.3, opencode `@opencode/cli` 2.0); check them again before implementing.

## The interfaces

**Configuration, in neutral terms.** A profile's `agent` section says what the agent should have; the harness says where and how its agent reads it ([configuration](docs/configuration.md#agent)):

| Key | Meaning |
|---|---|
| `instructions` | Standing instructions, written to the harness's instructions file |
| `skills` | A host directory of `<name>/SKILL.md` skills, mounted read-only where the harness reads skills |
| `mcp_servers` | MCP servers as `{name: {command, args, env}}`, written in the harness's format |
| `turn_end` | A command to run when the agent is about to end a turn; if it prints a reason, the agent goes on with it |
| `model`, `permission_mode` | The model, and how the agent approves its own actions |
| `settings` | Harness-native settings merged into its main config file |
| `config_files`, `config_mounts` | Files and host directories placed relative to the harness's config directory |

The office uses only these to set a worker up: the preamble as `instructions`, the role's skills as `skills`, the office tools (`zapper-agent office mcp`, a stdio bridge to the office's server in the daemon's MCP gateway) in `mcp_servers`, and `zapper-agent office stop-hook` as `turn_end`.

**`Harness`**: setting the agent up in the guest.

- `kind()`: its name in `agent.kind`.
- `config_dir(home)`: where it reads its configuration; `config_files` and `config_mounts` are relative to it.
- `skills_dir(home)`: where it reads skills; by default `<config_dir>/skills`.
- `guest(profile, home, env, files)`: files and environment from the neutral keys, such as settings, the instructions file, the MCP config, the turn-end hook, and switching off update checks.
- `mounts(agent, home)`: the skills directory and `config_mounts`; the default implementation is usually enough.
- `headless()`: how to run it headless (a driven session, an office worker), if it can be.

**`Headless` and `Conversation`**: running the agent headless, one long conversation per driven session or worker. The driver (or the office's foreman) opens a terminal channel (no tty) to the command, writes lines to its stdin, and reads JSON lines from its stdout. A `Conversation` is one agent process. It is stateful, so protocols with request ids and sessions keep them there.

- `argv()`: the command line, continuing an earlier conversation when given a session id.
- `prompt(text)`: the line that gives the agent a prompt (a wake).
- `read(line)`: what an output line means, as `Event`s:
  - `Session(id)`: the conversation's id, for resuming it after a restart.
  - `Activity(text)`: what the agent is doing now (`Edit pricing.md`), for the UI.
  - `TurnEnd { tokens, outcome, detail }`: the turn ended. `tokens` counts input, output and cache writes against budgets. `outcome` is `Done`, `RateLimited` (the foreman retries the same wake later) or `Failed(why)` (the wake is kept and the worker is marked in error). `detail` goes into the journal's `turn end` record.
- `steers()` and `steer(text)`: hand the agent a message while it works, where the harness supports that natively. Without it, the `turn_end` command hands mail over when the agent is about to stop (`foreman::stop_check`).

The fake Claude (`zapper-fake`, installed as `claude` in the `office-fake` image) is the test double for office e2e tests. It speaks Claude Code's stream-json and runs the `turn_end` hook from the settings the way Claude Code does. A new harness either gets its own fake or is tested through the same events.

## Claude Code (`claude-code`)

| | |
|---|---|
| Config directory | `~/.claude` |
| Settings | `~/.claude/settings.json` (`model`, `settings`, the `Stop` hook); `~/.claude.json` (onboarding and trust accepted, `mcpServers`); `/etc/claude-code/managed-settings.json` (root-owned: `permission_mode`) |
| Instructions | `~/.claude/CLAUDE.md` |
| Skills | `~/.claude/skills` |
| `turn_end` | a `Stop` hook; it blocks the stop with the reason it prints |
| Environment | `DISABLE_AUTOUPDATER=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`; the token from the profile (`CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`, added by the proxy) |
| Command | `claude -p --input-format stream-json --output-format stream-json --verbose --permission-mode bypassPermissions [--resume ID]` |
| Prompt | `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]}}` |
| Output | `session_id` on events; `assistant` events with `tool_use` blocks (activity); `result` ends a turn, with `usage` (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`), `is_error` and `result` (rate limits and API failures are recognised in its text) |
| Mid-turn mail | the `Stop` hook (`turn_end`) |
| Install | the `office` image installs a pinned version |

## pi (`pi`)

pi is the coding agent in [earendil-works/pi](https://github.com/earendil-works/pi), package `packages/coding-agent`, npm `@earendil-works/pi-coding-agent`, binary `pi`, on Node 22.19 or later. Implemented against 1.3.0, checked against a run on an OpenAI-compatible endpoint (2026-10-17).

| | |
|---|---|
| Config directory | `~/.pi/agent` |
| Settings | `settings.json`: `defaultModel` from `model`, `quietStartup: true`, `defaultProjectTrust: "never"` (a worker loads no `.pi/` from its work), then `settings` (`defaultProvider`, `retry.*`, `compaction.*`) |
| Instructions | `~/.pi/agent/AGENTS.md` |
| Skills | `~/.pi/agent/skills` |
| MCP servers | `~/.pi/agent/mcp.json`, `{"mcpServers": …}`, the neutral shape. Each server gets `"exposure": "direct"` unless set: pi's default (`codemode`) reaches MCP tools only from codemode scripts, so the office's tools wouldn't be declared to the model. Tools are named `mcp__<server>__<tool>` |
| Models | `models.json` in `config_files`: a provider with `baseUrl`, `api` (`openai-completions` for an OpenAI-compatible endpoint), `apiKey` as `${NAME}` from `env`, and `models`. With `{{secret:NAME}}` in `env`, pi sends the placeholder and the proxy puts the key in |
| `permission_mode` | nothing to set: pi has no approvals; zapper's sandbox is the control |
| `turn_end` | not used: pi runs no shell hooks; mail during a turn goes in as `follow_up` |
| Environment | `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`, `PI_TELEMETRY=0` |
| Command | `pi --mode rpc --session-id <id>`: the host picks a stable id per worker (`w-…`), which opens the session or creates it, so resuming needs nothing from the output |
| Prompt | `{"id": "z-N", "type": "prompt", "message": …}`; pi answers with a `response` (`success: false` if refused, which ends the turn as failed) |
| Output | `tool_execution_start {toolName, args}` → activity. `message_end` of an assistant message: `usage {input, output, cacheRead, cacheWrite}`, `stopReason` (`stop`, `toolUse`, `error`, `aborted`), `errorMessage`; the turn's tokens are `input + output + cacheWrite` summed over its messages. `auto_retry_end` with `success: false` and `finalError`: pi's own retries gave up. `agent_settled` ends the turn (`agent_end` alone is not final). A last `stopReason: "error"` is `RateLimited` when its message names a rate limit or overload (429, 529), else `Failed` (a refused key: `401: …token validation failed`, at once) |
| Mid-turn mail | `steers()`: the foreman sends `{"type": "follow_up", "message": …}` as mail arrives; pi delivers it when the agent would otherwise stop, before `agent_settled` |
| Install | the `office` image: `npm install -g --ignore-scripts @earendil-works/pi-coding-agent@$(PI_VERSION)` |
| Tests | `harness::tests::pi_setup` and `pi_output` (events as pi printed them); `zapper-fake`, installed as `pi` in the `office-fake` image, speaks the same RPC for office e2e (the `mixed` firm) |

Notes from the first runs: a turn on a 27B model behind a busy endpoint takes one to two minutes (the first byte came after 60 s); `httpIdleTimeoutMs` (300 s) covers it. pi also has an `abort` command, which a turn timeout could use instead of hanging the process up.

## opencode (`opencode`)

opencode is [anomalyco/opencode](https://github.com/anomalyco/opencode), npm `@opencode/cli` (V2; V1 was `opencode-ai`), binary `opencode`, a native build behind a Node launcher. **V2 changed a lot**: it renamed config keys, replaced the server API, reads only `AGENTS.md`, and has no Claude subscription login. Implement against V2 and its documentation in `services/www/src/docs/content/`.

**Install.** Install it in the image at a pinned version (`npm i -g @opencode/cli@2.0.x`), with `"update": "disable"` in its config and `OPENCODE_DISABLE_AUTOUPDATE=1`.

**Configuration** (`config.mdx`, `policies.mdx`): `opencode.json` or `.jsonc`, in increasing precedence: global `~/.config/opencode/`, the project, `OPENCODE_CONFIG_CONTENT`, then the managed `/etc/opencode/`, which users and projects can't override.

| Neutral key | opencode |
|---|---|
| config directory | `~/.config/opencode` (`OPENCODE_CONFIG_DIR`) |
| `settings`, `model` | `opencode.json`: `model` (`provider/model#variant`), `permissions`, `agents`, `commands`, `instructions[]`, `providers`, `plugins` |
| `instructions` | `~/.config/opencode/AGENTS.md`; extra files go in `instructions[]`. V2 doesn't read `CLAUDE.md` |
| `skills` | `~/.config/opencode/skills` (also `~/.claude/skills`, `~/.agents/skills`, `skills[]`); the model loads them through a `skill` tool |
| `mcp_servers` | `"mcp": {"servers": {name: {"type": "local", "command": [command, ...args], "environment": env}}}`, converted from the neutral shape. V2 exposes MCP tools through "Code Mode" by default; set `codemode: false` to expose them as tools |
| `permission_mode` | the managed `/etc/opencode/opencode.json`, root-owned like Claude Code's managed settings: `{"action":"*","resource":"*","effect":"allow"}`, or `--auto` on the command line. Without either, a headless run rejects every permission request. V2 allows `external_directory` by default |
| auth | `ANTHROPIC_API_KEY` from the environment, or `providers.anthropic.settings.apiKey` / `.authToken` (a bearer token) and `.baseURL`. There's no Claude Pro/Max login; whether a `claude setup-token` token works as `authToken` is unverified. Accounts from `opencode auth login` live in its database and take precedence over the environment |

**Driving.** opencode has no stdin streaming prompt mode like `claude -p`. Two ways fit:

1. **ACP over stdio (preferred).** `opencode acp` speaks the Agent Client Protocol: JSON-RPC 2.0, one message per line on stdin and stdout. It needs two things `Conversation` no longer has (they were removed while no harness used them): lines to send first, and a reply written back for a request from the agent.
   - Opening: `initialize`, then `session/new` (or `session/load` with the id to resume).
   - `prompt`: a `session/prompt` request with the session id from `session/new`'s response, which the conversation keeps.
   - `read`:
     - `session/update` notifications → `Activity` (tool calls) and text.
     - The `session/prompt` response with its `stopReason` → `TurnEnd`.
     - Requests from the agent, such as `session/request_permission` → a reply written back, allowing them as the profile's `permission_mode` says.
   - Token usage: check what opencode's ACP reports per turn. If it doesn't report usage, read it from `opencode` with `session.step.ended` events, or count from the database.
2. **HTTP.** `opencode serve` (or `--standalone`) per worker, with `POST /api/session/:id/prompt` and the `GET /api/event` SSE stream. Its run lifecycle events end turns: `session.execution.succeeded`, `.failed {error}`, `.interrupted`. `session.step.ended {tokens: {input, output, reasoning, cache: {read, write}}}` gives usage, and `session.retry.scheduled` gives rate limits. This needs a small bridge in the guest (`zapper-agent` turning the HTTP and SSE into lines), because the foreman speaks over a terminal channel. The SSE stream has no replay.

- **Sessions.** Ids look like `ses_…`; resume with `--session <id>` or `session/load`. They live in SQLite (`~/.local/share/opencode/opencode.db`, `OPENCODE_DB`), not in files.
- **Mid-turn mail.** A prompt's `delivery: "steer"` (at the next step) or `"queue"` (on the next turn) hands mail over natively, so `steers()` is true if the transport exposes it. There's no Stop hook: V2 plugin hooks cover prompt admission, model requests, retries, tools and permissions. `turn_end` can't be supported as a command, and the host's `queue` delivery replaces it.

**Open questions.**
- Whether ACP exposes `delivery` and token usage.
- How V2 reports a refused token.
- Which tools Code Mode leaves visible to the office's skills, which name tools like `mail_send`.

## Adding a harness

1. Add its name to `AGENT_KINDS` (`crates/zapper-policy/src/config.rs`) and to `harness::get`.
2. Implement `Harness`: `config_dir`, `skills_dir` if it isn't `<config_dir>/skills`, and `guest`, which writes its settings, instructions file and MCP config from the neutral keys and switches off update checks.
3. To drive it headless (a driven session, an office worker), implement `Headless` and `Conversation` for its headless protocol, mapping its output to `Session`, `Activity` and `TurnEnd`, and `steer` if it takes messages while it works.
4. Build an image that installs it at a pinned version, and a profile with `agent.kind` set to it.
5. Unit-test the mapping of its output (like `harness::tests::claude_output`), and for office workers add an e2e firm that uses it, with a fake that speaks its protocol or with the real agent.
6. Update [configuration](docs/configuration.md#agent-harnesses) and this page.
