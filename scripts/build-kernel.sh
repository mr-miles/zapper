#!/bin/sh
# Builds a guest kernel: the Firecracker CI config of the kernel `make deps`
# fetched, with a fragment from config/kernel/ merged over it. Firecracker
# boots one kernel file from the host and that kernel has no loadable
# modules, so what a guest needs must be built in.
#
#   scripts/build-kernel.sh k8s      # → ~/.local/share/zapper/kernels/vmlinux-6.1-k8s
#
# Builds in a container (podman, else docker, or $ZAPPER_OCI), so the host
# needs no toolchain. The source is checked against kernel.org's checksum.
set -eu

NAME=${1:-k8s}
VERSION=6.1.155
SHA256=c29387aeee085fbcbd91236224b9df805063bac43615e75cea2c6b29604a5c73
if [ -n "${ZAPPER_OCI:-}" ]; then
    OCI=$ZAPPER_OCI
elif command -v podman >/dev/null 2>&1; then
    OCI=podman
else
    OCI=docker
fi
# docker's containers write as root: the kernel is given back to you. (Under
# rootless podman, the container's root is you already.)
OWNER=
[ "$(basename "$OCI")" = docker ] && OWNER=$(id -u):$(id -g)
DATA=${ZAPPER_DATA:-$HOME/.local/share/zapper}
HERE=$(cd "$(dirname "$0")/.." && pwd)
FRAGMENT=$HERE/config/kernel/$NAME.config
BASE=$DATA/kernels/vmlinux-6.1.config
OUT=$DATA/kernels
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/zapper/kernel

[ -f "$FRAGMENT" ] || { echo "no fragment $FRAGMENT" >&2; exit 1; }
[ -f "$BASE" ] || { echo "no base config $BASE; run make deps" >&2; exit 1; }
grep -q "Linux/x86 $VERSION " "$BASE" || { echo "$BASE is not for $VERSION" >&2; exit 1; }
mkdir -p "$CACHE" "$OUT"

TARBALL=$CACHE/linux-$VERSION.tar.xz
if [ ! -s "$TARBALL" ]; then
    echo "downloading linux-$VERSION"
    curl -fsSL -o "$TARBALL.tmp" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$VERSION.tar.xz"
    mv "$TARBALL.tmp" "$TARBALL"
fi
echo "$SHA256  $TARBALL" | sha256sum -c --quiet || { echo "checksum mismatch for $TARBALL" >&2; rm -f "$TARBALL"; exit 1; }

echo "building vmlinux-6.1-$NAME in a container"
"$OCI" run --rm \
    -v "$CACHE:/cache:ro" -v "$FRAGMENT:/fragment.config:ro" -v "$BASE:/base.config:ro" -v "$OUT:/out" \
    -e VERSION="$VERSION" -e NAME="$NAME" -e OWNER="$OWNER" \
    docker.io/library/ubuntu:24.04 sh -euc '
        apt-get update -qq >/dev/null
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential flex bison bc libelf-dev libssl-dev xz-utils cpio python3 >/dev/null
        cd /tmp && tar -xf /cache/linux-$VERSION.tar.xz && cd linux-$VERSION
        cp /base.config .config
        scripts/kconfig/merge_config.sh -m .config /fragment.config >/dev/null
        make olddefconfig >/dev/null
        missing=""
        for opt in $(grep -E "^CONFIG_[A-Z0-9_]+=y" /fragment.config | cut -d= -f1); do
            grep -q "^$opt=y" .config || missing="$missing $opt"
        done
        if [ -n "$missing" ]; then echo "options that did not survive olddefconfig:$missing" >&2; exit 1; fi
        make -j"$(nproc)" vmlinux >/tmp/build.log 2>&1 || { tail -40 /tmp/build.log; exit 1; }
        cp vmlinux /out/vmlinux-6.1-$NAME.tmp
        cp .config /out/vmlinux-6.1-$NAME.config
        [ -z "$OWNER" ] || chown "$OWNER" /out/vmlinux-6.1-$NAME.tmp /out/vmlinux-6.1-$NAME.config
        mv /out/vmlinux-6.1-$NAME.tmp /out/vmlinux-6.1-$NAME
    '
echo "kernel vmlinux-6.1-$NAME: installed in $OUT"
