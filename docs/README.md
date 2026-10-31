[← README](../README.md) · **Docs**

# Documentation

zapper, the daemon, CLI and sandbox:

- [Architecture](architecture.md): components, a session's life, channels, exec supervision, the office's parts in the daemon.
- [Network](network.md): NIC and vsock modes, DNS, how connections are decided, address checks.
- [Policy](policy.md): net, exec and fs rules, layering, secrets, approvals, dry runs.
- [Security model](security.md): threat model, where each control is enforced, the office's controls, limitations.
- [Configuration](configuration.md): the config file, profiles, agents, firms, and the running and startup config.
- [Images and kernels](images.md): building root filesystems from OCI images, and guest kernels.
- [Instrumentation](instrumentation.md): events, audit logs, terminal recordings and the UI.
- [API](api.md): the REST API on the unix socket, and the UI's API.
- [Commands](commands.md): `zapper` commands and `make` targets.
- [SSH](ssh.md): `ssh ID.zapper` through the host's sshd, turned on and off per sandbox.
- [Testing](testing.md): unit and end-to-end tiers, the attack probe, manual scenarios, and which invariant each test covers.

The Office, a company of AI workers built on zapper:

- [The Office](the-office.md): what it is, what you see, scenarios, and its quickstart.
- [Quickstart](quickstart.md): from a fresh clone to a running firm, step by step.
- [Office reference](office.md): firms of AI workers, one VM each: mail and wake-ups, the shared drive, security officers, the cluster, the UI, replay.
- [Office prompts](office-prompts.md): the preamble, wake prompts, follow-ups, officers' prompts and an example kickoff.
