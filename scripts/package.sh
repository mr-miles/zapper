#!/bin/sh
# Packages a release build for running zapper without the source: the
# binaries, the deps script, the image recipes as scripts, and an installer.
# `zapper init` writes the example config and the workers' skills.
#
#   make build && scripts/package.sh v0.1.0    # dist/zapper-v0.1.0-x86_64-linux.tar.gz
#   OFFICE=0 make build && OFFICE=0 scripts/package.sh v0.1.0    # without the office
set -eu

VERSION=${1:?usage: $0 VERSION}
HERE=$(cd "$(dirname "$0")/.." && pwd)
ARCH=$(uname -m)
NAME=zapper-$VERSION-$ARCH-linux
OUT=$HERE/dist
ROOT=$OUT/$NAME

# As the Makefile builds them: zapper-inspect reads firms' runs.
BINS="zapper zapper-agent"
[ "${OFFICE:-1}" = 0 ] || BINS="$BINS zapper-inspect"
for b in $BINS; do
    [ -x "$HERE/target/release/$b" ] || { echo "no target/release/$b: run make build first" >&2; exit 1; }
done
# The agent runs as PID 1 in any image: it must not need a dynamic loader.
if readelf -l "$HERE/target/release/zapper-agent" | grep -q INTERP; then
    echo "target/release/zapper-agent is not static" >&2
    exit 1
fi

rm -rf "$ROOT"
mkdir -p "$ROOT/bin" "$ROOT/images" "$ROOT/scripts"
for b in $BINS; do
    install -m 0755 "$HERE/target/release/$b" "$ROOT/bin/"
done
install -m 0755 "$HERE/scripts/fetch-deps.sh" "$ROOT/scripts/"
cp "$HERE/LICENSE" "$ROOT/"

# The Makefile's image recipes, one script each, run with the installed zapper.
for image in $(sed -n 's/^IMAGE_FROM_\([^ :]*\) *:=.*/\1/p' "$HERE/Makefile"); do
    # make would expand ${ZAPPER} itself: a placeholder goes through instead.
    # shellcheck disable=SC2016
    cmd=$(make -s -n -C "$HERE" image IMAGE="$image" ZAPPER=__ZAPPER__ |
        sed -n 's/^__ZAPPER__ image build /"${ZAPPER:-zapper}" image build /p')
    [ -n "$cmd" ] || { echo "no image build command for $image" >&2; exit 1; }
    printf '#!/bin/sh\n# The %s image, as `make image IMAGE=%s` builds it.\nset -eu\n%s\n' \
        "$image" "$image" "$cmd" > "$ROOT/images/$image.sh"
    chmod 0755 "$ROOT/images/$image.sh"
done

cat > "$ROOT/install.sh" <<'EOF'
#!/bin/sh
# Installs zapper, zapper-agent and (with the office) zapper-inspect into
# ~/.local/bin (set PREFIX for another place); runs `zapper init`, which writes the example
# config as ~/.config/zapper/zapper.yaml and, if zapper has the office, the
# workers' skills into ~/.config/zapper/skills, keeping what is there; then
# puts Firecracker and a guest kernel into ~/.local/share/zapper.
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
"$PREFIX/bin/zapper" --config "$CONF/zapper.yaml" init

"$HERE/scripts/fetch-deps.sh"

case ":$PATH:" in
*":$PREFIX/bin:"*) ;;
*) echo "add $PREFIX/bin to PATH" ;;
esac
echo
echo "Next, build images (needs podman or docker); the base profiles use the ubuntu one, zapper run -p claude the office one:"
for f in "$HERE"/images/*.sh; do echo "  $f"; done
echo "then: zapper check (each line ok, or the fix), and zapper serve (prints the UI URL)"
EOF
chmod 0755 "$ROOT/install.sh"

tar -C "$OUT" -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "$OUT/$NAME.tar.gz"
