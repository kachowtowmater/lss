//! card #414: the collector's `slots` goes stale after a serve restart. Four tests drive the
//! REAL lss-collector binary against a fake SGLang (the keyed_engine_status.rs rig, minus the
//! key): a restart that changes the SERVED MODEL ID must show the new slots on /status within
//! one poll, the container START TIME changing must do the same (rv-lead-lss 20:46: the
//! restart fake drives the REAL decision code - no hand copy, no docker - over each of the
//! gate's identity keys: served model id, container start time, and the periodic re-read that
//! covers an unchanged identity), a failed fetch must not keep the good value as truth (the M2
//! mutation must die), and a configured `slots` is never second-guessed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One fake SGLang. `slots` is an atomic so a test can change what the engine says
/// (a restarted serve, bigger `--max-running-requests`) without restarting the HTTP thread;
/// `broken` makes /get_server_info fail, like an engine still booting; `info_hits` counts every
/// /get_server_info answer, so a test can prove the collector ASKED (or never asked) - no
/// assertion rides on a number the fake merely mirrored. `model` stands in for the served
/// model id: the gate's other identity key (the container start time comes from the fake
/// `docker` on PATH, not from the engine, card #443).
struct SlotsEngine {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
    slots: std::sync::Arc<AtomicU32>,
    broken: std::sync::Arc<AtomicU32>,
    info_hits: std::sync::Arc<AtomicU32>,
    model: Arc<AtomicU32>,
    /// card #443 r4: every path the engine answered, in order - the no-extra-HTTP test prints
    /// it in its assertion message so the extra per-poll request is NAMED, not just counted.
    paths: Arc<parking_lot::Mutex<Vec<String>>>,
    /// card #523: index in `paths` just after the most recent /metrics answer - a POLL
    /// BOUNDARY the engine thread itself records at answer time, so a test can slice the log
    /// between two boundaries with no observer race (reading a counter races the next request).
    metrics_boundary: Arc<AtomicU32>,
}

impl SlotsEngine {
    /// card #523: wait until the engine has recorded a poll boundary PAST `after` (None = its
    /// current boundary), then return that boundary - the path-log index just after the
    /// /metrics answer that ended the NEXT complete poll. The ENGINE THREAD stores the boundary
    /// itself at answer time (the log index right after its own push), so the value can never
    /// sit ahead of the log, and every entry below it belongs to a poll that has answered its
    /// /metrics. A boundary is a log INDEX, not a poll count, so "n more polls" is n calls,
    /// never `+ n`. The whole box is the status_when deadline (40 s) - a collector that stopped
    /// polling fails here, loudly.
    fn next_poll(&self, after: Option<u32>) -> u32 {
        let start = after.unwrap_or_else(|| self.metrics_boundary.load(Ordering::Relaxed));
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let now = self.metrics_boundary.load(Ordering::Relaxed);
            if now > start {
                return now;
            }
            assert!(Instant::now() < deadline, "the collector stopped polling: /metrics boundary still {start} after 40 s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn start(slots: u32) -> SlotsEngine {
        SlotsEngine::with(("fake-model-414".into(), slots, 1_700_000_000))
    }
    /// `model`: the served id /v1/models publishes; `slots`: max_running_requests. The tuple's
    /// third slot is vestigial (the old HTTP start-time route, deleted in card #443 - the
    /// start time is docker's now, via the fake `docker` on PATH).
    fn with((_model, slots, _start_ts): (String, u32, u32)) -> SlotsEngine {
        let slots = Arc::new(AtomicU32::new(slots));
        let broken = Arc::new(AtomicU32::new(0));
        let info_hits = Arc::new(AtomicU32::new(0));
        let model = Arc::new(AtomicU32::new(0));
        let (slots2, broken2, hits2, model2) = (slots.clone(), broken.clone(), info_hits.clone(), model.clone());
        let paths = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let paths2 = paths.clone();
        let metrics_boundary = Arc::new(AtomicU32::new(0));
        let mb2 = metrics_boundary.clone();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                if req.url() == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                // card #443 r4: remember every path the collector asked, so the no-extra-HTTP
                // assertion can NAME the extra one instead of only counting it. The record is
                // made on every arm (404s and the broken /get_server_info arm included), so
                // an extra GET /health per poll cannot pass unseen (rv-443's M3 gap).
                paths2.lock().push(req.url().to_string());
                let (code, body) = match req.url() {
                    u if u.starts_with("/docker/") && u.ends_with("/started_at") => {
                        // card #443: this route MUST NEVER be asked again - a 404 with the
                        // total-ask count already bumped above
                        (404, "route deleted (card #443)".to_string())
                    }
                    "/v1/models" => {
                        let id = match model2.load(Ordering::Relaxed) {
                            0 => "fake-model-414".to_string(),
                            1 => "fake-model-414-restarted".to_string(),
                            _ => "fake-model-414".to_string(),
                        };
                        (200, format!("{{\"data\":[{{\"id\":\"{id}\"}}]}}"))
                    }
                    "/metrics" => {
                        // card #523: a /metrics answer ENDS a poll's scrapes, so the engine
                        // thread itself marks the log index right after pushing it - the poll
                        // boundary the no-extra-HTTP test slices between.
                        mb2.store(paths2.lock().len() as u32, Ordering::Relaxed);
                        (200, "sglang:num_running_reqs{} 0\nsglang:num_queue_reqs{} 0\n".to_string())
                    }
                    "/get_server_info" => match broken2.load(Ordering::Relaxed) {
                        0 => {
                            hits2.fetch_add(1, Ordering::Relaxed);
                            (200, format!("{{\"version\":\"414.0\",\"max_running_requests\":{}}}", slots2.load(Ordering::Relaxed)))
                        }
                        // card #523: the 503 arm counts too (total bumped once, above) - rv-443
                        // M3: a broken /get_server_info arm must never be an invisible ask
                        n => (n, "no".to_string()),
                    },
                    _ => (404, "no".to_string()),
                };
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
            }
        });
        SlotsEngine { port, handle: Some(handle), slots, broken, info_hits, model, paths, metrics_boundary }
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
    /// `docker_started`: when set, a fake `docker` executable goes on the collector's PATH and
    /// answers `docker inspect --format <INSPECT_FORMAT>` exactly as docker would, reporting a
    /// container named `serve` in state `running` with that unix start time (card #443: the
    /// REAL decision code must read the start time through the REAL docker-inspect path).
    fn named(name: &str, engine: &str, recheck_secs: Option<i64>, slots_cfg: &str) -> Collector {
        Self::with_docker(name, engine, recheck_secs, slots_cfg, None)
    }
    fn with_docker(name: &str, engine: &str, recheck_secs: Option<i64>, slots_cfg: &str, docker_started: Option<i64>) -> Collector {
        let dir = std::env::temp_dir().join(format!("lss-414-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(started) = docker_started {
            write_docker_shim(&dir, started);
        }
        // a pinned serve container name with docker on PATH: the collector asks the REAL
        // `docker inspect` for it every poll; WITHOUT the shim (docker absent) the same pinned
        // name is the card's case - inspect finds nothing and the collector must NOT ask the
        // engine over HTTP for the start time. `serve_container = ""` reads as "auto" in the
        // config parser, so the plain-process tests pin the empty string explicitly here.
        let serve_ct_cfg = if docker_started.is_some() { "serve_container = \"serve\"\n" } else { "serve_container = \"\"\n" };
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = format!(
            "listen = [\"127.0.0.1:{port}\"]\ndb_path = \"{d}/lss.db\"\npoll_secs = 1\n{serve_ct_cfg}alert_cmd = \"/bin/true\"\n{slots_cfg}\n\n[probe]\nenabled = false\n\n[[engine]]\nkind = \"sglang\"\nurl = \"{engine}\"",
            d = dir.display(),
            slots_cfg = if slots_cfg.is_empty() { String::new() } else { format!("{slots_cfg}\n") }
        );
        std::fs::write(dir.join("collector.toml"), cfg).unwrap();
        let log = std::fs::File::create(dir.join("collector.log")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lss-collector"));
        cmd.arg("--config").arg(dir.join("collector.toml")).env("HOME", &dir);
        if docker_started.is_some() {
            // the fake `docker` must be the FIRST name on PATH the collector finds
            let mut path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
            path.insert(0, dir.join("bin"));
            cmd.env("PATH", std::env::join_paths(&path).unwrap());
        }
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

/// A fake `docker` executable: `inspect --format <INSPECT_FORMAT> <name>` prints one line -
/// container `serve`, running, 0 restarts, the given unix start time in RFC3339 exactly as
/// docker prints it - and anything else exits 1 like docker without such a container. The
/// collector reads the start time through the REAL docker-inspect path (card #443).
fn write_docker_shim(dir: &Path, started: i64) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let rfc = chrono::DateTime::from_timestamp(started, 0).unwrap().to_rfc3339();
    std::fs::write(
        bin.join("docker"),
        format!(
            "#!/bin/sh\ncase \"$1 $3\" in\n  \"inspect {{{{.Name}}}}|{{{{.State.Status}}}}|{{{{.RestartCount}}}}|{{{{.State.StartedAt}}}}\")\n    printf 'serve|running|0|%s\\n' \"{rfc}\" ;;\n  *) exit 1 ;;\nesac\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
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
    let v1models = |e: &SlotsEngine| e.paths.lock().iter().filter(|p| p.as_str() == "/v1/models").count();
    let vm_after_first = v1models(&e);
    e.model.store(1, Ordering::Relaxed);
    e.slots.store(8, Ordering::Relaxed);
    let s = c.status_when(|s| s["serve"]["slots"] == 8, "restart: new served id, max_running_requests 3 -> 8");
    assert_eq!(s["serve"]["slots"], 8);
    // the collector really re-ASKED (a mirrored stale number would pass the asserts above)
    assert!(e.info_hits.load(Ordering::Relaxed) > hits_after_first, "the re-fetch was the collector's own ask, not a carried value");
    // the new id was really served by /v1/models (card #523: read from the engine's path log,
    // the one record every answered request lands in)
    assert!(v1models(&e) > vm_after_first, "the new id was really served by /v1/models");
}

/// rv-lead-lss 20:46: the container START TIME is a gate key of its own. The fake moves ONLY
/// the start time (same served id throughout) - the same restart shape the periodic test
/// covers for a max_running_requests-only change - and the new slots must land within one
/// poll, via the REAL decision code: the start time arrives through the REAL docker-inspect
/// path (a fake `docker` on PATH reporting container `serve` running, card #443 - no HTTP
/// stand-in, no hand copy).
#[test]
fn slots_refetched_when_serve_container_start_time_changes() {
    let e = SlotsEngine::with(("fake-model-414".into(), 3, 1_700_000_000));
    // recheck 9_999_999 silences every OTHER re-fetch reason (unknown 0's first ask, up-edge,
    // age) so the START TIME is the only key that can ask: a settled value is not re-read
    // until the container's start time moves, then the REAL gate's serve_ct branch re-fetches
    // and takes 8. (The boot fetch itself is the gate's "never fetched yet" ask - hit-counted
    // below, a mirrored value would not notice.) The docker shim reports the boot start time.
    let c = Collector::with_docker("serve-ct", &e.url(), Some(9_999_999), "", Some(1_700_000_000));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    let model_at_boot = s["serve"]["model"].clone();
    assert_eq!(model_at_boot, "fake-model-414", "the scrape names the served id");
    // settled: with the age and identity keys quiet, nothing re-asks /get_server_info
    std::thread::sleep(Duration::from_millis(2200));
    let hits_settled = e.info_hits.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(e.info_hits.load(Ordering::Relaxed), hits_settled, "a settled value is not re-asked while the start time stands still");
    // the restart, the way docker sees it: SAME name, NEW start time (the shim is rewritten
    // on disk; the collector forks it fresh every poll, so the next inspect sees it). The
    // served id is untouched here - the start time is the only key that moves.
    write_docker_shim(&c.dir, 1_700_000_100);
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

/// card #443: with `serve_container` set and docker having NO answer (no `docker` executable at
/// all), the collector must not add an HTTP GET per poll - the old code asked
/// `http://<engine>/docker/<name>/started_at` every poll whenever docker inspect returned
/// nothing, a 404 every 5 s against a real SGLang. The fake engine counts EVERY request it
/// sees and RECORDS the path of each (r4: the extra request is named, not just counted). The
/// baseline set of paths a settled sglang poll legitimately asks is {/v1/models, /metrics} -
/// the same two scrapes at 3f21a9c (Sglang::scrape: openai_model asks /v1/models, then
/// fetch("/metrics")). The window is measured IN POLLS, with NO timing window at all: the
/// engine thread marks a POLL BOUNDARY in its own path log at each /metrics answer (card
/// #523), so the test slices the log between two boundaries and judges COMPLETE polls (the
/// r1-r4 counter window could catch the NEXT poll's first request at its edge - the #513
/// flake, 1 in 3 runs). The /get_server_info asks are counted separately - the slots gate's
/// own, already covered.
#[test]
fn no_extra_http_per_poll_when_docker_has_no_answer() {
    let e = SlotsEngine::start(3);
    let c = Collector::start("no-extra-http", &e.url(), Some(9_999_999));
    let s = c.status_when(|s| s["serve"]["slots"] == 3, "first boot, slots 3");
    assert_eq!(s["serve"]["slots"], 3);
    // settled: at least 3 further polls with nothing to re-read, OBSERVED (card #443 r3: a wall
    // clock sleep races the collector's own poll loop - /status is only rebuilt at the END of
    // each poll, so the boot wait can return inside the FIRST poll and a bare sleep starts its
    // window before any new scrape; a settle that sees no polls proves nothing). Each
    // /metrics answer is one poll's LAST scrape, so a boundary value names the log index just
    // after a COMPLETE poll's scrapes: b1 = 1 more /metrics past b0's, b2 = 1 more past b1.
    let b0 = e.next_poll(None); // land just after a COMPLETE poll's scrapes
    let info_at_start = e.info_hits.load(Ordering::Relaxed);
    let b1 = e.next_poll(Some(b0)); // poll A is COMPLETELY inside [b0, b1): /v1/models, /metrics
    let b2 = e.next_poll(Some(b1)); // poll B has answered EVERYTHING by b2 - b2 IS poll B's last answer
    let window = e.paths.lock()[b0 as usize..b2 as usize].to_vec();
    // every boundary sits right after a /metrics answer, so [b0, b2) holds only COMPLETE polls:
    // the /metrics answers in it ARE the polls (>= 2 by construction; more if this thread was
    // descheduled across a boundary, and then the window holds more WHOLE polls, never a partial
    // one - an exact "4" would be the flake again, one stall of > poll_secs away). A settled
    // sglang poll is exactly [/v1/models, /metrics]: the slots gate was quiet (recheck
    // 9_999_999, identity unchanged, nothing failed), so NO /get_server_info may appear, and
    // NOTHING ELSE either - the deleted /docker/<name>/started_at route, a /health, a detect -
    // any of them lands in the log and is NAMED here.
    let polls = window.iter().filter(|p| p.as_str() == "/metrics").count();
    assert!(polls >= 2, "the window [{b0}, {b2}) must hold at least two polls: [{}]", window.join(", "));
    assert_eq!(window.len(), 2 * polls, "{polls} complete polls ask exactly {} scrapes (/v1/models + /metrics each) - extra per-poll HTTP exists; paths between the poll boundaries: [{}]", 2 * polls, window.join(", "));
    for (i, p) in window.iter().enumerate() {
        let expected = if i % 2 == 0 { "/v1/models" } else { "/metrics" };
        assert_eq!(p, expected, "request {} between the poll boundaries is not the {} scrape of a settled sglang poll: [{}]", i + 1, if i % 2 == 0 { "first" } else { "second" }, window.join(", "));
    }
    assert_eq!(e.info_hits.load(Ordering::Relaxed), info_at_start, "no /get_server_info re-asks either");
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
