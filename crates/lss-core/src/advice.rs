//! ADVICE: evidence for settings and upgrade decisions. Rule-based and read-only: every finding
//! is one plain sentence with the number behind it, a severity (fine / watch / act) and the
//! setting or upgrade it points at. NOTHING here changes anything; a person decides.
//!
//! Every rule, its threshold and what it points at is documented in docs/ADVICE.md (a test
//! keeps that file and `RULES` in step).

use crate::config::AdviceConfig;
use crate::loadout::Saturation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Fine,
    Watch,
    Act,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Fine => "fine",
            Severity::Watch => "watch",
            Severity::Act => "act",
        }
    }
}

/// `prompt_sizes` says nothing until this many real requests are behind it.
pub const MIN_PROMPT_REQUESTS: f64 = 100.0;

/// Rule ids, in the order they are evaluated and documented.
pub const RULES: [&str; 14] = ["slots_saturated", "queue_wait", "kv_pressure", "evictions", "prompt_sizes", "cache_reuse", "spec_decoding", "time_split", "rejections", "growth", "energy", "throttling", "saturation_point", "targets"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    pub severity: Severity,
    /// `24h` | `7d` | `30d`
    pub window: String,
    /// plain English, with the number in it
    pub sentence: String,
    /// the number behind the sentence, for sorting and for agents
    pub value: Option<f64>,
    pub unit: String,
    /// the setting or the upgrade this is evidence for ("" when there is nothing to do)
    pub points_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneRejects {
    pub lane: String,
    pub too_busy_429: u64,
    pub too_large_413: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuThrottle {
    pub index: u32,
    /// % of the window with a thermal or hardware slowdown throttle active
    pub pct: f64,
    /// listed in `thermal_exclude`: measured, shown in the numbers, never in the advice wording
    pub excluded: bool,
}

/// Week over week: the last 7 days against the 7 before them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Growth {
    pub tokens_per_day: f64,
    pub tokens_per_day_prev: f64,
    pub requests_per_day: f64,
    pub requests_per_day_prev: f64,
    pub peak_users: f64,
    pub peak_users_prev: f64,
    /// days of data in the earlier week (a growth figure needs a real earlier week)
    pub prev_days_covered: f64,
    /// card #211: why `peak_users` is NOT this machine's count, when it is not - a gateway older
    /// than v5.10 cannot say which machine a user is on. `None` = this machine's own users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_users_basis: Option<String>,
}

/// Everything the rules look at, summarised over one window. `None` = not measured in it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowStats {
    pub name: String,
    pub secs: i64,
    /// how much of the window the collector has data for
    pub covered_secs: i64,
    pub slots: u32,
    pub slots_full_pct: Option<f64>,
    pub queue_pct: Option<f64>,
    pub queue_wait_p95_ms: Option<f64>,
    pub kv_peak_pct: Option<f64>,
    pub kv_p95_pct: Option<f64>,
    pub retracted_pct: Option<f64>,
    pub evicted_tokens: Option<f64>,
    /// the seconds of the window over which evictions were counted (the counter is younger than
    /// the window's other metrics); None = use the window's covered time
    pub evicted_secs: Option<i64>,
    pub prompt_p50: Option<f64>,
    pub prompt_p95: Option<f64>,
    pub prompt_max: Option<f64>,
    /// `prompt_max` is the upper edge of a histogram bucket ("up to"), not an exact figure
    pub prompt_max_is_bucket_edge: bool,
    /// requests behind the prompt numbers that were NOT the monitor's own probes (None = unknown)
    pub prompt_requests: Option<f64>,
    pub context_len: Option<f64>,
    pub cache_hit_pct: Option<f64>,
    /// (users at once, accept length) for the current loadout
    pub spec_by_level: Vec<(u64, f64)>,
    pub prefill_time_pct: Option<f64>,
    pub rejects: Vec<LaneRejects>,
    pub growth: Option<Growth>,
    pub wh_per_mtok: Option<f64>,
    pub gen_tokens: Option<f64>,
    pub throttle: Vec<GpuThrottle>,
    pub saturation: Option<Saturation>,
    /// how many levels of the concurrency curve qualify as evidence, and whether a benchmark
    /// measured any of them: live traffic alone needs three before it is worth a warning
    pub saturation_levels: u64,
    pub saturation_from_bench: bool,
    /// the owner's service targets over this window (`[targets]`); null = not computed
    pub targets: Option<crate::targets::TargetsStatus>,
}

fn span(name: &str) -> &'static str {
    match name {
        "24h" => "the last 24 hours",
        "7d" => "the week",
        "30d" => "the last 30 days",
        _ => "the period",
    }
}

fn pct(v: f64) -> String {
    if v < 0.05 {
        "0%".into()
    } else if v < 10.0 {
        format!("{v:.1}%")
    } else {
        format!("{v:.0}%")
    }
}

fn tokens(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1e6).replace(".0M", "M")
    } else if v >= 10_000.0 {
        format!("{:.0}k", v / 1e3)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

fn secs(ms: f64) -> String {
    if ms >= 10_000.0 {
        format!("{:.0} s", ms / 1000.0)
    } else if ms >= 1000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        format!("{ms:.0} ms")
    }
}

fn level(v: f64, watch: f64, act: f64) -> Severity {
    if v >= act {
        Severity::Act
    } else if v >= watch {
        Severity::Watch
    } else {
        Severity::Fine
    }
}

/// Every finding of one window, in `RULES` order. A rule without data says nothing.
pub fn evaluate(w: &WindowStats, cfg: &AdviceConfig) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let sp = span(&w.name);
    let mut add = |rule: &str, severity: Severity, sentence: String, value: Option<f64>, unit: &str, points_at: &str| {
        out.push(Finding { rule: rule.into(), severity, window: w.name.clone(), sentence, value, unit: unit.into(), points_at: points_at.into() });
    };

    // 1. slot saturation
    if let (Some(v), true) = (w.slots_full_pct, w.slots > 0) {
        let sev = level(v, cfg.slots_full_watch_pct, cfg.slots_full_act_pct);
        let tail = match sev {
            Severity::Fine => "no capacity problem",
            Severity::Watch => "getting close to full; keep an eye on it",
            Severity::Act => "more slots (if memory allows) or a second server",
        };
        add("slots_saturated", sev, format!("All {} slots were busy {} of {sp} - {tail}.", w.slots, pct(v)), Some(v), "%", if sev == Severity::Fine { "" } else { "max-running-requests / a second server" });
    }

    // 2. waiting in line
    if let Some(v) = w.queue_pct {
        let sev = level(v, cfg.queue_watch_pct, cfg.queue_act_pct);
        let wait = w.queue_wait_p95_ms.filter(|ms| *ms > 0.0).map(|ms| format!(", p95 wait {}", secs(ms))).unwrap_or_default();
        let sentence = match sev {
            Severity::Fine => format!("Requests waited in line {} of the time{wait} - nobody is being held up.", pct(v)),
            Severity::Watch => format!("Requests waited in line {} of the time{wait} - starting to queue at busy moments.", pct(v)),
            Severity::Act => format!("Requests waited in line {} of the time{wait} - consider more slots or a second server.", pct(v)),
        };
        add("queue_wait", sev, sentence, Some(v), "%", if sev == Severity::Fine { "" } else { "max-running-requests / a second server" });
    }

    // 3. KV memory
    if let Some(peak) = w.kv_peak_pct {
        let retracted = w.retracted_pct.unwrap_or(0.0);
        let sev = if retracted > 0.0 { Severity::Act } else { level(peak, cfg.kv_watch_pct, cfg.kv_act_pct) };
        let p95 = w.kv_p95_pct.map(|p| format!(" (p95 {})", pct(p))).unwrap_or_default();
        let (sentence, points) = if retracted > 0.0 {
            (format!("Memory (KV) ran out: the engine paused and re-queued requests {} of the time, peak use {}{p95} - allow fewer simultaneous users or a shorter context, or add GPU memory.", pct(retracted), pct(peak)), "max-running-requests / context-length / more GPU memory")
        } else if sev != Severity::Fine {
            (format!("Memory (KV) peaked at {}{p95} - close to the limit; more users or longer prompts will start pausing requests.", pct(peak)), "max-running-requests / context-length / more GPU memory")
        } else if peak < cfg.kv_low_pct {
            (format!("Memory (KV) never passed {}{p95} - you could allow more simultaneous users.", pct(peak)), "max-running-requests (raise)")
        } else {
            (format!("Memory (KV) peaked at {}{p95} - comfortable.", pct(peak)), "")
        };
        add("kv_pressure", sev, sentence, Some(peak), "%", points);
    }

    // 4. prompt sizes against the configured context. The engine's histogram also counts the
    // monitor's own small probes, so a handful of requests says nothing about real prompts.
    let enough_prompts = w.prompt_requests.is_none_or(|n| n >= MIN_PROMPT_REQUESTS);
    if let (true, Some(max), Some(ctx)) = (enough_prompts, w.prompt_max, w.context_len.filter(|c| *c > 0.0)) {
        let share = max / ctx * 100.0;
        let upto = if w.prompt_max_is_bucket_edge { "at most " } else { "" };
        let typical = match (w.prompt_p50, w.prompt_p95) {
            (Some(a), Some(b)) => format!(" (typical {}, p95 {})", tokens(a), tokens(b)),
            _ => String::new(),
        };
        let oversized = share < cfg.context_unused_pct;
        let tail = if oversized { " - the context is larger than anyone uses; a smaller one would free memory for more users" } else { "" };
        add("prompt_sizes", if oversized { Severity::Watch } else { Severity::Fine }, format!("Longest real prompt was {upto}{} tokens of the {} reserved{typical}{tail}.", tokens(max), tokens(ctx)), Some(max), "tokens", if oversized { "context-length (lower)" } else { "" });
    }

    // evictions: remembered text thrown out of the KV cache to make room for new requests
    let evicted_over = w.evicted_secs.unwrap_or(w.covered_secs);
    if let Some(evicted) = w.evicted_tokens.filter(|_| evicted_over > 0) {
        let per_hour = evicted / (evicted_over as f64 / 3600.0);
        let count = |v: f64| if v >= 1e6 { format!("{:.1}M", v / 1e6) } else if v >= 1e3 { format!("{:.0}k", v / 1e3) } else { format!("{v:.0}") };
        if evicted <= 0.0 {
            add("evictions", Severity::Fine, format!("Nothing was thrown out of memory (KV) in {sp} - everything it had read stayed available for reuse."), Some(0.0), "tok/h", "");
        } else {
            let heavy = per_hour >= cfg.evictions_watch_per_hour;
            let tail = if heavy { " - repeated prompts are being read again instead of reused; more memory for the cache would speed them up" } else { " - normal housekeeping at this rate" };
            add("evictions", if heavy { Severity::Watch } else { Severity::Fine }, format!("Memory (KV) threw out {} tokens of remembered text in {sp} to make room (about {} per hour){tail}.", count(evicted), count(per_hour)), Some(per_hour.round()), "tok/h", if heavy { "mem-fraction-static (higher) / context-length (lower) / more GPU memory" } else { "" });
        }
    }

    // 5. prefix-cache reuse
    if let Some(v) = w.cache_hit_pct {
        let low = v < cfg.cache_low_pct;
        let sentence = if low { format!("Only {} of prompt tokens came from the prefix cache - prompts rarely repeat, so reading speed (prefill) matters most here.", pct(v)) } else { format!("{} of prompt tokens came from the prefix cache - repeated context is nearly free.", pct(v)) };
        add("cache_reuse", if low { Severity::Watch } else { Severity::Fine }, sentence, Some(v), "%", if low { "prefill speed / cache size" } else { "" });
    }

    // 6. speculative decoding, by concurrency
    let mut spec: Vec<(u64, f64)> = w.spec_by_level.iter().copied().filter(|(n, a)| *n > 0 && a.is_finite() && *a > 0.0).collect();
    spec.sort_by_key(|p| p.0);
    if let (Some(first), Some(last)) = (spec.first().copied(), spec.last().copied()) {
        let drop_pct = if first.0 != last.0 && first.1 > 0.0 { (first.1 - last.1) / first.1 * 100.0 } else { 0.0 };
        let (sev, sentence, points) = if first.1 < cfg.spec_low_accept {
            (Severity::Act, format!("Speculative decoding accepts only {:.1} tokens per step - the drafter is barely paying for itself; try another drafter or turn it off.", first.1), "speculative-decoding settings")
        } else if drop_pct > cfg.spec_drop_pct {
            (Severity::Watch, format!("Speculative decoding accepts {:.1} tokens per step with {} user{} but {:.1} with {} - it helps less under load; fewer draft tokens may serve a busy server better.", first.1, first.0, if first.0 == 1 { "" } else { "s" }, last.1, last.0), "speculative-num-draft-tokens")
        } else {
            (Severity::Fine, format!("Speculative decoding accepts {:.1} tokens per step - the shortcut is working.", first.1), "")
        };
        add("spec_decoding", sev, sentence, Some(first.1), "tokens/step", points);
    }

    // 7. reading vs writing time
    if let Some(v) = w.prefill_time_pct {
        let heavy = v > cfg.prefill_heavy_pct;
        let sentence = if heavy { format!("{} of request time went into reading prompts - faster prefill would help more than faster decoding.", pct(v)) } else { format!("{} of request time went into reading prompts, the rest into writing - a normal split.", pct(v)) };
        add("time_split", if heavy { Severity::Watch } else { Severity::Fine }, sentence, Some(v), "%", if heavy { "prefill speed (chunked-prefill-size, more GPUs)" } else { "" });
    }

    // 8. rejections, per lane
    let days = (w.covered_secs.max(1) as f64 / 86_400.0).max(1.0 / 24.0);
    for r in &w.rejects {
        let total = r.too_busy_429 + r.too_large_413;
        let per_day = total as f64 / days;
        let sev = level(per_day, cfg.rejects_watch_per_day, cfg.rejects_act_per_day);
        if total == 0 {
            continue;
        }
        let tail = if sev == Severity::Fine { "rare enough to ignore" } else if r.too_large_413 > r.too_busy_429 { "mostly oversized prompts: raise that lane's prompt limit if they are legitimate" } else { "mostly \"too busy\": raise that lane's limits or add capacity" };
        add("rejections", sev, format!("The {} lane turned away {:.0} request{} a day (too busy: {}, too large: {}) - {tail}.", r.lane, per_day, if per_day.round() == 1.0 { "" } else { "s" }, r.too_busy_429, r.too_large_413), Some(per_day), "per day", if sev == Severity::Fine { "" } else { "gateway lane limits" });
    }
    if !w.rejects.is_empty() && w.rejects.iter().all(|r| r.too_busy_429 + r.too_large_413 == 0) {
        add("rejections", Severity::Fine, format!("The gateway turned nobody away in {sp}."), Some(0.0), "per day", "");
    }

    // 9. growth, week over week
    if let Some(g) = w.growth.as_ref().filter(|g| g.prev_days_covered >= 3.0 && g.tokens_per_day_prev > 0.0) {
        let change = (g.tokens_per_day - g.tokens_per_day_prev) / g.tokens_per_day_prev * 100.0;
        let sev = if change > cfg.growth_watch_pct { Severity::Watch } else { Severity::Fine };
        let dir = if change >= 0.0 { "grew" } else { "fell" };
        let tail = if sev == Severity::Watch { " - at this pace the capacity findings above will change soon" } else { "" };
        add("growth", sev, format!("Tokens per day {dir} {:.0}% week over week ({} to {}); requests per day {:.0} to {:.0}; most users at once {:.0} to {:.0}{basis}{tail}.", change.abs(), tokens(g.tokens_per_day_prev), tokens(g.tokens_per_day), g.requests_per_day_prev, g.requests_per_day, g.peak_users_prev, g.peak_users, basis = g.peak_users_basis.as_deref().map(|b| format!(" ({b})")).unwrap_or_default()), Some(change), "%", if sev == Severity::Watch { "capacity planning" } else { "" });
    }

    // 10. energy
    if let Some(wh) = w.wh_per_mtok.filter(|v| *v > 0.0) {
        let made = w.gen_tokens.map(|t| format!(" ({} tokens written in {sp})", tokens(t))).unwrap_or_default();
        add("energy", Severity::Fine, format!("Energy: {:.0} Wh of GPU power per 1M generated tokens{made}.", wh), Some(wh), "Wh/1M tokens", "");
    }

    // 11. GPU throttling (GPUs in thermal_exclude are measured but never worded)
    let worded: Vec<&GpuThrottle> = w.throttle.iter().filter(|g| !g.excluded).collect();
    if !w.throttle.is_empty() {
        match worded.iter().copied().max_by(|a, b| a.pct.total_cmp(&b.pct)) {
            Some(worst) if worst.pct >= cfg.throttle_watch_pct => {
                let sev = level(worst.pct, cfg.throttle_watch_pct, cfg.throttle_act_pct);
                let others: Vec<String> = worded.iter().filter(|g| g.index != worst.index && g.pct >= cfg.throttle_watch_pct).map(|g| format!("GPU{} {}", g.index, pct(g.pct))).collect();
                let also = if others.is_empty() { String::new() } else { format!(" (also {})", others.join(", ")) };
                add("throttling", sev, format!("GPU{} slowed itself down to stay cool {} of {sp}{also} - check airflow and fan curves before buying anything.", worst.index, pct(worst.pct)), Some(worst.pct), "%", "cooling / airflow");
            }
            Some(_) => add("throttling", Severity::Fine, format!("No GPU throttled for heat in {sp}."), Some(0.0), "%", ""),
            None => {}
        }
    }

    // 12. where total speed stops growing
    match &w.saturation {
        Some(s @ Saturation::After { users, .. }) => {
            // a warning needs evidence: a benchmark measured it, or live traffic covered at
            // least three levels. Two levels of bursty traffic are an observation, not a ceiling.
            let solid = w.saturation_from_bench || w.saturation_levels >= 3;
            let below_slots = w.slots > 0 && *users < u64::from(w.slots);
            let wasted = below_slots && solid;
            let mut sentence = s.sentence();
            if let Some(first) = sentence.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            let tail = if wasted {
                format!(", but {} are allowed at once - beyond {users} each extra user only slows the others down", w.slots)
            } else if below_slots {
                " in the little traffic seen so far - too few load levels to call it a limit (`lss bench quick` measures it)".to_string()
            } else {
                String::new()
            };
            add("saturation_point", if wasted { Severity::Watch } else { Severity::Fine }, format!("{sentence}{tail}."), Some(*users as f64), "users", if wasted { "max-running-requests (lower) / faster GPUs for more throughput" } else { "" });
        }
        Some(s @ Saturation::StillGrowing { users, .. }) => {
            let mut sentence = s.sentence();
            if let Some(first) = sentence.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            add("saturation_point", Severity::Fine, format!("{sentence} - there is throughput left."), Some(*users as f64), "users", "");
        }
        _ => {}
    }

    // 13. the owner's targets
    if let Some(t) = w.targets.as_ref().filter(|t| t.measured()) {
        // an uptime of 99.95 % must not read "100%"
        let share = |r: &crate::targets::TargetRow, v: f64| if r.key == "uptime" { format!("{}%", ((v * 100.0).round() / 100.0)) } else { pct(v) };
        let said = |r: &crate::targets::TargetRow| format!("{} {} (share of {}, goal {})", r.met_pct.map_or_else(|| "-".into(), |m| share(r, m)), r.label, r.basis, share(r, r.goal_pct));
        let missed = t.missed();
        if let Some(worst) = missed.iter().max_by(|a, b| (a.goal_pct - a.met_pct.unwrap_or(0.0)).total_cmp(&(b.goal_pct - b.met_pct.unwrap_or(0.0)))) {
            let gap = worst.goal_pct - worst.met_pct.unwrap_or(0.0);
            // an uptime target is in hundredths of a percent; the others in whole points
            let sev = if gap >= if worst.key == "uptime" { 1.0 } else { 10.0 } { Severity::Act } else { Severity::Watch };
            let also = if missed.len() > 1 { format!(" ({} more missed)", missed.len() - 1) } else { String::new() };
            let points = match worst.key.as_str() {
                "ttft" => "reading speed: chunked-prefill-size / fewer long prompts at once / faster GPUs",
                "speed" => "max-running-requests (lower) / faster GPUs",
                "queue" => "max-running-requests / a second server",
                // card #231 (lss-verifier-4): the combined ALERTS & INCIDENTS page no longer
                // exists - it split into 8 ALERTS and 9 INCIDENTS - and this string is rendered on
                // the ADVICE page, so it sent a reader to a page they cannot open. "what went
                // down" is the INCIDENTS half.
                _ => "reliability: see 9 INCIDENTS for what went down",
            };
            add("targets", sev, format!("Target missed in {sp}: {}{also}.", said(worst)), worst.met_pct, "%", points);
        } else {
            let n = t.rows.iter().filter(|r| r.ok == Some(true)).count();
            let tightest = t.rows.iter().filter(|r| r.ok == Some(true)).min_by(|a, b| (a.met_pct.unwrap_or(0.0) - a.goal_pct).total_cmp(&(b.met_pct.unwrap_or(0.0) - b.goal_pct)));
            add("targets", Severity::Fine, format!("All {n} targets met in {sp}{}.", tightest.map(|r| format!(" - the closest: {}", said(r))).unwrap_or_default()), tightest.and_then(|r| r.met_pct), "%", "");
        }
    }
    out
}

/// Most pressing first: severity, then the order of `RULES`. A `fine` finding is still worth
/// showing when nothing else is: it is the evidence that nothing needs doing.
pub fn rank(findings: &[Finding]) -> Vec<Finding> {
    let mut v: Vec<Finding> = findings.to_vec();
    v.sort_by_key(|f| (std::cmp::Reverse(f.severity), RULES.iter().position(|r| *r == f.rule).unwrap_or(RULES.len())));
    v
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdviceWindow {
    pub stats: WindowStats,
    pub findings: Vec<Finding>,
}

/// `GET /advice`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdviceDoc {
    pub v: u32,
    pub generated_at: i64,
    /// `24h`, `7d`, `30d`
    pub windows: Vec<AdviceWindow>,
    /// the two most pressing findings (from the longest window that has a day of data)
    pub top: Vec<Finding>,
    pub thermal_exclude: Vec<u32>,
}

pub fn build(now: i64, stats: Vec<WindowStats>, cfg: &AdviceConfig, thermal_exclude: &[u32]) -> AdviceDoc {
    let windows: Vec<AdviceWindow> = stats.into_iter().map(|s| AdviceWindow { findings: rank(&evaluate(&s, cfg)), stats: s }).collect();
    // the week is the headline; before there is a day of it, the last 24 hours are
    let headline = windows.iter().find(|w| w.stats.name == "7d" && w.stats.covered_secs >= 86_400).or_else(|| windows.iter().find(|w| w.stats.name == "24h")).or(windows.first());
    let top = headline.map(|w| w.findings.iter().take(2).cloned().collect()).unwrap_or_default();
    AdviceDoc { v: crate::STATUS_SCHEMA_VERSION, generated_at: now, windows, top, thermal_exclude: thermal_exclude.to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn week() -> WindowStats {
        WindowStats { name: "7d".into(), secs: 7 * 86_400, covered_secs: 7 * 86_400, slots: 8, ..Default::default() }
    }

    fn one(w: &WindowStats, rule: &str) -> Finding {
        let all = evaluate(w, &AdviceConfig::default());
        let hits: Vec<&Finding> = all.iter().filter(|f| f.rule == rule).collect();
        assert_eq!(hits.len(), 1, "{rule}: {all:?}");
        hits[0].clone()
    }

    #[test]
    fn targets() {
        use crate::targets::{TargetRow, TargetsStatus};
        let row = |key: &str, label: &str, met: f64, goal: f64, basis: &str| TargetRow { key: key.into(), label: label.into(), short: key.into(), met_pct: Some(met), goal_pct: goal, basis: basis.into(), ok: Some(met >= goal) };
        let met = TargetsStatus { window: "7d".into(), rows: vec![row("ttft", "first word within 5 s", 99.1, 95.0, "requests"), row("uptime", "server up", 99.95, 99.0, "time")] };
        let f = one(&WindowStats { targets: Some(met), ..week() }, "targets");
        assert_eq!((f.severity, f.sentence.as_str(), f.points_at.as_str()), (Severity::Fine, "All 2 targets met in the week - the closest: 99.95% server up (share of time, goal 99%).", ""));
        let near = TargetsStatus { window: "7d".into(), rows: vec![row("queue", "waited under 2 s for a slot", 91.0, 95.0, "requests"), row("ttft", "first word within 5 s", 94.0, 95.0, "requests")] };
        let f = one(&WindowStats { targets: Some(near), ..week() }, "targets");
        assert_eq!((f.severity, f.sentence.as_str()), (Severity::Watch, "Target missed in the week: 91% waited under 2 s for a slot (share of requests, goal 95%) (1 more missed)."));
        assert!(f.points_at.contains("second server"));
        let far = TargetsStatus { window: "7d".into(), rows: vec![row("speed", "written at 30 tok/s or faster", 60.0, 90.0, "tokens")] };
        assert_eq!(one(&WindowStats { targets: Some(far), ..week() }, "targets").severity, Severity::Act);
        let down = TargetsStatus { window: "7d".into(), rows: vec![row("uptime", "server up", 97.5, 99.0, "time")] };
        assert_eq!(one(&WindowStats { targets: Some(down), ..week() }, "targets").severity, Severity::Act);
        // nothing measured: the rule says nothing
        let blank = TargetsStatus { window: "7d".into(), rows: vec![TargetRow { key: "ttft".into(), ..Default::default() }] };
        assert!(evaluate(&WindowStats { targets: Some(blank), ..week() }, &AdviceConfig::default()).is_empty());
    }

    #[test]
    fn slots_saturated() {
        let f = one(&WindowStats { slots_full_pct: Some(0.6), ..week() }, "slots_saturated");
        assert_eq!((f.severity, f.sentence.as_str(), f.points_at.as_str()), (Severity::Fine, "All 8 slots were busy 0.6% of the week - no capacity problem.", ""));
        assert_eq!(one(&WindowStats { slots_full_pct: Some(7.0), ..week() }, "slots_saturated").severity, Severity::Watch);
        let act = one(&WindowStats { slots_full_pct: Some(31.0), ..week() }, "slots_saturated");
        assert_eq!((act.severity, act.value), (Severity::Act, Some(31.0)));
        assert!(act.sentence.contains("busy 31% of the week") && act.points_at.contains("second server"));
        // unknown slots or no data: the rule says nothing
        assert!(evaluate(&WindowStats { slots: 0, slots_full_pct: Some(50.0), ..week() }, &AdviceConfig::default()).is_empty());
        assert!(evaluate(&week(), &AdviceConfig::default()).is_empty());
    }

    #[test]
    fn queue_wait() {
        let f = one(&WindowStats { queue_pct: Some(14.0), queue_wait_p95_ms: Some(9_000.0), ..week() }, "queue_wait");
        assert_eq!((f.severity, f.sentence.as_str()), (Severity::Act, "Requests waited in line 14% of the time, p95 wait 9.0 s - consider more slots or a second server."));
        assert_eq!(one(&WindowStats { queue_pct: Some(3.0), ..week() }, "queue_wait").severity, Severity::Watch);
        let fine = one(&WindowStats { queue_pct: Some(0.0), ..week() }, "queue_wait");
        assert_eq!((fine.severity, fine.sentence.as_str()), (Severity::Fine, "Requests waited in line 0% of the time - nobody is being held up."));
    }

    #[test]
    fn kv_pressure() {
        let low = one(&WindowStats { kv_peak_pct: Some(29.0), kv_p95_pct: Some(12.0), ..week() }, "kv_pressure");
        assert_eq!((low.severity, low.sentence.as_str(), low.points_at.as_str()), (Severity::Fine, "Memory (KV) never passed 29% (p95 12%) - you could allow more simultaneous users.", "max-running-requests (raise)"));
        assert!(one(&WindowStats { kv_peak_pct: Some(55.0), ..week() }, "kv_pressure").sentence.ends_with("comfortable."));
        assert_eq!(one(&WindowStats { kv_peak_pct: Some(85.0), ..week() }, "kv_pressure").severity, Severity::Watch);
        assert_eq!(one(&WindowStats { kv_peak_pct: Some(97.0), ..week() }, "kv_pressure").severity, Severity::Act);
        // a retraction is the engine saying it ran out, whatever the peak reading was
        let out = one(&WindowStats { kv_peak_pct: Some(70.0), retracted_pct: Some(1.2), ..week() }, "kv_pressure");
        assert!(out.severity == Severity::Act && out.sentence.contains("paused and re-queued requests 1.2% of the time"), "{}", out.sentence);
    }

    #[test]
    fn prompt_sizes() {
        let f = one(&WindowStats { prompt_max: Some(490_000.0), context_len: Some(1_048_576.0), prompt_p50: Some(3_100.0), prompt_p95: Some(41_000.0), ..week() }, "prompt_sizes");
        assert_eq!((f.severity, f.points_at.as_str()), (Severity::Watch, "context-length (lower)"));
        assert!(f.sentence.starts_with("Longest real prompt was 490k tokens of the 1M reserved (typical 3.1k, p95 41k)"), "{}", f.sentence);
        let used = one(&WindowStats { prompt_max: Some(900_000.0), context_len: Some(1_048_576.0), ..week() }, "prompt_sizes");
        assert_eq!((used.severity, used.sentence.as_str()), (Severity::Fine, "Longest real prompt was 900k tokens of the 1M reserved."));
        let edge = one(&WindowStats { prompt_max: Some(131_072.0), prompt_max_is_bucket_edge: true, context_len: Some(200_000.0), ..week() }, "prompt_sizes");
        assert!(edge.sentence.contains("was at most 131k tokens"));
        // fifteen requests, all of them the monitor's own 50-token probes, are not evidence that
        // the context is oversized
        let probes_only = WindowStats { prompt_max: Some(100.0), prompt_max_is_bucket_edge: true, prompt_requests: Some(15.0), context_len: Some(1_048_576.0), ..week() };
        assert!(evaluate(&probes_only, &AdviceConfig::default()).is_empty());
        assert_eq!(one(&WindowStats { prompt_requests: Some(100.0), ..probes_only }, "prompt_sizes").severity, Severity::Watch);
    }

    #[test]
    fn cache_reuse() {
        assert_eq!(one(&WindowStats { cache_hit_pct: Some(99.6), ..week() }, "cache_reuse").sentence, "100% of prompt tokens came from the prefix cache - repeated context is nearly free.");
        let low = one(&WindowStats { cache_hit_pct: Some(12.0), ..week() }, "cache_reuse");
        assert!(low.severity == Severity::Watch && low.sentence.starts_with("Only 12% of prompt tokens"));
    }

    #[test]
    fn evictions() {
        let none = one(&WindowStats { evicted_tokens: Some(0.0), ..week() }, "evictions");
        assert_eq!((none.severity, none.sentence.as_str()), (Severity::Fine, "Nothing was thrown out of memory (KV) in the week - everything it had read stayed available for reuse."));
        let light = one(&WindowStats { evicted_tokens: Some(8_400_000.0), ..week() }, "evictions");
        assert_eq!((light.severity, light.value, light.sentence.as_str()), (Severity::Fine, Some(50_000.0), "Memory (KV) threw out 8.4M tokens of remembered text in the week to make room (about 50k per hour) - normal housekeeping at this rate."));
        let heavy = one(&WindowStats { evicted_tokens: Some(504_000_000.0), ..week() }, "evictions");
        assert_eq!((heavy.severity, heavy.points_at.as_str()), (Severity::Watch, "mem-fraction-static (higher) / context-length (lower) / more GPU memory"));
        assert!(heavy.sentence.contains("about 3.0M per hour") && heavy.sentence.contains("read again instead of reused"), "{}", heavy.sentence);
        // the eviction counter is younger than the window: the rate is over ITS seconds
        let young = one(&WindowStats { evicted_tokens: Some(8_400_000.0), evicted_secs: Some(2 * 86_400), ..week() }, "evictions");
        assert_eq!(young.value, Some(175_000.0), "8.4M over the 2 days it was counted, not over the week");
        // not measured = no finding
        assert!(!evaluate(&WindowStats { evicted_tokens: None, ..week() }, &AdviceConfig::default()).iter().any(|f| f.rule == "evictions"));
    }

    #[test]
    fn spec_decoding() {
        let good = one(&WindowStats { spec_by_level: vec![(1, 5.9), (2, 5.6), (8, 5.1)], ..week() }, "spec_decoding");
        assert_eq!((good.severity, good.sentence.as_str()), (Severity::Fine, "Speculative decoding accepts 5.9 tokens per step - the shortcut is working."));
        let drops = one(&WindowStats { spec_by_level: vec![(8, 3.1), (1, 5.9)], ..week() }, "spec_decoding");
        assert!(drops.severity == Severity::Watch && drops.sentence.contains("5.9 tokens per step with 1 user but 3.1 with 8"), "{}", drops.sentence);
        let poor = one(&WindowStats { spec_by_level: vec![(1, 1.3)], ..week() }, "spec_decoding");
        assert!(poor.severity == Severity::Act && poor.sentence.contains("only 1.3 tokens per step"));
        assert!(evaluate(&WindowStats { spec_by_level: vec![(1, 0.0)], ..week() }, &AdviceConfig::default()).is_empty(), "speculative decoding off: nothing to say");
    }

    #[test]
    fn time_split() {
        assert!(one(&WindowStats { prefill_time_pct: Some(19.0), ..week() }, "time_split").sentence.contains("a normal split"));
        let heavy = one(&WindowStats { prefill_time_pct: Some(72.0), ..week() }, "time_split");
        assert!(heavy.severity == Severity::Watch && heavy.sentence.starts_with("72% of request time went into reading prompts"));
    }

    #[test]
    fn rejections() {
        let w = WindowStats { rejects: vec![LaneRejects { lane: "public".into(), too_busy_429: 420, too_large_413: 21 }, LaneRejects { lane: "trusted".into(), ..Default::default() }], ..week() };
        let f = one(&w, "rejections");
        assert_eq!((f.severity, f.sentence.as_str()), (Severity::Act, "The public lane turned away 63 requests a day (too busy: 420, too large: 21) - mostly \"too busy\": raise that lane's limits or add capacity."));
        let big = one(&WindowStats { rejects: vec![LaneRejects { lane: "trusted".into(), too_busy_429: 1, too_large_413: 60 }], ..week() }, "rejections");
        assert!(big.severity == Severity::Watch && big.sentence.contains("oversized prompts"));
        let none = one(&WindowStats { rejects: vec![LaneRejects { lane: "public".into(), ..Default::default() }], ..week() }, "rejections");
        assert_eq!((none.severity, none.sentence.as_str()), (Severity::Fine, "The gateway turned nobody away in the week."));
        let few = one(&WindowStats { rejects: vec![LaneRejects { lane: "public".into(), too_busy_429: 7, ..Default::default() }], ..week() }, "rejections");
        assert!(few.severity == Severity::Fine && few.sentence.contains("rare enough to ignore"));
    }

    #[test]
    fn growth() {
        let g = Growth { tokens_per_day: 2_200_000.0, tokens_per_day_prev: 1_200_000.0, requests_per_day: 900.0, requests_per_day_prev: 610.0, peak_users: 6.0, peak_users_prev: 3.0, prev_days_covered: 7.0, peak_users_basis: None };
        let f = one(&WindowStats { growth: Some(g.clone()), ..week() }, "growth");
        assert_eq!(f.severity, Severity::Watch);
        assert!(f.sentence.starts_with("Tokens per day grew 83% week over week (1.2M to 2.2M); requests per day 610 to 900; most users at once 3 to 6"), "{}", f.sentence);
        let steady = one(&WindowStats { growth: Some(Growth { tokens_per_day: 1_100_000.0, ..g.clone() }), ..week() }, "growth");
        assert!(steady.severity == Severity::Fine && steady.sentence.contains("fell 8%"));
        // without a real earlier week there is no growth figure
        assert!(evaluate(&WindowStats { growth: Some(Growth { prev_days_covered: 1.0, ..g }), ..week() }, &AdviceConfig::default()).is_empty());
    }

    #[test]
    fn energy() {
        let f = one(&WindowStats { wh_per_mtok: Some(648.7), gen_tokens: Some(1_430_000.0), ..week() }, "energy");
        assert_eq!((f.severity, f.sentence.as_str()), (Severity::Fine, "Energy: 649 Wh of GPU power per 1M generated tokens (1.4M tokens written in the week)."));
    }

    #[test]
    fn throttling_leaves_the_excluded_gpus_out_of_the_wording() {
        let t = |pcts: [f64; 4], excluded: &[u32]| WindowStats { throttle: pcts.iter().enumerate().map(|(i, p)| GpuThrottle { index: i as u32, pct: *p, excluded: excluded.contains(&(i as u32)) }).collect(), ..week() };
        // GPU0 throttles a lot but is excluded (a known hot slot): it is never named
        let f = one(&t([38.0, 0.0, 4.2, 0.0], &[0]), "throttling");
        assert_eq!(f.severity, Severity::Watch);
        assert!(f.sentence.starts_with("GPU2 slowed itself down to stay cool 4.2% of the week") && !f.sentence.contains("GPU0"), "{}", f.sentence);
        let calm = one(&t([38.0, 0.0, 0.2, 0.0], &[0]), "throttling");
        assert_eq!((calm.severity, calm.sentence.as_str()), (Severity::Fine, "No GPU throttled for heat in the week."));
        let bad = one(&t([0.0, 14.0, 3.0, 0.0], &[]), "throttling");
        assert!(bad.severity == Severity::Act && bad.sentence.contains("GPU1") && bad.sentence.contains("(also GPU2 3.0%)"), "{}", bad.sentence);
        // every GPU excluded: nothing to word
        assert!(evaluate(&t([50.0, 50.0, 50.0, 50.0], &[0, 1, 2, 3]), &AdviceConfig::default()).is_empty());
    }

    #[test]
    fn saturation_point() {
        let f = one(&WindowStats { saturation: Some(Saturation::After { users: 4, total_tok_s: 640.0, from_bench: true }), saturation_from_bench: true, ..week() }, "saturation_point");
        // the same claim from two levels of live traffic is an observation, never a warning
        let thin = one(&WindowStats { saturation: Some(Saturation::After { users: 1, total_tok_s: 252.0, from_bench: true }), saturation_levels: 2, ..week() }, "saturation_point");
        assert_eq!(thin.severity, Severity::Fine);
        assert!(thin.sentence.contains("too few load levels to call it a limit") && thin.points_at.is_empty(), "{}", thin.sentence);
        assert_eq!(one(&WindowStats { saturation: Some(Saturation::After { users: 4, total_tok_s: 640.0, from_bench: true }), saturation_levels: 3, ..week() }, "saturation_point").severity, Severity::Watch);
        assert_eq!(f.severity, Severity::Watch);
        assert!(f.sentence.starts_with("Total speed stops growing after 4 users (about 640 tok/s in total), but 8 are allowed at once"), "{}", f.sentence);
        let full = one(&WindowStats { saturation: Some(Saturation::After { users: 8, total_tok_s: 900.0, from_bench: true }), ..week() }, "saturation_point");
        assert_eq!((full.severity, full.sentence.as_str()), (Severity::Fine, "Total speed stops growing after 8 users (about 900 tok/s in total)."));
        let growing = one(&WindowStats { saturation: Some(Saturation::StillGrowing { users: 8, total_tok_s: 1100.0 }), ..week() }, "saturation_point");
        assert!(growing.sentence.ends_with("there is throughput left."));
        assert!(evaluate(&WindowStats { saturation: Some(Saturation::NotEnough), ..week() }, &AdviceConfig::default()).is_empty());
    }

    #[test]
    fn the_top_two_are_the_most_pressing_of_the_week_and_every_rule_is_documented() {
        let stats = |name: &str, covered: i64| WindowStats { name: name.into(), secs: 86_400, covered_secs: covered, slots: 8, slots_full_pct: Some(0.6), queue_pct: Some(14.0), kv_peak_pct: Some(29.0), wh_per_mtok: Some(650.0), retracted_pct: None, ..Default::default() };
        let doc = build(1_000, vec![stats("24h", 86_400), stats("7d", 5 * 86_400), stats("30d", 5 * 86_400)], &AdviceConfig::default(), &[0]);
        assert_eq!(doc.windows.len(), 3);
        assert_eq!(doc.top.iter().map(|f| (f.rule.as_str(), f.severity, f.window.as_str())).collect::<Vec<_>>(), vec![("queue_wait", Severity::Act, "7d"), ("slots_saturated", Severity::Fine, "7d")]);
        assert_eq!(doc.thermal_exclude, vec![0]);
        // a collector that started an hour ago: the 24 h window is the headline
        let young = build(1_000, vec![stats("24h", 3_600), stats("7d", 3_600)], &AdviceConfig::default(), &[]);
        assert_eq!(young.top[0].window, "24h");
        assert!(build(1_000, vec![], &AdviceConfig::default(), &[]).top.is_empty());
        // docs/ADVICE.md names every rule and every threshold key
        let md = include_str!("../../../docs/ADVICE.md");
        for rule in RULES {
            assert!(md.contains(&format!("`{rule}`")), "docs/ADVICE.md does not document the rule `{rule}`");
        }
        let cfg = serde_json::to_value(AdviceConfig::default()).unwrap();
        for key in cfg.as_object().unwrap().keys() {
            assert!(md.contains(&format!("`{key}`")), "docs/ADVICE.md does not document the threshold `{key}`");
        }
        assert_eq!((Severity::Act.as_str(), Severity::Watch > Severity::Fine), ("act", true));
    }
}
