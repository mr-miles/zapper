[← Docs](README.md) · **Workbench**

# The workbench

`zapper-workbench` is what one person's OpenShell sandboxes share that OpenShell doesn't keep. OpenShell and its UI make, drive and reach the sandboxes (their policy, credentials, terminals and SSH); the workbench adds three things:

- **A drive**: files in `~/.local/share/zapper-workbench/drive`. You put files in `inbox/` on the host; each sandbox reads them at `/drive/inbox/` and writes its own `/drive/outbox/<sandbox>/`, which you read on the host. The drive decides that itself: a sandbox can't write the inbox or read another's outbox, and every path stays in the directory (no `..`, hidden names or symbolic links on the way).
- **A repository**: `~/.local/share/zapper-workbench/config.git`, bare. You clone it on the host and push to it, `main` included. Sandboxes reach it at `/git/config.git` and may push branches only, never `main`, `main/…` or tags, each push as the sandbox: an agent proposes a change on a branch, and you merge it.
- **Templates from the repository**: `zapper-workbench apply` turns a commit of `main` into the gateway's sandbox templates, one per profile, `wb-<profile>`. Each image's recipe is built where it or its user changed, as `localhost/zapper-workbench-<image>` (unless `openshell.yaml`'s `images:` names the image, which is then used as it is); its sandboxes get no `office` command, as they don't reach the office. A profile's template is its image, CPU and memory; its writable disk (`engine.disk`); its environment (the profile's, its harness's, and `ZAPPER_WORKBENCH_PROFILE`); its files (the harness's instructions, settings, MCP servers and turn-end hook, the skills and directories it mounts), staged under the driver's files root (`files_root:` in `openshell.yaml`), which openshell-firecracker copies into each sandbox made from the template before its workload starts; and labels saying which definition it is: `zapper-workbench: 1`, `zapper-profile: <profile>` and `zapper-commit: <commit>`. Its setup commands run in each sandbox `new` makes, once it is ready.

## Sandboxes from the templates

`zapper-workbench new PROFILE` makes a sandbox from the profile's template, with the template's labels on it and the workbench's providers attached, follows the gateway's stream of its status (`WatchSandbox`) until it is ready, and runs the profile's setup commands. A sandbox made directly from a `wb-` template (`openshell sandbox create --template wb-dev`, or OpenShell's UI) is the profile's too, files and all, but its setup commands don't run and it doesn't carry the template's labels (OpenShell copies neither). `status` lists the sandboxes `new` made, and why one wasn't set up. Changing a profile changes the sandboxes made after it is applied, not those already running.

## The repository's layout

```text
images/NAME.yaml             an image recipe (as config/zapper.yaml's images:)
profiles/NAME.yaml           a profile (as zapper.yaml's profiles:)
agents/NAME/AGENT.md         a profile's instructions, for one that says `agent.definition: NAME`
agents/NAME/skills/SKILL/    that agent's own skills
skills/SKILL/                skills every profile gets
```

A path relative to the repository (in mounts, config mounts and image arguments) is the checkout's.

## Reaching it from a sandbox

The workbench's edge is one HTTP service on host loopback, `127.0.0.1:8971`, which sandboxes reach as `host.openshell.internal:8971` through their supervisors, as the office's workers reach the office edge. Who is asking is the sandbox its supervisor names: the `workbench-edge` provider ([config/openshell-workbench-edge.yaml](../config/openshell-workbench-edge.yaml)) has the supervisor trade the sandbox's SPIFFE ID for a short-lived token and add it to the sandbox's requests outside the VM. A request without one is refused.

```
openshell profile import -f config/openshell-workbench-edge.yaml
openshell provider create --name workbench-edge --type workbench-edge --runtime-credentials
```

Then attach `workbench-edge` to the sandboxes that should reach the drive and repository. Inside one:

```
curl http://host.openshell.internal:8971/drive/inbox/
git clone http://host.openshell.internal:8971/git/config.git
```

## Commands

```
zapper-workbench serve           # the edge: the drive and the repository for the sandboxes
zapper-workbench apply           # main applied: images built where they changed, each profile a template
zapper-workbench check REF       # what applying a branch or commit would build and make
zapper-workbench new PROFILE     # a sandbox from the profile's template, labelled and set up once ready (--name to name it)
zapper-workbench status          # the commit applied last, the sandboxes `new` made, and where the drive and repository are
```

Its config is `~/.config/zapper-workbench/workbench.yaml` (all optional):

- `openshell:` the gateway's settings (default `~/.config/zapper/openshell.yaml`, the office's).
- `workspace:` the gateway workspace its templates and sandboxes are in; by default `openshell.yaml`'s `workspace:`, else `default`. It sees only that workspace's sandboxes, and the providers it attaches must be that workspace's, so give the OpenShell CLI the same one (`--workspace NAME` or `OPENSHELL_WORKSPACE`) when you create the providers and your sandboxes.
- `providers:` what `new` attaches (default `[anthropic, workbench-edge]`).
- `edge:` the edge's `listen`, `url` and `token_url`, each optional (port 8971 by default); its driver identity settings are `openshell.yaml`'s.

The repository holds no network policy: `policy.yaml`, or `policy:` in a profile, is refused, as a sandbox's policy is OpenShell's (`openshell policy set`, or its UI). Profile names are their templates' and labels': letters, digits, `-`, `_` and `.`, at most 60 of them. `check` and `apply` list what a profile sets that OpenShell ignores.
