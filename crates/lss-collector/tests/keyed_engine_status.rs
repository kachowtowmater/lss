//! card #331: an engine that refuses the collector's own key - the collector must say WHY it is
//! down, in words that name the setting, on /status and in the bench's refusal - not just DOWN.
//! Drives the REAL lss-collector binary against a fake engine that wants a Bearer on every route
//! (what `--api-key` does to SGLang/vLLM and what a keyed LAN proxy does).

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const RIGHT: &str = "test-engine-value-331";

fn keyed_engine() -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let h = std::thread::spawn(move || {
        for req in server.incoming_requests() {
            if req.url() == "/stop" {
                let _ = req.respond(tiny_http::Response::from_string("bye"));
                return;
            }
            let ok = req.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == format!("Bearer {RIGHT}"));
            let (code, body) = match (ok, req.url()) {
                (false, _) => (401, "{\"error\": \"invalid api key\"}".to_string()),
                (true, "/v1/models") => (200, "{\"data\": [{\"id\": \"m331\"}]}".to_string()),
                (true, "/metrics") => (200, "sglang:num_running_reqs{} 0\nsglang:num_queue_reqs{} 0\n".to_string()),
                _ => (404, "no".to_string()),
            };
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
        }
    });
    (url, h)
}

struct Collector {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Collector {
    fn start(name: &str, engine: &str, key: Option<&str>) -> Collector {
        let dir = std::env::temp_dir().join(format!("lss-331-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let key_line = key.map(|k| format!("api_key = \"{k}\"\n")).unwrap_or_default();
        let cfg = format!(
            "listen = [\"127.0.0.1:{port}\"]\ndb_path = \"{d}/lss.db\"\npoll_secs = 1\nserve_container = \"\"\nalert_cmd = \"/bin/true\"\n\n[probe]\nenabled = false\n\n[[engine]]\nkind = \"sglang\"\nurl = \"{engine}\"\n{key_line}",
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

    /// /status once it has a verdict on the serve (up, or down with or without a reason)
    fn status_when(&self, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            if let Ok(r) = ureq::get(&format!("http://127.0.0.1:{}/status", self.port)).call() {
                last = serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or_default();
                if done(&last) {
                    return last;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        panic!("no such /status within 40 s; last: {last}\ncollector log:\n{}", std::fs::read_to_string(self.dir.join("collector.log")).unwrap_or_default());
    }

    fn bench(&self) -> String {
        match ureq::post(&format!("http://127.0.0.1:{}/bench", self.port)).send_string("{\"profile\": \"quick\", \"force\": true}") {
            Ok(r) => r.into_string().unwrap_or_default(),
            Err(ureq::Error::Status(_, r)) => r.into_string().unwrap_or_default(),
            Err(e) => panic!("{e}"),
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_keyed_engine_that_refuses_the_collector_says_why_it_is_down() {
    let (engine, h) = keyed_engine();
    // no key configured
    let c = Collector::start("nokey", &engine, None);
    let s = c.status_when(|s| s["serve"]["down_reason"].is_string());
    let why = s["serve"]["down_reason"].as_str().unwrap();
    assert_eq!(s["serve"]["up"], false);
    assert!(why.contains("wants an API key (HTTP 401)") && why.contains("api_key") && why.contains("[[engine]]"), "{why}");
    let refusal = c.bench();
    assert!(refusal.contains("the serve is not up") && refusal.contains("wants an API key"), "the bench refusal says why: {refusal}");
    drop(c);
    // a wrong key
    let c = Collector::start("wrong", &engine, Some("test-wrong-value-331"));
    let s = c.status_when(|s| s["serve"]["down_reason"].is_string());
    let why = s["serve"]["down_reason"].as_str().unwrap();
    assert!(why.contains("refuses the configured API key (HTTP 401)"), "{why}");
    let text = serde_json::to_string(&s).unwrap();
    assert!(!text.contains("test-wrong-value-331"), "the key is never in /status");
    drop(c);
    // the right key: UP, and no reason field at all
    let c = Collector::start("right", &engine, Some(RIGHT));
    let s = c.status_when(|s| s["serve"]["up"] == true);
    assert!(s["serve"].get("down_reason").is_none(), "{}", s["serve"]);
    drop(c);
    let _ = ureq::get(&format!("{engine}/stop")).call();
    let _ = h.join();
}
