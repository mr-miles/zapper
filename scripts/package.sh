#!/bin/sh
# Packages a release build for running The Office without the source: its
# binaries, the static guest `office` command its images install, and an
# installer. `zapper-office init` writes the example configs and the
# workers' agents and skills.
#
#   make build && scripts/package.sh v0.1.0    # dist/zapper-v0.1.0-x86_64-linux.tar.gz
set -eu

VERSION=${1:?usage: $0 VERSION}
HERE=$(cd "$(dirname "$0")/.." && pwd)
ARCH=$(uname -m)
NAME=zapper-$VERSION-$ARCH-linux
OUT=$HERE/dist
ROOT=$OUT/$NAME

# zapper-office is The Office (serve, the firm CLI, image builds, init);
# zapper-inspect reads firms' past runs; zapper-agent is the sandboxes'
# `office` command; zapper-warden is zapperclaw's host side, and zapperclaw
# the office its sandbox's image (zapperclaw-office) installs.
BINS="zapper-office zapper-workbench zapper-inspect zapper-agent zapper-warden zapperclaw"
# zapper-keeper runs root-owned as its own user (scripts/install-keeper.sh
# sets it up), so it goes in libexec/, apart from what install.sh installs.
for b in $BINS zapper-keeper; do
    [ -x "$HERE/target/release/$b" ] || { echo "no target/release/$b: run make build first" >&2; exit 1; }
done
# The images copy the guest command into any base: it must not need a
# dynamic loader.
if readelf -l "$HERE/target/release/zapper-agent" | grep -q INTERP; then
    echo "target/release/zapper-agent is not static" >&2
    exit 1
fi

rm -rf "$ROOT"
mkdir -p "$ROOT/bin"
for b in $BINS; do
    install -m 0755 "$HERE/target/release/$b" "$ROOT/bin/"
done
mkdir -p "$ROOT/libexec" "$ROOT/scripts"
install -m 0755 "$HERE/target/release/zapper-keeper" "$ROOT/libexec/"
install -m 0755 "$HERE/scripts/install-keeper.sh" "$ROOT/scripts/"
cp "$HERE/LICENSE" "$ROOT/"

cat > "$ROOT/install.sh" <<'INSTALL'
#!/bin/sh
# Installs zapper-office, zapper-workbench, zapper-inspect, zapper-agent,
# zapper-warden and zapperclaw into ~/.local/bin
# (set PREFIX for another place), then runs `zapper-office init`, which
# writes the example configs into ~/.config/zapper and the workers' agents
# and skills beside them, keeping what is there.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX=${PREFIX:-$HOME/.local}
CONF=$HOME/.config/zapper

install -d "$PREFIX/bin"
for b in "$HERE"/bin/*; do
    install -m 0755 "$b" "$PREFIX/bin/"
    echo "installed $(basename "$b") in $PREFIX/bin"
done

install -d -m 0700 "$CONF"
# ZAPPER_CONFIG pins the target to this home, even when run from a checkout.
ZAPPER_CONFIG="$CONF/zapper.yaml" "$PREFIX/bin/zapper-office" init

case ":$PATH:" in
*":$PREFIX/bin:"*) ;;
*) echo "add $PREFIX/bin to PATH" ;;
esac
echo
echo "Next: point $CONF/openshell.yaml at your OpenShell gateway, build the"
echo "workers' image (zapper-office image office --agent $PREFIX/bin/zapper-agent),"
echo "and start the office: zapper-office serve (it prints the dashboard's URL)."
echo
echo "zapperclaw, an office in a sandbox of its own: build its image with the"
echo "installed zapperclaw (zapper-office image zapperclaw-office --arg"
echo "zapperclaw=$PREFIX/bin/zapperclaw), then run zapper-warden with a"
echo "warden.yaml and a zapperclaw config, as config/warden.yaml and"
echo "config/zapperclaw in the source (docs/warden.md)."
echo
echo "zapper-keeper, an append-only record (optional), is in $HERE/libexec;"
echo "sudo $HERE/scripts/install-keeper.sh sets it up."
INSTALL
chmod 0755 "$ROOT/install.sh"

tar -C "$OUT" -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "$OUT/$NAME.tar.gz"
