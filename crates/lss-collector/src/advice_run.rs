//! The numbers behind `GET /advice`: one `WindowStats` per window (24 h, 7 d, 30 d), read from
//! the rollup tiers (1-minute rows for the day, 10-minute rows beyond), the stored histograms
//! and the gate-log rollup. The rules themselves are pure (`lss_core::advice`).

use crate::db::Db;
use lss_core::advice::{AdviceDoc, GpuThrottle, Growth, LaneRejects, WindowStats};
use lss_core::compare::LoadoutCard;
use lss_core::config::Config;
use lss_core::hist::HistAccum;
use lss_core::series::Agg;

pub const WINDOWS: [(&str, i64); 3] = [("24h", 86_400), ("7d", 7 * 86_400), ("30d", 30 * 86_400)];

fn tier(secs: i64) -> i64 {
    if secs <= 86_400 { 60 } else { 600 }
}

/// Every row of one metric in the window, merged; and the per-row maxima (for a p95 of peaks).
fn merged(db: &Db, res: i64, metric: &str, from: i64, to: i64) -> Option<(Agg, Vec<f64>)> {
    let mut total: Option<Agg> = None;
    let mut peaks = Vec::new();
    let _ = db.rollup_rows(res, metric, from, to, |_, a| {
        peaks.push(a.max);
        match &mut total {
            Some(t) => t.merge(&a),
            None => total = Some(a),
        }
    });
    total.map(|t| (t, peaks))
}

fn percentile(values: &mut [f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[((values.len() - 1) as f64 * q).round() as usize])
}

fn hist(db: &Db, res: i64, metric: &str, from: i64, to: i64) -> HistAccum {
    let mut total = HistAccum::default();
    let _ = db.hist_rows(res, metric, from, to, |_, h| total.add(&h));
    total
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// What every window shares.
pub struct Ctx<'a> {
    pub slots: u32,
    pub first_sample_ts: i64,
    /// the current loadout (its concurrency curve, context length and bench scorecard)
    pub card: Option<&'a LoadoutCard>,
    pub gpu_indices: &'a [u32],
    pub thermal_exclude: &'a [u32],
}

/// The owner's targets over one window: shares from the engine's histograms, uptime from the
/// incident record.
pub fn targets(db: &Db, now: i64, name: &str, secs: i64, first_sample_ts: i64, cfg: &lss_core::targets::TargetsConfig) -> lss_core::targets::TargetsStatus {
    let (from, to, res) = (now - secs, now + 1, tier(secs));
    let start = from.max(first_sample_ts);
    let uptime_pct = (now > start).then(|| {
        let down: i64 = db.incidents_since(start).unwrap_or_default().iter().filter(|i| i.kind == lss_core::incidents::KIND_SERVE_DOWN).map(|i| (i.end.unwrap_or(now).min(now) - i.start.max(start)).max(0)).sum();
        (1.0 - down as f64 / (now - start) as f64).clamp(0.0, 1.0) * 100.0
    });
    // a gateway publishing /gate/health v5.2+ logs every request's prompt size and time to first byte: the first-word
    // target is judged on the SHORT prompts (a long uncached prompt waits for its own reading)
    let log = db.gatelog_merged(from, to).unwrap_or_default();
    let mut short = lss_core::gatelog::TimeAgg::default();
    for lane in [&log.public, &log.trusted] {
        for (bucket, upper) in lss_core::gatelog::SIZE_BUCKETS {
            if upper <= lss_core::targets::SHORT_PROMPT_TOKENS {
                if let Some(t) = lane.ttfb_by_size.get(bucket) {
                    short.merge(t);
                }
            }
        }
    }
    let ttft_short = Some(short.within(cfg.ttft_p95_s * 1000.0)).filter(|(counted, _)| *counted > 0);
    let inputs = lss_core::targets::TargetInputs { ttft: hist(db, res, "ttft", from, to), itl: hist(db, res, "itl", from, to), queue_time: hist(db, res, "queue_time", from, to), ttft_short, uptime_pct };
    lss_core::targets::evaluate(cfg, name, &inputs)
}

pub fn window_stats(db: &Db, now: i64, name: &str, secs: i64, cx: &Ctx) -> WindowStats {
    let Ctx { slots, first_sample_ts, card, gpu_indices, thermal_exclude } = *cx;
    let (from, to, res) = (now - secs, now + 1, tier(secs));
    let avg_pct = |metric: &str| merged(db, res, metric, from, to).map(|(a, _)| round1(a.avg() * 100.0));
    let sum = |metric: &str| merged(db, res, metric, from, to).map(|(a, _)| a.sum);
    let kv = merged(db, res, "kv_usage", from, to);
    let queue_time = hist(db, res, "queue_time", from, to);
    let prompts = hist(db, res, "prompt_tokens", from, to);
    let log = db.gatelog_merged(from, to).unwrap_or_default();
    let prompt_max_gate = log.public.prompt_max.max(log.trusted.prompt_max);
    let prompt_max_edge = prompts.counts.iter().rposition(|c| *c > 0.0).and_then(|i| prompts.le.get(i).copied());
    let tok_gen = sum("tok_gen");
    // A RATIO of two metrics only means something over the span BOTH were recorded (metrics were
    // added over time: on 2026-09-20 five hours of energy were divided by nineteen hours of
    // tokens, and ADVICE said 65 Wh per 1M tokens where MODEL said 653).
    let first = |metric: &str| db.rollup_first_ts(res, metric).ok().flatten();
    let both = |a: &str, b: &str| -> (Option<f64>, Option<f64>) {
        match (first(a), first(b)) {
            (Some(fa), Some(fb)) => {
                let start = from.max(fa).max(fb);
                let over = |m: &str| merged(db, res, m, start, to).map(|(agg, _)| agg.sum);
                (over(a), over(b))
            }
            _ => (None, None),
        }
    };
    let (tok_cached, tok_prompt) = both("tok_cached", "tok_prompt");
    let (prefill, e2e) = both("sum_prefill_s", "sum_e2e_s");
    let (energy, energy_tokens) = both("sum_energy_serving_j", "tok_gen");
    // a RATE is a count over the seconds that count was kept, not over the whole window
    let evicted_secs = first("tok_evicted").map(|f| (to - from.max(f)).max(0));
    let rejects = [("public", &log.public), ("trusted", &log.trusted)].into_iter().map(|(lane, l)| LaneRejects { lane: lane.into(), too_busy_429: l.status_count(429), too_large_413: l.status_count(413) }).collect();
    let throttle = gpu_indices
        .iter()
        .filter_map(|i| {
            let thermal = merged(db, res, &format!("gpu{i}_thr_thermal"), from, to)?.0.avg();
            let hw = merged(db, res, &format!("gpu{i}_thr_hw"), from, to).map_or(0.0, |(a, _)| a.avg());
            Some(GpuThrottle { index: *i, pct: round1(thermal.max(hw) * 100.0), excluded: thermal_exclude.contains(i) })
        })
        .collect();
    WindowStats {
        name: name.into(),
        secs,
        covered_secs: (now - first_sample_ts.max(from)).clamp(0, secs),
        slots,
        slots_full_pct: avg_pct("busy_slots"),
        queue_pct: avg_pct("busy_wait"),
        queue_wait_p95_ms: queue_time.quantile(0.95).map(|s| round1(s * 1000.0)),
        kv_peak_pct: kv.as_ref().map(|(a, _)| round1(a.max * 100.0)),
        kv_p95_pct: kv.map(|(_, mut peaks)| percentile(&mut peaks, 0.95).map(|p| round1(p * 100.0))).unwrap_or(None),
        retracted_pct: avg_pct("kv_retracted"),
        evicted_tokens: sum("tok_evicted"),
        evicted_secs,
        prompt_p50: prompts.quantile(0.5).map(round1),
        prompt_p95: prompts.quantile(0.95).map(round1),
        prompt_max: if prompt_max_gate > 0.0 { Some(prompt_max_gate) } else { prompt_max_edge },
        prompt_max_is_bucket_edge: prompt_max_gate <= 0.0 && prompt_max_edge.is_some(),
        prompt_requests: Some((prompts.count - db.probes_logged_since(from).unwrap_or(0) as f64).max(0.0)),
        context_len: card.map(|c| c.row.context_len).filter(|c| *c > 0.0),
        cache_hit_pct: match (tok_cached, tok_prompt) {
            (Some(c), Some(p)) if p > 0.0 => Some(round1((c / p).min(1.0) * 100.0)),
            _ => None,
        },
        // by concurrency: the current loadout's live curve
        spec_by_level: card.map(|c| c.row.curve.iter().filter(|r| r.samples >= lss_core::loadout::MIN_LEVEL_SAMPLES).filter_map(|r| r.spec_accept_length.map(|a| (r.running, a))).collect()).unwrap_or_default(),
        prefill_time_pct: match (prefill, e2e) {
            (Some(p), Some(e)) if e > 0.0 => Some(round1((p / e).min(1.0) * 100.0)),
            _ => None,
        },
        rejects,
        growth: None,
        wh_per_mtok: match (energy, energy_tokens) {
            (Some(j), Some(t)) if t > 0.0 && j > 0.0 => Some(round1(j / 3600.0 / (t / 1e6))),
            _ => None,
        },
        gen_tokens: tok_gen,
        throttle,
        targets: None,
        saturation: card.map(|c| lss_core::loadout::saturation_of(&lss_core::bench::merge_curve(&c.row.curve, c.speed()), c.row.peak_tok_s)),
        saturation_levels: card.map_or(0, |c| lss_core::loadout::curve_evidence(&lss_core::bench::merge_curve(&c.row.curve, c.speed())).0),
        saturation_from_bench: card.is_some_and(|c| lss_core::loadout::curve_evidence(&lss_core::bench::merge_curve(&c.row.curve, c.speed())).1),
    }
}

/// The last 7 days against the 7 before them, from the 10-minute tier.
pub fn growth(db: &Db, now: i64, first_sample_ts: i64) -> Option<Growth> {
    let week = 7 * 86_400;
    let (now_from, prev_from) = (now - week, now - 2 * week);
    // card #211: users at once ON THIS MACHINE (a v5.10+ gateway attributes each user's load to
    // the upstream serving it). Used only when BOTH weeks have it - comparing this box's peak
    // against a cross-machine peak would be a growth figure made of two different things.
    let here = |from: i64, to: i64| merged(db, 600, "users_active_here", from, to).map(|(a, _)| a.max);
    let by_machine = here(now_from, now + 1).is_some() && here(prev_from, now_from).is_some();
    // otherwise the gateway's count (every machine it fronts), else requests running at once
    let gate_users = |from: i64, to: i64| merged(db, 600, "users_active", from, to).map(|(a, _)| a.max).filter(|m| *m > 0.0);
    let span = |from: i64, to: i64| {
        let tokens = merged(db, 600, "tok_gen", from, to).map_or(0.0, |(a, _)| a.sum);
        let requests = merged(db, 600, "sum_requests", from, to).map_or(0.0, |(a, _)| a.sum);
        let users = if by_machine {
            here(from, to).unwrap_or(0.0)
        } else {
            gate_users(from, to).or_else(|| merged(db, 600, "running", from, to).map(|(a, _)| a.max)).unwrap_or(0.0)
        };
        (tokens, requests, users)
    };
    // say so when the figure is the gateway's cross-machine count, never pass it off as this box's
    let basis = (!by_machine && gate_users(prev_from, now + 1).is_some())
        .then(|| "gateway too old to split users by machine - counted across every machine it fronts".to_string());
    let prev_days = ((now_from - first_sample_ts.max(prev_from)).max(0) as f64) / 86_400.0;
    let now_days = (((now - first_sample_ts.max(now_from)).max(0) as f64) / 86_400.0).max(1.0 / 24.0);
    if prev_days <= 0.0 {
        return None;
    }
    let (t1, r1, u1) = span(now_from, now + 1);
    let (t0, r0, u0) = span(prev_from, now_from);
    Some(Growth { tokens_per_day: t1 / now_days, tokens_per_day_prev: t0 / prev_days.max(1.0 / 24.0), requests_per_day: round1(r1 / now_days), requests_per_day_prev: round1(r0 / prev_days.max(1.0 / 24.0)), peak_users: u1, peak_users_prev: u0, prev_days_covered: round1(prev_days), peak_users_basis: basis })
}

pub fn compute(db: &Db, now: i64, cfg: &Config, slots: u32, first_sample_ts: i64, card: Option<&LoadoutCard>, gpu_indices: &[u32]) -> AdviceDoc {
    let grown = growth(db, now, first_sample_ts);
    let cx = Ctx { slots, first_sample_ts, card, gpu_indices, thermal_exclude: &cfg.rules.thermal_exclude };
    let stats = WINDOWS
        .iter()
        .map(|(name, secs)| {
            let mut w = window_stats(db, now, name, *secs, &cx);
            w.targets = Some(targets(db, now, name, *secs, first_sample_ts, &cfg.targets));
            if *name != "24h" {
                w.growth = grown.clone();
            }
            w
        })
        .collect();
    lss_core::advice::build(now, stats, &cfg.advice, &cfg.rules.thermal_exclude)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::gpu::GpuSample;
    use lss_core::model::Sample;
    use lss_core::prom::ServeMetrics;

    const T0: i64 = 1_789_000_000 - 1_789_000_000 % 600;

    /// Two hours of 5 s samples: all 8 slots busy for the first 6 minutes of each hour, a queue
    /// for 3 of those, KV climbing to 62 %, GPU1 throttling for 12 minutes, GPU0 all the time.
    fn filled() -> (Db, i64) {
        let db = Db::memory();
        let mut pipeline = crate::rollup::Pipeline::default();
        let mut gen = 0.0;
        let n = 2 * 720;
        for k in 0..n {
            let ts = T0 + k * 5;
            let in_hour = k % 720;
            let busy = in_hour < 72;
            gen += if busy { 3000.0 } else { 50.0 };
            let m = ServeMetrics {
                running: if busy { 8.0 } else { 1.0 },
                queue: if in_hour < 36 { 3.0 } else { 0.0 },
                token_usage: if busy { 0.62 } else { 0.10 },
                generation_tokens_total: gen,
                prompt_tokens_total: gen * 10.0,
                cached_tokens_total: gen * 9.0,
                requests_total: k as f64,
                e2e_sum: k as f64 * 4.0,
                prefill_forward_sum: k as f64,
                gen_throughput: if busy { 600.0 } else { 10.0 },
                ..Default::default()
            };
            let gpus = (0..2).map(|i| GpuSample { index: i, power_w: Some(if busy { 250.0 } else { 40.0 }), throttle_mask: if i == 0 || (i == 1 && k < 144) { lss_core::gpu::THROTTLE_SW_THERMAL } else { 0 }, ..Default::default() }).collect();
            let s = Sample { ts, serve_up: true, metrics: Some(m), gpus_ok: true, gpus, slots: 8, ..Default::default() };
            pipeline.push(&db, &s, None, true);
        }
        (db, T0 + n * 5)
    }

    #[test]
    fn window_numbers_come_from_the_rollups() {
        let (db, now) = filled();
        let w = window_stats(&db, now, "24h", 86_400, &Ctx { slots: 8, first_sample_ts: T0, card: None, gpu_indices: &[0, 1], thermal_exclude: &[0] });
        assert_eq!(w.covered_secs, 7200);
        // the minute still open is not rolled up yet, so shares are within a fraction of a percent
        let near = |v: Option<f64>, want: f64| v.is_some_and(|x| (x - want).abs() <= 0.3);
        assert!(near(w.slots_full_pct, 10.0), "6 minutes of every hour: {:?}", w.slots_full_pct);
        assert!(near(w.queue_pct, 5.0), "{:?}", w.queue_pct);
        assert_eq!((w.kv_peak_pct, w.retracted_pct), (Some(62.0), Some(0.0)));
        assert_eq!(w.cache_hit_pct, Some(90.0));
        assert_eq!(w.prefill_time_pct, Some(25.0));
        let thr: Vec<(u32, f64, bool)> = w.throttle.iter().map(|g| (g.index, g.pct, g.excluded)).collect();
        assert_eq!((thr[0].0, thr[0].1, thr[0].2, thr[1].0, thr[1].2), (0, 100.0, true, 1, false));
        assert!((thr[1].1 - 10.0).abs() <= 0.3, "GPU1 throttled 12 of 120 minutes: {}", thr[1].1);
        // energy while serving: every sample serves (running >= 1): 2 GPUs x (250 W for 10 %, 40 W for 90 %)
        let wh = w.wh_per_mtok.unwrap();
        assert!(wh > 100.0 && wh < 1000.0, "{wh}");
        assert!(w.gen_tokens.unwrap() > 400_000.0);
        // the rules read it
        let doc = lss_core::advice::build(now, vec![w], &Config::default().advice, &[0]);
        let rules: Vec<(&str, &str)> = doc.windows[0].findings.iter().map(|f| (f.rule.as_str(), f.severity.as_str())).collect();
        assert!(rules.contains(&("slots_saturated", "watch")) && rules.contains(&("queue_wait", "watch")) && rules.contains(&("throttling", "act")), "{rules:?}");
        let throttling = doc.windows[0].findings.iter().find(|f| f.rule == "throttling").unwrap();
        assert!(throttling.sentence.starts_with("GPU1 ") && !throttling.sentence.contains("GPU0"), "{}", throttling.sentence);
        assert_eq!(doc.top.len(), 2);
    }

    /// card #211: two weeks of samples, one per 10 minutes, from a gateway fixture.
    fn two_weeks_with_gate(gate_json: &str, engine_model: Option<&str>) -> (Db, i64) {
        let db = Db::memory();
        let mut pipeline = crate::rollup::Pipeline::default();
        let gate = lss_core::gate::parse_gate_health(gate_json).unwrap();
        let n = 15 * 144;
        for k in 0..n {
            let s = Sample { ts: T0 + k * 600, serve_up: true, model: engine_model.map(String::from), users: lss_core::users::user_points(&gate), gate: Some(gate.clone()), slots: 8, ..Default::default() };
            pipeline.push(&db, &s, None, true);
        }
        (db, T0 + n * 600)
    }

    #[test]
    fn growth_counts_users_on_this_machine_and_says_so_when_the_gateway_is_too_old() {
        // a v5.10 gateway: user A on box-x, B on box-y; our engine serves model-b -> box-y
        let (db, now) = two_weeks_with_gate(include_str!("../../../fixtures/gate_health_v510.json"), Some("model-b"));
        let g = growth(&db, now, T0).expect("two weeks of data");
        assert_eq!((g.peak_users, g.peak_users_prev), (1.0, 1.0), "only B is on THIS machine, not A: {g:?}");
        assert_eq!(g.peak_users_basis, None, "this machine's own count needs no caveat");
        // a v5.9 gateway cannot split: the cross-machine count, SAID to be one
        let (db, now) = two_weeks_with_gate(include_str!("../../../fixtures/gate_health_v59.json"), Some("model-b"));
        let g = growth(&db, now, T0).unwrap();
        assert_eq!((g.peak_users, g.peak_users_prev), (2.0, 2.0), "{g:?}");
        let basis = g.peak_users_basis.as_deref().expect("the old-gateway figure must carry its caveat");
        assert!(basis.contains("gateway too old"), "{basis}");
    }

    #[test]
    fn an_empty_database_says_nothing_and_growth_needs_an_earlier_week() {
        let db = Db::memory();
        let doc = compute(&db, T0, &Config::default(), 8, T0, None, &[0]);
        assert_eq!(doc.windows.len(), 3);
        assert!(doc.windows.iter().all(|w| w.findings.iter().all(|f| f.rule == "rejections")), "only 'the gateway turned nobody away' can be said of nothing: {:?}", doc.windows[0].findings);
        assert!(growth(&db, T0, T0).is_none());
        let (db, now) = filled();
        assert!(growth(&db, now, T0).is_none(), "two hours of data have no earlier week");
        let g = growth(&db, now + 8 * 86_400, T0).unwrap();
        assert!(g.tokens_per_day_prev > 0.0 && g.tokens_per_day == 0.0 && g.peak_users_prev == 8.0, "{g:?}");
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    use lss_core::series::{Agg, RollupBatch};

    /// A database where one metric starts 14 hours after the other (metrics were added over
    /// time). 2026-09-20: five hours of energy divided by nineteen hours of tokens said 65 Wh per
    /// 1M tokens where the truth was 653.
    #[test]
    fn a_ratio_only_uses_the_span_both_metrics_cover() {
        let db = Db::memory();
        let now = 2_000_000_000 - 2_000_000_000 % 600;
        let hours = |h: i64| now - h * 3600;
        let write = |metric: &str, from_h: i64, per_bucket: f64| {
            let mut ts = hours(from_h);
            while ts < now {
                db.write_rollup(&RollupBatch { res: 600, ts, rows: vec![(metric.to_string(), Agg::from_row(per_bucket, per_bucket, per_bucket, 1.0))] }, true).unwrap();
                db.write_rollup(&RollupBatch { res: 60, ts, rows: vec![(metric.to_string(), Agg::from_row(per_bucket, per_bucket, per_bucket, 1.0))] }, true).unwrap();
                ts += 600;
            }
        };
        // 19 h of tokens (10 000 per bucket); energy (6000 J per bucket) only for the last 5 h
        write("tok_gen", 19, 10_000.0);
        write("sum_energy_serving_j", 5, 6_000.0);
        // cached tokens only for the last 5 h too, prompt tokens all along
        write("tok_prompt", 19, 100_000.0);
        write("tok_cached", 5, 90_000.0);
        // evictions counted for the last 2 h: 60 000 per bucket = 360 000 per hour
        write("tok_evicted", 2, 60_000.0);
        let cx = Ctx { slots: 8, first_sample_ts: hours(19), card: None, gpu_indices: &[], thermal_exclude: &[] };
        let w = window_stats(&db, now, "24h", 86_400, &cx);
        // 6000 J per 10 000 tokens = 0.6 J/token = 166.7 Wh per 1M - NOT 5/19 of it
        assert_eq!(w.wh_per_mtok, Some(166.7));
        assert_eq!(w.cache_hit_pct, Some(90.0), "cached over the prompt tokens of the same 5 hours, not of all 19");
        assert_eq!(w.evicted_secs, Some(2 * 3600 + 1));
        let findings = lss_core::advice::evaluate(&w, &lss_core::config::AdviceConfig::default());
        let evictions = findings.iter().find(|f| f.rule == "evictions").expect("evictions finding");
        assert!(evictions.value.is_some_and(|v| (v - 360_000.0).abs() < 200.0), "per hour over the two hours it was counted: {:?}", evictions.value);

        // /tokens: the request counter is 5 h old, the token counters 19 h: the 24h row says from when
        write("sum_requests", 5, 12.0);
        let doc = crate::query::tokens_doc(&db, now, now - 3600, hours(19), &Default::default(), &Default::default(), crate::query::TokensDocFlags::default());
        let day = doc.windows.iter().find(|w| w.name == "24h").unwrap();
        assert_eq!((day.requests, day.requests_since), (360.0, Some(hours(5))));
        assert_eq!(day.cache_share, Some(0.9), "cached over the prompt tokens of the span both cover");
        // ALL TIME is never smaller than 24 h: a ledger that began 5 h ago is topped up with the
        // 14 h of history from before it
        let ledger = lss_core::tokens::AllTime { since: hours(5), generated: lss_core::tokens::CounterTotal { total: 300_000.0, ..Default::default() }, ..Default::default() };
        let doc2 = crate::query::tokens_doc(&db, now, now - 3600, hours(19), &ledger, &Default::default(), crate::query::TokensDocFlags::default());
        let all = doc2.windows.iter().find(|w| w.name == "all").unwrap();
        let day2 = doc2.windows.iter().find(|w| w.name == "24h").unwrap();
        assert_eq!((doc2.all_time_since, all.secs), (hours(19), 19 * 3600));
        assert!(all.generated >= day2.generated - 10_000.0 && all.generated <= day2.generated + 300_000.0, "all {} vs 24h {}", all.generated, day2.generated);
        assert_eq!(all.generated, 300_000.0 + 10_000.0 * 14.0 * 6.0, "the ledger + the 84 buckets that ended before it began (never one twice)");
        let hour = doc.windows.iter().find(|w| w.name == "1h").unwrap();
        assert_eq!(hour.requests_since, None, "the last hour is fully covered: nothing to qualify");
    }
}
