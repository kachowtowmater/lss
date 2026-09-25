//! Alert rule engine. Pure: `Engine::evaluate(&Observation)` with the clock inside the
//! observation. The engine is serialisable so cooldowns, the C1 baseline and the thermal
//! digest survive a collector restart.
//!
//! Semantics shared by every sustained rule:
//!   * the condition must hold continuously for the rule's hold time before it FIRES;
//!   * a rule that fired stays quiet until it clears, then emits one RECOVERED message;
//!   * after an alert the rule is in cooldown: if it clears and trips again inside the
//!     cooldown the new alert is held back, and is sent when the cooldown ends if the
//!     condition is still true. A recovered message is only sent for an alert that was sent.

use crate::config::RulesConfig;
use crate::gpu::{throttle_flags, THROTTLE_THERMAL_MASK};
use crate::timeutil::fmt_duration;
use crate::xid::{xid_hint, XidEvent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Page,
    Hardware,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Page => "page",
            Severity::Hardware => "hardware",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertEvent {
    pub ts: i64,
    /// Stable rule key, e.g. `serve_down`, `thermal_temp:gpu2`, `xid:gpu3`.
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    /// true for the "recovered" message of a rule.
    pub recovered: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuObs {
    pub index: u32,
    pub temp_c: Option<f64>,
    pub throttle_mask: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestartObs {
    /// "serve" or "gate"
    pub role: String,
    pub detail: String,
}

/// card #264: one statvfs reading of the filesystem the collector watches (`disk_path`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskObs {
    pub path: String,
    pub total_bytes: u64,
    /// available to an unprivileged user (statvfs `f_bavail`) - what a build can still write
    pub avail_bytes: u64,
}

impl DiskObs {
    /// % used = everything a normal user can NOT write: `1 - avail/total`. Unlike `df`'s Use%
    /// (used / (used + avail)), the root-reserved blocks count as used here, so on ext4 (5%
    /// reserved) this reads a little HIGHER than `df` (about 0.4 points on the build host's / when
    /// verified - verifier F4). Conservative on purpose: what a build can still write is what fills up.
    pub fn used_pct(&self) -> f64 {
        if self.total_bytes == 0 { 0.0 } else { 100.0 * (1.0 - self.avail_bytes as f64 / self.total_bytes as f64) }
    }
    /// `92.0% used (80 GB free of 1000 GB)`
    pub fn describe(&self) -> String {
        format!("{:.1}% used ({} free of {})", self.used_pct(), gb(self.avail_bytes), gb(self.total_bytes))
    }
}

fn gb(bytes: u64) -> String {
    // the unit is chosen from the ROUNDED value, in integers (no float edge cases): 999,999,999
    // bytes rounds to 1000 MB, so it is '1.0 GB', never '1000 MB'; 9.95 GB is '10 GB', never
    // '10.0 GB'. Below 1 GB in MB (a tmpfs read '0.0 GB free of 0.0 GB' before).
    let mb = (bytes + 500_000) / 1_000_000;
    let tenths = (bytes + 50_000_000) / 100_000_000;
    if mb < 1000 {
        format!("{mb} MB")
    } else if tenths < 100 {
        format!("{}.{} GB", tenths / 10, tenths % 10)
    } else {
        format!("{} GB", (bytes + 500_000_000) / 1_000_000_000)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub now: i64,
    pub serve_up: bool,
    pub gate_up: bool,
    /// None when /metrics was unreadable.
    pub queue: Option<f64>,
    /// None when /gate/health was unreadable.
    pub trusted_waiters: Option<u64>,
    /// card #78: the trusted lane's in-flight prompt tokens (the budget's `used`), None when
    /// /gate/health was unreadable. The composite no-cache-heavy-budget rule reads this.
    pub trusted_inflight_tokens: Option<u64>,
    /// card #135: the trusted lane's budget CAPACITY - the threshold is expressed against
    /// the LIVE capacity (a constant equal to it can never fire; accumulation tops out AT
    /// capacity). None when /gate/health was unreadable or the gate is too old.
    pub trusted_budget_tokens: Option<u64>,
    /// card #78: the engine's prefix-cache hit rate, None when /metrics was unreadable.
    pub cache_hit_rate: Option<f64>,
    pub rejected_413: Option<u64>,
    pub rejected_429: Option<u64>,
    /// None when nvidia-smi itself failed (that is the gpu_missing rule's business).
    pub gpus: Option<Vec<GpuObs>>,
    pub xids: Vec<XidEvent>,
    pub restarts: Vec<RestartObs>,
    /// roles whose container is currently `running`
    pub running_roles: Vec<String>,
    /// decode tok/s of a VALID idle C1 probe that completed since the last evaluation
    /// (`probe::c1_reading`); a probe that collided with traffic is simply not reported
    pub probe_tok_s: Option<f64>,
    /// card #261: the ts of the probe `probe_tok_s` came from - its key in the probes table,
    /// quoted in the alert so anyone can look the probes up. `None` = use `now`.
    pub probe_ts: Option<i64>,
    /// operator-injected test messages (`lss-collector --test-alert`): each becomes one `info`
    /// alert that takes the same road as a real one - DB row, then the alert sink
    pub test_alerts: Vec<String>,
    /// an `lss bench` run is in progress: the load is ours and deliberate. The rules a
    /// benchmark would trip (queue pressure, gate waiters, the C1 decode rule) stand down until
    /// it ends; outages, restarts, Xids and heat still alert. A latency rule, if one is ever
    /// added, must honour this flag too.
    pub bench_active: bool,
    /// #47, 2026-09-21: `cfg.probe.interval_secs`, so `eval_c1` can judge staleness in probe
    /// intervals without the rule engine needing a whole `Config` (it only ever sees `RulesConfig`)
    pub probe_interval_secs: i64,
    /// #22, 2026-09-21: an `lss maintenance start|stop "reason"` window is open. Unlike
    /// `bench_active`, this never stands a rule down - a genuine problem during planned work
    /// must still be able to alert. It only changes how `eval_restarts` LABELS the restart it
    /// was opened for: `info`/"planned" instead of `warn`, so a deliberate restart does not
    /// read as an incident, while still being on the record.
    pub maintenance_active: bool,
    /// #108, 2026-09-22: the gate's own charging health, straight from its /gate/health
    /// `shadow` block. `None` when /gate/health was unreadable or the gate is older than the
    /// block. Two facts, two rules: the charge path RAISING (`charge_errors`), and the
    /// discount being inert with nothing raising at all (`discount_inert()`).
    pub gate_shadow: Option<crate::gate::GateShadow>,
    /// #36, 2026-09-21: omp's shared config (`~/.omp/agent/config.yml`) on THIS box names a
    /// default model this provider is not currently serving - `Some((configured, served))`.
    /// `None` = in sync, or nothing to compare (no omp config on this box, or the served model
    /// is not known yet). See `lss_core::omp`.
    pub omp_mismatch: Option<(String, String)>,
    /// card #264: the collector's own disk (`disk_path`), None when unread or not watched - the
    /// disk rules then hold their state (no reading is not a recovery).
    pub disk: Option<DiskObs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    None,
    Fire,
    Recover,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Sustained {
    since: Option<i64>,
    announced: bool,
    last_sent: Option<i64>,
}

impl Sustained {
    fn step(&mut self, now: i64, cond: bool, hold: i64, cooldown: i64) -> Edge {
        if !cond {
            let was = std::mem::take(&mut self.announced);
            self.since = None;
            return if was { Edge::Recover } else { Edge::None };
        }
        let since = *self.since.get_or_insert(now);
        if now - since >= hold && !self.announced && self.cooled(now, cooldown) {
            self.announced = true;
            self.last_sent = Some(now);
            return Edge::Fire;
        }
        Edge::None
    }

    fn cooled(&self, now: i64, cooldown: i64) -> bool {
        self.last_sent.is_none_or(|t| now - t >= cooldown)
    }

    fn held_for(&self, now: i64) -> i64 {
        self.since.map_or(0, |s| now - s)
    }
}

/// A counter watched for growth inside a sliding window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct GrowthWindow {
    points: VecDeque<(i64, u64)>,
    rule: Sustained,
}

impl GrowthWindow {
    /// Growth of the counter across the window. A counter that went backwards (the gate
    /// restarted) resets the window instead of reporting nonsense.
    fn push(&mut self, now: i64, value: u64, window: i64) -> u64 {
        if self.points.back().is_some_and(|(_, last)| value < *last) {
            self.points.clear();
        }
        self.points.push_back((now, value));
        while self.points.len() > 1 && self.points[1].0 <= now - window {
            self.points.pop_front();
        }
        value - self.points.front().map_or(value, |(_, v)| *v)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct C1State {
    baseline_samples: Vec<f64>,
    learned_baseline: Option<f64>,
    /// card #261: the low VALID probes the current streak is made of, as (probe ts, tok/s) - the
    /// probe's ts is its key in the probes table. A bare counter used to live here (`low_streak`)
    /// and it could not be checked against anything: 2026-09-23 a startup repair re-judged two
    /// probes it had counted as `contended`, the counter kept them, and ONE genuinely low probe
    /// later fired "3 consecutive". State saved with the old counter deserialises to an EMPTY
    /// streak (the counter is ignored), because a count nobody can name the probes of is not
    /// evidence.
    low_probes: Vec<(i64, f64)>,
    announced: bool,
    last_sent: Option<i64>,
    /// the baseline has been (re)built from VALID probes only; false in state saved by a
    /// collector from before probe validation existed
    validated: bool,
    /// #47, 2026-09-21: when C1 last had a VALID reading. Seeded to "now" on the very first
    /// evaluation ever (a grace period - the collector just started, not a real blind spot) and
    /// bumped every time a valid probe arrives; the staleness alert below is judged against it,
    /// entirely apart from the low-decode alert above, so one can never gate or delay the other.
    last_valid_ts: Option<i64>,
    stale: Sustained,
    /// #52, 2026-09-21: on a box with only brief scattered idle windows (card #50's measurement:
    /// the longest unbroken quiet run all week was 5m00s), C1 genuinely alternates between
    /// "stale 30+ min" and "one probe just snuck through" - each cycle is individually true, but
    /// five of them in an evening tell the reader nothing they did not already know after the
    /// first. Collapsed by identity, not by transition (see `FlapTracker`).
    stale_flaps: FlapTracker,
}

/// #52, 2026-09-21 (panel: hamel-husain): "collapse by alert IDENTITY, not by transition." A
/// rule that fires and recovers repeatedly is not N findings - it is ONE finding, "this
/// measurement is unreliable", and five fire/recover pairs in an evening is worse than saying
/// that once and then going quiet about the individual flaps. NEVER quiet about the ROW: it
/// stays visible, re-labelled, and the count keeps counting (`rule_states` reads `muted`/
/// `label` directly; nothing here deletes or hides history).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct FlapTracker {
    /// one timestamp per completed flap (a fire followed by its own recovery), oldest first,
    /// pruned to `FLAP_LABEL_WINDOW_SECS` (the longer of the two windows this tracks)
    flaps: VecDeque<i64>,
    /// an "now unreliable" message was sent for the CURRENT muted episode - once per episode,
    /// not once per flap, and reset the moment the flap rate calms back down
    muted_announced: bool,
}

/// The panel asked for "3 flaps within an hour". Deliberately widened to 2 hours here: a fire
/// needs `cfg.cooldown_secs` (1800s by default) to elapse since the LAST fire before it can fire
/// again, so 3 fires are at minimum 2 x 1800s = 3600s apart at the very fastest - exactly the
/// boundary of "within an hour", and this tracker's own `>` comparison would exclude a flap
/// sitting exactly on that boundary. 2 hours is the smallest window that is safely, always
/// reachable under the existing cooldown rather than a target that is mathematically on a knife
/// edge (sometimes literally unreachable) under today's config. Flagged on the card, not a
/// silent substitution: if the cooldown for this rule specifically is ever shortened, this
/// number should come down with it.
const FLAP_MUTE_WINDOW_SECS: i64 = 7_200;
const FLAP_MUTE_THRESHOLD: usize = 3;
/// the count shown in the muted label ("N flaps/24h") is the longer, more informative window -
/// distinct from the shorter window that decides whether to mute
const FLAP_LABEL_WINDOW_SECS: i64 = 86_400;

impl FlapTracker {
    fn note_flap(&mut self, now: i64) {
        self.flaps.push_back(now);
        self.prune(now);
    }

    fn prune(&mut self, now: i64) {
        while self.flaps.front().is_some_and(|t| *t <= now - FLAP_LABEL_WINDOW_SECS) {
            self.flaps.pop_front();
        }
    }

    fn count_since(&self, now: i64, window: i64) -> usize {
        self.flaps.iter().filter(|t| **t > now - window).count()
    }

    fn muted(&self, now: i64) -> bool {
        self.count_since(now, FLAP_MUTE_WINDOW_SECS) >= FLAP_MUTE_THRESHOLD
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct DigestGpu {
    max_temp_c: f64,
    hot_secs: i64,
    throttled_secs: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Digest {
    window_start: Option<i64>,
    last_eval: Option<i64>,
    gpus: BTreeMap<u32, DigestGpu>,
}

const DIGEST_PERIOD_SECS: i64 = 86_400;
pub const RULE_TEST_ALERT: &str = "test_alert";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Engine {
    serve_down: Sustained,
    serve_down_page: Sustained,
    gate_down: Sustained,
    queue: Sustained,
    waiters: Sustained,
    thermal_temp: BTreeMap<u32, Sustained>,
    thermal_throttle: BTreeMap<u32, Sustained>,
    gpu_missing: Sustained,
    gpu_count_seen: usize,
    rejects_413: GrowthWindow,
    rejects_429: GrowthWindow,
    /// rule key → last time an event alert was sent (xid:gpuN, container_restart:role)
    event_last_sent: BTreeMap<String, i64>,
    /// role → time of the restart we still owe a "stable again" message for
    restart_pending: BTreeMap<String, i64>,
    /// role → was the restart we are waiting on a PLANNED one (#22, 2026-09-21): the eventual
    /// "stable again" message must match the severity/label of its own fire, not whatever
    /// `maintenance_active` happens to be by the time the container stabilises - a window can
    /// close mid-restart, well before `restart_stable_secs` elapses.
    restart_planned: BTreeMap<String, bool>,
    c1: C1State,
    /// #36, 2026-09-21: omp's shared config naming a model this provider does not serve
    omp_default: Sustained,
    /// #108, 2026-09-22: the gate's effective-token charging, watched two ways
    charge_errors: Sustained,
    discount_inert: Sustained,
    /// card #78: the composite "big budget, no cache" rule
    cold_budget: Sustained,
    /// card #264: the collector's own disk filling up (warn / page)
    disk_full: Sustained,
    disk_full_page: Sustained,
    digest: Digest,
    /// what the last evaluation saw: only for `rule_states`, never saved
    #[serde(skip)]
    last: LastSeen,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct LastSeen {
    serve_up: bool,
    gate_up: bool,
    queue: Option<f64>,
    waiters: Option<u64>,
    gpus: Option<Vec<GpuObs>>,
    growth_413: Option<u64>,
    growth_429: Option<u64>,
    c1_tok_s: Option<f64>,
    omp_mismatch: Option<(String, String)>,
    gate_shadow: Option<crate::gate::GateShadow>,
    disk: Option<DiskObs>,
}

/// One row of `GET /rules`: what Alertmanager would call the state of an alerting rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleState {
    pub rule: String,
    pub severity: String,
    /// `ok` | `pending` (condition true, hold time or cooldown not over) | `firing`
    pub state: String,
    /// the condition, in words, with the numbers in force
    pub threshold: String,
    /// what the rule is looking at right now
    pub value: String,
    /// since when the condition has been true
    pub pending_since: Option<i64>,
    pub last_fired: Option<i64>,
    pub cooldown_remaining_s: i64,
}

impl Engine {
    /// The C1 baseline in force: the configured value wins, else the learned median.
    pub fn c1_baseline(&self, cfg: &RulesConfig) -> Option<f64> {
        cfg.c1_baseline_tok_s.or(self.c1.learned_baseline)
    }

    pub fn c1_baseline_progress(&self) -> usize {
        self.c1.baseline_samples.len()
    }

    /// True for engine state that predates probe validation: its learned baseline and its
    /// low-streak may have been fed by probes that collided with real traffic.
    /// card #261: the probes (ts) the current low-decode streak is counting.
    pub fn c1_streak_probes(&self) -> Vec<i64> {
        self.c1.low_probes.iter().map(|(ts, _)| *ts).collect()
    }

    /// card #261: drop from the low-decode streak every probe that is no longer a valid reading
    /// (a startup repair re-judged it). Returns the dropped probe ts.
    pub fn retain_c1_streak(&mut self, still_valid: impl Fn(i64) -> bool) -> Vec<i64> {
        let mut dropped = Vec::new();
        self.c1.low_probes.retain(|(ts, _)| {
            let keep = still_valid(*ts);
            if !keep {
                dropped.push(*ts);
            }
            keep
        });
        dropped
    }

    pub fn c1_needs_revalidation(&self) -> bool {
        !self.c1.validated
    }

    /// Rebuilds the learned baseline from `valid_values` - the decode rates of the most recent
    /// VALID probes, oldest first - and forgets any streak or alert built on unvalidated ones.
    /// Fewer than `c1_baseline_probes` values = back to learning, starting from these.
    pub fn relearn_c1_baseline(&mut self, cfg: &RulesConfig, valid_values: &[f64]) {
        let want = cfg.c1_baseline_probes.max(1);
        let recent = &valid_values[valid_values.len().saturating_sub(want)..];
        self.c1 = C1State {
            baseline_samples: recent.to_vec(),
            learned_baseline: (recent.len() >= want).then(|| median(recent)),
            last_sent: self.c1.last_sent,
            validated: true,
            ..Default::default()
        };
    }

    /// Rule keys that are currently firing (announced and not yet recovered). `now` decides
    /// whether a flap-muted identity still counts as firing: it does not (#52) - a MUTED row
    /// reads as "measurement unreliable", not "problem happening now", so it is deliberately
    /// left out of what turns the header red.
    pub fn firing(&self, now: i64) -> Vec<String> {
        let mut out = Vec::new();
        let mut add = |name: &str, s: &Sustained| {
            if s.announced {
                out.push(name.to_string());
            }
        };
        add("serve_down", &self.serve_down);
        add("serve_down_page", &self.serve_down_page);
        add("gate_down", &self.gate_down);
        add("gate_charge_errors", &self.charge_errors);
        add("gate_discount_inert", &self.discount_inert);
        add("gate_cold_budget", &self.cold_budget);
        add("queue_pressure", &self.queue);
        add("gate_waiters", &self.waiters);
        add("gpu_missing", &self.gpu_missing);
        add("disk_full", &self.disk_full);
        add("disk_full_page", &self.disk_full_page);
        add("rejects_413", &self.rejects_413.rule);
        add("rejects_429", &self.rejects_429.rule);
        for (i, s) in &self.thermal_temp {
            add(&format!("thermal_temp:gpu{i}"), s);
        }
        for (i, s) in &self.thermal_throttle {
            add(&format!("thermal_throttle:gpu{i}"), s);
        }
        if self.c1.announced {
            out.push("c1_decode".into());
        }
        if self.c1.stale.announced && !self.c1.stale_flaps.muted(now) {
            out.push("c1_stale".into());
        }
        out
    }

    /// Every rule with its state, for `GET /rules` and the ALERTS page. Reads the engine as the
    /// last `evaluate` left it; `now` only ages the cooldowns.
    pub fn rule_states(&self, cfg: &RulesConfig, now: i64) -> Vec<RuleState> {
        let l = &self.last;
        let cd = |last: Option<i64>| last.map_or(0, |t| (t + cfg.cooldown_secs - now).max(0));
        let sustained = |rule: &str, sev: Severity, s: &Sustained, threshold: String, value: String| RuleState {
            rule: rule.to_string(),
            severity: sev.as_str().to_string(),
            state: if s.announced { "firing" } else if s.since.is_some() { "pending" } else { "ok" }.to_string(),
            threshold,
            value,
            pending_since: s.since,
            last_fired: s.last_sent,
            cooldown_remaining_s: cd(s.last_sent),
        };
        let updown = |up: bool, s: &Sustained| if up { "up".to_string() } else { format!("down {}", fmt_duration(s.held_for(now))) };
        let opt = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{x:.0}"));
        let mut out = vec![
            sustained("serve_down", Severity::Warn, &self.serve_down, format!("serve down >= {}", fmt_duration(cfg.serve_down_secs)), updown(l.serve_up, &self.serve_down)),
            sustained("serve_down_page", Severity::Page, &self.serve_down_page, format!("serve down >= {}", fmt_duration(cfg.serve_down_page_secs)), updown(l.serve_up, &self.serve_down_page)),
            sustained("gate_down", Severity::Warn, &self.gate_down, format!("gate down >= {}", fmt_duration(cfg.gate_down_secs)), updown(l.gate_up, &self.gate_down)),
            // #108: both rows read from the same /gate/health shadow block
            sustained("gate_charge_errors", Severity::Warn, &self.charge_errors,
                format!("gate charge_errors > 0 for >= {}", fmt_duration(cfg.charge_errors_secs)),
                l.gate_shadow.as_ref().map_or("no reading".to_string(), |sh| format!("{} errors", sh.charge_errors))),
            sustained("gate_discount_inert", Severity::Warn, &self.discount_inert,
                format!("warm cache + >= {} admissions + 0 discounted for >= {}", cfg.discount_min_admissions, fmt_duration(cfg.discount_inert_secs)),
                l.gate_shadow.as_ref().map_or("no reading".to_string(), |sh| format!("{}/{} discounted, warm {}", sh.recent_discounted, sh.recent_admissions,
                    sh.cache_warm.map_or("?".to_string(), |w| w.to_string())))),
            sustained("queue_pressure", Severity::Warn, &self.queue, format!("queue >= {:.0} for {}", cfg.queue_reqs, fmt_duration(cfg.queue_secs)), format!("queue {}", opt(l.queue))),
            sustained("gate_waiters", Severity::Warn, &self.waiters, format!("trusted waiters > 0 for {}", fmt_duration(cfg.waiters_secs)), format!("waiters {}", l.waiters.map_or_else(|| "-".to_string(), |w| w.to_string()))),
            sustained("gpu_missing", Severity::Hardware, &self.gpu_missing, format!("GPUs < {} for {}", self.gpu_count_seen, fmt_duration(cfg.gpu_missing_secs)), format!("{} of {}", l.gpus.as_ref().map_or(0, Vec::len), self.gpu_count_seen)),
            sustained("omp_default_mismatch", Severity::Warn, &self.omp_default, format!("omp default != served for {}", fmt_duration(cfg.omp_mismatch_hold_secs)),
                l.omp_mismatch.as_ref().map_or_else(|| "in sync".to_string(), |(c, s)| format!("{c} != {s}"))),
        ];
        // card #264: the collector's own disk
        let disk_value = l.disk.as_ref().map_or_else(|| "no reading".to_string(), |d| format!("{} {}", d.path, d.describe()));
        for (rule, sev, s, pct) in [("disk_full", Severity::Warn, &self.disk_full, cfg.disk_warn_pct), ("disk_full_page", Severity::Page, &self.disk_full_page, cfg.disk_page_pct)] {
            out.push(sustained(rule, sev, s, format!("{} >= {pct:.0}% used for {} (recovers below {:.0}%)", cfg.disk_path, fmt_duration(cfg.disk_secs), pct - cfg.disk_recover_margin_pct), disk_value.clone()));
        }
        let mut indices: Vec<u32> = self.thermal_temp.keys().chain(self.thermal_throttle.keys()).copied().collect();
        indices.extend(l.gpus.iter().flatten().map(|g| g.index));
        indices.sort_unstable();
        indices.dedup();
        let none = Sustained::default();
        for i in indices {
            let g = l.gpus.iter().flatten().find(|g| g.index == i);
            let excluded = cfg.thermal_exclude.contains(&i);
            let note = if excluded { " (excluded: daily digest only)" } else { "" };
            out.push(sustained(&format!("thermal_temp:gpu{i}"), Severity::Warn, self.thermal_temp.get(&i).unwrap_or(&none),
                format!(">= {} for {}{note}", crate::units::temp_compact(cfg.thermal_temp_c), fmt_duration(cfg.thermal_secs)), crate::units::temp_opt(g.and_then(|g| g.temp_c), crate::units::TempStyle::Compact)));
            let flags = g.map(|g| throttle_flags(g.throttle_mask & THROTTLE_THERMAL_MASK)).unwrap_or_default();
            out.push(sustained(&format!("thermal_throttle:gpu{i}"), Severity::Warn, self.thermal_throttle.get(&i).unwrap_or(&none),
                format!("thermal slowdown for {}{note}", fmt_duration(cfg.thermal_secs)), if flags.is_empty() { "none".to_string() } else { flags.join("+") }));
        }
        let floor = self.c1_baseline(cfg).map(|b| b * cfg.c1_ratio);
        out.push(RuleState {
            rule: "c1_decode".into(),
            severity: Severity::Warn.as_str().into(),
            state: if self.c1.announced { "firing" } else if !self.c1.low_probes.is_empty() { "pending" } else { "ok" }.into(),
            threshold: match floor {
                Some(f) => format!("valid idle probe < {f:.1} tok/s, {} in a row", cfg.c1_consecutive),
                None => format!("learning the baseline ({}/{})", self.c1.baseline_samples.len(), cfg.c1_baseline_probes),
            },
            value: format!("{} tok/s, {} low in a row", l.c1_tok_s.map_or_else(|| "-".to_string(), |v| format!("{v:.1}")), self.c1.low_probes.len()),
            pending_since: None,
            last_fired: self.c1.last_sent,
            cooldown_remaining_s: cd(self.c1.last_sent),
        });
        // #52, 2026-09-21: a flap-muted identity rewrites its own row rather than keep repeating
        // fire/recover - "collapse by alert IDENTITY, not by transition." Still fully a row in
        // this list (never suppressed), just a different state word and a different sentence.
        if self.c1.stale_flaps.muted(now) {
            let flaps_24h = self.c1.stale_flaps.count_since(now, FLAP_LABEL_WINDOW_SECS);
            out.push(RuleState {
                rule: "c1_stale".into(),
                severity: Severity::Info.as_str().into(),
                state: "muted".into(),
                threshold: format!(">= {FLAP_MUTE_THRESHOLD} flaps within {} mutes the row", fmt_duration(FLAP_MUTE_WINDOW_SECS)),
                value: format!("c1_stale unreliable - {flaps_24h} flaps/24h, muted"),
                pending_since: self.c1.stale.since,
                last_fired: self.c1.stale.last_sent,
                cooldown_remaining_s: cd(self.c1.stale.last_sent),
            });
        } else {
            out.push(sustained("c1_stale", Severity::Info, &self.c1.stale,
                format!("no VALID idle reading for {} probe intervals", cfg.c1_stale_probe_intervals),
                self.c1.last_valid_ts.map_or_else(|| "never measured yet".to_string(), |t| format!("last valid {} ago", fmt_duration(now - t)))));
        }
        for (code, win, growth) in [(413, &self.rejects_413, l.growth_413), (429, &self.rejects_429, l.growth_429)] {
            out.push(sustained(&format!("rejects_{code}"), Severity::Info, &win.rule,
                format!("rejected_{code} grows > {} in {}", cfg.reject_growth, fmt_duration(cfg.reject_window_secs)), format!("+{}", growth.unwrap_or(0))));
        }
        // event rules: they fire once per event and have no "firing" state of their own
        let event = |rule: String, sev: Severity, threshold: &str, pending: Option<i64>, last: Option<i64>| RuleState {
            rule,
            severity: sev.as_str().to_string(),
            state: if pending.is_some() { "pending" } else { "ok" }.to_string(),
            threshold: threshold.to_string(),
            value: if pending.is_some() { "waiting for it to stay up".to_string() } else { "-".to_string() },
            pending_since: pending,
            last_fired: last,
            cooldown_remaining_s: cd(last),
        };
        for role in ["serve", "gate"] {
            let key = format!("container_restart:{role}");
            let last = self.event_last_sent.get(&key).copied();
            out.push(event(key, Severity::Warn, "any container restart", self.restart_pending.get(role).copied(), last));
        }
        let mut xid_keys: Vec<&String> = self.event_last_sent.keys().filter(|k| k.starts_with("xid:")).collect();
        xid_keys.sort();
        if xid_keys.is_empty() {
            out.push(event("xid:*".into(), Severity::Hardware, "any NVRM Xid in the kernel log", None, None));
        }
        for k in xid_keys {
            out.push(event(k.clone(), Severity::Hardware, "any NVRM Xid in the kernel log", None, self.event_last_sent.get(k).copied()));
        }
        out.push(event("thermal_digest".into(), Severity::Info, "daily summary of the excluded GPUs", None, None));
        out
    }

    pub fn evaluate(&mut self, cfg: &RulesConfig, o: &Observation) -> Vec<AlertEvent> {
        self.last = LastSeen {
            serve_up: o.serve_up,
            gate_up: o.gate_up,
            queue: o.queue,
            waiters: o.trusted_waiters,
            gpus: o.gpus.clone().or_else(|| self.last.gpus.take()),
            growth_413: self.last.growth_413,
            growth_429: self.last.growth_429,
            c1_tok_s: o.probe_tok_s.or(self.last.c1_tok_s),
            omp_mismatch: o.omp_mismatch.clone(),
            gate_shadow: o.gate_shadow.clone().or_else(|| self.last.gate_shadow.clone()),
            disk: o.disk.clone().or_else(|| self.last.disk.take()),
        };
        let mut out = Vec::new();
        self.eval_serve_down(cfg, o, &mut out);
        self.eval_gate_down(cfg, o, &mut out);
        self.eval_restarts(cfg, o, &mut out);
        self.eval_xids(cfg, o, &mut out);
        self.eval_queue(cfg, o, &mut out);
        self.eval_gpus(cfg, o, &mut out);
        self.eval_c1(cfg, o, &mut out);
        self.eval_rejects(cfg, o, &mut out);
        self.eval_omp_default(cfg, o, &mut out);
        self.eval_charging(cfg, o, &mut out);
        self.eval_cold_budget(cfg, o, &mut out);
        self.eval_disk(cfg, o, &mut out);
        for msg in &o.test_alerts {
            // no state, no cooldown: a test must fire every time it is asked for
            out.push(alert(o.now, RULE_TEST_ALERT, Severity::Info, false, format!("[TEST] {msg}")));
        }
        out
    }

    fn eval_serve_down(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let down = !o.serve_up;
        let down_for = if down { self.serve_down.held_for(o.now) } else { 0 };
        let was_down_for = self.serve_down.held_for(o.now);
        let warn = self.serve_down.step(o.now, down, cfg.serve_down_secs, cfg.cooldown_secs);
        // The page escalation is its own rule, so a warn that was just sent does not hold it back.
        let page = self.serve_down_page.step(o.now, down, cfg.serve_down_page_secs, cfg.cooldown_secs);
        if warn == Edge::Fire {
            out.push(alert(o.now, "serve_down", Severity::Warn, false,
                format!("serve DOWN for {} (/v1/models not answering)", fmt_duration(down_for))));
        }
        if page == Edge::Fire {
            out.push(alert(o.now, "serve_down_page", Severity::Page, false,
                format!("serve STILL DOWN after {} - needs a human", fmt_duration(down_for))));
        }
        if warn == Edge::Recover || page == Edge::Recover {
            let sev = if page == Edge::Recover { Severity::Page } else { Severity::Warn };
            out.push(alert(o.now, "serve_down", sev, true,
                format!("serve recovered after {} down", fmt_duration(was_down_for))));
        }
    }

    fn eval_gate_down(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let was = self.gate_down.held_for(o.now);
        match self.gate_down.step(o.now, !o.gate_up, cfg.gate_down_secs, cfg.cooldown_secs) {
            Edge::Fire => out.push(alert(o.now, "gate_down", Severity::Warn, false,
                format!("the gateway DOWN for {} (/gate/health not answering) - public endpoint is dark", fmt_duration(was)))),
            Edge::Recover => out.push(alert(o.now, "gate_down", Severity::Warn, true,
                format!("the gateway recovered after {} down", fmt_duration(was)))),
            Edge::None => {}
        }
    }

    /// #108: the gate's effective-token charging, watched TWO ways, because card #68 proved
    /// one way is not enough. `charge_errors` is the exception form - the charge path raised
    /// and every request silently fell back to gross. `discount_inert` is the OUTCOME form -
    /// nothing raised, the cache is warm, the engine is visible, traffic is flowing, and not
    /// one admission in the gate's last window was discounted. #68 was the second shape for
    /// 2,809 requests: the gauge that would have shouted did not exist, and nothing read it.
    fn eval_charging(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let Some(sh) = o.gate_shadow.as_ref() else {
            // no reading: NOT a reason to alert (gate_down owns that), and not a reason to
            // forget either - the clocks hold their state for the next real reading.
            return;
        };
        let errors = sh.charge_errors;
        push_edge(
            out,
            self.charge_errors.step(o.now, errors > 0, cfg.charge_errors_secs, cfg.cooldown_secs),
            o.now,
            "gate_charge_errors",
            Severity::Warn,
            format!(
                "the gateway charge path FAILING ({errors} errors): every trusted request is charging GROSS prompt tokens, so the budget refuses our own agents at ~10x the real cost. /gate/health shadow.charge_errors, and the gate log's charge_failed_gross line names the exception"
            ),
            "the gateway charge path recovered: effective-token charging is working again".to_string(),
        );
        let inert = sh.discount_inert(cfg.discount_min_admissions);
        push_edge(
            out,
            self.discount_inert.step(o.now, inert, cfg.discount_inert_secs, cfg.cooldown_secs),
            o.now,
            "gate_discount_inert",
            Severity::Warn,
            format!(
                "the gateway discount INERT: warm cache, engine visible, {} recent admissions and NOT ONE discounted. Nothing raised - this is the shape card #68 hid in for 2,809 requests. Check /gate/health shadow and the shadow log's charged_tokens vs est_tokens",
                sh.recent_admissions
            ),
            "the gateway discount is engaging again".to_string(),
        );
    }

    /// card #78: the composite "big budget, no cache" rule - trusted in-flight above the
    /// council ceiling AND the engine's cache hit rate below the point where budget tokens
    /// stop being cheap. That state turns a large budget from harmless to lethal (both Xid-8
    /// hangs happened with millions of REAL pending prefill), and it fires BEFORE the engine
    /// is in trouble rather than after. Two facts must hold together; either alone is normal.
    fn eval_cold_budget(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        if o.bench_active {
            return; // the load is ours and deliberate; stand down like the other budget rules
        }
        // card #135: the threshold is the LIVE budget capacity, not a constant - trusted
        // in-flight is bounded by capacity, so "strictly > a constant equal to the capacity"
        // can never fire. The breach is instead a FULLNESS test: the budget is >= 90% full
        // (accumulation tops out AT capacity; the only path above is a single oversized
        // request admitted while the budget was empty) AND the cache is cold.
        let (Some(inflight), Some(hit)) = (o.trusted_inflight_tokens, o.cache_hit_rate) else {
            return; // no reading is gate_down/metrics' business, never this rule's
        };
        let Some(capacity) = o.trusted_budget_tokens.filter(|c| *c > 0) else {
            return; // no capacity published: nothing to be full OF
        };
        let full = (inflight as f64) >= 0.9 * (capacity as f64);
        let breach = full && hit < cfg.cold_budget_hit_rate;
        push_edge(
            out,
            self.cold_budget.step(o.now, breach, cfg.cold_budget_secs, cfg.cooldown_secs),
            o.now,
            "gate_cold_budget",
            Severity::Warn,
            format!(
                "COLD-CACHE HEAVY BUDGET: {inflight} of {capacity} tokens in flight (>=90% full) with cache hit below {} for {} - the budget's tokens are now REAL prefill; this is the state that preceded both Xid-8 hangs. Reduce the budget or shed load before the engine wedges",
                cfg.cold_budget_hit_rate * 100.0,
                fmt_duration(cfg.cold_budget_secs)
            ),
            "cold-cache heavy budget recovered: in-flight back under the ceiling or the cache warmed up".to_string(),
        );
    }

    fn eval_restarts(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        for r in &o.restarts {
            let key = format!("container_restart:{}", r.role);
            // the restart clock always restarts, even when the alert itself is in cooldown
            let owed = self.restart_pending.contains_key(&r.role);
            if self.event_cooled(&key, o.now, cfg.cooldown_secs) {
                self.event_last_sent.insert(key.clone(), o.now);
                self.restart_pending.insert(r.role.clone(), o.now);
                self.restart_planned.insert(r.role.clone(), o.maintenance_active);
                let (sev, label) = if o.maintenance_active { (Severity::Info, "planned - ") } else { (Severity::Warn, "") };
                out.push(alert(o.now, &key, sev, false, format!("{label}{} container restarted: {}", r.role, r.detail)));
            } else if owed {
                self.restart_pending.insert(r.role.clone(), o.now);
            }
        }
        let stable: Vec<String> = self
            .restart_pending
            .iter()
            .filter(|(role, since)| {
                let healthy = o.running_roles.contains(role) && if role.as_str() == "serve" { o.serve_up } else { o.gate_up };
                healthy && o.now - **since >= cfg.restart_stable_secs
            })
            .map(|(role, _)| role.clone())
            .collect();
        for role in stable {
            let since = self.restart_pending.remove(&role).unwrap_or(o.now);
            // paired with its own FIRE, not with whatever maintenance_active is NOW: the window
            // may already have been closed (planned work confirmed done) before the container
            // finishes settling
            let (sev, label) = if self.restart_planned.remove(&role).unwrap_or(false) { (Severity::Info, "planned - ") } else { (Severity::Warn, "") };
            out.push(alert(o.now, &format!("container_restart:{role}"), sev, true,
                format!("{label}{role} container recovered: running and answering {} after the restart", fmt_duration(o.now - since))));
        }
    }

    /// #36, 2026-09-21: omp's shared config naming a default model this provider does not
    /// serve - the 2026-09-20 incident (omp fell back to an outside provider without a word)
    /// this rule exists to make impossible to miss again. Held for `omp_mismatch_hold_secs`
    /// before firing so the few seconds a swap takes to land in both the served model and (once
    /// it is re-pointed) omp's own config never look like a fault.
    fn eval_omp_default(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let mismatched = o.omp_mismatch.is_some();
        match self.omp_default.step(o.now, mismatched, cfg.omp_mismatch_hold_secs, cfg.cooldown_secs) {
            Edge::Fire => {
                let (configured, served) = o.omp_mismatch.clone().unwrap_or_default();
                out.push(alert(o.now, "omp_default_mismatch", Severity::Warn, false,
                    format!("omp's default model ({configured}) is not what is served ({served}) - agent sessions may silently fall back to an outside provider: re-point omp's default at the served id")));
            }
            Edge::Recover => out.push(alert(o.now, "omp_default_mismatch", Severity::Warn, true, "omp's default model matches what is served again".to_string())),
            Edge::None => {}
        }
    }

    fn eval_xids(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        for x in &o.xids {
            let key = match x.gpu {
                Some(i) => format!("xid:gpu{i}"),
                None => format!("xid:{}", x.pci),
            };
            // An Xid storm on one GPU is one alert per cooldown; every line is still an incident.
            if self.event_cooled(&key, o.now, cfg.cooldown_secs) {
                self.event_last_sent.insert(key.clone(), o.now);
                out.push(alert(o.now, &key, Severity::Hardware, false,
                    format!("{} Xid {} - {} [{}]", x.gpu_label(), x.xid, xid_hint(x.xid), x.detail)));
            }
        }
    }

    fn event_cooled(&self, key: &str, now: i64, cooldown: i64) -> bool {
        self.event_last_sent.get(key).is_none_or(|t| now - t >= cooldown)
    }

    /// card #264: `warn` at `disk_warn_pct` used, `page` at `disk_page_pct`, each held
    /// `disk_secs`. HYSTERESIS: once a rule has fired it stays up until the disk drops
    /// `disk_recover_margin_pct` points below its own line, so a disk sitting on the line cannot
    /// fire and recover every poll. No reading holds both rules as they are.
    fn eval_disk(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let Some(d) = &o.disk else { return };
        let pct = d.used_pct();
        for (s, line, rule, sev) in [(&mut self.disk_full, cfg.disk_warn_pct, "disk_full", Severity::Warn), (&mut self.disk_full_page, cfg.disk_page_pct, "disk_full_page", Severity::Page)] {
            let over = if s.announced { pct >= line - cfg.disk_recover_margin_pct } else { pct >= line };
            let e = s.step(o.now, over, cfg.disk_secs, cfg.cooldown_secs);
            push_edge(out, e, o.now, rule, sev,
                format!("disk {} on this box is {} - >= {line:.0}% for {}", d.path, d.describe(), fmt_duration(cfg.disk_secs)),
                format!("disk {} back to {}", d.path, d.describe()));
        }
    }

    fn eval_queue(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        // during a bench the queue and the gate's waiters are read as empty: a rule that was
        // pending stops counting, and one that was already firing recovers
        let q = if o.bench_active { 0.0 } else { o.queue.unwrap_or(0.0) };
        match self.queue.step(o.now, q >= cfg.queue_reqs, cfg.queue_secs, cfg.cooldown_secs) {
            Edge::Fire => out.push(alert(o.now, "queue_pressure", Severity::Warn, false,
                format!("queue pressure: {q:.0} requests queued (>= {:.0}) for {}", cfg.queue_reqs, fmt_duration(cfg.queue_secs)))),
            Edge::Recover => out.push(alert(o.now, "queue_pressure", Severity::Warn, true,
                format!("queue pressure recovered: {q:.0} queued"))),
            Edge::None => {}
        }
        let w = if o.bench_active { 0 } else { o.trusted_waiters.unwrap_or(0) };
        match self.waiters.step(o.now, w > 0, cfg.waiters_secs, cfg.cooldown_secs) {
            Edge::Fire => out.push(alert(o.now, "gate_waiters", Severity::Warn, false,
                format!("queue pressure: {w} trusted request(s) waiting at the gate for token budget for {}", fmt_duration(cfg.waiters_secs)))),
            Edge::Recover => out.push(alert(o.now, "gate_waiters", Severity::Warn, true,
                "gate waiters recovered: nobody waiting for token budget".to_string())),
            Edge::None => {}
        }
    }

    fn eval_gpus(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        let count = o.gpus.as_ref().map_or(0, Vec::len);
        self.gpu_count_seen = self.gpu_count_seen.max(count);
        let missing = count < self.gpu_count_seen;
        match self.gpu_missing.step(o.now, missing, cfg.gpu_missing_secs, cfg.cooldown_secs) {
            Edge::Fire => out.push(alert(o.now, "gpu_missing", Severity::Hardware, false,
                format!("nvidia-smi reports {count} GPU(s), expected {} - a GPU may have fallen off the bus", self.gpu_count_seen))),
            Edge::Recover => out.push(alert(o.now, "gpu_missing", Severity::Hardware, true,
                format!("all {count} GPUs visible again"))),
            Edge::None => {}
        }

        // No reading = no opinion: thermal state is held, never "recovered" by a failed poll.
        let Some(gpus) = &o.gpus else {
            self.digest.last_eval = Some(o.now);
            return;
        };
        let dt = self.digest.last_eval.map_or(0, |t| (o.now - t).clamp(0, 60));
        self.digest.last_eval = Some(o.now);
        self.digest.window_start.get_or_insert(o.now);

        for g in gpus {
            let hot = g.temp_c.is_some_and(|t| t >= cfg.thermal_temp_c);
            let throttled = g.throttle_mask & THROTTLE_THERMAL_MASK != 0;
            if cfg.thermal_exclude.contains(&g.index) {
                let d = self.digest.gpus.entry(g.index).or_default();
                d.max_temp_c = d.max_temp_c.max(g.temp_c.unwrap_or(0.0));
                d.hot_secs += if hot { dt } else { 0 };
                d.throttled_secs += if throttled { dt } else { 0 };
                continue;
            }
            let temp = g.temp_c.unwrap_or(0.0);
            let e = self.thermal_temp.entry(g.index).or_default().step(o.now, hot, cfg.thermal_secs, cfg.cooldown_secs);
            push_edge(out, e, o.now, &format!("thermal_temp:gpu{}", g.index), Severity::Warn,
                format!("GPU{} at {}, >= {} for {}", g.index, crate::units::temp(temp), crate::units::temp(cfg.thermal_temp_c), fmt_duration(cfg.thermal_secs)),
                format!("GPU{} thermal recovered: {}", g.index, crate::units::temp(temp)));
            let e = self.thermal_throttle.entry(g.index).or_default().step(o.now, throttled, cfg.thermal_secs, cfg.cooldown_secs);
            push_edge(out, e, o.now, &format!("thermal_throttle:gpu{}", g.index), Severity::Warn,
                format!("GPU{} thermal slowdown active ({}) for {}, {}", g.index, throttle_flags(g.throttle_mask).join("+"), fmt_duration(cfg.thermal_secs), crate::units::temp(temp)),
                format!("GPU{} thermal slowdown cleared", g.index));
        }

        if self.digest.window_start.is_some_and(|s| o.now - s >= DIGEST_PERIOD_SECS) {
            let lines: Vec<String> = self
                .digest
                .gpus
                .iter()
                .filter(|(_, d)| d.hot_secs > 0 || d.throttled_secs > 0)
                .map(|(i, d)| format!("GPU{i} max {}, {} at >= {}, {} thermally throttled",
                    crate::units::temp(d.max_temp_c), fmt_duration(d.hot_secs), crate::units::temp(cfg.thermal_temp_c), fmt_duration(d.throttled_secs)))
                .collect();
            if !lines.is_empty() {
                out.push(alert(o.now, "thermal_digest", Severity::Info, false,
                    format!("daily thermal digest (excluded from alerts): {}", lines.join("; "))));
            }
            self.digest.gpus.clear();
            self.digest.window_start = Some(o.now);
        }
    }

    fn eval_c1(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        // only VALID probes ever get here (probe::c1_reading): an invalid one is no observation.
        // A probe that finished while a bench was running is not a reading of an idle server.
        if o.bench_active {
            return;
        }
        if let Some(v) = o.probe_tok_s {
            self.c1.validated = true;
            self.c1.last_valid_ts = Some(o.now);
            if let Some(baseline) = self.c1_baseline(cfg) {
                let floor = cfg.c1_ratio * baseline;
                if v < floor {
                    let ts = o.probe_ts.unwrap_or(o.now);
                    if !self.c1.low_probes.iter().any(|(t, _)| *t == ts) {
                        self.c1.low_probes.push((ts, v));
                    }
                    let n = self.c1.low_probes.len();
                    let cooled = self.c1.last_sent.is_none_or(|t| o.now - t >= cfg.cooldown_secs);
                    if n >= cfg.c1_consecutive as usize && !self.c1.announced && cooled {
                        self.c1.announced = true;
                        self.c1.last_sent = Some(o.now);
                        let used: Vec<String> = self.c1.low_probes.iter().map(|(t, v)| format!("{} {v:.1}", crate::timeutil::fmt_utc(*t, "%H:%M:%SZ"))).collect();
                        out.push(alert(o.now, "c1_decode", Severity::Warn, false,
                            format!("C1 decode {v:.1} tok/s, below {floor:.1} ({:.0}% of baseline {baseline:.1}) on {n} consecutive valid idle probes (probes {})",
                                cfg.c1_ratio * 100.0, used.join(", "))));
                    }
                } else {
                    self.c1.low_probes.clear();
                    if std::mem::take(&mut self.c1.announced) {
                        out.push(alert(o.now, "c1_decode", Severity::Warn, true,
                            format!("C1 decode recovered: {v:.1} tok/s (baseline {baseline:.1})")));
                    }
                }
            } else {
                // still learning: the first N idle probes define normal and are not judged
                self.c1.baseline_samples.push(v);
                if self.c1.baseline_samples.len() >= cfg.c1_baseline_probes.max(1) {
                    self.c1.learned_baseline = Some(median(&self.c1.baseline_samples));
                }
            }
        }
        // #47: runs every tick, whether or not a fresh reading arrived this one, and is entirely
        // independent of the low-decode alert above - neither can gate or delay the other.
        self.eval_c1_stale(cfg, o, out);
    }

    /// A busy box can leave C1 without a single VALID reading for hours (card #45's stricter
    /// validation is honest about contention it used to miss): an hours-old number sitting next
    /// to a live clock, with nothing saying so, is the same kind of lie in the other direction.
    /// Fires once the newest valid reading has been `c1_stale_probe_intervals` probe intervals
    /// old for `C1_STALE_FIRE_HOLD_SECS` continuously (#52, 2026-09-21: the multi-interval wait
    /// was already most of the gate, but a hold on top, however small, is cheap insurance against
    /// firing the instant a boundary is crossed), and recovers the instant a valid reading
    /// arrives again.
    ///
    /// #52: on a box with only brief scattered idle windows (card #50's measurement: the longest
    /// unbroken quiet run all week was 5m00s) C1 genuinely, truthfully alternates between "stale
    /// 30+ min" and "one probe just snuck through" - each cycle on its own is a real fact, but
    /// repeating the same fire/recover pair five times in an evening tells the reader nothing new
    /// after the first. Past `FLAP_MUTE_THRESHOLD` flaps inside `FLAP_MUTE_WINDOW_SECS`, this
    /// identity goes quiet on the individual flaps (no new fire/recover `AlertEvent`s - nothing
    /// new lands in the mail spool) but is NEVER hidden: `rule_states` rewrites the row itself to
    /// name the flapping and keeps counting flaps for as long as they keep happening.
    fn eval_c1_stale(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        const C1_STALE_FIRE_HOLD_SECS: i64 = 300;
        // seeded to "now" on the very first tick ever evaluated: a grace period while the
        // collector is still warming up, never an alarm the instant it starts
        let last = *self.c1.last_valid_ts.get_or_insert(o.now);
        let threshold = o.probe_interval_secs.max(1) * i64::from(cfg.c1_stale_probe_intervals.max(1));
        let stale_for = o.now - last;
        let was_stale_for = self.c1.stale.held_for(o.now);
        let edge = self.c1.stale.step(o.now, stale_for >= threshold, C1_STALE_FIRE_HOLD_SECS, cfg.cooldown_secs);
        if edge == Edge::Recover {
            self.c1.stale_flaps.note_flap(o.now);
        }
        let muted = self.c1.stale_flaps.muted(o.now);
        match edge {
            Edge::Fire if !muted => out.push(alert(o.now, "c1_stale", Severity::Info, false,
                format!("C1 has not been able to measure for {} - the server has been busy", fmt_duration(stale_for)))),
            Edge::Recover if !muted => out.push(alert(o.now, "c1_stale", Severity::Info, true,
                format!("C1 is measuring again after {} unable to get a valid reading", fmt_duration(was_stale_for)))),
            _ => {}
        }
        if muted && !self.c1.stale_flaps.muted_announced {
            self.c1.stale_flaps.muted_announced = true;
            out.push(alert(o.now, "c1_stale", Severity::Info, false,
                format!("c1_stale has flapped {} times in {} - the MEASUREMENT is unreliable, not the server; going quiet on individual fire/recover pairs, the row stays visible and counted",
                    self.c1.stale_flaps.count_since(o.now, FLAP_MUTE_WINDOW_SECS), fmt_duration(FLAP_MUTE_WINDOW_SECS))));
        } else if !muted {
            self.c1.stale_flaps.muted_announced = false; // the flap rate calmed down: back to normal
        }
    }

    fn eval_rejects(&mut self, cfg: &RulesConfig, o: &Observation, out: &mut Vec<AlertEvent>) {
        for (code, value, win) in [(413, o.rejected_413, &mut self.rejects_413), (429, o.rejected_429, &mut self.rejects_429)] {
            let Some(value) = value else { continue };
            let growth = win.push(o.now, value, cfg.reject_window_secs);
            if code == 413 {
                self.last.growth_413 = Some(growth);
            } else {
                self.last.growth_429 = Some(growth);
            }
            let e = win.rule.step(o.now, growth > cfg.reject_growth, 0, cfg.cooldown_secs);
            push_edge(out, e, o.now, &format!("rejects_{code}"), Severity::Info,
                format!("gate rejected_{code} grew by {growth} in {} (> {})", fmt_duration(cfg.reject_window_secs), cfg.reject_growth),
                format!("gate rejected_{code} growth back to normal ({growth} in {})", fmt_duration(cfg.reject_window_secs)));
        }
    }
}

fn alert(ts: i64, rule: &str, severity: Severity, recovered: bool, message: String) -> AlertEvent {
    let message = if recovered { format!("RECOVERED: {message}") } else { message };
    AlertEvent { ts, rule: rule.to_string(), severity, message, recovered }
}

fn push_edge(out: &mut Vec<AlertEvent>, e: Edge, now: i64, rule: &str, sev: Severity, fire: String, recover: String) {
    match e {
        Edge::Fire => out.push(alert(now, rule, sev, false, fire)),
        Edge::Recover => out.push(alert(now, rule, sev, true, recover)),
        Edge::None => {}
    }
}

pub fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    if v.len() % 2 == 1 { v[mid] } else { (v[mid - 1] + v[mid]) / 2.0 }
}
