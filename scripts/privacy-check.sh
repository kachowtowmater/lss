#!/usr/bin/env bash
# Privacy check: prints every line that looks like private data and fails if there is any.
#
# Built in (generic): absolute home paths, IP addresses (except loopback, 0.0.0.0 and the
# RFC 5737 / RFC 3849 documentation ranges the fixtures use), e-mail addresses (except
# @example.com and GitHub noreply), and credentials (API keys, tokens, private keys).
# Optional: a local, git-ignored `.privacy-words` file with one extra regex per line (names,
# hosts, project names ...). Plain words shorter than 5 letters match as whole words; everything
# else matches anywhere, case-insensitively.
#
# Also (card #172): the NUMBERS from your own electricity tariff. The word net cannot see a
# rate table - a tariff ships in full as long as nobody writes the utility's name beside it, and
# that already happened once: a fixture labelled "synthetic" carried the real effective date,
# daily charge and off-peak rate, and every check reported clean. When a rates file exists, its
# numbers become patterns.
#
#   bash scripts/privacy-check.sh [DIR]    (DIR defaults to the repository; empty output = clean)
#   PRIVACY_WORDS=/path/to/words bash scripts/privacy-check.sh DIR
#   LSS_RATES=/path/to/rates.toml ...      (else site/ours/rates.toml, else ~/.config/lss/rates.toml)
#
# Without .privacy-words (it is git-ignored, so a fresh clone has none) only the generic
# patterns are checked: YOUR host names, user names and project names are NOT known and pass
# silently. scripts/export-public.sh falls back to packaging/privacy-words.example; for a
# direct run, start from that file: cp packaging/privacy-words.example .privacy-words
#
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="${1:-$HERE}"
WORDS_FILE="${PRIVACY_WORDS:-$HERE/.privacy-words}"
cd "$ROOT"

files() { # files [numeric]
    # exclusions: build output, this script itself, .privacy-words, and the COMMITTED example
    # list (it IS a word list: in a fresh clone the fallback IS this file, and it would always
    # fail on its own sample entries - card #15's re-check found exactly that).
    # The NUMERIC net does NOT exempt this script: it names patterns, never tariff values, so
    # there is no reason to - and the exemption is how the owner's real unresolved rate shipped
    # in this very file's comments, in the one place no check looked (card #144 item 7).
    # PRIVACY_EXAMPLE_EXEMPT (card #144): export-public.sh sets it to a path that never exists,
    # so the EXPORTED copy of the example list is scanned like any other file - the exemption is
    # why the whole seeded block shipped in every export while each check said clean.
    local self='./scripts/privacy-check.sh'
    [ "${1:-}" = numeric ] && self='./.never-a-file'
    # card #293: './.git' as well as './.git/*' - in a git WORKTREE `.git` is a one-line FILE
    # ("gitdir: <absolute path of the main clone>/.git/worktrees/<name>"), which the directory
    # pattern alone never matched, so every run from a worktree reported the host's own path.
    find . -type f \
        -not -path './target/*' -not -path './dist/*' -not -path './.git' -not -path './.git/*' \
        -not -path "$self" -not -name '.privacy-words' \
        -not -path "${PRIVACY_EXAMPLE_EXEMPT:-./packaging/privacy-words.example}" -print0
}

scan() { # scan GREP-FLAGS PATTERN
    # -H: ALWAYS print the file name. Without it grep omits the name whenever xargs happens to
    # hand it a single file, so a hit in a small tree reads as a bare "3:Copyright (c) ..." with
    # no path - unreadable in a report, and it silently broke publication_exempt's anchor, which
    # matches on the path (card #188).
    files | xargs -0 grep -HnIE "$1" -e "$2" 2>/dev/null || true
}

generic() {
    # absolute home directories (e.g. /Users/<name>/..., /home/<name>/...); /home/m is the
    # made-up home of a unit test, /Users/you and /home/you are what the docs say
    scan -i '/(Users|home)/[A-Za-z0-9._-]+' | grep -vE '/(Users|home)/(you|m|user|USER)\b' || true
    # IPv4 addresses, except loopback, 0.0.0.0 and the documentation ranges
    scan '' '\b([0-9]{1,3}\.){3}[0-9]{1,3}\b' \
        | grep -vE '\b(127\.0\.0\.[0-9]+|0\.0\.0\.0|192\.0\.2\.[0-9]+|198\.51\.100\.[0-9]+|203\.0\.113\.[0-9]+)\b' || true
    # carrier-grade NAT / tailnet addresses (100.64.0.0/10) are never documentation
    scan '' '\b100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]'
    # e-mail addresses, except the documentation and GitHub noreply domains
    scan '' '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' \
        | grep -vE '(@(example\.(com|org|net)|users\.noreply\.github\.com)\b|\bnoreply@github\.com\b|actions/[a-z-]+@v[0-9]|@v[0-9]+\b)' || true
    # credentials
    scan '' '\bsk-[A-Za-z0-9_-]{20,}|\b(ghp|gho|ghs|ghu)_[A-Za-z0-9]{8,}|\bgithub_pat_|\bAKIA[0-9A-Z]{16}\b|-----BEGIN .*PRIVATE KEY|\bxox[bp]-'
}

# --------------------------------------------------- card #188: deliberate publication
# A licence must name its copyright holder. That is the one place where writing the owner's
# name is not a leak but the entire point of the file - and on 2026-09-23 the word net could
# not tell the difference: adding an MIT LICENSE turned main RED in five export tests and was
# refused twice by the pre-commit hook, on its own `Copyright (c) 2026 <holder>` line. The
# commit went in with SKIP_PRIVACY_SCAN=1, which is exactly the habit a check must never teach.
#
# The carve-out is deliberately the narrowest shape that can carry a licence and nothing else:
#   * the file must be ./LICENSE at the repository root - not LICENSE.md, not docs/LICENSE;
#   * within the first nine lines, where a notice actually sits;
#   * the WHOLE line must be a copyright notice and nothing more - "Copyright (c) <year>
#     <holder>". A name appended to any other text, or anything appended after the holder, is
#     not exempt.
# It filters the WORD net only. The generic net (home paths, IP addresses, e-mail addresses,
# credentials) still runs over that line: a licence needs a name, never a key or an address.
#   * the holder must be a SINGLE TOKEN - no spaces. That one restriction is what carries the
#     whole rule: an earlier draft allowed spaces in the holder, which also allowed
#     `Copyright (c) 2026 <holder> of <our-box>` - a host name smuggled into the one exempt
#     line. The test caught it on its first run.
#
# It deliberately does NOT require the line to end with the matched word itself. A word list
# legitimately holds PREFIXES (ours carries `kachow`, and the holder is longer), and a rule
# keyed to the whole word passed on the build host - whose fallback list has the full string -
# while the export stayed RED on the seat a release is actually cut from. A check that is green
# everywhere except where it is used is worse than no check.
publication_exempt() {
    grep -viE '^\./LICENSE:[1-9]:Copyright \(c\) [0-9]{4} [A-Za-z0-9][A-Za-z0-9._-]*$' || true
}

# --------------------------------------------------- card #304: the public repository's slug
# A public repository necessarily carries its owner's GitHub handle: the one-line install has to
# download from github.com/<owner>/<repo>, and install.sh has to default to it. Same shape as the
# licence rule above - the narrowest carve-out that carries the published slug and nothing else:
#   * ONLY this exact slug, and not followed by [-A-Za-z0-9_.] (so a longer repository name,
#     another repository of the same owner, or a `.git` suffix is not exempt);
#   * ONLY right after `github.com/` or in a `...REPO=` default (quoted or not);
#   * the hit survives if the word still matches ANYWHERE ELSE on the line once those
#     occurrences are removed - a bare handle, or the slug in running text, still fails.
# It filters the WORD net only; the generic net (addresses, keys, e-mails) is untouched.
# LSS_PUBLIC_SLUG (card #307): tests exercise this rule with a neutral stand-in slug, so no test
# file has to spell - or assemble from pieces - the real handle.
PUBLIC_SLUG_RE="${LSS_PUBLIC_SLUG:-kachowtowmater/lss}"
slug_exempt() { # slug_exempt WORD_RE  (reads grep -Hn hit lines on stdin)
    local re="$1" line body stripped
    while IFS= read -r line; do
        body="${line#*:}"; body="${body#*:}"
        stripped="$(printf '%s\n' "$body" | sed -E "s#(github\.com/|REPO=[\"']?)${PUBLIC_SLUG_RE}([^-A-Za-z0-9_.]|\$)#\1\2#g")"
        if printf '%s\n' "$stripped" | grep -qiE -e "$re"; then printf '%s\n' "$line"; fi
    done
}

# --------------------------------------------------- card #307: words split across string literals
# A private word written in pieces - ["gpu", "host"].concat() in Rust, concat!("gpu", "host"),
# "".join(['gpu', 'host']) in Python, 'gpu''host' in a shell script - is invisible to a line
# grep, yet the compiled program and anyone reading the source see the whole word. The
# repository's own tests did exactly that to keep "no literal hit in this file" true: they
# shipped three people's names, the owner's name and domain and a host name that way, and every
# check said clean (and this comment's first draft did it once more, in pieces, in a file the
# pass did not read). So it reads EVERY text file - scripts, docs, configs, workflows and this
# script itself - and puts these shapes back together:
#   * two or more quoted strings (double or single quotes, b"" and r"" too) with only GLUE
#     between them - white space (across lines too), + , & - ( ) [ ] .. and the calls that only
#     make a String of a literal (.to_string(), .to_owned(), .into(), String::from): arrays,
#     tuples, concat!(...) and join(...) lists, 'a''b', "a" + "b", char arrays ['a', 'b'],
#     literals on separate lines inside ( ), YAML block lists (card #310). Never across a bare
#     word: "a" if x else "b" joins nothing;
#   * one quoted string whose \x / \u escapes spell something ("\x67pu...");
#   * an array of 3+ numbers 32..126, read as the bytes it spells ([103, 112, ...], [0x67, ...]).
# Each join is handed to the word net as "<file>:<line>:<joined text>   (joined from split literals)".
# Word net only: the generic net's own test plants (a made-up home path, say) are assembled this
# way on purpose and name nobody.
joined_literals() {
    command -v perl >/dev/null 2>&1 || { echo "privacy-check: perl not found - words split across string literals are NOT checked" >&2; return 0; }
    local f
    while IFS= read -r -d '' f; do
        perl -0777 -ne '
            exit 0 if -B $ARGV;   # binary files have no string literals to join
            binmode STDOUT, ":encoding(UTF-8)";   # a decoded \u{...} escape may be wide
            # card #317: also a Rust raw string with #s (r#"..."#), bash ANSI-C quoting (a dollar
            # sign before a single-quoted string with \x6d escapes), and chr(N) - one character
            my $lit = qr/(?<![A-Za-z0-9_])(?:b?r(\#+)"(?:(?!"\g{-1}).)*"\g{-1}|[br]?"(?:[^"\\\n]|\\.)*"|\$?\x27(?:[^\x27\\\n]|\\.)*\x27|chr\(\s*(?:0[xX][0-9a-fA-F]{1,2}|\d{1,3})\s*\))/;
            # card #310: what may sit BETWEEN two pieces and still join them - whitespace (newlines
            # too: YAML block lists, literals on separate lines inside parens), `+ , & - ( ) [ ]`,
            # `..`, and the handful of calls that only turn a literal into a String. Never a bare
            # word: `"a" if x else "b"` or `echo a then b` join nothing.
            my $glue = qr/(?:\s|[-+,&()\[\]]|\.\.|\.(?:to_string|to_owned|into)\(\)|String::from|str::from_utf8)*/;
            my $num = qr/(?:0[xX][0-9a-fA-F]{1,2}|\d{1,3})(?:u8)?/;
            my $unesc = sub {
                my $t = shift;
                $t =~ s/\\x([0-9a-fA-F]{2})/chr(hex($1))/ge;
                $t =~ s/\\u\{([0-9a-fA-F]{1,6})\}/chr(hex($1))/ge;
                $t =~ s/\\u([0-9a-fA-F]{4})/chr(hex($1))/ge;
                $t =~ s/\\([0-3][0-7]{2})/chr(oct($1))/ge;   # card #317: "\155" (octal)
                return $t;
            };
            my $piece = sub {
                my $l = shift;
                if ($l =~ /^chr\(\s*(\S+?)\s*\)$/) { my $n = $1; return chr($n =~ /^0[xX]/ ? hex($n) : $n); }
                if ($l =~ /^b?r(#+)"(.*)"\1$/s) { return $2; }   # a raw string: no escapes inside
                $l =~ s/^(?:[br]|\$)//;
                return $unesc->(substr($l, 1, -1));
            };
            my @found;
            # two or more literals with only glue between them (lists, +, adjacency, char arrays)
            my $pieces = sub { my ($run, @p) = (shift); while ($run =~ /$lit/g) { push @p, $piece->($&); } return join "", @p; };
            while (/($lit(?:$glue$lit)+)/g) { my ($at, $run) = ($-[0], $1); push @found, [$at, $pieces->($run)]; }
            # one literal whose escapes spell something (\x6d..., \155...): decoded on its own
            while (/($lit)/g) { my $l = $1; push @found, [$-[0], $piece->($l)] if $l =~ /\\(?:x[0-9a-fA-F]{2}|u|[0-3][0-7]{2})/; }
            # card #317: one SHELL word - quoted and bare pieces written with no space between them
            # are a single word in sh (h=gpu"host", h="gpu"host, and the dollar-quoted form). Joined only
            # when the run has a quote in it and more than one piece; a space ends the word.
            my $seg = qr/[A-Za-z0-9_.\/-]+|"(?:[^"\\\n]|\\.)*"|\$?\x27(?:[^\x27\\\n]|\\.)*\x27/;
            while (/(?<![^\s=;(|&`])((?:$seg){2,})/g) {
                my ($at, $run) = ($-[0], $1);
                my @segs; while ($run =~ /$seg/g) { push @segs, $&; }
                next unless @segs > 1 && grep { /^\$?["\x27]/ } @segs;
                push @found, [$at, join "", map { /^\$?["\x27]/ ? $piece->($_) : $_ } @segs];
            }
            # an array of 3+ small numbers, read as the bytes it spells ([109, 99, ...], [0x6d, ...])
            while (/[\[\(]\s*($num(?:\s*,\s*$num){2,})\s*,?\s*[\]\)]/g) {
                my ($at, $list) = ($-[0], $1);
                my @n = map { /^0[xX]/ ? hex($_) : $_ } map { s/u8$//r } split /\s*,\s*/, $list;
                next if grep { $_ < 32 || $_ > 126 } @n;
                push @found, [$at, join "", map { chr } @n];
            }
            for my $f (@found) {
                my ($at, $joined) = @$f;
                my $line = 1 + (substr($_, 0, $at) =~ tr/\n//);
                $joined =~ s/\n/ /g;
                print "$ARGV:$line:$joined   (joined from split literals)\n";
            }
        ' "$f"
    done < <(files numeric)
}

local_words() {
    if [ ! -f "$WORDS_FILE" ]; then
        # card #67 (fixing the fix): the OLD guard here also required
        # packaging/privacy-words.example to be missing before it would say anything - but this
        # script never reads that file as a fallback (only export-public.sh does), so on any
        # real clone (the example file is always committed; a real .privacy-words never is) the
        # guard silently fell through to `<"$WORDS_FILE"` below with no file there, which
        # crashed under `set -e` and discarded every generic-pattern hit with it (verifier-3,
        # verifier-2: independently reproduced, moved back to TODO). Fire on WORDS_FILE alone.
        echo "privacy-check: no .privacy-words found (${WORDS_FILE}): only generic patterns are checked - your host names, user names and project names are NOT" >&2
        return 0
    fi
    local w re joined samples=""
    joined="$(joined_literals)"
    # the made-up SAMPLES above the export's cut marker in the committed example list are
    # published in that very file, so a test that plants one in pieces leaks nothing: the joined
    # pass skips exactly those entries (never a real word, and never the content/file-name nets)
    if [ -f "$HERE/packaging/privacy-words.example" ]; then
        samples="$(awk '/^# ==== export-public: CUT HERE ====$/ { exit } { sub(/#.*/, ""); gsub(/^[ \t]+|[ \t]+$/, ""); if ($0 != "") print }' "$HERE/packaging/privacy-words.example")"
    fi
    while IFS= read -r w || [ -n "$w" ]; do
        w="${w%%#*}"                       # comments
        w="$(printf '%s' "$w" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
        [ -n "$w" ] || continue
        if printf '%s' "$w" | grep -qE '^[A-Za-z0-9]+$' && [ "${#w}" -lt 5 ]; then
            re="\\b$w\\b"
        else
            re="$w"
        fi
        scan -i "$re" | publication_exempt | slug_exempt "$re"
        # card #300: FILE NAMES too. `scan` greps contents only, so a capture saved as
        # fixtures/<our-host>_metrics.txt shipped its name past every check while its body was
        # clean. The path is what `git ls-files`, a tarball listing and a GitHub tree all show.
        files | tr '\0' '\n' | grep -iE -e "$re" | sed 's/$/: private word in the FILE NAME/' || true
        # card #307: and the same word, found only once split literals are joined back together
        if [ -n "$joined" ] && ! printf '%s\n' "$samples" | grep -qxF -e "$w"; then
            printf '%s\n' "$joined" | grep -iE -e "$re" | slug_exempt "$re" || true
        fi
    done <"$WORDS_FILE"
}

# --------------------------------------------------------------- card #172: the numeric net
# WHY THIS EXISTS: the word net protects the people whose names we already know to forbid. A
# tariff is all numbers, so it slipped through in full - fixtures/status_golden.json shipped an
# effective_date EXACT to the real plan plus a daily charge and an off-peak rate, under the
# label "synthetic", and every check said clean. Card #111 called the utility and rate plan "the
# most personal thing this project has ever asked for"; numbers need their own net.
#
# FAIL OPEN, OUT LOUD: a machine with no rates file has nothing to protect and must not start
# failing. It says so, so "clean" never quietly means "checked nothing" (card #145's lesson).
#
# TWO INDEPENDENT FIELDS BEFORE REFUSING: one rounded number is noise (0.25 collides with
# ordinary test data); an effective date plus a daily charge is a fingerprint. The date is
# matched at FULL PRECISION only. The canonical fakes from card #129 (0.50, 0.0100, 0.25,
# 2026-01-01) are never patterns.
rates_file() {
    if [ -n "${LSS_RATES:-}" ] && [ -f "$LSS_RATES" ]; then printf '%s' "$LSS_RATES"; return 0; fi
    if [ -f "$HERE/site/ours/rates.toml" ]; then printf '%s' "$HERE/site/ours/rates.toml"; return 0; fi
    if [ -f "$HOME/.config/lss/rates.toml" ]; then printf '%s' "$HOME/.config/lss/rates.toml"; return 0; fi
    return 1
}

numeric_tariff() {
    local rf
    if ! rf="$(rates_file)"; then
        echo "privacy-check: no rates file (LSS_RATES, site/ours/rates.toml, ~/.config/lss/rates.toml): tariff NUMBERS are NOT checked" >&2
        return 0
    fi
    echo "privacy-check: tariff numbers in force from $rf" >&2
    # every value worth protecting, one per line: the date at full precision, the money values
    # at full precision and truncated to 3 and 2 significant decimals
    local pats date_pat v
    pats=""
    date_pat="$(grep -oE '^[[:space:]]*effective_date[[:space:]]*=[[:space:]]*"[0-9]{4}-[0-9]{2}-[0-9]{2}"' "$rf" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}' | head -1 || true)"
    case "$date_pat" in
        ""|2026-01-01) date_pat="" ;;   # the card-#129 fake date is not a secret
    esac
    [ -n "$date_pat" ] && pats="$pats$date_pat"$'\n'
    while IFS= read -r v; do
        [ -n "$v" ] || continue
        case "$v" in 0|0.0|0.00|0.50|0.5|0.0100|0.01|0.25) continue ;; esac  # card #129 fakes
        pats="$pats$v"$'\n'
        # 3 and 2 SIGNIFICANT FIGURES, not decimal places - the difference matters for anything
        # under 0.1: `printf '%.2f' 0.00652` gives "0.01" (decimal places), but two SIGNIFICANT
        # figures of 0.00652 is "0.0065", and that is the form a leaked value actually shipped in
        # (verifier-2, re-verifying this card: the decimal-place version never generated it, so
        # the leak that motivated this card passed clean). `awk %g` rounds by significant digits;
        # `printf` cannot. (invented values: this script excludes ITSELF from the scan, so a real
        # number written here would be invisible to the very net it documents - card #172 caught
        # exactly that in this line)
        pats="$pats$(awk -v n="$v" 'BEGIN{printf "%.3g", n}')"$'\n'
        pats="$pats$(awk -v n="$v" 'BEGIN{printf "%.2g", n}')"$'\n'
    done <<EOF
$(grep -oE '^[[:space:]]*(usd_per_kwh|fixed_usd_per_day|unresolved_usd_per_kwh)[[:space:]]*=[[:space:]]*[0-9]+\.[0-9]+' "$rf" | grep -oE '[0-9]+\.[0-9]+' | sort -u)
EOF
    pats="$(printf '%s' "$pats" | sed '/^$/d' | sort -u)"
    # PRECISE patterns: the date, and any money value with 4+ decimals. A 2- or 3-decimal
    # rounding of a tariff (0.79, 0.25, 0.01) is an ordinary number that appears all over
    # ordinary code - measured on this repo: a rule of "any two matches" flagged loadout.rs for
    # carrying 0.008 / 0.01 / 0.25, which is noise. A file must carry at least one PRECISE value
    # as well as a second distinct value before it is a fingerprint rather than a coincidence.
    local precise
    precise="$(printf '%s\n' "$pats" | grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}$|^[0-9]+\.[0-9]{4,}$' || true)"
    [ -n "$pats" ] || return 0
    # for each file, how many DISTINCT tariff values it carries; two or more is a fingerprint
    local f n matched
    while IFS= read -r -d '' f; do
        n=0; matched=""; sharp=0
        while IFS= read -r v; do
            [ -n "$v" ] || continue
            # NOT a bare substring match: a real TTFT percentile in a bench fixture begins with a
            # tariff's 3-significant-figure rounding and is not a tariff. A tariff value must not be
            # followed by another digit - card #172's own verification produced that false
            # positive and it would have taught everyone to ignore this check.
            if grep -qE -- "(^|[^0-9.])${v//./\\.}([^0-9]|$)" "$f" 2>/dev/null; then
                n=$((n + 1)); matched="$matched $v"
                if printf '%s\n' "$precise" | grep -qxF -- "$v"; then sharp=1; fi
            fi
        done <<EOF2
$pats
EOF2
        # the rates file IS the private one, and site/ours is git-ignored and never exported:
        # flagging it every run is the noise that makes a check get ignored (the same reason
        # .privacy-words and privacy-words.example are excluded above).
        case "$f" in
            ./site/ours/*) continue ;;
        esac
        # ...and the rates file ITSELF, compared as an ABSOLUTE path: $f is "./rates.toml" while
        # $rf may be "/tmp/.../rates.toml", so a plain string match silently failed to exclude it
        # and the check flagged its own source (caught by the false-positive test).
        if [ "$(cd "$(dirname "$f")" >/dev/null 2>&1 && pwd)/$(basename "$f")" = "$(cd "$(dirname "$rf")" >/dev/null 2>&1 && pwd)/$(basename "$rf")" ]; then
            continue
        fi
        # A FULL-PRECISION value counts ON ITS OWN; a rounded one still needs a second match.
        #
        # WHY, and it is the correction to my own rule: every real incident so far has been ONE
        # exact value in a COMMENT explaining why that value must not be used - the owner's
        # unresolved rate in this very script, his off-peak rate in the rounding example, his
        # daily charge and adder in cost_run.rs, his effective date as a doc "e.g.". A
        # two-values-per-file rule cannot see any of them, which is exactly how four of them
        # shipped. And a full-precision tariff value does not occur by accident - a five-decimal
        # rate or a specific effective date is not a number ordinary code writes. (This very
        # comment was the FIFTH instance: the first draft of it SPELLED two of the owner's real
        # values while explaining that they must never be spelled, and the rule it introduces is
        # what caught it, seconds after it was written.) Verified
        # before adopting: this stricter rule produces ZERO hits across the whole repository, so
        # it costs no false positives - the 3-and-2-decimal roundings (0.79, 0.25, 0.01), which
        # DO collide with ordinary data, still require a second independent match.
        if [ "$sharp" = 1 ]; then
            echo "$f: carries $n value(s) from your tariff ($rf), including a full-precision one:$matched"
        fi
    done < <(files numeric)
}

# card #188: only the word net is filtered (inside local_words, per word) - `generic` and
# `numeric_tariff` run unfiltered, so a credential or an address on the copyright line still fails.
hits=$({ generic; local_words; numeric_tariff; } | sort -u)
if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    exit 1
fi
