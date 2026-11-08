[← Docs](README.md) · **Warden**

# The warden

`zapper-warden` is zapperclaw's host side ([proposal 09](proposals/09-zapperclaw.md), step 1). zapperclaw's office (the post office, its foremen and drive) runs in an OpenShell sandbox of its own, with no model credential and no network but the warden. The warden is the only thing that holds the gateway's credentials, and it does as little as it can with them: it makes worker sandboxes from fixed templates, starts their agents, and pipes bytes into the office sandbox. The office chooses a worker's name and profile from the warden's list, and when to start its agent; never an image, policy, provider, command line or file.

## What it serves

Only the office sandbox reaches the warden's routes. Its supervisor posts the sandbox's JWT-SVID to the warden's token endpoint (the `warden` provider, [config/openshell-warden.yaml](../config/openshell-warden.yaml)) and adds the token it gets to the office's requests, outside the VM, as workers reach the office edge. The warden serves the sandbox it made as the office (`zc-office`, from template `zc-office`) and no other.

| Route | Does | The warden holds |
|---|---|---|
| `GET /workers` | The workers it made: name, profile, sandbox, id, ready | Only sandboxes named, labelled and templated as it makes them |
| `PUT /workers/{name}` `{"profile": P}` | Makes worker `name` (sandbox `zcw-<name>`) from `zcp-<P>` with P's providers, lets it propose rules if P says so, runs P's setup. 201 made, 200 there already | P one of `profiles:`; the name's syntax; `max_workers` and P's `max` (429); a worker of another profile, or a sandbox of that name it didn't make (409) |
| `DELETE /workers/{name}` | Deletes it | Only its own (404 otherwise) |
| `POST /workers/{name}/agent` `{"ticket": T, "resume": R?}` | Runs P's harness headless in the worker (`claude -p --input-format stream-json …`, `--resume R` if given), and pipes its frame stream into the office sandbox's agent port, opened with the line `ZCAGENT T` | The command line is the harness's; `T` is 32 to 128 hex digits, `R` one plain id (no flags) |
| `GET /jwks` | The driver's JWKS, as it serves it | Public keys, for the office to check its workers' SVIDs |

And two forwards into the office sandbox, bytes only, by `ForwardTcp`:

| Host address | Office sandbox port | For |
|---|---|---|
| `openshell.yaml`'s `edge.listen` (127.0.0.1:8970) | `office.ports.edge` (8970) | The workers' supervisors: the office's tools, drive, Stop hook and token grant |
| `board` (127.0.0.1:8974) | `office.ports.board` (8099) | You: the dashboard and CLI, behind the office's own token |

The agent's stream goes the same way, as a forward the warden opens to `office.ports.agent` (8973): the office needs nothing of its supervisor but plain requests, and the warden reads none of what the agent and the office say.

Every call is recorded in `<state>/warden.jsonl` (private to its user): workers made, failed, deleted, agents started, refusals. It is the host's account of the office, apart from the office's own journal.

## Templates

`zapper-warden apply` makes the office's template (`zc-office`, from `office.profile`) and one per worker profile (`zcp-<profile>`), from the zapper config, with their files staged under the driver's files root (`zapperclaw/<template>`) as the workbench stages its own. A worker profile's template has what the office gave each worker it made, once: the office's tools as its MCP server (Claude Code over HTTP to the edge, other harnesses through `office mcp`), the Stop hook (`office stop-hook`), `ZAPPER_OFFICE_URL`, and the profile's own instructions, skills and model if `warden.yaml` gives them. The office's template gets `ZAPPERCLAW_WARDEN_URL` and its three ports. Only a harness the office can drive headless (Claude Code, pi) can be a worker profile.

## Setting it up

```
make build
make image IMAGE=office-fake          # the fake worker's image, for the test
make image IMAGE=zapperclaw-office    # the office sandbox's (for now a probe: curl, socat)
openshell profile import -f config/openshell-warden.yaml
openshell provider create --name warden --type warden --runtime-credentials
zapper-warden --config config/warden.yaml check    # the templates it would make
zapper-warden --config config/warden.yaml apply
zapper-warden --config config/warden.yaml serve    # makes zc-office if it isn't there
zapper-warden --config config/warden.yaml status
```

The `office-edge` provider ([config/openshell-office-edge.yaml](../config/openshell-office-edge.yaml)) is the workers' as before; the `anthropic` provider is the Claude workers'. [config/warden.yaml](../config/warden.yaml) has every setting, with its default.

[tests/e2e-warden.sh](../tests/e2e-warden.sh) is the step's gate on a real gateway: from inside the office sandbox, with curl, it makes a fake worker, is refused what it may not do, drives the worker's agent through the warden (its ticket, a prompt in as a frame, the turn's `result` out), checks the workers' edge forward, deletes the worker, and reads the warden's record.

## Status

Step 1 of [proposal 09](proposals/09-zapperclaw.md). The office sandbox is a probe until step 2 carves zapperclaw from zapper-office: until then nothing in it listens on the edge, board or agent port but what the test starts.
