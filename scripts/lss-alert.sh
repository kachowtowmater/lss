#!/bin/bash
# LLM SERVER STATUS alert dispatcher (runs on the GPU box, called by lss-collector).
# Two legs:
#
#   1. agent mail  -> an agent CLI on the seat (another machine, over ssh), tagged
#                     [FROM-lss-<this host>] so it is never mistaken for a human.
# It is ONE way to deliver an alert; `alert_cmd` in the collector's config can be any program
# that takes <severity> <message>. Without LSS_ALERT_SEAT and LSS_ALERT_AGENT it delivers
# nothing, says so in its log and still exits 0.
#   2. banner      -> macOS notification on the seat, title "LLM SERVER". Does not depend
#                     on any agent session being alive.
#
# Routing by severity:
#   info, warn            -> agent mail only
#   page, hardware        -> agent mail + banner
#   warn, mail not sent   -> banner as a fallback (a serve-down warning must land SOMEWHERE now)
#
# The mail leg is DURABLE. Fleet rule: only prompt an agent that is idle/done, so the script
# waits up to 45 s for that. If the agent stays busy (or the ssh leg fails) the mail is not lost:
# it is written to the spool, one file per message, and retried
#   * at the start of EVERY later invocation (oldest first, stops at the first failure), and
#   * by `lss-alert.sh --flush`, which the collector runs every 5 minutes.
# More than 5 spooled messages go out as ONE digest so a returning agent is not flooded.
# The spool holds at most 200 messages; beyond that the oldest is dropped, with a log line.
#
# Usage:  lss-alert.sh <info|warn|page|hardware> "<message>"
#         lss-alert.sh --flush                                retry the spool, send nothing new
# Test:   LSS_ALERT_TEST=1 lss-alert.sh info "install test"   (prefixes [TEST], sends for real)
#         LSS_ALERT_DRY_RUN=1 ...                             (logs what it would do, sends and spools nothing)
# Env:    read from ~/.config/lss/alert.env when that file exists (LSS_ALERT_ENV names another):
#         LSS_ALERT_SEAT (ssh host of the seat)  LSS_ALERT_AGENT (agent name there)
#         LSS_ALERT_AGENT_CMD (the remote CLI the agent answers; see AGENT_CMD below)
#         LSS_ALERT_AGENT_FALLBACK (more targets, space-separated, tried in order ONLY when the
#                       one before does not exist - a renamed or restarted agent - never when busy)
#         LSS_ALERT_TAG ([FROM-lss-<hostname>])  LSS_ALERT_WAIT_MS (45000)
#         LSS_STATE_DIR (~/.local/state/lss)  LSS_ALERT_SPOOL_MAX (200)  LSS_ALERT_DIGEST_OVER (5)
#         LSS_ALERT_ID  the collector's alert row id; ids whose mail got through late are
#                       appended to $LSS_STATE_DIR/mail-delivered.ids for the collector to pick up
#
# NEVER fails the caller: always exits 0. The last stdout line is machine-readable:
#   lss-alert: mail=OK|SPOOLED|FAIL|SKIP banner=OK|FAIL|SKIP delivered=0|1 spool=N
#   lss-alert: flush flushed=N spool=N                        (--flush)
# delivered=1 means a leg got through NOW; mail=SPOOLED means it is queued on disk.
set -uo pipefail

MODE=send
if [ "${1:-}" = "--flush" ]; then MODE=flush; fi
SEVERITY="${1:-info}"
MSG="${2:-}"
ENV_FILE="${LSS_ALERT_ENV:-$HOME/.config/lss/alert.env}"
# shellcheck disable=SC1090  # a site file, read when it exists
if [ -f "$ENV_FILE" ]; then . "$ENV_FILE"; fi
SEAT="${LSS_ALERT_SEAT:-}"
AGENT="${LSS_ALERT_AGENT:-}"
# AGENT_CMD: the remote CLI the agent answers (`<cmd> agent wait <name>` then
# `<cmd> agent prompt <name> <text>`). Anything that speaks that two-verb contract works;
# the shipped default is the neutral `agentctl`. Set LSS_ALERT_AGENT_CMD to your own.
AGENT_CMD="${LSS_ALERT_AGENT_CMD:-agentctl}"
FALLBACK="${LSS_ALERT_AGENT_FALLBACK:-}"
WAIT_MS="${LSS_ALERT_WAIT_MS:-45000}"
SPOOL_MAX="${LSS_ALERT_SPOOL_MAX:-200}"
DIGEST_OVER="${LSS_ALERT_DIGEST_OVER:-5}"
DIGEST_MAX_CHARS=6000
ALERT_ID="${LSS_ALERT_ID:--}"
TAG="${LSS_ALERT_TAG:-[FROM-lss-$(hostname -s 2>/dev/null || echo host)]}"
TITLE="LLM SERVER"
STATE_DIR="${LSS_STATE_DIR:-$HOME/.local/state/lss}"
LOG="$STATE_DIR/alert.log"
SPOOL="$STATE_DIR/mail-spool"
mkdir -p "$STATE_DIR" 2>/dev/null || true

log() { echo "$(date -Iseconds) $*" >>"$LOG" 2>/dev/null || true; }
# keep the log bounded (~2000 lines)
trim_log() { if [ -f "$LOG" ] && [ "$(wc -l <"$LOG")" -gt 4000 ]; then tail -n 2000 "$LOG" >"$LOG.tmp" && mv -f "$LOG.tmp" "$LOG"; fi; }
spool_files() { local f; shopt -s nullglob; for f in "$SPOOL"/*.msg; do echo "$f"; done; shopt -u nullglob; }
spool_count() { spool_files | wc -l | tr -d ' '; }

if [ -z "$SEAT" ] || [ -z "$AGENT" ]; then
  # not set up: nothing to deliver to. The alert is still in the collector's database and in this log.
  log "status=SKIP reason=not_configured hint=set_LSS_ALERT_SEAT_and_LSS_ALERT_AGENT_in_$ENV_FILE severity=$SEVERITY text=$MSG"
  if [ "$MODE" = flush ]; then echo "lss-alert: flush flushed=0 spool=$(spool_count)"; else echo "lss-alert: mail=SKIP banner=SKIP delivered=0 spool=$(spool_count)"; fi
  exit 0
fi
finish() {
  log "summary severity=$SEVERITY mail=$1 banner=$2 delivered=$3 spool=$(spool_count)"
  echo "lss-alert: mail=$1 banner=$2 delivered=$3 spool=$(spool_count)"
  trim_log
  exit 0
}

SSH=(ssh -o ConnectTimeout=10 -o BatchMode=yes "$SEAT")
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }

# deliver_mail <full mail text>: 0 = the agent was idle and got it. Otherwise MAIL_DETAIL says why.
# Card #245: LSS_ALERT_AGENT first, then each LSS_ALERT_AGENT_FALLBACK target - but a fallback is
# tried ONLY when the target before it does not exist. A busy or blocked agent exists and is
# waited for (the spool keeps the mail); redirecting it would split one conversation in two.
MAIL_DETAIL=""
DELIVERED_TO=""
deliver_mail() {
  local b out rc target
  if [ "${LSS_ALERT_DRY_RUN:-0}" = "1" ]; then log "dry-run channel=agent-mail target=$AGENT"; MAIL_DETAIL=dry_run; DELIVERED_TO=$AGENT; return 0; fi
  b=$(b64 "$1")
  for target in "$AGENT" $FALLBACK; do
    out=$("${SSH[@]}" "M=\$(printf '%s' '$b' | base64 -d); $AGENT_CMD agent wait '$target' --until idle --until done --timeout $WAIT_MS >/dev/null && $AGENT_CMD agent prompt '$target' \"\$M\"" 2>&1)
    rc=$?
    [ -n "$out" ] && printf '%s\n' "$out" >>"$LOG" 2>/dev/null
    if [ "$rc" -eq 0 ]; then
      MAIL_DETAIL=ok; DELIVERED_TO=$target
      [ "$target" != "$AGENT" ] && log "detail=delivered_to_fallback target=$target primary=$AGENT hint=the_primary_agent_does_not_exist"
      return 0
    fi
    case "$out" in
      *'"code":"timeout"'*) MAIL_DETAIL=agent_busy ;;
      *agent_not_found*)    MAIL_DETAIL=agent_not_found ;;
      *agent_blocked*)      MAIL_DETAIL=agent_blocked ;;
      # card #245: the seat has no such CLI (a site that never set LSS_ALERT_AGENT_CMD gets the
      # neutral default): say THAT, not "ssh or agent mail failed", and name the fix
      *"command not found"*)
        MAIL_DETAIL=agent_cmd_not_found
        log "hint=set_LSS_ALERT_AGENT_CMD_in_$ENV_FILE agent_cmd=$AGENT_CMD seat=$SEAT"
        return 1 ;;
      *)                    MAIL_DETAIL=ssh_or_agentmail_failed ;;
    esac
    [ "$MAIL_DETAIL" = agent_not_found ] || return 1
  done
  return 1
}

send_banner() {
  local b; b=$(b64 "[$SEVERITY] $MSG")
  if [ "${LSS_ALERT_DRY_RUN:-0}" = "1" ]; then log "dry-run channel=macos-notification"; return 0; fi
  "${SSH[@]}" "M=\$(printf '%s' '$b' | base64 -d); osascript -e 'on run argv' -e 'display notification (item 1 of argv) with title \"$TITLE\" sound name \"Basso\"' -e 'end run' \"\$M\"" >>"$LOG" 2>&1
}

# ---- the spool: $SPOOL/<8-digit sequence>-<epoch>.msg, line 1 = metadata, line 2 = mail text
spool_add() {   # <mail text> <reason>
  local seq f n oldest
  mkdir -p "$SPOOL" 2>/dev/null || return 1
  seq=$(( $(cat "$STATE_DIR/mail-spool.seq" 2>/dev/null || echo 0) + 1 ))
  echo "$seq" >"$STATE_DIR/mail-spool.seq" 2>/dev/null || return 1
  f=$(printf '%s/%08d-%s.msg' "$SPOOL" "$seq" "$(date +%s)")
  printf 'ts=%s id=%s severity=%s\n%s\n' "$(date +%s)" "$ALERT_ID" "$SEVERITY" "$1" >"$f.tmp" 2>/dev/null || return 1
  mv -f "$f.tmp" "$f" || return 1
  log "status=SPOOLED channel=agent-mail target=$AGENT reason=$2 file=$(basename "$f") spool=$(spool_count)"
  n=$(spool_count)
  while [ "$n" -gt "$SPOOL_MAX" ]; do
    oldest=$(spool_files | head -n 1)
    [ -n "$oldest" ] || break
    log "status=DROP channel=agent-mail detail=spool_full max=$SPOOL_MAX dropped=$(basename "$oldest") text=$(sed -n 2p "$oldest")"
    rm -f "$oldest"
    n=$(spool_count)
  done
  return 0
}

meta() { sed -n 1p "$1" | tr ' ' '\n' | sed -n "s/^$2=//p"; }     # meta <file> <key>
queued_at() { local t; t=$(meta "$1" ts); date -d "@$t" +%H:%M:%S 2>/dev/null || date -r "$t" +%H:%M:%S 2>/dev/null || echo "?"; }
mark_delivered() { local id; id=$(meta "$1" id); case "$id" in ''|-) ;; *) echo "$id" >>"$STATE_DIR/mail-delivered.ids" 2>/dev/null || true ;; esac; }

# flush_spool: 0 = the spool is empty afterwards. Oldest first; stops at the first failure.
FLUSHED=0
flush_spool() {
  local files=() f n text digest i more
  while IFS= read -r f; do files+=("$f"); done < <(spool_files)
  n=${#files[@]}
  [ "$n" -eq 0 ] && return 0
  if [ "${LSS_ALERT_DRY_RUN:-0}" = "1" ]; then log "dry-run detail=would_flush spool=$n"; return 1; fi
  if [ "$n" -gt "$DIGEST_OVER" ]; then
    # one message, not $n: the agent coming back from a long task must not be flooded
    digest="$TAG [digest] $n alerts were queued while $AGENT was not idle (oldest first):"
    i=0; more=0
    for f in "${files[@]}"; do
      i=$((i + 1))
      text=$(sed -n 2p "$f"); text=${text#"$TAG "}
      # past the size cap everything newer is only counted: the digest never skips around
      if [ "$more" -gt 0 ] || [ $(( ${#digest} + ${#text} )) -gt "$DIGEST_MAX_CHARS" ]; then more=$((more + 1)); continue; fi
      digest="$digest ($i) $(queued_at "$f") $text ||"
    done
    [ "$more" -gt 0 ] && digest="$digest +$more more not shown - full text in $LOG on $(hostname -s 2>/dev/null || echo the-gpu-box)"
    if deliver_mail "${digest% ||}"; then
      for f in "${files[@]}"; do mark_delivered "$f"; rm -f "$f"; done
      FLUSHED=$n
      log "status=OK channel=agent-mail target=$AGENT detail=flushed_digest count=$n"
      return 0
    fi
    log "status=FAIL channel=agent-mail target=$AGENT detail=flush_failed reason=$MAIL_DETAIL spool=$n"
    return 1
  fi
  for f in "${files[@]}"; do
    text="$(sed -n 2p "$f") (queued $(queued_at "$f"), delivered late)"
    if deliver_mail "$text"; then
      mark_delivered "$f"; rm -f "$f"; FLUSHED=$((FLUSHED + 1))
      log "status=OK channel=agent-mail target=$AGENT detail=flushed file=$(basename "$f")"
    else
      log "status=FAIL channel=agent-mail target=$AGENT detail=flush_failed reason=$MAIL_DETAIL spool=$(spool_count)"
      return 1
    fi
  done
  return 0
}

# One invocation at a time touches the spool (the collector's alert worker, its 5-minute flush
# and a human at a prompt can overlap). Without flock(1) the spool still works, unserialised.
take_lock() {
  command -v flock >/dev/null 2>&1 || return 0
  exec 9>"$STATE_DIR/alert.lock" || return 0
  flock -w 60 9 || log "status=WARN detail=lock_timeout_proceeding_unlocked"
}

if [ "$MODE" = "flush" ]; then
  take_lock
  flush_spool || true
  [ "$FLUSHED" -gt 0 ] && log "summary flush flushed=$FLUSHED spool=$(spool_count)"
  echo "lss-alert: flush flushed=$FLUSHED spool=$(spool_count)"
  trim_log
  exit 0
fi

if [ -z "$MSG" ]; then
  log "status=FAIL detail=empty_message"; finish SKIP SKIP 0
fi
case "$SEVERITY" in info|warn|page|hardware) ;; *) log "status=WARN detail=unknown_severity value=$SEVERITY"; SEVERITY=warn ;; esac
[ "${LSS_ALERT_TEST:-0}" = "1" ] && MSG="[TEST] ${MSG}"
MSG=$(printf '%s' "$MSG" | tr '\n\r' '  ')   # one line: a newline would submit a half-typed prompt

# Dedupe: the exact same alert inside 60 s is dropped (a crash-looping caller must not spam).
SUM=$(printf '%s|%s' "$SEVERITY" "$MSG" | cksum | cut -d' ' -f1)
NOW=$(date +%s)
if [ -r "$STATE_DIR/alert-last" ]; then
  read -r LAST_SUM LAST_TS <"$STATE_DIR/alert-last" || true
  if [ "${LAST_SUM:-}" = "$SUM" ] && [ $((NOW - ${LAST_TS:-0})) -lt 60 ]; then
    log "status=SKIP detail=duplicate_within_60s"; finish SKIP SKIP 0
  fi
fi
echo "$SUM $NOW" >"$STATE_DIR/alert-last" 2>/dev/null || true

take_lock
TEXT="$TAG [$SEVERITY] $MSG"
MAIL=FAIL; BANNER=SKIP
if flush_spool; then
  if deliver_mail "$TEXT"; then
    MAIL=OK; log "status=OK channel=agent-mail target=$DELIVERED_TO"
  elif spool_add "$TEXT" "$MAIL_DETAIL"; then
    MAIL=SPOOLED
  else
    log "status=FAIL channel=agent-mail target=$AGENT detail=$MAIL_DETAIL and_spool_write_failed"
  fi
elif [ "${LSS_ALERT_DRY_RUN:-0}" = "1" ]; then
  MAIL=SKIP
elif spool_add "$TEXT" "queued_behind_spool:$MAIL_DETAIL"; then
  # older mail is still waiting: keep the order, and do not make the caller wait a second time
  MAIL=SPOOLED
else
  log "status=FAIL channel=agent-mail target=$AGENT detail=spool_write_failed"
fi

WANT_BANNER=0
case "$SEVERITY" in page|hardware) WANT_BANNER=1 ;; esac
if [ "$SEVERITY" = "warn" ] && [ "$MAIL" != "OK" ]; then WANT_BANNER=1; log "detail=warn_falls_back_to_banner"; fi
if [ "$WANT_BANNER" = "1" ]; then
  if send_banner; then BANNER=OK; log "status=OK channel=macos-notification"
  else BANNER=FAIL; log "status=FAIL channel=macos-notification detail=ssh_or_osascript_failed"; fi
fi

if [ "$MAIL" = "OK" ] || [ "$BANNER" = "OK" ]; then finish "$MAIL" "$BANNER" 1; else finish "$MAIL" "$BANNER" 0; fi
