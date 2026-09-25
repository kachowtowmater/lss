//! card #279, ON THE WIRE. The unit tests in history.rs assert the two new fields on the typed
//! `Status`; these assert them on the SERIALISED document, by their dotted JSON path, because
//! that is what a client, a script and a renderer actually read. Phrased this way they compile
//! against a tree WITHOUT the fields, which is how they were proved RED on the parent (b345d79):
//! both came back `left: None`, the exact shape of #147's complaint - a number parsed by the
//! collector, carried nowhere.
use lss_core::history::{build_status, History, StatusInputs};
use lss_core::model::{Sample, Thresholds};
use lss_core::prom::ServeMetrics;

fn status_of(history: &History) -> lss_core::model::Status {
    build_status(&StatusInputs {
        now: 1_000, host: "gpu-box", collector_version: "0.1.0", collector_started_at: 0, poll_secs: 5, slots: 8,
        history, incidents: &[], alerts: &[], probes: &[], last_valid_probe: None, firing: vec![], serve_down_since: None,
        gate_down_since: None, restarts_today: 0, thermal_exclude: &[], probe_enabled: true, probe_interval_s: 300,
        c1_baseline: None, c1_baseline_source: String::new(),
        thresholds: Thresholds::new(&lss_core::config::RulesConfig::default(), None, 300),
        probes_admitted_since_gate_start: 0, latency: None, gpu_health: &[], user_aliases: &[], tailscale: &Default::default(),
        own_traffic: Default::default(), advice_top: Vec::new(), bench: Default::default(), maintenance: Default::default(),
        cost: None, spending: None, tokens_hour: None, tokens_day: None, tokens_week: None, tokens_month: None,
        tokens_hour_covered_secs: None, tokens_day_covered_secs: None, tokens_week_covered_secs: None, tokens_month_covered_secs: None,
        watch: None, loadout: None, prefill_typical: None, live_decode: None, public_priority: String::new(),
        trusted_priority: String::new(), targets: Default::default(), user_log_24h: None,
    })
}

#[test]
fn preemptions_reach_status_as_a_per_hour_rate() {
    let mut h = History::default();
    for (ts, r) in [(400_i64, 10.0_f64), (1_000, 40.0)] {
        h.push(Sample { ts, serve_up: true, metrics: Some(ServeMetrics { retracted: r, ..Default::default() }), ..Default::default() });
    }
    let v = serde_json::to_value(status_of(&h)).unwrap();
    assert_eq!(v["serve"]["preempted_per_hour_10m"].as_f64(), Some(180.0), "30 preemptions in 600 s is 180/hour on /status");
}

#[test]
fn the_gates_drain_estimate_reaches_status() {
    let mut h = History::default();
    h.push(Sample {
        ts: 1_000, serve_up: true, metrics: Some(ServeMetrics::default()),
        gate: Some(lss_core::gate::GateHealth {
            version: "v5.11".into(), upstream_ok: true,
            shadow: Some(serde_json::from_str(r#"{"enabled":true,"healthy":true,"seconds_to_drain_charged":12.5}"#).unwrap()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let v = serde_json::to_value(status_of(&h)).unwrap();
    assert_eq!(v["gate"]["seconds_to_drain_charged"].as_f64(), Some(12.5), "the gate's own drain estimate, carried onto /status");
}
