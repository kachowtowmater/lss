#!/bin/bash
# Print the release notes for <tag> - the ANNOTATED tag's own message - or fail loudly.
#
# Card #253 (v1.1.0, and v1.0.0 before it): `gh release create --notes-from-tag` published the
# tagged COMMIT's message as the release page, and exited 0. `fetch-depth: 0` (ab0c4da) was not
# enough: on a tag push, actions/checkout fetches every tag and THEN runs
#     git fetch --no-tags origin +<sha>:refs/tags/<tag>
# which overwrites the annotated tag with a LIGHTWEIGHT one pointing at the same commit, and gh
# silently falls back. So this script (a) re-fetches the tag OBJECT from the remote, (b) refuses
# to go on unless the tag is annotated, and (c) prints its message for `gh release create
# --notes-file` - the notes never depend on gh's fallback again.
#
# Usage:  scripts/release-notes.sh <tag> [remote]      (remote defaults to origin)
# Exit:   0 = the annotated message is on stdout. 1 = a lightweight or missing tag, or an empty
#         message: nothing on stdout, the reason on stderr.
set -uo pipefail
TAG="${1:?usage: scripts/release-notes.sh <tag> [remote]}"
REMOTE="${2:-origin}"

# the leading + lets the remote's tag object replace the lightweight local ref checkout left
if ! git fetch --no-tags "$REMOTE" "+refs/tags/$TAG:refs/tags/$TAG" >&2; then
    echo "release-notes: ERROR could not fetch refs/tags/$TAG from $REMOTE" >&2
    exit 1
fi
kind=$(git cat-file -t "refs/tags/$TAG" 2>/dev/null || echo missing)
if [ "$kind" != "tag" ]; then
    echo "release-notes: ERROR $TAG is a '$kind', not an ANNOTATED tag. A release page made from it" >&2
    echo "release-notes: would carry the commit message instead of release notes, so nothing is" >&2
    echo "release-notes: published. Tag the commit with 'git tag -a $TAG -F <notes-file> <commit>'." >&2
    exit 1
fi
notes=$(git tag -l --format='%(contents)' "$TAG")
# card #254: a SIGNED tag's %(contents) ends with its signature block (-----BEGIN PGP / SSH
# SIGNATURE----- ...). That is not release notes: cut exactly git's own %(contents:signature)
# off the end, whatever the signing format.
sig=$(git tag -l --format='%(contents:signature)' "$TAG")
if [ -n "$sig" ]; then
    notes="${notes%"$sig"}"
    notes="${notes%"${notes##*[![:space:]]}"}"
fi
if [ -z "${notes//[[:space:]]/}" ]; then
    echo "release-notes: ERROR $TAG is annotated but its message is empty" >&2
    exit 1
fi
printf '%s\n' "$notes"
