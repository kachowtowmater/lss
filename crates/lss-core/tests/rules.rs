//! Rule engine under a fake clock. Nothing here sleeps: `Sim` advances `now` by hand.

use lss_core::config::RulesConfig;
use lss_core::probe::{c1_reading, validate, ProbeEvidence, ProbeRecord};
use lss_core::rules::{AlertEvent, Engine, GpuObs, Observation, RestartObs, Severity};
use lss_core::xid::XidEvent;

struct Sim {
    cfg: RulesConfig,
    engine: Engine,
    now: i64,
    log: Vec<AlertEvent>,
}

fn healthy(now: i64) -> Observation {
    Observation {
        now,
        serve_up: true,
        gate_up: true,
        queue: Some(0.0),
        trusted_waiters: Some(0),
        rejected_413: Some(0),
        rejected_429: Some(0),
        gpus: Some((0..4).map(|i| GpuObs { index: i, temp_c: Some(60.0), throttle_mask: 0 }).collect()),
        running_roles: vec!["serve".into(), "gate".into()],
        probe_interval_secs: 300,
        ..Default::default()
    }
}

impl Sim {
    fn new() -> Self {
        // the scenario these tests were written for: GPU0 sits next to the exhaust and is excluded.
        // #47, 2026-09-21: c1_stale_probe_intervals defaults to a number no test here runs long
        // enough to reach, so every test not specifically about C1 staleness is unaffected by
        // that rule existing - tests that ARE about it (below) dial this down explicitly, the
        // same way other tests dial in `c1_baseline_tok_s`.
        Self { cfg: RulesConfig { thermal_exclude: vec![0], c1_stale_probe_intervals: 1_000_000, ..Default::default() }, engine: Engine::default(), now: 1_000_000, log: Vec::new() }
    }

    /// Runs `secs` of wall time in 5 s polls, building each observation with `f`.
    fn run(&mut self, secs: i64, f: impl Fn(&mut Observation)) -> Vec<AlertEvent> {
        let mut fired = Vec::new();
        let end = self.now + secs;
        while self.now < end {
            self.now += 5;
            let mut o = healthy(self.now);
            f(&mut o);
            fired.extend(self.engine.evaluate(&self.cfg, &o));
        }
        self.log.extend(fired.clone());
        fired
    }

    fn once(&mut self, f: impl Fn(&mut Observation)) -> Vec<AlertEvent> {
        self.run(5, f)
    }
}

fn rules(events: &[AlertEvent]) -> Vec<(&str, bool)> {
    events.iter().map(|e| (e.rule.as_str(), e.recovered)).collect()
}

#[test]
fn healthy_system_is_silent() {
    let mut s = Sim::new();
    assert!(s.run(3 * 3600, |_| {}).is_empty());
    assert!(s.engine.firing(s.now).is_empty());
}

#[test]
fn serve_down_warns_after_120s_pages_after_900s_and_recovers_once() {
    let mut s = Sim::new();
    assert!(s.run(115, |o| o.serve_up = false).is_empty(), "under 120 s: silent");
    let a = s.run(10, |o| o.serve_up = false);
    assert_eq!(rules(&a), vec![("serve_down", false)]);
    assert_eq!(a[0].severity, Severity::Warn);

    assert!(s.run(770, |o| o.serve_up = false).is_empty(), "no repeat while it stays down");
    let a = s.run(10, |o| o.serve_up = false);
    assert_eq!(rules(&a), vec![("serve_down_page", false)]);
    assert_eq!(a[0].severity, Severity::Page);
    assert!(s.run(3600, |o| o.serve_up = false).is_empty(), "page is sent once");

    let a = s.once(|_| {});
    assert_eq!(rules(&a), vec![("serve_down", true)]);
    assert!(a[0].message.starts_with("RECOVERED: serve recovered after 1h"), "{}", a[0].message);
    assert!(s.run(600, |_| {}).is_empty());
}

#[test]
fn a_short_outage_never_alerts_and_never_recovers() {
    let mut s = Sim::new();
    assert!(s.run(60, |o| o.serve_up = false).is_empty());
    assert!(s.run(600, |_| {}).is_empty(), "no recovered message for an alert that was never sent");
}

#[test]
fn cooldown_holds_back_a_flapping_rule_then_lets_it_through() {
    let mut s = Sim::new();
    let a = s.run(125, |o| o.serve_up = false);
    assert_eq!(rules(&a), vec![("serve_down", false)]);
    let a = s.run(60, |_| {});
    assert_eq!(rules(&a), vec![("serve_down", true)]);

    // down again 1 minute later: hold time is met at +120 s but the 30 min cooldown is not
    assert!(s.run(600, |o| o.serve_up = false).is_empty(), "inside cooldown: held back");
    assert!(s.run(60, |_| {}).is_empty(), "the held-back alert was never sent, so no recovery either");

    // still-true-when-cooldown-ends is delivered
    let a = s.run(1800, |o| o.serve_up = false);
    assert!(rules(&a).contains(&("serve_down", false)), "{a:?}");
    let sent_at = a.iter().find(|e| e.rule == "serve_down").unwrap().ts;
    let first = s.log[0].ts;
    assert!(sent_at - first >= 1800, "second alert {sent_at} must be >= cooldown after first {first}");
}

#[test]
fn cooldown_is_per_rule() {
    let mut s = Sim::new();
    let a = s.run(125, |o| o.serve_up = false);
    assert_eq!(rules(&a), vec![("serve_down", false)]);
    // a different rule is not affected by serve_down's cooldown
    let a = s.run(125, |o| {
        o.serve_up = false;
        o.gate_up = false;
    });
    assert_eq!(rules(&a), vec![("gate_down", false)]);
}

#[test]
fn container_restart_alerts_once_per_cooldown_and_reports_stable() {
    let mut s = Sim::new();
    let restart = |o: &mut Observation| o.restarts = vec![RestartObs { role: "serve".into(), detail: "glm RestartCount 0 -> 1".into() }];
    let a = s.once(restart);
    assert_eq!(rules(&a), vec![("container_restart:serve", false)]);
    assert!(a[0].message.contains("RestartCount 0 -> 1"));

    // crash loop: more restarts inside the cooldown are not re-alerted, and "recovered" waits
    s.run(60, |_| {});
    assert!(s.once(restart).is_empty());
    assert!(s.run(110, |_| {}).is_empty(), "not yet stable for 120 s since the LAST restart");
    let a = s.run(15, |_| {});
    assert_eq!(rules(&a), vec![("container_restart:serve", true)]);

    // a gate restart is its own rule key, so it is not muted by the serve's cooldown
    let a = s.once(|o| o.restarts = vec![RestartObs { role: "gate".into(), detail: "the gateway new StartedAt".into() }]);
    assert_eq!(rules(&a), vec![("container_restart:gate", false)]);
}

/// #22, 2026-09-21: a restart inside `lss maintenance start "reason"` reads `info`/"planned",
/// not `warn` - and its eventual "recovered" message is paired with its OWN fire, not with
/// whatever `maintenance_active` happens to be by the time the container settles (a window can
/// close mid-restart, well before `restart_stable_secs`).
#[test]
fn a_restart_during_a_maintenance_window_is_labelled_planned_not_a_warning() {
    let mut s = Sim::new();
    let restart = |o: &mut Observation| {
        o.restarts = vec![RestartObs { role: "serve".into(), detail: "glm new StartedAt".into() }];
        o.maintenance_active = true;
    };
    let a = s.once(restart);
    assert_eq!(rules(&a), vec![("container_restart:serve", false)]);
    assert_eq!(a[0].severity, Severity::Info, "planned restarts do not page/warn - info only, no banner");
    assert!(a[0].message.starts_with("planned - "), "{}", a[0].message);

    // the window closes WHILE the container is still settling - the eventual recovery message
    // must still read as planned, because that is what its own fire was
    let a = s.run(115, |o| o.maintenance_active = false);
    assert!(a.is_empty(), "not yet stable for 120 s since the restart");
    let a = s.run(15, |o| o.maintenance_active = false);
    assert_eq!(rules(&a), vec![("container_restart:serve", true)]);
    assert_eq!(a[0].severity, Severity::Info);
    assert!(a[0].message.starts_with("RECOVERED: planned - "), "{}", a[0].message);

    // an UNPLANNED restart right after is the ordinary warn - maintenance mode never leaks
    // past the window it was actually open for
    s.run(1800, |_| {}); // clear serve_down's/container_restart's cooldown
    let restart_unplanned = |o: &mut Observation| o.restarts = vec![RestartObs { role: "serve".into(), detail: "glm new StartedAt".into() }];
    let a = s.once(restart_unplanned);
    assert_eq!(rules(&a), vec![("container_restart:serve", false)]);
    assert_eq!(a[0].severity, Severity::Warn);
    assert!(!a[0].message.starts_with("planned - "), "{}", a[0].message);
}

#[test]
fn restart_recovery_waits_for_the_serve_to_answer() {
    let mut s = Sim::new();
    s.once(|o| o.restarts = vec![RestartObs { role: "serve".into(), detail: "x".into() }]);
    // container is running but the model is still loading: /v1/models is down
    assert!(s.run(115, |o| o.serve_up = false).iter().all(|e| !e.recovered));
    let a = s.run(300, |o| o.serve_up = false);
    assert!(a.iter().all(|e| e.rule != "container_restart:serve"), "{a:?}");
    let a = s.run(10, |_| {});
    assert!(rules(&a).contains(&("container_restart:serve", true)));
}

fn xid(gpu: u32, n: u32, ts: i64) -> XidEvent {
    XidEvent { ts, pci: format!("0000:{gpu:02x}:00"), xid: n, gpu: Some(gpu), detail: "pid=1, name=python3".into() }
}

#[test]
fn any_new_xid_is_a_hardware_alert_deduped_per_gpu() {
    let mut s = Sim::new();
    let a = s.once(|o| o.xids = vec![xid(3, 8, o.now), xid(3, 8, o.now), xid(1, 79, o.now)]);
    assert_eq!(rules(&a), vec![("xid:gpu3", false), ("xid:gpu1", false)]);
    assert!(a.iter().all(|e| e.severity == Severity::Hardware));
    assert!(a[0].message.contains("GPU3 Xid 8"), "{}", a[0].message);
    assert!(a[1].message.contains("fallen off the bus"));

    assert!(s.once(|o| o.xids = vec![xid(3, 8, o.now)]).is_empty(), "storm on the same GPU: one alert per cooldown");
    s.run(1800, |_| {});
    assert_eq!(rules(&s.once(|o| o.xids = vec![xid(3, 8, o.now)])), vec![("xid:gpu3", false)]);
}

#[test]
fn queue_pressure_needs_24_for_two_minutes() {
    let mut s = Sim::new();
    assert!(s.run(600, |o| o.queue = Some(23.0)).is_empty(), "23 is under the line");
    assert!(s.run(115, |o| o.queue = Some(24.0)).is_empty());
    assert!(s.once(|o| o.queue = Some(5.0)).is_empty(), "a dip resets the two minutes");
    assert!(s.run(115, |o| o.queue = Some(30.0)).is_empty());
    let a = s.run(10, |o| o.queue = Some(30.0));
    assert_eq!(rules(&a), vec![("queue_pressure", false)]);
    let a = s.once(|o| o.queue = Some(2.0));
    assert_eq!(rules(&a), vec![("queue_pressure", true)]);
}

#[test]
fn a_bench_window_suppresses_the_rules_a_benchmark_would_trip() {
    // `lss bench` at 8 users: a deep queue, waiters at the gate and slow "idle" probes, for half an hour
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    let loaded = |o: &mut Observation| {
        o.bench_active = true;
        o.queue = Some(60.0);
        o.trusted_waiters = Some(5);
        o.probe_tok_s = Some(40.0);
    };
    assert!(s.run(1800, loaded).is_empty(), "inside the bench window the load is ours: no queue, waiters or C1 alert");
    // what a benchmark does NOT excuse still alerts during it
    let a = s.run(130, |o| {
        o.bench_active = true;
        o.serve_up = false;
    });
    assert_eq!(rules(&a), vec![("serve_down", false)], "an outage during a bench is still an outage");
    s.run(10, |o| o.bench_active = true);
    // the window closes: the same readings now count from zero, not from when the bench started
    assert!(s.run(115, |o| o.queue = Some(60.0)).is_empty(), "the two minutes start when the bench ends");
    assert_eq!(rules(&s.run(10, |o| o.queue = Some(60.0))), vec![("queue_pressure", false)]);
    // a rule that was already firing when a bench starts recovers instead of staying red under it
    let a = s.once(|o| {
        o.bench_active = true;
        o.queue = Some(60.0);
    });
    assert_eq!(rules(&a), vec![("queue_pressure", true)]);
    // three slow probes taken DURING a bench never add up to a C1 alert afterwards
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    for _ in 0..3 {
        assert!(s.once(|o| { o.bench_active = true; o.probe_tok_s = Some(40.0); }).is_empty());
    }
    assert!(s.once(|o| o.probe_tok_s = Some(150.0)).is_empty(), "the first low idle probe after the bench is streak 1 of 3");
}

#[test]
fn gate_waiters_for_five_minutes() {
    let mut s = Sim::new();
    assert!(s.run(295, |o| o.trusted_waiters = Some(2)).is_empty());
    let a = s.run(10, |o| o.trusted_waiters = Some(2));
    assert_eq!(rules(&a), vec![("gate_waiters", false)]);
    assert_eq!(rules(&s.once(|_| {})), vec![("gate_waiters", true)]);
}

/// #36, 2026-09-21: the 2026-09-20 incident, in rule-engine terms - omp's default names a
/// model this provider does not serve. Held for `omp_mismatch_hold_secs` (default config: 600s;
/// this test sets it explicitly) so the few seconds a swap takes to land does not itself alert.
#[test]
fn omp_default_mismatch_holds_then_fires_and_names_both_ids() {
    let mut s = Sim::new();
    s.cfg.omp_mismatch_hold_secs = 300;
    let mismatched = |o: &mut Observation| o.omp_mismatch = Some(("acme/model-b".into(), "model-a".into()));
    assert!(s.run(295, mismatched).is_empty(), "under the hold: silent");
    let a = s.run(10, mismatched);
    assert_eq!(rules(&a), vec![("omp_default_mismatch", false)]);
    assert_eq!(a[0].severity, Severity::Warn);
    // card #205: the message says what to DO, in general terms - it may not name a script, because
    // the export drops ours and a public reader would be sent to a file their copy does not have.
    assert!(a[0].message.contains("acme/model-b") && a[0].message.contains("model-a") && a[0].message.contains("re-point omp's default"), "{}", a[0].message);
    assert!(!a[0].message.contains("scripts/"), "an alert may not send a reader to a repo path: {}", a[0].message);

    // fixed (omp's default was re-pointed, or the swap rolled back): recovers
    let a = s.once(|_| {});
    assert_eq!(rules(&a), vec![("omp_default_mismatch", true)]);

    // no omp config on this box, or nothing served yet: never a mismatch, never alerts
    assert!(s.run(3600, |_| {}).is_empty());
}

fn set_gpu(o: &mut Observation, idx: usize, temp: f64, mask: u64) {
    let g = &mut o.gpus.as_mut().unwrap()[idx];
    g.temp_c = Some(temp);
    g.throttle_mask = mask;
}

#[test]
fn thermal_by_temperature_and_by_slowdown_bit() {
    let mut s = Sim::new();
    assert!(s.run(595, |o| set_gpu(o, 2, 91.0, 0)).is_empty(), "under ten minutes");
    let a = s.run(10, |o| set_gpu(o, 2, 91.0, 0));
    assert_eq!(rules(&a), vec![("thermal_temp:gpu2", false)]);
    assert!(a[0].message.contains("GPU2 at 91°C / 196°F, >= 90°C / 194°F"), "{}", a[0].message);
    assert_eq!(rules(&s.once(|_| {})), vec![("thermal_temp:gpu2", true)]);

    // sw thermal slowdown bit (0x20) at a temperature below the line
    let a = s.run(605, |o| set_gpu(o, 1, 84.0, 0x20));
    assert_eq!(rules(&a), vec![("thermal_throttle:gpu1", false)]);
    assert!(a[0].message.contains("sw_thermal"));
    // hw thermal (0x40) counts too; the power cap bit (0x4) alone does not
    let mut s = Sim::new();
    assert!(s.run(1200, |o| set_gpu(o, 1, 70.0, 0x4)).is_empty(), "sw_power_cap is normal under load");
    assert_eq!(rules(&s.run(605, |o| set_gpu(o, 1, 70.0, 0x40))), vec![("thermal_throttle:gpu1", false)]);
}

#[test]
fn thermal_exclude_never_alerts_and_goes_to_the_daily_digest() {
    let mut s = Sim::new();
    // GPU0 (excluded by default) cooks for an hour, throttling for the whole of it
    let a = s.run(3600, |o| set_gpu(o, 0, 95.0, 0x20));
    assert!(a.is_empty(), "excluded GPU must not alert: {a:?}");
    let a = s.run(86_400, |_| {});
    assert_eq!(rules(&a), vec![("thermal_digest", false)]);
    assert_eq!(a[0].severity, Severity::Info);
    assert!(a[0].message.contains("GPU0 max 95°C / 203°F"), "{}", a[0].message);
    // 720 polls = 719 intervals of 5 s: the first poll has no elapsed time to attribute
    assert!(a[0].message.contains("59m55s at >= 90°C / 194°F"), "{}", a[0].message);

    // a quiet day produces no digest line at all
    assert!(s.run(2 * 86_400, |_| {}).is_empty());
}

#[test]
fn thermal_exclude_is_configurable() {
    let mut s = Sim::new();
    s.cfg.thermal_exclude = vec![];
    let a = s.run(605, |o| set_gpu(o, 0, 95.0, 0));
    assert_eq!(rules(&a), vec![("thermal_temp:gpu0", false)]);
}

#[test]
fn a_failed_nvidia_smi_poll_holds_thermal_state_and_raises_gpu_missing() {
    let mut s = Sim::new();
    s.run(605, |o| set_gpu(o, 2, 92.0, 0));
    let a = s.run(30, |o| o.gpus = None);
    assert!(a.is_empty(), "a failed poll is not a thermal recovery: {a:?}");
    let a = s.run(60, |o| o.gpus = None);
    assert_eq!(rules(&a), vec![("gpu_missing", false)]);
    assert_eq!(a[0].severity, Severity::Hardware);
    let a = s.once(|o| set_gpu(o, 2, 92.0, 0));
    assert_eq!(rules(&a), vec![("gpu_missing", true)]);
}

#[test]
fn a_gpu_dropping_off_the_bus_is_noticed() {
    let mut s = Sim::new();
    s.once(|_| {});
    let a = s.run(70, |o| {
        o.gpus.as_mut().unwrap().pop();
    });
    assert_eq!(rules(&a), vec![("gpu_missing", false)]);
    assert!(a[0].message.contains("reports 3 GPU(s), expected 4"));
}

#[test]
fn c1_baseline_is_the_median_of_the_first_12_probes() {
    let mut s = Sim::new();
    let probes = [190.0, 192.0, 188.0, 250.0, 191.0, 189.0, 193.0, 40.0, 190.0, 192.0, 191.0, 190.0];
    for (i, v) in probes.iter().enumerate() {
        assert_eq!(s.engine.c1_baseline(&s.cfg), None, "no baseline before probe {i}");
        assert!(s.once(|o| o.probe_tok_s = Some(*v)).is_empty(), "learning probes are never judged");
    }
    assert_eq!(s.engine.c1_baseline(&s.cfg), Some(190.5), "median ignores the 40 and 250 outliers");
}

#[test]
fn c1_alerts_on_three_consecutive_low_idle_probes_only() {
    let mut s = Sim::new();
    assert_eq!(s.cfg.c1_ratio, 0.8, "the default ratio");
    s.cfg.c1_baseline_tok_s = Some(200.0); // floor = 0.8 x 200 = 160
    assert!(s.once(|o| o.probe_tok_s = Some(150.0)).is_empty());
    assert!(s.once(|o| o.probe_tok_s = Some(155.0)).is_empty());
    assert!(s.once(|o| o.probe_tok_s = Some(161.0)).is_empty(), "a good probe resets the streak");
    assert!(s.once(|o| o.probe_tok_s = Some(150.0)).is_empty());
    // polls with no probe (skipped_busy included) neither count nor reset
    assert!(s.run(900, |_| {}).is_empty());
    assert!(s.once(|o| o.probe_tok_s = Some(159.9)).is_empty());
    let a = s.once(|o| o.probe_tok_s = Some(140.0));
    assert_eq!(rules(&a), vec![("c1_decode", false)]);
    assert!(a[0].message.contains("140.0 tok/s, below 160.0 (80% of baseline 200.0)"), "{}", a[0].message);
    assert!(s.once(|o| o.probe_tok_s = Some(130.0)).is_empty(), "no repeat while it stays low");

    let a = s.once(|o| o.probe_tok_s = Some(160.0));
    assert_eq!(rules(&a), vec![("c1_decode", true)], "exactly 0.8 x baseline is not 'below'");
}

#[test]
fn a_probe_at_85_percent_is_fine_under_the_default_ratio() {
    // the old 0.9 default alerted here; probe-to-probe noise on this box is about +-10 %
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    for _ in 0..6 {
        assert!(s.once(|o| o.probe_tok_s = Some(170.0)).is_empty());
    }
}

/// #47, 2026-09-21: on a busy box the stricter validation (card #45) can leave C1 without a
/// single VALID reading for hours - an hours-old number next to a live clock, unflagged, is the
/// same lie in the other direction. An `info` alert says so once the newest valid reading is
/// older than `c1_stale_probe_intervals` probe intervals, and recovers the instant one arrives.
#[test]
fn c1_raises_an_info_alert_when_it_has_not_been_able_to_measure_for_a_while() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    s.cfg.c1_stale_probe_intervals = 2; // healthy() sets probe_interval_secs = 300 -> 600s
    assert!(s.once(|o| o.probe_tok_s = Some(190.0)).is_empty(), "seeds the last-valid clock");
    // just under the threshold: nothing yet, still ok
    assert!(s.run(595, |_| {}).is_empty());
    assert_eq!(state_of(&s, "c1_stale").state, "ok");
    // crosses the staleness threshold: PENDING, not yet firing - #52 added a 300s hold on top
    // (cheap insurance against firing the instant a boundary is crossed)
    assert!(s.run(10, |_| {}).is_empty());
    assert_eq!(state_of(&s, "c1_stale").state, "pending");
    // holds continuously for the full 300s: fires exactly once, info severity, plain wording
    let a = s.run(300, |_| {});
    assert_eq!(rules(&a), vec![("c1_stale", false)]);
    assert_eq!(a[0].severity, Severity::Info);
    assert!(a[0].message.contains("C1 has not been able to measure for") && a[0].message.contains("the server has been busy"), "{}", a[0].message);
    assert!(s.run(300, |_| {}).is_empty(), "stays quiet while still stale (cooldown)");
    // a valid reading arrives: recovers on that exact tick
    let a = s.once(|o| o.probe_tok_s = Some(190.0));
    assert_eq!(rules(&a), vec![("c1_stale", true)]);
    assert!(a[0].message.contains("measuring again"), "{}", a[0].message);
    assert_eq!(state_of(&s, "c1_stale").state, "ok");
}

/// #47 item 3: the staleness rule must never gate or delay the EXISTING low-decode alert - a
/// box that goes quiet for a while and then comes back low still gets exactly the same
/// `c1_decode` alert, after exactly three consecutive low readings, as if the quiet period had
/// never happened; and staleness itself recovers on the very first reading back, not late.
#[test]
fn staleness_never_delays_the_low_decode_alert_once_readings_return() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0); // floor = 160
    s.cfg.c1_stale_probe_intervals = 2; // 600s at the default 300s probe interval
    assert!(s.once(|o| o.probe_tok_s = Some(190.0)).is_empty());
    // quiet long enough to go stale - 600s threshold + #52's 300s hold on top
    let a = s.run(900, |_| {});
    assert_eq!(rules(&a), vec![("c1_stale", false)]);
    // the FIRST reading back recovers staleness immediately, same tick - and it is itself low
    let a = s.once(|o| o.probe_tok_s = Some(150.0));
    assert_eq!(rules(&a), vec![("c1_stale", true)]);
    // three consecutive low readings still take exactly three to alert, same as ever - the
    // quiet period in between changed nothing about how the low-decode rule counts
    assert!(s.once(|o| o.probe_tok_s = Some(150.0)).is_empty());
    let a = s.once(|o| o.probe_tok_s = Some(140.0));
    assert_eq!(rules(&a), vec![("c1_decode", false)]);
    assert!(a[0].message.contains("3 consecutive"), "{}", a[0].message);
}

/// One probe as the collector books it: timed, then judged against what was seen around it.
fn probe(ts: i64, tok_s: f64, e: ProbeEvidence) -> ProbeRecord {
    let mut r = ProbeRecord { ts, status: "ok".into(), ttft_ms: Some(e.ttft_ms), decode_tok_s: Some(tok_s), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
    if let Err((reason, detail)) = validate(&e, RulesConfig::default().c1_max_ttft_s) {
        r.invalidate(reason, detail);
    }
    r
}

fn alone() -> ProbeEvidence {
    ProbeEvidence { gateway: true, engine_reports_load: true, running_before: 0.0, queue_before: 0.0, ttft_ms: 140.0, running_after: Some(1.0), queue_after: Some(0.0), admitted_before: Some(10), admitted_after: Some(11), probe_admitted: true, gen_tokens_before: None, gen_tokens_after: None, probe_tokens: 0.0, ttft_baseline_ms: None, requests_before: None, requests_after: None, prompt_tokens_before: None, prompt_tokens_after: None }
}
fn slow_ttft() -> ProbeEvidence {
    ProbeEvidence { ttft_ms: 26_548.0, ..alone() }
}
fn contended() -> ProbeEvidence {
    ProbeEvidence { admitted_after: Some(13), ..alone() }
}
fn busy_before() -> ProbeEvidence {
    ProbeEvidence { running_before: 1.0, ..alone() }
}

impl Sim {
    /// One poll that carries this batch of finished probes, the way the collector feeds them.
    fn poll_with(&mut self, batch: &[ProbeRecord]) -> Vec<AlertEvent> {
        let reading = c1_reading(batch);
        let ts = batch.iter().rev().find(|p| p.is_reading()).map(|p| p.ts);
        self.once(|o| {
            o.probe_tok_s = reading;
            o.probe_ts = ts;
        })
    }
}

#[test]
fn invalid_probes_neither_count_towards_c1_nor_reset_it() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0); // floor 160
    // the live false alarm: three LOW numbers in a row, every one of them from a collision
    for e in [slow_ttft(), contended(), busy_before(), slow_ttft()] {
        assert!(s.poll_with(&[probe(s.now, 141.4, e)]).is_empty());
    }
    assert!(s.engine.firing(s.now).is_empty(), "four contaminated low probes are not a slow server");

    // two honest low probes ...
    assert!(s.poll_with(&[probe(s.now, 150.0, alone())]).is_empty());
    assert!(s.poll_with(&[probe(s.now, 150.0, alone())]).is_empty());
    // ... a contaminated GOOD-looking one in between must not reset the streak ...
    assert!(s.poll_with(&[probe(s.now, 195.0, contended())]).is_empty());
    // ... so the third honest low probe fires
    let a = s.poll_with(&[probe(s.now, 150.0, alone())]);
    assert_eq!(rules(&a), vec![("c1_decode", false)]);
    // and a contaminated good one does not "recover" it either
    assert!(s.poll_with(&[probe(s.now, 199.0, slow_ttft())]).is_empty());
    assert_eq!(s.engine.firing(s.now), vec!["c1_decode"]);
    let a = s.poll_with(&[probe(s.now, 199.0, alone())]);
    assert_eq!(rules(&a), vec![("c1_decode", true)]);
}

#[test]
fn invalid_probes_are_not_learned_into_the_baseline() {
    let mut s = Sim::new();
    for i in 0..30 {
        let (v, e) = if i % 2 == 0 { (120.0, contended()) } else { (190.0, alone()) };
        s.poll_with(&[probe(s.now, v, e)]);
        if i < 22 {
            assert_eq!(s.engine.c1_baseline(&s.cfg), None, "only {} valid probes so far", (i + 1) / 2);
        }
    }
    assert_eq!(s.engine.c1_baseline(&s.cfg), Some(190.0), "12 valid probes, none of the 120s");
}

#[test]
fn a_baseline_learned_before_validation_existed_is_rebuilt_from_valid_probes() {
    let mut s = Sim::new();
    // state as an old collector left it: baseline learned from whatever came, streak of 2
    for v in [150.0, 141.4, 153.9, 183.0, 150.0, 145.0, 199.0, 148.0, 140.0, 152.0, 149.0, 151.0] {
        s.once(|o| o.probe_tok_s = Some(v));
    }
    assert_eq!(s.engine.c1_baseline(&s.cfg), Some(150.0));
    let mut old: serde_json::Value = serde_json::to_value(&s.engine).unwrap();
    old["c1"].as_object_mut().unwrap().remove("validated");
    old["c1"]["low_streak"] = 2.into();
    let mut engine: Engine = serde_json::from_value(old).unwrap();
    assert!(engine.c1_needs_revalidation());

    // the collector hands over the last valid readings from its DB, oldest first
    let valid: Vec<f64> = vec![170.0, 189.0, 191.0, 188.0, 192.0, 190.0, 187.0, 193.0, 190.0, 189.0, 191.0, 190.0, 192.0];
    engine.relearn_c1_baseline(&s.cfg, &valid);
    assert!(!engine.c1_needs_revalidation());
    assert_eq!(engine.c1_baseline(&s.cfg), Some(190.0), "median of the LAST 12 (the 170 fell off)");
    // the inherited streak is gone: one low probe is one, not three
    s.engine = engine;
    assert!(s.once(|o| o.probe_tok_s = Some(100.0)).is_empty());
    assert!(s.once(|o| o.probe_tok_s = Some(100.0)).is_empty());
    assert_eq!(rules(&s.once(|o| o.probe_tok_s = Some(100.0))), vec![("c1_decode", false)]);

    // too few valid probes: back to learning, seeded with what there is
    let mut e = Engine::default();
    e.relearn_c1_baseline(&s.cfg, &[190.0, 191.0]);
    assert_eq!(e.c1_baseline(&s.cfg), None);
    assert_eq!(e.c1_baseline_progress(), 2);
}

#[test]
fn a_test_alert_is_one_info_alert_per_request_with_no_state() {
    let mut s = Sim::new();
    let a = s.once(|o| o.test_alerts = vec!["pipeline check".into()]);
    assert_eq!(rules(&a), vec![("test_alert", false)]);
    assert_eq!(a[0].severity, Severity::Info);
    assert_eq!(a[0].message, "[TEST] pipeline check");
    assert!(s.engine.firing(s.now).is_empty(), "a test alert never shows as firing");
    // no cooldown: asking again fires again, and nothing is owed afterwards
    let a = s.once(|o| o.test_alerts = vec!["again".into(), "and again".into()]);
    assert_eq!(a.len(), 2);
    assert!(s.run(3600, |_| {}).is_empty());
}

#[test]
fn c1_config_baseline_wins_over_the_learned_one() {
    let mut s = Sim::new();
    for _ in 0..12 {
        s.once(|o| o.probe_tok_s = Some(100.0));
    }
    assert_eq!(s.engine.c1_baseline(&s.cfg), Some(100.0));
    s.cfg.c1_baseline_tok_s = Some(200.0);
    assert_eq!(s.engine.c1_baseline(&s.cfg), Some(200.0));
}

#[test]
fn reject_growth_over_20_in_10_minutes_is_info() {
    let mut s = Sim::new();
    s.once(|o| o.rejected_429 = Some(100));
    assert!(s.run(300, |o| o.rejected_429 = Some(120)).is_empty(), "+20 is not > 20");
    let a = s.once(|o| o.rejected_429 = Some(121));
    assert_eq!(rules(&a), vec![("rejects_429", false)]);
    assert_eq!(a[0].severity, Severity::Info);
    // the burst ages out of the window → recovered
    let a = s.run(700, |o| o.rejected_429 = Some(121));
    assert_eq!(rules(&a), vec![("rejects_429", true)]);

    // slow growth never trips: 1 per minute = 10 per window
    let mut s = Sim::new();
    let start = s.now;
    assert!(s.run(7200, move |o| o.rejected_413 = Some(((o.now - start) / 60) as u64)).is_empty());
}

#[test]
fn reject_counter_reset_by_a_gate_restart_is_not_growth() {
    let mut s = Sim::new();
    s.run(60, |o| o.rejected_413 = Some(5000));
    assert!(s.run(600, |o| o.rejected_413 = Some(3)).is_empty());
    assert!(s.run(60, |o| o.rejected_413 = None).is_empty(), "gate unreadable: no opinion");
}

#[test]
fn engine_state_survives_serialisation() {
    let mut s = Sim::new();
    s.run(125, |o| o.serve_up = false);
    let saved = serde_json::to_string(&s.engine).unwrap();
    s.engine = serde_json::from_str(&saved).unwrap();
    assert_eq!(s.engine.firing(s.now), vec!["serve_down".to_string()]);
    assert!(s.run(300, |o| o.serve_up = false).is_empty(), "restored engine must not re-send");
    assert_eq!(rules(&s.once(|_| {})), vec![("serve_down", true)]);
}

/// `GET /rules`: what Alertmanager shows as inactive / pending / firing.
fn state_of(s: &Sim, rule: &str) -> lss_core::rules::RuleState {
    s.engine.rule_states(&s.cfg, s.now).into_iter().find(|r| r.rule == rule).unwrap_or_else(|| panic!("no rule {rule}"))
}

#[test]
fn rule_states_go_ok_pending_firing_and_back_with_a_cooldown() {
    let mut s = Sim::new();
    s.run(60, |_| {});
    let q = state_of(&s, "queue_pressure");
    assert_eq!((q.state.as_str(), q.value.as_str(), q.threshold.as_str(), q.last_fired, q.cooldown_remaining_s), ("ok", "queue 0", "queue >= 24 for 2m00s", None, 0));

    // over the line, but not yet for two minutes: PENDING
    s.run(60, |o| o.queue = Some(30.0));
    let q = state_of(&s, "queue_pressure");
    assert_eq!((q.state.as_str(), q.value.as_str()), ("pending", "queue 30"));
    assert_eq!(q.pending_since, Some(s.now - 55));
    assert!(s.engine.firing(s.now).is_empty());

    // held for the full two minutes: FIRING, with a last-fired time and a cooldown running
    s.run(90, |o| o.queue = Some(30.0));
    let q = state_of(&s, "queue_pressure");
    assert_eq!(q.state, "firing");
    let fired_at = q.last_fired.expect("fired");
    assert_eq!(q.cooldown_remaining_s, s.cfg.cooldown_secs - (s.now - fired_at));
    assert_eq!(s.engine.firing(s.now), vec!["queue_pressure"]);

    // cleared: ok again, but the cooldown keeps counting down
    s.run(300, |_| {});
    let q = state_of(&s, "queue_pressure");
    assert_eq!((q.state.as_str(), q.pending_since), ("ok", None));
    assert_eq!(q.cooldown_remaining_s, s.cfg.cooldown_secs - (s.now - fired_at));

    // trips again inside the cooldown: held back = pending, not firing
    s.run(200, |o| o.queue = Some(40.0));
    assert_eq!(state_of(&s, "queue_pressure").state, "pending");
    s.run(s.cfg.cooldown_secs, |o| o.queue = Some(40.0));
    let q = state_of(&s, "queue_pressure");
    assert_eq!(q.state, "firing", "once the cooldown is over it goes out");
    assert!(q.last_fired.unwrap() > fired_at);
}

#[test]
fn every_rule_is_listed_with_its_threshold_and_what_it_sees() {
    let mut s = Sim::new();
    s.run(30, |o| {
        o.gpus.as_mut().unwrap()[2].temp_c = Some(93.0);
        o.gpus.as_mut().unwrap()[1].throttle_mask = 0x40;
        o.trusted_waiters = Some(3);
        o.probe_tok_s = Some(188.0);
    });
    let all = s.engine.rule_states(&s.cfg, s.now);
    let names: Vec<&str> = all.iter().map(|r| r.rule.as_str()).collect();
    for rule in ["serve_down", "serve_down_page", "gate_down", "queue_pressure", "gate_waiters", "gpu_missing", "thermal_temp:gpu0", "thermal_throttle:gpu3", "c1_decode", "rejects_413", "rejects_429", "container_restart:serve", "container_restart:gate", "xid:*", "thermal_digest"] {
        assert!(names.contains(&rule), "{rule} missing from {names:?}");
    }
    assert!(all.iter().all(|r| ["ok", "pending", "firing"].contains(&r.state.as_str()) && !r.threshold.is_empty() && !r.severity.is_empty()));
    let get = |rule: &str| all.iter().find(|r| r.rule == rule).unwrap();
    assert_eq!((get("thermal_temp:gpu2").state.as_str(), get("thermal_temp:gpu2").value.as_str()), ("pending", "93C/199F"));
    assert_eq!((get("thermal_throttle:gpu1").state.as_str(), get("thermal_throttle:gpu1").value.as_str()), ("pending", "hw_thermal"));
    assert!(get("thermal_temp:gpu0").threshold.contains("excluded"), "GPU0 is in this scenario's thermal_exclude");
    assert_eq!(get("thermal_temp:gpu0").state, "ok", "an excluded GPU never goes pending");
    assert_eq!((get("gate_waiters").state.as_str(), get("gate_waiters").value.as_str()), ("pending", "waiters 3"));
    assert_eq!(get("gpu_missing").value, "4 of 4");
    assert!(get("c1_decode").threshold.starts_with("learning the baseline (6/12)"), "{}", get("c1_decode").threshold);
    assert!(get("c1_decode").value.starts_with("188.0 tok/s"));
    assert_eq!((get("serve_down").severity.as_str(), get("serve_down_page").severity.as_str(), get("gpu_missing").severity.as_str()), ("warn", "page", "hardware"));

    // an Xid and a restart show up as event rules with their last-fired time
    s.once(|o| {
        o.xids = vec![XidEvent { ts: 1, pci: "0000:f1:00".into(), gpu: Some(3), xid: 79, detail: "fell off the bus".into() }];
        o.restarts = vec![RestartObs { role: "serve".into(), detail: "RestartCount 0 -> 1".into() }];
    });
    let xid = state_of(&s, "xid:gpu3");
    assert_eq!((xid.last_fired, xid.severity.as_str()), (Some(s.now), "hardware"));
    let restart = state_of(&s, "container_restart:serve");
    assert_eq!((restart.state.as_str(), restart.last_fired), ("pending", Some(s.now)), "pending until it has stayed up");
    // the snapshot of what the engine saw is not part of the saved state
    let saved = serde_json::to_string(&s.engine).unwrap();
    assert!(!saved.contains("\"last\""), "{saved}");
}

/// #52, 2026-09-21 (panel: hamel-husain): "collapse by alert IDENTITY, not by transition." Runs
/// the sim until c1_stale sends the next fire or recover `AlertEvent` for it - robust to the
/// exact tick math of hold + threshold + cooldown, which is exactly what these tests do not want
/// to hand-compute and re-break every time one of those numbers changes.
fn run_until_c1_stale_event(s: &mut Sim, max_secs: i64) -> AlertEvent {
    let end = s.now + max_secs;
    loop {
        assert!(s.now < end, "c1_stale never fired or recovered within {max_secs}s");
        if let Some(e) = s.once(|_| {}).into_iter().find(|e| e.rule == "c1_stale") {
            return e;
        }
    }
}

/// #52 checklist items 1 and 2: three flaps (fire then recover, three times) mute the identity
/// and rewrite its row - never suppress it. A fourth cycle, while muted, sends no new individual
/// fire/recover event (the noise the owner was actually receiving), but the row is never hidden.
#[test]
fn three_flaps_mute_and_relabel_c1_stale_but_never_hide_the_row() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    s.cfg.c1_stale_probe_intervals = 2; // 600s threshold at the default 300s probe interval
    assert!(s.once(|o| o.probe_tok_s = Some(190.0)).is_empty(), "seeds the last-valid clock");

    for n in 1..=2 {
        let fire = run_until_c1_stale_event(&mut s, 3_000);
        assert!(!fire.recovered, "flap {n}: expected a FIRE first");
        let recovered = s.once(|o| o.probe_tok_s = Some(190.0)).into_iter().find(|e| e.rule == "c1_stale");
        assert!(recovered.is_some_and(|e| e.recovered), "flap {n}: expected a RECOVER on the very next valid probe");
        assert_ne!(state_of(&s, "c1_stale").state, "muted", "not muted yet after flap {n}");
    }
    // the third flap fires normally, but its own recovery is superseded by the mute
    // announcement (the row now tells the fuller story instead of one more "recovered")
    let fire = run_until_c1_stale_event(&mut s, 3_000);
    assert!(!fire.recovered, "flap 3: expected a FIRE first");
    let events = s.once(|o| o.probe_tok_s = Some(190.0));
    let mute_msg = events.iter().find(|e| e.rule == "c1_stale").expect("the mute announcement lands on the same tick as the third flap's recovery");
    assert!(mute_msg.message.to_lowercase().contains("unreliable") && mute_msg.message.to_lowercase().contains("flapped"), "{}", mute_msg.message);

    // three flaps done: MUTED, re-labelled, and still a real row in the list (never hidden)
    let st = state_of(&s, "c1_stale");
    assert_eq!(st.state, "muted");
    assert!(st.value.contains("unreliable") && st.value.contains("flaps/24h") && st.value.contains("muted"), "{}", st.value);
    assert!(!s.engine.firing(s.now).contains(&"c1_stale".to_string()), "muted does not turn the header red - the measurement is unreliable, not the server");

    // a fourth stale/recover cycle happens underneath, but sends NOTHING new: this is the actual
    // noise fix - the owner stops receiving the fifth, sixth, seventh copy of the same pair. Run
    // well past when it would have fired (had it not been muted) and confirm silence.
    let quiet = s.run(3_000, |_| {});
    assert!(!rules(&quiet).iter().any(|(r, _)| *r == "c1_stale"), "muted: a new stale crossing sends no individual AlertEvent");
    let _ = s.once(|o| o.probe_tok_s = Some(190.0)); // recovers quietly too
    state_of(&s, "c1_stale"); // still a real row - panics if the rule vanished from the list
}

/// #52 checklist item 3 (the other half of item 2): a genuine SUSTAINED breach - not flapping,
/// just true and staying true - still fires promptly, on its own normal schedule, never
/// mistaken for a flap and never delayed by the muting machinery. Do not trade a real alert for
/// quiet.
#[test]
fn a_genuine_sustained_breach_is_not_muted_and_still_fires_promptly() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(200.0);
    s.cfg.c1_stale_probe_intervals = 2; // 600s threshold
    assert!(s.once(|o| o.probe_tok_s = Some(190.0)).is_empty());
    // one continuous stale period, never recovering: fires once, cleanly, and reads as a plain
    // firing row - not muted (zero flaps have happened, let alone three)
    let fire = run_until_c1_stale_event(&mut s, 3_000);
    assert!(!fire.recovered);
    let st = state_of(&s, "c1_stale");
    assert_eq!(st.state, "firing");
    assert!(s.engine.firing(s.now).contains(&"c1_stale".to_string()), "a real, sustained finding IS in the firing list");
}

// ------------------------------------------------------- #108: the gate's charging, watched

/// A healthy `shadow` block: warm, seeing the engine, discounting most of what it admits.
fn charging_well() -> lss_core::gate::GateShadow {
    lss_core::gate::GateShadow {
        enabled: true,
        scrapes_ok: 500,
        scrapes_failed: 3,
        healthy: true,
        cache_warm: Some(true),
        cold_since_age_s: Some(900.0),
        charge_errors: 0,
        recent_admissions: 200,
        recent_discounted: 180,
        // card #279: a healthy gate publishes a drain estimate; nothing in rules.rs reads it,
        // it is here because the literal must be complete.
        seconds_to_drain_charged: Some(12.0),
    }
}

#[test]
fn charge_errors_warn_after_a_minute_and_recover_once() {
    let mut s = Sim::new();
    assert!(s.run(3600, |o| o.gate_shadow = Some(charging_well())).is_empty(), "a working charge path is silent");
    assert!(
        s.run(55, |o| o.gate_shadow = Some(lss_core::gate::GateShadow { charge_errors: 7, ..charging_well() })).is_empty(),
        "under charge_errors_secs: still quiet"
    );
    let a = s.run(10, |o| o.gate_shadow = Some(lss_core::gate::GateShadow { charge_errors: 9, ..charging_well() }));
    assert_eq!(rules(&a), vec![("gate_charge_errors", false)]);
    assert_eq!(a[0].severity, Severity::Warn);
    assert!(a[0].message.contains("charging GROSS"), "{}", a[0].message);
    let b = s.run(60, |o| o.gate_shadow = Some(charging_well()));
    assert_eq!(rules(&b), vec![("gate_charge_errors", true)], "recovers once");
}

#[test]
fn the_inert_discount_warns_and_a_cold_or_quiet_box_does_not() {
    let inert = || lss_core::gate::GateShadow { recent_discounted: 0, ..charging_well() };
    let mut s = Sim::new();
    // THE card-68 shape: warm, engine visible, 200 admissions, none discounted.
    assert!(s.run(590, |o| o.gate_shadow = Some(inert())).is_empty(), "under discount_inert_secs: quiet");
    let a = s.run(15, |o| o.gate_shadow = Some(inert()));
    assert_eq!(rules(&a), vec![("gate_discount_inert", false)]);
    assert!(a[0].message.contains("NOT ONE discounted"), "{}", a[0].message);

    // a COLD cache charging gross is correct behaviour, not a fault - it must never alarm
    let mut s = Sim::new();
    let cold = || lss_core::gate::GateShadow { recent_discounted: 0, cache_warm: Some(false), ..charging_well() };
    assert!(s.run(3 * 3600, |o| o.gate_shadow = Some(cold())).is_empty(), "cold is not inert");

    // nor may a QUIET box: 4 admissions and no discounts says nothing at all
    let mut s = Sim::new();
    let quiet = || lss_core::gate::GateShadow { recent_admissions: 4, recent_discounted: 0, ..charging_well() };
    assert!(s.run(3 * 3600, |o| o.gate_shadow = Some(quiet())).is_empty(), "too little traffic to judge");

    // and neither may a gate that is not answering at all - gate_down owns that failure
    let mut s = Sim::new();
    assert!(
        s.run(3 * 3600, |o| o.gate_shadow = None).is_empty(),
        "a missing reading is not a charging fault"
    );
}

#[test]
fn charging_rules_appear_in_the_rules_document_with_their_numbers() {
    let mut s = Sim::new();
    s.run(30, |o| o.gate_shadow = Some(lss_core::gate::GateShadow { charge_errors: 2, recent_discounted: 0, ..charging_well() }));
    let states = s.engine.rule_states(&s.cfg, s.now);
    let row = |name: &str| states.iter().find(|r| r.rule == name).unwrap_or_else(|| panic!("{name} missing from /rules"));
    let e = row("gate_charge_errors");
    assert_eq!(e.state, "pending", "condition true, hold time not over: {e:?}");
    assert!(e.value.contains("2 errors"), "{e:?}");
    let d = row("gate_discount_inert");
    assert!(d.value.contains("0/200 discounted") && d.value.contains("warm true"), "{d:?}");
    assert!(d.threshold.contains("0 discounted"), "{d:?}");
}

// --- card #78: the composite "big budget, no cache" rule ---------------------------------------

#[test]
fn cold_budget_fires_on_the_2026_09_22_numbers() {
    // the exact state that prompted the card: 2,956,712 of 3,000,000 in flight, hit 0.0
    // (card #135: the threshold is the LIVE capacity, so the observation carries it)
    let mut sim = Sim::new();
    sim.cfg.cold_budget_secs = 60;
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(2_956_712);
        o.trusted_budget_tokens = Some(3_000_000);
        o.cache_hit_rate = Some(0.0);
    });
    let fired: Vec<_> = sim.log.iter().filter(|a| a.rule == "gate_cold_budget" && !a.recovered).collect();
    assert!(!fired.is_empty(), "the 09-22 state must fire: {:?}", sim.log.iter().map(|a| a.rule.clone()).collect::<Vec<_>>());
    assert!(fired[0].message.contains("2,956,712") || fired[0].message.contains("2956712"), "{}", fired[0].message);
}

#[test]
fn cold_budget_fires_at_the_live_1m_capacity() {
    // card #135 item 2: the rule MUST fire at the LIVE configuration - 1,000,000 capacity,
    // in-flight AT capacity (accumulation tops out there), cache cold. The old
    // "strictly > constant" threshold could never fire because the constant EQUALLED the cap.
    let mut sim = Sim::new();
    sim.cfg.cold_budget_secs = 60;
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(1_000_000); // AT the capacity, not above
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.0);
    });
    let fired: Vec<_> = sim.log.iter().filter(|a| a.rule == "gate_cold_budget" && !a.recovered).collect();
    assert!(!fired.is_empty(), "at-capacity + cold must fire at the live 1M config: {:?}", sim.log.iter().map(|a| a.rule.clone()).collect::<Vec<_>>());
}

#[test]
fn cold_budget_needs_both_facts() {
    // each fact alone is normal: big budget WITH a warm cache, or a cold cache with a small
    // budget. Only the pair is lethal.
    let mut sim = Sim::new();
    sim.cfg.cold_budget_secs = 60;
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(2_956_712);
        o.trusted_budget_tokens = Some(3_000_000);
        o.cache_hit_rate = Some(0.998); // warm: harmless
    });
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(500_000); // under 90% of 1M: harmless
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.0);
    });
    assert!(sim.log.iter().all(|a| a.rule != "gate_cold_budget"), "neither fact alone may fire: {:?}", sim.log.iter().map(|a| a.rule.clone()).collect::<Vec<_>>());
}

#[test]
fn cold_budget_recovers_and_stays_quiet_on_healthy() {
    let mut sim = Sim::new();
    sim.cfg.cold_budget_secs = 60;
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(2_956_712);
        o.trusted_budget_tokens = Some(3_000_000);
        o.cache_hit_rate = Some(0.5);
    });
    sim.run(180, |o| {
        o.trusted_inflight_tokens = Some(200_000);
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.998);
    });
    let fired = sim.log.iter().filter(|a| a.rule == "gate_cold_budget").count();
    let recovered = sim.log.iter().filter(|a| a.rule == "gate_cold_budget" && a.recovered).count();
    assert!(fired >= 1 && recovered >= 1, "fire then recover: {fired} fired, {recovered} recovered");
    // a healthy hour after: no re-fires
    sim.log.clear();
    sim.run(600, |o| {
        o.trusted_inflight_tokens = Some(200_000);
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.998);
    });
    assert!(sim.log.iter().all(|a| a.rule != "gate_cold_budget"));
}

// --- card #87 item 3: the cold-restart case is tested DELIBERATELY ------------------------------
// The 01:40 incident's shape: the serve container restarts (RestartCount 0 -> 1) and the
// agents immediately fire multi-100k prompts at a COLD cache - the state that preceded all
// three Xid-8 hangs. Two protections must hold together: the restart alerts (down minute +
// the restart event), and the cold-budget rule firing when the budget fills with REAL prefill
// while the cache is still cold. One clock drives both, at the incident's own numbers.

#[test]
fn a_cold_restart_with_agents_firing_400k_prompts_alerts_and_fires_the_budget_rule() {
    let mut sim = Sim::new();
    sim.cfg.cold_budget_secs = 60;
    sim.cfg.restart_stable_secs = 300;
    // warm baseline
    sim.run(120, |o| {
        o.trusted_inflight_tokens = Some(100_000);
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.998);
    });
    assert!(sim.log.iter().all(|a| a.rule != "gate_cold_budget"));
    // THE RESTART MINUTE(S): serve down, the restart event observed - the alert must fire
    let fired = sim.run(180, |o| {
        o.serve_up = false;
        o.restarts = vec![RestartObs { role: "serve".into(), detail: "Xid-8 hang; docker restart policy".into() }];
    });
    assert!(fired.iter().any(|a| a.rule == "container_restart:serve" && !a.recovered),
        "a serve restart must alert: {:?}", fired.iter().map(|a| a.rule.clone()).collect::<Vec<_>>());
    // THE COLD REFILL: the serve is back up, the cache is COLD (hit ~0 - agents firing 400k
    // unique prompts into a fresh radix cache), the budget fills past 90% with real prefill.
    let fired = sim.run(300, |o| {
        o.trusted_inflight_tokens = Some(950_000); // 95% of the 1M the budget is pinned at
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.05); // cold: almost nothing is hitting
    });
    assert!(fired.iter().any(|a| a.rule == "gate_cold_budget" && !a.recovered),
        "cold cache + full budget (the pre-hang state) must fire: {:?}",
        fired.iter().map(|a| a.rule.clone()).collect::<Vec<_>>());
    // and the recovery line comes when either condition clears
    let fired = sim.run(120, |o| {
        o.trusted_inflight_tokens = Some(50_000);
        o.trusted_budget_tokens = Some(1_000_000);
        o.cache_hit_rate = Some(0.998); // warmed up
    });
    assert!(fired.iter().any(|a| a.rule == "gate_cold_budget" && a.recovered),
        "drained budget or warm cache recovers the alert: {:?}",
        fired.iter().map(|a| (a.rule.clone(), a.recovered)).collect::<Vec<_>>());
}

// ---- card #261: the 2026-09-23 22:16Z C1 false alarm ----
// Live: alert #125 "C1 decode 143.1 tok/s, below 148.2 (80% of baseline 185.2) on 3 consecutive
// idle probes" - but the probes table held ONE valid low probe. The other two (19:35:29Z 125.2 and
// 19:50:38Z 140.8) had been logged valid, counted, then re-judged `contended` by the startup
// repair at the 19:52Z collector restart; the bare counter kept them. And the one that was left
// was not idle either: TTFT 259.8 ms against an idle median near 135, with a 200k-token request in
// the engine log for the whole window.

const T_1935: i64 = 1_790_192_129; // 2026-09-23 19:35:29Z
const T_1950: i64 = 1_790_193_038; // 2026-09-23 19:50:38Z
const T_2216: i64 = 1_790_201_764; // 2026-09-23 22:16:04Z

/// the 22:16Z probe exactly as the collector saw it: every counter either side read quiet
fn row_2216z() -> ProbeEvidence {
    ProbeEvidence { ttft_ms: 259.8, ttft_baseline_ms: Some(135.5), ..alone() }
}

#[test]
fn a_probe_at_twice_the_idle_ttft_is_contended() {
    let (reason, detail) = validate(&row_2216z(), 3.0).unwrap_err();
    assert_eq!(reason, "contended", "{detail}");
    assert!(detail.contains("259.8 ms") && detail.contains("135.5 ms"), "{detail}");
    // at the limit is still a reading; a TTFT inside 1.5x is not evidence of anything
    assert!(validate(&ProbeEvidence { ttft_ms: 203.0, ..row_2216z() }, 3.0).is_ok());
    // no idle median yet (too few valid probes): the check is skipped, never invented
    assert!(validate(&ProbeEvidence { ttft_baseline_ms: None, ..row_2216z() }, 3.0).is_ok());
}

#[test]
fn replaying_the_2216z_probe_fires_nothing() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(185.2); // floor 148.2
    // the two probes the live collector counted at 19:35Z and 19:50Z ...
    assert!(s.poll_with(&[probe(T_1935, 125.2, alone())]).is_empty());
    assert!(s.poll_with(&[probe(T_1950, 140.8, alone())]).is_empty());
    assert_eq!(s.engine.c1_streak_probes(), vec![T_1935, T_1950]);
    // ... which the startup repair then re-judged `contended`: the streak drops them
    let dropped = s.engine.retain_c1_streak(|ts| ![T_1935, T_1950].contains(&ts));
    assert_eq!(dropped, vec![T_1935, T_1950]);
    // and the 22:16Z probe itself is now judged contended, so it is no reading at all
    let rec = probe(T_2216, 143.1, row_2216z());
    assert_eq!(rec.invalid_reason.as_deref(), Some("contended"));
    assert!(s.poll_with(&[rec]).is_empty(), "22:16Z replayed must not alert");
    assert!(s.engine.firing(s.now).is_empty());
}

#[test]
fn a_streak_saved_as_a_bare_count_is_not_evidence() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(185.2);
    let mut old: serde_json::Value = serde_json::to_value(&s.engine).unwrap();
    old["c1"]["low_streak"] = 2.into();
    s.engine = serde_json::from_value(old).unwrap();
    // one low probe after a restart is ONE, not three
    assert!(s.poll_with(&[probe(T_2216, 143.1, alone())]).is_empty());
    assert!(s.engine.firing(s.now).is_empty());
}

#[test]
fn the_c1_alert_names_the_valid_probes_it_counted() {
    let mut s = Sim::new();
    s.cfg.c1_baseline_tok_s = Some(185.2);
    assert!(s.poll_with(&[probe(T_1935, 125.2, alone())]).is_empty());
    // an invalid probe in between neither counts nor resets
    assert!(s.poll_with(&[probe(T_1950, 140.8, contended())]).is_empty());
    assert!(s.poll_with(&[probe(T_1950 + 300, 140.0, alone())]).is_empty());
    let a = s.poll_with(&[probe(T_2216, 143.1, alone())]);
    assert_eq!(rules(&a), vec![("c1_decode", false)]);
    let m = &a[0].message;
    assert!(m.contains("3 consecutive valid idle probes"), "{m}");
    assert!(m.contains("19:35:29Z 125.2, 19:55:38Z 140.0, 22:16:04Z 143.1"), "{m}");
    assert!(!m.contains("19:50:38Z"), "the contended probe is not one of them: {m}");
}

// ---- card #266: the contention detector itself, WITHOUT the #261 TTFT rule ----
// The stored 5 s samples either side of the 22:16:04Z probe (the live collector DB, read-only):
//   22:16:04  running 0  requests_total 3637  prompt_tokens_total 1,151,725,834  generation_tokens_total 676,615
//   22:16:09  running 0  requests_total 3639  prompt_tokens_total 1,152,187,590  generation_tokens_total 676,771
// `running` read 0 throughout although the engine log showed `#running-req: 1` 22:14-22:19Z: the
// gauge is blind to that agent loop. Two requests finished: the probe (128 tokens) and a foreign
// one (461,756 prompt tokens, 28 generated). The engine counts tokens when a request FINISHES.

/// what the tight before/after reads could see when the "after" read landed between the foreign
/// request's finish and the probe's own: +28 generated, "less than the probe's own 128"
fn row_2216z_counters_only() -> ProbeEvidence {
    ProbeEvidence {
        ttft_ms: 259.8,
        ttft_baseline_ms: None, // the TTFT rule is OFF
        running_after: Some(0.0),
        gen_tokens_before: Some(676_615.0),
        gen_tokens_after: Some(676_643.0),
        probe_tokens: 128.0,
        ..alone()
    }
}

#[test]
fn the_2216z_probe_is_contended_with_the_ttft_rule_off() {
    let (reason, detail) = validate(&row_2216z_counters_only(), 3.0).unwrap_err();
    assert_eq!(reason, "contended", "{detail}");
    assert!(detail.contains("grew by 28"), "{detail}");
    let rec = probe(T_2216, 143.1, row_2216z_counters_only());
    assert!(!rec.is_reading(), "22:16Z must not be a C1 reading even with no idle-TTFT median");
}

#[test]
fn the_2216z_probe_after_the_settle_read_is_contended_on_requests_and_prompt_tokens() {
    // the settle read waits until the probe's own request is counted: all of 22:16:09's growth
    let settled = ProbeEvidence {
        gen_tokens_after: Some(676_771.0),
        requests_before: Some(3637.0),
        requests_after: Some(3639.0),
        prompt_tokens_before: Some(1_151_725_834.0),
        prompt_tokens_after: Some(1_152_187_590.0),
        ..row_2216z_counters_only()
    };
    let (reason, detail) = validate(&settled, 3.0).unwrap_err();
    assert_eq!(reason, "contended");
    assert!(detail.contains("1 other request"), "{detail}");
    // requests alone (a foreign request with a tiny prompt and the probe's exact token count)
    let requests_only = ProbeEvidence { prompt_tokens_after: Some(1_151_725_834.0 + 40.0), gen_tokens_after: Some(676_615.0 + 128.0), ..settled.clone() };
    assert_eq!(validate(&requests_only, 3.0).unwrap_err().0, "contended");
    // prompt tokens alone (the foreign request's finish not yet counted, its prompt already was)
    let prompt_only = ProbeEvidence { requests_after: Some(3638.0), gen_tokens_after: Some(676_615.0 + 128.0), ..settled };
    let (reason, detail) = validate(&prompt_only, 3.0).unwrap_err();
    assert_eq!(reason, "contended");
    assert!(detail.contains("grew by 461756"), "{detail}");
}

#[test]
fn a_clean_probe_passes_whether_or_not_its_own_request_was_counted_yet() {
    let clean = ProbeEvidence {
        running_after: Some(0.0),
        gen_tokens_before: Some(1000.0),
        requests_before: Some(50.0),
        prompt_tokens_before: Some(9000.0),
        probe_tokens: 128.0,
        ..alone()
    };
    // counted: +1 request, a sentence of prompt, exactly its own answer
    let counted = ProbeEvidence { gen_tokens_after: Some(1128.0), requests_after: Some(51.0), prompt_tokens_after: Some(9040.0), ..clean.clone() };
    assert!(validate(&counted, 3.0).is_ok());
    // not counted yet: nothing moved
    let not_yet = ProbeEvidence { gen_tokens_after: Some(1000.0), requests_after: Some(50.0), prompt_tokens_after: Some(9000.0), ..clean };
    assert!(validate(&not_yet, 3.0).is_ok());
}

/// card #264 (a): the serve box's / filled to 100% (2026-09-23, 125 leaked build volumes) and
/// nothing alerted. The collector now reads its own / (statvfs) every poll: `warn` at >= 90% used,
/// `page` at >= 97%, each held `disk_secs` (60 s) like every sustained rule, and RECOVERED only
/// once it drops a margin (2 points) below its own line - so a disk sitting on 90.0% cannot flap.
/// No reading holds the state (never a recovery).
#[test]
fn a_filling_disk_warns_at_90_pages_at_97_and_recovers_below_the_margin() {
    use lss_core::rules::DiskObs;
    let gb = 1_000_000_000u64;
    let at = |pct: f64| move |o: &mut Observation| {
        o.disk = Some(DiskObs { path: "/".into(), total_bytes: 1000 * gb, avail_bytes: ((100.0 - pct) * 10.0) as u64 * gb });
    };
    let mut s = Sim::new();
    assert!(s.run(120, at(89.0)).is_empty(), "89% is fine");
    // 91%: pending for 60 s, then one warn
    assert!(s.run(55, at(91.0)).is_empty(), "held 55 s: not yet");
    let e = s.run(10, at(91.0));
    assert_eq!(rules(&e), vec![("disk_full", false)], "{e:?}");
    assert_eq!(e[0].severity, Severity::Warn);
    assert!(e[0].message.contains("91") && e[0].message.contains("90 GB free") && e[0].message.contains("/"), "{}", e[0].message);
    assert!(s.engine.firing(s.now).contains(&"disk_full".to_string()));
    // 95% is still only the warn; 97.5% held 60 s pages
    assert!(s.run(120, at(95.0)).is_empty());
    let e = s.run(65, at(97.5));
    assert_eq!(rules(&e), vec![("disk_full_page", false)], "{e:?}");
    assert_eq!(e[0].severity, Severity::Page);
    // hysteresis: 96% is below 97 but inside the 2-point margin - still paging, no flap
    assert!(s.run(120, at(96.0)).is_empty(), "96% does not recover the page (margin 2)");
    let e = s.run(10, at(94.0));
    assert_eq!(rules(&e), vec![("disk_full_page", true)], "94% recovers the page: {e:?}");
    assert!(e[0].message.starts_with("RECOVERED: "), "{}", e[0].message);
    // the warn holds at 89% (inside its margin) and recovers at 87.5%
    assert!(s.run(60, at(89.0)).is_empty(), "89% does not recover the warn (margin 2)");
    let e = s.run(10, at(87.5));
    assert_eq!(rules(&e), vec![("disk_full", true)], "{e:?}");
    assert!(!s.engine.firing(s.now).iter().any(|r| r.starts_with("disk_full")));
    // no reading: nothing fires and nothing recovers
    let e = s.run(120, |o: &mut Observation| o.disk = None);
    assert!(e.is_empty(), "{e:?}");
    // the rule rows (GET /rules, the ALERTS page) name the reading
    let _ = s.run(10, at(92.0));
    let rows = s.engine.rule_states(&s.cfg, s.now);
    let row = rows.iter().find(|r| r.rule == "disk_full").expect("a disk_full row");
    assert!(row.threshold.contains("90%") && row.value.contains("92") && row.value.contains("free"), "{row:?}");
    assert!(rows.iter().any(|r| r.rule == "disk_full_page" && r.threshold.contains("97%")), "{rows:?}");
}

#[test]
fn the_disk_thresholds_come_from_the_config() {
    use lss_core::rules::DiskObs;
    let mut s = Sim::new();
    s.cfg.disk_warn_pct = 50.0;
    let e = s.run(70, |o: &mut Observation| o.disk = Some(DiskObs { path: "/data".into(), total_bytes: 100, avail_bytes: 40 }));
    assert_eq!(rules(&e), vec![("disk_full", false)], "{e:?}");
    assert!(e[0].message.contains("/data"), "{}", e[0].message);
}

/// card #264 (verifier note): a filesystem under 1 GB (a tmpfs) read '0.0 GB free of 0.0 GB'.
#[test]
fn a_disk_is_described_in_mb_below_one_gb_and_in_gb_above() {
    use lss_core::rules::DiskObs;
    let d = |total: u64, avail: u64| DiskObs { path: "/".into(), total_bytes: total, avail_bytes: avail }.describe();
    assert_eq!(d(10_000_000, 3_000_000), "70.0% used (3 MB free of 10 MB)");
    assert_eq!(d(1_000_000_000_000, 999_000_000), "99.9% used (999 MB free of 1000 GB)");
    assert_eq!(d(1_000_000_000_000, 2_500_000_000), "99.8% used (2.5 GB free of 1000 GB)");
    assert_eq!(d(1_000_000_000_000, 90_000_000_000), "91.0% used (90 GB free of 1000 GB)");
}

/// card #264 (verifier note): rounding at a unit boundary switches unit - 999,999,999 bytes is
/// '1.0 GB' (was '1000 MB'), 9.95 GB is '10 GB' (never '10.0 GB').
#[test]
fn a_size_that_rounds_up_to_the_next_unit_is_said_in_that_unit() {
    use lss_core::rules::DiskObs;
    let size = |b: u64| {
        let s = DiskObs { path: "/".into(), total_bytes: b, avail_bytes: b }.describe();
        s.trim_start_matches("0.0% used (").split(" free of ").next().unwrap().to_string()
    };
    for (bytes, want) in [
        (999_499_999, "999 MB"),
        (999_500_000, "1.0 GB"),
        (999_999_999, "1.0 GB"),
        (1_000_000_000, "1.0 GB"),
        (9_949_999_999, "9.9 GB"),
        (9_950_000_000, "10 GB"),
        (9_999_999_999, "10 GB"),
        (10_000_000_000, "10 GB"),
        (10_499_999_999, "10 GB"),
        (10_500_000_000, "11 GB"),
    ] {
        assert_eq!(size(bytes), want, "{bytes} bytes");
    }
}
