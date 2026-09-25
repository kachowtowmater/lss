//! Card #75: time-of-use electricity pricing, and turning the GPU watt samples the collector
//! already stores every 5 s into dollars. Pure logic in here - no clock, no config-file I/O -
//! same discipline as `rules.rs`: every function that needs "now" or "local time" takes it as an
//! argument, so a plan with a real 2.2x swing between peak and off-peak (a real utility's
//! time-of-use tariff - the real numbers live in the OWNER's own config, never here) can be
//! pinned by hand in a test rather than trusted by eye.
//!
//! An AVERAGE rate is a lie on a time-of-use plan: computing "kWh this hour x one flat number"
//! would be wrong by more than 2x during the 4-9pm summer peak on the plan this was built
//! against. Every watt-sample must be priced at the period that applies AT ITS OWN TIMESTAMP.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One named price window: WHEN it applies (season, weekday/weekend-or-holiday, local hour
/// range `[start_hour, end_hour)`) and what it costs. Multiple rows can share a name (e.g. a
/// TOU plan's "off-peak" is usually two disjoint hour ranges in a day) - this is deliberately a
/// flat list of simple rows, not a richer range type, so it maps directly onto a TOML table a
/// non-programmer can read and edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Period {
    pub name: String,
    pub usd_per_kwh: f64,
    /// "summer" | "winter" | "any"
    pub season: String,
    /// "weekday" | "weekend_or_holiday" | "any"
    pub day_kind: String,
    /// local hour, 0-23, inclusive
    pub start_hour: u32,
    /// local hour, 1-24, exclusive (24 = through midnight)
    pub end_hour: u32,
}

impl Period {
    fn matches(&self, ctx: DayContext) -> bool {
        let season_ok = match self.season.as_str() {
            "summer" => ctx.is_summer,
            "winter" => !ctx.is_summer,
            _ => true,
        };
        let day_ok = match self.day_kind.as_str() {
            "weekday" => !ctx.is_weekend_or_holiday,
            "weekend_or_holiday" => ctx.is_weekend_or_holiday,
            _ => true,
        };
        season_ok && day_ok && ctx.hour >= self.start_hour && ctx.hour < self.end_hour
    }
}

/// A day, reduced to exactly what a period needs to know: pure, no clock, no timezone. Build
/// one from a real timestamp with `day_context_at` (uses the box's own system-local time, same
/// convention as `timeutil::fmt_local`/`local_midnight` - there is no separate timezone setting
/// in this project, so the collector's own clock must be set to the meter's location), or by
/// hand in a test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayContext {
    pub hour: u32,
    pub is_summer: bool,
    pub is_weekend_or_holiday: bool,
}

/// Either a single flat number (the honest, common case when someone does not know or does not
/// want to configure a real time-of-use schedule - labelled `flat` wherever it is shown, never
/// silently treated as if it were the real plan) or a full time-of-use table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RatePlan {
    Flat { usd_per_kwh: f64 },
    TimeOfUse {
        /// summer = [summer_start_month, summer_end_month) — e.g. 6, 10 for Jun 1 -> Oct 1
        summer_start_month: u32,
        summer_end_month: u32,
        /// explicit calendar dates ("YYYY-MM-DD"), not computed - a utility's holiday schedule
        /// is its own short, yearly-refreshed list, not worth date-arithmetic and its edge cases
        holidays: Vec<String>,
        periods: Vec<Period>,
    },
}

/// The whole rate table, as loaded from `~/.config/lss/rates.toml` (or wherever `[rates]
/// path` in the collector's own config points) - see `config::RatesFile`, the on-disk shape;
/// this is the same data, holidays already parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct RateTable {
    pub name: String,
    /// the tariff/plan's own effective date, e.g. "2026-01-01" - shown on every cost figure so
    /// a stale table is visible, not silently trusted forever
    pub effective_date: String,
    pub plan: RatePlan,
    /// A utility's fixed daily charge. Shown separately, NEVER folded into a marginal
    /// per-hour or per-window cost figure - a live $/hour number must never pretend to be a bill.
    pub fixed_usd_per_day: Option<f64>,
    /// an amount that MAY belong on top of `usd_per_kwh` but is not settled (card #75: a real
    /// tariff can list extra per-kWh line items on top of the headline rate, while the utility's
    /// own consumer-facing page publishes delivery+generation only - unverified against an
    /// actual bill). Never applied; shown as a stated uncertainty wherever a cost figure is shown.
    pub unresolved_usd_per_kwh: Option<f64>,
    /// card #298: where the rate came from, in a few words - `CA avg (EIA 2026-06)`, `entered by
    /// hand (2026-09-24)` - written by `lss-collector cost-setup`, shown by lss after "rate:".
    /// `None` for a hand-written file that does not say (nothing is invented in its place).
    pub source: Option<String>,
    holidays: Vec<(i32, u32, u32)>, // (year, month, day), parsed once at construction
}

fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
    let mut parts = s.trim().splitn(3, '-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    (1..=12).contains(&m).then_some((y, m, d))?;
    (1..=31).contains(&d).then_some((y, m, d))
}

impl RateTable {
    pub fn new(name: String, effective_date: String, plan: RatePlan, fixed_usd_per_day: Option<f64>, unresolved_usd_per_kwh: Option<f64>) -> Self {
        let holidays = match &plan {
            RatePlan::TimeOfUse { holidays, .. } => holidays.iter().filter_map(|s| parse_ymd(s)).collect(),
            RatePlan::Flat { .. } => Vec::new(),
        };
        Self { name, effective_date, plan, fixed_usd_per_day, unresolved_usd_per_kwh, source: None, holidays }
    }

    /// card #298: the same table, labelled with where its rate came from (blank = unlabelled).
    pub fn with_source(mut self, source: Option<String>) -> Self {
        self.source = source.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self
    }

    pub fn is_flat(&self) -> bool {
        matches!(self.plan, RatePlan::Flat { .. })
    }

    /// The period (or the flat rate) that applies for `ctx`. `None` only for a malformed
    /// time-of-use table missing an hour somewhere - never guessed, a gap is left unpriced.
    pub fn price_for(&self, ctx: DayContext) -> Option<(f64, &str)> {
        match &self.plan {
            RatePlan::Flat { usd_per_kwh } => Some((*usd_per_kwh, "flat")),
            RatePlan::TimeOfUse { periods, .. } => periods.iter().find(|p| p.matches(ctx)).map(|p| (p.usd_per_kwh, p.name.as_str())),
        }
    }

    fn is_holiday(&self, y: i32, m: u32, d: u32) -> bool {
        self.holidays.contains(&(y, m, d))
    }
}

/// The clock-dependent entry point: `ts` interpreted in the box's own system-local time (see
/// `DayContext`'s own doc comment for the timezone caveat).
pub fn day_context_at(table: &RateTable, ts: i64) -> Option<DayContext> {
    use chrono::{Datelike, Local, TimeZone, Timelike, Weekday};
    let dt = Local.timestamp_opt(ts, 0).single()?;
    let (summer_start, summer_end) = match &table.plan {
        RatePlan::TimeOfUse { summer_start_month, summer_end_month, .. } => (*summer_start_month, *summer_end_month),
        RatePlan::Flat { .. } => (0, 0), // unused: Flat::price_for ignores DayContext entirely
    };
    let month = dt.month();
    let is_summer = if summer_end > summer_start { (summer_start..summer_end).contains(&month) } else { true };
    let is_weekend = matches!(dt.weekday(), Weekday::Sat | Weekday::Sun);
    let is_holiday = table.is_holiday(dt.year(), month, dt.day());
    Some(DayContext { hour: dt.hour(), is_summer, is_weekend_or_holiday: is_weekend || is_holiday })
}

/// The price at a real timestamp - `day_context_at` then `price_for`, the two combined.
pub fn usd_per_kwh_at(table: &RateTable, ts: i64) -> Option<(f64, String)> {
    let ctx = day_context_at(table, ts)?;
    table.price_for(ctx).map(|(price, name)| (price, name.to_string()))
}

/// A no-data gap this wide or wider between two consecutive samples is never bridged (a collector
/// restart, a stretch with no GPU reading): that interval is simply left out of the total, the
/// same "a gap is a gap, not a guess" rule `History::counter_window` already uses for tokens.
pub const MAX_STEP_SECS: i64 = 30;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostBreakdown {
    pub kwh: f64,
    pub usd: f64,
    /// period name -> (kWh, USD) - so a screen can show the peak/off-peak split, not just a total
    pub by_period: BTreeMap<String, (f64, f64)>,
    /// samples actually priced (for a UI to say how much of the window it could account for)
    pub intervals_priced: u64,
    /// consecutive-sample gaps skipped (a restart, a stretch with no GPU reading)
    pub intervals_skipped: u64,
    /// #102, 2026-09-22 (verifier): the EARLIEST timestamp actually priced into `kwh`/`usd` -
    /// `None` when nothing was priced at all. A window figure ("today", "last 24h") is only as
    /// old as this, never the window's own nominal start - a collector that has only been up
    /// 35 minutes must not let a screen show "$0.02 (since local midnight)" as if it covered the
    /// whole elapsed day. The caller compares this against the window's own requested start to
    /// say what fraction is real coverage, not guess at one.
    pub earliest_ts: Option<i64>,
}

/// Trapezoid-integrates a `(timestamp, total watts)` series (ascending) into dollars, pricing
/// EACH interval at the rate that applies at the interval's own start - this is the whole reason
/// this is a function and not `total_kwh * one_average_rate`. A sample with no reading is simply
/// absent from `samples`; the caller filters that, not this function - it only ever sees
/// (ts, watts) pairs it can price.
pub fn accumulate(table: &RateTable, samples: &[(i64, f64)]) -> CostBreakdown {
    let mut out = CostBreakdown::default();
    for pair in samples.windows(2) {
        let (t0, w0) = pair[0];
        let (t1, w1) = pair[1];
        let dt = t1 - t0;
        if dt <= 0 || dt > MAX_STEP_SECS {
            out.intervals_skipped += 1;
            continue;
        }
        let Some((price, name)) = usd_per_kwh_at(table, t0) else {
            out.intervals_skipped += 1;
            continue;
        };
        let avg_w = (w0 + w1) / 2.0;
        let step_kwh = avg_w * dt as f64 / 3_600_000.0; // W * s -> Wh -> kWh
        let step_usd = step_kwh * price;
        out.kwh += step_kwh;
        out.usd += step_usd;
        let e = out.by_period.entry(name).or_insert((0.0, 0.0));
        e.0 += step_kwh;
        e.1 += step_usd;
        out.intervals_priced += 1;
        // `samples` is ascending (this function's own doc comment) - the first successfully
        // priced interval's t0 is the earliest one there will ever be in this pass.
        out.earliest_ts.get_or_insert(t0);
    }
    out
}

/// card #174 (the owner: "the cost per 1m token is off"): like `accumulate`, but each sample also
/// carries whether the engine was IDLE at that instant (nothing running), so the caller can
/// separate "the box drawing power doing nothing" from "the box doing work" - folding standby
/// into a per-token price makes a quiet day look expensive and a busy day look cheap. Each
/// interval is attributed by its STARTING sample's state, the same convention `accumulate`
/// already uses for pricing (an interval is priced/attributed by its own start, never its end).
/// Returns (work, idle) - `work.kwh + idle.kwh` equals what a plain `accumulate` over the same
/// watts would have produced; nothing is lost, only split.
pub fn accumulate_split(table: &RateTable, samples: &[(i64, f64, bool)]) -> (CostBreakdown, CostBreakdown) {
    let mut work = CostBreakdown::default();
    let mut idle = CostBreakdown::default();
    for pair in samples.windows(2) {
        let (t0, w0, idle0) = pair[0];
        let (t1, w1, _) = pair[1];
        let dt = t1 - t0;
        if dt <= 0 || dt > MAX_STEP_SECS {
            (if idle0 { &mut idle } else { &mut work }).intervals_skipped += 1;
            continue;
        }
        let Some((price, name)) = usd_per_kwh_at(table, t0) else {
            (if idle0 { &mut idle } else { &mut work }).intervals_skipped += 1;
            continue;
        };
        let avg_w = (w0 + w1) / 2.0;
        let step_kwh = avg_w * dt as f64 / 3_600_000.0;
        let step_usd = step_kwh * price;
        let out = if idle0 { &mut idle } else { &mut work };
        out.kwh += step_kwh;
        out.usd += step_usd;
        let e = out.by_period.entry(name).or_insert((0.0, 0.0));
        e.0 += step_kwh;
        e.1 += step_usd;
        out.intervals_priced += 1;
        out.earliest_ts.get_or_insert(t0);
    }
    (work, idle)
}

/// Like `accumulate`, but each input is ALREADY an energy figure for a fixed bucket (a stored
/// rollup row's `sum_energy_j`, converted to kWh) rather than an instantaneous watt reading to
/// trapezoid-integrate - card #73's USERS section needs a ROLLING 24h dollar figure (to line up
/// with the same rolling-24h window the per-user token table already uses), and raw 5s watt
/// samples are not retained that far back (`raw_hours` default 24, but a rolling window can cross
/// the edge of what is still stored at any given moment) - the 1-minute rollup's own energy sum
/// per bucket IS retained that long (`retention_days`, default 14). Each bucket is priced at ITS
/// OWN start timestamp, same rule as `accumulate`; a bucket whose timestamp matches no period is
/// skipped, never guessed at. No gap check here (unlike `accumulate`): a stored rollup row IS the
/// energy that occurred in that exact bucket, never an interpolation between two points, so there
/// is nothing to bridge and nothing to double-count.
pub fn accumulate_bucketed_kwh(table: &RateTable, buckets: &[(i64, f64)]) -> CostBreakdown {
    let mut out = CostBreakdown::default();
    for &(ts, kwh) in buckets {
        let Some((price, name)) = usd_per_kwh_at(table, ts) else {
            out.intervals_skipped += 1;
            continue;
        };
        let usd = kwh * price;
        out.kwh += kwh;
        out.usd += usd;
        let e = out.by_period.entry(name).or_insert((0.0, 0.0));
        e.0 += kwh;
        e.1 += usd;
        out.intervals_priced += 1;
        // `buckets` is ascending (`db.rollup_rows` orders by ts) - same argument as `accumulate`.
        out.earliest_ts.get_or_insert(ts);
    }
    out
}

/// The on-disk shape of `~/.config/lss/rates.toml` (path set by `[rates] path` in the
/// collector's own config; "" = cost tracking off, the same "unset = nothing shown" convention
/// as the old flat-only `electricity_usd_per_kwh`). Deliberately its own small file, not a
/// section of `collector.toml`: the OWNER's real numbers (kept outside the repo, and git-ignored
/// wherever they land) are the single most personal thing this project ever asks for - utility, plan, home address's
/// climate zone by implication - and keeping them in a file of their own makes "never commit
/// this" a property of ONE path, not a section inside a file that also holds ordinary settings.
#[derive(Debug, Clone, Deserialize)]
pub struct RatesFile {
    pub name: String,
    pub effective_date: String,
    pub fixed_usd_per_day: Option<f64>,
    pub unresolved_usd_per_kwh: Option<f64>,
    pub plan: RatePlan,
    /// card #298: provenance, written by `lss-collector cost-setup` (all optional - a file
    /// written by hand without them parses exactly as before). `source` is the short label lss
    /// shows; `source_detail`/`source_url` are for the person reading the file.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_detail: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
}

pub fn parse_rates_file(text: &str) -> Result<RateTable, String> {
    let f: RatesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(RateTable::new(f.name, f.effective_date, f.plan, f.fixed_usd_per_day, f.unresolved_usd_per_kwh).with_source(f.source))
}

/// The figure the whole card exists for: dollars per million tokens, from an energy cost and a
/// token count over the SAME window. `None` when there is nothing to divide by - never a
/// fabricated 0 or an infinite number silently shown.
pub fn usd_per_million_tokens(usd: f64, tokens: f64) -> Option<f64> {
    (tokens > 0.0).then(|| usd / tokens * 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tou_shaped_table() -> RateTable {
        // #75, 2026-09-22: the SHAPE of a real time-of-use plan, with round, obviously-fake
        // numbers (the real numbers are the OWNER's own, in config, never in this repo) - the
        // same 6 period-kinds a real utility's TOU table has: summer on/mid/off-peak (split by
        // weekday vs weekend/holiday), winter mid/super-off/off-peak (same all week).
        let periods = vec![
            Period { name: "summer_on_peak".into(), usd_per_kwh: 0.70, season: "summer".into(), day_kind: "weekday".into(), start_hour: 16, end_hour: 21 },
            Period { name: "summer_off_peak".into(), usd_per_kwh: 0.20, season: "summer".into(), day_kind: "weekday".into(), start_hour: 0, end_hour: 16 },
            Period { name: "summer_off_peak".into(), usd_per_kwh: 0.20, season: "summer".into(), day_kind: "weekday".into(), start_hour: 21, end_hour: 24 },
            Period { name: "summer_mid_peak".into(), usd_per_kwh: 0.35, season: "summer".into(), day_kind: "weekend_or_holiday".into(), start_hour: 16, end_hour: 21 },
            Period { name: "summer_off_peak".into(), usd_per_kwh: 0.20, season: "summer".into(), day_kind: "weekend_or_holiday".into(), start_hour: 0, end_hour: 16 },
            Period { name: "summer_off_peak".into(), usd_per_kwh: 0.20, season: "summer".into(), day_kind: "weekend_or_holiday".into(), start_hour: 21, end_hour: 24 },
            Period { name: "winter_mid_peak".into(), usd_per_kwh: 0.45, season: "winter".into(), day_kind: "any".into(), start_hour: 16, end_hour: 21 },
            Period { name: "winter_super_off_peak".into(), usd_per_kwh: 0.15, season: "winter".into(), day_kind: "any".into(), start_hour: 8, end_hour: 16 },
            Period { name: "winter_off_peak".into(), usd_per_kwh: 0.15, season: "winter".into(), day_kind: "any".into(), start_hour: 21, end_hour: 24 },
            Period { name: "winter_off_peak".into(), usd_per_kwh: 0.15, season: "winter".into(), day_kind: "any".into(), start_hour: 0, end_hour: 8 },
        ];
        let plan = RatePlan::TimeOfUse { summer_start_month: 6, summer_end_month: 10, holidays: vec!["2026-07-04".into()], periods };
        // card #129: these three scalars (base charge, unresolved per-kWh, effective date) were
        // verbatim the owner's real tariff values, even though the period PRICES right above are
        // already properly fuzzed - a number leaks nothing a word-list scan can catch, so it sat
        // undetected in a tracked file. Round, made-up values only; the tests need internal
        // consistency, never the real ones.
        RateTable::new("test utility TOU (fake numbers)".into(), "2026-01-01".into(), plan, Some(0.50), Some(0.0100))
    }

    #[test]
    fn summer_weekday_peaks_at_4pm_and_drops_at_9pm() {
        let t = tou_shaped_table();
        let ctx = DayContext { hour: 15, is_summer: true, is_weekend_or_holiday: false };
        assert_eq!(t.price_for(ctx), Some((0.20, "summer_off_peak")));
        let ctx = DayContext { hour: 16, is_summer: true, is_weekend_or_holiday: false };
        assert_eq!(t.price_for(ctx), Some((0.70, "summer_on_peak")), "4pm is the boundary itself: on-peak starts HERE");
        let ctx = DayContext { hour: 20, is_summer: true, is_weekend_or_holiday: false };
        assert_eq!(t.price_for(ctx), Some((0.70, "summer_on_peak")));
        let ctx = DayContext { hour: 21, is_summer: true, is_weekend_or_holiday: false };
        assert_eq!(t.price_for(ctx), Some((0.20, "summer_off_peak")), "9pm: back to off-peak");
    }

    #[test]
    fn summer_weekend_is_mid_peak_not_on_peak_at_the_same_hour() {
        let t = tou_shaped_table();
        let ctx = DayContext { hour: 17, is_summer: true, is_weekend_or_holiday: true };
        assert_eq!(t.price_for(ctx), Some((0.35, "summer_mid_peak")), "same hour, weekend: mid-peak, never on-peak");
    }

    #[test]
    fn winter_has_no_on_peak_at_all_and_a_super_off_peak_window() {
        let t = tou_shaped_table();
        assert_eq!(t.price_for(DayContext { hour: 17, is_summer: false, is_weekend_or_holiday: false }), Some((0.45, "winter_mid_peak")));
        assert_eq!(t.price_for(DayContext { hour: 10, is_summer: false, is_weekend_or_holiday: false }), Some((0.15, "winter_super_off_peak")));
        assert_eq!(t.price_for(DayContext { hour: 22, is_summer: false, is_weekend_or_holiday: true }), Some((0.15, "winter_off_peak")), "winter ignores weekday/weekend entirely");
    }

    #[test]
    fn a_holiday_prices_like_a_weekend_even_on_a_weekday() {
        let t = tou_shaped_table();
        // 2026-07-04 is a Saturday this year anyway; the point is day_context_at must mark it a
        // holiday via the table, not rely on it happening to also be a weekend
        let ctx = day_context_at(&t, chrono_ts(2026, 7, 4, 17, 0)).unwrap();
        assert!(ctx.is_weekend_or_holiday);
        assert_eq!(t.price_for(ctx), Some((0.35, "summer_mid_peak")));
    }

    #[test]
    fn flat_rate_ignores_the_day_entirely_and_is_always_labelled_flat() {
        let t = RateTable::new("flat".into(), "2026-01-01".into(), RatePlan::Flat { usd_per_kwh: 0.30 }, None, None);
        assert_eq!(t.price_for(DayContext { hour: 3, is_summer: false, is_weekend_or_holiday: false }), Some((0.30, "flat")));
        assert_eq!(t.price_for(DayContext { hour: 18, is_summer: true, is_weekend_or_holiday: true }), Some((0.30, "flat")));
        assert!(t.is_flat());
    }

    /// #75 item 5, the pinned arithmetic: a known watt-hours input against a known rate table,
    /// crossing the 4pm summer weekday boundary, checkable by hand. 25 s steps (real collector
    /// samples are 5 s apart; 25 s is still within `MAX_STEP_SECS`, just bigger for a rounder
    /// example), 1200 W flat (4 GPUs at 300 W each, a plausible real reading):
    ///   15:59:35 -> 16:00:00  (priced at the 15:59:35 START: still off-peak)
    ///     1200 W x 25 s = 30,000 Ws = 0.0083333 kWh  x $0.20  = $0.00167
    ///   16:00:00 -> 16:00:25  (priced at the 16:00:00 START: the boundary itself, on-peak)
    ///     1200 W x 25 s = 30,000 Ws = 0.0083333 kWh  x $0.70  = $0.0058
    /// total kWh = 0.0166667, total usd = $0.00725
    #[test]
    fn a_watt_hour_series_crossing_4pm_prices_each_step_at_its_own_start() {
        let t = tou_shaped_table();
        let base = chrono_ts(2026, 7, 20, 15, 59) + 35; // a Monday - summer weekday, 15:59:35
        let samples = [(base, 1200.0), (base + 25, 1200.0), (base + 50, 1200.0)];
        let c = accumulate(&t, &samples);
        let step_kwh = 1200.0 * 25.0 / 3_600_000.0;
        assert!((c.kwh - 2.0 * step_kwh).abs() < 1e-9, "{}", c.kwh);
        assert!((c.usd - (step_kwh * 0.20 + step_kwh * 0.70)).abs() < 1e-9, "{}", c.usd);
        assert_eq!(c.intervals_priced, 2);
        assert_eq!(c.intervals_skipped, 0);
        let (off_kwh, off_usd) = c.by_period["summer_off_peak"];
        assert!((off_kwh - step_kwh).abs() < 1e-9 && (off_usd - step_kwh * 0.20).abs() < 1e-9, "{off_kwh} {off_usd}");
        let (on_kwh, on_usd) = c.by_period["summer_on_peak"];
        assert!((on_kwh - step_kwh).abs() < 1e-9 && (on_usd - step_kwh * 0.70).abs() < 1e-9, "{on_kwh} {on_usd}");
    }

    /// card #73: the USERS section's rolling-24h dollar figure prices stored ROLLUP buckets, not
    /// raw watt samples - each bucket is ALREADY an energy figure (no trapezoid step between two
    /// readings), so this pins the arithmetic the same way the boundary test above does for
    /// `accumulate`, just on the other function.
    #[test]
    fn accumulate_bucketed_kwh_prices_each_bucket_at_its_own_start_no_trapezoid_involved() {
        let t = tou_shaped_table();
        let off_ts = chrono_ts(2026, 7, 20, 15, 59); // Monday, summer weekday: off-peak
        let on_ts = chrono_ts(2026, 7, 20, 16, 5); // same day, now on-peak
        let c = accumulate_bucketed_kwh(&t, &[(off_ts, 0.5), (on_ts, 0.3)]);
        assert!((c.kwh - 0.8).abs() < 1e-9, "{}", c.kwh);
        assert!((c.usd - (0.5 * 0.20 + 0.3 * 0.70)).abs() < 1e-9, "{}", c.usd);
        assert_eq!((c.intervals_priced, c.intervals_skipped), (2, 0));
        assert!((c.by_period["summer_off_peak"].0 - 0.5).abs() < 1e-9);
        assert!((c.by_period["summer_on_peak"].0 - 0.3).abs() < 1e-9);
    }

    /// a malformed table (no matching period for some hour) must skip that bucket's kWh, not
    /// guess a price for it or silently drop it from `intervals_skipped`'s accounting.
    #[test]
    fn accumulate_bucketed_kwh_skips_a_bucket_no_period_covers() {
        let mut t = tou_shaped_table();
        let RatePlan::TimeOfUse { periods, .. } = &mut t.plan else { unreachable!() };
        periods.retain(|p| p.name != "summer_on_peak"); // leaves a 16:00-21:00 weekday hole
        let on_ts = chrono_ts(2026, 7, 20, 16, 5);
        let c = accumulate_bucketed_kwh(&t, &[(on_ts, 0.3)]);
        assert_eq!((c.kwh, c.usd, c.intervals_priced, c.intervals_skipped), (0.0, 0.0, 0, 1));
    }

    /// card #174 (the owner: "the cost per 1m token is off"), item 4's own arithmetic pin: a known
    /// mix of two idle steps and two working steps must split into exactly the work/idle kWh and
    /// usd the hand arithmetic says, at the SAME summer off-peak rate for both so only the split
    /// itself is under test, not the pricing `accumulate`'s own boundary test already covers.
    #[test]
    fn accumulate_split_separates_idle_watts_from_working_watts() {
        let t = tou_shaped_table();
        let base = chrono_ts(2026, 7, 20, 10, 0); // summer weekday off-peak throughout
        // idle: two 5 s steps at 200 W (the box drawing standby power, nothing running), THEN
        // work: one 5 s step at 1200 W (actively serving). THREE intervals, not two: attribution
        // is by each interval's OWN starting sample (the same rule `accumulate` already uses for
        // pricing), so the boundary interval - (base+5, still idle) -> (base+10, now working) -
        // is ALSO idle, because its t0 sample was.
        let samples = [
            (base, 200.0, true),
            (base + 5, 200.0, true),
            (base + 10, 1200.0, false),
            (base + 15, 1200.0, false),
        ];
        let (work, idle) = accumulate_split(&t, &samples);
        // idle: step 1 (200W -> 200W) + step 2 (200W -> 1200W, t0 idle) = its own two averages
        let idle_kwh = (200.0 * 5.0 + (200.0 + 1200.0) / 2.0 * 5.0) / 3_600_000.0;
        // work: step 3 (1200W -> 1200W, t0 working) only
        let work_kwh = 1200.0 * 5.0 / 3_600_000.0;
        assert!((idle.kwh - idle_kwh).abs() < 1e-9, "{}", idle.kwh);
        assert!((idle.usd - idle_kwh * 0.20).abs() < 1e-9, "{}", idle.usd);
        assert_eq!(idle.intervals_priced, 2);
        assert!((work.kwh - work_kwh).abs() < 1e-9, "{}", work.kwh);
        assert!((work.usd - work_kwh * 0.20).abs() < 1e-9, "{}", work.usd);
        assert_eq!(work.intervals_priced, 1);
        // nothing lost: work + idle equals what a plain accumulate() over the same watts gives
        let plain = accumulate(&t, &samples.iter().map(|&(ts, w, _)| (ts, w)).collect::<Vec<_>>());
        assert!((work.kwh + idle.kwh - plain.kwh).abs() < 1e-9);
        assert!((work.usd + idle.usd - plain.usd).abs() < 1e-9);
    }

    #[test]
    fn a_gap_wider_than_max_step_is_skipped_not_bridged() {
        let t = tou_shaped_table();
        let base = chrono_ts(2026, 7, 20, 10, 0);
        let samples = [(base, 1000.0), (base + 5, 1000.0), (base + 3_600, 1000.0)]; // a 1h hole
        let c = accumulate(&t, &samples);
        assert_eq!((c.intervals_priced, c.intervals_skipped), (1, 1));
    }

    #[test]
    fn usd_per_million_tokens_is_none_with_nothing_to_divide_by() {
        assert_eq!(usd_per_million_tokens(1.0, 0.0), None);
        assert_eq!(usd_per_million_tokens(2.0, 1_000_000.0), Some(2.0));
        assert!((usd_per_million_tokens(0.5, 500_000.0).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_malformed_holiday_string_is_dropped_not_panicked_on() {
        let plan = RatePlan::TimeOfUse { summer_start_month: 6, summer_end_month: 10, holidays: vec!["not-a-date".into(), "2026-13-40".into()], periods: vec![] };
        let t = RateTable::new("x".into(), "x".into(), plan, None, None);
        assert!(!t.is_holiday(2026, 1, 1));
    }

    #[test]
    fn parses_a_flat_rates_toml() {
        let text = "name = \"flat\"\neffective_date = \"2026-01-01\"\n\n[plan]\nkind = \"flat\"\nusd_per_kwh = 0.30\n";
        let t = parse_rates_file(text).unwrap();
        assert!(t.is_flat());
        assert_eq!(t.price_for(DayContext { hour: 12, is_summer: true, is_weekend_or_holiday: false }), Some((0.30, "flat")));
    }

    #[test]
    fn parses_a_time_of_use_rates_toml_with_the_real_file_shape() {
        // the same shape packaging/rates.toml.example ships. card #172 (verifier-2): these used
        // to be "obviously fake" only in the comment - the numbers were the real plan, truncated
        // one rounding step, and every privacy net reported clean because none of them looked at
        // NUMBERS. Invented outright now, chosen so no 2-3-significant-figure form of any value
        // here collides with the real plan's own values either.
        let text = r#"
name = "Example Utility TOU"
effective_date = "2026-01-01"
fixed_usd_per_day = 1.10
unresolved_usd_per_kwh = 0.0060

[plan]
kind = "time_of_use"
summer_start_month = 6
summer_end_month = 10
holidays = ["2026-01-01", "2026-07-04"]

[[plan.periods]]
name = "summer_on_peak"
usd_per_kwh = 0.65
season = "summer"
day_kind = "weekday"
start_hour = 16
end_hour = 21

[[plan.periods]]
name = "summer_off_peak"
usd_per_kwh = 0.20
season = "summer"
day_kind = "weekday"
start_hour = 0
end_hour = 16
"#;
        let t = parse_rates_file(text).unwrap();
        assert!(!t.is_flat());
        assert_eq!(t.fixed_usd_per_day, Some(1.10));
        assert_eq!(t.unresolved_usd_per_kwh, Some(0.0060));
        assert_eq!(t.price_for(DayContext { hour: 17, is_summer: true, is_weekend_or_holiday: false }), Some((0.65, "summer_on_peak")));
        let ctx = day_context_at(&t, chrono_ts(2026, 7, 4, 12, 0)).unwrap();
        assert!(ctx.is_weekend_or_holiday, "the configured holiday must be honoured, not just the calendar weekend");
    }

    #[test]
    fn packaging_rates_example_actually_parses() {
        // packaging/rates.toml.example's active (uncommented) option must parse cleanly - the
        // same "the shipped example must actually work" discipline config.rs's own test applies
        // to collector.toml.example
        let text = include_str!("../../../packaging/rates.toml.example");
        let t = parse_rates_file(text).expect("packaging/rates.toml.example must parse");
        assert!(t.is_flat(), "OPTION 1 (flat) is the active, uncommented example");
        assert_eq!(t.fixed_usd_per_day, Some(0.50));
    }

    #[test]
    fn garbage_toml_is_an_error_not_a_panic() {
        assert!(parse_rates_file("not valid toml {{{").is_err());
        assert!(parse_rates_file("").is_err(), "a required field missing is an error, not a silently-defaulted table");
    }

    /// Local time, seconds since epoch - a small helper so the tests read as calendar dates,
    /// not raw unix numbers. Deliberately NOT `pub`: production code goes through `day_context_at`.
    fn chrono_ts(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        use chrono::{Local, TimeZone};
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().expect("a real local date/time").timestamp()
    }
}

// ------------------------------------------------------------- card #176: spending over time

/// One local day's spend. `covered_secs` is how much of that day was actually priced, so a
/// screen can say "3 days of data", not "this month".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaySpend {
    /// local calendar date, `YYYY-MM-DD` - the unit the owner asked in ("every day")
    pub date: String,
    /// local midnight of that day, so a chart can place it without re-parsing the date
    pub ts: i64,
    pub usd: f64,
    pub kwh: f64,
    pub covered_secs: i64,
}

/// A spending window: the money, and how much of the window it actually covers.
///
/// card #102's rule applied to money, which is item 1 of card #176 in his own words: "a month
/// that is 3 days old says so rather than rendering a third of a month as if it were a month".
/// `usd`/`kwh` are `None` when nothing in the window could be priced - never 0.0, which would
/// read as "we spent nothing".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpendWindow {
    pub usd: Option<f64>,
    pub kwh: Option<f64>,
    /// seconds in this window that were actually priced
    pub covered_secs: i64,
    /// how long the window is MEANT to be (a month-to-date on the 3rd is 3 days, not 30)
    pub nominal_secs: i64,
}

impl SpendWindow {
    /// Sum the days that fall in `[from, to)`. Days are local-midnight keyed.
    fn of(days: &[DaySpend], from: i64, to: i64, nominal_secs: i64) -> Self {
        let mut usd = 0.0;
        let mut kwh = 0.0;
        let mut covered = 0;
        let mut any = false;
        for d in days.iter().filter(|d| d.ts >= from && d.ts < to) {
            usd += d.usd;
            kwh += d.kwh;
            covered += d.covered_secs;
            any = true;
        }
        Self {
            usd: any.then_some(usd),
            kwh: any.then_some(kwh),
            covered_secs: covered,
            nominal_secs,
        }
    }

    /// True when the window has at least `frac` of its nominal length priced. A screen uses this
    /// to decide whether a figure is worth presenting without a caveat.
    pub fn is_well_covered(&self, frac: f64) -> bool {
        self.nominal_secs > 0 && (self.covered_secs as f64) >= self.nominal_secs as f64 * frac
    }
}

/// Everything card #176 asks for, assembled from the DAILY series. Pure: the collector reads the
/// rollup and prices it, this decides what the windows and the projection are allowed to say.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpendingStatus {
    pub today: SpendWindow,
    pub yesterday: SpendWindow,
    /// since local midnight of the current week's Monday
    pub this_week: SpendWindow,
    pub last_7d: SpendWindow,
    /// since local midnight of the 1st of the current month
    pub this_month: SpendWindow,
    pub last_30d: SpendWindow,
    /// card #226 (the owner: "for electric u need to add month a year to date"): since local
    /// midnight of January 1st. Its `nominal_secs` is the time since Jan 1, and `covered_secs`
    /// what was actually priced - the stored history (the 10-minute rollup) does not reach back
    /// a year, so for most of the year this window is honestly partial, never padded.
    pub this_year: SpendWindow,
    /// the first local date inside `this_year` that has any priced energy (`YYYY-MM-DD`), so a
    /// partial year can say "since <date>"; `None` when nothing this year was priced
    pub year_first_day: Option<String>,
    /// oldest first, one entry per local day that had any priced energy (item 2's chart input)
    pub daily: Vec<DaySpend>,
    /// item 3: only Some when the month-so-far is long enough AND covered enough to support it
    pub month_projection_usd: Option<f64>,
    /// why there is no projection, when there is none - never silence
    pub projection_note: String,
}

/// Item 3's rule, and the reason it is a named constant: extrapolating a 3-day month to 30 days
/// is exactly the "plausible, confidently wrong number" this whole feature has been careful to
/// avoid. A projection needs a week of the month elapsed and most of it actually priced.
pub const PROJECTION_MIN_DAYS: i64 = 7;
pub const PROJECTION_MIN_COVERAGE: f64 = 0.8;

/// Assemble the windows and the projection. `day_starts` gives the local midnights this function
/// cannot compute itself (the caller owns the timezone): `(today, yesterday, week_start,
/// month_start, days_in_month, elapsed_days_in_month)`.
#[allow(clippy::too_many_arguments)]
pub fn spending(
    days: &[DaySpend],
    now: i64,
    today_start: i64,
    yesterday_start: i64,
    week_start: i64,
    month_start: i64,
    days_in_month: i64,
    elapsed_days_in_month: i64,
    year_start: i64,
) -> SpendingStatus {
    let day = 86_400;
    let today = SpendWindow::of(days, today_start, today_start + day, (now - today_start).max(0));
    let yesterday = SpendWindow::of(days, yesterday_start, today_start, day);
    let this_week = SpendWindow::of(days, week_start, now + day, (now - week_start).max(0));
    let last_7d = SpendWindow::of(days, today_start - 6 * day, now + day, 7 * day);
    let this_month = SpendWindow::of(days, month_start, now + day, (now - month_start).max(0));
    let last_30d = SpendWindow::of(days, today_start - 29 * day, now + day, 30 * day);
    // card #226: year to date - the same summing rule as every other window, so a year the
    // history does not reach back through reads as covered < nominal, never as a full year
    let this_year = SpendWindow::of(days, year_start, now + day, (now - year_start).max(0));
    let year_first_day = days.iter().filter(|d| d.ts >= year_start && d.ts < now + day).map(|d| (d.ts, d.date.clone())).min().map(|(_, date)| date);

    // item 3: a projection or an explanation, never a silent extrapolation
    let (month_projection_usd, projection_note) = match this_month.usd {
        _ if elapsed_days_in_month < PROJECTION_MIN_DAYS => (
            None,
            format!(
                "too early to project: {} of {} days into the month (needs {})",
                elapsed_days_in_month, days_in_month, PROJECTION_MIN_DAYS
            ),
        ),
        _ if !this_month.is_well_covered(PROJECTION_MIN_COVERAGE) => (
            None,
            format!(
                "not enough data to project: only {} of the {} days so far were priced",
                fmt_days(this_month.covered_secs),
                elapsed_days_in_month
            ),
        ),
        Some(usd) if elapsed_days_in_month > 0 => (
            Some(usd / elapsed_days_in_month as f64 * days_in_month as f64),
            format!(
                "projected from {} days at this rate",
                elapsed_days_in_month
            ),
        ),
        _ => (None, "nothing priced this month yet".to_string()),
    };

    SpendingStatus {
        today,
        yesterday,
        this_week,
        last_7d,
        this_month,
        last_30d,
        this_year,
        year_first_day,
        daily: days.to_vec(),
        month_projection_usd,
        projection_note,
    }
}

/// card #181: the same words the spending block uses, for the readings that project it.
pub fn fmt_days_pub(secs: i64) -> String {
    fmt_days(secs)
}

fn fmt_days(secs: i64) -> String {
    let d = secs as f64 / 86_400.0;
    if d < 1.0 {
        format!("{:.1} days", d)
    } else {
        format!("{:.0} days", d)
    }
}

#[cfg(test)]
mod spending_tests {
    use super::*;

    fn day(ts: i64, date: &str, usd: f64) -> DaySpend {
        DaySpend { date: date.into(), ts, usd, kwh: usd * 4.0, covered_secs: 86_400 }
    }

    /// A clean month: the 10th, nine full days behind it.
    fn nine_days() -> (Vec<DaySpend>, i64, i64) {
        let month_start = 1_000_000 - (1_000_000 % 86_400); // pretend this is the 1st
        let today_start = month_start + 9 * 86_400;
        let days: Vec<DaySpend> = (0..10)
            .map(|i| day(month_start + i * 86_400, "2026-09-0x", 2.0))
            .collect();
        (days, month_start, today_start)
    }

    #[test]
    fn windows_sum_only_the_days_inside_them() {
        let (days, month_start, today_start) = nine_days();
        let now = today_start + 3600;
        let s = spending(&days, now, today_start, today_start - 86_400, today_start - 2 * 86_400, month_start, 30, 10, 0);
        assert_eq!(s.today.usd, Some(2.0));
        assert_eq!(s.yesterday.usd, Some(2.0));
        assert_eq!(s.last_7d.usd, Some(14.0), "7 days x $2");
        assert_eq!(s.this_month.usd, Some(20.0), "all ten days");
        assert_eq!(s.last_30d.usd, Some(20.0), "only ten days exist");
        assert_eq!(s.daily.len(), 10, "the chart gets every day");
    }

    /// card #226: the year-to-date window across a YEAR BOUNDARY - December's days are in the
    /// history but not in this year, and must not leak into it.
    #[test]
    fn year_to_date_counts_only_days_since_january_1st() {
        let jan1 = 1_800_000_000 - (1_800_000_000 % 86_400); // pretend this is local Jan 1
        let days = vec![
            day(jan1 - 2 * 86_400, "2025-12-30", 5.0),
            day(jan1 - 86_400, "2025-12-31", 5.0),
            day(jan1, "2026-01-01", 1.0),
            day(jan1 + 86_400, "2026-01-02", 2.0),
        ];
        let today_start = jan1 + 86_400;
        let now = today_start + 3600;
        let s = spending(&days, now, today_start, jan1, jan1 - 3 * 86_400, jan1, 31, 2, jan1);
        assert_eq!(s.this_year.usd, Some(3.0), "Jan 1 + Jan 2 only - last year's $10 must not leak in");
        assert_eq!(s.this_year.kwh, Some(12.0));
        assert_eq!(s.this_year.nominal_secs, now - jan1);
        assert_eq!(s.year_first_day.as_deref(), Some("2026-01-01"));
        assert!(s.this_year.is_well_covered(0.95), "two full days out of 1d1h: covered");
    }

    /// card #226: history that starts months after Jan 1 (the 10-minute rollup keeps ~90 days)
    /// is a PARTIAL year - covered far below nominal, and the first priced day named - never a
    /// figure that reads as the whole year.
    #[test]
    fn year_to_date_with_history_starting_mid_year_is_partial_and_names_its_first_day() {
        let jan1 = 1_800_000_000 - (1_800_000_000 % 86_400);
        let first = jan1 + 200 * 86_400;
        let days: Vec<DaySpend> = (0..30).map(|i| day(first + i * 86_400, if i == 0 { "2026-07-20" } else { "2026-07-2x" }, 2.0)).collect();
        let today_start = first + 29 * 86_400;
        let now = today_start + 3600;
        let s = spending(&days, now, today_start, today_start - 86_400, today_start - 3 * 86_400, today_start - 10 * 86_400, 31, 11, jan1);
        assert_eq!(s.this_year.usd, Some(60.0));
        assert_eq!(s.this_year.covered_secs, 30 * 86_400);
        assert!(s.this_year.nominal_secs > 220 * 86_400, "the window is still since Jan 1");
        assert!(!s.this_year.is_well_covered(0.95), "30 days of a 229-day year is a floor, not a total");
        assert_eq!(s.year_first_day.as_deref(), Some("2026-07-20"));
    }

    /// card #226: nothing priced this year = None and no first day, never $0.00.
    #[test]
    fn year_to_date_with_nothing_priced_is_none_not_zero() {
        let s = spending(&[], 1_000_000, 900_000, 813_600, 813_600, 900_000, 30, 2, 500_000);
        assert_eq!((s.this_year.usd, s.this_year.kwh, s.year_first_day.clone()), (None, None, None));
        assert!(s.this_year.nominal_secs > 0);
    }

    #[test]
    fn a_window_with_nothing_priced_is_none_not_zero() {
        // "we spent nothing" and "we could not price it" are different facts; only one is true
        let s = spending(&[], 1_000_000, 900_000, 813_600, 813_600, 900_000, 30, 2, 0);
        assert_eq!(s.today.usd, None);
        assert_eq!(s.this_month.usd, None);
        assert_eq!(s.today.kwh, None);
    }

    #[test]
    fn a_three_day_old_month_refuses_to_project_and_says_why() {
        // card #176 item 3, in the owner's own framing: never extrapolate 3 days into a month
        let month_start = 1_000_000 - (1_000_000 % 86_400);
        let today_start = month_start + 2 * 86_400;
        let days: Vec<DaySpend> = (0..3).map(|i| day(month_start + i * 86_400, "d", 2.0)).collect();
        let s = spending(&days, today_start + 3600, today_start, today_start - 86_400, today_start, month_start, 30, 3, 0);
        assert_eq!(s.month_projection_usd, None);
        assert!(s.projection_note.contains("too early to project"), "{}", s.projection_note);
        assert!(s.projection_note.contains("3 of 30 days"), "{}", s.projection_note);
        // ...and the month figure itself is still SHOWN - it is a true statement about a short
        // month, not a bad statement about a long one. Note what coverage does and does not
        // mean here, because my own first draft of this test got it backwards: nominal_secs for
        // a month-to-date is the ELAPSED time, so three fully-priced days ARE well covered -
        // the question "is this a whole month?" is answered by the elapsed-days rule in the
        // projection, not by coverage.
        assert_eq!(s.this_month.usd, Some(6.0));
        assert!(s.this_month.is_well_covered(0.8), "three priced days cover three elapsed days");
        assert!(s.this_month.nominal_secs < 4 * 86_400, "a 3-day-old month is not 30 days long");
    }

    #[test]
    fn a_projection_appears_once_the_month_is_long_and_covered_enough() {
        let (days, month_start, today_start) = nine_days();
        let s = spending(&days, today_start + 3600, today_start, today_start - 86_400, today_start, month_start, 30, 10, 0);
        // $20 over 10 days -> $2/day -> $60 for a 30-day month
        let p = s.month_projection_usd.expect("10 covered days supports a projection");
        assert!((p - 60.0).abs() < 0.01, "{p}");
        assert!(s.projection_note.contains("projected from 10 days"), "{}", s.projection_note);
    }

    #[test]
    fn a_long_but_barely_covered_month_still_refuses() {
        // 20 days elapsed, but only 4 days of them were ever priced (a collector that was down):
        // long enough by the calendar, nowhere near enough by the data.
        let month_start = 1_000_000 - (1_000_000 % 86_400);
        let today_start = month_start + 19 * 86_400;
        let days: Vec<DaySpend> = (0..4).map(|i| day(month_start + i * 86_400, "d", 2.0)).collect();
        let s = spending(&days, today_start + 3600, today_start, today_start - 86_400, today_start, month_start, 30, 20, 0);
        assert_eq!(s.month_projection_usd, None, "20 calendar days but 4 priced must not project");
        assert!(s.projection_note.contains("not enough data"), "{}", s.projection_note);
        assert!(s.projection_note.contains("4 days"), "it must say how much it had: {}", s.projection_note);
    }
}
