#!/usr/bin/env bash
# Write a clean copy of this repository WITHOUT our site (site/ours/, gate/), then run
# scripts/privacy-check.sh over the copy. Any hit fails the export and removes the copy: a tree
# that did not pass is never left lying around to be published by mistake.
#
#   scripts/export-public.sh <new-or-empty-directory>
#
# It only WRITES a directory. It creates no repository and pushes nothing.
# What is copied: what a commit would hold (tracked + untracked-but-not-ignored files). In a
# checkout without git metadata: every file except target/, dist/ and .git/.
# Env: LSS_EXPORT_SOURCE  the tree to export (default: the repository this script is in)
#      PRIVACY_WORDS      extra private words, one regex per line (default: <source>/.privacy-words,
#                         falling back to the committed packaging/privacy-words.example)

set -euo pipefail
DEST="${1:?usage: scripts/export-public.sh <new-or-empty-directory>}"
SRC="${LSS_EXPORT_SOURCE:-$(cd "$(dirname "$0")/.." && pwd)}"
WORDS="${PRIVACY_WORDS:-$SRC/.privacy-words}"
# A fresh clone has no .privacy-words (it is git-ignored), so it would check only the generic
# patterns and host NAMES would pass silently: fall back to the committed example list, whose
# entries are examples, not defaults.
[ -f "$WORDS" ] || WORDS="$SRC/packaging/privacy-words.example"
# card #67: say which word list is actually in force, so the weaker fallback is never silent.
# card #144: and REFUSE, rather than warn and proceed. The tree we would ever publish FROM is a
# fresh clone, .privacy-words is git-ignored so a fresh clone HAS none, and the fallback list
# cannot see our own host names - so the one case that matters was the one case the scan was
# blind in. Measured 2026-09-22: a fresh-clone export reported "privacy check clean", exit 0,
# while the result carried our host name in 4 files and two model ids we run in 34 more.
# "Fails closed on any hit" was true and was not enough: it now also fails closed on a MISSING
# WORD LIST. Override deliberately with LSS_EXPORT_ALLOW_NO_WORDS=1 when you genuinely mean to
# check generic patterns only (a CI smoke test of the script itself, say) - it is loud either way.
if [ "$WORDS" = "$SRC/packaging/privacy-words.example" ]; then
    echo "export-public: no .privacy-words found at $SRC/.privacy-words: falling back to the committed example list - only generic patterns plus its EXAMPLE words are checked, your host names and user names are NOT" >&2
    if [ "${LSS_EXPORT_ALLOW_NO_WORDS:-0}" != "1" ]; then
        echo "export-public: REFUSING to export a tree whose private-word check cannot see your own names." >&2
        echo "  Write $SRC/.privacy-words (one regex per line: host names, user names, model ids," >&2
        echo "  utility/plan names, key names), or set LSS_EXPORT_ALLOW_NO_WORDS=1 if you truly mean" >&2
        echo "  generic patterns only. A publishable tree has to be checked against YOUR words," >&2
        echo "  and the tree you publish from is exactly the one that has no list (card #144)." >&2
        exit 4
    fi
    # privacy-check then sees a list in force and stays quiet; this line is the one that talks.
    export PRIVACY_WORDS="$WORDS"
fi

# never exported, whatever else happens. Keep this list and your site directory's README in step.
excluded() {
    case "$1" in
        # gate/ is the gateway engineer's: its upstream name, our public endpoint and its
        # test user names are DEPLOYMENT vocabulary, not product surface, and the words are
        # load-bearing in its tests - a public export carries the product, not our box names.
        site/ours/*|gate/*|.privacy-words) return 0 ;;
        # card #149 (PUBLIC BLOCKER) + #143: the gateway's DEPLOY tooling goes with the gateway.
        # gate/ is already excluded, so a public copy that shipped these handed a stranger a
        # script for deploying a component they do not have, plus a test binary that reads
        # a gateway source file for GATE_VERSION and therefore FAILED 8 of 10 tests on a fresh
        # export.
        # A red suite on a first clone is the strongest signal that a project is unmaintained,
        # and it made "is main green" unverifiable by anyone outside this seat. The env EXAMPLE
        # goes too: it is the gateway config, and it is where our own host name and two model ids
        # leaked into the public tree (card #144).
        scripts/gate-deploy.sh|crates/lss-collector/tests/gate_deploy_script.rs|packaging/gate-env.conf.example) return 0 ;;
        # card #163 item 1 (verifier-2/verifier-3): same class again - this script rewrites
        # ~/.omp/agent/config.yml (omp is OUR agent-infrastructure tool, not something a stranger
        # runs) and its default GATE_URL is our trusted lane at 127.0.0.1:8096. A stranger has no
        # omp and no reason to run this.
        scripts/omp-sync-default-model.sh) return 0 ;;
        # card #180 gate 2 (from the unmerged fix/143 branch, whose exclusion half never landed -
        # only its docstring half did): this script reads the GATEWAY's admission shadow log, and
        # gate/ does not ship. A stranger gets a python script for a component they do not have,
        # pointed at a state directory that does not exist on their machine. Same call as
        # gate-deploy.sh and omp-sync-default-model.sh: the tooling goes with the thing it drives.
        scripts/shadow-analysis.py) return 0 ;;
        # card #180 gate 4, second sweep of the same class (lss-builder-2): this one is not
        # gateway tooling, it is AGENT-WORKFLOW tooling. scripts/worker-checkout.sh exists
        # because several AI workers edit one checkout at once (its own header says "two
        # builders/verifiers and an omp worker"), which is a fact about how this project is
        # DEVELOPED, not about the product a stranger installs. omp-sync-default-model.sh was
        # dropped for exactly this reason; this is its sibling, and it shipped in every export
        # until now - along with the test that drives it. Nothing in the public copy references
        # either any more (the card-#180 README rewrite removed the worker-checkout section), so
        # the exclusion leaves no dangling pointer.
        scripts/worker-checkout.sh|crates/lss-collector/tests/worker_checkout_script.rs) return 0 ;;
        # card #190 (verifier, re-checking #144 item 3): the same class a THIRD time, and this
        # one is a whole DOCUMENT rather than a script. docs/OMP-DEFAULT-MODEL.md is the
        # write-up of a 2026-09-20 incident on OUR agent fleet: which provider patterns are in
        # our omp config, what our agent work silently fell back to, and the edit we decided to
        # make on "every box that has omp". A stranger has no omp, no fleet and no stake in our
        # incident - and it is the doc that pointed hardest at scripts/omp-sync-default-model.sh,
        # excluded just above. The alert it documents (omp_default_mismatch) still ships and is
        # still documented, generically, in docs/RUNBOOK.md; what goes is our incident report.
        docs/OMP-DEFAULT-MODEL.md) return 0 ;;
        # card #302: how WE publish the public copy (our GitHub account, the release run we
        # watch). A stranger's copy is already the published thing; it has nothing to publish.
        scripts/publish-public.sh) return 0 ;;
        # card #324: the e2e gate's RED-first evidence (scripts/e2e/RED-on-<sha>.txt) is the
        # development record of a gate failing BEFORE the work - a transcript of our runs, not
        # something a stranger runs or reads. The gate itself (run.sh, scenario.sh, ...) ships.
        scripts/e2e/RED-*.txt) return 0 ;;
        *) return 1 ;;
    esac
}

if [ -e "$DEST" ] && [ -n "$(ls -A "$DEST" 2>/dev/null)" ]; then
    echo "export-public: $DEST exists and is not empty: give a new or empty directory" >&2
    exit 2
fi
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
case "$DEST/" in "$SRC"/*) echo "export-public: $DEST is inside the source tree" >&2; rmdir "$DEST" 2>/dev/null || true; exit 2 ;; esac

list() {
    if git -C "$SRC" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git -C "$SRC" ls-files -z --cached --others --exclude-standard
    else
        (cd "$SRC" && find . -type f -not -path './target/*' -not -path './dist/*' -not -path './.git/*' -print0)
    fi
}

n=0
while IFS= read -r -d '' f; do
    f="${f#./}"
    if excluded "$f" || [ ! -f "$SRC/$f" ]; then continue; fi
    mkdir -p "$DEST/$(dirname "$f")"
    cp -p "$SRC/$f" "$DEST/$f"
    n=$((n + 1))
done < <(list)

# card #158: the exported ci.yml still named gate/ - a "gate/**" path filter in both triggers,
# plus the whole 'gate' job (working-directory: gate, a pip install pinned to the gateway's own
# Dockerfile). Card #143 already guarded the job to SKIP when gate/ does not exist, which stops
# it failing - but a published copy should not still MENTION a directory it does not ship, so
# strip the job and its path filters outright rather than leave them dormant.
#
# card #160 (verifier-3): the first version matched the LITERAL "gate/**" path-filter line, so a
# differently-shaped filter under the same list - "gate/tests/**", a different glob, different
# quoting - would survive both the strip and its test untouched. Match any path-filter list item
# that starts with gate/ at all, quoted or not, rather than one exact string.
#
# card #167 (verifier-3/verifier-2): admitting the single quote into the character class (#165)
# forced this awk program into a DOUBLE-quoted shell string, so plain awk idioms ($0, $1, $NF)
# would silently become SHELL expansions instead of awk fields - not a syntax error, a silently
# different program ($0 is the whole line in awk, the script's own name in the shell). Back to a
# SINGLE-quoted program: the quote characters are octal escapes (\047 = ', \042 = "), which pass
# through untouched with no shell interpretation at all. Verified against BSD awk (macOS), gawk,
# mawk and the original one-true-awk - identical result on all four, all three filter shapes.
#
# card #302: the rust job's changed-file regex now carries a '|gate/' alternative (Rust tests read
# gate/ files). The gsub drops any '|gate/...' alternative from non-comment lines, so the exported
# workflow still names no gate/ path; comments are left alone (they may name the exclusion).
if [ -f "$DEST/.github/workflows/ci.yml" ]; then
    awk '
        /^      - [\047\042]?gate\// { next }
        /^  gate:$/ { skip = 1; next }
        skip && /^  [^ ]/ { skip = 0 }
        skip { next }
        /^ *#/ { print; next }
        { gsub(/\|gate\/[^|)\047]*/, "") }
        { print }
    ' "$DEST/.github/workflows/ci.yml" > "$DEST/.github/workflows/ci.yml.tmp"
    mv "$DEST/.github/workflows/ci.yml.tmp" "$DEST/.github/workflows/ci.yml"
fi

# card #144: packaging/privacy-words.example is a WORD LIST, so the privacy check exempts it
# (it names what it forbids) - and it named everything: our hosts, model ids, agent names, the
# GitHub account, the public domain, the utility and plan. Every export shipped it verbatim and
# said "clean". The committed file keeps its full list (CI and the hook in THIS repository run
# on it); the exported copy is cut at the marker line, keeping only the generic examples.
EXAMPLE="$DEST/packaging/privacy-words.example"
CUT='# ==== export-public: CUT HERE ===='
if [ -f "$EXAMPLE" ] && grep -qxF -- "$CUT" "$EXAMPLE"; then
    awk -v cut="$CUT" '$0 == cut { exit } { print }' "$EXAMPLE" > "$EXAMPLE.tmp"
    mv "$EXAMPLE.tmp" "$EXAMPLE"
fi
# ...and the cut is CHECKED, not assumed: scan the exported example with every word in force
# except the SAMPLE entries above the marker in the source (it would match its own samples
# otherwise). Subtracting what the exported copy happens to contain would be blind to exactly
# the failure this guards: an uncut copy "contains" every private word, so none would be scanned.
#   marker in the source  -> words minus the source's above-marker samples
#   no marker, real list  -> every word (a fork's own example must not name its own secrets)
#   no marker, fallback   -> nothing to scan with: the list IS this file. That mode cannot
#                            publish without LSS_EXPORT_ALLOW_NO_WORDS; this is its stated blind spot.
# The committed samples themselves are pinned by a test (export_script.rs), so a private word
# written ABOVE the marker fails there rather than here.
example_hits=""
if [ -f "$EXAMPLE" ]; then
    entries() { sed -E 's/#.*//; s/^[[:space:]]+|[[:space:]]+$//g; /^$/d' "$@"; }
    rest="$(mktemp)"
    SRC_EXAMPLE="$SRC/packaging/privacy-words.example"
    if [ -f "$SRC_EXAMPLE" ] && grep -qxF -- "$CUT" "$SRC_EXAMPLE"; then
        samples="$(mktemp)"
        awk -v cut="$CUT" '$0 == cut { exit } { print }' "$SRC_EXAMPLE" | entries > "$samples"
        entries "$WORDS" | grep -vxF -f "$samples" > "$rest" || true
        rm -f "$samples"
    elif [ "$WORDS" != "$SRC_EXAMPLE" ]; then
        entries "$WORDS" > "$rest"
    fi
    if [ -s "$rest" ]; then
        example_hits="$(PRIVACY_WORDS="$rest" PRIVACY_EXAMPLE_EXEMPT=./.never-a-file \
            bash "$SRC/scripts/privacy-check.sh" "$DEST" 2>/dev/null | grep -F './packaging/privacy-words.example:' || true)"
    fi
    rm -f "$rest"
fi
if [ -n "$example_hits" ]; then
    printf '%s\n' "$example_hits"
    rm -rf "$DEST"
    echo "export-public: FAILED - the exported packaging/privacy-words.example still names private words ($(printf '%s\n' "$example_hits" | wc -l | tr -d ' ') line(s) above; is the '$CUT' marker above every private entry?); $DEST was removed" >&2
    exit 1
fi

if hits=$(PRIVACY_WORDS="$WORDS" bash "$SRC/scripts/privacy-check.sh" "$DEST"); then
    echo "export-public: $n files written to $DEST, privacy check clean"
else
    printf '%s\n' "$hits"
    rm -rf "$DEST"
    echo "export-public: FAILED the privacy check ($(printf '%s\n' "$hits" | wc -l | tr -d ' ') line(s) above); $DEST was removed" >&2
    exit 1
fi
