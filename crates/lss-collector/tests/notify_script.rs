//! Drives the REAL scripts/lss-notify.sh - the generic dispatcher install.sh installs and points
//! `alert_cmd` at (card #81). A stranger has no agent CLI and no seat, so this leg must work with
//! nothing but a hook URL and curl.
//!
//! A tiny HTTP server is started IN-PROCESS (std TcpListener + a hand-rolled request read) so the
//! test proves the POST actually leaves the script: the body, the ntfy headers and the JSON
//! escaping are what the receiving end sees. No external network, no side effects.

use std::io::{Read, Write};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

static N: AtomicU32 = AtomicU32::new(0);

/// One received request: method + path + raw headers + body.
#[derive(Clone, Debug)]
struct Request {
    first_line: String,
    raw: String,
}

/// A background server on an ephemeral port that records what it was sent. Answers 200.
/// The received requests come back over a channel (no shared lock, and nothing to poison).
fn start_server() -> (String, Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            if let Some(req) = read_request(&mut s) {
                let _ = tx.send(req);
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

/// Read one HTTP request off the socket: headers to the blank line, then Content-Length bytes.
fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(Request { first_line: head.lines().next().unwrap_or("").to_string(), raw: String::from_utf8_lossy(&buf).into_owned() })
}

/// Pull every request that arrived, waiting briefly so the POST has time to land.
fn received(rx: &Receiver<Request>) -> Vec<Request> {
    let mut v = Vec::new();
    while let Ok(r) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
        v.push(r);
    }
    v
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("lss-notify-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// Run the REAL script with a clean env; `env` supplies the configuration under test.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lss-notify.sh");
        let mut cmd = Command::new("bash");
        cmd.arg(script)
            .args(args)
            .env("HOME", &self.dir)
            .env("LSS_STATE_DIR", self.dir.join("state"))
            // the script must be testable without any site file: point it at one that does not exist
            .env("LSS_ALERT_ENV", self.dir.join("no-such-alert.env"));
        for k in ["LSS_NOTIFY_NTFY", "LSS_NOTIFY_WEBHOOK", "LSS_NOTIFY_CMD", "LSS_NOTIFY_DRY_RUN", "LSS_NOTIFY_TEST", "LSS_NOTIFY_NO_BANNER"] {
            cmd.env_remove(k);
        }
        let out = cmd.envs(env.iter().copied()).output().expect("bash runs");
        assert!(out.status.success(), "lss-notify.sh must NEVER fail its caller (the collector's alert must not break the collector): {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("state/alert.log")).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_ntfy_hook_delivers_the_alert_as_a_real_post() {
    let (url, rx) = start_server();
    let b = Sandbox::new();
    let out = b.run(&["page", "gpu0 over 90C"], &[("LSS_NOTIFY_NTFY", &format!("{url}/my-topic"))]);
    assert!(out.contains("channel=ntfy delivered=1"), "{out}");
    let reqs = received(&rx);
    assert_eq!(reqs.len(), 1, "exactly one POST must leave the script: {reqs:?}");
    let r = &reqs[0];
    assert!(r.first_line.starts_with("POST /my-topic"), "{r:?}");
    // ntfy's own API: Title/Priority/Tags are headers, the message is the body
    assert!(r.raw.contains("Title: LLM SERVER"), "{r:?}");
    assert!(r.raw.contains("Priority: urgent"), "page must ride as ntfy 'urgent': {r:?}");
    assert!(r.raw.contains("[page] gpu0 over 90C"), "the body carries the severity-tagged text: {r:?}");
}

#[test]
fn an_http_webhook_delivers_a_json_body_that_survives_quotes() {
    let (url, rx) = start_server();
    let b = Sandbox::new();
    let out = b.run(&["warn", "queue \"pressure\" at 24"], &[("LSS_NOTIFY_WEBHOOK", &format!("{url}/hook"))]);
    assert!(out.contains("channel=webhook delivered=1"), "{out}");
    let reqs = received(&rx);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let r = &reqs[0];
    assert!(r.first_line.starts_with("POST /hook"), "{r:?}");
    assert!(r.raw.contains("Content-Type: application/json"), "{r:?}");
    // the quotes in the message must be ESCAPED, not emitted raw (that would be invalid JSON)
    assert!(r.raw.contains(r#"{"severity":"warn","message":"queue \"pressure\" at 24"}"#), "{r:?}");
}

#[test]
fn a_custom_command_is_run_with_severity_and_message() {
    let b = Sandbox::new();
    // a tiny script that writes its two arguments to a file: proves the exact two arguments the
    // collector's contract names (`<severity> <message>`), with no shell-quoting tricks in the test
    let sink = b.dir.join("cmd-args");
    let script = b.dir.join("my-notifier.sh");
    exec_file::write_exec(&script, &format!("#!/bin/bash\nprintf '%s|%s' \"$1\" \"$2\" > {}\n", sink.display()));
    let out = b.run(&["hardware", "xid on gpu1"], &[("LSS_NOTIFY_CMD", &script.display().to_string())]);
    assert!(out.contains("channel=cmd delivered=1"), "{out}");
    let args = std::fs::read_to_string(&sink).expect("the command ran and wrote its args");
    assert_eq!(args, "hardware|xid on gpu1", "{args}");
}

#[test]
fn every_failure_is_honest_and_still_exits_zero() {
    let b = Sandbox::new();
    // a hook that cannot connect: delivered=0, the reason logged, the caller unaffected
    let out = b.run(&["warn", "x"], &[("LSS_NOTIFY_NTFY", "http://127.0.0.1:9/nothing"), ("LSS_NOTIFY_TIMEOUT", "2")]);
    assert!(out.contains("delivered=0") && out.contains("http_failed"), "{out}");
    assert!(b.log().contains("status=FAIL channel=http"), "{}", b.log());

    // nothing configured: says so plainly, names nothing it did not do
    let out = b.run(&["warn", "y"], &[("LSS_NOTIFY_NO_BANNER", "1")]);
    assert!(out.contains("channel=none delivered=0 detail=not_configured"), "{out}");
    assert!(b.log().contains("reason=not_configured"), "{}", b.log());
}

#[test]
fn a_dry_run_reports_dry_run_and_sends_nothing() {
    let (url, rx) = start_server();
    let b = Sandbox::new();
    let out = b.run(&["page", "x"], &[("LSS_NOTIFY_NTFY", &format!("{url}/t")), ("LSS_NOTIFY_DRY_RUN", "1")]);
    assert!(out.contains("delivered=0") && out.contains("dry_run"), "a dry run must never claim delivery: {out}");
    assert!(received(&rx).is_empty(), "a dry run must not send");
}

#[test]
fn flush_is_a_noop_that_answers_in_the_collectors_shape() {
    // the collector runs `alert_cmd --flush` every alert_flush_secs and treats
    // "flushed=0 spool=0" as "nothing to report" - this dispatcher spools nothing, but must still
    // answer in that exact shape so the collector stays quiet.
    let b = Sandbox::new();
    let out = b.run(&["--flush"], &[]);
    assert!(out.contains("flush flushed=0 spool=0"), "{out}");
}
