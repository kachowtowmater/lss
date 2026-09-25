//! Prometheus histograms, the way Grafana reads them: percentiles come from bucket DELTAS
//! over a window (`histogram_quantile(q, rate(..._bucket[1m]))`), never from the lifetime
//! cumulative counts - a server that has answered a million fast requests would otherwise
//! hide the slow minute happening right now.
//!
//! Pure: a scrape goes in as text-derived `prom::Series`, windows come out.

use crate::prom::Series;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// (short name used in metric names and the API, SGLang histogram base name)
pub const LATENCY_METRICS: [(&str, &str); 4] = [
    ("ttft", "time_to_first_token_seconds"),
    ("e2e", "e2e_request_latency_seconds"),
    ("itl", "inter_token_latency_seconds"),
    ("queue_time", "queue_time_seconds"),
];

/// Every histogram the collector windows: the four latencies (seconds), then the two request
/// SIZE histograms (tokens). Only the latencies become percentile series; all six have their
/// bucket deltas stored for the distribution views.
pub const HIST_METRICS: [(&str, &str); 6] = [
    ("ttft", "time_to_first_token_seconds"),
    ("e2e", "e2e_request_latency_seconds"),
    ("itl", "inter_token_latency_seconds"),
    ("queue_time", "queue_time_seconds"),
    ("prompt_tokens", "prompt_tokens_histogram"),
    ("gen_tokens", "generation_tokens_histogram"),
];
pub const N_HIST: usize = HIST_METRICS.len();

pub fn latency_index(short: &str) -> Option<usize> {
    LATENCY_METRICS.iter().position(|(s, _)| *s == short)
}

pub fn hist_index(short: &str) -> Option<usize> {
    HIST_METRICS.iter().position(|(s, _)| *s == short)
}

/// `s` for the latency histograms, `tokens` for the size ones.
pub fn hist_unit(short: &str) -> &'static str {
    if latency_index(short).is_some() { "s" } else { "tokens" }
}

/// One scrape of one histogram: cumulative counts per upper bound.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistSnapshot {
    /// finite upper bounds, ascending; the `+Inf` bucket is implied after the last one
    pub le: Vec<f64>,
    /// cumulative counts, `le.len() + 1` long (the last entry is `+Inf` = everything)
    pub cum: Vec<f64>,
    pub sum: f64,
    pub count: f64,
}

pub type HistSet = [Option<HistSnapshot>; N_HIST];

/// Same label rules as `prom::Extractor`: `tp_rank="0"` (or no rank label), and when the
/// engine publishes an explicit total (`priority=""`) only that row set counts; otherwise
/// the per-priority / per-streaming label sets are summed bucket by bucket.
pub fn extract_histogram(series: &[Series], base: &str) -> Option<HistSnapshot> {
    extract_histogram_named(series, &format!("sglang:{base}"))
}

/// The same for any engine: `family` is the full metric family name (`vllm:time_to_first_token_seconds`,
/// `tgi_request_duration`), and `only` keeps the rows carrying that label (`method="decode"`).
pub fn extract_histogram_named(series: &[Series], family: &str) -> Option<HistSnapshot> {
    extract_histogram_where(series, family, None)
}

pub fn extract_histogram_where(series: &[Series], family: &str, only: Option<(&str, &str)>) -> Option<HistSnapshot> {
    let series: Vec<Series> = match only {
        Some((k, v)) => series.iter().filter(|s| s.label(k) == Some(v)).cloned().collect(),
        None => series.to_vec(),
    };
    let series = series.as_slice();
    let bucket = format!("{family}_bucket");
    let usable = |s: &&Series| s.label("tp_rank").is_none_or(|r| r == "0") && s.value.is_finite();
    let rows: Vec<&Series> = series.iter().filter(|s| s.name == bucket).filter(usable).collect();
    if rows.is_empty() {
        return None;
    }
    let explicit_total = rows.iter().any(|s| s.label("priority") == Some(""));
    let keep = |s: &&Series| !explicit_total || s.label("priority") == Some("");
    let mut bounds: Vec<(f64, f64)> = Vec::new();
    for s in rows.iter().copied().filter(|s| keep(s)) {
        let le = match s.label("le")? {
            "+Inf" | "Inf" => f64::INFINITY,
            v => v.parse::<f64>().ok()?,
        };
        match bounds.iter_mut().find(|(b, _)| *b == le) {
            Some((_, c)) => *c += s.value,
            None => bounds.push((le, s.value)),
        }
    }
    bounds.sort_by(|a, b| a.0.total_cmp(&b.0));
    if bounds.last().is_none_or(|(b, _)| b.is_finite()) {
        // no +Inf row: the last finite bucket is all we know
        let top = bounds.last().map_or(0.0, |(_, c)| *c);
        bounds.push((f64::INFINITY, top));
    }
    let scalar = |suffix: &str| -> f64 {
        let name = format!("{family}_{suffix}");
        series.iter().filter(|s| s.name == name).filter(usable).filter(keep).map(|s| s.value).sum()
    };
    Some(HistSnapshot {
        le: bounds.iter().filter(|(b, _)| b.is_finite()).map(|(b, _)| *b).collect(),
        cum: bounds.iter().map(|(_, c)| *c).collect(),
        sum: scalar("sum"),
        count: scalar("count"),
    })
}

pub fn extract_latency(series: &[Series]) -> HistSet {
    let mut out: HistSet = Default::default();
    for (i, (_, base)) in HIST_METRICS.iter().enumerate() {
        out[i] = extract_histogram(series, base);
    }
    out
}

/// What happened between two scrapes, or inside one window: NON-cumulative counts per bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistAccum {
    pub le: Vec<f64>,
    /// `le.len() + 1` long; the last entry is the `+Inf` bucket
    pub counts: Vec<f64>,
    pub sum: f64,
    pub count: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistSummary {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub avg_ms: f64,
    pub count: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawSummary {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub avg: f64,
    pub count: f64,
}

fn per_bucket(cum: &[f64]) -> Vec<f64> {
    let mut prev = 0.0;
    cum.iter()
        .map(|c| {
            let d = (c - prev).max(0.0);
            prev = prev.max(*c);
            d
        })
        .collect()
}

/// `cur - prev`. A counter that went backwards (the engine restarted) or changed its bucket
/// layout makes the current totals the delta: everything in them happened since the restart.
pub fn delta(prev: &HistSnapshot, cur: &HistSnapshot) -> HistAccum {
    let reset = prev.le != cur.le || cur.count < prev.count || cur.cum.iter().zip(&prev.cum).any(|(c, p)| c < p);
    if reset {
        return HistAccum { le: cur.le.clone(), counts: per_bucket(&cur.cum), sum: cur.sum.max(0.0), count: cur.count.max(0.0) };
    }
    let cum: Vec<f64> = cur.cum.iter().zip(&prev.cum).map(|(c, p)| c - p).collect();
    HistAccum { le: cur.le.clone(), counts: per_bucket(&cum), sum: (cur.sum - prev.sum).max(0.0), count: (cur.count - prev.count).max(0.0) }
}

impl HistAccum {
    pub fn is_empty(&self) -> bool {
        self.total() <= 0.0
    }

    /// Observations in the buckets (the histogram's own `_count` can lag a scrape behind).
    pub fn total(&self) -> f64 {
        self.counts.iter().sum()
    }

    pub fn add(&mut self, d: &HistAccum) {
        if d.counts.is_empty() {
            return;
        }
        if self.le != d.le || self.counts.len() != d.counts.len() {
            // first delta, or the engine came back with another bucket layout
            *self = d.clone();
            return;
        }
        for (a, b) in self.counts.iter_mut().zip(&d.counts) {
            *a += b;
        }
        self.sum += d.sum;
        self.count += d.count;
    }

    /// `histogram_quantile`: linear interpolation inside the bucket that holds the rank; a
    /// rank that lands in `+Inf` reports the highest finite bound. Seconds. None = no data.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.total();
        if total <= 0.0 || self.le.is_empty() {
            return None;
        }
        let rank = q.clamp(0.0, 1.0) * total;
        let mut seen = 0.0;
        for (i, c) in self.counts.iter().enumerate() {
            if *c <= 0.0 {
                continue;
            }
            if seen + c >= rank {
                let Some(upper) = self.le.get(i).copied() else { return self.le.last().copied() };
                let lower = if i == 0 { 0.0_f64.min(upper) } else { self.le[i - 1] };
                return Some(lower + (upper - lower) * ((rank - seen) / c).clamp(0.0, 1.0));
            }
            seen += c;
        }
        self.le.last().copied()
    }

    /// The share (0..=1) of observations at or under `x`, interpolated inside the bucket that
    /// holds `x`. None without data. What "N % of requests met the target" is read from.
    pub fn fraction_le(&self, x: f64) -> Option<f64> {
        let total = self.total();
        if total <= 0.0 || self.le.is_empty() || !x.is_finite() {
            return None;
        }
        let mut seen = 0.0;
        for (i, c) in self.counts.iter().enumerate() {
            let Some(upper) = self.le.get(i).copied() else { break };
            let lower = if i == 0 { 0.0_f64.min(upper) } else { self.le[i - 1] };
            if x >= upper {
                seen += c;
                continue;
            }
            if x > lower {
                seen += c * ((x - lower) / (upper - lower)).clamp(0.0, 1.0);
            }
            break;
        }
        Some((seen / total).clamp(0.0, 1.0))
    }

    pub fn summary(&self) -> Option<HistSummary> {
        let ms = |q: f64| self.quantile(q).map(|s| round3(s * 1000.0));
        let n = if self.count > 0.0 { self.count } else { self.total() };
        Some(HistSummary { p50_ms: ms(0.5)?, p90_ms: ms(0.9)?, p99_ms: ms(0.99)?, avg_ms: round3(self.sum / n * 1000.0), count: self.total() })
    }

    /// p50/p90/p99/avg in the histogram's NATIVE unit (seconds, or tokens), for the histograms
    /// where milliseconds would be a lie.
    pub fn summary_raw(&self) -> Option<RawSummary> {
        let n = if self.count > 0.0 { self.count } else { self.total() };
        Some(RawSummary { p50: round3(self.quantile(0.5)?), p90: round3(self.quantile(0.9)?), p99: round3(self.quantile(0.99)?), avg: round3(self.sum / n), count: self.total() })
    }

    /// Compact storage form: `index:count` pairs of the non-empty buckets.
    pub fn encode_counts(&self) -> String {
        self.counts.iter().enumerate().filter(|(_, c)| **c > 0.0).map(|(i, c)| format!("{i}:{c:.0}")).collect::<Vec<_>>().join(",")
    }

    pub fn decode_counts(text: &str, buckets: usize) -> Vec<f64> {
        let mut out = vec![0.0; buckets];
        for part in text.split(',') {
            let Some((i, c)) = part.split_once(':') else { continue };
            if let (Ok(i), Ok(c)) = (i.parse::<usize>(), c.parse::<f64>()) {
                if let Some(slot) = out.get_mut(i) {
                    *slot += c;
                }
            }
        }
        out
    }
}

pub fn encode_bounds(le: &[f64]) -> String {
    le.iter().map(|b| format!("{b}")).collect::<Vec<_>>().join(",")
}

pub fn decode_bounds(text: &str) -> Vec<f64> {
    text.split(',').filter_map(|b| b.parse().ok()).collect()
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// A window that just ended: one accumulator per latency metric (empty = no requests).
#[derive(Debug, Clone, PartialEq)]
pub struct ClosedWindow {
    /// 60 or 600
    pub res: i64,
    /// start of the window
    pub ts: i64,
    pub accs: [HistAccum; N_HIST],
}

impl ClosedWindow {
    /// The series points of this window: `ttft_p50_ms` … `queue_time_avg_ms`, plus `_count`.
    pub fn points(&self) -> Vec<(String, f64, f64)> {
        let mut out = Vec::new();
        for (i, (short, _)) in LATENCY_METRICS.iter().enumerate() {
            let Some(s) = self.accs[i].summary() else { continue };
            for (suffix, v) in [("p50_ms", s.p50_ms), ("p90_ms", s.p90_ms), ("p99_ms", s.p99_ms), ("avg_ms", s.avg_ms)] {
                out.push((format!("{short}_{suffix}"), v, s.count));
            }
        }
        out
    }
}

/// The last ten minutes, for `/status.serve.latency`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LatencyNow {
    pub window_s: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft: Option<HistSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e: Option<HistSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub itl: Option<HistSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_time: Option<HistSummary>,
}

const RECENT_MINUTES: usize = 10;

/// Feeds on scrapes, cuts them into 1-minute and 10-minute windows of bucket deltas.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    prev: Option<HistSet>,
    minute: Option<(i64, [HistAccum; N_HIST])>,
    ten: Option<(i64, [HistAccum; N_HIST])>,
    recent: VecDeque<(i64, [HistAccum; N_HIST])>,
}

impl LatencyTracker {
    /// `set = None` when the scrape failed: the windows still close on time, and the next good
    /// scrape is diffed against the last good one.
    pub fn observe(&mut self, ts: i64, set: Option<&HistSet>) -> Vec<ClosedWindow> {
        let mut closed = Vec::new();
        for (res, slot) in [(60, &mut self.minute), (600, &mut self.ten)] {
            let start = ts - ts.rem_euclid(res);
            if slot.as_ref().is_some_and(|(t, _)| *t != start) {
                let (t, accs) = slot.take().expect("checked");
                if res == 60 {
                    self.recent.push_back((t, accs.clone()));
                    while self.recent.len() > RECENT_MINUTES {
                        self.recent.pop_front();
                    }
                }
                if accs.iter().any(|a| !a.is_empty()) {
                    closed.push(ClosedWindow { res, ts: t, accs });
                }
            }
            slot.get_or_insert_with(|| (start, Default::default()));
        }
        let Some(set) = set else { return closed };
        if let Some(prev) = &self.prev {
            for i in 0..N_HIST {
                let (Some(p), Some(c)) = (&prev[i], &set[i]) else { continue };
                let d = delta(p, c);
                if d.is_empty() {
                    continue;
                }
                if let Some((_, accs)) = &mut self.minute {
                    accs[i].add(&d);
                }
                if let Some((_, accs)) = &mut self.ten {
                    accs[i].add(&d);
                }
            }
        }
        self.prev = Some(set.clone());
        self.recent.retain(|(t, _)| *t > ts - (RECENT_MINUTES as i64 + 1) * 60);
        closed
    }

    /// Percentiles over the closed minutes of the last ten plus the minute in progress.
    pub fn now(&self) -> LatencyNow {
        let merged = |i: usize| -> Option<HistSummary> {
            let mut acc = HistAccum::default();
            for (_, accs) in self.recent.iter().chain(self.minute.iter()) {
                acc.add(&accs[i]);
            }
            acc.summary()
        };
        LatencyNow { window_s: RECENT_MINUTES as i64 * 60, ttft: merged(0), e2e: merged(1), itl: merged(2), queue_time: merged(3) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(le: &[f64], per_bucket: &[f64], mean: f64) -> HistSnapshot {
        let mut cum = Vec::new();
        let mut run = 0.0;
        for c in per_bucket {
            run += c;
            cum.push(run);
        }
        HistSnapshot { le: le.to_vec(), cum, sum: run * mean, count: run }
    }

    fn plus(a: &HistSnapshot, b: &HistSnapshot) -> HistSnapshot {
        HistSnapshot { le: a.le.clone(), cum: a.cum.iter().zip(&b.cum).map(|(x, y)| x + y).collect(), sum: a.sum + b.sum, count: a.count + b.count }
    }

    const TENTHS: [f64; 10] = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

    #[test]
    fn uniform_distribution_gives_the_textbook_percentiles() {
        // 1000 observations spread evenly over 0..1 s: p50 = 0.5, p90 = 0.9, p99 = 0.99
        let mut counts = vec![100.0; 10];
        counts.push(0.0);
        let acc = delta(&HistSnapshot { le: TENTHS.to_vec(), cum: vec![0.0; 11], ..Default::default() }, &snap(&TENTHS, &counts, 0.5));
        let near = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(near(acc.quantile(0.5), 0.5));
        assert!(near(acc.quantile(0.9), 0.9));
        assert!(near(acc.quantile(0.99), 0.99));
        let s = acc.summary().unwrap();
        assert_eq!((s.p50_ms, s.p90_ms, s.p99_ms, s.avg_ms, s.count), (500.0, 900.0, 990.0, 500.0, 1000.0));
    }

    #[test]
    fn one_bucket_interpolates_inside_it_and_inf_reports_the_top_bound() {
        let le = [0.2, 0.4, 0.8];
        let acc = HistAccum { le: le.to_vec(), counts: vec![0.0, 10.0, 0.0, 0.0], sum: 3.0, count: 10.0 };
        assert!((acc.quantile(0.5).unwrap() - 0.3).abs() < 1e-9, "middle of (0.2, 0.4]");
        assert!((acc.quantile(1.0).unwrap() - 0.4).abs() < 1e-9);
        let tail = HistAccum { le: le.to_vec(), counts: vec![1.0, 0.0, 0.0, 9.0], sum: 900.0, count: 10.0 };
        assert_eq!(tail.quantile(0.99), Some(0.8), "+Inf has no upper bound to interpolate to");
        assert_eq!(HistAccum { le: le.to_vec(), counts: vec![0.0; 4], ..Default::default() }.quantile(0.5), None, "no requests = no percentile, not zero");
        // a first bucket at le=0 (queue_time has one) is a point, not a range
        let zero = HistAccum { le: vec![0.0, 0.001], counts: vec![5.0, 0.0, 0.0], sum: 0.0, count: 5.0 };
        assert_eq!(zero.quantile(0.5), Some(0.0));
    }

    #[test]
    fn percentiles_come_from_the_delta_not_from_the_lifetime_counts() {
        // lifetime: a million fast answers. This minute: 100 slow ones.
        let lifetime = snap(&TENTHS, &[1e6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0.05);
        let slow_minute = snap(&TENTHS, &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 50.0, 50.0, 0.0], 0.9);
        let now = plus(&lifetime, &slow_minute);
        let cumulative = delta(&HistSnapshot { le: TENTHS.to_vec(), cum: vec![0.0; 11], ..Default::default() }, &now);
        assert!(cumulative.quantile(0.99).unwrap() < 0.1, "the lifetime view hides the slow minute");
        let d = delta(&lifetime, &now);
        assert_eq!(d.total(), 100.0);
        assert!((d.quantile(0.5).unwrap() - 0.9).abs() < 1e-9);
        assert!((d.summary().unwrap().avg_ms - 900.0).abs() < 1e-6);
    }

    #[test]
    fn a_counter_reset_makes_the_new_totals_the_delta() {
        let before = snap(&TENTHS, &[500.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0.05);
        let after_restart = snap(&TENTHS, &[0.0, 0.0, 7.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0.25);
        let d = delta(&before, &after_restart);
        assert_eq!(d.total(), 7.0);
        assert!((d.quantile(0.5).unwrap() - 0.25).abs() < 1e-9);
        // another bucket layout is a reset too
        let other = snap(&[1.0, 2.0], &[3.0, 0.0, 0.0], 0.5);
        assert_eq!(delta(&before, &other).le, vec![1.0, 2.0]);
    }

    #[test]
    fn the_real_scrape_has_all_four_histograms_with_their_own_label_rules() {
        let series = crate::prom::parse(include_str!("../../../fixtures/sglang_metrics.txt"));
        let set = extract_latency(&series);
        let ttft = set[0].as_ref().expect("ttft");
        assert_eq!(*ttft.cum.last().unwrap(), 13.0, "summed over priority x is_streaming, like ttft_count");
        assert_eq!(ttft.count, 13.0);
        assert_eq!(ttft.le.len() + 1, ttft.cum.len());
        assert!(ttft.le.windows(2).all(|w| w[0] < w[1]));
        let queue = set[3].as_ref().expect("queue_time");
        assert_eq!(*queue.cum.last().unwrap(), 14.0, "priority=\"\" on four ranks: rank 0 only");
        assert!(set[1].is_some() && set[2].is_some());
        // the request-size histograms ride along, in tokens
        let prompt = set[4].as_ref().expect("prompt_tokens");
        assert_eq!(*prompt.cum.last().unwrap(), 13.0);
        assert!((prompt.sum - (506_827.0 + 195.0 + 15.0)).abs() < 1e-6, "{}", prompt.sum);
        assert_eq!(set[5].as_ref().map(|g| g.sum), Some(909.0 + 114.0 + 14.0));
        assert_eq!((hist_unit("ttft"), hist_unit("gen_tokens"), hist_index("prompt_tokens")), ("s", "tokens", Some(4)));
    }

    #[test]
    fn the_tracker_cuts_one_and_ten_minute_windows_on_a_fake_clock() {
        let zero = snap(&TENTHS, &[0.0; 11], 0.0);
        let mut t = LatencyTracker::default();
        let mut cur = zero.clone();
        let set = |s: &HistSnapshot| -> HistSet { [Some(s.clone()), None, None, None, None, None] };
        let mut closed = Vec::new();
        // minute 0: 10 requests in (0.1, 0.2]; minute 1: nothing; minute 2: 10 in (0.8, 0.9]
        for ts in (0..1200).step_by(5) {
            if (5..=50).contains(&ts) {
                cur = plus(&cur, &snap(&TENTHS, &[0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0.15));
            }
            if (125..=170).contains(&ts) {
                cur = plus(&cur, &snap(&TENTHS, &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0], 0.85));
            }
            // a failed scrape in the middle must not lose or double anything
            let scrape = if ts == 30 { None } else { Some(set(&cur)) };
            closed.extend(t.observe(1_000_000_200 + ts, scrape.as_ref()));
        }
        let minutes: Vec<&ClosedWindow> = closed.iter().filter(|c| c.res == 60).collect();
        assert_eq!(minutes.len(), 2, "the idle minutes close silently");
        assert_eq!(minutes[0].ts, 1_000_000_200);
        assert_eq!(minutes[0].accs[0].total(), 20.0);
        assert!((minutes[0].accs[0].quantile(0.5).unwrap() - 0.15).abs() < 1e-9);
        assert_eq!(minutes[1].ts, 1_000_000_320);
        assert!((minutes[1].accs[0].quantile(0.5).unwrap() - 0.85).abs() < 1e-9);
        let tens: Vec<&ClosedWindow> = closed.iter().filter(|c| c.res == 600).collect();
        assert_eq!(tens.len(), 1);
        assert_eq!(tens[0].accs[0].total(), 30.0, "the 10-minute window merges BUCKETS, it does not average percentiles");
        let pts = minutes[0].points();
        assert!(pts.iter().any(|(n, v, c)| n == "ttft_p50_ms" && (*v - 150.0).abs() < 1e-6 && *c == 20.0), "{pts:?}");
        // /status window: by now both bursts are older than ten minutes
        assert_eq!(t.now().ttft, None);
    }

    #[test]
    fn counts_round_trip_through_the_storage_form() {
        let acc = HistAccum { le: TENTHS.to_vec(), counts: vec![0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 12.0, 1.0], sum: 1.0, count: 16.0 };
        assert_eq!(acc.encode_counts(), "1:3,9:12,10:1");
        assert_eq!(HistAccum::decode_counts(&acc.encode_counts(), 11), acc.counts);
        assert_eq!(HistAccum::decode_counts("", 3), vec![0.0; 3]);
        assert_eq!(decode_bounds(&encode_bounds(&TENTHS)), TENTHS.to_vec());
    }
}
