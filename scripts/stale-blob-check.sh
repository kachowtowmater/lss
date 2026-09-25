#!/usr/bin/env bash
# Fail when a commit sets a changed file's content BACK to a version that file already held
# before its most recent change (card #201).
#
#   scripts/stale-blob-check.sh                  # HEAD
#   scripts/stale-blob-check.sh <commit>         # one commit
#   scripts/stale-blob-check.sh <base>..<head>   # every non-merge commit in the range
#
# Exit 0 clean · 1 a resurrection was found · 2 the arguments or the repository are unusable.
#
# WHY THIS EXISTS. On 2026-09-22 one commit (9e5f317) set two files' blobs back to versions they
# had held before their most recent change - a bad `git stash` resolution in a working tree shared
# by eight agents. Proven at blob level, on two of the gateway's own test files: 9e5f317's
# test_shadow.py is byte-identical to d5f3363^'s, and its conftest.py to d15ec6d^'s. It left a
# test RED ON MAIN for about 29 hours and undid a fix, so test_admission_integration.py ran in
# 121.59s instead of 1.00s.
# NOTHING NOTICED EITHER. A diff review does not catch it: every line of the resurrected file
# looks like something somebody wrote on purpose, because somebody did - just earlier. The only
# signal is the one this script reads, that the new blob is an OLD blob of that same file.
#
# WHAT COUNTS AS A RESURRECTION. For each file a commit changes, against its FIRST parent:
#   * the new blob is compared with every blob that file held earlier in the parent's history,
#     EXCLUDING the parent's own (which is by definition the version being changed);
#   * a match means the commit moved the file backwards to a version it had already left behind.
# Merge commits are skipped: a merge legitimately reintroduces the other side's blobs. A deletion
# is not a resurrection. Renames are not followed, so a rename that also reverts content reads as
# an addition and is not caught - a stated limit, not an oversight.
#
# AN INTENTIONAL REVERT IS LEGITIMATE, AND MUST SAY SO. Restoring an old version on purpose is a
# normal thing to do, and it looks identical to this defect from the outside - the difference is
# whether anyone MEANT it. So it is allowed, but only when the commit message declares it, never
# silently:
#   * `git revert` writes "This reverts commit <sha>." itself - that is accepted as-is, because
#     the tool wrote it and the act was deliberate;
#   * a revert done by hand (a checkout of an old version, a stash resolution someone really did
#     intend) needs a line `Blob-Revert: <why>` in the commit message.
# A commit that restores an old blob and explains neither is the exact shape of card #201 and is
# what this refuses. `Blob-Revert:` requires a reason precisely so that writing it is a decision.
#
# KNOWN LIMIT, worth stating because a guard nobody understands gets worked around: CI can only
# check commits that are in a range it was handed, and .github/workflows/ci.yml does not start a
# run for a push that touches only unwatched paths (docs-only, say). A resurrection in such a push
# is not seen. Run this locally over the range when that matters.
set -euo pipefail

DEPTH="${LSS_STALE_BLOB_DEPTH:-60}"   # how many earlier versions of a file to compare against

git rev-parse --git-dir >/dev/null 2>&1 || { echo "stale-blob-check: not inside a git repository" >&2; exit 2; }

# ------------------------------------------------------------------- which commits to examine
COMMITS=""
if [ "$#" -eq 0 ]; then
    set -- HEAD
fi
for spec in "$@"; do
    case "$spec" in
        *..*)
            # A range. An empty range is not an error - a push that added no commits is fine.
            found="$(git rev-list --no-merges "$spec" 2>/dev/null)" \
                || { echo "stale-blob-check: '$spec' is not a range this repository can resolve" >&2; exit 2; }
            COMMITS="$COMMITS $found"
            ;;
        *)
            one="$(git rev-parse --verify --quiet "$spec^{commit}")" \
                || { echo "stale-blob-check: '$spec' is not a commit" >&2; exit 2; }
            COMMITS="$COMMITS $one"
            ;;
    esac
done

findings=0
checked=0

for c in $COMMITS; do
    # A merge reintroduces the other side's blobs by design; judging it against its first parent
    # would flag every merge that brings a file back.
    parents="$(git rev-list --parents -n 1 "$c")"
    nparents=$(( $(printf '%s\n' "$parents" | wc -w) - 1 ))
    [ "$nparents" -eq 1 ] || continue
    parent="${parents##* }"
    checked=$((checked + 1))

    subject="$(git log -1 --format=%s "$c")"
    body="$(git log -1 --format=%B "$c")"
    # the two declarations that make restoring an old blob deliberate rather than accidental
    declared=0
    printf '%s\n' "$body" | grep -qE '^[[:space:]]*This reverts commit [0-9a-f]{7,40}\.?[[:space:]]*$' && declared=1
    printf '%s\n' "$body" | grep -qE '^[[:space:]]*Blob-Revert:[[:space:]]*[^[:space:]]' && declared=1

    while IFS= read -r f; do
        [ -n "$f" ] || continue
        # deleted in this commit: nothing was set back to anything
        new="$(git rev-parse --verify --quiet "$c:$f")" || continue
        # Every earlier version of this file, newest first. The FIRST entry is the parent's own
        # blob - the version this commit is changing - so it is never a resurrection.
        n=0
        skipped_current=0
        while IFS= read -r h; do
            [ -n "$h" ] || continue
            old="$(git rev-parse --verify --quiet "$h:$f")" || continue
            if [ "$skipped_current" -eq 0 ]; then
                skipped_current=1
                continue
            fi
            n=$((n + 1))
            [ "$n" -le "$DEPTH" ] || break
            if [ "$old" = "$new" ]; then
                if [ "$declared" -eq 1 ]; then
                    break   # a declared revert: this is what it said it would do
                fi
                findings=$((findings + 1))
                echo "stale-blob-check: RESURRECTED BLOB"
                echo "  commit : $(git rev-parse --short "$c")  $subject"
                echo "  file   : $f"
                echo "  blob   : $new"
                echo "  is the version this file held at $(git rev-parse --short "$h") ($(git log -1 --format=%s "$h"))"
                echo "  ...which is OLDER than the version its parent $(git rev-parse --short "$parent") carried, so this commit moved the file BACKWARDS."
                echo "  If that was deliberate, say so in the message: a 'Blob-Revert: <why>' line, or use git revert."
                echo
                break
            fi
        done <<EOF
$(git rev-list --max-count=$((DEPTH + 1)) "$parent" -- "$f")
EOF
    done <<EOF
$(git diff --name-only "$parent" "$c")
EOF
done

if [ "$findings" -gt 0 ]; then
    echo "stale-blob-check: $findings resurrected blob(s) across $checked commit(s). See card #201: a commit that quietly restores an old version of a file left main red for 29 hours and nothing noticed." >&2
    exit 1
fi
echo "stale-blob-check: $checked commit(s) checked, no file was set back to a version it had already left behind"
