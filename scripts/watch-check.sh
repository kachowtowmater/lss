#!/bin/bash
# A generic starting point for lss's WATCH page (card #74). This collector never fetches
# anything itself - see docs/STATUS-JSON.md's "WATCH" section and packaging/watch.json.example
# for the shape [watch] path expects. This script checks GitHub's public releases API for each
# source in a small list and writes that file; run it on any schedule you like (cron, a systemd
# timer, an agent sweep) - hourly is plenty.
#
# SOURCE LIST (default ~/.config/lss/watch-sources.txt, one per line, "|"-separated):
#   <name>|<what this source's recipes actually feed, plain English>|<owner>/<repo>
# Blank lines and lines starting with # are skipped. Example:
#   sglang|serving engine releases|sgl-project/sglang
#
# Needs: curl, jq. No credentials: GitHub's public releases API answers unauthenticated, rate
# limited to 60/hour per IP - plenty for an hourly check of a handful of repos. Set GITHUB_TOKEN
# to raise that limit if you are watching many; it is sent only to api.github.com, never written
# to the output file, and nothing here needs it to work at all.
#
# HARD CONSTRAINT this script exists to honour: the CONTENT of a release, never its title alone -
# `newest.summary` below is real text from the release body, not `.name`. A release with no body
# says so honestly rather than falling back to a title standing in as if it were content.
set -euo pipefail

SOURCES="${WATCH_SOURCES:-$HOME/.config/lss/watch-sources.txt}"
OUT="${WATCH_OUT:-$HOME/.config/lss/watch.json}"

command -v curl >/dev/null || { echo "watch-check: curl is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "watch-check: jq is required" >&2; exit 1; }
# card #244: A MISSING SOURCE LIST IS THE FIRST-RUN STATE, NOT AN ERROR. This exited 1, and on
# 2026-09-23 install-collector.sh enabled lss-watch.timer for the first time on the collector host:
# the unit then FAILED every hour on a machine where nobody had configured a source yet, and the
# collector logged "watch.json: No such file" beside it. Card #116 had already decided this
# question for the EMPTY list ("the honest first-run state" - it writes `{"sources": []}` and
# exits 0); a missing file is the same fact one step earlier, so it takes the same path. Writing
# the empty document also stops the collector's own complaint, because the file it reads now
# exists and honestly says nothing is configured.
# Not silent, though: the line below names the file and how to fill it, on stdout (not stderr),
# because a timer's job log should read as information, not as a fault.
if [ ! -f "$SOURCES" ]; then
    echo "watch-check: no sources configured ($SOURCES does not exist) - one line per source: name|covers|owner/repo"
    mkdir -p "$(dirname "$OUT")"
    printf '{"sources": []}\n' > "$OUT.new"
    mv -f "$OUT.new" "$OUT"
    echo "watch-check: wrote $OUT (0 sources)"
    exit 0
fi

now="$(date +%s)"
entries=()

# a plain function, not an array: macOS ships bash 3.2, where `"${ARR[@]}"` on an EMPTY array
# throws "unbound variable" under `set -u` - a real portability trap for a script meant to run
# on anyone's own machine, not just a Linux collector host.
fetch_latest_release() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        curl -fsS -m 15 -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$1/releases/latest" 2>/dev/null || true
    else
        curl -fsS -m 15 -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$1/releases/latest" 2>/dev/null || true
    fi
}

while IFS='|' read -r name covers repo; do
    [ -z "$name" ] && continue
    case "$name" in \#*) continue ;; esac
    resp="$(fetch_latest_release "$repo")"
    if [ -z "$resp" ] || ! jq -e . >/dev/null 2>&1 <<<"$resp"; then
        # unreachable, no releases, or a bad repo slug - never blank the whole page for one
        # source being down; report it as checked, nothing found, and move on to the rest
        entries+=("$(jq -n --arg n "$name" --arg c "$covers" --argjson t "$now" '{name: $n, covers: $c, last_checked: $t, newest: null}')")
        continue
    fi
    date="$(jq -r '.published_at // empty' <<<"$resp" | cut -dT -f1)"
    if [ -z "$date" ]; then
        entries+=("$(jq -n --arg n "$name" --arg c "$covers" --argjson t "$now" '{name: $n, covers: $c, last_checked: $t, newest: null}')")
        continue
    fi
    # `|| true`: `head -3` closing the pipe early after a large release body sends the upstream
    # `grep`/`jq` a SIGPIPE (exit 141) - a normal, expected thing for a pipeline ending in `head`,
    # but `pipefail` + `set -e` would otherwise treat that as this script's own failure.
    body="$(jq -r '.body // ""' <<<"$resp" | grep -v '^[[:space:]]*$' | head -3 | tr '\n' ' ' | cut -c1-300 || true)"
    if [ -z "$body" ]; then
        body="(no release notes body was published for this release - see the URL)"
    fi
    html_url="$(jq -r '.html_url // ""' <<<"$resp")"
    entries+=("$(jq -n --arg n "$name" --arg c "$covers" --argjson t "$now" --arg d "$date" --arg s "$body" --arg u "$html_url" \
        '{name: $n, covers: $c, last_checked: $t, newest: {date: $d, summary: $s, url: $u, has_receipt: true}}')")
done < "$SOURCES"

mkdir -p "$(dirname "$OUT")"
# card #116: the comment above `entries=()` already names the bash-3.2 empty-array trap, but this
# line still hit it directly - an empty or all-comments source list (the honest first-run state)
# left `entries` empty, and `"${entries[@]}"` under `set -u` threw "unbound variable" here, so the
# WATCH page never even got a `{"sources": []}` to read: it crashed instead of shipping empty.
if [ "${#entries[@]}" -eq 0 ]; then
    printf '{"sources": []}\n' > "$OUT.new"
else
    # card #141: touch `entries[0]` before the `[@]` expansion. The card-#116 guard above is only
    # OBSERVABLE on bash < 4.4 - measured on GNU bash 5.2.21 (`BASH_COMPAT=32` included), an empty
    # `"${entries[@]}"` under `set -u` is an ordinary no-op, so a suite running 5.2 (our CI
    # container) cannot tell the guard from its absence. A DIRECT subscript is different: the line
    # below is reached only when the list is NON-empty, and `"${entries[0]}"` under `set -u` is an
    # unbound variable on 5.2 AND 3.2 alike, so removing the guard crashes on EVERY bash we run.
    # This makes the guard provable, not merely correct.
    _first="${entries[0]}"
    printf '%s\n' "${entries[@]}" | jq -s '{sources: .}' > "$OUT.new"
fi
mv -f "$OUT.new" "$OUT"
echo "watch-check: wrote $OUT ($(jq '.sources | length' "$OUT") sources)"
