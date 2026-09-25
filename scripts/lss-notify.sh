#!/bin/bash
# Generic alert dispatcher for LLM SERVER STATUS - the one a STRANGER can actually receive.
#
# The collector calls whatever `alert_cmd` names as:  <alert_cmd> <severity> <message>
# (and `<alert_cmd> --flush` every `alert_flush_secs` to retry anything spooled). Any program
# that takes those two arguments works; this one exists because it needs NOTHING but a hook URL
# and a `curl`, so a stranger who does not run our fleet, has no macOS and no agent named
# '<agent>' still gets their alerts.
#
# It delivers to ONE of these, whichever you configure (first one set wins):
#   LSS_NOTIFY_NTFY       an ntfy topic URL   e.g. https://ntfy.sh/my-private-topic
#   LSS_NOTIFY_WEBHOOK    any HTTP endpoint  e.g. https://hooks.slack.com/services/...
#   LSS_NOTIFY_CMD        any program you already have: it is run as `$LSS_NOTIFY_CMD <severity> <message>`
#                         (notify-send, mail, osascript, a curl to your own bot, ...)
# Plus, on macOS, a Notification Centre banner when no hook is set - so a bare install still shows
# something. Everything else, and every delivery failure, is honest: it logs and exits 0 (the
# collector's alert must never be the thing that fails the collector).
#
# Config: ~/.config/lss/alert.env (LSS_ALERT_ENV names another file); export the vars instead if
# you prefer. This is the GENERIC sibling of scripts/lss-alert.sh, which is OUR fleet's dispatcher
# (agent mail + a banner on the seat) and reads the same file.
#
#   LSS_NOTIFY_NTFY=https://ntfy.sh/my-topic
#   LSS_NOTIFY_WEBHOOK=https://hooks.example.com/T000/B000/xxxx
#   LSS_NOTIFY_CMD="notify-send LLM-SERVER"
#   LSS_STATE_DIR=~/.local/state/lss   LSS_NOTIFY_TIMEOUT=10   LSS_NOTIFY_NO_BANNER=0
#
# Usage:  lss-notify.sh <info|warn|page|hardware> "<message>"
#         lss-notify.sh --flush                 (nothing is spooled here: prints flush flushed=0)
# Test:   LSS_NOTIFY_DRY_RUN=1 lss-notify.sh warn "install test"   (prints what it would send)
#         LSS_NOTIFY_TEST=1    lss-notify.sh warn "install test"   (prepends [TEST], sends for real)
set -uo pipefail

MODE=send
if [ "${1:-}" = "--flush" ]; then MODE=flush; fi
SEVERITY="${1:-info}"
MSG="${2:-}"
ENV_FILE="${LSS_ALERT_ENV:-$HOME/.config/lss/alert.env}"
# shellcheck disable=SC1090  # a site file, read when it exists
if [ -f "$ENV_FILE" ]; then . "$ENV_FILE"; fi
NTFY="${LSS_NOTIFY_NTFY:-}"
WEBHOOK="${LSS_NOTIFY_WEBHOOK:-}"
CMD="${LSS_NOTIFY_CMD:-}"
TIMEOUT="${LSS_NOTIFY_TIMEOUT:-10}"
TITLE="LLM SERVER"
STATE_DIR="${LSS_STATE_DIR:-$HOME/.local/state/lss}"
LOG="$STATE_DIR/alert.log"
mkdir -p "$STATE_DIR" 2>/dev/null || true

log() { echo "$(date -Iseconds) $*" >>"$LOG" 2>/dev/null || true; }

# --flush: this dispatcher spools nothing (a failed hook is logged, not queued), so it is a no-op
# that still answers in the shape the collector expects.
if [ "$MODE" = flush ]; then
  echo "lss-notify: flush flushed=0 spool=0"
  exit 0
fi

finish() {  # finish <channel> <delivered 0|1> [detail]
  local channel="$1" delivered="$2" detail="${3:-}"
  log "summary severity=$SEVERITY channel=$channel delivered=$delivered detail=$detail text=$MSG"
  echo "lss-notify: channel=$channel delivered=$delivered${detail:+ detail=$detail}"
  exit 0
}

if [ -z "$MSG" ]; then finish none 0 empty_message; fi
case "$SEVERITY" in info|warn|page|hardware) ;; *) log "status=WARN detail=unknown_severity value=$SEVERITY"; SEVERITY=warn ;; esac
[ "${LSS_NOTIFY_TEST:-0}" = "1" ] && MSG="[TEST] ${MSG}"
MSG=$(printf '%s' "$MSG" | tr '\n\r' '  ')   # one line

TEXT="[$SEVERITY] $MSG"
DRY="${LSS_NOTIFY_DRY_RUN:-0}"

# ---- a hook URL, via curl (no dependency beyond curl, which every install here already has)
send_http() {  # send_http <url> <extra curl args...> < <body>
  local url="$1"; shift
  if [ "$DRY" = "1" ]; then log "dry-run channel=http url=$url"; return 0; fi
  if ! command -v curl >/dev/null 2>&1; then log "status=FAIL channel=http detail=no_curl"; return 1; fi
  local out rc
  out=$(curl -fsS --max-time "$TIMEOUT" "$@" "$url" 2>&1); rc=$?
  [ -n "$out" ] && printf '%s\n' "$out" >>"$LOG" 2>/dev/null
  [ "$rc" -eq 0 ] && return 0
  log "status=FAIL channel=http url=$url curl_rc=$rc"
  return 1
}

if [ -n "$NTFY" ]; then
  # ntfy: the message is the body; Title/Priority/Tags ride as headers (ntfy's own API)
  if send_http "$NTFY" -H "Title: $TITLE" -H "Priority: $([ "$SEVERITY" = page ] || [ "$SEVERITY" = hardware ] && echo urgent || echo high)" \
       -H "Tags: llm_server_status" -d "$TEXT"; then
    # a dry run SENT nothing: it is reported as dry-run, never as delivered
    [ "$DRY" = "1" ] && finish ntfy 0 dry_run
    finish ntfy 1
  fi
  finish ntfy 0 http_failed
fi

if [ -n "$WEBHOOK" ]; then
  # a plain POST: `{"severity": ..., "message": ...}` - parseable by any webhook receiver
  body=$(printf '{"severity":"%s","message":"%s"}' "$SEVERITY" "$(printf '%s' "$MSG" | sed 's/\\/\\\\/g; s/"/\\"/g')")
  if send_http "$WEBHOOK" -H "Content-Type: application/json" -d "$body"; then
    [ "$DRY" = "1" ] && finish webhook 0 dry_run
    finish webhook 1
  fi
  finish webhook 0 http_failed
fi

if [ -n "$CMD" ]; then
  # your own command: run as `$LSS_NOTIFY_CMD <severity> <message>`
  if [ "$DRY" = "1" ]; then log "dry-run channel=cmd cmd=$CMD"; finish cmd 0 dry_run; fi
  # shellcheck disable=SC2086  # a deliberate word split: the value is a command + its own args
  if $CMD "$SEVERITY" "$MSG" >>"$LOG" 2>&1; then finish cmd 1; fi
  finish cmd 0 cmd_failed
fi

# ---- nothing configured: on macOS still show a Notification Centre banner (a bare install should
# see SOMETHING); everywhere else say plainly that nobody is listening yet.
if [ "$(uname -s 2>/dev/null || echo unknown)" = "Darwin" ] && [ "${LSS_NOTIFY_NO_BANNER:-0}" != "1" ] \
   && command -v osascript >/dev/null 2>&1; then
  if [ "$DRY" = "1" ]; then log "dry-run channel=banner"; finish banner 0 dry_run; fi
  if osascript -e 'on run argv' -e "display notification (item 1 of argv) with title \"$TITLE\" sound name \"Basso\"" -e 'end run' "$TEXT" >/dev/null 2>&1; then
    finish banner 1
  fi
  finish banner 0 osascript_failed
fi

log "status=SKIP reason=not_configured hint=set_LSS_NOTIFY_NTFY_or_LSS_NOTIFY_WEBHOOK_or_LSS_NOTIFY_CMD_in_$ENV_FILE severity=$SEVERITY text=$MSG"
finish none 0 not_configured
