# The install e2e: the definition of done for the installer

`scripts/e2e/run.sh` installs lss the way a stranger would, on a machine that has never seen it, and
checks the result from the outside. It is the acceptance gate for the installer, the setup wizard
and the cost wizard. The work is done when it exits 0 in both places:

```bash
scripts/e2e/run.sh local                        # this Mac (or Linux box), under a throwaway HOME
scripts/e2e/run.sh container --host gpu-box     # a fresh ubuntu:24.04 container on a docker host
```

Exit codes: **0** PASS (every assertion passed) · **1** FAIL (the `FAIL` lines say which one and why) ·
**2** the harness could not run, including its own self-test (S0) failing.

## What it sets up

| piece | what it is |
|---|---|
| `fake_engine.py` | a local LLM server, standard-library Python only. It answers `/v1/models`, Prometheus `/metrics` and each engine's own endpoints in the shapes lss reads. `--flavor sglang\|vllm\|ollama\|llamacpp` picks which engine it imitates, on that engine's default port. The model id is `e2e-fake-<flavor>`, so a check can tell this server apart from any real engine on the machine. All numbers are synthetic, and the token counters grow over time. |
| a release host | `python3 -m http.server`, laid out like GitHub's `releases/latest/download/`: `install.sh` and `lss-<triple>.tar.gz` with a `.sha256` for each. The installer is pointed at it with `LSS_RELEASE_BASE`. A second tree, `bad/`, serves corrupted tarballs next to the real checksums. |
| a clean machine | **container**: `ubuntu:24.04` plus only `curl python3 expect procps ca-certificates`, run as an ordinary user. No Rust, no systemd user session, no `ss`/`netstat`/`lsof`. **local**: a new `HOME` for each scenario and a `PATH` of system directories only, so no cargo. `launchctl` is a shim (`shims/launchctl`): it runs the plist's program in the background and never touches your real login session. Check Z.1 confirms the real launchd agents are the same after the run as before it. |
| `wizard.exp` | drives the interactive wizard through a real pseudo-terminal, so `curl … \| bash` reads its answers from `/dev/tty` just as it would for a person. |

## The scenarios

Each scenario starts from an empty `HOME`, and each one uninstalls when it is done.

| id | what runs | asserts |
|---|---|---|
| **S0** | the gate tests itself | `wizard.exp` answers every question in its contract (see `selftest_wizard.sh`). Every assertion S2/S3 use **can pass with today's binaries** when the wizard's files are written by hand. If S0 fails, the run exits 2 and says nothing about the installer. |
| **S1** | `curl -fsSL $BASE/install.sh \| bash -s -- --yes` with the engine on its default port | exits 0 · `lss` and `lss-collector` are in `~/.local/bin` · the configs are written · `alert_cmd` names a program that exists · the collector is **running** (`/health`) · `lss status --json` shows `serve.up=true` and the fake model · `--uninstall --yes` is clean |
| **S2** | `… \| bash -s -- --yes --engine-url URL --zip 94103` (the CI path, no questions asked) | as S1, plus: `collector.toml` has `url = "URL"` · `rates.toml` came from the ZIP · `status.cost` uses that rate and names where it came from |
| **S3** | `curl -fsSL $BASE/install.sh \| bash`, answered through a real terminal | as S2, plus: the wizard **asked** for the engine URL (or found it and wrote it) and **asked** for a ZIP code |
| **S4** | the tarball does not match its `.sha256` | exits non-zero, installs nothing, and says "checksum" |

Details of the checks:

- **The collector is running.** Something must answer `http://127.0.0.1:8099/health` within 30 s of
  the installer exiting. A container has no service manager, so the installer has to start the
  collector some other way (for example `nohup`). When the installer does not start it, the
  harness starts it by hand so that the later checks still report something useful. It says so in
  the output, and it stops that collector before the uninstall check, so the uninstaller is never
  blamed for a process it did not start.
- **Where the rate came from (C1 and C3).** `rates.toml` is checked by its key lines only;
  comments never count. (1) `source = "…"` must name the ZIP (`--zip`, default 94103) or EIA, and
  must carry the **data month**. (2) `effective_date` is that month, written `YYYY-MM` or
  `YYYY-MM-DD`. It must be no more than 36 months old and not in the future, and it cannot be
  today's date, because that is the install date, not the date of the data. (3) `usd_per_kwh`
  must be plausible. For 94103 (San Francisco) that means a California residential average,
  0.20-0.50. Any other ZIP gets 0.05-1.00, and `E2E_RATE_MIN`/`E2E_RATE_MAX` override either
  range. C3 then requires three things of `status.cost`: `current_usd_per_kwh` equals that rate,
  `effective_date` starts with the data month, and some string field names the ZIP or EIA. A C1
  that fails leaves no rate behind, so C3 fails too instead of comparing against an earlier
  scenario's number (card #306). S0.M1-M6 feed deliberately wrong files through these checks,
  and each one must be rejected.
- **C2, live dollars per hour,** needs a GPU power reading. A machine without one reports
  `NOT-COVERED`, and the reason is printed. It never reports PASS in that case.
- **Uninstall is clean** means these things are gone: the programs, `lss-notify.sh`, the
  service files, the running collector and the `/health` listener. Configs and history stay, as
  documented. If nothing was installed, the uninstall check **fails** rather than passing on an
  empty machine.

## The wizard contract (what `wizard.exp` answers)

The driver reads the last line of output, which is the open question, and tries these in order:

| the question contains | answer |
|---|---|
| IP + look up/location, with `[y/N]` | `n`. A test never agrees to a network lookup. |
| track/electricity/cost, with `[y/N]` | `y` |
| a one-line menu such as `zip / manual / skip` | `zip`. A multi-line menu ending `Choice [zip]:` gets Enter. |
| `ZIP code`, or a question ending `ZIP:` | the ZIP |
| `API key` / `bearer token` / `access token` | empty (none) |
| engine/server/inference plus URL/address | the fake engine's URL |
| any other `[Y/n]`, `[y/N]` or `[default]` | Enter |

If a question is still unanswered after 30 s, that is a FAIL. The output shows `WIZARD-STUCK` and
the last lines the wizard printed. Every answer is logged as a `WIZARD-ANSWER` line in `S3-wizard.log`.
If you change how the wizard asks a question, keep it within this table, or change the table and
`selftest_wizard.sh` together so that S0 proves the new version.

## Options

```
--release vX.Y.Z   the tarballs to install (via `gh release download`; default: the latest release)
--dist DIR         use the lss-<triple>.tar.gz + .sha256 already in DIR
--ref REF          test install.sh as of this git ref (default: the working tree's)
--flavor F         sglang (default) | vllm | ollama | llamacpp | all
--only "S0 S3"     a subset of the scenarios
--zip ZIP          the ZIP the wizard is given (default 94103)
--out DIR          keep the logs there (per flavor: S*-install.log, S3-wizard.log, S*-status.json …)
```

Binaries: when a card changes the programs themselves (for example an `lss setup` subcommand),
the tarballs have to come from that commit and not from an older release. `scripts/e2e/dist.sh`
builds them on a Linux docker host, **macOS included**, and packs them exactly the way
`release.yml` does (`lss`, `lss-collector`, `lss-notify.sh` at the root, a `shasum -a 256` line
beside each). Nothing is compiled on the machine that runs it:

```bash
scripts/e2e/dist.sh --host gpu-box --ref <sha> --target aarch64-apple-darwin --out /tmp/dist-mac
scripts/e2e/run.sh local --ref <sha> --dist /tmp/dist-mac            # this Mac, a throwaway HOME

scripts/e2e/dist.sh --host gpu-box --ref <sha> --target x86_64-unknown-linux-musl --out /tmp/dist-linux
scripts/e2e/run.sh container --host gpu-box --ref <sha> --dist /tmp/dist-linux
```

With no `--target` it builds all four release triples. It uses cargo-zigbuild in the public
`ghcr.io/rust-cross/cargo-zigbuild` image, which bundles zig as the linker, the macOS 11.3 SDK at
`$SDKROOT`, and the Apple and musl Rust targets. The build is `--locked` and uses a `git archive`
of the ref, never a live tree. If the image's rustc is too old for the lockfile, it installs
`stable` inside the container. The remote build dir and its target volume are removed afterwards.
`LSS_ZIGBUILD_IMAGE` overrides the image.

## RED on main

This gate's output on `origin/main` at 5e4b92f (v1.1.2 binaries) was saved before any installer
work began, as `RED-on-5e4b92f.txt` next to this README in the development repository. Evidence
files like it are left out of the public copy. It fails on both machines, and for the expected reasons:

- There is no setup wizard. S3 never asks for an engine URL, and `collector.toml` stays on `auto`.
- There is no ZIP-based cost. `--engine-url`/`--zip` are unknown options, and the interactive
  cost question writes a flat `0.00` with no source.
- The `curl | bash` path has gaps. `alert_cmd` points at an `lss-notify.sh` that a piped install
  never installs. In a container nothing starts the collector. Without `ss`/`lsof`, auto-detect
  cannot find an engine on a non-default port.
