#!/bin/sh
# The OpenShell image for a zapper image (docs/openshell-migration.md, phase 1):
# localhost/zapper-openshell-IMAGE, from localhost/zapper-image-IMAGE (which
# `zapper image build IMAGE` / `make image IMAGE=IMAGE` leaves in podman).
#
# OpenShell's microVM driver runs every command as the image's uid 1000 (or
# its `sandbox` account), never root, and starts in the image's WORKDIR. So
# this adds the profile's user as uid 1000 with its home, makes /work theirs,
# and puts in the files the office writes outside the user's reach (the
# `browse` command), which the office then finds already there; and the
# static `zapper-agent` as `office` (the workers' CLI and stop hook, which
# reach the office edge over HTTP), which zapper's guest agent installed at
# boot. Run `make build` first: it is target/release/zapper-agent.
#
#   scripts/openshell-image.sh office-fake          # user `agent`, as the office's profiles
#   scripts/openshell-image.sh office agent
set -eu
image=${1:?usage: scripts/openshell-image.sh IMAGE [USER]}
user=${2:-agent}
engine=${ZAPPER_OCI:-podman}
here=$(cd "$(dirname "$0")/.." && pwd)
from=localhost/zapper-image-$image
tag=localhost/zapper-openshell-$image

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
cp "$here/crates/zapper-office/src/browse.js" "$dir/browse"
agent=${ZAPPER_AGENT:-$here/target/release/zapper-agent}
[ -x "$agent" ] || { echo "no $agent: run make build first (or set ZAPPER_AGENT)" >&2; exit 1; }
cp "$agent" "$dir/zapper-agent"
cat > "$dir/Containerfile" <<EOF
FROM $from
# Ubuntu's images have an \`ubuntu\` account at uid 1000: the user takes its place.
RUN old=\$(getent passwd 1000 | cut -d: -f1); if [ -n "\$old" ] && [ "\$old" != "$user" ]; then userdel -r "\$old"; fi; \\
    id -u $user >/dev/null 2>&1 || useradd -m -u 1000 -U -s /bin/bash $user; \\
    mkdir -p /work && chown $user:$user /work
COPY --chmod=755 browse /usr/local/bin/browse
COPY --chmod=755 zapper-agent /usr/local/bin/zapper-agent
RUN ln -sf zapper-agent /usr/local/bin/office
ENV HOME=/home/$user
WORKDIR /work
EOF
"$engine" build -t "$tag" "$dir"
echo "$tag"
