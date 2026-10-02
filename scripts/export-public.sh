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
# card #553: excluded() now lives in ONE place, scripts/export-excluded.sh, sourced below - the
# pre-commit privacy hook sources the same file, so the hook's skip list and the export's can no
# longer drift apart (before, the hook hand-kept 3 of these entries and blocked edits to
# excluded files on their pre-existing private words).
source "$(dirname "$0")/export-excluded.sh"

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

# card #547: PR #19 moves our private CI to the SELF-HOSTED runner ([self-hosted, linux,
# llm-server-status]). A stranger's copy has no such runner and every job would sit "Queued"
# forever, so the exported ci.yml must say the label ANY hosted GitHub can give:
# ubuntu-latest. The whole line is replaced, not edited inside the list - a different order or a
# second label would keep a stale meaning - and indentation is kept by construction (the match is
# anchored on leading spaces; substr prints the same prefix). Other runs-on lines (macos) are
# untouched, but a COMMENT or STEP NAME that says the label in prose (the shellcheck step's card
# #820 note) is reworded too: the label names infrastructure the public copy does not have, and
# "is main green" in the exported copy is checked by grepping the whole workflows directory for
# it. The step's conditional is untouched - it is about the shellcheck binary, not the runner.
# EVERY workflow file gets the rewrite, not only ci.yml: PR #19 also moves release.yml's Linux job
# to the runner, and publish-public.sh's P4 refuses any exported runs-on it does not recognise.
for wf in "$DEST"/.github/workflows/*.yml "$DEST"/.github/workflows/*.yaml; do
    [ -f "$wf" ] || continue
    awk '
        /^ *runs-on: *\[self-hosted, +linux, +llm-server-status\] *$/ { print substr($0, 1, length($0) - length(substr($0, match($0, /r/)))) "runs-on: ubuntu-latest"; next }
        { gsub(/# card #[0-9]+: the self-hosted runner/, "# card 820: the runner"); gsub(/ensure shellcheck \(self-hosted runner\)/, "ensure shellcheck"); print }
    ' "$wf" > "$wf.tmp"
    mv "$wf.tmp" "$wf"
done

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
