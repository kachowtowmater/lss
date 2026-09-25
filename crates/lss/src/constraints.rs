//! #177: WHAT IS WORST RIGHT NOW, AND WHAT IT MEANS FOR YOU - page 1's verdict line, as a
//! projection of card #179's `lss_core::readings` (after nixfred/pulse, the owner's reference:
//! "incorporate his architecture into our llm status").
//!
//! #179 step 3: this file no longer PRODUCES readings. The producer is `lss_core::readings::all`
//! (one `readings()` per domain, scored on the one scale, every explanation in its `detail`);
//! this file only projects it into page 1's verdict line. Everything the #177 producer judged
//! (the silent engine, the gateway-vs-engine hold, failed and turned-away requests, the owner's
//! thermal and queue alert lines) moved into lss-core with it.

use lss_core::model::Status;
use lss_core::readings::{self, leading, rank, Reading, LOW};

/// Everything page 1 can judge from the status document - the lss-core readings, judged at the
/// moment the collector wrote it.
pub fn readings(s: &Status) -> Vec<Reading> {
    readings::all(s, s.generated_at)
}

/// The verdict line: the worst reading (via `readings::leading`, which never picks an
/// informational row and needs a score of at least LOW), and the other readings at or above LOW,
/// worst first. `leader == None` = nothing is constrained, and `calm` says so in words.
pub struct Headline {
    pub leader: Option<Reading>,
    pub also: Vec<String>,
    pub calm: String,
}

pub fn headline(s: &Status) -> Headline {
    headline_with(s, None)
}

/// `headline`, with the key of the reading that led LAST time. `readings::leading` then applies
/// its 0.08 hysteresis: a challenger must beat the incumbent by that margin to take the line, so
/// two close readings do not swap places every refresh. Card #179 built the margin; nothing on
/// screen ever passed an incumbent, so it never applied (found by the orchestrator's audit).
pub fn headline_with(s: &Status, incumbent: Option<&str>) -> Headline {
    let all = readings(s);
    let leader = leading(&all, incumbent).cloned();
    let also: Vec<String> = rank(&all)
        .into_iter()
        .filter(|r| r.severity >= LOW && leader.as_ref().is_none_or(|l| l.key != r.key))
        .map(|r| r.label.clone())
        .collect();
    let sv = &s.serve;
    let hottest = s.gpus.iter().filter_map(|g| g.sample.temp_c.map(|t| (g.sample.index, t))).max_by(|a, b| a.1.total_cmp(&b.1));
    let hot = hottest.map_or_else(String::new, |(i, t)| format!(" \u{b7} hottest GPU{i} {t:.0}C"));
    // card #180 gate 3 (a clean install against Ollama): the calm sentence said "0/0 slots busy
    // · KV 0%" for an engine that reports neither - a made-up zero in the one line always on
    // screen. It names only what the engine actually publishes.
    let mut parts = vec!["nothing is constrained".to_string()];
    if !sv.is_na("running") {
        parts.push(if sv.slots > 0 { format!("{:.0}/{} slots busy", sv.running, sv.slots) } else { format!("{:.0} running", sv.running) });
    }
    if !sv.is_na("kv_usage") {
        parts.push(format!("KV {:.0}%", sv.kv_usage * 100.0));
    }
    let calm = format!("{}{hot}", parts.join(" \u{b7} "));
    Headline { leader, also, calm }
}

/// The band word for the one line it leads ("CRITICAL", "HIGH", "WATCH", "OFFLINE") - always
/// from #179's `Band::word`, never spelled here.
pub fn band_word(r: &Reading) -> String {
    r.band().word().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::gpu::GpuSample;
    use lss_core::model::GpuStatus;
    use lss_core::readings::Band;

    fn calm() -> Status {
        let mut s = Status::default();
        s.serve.up = true;
        s.serve.slots = 8;
        s.serve.running = 2.0;
        s.serve.kv_usage = 0.3;
        s.serve.secs_since_last_token = Some(1);
        s.gate.up = true;
        s.thresholds.thermal_temp_c = 90.0;
        s.thresholds.queue_reqs = 4.0;
        s.gpus = (0..4).map(|i| GpuStatus { sample: GpuSample { index: i, temp_c: Some(60.0), ..Default::default() }, ..Default::default() }).collect();
        s
    }

    fn lead(s: &Status) -> Option<Reading> {
        headline(s).leader
    }

    /// verifier-3's ruler for #177, on #179's one scale: each fault ALONE leads with its own
    /// resource, and nothing wrong leads with nothing (the calm sentence says so in words).
    #[test]
    fn each_fault_alone_leads_with_its_own_resource_on_the_one_scale() {
        let h = headline(&calm());
        assert!(h.leader.is_none() && h.also.is_empty(), "calm must have no leader");
        assert!(h.calm.starts_with("nothing is constrained"), "{}", h.calm);

        let mut s = calm();
        s.gpus[2].sample.temp_c = Some(92.0);
        let r = lead(&s).expect("a hot GPU leads");
        assert_eq!(r.key, "gpu.2.temp");
        assert!(matches!(r.band(), Band::High | Band::Critical), "over the alert line is high or worse: {:?}", r.band());

        let mut s = calm();
        s.serve.kv_usage = 0.98;
        assert_eq!(lead(&s).unwrap().key, "serve.kv");
        let mut s = calm();
        s.serve.kv_usage = 0.90;
        assert_eq!(lead(&s).unwrap().band(), Band::Watch, "90% KV is watch, not yet refusing");

        let mut s = calm();
        s.lanes.trusted.budget_tokens = Some(1_000_000);
        s.lanes.trusted.inflight_tokens = 900_000;
        // #196: the gateway readings are PER LANE, so the headline names WHICH lane's budget is
        // full - a different problem with a different fix from the other lane's
        assert_eq!(lead(&s).unwrap().key, "gate.trusted.budget");

        let mut s = calm();
        s.lanes.trusted.waiters = 3;
        let r = lead(&s).unwrap();
        assert_eq!(r.key, "gate.waiters");
        assert!(r.detail.contains("not the GPUs"), "it must say WHOSE limit it is: {}", r.detail);

        let mut s = calm();
        s.serve.running = 8.0;
        assert_eq!(lead(&s).unwrap().key, "serve.slots");

        let mut s = calm();
        s.serve.up = false;
        let r = lead(&s).unwrap();
        assert_eq!((r.key.as_str(), r.band()), ("serve.up", Band::Offline));

        let mut s = calm();
        s.serve.secs_since_last_token = Some(75);
        let r = lead(&s).unwrap();
        assert_eq!(r.key, "serve.silent");
        assert!(r.detail.contains("for 75s") && r.detail.contains("GPU hangs"), "{}", r.detail);
    }

    /// The live screen passes the last leader back in, so a challenger inside the 0.08 margin
    /// does not take the line; one clearly worse does. A GPU at its 90C alert line scores 0.75;
    /// KV 96% scores 0.80 (inside the margin), KV 99% 0.95 (outside it).
    #[test]
    fn the_leader_holds_against_a_challenger_inside_the_hysteresis_margin() {
        let mut s = calm();
        s.gpus[2].sample.temp_c = Some(90.0);
        let first = headline_with(&s, None).leader.unwrap();
        assert_eq!(first.key, "gpu.2.temp");
        s.serve.kv_usage = 0.96;
        assert_eq!(headline_with(&s, None).leader.unwrap().key, "serve.kv", "with no history the worse reading leads");
        assert_eq!(headline_with(&s, Some(&first.key)).leader.unwrap().key, "gpu.2.temp", "inside the margin the incumbent keeps the line");
        s.serve.kv_usage = 0.99;
        assert_eq!(headline_with(&s, Some(&first.key)).leader.unwrap().key, "serve.kv", "clearly worse takes it over");
    }

    #[test]
    fn failed_and_turned_away_requests_are_named_and_ignored_without_a_gateway() {
        let mut s = calm();
        s.lanes.trusted.codes_10m.c5xx = 6;
        let r = lead(&s).unwrap();
        assert_eq!(r.key, "gate.trusted.failed", "#196: which lane failed, not that someone did");
        assert!(r.detail.contains("Your own agents"), "and whose requests those were: {}", r.detail);
        let mut s = calm();
        s.lanes.public.codes_10m.c4xx = 10;
        let r = lead(&s).unwrap();
        assert_eq!(r.key, "gate.public.turned");
        assert!(r.detail.contains("Outside callers"), "{}", r.detail);
        // the other lane being clean is not silence about it: it has its own row, scoring 0
        let clean = crate::constraints::readings(&s).into_iter().find(|r| r.key == "gate.trusted.turned").unwrap();
        assert_eq!(clean.severity, 0.0);
        let mut s = calm();
        s.gate.absent = true;
        s.lanes.public.codes_10m.c5xx = 3;
        assert!(lead(&s).is_none(), "no gateway: lane counters mean nothing and are not judged");
    }

    /// two faults at once: the worse one leads, the other is named in `also` (never dropped); a
    /// GPU excluded from thermal alerting (a known-hot card) never leads on heat.
    #[test]
    fn the_calm_sentence_names_only_what_the_engine_reports() {
        let h = headline(&calm());
        assert!(h.calm.contains("2/8 slots busy") && h.calm.contains("KV 30%"), "{}", h.calm);
        let mut s = calm();
        s.serve.not_reported = vec!["running".into(), "kv_usage".into()];
        let h = headline(&s);
        assert!(!h.calm.contains("slots") && !h.calm.contains("KV"), "an engine that reports neither must not show 0/0 or KV 0%: {}", h.calm);
        assert!(h.calm.starts_with("nothing is constrained"), "{}", h.calm);
    }

    #[test]
    fn the_worst_leads_and_nothing_is_dropped() {
        let mut s = calm();
        s.serve.kv_usage = 0.90;
        s.gpus[1].sample.temp_c = Some(95.0);
        let h = headline(&s);
        assert_eq!(h.leader.unwrap().key, "gpu.1.temp");
        assert!(h.also.contains(&"KV cache".to_string()), "{:?}", h.also);
        let mut s = calm();
        s.gpus[0].thermal_excluded = true;
        s.gpus[0].sample.temp_c = Some(99.0);
        assert!(lead(&s).is_none());
    }
}
