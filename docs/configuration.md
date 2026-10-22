[← README](../README.md) · [Docs](README.md) · **Configuration**

# Configuration

## Overview

The Office and the zapper daemon under it read one YAML file: `--config` or `$ZAPPER_CONFIG` if given; else `config/zapper.yaml` when run from a checkout's root (the [repository's config](../config/zapper.yaml), tracked); else `~/.config/zapper/zapper.yaml`, which `zapper init` creates from the same file and never overwrites.

Relative paths in the config (`skills_dir`, secret files, mount hosts, daemon paths) resolve against the config file's directory, and `~/` against your home, so the config needs no absolute path and runs from a fresh clone. Generated files stay outside the repository: Firecracker, kernels, images and state under `~/.local`, the Claude token under `~/.config/zapper/`. The file is written back by `zapper config save`; a config holding secret values inline (`value:`) must be readable only by you.

## Running and startup config

The daemon loads the file at start as the running config. API changes edit the running config, as on a router:

| Command | Effect |
|---|---|
| `zapper config show` | Print the running config, with secret values redacted |
| `zapper config startup` | Print the file as loaded |
| `zapper config diff` | Show what `save` would write |
| `zapper config save` | Write the running config to the file (0600), keeping the previous file as `.yaml.bak` |
| `zapper config reload` | Load the file again, discarding unsaved changes |
| `zapper config edit` | Edit the running config in `$EDITOR` |
| `zapper config apply FILE` | Replace the running config with a file |

Each change is validated as a whole before it applies. A change with an unknown profile, an `extends` cycle, a duplicate rule id, a bad glob or an unknown secret is rejected, and the running config stays as it was. When a config containing `<redacted>` inline secret values is applied, those secrets keep their current values. `save` writes plain YAML, so comments in the file are lost.

## The file

```
daemon:     paths, UI address, limits, private networks               (below)
secrets:    name → source, mode and hosts                             (policy.md: Secrets)
policy:     the global layer                                          (policy.md)
profiles:   name → profile: the VM of a worker, facility or session   (below)
firms:      name → org chart, budgets, officers, the firm's services  (office.md)
```

### daemon

| Key | Default | Meaning |
|---|---|---|
| `firecracker` | `~/.local/share/zapper/bin/firecracker` | VMM binary (`make deps`) |
| `agent` | next to `zapper` | Static `zapper-agent` binary |
| `images` | `~/.local/share/zapper/images` | Root filesystem images, `<name>.ext4` |
| `kernels` | `~/.local/share/zapper/kernels` | Guest kernels |
| `state` | `~/.local/state/zapper` | Session directories with audit logs, recordings, and disks while running |
| `ui` | `127.0.0.1:7878` | Address of the UI (state and approvals); empty turns the UI off. Loopback by default; another address serves it to the network, for a remote host ([a remote host](#a-remote-host)) |
| `ask_timeout_secs` | `60` | How long an `ask` waits before it denies |
| `max_sessions` | `16` | Sessions starting or running at the same time |
| `private_networks` | `[]` | CIDRs of private networks sessions may reach ([network](network.md#address-checks)) |
| `keeper` | unset | `{socket, dir}`: the record kept by `zapper-keeper` as a separate user, append-only ([security: the record](security.md#the-record)); `scripts/install-keeper.sh` prints the values. Read at start: changing it needs a restart |

### profiles

```
profiles:
  office:
    extends: base             # engine and agent merge key by key; mounts and setup add up
    description: …
    engine: { image: office, kernel: vmlinux-6.1, vcpus: 2, memory_mib: 4096, disk: 16GiB, ttl_secs: 0, network: nic, boot_args: "" }
    agent:  { … }             # below
    mounts: [{ host: "{workdir}", guest: /work, mode: rw, extra: 8GiB }]
    setup:  ["apt-get install -y jq"]    # run as root at boot, under this profile's policy
    policy: { net: […], exec: […], fs: […], defaults: { net: ask } }
```

`engine.network` is `nic` (the default) or `vsock`; [network](network.md) describes both.

`mounts` are copied in at boot. `{workdir}` is the directory `zapper run` was started in, or the one given with `-C`. A `rw` disk gets `extra` free space, 1 GiB by default. Changes stay in the session's disk until you copy them out with `zapper cp ID:/work ./out`.

### agent

| Key | Default | Meaning |
|---|---|---|
| `kind` | `claude-code` | The agent harness: `claude-code` sets Claude Code up and `pi` sets pi up from the keys below (both can run office workers); `generic` writes only `env` and `files`. See [agent harnesses](#agent-harnesses) |
| `command` | `[claude]` | What `zapper run` and `zapper attach` start |
| `user` | `agent` | The user the command runs as, created with uid 1000 |
| `root` | `false` | Run as root; this gives up the in-guest guardrails ([security](security.md#what-is-enforced-where)) |
| `sudo` | `false` | Run as `user` with passwordless `sudo` and none of the in-guest controls: no `no_new_privs` and no exec supervision, so exec rules don't apply and setuid binaries work. The image needs `sudo` (`make image` installs it). Like `root`, this gives up the in-guest guardrails and keeps the host-side ones; unlike `root`, the agent keeps its own home, files and user, which agents such as Claude Code need for their bypass modes. Not together with `root` |
| `workdir` | `/work` | Working directory |
| `env` | | Environment. `{{secret:NAME}}` becomes a placeholder for `proxy` secrets and the value for `guest` secrets |
| `files` | | Guest path (`~` is the home directory) → content, with `{{secret:NAME}}` as in `env`. Files with a `guest` secret are created 0600 |
| `model` | | The model. Claude Code: `model` in `~/.claude/settings.json` |
| `permission_mode` | | How the agent approves its own actions. Claude Code: `permissions.defaultMode` in the managed settings file, `/etc/claude-code/managed-settings.json`, which is root-owned. Set there, Claude Code doesn't ask at each start whether to switch to auto mode, and the workload can't change it. With `bypassPermissions`, the file also sets `skipDangerousModePermissionPrompt` |
| `instructions` | | Standing instructions, written to the harness's instructions file. Claude Code: `~/.claude/CLAUDE.md` (`claude_md` is accepted too) |
| `skills` | | A host directory of skills (`<name>/SKILL.md`), mounted read-only where the harness reads them. Claude Code: `~/.claude/skills` |
| `mcp_servers` | | MCP servers, `{name: {command, args, env}}`. Claude Code: `mcpServers` in `~/.claude.json` |
| `turn_end` | | A command run when the agent is about to end a turn; what it prints makes the agent go on. Claude Code: a `Stop` hook |
| `settings` | | Harness-native settings merged into its main config file. Claude Code: `~/.claude/settings.json` |
| `config_files` | | Files in the harness's config directory, by path relative to it (Claude Code: `~/.claude`), with `{{secret:NAME}}` as in `files` |
| `config_mounts` | | Host directories mounted in the harness's config directory: `{host, guest, mode, extra}` with `guest` relative to it, read-only unless `mode: rw` (copied in at boot, like `mounts`) |

Every session's environment also has:

- `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE`, so tools trust the session CA
- `ZAPPER_SESSION`, the session id
- in vsock mode, `HTTP_PROXY` and `HTTPS_PROXY` set to `http://127.0.0.1:3128`

For `claude-code`, zapper also sets `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, and marks onboarding, the folder trust dialog and the bypass-permissions prompt as accepted. For `pi`, it sets `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1` and `PI_TELEMETRY=0`, writes `settings.json` (`model` as `defaultModel`, `quietStartup`, `defaultProjectTrust: never`, then `settings`), `AGENTS.md` and `mcp.json` (servers declared to the model directly: `exposure: direct`, unless set) in `~/.pi/agent`; providers and models go in `config_files` as `models.json`, with keys as `${ENV}` from `env` (see the `office-deepseek` profile).

### Agent harnesses

The agent keys above are neutral: each harness (`crates/zapper/src/daemon/harness.rs`) writes them where and how its agent reads them, and the office gives its workers their preamble, skills, tools and turn-end hook through them, so it doesn't depend on one agent. A harness implements `Harness` (its config directory, its skills directory, files and environment from the neutral keys, mounts) and, to run office workers, `Driver`, which starts a `Conversation`: the command line, a handshake if the protocol has one, how a prompt goes in, what each output line means (the session id for resuming, the current tool, the end of a turn with its tokens, a rate limit or failure, a reply to write back), and handing over a message while the agent works if it can.

`claude-code` and `pi` are implemented; opencode isn't yet. [HARNESS.md](../HARNESS.md) describes the interfaces in full and how each implements them; in short:

| | Claude Code | pi | opencode (V2) |
|---|---|---|---|
| Config directory | `~/.claude` | `~/.pi/agent` (`PI_CODING_AGENT_DIR`) | `~/.config/opencode`; `/etc/opencode` for managed settings |
| Instructions | `CLAUDE.md` | `AGENTS.md` | `AGENTS.md` |
| Skills | `skills/` | `skills/` | `skills/` |
| MCP servers | `mcpServers` in `~/.claude.json` | `mcp.json`, the same shape | `mcp.servers` in `opencode.json`, `command` as one array |
| Turn end | a `Stop` hook | none: mail during a turn goes in as a `follow_up` command | none; the host's `queue` delivery |
| Headless | `claude -p`, stream-json in and out | `pi --mode rpc`, JSON lines | `opencode acp` (JSON-RPC over stdio) or `opencode serve` |
| Resume | `--resume ID` | `--session-id ID` | `--session ID` |

### firms

The office's org charts: roles, workers, budgets, security officers and the firm's services. They are described on the [office](office.md#configuration) page. Each role names a profile; the office adds the worker's preamble, skills, the `office` MCP server, the model and credentials to it at boot, and the profile decides the image, the VM's size, the network rules and the secrets.

### Model credentials: Claude and OpenRouter

Both are `proxy` secrets in files you create: the VMs see a placeholder, and the host puts the value into requests to the secret's hosts. zapper never asks for them, and an agent setting zapper up must not read or print them.

**Claude subscription** (secret `claude`, every Claude Code session and worker). Usage counts against your Pro, Max, Team or Enterprise plan, not API billing.

1. On any machine with Claude Code and a browser (your laptop will do: the token belongs to your account, not to the machine), run `claude setup-token`. It opens the sign-in page and prints a long-lived OAuth token, `sk-ant-oat…`.
2. On the zapper host, store it in a 0600 file, without it reaching your shell history (paste it at the prompt):

   ```
   mkdir -p -m 700 ~/.config/zapper
   (umask 077; read -rsp 'Claude token: ' t; printf '%s' "$t" > ~/.config/zapper/claude-token); echo
   ```

3. `zapper check`. No restart is needed: the host reads the file on each request that uses it.

Claude Code's banner says "Claude API" with this token, because the token can't read your plan; `/status` shows "Auth token: CLAUDE_CODE_OAUTH_TOKEN" and `/usage` your plan's limits. To bill a Claude Console API key instead, the comments on `secrets` in the config show the `anthropic` secret; don't set both, as Claude Code prefers the API key.

**OpenRouter** (secret `openrouter`, only for the profiles on pi: `office-deepseek`, `office-gpt-sol`, `office-gpt-terra`, `office-qwen`, `office-mimo`, `office-minimax`, and their `-engineer` variants; the red-team scenario's operative uses one by default).

1. Create a key at [openrouter.ai/settings/keys](https://openrouter.ai/settings/keys) (`sk-or-v1-…`). Add credits, or use only `:free` models. If you limit the key to some models, allow the ones those profiles name (their `model` in the config).
2. Store it the same way:

   ```
   (umask 077; read -rsp 'OpenRouter key: ' t; printf '%s' "$t" > ~/.config/zapper/openrouter-token); echo
   ```

The host reads a `proxy` secret's file on each request that uses it, so a new or replaced token applies at once. With the file missing, those requests fail with the secret's name, in `zapper events` and the audit log.

### A remote host

zapper's UI (`daemon.ui`) and a firm's Kubernetes dashboard (`dashboard_port`) listen on loopback by default. On a remote host, the simplest way in needs no config change, an SSH tunnel from your machine:

```
ssh -L 7878:127.0.0.1:7878 -L 8081:127.0.0.1:8081 you@zapper-host
```

Then open the URL `zapper serve` printed (it is also in `state/ui-url`) on your machine; the product port (`0.0.0.0:8080` in the example firm) is already on the network.

To serve them to the network instead:

```
daemon:
  ui: 0.0.0.0:7878
firms:
  acme:
    dashboard_port: 0.0.0.0:8081
```

> [!CAUTION]
> **Whoever has the UI's token controls zapper**: sessions, rules, approvals and firms, and with the dashboard's token the firm's cluster as cluster-admin. Listening on the network, both are **plain HTTP**: the tokens cross it unencrypted. Use this on a network you trust, or put a TLS proxy in front. **zapper isn't built for production**, and `zapper serve` prints a warning when either listens beyond loopback.

What still holds: the UI takes the token (as a cookie set for the name you reached it by), and its writes need the `X-Zapper-UI` header and an `Origin` naming the host the request came to, so another site can't write through your browser. The VMs can't reach the UI or the dashboard whatever they listen on: the host's own addresses are closed to them, and their rules are unchanged. For a TLS proxy in front, have the UI listen on an address other than loopback that the proxy reaches, and let the proxy pass the browser's `Host` through: on loopback, the UI takes only local names in `Host` and `Origin`.

## Verification

```
zapper check                     # loads and validates the file
zapper profile show office-engineer   # the profile and its resolved form
```

## Limitations

- `save` drops comments from the file; keep commentary in the example or the docs.
- A running session keeps the engine and agent it started with. Rule changes apply to it at once; engine and agent changes apply to sessions started afterwards.

## References

- Sources: `crates/zapper-policy/src/config.rs`, `crates/zapper/src/daemon/{mod,spec}.rs`
