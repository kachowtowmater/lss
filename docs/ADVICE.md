# ADVICE - evidence for settings and upgrade decisions

`lss advice`, the ADVICE page (key `9`) and the ADVICE box on the overview answer one question:
**"is there anything I should change, and how do I know?"**

* It is **rule-based**. Every finding is one plain sentence with the number behind it, a
  severity and the setting or upgrade it points at. No model is asked for an opinion.
* It **never changes anything.** It is evidence; a person decides.
* It is summarised over three windows - the last **24 hours**, the **week** (7 days) and the last
  **30 days** - from the collector's 1-minute and 10-minute rollups, refreshed every 5 minutes.
  The overview shows the two most pressing findings of the week (of the last 24 hours until the
  collector has a day of data).
* A rule with no data says nothing. `null` is never turned into a zero.

| severity | meaning |
|---|---|
| `fine` | measured, and nothing needs doing. Still shown: it is the proof that you are fine. |
| `watch` | not a problem yet; look again next week, or before adding users. |
| `act` | this is costing you something now. The sentence says what to consider. |

Thresholds live in the collector's config under `[advice]` (defaults below). Restart the
collector after changing them.

## The rules

### `slots_saturated` - are all the seats taken?
*Measures:* the share of the time `running requests == max slots` (`max_running_requests`).
*Series:* `busy_slots` (0/1 per 5 s sample; its average is the share).

| | default | |
|---|---|---|
| `slots_full_watch_pct` | 5 | `watch` from this share of the window |
| `slots_full_act_pct` | 20 | `act` from here |

*Example:* "All 8 slots were busy 0.6% of the week - no capacity problem."
*Points at:* `--max-running-requests` (raise it only if `kv_pressure` says memory allows), or a
second server.

### `queue_wait` - did anyone wait in line?
*Measures:* the share of the time the engine's queue was not empty, and the p95 of SGLang's
`queue_time_seconds` histogram over the window. *Series:* `busy_wait`.

| | default | |
|---|---|---|
| `queue_watch_pct` | 2 | |
| `queue_act_pct` | 10 | |

*Example:* "Requests waited in line 14% of the time, p95 wait 9.0 s - consider more slots or a
second server."
*Points at:* more slots, or a second server. Read it together with `saturation_point`: if total
speed has already stopped growing, more slots only make everyone slower.

### `kv_pressure` - is the model's working memory (KV cache) full?
*Measures:* the peak and the p95 of KV token usage, the share of samples in which the engine
had **retracted** a request (`num_retracted_reqs > 0`: it ran out of KV, paused a request and
re-queued it), and the tokens evicted from the prefix cache (`evicted_tokens_total`).

| | default | |
|---|---|---|
| `kv_low_pct` | 30 | peak below this = "you could allow more simultaneous users" |
| `kv_watch_pct` | 80 | |
| `kv_act_pct` | 95 | any retraction is `act` whatever the peak |

*Examples:* "Memory (KV) never passed 30% - you could allow more simultaneous users." ·
"Memory (KV) ran out: the engine paused and re-queued requests 1.2% of the time …"
*Points at:* `--max-running-requests`, `--context-length`, `--mem-fraction-static`, or more
GPU memory.

### `evictions` - is it forgetting text it could have reused?
*Measures:* tokens the engine threw out of the KV cache to make room for new requests
(`evicted_tokens_total`), per hour of the window. Some eviction is normal housekeeping; a lot of it
means text that repeats (a system prompt, earlier turns of a conversation) is being read again
instead of coming from the cache, which costs reading time (prefill).

| threshold | default | |
|---|---|---|
| `evictions_watch_per_hour` | 1000000 | tokens per hour; at or above = `watch` |

*Example:* "Memory (KV) threw out 8.4M tokens of remembered text in the week to make room (about 50k per hour) - normal housekeeping at this rate."
*Points at:* `--mem-fraction-static` (higher), `--context-length` (lower, so more of the memory is cache), or more GPU memory.

### `prompt_sizes` - is the reserved context the size people use?
*Measures:* p50 / p95 / max of the real prompt lengths against the configured context
(`context_len`). The max is the gateway's figure when it logs one (the gateway ≥ v5.2);
otherwise it is the upper edge of the highest occupied bucket of SGLang's
`prompt_tokens_histogram`, worded "at most". That histogram also counts the monitor's own small
probes, so the rule says nothing until at least 100 requests that were not probes are behind it.

| | default | |
|---|---|---|
| `context_unused_pct` | 50 | longest prompt below this share of the context = `watch` |

*Example:* "Longest real prompt was 490k tokens of the 1M reserved."
*Points at:* `--context-length`. A smaller context frees KV memory for more users.

### `cache_reuse` - are repeated prompts being reused?
*Measures:* cached prompt tokens / all prompt tokens (`cached_tokens_total`).

| | default | |
|---|---|---|
| `cache_low_pct` | 20 | below = `watch` |

*Points at:* nothing to change by itself. A low share means prefill speed matters more for this
workload; a high share means long shared context (system prompts, agent history) is nearly free.

### `spec_decoding` - is the drafting shortcut paying off, also under load?
*Measures:* the speculative-decoding accept length **by concurrency level**, for the current
loadout (from the server's own metrics, never from a client).

| | default | |
|---|---|---|
| `spec_low_accept` | 1.5 | accept length below this with one user = `act` |
| `spec_drop_pct` | 25 | accept length falling by more than this from the quietest to the busiest level = `watch` |

*Points at:* `--speculative-num-draft-tokens` / `--speculative-num-steps`, the drafter itself.

### `time_split` - is the time going into reading or into writing?
*Measures:* request-seconds in SGLang's `prefill_forward` stage over request-seconds end to end
(`per_stage_req_latency_seconds`, `e2e_request_latency_seconds`). Absent when the engine does
not expose the stage metrics.

| | default | |
|---|---|---|
| `prefill_heavy_pct` | 60 | above = `watch` |

*Points at:* prefill speed: `--chunked-prefill-size`, more tensor parallelism / GPUs.

### `rejections` - is the gateway turning people away?
*Measures:* per lane, the 429 (too busy / rate limited) and 413 (prompt too large) answers in
the gateway's audit log, per day.

| | default | |
|---|---|---|
| `rejects_watch_per_day` | 5 | |
| `rejects_act_per_day` | 50 | |

*Points at:* that lane's limits in the gateway (requests per minute, concurrency, in-flight
token budget, prompt size), or capacity.

### `growth` - how fast is usage growing?
*Measures:* tokens per day, requests per day and the most users at once, the last 7 days against
the 7 before them. Says nothing until the earlier week has at least 3 days of data.

| | default | |
|---|---|---|
| `growth_watch_pct` | 50 | tokens per day up by more than this = `watch` |

*Points at:* capacity planning: every other finding will move with it.

### `energy` - what does a million tokens cost in electricity?
*Measures:* GPU energy while at least one request was running (5 s power samples, integrated)
per 1M generated tokens. Always `fine`: it is a number to compare loadouts and hardware with
(`lss compare` has the same figure per loadout).

### `throttling` - are the GPUs slowing themselves down?
*Measures:* per GPU, the share of the time a thermal or hardware-slowdown throttle reason was
active. **GPUs listed in `[rules] thermal_exclude` are measured and shown in the numbers, but
never named in the advice wording**: a card known to sit in a hot slot should not drown out a
new problem.

| | default | |
|---|---|---|
| `throttle_watch_pct` | 1 | |
| `throttle_act_pct` | 10 | |

*Points at:* airflow, fan curves, slot spacing - before any purchase.

### `saturation_point` - where does adding users stop adding speed?
*Measures:* the concurrency table of the current loadout (live samples bucketed by the number of
running requests, overridden by `lss bench` where it measured a level). The saturation point is
the first level that no higher level beats by more than the +-3% noise band. `watch` when that
point is below the number of slots allowed.

*Example:* "Total speed stops growing after 4 users."
*Points at:* `--max-running-requests` (beyond the saturation point extra users only slow the
others down); more total throughput needs faster or more GPUs.

### `targets` - is the service meeting the targets you set?
*Measures:* the `[targets]` you set in the collector's config (`ttft_p95_s`, `min_tok_s_per_user`,
`max_queue_wait_s`, `uptime_pct`) against what really happened in the window: the share of requests
whose first word came in time, the share of tokens written at least as fast as the per-user
minimum, the share of requests that waited less than the limit for a slot, and the share of the
time the server was up. A latency target counts as met when 95% of requests meet it; the uptime
target when the share of time up reaches `uptime_pct`. `fine` when every target is met (it names
the closest one), `watch` when one is missed, `act` when the worst miss is 10 points or more
(1 point for uptime).

*Example:* "Target missed in the week: 91% waited under 2 s for a slot (share of requests, goal 95%)."
*Points at:* first word late -> reading speed (`--chunked-prefill-size`, fewer long prompts at once,
faster GPUs); too slow per user -> `--max-running-requests` (lower) or faster GPUs; waiting for a
slot -> `--max-running-requests` or a second server; uptime -> 9 INCIDENTS shows what went down (and 8 ALERTS what fired).

## Reading it

`lss advice` prints the week; `lss advice --range 24h|30d` another window; `--json` the whole
document (`GET /advice`): every window's numbers (`stats`) and findings, so an agent can apply
its own thresholds.
