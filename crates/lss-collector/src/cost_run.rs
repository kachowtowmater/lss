//! Card #75: the collector-side glue around `lss_core::rates` - turning the GPU watt samples
//! already stored every 5 s into a `CostStatus`. All the pricing logic itself (the part worth
//! distrusting) lives in `lss_core::rates`, pure and tested there; this file is only I/O -
//! reading stored samples for a window and reusing counters the rest of the collector already
//! tracks.

use crate::db::Db;
use lss_core::model::CostStatus;
use lss_core::rates::{self, RateTable};
use lss_core::series::RES_1M;

/// `(ts, total watts)` for every stored sample in `[from, to)` that actually had a GPU reading -
/// a sample with none is simply absent from the series, never treated as a 0 W step.
fn watt_series(db: &Db, from: i64, to: i64) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let _ = db.each_sample(from, to, |s| {
        let v: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
        if !v.is_empty() {
            out.push((s.ts, v.iter().sum::<f64>()));
        }
    });
    out
}

/// card #174: like `watt_series`, but each reading also carries whether the engine was IDLE at
/// that instant (`running == 0` on the serve metrics for that same sample - nothing in flight,
/// nothing queued behind it). A sample with no GPU reading is absent, same as `watt_series`; a
/// sample with GPU watts but no serve metrics (the serve was down) counts as idle - the box was
/// still drawing power and doing no work either way.
fn watt_series_with_activity(db: &Db, from: i64, to: i64) -> Vec<(i64, f64, bool)> {
    let mut out = Vec::new();
    let _ = db.each_sample(from, to, |s| {
        let v: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
        if !v.is_empty() {
            let idle = s.metrics.as_ref().is_none_or(|m| m.running <= 0.0);
            out.push((s.ts, v.iter().sum::<f64>(), idle));
        }
    });
    out
}

/// card #73: one bucket per stored 1-minute rollup row of `sum_energy_j` in `[from, to)`,
/// converted to kWh - the input `accumulate_bucketed_kwh` wants. Reads the ROLLUP, never raw
/// samples: a rolling 24h window can reach past what `raw_hours` still retains, but the 1-minute
/// rollup (`retention_days`, default 14) comfortably covers it.
fn energy_kwh_buckets(db: &Db, from: i64, to: i64) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let _ = db.rollup_rows(RES_1M, "sum_energy_j", from, to, |ts, agg| out.push((ts, agg.sum / 3_600_000.0)));
    out
}

/// card #174: the three token counters `compute` needs, bundled so the function itself stays
/// under clippy's argument-count lint rather than growing a ninth bare parameter next time.
#[derive(Debug, Clone, Copy, Default)]
pub struct TodayTokens {
    pub generated: Option<f64>,
    pub prompt: Option<f64>,
    pub cached: Option<f64>,
}

/// `live_watts` = the LATEST sample's own total, already known to the caller (avoids a second DB
/// read just to re-derive "right now"). `today_tokens` = the SAME counter-growth-since-local-
/// midnight the rest of the collector already computes (`History::counter_window`) - reused
/// here, not re-derived, so this never disagrees with the TOKENS page about how many tokens
/// "today" actually contained.
pub fn compute(db: &Db, table: &RateTable, now: i64, midnight: i64, live_watts: Option<f64>, today_tokens: TodayTokens) -> CostStatus {
    let TodayTokens { generated: today_generated_tokens, prompt: today_prompt_tokens, cached: today_cached_tokens } = today_tokens;
    let (current_usd_per_kwh, current_period) = rates::usd_per_kwh_at(table, now).map_or((None, String::new()), |(p, n)| (Some(p), n));
    let live_usd_per_hour = live_watts.zip(current_usd_per_kwh).map(|(w, price)| w / 1000.0 * price);
    // card #174 (the owner: "the cost per 1m token is off"): split today's energy into WORK (the
    // engine had something running) and STANDBY (it did not), so the primary per-token figure
    // below is priced on work energy alone - folding standby into it made quiet days look
    // expensive and busy days look cheap.
    let (work, standby) = rates::accumulate_split(table, &watt_series_with_activity(db, midnight, now + 1));
    let today = rates::accumulate(table, &watt_series(db, midnight, now + 1));
    let priced = today.intervals_priced > 0;
    let work_priced = work.intervals_priced > 0;
    let standby_priced = standby.intervals_priced > 0;
    // uncached prefill = prompt tokens minus the ones served from cache - what the engine
    // actually had to COMPUTE while reading, the same distinction `cached_share_10m` already
    // draws on the live speed side of this page. A cache hit costs KV residency, not compute.
    let today_real_work_tokens = today_prompt_tokens.zip(today_cached_tokens).map(|(p, c)| (p - c).max(0.0)).zip(today_generated_tokens).map(|(uncached, gen)| uncached + gen);
    // card #73: the ROLLING last 24h (not "today" / since midnight above) - the same window the
    // USERS section's per-user token table already uses, priced from rollup buckets so it can
    // reach past raw_hours if it needs to.
    let last_24h = rates::accumulate_bucketed_kwh(table, &energy_kwh_buckets(db, now - 86_400, now + 1));
    let last_24h_priced = last_24h.intervals_priced > 0;
    CostStatus {
        rate_name: table.name.clone(),
        rate_source: table.source.clone().unwrap_or_default(),
        effective_date: table.effective_date.clone(),
        is_flat: table.is_flat(),
        current_usd_per_kwh,
        current_period,
        live_usd_per_hour,
        today_kwh: priced.then_some(today.kwh),
        today_usd: priced.then_some(today.usd),
        today_usd_per_million_generated_tokens: today_generated_tokens.and_then(|t| rates::usd_per_million_tokens(today.usd, t)).filter(|_| priced),
        today_usd_per_million_prompt_tokens: today_prompt_tokens.and_then(|t| rates::usd_per_million_tokens(today.usd, t)).filter(|_| priced),
        today_usd_per_million_real_work_tokens: today_real_work_tokens.and_then(|t| rates::usd_per_million_tokens(work.usd, t)).filter(|_| work_priced),
        today_standby_kwh: standby_priced.then_some(standby.kwh),
        today_standby_usd: standby_priced.then_some(standby.usd),
        fixed_usd_per_day: table.fixed_usd_per_day,
        unresolved_usd_per_kwh: table.unresolved_usd_per_kwh,
        last_24h_kwh: last_24h_priced.then_some(last_24h.kwh),
        last_24h_usd: last_24h_priced.then_some(last_24h.usd),
        // #102 (verifier): "now - earliest priced sample" - never the window's own nominal
        // length, so a collector that has only been up part of the window says so honestly.
        today_covered_secs: today.earliest_ts.map(|t| (now - t).max(0)),
        last_24h_covered_secs: last_24h.earliest_ts.map(|t| (now - t).max(0)),
    }
}

/// One local day's priced energy, for every day in `[from, now]`. Reads the **10-MINUTE**
/// rollup, which is card #176's item 4 in its own words ("the 90-day rollup is the source for
/// the long windows - do not recompute from 5s samples for a month of data"): the raw samples
/// are kept for `raw_hours` (24h) and the 1-minute rollup for `retention_days` (14), so neither
/// can answer "last 30 days" at all, while the 10-minute tier is retained for 90.
///
/// Each day is priced by `accumulate_bucketed_kwh`, so a day spanning two tariff periods is
/// split at the right prices rather than averaged - the same function the rolling-24h figure
/// uses, for the same reason.
fn daily_spend(db: &Db, table: &RateTable, from: i64, now: i64) -> Vec<rates::DaySpend> {
    let mut by_day: std::collections::BTreeMap<i64, Vec<(i64, f64)>> = std::collections::BTreeMap::new();
    let _ = db.rollup_rows(lss_core::series::RES_10M, "sum_energy_j", from, now + 1, |ts, agg| {
        by_day
            .entry(lss_core::timeutil::local_midnight(ts))
            .or_default()
            .push((ts, agg.sum / 3_600_000.0));
    });
    by_day
        .into_iter()
        .filter_map(|(day_ts, buckets)| {
            let b = rates::accumulate_bucketed_kwh(table, &buckets);
            // a day whose buckets could not be priced at all contributes nothing rather than a
            // zero - the windows above it must not read "we spent $0" for an unpriced day
            (b.intervals_priced > 0).then(|| rates::DaySpend {
                date: lss_core::timeutil::fmt_local(day_ts, "%Y-%m-%d"),
                ts: day_ts,
                usd: b.usd,
                kwh: b.kwh,
                // each priced 10-minute bucket stands for its own 600 seconds of the day
                covered_secs: (b.intervals_priced as i64) * lss_core::series::RES_10M,
            })
        })
        .collect()
}

/// The whole card-#176 block. The calendar arithmetic (week start, month start, days in month)
/// lives here because it needs the local timezone; the *judgement* - what a window may claim and
/// whether a projection is allowed - is in `lss_core::rates::spending`, pure and tested there.
pub fn spending(db: &Db, table: &RateTable, now: i64) -> rates::SpendingStatus {
    use lss_core::timeutil::local_midnight;
    let today_start = local_midnight(now);
    let chart_from = today_start - 31 * 86_400;
    // card #226: year to date reads the SAME daily series from Jan 1 (or from 31 days back, in
    // early January). The 10-minute rollup keeps ~90 days, so most of the year is simply not
    // stored - that shows up as covered < nominal on `this_year`, never as a padded figure.
    let year_from = year_start(now);
    let days = daily_spend(db, table, year_from.min(chart_from), now);
    let (month_start, days_in_month, elapsed_days) = month_bounds(now);
    let mut sp = rates::spending(
        &days,
        now,
        today_start,
        today_start - 86_400,
        week_start(now),
        month_start,
        days_in_month,
        elapsed_days,
        year_from,
    );
    // the chart (card #176 item 2) stays the last 31 days - reading further back for the year
    // must not change what `daily` means to its readers
    sp.daily.retain(|d| d.ts >= chart_from);
    sp
}

/// card #226: local midnight of January 1st of `now`'s year. Found by day-of-year, then SNAPPED
/// to a real local midnight: subtracting whole 86,400 s days lands an hour off across a DST
/// change, so step to noon of that day and take its local midnight.
fn year_start(now: i64) -> i64 {
    let day_of_year: i64 = lss_core::timeutil::fmt_local(now, "%j").parse().unwrap_or(1);
    let approx = lss_core::timeutil::local_midnight(now) - (day_of_year - 1).max(0) * 86_400;
    lss_core::timeutil::local_midnight(approx + 12 * 3_600)
}

/// Local midnight of the current week's Monday.
fn week_start(now: i64) -> i64 {
    let midnight = lss_core::timeutil::local_midnight(now);
    // %u is ISO weekday, 1 = Monday
    let dow: i64 = lss_core::timeutil::fmt_local(midnight, "%u").parse().unwrap_or(1);
    midnight - (dow - 1).max(0) * 86_400
}

/// `(local midnight of the 1st, days in this month, days elapsed INCLUDING today)`.
fn month_bounds(now: i64) -> (i64, i64, i64) {
    let day_of_month: i64 = lss_core::timeutil::fmt_local(now, "%d").parse().unwrap_or(1);
    let month_start = lss_core::timeutil::local_midnight(now) - (day_of_month - 1).max(0) * 86_400;
    // walk forward to the 1st of the next month rather than carrying a length table (and its
    // leap-year special case): the formatter already knows the calendar
    let mut probe = month_start + 28 * 86_400;
    let this_month = lss_core::timeutil::fmt_local(month_start, "%Y-%m");
    while lss_core::timeutil::fmt_local(probe, "%Y-%m") == this_month {
        probe += 86_400;
    }
    let days_in_month = (probe - month_start) / 86_400;
    (month_start, days_in_month, day_of_month)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// card #226: Jan 1 at local midnight, whatever day of the year it is asked on.
    #[test]
    fn year_start_is_local_midnight_of_january_first() {
        for now in [1_767_225_600 + 3_600, 1_780_000_000, 1_798_761_599] {
            let y = year_start(now);
            assert_eq!(lss_core::timeutil::fmt_local(y, "%m-%d %H:%M"), "01-01 00:00", "asked at {now}");
            assert_eq!(lss_core::timeutil::fmt_local(y, "%Y"), lss_core::timeutil::fmt_local(now, "%Y"));
            assert!(y <= now);
        }
    }
    use crate::db::Db;
    use lss_core::gpu::GpuSample;
    use lss_core::model::Sample;
    use lss_core::rates::{RatePlan, RateTable};

    fn flat_table() -> RateTable {
        // card #129: the previous test values sat one rounding step from the owner's real daily
        // charge and unresolved per-kWh adder - round, unambiguous values instead; nothing here
        // needs to be traceable to a bill. card #172: and this comment no longer SPELLS the real
        // pair it warns about, which is how the numeric privacy net found it.
        RateTable::new("flat".into(), "2026-01-01".into(), RatePlan::Flat { usd_per_kwh: 0.30 }, Some(0.50), Some(0.0100))
    }

    fn sample_with_watts(ts: i64, watts: f64) -> Sample {
        Sample { ts, serve_up: true, gpus_ok: true, gpus: vec![GpuSample { index: 0, power_w: Some(watts), ..Default::default() }], ..Default::default() }
    }

    /// card #174: `running` is what `watt_series_with_activity` reads to classify a sample as
    /// work (`running > 0`) or standby (`running == 0`, or no metrics at all - the serve down).
    fn sample_with_watts_and_running(ts: i64, watts: f64, running: f64) -> Sample {
        Sample { metrics: Some(lss_core::prom::ServeMetrics { running, ..Default::default() }), ..sample_with_watts(ts, watts) }
    }

    #[test]
    fn live_and_today_come_from_the_stored_samples_at_a_flat_rate() {
        let db = Db::memory();
        let midnight = 1_000_000;
        // 3 samples, 25 s apart, 1200 W flat - two priced intervals, same arithmetic shape as
        // rates.rs's own pinned test, just via the DB path this time. All three RUNNING (work),
        // so the old today_usd/today_kwh figures (which do not care about the split) and the new
        // real-work figure land on the SAME energy.
        db.insert_sample(&sample_with_watts_and_running(midnight, 1200.0, 1.0)).unwrap();
        db.insert_sample(&sample_with_watts_and_running(midnight + 25, 1200.0, 1.0)).unwrap();
        db.insert_sample(&sample_with_watts_and_running(midnight + 50, 1200.0, 1.0)).unwrap();
        let now = midnight + 50;
        let table = flat_table();
        // prompt 500k, cached 300k -> uncached prefill 200k; generated 2M -> real-work denominator 2.2M
        let c = compute(&db, &table, now, midnight, Some(1200.0), TodayTokens { generated: Some(2_000_000.0), prompt: Some(500_000.0), cached: Some(300_000.0) });
        assert_eq!(c.rate_name, "flat");
        assert!(c.is_flat);
        assert_eq!(c.current_usd_per_kwh, Some(0.30));
        assert!((c.live_usd_per_hour.unwrap() - (1200.0 / 1000.0 * 0.30)).abs() < 1e-9);
        let expect_kwh = 2.0 * (1200.0 * 25.0 / 3_600_000.0);
        assert!((c.today_kwh.unwrap() - expect_kwh).abs() < 1e-9, "{:?}", c.today_kwh);
        assert!((c.today_usd.unwrap() - expect_kwh * 0.30).abs() < 1e-9, "{:?}", c.today_usd);
        assert!((c.today_usd_per_million_generated_tokens.unwrap() - (expect_kwh * 0.30 / 2_000_000.0 * 1_000_000.0)).abs() < 1e-6);
        assert!((c.today_usd_per_million_real_work_tokens.unwrap() - (expect_kwh * 0.30 / 2_200_000.0 * 1_000_000.0)).abs() < 1e-6, "{:?}", c.today_usd_per_million_real_work_tokens);
        // every sample was working: nothing landed in standby
        assert_eq!((c.today_standby_kwh, c.today_standby_usd), (None, None));
        assert_eq!(c.fixed_usd_per_day, Some(0.50));
        assert_eq!(c.unresolved_usd_per_kwh, Some(0.0100));
    }

    /// card #174 (the owner: "the cost per 1m token is off"): the whole point of the split - a mix
    /// of standby and working samples must divide the WORK energy (not all of it) by real-work
    /// tokens, and the standby energy must show up on its own, separately.
    #[test]
    fn standby_energy_is_excluded_from_the_real_work_headline() {
        let db = Db::memory();
        let midnight = 1_000_000;
        // idle (standby) for the first 50 s at 200 W, then working for 25 s at 1200 W. THREE
        // intervals, not two: attribution is by each interval's STARTING sample (the same rule
        // `accumulate` already uses for pricing), so the boundary interval - (midnight+25, still
        // idle) -> (midnight+50, now working) - is ALSO idle, because its t0 sample was.
        db.insert_sample(&sample_with_watts_and_running(midnight, 200.0, 0.0)).unwrap();
        db.insert_sample(&sample_with_watts_and_running(midnight + 25, 200.0, 0.0)).unwrap();
        db.insert_sample(&sample_with_watts_and_running(midnight + 50, 1200.0, 1.0)).unwrap();
        db.insert_sample(&sample_with_watts_and_running(midnight + 75, 1200.0, 1.0)).unwrap();
        let now = midnight + 75;
        let table = flat_table();
        let c = compute(&db, &table, now, midnight, Some(1200.0), TodayTokens { generated: Some(1_000_000.0), prompt: Some(1_000_000.0), cached: Some(0.0) });
        // idle: step 1 (200W -> 200W) + step 2 (200W -> 1200W, t0 idle) = its own two averages
        let idle_kwh = (200.0 * 25.0 + (200.0 + 1200.0) / 2.0 * 25.0) / 3_600_000.0;
        // work: step 3 (1200W -> 1200W, t0 working) only
        let work_kwh = 1200.0 * 25.0 / 3_600_000.0;
        assert!((c.today_standby_kwh.unwrap() - idle_kwh).abs() < 1e-9, "{:?}", c.today_standby_kwh);
        assert!((c.today_standby_usd.unwrap() - idle_kwh * 0.30).abs() < 1e-9, "{:?}", c.today_standby_usd);
        // real-work denominator: uncached prefill (1M - 0 cached) + generated (1M) = 2M
        assert!((c.today_usd_per_million_real_work_tokens.unwrap() - (work_kwh * 0.30 / 2_000_000.0 * 1_000_000.0)).abs() < 1e-6, "{:?}", c.today_usd_per_million_real_work_tokens);
        // today_kwh (the OLD, unsplit figure) still covers everything, work and standby together
        assert!((c.today_kwh.unwrap() - (idle_kwh + work_kwh)).abs() < 1e-9, "{:?}", c.today_kwh);
    }

    #[test]
    fn nothing_stored_yet_is_none_not_zero() {
        let db = Db::memory();
        let table = flat_table();
        let c = compute(&db, &table, 1_000_000, 1_000_000 - 3600, None, TodayTokens::default());
        assert_eq!((c.today_kwh, c.today_usd, c.live_usd_per_hour, c.last_24h_kwh, c.last_24h_usd), (None, None, None, None, None));
        assert_eq!((c.today_usd_per_million_real_work_tokens, c.today_standby_kwh, c.today_standby_usd), (None, None, None));
        // the rate itself is still known even with nothing to price yet - a screen can still say
        // what plan is configured and what it costs right now if the draw ever becomes known
        assert_eq!(c.current_usd_per_kwh, Some(0.30));
    }

    /// card #73: the rolling-24h figure comes from the 1-minute ROLLUP (`sum_energy_j`), not raw
    /// watt samples - this pins the DB-reading half (the pricing arithmetic itself is
    /// `rates::accumulate_bucketed_kwh`'s own pinned test).
    #[test]
    fn rolling_24h_prices_stored_rollup_buckets_not_raw_samples() {
        use lss_core::series::{Agg, RollupBatch};
        let db = Db::memory();
        let now = 2_000_000;
        let put = |ts: i64, joules: f64| db.write_rollup(&RollupBatch { res: RES_1M, ts, rows: vec![("sum_energy_j".to_string(), Agg::from_row(joules, joules, joules, 1.0))] }, true).unwrap();
        put(now - 3600, 3_600_000.0); // 1 kWh, one hour ago
        put(now - 1800, 1_800_000.0); // 0.5 kWh, half an hour ago
        let table = flat_table();
        // midnight == now: "today" is deliberately empty (no raw samples stored) - isolates this
        // test to last_24h alone, proving it does NOT depend on the raw-sample path at all
        let c = compute(&db, &table, now, now, None, TodayTokens::default());
        assert_eq!((c.today_kwh, c.today_usd), (None, None));
        assert!((c.last_24h_kwh.unwrap() - 1.5).abs() < 1e-9, "{:?}", c.last_24h_kwh);
        assert!((c.last_24h_usd.unwrap() - 1.5 * 0.30).abs() < 1e-9, "{:?}", c.last_24h_usd);
    }

    /// card #226, through the collector's REAL read path (the 10-minute rollup, like week and
    /// month): asked on Jan 3, the year to date holds Jan 1 + Jan 2 and NOT Dec 31, while the
    /// rolling 30 days still sees all three; the first priced day of the year is named.
    #[test]
    fn year_to_date_reads_the_rollup_from_january_first_across_the_boundary() {
        use lss_core::series::{Agg, RollupBatch, RES_10M};
        let db = Db::memory();
        let jan1 = year_start(1_780_000_000);
        let now = jan1 + 2 * 86_400 + 3_600; // Jan 3, 01:00 local
        let put = |ts: i64| db.write_rollup(&RollupBatch { res: RES_10M, ts, rows: vec![("sum_energy_j".to_string(), Agg::from_row(3_600_000.0, 3_600_000.0, 3_600_000.0, 1.0))] }, true).unwrap();
        put(jan1 - 86_400 + 600); // Dec 31: last year
        put(jan1 + 600); // Jan 1
        put(jan1 + 86_400 + 600); // Jan 2
        let sp = spending(&db, &flat_table(), now);
        assert!((sp.this_year.kwh.unwrap() - 2.0).abs() < 1e-9, "Dec 31 must not count toward this year: {:?}", sp.this_year);
        assert!((sp.last_30d.kwh.unwrap() - 3.0).abs() < 1e-9, "{:?}", sp.last_30d);
        assert_eq!(sp.year_first_day.as_deref(), Some(lss_core::timeutil::fmt_local(jan1, "%Y-%m-%d").as_str()));
        assert_eq!(sp.this_year.nominal_secs, now - jan1);
        // and with nothing stored at all: None, never $0.00
        let empty = spending(&Db::memory(), &flat_table(), now);
        assert_eq!((empty.this_year.usd, empty.year_first_day), (None, None));
    }
}

// ------------------------------------------------------------- card #176: spending over time
