//! card #450: the disk_growth rule, beside the %-used rules.
use lss_core::config::RulesConfig;
use lss_core::rules::{AlertEvent, Engine, DiskObs, Observation, Severity};

struct Sim {
    cfg: RulesConfig,
    engine: Engine,
    now: i64,
}

impl Sim {
    fn new() -> Self {
        // #47, 2026-09-21, as in tests/rules.rs: dial the c1_stale rule out - these tests never
        // send probes, so a default c1_stale_probe_intervals would fire "no valid probe" noise.
        Self { cfg: RulesConfig { c1_stale_probe_intervals: 1_000_000, ..Default::default() }, engine: Engine::default(), now: 1_000_000 }
    }
    fn feed(&mut self, secs: i64, avail_gb: u64) -> Vec<AlertEvent> {
        let mut out = Vec::new();
        let end = self.now + secs;
        while self.now < end {
            self.now += 5;
            let o = Observation {
                now: self.now,
                serve_up: true,
                gate_up: true,
                queue: Some(0.0),
                trusted_waiters: Some(0),
                gpus: Some((0..4).map(|i| lss_core::rules::GpuObs { index: i, temp_c: Some(60.0), throttle_mask: 0 }).collect()),
                probe_interval_secs: 300,
                disk: Some(DiskObs { path: "/".into(), total_bytes: 2000 * 1_000_000_000, avail_bytes: avail_gb * 1_000_000_000 }),
                ..Default::default()
            };
            out.extend(self.engine.evaluate(&self.cfg, &o));
        }
        out
    }
}

/// 120 GB gone in 5 min: a runaway fill that never reaches the 90% line must still warn.
#[test]
fn disk_growth_fires_on_a_fast_drop() {
    let mut s = Sim::new();
    // 89% used, steady, for an hour - the %-used rules must stay quiet the whole way
    assert!(s.feed(3600, 1100).is_empty(), "a steady disk is silent: {}", s.engine.firing(s.now).join(","));
    assert!(s.engine.firing(s.now).is_empty());
    // 120 GB in 5 min: pending ~60 s, then one warn - and disk_full/disk_full_page stay silent
    assert!(s.feed(55, 980).is_empty(), "held 55 s: not yet");
    let e = s.feed(15, 980);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].rule, "disk_growth");
    assert_eq!(e[0].severity, Severity::Warn);
    assert!(!e[0].recovered);
    assert!(e[0].message.contains("filling fast") && e[0].message.contains("120.0 GB") && e[0].message.contains("/"), "{}", e[0].message);
    let firing = s.engine.firing(s.now);
    assert!(firing.contains(&"disk_growth".to_string()), "{firing:?}");
    assert!(!firing.iter().any(|r| r.starts_with("disk_full")), "{firing:?}");
    assert_eq!((s.cfg.disk_growth_gb, s.cfg.disk_growth_secs), (100.0, 600));
}

/// 80 GB over 10 min: a real but slow trend that the %-used rules will catch in time - no alert.
#[test]
fn disk_growth_stays_quiet_on_a_slow_drop() {
    let mut s = Sim::new();
    assert!(s.feed(3600, 1100).is_empty());
    assert!(s.feed(600, 1020).is_empty(), "80 GB in 10 min is under the 100 GB line: {e:?}", e = s.engine.firing(s.now));
    assert!(s.engine.firing(s.now).is_empty());
    // and it keeps scrolling along without ever tripping, windows rolling as points age out
    assert!(s.feed(1200, 950).is_empty());
    assert!(s.engine.firing(s.now).is_empty());
}

/// Recovery: the drop over the window falls back under the line (the window keeps the
/// high-water avail, so recovery needs the aged-out points gone, not just a tick up).
#[test]
fn disk_growth_recovers_when_the_drop_stops() {
    let mut s = Sim::new();
    assert!(s.feed(3600, 1100).is_empty());
    assert!(s.feed(55, 980).is_empty());
    let e = s.feed(10, 980);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].rule, "disk_growth");
    // stop the fill: ~60 s more only ages points a little - still 120 GB inside the window
    assert!(s.feed(60, 980).is_empty(), "still over the line inside the window: {e:?}", e = s.engine.firing(s.now));
    // the rest of the window: the 1100-GB points age out one by one, and the moment the
    // biggest drop over the remaining window falls under 100 GB the rule recovers
    // (the last 1100-GB point leaves the window 540 s after the fill stopped)
    let e = s.feed(540, 980);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].rule, "disk_growth");
    assert!(e[0].recovered, "{}", e[0].message);
    assert!(e[0].message.starts_with("RECOVERED: "), "{}", e[0].message);
    assert!(e[0].message.contains("no longer filling fast"), "{}", e[0].message);
    assert!(!s.engine.firing(s.now).contains(&"disk_growth".to_string()));
    // and it stays recovered while nothing more happens
    assert!(s.feed(120, 980).is_empty());
}
