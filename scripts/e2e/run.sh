#!/bin/bash
# The install e2e - the definition of done for the installer + setup/cost wizards (card #296).
#
#   scripts/e2e/run.sh local   [options]   this Mac (or Linux box), under a throwaway HOME
#   scripts/e2e/run.sh container --host H [options]
#                                          a fresh ubuntu:24.04 container (no Rust) on the docker host H
#                                          (an ssh name; or set LSS_E2E_HOST - there is no default)
#
# Options:
#   --release vX.Y.Z   the release tarballs to install (downloaded with `gh release download`; default: latest)
#   --dist DIR         use the lss-<triple>.tar.gz (+ .sha256) already in DIR instead (e.g. the output of
#                      `scripts/e2e/dist.sh --host H --ref <sha>`, which builds any commit, macOS included)
#   --ref REF          the install.sh to test, from this git ref (default: the working tree's install.sh)
#   --flavor F         sglang (default) | vllm | ollama | llamacpp | all
#   --only "S1 S3"     run a subset of the scenarios
#   --zip ZIP          the ZIP the wizard is given (default 94103; its plausible-rate range is California's)
#   --out DIR          keep the logs here (default: a fresh temp dir; printed at the end)
#   --host H           the ssh name of the docker host for `container` (or $LSS_E2E_HOST; required)
#
# Exit: 0 PASS (every assertion) · 1 FAIL (the FAIL lines say which, and why) · 2 the harness could not run.
# What the scenarios assert: scripts/e2e/README.md.
set -euo pipefail
E2E="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$E2E/../.." && pwd)"
MODE="${1:-}"; shift || true
RELEASE=""; DIST=""; REF=""; FLAVOR="sglang"; ONLY=""; ZIPC="94103"; OUT=""; DHOST="${LSS_E2E_HOST:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --release) RELEASE="${2:?}"; shift 2 ;;
        --dist) DIST="${2:?}"; shift 2 ;;
        --ref) REF="${2:?}"; shift 2 ;;
        --flavor) FLAVOR="${2:?}"; shift 2 ;;
        --only) ONLY="${2:?}"; shift 2 ;;
        --zip) ZIPC="${2:?}"; shift 2 ;;
        --out) OUT="${2:?}"; shift 2 ;;
        --host) DHOST="${2:?}"; shift 2 ;;
        -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "run.sh: unknown option $1" >&2; exit 2 ;;
    esac
done
case "$MODE" in local|container) ;; *) sed -n '2,22p' "$0" >&2; exit 2 ;; esac
if [ "$MODE" = container ] && [ -z "$DHOST" ]; then echo "run.sh: container mode needs --host <ssh name of a Linux box with docker> (or LSS_E2E_HOST)" >&2; exit 2; fi

OUT="${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/lss-e2e-out.XXXXXX")}"
mkdir -p "$OUT"
REL="$OUT/release"; rm -rf "$REL"; mkdir -p "$REL"

# ---- install.sh under test
if [ -n "$REF" ]; then
    git -C "$REPO" show "$REF:install.sh" >"$REL/install.sh"
    SRC_DESC="install.sh @ $(git -C "$REPO" rev-parse --short "$REF")"
else
    cp "$REPO/install.sh" "$REL/install.sh"
    SRC_DESC="install.sh from the working tree @ $(git -C "$REPO" rev-parse --short HEAD)$(git -C "$REPO" diff --quiet HEAD -- install.sh || echo ' + local edits')"
fi

# ---- the tarballs: which triples this run needs
case "$MODE" in
    container) TRIPLES="x86_64-unknown-linux-musl" ;;
    local)
        case "$(uname -s)-$(uname -m)" in
            Darwin-arm64) TRIPLES="aarch64-apple-darwin" ;; Darwin-x86_64) TRIPLES="x86_64-apple-darwin" ;;
            Linux-x86_64) TRIPLES="x86_64-unknown-linux-musl" ;; Linux-aarch64) TRIPLES="aarch64-unknown-linux-musl" ;;
            *) echo "run.sh: no release triple for $(uname -sm)" >&2; exit 2 ;;
        esac ;;
esac
for t in $TRIPLES; do
    if [ -n "$DIST" ]; then
        cp "$DIST/lss-$t.tar.gz" "$DIST/lss-$t.tar.gz.sha256" "$REL/" || { echo "run.sh: $DIST has no lss-$t.tar.gz(.sha256)" >&2; exit 2; }
        BIN_DESC="binaries from $DIST"
    else
        tag="${RELEASE:-$(cd "$REPO" && gh release view --json tagName -q .tagName)}"
        (cd "$REPO" && gh release download "$tag" -p "lss-$t.tar.gz" -p "lss-$t.tar.gz.sha256" -D "$REL" --clobber) >/dev/null \
            || { echo "run.sh: could not download lss-$t.tar.gz from release $tag" >&2; exit 2; }
        BIN_DESC="binaries from release $tag"
    fi
done
echo "e2e: $MODE · $SRC_DESC · $BIN_DESC · logs in $OUT"

FLAVORS="$FLAVOR"; [ "$FLAVOR" = all ] && FLAVORS="sglang vllm ollama llamacpp"
rc=0
for f in $FLAVORS; do
    echo; echo "######## flavor $f"
    case "$MODE" in
    local)
        # a stranger's machine: no cargo on PATH (the scenario refuses to run with one)
        E2E_RELEASE_DIR="$REL" E2E_OUT="$OUT/$f" E2E_FLAVOR="$f" E2E_ZIP="$ZIPC" E2E_ONLY="${ONLY:-S0 S1 S2 S3 S4}" \
            PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" bash "$E2E/scenario.sh" || rc=$?
        ;;
    container)
        # one fresh directory per run on the docker host; the container is --rm, the dir is removed after
        rdir="lss-e2e-$(date +%Y%m%d-%H%M%S)-$$"
        ssh "$DHOST" "mkdir -p ~/$rdir/e2e ~/$rdir/release"
        scp -q -r "$E2E/." "$DHOST:$rdir/e2e/"
        scp -q "$REL"/* "$DHOST:$rdir/release/"
        # the container: ubuntu:24.04, and only what a stranger would apt-get - curl, python3, expect. No Rust.
        set +e
        ssh "$DHOST" "docker run --rm --name $rdir -v \$HOME/$rdir:/e2e-run ubuntu:24.04 bash -c '
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq >/dev/null && apt-get install -y -qq curl python3 expect procps ca-certificates >/dev/null 2>&1 || { echo apt-get failed >&2; exit 2; }
            useradd -m e2e && chown -R e2e /e2e-run
            su e2e -c \"cd ~ && E2E_RELEASE_DIR=/e2e-run/release E2E_OUT=/e2e-run/out E2E_FLAVOR=$f E2E_ZIP=$ZIPC E2E_ONLY=\\\"${ONLY:-S0 S1 S2 S3 S4}\\\" bash /e2e-run/e2e/scenario.sh\"
        '"
        frc=$?
        set -e
        mkdir -p "$OUT/$f"; scp -q -r "$DHOST:$rdir/out/." "$OUT/$f/" 2>/dev/null || true
        # the container's user owns what it wrote: remove it from inside a container, then the dir
        ssh "$DHOST" "docker run --rm -v \$HOME/$rdir:/x ubuntu:24.04 sh -c 'rm -rf /x/* /x/.[!.]*' >/dev/null 2>&1; rmdir ~/$rdir"
        [ "$frc" = 0 ] || rc=$frc
        ;;
    esac
done
echo; echo "e2e logs: $OUT"
exit "$rc"
