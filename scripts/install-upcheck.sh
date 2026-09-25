#!/bin/bash
# Install / upgrade the up-check launchd agent on the seat (macOS). Idempotent.
#
# card #82: the up-check probes only the targets you NAME, and this installer refuses to hand you
# an agent that watches nothing. Set the URLs in ~/.config/lss/upcheck.env (the agent reads it every
# run) or export them here:
#   LSS_UPCHECK_SERVE_URL     what serves the model: the ENGINE (http://<gpu-box>:8090/v1/models for
#                             SGLang, :8000 vLLM, :11434 Ollama, :8080 llama.cpp, :1234 LM Studio)
#                             or your gateway if you run one - it is never guessed, because a guessed
#                             port can only fail and this alarms when it fails.
#   LSS_UPCHECK_COLLECTOR_URL lss-collector's /health (defaults to http://127.0.0.1:8099/health)
# The collector leg keeps its default; the serve leg has none, and installing with neither a
# configured nor a reachable target is refused rather than silently installed as a permanent alarm.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
LABEL=ai.lss.upcheck
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
ENV_FILE="${LSS_UPCHECK_ENV:-$HOME/.config/lss/upcheck.env}"

# read the site file the agent itself will read, so this installer and the agent agree
if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090  # a site file, read when it exists
  . "$ENV_FILE"
fi
SERVE_URL="${LSS_UPCHECK_SERVE_URL:-}"
COLLECTOR_URL="${LSS_UPCHECK_COLLECTOR_URL:-http://127.0.0.1:8099/health}"

if [ -z "$SERVE_URL" ]; then
  cat >&2 <<EOF
install-upcheck: refusing to install with no LLM serve target.

  There is no sensible default: the serve could be the engine on any port (SGLang :8090,
  vLLM :8000, Ollama :11434, llama.cpp :8080, LM Studio :1234) or a gateway in front of it, and
  the old default pointed at a fixed gateway port - one that exists only if you run a gateway
  there. On any other machine that probe can only fail, and this up-check would then banner
  "serve down" forever over a perfectly healthy server.

  Do one of these, then re-run:
    echo 'LSS_UPCHECK_SERVE_URL=http://<gpu-box>:8090/v1/models' >> $ENV_FILE
    echo 'LSS_UPCHECK_COLLECTOR_URL=http://<gpu-box>:8099/health' >> $ENV_FILE
  or:
    LSS_UPCHECK_SERVE_URL=http://<gpu-box>:8090/v1/models $0

  To install ONLY the collector leg (no serve target), copy the plist and the script by hand:
    cp $HERE/scripts/lss-upcheck.sh ~/.local/bin/ && cp $HERE/packaging/$LABEL.plist ~/Library/LaunchAgents/
    sed -i '' "s|__HOME__|\$HOME|g" ~/Library/LaunchAgents/$LABEL.plist
    launchctl bootstrap gui/\$(id -u) ~/Library/LaunchAgents/$LABEL.plist
  The script skips an unset serve leg (it reports "unset" and never alerts), so that is safe.
EOF
  exit 2
fi

mkdir -p "$HOME/.local/bin" "$HOME/.local/state/lss" "$HOME/Library/LaunchAgents"
install -m 0755 "$HERE/scripts/lss-upcheck.sh" "$HOME/.local/bin/lss-upcheck.sh"
sed "s|__HOME__|$HOME|g" "$HERE/packaging/$LABEL.plist" >"$PLIST"
plutil -lint "$PLIST" >/dev/null
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
sleep 2
launchctl print "gui/$(id -u)/$LABEL" | grep -E "state =|run interval|last exit code|program =" || true

# card #82 item 2: say exactly which URL this agent will ask - an installed alarm that nobody can
# read back is how a wrong target survives for weeks.
echo "install-upcheck: installed -> $HOME/.local/bin/lss-upcheck.sh (+ $PLIST), every 60 s" >&2
echo "install-upcheck:   serve     GET $SERVE_URL" >&2
echo "install-upcheck:   collector GET $COLLECTOR_URL" >&2
echo "install-upcheck:   state     ~/.local/state/lss/ (upcheck.last says what each run saw)" >&2
echo "install-upcheck: smoke it now: LSS_UPCHECK_TEST=1 $HOME/.local/bin/lss-upcheck.sh" >&2
