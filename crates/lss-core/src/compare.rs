//! `lss loadouts` and `lss compare A B`: every loadout with its passive scorecard and its newest
//! bench scorecards, and two of them side by side with deltas, a +-3 % noise band and one
//! plain-language verdict per category.
//!
//! Like is only compared with like: a number from `lss bench` is compared with the other
//! side's bench number, a number measured passively from live traffic with the other side's
//! live number. A bench figure against a live figure would compare the workloads, not the models.

use crate::bench::{Headline, Scorecard};
use crate::loadout::{LoadoutRow, NOISE};
use serde::{Deserialize, Serialize};

/// One loadout: what real traffic showed, and the newest run of each bench profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutCard {
    #[serde(flatten)]
    pub row: LoadoutRow,
    /// the newest COMPLETE run of each profile (an aborted run never becomes the scorecard)
    pub quick: Option<Scorecard>,
    pub full: Option<Scorecard>,
    /// the newest complete accuracy run per dataset
    pub accuracy: Vec<Scorecard>,
    /// when each profile last finished, complete or not (what `lss loadouts` dates)
    pub last_quick_at: Option<i64>,
    pub last_full_at: Option<i64>,
    pub last_accuracy_at: Option<i64>,
    /// "about N more simultaneous users before waits start", with the reasoning (added 2026-09-20)
    pub headroom: Option<crate::headroom::Headroom>,
}

impl LoadoutCard {
    /// LONG-CONVERSATION SPEED: decode tok/s for one user when 0 / 16k / 64k / 128k / 250k tokens
    /// are already in the conversation. A bench cell overrides what live traffic showed.
    pub fn long_context(&self) -> Vec<crate::loadout::CtxRow> {
        let mut rows = if self.row.long_context.is_empty() {
            crate::loadout::CTX_BUCKETS.iter().map(|(b, ctx, _)| crate::loadout::CtxRow { bucket: (*b).to_string(), context_tokens: *ctx, source: "live".into(), ..Default::default() }).collect()
        } else {
            self.row.long_context.clone()
        };
        for r in &mut rows {
            // the newest scorecard that measured this context wins (full has the long ones)
            let cell = [self.full.as_ref(), self.quick.as_ref()].into_iter().flatten().filter_map(|s| s.decode.iter().find(|c| c.concurrency == 1 && crate::loadout::ctx_bucket(c.context as f64) == r.bucket && c.per_user_tok_s > 0.0 && c.errors == 0)).next();
            if let Some(c) = cell {
                r.tok_s = Some(c.per_user_tok_s);
                r.source = "bench".into();
            }
        }
        rows
    }

    /// The headroom estimate from this card's own evidence.
    pub fn estimate_headroom(&self, min_tok_s_per_user: f64) -> crate::headroom::Headroom {
        let curve = crate::bench::merge_curve(&self.row.curve, self.speed());
        // same vetoed claim as ADVICE (#38): a level or a peak above the claimed ceiling means
        // there is no ceiling, whatever the qualified levels alone would say
        let saturation = crate::loadout::saturation_of(&curve, self.row.peak_tok_s);
        crate::headroom::estimate(&crate::headroom::HeadroomInputs { slots: self.row.slots, curve: &curve, live: &self.row.curve, kv_peak: self.row.kv_peak, saturation: Some(&saturation), min_tok_s_per_user })
    }

    /// The speed scorecard to read: `full` when there is one, else `quick`.
    pub fn speed(&self) -> Option<&Scorecard> {
        self.full.as_ref().or(self.quick.as_ref())
    }

    /// The headline numbers: bench where it ran, the passive figures otherwise.
    pub fn headline(&self) -> Headline {
        let mut h = self.speed().map(Scorecard::headline).unwrap_or_default();
        let r = &self.row;
        h.c1_tok_s = h.c1_tok_s.or(r.c1_tok_s);
        h.ttft_c1_ms = h.ttft_c1_ms.or(r.ttft_p50_ms);
        if h.max_total_tok_s.is_none() {
            h.max_total_tok_s = r.peak_tok_s;
            h.max_total_at = r.peak_at_running.map(|n| n as u32);
        }
        h.prefill_8k_tok_s = h.prefill_8k_tok_s.or(r.prefill_tok_s);
        if let Some(a) = self.accuracy.first().and_then(|s| s.accuracy.first()) {
            h.accuracy = Some(a.score);
            h.accuracy_dataset = Some(a.dataset.clone());
        }
        h
    }
}

/// `GET /loadouts`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutsDoc {
    pub v: u32,
    pub generated_at: i64,
    /// newest first; the one with `current: true` is what is serving now
    pub loadouts: Vec<LoadoutCard>,
}

/// `current` | `previous` | `best` | a loadout id (prefix) | a model name (prefix).
pub fn resolve<'a>(selector: &str, cards: &'a [LoadoutCard]) -> Result<&'a LoadoutCard, String> {
    let sel = selector.trim();
    if cards.is_empty() {
        return Err("no loadout on record yet: the collector records one as soon as it sees the serve up".into());
    }
    let newest_first = || {
        let mut v: Vec<&LoadoutCard> = cards.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.row.last_seen));
        v
    };
    match sel {
        "current" => cards.iter().find(|c| c.row.current).ok_or_else(|| "nothing is serving right now, so there is no `current` loadout (try `previous`, or an id from `lss loadouts`)".into()),
        "previous" => newest_first().into_iter().find(|c| !c.row.current).ok_or_else(|| "there is no `previous` loadout yet: only one has ever been seen".into()),
        "best" => cards
            .iter()
            .filter(|c| c.headline().max_total_tok_s.is_some())
            .max_by(|a, b| a.headline().max_total_tok_s.unwrap_or(0.0).total_cmp(&b.headline().max_total_tok_s.unwrap_or(0.0)))
            .ok_or_else(|| "no loadout has a measured total speed yet, so there is no `best`".into()),
        "" => Err("an empty selector: use current, previous, best, a loadout id or a model name".into()),
        _ => {
            let by_id: Vec<&LoadoutCard> = cards.iter().filter(|c| sel.len() >= 4 && c.row.id.starts_with(&sel.to_ascii_lowercase())).collect();
            match by_id.len() {
                1 => return Ok(by_id[0]),
                n if n > 1 => return Err(format!("`{sel}` matches {n} loadout ids ({}): use more characters", by_id.iter().map(|c| c.row.id.as_str()).collect::<Vec<_>>().join(", "))),
                _ => {}
            }
            let low = sel.to_lowercase();
            newest_first().into_iter().find(|c| c.row.model.to_lowercase().starts_with(&low)).ok_or_else(|| format!("no loadout matches `{sel}`: use current, previous, best, an id or a model name from `lss loadouts`"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    /// within the noise band
    Same,
    /// A is clearly better than B
    Better,
    Worse,
    /// one side (or both) has no such measurement
    NoData,
    /// card #14: both sides HAVE the measurement, and the two were taken under background load
    /// too different for the difference to mean anything (`bench::load_gap`). The numbers are
    /// still shown - the subtraction is not.
    Incomparable,
}

pub const CATEGORIES: [(&str, &str); 7] = [
    ("speed_alone", "speed alone"),
    ("speed_under_load", "speed under load"),
    ("reading_speed", "reading speed"),
    ("long_context", "long context"),
    ("accuracy", "accuracy"),
    ("efficiency", "efficiency"),
    ("reliability", "reliability"),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareRow {
    pub category: String,
    pub label: String,
    pub unit: String,
    pub a: Option<f64>,
    pub b: Option<f64>,
    /// `bench` | `live`: where BOTH numbers come from; empty when there is no pair
    pub source: String,
    pub higher_is_better: bool,
    /// (a - b) / b, percent; null without a pair
    pub delta_pct: Option<f64>,
    pub mark: Mark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub category: String,
    pub title: String,
    pub sentence: String,
}

/// `lss compare A B --json`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub v: u32,
    pub a: Side,
    pub b: Side,
    pub noise_pct: f64,
    /// card #14: why the two sides' BENCH numbers must not be subtracted from each other (their
    /// background load was too different, or one of them never recorded it). `null` = they may
    /// be. Live-traffic rows are unaffected: they were never claimed to be measured alone.
    pub load_warning: Option<String>,
    pub rows: Vec<CompareRow>,
    pub verdicts: Vec<Verdict>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Side {
    pub selector: String,
    pub id: String,
    pub model: String,
    pub image_tag: String,
    pub flags: String,
    pub first_seen: i64,
    /// the bench profile the speed numbers come from, if any
    pub bench_profile: Option<String>,
    pub bench_at: Option<i64>,
    /// card #14: `quiet` / `loaded` / `unknown` for the scorecard the bench numbers come from
    pub bench_load: crate::bench::LoadClass,
    /// the same in one sentence, for a screen that has room for it
    pub bench_load_detail: String,
}

fn side(selector: &str, c: &LoadoutCard) -> Side {
    Side {
        selector: selector.to_string(),
        id: c.row.id.clone(),
        model: c.row.model.clone(),
        image_tag: c.row.image_tag.clone(),
        flags: c.row.flags.clone(),
        first_seen: c.row.first_seen,
        bench_profile: c.speed().map(|s| s.profile.clone()),
        bench_at: c.speed().map(|s| s.ended_at),
        bench_load: c.speed().map(Scorecard::load_class).unwrap_or_default(),
        bench_load_detail: c.speed().map(Scorecard::load_sentence).unwrap_or_default(),
    }
}

/// The delta of A against B and what it means, with the noise band applied.
pub fn judge(a: Option<f64>, b: Option<f64>, higher_is_better: bool) -> (Option<f64>, Mark) {
    match (a, b) {
        // a rate that was zero on the other side: there is no percentage, but there is a direction
        (Some(a), Some(b)) if a.is_finite() && b == 0.0 => {
            if a == 0.0 {
                (Some(0.0), Mark::Same)
            } else {
                (None, if (a > 0.0) == higher_is_better { Mark::Better } else { Mark::Worse })
            }
        }
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() && b != 0.0 => {
            let delta = (a - b) / b.abs();
            let mark = if delta.abs() <= NOISE + 1e-12 {
                Mark::Same
            } else if (delta > 0.0) == higher_is_better {
                Mark::Better
            } else {
                Mark::Worse
            };
            (Some((delta * 1000.0).round() / 10.0), mark)
        }
        _ => (None, Mark::NoData),
    }
}

/// (bench value, live value) of one metric on one side.
type Pick = fn(&LoadoutCard) -> (Option<f64>, Option<f64>);

fn bench_cell(c: &LoadoutCard, concurrency: u32, context: u64) -> Option<&crate::bench::DecodeCell> {
    c.speed().and_then(|s| s.cell(concurrency, context))
}

fn top_common_concurrency(a: &LoadoutCard, b: &LoadoutCard) -> Option<u32> {
    let levels = |c: &LoadoutCard| -> Vec<u32> { c.speed().map(|s| s.decode.iter().filter(|d| d.context == 0).map(|d| d.concurrency).collect()).unwrap_or_default() };
    let lb = levels(b);
    levels(a).into_iter().filter(|n| lb.contains(n)).max()
}

pub fn compare(sel_a: &str, a: &LoadoutCard, sel_b: &str, b: &LoadoutCard) -> Comparison {
    let mut rows: Vec<CompareRow> = Vec::new();
    let mut push = |category: &str, label: &str, unit: &str, higher: bool, pa: (Option<f64>, Option<f64>), pb: (Option<f64>, Option<f64>)| {
        // the same source on both sides, bench first
        let (va, vb, source) = match (pa, pb) {
            ((Some(x), _), (Some(y), _)) => (Some(x), Some(y), "bench"),
            ((_, Some(x)), (_, Some(y))) => (Some(x), Some(y), "live"),
            ((x, lx), (y, ly)) => (x.or(lx), y.or(ly), ""),
        };
        let (delta_pct, mark) = if source.is_empty() { (None, Mark::NoData) } else { judge(va, vb, higher) };
        rows.push(CompareRow { category: category.into(), label: label.into(), unit: unit.into(), a: va, b: vb, source: source.into(), higher_is_better: higher, delta_pct, mark });
    };
    let metric = |pick: Pick| (pick(a), pick(b));

    let (pa, pb) = metric(|c| (bench_cell(c, 1, 0).map(|d| d.per_user_tok_s), c.row.c1_tok_s));
    push("speed_alone", "one user alone", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (bench_cell(c, 1, 0).and_then(|d| d.ttft_ms), c.row.ttft_p50_ms));
    push("speed_alone", "time to first word", "ms", false, pa, pb);

    let (pa, pb) = metric(|c| (c.speed().and_then(|s| s.headline().max_total_tok_s), c.row.peak_tok_s));
    push("speed_under_load", "best total speed", "tok/s", true, pa, pb);
    if let Some(n) = top_common_concurrency(a, b) {
        let at = |c: &LoadoutCard, f: fn(&crate::bench::DecodeCell) -> f64| (bench_cell(c, n, 0).map(f), None);
        push("speed_under_load", &format!("each user, {n} at once"), "tok/s", true, at(a, |d| d.per_user_tok_s), at(b, |d| d.per_user_tok_s));
        push("speed_under_load", &format!("total, {n} at once"), "tok/s", true, at(a, |d| d.total_tok_s), at(b, |d| d.total_tok_s));
    }

    let (pa, pb) = metric(|c| (c.speed().and_then(|s| s.prefill_near(8_192)).map(|p| p.tok_s), c.row.prefill_tok_s));
    push("reading_speed", "reading an 8k prompt", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (c.speed().and_then(|s| s.prefill_near(65_536)).map(|p| p.tok_s), None));
    push("reading_speed", "reading a 64k prompt", "tok/s", true, pa, pb);

    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.prefill_near(131_072)).map(|p| p.tok_s), None));
    push("long_context", "reading a 128k prompt", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (bench_cell(c, 1, 16_384).map(|d| d.per_user_tok_s), None));
    push("long_context", "one user, 16k already read", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.cell(1, 65_536)).map(|d| d.per_user_tok_s), None));
    push("long_context", "one user, 64k already read", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.cell(1, 131_072)).map(|d| d.per_user_tok_s), None));
    push("long_context", "one user, 128k already read", "tok/s", true, pa, pb);
    let (pa, pb) = metric(|c| (c.full.as_ref().filter(|s| !s.needle.is_empty()).map(|s| s.needle.iter().filter(|n| n.pass).count() as f64), None));
    push("long_context", "needle found (of 3 depths)", "", true, pa, pb);

    let mut datasets: Vec<String> = a.accuracy.iter().chain(b.accuracy.iter()).flat_map(|s| s.accuracy.iter().map(|x| x.dataset.clone())).collect();
    datasets.sort();
    datasets.dedup();
    for d in &datasets {
        let score = |c: &LoadoutCard| (c.accuracy.iter().flat_map(|s| s.accuracy.iter()).find(|x| &x.dataset == d).map(|x| (x.score * 1000.0).round() / 10.0), None);
        push("accuracy", d, "%", true, score(a), score(b));
    }
    if datasets.is_empty() {
        push("accuracy", "accuracy", "%", true, (None, None), (None, None));
    }

    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.tokens_per_joule), c.row.tokens_per_joule));
    push("efficiency", "tokens per joule", "tok/J", true, pa, pb);
    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.wh_per_mtok), c.row.wh_per_mtok));
    push("efficiency", "energy per 1M tokens", "Wh", false, pa, pb);

    // reliability: what real traffic showed (there is no benchmark for staying up), plus the
    // bench's garbled-output rate
    let pct100 = |v: Option<f64>| v.map(|x| (x * 1_000_000.0).round() / 10_000.0);
    push("reliability", "time up", "%", true, (None, a.row.uptime_pct), (None, b.row.uptime_pct));
    push("reliability", "requests that failed (5xx)", "%", false, (None, pct100(a.row.error_rate)), (None, pct100(b.row.error_rate)));
    push("reliability", "requests turned away (429)", "%", false, (None, pct100(a.row.rate_429)), (None, pct100(b.row.rate_429)));
    push("reliability", "cold start", "s", false, (None, a.row.cold_start_avg_s), (None, b.row.cold_start_avg_s));
    let (pa, pb) = metric(|c| (c.full.as_ref().and_then(|s| s.garbled).map(|g| g.per_100k), None));
    push("reliability", "garbled output per 100k chars", "", false, pa, pb);

    // card #14: a difference between two runs taken under materially different background load
    // is not a difference between the two loadouts, and printing it as one is worse than
    // printing nothing. The numbers stay on screen - only the subtraction is withdrawn, and only
    // for the rows that came from `lss bench`.
    let load_warning = match (a.speed(), b.speed()) {
        (Some(sa), Some(sb)) => crate::bench::load_gap(sa, sb),
        _ => None,
    };
    if load_warning.is_some() {
        for r in rows.iter_mut().filter(|r| r.source == "bench") {
            r.delta_pct = None;
            r.mark = Mark::Incomparable;
        }
    }

    let verdicts = CATEGORIES.iter().map(|(key, title)| Verdict { category: (*key).into(), title: (*title).into(), sentence: verdict(key, &rows, sel_a, sel_b, load_warning.as_deref()) }).collect();
    Comparison { v: crate::STATUS_SCHEMA_VERSION, a: side(sel_a, a), b: side(sel_b, b), noise_pct: NOISE * 100.0, load_warning, rows, verdicts }
}

/// The row with the biggest difference among those marked `mark`; the EARLIER row wins a tie
/// (rows are listed most meaningful first).
fn biggest<'a>(rows: &[&'a CompareRow], mark: Mark) -> Option<&'a CompareRow> {
    let mut best: Option<&CompareRow> = None;
    for r in rows.iter().copied().filter(|r| r.mark == mark) {
        if best.is_none_or(|b| r.delta_pct.unwrap_or(0.0).abs() > b.delta_pct.unwrap_or(0.0).abs()) {
            best = Some(r);
        }
    }
    best
}

/// One plain sentence for a category, led by its first row that has a pair.
fn verdict(category: &str, rows: &[CompareRow], a: &str, b: &str, load_warning: Option<&str>) -> String {
    let of: Vec<&CompareRow> = rows.iter().filter(|r| r.category == category).collect();
    let paired: Vec<&CompareRow> = of.iter().copied().filter(|r| !matches!(r.mark, Mark::NoData | Mark::Incomparable)).collect();
    // card #14: this category had a pair and lost it to the load gap - say THAT, not "not
    // measured on both". The two failures want opposite actions from the reader.
    if paired.is_empty() && load_warning.is_some() && of.iter().any(|r| r.mark == Mark::Incomparable) {
        // short on purpose: the full reason is printed ONCE, next to the two sides, and repeating
        // a paragraph under all seven categories buries it instead of making it louder
        return "cannot be compared: the two runs were measured under different background load, so the difference would be the traffic as much as the loadout (the note above says by how much)".to_string();
    }
    let (better_word, worse_word) = match category {
        "accuracy" => ("more accurate", "less accurate"),
        "efficiency" => ("more efficient", "less efficient"),
        "reliability" => ("more reliable", "less reliable"),
        "long_context" => ("better", "worse"),
        _ => ("faster", "slower"),
    };
    let Some(lead) = paired.first() else {
        return match category {
            "accuracy" => format!("not measured on both: run `lss bench accuracy` on each (long: pick a quiet window), then `lss compare {a} {b} --accuracy`"),
            "long_context" => "not measured on both: `lss bench full` measures 128k prompts and the needle".to_string(),
            "reliability" => "not enough real traffic on both sides yet".to_string(),
            _ => "no measurement on both sides yet: run `lss bench quick` on each loadout".to_string(),
        };
    };
    let better = paired.iter().filter(|r| r.mark == Mark::Better).count();
    let worse = paired.iter().filter(|r| r.mark == Mark::Worse).count();
    let amount = |r: &CompareRow| match r.delta_pct {
        Some(d) => format!("{} {:.0}%", r.label, d.abs()),
        None => r.label.clone(),
    };
    if better == 0 && worse == 0 {
        format!("~ same: every difference is within the +-{:.0}% run-to-run noise", NOISE * 100.0)
    } else if worse == 0 {
        format!("{a} is {better_word} than {b} ({})", amount(biggest(&paired, Mark::Better).unwrap_or(lead)))
    } else if better == 0 {
        format!("{a} is {worse_word} than {b} ({})", amount(biggest(&paired, Mark::Worse).unwrap_or(lead)))
    } else {
        let wins: Vec<String> = paired.iter().filter(|r| r.mark == Mark::Better).map(|r| amount(r)).collect();
        let losses: Vec<String> = paired.iter().filter(|r| r.mark == Mark::Worse).map(|r| amount(r)).collect();
        format!("mixed: {a} wins on {}; loses on {}", wins.join(", "), losses.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{Accuracy, BackgroundLoad, DecodeCell, LoadClass, NeedleResult, PrefillCell};

    /// A run measured on a quiet box: `quiet(0.0)`. `quiet(n)` is the same run with `n` other
    /// requests in flight throughout - card #14's whole subject.
    fn quiet(foreign: f64) -> Option<BackgroundLoad> {
        Some(BackgroundLoad {
            samples: 150,
            polls_with_traffic: if foreign >= 0.5 { 150 } else { 0 },
            concurrent_avg: Some(foreign),
            concurrent_max: Some(foreign),
            source: crate::bench::LOAD_SRC_USERS.into(),
            queue_avg: 0.0,
            queue_max: 0.0,
            engine_tok_s: Some(400.0),
        })
    }

    fn bench(profile: &str, c1: f64, c8_total: f64, prefill8k: f64) -> Scorecard {
        Scorecard {
            profile: profile.into(),
            status: "ok".into(),
            ended_at: 5_000,
            background: quiet(0.0),
            decode: vec![
                DecodeCell { concurrency: 1, context: 0, per_user_tok_s: c1, total_tok_s: c1, ttft_ms: Some(140.0), ..Default::default() },
                DecodeCell { concurrency: 8, context: 0, per_user_tok_s: c8_total / 8.0, total_tok_s: c8_total, ttft_ms: Some(400.0), ..Default::default() },
                DecodeCell { concurrency: 1, context: 16_384, per_user_tok_s: c1 * 0.9, total_tok_s: c1 * 0.9, ..Default::default() },
            ],
            prefill: vec![PrefillCell { tokens: 8_198, tok_s: prefill8k, ttft_ms: 1400.0 }, PrefillCell { tokens: 65_542, tok_s: (prefill8k * 1.1).round(), ttft_ms: 9800.0 }],
            ..Default::default()
        }
    }

    fn card(id: &str, model: &str, current: bool, last_seen: i64, quick: Option<Scorecard>) -> LoadoutCard {
        LoadoutCard { row: LoadoutRow { id: id.into(), model: model.into(), image_tag: "tag".into(), current, last_seen, c1_tok_s: Some(180.0), peak_tok_s: Some(300.0), peak_at_running: Some(2), tokens_per_joule: Some(0.40), wh_per_mtok: Some(690.0), ..Default::default() }, quick, ..Default::default() }
    }

    /// #38 residual, found by the verifier: `estimate_headroom` is what the COLLECTOR publishes
    /// (`loadouts.rs:181` -> `card.headroom` -> `/status`, `/loadouts`) and what `lss` prints
    /// (report.rs, ui/pages.rs) - and it still called the un-vetoed `saturation()` after the fix
    /// to the ADVICE path landed, so the HEADROOM box on every screen still said "no room" while
    /// the table right under it, on the same screen, said "not enough traffic". Rows from the
    /// verifier's own re-check: 900@1=200, 900@2=205, 900@3=203, 40@6=400, peak 400.
    #[test]
    fn estimate_headroom_never_disagrees_with_the_table_under_it() {
        let curve = vec![
            crate::loadout::CurveRow { running: 1, samples: 900, tok_s: Some(200.0), per_request_tok_s: Some(200.0), source: "live".into(), ..Default::default() },
            crate::loadout::CurveRow { running: 2, samples: 900, tok_s: Some(205.0), per_request_tok_s: Some(102.5), source: "live".into(), ..Default::default() },
            crate::loadout::CurveRow { running: 3, samples: 900, tok_s: Some(203.0), per_request_tok_s: Some(67.7), source: "live".into(), ..Default::default() },
            crate::loadout::CurveRow { running: 6, samples: 40, tok_s: Some(400.0), per_request_tok_s: Some(66.7), source: "live".into(), ..Default::default() },
        ];
        let mut c = card("id", "m", true, 5_000, None);
        c.row.curve = curve;
        c.row.peak_tok_s = Some(400.0);
        c.row.peak_at_running = Some(6);
        c.row.slots = 8;
        // the qualified levels alone (1, 2, 3: all >= 120 samples and a real share) would say
        // "stops growing after 1" - but level 6 at 400 tok/s vetoes it, and so does the peak
        let unvetoed = crate::loadout::saturation(&crate::loadout::curve_points(&c.row.curve));
        assert!(matches!(unvetoed, crate::loadout::Saturation::After { users: 1, .. }), "{unvetoed:?}");
        let h = c.estimate_headroom(0.0);
        assert_ne!(h.limited_by, "speed", "{}", h.sentence);
        assert!(!h.sentence.contains("no room"), "{}", h.sentence);
        let speed = h.ceilings.iter().find(|x| x.kind == "speed").unwrap();
        assert_eq!(speed.users, None, "{}", speed.why);
    }

    #[test]
    fn deltas_and_the_three_percent_noise_band() {
        assert_eq!(judge(Some(103.0), Some(100.0), true), (Some(3.0), Mark::Same), "exactly 3 % is still noise");
        assert_eq!(judge(Some(103.1), Some(100.0), true), (Some(3.1), Mark::Better));
        assert_eq!(judge(Some(96.9), Some(100.0), true), (Some(-3.1), Mark::Worse));
        assert_eq!(judge(Some(97.0), Some(100.0), true), (Some(-3.0), Mark::Same));
        // lower is better (time to first word, energy): a bigger number is a loss
        assert_eq!(judge(Some(150.0), Some(100.0), false), (Some(50.0), Mark::Worse));
        assert_eq!(judge(Some(80.0), Some(100.0), false), (Some(-20.0), Mark::Better));
        assert_eq!((judge(None, Some(1.0), true).1, judge(Some(f64::NAN), Some(1.0), true).1), (Mark::NoData, Mark::NoData));
        // the other side was zero: there is no percentage, but there is a direction
        assert_eq!((judge(Some(1.0), Some(0.0), true), judge(Some(1.0), Some(0.0), false), judge(Some(0.0), Some(0.0), false)), ((None, Mark::Better), (None, Mark::Worse), (Some(0.0), Mark::Same)));
    }

    #[test]
    fn selectors_current_previous_best_id_and_model_prefix() {
        let cards = vec![
            card("aaaa11112222", "model-a", true, 900, Some(bench("quick", 190.0, 700.0, 5800.0))),
            card("bbbb33334444", "model-b", false, 800, Some(bench("quick", 150.0, 900.0, 4000.0))),
            card("aaaa99990000", "zeta-7b", false, 500, None),
        ];
        assert_eq!(resolve("current", &cards).unwrap().row.id, "aaaa11112222");
        assert_eq!(resolve("previous", &cards).unwrap().row.id, "bbbb33334444", "the newest one that is not serving now");
        assert_eq!(resolve("best", &cards).unwrap().row.model, "model-b", "best = the highest measured total speed");
        assert_eq!(resolve("bbbb", &cards).unwrap().row.id, "bbbb33334444");
        assert!(resolve("aaaa", &cards).unwrap_err().contains("matches 2 loadout ids"));
        assert_eq!(resolve("Zeta", &cards).unwrap().row.id, "aaaa99990000", "a model name prefix, any case");
        assert_eq!(resolve("mod", &cards).unwrap().row.id, "aaaa11112222", "a model-name prefix of model-a");
        assert!(resolve("llama", &cards).unwrap_err().contains("no loadout matches"));
        assert!(resolve("current", &[]).unwrap_err().contains("no loadout on record"));
        assert!(resolve("previous", &cards[..1]).unwrap_err().contains("only one"));
        let idle: Vec<LoadoutCard> = cards.iter().cloned().map(|mut c| { c.row.current = false; c }).collect();
        assert!(resolve("current", &idle).unwrap_err().contains("nothing is serving"));
    }

    #[test]
    fn side_by_side_with_verdict_lines() {
        let a = card("aaaa11112222", "model-a", true, 900, Some(bench("quick", 190.0, 700.0, 5800.0)));
        let b = card("bbbb33334444", "model-b", false, 800, Some(bench("quick", 150.0, 900.0, 5750.0)));
        let c = compare("current", &a, "previous", &b);
        let row = |label: &str| c.rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("{label}"));
        assert_eq!((row("one user alone").a, row("one user alone").b, row("one user alone").delta_pct, row("one user alone").mark), (Some(190.0), Some(150.0), Some(26.7), Mark::Better));
        assert_eq!((row("best total speed").delta_pct, row("best total speed").mark), (Some(-22.2), Mark::Worse));
        assert_eq!(row("each user, 8 at once").mark, Mark::Worse);
        assert_eq!((row("reading an 8k prompt").delta_pct, row("reading an 8k prompt").mark), (Some(0.9), Mark::Same), "0.9 % is noise");
        assert_eq!(row("time to first word").mark, Mark::Same);
        assert!(c.rows.iter().filter(|r| r.mark != Mark::NoData).all(|r| r.source == "bench" || r.source == "live"));
        let v = |cat: &str| c.verdicts.iter().find(|v| v.category == cat).unwrap().sentence.clone();
        assert_eq!(v("speed_alone"), "current is faster than previous (one user alone 27%)");
        assert_eq!(v("speed_under_load"), "current is slower than previous (best total speed 22%)");
        assert_eq!(v("reading_speed"), "~ same: every difference is within the +-3% run-to-run noise");
        assert_eq!(v("long_context"), "current is better than previous (one user, 16k already read 27%)");
        assert!(v("accuracy").contains("lss bench accuracy"), "{}", v("accuracy"));
        // efficiency: neither has a full bench, both have live numbers, and they are identical
        assert_eq!((row("tokens per joule").source.as_str(), row("tokens per joule").mark), ("live", Mark::Same));
        assert_eq!(c.verdicts.len(), CATEGORIES.len());
        assert_eq!((c.a.model.as_str(), c.b.selector.as_str(), c.noise_pct), ("model-a", "previous", 3.0));
    }

    /// card #14. The scorecard may now be measured with other traffic on the box - that is the
    /// only way it ever gets measured at all on a server that is also the fleet's only serve. The
    /// price is that two runs taken under different traffic must NOT be subtracted from each
    /// other: the difference would be the traffic as much as the loadout. Numbers stay, deltas go,
    /// and the reason is said out loud.
    #[test]
    fn runs_under_different_background_load_are_not_compared() {
        let mut quiet_run = bench("quick", 190.0, 700.0, 5800.0);
        quiet_run.background = quiet(0.0);
        let mut loaded_run = bench("quick", 150.0, 900.0, 5750.0);
        loaded_run.background = quiet(2.0);
        loaded_run.under_load = true;
        assert_eq!((quiet_run.load_class(), loaded_run.load_class()), (LoadClass::Quiet, LoadClass::Loaded));

        let a = card("aaaa11112222", "model-a", true, 900, Some(quiet_run.clone()));
        let b = card("bbbb33334444", "model-b", false, 800, Some(loaded_run.clone()));
        let c = compare("current", &a, "previous", &b);
        let why = c.load_warning.expect("quiet vs 2 other users in flight is not comparable");
        assert!(why.contains("different background load") && why.contains("0.00") && why.contains("2.00"), "{why}");
        let row = |label: &str| c.rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("{label}"));
        // the NUMBERS are still there - withholding them would hide a measurement that was made
        assert_eq!((row("one user alone").a, row("one user alone").b), (Some(190.0), Some(150.0)));
        // the SUBTRACTION is not
        assert_eq!((row("one user alone").delta_pct, row("one user alone").mark), (None, Mark::Incomparable));
        assert!(c.rows.iter().filter(|r| r.source == "bench").all(|r| r.mark == Mark::Incomparable && r.delta_pct.is_none()));
        // a row measured from real traffic never claimed to be taken alone: it is untouched
        let tpj = row("tokens per joule");
        assert_eq!((tpj.source.as_str(), tpj.mark), ("live", Mark::Same));
        let v = |cat: &str| c.verdicts.iter().find(|v| v.category == cat).unwrap().sentence.clone();
        assert!(v("speed_alone").starts_with("cannot be compared:"), "{}", v("speed_alone"));
        assert!(!v("speed_alone").contains("faster"), "no verdict may survive the load gap: {}", v("speed_alone"));
        assert_eq!(c.a.bench_load, LoadClass::Quiet);
        assert_eq!(c.b.bench_load, LoadClass::Loaded);

        // the rule's own boundary: it is derived from the +-3 % noise band, so a load difference
        // small enough that one run cannot have had more than 3 % more of the engine IS compared
        let mut nearly = quiet_run.clone();
        nearly.background = quiet(0.02);
        let near = card("cccc55556666", "model-c", false, 700, Some(nearly));
        assert!(compare("current", &a, "near", &near).load_warning.is_none(), "0.02 of one extra user is inside the noise band");
        let mut over = quiet_run.clone();
        over.background = quiet(0.05);
        let over = card("dddd77778888", "model-d", false, 600, Some(over));
        assert!(compare("current", &a, "over", &over).load_warning.is_some(), "0.05 is outside it");

        // a run that never recorded its background load is comparable with NOTHING: an
        // unmeasured difference is not a small one
        let mut legacy = quiet_run.clone();
        legacy.background = None;
        let legacy = card("eeee99990000", "model-e", false, 500, Some(legacy));
        let c = compare("current", &a, "old", &legacy);
        assert!(c.load_warning.as_deref().is_some_and(|w| w.contains("did not record")), "{:?}", c.load_warning);
        assert_eq!(c.b.bench_load, LoadClass::Unknown);
    }

    #[test]
    fn a_bench_number_is_never_compared_with_a_live_one() {
        let benched = card("aaaa11112222", "glm", true, 900, Some(bench("quick", 190.0, 700.0, 5800.0)));
        let passive = card("bbbb33334444", "model-b", false, 800, None);
        let c = compare("current", &benched, "previous", &passive);
        let alone = c.rows.iter().find(|r| r.label == "one user alone").unwrap();
        assert_eq!((alone.source.as_str(), alone.a, alone.b, alone.mark), ("live", Some(180.0), Some(180.0), Mark::Same), "both sides fall back to their passive C1 probe");
        let prefill = c.rows.iter().find(|r| r.label == "reading a 64k prompt").unwrap();
        assert_eq!((prefill.a, prefill.b, prefill.mark, prefill.delta_pct), (Some(6380.0), None, Mark::NoData, None), "shown, not judged");
        assert!(c.verdicts.iter().find(|v| v.category == "long_context").unwrap().sentence.contains("lss bench full"));
    }

    #[test]
    fn accuracy_needle_and_mixed_results() {
        let mut a = card("aaaa11112222", "glm", true, 900, None);
        let mut b = card("bbbb33334444", "model-b", false, 800, None);
        let acc = |score: f64| Scorecard { profile: "accuracy".into(), status: "ok".into(), accuracy: vec![Accuracy { dataset: "gsm8k".into(), score, n: 1319, correct: (score * 1319.0) as u64, ..Default::default() }], ..Default::default() };
        a.accuracy = vec![acc(0.945)];
        b.accuracy = vec![acc(0.868)];
        let needle = |passes: usize| (0..3).map(|i| NeedleResult { depth_pct: [10, 50, 90][i], pass: i < passes, ..Default::default() }).collect::<Vec<_>>();
        a.full = Some(Scorecard { needle: needle(3), prefill: vec![PrefillCell { tokens: 131_080, tok_s: 5000.0, ttft_ms: 26_000.0 }], ..bench("full", 190.0, 700.0, 5800.0) });
        b.full = Some(Scorecard { needle: needle(1), prefill: vec![PrefillCell { tokens: 131_080, tok_s: 6000.0, ttft_ms: 21_000.0 }], ..bench("full", 190.0, 700.0, 5800.0) });
        let c = compare("model-a", &a, "model-b", &b);
        let v = |cat: &str| c.verdicts.iter().find(|v| v.category == cat).unwrap().sentence.clone();
        assert_eq!(v("accuracy"), "model-a is more accurate than model-b (gsm8k 9%)");
        assert_eq!(v("long_context"), "mixed: model-a wins on needle found (of 3 depths) 200%; loses on reading a 128k prompt 17%");
        assert_eq!(a.headline().accuracy, Some(0.945));
        assert_eq!((a.speed().map(|s| s.profile.as_str()), card("x", "m", false, 1, None).headline().c1_tok_s), (Some("full"), Some(180.0)), "full wins over quick; no bench = the passive numbers");
    }
}
