//! card #79: the collector self-deadlocked on its FIRST poll because ONE statement took
//! `shared.lock()` TWICE (`bench:` and `maintenance:` inside the same `build_status(...)`
//! argument list). `std::sync::Mutex` is not reentrant and a guard temporary inside a call
//! expression lives until the end of the whole statement, so the second lock waited on the
//! first forever - while holding the mutex the HTTP thread needs, which is why /health,
//! /status, /tokens and /metrics answered NOTHING AT ALL (http=000) rather than answering an
//! error. Production survived only because its binary predated the change.
//!
//! Two tests, deliberately different in kind:
//!   1. `no_statement_locks_the_same_mutex_twice` scans the source for the CLASS - the card's
//!      item 2 said "grep the tree", and a grep is a snapshot, so it is a test instead.
//!   2. `collector_answers_health_on_its_first_poll` runs the real binary and proves the
//!      OUTCOME: an answer, quickly. A future deadlock anywhere in the poll loop fails it,
//!      including one this file's scanner cannot see.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[path = "common/test_ports.rs"]
mod test_ports;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The receiver a `.lock()` is called on: the identifier chain immediately before it
/// (`shared`, `self.db`, `state.inner`). Returns None for something we cannot name, which is
/// counted as its own distinct receiver rather than guessed at.
fn lock_receiver(before: &str) -> Option<String> {
    let bytes = before.as_bytes();
    let mut end = bytes.len();
    // tolerate `shared .lock()` and a line break before the dot
    while end > 0 && (bytes[end - 1] as char).is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_alphanumeric() || c == '_' || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    Some(before[start..end].trim_matches('.').to_string())
}

/// Statements: the source split on `;` and on block boundaries at paren depth 0. A real Rust
/// tokenizer is overkill here, but the cheap version is NOT good enough - a first draft counted
/// `'"'` as opening a string and merged five statements into one, reporting three test
/// functions as offenders. So this handles line and block comments, raw strings (`r"..."`,
/// `r#"..."#`), escapes, and char literals, which is what it takes to not lie.
fn statements(src: &str) -> Vec<String> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        // comments
        // comment TEXT is not code: it must not reach `cur`, or a doc comment that shows
        // `x.lock()` twice as an EXAMPLE fails the test that reads it (measured - this very
        // file's card-109 doc comment did exactly that).
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            let mut nest = 0;
            while i < b.len() {
                if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
                    nest += 1;
                    i += 2;
                    continue;
                }
                if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                    nest -= 1;
                    i += 2;
                    if nest == 0 {
                        break;
                    }
                    continue;
                }
                i += 1;
            }
            continue;
        }
        // raw string: r"..." / r#"..."#
        if c == 'r' && i + 1 < b.len() && (b[i + 1] == '"' || b[i + 1] == '#') {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < b.len() && b[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == '"' {
                let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                let rest: String = b[j + 1..].iter().collect();
                let len = rest.find(&close).map_or(rest.len(), |k| k + close.len());
                cur.push('r');
                cur.push_str(&b[i + 1..=j].iter().collect::<String>());
                cur.push_str(&rest[..len]);
                i = j + 1 + len;
                continue;
            }
        }
        // normal string
        if c == '"' {
            cur.push(c);
            i += 1;
            while i < b.len() {
                if b[i] == '\\' {
                    cur.push(b[i]);
                    if i + 1 < b.len() {
                        cur.push(b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                cur.push(b[i]);
                if b[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // char literal - `'"'` is the one that broke the naive version. A lifetime (`'a`) has
        // no closing quote, so only consume when it really looks like a literal.
        if c == '\'' {
            let is_escaped = i + 3 < b.len() && b[i + 1] == '\\' && b[i + 3] == '\'';
            let is_plain = i + 2 < b.len() && b[i + 1] != '\\' && b[i + 2] == '\'';
            if is_escaped || is_plain {
                let n = if is_escaped { 4 } else { 3 };
                for k in 0..n {
                    cur.push(b[i + k]);
                }
                i += n;
                continue;
            }
        }
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' | '}' => {
                // a block boundary: guards drop at the brace, so two locks either side of one
                // are in different scopes
                out.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            ';' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur);
    out
}

/// Two locks in two different `match` arms cannot both run, so `=>` between them means they
/// are alternatives, not a double lock. Everything else inside one statement is reported.
fn separated_by_match_arm(stmt: &str, a: usize, b: usize) -> bool {
    stmt[a..b].contains("=>")
}

#[test]
fn no_statement_locks_the_same_mutex_twice() {
    let mut files = Vec::new();
    rust_sources(&repo().join("crates"), &mut files);
    assert!(files.len() > 10, "found {} source files - the walk is broken", files.len());
    let mut offenders = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap();
        for stmt in statements(&src) {
            let mut seen: Vec<(String, usize)> = Vec::new();
            for (i, _) in stmt.match_indices(".lock()") {
                let name = lock_receiver(&stmt[..i]).unwrap_or_else(|| "<unnamed>".into());
                if seen.iter().any(|(n, at)| *n == name && !separated_by_match_arm(&stmt, *at, i)) {
                    let line = src[..src.find(stmt.trim()).unwrap_or(0)].lines().count() + 1;
                    offenders.push(format!(
                        "{}:~{} takes `{}.lock()` twice in ONE statement: {}",
                        f.strip_prefix(repo()).unwrap_or(f).display(),
                        line,
                        name,
                        stmt.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>()
                    ));
                }
                seen.push((name, i));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "std::sync::Mutex is NOT reentrant and a guard temporary lives to the end of its \
         statement, so this self-deadlocks (card #79):\n  {}",
        offenders.join("\n  ")
    );
}

struct Collector {
    child: Child,
    addr: SocketAddr,
    dir: PathBuf,
    /// card #323: the engine address, held so that nothing ever answers on it (see test_ports)
    _engine: test_ports::Refusing,
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// One raw GET, with timeouts, returning the status line - or None when nothing answered.
/// Raw TCP on purpose: the failure being pinned is "no response at all", and an HTTP client
/// that retries or buffers could hide it.
fn get(addr: SocketAddr, path: &str, timeout: Duration) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&addr, timeout).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.lines().next().map(|l| l.to_string())
}

fn start_collector() -> Collector {
    let port = free_port();
    // nothing listens: the poll must still complete and publish. HELD for the collector's life
    // (card #323): a free_port() number could be handed to another test's listener meanwhile
    let engine = test_ports::refusing_port();
    let engine_port = engine.port;
    let dir = std::env::temp_dir().join(format!("lss-79-{}-{}", std::process::id(), port));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!(
        "host = \"card79\"\npoll_secs = 1\nengine_url = \"http://127.0.0.1:{engine_port}\"\n\
         engine_kind = \"openai\"\nserve_container = \"\"\nlisten = [\"127.0.0.1:{port}\"]\n\
         db_path = \"{}/lss.db\"\nalert_cmd = \"\"\ntailscale_lookup = false\n\
         [probe]\nenabled = false\n\n[rates]\npath = \"\"\n",
        dir.display()
    );
    let cfg_path = dir.join("collector.toml");
    std::fs::write(&cfg_path, cfg).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_lss-collector"))
        .args(["--config", cfg_path.to_str().unwrap()])
        .env("HOME", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the collector binary runs");
    Collector { child, addr: format!("127.0.0.1:{port}").parse().unwrap(), dir, _engine: engine }
}

#[test]
fn collector_answers_health_on_its_first_poll() {
    let c = start_collector();
    // (a) THE card-79 assertion: an answer, at all, quickly. The deadlocked build answered
    // nothing for as long as anyone waited (measured: http=000 at t=20 s).
    // card #180 gate 2: this bound was 2 s and it flaked once in 12 workspace runs on a loaded
    // 64-core box (measured while hunting a different flake). The bound is not the point of the
    // test - "answers AT ALL rather than never" is: the deadlocked build returned http=000 at
    // t=20 s and would have at t=200. A latency budget belongs in a benchmark, not here, and a
    // release gate that wants CI green on the tagged commit cannot afford an assertion that
    // fails when the machine is busy. 15 s still fails instantly against a real deadlock.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut first = None;
    while Instant::now() < deadline {
        if let Some(line) = get(c.addr, "/health", Duration::from_millis(400)) {
            first = Some(line);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let first = first.unwrap_or_else(|| {
        panic!("/health did not answer within 15 s - the collector is deadlocked or not listening")
    });
    assert!(first.contains("200") || first.contains("503"), "unexpected status line: {first}");

    // (b) and the poll loop must COMPLETE a cycle: /health only turns 200 after the poll
    // publishes last_sample_ts, which happens AFTER the statement that used to deadlock. This
    // is the half that a listener answering 503 from a wedged process could not fake.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = first;
    while Instant::now() < deadline {
        if let Some(line) = get(c.addr, "/health", Duration::from_millis(500)) {
            last = line;
            if last.contains("200") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(last.contains("200"), "/health never became 200 - the poll loop never finished a cycle: {last}");

    // (c) /status must answer too: it is served from the same mutex the poll loop holds.
    let status = get(c.addr, "/status", Duration::from_secs(2));
    assert!(status.is_some_and(|l| l.contains("200")), "/status did not answer 200");
}

// ----------------------------------------------------- card #109: ONE lock order, enforced

/// The declared order for this binary's long-lived mutexes. A lock may be taken while holding
/// any lock EARLIER in this list, never one later. Ranking them (rather than banning nesting
/// outright) is what the code can actually satisfy: the poll loop legitimately holds the `db`
/// guard across a whole cycle and publishes into `shared` inside it.
const LOCK_ORDER: [&str; 3] = ["db", "shared", "tailscale"];

/// `r.ctx.db` -> "db", `ctx.shared` -> "shared". Receivers that are not in LOCK_ORDER (test
/// fakes, the TUI's own small mutexes) are not ranked and are ignored: this test is about the
/// pair that deadlocked, and a rule that flags everything gets deleted.
fn rank(receiver: &str) -> Option<usize> {
    let last = receiver.rsplit('.').next().unwrap_or(receiver);
    LOCK_ORDER.iter().position(|k| *k == last)
}

#[derive(Debug)]
struct Held {
    receiver: String,
    name: Option<String>, // the `let` binding, so `drop(name)` can end it
    line: usize,
    depth: usize,
}

/// The statement text an acquisition sits in, from `from` to the terminating `;` at paren depth
/// 0 (or the next block boundary). Used to tell a BOUND GUARD from a bound VALUE.
fn statement_at(chars: &[char], from: usize) -> String {
    let mut depth = 0i32;
    let mut out = String::new();
    for &c in &chars[from..] {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ';' if depth == 0 => break,
            '{' | '}' if depth == 0 => break,
            _ => {}
        }
        out.push(c);
    }
    out
}

/// True when this acquisition's guard is BOUND (lives to the end of the block) rather than a
/// temporary (dies at the `;`). `let g = x.lock().unwrap();` binds the guard;
/// `let v = x.lock().unwrap().field.clone();` binds a VALUE and drops the guard at the `;`.
/// Getting this wrong is not academic: the first version of this test called every `let` a
/// binding and reported four innocent sites, including two in `lss`'s TUI.
fn binds_the_guard(rest_of_statement: &str) -> bool {
    let mut t = rest_of_statement;
    for head in [".lock()", "lock("] {
        if let Some(i) = t.find(head) {
            t = &t[i + head.len()..];
            break;
        }
    }
    // Drop every balanced (...) group - closure bodies and `expect("...")` arguments - then the
    // tail must be nothing but an unwrap chain. `.clone()`, `.field`, `.bench_runs(20)` or an
    // index all mean a VALUE was bound and the guard died at the `;`.
    let mut flat = String::new();
    let mut depth = 0i32;
    for c in t.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth <= 0 => flat.push(c),
            _ => {}
        }
    }
    flat.split('.')
        .map(str::trim)
        .filter(|seg| !seg.is_empty())
        .all(|seg| matches!(seg, "unwrap" | "unwrap_or_else" | "expect"))
}

/// Every place a ranked lock is taken while a lock LATER in LOCK_ORDER is held.
fn order_violations(path: &Path, src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut held: Vec<Held> = Vec::new();
    let mut out = Vec::new();
    let (mut depth, mut line, mut i, mut stmt_start) = (0usize, 1usize, 0usize, 0usize);
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '"' {
            i += 1;
            while i < chars.len() {
                match chars[i] {
                    '\\' => i += 2,
                    '"' => {
                        i += 1;
                        break;
                    }
                    '\n' => {
                        line += 1;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if c == '\'' {
            let escaped = i + 3 < chars.len() && chars[i + 1] == '\\' && chars[i + 3] == '\'';
            let plain = i + 2 < chars.len() && chars[i + 1] != '\\' && chars[i + 2] == '\'';
            if escaped || plain {
                i += if escaped { 4 } else { 3 };
                continue;
            }
        }
        let rest: String = chars[i..(i + 8).min(chars.len())].iter().collect();
        // an explicit `drop(g)` ends that guard early - bench_run_tests.rs relies on it
        if rest.starts_with("drop(") {
            let after: String = chars[i + 5..].iter().collect();
            if let Some(end) = after.find(')') {
                let name = after[..end].trim().to_string();
                held.retain(|h| h.name.as_deref() != Some(name.as_str()));
            }
        }
        let acquired: Option<String> = if rest.starts_with(".lock()") {
            let before: String = chars[..i].iter().collect();
            lock_receiver(&before)
        } else if rest.starts_with("lock(&") {
            let after: String = chars[i + 6..].iter().collect();
            let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).unwrap_or(after.len());
            (end > 0).then(|| after[..end].to_string())
        } else {
            None
        };
        if let Some(receiver) = acquired {
            let stmt_head: String = chars[stmt_start..i].iter().collect();
            let rest_stmt = statement_at(&chars, i);
            let binding = stmt_head.trim_start().starts_with("let ") && binds_the_guard(&rest_stmt);
            let name = binding
                .then(|| {
                    stmt_head
                        .trim_start()
                        .trim_start_matches("let ")
                        .trim_start_matches("mut ")
                        .split('=')
                        .next()
                        .map(|n| n.trim().to_string())
                })
                .flatten();
            if let Some(r_new) = rank(&receiver) {
                for h in &held {
                    if h.receiver == receiver {
                        continue; // same mutex twice is the OTHER test's finding
                    }
                    if rank(&h.receiver).is_some_and(|r_held| r_held >= r_new) {
                        out.push(format!(
                            "{}:{} takes `{}` while `{}` (line {}) is held - LOCK_ORDER is {:?}",
                            path.display(),
                            line,
                            receiver,
                            h.receiver,
                            h.line,
                            LOCK_ORDER
                        ));
                    }
                }
            }
            held.push(Held { receiver, name, line, depth });
            i += if rest.starts_with(".lock()") { 7 } else { 6 };
            continue;
        }
        match c {
            '{' => {
                depth += 1;
                stmt_start = i + 1;
            }
            '}' => {
                held.retain(|h| h.depth < depth);
                depth = depth.saturating_sub(1);
                stmt_start = i + 1;
            }
            ';' => {
                held.retain(|h| h.name.is_some()); // temporaries die here, bindings do not
                stmt_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// card #109: the two mutexes are acquired in ONE declared order, everywhere.
///
/// The pair that made this a card: the startup path held `shared` and locked `db` inside it
/// (main.rs), while the poll loop holds the `db` guard across a whole cycle and publishes into
/// `shared` inside it - opposite orders, both in PRODUCTION code, which is the shape that
/// deadlocks the moment the two run at once. The startup path now reads from `db` first and
/// takes `shared` afterwards, so `db` is always the outer lock. This test is the part that
/// lasts: LOCK_ORDER is declared once and a violation fails the build.
#[test]
fn locks_are_taken_in_the_declared_order() {
    let mut files = Vec::new();
    rust_sources(&repo().join("crates"), &mut files);
    let mut offenders = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap();
        let rel = f.strip_prefix(repo()).unwrap_or(f).to_path_buf();
        offenders.extend(order_violations(&rel, &src));
    }
    assert!(
        offenders.is_empty(),
        "lock order inverted (card #109) - acquire in LOCK_ORDER, or drop the outer guard first:\n  {}",
        offenders.join("\n  ")
    );
}
