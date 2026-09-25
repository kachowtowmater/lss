//! The read side of the history API: `GET /series`, `/hist`, `/gateway`. Each HTTP thread owns
//! a read-only database connection, so nothing here can block the poll loop.

use crate::db::{Db, Retention};
use lss_core::gatelog::LogDelta;
use lss_core::hist::{hist_index, hist_unit, latency_index, HistAccum};
use lss_core::series::{build_gateway, CodeCoverage, hist_plan, is_minute_native, is_probe_metric, plan, parse_range, sample_points, Agg, HistDoc, SeriesBuilder, Tier, RANGES};
use std::collections::{HashMap, HashSet};

pub const MAX_TOKENS: usize = 128;
const DEFAULT_RANGE: i64 = 3600;

pub type Params = HashMap<String, String>;

/// `a=1&b=x%2Cy` -> map. Only what this API needs: `%XX` and `+`.
pub fn parse_query(url: &str) -> Params {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (!k.is_empty()).then(|| (percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn bad(msg: &str) -> (u16, String) {
    (400, format!("{}\n", serde_json::json!({"v": 1, "error": msg})))
}

fn db_err(e: rusqlite::Error) -> (u16, String) {
    (500, format!("{}\n", serde_json::json!({"v": 1, "error": format!("database: {e}")})))
}

fn range_of(q: &Params) -> Result<i64, (u16, String)> {
    match q.get("range") {
        None => Ok(DEFAULT_RANGE),
        Some(r) => parse_range(r).ok_or_else(|| bad("range must look like 15m, 1h, 6h, 24h, 7d (or plain seconds)")),
    }
}

/// card #281 (redesign R2c): the three page-1 trend series `/status` cannot build from its
/// in-memory hour, filled onto its own grid (`series.start_ts`, `step_s`, `points`):
/// - `ttft_p99_ms` / `itl_p99_ms`: the SAME minute rollups `/series` serves under those names
///   (only the collector holds the engine's histograms). A minute's p99 fills both of that
///   minute's 30 s buckets; a minute with no rollup stays null.
/// - `usd_per_hour`: the bucket's MEAN total GPU power, `power_mean_w` (`History::power_mean_w`,
///   card #283: energy over the bucket, not the max one spiky sample sets) priced at the rate in force at the bucket's own midpoint - the
///   same `rates::usd_per_kwh_at` the live "$/h now" figure uses. No rates table = all null,
///   never a guessed price.
///
/// A database error leaves a series null (an empty chart), never a partial fabrication.
pub fn fill_status_series(db: &Db, sr: &mut lss_core::model::Series, rates: Option<&lss_core::rates::RateTable>, power_mean_w: &[Option<f64>]) {
    let (start, step, n) = (sr.start_ts, sr.step_s.max(1), sr.points);
    let end = start + n as i64 * step;
    for (metric, out) in [("ttft_p99_ms", &mut sr.ttft_p99_ms), ("itl_p99_ms", &mut sr.itl_p99_ms)] {
        out.clear();
        out.resize(n, None);
        let mut rows: Vec<(i64, Agg)> = Vec::new();
        if db.rollup_rows(60, metric, start - 60, end, |ts, a| rows.push((ts, a))).is_err() {
            continue;
        }
        for (ts, a) in rows {
            if a.n <= 0.0 {
                continue;
            }
            let v = (a.sum / a.n * 10.0).round() / 10.0;
            // every bucket whose start falls inside this minute
            let first = (ts - start).div_euclid(step).max(0);
            let last = (ts + 60 - 1 - start).div_euclid(step).min(n as i64 - 1);
            for i in first..=last {
                if let Some(slot) = out.get_mut(i as usize) {
                    *slot = Some(v);
                }
            }
        }
    }
    sr.usd_per_hour = match rates {
        Some(table) => (0..n)
            .map(|i| {
                let watts = power_mean_w.get(i).copied().flatten()?;
                let (price, _) = lss_core::rates::usd_per_kwh_at(table, start + i as i64 * step + step / 2)?;
                Some((watts / 1000.0 * price * 10_000.0).round() / 10_000.0)
            })
            .collect(),
        None => vec![None; n],
    };
}

/// card #284: the collector's /status document, assembled in ONE place (main.rs calls this, and
/// only this): lss-core's `build_status`, then the series only the collector can fill
/// (`fill_status_series`), with `$/h` priced on the inputs' own history's MEAN bucket power
/// (`History::power_mean_w`, #283) - never the max `series.gpu_power_w` the power chart shows.
pub fn assemble_status(db: &Db, i: &lss_core::history::StatusInputs, rates: Option<&lss_core::rates::RateTable>) -> lss_core::model::Status {
    let mut status = lss_core::history::build_status(i);
    fill_status_series(db, &mut status.series, rates, &i.history.power_mean_w(i.now));
    status
}

/// `GET /series?metrics=a,b:max&range=6h&step=auto`
pub fn series(db: &Db, now: i64, q: &Params, keep: Retention) -> (u16, String) {
    let tokens: Vec<String> = q.get("metrics").map(|m| m.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()).unwrap_or_default();
    if tokens.is_empty() {
        let mut names = db.metric_names().unwrap_or_default();
        names.extend(["c1_tok_s", "c1_invalid", "c1_invalid_tok_s"].map(String::from));
        names.sort();
        return (200, format!("{}\n", serde_json::json!({"v": 1, "metrics": names, "ranges": RANGES, "aggregates": ["avg", "min", "max"], "max_points": lss_core::series::MAX_POINTS})));
    }
    if tokens.len() > MAX_TOKENS {
        return bad("too many metrics in one request (128 at most)");
    }
    let range = match range_of(q) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let step = match q.get("step").map(String::as_str) {
        None | Some("auto") | Some("") => None,
        Some(s) => match parse_range(s) {
            Some(v) => Some(v),
            None => return bad("step must be `auto` or a duration like 60, 60s, 10m"),
        },
    };
    let p = plan(now, range, step, (keep.raw, keep.m1, keep.m10));
    let mut b = SeriesBuilder::new(p, &tokens);
    let end = p.start_ts + p.points as i64 * p.step_s;
    let names = b.names();
    let recorded: HashSet<String> = db.metric_names().unwrap_or_default().into_iter().collect();
    let mut from_samples: HashSet<&str> = HashSet::new();
    for name in &names {
        if is_probe_metric(name) {
            b.mark_known(name);
            continue;
        }
        let res = match p.tier {
            Tier::Raw if !is_minute_native(name) => {
                if recorded.contains(name) {
                    b.mark_known(name);
                }
                from_samples.insert(name.as_str());
                continue;
            }
            Tier::Raw | Tier::M1 => 60,
            Tier::M10 => 600,
        };
        let mut rows: Vec<(i64, Agg)> = Vec::new();
        match db.rollup_rows(res, name, p.start_ts, end, |ts, a| rows.push((ts, a))) {
            Ok(true) => b.mark_known(name),
            Ok(false) => {}
            Err(e) => return db_err(e),
        }
        for (ts, a) in rows {
            b.add(name, ts, a);
        }
    }
    if !from_samples.is_empty() {
        let mut prev = None;
        let mut points = Vec::new();
        // one sample earlier than the window, so the first rate has something to diff against
        let r = db.each_sample(p.start_ts - 10, end, |s| {
            for (name, v) in sample_points(prev.as_ref(), &s) {
                if s.ts >= p.start_ts && from_samples.contains(name.as_str()) {
                    points.push((name, s.ts, v));
                }
            }
            prev = Some(s);
        });
        if let Err(e) = r {
            return db_err(e);
        }
        for (name, ts, v) in points {
            b.add(&name, ts, Agg::one(v));
        }
    }
    if names.iter().any(|n| is_probe_metric(n)) {
        let probes = match db.probes_between(p.start_ts, end) {
            Ok(p) => p,
            Err(e) => return db_err(e),
        };
        for pr in probes {
            match (pr.is_reading(), pr.decode_tok_s) {
                (true, Some(v)) => b.add("c1_tok_s", pr.ts, Agg::one(v)),
                (false, v) => {
                    b.add("c1_invalid", pr.ts, Agg::one(1.0));
                    if let Some(v) = v {
                        b.add("c1_invalid_tok_s", pr.ts, Agg::one(v));
                    }
                }
                _ => {}
            }
        }
    }
    (200, serde_json::to_string(&b.finish(now)).unwrap_or_else(|_| "{}".into()) + "\n")
}

/// `GET /hist?metric=ttft&range=1h`
pub fn hist(db: &Db, now: i64, q: &Params, keep: Retention) -> (u16, String) {
    let Some(metric) = q.get("metric").filter(|m| hist_index(m).is_some()) else {
        return bad("metric must be one of ttft, e2e, itl, queue_time, prompt_tokens, gen_tokens");
    };
    let range = match range_of(q) {
        Ok(r) => r.min(keep.m10),
        Err(e) => return e,
    };
    let (res, step, start, cols) = hist_plan(now, range, keep.m1);
    let mut rows: Vec<(i64, HistAccum)> = Vec::new();
    if let Err(e) = db.hist_rows(res, metric, start, start + cols as i64 * step, |ts, acc| rows.push((ts, acc))) {
        return db_err(e);
    }
    // the bucket layout in force at the end of the range is the one that is drawn
    let le = rows.last().map(|(_, a)| a.le.clone()).unwrap_or_default();
    let mut total = HistAccum { le: le.clone(), counts: vec![0.0; le.len() + 1], ..Default::default() };
    let mut columns: Vec<Vec<f64>> = vec![Vec::new(); cols];
    for (ts, acc) in rows.iter().filter(|(_, a)| a.le == le) {
        total.add(acc);
        let idx = ((ts - start) / step).clamp(0, cols as i64 - 1) as usize;
        if columns[idx].is_empty() {
            columns[idx] = acc.counts.clone();
        } else {
            for (a, b) in columns[idx].iter_mut().zip(&acc.counts) {
                *a += b;
            }
        }
    }
    let doc = HistDoc { v: lss_core::STATUS_SCHEMA_VERSION, generated_at: now, metric: metric.clone(), range_s: range, step_s: step, start_ts: start, summary: latency_index(metric).and_then(|_| total.summary()), unit: hist_unit(metric).to_string(), summary_raw: total.summary_raw(), total: total.counts, le, columns };
    (200, serde_json::to_string(&doc).unwrap_or_else(|_| "{}".into()) + "\n")
}

/// `GET /gateway?range=1h`. Up to an hour the stored samples are merged exactly; beyond that
/// the 10-minute gate-log rollup is, plus the bucket still open in the poll loop. `probe_ip` =
/// the masked address the collector's own probe reaches the gate from.
pub fn gateway(db: &Db, now: i64, q: &Params, keep: Retention, open_bucket: Option<&(i64, LogDelta)>, probe_ip: Option<&str>) -> (u16, String) {
    let range = match range_of(q) {
        Ok(r) => r.min(keep.m10),
        Err(e) => return e,
    };
    let from = now - range;
    let mut log = LogDelta::default();
    // which part of the range has per-key status codes at all (data from before 2026-09-19 has none)
    let mut coverage = CodeCoverage::default();
    let mut fold = |ts: i64, d: &LogDelta| {
        coverage.observe(ts, d);
        log.merge(d);
    };
    let merged = if range <= 3600 {
        db.each_sample(from + 1, now + 1, |s| fold(s.ts, &s.log))
    } else {
        db.each_gatelog(from - from.rem_euclid(600), now + 1, |ts, d| fold(ts, &d)).map(|()| {
            if let Some((ts, open)) = open_bucket {
                fold(*ts, open);
            }
        })
    };
    if let Err(e) = merged {
        return db_err(e);
    }
    let probes: Vec<(i64, u16)> = match db.probes_between(from + 1, now + 1) {
        Ok(p) => p.iter().filter_map(|p| p.gate_logged_status().map(|code| (p.ts, code))).collect(),
        Err(e) => return db_err(e),
    };
    (200, serde_json::to_string(&build_gateway(now, range, &log, &probes, &coverage, probe_ip)).unwrap_or_else(|_| "{}".into()) + "\n")
}

/// `GET /tokens`: exact token totals of the last hour, day, week and of today, summed from the
/// rollup tiers (so a 7-day total reads ~1000 rows), then ALL TIME from the persistent ledger
/// (which survives serve restarts), the peaks, the request-length distributions of the last
/// 7 days and who the tokens went to.
/// card #23: the two facts `lss tokens` needs besides the numbers (kept as one arg to stay
/// under clippy's arity limit): no gateway configured, and the engine publishes no counters.
#[derive(Default)]
pub struct TokensDocFlags {
    pub gate_absent: bool,
    pub tokens_not_reported: bool,
}

pub fn tokens_doc(db: &Db, now: i64, local_midnight: i64, since: i64, all: &lss_core::tokens::AllTime, users: &lss_core::users::UsersStatus, flags: TokensDocFlags) -> lss_core::tokens::TokensDoc {
    use lss_core::tokens::{peak_by_day, user_tokens, TokenWindow, TokensDoc};
    let sums = |res: i64, from: i64| -> [f64; 4] { ["tok_gen", "tok_prompt", "tok_cached", "sum_requests"].map(|m| db.rollup_sum(res, m, from, now + 1).unwrap_or(0.0)) };
    let mut windows: Vec<TokenWindow> = [("1h", 3600), ("24h", 86_400), ("7d", 7 * 86_400), ("today", (now - local_midnight).max(0))]
        .into_iter()
        .map(|(name, secs)| {
            let [generated, prompt, cached, requests] = if secs <= 86_400 {
                sums(60, now - secs)
            } else {
                // the last day from the 1-minute tier (it is a minute behind, not ten), the days
                // before it from the 10-minute tier: a week is never less than its last day
                let seam = (now - 86_400) - (now - 86_400).rem_euclid(600);
                let (old, new) = (["tok_gen", "tok_prompt", "tok_cached", "sum_requests"].map(|m| db.rollup_sum(600, m, now - secs, seam).unwrap_or(0.0)), sums(60, seam));
                [0, 1, 2, 3].map(|i| old[i] + new[i])
            };
            let mut w = TokenWindow::new(name, secs, generated, prompt, cached, requests);
            // every figure of a row must cover the same span. The cache share: cached over the
            // prompt tokens counted since the cached counter exists. The requests: say from when.
            let tier = if secs <= 86_400 { 60 } else { 600 };
            let first = |m: &str| db.rollup_first_ts(tier, m).ok().flatten();
            if let (Some(fc), Some(fp)) = (first("tok_cached"), first("tok_prompt")) {
                let start = (now - secs).max(fc).max(fp);
                let (c, p) = (db.rollup_sum(tier, "tok_cached", start, now + 1).unwrap_or(0.0), db.rollup_sum(tier, "tok_prompt", start, now + 1).unwrap_or(0.0));
                w.cache_share = (p > 0.0).then(|| ((c / p).min(1.0) * 10_000.0).round() / 10_000.0);
            }
            w.requests_since = match (first("sum_requests"), first("tok_gen")) {
                (Some(fr), Some(fg)) if fr > (now - secs).max(fg) + tier => Some(fr),
                _ => None,
            };
            w
        })
        .collect();
    // ALL TIME = the persistent ledger, plus what the 10-minute history holds from BEFORE the
    // ledger began (the ledger is younger than the history on a collector that was upgraded): an
    // "all time" smaller than "24h" reads as nonsense. Only buckets that ended before the ledger
    // started are added, so nothing is ever counted twice.
    let before = |m: &str| if all.since > 0 { db.rollup_sum(600, m, 0, (all.since - 599).max(0)).unwrap_or(0.0) } else { 0.0 };
    let history_since = db.rollup_first_ts(600, "tok_gen").ok().flatten().filter(|t| all.since == 0 || *t < all.since);
    let all_since = history_since.unwrap_or(all.since);
    let mut all_row = TokenWindow::new("all", (now - all_since).max(0), all.generated.total + before("tok_gen"), all.prompt.total + before("tok_prompt"), all.cached.total + before("tok_cached"), all.requests.total + before("sum_requests"));
    all_row.requests_since = windows.iter().find(|w| w.name == "7d").and_then(|w| w.requests_since).filter(|t| *t > all_since + 600);
    windows.push(all_row);
    let lengths = |metric: &str| {
        let mut total = HistAccum::default();
        let _ = db.hist_rows(600, metric, now - 7 * 86_400, now + 1, |_, h| total.add(&h));
        total.summary_raw()
    };
    TokensDoc {
        v: lss_core::STATUS_SCHEMA_VERSION,
        generated_at: now,
        since,
        windows,
        all_time_since: all_since,
        peak: (all.peak.tok_s > 0.0).then(|| all.peak.clone()),
        peak_by_day: peak_by_day(all, 14),
        output_len: lengths("gen_tokens"),
        prompt_len: lengths("prompt_tokens"),
        per_user_available: users.available,
        per_user: user_tokens(users),
        gate_absent: flags.gate_absent,
        tokens_not_reported: flags.tokens_not_reported,
    }
}

/// The address the gate's USER TABLE files the probe under: the probe talks to `gate_url` from
/// this machine, so a loopback gate URL means `127.0.0.1` / `::1`. None = the URL names a host.
pub fn probe_user_of(gate_url: &str) -> Option<String> {
    let host = gate_url.split("://").nth(1).unwrap_or(gate_url).split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.parse::<u16>().is_ok() { h } else { host }).trim_start_matches('[').trim_end_matches(']');
    match host {
        "localhost" => Some("127.0.0.1".into()),
        h => h.parse::<std::net::IpAddr>().ok().filter(std::net::IpAddr::is_loopback).map(|ip| ip.to_string()),
    }
}

/// The masked address the probe's requests carry in the gate log: the probe talks to
/// `gate_url` from this machine, so a loopback gate URL means `127.0.0.x`. None = the URL names
/// a host, and the address the gate sees cannot be known from here.
pub fn probe_ip_of(gate_url: &str) -> Option<String> {
    let rest = gate_url.split_once("://").map_or(gate_url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    let host = if host.eq_ignore_ascii_case("localhost") { "127.0.0.1" } else { host };
    host.parse::<std::net::IpAddr>().ok().map(|_| lss_core::gatelog::mask_ip(host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::Pipeline;
    use lss_core::model::Sample;
    use lss_core::prom::ServeMetrics;
    use lss_core::series::{SeriesDoc, MAX_POINTS};

    const DAY: i64 = 86_400;
    const KEEP: Retention = Retention { raw: DAY, m1: 14 * DAY, m10: 90 * DAY };
    const T0: i64 = 1_800_000_000; // a round 10-minute boundary

    fn filled(hours: i64) -> (Db, i64) {
        let db = Db::memory();
        let mut pipe = Pipeline::default();
        db.begin().unwrap();
        for k in 0..(hours * 720) {
            let ts = T0 + k * 5;
            let s = Sample { ts, serve_up: true, metrics: Some(ServeMetrics { gen_throughput: (k / 720) as f64 * 100.0, queue: (k % 12) as f64, prompt_tokens_total: k as f64 * 500.0, ..Default::default() }), ..Default::default() };
            db.insert_sample(&s).unwrap();
            pipe.push(&db, &s, None, true);
        }
        db.commit().unwrap();
        (db, T0 + hours * 3600 - 5)
    }

    /// card #283 (lss-verifier-4 on #281): a SPIKY bucket, 100 W, 100 W, then one 1000 W sample.
    /// It reaches the collector as max 1000 W (the power chart's peak) and mean 400 W (history's
    /// own test pins that mean); `$/h` is priced on the MEAN, the energy the bucket actually used,
    /// like the live "$/h now" figure, never on the peak.
    #[test]
    fn dollars_per_hour_is_priced_on_the_buckets_mean_power_not_its_peak() {
        use lss_core::rates::{RatePlan, RateTable};
        let mut sr = lss_core::model::Series { step_s: 30, start_ts: T0, points: 120, ..Default::default() };
        sr.gpu_power_w = vec![None; 120];
        sr.gpu_power_w[7] = Some(1_000.0);
        let mut mean = vec![None; 120];
        mean[7] = Some(400.0);
        let table = RateTable::new("flat".into(), "2026-01-01".into(), RatePlan::Flat { usd_per_kwh: 0.30 }, None, None);
        fill_status_series(&Db::memory(), &mut sr, Some(&table), &mean);
        assert_eq!(sr.usd_per_hour[7], Some(0.12), "mean 400 W at $0.30/kWh = $0.12/h (the peak would read $0.30/h)");
        assert_eq!(sr.gpu_power_w[7], Some(1_000.0), "the power chart keeps the bucket's peak");
    }

    /// card #284 (#283's verifier): the ASSEMBLY main.rs runs, driven end to end - a real
    /// History with one spiky 30 s bucket (100 W, 100 W, then 1000 W) through
    /// `assemble_status`: the published series keeps the 1000 W peak for the power chart and
    /// prices `$/h` on the 400 W mean. Wiring the max series back in goes RED here.
    #[test]
    fn the_collectors_status_assembly_prices_dollars_per_hour_on_the_mean() {
        use lss_core::history::{History, StatusInputs};
        use lss_core::model::Thresholds;
        use lss_core::rates::{RatePlan, RateTable};
        let now = T0 + 7_200;
        let bucket = now - now.rem_euclid(30) - 600; // a whole 30 s bucket, ten minutes back
        let mut history = History::default();
        for (dt, w) in [(0, 100.0), (10, 100.0), (20, 1_000.0)] {
            let gpu = lss_core::gpu::GpuSample { index: 0, power_w: Some(w), ..Default::default() };
            history.push(Sample { ts: bucket + dt, serve_up: true, gpus_ok: true, gpus: vec![gpu], ..Default::default() });
        }
        let table = RateTable::new("flat".into(), "2026-01-01".into(), RatePlan::Flat { usd_per_kwh: 0.30 }, None, None);
        let s = assemble_status(&Db::memory(), &StatusInputs {
            now, host: "gpu-box", collector_version: "0", collector_started_at: now, poll_secs: 5, slots: 8,
            history: &history, incidents: &[], alerts: &[], probes: &[], last_valid_probe: None, firing: vec![], serve_down_since: None,
            gate_down_since: None, restarts_today: 0, thermal_exclude: &[], probe_enabled: false, probe_interval_s: 300,
            c1_baseline: None, c1_baseline_source: String::new(),
            thresholds: Thresholds::new(&lss_core::config::RulesConfig::default(), None, 300), probes_admitted_since_gate_start: 0, latency: None, gpu_health: &[], user_aliases: &[], tailscale: &Default::default(), own_traffic: Default::default(), advice_top: Vec::new(), bench: Default::default(), maintenance: Default::default(), cost: None, spending: None, tokens_hour: None, tokens_day: None, tokens_week: None, tokens_month: None, tokens_hour_covered_secs: None, tokens_day_covered_secs: None, tokens_week_covered_secs: None, tokens_month_covered_secs: None, watch: None, loadout: None, prefill_typical: None, live_decode: None, public_priority: String::new(), trusted_priority: String::new(), targets: Default::default(), user_log_24h: None,
        }, Some(&table));
        let i = ((bucket - s.series.start_ts) / s.series.step_s) as usize;
        assert_eq!(s.series.gpu_power_w[i], Some(1_000.0), "the power chart keeps the bucket's peak");
        assert_eq!(s.series.usd_per_hour[i], Some(0.12), "$/h on the 400 W mean at $0.30/kWh, not the $0.30/h the peak would price");
    }

    /// card #281: `/status`'s trend grid, the part only the collector can fill - the TTFT / ITL
    /// p99 minute rollups placed on the 30 s grid, and $/h priced from the bucket's own power.
    #[test]
    fn the_status_series_gets_the_histogram_p99s_and_the_priced_dollars_per_hour() {
        use lss_core::rates::{RatePlan, RateTable};
        let db = Db::memory();
        let start = T0; // a round minute
        // two stored minutes of TTFT p99 (the minute rollup the /series endpoint serves), one of ITL
        for (ts, metric, v) in [(start, "ttft_p99_ms", 620.0), (start + 120, "ttft_p99_ms", 2_050.0), (start + 60, "itl_p99_ms", 14.2)] {
            db.write_rollup(&lss_core::series::RollupBatch { res: 60, ts, rows: vec![(metric.to_string(), Agg::one(v))] }, true).unwrap();
        }
        let mut sr = lss_core::model::Series { step_s: 30, start_ts: start, points: 120, ..Default::default() };
        sr.gpu_power_w = vec![None; 120];
        sr.gpu_power_w[0] = Some(1_000.0);
        sr.gpu_power_w[5] = Some(500.0);
        let table = RateTable::new("flat".into(), "2026-01-01".into(), RatePlan::Flat { usd_per_kwh: 0.30 }, None, None);
        let flat_power = sr.gpu_power_w.clone(); // no spikes here: the mean IS these values
        fill_status_series(&db, &mut sr, Some(&table), &flat_power);
        assert_eq!((sr.ttft_p99_ms[0], sr.ttft_p99_ms[1]), (Some(620.0), Some(620.0)), "a minute's p99 fills both of its 30 s buckets");
        assert_eq!((sr.ttft_p99_ms[2], sr.ttft_p99_ms[3]), (None, None), "a minute with no rollup stays null - never carried forward");
        assert_eq!((sr.ttft_p99_ms[4], sr.ttft_p99_ms[5]), (Some(2_050.0), Some(2_050.0)));
        assert_eq!((sr.itl_p99_ms[1], sr.itl_p99_ms[2], sr.itl_p99_ms[3]), (None, Some(14.2), Some(14.2)));
        assert_eq!(sr.ttft_p99_ms.len(), 120, "on the status grid, every bucket present");
        assert_eq!(sr.usd_per_hour[0], Some(0.30), "1 kW at $0.30/kWh is $0.30/h");
        assert_eq!(sr.usd_per_hour[5], Some(0.15));
        assert_eq!(sr.usd_per_hour[1], None, "no power reading in the bucket: no price, never $0");
        // no rates table: every $/h bucket is null (the page says "no rates.toml"), never 0
        fill_status_series(&db, &mut sr, None, &[]);
        assert!(sr.usd_per_hour.iter().all(Option::is_none) && sr.usd_per_hour.len() == 120);
        // an empty database (a fresh install, or an engine with no histograms): all null
        let mut fresh = lss_core::model::Series { step_s: 30, start_ts: start, points: 120, ..Default::default() };
        fill_status_series(&Db::memory(), &mut fresh, Some(&table), &[]);
        assert!(fresh.ttft_p99_ms.iter().chain(&fresh.itl_p99_ms).chain(&fresh.usd_per_hour).all(Option::is_none));
    }

    fn get(db: &Db, now: i64, url: &str) -> SeriesDoc {
        let (code, body) = series(db, now, &parse_query(url), KEEP);
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[test]
    fn query_strings() {
        let q = parse_query("/series?metrics=queue%3Amax%2Cdecode_tok_s&range=6h&step=auto&x");
        assert_eq!(q["metrics"], "queue:max,decode_tok_s");
        assert_eq!((q["range"].as_str(), q["step"].as_str(), q["x"].as_str()), ("6h", "auto", ""));
        assert!(parse_query("/series").is_empty());
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn series_are_aligned_capped_and_tiered() {
        let (db, now) = filled(30);
        for (range, tier) in [("15m", "raw"), ("1h", "raw"), ("6h", "1m"), ("24h", "1m"), ("7d", "10m")] {
            let doc = get(&db, now, &format!("/series?metrics=decode_tok_s,queue,queue:avg,prompt_tok_s&range={range}"));
            assert_eq!(doc.tier, tier, "{range}");
            assert!(doc.points <= MAX_POINTS);
            assert_eq!(doc.series.len(), 4);
            for (name, s) in &doc.series {
                assert_eq!(s.len(), doc.points, "{range} {name}: aligned arrays");
            }
            assert!(doc.unknown.is_empty(), "{range} {:?}", doc.unknown);
            assert!(doc.start_ts + doc.points as i64 * doc.step_s > now);
            assert_eq!(doc.last("queue"), Some(11.0), "{range}: max of every bucket");
            // the newest CLOSED minute holds the whole 0..11 sawtooth; a raw bucket holds 1 or 2 samples
            let avg = match range { "15m" => 11.0, "1h" => 10.5, _ => 5.5 };
            assert_eq!(doc.last("queue:avg"), Some(avg), "{range}");
            assert_eq!(doc.last("prompt_tok_s"), Some(100.0), "{range}: 500 tokens per 5 s");
        }
        let day = get(&db, now, "/series?metrics=decode_tok_s&range=24h");
        assert_eq!((day.step_s, day.points), (180, 480));
        assert_eq!(day.last("decode_tok_s"), Some(2900.0));
        // an explicit fine step over a long range is raised to the cap, never refused
        let forced = get(&db, now, "/series?metrics=decode_tok_s&range=24h&step=5");
        assert!(forced.points <= MAX_POINTS && forced.step_s >= 144);
    }

    #[test]
    fn unknown_metrics_are_named_and_bad_requests_are_400() {
        let (db, now) = filled(2);
        let doc = get(&db, now, "/series?metrics=decode_tok_s,no_such_thing&range=1h");
        assert_eq!(doc.unknown, vec!["no_such_thing"]);
        assert!(doc.get("no_such_thing").iter().all(Option::is_none));
        assert_eq!(series(&db, now, &parse_query("/series?metrics=x&range=soon"), KEEP).0, 400);
        assert_eq!(series(&db, now, &parse_query("/series?metrics=x&step=fast"), KEEP).0, 400);
        let many: Vec<String> = (0..130).map(|i| format!("m{i}")).collect();
        assert_eq!(series(&db, now, &parse_query(&format!("/series?metrics={}", many.join(","))), KEEP).0, 400);
        let (code, list) = series(&db, now, &parse_query("/series"), KEEP);
        assert_eq!(code, 200);
        assert!(list.contains("\"decode_tok_s\"") && list.contains("\"c1_tok_s\"") && list.contains("\"7d\""), "{list}");
    }

    #[test]
    fn probes_are_a_series_with_valid_and_invalid_marked() {
        let (db, now) = filled(2);
        let ok = lss_core::probe::ProbeRecord { ts: now - 600, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(191.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
        let mut bad = ok.clone();
        bad.ts = now - 300;
        bad.decode_tok_s = Some(140.0);
        bad.invalidate("contended", "someone else".into());
        db.insert_probe(&ok).unwrap();
        db.insert_probe(&bad).unwrap();
        db.insert_probe(&lss_core::probe::ProbeRecord::skipped_busy(now - 60, 1.0, 0.0)).unwrap();
        let doc = get(&db, now, "/series?metrics=c1_tok_s,c1_invalid,c1_invalid_tok_s&range=1h&step=60");
        assert!(doc.unknown.is_empty());
        let some = |name: &str| doc.get(name).iter().flatten().copied().collect::<Vec<f64>>();
        assert_eq!(some("c1_tok_s"), vec![191.0]);
        assert_eq!(some("c1_invalid"), vec![1.0, 1.0]);
        assert_eq!(some("c1_invalid_tok_s"), vec![140.0]);
    }

    #[test]
    fn hist_and_gateway_answers() {
        let db = Db::memory();
        let now = T0 + 3600;
        let le = vec![0.1, 0.2, 0.4];
        for (k, counts) in [(10, vec![0.0, 8.0, 2.0, 0.0]), (40, vec![0.0, 0.0, 9.0, 1.0])] {
            let acc = HistAccum { le: le.clone(), counts, sum: 3.0, count: 10.0 };
            db.write_hist_window(&lss_core::hist::ClosedWindow { res: 60, ts: T0 + k * 60, accs: [acc, Default::default(), Default::default(), Default::default(), Default::default(), Default::default()] }).unwrap();
        }
        let (code, body) = hist(&db, now, &parse_query("/hist?metric=ttft&range=1h"), KEEP);
        assert_eq!(code, 200);
        let doc: HistDoc = serde_json::from_str(&body).unwrap();
        assert_eq!((doc.le.clone(), doc.total.clone(), doc.step_s), (le, vec![0.0, 8.0, 11.0, 1.0], 60));
        assert_eq!(doc.columns.iter().filter(|c| !c.is_empty()).count(), 2);
        assert_eq!(doc.summary.unwrap().count, 20.0);
        assert_eq!((doc.unit.as_str(), doc.summary_raw.map(|r| r.count)), ("s", Some(20.0)));
        assert_eq!(hist(&db, now, &parse_query("/hist?metric=nope"), KEEP).0, 400);
        // the request-size histograms: tokens, so no millisecond summary
        let mut accs: [HistAccum; 6] = Default::default();
        accs[4] = HistAccum { le: vec![1000.0, 10_000.0], counts: vec![6.0, 4.0, 0.0], sum: 30_000.0, count: 10.0 };
        db.write_hist_window(&lss_core::hist::ClosedWindow { res: 60, ts: T0 + 600, accs }).unwrap();
        let sizes: HistDoc = serde_json::from_str(&hist(&db, now, &parse_query("/hist?metric=prompt_tokens&range=1h"), KEEP).1).unwrap();
        assert_eq!((sizes.unit.as_str(), sizes.summary, sizes.summary_raw.map(|r| (r.avg, r.count))), ("tokens", None, Some((3000.0, 10.0))));
        let empty: HistDoc = serde_json::from_str(&hist(&db, now, &parse_query("/hist?metric=itl"), KEEP).1).unwrap();
        assert!(empty.summary.is_none() && empty.le.is_empty());

        // gateway: an hour from the samples, a day from the 10-minute rollup plus the open bucket
        let (log, _) = lss_core::gatelog::ingest(include_str!("../../../fixtures/gate_log.txt"), None);
        db.insert_sample(&Sample { ts: now - 100, log: log.clone(), ..Default::default() }).unwrap();
        db.write_gatelog(now - 7200, &log, true).unwrap();
        let hour: lss_core::series::GatewayDoc = serde_json::from_str(&gateway(&db, now, &parse_query("/gateway?range=1h"), KEEP, None, None).1).unwrap();
        assert_eq!((hour.lanes[0].requests, hour.codes_coverage.as_str()), (24, "full"));
        let open = (now - 300, log.clone());
        let day: lss_core::series::GatewayDoc = serde_json::from_str(&gateway(&db, now, &parse_query("/gateway?range=24h"), KEEP, Some(&open), None).1).unwrap();
        assert_eq!(day.lanes[0].requests, 48);
        assert_eq!(day.keys.iter().find(|k| k.name == "key-a").map(|k| (k.requests, k.c2xx)), Some((28, Some(16))));

        // a bucket stored by the previous collector (per-key counts, no codes) earlier in the day:
        // the tables say from when the codes count, and a key only seen back then has none
        let mut old = log.clone();
        for lane in [&mut old.public, &mut old.trusted] {
            lane.by_key_status.clear();
            lane.by_ip.clear();
        }
        old.public.by_key.insert("retired-key".into(), 3);
        db.write_gatelog(now - 6 * 3600, &old, true).unwrap();
        let day: lss_core::series::GatewayDoc = serde_json::from_str(&gateway(&db, now, &parse_query("/gateway?range=24h"), KEEP, Some(&open), Some("127.0.0.x")).1).unwrap();
        assert_eq!((day.codes_coverage.as_str(), day.codes_since), ("partial", Some(now - 7200)));
        let key = |name: &str| day.keys.iter().find(|k| k.name == name).cloned().unwrap();
        assert_eq!((key("key-a").requests, key("key-a").coded_requests, key("key-a").c2xx), (42, 28, Some(16)));
        assert_eq!((key("retired-key").requests, key("retired-key").c2xx, key("retired-key").c4xx), (3, None, None));
        assert!(gateway(&db, now, &parse_query("/gateway?range=24h"), KEEP, None, None).1.contains("\"2xx\":null"));
    }

    #[test]
    fn token_totals_come_from_the_rollup_tiers_and_the_ledger() {
        let (db, now) = filled(30);
        // `filled` grows the prompt counter by 500 every 5 s and never generates
        let mut all = lss_core::tokens::AllTime { since: T0, ..Default::default() };
        all.prompt.total = 9e9;
        all.peak = lss_core::tokens::Peak { tok_s: 712.0, ts: T0 + 5 };
        let doc = tokens_doc(&db, now, now - 7200, T0, &all, &Default::default(), TokensDocFlags::default());
        let w = |name: &str| doc.windows.iter().find(|w| w.name == name).cloned().unwrap();
        // the minute in progress is not rolled up yet: 59 closed minutes x 12 samples x 500
        assert_eq!(w("1h").prompt, 59.0 * 6000.0);
        assert_eq!(w("today").prompt, 119.0 * 6000.0);
        assert_eq!((w("1h").generated, w("1h").cache_share), (0.0, Some(0.0)));
        assert!(w("24h").prompt > 23.0 * 360_000.0 && w("7d").prompt >= w("24h").prompt, "a week is never less than its last day: {:?}", doc.windows);
        assert_eq!((w("all").prompt, w("all").secs, doc.all_time_since), (9e9, now - T0, T0), "all time comes from the ledger, not from what the tiers still hold");
        assert_eq!(doc.windows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["1h", "24h", "7d", "today", "all"]);
        assert_eq!((doc.since, doc.peak.as_ref().map(|p| p.tok_s), doc.per_user_available, doc.output_len), (T0, Some(712.0), false, None));
    }

    #[test]
    fn the_probe_user_is_the_loopback_address_the_gate_sees() {
        assert_eq!(probe_user_of("http://127.0.0.1:8096").as_deref(), Some("127.0.0.1"));
        assert_eq!(probe_user_of("http://localhost:8096/").as_deref(), Some("127.0.0.1"));
        assert_eq!(probe_user_of("http://[::1]:8096").as_deref(), Some("::1"));
        assert_eq!(probe_user_of("http://gate.internal:8096"), None, "a named host: the address the gate sees cannot be known from here");
        assert_eq!(probe_user_of("http://192.0.2.7:8096"), None);
    }

    #[test]
    fn the_probe_address_comes_from_the_gate_url() {
        assert_eq!(probe_ip_of("http://127.0.0.1:8096").as_deref(), Some("127.0.0.x"));
        assert_eq!(probe_ip_of("http://localhost:8096/").as_deref(), Some("127.0.0.x"));
        assert_eq!(probe_ip_of("http://192.0.2.32:8096").as_deref(), Some("192.0.2.x"));
        assert_eq!(probe_ip_of("http://[::1]:8096").as_deref(), Some("0:0:0::x"));
        assert_eq!(probe_ip_of("http://gate.internal:8096"), None, "a host name: what the gate sees cannot be known from here");
    }
}
