//! Who is on the server: the gate's per-user live stats (a gateway publishing /gate/health v5.2+), with friendly
//! names for the trusted lane, whose "users" are client addresses.

use crate::gate::{series_id, GateHealth, GateTotals, GateUser};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `[[user_alias]] ip = "…" name = "…"` in the collector's config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserAlias {
    pub ip: String,
    pub name: String,
}

/// The friendly name of a user. Only trusted-lane users are addresses; a public user already
/// IS a name (the key's). The config map wins over the tailscale lookup.
pub fn alias_for(lane: &str, user: &str, aliases: &[UserAlias], tailscale: &BTreeMap<String, String>) -> Option<String> {
    if lane != "trusted" {
        return None;
    }
    let same = |a: &str| match (a.trim().parse::<std::net::IpAddr>(), user.trim().parse::<std::net::IpAddr>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == user.trim(),
    };
    aliases.iter().find(|a| same(&a.ip) && !a.name.is_empty()).map(|a| a.name.clone()).or_else(|| tailscale.iter().find(|(ip, _)| same(ip)).map(|(_, name)| name.clone()))
}

/// `tailscale status --json` -> address -> host name (this machine and every peer).
pub fn parse_tailscale_status(json: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return out };
    let mut node = |n: &serde_json::Value| {
        let name = n["HostName"].as_str().filter(|s| !s.is_empty()).or_else(|| n["DNSName"].as_str().and_then(|d| d.split('.').next())).unwrap_or("");
        if name.is_empty() {
            return;
        }
        for ip in n["TailscaleIPs"].as_array().into_iter().flatten().filter_map(|i| i.as_str()) {
            out.insert(ip.to_string(), name.to_string());
        }
    };
    node(&v["Self"]);
    for peer in v["Peer"].as_object().into_iter().flat_map(|m| m.values()) {
        node(peer);
    }
    out
}

/// What goes into a stored 5 s sample for a user who is doing something right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPoint {
    /// `gate::series_id`: the `<id>` of `user_inflight.<id>` / `user_rpm.<id>`
    pub id: String,
    pub inflight: u64,
    pub rpm: u64,
    /// card #211: `inflight` split by the gate's upstream name (v5.10+), nonzero entries only.
    /// Empty on an older gate - `Sample.gate.attributed_upstreams` says which case it is.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_upstream: BTreeMap<String, u64>,
}

/// The users of one poll worth keeping as series: whoever has a request in flight or sent one
/// in the last minute. At most 16, busiest first, so a key-spraying client cannot grow the sample.
pub fn user_points(gate: &GateHealth) -> Vec<UserPoint> {
    let mut rows: Vec<&GateUser> = gate.users.iter().flatten().filter(|u| u.inflight > 0 || u.rpm_now > 0).collect();
    rows.sort_by(|a, b| b.inflight.cmp(&a.inflight).then(b.rpm_now.cmp(&a.rpm_now)).then(a.user.cmp(&b.user)));
    rows.into_iter().take(16).map(|u| UserPoint {
            id: series_id(&u.lane, &u.user),
            inflight: u.inflight,
            rpm: u.rpm_now,
            by_upstream: u.by_upstream.iter().filter(|(_, l)| l.inflight > 0).map(|(up, l)| (up.clone(), l.inflight)).collect(),
        }).collect()
}

/// One row of `/status.users.rows[]`: the gate's row plus its friendly name and series id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserRow {
    /// what to call them: the alias when there is one, else `user`
    pub name: String,
    pub series_id: String,
    /// THEIR OWN experience over the last 24 h, from the gate's log (a gateway publishing /gate/health v5.2+; null
    /// until they had an answered request in that time)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experience: Option<UserExperience>,
    /// card #195: HOW LONG SINCE THIS CALLER LAST DID ANYTHING, in seconds, worked out by the
    /// collector against its OWN clock at the moment it read the gateway's snapshot. An AGE, not
    /// the raw `last_seen` wall-clock the gateway publishes: a screen reading `/status` from
    /// another box must never have to reason about clock skew between the two to answer "is this
    /// person still on". Same shape and same reason as `serve.secs_since_last_token`.
    ///
    /// It is since their last REQUEST, not since their last token: the gateway stamps `last_seen`
    /// when it accounts a request and keeps no per-user token clock. So a caller streaming a long
    /// answer right now has BOTH a growing age and `inflight > 0`, which is why anything reading
    /// this must read in-flight first and this second.
    ///
    /// `None` = the gateway published no `last_seen` for them at all. That is UNKNOWN - never 0
    /// and never "idle". An absent `last_seen` deserialises to `0.0`, and a missing timestamp
    /// rendered as an age would claim the caller was here this very second.
    pub secs_since_last_request: Option<i64>,
    #[serde(flatten)]
    pub gate: GateUser,
}

/// The age of a gateway timestamp at the collector's `now`, for `UserRow::secs_since_last_request`.
/// `0.0` is what serde gives an ABSENT `last_seen`, so it is the one value that means "the gateway
/// did not say" rather than a real instant - it becomes `None`, never an age. Clamped at zero at
/// the other end: a gateway clock a second ahead of the collector's reads as "just now", which is
/// true, instead of as a negative age.
fn secs_since(stamp: f64, now: i64) -> Option<i64> {
    (stamp > 0.0).then(|| (now as f64 - stamp).max(0.0).round() as i64)
}

/// What one user got: time to the first word and writing speed, over the last 24 hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserExperience {
    /// answered requests behind the numbers
    pub requests: u64,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p95_ms: Option<f64>,
    /// their generated tokens over their writing seconds
    pub tok_s: Option<f64>,
    /// the median request's writing speed
    pub tok_s_p50: Option<f64>,
}

impl UserExperience {
    pub fn of(e: &crate::gatelog::UserExp) -> Option<UserExperience> {
        let r1 = |v: f64| (v * 10.0).round() / 10.0;
        (e.n > 0).then(|| UserExperience { requests: e.n, ttft_p50_ms: e.ttfb_quantile_ms(0.5).map(f64::round), ttft_p95_ms: e.ttfb_quantile_ms(0.95).map(f64::round), tok_s: e.speed_avg().map(r1), tok_s_p50: e.speed_quantile(0.5).map(r1) })
    }
}

/// Give every row its user's experience. `log` is the gate log merged over the last 24 h.
/// The address the collector's own probe comes from is left out: its tiny requests cannot be
/// told from that user's in the log, and would flatter them.
pub fn attach_experience(users: &mut UsersStatus, log: &crate::gatelog::LogDelta, probe_user: Option<&str>) {
    let find = |lane: &str, user: &str| -> Option<UserExperience> {
        if lane == "trusted" && probe_user == Some(user) {
            return None;
        }
        let l = if lane == "public" { &log.public } else { &log.trusted };
        l.by_user.get(user).and_then(UserExperience::of)
    };
    for r in users.rows.iter_mut().chain(users.bench.iter_mut()) {
        r.experience = find(&r.gate.lane, &r.gate.user);
    }
}

/// The name the gate files `X-LSS-Bench: 1` requests under (`lss bench`'s own traffic).
pub const BENCH_USER: &str = "lss-bench";
/// The name the collector's own C1 probe requests are shown under.
pub const PROBE_USER: &str = "lss-probe";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsersStatus {
    /// false = the gate does not publish per-user stats (older than v5.2, or down)
    pub available: bool,
    /// REAL users with a request in flight right now (never the bench, never the probe)
    pub active_now: u64,
    /// the gate's totals with the bench and the probe taken out
    pub totals: GateTotals,
    /// real users, busiest first (in flight, then requests in 24 h); at most 50
    pub rows: Vec<UserRow>,
    /// `lss bench` traffic, shown on its own and in no total
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bench: Option<UserRow>,
    /// the collector's own C1 probe requests, shown on their own and in no total
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<UserRow>,
}

/// What the collector knows about its OWN requests, so they can be taken out of the user table.
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnTraffic<'a> {
    /// the address the gate sees the probe come from (`127.0.0.1` for a loopback gate URL)
    pub probe_user: Option<&'a str>,
    /// probe requests the gate answered in the last hour / 24 hours
    pub probes_1h: u64,
    pub probes_24h: u64,
}

/// How long after their last request a caller with nothing in flight still counts as "active".
/// The gateway's own `users_active_10m` counts exactly this window, so anything that calls a
/// caller quiet by this constant can never contradict the "N in 10 min" figure published beside
/// it. card #195 reuses it as the USERS table's live/quiet line for that reason.
pub const ACTIVE_WINDOW_SECS: f64 = 600.0;

pub fn build_users(gate: Option<&GateHealth>, aliases: &[UserAlias], tailscale: &BTreeMap<String, String>, own: &OwnTraffic, now: i64) -> UsersStatus {
    let Some(users) = gate.and_then(|g| g.users.as_ref()) else { return UsersStatus::default() };
    let named = |u: &GateUser| UserRow { name: alias_for(&u.lane, &u.user, aliases, tailscale).unwrap_or_else(|| u.user.clone()), series_id: series_id(&u.lane, &u.user), experience: None, secs_since_last_request: secs_since(u.last_seen, now), gate: u.clone() };
    let recent = |u: &GateUser| now as f64 - u.last_seen <= ACTIVE_WINDOW_SECS;
    let mut totals = gate.and_then(|g| g.totals.clone()).unwrap_or_else(|| GateTotals { inflight: users.iter().map(|u| u.inflight).sum(), ..Default::default() });
    let mut rows: Vec<UserRow> = Vec::new();
    let (mut bench, mut probe) = (None, None);
    for u in users {
        if u.lane == "trusted" && u.user == BENCH_USER {
            totals.inflight = totals.inflight.saturating_sub(u.inflight);
            // card #211: and out of the per-machine split, on the machine it was running on
            if let Some(split) = totals.inflight_by_upstream.as_mut() {
                for (up, l) in &u.by_upstream {
                    if let Some(n) = split.get_mut(up) {
                        *n = n.saturating_sub(l.inflight);
                    }
                }
                split.retain(|_, n| *n > 0);
            }
            totals.users_24h = totals.users_24h.saturating_sub(1);
            totals.users_active_10m = totals.users_active_10m.saturating_sub(u64::from(recent(u)));
            bench = Some(named(u));
            continue;
        }
        let is_probe_address = u.lane == "trusted" && own.probe_user.is_some_and(|p| p == u.user) && own.probes_24h > 0;
        if !is_probe_address {
            rows.push(named(u));
            continue;
        }
        // The probe's address: the collector knows how many of these requests were its own.
        // They become the `lss-probe` row; whatever is left is a real user on the same address.
        let mine_24h = own.probes_24h.min(u.requests_24h);
        let mine_1h = own.probes_1h.min(u.requests_1h);
        probe = Some(UserRow {
            name: PROBE_USER.into(),
            series_id: series_id(&u.lane, PROBE_USER),
            experience: None,
            secs_since_last_request: secs_since(u.last_seen, now),
            gate: GateUser { lane: u.lane.clone(), user: PROBE_USER.into(), requests_1h: mine_1h, requests_24h: mine_24h, ok_24h: mine_24h.min(u.ok_24h), completion_tokens_exact: true, first_seen: u.first_seen, last_seen: u.last_seen, last_status: u.last_status, ..Default::default() },
        });
        let mut rest = u.clone();
        rest.requests_24h -= mine_24h;
        rest.requests_1h -= mine_1h;
        rest.ok_24h = rest.ok_24h.saturating_sub(mine_24h);
        if rest.requests_24h == 0 && rest.inflight == 0 {
            // nobody but the probe used this address
            totals.users_24h = totals.users_24h.saturating_sub(1);
            totals.users_active_10m = totals.users_active_10m.saturating_sub(u64::from(recent(u)));
        } else {
            rows.push(named(&rest));
        }
    }
    rows.sort_by(|a, b| b.gate.inflight.cmp(&a.gate.inflight).then(b.gate.requests_24h.cmp(&a.gate.requests_24h)).then(a.name.cmp(&b.name)));
    let active_now = rows.iter().filter(|r| r.gate.inflight > 0).count() as u64;
    rows.truncate(50);
    UsersStatus { available: true, active_now, totals, rows, bench, probe }
}

/// How `s` cycles the USERS table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UserSort {
    /// running now, then requests in 24 h (the order `/status` sends)
    #[default]
    Busiest,
    Requests24h,
    OutputTokens,
    PromptTokens,
    LastSeen,
    Name,
}

impl UserSort {
    pub const ALL: [UserSort; 6] = [UserSort::Busiest, UserSort::Requests24h, UserSort::OutputTokens, UserSort::PromptTokens, UserSort::LastSeen, UserSort::Name];

    pub fn label(self) -> &'static str {
        match self {
            UserSort::Busiest => "running now",
            UserSort::Requests24h => "requests 24h",
            UserSort::OutputTokens => "output tokens",
            UserSort::PromptTokens => "prompt tokens",
            UserSort::LastSeen => "last seen",
            UserSort::Name => "name",
        }
    }

    pub fn next(self) -> UserSort {
        Self::ALL[(Self::ALL.iter().position(|s| *s == self).unwrap_or(0) + 1) % Self::ALL.len()]
    }

    pub fn apply(self, rows: &mut [UserRow]) {
        match self {
            UserSort::Busiest => rows.sort_by(|a, b| b.gate.inflight.cmp(&a.gate.inflight).then(b.gate.requests_24h.cmp(&a.gate.requests_24h)).then(a.name.cmp(&b.name))),
            UserSort::Requests24h => rows.sort_by(|a, b| b.gate.requests_24h.cmp(&a.gate.requests_24h).then(a.name.cmp(&b.name))),
            UserSort::OutputTokens => rows.sort_by(|a, b| b.gate.completion_tokens_24h.cmp(&a.gate.completion_tokens_24h).then(a.name.cmp(&b.name))),
            UserSort::PromptTokens => rows.sort_by(|a, b| b.gate.prompt_tokens_est_24h.cmp(&a.gate.prompt_tokens_est_24h).then(a.name.cmp(&b.name))),
            UserSort::LastSeen => rows.sort_by(|a, b| b.gate.last_seen.total_cmp(&a.gate.last_seen).then(a.name.cmp(&b.name))),
            UserSort::Name => rows.sort_by_key(|a| a.name.to_lowercase()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::parse_gate_health;

    const V52: &str = include_str!("../../../fixtures/gate_health_v52.json");
    const NOW: i64 = 1_789_820_000;

    #[test]
    fn the_user_table_parses_and_is_ordered_busiest_first() {
        let gate = parse_gate_health(V52).unwrap();
        assert_eq!(gate.version, "v5.2");
        let u = build_users(Some(&gate), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW);
        assert!(u.available);
        assert_eq!((u.active_now, u.totals.inflight, u.totals.users_active_10m, u.totals.users_24h, u.totals.unauthenticated_24h), (2, 3, 3, 5, 7));
        let names: Vec<&str> = u.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["acme", "192.0.2.76", "127.0.0.1", "2001:db8::17", "canary"]);
        let acme = &u.rows[0].gate;
        assert_eq!((acme.inflight, acme.peak_inflight_10m, acme.peak_inflight_24h, acme.conc_limit, acme.rpm_limit, acme.rpm_now), (2, 3, 4, Some(4), Some(60), 12));
        assert_eq!((acme.requests_1h, acme.requests_24h, acme.ok_24h, acme.rejected_24h, acme.errors_24h, acme.client_closed_24h), (340, 2100, 2050, 12, 3, 35));
        assert!(acme.completion_tokens_exact && !u.rows[1].gate.completion_tokens_exact);
        assert_eq!((u.rows[1].gate.conc_limit, u.rows[3].gate.last_status), (None, Some(502)));
        // ids are safe in `/series?metrics=a,b:max` whatever the user is called
        assert_eq!((u.rows[0].series_id.as_str(), u.rows[1].series_id.as_str(), u.rows[3].series_id.as_str()), ("p.acme", "t.192.0.2.76", "t.2001_db8__17"));
        assert_eq!(series_id("public", "a,b:c d/é"), "p.a_b_c_d__");
        let json = serde_json::to_value(&u).unwrap();
        assert_eq!((json["rows"][0]["user"].as_str(), json["rows"][0]["inflight"].as_u64(), json["rows"][1]["conc_limit"].is_null()), (Some("acme"), Some(2), true));
    }

    #[test]
    fn a_gate_without_user_stats_is_simply_not_available() {
        // the gateway v5.1: no `users`, no `totals`
        let old = parse_gate_health(r#"{"version":"v5.1","upstreams":{"gpu":{"ok":true}},"admission":{"trusted":{"admitted":3}}}"#).unwrap();
        assert_eq!((old.users.is_none(), old.totals.is_none()), (true, true));
        let u = build_users(Some(&old), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW);
        assert_eq!(u, UsersStatus::default());
        assert!(!u.available && u.rows.is_empty());
        assert_eq!(build_users(None, &[], &BTreeMap::new(), &OwnTraffic::default(), NOW), UsersStatus::default(), "gate down");
        assert!(user_points(&old).is_empty());
        // an older lss reading a newer /status, and a newer lss reading an older one
        let none: UsersStatus = serde_json::from_str("{}").unwrap();
        assert!(!none.available);
        // `users: []` IS available: a v5.2 gate nobody has used yet
        let empty = parse_gate_health(r#"{"version":"v5.2","users":[],"totals":{"inflight":0}}"#).unwrap();
        assert!(build_users(Some(&empty), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW).available);
        // the users never reach a stored sample
        let gate = parse_gate_health(V52).unwrap();
        assert!(!serde_json::to_string(&gate).unwrap().contains("acme"));
    }

    #[test]
    fn aliases_name_trusted_addresses_config_first_then_tailscale() {
        let aliases = vec![UserAlias { ip: "192.0.2.76".into(), name: "laptop".into() }, UserAlias { ip: "2001:0db8:0::17".into(), name: "v6-box".into() }, UserAlias { ip: "acme".into(), name: "nope".into() }];
        let ts = parse_tailscale_status(r#"{"Self":{"HostName":"gpu-box","TailscaleIPs":["198.51.100.1"]},"Peer":{"k1":{"HostName":"seat","TailscaleIPs":["192.0.2.76","2001:db8::99"]},"k2":{"HostName":"","DNSName":"phone.tail.example.","TailscaleIPs":["198.51.100.9"]}}}"#);
        assert_eq!(ts.get("198.51.100.9").map(String::as_str), Some("phone"));
        assert_eq!(alias_for("trusted", "192.0.2.76", &aliases, &ts).as_deref(), Some("laptop"), "the config map wins");
        assert_eq!(alias_for("trusted", "2001:db8::17", &aliases, &ts).as_deref(), Some("v6-box"), "addresses compare as addresses, not as text");
        assert_eq!(alias_for("trusted", "2001:db8::99", &[], &ts).as_deref(), Some("seat"));
        assert_eq!(alias_for("trusted", "198.51.100.1", &[], &ts).as_deref(), Some("gpu-box"));
        assert_eq!(alias_for("trusted", "203.0.113.200", &aliases, &ts), None);
        assert_eq!(alias_for("public", "acme", &aliases, &ts), None, "a public user is a key name: never aliased");
        assert!(parse_tailscale_status("not json").is_empty());
        let gate = parse_gate_health(V52).unwrap();
        let u = build_users(Some(&gate), &aliases, &ts, &OwnTraffic::default(), NOW);
        assert_eq!(u.rows[1].name, "laptop");
        assert_eq!(u.rows[1].gate.user, "192.0.2.76", "the raw id stays");
    }

    /// card #195: the collector publishes an AGE per caller, and a caller the gateway never gave a
    /// `last_seen` for is UNKNOWN - the one thing this must never do is call them "0 seconds ago".
    #[test]
    fn every_user_carries_how_long_since_their_last_request_and_never_invents_one() {
        let gate = parse_gate_health(V52).unwrap();
        let u = build_users(Some(&gate), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW);
        let age = |name: &str| u.rows.iter().find(|r| r.name == name).expect("row").secs_since_last_request;
        // the fixture's own stamps, as ages at NOW: 9.8s, 2s, 5 min, 8h11m, 5h16m
        assert_eq!((age("acme"), age("192.0.2.76"), age("127.0.0.1")), (Some(10), Some(2), Some(300)));
        assert_eq!((age("2001:db8::17"), age("canary")), (Some(29_500), Some(19_000)));
        // a gateway that sent no last_seen at all: serde's 0.0. UNKNOWN, never an age of `now`
        let mut quiet = gate.clone();
        quiet.users.as_mut().unwrap().retain(|g| g.user == "canary");
        quiet.users.as_mut().unwrap()[0].last_seen = 0.0;
        let q = build_users(Some(&quiet), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW);
        assert_eq!(q.rows[0].secs_since_last_request, None, "no last_seen is unknown, not 0 and not idle");
        let json = serde_json::to_value(&q).unwrap();
        assert!(json["rows"][0]["secs_since_last_request"].is_null(), "and it says so on the wire: null, not 0");
        // a gateway clock ahead of the collector's is "just now", never a negative age
        let mut ahead = quiet.clone();
        ahead.users.as_mut().unwrap()[0].last_seen = NOW as f64 + 3.0;
        assert_eq!(build_users(Some(&ahead), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW).rows[0].secs_since_last_request, Some(0));
        // the probe row gets one too - it is a caller like any other on this screen
        let own = OwnTraffic { probe_user: Some("127.0.0.1"), probes_1h: 12, probes_24h: 288 };
        let p = build_users(Some(&gate), &[], &BTreeMap::new(), &own, NOW);
        assert_eq!(p.probe.unwrap().secs_since_last_request, Some(300));
    }

    #[test]
    fn only_active_users_become_series_points() {
        let gate = parse_gate_health(V52).unwrap();
        let pts = user_points(&gate);
        assert_eq!(pts, vec![UserPoint { id: "p.acme".into(), inflight: 2, rpm: 12, ..Default::default() }, UserPoint { id: "t.192.0.2.76".into(), inflight: 1, rpm: 2, ..Default::default() }]);
    }
    #[test]
    fn the_bench_and_the_probe_are_shown_apart_and_are_in_no_total() {
        let mut gate = parse_gate_health(V52).unwrap();
        gate.users.as_mut().unwrap().push(GateUser { lane: "trusted".into(), user: BENCH_USER.into(), inflight: 8, requests_24h: 40, last_seen: NOW as f64 - 5.0, ..Default::default() });
        let t = gate.totals.as_mut().unwrap();
        (t.inflight, t.users_active_10m, t.users_24h) = (11, 4, 6);
        // 127.0.0.1 made 288 requests in 24 h and the collector sent exactly 288 probes: it is the probe
        let own = OwnTraffic { probe_user: Some("127.0.0.1"), probes_1h: 12, probes_24h: 288 };
        let u = build_users(Some(&gate), &[], &BTreeMap::new(), &own, NOW);
        let names: Vec<&str> = u.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["acme", "192.0.2.76", "2001:db8::17", "canary"], "neither lss-bench nor the probe's address is a user");
        assert_eq!((u.active_now, u.totals.inflight, u.totals.users_24h), (2, 3, 4), "8 bench requests in flight are not users; bench and probe are not users of the day");
        assert_eq!(u.totals.users_active_10m, 2, "the bench (5 s ago) and the probe (5 min ago) are not active users");
        let (bench, probe) = (u.bench.unwrap(), u.probe.unwrap());
        assert_eq!((bench.name.as_str(), bench.gate.inflight, bench.gate.requests_24h), (BENCH_USER, 8, 40));
        assert_eq!((probe.name.as_str(), probe.gate.requests_1h, probe.gate.requests_24h), (PROBE_USER, 12, 288));
        // a real user on the same address keeps what the probe did not send
        let own = OwnTraffic { probe_user: Some("127.0.0.1"), probes_1h: 2, probes_24h: 200 };
        let u = build_users(Some(&gate), &[], &BTreeMap::new(), &own, NOW);
        let local = u.rows.iter().find(|r| r.gate.user == "127.0.0.1").expect("the rest of the traffic stays a user");
        assert_eq!((local.gate.requests_1h, local.gate.requests_24h, u.probe.unwrap().gate.requests_24h, u.totals.users_24h), (10, 88, 200, 5));
        // without a probe address nothing is guessed
        assert!(build_users(Some(&gate), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW).probe.is_none());
    }

    #[test]
    fn s_cycles_the_sort_and_every_order_is_total() {
        let gate = parse_gate_health(V52).unwrap();
        let mut rows = build_users(Some(&gate), &[], &BTreeMap::new(), &OwnTraffic::default(), NOW).rows;
        let mut seen = vec![UserSort::default()];
        while seen.len() < UserSort::ALL.len() {
            seen.push(seen[seen.len() - 1].next());
        }
        assert_eq!(seen, UserSort::ALL.to_vec());
        assert_eq!(UserSort::Name.next(), UserSort::Busiest, "and round again");
        UserSort::Name.apply(&mut rows);
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["127.0.0.1", "192.0.2.76", "2001:db8::17", "acme", "canary"]);
        UserSort::Requests24h.apply(&mut rows);
        assert_eq!(rows[0].name, "acme");
        UserSort::LastSeen.apply(&mut rows);
        assert_eq!((rows[0].name.as_str(), rows[4].name.as_str()), ("192.0.2.76", "2001:db8::17"));
        assert!(UserSort::ALL.iter().all(|s| !s.label().is_empty()));
    }
}
