//! Downsampling on write: every 5 s sample is folded into the 1-minute and 10-minute tiers as
//! it arrives, latency histograms are cut into windows of bucket deltas, and the gate log is
//! merged per 10 minutes. Nothing here reads a clock: the sample's own `ts` drives it.

use crate::db::Db;
use lss_core::gatelog::LogDelta;
use lss_core::hist::{ClosedWindow, HistSet, LatencyNow, LatencyTracker};
use lss_core::model::Sample;
use lss_core::series::{sample_points, Agg, Roller, RollupBatch, RES_10M, RES_1M};

pub struct Pipeline {
    m1: Roller,
    m10: Roller,
    latency: LatencyTracker,
    gate: Option<(i64, LogDelta)>,
    prev: Option<Sample>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self { m1: Roller::new(RES_1M), m10: Roller::new(RES_10M), latency: LatencyTracker::default(), gate: None, prev: None }
    }
}

fn latency_batch(w: &ClosedWindow) -> RollupBatch {
    RollupBatch { res: w.res, ts: w.ts, rows: w.points().into_iter().map(|(name, v, n)| (name, Agg::from_row(v, v, v, n))).collect() }
}

impl Pipeline {
    /// Replays samples that are already stored (after a restart: the buckets still open), so a
    /// restart in the middle of a minute loses nothing. Writes nothing for finished buckets that
    /// are already in the database (`replace = false`).
    pub fn replay(&mut self, db: &Db, samples: impl Iterator<Item = Sample>) {
        for s in samples {
            self.push(db, &s, None, false);
        }
    }

    /// One live sample. Errors are logged, never fatal: a monitor keeps monitoring.
    pub fn push(&mut self, db: &Db, sample: &Sample, hists: Option<&HistSet>, replace: bool) -> Vec<ClosedWindow> {
        let points = sample_points(self.prev.as_ref(), sample);
        for roller in [&mut self.m1, &mut self.m10] {
            if let Some(batch) = roller.add(sample.ts, &points) {
                log_err("rollup", db.write_rollup(&batch, replace));
            }
        }
        let closed = self.latency.observe(sample.ts, hists);
        for w in &closed {
            log_err("hist", db.write_hist_window(w));
            log_err("rollup", db.write_rollup(&latency_batch(w), true));
        }
        let bucket = sample.ts - sample.ts.rem_euclid(RES_10M);
        if self.gate.as_ref().is_some_and(|(t, _)| *t != bucket) {
            if let Some((t, log)) = self.gate.take() {
                log_err("gatelog", db.write_gatelog(t, &log, replace));
            }
        }
        self.gate.get_or_insert_with(|| (bucket, LogDelta::default())).1.merge(&sample.log);
        self.prev = Some(sample.clone());
        closed
    }

    /// The 10-minute gate-log bucket still in progress (the gateway tables add it on top of
    /// what is stored).
    pub fn gate_open_bucket(&self) -> Option<&(i64, LogDelta)> {
        self.gate.as_ref()
    }

    pub fn latency_now(&self) -> LatencyNow {
        self.latency.now()
    }
}

fn log_err(what: &str, r: rusqlite::Result<()>) {
    if let Err(e) = r {
        eprintln!("db: {what} write failed: {e}");
    }
}

/// One-off, for a database written by a collector from before the rollup tiers: builds them
/// from the stored samples, streamed, in transactions small enough that the live loop (which
/// writes through its own connection) never waits long. Rows the live loop already wrote win.
pub fn backfill(db: &Db, until: i64) -> rusqlite::Result<usize> {
    let mut pipe = Pipeline::default();
    let mut n = 0;
    let mut from = 0;
    loop {
        // one hour of samples per transaction
        let Some(first) = db.first_sample_at_or_after(from)? else { break };
        if first >= until {
            break;
        }
        let to = (first - first.rem_euclid(3600) + 3600).min(until);
        let mut chunk = Vec::new();
        db.each_sample(first, to, |s| chunk.push(s))?;
        db.begin()?;
        for s in &chunk {
            pipe.push(db, s, None, false);
        }
        db.commit()?;
        n += chunk.len();
        from = to;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Retention;
    use lss_core::hist::HistSnapshot;
    use lss_core::prom::ServeMetrics;

    const DAY: i64 = 86_400;
    const KEEP: Retention = Retention { raw: DAY, m1: 14 * DAY, m10: 90 * DAY };
    /// a fake clock: a round 10-minute boundary
    const T0: i64 = 1_800_000_000; // a round 10-minute boundary

    fn sample(ts: i64, decode: f64) -> Sample {
        Sample { ts, serve_up: true, metrics: Some(ServeMetrics { gen_throughput: decode, queue: (ts % 7) as f64, ..Default::default() }), ..Default::default() }
    }

    fn rows(db: &Db, res: i64, metric: &str) -> Vec<(i64, Agg)> {
        let mut out = Vec::new();
        db.rollup_rows(res, metric, 0, i64::MAX, |ts, a| out.push((ts, a))).unwrap();
        out
    }

    #[test]
    fn samples_are_downsampled_into_both_tiers_as_they_are_written() {
        let db = Db::memory();
        let mut pipe = Pipeline::default();
        // 25 minutes of 5 s samples: decode = the minute number
        for k in 0..(25 * 12) {
            let ts = T0 + k * 5;
            db.insert_sample(&sample(ts, (k / 12) as f64)).unwrap();
            pipe.push(&db, &sample(ts, (k / 12) as f64), None, true);
        }
        let m1 = rows(&db, 60, "decode_tok_s");
        assert_eq!(m1.len(), 24, "the 25th minute is still open");
        assert_eq!((m1[0].0, m1[0].1.n, m1[0].1.avg()), (T0, 12.0, 0.0));
        assert_eq!((m1[23].0, m1[23].1.avg()), (T0 + 23 * 60, 23.0));
        let m10 = rows(&db, 600, "decode_tok_s");
        assert_eq!(m10.len(), 2);
        assert_eq!((m10[0].1.n, m10[0].1.min, m10[0].1.max, m10[0].1.avg()), (120.0, 0.0, 9.0, 4.5));
        assert_eq!(rows(&db, 600, "queue")[0].1.max, 6.0);
        assert!(db.metric_names().unwrap().contains(&"serve_up".to_string()));
    }

    #[test]
    fn a_restart_replays_the_open_buckets_and_loses_nothing() {
        let db = Db::memory();
        let mut pipe = Pipeline::default();
        for k in 0..18 {
            let s = sample(T0 + k * 5, 10.0);
            db.insert_sample(&s).unwrap();
            pipe.push(&db, &s, None, true);
        }
        drop(pipe); // the collector dies 30 s into the second minute
        let mut pipe = Pipeline::default();
        let mut stored = Vec::new();
        db.each_sample(T0, i64::MAX, |s| stored.push(s)).unwrap();
        pipe.replay(&db, stored.into_iter());
        for k in 18..24 {
            pipe.push(&db, &sample(T0 + k * 5, 30.0), None, true);
        }
        pipe.push(&db, &sample(T0 + 120, 0.0), None, true);
        let m1 = rows(&db, 60, "decode_tok_s");
        assert_eq!(m1.len(), 2);
        assert_eq!((m1[1].1.n, m1[1].1.avg()), (12.0, 20.0), "six samples from before the restart, six after");
    }

    #[test]
    fn retention_is_per_tier_on_a_fake_clock() {
        let db = Db::memory();
        let now = T0 + 100 * DAY;
        let one = |res: i64, ts: i64| RollupBatch { res, ts, rows: vec![("x".into(), Agg::one(1.0))] };
        for age_days in [0, 1, 2, 13, 15, 89, 91] {
            let ts = now - age_days * DAY - 60;
            db.insert_sample(&sample(ts, 1.0)).unwrap();
            db.write_rollup(&one(60, ts), true).unwrap();
            db.write_rollup(&one(600, ts - ts % 600), true).unwrap();
            db.write_gatelog(ts - ts % 600, &LogDelta { public: lss_core::gatelog::LaneLog { requests: 1, ..Default::default() }, ..Default::default() }, true).unwrap();
        }
        assert!(db.prune(now, KEEP).unwrap() > 0);
        let mut raw = Vec::new();
        db.each_sample(0, i64::MAX, |s| raw.push((now - s.ts) / DAY)).unwrap();
        assert_eq!(raw, vec![0], "raw 5 s samples: 24 h");
        let ages = |res: i64| -> Vec<i64> { rows(&db, res, "x").iter().map(|(ts, _)| (now - ts) / DAY).rev().collect() };
        assert_eq!(ages(60), vec![0, 1, 2, 13], "1-minute rollups: 14 days");
        assert_eq!(ages(600), vec![0, 1, 2, 13, 15, 89], "10-minute rollups: 90 days");
        assert_eq!(db.gatelog_merged(0, i64::MAX).unwrap().public.requests, 6);
        assert_eq!(db.prune(now, KEEP).unwrap(), 0, "idempotent");
    }

    #[test]
    fn latency_windows_are_stored_as_bucket_deltas_and_as_percentile_series() {
        let db = Db::memory();
        let mut pipe = Pipeline::default();
        let le = vec![0.1, 0.2, 0.4, 0.8];
        let snap = |in_second_bucket: f64| -> HistSet {
            let cum = vec![0.0, in_second_bucket, in_second_bucket, in_second_bucket, in_second_bucket];
            [Some(HistSnapshot { le: le.clone(), cum, sum: in_second_bucket * 0.15, count: in_second_bucket }), None, None, None, None, None]
        };
        for k in 0..30 {
            let ts = T0 + k * 5;
            // 2 requests per poll during the first minute, none after
            let seen = (k.min(11) * 2) as f64;
            pipe.push(&db, &sample(ts, 0.0), Some(&snap(seen)), true);
        }
        let p50 = rows(&db, 60, "ttft_p50_ms");
        assert_eq!(p50.len(), 1, "idle minutes write nothing");
        assert_eq!((p50[0].0, p50[0].1.avg(), p50[0].1.n), (T0, 150.0, 22.0));
        let mut windows = Vec::new();
        db.hist_rows(60, "ttft", 0, i64::MAX, |ts, acc| windows.push((ts, acc))).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].1.counts, vec![0.0, 22.0, 0.0, 0.0, 0.0]);
        assert_eq!(windows[0].1.le, le);
        assert_eq!(pipe.latency_now().ttft.map(|s| s.p50_ms), Some(150.0));
    }

    #[test]
    fn the_gate_log_is_merged_per_ten_minutes() {
        let db = Db::memory();
        let mut pipe = Pipeline::default();
        let (log, _) = lss_core::gatelog::ingest(include_str!("../../../fixtures/gate_log.txt"), None);
        for k in 0..3 {
            let mut s = sample(T0 + k * 300, 0.0);
            s.log = log.clone();
            pipe.push(&db, &s, None, true);
        }
        assert_eq!(db.gatelog_merged(0, i64::MAX).unwrap().public.requests, 48, "two samples in the closed bucket");
        assert_eq!(pipe.gate_open_bucket().map(|(t, l)| (*t, l.public.requests)), Some((T0 + 600, 24)));
    }

    #[test]
    fn an_old_database_is_backfilled_without_overwriting_live_rows() {
        let db = Db::memory();
        for k in 0..(3 * 720) {
            db.insert_sample(&sample(T0 + k * 5, 50.0)).unwrap();
        }
        // the live loop already wrote one minute with another value
        db.write_rollup(&RollupBatch { res: 60, ts: T0 + 60, rows: vec![("decode_tok_s".into(), Agg::one(777.0))] }, true).unwrap();
        assert_eq!(backfill(&db, T0 + 3 * 3600).unwrap(), 3 * 720);
        let m1 = rows(&db, 60, "decode_tok_s");
        assert_eq!(m1.len(), 179, "every closed minute");
        assert_eq!(m1[1].1.avg(), 777.0, "a row that was already there wins");
        assert_eq!(m1[2].1.avg(), 50.0);
        assert_eq!(rows(&db, 600, "decode_tok_s").len(), 17);
    }
}
