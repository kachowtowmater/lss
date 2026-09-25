//! HTTP: /status, /metrics, /health, /rules - and POST /test-alert, from this machine only.
//! Those GET bodies are pre-rendered by the poll loop; a request only copies a string. The
//! history endpoints (/series, /hist, /gateway) read the database through a read-only
//! connection owned by the listener thread, and their answers are cached for a moment, so a
//! client can never slow the collector down.

use crate::db::{Db, Retention};
use crate::{query, Shared};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the history endpoints read from.
#[derive(Clone)]
pub struct HistorySource {
    pub db_path: String,
    pub keep: Retention,
    /// the masked address the collector's own probe has in the gate log (see `query::probe_ip_of`)
    pub probe_ip: Option<String>,
}

/// Identical history requests inside this window share one answer (several screens open on
/// the same page cost one query).
const CACHE_TTL: Duration = Duration::from_secs(2);
const CACHE_MAX: usize = 64;
use tiny_http::{Header, Method, Response, Server};

/// The poll loop is considered wedged when its newest sample is older than this.
const STALE_SECS: i64 = 30;
const TEST_ALERT_MAX_CHARS: usize = 300;
const TEST_ALERT_MAX_PENDING: usize = 5;

pub fn serve_forever(addr: String, shared: Arc<Mutex<Shared>>, source: HistorySource, bench: Arc<crate::bench_run::BenchCtx>) {
    let mut warned = false;
    let mut reader: Option<Db> = None;
    let mut cache: HashMap<String, (Instant, (u16, String))> = HashMap::new();
    loop {
        match Server::http(&addr) {
            Ok(server) => {
                eprintln!("http: listening on {addr}");
                // decided from the address actually bound, not from the config string
                let listener_is_loopback = server.server_addr().to_ip().is_some_and(|a| a.ip().is_loopback());
                for mut req in server.incoming_requests() {
                    let path = req.url().split('?').next().unwrap_or("").to_string();
                    let (code, ctype, body) = if path == "/test-alert" {
                        let mut text = String::new();
                        let _ = req.as_reader().take(4096).read_to_string(&mut text);
                        test_alert(listener_is_loopback, req.remote_addr().copied(), req.method() == &Method::Post, &text, &shared)
                    } else if req.method() == &Method::Post && path.starts_with("/bench") {
                        let mut text = String::new();
                        let _ = req.as_reader().take(8192).read_to_string(&mut text);
                        let (code, body) = bench_post(&path, listener_is_loopback, req.remote_addr().copied(), &text, &bench);
                        (code, "application/json", body)
                    } else if req.method() == &Method::Post && path.starts_with("/maintenance") {
                        let mut text = String::new();
                        let _ = req.as_reader().take(4096).read_to_string(&mut text);
                        let (code, body) = maintenance_post(&path, listener_is_loopback, req.remote_addr().copied(), &text, &shared);
                        (code, "application/json", body)
                    } else if (req.method() == &Method::Get || req.method() == &Method::Head) && is_history(&path) {
                        let url = req.url().to_string();
                        let (code, body) = match cache.get(&url).filter(|(at, _)| at.elapsed() < CACHE_TTL) {
                            Some((_, hit)) => hit.clone(),
                            None => {
                                let fresh = history(&path, &url, &mut reader, &source, &shared);
                                if cache.len() >= CACHE_MAX {
                                    cache.clear();
                                }
                                cache.insert(url, (Instant::now(), fresh.clone()));
                                fresh
                            }
                        };
                        (code, "application/json", body)
                    } else if req.method() == &Method::Get || req.method() == &Method::Head {
                        route(&path, &shared)
                    } else {
                        (405, "text/plain", "method not allowed\n".into())
                    };
                    let header = Header::from_bytes("Content-Type", ctype).expect("static header");
                    let _ = req.respond(Response::from_string(body).with_status_code(code).with_header(header));
                }
            }
            Err(e) => {
                // e.g. the tailnet address is not up yet at boot: keep the other listener, retry this one
                if !warned {
                    eprintln!("http: cannot bind {addr}: {e} - retrying every 30 s");
                    warned = true;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn is_history(path: &str) -> bool {
    matches!(path, "/series" | "/hist" | "/gateway")
}

fn history(path: &str, url: &str, reader: &mut Option<Db>, source: &HistorySource, shared: &Arc<Mutex<Shared>>) -> (u16, String) {
    if reader.is_none() {
        match Db::open_readonly(&source.db_path) {
            Ok(db) => *reader = Some(db),
            Err(e) => return (503, format!("{{\"v\":1,\"error\":\"database not readable: {}\"}}\n", e.to_string().replace('"', "'"))),
        }
    }
    let Some(db) = reader.as_ref() else { return (503, "{\"v\":1,\"error\":\"no database\"}\n".into()) };
    let q = query::parse_query(url);
    let now = crate::unix_now();
    match path {
        "/series" => query::series(db, now, &q, source.keep),
        "/hist" => query::hist(db, now, &q, source.keep),
        _ => {
            let open = shared.lock().unwrap_or_else(|e| e.into_inner()).gate_open.clone();
            query::gateway(db, now, &q, source.keep, open.as_ref(), source.probe_ip.as_deref())
        }
    }
}

/// `POST /test-alert` (body = the message): queues a synthetic `info` alert for the poll loop,
/// which sends it down the real road - rule engine, `alerts` row, alert_cmd. It is the only
/// request that makes the collector DO something, so it is refused unless it arrived on a
/// loopback listener AND from a loopback peer: the tailnet listener can never trigger it.
pub fn test_alert(listener_is_loopback: bool, remote: Option<SocketAddr>, is_post: bool, body: &str, shared: &Arc<Mutex<Shared>>) -> (u16, &'static str, String) {
    if !listener_is_loopback || !remote.is_some_and(|r| r.ip().is_loopback()) {
        return (403, "application/json", "{\"error\":\"test-alert is only accepted on 127.0.0.1, from this machine\"}\n".into());
    }
    if !is_post {
        return (405, "application/json", "{\"error\":\"POST the message as the request body\"}\n".into());
    }
    let msg: String = body.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().chars().take(TEST_ALERT_MAX_CHARS).collect();
    if msg.is_empty() {
        return (400, "application/json", "{\"error\":\"empty message\"}\n".into());
    }
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    if s.test_alerts.len() >= TEST_ALERT_MAX_PENDING {
        return (429, "application/json", "{\"error\":\"too many test alerts pending\"}\n".into());
    }
    s.test_alerts.push(msg.clone());
    (202, "application/json", format!("{}\n", serde_json::json!({"queued": true, "rule": lss_core::rules::RULE_TEST_ALERT, "message": format!("[TEST] {msg}")})))
}

/// `POST /bench` (start), `/bench/cancel`, `/bench/compare-accuracy`: the requests that make the
/// collector load the model server, so, like `/test-alert`, they are only accepted on a loopback
/// listener from a loopback peer. From another machine `lss bench` goes through ssh.
pub fn bench_post(path: &str, listener_is_loopback: bool, remote: Option<SocketAddr>, body: &str, bench: &Arc<crate::bench_run::BenchCtx>) -> (u16, String) {
    let json = |code: u16, v: serde_json::Value| (code, format!("{v}\n"));
    if !listener_is_loopback || !remote.is_some_and(|r| r.ip().is_loopback()) {
        return json(403, serde_json::json!({"v": 1, "ok": false, "message": "a benchmark can only be started on the GPU box itself (127.0.0.1): run `lss bench` there, or over ssh"}));
    }
    match path {
        "/bench" => {
            let req: lss_core::bench::BenchRequest = match serde_json::from_str(if body.trim().is_empty() { "{}" } else { body }) {
                Ok(r) => r,
                Err(e) => return json(400, serde_json::json!({"v": 1, "ok": false, "message": format!("the request body is not the JSON `lss bench` sends: {e}")})),
            };
            let (code, started) = crate::bench_run::request_start(bench, &req);
            json(code, serde_json::to_value(started).unwrap_or_default())
        }
        "/bench/cancel" => {
            let (code, r) = crate::bench_run::request_cancel(bench);
            json(code, serde_json::to_value(r).unwrap_or_default())
        }
        "/bench/compare-accuracy" => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            crate::bench_run::compare_accuracy(bench, v["a"].as_str().unwrap_or(""), v["b"].as_str().unwrap_or(""))
        }
        _ => json(404, serde_json::json!({"v": 1, "ok": false, "message": "not found"})),
    }
}

/// `POST /maintenance/start` (body `{"reason":..., "minutes":...}`), `/maintenance/stop`: like
/// `/bench`, these change how the collector alerts, so - same rule as `/test-alert` and `/bench`
/// - they are only accepted on a loopback listener from a loopback peer (#22, 2026-09-21).
pub fn maintenance_post(path: &str, listener_is_loopback: bool, remote: Option<SocketAddr>, body: &str, shared: &Arc<Mutex<Shared>>) -> (u16, String) {
    let json = |code: u16, v: serde_json::Value| (code, format!("{v}\n"));
    if !listener_is_loopback || !remote.is_some_and(|r| r.ip().is_loopback()) {
        return json(403, serde_json::json!({"ok": false, "message": "maintenance mode can only be changed on the GPU box itself (127.0.0.1): run `lss maintenance` there, or over ssh"}));
    }
    let now = crate::unix_now();
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    match path {
        "/maintenance/start" => {
            let req: lss_core::maintenance::MaintenanceRequest = match serde_json::from_str(if body.trim().is_empty() { "{}" } else { body }) {
                Ok(r) => r,
                Err(e) => return json(400, serde_json::json!({"ok": false, "message": format!("the request body is not the JSON `lss maintenance start` sends: {e}")})),
            };
            let reply = lss_core::maintenance::start(&mut s.maintenance, now, &req.reason, req.minutes);
            json(if reply.ok { 200 } else { 400 }, serde_json::to_value(reply).unwrap_or_default())
        }
        "/maintenance/stop" => match lss_core::maintenance::stop(&mut s.maintenance, now) {
            Some(reply) => json(200, serde_json::to_value(reply).unwrap_or_default()),
            None => json(200, serde_json::json!({"ok": false, "message": "no maintenance window is open", "expires_at": 0})),
        },
        _ => json(404, serde_json::json!({"ok": false, "message": "not found"})),
    }
}

pub fn route(url: &str, shared: &Arc<Mutex<Shared>>) -> (u16, &'static str, String) {
    let path = url.split('?').next().unwrap_or("");
    let s = shared.lock().unwrap_or_else(|e| e.into_inner());
    match path {
        "/status" => (200, "application/json", s.status_json.clone()),
        "/metrics" => (200, "text/plain; version=0.0.4", s.metrics_text.clone()),
        "/rules" => (200, "application/json", s.rules_json.clone()),
        "/loadouts" => (200, "application/json", s.loadouts_json.clone()),
        "/tokens" => (200, "application/json", s.tokens_json.clone()),
        "/advice" => (200, "application/json", s.advice_json.clone()),
        "/bench" => (200, "application/json", s.bench_json.clone()),
        "/health" => {
            let age = crate::unix_now() - s.last_sample_ts;
            let ok = s.last_sample_ts > 0 && age <= STALE_SECS;
            let body = format!("{{\"ok\":{ok},\"last_sample_age_s\":{},\"version\":\"{}\"}}\n", if s.last_sample_ts > 0 { age } else { -1 }, env!("CARGO_PKG_VERSION"));
            (if ok { 200 } else { 503 }, "application/json", body)
        }
        "/" => (200, "text/plain", "lss-collector: /status /rules /loadouts /tokens /advice /bench (JSON, schema v1)  /series?metrics=a,b&range=6h&step=auto  /hist?metric=ttft&range=1h  /gateway?range=1h  /metrics (Prometheus)  /health  POST /test-alert /bench /bench/cancel /maintenance/start /maintenance/stop (127.0.0.1 only)\n".into()),
        _ => (404, "text/plain", "not found\n".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let shared = Arc::new(Mutex::new(Shared { status_json: "{\"v\":1}".into(), metrics_text: "llm_serve_up 1\n".into(), ..Default::default() }));
        assert_eq!(route("/status", &shared), (200, "application/json", "{\"v\":1}".into()));
        assert_eq!(route("/metrics?x=1", &shared).2, "llm_serve_up 1\n");
        assert_eq!(route("/nope", &shared).0, 404);
        shared.lock().unwrap().rules_json = "{\"v\":1,\"rules\":[]}".into();
        assert_eq!(route("/rules", &shared), (200, "application/json", "{\"v\":1,\"rules\":[]}".into()));
        assert!(is_history("/series") && is_history("/hist") && is_history("/gateway") && !is_history("/status"));
        assert_eq!(route("/health", &shared).0, 503, "no sample yet = not healthy");
        shared.lock().unwrap().last_sample_ts = crate::unix_now();
        let (code, _, body) = route("/health", &shared);
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":true"));
        shared.lock().unwrap().last_sample_ts = crate::unix_now() - 120;
        assert_eq!(route("/health", &shared).0, 503, "stale poll loop = not healthy");
    }

    #[test]
    fn test_alert_is_loopback_only_post_only_and_bounded() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let local: Option<SocketAddr> = Some("127.0.0.1:50000".parse().unwrap());
        let tailnet: Option<SocketAddr> = Some("192.0.2.5:50000".parse().unwrap());
        // the tailnet listener never accepts it, whoever asks; nor does a non-local peer
        assert_eq!(test_alert(false, local, true, "x", &shared).0, 403);
        assert_eq!(test_alert(false, tailnet, true, "x", &shared).0, 403);
        assert_eq!(test_alert(true, tailnet, true, "x", &shared).0, 403);
        assert_eq!(test_alert(true, None, true, "x", &shared).0, 403);
        assert_eq!(test_alert(true, local, false, "x", &shared).0, 405);
        assert_eq!(test_alert(true, local, true, "  \n ", &shared).0, 400);
        assert!(shared.lock().unwrap().test_alerts.is_empty(), "nothing refused was queued");

        let (code, _, body) = test_alert(true, local, true, " pipeline\ncheck ", &shared);
        assert_eq!(code, 202);
        assert!(body.contains("\"message\":\"[TEST] pipeline check\""), "{body}");
        assert_eq!(shared.lock().unwrap().test_alerts, vec!["pipeline check"], "control characters are flattened");
        let long = "y".repeat(1000);
        test_alert(true, local, true, &long, &shared);
        assert_eq!(shared.lock().unwrap().test_alerts[1].chars().count(), TEST_ALERT_MAX_CHARS);
        for _ in 0..3 {
            assert_eq!(test_alert(true, local, true, "fill", &shared).0, 202);
        }
        assert_eq!(test_alert(true, local, true, "one too many", &shared).0, 429);
    }

    /// #22, 2026-09-21: same loopback-only rule as `/bench` and `/test-alert` - changing
    /// alerting behaviour is only ever a decision made on the box itself.
    #[test]
    fn maintenance_post_is_loopback_only_and_writes_through_to_shared_state() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let local: Option<SocketAddr> = Some("127.0.0.1:50000".parse().unwrap());
        let tailnet: Option<SocketAddr> = Some("192.0.2.5:50000".parse().unwrap());
        assert_eq!(maintenance_post("/maintenance/start", false, local, "{\"reason\":\"x\"}", &shared).0, 403);
        assert_eq!(maintenance_post("/maintenance/start", true, tailnet, "{\"reason\":\"x\"}", &shared).0, 403);
        assert!(!shared.lock().unwrap().maintenance.active, "nothing refused was applied");

        let (code, body) = maintenance_post("/maintenance/start", true, local, "{\"reason\":\"gate v5.2 swap\",\"minutes\":15}", &shared);
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":true") && body.contains("gate v5.2 swap"), "{body}");
        assert!(shared.lock().unwrap().maintenance.active);
        assert_eq!(shared.lock().unwrap().maintenance.reason, "gate v5.2 swap");

        // a blank reason is refused, and does not disturb the window already open
        let (code, body) = maintenance_post("/maintenance/start", true, local, "{\"reason\":\"  \"}", &shared);
        assert_eq!(code, 400);
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(shared.lock().unwrap().maintenance.active, "the earlier valid window must survive a rejected one");

        let (code, body) = maintenance_post("/maintenance/stop", true, local, "", &shared);
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":true") && body.contains("gate v5.2 swap"), "{body}");
        assert!(!shared.lock().unwrap().maintenance.active);

        // stopping again: not an error, just says so
        let (code, body) = maintenance_post("/maintenance/stop", true, local, "", &shared);
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":false"), "{body}");

        assert_eq!(maintenance_post("/maintenance/nope", true, local, "", &shared).0, 404);
    }
}
