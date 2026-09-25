//! the gateway audit log (`docker logs -t <your gateway container>`): JSON audit records interleaved with
//! aiohttp access-log lines and the occasional traceback.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AuditRecord {
    pub caller: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub model: Option<String>,
    pub verdict: String,
    pub status: u16,
    #[serde(default)]
    pub ip: Option<String>,
    /// the gate's prompt-size estimate; today it only logs it on the requests it rejects
    #[serde(default)]
    pub estimated_tokens: Option<f64>,
    /// a gateway publishing /gate/health v5.2+: on EVERY line - the prompt estimate, what was generated, whether
    /// that count is exact, the request's duration and its time to the first upstream byte
    #[serde(default)]
    pub est_tokens: Option<f64>,
    #[serde(default)]
    pub completion_tokens: Option<f64>,
    #[serde(default)]
    pub completion_exact: Option<bool>,
    #[serde(default)]
    pub dur_ms: Option<f64>,
    #[serde(default)]
    pub ttfb_ms: Option<f64>,
    /// a gateway publishing /gate/health v5.2+: true for `lss bench`'s own requests (filed under `lss-bench`)
    #[serde(default)]
    pub bench: Option<bool>,
}

/// Prompt-size classes for "how long until the first token, by how much was sent".
pub const SIZE_BUCKETS: [(&str, f64); 5] = [("<1k", 1_000.0), ("1-8k", 8_000.0), ("8-32k", 32_000.0), ("32-128k", 128_000.0), ("128k+", f64::INFINITY)];

pub fn size_bucket(prompt_tokens: f64) -> &'static str {
    SIZE_BUCKETS.iter().find(|(_, upper)| prompt_tokens < *upper).map_or("128k+", |(name, _)| name)
}

/// Count / sum / max of a duration in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeAgg {
    pub n: u64,
    pub sum_ms: f64,
    pub max_ms: f64,
    /// how many took up to each of `TIME_EDGES_MS` (not cumulative; slower ones are only in
    /// `n`): enough to say "what share got its first word within N seconds" (added 2026-09-20)
    #[serde(skip_serializing_if = "no_counts")]
    pub le: [u64; 8],
    /// how many of `n` were counted into `le` (requests logged before the edges existed are in
    /// `n` only): the denominator of "what share was within N seconds"
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub le_n: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Upper edges of `TimeAgg::le`, milliseconds.
pub const TIME_EDGES_MS: [f64; 8] = [1_000.0, 2_000.0, 3_000.0, 5_000.0, 10_000.0, 20_000.0, 30_000.0, 60_000.0];

fn no_counts(le: &[u64; 8]) -> bool {
    le.iter().all(|c| *c == 0)
}

impl TimeAgg {
    pub fn add(&mut self, ms: f64) {
        self.n += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        self.le_n += 1;
        if let Some(i) = TIME_EDGES_MS.iter().position(|edge| ms <= *edge) {
            self.le[i] += 1;
        }
    }
    pub fn merge(&mut self, o: &TimeAgg) {
        self.n += o.n;
        self.sum_ms += o.sum_ms;
        self.max_ms = self.max_ms.max(o.max_ms);
        for (mine, theirs) in self.le.iter_mut().zip(o.le.iter()) {
            *mine += theirs;
        }
        self.le_n += o.le_n;
    }
    /// How many took at most `ms`, counted at the largest edge that is not above it (so the
    /// answer never flatters). None = recorded before the edges existed.
    /// (requests counted with edges, how many of them took at most `ms`).
    pub fn within(&self, ms: f64) -> (u64, u64) {
        (self.le_n, TIME_EDGES_MS.iter().zip(self.le.iter()).filter(|(edge, _)| **edge <= ms).map(|(_, c)| *c).sum())
    }
    pub fn avg_ms(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum_ms / self.n as f64)
    }
}

/// Upper edges of the per-user time-to-first-word buckets, milliseconds (the last is open).
pub const EXP_TTFB_MS: [f64; 11] = [50.0, 100.0, 200.0, 400.0, 800.0, 1_500.0, 3_000.0, 6_000.0, 12_000.0, 30_000.0, 60_000.0];
/// Upper edges of the per-user writing-speed buckets, tokens per second (the last is open).
pub const EXP_TOK_S: [f64; 11] = [5.0, 10.0, 20.0, 30.0, 40.0, 60.0, 80.0, 100.0, 150.0, 200.0, 300.0];
/// A request needs this many generated tokens and this long a writing phase to have a speed.
const EXP_MIN_TOKENS: f64 = 16.0;
const EXP_MIN_WRITE_MS: f64 = 200.0;
/// users kept per log delta (a key-spraying client cannot grow a stored sample)
const EXP_MAX_USERS: usize = 64;

/// WHAT ONE USER EXPERIENCED (a gateway publishing /gate/health v5.2+ logs it per request): how long until the
/// first word, and how fast the answer was then written. Two small histograms, so a day of
/// them merges into honest percentiles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserExp {
    /// answered requests with a time to first word
    pub n: u64,
    /// `EXP_TTFB_MS.len() + 1` counts
    pub ttfb: Vec<u64>,
    /// requests long enough to have a writing speed, their token and second sums, and
    /// `EXP_TOK_S.len() + 1` counts
    pub speed_n: u64,
    pub tokens: f64,
    pub write_secs: f64,
    pub speed: Vec<u64>,
}

fn bucket_add(counts: &mut Vec<u64>, edges: &[f64], v: f64) {
    if counts.len() != edges.len() + 1 {
        counts.resize(edges.len() + 1, 0);
    }
    let i = edges.iter().position(|e| v <= *e).unwrap_or(edges.len());
    counts[i] += 1;
}

fn bucket_quantile(counts: &[u64], edges: &[f64], q: f64) -> Option<f64> {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = q.clamp(0.0, 1.0) * total as f64;
    let mut seen = 0.0;
    for (i, c) in counts.iter().enumerate() {
        let c = *c as f64;
        if c > 0.0 && seen + c >= rank {
            let lower = if i == 0 { 0.0 } else { edges[i - 1] };
            // the open last bucket has no upper edge: report its lower edge ("at least")
            let Some(upper) = edges.get(i).copied() else { return Some(lower) };
            return Some(lower + (upper - lower) * ((rank - seen) / c).clamp(0.0, 1.0));
        }
        seen += c;
    }
    edges.last().copied()
}

impl UserExp {
    pub fn add(&mut self, ttfb_ms: f64, dur_ms: Option<f64>, completion_tokens: Option<f64>) {
        self.n += 1;
        bucket_add(&mut self.ttfb, &EXP_TTFB_MS, ttfb_ms);
        if let (Some(dur), Some(tokens)) = (dur_ms, completion_tokens) {
            let write_ms = dur - ttfb_ms;
            if tokens >= EXP_MIN_TOKENS && write_ms >= EXP_MIN_WRITE_MS {
                self.speed_n += 1;
                self.tokens += tokens;
                self.write_secs += write_ms / 1000.0;
                bucket_add(&mut self.speed, &EXP_TOK_S, tokens / (write_ms / 1000.0));
            }
        }
    }

    pub fn merge(&mut self, o: &UserExp) {
        self.n += o.n;
        self.speed_n += o.speed_n;
        self.tokens += o.tokens;
        self.write_secs += o.write_secs;
        for (mine, theirs) in [(&mut self.ttfb, &o.ttfb), (&mut self.speed, &o.speed)] {
            if mine.len() < theirs.len() {
                mine.resize(theirs.len(), 0);
            }
            for (m, t) in mine.iter_mut().zip(theirs) {
                *m += t;
            }
        }
    }

    pub fn ttfb_quantile_ms(&self, q: f64) -> Option<f64> {
        bucket_quantile(&self.ttfb, &EXP_TTFB_MS, q)
    }

    pub fn speed_quantile(&self, q: f64) -> Option<f64> {
        bucket_quantile(&self.speed, &EXP_TOK_S, q)
    }

    /// all their generated tokens over all their writing seconds
    pub fn speed_avg(&self) -> Option<f64> {
        (self.write_secs > 0.0).then(|| self.tokens / self.write_secs)
    }
}

/// Client address with the host part masked: `203.0.113.x`, `2001:db8:1::x`. The full
/// address never reaches the collector's database or its API.
pub fn mask_ip(ip: &str) -> String {
    let ip = ip.trim();
    if let Ok(v4) = ip.parse::<std::net::Ipv4Addr>() {
        let o = v4.octets();
        return format!("{}.{}.{}.x", o[0], o[1], o[2]);
    }
    if let Ok(v6) = ip.parse::<std::net::Ipv6Addr>() {
        let s = v6.segments();
        return format!("{:x}:{:x}:{:x}::x", s[0], s[1], s[2]);
    }
    "(unknown)".into()
}

/// Count / sum / max of the gate's `estimated_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstTokens {
    pub n: u64,
    pub sum: f64,
    pub max: f64,
}

impl EstTokens {
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn add(&mut self, v: f64) {
        self.n += 1;
        self.sum += v;
        self.max = self.max.max(v);
    }
    pub fn merge(&mut self, o: &EstTokens) {
        self.n += o.n;
        self.sum += o.sum;
        self.max = self.max.max(o.max);
    }
    pub fn avg(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
}

/// Splits docker's `-t` prefix off a log line: (`2026-09-19T11:41:48.070064373Z`, rest).
/// docker pads the fraction to nine digits, so the stamps order lexicographically.
pub fn split_docker_ts(line: &str) -> Option<(&str, &str)> {
    let (ts, rest) = line.split_once(' ')?;
    let b = ts.as_bytes();
    (ts.len() >= 20 && b[4] == b'-' && b[10] == b'T' && ts.ends_with('Z')).then_some((ts, rest))
}

/// An audit record, or None for access-log lines, startup events, tracebacks, truncated JSON.
pub fn parse_audit(rest: &str) -> Option<AuditRecord> {
    let rest = rest.trim();
    if !rest.starts_with('{') {
        return None;
    }
    serde_json::from_str(rest).ok()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneLog {
    /// inference requests (POST). Health checks and `/v1/models` GETs are in `other`.
    pub requests: u64,
    pub other: u64,
    pub by_status: BTreeMap<u16, u64>,
    pub by_verdict: BTreeMap<String, u64>,
    pub by_key: BTreeMap<String, u64>,
    /// key -> HTTP status -> count (added 2026-09-19; absent from older stored samples)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_key_status: BTreeMap<String, BTreeMap<u16, u64>>,
    #[serde(skip_serializing_if = "EstTokens::is_empty")]
    pub est: EstTokens,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_key_est: BTreeMap<String, EstTokens>,
    /// masked client address -> count
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_ip: BTreeMap<String, u64>,
    /// a gateway publishing /gate/health v5.2+: time to the first upstream byte of the answered (2xx) requests, by
    /// prompt-size class (`SIZE_BUCKETS`)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ttfb_by_size: BTreeMap<String, TimeAgg>,
    /// a gateway publishing /gate/health v5.2+: completion tokens of the answered requests, and how many of them the
    /// gate had to estimate
    #[serde(skip_serializing_if = "is_zero")]
    pub completion_tokens: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub completion_estimated: f64,
    /// a gateway publishing /gate/health v5.2+: the largest prompt (estimated tokens) among the ANSWERED requests
    #[serde(skip_serializing_if = "is_zero")]
    pub prompt_max: f64,
    /// a gateway publishing /gate/health v5.2+: each user's own experience, keyed by what the gate calls them (the
    /// key's name on the public lane, the client address on the trusted one, `lss-bench`)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_user: BTreeMap<String, UserExp>,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

impl LaneLog {
    pub fn is_empty(&self) -> bool {
        self.requests == 0 && self.other == 0
    }

    pub fn merge(&mut self, o: &LaneLog) {
        self.requests += o.requests;
        self.other += o.other;
        for (k, v) in &o.by_status {
            *self.by_status.entry(*k).or_default() += v;
        }
        for (k, v) in &o.by_verdict {
            *self.by_verdict.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.by_key {
            *self.by_key.entry(k.clone()).or_default() += v;
        }
        for (k, codes) in &o.by_key_status {
            let mine = self.by_key_status.entry(k.clone()).or_default();
            for (code, n) in codes {
                *mine.entry(*code).or_default() += n;
            }
        }
        self.est.merge(&o.est);
        for (k, e) in &o.by_key_est {
            self.by_key_est.entry(k.clone()).or_default().merge(e);
        }
        for (k, v) in &o.by_ip {
            *self.by_ip.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.ttfb_by_size {
            self.ttfb_by_size.entry(k.clone()).or_default().merge(v);
        }
        self.completion_tokens += o.completion_tokens;
        self.completion_estimated += o.completion_estimated;
        self.prompt_max = self.prompt_max.max(o.prompt_max);
        for (k, v) in &o.by_user {
            self.by_user.entry(k.clone()).or_default().merge(v);
        }
    }

    pub fn status_count(&self, code: u16) -> u64 {
        self.by_status.get(&code).copied().unwrap_or(0)
    }

    /// (2xx, 4xx, 5xx)
    pub fn classes(&self) -> (u64, u64, u64) {
        let mut c = (0, 0, 0);
        for (status, n) in &self.by_status {
            match status / 100 {
                2 => c.0 += n,
                4 => c.1 += n,
                5 => c.2 += n,
                _ => {}
            }
        }
        c
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogDelta {
    #[serde(skip_serializing_if = "LaneLog::is_empty")]
    pub public: LaneLog,
    #[serde(skip_serializing_if = "LaneLog::is_empty")]
    pub trusted: LaneLog,
}

impl LogDelta {
    pub fn is_empty(&self) -> bool {
        self.public.is_empty() && self.trusted.is_empty()
    }

    pub fn add(&mut self, r: &AuditRecord) {
        let lane = if r.caller == "public" { &mut self.public } else { &mut self.trusted };
        if r.method != "POST" {
            lane.other += 1;
            return;
        }
        lane.requests += 1;
        *lane.by_status.entry(r.status).or_default() += 1;
        *lane.by_verdict.entry(r.verdict.clone()).or_default() += 1;
        let key = r.key.clone().unwrap_or_else(|| "(no key)".to_string());
        *lane.by_key.entry(key.clone()).or_default() += 1;
        *lane.by_key_status.entry(key.clone()).or_default().entry(r.status).or_default() += 1;
        if let Some(est) = r.estimated_tokens.filter(|v| v.is_finite() && *v >= 0.0) {
            lane.est.add(est);
            lane.by_key_est.entry(key).or_default().add(est);
        }
        if let Some(ip) = &r.ip {
            *lane.by_ip.entry(mask_ip(ip)).or_default() += 1;
        }
        let answered = (200..300).contains(&r.status);
        if let (true, Some(prompt)) = (answered, r.est_tokens.filter(|v| v.is_finite() && *v >= 0.0)) {
            lane.prompt_max = lane.prompt_max.max(prompt);
        }
        if let (true, Some(prompt), Some(ttfb)) = (answered, r.est_tokens.filter(|v| v.is_finite() && *v >= 0.0), r.ttfb_ms.filter(|v| v.is_finite() && *v >= 0.0)) {
            lane.ttfb_by_size.entry(size_bucket(prompt).to_string()).or_default().add(ttfb);
        }
        if let (true, Some(ttfb)) = (answered, r.ttfb_ms.filter(|v| v.is_finite() && *v >= 0.0)) {
            // who: the gate's own naming of its users. Card #57: a keyed TRUSTED request is
            // the key's name (the harness), like public; keyless trusted stays the IP.
            let who = if r.bench == Some(true) {
                Some(crate::users::BENCH_USER.to_string())
            } else if r.key.is_some() {
                r.key.clone()
            } else {
                r.ip.clone()
            };
            if let Some(who) = who.filter(|w| !w.is_empty()) {
                if lane.by_user.len() < EXP_MAX_USERS || lane.by_user.contains_key(&who) {
                    lane.by_user.entry(who).or_default().add(ttfb, r.dur_ms.filter(|v| v.is_finite()), r.completion_tokens.filter(|v| v.is_finite()));
                }
            }
        }
        if let (true, Some(out)) = (answered, r.completion_tokens.filter(|v| v.is_finite() && *v >= 0.0)) {
            lane.completion_tokens += out;
            if r.completion_exact == Some(false) {
                lane.completion_estimated += out;
            }
        }
    }

    pub fn merge(&mut self, o: &LogDelta) {
        self.public.merge(&o.public);
        self.trusted.merge(&o.trusted);
    }
}

/// Consumes `docker logs -t` output, skipping everything at or before `after_ts` (the last
/// stamp already counted). Returns the delta and the newest stamp seen.
pub fn ingest(text: &str, after_ts: Option<&str>) -> (LogDelta, Option<String>) {
    let mut delta = LogDelta::default();
    let mut newest: Option<String> = after_ts.map(str::to_string);
    for line in text.lines() {
        let Some((ts, rest)) = split_docker_ts(line) else { continue };
        if after_ts.is_some_and(|a| ts <= a) {
            continue;
        }
        if newest.as_deref().is_none_or(|n| ts > n) {
            newest = Some(ts.to_string());
        }
        if let Some(rec) = parse_audit(rest) {
            delta.add(&rec);
        }
    }
    (delta, newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = include_str!("../../../fixtures/gate_log.txt");

    #[test]
    fn fixture_counts_by_lane_status_verdict_key() {
        let (d, newest) = ingest(LOG, None);
        assert_eq!(newest.as_deref(), Some("2026-09-19T11:59:02.000000001Z"));
        // real head of the file: 2 trusted POSTs (200 + 413) and one GET /v1/models
        // synthetic body: 2 x (12 public + 8 trusted) POSTs
        assert_eq!(d.public.requests, 24);
        assert_eq!(d.trusted.requests, 16 + 2);
        assert_eq!(d.trusted.other, 1);
        assert_eq!(d.public.classes(), (14, 10, 0));
        assert_eq!(d.trusted.classes(), (9, 3, 6));
        assert_eq!(d.public.by_key.get("key-a"), Some(&14));
        assert_eq!(d.public.by_key.get("key-b"), Some(&8));
        assert_eq!(d.public.by_key.get("canary"), Some(&2));
        assert_eq!(d.trusted.by_key.get("(no key)"), Some(&18));
        assert_eq!(d.trusted.by_verdict.get("reject-prompt-tokens"), Some(&1));
        assert_eq!(d.trusted.by_verdict.get("upstream-down"), Some(&2));
        assert_eq!(d.public.by_status.get(&499), Some(&2));
    }

    #[test]
    fn per_key_status_estimated_tokens_and_masked_addresses() {
        let text = concat!(
            "2026-09-19T12:00:00.000000001Z {\"t\": \"x\", \"caller\": \"public\", \"key\": \"acme\", \"ip\": \"203.0.113.37\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"pass\", \"status\": 200}\n",
            "2026-09-19T12:00:01.000000001Z {\"t\": \"x\", \"caller\": \"public\", \"key\": \"acme\", \"ip\": \"203.0.113.99\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"reject-prompt-tokens\", \"status\": 413, \"estimated_tokens\": 250000}\n",
            "2026-09-19T12:00:02.000000001Z {\"t\": \"x\", \"caller\": \"public\", \"key\": \"acme\", \"ip\": \"2001:db8:1:2::9\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"reject-budget\", \"status\": 429, \"estimated_tokens\": 50000}\n",
        );
        let (d, _) = ingest(text, None);
        let codes = &d.public.by_key_status["acme"];
        assert_eq!((codes[&200], codes[&413], codes[&429]), (1, 1, 1));
        assert_eq!(d.public.est, EstTokens { n: 2, sum: 300_000.0, max: 250_000.0 });
        assert_eq!(d.public.by_key_est["acme"].avg(), Some(150_000.0));
        assert_eq!(d.public.by_ip.get("203.0.113.x"), Some(&2), "the last octet is masked at ingestion");
        assert_eq!(d.public.by_ip.get("2001:db8:1::x"), Some(&1));
        assert!(!serde_json::to_string(&d).unwrap().contains("137.37"), "no full address is ever stored");
        assert_eq!(mask_ip("not an ip"), "(unknown)");
        // an older stored sample without the new maps still loads, and merges
        let old: LaneLog = serde_json::from_str("{\"requests\":2,\"by_status\":{\"200\":2},\"by_key\":{\"acme\":2}}").unwrap();
        let mut sum = d.public.clone();
        sum.merge(&old);
        assert_eq!((sum.requests, sum.status_count(200), sum.by_key["acme"]), (5, 3, 5));
    }

    #[test]
    fn v52_lines_give_ttft_by_prompt_size_and_the_older_log_still_parses() {
        let (d, _) = ingest(include_str!("../../../fixtures/gate_log_v52.txt"), None);
        assert_eq!((d.public.requests, d.trusted.requests, d.trusted.other), (4, 2, 1));
        let t = |lane: &LaneLog, b: &str| lane.ttfb_by_size.get(b).copied().unwrap_or_default();
        assert_eq!((t(&d.public, "<1k").n, t(&d.public, "<1k").sum_ms, t(&d.public, "<1k").max_ms), (1, 190.0, 190.0));
        assert_eq!((t(&d.public, "1-8k").n, t(&d.public, "1-8k").sum_ms, t(&d.public, "1-8k").max_ms), (2, 1600.0, 990.0));
        // and how many got their first word within N seconds (for the owner's first-word target)
        assert_eq!((t(&d.public, "1-8k").within(5_000.0), t(&d.public, "1-8k").within(500.0)), ((2, 2), (2, 0)));
        // requests logged before the edges existed are never in the denominator
        let mut old = TimeAgg { n: 40, sum_ms: 80_000.0, max_ms: 9_000.0, ..Default::default() };
        old.merge(&t(&d.public, "1-8k"));
        assert_eq!((old.n, old.within(5_000.0)), (42, (2, 2)));
        assert_eq!(t(&d.public, "1-8k").avg_ms(), Some(800.0));
        assert_eq!((t(&d.trusted, "32-128k").n, t(&d.trusted, "128k+").max_ms), (1, 71_000.0));
        assert!(!d.public.ttfb_by_size.contains_key("32-128k"), "a rejected request has no first token");
        assert_eq!((d.public.completion_tokens, d.public.completion_estimated), (1422.0, 410.0));
        assert_eq!(d.public.est.n, 1, "`estimated_tokens` (reject-only) keeps its meaning");
        assert_eq!([0.0, 999.0, 1000.0, 7999.0, 8000.0, 127_999.0, 128_000.0, 9e9].map(size_bucket), ["<1k", "<1k", "1-8k", "1-8k", "8-32k", "32-128k", "128k+", "128k+"]);
        // the v5.1 log has none of the new fields: nothing appears, nothing breaks, nothing is stored
        let (old, _) = ingest(LOG, None);
        assert!(old.public.ttfb_by_size.is_empty() && old.public.completion_tokens == 0.0);
        assert!(!serde_json::to_string(&old).unwrap().contains("ttfb"));
    }

    #[test]
    fn resumes_strictly_after_the_last_stamp() {
        let (all, _) = ingest(LOG, None);
        let cut = "2026-09-19T11:51:00.100000000Z";
        let (tail, newest) = ingest(LOG, Some(cut));
        assert!(tail.public.requests + tail.trusted.requests < all.public.requests + all.trusted.requests);
        let (none, n2) = ingest(LOG, newest.as_deref());
        assert!(none.is_empty(), "re-reading the same window must count nothing twice");
        assert_eq!(n2, newest);
    }

    #[test]
    fn keyed_trusted_requests_are_the_key_not_the_ip() {
        // card #57: a keyed TRUSTED line buckets per-key like public does, and the per-user
        // experience identity is the KEY name (the harness), not the shared client IP;
        // the keyless trusted line keeps the IP identity exactly as before. card #307: a neutral
        // key name - it used to be a real one assembled from pieces, which shipped it anyway.
        let who = "harness-agents".to_string();
        let text = format!(
            concat!(
                "2026-09-22T05:00:00.000000001Z {{\"t\": \"x\", \"caller\": \"trusted\", \"key\": \"{}\", \"ip\": \"192.0.2.77\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"pass\", \"status\": 200, \"est_tokens\": 1000, \"ttfb_ms\": 300}}\n",
                "2026-09-22T05:00:01.000000001Z {{\"t\": \"x\", \"caller\": \"trusted\", \"key\": \"{}\", \"ip\": \"192.0.2.77\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"pass\", \"status\": 200, \"est_tokens\": 2000, \"ttfb_ms\": 500}}\n",
                "2026-09-22T05:00:02.000000001Z {{\"t\": \"x\", \"caller\": \"trusted\", \"ip\": \"192.0.2.77\", \"method\": \"POST\", \"path\": \"/v1/chat/completions\", \"model\": \"m\", \"verdict\": \"pass\", \"status\": 200, \"est_tokens\": 500, \"ttfb_ms\": 200}}\n",
            ),
            who, who,
        );
        let (d, _) = ingest(&text, None);
        assert_eq!(d.trusted.by_key.get(&who), Some(&2));
        assert_eq!(d.trusted.by_key.get("(no key)"), Some(&1));
        // by_key_est reads the reject-era `estimated_tokens` field, which pass lines do not
        // carry - per-key estimates of ANSWERED requests come from by_user (below), not here.
        assert!(d.trusted.by_user.contains_key(&who), "keyed identity");
        assert!(d.trusted.by_user.contains_key("192.0.2.77"), "keyless keeps the IP identity");
    }

    #[test]
    fn malformed_lines_are_skipped() {
        assert!(parse_audit("127.0.0.1 [19/Sep/2026] \"GET / HTTP/1.1\" 200").is_none());
        assert!(parse_audit("{\"event\": \"started\"}").is_none());
        assert!(parse_audit("{\"t\": \"x\", \"caller\": \"public\", \"verdict\": \"pass\", \"status\": 200").is_none());
        assert!(split_docker_ts("Traceback (most recent call last):").is_none());
    }
}
