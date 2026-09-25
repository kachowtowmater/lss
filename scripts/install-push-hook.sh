#!/usr/bin/env bash
# card #169: a fresh clone (or a fresh worktree) has the hook FILE
# (scripts/hooks/pre-push-scope) but nothing wires it into git, so nothing checks a push until
# this is run once. The failure it prevents has already happened twice in one afternoon, to two
# different agents, and in both cases the work was recovered by luck rather than by a control.
#
#   scripts/install-push-hook.sh
#
# Uses `git rev-parse --git-path hooks` rather than a hardcoded `.git/hooks`, so it installs to
# the right place under a worktree checkout or a global `core.hooksPath` override alike - each
# git worktree has its own hooks path, which is worth knowing here because these agents work in
# a worktree per card.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
git rev-parse --git-dir >/dev/null 2>&1 || { echo "install-push-hook: $HERE is not a git checkout - nothing to install into" >&2; exit 2; }
HOOKS_DIR="$(git rev-parse --git-path hooks)"
mkdir -p "$HOOKS_DIR"
printf '#!/bin/sh\n# card #169: no-op CLEANLY when this repo has no such script. The one-liner this replaces\n# was `[ -x script ] && exec bash script`, whose exit status in a repo WITHOUT the script is 1,\n# so a GLOBAL core.hooksPath install of it blocked every push in every other repository on\n# the machine (measured on this seat: exit 1 in an unrelated checkout).\nif [ -x scripts/hooks/pre-push-scope ]; then\n  exec bash "$(git rev-parse --show-toplevel)/scripts/hooks/pre-push-scope" "$@"\nfi\nexit 0\n' > "$HOOKS_DIR/pre-push"
chmod +x "$HOOKS_DIR/pre-push"
# card #169: chmod the repo-local script only if it IS here. Doing both in one chmod
# made the installer die with a bare "cannot access" in any checkout that does not
# carry it - which is exactly the case the wrapper is written to tolerate.
if [ -f scripts/hooks/pre-push-scope ]; then
    chmod +x scripts/hooks/pre-push-scope
else
    echo "install: note - this checkout has no scripts/hooks/pre-push-scope; the wrapper is installed and will no-op here" >&2
fi
echo "install-push-hook: installed at $HOOKS_DIR/pre-push - a push from a branch that does not contain its remote now fails"
