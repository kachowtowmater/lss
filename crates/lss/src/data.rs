//! What a detail page needs from the collector's history API (`/series`, `/hist`, `/gateway`,
//! `/rules`), and the one function that fetches it. Shared by the screen and the agents' CLI.

use lss_core::hist::LATENCY_METRICS;
use lss_core::advice::AdviceDoc;
use lss_core::bench::BenchDoc;
use lss_core::compare::LoadoutsDoc;
use lss_core::series::{parse_range, GatewayDoc, HistDoc, RulesDoc, SeriesDoc, RANGES};
use lss_core::tokens::TokensDoc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageId {
    Latency,
    Load,
    Gpus,
    Users,
    Tokens,
    Model,
    Gateway,
    Alerts,
    Incidents,
    Advice,
}

impl PageId {
    /// In key order: `1` LATENCY … `9` INCIDENTS, `a` ADVICE (card #231, the owner: "alerts and
    /// incident should have their pages" - the combined page 8 split in two, so 1-9 ran out of
    /// digits for the tenth page. `0` stays the OVERVIEW's own key (the orchestrator's correction,
    /// read from the owner: "page 0 has no tab thats why tab wasnt working" - `0` was never free
    /// to reassign), so ADVICE takes the next free key instead: `a`.
    pub const ALL: [PageId; 10] = [PageId::Latency, PageId::Load, PageId::Gpus, PageId::Users, PageId::Tokens, PageId::Model, PageId::Gateway, PageId::Alerts, PageId::Incidents, PageId::Advice];

    /// The page's position in the ring, 1-based - `10` for ADVICE, the last entry. NOT the same
    /// as the digit key that opens it (`key_digit`, below): ADVICE's key is `a`, not `10` or `0`.
    pub fn number(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0) + 1
    }

    /// The single key that opens this page directly - `1`..`9` for the first nine, `a` for
    /// ADVICE (the tenth and last; `0` is reserved for the overview, never a page).
    pub fn key_digit(self) -> char {
        match self {
            PageId::Advice => 'a',
            _ => char::from_digit(self.number() as u32, 10).unwrap_or('?'),
        }
    }

    /// The page a key opens - the inverse of `key_digit`: `'1'`..`'9'` are positions 1-9, `'a'`
    /// is ADVICE. `'0'` is deliberately absent here: it is the overview's key, handled by the
    /// caller (`ui/mod.rs`'s top-level handler), never a `PageId`.
    pub fn from_digit(c: char) -> Option<PageId> {
        match c {
            'a' => Some(PageId::Advice),
            '1'..='9' => Self::ALL.get(c.to_digit(10)? as usize - 1).copied(),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            PageId::Latency => "LATENCY",
            PageId::Load => "LOAD",
            PageId::Gpus => "GPUS",
            PageId::Users => "USERS",
            PageId::Tokens => "TOKENS",
            PageId::Model => "MODEL",
            PageId::Gateway => "GATEWAY",
            PageId::Alerts => "ALERTS",
            PageId::Incidents => "INCIDENTS",
            PageId::Advice => "ADVICE",
        }
    }

    /// The CLI word: `lss latency`, `lss load`, … NEITHER `alerts` NOR `incidents` (already the
    /// top-level one-shot reports' own words, `lss alerts`/`lss incidents` - a detail-page word
    /// that collided would silently steal them, since `main.rs` tries `PageId::from_command`
    /// before falling back to the one-shot report). ALERTS keeps `rules`, its pre-#231 word, for
    /// the same reason; INCIDENTS gets `events`, the underlying reading source's own name.
    pub fn command(self) -> &'static str {
        match self {
            PageId::Latency => "latency",
            PageId::Load => "load",
            PageId::Gpus => "gpus",
            PageId::Users => "users",
            PageId::Tokens => "tokens",
            PageId::Model => "model",
            PageId::Gateway => "gateway",
            PageId::Alerts => "rules",
            PageId::Incidents => "events",
            PageId::Advice => "advice",
        }
    }

    pub fn from_command(word: &str) -> Option<PageId> {
        Self::ALL.iter().copied().find(|p| p.command() == word)
    }

    pub fn next(self) -> PageId {
        Self::ALL[self.number() % Self::ALL.len()]
    }
}

pub const DEFAULT_RANGE: usize = 1; // "1h"

pub fn range_label(idx: usize) -> &'static str {
    RANGES[idx % RANGES.len()]
}

pub fn range_secs(idx: usize) -> i64 {
    parse_range(range_label(idx)).unwrap_or(3600)
}

pub const LOAD_TOKENS: [&str; 21] = [
    "decode_tok_s", "prompt_tok_s", "running_public", "running_trusted", "queue_public", "queue_trusted", "kv_usage", "cache_hit_rate",
    "spec_accept_length", "spec_accept_rate", "req_per_min", "c1_tok_s", "c1_invalid_tok_s", "c1_invalid",
    // reading (prefill): its speed while reading, what was read against what came from the
    // cache, the prompt tokens still in flight at the gateway, and where the request time goes
    "prefill_tok_s", "tok_prefill", "tok_cached", "inflight_public", "inflight_trusted", "sum_prefill_s", "sum_e2e_s",
];
pub const GATEWAY_TOKENS: [&str; 5] = ["inflight_public", "inflight_trusted", "waiters_public", "waiters_trusted", "gate_requests:avg"];
pub const GPU_FIELDS: [&str; 9] = ["temp_c", "power_w", "clock_mhz", "clock_mhz:min", "util_pct", "mem_used_mib", "mem_util_pct", "thr_thermal", "thr_hw"];

pub fn latency_tokens() -> Vec<String> {
    LATENCY_METRICS.iter().flat_map(|(short, _)| ["p50_ms", "p90_ms", "p99_ms", "avg_ms"].map(|s| format!("{short}_{s}"))).collect()
}

pub fn gpu_tokens(indices: &[u32]) -> Vec<String> {
    let mut t: Vec<String> = indices.iter().flat_map(|i| GPU_FIELDS.iter().map(move |f| format!("gpu{i}_{f}"))).collect();
    t.extend(indices.iter().map(|i| format!("gpu{i}_thr_power")));
    t.push("gpu_power_total_w".into());
    t
}

/// What a page's requests depend on besides the range: which GPUs there are, and which users
/// are worth a line on the USERS charts (the busiest few, by series id).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageCtx {
    pub gpus: Vec<u32>,
    pub user_ids: Vec<String>,
}

/// At most this many users get their own line on a chart.
pub const CHART_USERS: usize = 6;

impl PageCtx {
    pub fn of(status: &lss_core::model::Status) -> PageCtx {
        let mut ids: Vec<String> = status.users.rows.iter().take(CHART_USERS).map(|r| r.series_id.clone()).collect();
        ids.extend(status.users.bench.iter().map(|b| b.series_id.clone()));
        PageCtx { gpus: status.gpus.iter().map(|g| g.sample.index).collect(), user_ids: ids }
    }
}

pub fn user_tokens(ids: &[String]) -> Vec<String> {
    // the two lane series are there whatever the gateway's version: the page's fallback chart
    let mut t = vec!["users_active".to_string(), "users_inflight".to_string(), "running_public".to_string(), "running_trusted".to_string()];
    t.extend(ids.iter().flat_map(|id| [format!("user_inflight.{id}"), format!("user_rpm.{id}")]));
    t
}

/// The TOKENS page's bars: tokens per hour (per 6 hours on the 7-day view).
pub fn token_bar_step(range: &str) -> i64 {
    if parse_range(range).unwrap_or(3600) > 86_400 { 6 * 3600 } else { 3600 }
}

/// The TOKENS bars always cover at least a day: an hour of one bar says nothing.
pub fn token_bar_range(range: &str) -> &str {
    if parse_range(range).unwrap_or(3600) > 86_400 { "7d" } else { "24h" }
}

/// Everything one page shows, as fetched. A part that failed is `None` and `error` says why;
/// the page still draws (boxed) with what it has.
#[derive(Debug, Clone, Default)]
pub struct PageData {
    pub page: Option<PageId>,
    pub range_idx: usize,
    pub series: Option<SeriesDoc>,
    pub hists: Vec<HistDoc>,
    pub gateway: Option<GatewayDoc>,
    pub rules: Option<RulesDoc>,
    pub tokens: Option<TokensDoc>,
    pub loadouts: Option<LoadoutsDoc>,
    pub bench: Option<BenchDoc>,
    pub advice: Option<AdviceDoc>,
    pub error: Option<String>,
    pub fetched_at: i64,
}

impl PageData {
    pub fn is_for(&self, page: PageId, range_idx: usize) -> bool {
        self.page == Some(page) && (self.range_idx == range_idx || matches!(page, PageId::Alerts | PageId::Incidents | PageId::Model | PageId::Advice))
    }
}

fn get(base: &str, path: &str) -> Result<String, String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(Duration::from_secs(10)).build();
    match agent.get(&url).call() {
        Ok(r) => r.into_string().map_err(|e| format!("reading {path}: {e}")),
        Err(ureq::Error::Status(404, _)) => Err(format!("the collector has no {} (older than this lss): upgrade lss-collector", path.split('?').next().unwrap_or(path))),
        Err(ureq::Error::Status(code, r)) => Err(format!("{path} answered HTTP {code} {}", r.into_string().unwrap_or_default().trim())),
        Err(e) => {
            let text = e.to_string();
            Err(format!("cannot reach the collector ({})", text.rsplit(": ").next().unwrap_or(&text)))
        }
    }
}

fn get_json<T: serde::de::DeserializeOwned>(base: &str, path: &str) -> Result<T, String> {
    let body = get(base, path)?;
    serde_json::from_str(&body).map_err(|e| format!("{}: not the JSON this lss expects: {e}", path.split('?').next().unwrap_or(path)))
}

/// The raw body too, for `--json`.
pub fn fetch_raw(base: &str, path: &str) -> Result<String, String> {
    get(base, path)
}

pub fn page_paths(page: PageId, range: &str, ctx: &PageCtx) -> Vec<String> {
    let series = |tokens: Vec<String>, step: &str| format!("/series?metrics={}&range={range}&step={step}", tokens.join(","));
    let minute_step = if parse_range(range).unwrap_or(3600) <= 6 * 3600 { "60" } else { "auto" };
    match page {
        PageId::Latency => {
            let mut p = vec![series(latency_tokens(), minute_step)];
            p.extend(LATENCY_METRICS.iter().map(|(short, _)| format!("/hist?metric={short}&range={range}")));
            p
        }
        PageId::Load => vec![series(LOAD_TOKENS.iter().map(|s| s.to_string()).collect(), "auto")],
        PageId::Gpus => vec![series(gpu_tokens(&ctx.gpus), "auto")],
        PageId::Users => vec![series(user_tokens(&ctx.user_ids), "auto")],
        PageId::Tokens => vec![
            "/tokens".to_string(),
            format!("/series?metrics=tok_gen:sum,tok_prompt:sum,tok_cached:sum&range={}&step={}", token_bar_range(range), token_bar_step(range)),
            format!("/hist?metric=gen_tokens&range={}", token_bar_range(range)),
            format!("/hist?metric=prompt_tokens&range={}", token_bar_range(range)),
        ],
        PageId::Model => vec!["/loadouts".to_string(), "/bench".to_string()],
        PageId::Advice => vec!["/advice".to_string()],
        PageId::Gateway => vec![series(GATEWAY_TOKENS.iter().map(|s| s.to_string()).collect(), "auto"), format!("/gateway?range={range}")],
        // #231: ALERTS and INCIDENTS are two pages now, both still projections of the ONE /rules
        // document (rules + alert history on one, incidents on the other) - no new endpoint.
        PageId::Alerts | PageId::Incidents => vec!["/rules".to_string()],
    }
}

/// Fetches what `page` shows for the range. Never fails as a whole: see `PageData::error`.
pub fn fetch_page(base: &str, page: PageId, range_idx: usize, ctx: &PageCtx, now: i64) -> PageData {
    let mut d = PageData { page: Some(page), range_idx, fetched_at: now, ..Default::default() };
    for path in page_paths(page, range_label(range_idx), ctx) {
        if let Err(e) = d.take(&path, get(base, &path)) {
            d.error.get_or_insert(e);
        }
    }
    d
}

impl PageData {
    /// Files one fetched body under the document its path names.
    pub fn take(&mut self, path: &str, body: Result<String, String>) -> Result<(), String> {
        let body = body?;
        let what = path.split('?').next().unwrap_or(path);
        fn parse<T: serde::de::DeserializeOwned>(what: &str, body: &str) -> Result<T, String> {
            serde_json::from_str(body).map_err(|e| format!("{what}: not the JSON this lss expects: {e}"))
        }
        match what {
            "/series" => self.series = Some(parse(what, &body)?),
            "/hist" => self.hists.push(parse(what, &body)?),
            "/gateway" => self.gateway = Some(parse(what, &body)?),
            "/rules" => self.rules = Some(parse(what, &body)?),
            "/tokens" => self.tokens = Some(parse(what, &body)?),
            "/loadouts" => self.loadouts = Some(parse(what, &body)?),
            "/bench" => self.bench = Some(parse(what, &body)?),
            "/advice" => self.advice = Some(parse(what, &body)?),
            other => return Err(format!("{other}: this lss does not know that document")),
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_none() && self.hists.is_empty() && self.gateway.is_none() && self.rules.is_none() && self.tokens.is_none() && self.loadouts.is_none() && self.bench.is_none() && self.advice.is_none()
    }
}

/// One JSON document of the collector (`/loadouts`, `/bench`, …).
pub fn fetch_doc<T: serde::de::DeserializeOwned>(base: &str, path: &str) -> Result<T, String> {
    get_json(base, path)
}

/// `POST` a JSON body; returns (HTTP status, body). A refusal (409 …) is an answer, not an error.
pub fn post_json(base: &str, path: &str, body: &serde_json::Value) -> Result<(u16, String), String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(Duration::from_secs(150)).build();
    match agent.post(&url).set("Content-Type", "application/json").send_string(&body.to_string()) {
        Ok(r) => {
            let code = r.status();
            Ok((code, r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(code, r)) => Ok((code, r.into_string().unwrap_or_default())),
        Err(e) => {
            let text = e.to_string();
            Err(format!("cannot reach the collector ({})", text.rsplit(": ").next().unwrap_or(&text)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Card #231: the ring has 10 entries in order, 1-9 then ADVICE at the key `a` - `0` stays
    /// the overview's own key (the orchestrator's correction), never a `PageId`.
    #[test]
    fn pages_are_numbered_named_and_cycle() {
        assert_eq!(PageId::ALL.map(PageId::number), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(PageId::ALL.map(PageId::title), ["LATENCY", "LOAD", "GPUS", "USERS", "TOKENS", "MODEL", "GATEWAY", "ALERTS", "INCIDENTS", "ADVICE"]);
        assert_eq!(PageId::ALL.map(PageId::key_digit), ['1', '2', '3', '4', '5', '6', '7', '8', '9', 'a']);
        assert_eq!(PageId::from_digit('3'), Some(PageId::Gpus));
        assert_eq!((PageId::from_digit('4'), PageId::from_digit('9')), (Some(PageId::Users), Some(PageId::Incidents)));
        assert_eq!(PageId::from_digit('a'), Some(PageId::Advice), "a opens ADVICE, the tenth page");
        assert_eq!(PageId::from_digit('0'), None, "0 is the overview's key, not a PageId - ui/mod.rs handles it separately");
        assert_eq!(PageId::from_digit('x'), None);
        // `PageId::next()`/`.number()` still only wrap within the 10 pages themselves - the
        // overview is NOT a `PageId`, so it cannot appear here. The 11-stop ring that includes
        // the overview (0 -> 1 -> … -> ADVICE -> 0) is built in `ui/mod.rs` on top of these,
        // where `View::Overview` is a real, distinct `View` variant.
        assert_eq!(PageId::Incidents.next(), PageId::Advice);
        assert_eq!(PageId::Advice.next(), PageId::Latency);
        assert_eq!((PageId::from_command("users"), PageId::from_command("model"), PageId::from_command("advice")), (Some(PageId::Users), Some(PageId::Model), Some(PageId::Advice)));
        assert_eq!(PageId::from_command("rules"), Some(PageId::Alerts));
        assert_eq!(PageId::from_command("events"), Some(PageId::Incidents));
        // neither detail-page word may collide with the existing one-shot `lss alerts`/
        // `lss incidents` reports (main.rs tries PageId::from_command FIRST) - both must stay None.
        assert_eq!((PageId::from_command("alerts"), PageId::from_command("incidents")), (None, None));
        assert_eq!(PageId::from_command("status"), None);
    }

    #[test]
    fn requests_name_every_series_a_page_draws() {
        let lat = page_paths(PageId::Latency, "1h", &PageCtx::default());
        assert_eq!(lat.len(), 5, "one /series plus one /hist per latency metric");
        assert!(lat[0].contains("ttft_p50_ms") && lat[0].contains("queue_time_avg_ms") && lat[0].ends_with("&range=1h&step=60"), "{}", lat[0]);
        assert!(page_paths(PageId::Latency, "7d", &PageCtx::default())[0].ends_with("step=auto"));
        let gpus = page_paths(PageId::Gpus, "6h", &PageCtx { gpus: vec![0, 1, 2, 3], ..Default::default() });
        assert!(gpus[0].contains("gpu3_clock_mhz:min") && gpus[0].contains("gpu_power_total_w") && gpus[0].contains("gpu0_thr_power"));
        assert!(gpu_tokens(&[0, 1, 2, 3, 4, 5, 6, 7]).len() <= 128, "an 8-GPU box still fits one request");
        assert_eq!(page_paths(PageId::Alerts, "24h", &PageCtx::default()), vec!["/rules"]);
        assert_eq!(page_paths(PageId::Incidents, "24h", &PageCtx::default()), vec!["/rules"], "both pages read the one /rules document");
        assert_eq!(page_paths(PageId::Gateway, "24h", &PageCtx::default())[1], "/gateway?range=24h");
        let users = page_paths(PageId::Users, "6h", &PageCtx { user_ids: vec!["p.acme".into(), "t.192.0.2.76".into()], ..Default::default() });
        assert_eq!(users, vec!["/series?metrics=users_active,users_inflight,running_public,running_trusted,user_inflight.p.acme,user_rpm.p.acme,user_inflight.t.192.0.2.76,user_rpm.t.192.0.2.76&range=6h&step=auto"]);
        let tokens = page_paths(PageId::Tokens, "1h", &PageCtx::default());
        assert_eq!(tokens[0], "/tokens");
        assert!(tokens[1].contains("tok_gen:sum,tok_prompt:sum") && tokens[1].ends_with("&range=24h&step=3600"), "tokens per HOUR over a day, whatever the range key says: {}", tokens[1]);
        assert!(page_paths(PageId::Tokens, "7d", &PageCtx::default())[1].ends_with("&range=7d&step=21600"));
        assert_eq!(page_paths(PageId::Model, "1h", &PageCtx::default()), vec!["/loadouts", "/bench"]);
        assert_eq!(page_paths(PageId::Advice, "1h", &PageCtx::default()), vec!["/advice"]);
        // a document lands where its path says, and an unknown one is an error, not a panic
        let mut d = PageData::default();
        assert!(d.is_empty());
        d.take("/advice", Ok("{\"v\":1,\"windows\":[]}".into())).unwrap();
        assert!(d.advice.is_some() && !d.is_empty());
        assert!(d.take("/nope", Ok("{}".into())).is_err() && d.take("/tokens", Err("down".into())).is_err() && d.take("/tokens", Ok("<html>".into())).unwrap_err().contains("/tokens"));
    }
}
