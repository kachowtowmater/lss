#!/bin/bash
# S0 only: a stand-in wizard that asks the questions of the contract in wizard.exp, in the phrasing
# a real wizard might use, and records the answers it got. Proves wizard.exp answers each one - so
# an S3 FAIL is the installer's, never the driver's.
out="${1:?answers file}"
q() { local reply; printf '%s' "$1"; read -r reply </dev/tty; printf '%s=%s\n' "$2" "$reply" >>"$out"; }
echo "== LLM SERVER STATUS setup"
q "   Found nothing on the usual ports. Engine URL (e.g. http://192.0.2.20:8000) [auto]: " engine_url
q "   API key for this server, if it needs one (Enter for none): " api_key
q "   Track electricity cost from GPU power? [y/N] " track
q "   Look up your location from your IP address (one HTTPS call to ipapi.co)? [y/N] " ip
q "   How should lss price electricity - zip / manual \$/kWh / skip? [zip]: " method
q "   Your ZIP code: " zip
q "   Install the background service? [Y/n] " service
q "   Poll interval, seconds [5] " poll
echo "done"
