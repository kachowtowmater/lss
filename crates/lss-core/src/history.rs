//! The in-memory hour of samples and the `/status` document builder.

use crate::gatelog::LaneLog;
use crate::gpu::throttle_flags;
use crate::incidents::Incident;
use crate::model::*;
use crate::probe::ProbeRecord;
use crate::STATUS_SCHEMA_VERSION;
use std::collections::VecDeque;

pub const HISTORY_SECS: i64 = 3600;
pub const SERIES_STEP_SECS: i64 = 30;
pub const SERIES_POINTS: usize = (HISTORY_SECS / SERIES_STEP_SECS) as usize;
const RATE_WINDOW_SECS: i64 = 600;

/// How many polls of GPU utilisation the collector takes the median of before publishing it
/// (card #197). COUNTED IN POLLS, not seconds, because the thing being rejected is SAMPLING
/// NOISE and the noise was measured in polls.
///
/// The measurement, read-only over this box's own stored samples (17,445 usable polls / 24.25 h,
/// 4 cards, 5 s polls): the shipped instantaneous `gpu.skew` could headline on 7.94% of polls,
/// and its hit RUNS were median 1 poll, p90 2 - a different card dipping each poll, which is a
/// sampling phase, not a straggler. A median of N samples cannot be moved by fewer than N/2
/// outliers, so rejecting a 2-poll dip needs N >= 5; 6 is the first even count past it, and on
/// this box 6 polls is 30 s - the same "sustained" the card's own evidence used. Replayed with
/// the median per card over 6 polls and everything else unchanged, the same rule can headline on
/// 3.16% of polls (20 s: 3.68%, 40 s: 2.73%, 60 s: 2.30% - the curve flattens after 30 s while
/// the detection lag keeps growing, since a window only registers a fault halfway through it).
pub const UTIL_MEDIAN_POLLS: usize = 6;

#[derive(Debug, Default)]
pub struct History {
    samples: VecDeque<Sample>,
}

impl History {
    pub fn push(&mut self, s: Sample) {
        let cutoff = s.ts - HISTORY_SECS - SERIES_STEP_SECS;
        self.samples.push_back(s);
        while self.samples.front().is_some_and(|f| f.ts < cutoff) {
            self.samples.pop_front();
        }
    }

    pub fn latest(&self) -> Option<&Sample> {
        self.samples.back()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter()
    }

    fn since(&self, ts: i64) -> impl Iterator<Item = &Sample> {
        self.samples.iter().filter(move |s| s.ts > ts)
    }

    /// The MEDIAN utilisation of one card over the last `UTIL_MEDIAN_POLLS` polls, or `None`
    /// when this history does not hold that many readings for it (card #197). Medians, not
    /// means: one card sampled between two kernels must not drag the window with it.
    pub fn gpu_util_median(&self, index: u32) -> Option<f64> {
        let mut v: Vec<f64> = self
            .samples
            .iter()
            .rev()
            .take(UTIL_MEDIAN_POLLS)
            .map(|s| s.gpus.iter().find(|g| g.index == index).and_then(|g| g.util_pct))
            .collect::<Option<Vec<f64>>>()?;
        if v.len() < UTIL_MEDIAN_POLLS {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let n = v.len();
        Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
    }

    fn lane_log(&self, since: i64, pick: impl Fn(&Sample) -> &LaneLog) -> LaneLog {
        let mut acc = LaneLog::default();
        for s in self.since(since) {
            acc.merge(pick(s));
        }
        acc
    }

    /// Average of a (sum, count) histogram pair over the rate window, in milliseconds.
    fn windowed_avg_ms(&self, now: i64, pick: impl Fn(&crate::prom::ServeMetrics) -> (f64, f64)) -> Option<f64> {
        let cur = self.samples.iter().rev().find_map(|s| s.metrics.as_ref()).map(&pick)?;
        let old = self
            .samples
            .iter()
            .filter(|s| s.ts >= now - RATE_WINDOW_SECS)
            .find_map(|s| s.metrics.as_ref())
            .map(&pick)
            .unwrap_or((0.0, 0.0));
        // counters restart from zero with the engine: then the current totals ARE the window
        let (ds, dc) = if cur.1 >= old.1 && cur.0 >= old.0 { (cur.0 - old.0, cur.1 - old.1) } else { cur };
        (dc > 0.0).then(|| ds / dc * 1000.0)
    }

    /// (prompt tokens the GPUs read, request-seconds spent reading them, prompt tokens, cached
    /// prompt tokens) over the rate window. Steps across an engine restart add nothing.
    fn reading_window(&self, now: i64) -> (f64, f64, f64, f64) {
        let mut acc = (0.0, 0.0, 0.0, 0.0);
        let mut prev: Option<&crate::prom::ServeMetrics> = None;
        for s in self.samples.iter().filter(|s| s.ts >= now - RATE_WINDOW_SECS) {
            let Some(m) = s.metrics.as_ref() else {
                prev = None;
                continue;
            };
            if let Some(pm) = prev {
                let same = m.comparable(pm);
                let grown = |a: f64, b: f64| if same && a >= b { a - b } else { 0.0 };
                acc.0 += m.computed_prompt_tokens_since(pm);
                acc.1 += grown(m.prefill_forward_sum, pm.prefill_forward_sum);
                acc.2 += grown(m.prompt_tokens_total, pm.prompt_tokens_total);
                acc.3 += grown(m.cached_tokens_total, pm.cached_tokens_total);
            }
            prev = Some(m);
        }
        acc
    }

    /// Sum of the growth of one monotonic counter (`pick`) over the last `span_secs`, ending
    /// `now`. `None` = fewer than two comparable samples in the span - nothing to compute from
    /// yet, not zero. Steps across an engine restart (`comparable` fails) add nothing, same as
    /// `reading_window`. #51, 2026-09-21: shared by the eviction-rate and WORK (1h token
    /// totals) fields the page-1 redesign panel asked for. `pub` since #54, 2026-09-21: the
    /// collector's `bench` idle gate reads `generation_tokens_total`'s growth through this same
    /// method, the same evidence the probe's own validity check (#45) uses - request/status
    /// counts miss what matters, only the token counter proves the engine actually did nothing.
    pub fn counter_window(&self, now: i64, span_secs: i64, pick: impl Fn(&crate::prom::ServeMetrics) -> f64) -> Option<f64> {
        let mut total = 0.0;
        let mut seen = false;
        let mut prev: Option<&crate::prom::ServeMetrics> = None;
        for s in self.samples.iter().filter(|s| s.ts >= now - span_secs) {
            let Some(m) = s.metrics.as_ref() else {
                prev = None;
                continue;
            };
            if let Some(pm) = prev {
                if m.comparable(pm) && pick(m) >= pick(pm) {
                    total += pick(m) - pick(pm);
                    seen = true;
                }
            }
            prev = Some(m);
        }
        seen.then_some(total)
    }

    /// Seconds since `generation_tokens_total` last grew, anywhere in the retained history (up
    /// to `HISTORY_SECS`). `None` = no growth seen in what is retained - the engine has not
    /// written a token in over an hour (or there are not yet two comparable samples). #51,
    /// 2026-09-21 (the panel, unprompted): "that is what the Xid 8 hang looked like from the
    /// outside, and nothing on the page shows it."
    fn secs_since_last_token(&self, now: i64) -> Option<i64> {
        let mut prev: Option<&Sample> = None;
        let mut last_growth_ts: Option<i64> = None;
        for s in &self.samples {
            if let (Some(p), Some(m)) = (prev, s.metrics.as_ref()) {
                if let Some(pm) = p.metrics.as_ref() {
                    if m.comparable(pm) && m.generation_tokens_total > pm.generation_tokens_total {
                        last_growth_ts = Some(s.ts);
                    }
                }
            }
            prev = Some(s);
        }
        last_growth_ts.map(|ts| (now - ts).max(0))
    }

    /// card #283: total GPU power, all cards, each 30 s bucket's MEAN, on exactly `series`' grid -
    /// what the collector prices the `$/h` trend from (energy over the bucket, like the live "$/h
    /// now" figure). `series` publishes the bucket's MAX for the power chart, where a spike is the
    /// point; a price is a rate, and one spiky sample must not set a whole bucket's cost. Not in
    /// `/status` itself: the collector prices the series before it serializes.
    pub fn power_mean_w(&self, now: i64) -> Vec<Option<f64>> {
        let end = now - now.rem_euclid(SERIES_STEP_SECS) + SERIES_STEP_SECS;
        let start = end - HISTORY_SECS;
        let mut power = vec![Bucket::default(); SERIES_POINTS];
        for s in self.samples.iter().filter(|s| s.ts >= start && s.ts < end) {
            let watts: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
            if !watts.is_empty() {
                power[((s.ts - start) / SERIES_STEP_SECS) as usize].add(watts.iter().sum());
            }
        }
        power.iter().map(Bucket::mean).collect()
    }

    fn series(&self, now: i64) -> Series {
        let end = now - now.rem_euclid(SERIES_STEP_SECS) + SERIES_STEP_SECS;
        let start = end - HISTORY_SECS;
        let gpu_count = self.samples.iter().map(|s| s.gpus.iter().map(|g| g.index as usize + 1).max().unwrap_or(0)).max().unwrap_or(0);

        let mut decode = vec![Bucket::default(); SERIES_POINTS];
        let mut running = vec![Bucket::default(); SERIES_POINTS];
        let mut q_pub = vec![Bucket::default(); SERIES_POINTS];
        let mut q_tru = vec![Bucket::default(); SERIES_POINTS];
        // #69: a bucket that never saw a sample with a gate publishing `budget_tokens` (card
        // #31) stays `None` after `Bucket::max` below - never a guessed 0 for "no gate" or "gate
        // too old".
        let mut budget_used = vec![Bucket::default(); SERIES_POINTS];
        let mut w_pub = vec![Bucket::default(); SERIES_POINTS];
        let mut w_tru = vec![Bucket::default(); SERIES_POINTS];
        let mut kv = vec![Bucket::default(); SERIES_POINTS];
        let mut temps = vec![vec![Bucket::default(); SERIES_POINTS]; gpu_count];
        // prompt tokens really read in each bucket (cache hits are not reading); None = no sample
        let mut read: Vec<Option<f64>> = vec![None; SERIES_POINTS];
        // card #281: (prompt tokens, cached prompt tokens) that arrived in each bucket - the
        // prefix-hit share is their ratio; None = no comparable counter step in the bucket
        let mut hit: Vec<Option<(f64, f64)>> = vec![None; SERIES_POINTS];
        let mut spec = vec![Bucket::default(); SERIES_POINTS];
        let mut refused: Vec<Option<f64>> = vec![None; SERIES_POINTS];
        let mut power = vec![Bucket::default(); SERIES_POINTS];
        let mut prev: Option<&crate::prom::ServeMetrics> = None;

        for s in &self.samples {
            if s.ts < start || s.ts >= end {
                prev = s.metrics.as_ref();
                continue;
            }
            let i = ((s.ts - start) / SERIES_STEP_SECS) as usize;
            if let Some(m) = &s.metrics {
                let tokens = prev.map_or(0.0, |pm| m.computed_prompt_tokens_since(pm));
                *read[i].get_or_insert(0.0) += tokens;
                // card #281: an engine that reports no token counters, or no speculative
                // decoding, contributes nothing - its bucket stays null, never a 0
                let reports = |key: &str| !s.not_reported.iter().any(|k| k == key);
                if let Some(pm) = prev.filter(|pm| m.comparable(pm) && reports("tokens") && reports("cache_hit")) {
                    let (dp, dc) = (m.prompt_tokens_total - pm.prompt_tokens_total, m.cached_tokens_total - pm.cached_tokens_total);
                    if dp >= 0.0 && dc >= 0.0 {
                        let acc = hit[i].get_or_insert((0.0, 0.0));
                        acc.0 += dp;
                        acc.1 += dc;
                    }
                }
                if reports("spec") {
                    spec[i].add(m.spec_accept_rate);
                }
                prev = Some(m);
                decode[i].add(m.gen_throughput);
                running[i].add(m.running);
                q_pub[i].add(m.queue_public);
                q_tru[i].add(m.queue_trusted);
                kv[i].add(m.kv_usage());
            }
            // #69: the gate's own lane admission, not the engine's Prometheus metrics - a
            // sample with no gate (or an old one with no `budget_tokens`) contributes nothing to
            // this bucket, same as `metrics: None` above skips decode/running/queue.
            if let Some(g) = &s.gate {
                if let Some(cap) = g.trusted.budget_tokens.filter(|c| *c > 0) {
                    budget_used[i].add(g.trusted.inflight_tokens as f64 / cap as f64);
                }
                w_pub[i].add(g.public.waiters as f64);
                w_tru[i].add(g.trusted.waiters as f64);
            }
            for g in &s.gpus {
                if let (Some(t), Some(row)) = (g.temp_c, temps.get_mut(g.index as usize)) {
                    row[i].add(t);
                }
            }
            // card #281: all cards' power at this instant, when at least one card reported it
            let watts: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
            if !watts.is_empty() {
                power[i].add(watts.iter().sum());
            }
            // card #281: the gate's policy refusals (413 too large, 429 too busy), all lanes. A
            // sample with a gate read contributes (possibly 0); no gate = the bucket stays null.
            if s.gate.is_some() {
                let n: u64 = [&s.log.public, &s.log.trusted].iter().map(|l| l.by_status.get(&413).copied().unwrap_or(0) + l.by_status.get(&429).copied().unwrap_or(0)).sum();
                *refused[i].get_or_insert(0.0) += n as f64;
            }
        }
        Series {
            step_s: SERIES_STEP_SECS,
            start_ts: start,
            points: SERIES_POINTS,
            // decode is a rate: the mean is the honest number. The rest are levels where a
            // spike is the whole point, so a bucket reports its max.
            decode_tok_s: decode.iter().map(Bucket::mean).collect(),
            prefill_tok_s: read.iter().map(|r| r.map(|t| (t / SERIES_STEP_SECS as f64).round())).collect(),
            running: running.iter().map(Bucket::max).collect(),
            queue_public: q_pub.iter().map(Bucket::max).collect(),
            queue_trusted: q_tru.iter().map(Bucket::max).collect(),
            budget_used_frac: budget_used.iter().map(Bucket::max).collect(),
            waiters_public: w_pub.iter().map(Bucket::max).collect(),
            waiters_trusted: w_tru.iter().map(Bucket::max).collect(),
            kv_usage: kv.iter().map(Bucket::max).collect(),
            gpu_temp_c: temps.iter().map(|row| row.iter().map(Bucket::max).collect()).collect(),
            // card #281: the histogram p99s and the priced $/h are the collector's to fill (it
            // holds the histogram rollups and the rates table) - null until it does
            ttft_p99_ms: vec![None; SERIES_POINTS],
            itl_p99_ms: vec![None; SERIES_POINTS],
            prefix_hit: hit.iter().map(|h| h.and_then(|(p, c)| (p > 0.0).then(|| round3((c / p).clamp(0.0, 1.0))))).collect(),
            spec_accept_rate: spec.iter().map(Bucket::mean).collect(),
            refused,
            gpu_power_w: power.iter().map(Bucket::max).collect(),
            usd_per_hour: vec![None; SERIES_POINTS],
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Bucket {
    sum: f64,
    max: f64,
    n: u32,
}

impl Bucket {
    fn add(&mut self, v: f64) {
        if !v.is_finite() {
            return;
        }
        self.max = if self.n == 0 { v } else { self.max.max(v) };
        self.sum += v;
        self.n += 1;
    }
    fn mean(&self) -> Option<f64> {
        (self.n > 0).then(|| round3(self.sum / f64::from(self.n)))
    }
    fn max(&self) -> Option<f64> {
        (self.n > 0).then(|| round3(self.max))
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub struct StatusInputs<'a> {
    pub now: i64,
    pub host: &'a str,
    pub collector_version: &'a str,
    pub collector_started_at: i64,
    pub poll_secs: u64,
    pub slots: u32,
    pub history: &'a History,
    /// newest first
    pub incidents: &'a [Incident],
    /// newest first
    pub alerts: &'a [AlertRow],
    /// newest first
    pub probes: &'a [ProbeRecord],
    /// the newest valid completed probe when it is older than everything in `probes`
    pub last_valid_probe: Option<&'a ProbeRecord>,
    pub firing: Vec<String>,
    pub serve_down_since: Option<i64>,
    pub gate_down_since: Option<i64>,
    pub restarts_today: u32,
    pub thermal_exclude: &'a [u32],
    pub probe_enabled: bool,
    pub probe_interval_s: i64,
    pub c1_baseline: Option<f64>,
    pub c1_baseline_source: String,
    pub thresholds: Thresholds,
    /// the collector's own probes the gate admitted since the GATE started (the life of its
    /// `admitted` counter); the 10/60 minute shares are worked out from `probes`
    pub probes_admitted_since_gate_start: u64,
    /// histogram percentiles of the last ten minutes; None = none yet
    pub latency: Option<crate::hist::LatencyNow>,
    /// the newest slow health read of every GPU (may be empty)
    pub gpu_health: &'a [crate::gpu::GpuHealth],
    pub user_aliases: &'a [crate::users::UserAlias],
    /// address -> host name from `tailscale status` (empty unless the lookup is switched on)
    pub tailscale: &'a std::collections::BTreeMap<String, String>,
    /// the collector's own probe requests, to keep them out of the user table
    pub own_traffic: crate::users::OwnTraffic<'a>,
    /// what the overview shows of `GET /advice`, `GET /bench` and the current loadout
    pub advice_top: Vec<crate::advice::Finding>,
    pub bench: crate::bench::BenchBrief,
    /// #22, 2026-09-21: `lss maintenance start|stop "reason"`'s live state
    pub maintenance: crate::maintenance::MaintenanceState,
    pub loadout: Option<crate::model::LoadoutBrief>,
    /// the current loadout's long-run reading speed (shown while nothing is being read)
    pub prefill_typical: Option<f64>,
    /// #50, 2026-09-21: decode speed from real traffic at the concurrency level running right
    /// now (`Loadouts::live_speed`) - see `ServeStatus::live_decode`'s own doc comment
    pub live_decode: Option<crate::loadout::CurveRow>,
    /// #118, 2026-09-22: `cfg.public_priority`/`cfg.trusted_priority`, passed straight through -
    /// see `ServeStatus::public_priority`'s own doc comment.
    pub public_priority: String,
    pub trusted_priority: String,
    pub targets: crate::targets::TargetsStatus,
    /// the gate's log merged over the last 24 h (each user's own experience); None = not read
    pub user_log_24h: Option<&'a crate::gatelog::LogDelta>,
    /// #75, 2026-09-22: electricity cost, already computed (`cost_run::compute` on the collector
    /// side) - passed straight through, the same pattern as `bench`/`maintenance`/`targets`
    pub cost: Option<crate::model::CostStatus>,
    /// card #176: the spend-over-time block, built by the collector from the 10-minute rollup
    pub spending: Option<crate::rates::SpendingStatus>,
    /// #73, 2026-09-22: TOKENS section's hour/day/week/month windows, already computed
    /// (`tokens_run::compute`, a rollup query the collector's own DB - all four share the same
    /// source since #100, so they are monotonic by construction; see tokens_run.rs's own doc
    /// comment for why `hour` no longer comes from `work_1h`).
    pub tokens_hour: Option<crate::model::WorkWindow>,
    pub tokens_day: Option<crate::model::WorkWindow>,
    pub tokens_week: Option<crate::model::WorkWindow>,
    pub tokens_month: Option<crate::model::WorkWindow>,
    /// #102, 2026-09-22 (verifier): real seconds of data behind each window above - see
    /// `TokenWindows`'s own doc comment.
    pub tokens_hour_covered_secs: Option<i64>,
    pub tokens_day_covered_secs: Option<i64>,
    pub tokens_week_covered_secs: Option<i64>,
    pub tokens_month_covered_secs: Option<i64>,
    /// #74, 2026-09-22: already read back from `[watch] path` (`watch_run::Watcher::poll` on the
    /// collector side) - passed straight through, the same pattern as `cost`.
    pub watch: Option<crate::watch::WatchStatus>,
}

/// The gate's default key label for trusted traffic, which is where the probe shows up.
const NO_KEY: &str = "(no key)";

/// Takes the collector's own probe requests out of a lane, and says how many it took.
fn subtract_probes(lane: &mut LaneStatus, probes: &[ProbeRecord], now: i64, admitted_since_gate_start: u64) {
    let mut share = LaneProbes { admitted: admitted_since_gate_start.min(lane.admitted), ..Default::default() };
    for p in probes {
        let Some(code) = p.gate_logged_status() else { continue };
        if p.ts > now - HISTORY_SECS && p.ts <= now {
            share.requests_60m += 1;
        }
        if p.ts > now - RATE_WINDOW_SECS && p.ts <= now {
            share.requests_10m += 1;
            let class = match code / 100 {
                2 => &mut lane.codes_10m.c2xx,
                4 => &mut lane.codes_10m.c4xx,
                5 => &mut lane.codes_10m.c5xx,
                _ => continue,
            };
            *class = class.saturating_sub(1);
        }
    }
    // The gate logs a request when it ENDS and the probe is booked a poll later, so a window
    // edge can hold one without the other: never subtract more than is there.
    share.requests_10m = share.requests_10m.min(lane.requests_10m);
    lane.requests_10m -= share.requests_10m;
    lane.admitted -= share.admitted;
    if let Some(row) = lane.top_keys_60m.iter_mut().find(|k| k.key == NO_KEY) {
        share.requests_60m = share.requests_60m.min(row.count);
        row.count -= share.requests_60m;
    } else {
        share.requests_60m = 0;
    }
    lane.top_keys_60m.retain(|k| k.count > 0);
    lane.top_keys_60m.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    lane.probes = share;
}

pub fn build_status(i: &StatusInputs) -> Status {
    let latest = i.history.latest();
    let m = latest.and_then(|s| s.metrics.clone()).unwrap_or_default();
    let serve_ct = latest.and_then(|s| s.serve_ct.clone());
    let gate_ct = latest.and_then(|s| s.gate_ct.clone());
    let gate = latest.and_then(|s| s.gate.clone());
    let serve_up = latest.is_some_and(|s| s.serve_up);

    let (read_tokens, read_secs, prompt_10m, cached_10m) = i.history.reading_window(i.now);
    let evicted_tok_per_hour_10m = i.history.counter_window(i.now, RATE_WINDOW_SECS, |m| m.evicted_tokens_total).map(|tok| round3(tok / (RATE_WINDOW_SECS as f64 / 3600.0)));
    // card #279: the same window and the same arithmetic as the eviction rate above, deliberately -
    // the two are read side by side on SERVING, and a reader comparing them must not be comparing
    // two different windows. `counter_window` already returns None rather than 0 when there are
    // too few comparable samples, and skips a counter that went backwards (an engine restart).
    // ... but ONLY when the engine actually publishes the counter. `to_serve_metrics` turns an
    // absent number into 0, so on an engine without it the window sees a flat 0 and this would
    // publish `Some(0.0)` - "nothing was preempted" - for "we have no idea". `None` instead, and
    // `serve.not_reported` carries "preemptions" so the screen can say which engine is silent.
    let reports_preemptions = !latest.is_some_and(|s| s.not_reported.iter().any(|k| k == "preemptions"));
    let preempted_per_hour_10m = reports_preemptions
        .then(|| i.history.counter_window(i.now, RATE_WINDOW_SECS, |m| m.retracted).map(|n| round3(n / (RATE_WINDOW_SECS as f64 / 3600.0))))
        .flatten();
    let work_1h = match (
        i.history.counter_window(i.now, HISTORY_SECS, |m| m.prompt_tokens_total),
        i.history.counter_window(i.now, HISTORY_SECS, |m| m.cached_tokens_total),
        i.history.counter_window(i.now, HISTORY_SECS, |m| m.generation_tokens_total),
        i.history.counter_window(i.now, HISTORY_SECS, |m| m.requests_total),
    ) {
        (Some(prompt), Some(cached), Some(generated), Some(requests)) => Some(WorkWindow { prompt, cached, generated, requests }),
        _ => None,
    };
    let serve = ServeStatus {
        // card #331: filled by the collector, which knows the scrape's own error
        down_reason: None,
        prefill_tok_s: (read_tokens >= 500.0 && read_secs > 0.05).then(|| (read_tokens / read_secs).round()),
        prefill_tok_s_typical: i.prefill_typical,
        cached_share_10m: (prompt_10m > 0.0).then(|| round3((cached_10m / prompt_10m).clamp(0.0, 1.0))),
        prefill_inflight: m.prefill_inflight_reqs,
        engine: latest.map(|s| s.engine.clone()).unwrap_or_default(),
        not_reported: latest.map(|s| s.not_reported.clone()).unwrap_or_default(),
        decode_tok_s_from_probe: false,
        up: serve_up,
        model: i.history.samples.iter().rev().find_map(|s| s.model.clone()),
        container: serve_ct.as_ref().map(|c| c.name.clone()),
        container_status: serve_ct.as_ref().map(|c| c.status.clone()),
        started_at: serve_ct.as_ref().map(|c| c.started_at).filter(|t| *t > 0),
        uptime_s: serve_ct.as_ref().filter(|c| c.running() && c.started_at > 0 && serve_up).map(|c| (i.now - c.started_at).max(0)),
        restart_count: serve_ct.as_ref().map_or(0, |c| c.restart_count),
        restarts_today: i.restarts_today,
        down_since: i.serve_down_since,
        running: m.running,
        slots: i.slots,
        queue: m.queue,
        decode_tok_s: m.gen_throughput,
        kv_usage: round3(m.kv_usage()),
        kv_used_tokens: m.kv_used_tokens,
        kv_max_tokens: m.max_total_tokens,
        ttft_avg_ms_10m: i.history.windowed_avg_ms(i.now, |m| (m.ttft_sum, m.ttft_count)).map(round3),
        itl_avg_ms_10m: i.history.windowed_avg_ms(i.now, |m| (m.itl_sum, m.itl_count)).map(round3),
        queue_time_avg_ms_10m: i.history.windowed_avg_ms(i.now, |m| (m.queue_time_sum, m.queue_time_count)).map(round3),
        spec_accept_length: m.spec_accept_length,
        spec_accept_rate: m.spec_accept_rate,
        cache_hit_rate: m.cache_hit_rate,
        prompt_tokens_total: m.prompt_tokens_total,
        generation_tokens_total: m.generation_tokens_total,
        requests_total: m.requests_total,
        latency: i.latency.clone(),
        cached_tokens_total: m.cached_tokens_total,
        context_len: m.context_len,
        evicted_tok_per_hour_10m,
        preempted_per_hour_10m,
        secs_since_last_token: i.history.secs_since_last_token(i.now),
        work_1h,
        // the collector never sets this itself - it is a client-only fact about the JSON it
        // receives, computed by `client::parse` (see the field's own doc comment)
        page1_fields_deployed: false,
        live_decode: i.live_decode.clone(),
        public_priority: i.public_priority.clone(),
        trusted_priority: i.trusted_priority.clone(),
    };

    let gate_absent = latest.is_some_and(|s| s.gate_absent);
    let gate_status = GateStatus {
        // no gateway configured is not "down": nothing turns red, the screen says so instead
        up: gate.is_some() || gate_absent,
        absent: gate_absent,
        version: gate.as_ref().map(|g| g.version.clone()),
        upstream_ok: gate.as_ref().is_some_and(|g| g.upstream_ok),
        container_status: gate_ct.as_ref().map(|c| c.status.clone()),
        started_at: gate_ct.as_ref().map(|c| c.started_at).filter(|t| *t > 0),
        restart_count: gate_ct.as_ref().map_or(0, |c| c.restart_count),
        down_since: i.gate_down_since,
        // card #279: straight through from the gate's own shadow block - the collector measures
        // nothing here, it carries what the gateway already published (card #154).
        seconds_to_drain_charged: gate.as_ref().and_then(|g| g.shadow.as_ref()).and_then(|sh| sh.seconds_to_drain_charged),
    };

    let gpus = latest
        .map(|s| {
            s.gpus
                .iter()
                .map(|g| GpuStatus {
                    sample: g.clone(),
                    // card #197: the SUSTAINED utilisation, published beside the instantaneous
                    // one rather than replacing it - the TP-skew reading needs a number that a
                    // single NVML sample cannot move, and only the collector has the samples to
                    // build one from (see `UTIL_MEDIAN_POLLS`).
                    util_pct_med: i.history.gpu_util_median(g.index),
                    temp_f: g.temp_c.map(crate::units::c_to_f_round),
                    mem_temp_f: g.mem_temp_c.map(crate::units::c_to_f_round),
                    throttle: throttle_flags(g.throttle_mask).into_iter().map(String::from).collect(),
                    thermal_excluded: i.thermal_exclude.contains(&g.index),
                    health: i.gpu_health.iter().find(|h| h.index == g.index).cloned(),
                })
                .collect()
        })
        .unwrap_or_default();

    let lane = |running: f64, queued: f64, adm: Option<&crate::gate::LaneAdmission>, pick: fn(&Sample) -> &LaneLog| {
        let adm = adm.cloned().unwrap_or_default();
        let ten = i.history.lane_log(i.now - RATE_WINDOW_SECS, pick);
        let hour = i.history.lane_log(i.now - HISTORY_SECS, pick);
        let (c2xx, c4xx, c5xx) = ten.classes();
        let mut keys: Vec<KeyCount> = hour.by_key.into_iter().map(|(key, count)| KeyCount { key, count }).collect();
        keys.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        keys.truncate(6); // one spare: the probe may empty the "(no key)" row
        LaneStatus {
            running,
            queued,
            inflight_tokens: adm.inflight_tokens,
            waiters: adm.waiters,
            admitted: adm.admitted,
            rejected_413: adm.rejected_413,
            rejected_429: adm.rejected_429,
            client_closed: adm.client_closed,
            upstream_down: adm.upstream_down,
            max_duration: adm.max_duration,
            requests_10m: ten.requests,
            codes_10m: CodeClasses { c2xx, c4xx, c5xx },
            top_keys_60m: keys,
            probes: LaneProbes::default(),
            budget_tokens: adm.budget_tokens,
            max_prompt_tokens: adm.max_prompt_tokens,
        }
    };
    let public = lane(m.running_public, m.queue_public, gate.as_ref().map(|g| &g.public), |s| &s.log.public);
    let mut trusted = lane(m.running_trusted, m.queue_trusted, gate.as_ref().map(|g| &g.trusted), |s| &s.log.trusted);
    // the probe goes through the gate's TRUSTED port: that lane must not count the monitor as a user
    subtract_probes(&mut trusted, i.probes, i.now, i.probes_admitted_since_gate_start);
    let mut lanes = Lanes { public, trusted };
    lanes.public.top_keys_60m.truncate(5);
    lanes.trusted.top_keys_60m.truncate(5);

    let probe_history: Vec<ProbeRecord> = i.probes.to_vec();
    let last_ok = probe_history.iter().find(|p| p.is_reading()).or(i.last_valid_probe).cloned();
    // ENGINES.md promises Ollama / LM Studio / a plain OpenAI-compatible server a writing speed
    // "from the probe". Where the engine reports none, the monitor's own newest reading IS the
    // number - said as what it is, never passed off as the engine's own (2026-09-20, E1).
    let mut serve = serve;
    if serve.not_reported.iter().any(|k| k == "decode_tok_s") {
        if let Some(v) = last_ok.as_ref().and_then(|p| p.decode_tok_s) {
            serve.decode_tok_s = v;
            serve.decode_tok_s_from_probe = true;
            serve.not_reported.retain(|k| k != "decode_tok_s");
        }
    }
    Status {
        v: STATUS_SCHEMA_VERSION,
        host: i.host.to_string(),
        generated_at: i.now,
        collector: CollectorInfo {
            version: i.collector_version.to_string(),
            started_at: i.collector_started_at,
            poll_secs: i.poll_secs,
            last_sample_ts: latest.map(|s| s.ts),
        },
        lanes,
        serve,
        gate: gate_status,
        gpu_source: latest.map(|s| if s.gpu_source.is_empty() { "nvidia".to_string() } else { s.gpu_source.clone() }).unwrap_or_else(|| "nvidia".to_string()),
        gpus,
        series: i.history.series(i.now),
        incidents: i.incidents.to_vec(),
        alerts: i.alerts.to_vec(),
        firing: i.firing.clone(),
        probe: ProbeStatus {
            enabled: i.probe_enabled,
            interval_s: i.probe_interval_s,
            baseline_tok_s: i.c1_baseline,
            baseline_source: i.c1_baseline_source.clone(),
            last_ok,
            invalid_skipped: probe_history.iter().take_while(|p| !p.is_reading()).count() as u32,
            history: probe_history,
        },
        thresholds: i.thresholds.clone(),
        users: {
            let mut users = crate::users::build_users(gate.as_ref(), i.user_aliases, i.tailscale, &i.own_traffic, i.now);
            if let Some(log) = i.user_log_24h {
                crate::users::attach_experience(&mut users, log, i.own_traffic.probe_user);
            }
            users
        },
        advice_top: i.advice_top.clone(),
        bench: i.bench.clone(),
        maintenance: i.maintenance.clone(),
        loadout: i.loadout.clone(),
        targets: i.targets.clone(),
        cost: i.cost.clone(),
        spending: i.spending.clone(),
        tokens_by_window: Some(crate::model::TokenWindows {
            hour: i.tokens_hour,
            hour_covered_secs: i.tokens_hour_covered_secs,
            day: i.tokens_day,
            day_covered_secs: i.tokens_day_covered_secs,
            week: i.tokens_week,
            week_covered_secs: i.tokens_week_covered_secs,
            month: i.tokens_month,
            month_covered_secs: i.tokens_month_covered_secs,
        }),
        watch: i.watch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prom::ServeMetrics;

    /// card #281: one sample on the series grid, with prompt/cached counters, a spec rate, two
    /// GPUs' power and (optionally) a gate whose log carries 413/429 refusals.
    fn trend_sample(ts: i64, prompt: f64, cached: f64, spec: f64, watts: [f64; 2], refusals: Option<(u64, u64)>) -> Sample {
        let metrics = ServeMetrics { prompt_tokens_total: prompt, cached_tokens_total: cached, spec_accept_rate: spec, ..Default::default() };
        let gpus = watts.iter().enumerate().map(|(i, w)| crate::gpu::GpuSample { index: i as u32, power_w: Some(*w), ..Default::default() }).collect();
        let mut s = Sample { ts, serve_up: true, metrics: Some(metrics), gpus_ok: true, gpus, ..Default::default() };
        if let Some((r413, r429)) = refusals {
            s.gate = Some(Default::default());
            s.log.public.by_status.insert(413, r413);
            s.log.trusted.by_status.insert(429, r429);
            s.log.trusted.by_status.insert(200, 50); // served, not refused
        }
        s
    }

    #[test]
    fn the_trend_series_carry_prefix_hit_spec_accept_refusals_and_power_per_bucket() {
        // card #281: bucket k of the series covers [start + 30k, start + 30k + 30)
        let now: i64 = 36_000;
        let end = now - now.rem_euclid(SERIES_STEP_SECS) + SERIES_STEP_SECS;
        let start = end - HISTORY_SECS;
        let at = |k: i64, off: i64| start + k * SERIES_STEP_SECS + off;
        let mut h = History::default();
        h.push(trend_sample(at(100, 0), 10_000.0, 9_000.0, 0.70, [60.0, 80.0], Some((0, 0))));
        // bucket 100: +1000 prompt tokens, +950 of them cached -> 95%; spec mean 0.65; power max 150
        h.push(trend_sample(at(100, 10), 10_600.0, 9_560.0, 0.60, [70.0, 80.0], Some((1, 0))));
        h.push(trend_sample(at(100, 20), 11_000.0, 9_950.0, 0.65, [65.0, 75.0], Some((0, 2))));
        // bucket 110: no gate read, no prompt growth
        h.push(trend_sample(at(110, 0), 11_000.0, 9_950.0, 0.50, [50.0, 50.0], None));
        h.push(trend_sample(at(110, 10), 11_000.0, 9_950.0, 0.50, [50.0, 50.0], None));
        let sr = h.series(now);
        assert_eq!(sr.prefix_hit[100], Some(0.95), "cached / prompt growth in the bucket (the first sample's step is from before it)");
        assert_eq!(sr.spec_accept_rate[100], Some(0.65), "the bucket's mean acceptance");
        assert_eq!(sr.gpu_power_w[100], Some(150.0), "all cards summed, the bucket's max");
        let mean = h.power_mean_w(now);
        assert_eq!((mean[100], mean[50], mean.len()), (Some(143.333), None, SERIES_POINTS), "card #283: the same bucket's MEAN (140, 150, 140) on the same grid - what $/h is priced from");
        assert_eq!(sr.refused[100], Some(3.0), "413s + 429s across both lanes; 200s are not refusals");
        assert_eq!(sr.refused[110], None, "no gateway read in the bucket: no data, never 0");
        assert_eq!(sr.prefix_hit[110], None, "no prompt tokens arrived: no share to report, never 0");
        assert_eq!(sr.prefix_hit[50], None, "a bucket with no sample at all is null in every series");
        assert_eq!((sr.spec_accept_rate[50], sr.gpu_power_w[50]), (None, None));
        // the collector fills these from what only it holds; lss-core never guesses them
        assert!(sr.ttft_p99_ms.iter().chain(&sr.itl_p99_ms).chain(&sr.usd_per_hour).all(Option::is_none));
        assert_eq!((sr.ttft_p99_ms.len(), sr.usd_per_hour.len(), sr.refused.len()), (SERIES_POINTS, SERIES_POINTS, SERIES_POINTS), "every series on the same grid");
    }

    #[test]
    fn an_engine_without_spec_decoding_or_token_counters_gets_null_not_zero() {
        let now: i64 = 36_000;
        let end = now - now.rem_euclid(SERIES_STEP_SECS) + SERIES_STEP_SECS;
        let start = end - HISTORY_SECS;
        let mut h = History::default();
        for (k, prompt) in [(0, 100.0), (10, 400.0)] {
            let mut s = trend_sample(start + 60 * 30 + k, prompt, 0.0, 0.0, [10.0, 10.0], None);
            s.not_reported = vec!["spec".into(), "tokens".into()];
            h.push(s);
        }
        let sr = h.series(now);
        assert_eq!(sr.spec_accept_rate[60], None, "no speculative decoding is not 0% acceptance");
        assert_eq!(sr.prefix_hit[60], None, "counters the engine does not report are not a 0% hit rate");
        assert_eq!(sr.gpu_power_w[60], Some(20.0), "what IS reported still comes through");
    }

    fn sample(ts: i64, evicted: f64, generated: f64) -> Sample {
        Sample { ts, serve_up: true, metrics: Some(ServeMetrics { evicted_tokens_total: evicted, generation_tokens_total: generated, ..Default::default() }), ..Default::default() }
    }

    /// card #279: the full `/status` document, so a test can assert a field ARRIVED - not just
    /// that the arithmetic behind it is right. #147's whole lesson is that those are different
    /// claims: four fields were computed correctly and reached nothing for weeks.
    fn status_of(history: &History) -> crate::model::Status {
        build_status(&StatusInputs {
            now: 1_000, host: "gpu-box", collector_version: "0.1.0", collector_started_at: 0, poll_secs: 5, slots: 8,
            history, incidents: &[], alerts: &[], probes: &[], last_valid_probe: None, firing: vec![], serve_down_since: None,
            gate_down_since: None, restarts_today: 0, thermal_exclude: &[], probe_enabled: true, probe_interval_s: 300,
            c1_baseline: None, c1_baseline_source: String::new(),
            thresholds: Thresholds::new(&crate::config::RulesConfig::default(), None, 300),
            probes_admitted_since_gate_start: 0, latency: None, gpu_health: &[], user_aliases: &[], tailscale: &Default::default(),
            own_traffic: Default::default(), advice_top: Vec::new(), bench: Default::default(), maintenance: Default::default(),
            cost: None, spending: None, tokens_hour: None, tokens_day: None, tokens_week: None, tokens_month: None,
            tokens_hour_covered_secs: None, tokens_day_covered_secs: None, tokens_week_covered_secs: None, tokens_month_covered_secs: None,
            watch: None, loadout: None, prefill_typical: None, live_decode: None, public_priority: String::new(),
            trusted_priority: String::new(), targets: Default::default(), user_log_24h: None,
        })
    }

    /// A sample whose engine DOES publish a preemption counter (SGLang's `num_retracted_reqs`).
    /// `evicted` is set to a different, larger number on purpose: a rate accidentally built on
    /// the eviction counter next door would come out 10x and this test would say so.
    fn preempting(ts: i64, retracted: f64, evicted: f64) -> Sample {
        Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics { retracted, evicted_tokens_total: evicted, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn preempted_per_hour_is_the_retracted_counters_growth_over_the_ten_minute_window() {
        let mut h = History::default();
        h.push(preempting(400, 10.0, 1_000.0)); // seeds `prev`
        h.push(preempting(1_000, 40.0, 9_000.0)); // +30 preemptions over the 600 s window
        let s = status_of(&h);
        assert_eq!(s.serve.preempted_per_hour_10m, Some(180.0), "30 in 600 s is 180/hour - and NOT the eviction counter's 8,000 (48,000/hour)");
        assert_eq!(s.serve.evicted_tok_per_hour_10m, Some(48_000.0), "the neighbour it must not be confused with");
    }

    #[test]
    fn a_real_zero_preemptions_is_published_as_zero_not_as_unknown() {
        let mut h = History::default();
        h.push(preempting(400, 7.0, 0.0));
        h.push(preempting(1_000, 7.0, 0.0)); // the counter exists and did not move: nothing was pushed out
        assert_eq!(status_of(&h).serve.preempted_per_hour_10m, Some(0.0), "a reported, flat counter is a MEASURED zero");
    }

    #[test]
    fn one_sample_is_not_a_rate() {
        let mut h = History::default();
        h.push(preempting(1_000, 7.0, 0.0));
        assert_eq!(status_of(&h).serve.preempted_per_hour_10m, None, "nothing to take a delta against");
    }

    /// The silent-zero guard. `EngineMetrics::to_serve_metrics` turns an unreported counter into
    /// 0, so on an engine with no preemption metric the window sees a flat 0 - and 0/hour reads
    /// as "nothing was pushed out", which is the OPPOSITE of "this engine cannot tell us".
    #[test]
    fn an_engine_that_reports_no_preemption_counter_publishes_none_not_a_fake_zero() {
        let mut h = History::default();
        for ts in [400, 1_000] {
            let mut s = preempting(ts, 0.0, 0.0);
            s.not_reported = vec!["preemptions".into()];
            h.push(s);
        }
        let s = status_of(&h);
        assert_eq!(s.serve.preempted_per_hour_10m, None, "an engine that publishes no counter must not publish a zero rate");
        assert!(s.serve.is_na("preemptions"), "and the screen must be able to say WHICH engine is silent");
        assert_eq!(s.serve.na("preemptions").as_deref(), Some("n/a (not reported by the engine)"));
    }

    /// `preemptions` must be a real `METRIC_FIELDS` key, or `not_reported` could never carry it
    /// and the guard above would be unreachable in production.
    #[test]
    fn preemptions_is_a_known_metric_field_and_sglang_reports_it() {
        assert!(crate::engine::METRIC_FIELDS.iter().any(|(k, _)| *k == "preemptions"), "the key the guard reads must exist");
        let sglang = crate::engine::EngineMetrics::from_sglang(&ServeMetrics { retracted: 3.0, ..Default::default() });
        assert_eq!(sglang.preemptions_total, Some(3.0));
        assert!(!sglang.not_reported(None).iter().any(|k| k == "preemptions"), "SGLang publishes it");
        let silent = crate::engine::EngineMetrics { preemptions_total: None, ..Default::default() };
        assert!(silent.not_reported(None).iter().any(|k| k == "preemptions"), "an engine without the counter is listed");
    }

    fn with_shadow(ts: i64, drain: Option<f64>) -> Sample {
        Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics::default()),
            gate: Some(crate::gate::GateHealth {
                version: "v5.11".into(),
                upstream_ok: true,
                shadow: Some(crate::gate::GateShadow { enabled: true, healthy: true, seconds_to_drain_charged: drain, ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// card #279's second half: the gate has computed this since v5.10 (card #154) and nothing
    /// carried it the last hop into `/status`, so every screen had to send the reader to the
    /// gateway's own health endpoint for it.
    #[test]
    fn seconds_to_drain_charged_reaches_status_from_the_gates_shadow_block() {
        let mut h = History::default();
        h.push(with_shadow(1_000, Some(12.5)));
        assert_eq!(status_of(&h).gate.seconds_to_drain_charged, Some(12.5));
    }

    #[test]
    fn a_gate_that_cannot_compute_a_drain_time_publishes_none_not_zero() {
        let mut h = History::default();
        h.push(with_shadow(1_000, None)); // v5.10+ but no prefill rate yet
        assert_eq!(status_of(&h).gate.seconds_to_drain_charged, None, "0 would read as `drains instantly`");
        let mut old = History::default();
        let mut s = with_shadow(1_000, Some(9.0));
        s.gate.as_mut().unwrap().shadow = None; // a gate older than v5.10 publishes no shadow at all
        old.push(s);
        assert_eq!(status_of(&old).gate.seconds_to_drain_charged, None, "a gate with no shadow block at all");
    }

    #[test]
    fn counter_window_sums_growth_inside_the_window_and_ignores_outside_it() {
        // like `reading_window`, the first IN-WINDOW sample only seeds `prev` - it takes a
        // second in-window sample before a delta is counted, so growth entirely before the
        // window (100 -> 150 here, both before ts=400) never leaks in
        let mut h = History::default();
        h.push(sample(0, 100.0, 0.0));
        h.push(sample(100, 150.0, 0.0)); // both before the window (ts < 400): never counted
        h.push(sample(700, 200.0, 0.0)); // seeds `prev` - not yet a delta
        h.push(sample(900, 260.0, 0.0)); // +60 against the previous IN-WINDOW sample
        let grown = h.counter_window(1_000, 600, |m| m.evicted_tokens_total);
        assert_eq!(grown, Some(60.0), "only the 700->900 step is a delta between two in-window samples");
    }

    #[test]
    fn counter_window_is_none_with_fewer_than_two_comparable_samples() {
        let mut h = History::default();
        assert_eq!(h.counter_window(1_000, 600, |m| m.evicted_tokens_total), None, "empty history");
        h.push(sample(900, 100.0, 0.0));
        assert_eq!(h.counter_window(1_000, 600, |m| m.evicted_tokens_total), None, "one sample: nothing to take a delta of");
    }

    #[test]
    fn counter_window_never_goes_negative_across_a_restart() {
        // an engine restart resets counters_v (comparable() fails) as well as the counter itself;
        // the step contributes 0, not a negative delta, and the window is still "seen"
        let mut h = History::default();
        h.push(sample(700, 500.0, 0.0));
        let mut restarted = sample(900, 20.0, 0.0); // counter reset low after a restart
        restarted.metrics.as_mut().unwrap().counters_v = 1;
        h.push(restarted);
        let mut next = sample(950, 40.0, 0.0); // same new epoch as the restarted sample
        next.metrics.as_mut().unwrap().counters_v = 1;
        h.push(next); // +20 counts, against the restarted sample - the same epoch
        let grown = h.counter_window(1_000, 600, |m| m.evicted_tokens_total);
        assert_eq!(grown, Some(20.0), "the restart step is 0, not negative; the next real step still counts");
    }

    #[test]
    fn secs_since_last_token_finds_the_newest_growth_in_the_retained_history() {
        let mut h = History::default();
        h.push(sample(0, 0.0, 100.0));
        h.push(sample(300, 0.0, 150.0)); // grew here
        h.push(sample(600, 0.0, 150.0)); // flat
        h.push(sample(900, 0.0, 150.0)); // still flat
        assert_eq!(h.secs_since_last_token(1_000), Some(700), "last growth was at ts=300, now=1000");
    }

    /// CARD #197: the windowed input the TP-skew reading is scored on, built from REAL polls
    /// replayed read-only off this box's own sample table (the run at 1790085649, six consecutive
    /// 5 s polls). A DIFFERENT card dips on each of them while the pack sits at 95+ - that is the
    /// sampler catching whichever card is between kernels, and the medians say so: no card comes
    /// out more than a few points behind the others, where the instantaneous vector of the last
    /// poll alone reads 32 points of "skew".
    #[test]
    fn a_cards_windowed_median_ignores_the_poll_it_was_caught_between_kernels_on() {
        let poll = |ts: i64, u: [f64; 4]| Sample {
            ts,
            serve_up: true,
            gpus_ok: true,
            gpus: (0..4).map(|i| crate::gpu::GpuSample { index: i as u32, util_pct: Some(u[i]), ..Default::default() }).collect(),
            ..Default::default()
        };
        let real = [
            [76.0, 97.0, 97.0, 43.0],
            [96.0, 98.0, 98.0, 96.0],
            [67.0, 97.0, 55.0, 85.0],
            [95.0, 58.0, 95.0, 98.0],
            [37.0, 95.0, 77.0, 32.0],
            [97.0, 62.0, 94.0, 95.0],
        ];
        let mut h = History::default();
        for (k, u) in real.iter().enumerate() {
            // five of the six is not a window yet: MISSING is not a number to judge
            if k == 5 {
                assert_eq!(h.gpu_util_median(1), None, "{} polls is short of the window", k);
            }
            h.push(poll(1_790_085_649 + 5 * k as i64, *u));
        }
        assert_eq!(h.gpu_util_median(0), Some(85.5));
        assert_eq!(h.gpu_util_median(1), Some(96.0), "the card the LAST poll caught at 62 was at the pack all window");
        assert_eq!(h.gpu_util_median(2), Some(94.5));
        assert_eq!(h.gpu_util_median(3), Some(90.0));
        assert_eq!(h.gpu_util_median(7), None, "a card this box does not have");
        // a card that stops being reported at all takes its median with it
        h.push(poll(1_790_085_679, [90.0, 90.0, 90.0, 90.0]));
        let mut missing = poll(1_790_085_684, [90.0, 90.0, 90.0, 90.0]);
        missing.gpus.remove(1);
        h.push(missing);
        assert_eq!(h.gpu_util_median(1), None, "one poll with no reading for the card is not a median");
        assert!(h.gpu_util_median(0).is_some());
    }

    #[test]
    fn secs_since_last_token_is_none_when_nothing_ever_grew() {
        let mut h = History::default();
        h.push(sample(0, 0.0, 100.0));
        h.push(sample(500, 0.0, 100.0));
        assert_eq!(h.secs_since_last_token(1_000), None, "flat the whole retained history: never a growth to time");
    }
}
