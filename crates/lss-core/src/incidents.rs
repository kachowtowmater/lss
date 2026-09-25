//! Incident tracking: turns successive observations into open/close operations.
//! The tracker is serialisable so an incident that is open across a collector restart
//! is closed by the new process, and a serve restart that happened while the collector
//! was down is still noticed.

use crate::docker::ContainerState;
use crate::timeutil::fmt_duration;
use crate::xid::{xid_hint, XidEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const KIND_SERVE_DOWN: &str = "serve_down";
pub const KIND_GATE_DOWN: &str = "gate_down";
pub const KIND_CONTAINER_RESTART: &str = "container_restart";
pub const KIND_XID: &str = "xid";
/// An `lss bench` run: a WINDOW, not a fault. Open while the benchmark loads the server, so the
/// charts can be annotated and the rules a benchmark would trip stand down. It is kept out of
/// the downtime arithmetic (it is not an outage).
pub const KIND_BENCH: &str = "bench";
/// #22, 2026-09-21: `lss maintenance start|stop "reason"` (`maintenance::start`/`stop`). A
/// deliberate, announced WINDOW, not a fault - open for its whole duration so the timeline and
/// charts (an `M` marker) show it plainly. Kept out of the downtime arithmetic exactly like
/// `KIND_BENCH` (only `KIND_SERVE_DOWN`/`KIND_GATE_DOWN` are ever summed as outage time). The
/// restart it is meant to cover still shows as its own `KIND_CONTAINER_RESTART` event - this
/// window does not replace that record, it only changes how `rules::eval_restarts` labels and
/// grades the restart while it is open: `info`/"planned", not `warn`.
pub const KIND_MAINTENANCE: &str = "maintenance";

/// Consecutive failed polls before a down incident opens (one slow poll is not an outage).
/// The incident's start is back-dated to the first miss.
pub const MISSES_TO_OPEN: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    pub id: i64,
    pub start: i64,
    /// None while the incident is still open.
    pub end: Option<i64>,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IncidentOp {
    Open { kind: &'static str, start: i64, detail: String },
    /// Closes the open incident of this kind.
    Close { kind: &'static str, end: i64, duration_secs: i64 },
    /// Point-in-time incident (start == end). `subject` is the container role ("serve" /
    /// "gate") for a restart and the GPU label for an Xid.
    /// `key` is what makes the event unique, when the wording must not: an Xid is
    /// (kind, GPU or PCI address, Xid number, kernel timestamp to the second), however its
    /// description is phrased by this or a later version. None = dedupe on (kind, ts, detail).
    Event { kind: &'static str, ts: i64, subject: String, detail: String, key: Option<String> },
}

/// The dedupe key of an Xid incident.
pub fn xid_key(gpu_label: &str, xid: u32, ts: i64) -> String {
    format!("xid|{gpu_label}|{xid}|{ts}")
}

/// The same key, recovered from a stored incident (`GPU1 Xid 154 (…) …` / `PCI 0000:99:00 Xid 31 …`):
/// rows written before the key existed are keyed by the one-off migration with this.
pub fn xid_key_from_detail(detail: &str, start: i64) -> Option<String> {
    let (label, rest) = detail.split_once(" Xid ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let xid: u32 = digits.parse().ok()?;
    (!label.is_empty()).then(|| xid_key(label.trim(), xid, start))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct DownTracker {
    misses: u32,
    first_miss: Option<i64>,
    open_since: Option<i64>,
}

impl DownTracker {
    fn step(&mut self, kind: &'static str, now: i64, up: bool, detail: &str, ops: &mut Vec<IncidentOp>) {
        if up {
            if let Some(start) = self.open_since.take() {
                ops.push(IncidentOp::Close { kind, end: now, duration_secs: now - start });
            }
            self.misses = 0;
            self.first_miss = None;
            return;
        }
        self.misses = self.misses.saturating_add(1);
        let first = *self.first_miss.get_or_insert(now);
        if self.open_since.is_none() && self.misses >= MISSES_TO_OPEN {
            self.open_since = Some(first);
            ops.push(IncidentOp::Open { kind, start: first, detail: detail.to_string() });
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncidentTracker {
    serve: DownTracker,
    gate: DownTracker,
    /// role ("serve" / "gate") → last seen container identity
    containers: BTreeMap<String, ContainerState>,
}

pub struct IncidentInput<'a> {
    pub now: i64,
    pub serve_up: bool,
    pub serve_detail: &'a str,
    pub gate_up: bool,
    pub gate_detail: &'a str,
    /// (role, state). A role whose container could not be inspected is simply absent.
    pub containers: &'a [(&'a str, ContainerState)],
    pub xids: &'a [XidEvent],
}

impl IncidentTracker {
    pub fn serve_down_since(&self) -> Option<i64> {
        self.serve.open_since
    }
    pub fn gate_down_since(&self) -> Option<i64> {
        self.gate.open_since
    }

    pub fn step(&mut self, input: &IncidentInput) -> Vec<IncidentOp> {
        let mut ops = Vec::new();
        self.serve.step(KIND_SERVE_DOWN, input.now, input.serve_up, input.serve_detail, &mut ops);
        self.gate.step(KIND_GATE_DOWN, input.now, input.gate_up, input.gate_detail, &mut ops);

        for (role, cur) in input.containers {
            if let Some(prev) = self.containers.get(*role) {
                if let Some(detail) = restart_detail(prev, cur) {
                    let ts = if cur.started_at > prev.started_at { cur.started_at } else { input.now };
                    ops.push(IncidentOp::Event { kind: KIND_CONTAINER_RESTART, ts, subject: (*role).to_string(), detail, key: None });
                }
            }
            // A container that is mid-restart reports started_at from its previous run;
            // remember it anyway - the next poll sees the new stamp and that is one event.
            self.containers.insert((*role).to_string(), cur.clone());
        }

        for x in input.xids {
            let detail = format!("{} Xid {} ({}) {}", x.gpu_label(), x.xid, xid_hint(x.xid), x.detail);
            ops.push(IncidentOp::Event { kind: KIND_XID, ts: x.ts, subject: x.gpu_label(), detail: detail.trim().to_string(), key: Some(xid_key(&x.gpu_label(), x.xid, x.ts)) });
        }
        ops
    }
}

/// Some(description) when `cur` is a different run of the container than `prev`.
fn restart_detail(prev: &ContainerState, cur: &ContainerState) -> Option<String> {
    if cur.name != prev.name {
        return Some(format!("{} replaced by {}", prev.name, cur.name));
    }
    if cur.restart_count != prev.restart_count {
        return Some(format!("{} RestartCount {} -> {}", cur.name, prev.restart_count, cur.restart_count));
    }
    if cur.started_at != prev.started_at && cur.started_at != 0 {
        let up_for = if prev.started_at > 0 && cur.started_at > prev.started_at {
            format!(" (previous run lasted {})", fmt_duration(cur.started_at - prev.started_at))
        } else {
            String::new()
        };
        return Some(format!("{} new StartedAt{}", cur.name, up_for));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ct(name: &str, restarts: u64, started: i64) -> ContainerState {
        ContainerState { name: name.into(), status: "running".into(), restart_count: restarts, started_at: started }
    }

    fn step(t: &mut IncidentTracker, now: i64, serve_up: bool, gate_up: bool, cts: &[(&str, ContainerState)], xids: &[XidEvent]) -> Vec<IncidentOp> {
        t.step(&IncidentInput { now, serve_up, serve_detail: "/v1/models: timeout", gate_up, gate_detail: "/gate/health: refused", containers: cts, xids })
    }

    #[test]
    fn serve_down_opens_after_two_misses_backdated_and_closes_with_duration() {
        let mut t = IncidentTracker::default();
        assert!(step(&mut t, 100, true, true, &[], &[]).is_empty());
        assert!(step(&mut t, 105, false, true, &[], &[]).is_empty(), "one miss is not an outage");
        let ops = step(&mut t, 110, false, true, &[], &[]);
        assert_eq!(ops, vec![IncidentOp::Open { kind: KIND_SERVE_DOWN, start: 105, detail: "/v1/models: timeout".into() }]);
        assert_eq!(t.serve_down_since(), Some(105));
        assert!(step(&mut t, 115, false, true, &[], &[]).is_empty(), "no duplicate open");
        let ops = step(&mut t, 405, true, true, &[], &[]);
        assert_eq!(ops, vec![IncidentOp::Close { kind: KIND_SERVE_DOWN, end: 405, duration_secs: 300 }]);
        assert_eq!(t.serve_down_since(), None);
    }

    #[test]
    fn single_blip_never_opens() {
        let mut t = IncidentTracker::default();
        for (now, up) in [(0, true), (5, false), (10, true), (15, false), (20, true)] {
            assert!(step(&mut t, now, up, true, &[], &[]).is_empty());
        }
    }

    #[test]
    fn gate_down_is_independent_of_serve_down() {
        let mut t = IncidentTracker::default();
        step(&mut t, 0, true, false, &[], &[]);
        let ops = step(&mut t, 5, true, false, &[], &[]);
        assert!(matches!(&ops[..], [IncidentOp::Open { kind: KIND_GATE_DOWN, start: 0, .. }]));
        let ops = step(&mut t, 50, true, true, &[], &[]);
        assert!(matches!(&ops[..], [IncidentOp::Close { kind: KIND_GATE_DOWN, duration_secs: 50, .. }]));
    }

    #[test]
    fn open_incident_survives_a_collector_restart() {
        let mut t = IncidentTracker::default();
        step(&mut t, 0, false, true, &[], &[]);
        step(&mut t, 5, false, true, &[], &[]);
        let saved = serde_json::to_string(&t).unwrap();
        let mut t2: IncidentTracker = serde_json::from_str(&saved).unwrap();
        let ops = step(&mut t2, 65, true, true, &[], &[]);
        assert_eq!(ops, vec![IncidentOp::Close { kind: KIND_SERVE_DOWN, end: 65, duration_secs: 65 }]);
    }

    #[test]
    fn container_restart_by_count_by_started_at_and_by_replacement() {
        let mut t = IncidentTracker::default();
        assert!(step(&mut t, 0, true, true, &[("serve", ct("glm", 0, 1000))], &[]).is_empty(), "first sight is a baseline");
        assert!(step(&mut t, 5, true, true, &[("serve", ct("glm", 0, 1000))], &[]).is_empty());

        let ops = step(&mut t, 10, true, true, &[("serve", ct("glm", 1, 1000))], &[]);
        assert_eq!(ops, vec![IncidentOp::Event { kind: KIND_CONTAINER_RESTART, ts: 10, subject: "serve".into(), detail: "glm RestartCount 0 -> 1".into(), key: None }]);

        let ops = step(&mut t, 15, true, true, &[("serve", ct("glm", 1, 4600))], &[]);
        assert_eq!(ops, vec![IncidentOp::Event { kind: KIND_CONTAINER_RESTART, ts: 4600, subject: "serve".into(), detail: "glm new StartedAt (previous run lasted 1h00m)".into(), key: None }]);

        let ops = step(&mut t, 20, true, true, &[("serve", ct("model-b", 0, 5000))], &[]);
        assert_eq!(ops, vec![IncidentOp::Event { kind: KIND_CONTAINER_RESTART, ts: 5000, subject: "serve".into(), detail: "glm replaced by model-b".into(), key: None }]);

        assert!(step(&mut t, 25, true, true, &[("serve", ct("model-b", 0, 5000))], &[]).is_empty(), "one event per restart");
    }

    #[test]
    fn a_container_that_vanishes_from_inspect_is_not_a_restart() {
        let mut t = IncidentTracker::default();
        step(&mut t, 0, true, true, &[("serve", ct("glm", 0, 1000))], &[]);
        assert!(step(&mut t, 5, true, true, &[], &[]).is_empty());
        assert!(step(&mut t, 10, true, true, &[("serve", ct("glm", 0, 1000))], &[]).is_empty());
    }

    #[test]
    fn xid_becomes_a_point_incident_with_gpu_and_number() {
        let mut t = IncidentTracker::default();
        let x = XidEvent { ts: 77, pci: "0000:f1:00".into(), xid: 8, gpu: Some(3), detail: "pid=1, name=python3".into() };
        let ops = step(&mut t, 80, true, true, &[], &[x]);
        assert_eq!(ops, vec![IncidentOp::Event { kind: KIND_XID, ts: 77, subject: "GPU3".into(), detail: "GPU3 Xid 8 (GPU stopped processing (hang / watchdog)) pid=1, name=python3".into(), key: Some("xid|GPU3|8|77".into()) }]);
        // the key does not depend on how the description is worded, and a stored row gives the same one back
        assert_eq!(xid_key_from_detail("GPU3 Xid 8 (GPU stopped processing (hang / watchdog)) pid=1, name=python3", 77).as_deref(), Some("xid|GPU3|8|77"));
        assert_eq!(xid_key_from_detail("GPU1 Xid 154 (see NVIDIA Xid catalogue) GPU recovery action changed", 5), xid_key_from_detail("GPU1 Xid 154 (GPU recovery action changed (reset / reboot required)) GPU recovery action changed", 5));
        assert_eq!(xid_key_from_detail("PCI 0000:99:00 Xid 31 page fault", 9).as_deref(), Some("xid|PCI 0000:99:00|31|9"));
        assert_eq!(xid_key_from_detail("the gateway new StartedAt", 9), None);
    }
}
