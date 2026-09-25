# RUNBOOK — what each alert means and what to do

Alerts arrive tagged `[FROM-lss-<host>] [<severity>] <message>`; a rule that clears sends
`RECOVERED: …`. Severity decides the route (see README → *How alerts route*):
`info`/`warn` = agent mail, `page`/`hardware` = agent mail + banner on the seat (the machine you sit at).
Mail the agent could not take (it was busy) waits in the spool and arrives later, marked
`(queued HH:MM:SS, delivered late)` or folded into one `[digest]` message.
Every alert is also a row in `lss alerts`, delivered or not.

## What each alert means / what to do

| rule (severity) | fires when | it means | first thing to do |
|---|---|---|---|
| `serve_down` (warn) | `/v1/models` silent for 120 s | the model server is not answering: loading, crashed, or swapped | `docker ps -a \| head` on the GPU box; a planned load recovers by itself in ~5 min |
| `serve_down_page` (page) | still down after 900 s | it did not come back | `docker logs --tail 80 <serve container>`; ask the other sessions before bringing anything up |
| `gate_down` (warn) | `/gate/health` silent for 120 s | every lane through your gateway is affected | check your gateway however you run it; this repository ships no gateway, so there is no in-tree deploy recipe to point at |
| `container_restart:serve\|gate` (warn) | RestartCount/StartedAt changed, or another container took :8090 | a deliberate restart, or a crash docker recovered | wait for `RECOVERED` (120 s stable); none in ~10 min = treat as `serve_down` |
| `xid:gpuN` (hardware) | kernel logged `NVRM: Xid` | GPU fault; 8/43/45 usually kill the engine, 79/119/120/154 need a reset | `lss incidents`, `journalctl -k \| grep -i nvrm \| tail -20`; reset/reboot is a human call |
| `gpu_missing` (hardware) | nvidia-smi failed or fewer GPUs for 60 s | a GPU fell off the bus or the driver is wedged | `nvidia-smi` by hand (a hang is the answer), `dmesg \| tail` |
| `disk_full` (warn) / `disk_full_page` (page) | the collector's own `/` (`disk_path`) ≥ 90 % / ≥ 97 % used for 60 s; recovers 2 points below its line | the serve box is running out of disk - a full `/` stops docker, logs and the engine's own writes (on 2026-09-23, 125 leaked build volumes filled it to 100 % with nothing alerting) | `df -h /`, then `docker system df`; build leftovers first (see "The build host" below), never a volume a running container uses |
| `queue_pressure` (warn) | queue ≥ 24 for 120 s | demand exceeds the box; 429s are next | LANES → top keys: who is it; fix on the demand side |
| `gate_waiters` (warn) | trusted waiters > 0 for 300 s | big prefills are waiting for token budget | same; or a budget change in the gate |
| `charge_errors` (warn) | the gateway's charging path raised for ≥ 60 s (card #68's silent-failure gauge) | every trusted request is being charged GROSS tokens - the discount is off | `docker logs <your gateway container> \| grep shadow`; `ENGINE_METRICS_URL` reachable? (`curl <engine>/metrics` from the gate box) |
| `discount_inert` (warn) | warm cache + traffic + ZERO discounts for ≥ 600 s (≥ 50 admissions first) | the effective-token discount is inert - the budget is being charged in the wrong unit again | the shadow log: `charged_tokens == est_tokens` everywhere? check `/gate/health`'s `shadow` block (`scrapes_ok > 0`) |
| `reject-hang-interlock` (event) | the gate 503'd new trusted requests: engine reports running but advanced NEITHER decode NOR prefill for > 60 s (a long prefill that still consumes prefill_compute is NOT refused) | the Xid-8 signature - treat like `xid:gpuN`, the engine may be wedged | `lss status` (is anything running?), `docker logs --tail 50 glm-…`; if real, the gate self-recovers when tokens flow again |
| `thermal_temp:gpuN` / `thermal_throttle:gpuN` (warn) | ≥ 90 °C, or a thermal slowdown bit, for 600 s | cooling problem (GPU0 is excluded: digest only) | fans/airflow/ambient; do NOT raise the power cap |
| `thermal_digest` (info) | once per 24 h, only if an excluded GPU was hot/throttled | how long GPU0 spent hot | nothing; it is a record |
| `c1_decode` (warn) | VALID idle probe < 0.8 × baseline, 3 in a row (probes that met real traffic are ignored) | single-stream decode got slower | GPUS panel (clocks, throttle), restarts today, `spec accept`; `lss probe` |
| `rejects_413` / `rejects_429` (info) | counter grew by > 20 in 600 s | a client is over the limits or retrying hard | LANES → top keys tells you who |
| `test_alert` (info) | someone ran `lss-collector --test-alert` | nothing: it is the pipeline test, message starts `[TEST]` | nothing |
| banner "… NOT ANSWERING from <seat>" | the seat's up-check missed twice | the GPU box, the network, the serve, the gateway, or the collector is down | `ping <gpu-box>`, then the matching section below |

Thresholds above are the defaults; the ones in force are in `lss status --json` → `thresholds`
and in `lss-collector --check-config` on the GPU box.

First moves for anything: `lss status` (from any box), then on the GPU box
`journalctl --user -u lss-collector -n 50` and `docker ps`. This runbook assumes the systemd
`--user` unit. If the installer said `no systemd --user session`, the collector runs under `nohup`
instead: its log is `~/.local/state/lss/lss-collector.log`, and stopping and starting it is in the
README's Troubleshooting.

**The collector never restarts, reloads or reconfigures anything** - a person does. If the GPU
box is shared, check who else is driving it (`docker ps`) and say so before you restart the
serve; if outside users depend on the endpoint, tell them first.

## Pages — where to look, and which dashboard panel each one replaces

`lss` opens on the OVERVIEW. Arrows move the focus, `enter`/`tab` opens the focused box full
screen, `1`–`5` jump straight to a page, `r` cycles the range (15m → 1h → 6h → 24h → 7d),
`↑↓`/PgUp/PgDn scroll, `esc` comes back. The same data without a terminal:
`lss latency|load|gpus|gateway [--range 6h] [--json]` and `lss rules [--json]`.

| page | look here when | it replaces |
|---|---|---|
| `1` LATENCY (from SERVE) | "it feels slow": TTFT / e2e / inter-token / queue-time p50·p90·p99 over time, and WHERE the requests fell (bucket bars + heat strip). A TTFT that climbs while ITL stays flat = prefill pressure (big prompts, full queue); ITL climbing = decode is slow (clocks, throttle, too many running) | Grafana *SGLang*: E2E Request Latency, Time To First Token, the two latency heatmaps |
| `2` LOAD | `queue_pressure`, `c1_decode`, "who is using it": tok/s, running and queued per lane, KV and cache hit, spec accept, requests/min, the C1 probe history (`x` = a probe that met real traffic and was not used) | Grafana *SGLang*: Running Requests, Generation Throughput, Cache Hit Rate, Queued Requests |
| `3` GPUS (from GPUS) | `thermal_*`, `xid:*`, `gpu_missing`, `c1_decode`: per-GPU temperature, power vs cap, SM clock vs max, utilisation, memory; the THROTTLE timeline (`p` power cap is normal under the 300 W limit, `T`/`H` are not, `X` = Xid); HEALTH: PCIe link now vs max, ECC, remapped rows, retired pages. A link narrower than its max, any uncorrected ECC, a pending/failed row remap are RED | NVIDIA *DCGM* dashboard: GPU Temperature, Power Usage, SM Clocks, GPU Utilization, Framebuffer Mem Used |
| `7` GATEWAY (from LANES) | `rejects_413/429`, `gate_waiters`, 5xx: per-lane and per-key requests and status codes (413 = prompt too big, 429 = budget/queue, 499 = client gave up, 503 = upstream down), estimated prompt tokens of the rejected requests, in-flight tokens and waiters over time, top client addresses (last octet masked) | the gate's audit log (there is no Grafana panel for it) |
| `8` ALERTS and `9` INCIDENTS (from INCIDENTS / ALERTS) | "is anything about to fire": every rule with `ok` / `pending` / `FIRING`, its condition, what it sees now, when it last fired, cooldown left; alert history with delivery status and the mail-spool depth; incidents with durations; uptime % 24 h / 7 d | Prometheus *Alerts* page (inactive / pending / firing) + Alertmanager |

`pending` means the condition is true but the hold time (or the rule's 30 min cooldown) is not
over yet: it is the early warning. Not available, and why: DCGM's tensor-core / SM-active
utilisation needs DCGM profiling counters, which `nvidia-smi` does not expose; the
memory-controller utilisation stands in. Memory temperature is `N/A` on these boards.

## serve_down  (warn after 120 s, `serve_down_page` = page after 900 s)
SGLang's `/v1/models` has not answered for that long. A model load takes ~5 min, so a
planned model load produces a warn and a recovery; a page means it did not come back.
1. `docker ps -a | head` — is the serve container `running`, `restarting`, gone?
2. `docker logs --tail 80 <serve container>` — a traceback / CUDA error / OOM at load?
3. `lss incidents` — an `xid` just before the outage means the GPU faulted: see *xid*.
4. Container in a restart loop: read the log before touching it; roll back to the last
   loadout that worked (`lss loadouts` lists them). Container gone and nobody announced a swap:
   ask before bringing anything up.

## gate_down  (warn after 120 s)
your gateway's health endpoint (whatever `gate_url` in the collector config points at) has gone
silent: every lane that goes through it is affected even if the engine itself is fine. Check your
gateway however you run it - its own logs, `docker ps` / `docker logs <your gateway container>`
if it is containerized, its process supervisor otherwise - and restart or redeploy it by whatever
means you deployed it. This repository does not ship a gateway, so there is no in-tree deploy
recipe to point at; if `gate_url` is empty, this alert never fires.

## container_restart:serve / container_restart:gate  (warn — `info`/"planned" inside a
## maintenance window, see below)
docker `RestartCount` changed, `StartedAt` changed, or (serve) the container on :8090 was
replaced by another. Expected after a deliberate swap/restart — otherwise it crashed and
docker brought it back. `RECOVERED` follows once the container has been up and answering
for 120 s; no `RECOVERED` within ~10 min of a serve restart = it is not loading: treat as
*serve_down*. Repeats inside the 30 min cooldown are not re-alerted; count them with
`lss incidents` and the header's `restarts today`. Run `lss maintenance start "reason"` BEFORE
a deliberate restart (a model swap, a gate upgrade) so this shows `info`/"planned - …" instead
of a `warn` — a real, unrelated problem during the window still alerts normally, only the
restart itself is relabelled.

## xid:gpuN  (hardware)
The kernel logged `NVRM: Xid` for that GPU (mapped from the PCI address). One alert per
GPU per cooldown; every line is still an incident. History on this box: Xid 8 on GPU1
(09-17) and GPU3 (09-18) — a hang, which took the serve down — and Xid 119/154 on GPU1
(09-13, GSP timeout, "GPU Reset Required").
1. `lss incidents` for the number; `journalctl -k | grep -i nvrm | tail -20` for context.
2. Is the serve still up (`lss status`)? Xid 8/43/45 usually kills the engine process.
3. 79 (fell off the bus), 119/120 (GSP), 154 (recovery action): `nvidia-smi` may hang or
   show fewer GPUs — expect `gpu_missing` too. These need a GPU reset or a reboot; that is
   a human decision on a shared box.
4. 48/63/64/94/95 are ECC: note the GPU, check `nvidia-smi -q -d ECC`, plan an RMA if it repeats.
Same GPU twice in a week is a hardware conversation, not a software one.

## gpu_missing  (hardware, after 60 s)
nvidia-smi failed/timed out, or reports fewer GPUs than it used to. Usually follows an Xid.
`nvidia-smi` by hand (it may hang — that is the answer), `dmesg | tail`. Thermal rules hold
their state while there is no reading; they do not "recover" on a failed poll.

## queue_pressure (warn) / gate_waiters (warn)
`queue_pressure`: `num_queue_reqs >= 24` for 2 min (the engine caps at 32 queued, then 429s).
`gate_waiters`: trusted requests have been waiting at the gate for prompt-token budget for
5 min. Both mean demand exceeds the box, mostly big agent prefills.
`lss status` → LANES: who is it (top keys), is `inflight` near the budget, are 429s growing?
Nothing is broken; the fix is on the demand side (fewer parallel agents, smaller contexts)
or a budget change in the gate.

## thermal_temp:gpuN / thermal_throttle:gpuN  (warn)
`>= 90 °C` for 10 min, or a thermal slowdown bit (`sw_thermal` 0x20 / `hw_thermal` 0x40)
for 10 min. `sw_power_cap` alone is normal under load and never alerts.
Check fans and airflow (`lss status` shows fan %), ambient, and whether one card is much
hotter than its neighbours. Do not raise the power cap — that was measured and made
decode worse.
**GPU0 is in `thermal_exclude`** (the owner handles it): it never alerts. If it spent time
hot or throttled, one `thermal_digest` info line per 24 h says how long; a quiet day says
nothing.

## c1_decode  (warn)
The idle single-stream probe (128 tokens through the gate, only when running = queue = 0)
measured decode below 0.8 × baseline (`c1_ratio`) three times in a row (`c1_consecutive`).
Baseline = `c1_baseline_tok_s` or the median of the first 12 probes. `lss probe` shows the
baseline, its source and the floor in force; the screen's yellow C1 value uses the same floor
(`/status` → `thresholds`), not a number of its own.
Real causes seen on this class of box: a GPU stuck at low clocks or thermally throttled
(GPUS panel), a changed serve config/arm (restarts today?), spec-decode acceptance
collapsing (`spec accept` in SERVE). Probe-to-probe noise here is about ±10 %, so one low
probe is nothing; three is a pattern — that noise is why the ratio is 0.8 and not 0.9.

**Only VALID probes count.** A request that arrives right after the idle check shares the
engine with the probe and the number is no longer an idle number (2026-09-19: TTFT 57.8 s and
26.5 s, 141–154 tok/s while the outside user was active — a false alarm in the making). A
probe is valid only if the engine was idle before it, its TTFT was ≤ 3 s (`c1_max_ttft_s`),
and right after it the engine ran nothing else and the gate had admitted nobody else. The
rest are stored as `INVALID slow_ttft | contended | busy_before` (`lss probe` lists them with
the reason) and are ignored by the baseline, by this rule (they neither count nor reset the
streak) and by `/metrics`. `lss status` shows the last valid reading and `(n invalid
skipped)` since it; on a box that is busy for hours the reading just gets older
(`llm_serve_c1_probe_age_seconds`) — that is "no data", not "slow". So when this alert does
fire, three probes that ran ALONE were slow: take it seriously.

After a deliberate arm change, reset the
baseline: set `c1_baseline_tok_s` in `~/.config/lss/collector.toml` and restart, or relearn
(the collector rewrites its state while it runs, so stop it first):
```
systemctl --user stop lss-collector
sqlite3 ~/.local/state/lss/lss.db "DELETE FROM kv WHERE k='engine'"   # also resets cooldowns
systemctl --user start lss-collector
```

## rejects_413 / rejects_429  (info)
The gate's rejection counters grew by more than 20 in 10 min. 413 = prompts over the
per-request token limit; 429 = concurrency / budget / queue-full. Informational: someone's
client is misconfigured or retrying hard. LANES → top keys tells you who.

## omp_default_mismatch  (warn; card #36, 2026-09-21)
`~/.omp/agent/config.yml`'s `modelRoles.default` on THIS box names a model this provider is not
currently serving. The 2026-09-20 incident this exists to catch: omp could not resolve the
default and silently fell back to an OUTSIDE provider - agent work left this hardware without a
word, found by hand, not by any alert. Held `omp_mismatch_hold_secs` (default 10 min) before
firing so the few seconds a swap takes to land does not itself alert.
1. `lss rules` (or the ALERTS page) shows `configured != served` in the value.
2. `curl -s http://<your-server>/v1/models` for the id this server actually serves now, and set
   `modelRoles.default` to it. Edit it the way you normally edit that file - omp reads it live,
   so write it atomically (write a temp file beside it and rename) rather than in place, and keep
   a `.bak` copy.
3. This box only: this alert only ever sees the config on the box the collector itself runs on.
   Any other box with its own `~/.omp/agent/config.yml` needs the same edit; nothing here can see
   them.
4. If this can happen at all, the config still lets an unresolvable default fall back to an
   outside provider. Remove that provider from `enabledModels` so it errors instead - see
   `docs/MODEL-SWAP.md` step 3, which is where this belongs on every swap, not just when the
   alert fires.
Recovers on its own once the config is back in sync (`RECOVERED: omp's default model matches
what is served again`) - no action needed once step 2 is done.

## Banner from the seat: "LLM serve NOT ANSWERING" / "lss-collector NOT ANSWERING"
The seat's up-check (`ai.lss.upcheck`, every 60 s) missed twice. This is the only alarm that
still works when the GPU box itself is dead or off the network.
* both targets missing → the box or the network: `ping <gpu-box>`.
* only the serve → the serve that `LSS_UPCHECK_SERVE_URL` points at is down - the engine on its own
  port (`:8090` SGLang, `:8000` vLLM, `:11434` Ollama, `:8080` llama.cpp, `:1234` LM Studio), or your
  gateway if you run one. Then see *serve_down* / *gate_down*.
* only the collector → `ssh <gpu-box> systemctl --user status lss-collector`; its `/health`
  also turns 503 when the poll loop is stuck.

**No target is ever guessed (card #82).** The serve leg has NO default: if
`LSS_UPCHECK_SERVE_URL` is unset the up-check never probes anything for it and never banners -
`upcheck.last` reads `serve=unset` so "this leg is not being watched" is visible instead of a
permanent false "serve down" (that is what an old default pointed at a gateway's trusted port -
which a stranger does not run - produced for everyone else). `scripts/install-upcheck.sh` refuses
to install with no serve target and prints the exact URLs it installed. The collector leg keeps
its `http://127.0.0.1:8099/health` default.
State: `~/.local/state/lss/` — `ALERT` exists while something is down, `upcheck.log` has the
history, `upcheck.last` is the heartbeat of the latest run (`serve=`/`collector=` each `ok`,
`MISS`, or `unset`).

## `bench` in INCIDENTS
Not a fault: the window during which `lss bench` loaded the server on purpose. While it is open
the queue, gate-waiter and C1 rules stand down and the C1 probe pauses; outages, restarts, Xids
and heat still alert. A run that ends `aborted: real traffic …` stopped because someone used the
server: run it again when it is quiet. `aborted: the collector restarted …` = the collector was
restarted mid-run (the harness dies with it). `lss bench status` has the last runs;
`docs/MODEL-SWAP.md` has the routine.

## `maintenance` in INCIDENTS (card #22, 2026-09-21)
Not a fault: a deliberately announced window, `lss maintenance start "reason"` /
`lss maintenance stop`. Unlike `bench`, nothing STANDS DOWN — a real, unrelated problem during
the window (an Xid, a thermal excursion, an unplanned outage) still alerts exactly as it always
does. The only thing that changes is the label and severity of the restart the window was opened
for: `container_restart:serve`/`:gate` reads `info`/"planned - …" instead of `warn` while the
window is open, and the eventual `RECOVERED` message keeps that same label even if the window
has already been closed by the time the container finishes settling (`lss maintenance stop`
right after confirming it is healthy is normal and expected). The window itself is on the
timeline as its own `maintenance` incident and as an `M` marker on the LOAD page's charts, in a
calm colour, never red — it explains a restart's dip, it is not one.
1. `lss maintenance start "gate v5.2 swap"` before touching anything that restarts a container.
2. Do the work. A genuine unrelated alert during this time is still real — do not dismiss it as
   "probably the maintenance."
3. `lss maintenance stop` the moment the restart is confirmed healthy — do not leave it open
   "just in case," since every restart until then reads as planned.
4. Forgot to stop it? It auto-expires 60 minutes after `start` (`--minutes N` to change that, up
   to a 240-minute ceiling) — a forgotten window can never mute real restart alerts forever.
`lss maintenance status` shows the open window's reason and how long until it auto-expires;
`lss incidents` / the INCIDENTS panel show the closed ones too.

## The monitor itself
* `lss` says COLLECTOR UNREACHABLE → the serve may be fine. `systemctl --user status
  lss-collector` on the GPU box; it is `Restart=always`.
* **Is the alert path alive?** On the GPU box:
  ```
  ~/.local/bin/lss-collector --test-alert "runbook check"   # rule engine -> alerts row -> alert_cmd, on the LIVE collector
  tail -5 ~/.local/state/lss/alert.log                      # status=OK (delivered) or status=SPOOLED (agent busy)
  ```
  and `lss alerts` from any box shows the row (`test_alert`, `[TEST] …`). The endpoint behind it
  (`POST /test-alert`) only answers on `127.0.0.1`; from any other address it is a 403.
  `LSS_ALERT_TEST=1 ~/bin/lss-alert.sh page "test"` tests the script alone, banner included;
  `LSS_UPCHECK_TEST=1 ~/.local/bin/lss-upcheck.sh` on the seat tests the up-check (it starts its own
  throwaway local server, so it proves the whole cycle without depending on the GPU box).
* `[not delivered]` / `[undelivered]` next to an alert → no leg has got through YET. `tail
  ~/.local/state/lss/alert.log` on the GPU box says why: `reason=not_configured` (no
  `~/.config/lss/alert.env`: nobody to deliver to),
  `reason=agent_busy` (the agent named in `LSS_ALERT_AGENT` was working for the whole 45 s
  wait), `agent_not_found` (no agent of that name: `LSS_ALERT_AGENT` overrides),
  `agent_blocked` (it is sitting on a permission dialog), `ssh_or_agentmail_failed`.
  In every case the mail is in `~/.local/state/lss/mail-spool/` (one `.msg` file each) and is
  retried by every later alert and by the collector's `lss-alert.sh --flush` every 5 min; the
  row flips to delivered when it goes out. A `warn` also falls back to a banner at once;
  `page`/`hardware` always banner. Force a retry: `~/bin/lss-alert.sh --flush`.
  `status=DROP … detail=spool_full` = more than 200 were waiting and the oldest was dropped
  (its text is in that log line). To discard the spool: `rm ~/.local/state/lss/mail-spool/*.msg`.
* **Memory.** The unit runs with `MemoryHigh=256M`, `MemoryMax=512M`, `MemorySwapMax=0`
  (`systemctl --user show lss-collector -p MemoryCurrent -p MemoryPeak`). Normal is ~15 MB.
  The only heavy read is the once-only 7-day Xid back-fill, streamed through
  `journalctl | grep` (72 MB peak for the cgroup, measured). `xid backfill: … timed out` in
  the journal = it did not finish and is redone on the next start.
* LANES `probes: N` = the collector's own C1 probes, already taken out of the trusted lane.
  In the gate's access log they are the `lss-probe/<version>` user agent.
* **History / pages empty?** A page that says `the collector has no /series` is talking to a
  collector older than this `lss`: `scripts/install-collector.sh`. `curl -s
  'http://127.0.0.1:8099/series'` on the GPU box lists every recorded metric; `…/series?metrics=queue&range=6h`
  is what the screen asks for. Latency percentiles only exist for minutes that had requests, and
  only from the moment the rollup-aware collector was first started (2026-09-19 14:47).
* **Database.** `~/.local/state/lss/lss.db`: raw samples 24 h, 1-minute tier 14 d, 10-minute
  tier 90 d (`raw_hours`, `retention_days`, `rollup_10m_days`); ~4 MB/day, levels off near
  130 MB. The hourly prune logs `retention: pruned N rows; database X MB`.

## The build host: `remote-build.sh` cleans up after itself (for builders and verifiers)

A cargo target volume is GBs; on 2026-09-23 125 leaked `lss-target-*` volumes filled the build
host's `/` to 100 % (card #264). Since then:

- **A pinned run (`--ref <sha>`) removes what it made when it ends** - pass, fail or Ctrl-C: its
  own container, its target volume, and its remote dir, and prints exactly what it removed (and
  `could not remove …` for anything that would not go). Without `LSS_BUILD_DIR`, every pinned run
  gets its own dir (`lss-build-<sha12>-<cmd>-<pid>`), so a `test` and a `clippy` of one sha side by
  side never share one. **With an explicit `LSS_BUILD_DIR`, runs CAN share a dir** - so nothing is
  removed while ANY running container still mounts the dir or its volume: the run says `kept … -
  still in use by running container(s): <names>` and prints the command to remove them later. It compiles from a cold target (the download cache
  `cargo-registry` is shared and kept) - expect a few minutes of build before the tests.
- **`--keep` (or `LSS_BUILD_KEEP=1`) keeps the dir and the volume** (the run's own container is still
  stopped - keep means the files, not a suite left running) and prints the one command that
  removes them later. With a fixed `LSS_BUILD_DIR` that is also how to reuse a warm cache across
  runs. Remove them yourself when done.
- **`build --ref` keeps its dir** (its `dist/` holds the binaries you asked for) and removes only
  the volume; it prints where `dist/` is. Delete the dir when you have copied what you need.
- **Never removed automatically:** a live (non `--ref`) run's dir, the shared `lss-build` /
  `lss-target`, a volume you named with `LSS_BUILD_TARGET_VOL`, and the shared `cargo-registry`.

**Pruning leftovers safely** (older runs, `--keep`, killed shells). The build host is shared: only
remove what no live run uses, and say so to the other sessions first.

```
docker ps --format '{{.Names}}' | grep -E '^lss-(run|build)-'     # live runs: leave their dirs and volumes alone
docker volume ls -q --filter dangling=true | grep '^lss-target-'  # volumes NO container uses (candidates)
docker ps -a --filter volume=<name> -q                           # empty = nothing holds <name>
docker volume rm <name>                                           # refuses a volume still in use
ls -dt ~/lss-build-*/                                             # dirs, newest first; a dir of a live run is in use
docker run --rm -v "$HOME/<dir>:/x" lss-build:latest sh -c 'rm -rf /x/* /x/.[!.]*'; rmdir "$HOME/<dir>"   # root-owned files inside
```

Never `docker volume prune` or `docker system prune` on the build host: it also serves the model and
other sessions' containers, and a prune does not ask which volumes are yours.

