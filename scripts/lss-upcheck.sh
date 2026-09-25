#!/bin/bash
# LLM SERVER STATUS up-check: runs on ANOTHER machine (the seat) every 60 s from launchd.
# The GPU box cannot report its own death, so the seat asks from outside. TWO targets, each
# OPTIONAL and each probed ONLY when its URL is configured:
#   serve      GET  $LSS_UPCHECK_SERVE_URL      (whatever serves the model - the engine itself,
#                                                or a gateway in front of it)
#   collector  GET  $LSS_UPCHECK_COLLECTOR_URL  (lss-collector /health: 503 when its poll loop is stuck)
# Two consecutive misses of a target -> macOS banner + line in upcheck.log + ALERT sentinel.
# First success after that -> "recovered" banner, sentinel cleared.
#
# NO GUESSED SERVE URL (card #82): the serve target used to default to a fixed gateway port -
# a port that exists only if you run a gateway there, which most machines do not. On any other
# machine that probe can only fail, so the up-check - the one component whose entire job is to be
# believed - banner'd "serve down" forever over a perfectly healthy server. A target nobody
# configured is never probed: it is reported as "unset" (no banner, no sentinel) and that fact is
# visible in upcheck.last. Set LSS_UPCHECK_SERVE_URL to the thing that must answer.
#
# State: ~/.local/state/lss/  (miss.<target>, ALERT, upcheck.log, upcheck.last = heartbeat of the latest run)
# Test:  LSS_UPCHECK_TEST=1 scripts/lss-upcheck.sh
#        runs the whole miss -> alert -> recover cycle against dead/live local URLs in a
#        temp state dir, prints PASS/FAIL, shows ONE real "[TEST]" banner, touches no real state.
set -uo pipefail

# Where the GPU box is comes from ~/.config/lss/upcheck.env when that file exists:
#   LSS_UPCHECK_SERVE_URL=http://<gpu-box>:8090/v1/models       # the engine, or the gateway's port
#   LSS_UPCHECK_COLLECTOR_URL=http://<gpu-box>:8099/health
ENV_FILE="${LSS_UPCHECK_ENV:-$HOME/.config/lss/upcheck.env}"
# shellcheck disable=SC1090  # a site file, read when it exists
if [ -f "$ENV_FILE" ]; then . "$ENV_FILE"; fi
# card #82: NO default for the serve target - an unconfigured leg is skipped, never guessed.
SERVE_URL="${LSS_UPCHECK_SERVE_URL:-}"
COLLECTOR_URL="${LSS_UPCHECK_COLLECTOR_URL:-http://127.0.0.1:8099/health}"
STATE_DIR="${LSS_STATE_DIR:-$HOME/.local/state/lss}"
TIMEOUT="${LSS_UPCHECK_TIMEOUT:-8}"
MISSES_TO_ALERT=2
TITLE="LLM SERVER"
LOG="$STATE_DIR/upcheck.log"
SENTINEL="$STATE_DIR/ALERT"

log() { echo "$(date '+%Y-%m-%dT%H:%M:%S%z') $*" >>"$LOG"; }

banner() {
  log "banner: $1"
  [ "${LSS_UPCHECK_NO_BANNER:-0}" = "1" ] && return 0
  /usr/bin/osascript -e 'on run argv' -e "display notification (item 1 of argv) with title \"$TITLE\" sound name \"Basso\"" -e 'end run' "$1" >/dev/null 2>&1 || log "banner FAILED (osascript)"
}

# probe <url> -> 0 when the URL answers 2xx inside the timeout
probe() { /usr/bin/curl -fsS -o /dev/null -m "$TIMEOUT" "$1" 2>/dev/null; }

sentinel_has() { [ -f "$SENTINEL" ] && grep -q "^$1 " "$SENTINEL"; }
sentinel_add() { echo "$1 down since $(date '+%Y-%m-%d %H:%M:%S') $2" >>"$SENTINEL"; }
sentinel_remove() {
  [ -f "$SENTINEL" ] || return 0
  grep -v "^$1 " "$SENTINEL" >"$SENTINEL.tmp" || true
  if [ -s "$SENTINEL.tmp" ]; then mv -f "$SENTINEL.tmp" "$SENTINEL"; else rm -f "$SENTINEL" "$SENTINEL.tmp"; fi
}

check() {
  local name="$1" url="$2" what="$3" miss_file="$STATE_DIR/miss.$1" misses
  # card #82: nobody configured this target, so there is nothing to ask. Do NOT probe a guessed
  # URL (that is what made a stranger's up-check banner "serve down" forever) and do NOT write a
  # miss - report it as "unset" so upcheck.last says honestly that this leg is not being watched.
  if [ -z "$url" ]; then
    log "unset: $name (no URL configured - not watched)"
    rm -f "$miss_file"
    return 2
  fi
  if probe "$url"; then
    if sentinel_has "$name"; then
      sentinel_remove "$name"
      banner "RECOVERED: $what is answering again ($url)"
    fi
    rm -f "$miss_file"
    return 0
  fi
  misses=$(( $(cat "$miss_file" 2>/dev/null || echo 0) + 1 ))
  echo "$misses" >"$miss_file"
  log "miss $misses: $name $url"
  if [ "$misses" -ge "$MISSES_TO_ALERT" ] && ! sentinel_has "$name"; then
    sentinel_add "$name" "$url"
    banner "$what NOT ANSWERING from $(hostname -s 2>/dev/null || echo here) ($misses checks in a row): $url"
  fi
  return 1
}

run_once() {
  mkdir -p "$STATE_DIR"
  local r_serve=ok r_collector=ok rc
  check serve "$SERVE_URL" "LLM serve"; rc=$?
  if [ "$rc" = "0" ]; then r_serve=ok; elif [ "$rc" = "2" ]; then r_serve="unset"; else r_serve=MISS; fi
  check collector "$COLLECTOR_URL" "lss-collector"; rc=$?
  if [ "$rc" = "0" ]; then r_collector=ok; elif [ "$rc" = "2" ]; then r_collector="unset"; else r_collector=MISS; fi
  # heartbeat: a healthy run is otherwise silent, and "is it even running?" must be answerable.
  # "unset" is its own state - distinct from ok and from MISS, and never counted as an alarm.
  echo "$(date '+%Y-%m-%dT%H:%M:%S%z') serve=$r_serve collector=$r_collector" >"$STATE_DIR/upcheck.last"
  # keep the log bounded (~2000 lines)
  if [ -f "$LOG" ] && [ "$(wc -l <"$LOG")" -gt 2000 ]; then tail -n 1000 "$LOG" >"$LOG.tmp" && mv -f "$LOG.tmp" "$LOG"; fi
  return 0
}

self_test() {
  local tmp fails=0 srv_pid="" live_url
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/lss-upcheck-test.XXXXXX")
  export LSS_STATE_DIR="$tmp"; STATE_DIR="$tmp"; LOG="$tmp/upcheck.log"; SENTINEL="$tmp/ALERT"
  TIMEOUT=2; TITLE="LLM SERVER [TEST]"
  expect() { if eval "$2"; then echo "PASS  $1"; else echo "FAIL  $1"; fails=$((fails + 1)); fi; }

  # card #82: the self-test must not depend on OUR box being up - a stranger runs it too. Bring up
  # our own throwaway HTTP server on a free port and use THAT as the reachable target.
  live_url="http://127.0.0.1:9/definitely-dead"       # placeholder, replaced below if a server starts
  if command -v python3 >/dev/null 2>&1; then
    local port
    port=$(python3 - <<'PY' 2>/dev/null || true
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
)
    if [ -n "$port" ]; then
      python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1 &
      srv_pid=$!
      live_url="http://127.0.0.1:$port/"
      for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        probe "$live_url" && break
        sleep 0.2
      done
      if ! probe "$live_url"; then
        echo "NOTE  could not start a local test server: the 'reachable' halves are skipped"
        live_url=""
      fi
    fi
  else
    echo "NOTE  no python3: the 'reachable' halves of this self-test are skipped"
  fi

  # --- card #82: an UNCONFIGURED leg is never probed and never alerts -------------------------
  # hermetic: with no serve URL the ONLY thing this block can alarm about is a guessed serve port,
  # so a sentinel appearing here IS the bug the card names - the collector leg points at our own
  # local server, which is reachable whenever live_url is set.
  LSS_UPCHECK_NO_BANNER=1
  SERVE_URL=""; COLLECTOR_URL="${live_url:-http://127.0.0.1:9/definitely-dead}"
  rm -f "$SENTINEL" "$tmp/miss.serve" "$tmp/miss.collector"
  run_once; run_once; run_once   # three runs: a guessed URL would have alerted on the 2nd
  expect "unset serve: no miss counter (card #82)" '[ ! -f "$tmp/miss.serve" ]'
  expect "unset serve: logged as unset"            'grep -q "unset: serve (no URL configured" "$LOG"'
  expect "unset serve: heartbeat says unset"       'grep -q "serve=unset" "$tmp/upcheck.last"'
  if [ -n "$live_url" ]; then
    expect "unset serve: no sentinel at all"       '[ ! -f "$SENTINEL" ]'
    expect "unset serve: collector still watched"  'grep -q "collector=ok" "$tmp/upcheck.last"'
  fi

  if [ -z "$live_url" ]; then
    echo "lss-upcheck self-test: PARTIAL - no local target; the reachable halves were skipped"
    rm -rf "$tmp"
    return 0
  fi

  # --- the miss -> alert -> recover cycle, entirely against local URLs -------------------------
  rm -f "$LOG" "$SENTINEL" "$tmp/miss.serve"
  LSS_UPCHECK_NO_BANNER=1
  SERVE_URL="http://127.0.0.1:9/definitely-dead"; COLLECTOR_URL="$live_url"
  run_once
  expect "1 miss: no alert yet"                 '[ ! -f "$SENTINEL" ] && [ "$(cat "$tmp/miss.serve")" = "1" ]'
  run_once
  expect "2 misses: ALERT sentinel written"     'grep -q "^serve down since" "$SENTINEL"'
  expect "2 misses: banner logged"              'grep -q "banner: LLM serve NOT ANSWERING" "$LOG"'
  run_once
  expect "3 misses: no second banner"           '[ "$(grep -c "NOT ANSWERING" "$LOG")" = "1" ]'
  expect "healthy target never alerts"          '! grep -q "^collector " "$SENTINEL"'
  LSS_UPCHECK_NO_BANNER=0   # the one real banner of the test: the recovery
  SERVE_URL="$live_url"
  run_once
  expect "recovery: sentinel cleared"           '[ ! -f "$SENTINEL" ]'
  expect "recovery: banner logged"              'grep -q "banner: RECOVERED: LLM serve" "$LOG"'
  expect "recovery: miss counter reset"         '[ ! -f "$tmp/miss.serve" ]'
  echo "--- test log ($tmp/upcheck.log)"; cat "$LOG"
  if [ -n "$srv_pid" ]; then kill "$srv_pid" 2>/dev/null || true; wait "$srv_pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
  [ "$fails" -eq 0 ] && echo "lss-upcheck self-test: PASS" || { echo "lss-upcheck self-test: $fails FAILED"; return 1; }
}

if [ "${LSS_UPCHECK_TEST:-0}" = "1" ]; then self_test; else run_once; fi
