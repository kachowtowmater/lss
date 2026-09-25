//! the gateway `GET /gate/health`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneAdmission {
    pub admitted: u64,
    pub rejected_413: u64,
    pub rejected_429: u64,
    pub client_closed: u64,
    pub upstream_down: u64,
    pub max_duration: u64,
    pub inflight_tokens: u64,
    pub waiters: u64,
    /// #31, 2026-09-21: the in-flight prompt-token budget this lane's `inflight_tokens` is
    /// judged against. `None` on public (it has no in-flight budget, only the cap below) and on
    /// a gate older than this field.
    /// #63, 2026-09-22: on a v5.4+ gate BOTH values are in EFFECTIVE tokens (the raw estimate
    /// times max(floor, 1-cache_hit_rate); the shadow log carries `est_tokens` AND
    /// `charged_tokens` per request so the two units stay separable). Nothing to change here -
    /// the collector compares like against like.
    pub budget_tokens: Option<u64>,
    /// the per-request prompt-token cap (413 above this): always real on a gate new enough to
    /// publish it, `None` on an older one.
    pub max_prompt_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateHealth {
    pub version: String,
    /// true when every upstream the gate knows reports ok
    pub upstream_ok: bool,
    pub trusted: LaneAdmission,
    pub public: LaneAdmission,
    /// card #108: the gate's own view of its effective-token charging. `None` on a gate older
    /// than v5.5+card108. This is what the `discount_inert` rule reads - see `GateShadow`.
    pub shadow: Option<GateShadow>,
    /// Per-user live stats (a gateway publishing /gate/health v5.2+). None = this gate does not publish them.
    /// Never written into a stored sample: the active users go in as compact `UserPoint`s.
    #[serde(skip)]
    pub users: Option<Vec<GateUser>>,
    #[serde(skip)]
    pub totals: Option<GateTotals>,
    /// card #211: the gate's CONFIGURED upstream names - but only when the gate attributes load
    /// to them (v5.10+, card #209: `totals.inflight_by_upstream` is present). `None` = a gate
    /// too old to say which machine a user is on, so no per-machine figure can be built from it.
    /// Stored in the sample so the per-upstream series can emit an honest 0 for a machine nobody
    /// is on, which is what lets "0 here" be told apart from "never measured".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_upstreams: Option<Vec<String>>,
    /// card #211: each configured upstream's served model ids (`/gate/health .upstreams.<name>.loaded`),
    /// used to tell which upstream is THIS machine: the one serving the model our own engine serves.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub upstream_loaded: BTreeMap<String, Vec<String>>,
}

/// The gate's `shadow` block (the gateway v5.5 + card #108): the engine-snapshot poller's
/// health AND the charging outcome it drives. Two different failures live here:
///   - `charge_errors > 0`: the charge path RAISED, so every request falls back to gross. Card
///     #68 was exactly this (an unbound name), and it read identically to a cold cache.
///   - `cache_warm && healthy && recent_admissions high && recent_discounted == 0`: nothing
///     raised and nothing is discounted anyway. The same fault in outcome form, which is how
///     #68 stayed invisible for 2,809 requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateShadow {
    pub enabled: bool,
    pub scrapes_ok: u64,
    pub scrapes_failed: u64,
    /// the poller has a FRESH, OK snapshot right now
    pub healthy: bool,
    /// the discount is allowed to apply (past the cold-cache grace, no restart since)
    pub cache_warm: Option<bool>,
    pub cold_since_age_s: Option<f64>,
    /// the charge path raised this many times since the gate started
    pub charge_errors: u64,
    /// of the last `recent_admissions` trusted admissions, how many were DISCOUNTED
    pub recent_admissions: u64,
    pub recent_discounted: u64,
    /// card #279: seconds for the in-flight CHARGED tokens to drain at the engine's recent prefill
    /// rate (card #154 publishes it; gateway v5.10+). `None` on an older gate, or when the gate
    /// has no prefill rate to divide by - the gate itself distinguishes those two in its own
    /// `queued_uncached_reason`, and this field never invents a 0 for either.
    #[serde(default)]
    pub seconds_to_drain_charged: Option<f64>,
}

impl GateShadow {
    /// card #108: warm, seeing the engine, plenty of traffic - and not one discount. `min_n`
    /// keeps a quiet box from reading as broken.
    pub fn discount_inert(&self, min_n: u64) -> bool {
        self.enabled
            && self.healthy
            && self.cache_warm == Some(true)
            && self.recent_admissions >= min_n
            && self.recent_discounted == 0
    }
}

/// One row of the gate's `users[]`: a key NAME on the public lane, a client address on the
/// trusted one. No key material ever appears here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateUser {
    pub lane: String,
    pub user: String,
    pub inflight: u64,
    pub peak_inflight_10m: u64,
    pub peak_inflight_24h: u64,
    pub conc_limit: Option<u64>,
    pub rpm_limit: Option<u64>,
    pub rpm_now: u64,
    pub requests_1h: u64,
    pub requests_24h: u64,
    pub ok_24h: u64,
    pub rejected_24h: u64,
    pub errors_24h: u64,
    pub client_closed_24h: u64,
    pub prompt_tokens_est_24h: u64,
    pub completion_tokens_24h: u64,
    /// false = some of `completion_tokens_24h` was estimated by the gate
    pub completion_tokens_exact: bool,
    pub inflight_prompt_tokens_est: u64,
    pub first_seen: f64,
    pub last_seen: f64,
    pub last_status: Option<u16>,
    /// card #211 (the gate's v5.10, card #209): WHICH MACHINE this user's load is on, keyed by
    /// the gate's upstream NAME. The flat figures above are the sum across every upstream the
    /// gate fronts and must never be read as load on any one of them. Empty on a gate older than
    /// v5.10 - and on v5.10 for a user whose every request was rejected before its model resolved.
    pub by_upstream: BTreeMap<String, UpstreamLoad>,
}

/// One user's load on ONE upstream (the gate's `users[].by_upstream.<name>`, v5.10+).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamLoad {
    pub inflight: u64,
    pub requests_24h: u64,
    pub peak_inflight_10m: u64,
    pub peak_inflight_24h: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateTotals {
    pub inflight: u64,
    /// card #211 (the gate's v5.10): `inflight` split by upstream name, listing only upstreams
    /// with something in flight. An `Option`, not a bare map, on purpose: `None` = a gate older
    /// than v5.10 (it cannot split), `Some({})` = a v5.10 gate with nothing in flight anywhere.
    /// A bare map would make those two read the same.
    pub inflight_by_upstream: Option<BTreeMap<String, u64>>,
    pub users_active_10m: u64,
    pub users_24h: u64,
    pub unauthenticated_24h: u64,
}

/// A user id made safe for a metric name (`user_inflight.<id>`): anything that is not a
/// letter, digit, `.`, `_` or `-` becomes `_`, so an IPv6 address or a key with a comma can
/// never break `/series?metrics=a,b:max`.
pub fn series_id(lane: &str, user: &str) -> String {
    let clean: String = user.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(40).collect();
    format!("{}.{clean}", if lane == "trusted" { "t" } else { "p" })
}

/// card #211: an upstream NAME made safe for a metric name (`users_active.<id>`), by the same
/// rule as `series_id`. Upstream names come from the gate's own config, so they are bounded.
pub fn upstream_series_id(upstream: &str) -> String {
    upstream.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(40).collect()
}

impl GateHealth {
    /// card #211: which of the gate's upstreams is THIS machine - the one whose served models
    /// include the model our own engine serves; when that does not single one out (engine model
    /// unknown, or served by several), the gate's FIRST configured upstream, the same rule card
    /// #210 uses for whose budget it is. `None` on a gate too old to attribute load (pre-v5.10),
    /// where no per-machine figure exists to pick from.
    pub fn this_upstream(&self, engine_model: Option<&str>) -> Option<&str> {
        let ups = self.attributed_upstreams.as_ref()?;
        if let Some(m) = engine_model.filter(|m| !m.is_empty()) {
            let hits: Vec<&String> = ups.iter().filter(|u| self.upstream_loaded.get(*u).is_some_and(|l| l.iter().any(|x| x == m))).collect();
            if let [one] = hits.as_slice() {
                return Some(one.as_str());
            }
        }
        ups.first().map(String::as_str)
    }

    pub fn rejected_413(&self) -> u64 {
        self.trusted.rejected_413 + self.public.rejected_413
    }
    pub fn rejected_429(&self) -> u64 {
        self.trusted.rejected_429 + self.public.rejected_429
    }
}

#[derive(Deserialize)]
struct RawUpstream {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    loaded: Vec<String>,
}

/// card #211: `upstreams` in the gate's OWN order. A BTreeMap would sort the names, and the gate's
/// FIRST configured upstream is the one its budget belongs to (card #210) - an alphabetical order
/// would pick it right only by luck of the names.
fn ordered_upstreams<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<(String, RawUpstream)>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<(String, RawUpstream)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of upstream name -> upstream")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, RawUpstream>()? {
                out.push((k, v));
            }
            Ok(out)
        }
    }
    d.deserialize_map(V)
}

#[derive(Deserialize)]
struct RawHealth {
    #[serde(default)]
    version: String,
    #[serde(default, deserialize_with = "ordered_upstreams")]
    upstreams: Vec<(String, RawUpstream)>,
    #[serde(default)]
    admission: BTreeMap<String, LaneAdmission>,
    #[serde(default)]
    users: Option<Vec<GateUser>>,
    #[serde(default)]
    totals: Option<GateTotals>,
    #[serde(default)]
    shadow: Option<GateShadow>,
}

pub fn parse_gate_health(json: &str) -> Option<GateHealth> {
    let raw: RawHealth = serde_json::from_str(json).ok()?;
    let attributed = raw.totals.as_ref().is_some_and(|t| t.inflight_by_upstream.is_some());
    Some(GateHealth {
        attributed_upstreams: attributed.then(|| raw.upstreams.iter().map(|(k, _)| k.clone()).collect()),
        upstream_loaded: raw.upstreams.iter().map(|(k, u)| (k.clone(), u.loaded.clone())).collect(),
        version: raw.version,
        upstream_ok: !raw.upstreams.is_empty() && raw.upstreams.iter().all(|(_, u)| u.ok),
        trusted: raw.admission.get("trusted").cloned().unwrap_or_default(),
        public: raw.admission.get("public").cloned().unwrap_or_default(),
        users: raw.users,
        totals: raw.totals,
        shadow: raw.shadow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// card #279: the field is read off the shadow block, and its ABSENCE (a gate older than
    /// v5.10) parses as `None` rather than failing the whole health document.
    #[test]
    fn the_shadow_blocks_drain_estimate_is_parsed_and_its_absence_is_tolerated() {
        let with = parse_gate_health(r#"{"version":"v5.11","upstreams":{"a":{"ok":true}},"shadow":{"enabled":true,"seconds_to_drain_charged":41.5}}"#).expect("parses");
        assert_eq!(with.shadow.as_ref().unwrap().seconds_to_drain_charged, Some(41.5));
        let without = parse_gate_health(r#"{"version":"v5.9","upstreams":{"a":{"ok":true}},"shadow":{"enabled":true}}"#).expect("an older gate's health still parses");
        assert_eq!(without.shadow.as_ref().unwrap().seconds_to_drain_charged, None, "absent is unknown, not zero");
        assert!(without.shadow.as_ref().unwrap().enabled, "and the rest of the block is unaffected");
    }

    #[test]
    fn real_health_body() {
        let body = r#"{"version": "v5", "upstreams": {"gpu-box": {"ok": true, "loaded": ["model-a"]}}, "admission": {"trusted": {"admitted": 1, "rejected_413": 1, "rejected_429": 0, "client_closed": 0, "upstream_down": 0, "max_duration": 0, "inflight_tokens": 0, "waiters": 0}, "public": {"admitted": 0, "rejected_413": 0, "rejected_429": 0, "client_closed": 0, "upstream_down": 0, "max_duration": 0, "inflight_tokens": 0, "waiters": 0}}}"#;
        let h = parse_gate_health(body).unwrap();
        assert_eq!(h.version, "v5");
        assert!(h.upstream_ok);
        assert_eq!(h.trusted.admitted, 1);
        assert_eq!(h.rejected_413(), 1);
        assert_eq!(h.rejected_429(), 0);
    }

    #[test]
    fn tolerates_missing_and_extra_fields() {
        // the gate's #59 `shadow` block must not break older parsers (serde ignores it)
        let h = parse_gate_health(r#"{"version":"v6","admission":{"trusted":{"waiters":3,"new_field":1}},"shadow":{"enabled":true,"scrapes_ok":5,"scrapes_failed":0,"last_ok_age_s":1.2,"healthy":true}}"#).unwrap();
        assert_eq!(h.trusted.waiters, 3);
        assert!(!h.upstream_ok);
        assert!(parse_gate_health("<html>").is_none());
    }
}
