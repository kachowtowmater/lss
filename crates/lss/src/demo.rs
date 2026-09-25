//! Deterministic, real-looking data for every page: what the render tests draw, and what
//! `lss --demo` shows without a collector (screenshots, trying a layout). No randomness, no
//! clock: the same inputs always give the same picture.

use crate::data::{gpu_tokens, latency_tokens, range_secs, PageCtx, PageData, PageId, GATEWAY_TOKENS, LOAD_TOKENS};
use lss_core::advice::{AdviceDoc, GpuThrottle, Growth, LaneRejects, WindowStats};
use lss_core::bench::{Accuracy, BenchBrief, BenchDoc, Check, DecodeCell, LastRun, NeedleResult, PrefillCell, Scorecard};
use lss_core::compare::{LoadoutCard, LoadoutsDoc};
use lss_core::loadout::{Curve, LoadoutAcc, LoadoutIdentity, Saturation};
use lss_core::tokens::{DayPeak, Peak, TokenWindow, TokensDoc};
use lss_core::hist::{HistAccum, LATENCY_METRICS};
use lss_core::model::Status;
use lss_core::series::{hist_plan, plan, Agg, GatewayDoc, HistDoc, RulesDoc, SeriesBuilder, SeriesDoc};

const DAY: i64 = 86_400;
const KEEP: (i64, i64, i64) = (DAY, 14 * DAY, 90 * DAY);

/// The `/status` golden as it is: a real capture of a box whose gateway is older than v5.2
/// (no per-user numbers), with no benchmark and no advice yet.
pub fn status_plain() -> Status {
    let mut s: Status = serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).expect("the golden parses");
    // card #225: the golden fixture now CARRIES user rows (so the orphan-key guard enumerates
    // every key under them); the "plain" capture is what a gateway older than v5.2 sends, which
    // has no per-user stats - so it says so explicitly rather than inheriting the fixture's rows
    s.users = Default::default();
    // card #234: likewise the golden carries a synthetic serve.live_decode (a CurveRow) so its
    // fields are guarded; the demo capture's renders were designed with no live reading, so it
    // stays absent here - every render is unchanged
    s.serve.live_decode = None;
    // card #235: and the golden's synthetic bench.last - the demo capture shows no run on record
    s.bench.last = None;
    s
}

/// The same capture with everything the newer pages show: the gateway's user table (v5.2), a
/// benchmark on record, the current loadout and the two most pressing pieces of advice.
pub fn status() -> Status {
    let mut s = status_plain();
    let now = s.generated_at;
    let mut gate = lss_core::gate::parse_gate_health(include_str!("../../../fixtures/gate_health_v52.json")).expect("the v5.2 fixture parses");
    if let Some(users) = gate.users.as_mut() {
        users.push(lss_core::gate::GateUser { lane: "trusted".into(), user: lss_core::users::BENCH_USER.into(), requests_24h: 9, ok_24h: 9, completion_tokens_24h: 5_400, completion_tokens_exact: true, last_seen: (now - 7_400) as f64, last_status: Some(200), ..Default::default() });
    }
    let aliases = [lss_core::users::UserAlias { ip: "192.0.2.76".into(), name: "laptop".into() }, lss_core::users::UserAlias { ip: "2001:db8::17".into(), name: "office".into() }];
    let own = lss_core::users::OwnTraffic { probe_user: Some("127.0.0.1"), probes_1h: 12, probes_24h: 288 };
    s.users = lss_core::users::build_users(Some(&gate), &aliases, &Default::default(), &own, now);
    let (log, _) = lss_core::gatelog::ingest(include_str!("../../../fixtures/gate_log_v52.txt"), None);
    lss_core::users::attach_experience(&mut s.users, &log, own.probe_user);
    s.serve.running = 3.0;
    s.advice_top = advice(now).top;
    s.bench = bench(now).brief;
    let cards = loadouts(now);
    // reading speed (prefill): something was read in the last ten minutes
    s.serve.prefill_tok_s = Some(4_180.0);
    s.serve.prefill_tok_s_typical = cards.loadouts.first().and_then(|c| c.row.prefill_tok_s);
    s.serve.cached_share_10m = Some(0.95);
    // reading is bursty: it happens where writing happens, in shorter spikes
    s.series.prefill_tok_s = s.series.decode_tok_s.iter().enumerate().map(|(i, d)| d.map(|v| if v > 0.0 && i % 3 != 1 { (v * 9.0).round() } else { 0.0 })).collect();
    // card #281: the three trend series only a collector can fill (histogram p99s from its minute
    // rollups, $/h from its rates table) - the demo has no collector, so they are shaped from the
    // demo's own running series (a busier minute is a slower one) and its own power and rate
    let at_minute = |v: &[Option<f64>], f: &dyn Fn(f64) -> f64| -> Vec<Option<f64>> { v.chunks(2).flat_map(|c| { let x = c.iter().flatten().copied().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b)))); vec![x.map(|x| (f(x) * 10.0).round() / 10.0); c.len()] }).collect() };
    s.series.ttft_p99_ms = at_minute(&s.series.running, &|r| 600.0 + 350.0 * r);
    s.series.itl_p99_ms = at_minute(&s.series.running, &|r| 11.0 + 1.0 * r);
    s.series.prefix_hit = s.series.decode_tok_s.iter().map(|d| d.map(|_| s.serve.cached_share_10m.unwrap_or(0.95))).collect();
    s.series.usd_per_hour = s.series.gpu_power_w.iter().map(|w| w.map(|w| (w / 1000.0 * 0.30 * 10_000.0).round() / 10_000.0)).collect();
    s.targets = targets("24h", 80.0);
    s.loadout = cards.loadouts.first().map(|c| lss_core::model::LoadoutBrief { id: c.row.id.clone(), model: c.row.model.clone(), image_tag: c.row.image_tag.clone(), first_seen: c.row.first_seen, runs: c.row.runs, flags: c.row.flags.clone() });
    // #51: page 1 redesign fields
    s.serve.page1_fields_deployed = true; // the demo represents a fully-deployed collector
    s.serve.evicted_tok_per_hour_10m = Some(58_000.0);
    s.serve.secs_since_last_token = Some(2);
    s.serve.work_1h = Some(lss_core::model::WorkWindow { prompt: 210_000.0, cached: 198_000.0, generated: 42_000.0, requests: 61.0 });
    // #118: the SGLang priority label the gate stamps per lane
    s.serve.public_priority = "10".into();
    s.serve.trusted_priority = "0".into();
    // #73: page 1 redesign around cost/loadout - electricity + tokens-by-window demo figures
    s.cost = Some(lss_core::model::CostStatus {
        rate_name: "Example Utility TOU (demo)".into(),
        rate_source: String::new(),
        effective_date: "2026-01-01".into(),
        is_flat: false,
        current_usd_per_kwh: Some(0.30),
        current_period: "summer_off_peak".into(),
        live_usd_per_hour: Some(0.42),
        today_kwh: Some(9.6),
        today_usd: Some(3.87),
        today_usd_per_million_generated_tokens: Some(9.21),
        today_usd_per_million_prompt_tokens: Some(0.49),
        // card #174: the primary figure - work energy only, over uncached prefill + generated -
        // reads far lower than the old generated-only headline, illustrating exactly the
        // ~10x inflation the owner measured on a live hour.
        today_usd_per_million_real_work_tokens: Some(0.87),
        today_standby_kwh: Some(1.1),
        today_standby_usd: Some(0.44),
        // card #172 (verifier-2): these two were still the real plan, truncated one rounding
        // step - the SAME miss as status_golden.rs's own
        // fixture, in `lss --demo`, the first command a stranger runs. Invented outright now.
        fixed_usd_per_day: Some(1.25),
        unresolved_usd_per_kwh: Some(0.0050),
        last_24h_kwh: Some(24.3),
        last_24h_usd: Some(9.85),
        // #102: full coverage in the demo - a long-lived box, not a fresh install
        today_covered_secs: Some(now - lss_core::timeutil::local_midnight(now)),
        last_24h_covered_secs: Some(86_400),
    });
    s.tokens_by_window = Some(lss_core::model::TokenWindows {
        hour: s.serve.work_1h,
        hour_covered_secs: Some(3_600),
        day: Some(lss_core::model::WorkWindow { prompt: 5_040_000.0, cached: 4_752_000.0, generated: 1_008_000.0, requests: 1_464.0 }),
        day_covered_secs: Some(86_400),
        week: Some(lss_core::model::WorkWindow { prompt: 35_280_000.0, cached: 33_264_000.0, generated: 7_056_000.0, requests: 10_248.0 }),
        week_covered_secs: Some(7 * 86_400),
        month: Some(lss_core::model::WorkWindow { prompt: 151_200_000.0, cached: 142_560_000.0, generated: 30_240_000.0, requests: 43_920.0 }),
        month_covered_secs: Some(30 * 86_400),
    });
    // #74: WATCH page demo figures - a receipted release and a bare social claim, so `lss
    // --demo` shows both the honest content-not-titles rendering and the UNVERIFIED label.
    s.watch = Some(lss_core::watch::WatchStatus {
        sources: vec![
            lss_core::watch::WatchSource {
                name: "Example Recipe Repo (demo)".into(),
                covers: "GLM loadouts on the GPU box".into(),
                last_checked: Some(now - 5_400),
                newest: Some(lss_core::watch::WatchItem {
                    date: "2026-09-20".into(),
                    summary: "swapped the chat template for a fixed one from upstream, gated PASS same day".into(),
                    url: "https://example.com/releases/v1.2.3".into(),
                    has_receipt: true,
                }),
            },
            lss_core::watch::WatchSource {
                name: "Example Social Account (demo)".into(),
                covers: "reference only".into(),
                last_checked: Some(now - 1_800),
                newest: Some(lss_core::watch::WatchItem { date: "2026-09-19".into(), summary: "claims a new kernel is 2x faster, no link".into(), url: String::new(), has_receipt: false }),
            },
        ],
    });
    s
}

/// The owner's targets: three met, the queue wait missed (`queue_met` % of requests).
pub fn targets(window: &str, queue_met: f64) -> lss_core::targets::TargetsStatus {
    use lss_core::hist::HistAccum;
    let h = |le: &[f64], counts: &[f64]| HistAccum { le: le.to_vec(), counts: counts.to_vec(), sum: 0.0, count: counts.iter().sum() };
    lss_core::targets::evaluate(
        &Default::default(),
        window,
        &lss_core::targets::TargetInputs {
            ttft: h(&[1.0, 5.0, 10.0], &[800.0, 160.0, 40.0, 0.0]),
            itl: h(&[0.02, 0.05], &[9_000.0, 1_000.0, 0.0]),
            queue_time: h(&[0.5, 2.0, 8.0], &[500.0, queue_met * 10.0 - 500.0, 1_000.0 - queue_met * 10.0, 0.0]),
            uptime_pct: Some(99.95),
            ttft_short: None,
        },
    )
}

#[allow(clippy::too_many_arguments)] // a demo-data builder: every argument is a number on the screen
fn scorecard(run_id: i64, loadout_id: &str, model: &str, profile: &str, ended_at: i64, c1: f64, totals: [f64; 4], prefill: [f64; 3]) -> Scorecard {
    let levels = [1u32, 2, 4, 8];
    let mut decode: Vec<DecodeCell> = levels.iter().zip(totals).map(|(n, t)| DecodeCell { concurrency: *n, context: 0, per_user_tok_s: ((if *n == 1 { c1 } else { t / f64::from(*n) }) * 10.0).round() / 10.0, total_tok_s: if *n == 1 { c1 } else { t }, ttft_ms: Some(120.0 + f64::from(*n) * 45.0), ..Default::default() }).collect();
    decode.extend(levels.iter().zip(totals).map(|(n, t)| DecodeCell { concurrency: *n, context: 16_384, per_user_tok_s: ((if *n == 1 { c1 } else { t / f64::from(*n) }) * 8.8).round() / 10.0, total_tok_s: ((if *n == 1 { c1 } else { t }) * 8.8).round() / 10.0, ttft_ms: Some(2_600.0 + f64::from(*n) * 300.0), ..Default::default() }));
    let sizes = [8_198u64, 64_509, 131_080];
    let full = profile == "full";
    if full {
        // the long-conversation step: one user, 64k / 128k / 250k tokens already read
        decode.extend([(65_536u64, 0.71), (131_072, 0.55), (250_000, 0.38)].map(|(context, k)| DecodeCell { concurrency: 1, context, per_user_tok_s: (c1 * k * 10.0).round() / 10.0, total_tok_s: (c1 * k * 10.0).round() / 10.0, ttft_ms: Some(context as f64 / 6.2), ..Default::default() }));
    }
    Scorecard {
        run_id,
        loadout_id: loadout_id.into(),
        model: model.into(),
        profile: profile.into(),
        started_at: ended_at - if full { 1_790 } else { 312 },
        ended_at,
        duration_s: if full { 1_790 } else { 312 },
        status: "ok".into(),
        note: "after the swap".into(),
        harness_version: "0.4.29".into(),
        harness_commit: "86cf05c".into(),
        target: "engine direct".into(),
        decode,
        prefill: sizes.iter().zip(prefill).take(if full { 3 } else { 2 }).map(|(t, s)| PrefillCell { tokens: *t, tok_s: s, ttft_ms: (*t as f64 / s * 1000.0).round() }).collect(),
        sanity: ["arithmetic", "tool call", "json output"].iter().map(|n| Check { name: (*n).into(), pass: true, detail: "ok".into() }).collect(),
        needle: if full { [10, 50, 90].iter().map(|d| NeedleResult { depth_pct: *d, prompt_tokens: 251_344, pass: true, detail: format!("found the code at {d}% depth"), secs: 44.0 }).collect() } else { Vec::new() },
        accept_length: Some(5.71),
        kv_tokens: Some(3_788_160.0),
        tokens_per_joule: Some(0.61),
        wh_per_mtok: Some(455.0),
        avg_watts: Some(948.0),
        garbled: full.then_some(lss_core::bench::Garbled { chars: 412_000, replacement_chars: 0, cjk_chars: 0, per_100k: 0.0 }),
        // card #14: the demo's runs are the gold standard - measured with nothing else on the
        // box, and the scorecard says so out loud rather than leaving it to be assumed
        background: Some(lss_core::bench::BackgroundLoad {
            samples: if full { 895 } else { 156 },
            polls_with_traffic: 0,
            concurrent_avg: Some(0.0),
            concurrent_max: Some(0.0),
            source: lss_core::bench::LOAD_SRC_USERS.into(),
            queue_avg: 0.0,
            queue_max: 0.0,
            engine_tok_s: Some(if full { 418.0 } else { 402.0 }),
        }),
        raw_dir: format!("~/.local/state/lss/bench/{run_id}-{profile}-{model}"),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)] // a demo-data builder
fn card(id: &str, model: &str, image: &str, flags: &str, current: bool, first_seen: i64, last_seen: i64, c1: f64, live_totals: &[(f64, f64, u64)], now: i64) -> LoadoutCard {
    let mut acc = LoadoutAcc { runs: if current { 2 } else { 1 }, last_started_at: first_seen, samples: 20_000, up_samples: if current { 20_000 } else { 19_940 }, secs: 100_000.0, serving_secs: 21_000.0, joules: 23.4e6, serving_joules: 16.8e6, gen_tokens: if current { 26.4e6 } else { 19.1e6 }, prompt_tokens: 812.0e6, cached_tokens: 771.0e6, requests: 41_200.0, prefill_secs: 6_900.0, prefill_tokens: 41.0e6, spec_len_sum: 5.88 * 900.0, spec_rate_sum: 0.79 * 900.0, spec_samples: 900, kv_peak: 0.33, kv_capacity_tokens: 3_788_160.0, context_len: 1_048_576.0, slots: 8, c1_probes: 121, c1_sum: c1 * 121.0, c1_best: c1 * 1.12, gate_requests: 41_500, gate_5xx: 9, gate_429: 38, gate_503: 2, prompt_max_tokens: 0.0, last_ts: last_seen, e2e_secs: 310_000.0, prefill_forward_secs: 58_000.0, ..Default::default() };
    let mut curve = Curve::default();
    for (running, total, samples) in live_totals {
        for _ in 0..*samples {
            curve.observe(*running, *total);
        }
        curve.observe_extra(*running, 0.19 * running * 40.0, 40.0, 6.1 - running * 0.35);
    }
    acc.curve = curve;
    // one request at a time, by how long its conversation already was
    for (bucket, k, samples) in [("0", 1.0, 5_200u64), ("16k", 0.9, 2_100), ("64k", 0.72, 640), ("128k", 0.0, 3)] {
        acc.ctx_speed.insert(bucket.into(), lss_core::loadout::CurveCell { samples, tok_s_sum: c1 * k * samples as f64, ..Default::default() });
    }
    acc.cold_starts = if current { vec![431.0, 412.0] } else { vec![388.0] };
    acc.engine_startup_s = 371.0;
    acc.read_peak_tok_s = 6_410.0;
    let le: Vec<f64> = vec![100.0, 1_000.0, 4_000.0, 16_000.0, 65_536.0, 131_072.0, 524_288.0];
    acc.prompt_len = HistAccum { le: le.clone(), counts: vec![900.0, 9_000.0, 14_000.0, 11_000.0, 4_800.0, 1_200.0, 300.0, 0.0], sum: 812.0e6, count: 41_200.0 };
    acc.gen_len = HistAccum { le: vec![16.0, 64.0, 256.0, 1_024.0, 4_096.0, 16_384.0], counts: vec![2_000.0, 6_000.0, 14_000.0, 13_000.0, 5_600.0, 600.0, 0.0], sum: 26.4e6, count: 41_200.0 };
    acc.itl = HistAccum { le: vec![0.002, 0.004, 0.006, 0.008, 0.012, 0.02], counts: vec![0.0, 9_000.0, 50_000.0, 30_000.0, 9_000.0, 2_000.0, 0.0], sum: 580.0, count: 100_000.0 };
    acc.ttft = HistAccum { le: vec![0.1, 0.2, 0.4, 1.0, 2.0, 6.0, 20.0], counts: vec![4_000.0, 12_000.0, 11_000.0, 7_000.0, 4_000.0, 2_500.0, 700.0, 0.0], sum: 39_000.0, count: 41_200.0 };
    let _ = now;
    let identity = LoadoutIdentity { id: id.into(), model: model.into(), image: image.into(), args_hash: format!("{id}{id}{id}{id}{id}aaaa"), flags: flags.into() };
    let mut row = lss_core::loadout::row(&identity, &acc, first_seen, current);
    // the demo knows what a kWh costs, so the MODEL page shows money
    row.apply_price(Some(0.30));
    LoadoutCard { row, ..Default::default() }
}

/// Three loadouts: model-a serving now (quick + full + accuracy on record), model-b before it
/// (quick + accuracy), a third loadout before that (never benchmarked: live numbers only).
pub fn loadouts(now: i64) -> LoadoutsDoc {
    let flags = "tp 4 · ep 4 · ctx 1048576 · quant modelopt_fp4 · kv fp8_e4m3 · slots 8 · spec NEXTN · draft 6";
    let mut model_a = card("3f9a1c07e2b4", "model-a", "model-a-serve:1.0", flags, true, now - 4 * DAY - 3_600, now, 187.6, &[(1.0, 188.0, 8_200), (2.0, 352.0, 2_900), (3.0, 480.0, 1_100), (4.0, 596.0, 1_000), (5.0, 650.0, 900), (6.0, 690.0, 850), (8.0, 705.0, 40)], now);
    model_a.quick = Some(scorecard(14, "3f9a1c07e2b4", "model-a", "quick", now - 2 * 3_600, 192.4, [192.4, 361.0, 604.0, 705.0], [5_791.0, 6_551.0, 6_100.0]));
    model_a.full = Some(scorecard(12, "3f9a1c07e2b4", "model-a", "full", now - 3 * DAY, 191.0, [191.0, 358.0, 598.0, 702.0], [5_760.0, 6_520.0, 6_080.0]));
    let accuracy = |run: i64, id: &str, model: &str, at: i64, score: f64| Scorecard { run_id: run, loadout_id: id.into(), model: model.into(), profile: "accuracy".into(), status: "ok".into(), started_at: at - 9_000, ended_at: at, duration_s: 9_000, accuracy: vec![Accuracy { dataset: "gsm8k".into(), score, n: 1_319, correct: (score * 1_319.0).round() as u64, wilson95_low: Some(score - 0.013), wilson95_high: Some(score + 0.012), file: format!("~/.local/state/lss/bench/{run}-accuracy-{model}/accuracy-gsm8k.json") }], ..Default::default() };
    model_a.accuracy = vec![accuracy(13, "3f9a1c07e2b4", "model-a", now - 2 * DAY, 0.9454)];
    (model_a.last_quick_at, model_a.last_full_at, model_a.last_accuracy_at) = (Some(now - 2 * 3_600), Some(now - 3 * DAY), Some(now - 2 * DAY));

    let mut model_b = card("b81e55d09c3a", "model-b", "model-b-serve:1.0", "tp 4 · ctx 524288 · quant fp8 · slots 8 · spec EAGLE · draft 4", false, now - 11 * DAY, now - 4 * DAY - 4_000, 151.2, &[(1.0, 150.0, 9_000), (2.0, 290.0, 2_000), (4.0, 560.0, 700), (8.0, 880.0, 60)], now);
    model_b.quick = Some(scorecard(9, "b81e55d09c3a", "model-b", "quick", now - 10 * DAY, 150.3, [150.3, 292.0, 566.0, 905.0], [5_720.0, 5_100.0, 4_400.0]));
    model_b.accuracy = vec![accuracy(10, "b81e55d09c3a", "model-b", now - 9 * DAY, 0.9212)];
    (model_b.last_quick_at, model_b.last_accuracy_at) = (Some(now - 10 * DAY), Some(now - 9 * DAY));

    model_a.headroom = Some(model_a.estimate_headroom(30.0));
    model_b.headroom = Some(model_b.estimate_headroom(30.0));
    let model_c = card("c4d2e9907f11", "model-c", "model-c-serve:1.0", "tp 4 · ctx 262144 · quant awq · slots 8", false, now - 19 * DAY, now - 11 * DAY - 900, 204.9, &[(1.0, 205.0, 7_000), (2.0, 366.0, 1_500), (4.0, 520.0, 300)], now);
    LoadoutsDoc { v: 1, generated_at: now, loadouts: vec![model_a, model_b, model_c] }
}

pub fn bench(now: i64) -> BenchDoc {
    let cards = loadouts(now);
    let mut runs: Vec<Scorecard> = cards.loadouts.iter().flat_map(|c| c.quick.iter().chain(c.full.iter()).chain(c.accuracy.iter()).cloned()).collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.run_id));
    let last = runs.first().map(LastRun::of);
    BenchDoc { v: 1, generated_at: now, brief: BenchBrief { state: "idle".into(), last, configured: true, ..Default::default() }, runs }
}

pub fn tokens(now: i64) -> TokensDoc {
    let w = |name: &str, secs: i64, generated: f64, prompt: f64, requests: f64| TokenWindow::new(name, secs, generated, prompt, prompt * 0.95, requests);
    TokensDoc {
        v: 1,
        generated_at: now,
        since: now - 19 * DAY,
        windows: vec![w("1h", 3_600, 96_400.0, 4.1e6, 212.0), w("24h", DAY, 1.43e6, 61.0e6, 2_310.0), w("7d", 7 * DAY, 9.8e6, 402.0e6, 15_900.0), w("today", 14 * 3_600, 0.92e6, 39.5e6, 1_480.0), w("all", 19 * DAY, 26.4e6, 1.09e9, 41_200.0)],
        all_time_since: now - 19 * DAY,
        peak: Some(Peak { tok_s: 912.0, ts: now - 10 * DAY + 3_000 }),
        peak_by_day: (0..14).map(|d| DayPeak { day: lss_core::timeutil::fmt_local(now - d * DAY, "%Y-%m-%d"), tok_s: (520.0 + noise(d as usize, 9) * 260.0).round(), ts: now - d * DAY - 5_000 }).collect(),
        output_len: cards_len(&[16.0, 64.0, 256.0, 1_024.0, 4_096.0, 16_384.0], &[2_000.0, 6_000.0, 14_000.0, 13_000.0, 5_600.0, 600.0, 0.0], 26.4e6),
        prompt_len: cards_len(&[100.0, 1_000.0, 4_000.0, 16_000.0, 65_536.0, 131_072.0, 524_288.0], &[900.0, 9_000.0, 14_000.0, 11_000.0, 4_800.0, 1_200.0, 300.0, 0.0], 812.0e6),
        per_user_available: true,
        per_user: lss_core::tokens::user_tokens(&status().users),
        gate_absent: false,
        tokens_not_reported: false,
    }
}

fn cards_len(le: &[f64], counts: &[f64], sum: f64) -> Option<lss_core::hist::RawSummary> {
    HistAccum { le: le.to_vec(), counts: counts.to_vec(), sum, count: counts.iter().sum() }.summary_raw()
}

pub fn advice(now: i64) -> AdviceDoc {
    let window = |name: &str, secs: i64, full: f64, queue: f64, wait: f64| WindowStats {
        name: name.into(),
        secs,
        covered_secs: secs.min(19 * DAY),
        slots: 8,
        slots_full_pct: Some(full),
        queue_pct: Some(queue),
        queue_wait_p95_ms: Some(wait),
        kv_peak_pct: Some(33.0),
        kv_p95_pct: Some(14.0),
        retracted_pct: Some(0.0),
        evicted_tokens: Some(92_160.0),
        evicted_secs: None,
        prompt_p50: Some(3_100.0),
        prompt_p95: Some(41_000.0),
        prompt_max: Some(490_000.0),
        prompt_max_is_bucket_edge: false,
        prompt_requests: Some(15_900.0),
        context_len: Some(1_048_576.0),
        cache_hit_pct: Some(95.0),
        spec_by_level: vec![(1, 5.9), (2, 5.6), (4, 5.1), (8, 3.9)],
        prefill_time_pct: Some(19.0),
        rejects: vec![LaneRejects { lane: "public".into(), too_busy_429: 38, too_large_413: 3 }, LaneRejects { lane: "trusted".into(), ..Default::default() }],
        growth: (name != "24h").then_some(Growth { tokens_per_day: 1.4e6, tokens_per_day_prev: 0.86e6, requests_per_day: 2_270.0, requests_per_day_prev: 1_510.0, peak_users: 6.0, peak_users_prev: 4.0, prev_days_covered: 7.0, peak_users_basis: None }),
        wh_per_mtok: Some(649.0),
        gen_tokens: Some(if name == "24h" { 1.43e6 } else { 9.8e6 }),
        throttle: vec![GpuThrottle { index: 0, pct: 38.0, excluded: true }, GpuThrottle { index: 1, pct: 0.0, excluded: false }, GpuThrottle { index: 2, pct: 1.4, excluded: false }, GpuThrottle { index: 3, pct: 0.0, excluded: false }],
        saturation: Some(Saturation::After { users: 5, total_tok_s: 650.0, from_bench: true }),
        // measured by `lss bench`: evidence enough for the WATCH
        saturation_levels: 4,
        saturation_from_bench: true,
        targets: Some(targets(name, if name == "24h" { 80.0 } else { 93.0 })),
    };
    lss_core::advice::build(now, vec![window("24h", DAY, 1.9, 4.0, 2_100.0), window("7d", 7 * DAY, 0.6, 14.0, 9_000.0), window("30d", 30 * DAY, 0.4, 11.0, 7_400.0)], &Default::default(), &[0])
}

/// A small hash -> 0..1, so curves wobble the same way on every run.
fn noise(i: usize, salt: u64) -> f64 {
    let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 29;
    (x % 10_000) as f64 / 10_000.0
}

/// 0..1 "how busy is the box" at point `i` of `n`: two waves of traffic and a quiet tail.
fn load_at(i: usize, n: usize) -> f64 {
    let t = i as f64 / n.max(1) as f64;
    let wave = ((t * 9.0).sin() * 0.5 + 0.5) * ((t * 2.3 + 0.7).sin() * 0.4 + 0.6);
    let burst = if (0.55..0.68).contains(&t) { 0.55 } else { 0.0 };
    (wave * 0.7 + burst + noise(i, 1) * 0.12).clamp(0.0, 1.0) * if t > 0.93 { 0.25 } else { 1.0 }
}

fn value(token: &str, i: usize, n: usize, salt: u64) -> Option<f64> {
    let l = load_at(i, n);
    let jig = noise(i, salt);
    let name = token.split(':').next().unwrap_or(token);
    let gpu = name.strip_prefix("gpu").and_then(|r| r.chars().next()).and_then(|c| c.to_digit(10)).map_or(0.0, f64::from);
    let latency = |base: f64, q: f64| Some(base * (1.0 + l * l * 3.0 * q) * (0.9 + jig * 0.2));
    // quiet minutes have no requests: no percentile, not a zero
    let busy = l > 0.12;
    match name {
        "decode_tok_s" => Some(l * 640.0 + if l > 0.05 { 60.0 * jig } else { 0.0 }),
        "prompt_tok_s" => Some(l * l * 5200.0 * (0.6 + jig)),
        // reading happens in bursts: most steps read nothing
        "prefill_tok_s" => (l > 0.2 && i % 3 != 1).then_some(3_600.0 + l * 2_400.0 + jig * 500.0),
        "tok_prefill" => Some((l * l * 0.5e6).round()),
        "sum_prefill_s" => Some(l * 40.0),
        "sum_e2e_s" => Some(l * 190.0 + 1.0),
        "running_public" => Some((l * 5.4).round()),
        "running_trusted" => Some((l * 2.6 * (0.5 + jig)).round()),
        "queue_public" => Some(((l - 0.72).max(0.0) * 30.0).round()),
        "queue_trusted" => Some(((l - 0.85).max(0.0) * 12.0).round()),
        "kv_usage" => Some(0.08 + l * 0.55),
        "cache_hit_rate" => Some(0.35 + 0.4 * (1.0 - l) + jig * 0.05),
        "spec_accept_length" => Some(3.3 - l * 0.6 + jig * 0.2),
        "spec_accept_rate" => Some(0.74 - l * 0.12 + jig * 0.03),
        "req_per_min" => Some((l * 46.0 + jig * 4.0).round()),
        // a probe every ~5 minutes: a reading when the box is quiet, invalid when it met traffic
        "c1_tok_s" => (i % 30 == 4 && l < 0.45).then_some(186.0 + jig * 8.0),
        "c1_invalid_tok_s" => (i % 30 == 4 && (0.45..0.7).contains(&l)).then_some(120.0 + jig * 40.0),
        "c1_invalid" => (i % 30 == 4 && l >= 0.45).then_some(1.0),
        "inflight_public" => Some((l * 182_000.0 * (0.7 + jig * 0.3)).round()),
        "inflight_trusted" => Some((l * 64_000.0 * jig).round()),
        "waiters_public" => Some(((l - 0.8).max(0.0) * 20.0).round()),
        "waiters_trusted" => Some(((l - 0.9).max(0.0) * 10.0).round()),
        "gate_requests" => Some((l * 4.0).round()),
        "users_active" => Some((l * 3.4).round()),
        "users_inflight" => Some((l * 7.2).round()),
        "tok_gen" => Some((l * 210_000.0 + jig * 20_000.0).round()),
        "tok_prompt" => Some((l * l * 9.0e6 + jig * 300_000.0).round()),
        "tok_cached" => Some((l * l * 8.5e6).round()),
        n if n.starts_with("user_inflight.") => {
            let who = salt % 5;
            Some((l * (4.2 - who as f64 * 0.9)).max(0.0).round())
        }
        n if n.starts_with("user_rpm.") => {
            let who = salt % 5;
            Some(((l * (26.0 - who as f64 * 5.0)).max(0.0) * (0.9 + jig * 0.2)).round())
        }
        "gpu_power_total_w" => Some(4.0 * (62.0 + l * 236.0)),
        "ttft_p50_ms" => busy.then(|| latency(190.0, 0.15)).flatten(),
        "ttft_p90_ms" => busy.then(|| latency(420.0, 0.5)).flatten(),
        "ttft_p99_ms" => busy.then(|| latency(900.0, 1.0)).flatten(),
        "ttft_avg_ms" => busy.then(|| latency(260.0, 0.3)).flatten(),
        "e2e_p50_ms" => busy.then(|| latency(3200.0, 0.2)).flatten(),
        "e2e_p90_ms" => busy.then(|| latency(9000.0, 0.4)).flatten(),
        "e2e_p99_ms" => busy.then(|| latency(21_000.0, 0.6)).flatten(),
        "e2e_avg_ms" => busy.then(|| latency(4600.0, 0.3)).flatten(),
        "itl_p50_ms" => busy.then(|| latency(4.6, 0.1)).flatten(),
        "itl_p90_ms" => busy.then(|| latency(7.2, 0.3)).flatten(),
        "itl_p99_ms" => busy.then(|| latency(15.0, 0.8)).flatten(),
        "itl_avg_ms" => busy.then(|| latency(5.1, 0.15)).flatten(),
        "queue_time_p50_ms" => busy.then(|| latency(0.8, 2.0)).flatten(),
        "queue_time_p90_ms" => busy.then(|| latency(6.0, 6.0)).flatten(),
        "queue_time_p99_ms" => busy.then(|| latency(45.0, 12.0)).flatten(),
        "queue_time_avg_ms" => busy.then(|| latency(3.0, 4.0)).flatten(),
        n if n.ends_with("_temp_c") => Some(44.0 + gpu * 2.5 + l * 27.0 + jig * 1.5),
        n if n.ends_with("_power_w") => Some(58.0 + gpu * 3.0 + l * 238.0 * (0.92 + jig * 0.08)),
        n if n.ends_with("_clock_mhz") => Some(if l < 0.05 { 600.0 + jig * 300.0 } else { 2900.0 - l * 900.0 - gpu * 20.0 - if token.ends_with(":min") { 250.0 } else { 0.0 } }),
        n if n.ends_with("_mem_util_pct") => Some(l * 58.0 + jig * 6.0),
        n if n.ends_with("_util_pct") => Some((l * 104.0).min(100.0) - jig * 4.0 * l),
        n if n.ends_with("_mem_used_mib") => Some(88_400.0 + gpu * 120.0 + l * 1400.0),
        n if n.ends_with("_thr_power") => Some(f64::from(u8::from(l > 0.78))),
        n if n.ends_with("_thr_thermal") || n.ends_with("_thr_hw") => Some(0.0),
        _ => None,
    }
}

pub fn series(tokens: &[String], range_idx: usize, min_step: Option<i64>, now: i64) -> SeriesDoc {
    let p = plan(now, range_secs(range_idx), min_step, KEEP);
    let mut b = SeriesBuilder::new(p, tokens);
    for (k, token) in tokens.iter().enumerate() {
        let name = token.split(':').next().unwrap_or(token).to_string();
        b.mark_known(&name);
        for i in 0..p.points {
            let v = if name.ends_with("_thr_thermal") {
                // GPU2 ran into its thermal limit for a few minutes during the burst
                let t = i as f64 / p.points as f64;
                Some(f64::from(u8::from(name.starts_with("gpu2") && (0.60..0.64).contains(&t))))
            } else {
                value(token, i, p.points, k as u64 + 7)
            };
            if let Some(v) = v {
                b.add(&name, p.start_ts + i as i64 * p.step_s, Agg::one(v));
            }
        }
    }
    b.finish(now)
}

fn hist(short: &str, range_idx: usize, now: i64) -> HistDoc {
    let range = range_secs(range_idx);
    let (_, step, start, cols) = hist_plan(now, range, 14 * DAY);
    // bucket layouts as SGLang publishes them
    let le: Vec<f64> = match short {
        "gen_tokens" => vec![16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16_384.0, 32_768.0],
        "prompt_tokens" => vec![100.0, 300.0, 1000.0, 3000.0, 10_000.0, 30_000.0, 100_000.0, 300_000.0, 1_000_000.0],
        "itl" => vec![0.002, 0.004, 0.006, 0.008, 0.01, 0.015, 0.02, 0.025, 0.03, 0.035, 0.04, 0.06, 0.08, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 2.0, 4.0, 6.0, 8.0],
        "queue_time" => vec![0.0, 0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0],
        "e2e" => vec![0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 2.0, 4.0, 6.0, 8.0, 10.0, 20.0, 40.0, 60.0, 80.0, 100.0, 200.0, 400.0, 800.0],
        _ => vec![0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 2.0, 4.0, 6.0, 8.0, 10.0, 20.0, 40.0, 60.0, 80.0, 100.0, 200.0, 400.0],
    };
    // where the mass sits when idle, and how far load pushes it up the buckets
    let (centre, spread): (f64, f64) = match short {
        "gen_tokens" => (5.0, 2.0),
        "prompt_tokens" => (3.0, 2.5),
        "itl" => (1.0, 2.0),
        "queue_time" => (1.0, 5.0),
        "e2e" => (6.0, 4.0),
        _ => (1.0, 5.0),
    };
    let mut total = HistAccum { le: le.clone(), counts: vec![0.0; le.len() + 1], ..Default::default() };
    let mut columns = Vec::with_capacity(cols);
    for c in 0..cols {
        let l = load_at(c, cols);
        if l <= 0.12 {
            columns.push(Vec::new());
            continue;
        }
        let peak = centre + l * l * spread;
        let counts: Vec<f64> = (0..=le.len()).map(|b| ((l * 60.0) * (-((b as f64 - peak).powi(2)) / 1.6).exp() * (0.8 + noise(c * 31 + b, 5) * 0.4)).round()).collect();
        let secs: f64 = counts.iter().enumerate().map(|(b, n)| n * le.get(b).copied().unwrap_or(le[le.len() - 1]) * 0.8).sum();
        total.add(&HistAccum { le: le.clone(), sum: secs, count: counts.iter().sum(), counts: counts.clone() });
        columns.push(counts);
    }
    HistDoc { v: 1, generated_at: now, metric: short.to_string(), range_s: range, step_s: step, start_ts: start, summary: lss_core::hist::latency_index(short).and_then(|_| total.summary()), unit: lss_core::hist::hist_unit(short).to_string(), summary_raw: total.summary_raw(), total: total.counts, le, columns }
}

pub fn gateway(range_idx: usize, now: i64) -> GatewayDoc {
    let mut doc: GatewayDoc = serde_json::from_str(include_str!("../../../fixtures/gateway_golden.json")).expect("the golden parses");
    doc.generated_at = now;
    doc.range_s = range_secs(range_idx);
    doc
}

/// The gateway tables as they look while part of the range predates per-key status codes:
/// `key-a` has a breakdown for some of its requests, `key-b` for none of them.
pub fn gateway_partial(range_idx: usize, now: i64) -> GatewayDoc {
    let mut doc = gateway(range_idx, now);
    doc.codes_coverage = "partial".into();
    doc.codes_since = Some(now - 3 * 3600 - now % 60);
    for k in &mut doc.keys {
        match k.name.as_str() {
            "key-a" => k.requests = 1920,
            "key-b" => {
                let bare = lss_core::series::GatewayRow::uncoded(&k.name, &k.lane, k.requests);
                *k = bare;
            }
            _ => {}
        }
    }
    doc
}

pub fn rules(now: i64) -> RulesDoc {
    let mut doc: RulesDoc = serde_json::from_str(include_str!("../../../fixtures/rules_golden.json")).expect("the golden parses");
    doc.generated_at = now;
    doc
}

/// A calm version of the rules document: nothing pending, nothing firing.
pub fn rules_calm(now: i64) -> RulesDoc {
    let mut doc = rules(now);
    for r in &mut doc.rules {
        r.state = "ok".into();
        r.pending_since = None;
    }
    doc.firing.clear();
    doc
}

pub fn page(status: &Status, page: PageId, range_idx: usize, now: i64) -> PageData {
    let mut d = PageData { page: Some(page), range_idx, fetched_at: now, ..Default::default() };
    let own = |t: &[&str]| -> Vec<String> { t.iter().map(|s| s.to_string()).collect() };
    match page {
        PageId::Latency => {
            d.series = Some(series(&latency_tokens(), range_idx, Some(60), now));
            d.hists = LATENCY_METRICS.iter().map(|(short, _)| hist(short, range_idx, now)).collect();
        }
        PageId::Load => d.series = Some(series(&own(&LOAD_TOKENS), range_idx, None, now)),
        PageId::Gpus => {
            let idx: Vec<u32> = status.gpus.iter().map(|g| g.sample.index).collect();
            d.series = Some(series(&gpu_tokens(&idx), range_idx, None, now));
        }
        PageId::Gateway => {
            d.series = Some(series(&own(&GATEWAY_TOKENS), range_idx, None, now));
            d.gateway = Some(gateway(range_idx, now));
        }
        // #231: ALERTS and INCIDENTS split into two pages but still read the one /rules document
        // (`page_paths`), so demo data comes from the same source for both.
        PageId::Alerts | PageId::Incidents => d.rules = Some(if status.firing.is_empty() { rules_calm(now) } else { rules(now) }),
        PageId::Users => d.series = Some(series(&crate::data::user_tokens(&PageCtx::of(status).user_ids), range_idx, None, now)),
        PageId::Tokens => {
            d.tokens = Some(tokens(now));
            let long = range_secs(range_idx) > DAY;
            d.series = Some(series(&own(&["tok_gen:sum", "tok_prompt:sum", "tok_cached:sum"]), if long { 4 } else { 3 }, Some(if long { 6 * 3_600 } else { 3_600 }), now));
            d.hists = vec![hist("gen_tokens", 3, now), hist("prompt_tokens", 3, now)];
        }
        PageId::Model => {
            d.loadouts = Some(loadouts(now));
            d.bench = Some(bench(now));
        }
        PageId::Advice => d.advice = Some(advice(now)),
    }
    d
}

/// #92: a plausible small fleet for `lss --demo` - the box on screen (mirrors `status()`'s own
/// numbers, so the two never disagree) plus two other nodes, one serving and one idle, so `f` has
/// something real to demo without a collector anywhere.
pub fn fleet(current: &Status) -> Vec<crate::ui::FleetEntry> {
    use crate::ui::{FleetEntry, FleetState};
    vec![
        FleetEntry {
            name: current.host.clone(),
            url: "demo".into(),
            state: Some(Ok(FleetState {
                up: current.serve.up,
                host: current.host.clone(),
                engine: current.serve.engine.clone(),
                model: current.serve.model.clone(),
                decode_tok_s: current.serve.decode_tok_s,
                prefill_tok_s: current.serve.prefill_tok_s,
                running: current.serve.running,
                slots: current.serve.slots,
                kv_usage: current.serve.kv_usage,
                gpu_count: current.gpus.len(),
                total_watts: (!current.gpus.is_empty()).then(|| current.gpus.iter().filter_map(|g| g.sample.power_w).sum()),
                cost_per_hour: current.cost.as_ref().and_then(|c| c.live_usd_per_hour),
                generated_tokens_total: current.serve.generation_tokens_total,
                not_reported: current.serve.not_reported.clone(),
            })),
        },
        FleetEntry {
            name: "spark-a".into(),
            url: "demo".into(),
            state: Some(Ok(FleetState {
                up: true,
                host: "spark-a".into(),
                engine: "sglang".into(),
                model: Some("model-b".into()),
                decode_tok_s: 96.0,
                prefill_tok_s: Some(1_450.0),
                running: 1.0,
                slots: 4,
                kv_usage: 0.18,
                gpu_count: 1,
                total_watts: Some(210.0),
                cost_per_hour: Some(0.09),
                generated_tokens_total: 82_000.0,
                not_reported: Vec::new(),
            })),
        },
        FleetEntry {
            name: "spark-b".into(),
            url: "demo".into(),
            state: Some(Ok(FleetState {
                up: false,
                host: "spark-b".into(),
                engine: "sglang".into(),
                model: None,
                decode_tok_s: 0.0,
                prefill_tok_s: None,
                running: 0.0,
                slots: 4,
                kv_usage: 0.0,
                gpu_count: 1,
                total_watts: Some(35.0),
                cost_per_hour: Some(0.015),
                generated_tokens_total: 0.0,
                not_reported: Vec::new(),
            })),
        },
    ]
}
