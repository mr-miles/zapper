[← README](../README.md) · [Docs](README.md) · **Images and kernels**

# Images and kernels

## Overview

An image is an ext4 file built from a container image's filesystem. A profile selects one with `engine.image`, and each session boots its own copy. A kernel is an uncompressed `vmlinux`, selected with `engine.kernel`. The guest agent comes from an initramfs that the daemon builds when it starts, so images don't need it. In the example firm, workers boot the `office` image, and the cluster boots the `k3s` image with the `vmlinux-6.1-k8s` kernel.

## How it works

`zapper image build NAME` builds the image from its recipe in the config, under `images:`: the OCI image it starts `from`, the commands it `run`s, the files or directories from the host it takes as `args` (each a name, a description and a default), where it `copy`s them, after those commands, and the commands to run once they are in (`after`). With the copies after `run`, changing an argument's file rebuilds only from the copy: the steps before it stay cached. `make image IMAGE=NAME` runs it after `make build`. Without a recipe, `zapper image build NAME --from REF [--run CMD]…` gives the base and steps on the command line.

When the daemon is running, `zapper image build` asks it to build (`POST /v1/images/NAME/build`), from its running config, and prints the build's log as it comes; it exits 0 if the image was built. The daemon runs one build at a time and keeps each build's log, its latest 5000 lines, for anyone who follows it later (`GET /v1/images/builds/ID`), such as a UI; each build is an `image` event when it starts and when it ends. Without a daemon, or with `--local`, the CLI builds itself, from the config file; `make image` always does (`--local`), so it builds from the checkout's config whatever daemon runs. Either way, the same code builds the image, and the files a recipe's `args` name are read by whichever builds.

```yaml
images:
  office-fake:
    description: A fake Claude Code and pi, for the office's end-to-end tests
    from: docker.io/library/ubuntu:26.04
    run:
      - "apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git less procps iproute2 && rm -rf /var/lib/apt/lists/*"
    args:
      fake-source:                       # --arg fake-source=PATH, else the default
        default: ../target/x86_64-unknown-linux-musl/release/zapper-fake   # relative to the config's directory
      scripts:
        default: ../tests/fake
    copy:
      - { from: fake-source, to: /usr/local/bin/claude }
      - { from: fake-source, to: /usr/local/bin/pi }
      - { from: scripts, to: /fake }     # a directory: its contents go into /fake
    after:
      - "claude --version && pi --version && ls /fake"
```

1. Pulls `REF` with podman, or builds `FROM REF`: one `RUN` per step, then one `COPY` per `copy` entry (modes kept; a directory's contents into the directory named), then one `RUN` per `after` step. `$ZAPPER_OCI` selects another engine.
2. Exports the container's filesystem as a tar.
3. Extracts the tar and runs `mkfs.ext4 -d` inside `podman unshare`, the engine's user namespace. This keeps the image's file ownership without needing root; outside it, every file would belong to the building user, and uid 1000 in the guest would own `/usr`.
4. Writes `images/NAME.ext4`, and `images/NAME.json` with the source, steps, size and build time.

The image is sized at its content plus 25% plus 256 MiB, unless `--size` is given. Each session's copy is grown to `engine.disk`.

## Usage

```
make image IMAGE=office                        # office workers: Claude Code and pi (pinned), git, kubectl, podman, Node.js, Python, headless Chromium
make image IMAGE=k3s                           # a firm's cluster: k3s (pinned) and its air-gap images
make image                                     # ubuntu-26.04 with curl, git and CA certificates, for the base profiles
make image IMAGE=office-fake                   # the office's e2e tests: the fake Claude Code and pi (zapper-fake) as `claude` and `pi`, and tests/fake at /fake
zapper image build office-fake --arg fake-source=./my-fake   # a recipe's file from elsewhere
zapper image build rust --from docker.io/library/rust:1-slim --run "apt-get update && apt-get install -y git"
zapper image ls
```

For TLS through inspected connections, an image needs a CA bundle at `/etc/ssl/certs/ca-certificates.crt`, as the `ca-certificates` package provides. The agent adds the session CA to it at boot.

`make deps` downloads the Firecracker CI kernel 6.1 to `kernels/vmlinux-6.1`, with its config alongside. Another kernel works if it has these built in: virtio-mmio block and net, vsock, `IP_PNP`, devtmpfs, ext4, seccomp filters, cgroup2 and BSD process accounting (`CONFIG_BSD_PROCESS_ACCT`).

Firecracker boots the kernel file from the host, not one from the image, and the CI kernel has no loadable modules: what a guest needs must be built in. `make kernel KERNEL=k8s` builds `kernels/vmlinux-6.1-k8s` for a firm's cluster: the same 6.1 source (checked against kernel.org's checksum) and config, with `config/kernel/k8s.config` merged over it (the netfilter matches kube-proxy uses, ipset, nftables, VXLAN, IPVS). It builds in a container and fails if an option doesn't survive `olddefconfig`. Another fragment in `config/kernel/` builds another kernel the same way.

Versions are pinned in the recipes in `config/zapper.yaml`: Claude Code's for the office image (its auto-update is off in sessions, so a run uses the version the image was built with), pi's, Playwright's, Node.js's and kubectl's, and k3s's with the checksums of k3s and its images. Update by changing them and rebuilding.

## Verification

```
zapper image ls                                # name, size and source of each image
zapper run -p default -- cat /etc/os-release   # boots the image
```

## Limitations

- Sessions change their own copy, and the image stays as built. Put tools in the image, or install them with the profile's `setup` steps.
- Image builds run with full network access, as trusted operator steps.

## References

- Firecracker CI kernels: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
- k3s air-gap install: https://docs.k3s.io/installation/airgap
- Sources: `crates/zapper/src/image.rs`, `crates/zapper-core/src/daemon/vm.rs`, `scripts/fetch-deps.sh`, `scripts/build-kernel.sh`, `config/kernel/`
