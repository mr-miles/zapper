[← README](../README.md) · [Docs](README.md) · **Images and kernels**

# Images and kernels

## Overview

An image is an ext4 file built from a container image's filesystem. A profile selects one with `engine.image`, and each session boots its own copy. A kernel is an uncompressed `vmlinux`, selected with `engine.kernel`. The guest agent comes from an initramfs that the daemon builds when it starts, so images don't need it. In the example firm, workers boot the `office` image, and the cluster boots the `k3s` image with the `vmlinux-6.1-k8s` kernel.

## How it works

`zapper image build NAME --from REF [--run CMD]…`:

1. Pulls `REF` with podman, or builds `FROM REF` with one `RUN` per `--run`. `$ZAPPER_OCI` selects another engine.
2. Exports the container's filesystem as a tar.
3. Extracts the tar and runs `mkfs.ext4 -d` inside `podman unshare`, the engine's user namespace. This keeps the image's file ownership without needing root; outside it, every file would belong to the building user, and uid 1000 in the guest would own `/usr`.
4. Writes `images/NAME.ext4`, and `images/NAME.json` with the source, steps, size and build time.

The image is sized at its content plus 25% plus 256 MiB, unless `--size` is given. Each session's copy is grown to `engine.disk`.

## Usage

```
make image IMAGE=office                        # office workers: Claude Code and pi (pinned), git, kubectl, podman, Node.js, Python, headless Chromium
make image IMAGE=k3s                           # a firm's cluster: k3s (pinned) and its air-gap images
make image                                     # ubuntu-26.04 with curl, git and CA certificates, for the base profiles
zapper image build rust --from docker.io/library/rust:1-slim --run "apt-get update && apt-get install -y git"
zapper image ls
```

For TLS through inspected connections, an image needs a CA bundle at `/etc/ssl/certs/ca-certificates.crt`, as the `ca-certificates` package provides. The agent adds the session CA to it at boot.

`make deps` downloads the Firecracker CI kernel 6.1 to `kernels/vmlinux-6.1`, with its config alongside. Another kernel works if it has these built in: virtio-mmio block and net, vsock, `IP_PNP`, devtmpfs, ext4, seccomp filters, cgroup2 and BSD process accounting (`CONFIG_BSD_PROCESS_ACCT`).

Firecracker boots the kernel file from the host, not one from the image, and the CI kernel has no loadable modules: what a guest needs must be built in. `make kernel KERNEL=k8s` builds `kernels/vmlinux-6.1-k8s` for a firm's cluster: the same 6.1 source (checked against kernel.org's checksum) and config, with `config/kernel/k8s.config` merged over it (the netfilter matches kube-proxy uses, ipset, nftables, VXLAN, IPVS). It builds in a container and fails if an option doesn't survive `olddefconfig`. Another fragment in `config/kernel/` builds another kernel the same way.

Versions are pinned in the Makefile: `CLAUDE_VERSION` for the office image (Claude Code's auto-update is off in sessions, so a run uses the version the image was built with), `PI_VERSION`, `PLAYWRIGHT_VERSION`, `NODE_VERSION`, `KUBECTL_VERSION`, and `K3S_VERSION` with the checksums of k3s and its images. Update by changing them and rebuilding.

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
- Sources: `crates/zapper/src/image.rs`, `crates/zapper/src/daemon/vm.rs`, `scripts/fetch-deps.sh`, `scripts/build-kernel.sh`, `config/kernel/`
