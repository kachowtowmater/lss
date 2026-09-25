#!/usr/bin/env bash
# card #166 (#144 item 7): a fresh clone has the hook FILE
# (scripts/hooks/pre-commit-privacy) but nothing wires it into git, so nothing runs at commit
# time until this is run once. CI and the export catch a leak AFTER a push - by which time it is
# in public history, the one place it cannot be taken back from. This is the only control that
# stops a private word reaching a COMMIT, and it does not exist until this script runs.
#
#   scripts/install-privacy-hook.sh
#
# Uses `git rev-parse --git-path hooks` rather than a hardcoded `.git/hooks`, so it installs to
# the right place under a worktree checkout or a global `core.hooksPath` override alike.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
git rev-parse --git-dir >/dev/null 2>&1 || { echo "install-privacy-hook: $HERE is not a git checkout - nothing to install into" >&2; exit 2; }
HOOKS_DIR="$(git rev-parse --git-path hooks)"
mkdir -p "$HOOKS_DIR"
printf '#!/bin/sh\n# card #169: no-op CLEANLY when this repo has no such script. The one-liner this replaces\n# was `[ -x script ] && exec bash script`, whose exit status in a repo WITHOUT the script is 1,\n# so a GLOBAL core.hooksPath install of it blocked every commit in every other repository on\n# the machine (measured on this seat: exit 1 in an unrelated checkout).\nif [ -x scripts/hooks/pre-commit-privacy ]; then\n  exec bash "$(git rev-parse --show-toplevel)/scripts/hooks/pre-commit-privacy" "$@"\nfi\nexit 0\n' > "$HOOKS_DIR/pre-commit"
chmod +x "$HOOKS_DIR/pre-commit"
# card #169: chmod the repo-local script only if it IS here. Doing both in one chmod
# made the installer die with a bare "cannot access" in any checkout that does not
# carry it - which is exactly the case the wrapper is written to tolerate.
if [ -f scripts/hooks/pre-commit-privacy ]; then
    chmod +x scripts/hooks/pre-commit-privacy
else
    echo "install: note - this checkout has no scripts/hooks/pre-commit-privacy; the wrapper is installed and will no-op here" >&2
fi
echo "install-privacy-hook: installed at $HOOKS_DIR/pre-commit - a staged file with a private word now blocks the commit"
