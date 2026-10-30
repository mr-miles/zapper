[← README](../README.md) · [Docs](README.md) · **Policy**

# Policy

## Overview

A policy layer holds three ordered rule lists, `net`, `exec` and `fs`, and optional `defaults`. Each session is evaluated against a stack of layers:

```
final rules  every layer's final rules, global first
session      rules added to a running session (approvals, `zapper rule add session:ID`)
role         in a firm, standing approvals for the worker's role (`role:FIRM/ROLE`)
profile      the session's profile
parents      the profiles it extends, nearest first
global       `policy:` at the top of the config
defaults     `defaults.net` / `defaults.exec` from the most specific layer that sets them, else deny
```

Within a layer, the first matching rule decides, as in an ACL. Evaluation uses the running config, so a rule change applies to running sessions, and so to a running firm's workers, at once.

An office engineer in the example firm, for instance, is evaluated against its session's rules, then its role's layer (`role:acme/engineer`), then the `office-engineer` profile, then `office` and `base`, which it extends, then the global layer. Security officers' decisions land in the first two: `once` and `worker` approvals in the worker's session layer, `role` approvals in the role layer, which the run keeps for all workers of that role. They are never global and never past a `final` rule ([office](office.md#limits-that-hold-whatever-an-officer-decides)).

## Rules

### net

```
- { action: allow, host: "*.github.com", ports: [443], methods: [GET], path: "/repos/**", secrets: [gh], comment: … }
```

| Field | Meaning |
|---|---|
| `action` | `allow`, `deny`, or `ask`, which waits for an operator and denies after `daemon.ask_timeout_secs`; in a firm, `ask` refuses at once and becomes an access request for the officers |
| `host` | Case-insensitive glob. `*` also matches dots, so `*.github.com` matches `api.github.com` but `github.com` needs its own rule. An IP address matches as text. |
| `ports` | Ports the rule applies to; empty means all. |
| `methods`, `path` | Match the HTTP request. `path` is a glob over the path without the query, where `*` stays within one segment and `**` spans segments. In it, `{session}` stands for the calling session's ID and `{label:KEY}` for the value of its label `KEY`, each matching only itself; a rule naming a label the session lacks matches nothing (other `{a,b}` braces are alternatives, as in any glob). The path is matched as the upstream will read it: percent-encoded unreserved characters decoded and `.` and `..` segments resolved, so `/x/../admin` and `/%61dmin` are `/admin`, and that path is what is forwarded ([network: inspection](network.md#connections)). Either field makes connections to matching hosts inspected. |
| `secrets` | Secrets whose placeholders are replaced with their values in the headers of allowed requests. Makes connections inspected. |
| `set_headers` | Headers set on allowed requests; values may use `{{secret:NAME}}`. Makes connections inspected. |
| `final` | Evaluated before the other rules, starting with the global layer. |

An inspected connection is decided request by request; a rule without `methods` or `path` matches each request to its host. A passed-through connection is decided once, on host and port. How connections reach these rules is described on the [network](network.md) page.

### exec

```
- { action: deny, path: curl, args: "*--upload-file*" }
- { action: ask, path: "/usr/bin/*" }
```

If `path` contains `/`, it's a glob over the resolved executable path, otherwise over the file name. `args` is a glob over the arguments after argv[0], joined with spaces.

### fs

```
- { path: "/work/**/.env", access: hidden }     # covered by an empty file or directory
- { path: "/work/.git/config", access: ro }     # remounted read-only
```

The guest applies these rules at boot ([limitations](security.md#limitations)). The `fs` rules of all layers apply.

## Secrets

A session can use every secret it's given, and can read the value of each `guest` secret; see [security: secrets given to a session](security.md#secrets-given-to-a-session-can-be-used-by-it).

```
secrets:
  gh:      { env: GITHUB_TOKEN, hosts: [api.github.com] }
  claude:  { file: ~/.config/zapper/claude-token, hosts: [api.anthropic.com] }
  oauth:   { file: ~/.claude/.credentials.json, json: /claudeAiOauth/accessToken, hosts: [api.anthropic.com] }
  ci:      { file: ~/.config/zapper/ci.token, hosts: [ci.internal.example], plaintext: true }
  s3:      { file: ~/.config/zapper/s3-session-key, mode: guest }
  tls_key: { file: ~/.config/zapper/client.key, mode: guest }
  app:     { command: [zapper, mint, github-app, --app, acme.yaml, --repo, acme/api, --perm, contents=read], hosts: [api.github.com] }
```

`mode` sets how the secret reaches the workload:

| Mode | The guest sees | Use for |
|---|---|---|
| `proxy` (default) | a placeholder | bearer tokens and API keys, which travel in headers |
| `guest` | the value | credentials the client holds itself: client certificates and keys, kubeconfigs (as workers with a cluster grant get for the firm's cluster) |
| `aws` | a fake access key id | AWS keys: the proxy signs the guest's requests with them ([AWS](#aws)) |

Profiles refer to secrets as `{{secret:<name>}}` in `agent.env` or `agent.files`. The workload can read and use a `guest` secret once it's delivered, so issue those scoped and short-lived.

A `proxy` secret appears in the guest as `zapper-secret-<name>`. The proxy replaces the placeholder with the value when:

- a rule that lists the secret allows the request
- the destination matches the secret's `hosts`
- the connection uses TLS, or the secret sets `plaintext: true`

Values are read each time they're used, so a rotated file or variable takes effect without a restart.

A `command` secret is minted by a command on the host, for tokens that expire, such as a GitHub App's. The command prints the value, or `{"value": ..., "expires_at": RFC 3339}`, and runs with `PATH`, `HOME`, `XDG_CONFIG_HOME` and `ZAPPER_*` only (`zapper` as the first word is this zapper). Its value is kept until it nears its expiry (or for `ttl_secs`, default 300, when it prints none), and minted again ahead of it in the background while the old one is still served; once expired, requests wait for one new mint and fail if it fails. However many requests wait, the command runs once, and a failure is the answer to all of them, and to new requests for the next 5 seconds. Waiting holds no thread of the daemon's. `timeout_secs` bounds each run (default 30). A `guest` command secret is minted once, when the session boots. The command's output never appears in an error. The office's daemon mints its own GitHub tokens this way ([office: a firm on GitHub](office.md#a-firm-on-github)).

### AWS

AWS requests are signed over their headers and body (Signature Version 4), so the key can't travel in a header for the proxy to add. An `aws` secret keeps the keys on the host instead: the guest holds a fake key and signs with it, and the proxy signs each allowed request again with the real one.

```
secrets:
  aws:
    command: [aws, configure, export-credentials, --profile, dev, --format, process]
    mode: aws
    hosts: ["s3.*.amazonaws.com", "*.s3.*.amazonaws.com"]
profiles:
  dev:
    agent:
      env:
        AWS_ACCESS_KEY_ID: "{{secret:aws}}"
        AWS_SECRET_ACCESS_KEY: "{{secret:aws}}"
        AWS_REGION: eu-west-2
    policy:
      net:
        - { action: allow, host: "*.s3.eu-west-2.amazonaws.com", ports: [443], methods: [GET, PUT], secrets: [aws] }
```

- **The value** is AWS's credential_process JSON, `{"AccessKeyId", "SecretAccessKey", "SessionToken"?, "Expiration"?}`, from any source: a file, an inline value, or a command such as `aws configure export-credentials` or one that assumes a role. Temporary keys from a command are minted again before their `Expiration`.
- **In the guest**, `{{secret:aws}}` is a fake access key id, `ZAPPER` and fourteen characters made from the secret's name; it serves as the secret key too. The guest's SDK signs as it always does. Nothing in the guest is a usable key.
- **The proxy signs again** an inspected request whose signature names that fake key (in its `Authorization` header, or a presigned URL's `X-Amz-Credential`), when a rule allowing the request lists the secret, the host matches the secret's `hosts`, and the connection uses TLS (or the secret sets `plaintext: true`). It keeps the region, service and signed headers the guest chose, dates the request by its own clock, adds the session token when the keys are temporary, and signs with the real keys. A request signed with the fake key that no such rule lists is refused with `403`; one signed with keys of the guest's own is forwarded as it is.
- **The body**: its hash is the one the guest sent in `x-amz-content-sha256`, or `UNSIGNED-PAYLOAD`. Without one (AWS's JSON and query APIs), the body is read and hashed, up to 32 MiB. An S3 upload in signed chunks (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`, with or without a signed trailer) has each chunk's signature redone as it streams, so large uploads aren't held in memory. SigV4a (ECDSA) payloads are refused.
- **Responses** have the secret key and session token masked, like any injected value.
- **Hosts**: patterns are globs in which `*` spans dots. `s3.*.amazonaws.com` and `*.s3.*.amazonaws.com` are S3 alone (path-style and virtual-hosted); `s3*.amazonaws.com` would also take in `s3tables`, `s3-control` and `s3express-control`, separate services. `*.amazonaws.com` is the whole account, and never matches a look-alike such as `amazonaws.com.evil.test`.

## Approvals

Outside a firm, `ask` holds the connection, request or exec and emits an `ask` event, for the operator to answer. In a firm, a network `ask` is refused at once and decided by the security officers or the board ([office: deciding requests](office.md#deciding-requests)), and firm profiles can't have `exec` rules with `ask`.

Who decides a session's network `ask`s is the operator, unless a decider is registered for the session. A decider is registered in the daemon (`Core::add_decider`) for the sessions its labels pick; the office registers its security officers for a run's sessions while the firm runs. A request to such a session is refused at once, saying where it went ("access to HOST:PORT requested from security (request r-3); …"), and the decider is told of it, to answer later with rules on the session; it also hears which rule allowed each allowed request, so an approval for once goes with its use. A decider only sees what the rules leave as `ask`: `final` rules and every `allow` or `deny` come first. `exec` asks are always the operator's.

```
zapper approvals                                # pending
zapper approve 3sicfmqu                         # allow this request
zapper approve 3sicfmqu --remember session      # and add an allow rule to the session
zapper deny 3sicfmqu --remember session         # and add a deny rule to the session
```

An answer is kept in memory. Without `--remember` it applies to the one request; with `--remember session` it adds an allow or deny rule to the session's own layer, which ends with the session. Answers never change the running or startup config; to keep a decision, add a rule to a profile with `zapper rule add`.

A session can have up to 32 pending approvals; further requests are denied until some are answered.

## Usage

```
zapper rules                                    # all layers, each in evaluation order
zapper rule add global '{kind: net, action: deny, host: "*.evil.example", final: true}'
zapper rule add profile:office '{kind: exec, action: deny, path: "/usr/bin/ssh"}' --at 0
zapper rule add session:tusn49 '{kind: net, action: allow, host: pypi.org}'
zapper rule rm net-4
zapper eval -p office-engineer net git.acme.internal:443   # the decision, its rule and layer
zapper eval -s tusn49 exec /usr/bin/perl perl -e 1
```

A rule gets its id when it's added, such as `net-4`, or `s-net-1` for a session rule, and keeps it when saved.

## Verification

`zapper eval` shows which rule decides and in which layer. `make test` covers layering, final rules, defaults, inspection, DNS validation and when names are looked up. `make test-e2e` covers allow, deny, ask, method rules, secret injection and exec rules with real VMs.

## Limitations

- Host globs match names. Addresses are covered by the [address checks](network.md#address-checks).
- A passed-through connection is decided on host and port. Inspection rules can restrict methods and paths.

## References

- Sources: `crates/zapper-policy/src/{config,eval,template}.rs`, `crates/zapper-core/src/daemon/{proxy,dns}.rs`
