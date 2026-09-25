#!/bin/bash
# The install e2e's assertions (card #296), run INSIDE the machine under test: a fresh ubuntu:24.04
# container (run.sh container) or this Mac under a throwaway HOME (run.sh local). Not meant to be
# called by hand - run.sh prepares the release directory and the environment. See README.md.
#
#   E2E_RELEASE_DIR  holds install.sh and lss-<triple>.tar.gz + .sha256 (what a release publishes)
#   E2E_OUT          where the per-scenario logs go
#   E2E_FLAVOR       sglang (default) | vllm | ollama | llamacpp  - which fake engine to serve
#   E2E_ZIP          the ZIP code the wizard is given (default 94103, San Francisco, CA)
#   E2E_ONLY         optional: a space-separated subset of scenarios (S0 S1 S2 S3 S4)
#
# Output: one line per assertion - PASS / FAIL / NOT-COVERED - then a summary.
# Exit: 0 = every assertion passed, 1 = at least one FAIL, 2 = the harness itself could not run
#       (including S0, the gate's own self-test, failing: then no verdict about the installer is given).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REL="${E2E_RELEASE_DIR:?}"
OUT="${E2E_OUT:?}"
FLAVOR="${E2E_FLAVOR:-sglang}"
ZIP="${E2E_ZIP:-94103}"
ONLY="${E2E_ONLY:-S0 S1 S2 S3 S4}"
HOST_PORT=18400            # the local "GitHub releases" host
PINNED_PORT=18431          # the engine the wizard / --engine-url are pointed at (not a default port)
COLLECTOR_URL="http://127.0.0.1:8099"
OS="$(uname -s)"
mkdir -p "$OUT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/lss-e2e.XXXXXX")"
PIDS=()

case "$FLAVOR" in
    sglang) DEFAULT_PORT=30000 ;; vllm) DEFAULT_PORT=8000 ;; ollama) DEFAULT_PORT=11434 ;; llamacpp) DEFAULT_PORT=8080 ;;
    *) echo "scenario: unknown flavor $FLAVOR" >&2; exit 2 ;;
esac

N_PASS=0; N_FAIL=0; N_NC=0; FAILS=""
pass() { N_PASS=$((N_PASS + 1)); printf 'PASS  %-7s %s\n' "$1" "$2"; }
fail() { N_FAIL=$((N_FAIL + 1)); FAILS="$FAILS$1 "; printf 'FAIL  %-7s %s\n' "$1" "$2"; }
nc()   { N_NC=$((N_NC + 1)); printf 'NOT-COVERED %-7s %s\n' "$1" "$2"; }
check() { local id="$1" what="$2"; shift 2; if "$@"; then pass "$id" "$what"; else fail "$id" "$what${WHY:+ - $WHY}"; fi; WHY=""; }
WHY=""

cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
    pkill -f "$WORK/" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# ------------------------------------------------------------------ the environment
command -v curl >/dev/null && command -v python3 >/dev/null && command -v expect >/dev/null || { echo "scenario: needs curl, python3 and expect" >&2; exit 2; }
[ -f "$REL/install.sh" ] || { echo "scenario: $REL/install.sh missing" >&2; exit 2; }
if command -v cargo >/dev/null 2>&1; then
    echo "scenario: cargo is on PATH ($(command -v cargo)) - this gate must prove the install needs no Rust; run.sh gives it a PATH without cargo" >&2
    exit 2
fi

# the release host: GitHub's layout, so LSS_RELEASE_BASE=<host> means what https://github.com/O/R/releases means
SITE="$WORK/site"
mkdir -p "$SITE/latest/download" "$SITE/bad/latest/download"
cp "$REL/install.sh" "$SITE/install.sh"
n_assets=0
for t in "$REL"/lss-*.tar.gz; do
    [ -e "$t" ] || continue
    cp "$t" "$t.sha256" "$SITE/latest/download/" 2>/dev/null || { echo "scenario: $t has no .sha256 next to it" >&2; exit 2; }
    # S4's host: the same file names, but every tarball is corrupted (its .sha256 is the genuine one)
    { cat "$t"; printf 'tampered'; } >"$SITE/bad/latest/download/$(basename "$t")"
    cp "$t.sha256" "$SITE/bad/latest/download/"
    n_assets=$((n_assets + 1))
done
[ "$n_assets" -gt 0 ] || { echo "scenario: no lss-*.tar.gz in $REL" >&2; exit 2; }
( cd "$SITE" && exec python3 -m http.server "$HOST_PORT" --bind 127.0.0.1 >"$OUT/release-host.log" 2>&1 ) &
PIDS+=($!); disown $!
BASE="http://127.0.0.1:$HOST_PORT"

start_engine() { # start_engine PORT -> the pid in $ENGINE_PID
    python3 "$HERE/fake_engine.py" --flavor "$FLAVOR" --port "$1" ${2:+--model "$2"} >>"$OUT/fake-engine.log" 2>&1 &
    ENGINE_PID=$!; disown "$ENGINE_PID"; PIDS+=("$ENGINE_PID")
}
wait_url() { # wait_url URL SECS
    local i=0; while [ "$i" -lt "$2" ]; do curl -fsS --max-time 2 "$1" >/dev/null 2>&1 && return 0; sleep 1; i=$((i + 1)); done; return 1
}
wait_url "$BASE/install.sh" 15 || { echo "scenario: the release host did not come up" >&2; exit 2; }

# a stranger's machine: no cargo, the usual system dirs, the shims (macOS: a launchctl that never
# touches the real login session). HOME is fresh per scenario.
SHIMS="$WORK/shims"; mkdir -p "$SHIMS"
if [ "$OS" = Darwin ]; then cp "$HERE/shims/launchctl" "$SHIMS/launchctl"; fi
BASE_PATH="$SHIMS:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

new_home() { # new_home NAME -> $H
    H="$WORK/home-$1"; mkdir -p "$H"
    export E2E_SHIM_STATE="$H/.e2e-launchd"
}
in_home() { # in_home CMD... : run with the scenario's HOME and PATH, from inside HOME (never a checkout)
    ( cd "$H" && env -i HOME="$H" USER="${USER:-e2e}" LOGNAME="${USER:-e2e}" PATH="$BASE_PATH" TERM="${TERM:-xterm}" \
        E2E_SHIM_STATE="$E2E_SHIM_STATE" LSS_RELEASE_BASE="$BASE" TMPDIR="${TMPDIR:-/tmp}" "$@" )
}
json_get() { # json_get FILE EXPR  (EXPR is python over `d`)
    python3 -c "import json,sys; d=json.load(open(sys.argv[1])); v=$2; print('' if v is None else (json.dumps(v) if isinstance(v,(dict,list,bool)) else v))" "$1" 2>/dev/null
}
status_json() { # status_json OUTFILE -> 0 if lss printed parseable JSON
    in_home "$H/.local/bin/lss" status --json >"$1" 2>"$1.err" && python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$1" 2>/dev/null
}
collector_pids() { pgrep -f "$H/.local/bin/lss-collector" 2>/dev/null; }

# ------------------------------------------------------------------ shared assertions
A_installed() { # ID
    WHY=""
    for p in lss lss-collector; do
        [ -x "$H/.local/bin/$p" ] || { WHY="$H/.local/bin/$p is missing or not executable"; return 1; }
    done
    return 0
}
A_alert_cmd_exists() {
    local cfg="$H/.config/lss/collector.toml" cmd
    [ -f "$cfg" ] || { WHY="no $cfg"; return 1; }
    cmd="$(sed -n 's/^alert_cmd *= *"\([^"]*\)".*/\1/p' "$cfg" | head -1)"
    [ -z "$cmd" ] && return 0
    [ -x "$cmd" ] || { WHY="collector.toml alert_cmd = \"$cmd\", which does not exist (a curl|bash install has no scripts/ next to it)"; return 1; }
}
A_collector_running() { # waits up to 30 s
    if wait_url "$COLLECTOR_URL/health" 30; then return 0; fi
    WHY="nothing answered $COLLECTOR_URL/health within 30 s after the install; installer's last words: $(tail -3 "$LOG" | tr '\n' ' ' | cut -c1-240)"
    return 1
}
ensure_collector_for_rest() { # when the installer did not start it, start it by hand (logged) so the rest is still tested
    HARNESS_STARTED=0
    wait_url "$COLLECTOR_URL/health" 1 && return 0
    HARNESS_STARTED=1
    [ -x "$H/.local/bin/lss-collector" ] || return 1
    echo "      (harness: starting lss-collector by hand so the assertions after this one still say something)"
    ( cd "$H" && env -i HOME="$H" PATH="$BASE_PATH" nohup "$H/.local/bin/lss-collector" --config "$H/.config/lss/collector.toml" >>"$OUT/$SC-collector-by-hand.log" 2>&1 </dev/null & )
    wait_url "$COLLECTOR_URL/health" 20
}
A_engine_up() { # MODEL : lss status --json shows the serve up and serving MODEL, within 60 s
    local want="$1" i=0 up model
    while [ "$i" -lt 30 ]; do
        if status_json "$OUT/$SC-status.json"; then
            up="$(json_get "$OUT/$SC-status.json" "d.get('serve',{}).get('up')")"
            model="$(json_get "$OUT/$SC-status.json" "d.get('serve',{}).get('model')")"
            [ "$up" = true ] && [ "$model" = "$want" ] && return 0
        fi
        sleep 2; i=$((i + 1))
    done
    WHY="after 60 s: serve.up=${up:-?} serve.model=${model:-?} (want true / $want); lss said: $(cat "$OUT/$SC-status.json.err" "$OUT/$SC-status.json" 2>/dev/null | tr '\n' ' ' | cut -c1-200)"
    return 1
}
A_config_points_at() { # URL
    local cfg="$H/.config/lss/collector.toml"
    grep -Eq "^[[:space:]]*(url|engine_url)[[:space:]]*=[[:space:]]*\"$1/?\"" "$cfg" 2>/dev/null && return 0
    WHY="$cfg has no url = \"$1\" ($(grep -E 'url' "$cfg" 2>/dev/null | tr '\n' ' ' | cut -c1-200))"
    return 1
}
# a plausible residential $/kWh for the ZIP: 94103 (San Francisco) must look like a California
# average; any other ZIP gets a wide sanity range unless E2E_RATE_MIN/E2E_RATE_MAX say otherwise
if [ "$ZIP" = 94103 ]; then RATE_MIN="${E2E_RATE_MIN:-0.20}"; RATE_MAX="${E2E_RATE_MAX:-0.50}"; RATE_WHAT="California residential average"
else RATE_MIN="${E2E_RATE_MIN:-0.05}"; RATE_MAX="${E2E_RATE_MAX:-1.00}"; RATE_WHAT="residential rate"; fi
RATE=""        # the usd_per_kwh C1 read from rates.toml - reset by EVERY A_rates_from_zip call (card #306)
DATA_MONTH=""  # the YYYY-MM the rate's data is for (effective_date)
A_rates_from_zip() {
    RATE=""; DATA_MONTH=""
    local r="$H/.config/lss/rates.toml" verdict
    [ -f "$r" ] || { WHY="no $r was written"; return 1; }
    # card #306: every rule reads KEY LINES only (comments never count), and the date is the DATA's:
    #   source = "..."        names ZIP $ZIP or EIA, AND carries the data month
    #   effective_date = ...  YYYY-MM or YYYY-MM-DD: within the last 36 months, not in the future,
    #                         and not today's full date (that is the install day, not the data's)
    #   usd_per_kwh = N       within [$RATE_MIN, $RATE_MAX]
    verdict="$(python3 - "$r" "$ZIP" "$RATE_MIN" "$RATE_MAX" <<'PY'
import datetime, re, sys
path, zipc, lo, hi = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
keys = {}
for line in open(path):
    line = line.split('#', 1)[0] if not re.match(r'\s*\w+\s*=\s*"[^"]*#', line) else line
    m = re.match(r'\s*([A-Za-z_]+)\s*=\s*(.+?)\s*$', line)
    if m and m.group(1) not in keys:
        keys[m.group(1)] = m.group(2).strip().strip('"')
def out(ok, msg, rate="", month=""):
    print(f"{'OK' if ok else 'FAIL'}|{rate}|{month}|{msg}"); sys.exit(0)
src = keys.get('source')
if src is None: out(False, 'no source = "..." key line')
if not re.search(rf'\b{re.escape(zipc)}\b|EIA', src, re.I): out(False, f'source = "{src}" names neither ZIP {zipc} nor EIA')
eff = keys.get('effective_date')
if eff is None: out(False, 'no effective_date key line: the date the rate data is for')
m = re.fullmatch(r'(\d{4})-(\d{2})(?:-(\d{2}))?', eff)
if not m: out(False, f'effective_date = "{eff}" is not YYYY-MM or YYYY-MM-DD')
today = datetime.date.today()
y, mo = int(m.group(1)), int(m.group(2))
if not 1 <= mo <= 12: out(False, f'effective_date = "{eff}": no month {mo}')
age = (today.year - y) * 12 + (today.month - mo)
if age < 0: out(False, f'effective_date = "{eff}" is in the future')
if age > 36: out(False, f'effective_date = "{eff}" is more than 36 months old')
if eff == today.isoformat(): out(False, f'effective_date = "{eff}" is TODAY - the install date, not the date of the rate data')
month = f"{y:04d}-{mo:02d}"
if month not in src: out(False, f'source = "{src}" does not carry the data month {month} of effective_date')
raw = keys.get('usd_per_kwh')
try: rate = float(raw)
except (TypeError, ValueError): out(False, f'usd_per_kwh = {raw!r} is not a number')
if not lo <= rate <= hi: out(False, f'usd_per_kwh = {rate} is outside {lo}-{hi}', raw, month)
out(True, 'ok', raw, month)
PY
)"
    # '|', not a tab: bash's read merges runs of whitespace separators, so empty fields would shift
    IFS='|' read -r v RATE DATA_MONTH msg <<<"$verdict"
    [ "$v" = OK ] && return 0
    RATE=""; DATA_MONTH=""
    WHY="$r: ${msg:-could not be read} ($(grep -v '^[[:space:]]*#' "$r" | tr '\n' ' ' | cut -c1-200))"
    return 1
}
A_status_cost() { # cost object present, priced from the ZIP rate, and says where the rate came from
    local f="$OUT/$SC-status.json" per src
    status_json "$f" || { WHY="lss status --json failed"; return 1; }
    [ "$(json_get "$f" "d.get('cost') is not None")" = true ] || { WHY="status.cost is null (cost tracking off)"; return 1; }
    per="$(json_get "$f" "d['cost'].get('current_usd_per_kwh')")"
    [ -n "$RATE" ] || { WHY="no rate to compare against: rates.toml did not pass C1, so status.cost cannot be checked against it"; return 1; }
    python3 -c "import sys; sys.exit(0 if abs(float(sys.argv[1])-float(sys.argv[2]))<1e-9 else 1)" "${per:-x}" "$RATE" 2>/dev/null \
        || { WHY="status.cost.current_usd_per_kwh=${per:-null}, rates.toml says $RATE"; return 1; }
    local eff; eff="$(json_get "$f" "d['cost'].get('effective_date')")"
    case "$eff" in "$DATA_MONTH"*) ;; *) WHY="status.cost.effective_date='$eff', but the rate data is for $DATA_MONTH"; return 1 ;; esac
    src="$(json_get "$f" "' '.join(str(v) for k,v in d['cost'].items() if isinstance(v,str))")"
    printf '%s' "$src" | grep -Eiq "\\b$ZIP\\b|EIA" || { WHY="no string in status.cost names where the rate came from (ZIP $ZIP / EIA): '$src'"; return 1; }
}
A_uninstall_clean() {
    local ulog="$OUT/$SC-uninstall.log" rc
    # a collector the HARNESS started (because the installer did not) is not the uninstaller's to stop
    if [ "${HARNESS_STARTED:-0}" = 1 ]; then pkill -f "$H/.local/bin/lss-collector" 2>/dev/null; sleep 1; HARNESS_STARTED=0; fi
    [ -x "$H/.local/bin/lss-collector" ] || { WHY="nothing was installed, so there is nothing to prove uninstall removes (a PASS here would be vacuous)"; return 1; }
    in_home bash -c "curl -fsSL '$BASE/install.sh' | bash -s -- --uninstall --yes" >"$ulog" 2>&1; rc=$?
    [ "$rc" = 0 ] || { WHY="--uninstall exited $rc: $(tail -2 "$ulog" | tr '\n' ' ')"; return 1; }
    for p in lss lss-collector lss-notify.sh; do [ -e "$H/.local/bin/$p" ] && { WHY="$H/.local/bin/$p still there"; return 1; }; done
    ls "$H/Library/LaunchAgents"/ai.lss.collector*.plist "$H/.config/systemd/user"/lss-collector*.service >/dev/null 2>&1 && { WHY="a service file is still installed"; return 1; }
    sleep 3
    if [ -n "$(collector_pids)" ]; then WHY="lss-collector is still running (pids $(collector_pids | tr '\n' ' ')) after --uninstall"; return 1; fi
    if wait_url "$COLLECTOR_URL/health" 1; then WHY="$COLLECTOR_URL/health still answers after --uninstall"; return 1; fi
}
end_scenario() { # kill whatever the scenario left behind (a FAIL above already said so)
    pkill -f "$H/.local/bin/lss-collector" 2>/dev/null
    [ -n "${ENGINE_PID:-}" ] && kill "$ENGINE_PID" 2>/dev/null
    ENGINE_PID=""
    sleep 1
}
live_cost_note() {
    local f="$OUT/$SC-status.json" live
    live="$(json_get "$f" "(d.get('cost') or {}).get('live_usd_per_hour')")"
    if [ -n "$live" ]; then pass "$SC.C2" "status.cost.live_usd_per_hour = $live (a GPU power reading was priced)"
    elif [ "$(json_get "$f" "d.get('cost') is None")" = true ] || [ ! -s "$f" ]; then nc "$SC.C2" "live \$/h: cost tracking is off here (see C3), so there is no \$/h to check"
    else nc "$SC.C2" "live \$/h: cost is on, but the collector has no GPU power reading on this machine (status.cost.live_usd_per_hour = null), so no dollar-per-hour figure is expected"; fi
}

run_installer() { # run_installer ARGS... (piped from the release host, exactly like the one-liner)
    in_home bash -c "curl -fsSL '$BASE/install.sh' | bash -s -- $*" >"$LOG" 2>&1
}

LAUNCHD_BEFORE=""; [ "$OS" = Darwin ] && LAUNCHD_BEFORE="$(/bin/launchctl list 2>/dev/null | grep -c 'ai.lss.collector')"
echo "lss install e2e: $(uname -sm), flavor $FLAVOR, ZIP $ZIP, release host $BASE ($n_assets assets), install.sh sha256 $( (sha256sum "$REL/install.sh" 2>/dev/null || shasum -a 256 "$REL/install.sh") | cut -c1-12)"

# ------------------------------------------------------------------ S0: the gate tests itself
# Two things must hold before any FAIL below can be blamed on the installer:
#  (a) wizard.exp answers every question of its contract (a stand-in wizard asks them), and
#  (b) every assertion of S2/S3 CAN pass with the programs as they are: the files the wizards are
#      to write are written here BY HAND, and the same assertion functions must then PASS.
# A FAIL in S0 exits 2: the gate is broken, and no verdict about the installer is given.
if [[ " $ONLY " == *" S0 "* ]]; then
    SC=S0; new_home s0; LOG="$OUT/S0-install.log"
    URL="http://127.0.0.1:$PINNED_PORT"
    echo "== S0  self-test: the wizard driver and the assertions (no installer feature is judged here)"
    s0_fail_before=$N_FAIL
    ans="$H/answers"
    in_home expect "$HERE/wizard.exp" 10 "$URL" "$ZIP" bash "$HERE/selftest_wizard.sh" "$ans" >"$OUT/S0-wizard.log" 2>&1
    want="engine_url=$URL api_key= track=y ip=n method=zip zip=$ZIP service= poll="
    got="$(tr '\n' ' ' <"$ans" 2>/dev/null | sed 's/ $//')"
    WHY="got: $got"; check S0.W "wizard.exp answers every contract question: $want" test "$got" = "$want"
    start_engine "$PINNED_PORT"; wait_url "$URL/v1/models" 10
    run_installer --yes --no-service
    check S0.A2 "(setup) the release installs with --yes" A_installed
    cfg="$H/.config/lss/collector.toml"
    { echo 'listen = ["127.0.0.1:8099"]'; echo "alert_cmd = \"$H/.local/bin/lss-notify.sh\""; printf '\n[[engine]]\nkind = "%s"\nurl = "%s"\n' "$FLAVOR" "$URL"; } >"$cfg"
    [ -e "$H/.local/bin/lss-notify.sh" ] || { printf '#!/bin/sh\nexit 0\n' >"$H/.local/bin/lss-notify.sh"; chmod +x "$H/.local/bin/lss-notify.sh"; }
    # a data month six months back: inside the 36-month window, never today
    S0_MONTH="$(python3 -c "import datetime as d; t=d.date.today(); m=t.year*12+t.month-1-6; print(f'{m//12:04d}-{m%12+1:02d}')")"
    s0_rates() { # s0_rates EFFECTIVE_DATE SOURCE USD_PER_KWH [extra line]
        printf 'name = "Residential average (EIA, ZIP %s)"\neffective_date = "%s"\nsource = "%s"\n%s\n\n[plan]\nkind = "flat"\nusd_per_kwh = %s\n' "$ZIP" "$1" "$2" "${4:-}" "$3" >"$H/.config/lss/rates.toml"
    }
    S0_SRC="EIA state average, $S0_MONTH, looked up from ZIP $ZIP"
    S0_RATE="$(python3 -c "import sys; print(round((float(sys.argv[1])+float(sys.argv[2]))/2, 4))" "$RATE_MIN" "$RATE_MAX")"
    s0_rates "$S0_MONTH" "$S0_SRC" "$S0_RATE"
    check S0.B1 "A_config_points_at passes on a hand-written config" A_config_points_at "$URL"
    check S0.A4 "A_alert_cmd_exists passes when the program exists" A_alert_cmd_exists
    check S0.C1 "A_rates_from_zip passes on a hand-written rates.toml" A_rates_from_zip
    # card #306 mutations: each rates.toml below is WRONG, and C1 must say so (restored after)
    must_fail() { local id="$1" what="$2"; shift 2; if "$@"; then fail "$id" "$what - the check PASSED a file it must reject"; else pass "$id" "$what (rejected: $WHY)"; fi; WHY=""; }
    s0_rates "$(date +%Y-%m-%d)" "EIA state average, $(date +%Y-%m), ZIP $ZIP" "$S0_RATE"
    must_fail S0.M1 "C1 rejects effective_date = today (the install date)" A_rates_from_zip
    s0_rates "not-a-date" "EIA state average, ZIP $ZIP" "$S0_RATE" "# data for $S0_MONTH"
    must_fail S0.M2 "C1 rejects a data month that is only in a comment" A_rates_from_zip
    s0_rates "$S0_MONTH" "EIA state average, 2019-01, looked up from ZIP $ZIP" "$S0_RATE"
    must_fail S0.M3 "C1 rejects a source whose month is not the effective_date's" A_rates_from_zip
    s0_rates "$S0_MONTH" "state average for $S0_MONTH, looked up from ZIP 00000" "$S0_RATE"
    must_fail S0.M4 "C1 rejects a source naming neither ZIP $ZIP nor EIA" A_rates_from_zip
    s0_rates "$S0_MONTH" "$S0_SRC" "0.00"
    must_fail S0.M5 "C1 rejects usd_per_kwh = 0.00" A_rates_from_zip
    s0_rates "$S0_MONTH" "$S0_SRC" "$S0_RATE"
    A_rates_from_zip >/dev/null; WHY=""
    # card #322: the collector starts HERE, after the M1-M5 rewrites and the restore above - never
    # before them. It reads rates.toml ONCE, at start (a changed rates.toml needs a restart, by
    # design), so a collector launched before the mutations kept whichever version was on disk
    # when its startup reached that read: M2's effective_date "not-a-date" or M5's 0.00. That is
    # exactly the two S0.C3 flakes seen on macOS (2 of 8 local runs), where a fresh binary starts
    # slower than in a container (0 of 12). Starting it after the last write needs no sleep: the
    # file is final before the process exists, and A5 below waits for /health as before.
    ( cd "$H" && env -i HOME="$H" PATH="$BASE_PATH" nohup "$H/.local/bin/lss-collector" --config "$cfg" >>"$OUT/S0-collector.log" 2>&1 </dev/null & )
    check S0.A5 "A_collector_running passes" A_collector_running
    check S0.A6 "A_engine_up passes (engine UP, model e2e-fake-$FLAVOR)" A_engine_up "e2e-fake-$FLAVOR"
    check S0.C3 "A_status_cost passes with THESE binaries (cost from rates.toml, its source visible)" A_status_cost
    # card #306: a C1 that FAILS must leave no rate behind for C3 to compare with. The collector
    # still prices at S0_RATE (it read the file at start), so with a stale RATE this C3 would PASS.
    mv "$H/.config/lss/rates.toml" "$H/.config/lss/rates.toml.away"
    A_rates_from_zip; WHY=""
    must_fail S0.M6 "C3 fails when C1 failed (no stale rate from an earlier scenario)" A_status_cost
    mv "$H/.config/lss/rates.toml.away" "$H/.config/lss/rates.toml"
    pkill -f "$H/.local/bin/lss-collector" 2>/dev/null; sleep 1
    check S0.A7 "A_uninstall_clean passes" A_uninstall_clean
    end_scenario
    if [ "$N_FAIL" != "$s0_fail_before" ]; then
        echo "VERDICT  HARNESS-BROKEN: S0 (the gate's self-test) failed, so the FAILs above are the gate's, not the installer's"
        exit 2
    fi
fi

# ------------------------------------------------------------------ S1: curl | bash -s -- --yes (auto-detect)
if [[ " $ONLY " == *" S1 "* ]]; then
    SC=S1; new_home s1; LOG="$OUT/S1-install.log"
    echo "== S1  curl -fsSL \$BASE/install.sh | bash -s -- --yes   (engine on its default port $DEFAULT_PORT, found by itself)"
    start_engine "$DEFAULT_PORT"; wait_url "http://127.0.0.1:$DEFAULT_PORT/v1/models" 10
    run_installer --yes; rc=$?
    check S1.A1 "the one-liner exits 0 (got $rc)" test "$rc" = 0
    check S1.A2 "lss and lss-collector are installed in ~/.local/bin" A_installed
    check S1.A3 "collector.toml and lss.toml were written" test -f "$H/.config/lss/collector.toml" -a -f "$H/.config/lss/lss.toml"
    check S1.A4 "collector.toml's alert_cmd names a program that exists" A_alert_cmd_exists
    check S1.A5 "the collector is running after the install (it answers /health)" A_collector_running
    ensure_collector_for_rest
    check S1.A6 "lss status --json: the engine is UP and serving e2e-fake-$FLAVOR" A_engine_up "e2e-fake-$FLAVOR"
    check S1.A7 "--uninstall --yes removes the programs and the service, and stops the collector" A_uninstall_clean
    end_scenario
fi

# ------------------------------------------------------------------ S2: non-interactive, engine + ZIP given as flags
if [[ " $ONLY " == *" S2 "* ]]; then
    SC=S2; new_home s2; LOG="$OUT/S2-install.log"
    URL="http://127.0.0.1:$PINNED_PORT"
    echo "== S2  curl ... | bash -s -- --yes --engine-url $URL --zip $ZIP   (CI: no questions, everything from flags)"
    start_engine "$PINNED_PORT"; wait_url "$URL/v1/models" 10
    run_installer --yes --engine-url "$URL" --zip "$ZIP"; rc=$?
    check S2.A1 "the one-liner with --engine-url/--zip exits 0 (got $rc: $(grep -m1 -i 'unknown option\|error' "$LOG" | cut -c1-120))" test "$rc" = 0
    check S2.A2 "lss and lss-collector are installed" A_installed
    check S2.B1 "collector.toml points at $URL" A_config_points_at "$URL"
    check S2.C1 "rates.toml was made from ZIP $ZIP: a source naming it and the data month, a plausible $RATE_WHAT" A_rates_from_zip
    check S2.A5 "the collector is running after the install" A_collector_running
    ensure_collector_for_rest
    check S2.A6 "lss status --json: the engine is UP and serving e2e-fake-$FLAVOR" A_engine_up "e2e-fake-$FLAVOR"
    check S2.C3 "lss status --json: a cost priced at the ZIP's rate, naming where the rate came from" A_status_cost
    live_cost_note
    check S2.A7 "--uninstall --yes is clean" A_uninstall_clean
    end_scenario
fi

# ------------------------------------------------------------------ S3: the interactive wizard, driven through a real terminal
if [[ " $ONLY " == *" S3 "* ]]; then
    SC=S3; new_home s3; LOG="$OUT/S3-wizard.log"
    URL="http://127.0.0.1:$PINNED_PORT"
    echo "== S3  curl -fsSL \$BASE/install.sh | bash   (a person at a terminal: engine URL $URL, ZIP $ZIP)"
    start_engine "$PINNED_PORT"; wait_url "$URL/v1/models" 10
    in_home expect "$HERE/wizard.exp" 30 "$URL" "$ZIP" bash -c "curl -fsSL '$BASE/install.sh' | bash" >"$LOG" 2>&1; rc=$?
    stuck="$(grep -A8 'WIZARD-STUCK' "$LOG" | tr '\r\n' '  ' | cut -c1-300)"
    check S3.A1 "the wizard runs to the end and exits 0 (got $rc)${stuck:+ - $stuck}" test "$rc" = 0
    answers="$(grep -o 'WIZARD-ANSWER [0-9]*: [^>]*' "$LOG" | sed 's/WIZARD-ANSWER [0-9]*: //; s/ -$//' | tr '\n' ',' | sed 's/,$//')"
    asked_url=0; grep -q 'WIZARD-ANSWER.*engine URL' "$LOG" && asked_url=1
    asked_zip=0; grep -q 'WIZARD-ANSWER.*ZIP code' "$LOG" && asked_zip=1
    WHY="questions the wizard asked: ${answers:-none}"; check S3.W1 "the wizard asked for the engine's URL (or found $URL by itself and wrote it)" bash -c "[ $asked_url = 1 ] || grep -q '$URL' '$H/.config/lss/collector.toml' 2>/dev/null"
    WHY="questions the wizard asked: ${answers:-none}"; check S3.W2 "the wizard asked for a ZIP code" test "$asked_zip" = 1
    check S3.A2 "lss and lss-collector are installed" A_installed
    check S3.B1 "collector.toml points at $URL" A_config_points_at "$URL"
    check S3.A4 "collector.toml's alert_cmd names a program that exists" A_alert_cmd_exists
    check S3.C1 "rates.toml was made from ZIP $ZIP: a source naming it and the data month, a plausible $RATE_WHAT" A_rates_from_zip
    check S3.A5 "the collector is running after the install" A_collector_running
    ensure_collector_for_rest
    check S3.A6 "lss status --json: the engine is UP and serving e2e-fake-$FLAVOR" A_engine_up "e2e-fake-$FLAVOR"
    check S3.C3 "lss status --json: a cost priced at the ZIP's rate, naming where the rate came from" A_status_cost
    live_cost_note
    check S3.A7 "--uninstall --yes is clean" A_uninstall_clean
    end_scenario
fi

# ------------------------------------------------------------------ S4: a tampered download is refused
if [[ " $ONLY " == *" S4 "* ]]; then
    SC=S4; new_home s4; LOG="$OUT/S4-install.log"
    echo "== S4  the release tarball does not match its .sha256: the installer must refuse and install nothing"
    in_home env LSS_RELEASE_BASE="$BASE/bad" bash -c "curl -fsSL '$BASE/install.sh' | LSS_RELEASE_BASE='$BASE/bad' bash -s -- --yes --no-service" >"$LOG" 2>&1; rc=$?
    check S4.T1 "a checksum mismatch exits non-zero (got $rc)" test "$rc" != 0
    check S4.T2 "nothing was installed" bash -c "! [ -e '$H/.local/bin/lss' ] && ! [ -e '$H/.local/bin/lss-collector' ]"
    check S4.T3 "it says the checksum did not match" grep -qi 'checksum' "$LOG"
    end_scenario
fi

# ------------------------------------------------------------------ safety: nothing escaped the throwaway HOME
if [ "$OS" = Darwin ]; then
    check Z.1 "the real launchd's ai.lss.collector agents are as they were before the run ($LAUNCHD_BEFORE)" test "$(/bin/launchctl list 2>/dev/null | grep -c 'ai.lss.collector')" = "$LAUNCHD_BEFORE"
fi

echo
echo "SUMMARY  $N_PASS passed, $N_FAIL failed, $N_NC not covered   (flavor $FLAVOR, $(uname -sm))"
[ "$N_FAIL" = 0 ] && { echo "VERDICT  PASS"; exit 0; }
echo "VERDICT  FAIL: $FAILS"
exit 1
