//! HEADROOM: a plain estimate of "about N more simultaneous users before waits start", with
//! the reasoning. Three independent ceilings are worked out from what was measured and the
//! LOWEST one is the answer:
//!
//! * slots   the engine runs at most `slots` requests at once; the next one waits in the queue
//! * memory  the KV cache: what each running request used at the busiest moment, scaled up
//! * speed   the concurrency curve: where total speed stops growing (more users only slow each
//!   other down), or where each user's speed falls under the `min_tok_s_per_user` target
//!
//! "Busy now" is the concurrency the server typically runs at when it is busy: the level under
//! which 95 % of the serving samples sit.

use crate::loadout::{CurveRow, Saturation, MIN_LEVEL_SAMPLES};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ceiling {
    /// `slots` | `memory` | `speed`
    pub kind: String,
    /// users at once this ceiling allows; null = not measurable yet
    pub users: Option<u64>,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Headroom {
    /// null = not enough evidence yet
    pub more_users: Option<u64>,
    /// how many requests run at once when the server is busy (p95 of the serving samples)
    pub busy_users: Option<u64>,
    /// which ceiling is the lowest: `slots` | `memory` | `speed`
    pub limited_by: String,
    pub sentence: String,
    pub ceilings: Vec<Ceiling>,
}

#[derive(Debug, Clone, Default)]
pub struct HeadroomInputs<'a> {
    pub slots: u64,
    /// the concurrency curve, live rows merged with bench rows
    pub curve: &'a [CurveRow],
    /// the LIVE rows only (their sample counts say how busy the server really is)
    pub live: &'a [CurveRow],
    /// 0..=1, the highest KV use seen
    pub kv_peak: f64,
    pub saturation: Option<&'a Saturation>,
    /// 0 = no speed target
    pub min_tok_s_per_user: f64,
}

/// The level under which 95 % of the serving samples sit.
pub fn busy_level(live: &[CurveRow]) -> Option<u64> {
    let total: u64 = live.iter().map(|r| r.samples).sum();
    if total < MIN_LEVEL_SAMPLES {
        return None;
    }
    let mut seen = 0u64;
    for r in live {
        seen += r.samples;
        if seen as f64 >= total as f64 * 0.95 {
            return Some(r.running);
        }
    }
    live.last().map(|r| r.running)
}

pub fn estimate(i: &HeadroomInputs) -> Headroom {
    let busy = busy_level(i.live);
    let peak_running = i.live.iter().filter(|r| r.samples > 0).map(|r| r.running).max();
    let mut ceilings = Vec::new();
    ceilings.push(if i.slots > 0 {
        Ceiling { kind: "slots".into(), users: Some(i.slots), why: format!("the engine runs at most {} requests at once; the next one waits", i.slots) }
    } else {
        Ceiling { kind: "slots".into(), users: None, why: "the engine's slot count is not known".into() }
    });
    ceilings.push(match peak_running {
        Some(peak) if i.kv_peak > 0.005 => {
            let per_user = i.kv_peak / peak as f64;
            // leave a tenth free: a full cache retracts (pauses) running requests
            let users = ((0.9 / per_user).floor() as u64).max(peak);
            Ceiling { kind: "memory".into(), users: Some(users), why: format!("memory (KV) peaked at {:.0}% with up to {peak} running: about {:.0}% each, so about {users} fit before it is 90% full", i.kv_peak * 100.0, per_user * 100.0) }
        }
        _ => Ceiling { kind: "memory".into(), users: None, why: "memory (KV) use has not been seen under load yet".into() },
    });
    let all_live = crate::loadout::live_samples(i.curve);
    let by_target = (i.min_tok_s_per_user > 0.0).then(|| i.curve.iter().filter(|r| crate::loadout::qualifies(r, all_live)).filter(|r| r.per_request_tok_s.is_some_and(|t| t >= i.min_tok_s_per_user)).map(|r| r.running).max()).flatten();
    let measured_top = i.curve.iter().filter(|r| r.tok_s.is_some() && crate::loadout::qualifies(r, all_live)).map(|r| r.running).max();
    // The SAME bar as the ADVICE rule: a speed ceiling is claimed from a benchmark, or from at
    // least three qualified live levels. With fewer, the headline comes from slots and memory -
    // ADVICE and HEADROOM must never disagree on the same evidence (2026-09-20).
    let (levels, from_bench) = crate::loadout::curve_evidence(i.curve);
    let solid = from_bench || levels >= 3;
    ceilings.push(match (i.saturation.filter(|_| solid), by_target) {
        (Some(Saturation::After { users, total_tok_s, .. }), target) => {
            let cap = target.map_or(*users, |t| t.min(*users));
            let why = if target.is_some_and(|t| t < *users) { format!("each user still gets {:.0} tok/s or more up to {cap} at once (the target)", i.min_tok_s_per_user) } else { format!("total speed stops growing after {users} at once (about {total_tok_s:.0} tok/s): more users only slow each other down") };
            Ceiling { kind: "speed".into(), users: Some(cap), why }
        }
        (Some(Saturation::StillGrowing { users, .. }), target) => {
            // no ceiling found up to the most that was measured: that level is a floor, not a cap
            let top = measured_top.unwrap_or(*users);
            match target.filter(|t| *t < top) {
                Some(t) => Ceiling { kind: "speed".into(), users: Some(t), why: format!("each user still gets {:.0} tok/s or more up to {t} at once (the target)", i.min_tok_s_per_user) },
                None => Ceiling { kind: "speed".into(), users: None, why: format!("total speed was still growing at {users} at once, the most measured: no speed ceiling found yet") },
            }
        }
        _ if !solid && levels > 0 => Ceiling { kind: "speed".into(), users: None, why: format!("only {levels} load level{} has enough traffic to judge: not enough to say where speed stops growing (`lss bench quick` measures it)", if levels == 1 { "" } else { "s" }) },
        _ => Ceiling { kind: "speed".into(), users: None, why: "too little traffic at different loads to see where speed stops growing (lss bench quick measures it)".into() },
    });
    let lowest = ceilings.iter().filter_map(|c| c.users.map(|u| (u, c.kind.clone()))).min_by_key(|(u, _)| *u);
    let (more, limited_by, sentence) = match (lowest, busy) {
        (Some((cap, kind)), Some(b)) => {
            let more = cap.saturating_sub(b);
            let what = match kind.as_str() {
                "slots" => "the slots run out",
                "memory" => "memory (KV) runs out",
                _ => "answers get slower for everyone",
            };
            let s = if more == 0 { format!("no room for more simultaneous users: it is already at its limit - when busy it runs {b} at once, and {what} beyond {cap}") } else { format!("about {more} more simultaneous user{} before waits start: when busy it runs {b} at once, and {what} at {cap}", if more == 1 { "" } else { "s" }) };
            (Some(more), kind, s)
        }
        (Some((cap, kind)), None) => (Some(cap), kind, format!("about {cap} simultaneous users before waits start (hardly any traffic yet, so all of it is free)")),
        (None, _) => (None, String::new(), "not enough evidence yet for a headroom estimate".to_string()),
    };
    Headroom { more_users: more, busy_users: busy, limited_by, sentence, ceilings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(levels: &[(u64, u64, f64)], source: &str) -> Vec<CurveRow> {
        levels.iter().map(|(n, samples, total)| CurveRow { running: *n, samples: *samples, tok_s: (*total > 0.0).then_some(*total), per_request_tok_s: (*total > 0.0).then(|| total / *n as f64), source: source.into(), ..Default::default() }).collect()
    }

    /// 2026-09-20, live: `1 at once: 948 samples, 252 tok/s` and `2 at once: 22 samples, 117 tok/s`
    /// (the 22 were moments a second request was READING a 124k-token prompt). The screen said
    /// "no room for more simultaneous users ... at 1" about a server that benches C8 at 712 tok/s.
    #[test]
    fn two_dozen_samples_of_a_second_request_are_not_a_ceiling() {
        let live = rows(&[(1, 948, 252.0), (2, 22, 117.0)], "live");
        let points = crate::loadout::curve_points(&live);
        assert_eq!(points, vec![(1, 252.0)], "22 samples (2% of the traffic, under two minutes) are not a level");
        let sat = crate::loadout::saturation(&points);
        assert_eq!(sat, Saturation::NotEnough);
        assert_eq!(crate::loadout::curve_evidence(&live), (1, false));
        let h = estimate(&HeadroomInputs { slots: 8, curve: &live, live: &live, kv_peak: 0.33, saturation: Some(&sat), min_tok_s_per_user: 30.0 });
        let speed = h.ceilings.iter().find(|c| c.kind == "speed").unwrap();
        assert_eq!(speed.users, None, "{}", speed.why);
        assert!(speed.why.contains("only 1 load level has enough traffic to judge") && speed.why.contains("lss bench quick"), "{}", speed.why);
        assert!(!h.sentence.contains("no room") && h.more_users.is_some_and(|m| m >= 1), "a healthy server is never told it is full from noise: {}", h.sentence);
        assert_ne!(h.limited_by, "speed");
        // ten minutes at a real share of the traffic DOES count
        let real = rows(&[(1, 900, 252.0), (2, 200, 260.0)], "live");
        assert_eq!(crate::loadout::curve_points(&real).len(), 2);
        // and a benchmark cell counts whatever its sample count
        let bench = [rows(&[(1, 948, 252.0)], "live"), rows(&[(8, 0, 712.0)], "bench")].concat();
        assert_eq!(crate::loadout::saturation(&crate::loadout::curve_points(&bench)), Saturation::StillGrowing { users: 8, total_tok_s: 712.0 });
    }

    #[test]
    fn the_wording_when_it_really_is_at_its_limit() {
        let live = rows(&[(1, 300, 190.0), (2, 4000, 200.0), (3, 300, 201.0)], "live");
        let sat = crate::loadout::saturation(&crate::loadout::curve_points(&live));
        let h = estimate(&HeadroomInputs { slots: 8, curve: &live, live: &live, kv_peak: 0.0, saturation: Some(&sat), min_tok_s_per_user: 0.0 });
        assert_eq!(h.sentence, "no room for more simultaneous users: it is already at its limit - when busy it runs 3 at once, and answers get slower for everyone beyond 2");
    }

    #[test]
    fn the_lowest_ceiling_wins_and_the_reasoning_is_kept() {
        // mostly 1-2 at once, sometimes 3; speed flat after 6; KV 33 % at 3 running -> 8 fit
        let live = rows(&[(1, 8000, 190.0), (2, 1500, 360.0), (3, 400, 480.0), (4, 60, 600.0), (5, 0, 0.0), (6, 30, 690.0), (7, 0, 0.0), (8, 10, 700.0)], "live");
        // the levels live traffic hardly reaches were measured by `lss bench`: those count
        let curve: Vec<CurveRow> = live.iter().cloned().map(|mut r| {
            if r.running >= 3 && r.tok_s.is_some() {
                r.source = "bench".into();
            }
            r
        }).collect();
        let sat = crate::loadout::saturation_of(&curve, Some(700.0));
        assert_eq!(sat, Saturation::After { users: 6, total_tok_s: 690.0, from_bench: true });
        let h = estimate(&HeadroomInputs { slots: 8, curve: &curve, live: &live, kv_peak: 0.33, saturation: Some(&sat), min_tok_s_per_user: 0.0 });
        assert_eq!(h.busy_users, Some(2), "95 % of the serving samples have 2 or fewer running");
        assert_eq!((h.more_users, h.limited_by.as_str()), (Some(4), "speed"));
        assert!(h.sentence.starts_with("about 4 more simultaneous users before waits start"), "{}", h.sentence);
        assert_eq!(h.ceilings.iter().map(|c| (c.kind.as_str(), c.users)).collect::<Vec<_>>(), [("slots", Some(8)), ("memory", Some(21)), ("speed", Some(6))]);
        // a speed target lowers the speed ceiling: 600/4 = 150 tok/s each is the last level >= 140
        let h = estimate(&HeadroomInputs { slots: 8, curve: &curve, live: &live, kv_peak: 0.33, saturation: Some(&sat), min_tok_s_per_user: 140.0 });
        assert_eq!((h.more_users, h.ceilings[2].users), (Some(2), Some(4)));
        assert!(h.ceilings[2].why.contains("140 tok/s"), "{}", h.ceilings[2].why);
    }

    #[test]
    fn memory_or_slots_can_be_the_limit_and_full_is_said_plainly() {
        let live = rows(&[(1, 100, 100.0), (2, 100, 200.0), (3, 100, 300.0), (4, 4000, 400.0)], "live");
        let growing = Saturation::StillGrowing { users: 4, total_tok_s: 400.0 };
        let h = estimate(&HeadroomInputs { slots: 4, curve: &live, live: &live, kv_peak: 0.2, saturation: Some(&growing), min_tok_s_per_user: 0.0 });
        assert_eq!((h.more_users, h.limited_by.as_str(), h.busy_users), (Some(0), "slots", Some(4)));
        assert!(h.sentence.starts_with("no room for more simultaneous users"), "{}", h.sentence);
        let h = estimate(&HeadroomInputs { slots: 32, curve: &live, live: &live, kv_peak: 0.8, saturation: Some(&growing), min_tok_s_per_user: 0.0 });
        assert_eq!((h.limited_by.as_str(), h.ceilings[1].users), ("memory", Some(4)), "80 % with 4 running = 20 % each: 4 fit under 90 %");
    }

    #[test]
    fn no_evidence_is_said_not_guessed() {
        let h = estimate(&HeadroomInputs::default());
        assert_eq!((h.more_users, h.busy_users), (None, None));
        assert!(h.sentence.contains("not enough evidence"));
        // slots known but no traffic: everything is free
        let h = estimate(&HeadroomInputs { slots: 8, ..Default::default() });
        assert_eq!(h.more_users, Some(8));
    }
}
