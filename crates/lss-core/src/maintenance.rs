//! Planned-maintenance mode (card #22): `lss maintenance start|stop "reason"` marks a
//! deliberate window so a restart inside it does not read as an incident. The window itself
//! is still recorded (as `incidents::KIND_MAINTENANCE`, a WINDOW not a fault, exactly like
//! `KIND_BENCH`) - it is the individual restart WARN that is relabelled `info`/"planned"
//! (`rules::eval_restarts`), never suppressed outright: a genuine, unrelated problem during a
//! maintenance window must still be able to alert.

use serde::{Deserialize, Serialize};

/// A forgotten `stop` must not mute restart alerts forever: this is the default SAFETY
/// ceiling, not the expected duration - `lss maintenance stop` is still the normal way to end
/// a window early, the moment the planned work is actually done.
pub const DEFAULT_MINUTES: u32 = 60;
/// Nobody plans a restart window this long; a request past this is almost certainly a
/// minutes-vs-hours typo, and would otherwise leave real restart alerts muted for most of a
/// day. `start` clamps into it rather than refusing outright - the caller's own `expires_at`
/// in the reply says exactly what was granted.
pub const MAX_MINUTES: u32 = 240;

/// `POST /maintenance/start` body.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MaintenanceRequest {
    pub reason: String,
    /// 0 (or absent) = `DEFAULT_MINUTES`; clamped to `MAX_MINUTES`.
    pub minutes: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceReply {
    pub ok: bool,
    pub message: String,
    pub expires_at: i64,
}

/// Live window state: shared between the HTTP thread (start/stop write it) and the poll loop
/// (reads it every tick to open/close the incident, feed `rules::Observation::maintenance_active`,
/// and auto-expire it). Also carried on `Status.maintenance` so the CLI and UI can show it
/// without a dedicated endpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceState {
    pub active: bool,
    pub reason: String,
    pub started_at: i64,
    pub expires_at: i64,
}

/// `minutes` clamped into `[1, MAX_MINUTES]`; 0 means "use the default".
pub fn clamp_minutes(minutes: u32) -> u32 {
    if minutes == 0 {
        DEFAULT_MINUTES
    } else {
        minutes.clamp(1, MAX_MINUTES)
    }
}

/// Pure state transition, unit-tested without any HTTP: a blank reason is refused (a window
/// with no reason on the timeline is not an annotation, it is a blank spot), starting again
/// while already active simply extends/relabels the SAME window rather than opening a second
/// one (there is only ever one `KIND_MAINTENANCE` incident open at a time, like `KIND_BENCH`).
pub fn start(state: &mut MaintenanceState, now: i64, reason: &str, minutes: u32) -> MaintenanceReply {
    let reason = reason.trim();
    if reason.is_empty() {
        return MaintenanceReply { ok: false, message: "a reason is required: lss maintenance start \"reason\"".into(), expires_at: 0 };
    }
    let mins = clamp_minutes(minutes);
    let expires_at = now + i64::from(mins) * 60;
    *state = MaintenanceState { active: true, reason: reason.to_string(), started_at: now, expires_at };
    MaintenanceReply { ok: true, message: format!("maintenance window open: {reason} (auto-expires in {mins}m unless stopped first)"), expires_at }
}

/// `None` reply when nothing was open (nothing to stop, nothing to announce).
pub fn stop(state: &mut MaintenanceState, now: i64) -> Option<MaintenanceReply> {
    if !state.active {
        return None;
    }
    let (reason, lasted) = (state.reason.clone(), now - state.started_at);
    *state = MaintenanceState::default();
    Some(MaintenanceReply { ok: true, message: format!("maintenance window closed after {}: {reason}", crate::timeutil::fmt_duration(lasted)), expires_at: 0 })
}

/// True once an active window's `expires_at` has passed: the poll loop then closes it exactly
/// as `stop` would, so a forgotten window never mutes real restart alerts indefinitely.
pub fn expired(state: &MaintenanceState, now: i64) -> bool {
    state.active && now >= state.expires_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_default_clamp_and_ceiling() {
        assert_eq!(clamp_minutes(0), DEFAULT_MINUTES);
        assert_eq!(clamp_minutes(30), 30);
        assert_eq!(clamp_minutes(9_999), MAX_MINUTES);
    }

    #[test]
    fn a_blank_reason_is_refused_and_nothing_opens() {
        let mut s = MaintenanceState::default();
        let r = start(&mut s, 1000, "   ", 30);
        assert!(!r.ok);
        assert!(!s.active, "a refused start must not leave a half-open window");
    }

    #[test]
    fn start_then_stop_round_trips_and_auto_expires_when_forgotten() {
        let mut s = MaintenanceState::default();
        let r = start(&mut s, 1000, "gate v5.2 swap", 30);
        assert!(r.ok && s.active);
        assert_eq!((s.reason.as_str(), s.started_at, s.expires_at), ("gate v5.2 swap", 1000, 1000 + 30 * 60));
        assert!(!expired(&s, 1000 + 29 * 60), "not due yet");
        assert!(!expired(&s, 1000 + 30 * 60 - 1));
        assert!(expired(&s, 1000 + 30 * 60), "the boundary itself counts as due");

        let stopped = stop(&mut s, 1000 + 600).expect("a window was open");
        assert!(stopped.ok && stopped.message.contains("10m00s") && stopped.message.contains("gate v5.2 swap"), "{}", stopped.message);
        assert!(!s.active, "stop clears the window back to default");
        assert!(stop(&mut s, 2000).is_none(), "stopping an already-closed window is a no-op, not an error reply");
    }

    #[test]
    fn a_missing_minutes_value_uses_the_default_and_an_absurd_one_is_capped() {
        let mut s = MaintenanceState::default();
        let r = start(&mut s, 0, "quick restart", 0);
        assert_eq!(r.expires_at, i64::from(DEFAULT_MINUTES) * 60);
        let mut s2 = MaintenanceState::default();
        let r2 = start(&mut s2, 0, "quick restart", 100_000);
        assert_eq!(r2.expires_at, i64::from(MAX_MINUTES) * 60, "a minutes-vs-hours typo does not mute alerts for a whole day");
    }

    #[test]
    fn starting_again_while_active_replaces_the_window_rather_than_stacking_a_second_one() {
        let mut s = MaintenanceState::default();
        start(&mut s, 1000, "first reason", 10);
        let r = start(&mut s, 1200, "actually, a different reason", 20);
        assert!(r.ok);
        assert_eq!((s.reason.as_str(), s.started_at, s.expires_at), ("actually, a different reason", 1200, 1200 + 20 * 60));
    }
}
