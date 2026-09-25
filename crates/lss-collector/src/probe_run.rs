//! The C1 decode probe: ONE streamed request, max_tokens 128, through the gate's trusted
//! port, only when the engine is idle. This thread is the only thing that ever sends a
//! probe and it sends them synchronously, so two can never be in flight.

use crate::collect::http_get;
use crate::Shared;
use lss_core::config::Config;
use lss_core::gate::parse_gate_health;
use lss_core::probe::{decide, parse_sse_line, timing, validate, ProbeDecision, ProbeEvidence, ProbeGate, ProbeRecord, SseEvent, PROBE_HEADER, PROBE_USER};
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run_loop(cfg: Config, shared: Arc<Mutex<Shared>>, mut last_attempt: Option<i64>) {
    let agent = crate::tls::agent().timeout_connect(Duration::from_secs(3)).build();
    // card #316: the gateway has its own TLS setting (`tls_verify` is per URL)
    let gate_agent = crate::tls::gate_agent().timeout_connect(Duration::from_secs(3)).build();
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let now = crate::unix_now();
        let (serve_up, model, running, queue, bench_active, baseline, ttft_baseline) = {
            let s = shared.lock().unwrap_or_else(|e| e.into_inner());
            (s.serve_up, s.model.clone(), s.running, s.queue, s.bench.active, s.c1_baseline, s.c1_ttft_baseline_ms)
        };
        if bench_active {
            // `lss bench` owns the server: a probe in the gap between two of its steps would
            // look like a real user to its abort watch, and would not be an idle reading anyway.
            // `last_attempt` is left alone, so the probe runs as soon as the bench is over.
            continue;
        }
        let gate = ProbeGate { now, last_attempt, interval_secs: cfg.probe.interval_secs, in_flight: false, serve_up, running, queue };
        let record = match decide(&gate) {
            ProbeDecision::Run => {
                // the 5 s sample may be stale: look again right before sending
                let fresh = engine_load(&agent, &cfg);
                // an engine that loads on demand (Ollama) says nothing is loaded until the first
                // request: ask it what it COULD serve, or C1 would never measure anything
                let model = model.filter(|m| m != lss_core::engine::NO_MODEL).or_else(|| probe_model(&agent, &cfg));
                match (fresh, model) {
                    (Some(m), _) if m.running > 0.0 || m.queue > 0.0 => ProbeRecord::skipped_busy(now, m.running, m.queue),
                    (Some(m), Some(model)) => run_validated_probe((&agent, &gate_agent), &cfg, &model, now, &m, baseline, ttft_baseline),
                    _ => continue,
                }
            }
            ProbeDecision::SkipBusy => ProbeRecord::skipped_busy(now, running, queue),
            ProbeDecision::NotDue | ProbeDecision::InFlight | ProbeDecision::SkipDown => continue,
        };
        last_attempt = Some(now);
        let verdict = if record.valid { "valid".to_string() } else { format!("INVALID {}", record.invalid_reason.as_deref().unwrap_or("?")) };
        eprintln!("probe: {} {verdict} ttft_ms={:?} decode_tok_s={:?} {}", record.status, record.ttft_ms, record.decode_tok_s, record.detail);
        shared.lock().unwrap_or_else(|e| e.into_inner()).probe_results.push(record);
    }
}

/// What to ask for when the engine has nothing loaded yet.
fn probe_model(agent: &ureq::Agent, cfg: &Config) -> Option<String> {
    let base = cfg.sglang_url.trim_end_matches('/').to_string();
    let fetch = |path: &str| http_get(agent, &format!("{base}{path}"));
    let kind = lss_core::engine::EngineKind::parse(&cfg.engine_kind).or_else(|| lss_core::engine::detect(&fetch).map(|(k, _)| k))?;
    lss_core::engine::servable_model(kind, &fetch)
}

/// How busy the engine is right now, whatever the engine: (running, queued). An engine that
/// does not publish them (Ollama, LM Studio, a plain OpenAI-compatible server) counts as idle -
/// the probe then still runs, and its TTFT check is what throws out a reading that met traffic.
struct Load {
    running: f64,
    queue: f64,
    /// false = this engine publishes no request count at all (Ollama, LM Studio, generic
    /// OpenAI): "idle" cannot be proved, only assumed
    reports_load: bool,
    /// `generation_tokens_total`, when the adapter has it (card #45): the counter check that
    /// catches a request the 5 s poll's own `running` gauge never saw
    gen_tokens: Option<f64>,
    /// card #266: `requests_total` / `prompt_tokens_total`, when the adapter has them
    requests: Option<f64>,
    prompt_tokens: Option<f64>,
}

/// card #266: how long to wait, after the stream ends, for the engine to count the probe's OWN
/// request before judging the counters. The engine counts a request when it finishes, which can
/// be a moment after the last chunk reached us; judging before that lets another request's
/// finish pass for the probe's own.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const SETTLE_STEP: Duration = Duration::from_millis(200);

fn engine_load(agent: &ureq::Agent, cfg: &Config) -> Option<Load> {
    let base = cfg.sglang_url.trim_end_matches('/').to_string();
    let fetch = |path: &str| http_get(agent, &format!("{base}{path}"));
    let kind = lss_core::engine::EngineKind::parse(&cfg.engine_kind).or_else(|| lss_core::engine::detect(&fetch).map(|(k, _)| k))?;
    let scrape = lss_core::engine::adapter_for(kind, &cfg.public_priority, &cfg.trusted_priority).scrape(&fetch);
    // numbers are enough (the model list is the poll loop's business); neither = unreachable
    if scrape.model.is_err() && scrape.metrics.is_none() {
        return None;
    }
    let m = scrape.metrics.unwrap_or_default();
    Some(Load { running: m.running.unwrap_or(0.0), queue: m.queued.unwrap_or(0.0), reports_load: m.running.is_some(), gen_tokens: m.generation_tokens_total, requests: m.requests_total, prompt_tokens: m.prompt_tokens_total })
}

/// card #266: the engine read right after the probe, once the probe's own request has been
/// counted (`requests_total` grew) or `SETTLE_TIMEOUT` passed. An engine without the counter is
/// read once, as before.
fn settled_load(agent: &ureq::Agent, cfg: &Config, before: &Load) -> Option<Load> {
    let t0 = Instant::now();
    loop {
        let now = engine_load(agent, cfg);
        let counted = match (before.requests, now.as_ref().and_then(|l| l.requests)) {
            (Some(b), Some(a)) => a - b >= 1.0,
            _ => true,
        };
        if counted || t0.elapsed() >= SETTLE_TIMEOUT {
            return now;
        }
        std::thread::sleep(SETTLE_STEP);
    }
}

/// Gate `admitted`, public + trusted: it grows by exactly one per request let through.
fn gate_admitted(agent: &ureq::Agent, cfg: &Config) -> Option<u64> {
    if !cfg.has_gate() {
        return None;
    }
    let body = http_get(agent, &format!("{}/gate/health", cfg.gate_url)).ok()?;
    parse_gate_health(&body).map(|h| h.public.admitted + h.trusted.admitted)
}

/// A probe is only a C1 reading if it ran ALONE. `before` = the engine totals read a moment
/// ago (the caller already refused to probe a busy engine); the gate's admitted counters are
/// read either side of the probe and the engine once more right after it.
/// `(agent, gate_agent)`: the engine's and the gateway's (card #316: `tls_verify` is per URL).
fn run_validated_probe((agent, gate_agent): (&ureq::Agent, &ureq::Agent), cfg: &Config, model: &str, now: i64, before: &Load, baseline: Option<f64>, ttft_baseline_ms: Option<f64>) -> ProbeRecord {
    let admitted_before = gate_admitted(gate_agent, cfg);
    // the probe goes through the gateway when there is one (`chat_base`)
    let mut record = run_probe(if cfg.has_gate() { gate_agent } else { agent }, cfg, model, now, baseline);
    if !record.is_ok() {
        // no timing at all: not a reading, and the reason is the status itself
        record.valid = false;
        record.invalid_reason = Some(record.status.clone());
        return record;
    }
    let after = settled_load(agent, cfg, before);
    let reports_load = after.as_ref().is_none_or(|l| l.reports_load);
    let evidence = ProbeEvidence {
        running_before: before.running,
        queue_before: before.queue,
        ttft_ms: record.ttft_ms.unwrap_or(f64::MAX),
        running_after: after.as_ref().filter(|l| l.reports_load).map(|m| m.running),
        queue_after: after.as_ref().filter(|l| l.reports_load).map(|m| m.queue),
        admitted_before,
        admitted_after: gate_admitted(gate_agent, cfg),
        probe_admitted: record.was_admitted(),
        gateway: cfg.has_gate(),
        engine_reports_load: reports_load,
        gen_tokens_before: before.gen_tokens,
        gen_tokens_after: after.as_ref().and_then(|l| l.gen_tokens),
        probe_tokens: f64::from(record.tokens.unwrap_or(0)),
        ttft_baseline_ms,
        requests_before: before.requests,
        requests_after: after.as_ref().and_then(|l| l.requests),
        prompt_tokens_before: before.prompt_tokens,
        prompt_tokens_after: after.as_ref().and_then(|l| l.prompt_tokens),
    };
    match validate(&evidence, cfg.rules.c1_max_ttft_s) {
        Err((reason, detail)) => record.invalidate(reason, detail),
        Ok(lss_core::probe::Confidence::CouldNotRuleOut(why)) => {
            // still a reading - the median of many is what MODEL shows - but it says what was
            // not proved, so nobody reads it as "measured on an idle server"
            record.unverified = true;
            record.detail = format!("{}: {why}", lss_core::probe::UNVERIFIED_NOTE);
        }
        Ok(lss_core::probe::Confidence::RanAlone) => {}
    }
    record
}

/// The probe's request body. `user` marks it as the monitor's own for anyone reading the
/// engine's or the gate's side; the header does the same on the wire.
pub fn probe_body(cfg: &Config, model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": cfg.probe.prompt}],
        "max_tokens": cfg.probe.max_tokens,
        "temperature": 0,
        "stream": true,
        "stream_options": {"include_usage": true},
        "user": PROBE_USER,
    })
}

fn run_probe(agent: &ureq::Agent, cfg: &Config, model: &str, now: i64, baseline: Option<f64>) -> ProbeRecord {
    let body = probe_body(cfg, model);
    let fail = |status: &str, detail: String, http_status: Option<u16>| ProbeRecord { ts: now, status: status.into(), ttft_ms: None, decode_tok_s: None, tokens: None, detail, http_status, valid: false, invalid_reason: Some(status.to_string()) , unverified: false};
    let deadline = Duration::from_secs(cfg.probe.timeout_secs);
    let t0 = Instant::now();
    let resp = match crate::collect::authed(agent.post(&format!("{}/v1/chat/completions", cfg.chat_base())))
        .timeout(deadline)
        .set("Content-Type", "application/json")
        .set(PROBE_HEADER, "1")
        // the gate's access log prints the User-Agent: this is what makes the probe visible there
        .set("User-Agent", concat!("lss-probe/", env!("CARGO_PKG_VERSION")))
        .send_string(&body.to_string())
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return fail("error", format!("HTTP {code}"), Some(code)),
        Err(e) => {
            let timed_out = t0.elapsed() >= deadline;
            return fail(if timed_out { "timeout" } else { "error" }, e.to_string().chars().take(120).collect(), None);
        }
    };
    let http_status = Some(resp.status());
    let mut chunk_ms = Vec::new();
    let mut usage = None;
    for line in BufReader::new(resp.into_reader()).lines() {
        let Ok(line) = line else {
            return fail(if t0.elapsed() >= deadline { "timeout" } else { "error" }, "stream broke".into(), http_status);
        };
        match parse_sse_line(&line) {
            SseEvent::Token => chunk_ms.push(t0.elapsed().as_secs_f64() * 1000.0),
            SseEvent::Usage(n) => usage = Some(n),
            SseEvent::Done => break,
            SseEvent::Other => {}
        }
    }
    match timing(&chunk_ms, usage) {
        Some(t) => {
            let mut rec = ProbeRecord {
                ts: now,
                status: "ok".into(),
                ttft_ms: Some((t.ttft_ms * 10.0).round() / 10.0),
                decode_tok_s: Some((t.decode_tok_s * 10.0).round() / 10.0),
                tokens: Some(t.tokens),
                detail: String::new(),
                http_status,
                valid: true,
                invalid_reason: None,
                unverified: false,
            };
            // an answer that arrived in one piece is not a speed, whatever the arithmetic says
            if let Some(why) = lss_core::probe::burst_reason(&t, baseline) {
                rec.invalidate(lss_core::probe::INVALID_BURST, why);
            }
            rec
        }
        None => fail("error", format!("stream too short to time ({} chunks)", chunk_ms.len()), http_status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn the_probe_is_tagged_on_the_wire_and_records_the_http_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0_u8; 4096];
            // headers, then a body of Content-Length bytes
            loop {
                let n = sock.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let len: usize = text.to_lowercase().split("content-length:").nth(1).and_then(|r| r.lines().next()).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    if req.len() >= split + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
            std::thread::sleep(Duration::from_millis(20));
            let _ = write!(sock, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", sse.len());
            let (first, rest) = sse.split_at(sse.find("\n\n").unwrap() + 2);
            let _ = sock.write_all(first.as_bytes());
            let _ = sock.flush();
            std::thread::sleep(Duration::from_millis(30));
            let _ = sock.write_all(rest.as_bytes());
            String::from_utf8_lossy(&req).to_string()
        });
        let cfg = Config { gate_url: format!("http://127.0.0.1:{port}"), ..Default::default() };
        let agent = ureq::AgentBuilder::new().build();
        let rec = run_probe(&agent, &cfg, "local", 1234, None);
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions "), "{req}");
        assert!(req.to_lowercase().contains("x-lss-probe: 1"), "{req}");
        assert!(req.to_lowercase().contains("user-agent: lss-probe/"), "{req}");
        assert!(req.contains("\"user\":\"lss-probe\""), "{req}");
        assert!(req.contains("\"max_tokens\":128") && req.contains("\"stream\":true"));
        assert_eq!((rec.status.as_str(), rec.http_status, rec.tokens), ("ok", Some(200), Some(9)), "{rec:?}");

        // a gate that refuses: the status is recorded, which is what keeps it out of `admitted`
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        });
        let cfg = Config { gate_url: format!("http://127.0.0.1:{port}"), ..Default::default() };
        let rec = run_probe(&agent, &cfg, "local", 1234, None);
        assert_eq!((rec.status.as_str(), rec.http_status), ("error", Some(429)));
        assert!(!rec.was_admitted());
    }

    /// A fake box on one port: the engine's /metrics, the gate's /gate/health and the chat
    /// endpoint. `others` = requests the gate admits for someone else while the probe runs;
    /// `running_after` = what the engine reports once it is over; `ttft` = delay to first token.
    fn fake_box(others: u64, running_after: f64, ttft: Duration) -> (String, std::thread::JoinHandle<()>) {
        fake_box_chunks(others, running_after, ttft, 64)
    }

    fn fake_box_chunks(others: u64, running_after: f64, ttft: Duration, chunks: usize) -> (String, std::thread::JoinHandle<()>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut admitted, mut probed) = (60_u64, false);
            for mut req in server.incoming_requests() {
                let path = req.url().to_string();
                let body = match path.as_str() {
                    "/gate/health" => format!("{{\"version\":\"v5\",\"upstreams\":{{\"m\":{{\"ok\":true}}}},\"admission\":{{\"trusted\":{{\"admitted\":{admitted}}},\"public\":{{\"admitted\":0}}}}}}"),
                    "/metrics" => {
                        let running = if probed { running_after } else { 0.0 };
                        format!("sglang:num_running_reqs{{priority=\"\",tp_rank=\"0\"}} {running}\nsglang:num_queue_reqs{{priority=\"\",tp_rank=\"0\"}} 0.0\n")
                    }
                    "/v1/chat/completions" => {
                        let mut sink = String::new();
                        let _ = req.as_reader().read_to_string(&mut sink);
                        std::thread::sleep(ttft);
                        admitted += 1 + others;
                        probed = true;
                        // one token per chunk, the way a server streams: a probe whose answer
                        // arrives in a few fat chunks is a BURST and is stored invalid (see
                        // `probe::burst_reason`), which is a different test
                        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";
                        format!("{}data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":64}}}}\n\ndata: [DONE]\n\n", chunk.repeat(chunks))
                    }
                    "/stop" => {
                        let _ = req.respond(tiny_http::Response::from_string("bye"));
                        return;
                    }
                    _ => String::new(),
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        (url, handle)
    }

    /// an idle-before reading with no counter and no GPU clock evidence: neither new #45 check
    /// fires, so these tests still probe exactly what they probed before that card
    fn no_load() -> Load {
        Load { running: 0.0, queue: 0.0, reports_load: true, gen_tokens: None, requests: None, prompt_tokens: None }
    }

    fn probe_against(others: u64, running_after: f64, ttft: Duration, max_ttft_s: f64) -> ProbeRecord {
        let (url, handle) = fake_box(others, running_after, ttft);
        let mut cfg = Config { gate_url: url.clone(), sglang_url: url.clone(), ..Default::default() };
        cfg.rules.c1_max_ttft_s = max_ttft_s;
        let agent = ureq::AgentBuilder::new().build();
        let rec = run_validated_probe((&agent, &agent), &cfg, "local", 1234, &no_load(), None, None);
        let _ = agent.get(&format!("{url}/stop")).call();
        let _ = handle.join();
        rec
    }

    #[test]
    fn a_probe_that_ran_alone_is_stored_valid() {
        let rec = probe_against(0, 1.0, Duration::ZERO, 3.0);
        assert_eq!((rec.status.as_str(), rec.valid, rec.invalid_reason.as_deref()), ("ok", true, None), "{rec:?}");
    }

    /// The same server, answering in 8 fat chunks instead of 64: not a speed, whatever the
    /// arithmetic says (2026-09-20: 26,773 tok/s stored as a valid reading).
    #[test]
    fn a_probe_whose_answer_arrived_in_one_burst_is_stored_invalid() {
        let (url, handle) = fake_box_chunks(0, 1.0, Duration::ZERO, 8);
        let mut cfg = Config { gate_url: url.clone(), sglang_url: url.clone(), ..Default::default() };
        cfg.rules.c1_max_ttft_s = 3.0;
        let agent = ureq::AgentBuilder::new().build();
        let rec = run_validated_probe((&agent, &agent), &cfg, "local", 1234, &no_load(), None, None);
        let _ = agent.get(&format!("{url}/stop")).call();
        let _ = handle.join();
        assert_eq!((rec.status.as_str(), rec.valid, rec.invalid_reason.as_deref()), ("ok", false, Some("burst")), "{rec:?}");
        assert!(rec.detail.contains("arrived in one burst"), "{}", rec.detail);
    }

    /// E1: `gate_url = ""` is what a new user has. Before 2026-09-20 every probe there was
    /// stored INVALID ("gate admitted counters unreadable"), so C1 never learned anything.
    #[test]
    fn with_no_gateway_the_engine_alone_decides() {
        let probe_no_gate = |running_after: f64| {
            let (url, handle) = fake_box(0, running_after, Duration::ZERO);
            let cfg = Config { gate_url: String::new(), sglang_url: url.clone(), engine_kind: "sglang".into(), ..Default::default() };
            let agent = ureq::AgentBuilder::new().build();
            let rec = run_validated_probe((&agent, &agent), &cfg, "local", 1234, &no_load(), None, None);
            let _ = agent.get(&format!("{url}/stop")).call();
            let _ = handle.join();
            rec
        };
        // an engine that reports its load (SGLang here, llama.cpp live) still PROVES it ran alone
        let rec = probe_no_gate(1.0);
        assert_eq!((rec.status.as_str(), rec.valid, rec.unverified, rec.invalid_reason.as_deref()), ("ok", true, false, None), "{rec:?}");
        assert!(rec.decode_tok_s.is_some_and(|v| v > 0.0));
        // and it still catches somebody else on the engine
        let busy = probe_no_gate(3.0);
        assert_eq!((busy.valid, busy.invalid_reason.as_deref()), (false, Some("contended")));
    }

    #[test]
    fn a_probe_with_a_slow_first_token_is_stored_invalid() {
        let rec = probe_against(0, 1.0, Duration::from_millis(250), 0.1);
        assert_eq!((rec.valid, rec.invalid_reason.as_deref()), (false, Some("slow_ttft")), "{rec:?}");
        assert!(rec.decode_tok_s.is_some(), "the number is kept for the record, it is just never used");
    }

    #[test]
    fn a_probe_that_shared_the_engine_is_stored_invalid() {
        // the gate let two other requests in while the probe ran
        let rec = probe_against(2, 1.0, Duration::ZERO, 3.0);
        assert_eq!((rec.valid, rec.invalid_reason.as_deref()), (false, Some("contended")), "{rec:?}");
        assert!(rec.detail.contains("admitted 2 other request(s)"), "{}", rec.detail);
        // nobody new at the gate, but the engine is still running someone next to the probe
        let rec = probe_against(0, 3.0, Duration::ZERO, 3.0);
        assert_eq!((rec.valid, rec.invalid_reason.as_deref()), (false, Some("contended")), "{rec:?}");
    }

    /// #45, end-to-end over the real HTTP scrape (not just `validate()`'s own unit test): the
    /// live 08:45 mechanism. `/metrics` reports `running=0` and `queue=0` both before and after -
    /// today's gauge check alone would call this a clean idle reading - but `generation_tokens_total`
    /// jumps by far more than the probe's own 64-token answer, because another client's request
    /// started and finished entirely between the two scrapes this test takes. Proves the new
    /// `Load.gen_tokens` field actually reaches `ProbeEvidence` through `engine_load` and
    /// `run_validated_probe`, not just that `validate()` rejects it in isolation.
    #[test]
    fn a_request_invisible_to_running_but_not_to_the_token_counter_is_caught() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let mut probed = false;
            for mut req in server.incoming_requests() {
                let path = req.url().to_string();
                let body = match path.as_str() {
                    "/gate/health" => "{\"version\":\"v5\",\"upstreams\":{\"m\":{\"ok\":true}},\"admission\":{\"trusted\":{\"admitted\":60},\"public\":{\"admitted\":0}}}".to_string(),
                    "/metrics" => {
                        // running/queue stay 0 THE WHOLE TIME - a contaminating request that both
                        // starts and ends between these two scrapes is invisible to the gauge
                        let gen = if probed { 48_213.0 + 288.0 } else { 48_213.0 };
                        format!("sglang:num_running_reqs{{priority=\"\",tp_rank=\"0\"}} 0.0\nsglang:num_queue_reqs{{priority=\"\",tp_rank=\"0\"}} 0.0\nsglang:generation_tokens_total{{priority=\"\",tp_rank=\"0\"}} {gen}\n")
                    }
                    "/v1/chat/completions" => {
                        let mut sink = String::new();
                        let _ = req.as_reader().read_to_string(&mut sink);
                        probed = true;
                        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";
                        format!("{}data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":64}}}}\n\ndata: [DONE]\n\n", chunk.repeat(64))
                    }
                    "/stop" => {
                        let _ = req.respond(tiny_http::Response::from_string("bye"));
                        return;
                    }
                    _ => String::new(),
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        let cfg = Config { gate_url: url.clone(), sglang_url: url.clone(), ..Default::default() };
        let agent = ureq::AgentBuilder::new().build();
        let before = engine_load(&agent, &cfg).expect("the fake box answers");
        assert_eq!((before.running, before.gen_tokens), (0.0, Some(48_213.0)), "before the probe: idle, and the counter's starting point is on record");
        let rec = run_validated_probe((&agent, &agent), &cfg, "local", 1234, &before, None, None);
        let _ = agent.get(&format!("{url}/stop")).call();
        let _ = handle.join();
        assert_eq!((rec.status.as_str(), rec.valid, rec.invalid_reason.as_deref()), ("ok", false, Some("contended")), "{rec:?}");
        assert!(rec.detail.contains("288") && rec.detail.contains("64 tokens"), "{}", rec.detail);
    }

    /// card #266 (narrow FAIL): the settle wait itself. An engine that publishes its counters a
    /// moment AFTER the stream ends (a metrics logger on a timer, or counting at request finish):
    /// right after the last chunk it shows only part of the probe's own answer (+40 of 64
    /// tokens) and not yet its request. Read immediately, +40 is "neither 0 nor the probe's own
    /// 64" and an idle probe is thrown out as contended. `settled_load` waits until the engine
    /// has counted the probe's own request, then reads the whole, correct picture. Skipping the
    /// wait turns this test RED.
    #[test]
    fn the_after_read_waits_until_the_engine_has_counted_the_probes_own_request() {
        const LAG: Duration = Duration::from_millis(600);
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let handle = std::thread::spawn(move || {
            let mut answered: Option<Instant> = None;
            for mut req in server.incoming_requests() {
                let path = req.url().to_string();
                let body = match path.as_str() {
                    "/gate/health" => "{\"version\":\"v5\",\"upstreams\":{\"m\":{\"ok\":true}},\"admission\":{\"trusted\":{\"admitted\":60},\"public\":{\"admitted\":0}}}".to_string(),
                    "/metrics" => {
                        // (generated, requests, prompt): idle / part-published / fully counted
                        let (gen, reqs, prompt) = match answered {
                            None => (1000.0, 50.0, 9000.0),
                            Some(t) if t.elapsed() < LAG => (1040.0, 50.0, 9000.0),
                            Some(_) => (1064.0, 51.0, 9030.0),
                        };
                        let l = "{priority=\"\",tp_rank=\"0\"}";
                        format!("sglang:num_running_reqs{l} 0.0\nsglang:num_queue_reqs{l} 0.0\nsglang:generation_tokens_total{l} {gen}\nsglang:num_requests_total{l} {reqs}\nsglang:prompt_tokens_total{l} {prompt}\n")
                    }
                    "/v1/chat/completions" => {
                        let mut sink = String::new();
                        let _ = req.as_reader().read_to_string(&mut sink);
                        answered = Some(Instant::now());
                        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";
                        format!("{}data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":64}}}}\n\ndata: [DONE]\n\n", chunk.repeat(64))
                    }
                    "/stop" => {
                        let _ = req.respond(tiny_http::Response::from_string("bye"));
                        return;
                    }
                    _ => String::new(),
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        let cfg = Config { gate_url: url.clone(), sglang_url: url.clone(), ..Default::default() };
        let agent = ureq::AgentBuilder::new().build();
        let before = engine_load(&agent, &cfg).expect("the fake box answers");
        assert_eq!((before.gen_tokens, before.requests, before.prompt_tokens), (Some(1000.0), Some(50.0), Some(9000.0)), "the counters reach Load");
        let t0 = Instant::now();
        let rec = run_validated_probe((&agent, &agent), &cfg, "local", 1234, &before, None, None);
        let took = t0.elapsed();
        let _ = agent.get(&format!("{url}/stop")).call();
        let _ = handle.join();
        assert_eq!((rec.status.as_str(), rec.valid, rec.invalid_reason.as_deref()), ("ok", true, None), "an idle probe read before its own request was counted: {rec:?}");
        assert!(took >= LAG, "the after-read did not wait for the engine to count the probe ({took:?})");
        assert!(took < LAG + SETTLE_TIMEOUT, "it waited for the count, not the full timeout ({took:?})");
    }
}
