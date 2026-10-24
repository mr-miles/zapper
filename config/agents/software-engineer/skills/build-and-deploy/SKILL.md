---
name: build-and-deploy
description: "How engineers build and deploy in the firm's cluster: dev and prod, working with teammates, images, manifests, probes, ingress, rollouts and storage. Use it whenever you build, deploy or debug a workload."
---

# Building and deploying

## Development and the release

- **Development** is yours: your branches, and your workloads in `dev` named with your prefix. Build, break, load-test and debug there freely; leave others' things alone.
- **The release** is `prod`, at `http://apps.<domain>`: what the board and everyone see. It changes only in a release your manager assigns, and only with what that assignment names.
- So never deploy to, patch, restart or test against `prod` during development; never change cluster-wide settings (Traefik, CRDs, cluster roles) outside a release; never load-test what others use.

## Working with the team

- Agree interfaces with teammates by mail before building against them (API shapes, ports, paths, who owns which directories).
- If the repository is empty, push the initial layout (README, directories, a deploy script) on a branch and hand it to the merger (office-git), so everyone branches from the same `main`.

## Building and deploying

- Pick the simplest approach that works (images built with podman and a registry in the cluster, in-cluster builds, or public images with manifests), and write it down in the repository so a teammate can redeploy.
- **Build and deploy tooling is yours to deploy**, with your cluster grant (office-resources, *The cluster*): nobody approves tools; security decides only the access they need.
- Manifests in the repository, applied with `kubectl apply -f`; the namespace and your prefix as parameters (`./deploy.sh dev ada`, `./deploy.sh prod`).
- Deployments with readiness and liveness probes, resource requests, and an image tag per build; a Service in front of each.
- In `prod`, an Ingress for `apps.<domain>` (or with no `host`) on `path: /`, `pathType: Prefix`: it replaces the firm's landing page (namespace `landing`, lowest priority); a `defaultBackend` doesn't. The board sees the same through its own port, which asks for `apps.<domain>` and maps it back to its address in redirects and links.
- Check every rollout (`kubectl rollout status … --timeout=120s`); if it hangs: `get pods`, `describe`, `logs`, `get events`.
- State that must survive restarts needs storage (a PVC with local-path, or a database). Say in your report what survives a restart.
- `ImagePullBackOff` often means the cluster's pull is waiting for a security officer; Kubernetes retries once approved. Pin or vendor dependencies once fetched.
