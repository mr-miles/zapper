#!/bin/sh
# Sets up zapper-keeper, which keeps zapper's record (firms' journals and
# transcripts, sessions' audit logs and terminal recordings) as its own user,
# so the user who runs zapper can add to the record and never change it.
#
#   make build && sudo scripts/install-keeper.sh [USER]
#   sudo scripts/install-keeper.sh [USER]    # from a release's tarball
#
# USER is who runs zapper (default: the user who ran sudo). Needs root once:
# it creates the system user zapper-keeper, installs the binary root-owned in
# /usr/local/libexec (so USER can't replace it), the record directory and a
# confined systemd service. Then add the printed lines to zapper's config and
# restart the daemon. Undo: systemctl disable --now zapper-keeper; the record
# in /var/lib/zapper-keeper stays until you remove it.
set -eu

[ "$(id -u)" = 0 ] || { echo "run with sudo: it creates a system user and a service" >&2; exit 1; }
WRITER=${1:-${SUDO_USER:-}}
[ -n "$WRITER" ] || { echo "usage: sudo $0 USER (who runs zapper)" >&2; exit 1; }
UID_W=$(id -u "$WRITER")
GID_W=$(id -g "$WRITER")
GROUP_W=$(id -gn "$WRITER")
HERE=$(cd "$(dirname "$0")/.." && pwd)
# A checkout's build, or a release's libexec/.
BIN="$HERE/target/release/zapper-keeper"
[ -x "$BIN" ] || BIN="$HERE/libexec/zapper-keeper"
[ -x "$BIN" ] || { echo "no $HERE/target/release/zapper-keeper: run make build first" >&2; exit 1; }

# The keeper's own user, in the writer's group so it can give the record
# that group: the writer reads it, nobody else does.
id zapper-keeper >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /var/lib/zapper-keeper --shell /usr/sbin/nologin zapper-keeper
usermod -a -G "$GROUP_W" zapper-keeper

install -d -o root -g root -m 0755 /usr/local/libexec
install -o root -g root -m 0755 "$BIN" /usr/local/libexec/zapper-keeper
# Traversable, not listable: the key inside is the keeper's alone.
install -d -o zapper-keeper -g zapper-keeper -m 0711 /var/lib/zapper-keeper
install -d -o zapper-keeper -g "$GID_W" -m 0750 /var/lib/zapper-keeper/record
install -d -o zapper-keeper -g "$GID_W" -m 0750 "/var/lib/zapper-keeper/record/$UID_W"

cat > /etc/systemd/system/zapper-keeper.service <<EOF
[Unit]
Description=zapper-keeper: keeps zapper's record append-only for $WRITER

[Service]
User=zapper-keeper
Group=zapper-keeper
SupplementaryGroups=$GROUP_W
ExecStart=/usr/local/libexec/zapper-keeper serve --socket /run/zapper-keeper/keeper.sock --dir /var/lib/zapper-keeper/record/$UID_W --key /var/lib/zapper-keeper/keeper.key --writer-uid $UID_W --group $GID_W --ui 127.0.0.1:7879
RuntimeDirectory=zapper-keeper
RuntimeDirectoryMode=0755
StateDirectory=zapper-keeper
StateDirectoryMode=0711
Restart=always
RestartSec=1
# Confined: it reads and writes its own directory and nothing else.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
CapabilityBoundingSet=
AmbientCapabilities=
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@mount @swap @reboot @module @raw-io @clock @debug @obsolete
ReadWritePaths=/var/lib/zapper-keeper /run/zapper-keeper
UMask=0027

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now zapper-keeper
sleep 1
systemctl --no-pager --lines=3 status zapper-keeper || true

cat <<EOF

zapper-keeper is running. Add this to zapper's config (daemon:), then restart
the daemon (\`zapper serve\`); \`zapper check\` confirms it:

  daemon:
    keeper:
      socket: /run/zapper-keeper/keeper.sock
      dir: /var/lib/zapper-keeper/record/$UID_W

The keeper's UI: the URL with its token is in /var/lib/zapper-keeper/record/$UID_W/keeper-ui
(readable by $WRITER).
EOF
