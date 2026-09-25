# The collector's HTTP API — schema v1

| endpoint | what | cost to the collector |
|---|---|---|
| [`GET /status`](#get-status) | the live document the overview draws | a string copy (pre-rendered every poll) |
| [`GET /rules`](#get-rules) | every alert rule with its state, alert history, incidents, uptime | a string copy |
| [`GET /series`](#get-series) | time series, aligned, at most 600 points each | one indexed read on a read-only connection |
| [`GET /hist`](#get-hist) | latency histogram buckets over a range | same |
| [`GET /gateway`](#get-gateway) | per-lane / per-key / per-address request tables | same |
| [`GET /tokens`](#get-tokens) | tokens served: windows, all time, peaks, lengths, per user | a string copy (rebuilt every minute) |
| [`GET /loadouts`](#get-loadouts) | every loadout: passive scorecard + newest bench scorecards | a string copy |
| [`GET /bench`](#get-bench-post-bench) | is a benchmark running, and the last 30 runs | a string copy |
| [`GET /advice`](#get-advice) | evidence for settings and upgrade decisions | a string copy (rebuilt every 5 minutes) |
| [`GET /metrics`](#get-metrics) | Prometheus text | a string copy |
| [`GET /health`](#get-health), [`POST /test-alert`](#post-test-alert), [`POST /bench`](#get-bench-post-bench) | liveness; pipeline test; start a benchmark (127.0.0.1 only) | |
| [`POST /maintenance/start`, `POST /maintenance/stop`](#post-maintenancestart-post-maintenancestop) | open/close a planned-maintenance window (127.0.0.1 only) | |

Every JSON document carries `"v": 1`. The history endpoints (`/series`, `/hist`, `/gateway`)
answer identical requests from a 2-second cache, read through their own read-only SQLite
connection (WAL), and therefore never block the poll loop. A bad request is `400`
`{"v":1,"error":"…"}`.

# `GET /status`

Served by `lss-collector` on `:8099`. One JSON document, rebuilt on every poll (5 s), so a
request costs the collector a string copy. `lss status --json` prints it verbatim;
`lss incidents|alerts|probe --json` print the matching slice plus `"v"`.

**Stability contract.** `"v": 1` stays as long as changes are *additive* (new keys, new
incident kinds, new rule names). Renaming or removing a key, or changing a unit, bumps `v`.
Clients must ignore keys they do not know and tolerate missing ones — `lss` does both, and
refuses a document whose `v` it does not understand. The shape is pinned by the golden
test (`fixtures/status_golden.json`, built from real fixtures): any schema change shows up
as a diff there and must be reflected in this file. `/series`, `/rules` and `/gateway` are pinned
the same way (`fixtures/series_golden.json`, `rules_golden.json`, `gateway_golden.json`;
`scripts/remote-build.sh bless` regenerates all four).

All timestamps are unix seconds (UTC). `null` means "not known", never zero.

## Top level

| key | type | meaning |
|---|---|---|
| `v` | int | schema version, `1` |
| `host` | string | collector's `host` config, else the machine's host name (`gpu-box`) |
| `generated_at` | int | time of the poll that produced this document. `now - generated_at > 30` = the poll loop is stuck (`/health` turns 503 at the same point) |
| `collector` | object | `version`, `started_at`, `poll_secs`, `last_sample_ts` |
| `serve` | object | the model server, below |
| `gate` | object | the gateway, below |
| `gpus` | array | one object per GPU, below |
| `lanes` | object | `public` and `trusted`, below |
| `series` | object | last 60 min for sparklines, below |
| `incidents` | array | last 7 days plus anything still open, newest first |
| `alerts` | array | last 20, newest first |
| `firing` | string[] | rule keys alerting right now (sent and not yet recovered) |
| `probe` | object | C1 idle decode probe, below |
| `thresholds` | object | the alert thresholds in force, below (added 2026-09-19; absent from older collectors) |
| `users` | object | who is on the server, below (added 2026-09-20; `available: false` with a gateway older than the gateway v5.2) |
| `advice_top` | array | the two most pressing findings of `GET /advice` (added 2026-09-20; empty until the first is worked out) |
| `bench` | object | `state` (`idle` / `running`), `profile`, `started_at`, `step`, `step_index`, `steps`, `configured`, and `last` = the newest finished run `{run_id, profile, ended_at, status, aborted, model, headline, load, failed_checks}` (added 2026-09-20): `aborted` is null unless the run was aborted (then why); `failed_checks` (card #338, 2026-09-24) = the checks that run FAILED, in words (`json output`, `needle at 50%`), `[]` when all passed or none ran - `status` stays `ok` for a run that completed, so read the two together: `ok` with a non-empty `failed_checks` is what `lss` and the UI's BENCH box print as `ok, but 1 check failed: json output` (the CLI scorecard's `OK, BUT 1 CHECK FAILED: …`, card #334); a collector from before this omits the key; `load` is `quiet` / `loaded` / `unknown` - the condition the headline numbers were measured under, never to be read as quiet when unknown; `headline` = `{c1_tok_s, ttft_c1_ms, max_total_tok_s, max_total_at, prefill_8k_tok_s, accuracy, accuracy_dataset}` - one user alone (tok/s and time to first token), the best total tok/s and the concurrency it was reached at, prefill speed at an 8k prompt, and the accuracy score with the dataset it was scored on; each null when that step did not run (fields documented 2026-09-23, card #235 - they were shipped but invisible to the doc check while the golden left `last` null) |
| `loadout` | object\|null | what is being served, as a loadout: `id`, `model`, `image_tag`, `first_seen`, `runs`, `flags` (added 2026-09-20; `flags` added 2026-09-21 for #51 - the safe launch flags string, `crate::loadout::safe_flags`: `tp 4 · ctx 1048576 · quant fp4 · slots 8 · spec NEXTN …`) |

## `serve`

| key | type | meaning |
|---|---|---|
| `up` | bool | SGLang `/v1/models` answered with a model on the last poll |
| `model` | string\|null | served model id (last one seen, kept while down) |
| `container`, `container_status` | string\|null | docker name and state (`running`, `restarting`, `exited` …) |
| `started_at`, `uptime_s` | int\|null | container StartedAt; `uptime_s` is null while the serve is down |
| `restart_count` | int | docker `RestartCount` |
| `restarts_today` | int | serve `container_restart` incidents since local midnight on the GPU box |
| `down_since` | int\|null | start of the open `serve_down` incident |
| `seconds_to_drain_charged` | number\|null | added 2026-09-23 (#279): the gate's own estimate of how long its in-flight CHARGED tokens take to drain at the engine's recent prefill rate, straight from `/gate/health` `shadow.seconds_to_drain_charged` (published since v5.10, card #154). `null` = no gateway, a gateway older than v5.10, or no prefill rate to divide by - never 0, which would read as "drains instantly" |
| `running`, `queue` | number | `sglang:num_running_reqs` / `num_queue_reqs`, engine total (`priority=""`, `tp_rank="0"`) |
| `slots` | int | max concurrent requests (`max_running_requests`) |
| `decode_tok_s` | number | `sglang:gen_throughput` |
| `decode_tok_s_from_probe` | bool | `true` = `decode_tok_s` is the monitor's own C1 probe, not a figure the engine published (this engine publishes no speed). Say so wherever you show the number rather than passing it off as the engine's |
| `kv_usage` | number 0..1 | `token_usage` (falls back to `kv_used_tokens / max_total_num_tokens`) |
| `kv_used_tokens`, `kv_max_tokens` | number | |
| `ttft_avg_ms_10m`, `itl_avg_ms_10m`, `queue_time_avg_ms_10m` | number\|null | histogram `_sum/_count` deltas over the last 10 min; null = no requests in the window. Includes the collector's own probe |
| `spec_accept_length`, `spec_accept_rate`, `cache_hit_rate` | number | engine gauges |
| `prompt_tokens_total`, `generation_tokens_total`, `requests_total` | number | engine counters (reset when the engine restarts) |
| `latency` | object | added 2026-09-19. `window_s` (600) and, per histogram that saw requests in the window, `ttft`, `e2e`, `itl`, `queue_time` = `{p50_ms, p90_ms, p99_ms, avg_ms, count}`. Percentiles come from histogram **bucket deltas** (see [Percentiles](#percentiles)), so they describe the last 10 minutes, not the life of the engine. A key is absent when its histogram saw nothing; the whole object is absent from older collectors |
| `evicted_tok_per_hour_10m` | number\|null | added 2026-09-21 (#51): KV tokens thrown out of the prefix cache, as a tokens/hour rate over the last 10 min (`evicted_tokens_total` delta, scaled). `null` = fewer than two comparable samples in the window yet - not zero; a real zero means nothing was evicted |
| `preempted_per_hour_10m` | number\|null | added 2026-09-23 (#279): requests the engine PUSHED OUT of the running batch to make room, as a per-hour rate over the same 10-min window as `evicted_tok_per_hour_10m` (`num_retracted_reqs` delta, scaled). Eviction throws away cached prefix; preemption throws away work in flight. `null` = fewer than two comparable samples yet, OR an engine that publishes no preemption counter (then `not_reported` carries `preemptions`) - never zero for either; a zero here is a MEASURED zero |
| `secs_since_last_token` | int\|null | added 2026-09-21 (#51): seconds since `generation_tokens_total` last grew, anywhere in the retained history (up to 1 h). `null` = no growth seen in that whole hour (what an Xid-class hang looks like from outside), not "just started" |
| `work_1h` | object\|null | added 2026-09-21 (#51): `{prompt, cached, generated, requests}` - the same four counters as `prompt_tokens_total` / `cached_tokens_total` / `generation_tokens_total` / `requests_total`, but their growth over the last hour only, not since the engine started. `null` = fewer than two comparable samples in the last hour |
| `public_priority`, `trusted_priority` | string | the SGLang priority label the gate stamps per lane, echoed from the collector's config (`public_priority` / `trusted_priority`; the shipped example is `"10"` and `"0"`). `""` means an older collector that has no key for it - UNKNOWN, never "no priority" |

## `gate`

`up` (bool: `/gate/health` answered), `version`, `upstream_ok`, `container_status`,
`started_at`, `restart_count`, `down_since`, `seconds_to_drain_charged`.
`absent` (bool, added 2026-09-20): NO gateway is configured at all (`gate_url` unset in the
collector's config) - not the same as the gateway being DOWN. `absent: true` comes with
`up: true` by design, so a missing-but-optional component never reads as an outage; `lss`
shows LANES / USERS / GATEWAY as "no gateway" from it.

## `gpus[]`

`index`, `temp_c`, `power_w`, `power_limit_w`, `clock_mhz` (SM clock), `util_pct`,
`mem_used_mib`, `mem_total_mib`, `fan_pct` — numbers or null (`[N/A]`);
`throttle_mask` (int, `clocks_throttle_reasons.active`);
`throttle` (string[], decoded: `hw_thermal` 0x40, `sw_thermal` 0x20, `hw_slowdown` 0x8,
`hw_power_brake` 0x80, `sw_power_cap` 0x4);
`thermal_excluded` (bool: in `thermal_exclude`, digest only).
An empty array means nvidia-smi failed on the last poll.

Added 2026-09-19 (absent from older collectors; every field `null` when the board says `[N/A]`):
`mem_util_pct` (`utilization.memory`), `mem_temp_c` (`temperature.memory`) on the 5 s cadence, and
`health`, read once a minute: `clock_max_mhz`, `pstate`, `pcie_gen` / `pcie_gen_max`,
`pcie_width` / `pcie_width_max`, `ecc_corrected` / `ecc_uncorrected` (volatile totals),
`remap_correctable` / `remap_uncorrectable` (numbers), `remap_pending` / `remap_failure` (bool),
`retired_sbe` / `retired_dbe` (numbers), `retired_pending` (bool), `power_max_limit_w`,
`power_enforced_w`, `ts` (when it was read). If the driver refuses the two extra 5 s fields the
collector falls back to the base query for good: a driver change can never look like a missing GPU.

## `lanes.public` / `lanes.trusted`

| key | source |
|---|---|
| `running`, `queued` | SGLang gauges for the lane's priority label (public `"10"`, trusted `"0"`) |
| `inflight_tokens`, `waiters`, `admitted`, `rejected_413`, `rejected_429`, `client_closed`, `upstream_down`, `max_duration` | `/gate/health` admission counters (reset when the gate restarts) |
| `requests_10m`, `codes_10m` = `{"2xx","4xx","5xx"}` | gate audit log, POST requests, last 10 min |
| `top_keys_60m` = `[{key,count}]` (max 5) | gate audit log, last 60 min. Key NAMES only; `(no key)` = trusted traffic |
| `probes` = `{admitted, requests_10m, requests_60m}` | the collector's OWN C1 probe requests, which it has already **subtracted** from `admitted` (since the gate started), from `requests_10m`/`codes_10m`, and from the `(no key)` row of `top_keys_60m`. Add them back to get the gate's raw numbers. Always zero for `public`: the probe uses the trusted port |

The probe is recognisable on the wire by `X-LSS-Probe: 1`, `"user": "lss-probe"` and
`User-Agent: lss-probe/<version>`; the gate is unchanged and counts it like any trusted
request, so the subtraction happens here, from the collector's own record of what it sent
(`probe.history[].http_status`). A probe rejected before admission (4xx) is not in `admitted`;
a client-side timeout is what the gate logs as 499.

## `series`

`step_s` (30), `start_ts`, `points` (120). Arrays of length `points`, oldest first, `null`
where the bucket has no sample: `decode_tok_s` (bucket mean), `running`, `queue_public`,
`queue_trusted`, `kv_usage` (bucket max), and `gpu_temp_c` = one array per GPU index (max).

Added 2026-09-22 (card #69): `budget_used_frac` (bucket max) - the trusted lane's in-flight
prompt-token budget used, `inflight_tokens / budget_tokens` (card #31), as a 0.0-1.0 fraction;
`null` in a bucket that saw no gate, or a gate too old to publish `budget_tokens` at all - never
a guessed 0. Public has no in-flight budget (only a per-request cap), so this is trusted-lane
only. `waiters_public` / `waiters_trusted` (bucket max): requests held back waiting for that
budget, same `null` rule. Requests essentially never queue AT THE GATEWAY (`queue_public` /
`queue_trusted` above are near-permanently 0) - they are either admitted or held on this budget,
which is what actually varies.

## `incidents[]`

`id`, `start`, `end` (null = still open; `end == start` = point event), `kind`, `detail`.
Kinds: `serve_down`, `gate_down` (ranges), `container_restart`, `xid` (points).
An Xid is ONE incident per (GPU or PCI address, Xid number, kernel timestamp to the second),
whatever wording its description had when it was booked: the back-fill, an overlapping journal
read or a later version that phrases the hint differently can never book it twice (a unique
`dedupe_key` in the database enforces it; duplicates from before were merged once, the oldest id
kept with the newest wording).

## `alerts[]`

`id`, `ts`, `rule`, `severity` (`info|warn|page|hardware`), `message`,
`recovered` (bool: this row is the "RECOVERED: …" message of `rule`),
`delivered` (bool: the alert script reported at least one leg delivered).
`delivered` also turns true later, when spooled agent mail for that alert is finally handed over.
Rule keys: `test_alert` (operator-injected, `lss-collector --test-alert`), `serve_down`, `serve_down_page`, `gate_down`, `container_restart:serve|gate`,
`xid:gpuN`, `queue_pressure`, `gate_waiters`, `thermal_temp:gpuN`, `thermal_throttle:gpuN`,
`thermal_digest`, `gpu_missing`, `c1_decode`, `rejects_413`, `rejects_429`,
`gate_charge_errors` / `gate_discount_inert` (added 2026-09-22, card #108: the gateway's
effective-token charging, watched TWO ways from its `/gate/health` `shadow` block - the
exception form, `charge_errors > 0` held `[rules] charge_errors_secs`, meaning the charge path
is raising and every trusted request is charging GROSS; and the OUTCOME form, a warm cache with
at least `discount_min_admissions` recent admissions and ZERO discounted, held
`discount_inert_secs`. Card #68 was the second shape and it was invisible for 2,809 requests: a
cold cache and a permanently-failing charge path look identical in the log. A COLD cache, a
quiet box, and a gate that is not answering at all never fire either rule - `gate_down` owns the
last one), `omp_default_mismatch`
(added 2026-09-21, card #36: `[rules] omp_config_path` on THIS box names a model this provider is
not serving - held `omp_mismatch_hold_secs` before firing so the seconds a swap takes to land does
not itself alert; `""` = not watched, e.g. a box with no omp installed).

## `probe`

`enabled`, `interval_s`, `baseline_tok_s` (null while learning),
`baseline_source` (`config` | `learned` | `learning N/12`),
`last_ok` (the newest **valid** completed probe — THE C1 reading; it may be older than
everything in `history`, and a probe that collided with traffic never replaces it),
`invalid_skipped` (int: probes newer than `last_ok` that were not a reading — invalid, skipped
or failed), `history` (last 50, newest first, valid or not). A record: `ts`, `status` (`ok | skipped_busy | error | timeout`), `ttft_ms`,
`decode_tok_s`, `tokens`, `detail`, `http_status` (int|null: what the gate answered; null =
nothing was sent, no response arrived, or the row predates the field),
`valid` (bool) and `invalid_reason` (string|null).

**Probe validity.** The probe asks "how fast does ONE stream decode on an idle engine". A
request that lands right after the idle check shares the engine with it (seen live: TTFT
57.8 s and 26.5 s, decode 140–150 instead of ~190), so a completed probe is `valid` only if
ALL of this held:

| | condition | else `invalid_reason` |
|---|---|---|
| a | engine `running == 0` and `queue == 0` immediately before | `busy_before` (the probe is not even sent: `status = skipped_busy`) |
| b | TTFT ≤ `thresholds.c1_max_ttft_s` (3 s) | `slow_ttft` |
| c | on an engine that reports `generation_tokens_total`: the counter grows by no more than the probe's own answer across the probe's own window - the check that decides, because `running` is a gauge sampled every 5 s and misses a request that starts and finishes between two scrapes (card #45, 2026-09-20) | `contended` |
| d | immediately after: engine `running ≤ 1` (the probe itself), `queue == 0`, and the gate's `admitted` (public + trusted) grew by nothing but the probe. Counters that cannot be read count as not holding | `contended` |

A GPU cold-clock check (invalidate a probe whose GPU sat below 500 MHz right before it began) was
tried alongside (c) and reverted the same day: live, a power-managed GPU idling down between
requests turned out to be the ROUTINE state right before almost every genuinely-idle probe, not a
rare event - the check took whole hours to zero valid probes before it was caught. (c) alone had
already caught every case it was built for.

A probe that failed has `valid: false` and `invalid_reason` = its status (`error`, `timeout`).
An invalid probe is stored and listed, and used NOWHERE: not for learning the baseline, not by
the `c1_decode` rule (it neither counts as low nor resets the streak), not in
`llm_serve_c1_*`. Rows from before the field existed were judged once by their TTFT.

## `thresholds`

What the rule engine is comparing against right now, so a client never hard-codes a copy.
`lss` colours the C1 value, the queue and the GPU temperatures from these.

| key | type | meaning |
|---|---|---|
| `c1_ratio` | number | `[rules] c1_ratio`, default `0.8` |
| `c1_consecutive` | int | low idle probes in a row before `c1_decode` fires, default `3` |
| `c1_baseline_tok_s` | number\|null | same value as `probe.baseline_tok_s`; null while learning |
| `c1_floor_tok_s` | number\|null | `c1_ratio × c1_baseline_tok_s`, rounded to 0.1: a VALID probe below this counts as low |
| `c1_max_ttft_s` | number | `[rules] c1_max_ttft_s`, default `3`: a slower first token makes the probe invalid |
| `queue_reqs` | number | `queue_pressure` threshold, default `24` |
| `thermal_temp_c` | number | `thermal_temp` threshold, default `90` |

A client talking to a collector without `thresholds` should assume these defaults (`lss` does).

## `spending` (card #176)

Spend over TIME, and the daily series a chart is drawn from. `null` when cost tracking is off,
exactly like `cost` - never a set of zeroed windows, because "we spent nothing" and "we cannot
price it" are different facts. Built by the collector from the **10-minute** rollup (retained 90
days), never from 5-second samples: a month of raw samples does not exist to be summed.

Every window carries its own coverage - card #102's rule applied to money, so a month that is
three days old is a three-day figure and says so.

| field | meaning |
|---|---|
| `spending.daily[].covered_secs` | how much of that day was priced - a partly-covered day is visible, not silently short |
| `spending.daily[].date` | local calendar date, `YYYY-MM-DD` |
| `spending.daily[].kwh` | that day's energy |
| `spending.daily[].ts` | local midnight of that day, so a chart can place it without re-parsing the date |
| `spending.daily[].usd` | that day's dollars |
| `spending.last_30d.covered_secs` | the last 30 local days, rolling: seconds of the window actually priced |
| `spending.last_30d.kwh` | the last 30 local days, rolling: energy in the window, same null rule as `usd` |
| `spending.last_30d.nominal_secs` | the last 30 local days, rolling: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.last_30d.usd` | the last 30 local days, rolling: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |
| `spending.last_7d.covered_secs` | the last 7 local days, rolling: seconds of the window actually priced |
| `spending.last_7d.kwh` | the last 7 local days, rolling: energy in the window, same null rule as `usd` |
| `spending.last_7d.nominal_secs` | the last 7 local days, rolling: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.last_7d.usd` | the last 7 local days, rolling: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |
| `spending.month_projection_usd` | where this month lands at the current rate - `null` unless at least 7 days have elapsed AND 80% of them were priced (card #176 item 3: never extrapolate 3 days into a month) |
| `spending.projection_note` | why there is no projection, when there is none - never silence |
| `spending.this_year.covered_secs` | since local midnight of January 1st (card #226, "year to date"): seconds of the window actually priced. The stored history (the 10-minute rollup, ~90 days) rarely reaches Jan 1, so this is usually far below `nominal_secs` - a floor, stated as one |
| `spending.this_year.kwh` | since local midnight of January 1st: energy in the window, same null rule as `usd` |
| `spending.this_year.nominal_secs` | since local midnight of January 1st: how long the window is MEANT to be (the time since Jan 1) |
| `spending.this_year.usd` | since local midnight of January 1st: dollars spent in the window - `null` when nothing in it could be priced (never 0.00) |
| `spending.year_first_day` | the first local date (`YYYY-MM-DD`) this year with any priced energy - how a partial year says "since <date>"; `null` when nothing this year was priced |
| `spending.this_month.covered_secs` | since local midnight of the 1st: seconds of the window actually priced |
| `spending.this_month.kwh` | since local midnight of the 1st: energy in the window, same null rule as `usd` |
| `spending.this_month.nominal_secs` | since local midnight of the 1st: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.this_month.usd` | since local midnight of the 1st: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |
| `spending.this_week.covered_secs` | since local midnight of this week's Monday: seconds of the window actually priced |
| `spending.this_week.kwh` | since local midnight of this week's Monday: energy in the window, same null rule as `usd` |
| `spending.this_week.nominal_secs` | since local midnight of this week's Monday: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.this_week.usd` | since local midnight of this week's Monday: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |
| `spending.today.covered_secs` | since local midnight: seconds of the window actually priced |
| `spending.today.kwh` | since local midnight: energy in the window, same null rule as `usd` |
| `spending.today.nominal_secs` | since local midnight: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.today.usd` | since local midnight: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |
| `spending.yesterday.covered_secs` | the previous local day, whole: seconds of the window actually priced |
| `spending.yesterday.kwh` | the previous local day, whole: energy in the window, same null rule as `usd` |
| `spending.yesterday.nominal_secs` | the previous local day, whole: how long the window is MEANT to be - a month-to-date on the 3rd is 3 days, not 30 |
| `spending.yesterday.usd` | the previous local day, whole: dollars spent in the window - `null` when nothing in it could be priced (never 0.00, which would read as "we spent nothing") |

# `GET /rules`

What Prometheus' *Alerts* page and Alertmanager show, in one document, pre-rendered every poll.

| key | meaning |
|---|---|
| `rules[]` | one row per rule: `rule`, `severity`, `state` (`ok` \| `pending` \| `firing` \| `muted`), `threshold` (the condition in words, with the numbers in force), `value` (what the rule sees now), `pending_since` (int\|null: since when the condition holds), `last_fired` (int\|null), `cooldown_remaining_s` |
| `firing` | same as `/status.firing` |
| `spool_depth` | int\|null: agent mail waiting in `mail-spool/`; null = unreadable |
| `alerts[]` | last 100, newest first, same shape as `/status.alerts[]` (`delivered` = a leg got through, at once or later from the spool) |
| `incidents[]` | last 90 days, at most 200, newest first |
| `uptime` | `h24`, `d7`: share of the window the serve answered (0..1, from `serve_down` incidents), `since` = the collector's first sample; a window reaching further back is cut there. Time the collector itself was down counts as up (unknown) |

`pending` = the condition is true but its hold time (or the rule's cooldown) is not over; it is
what Prometheus calls *pending*. Event rules (`xid:gpuN`, `container_restart:role`) have no
firing state: they fire once per event and show `last_fired`; `container_restart:*` is `pending`
while it waits for the container to stay up. `xid:*` is the placeholder row until an Xid has been seen.

`muted` (added 2026-09-21, card #52): a rule that has fired and recovered (flapped) 3 or more
times within the mute window reads `muted` instead of cycling through `ok`/`pending`/`firing` -
the row is REWRITTEN, never hidden: `threshold` names the mute rule itself, `value` becomes
`"<rule> unreliable - N flaps/24h, muted"`. Only `c1_stale` can reach this state today (the box's
own scattered idle windows made it flap several times a day); the collector stops sending
individual fire/recover alerts for a muted rule (nothing new in the mail spool) but the row, and
its running flap count, are never suppressed - `GET /rules` still lists it, and `/status.firing`
deliberately leaves a muted rule out (it reads as "measurement unreliable", not "problem now").

# `GET /series`

`GET /series?metrics=a,b:max&range=6h&step=auto`

| param | |
|---|---|
| `metrics` | comma-separated, at most 128. `name` or `name:avg\|min\|max`. Without it the answer is the catalogue: `{"metrics":[…],"ranges":[…],"aggregates":[…],"max_points":600}` |
| `range` | `15m` `1h` `6h` `24h` `7d`, any `<n>s\|m\|h\|d`, or seconds. Default `1h`, capped at the 10-minute tier's retention |
| `step` | `auto` (default) or a duration. Raised, never refused, when it would give more than 600 points |

Answer: `v`, `generated_at`, `range_s`, `step_s`, `start_ts`, `points`, `tier` (`raw` \| `1m` \|
`10m`), `series` = `{token: [number\|null, …]}` and `unknown` = requested names never recorded.
**Every array has exactly `points` entries and index `i` is the bucket starting at
`start_ts + i × step_s` in all of them**; buckets sit on multiples of `step_s`, the last one
holds *now*. `null` = no data, never zero.

| tier | source | kept | used when |
|---|---|---|---|
| `raw` | the 5 s samples | 24 h (`raw_hours`) | step < 60 s and range ≤ 2 h (`auto`: 15m → 5 s, 1h → 10 s) |
| `1m` | avg/min/max per minute, written as each minute closes | 14 d (`retention_days`) | step < 10 min (`auto`: 6h → 60 s, 24h → 180 s) |
| `10m` | avg/min/max per 10 minutes | 90 d (`rollup_10m_days`) | everything longer (`auto`: 7d → 20 min) |

Both rollup tiers are fed from the raw samples **on write**, so a 7-day chart reads ~500 rows per
series. The minute in progress is not in the `1m` tier yet (a 6 h chart lags by up to a minute).
A collector restart replays the open buckets from the stored samples and loses nothing.

Default aggregate when the token does not say: **max** for levels where the spike is the point
(`queue*`, `running*`, `waiters*`, `inflight*`, `*_temp_c`, `kv_usage`, `*_thr_*`, `*_p90_ms`,
`*_p99_ms`), **avg** for the rest.

Metrics: `serve_up`, `gate_up`, `decode_tok_s` (SGLang's gauge), `gen_tok_s`, `prompt_tok_s`,
`req_per_min` (rates of the engine counters), `running`, `queue`, `running_public|trusted`,
`queue_public|trusted`, `kv_usage`, `cache_hit_rate`, `spec_accept_length`, `spec_accept_rate`,
`inflight_public|trusted`, `waiters_public|trusted`, `gate_requests`, `gpu_power_total_w`, per GPU
`gpuN_temp_c`, `gpuN_power_w`, `gpuN_clock_mhz`, `gpuN_util_pct`, `gpuN_mem_used_mib`,
`gpuN_mem_util_pct`, `gpuN_mem_temp_c`, `gpuN_thr_thermal`, `gpuN_thr_hw`, `gpuN_thr_power`
(0/1; their avg is the share of time throttled); the per-minute latency series
`ttft|e2e|itl|queue_time` × `_p50_ms|_p90_ms|_p99_ms|_avg_ms` (only minutes with requests have a
value; ask for `step=60` or coarser); and, from the probes table at every tier, `c1_tok_s` (valid
probes), `c1_invalid_tok_s`, `c1_invalid` (1 per invalid / skipped / failed probe).

## Percentiles

SGLang publishes cumulative histograms. The collector diffs the `_bucket` series between
scrapes, sums the deltas per 1-minute (and 10-minute) window, and computes p50/p90/p99 from
THAT with Prometheus' `histogram_quantile` interpolation, the way Grafana's
`histogram_quantile(0.99, rate(..._bucket[1m]))` does. A counter that goes backwards (engine
restart) or a changed bucket layout makes the new totals the delta. The 10-minute tier merges
BUCKETS, it never averages percentiles. Label rules are the ones of the gauges: `tp_rank="0"`
(or no rank), an explicit `priority=""` row set wins, otherwise the label sets are summed.
A rank that lands in `+Inf` reports the highest finite bound. The resolution is SGLang's
bucket layout (TTFT: 0.1 … 400 s in 18 steps), so a p50 inside the 2–4 s bucket is an
interpolation, not a measurement.

# `GET /hist`

`GET /hist?metric=ttft|e2e|itl|queue_time&range=1h` — the bucket deltas behind the percentiles.
`le` = finite upper bounds in **seconds** (one more bucket, `+Inf`, follows), `total` =
observations per bucket over the range (`le.length + 1` long), `summary` = `{p50_ms, p90_ms,
p99_ms, avg_ms, count}` over the range (null = nothing), `step_s` / `start_ts` / `columns` = the
same counts cut into at most 120 time columns for a heat strip (`[]` = an empty column). From the
1-minute windows up to 24 h, the 10-minute windows beyond. `itl` counts token gaps, the others requests.

# `GET /gateway`

`GET /gateway?range=1h` — from the gate's audit log. `lanes[]` (`public`, `trusted`), `keys[]`
(busiest first, max 50, `lane` says where they came in), `ips[]` (busiest first, max 20). A row:
`name`, `lane`, `requests`, `coded_requests`, `2xx`, `4xx`, `5xx`, `413`, `429`, `499`, `503`,
`est_tokens_n`, `est_tokens_avg`, `est_tokens_max` (the gate logs `estimated_tokens` only on
requests it rejects, so these describe the rejected ones; null = none).

**A code column is `null` when it is NOT KNOWN, never `0`.** Per-key status codes are recorded
from 2026-09-19 14:47 on; older data has per-key counts only. `codes_coverage` says how much of
the range has them: `full`, `partial` or `none`. When `partial`, `codes_since` (unix s; else
null) is where the breakdown starts: a key row's code columns then count only its
`coded_requests` of `requests`, and a key seen only before that has `null` in every code
column. Lane rows are always complete. Address rows never carry codes (`null`). `lss` draws
`null` as `-` and notes `codes since HH:MM` in the KEYS title.

`probes_excluded` = the collector's own C1 probes in the range, already taken out of the
`trusted` lane, the `(no key)` row and the probe's own address row in `ips[]` (`127.0.0.x` when
`gate_url` is loopback; with a host name in `gate_url` the address cannot be known and the row
stays). **Client addresses are masked when the log line is read** (`98.97.137.x`,
`2001:db8:1::x`): a full address is never stored or served. Up to 1 h the stored samples are
merged exactly; longer ranges use a 10-minute gate-log rollup (so the left edge is rounded down
to 10 minutes) plus the bucket still open.

# `GET /metrics`

Prometheus text, every series labelled `host="<host>"`. The contract series:

```
llm_serve_up                                  1 | 0
llm_serve_restart_count                       docker RestartCount of the serve container
llm_serve_c1_decode_tokens_per_second         last VALID idle C1 probe (keeps its value while probes are invalid)
llm_serve_c1_probe_age_seconds                seconds since that probe: judge freshness with this
```

`llm_serve_c1_*` series are **absent** until a valid probe exists (a 0 would read as
"decoding at 0 tok/s"). They only ever move on a VALID probe; while the box is busy they hold
the last valid value and `llm_serve_c1_probe_age_seconds` /
`llm_serve_c1_probes_invalid_skipped` grow. `llm_serve_c1_probe_timestamp_seconds` is the same
moment as an absolute time.
Also: `llm_serve_info{model,container}`, `llm_serve_restarts_today`,
`llm_serve_uptime_seconds`, `llm_serve_c1_ttft_seconds`,
`llm_serve_c1_baseline_tokens_per_second`, `llm_serve_decode_tokens_per_second`,
`llm_serve_kv_usage_ratio`, `llm_serve_slots`,
`llm_serve_running_requests{lane}`, `llm_serve_queued_requests{lane}`, `llm_gate_up`,
`llm_gate_waiters{lane}`, `llm_gate_inflight_tokens{lane}`,
`llm_gate_rejected_total{lane,code}`, `llm_gpu_temperature_celsius{gpu}`,
`llm_gpu_power_watts{gpu}`, `llm_gpu_utilization_percent{gpu}`,
`llm_gpu_memory_used_mib{gpu}`, `llm_gpu_throttle_mask{gpu}`,
`llm_gpu_thermal_throttled{gpu}`, `lss_alerts_firing`, `lss_incidents_open`,
`lss_last_sample_timestamp_seconds`.

# `POST /test-alert`

Body = the message (≤ 300 chars). Accepted only on the `127.0.0.1` listener from a loopback
peer (`403` otherwise), `202 {"queued":true,"rule":"test_alert","message":"[TEST] …"}`. The
poll loop hands it to the rule engine on its next turn, so it becomes an `alerts[]` row and an
`alert_cmd` call exactly like a real alert. `lss-collector --test-alert "<msg>"` is the client.

# `GET /health`

`{"ok":true,"last_sample_age_s":3,"version":"0.1.0"}` — 200 while the newest sample is at
most 30 s old, 503 otherwise. This is what the seat's up-check asks.

## `users`  (added 2026-09-20)

From the gateway's trusted-only `GET /gate/health` (a gateway publishing /gate/health v5.2+). With an older gateway,
or while it is down: `{"available": false, "active_now": 0, "totals": {…zeros…}, "rows": []}`.

| key | meaning |
|---|---|
| `available` | the gateway publishes per-user stats |
| `active_now` | REAL users with a request in flight (never the bench, never the probe) |
| `totals` | `inflight`, `users_active_10m`, `users_24h`, `unauthenticated_24h`, with the bench and the probe taken out; and (added 2026-09-23, card #211) `inflight_by_upstream`: `inflight` split by the gateway's upstream name (the MACHINE serving it), upstreams with nothing in flight omitted. `null` = a gateway older than v5.10, which cannot say which machine a request is on - never read `inflight` as load on any one machine |
| `rows[]` | real users, busiest first (at most 50): `name` (the alias when there is one, else `user`), `series_id` (the `<id>` of the `user_inflight.<id>` / `user_rpm.<id>` series), and the gateway's row: `lane`, `user`, `inflight`, `peak_inflight_10m`, `peak_inflight_24h`, `conc_limit`, `rpm_limit` (null = no limit), `rpm_now`, `requests_1h`, `requests_24h`, `ok_24h`, `rejected_24h`, `errors_24h`, `client_closed_24h`, `prompt_tokens_est_24h` (the gateway's estimate), `completion_tokens_24h`, `completion_tokens_exact` (false = partly estimated), `inflight_prompt_tokens_est`, `first_seen`, `last_seen`, `last_status`, (added 2026-09-23) `secs_since_last_request`, and (added 2026-09-23, card #211) `by_upstream`: `{<upstream name>: {inflight, requests_24h, peak_inflight_10m, peak_inflight_24h}}` - which machine this user's load is on. The flat figures in the row are the sum across every machine the gateway fronts. `{}` on a gateway older than v5.10 (and on v5.10 for a user whose every request was rejected before its model resolved) |
| `rows[].secs_since_last_request` | int\|null — seconds since the gateway last accounted a request for that caller, worked out by the COLLECTOR against its own clock when it read the snapshot, so a reader never has to reason about clock skew from the raw `last_seen`. It is since their last request, not their last token (the gateway keeps no per-user token clock), so a caller streaming a long answer has both a growing age and `inflight > 0` — read `inflight` first. `null` = the gateway published no `last_seen` for them: **unknown, never 0 and never "idle"**. Same for `bench.secs_since_last_request` / `probe.secs_since_last_request` |
| `bench` | absent, or the `lss-bench` row: requests sent with `X-LSS-Bench: 1`. In no total |
| `probe` | absent, or the `lss-probe` row: the collector's own C1 probes, taken out of the row of the address they come from (the rest of that address's traffic stays a user). In no total |

Trusted-lane users are client addresses; `name` comes from `[[user_alias]]` in the collector's
config, else from `tailscale status --json` when `tailscale_lookup = true`.
Series: `users_active`, `users_inflight` (every machine the gateway fronts), and per active user `user_inflight.<id>`, `user_rpm.<id>`. From a v5.10+ gateway also, per machine, `users_active.<upstream>` / `users_inflight.<upstream>` (an idle machine reads 0) and THIS machine's under a fixed name, `users_active_here` / `users_inflight_here` - this machine being the upstream that serves the model this collector's engine serves, else the gateway's first configured upstream (card #211). An older gateway produces none of the per-machine series, and the growth advice then says its user count is across every machine.

`serve` also gained `cached_tokens_total` and `context_len`.

## Added 2026-09-20: reading speed, targets, Fahrenheit twins (all additive)
| key | type | meaning |
|---|---|---|
| `serve.prefill_tok_s` | number\|null | READING SPEED (prefill): prompt tokens the GPUs read per second while reading, over the last 10 minutes. Cache hits are not reading and are never counted. `null` = nothing was read in that time |
| `serve.prefill_tok_s_typical` | number\|null | the same over the whole life of the current loadout: what to show while nothing is being read |
| `serve.cached_share_10m` | number\|null | share (0..1) of the last 10 minutes' prompt tokens that came from the prefix cache |
| `serve.prefill_inflight` | number | requests whose prompt is being read right now |
| `series.prefill_tok_s` | (number\|null)[] | prompt tokens read per second of each 30 s bucket (0 = nothing was read) |
| `targets` | object | `window` (`24h`) and `rows[]` = `{key: ttft\|speed\|queue\|uptime, label, short, met_pct, goal_pct, basis: requests\|tokens\|time, ok}`; `met_pct` / `ok` are `null` until something was measured; a target set to `0` has no row |
| `gpus[].temp_f`, `gpus[].mem_temp_f` | number\|null | the Fahrenheit twins of `temp_c` / `mem_temp_c`. JSON always keeps Celsius; `temp_units` only changes what people read |
| `thresholds.thermal_temp_f` | number | the twin of `thermal_temp_c` |

## Added 2026-09-23: the page-1 trend series (additive, card #281)
Seven more 1-hour series on the same grid as `series.decode_tok_s` (`step_s` 30, `points` 120,
oldest first). An older collector sends none of them: a client reads a missing series as "no
data" (an empty chart), never as zeros. `null` in a bucket = no data there.
| key | type | meaning |
|---|---|---|
| `series.ttft_p99_ms` | (number\|null)[] | time to first token, p99, ms, from the engine's own histogram. Filled by the collector from its stored minute rollups (the `/series` metric of the same name), so each minute's p99 sits in both of that minute's buckets. All `null` when the engine publishes no TTFT histogram |
| `series.itl_p99_ms` | (number\|null)[] | time between tokens, p99, ms - same source and resolution as `ttft_p99_ms` |
| `series.prefix_hit` | (number\|null)[] | prefix-cache share (0..1) of the prompt tokens that arrived in the bucket: cached / prompt counter growth, the same "of prompt tokens" denominator as `serve.cached_share_10m`. `null` = no prompt tokens arrived, or the engine reports no token counters |
| `series.spec_accept_rate` | (number\|null)[] | speculative-decoding acceptance rate (0..1), the bucket's mean. `null` = the engine runs no speculative decoding (it lists `spec` in `serve.not_reported`) - never 0 |
| `series.refused` | (number\|null)[] | requests the gateway refused in the bucket, all lanes: 413 (too large) + 429 (too busy), from its audit log. `null` = no gateway read in that bucket |
| `series.gpu_power_w` | (number\|null)[] | total GPU power, all cards, W, the bucket's max. `null` = no card reported power |
| `series.usd_per_hour` | (number\|null)[] | electricity cost, $/h: `gpu_power_w` priced at the rate in force at the bucket's midpoint (the collector's `rates.toml`, the same lookup as `cost.live_usd_per_hour`). All `null` with no rates table |

## Added 2026-09-23: sustained GPU utilisation (additive)
| key | type | meaning |
|---|---|---|
| `gpus[].util_pct_med` | number\|null | this card's utilisation as a MEDIAN over the last 6 polls (30 s at the default 5 s poll) — the SUSTAINED figure. `util_pct` beside it stays the instantaneous NVML reading. `null` = the collector has not held 6 polls carrying this card yet (or predates the field) |

Why it exists (card #197): a TP group's "one card is lagging" alarm was scored on a single NVML
sample, and 80% of what it fired on lasted ONE poll — a different card dipping each time while
the pack sat at 95+, which is the sampler catching whichever card is between kernels rather than
a straggler. Replayed read-only over 17,445 stored polls (24.25 h, 4 cards), scoring the median
per card instead cuts the share of polls that alarm can headline from 7.94% to 3.16%. A reader
wanting "is this card behind the others right now" should compare `util_pct_med` across the
array, not `util_pct`; a reader wanting the instantaneous board reading still has `util_pct`.

## Added 2026-09-21: C1 staleness (all additive)
On a busy box the stricter probe validation (card #45) can leave C1 without a single VALID
reading for hours; an hours-old number sitting next to a live clock, with nothing saying so, is
the same lie in the other direction as an unproved reading shown as proved.
| key | type | meaning |
|---|---|---|
| `thresholds.c1_stale_secs` | number | C1 counts as stale once the newest VALID reading is this many seconds old (`rules.c1_stale_probe_intervals`, default 6, x the probe's own interval). The client uses this, not its own copy of the multiplier, so the screen and the `c1_stale` rule never disagree |
A new `info`-severity rule, `c1_stale`: fires the instant `now - last_ok.ts >= thresholds.c1_stale_secs`
(no extra hold time - the multi-interval wait is already the condition) and recovers the instant a
valid reading arrives again. Entirely independent of `c1_decode` (the low-speed alert): neither
can gate or delay the other. `lss status` and the overview mark a stale reading in the C1 line
itself rather than a separate document field, using the same `c1_stale_secs` threshold.

## Added 2026-09-21: gate admission budget (all additive; card #31)
The trusted lane's in-flight token budget and each lane's per-request prompt-token cap were
already enforced by the gateway; neither was visible, so a client could not show how close to the
ceiling admission actually was.
| key | type | meaning |
|---|---|---|
| `lanes.public.budget_tokens`, `lanes.trusted.budget_tokens` | number\|null | the in-flight token budget `inflight_tokens` is judged against. `null` for public: it has no in-flight budget at all, only the cap below; `null` also on a gate too old to publish it |
| `lanes.public.max_prompt_tokens`, `lanes.trusted.max_prompt_tokens` | number\|null | the per-request prompt-token cap (a request over this is rejected 413). `null` on a gate too old to publish it |
`lss status`'s LANE line and the overview's LANES panel show `inflight N of BUDGET tok (P%)`
when a budget is published, and the plain token count when it is not (public, or an older gate) -
never an invented "of 0".

## Added 2026-09-21: page 1 redesign fields (all additive; card #51)
3-expert panel verdict (lianmin-zheng, woosuk-kwon, hamel-husain): KV % alone reads as headroom
while eviction is heavy; nothing showed whether the engine had gone silent; the WORK box needs a
recent window, not just all-time counters; and the loadout's safe flags were already computed but
not carried onto `/status`.
| key | type | meaning |
|---|---|---|
| `serve.evicted_tok_per_hour_10m` | number\|null | KV tokens thrown out of the prefix cache, as a tokens/hour rate over the last 10 min. `null` = fewer than two comparable samples in the window yet - not zero |
| `serve.preempted_per_hour_10m` | number\|null | requests pushed out of the running batch to make room, as a per-hour rate over the last 10 min. `null` = too few samples or an engine that does not report it - never zero for either |
| `serve.secs_since_last_token` | int\|null | seconds since `generation_tokens_total` last grew, anywhere in the retained history (up to 1 h). `null` = no growth in that whole hour |
| `serve.work_1h` | object\|null | `{prompt, cached, generated, requests}` - the same four counters as the `_total` all-time ones, grown over the last hour only. `null` = fewer than two comparable samples in the last hour |
| `loadout.flags` | string | the loadout's safe launch flags (`tp 4 · ctx 1048576 · quant fp4 · slots 8 · spec NEXTN …`) - already computed for every loadout identity (see `GET /loadouts`), just not on `/status` before this |
An admission verdict (`OK` / `GATEWAY-LIMITED` / `ENGINE-FULL` / `KV-LIMITED` / `DOWN`) is
computed CLIENT-SIDE (`Status::admission_verdict`) from fields already on this document - it is
not a new JSON field.

## Added 2026-09-21: live decode speed from real traffic (all additive; card #50)
The idle-only C1 probe goes stale for 30+ minutes at a time on a box that is never idle (3 days of
samples: best hour 94% quiet, longest unbroken quiet run all week 5m00s). `serve.live_decode` is a
speed number derived from REAL requests instead - the engine's own concurrency curve
(`GET /loadouts`'s `curve[]`, already fed passively from every 5 s sample, no benchmark involved),
read at the concurrency level the engine is at right now.
| key | type | meaning |
|---|---|---|
| `serve.live_decode` | object\|null | one `curve[]` row (`{running, samples, tok_s, per_request_tok_s, ttft_ms, spec_accept_length, source}`) at the CURRENT concurrency level. `null` = nothing has ever been observed at exactly this level yet, or `page1_fields_deployed` is `false` (older collector) - never a fabricated or interpolated number for a level nobody has been seen at |
**Bucketed by concurrency on purpose**: raw tok/s under load is not comparable to C1's clean-room,
1-request number, so `live_decode` is always shown labelled with its own concurrency and sample
count (e.g. "80.0 tok/s/req at 4 in flight (37 samples)"), beside C1 rather than instead of it, and
must never be presented as if it were the same measurement.

## Added 2026-09-21: planned-maintenance mode (all additive; card #22)
See [`POST /maintenance/start`, `POST /maintenance/stop`](#post-maintenancestart-post-maintenancestop)
above for the full behaviour; this is its shape on `/status`.
| key | type | meaning |
|---|---|---|
| `maintenance` | object | `{active, reason, started_at, expires_at}` - never null; a fresh collector's default is simply `active: false` with the other three at their zero values |

## Added 2026-09-22: electricity cost from GPU watts, time-of-use aware (all additive; card #75)
`cost` is `null` unless `[rates] path` in the collector's own config points at a real, parseable
rate table (`~/.config/lss/rates.toml` by default) - **never a guessed number**. The table itself
lives in that separate file, never in `collector.toml` and never in this repo; see
`packaging/rates.toml.example` for the shape (a generic example with obviously-fake numbers).
| key | type | meaning |
|---|---|---|
| `cost` | object\|null | see below; `null` = no rate table configured, cost tracking is simply off |
| `cost.rate_name` | string | the plan's own name, from the config file |
| `cost.effective_date` | string | the tariff/plan's own effective date (e.g. `"2026-01-01"`) - a stale table should be visible on screen, never silently trusted forever |
| `cost.is_flat` | bool | `true` = a single flat number was configured, not a real time-of-use schedule - **must be labelled "flat" wherever shown**, never presented as if it were the real plan |
| `cost.current_usd_per_kwh` | number\|null | the $/kWh that applies RIGHT NOW, from the rate table |
| `cost.current_period` | string | the period name that applies right now (e.g. `"summer_on_peak"`, or `"flat"`) |
| `cost.live_usd_per_hour` | number\|null | dollars per hour AT THE CURRENT DRAW: the latest GPU watt reading x `current_usd_per_kwh`. `null` = no GPU power reading right now, never 0 |
| `cost.today_kwh`, `cost.today_usd` | number\|null | since local midnight, from the stored watt samples, EACH PRICED AT ITS OWN TIMESTAMP (a time-of-use plan makes one average rate a lie - a real utility's summer peak can be 2x or more its own off-peak). `null` = nothing could be priced yet today, never a fabricated 0 |
| `cost.today_covered_secs` | int\|null | how many of the seconds since local midnight `today_kwh`/`today_usd` actually cover - compare against "now minus local midnight" to tell full coverage from a collector that has only been up part of the day. `null` only when `today_kwh` is also `null` |
| `cost.last_24h_covered_secs` | int\|null | the same idea for `last_24h_kwh`/`last_24h_usd` - compare against 86,400 |
| `cost.today_usd_per_million_generated_tokens`, `cost.today_usd_per_million_prompt_tokens` | number\|null | the figure the whole card exists for: dollars per million tokens today, generated and prompt counted separately (from the SAME `today_usd` and the day's own token counters - never mixed with a different window) |
| `cost.fixed_usd_per_day` | number\|null | a per-day fixed charge from the rate table - shown for context, **never added into `today_usd` or any other figure above**: a marginal per-hour or per-day cost must never pretend to be a bill |
| `cost.unresolved_usd_per_kwh` | number\|null | an amount that MAY belong on top of every `usd_per_kwh` above but is not settled (card #75: a real tariff can list extra per-kWh line items on top of the headline rate; the utility's own consumer page publishes delivery+generation only) - shown as a stated uncertainty, **never applied either way** |

## Added 2026-09-22: rolling-24h cost, and tokens by window (all additive; card #73)
Page 1 was redesigned around COST and LOADOUT instead of health (the panel-built ADMIT/SPEED/WORK/
EVENTS sections are gone from the `v` overview; page 1's own `v` toggle and the original overview
are unchanged). Two new pieces of data support it:
| key | type | meaning |
|---|---|---|
| `cost.last_24h_kwh`, `cost.last_24h_usd` | number\|null | the ROLLING last 24 hours - **distinct from `cost.today_kwh`/`cost.today_usd`, which are since local midnight** - priced from the collector's own 1-minute rollup of stored energy (`sum_energy_j`), each bucket still priced at its own timestamp, never a blended average. This is the window the USERS table's per-user token counts already use (the gate's own `_24h` counters), so a per-user dollar split can use the SAME window its token share comes from - splitting `today_usd` by a rolling-24h share would silently mix two different windows. `null` = no priced bucket in the last 24h |
| `tokens_by_window` | object\|null | `null` = an older collector that predates this field. Otherwise always present; each of the four fields below is independently `number\|null` per field (`prompt`, `cached`, `generated`, `requests`) |
| `tokens_by_window.hour` | object\|null | same figures as `serve.work_1h` (raw 5s samples) |
| `tokens_by_window.day`, `tokens_by_window.week` | object\|null | from the 1-minute rollup (`retention_days`, default 14) - `null` = no `tok_gen` rollup stored yet at all (not deployed long enough), never a fabricated 0 |
| `tokens_by_window.month` | object\|null | from the 10-minute rollup (`rollup_10m_days`, default 90) |
| `tokens_by_window.{hour,day,week,month}_covered_secs` | int\|null | seconds of REAL data behind the matching window - compare against 3,600 / 86,400 / 604,800 / 2,592,000 to tell full coverage from a collector that has not been up that long. `null` only when the matching window is also `null`. **Divide a window's figures by its nominal length and you will be wrong by the coverage ratio; divide by this instead.** |

**Per-user cost/energy across hour/week/month is NOT in this document.** The gate's own `_24h`
counters are the only per-user history this project stores anywhere; a per-user breakdown at
other windows needs a new per-user rollup table this collector does not build yet - a real,
flagged follow-up, not guessed at. The USERS section on page 1 shows per-user dollars for the
last 24h only (`cost.last_24h_usd` x that user's share of the 24h token total).

## Added 2026-09-22: cost basis fixed to real work, standby separated (all additive; card #174)
the owner: "i think the cost per 1m token is off" - he was right. The old headline
(`cost.today_usd_per_million_generated_tokens`) divided ALL of today's energy by GENERATED
tokens only; measured on a live hour this engine did about 10x more real token work reading
(uncached prefill) than writing, so the number read roughly 10x too high for what a reader would
compare against a provider's per-million price. `generated`/`prompt` stay on the document
(real, correctly documented figures, just not the headline any more) - these three fields are
the new primary basis.
| key | type | meaning |
|---|---|---|
| `cost.today_usd_per_million_real_work_tokens` | number\|null | the PRIMARY figure: today's WORK energy only (standby excluded, below) over (uncached prefill + generated) tokens - the tokens the engine actually computed. A cache hit costs KV residency, not compute, so it is excluded from the denominator the same way standby is excluded from the numerator. `null` when there is nothing to divide by yet |
| `cost.today_standby_kwh`, `cost.today_standby_usd` | number\|null | today's energy/cost while the engine had NOTHING running (idle/standby power draw) - separated from `cost.today_kwh`/`cost.today_usd` (which still cover everything, work and standby together) so a quiet day does not look expensive and a busy day does not look cheap in the per-token figure. `null` = no idle samples today yet |

## Added 2026-09-22: WATCH - what the sources you follow have published (all additive; card #74)
`watch` is `null` unless `[watch] path` in the collector's own config points at a real,
parseable JSON file (`~/.config/lss/watch.json` by default) - **never a guessed source list**.
This collector NEVER fetches anything itself to build this file; a separate process the owner
runs on whatever schedule they choose (a cron job, an agent sweep, `scripts/watch-check.sh` as a
generic starting point) writes it, and the collector only reads it back. See
`packaging/watch.json.example` for the shape.
| key | type | meaning |
|---|---|---|
| `watch` | object\|null | `null` = no `[watch] path` configured, watch tracking is simply off |
| `watch.sources` | array | one entry per followed source, in the file's own order |
| `watch.sources[].name`, `.covers` | string | the source's name, and plain English for what it is followed FOR (e.g. "GLM loadouts on the GPU box") - both required on screen, never just a bare name |
| `watch.sources[].last_checked` | int\|null | unix ts of the last time THIS source was actually checked - `null` = never checked yet. Its own field per source (a fast-moving repo can be checked hourly, a slow one daily) - **never borrowed from a different source or a whole-file timestamp** |
| `watch.sources[].newest` | object\|null | the newest item this source has published - `null` while `last_checked` is also `null` means "never checked"; `null` while `last_checked` is set means "checked, and found nothing new (yet)" - a screen must tell those two apart, never blur them into the same blank |
| `watch.sources[].newest.date` | string | the item's OWN date (`"YYYY-MM-DD"`), never guessed - **distinct from `last_checked`**, which is when the CHECK ran, not when the item was published |
| `watch.sources[].newest.summary` | string | the item's real CONTENT - **never a bare title**. Bought with a mistake (2026-09-01: a title-only sweep reported a defect in our own checkpoint that the card's own body said was someone else's engine, not ours) |
| `watch.sources[].newest.url` | string | link to the item itself |
| `watch.sources[].newest.has_receipt` | bool | `false` = a bare social/hypothesis-tier claim with no receipt (an issue, PR, commit, file:line, or HF id) - **must render UNVERIFIED on screen, never presented as a finding** |

**This document, and every page built on it, only ever REPORTS.** Nothing in this crate or
`lss-collector` acts on `watch` - switches a loadout, restarts a service - that stays a human
decision, always, on every path that touches this field.

## Added 2026-09-20: any engine (all additive)
| key | type | meaning |
|---|---|---|
| `serve.engine` | string | which engine serves it: `sglang`, `vllm`, `llamacpp`, `ollama`, `lmstudio`, `tgi`, `openai`; `""` from an older collector (= SGLang) |
| `serve.not_reported` | string[] | numbers THIS engine does not publish, as keys: `running`, `queued`, `kv_usage`, `kv_capacity`, `decode_tok_s`, `prefill_tok_s`, `tokens`, `requests`, `cache_hit`, `spec`, `ttft`, `itl`, `e2e`, `queue_time`, `slots`. A number listed here is `0` / `null` in this document and must be shown as `n/a (not reported by <engine>)`, never as zero ([ENGINES.md](ENGINES.md) has the matrix) |
| `gate.absent` | bool | no gateway is configured (`gate_url = ""`). `gate.up` is `true` then, so nothing reads as an outage; LANES / USERS / GATEWAY say "no gateway configured" |
| `gpu_source` | string | where the GPU numbers come from: `nvidia`, `amd`, `apple`, `none`. `none` = no GPU tool on the machine: `gpus` is empty and that is not a failure. On `apple`, `temp_c` and `power_w` are `null` (they need root) |
| `bench.builtin` | bool | no external harness is configured: `lss bench quick` / `full` run lss's built-in mini bench |

Series names gained `prefill_tok_s` (only for the steps in which something was read, so its
average is the speed while reading), `tok_prefill` (prompt tokens read, `:sum`), and the
request-time split `sum_prefill_s` / `sum_e2e_s`.

# `GET /tokens`
`v`, `generated_at`, `since` (the windowed series start here), `windows[]` =
`{name, secs, generated, prompt, cached, cache_share, requests}` for `1h`, `24h`, `7d`, `today`
(since local midnight on the collector's host) and `all`; `all_time_since`; `peak` =
`{tok_s, ts}` (the highest total decode tok/s ever seen) and `peak_by_day[]` (14 days, newest
first); `output_len` / `prompt_len` = `{p50, p90, p99, avg, count}` in tokens over 7 days;
`per_user_available`, `per_user[]` = `{name, lane, prompt_est_24h, output_24h, output_exact,
requests_24h, output_share}`.

The windows are exact sums of the engine's counters' growth (`tok_gen`, `tok_prompt`,
`tok_cached`, `sum_requests`). **`all` survives serve restarts**: the collector keeps a ledger fed
by that growth; a counter that goes down, or a changed container start time, is a reset, and
everything the new counter shows is counted as new.

# `GET /loadouts`
`loadouts[]`, newest first; the one with `current: true` is serving now. Each is the PASSIVE
scorecard of the loadout (`id`, `model`, `image`, `image_tag`, `args_hash` (sha256), `flags`,
`first_seen`, `last_seen`, `started_at`, `runs`, `hours_observed`, `uptime_pct`, `c1_tok_s`,
`c1_best_tok_s`, `c1_probes`, `decode_p50_tok_s`, `decode_p90_tok_s`, `slots`, `curve[]` =
`{running, samples, tok_s, per_request_tok_s, ttft_ms, spec_accept_length, source}`,
`saturation` = `{kind: not_enough | after | still_growing, users, total_tok_s}`, `peak_tok_s`,
`peak_at_running`, `prefill_tok_s`, `ttft_p50_ms`, `ttft_p90_ms`, `ttft_by_size[]`,
`avg_prompt_tokens`, `avg_gen_tokens`, `prompt_p50_tokens`, `prompt_p95_tokens`,
`prompt_max_tokens`, `prompt_max_is_bucket_edge`, `spec_accept_length`, `spec_accept_rate`,
`cache_hit_share`, `kv_peak`, `kv_capacity_tokens`, `context_len`, `evicted_tokens`,
`retracted_pct`, `prefill_time_share`, `gen_tokens`, `prompt_tokens`, `cached_tokens`,
`requests`, `tokens_per_joule`, `wh_per_mtok`, `wh_per_mtok_incl_idle`, `avg_watts_serving`,
`avg_watts`, `gate_requests`, `error_rate`, `rate_429`, `rate_503`) plus `quick`, `full`
(the newest COMPLETE bench scorecard of that profile, or null), `accuracy[]` (the newest per
dataset) and `last_quick_at`, `last_full_at`, `last_accuracy_at` (the last attempt, complete or
not). `null` = not measured, never zero.

# `GET /bench`, `POST /bench`
`GET`: `brief` (as `status.bench`) and `runs[]`, newest first: the scorecards (`run_id`,
`loadout_id`, `model`, `profile`, `started_at`, `ended_at`, `duration_s`, `status` = `running` /
`ok` / `aborted` / `failed` / `timeout`, `aborted` = why, `forced`, `under_load`, `background`,
`note`, `harness_version`,
`harness_commit`, `target`, `decode[]` = `{concurrency, context, per_user_tok_s, total_tok_s,
ttft_ms, errors, capacity_limited}`, `prefill[]` = `{tokens, tok_s, ttft_ms}`, `sanity[]` and
`needle[]` with `pass` and `detail`, `accept_length`, `accept_rate`, `kv_tokens`,
`tokens_per_joule`, `wh_per_mtok`, `avg_watts`, `accuracy[]` = `{dataset, score, n, correct,
wilson95_low, wilson95_high, file}`, `raw_dir`).

## What else was on the server (card #14, 2026-09-23)

Every scorecard says what the server was doing while it was taken, because the same loadout
measured alone and measured with two other people on it gives two different, both-correct
answers. Three fields carry it:

- `forced` - the idle gate was skipped (`--force` or `--under-load`).
- `under_load` - the run was asked to measure WITH other traffic, so the abort-on-traffic watch
  stood down for it. This is a request, not a finding: an `--under-load` run on a box that
  happened to stay quiet is still quiet and says so.
- `background` - what the safety poll actually saw, sampled at EVERY poll across the whole run
  (about every 2 s), never read once at the start:
  `{samples, polls_with_traffic, concurrent_avg, concurrent_max, source, queue_avg, queue_max,
  engine_tok_s}`. `concurrent_*` = requests in flight that were not the benchmark's;
  `source` = how they were counted, at its weakest over the run - `gateway user table` (a
  `/gate/health` v5.2+ user table, exact), `gateway lanes` (the engine's per-priority counters),
  `engine total less the bench's own` (no gateway: it can show traffic, it can never prove the
  absence of it), or `not measured`. `queue_*` is the ENGINE's queue, the benchmark's own
  requests included. `engine_tok_s` is the engine's whole output over the window, the
  benchmark's own generation included - the denominator that gives a foreign-request count a
  size, not a measure of the foreign load on its own. `null` = a run from before this existed.

`status.bench.last.load` carries the one-word verdict for the newest run: `quiet` (nothing else
touched the server for the whole run - the gold standard), `loaded` (other traffic shared it) or
`unknown` (nobody recorded it, or nothing could tell the benchmark's requests from anyone
else's). **`unknown` must never be read as `quiet`.** `lss loadouts`, `lss bench status`,
`lss model` and the live screen all print it next to the numbers it applies to.

KNOWN LIMIT, published rather than hidden: these are instantaneous gauges read every
`[bench] poll_secs`. A request shorter than that interval can begin and end between two polls
and be counted by neither. `samples` is the denominator, and the `engine total less the bench's
own` source is never accepted as proof of quiet for exactly this reason.

`POST /bench` `{"profile": "quick|full|accuracy|dry-run", "force": false, "under_load": false,
"note": "", "dataset": ""}`,
`POST /bench/cancel`, `POST /bench/compare-accuracy` `{"a": file, "b": file}`: accepted on a
loopback listener from a loopback peer only (403 otherwise). The answer is
`{v, ok, run_id, message, under_load}`; `202` = started, `409` = refused (busy, already running,
not set up), with the reason in `message`. `under_load` is what the COLLECTOR understood, echoed
back on the acceptance and on the refusal alike. `BenchRequest` is `#[serde(default)]` with no
`deny_unknown_fields`, so a collector older than the flag drops it silently and answers exactly
as it would have to a request that never carried it - the caller is told to `--force`, does, and
the run dies at the first foreign request. The echo is how a client tells that skew from a real
refusal; `lss` prints a named error and exits 2 when it asked for `--under-load` and the answer
does not echo it. (`lss` and `lss-collector` are two binaries: updating one does not update the
other, and this is the failure that shape produces.) **Under `--json` that error is an object, not
a sentence** (card #208 - it used to be neither, because the `--json` path returned above the
check and exited 0 in silence, which is the same silent drop aimed at the one caller that cannot
see it): `{"v": 1, "error": "collector_too_old_for_under_load", "detail": "<the sentence>",
"answer": <the collector's whole reply>}` on stdout, exit 2. The `error` code is stable and is
what a script should branch on; `answer` carries `run_id` and the rest, so nothing is traded away
for the error. It is one document, because two on stdout parse as neither. `force` skips the idle gate and leaves the abort-on-traffic watch
armed; `under_load` skips the gate AND stands the watch down, which is the only way to finish a
run on a server that is never idle. What that costs is stated below.

## Comparing two runs (card #14, 2026-09-23)

`lss compare A B` will NOT subtract one bench number from another when the two runs were taken
under materially different background load, and says why instead. The rule is derived, not
chosen: the comparison already declares a +-3 % run-to-run noise band and refuses to call
anything inside it a difference, so the same band is applied to the CAUSE. Under fair sharing,
one benchmark request gets `1/(1+n)` of the engine with `n` other requests in flight, so two runs
are comparable only while `|1/(1+na) - 1/(1+nb)| / max(...) <= 0.03`. At that band it tolerates
about 0.03 of one continuously-busy other request - three percent of one extra user - and
nothing more, which makes `quiet` against `loaded` incomparable in every case worth the name
(nothing against one steady other user is a 50 % difference in share, not a 3 % one). A run whose
background load was never recorded is comparable with nothing at all: an unmeasured difference is
not a small one.

In `lss compare --json` this appears as `load_warning` (the sentence, or `null`), `a.bench_load` /
`b.bench_load` (`quiet` / `loaded` / `unknown`) with `a.bench_load_detail` / `b.bench_load_detail`,
and `rows[].mark = "incomparable"` on every row whose `source` is `bench`. Those rows keep their
`a` and `b` values - the measurements were made and are still shown - but their `delta_pct` is
`null`. Rows measured from real traffic (`source: "live"`) are untouched: they never claimed to
have been measured alone.

# `POST /maintenance/start`, `POST /maintenance/stop`
Card #22, 2026-09-21: `lss maintenance start "reason" [--minutes N] | stop | status`. Like
`/bench`, accepted on a loopback listener from a loopback peer only (403 otherwise) - changing
alerting behaviour is only ever a decision made on the box itself.

`POST /maintenance/start` `{"reason": "gate v5.2 swap", "minutes": 30}`: `minutes` 0/absent =
60 (the default safety ceiling if `stop` is never called), clamped to at most 240. A blank
`reason` is refused with `400`. Starting again while already active REPLACES the window (its
reason/timer), it does not stack a second one. Answer: `{"ok": bool, "message": string,
"expires_at": int}` - `200` on success, `400` on a blank reason.

`POST /maintenance/stop`: closes the window early. `{"ok": false, "message": "no maintenance
window is open", "expires_at": 0}` (still `200`, not an error) when nothing was open.

While a window is open, restarts on `serve`/`gate` are relabelled `info`/"planned - …" on
`GET /rules`' alert history instead of `warn` (see `container_restart:serve`/`:gate` in
[RUNBOOK.md](RUNBOOK.md)); nothing else about the rule engine changes - a genuine unrelated
alert (Xid, thermal, an unplanned outage) still fires normally. The window itself is its own
`maintenance`-kind row in `status.incidents` (kept out of the uptime/downtime arithmetic exactly
like a `bench` window) and an `M` marker on the LOAD page's charts. A forgotten `stop`
auto-expires at `expires_at` - the very next poll after that closes it exactly as `stop` would.

# `GET /advice`
`windows[]` for `24h`, `7d`, `30d`: `stats` (every number the rules looked at; `null` = not
measured) and `findings[]` = `{rule, severity: fine|watch|act, window, sentence, value, unit,
points_at}`, most pressing first; `top` = the two findings the overview shows;
`thermal_exclude`. Every rule and threshold: [ADVICE.md](ADVICE.md).
