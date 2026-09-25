#!/bin/bash
# card #314: build the release tarballs of ANY commit - macOS included - on a Linux docker host, so
# `run.sh local --dist DIR` (and `run.sh container --dist DIR`) can judge binaries that are not
# released yet. Without this the macOS half of the gate could only ever test the LAST RELEASE.
#
#   scripts/e2e/dist.sh --host H --ref REF [--target T]... [--out DIR]
#
#   --host H     ssh name of a Linux box with docker (or $LSS_E2E_HOST). Nothing compiles here:
#                this seat never runs cargo (see the repository README, "Building").
#   --ref REF    the commit to build (git archive of it - never the live working tree)
#   --target T   a release triple; repeat for several. Default: all four release.yml builds:
#                x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin
#   --out DIR    where the tarballs land (default ./dist-<sha12>)
#   --layout-only  print what each tarball of REF would hold, and from where, then stop: no host,
#                no build (card #318; the test drives it)
#
# Output: DIR/lss-<triple>.tar.gz + .sha256 for each target, packed EXACTLY as THAT REF'S OWN
# release.yml packs a release (card #318): the files its `tar -C "$STAGE" -czf "$ASSET" ...` line
# names, at the tarball root - lss and lss-collector from the build, anything else (lss-notify.sh
# since #81's release.yml) copied from the ref with the mode its `install -m` line gives. A ref
# with no release.yml at all gets lss + lss-collector (what every release so far held), said aloud.
# A `shasum -a 256` line beside each, so what the gate installs has the shape a stranger's download has.
#
# How: cargo-zigbuild in the public image ghcr.io/rust-cross/cargo-zigbuild (zig as the linker,
# the macOS 11.3 SDK at $SDKROOT, the apple and musl rust targets preinstalled), `--locked`.
# The remote build dir and its cargo target volume are per-sha and REMOVED at the end; the cargo
# registry volume (`cargo-registry`, the one scripts/remote-build.sh uses) is shared and kept.
#
# Exit: 0 every tarball built · 1 a build failed · 2 bad arguments.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
IMAGE="${LSS_ZIGBUILD_IMAGE:-ghcr.io/rust-cross/cargo-zigbuild:latest}"
HOST="${LSS_E2E_HOST:-}"; REF=""; OUT=""; TARGETS=(); LAYOUT_ONLY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --host) HOST="${2:?--host needs a value}"; shift 2 ;;
        --ref) REF="${2:?--ref needs a value}"; shift 2 ;;
        --target) TARGETS+=("${2:?--target needs a value}"); shift 2 ;;
        --out) OUT="${2:?--out needs a value}"; shift 2 ;;
        --layout-only) LAYOUT_ONLY=1; shift ;;
        -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
        *) echo "dist.sh: unknown option $1" >&2; exit 2 ;;
    esac
done
[ -n "$HOST" ] || [ "$LAYOUT_ONLY" = 1 ] || { echo "dist.sh: --host <ssh name of a Linux box with docker> (or LSS_E2E_HOST)" >&2; exit 2; }
[ -n "$REF" ] || { echo "dist.sh: --ref <commit> - the gate judges a commit, never a live tree" >&2; exit 2; }
SHA="$(git -C "$REPO" rev-parse --verify --quiet "$REF^{commit}")" || { echo "dist.sh: --ref '$REF' is not a commit here" >&2; exit 2; }
[ ${#TARGETS[@]} -gt 0 ] || TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin)
for t in "${TARGETS[@]}"; do
    case "$t" in x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|aarch64-apple-darwin|x86_64-apple-darwin) ;;
        *) echo "dist.sh: $t is not a release triple" >&2; exit 2 ;; esac
done
SHORT="${SHA:0:12}"
RDIR="lss-dist-$SHORT"
VOL="lss-dist-target-$SHORT"

# the commit, not the working tree; and every extra packed file comes from the same commit
PIN="$(mktemp -d "${TMPDIR:-/tmp}/lss-dist-XXXXXX")"
REMOTE_USED=0
cleanup() {
    rm -rf "$PIN"
    [ "$REMOTE_USED" = 1 ] || return 0
    ssh "$HOST" "docker run --rm -v \$HOME/$RDIR:/io $IMAGE rm -rf /io/out >/dev/null 2>&1; rm -rf \$HOME/$RDIR; docker volume rm -f $VOL >/dev/null 2>&1; true" || true
}
trap cleanup EXIT
git -C "$REPO" archive --format=tar "$SHA" | tar -x -C "$PIN"

# card #318: WHAT goes in a tarball is the ref's own release.yml's business. LAYOUT holds one line
# per file: "<name> build" (lss, lss-collector: from this build) or "<name> <mode> <path in the ref>".
REL="$PIN/.github/workflows/release.yml"
LAYOUT=""
if [ ! -f "$REL" ]; then
    echo "dist.sh: $SHORT has no .github/workflows/release.yml - packing lss + lss-collector (what every release so far held)" >&2
    LAYOUT=$'lss build\nlss-collector build'
else
    files="$(sed -n 's/.*tar -C "\$STAGE" -czf "\$ASSET" \(.*\)$/\1/p' "$REL" | head -1)"
    [ -n "$files" ] || { echo "dist.sh: $SHORT's release.yml has no 'tar -C \"\$STAGE\" -czf \"\$ASSET\" ...' line - cannot tell what its tarballs hold" >&2; exit 1; }
    for f in $files; do
        case "$f" in
            lss|lss-collector) LAYOUT+="$f build"$'\n' ;;
            *)
                src="$(sed -n "s#.*install -m \([0-7]*\) \([^ ]*\) \"\\\$STAGE/$f\".*#\1 \2#p" "$REL" | head -1)"
                [ -n "$src" ] || { echo "dist.sh: $SHORT's release.yml packs '$f' but has no 'install -m MODE SRC \"\$STAGE/$f\"' line saying where it comes from" >&2; exit 1; }
                [ -f "$PIN/${src#* }" ] || { echo "dist.sh: $SHORT's release.yml packs '$f' from ${src#* }, which is not in $SHORT" >&2; exit 1; }
                LAYOUT+="$f $src"$'\n' ;;
        esac
    done
fi
LAYOUT="$(printf '%s\n' "$LAYOUT" | sed '/^$/d')"
if [ "$LAYOUT_ONLY" = 1 ]; then
    printf '%s\n' "$LAYOUT" | sed 's/^/layout: /'
    exit 0
fi

OUT="${OUT:-$PWD/dist-$SHORT}"
mkdir -p "$OUT"
echo "dist.sh: building $SHA for ${TARGETS[*]} on $HOST ($IMAGE) -> $OUT"
echo "dist.sh: each tarball holds, as $SHORT's release.yml packs it: $(printf '%s\n' "$LAYOUT" | cut -d' ' -f1 | tr '\n' ' ')"
# the build itself, as a script that travels with the tree (no quoting through ssh)
cat >"$PIN/.dist-build.sh" <<'BUILD'
set -e
cd /io
args=""; for t in "$@"; do args="$args --target $t"; done
# the image's rustc may lag the repository's: try it first, else bring in `stable`
if ! cargo zigbuild --release --locked $args -p lss -p lss-collector 2>/tmp/first.log; then
    if grep -qiE "requires rustc|rustc [0-9.]+ is not supported|edition2024|feature .* is required" /tmp/first.log; then
        echo "dist.sh: the image's rustc ($(rustc --version)) is too old for this tree - using stable" >&2
        rustup toolchain install stable --profile minimal >/dev/null 2>&1
        for t in "$@"; do rustup target add --toolchain stable "$t" >/dev/null 2>&1; done
        cargo +stable zigbuild --release --locked $args -p lss -p lss-collector
    else
        tail -40 /tmp/first.log >&2; exit 1
    fi
fi
for t in "$@"; do mkdir -p "/io/out/$t"; cp "/io/target/$t/release/lss" "/io/target/$t/release/lss-collector" "/io/out/$t/"; done
chmod -R a+rwX /io/out
BUILD
REMOTE_USED=1
rsync -a --delete "$PIN/" "$HOST:$RDIR/"
ssh "$HOST" "docker run --rm -v \$HOME/$RDIR:/io -v $VOL:/io/target -v cargo-registry:/usr/local/cargo/registry -w /io $IMAGE bash /io/.dist-build.sh ${TARGETS[*]}" \
    || { echo "dist.sh: the build failed on $HOST" >&2; exit 1; }

# pack each triple exactly like release.yml
for t in "${TARGETS[@]}"; do
    stage="$PIN/stage-$t"; mkdir -p "$stage"
    scp -q "$HOST:$RDIR/out/$t/lss" "$HOST:$RDIR/out/$t/lss-collector" "$stage/"
    chmod 0755 "$stage/lss" "$stage/lss-collector"
    names=()
    while read -r name mode path; do
        names+=("$name")
        [ "$mode" = build ] || install -m "$mode" "$PIN/$path" "$stage/$name"
    done <<<"$LAYOUT"
    asset="lss-$t.tar.gz"
    tar -C "$stage" -czf "$OUT/$asset" "${names[@]}"
    (cd "$OUT" && (sha256sum "$asset" 2>/dev/null || shasum -a 256 "$asset") >"$asset.sha256")
    echo "dist.sh: $OUT/$asset  $(cut -c1-16 "$OUT/$asset.sha256")...  ($(file -b "$stage/lss-collector" | cut -d, -f1-2))"
done
echo "dist.sh: done - run.sh local|container --ref $SHORT --dist $OUT"
