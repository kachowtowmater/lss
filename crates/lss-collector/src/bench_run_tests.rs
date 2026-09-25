//! `lss bench` end to end against a FAKE harness (a shell script standing in for
//! `llm_decode_bench.py`) and a fake engine + gateway (one tiny_http server playing SGLang's
//! `/metrics`, the gate's `/gate/health` and `/v1/chat/completions`). No model, no GPU, no load.

use crate::bench_run::{self, BenchCtx};
use crate::db::Db;
use crate::Shared;
use lss_core::bench::BenchRequest;
use lss_core::config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SAMPLE: &str = include_str!("../../../fixtures/benchmark_results_sample.json");

#[derive(Default)]
struct Fake {
    /// engine total across every lane (the `priority=""` row)
    running: f64,
    /// requests the engine labels with the TRUSTED lane's priority ("0").
    ///
    /// Card #207: this is where the BENCH'S OWN direct-to-engine requests land, because SGLang
    /// stamps `priority="0"` on a request that asked for no priority - so a test that wants to
    /// model a real quiet run has to put the bench's own concurrency in here, and the watch must
    /// still not abort. Setting it above the bench's own load is what "somebody else showed up"
    /// looks like.
    gateway_trusted: f64,
    /// requests in the PUBLIC lane ("10"): never the bench's, it holds no key for that lane
    gateway_public: f64,
    /// Some = a v5.2 gate: (user, in flight) rows; None = v5.1, no user table
    users: Option<Vec<(String, u64)>>,
    /// every chat request the "gateway" received: (the X-LSS-Bench header, body)
    chats: Vec<(Option<String>, serde_json::Value)>,
    /// what the gateway answers to a chat request instead of a model answer
    chat_status: Option<u16>,
    /// card #312: Some = the engine wants `Authorization: Bearer <this>` on /v1/models (401 else)
    required_key: Option<String>,
    /// the Authorization header of every /v1/models request FROM THE HARNESS (its User-Agent says
    /// so; the collector's own watch asks /v1/models too), and of every chat request
    models_auth: Vec<Option<String>>,
    chat_auth: Vec<Option<String>>,
    /// card #312: lss's own preflight requests (User-Agent lss-preflight/...): (path, Authorization)
    preflight: Vec<(String, Option<String>)>,
    /// card #312: the key is demanded of the HARNESS only (preflight let through) - exercises the
    /// harness-log backstop behind the preflight
    harness_only: bool,
    /// card #312: the preflight's GET /v1/models is open, only its chat request wants the key
    guard_chat_only: bool,
}

fn metrics_text(f: &Fake) -> String {
    let l = "engine_type=\"unified\",model_name=\"m\",moe_ep_rank=\"0\",pp_rank=\"0\"";
    format!(
        "sglang:num_running_reqs{{{l},priority=\"\",tp_rank=\"0\"}} {}\nsglang:num_running_reqs{{{l},priority=\"0\",tp_rank=\"0\"}} {}\nsglang:num_running_reqs{{{l},priority=\"10\",tp_rank=\"0\"}} {}\nsglang:num_queue_reqs{{{l},priority=\"\",tp_rank=\"0\"}} 0.0\nsglang:max_total_num_tokens{{{l},tp_rank=\"0\"}} 3788160.0\nsglang:generation_tokens_total{{{l},priority=\"0\"}} 1000.0\n",
        f.running, f.gateway_trusted, f.gateway_public
    )
}

fn health_json(f: &Fake) -> String {
    match &f.users {
        None => serde_json::json!({"version": "v5.1", "upstreams": {"gpu": {"ok": true}}, "admission": {}}).to_string(),
        Some(rows) => {
            let users: Vec<serde_json::Value> = rows.iter().map(|(u, n)| serde_json::json!({"lane": "trusted", "user": u, "inflight": n})).collect();
            serde_json::json!({"version": "v5.2", "upstreams": {"gpu": {"ok": true}}, "admission": {}, "users": users, "totals": {"inflight": rows.iter().map(|r| r.1).sum::<u64>()}}).to_string()
        }
    }
}

fn chat_answer(body: &serde_json::Value) -> serde_json::Value {
    let message = if body.get("tools").is_some() {
        serde_json::json!({"content": null, "tool_calls": [{"function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}}]})
    } else if body.get("response_format").is_some() {
        serde_json::json!({"content": "{\"colour\": \"blue\", \"count\": 3}"})
    } else if body["messages"][0]["content"].as_str().is_some_and(|c| c.contains("secret access code")) {
        serde_json::json!({"content": format!("The code is {}.", lss_core::bench::NEEDLE_CODE)})
    } else {
        serde_json::json!({"content": "391"})
    };
    serde_json::json!({"choices": [{"message": message}], "usage": {"prompt_tokens": 251_000}})
}

/// Starts the fake engine + gateway; returns its base URL.
fn fake_server(state: Arc<Mutex<Fake>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", server.server_addr().to_ip().expect("ip"));
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let path = req.url().split('?').next().unwrap_or("").to_string();
            // card #312: lss's own preflight, with or without the key, before any harness step
            if req.headers().iter().any(|h| h.field.equiv("User-Agent") && h.value.as_str().starts_with("lss-preflight/")) {
                let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                let mut s = state.lock().unwrap();
                s.preflight.push((path.clone(), auth.clone()));
                let refused = !s.harness_only && !(s.guard_chat_only && path == "/v1/models") && s.required_key.as_ref().is_some_and(|k| auth.as_deref() != Some(format!("Bearer {k}").as_str()));
                let (code, body) = match (refused, path.as_str()) {
                    (true, _) => (401, "{\"error\": \"invalid api key\"}".to_string()),
                    (false, "/v1/models") => (200, "{\"object\": \"list\", \"data\": [{\"id\": \"model-a\"}]}".to_string()),
                    (false, _) => (200, serde_json::json!({"choices": [{"message": {"content": "hi"}}]}).to_string()),
                };
                drop(s);
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
                continue;
            }
            let (code, body) = match path.as_str() {
                "/metrics" => (200, metrics_text(&state.lock().unwrap())),
                "/gate/health" => (200, health_json(&state.lock().unwrap())),
                "/v1/models" => {
                    let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                    let from_harness = req.headers().iter().any(|h| h.field.equiv("User-Agent") && h.value.as_str() == "fake-harness");
                    let mut s = state.lock().unwrap();
                    if from_harness {
                        s.models_auth.push(auth.clone());
                    }
                    match &s.required_key {
                        Some(k) if auth.as_deref() != Some(format!("Bearer {k}").as_str()) => (401, "{\"error\": \"invalid api key\"}".to_string()),
                        _ => (200, "{\"object\": \"list\", \"data\": [{\"id\": \"model-a\"}]}".to_string()),
                    }
                }
                "/v1/chat/completions" => {
                    let mut text = String::new();
                    let _ = req.as_reader().read_to_string(&mut text);
                    let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                    let header = req.headers().iter().find(|h| h.field.equiv("X-LSS-Bench")).map(|h| h.value.to_string());
                    let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                    let mut s = state.lock().unwrap();
                    s.chat_auth.push(auth);
                    s.chats.push((header, body.clone()));
                    match s.chat_status {
                        Some(code) => (code, "{\"error\": \"refused\"}".to_string()),
                        None => (200, chat_answer(&body).to_string()),
                    }
                }
                _ => (404, "not found".to_string()),
            };
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
        }
    });
    url
}

static N: AtomicU32 = AtomicU32::new(0);

/// card #340: the `LSS_TEST_SSH` tests all log in as the ONE remote user and each checks that its
/// run left that user's `.cache/lss-bench` empty - run in parallel, one saw another's live run
/// directory ('the per-run remote directory was removed after the copy', left ".cache/lss-bench").
/// They take this lock, one at a time; the stand-in-ssh tests each have their own remote HOME.
static REAL_SSH: Mutex<()> = Mutex::new(());

struct Rig {
    ctx: Arc<BenchCtx>,
    fake: Arc<Mutex<Fake>>,
    dir: PathBuf,
}

/// `sleep_secs`: how long the fake harness "benchmarks" before it writes its result.
fn rig(sleep_secs: u32, tune: impl FnOnce(&mut Config)) -> Rig {
    let dir = std::env::temp_dir().join(format!("lss-bench-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sample.json"), SAMPLE).unwrap();
    let accuracy = serde_json::json!({"metadata": {"version": "0.4.29"}, "accuracy": {"scored": 200, "correct": 188, "accuracy": 0.94, "wilson95_low": 0.9, "wilson95_high": 0.97}});
    std::fs::write(dir.join("accuracy.json"), accuracy.to_string()).unwrap();
    // the stand-in for `python3 llm_decode_bench.py …`: it is run as `/bin/sh fake_harness.sh …`
    let script = format!(
        "#!/bin/sh\nout=\"\"\nsrc=\"{d}/sample.json\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --help) echo \"usage: fake_harness [--concurrency CONCURRENCY] [--output OUTPUT]\"; exit 0 ;;\n    --output) out=\"$2\"; shift ;;\n    --test-profile) src=\"{d}/accuracy.json\"; shift ;;\n  esac\n  shift\ndone\necho \"New version available: v9.9.9\"\nread answer && echo \"UPGRADED\" > \"{d}/self-updated\"\nsleep {sleep_secs}\ncp \"$src\" \"$out\"\necho \"Results saved to $out\"\n",
        d = dir.display()
    );
    std::fs::write(dir.join("fake_harness.sh"), script).unwrap();
    let fake = Arc::new(Mutex::new(Fake::default()));
    let url = fake_server(fake.clone());
    let bench = lss_core::config::BenchConfig { harness: dir.join("fake_harness.sh").display().to_string(), python: "/bin/sh".into(), results_dir: dir.join("results").display().to_string(), ..Default::default() };
    let mut cfg = Config { sglang_url: url.clone(), gate_url: url, bench, ..Default::default() };
    tune(&mut cfg);
    // #54, 2026-09-21: a shown-quiet counter, not `None` ("not enough history yet") - most of
    // these tests are not about the idle gate and expect a start to succeed by default
    let shared = Arc::new(Mutex::new(Shared { serve_up: true, model: Some("model-a".into()), slots: 8, loadout_id: Some("aaaa11112222".into()), power_w: Some(800.0), recent_tokens_generated: Some(0.0), ..Default::default() }));
    let ctx = Arc::new(BenchCtx { cfg, home: "/nonexistent".into(), db: Arc::new(Mutex::new(Db::memory())), shared, state_dir: dir.clone(), poll: Duration::from_millis(40) });
    Rig { ctx, fake, dir }
}

impl Rig {
    fn start(&self, profile: &str, force: bool) -> (u16, lss_core::bench::BenchStarted) {
        bench_run::request_start(&self.ctx, &BenchRequest { profile: profile.into(), force, note: "test run".into(), ..Default::default() })
    }

    /// card #14: `lss bench quick --under-load`.
    fn start_under_load(&self, profile: &str) -> (u16, lss_core::bench::BenchStarted) {
        bench_run::request_start(&self.ctx, &BenchRequest { profile: profile.into(), under_load: true, note: "test run".into(), ..Default::default() })
    }

    fn wait(&self) -> lss_core::bench::Scorecard {
        let until = Instant::now() + Duration::from_secs(30);
        while self.ctx.shared.lock().unwrap().bench.active {
            assert!(Instant::now() < until, "the bench never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
        self.ctx.db.lock().unwrap().bench_runs(1).unwrap().remove(0)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_idle_gate_refuses_a_busy_or_recently_used_server() {
    let r = rig(0, |_| {});
    r.ctx.shared.lock().unwrap().running = 2.0;
    let (code, no) = r.start("quick", false);
    assert_eq!((code, no.ok, no.run_id), (409, false, None));
    assert!(no.message.contains("the server is busy (2 running"), "{}", no.message);
    {
        let mut s = r.ctx.shared.lock().unwrap();
        s.running = 0.0;
        // #54, 2026-09-21: the token counter, not a request count - a handful of tiny completed
        // requests must not be what blocks this; only real generation does
        s.recent_tokens_generated = Some(1_240.0);
    }
    assert!(r.start("quick", false).1.message.contains("1240 token(s) in the last 5 min"));
    r.ctx.shared.lock().unwrap().recent_tokens_generated = Some(0.0);
    r.ctx.shared.lock().unwrap().other_users_inflight = 1;
    assert!(!r.start("quick", false).1.ok, "a request in flight at the gateway is not idle either");
    r.ctx.shared.lock().unwrap().other_users_inflight = 0;
    // not enough history to PROVE it quiet (e.g. the collector just started) - not a free pass
    r.ctx.shared.lock().unwrap().recent_tokens_generated = None;
    assert!(r.start("quick", false).1.message.contains("not enough history"));
    r.ctx.shared.lock().unwrap().recent_tokens_generated = Some(0.0);
    // nothing was started, recorded or locked by the refusals
    assert!(r.ctx.db.lock().unwrap().bench_runs(5).unwrap().is_empty());
    assert!(!bench_run::lock_path(&r.ctx).exists() && !r.ctx.shared.lock().unwrap().bench.active);
    // --force starts anyway
    let (code, yes) = r.start("quick", true);
    assert_eq!((code, yes.ok), (202, true));
    assert!(r.wait().forced);
    // but never on a serve that is down, and never with a profile or dataset it does not know
    r.ctx.shared.lock().unwrap().serve_up = false;
    assert!(r.start("quick", true).1.message.contains("not up"));
    assert_eq!(r.start("everything", true).0, 400);
    assert_eq!(bench_run::request_start(&r.ctx, &BenchRequest { profile: "accuracy".into(), dataset: "my-own".into(), ..Default::default() }).0, 400);
}

#[test]
fn a_quick_run_wraps_the_harness_and_normalises_the_scorecard() {
    let r = rig(0, |_| {});
    let (code, started) = r.start("quick", false);
    assert_eq!((code, started.ok), (202, true), "{}", started.message);
    assert!(r.ctx.shared.lock().unwrap().bench.active, "the bench window is open from the first moment");
    let card = r.wait();
    assert_eq!((card.status.as_str(), card.aborted.clone(), card.profile.as_str(), card.model.as_str(), card.loadout_id.as_str()), ("ok", None, "quick", "model-a", "aaaa11112222"));
    assert_eq!((card.run_id, started.run_id, card.note.as_str(), card.harness_version.as_str()), (1, Some(1), "test run", "0.4.29"));
    // the scorecard is the REAL sample file, normalised
    let h = card.headline();
    assert_eq!((h.c1_tok_s, h.max_total_tok_s, h.max_total_at, h.prefill_8k_tok_s, h.ttft_c1_ms), (Some(86.8), Some(361.8), Some(4), Some(5791.0), Some(143.3)));
    assert_eq!(card.kv_tokens, Some(3_788_160.0), "KV capacity from the server's metrics");
    // three sanity checks, through the gateway, every one tagged as bench traffic
    assert_eq!(card.sanity.iter().map(|c| (c.name.as_str(), c.pass)).collect::<Vec<_>>(), vec![("arithmetic", true), ("tool call", true), ("json output", true)]);
    let fake = r.fake.lock().unwrap();
    assert_eq!(fake.chats.len(), 3);
    assert!(fake.chats.iter().all(|(h, b)| h.as_deref() == Some("1") && b["user"] == "lss-bench"), "X-LSS-Bench: 1 on every request of ours");
    drop(fake);
    // the raw harness JSON is kept per run; the harness was never allowed to update itself
    let raw = PathBuf::from(&card.raw_dir);
    assert!(raw.ends_with("1-quick-model-a") && raw.join("decode.json").is_file() && raw.join("prefill.json").is_file() && raw.join("harness.log").is_file());
    assert!(!r.dir.join("self-updated").exists(), "stdin is /dev/null: the upgrade prompt read EOF");
    assert!(card.target.contains("engine direct"), "{}", card.target);
    // afterwards: unlocked, idle, remembered, and the bench's own requests are known
    let s = r.ctx.shared.lock().unwrap();
    assert!(!s.bench.active && !bench_run::lock_path(&r.ctx).exists());
    assert_eq!((s.bench.brief.state.as_str(), s.bench.brief.last.as_ref().map(|l| (l.status.clone(), l.headline.c1_tok_s)), s.bench.own_requests.len()), ("idle", Some(("ok".into(), Some(86.8))), 3));
    assert_eq!(s.bench.generation, 2, "bumped at the start and at the end");
    drop(s);
    // card #14: a quiet run says so with evidence - how many times it looked, and with what
    let b = card.background.clone().expect("every run records what else was on the server");
    assert_eq!((b.polls_with_traffic, b.concurrent_max, b.source.as_str()), (0, Some(0.0), lss_core::bench::LOAD_SRC_LANES));
    assert!(b.samples > 0 && card.load_class() == lss_core::bench::LoadClass::Quiet && !card.under_load, "{b:?}");
}

/// card #14, the whole point of the change. The box this monitors is also the fleet's only LLM
/// serve: four attempts over three days were refused by the idle gate, the best quiet window ever
/// observed was 2m30s, and a three-day survey of the engine's token counter found the longest flat
/// run in ANY hour was 5m00s - at or below what the gate asks for. A scorecard that never gets
/// measured is worth nothing, so `--under-load` measures WITH the traffic and the scorecard
/// carries the traffic with it.
#[test]
fn under_load_measures_with_the_traffic_on_the_box_and_records_what_it_was() {
    let r = rig(1, |_| {});
    {
        let mut s = r.ctx.shared.lock().unwrap();
        s.running = 2.0;
        s.recent_tokens_generated = Some(9_000.0);
    }
    // the gate still refuses a plain run, and still says exactly why
    let (code, no) = r.start("quick", false);
    assert_eq!((code, no.ok), (409, false));
    assert!(no.message.contains("--under-load"), "the refusal names the way through it: {}", no.message);

    // --force gets past the gate but NOT past the abort watch: somebody else's request kills it,
    // which is why three days of --force attempts produced no scorecard either
    r.fake.lock().unwrap().users = Some(vec![("acme".into(), 2)]);
    assert!(r.start("quick", true).1.ok);
    let forced = r.wait();
    assert_eq!(forced.status, "aborted", "{:?}", forced.aborted);
    assert!(forced.aborted.as_deref().unwrap().contains("not the bench"), "{:?}", forced.aborted);

    // --under-load runs to the end with the same traffic on the box
    let (code, started) = r.start_under_load("quick");
    assert_eq!((code, started.ok), (202, true), "{}", started.message);
    let card = r.wait();
    assert_eq!(card.status, "ok", "{:?}", card.aborted);
    assert!(card.under_load && card.forced, "the idle gate was skipped and the run says so");
    let b = card.background.clone().expect("a run under load without the load on the scorecard is the failure this prevents");
    assert!(b.samples > 0 && b.polls_with_traffic == b.samples, "every poll saw the other user: {b:?}");
    assert_eq!((b.concurrent_avg, b.concurrent_max, b.source.as_str()), (Some(2.0), Some(2.0), lss_core::bench::LOAD_SRC_USERS));
    assert_eq!(card.load_class(), lss_core::bench::LoadClass::Loaded);
    assert!(card.load_sentence().starts_with("UNDER LOAD:"), "{}", card.load_sentence());
    // it is still a real scorecard, not a placeholder
    assert!(!card.decode.is_empty() && card.sanity.len() == 3, "{:?}", card.decode);
    // THE CASE THAT MATTERS, and the one this test suite did not drive before (2026-09-23):
    // traffic that arrives PART WAY THROUGH. The run above started with the traffic already
    // there; a watch that only looks at the first poll would pass that and still kill a run the
    // moment somebody showed up. This is the exact scenario of
    // `real_traffic_aborts_the_run_and_kills_the_harness`, with the flag on and the opposite
    // expectation - and it is checked against that test's own abort, four lines down, so
    // "it completed" cannot quietly mean "no traffic ever arrived".
    let r = rig(2, |_| {});
    assert!(r.start_under_load("quick").1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 2.0;
        // #207: in the PUBLIC lane. A request in the trusted lane would be indistinguishable
        // from the bench's own priority-less traffic, which is the whole defect this models.
        f.gateway_public = 1.0;
    }
    let card = r.wait();
    assert_eq!(card.status, "ok", "traffic arriving mid-run must NOT kill an --under-load run: {:?}", card.aborted);
    let b = card.background.clone().expect("the traffic that arrived is on the scorecard");
    assert!(b.concurrent_max.is_some_and(|m| m >= 1.0) && b.polls_with_traffic > 0, "the run must have SEEN it, not merely survived: {b:?}");
    assert_eq!(card.load_class(), lss_core::bench::LoadClass::Loaded);
    // the control: the same traffic, the same rig, the flag OFF - this one dies, which is what
    // proves the assertion above is about the flag and not about the traffic never arriving
    let control = rig(2, |_| {});
    assert!(control.start("quick", true).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = control.fake.lock().unwrap();
        f.running = 2.0;
        // #207: in the PUBLIC lane. A request in the trusted lane would be indistinguishable
        // from the bench's own priority-less traffic, which is the whole defect this models.
        f.gateway_public = 1.0;
    }
    assert_eq!(control.wait().status, "aborted", "without the flag the very same traffic kills the run");

    // the refusal ECHOES what the collector understood, so a client talking to a collector too
    // old to know the flag can say so instead of leaving a dead run as the only clue
    let r = rig(0, |_| {});
    r.ctx.shared.lock().unwrap().serve_up = false;
    let (code, no) = r.start_under_load("quick");
    assert_eq!((code, no.ok, no.under_load), (409, false, true), "{}", no.message);

    // cancelling still works under load - standing the abort watch down is not standing the
    // operator down
    let r = rig(20, |_| {});
    r.fake.lock().unwrap().users = Some(vec![("acme".into(), 1)]);
    assert!(r.start_under_load("quick").1.ok);
    assert!(bench_run::request_cancel(&r.ctx).1.ok);
    assert_eq!(r.wait().aborted.as_deref(), Some("cancelled by the operator"));
}

#[test]
fn real_traffic_aborts_the_run_and_kills_the_harness() {
    // v5.1 gate (no user table): a request shows up under the PUBLIC lane's priority label
    let r = rig(20, |_| {});
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 2.0;
        // #207: in the PUBLIC lane. A request in the trusted lane would be indistinguishable
        // from the bench's own priority-less traffic, which is the whole defect this models.
        f.gateway_public = 1.0;
    }
    let t0 = Instant::now();
    let card = r.wait();
    assert!(t0.elapsed() < Duration::from_secs(5), "aborted at the next poll, not after the harness's 20 s");
    assert_eq!(card.status, "aborted");
    assert!(card.aborted.as_deref().unwrap().starts_with("aborted: real traffic: 1 request(s) came in through the gateway"), "{:?}", card.aborted);
    assert!(card.decode.is_empty() && !PathBuf::from(&card.raw_dir).join("warmup-decode.json").exists(), "the harness was killed before it wrote anything");
    assert!(!bench_run::lock_path(&r.ctx).exists() && !r.ctx.shared.lock().unwrap().bench.active);
    // card #14: a run that DIED still says what it saw. A two-second sample is thin, but
    // "not recorded" on a run killed BY the traffic is the one answer that is simply false -
    // the sample is taken before the abort decision, not after it.
    let b = card.background.clone().expect("an aborted run records what it saw before it died");
    assert!(b.samples > 0 && b.polls_with_traffic > 0, "{b:?}");
    assert_eq!(card.load_class(), lss_core::bench::LoadClass::Loaded);

    // v5.2 gate: a user that is not lss-bench has a request in flight
    let r = rig(20, |_| {});
    r.fake.lock().unwrap().users = Some(vec![("lss-bench".into(), 8)]);
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.ctx.shared.lock().unwrap().bench.active, "its own lss-bench traffic is not a reason to stop");
    r.fake.lock().unwrap().users = Some(vec![("lss-bench".into(), 8), ("acme".into(), 1)]);
    let card = r.wait();
    assert!(card.aborted.as_deref().unwrap().contains("not the bench (gateway user table)"), "{:?}", card.aborted);

    // `lss bench cancel`
    let r = rig(20, |_| {});
    assert!(!bench_run::request_cancel(&r.ctx).1.ok, "nothing to cancel");
    assert!(r.start("quick", false).1.ok);
    assert!(bench_run::request_cancel(&r.ctx).1.ok);
    assert_eq!(r.wait().aborted.as_deref(), Some("cancelled by the operator"));
}

/// Card #207, 2026-09-23 - THE BENCH WAS SHOOTING ITSELF. Reproduced by verifier-8 on the live
/// box: one 1-token POST straight to the engine moved `num_requests_total{priority="0"}` 32 ->
/// 33, because SGLang stamps `priority="0"` on a request that asked for no priority at all and
/// `"0"` is exactly this box's TRUSTED lane. The harness sends its load direct to the engine, so
/// the lane counter the abort watch was reading as "somebody came in through the gateway" held
/// the bench's OWN requests, and two plain runs died in about two seconds each while the
/// gateway's authoritative user table reported zero other users.
///
/// The rig below is that box: the bench's own request sitting in the trusted lane, no --force and
/// no --under-load, and the run has to finish.
#[test]
fn the_benchs_own_direct_to_engine_load_is_not_counted_as_gateway_traffic() {
    let r = rig(1, |_| {});
    // a v5.2 gate from the first poll, answering the question authoritatively: nobody but the bench
    r.fake.lock().unwrap().users = Some(vec![("lss-bench".into(), 0)]);
    assert!(r.start("quick", false).1.ok, "a quiet box starts a plain run");
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        // what the engine shows WHILE the harness runs: the bench's own request, under priority="0"
        f.running = 1.0;
        f.gateway_trusted = 1.0;
    }
    let card = r.wait();
    assert_eq!(card.status, "ok", "#207: a plain run must survive its own traffic: {:?}", card.aborted);
    let b = card.background.clone().expect("the run still records what it looked at");
    assert_eq!((b.polls_with_traffic, b.concurrent_max, b.source.as_str()), (0, Some(0.0), lss_core::bench::LOAD_SRC_USERS));
    assert_eq!(card.load_class(), lss_core::bench::LoadClass::Quiet, "and it is entitled to say the box was quiet");

    // the same box with a gate too old to publish a user table: the trusted lane ALONE must
    // still not convict the bench of being somebody else
    let r = rig(1, |_| {});
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 1.0;
        f.gateway_trusted = 1.0;
    }
    assert_eq!(r.wait().status, "ok", "#207: no user table is not a licence to read the bench's own lane as traffic");

    // THE TEETH: one more request in that same lane than the bench could have launched IS
    // somebody else, and still kills the run at the next poll. Without this, deleting the abort
    // watch outright would pass both assertions above.
    let r = rig(20, |_| {});
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 2.0;
        f.gateway_trusted = 2.0;
    }
    let card = r.wait();
    assert_eq!(card.status, "aborted");
    assert!(card.aborted.as_deref().unwrap().contains("1 request(s) came in through the gateway"), "{:?}", card.aborted);
}

/// card #212 (verifier-9 on #207): a box whose PUBLIC lane carries the engine's default
/// priority "0". The harness's own priority-less request lands in that lane's counter, and
/// #207's hard-wired "the bench's lane is trusted" read it as a stranger and aborted the run on
/// its own traffic again. The rig swaps the lane config; the fake engine's priority="0" row is
/// where the bench's request sits (its field is named for the DEFAULT config's lane).
#[test]
fn a_box_whose_public_lane_is_priority_0_does_not_abort_on_the_benchs_own_load() {
    let swapped = |c: &mut Config| {
        c.public_priority = "0".into();
        c.trusted_priority = "10".into();
    };
    let r = rig(1, swapped);
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 1.0;
        f.gateway_trusted = 1.0; // the priority="0" row = THIS box's public lane
    }
    let card = r.wait();
    assert_eq!(card.status, "ok", "#212: the bench's own request in the priority-0 PUBLIC lane is not foreign: {:?}", card.aborted);
    // teeth: one request more than the bench launched, in that same lane, still aborts
    let r = rig(20, swapped);
    assert!(r.start("quick", false).1.ok);
    std::thread::sleep(Duration::from_millis(200));
    {
        let mut f = r.fake.lock().unwrap();
        f.running = 2.0;
        f.gateway_trusted = 2.0;
    }
    assert_eq!(r.wait().status, "aborted");
}

/// card #213 (verifier-9): a bench request that times out on the CLIENT keeps running on the
/// engine. It used to fall out of the bench's own count the moment the client gave up (after a
/// one-poll grace), so the next poll read the engine still serving it as somebody else's traffic
/// and aborted the run. It now stays the bench's until the engine's own total shows it gone -
/// and a stranger who arrives AFTER that is still caught.
#[test]
fn a_request_abandoned_client_side_stays_the_benchs_until_the_engine_finishes_it() {
    use lss_core::bench::{abort_reason, OwnLane, SafetyObs};
    let poll = Duration::from_millis(40);
    let t0 = Instant::now();
    let mut own = bench_run::OwnLoad::new(t0);
    let obs = |own: u64, running: f64| SafetyObs { running: Some(running), queue: Some(0.0), gateway_public: Some(0.0), gateway_trusted: Some(running), other_users_inflight: None, own_gateway_inflight: own, own_lane: OwnLane::Trusted };
    // one sanity request in flight: the engine shows it, it is ours
    own.start(1);
    assert_eq!(own.count(t0, Some(1.0)), 1);
    // it times out CLIENT-side: the call returns status 0, but the engine is still running it
    own.finish(t0, poll, 1);
    let later = t0 + poll * 10; // well past the one-poll grace
    let n = own.count(later, Some(1.0));
    assert_eq!(n, 1, "the abandoned request is still the bench's");
    assert_eq!(abort_reason(&obs(n, 1.0), 0), None, "#213: it must not read as somebody else's traffic");
    // the engine finishes it: the attribution clears
    assert_eq!(own.count(later, Some(0.0)), 0);
    // teeth: a real stranger arriving after that IS caught
    let n = own.count(later + poll, Some(1.0));
    assert_eq!(n, 0);
    assert!(abort_reason(&obs(n, 1.0), 0).is_some(), "once the bench's request is gone, one running request is a stranger");
    // and an abandoned request never hides more than itself: one abandoned, two running = one stranger
    let mut own = bench_run::OwnLoad::new(t0);
    own.start(1);
    own.finish(t0, poll, 1);
    let n = own.count(later, Some(2.0));
    assert!(abort_reason(&obs(n, 2.0), 0).is_some(), "two running, one of them the bench's: the other one aborts");
}

#[test]
fn one_bench_at_a_time() {
    let r = rig(3, |_| {});
    assert!(r.start("quick", false).1.ok);
    let (code, second) = r.start("full", true);
    assert_eq!((code, second.ok), (409, false));
    assert!(second.message.contains("already running (quick)"), "{}", second.message);
    assert!(bench_run::lock_path(&r.ctx).exists());
    assert!(bench_run::request_cancel(&r.ctx).1.ok);
    r.wait();

    // the lock FILE: another live process holds it (here: this one) -> refused; a dead one -> taken over
    let r = rig(0, |_| {});
    bench_run::take_lock(&r.ctx, "full", crate::unix_now() - 90).unwrap();
    let (code, no) = r.start("quick", false);
    assert_eq!(code, 409);
    assert!(no.message.contains("already running (full, started 9") && no.message.contains("pid"), "{}", no.message);
    assert!(r.ctx.db.lock().unwrap().bench_runs(5).unwrap().is_empty() && !r.ctx.shared.lock().unwrap().bench.active);
    std::fs::write(bench_run::lock_path(&r.ctx), "{\"pid\": 2147483000, \"profile\": \"quick\", \"started_at\": 5}").unwrap();
    assert!(r.start("quick", false).1.ok, "a lock left by a process that no longer exists is taken over");
    assert_eq!(r.wait().status, "ok");
    // a collector that died mid-run: the next start closes the run and clears the lock
    let r = rig(0, |_| {});
    let db = r.ctx.db.lock().unwrap();
    db.bench_start("aaaa11112222", "full", 100, &lss_core::bench::Scorecard { status: "running".into(), started_at: 100, ..Default::default() }).unwrap();
    drop(db);
    std::fs::write(bench_run::lock_path(&r.ctx), "{\"pid\": 2147483000}").unwrap();
    bench_run::recover(&r.ctx, 400);
    let left = r.ctx.db.lock().unwrap().bench_runs(1).unwrap().remove(0);
    assert_eq!((left.status.as_str(), left.duration_s, bench_run::lock_path(&r.ctx).exists()), ("aborted", 300, false));
    assert!(left.aborted.unwrap().contains("the collector restarted"));
}

#[test]
fn a_profile_has_a_hard_timeout() {
    let r = rig(30, |c| c.bench.quick_timeout_secs = 1);
    assert!(r.start("quick", false).1.ok);
    let t0 = Instant::now();
    let card = r.wait();
    assert!(t0.elapsed() < Duration::from_secs(6), "{:?}", t0.elapsed());
    assert_eq!(card.status, "timeout");
    assert!(card.aborted.as_deref().unwrap().starts_with("timeout after 1s (the `quick` profile's hard limit)"), "{:?}", card.aborted);
    assert!(!bench_run::lock_path(&r.ctx).exists());
}

#[test]
fn full_adds_the_needle_accuracy_stores_the_score_and_the_dry_run_sends_nothing() {
    let r = rig(0, |c| c.bench.needle_tokens = 2_000);
    assert!(r.start("full", false).1.ok);
    let card = r.wait();
    assert_eq!(card.status, "ok", "{:?}", card.aborted);
    assert_eq!(card.needle.iter().map(|n| (n.depth_pct, n.pass, n.prompt_tokens)).collect::<Vec<_>>(), vec![(10, true, 251_000), (50, true, 251_000), (90, true, 251_000)]);
    assert_eq!(r.fake.lock().unwrap().chats.len(), 12, "3 sanity checks + the needle at 3 depths + the 6 garbled-output prompts");

    // a gateway that refuses a prompt this large: the needle goes straight to the engine, and says so
    let r = rig(0, |c| c.bench.needle_tokens = 2_000);
    r.fake.lock().unwrap().chat_status = Some(413);
    assert!(r.start("full", false).1.ok);
    let card = r.wait();
    assert!(card.sanity.iter().all(|c| !c.pass && c.detail.starts_with("HTTP 413")), "{:?}", card.sanity);
    assert!(card.needle.iter().all(|n| n.detail.contains("the gateway refused a prompt this large")), "{:?}", card.needle);

    let r = rig(0, |_| {});
    assert!(r.start("accuracy", false).1.ok);
    let card = r.wait();
    assert_eq!(card.status, "ok", "{:?}", card.aborted);
    let a = &card.accuracy[0];
    assert_eq!((a.dataset.as_str(), a.score, a.n, a.correct), ("gsm8k", 0.94, 200, 188));
    assert!(a.file.ends_with("accuracy-gsm8k.json") && std::path::Path::new(&a.file).is_file(), "the file `lss compare --accuracy` hands to the harness: {}", a.file);

    let r = rig(0, |_| {});
    assert!(r.start("dry-run", false).1.ok);
    let card = r.wait();
    assert_eq!((card.status.as_str(), card.decode.len(), r.fake.lock().unwrap().chats.len()), ("ok", 0, 0), "{:?}", card.aborted);
    assert!(r.ctx.shared.lock().unwrap().bench.brief.last.is_none(), "a dry run is not a benchmark result");
}

#[test]
fn a_missing_harness_is_a_clear_error_not_a_crash() {
    // no harness at all: quick / full fall back to the built-in mini bench (its own test); what
    // only the harness can do says so
    let r = rig(0, |c| c.bench.harness = String::new());
    let (code, no) = r.start("accuracy", false);
    assert_eq!(code, 409);
    assert!(no.message.contains("not set up") && no.message.contains("[bench] harness"), "{}", no.message);
    let r = rig(0, |c| c.bench.harness = "/nowhere/llm_decode_bench.py".into());
    assert!(r.start("quick", false).1.message.contains("is not at /nowhere/llm_decode_bench.py"));
    // the interpreter is missing: the run is recorded as failed, with the reason
    let r = rig(0, |c| c.bench.python = "/nowhere/python3".into());
    assert!(r.start("quick", false).1.ok);
    let card = r.wait();
    assert_eq!(card.status, "failed");
    assert!(card.aborted.as_deref().unwrap().contains("cannot start `/nowhere/python3"), "{:?}", card.aborted);
}

/// No external harness (a stranger's machine): `lss bench quick` still gives a scorecard - lss's
/// own requests over the OpenAI chat API, straight at the engine when there is no gateway.
#[test]
fn without_a_harness_the_built_in_mini_bench_gives_a_scorecard() {
    let r = rig(0, |c| {
        c.bench.harness = String::new();
        c.gate_url = String::new();
        c.engine_kind = "sglang".into();
    });
    let (code, started) = r.start("quick", false);
    assert!(code == 202 && started.ok, "{}", started.message);
    let card = r.wait();
    // card #334: the run's own wall time, to the millisecond (a fast fake engine takes < 1 s)
    let ms = card.duration_ms.expect("duration_ms is recorded");
    assert!(ms >= 0 && ms / 1000 == card.duration_s, "duration_ms {ms} agrees with duration_s {}", card.duration_s);
    assert_eq!(card.status, "ok", "{:?}", card.aborted);
    assert!(card.target.starts_with("built-in mini bench (no external harness configured)"), "{}", card.target);
    // card #84: with no gateway configured the target must NOT name a gateway port that
    // does not exist, the clauses must be separated, and the engine-direct route is stated.
    assert!(!card.target.contains("trusted port ()"), "no empty gateway URL: {}", card.target);
    assert!(!card.target.contains("the gateway's trusted port"), "no gateway, no gateway clause: {}", card.target);
    assert!(card.target.contains("no gateway configured"), "{}", card.target);
    assert!(card.target.contains(" · "), "clauses are separated: {}", card.target);
    assert_eq!(card.decode.iter().map(|c| (c.concurrency, c.errors)).collect::<Vec<_>>(), vec![(1, 0), (2, 0), (4, 0), (8, 0)], "1 / 2 / 4 / 8 at once, capped at the 8 slots");
    assert!(card.decode.iter().all(|c| c.per_user_tok_s > 0.0 && c.total_tok_s > 0.0), "{:?}", card.decode);
    assert_eq!(card.prefill.len(), 2, "two prompt sizes: {:?}", card.prefill);
    assert!(card.prefill.iter().all(|p| p.tok_s > 0.0 && p.tokens > 0));
    assert_eq!(card.sanity.len(), 3);
    let chats = r.fake.lock().unwrap().chats.clone();
    assert_eq!(chats.len(), 1 + (1 + 2 + 4 + 8) + 2 + 3, "warm-up + the sweep + two prompts + the sanity checks");
    assert!(chats.iter().all(|(header, _)| header.as_deref() == Some("1")), "every request says it is a benchmark");
    // accuracy needs the harness's datasets: said plainly, nothing is sent
    let r = rig(0, |c| c.bench.harness = String::new());
    let (code, refused) = r.start("accuracy", false);
    assert!(code == 409 && !refused.ok && refused.message.contains("harness"), "{}", refused.message);
    assert!(r.fake.lock().unwrap().chats.is_empty());
}

/// card #233 (lss-verifier-3 on #213): `timed_out()` is the ONLY thing that turns a status-0
/// reply into "abandoned, still running on the engine", and both request paths return status 0
/// for ANY transport error. So it must say yes only for a request that ran out its client
/// timeout (ureq's deadline fires at, or a hair under, the configured value) and no for an
/// error that came back early - a refused connection, a DNS failure, a reset. Booking one of
/// those as abandoned would keep a phantom bench request in the count and hide real traffic
/// from the abort watch.
#[test]
fn only_a_request_that_ran_out_its_client_timeout_counts_as_abandoned() {
    let timeout = Duration::from_secs(120);
    // ran out the clock: exactly at, just past, and inside the 1s tolerance below it
    for secs in [120.0, 120.3, 119.9, 119.2, 119.0] {
        assert!(bench_run::timed_out(secs, timeout), "{secs}s of a 120s timeout is the client giving up");
    }
    // came back early with no status: a connection error, not an abandoned request
    for secs in [0.002, 0.05, 3.0, 5.0, 60.0, 118.9] {
        assert!(!bench_run::timed_out(secs, timeout), "{secs}s of a 120s timeout is an early error, not an abandoned request");
    }
    // the tolerance is one second, not a fraction of the timeout: a short step still works
    assert!(bench_run::timed_out(9.5, Duration::from_secs(10)));
    assert!(!bench_run::timed_out(0.01, Duration::from_secs(10)));
}

/// card #236 (residual of #233): an error that lands INSIDE the 1 s tolerance but is not the
/// client's timeout - here the far end drops the connection 0.5 s before a 2 s timeout - is a
/// request the engine is no longer running, and must not be booked as abandoned. Real sockets
/// and the real ureq agent, so what is pinned is ureq's actual error, not a hand-built one.
#[test]
fn a_connection_dropped_just_before_the_timeout_is_not_an_abandoned_request() {
    use std::io::Read as _;
    use std::net::TcpListener;
    let post = |url: &str, timeout: Duration| {
        let started = Instant::now();
        let err = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(timeout).build().post(url).send_string("{}").expect_err("no response is ever sent");
        (bench_run::client_timed_out(&err), started.elapsed().as_secs_f64(), err.to_string())
    };
    let timeout = Duration::from_secs(2);

    // 1. the far end reads the request, waits 1.5 s and closes: inside the tolerance, not a timeout
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/chat/completions", l.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let _ = s.read(&mut [0u8; 4096]);
        std::thread::sleep(Duration::from_millis(1500));
        drop(s);
    });
    let (gave_up, secs, err) = post(&url, timeout);
    server.join().unwrap();
    assert!(bench_run::timed_out(secs, timeout), "the case #236 is about: {secs}s is inside the 1 s tolerance of 2 s ({err})");
    assert!(!gave_up, "a dropped connection is not the client's timeout: {err}");
    assert!(!bench_run::abandoned(0, gave_up, secs, timeout), "a request the engine dropped at {secs}s is not still running on it: {err}");

    // 2. the far end never answers: the client's own deadline fires, the request is abandoned
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/chat/completions", l.local_addr().unwrap());
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let _ = s.read(&mut [0u8; 4096]);
        let _ = stop_rx.recv();
    });
    let (gave_up, secs, err) = post(&url, Duration::from_secs(1));
    let _ = stop_tx.send(());
    server.join().unwrap();
    assert!(gave_up, "ureq's deadline is a TimedOut io error: {err}");
    assert!(bench_run::abandoned(0, gave_up, secs, Duration::from_secs(1)), "{secs}s: {err}");

    // 3. nothing listening: refused at once, not a timeout, not abandoned (card #323: the port
    // is HELD, never listening - a released number could be handed to another test's listener)
    let held = crate::test_ports::refusing_port();
    let port = held.port;
    let (gave_up, secs, err) = post(&format!("http://127.0.0.1:{port}/"), timeout);
    assert!(!gave_up && !bench_run::abandoned(0, gave_up, secs, timeout), "{err}");

    // a real HTTP status is never abandoned, however long it took
    assert!(!bench_run::abandoned(504, true, 2.0, timeout));
}

/// card #312: a stand-in for `llm_decode_bench.py`, in Python like the real one (lss runs a keyed
/// step through `python -c KEY_SHIM harness`, which needs a Python harness). It behaves like the
/// REAL harness on the points that matter, as measured by the verifier:
///   * `--api-key K` becomes `Authorization: Bearer K` on its requests to `--host`/`--port`;
///   * on a 401 it does NOT fail: it prints the warning the real one prints, measures, says
///     'Done.' and exits 0.
///
/// It also writes down its own OS-level command line and environment (/proc/self, on Linux), so
/// the test can prove the key never reached either.
fn keyed_harness(r: &Rig) {
    let script = format!(
        r#"import os, shutil, sys, urllib.request, urllib.error
a = sys.argv[1:]
def opt(n):
    return a[a.index(n) + 1] if n in a else ""
if "--help" in a:
    print("usage"); sys.exit(0)
for name in ("cmdline", "environ"):
    p = "/proc/self/" + name
    if os.path.exists(p):
        with open(p, "rb") as f, open("{d}/seen-%s-%d" % (name, os.getpid()), "wb") as o:
            o.write(f.read())
key = opt("--api-key")
req = urllib.request.Request("http://%s:%s/v1/models" % (opt("--host"), opt("--port")), headers={{"User-Agent": "fake-harness"}})
if key:
    req.add_header("Authorization", "Bearer " + key)
try:
    urllib.request.urlopen(req, timeout=10).read()
except urllib.error.HTTPError as e:
    print("WARNING: Only OpenAI /v1 endpoints appear reachable (HTTP %d on /v1/models)" % e.code)
shutil.copy("{d}/sample.json", opt("--output"))
print("Done.")
"#,
        d = r.dir.display()
    );
    std::fs::write(r.dir.join("fake_harness.sh"), script).unwrap();
}

fn python3() -> bool {
    std::process::Command::new("python3").arg("-c").arg("import urllib.request").output().is_ok_and(|o| o.status.success())
}

#[test]
fn a_keyed_engine_gets_its_key_on_stdin_and_nowhere_else_sees_it() {
    // card #312: the collector's own requests carried the [[engine]] api_key (collect::authed),
    // but the external harness got only the engine URL - so on a keyed engine every harness step
    // was refused. The key now reaches it on STDIN (bench::KEY_SHIM), never on its command line.
    if !python3() {
        eprintln!("SKIPPED: no python3 here - this test runs a Python harness stand-in");
        return;
    }
    let key = "test-bench-value-7321";
    let r = rig(0, |c| {
        c.engine_api_key = key.into();
        c.bench.python = "python3".into();
    });
    keyed_harness(&r);
    r.fake.lock().unwrap().required_key = Some(key.into());
    let (code, started) = r.start("quick", false);
    assert_eq!((code, started.ok), (202, true), "{}", started.message);
    let card = r.wait();
    assert_eq!((card.status.as_str(), card.aborted.clone()), ("ok", None), "the harness authenticated");
    let fake = r.fake.lock().unwrap();
    assert!(!fake.models_auth.is_empty() && fake.models_auth.iter().all(|a| a.as_deref() == Some(&format!("Bearer {key}")[..])), "every harness request to the engine carries the key: {:?}", fake.models_auth);
    // lss's own sanity checks go through the GATEWAY: the engine's key never goes there
    assert!(!fake.chat_auth.is_empty() && fake.chat_auth.iter().all(Option::is_none), "no Authorization to the gateway: {:?}", fake.chat_auth);
    drop(fake);
    // not on the harness's command line, not in its environment (read from /proc by the harness itself)
    let seen: Vec<PathBuf> = std::fs::read_dir(&r.dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("seen-"))).collect();
    if cfg!(target_os = "linux") {
        assert!(seen.iter().any(|p| p.to_string_lossy().contains("seen-cmdline")), "the harness recorded its command line: {seen:?}");
    }
    for p in &seen {
        let bytes = std::fs::read(p).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(key), "{} holds the key", p.display());
    }
    // and on nothing lss stores or shows
    assert!(!serde_json::to_string(&card).unwrap().contains(key), "the scorecard must not carry the key");
    let log = std::fs::read_to_string(PathBuf::from(&card.raw_dir).join("harness.log")).unwrap_or_default();
    assert!(!log.contains(key) && log.contains("Done."), "harness.log: {log}");
}

#[test]
fn a_keyed_engine_that_refuses_the_harness_fails_the_run_though_the_harness_exits_0() {
    // the REAL harness with no key: 'WARNING ... (HTTP 401 on /metrics)' ... 'Done.', exit 0. A
    // refused run is not a measurement: lss reads the refusal in the log and fails the run, and
    // says which fix applies - no key configured, or a key that was refused.
    if !python3() {
        eprintln!("SKIPPED: no python3 here");
        return;
    }
    for (configured, hint) in [("", "set api_key in its [[engine]] block"), ("test-wrong-value-0000", "api_key was refused")] {
        let r = rig(0, |c| {
            c.engine_api_key = configured.into();
            c.bench.python = "python3".into();
        });
        keyed_harness(&r);
        // the preflight is let through here, so this is the BACKSTOP behind it (card #312 FAIL 1)
        r.fake.lock().unwrap().harness_only = true;
        r.fake.lock().unwrap().required_key = Some("test-bench-value-7321".into());
        let (code, started) = r.start("quick", false);
        assert_eq!((code, started.ok), (202, true), "{}", started.message);
        let card = r.wait();
        let why = card.aborted.clone().unwrap_or_default();
        assert_eq!(card.status, "failed", "a 401 from the engine is not a benchmark ({configured:?}): {why}");
        assert!(why.contains("HTTP 401") && why.contains(hint), "{configured:?}: {why}");
        let log = std::fs::read_to_string(PathBuf::from(&card.raw_dir).join("harness.log")).unwrap_or_default();
        assert!(log.contains("Done."), "the stand-in exited 0 like the real one: {log}");
    }
}

#[test]
fn a_missing_or_wrong_key_is_stopped_by_the_preflight_before_the_harness_starts() {
    // card #312 verifier FAIL 1: the REAL harness, refused, prints "SGLang metrics are disabled",
    // ERROR cells, "Done." and exits 0 - no "401" to find. So lss asks the engine itself, with the
    // key it would hand the harness, before the first step that talks to the engine.
    if !python3() {
        eprintln!("SKIPPED: no python3 here");
        return;
    }
    let right = "test-bench-value-7321";
    for (configured, hint) in [("", "none is configured"), ("test-wrong-value-0000", "refused the configured key")] {
        let r = rig(0, |c| {
            c.engine_api_key = configured.into();
            c.bench.python = "python3".into();
        });
        keyed_harness(&r);
        r.fake.lock().unwrap().required_key = Some(right.into());
        let (code, started) = r.start("quick", false);
        assert_eq!((code, started.ok), (202, true), "{}", started.message);
        let card = r.wait();
        let why = card.aborted.clone().unwrap_or_default();
        assert_eq!(card.status, "failed", "{configured:?}: {why}");
        assert!(why.contains("HTTP 401 on /v1/models") && why.contains(hint) && why.contains("[[engine]]") && why.contains("api_key"), "{configured:?}: {why}");
        assert!(!configured.is_empty() || !why.contains("test-"), "{why}");
        assert!(configured.is_empty() || !why.contains(configured), "the hint never shows the key: {why}");
        assert!(!card.complete(), "a failed run is not complete: `lss bench` exits 1 on it");
        // the harness never ran: no harness.log, and it never recorded itself
        assert!(!PathBuf::from(&card.raw_dir).join("harness.log").exists(), "the harness was started anyway");
        assert!(!std::fs::read_dir(&r.dir).unwrap().filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("seen-")), "the harness ran");
        let fake = r.fake.lock().unwrap();
        let expect = if configured.is_empty() { None } else { Some(format!("Bearer {configured}")) };
        assert_eq!(fake.preflight, vec![("/v1/models".to_string(), expect)], "one preflight GET, with exactly the configured key");
    }
    // the right key: the preflight passes (GET /v1/models, then a 1-token chat, both keyed) and the run proceeds
    let r = rig(0, |c| {
        c.engine_api_key = right.into();
        c.bench.python = "python3".into();
    });
    keyed_harness(&r);
    r.fake.lock().unwrap().required_key = Some(right.into());
    let (code, _) = r.start("quick", false);
    assert_eq!(code, 202);
    let card = r.wait();
    assert_eq!(card.status, "ok", "{:?}", card.aborted);
    let fake = r.fake.lock().unwrap();
    let bearer = Some(format!("Bearer {right}"));
    assert_eq!(fake.preflight, vec![("/v1/models".to_string(), bearer.clone()), ("/v1/chat/completions".to_string(), bearer)]);
}

#[test]
fn an_engine_that_guards_only_inference_is_caught_by_the_preflight_chat() {
    // /v1/models is open, only chat wants the key: the 1-token authed request is what finds it
    let r = rig(0, |c| c.bench.python = "/bin/sh".into());
    {
        let mut f = r.fake.lock().unwrap();
        f.required_key = Some("test-chat-only-value".into());
        f.guard_chat_only = true;
    }
    let err = bench_run::preflight(&r.ctx, "model-a").expect_err("the chat is refused");
    assert!(err.contains("HTTP 401 on /v1/chat/completions") && err.contains("none is configured"), "{err}");
    let paths: Vec<String> = r.fake.lock().unwrap().preflight.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(paths, ["/v1/models", "/v1/chat/completions"]);
}

/// card #330: `[bench] launcher = ["ssh", host]` - ssh does not run the command in lss's run
/// directory, it runs it in the REMOTE shell's cwd (the remote user's HOME), so every relative
/// `--output decode.json` landed there and lss found nothing ('the harness finished but wrote no
/// result this version of lss understands'). The results must come back to the run directory,
/// and the remote side must be left clean.
fn over_ssh(launcher: Vec<String>, remote_ls: impl Fn(&str) -> String) {
    over_ssh_with(launcher, None, remote_ls);
}

/// card #333: `tilde` = Some(put the harness there): `[bench] harness = "~/fake_harness.sh"`, the
/// file exists ONLY in the remote user's HOME (the rig's own home, /nonexistent, has nothing).
fn over_ssh_with(launcher: Vec<String>, tilde: Option<&dyn Fn(&std::path::Path)>, remote_ls: impl Fn(&str) -> String) -> lss_core::bench::Scorecard {
    let r = rig(0, |c| {
        c.bench.launcher = launcher;
        if tilde.is_some() {
            c.bench.harness = "~/fake_harness.sh".into();
        }
    });
    if let Some(place) = tilde {
        place(&r.dir.join("fake_harness.sh"));
    }
    let (code, started) = r.start("quick", false);
    assert_eq!((code, started.ok), (202, true), "{}", started.message);
    let card = r.wait();
    let raw = PathBuf::from(&card.raw_dir);
    let log = std::fs::read_to_string(raw.join("harness.log")).unwrap_or_default();
    assert_eq!((card.status.as_str(), card.aborted.clone()), ("ok", None), "{card:?}\nharness.log:\n{log}");
    assert_eq!(card.headline().c1_tok_s, Some(86.8), "the REAL sample file came back and was read");
    assert!(raw.join("decode.json").is_file() && raw.join("prefill.json").is_file(), "the results are in the local run dir: {}", raw.display());
    assert_eq!(remote_ls("decode.json prefill.json"), "", "nothing was left in the remote HOME");
    assert_eq!(remote_ls(".cache/lss-bench"), "", "the per-run remote directory was removed after the copy");
    card
}

#[test]
fn over_an_ssh_launcher_the_results_come_back_to_the_run_directory() {
    // a stand-in `ssh` that behaves as ssh does: argv after the host is JOINED with spaces and run
    // by the remote user's shell, starting in the remote user's HOME (here: a separate directory)
    let d = std::env::temp_dir().join(format!("lss-ssh-fake-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let (bin, remote) = (d.join("bin"), d.join("remote-home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let ssh = bin.join("ssh");
    crate::exec_file::write_exec(&ssh, &format!("#!/bin/sh\nshift\ncd \"{r}\" && HOME=\"{r}\" exec /bin/sh -c \"$*\"\n", r = remote.display()));
    over_ssh(vec![ssh.display().to_string(), "gpu-box".into()], |what| {
        what.split(' ').filter(|w| remote.join(w).exists() && std::fs::read_dir(remote.join(w)).map_or(true, |mut e| e.next().is_some())).collect::<Vec<_>>().join(" ")
    });
    let _ = std::fs::remove_dir_all(&d);
}

/// card #330, against a REAL sshd: `LSS_TEST_SSH=user@host` (key login, a remote user whose HOME
/// is not this one). Skipped without it - no sshd in the ordinary build.
#[test]
fn over_a_real_ssh_launcher_the_results_come_back_to_the_run_directory() {
    let Ok(target) = std::env::var("LSS_TEST_SSH") else {
        eprintln!("SKIPPED: set LSS_TEST_SSH=user@host (a real sshd, key login) to run this");
        return;
    };
    let _one_at_a_time = REAL_SSH.lock().unwrap_or_else(|e| e.into_inner());
    let ssh: Vec<String> = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", &target].map(String::from).to_vec();
    let remote_ls = |what: &str| {
        let mut c = std::process::Command::new(&ssh[0]);
        c.args(&ssh[1..]).arg(format!("for w in {what}; do [ -e \"$w\" ] && [ -n \"$(ls -A \"$w\" 2>/dev/null || echo file)\" ] && printf '%s ' \"$w\"; done; true"));
        String::from_utf8_lossy(&c.output().expect("ssh runs").stdout).trim().to_string()
    };
    assert_eq!(remote_ls("/"), "/", "the ssh login itself works (LSS_TEST_SSH={target})");
    over_ssh(ssh.clone(), remote_ls);
}

/// card #333: `[bench] harness = "~/…"` over an ssh launcher means the REMOTE user's home. It was
/// expanded with the collector's own home before the argv went over ssh ('the benchmark harness
/// is not at /nonexistent/fake_harness.sh', or the wrong file on a shared path).
#[test]
fn over_an_ssh_launcher_a_tilde_harness_path_is_the_remote_users_home() {
    let d = std::env::temp_dir().join(format!("lss-ssh-tilde-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let (bin, remote) = (d.join("bin"), d.join("remote-home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let ssh = bin.join("ssh");
    crate::exec_file::write_exec(&ssh, &format!("#!/bin/sh\nshift\ncd \"{r}\" && HOME=\"{r}\" exec /bin/sh -c \"$*\"\n", r = remote.display()));
    let launcher = vec![ssh.display().to_string(), "gpu-box".into()];
    let place = |local: &std::path::Path| {
        std::fs::copy(local, remote.join("fake_harness.sh")).unwrap();
    };
    over_ssh_with(launcher.clone(), Some(&place), |what| what.split(' ').filter(|w| remote.join(w).exists() && std::fs::read_dir(remote.join(w)).map_or(true, |mut e| e.next().is_some())).collect::<Vec<_>>().join(" "));
    // and a harness that is NOT there on the far side is a clear refusal, naming the remote home
    let r = rig(0, |c| {
        c.bench.launcher = launcher.clone();
        c.bench.harness = "~/no-such-harness.py".into();
    });
    let (code, started) = r.start("quick", false);
    assert_ne!(code, 202, "{}", started.message);
    assert!(started.message.contains("~/no-such-harness.py") && started.message.contains("remote"), "{}", started.message);
    let _ = std::fs::remove_dir_all(&d);
}

/// card #333 against a REAL sshd (`LSS_TEST_SSH=user@host`, a remote user whose HOME is not this
/// one): the harness is copied into THAT user's home and named `~/…`.
#[test]
fn over_a_real_ssh_launcher_a_tilde_harness_path_is_the_remote_users_home() {
    let Ok(target) = std::env::var("LSS_TEST_SSH") else {
        eprintln!("SKIPPED: set LSS_TEST_SSH=user@host (a real sshd, key login) to run this");
        return;
    };
    let _one_at_a_time = REAL_SSH.lock().unwrap_or_else(|e| e.into_inner());
    let ssh: Vec<String> = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", &target].map(String::from).to_vec();
    let run = |remote_cmd: &str, stdin: Option<&[u8]>| -> String {
        use std::io::Write;
        let mut c = std::process::Command::new(&ssh[0]);
        c.args(&ssh[1..]).arg(remote_cmd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        let mut child = c.spawn().expect("ssh runs");
        if let (Some(b), Some(mut i)) = (stdin, child.stdin.take()) {
            i.write_all(b).unwrap();
        }
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).trim().to_string()
    };
    let place = |local: &std::path::Path| {
        run("cat > fake_harness.sh", Some(&std::fs::read(local).unwrap()));
    };
    let remote_ls = |what: &str| run(&format!("for w in {what}; do [ -e \"$w\" ] && [ -n \"$(ls -A \"$w\" 2>/dev/null || echo file)\" ] && printf '%s ' \"$w\"; done; true"), None);
    over_ssh_with(ssh.clone(), Some(&place), remote_ls);
    run("rm -f fake_harness.sh", None);
}


/// card #339: a stand-in `ssh` (argv joined, run by /bin/sh in a separate remote HOME) in a
/// scratch directory; returns (scratch dir, the launcher, the remote HOME).
fn fake_ssh(tag: &str) -> (PathBuf, Vec<String>, PathBuf) {
    let d = std::env::temp_dir().join(format!("lss-ssh-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let (bin, remote) = (d.join("bin"), d.join("remote-home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let ssh = bin.join("ssh");
    crate::exec_file::write_exec(&ssh, &format!("#!/bin/sh\nshift\ncd \"{r}\" && HOME=\"{r}\" exec /bin/sh -c \"$*\"\n", r = remote.display()));
    (d, vec![ssh.display().to_string(), "gpu-box".into()], remote)
}

/// A git checkout with one commit in `dir`; its short HEAD.
fn git_checkout(dir: &std::path::Path) -> String {
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=lss-test", "-c", "user.email=lss-test@invalid", "-c", "commit.gpgsign=false"]).args(args).output().expect("git runs");
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "harness"]);
    git(&["rev-parse", "--short", "HEAD"])
}

/// card #339: over an ssh launcher the harness checkout is on the OTHER machine, but its commit
/// was read with a LOCAL `git -C <harness dir>` - so the scorecard's harness commit was always
/// empty. It comes from the far side through the same launcher; when it cannot be read there the
/// card says so ('unknown (remote)'), never a blank.
#[test]
fn over_an_ssh_launcher_the_harness_commit_is_the_remote_checkouts() {
    let (d, launcher, remote) = fake_ssh("commit");
    let want = git_checkout(&remote);
    let place = |local: &std::path::Path| {
        std::fs::copy(local, remote.join("fake_harness.sh")).unwrap();
    };
    let ls = |what: &str| what.split(' ').filter(|w| remote.join(w).exists() && std::fs::read_dir(remote.join(w)).map_or(true, |mut e| e.next().is_some())).collect::<Vec<_>>().join(" ");
    let card = over_ssh_with(launcher.clone(), Some(&place), ls);
    assert_eq!(card.harness_commit, want, "the REMOTE checkout's commit (~ = the remote HOME)");
    // the harness is there but not in a git checkout: said, not left blank
    std::fs::remove_dir_all(remote.join(".git")).unwrap();
    let card = over_ssh_with(launcher, Some(&place), ls);
    assert_eq!(card.harness_commit, "unknown (remote)");
    let _ = std::fs::remove_dir_all(&d);
}

/// card #339 against a REAL sshd (`LSS_TEST_SSH=user@host`, a remote user whose HOME is not this
/// one): that user's HOME is made a git checkout holding the harness, named `~/fake_harness.sh`.
#[test]
fn over_a_real_ssh_launcher_the_harness_commit_is_the_remote_checkouts() {
    let Ok(target) = std::env::var("LSS_TEST_SSH") else {
        eprintln!("SKIPPED: set LSS_TEST_SSH=user@host (a real sshd, key login) to run this");
        return;
    };
    let _one_at_a_time = REAL_SSH.lock().unwrap_or_else(|e| e.into_inner());
    let ssh: Vec<String> = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", &target].map(String::from).to_vec();
    let run = |remote_cmd: &str, stdin: Option<&[u8]>| -> String {
        use std::io::Write;
        let mut c = std::process::Command::new(&ssh[0]);
        c.args(&ssh[1..]).arg(remote_cmd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        let mut child = c.spawn().expect("ssh runs");
        if let (Some(b), Some(mut i)) = (stdin, child.stdin.take()) {
            i.write_all(b).unwrap();
        }
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).trim().to_string()
    };
    let want = run("git init -q && git -c user.name=lss-test -c user.email=lss-test@invalid -c commit.gpgsign=false commit -q --allow-empty -m harness && git rev-parse --short HEAD", None);
    assert!(want.len() >= 4, "a git checkout in the remote HOME ({target}): got {want:?}");
    let place = |local: &std::path::Path| {
        run("cat > fake_harness.sh", Some(&std::fs::read(local).unwrap()));
    };
    let remote_ls = |what: &str| run(&format!("for w in {what}; do [ -e \"$w\" ] && [ -n \"$(ls -A \"$w\" 2>/dev/null || echo file)\" ] && printf '%s ' \"$w\"; done; true"), None);
    let card = over_ssh_with(ssh.clone(), Some(&place), remote_ls);
    run("rm -rf .git", None);
    let unknown = over_ssh_with(ssh.clone(), Some(&place), remote_ls);
    run("rm -f fake_harness.sh", None);
    assert_eq!(card.harness_commit, want, "the REMOTE checkout's commit");
    assert_eq!(unknown.harness_commit, "unknown (remote)");
}

/// card #342 (#333's check, lss-inst-2's finding): ssh's OWN failure - exit 255, its complaints on
/// stderr (kex noise, "Connection refused"), nothing on stdout - is "could not check", never "the
/// harness is not there". Reading only the exit code, or treating any non-"found" answer as
/// missing, would send the person to clone a harness that may well be there.
#[test]
fn over_an_ssh_launcher_a_failed_ssh_is_never_read_as_a_missing_harness() {
    let d = std::env::temp_dir().join(format!("lss-ssh-255-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    let ssh = d.join("ssh");
    crate::exec_file::write_exec(&ssh, "#!/bin/sh\necho 'kex_exchange_identification: read: Connection reset by peer' >&2\necho 'ssh: connect to host gpu-box port 22: Connection refused' >&2\nexit 255\n");
    let r = rig(0, |c| {
        c.bench.launcher = vec![ssh.display().to_string(), "gpu-box".into()];
        c.bench.harness = "~/x".into();
    });
    let (code, started) = r.start("quick", false);
    assert_ne!(code, 202, "{}", started.message);
    assert!(started.message.contains("could not check"), "{}", started.message);
    assert!(!started.message.contains("is not at"), "a failed ssh read as a missing harness: {}", started.message);
    assert!(started.message.contains("Connection refused"), "ssh's own last word is shown: {}", started.message);
    let _ = std::fs::remove_dir_all(&d);
}

/// card #340: two collectors benching through ONE remote user at the same time. Each has its own
/// database, so both runs are run 1 with the same name ('1-quick-model-a'), and the remote run
/// dir was keyed by that name alone: they shared it, and the first to finish removed it (`rm -rf`)
/// under the second, whose result then could not be copied back. Each run's remote directory must
/// be its own. `a` finishes first (1 s steps), `b` is still inside its step (3 s) when `a` cleans.
fn two_collectors_at_once(launcher: Vec<String>, remote_ls: impl Fn(&str) -> String) {
    let a = rig(1, |c| c.bench.launcher = launcher.clone());
    let b = rig(3, |c| c.bench.launcher = launcher.clone());
    let (ca, sa) = a.start("quick", false);
    let (cb, sb) = b.start("quick", false);
    assert_eq!((ca, sa.ok, cb, sb.ok), (202, true, 202, true), "{} / {}", sa.message, sb.message);
    assert_eq!((sa.run_id, sb.run_id), (Some(1), Some(1)), "two collectors, two databases: the same run id on both");
    for (who, r) in [("a", &a), ("b", &b)] {
        let card = r.wait();
        let log = std::fs::read_to_string(PathBuf::from(&card.raw_dir).join("harness.log")).unwrap_or_default();
        assert_eq!((card.status.as_str(), card.aborted.clone()), ("ok", None), "collector {who}: {card:?}\nharness.log:\n{log}");
        assert_eq!(card.headline().c1_tok_s, Some(86.8), "collector {who}: its own result came back");
    }
    assert_eq!(remote_ls(".cache/lss-bench"), "", "both per-run remote directories were removed");
}

#[test]
fn over_an_ssh_launcher_two_collectors_as_one_remote_user_do_not_share_a_run_dir() {
    let d = std::env::temp_dir().join(format!("lss-ssh-two-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let (bin, remote) = (d.join("bin"), d.join("remote-home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    // the stand-in ssh also logs each remote command, to see which directory each run used
    let ssh = bin.join("ssh");
    crate::exec_file::write_exec(&ssh, &format!("#!/bin/sh\nshift\nprintf '%s\\n' \"$*\" >> \"{l}\"\ncd \"{r}\" && HOME=\"{r}\" exec /bin/sh -c \"$*\"\n", r = remote.display(), l = d.join("ssh.log").display()));
    two_collectors_at_once(vec![ssh.display().to_string(), "gpu-box".into()], |what| {
        what.split(' ').filter(|w| remote.join(w).exists() && std::fs::read_dir(remote.join(w)).map_or(true, |mut e| e.next().is_some())).collect::<Vec<_>>().join(" ")
    });
    let log = std::fs::read_to_string(d.join("ssh.log")).unwrap();
    let mut dirs: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix("mkdir -p ")?.split(' ').next()).collect();
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 2, "one remote directory per run, two runs: {dirs:?}\n{log}");
    let _ = std::fs::remove_dir_all(&d);
}

/// card #340 against a REAL sshd (`LSS_TEST_SSH=user@host`): both collectors log in as that one user.
#[test]
fn over_a_real_ssh_launcher_two_collectors_as_one_remote_user_do_not_share_a_run_dir() {
    let Ok(target) = std::env::var("LSS_TEST_SSH") else {
        eprintln!("SKIPPED: set LSS_TEST_SSH=user@host (a real sshd, key login) to run this");
        return;
    };
    let _one_at_a_time = REAL_SSH.lock().unwrap_or_else(|e| e.into_inner());
    let ssh: Vec<String> = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", &target].map(String::from).to_vec();
    let remote_ls = |what: &str| {
        let mut c = std::process::Command::new(&ssh[0]);
        c.args(&ssh[1..]).arg(format!("for w in {what}; do [ -e \"$w\" ] && [ -n \"$(ls -A \"$w\" 2>/dev/null || echo file)\" ] && printf '%s ' \"$w\"; done; true"));
        String::from_utf8_lossy(&c.output().expect("ssh runs").stdout).trim().to_string()
    };
    two_collectors_at_once(ssh.clone(), remote_ls);
}
