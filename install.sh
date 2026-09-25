#!/usr/bin/env bash
# LLM SERVER STATUS installer: for anyone, on any machine that serves an LLM - or that can reach
# one on the network.
#
#   curl -fsSL https://github.com/kachowtowmater/lss/releases/latest/download/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --yes      the same, asking nothing (CI, scripts)
#   ./install.sh                  from a clone: asks before each step
#   ./install.sh --dry-run        print what would happen; change nothing
#   ./install.sh --uninstall      stop and remove the service and the programs (data stays)
#
# What it does:
#   1. puts `lss` and `lss-collector` in ~/.local/bin: the release binary for this machine,
#      downloaded and checked against its published .sha256 (from a clone without a release:
#      built with cargo)
#   2. the setup wizard (`lss setup`, re-runnable any time): finds the LLM server - SGLang, vLLM,
#      llama.cpp, Ollama, LM Studio, TGI, anything OpenAI-compatible - or asks for its address
#      (this machine or another on your network) and API key, tests the connection live, sets
#      the electricity rate (ZIP code, a price you type, or skip) and writes ~/.config/lss/.
#      An existing file is never replaced without asking.
#   3. runs the collector in the background: a systemd --user unit on Linux, a launchd agent on
#      macOS - then prints the one command to type: lss
# No root, no docker, no gateway and no GPU tool are needed; each is used when it is there.
#
# Piped from curl, questions are read from the terminal (/dev/tty), never from the pipe.
set -euo pipefail

# The public repository the one-line install downloads from. release.yml re-stamps this line
# with the repository that published the release, so an installer and its binaries always come
# from the same place. --repo OWNER/NAME or $LSS_REPO override it; $LSS_RELEASE_BASE overrides
# the whole release URL (a mirror, a test server).
DEFAULT_REPO="kachowtowmater/lss"
REPO="${LSS_REPO:-}"
RELEASE_BASE="${LSS_RELEASE_BASE:-}"
TTY_IN="${LSS_TTY:-/dev/tty}"
PREFIX="$HOME/.local/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/lss"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/lss"
# Run from a file (a clone, or a downloaded copy) or piped from curl (no file, no checkout)?
SELF="${BASH_SOURCE[0]:-}"
if [ -n "$SELF" ] && [ -f "$SELF" ]; then
    PIPED=0
    HERE="$(cd "$(dirname "$SELF")" && pwd)"
else
    PIPED=1
    HERE=""
fi
YES=0
DRY=0
UNINSTALL=0
NO_SERVICE=0
NO_BUILD=0
BINARY_DIR=""
VERSION=""
BASE_PORT=8099
SETUP_ARGS=()

usage() {
    cat <<'EOF'
LLM SERVER STATUS installer

Usage: install.sh [options]        (or: curl -fsSL URL/install.sh | bash -s -- [options])

  --yes              accept every default, ask nothing
  --dry-run          print what would happen; change nothing
  --uninstall        stop and remove the service and the programs (configs and history stay)
  --prefix DIR       install lss and lss-collector into DIR (default ~/.local/bin)
  --repo OWNER/NAME  download release binaries from this GitHub repository (or $LSS_REPO;
                     default: the public repository this installer was published from)
  --version vX.Y.Z   install this release (default: the latest)
  --binary-dir DIR   install the lss and lss-collector found in DIR instead
  --no-build         never build from source (fail if no release binary fits this machine)
  --no-service       do not set up or start the background service
 the setup wizard, without its questions (all optional):
  --url ADDR         (or --engine-url) the engine's address: localhost:8000, 192.0.2.20:8000,
                     http://host:port or https://host:port
  --kind KIND        sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai | auto
  --api-key KEY      the engine's API key, if it was started with one (or $LSS_API_KEY)
  --all              several engines found: watch every one of them
  --zip ZIP | --rate USD_PER_KWH | --from-ip | --skip-cost    how to price electricity
  --force            replace existing config files (a .bak copy is kept)
  -h, --help         this help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --yes|-y) YES=1; SETUP_ARGS+=(--yes) ;;
        --dry-run) DRY=1 ;;
        --uninstall) UNINSTALL=1 ;;
        --no-service) NO_SERVICE=1 ;;
        --no-build) NO_BUILD=1 ;;
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift ;;
        --binary-dir) BINARY_DIR="${2:?--binary-dir needs a directory}"; shift ;;
        --version) VERSION="${2:?--version needs a tag like v1.0.0}"; shift ;;
        --repo) REPO="${2:?--repo needs OWNER/NAME}"; shift ;;
        --url|--engine-url) SETUP_ARGS+=(--url "${2:?$1 needs a value}"); shift ;;
        --kind|--api-key|--zip|--rate) SETUP_ARGS+=("$1" "${2:?$1 needs a value}"); shift ;;
        --all|--from-ip|--skip-cost|--force) SETUP_ARGS+=("$1") ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

if [ -z "$RELEASE_BASE" ] && [ -n "$REPO" ]; then RELEASE_BASE="https://github.com/$REPO/releases"; fi
# v1.2.1 (tb lss #302): are we inside a checkout of THIS project (its Cargo.toml and its crates
# beside this file)? Only then is there source to build from, and only then does a git origin
# name OUR repository - a downloaded install.sh kept in some other project's git folder (a home
# folder under dotfiles version control, say) must not take THAT project's releases, or build it.
IN_SOURCE=0
[ "$PIPED" = 0 ] && [ -f "$HERE/Cargo.toml" ] && [ -d "$HERE/crates/lss-collector" ] && IN_SOURCE=1
# card #86 item 2: with no --repo / $LSS_REPO, a clone looks at its OWN origin - a clone of a
# repository that publishes releases gets its binaries without anyone passing a flag.
if [ -z "$RELEASE_BASE" ] && [ -z "$REPO" ] && [ "$IN_SOURCE" = 1 ] && command -v git >/dev/null 2>&1; then
    origin=$(git -C "$HERE" remote get-url origin 2>/dev/null || true)
    # card #189: OWNER/NAME out of every remote shape the host hands out - https://.../NAME.git,
    # https://.../NAME, ssh://git@.../NAME.git and the scp form git@...:OWNER/NAME.git. The
    # trailing .git is part of the DEFAULT clone URL, and left on it every download 404s; the scp
    # form has no "github.com/" to cut at, so the whole remote used to land inside the URL.
    case "$origin" in
        *github.com[:/]*/*)
            slug="${origin#*github.com}"   # ":OWNER/NAME.git" or "/OWNER/NAME.git"
            slug="${slug#[:/]}"
            slug="${slug%/}"               # a trailing slash, then the trailing .git
            slug="${slug%.git}"
            slug="${slug%/}"
            [ -n "$slug" ] && RELEASE_BASE="https://github.com/$slug/releases"
            ;;
    esac
fi
# card #299: piped from curl there is no checkout to build from and no origin to look at - the
# public repository is the only place the programs can come from. v1.2.1 (tb lss #302): the same
# holds for a DOWNLOADED copy (`curl -fsSLo install.sh <release URL>; bash install.sh`): outside a
# source checkout (IN_SOURCE, above) there is nothing to build either, so it downloads from
# the repository that published it too. Before, only the piped form did, and the downloaded one
# stopped with 'No release binary fits this machine'. Inside a clone, its own origin (above) or a
# build from source still decides.
if [ -z "$RELEASE_BASE" ] && [ "$IN_SOURCE" = 0 ] && [ -z "$BINARY_DIR" ] && [ "$UNINSTALL" = 0 ]; then
    REPO="$DEFAULT_REPO"
    RELEASE_BASE="https://github.com/$REPO/releases"
fi

say() { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
run() { # run CMD...   (printed, and skipped, in a dry run)
    if [ "$DRY" = 1 ]; then say "   would run: $*"; else "$@"; fi
}
stop_nohup_collectors() { # card #328: stop every collector a pid file of ours names - ONLY a live
    # lss-collector (#320's check: a pid file outliving its process may name a reused pid, which is
    # left alone) - and remove the pid files. A service manager that takes over (systemd --user,
    # launchd) and the nohup restart both come here first, so the old background collector is gone
    # and :8099 is free before the new one starts; --uninstall comes here too.
    local pidfile pid stopped=""
    for pidfile in "$STATE_DIR"/lss-collector*.pid; do
        [ -e "$pidfile" ] || continue
        pid="$(cat "$pidfile" 2>/dev/null || true)"
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && ps -p "$pid" -o args= 2>/dev/null | grep -q "lss-collector"; then
            run kill "$pid" || true
            say "   stopped the collector started earlier in the background (pid $pid)"
            stopped="$stopped $pid"
        elif [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            say "   left pid $pid alone: $pidfile names it, but it is not an lss-collector"
        fi
        run rm -f "$pidfile"
    done
    wait_gone $stopped
}
wait_gone() { # wait_gone PID...: up to 5 s until they are gone - the next collector binds the same
    # address at once
    [ "$#" -gt 0 ] && [ "$DRY" != 1 ] || return 0
    local n=0 pid alive
    while [ "$n" -lt 50 ]; do
        alive=0
        for pid in "$@"; do kill -0 "$pid" 2>/dev/null && alive=1; done
        [ "$alive" = 0 ] && break
        sleep 0.1; n=$((n + 1))
    done
}
stop_hand_started_collectors() { # v1.2.1: --uninstall also stops a collector started BY HAND (no
    # pid file), found by what it runs - but only one that is certainly THIS install's: its
    # --config is in $CONFIG_DIR, or it has no --config (so it reads this install's default) and it
    # is $PREFIX/lss-collector itself. Any other lss-collector of this user is left running, with its
    # pid and the command that stops it. Only processes whose PROGRAM is lss-collector count (a
    # `tail -f .../lss-collector.log` is none of our business). Same stop as stop_nohup_collectors.
    local pid cmd rest conf ours stopped=""
    while read -r pid cmd rest; do
        [ -n "$pid" ] && [ "$pid" != "$$" ] || continue
        [ "${cmd##*/}" = lss-collector ] || continue
        conf=""
        case " $rest " in
            *" --config "*) conf="${rest#*--config }"; conf="${conf%% *}" ;;
            *" --config="*) conf="${rest#*--config=}"; conf="${conf%% *}" ;;
        esac
        ours=0
        if [ -n "$conf" ]; then
            case "$conf" in "$CONFIG_DIR"/*) ours=1 ;; esac
        elif [ "$cmd" = "$PREFIX/lss-collector" ]; then
            ours=1
        fi
        if [ "$ours" = 1 ]; then
            run kill "$pid" || true
            say "   $([ "$DRY" = 1 ] && echo 'would stop' || echo stopped) a collector started by hand (pid $pid: $cmd${rest:+ $rest})"
            stopped="$stopped $pid"
        else
            say "   left pid $pid alone: an lss-collector that is not certainly this install's ($cmd${rest:+ $rest}). If it should go too:  kill $pid"
        fi
    done < <(ps -U "$(id -u)" -o pid= -o args= 2>/dev/null)
    # shellcheck disable=SC2086
    wait_gone $stopped
}
ask() { # ask "question" -> 0 = yes. Default yes; --yes and a dry run never ask.
    if [ "$YES" = 1 ] || [ "$DRY" = 1 ]; then return 0; fi
    local reply=""
    printf '%s [Y/n] ' "$1"
    # the terminal, never stdin: piped from curl, stdin is this script
    if { exec 3<"$TTY_IN"; } 2>/dev/null; then read -r reply <&3 || reply=""; exec 3<&-; else say "(no terminal to ask: taking the default, yes)"; fi
    case "$reply" in n|N|no|NO) return 1 ;; *) return 0 ;; esac
}

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Linux) SERVICE_KIND="systemd" ;;
    Darwin) SERVICE_KIND="launchd" ;;
    *) SERVICE_KIND="none" ;;
esac
UNIT_DIR="$HOME/.config/systemd/user"
AGENT_DIR="$HOME/Library/LaunchAgents"
AGENT_LABEL="ai.lss.collector"

unit_name() { if [ "$1" = 1 ]; then echo "lss-collector"; else echo "lss-collector-$1"; fi; }
config_name() { if [ "$1" = 1 ]; then echo "collector.toml"; else echo "collector-$1.toml"; fi; }
start_line() { # start_line INDEX: the one command that starts collector INDEX in the background
    # exactly like the no-session fallback does (card #301): nohup, its log, and the pid file that
    # --uninstall and the next install read
    local name; name="$(unit_name "$1")"
    echo "nohup $PREFIX/lss-collector --config $CONFIG_DIR/$(config_name "$1") >>$STATE_DIR/$name.log 2>&1 & echo \$! > $STATE_DIR/$name.pid"
}

# ------------------------------------------------------------------ uninstall
if [ "$UNINSTALL" = 1 ]; then
    step "Removing the service and the programs (your configs in $CONFIG_DIR and history in $STATE_DIR stay)"
    # E4, 2026-09-20: --uninstall used to remove things unasked, even without --yes.
    # card #320 (6): and with NO terminal, ask() takes its default (yes) - so a removal nobody
    # confirmed still happened. Removing needs a person's yes or an explicit --yes.
    if [ "$YES" != 1 ] && [ "$DRY" != 1 ] && ! { exec 3<"$TTY_IN"; } 2>/dev/null; then
        say "   no terminal to ask, and no --yes: nothing was removed. To remove without a question:  install.sh --uninstall --yes"
        exit 1
    fi
    exec 3<&- 2>/dev/null || true
    if ! ask "Stop the service and remove lss and lss-collector from $PREFIX?"; then
        say "   left everything in place."
        exit 0
    fi
    # card #180 gate 6: a machine with systemctl but no user session (a container, an ssh session
    # without lingering) printed a raw D-Bus error here
    if [ "$SERVICE_KIND" = systemd ] && command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
        for unit in "$UNIT_DIR"/lss-collector*.service; do
            [ -e "$unit" ] || continue
            run systemctl --user disable --now "$(basename "$unit")" || true
            run rm -f "$unit"
        done
        run systemctl --user daemon-reload || true
    elif [ "$SERVICE_KIND" = systemd ]; then
        for unit in "$UNIT_DIR"/lss-collector*.service; do
            [ -e "$unit" ] || continue
            run rm -f "$unit"
        done
    elif [ "$SERVICE_KIND" = launchd ]; then
        for plist in "$AGENT_DIR/$AGENT_LABEL"*.plist; do
            [ -e "$plist" ] || continue
            run launchctl unload "$plist" || true
            run rm -f "$plist"
        done
    fi
    # a collector this installer started itself (no service manager here): its pid file
    stop_nohup_collectors
    # and one started by hand, with no pid file (v1.2.1)
    stop_hand_started_collectors
    run rm -f "$PREFIX/lss" "$PREFIX/lss-collector" "$PREFIX/lss-notify.sh"
    say "Done."
    exit 0
fi

say ""
say "LLM SERVER STATUS - install"
say "  1 the programs   2 the setup wizard (your LLM server, a live test, the electricity rate)"
say "  3 the background collector      Nothing needs root. --help lists every option."

# ------------------------------------------------------------------ 1. the programs
step "1/3  The programs: lss (the screen) and lss-collector (the background part)"
target_triple() {
    case "$OS-$ARCH" in
        Linux-x86_64|Linux-amd64) echo "x86_64-unknown-linux-musl" ;;
        Linux-aarch64|Linux-arm64) echo "aarch64-unknown-linux-musl" ;;
        Darwin-arm64) echo "aarch64-apple-darwin" ;;
        Darwin-x86_64) echo "x86_64-apple-darwin" ;;
        *) echo "" ;;
    esac
}
sha256_of() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1; }
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
have_programs() { [ -x "$STAGE/lss" ] && [ -x "$STAGE/lss-collector" ]; }

if [ -n "$BINARY_DIR" ]; then
    cp "$BINARY_DIR/lss" "$BINARY_DIR/lss-collector" "$STAGE/" || { echo "install.sh: $BINARY_DIR must hold lss and lss-collector" >&2; exit 1; }
    [ -f "$BINARY_DIR/lss-notify.sh" ] && cp "$BINARY_DIR/lss-notify.sh" "$STAGE/"
    chmod +x "$STAGE/lss" "$STAGE/lss-collector"
    say "   using the programs in $BINARY_DIR"
fi
# card #180 gate 6: measured in a clean ubuntu:24.04 (which ships without curl) - say that it is
# the DOWNLOADER that is missing, not the release.
NO_CURL_FOR_RELEASE=0
if ! have_programs && [ -n "$RELEASE_BASE" ] && [ -n "$(target_triple)" ] && ! command -v curl >/dev/null 2>&1; then
    NO_CURL_FOR_RELEASE=1
    say "   a release binary may exist for $(target_triple), but curl is not installed to download it"
fi
DOWNLOAD_FAILED=""
if ! have_programs && [ -n "$RELEASE_BASE" ] && [ -n "$(target_triple)" ] && command -v curl >/dev/null 2>&1; then
    triple="$(target_triple)"
    if [ -n "$VERSION" ]; then url="$RELEASE_BASE/download/$VERSION/lss-$triple.tar.gz"; else url="$RELEASE_BASE/latest/download/lss-$triple.tar.gz"; fi
    say "   the release for this machine ($triple): $url"
    if [ "$DRY" = 1 ]; then
        say "   would download $url (and its .sha256), and install it only if the checksum matches"
    else
        # card #299: the HTTP status says WHICH failure it was - no such release (404: wrong
        # repository name, no release yet, or a private repository) is not the same as no network
        code="$(curl -sSL --max-time 120 -o "$STAGE/lss.tar.gz" -w '%{http_code}' "$url" 2>"$STAGE/curl.err" || true)"
        if [ "$code" = 200 ]; then
            sumcode="$(curl -sSL --max-time 30 -o "$STAGE/lss.tar.gz.sha256" -w '%{http_code}' "$url.sha256" 2>/dev/null || true)"
            if [ "$sumcode" != 200 ]; then
                DOWNLOAD_FAILED="the release has no checksum file ($url.sha256: HTTP ${sumcode:-none}) - not installed unchecked"
            else
                want="$(cut -d' ' -f1 "$STAGE/lss.tar.gz.sha256" | tr -d '[:space:]')"
                got="$(sha256_of "$STAGE/lss.tar.gz")"
                if [ -n "$want" ] && [ "$want" = "$got" ]; then
                    tar -xzf "$STAGE/lss.tar.gz" -C "$STAGE"
                    say "   downloaded; sha256 matches the published checksum ($got)"
                else
                    DOWNLOAD_FAILED="the download does NOT match its published checksum (want ${want:-nothing}, got $got) - not installed"
                    rm -f "$STAGE/lss" "$STAGE/lss-collector"
                fi
            fi
        elif [ "$code" = 404 ]; then
            DOWNLOAD_FAILED="no such release: $url answered 404. The repository name may be wrong, it may have no release for $triple yet, or it may be private. Point at another with --repo OWNER/NAME (or LSS_REPO)."
        else
            DOWNLOAD_FAILED="could not download $url (HTTP ${code:-none}: $(head -c 200 "$STAGE/curl.err" 2>/dev/null | tr '\n' ' ')) - is this machine online?"
        fi
        [ -n "$DOWNLOAD_FAILED" ] && say "   $DOWNLOAD_FAILED"
    fi
elif ! have_programs && [ -n "$RELEASE_BASE" ] && [ -z "$(target_triple)" ]; then
    say "   there is no release binary for $OS $ARCH"
fi
if ! have_programs && [ "$DRY" != 1 ]; then
    if [ "$IN_SOURCE" = 0 ]; then
        # piped from curl, or a copy of this file outside our source checkout: nothing to build from
        echo "install.sh: could not get the programs for $OS $ARCH. ${DOWNLOAD_FAILED:-No release binary fits this machine.}" >&2
        [ "$NO_CURL_FOR_RELEASE" = 1 ] && echo "  Install curl (Debian/Ubuntu: sudo apt install curl · Fedora: sudo dnf install curl) and run this again." >&2
        echo "  Or build from source: git clone the repository, then ./install.sh there (needs Rust: https://rustup.rs). Nothing was installed." >&2
        exit 1
    fi
    if [ "$NO_BUILD" = 1 ]; then echo "install.sh: no release binary fits this machine and --no-build was given. ${DOWNLOAD_FAILED}" >&2; exit 1; fi
    if ! command -v cargo >/dev/null 2>&1; then
        if [ "$NO_CURL_FOR_RELEASE" = 1 ]; then
            echo "install.sh: curl is not installed, so the release binary for $(target_triple) could not be downloaded, and there is no Rust toolchain to build one." >&2
            echo "  Install curl (Debian/Ubuntu: sudo apt install curl · Arch: sudo pacman -S curl · Fedora: sudo dnf install curl) and run this again," >&2
            echo "  or install Rust (https://rustup.rs) to build from this checkout. Nothing was installed." >&2
            exit 1
        fi
        echo "install.sh: no release binary for $OS $ARCH and no Rust toolchain to build one. ${DOWNLOAD_FAILED}" >&2
        echo "  Install Rust (https://rustup.rs), then run this again from a clone of the repository." >&2
        exit 1
    fi
    if ask "   Build lss from this checkout with cargo (a few minutes)?"; then
        (cd "$HERE" && cargo build --release -p lss -p lss-collector)
        cp "$HERE/target/release/lss" "$HERE/target/release/lss-collector" "$STAGE/"
    else
        say "Nothing installed."; exit 0
    fi
fi
# the alert dispatcher (card #81): in the release tarball, or next to this installer in a clone
if [ ! -f "$STAGE/lss-notify.sh" ] && [ -n "$HERE" ] && [ -f "$HERE/scripts/lss-notify.sh" ]; then cp "$HERE/scripts/lss-notify.sh" "$STAGE/"; fi
if ask "   Install them into $PREFIX?"; then
    run mkdir -p "$PREFIX"
    for program in lss lss-collector; do
        # write next to the old one, then swap: a running collector keeps its old file until restarted
        run install -m 0755 "$STAGE/$program" "$PREFIX/$program.new"
        run mv "$PREFIX/$program.new" "$PREFIX/$program"
        if [ "$OS" = Darwin ] && [ "$DRY" != 1 ]; then codesign --sign - --force "$PREFIX/$program" >/dev/null 2>&1 || true; fi
    done
    if [ -f "$STAGE/lss-notify.sh" ]; then
        run install -m 0755 "$STAGE/lss-notify.sh" "$PREFIX/lss-notify.sh"
    else
        say "   note: no lss-notify.sh came with these programs: alerts go nowhere until alert_cmd names a command (packaging/alert.env.example)"
    fi
else
    say "Nothing installed."; exit 0
fi
case ":$PATH:" in *":$PREFIX:"*) ;; *) say "   note: $PREFIX is not on your PATH - add it to your shell profile:  export PATH=\"$PREFIX:\$PATH\"" ;; esac
COLLECTOR="$PREFIX/lss-collector"
[ "$DRY" = 1 ] && COLLECTOR="$STAGE/lss-collector"

# ------------------------------------------------------------------ 2. the setup wizard
step "2/3  Setup: your LLM server, a live test, the electricity rate"
SETUP_ARGS+=(--no-service --config-dir "$CONFIG_DIR" --prefix "$PREFIX")
N_UNITS=1
has_wizard() { [ -x "$COLLECTOR" ] && "$COLLECTOR" setup --help </dev/null >/dev/null 2>&1; }
if [ "$DRY" = 1 ]; then
    say "   would run: lss-collector setup ${SETUP_ARGS[*]}"
    say "   (it asks before writing anything in $CONFIG_DIR, and never replaces a file without asking)"
elif has_wizard; then
    # the wizard reads the terminal itself (/dev/tty); its stdin is never this script
    if { exec 3<"$TTY_IN"; } 2>/dev/null; then exec 3<&-; wiz_in="$TTY_IN"; else wiz_in=/dev/null; fi
    set +e
    "$COLLECTOR" setup "${SETUP_ARGS[@]}" <"$wiz_in"
    wiz=$?
    set -e
    case "$wiz" in
        0) ;;
        3) say ""; say "Setup stopped. The programs are installed: run  lss setup  when you are ready."; exit 0 ;;
        *) echo "install.sh: the setup wizard failed (exit $wiz). The programs are installed; run  lss setup  to try again." >&2; exit 1 ;;
    esac
    N_UNITS=1
    while [ -e "$CONFIG_DIR/$(config_name $((N_UNITS + 1)))" ]; do N_UNITS=$((N_UNITS + 1)); done
else
    # an lss-collector from before the wizard (an older release, --binary-dir): what the
    # installer always did - find the server(s), write configs that are not there yet
    say "   (this lss-collector has no setup wizard: an older version - finding the server the old way)"
    DETECTED="$("$COLLECTOR" --config /dev/null --detect </dev/null 2>/dev/null || true)"
    say "$DETECTED" | sed 's/^/   /'
    ENGINES="$(printf '%s\n' "$DETECTED" | awk -F'"' '/kind = "/{k=$2} /url = "/{ if (k != "") print k, $2; k="" }')"
    N_ENGINES="$(printf '%s' "$ENGINES" | grep -c . || true)"
    if [ "$N_ENGINES" = 0 ]; then
        say "   None answers right now. That is fine: the collector keeps looking every 15 seconds and"
        say "   picks your server up as soon as it is running (or pin it: [[engine]] in collector.toml)."
    fi
    write_new() { # write_new PATH  (content on stdin)
        if [ -e "$1" ]; then say "   kept $1 (it exists)"; cat >/dev/null; return 0; fi
        mkdir -p "$(dirname "$1")"
        cat >"$1"
        say "   wrote $1"
    }
    collector_config() { # collector_config INDEX KIND URL
        local port=$((BASE_PORT + $1 - 1))
        echo "# written by install.sh. Every key and its default: packaging/collector.toml.example"
        echo "listen = [\"127.0.0.1:$port\"]"
        if [ "$1" != 1 ]; then echo "db_path = \"$STATE_DIR/lss-$1.db\""; fi
        [ -x "$PREFIX/lss-notify.sh" ] && echo "alert_cmd = \"$PREFIX/lss-notify.sh\"   # called as: alert_cmd <severity> <message>; set LSS_NOTIFY_NTFY/WEBHOOK/CMD in $CONFIG_DIR/alert.env"
        if [ -n "$2" ]; then
            printf '\n[[engine]]\nkind = "%s"\nurl = "%s"\n' "$2" "$3"
        else
            printf '\n# engine_url = "auto": the collector finds the LLM server by itself (lss-collector --detect)\n'
        fi
    }
    if ask "   Write the configs?"; then
        if [ "$N_ENGINES" -le 1 ]; then
            collector_config 1 "" "" | write_new "$CONFIG_DIR/collector.toml"
            printf '# written by install.sh\nurl = "http://127.0.0.1:%s"\n' "$BASE_PORT" | write_new "$CONFIG_DIR/lss.toml"
        else
            i=0
            servers="# written by install.sh: one [[server]] per LLM server on this machine ([ and ] switch)"
            while read -r kind url; do
                [ -n "$kind" ] || continue
                i=$((i + 1))
                collector_config "$i" "$kind" "$url" | write_new "$CONFIG_DIR/$(config_name "$i")"
                servers="$servers"$'\n'"[[server]]"$'\n'"name = \"$kind\""$'\n'"url = \"http://127.0.0.1:$((BASE_PORT + i - 1))\""
            done <<<"$ENGINES"
            printf '%s\n' "$servers" | write_new "$CONFIG_DIR/lss.toml"
            N_UNITS=$i
        fi
    fi
    say "   electricity cost: write $CONFIG_DIR/rates.toml by hand (packaging/rates.toml.example), or upgrade and run  lss setup"
fi

# ------------------------------------------------------------------ 3. the background service
step "3/3  Running the collector in the background"
systemd_unit() { # systemd_unit INDEX
    cat <<EOF
[Unit]
Description=LLM SERVER STATUS collector$([ "$1" = 1 ] || echo " ($1)")
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart=$PREFIX/lss-collector --config $CONFIG_DIR/$(config_name "$1")
Restart=always
RestartSec=5
SyslogIdentifier=$(unit_name "$1")
Nice=5
MemoryHigh=256M
MemoryMax=512M

[Install]
WantedBy=default.target
EOF
}
launchd_plist() { # launchd_plist INDEX
    local label="$AGENT_LABEL"
    [ "$1" = 1 ] || label="$AGENT_LABEL-$1"
    cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$PREFIX/lss-collector</string><string>--config</string><string>$CONFIG_DIR/$(config_name "$1")</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>$STATE_DIR/$(unit_name "$1").log</string>
  <key>StandardErrorPath</key><string>$STATE_DIR/$(unit_name "$1").log</string>
</dict></plist>
EOF
}
if [ "$NO_SERVICE" = 1 ]; then
    say "   skipped (--no-service). Start it yourself:  $PREFIX/lss-collector"
elif [ "$SERVICE_KIND" = systemd ] && command -v systemctl >/dev/null 2>&1 && { [ "$DRY" = 1 ] || systemctl --user show-environment >/dev/null 2>&1; }; then
    if ask "   Set it up as a systemd --user service (starts at login, restarts by itself)?"; then
        for i in $(seq 1 "$N_UNITS"); do
            if [ "$DRY" = 1 ]; then say "   would write $UNIT_DIR/$(unit_name "$i").service"; else mkdir -p "$UNIT_DIR" "$STATE_DIR"; systemd_unit "$i" >"$UNIT_DIR/$(unit_name "$i").service"; fi
        done
        # card #328: a collector the nohup fallback started earlier would still hold :8099
        stop_nohup_collectors
        run systemctl --user daemon-reload
        for i in $(seq 1 "$N_UNITS"); do run systemctl --user enable --now "$(unit_name "$i")"; run systemctl --user restart "$(unit_name "$i")"; done
        # card #328: $USER is not set everywhere (a container, cron, some su shells) and this script
        # runs under `set -u` - it died here, right after enabling the unit
        me="${USER:-$(id -un 2>/dev/null || echo "$(id -u)")}"
        if command -v loginctl >/dev/null 2>&1 && ! loginctl show-user "$me" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
            say "   note: to keep it running while you are logged out:  sudo loginctl enable-linger $me"
        fi
    fi
elif [ "$SERVICE_KIND" = launchd ]; then
    if ask "   Set it up as a launchd agent (starts at login, restarts by itself)?"; then
        stop_nohup_collectors  # card #328, as for systemd above
        for i in $(seq 1 "$N_UNITS"); do
            label="$AGENT_LABEL"; [ "$i" = 1 ] || label="$AGENT_LABEL-$i"
            if [ "$DRY" = 1 ]; then say "   would write $AGENT_DIR/$label.plist"; else mkdir -p "$AGENT_DIR" "$STATE_DIR"; launchd_plist "$i" >"$AGENT_DIR/$label.plist"; fi
            run launchctl unload "$AGENT_DIR/$label.plist" 2>/dev/null || true
            run launchctl load "$AGENT_DIR/$label.plist"
        done
    fi
else
    # card #296 gate: no systemd --user session and no launchd (a container, a minimal VM). The
    # collector is still STARTED - in the background, until this machine restarts - instead of
    # an install that ends "Done" with nothing running.
    say "   no systemd --user session and no launchd here (a container?)"
    if ask "   Start the collector in the background now (it runs until this machine restarts)?"; then
        # the previous run's background collector(s), checked like everywhere else (card #328)
        stop_nohup_collectors
        for i in $(seq 1 "$N_UNITS"); do
            name="$(unit_name "$i")"
            pidfile="$STATE_DIR/$name.pid"
            if [ "$DRY" = 1 ]; then
                say "   would run: nohup $PREFIX/lss-collector --config $CONFIG_DIR/$(config_name "$i") >>$STATE_DIR/$name.log 2>&1 &"
                continue
            fi
            mkdir -p "$STATE_DIR"
            nohup "$PREFIX/lss-collector" --config "$CONFIG_DIR/$(config_name "$i")" </dev/null >>"$STATE_DIR/$name.log" 2>&1 &
            echo $! >"$pidfile"
            say "   started $name (pid $!, log $STATE_DIR/$name.log)"
        done
        # card #301 (re-verification): the old line ('lss-collector --config ... &') started it with
        # no nohup, no log, and left the pid file naming the pre-reboot process - so --uninstall and
        # the next install could not find the collector it started. The same start as above, instead:
        say "   after a restart, start it again with:"
        for i in $(seq 1 "$N_UNITS"); do say "     $(start_line "$i")"; done
    else
        say "   Start it yourself:"
        for i in $(seq 1 "$N_UNITS"); do
            say "     $(start_line "$i")"
        done
    fi
fi

# ------------------------------------------------------------------ done
step "Done"
say "   Type:   lss            the live screen (? shows the keys, q quits)"
say "           lss status     the same as one page of text"
say "           lss setup      change the LLM server, its API key or the electricity rate"
say "           lss bench quick   measure this model in ~5 minutes (only while the server is idle)"
say "   Which numbers your engine publishes, and what 'n/a' means: docs/ENGINES.md"
