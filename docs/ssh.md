[← Docs](README.md) · **SSH**

# SSH into a sandbox

`ssh ID.zapper` opens the sandbox's agent (its profile's command) in a terminal, from your own machine, and `ssh ID.zapper CMD` runs a command there. There is no sshd in the guest and no new listener: your host's sshd authenticates your key and runs zapper's gateway as the key's only command, and the gateway carries SSH to the sandbox's terminals as `zapper attach` does. Each terminal is recorded, runs as the workload user, and has every program it starts checked against the session's `exec` rules.

```
your machine: ssh ID.zapper
  └ ProxyCommand ssh -T you@host ID.zapper        the host's sshd checks your key
       └ forced command: zapper ssh proxy --key NAME   the gateway: SSH on its stdin and stdout
            └ the daemon's API: a terminal in sandbox ID, refused while its SSH is off
```

## Setting it up

On the zapper host, `zapper ssh config --host you@host` prints three things:

1. A `~/.ssh/config` block for your machine: `Host *.zapper` with `ProxyCommand ssh -T you@host %h` and `HostKeyAlias zapper`.
2. A `known_hosts` line for your machine: the gateway's host key under the alias `zapper`, one key for every sandbox (made on first use in `~/.config/zapper/ssh_host_ed25519`, mode 0600).
3. The `authorized_keys` line for the host, for each key that may reach sandboxes:

   ```
   restrict,command="ZAPPER_SOCKET=/run/user/1000/zapper/api.sock /home/you/.local/bin/zapper ssh proxy --key laptop" ssh-ed25519 AAAA… laptop
   ```

   `restrict` gives the key no shell, no forwarding and no agent on the host: sshd runs only the gateway, which takes the sandbox from the name you asked for (`SSH_ORIGINAL_COMMAND`). `--key` names the key in the record.

Without `--host`, the lines are for SSH on the host itself (`ProxyCommand zapper ssh proxy %h`).

## On and off, per sandbox

A sandbox takes SSH only when it was created with `--ssh` (`zapper run --ssh`, `"ssh": true` in `POST /v1/sessions`) or turned on later; it is off by default.

```
zapper ssh on ID       # SSH reaches it, from now on
zapper ssh off ID      # refused from now on, and the terminals SSH has open in it closed
```

Both work while it runs. Off closes its SSH terminals at once (a persistent one is detached and keeps running); `zapper attach` isn't affected. A refusal is an `ssh` event with the key's name, so the record shows who tried; `enabled` and `disabled` are events too, and a persistent sandbox keeps the setting. `zapper ps` shows `on` in its `SSH` column.

The switch is enforced by the daemon, not the gateway: the gateway marks its terminals as SSH's (`ssh=true` on `attach`), and the daemon refuses them while the session's SSH is off. A key in `authorized_keys` can run nothing but the gateway, so it can't open a terminal any other way.

## What works

| You run | In the sandbox |
|---|---|
| `ssh ID.zapper` | The sandbox's SSH terminal: the profile's command (Claude Code for `-p claude`) at your terminal's size, resized with it. It is persistent: a dropped connection leaves the agent running, and the next `ssh ID.zapper` attaches to it again, what it kept replayed |
| `ssh -t ID.zapper CMD` | `CMD` under `/bin/sh -c`, in a terminal |
| `ssh ID.zapper CMD`, `rsync` | `CMD` under `/bin/sh -c` without a terminal: stdin, stdout and stderr binary-safe, its exit status back |

Your `TERM`, `LANG` and `LC_*` reach the program; nothing else of your environment does. Up to 16 channels per connection.

Refused: port forwarding either way (`-L`, `-R`, `-D`), agent and X11 forwarding, subsystems (`sftp`, and so OpenSSH 9's `scp`), and legacy `scp -O`. `zapper cp ID:/path DIR` copies out.

## Connections

A sandbox has one SSH terminal, the agent's. Each `ssh ID.zapper` attaches to it while its program runs, and starts it again once it has exited. A new connection takes over from one attached already, which is closed: a connection left behind (a laptop asleep) is booted by connecting again. The gateway also sends a keepalive every 30 seconds and ends a connection after three go unanswered, about 90 seconds; the agent keeps running. A persistent sandbox keeps the terminal's number, so after a daemon restart `ssh` attaches to the same one.

`ssh ID.zapper CMD` is a terminal of its own, ended with its connection, so several can run at once. `zapper ssh off ID` closes every SSH connection to the sandbox; the agent's terminal is detached and keeps running.
