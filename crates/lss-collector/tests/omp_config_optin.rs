//! tb lss #518 (r3): the collector's omp opt-in wiring, driven through the REAL binary e2e.
//! #514's mutant M2 made the collector ALWAYS watch the default `~/.omp/agent/config.yml`; the
//! lss-core rules tests kept it green because they never execute `run()`'s wiring. So this test
//! starts the actual `lss-collector` process against a fake sglang engine and reads `GET /rules`
//! (the document fed by the poll loop's own `Observation.omp_mismatch` - the value run() wired):
//!
//! - `omp_config_path = ""` (the default) -> `omp_default_mismatch` is NEVER armed, even though a
//!   real omp config sits at the default path under the collector's HOME;
//! - `omp_config_path = "~/.omp/agent/config.yml"` -> the rule's pending_since arms (the
//!   configured default `other-model` != the served `m518`).

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const SERVED: &str = "m518";
const OMP_DEFAULT: &str = "other-model-518";

fn fake_engine() -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let h = std::thread::spawn(move || {
        for req in server.incoming_requests() {
            if req.url() == "/stop" {
                let _ = req.respond(tiny_http::Response::from_string("bye"));
                return;
            }
            let (code, body) = match req.url() {
                "/v1/models" => (
                    200,
                    format!("{{\"data\": [{{\"id\": \"{SERVED}\"}}]}}"),
                ),
                "/metrics" => (
                    200,
                    "sglang:num_running_reqs{} 0\nsglang:num_queue_reqs{} 0\n".to_string(),
                ),
                _ => (404, "no".to_string()),
            };
            let _ = req.respond(
                tiny_http::Response::from_string(body)
                    .with_status_code(code)
                    .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()),
            );
        }
    });
    (url, h)
}

/// One temp HOME D per collector: D holds collector.toml, D/.omp/agent/config.yml holds an omp
/// config whose default model is NOT what the engine serves. Drop kills the child and removes D.
struct Collector {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Collector {
    fn start(name: &str, engine: &str, omp_line: &str) -> Collector {
        let dir = std::env::temp_dir().join(format!("lss-518-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".omp/agent")).unwrap();
        // what run()'s watcher would read if it ignored the empty-key opt-in: a REAL omp config
        // at the DEFAULT path, naming a default model the engine does not serve
        std::fs::write(
            dir.join(".omp/agent/config.yml"),
            format!("modelRoles:\n  default: {OMP_DEFAULT}\n"),
        )
        .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = format!(
            "listen = [\"127.0.0.1:{port}\"]\ndb_path = \"{d}/lss.db\"\npoll_secs = 1\nserve_container = \"\"\nalert_cmd = \"/bin/true\"\n\n[probe]\nenabled = false\n\n[[engine]]\nkind = \"sglang\"\nurl = \"{engine}\"\n\n[rules]\nomp_config_path = \"{omp_line}\"\n",
            d = dir.display()
        );
        std::fs::write(dir.join("collector.toml"), cfg).unwrap();
        let log = std::fs::File::create(dir.join("collector.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_lss-collector"))
            .arg("--config")
            .arg(dir.join("collector.toml"))
            .env("HOME", &dir)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Collector { child, port, dir }
    }

    /// Poll GET /rules until `done` says we have our answer (or fail with the last document).
    fn rules_when(&self, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            if let Ok(r) = ureq::get(&format!("http://127.0.0.1:{}/rules", self.port)).call() {
                last = serde_json::from_str(&r.into_string().unwrap_or_default())
                    .unwrap_or_default();
                if done(&last) {
                    return last;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        panic!(
            "no such /rules within 25 s; last: {last}\ncollector log:\n{}",
            std::fs::read_to_string(self.dir.join("collector.log")).unwrap_or_default()
        );
    }

    /// the `omp_default_mismatch` row, once the poll loop has rendered it
    fn omp_rule(&self) -> serde_json::Value {
        self.rules_when(|doc| {
            doc["rules"]
                .as_array()
                .is_some_and(|rs| rs.iter().any(|r| r["rule"] == "omp_default_mismatch"))
        })["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rule"] == "omp_default_mismatch")
            .cloned()
            .unwrap()
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// NEED: with `omp_config_path` empty (the shipped default), the omp rule must be never armed -
/// `ok`, no `pending_since`, forever - although a real omp config names `other-model-518` at the
/// very path M2 forced the collector to watch. Runs the full 8 s a mutant would need to arm it
/// (hold 600 s >> this window, so ANY arm shows up as pending).
#[test]
fn collector_never_reads_the_omp_config_when_omp_config_path_is_empty() {
    let (engine, h) = fake_engine();
    let c = Collector::start("empty", &engine, "");
    // arm-or-silence: any pending_since inside 8 s means the config WAS read
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let r = c.omp_rule();
        assert!(
            r["pending_since"].is_null(),
            "the omp rule armed although omp_config_path is empty - the collector read \
             {}/.omp/agent/config.yml (value: {r})",
            c.dir.display()
        );
        assert_eq!(r["state"], "ok", "{r}");
        std::thread::sleep(Duration::from_millis(400));
    }
    drop(c);
    let _ = ureq::get(&format!("{engine}/stop")).call();
    let _ = h.join();
}

/// CONTROL: a SET path flips the wiring on - the rule's pending_since arms within a poll or two.
#[test]
fn collector_flags_the_omp_default_when_omp_config_path_is_set() {
    let (engine, h) = fake_engine();
    let c = Collector::start("set", &engine, "~/.omp/agent/config.yml");
    let r = c.rules_when(|doc| {
        doc["rules"]
            .as_array()
            .is_some_and(|rs| {
                rs.iter().any(|r| {
                    r["rule"] == "omp_default_mismatch" && r["pending_since"].is_i64()
                })
            })
    });
    let r = r["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "omp_default_mismatch")
        .unwrap();
    assert_eq!(r["state"], "pending", "held for omp_mismatch_hold_secs: {r}");
    assert!(
        r["value"].as_str().is_some_and(|v| v.contains(OMP_DEFAULT) && v.contains(SERVED)),
        "the row names both sides of the mismatch: {r}"
    );
    drop(c);
    let _ = ureq::get(&format!("{engine}/stop")).call();
    let _ = h.join();
}
