//! Time series: the metric catalogue, the rollup tiers, and the `/series` query plan.
//!
//! Three tiers, downsampled ON WRITE so a long range never reads raw samples:
//!   raw   5 s samples            kept 24 h   (the `samples` table)
//!   1m    avg/min/max per minute kept 14 d
//!   10m   avg/min/max per 10 min kept 90 d
//! Pure: no clock, no database. The collector feeds samples in and stores what comes out.

use crate::gpu::{THROTTLE_HW_POWER_BRAKE, THROTTLE_HW_SLOWDOWN, THROTTLE_SW_POWER_CAP, THROTTLE_THERMAL_MASK};
use crate::incidents::Incident;
use crate::model::{AlertRow, Sample};
use crate::rules::RuleState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RES_RAW: i64 = 5;
pub const RES_1M: i64 = 60;
pub const RES_10M: i64 = 600;
/// `/series` never returns more points than this per series.
pub const MAX_POINTS: usize = 600;
/// The raw tier is only read for short ranges: a request costs one JSON parse per sample.
pub const RAW_MAX_RANGE: i64 = 2 * 3600;

/// The ranges the screen cycles through with `r`.
pub const RANGES: [&str; 5] = ["15m", "1h", "6h", "24h", "7d"];

/// `15m`, `1h`, `6h`, `24h`, `7d`, `90s`, or plain seconds.
pub fn parse_range(text: &str) -> Option<i64> {
    let t = text.trim();
    let (num, mult) = match t.chars().last()? {
        's' => (&t[..t.len() - 1], 1),
        'm' => (&t[..t.len() - 1], 60),
        'h' => (&t[..t.len() - 1], 3600),
        'd' => (&t[..t.len() - 1], 86_400),
        _ => (t, 1),
    };
    num.parse::<i64>().ok().filter(|n| *n > 0).map(|n| n * mult)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AggKind {
    /// rates and ratios: the mean is the honest number
    #[default]
    Avg,
    Min,
    /// levels where the spike is the point (queue, temperature)
    Max,
    /// counts per sample (tokens, requests): a bucket holds their total
    Sum,
}

impl AggKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "avg" | "mean" => Some(AggKind::Avg),
            "min" => Some(AggKind::Min),
            "max" => Some(AggKind::Max),
            "sum" => Some(AggKind::Sum),
            _ => None,
        }
    }
}

/// How a metric is downsampled when the request does not say (`name:max` says).
pub fn default_agg(metric: &str) -> AggKind {
    if metric.starts_with("tok_") || metric.starts_with("sum_") || metric == "gate_requests" {
        return AggKind::Sum;
    }
    let spiky = ["queue", "running", "waiters", "inflight", "budget_used", "users_active", "_temp_c", "kv_usage", "_thr_", "_p99_ms", "_p90_ms", "c1_invalid"];
    if spiky.iter().any(|s| metric.contains(s)) { AggKind::Max } else { AggKind::Avg }
}

/// Metrics that only exist per closed minute (they are not in a 5 s sample).
pub fn is_minute_native(metric: &str) -> bool {
    metric.ends_with("_ms") && crate::hist::LATENCY_METRICS.iter().any(|(s, _)| metric.starts_with(s))
}

/// Metrics served from the `probes` table at every tier.
pub fn is_probe_metric(metric: &str) -> bool {
    metric.starts_with("c1_")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub n: f64,
}

impl Agg {
    pub fn one(v: f64) -> Self {
        Agg { sum: v, min: v, max: v, n: 1.0 }
    }
    /// A stored rollup row.
    pub fn from_row(avg: f64, min: f64, max: f64, n: f64) -> Self {
        let n = n.max(1.0);
        Agg { sum: avg * n, min, max, n }
    }
    pub fn merge(&mut self, o: &Agg) {
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.n += o.n;
    }
    pub fn avg(&self) -> f64 {
        self.sum / self.n.max(1.0)
    }
    pub fn pick(&self, kind: AggKind) -> f64 {
        match kind {
            AggKind::Avg => self.avg(),
            AggKind::Min => self.min,
            AggKind::Max => self.max,
            AggKind::Sum => self.sum,
        }
    }
}

/// Every number a 5 s sample contributes. `prev` (the sample before it) turns the engine's
/// counters into rates.
pub fn sample_points(prev: Option<&Sample>, cur: &Sample) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::with_capacity(64);
    let mut put = |name: &str, v: f64| {
        if v.is_finite() {
            out.push((name.to_string(), v));
        }
    };
    // card #180 gate 3 (lss-builder-3, measured against a real Ollama): `ServeMetrics` fields
    // are plain `f64`, so an engine that never publishes a metric still leaves it at its
    // struct default, 0.0 - and this function used to push that 0.0 into the series as a real
    // point. Downstream that becomes a fabricated flat-zero line on the LOAD/LATENCY pages and
    // in `/series`, indistinguishable from "measured, and it's genuinely zero". `not_reported`
    // (the same `engine::METRIC_FIELDS` keys the live `/status` page already uses for its own
    // "n/a - not reported by X" wording) is carried on every stored `Sample` for exactly this
    // reason; this is the one place that data was collected and never consulted.
    let reported = |key: &str| !cur.not_reported.iter().any(|k| k == key);
    put("serve_up", f64::from(u8::from(cur.serve_up)));
    if let Some(m) = &cur.metrics {
        if reported("decode_tok_s") {
            put("decode_tok_s", m.gen_throughput);
        }
        if reported("running") {
            put("running", m.running);
            put("running_public", m.running_public);
            put("running_trusted", m.running_trusted);
        }
        if reported("queued") {
            put("queue", m.queue);
            put("queue_public", m.queue_public);
            put("queue_trusted", m.queue_trusted);
            put("busy_wait", f64::from(u8::from(m.queue > 0.0)));
        }
        if reported("kv_usage") {
            put("kv_usage", m.kv_usage());
        }
        if reported("cache_hit") {
            put("cache_hit_rate", m.cache_hit_rate);
        }
        if reported("spec") {
            put("spec_accept_length", m.spec_accept_length);
            put("spec_accept_rate", m.spec_accept_rate);
        }
        // 0/1 indicators: their average over a window is "the share of the time" - meaningless
        // if either half of the comparison (how many are running, how many slots there are) is
        // not real.
        if cur.slots > 0 && reported("running") && reported("slots") {
            put("busy_slots", f64::from(u8::from(m.running >= f64::from(cur.slots))));
        }
        put("kv_retracted", f64::from(u8::from(m.retracted > 0.0)));
        if let Some((p, pm)) = prev.and_then(|p| p.metrics.as_ref().map(|pm| (p, pm))) {
            let dt = (cur.ts - p.ts) as f64;
            // a gap or an engine restart (counters back to zero) is not a rate
            if dt > 0.0 && dt <= 60.0 {
                let rate = |now: f64, before: f64| (now >= before).then(|| (now - before) / dt);
                // "tokens" covers both counters this rate/delta pair is built from together -
                // the same pairing `engine::not_reported`'s own "tokens" key checks.
                if reported("tokens") {
                    if let Some(r) = rate(m.prompt_tokens_total, pm.prompt_tokens_total) {
                        put("prompt_tok_s", r);
                    }
                    if let Some(r) = rate(m.generation_tokens_total, pm.generation_tokens_total) {
                        put("gen_tok_s", r);
                    }
                }
                // card #180 gate 3 (the re-run against a real Ollama): these three were the ones
                // 710add5 could not gate - no not_reported key existed for a request counter - so
                // `lss load` still drew req_per_min, sum_e2e_s and sum_prefill_s as flat zeros
                // while real requests were being answered. `requests` is that key now; the two
                // time sums come from the request-duration family `e2e` already names.
                if reported("requests") {
                    if let Some(r) = rate(m.requests_total, pm.requests_total) {
                        put("req_per_min", r * 60.0);
                    }
                }
                // token COUNTS of this sample (`:sum` over a bucket = tokens in it)
                // never across two samples that carry different sets of counters (a counter an
                // older binary did not store reads back as 0, and its whole life would be booked here)
                let same = m.comparable(pm);
                let grown = |now: f64, before: f64| (same && now >= before).then_some(now - before);
                if reported("tokens") {
                    if let Some(d) = grown(m.generation_tokens_total, pm.generation_tokens_total) {
                        put("tok_gen", d);
                    }
                    if let Some(d) = grown(m.prompt_tokens_total, pm.prompt_tokens_total) {
                        put("tok_prompt", d);
                    }
                }
                if reported("cache_hit") {
                    if let Some(d) = grown(m.cached_tokens_total, pm.cached_tokens_total) {
                        put("tok_cached", d);
                    }
                }
                // prompt tokens the GPUs really read (cache hits cost nothing and are not in here).
                // `prefill_tok_s` only exists for the steps in which something was read, so its
                // average is "the reading speed while reading", not diluted by idle time.
                if reported("prefill_tok_s") {
                    let read = m.computed_prompt_tokens_since(pm);
                    put("tok_prefill", read);
                    if read > 0.0 {
                        put("prefill_tok_s", read / dt);
                    }
                }
                if let Some(d) = grown(m.evicted_tokens_total, pm.evicted_tokens_total) {
                    put("tok_evicted", d);
                }
                if reported("requests") {
                    if let Some(d) = grown(m.requests_total, pm.requests_total) {
                        put("sum_requests", d);
                    }
                }
                // request-seconds end to end and in the prefill forward pass: the time split
                if let (Some(e2e), Some(prefill), true) = (grown(m.e2e_sum, pm.e2e_sum), grown(m.prefill_forward_sum, pm.prefill_forward_sum), reported("e2e")) {
                    put("sum_e2e_s", e2e);
                    put("sum_prefill_s", prefill);
                }
                // energy of this step (trapezoid), in total and while at least one request ran
                let watts = |s: &Sample| -> Option<f64> {
                    let v: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                if let (Some(a), Some(b), true) = (watts(p), watts(cur), dt <= 30.0) {
                    let joules = (a + b) / 2.0 * dt;
                    put("sum_energy_j", joules);
                    if m.running >= 1.0 {
                        put("sum_energy_serving_j", joules);
                    }
                }
            }
        }
    }
    if let Some(g) = &cur.gate {
        put("inflight_public", g.public.inflight_tokens as f64);
        put("inflight_trusted", g.trusted.inflight_tokens as f64);
        put("waiters_public", g.public.waiters as f64);
        put("waiters_trusted", g.trusted.waiters as f64);
        // card #69: the LANES chart's real subject - how full the in-flight budget is.
        // A fraction (0..1) so the chart never needs to know the capacity, and it VARIES
        // (the gate queue it replaces sat at zero for days). Waiters ride along raw.
        if let Some(budget) = g.trusted.budget_tokens.filter(|b| *b > 0) {
            put("budget_used_frac", g.trusted.inflight_tokens as f64 / budget as f64);
        }
    }
    put("gate_up", f64::from(u8::from(cur.gate.is_some())));
    if cur.gate.is_some() {
        // per-user concurrency and request rate (a gateway publishing /gate/health v5.2+; nobody active = zeros)
        // the benchmark's own traffic is not a user
        let real = || cur.users.iter().filter(|u| !u.id.ends_with(crate::users::BENCH_USER));
        // CROSS-MACHINE figures (card #211): a user busy on ANY upstream the gate fronts. The
        // gate can front several physical machines, so these are never "load on this box".
        put("users_active", real().filter(|u| u.inflight > 0).count() as f64);
        put("users_inflight", real().map(|u| u.inflight).sum::<u64>() as f64);
        // card #211: the same two, PER MACHINE (`users_active.<upstream>`), from the gate's
        // v5.10 attribution. Every configured upstream gets a point, 0 included, so a machine
        // nobody is on reads 0 rather than "no data"; an older gate gets none at all, which is
        // how the advice tells "too old to say" from "idle".
        if let Some(g) = cur.gate.as_ref() {
            let here = g.this_upstream(cur.model.as_deref());
            for up in g.attributed_upstreams.iter().flatten() {
                let on = |u: &&crate::users::UserPoint| u.by_upstream.get(up).copied().unwrap_or(0);
                let (active, inflight) = (real().filter(|u| on(u) > 0).count() as f64, real().map(|u| on(&u)).sum::<u64>() as f64);
                let id = crate::gate::upstream_series_id(up);
                put(&format!("users_active.{id}"), active);
                put(&format!("users_inflight.{id}"), inflight);
                // ...and the one that is THIS machine under a fixed name, so a reader (the growth
                // advice) needs no upstream name to ask "how many users were on this box"
                if here == Some(up.as_str()) {
                    put("users_active_here", active);
                    put("users_inflight_here", inflight);
                }
            }
        }
    }
    for u in &cur.users {
        put(&format!("user_inflight.{}", u.id), u.inflight as f64);
        put(&format!("user_rpm.{}", u.id), u.rpm as f64);
    }
    let gate_reqs = cur.log.public.requests + cur.log.trusted.requests;
    if gate_reqs > 0 {
        put("gate_requests", gate_reqs as f64);
    }
    let mut total_power = None;
    for g in &cur.gpus {
        let i = g.index;
        let mut gput = |what: &str, v: Option<f64>| {
            if let Some(v) = v {
                put(&format!("gpu{i}_{what}"), v);
            }
        };
        gput("temp_c", g.temp_c);
        gput("power_w", g.power_w);
        gput("clock_mhz", g.clock_mhz);
        gput("util_pct", g.util_pct);
        gput("mem_used_mib", g.mem_used_mib);
        gput("mem_util_pct", g.mem_util_pct);
        gput("mem_temp_c", g.mem_temp_c);
        let bit = |mask: u64| Some(f64::from(u8::from(g.throttle_mask & mask != 0)));
        gput("thr_thermal", bit(THROTTLE_THERMAL_MASK));
        gput("thr_hw", bit(THROTTLE_HW_SLOWDOWN | THROTTLE_HW_POWER_BRAKE));
        gput("thr_power", bit(THROTTLE_SW_POWER_CAP));
        if let Some(p) = g.power_w {
            *total_power.get_or_insert(0.0) += p;
        }
    }
    if let Some(p) = total_power {
        put("gpu_power_total_w", p);
    }
    out
}

/// One finished rollup bucket, ready to be written.
#[derive(Debug, Clone, PartialEq)]
pub struct RollupBatch {
    pub res: i64,
    pub ts: i64,
    pub rows: Vec<(String, Agg)>,
}

/// Accumulates points into fixed buckets of `res` seconds; a bucket is handed out when the
/// first point of a later bucket arrives (or on `flush`).
#[derive(Debug)]
pub struct Roller {
    res: i64,
    bucket: Option<i64>,
    acc: BTreeMap<String, Agg>,
}

impl Roller {
    pub fn new(res: i64) -> Self {
        Self { res, bucket: None, acc: BTreeMap::new() }
    }

    pub fn add(&mut self, ts: i64, points: &[(String, f64)]) -> Option<RollupBatch> {
        let start = ts - ts.rem_euclid(self.res);
        let done = if self.bucket.is_some_and(|b| b != start) { self.flush() } else { None };
        self.bucket = Some(start);
        for (name, v) in points {
            match self.acc.get_mut(name) {
                Some(a) => a.merge(&Agg::one(*v)),
                None => {
                    self.acc.insert(name.clone(), Agg::one(*v));
                }
            }
        }
        done
    }

    pub fn flush(&mut self) -> Option<RollupBatch> {
        let ts = self.bucket.take()?;
        let rows: Vec<(String, Agg)> = std::mem::take(&mut self.acc).into_iter().collect();
        (!rows.is_empty()).then_some(RollupBatch { res: self.res, ts, rows })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Raw,
    M1,
    M10,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Raw => "raw",
            Tier::M1 => "1m",
            Tier::M10 => "10m",
        }
    }
    pub fn res(self) -> i64 {
        match self {
            Tier::Raw => RES_RAW,
            Tier::M1 => RES_1M,
            Tier::M10 => RES_10M,
        }
    }
}

/// Where a `/series` request reads from and how its arrays are laid out. Every series of one
/// answer shares `start_ts`, `step_s` and `points`, so index i is the same moment in all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub tier: Tier,
    pub range_s: i64,
    pub step_s: i64,
    pub start_ts: i64,
    pub points: usize,
}

/// `step = None` is `auto`: the finest step that keeps the answer under `MAX_POINTS`.
/// `retention` = (raw, 1m, 10m) seconds actually kept.
pub fn plan(now: i64, range_s: i64, step: Option<i64>, retention: (i64, i64, i64)) -> Plan {
    let range_s = range_s.clamp(60, retention.2.max(60));
    let floor = (range_s + MAX_POINTS as i64 - 1) / MAX_POINTS as i64;
    let want = step.unwrap_or(1).max(floor).max(1);
    let tier = if want < RES_1M && range_s <= RAW_MAX_RANGE.min(retention.0) {
        Tier::Raw
    } else if want < RES_10M && range_s <= retention.1 {
        Tier::M1
    } else {
        Tier::M10
    };
    let res = tier.res();
    let step_s = (want + res - 1) / res * res;
    let end = now - now.rem_euclid(step_s) + step_s;
    let points = (((range_s + step_s - 1) / step_s) as usize).clamp(1, MAX_POINTS);
    Plan { tier, range_s, step_s, start_ts: end - points as i64 * step_s, points }
}

/// `name` or `name:avg|min|max`.
pub fn parse_metric_token(token: &str) -> (String, AggKind) {
    match token.rsplit_once(':') {
        Some((name, agg)) if AggKind::parse(agg).is_some() => (name.to_string(), AggKind::parse(agg).unwrap_or_default()),
        _ => (token.to_string(), default_agg(token)),
    }
}

/// `GET /series`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeriesDoc {
    pub v: u32,
    pub generated_at: i64,
    pub range_s: i64,
    pub step_s: i64,
    pub start_ts: i64,
    pub points: usize,
    /// `raw` | `1m` | `10m`
    pub tier: String,
    /// requested token -> one value per step, oldest first, `null` = no data
    pub series: BTreeMap<String, Vec<Option<f64>>>,
    /// requested names the collector has never recorded
    pub unknown: Vec<String>,
}

impl SeriesDoc {
    pub fn get(&self, token: &str) -> &[Option<f64>] {
        self.series.get(token).map_or(&[], Vec::as_slice)
    }
    pub fn last(&self, token: &str) -> Option<f64> {
        self.get(token).iter().rev().flatten().next().copied()
    }
    pub fn ts_of(&self, index: usize) -> i64 {
        self.start_ts + index as i64 * self.step_s
    }
}

/// Folds tier rows into the aligned arrays of one answer.
pub struct SeriesBuilder {
    plan: Plan,
    tokens: Vec<(String, String, AggKind)>,
    cells: Vec<Vec<Option<Agg>>>,
    seen: Vec<bool>,
}

impl SeriesBuilder {
    pub fn new(plan: Plan, tokens: &[String]) -> Self {
        let tokens: Vec<(String, String, AggKind)> = tokens
            .iter()
            .map(|t| {
                let (name, agg) = parse_metric_token(t);
                (t.clone(), name, agg)
            })
            .collect();
        Self { plan, cells: vec![vec![None; plan.points]; tokens.len()], seen: vec![false; tokens.len()], tokens }
    }

    pub fn plan(&self) -> Plan {
        self.plan
    }

    /// Distinct metric names asked for.
    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.tokens.iter().map(|(_, name, _)| name.clone()).collect();
        n.sort();
        n.dedup();
        n
    }

    pub fn mark_known(&mut self, metric: &str) {
        for (i, (_, name, _)) in self.tokens.iter().enumerate() {
            if name == metric {
                self.seen[i] = true;
            }
        }
    }

    pub fn add(&mut self, metric: &str, ts: i64, agg: Agg) {
        if ts < self.plan.start_ts {
            return;
        }
        let idx = ((ts - self.plan.start_ts) / self.plan.step_s) as usize;
        if idx >= self.plan.points {
            return;
        }
        for (i, (_, name, _)) in self.tokens.iter().enumerate() {
            if name != metric {
                continue;
            }
            self.seen[i] = true;
            match &mut self.cells[i][idx] {
                Some(a) => a.merge(&agg),
                slot => *slot = Some(agg),
            }
        }
    }

    pub fn finish(self, now: i64) -> SeriesDoc {
        let mut series = BTreeMap::new();
        let mut unknown = Vec::new();
        for (i, (token, _, kind)) in self.tokens.iter().enumerate() {
            if !self.seen[i] {
                unknown.push(token.clone());
            }
            series.insert(token.clone(), self.cells[i].iter().map(|c| c.map(|a| round4(a.pick(*kind)))).collect());
        }
        SeriesDoc {
            v: crate::STATUS_SCHEMA_VERSION,
            generated_at: now,
            range_s: self.plan.range_s,
            step_s: self.plan.step_s,
            start_ts: self.plan.start_ts,
            points: self.plan.points,
            tier: self.plan.tier.name().to_string(),
            series,
            unknown,
        }
    }
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// `GET /hist`: the bucket deltas of one latency histogram over a range.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistDoc {
    pub v: u32,
    pub generated_at: i64,
    pub metric: String,
    pub range_s: i64,
    /// width of one heat-strip column
    pub step_s: i64,
    pub start_ts: i64,
    /// finite upper bounds in SECONDS; one more bucket (`+Inf`) follows the last
    pub le: Vec<f64>,
    /// observations per bucket over the whole range, `le.len() + 1` long
    pub total: Vec<f64>,
    pub summary: Option<crate::hist::HistSummary>,
    /// `s` (the latency histograms) or `tokens` (`prompt_tokens`, `gen_tokens`): the unit of `le`
    pub unit: String,
    /// p50/p90/p99/avg in that unit. For the size histograms `summary` (milliseconds) is null
    /// and this is the one to read
    pub summary_raw: Option<crate::hist::RawSummary>,
    /// one entry per column, oldest first; an empty array = no requests in that column
    pub columns: Vec<Vec<f64>>,
}

/// Column layout of a `/hist` answer: (tier resolution, step, start, columns).
pub fn hist_plan(now: i64, range_s: i64, retention_1m: i64) -> (i64, i64, i64, usize) {
    const COLUMNS: i64 = 120;
    let res = if range_s <= retention_1m.min(86_400) { RES_1M } else { RES_10M };
    let step = ((range_s + COLUMNS - 1) / COLUMNS + res - 1) / res * res;
    let end = now - now.rem_euclid(step) + step;
    let cols = ((range_s + step - 1) / step).max(1);
    (res, step, end - cols * step, cols as usize)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayRow {
    /// lane name, key name or masked address
    pub name: String,
    /// for key and address rows: the lane they came in on
    pub lane: String,
    pub requests: u64,
    /// how many of `requests` have a status breakdown behind the code columns below. Equal to
    /// `requests` unless part of the range predates per-key status codes (`codes_since`)
    pub coded_requests: u64,
    /// Status classes and the four codes the gate's admission control answers with. `null` =
    /// NOT KNOWN (no breakdown was recorded for this row in the range), which is not zero.
    /// Address rows never have one.
    #[serde(rename = "2xx")]
    pub c2xx: Option<u64>,
    #[serde(rename = "4xx")]
    pub c4xx: Option<u64>,
    #[serde(rename = "5xx")]
    pub c5xx: Option<u64>,
    #[serde(rename = "413")]
    pub s413: Option<u64>,
    #[serde(rename = "429")]
    pub s429: Option<u64>,
    #[serde(rename = "499")]
    pub s499: Option<u64>,
    #[serde(rename = "503")]
    pub s503: Option<u64>,
    /// requests that carried the gate's `estimated_tokens` (today: the rejected ones)
    pub est_tokens_n: u64,
    pub est_tokens_avg: Option<f64>,
    pub est_tokens_max: Option<f64>,
}

impl GatewayRow {
    pub fn from_codes(name: &str, lane: &str, codes: &BTreeMap<u16, u64>, est: &crate::gatelog::EstTokens) -> Self {
        let class = |c: u16| codes.iter().filter(|(code, _)| **code / 100 == c).map(|(_, n)| n).sum::<u64>();
        let one = |c: u16| codes.get(&c).copied().unwrap_or(0);
        let total: u64 = codes.values().sum();
        GatewayRow {
            name: name.to_string(),
            lane: lane.to_string(),
            requests: total,
            coded_requests: total,
            c2xx: Some(class(2)),
            c4xx: Some(class(4)),
            c5xx: Some(class(5)),
            s413: Some(one(413)),
            s429: Some(one(429)),
            s499: Some(one(499)),
            s503: Some(one(503)),
            est_tokens_n: est.n,
            est_tokens_avg: est.avg().map(f64::round),
            est_tokens_max: (est.n > 0).then_some(est.max),
        }
    }

    /// A row whose breakdown is not known: counts only.
    pub fn uncoded(name: &str, lane: &str, requests: u64) -> Self {
        GatewayRow { name: name.to_string(), lane: lane.to_string(), requests, ..Default::default() }
    }
}

/// From when on the range has per-key status codes and client addresses (the gate log was
/// stored without them before 2026-09-19). Fed the log deltas of the range OLDEST FIRST.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeCoverage {
    since: Option<i64>,
    gap: bool,
}

impl CodeCoverage {
    pub fn observe(&mut self, ts: i64, log: &crate::gatelog::LogDelta) {
        for lane in [&log.public, &log.trusted] {
            if lane.requests == 0 {
                continue;
            }
            let coded: u64 = lane.by_key_status.values().flat_map(|codes| codes.values()).sum();
            if coded >= lane.requests {
                self.since.get_or_insert(ts);
            } else {
                // not (fully) broken down: whatever coverage there was before does not reach to here
                self.since = None;
                self.gap = true;
            }
        }
    }

    /// `full` = every request of the range has its breakdown.
    pub fn label(&self) -> &'static str {
        match (self.gap, self.since) {
            (false, _) => "full",
            (true, Some(_)) => "partial",
            (true, None) => "none",
        }
    }

    /// Some(ts) only when the coverage is partial: the breakdown exists from here on.
    pub fn since(&self) -> Option<i64> {
        self.since.filter(|_| self.gap)
    }

    fn covers(&self, ts: i64) -> bool {
        !self.gap || self.since.is_some_and(|s| ts >= s)
    }
}

/// `GET /gateway`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayDoc {
    pub v: u32,
    pub generated_at: i64,
    pub range_s: i64,
    /// `public`, `trusted`
    pub lanes: Vec<GatewayRow>,
    /// busiest first
    pub keys: Vec<GatewayRow>,
    /// busiest first, addresses masked (`a.b.c.x`), the collector's own probe taken out
    pub ips: Vec<GatewayRow>,
    /// the collector's own C1 probes in the range, already taken out of `trusted`, `(no key)`
    /// and the probe's address row
    pub probes_excluded: u64,
    /// `full` | `partial` | `none`: how much of the range has per-key status codes
    pub codes_coverage: String,
    /// when `partial`: the code columns of `keys[]` (and `ips[]`) only count from here on
    pub codes_since: Option<i64>,
}

pub const NO_KEY: &str = "(no key)";

/// Builds the tables from the merged gate log of the range. `probes` = (when, the HTTP status
/// the gate logged) for each of the collector's own probes in the range; `probe_ip` = the
/// masked address they come from.
pub fn build_gateway(now: i64, range_s: i64, log: &crate::gatelog::LogDelta, probes: &[(i64, u16)], coverage: &CodeCoverage, probe_ip: Option<&str>) -> GatewayDoc {
    let mut trusted = log.trusted.clone();
    let mut excluded = 0;
    for (ts, code) in probes {
        // never take out more than the log holds: the two are booked a poll apart
        let Some(n) = trusted.by_status.get_mut(code).filter(|n| **n > 0) else { continue };
        *n -= 1;
        trusted.requests = trusted.requests.saturating_sub(1);
        if let Some(k) = trusted.by_key.get_mut(NO_KEY) {
            *k = k.saturating_sub(1);
        }
        excluded += 1;
        // the per-key codes and the addresses only hold the probes of the covered part
        if !coverage.covers(*ts) {
            continue;
        }
        if let Some(k) = trusted.by_key_status.get_mut(NO_KEY).and_then(|m| m.get_mut(code)) {
            *k = k.saturating_sub(1);
        }
        if let Some(n) = probe_ip.and_then(|ip| trusted.by_ip.get_mut(ip)) {
            *n = n.saturating_sub(1);
        }
    }
    let full = coverage.label() == "full";
    let mut keys = Vec::new();
    let mut ips = Vec::new();
    let mut lanes = Vec::new();
    for (lane, l) in [("public", &log.public), ("trusted", &trusted)] {
        lanes.push(GatewayRow::from_codes(lane, lane, &l.by_status, &l.est));
        for (key, n) in &l.by_key {
            let none = crate::gatelog::EstTokens::default();
            let est = l.by_key_est.get(key).unwrap_or(&none);
            let coded: u64 = l.by_key_status.get(key).map_or(0, |codes| codes.values().sum());
            let mut row = match l.by_key_status.get(key) {
                // a breakdown exists (for the whole range, or for its covered part)
                Some(codes) if full || coded > 0 => GatewayRow::from_codes(key, lane, codes, est),
                // none recorded for this key in the range: unknown, NOT zero
                _ if !full => GatewayRow::uncoded(key, lane, *n),
                _ => GatewayRow::from_codes(key, lane, &BTreeMap::new(), est),
            };
            row.requests = row.requests.max(*n);
            if row.requests > 0 {
                keys.push(row);
            }
        }
        for (ip, n) in l.by_ip.iter().filter(|(_, n)| **n > 0) {
            ips.push(GatewayRow::uncoded(ip, lane, *n));
        }
    }
    let busiest = |a: &GatewayRow, b: &GatewayRow| b.requests.cmp(&a.requests).then_with(|| a.name.cmp(&b.name));
    keys.sort_by(busiest);
    ips.sort_by(busiest);
    keys.truncate(50);
    ips.truncate(20);
    GatewayDoc { v: crate::STATUS_SCHEMA_VERSION, generated_at: now, range_s, lanes, keys, ips, probes_excluded: excluded, codes_coverage: coverage.label().to_string(), codes_since: coverage.since() }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Uptime {
    /// share of the last 24 h / 7 d the serve answered, 0..=1; null = no data yet
    pub h24: Option<f64>,
    pub d7: Option<f64>,
    /// the collector's first sample: a window reaching further back is cut here
    pub since: i64,
}

/// Serve availability from the `serve_down` incidents. Time the collector itself was not
/// running is unknown and counts as up.
pub fn uptime(now: i64, first_sample_ts: i64, incidents: &[Incident]) -> Uptime {
    let window = |secs: i64| -> Option<f64> {
        let start = (now - secs).max(first_sample_ts);
        let span = now - start;
        if span <= 0 {
            return None;
        }
        let down: i64 = incidents
            .iter()
            .filter(|i| i.kind == crate::incidents::KIND_SERVE_DOWN)
            .map(|i| (i.end.unwrap_or(now).min(now) - i.start.max(start)).max(0))
            .sum();
        Some(((1.0 - down as f64 / span as f64).clamp(0.0, 1.0) * 100_000.0).round() / 100_000.0)
    };
    Uptime { h24: window(86_400), d7: window(7 * 86_400), since: first_sample_ts }
}

/// `GET /rules`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesDoc {
    pub v: u32,
    pub generated_at: i64,
    pub rules: Vec<RuleState>,
    pub firing: Vec<String>,
    /// agent mail waiting in the spool; null = the spool directory could not be read
    pub spool_depth: Option<u64>,
    /// last 100, newest first
    pub alerts: Vec<AlertRow>,
    /// last 90 days (at most 200), newest first
    pub incidents: Vec<Incident>,
    pub uptime: Uptime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::GpuSample;
    use crate::prom::ServeMetrics;

    const DAY: i64 = 86_400;
    const KEEP: (i64, i64, i64) = (DAY, 14 * DAY, 90 * DAY);

    fn sample(ts: i64, decode: f64, queue: f64, prompt_total: f64) -> Sample {
        Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics { gen_throughput: decode, queue, queue_public: queue, prompt_tokens_total: prompt_total, requests_total: prompt_total / 100.0, ..Default::default() }),
            gpus_ok: true,
            gpus: vec![GpuSample { index: 0, temp_c: Some(50.0 + queue), power_w: Some(200.0), throttle_mask: 0x4, ..Default::default() }, GpuSample { index: 1, power_w: Some(100.0), throttle_mask: 0x40, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn ranges_parse() {
        assert_eq!(RANGES.map(|r| parse_range(r).unwrap()), [900, 3600, 21_600, DAY, 7 * DAY]);
        assert_eq!(parse_range("90"), Some(90));
        assert_eq!(parse_range("0h"), None);
        assert_eq!(parse_range("soon"), None);
    }

    #[test]
    fn a_sample_becomes_points_and_counters_become_rates() {
        let a = sample(1000, 400.0, 2.0, 10_000.0);
        let b = sample(1005, 420.0, 3.0, 15_000.0);
        let first = sample_points(None, &a);
        assert!(!first.iter().any(|(n, _)| n == "prompt_tok_s"), "one sample is not a rate");
        let pts: BTreeMap<String, f64> = sample_points(Some(&a), &b).into_iter().collect();
        assert_eq!((pts["tok_prompt"], pts["tok_gen"]), (5000.0, 0.0), "token counts per sample: `:sum` adds them up");
        assert_eq!((default_agg("tok_gen"), parse_metric_token("tok_prompt:avg").1, parse_metric_token("x:sum").1), (AggKind::Sum, AggKind::Avg, AggKind::Sum));
        let mut hour = Agg::one(5000.0);
        hour.merge(&Agg::one(2500.0));
        assert_eq!((hour.pick(AggKind::Sum), Agg::from_row(3750.0, 2500.0, 5000.0, 2.0).pick(AggKind::Sum)), (7500.0, 7500.0), "a stored row gives the same total back");
        assert_eq!(pts["decode_tok_s"], 420.0);
        assert_eq!(pts["prompt_tok_s"], 1000.0);
        assert_eq!(pts["req_per_min"], 600.0);
        assert_eq!(pts["gpu0_temp_c"], 53.0);
        assert_eq!(pts["gpu_power_total_w"], 300.0);
        assert_eq!((pts["gpu0_thr_power"], pts["gpu0_thr_thermal"], pts["gpu1_thr_thermal"]), (1.0, 0.0, 1.0));
        assert!(!pts.contains_key("gpu1_temp_c"), "[N/A] is no point, not a zero");
        // an engine restart (counter went back) and a long gap are not rates
        let restarted = sample(1010, 0.0, 0.0, 50.0);
        assert!(!sample_points(Some(&b), &restarted).iter().any(|(n, _)| n == "prompt_tok_s"));
        let late = sample(2000, 0.0, 0.0, 99_000.0);
        assert!(!sample_points(Some(&b), &late).iter().any(|(n, _)| n == "prompt_tok_s"));
        // a sample stored by an older binary has no value for a counter added since (it reads
        // back as 0): the counter's whole life is NOT one step's growth
        let mut upgraded = sample(1015, 0.0, 0.0, 12_000.0);
        if let Some(m) = upgraded.metrics.as_mut() {
            m.counters_v = crate::prom::COUNTERS_V;
            m.cached_tokens_total = 772_018_048.0;
            m.prefill_compute_tokens_total = 3_273_728.0;
        }
        let across: BTreeMap<String, f64> = sample_points(Some(&b), &upgraded).into_iter().collect();
        assert!(!across.contains_key("tok_cached") && !across.contains_key("tok_prompt") && !across.contains_key("prefill_tok_s"), "{across:?}");
        assert_eq!(across.get("tok_prefill"), Some(&0.0));
        // the next sample of the new binary diffs normally
        let mut next = upgraded.clone();
        next.ts += 5;
        if let Some(m) = next.metrics.as_mut() {
            m.cached_tokens_total += 4_000.0;
        }
        assert_eq!(sample_points(Some(&upgraded), &next).into_iter().collect::<BTreeMap<_, _>>().get("tok_cached"), Some(&4_000.0));
    }

    #[test]
    fn unreported_metrics_are_never_fabricated_zeros() {
        // card #180 gate 3 (lss-builder-3, measured on a real Ollama): a plain `f64` metric an
        // engine never publishes is still 0.0 in the struct, and this function used to push
        // that 0.0 into the series as a real point - `lss load`/`lss latency` and `/series`
        // then drew a fabricated flat-zero line for decode_tok_s, kv_usage, cache_hit_rate,
        // running/queue and spec_accept, exactly as observed against a real, minimal engine.
        let not_reported = ["running", "queued", "kv_usage", "decode_tok_s", "cache_hit", "spec", "tokens", "requests", "e2e", "prefill_tok_s"].map(String::from).to_vec();
        // slots IS reported here (a plausible mix: an engine can publish its slot count without
        // publishing how many are running), which is exactly why busy_slots below must still
        // be gated on "running" too, not "slots" alone.
        let a = Sample { ts: 1000, serve_up: true, slots: 8, metrics: Some(ServeMetrics::default()), not_reported: not_reported.clone(), ..Default::default() };
        let b = Sample { ts: 1005, serve_up: true, slots: 8, metrics: Some(ServeMetrics::default()), not_reported, ..Default::default() };
        let pts: BTreeMap<String, f64> = sample_points(Some(&a), &b).into_iter().collect();
        for absent in ["decode_tok_s", "running", "running_public", "running_trusted", "queue", "queue_public", "queue_trusted", "busy_wait", "kv_usage", "cache_hit_rate", "spec_accept_length", "spec_accept_rate", "busy_slots", "prompt_tok_s", "gen_tok_s", "tok_gen", "tok_prompt", "tok_cached", "tok_prefill", "prefill_tok_s", "req_per_min", "sum_requests", "sum_e2e_s", "sum_prefill_s"] {
            assert!(!pts.contains_key(absent), "{absent} must be absent, not a fabricated 0.0 - {pts:?}");
        }
        // still present: serve_up itself (kv_retracted has no not_reported category of its own)
        assert!(pts.contains_key("serve_up"));
        // once the engine DOES report them, the same fields flow through normally
        let mut c = b.clone();
        c.ts = 1010;
        c.not_reported.clear();
        if let Some(m) = c.metrics.as_mut() {
            m.gen_throughput = 420.0;
            m.running = 3.0;
            m.requests_total = 2.0; // 2 requests finished in these 5 s
            m.e2e_sum = 1.5;
            m.prefill_forward_sum = 0.25;
        }
        let pts2: BTreeMap<String, f64> = sample_points(Some(&b), &c).into_iter().collect();
        assert_eq!((pts2.get("decode_tok_s"), pts2.get("running")), (Some(&420.0), Some(&3.0)));
        assert_eq!((pts2.get("req_per_min"), pts2.get("sum_requests")), (Some(&24.0), Some(&2.0)), "a reported request counter flows through - {pts2:?}");
        assert_eq!((pts2.get("sum_e2e_s"), pts2.get("sum_prefill_s")), (Some(&1.5), Some(&0.25)), "{pts2:?}");
        assert!(pts2.contains_key("busy_slots"), "reported + slots > 0 flows through - {pts2:?}");
    }

    #[test]
    fn the_roller_downsamples_on_write() {
        let mut m1 = Roller::new(RES_1M);
        let mut out = Vec::new();
        // two full minutes of 5 s points, then the first point of a third
        for k in 0..25 {
            let ts = 6000 + k * 5;
            let v = if k < 12 { k as f64 } else { 100.0 };
            out.extend(m1.add(ts, &[("x".to_string(), v)]));
        }
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].res, out[0].ts, out[1].ts), (60, 6000, 6060));
        let a = out[0].rows[0].1;
        assert_eq!((a.n, a.min, a.max, a.avg()), (12.0, 0.0, 11.0, 5.5));
        assert_eq!(out[1].rows[0].1.avg(), 100.0);
        let tail = m1.flush().unwrap();
        assert_eq!((tail.ts, tail.rows[0].1.n), (6120, 1.0));
        assert_eq!(m1.flush(), None);
    }

    #[test]
    fn auto_step_picks_the_tier_and_caps_the_points() {
        let now = 1_789_820_003;
        let want = [(900, Tier::Raw, 5, 180), (3600, Tier::Raw, 10, 360), (21_600, Tier::M1, 60, 360), (DAY, Tier::M1, 180, 480), (7 * DAY, Tier::M10, 1200, 504)];
        for (range, tier, step, points) in want {
            let p = plan(now, range, None, KEEP);
            assert_eq!((p.tier, p.step_s, p.points), (tier, step, points), "range {range}");
            assert!(p.points <= MAX_POINTS);
            assert_eq!(p.start_ts % p.step_s, 0, "buckets sit on step boundaries");
            assert!(p.start_ts + p.points as i64 * p.step_s > now, "the newest bucket holds now");
        }
        // an explicit step is honoured when it fits, raised when it would break the cap
        assert_eq!(plan(now, 3600, Some(60), KEEP).step_s, 60);
        assert_eq!(plan(now, 3600, Some(60), KEEP).tier, Tier::M1);
        let forced = plan(now, 7 * DAY, Some(5), KEEP);
        assert_eq!((forced.tier, forced.step_s), (Tier::M10, 1200));
        assert!(plan(now, 400 * DAY, None, KEEP).range_s == 90 * DAY, "nothing older than the 10m tier exists");
        for range in (60..8 * DAY).step_by(7919) {
            for step in [None, Some(1), Some(5), Some(60), Some(3600)] {
                let p = plan(now, range, step, KEEP);
                assert!(p.points <= MAX_POINTS && p.points >= 1 && p.step_s % p.tier.res() == 0, "{range} {step:?} {p:?}");
            }
        }
    }

    #[test]
    fn series_are_aligned_and_each_token_picks_its_own_aggregate() {
        let p = plan(6_000_000 + 7, 600, Some(60), KEEP);
        assert_eq!((p.points, p.step_s, p.start_ts), (10, 60, 6_000_000 + 60 - 600));
        let tokens: Vec<String> = ["queue", "queue:avg", "decode_tok_s", "nope"].iter().map(|s| s.to_string()).collect();
        let mut b = SeriesBuilder::new(p, &tokens);
        assert_eq!(b.names(), vec!["decode_tok_s", "nope", "queue"]);
        // two raw points in the same step, one in the last step, one outside the window
        b.add("queue", p.start_ts + 5, Agg::one(2.0));
        b.add("queue", p.start_ts + 50, Agg::one(8.0));
        b.add("queue", p.start_ts + 9 * 60 + 59, Agg::from_row(3.0, 1.0, 9.0, 12.0));
        b.add("queue", p.start_ts - 1, Agg::one(99.0));
        b.add("queue", p.start_ts + 600, Agg::one(99.0));
        b.add("decode_tok_s", p.start_ts + 61, Agg::one(400.0));
        let doc = b.finish(6_000_007);
        assert_eq!(doc.unknown, vec!["nope"]);
        for s in doc.series.values() {
            assert_eq!(s.len(), 10, "every series has exactly `points` entries");
        }
        assert_eq!(doc.get("queue")[0], Some(8.0), "queue defaults to max: the spike is the point");
        assert_eq!(doc.get("queue:avg")[0], Some(5.0));
        assert_eq!((doc.get("queue")[9], doc.get("queue:avg")[9]), (Some(9.0), Some(3.0)));
        assert_eq!(doc.get("decode_tok_s")[1], Some(400.0));
        assert_eq!(doc.get("decode_tok_s")[0], None, "no data is null, never zero");
        assert_eq!(doc.ts_of(1), p.start_ts + 60);
        assert_eq!(doc.last("queue"), Some(9.0));
        assert_eq!((doc.tier.as_str(), doc.v), ("1m", 1));
    }

    #[test]
    fn hist_columns_follow_the_tier() {
        let now = 1_789_820_003;
        assert_eq!(hist_plan(now, 900, 14 * DAY).1, 60);
        assert_eq!(hist_plan(now, 3600, 14 * DAY), (60, 60, now - now % 60 + 60 - 3600, 60));
        let (res, step, _, cols) = hist_plan(now, 7 * DAY, 14 * DAY);
        assert_eq!((res, step % 600, cols <= 120), (600, 0, true));
    }

    fn full_coverage(log: &crate::gatelog::LogDelta) -> CodeCoverage {
        let mut c = CodeCoverage::default();
        c.observe(1, log);
        c
    }

    #[test]
    fn gateway_tables_take_the_probe_out_of_lanes_keys_and_addresses() {
        let (log, _) = crate::gatelog::ingest(include_str!("../../../fixtures/gate_log.txt"), None);
        let cov = full_coverage(&log);
        assert_eq!((cov.label(), cov.since()), ("full", None));
        let doc = build_gateway(10, 3600, &log, &[(5, 200), (6, 200), (7, 499), (8, 200)], &cov, Some("127.0.0.x"));
        assert_eq!(doc.probes_excluded, 3, "there is no 499 in the trusted log to take out");
        let trusted = &doc.lanes[1];
        assert_eq!((trusted.name.as_str(), trusted.requests, trusted.c2xx), ("trusted", 15, Some(6)));
        assert_eq!((trusted.s413, trusted.s429, trusted.est_tokens_n, trusted.est_tokens_max), (Some(1), Some(2), 3, Some(1_150_022.0)));
        let public = &doc.lanes[0];
        assert_eq!((public.requests, public.c2xx, public.c4xx, public.s499), (24, Some(14), Some(10), Some(2)));
        assert_eq!((doc.keys[0].name.as_str(), doc.keys[0].requests, doc.keys[0].coded_requests, doc.keys[0].c2xx), (NO_KEY, 15, 15, Some(6)));
        assert_eq!((doc.keys[1].name.as_str(), doc.keys[1].lane.as_str(), doc.keys[1].requests), ("key-a", "public", 14));
        assert!(doc.ips.iter().all(|r| r.name.ends_with(".x") && r.c2xx.is_none()), "{:?}", doc.ips);
        // the probe comes from 127.0.0.x on the trusted lane: those three requests are not a client
        let with_probe = build_gateway(10, 3600, &log, &[], &cov, Some("127.0.0.x"));
        let local = |d: &GatewayDoc| d.ips.iter().find(|r| r.name == "127.0.0.x").map(|r| r.requests);
        assert_eq!(local(&with_probe).zip(local(&doc)).map(|(a, b)| a - b), Some(3));
        // an address that was ONLY the probe disappears from the table
        let only_probe = crate::gatelog::LogDelta { trusted: crate::gatelog::LaneLog { requests: 2, by_status: [(200, 2)].into(), by_key: [(NO_KEY.to_string(), 2)].into(), by_key_status: [(NO_KEY.to_string(), [(200, 2)].into())].into(), by_ip: [("127.0.0.x".to_string(), 2)].into(), ..Default::default() }, ..Default::default() };
        let d = build_gateway(10, 3600, &only_probe, &[(5, 200), (6, 200)], &full_coverage(&only_probe), Some("127.0.0.x"));
        assert!(d.ips.is_empty() && d.keys.is_empty(), "{d:?}");
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"2xx\":") && json.contains("\"429\":") && json.contains("\"codes_coverage\":\"full\""));
    }

    #[test]
    fn a_status_breakdown_that_is_not_known_is_null_never_zero() {
        use crate::gatelog::{LaneLog, LogDelta};
        // before the upgrade the gate log was stored with per-key COUNTS only
        // (`gone` is a key that was only ever seen before the upgrade)
        let old = |n: u64, gone: u64| LogDelta { public: LaneLog { requests: n, by_status: [(200, n)].into(), by_key: [("acme".to_string(), n - gone), ("gone".to_string(), gone)].into(), ..Default::default() }, ..Default::default() };
        let new = |ok: u64, rejected: u64| LogDelta {
            public: LaneLog { requests: ok + rejected, by_status: [(200, ok), (429, rejected)].into(), by_key: [("acme".to_string(), ok + rejected)].into(), by_key_status: [("acme".to_string(), [(200, ok), (429, rejected)].into())].into(), by_ip: [("203.0.113.x".to_string(), ok + rejected)].into(), ..Default::default() },
            ..Default::default()
        };
        let mut cov = CodeCoverage::default();
        let mut log = LogDelta::default();
        for (ts, d) in [(1000, old(1000, 1)), (1600, old(761, 0)), (2200, LogDelta::default()), (2800, new(150, 9)), (3400, new(0, 0)), (4000, new(9, 0))] {
            cov.observe(ts, &d);
            log.merge(&d);
        }
        assert_eq!((cov.label(), cov.since()), ("partial", Some(2800)));
        let doc = build_gateway(5000, 86_400, &log, &[], &cov, None);
        assert_eq!((doc.codes_coverage.as_str(), doc.codes_since), ("partial", Some(2800)));
        // the lane totals were always stored with their codes: complete
        assert_eq!((doc.lanes[0].requests, doc.lanes[0].c2xx, doc.lanes[0].s429), (1929, Some(1920), Some(9)));
        // `acme`: 1 928 requests, a breakdown for the 168 since 2800 - shown as such, not as "159 of 1 928 were 2xx"
        let acme = &doc.keys[0];
        assert_eq!((acme.name.as_str(), acme.requests, acme.coded_requests, acme.c2xx, acme.s429, acme.c5xx), ("acme", 1928, 168, Some(159), Some(9), Some(0)));
        // `gone` was only seen before the upgrade: its codes are unknown
        let gone = &doc.keys[1];
        assert_eq!((gone.requests, gone.coded_requests, gone.c2xx, gone.c4xx, gone.c5xx, gone.s413, gone.s503), (1, 0, None, None, None, None, None));
        let json = serde_json::to_value(gone).unwrap();
        assert!(json["2xx"].is_null() && json["429"].is_null() && json["requests"] == 1, "{json}");

        // nothing covered at all: every key is unknown; everything covered: real zeros stay zeros
        let mut none = CodeCoverage::default();
        none.observe(1000, &old(10, 1));
        assert_eq!((none.label(), none.since()), ("none", None));
        assert!(build_gateway(5000, 3600, &old(10, 1), &[], &none, None).keys.iter().all(|k| k.c2xx.is_none() && k.requests > 0));
        let fresh = new(5, 0);
        let all = build_gateway(5000, 3600, &fresh, &[], &full_coverage(&fresh), None);
        assert_eq!((all.keys[0].c2xx, all.keys[0].c5xx, all.keys[0].coded_requests), (Some(5), Some(0), 5));
        // coverage that is interrupted again (an old collector ran for a while) restarts after the gap
        let mut broken = CodeCoverage::default();
        for (ts, d) in [(1, new(1, 0)), (2, old(5, 0)), (3, new(1, 0))] {
            broken.observe(ts, &d);
        }
        assert_eq!(broken.since(), Some(3));
    }

    #[test]
    fn probes_from_before_the_coverage_are_not_taken_out_of_codes_that_never_held_them() {
        use crate::gatelog::{LaneLog, LogDelta};
        let lane = |n: u64, coded: bool| LaneLog {
            requests: n,
            by_status: [(200, n)].into(),
            by_key: [(NO_KEY.to_string(), n)].into(),
            by_key_status: if coded { [(NO_KEY.to_string(), [(200, n)].into())].into() } else { Default::default() },
            by_ip: if coded { [("127.0.0.x".to_string(), n)].into() } else { Default::default() },
            ..Default::default()
        };
        let mut cov = CodeCoverage::default();
        let mut log = LogDelta::default();
        for (ts, d) in [(100, LogDelta { trusted: lane(10, false), ..Default::default() }), (200, LogDelta { trusted: lane(4, true), ..Default::default() })] {
            cov.observe(ts, &d);
            log.merge(&d);
        }
        // 8 probes before the coverage starts, 2 after
        let probes: Vec<(i64, u16)> = (0..8).map(|k| (100 + k, 200)).chain([(200, 200), (201, 200)]).collect();
        let doc = build_gateway(300, 3600, &log, &probes, &cov, Some("127.0.0.x"));
        assert_eq!((doc.probes_excluded, doc.lanes[1].requests), (10, 4));
        let row = &doc.keys[0];
        assert_eq!((row.requests, row.coded_requests, row.c2xx), (4, 2, Some(2)), "only the 2 covered probes leave the coded part");
        assert_eq!(doc.ips[0].requests, 2);
    }

    #[test]
    fn uptime_counts_only_what_the_collector_saw() {
        let now = 100 * DAY;
        let inc = |start: i64, end: Option<i64>, kind: &str| Incident { id: 0, start, end, kind: kind.into(), detail: String::new() };
        let incidents = vec![
            inc(now - 3600, Some(now - 3600 + 864), "serve_down"), // 1% of a day
            inc(now - 3 * DAY, Some(now - 3 * DAY + 6048), "serve_down"), // 1% of a week
            inc(now - 2 * DAY, Some(now - 2 * DAY + 5000), "gate_down"),
            inc(now - 10 * DAY, Some(now - 9 * DAY), "serve_down"),
        ];
        let u = uptime(now, 0, &incidents);
        assert_eq!(u.h24, Some(0.99));
        assert_eq!(u.d7, Some(1.0 - (864.0 + 6048.0) / (7.0 * DAY as f64)).map(|v: f64| (v * 100_000.0).round() / 100_000.0));
        // still open, and the collector is only two hours old: the window is those two hours
        let young = uptime(now, now - 7200, &[inc(now - 1800, None, "serve_down")]);
        assert_eq!((young.h24, young.d7), (Some(0.75), Some(0.75)));
        assert_eq!(uptime(now, now, &[]).h24, None);
    }
}

/// card #211: users on THIS machine vs every machine the gateway fronts (the gate's v5.10
/// `by_upstream`, card #209). Fixture: user A busy only on box-x, user B only on box-y, and the
/// benchmark on box-x (never a user).
#[cfg(test)]
mod upstream_tests {
    use super::sample_points;
    use crate::gate::parse_gate_health;
    use crate::model::Sample;
    use crate::users::user_points;
    use std::collections::BTreeMap;

    const V510: &str = include_str!("../../../fixtures/gate_health_v510.json");
    const V59: &str = include_str!("../../../fixtures/gate_health_v59.json");

    fn points(json: &str, engine_model: Option<&str>) -> BTreeMap<String, f64> {
        let gate = parse_gate_health(json).expect("the fixture parses");
        let cur = Sample { ts: 1_789_820_000, serve_up: true, model: engine_model.map(String::from), users: user_points(&gate), gate: Some(gate), ..Default::default() };
        sample_points(None, &cur).into_iter().collect()
    }

    #[test]
    fn a_user_busy_only_on_one_machine_counts_only_there_and_the_flat_figure_is_the_sum() {
        let p = points(V510, None);
        assert_eq!(p.get("users_active.box-x"), Some(&1.0), "A is on box-x only (the bench on box-x is not a user): {p:?}");
        assert_eq!(p.get("users_active.box-y"), Some(&1.0), "B is on box-y only: {p:?}");
        assert_eq!(p.get("users_inflight.box-x"), Some(&2.0), "{p:?}");
        assert_eq!(p.get("users_inflight.box-y"), Some(&3.0), "{p:?}");
        assert_eq!(p["users_active"], p["users_active.box-x"] + p["users_active.box-y"], "flat = the cross-machine sum");
        assert_eq!(p["users_inflight"], p["users_inflight.box-x"] + p["users_inflight.box-y"]);
    }

    #[test]
    fn this_machine_is_the_upstream_serving_our_engines_model_else_the_first_configured() {
        // our engine serves model-b -> box-y is this machine, although it is not first
        let p = points(V510, Some("model-b"));
        assert_eq!((p.get("users_active_here"), p.get("users_inflight_here")), (Some(&1.0), Some(&3.0)), "{p:?}");
        // engine model unknown -> the gate's FIRST configured upstream (card #210's rule)
        let p = points(V510, None);
        assert_eq!((p.get("users_active_here"), p.get("users_inflight_here")), (Some(&1.0), Some(&2.0)), "box-x: {p:?}");
        // "first configured" is the gate's ORDER, not the alphabet
        let zeta_first = r#"{"version":"v5.10","upstreams":{"zeta":{"ok":true,"loaded":["m-z"]},"alpha":{"ok":true,"loaded":["m-a"]}},"totals":{"inflight":0,"inflight_by_upstream":{}}}"#;
        let g = parse_gate_health(zeta_first).unwrap();
        assert_eq!(g.this_upstream(None), Some("zeta"));
        assert_eq!(g.this_upstream(Some("m-a")), Some("alpha"));
        assert_eq!(g.this_upstream(Some("not-served-anywhere")), Some("zeta"));
        // an idle machine reads an honest 0, not "no data"
        assert_eq!(points(zeta_first, None).get("users_active.zeta"), Some(&0.0));
    }

    #[test]
    fn a_v59_gate_still_parses_and_claims_no_per_machine_figure() {
        let g = parse_gate_health(V59).expect("a pre-v5.10 /gate/health still deserializes");
        assert!(g.attributed_upstreams.is_none());
        assert!(g.users.as_ref().unwrap().iter().all(|u| u.by_upstream.is_empty()));
        assert_eq!(g.this_upstream(Some("model-a")), None, "too old to say which machine");
        let p = points(V59, Some("model-a"));
        assert_eq!(p.get("users_active"), Some(&2.0), "the flat cross-machine count is still there: {p:?}");
        assert!(!p.keys().any(|k| k.starts_with("users_active.") || k.starts_with("users_inflight.") || k.ends_with("_here")), "no per-machine series from a gate that cannot split: {p:?}");
    }

    #[test]
    fn the_benchmark_is_taken_out_of_the_per_machine_totals_too() {
        let g = parse_gate_health(V510).unwrap();
        let st = crate::users::build_users(Some(&g), &[], &BTreeMap::new(), &crate::users::OwnTraffic::default(), 1_789_820_000);
        let split = st.totals.inflight_by_upstream.clone().expect("a v5.10 gate splits");
        assert_eq!(split.get("box-x"), Some(&2), "box-x had 3 in flight, 1 of them the bench: {split:?}");
        assert_eq!(split.get("box-y"), Some(&3));
        assert_eq!(st.totals.inflight, 5);
    }
}
