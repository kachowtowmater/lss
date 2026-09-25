//! The stored sample and the `/status` document (schema v1 - see docs/STATUS-JSON.md).
//! Additive changes keep `v`; renames or removals bump it. Every struct deserialises with
//! `#[serde(default)]` so an older `lss` keeps working against a newer collector.

use crate::docker::ContainerState;
use crate::gate::GateHealth;
use crate::gatelog::LogDelta;
use crate::gpu::GpuSample;
use crate::incidents::Incident;
use crate::probe::ProbeRecord;
use crate::prom::ServeMetrics;
use serde::{Deserialize, Serialize};

/// One 5-second poll, stored as compact JSON in `samples.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sample {
    pub ts: i64,
    pub serve_up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ServeMetrics>,
    /// false when nvidia-smi itself failed
    pub gpus_ok: bool,
    pub gpus: Vec<GpuSample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve_ct: Option<ContainerState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_ct: Option<ContainerState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateHealth>,
    #[serde(skip_serializing_if = "LogDelta::is_empty")]
    pub log: LogDelta,
    /// the users doing something right now (a gateway publishing /gate/health v5.2+), for the per-user series
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<crate::users::UserPoint>,
    /// the most requests the engine runs at once (0 = not known), for "all slots busy"
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub slots: u32,
    /// which engine answered (`sglang`, `vllm`, ...; "" = a sample from before adapters existed)
    #[serde(skip_serializing_if = "String::is_empty")]
    pub engine: String,
    /// the `engine::METRIC_FIELDS` keys this engine has no number for
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_reported: Vec<String>,
    /// no gateway is configured (so a missing `gate` is not an outage)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gate_absent: bool,
    /// `nvidia` | `amd` | `apple` | `none`; "" = nvidia (older samples)
    #[serde(skip_serializing_if = "String::is_empty")]
    pub gpu_source: String,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub v: u32,
    pub host: String,
    pub generated_at: i64,
    pub collector: CollectorInfo,
    pub serve: ServeStatus,
    pub gate: GateStatus,
    /// where the GPU numbers come from (added 2026-09-20): `nvidia | amd | apple | none`.
    /// `none` = no GPU tool on this machine: GPUS is hidden, nothing alerts
    pub gpu_source: String,
    pub gpus: Vec<GpuStatus>,
    pub lanes: Lanes,
    pub series: Series,
    pub incidents: Vec<Incident>,
    pub alerts: Vec<AlertRow>,
    pub firing: Vec<String>,
    pub probe: ProbeStatus,
    pub thresholds: Thresholds,
    /// who is on the server (added 2026-09-20; `available: false` with a gate older than v5.2)
    pub users: crate::users::UsersStatus,
    /// the two most pressing findings of `GET /advice` (added 2026-09-20; empty = nothing to say yet)
    pub advice_top: Vec<crate::advice::Finding>,
    /// the benchmark: running or not, and the last run (added 2026-09-20)
    pub bench: crate::bench::BenchBrief,
    /// `lss maintenance start|stop "reason"` (added #22, 2026-09-21): a planned window, active
    /// or not - never null, a fresh collector's default is simply `active: false`
    pub maintenance: crate::maintenance::MaintenanceState,
    /// what is being served, as a loadout (added 2026-09-20; null until the serve was seen up)
    pub loadout: Option<LoadoutBrief>,
    /// the owner's service targets over the last 24 h (added 2026-09-20; empty rows = none yet)
    pub targets: crate::targets::TargetsStatus,
    /// #75, 2026-09-22: electricity cost from the GPU watt samples already collected, priced by
    /// the owner's own time-of-use rate table. `None` = no `[rates] path` configured - never a
    /// guessed number. See `rates.rs`'s own doc comment for the whole design.
    pub cost: Option<CostStatus>,
    /// card #176 (the owner: "how much we are spending everyday, every week, every month or some
    /// kind of chart"): spend per day, and the day/week/month windows built from it. `None` when
    /// cost tracking is off, exactly like `cost` - never an empty set of zeroed windows.
    pub spending: Option<crate::rates::SpendingStatus>,
    /// #73, 2026-09-22: generated/prompt/cached tokens at hour/day/week/month, for page 1's
    /// TOKENS section. Always `Some` once deployed (each of its four fields is independently
    /// `Option`) - wrapped in `Option` here only so an older collector's document (which has no
    /// concept of this field at all) round-trips as `None`, the same convention as `cost`.
    pub tokens_by_window: Option<TokenWindows>,
    /// #74, 2026-09-22: what the sources the owner follows for serving recipes have published,
    /// and when - see `watch.rs`'s own doc comment for the whole design and its hard
    /// constraints. `None` = no `[watch] path` configured, never a guessed/empty list standing
    /// in for "off".
    pub watch: Option<crate::watch::WatchStatus>,
}

/// One rate table's live and "since local midnight" numbers - `[rates] path` on `/status`.
/// Every figure here is delivery+generation only; `fixed_usd_per_day` (never folded into any
/// other field here) and `unresolved_usd_per_kwh` (never applied to any other field here) are
/// what a screen must show alongside these numbers so a marginal cost never reads as a bill.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CostStatus {
    pub rate_name: String,
    /// card #298: where the rate came from - `CA avg (EIA 2026-06)`, `entered by hand
    /// (2026-09-24)` - from `rates.toml`'s `source`. Empty (and left out of the JSON) when the
    /// file does not say; a screen then shows nothing rather than a guess.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rate_source: String,
    /// the tariff/plan's own effective date (e.g. "2026-01-01") - a stale table should be
    /// visible on screen, not silently trusted forever
    pub effective_date: String,
    /// true = a single flat number, never a real time-of-use schedule - a screen must label this
    /// plainly, the same instruction the owner gave for the whole feature
    pub is_flat: bool,
    /// the $/kWh and the period name (e.g. "summer_on_peak", or "flat") that apply RIGHT NOW
    pub current_usd_per_kwh: Option<f64>,
    pub current_period: String,
    /// dollars per hour AT THE CURRENT DRAW - GPU watts right now x `current_usd_per_kwh`.
    /// `None` when there is no GPU power reading, not 0
    pub live_usd_per_hour: Option<f64>,
    /// since local midnight, from the stored watt samples, each priced at its own timestamp.
    /// `None` = nothing could be priced yet today (no samples, or a rate table with no match) -
    /// never a fabricated 0
    pub today_kwh: Option<f64>,
    pub today_usd: Option<f64>,
    /// the figure the whole card exists for: dollars per million tokens today, generated and
    /// prompt separately - `None` when there is nothing to divide by yet
    pub today_usd_per_million_generated_tokens: Option<f64>,
    pub today_usd_per_million_prompt_tokens: Option<f64>,
    /// card #174 (the owner: "the cost per 1m token is off" - the old figure divided ALL today's
    /// energy by generated tokens only, inflating the headline by roughly the ratio of uncached
    /// prefill to generated - about 10x, measured on a live hour). The PRIMARY figure: today's
    /// WORK energy (excludes standby, below) over (uncached prefill + generated) tokens - the
    /// tokens the engine actually computed, cache hits excluded because a cache hit costs KV
    /// residency, not compute. `None` when there is nothing to divide by yet.
    pub today_usd_per_million_real_work_tokens: Option<f64>,
    /// today's energy/cost while the engine had NOTHING running (idle/standby power draw),
    /// separated from `today_usd`/`today_kwh` above so a quiet day does not look expensive and a
    /// busy day does not look cheap in the per-token figure. `None` = no idle samples today yet.
    pub today_standby_kwh: Option<f64>,
    pub today_standby_usd: Option<f64>,
    /// The utility's fixed daily charge from the rate table - shown, NEVER added into
    /// `today_usd` or any other figure on this struct. A marginal per-hour or per-day cost must
    /// never pretend to be a bill.
    pub fixed_usd_per_day: Option<f64>,
    /// an amount that MAY belong on top of every `usd_per_kwh` here but is not settled (the
    /// tariff's Fixed Recovery Charge + MCAM, unverified against an actual bill) - shown as a
    /// stated uncertainty, never silently applied either way
    pub unresolved_usd_per_kwh: Option<f64>,
    /// card #73: the ROLLING last 24 hours (NOT "today" / since local midnight - `today_usd`
    /// above is that) - priced from stored rollup buckets, not raw watt samples, because a
    /// rolling window can reach past what `raw_hours` still retains. Exists so the USERS
    /// section's per-user dollar split lines up with the SAME rolling-24h window its token
    /// counts already use (the gate's own `_24h` counters) - splitting `today_usd` by a
    /// rolling-24h token share would silently mix two different windows. `None` = no priced
    /// bucket in the last 24h (no rate table, or the collector has not stored one yet).
    pub last_24h_kwh: Option<f64>,
    pub last_24h_usd: Option<f64>,
    /// #102, 2026-09-22 (verifier): how many of the seconds SINCE THE EARLIEST PRICED SAMPLE
    /// `today_kwh`/`today_usd` actually cover - compare against "now minus local midnight" to
    /// know whether `today` is FULL coverage or a collector that has only been up part of the
    /// day. `None` whenever `today_kwh` is also `None` (nothing priced at all yet).
    pub today_covered_secs: Option<i64>,
    /// the same idea for `last_24h_kwh`/`last_24h_usd` - compare against 86,400.
    pub last_24h_covered_secs: Option<i64>,
}

/// The current loadout in brief: `GET /loadouts` has the rest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutBrief {
    pub id: String,
    pub model: String,
    pub image_tag: String,
    pub first_seen: i64,
    pub runs: u64,
    /// the safe launch flags (`crate::loadout::safe_flags`): `tp 4 · ctx 1048576 · quant fp4 ·
    /// slots 8 · spec NEXTN ...`. Added #51, 2026-09-21, for page 1's LOADOUT line - already
    /// computed for every loadout identity, just not carried this far before.
    pub flags: String,
}

/// #51, 2026-09-21 (panel: lianmin-zheng, woosuk-kwon, hamel-husain): "we already lost hours to
/// a queue that was the gateway's fault while the engine was idle." One printed word, not an
/// inference the reader has to make by comparing two boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmitVerdict {
    /// nothing is refusing work
    Ok,
    /// the gateway is holding requests back (waiters queued) while the engine has a free slot -
    /// the exact scenario the panel cited: it looks like the engine's fault and is not
    GatewayLimited,
    /// every slot the engine will run at once is running: the engine itself is the constraint
    EngineFull,
    /// KV is exhausted enough that a new request is more likely to wait on memory than a slot
    KvLimited,
    /// the engine is not up at all - not one of the panel's four words, but reporting
    /// ENGINE-FULL (or anything else) while it is actually DOWN would be a lie, and this whole
    /// verdict exists so the reader never has to infer that across two boxes
    Down,
}

impl AdmitVerdict {
    pub fn label(self) -> &'static str {
        match self {
            AdmitVerdict::Ok => "OK",
            AdmitVerdict::GatewayLimited => "GATEWAY-LIMITED",
            AdmitVerdict::EngineFull => "ENGINE-FULL",
            AdmitVerdict::KvLimited => "KV-LIMITED",
            AdmitVerdict::Down => "DOWN",
        }
    }
}

/// Above this KV usage, with a slot still free, a new request is more likely to be waiting on
/// memory than on a slot - "KV-LIMITED" earns its own word rather than reading as healthy.
pub const KV_LIMITED_THRESHOLD: f64 = 0.97;

impl Status {
    /// C1 baseline, from `thresholds` (or `probe` when the collector predates `thresholds`).
    pub fn c1_baseline(&self) -> Option<f64> {
        self.thresholds.c1_baseline_tok_s.or(self.probe.baseline_tok_s)
    }

    /// The decode rate under which the `c1_decode` rule counts a probe as low.
    pub fn c1_floor(&self) -> Option<f64> {
        self.thresholds.c1_floor_tok_s.or_else(|| self.c1_baseline().map(|b| b * self.thresholds.c1_ratio))
    }

    /// One computed word answering "can it take the next request, and if not, who is refusing -
    /// me or the engine" (Woosuk). Checked in this order: DOWN first - every other word implies
    /// the engine is up and merely constrained, which would be a lie while it is not running at
    /// all; then a gateway holding requests back WHILE the engine has room (the surprising case,
    /// and the one that has already cost real hours); only once the engine itself has no free
    /// slot does that become the answer; KV is checked last, since a full engine or a gate-side
    /// queue is a more immediate answer than memory pressure that has not yet actually blocked a
    /// request.
    ///
    /// Ship-gate verifier, 2026-09-21: the guard used to also require `gate.up && !gate.absent`,
    /// so a gateway that reads DOWN or unreachable - with its last-known lane data still showing
    /// waiters - produced the single word `OK`. Waiters are evidence of something refusing work
    /// whatever the gate's own health endpoint says about itself, so the guard no longer looks at
    /// gate health at all: only whether the engine has room decides GATEWAY-LIMITED vs ENGINE-FULL.
    pub fn admission_verdict(&self) -> AdmitVerdict {
        if !self.serve.up {
            return AdmitVerdict::Down;
        }
        let waiters = self.lanes.public.waiters + self.lanes.trusted.waiters;
        let engine_full = self.serve.slots > 0 && self.serve.running >= f64::from(self.serve.slots);
        if waiters > 0 && !engine_full {
            return AdmitVerdict::GatewayLimited;
        }
        if engine_full {
            return AdmitVerdict::EngineFull;
        }
        if self.serve.kv_usage >= KV_LIMITED_THRESHOLD {
            return AdmitVerdict::KvLimited;
        }
        AdmitVerdict::Ok
    }
}

/// The alert thresholds in force on the collector, so a client colours its screen with the
/// same numbers the rule engine alerts on instead of carrying its own copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// C1 alert floor = `c1_ratio` x `c1_baseline_tok_s`
    pub c1_ratio: f64,
    pub c1_consecutive: u32,
    /// null while the baseline is still being learned
    pub c1_baseline_tok_s: Option<f64>,
    pub c1_floor_tok_s: Option<f64>,
    /// a probe with a slower first token is invalid (`slow_ttft`)
    pub c1_max_ttft_s: f64,
    pub queue_reqs: f64,
    pub thermal_temp_c: f64,
    /// the same threshold in Fahrenheit (added 2026-09-20; JSON keeps Celsius and adds `_f`)
    pub thermal_temp_f: f64,
    /// C1 counts as stale once the newest VALID reading is this many seconds old (card #47,
    /// 2026-09-21: `c1_stale_probe_intervals` x the probe's own interval) - the client uses this,
    /// not its own copy of the multiplier, so it never disagrees with the rule that alerts on it
    pub c1_stale_secs: i64,
}

impl Thresholds {
    pub fn new(rules: &crate::config::RulesConfig, c1_baseline: Option<f64>, probe_interval_secs: i64) -> Self {
        Self {
            c1_ratio: rules.c1_ratio,
            c1_consecutive: rules.c1_consecutive,
            c1_baseline_tok_s: c1_baseline,
            c1_floor_tok_s: c1_baseline.map(|b| (b * rules.c1_ratio * 10.0).round() / 10.0),
            c1_max_ttft_s: rules.c1_max_ttft_s,
            queue_reqs: rules.queue_reqs,
            thermal_temp_c: rules.thermal_temp_c,
            thermal_temp_f: crate::units::c_to_f_round(rules.thermal_temp_c),
            c1_stale_secs: probe_interval_secs.max(1) * i64::from(rules.c1_stale_probe_intervals.max(1)),
        }
    }
}

/// A collector from before `thresholds` existed: fall back to the built-in rule defaults.
impl Default for Thresholds {
    fn default() -> Self {
        Self::new(&crate::config::RulesConfig::default(), None, crate::config::ProbeConfig::default().interval_secs)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorInfo {
    pub version: String,
    pub started_at: i64,
    pub poll_secs: u64,
    pub last_sample_ts: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeStatus {
    pub up: bool,
    /// card #331: WHY the serve is down, in words, when the engine told us - today: it refused
    /// the collector's requests for want of (or with a wrong) API key. Absent = no known reason
    /// (not answering, loading, ...), and on every UP document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_reason: Option<String>,
    pub model: Option<String>,
    pub container: Option<String>,
    pub container_status: Option<String>,
    pub started_at: Option<i64>,
    pub uptime_s: Option<i64>,
    pub restart_count: u64,
    pub restarts_today: u32,
    pub down_since: Option<i64>,
    pub running: f64,
    pub slots: u32,
    pub queue: f64,
    pub decode_tok_s: f64,
    pub kv_usage: f64,
    pub kv_used_tokens: f64,
    pub kv_max_tokens: f64,
    pub ttft_avg_ms_10m: Option<f64>,
    pub itl_avg_ms_10m: Option<f64>,
    pub queue_time_avg_ms_10m: Option<f64>,
    pub spec_accept_length: f64,
    pub spec_accept_rate: f64,
    pub cache_hit_rate: f64,
    pub prompt_tokens_total: f64,
    pub generation_tokens_total: f64,
    pub requests_total: f64,
    /// p50/p90/p99/avg of TTFT, e2e, ITL and queue time over the last 10 min, from histogram
    /// bucket deltas (added 2026-09-19; absent from older collectors and until a scrape pair exists)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<crate::hist::LatencyNow>,
    /// added 2026-09-20: prompt tokens served from the prefix cache, and the configured context
    pub cached_tokens_total: f64,
    pub context_len: f64,
    /// READING SPEED (prefill), added 2026-09-20: prompt tokens the GPUs read per second while
    /// reading, over the last 10 minutes; null = nothing was read in that time
    pub prefill_tok_s: Option<f64>,
    /// the same over the whole life of the current loadout (what to show while it is idle)
    pub prefill_tok_s_typical: Option<f64>,
    /// share (0..=1) of the prompt tokens of the last 10 minutes that came from the prefix cache
    pub cached_share_10m: Option<f64>,
    /// requests whose prompt is being read right now
    pub prefill_inflight: f64,
    /// which engine serves it (added 2026-09-20): `sglang | vllm | llamacpp | ollama | lmstudio |
    /// tgi | openai`; "" from an older collector (= SGLang)
    pub engine: String,
    /// `decode_tok_s` is the monitor's own C1 probe, because this engine publishes no speed
    /// (added 2026-09-20): the screen says so rather than passing it off as the engine's
    pub decode_tok_s_from_probe: bool,
    /// numbers this engine does not publish, as `engine::METRIC_FIELDS` keys. A number listed
    /// here is 0 in this document and must be shown as `n/a (not reported by <engine>)`.
    pub not_reported: Vec<String>,
    /// #51, 2026-09-21 (the panel, unanimous): KV % alone reads as headroom while eviction is
    /// heavy - tokens/hour thrown out of the prefix cache over the last 10 minutes, the pairing
    /// KV free-in-tokens needs to not be misleading on its own. `None` = not enough comparable
    /// samples in the window yet (not zero - a real zero means nothing was evicted).
    pub evicted_tok_per_hour_10m: Option<f64>,
    /// card #279 (card #274's finding): requests the engine PUSHED OUT of the running batch to
    /// make room, per hour over the same 10-minute window as `evicted_tok_per_hour_10m`. This is
    /// the other half of what "KV 0% used" hides - eviction throws away cached prefix, preemption
    /// throws away work already in flight, and SYNTHESIS' addendum asked for both on SERVING.
    /// The counter exists in the engine scrape (`num_retracted_reqs` -> `ServeMetrics::retracted`,
    /// SGLang's retractions / vLLM's preemptions) and simply never reached this schema, so the
    /// SERVING page had to print "preempted —".
    /// `None` = not enough comparable samples in the window, or an engine that does not report it
    /// - NEVER 0, because a real 0 means "nothing was preempted", which is the opposite fact.
    pub preempted_per_hour_10m: Option<f64>,
    /// #51, 2026-09-21 (the panel, unprompted): "that is what the Xid 8 hang looked like from
    /// the outside, and nothing on the page shows it." `None` = no token generated anywhere in
    /// the retained history (up to an hour) - not "just started", genuinely silent that long.
    pub secs_since_last_token: Option<i64>,
    /// #51, 2026-09-21: prompt / cached / generated / requests over the last hour, for the WORK
    /// box (the panel's 24h/all-time figures come from other places - 24h needs a DB rollup this
    /// does not attempt yet; all-time is the `_total` counters above). `None` = fewer than two
    /// comparable samples in the last hour.
    pub work_1h: Option<WorkWindow>,
    /// Ship-gate verifier, 2026-09-21: NOT part of the collector's JSON (`#[serde(skip)]` - a
    /// collector that sets this would be lying about its own vocabulary). Set by
    /// `client::parse` from raw JSON key presence, because `evicted_tok_per_hour_10m` /
    /// `secs_since_last_token` / `work_1h` being absent from an old collector and being present-
    /// but-`null` on a new one both deserialise to the same `None` - the one thing that tells
    /// them apart is whether the KEY existed in the body at all, which typed deserialisation
    /// already throws away. Without this, "not deployed yet" and "deployed, computed nothing"
    /// print the same sentence, and one of those sentences is a lie about a field that is simply
    /// not there ("last token: none in the last hour" while the token counter is climbing).
    #[serde(skip)]
    pub page1_fields_deployed: bool,
    /// #50, 2026-09-21: decode speed measured from REAL traffic at the concurrency level running
    /// right now (`crate::loadout::CurveRow` - `running`, `samples`, `tok_s` = the aggregate at
    /// that level, `per_request_tok_s` = what each of them gets), passively observed
    /// (`Curve::observe`, fed every sample, no probe, never needs the engine idle) - shown beside
    /// C1, never presented as equal to it: C1 is the clean-room reference (one request, nothing
    /// else running); this is what real traffic actually gets at whatever load is on the box
    /// right now. `None` before this exact concurrency level has been observed, or before a
    /// loadout is known at all.
    pub live_decode: Option<crate::loadout::CurveRow>,
    /// #118, 2026-09-22 (verifier-2): the SGLang priority label the gate stamps per lane
    /// (`public_priority`/`trusted_priority` in the collector's own config) - these are not
    /// uncomputable, they simply were not carried to a client field before this. `""` only when
    /// the collector predates this field (an old collector's document has no key for it at all;
    /// `client::parse` never fabricates a value it did not receive).
    pub public_priority: String,
    pub trusted_priority: String,
}

/// One window's worth of `WORK`: prompt tokens in, of which from the cache, generated, requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkWindow {
    pub prompt: f64,
    pub cached: f64,
    pub generated: f64,
    pub requests: f64,
}

/// card #73's TOKENS section: the same `WorkWindow` shape, at four fixed windows. `hour`/`day`/
/// `week` all come from the 1-minute rollup (`retention_days`, default 14); `month` from the
/// 10-minute rollup (`rollup_10m_days`, default 90) - all four the SAME kind of source, so they
/// are monotonic by construction (#100, 2026-09-22: `hour` used to come from `work_1h`'s raw
/// samples, a different source, which could read higher than `day` on a low-uptime collector).
/// Each is `None` on its own - no rollup for that metric stored yet at all (an old collector, or
/// one that has not been up long enough) - never a fabricated 0 standing in for "nothing
/// happened" vs "nothing recorded".
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenWindows {
    pub hour: Option<WorkWindow>,
    /// #102, 2026-09-22 (verifier): seconds of REAL data behind `hour` - compare against 3,600
    /// to know whether it is full coverage or a collector that has not been up that long yet.
    /// `None` only when `hour` is also `None`.
    pub hour_covered_secs: Option<i64>,
    pub day: Option<WorkWindow>,
    /// compare against 86,400
    pub day_covered_secs: Option<i64>,
    pub week: Option<WorkWindow>,
    /// compare against 604,800
    pub week_covered_secs: Option<i64>,
    pub month: Option<WorkWindow>,
    /// compare against 2,592,000
    pub month_covered_secs: Option<i64>,
}

impl ServeStatus {
    /// `Some("n/a (not reported by vLLM)")` when the engine does not publish `key`.
    pub fn na(&self, key: &str) -> Option<String> {
        self.is_na(key).then(|| format!("n/a (not reported by {})", self.engine_label()))
    }
    pub fn is_na(&self, key: &str) -> bool {
        self.not_reported.iter().any(|k| k == key)
    }
    pub fn engine_label(&self) -> &'static str {
        crate::engine::EngineKind::parse(&self.engine).map_or("the engine", crate::engine::EngineKind::label)
    }
    /// What to say when `prefill_tok_s` AND `prefill_tok_s_typical` are both null: two different
    /// facts wore the same words (E4, 2026-09-20). `prompt_tokens_total` (all-time, cache hits
    /// included) beating `cached_tokens_total` means the engine really did compute prompt tokens -
    /// a rate just is not confident yet (`READ_PEAK_MIN_TOKENS`) - which is not "nothing read".
    pub fn no_reading_reason(&self) -> &'static str {
        if self.prompt_tokens_total > self.cached_tokens_total { "not enough reading yet to report a rate" } else { "no prompt read yet" }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateStatus {
    pub up: bool,
    /// no gateway is configured (added 2026-09-20): LANES / USERS / GATEWAY say so; `up` is
    /// true then, so nothing reads as an outage
    pub absent: bool,
    pub version: Option<String>,
    pub upstream_ok: bool,
    pub container_status: Option<String>,
    pub started_at: Option<i64>,
    pub restart_count: u64,
    pub down_since: Option<i64>,
    /// card #279: how long the gate's in-flight CHARGED tokens would take to drain at the engine's
    /// recent prefill rate (the gateway's own `/gate/health` `shadow.seconds_to_drain_charged`,
    /// published since v5.10 by card #154). The SERVING page wanted it beside the queue and had to
    /// say "drain on GATEWAY" because nothing carried it into /status.
    /// `None` = no gateway, a gateway older than v5.10, or the gate could not compute it (no
    /// prefill rate yet) - never 0, which would read as "drains instantly".
    pub seconds_to_drain_charged: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuStatus {
    #[serde(flatten)]
    pub sample: GpuSample,
    /// card #197: this card's utilisation as a MEDIAN over the last `UTIL_MEDIAN_POLLS` polls
    /// (30 s at the default 5 s poll), the input the TP-skew reading is scored on. `sample.util_pct`
    /// beside it stays the instantaneous reading. `None` = this collector has not held that many
    /// polls for the card yet (or predates the field) - which is a reason to say nothing about
    /// skew, never a reason to fall back to one sample.
    pub util_pct_med: Option<f64>,
    /// `temp_c` / `mem_temp_c` in Fahrenheit (added 2026-09-20; Celsius stays the source)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_f: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_temp_f: Option<f64>,
    /// decoded `throttle_mask`: hw_thermal, sw_thermal, hw_slowdown, hw_power_brake, sw_power_cap
    pub throttle: Vec<String>,
    /// true for GPUs in `thermal_exclude` (digest only, never a thermal alert)
    pub thermal_excluded: bool,
    /// link / ECC / remap / limits, read once a minute (added 2026-09-19)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::gpu::GpuHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lanes {
    pub public: LaneStatus,
    pub trusted: LaneStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneStatus {
    pub running: f64,
    pub queued: f64,
    pub inflight_tokens: u64,
    pub waiters: u64,
    pub admitted: u64,
    pub rejected_413: u64,
    pub rejected_429: u64,
    pub client_closed: u64,
    pub upstream_down: u64,
    pub max_duration: u64,
    pub requests_10m: u64,
    pub codes_10m: CodeClasses,
    pub top_keys_60m: Vec<KeyCount>,
    /// the collector's own C1 probe requests, already SUBTRACTED from the numbers above
    pub probes: LaneProbes,
    /// #31, 2026-09-21: the in-flight token budget `inflight_tokens` is judged against.
    /// `None` on public (no in-flight budget there) and on a gate too old to publish it.
    pub budget_tokens: Option<u64>,
    /// the per-request prompt-token cap (413 above this); `None` on an older gate.
    pub max_prompt_tokens: Option<u64>,
}

/// The collector's own probe traffic in a lane (it only ever uses the trusted one).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneProbes {
    /// probes the gate admitted since it started - taken out of `admitted`
    pub admitted: u64,
    /// taken out of `requests_10m` / `codes_10m`
    pub requests_10m: u64,
    /// taken out of the `(no key)` row of `top_keys_60m`
    pub requests_60m: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeClasses {
    #[serde(rename = "2xx")]
    pub c2xx: u64,
    #[serde(rename = "4xx")]
    pub c4xx: u64,
    #[serde(rename = "5xx")]
    pub c5xx: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyCount {
    pub key: String,
    pub count: u64,
}

/// Fixed-step series covering the last hour, oldest first. `null` = no data in that bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Series {
    pub step_s: i64,
    pub start_ts: i64,
    pub points: usize,
    pub decode_tok_s: Vec<Option<f64>>,
    /// READING SPEED (prefill), added 2026-09-20: prompt tokens read per second of each bucket
    /// (cache hits are not reading; 0 = nothing was read)
    pub prefill_tok_s: Vec<Option<f64>>,
    pub running: Vec<Option<f64>>,
    pub queue_public: Vec<Option<f64>>,
    pub queue_trusted: Vec<Option<f64>>,
    /// #69, 2026-09-22 (the owner: "what the hell does this graph do in lanes ... i dont even know
    /// what this tells me that is useful" - about `queue_public`/`queue_trusted` above, which is
    /// flat at zero almost always because requests essentially never queue AT THE GATEWAY; they
    /// are either admitted or HELD on the trusted lane's in-flight token budget, which is what
    /// actually varies and drives a decision - the reader can see the budget filling BEFORE the
    /// rejections start. `None` = no gate, or a gate older than card #31 (no `budget_tokens`
    /// published) - never a guessed 0. Public has no in-flight budget (only a request cap), so
    /// this is trusted-lane only, same as `LaneAdmission::budget_tokens`'s own doc comment.
    pub budget_used_frac: Vec<Option<f64>>,
    pub waiters_public: Vec<Option<f64>>,
    pub waiters_trusted: Vec<Option<f64>>,
    pub kv_usage: Vec<Option<f64>>,
    /// one inner series per GPU index
    pub gpu_temp_c: Vec<Vec<Option<f64>>>,
    // card #281 (redesign R2c): the page-1 trend grid's other series (DESIGN §S5), same step
    // and grid as the fields above. Additive: an older collector sends none of them, and a
    // client reads that as "no data" (an empty chart), never as zeros. `null` in a bucket = no
    // data there - never a guessed 0.
    /// TTFT p99 in ms, from the engine's own histogram: the collector fills it from its stored
    /// minute rollups (`ttft_p99_ms`, the `/series` metric), so each minute's p99 sits in both
    /// of that minute's buckets. All `null` when the engine publishes no TTFT histogram.
    pub ttft_p99_ms: Vec<Option<f64>>,
    /// ITL (time between tokens) p99 in ms - same source and resolution as `ttft_p99_ms`.
    pub itl_p99_ms: Vec<Option<f64>>,
    /// prefix-cache hit share of the prompt tokens that arrived in the bucket (cached / prompt,
    /// counter growth - page 1's own "of prompt tokens" denominator), 0..1. `null` = no prompt
    /// tokens arrived, or the engine does not report the counters.
    pub prefix_hit: Vec<Option<f64>>,
    /// speculative-decoding acceptance rate, 0..1, the bucket's mean. `null` = the engine runs
    /// no speculative decoding (it reports no `spec` field), never 0.
    pub spec_accept_rate: Vec<Option<f64>>,
    /// requests the gateway REFUSED in the bucket, all lanes: 413 (too large) + 429 (too busy),
    /// from the gate's audit log. `null` = no gateway log read in that bucket.
    pub refused: Vec<Option<f64>>,
    /// total GPU power in W, all cards, the bucket's max. `null` = no GPU power reading.
    pub gpu_power_w: Vec<Option<f64>>,
    /// electricity cost in $/h: `gpu_power_w` priced at the rate in force at the bucket's own
    /// time (the collector's rates table). All `null` when no rates.toml is configured.
    pub usd_per_hour: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertRow {
    pub id: i64,
    pub ts: i64,
    pub rule: String,
    pub severity: String,
    pub message: String,
    pub recovered: bool,
    /// did the alert sink report at least one leg delivered
    pub delivered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeStatus {
    pub enabled: bool,
    pub interval_s: i64,
    pub baseline_tok_s: Option<f64>,
    /// "config", "learned" or "learning N/M"
    pub baseline_source: String,
    /// the newest VALID completed probe: THE C1 reading. A probe that collided with traffic
    /// never replaces it, however recent
    pub last_ok: Option<ProbeRecord>,
    /// probes newer than `last_ok` that were not a reading (invalid, skipped, failed)
    pub invalid_skipped: u32,
    pub history: Vec<ProbeRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reading_reason_tells_apart_never_read_from_not_enough_yet() {
        // E4, 2026-09-20: llama.cpp served real requests (prompt tokens computed, not just cache
        // hits) but the rate was not confident yet - the old text ("no prompt read yet") lied.
        let mut sv = ServeStatus::default();
        assert_eq!(sv.no_reading_reason(), "no prompt read yet", "nothing served at all: the old wording is still true");
        sv.cached_tokens_total = 50.0;
        sv.prompt_tokens_total = 50.0;
        assert_eq!(sv.no_reading_reason(), "no prompt read yet", "every prompt token so far was a cache hit: still nothing was actually read");
        sv.prompt_tokens_total = 300.0;
        assert_eq!(sv.no_reading_reason(), "not enough reading yet to report a rate", "some tokens were computed, not cached: reading did happen");
    }

    fn admit_fixture() -> Status {
        Status { serve: ServeStatus { up: true, slots: 8, running: 2.0, kv_usage: 0.2, ..Default::default() }, gate: GateStatus { up: true, absent: false, ..Default::default() }, ..Default::default() }
    }

    #[test]
    fn admission_verdict_down_beats_every_other_word() {
        // every other verdict implies the engine is up and merely constrained - showing
        // ENGINE-FULL (or anything else) while it is actually down would be a lie
        let mut s = admit_fixture();
        s.serve.up = false;
        s.serve.running = 8.0; // would read ENGINE-FULL if `up` were not checked first
        s.lanes.trusted.waiters = 5; // would read GATEWAY-LIMITED
        assert_eq!(s.admission_verdict(), AdmitVerdict::Down);
    }

    #[test]
    fn admission_verdict_ok_when_nothing_is_refusing_work() {
        assert_eq!(admit_fixture().admission_verdict(), AdmitVerdict::Ok);
    }

    #[test]
    fn admission_verdict_gateway_limited_when_the_gate_holds_requests_back_with_a_free_slot() {
        // #51, 2026-09-21: the exact scenario the panel cited - hours lost to a queue that was
        // the gateway's fault while the engine sat idle. running (2) < slots (8): the engine has room.
        let mut s = admit_fixture();
        s.lanes.trusted.waiters = 3;
        assert_eq!(s.admission_verdict(), AdmitVerdict::GatewayLimited);
        // the public lane counts too
        let mut s = admit_fixture();
        s.lanes.public.waiters = 1;
        assert_eq!(s.admission_verdict(), AdmitVerdict::GatewayLimited);
    }

    #[test]
    fn admission_verdict_engine_full_wins_over_gateway_limited() {
        // waiters alone would read as GATEWAY-LIMITED, but the engine itself has no free slot -
        // that is the more binding fact, and gateway waiters are an expected consequence of it,
        // not a separate problem to chase.
        let mut s = admit_fixture();
        s.serve.running = 8.0;
        s.lanes.trusted.waiters = 5;
        assert_eq!(s.admission_verdict(), AdmitVerdict::EngineFull);
    }

    #[test]
    fn admission_verdict_kv_limited_only_once_the_engine_and_gateway_are_not_the_answer() {
        let mut s = admit_fixture();
        s.serve.kv_usage = 0.99;
        assert_eq!(s.admission_verdict(), AdmitVerdict::KvLimited);
        // just under the threshold: still OK, not a false alarm
        let mut s = admit_fixture();
        s.serve.kv_usage = KV_LIMITED_THRESHOLD - 0.001;
        assert_eq!(s.admission_verdict(), AdmitVerdict::Ok);
    }

    #[test]
    fn admission_verdict_trusts_waiters_even_when_the_gate_itself_reads_down_or_absent() {
        // ship-gate verifier, 2026-09-21: this used to assert Ok for both - the exact bug. A
        // gateway that reads DOWN or absent, with waiters queued, is not a healthy "OK": waiters
        // are evidence of something refusing work whatever the gate's own health says about itself.
        let mut s = admit_fixture();
        s.gate.absent = true;
        s.lanes.trusted.waiters = 4;
        assert_eq!(s.admission_verdict(), AdmitVerdict::GatewayLimited, "waiters + gate absent -> not OK");
        let mut s = admit_fixture();
        s.gate.up = false;
        s.lanes.trusted.waiters = 4;
        assert_eq!(s.admission_verdict(), AdmitVerdict::GatewayLimited, "waiters + gate down -> not OK");
    }

    #[test]
    fn admission_verdict_zero_slots_never_reads_as_engine_full() {
        // slots = 0 means "not known yet" (per the field's own doc comment), not "no capacity" -
        // running (0) >= slots (0) must not false-positive as ENGINE-FULL
        let mut s = admit_fixture();
        s.serve.slots = 0;
        s.serve.running = 0.0;
        assert_eq!(s.admission_verdict(), AdmitVerdict::Ok);
    }
}
