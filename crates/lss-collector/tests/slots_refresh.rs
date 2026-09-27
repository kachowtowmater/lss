//! card #414: the collector's `slots` goes stale after a serve restart. Four tests drive the
//! REAL lss-collector binary against a fake SGLang (the keyed_engine_status.rs rig, minus the
//! key): a restart that changes the SERVED MODEL ID must show the new slots on /status within
//! one poll, the container START TIME changing must do the same (rv-lead-lss 20:46: the
//! restart fake drives the REAL decision code - no hand copy, no docker - over each of the
//! gate's identity keys: served model id, container start time, and the periodic re-read that
//! covers an unchanged identity), a failed fetch must not keep the good value as truth (the M2
//! mutation must die), and a configured `slots` is never second-guessed.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// One fake SGLang. `slots` is an atomic so a test can change what the engine says
/// (a restarted serve, bigger `--max-running-requests`) without restarting the HTTP thread;
/// `broken` makes /get_server_info fail, like an engine still booting; `info_hits` counts every
/// /get_server_info answer, so a test can prove the collector ASKED (or never asked) - no
/// assertion rides on a number the fake merely mirrored. `model` and `start_ts` stand in for
/// the served model id and the serve container's start time: the gate's other two identity
/// keys. `v1models_hits` counts /v1/models (the scrape asks it every poll).
struct SlotsEngine {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
    slots: std::sync::Arc<AtomicU32>,
    broken: std::sync::Arc<AtomicU32>,
    info_hits: std::sync::Arc<AtomicU32>,
    model: std::sync::Arc<std::sync::Mutex<String>>,
    start_ts: std::sync::Arc<AtomicU32>,
    v1models_hits: std::sync::Arc<AtomicU32>,
}

impl SlotsEngine {
    fn start(slots: u32) -> SlotsEngine {
        SlotsEngine::with(("fake-model-414".into(), slots, 1_700_000_000))
    }
    /// `model`: the served id /v1/models publishes; `slots`: max_running_requests;
    /// `start_ts`: the container start time the collector's docker inspect would see.
    fn with((model, slots, start_ts): (String, u32, u32)) -> SlotsEngine {
        let slots = std::sync::Arc::new(AtomicU32::new(slots));
        let broken = std::sync::Arc::new(AtomicU32::new(0));
        let info_hits = std::sync::Arc::new(AtomicU32::new(0));
        let model = std::sync::Arc::new(std::sync::Mutex::new(model));
        let start_ts = std::sync::Arc::new(AtomicU32::new(start_ts));
        let v1models_hits = std::sync::Arc::new(AtomicU32::new(0));
        let (slots2, broken2, hits2, model2, ts2, vm2) = (slots.clone(), broken.clone(), info_hits.clone(), model.clone(), start_ts.clone(), v1models_hits.clone());
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                if req.url() == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                let (code, body) = match req.url() {
                    "/v1/models" => {
                        vm2.fetch_add(1, Ordering::Relaxed);
                        let id = model2.lock().unwrap().clone();
                        (200, format!("{{\"data\":[{{\"id\":\"{id}\"}}]}}"))
                    }
                    // any `/docker/<name>/started_at`: the collector asks by the container NAME
                    // it knows ("" for a plain process) - the start time is per-engine, not per-name
                    u if u.starts_with("/docker/") && u.ends_with("/started_at") => {
                        (200, ts2.load(Ordering::Relaxed).to_string())
                    }
                    "/metrics" => (200, "sglang:num_running_reqs{} 0\nsglang:num_queue_reqs{} 0\n".to_string()),
                    "/get_server_info" => match broken2.load(Ordering::Relaxed) {
                        0 => {
                            hits2.fetch_add(1, Ordering::Relaxed);
                            (200, format!("{{\"version\":\"414.0\",\"max_running_requests\":{}}}", slots2.load(Ordering::Relaxed)))
                        }
                        n => (n, "no".to_string()),
                    },
                    _ => (404, "no".to_string()),
                };
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
            }
        });
        SlotsEngine { port, handle: Some(handle), slots, broken, info_hits, model, start_ts, v1models_hits }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for SlotsEngine {
    fn drop(&mut self) {
        let _ = ureq::get(&format!("http://127.0.0.1:{}/stop", self.port)).call();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Collector {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Collector {
    /// `recheck_secs`: the `LSS_SLOTS_RECHECK_SECS` override this run's collector sees (None =
    /// the 300 s default). The card's own e2e knob - the REAL poll loop reads it on every poll.
    fn start(name: &str, engine: &str, recheck_secs: Option<i64>) -> Collector {
        Self::named(name, engine, recheck_secs, "")
    }
    /// `slots_cfg`: a literal `slots = N` line in the collector's config (the operator's word).
    fn named(name: &str, engine: &str, recheck_secs: Option<i64>, slots_cfg: &str) -> Collector {
        let dir = std::env::temp_dir().join(format!("lss-414-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = format!(
            "listen = [\"127.0.0.1:{port}\"]\ndb_path = \"{d}/lss.db\"\npoll_secs = 1\nserve_container = \"\"\nalert_cmd = \"/bin/true\"\n{slots_cfg}\n\n[probe]\nenabled = false\n\n[[engine]]\nkind = \"sglang\"\nurl = \"{engine}\"",
            d = dir.display(),
            slots_cfg = if slots_cfg.is_empty() { String::new() } else { format!("{slots_cfg}\n") }
        );
        std::fs::write(dir.join("collector.toml"), cfg).unwrap();
        let log = std::fs::File::create(dir.join("collector.log")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lss-collector"));
        cmd.arg("--config").arg(dir.join("collector.toml")).env("HOME", &dir);
        if let Some(secs) = recheck_secs {
            cmd.env("LSS_SLOTS_RECHECK_SECS", secs.to_string());
        } else {
            cmd.env_remove("LSS_SLOTS_RECHECK_SECS");
        }
        let child = cmd.stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
        Collector { child, port, dir }
    }

    fn status(&self) -> serde_json::Value {
        ureq::get(&format!("http://127.0.0.1:{}/status", self.port))
            .call()
            .ok()
            .and_then(|r| serde_json::from_str(&r.into_string().unwrap_or_default()).ok())
            .unwrap_or_default()
    }

    /// /status once `done` says so (with the log on a timeout, like the #331 rig)
    fn status_when(&self, done: impl Fn(&serde_json::Value) -> bool, what: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            last = self.status();
            if done(&last) {
                return last;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        panic!("no such /status within 40 s ({what}); last: {last}\ncollector log:\n{}", std::fs::read_to_string(self.dir.join("collector.log")).unwrap_or_default());
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Repro 2026-09-25: a serve restarted with --max-running-requests 3 -> 8 kept saying 3/3.
/// The fake restarts the way a real one does: a new SERVED MODEL ID (the identity a loadout is
/// keyed on) beside new max_running_requests, no down->up edge - and /status must follow within
/// one poll. (The SAME-model restart - rv-lead-lss 20:46: no new id, no new container - is the
/// periodic test's job, the C1 case only the re-read can see.)
#[test]
fn slots_refetched_when_engine_identity_changes() {
    let e = SlotsEngine::start(3);
    // the periodic re-read would mask an identity bug, so it is sent out of the way: 0 re-reads
    // between the fetches the test itself causes (0 = "every poll is too soon", i.e. never by age)
    let c = Collector::start("identity", &e.url(), Some(9_999_999));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    let hits_after_first = e.info_hits.load(Ordering::Relaxed);
    let vm_after_first = e.v1models_hits.load(Ordering::Relaxed);
    *e.model.lock().unwrap() = "fake-model-414-restarted".into();
    e.slots.store(8, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 8, "restart: new served id, max_running_requests 3 -> 8");
    assert_eq!(s["serve"]["slots"], 8);
    // the collector really re-ASKED (a mirrored stale number would pass the asserts above)
    assert!(e.info_hits.load(Ordering::Relaxed) > hits_after_first, "the re-fetch was the collector's own ask, not a carried value");
    assert!(e.v1models_hits.load(Ordering::Relaxed) > vm_after_first, "the new id was really served by /v1/models");
}

/// rv-lead-lss 20:46: the container START TIME is a gate key of its own. The fake moves ONLY
/// the start time (same served id throughout, /v1/models hit-counted to prove it) - the same
/// restart shape the periodic test covers for a max_running_requests-only change - and the
/// new slots must land within one poll, via the REAL decision code (no hand copy, no docker:
/// the collector's docker inspect finds nothing, and the start time arrives over the fake's
/// own HTTP like the card's other signals).
#[test]
fn slots_refetched_when_serve_container_start_time_changes() {
    let e = SlotsEngine::with(("fake-model-414".into(), 3, 1_700_000_000));
    // recheck 9_999_999 silences every OTHER re-fetch reason (unknown 0's first ask, up-edge,
    // age) so the START TIME is the only key that can ask: a settled value is not re-read
    // until the container's start time moves, then the REAL gate's serve_ct branch re-fetches
    // and takes 8. (The boot fetch itself is the gate's "never fetched yet" ask - hit-counted
    // below, a mirrored value would not notice.)
    let c = Collector::start("serve-ct", &e.url(), Some(9_999_999));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    let model_at_boot = s["serve"]["model"].clone();
    assert_eq!(model_at_boot, "fake-model-414", "the scrape names the served id");
    // settled: with the age and identity keys quiet, nothing re-asks /get_server_info
    std::thread::sleep(Duration::from_millis(2200));
    let hits_settled = e.info_hits.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(e.info_hits.load(Ordering::Relaxed), hits_settled, "a settled value is not re-asked while the start time stands still");
    e.start_ts.store(1_700_000_100, Ordering::Relaxed);
    e.slots.store(8, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 8, "restart: container start time moved, model unchanged");
    assert_eq!(s["serve"]["slots"], 8);
    // the served id did not move: the start time was the only key that changed (the scrape asks
    // /v1/models every poll, so a /v1/models hit count cannot say this - the id it served can)
    assert_eq!(s["serve"]["model"], model_at_boot, "same served id before and after the restart");
    // the collector really re-ASKED after the move (a mirrored stale number would pass above)
    assert!(e.info_hits.load(Ordering::Relaxed) > hits_settled, "the re-fetch was the collector's own ask after the start time moved");
}

/// One failed fetch during boot must not freeze a wrong value forever, AND a failed fetch after
/// a good value must not keep the good value as truth (rv M2: the old code's `unwrap_or(old)`
/// survived every test that only ever failed from a cold 0).
#[test]
fn failed_slots_fetch_is_retried_not_kept() {
    let e = SlotsEngine::start(3);
    // recheck 0: the re-read is attempted EVERY poll, so the moment the engine stops answering
    // the fetch fails and /status drops to 0 within one poll (rv M2: the old code kept the
    // good 3 through a failed fetch - only a per-poll re-ask can even SEE that failure).
    let c = Collector::start("retry", &e.url(), Some(0));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    // now the engine stops answering /get_server_info while /v1/models still says up
    e.broken.store(503, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 0, "a failed fetch is not kept as truth (3 -> 0, retried)");
    assert_eq!(s["serve"]["slots"], 0, "the old 3 must not survive a failed fetch (M2)");
    e.broken.store(0, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "fetch retried after the engine healed");
    assert_eq!(s["serve"]["slots"], 3);
    // M2's other half (rv-lead-lss 20:46): a fetch that fails AFTER a good non-zero value must
    // not leave the good value standing - the next /status shows unknown (0), then the retry
    // takes the healed engine's answer.
    e.broken.store(503, Ordering::Relaxed);
    e.slots.store(8, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 0, "a LATER failed fetch drops the good 3 (M2 phase 2: 3 -> 0)");
    assert_eq!(s["serve"]["slots"], 0, "the good 3 must not survive a later failed fetch (M2)");
    e.broken.store(0, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 8, "retry takes the healed engine's answer (8, not the stale 3)");
    assert_eq!(s["serve"]["slots"], 8);
}

/// Slots are re-read at least every N seconds even when nothing else changes - here on the REAL
/// poll loop with the interval shortened by the env override, because a test cannot wait 5 min.
#[test]
fn slots_refreshed_periodically() {
    // the SAME model id and the SAME container start time throughout: the ONLY path that can
    // discover the new value is the periodic re-read (lead 22:23: this is the C1 case - a
    // max_running_requests change with neither a new id nor a new container)
    let e = SlotsEngine::with(("fake-model-414".into(), 3, 1_700_000_000));
    // 0 = the re-read is attempted EVERY poll: the periodic path must be visible inside 40 s
    let c = Collector::start("periodic", &e.url(), Some(0));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    e.slots.store(8, Ordering::Relaxed);
    // the identity never changed here: only the periodic re-read can discover 8
    let s = c.status_when(|s| s["serve"]["slots"] == 8, "periodic refresh");
    assert_eq!(s["serve"]["slots"], 8);
}

/// A configured `slots` is the operator's word: the collector never re-ASKS the engine for a
/// slot count and never changes the number - /status holds 5 across several polls while the
/// engine would have answered something else. The fake's hit counter bounds the asks: the
/// collector's LOADOUT identification asks /get_server_info exactly once (a plain process
/// must describe itself once, at the first sample - that ask is not the slots gate), and it
/// never grows: a second ask would be the slots gate second-guessing the configured value.
#[test]
fn a_configured_slots_is_never_second_guessed() {
    let e = SlotsEngine::start(3);
    let c = Collector::named("cfg", &e.url(), None, "slots = 5");
    let s = c.status_when(|s| s["serve"]["up"] == true, "serve up with a configured slots");
    assert_eq!(s["serve"]["slots"], 5, "the configured value stands");
    std::thread::sleep(Duration::from_secs(3));
    let s = c.status();
    assert_eq!(s["serve"]["slots"], 5, "still the configured value after several polls");
    let hits = e.info_hits.load(Ordering::Relaxed);
    assert!(hits <= 1, "cfg.slots means the slots gate never asks (the loadout's ONE boot-time identification ask is not the gate; hits: {hits})");
    drop(c);
    // the engine says 3 the whole time: /status holding 5 through every poll is the proof
    // the configured value was never second-guessed (asserts above).
}
