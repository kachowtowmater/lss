#!/bin/bash
# Build and test on a REMOTE Linux host that has docker (typically the GPU box) when this machine
# cannot: sync the repo there and run cargo inside the build container.
#   LSS_BUILD_HOST=<ssh host> scripts/remote-build.sh test      cargo test --workspace, TWICE, under
#                                     two NON-UTC timezones (see "TIMEZONE" below)
#   scripts/remote-build.sh clippy    cargo clippy --all-targets -- -D warnings
#   scripts/remote-build.sh build     static musl release build of lss-collector (+ lss) → ~/lss-build/dist/
#   scripts/remote-build.sh bless     regenerate fixtures/*_golden.json (status, series, rules, gateway) and the overview
#                                     renders in fixtures/renders/, and copy them back
#
# VERIFYING A PINNED TREE (card #202) - use this, not the live checkout, whenever the result is
# going to be quoted as a verdict:
#   scripts/remote-build.sh test --ref <sha>        (or LSS_BUILD_REF=<sha>)
# It syncs `git archive <sha>` instead of the working tree, so the thing tested is a COMMIT and
# not "whatever the shared checkout happened to hold while rsync was running". WHY: on 2026-09-22
# a verifier syncing from the live shared checkout watched origin/main move b4a4ebd -> cc9a729
# MID-RUN. It reported "611 passed"; twenty minutes later one test failed 5/5 deterministically
# on what looked like the same tree. Neither number was wrong - they were two different trees, and
# nothing in the output said so. A pinned run names the sha it is verifying, in its first line.
# With --ref and no explicit LSS_BUILD_DIR the remote dir is derived from the sha AND the run
# (lss-build-<sha12>-<cmd>-<pid>), so no two pinned runs ever land on top of each other. `bless`
# is refused with --ref: blessing writes fixtures BACK, and an archive of a past commit has nowhere
# to write them.
#
# CLEANUP (card #264): a pinned run REMOVES its remote dir and its cargo target volume when it ends
# - pass, fail or interrupt - because a target volume is GBs (125 leaked ones filled the build
# host's / to 100% on 2026-09-23). Every pinned run therefore compiles from a cold target (the
# cargo download cache, `cargo-registry`, is shared and kept).
#   --keep  (or LSS_BUILD_KEEP=1)   keep both; the run prints the command that removes them later.
#                                   With a fixed LSS_BUILD_DIR this is also how to keep a warm cache.
# `build` keeps the dir (its dist/ is the point) and removes only the volume. Never removed: a
# live (non --ref) run's dir, the shared default lss-build / lss-target, a volume the caller
# named with LSS_BUILD_TARGET_VOL, or a dir name that is not a plain name.
#
# TIMEZONE (card #232) - `test` runs the suite under a NON-UTC TZ, and by default twice.
# WHY: the build container had TZ empty, i.e. UTC, and under UTC local time IS UTC - so every
# "this is local midnight, not UTC midnight" assertion in this repo was vacuous. Measured on
# 2026-09-23 (card #226): the mutation `year_start in UTC instead of local` left
# `cargo test --workspace` FULLY GREEN in the container, while the same mutated tree under
# TZ=America/Los_Angeles failed cost_run::tests::year_start_is_local_midnight_of_january_first
# with `left: "01-01 16:00"  right: "01-01 00:00"`. The code and the test were both correct; only
# the environment was blind, and the project's own gate could not prove its central promise.
# The second pass uses a HALF-HOUR offset on purpose (+05:30): a zone that is a whole number of
# hours from UTC cannot catch an arithmetic error that happens to be hour-aligned, and it was also
# a day ahead of the first zone during this run, which exercises date rollover for free.
#      LSS_TEST_TZ   first pass  (default America/Los_Angeles)
#      LSS_TEST_TZ2  second pass (default Asia/Kolkata; set it EMPTY to run one pass only)
# Both passes must be green. Each prints the zone and the container's own `date` first, so a green
# run can never be mistaken for one that quietly ran in UTC.
#
# Env: LSS_BUILD_HOST (required)  the ssh host
#      LSS_BUILD_DIR              the remote directory (default lss-build, or lss-build-<sha> with --ref)
#      LSS_BUILD_SRC              the source tree to sync (default: this script's repository)
#      LSS_BUILD_REF              a git ref to verify instead of the working tree (same as --ref)
#      LSS_BUILD_TARGET_VOL       the cargo target volume (default: derived from LSS_BUILD_DIR)
#      LSS_BUILD_KEEP=1           with --ref: keep the remote dir and target volume (same as --keep)
set -euo pipefail
HOST="${LSS_BUILD_HOST:?set LSS_BUILD_HOST to the ssh name of the build host (a Linux box with docker)}"

# card #202 item 2: --ref is pulled out FIRST, because both argument loops below treat any
# non-flag word as the source-tree override - its value would otherwise be read as a path.
REF="${LSS_BUILD_REF:-}"
KEEP="${LSS_BUILD_KEEP:-0}"
ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --ref)   REF="${2:?--ref needs a git ref}"; shift 2 ;;
        --ref=*) REF="${1#--ref=}"; shift ;;
        --keep)  KEEP=1; shift ;;
        *)       ARGS+=("$1"); shift ;;
    esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}

REMOTE_DIR="${LSS_BUILD_DIR:-lss-build}"
# card #159: the source tree is where THIS SCRIPT lives (the main checkout) by default, but an
# isolated worktree's changes cannot be verified without an override - the old rsync silently
# overwrote any manual sync with --delete. LSS_BUILD_SRC (or the first argument) names the tree
# to sync; it must exist and look like this repo (a sanity check that catches a typo'd path
# before it wipes the remote tree).
# card #164 (verifier-2): if BOTH are set, the POSITIONAL ARGUMENT WINS over LSS_BUILD_SRC - the
# opposite of what a reader would likely guess (env usually reads as "the default", the
# argument as "an override of an override"). Stated here because nothing else says so.
REPO="${LSS_BUILD_SRC:-$(cd "$(dirname "$0")/.." && pwd)}"
SRC_ARG=""
for arg in "$@"; do
    case "$arg" in
        -*) ;;
        test|clippy|build|bless) ;;
        *)  if [ -z "$SRC_ARG" ]; then SRC_ARG="$arg"; fi ;;
    esac
done
if [ -n "$SRC_ARG" ] && [ -d "$SRC_ARG" ]; then
    REPO="$(cd "$SRC_ARG" && pwd)"
elif [ -n "$SRC_ARG" ]; then
    echo "remote-build: the source override does not exist: $SRC_ARG" >&2
    exit 2
fi
if [ ! -f "$REPO/Cargo.toml" ] || [ ! -f "$REPO/scripts/remote-build.sh" ]; then
    echo "remote-build: $REPO is not a plausible source tree (no Cargo.toml / scripts/remote-build.sh)" >&2
    exit 2
fi
CMD=""
for arg in "$@"; do
    case "$arg" in
        test|clippy|build|bless) CMD="$arg" ;;
    esac
done
CMD="${CMD:-test}"

# ------------------------------------------------------------------ card #202: WHAT is verified
# With --ref, the tree that gets synced is an ARCHIVE OF A COMMIT, extracted here and thrown away
# afterwards, so nothing another agent does to the shared checkout mid-run can reach it.
PIN_DIR=""
# card #264: a pinned (--ref) run removes what it made on the build host when it ends - pass, fail
# or interrupt - because a cargo target volume is GBs and 125 leaked ones filled the build host's
# / to 100% (2026-09-23). REMOTE_TOUCHED is set just before the first rsync, so a run refused
# before touching the host (a bad ref, a pinned bless) sends nothing at all.
REMOTE_TOUCHED=0
remote_cleanup() {
    [ -n "$SHA" ] && [ "$REMOTE_TOUCHED" = 1 ] || return 0
    # only what THIS run created: never the shared default dir/volume, never a volume the caller
    # named (LSS_BUILD_TARGET_VOL), never a dir name that is not a plain name (it goes to rm -rf)
    local rm_dir=1 rm_vol=1 said=""
    case "$REMOTE_DIR" in lss-build|.|..|*[!A-Za-z0-9._-]*|"") rm_dir=0 ;; esac
    { [ -n "${LSS_BUILD_TARGET_VOL:-}" ] || [ "$TARGET_VOL" = lss-target ]; } && rm_vol=0
    # build: dist/ in the dir is what the caller came for - keep the dir, drop the volume
    if [ "$CMD" = build ] && [ "$rm_dir" = 1 ]; then
        rm_dir=0
        said="; the binaries stay in $HOST:~/$REMOTE_DIR/dist (remove the dir when done: ssh $HOST 'rm -rf ~/$REMOTE_DIR')"
    fi
    # --keep keeps the FILES, never the process (verifier F3): this run's own container is stopped
    # either way, so an interrupted run cannot leave its suite running on the shared host
    [ "$KEEP" = 1 ] && { rm_dir=0; rm_vol=0; }
    # On the host: stop our own container; then, if anything is to go, refuse while ANY running
    # container still mounts the dir or the volume (verifier F1: two runs sharing one explicit
    # LSS_BUILD_DIR - the first to end deleted the dir under the other's running suite); then
    # remove, and report each result as it really happened (F2).
    # (read from a quoted heredoc, not a heredoc inside $( ): the Mac's bash 3.2 expands $vars in
    # the latter and dies under set -u - silently, since this runs from the EXIT trap)
    local script res
    IFS= read -r -d '' script <<'REMOTE' || true
run=$1; dir=$2; vol=$3; rm_dir=$4; rm_vol=$5
docker rm -f "$run" >/dev/null 2>&1
[ "$rm_dir" = 1 ] || [ "$rm_vol" = 1 ] || exit 0
busy=""
for id in $(docker ps -q 2>/dev/null); do
    m="$(docker inspect --format '{{.Name}}{{range .Mounts}}|{{.Source}}|{{.Name}}{{end}}|' "$id" 2>/dev/null)"
    case "$m" in *"|$HOME/$dir|"*|*"|$vol|"*) busy="$busy ${m%%|*}" ;; esac
done
if [ -n "$busy" ]; then echo "INUSE$busy"; exit 0; fi
if [ "$rm_vol" = 1 ]; then
    if docker volume rm "$vol" >/dev/null 2>&1; then echo VOL_REMOVED; else echo VOL_FAILED; fi
fi
if [ "$rm_dir" = 1 ]; then
    # files a build container wrote into the dir can be root's: empty it from a container first
    docker run --rm -v "$HOME/$dir:/x" lss-build:latest sh -c 'rm -rf /x/* /x/.[!.]* 2>/dev/null; true' >/dev/null 2>&1
    rm -rf "${HOME:?}/$dir" 2>/dev/null
    if [ -e "$HOME/$dir" ]; then echo DIR_FAILED; else echo DIR_REMOVED; fi
fi
REMOTE
    if ! res="$(printf '%s' "$script" | ssh "$HOST" bash -s -- "$RUN_NAME" "$REMOTE_DIR" "$TARGET_VOL" "$rm_dir" "$rm_vol")"; then
        # the cleanup itself could not run (the host rebooted, the network dropped): nothing is
        # known to be removed - say what may be left and how to remove it, never stay silent
        echo "remote-build: could not reach $HOST to clean up - its container, $HOST:~/$REMOTE_DIR and volume $TARGET_VOL may be left; once it answers: ssh $HOST 'docker rm -f $RUN_NAME; docker volume rm $TARGET_VOL; rm -rf ~/$REMOTE_DIR'" >&2
        return 0
    fi
    local removed="" failed=""
    case "$res" in
        *INUSE*)
            local who="${res#*INUSE}"; who="$(printf '%s' "$who" | head -n 1)"
            echo "remote-build: kept $HOST:~/$REMOTE_DIR and volume $TARGET_VOL - still in use by running container(s):${who} (another run with the same LSS_BUILD_DIR?). Remove them once it has ended: ssh $HOST 'docker volume rm $TARGET_VOL; rm -rf ~/$REMOTE_DIR'"
            return 0 ;;
    esac
    # (strings, not arrays: an empty array is 'unbound' under set -u in the Mac's bash 3.2)
    [[ "$res" == *DIR_REMOVED* ]] && removed="$HOST:~/$REMOTE_DIR"
    [[ "$res" == *VOL_REMOVED* ]] && removed="${removed:+$removed and }volume $TARGET_VOL"
    [[ "$res" == *DIR_FAILED* ]] && failed="$HOST:~/$REMOTE_DIR"
    [[ "$res" == *VOL_FAILED* ]] && failed="${failed:+$failed and }volume $TARGET_VOL"
    if [ "$KEEP" = 1 ]; then
        echo "remote-build: kept $HOST:~/$REMOTE_DIR and volume $TARGET_VOL (--keep; its container was stopped); remove them with: ssh $HOST 'docker volume rm $TARGET_VOL; rm -rf ~/$REMOTE_DIR'"
    fi
    [ -n "$removed" ] && echo "remote-build: removed $removed (pass --keep to keep them)$said"
    [ -z "$removed" ] && [ -n "$said" ] && [ "$KEEP" != 1 ] && echo "remote-build: kept the dir${said}"
    [ -n "$failed" ] && echo "remote-build: could not remove $failed - remove by hand: ssh $HOST 'docker volume rm $TARGET_VOL; rm -rf ~/$REMOTE_DIR'" >&2
    return 0
}
cleanup() { remote_cleanup; [ -n "$PIN_DIR" ] && rm -rf "$PIN_DIR"; return 0; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
SHA=""
if [ -n "$REF" ]; then
    git -C "$REPO" rev-parse --verify --quiet "$REF^{commit}" >/dev/null \
        || { echo "remote-build: --ref '$REF' is not a commit in $REPO" >&2; exit 2; }
    SHA="$(git -C "$REPO" rev-parse "$REF^{commit}")"
    if [ "$CMD" = "bless" ]; then
        echo "remote-build: 'bless' writes fixtures BACK into the source tree, and --ref is an archive of a past commit with nowhere to write. Bless from a working tree." >&2
        exit 2
    fi
    # card #264: one dir PER RUN (not per sha): a test and a clippy of one sha side by side would
    # otherwise share it, and the first to finish now deletes its dir - under the other one
    [ -n "${LSS_BUILD_DIR:-}" ] || REMOTE_DIR="lss-build-${SHA:0:12}-$CMD-$$"
    PIN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/lss-pin-XXXXXX")"
    git -C "$REPO" archive --format=tar "$SHA" | tar -x -C "$PIN_DIR"
    [ -f "$PIN_DIR/Cargo.toml" ] || { echo "remote-build: the archive of $SHA has no Cargo.toml" >&2; exit 2; }
    # card #238: `git archive` stamps every file with the COMMIT's time. This build dir's target
    # volume can already hold a binary built LATER from other source - a mutation run of the same
    # sha - and cargo, comparing mtimes, then judges the restored original "older than the build"
    # and runs the STALE MUTATED binary while the tree on disk is clean. Measured on the build host:
    # restore the original from the archive -> the mutated test still fails; touch it -> it
    # passes. Stamping the pinned tree "now" makes cargo rebuild whatever it holds (workspace
    # crates only - the dependency cache is untouched).
    find "$PIN_DIR" -type f -exec touch {} +
    REPO="$PIN_DIR"
fi

# card #202 item 1: ONE TARGET VOLUME PER BUILD DIR. Card #94 gave every worker its own remote
# SOURCE dir via LSS_BUILD_DIR, and then every one of them mounted the same `lss-target` anyway -
# so N workers shared one cargo target directory, serialising on its lock and rebuilding over
# each other's artifacts. (The workaround was visible on the build host as a pile of hand-made
# lss-target-* volumes.) The volume now follows the build dir, and the DEFAULT dir keeps the
# historical name so an existing cache is not orphaned.
if [ -n "${LSS_BUILD_TARGET_VOL:-}" ]; then
    TARGET_VOL="$LSS_BUILD_TARGET_VOL"
elif [ "$REMOTE_DIR" = "lss-build" ]; then
    TARGET_VOL="lss-target"
else
    # a docker volume name is [a-zA-Z0-9][a-zA-Z0-9_.-]* - anything else becomes a dash
    TARGET_VOL="lss-target-$(printf '%s' "$REMOTE_DIR" | tr -c 'A-Za-z0-9_.-' '-')"
fi

# card #264: a known container name, so an interrupted run's cleanup can stop the container
# (it outlives a killed ssh) before removing the volume it holds
RUN_NAME="lss-run-$(printf '%s' "$REMOTE_DIR" | tr -c 'A-Za-z0-9_.-' '-')-$$"

# Say what is being verified BEFORE doing it, so a verdict can never be quoted without its tree.
if [ -n "$SHA" ]; then
    echo "remote-build: verifying PINNED $SHA (git archive) on $HOST:$REMOTE_DIR, target volume $TARGET_VOL"
else
    # A source tree with no git metadata is legitimate (the export's own tests build one), and
    # `set -o pipefail` is on: git's failure must be swallowed INSIDE the pipeline, not after it.
    head_sha="$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo 'no-git')"
    dirty="$( { git -C "$REPO" status --porcelain 2>/dev/null || true; } | wc -l | tr -d ' ')"
    echo "remote-build: verifying the LIVE tree $REPO (HEAD $head_sha, ${dirty:-?} uncommitted path(s)) on $HOST:$REMOTE_DIR, target volume $TARGET_VOL"
    echo "remote-build: the live tree can change under a long run - pass --ref <sha> when this result will be quoted as a verdict (card #202)."
fi

# .privacy-words is excluded: it is the OWNER's personal vocabulary (git-ignored) and the
# build tree must test what a fresh clone sees - the committed fallback list - not this
# seat's words (card #15's real-tree test fails on the owner's own comments otherwise).
REMOTE_TOUCHED=1
rsync -a --delete --exclude target --exclude .git --exclude dist --exclude .privacy-words --exclude site "$REPO/" "$HOST:$REMOTE_DIR/"
ssh "$HOST" "docker image inspect lss-build:latest >/dev/null 2>&1 || docker build -q -t lss-build:latest -f $REMOTE_DIR/packaging/Dockerfile.build $REMOTE_DIR/packaging"

run() {
  ssh "$HOST" "docker run --rm --name $RUN_NAME -v \$HOME/$REMOTE_DIR:/w -v $TARGET_VOL:/w/target \
    -v cargo-registry:/usr/local/cargo/registry -w /w -e CARGO_TERM_COLOR=never ${2:-} lss-build:latest bash -c '$1'"
}

TEST_TZ="${LSS_TEST_TZ-America/Los_Angeles}"
TEST_TZ2="${LSS_TEST_TZ2-Asia/Kolkata}"

case "$CMD" in
  # card #232: one pass per timezone, and the zone is ANNOUNCED - a suite that passes because the
  # box was UTC is the failure this exists to stop, so "which zone was that?" must never be a
  # question the output leaves open. Any pass failing fails the command (set -e is on).
  test)   for _tz in "$TEST_TZ" ${TEST_TZ2:+"$TEST_TZ2"}; do
            echo "=== cargo test --workspace under TZ=$_tz"
            run "date && cargo test --workspace 2>&1" "-e TZ=$_tz"
          done ;;
  # `rust:latest` ships WITHOUT clippy: install it when it is missing, so "clippy clean" can never
  # be a step that silently did not run (the last line printed is clippy's own version)
  clippy) run "(cargo clippy --version >/dev/null 2>&1 || rustup component add clippy >/dev/null 2>&1) && cargo clippy --workspace --all-targets -- -D warnings 2>&1 && cargo clippy --version" ;;
  bless)  run "cargo test -p lss-core --test status_golden 2>&1; cargo test -p lss --test render overview_golden_renders 2>&1; cargo test -p lss --test render gpus_lines_golden_render 2>&1; chown -R \$(stat -c %u:%g /w) /w/fixtures" "-e LSS_BLESS=1"
          rsync -a --include='*_golden.json' --include='status_keys_baseline.txt' --include='renders/' --include='renders/*.txt' --exclude='*' "$HOST:$REMOTE_DIR/fixtures/" "$REPO/fixtures/" ;;
  build)  ssh "$HOST" "cd $REMOTE_DIR && chmod -f u+w Cargo.lock 2>/dev/null; true"
          run "cargo build --release --target x86_64-unknown-linux-musl -p lss-collector -p lss 2>&1 && mkdir -p dist && cp target/x86_64-unknown-linux-musl/release/lss-collector target/x86_64-unknown-linux-musl/release/lss dist/ && chown -R \$(stat -c %u:%g /w) dist" ;;
  *) echo "usage: $0 test|clippy|build|bless" >&2; exit 2 ;;
esac
# the lock file is generated on the build host: bring it home so it is committed. Not with --ref:
# $REPO is then a throwaway archive of a past commit, and a pinned VERIFICATION must not write
# anything back into the tree it was asked to judge.
if [ -z "$SHA" ]; then
    rsync -a "$HOST:$REMOTE_DIR/Cargo.lock" "$REPO/Cargo.lock" 2>/dev/null || true
fi
