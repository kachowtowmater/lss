//! "How many tokens did we serve": totals that survive serve restarts. The engine's counters
//! (`generation_tokens_total` …) start again from zero whenever the serve restarts, so the
//! collector keeps its own running total, fed by the GROWTH of those counters, and stores it.

use crate::hist::RawSummary;
use crate::prom::ServeMetrics;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A total that only grows, built from a counter that can reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CounterTotal {
    pub total: f64,
    /// the counter's value at the last observation (None = never seen)
    pub last: Option<f64>,
}

impl CounterTotal {
    /// Returns what this observation added.
    /// * first sight: nothing (what the engine counted before we looked is not ours to claim);
    /// * the counter grew: the growth, also across a collector restart (`last` is stored);
    /// * the counter went DOWN, or `reset` says the engine restarted: everything it shows now
    ///   was counted since that restart, so all of it is new.
    pub fn observe(&mut self, value: f64, reset: bool) -> f64 {
        if !value.is_finite() || value < 0.0 {
            return 0.0;
        }
        let added = match self.last {
            None => 0.0,
            Some(last) if reset || value < last => value,
            Some(last) => value - last,
        };
        self.last = Some(value);
        self.total += added;
        added
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Peak {
    pub tok_s: f64,
    pub ts: i64,
}

/// Days of peaks kept.
pub const PEAK_DAYS: usize = 60;

/// The persistent token ledger (stored as JSON in the collector's key-value table).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AllTime {
    /// when this ledger started counting
    pub since: i64,
    pub generated: CounterTotal,
    pub prompt: CounterTotal,
    pub cached: CounterTotal,
    pub requests: CounterTotal,
    /// the serve container's start time the counters were last read under: a change = a restart
    pub epoch: i64,
    /// the highest total decode tok/s ever seen in a 5 s sample
    pub peak: Peak,
    /// local day (`2026-09-20`) -> that day's peak
    pub day_peaks: BTreeMap<String, Peak>,
}

impl AllTime {
    /// One 5 s sample. `epoch` = the serve container's start time (0 = unknown), `day` = the
    /// local calendar day of `ts` on the collector's host.
    pub fn observe(&mut self, ts: i64, day: &str, epoch: i64, m: &ServeMetrics) {
        if self.since == 0 {
            self.since = ts;
        }
        let reset = epoch != 0 && self.epoch != 0 && epoch != self.epoch;
        if epoch != 0 {
            self.epoch = epoch;
        }
        self.generated.observe(m.generation_tokens_total, reset);
        self.prompt.observe(m.prompt_tokens_total, reset);
        self.cached.observe(m.cached_tokens_total, reset);
        self.requests.observe(m.requests_total, reset);
        let tok_s = m.gen_throughput;
        if tok_s.is_finite() && tok_s > 0.0 {
            if tok_s > self.peak.tok_s {
                self.peak = Peak { tok_s, ts };
            }
            let d = self.day_peaks.entry(day.to_string()).or_default();
            if tok_s > d.tok_s {
                *d = Peak { tok_s, ts };
            }
            while self.day_peaks.len() > PEAK_DAYS {
                let Some(oldest) = self.day_peaks.keys().next().cloned() else { break };
                self.day_peaks.remove(&oldest);
            }
        }
    }
}

/// Tokens of one window, EXACT, from the engine's own counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenWindow {
    /// `1h` | `24h` | `7d` | `today` (since local midnight on the collector's host) | `all`
    pub name: String,
    pub secs: i64,
    pub generated: f64,
    pub prompt: f64,
    /// of `prompt`: served from the prefix cache
    pub cached: f64,
    /// cached / prompt, 0..=1; null = no prompt tokens in the window
    pub cache_share: Option<f64>,
    pub requests: f64,
    /// Some(ts) = the request counter was only kept from `ts`, LATER than this window starts and
    /// later than the token counters: `requests` covers less than the row says (added 2026-09-20)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_since: Option<i64>,
}

impl TokenWindow {
    pub fn new(name: &str, secs: i64, generated: f64, prompt: f64, cached: f64, requests: f64) -> Self {
        let cache_share = (prompt > 0.0).then(|| ((cached / prompt).min(1.0) * 10_000.0).round() / 10_000.0);
        TokenWindow { name: name.to_string(), secs, generated, prompt, cached, cache_share, requests, requests_since: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DayPeak {
    pub day: String,
    pub tok_s: f64,
    pub ts: i64,
}

/// Who the tokens went to (a gateway publishing /gate/health v5.2+). Prompt tokens are the gate's estimate; output
/// tokens are exact unless `output_exact` is false.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserTokens {
    pub name: String,
    pub lane: String,
    pub prompt_est_24h: u64,
    pub output_24h: u64,
    pub output_exact: bool,
    pub requests_24h: u64,
    /// share of all REAL users' output tokens, 0..=1
    pub output_share: Option<f64>,
}

pub fn user_tokens(users: &crate::users::UsersStatus) -> Vec<UserTokens> {
    let total: u64 = users.rows.iter().map(|r| r.gate.completion_tokens_24h).sum();
    let mut out: Vec<UserTokens> = users
        .rows
        .iter()
        .map(|r| UserTokens {
            name: r.name.clone(),
            lane: r.gate.lane.clone(),
            prompt_est_24h: r.gate.prompt_tokens_est_24h,
            output_24h: r.gate.completion_tokens_24h,
            output_exact: r.gate.completion_tokens_exact,
            requests_24h: r.gate.requests_24h,
            output_share: (total > 0).then(|| ((r.gate.completion_tokens_24h as f64 / total as f64) * 10_000.0).round() / 10_000.0),
        })
        .collect();
    out.sort_by(|a, b| b.output_24h.cmp(&a.output_24h).then(a.name.cmp(&b.name)));
    out
}

/// `GET /tokens`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokensDoc {
    pub v: u32,
    pub generated_at: i64,
    /// the windowed series start here: a window reaching further back is incomplete
    pub since: i64,
    /// `1h`, `24h`, `7d`, `today`, then `all` (since `all_time_since`, across serve restarts)
    pub windows: Vec<TokenWindow>,
    pub all_time_since: i64,
    /// the highest total decode tok/s ever seen, and per local day (newest first, 14 days)
    pub peak: Option<Peak>,
    pub peak_by_day: Vec<DayPeak>,
    /// length of the answers and of the prompts over the last 7 days, in tokens
    pub output_len: Option<RawSummary>,
    pub prompt_len: Option<RawSummary>,
    /// false = the gate does not publish per-user stats (older than v5.2)
    pub per_user_available: bool,
    pub per_user: Vec<UserTokens>,
    /// card #23: NO gateway configured at all (the collector's gate_absent). `lss tokens`
    /// says so instead of printing a table of zeros for an engine that publishes no counters.
    #[serde(default)]
    pub gate_absent: bool,
    /// card #23: the engine does not publish token counters at all (Ollama, LM Studio) -
    /// "0 written / 0 read" would be a made-up zero, not a measurement.
    #[serde(default)]
    pub tokens_not_reported: bool,
}

pub fn peak_by_day(all: &AllTime, days: usize) -> Vec<DayPeak> {
    all.day_peaks.iter().rev().take(days).map(|(day, p)| DayPeak { day: day.clone(), tok_s: (p.tok_s * 10.0).round() / 10.0, ts: p.ts }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(gen: f64, prompt: f64, tok_s: f64) -> ServeMetrics {
        ServeMetrics { generation_tokens_total: gen, prompt_tokens_total: prompt, cached_tokens_total: prompt / 2.0, requests_total: gen / 100.0, gen_throughput: tok_s, ..Default::default() }
    }

    #[test]
    fn the_all_time_counter_survives_counter_resets() {
        let mut t = CounterTotal::default();
        assert_eq!(t.observe(5_000.0, false), 0.0, "what the engine counted before we first looked is not ours");
        assert_eq!(t.observe(5_400.0, false), 400.0);
        assert_eq!(t.observe(5_400.0, false), 0.0);
        // the serve restarts: the counter starts again from zero and has already counted 120
        assert_eq!(t.observe(120.0, false), 120.0, "a counter that went down is a reset: all of it is new");
        assert_eq!(t.observe(300.0, false), 180.0);
        assert_eq!(t.total, 700.0);
        // a restart the value alone cannot show: the new engine has ALREADY passed the old count
        assert_eq!(t.observe(900.0, true), 900.0, "the container start time changed, so this is a reset");
        assert_eq!(t.total, 1600.0);
        // junk never moves it
        assert_eq!((t.observe(f64::NAN, false), t.observe(-1.0, false), t.total, t.last), (0.0, 0.0, 1600.0, Some(900.0)));
    }

    #[test]
    fn the_ledger_survives_a_collector_restart_and_a_serve_restart() {
        let mut a = AllTime::default();
        a.observe(1_000, "2026-09-19", 900, &m(10_000.0, 80_000.0, 0.0));
        a.observe(1_005, "2026-09-19", 900, &m(10_600.0, 83_000.0, 190.0));
        assert_eq!((a.since, a.generated.total, a.prompt.total, a.cached.total), (1_000, 600.0, 3_000.0, 1_500.0));
        // the collector restarts: the ledger goes through its stored form, and the tokens
        // generated while it was down are still counted (the engine kept its counter)
        let mut a: AllTime = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        a.observe(2_000, "2026-09-19", 900, &m(15_600.0, 90_000.0, 712.0));
        assert_eq!(a.generated.total, 5_600.0);
        // the SERVE restarts (another container start): the counters begin again
        a.observe(3_000, "2026-09-20", 2_950, &m(40.0, 900.0, 88.0));
        a.observe(3_005, "2026-09-20", 2_950, &m(240.0, 1_900.0, 95.0));
        assert_eq!((a.generated.total, a.prompt.total, a.epoch), (5_840.0, 11_900.0, 2_950));
        // while the serve is down there is simply no observation; an unknown epoch never resets
        a.observe(3_010, "2026-09-20", 0, &m(300.0, 2_000.0, 0.0));
        assert_eq!((a.generated.total, a.epoch), (5_900.0, 2_950));
        // the peak ever and the peak of each day
        assert_eq!(a.peak, Peak { tok_s: 712.0, ts: 2_000 });
        assert_eq!(peak_by_day(&a, 14), vec![DayPeak { day: "2026-09-20".into(), tok_s: 95.0, ts: 3_005 }, DayPeak { day: "2026-09-19".into(), tok_s: 712.0, ts: 2_000 }]);
        // an older stored ledger (fewer fields) still loads
        let old: AllTime = serde_json::from_str("{\"since\":5,\"generated\":{\"total\":9.0}}").unwrap();
        assert_eq!((old.since, old.generated.total, old.generated.last), (5, 9.0, None));
    }

    #[test]
    fn only_so_many_days_of_peaks_are_kept() {
        let mut a = AllTime::default();
        for d in 0..(PEAK_DAYS as i64 + 5) {
            a.observe(d * 86_400, &format!("day-{d:03}"), 1, &m(d as f64, 0.0, 100.0 + d as f64));
        }
        assert_eq!(a.day_peaks.len(), PEAK_DAYS);
        assert!(!a.day_peaks.contains_key("day-000") && a.day_peaks.contains_key("day-064"));
        assert_eq!(a.peak.tok_s, 164.0);
    }

    #[test]
    fn windows_and_the_per_user_split() {
        let w = TokenWindow::new("24h", 86_400, 1000.0, 8000.0, 6000.0, 12.0);
        assert_eq!((w.cache_share, TokenWindow::new("1h", 3600, 0.0, 0.0, 0.0, 0.0).cache_share), (Some(0.75), None));
        let gate = crate::gate::parse_gate_health(include_str!("../../../fixtures/gate_health_v52.json")).unwrap();
        let users = crate::users::build_users(Some(&gate), &[], &BTreeMap::new(), &crate::users::OwnTraffic::default(), 1_789_820_000);
        let split = user_tokens(&users);
        assert_eq!(split[0].name, "acme");
        assert!(split.windows(2).all(|p| p[0].output_24h >= p[1].output_24h), "biggest producer first");
        let shares: f64 = split.iter().filter_map(|u| u.output_share).sum();
        assert!((shares - 1.0).abs() < 0.001, "{shares}");
        assert!(split.iter().any(|u| !u.output_exact), "an estimated count is marked, so the screen can show `~`");
        assert!(user_tokens(&crate::users::UsersStatus::default()).is_empty());
    }
}
