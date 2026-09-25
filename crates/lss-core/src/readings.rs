//! READINGS (card #179, after nixfred/pulse): every reading is ONE uniform record, and every
//! surface - the headline sentence, the worst-first ranking, the per-GPU boxes - is a projection
//! of that one list. Adding a domain means adding readings; nothing downstream changes.
//!
//! Unlike things are scored on one scale by `ramp(value, at, full)`: `at` scores 0 and `full`
//! scores 1 in EITHER direction, so missing headroom (`ramp(hit_pct, 55.0, 5.0)`) and excess heat
//! (`ramp(temp_c, 78.0, 92.0)`) land on the same 0..1 axis and can be compared.
//!
//! Three rules this module enforces so the renderers cannot get them wrong:
//! * only an OFFLINE reading scores 1.0; every live score is clamped to `CEILING`, so a dead
//!   collector or engine always outranks a saturated one;
//! * MISSING is not ZERO: a reading with no input is `State::NoData` (scores 0, renders an em dash
//!   with its reason), which is a different fact from a live reading that measured nothing;
//! * an `informational` row (cost, model id, launch flags) shows and sorts but can NEVER lead -
//!   facts about the setup kept outscoring real problems on an idle machine in pulse.
//!
//! Pure: no I/O, no clocks.

use serde::{Deserialize, Serialize};

/// Below this a reading is fine.
pub const LOW: f64 = 0.34;
/// From here a reading is worth watching; from `MEDIUM` it is high; from `HIGH` it is critical.
pub const MEDIUM: f64 = 0.62;
pub const HIGH: f64 = 0.85;
/// The most a LIVE reading can score. 1.0 is reserved for `State::Offline`.
pub const CEILING: f64 = 0.97;
/// The score of a dead source - exclusive, nothing live can reach it.
pub const OFFLINE: f64 = 1.0;
/// A challenger must beat the current headline by this much to replace it, so the headline does
/// not flicker between two readings a hair apart. Offline takes the headline immediately.
pub const HYSTERESIS: f64 = 0.08;

/// Why a reading has (or lacks) a value. Lives in the RECORD, not the scorer: an idle GPU and a
/// dead collector are both "no number" to a naive ramp and they are opposite facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// measured now
    Live,
    /// nothing to measure from (not configured, not reported by this engine, too early) - not a fault
    NoData { reason: String },
    /// the source that should report this is down or stale - the worst thing a reading can say
    Offline { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// stable identity, e.g. `serve.kv`, `gpu.2.temp`; the headline's hysteresis follows it
    pub key: String,
    /// short name for a label column, e.g. "KV cache"
    pub label: String,
    /// the display value, already formatted; `None` only when the state is not `Live`
    pub value: Option<String>,
    /// ONE sentence saying what this reading costs the reader - the only place its explanation
    /// lives. For NoData / Offline, the reason.
    pub detail: String,
    /// 0..=CEILING for live readings, exactly OFFLINE for offline, 0 for no data
    pub severity: f64,
    /// shows and sorts, never leads
    pub informational: bool,
    #[serde(flatten)]
    pub state: State,
}

impl Reading {
    /// A live reading. `score` is clamped to 0..=CEILING (a NaN scores 0).
    pub fn live(key: impl Into<String>, label: impl Into<String>, value: impl Into<String>, detail: impl Into<String>, score: f64) -> Self {
        Reading {
            key: key.into(),
            label: label.into(),
            value: Some(value.into()),
            detail: detail.into(),
            severity: clamp01(score).min(CEILING),
            informational: false,
            state: State::Live,
        }
    }

    /// No input to measure from. Scores 0; renders as an em dash with `reason`.
    pub fn no_data(key: impl Into<String>, label: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Reading {
            key: key.into(),
            label: label.into(),
            value: None,
            detail: reason.clone(),
            severity: 0.0,
            informational: false,
            state: State::NoData { reason },
        }
    }

    /// The source is down or stale. Scores exactly `OFFLINE`, which nothing live can reach.
    pub fn offline(key: impl Into<String>, label: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Reading {
            key: key.into(),
            label: label.into(),
            value: None,
            detail: reason.clone(),
            severity: OFFLINE,
            informational: false,
            state: State::Offline { reason },
        }
    }

    /// Mark as a fact about the setup (cost, model id, flags): it shows and sorts, never leads.
    pub fn informational(mut self) -> Self {
        self.informational = true;
        self
    }

    pub fn band(&self) -> Band {
        match self.state {
            State::Offline { .. } => Band::Offline,
            State::NoData { .. } => Band::NoData,
            State::Live => Band::of(self.severity),
        }
    }

    /// The display value, or an em dash when there is none (the reason is in `detail`).
    pub fn value_or_dash(&self) -> &str {
        self.value.as_deref().unwrap_or("\u{2014}")
    }
}

/// Where a reading sits on the one severity scale. Every surface takes its WORD from here, so two
/// domains can never describe the same band differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    NoData,
    Ok,
    Watch,
    High,
    Critical,
    Offline,
}

impl Band {
    /// The band of a live score.
    pub fn of(score: f64) -> Band {
        if score >= HIGH {
            Band::Critical
        } else if score >= MEDIUM {
            Band::High
        } else if score >= LOW {
            Band::Watch
        } else {
            Band::Ok
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Band::NoData => "no data",
            Band::Ok => "ok",
            Band::Watch => "watch",
            Band::High => "high",
            Band::Critical => "critical",
            Band::Offline => "offline",
        }
    }
}

fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}

/// 0 at `at`, 1 at `full`, linear between and clamped outside - in EITHER direction:
/// `ramp(t, 78.0, 92.0)` scores heat, `ramp(hit, 55.0, 5.0)` scores a falling cache hit rate.
/// A NaN value scores 0. If `at == full` it is a step: 1 at or past `full`, else 0.
pub fn ramp(value: f64, at: f64, full: f64) -> f64 {
    if value.is_nan() {
        return 0.0;
    }
    if at == full {
        return if value >= full { 1.0 } else { 0.0 };
    }
    clamp01((value - at) / (full - at))
}

/// Corroboration: a score counts only when a second source confirms the condition NOW (a queue
/// wait only if something is actually waiting; throttle seconds only if the driver reports a
/// reason at this moment). Unconfirmed, it scores 0 - a single-source signal must not lead.
pub fn corroborate(score: f64, confirmed: bool) -> f64 {
    if confirmed {
        score
    } else {
        0.0
    }
}

/// Worst first. Ties break by key so the order is stable between refreshes. Informational rows
/// are ranked with the rest - they show and sort; they only cannot lead.
pub fn rank(all: &[Reading]) -> Vec<&Reading> {
    let mut v: Vec<&Reading> = all.iter().collect();
    v.sort_by(|a, b| b.severity.total_cmp(&a.severity).then_with(|| a.key.cmp(&b.key)));
    v
}

/// The worst non-informational reading of a group (e.g. one GPU's temp/throttle/power), or None.
pub fn worst_of(all: &[Reading]) -> Option<&Reading> {
    rank(all).into_iter().find(|r| !r.informational)
}

/// The headline: the worst NON-informational reading at or above `LOW`, or None when nothing
/// deserves one. `incumbent` is the key that led last time: it keeps the headline unless a
/// challenger beats it by `HYSTERESIS`, or the challenger is offline and the incumbent is not.
pub fn leading<'a>(all: &'a [Reading], incumbent: Option<&str>) -> Option<&'a Reading> {
    let best = worst_of(all).filter(|r| r.severity >= LOW)?;
    let Some(inc) = incumbent.and_then(|k| all.iter().find(|r| r.key == k && !r.informational && r.severity >= LOW)) else {
        return Some(best);
    };
    if inc.key == best.key {
        return Some(inc);
    }
    let best_offline = matches!(best.state, State::Offline { .. });
    let inc_offline = matches!(inc.state, State::Offline { .. });
    if best_offline && !inc_offline {
        return Some(best);
    }
    if best.severity >= inc.severity + HYSTERESIS {
        Some(best)
    } else {
        Some(inc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(key: &str, score: f64) -> Reading {
        Reading::live(key, key, "v", "d", score)
    }

    #[test]
    fn ramp_scores_heat_upward() {
        assert_eq!(ramp(78.0, 78.0, 92.0), 0.0);
        assert_eq!(ramp(92.0, 78.0, 92.0), 1.0);
        assert!((ramp(85.0, 78.0, 92.0) - 0.5).abs() < 1e-12);
        assert_eq!(ramp(60.0, 78.0, 92.0), 0.0, "below `at` clamps to 0");
        assert_eq!(ramp(99.0, 78.0, 92.0), 1.0, "past `full` clamps to 1");
    }

    #[test]
    fn ramp_scores_missing_headroom_downward() {
        // a prefix-cache hit rate: fine at 55 %, worst at 5 %
        assert_eq!(ramp(55.0, 55.0, 5.0), 0.0);
        assert_eq!(ramp(5.0, 55.0, 5.0), 1.0);
        assert!((ramp(30.0, 55.0, 5.0) - 0.5).abs() < 1e-12);
        assert_eq!(ramp(98.0, 55.0, 5.0), 0.0, "a high hit rate is fine, not negative");
        assert_eq!(ramp(0.0, 55.0, 5.0), 1.0);
    }

    #[test]
    fn ramp_edge_cases_never_panic_or_lie() {
        assert_eq!(ramp(f64::NAN, 0.0, 1.0), 0.0);
        assert_eq!(ramp(5.0, 5.0, 5.0), 1.0, "degenerate ramp is a step at `full`");
        assert_eq!(ramp(4.9, 5.0, 5.0), 0.0);
        assert_eq!(ramp(f64::INFINITY, 0.0, 1.0), 1.0);
    }

    #[test]
    fn only_offline_reaches_one() {
        let saturated = Reading::live("serve.kv", "KV cache", "100%", "d", 1.0);
        assert_eq!(saturated.severity, CEILING, "a live reading is capped below offline");
        let beyond = Reading::live("x", "x", "v", "d", 7.0);
        assert_eq!(beyond.severity, CEILING);
        let dead = Reading::offline("collector", "collector", "no sample for 90 s");
        assert_eq!(dead.severity, OFFLINE);
        assert!(dead.severity > saturated.severity);
        assert_eq!(dead.band(), Band::Offline);
        assert_eq!(dead.value_or_dash(), "\u{2014}");
    }

    #[test]
    fn a_dead_source_outranks_a_saturated_one() {
        let all = vec![live("serve.kv", 1.0), Reading::offline("engine", "engine", "/metrics timed out")];
        assert_eq!(leading(&all, None).unwrap().key, "engine");
        assert_eq!(rank(&all)[0].key, "engine");
    }

    #[test]
    fn missing_is_not_zero_and_not_offline() {
        let nd = Reading::no_data("cost", "cost", "no rates.toml configured - cost tracking is off");
        assert_eq!(nd.severity, 0.0);
        assert_eq!(nd.band(), Band::NoData);
        assert_eq!(nd.value, None);
        assert_eq!(nd.value_or_dash(), "\u{2014}");
        assert_eq!(nd.detail, "no rates.toml configured - cost tracking is off", "the dash carries its reason");
        let idle = live("gpu.0.util", 0.0);
        assert_eq!(idle.band(), Band::Ok);
        assert_ne!(nd.band(), idle.band(), "no data and a measured zero are different facts");
        assert!(leading(&[nd], None).is_none(), "no data never takes the headline");
    }

    #[test]
    fn an_informational_row_can_never_lead_however_high_it_scores() {
        let cost = Reading::live("cost.usd_per_hour", "cost", "$9.99/h", "d", 1.0).informational();
        let model = Reading::live("loadout.model", "model", "m", "d", 0.9).informational();
        let real = live("gpu.1.temp", 0.40);
        let all = vec![cost.clone(), model.clone(), real];
        assert_eq!(leading(&all, None).unwrap().key, "gpu.1.temp");
        // ...even when it is named as the incumbent
        assert_eq!(leading(&all, Some("cost.usd_per_hour")).unwrap().key, "gpu.1.temp");
        // ...and with nothing else on the page there is no headline at all, not a cost headline
        assert!(leading(&[cost.clone(), model], None).is_none());
        // it still shows and sorts
        assert_eq!(rank(&all)[0].key, "cost.usd_per_hour");
        assert_eq!(worst_of(&all).unwrap().key, "gpu.1.temp");
    }

    #[test]
    fn nothing_below_low_takes_the_headline() {
        let all = vec![live("a", LOW - 0.01), live("b", 0.0)];
        assert!(leading(&all, None).is_none());
        assert_eq!(leading(&[live("a", LOW)], None).unwrap().key, "a", "LOW itself is enough");
    }

    #[test]
    fn the_headline_has_hysteresis() {
        let inc = live("serve.kv", 0.50);
        // a challenger a hair ahead does not steal it
        let all = vec![inc.clone(), live("gpu.0.temp", 0.50 + HYSTERESIS - 0.01)];
        assert_eq!(leading(&all, Some("serve.kv")).unwrap().key, "serve.kv");
        // one that beats it by the margin does
        let all = vec![inc.clone(), live("gpu.0.temp", 0.50 + HYSTERESIS)];
        assert_eq!(leading(&all, Some("serve.kv")).unwrap().key, "gpu.0.temp");
        // with no incumbent, the plain worst leads
        let all = vec![inc.clone(), live("gpu.0.temp", 0.51)];
        assert_eq!(leading(&all, None).unwrap().key, "gpu.0.temp");
        // an incumbent that has dropped below LOW (or vanished) holds nothing
        let all = vec![live("serve.kv", 0.1), live("gpu.0.temp", 0.40)];
        assert_eq!(leading(&all, Some("serve.kv")).unwrap().key, "gpu.0.temp");
        assert_eq!(leading(&all, Some("gone")).unwrap().key, "gpu.0.temp");
    }

    #[test]
    fn offline_takes_the_headline_immediately_but_two_offlines_do_not_flicker() {
        let all = vec![live("serve.kv", CEILING), Reading::offline("gate", "gateway", "down")];
        assert_eq!(leading(&all, Some("serve.kv")).unwrap().key, "gate");
        let all = vec![Reading::offline("engine", "engine", "down"), Reading::offline("gate", "gateway", "down")];
        assert_eq!(leading(&all, Some("gate")).unwrap().key, "gate");
        assert_eq!(leading(&all, None).unwrap().key, "engine", "ties break by key");
    }

    #[test]
    fn corroboration_zeroes_an_unconfirmed_signal() {
        assert_eq!(corroborate(0.9, false), 0.0);
        assert_eq!(corroborate(0.9, true), 0.9);
        let wait = Reading::live("serve.queue_wait", "queue wait", "3 s", "d", corroborate(ramp(3000.0, 200.0, 5000.0), false));
        assert!(leading(&[wait], None).is_none(), "a queue wait with nobody waiting cannot lead");
    }

    #[test]
    fn bands_and_their_words_come_from_one_place() {
        assert_eq!(Band::of(0.0), Band::Ok);
        assert_eq!(Band::of(LOW - 1e-9), Band::Ok);
        assert_eq!(Band::of(LOW), Band::Watch);
        assert_eq!(Band::of(MEDIUM), Band::High);
        assert_eq!(Band::of(HIGH), Band::Critical);
        assert_eq!(Band::of(CEILING), Band::Critical);
        let all = [Band::NoData, Band::Ok, Band::Watch, Band::High, Band::Critical, Band::Offline];
        let words: std::collections::HashSet<_> = all.iter().map(|b| b.word()).collect();
        assert_eq!(words.len(), all.len(), "every band has its own word");
        // same score in two domains => same word
        assert_eq!(live("serve.kv", 0.7).band().word(), live("gpu.3.temp", 0.7).band().word());
    }

    #[test]
    fn rank_is_worst_first_and_stable() {
        let all = vec![live("b", 0.5), live("a", 0.5), live("c", 0.9), Reading::no_data("d", "d", "r")];
        let keys: Vec<_> = rank(&all).iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["c", "a", "b", "d"]);
    }

    #[test]
    fn the_record_serialises_with_its_state_inline() {
        let j = serde_json::to_value(Reading::no_data("cost", "cost", "off")).unwrap();
        assert_eq!(j["state"], "no_data");
        assert_eq!(j["reason"], "off");
        assert_eq!(j["value"], serde_json::Value::Null);
        let back: Reading = serde_json::from_value(serde_json::to_value(live("k", 0.5)).unwrap()).unwrap();
        assert_eq!(back.state, State::Live);
        assert_eq!(back.severity, 0.5);
    }
}

// ============================================================================================
// STEP 2 (card #179): one readings() per domain over the /status model. Every surface is a
// projection of `all()`. The `detail` sentence is the ONE place a reading's explanation lives:
// what it COSTS the reader, never a threshold restated.
// ============================================================================================

use crate::model::{AdmitVerdict, GpuStatus, Status};

fn pct0(x: f64) -> String {
    format!("{x:.0}%")
}

fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn tok(x: f64) -> String {
    if x >= 1e6 {
        format!("{:.1}M", x / 1e6)
    } else if x >= 1e3 {
        format!("{:.1}k", x / 1e3)
    } else {
        format!("{x:.0}")
    }
}

/// How long without a sample before the collector itself counts as offline: three polls, and
/// never under 30 s so one slow poll is not an outage.
fn collector_stale_after(s: &Status) -> i64 {
    (s.collector.poll_secs as i64 * 3).max(30)
}

/// Every reading on the page, in no particular order - `rank` / `leading` decide the order.
pub fn all(s: &Status, now: i64) -> Vec<Reading> {
    let mut v = collector(s, now);
    v.extend(serve(s, now));
    v.extend(gpus(s));
    v.extend(gateway(s));
    v.extend(cost(s));
    v.extend(loadout(s));
    v.extend(events(s, now));
    v
}

/// The collector is the source of every other number: when it goes quiet it must outrank all of
/// them, because everything else on the page is then old news.
pub fn collector(s: &Status, now: i64) -> Vec<Reading> {
    match s.collector.last_sample_ts {
        Some(ts) if now - ts > collector_stale_after(s) => vec![Reading::offline(
            "collector",
            "collector",
            format!("No sample for {}: every number on this page is at least that old.", dur(now - ts)),
        )],
        _ => vec![],
    }
}

pub fn serve(s: &Status, now: i64) -> Vec<Reading> {
    let sv = &s.serve;
    if !sv.up {
        let since = sv.down_since.map(|t| format!(" for {}", dur(now - t))).unwrap_or_default();
        return vec![Reading::offline("serve.up", "engine", format!("The model server has not answered{since}: nothing reaches the model until it is back."))];
    }
    let mut v = Vec::new();
    let verdict = s.admission_verdict();

    // running but silent - the shape that came before the Xid-8 GPU hangs (card #124): 10 s is
    // normal jitter, 90 s is a hang
    if let Some(secs) = sv.secs_since_last_token.filter(|_| sv.running > 0.0) {
        v.push(Reading::live("serve.silent", "engine", format!("{secs}s silent"),
            format!("{:.0} requests are running but no token has come out for {secs}s - the pattern that came before the GPU hangs.", sv.running),
            ramp(secs as f64, 10.0, 90.0)));
    }

    // KV cache
    if sv.is_na("kv_usage") {
        v.push(Reading::no_data("serve.kv", "KV cache", format!("{} does not report KV cache use.", sv.engine_label())));
    } else {
        let p = sv.kv_usage * 100.0;
        v.push(Reading::live("serve.kv", "KV cache", pct0(p),
            "When the KV cache fills, new requests wait for a running one to finish, however idle the GPUs look.",
            ramp(p, 80.0, 100.0)));
    }

    // evictions, as whole-cache refills per hour
    match (sv.evicted_tok_per_hour_10m, sv.kv_max_tokens > 0.0) {
        (Some(e), true) => {
            let turns = e / sv.kv_max_tokens;
            v.push(Reading::live("serve.evictions", "evictions", format!("{} tok/h", tok(e)),
                "Cached prompts are pushed out to make room, so a returning conversation pays to read its whole history again.",
                ramp(turns, 0.5, 5.0)));
        }
        (Some(_), false) => v.push(Reading::no_data("serve.evictions", "evictions", "The KV cache size is unknown, so evictions cannot be put in proportion.")),
        (None, _) => v.push(Reading::no_data("serve.evictions", "evictions", "No eviction samples in the last 10 minutes yet.")),
    }

    // queue depth - against the owner's queue alert line when one is set, else the slot count
    // (a queue as deep as the batch means a whole batch waits)
    let qfull = if s.thresholds.queue_reqs > 0.0 { s.thresholds.queue_reqs * 1.5 } else { f64::from(sv.slots.max(1)) };
    v.push(if sv.queue > 0.0 {
        Reading::live("serve.queue", "queue", format!("{:.0} waiting", sv.queue),
            "Requests are waiting for a free slot: each one's first word is delayed until a running answer finishes.",
            ramp(sv.queue, 0.0, qfull))
    } else {
        Reading::live("serve.queue", "queue", "0 waiting", "Every request gets a slot straight away.", 0.0)
    });

    if verdict == AdmitVerdict::EngineFull {
        v.push(Reading::live("serve.slots", "slots", format!("{:.0}/{}", sv.running, sv.slots),
            "Every slot is busy, so the next request waits until an answer finishes.", 0.70));
    }

    let lat = sv.latency.as_ref();
    // queue wait - corroborated: only scores while something is actually waiting
    let waiting = sv.queue > 0.0 || s.lanes.trusted.waiters > 0 || s.lanes.public.waiters > 0;
    match lat.and_then(|l| l.queue_time.as_ref()) {
        Some(q) => v.push(Reading::live("serve.queue_wait", "queue wait", format!("p50 {:.0} ms", q.p50_ms),
            "Half of recent requests sat in line at least this long before the model started on them.",
            corroborate(ramp(q.p50_ms, 200.0, 5000.0), waiting))),
        None => v.push(Reading::no_data("serve.queue_wait", "queue wait", "No queue-time histogram in the recent window.")),
    }

    // prefix-cache hit rate, inverted
    if sv.is_na("cache_hit") {
        v.push(Reading::no_data("serve.cache_hit", "cache hit", format!("{} does not report a prefix-cache hit rate.", sv.engine_label())));
    } else if sv.prompt_tokens_total <= 0.0 {
        // a fresh engine reports 0 % before it has read anything: missing, not a 0 % hit rate
        v.push(Reading::no_data("serve.cache_hit", "cache hit", "No prompt has been read yet, so there is no hit rate to judge."));
    } else {
        // RATES, NOT COUNTERS (card #196, closing the open finding from #179's review). The
        // number scored here is `cached_share_10m` - the share of the last ten minutes' PROMPT
        // TOKENS that came from the cache, a token-weighted rate built from counter deltas - and
        // NOT the engine's `cache_hit_rate`, which is a per-prefill-report snapshot: one cold
        // batch between two warm ones puts it at 0% with nothing wrong.
        //
        // Replayed over 24h of this box's own stored samples (17,135 polls with a usable window):
        //   snapshot, uncorroborated ...................... 16.08% of polls could headline
        //   snapshot, corroborated by TTFT > 5 s (shipped) .. 2.54%
        //   TTFT > 5 s at all, ANY hit rate ................ 15.93%   <- the corroborator's base
        //   windowed share ................................. 0.00%   (min 59.4, median 99.3)
        // The shipped corroborator confirmed 435 of 2,756 snapshot hits = 15.8%, against a base
        // rate of 15.93%: on this box it agreed at CHANCE, so it was not a second source at all.
        // The windowed share needs no rescue - a ten-minute token-weighted share only falls when
        // the misses are real. The corroboration stays on top because it still costs nothing and
        // it is what catches the genuine case; the snapshot stays visible as an informational row
        // so a reader can still see what the engine itself last reported.
        let windowed = sv.cached_share_10m.map(|v| v * 100.0);
        let p = windowed.unwrap_or(sv.cache_hit_rate * 100.0);
        let d = if p >= 55.0 {
            "Most of each prompt is served from cache, so long conversations start quickly."
        } else {
            "Prompts that miss the cache are read from scratch, so long conversations get slower to start with every turn."
        };
        // corroborated (#179): confirmed only when a second source agrees requests are ACTUALLY
        // slower right now - TTFT past the box's own probe-validity threshold, or prefill running
        // well under its own typical throughput - the same pattern queue_wait already uses above.
        let ttft_bad = sv.ttft_avg_ms_10m.is_some_and(|t| t > s.thresholds.c1_max_ttft_s * 1000.0);
        let prefill_slow = matches!((sv.prefill_tok_s, sv.prefill_tok_s_typical), (Some(now), Some(typ)) if typ > 0.0 && now < typ * 0.5);
        let value = match windowed {
            Some(_) => format!("{} of the last 10 min's prompt tokens", pct0(p)),
            None => pct0(p),
        };
        v.push(Reading::live("serve.cache_hit", "cache hit", value, d, corroborate(ramp(p, 55.0, 5.0), ttft_bad || prefill_slow)));
        // what the engine itself last reported, kept visible and never judged: it is a snapshot
        // of one prefill report, which is a fact about the engine rather than about the box
        if windowed.is_some() {
            v.push(Reading::live("serve.cache_hit_now", "cache hit (engine)", pct0(sv.cache_hit_rate * 100.0),
                "What the engine reported for its last prompt read, which swings with a single cold batch.", 0.0).informational());
        }
    }

    // single-request speed against this box's own baseline (the C1 probe), inverted
    match (s.probe.baseline_tok_s, s.probe.last_ok.as_ref()) {
        (None, _) => v.push(Reading::no_data("serve.speed", "speed", "No baseline yet: a single request's normal speed on this box is still being learned.")),
        (Some(_), None) => v.push(Reading::no_data("serve.speed", "speed", "The speed probe has not completed a reading yet.")),
        (Some(base), Some(p)) => {
            let age = now - p.ts;
            match p.decode_tok_s {
                Some(t) if age <= s.thresholds.c1_stale_secs.max(1) && base > 0.0 => v.push(Reading::live("serve.speed", "speed",
                    format!("{t:.0} tok/s ({:.0}% of normal)", t / base * 100.0),
                    "One answer is being written at this speed; below normal, every reply takes longer to finish.",
                    ramp(t, base, base * 0.45))),
                _ => v.push(Reading::no_data("serve.speed", "speed", format!("The server has been too busy to time a single request for {}.", dur(age)))),
            }
        }
    }

    // time to first token, p90 - scored against the probe's own TTFT limit
    match lat.and_then(|l| l.ttft.as_ref()) {
        Some(t) => {
            let lim = s.thresholds.c1_max_ttft_s * 1000.0;
            let score = if lim > 0.0 { ramp(t.p90_ms, lim, lim * 4.0) } else { 0.0 };
            v.push(Reading::live("serve.ttft", "first word", format!("p50 {:.0} / p90 {:.0} ms", t.p50_ms, t.p90_ms),
                "One request in ten waited at least the p90 for its first word.", score));
        }
        None => v.push(Reading::no_data("serve.ttft", "first word", "No time-to-first-token histogram in the recent window.")),
    }

    // facts that show but are not judged here: no calibrated threshold exists for them
    v.push(Reading::live("serve.decode", "writing", format!("{:.0} tok/s", sv.decode_tok_s),
        "Total writing speed across every running request.", 0.0).informational());
    match sv.prefill_tok_s {
        Some(p) => v.push(Reading::live("serve.prefill", "reading", format!("{} tok/s", tok(p)),
            "How fast prompts are read right now; a long prompt waits roughly its length divided by this for its first word.", 0.0).informational()),
        None => v.push(Reading::no_data("serve.prefill", "reading", format!("No prompt-reading rate: {}.", sv.no_reading_reason()))),
    }
    if let Some(i) = lat.and_then(|l| l.itl.as_ref()) {
        v.push(Reading::live("serve.itl", "between words", format!("p50 {:.1} ms", i.p50_ms),
            "The gap between words once an answer is streaming.", 0.0).informational());
    }
    if sv.spec_accept_length > 0.0 {
        v.push(Reading::live("serve.accept", "accept", format!("{:.2}", sv.spec_accept_length),
            "Tokens the draft model gets accepted per step; more means more of the writing comes free.", 0.0).informational());
    }
    v.push(Reading::live("serve.running", "running", format!("{:.0}/{}", sv.running, sv.slots),
        "Requests being answered at once, out of the engine's slots.", 0.0).informational());
    v
}

pub fn gpus(s: &Status) -> Vec<Reading> {
    let mut v = Vec::new();
    let group = s.gpus.len() > 1;
    for g in &s.gpus {
        v.extend(gpu(g, group, s.thresholds.thermal_temp_c));
    }
    // TP-group utilisation SKEW - ours alone: the cards split every request, so the slowest sets
    // the pace.
    //
    // TWO CORRECTIONS (card #196), each measured on a 24h read-only replay of this box's own
    // stored samples - 17,262 polls, four cards:
    //
    // * MEDIANS, NOT EXTREMES. `max - min` is two outliers subtracted from each other, so one
    //   card sampled mid-step makes the whole group look skewed. The laggard's distance from the
    //   PACK (`median - min`) is what "the group runs at the pace of the slowest" actually says,
    //   and it is the thing pulse reaches for medians to protect.
    // * THE CONFIRMATION MUST COME FROM A DIFFERENT SOURCE THAN THE READING. A raw skew ramp
    //   fires HARDEST WHEN THE BOX IS IDLE - p90 spread 92 idle against 54 busy - because four
    //   cards drifting between 0% and 100% with nothing to do spread further than four cards
    //   sharing a batch. An idle box with uneven utilisation is not a fault. NVML says the cards
    //   are uneven; only the ENGINE can say whether there was any work for them to be uneven
    //   about, so that is the corroborator. (The previous guard, `max util >= 50`, was the same
    //   util vector asking itself and removed almost nothing.) The pack being loaded is the
    //   reading's own premise, not a second source: "held back" is meaningless when the pack is
    //   not running.
    //
    //   Share of polls this reading could headline, same replay:
    //     raw spread, uncorroborated ................. 32.81%
    //     raw spread, corroborated on max util ....... 30.70%   (what this replaces)
    //     median skew, engine running ................  9.10%
    //     median skew, engine running + loaded pack ..  8.09%   (this)
    //   and the idle case is now structurally zero rather than merely rarer.
    //
    // * CARD #197: THE INPUT IS WINDOWED, AND IT HAS TO BE THE COLLECTOR'S WINDOW. #196 left
    //   this reading firing on a single NVML sample: 80% of its remaining hits lasted ONE poll
    //   (1,072 runs, median length 1, p90 2). Dumping those runs sample by sample says what they
    //   are - a DIFFERENT card dips on each poll ([76,97,97,43] then [95,58,95,98] then
    //   [37,95,77,32]) while the pack sits at 95+. That is the 5 s sampler catching whichever
    //   card is between kernels, not a straggler, and no threshold on an instantaneous vector
    //   can tell the two apart.
    //
    //   So the reading is scored on `util_pct_med` - each card's own median over the last
    //   `UTIL_MEDIAN_POLLS` polls - and a card only looks behind here if it was behind for most
    //   of that window. Replayed read-only over 17,445 polls / 24.25 h of this box's stored
    //   samples, the share of polls this reading can headline falls 7.94% -> 3.16%, and the
    //   jitter runs above collapse (they are a different card each poll, so no card's median
    //   moves). Measured against the alternative of taking the median of the per-poll SKEW
    //   instead (4.49% on the same replay): that one keeps firing on exactly those runs, because
    //   the skew MAGNITUDE persists while no single card is ever behind - which is the question
    //   this reading asks.
    //
    //   THE WINDOW BELONGS TO THE COLLECTOR, not to a ring in the TUI: this module is pure (no
    //   clocks, no state), `lss status`, the TUI and any script on /status must be given the
    //   same number rather than each smoothing its own, and a viewer's ring starts empty on
    //   every launch and resize while the collector already holds the samples. A collector too
    //   old (or too fresh) to publish the median says so and is not judged - falling back to the
    //   instantaneous vector would be the defect wearing a new field name.
    let meds: Option<Vec<f64>> = s.gpus.iter().map(|g| g.util_pct_med).collect();
    match meds {
        Some(meds) if meds.len() > 1 => {
            let pack = median(&meds);
            let lo = meds.iter().cloned().fold(f64::MAX, f64::min);
            let behind = (pack - lo).max(0.0);
            let engine_working = s.serve.up && s.serve.running > 0.0;
            let window = if s.collector.poll_secs > 0 { dur(s.collector.poll_secs as i64 * crate::history::UTIL_MEDIAN_POLLS as i64) } else { format!("{} polls", crate::history::UTIL_MEDIAN_POLLS) };
            v.push(Reading::live("gpu.skew", "GPU skew", format!("{behind:.0} pts behind the pack over {window}"),
                "The cards split every request between them, so the one lagging the rest holds them up and the group runs at the pace of the slowest.",
                corroborate(ramp(behind, 10.0, 40.0), engine_working && pack >= 50.0)));
        }
        Some(_) => {}
        None if s.gpus.len() > 1 => v.push(Reading::no_data("gpu.skew", "GPU skew",
            "No windowed utilisation yet: one card lagging the others is only worth saying when it has lagged for a while, and this collector has not published enough polls to tell (an older collector never will - update it).")),
        None => {}
    }
    v
}

/// The middle value, averaging the two middles of an even-length list. Medians, not means: one
/// card sampled mid-step must not move the number every other card is judged against.
fn median(xs: &[f64]) -> f64 {
    let mut v: Vec<f64> = xs.to_vec();
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// One card's readings (temp, throttle, power, util, memory) - what a per-GPU box projects.
pub fn gpu_card(s: &Status, g: &GpuStatus) -> Vec<Reading> {
    gpu(g, s.gpus.len() > 1, s.thresholds.thermal_temp_c)
}

/// The worst band among a group's JUDGED readings (informational rows never colour anything).
pub fn worst_band(rs: &[Reading]) -> Band {
    rs.iter().filter(|r| !r.informational).map(Reading::band).max().unwrap_or(Band::Ok)
}

fn gpu(g: &GpuStatus, group: bool, alert_c: f64) -> Vec<Reading> {
    let i = g.sample.index;
    let k = |f: &str| format!("gpu.{i}.{f}");
    let label = |f: &str| format!("GPU{i} {f}");
    let mut v = Vec::new();

    match g.sample.temp_c {
        Some(t) => {
            let d = if group {
                "A card this hot slows its clocks to protect itself, and the whole group waits for it."
            } else {
                "A card this hot slows its clocks to protect itself, and every answer slows with it."
            };
            // the owner's thermal alert line scores high-to-critical; watching starts 15 C under it
            let score = if alert_c > 0.0 { ramp(t, alert_c - 15.0, alert_c + 5.0) } else { ramp(t, 78.0, 92.0) };
            let r = Reading::live(k("temp"), label("temp"), format!("{t:.0}\u{b0}C"), d, score);
            v.push(if g.thermal_excluded {
                Reading { detail: "Thermal alerts are off for this card by configuration, so its heat is shown but never headlined.".into(), ..r }.informational()
            } else {
                r
            });
        }
        None => v.push(Reading::no_data(k("temp"), label("temp"), "The driver reports no temperature for this card.")),
    }

    // throttle - corroborated: scores only while the driver reports a reason NOW, and then by how
    // far the clock sits below its maximum (we have no throttle-seconds counter to rate).
    // Card #179 (lss-verifier-4, measured on a 24h replay of this box's own stored samples):
    // `sw_power_cap` is NOT a constraint on this hardware - it fires because these cards run at
    // a deliberately CHOSEN power limit (the owner has settled this: raising the cap made decode
    // worse), exactly the "facts about the setup" case pulse's own `informational` rule names.
    // Counting it as a scoring reason made a healthy, correctly-capped card headline page 1 on
    // 19.7% of all polls. Only a reason the driver reports as an ACTIVE constraint right now -
    // thermal or a hardware slowdown/power-brake event - scores; a chosen-profile reason is
    // still named in the reading's own text (a reader can see the box is capped) but never
    // headlines.
    const THROTTLE_SCORES: &[&str] = &["hw_thermal", "sw_thermal", "hw_slowdown", "hw_power_brake"];
    let max = g.health.as_ref().and_then(|h| h.clock_max_mhz);
    let all_reasons: Vec<&str> = g.throttle.iter().map(String::as_str).filter(|t| !t.is_empty() && *t != "clean").collect();
    let scoring_reasons: Vec<&str> = all_reasons.iter().copied().filter(|r| THROTTLE_SCORES.contains(r)).collect();
    if all_reasons.is_empty() {
        v.push(Reading::live(k("throttle"), label("throttle"), "none", "The driver reports no reason to hold the clocks back.", 0.0));
    } else if scoring_reasons.is_empty() {
        v.push(Reading::live(k("throttle"), label("throttle"), all_reasons.join(", "),
            "The driver reports this card is held to a configured power limit - a chosen setting, not a fault.", 0.0)
            .informational());
    } else {
        let reasons = scoring_reasons.join(", ");
        match (g.sample.clock_mhz, max) {
            (Some(c), Some(m)) if m > 0.0 => {
                let deficit = (1.0 - c / m) * 100.0;
                v.push(Reading::live(k("throttle"), label("throttle"), format!("{reasons} (-{:.0}%)", deficit.max(0.0)),
                    "The driver is holding this card's clock down, so it does less work per second than it can.",
                    corroborate(ramp(deficit, 5.0, 45.0), true)));
            }
            _ => v.push(Reading::live(k("throttle"), label("throttle"), reasons,
                "The driver is holding this card's clock down; how far is unknown until its maximum clock has been read.", 0.0)),
        }
    }

    if let (Some(p), Some(l)) = (g.sample.power_w, g.sample.power_limit_w) {
        v.push(Reading::live(k("power"), label("power"), format!("{p:.0}/{l:.0} W"), "Power drawn against this card's limit.", 0.0).informational());
    }
    if let Some(u) = g.sample.util_pct {
        v.push(Reading::live(k("util"), label("util"), pct0(u), "Share of the last interval this card was computing.", 0.0).informational());
    }
    if let (Some(u), Some(t)) = (g.sample.mem_used_mib, g.sample.mem_total_mib) {
        v.push(Reading::live(k("mem"), label("memory"), format!("{:.1}/{:.1} GiB", u / 1024.0, t / 1024.0),
            "Memory the engine reserved up front for weights and cache; full is normal for a serving card.", 0.0).informational());
    }
    v
}

/// No rows at all when there is no gateway (a stranger's box): a missing component is not a fault.
///
/// CARD #196: PER LANE, not summed. The LANES box on page 1 has always shown a row per lane, so
/// summing the two here left the renderer to decide, on its own thresholds, which lane's number
/// to colour - the second scale this card exists to delete. It also loses the answer the owner
/// actually wants: "outside callers are being refused" and "my own agents are being refused" are
/// different problems with different fixes, and the headline can now name which. The trusted
/// lane's budget was the only one judged at all; the public lane's is now judged too.
pub fn gateway(s: &Status) -> Vec<Reading> {
    let gt = &s.gate;
    if gt.absent {
        return vec![];
    }
    if !gt.up {
        return vec![Reading::offline("gate.up", "gateway", "The gateway is not answering: every client that goes through it is refused until it is back.")];
    }
    let mut v = Vec::new();
    // the hold is judged once, across both lanes: "is the wait ours or the engine's" is a
    // question about the gateway as a whole, and the verdict that corroborates it is too
    let waiters = s.lanes.trusted.waiters + s.lanes.public.waiters;
    v.push(if waiters == 0 {
        Reading::live("gate.waiters", "held", "0 waiting", "Nothing is held at the gateway.", 0.0)
    } else if s.admission_verdict() == AdmitVerdict::GatewayLimited {
        // corroborated: the engine has a free slot, so the hold is the gateway's own budget
        Reading::live("gate.waiters", "held", format!("{waiters} waiting"),
            "Requests are held at the gateway while the engine has free slots: the wait is ours - the token budget, not the GPUs.",
            MEDIUM + 0.3 * ramp(waiters as f64, 0.0, 16.0))
    } else {
        Reading::live("gate.waiters", "held", format!("{waiters} waiting"),
            "Requests wait at the gateway because the engine is busy; they go in as answers finish.", 0.0)
    });
    for (lane, who, l) in [("public", "Outside callers", &s.lanes.public), ("trusted", "Your own agents and tools", &s.lanes.trusted)] {
        let k = |f: &str| format!("gate.{lane}.{f}");
        match l.budget_tokens {
            Some(b) if b > 0 => {
                let p = l.inflight_tokens as f64 / b as f64 * 100.0;
                v.push(Reading::live(k("budget"), format!("{lane} budget"), format!("{} of {} ({})", tok(l.inflight_tokens as f64), tok(b as f64), pct0(p)),
                    format!("When the {lane} lane's token budget is full, its requests wait at the gateway even if the engine has room."),
                    ramp(p, 70.0, 100.0)));
            }
            _ => v.push(Reading::no_data(k("budget"), format!("{lane} budget"), format!("This gateway publishes no in-flight token budget for the {lane} lane."))),
        }
        let failed = l.codes_10m.c5xx;
        let turned = l.codes_10m.c4xx;
        v.push(Reading::live(k("failed"), format!("{lane} failed"), format!("{failed} in 10 min"),
            if failed > 0 { format!("{who} got an error instead of an answer.") } else { format!("No {lane} request has failed in the last 10 minutes.") },
            if failed > 0 { 0.50 + 0.4 * ramp(failed as f64, 0.0, 20.0) } else { 0.0 }));
        v.push(Reading::live(k("turned"), format!("{lane} turned away"), format!("{turned} in 10 min"),
            if turned > 0 { format!("{who} were refused at the gateway and have to retry.") } else { format!("No {lane} caller was turned away in the last 10 minutes.") },
            if turned > 0 { 0.40 + 0.2 * ramp(turned as f64, 0.0, 50.0) } else { 0.0 }));
    }
    v
}

/// The readings for one lane, worst first - what the LANES box projects into a lane's row.
pub fn gate_lane(s: &Status, lane: &str) -> Vec<Reading> {
    let p = format!("gate.{lane}.");
    gateway(s).into_iter().filter(|r| r.key.starts_with(&p)).collect()
}

/// Cost is a fact about the setup: it shows and sorts, never leads. Every figure names the rate's
/// effective date and what it leaves out.
pub fn cost(s: &Status) -> Vec<Reading> {
    let Some(c) = s.cost.as_ref() else {
        return vec![Reading::no_data("cost", "cost", "No rates file is configured, so cost tracking is off.").informational()];
    };
    // card #298: name the rate's source when rates.toml states one ("rate: CA avg (EIA 2026-06)")
    let from = if c.rate_source.is_empty() { String::new() } else { format!("rate: {}; ", c.rate_source) };
    let basis = format!("{from}rates effective {}; delivery and generation only, excludes the fixed daily charge", c.effective_date);
    let mut v = Vec::new();
    v.push(match (c.live_usd_per_hour, c.current_usd_per_kwh) {
        (Some(h), Some(k)) => Reading::live("cost.live", "cost now", format!("${h:.2}/hour"),
            format!("What the GPUs draw right now costs per hour at ${k:.3}/kWh{} ({basis}).",
                if c.current_period.is_empty() { String::new() } else { format!(", the {} rate", c.current_period) }), 0.0),
        _ => Reading::no_data("cost.live", "cost now", "No live power reading or no rate for this hour."),
    }.informational());
    v.push(match c.today_usd {
        Some(u) => Reading::live("cost.today", "today", format!("${u:.2}"), format!("Spent on GPU power since midnight ({basis})."), 0.0),
        None => Reading::no_data("cost.today", "today", "No priced power samples since midnight yet."),
    }.informational());
    v.push(match c.today_usd_per_million_real_work_tokens {
        Some(u) => Reading::live("cost.per_1m", "per 1M tokens", format!("${u:.3}"),
            format!("What a million tokens of real work - prompt actually read plus text written - cost today ({basis})."), 0.0),
        None => Reading::no_data("cost.per_1m", "per 1M tokens", "Not enough real work today to price a million tokens."),
    }.informational());
    // card #181: the WEEK and the MONTH, which card #176 computed but left where only `lss
    // status` and page 5 could show them. They are money facts, so they are informational and
    // can never headline - but a reader looking at page 1 for "what is this costing me" is
    // asking a question no per-hour or per-today figure answers.
    if let Some(sp) = s.spending.as_ref() {
        v.push(spend_window("cost.week", "this week", &sp.this_week, "since Monday", &basis).informational());
        // card #226: "month to date" - it always WAS since the 1st; the label now says so
        v.push(spend_window("cost.month", "month to date", &sp.this_month, "since the 1st", &basis).informational());
        // card #226 (the owner: "for electric u need to add month a year to date"): since Jan 1.
        // The stored history rarely reaches that far back (the 10-minute rollup keeps ~90 days),
        // so when the first priced day is after Jan 1 the reading NAMES it - the partial year is
        // stated, not padded - and spend_window's floor caveat carries the covered/nominal gap.
        let jan1 = format!("{}-01-01", crate::timeutil::fmt_local(s.generated_at, "%Y"));
        let ytd_span = match sp.year_first_day.as_deref() {
            Some(first) if first > jan1.as_str() => format!("since Jan 1 (the stored history starts {first})"),
            _ => "since Jan 1".to_string(),
        };
        v.push(spend_window("cost.ytd", "year to date", &sp.this_year, &ytd_span, &basis).informational());
        // The projection exists ONLY when card #176 decided the window supports one, and its
        // absence carries #176's own reason rather than a figure nobody should trust.
        //
        // #199: the label is "month projected", not the old "month at this rate" - 18 characters
        // did not fit page 1's one label column (16), so the figure it qualifies sat flush
        // against it on screen. The renderer now holds that grid whatever a label does
        // (`dash::fit_label`), but a label that has to be CUT to fit is a worse row than one
        // written to fit, and every other label on the page is written to fit. It still names
        // the same thing the key and the refusal line do ("no month projection: ...").
        v.push(match sp.month_projection_usd {
            Some(p) => Reading::live("cost.month_projection", "month projected", format!("~${p:.2}"),
                format!("Where this month lands if the days so far are typical - {} ({basis}).", sp.projection_note), 0.0),
            None => Reading::no_data("cost.month_projection", "month projected", sp.projection_note.clone()),
        }.informational());
    }
    v
}

/// One spending window as a reading. A window that priced little of its own span says so instead
/// of showing a small number: a month that is three days old reporting "$6" is not wrong so much
/// as unreadable, and card #176's whole rule is that partial coverage is stated, never implied.
fn spend_window(key: &str, label: &str, w: &crate::rates::SpendWindow, span: &str, basis: &str) -> Reading {
    let Some(usd) = w.usd else {
        return Reading::no_data(key, label, format!("Nothing priced yet {span}."));
    };
    let kwh = w.kwh.map_or_else(String::new, |k| format!(" \u{b7} {k:.2} kWh"));
    // The SAME trigger the page-1 box uses for every other caveat in it (`coverage_note`, 0.95),
    // deliberately NOT #176's 0.8 projection gate. Found by mutation: with two thresholds, a
    // window covered 85% of its span carried a caveat on page 1 and none in /status - the same
    // fact, two answers, which is exactly the second-scale problem #179 exists to remove. 0.8
    // still gates the PROJECTION, which is a different question (is this worth extrapolating).
    let covered = if w.nominal_secs > 0 && (w.covered_secs as f64) >= w.nominal_secs as f64 * 0.95 {
        String::new()
    } else {
        format!(" Only {} of {} {} carried priced power, so this is a floor, not the total.",
            crate::rates::fmt_days_pub(w.covered_secs), crate::rates::fmt_days_pub(w.nominal_secs), span)
    };
    Reading::live(key, label, format!("${usd:.2}{kwh}"),
        format!("Spent on GPU power {span} ({basis}).{covered}"), 0.0)
}

pub fn loadout(s: &Status) -> Vec<Reading> {
    let mut v = Vec::new();
    let model = s.serve.model.clone().or_else(|| s.loadout.as_ref().map(|l| l.model.clone())).filter(|m| !m.is_empty());
    if let Some(m) = model {
        v.push(Reading::live("loadout.model", "model", m, "The model the engine has loaded.", 0.0).informational());
    }
    if let Some(l) = s.loadout.as_ref().filter(|l| !l.flags.is_empty()) {
        v.push(Reading::live("loadout.flags", "flags", l.flags.clone(), "The launch flags this loadout runs with.", 0.0).informational());
    }
    v
}

/// What an alert's own severity word is worth on the one scale. THE only place that mapping
/// lives (card #196): page 1's ALERTS box painted every live alert by comparing the same word to
/// "critical" itself, so an `info` alert that `events()` had deliberately scored below LOW still
/// came out amber in the box beside the verdict line that ignored it.
///
/// CARD #198: every word `rules::Severity` can actually produce is here now. `page` and
/// `hardware` fell through to the `_` arm and scored `LOW/2`, so the two MOST urgent words the
/// rule engine has ("wake someone", "a GPU fault") were the only ones page 1 painted with no ink
/// at all, while a `warn` beside them went amber. `page` is what `serve_down_page` fires - the
/// engine has been down long enough to wake a human - so it sits with `critical`; `hardware`
/// (an Xid, a GPU that vanished) is amber, because the live state of the cards is what the GPU
/// readings say and red is reserved for `Critical` and `Offline`.
pub fn alert_score(severity: &str) -> f64 {
    match severity {
        "crit" | "critical" | "page" => HIGH,
        "warn" | "warning" | "hardware" => MEDIUM,
        _ => LOW / 2.0,
    }
}

/// How recently the driver must have reported an Xid for that dated record to be coloured as
/// something happening NOW (card #198).
///
/// MEASURED, read-only, on this box's own stored samples (17,333 polls over the 24h the sample
/// table retains, against the 7 Xid incidents on record). Share of polls an "an Xid is on the
/// books" rule would have coloured:
///   within 7 days ... 100.00%   <- what the hand-written amber effectively did
///   within 48 h ..... 100.00%
///   within 24 h ......  98.04%
///   within 6 h .......  23.29%
///   within 1 h ........  2.57%  <- this
/// Ink that is on for 100% of the day says nothing; the reading has to mean "a card threw a
/// hardware fault JUST NOW", and an hour is the longest window on this box that still does.
/// Anything older is a dated record, and `unwell` (the #196 rule) is what brings it back.
pub const XID_FRESH_SECS: i64 = 3_600;

/// Alerts firing NOW, scored by their own severity; informational-level alerts show below the
/// headline line (an "info" alert is by definition not the worst thing on the page). Plus the two
/// event FACTS page 1 also shows: an alert the sink could not hand over, and a driver-reported
/// Xid (card #198) - `now` is the clock those two are aged against.
pub fn events(s: &Status, now: i64) -> Vec<Reading> {
    let mut v = Vec::new();
    for rule in &s.firing {
        let last = s.alerts.iter().filter(|a| &a.rule == rule && !a.recovered).max_by_key(|a| a.ts);
        let (sev, msg) = last.map_or(("warn", format!("Alert {rule} is firing.")), |a| (a.severity.as_str(), a.message.clone()));
        let mut msg = msg;
        if !msg.ends_with('.') {
            msg.push('.');
        }
        v.push(Reading::live(format!("alert.{rule}"), "alert", rule.clone(), msg, alert_score(sev)));
    }
    let open: Vec<&crate::incidents::Incident> = s.incidents.iter().filter(|i| i.end.is_none()).collect();
    v.push(Reading::live("events.incidents", "incidents", format!("{} (7 days), {} open", s.incidents.len(), open.len()),
        "Outages and restarts recorded in the last week.", 0.0).informational());

    // AN OPEN INCIDENT, CORROBORATED (card #196). Page 1 painted any open non-maintenance
    // incident RED on its own, which is a claim the page cannot back: an incident is a DATED
    // RECORD that has not been seen to close, and whether the condition is still live is what
    // serve.up / gate.up / a firing alert already say. An incident nobody closed on a server that
    // has been healthy since would have shown red beside a verdict line saying OK - two surfaces
    // contradicting each other about the same box, which is the thing this card removes.
    //
    // So it scores only when a second source agrees the box is UNWELL right now - the same rule
    // the queue wait, the cache hit and the GPU skew already use. Uncorroborated it is shown and
    // ranked past, never coloured as a fault. There is no measurement behind a bolder rule: this
    // box's own history holds 36 incidents and NOT ONE has ever been left open, so any score I
    // invented for the open case would be a guess dressed as a threshold.
    let unwell = !s.serve.up || (!s.gate.absent && !s.gate.up) || s.firing.iter().any(|r| {
        s.alerts.iter().filter(|a| &a.rule == r && !a.recovered).max_by_key(|a| a.ts).is_none_or(|a| alert_score(&a.severity) >= LOW)
    });
    v.extend(undelivered(s));
    v.extend(xids(s, now, unwell));
    let live: Vec<&&crate::incidents::Incident> = open.iter().filter(|i| i.kind != crate::incidents::KIND_MAINTENANCE).collect();
    if let Some(i) = live.first() {
        let r = Reading::live("events.open", "open incident", i.kind.clone(),
            format!("An incident opened {} has not been seen to close.", i.kind),
            corroborate(MEDIUM, unwell));
        v.push(if unwell { r } else { Reading { detail: format!("An incident opened {} has not been seen to close, and nothing else says the box is unwell right now.", i.kind), ..r }.informational() });
    } else if let Some(i) = open.iter().find(|i| i.kind == crate::incidents::KIND_MAINTENANCE) {
        v.push(Reading::live("events.open", "open incident", i.kind.clone(),
            "Planned maintenance is open, which is a note to yourself rather than something wrong.", 0.0).informational());
    }
    v
}

/// AN ALERT THE SINK COULD NOT HAND OVER, one reading per alert row so the marker beside a row
/// is that row's OWN judgement (card #198). Page 1 painted `[not delivered]` amber by hand, which
/// is the second scale #196 exists to delete - and on this box it was amber for the wrong reason.
///
/// MEASURED on this box's own alert table (104 alerts over four days): 91 were handed over and
/// 13 were not, and ALL 13 of the undelivered ones are `info` - every `warn`, `hardware` and
/// recovered alert got out. So the hand-written amber was, in practice, a second way of painting
/// an `info` alert amber: exactly the contradiction #196 removed from the severity word, arriving
/// through the delivery flag instead.
///
/// Two things therefore gate the ink, and each is a source other than the flag itself:
/// * THE ALERT'S OWN SEVERITY. Not being told about something the page scores below Watch is not
///   a fault; not being told about a `warn` or worse is. The cap at `MEDIUM` keeps it amber: the
///   alert's own row already carries whatever red its severity earns, and this reading is about
///   the delivery path, not about the box.
/// * DELIVERY HAVING DEMONSTRABLY WORKED HERE. `delivered` starts false and only ever turns true,
///   so on a box with no `alert_cmd` configured EVERY alert is undelivered for ever - a permanent
///   amber that reports the absence of a feature as a fault. If nothing in the list was ever
///   handed over, there is no sink to have failed, and the row says that instead.
fn undelivered(s: &Status) -> Vec<Reading> {
    let works = s.alerts.iter().any(|a| a.delivered);
    s.alerts
        .iter()
        .filter(|a| !a.delivered)
        .map(|a| {
            let own = alert_score(&a.severity);
            let r = Reading::live(format!("events.undelivered.{}", a.id), "alert not delivered", a.rule.clone(),
                format!("The {} alert \"{}\" was never handed over to the alert sink: nobody was told, and the next one may go the same way.", a.severity, a.message.trim_end_matches('.')),
                corroborate(own.min(MEDIUM), works && own >= LOW));
            if !works {
                return Reading { detail: format!("Not handed over - and nothing on this box ever has been, so there is probably no alert sink configured rather than one that failed ({}).", a.rule), ..r }.informational();
            }
            if own < LOW {
                return Reading { detail: format!("Not handed over, and this page scores a {} alert below watch anyway: an alert nobody needed to be woken for is not a delivery fault ({}).", a.severity, a.rule), ..r }.informational();
            }
            r
        })
        .collect()
}

/// A DRIVER-REPORTED Xid, one reading per incident (card #198). The INCIDENTS box painted the
/// word `xid` amber whenever it appeared, which is ink on a KIND rather than on a severity: an
/// Xid incident is a dated record inside a SEVEN-DAY list, so on this box (7 Xids on record) that
/// amber was on for 100% of the last day's polls - see `XID_FRESH_SECS` for the replay. Ink that
/// is always on carries no information.
///
/// It is coloured when either source says it is about NOW: the fault is fresh (within
/// `XID_FRESH_SECS`), or something else says the box is unwell right now and this is the newest
/// hardware fault on record - #196's rule for the open incident, applied to the one record that
/// could explain what is wrong. Otherwise it shows and ranks as what it is: history.
///
/// Amber, never red: `MEDIUM` is the same score the firing `hardware` alert gets, and the live
/// state of the cards belongs to the GPU readings.
/// The kernel's own words for one Xid, trimmed of the trailing punctuation the incident detail
/// carries, so the reading's sentence stays one sentence.
fn fault(detail: &str) -> &str {
    detail.trim().trim_end_matches(['.', ',', ' '])
}

fn xids(s: &Status, now: i64, unwell: bool) -> Vec<Reading> {
    let newest = s.incidents.iter().filter(|i| i.kind == crate::incidents::KIND_XID).map(|i| i.start).max();
    s.incidents
        .iter()
        .filter(|i| i.kind == crate::incidents::KIND_XID)
        .map(|i| {
            let age = (now - i.start).max(0);
            let fresh = age <= XID_FRESH_SECS;
            let explains = unwell && Some(i.start) == newest;
            let r = Reading::live(format!("events.xid.{}", i.id), "GPU driver error", i.kind.clone(),
                if fresh {
                    format!("The driver reported a GPU fault {} ago ({}): a card that has just thrown an Xid can hang or drop off the bus again.", dur(age), fault(&i.detail))
                } else {
                    format!("The driver reported a GPU fault {} ago ({}), and something is wrong on this box right now - this is the newest hardware fault that could explain it.", dur(age), fault(&i.detail))
                },
                corroborate(MEDIUM, fresh || explains));
            if fresh || explains {
                r
            } else {
                Reading { detail: format!("A GPU fault the driver reported {} ago ({}), with nothing saying the box is unwell since: a dated record, not something happening now.", dur(age), fault(&i.detail)), ..r }.informational()
            }
        })
        .collect()
}

#[cfg(test)]
mod domain_tests {
    use super::*;
    use crate::gpu::{GpuHealth, GpuSample};
    use crate::hist::{HistSummary, LatencyNow};
    use crate::model::{AlertRow, CollectorInfo, CostStatus, GpuStatus, LoadoutBrief, Status};

    const NOW: i64 = 1_800_000_000;

    fn gpu(i: u32, temp: f64, util: f64) -> GpuStatus {
        GpuStatus {
            sample: GpuSample { index: i, temp_c: Some(temp), util_pct: Some(util), clock_mhz: Some(2250.0), power_w: Some(250.0), power_limit_w: Some(300.0), ..Default::default() },
            // #197: a card that has been at this utilisation all window, which is what the skew
            // reading is actually scored on
            util_pct_med: Some(util),
            health: Some(GpuHealth { index: i, clock_max_mhz: Some(2430.0), ..Default::default() }),
            ..Default::default()
        }
    }

    /// A healthy, busy 4-GPU box with a gateway and cost configured: nothing should headline.
    fn calm() -> Status {
        let mut s = Status { generated_at: NOW, collector: CollectorInfo { poll_secs: 5, last_sample_ts: Some(NOW - 2), ..Default::default() }, ..Default::default() };
        s.serve.up = true;
        s.serve.engine = "sglang".into();
        s.serve.slots = 8;
        s.serve.running = 3.0;
        s.serve.kv_usage = 0.30;
        s.serve.kv_max_tokens = 1_000_000.0;
        s.serve.evicted_tok_per_hour_10m = Some(0.0);
        s.serve.cache_hit_rate = 0.98;
        s.serve.prompt_tokens_total = 5e9;
        s.serve.decode_tok_s = 340.0;
        s.serve.model = Some("some-model".into());
        s.serve.latency = Some(LatencyNow {
            queue_time: Some(HistSummary { p50_ms: 20.0, p90_ms: 50.0, p99_ms: 90.0, ..Default::default() }),
            ttft: Some(HistSummary { p50_ms: 300.0, p90_ms: 900.0, p99_ms: 2000.0, ..Default::default() }),
            ..Default::default()
        });
        s.thresholds.c1_max_ttft_s = 5.0;
        s.thresholds.c1_stale_secs = 600;
        s.probe.baseline_tok_s = Some(180.0);
        s.probe.last_ok = Some(serde_json::from_value(serde_json::json!({"ts": NOW - 60, "status": "ok", "decode_tok_s": 175.0})).unwrap());
        s.gpus = (0..4).map(|i| gpu(i, 70.0, 95.0)).collect();
        s.gate.up = true;
        s.lanes.trusted.budget_tokens = Some(1_000_000);
        s.lanes.trusted.inflight_tokens = 200_000;
        s.cost = Some(CostStatus { effective_date: "2026-01-01".into(), live_usd_per_hour: Some(99.0), current_usd_per_kwh: Some(0.5), today_usd: Some(999.0), ..Default::default() });
        s.loadout = Some(LoadoutBrief { model: "some-model".into(), flags: "--tp 4".into(), ..Default::default() });
        s
    }

    fn lead(s: &Status) -> Option<String> {
        leading(&all(s, NOW), None).map(|r| r.key.clone())
    }

    /// CARD #197: the TP-skew reading is scored on the collector's WINDOWED median, and both
    /// cases below are REAL polls replayed read-only off this box's own sample table.
    ///
    /// The jitter case is the run at 1790085649 - six consecutive 5 s polls in which a DIFFERENT
    /// card dips each time while the pack sits at 95+. The instantaneous vector of its last poll
    /// reads 32 points of skew and scores Critical under the shipped rule; no card's median over
    /// the six is more than 7 points off the pack, so the windowed rule scores it zero. That
    /// shape is 80% of what the reading used to fire on.
    ///
    /// The straggler case is the run at 1790104275 - GPU1 at 65, 0, 47, 47, 61, 69 while the
    /// other three hold 90+. That is a card genuinely behind the pack for half a minute, and it
    /// still scores Critical.
    #[test]
    fn the_skew_reading_scores_a_card_that_stayed_behind_and_not_the_poll_it_was_caught_between_kernels_on() {
        let med = |u: [f64; 4]| -> f64 {
            let mut v = u.to_vec();
            v.sort_by(f64::total_cmp);
            (v[1] + v[2]) / 2.0
        };
        let with = |last: [f64; 4], window: [f64; 4]| {
            let mut s = calm();
            s.collector.poll_secs = 5;
            s.gpus = (0..4)
                .map(|i| {
                    let mut g = gpu(i, 70.0, last[i as usize]);
                    g.util_pct_med = Some(window[i as usize]);
                    g
                })
                .collect();
            s
        };
        // the jitter run's last poll, and each card's own median over the six polls
        let jitter = with([97.0, 62.0, 94.0, 95.0], [85.5, 96.0, 94.5, 90.0]);
        let r = all(&jitter, NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap();
        assert_eq!(r.severity, 0.0, "one poll's dip is not a straggler: {r:?}");
        assert!(r.value.as_deref().unwrap().starts_with("7 pts behind the pack over 30s"), "the row says what window it judged: {r:?}");
        assert_eq!(lead(&jitter), None, "and page 1 says nothing is constrained");
        // ...while the SHIPPED input - that same poll's instantaneous vector - would have scored
        // Critical, which is the defect this card exists to remove
        let inst = med([97.0, 62.0, 94.0, 95.0]) - 62.0;
        assert!(ramp(inst, 10.0, 40.0) >= LOW && Band::of(ramp(inst, 10.0, 40.0)) == Band::High, "the instantaneous vector really did score high enough to headline: {inst} points");

        // a real straggler: GPU1 behind the pack for the whole window
        let real = with([90.0, 59.0, 90.0, 92.0], [92.0, 53.0, 93.0, 91.5]);
        let r = all(&real, NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap();
        assert_eq!(r.band(), Band::Critical, "a card 38 points behind the pack for 30 s holds the whole group up: {r:?}");
        assert_eq!(lead(&real).as_deref(), Some("gpu.skew"));

        // the corroboration #196 measured stays: an IDLE box with the same windowed spread is
        // not a fault, however uneven the cards look
        let mut idle = real.clone();
        idle.serve.running = 0.0;
        assert_eq!(all(&idle, NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap().severity, 0.0);

        // a collector that publishes no window is NOT quietly judged on one sample
        let mut old = real.clone();
        for g in &mut old.gpus {
            g.util_pct_med = None;
        }
        let r = all(&old, NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap();
        assert_eq!(r.band(), Band::NoData, "{r:?}");
        assert!(r.detail.contains("update it"), "{}", r.detail);
        assert_eq!(lead(&old), None);
    }

    #[test]
    fn a_calm_busy_box_has_no_headline_even_with_big_cost_numbers() {
        let s = calm();
        let rs = all(&s, NOW);
        assert_eq!(leading(&rs, None).map(|r| r.key.clone()), None, "{:#?}", rank(&rs).iter().take(3).collect::<Vec<_>>());
        assert!(rs.iter().filter(|r| r.key.starts_with("cost.")).all(|r| r.informational));
    }

    /// card #226: a year the stored history does not reach back through is stated as partial -
    /// the reading names the first priced day and calls the figure a floor - never a padded or
    /// confident "year to date".
    #[test]
    fn year_to_date_names_where_the_history_starts_and_is_a_floor_when_partial() {
        use crate::rates::{SpendWindow, SpendingStatus};
        let mut s = calm();
        let year = crate::timeutil::fmt_local(s.generated_at, "%Y");
        s.spending = Some(SpendingStatus {
            this_year: SpendWindow { usd: Some(412.50), kwh: Some(1031.0), covered_secs: 40 * 86_400, nominal_secs: 200 * 86_400 },
            year_first_day: Some(format!("{year}-06-01")),
            ..Default::default()
        });
        let rs = all(&s, NOW);
        let ytd = rs.iter().find(|r| r.key == "cost.ytd").expect("cost.ytd");
        assert_eq!(ytd.label, "year to date");
        assert_eq!(ytd.value.as_deref(), Some("$412.50 \u{b7} 1031.00 kWh"));
        assert!(ytd.detail.contains(&format!("the stored history starts {year}-06-01")), "{}", ytd.detail);
        assert!(ytd.detail.contains("floor, not the total"), "40 of 200 days is a floor: {}", ytd.detail);
        // history that DOES reach Jan 1 says plainly "since Jan 1", no apology
        s.spending = Some(SpendingStatus {
            this_year: SpendWindow { usd: Some(9.0), kwh: Some(20.0), covered_secs: 86_400, nominal_secs: 86_400 },
            year_first_day: Some(format!("{year}-01-01")),
            ..Default::default()
        });
        let rs = all(&s, NOW);
        let ytd = rs.iter().find(|r| r.key == "cost.ytd").unwrap();
        assert!(ytd.detail.contains("since Jan 1") && !ytd.detail.contains("history starts") && !ytd.detail.contains("floor"), "{}", ytd.detail);
    }

    /// card #181: the week and the month reach page 1 through THESE rows, so what they may say
    /// is pinned here rather than in the renderer. A window that priced little of its own span
    /// states that; a refused projection carries card #176's reason; and none of them can ever
    /// take the headline, whatever the dollar figure is.
    #[test]
    fn the_week_and_the_month_are_money_facts_that_state_their_coverage_and_never_lead() {
        use crate::rates::{SpendWindow, SpendingStatus};
        let mut s = calm();
        s.spending = Some(SpendingStatus {
            this_week: SpendWindow { usd: Some(48.00), kwh: Some(120.0), covered_secs: 6 * 86_400, nominal_secs: 6 * 86_400 },
            // three days into the month, and only one of them priced: a floor, not a total
            this_month: SpendWindow { usd: Some(6.00), kwh: Some(15.0), covered_secs: 86_400, nominal_secs: 3 * 86_400 },
            month_projection_usd: None,
            projection_note: "too early to project: 3 of 30 days into the month (needs 7)".into(),
            ..Default::default()
        });
        let rs = all(&s, NOW);
        let get = |k: &str| rs.iter().find(|r| r.key == k).unwrap_or_else(|| panic!("{k} is missing from the readings"));

        let week = get("cost.week");
        assert_eq!(week.value.as_deref(), Some("$48.00 \u{b7} 120.00 kWh"));
        assert!(week.detail.contains("since Monday"), "{}", week.detail);
        assert!(!week.detail.contains("floor"), "a fully covered week has nothing to apologise for: {}", week.detail);

        let month = get("cost.month");
        assert!(month.detail.contains("floor, not the total"), "a thinly covered month says so: {}", month.detail);
        assert!(month.detail.contains("1 days") || month.detail.contains("1.0 days"), "and says how much it priced: {}", month.detail);

        // card #226: the relabel, and the year to date reads as a real reading or an honest no-data
        assert_eq!(month.label, "month to date");
        let ytd = get("cost.ytd");
        assert!(ytd.informational, "a money fact never headlines");
        assert!(ytd.value.is_none() && ytd.detail.contains("Nothing priced yet since Jan 1"), "no year data = no figure: {:?}", ytd);

        let proj = get("cost.month_projection");
        assert!(proj.value.is_none(), "a refused projection shows no figure");
        assert!(proj.detail.contains("too early to project"), "it carries #176's own reason: {}", proj.detail);

        // the trigger matches page 1's own caveat renderer, not #176's projection gate: a
        // window covered 85% of its span must state it on BOTH surfaces or the two disagree
        let mut s85 = s.clone();
        if let Some(sp) = s85.spending.as_mut() {
            sp.this_week = SpendWindow { usd: Some(40.0), kwh: Some(100.0), covered_secs: 85, nominal_secs: 100 };
        }
        let rs85 = all(&s85, NOW);
        assert!(
            rs85.iter().find(|r| r.key == "cost.week").unwrap().detail.contains("floor, not the total"),
            "85% covered is not the full window: {}",
            rs85.iter().find(|r| r.key == "cost.week").unwrap().detail
        );

        // the point of the whole architecture: money never headlines, however large
        assert!(rs.iter().filter(|r| r.key.starts_with("cost.")).all(|r| r.informational));
        assert_eq!(leading(&rs, None).map(|r| r.key.clone()), None, "money must not headline a calm box");
        assert_eq!(leading(&rs, Some("cost.week")).map(|r| r.key.clone()), None, "not even as the incumbent");
    }

    /// No spending yet (an old collector, or a fresh database) must not invent rows.
    #[test]
    fn no_spending_means_no_week_or_month_rows_rather_than_zeros() {
        let mut s = calm();
        s.spending = None;
        let rs = all(&s, NOW);
        assert!(!rs.iter().any(|r| r.key.starts_with("cost.week") || r.key.starts_with("cost.month")), "no invented zeros");
        assert!(rs.iter().any(|r| r.key == "cost.today"), "the rest of the cost domain still reports");
    }

    #[test]
    fn a_hot_gpu_alone_names_that_gpu() {
        let mut s = calm();
        s.gpus[2].sample.temp_c = Some(90.0);
        assert_eq!(lead(&s).as_deref(), Some("gpu.2.temp"));
    }

    #[test]
    fn a_hot_gpu_with_thermal_alerts_off_is_shown_but_never_headlined() {
        let mut s = calm();
        s.gpus[0].sample.temp_c = Some(99.0);
        s.gpus[0].thermal_excluded = true;
        assert_eq!(lead(&s), None);
        let r = all(&s, NOW).into_iter().find(|r| r.key == "gpu.0.temp").unwrap();
        assert!(r.informational && r.severity > HIGH, "still scored, still sorted: {r:?}");
    }

    #[test]
    fn a_full_kv_cache_names_kv() {
        let mut s = calm();
        s.serve.kv_usage = 0.95;
        assert_eq!(lead(&s).as_deref(), Some("serve.kv"));
    }

    #[test]
    fn a_dead_collector_outranks_a_saturated_engine() {
        let mut s = calm();
        s.serve.kv_usage = 1.0;
        s.gpus[1].sample.temp_c = Some(99.0);
        s.collector.last_sample_ts = Some(NOW - 120);
        assert_eq!(lead(&s).as_deref(), Some("collector"));
    }

    #[test]
    fn an_engine_that_is_down_leads_and_its_stale_numbers_are_not_scored() {
        let mut s = calm();
        s.serve.up = false;
        s.serve.down_since = Some(NOW - 300);
        let rs = all(&s, NOW);
        assert_eq!(leading(&rs, None).unwrap().key, "serve.up");
        assert!(!rs.iter().any(|r| r.key == "serve.kv"), "a down engine's KV is missing, not zero");
        assert!(rs.iter().find(|r| r.key == "serve.up").unwrap().detail.contains("5m"));
    }

    #[test]
    fn tp_group_skew_scores_only_on_a_busy_group() {
        let mut s = calm();
        set_util(&mut s.gpus[3], 55.0); // 95 vs 55: a lagging group
        assert_eq!(lead(&s).as_deref(), Some("gpu.skew"));
        // an even group has no skew
        let r = all(&calm(), NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap();
        assert_eq!(r.severity, 0.0);
    }

    /// #197: the skew reading is scored on the WINDOWED median, so a case about the skew
    /// arithmetic has to move both numbers - the instantaneous sample the board shows and the
    /// median the reading judges. A case about the WINDOW itself moves them apart on purpose
    /// (`the_skew_reading_scores_a_card_that_stayed_behind_...`).
    fn set_util(g: &mut GpuStatus, u: f64) {
        g.sample.util_pct = Some(u);
        g.util_pct_med = Some(u);
    }

    fn skew(s: &Status) -> Reading {
        all(s, NOW).into_iter().find(|r| r.key == "gpu.skew").unwrap()
    }

    /// Card #196, from a 24h replay of this box's own stored samples: the raw `max - min` ramp
    /// scored on 32.8% of polls and its p90 was 92 while the box was IDLE against 54 while it was
    /// busy - the alarm for "a card is holding the group up" was loudest when there was no group
    /// and nothing to hold up. Both halves of the fix get a case, and each case fails on its own
    /// if the other half is removed.
    #[test]
    fn an_idle_box_with_wildly_uneven_gpus_is_not_a_straggler() {
        // the measured idle shape: three cards parked, one still winding down, engine empty
        let mut idle = calm();
        idle.serve.running = 0.0;
        for (i, g) in idle.gpus.iter_mut().enumerate() {
            set_util(g, if i == 3 { 100.0 } else { 0.0 });
        }
        let r = skew(&idle);
        assert_eq!(r.severity, 0.0, "a 100-point spread with the engine empty is not a fault: {r:?}");
        assert_ne!(lead(&idle).as_deref(), Some("gpu.skew"), "skew must not headline an idle box");
        // the SAME util vector with the engine actually running requests is still not a straggler,
        // because the pack is not loaded - there is nothing for the laggard to be holding up
        let mut running = idle.clone();
        running.serve.running = 3.0;
        assert_eq!(skew(&running).severity, 0.0, "one card ahead of a parked pack is not the pack waiting on one card");

        // THE CORROBORATION'S OWN CASE, and it has to be its own: the median fix alone does not
        // cover it. A LOADED pack with a genuinely lagging card - 95/95/95/20, a 75-point median
        // skew - while the engine reports NOTHING RUNNING. That is 12 percentage points of this
        // box's polls in the 24h replay (median skew scores on 21.4% of polls corroborated on the
        // util vector, 9.1% corroborated on the engine), and it is not a straggler: with no
        // request in flight there is no group and nothing being held up. Only the ENGINE can say
        // that, which is the whole reason the confirming source had to change.
        let mut no_work = calm();
        no_work.serve.running = 0.0;
        set_util(&mut no_work.gpus[3], 20.0);
        let r = skew(&no_work);
        assert_eq!(r.severity, 0.0, "a loaded pack with no request in flight is not a group waiting on anyone: {r:?}");
        assert_ne!(lead(&no_work).as_deref(), Some("gpu.skew"));
        // ...and the engine saying there IS work turns exactly that reading on
        let mut at_work = no_work.clone();
        at_work.serve.running = 3.0;
        assert!(skew(&at_work).severity >= HIGH, "{:?}", skew(&at_work));
        assert_eq!(lead(&at_work).as_deref(), Some("gpu.skew"));
        // ...and a genuinely lagging card in a loaded, working group DOES score and lead
        let mut lag = calm();
        set_util(&mut lag.gpus[3], 20.0); // 95/95/95/20
        assert_eq!(lead(&lag).as_deref(), Some("gpu.skew"));
        assert!(skew(&lag).severity >= HIGH, "{:?}", skew(&lag));
    }

    /// Medians, not extremes: `max - min` is two outliers subtracted from each other. One card
    /// running ahead of a pack that is keeping pace with itself is not a group held back, and it
    /// scored 1.0 under the old spread.
    #[test]
    fn one_card_ahead_of_an_even_pack_is_not_the_pack_waiting_on_one_card() {
        let mut s = calm();
        for (i, g) in s.gpus.iter_mut().enumerate() {
            set_util(g, if i == 0 { 100.0 } else { 55.0 });
        }
        // max - min is 45 points and would have scored 1.0; median - min is 0
        let r = skew(&s);
        assert_eq!(r.severity, 0.0, "{r:?}");
        assert_eq!(r.value.as_deref(), Some("0 pts behind the pack over 30s"));
        // and the mirror image - one card BEHIND the same pack - does score
        let mut s = calm();
        for (i, g) in s.gpus.iter_mut().enumerate() {
            set_util(g, if i == 0 { 10.0 } else { 55.0 });
        }
        assert_eq!(skew(&s).value.as_deref(), Some("45 pts behind the pack over 30s"));
        assert_eq!(lead(&s).as_deref(), Some("gpu.skew"));
    }

    #[test]
    fn a_queue_wait_scores_only_while_something_is_waiting() {
        let mut s = calm();
        s.serve.latency.as_mut().unwrap().queue_time.as_mut().unwrap().p50_ms = 4000.0;
        let r = all(&s, NOW).into_iter().find(|r| r.key == "serve.queue_wait").unwrap();
        assert_eq!(r.severity, 0.0, "a slow histogram with nobody waiting now is history");
        s.serve.queue = 2.0;
        assert_eq!(lead(&s).as_deref(), Some("serve.queue_wait"));
    }

    #[test]
    fn throttle_scores_only_with_a_reason_now() {
        let mut s = calm();
        s.gpus[1].sample.clock_mhz = Some(1200.0); // well below max, but no reason reported
        assert_eq!(all(&s, NOW).into_iter().find(|r| r.key == "gpu.1.throttle").unwrap().severity, 0.0);
        s.gpus[1].throttle = vec!["hw_slowdown".into()];
        assert_eq!(lead(&s).as_deref(), Some("gpu.1.throttle"));
        // a power cap that barely moves the clock (the normal loaded state) is not a problem
        let mut p = calm();
        p.gpus[0].throttle = vec!["sw_power_cap".into()];
        assert_eq!(lead(&p), None);
    }

    #[test]
    fn sw_power_cap_never_headlines_a_healthy_capped_card_even_at_a_real_deficit() {
        // card #179 (lss-verifier-4): a REAL sample row from this box, replayed from the
        // collector's own stored history - sw_power_cap, 268/300 W, clock 2130 of a 3090 MHz
        // boost ceiling (a 31% deficit against that ceiling, which no card here ever reaches
        // even idle). Before this fix: ramp(31, 5, 45) = 0.65 (HIGH) and this headlined. The
        // cap is a CHOSEN setting (the owner has settled the power question), not a fault, so
        // it must never lead - however far the clock sits below an unreachable boost number.
        let mut s = calm();
        s.gpus[0].throttle = vec!["sw_power_cap".into()];
        s.gpus[0].sample.clock_mhz = Some(2130.0);
        s.gpus[0].sample.power_w = Some(268.0);
        s.gpus[0].sample.power_limit_w = Some(300.0);
        s.gpus[0].sample.util_pct = Some(84.0);
        s.gpus[0].health = Some(GpuHealth { index: 0, clock_max_mhz: Some(3090.0), ..Default::default() });
        assert_ne!(lead(&s).as_deref(), Some("gpu.0.throttle"), "a chosen power cap must not headline");
        let r = all(&s, NOW).into_iter().find(|r| r.key == "gpu.0.throttle").unwrap();
        assert_eq!(r.severity, 0.0, "{r:?}");
        assert!(r.informational, "shown, but never eligible to lead - {r:?}");
        assert!(r.value.as_deref().is_some_and(|v| v.contains("sw_power_cap")), "still named in the text - {r:?}");
        // the same clock deficit WITH a real thermal reason present still scores and can lead
        s.gpus[0].throttle = vec!["hw_thermal".into(), "sw_power_cap".into()];
        assert_eq!(lead(&s).as_deref(), Some("gpu.0.throttle"));
        let r = all(&s, NOW).into_iter().find(|r| r.key == "gpu.0.throttle").unwrap();
        assert!(!r.informational, "a genuine constraint must score - {r:?}");
        assert!(r.value.as_deref().is_some_and(|v| v.contains("hw_thermal") && !v.contains("sw_power_cap")), "only the REAL reason drives the deficit text - {r:?}");
    }

    #[test]
    fn a_gateway_holding_requests_says_the_wait_is_ours() {
        let mut s = calm();
        s.lanes.trusted.waiters = 3;
        s.lanes.trusted.inflight_tokens = 990_000;
        let top = leading(&all(&s, NOW), None).unwrap().clone();
        assert!(top.key == "gate.trusted.budget" || top.key == "gate.waiters", "{top:?}");
        assert!(all(&s, NOW).iter().any(|r| r.detail.contains("the wait is ours")));
    }

    #[test]
    fn a_strangers_box_with_no_gateway_and_no_rates_gets_no_fake_rows() {
        let mut s = calm();
        s.gate.absent = true;
        s.gate.up = false;
        s.cost = None;
        s.loadout = None;
        let rs = all(&s, NOW);
        assert!(!rs.iter().any(|r| r.key.starts_with("gate.")), "no gateway rows at all");
        let c = rs.iter().find(|r| r.key == "cost").unwrap();
        assert_eq!(c.band(), Band::NoData);
        assert!(c.informational && c.detail.contains("cost tracking is off"));
        assert_eq!(leading(&rs, None), None);
    }

    #[test]
    fn a_firing_warn_alert_can_lead_and_an_info_alert_cannot() {
        let mut s = calm();
        s.firing = vec!["c1_stale".into()];
        s.alerts = vec![AlertRow { rule: "c1_stale".into(), severity: "info".into(), message: "C1 has not been able to measure".into(), ts: NOW - 10, ..Default::default() }];
        assert_eq!(lead(&s), None);
        s.firing.push("gate_waiters".into());
        s.alerts.push(AlertRow { rule: "gate_waiters".into(), severity: "warn".into(), message: "queue pressure".into(), ts: NOW - 5, ..Default::default() });
        assert_eq!(lead(&s).as_deref(), Some("alert.gate_waiters"));
    }

    /// Card #196: an OPEN incident is a dated record that has not been seen to close, NOT a claim
    /// that anything is wrong now. Page 1 painted every open non-maintenance incident red on its
    /// own, so one nobody closed would have shown red beside a verdict line saying OK. It scores
    /// only when a second source agrees the box is unwell - and there is deliberately no bolder
    /// rule than that, because this box's own history holds 36 incidents and not one has ever
    /// been left open, so any threshold for the open case would be invented rather than measured.
    #[test]
    fn an_open_incident_nobody_closed_is_not_a_claim_that_something_is_wrong_now() {
        use crate::incidents::Incident;
        let open = |kind: &str| Incident { id: 1, start: NOW - 500_000, end: None, kind: kind.into(), detail: "d".into() };
        let mut s = calm();
        s.incidents = vec![open("xid")];
        let r = all(&s, NOW).into_iter().find(|r| r.key == "events.open").unwrap();
        assert_eq!(r.severity, 0.0, "nothing else says the box is unwell: {r:?}");
        assert!(r.informational, "shown and ranked past, never coloured as a fault: {r:?}");
        assert_eq!(lead(&s), None, "and the page still says nothing is constrained");

        // the engine being down is a second source agreeing: NOW it scores
        let mut down = s.clone();
        down.serve.up = false;
        let r = all(&down, NOW).into_iter().find(|r| r.key == "events.open").unwrap();
        assert!(!r.informational && r.severity >= LOW, "{r:?}");
        assert_eq!(lead(&down).as_deref(), Some("serve.up"), "...though the engine itself still leads, being offline");

        // a firing warn alert is a second source too
        let mut firing = s.clone();
        firing.firing = vec!["gate_waiters".into()];
        firing.alerts = vec![AlertRow { rule: "gate_waiters".into(), severity: "warn".into(), message: "queue pressure".into(), ts: NOW - 5, ..Default::default() }];
        assert!(all(&firing, NOW).iter().find(|r| r.key == "events.open").unwrap().severity >= LOW);
        // ...but an INFO alert is not: the page already refuses to let one headline
        let mut info = s.clone();
        info.firing = vec!["c1_stale".into()];
        info.alerts = vec![AlertRow { rule: "c1_stale".into(), severity: "info".into(), message: "C1 could not measure".into(), ts: NOW - 5, ..Default::default() }];
        assert_eq!(all(&info, NOW).iter().find(|r| r.key == "events.open").unwrap().severity, 0.0);

        // planned maintenance left open is a note to yourself, whatever else is happening
        let mut maint = calm();
        maint.serve.up = false;
        maint.incidents = vec![open(crate::incidents::KIND_MAINTENANCE)];
        let r = all(&maint, NOW).into_iter().find(|r| r.key == "events.open").unwrap();
        assert!(r.informational && r.severity == 0.0, "{r:?}");

        // and a box with nothing open has no row at all rather than a zero
        assert!(!all(&calm(), NOW).iter().any(|r| r.key == "events.open"));
    }

    /// Card #196: an alert's severity word becomes a band in ONE place, so the ALERTS box beside
    /// the verdict line cannot paint an `info` alert amber while the line ranks it below LOW.
    #[test]
    fn an_alerts_severity_word_becomes_a_band_in_one_place() {
        assert!(alert_score("info") < LOW, "an info alert is not a problem");
        assert_eq!(Band::of(alert_score("warn")), Band::High);
        assert_eq!(Band::of(alert_score("warning")), Band::High);
        assert_eq!(Band::of(alert_score("critical")), Band::Critical);
        assert_eq!(Band::of(alert_score("crit")), Band::Critical);
        // #198: every word `rules::Severity` can produce is in the table. `page` and `hardware`
        // fell through to the `_` arm, so the two most urgent words the rule engine has were the
        // only ones page 1 gave no ink at all while a `warn` beside them went amber.
        assert_eq!(Band::of(alert_score("page")), Band::Critical, "`page` is serve_down_page: wake someone");
        assert_eq!(Band::of(alert_score("hardware")), Band::High, "a GPU fault is amber - red is Critical and Offline only");
        assert!(alert_score("hardware") > alert_score("info") && alert_score("page") >= alert_score("warn"), "the urgent words outrank the quiet ones");
        // and `events()` uses it rather than a copy
        let mut s = calm();
        s.firing = vec!["r".into()];
        s.alerts = vec![AlertRow { rule: "r".into(), severity: "critical".into(), message: "boom".into(), ts: NOW, ..Default::default() }];
        let r = all(&s, NOW).into_iter().find(|r| r.key == "alert.r").unwrap();
        assert_eq!(r.severity, alert_score("critical"));
    }

    /// CARD #198, the first of page 1's two hand-painted ambers. `[not delivered]` was amber by
    /// kind; on this box that made it a second way of painting an `info` alert amber (all 13
    /// undelivered alerts on record are `info`, and every `warn` got out), which is exactly the
    /// contradiction #196 removed from the severity word. It is now a reading per alert row,
    /// scored only when the alert's own severity is one the page cares about AND delivery has
    /// demonstrably worked on this box.
    #[test]
    fn an_undelivered_alert_is_judged_by_its_own_severity_and_only_where_a_sink_works() {
        let alert = |id: i64, sev: &str, delivered: bool| AlertRow {
            id, ts: NOW - 60, rule: format!("r{id}"), severity: sev.into(), message: "something happened".into(), recovered: false, delivered,
        };
        let get = |s: &Status, id: i64| all(s, NOW).into_iter().find(|r| r.key == format!("events.undelivered.{id}")).unwrap_or_else(|| panic!("no reading for alert {id}"));

        // a working sink (one alert got out) that dropped a warn: that is the fault
        let mut s = calm();
        s.alerts = vec![alert(1, "warn", false), alert(2, "warn", true)];
        let r = get(&s, 1);
        assert!(!r.informational && Band::of(r.severity) == Band::High, "amber, never red: {r:?}");
        assert!(r.severity <= MEDIUM, "the delivery path is never worse than amber: {r:?}");
        assert!(r.detail.contains("nobody was told"), "{}", r.detail);
        assert_eq!(lead(&s).as_deref(), Some("events.undelivered.1"), "nothing else is wrong, so being un-paged is the worst thing on the page");
        assert!(!all(&s, NOW).iter().any(|r| r.key == "events.undelivered.2"), "a delivered alert has no row at all");

        // the same box, the same flag, an INFO alert: not a fault, and it cannot colour anything
        let mut s = calm();
        s.alerts = vec![alert(1, "info", false), alert(2, "warn", true)];
        let r = get(&s, 1);
        assert!(r.informational && r.severity == 0.0, "{r:?}");
        assert!(r.detail.contains("below watch"), "{}", r.detail);
        assert_eq!(lead(&s), None);

        // and a box where NOTHING has ever been handed over has no sink to have failed: a
        // permanent amber on every alert would report a missing feature as a fault
        let mut s = calm();
        s.alerts = vec![alert(1, "critical", false), alert(2, "warn", false)];
        for id in [1, 2] {
            let r = get(&s, id);
            assert!(r.informational && r.severity == 0.0, "{r:?}");
            assert!(r.detail.contains("no alert sink configured"), "{}", r.detail);
        }
        assert_eq!(lead(&s), None);
    }

    /// CARD #198, the second amber: the word `xid` in the INCIDENTS box. It was painted by KIND
    /// on a dated record inside a seven-day list, so on this box's own stored samples (17,333
    /// polls, 7 Xids on record) that ink was lit on 100.00% of the last day's polls - 98.04% at
    /// 24h, 23.29% at 6h, 2.57% at 1h, which is why `XID_FRESH_SECS` is an hour. A fresh fault is
    /// amber; an old one is history until something else says the box is unwell now.
    #[test]
    fn an_xid_is_coloured_while_it_is_fresh_or_while_the_box_is_unwell_and_never_just_because_it_is_on_record() {
        use crate::incidents::Incident;
        let xid = |id: i64, ago: i64| Incident { id, start: NOW - ago, end: Some(NOW - ago), kind: crate::incidents::KIND_XID.into(), detail: "GPU0 Xid 8 (GPU stopped processing (hang / watchdog))".into() };
        let get = |s: &Status, id: i64| all(s, NOW).into_iter().find(|r| r.key == format!("events.xid.{id}")).unwrap_or_else(|| panic!("no reading for incident {id}"));

        // fresh: the driver reported a hardware fault minutes ago
        let mut s = calm();
        s.incidents = vec![xid(1, 600)];
        let r = get(&s, 1);
        assert!(!r.informational && Band::of(r.severity) == Band::High, "amber, never red: {r:?}");
        assert!(r.detail.contains("10m ago") && r.detail.contains("Xid 8"), "{}", r.detail);
        assert_eq!(lead(&s).as_deref(), Some("events.xid.1"));

        // one hour and one second later it is a dated record, not something happening now
        let mut s = calm();
        s.incidents = vec![xid(1, XID_FRESH_SECS + 1)];
        let r = get(&s, 1);
        assert!(r.informational && r.severity == 0.0, "{r:?}");
        assert!(r.detail.contains("dated record"), "{}", r.detail);
        assert_eq!(lead(&s), None, "a week of Xids on the books must not keep page 1 amber for the week");

        // ...unless something else says the box is unwell: then the newest fault is the one that
        // could explain it, and the older ones stay history
        let mut s = calm();
        s.incidents = vec![xid(1, 4 * 86_400), xid(2, XID_FRESH_SECS * 3)];
        s.serve.up = false;
        let newest = get(&s, 2);
        assert!(!newest.informational && newest.severity >= LOW, "{newest:?}");
        assert!(newest.detail.contains("could explain it"), "{}", newest.detail);
        assert!(get(&s, 1).informational, "only the newest fault is offered as the explanation");

        // a restart or an outage record is never given this reading at all
        let mut s = calm();
        s.incidents = vec![Incident { id: 9, start: NOW - 60, end: Some(NOW - 30), kind: crate::incidents::KIND_CONTAINER_RESTART.into(), detail: "serve restarted".into() }];
        assert!(!all(&s, NOW).iter().any(|r| r.key.starts_with("events.xid.")));
    }

    #[test]
    fn a_slow_single_request_names_speed_and_a_stale_probe_is_no_data() {
        let mut s = calm();
        s.probe.last_ok.as_mut().unwrap().decode_tok_s = Some(90.0); // half of normal
        assert_eq!(lead(&s).as_deref(), Some("serve.speed"));
        s.probe.last_ok.as_mut().unwrap().ts = NOW - 3600;
        let r = all(&s, NOW).into_iter().find(|r| r.key == "serve.speed").unwrap();
        assert_eq!(r.band(), Band::NoData);
        assert!(r.detail.contains("too busy"), "{r:?}");
    }

    #[test]
    fn merged_from_177_silent_engine_full_slots_clean_throttle_and_an_engine_full_hold() {
        // running but silent: the Xid-8 precursor leads
        let mut s = calm();
        s.serve.secs_since_last_token = Some(80);
        assert_eq!(lead(&s).as_deref(), Some("serve.silent"));
        // 'clean' is the driver saying NOTHING is wrong - never a throttle reason
        let mut s = calm();
        s.gpus[1].throttle = vec!["clean".into()];
        s.gpus[1].sample.clock_mhz = Some(1000.0);
        assert_eq!(all(&s, NOW).into_iter().find(|r| r.key == "gpu.1.throttle").unwrap().value.as_deref(), Some("none"));
        // every slot busy leads as slots
        let mut s = calm();
        s.serve.running = 8.0;
        assert_eq!(lead(&s).as_deref(), Some("serve.slots"));
        // ...and requests held at the gateway THEN are the engine's wait, not ours: not scored
        s.lanes.trusted.waiters = 3;
        let r = all(&s, NOW).into_iter().find(|r| r.key == "gate.waiters").unwrap();
        assert_eq!(r.severity, 0.0, "{r:?}");
        assert!(!r.detail.contains("the wait is ours"));
    }

    #[test]
    fn a_fresh_engine_that_has_read_nothing_is_not_a_zero_percent_cache() {
        let mut s = calm();
        s.serve.prompt_tokens_total = 0.0;
        s.serve.cache_hit_rate = 0.0;
        let r = all(&s, NOW).into_iter().find(|r| r.key == "serve.cache_hit").unwrap();
        assert_eq!(r.band(), Band::NoData, "{r:?}");
        assert_eq!(lead(&s), None);
        // once it has read prompts AND a second source confirms requests are actually slow
        // (TTFT past the probe-validity threshold), a 0 % hit rate is real and leads.
        s.serve.prompt_tokens_total = 1e6;
        s.serve.ttft_avg_ms_10m = Some(6_000.0); // > thresholds.c1_max_ttft_s (5.0 s)
        assert_eq!(lead(&s).as_deref(), Some("serve.cache_hit"));
    }

    /// CARD #196, RATES NOT COUNTERS - and the close of the finding #179 shipped with open.
    /// The engine's `cache_hit_rate` is a per-prefill-report SNAPSHOT: one cold batch between two
    /// warm ones puts it at 0% with nothing wrong. #179 wrapped it in corroboration; replayed
    /// over 24h of this box's own stored samples (17,135 polls with a usable window) that left it
    /// able to headline on 2.54% of polls, because the corroborator (TTFT > 5 s) is true of 15.93%
    /// of ALL polls here and agreed with the snapshot at 15.8% - i.e. AT CHANCE, which is not a
    /// second source. The windowed, token-weighted `cached_share_10m` scored on 0.00% of the same
    /// polls (min 59.4%, median 99.3%) and still catches a real collapse.
    #[test]
    fn a_cold_batch_cannot_headline_a_box_whose_ten_minute_cache_share_is_healthy() {
        let mut s = calm();
        s.serve.cached_share_10m = Some(0.99);
        s.serve.cache_hit_rate = 0.0; // the engine's last prefill report: one cold batch
        s.serve.ttft_avg_ms_10m = Some(6_000.0); // ...with the old corroborator agreeing, as it did at chance
        let r = all(&s, NOW).into_iter().find(|r| r.key == "serve.cache_hit").unwrap();
        assert_eq!(r.severity, 0.0, "a ten-minute share of 99% is not a cache problem: {r:?}");
        assert_ne!(lead(&s).as_deref(), Some("serve.cache_hit"));
        assert!(r.value.as_deref().is_some_and(|v| v.contains("99%") && v.contains("10 min")), "the judged number says which window it is: {r:?}");
        // the engine's own last number stays VISIBLE and is never judged - it is a fact about the
        // engine's report, not about the box
        let now = all(&s, NOW).into_iter().find(|r| r.key == "serve.cache_hit_now").unwrap();
        assert!(now.informational && now.value.as_deref() == Some("0%"), "{now:?}");
        // a REAL, sustained collapse - ten minutes of it - still leads
        s.serve.cached_share_10m = Some(0.02);
        assert_eq!(lead(&s).as_deref(), Some("serve.cache_hit"));
        // ...and only while something else agrees requests are actually slower
        s.serve.ttft_avg_ms_10m = None;
        assert_ne!(lead(&s).as_deref(), Some("serve.cache_hit"));
        // an older collector that publishes no window at all keeps the previous behaviour rather
        // than losing the reading: the snapshot, corroborated
        let mut old = calm();
        old.serve.cached_share_10m = None;
        old.serve.cache_hit_rate = 0.0;
        old.serve.ttft_avg_ms_10m = Some(6_000.0);
        assert_eq!(lead(&old).as_deref(), Some("serve.cache_hit"));
        assert!(!all(&old, NOW).iter().any(|r| r.key == "serve.cache_hit_now"), "and no second row inventing a window it does not have");
    }

    /// Card #196: the gateway is judged PER LANE, so the headline names which lane is refusing
    /// work. "Outside callers are being turned away" and "my own agents are being turned away"
    /// are different problems with different fixes, and the summed row could say neither.
    #[test]
    fn the_gateway_names_which_lane_is_refusing_work() {
        let mut s = calm();
        s.lanes.public.codes_10m.c4xx = 10;
        let r = leading(&all(&s, NOW), None).unwrap().clone();
        assert_eq!(r.key, "gate.public.turned");
        assert!(r.detail.contains("Outside callers"), "{r:?}");
        let rs = all(&s, NOW);
        let quiet = rs.iter().find(|r| r.key == "gate.trusted.turned").unwrap();
        assert_eq!(quiet.severity, 0.0, "the clean lane has its own row and stays quiet: {quiet:?}");
        // the public lane's budget is judged too - only the trusted one ever was
        let mut s = calm();
        s.lanes.public.budget_tokens = Some(1_000_000);
        s.lanes.public.inflight_tokens = 990_000;
        assert_eq!(lead(&s).as_deref(), Some("gate.public.budget"));
    }

    #[test]
    fn a_zero_percent_cache_reading_with_nothing_else_slow_does_not_headline() {
        // card #179 (verifier, lss-verifier-4): the engine's cache_hit_rate is a per-report
        // snapshot, not windowed - one cold batch between two warm ones reads as 0 % with
        // nothing actually wrong. Measured on a healthy server: unconfirmed, this alone
        // headlined page 1 on 23 % of polls. Confirmed only by a second source (TTFT, or
        // prefill running well under its own typical).
        let mut s = calm();
        s.serve.cache_hit_rate = 0.0; // real reads happened (prompt_tokens_total is calm()'s 5e9)
        assert_ne!(lead(&s).as_deref(), Some("serve.cache_hit"), "unconfirmed - must not headline");
        let r = all(&s, NOW).into_iter().find(|r| r.key == "serve.cache_hit").unwrap();
        assert_eq!(r.severity, 0.0, "{r:?}");
        // TTFT confirms it: NOW it leads
        s.serve.ttft_avg_ms_10m = Some(6_000.0);
        assert_eq!(lead(&s).as_deref(), Some("serve.cache_hit"));
        // prefill throughput confirms it too, on its own
        let mut s = calm();
        s.serve.cache_hit_rate = 0.0;
        s.serve.prefill_tok_s = Some(400.0);
        s.serve.prefill_tok_s_typical = Some(4_000.0); // < half of typical
        assert_eq!(lead(&s).as_deref(), Some("serve.cache_hit"));
    }

    #[test]
    fn the_golden_status_projects_cleanly() {
        // a real, complete /status document (the fixture every renderer is tested against)
        let s: Status = serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).unwrap();
        let rs = all(&s, s.generated_at);
        assert!(rs.len() > 20, "{}", rs.len());
        let mut keys: Vec<_> = rs.iter().map(|r| r.key.as_str()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n);
        assert!(rs.iter().all(|r| r.detail.ends_with('.')));
        if let Some(l) = leading(&rs, None) {
            assert!(!l.informational);
        }
    }

    #[test]
    fn every_reading_has_a_unique_key_and_a_one_sentence_detail() {
        let mut s = calm();
        s.firing = vec!["x".into()];
        for rs in [all(&s, NOW), all(&Status::default(), NOW)] {
            let mut keys: Vec<_> = rs.iter().map(|r| r.key.clone()).collect();
            let n = keys.len();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), n, "duplicate keys: {rs:#?}");
            for r in &rs {
                assert!(r.detail.ends_with('.') && !r.detail.is_empty(), "detail must be a sentence: {r:?}");
                assert!(r.detail.matches(". ").count() == 0, "one sentence, not two: {r:?}");
                if r.state != State::Live {
                    assert!(r.value.is_none());
                }
            }
        }
    }
}
