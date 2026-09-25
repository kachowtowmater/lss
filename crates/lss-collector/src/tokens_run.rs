//! Card #73's TOKENS section: generated/prompt/cached tokens at hour/day/week/month, from the
//! collector's own rollup tables.
//!
//! #100, 2026-09-22 (verifier): `hour` used to come from `work_1h` (raw 5s samples, no DB call),
//! a DIFFERENT source than day/week/month (the rollup) - on a collector with anything less than
//! a full day of uptime, that let `hour` read HIGHER than `day` on screen (the rollup lagging a
//! sample buffer that runs current-to-now is not a small effect, it can be most of the window).
//! `hour` is now queried the SAME way as every other window, from the SAME source, over the SAME
//! kind of range - the four windows are then monotonic BY CONSTRUCTION (a longer window over the
//! same rollup can only ever sum everything a shorter one did, plus more), not by a patched-on
//! clamp that would hide the real number. The cost: `hour` can lag up to ~60s behind the raw
//! samples (the CURRENT, still-open 1-minute bucket is not in the rollup yet) - imperceptible on
//! an hour-wide figure, and a small price for a number that is never a confident lie.
//!
//! `tok_gen` / `tok_prompt` / `tok_cached` / `sum_requests` are already rolled up per sample
//! (`series::sample_points`) as SUM-kind metrics, at both 1-minute and 10-minute resolution -
//! this file only reads them back over a window, it adds no new metric and no new source.

use crate::db::Db;
use lss_core::model::WorkWindow;
use lss_core::series::{RES_10M, RES_1M};

/// `None` = no rollup for `tok_gen` stored at this resolution AT ALL yet (an old collector, or
/// one that has not been running long enough to have written even one bucket) - never a
/// fabricated 0 standing in for "nothing recorded". Once ANY history exists, this sums exactly
/// what is stored for the requested window - never guessed, never padded to a "complete" window.
///
/// #102, 2026-09-22 (verifier): also returns how many of the window's own seconds are ACTUALLY
/// covered by stored data - "now minus the later of (the window's own start, the earliest
/// `tok_gen` bucket ever stored)". A collector up for 35 minutes must not let "day"/"week" both
/// read as if they held a full day/week just because the query itself always spans that far -
/// the caller compares this against the window's nominal length to say so on screen.
fn window(db: &Db, res: i64, now: i64, from: i64) -> (Option<WorkWindow>, Option<i64>) {
    let Some(first_ts) = db.rollup_first_ts(res, "tok_gen").ok().flatten() else { return (None, None) };
    let sum = |metric: &str| db.rollup_sum(res, metric, from, now + 1).unwrap_or(0.0);
    let w = WorkWindow { prompt: sum("tok_prompt"), cached: sum("tok_cached"), generated: sum("tok_gen"), requests: sum("sum_requests") };
    let covered_secs = (now - first_ts.max(from)).max(0);
    (Some(w), Some(covered_secs))
}

pub struct Windows {
    pub hour: Option<WorkWindow>,
    pub hour_covered_secs: Option<i64>,
    pub day: Option<WorkWindow>,
    pub day_covered_secs: Option<i64>,
    pub week: Option<WorkWindow>,
    pub week_covered_secs: Option<i64>,
    pub month: Option<WorkWindow>,
    pub month_covered_secs: Option<i64>,
}

/// hour/day/week all use the 1-minute rollup (`retention_days`, default 14 - comfortably covers
/// all three); month uses the 10-minute rollup (`rollup_10m_days`, default 90). All four share
/// the SAME `to` (`now + 1`) and a strictly earlier `from` as the window widens, so - given the
/// same source - each is a superset of the one before it: hour <= day <= week always holds
/// (month uses a coarser rollup, so it is compared separately, never assumed >= week by the same
/// bucket-level argument, though in practice it always is too since it is a wider real range).
pub fn compute(db: &Db, now: i64) -> Windows {
    let (hour, hour_covered_secs) = window(db, RES_1M, now, now - 3_600);
    let (day, day_covered_secs) = window(db, RES_1M, now, now - 86_400);
    let (week, week_covered_secs) = window(db, RES_1M, now, now - 7 * 86_400);
    let (month, month_covered_secs) = window(db, RES_10M, now, now - 30 * 86_400);
    Windows { hour, hour_covered_secs, day, day_covered_secs, week, week_covered_secs, month, month_covered_secs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::series::{Agg, RollupBatch};

    fn put(db: &Db, res: i64, ts: i64, metric: &str, v: f64) {
        db.write_rollup(&RollupBatch { res, ts, rows: vec![(metric.to_string(), Agg::from_row(v, v, v, 1.0))] }, true).unwrap();
    }

    #[test]
    fn day_and_week_sum_the_1m_rollup_month_sums_the_10m_rollup() {
        let db = Db::memory();
        let now = 10_000_000;
        // two buckets inside the day AND the week window
        put(&db, RES_1M, now - 3_600, "tok_gen", 100.0);
        put(&db, RES_1M, now - 3_600, "tok_prompt", 40.0);
        put(&db, RES_1M, now - 3_600, "tok_cached", 10.0);
        put(&db, RES_1M, now - 3_600, "sum_requests", 5.0);
        put(&db, RES_1M, now - 2 * 86_400, "tok_gen", 200.0); // inside the week, outside the day
        put(&db, RES_1M, now - 2 * 86_400, "tok_prompt", 80.0);
        // a 10m bucket 10 days ago - inside the month, outside day and week
        put(&db, RES_10M, now - 10 * 86_400, "tok_gen", 9_000.0);
        let w = compute(&db, now);
        assert_eq!(w.day.unwrap().generated, 100.0, "the 2-day-old bucket must not count towards 'day'");
        assert_eq!(w.week.unwrap().generated, 300.0, "both buckets inside 7 days");
        assert_eq!((w.day.unwrap().prompt, w.day.unwrap().cached, w.day.unwrap().requests), (40.0, 10.0, 5.0));
        assert_eq!(w.month.unwrap().generated, 9_000.0, "only the 10-minute rollup is queried for month");
    }

    #[test]
    fn no_rollup_at_all_is_none_not_a_fabricated_zero() {
        let db = Db::memory();
        let w = compute(&db, 1_000_000);
        assert!(w.hour.is_none() && w.day.is_none() && w.week.is_none() && w.month.is_none());
    }

    /// #100 (verifier): the four windows are monotonic BY CONSTRUCTION now that `hour` reads
    /// from the same rollup as the rest - this pins that with real stored buckets spread across
    /// the whole range, never just trusting the doc comment's own argument. Every timestamp is
    /// written to BOTH resolutions, matching how a live collector's pipeline actually works
    /// (`rollup.rs`'s `Pipeline::push` feeds the SAME sample into the 1-minute AND 10-minute
    /// roller at once) - month must not be compared against synthetic, disconnected 10m data.
    #[test]
    fn hour_le_day_le_week_le_month_for_any_stored_data() {
        let db = Db::memory();
        let now = 20_000_000;
        let put_both = |ts: i64, v: f64| {
            put(&db, RES_1M, ts, "tok_gen", v);
            put(&db, RES_10M, ts - ts.rem_euclid(RES_10M), "tok_gen", v);
        };
        put_both(now - 300, 10.0); // inside hour, day, week, month
        put_both(now - 5_000, 20.0); // inside day, week, month, outside hour
        put_both(now - 2 * 86_400, 30.0); // inside week, month, outside hour, day
        put_both(now - 20 * 86_400, 40.0); // inside month only
        let w = compute(&db, now);
        let (h, d, wk, m) = (w.hour.unwrap().generated, w.day.unwrap().generated, w.week.unwrap().generated, w.month.unwrap().generated);
        assert_eq!((h, d, wk, m), (10.0, 30.0, 60.0, 100.0), "each window sums exactly the buckets inside it");
        assert!(h <= d && d <= wk && wk <= m, "hour={h} day={d} week={wk} month={m} - must stay monotonic");
    }

    /// #102 (verifier): a collector up 35 minutes must say "day" only actually covers 35
    /// minutes, not the full 86,400s the query always spans.
    #[test]
    fn a_short_lived_collector_reports_its_real_coverage_not_the_windows_nominal_length() {
        let db = Db::memory();
        let now = 30_000_000;
        put(&db, RES_1M, now - 35 * 60, "tok_gen", 5.0); // the earliest bucket that exists at all
        let w = compute(&db, now);
        assert_eq!(w.hour_covered_secs, Some(35 * 60), "hour's own 3600s window is itself wider than 35m of real data - covered = real data span");
        assert_eq!(w.day_covered_secs, Some(35 * 60), "day nominally spans 86,400s but only 35m is real");
        assert_eq!(w.week_covered_secs, Some(35 * 60));
    }

    /// once the collector has been up longer than the window itself, coverage is the window's
    /// own full nominal length, never inflated past it.
    #[test]
    fn a_long_lived_collector_reports_full_coverage_capped_at_the_windows_own_length() {
        let db = Db::memory();
        let now = 30_000_000;
        put(&db, RES_1M, now - 60 * 86_400, "tok_gen", 5.0); // 60 days of history
        let w = compute(&db, now);
        assert_eq!(w.hour_covered_secs, Some(3_600), "capped at the window's own length, not the collector's full uptime");
        assert_eq!(w.day_covered_secs, Some(86_400));
        assert_eq!(w.week_covered_secs, Some(7 * 86_400));
    }
}
