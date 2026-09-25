//! TARGETS: the service levels the owner wants, and how much of the traffic (or of the time)
//! met each one. `[targets]` in the collector's config; every key is optional and has a default.
//!
//! * `ttft_p95_s`          95 % of requests should get their first word within this many seconds
//! * `min_tok_s_per_user`  the writing speed each user should at least get, tokens per second
//! * `max_queue_wait_s`    a request should not wait longer than this before the model starts on it
//! * `uptime_pct`          the share of the time the server should answer
//!
//! The first three are read from the ENGINE's own histograms (so they count every request,
//! whoever sent it); uptime from the incident record.

use crate::hist::HistAccum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetsConfig {
    pub ttft_p95_s: f64,
    pub min_tok_s_per_user: f64,
    pub max_queue_wait_s: f64,
    pub uptime_pct: f64,
}

impl Default for TargetsConfig {
    fn default() -> Self {
        Self { ttft_p95_s: 5.0, min_tok_s_per_user: 30.0, max_queue_wait_s: 2.0, uptime_pct: 99.0 }
    }
}

/// Prompts under this many tokens are "short": the first-word target is judged on them when the
/// gateway says how long each prompt was.
pub const SHORT_PROMPT_TOKENS: f64 = 8_000.0;
/// ... and only once this many short requests were seen in the window.
pub const MIN_SHORT_REQUESTS: u64 = 20;
/// `basis` of the first-word row when it is judged on short prompts only.
pub const SHORT_BASIS: &str = "requests under 8k tokens";

/// The sentence that goes with the first-word row, in plain words.
pub fn first_word_note(row: &TargetRow) -> Option<&'static str> {
    (row.key == "ttft").then(|| if row.basis == SHORT_BASIS { "first word is judged on prompts under 8k tokens: a long prompt that is not cached waits for its own reading, whatever the server does" } else { "first word counts EVERY prompt here: a long prompt that is not cached misses it by design (a gateway that logs prompt sizes lets lss judge short prompts only)" })
}

/// The share of requests that must meet a latency target for it to count as met: a p95 target.
pub const REQUEST_SHARE_GOAL_PCT: f64 = 95.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetRow {
    /// `ttft` | `speed` | `queue` | `uptime`
    pub key: String,
    /// plain words: "first word within 5 s"
    pub label: String,
    /// one short word for the overview line: `first word`
    pub short: String,
    /// the % of requests / of tokens / of the time that met it; null = nothing measured yet
    pub met_pct: Option<f64>,
    /// the % that must meet it
    pub goal_pct: f64,
    /// `requests` | `tokens` | `time`: what `met_pct` is a share of
    pub basis: String,
    /// null while nothing is measured
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetsStatus {
    /// the window the shares are over, e.g. `24h`
    pub window: String,
    pub rows: Vec<TargetRow>,
}

impl TargetsStatus {
    pub fn missed(&self) -> Vec<&TargetRow> {
        self.rows.iter().filter(|r| r.ok == Some(false)).collect()
    }
    pub fn measured(&self) -> bool {
        self.rows.iter().any(|r| r.met_pct.is_some())
    }
}

/// What the collector measured over the window.
#[derive(Debug, Clone, Default)]
pub struct TargetInputs {
    pub ttft: HistAccum,
    pub itl: HistAccum,
    pub queue_time: HistAccum,
    /// With a gateway that logs the prompt size of every request (a gateway publishing /gate/health v5.2+): the
    /// requests with a SHORT prompt (under `SHORT_PROMPT_TOKENS`) and how many of them got their
    /// first word within the target. A long prompt that is not in the cache takes as long as
    /// reading it takes - 124k tokens at 4 000 tok/s is 31 s whatever the server does - so the
    /// first-word target is only fair on short prompts. None = no such log: every request counts.
    pub ttft_short: Option<(u64, u64)>,
    /// 0..=100; None = no sample in the window
    pub uptime_pct: Option<f64>,
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn secs(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{v:.0} s")
    } else {
        format!("{v:.1} s")
    }
}

pub fn evaluate(cfg: &TargetsConfig, window: &str, i: &TargetInputs) -> TargetsStatus {
    let share = |h: &HistAccum, x: f64| h.fraction_le(x).map(|f| round1(f * 100.0));
    let row = |key: &str, label: String, short: &str, met: Option<f64>, goal: f64, basis: &str| TargetRow { key: key.into(), label, short: short.into(), met_pct: met, goal_pct: goal, basis: basis.into(), ok: met.map(|m| m + 1e-9 >= goal) };
    let mut rows = Vec::new();
    if cfg.ttft_p95_s > 0.0 {
        match i.ttft_short.filter(|(n, _)| *n >= MIN_SHORT_REQUESTS) {
            // judged where it is fair: prompts short enough that reading them is not the wait
            Some((n, within)) => rows.push(row("ttft", format!("first word within {}, short prompts", secs(cfg.ttft_p95_s)), "first word", Some(round1(within as f64 / n as f64 * 100.0)), REQUEST_SHARE_GOAL_PCT, SHORT_BASIS)),
            // no per-request prompt sizes: every request counts, long uncached prompts included
            // (the screen says so next to the number)
            None => rows.push(row("ttft", format!("first word within {}", secs(cfg.ttft_p95_s)), "first word", share(&i.ttft, cfg.ttft_p95_s), REQUEST_SHARE_GOAL_PCT, "requests")),
        }
    }
    if cfg.min_tok_s_per_user > 0.0 {
        // a token that arrived within 1/target seconds of the one before it was written at the
        // target speed or faster
        rows.push(row("speed", format!("written at {:.0} tok/s or faster", cfg.min_tok_s_per_user), "speed", share(&i.itl, 1.0 / cfg.min_tok_s_per_user), 90.0, "tokens"));
    }
    if cfg.max_queue_wait_s > 0.0 {
        rows.push(row("queue", format!("waited under {} for a slot", secs(cfg.max_queue_wait_s)), "no wait", share(&i.queue_time, cfg.max_queue_wait_s), REQUEST_SHARE_GOAL_PCT, "requests"));
    }
    if cfg.uptime_pct > 0.0 {
        rows.push(row("uptime", "server up".to_string(), "up", i.uptime_pct.map(|u| (u * 100.0).round() / 100.0), cfg.uptime_pct, "time"));
    }
    TargetsStatus { window: window.into(), rows }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(le: &[f64], counts: &[f64]) -> HistAccum {
        HistAccum { le: le.to_vec(), counts: counts.to_vec(), sum: 0.0, count: counts.iter().sum() }
    }

    #[test]
    fn shares_come_from_the_histograms_and_each_target_is_judged() {
        // 100 requests: 80 under 1 s, 16 in 1..5 s, 4 in 5..10 s
        let ttft = hist(&[1.0, 5.0, 10.0], &[80.0, 16.0, 4.0, 0.0]);
        // tokens: 900 within 20 ms (>= 50 tok/s), 100 between 20 and 50 ms
        let itl = hist(&[0.02, 0.05], &[900.0, 100.0, 0.0]);
        let queue = hist(&[0.5, 2.0, 8.0], &[50.0, 30.0, 20.0, 0.0]);
        let inputs = TargetInputs { ttft, itl, queue_time: queue, uptime_pct: Some(99.42), ttft_short: None };
        let t = evaluate(&TargetsConfig::default(), "24h", &inputs);
        let get = |k: &str| t.rows.iter().find(|r| r.key == k).unwrap();
        assert_eq!((get("ttft").met_pct, get("ttft").ok, get("ttft").label.as_str()), (Some(96.0), Some(true), "first word within 5 s"));
        // 30 tok/s = 33.3 ms: all of the first bucket and 44 % of the second
        assert_eq!((get("speed").met_pct, get("speed").ok, get("speed").basis.as_str()), (Some(94.4), Some(true), "tokens"));
        assert_eq!((get("queue").met_pct, get("queue").ok), (Some(80.0), Some(false)), "20 of 100 waited longer than 2 s");
        assert_eq!((get("uptime").met_pct, get("uptime").ok, get("uptime").goal_pct), (Some(99.42), Some(true), 99.0));
        assert_eq!(t.missed().len(), 1);
        // a gateway that logs prompt sizes: the first word is judged on SHORT prompts only. 2026-09-20:
        // this healthy server read 124k-token prompts at 4 000 tok/s (31 s each) and "missed" 5 s.
        let sized = evaluate(&TargetsConfig::default(), "24h", &TargetInputs { ttft_short: Some((200, 197)), ..inputs.clone() });
        let row = sized.rows.iter().find(|r| r.key == "ttft").unwrap();
        assert_eq!((row.met_pct, row.ok, row.label.as_str(), row.basis.as_str()), (Some(98.5), Some(true), "first word within 5 s, short prompts", SHORT_BASIS));
        assert!(first_word_note(row).unwrap().contains("under 8k tokens") && first_word_note(get("ttft")).unwrap().contains("EVERY prompt") && first_word_note(get("queue")).is_none());
        // too few short requests to judge: every request counts, as before
        let few = evaluate(&TargetsConfig::default(), "24h", &TargetInputs { ttft_short: Some((5, 1)), ..inputs.clone() });
        assert_eq!(few.rows[0].met_pct, Some(96.0));
        assert!(t.measured());
    }

    #[test]
    fn nothing_measured_is_unknown_not_a_pass_or_a_miss_and_zero_switches_a_target_off() {
        let t = evaluate(&TargetsConfig::default(), "24h", &TargetInputs::default());
        assert_eq!(t.rows.len(), 4);
        assert!(t.rows.iter().all(|r| r.met_pct.is_none() && r.ok.is_none()));
        assert!(!t.measured() && t.missed().is_empty());
        let off = TargetsConfig { min_tok_s_per_user: 0.0, uptime_pct: 0.0, ..Default::default() };
        let keys: Vec<String> = evaluate(&off, "24h", &TargetInputs::default()).rows.into_iter().map(|r| r.key).collect();
        assert_eq!(keys, ["ttft", "queue"]);
        assert_eq!(secs(2.5), "2.5 s");
    }

    #[test]
    fn fraction_le_interpolates_and_clamps() {
        let h = hist(&[1.0, 5.0], &[10.0, 10.0, 5.0]);
        assert_eq!(h.fraction_le(1.0), Some(0.4));
        assert_eq!(h.fraction_le(3.0), Some(0.6));
        assert_eq!(h.fraction_le(0.0), Some(0.0));
        assert_eq!(h.fraction_le(100.0), Some(0.8), "what sits in +Inf never met a finite target");
        assert_eq!(HistAccum::default().fraction_le(1.0), None);
    }
}
