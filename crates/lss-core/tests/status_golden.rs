//! `/status` schema golden test. The document is built from the REAL fixtures with a fixed
//! clock and compared byte-for-byte with fixtures/status_golden.json - any change to the
//! schema shows up as a diff in review. Regenerate deliberately with:
//!     LSS_BLESS=1 cargo test -p lss-core --test status_golden

use lss_core::docker::ContainerState;
use lss_core::gate::parse_gate_health;
use lss_core::gatelog::ingest;
use lss_core::gpu::parse_gpu_csv;
use lss_core::history::{build_status, History, StatusInputs, SERIES_POINTS};
use lss_core::incidents::Incident;
use lss_core::config::RulesConfig;
use lss_core::model::{AlertRow, CostStatus, LoadoutBrief, Sample, Status, Thresholds, WorkWindow};
use lss_core::watch::{WatchItem, WatchSource, WatchStatus};
use lss_core::probe::ProbeRecord;
use lss_core::prom::extract_serve_metrics;

const NOW: i64 = 1_789_820_000;
const GATE_HEALTH: &str = r#"{"version": "v5", "upstreams": {"gpu-box": {"ok": true, "loaded": ["model-a"]}}, "admission": {"trusted": {"admitted": 9, "rejected_413": 1, "rejected_429": 0, "client_closed": 0, "upstream_down": 0, "max_duration": 0, "inflight_tokens": 0, "waiters": 0}, "public": {"admitted": 0, "rejected_413": 0, "rejected_429": 0, "client_closed": 0, "upstream_down": 0, "max_duration": 0, "inflight_tokens": 0, "waiters": 0}}}"#;

/// The 40 one-a-minute samples the golden documents are built from.
fn golden_samples() -> Vec<Sample> {
    let mut history = History::default();
    fill_history(&mut history);
    history.iter().cloned().collect()
}

pub fn golden_status() -> Status {
    let mut history = History::default();
    fill_history(&mut history);
    let mut s = golden_status_from(&history);
    // card #225 (#168's class, one level down): users.rows was EMPTY and
    // users.totals.inflight_by_upstream null, so the orphan-key guard's walker never descended
    // into rows[].by_upstream.* - a field added there would ship unguarded. The users block now
    // comes from a synthetic v5.10 gateway (#211's fixture: upstreams box-x / box-y, documentation
    // addresses, the bench taken out) so the baseline enumerates every key under it.
    let gate = lss_core::gate::parse_gate_health(include_str!("../../../fixtures/gate_health_v510.json")).expect("the v5.10 fixture parses");
    s.users = lss_core::users::build_users(Some(&gate), &[], &Default::default(), &Default::default(), NOW);
    // card #234 (#225's first known hole): serve.live_decode was null, so the walker recorded one
    // bare leaf and none of CurveRow's fields were guarded. A synthetic passive reading - 3 at
    // once, 480 samples - so the baseline enumerates every field; never a real measurement.
    s.serve.live_decode = Some(lss_core::loadout::CurveRow {
        running: 3,
        samples: 480,
        tok_s: Some(312.0),
        per_request_tok_s: Some(104.0),
        ttft_ms: Some(410.0),
        spec_accept_length: Some(3.2),
        source: "live".into(),
    });
    // card #235 (#225's second known hole): bench.last was null, so LastRun's fields - and its
    // Headline's - were one bare leaf. A synthetic finished run, every headline figure present;
    // `aborted` stays None because a finished run was not aborted (pinned as a scalar null).
    s.bench.last = Some(lss_core::bench::LastRun {
        run_id: 7,
        profile: "quick".into(),
        ended_at: NOW - 3_600,
        status: "ok".into(),
        aborted: None,
        model: "model-a".into(),
        headline: lss_core::bench::Headline {
            c1_tok_s: Some(188.0),
            ttft_c1_ms: Some(92.0),
            max_total_tok_s: Some(1_240.0),
            max_total_at: Some(16),
            prefill_8k_tok_s: Some(4_100.0),
            accuracy: Some(0.91),
            accuracy_dataset: Some("synthetic-set".into()),
        },
        load: lss_core::bench::LoadClass::Quiet,
        // card #338: one failed check, so the array's element type is pinned (an empty array
        // would be one bare leaf, #235's class of hole)
        failed_checks: vec!["json output".into()],
    });
    s
}

fn fill_history(history: &mut History) {
    let metrics = extract_serve_metrics(include_str!("../../../fixtures/sglang_metrics.txt"), "10", "0");
    let gpus = parse_gpu_csv(include_str!("../../../fixtures/nvidia_smi.csv"));
    let (log, _) = ingest(include_str!("../../../fixtures/gate_log.txt"), None);

    // 40 minutes of polls, one a minute, with a load bump in the middle
    for k in (0..40).rev() {
        let ts = NOW - k * 60;
        let mut m = metrics.clone();
        let busy = (10..20).contains(&k);
        if busy {
            m.running = 3.0;
            m.running_trusted = 2.0;
            m.running_public = 1.0;
            m.queue_trusted = 4.0;
            m.queue = 4.0;
            m.gen_throughput = 410.5;
            m.token_usage = 0.31;
        }
        // histogram counters move so the 10-minute averages have something to chew on
        m.ttft_count += (40 - k) as f64;
        m.ttft_sum += (40 - k) as f64 * 0.25;
        let mut g = gpus.clone();
        if busy {
            for gpu in &mut g {
                gpu.temp_c = gpu.temp_c.map(|t| t + 20.0);
            }
        }
        // #69: a real v5.4+ gate publishing card #31's budget_tokens, filling toward the cap
        // in the same busy window every other field bumps in - the golden's own worked example
        // of "see the budget fill before the rejections start".
        let mut gate = parse_gate_health(GATE_HEALTH);
        if let Some(gh) = &mut gate {
            gh.trusted.budget_tokens = Some(1_000_000);
            if busy {
                gh.trusted.inflight_tokens = 700_000 + (40 - k) as u64 * 8_000;
                gh.trusted.waiters = 2;
            }
        }
        history.push(Sample {
            ts,
            serve_up: true,
            model: Some("model-a".into()),
            metrics: Some(m),
            gpus_ok: true,
            gpus: g,
            serve_ct: Some(ContainerState { name: "model-a-sglang-sm120".into(), status: "running".into(), restart_count: 0, started_at: 1_789_817_789 }),
            gate_ct: Some(ContainerState { name: "the gateway".into(), status: "running".into(), restart_count: 0, started_at: 1_789_818_070 }),
            gate,
            log: if k == 3 { log.clone() } else { Default::default() },
            users: Vec::new(),
            slots: 8,
            ..Default::default()
        });
    }
}

fn win(usd: f64, kwh: f64, covered: i64, nominal: i64) -> lss_core::rates::SpendWindow {
    lss_core::rates::SpendWindow { usd: Some(usd), kwh: Some(kwh), covered_secs: covered, nominal_secs: nominal }
}

fn golden_status_from(history: &History) -> Status {

    let incidents = vec![
        Incident { id: 4, start: 1_789_817_789, end: Some(1_789_817_789), kind: "container_restart".into(), detail: "model-a-sglang-sm120 new StartedAt (previous run lasted 3d14h)".into() },
        Incident { id: 3, start: 1_789_817_400, end: Some(1_789_818_050), kind: "serve_down".into(), detail: "/v1/models: connection refused".into() },
        Incident { id: 2, start: 1_789_791_777, end: Some(1_789_791_777), kind: "xid".into(), detail: "GPU3 Xid 8 (GPU stopped processing (hang / watchdog)) pid=3177412, name=python3, channel 0x00000004".into() },
        Incident { id: 1, start: 1_789_679_648, end: Some(1_789_679_648), kind: "xid".into(), detail: "GPU1 Xid 8 (GPU stopped processing (hang / watchdog)) pid=569804, name=python3, channel 0x00000005".into() },
    ];
    let alerts = vec![
        AlertRow { id: 2, ts: 1_789_818_055, rule: "serve_down".into(), severity: "warn".into(), message: "RECOVERED: serve recovered after 10m50s down".into(), recovered: true, delivered: true },
        AlertRow { id: 1, ts: 1_789_817_525, rule: "serve_down".into(), severity: "warn".into(), message: "serve DOWN for 2m05s (/v1/models not answering)".into(), recovered: false, delivered: true },
    ];
    let probes = vec![
        // the newest probe collided with real traffic (the live case of 2026-09-19): stored, shown, never used
        ProbeRecord { ts: NOW - 60, status: "ok".into(), ttft_ms: Some(26548.4), decode_tok_s: Some(141.4), tokens: Some(128), detail: "TTFT 26.5 s > 3 s: the engine was prefilling someone else".into(), http_status: Some(200), valid: false, invalid_reason: Some("slow_ttft".into()) , unverified: false},
        ProbeRecord { ts: NOW - 120, status: "ok".into(), ttft_ms: Some(92.4), decode_tok_s: Some(191.7), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false},
        ProbeRecord::skipped_busy(NOW - 720, 3.0, 4.0),
        // a row from before http_status was recorded
        ProbeRecord { ts: NOW - 1320, status: "ok".into(), ttft_ms: Some(95.0), decode_tok_s: Some(190.2), tokens: Some(128), detail: String::new(), http_status: None, valid: true, invalid_reason: None , unverified: false},
    ];

    build_status(&StatusInputs {
        now: NOW,
        host: "gpu-box",
        collector_version: "0.1.0",
        collector_started_at: NOW - 86_400,
        poll_secs: 5,
        slots: 8,
        history,
        incidents: &incidents,
        alerts: &alerts,
        probes: &probes,
        last_valid_probe: None,
        firing: vec![],
        serve_down_since: None,
        gate_down_since: None,
        restarts_today: 1,
        thermal_exclude: &[0],
        probe_enabled: true,
        probe_interval_s: 300,
        c1_baseline: Some(190.5),
        c1_baseline_source: "learned".into(),
        thresholds: Thresholds::new(&RulesConfig::default(), Some(190.5), 300),
        probes_admitted_since_gate_start: 3,
        latency: Some(golden_latency()),
        user_aliases: &[],
        tailscale: &Default::default(),
        own_traffic: Default::default(),
        advice_top: Vec::new(),
        bench: Default::default(), maintenance: Default::default(),
        // card #168: cost/watch/loadout POPULATED with synthetic values, so the orphan-key
        // guard's walker descends into them (~25 fields join the baseline). A fake utility per
        // card #129's own convention - never this seat's real rate plan. card #172 (verifier-2,
        // re-verifying its own control): fixed_usd_per_day/unresolved_usd_per_kwh were STILL the
        // real plan, truncated one rounding step, under a
        // label saying "synthetic" - every privacy net reported clean, including #172's own
        // FIRST version, because its number-net rounded by DECIMAL PLACES and the leaked value needs
        // SIGNIFICANT-FIGURE rounding to be generated as a pattern. Every value below is now
        // invented outright (not derived from the real plan by any rounding), and chosen so no
        // 2-3-significant-figure form of it collides with the real plan's own values either.
        cost: Some(CostStatus {
            rate_name: "Test Utility TOU (synthetic, card #129 style)".into(),
            rate_source: String::new(), // card #298: empty = left out of the JSON, golden unchanged
            effective_date: "2026-01-01".into(),
            is_flat: false,
            current_usd_per_kwh: Some(0.30),
            current_period: "summer_off_peak".into(),
            live_usd_per_hour: Some(0.42),
            today_kwh: Some(9.6),
            today_usd: Some(3.87),
            today_usd_per_million_generated_tokens: Some(9.21),
            today_usd_per_million_prompt_tokens: Some(0.49),
            // card #174: the primary figure - work energy only, over uncached prefill + generated
            today_usd_per_million_real_work_tokens: Some(0.87),
            today_standby_kwh: Some(1.1),
            today_standby_usd: Some(0.44),
            fixed_usd_per_day: Some(1.25),
            unresolved_usd_per_kwh: Some(0.0050),
            last_24h_kwh: Some(24.3),
            last_24h_usd: Some(9.85),
            // card #232: a LITERAL, not `NOW - local_midnight(NOW)`. This fixture's own input was
            // TZ-DEPENDENT, so the golden file could only ever match one machine's timezone: the
            // build container runs UTC, where it comes out 44,000, and under
            // TZ=America/Los_Angeles the same fixture produces 18,800 - the single key that made
            // status_matches_the_golden_file the ONLY test in the workspace that changes result
            // with TZ. A golden exists to catch a SCHEMA change; deriving one of its values from
            // the host's clock zone made it catch the host instead. 44,000 keeps the committed
            // golden byte-identical (no re-bless), and it is now the same in every zone.
            // The real local-midnight behaviour is tested where it belongs, against the function
            // rather than against a fixture: cost_run's year_start_is_local_midnight_of_january_
            // first and the coverage tests in rates.rs.
            today_covered_secs: Some(44_000),
            last_24h_covered_secs: Some(86_400),
        }),
        // card #176: populated for the same reason the cost block is - a top-level subtree that
        // serialises to null fails #168's guard loudly, and the baseline must enumerate these
        // fields so a future one cannot appear unnoticed. Synthetic values, never a real bill.
        spending: Some(lss_core::rates::SpendingStatus {
            today: win(3.87, 9.6, 43_200, 43_200),
            yesterday: win(8.10, 20.1, 86_400, 86_400),
            this_week: win(24.30, 60.3, 3 * 86_400, 3 * 86_400),
            last_7d: win(56.70, 140.7, 7 * 86_400, 7 * 86_400),
            this_month: win(97.20, 241.2, 12 * 86_400, 12 * 86_400),
            last_30d: win(243.00, 603.0, 30 * 86_400, 30 * 86_400),
            // card #226: a partial year - the stored history starts well after Jan 1
            this_year: win(612.00, 1520.0, 45 * 86_400, 200 * 86_400),
            year_first_day: Some("2026-06-01".into()),
            daily: (0..3)
                .map(|i| lss_core::rates::DaySpend {
                    date: format!("2026-01-{:02}", i + 1),
                    ts: 1_000_000 - (1_000_000 % 86_400) - (2 - i) * 86_400,
                    usd: 8.10,
                    kwh: 20.1,
                    covered_secs: 86_400,
                })
                .collect(),
            month_projection_usd: Some(243.00),
            projection_note: "projected from 12 days at this rate".into(),
        }),
        tokens_hour: Some(WorkWindow { prompt: 210_000.0, cached: 198_000.0, generated: 42_000.0, requests: 61.0 }),
        tokens_day: Some(WorkWindow { prompt: 5_040_000.0, cached: 4_752_000.0, generated: 1_008_000.0, requests: 1_464.0 }),
        tokens_week: Some(WorkWindow { prompt: 35_280_000.0, cached: 33_264_000.0, generated: 7_056_000.0, requests: 10_248.0 }),
        tokens_month: Some(WorkWindow { prompt: 151_200_000.0, cached: 142_560_000.0, generated: 30_240_000.0, requests: 43_920.0 }),
        tokens_hour_covered_secs: Some(3_600),
        tokens_day_covered_secs: Some(86_400),
        tokens_week_covered_secs: Some(7 * 86_400),
        tokens_month_covered_secs: Some(30 * 86_400),
        watch: Some(WatchStatus {
            sources: vec![WatchSource {
                name: "Example Recipe Repo (synthetic)".into(),
                covers: "GLM loadouts on the GPU box (synthetic)".into(),
                last_checked: Some(NOW - 5_400),
                newest: Some(WatchItem {
                    date: "2026-09-20".into(),
                    summary: "swapped the chat template for a fixed one from upstream (synthetic)".into(),
                    url: "https://example.com/releases/v1.2.3".into(),
                    has_receipt: true,
                }),
            }],
        }),
        loadout: Some(LoadoutBrief {
            id: "3f9a1c07e2b4".into(),
            model: "model-a".into(),
            image_tag: "1.0".into(),
            first_seen: NOW - 4 * 86_400,
            runs: 2,
            flags: "tp 4 · ctx 1048576 · quant fp4 (synthetic)".into(),
        }),
        prefill_typical: None, live_decode: None, public_priority: String::new(), trusted_priority: String::new(),
        targets: Default::default(), user_log_24h: None,
        gpu_health: &lss_core::gpu::parse_gpu_health_csv(include_str!("../../../fixtures/nvidia_smi_health.csv"), NOW - 20),
    })
}

/// Ten minutes of latency: the REAL scrape as the starting point, then a known number of
/// requests added to known buckets, so the percentiles in the golden can be checked by hand.
fn golden_latency() -> lss_core::hist::LatencyNow {
    use lss_core::hist::{extract_latency, HistSet, LatencyTracker};
    let base = extract_latency(&lss_core::prom::parse(include_str!("../../../fixtures/sglang_metrics.txt")));
    // (bucket index, observations) added per metric between the two scrapes
    let added: [&[(usize, f64)]; 4] = [&[(1, 60.0), (3, 30.0), (6, 9.0), (9, 1.0)], &[(6, 50.0), (8, 40.0), (11, 10.0)], &[(1, 800.0), (2, 150.0), (5, 50.0)], &[(1, 90.0), (4, 10.0)]];
    let mut later: HistSet = base.clone();
    for (h, adds) in later.iter_mut().zip(added) {
        let h = h.as_mut().expect("the fixture has all four histograms");
        for (bucket, n) in adds {
            for c in h.cum.iter_mut().skip(*bucket) {
                *c += n;
            }
            let upper = h.le.get(*bucket).copied().unwrap_or(0.0);
            h.sum += n * upper * 0.75;
            h.count += n;
        }
    }
    let mut t = LatencyTracker::default();
    t.observe(NOW - 125, Some(&base));
    t.observe(NOW - 120, Some(&later));
    t.observe(NOW, Some(&later));
    t.now()
}

#[test]
fn status_matches_the_golden_file() {
    let status = golden_status();
    let rendered = serde_json::to_string_pretty(&status).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/status_golden.json");
    if std::env::var_os("LSS_BLESS").is_some() {
        std::fs::write(path, &rendered).unwrap();
    }
    let golden = std::fs::read_to_string(path).expect("fixtures/status_golden.json missing: run with LSS_BLESS=1");
    assert_eq!(rendered, golden, "the /status schema changed: review the diff, update docs/STATUS-JSON.md, re-bless");
}

#[test]
fn schema_v1_contract() {
    let v: serde_json::Value = serde_json::to_value(golden_status()).unwrap();
    assert_eq!(v["v"], 1);
    for key in ["host", "generated_at", "collector", "serve", "gate", "gpus", "lanes", "series", "incidents", "alerts", "firing", "probe", "thresholds", "users"] {
        assert!(!v[key].is_null(), "top-level key `{key}` missing");
    }
    for key in ["up", "model", "uptime_s", "restart_count", "restarts_today", "running", "slots", "queue", "decode_tok_s", "kv_usage", "ttft_avg_ms_10m", "spec_accept_length", "cache_hit_rate"] {
        assert!(v["serve"].get(key).is_some(), "serve.{key} missing");
    }
    for lane in ["public", "trusted"] {
        for key in ["running", "queued", "inflight_tokens", "waiters", "codes_10m", "top_keys_60m", "rejected_413", "rejected_429", "probes"] {
            assert!(v["lanes"][lane].get(key).is_some(), "lanes.{lane}.{key} missing");
        }
        for class in ["2xx", "4xx", "5xx"] {
            assert!(v["lanes"][lane]["codes_10m"][class].is_u64());
        }
    }
    for key in ["admitted", "requests_10m", "requests_60m"] {
        assert!(v["lanes"]["trusted"]["probes"][key].is_u64(), "lanes.trusted.probes.{key} missing");
    }
    assert_eq!(v["thresholds"]["c1_ratio"], 0.8);
    assert_eq!(v["thresholds"]["c1_baseline_tok_s"], 190.5);
    assert_eq!(v["thresholds"]["c1_floor_tok_s"], 152.4);
    assert_eq!(v["thresholds"]["queue_reqs"], 24.0);
    assert_eq!(v["thresholds"]["thermal_temp_c"], 90.0);
    // GPU rows are flat: sample fields and the decoded flags side by side
    assert_eq!(v["gpus"].as_array().unwrap().len(), 4);
    assert_eq!(v["gpus"][0]["index"], 0);
    assert_eq!(v["gpus"][0]["thermal_excluded"], true);
    assert_eq!(v["gpus"][1]["thermal_excluded"], false);
    assert!(v["gpus"][0]["temp_c"].is_number());

    let series = &v["series"];
    assert_eq!(series["points"], SERIES_POINTS);
    assert_eq!(series["step_s"], 30);
    for key in ["decode_tok_s", "running", "queue_public", "queue_trusted", "kv_usage"] {
        assert_eq!(series[key].as_array().unwrap().len(), SERIES_POINTS, "series.{key} length");
    }
    assert_eq!(series["gpu_temp_c"].as_array().unwrap().len(), 4);
    assert_eq!(series["gpu_temp_c"][0].as_array().unwrap().len(), SERIES_POINTS);
}

#[test]
fn derived_values_are_right() {
    let s = golden_status();
    assert!(s.serve.up);
    assert_eq!(s.serve.model.as_deref(), Some("model-a"));
    assert_eq!(s.serve.uptime_s, Some(NOW - 1_789_817_789));
    assert_eq!(s.serve.slots, 8);
    // 10 samples in the last 10 min, +1 request and +0.25 s each → 250 ms average TTFT
    assert_eq!(s.serve.ttft_avg_ms_10m, Some(250.0));
    assert_eq!(s.serve.itl_avg_ms_10m, None, "no ITL movement in the window");
    // the gate-log delta sits 3 minutes back: inside both windows
    assert_eq!(s.lanes.public.requests_10m, 24);
    assert_eq!((s.lanes.public.codes_10m.c2xx, s.lanes.public.codes_10m.c4xx, s.lanes.public.codes_10m.c5xx), (14, 10, 0));
    assert_eq!(s.lanes.trusted.codes_10m.c5xx, 6);
    assert_eq!(s.lanes.public.top_keys_60m[0].key, "key-a");
    assert_eq!(s.lanes.public.top_keys_60m[0].count, 14);
    assert_eq!(s.lanes.trusted.rejected_413, 1);
    // The collector's own probes are taken OUT of the trusted lane and shown on their own.
    // Fixture: 18 trusted POSTs (9 x 2xx, 3 x 4xx, 6 x 5xx) and 9 admitted, of which the probe
    // made 2 inside the last 10 min, 3 inside 60 min and 3 since the gate started. An INVALID
    // probe was still a request: it is subtracted like any other.
    let t = &s.lanes.trusted;
    assert_eq!((t.probes.admitted, t.probes.requests_10m, t.probes.requests_60m), (3, 2, 3));
    assert_eq!(t.admitted, 6);
    assert_eq!(t.requests_10m, 16);
    assert_eq!((t.codes_10m.c2xx, t.codes_10m.c4xx, t.codes_10m.c5xx), (7, 3, 6));
    assert_eq!(t.top_keys_60m[0].key, "(no key)");
    assert_eq!(t.top_keys_60m[0].count, 15);
    assert_eq!(s.lanes.public.probes, Default::default(), "the probe never uses the public lane");
    assert_eq!(s.lanes.public.admitted, 0);
    // series: the load bump is visible, the first 20 minutes are empty
    assert!(s.series.decode_tok_s[0].is_none());
    assert_eq!(s.series.decode_tok_s.iter().flatten().fold(0.0_f64, |a, b| a.max(*b)), 410.5);
    assert_eq!(s.series.queue_trusted.iter().flatten().fold(0.0_f64, |a, b| a.max(*b)), 4.0);
    assert_eq!(s.series.gpu_temp_c[0].iter().flatten().fold(0.0_f64, |a, b| a.max(*b)), 72.0);
    // the C1 reading is the newest VALID probe, not the newest one
    assert_eq!(s.probe.history[0].decode_tok_s, Some(141.4));
    assert!(!s.probe.history[0].valid);
    assert_eq!(s.probe.last_ok.as_ref().unwrap().decode_tok_s, Some(191.7));
    assert_eq!(s.probe.invalid_skipped, 1);
    assert_eq!(s.probe.history.len(), 4);
    assert_eq!(s.thresholds.c1_max_ttft_s, 3.0);
}

#[test]
fn round_trips_and_tolerates_unknown_and_missing_fields() {
    let s = golden_status();
    let json = serde_json::to_string(&s).unwrap();
    let back: Status = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
    // a future collector adds fields, an old one lacks some: the client must cope with both
    let sparse: Status = serde_json::from_str(r#"{"v":1,"host":"x","serve":{"up":true,"brand_new":1},"future":{}}"#).unwrap();
    assert!(sparse.serve.up);
    assert!(sparse.gpus.is_empty());
    assert_eq!(sparse.thresholds.c1_ratio, 0.8, "an older collector sends no thresholds: the built-in defaults apply");
}

#[test]
fn empty_history_still_builds_a_valid_document() {
    let history = History::default();
    let s = build_status(&StatusInputs {
        now: NOW, host: "gpu-box", collector_version: "0.1.0", collector_started_at: NOW, poll_secs: 5, slots: 8,
        history: &history, incidents: &[], alerts: &[], probes: &[], last_valid_probe: None, firing: vec![], serve_down_since: None,
        gate_down_since: None, restarts_today: 0, thermal_exclude: &[0], probe_enabled: true, probe_interval_s: 300,
        c1_baseline: None, c1_baseline_source: "learning 0/12".into(),
        thresholds: Thresholds::new(&RulesConfig::default(), None, 300), probes_admitted_since_gate_start: 5, latency: None, gpu_health: &[], user_aliases: &[], tailscale: &Default::default(), own_traffic: Default::default(), advice_top: Vec::new(), bench: Default::default(), maintenance: Default::default(), cost: None, spending: None, tokens_hour: None, tokens_day: None, tokens_week: None, tokens_month: None, tokens_hour_covered_secs: None, tokens_day_covered_secs: None, tokens_week_covered_secs: None, tokens_month_covered_secs: None, watch: None, loadout: None, prefill_typical: None, live_decode: None, public_priority: String::new(), trusted_priority: String::new(), targets: Default::default(), user_log_24h: None,
    });
    assert_eq!(s.lanes.trusted.probes.admitted, 0, "nothing to subtract from: never below zero");
    assert_eq!(s.thresholds.c1_floor_tok_s, None);
    assert_eq!(s.v, 1);
    assert!(!s.serve.up);
    assert_eq!(s.series.decode_tok_s.len(), SERIES_POINTS);
    assert!(s.collector.last_sample_ts.is_none());
}

#[test]
fn the_last_valid_reading_survives_a_long_run_of_invalid_probes() {
    let history = History::default();
    let old_valid = ProbeRecord { ts: NOW - 20_000, status: "ok".into(), ttft_ms: Some(130.0), decode_tok_s: Some(189.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
    let mut bad = old_valid.clone();
    bad.ts = NOW - 30;
    bad.decode_tok_s = Some(120.0);
    bad.invalidate("contended", "the gate admitted 1 other request(s) during the probe".into());
    let recent = vec![bad, ProbeRecord::skipped_busy(NOW - 330, 2.0, 0.0)];
    let s = build_status(&StatusInputs {
        now: NOW, host: "gpu-box", collector_version: "0.1.0", collector_started_at: NOW, poll_secs: 5, slots: 8,
        history: &history, incidents: &[], alerts: &[], probes: &recent, last_valid_probe: Some(&old_valid), firing: vec![], serve_down_since: None,
        gate_down_since: None, restarts_today: 0, thermal_exclude: &[0], probe_enabled: true, probe_interval_s: 300,
        c1_baseline: Some(190.0), c1_baseline_source: "learned".into(),
        thresholds: Thresholds::new(&RulesConfig::default(), Some(190.0), 300), probes_admitted_since_gate_start: 0, latency: None, gpu_health: &[], user_aliases: &[], tailscale: &Default::default(), own_traffic: Default::default(), advice_top: Vec::new(), bench: Default::default(), maintenance: Default::default(), cost: None, spending: None, tokens_hour: None, tokens_day: None, tokens_week: None, tokens_month: None, tokens_hour_covered_secs: None, tokens_day_covered_secs: None, tokens_week_covered_secs: None, tokens_month_covered_secs: None, watch: None, loadout: None, prefill_typical: None, live_decode: None, public_priority: String::new(), trusted_priority: String::new(), targets: Default::default(), user_log_24h: None,
    });
    assert_eq!(s.probe.last_ok.as_ref().unwrap().decode_tok_s, Some(189.0), "nothing valid in the recent history: the older valid reading stays");
    assert_eq!(s.probe.invalid_skipped, 2);
    let metrics = lss_core::promout::render(&s);
    assert!(metrics.contains("llm_serve_c1_decode_tokens_per_second{host=\"gpu-box\"} 189"), "{metrics}");
    assert!(metrics.contains("llm_serve_c1_probe_age_seconds{host=\"gpu-box\"} 20000"), "{metrics}");
}

/// Byte-for-byte goldens of the history API documents (`GET /series`, `GET /rules`,
/// `GET /gateway`), blessed together with the `/status` one.
fn check_golden(name: &str, rendered: String) {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("LSS_BLESS").is_some() {
        std::fs::write(&path, &rendered).unwrap();
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("fixtures/{name} missing: run with LSS_BLESS=1"));
    assert_eq!(rendered, golden, "the shape of {name} changed: review the diff, update docs/STATUS-JSON.md, re-bless");
}

#[test]
fn series_document_matches_its_golden() {
    use lss_core::series::{plan, sample_points, Agg, SeriesBuilder};
    let keep = (86_400, 14 * 86_400, 90 * 86_400);
    let p = plan(NOW, 600, Some(60), keep);
    let tokens: Vec<String> = ["decode_tok_s", "queue", "queue:avg", "gpu0_temp_c", "ttft_p99_ms", "never_recorded"].iter().map(|s| s.to_string()).collect();
    let mut b = SeriesBuilder::new(p, &tokens);
    let status_inputs = golden_samples();
    let mut prev: Option<&Sample> = None;
    for s in &status_inputs {
        for (name, v) in sample_points(prev, s) {
            b.add(&name, s.ts, Agg::one(v));
        }
        prev = Some(s);
    }
    b.add("ttft_p99_ms", NOW - 120, Agg::from_row(2000.0, 2000.0, 2000.0, 100.0));
    check_golden("series_golden.json", serde_json::to_string_pretty(&b.finish(NOW)).unwrap() + "\n");
}

#[test]
fn rules_document_matches_its_golden() {
    use lss_core::rules::{Engine, GpuObs, Observation};
    use lss_core::series::{uptime, RulesDoc};
    let cfg = RulesConfig { thermal_exclude: vec![0], ..Default::default() };
    let mut engine = Engine::default();
    // ten minutes of polls: GPU2 runs hot (pending, then firing), the queue is over the line for a minute (pending)
    for k in 0..=132 {
        let now = NOW - 660 + k * 5;
        let obs = Observation {
            now,
            serve_up: true,
            gate_up: true,
            queue: Some(if k >= 120 { 30.0 } else { 2.0 }),
            trusted_waiters: Some(0),
            rejected_413: Some(1),
            rejected_429: Some(0),
            gpus: Some((0..4).map(|i| GpuObs { index: i, temp_c: Some(if i == 2 { 93.0 } else { 61.0 }), throttle_mask: 0 }).collect()),
            probe_tok_s: (k == 100).then_some(191.7),
            ..Default::default()
        };
        engine.evaluate(&cfg, &obs);
    }
    let status = golden_status();
    let doc = RulesDoc {
        v: 1,
        generated_at: NOW,
        rules: engine.rule_states(&cfg, NOW),
        firing: engine.firing(NOW),
        spool_depth: Some(2),
        alerts: status.alerts.clone(),
        incidents: status.incidents.clone(),
        uptime: uptime(NOW, NOW - 3 * 86_400, &status.incidents),
    };
    let state = |rule: &str| doc.rules.iter().find(|r| r.rule == rule).map(|r| r.state.clone()).unwrap_or_default();
    assert_eq!(state("thermal_temp:gpu2"), "firing");
    assert_eq!(state("queue_pressure"), "pending");
    assert_eq!(state("serve_down"), "ok");
    assert_eq!(doc.firing, vec!["thermal_temp:gpu2"]);
    check_golden("rules_golden.json", serde_json::to_string_pretty(&doc).unwrap() + "\n");
}

#[test]
fn gateway_document_matches_its_golden() {
    let (log, _) = ingest(include_str!("../../../fixtures/gate_log.txt"), None);
    let mut coverage = lss_core::series::CodeCoverage::default();
    coverage.observe(NOW - 600, &log);
    let doc = lss_core::series::build_gateway(NOW, 3600, &log, &[(NOW - 900, 200), (NOW - 600, 200), (NOW - 300, 200)], &coverage, Some("127.0.0.x"));
    check_golden("gateway_golden.json", serde_json::to_string_pretty(&doc).unwrap() + "\n");
}

/// Additive compatibility, both ways: the `/status` of the collector BEFORE users, tokens and
/// loadouts existed still loads in this build (everything new is simply not available), and a
/// document from this build still gives an older reader every key it knew.
#[test]
fn status_stays_v1_and_additive_with_users() {
    let now = golden_status();
    assert_eq!(now.v, 1);
    // card #225: the golden's users come from a synthetic v5.10 gateway, so they ARE available
    assert!(now.users.available && !now.users.rows.is_empty(), "the golden carries user rows (#225)");
    let mut v: serde_json::Value = serde_json::to_value(&now).unwrap();
    // what a collector from before this round sent
    let obj = v.as_object_mut().unwrap();
    obj.remove("users");
    for added in ["advice_top", "bench", "loadout"] {
        assert!(obj.remove(added).is_some(), "{added} is in today's /status");
    }
    obj["serve"].as_object_mut().unwrap().remove("cached_tokens_total");
    obj["serve"].as_object_mut().unwrap().remove("context_len");
    let old: Status = serde_json::from_value(v).unwrap();
    assert_eq!((old.users.available, old.users.rows.len(), old.serve.context_len), (false, 0, 0.0));
    assert_eq!((old.advice_top.len(), old.bench.state.as_str(), old.loadout.is_none()), (0, "", true), "an older collector simply has none of it");
    assert_eq!(old.serve.decode_tok_s, now.serve.decode_tok_s);
    // a v5.2 gate: the users appear, nothing else moves
    let gate = parse_gate_health(include_str!("../../../fixtures/gate_health_v52.json")).unwrap();
    let users = lss_core::users::build_users(Some(&gate), &[], &Default::default(), &Default::default(), NOW);
    let mut with = now.clone();
    with.users = users;
    let json = serde_json::to_value(&with).unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!((json["users"]["available"].as_bool(), json["users"]["totals"]["users_24h"].as_u64(), json["users"]["rows"][0]["name"].as_str()), (Some(true), Some(5), Some("acme")));
    for key in ["inflight", "peak_inflight_10m", "peak_inflight_24h", "conc_limit", "rpm_limit", "rpm_now", "requests_1h", "requests_24h", "ok_24h", "rejected_24h", "errors_24h", "client_closed_24h", "prompt_tokens_est_24h", "completion_tokens_24h", "completion_tokens_exact", "last_seen", "series_id", "lane", "user", "by_upstream"] {
        assert!(json["users"]["rows"][0].get(key).is_some(), "users.rows[].{key} missing");
    }
    // card #211: a v5.2 gate cannot split by machine - the key is there and says so with null
    assert_eq!(json["users"]["totals"].get("inflight_by_upstream"), Some(&serde_json::Value::Null));
    let mut stripped = json.clone();
    stripped.as_object_mut().unwrap().remove("users");
    let mut base = serde_json::to_value(&now).unwrap();
    base.as_object_mut().unwrap().remove("users");
    assert_eq!(stripped, base, "users are purely additive");
}


/// card #281: the page-1 trend grid's seven series (DESIGN §S5) are on /status, on the SAME grid
/// as `decode_tok_s` - and a series with nothing behind it is all `null`, never zeros. Read as
/// JSON, the way a client reads it, so it holds on the wire and not only on the Rust type.
#[test]
fn status_carries_the_seven_trend_series_on_the_hour_grid_and_null_is_not_zero() {
    let v = serde_json::to_value(golden_status()).unwrap();
    let series = &v["series"];
    let points = series["points"].as_u64().unwrap() as usize;
    assert_eq!(points, series["decode_tok_s"].as_array().unwrap().len());
    for key in ["ttft_p99_ms", "itl_p99_ms", "prefix_hit", "spec_accept_rate", "refused", "gpu_power_w", "usd_per_hour"] {
        let arr = series[key].as_array().unwrap_or_else(|| panic!("series.{key} is missing from /status"));
        assert_eq!(arr.len(), points, "series.{key} is on the same grid as decode_tok_s");
    }
    // the golden history has GPU power, a gate log and a speculative engine in its last 20 min...
    let non_null = |key: &str| series[key].as_array().unwrap().iter().filter(|x| !x.is_null()).count();
    for key in ["gpu_power_w", "spec_accept_rate", "refused"] {
        assert!(non_null(key) > 0, "series.{key} has data where the history does");
    }
    // ...and no histogram rollups or rates table (lss-core has neither): those stay null, never 0
    for key in ["ttft_p99_ms", "itl_p99_ms", "usd_per_hour"] {
        assert_eq!(non_null(key), 0, "series.{key} is the collector's to fill; lss-core must not invent it");
    }
}

/// card #147 (Chip Huyen, #124's panel): "the panel's output was accepted as DATA requirements
/// with no RENDER CONTRACT, so collection and display had separate owners and no gate." Four
/// fields (admission_verdict, secs_since_last_token, kv_free, the TTFT split) were built,
/// plumbed onto /status and rendered by nothing for weeks before an auditor found them. This is
/// the gate: every leaf key of a fully-populated /status document must be a KNOWN key (checked
/// into `fixtures/status_keys_baseline.txt`, one dotted path per line, `LSS_BLESS=1` rewrites
/// it, the same convention every other golden fixture in this repo already uses); a key that is
/// new, renamed, or removed FAILS LOUDLY here, by design, instead of drifting silently.
///
/// THE CHEAP VERSION, deliberately: this does not re-derive "is X actually read by a renderer"
/// on every run (a real static-analysis check, and a much bigger thing to build and keep green).
/// EVERY baseline key is trusted RENDERED/ALERTED by default - verified ONCE, mechanically
/// (verifier-2, this card's own history: extracted every `pub <field>:` from model.rs, grepped
/// crates/lss/src, rules.rs, promout.rs and http.rs for each one; 13 of 168 had no consumer
/// anywhere). The `EXCEPTIONS` map below overrides that default ONLY for those already-audited
/// keys plus the panel's own named seed (the TTFT split) - QUERY_ONLY (a human or a script reads
/// it, never a screen) or ORPHAN (a known, tracked gap - a card number, not just a shrug), each
/// with a one-line reason verifier-2's own rule demands: never write QUERY_ONLY without saying
/// who reads it and why. A field that changes consumer (gains or loses its only reader) without
/// changing its NAME, or a brand-new field nobody adds to `EXCEPTIONS`, will not be caught by
/// the "trusted by default" half of this design - a real, accepted limit of the cheap version,
/// not an oversight. What IS caught, always: the baseline diff, which is what actually fires the
/// moment anyone adds, renames or removes a key without a deliberate re-bless.
mod orphan_fields {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Class {
        /// a human or a script reads it from /status directly - never a screen. Reason required.
        QueryOnly,
        /// a known, tracked gap: computed and shipped, consumed by nothing yet. Reason (which
        /// names the follow-up card) required - "known" is not the same as "acceptable".
        Orphan,
    }

    /// Overrides to the "every baseline key is Rendered/Alerted" default. Keyed by the same
    /// dotted leaf path `leaf_paths` produces (array elements collapsed to `field[]`).
    fn exceptions() -> Vec<(&'static str, Class, &'static str)> {
        vec![
            // verifier-2, #147's own mechanical audit (grepped every `pub <field>:` in model.rs
            // against crates/lss/src, rules.rs, promout.rs, http.rs) - TRUE ORPHANS: computed on
            // every poll, serialised into every /status document, read by nobody. Filed as #150.
            ("thresholds.thermal_temp_f", Class::Orphan, "a Fahrenheit variant of thermal_temp_c (which rules.rs DOES use) with no reader anywhere - #150"),
            ("gate.container_status", Class::Orphan, "the gateway container's raw status string; restart_count/restarts_today tracking is what's actually rendered - #150"),
            // genuinely QUERY_ONLY: consumed by the gateway/collector's own admission logic or a
            // script reading /status directly - never by a renderer or a rule.
            ("lanes.trusted.max_prompt_tokens", Class::QueryOnly, "read by the gateway/collector to explain a 413, not displayed as a bare number"),
            ("lanes.public.max_prompt_tokens", Class::QueryOnly, "same as lanes.trusted.max_prompt_tokens"),
            ("lanes.trusted.max_duration", Class::QueryOnly, "a gate-internal admission parameter; not a live status fact a screen reports"),
            ("lanes.public.max_duration", Class::QueryOnly, "same as lanes.trusted.max_duration"),
            ("lanes.trusted.probes.requests_60m", Class::QueryOnly, "a rolling input to the probe classifier's own arithmetic, not shown standalone"),
            ("lanes.public.probes.requests_60m", Class::QueryOnly, "same as lanes.trusted.probes.requests_60m"),
            // card #147 item 3: the TTFT split (queue-wait + prefill-compute), the panel's own
            // named seed - post-hoc analysis, the field stays published on purpose, not live.
            // Forward-declared: `golden_status()`'s own latency fixture does not populate
            // queue_time (its histogram carries zero counts, so every summary field is None and
            // skip_serializing_if omits the whole object) - these entries are inert until a
            // fixture or a live box actually reports it, which is correct: an absent Option is
            // not a rendering gap to classify, only a present-and-unrendered field is.
            ("serve.latency.queue_time.p50_ms", Class::QueryOnly, "TTFT decomposed into queue-wait + prefill-compute: post-hoc analysis per the #51 panel, kept published on purpose, not rendered live"),
            ("serve.latency.queue_time.p90_ms", Class::QueryOnly, "same as p50_ms"),
            ("serve.latency.queue_time.p99_ms", Class::QueryOnly, "same as p50_ms"),
            ("serve.latency.queue_time.avg_ms", Class::QueryOnly, "same as p50_ms"),
            ("serve.latency.queue_time.count", Class::QueryOnly, "same as p50_ms"),
            // card #211: the gate's v5.10 per-machine split of totals.inflight. What a SCREEN
            // shows per machine comes from the users_active.<upstream> / _here series; this field
            // is for a script or the budget work (#210) asking "how much is in flight on which box"
            ("users.totals.inflight_by_upstream.box-x", Class::QueryOnly, "per-machine split of totals.inflight for scripts and the budget work; the screen reads the per-machine SERIES instead"),
            ("users.totals.inflight_by_upstream.box-y", Class::QueryOnly, "same as users.totals.inflight_by_upstream.box-x"),
            // card #225: now that the golden carries user rows (#211's synthetic v5.10 gateway), each
            // user's per-machine split is enumerated. No screen renders it per user - the users page
            // and page 1 read the per-machine SERIES the collector derives from it (#211)
            ("users.rows[].by_upstream.box-y.inflight", Class::QueryOnly, "one user's in-flight on one machine; screens read the users_active.<upstream> series instead (#211)"),
            ("users.rows[].by_upstream.box-y.requests_24h", Class::QueryOnly, "same as users.rows[].by_upstream.box-y.inflight"),
            ("users.rows[].by_upstream.box-y.peak_inflight_10m", Class::QueryOnly, "same as users.rows[].by_upstream.box-y.inflight"),
            ("users.rows[].by_upstream.box-y.peak_inflight_24h", Class::QueryOnly, "same as users.rows[].by_upstream.box-y.inflight"),
            ("users.bench.by_upstream.box-x.inflight", Class::QueryOnly, "the benchmark's own per-machine split; it is taken OUT of every total, never shown as a user"),
            ("users.bench.by_upstream.box-x.requests_24h", Class::QueryOnly, "same as users.bench.by_upstream.box-x.inflight"),
            ("users.bench.by_upstream.box-x.peak_inflight_10m", Class::QueryOnly, "same as users.bench.by_upstream.box-x.inflight"),
            ("users.bench.by_upstream.box-x.peak_inflight_24h", Class::QueryOnly, "same as users.bench.by_upstream.box-x.inflight"),
            // cards #279/#281: published on /status (and `lss status --json`) with NO TUI reader. Their
            // renderer was the page-1/SERVING redesign (#273/#282), which the owner SHELVED on
            // 2026-09-24 ("i like the way lss loads right now"). They stay as JSON-only data, declared
            // here so #147 does not read them as drift; a future renderer deletes the matching line.
            ("serve.preempted_per_hour_10m", Class::Orphan, "requests pushed out of the running batch, per hour; JSON-only (the SERVING redesign that rendered it is shelved)"),
            ("gate.seconds_to_drain_charged", Class::Orphan, "the gate's own drain estimate for its in-flight charged tokens; JSON-only (renderer shelved)"),
            ("series.ttft_p99_ms", Class::Orphan, "1h series; JSON-only (the page-1 trend grid that read it is shelved)"),
            ("series.itl_p99_ms", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
            ("series.prefix_hit", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
            ("series.spec_accept_rate", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
            ("series.refused", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
            ("series.gpu_power_w", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
            ("series.usd_per_hour", Class::Orphan, "1h series; JSON-only (renderer shelved)"),
        ]
    }

    /// Walks a `serde_json::Value`, collecting one dotted path per LEAF (a value with no further
    /// object/array-of-objects to descend into). An array of objects (`gpus`, `incidents`, ...)
    /// contributes ONE path per field name, from its first element only, with `[]` marking the
    /// collapse - classification is per field, not per element; an array of plain values (a
    /// series like `decode_tok_s: [f64]`) is itself the leaf.
    fn leaf_paths(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, vv) in map {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    leaf_paths(vv, &p, out);
                }
            }
            serde_json::Value::Array(items) => match items.first() {
                Some(first @ serde_json::Value::Object(_)) => leaf_paths(first, &format!("{prefix}[]"), out),
                _ => out.push(prefix.to_string()),
            },
            _ => out.push(prefix.to_string()),
        }
    }

    fn baseline_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/status_keys_baseline.txt")
    }

    #[test]
    fn every_status_key_is_a_known_baseline_key_with_exceptions_reasoned() {
        let status = golden_status();
        let json = serde_json::to_value(&status).unwrap();
        let mut paths = Vec::new();
        leaf_paths(&json, "", &mut paths);
        paths.sort();
        paths.dedup();

        let path = baseline_path();
        if std::env::var_os("LSS_BLESS").is_some() {
            std::fs::write(&path, paths.join("\n") + "\n").unwrap();
        }
        let baseline_text = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("fixtures/status_keys_baseline.txt missing: run with LSS_BLESS=1"));
        let baseline: Vec<&str> = baseline_text.lines().filter(|l| !l.is_empty()).collect();

        // card #168 item 4: a null subtree is the walker's BLIND SPOT - leaf_paths records a
        // bare key and never descends, so any struct added under it would ship unguarded. The
        // #168 hole (cost/watch/loadout all null) is closed by POPULATING the fixture; this
        // assertion makes the next Option<...Status> that lands null a loud failure instead
        // of another quiet hole.
        // card #168 item 4, precisely scoped: a TOP-LEVEL subtree that serializes to null is
        // the hole (leaf_paths records it as one bare leaf; everything under it is unguarded).
        // A scalar Option field that is legitimately None mid-tree is a leaf either way and is
        // NOT a hole - the baseline already carries it as a path.
        let null_tops: Vec<&String> = match json.as_object() {
            Some(map) => map.iter().filter(|(_, v)| v.is_null()).map(|(k, _)| k).collect(),
            None => Vec::new(),
        };
        assert!(null_tops.is_empty(), "#168: the golden fixture leaves top-level subtree(s) NULL - the orphan guard cannot see inside them (leaf_paths treats null as one bare leaf): {:?}. Populate them with synthetic values in golden_status_from() and re-bless.", null_tops);

        // card #225, #168 extended one level down: a NESTED null is the same blind spot when the
        // field is a whole struct (the walker records one bare leaf and never descends) - users.rows
        // was exactly that until this card. JSON null cannot tell a None struct from a None scalar,
        // so the set is PINNED: every nested null the golden carries is listed with what it is. A
        // NEW one fails here and forces the choice - a scalar that is legitimately None (add it),
        // or a struct that must be POPULATED in the fixture so its fields are guarded.
        let scalar_nulls = [
            "serve.down_since", "serve.itl_avg_ms_10m", "serve.queue_time_avg_ms_10m", "serve.prefill_tok_s",
            "serve.prefill_tok_s_typical", "serve.cached_share_10m", "serve.secs_since_last_token", "gate.down_since",
            "gpus[].health.retired_sbe", "gpus[].health.retired_dbe", "gpus[].health.retired_pending",
            "lanes.public.budget_tokens", "lanes.public.max_prompt_tokens", "lanes.trusted.max_prompt_tokens",
            "probe.last_ok.invalid_reason", "users.rows[].conc_limit", "users.rows[].rpm_limit",
            "users.bench.conc_limit", "users.bench.rpm_limit", "users.bench.last_status",
            "bench.profile", "bench.started_at", "bench.step",
            "bench.last.aborted", // card #235: a finished run was not aborted - legitimately None
            // card #279: a SCALAR that is legitimately None here - fixtures/gate_health_v510.json
            // is a real v5.10 capture with no `shadow` block at all, so the gate published no
            // drain estimate to carry. The wiring itself is pinned where it can be, against the
            // parse and against build_status: gate.rs's
            // the_shadow_blocks_drain_estimate_is_parsed_and_its_absence_is_tolerated and
            // history.rs's seconds_to_drain_charged_reaches_status_from_the_gates_shadow_block.
            "gate.seconds_to_drain_charged",
        ];
        // KNOWN HOLES, named so they cannot be forgotten: whole structs the golden leaves null, so
        // nothing under them is guarded yet. Populating them is follow-up work (card #225's note).
        // serve.live_decode (card #234) and bench.last (card #235) are populated now: no known holes
        let known_struct_holes: [&str; 0] = [];
        fn nested_nulls(v: &serde_json::Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, vv) in map {
                        let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        nested_nulls(vv, &p, depth + 1, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    if let Some(first @ serde_json::Value::Object(_)) = items.first() {
                        nested_nulls(first, &format!("{prefix}[]"), depth, out);
                    }
                }
                serde_json::Value::Null if depth >= 2 => out.push(prefix.to_string()),
                _ => {}
            }
        }
        let mut nested = Vec::new();
        nested_nulls(&json, "", 0, &mut nested);
        let unclassified: Vec<&String> = nested.iter().filter(|p| !scalar_nulls.contains(&p.as_str()) && !known_struct_holes.contains(&p.as_str())).collect();
        assert!(unclassified.is_empty(), "#225: NEW nested null(s) in the golden - a None struct here is an unguarded subtree. Populate it in the fixture, or (if it is a scalar that is legitimately None) add it to scalar_nulls: {unclassified:?}");

        let added: Vec<&String> = paths.iter().filter(|p| !baseline.contains(&p.as_str())).collect();
        let removed: Vec<&&str> = baseline.iter().filter(|b| !paths.iter().any(|p| p == *b)).collect();
        assert!(
            added.is_empty() && removed.is_empty(),
            "#147: /status's key set no longer matches fixtures/status_keys_baseline.txt - a key              is new, renamed, or removed. Classify any NEW key first (Rendered/Alerted by              default, or add it to `exceptions()` as QUERY_ONLY/Orphan with a reason), THEN              re-bless (LSS_BUILD_HOST=... scripts/remote-build.sh bless).
added:
{}
removed:
{}",
            added.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n"),
            removed.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n"),
        );

        // every exception must carry a real reason - an empty one defeats the "who reads
        // this, and why" discipline verifier-2's own audit established. Membership in the
        // CURRENT baseline is deliberately not required: a forward-declared exception (the
        // TTFT split, absent from this specific fixture - see `exceptions()`'s own comment) is
        // harmless until the field is actually present, and requiring presence would make this
        // test's own fixture gaps block a classification decision that is already correct.
        for (key, _, reason) in exceptions() {
            assert!(!reason.trim().is_empty(), "{key}: QUERY_ONLY/Orphan with no reason");
        }
    }

    /// card #225's proof: a field planted UNDER by_upstream (and a new upstream name in the
    /// totals split) is a new key the baseline diff catches - before this card the walker never
    /// descended there, because users.rows was empty and inflight_by_upstream was null.
    #[test]
    fn a_new_field_under_by_upstream_fails_the_baseline_diff() {
        let status = golden_status();
        let mut json = serde_json::to_value(&status).unwrap();
        json["users"]["rows"][0]["by_upstream"]["box-y"]["planted_field_225"] = serde_json::json!(1);
        json["users"]["totals"]["inflight_by_upstream"]["box-z"] = serde_json::json!(1);
        let mut paths = Vec::new();
        leaf_paths(&json, "", &mut paths);
        let baseline_text = std::fs::read_to_string(baseline_path()).unwrap();
        let baseline: Vec<&str> = baseline_text.lines().collect();
        for planted in ["users.rows[].by_upstream.box-y.planted_field_225", "users.totals.inflight_by_upstream.box-z"] {
            assert!(paths.iter().any(|p| p == planted), "the walker must reach {planted}");
            assert!(!baseline.contains(&planted), "{planted} must NOT already be in the baseline - the diff has to see it as new");
        }
        // and the golden's real per-machine keys ARE in the baseline (the walker reached them)
        assert!(baseline.contains(&"users.rows[].by_upstream.box-y.inflight") && baseline.contains(&"users.totals.inflight_by_upstream.box-x"));
    }

    /// card #234's proof (lss-verifier-3's plant, as JSON): a field that appears inside
    /// serve.live_decode (a CurveRow) is reached by the walker and is NOT in the baseline, so the
    /// diff fails - before #234 the golden left live_decode null and the walker never descended.
    #[test]
    fn a_new_field_in_live_decode_fails_the_baseline_diff() {
        let mut json = serde_json::to_value(golden_status()).unwrap();
        json["serve"]["live_decode"]["planted_in_a_known_hole"] = serde_json::json!(1);
        let mut paths = Vec::new();
        leaf_paths(&json, "", &mut paths);
        let baseline_text = std::fs::read_to_string(baseline_path()).unwrap();
        let baseline: Vec<&str> = baseline_text.lines().collect();
        let planted = "serve.live_decode.planted_in_a_known_hole";
        assert!(paths.iter().any(|p| p == planted), "the walker must reach {planted}");
        assert!(!baseline.contains(&planted), "{planted} must be NEW to the baseline");
        for f in ["running", "samples", "tok_s", "per_request_tok_s", "ttft_ms", "spec_accept_length", "source"] {
            assert!(baseline.contains(&format!("serve.live_decode.{f}").as_str()), "serve.live_decode.{f} must be guarded");
        }
    }

    /// card #235's proof (lss-verifier-3's plant, as JSON): a key that appears inside bench.last
    /// (a LastRun) or its headline is reached by the walker and is NEW to the baseline.
    #[test]
    fn a_new_field_in_bench_last_fails_the_baseline_diff() {
        let mut json = serde_json::to_value(golden_status()).unwrap();
        json["bench"]["last"]["planted_in_a_known_hole"] = serde_json::json!(1);
        json["bench"]["last"]["headline"]["planted_in_a_known_hole"] = serde_json::json!(1);
        let mut paths = Vec::new();
        leaf_paths(&json, "", &mut paths);
        let baseline_text = std::fs::read_to_string(baseline_path()).unwrap();
        let baseline: Vec<&str> = baseline_text.lines().collect();
        for planted in ["bench.last.planted_in_a_known_hole", "bench.last.headline.planted_in_a_known_hole"] {
            assert!(paths.iter().any(|p| p == planted), "the walker must reach {planted}");
            assert!(!baseline.contains(&planted), "{planted} must be NEW to the baseline");
        }
        for f in ["run_id", "profile", "ended_at", "status", "aborted", "model", "load", "failed_checks", "headline.c1_tok_s", "headline.max_total_at", "headline.accuracy_dataset"] {
            assert!(baseline.contains(&format!("bench.last.{f}").as_str()), "bench.last.{f} must be guarded");
        }
    }

    /// #147 item 2, the acceptance test's own teeth: a genuinely new key must fail - checked
    /// directly rather than trusted - a baseline diff that silently accepted anything would be
    /// worse than no gate at all.
    #[test]
    fn a_brand_new_key_fails_the_baseline_diff() {
        let status = golden_status();
        let mut json = serde_json::to_value(&status).unwrap();
        json.as_object_mut().unwrap().insert("totally_new_field_147".into(), serde_json::json!(1));
        let mut paths = Vec::new();
        leaf_paths(&json, "", &mut paths);
        let baseline_text = std::fs::read_to_string(baseline_path()).unwrap();
        let baseline: Vec<&str> = baseline_text.lines().collect();
        assert!(paths.iter().any(|p| p == "totally_new_field_147" && !baseline.contains(&p.as_str())), "the planted new key must not already be in the baseline");
    }
}

/// card #171 (from #144 item 5, verifier-2): the ORPHAN-KEY guard (`orphan_fields` above) makes
/// sure a new `/status` field gets CLASSIFIED. It says nothing about whether a reader can look
/// the field up - seven fields shipped, undocumented, unnoticed, because nothing checked. This
/// module is the doc-side sibling: every shipped `/status` key must appear somewhere in
/// docs/STATUS-JSON.md (in the tables that document it, or in the surrounding prose), or be
/// named in `exceptions()` with a reason - the same "a gap is a decision, not an accident" shape
/// `orphan_fields::exceptions()` already established, other direction.
mod doc_coverage {
    use std::collections::BTreeSet;

    fn doc_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/STATUS-JSON.md")
    }

    fn baseline_text() -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/status_keys_baseline.txt")).unwrap()
    }

    /// A shipped key documented nowhere, and the reason it is allowed to stay that way for now.
    /// Empty today (#171 landed with the doc caught up) - a future gap belongs here, reasoned,
    /// not silently tolerated.
    fn exceptions() -> Vec<(&'static str, &'static str)> {
        // card #225: the golden now carries a v5.10 gateway's split, whose last path segment is
        // an upstream's NAME - data from the gateway's own config (the fixture's box-x / box-y),
        // not a schema key any doc could name. The field itself, `inflight_by_upstream`, IS
        // documented in the users `totals` row.
        vec![
            ("users.totals.inflight_by_upstream.box-x", "a map keyed by upstream NAME (data, not schema); the field inflight_by_upstream is documented in the users totals row"),
            ("users.totals.inflight_by_upstream.box-y", "same as users.totals.inflight_by_upstream.box-x"),
        ]
    }

    /// docs/STATUS-JSON.md documents SIX endpoints under separate `# ` (H1) headings, but only
    /// `/status` is pinned by fixtures/status_keys_baseline.txt - a diff that ignores this flags
    /// every `/gateway` and `/tokens` field as an orphan (verifier-2's first attempt: 298 phantom
    /// findings). The organisational trap on top of that: `## \`users\`` and every
    /// `## Added 2026-09-xx` section are ALL /status additions, but they sit physically nested
    /// under OTHER endpoints' `# ` headings (mostly `# GET /health`) later in the file, because
    /// that is where they were pasted in as the schema grew. Both facts are handled here by
    /// keying off the H2 heading text itself, not the file's physical H1 nesting.
    ///
    /// Returns the dotted prefix(es) a BARE key in that section's rows expands to. `""` means
    /// "the key is already written fully-qualified in this section's own convention" (true of
    /// `## Top level` and every `## Added ...` section, which paste in complete dotted paths
    /// like `serve.prefill_tok_s` rather than a bare `prefill_tok_s`). `None` means the section
    /// is out of /status scope entirely and its lines are not scanned.
    fn section_prefixes(heading: &str) -> Option<Vec<&'static str>> {
        match heading {
            "Top level" => Some(vec![""]),
            "`serve`" => Some(vec!["serve."]),
            "`gate`" => Some(vec!["gate."]),
            "`gpus[]`" => Some(vec!["gpus[]."]),
            // one heading, two lanes: a bare key here is documented if EITHER expands to a real
            // shipped key - the doc describes the shared shape once, not per lane.
            "`lanes.public` / `lanes.trusted`" => Some(vec!["lanes.public.", "lanes.trusted."]),
            "`series`" => Some(vec!["series."]),
            "`incidents[]`" => Some(vec!["incidents[]."]),
            "`alerts[]`" => Some(vec!["alerts[]."]),
            "`probe`" => Some(vec!["probe."]),
            "`thresholds`" => Some(vec!["thresholds."]),
            h if h.contains("users") => Some(vec!["users."]),
            h if h.starts_with("Added ") => Some(vec![""]),
            _ => None,
        }
    }

    /// One backtick-delimited span's content. A plain prose span ("run this command") is
    /// dropped - it has whitespace and no `{`, so it cannot be one of the doc's two list forms
    /// below and is never a single field name either. A span with EITHER form is kept, spaces
    /// and all; `extract_idents` cleans it up.
    fn spans(line: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while let Some(rel) = line[i..].find('`') {
            let start = i + rel + 1;
            let Some(end_rel) = line.get(start..).and_then(|s| s.find('`')) else { break };
            let end = start + end_rel;
            let content = &line[start..end];
            let has_whitespace = content.chars().any(char::is_whitespace);
            if !content.is_empty() && (content.contains('{') || content.contains('"') || !has_whitespace) {
                out.push(content);
            }
            i = end + 1;
        }
        out
    }

    /// A span's identifier candidate(s). Three shapes, in order:
    /// - a JSON-quoted example, `` `{"2xx","4xx","5xx"}` `` - pull the QUOTED atoms out
    ///   ("2xx", "4xx", "5xx"), ignoring the surrounding `{}`/JSON syntax entirely;
    /// - the doc's own brace shorthand for several sibling keys at once, e.g.
    ///   `tokens_by_window.{hour,day,week,month}_covered_secs` or `{admitted, requests_10m,
    ///   requests_60m}` - one level, no nesting, alternatives trimmed of the doc's own spacing;
    /// - a plain single identifier, used as-is. A leading `.` (the doc's own continuation
    ///   shorthand - "`watch.sources[].name`, `.covers`" means the second span is
    ///   `watch.sources[].covers`) is stripped by the caller, not here.
    fn extract_idents(span: &str) -> Vec<String> {
        // `[{key,count}]` (an array-of-objects value shape, e.g. `top_keys_60m` = `[{key,count}]`)
        // - strip the outer array brackets so the brace form below sees a plain `{key,count}`
        // and does not fold them into the alternatives as literal `[` / `]` characters.
        let span = span.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(span);
        if span.contains('"') {
            let mut out = Vec::new();
            let mut i = 0usize;
            while let Some(rel) = span[i..].find('"') {
                let start = i + rel + 1;
                let Some(end_rel) = span.get(start..).and_then(|s| s.find('"')) else { break };
                out.push(span[start..start + end_rel].to_string());
                i = start + end_rel + 1;
            }
            return out;
        }
        if let (Some(b), Some(e)) = (span.find('{'), span.rfind('}')) {
            if e > b {
                let (pre, rest) = (&span[..b], &span[b + 1..e]);
                let post = &span[e + 1..];
                return rest.split(',').map(|a| format!("{pre}{}{post}", a.trim())).collect();
            }
        }
        vec![span.to_string()]
    }

    /// Every dotted `/status` key form findable in the doc, scoped to the sections
    /// `section_prefixes` recognises. Extra, spurious entries this over-generates (a value-shape
    /// example like `` `{p50_ms, p90_ms, ...}` `` expands into bare `p50_ms` etc.) are harmless:
    /// the only check this feeds is "is a REAL baseline key present in this set", and a garbage
    /// string coincidentally matching a real dotted path is not a realistic risk.
    fn documented_keys() -> BTreeSet<String> {
        let text = std::fs::read_to_string(doc_path()).unwrap();
        let mut in_status_h1 = false;
        let mut prefixes: Option<Vec<&'static str>> = None;
        let mut out = BTreeSet::new();
        for line in text.lines() {
            if let Some(h1) = line.strip_prefix("# ") {
                in_status_h1 = h1.trim() == "`GET /status`";
                prefixes = if in_status_h1 { Some(vec![""]) } else { None };
                continue;
            }
            if let Some(h2) = line.strip_prefix("## ") {
                let h2 = h2.trim();
                prefixes = section_prefixes(h2).or(if in_status_h1 { Some(vec![""]) } else { None });
                continue;
            }
            let Some(pfx) = &prefixes else { continue };
            for span in spans(line) {
                for ident in extract_idents(span) {
                    let ident = ident.trim_start_matches('.');
                    if ident.is_empty() {
                        continue;
                    }
                    // the fully-qualified form, per this section's own prefix convention...
                    for p in pfx {
                        out.insert(format!("{p}{ident}"));
                    }
                    // ...AND the bare leaf, unprefixed (trailing `[]` dropped: an array-of-
                    // objects field the golden fixture happens to ship EMPTY records as one bare
                    // baseline leaf with no `[]`, same as the #168 null-subtree class but for an
                    // empty Vec rather than an Option::None). Several composite objects
                    // (`collector`, `bench`, `gpus[].health`, `probe.last_ok`, ...) are documented
                    // as a PROSE list of their sub-field names next to the object's own row, one
                    // nesting level deeper than any `section_prefixes` entry reaches -
                    // `gpus[].health`'s fields are named in a sentence inside the
                    // `## \`gpus[]\`` section, so they land here as bare `clock_max_mhz` rather
                    // than the full `gpus[].health.clock_max_mhz`. The full-path check is the
                    // primary signal; this bare form is the fallback that makes those genuinely-
                    // written sentences count as documentation instead of a parser gap.
                    out.insert(ident.trim_end_matches("[]").to_string());
                }
            }
        }
        out
    }

    #[test]
    fn every_shipped_status_key_is_documented_or_excepted() {
        let baseline_text = baseline_text();
        let baseline: Vec<&str> = baseline_text.lines().filter(|l| !l.is_empty()).collect();
        let documented = documented_keys();
        let excepted: BTreeSet<&str> = exceptions().iter().map(|(k, _)| *k).collect();

        fn leaf(k: &str) -> &str {
            k.rsplit('.').next().unwrap_or(k).trim_end_matches("[]")
        }
        let missing: Vec<&&str> = baseline.iter()
            .filter(|k| !documented.contains(**k) && !documented.contains(leaf(k)) && !excepted.contains(*k))
            .collect();
        assert!(missing.is_empty(),
            "#171: shipped /status key(s) documented nowhere in docs/STATUS-JSON.md and not in doc_coverage::exceptions():\n  {}\nDocument it there, or add (\"key\", \"reason\") to exceptions().",
            missing.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("\n  "));

        for (key, reason) in exceptions() {
            assert!(!reason.trim().is_empty(), "{key}: undocumented with no reason in doc_coverage::exceptions()");
        }
    }

    /// #171 item 3, the test's own teeth: a genuinely new, undocumented key must fail - checked
    /// directly, not trusted, matching `orphan_fields::a_brand_new_key_fails_the_baseline_diff`'s
    /// own reasoning for existing.
    #[test]
    fn a_new_undocumented_key_is_not_already_covered() {
        let documented = documented_keys();
        assert!(!documented.contains("totally_new_undocumented_field_171"), "the planted key must not already read as documented");
    }

    /// The three /status keys #171 found genuinely documented only in prose under `## \`series\``
    /// (`step_s`, `start_ts`, `points` - never a table row) must still read as covered, proving
    /// this scanner is not table-row-only in a way that would falsely flag them.
    #[test]
    fn prose_documented_series_fields_are_not_false_gaps() {
        let documented = documented_keys();
        for k in ["series.step_s", "series.start_ts", "series.points"] {
            assert!(documented.contains(k), "{k} is documented only in prose under ## `series` and must still read as covered");
        }
    }
}
