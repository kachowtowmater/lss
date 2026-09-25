//! Zero-config: find the LLM server(s) on THIS machine, whatever the engine.
//!
//! Candidates are the ports something is listening on (`ss`, else `netstat`, else `lsof`), the
//! ports docker publishes, and every engine's default port. Each is asked a handful of cheap
//! GETs with short timeouts and recognised by its fingerprint (`lss_core::engine`). Nothing is
//! written, nothing is started, and no request costs the server a token.

use crate::cmd::{self, Guard};
use lss_core::config::Config;
use lss_core::engine::{self, EngineKind, Found};
use std::time::Duration;

const KNOCK_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_PORTS: usize = 48;

/// GET `base + path` with detection's short timeouts.
pub fn fetcher(base: &str, timeout: Duration) -> impl Fn(&str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_millis(400)).timeout(timeout).build();
    let base = base.trim_end_matches('/').to_string();
    move |path: &str| match crate::collect::authed(agent.get(&format!("{base}{path}"))).call() {
        // a body that is not text (or is huge) is not an LLM server's answer to these paths
        Ok(resp) => resp.into_string().map_err(|e| format!("read: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(e.to_string().chars().take(80).collect()),
    }
}

/// The ports worth asking, most likely first. `skip` = our own listeners.
pub fn candidate_ports(skip: &[u16]) -> Vec<u16> {
    let quick = Duration::from_secs(3);
    let listening = [("ss", vec!["-ltnH"]), ("netstat", vec!["-an", "-p", "tcp"]), ("netstat", vec!["-ltn"]), ("lsof", vec!["-iTCP", "-sTCP:LISTEN", "-nP"])]
        .iter()
        .find_map(|(prog, args)| cmd::run(&Guard::default(), prog, args, quick).ok().map(|o| engine::listening_ports(&o.stdout)).filter(|p| !p.is_empty()))
        .unwrap_or_default();
    let docker = cmd::run(&Guard::default(), "docker", &["ps", "--format", "{{.Ports}}"], quick).map(|o| engine::docker_published_ports(&o.stdout)).unwrap_or_default();
    let mut ports: Vec<u16> = Vec::new();
    // the defaults first (when they are listening, or when we cannot tell what is listening)
    for p in engine::COMMON_PORTS.iter().copied().filter(|p| listening.is_empty() || listening.contains(p) || docker.contains(p)).chain(docker.iter().copied()).chain(listening.iter().copied().filter(|p| *p >= 1024)) {
        if !ports.contains(&p) && !skip.contains(&p) {
            ports.push(p);
        }
    }
    ports.truncate(MAX_PORTS);
    ports
}

/// The full, multi-path identification of one port.
fn detect_one(port: u16) -> Option<Found> {
    let url = format!("http://127.0.0.1:{port}");
    let fetch = fetcher(&url, KNOCK_TIMEOUT);
    let (kind, why) = engine::detect(&fetch)?;
    let identity = engine::adapter_for(kind, "", "").identity(&fetch);
    Some(Found { kind, url, why, identity })
}

/// Every LLM server on this machine, and the ports that were asked.
///
/// #44 D1, 2026-09-21: a candidate outside the well-known engine ports - typically our own
/// gateway, or any other proxy in front of a real engine already found - used to get the SAME
/// multi-path `detect()` as everything else, even though `rank()` was always going to throw it
/// away the moment its model matched a real engine's. Live: a zero-config scan added 6 rejected
/// and 4 no-key entries to our own gateway's audit log for a proxy `rank()` discarded anyway.
/// Ports in `engine::COMMON_PORTS` run the full detection first, in parallel, as before (a real
/// engine on its usual port is the common case and deserves no extra round trip). Every OTHER
/// port is asked the one cheap question - `/v1/models` alone - first: if that already names a
/// model a real (non-generic) engine from the first group is serving, this port is a proxy and
/// nothing more is asked of it. A port whose model does not match (or that answers nothing to
/// the cheap question) still gets the full detection - this only ever SKIPS work `rank()` would
/// have undone anyway, never a real find.
pub fn scan(skip: &[u16]) -> (Vec<Found>, Vec<u16>) {
    let ports = candidate_ports(skip);
    let (primary, secondary): (Vec<u16>, Vec<u16>) = ports.iter().copied().partition(|p| engine::COMMON_PORTS.contains(p));
    (scan_ports(&primary, &secondary), ports)
}

/// The two-phase scan itself, over an explicit port list - split out from [`scan`] so it is
/// testable without a real `ss`/`docker` on the machine running the test.
fn scan_ports(primary: &[u16], secondary: &[u16]) -> Vec<Found> {
    let mut found: Vec<Found> = std::thread::scope(|s| {
        let handles: Vec<_> = primary.iter().copied().map(|port| s.spawn(move || detect_one(port))).collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });
    let known_models: Vec<String> = found.iter().filter(|f| f.kind != EngineKind::OpenAi).filter_map(|f| f.identity.model.clone()).collect();
    let secondary_found: Vec<Found> = std::thread::scope(|s| {
        let handles: Vec<_> = secondary
            .iter()
            .copied()
            .map(|port| {
                let known_models = &known_models;
                s.spawn(move || {
                    let url = format!("http://127.0.0.1:{port}");
                    let fetch = fetcher(&url, KNOCK_TIMEOUT);
                    if engine::openai_model(&fetch).is_ok_and(|m| known_models.contains(&m)) {
                        return None; // a proxy in front of an engine already found: stop here
                    }
                    detect_one(port)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });
    found.extend(secondary_found);
    rank(found)
}

/// Engines lss knows by name first. A plain OpenAI-compatible answer that serves the same model
/// as one of those is a gateway or proxy in front of it, not a second server: dropped.
pub fn rank(mut found: Vec<Found>) -> Vec<Found> {
    let native: Vec<Option<String>> = found.iter().filter(|f| f.kind != EngineKind::OpenAi).map(|f| f.identity.model.clone()).collect();
    found.retain(|f| f.kind != EngineKind::OpenAi || !native.contains(&f.identity.model));
    // the one this collector will watch comes first: a recognised engine with a model loaded
    found.sort_by_key(|f| (f.kind == EngineKind::OpenAi, f.identity.model.is_none(), f.url.clone()));
    found
}

/// Our own listeners: never knock on ourselves.
pub fn own_ports(cfg: &Config) -> Vec<u16> {
    cfg.listen.iter().filter_map(|a| a.rsplit(':').next().and_then(|p| p.parse().ok())).collect()
}

/// `lss-collector --detect`: what was found and why, and the config that pins it.
pub fn report(cfg: &Config) -> (String, i32) {
    if let Some((kind, url)) = cfg.pinned_engine() {
        let fetch = fetcher(&url, KNOCK_TIMEOUT);
        let seen = engine::detect(&fetch);
        let head = format!("The config pins the engine: {} at {url}\n", kind.map_or("auto", EngineKind::name));
        return match seen {
            Some((k, why)) => {
                let identity = engine::adapter_for(kind.unwrap_or(k), "", "").identity(&fetch);
                (format!("{head}{}", engine::describe(&[Found { kind: kind.unwrap_or(k), url, why, identity }], &[])), 0)
            }
            None => (format!("{head}  ... and nothing lss recognises answers there right now (is it running?)\n"), 1),
        };
    }
    let (found, tried) = scan(&own_ports(cfg));
    let mut text = engine::describe(&found, &tried);
    if found.len() > 1 {
        text.push_str("\nThis collector watches the FIRST one. Give each of the others a collector of its own\n(its own config with a [[engine]] block, `listen` port and `db_path`), and list them all as\n[[server]] entries in lss.toml: docs/ENGINES.md shows how. install.sh does this for you.\n");
    }
    let code = i32::from(found.is_empty());
    (text, code)
}

/// Fill in the engine URL and kind before the collector starts. With `engine_url = "auto"` this
/// looks until something answers (a server that is still loading its model is normal at boot).
pub fn resolve(cfg: &mut Config) {
    if let Some((kind, url)) = cfg.pinned_engine() {
        cfg.engine_api_key = cfg.pinned_api_key();
        cfg.sglang_url = url;
        if let Some(k) = kind {
            cfg.engine_kind = k.name().to_string();
        } else {
            cfg.engine_kind = "auto".to_string();
        }
        return;
    }
    let skip = own_ports(cfg);
    let mut waiting: Option<Waiting> = None;
    loop {
        let (found, tried) = scan(&skip);
        if let Some(f) = found.first() {
            eprintln!("lss-collector: found {} at {} ({}){}", f.kind.label(), f.url, f.why, if found.len() > 1 { format!(" - and {} more: see --detect", found.len() - 1) } else { String::new() });
            cfg.sglang_url = f.url.clone();
            cfg.engine_kind = f.kind.name().to_string();
            // the real server binds the same addresses next: release them first
            if let Some(w) = waiting.take() {
                w.stop();
            }
            return;
        }
        if waiting.is_none() {
            eprintln!("lss-collector: no LLM server found on this machine yet (asked ports {tried:?}); looking again every 15 s. Pin one with [[engine]] in the config, or see `lss-collector --detect`.");
            let w = Waiting::start(&cfg.listen, &waiting_message(&tried));
            // card #180 gate 2: say whether the waiting answer actually exists. If it took none
            // of the configured addresses, `lss` will show "collector unreachable" - the exact
            // wrong message this waiter was built to replace - and the reason belongs in the log
            // now rather than in a puzzled reading of it later.
            if w.bound().is_empty() {
                eprintln!("lss-collector: WARNING - could not answer on any of {:?} while waiting, so `lss` will say the collector is unreachable instead of waiting for an engine. Free the address, or set a different `listen` in collector.toml.", cfg.listen);
            } else if w.bound().len() < cfg.listen.len() {
                eprintln!("lss-collector: answering the waiting message on {:?} only, of {:?} configured.", w.bound(), cfg.listen);
            }
            waiting = Some(w);
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

/// card #180 gate 6, measured in a clean container: a stranger who installs lss BEFORE starting
/// their model got "COLLECTOR UNREACHABLE ... the serve may be fine; the monitor is not" - the
/// exact opposite of the truth, because the collector opened no port at all until it found an
/// engine. While it looks, it now answers every request on its `listen` addresses with a 503
/// that says so (`X-LSS-Waiting: engine`), and `lss` shows that sentence instead.
pub const WAITING_HEADER: &str = "X-LSS-Waiting";

pub fn waiting_message(tried: &[u16]) -> String {
    format!(
        "lss-collector is running but has not found an LLM server on this machine yet (asked ports {tried:?}). Start your model server - the collector finds it by itself within 15 s - or pin it with [[engine]] in collector.toml (lss-collector --detect shows what it sees)."
    )
}

/// The placeholder listeners, one thread per `listen` address. `stop` joins them, which drops
/// every listener, so the collector's real HTTP server can bind the same address right after.
pub struct Waiting {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// card #180 gate 2: the addresses this waiter actually got. A bind that fails is not fatal,
    /// but it must not be SILENT: without this, "the collector answers 503 while it waits" could
    /// be false on a machine where something already holds the address, and the only symptom
    /// would be the connection-refused message this whole feature exists to replace.
    bound: Vec<String>,
}

impl Waiting {
    pub fn start(listen: &[String], message: &str) -> Waiting {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut threads = Vec::new();
        let mut bound = Vec::new();
        for addr in listen {
            // a bind that fails here is not fatal: the real server retries its binds too. But it
            // is REPORTED, both to the log and in `bound()` - a waiter that quietly took no
            // address leaves a stranger with exactly the "connection refused" confusion that
            // card #180 gate 6 built this waiter to remove.
            let listener = match std::net::TcpListener::bind(addr) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("lss-collector: cannot answer on {addr} while waiting for an engine ({e}) - something else holds it, so `lss` will report this address as unreachable rather than waiting");
                    continue;
                }
            };
            if let Err(e) = listener.set_nonblocking(true) {
                eprintln!("lss-collector: cannot answer on {addr} while waiting for an engine ({e})");
                continue;
            }
            let stop = stop.clone();
            let body = message.to_string();
            bound.push(addr.clone());
            threads.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                            let mut buf = [0u8; 2048];
                            let _ = stream.read(&mut buf);
                            let reply = format!(
                                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\n{WAITING_HEADER}: engine\r\nRetry-After: 15\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(reply.as_bytes());
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            }));
        }
        Waiting { stop, threads, bound }
    }

    /// The addresses this waiter is actually answering on - never assumed to be everything it was
    /// asked for (see `bound`).
    pub fn bound(&self) -> &[String] {
        &self.bound
    }

    pub fn stop(self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's ephemeral (auto-assigned) port range: Linux publishes it; elsewhere assume the
    /// widest common one (Linux's default lower bound, below macOS's 49152).
    fn ephemeral_range() -> (u16, u16) {
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .ok()
            .and_then(|t| {
                let mut it = t.split_whitespace().filter_map(|x| x.parse::<u16>().ok());
                Some((it.next()?, it.next()?))
            })
            .unwrap_or((32768, 65535))
    }

    /// card #323: a free loopback port the kernel never hands out on its own - below the ephemeral
    /// range, so no `bind(:0)` and no outgoing connection anywhere on the machine can be given it.
    /// Only an explicit bind of this exact number can collide, and the caller retries that.
    fn non_ephemeral_port() -> Option<u16> {
        let (bottom, top) = port_band(ephemeral_range().0)?;
        let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64) ^ u64::from(std::process::id());
        for _ in 0..1000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let port = bottom + ((seed >> 33) % u64::from(top - bottom)) as u16;
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return Some(port);
            }
        }
        None
    }

    /// card #327: the ports `non_ephemeral_port` may pick, `bottom..top`: up to 10,000 ports just
    /// below the ephemeral range (and below 30,000), never under 1024 (binding those needs root).
    /// None = there are none: a kernel whose ephemeral range starts at or below 1025 leaves no
    /// unprivileged port under it. That used to be `top - bottom` on u16 - an underflow panic in
    /// a test build, or `% 0` - on exactly such a kernel.
    fn port_band(ephemeral_lo: u16) -> Option<(u16, u16)> {
        let top = ephemeral_lo.min(30_000);
        let bottom = top.saturating_sub(10_000).max(1024);
        (top > bottom).then_some((bottom, top))
    }

    const NO_BAND: &str = "SKIPPED: this kernel's ephemeral port range starts at or below 1025, so there is no unprivileged port below it to test the handover on (card #327)";

    #[test]
    fn the_port_band_is_empty_not_a_panic_when_the_ephemeral_range_starts_low() {
        // card #327: net.ipv4.ip_local_port_range = "1024 65535" is a legal (if unusual) setting
        assert_eq!(port_band(1024), None);
        assert_eq!(port_band(1), None);
        assert_eq!(port_band(0), None);
        assert_eq!(port_band(1025), Some((1024, 1025)), "one port is still a band");
        assert_eq!(port_band(5000), Some((1024, 5000)));
        assert_eq!(port_band(32768), Some((20_000, 30_000)), "Linux's default range");
        assert_eq!(port_band(49152), Some((20_000, 30_000)), "macOS / IANA default");
    }

    /// card #323: the handover test's port is never one the kernel can auto-assign.
    #[test]
    fn the_handover_port_is_never_one_the_kernel_hands_out_on_its_own() {
        let (lo, _) = ephemeral_range();
        if port_band(lo).is_none() {
            eprintln!("{NO_BAND}");
            return;
        }
        for _ in 0..200 {
            let p = non_ephemeral_port().expect("a free port in the band");
            assert!(p < lo && p >= 1024, "port {p} is inside (or near) the ephemeral range starting at {lo}");
        }
    }

    /// card #323, the forced reuse: the #323 flake's exact shape, made on purpose. Another process's
    /// socket that holds an ephemeral port WITHOUT listening (an outgoing connection's local port,
    /// or any bound-but-idle socket) makes our rebind fail with EADDRINUSE while nothing answers a
    /// connect - indistinguishable, from outside, from our own listener outliving stop(). That is
    /// why the handover test must never pick a port such a socket can get: see the test above.
    #[cfg(unix)]
    #[test]
    fn a_non_listening_stranger_on_the_port_blocks_the_rebind_and_answers_nothing() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // the stranger: a plain socket bound to the port, no SO_REUSEADDR, never listening
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        let sa = libc::sockaddr_in {
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be() },
            sin_zero: [0; 8],
        };
        let rc = unsafe { libc::bind(fd, &sa as *const libc::sockaddr_in as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) };
        if rc != 0 {
            unsafe { libc::close(fd) };
            return; // lost the port before the stranger could take it: nothing to show this run
        }
        let rebind = std::net::TcpListener::bind(("127.0.0.1", port));
        let connect = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(1));
        unsafe { libc::close(fd) };
        assert_eq!(rebind.err().map(|e| e.kind()), Some(std::io::ErrorKind::AddrInUse), "a bound, non-listening stranger blocks the rebind");
        assert!(connect.is_err(), "and nothing answers a connect: the '{{addr}} is still OURS' shape");
    }

    /// card #323 (reopened): does THIS process still hold a socket bound to `port`? The socket
    /// inodes on that local port (/proc/net/tcp and tcp6) against this process's own fds
    /// (/proc/self/fd). None = no /proc (macOS): the caller falls back to asking what answers.
    fn own_socket_on(port: u16) -> Option<bool> {
        let mut inodes = std::collections::HashSet::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(table) else { continue };
            for line in text.lines().skip(1) {
                let w: Vec<&str> = line.split_whitespace().collect();
                let local = w.get(1).and_then(|a| a.rsplit(':').next()).and_then(|h| u16::from_str_radix(h, 16).ok());
                if let (Some(p), Some(inode)) = (local, w.get(9)) {
                    if p == port && *inode != "0" {
                        inodes.insert(format!("socket:[{inode}]"));
                    }
                }
            }
        }
        let fds = std::fs::read_dir("/proc/self/fd").ok()?;
        Some(fds.filter_map(|e| std::fs::read_link(e.ok()?.path()).ok()).any(|l| inodes.contains(&l.display().to_string())))
    }

    /// After `stop()`, `addr` will not rebind: is that OUR doing (the defect the handover test
    /// exists to catch) or somebody else's (that attempt proves nothing)? OURS = our waiter still
    /// answers it, or this process still holds a socket on it. NOT ours = a stranger listening -
    /// or, the #323 reopened flake, a child that ANOTHER test thread forked while our listener was
    /// open: it holds an inherited copy until it execs, so the socket outlives our close() and
    /// the rebind fails, and if it execs before our probe connects, nothing answers either. The
    /// old rule ("unbindable and nothing answers = ours") called that ours: 176 of 300 handovers
    /// under a fork storm, with this process holding no socket on the port in any of them.
    /// Without /proc (macOS) it falls back to that old rule.
    fn still_ours(addr: &str) -> bool {
        use std::io::{Read, Write};
        let port: u16 = addr.rsplit(':').next().and_then(|p| p.parse().ok()).expect("host:port");
        let held_here = own_socket_on(port);
        let answer = std::net::TcpStream::connect(addr).ok().map(|mut probe| {
            let _ = probe.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = probe.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n");
            let mut text = String::new();
            let _ = probe.read_to_string(&mut text);
            text
        });
        if answer.as_deref().is_some_and(|a| a.contains(WAITING_HEADER)) {
            return true; // our waiter outlived stop()
        }
        held_here.unwrap_or(answer.is_none())
    }

    /// card #323 reopened, forced: a child forked while the waiter's listener is open holds an
    /// inherited copy of it, so after `stop()` the address will not rebind; if that child then
    /// goes away (exec or exit) before anyone connects, nothing answers either. That is not our
    /// listener outliving stop() and must not be reported as it - while a socket THIS process
    /// really still holds must be, even one that answers nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_forked_childs_copy_of_the_listener_is_not_ours_but_our_own_socket_is() {
        let Some(port) = non_ephemeral_port() else {
            eprintln!("{NO_BAND}");
            return;
        };
        let addr = format!("127.0.0.1:{port}");
        let w = Waiting::start(std::slice::from_ref(&addr), "waiting");
        if w.bound() != [addr.clone()] {
            w.stop();
            return; // lost the port to another process before the child could inherit it
        }
        let mut pipe = [0i32; 2];
        // O_CLOEXEC: a process another test thread spawns must not keep the write end open
        assert_eq!(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            // the child: holds every fd the parent had (the listener too) until the pipe closes.
            // Only async-signal-safe calls here: read, _exit.
            unsafe {
                libc::close(pipe[1]);
                let mut b = 0u8;
                libc::read(pipe[0], (&mut b as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(pipe[0]) };
        w.stop();
        let rebind = std::net::TcpListener::bind(&addr).map(drop);
        // the child goes away, as a forked child does when it execs
        unsafe {
            libc::close(pipe[1]);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert_eq!(rebind.err().map(|e| e.kind()), Some(std::io::ErrorKind::AddrInUse), "the child's inherited copy kept the address after stop()");
        // nothing answers now - the exact shape the old rule called 'still OURS'. (Another test
        // thread's own child may still hold a copy too, and then a connect lands in its queue:
        // not this shape, so nothing to show this run.)
        if std::net::TcpStream::connect(&addr).is_ok() {
            return;
        }
        assert!(!still_ours(&addr), "a forked child's copy is not our listener outliving stop()");
        // and the defect itself still counts: a socket THIS process holds on a port, silent. (Any
        // port does: `bind(:0)` is atomic, where probe-then-bind is the very race above - a child
        // forked mid-probe still holds the probed port.)
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let held_addr = format!("127.0.0.1:{}", held.local_addr().unwrap().port());
        assert!(still_ours(&held_addr), "a socket this process still holds is ours, answer or not");
        drop(held);
    }

    /// card #180 gate 6: while no engine is found the collector's own address answers 503 with
    /// the waiting header and a sentence saying what to do - and `stop` releases the address so
    /// the real server can bind it.
    #[test]
    fn while_waiting_for_an_engine_the_listen_address_says_so_and_is_released_after() {
        use std::io::{Read, Write};
        // card #180 gate 2: this test was FLAKY - 1 failure in 4 `cargo test --workspace` runs on a
        // 64-core build box, 0 in 6 runs of this binary alone. It asked the OS for a free port, DROPPED the
        // listener, and then reused the NUMBER, so under full-workspace parallelism another test
        // process could take that port in the window. The old code then hid it twice: a failed
        // bind in Waiting::start was a silent `continue`, so the connect below reached whatever
        // else was listening, and only the final rebind assertion failed - blaming the port
        // release, which was never the bug. Now `bound()` says what the waiter really got, and
        // this loop simply moves on to another port when it loses the race. A flaky gate is worse
        // than a red one: gate 2 wants CI green on the commit we tag, and 1-in-4 means a green
        // run proves nothing.
        let mut attempts_lost_to_other_processes = 0;
        // one full attempt per iteration: acquire a port, prove the waiting answer, prove the
        // handover. An attempt lost to another process is retried, never reported as a result.
        loop {
        let mut w = None;
        let mut addr = String::new();
        for _ in 0..25 {
            // card #323: NOT `bind(:0)`. A port from the kernel's ephemeral range can be handed,
            // the moment stop() releases it, to ANY other process's outgoing connection as its
            // local port - a socket that is not listening and has no SO_REUSEADDR, so the rebind
            // below fails with EADDRINUSE while nothing answers a connect: exactly the shape the
            // assertion below (rightly) calls "still OURS". Seen at 7400425: port 36365, inside
            // Linux's 32768-60999. A port BELOW that range is never auto-assigned to anybody.
            let Some(port) = non_ephemeral_port() else {
                eprintln!("{NO_BAND}");
                return;
            };
            addr = format!("127.0.0.1:{port}");
            let candidate = Waiting::start(std::slice::from_ref(&addr), &waiting_message(&[8000, 11434]));
            if candidate.bound() == [addr.clone()] {
                w = Some(candidate);
                break;
            }
            // lost the port to another process: it reported that honestly, so try another
            candidate.stop();
        }
        let w = w.expect("a free port within 25 tries");
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
        assert!(reply.contains(&format!("{WAITING_HEADER}: engine")), "{reply}");
        assert!(reply.contains("has not found an LLM server") && reply.contains("[8000, 11434]"), "{reply}");
        // THIRD ROUND on this one assertion, and the two rounds before it were me guessing at a
        // cause I had not measured. The error, once I actually printed it, is EADDRINUSE - which
        // has TWO possible causes that a bare `expect` cannot tell apart:
        //   1. our own listener is still holding the address after stop() returned. THAT IS THE
        //      PRODUCT DEFECT this assertion exists to catch: the real server binds these
        //      addresses next, with no patience, and a stranger's engine would fail to start.
        //   2. some OTHER process took the port in the gap. The port came from the ephemeral
        //      range and the whole workspace binds :0 constantly under parallel test load, so
        //      this is ordinary environmental noise and says nothing about our code.
        // A test that cannot distinguish them is either flaky (if it fails) or useless (if it
        // retries until it passes), so it now DISTINGUISHES: nothing listening means the address
        // is ours and still held -> fail; something listening means a stranger owns it -> that
        // attempt is void and we start over on a fresh port. 1 in 4 runs before, then 2 in 11
        // after my first "fix", because both attempts treated cause 2 as if it were cause 1.
        drop(s);
        w.stop();
        match std::net::TcpListener::bind(&addr) {
            Ok(_) => break,
            Err(e) => {
                // WHO is holding it decides - see `still_ours`: our own process still holding
                // a socket on it (or our waiter answering) is the defect; a stranger, or another
                // test thread's forked child that inherited our listener and has not exec'd yet
                // (card #323 reopened), is not.
                let ours = still_ours(&addr);
                assert!(
                    !ours,
                    "after stop, {addr} is still OURS - either our waiter answered it or this process still holds a socket on it (without /proc: nothing is listening and it still will not bind). The real server binds these addresses NEXT, with no patience, so this is exactly the handover this test exists to protect: {e}"
                );
                // a stranger is listening: this attempt proved nothing, so run it again
                attempts_lost_to_other_processes += 1;
                assert!(attempts_lost_to_other_processes < 25, "lost the port to other processes 25 times running");
                continue;
            }
        }
        }
    }

    /// card #180 gate 2, the defect BEHIND the flake: a bind Waiting could not get was a silent
    /// `continue`. So on a machine where something already holds the configured address, the
    /// collector reported "no LLM server found ... looking again" and then answered NOTHING - and
    /// the symptom a stranger saw was "collector unreachable", the precise confusion gate 6 built
    /// this waiter to remove. An honest waiter says which addresses it actually took.
    #[test]
    fn an_address_the_waiter_cannot_take_is_reported_not_silently_skipped() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = format!("127.0.0.1:{}", held.local_addr().unwrap().port());
        let free_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let free = format!("127.0.0.1:{free_port}");

        // asked for both; only one is available
        let w = Waiting::start(&[taken.clone(), free.clone()], "waiting");
        assert!(!w.bound().contains(&taken), "it cannot have taken an address someone else holds: {:?}", w.bound());
        assert!(w.bound().len() < 2, "and it must not claim both: {:?}", w.bound());
        w.stop();

        // asked for ONLY the taken one: bound() is empty, which is what lets the caller warn
        // instead of leaving a stranger to guess
        let w = Waiting::start(std::slice::from_ref(&taken), "waiting");
        assert!(w.bound().is_empty(), "a waiter that got nothing must say so, not look healthy: {:?}", w.bound());
        w.stop();
        drop(held);
    }

    use lss_core::engine::EngineIdentity;

    #[test]
    fn a_gateway_in_front_of_a_known_engine_is_not_a_second_server() {
        let f = |kind, port: u16, model: &str| Found { kind, url: format!("http://127.0.0.1:{port}"), why: String::new(), identity: EngineIdentity { model: Some(model.into()), ..Default::default() } };
        let ranked = rank(vec![f(EngineKind::OpenAi, 8096, "glm"), f(EngineKind::Sglang, 8090, "glm"), f(EngineKind::OpenAi, 1234, "other-model"), f(EngineKind::Ollama, 11434, "mistral:latest")]);
        assert_eq!(ranked.iter().map(|x| (x.kind, x.url.as_str())).collect::<Vec<_>>(), vec![(EngineKind::Ollama, "http://127.0.0.1:11434"), (EngineKind::Sglang, "http://127.0.0.1:8090"), (EngineKind::OpenAi, "http://127.0.0.1:1234")]);
        // an engine with nothing loaded (an idle Ollama) never outranks the one that is serving
        let mut idle = f(EngineKind::Ollama, 11435, "x");
        idle.identity.model = None;
        assert_eq!(rank(vec![idle, f(EngineKind::Sglang, 8090, "glm")])[0].kind, EngineKind::Sglang);
    }

    #[test]
    fn a_pinned_engine_is_never_scanned_for_and_our_own_port_is_never_knocked_on() {
        let mut cfg = lss_core::config::parse_config("[[engine]]\nkind = \"ollama\"\nurl = \"http://127.0.0.1:11434/\"\n").unwrap();
        resolve(&mut cfg);
        assert_eq!((cfg.sglang_url.as_str(), cfg.engine_kind.as_str()), ("http://127.0.0.1:11434", "ollama"));
        let mut cfg = lss_core::config::parse_config("engine_url = \"http://127.0.0.1:9\"\nlisten = [\"127.0.0.1:8099\", \"192.0.2.7:8100\"]\n").unwrap();
        resolve(&mut cfg);
        assert_eq!((cfg.sglang_url.as_str(), cfg.engine_kind.as_str()), ("http://127.0.0.1:9", "auto"), "a URL without a kind: recognised when it answers");
        assert_eq!(own_ports(&cfg), vec![8099, 8100]);
        assert!(!candidate_ports(&[8099]).contains(&8099));
    }

    /// A port whose `/metrics` carries `sglang:` series: a real engine, recognised in the
    /// primary (well-known-port) pass exactly as before.
    fn primary_engine(model: &'static str) -> (u16, std::thread::JoinHandle<()>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                if path == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                let body = match path.as_str() {
                    "/v1/models" => format!("{{\"data\":[{{\"id\":\"{model}\"}}]}}"),
                    "/metrics" => "sglang:num_running_reqs{} 0.0\n".to_string(),
                    _ => "{}".to_string(),
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        (port, handle)
    }

    /// A port that only ever answers `/v1/models` a certain way, and records every path it was
    /// asked (`/stop` excluded) - the fake gateway/proxy this card is about.
    fn watched_proxy(model: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                if path == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                hits2.lock().unwrap().push(path.clone());
                let body = if path == "/v1/models" { format!("{{\"data\":[{{\"id\":\"{model}\"}}]}}") } else { "{}".to_string() };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        (port, hits, handle)
    }

    fn stop(port: u16) {
        let agent = ureq::AgentBuilder::new().build();
        let _ = agent.get(&format!("http://127.0.0.1:{port}/stop")).call();
    }

    /// #44 D1, 2026-09-21: live, a zero-config scan added 6 rejected and 4 no-key entries to
    /// our own gateway's audit log - `rank()` always threw the gateway's find away (same model,
    /// generic OpenAI kind), but only after several more requests had already landed on it.
    #[test]
    fn a_secondary_port_serving_a_known_model_is_asked_nothing_beyond_the_one_cheap_question() {
        let (p1, h1) = primary_engine("glm-test");
        let (p2, hits, h2) = watched_proxy("glm-test");
        let found = scan_ports(&[p1], &[p2]);
        stop(p1);
        stop(p2);
        let _ = h1.join();
        let _ = h2.join();
        assert_eq!(found.iter().map(|f| f.url.as_str()).collect::<Vec<_>>(), vec![format!("http://127.0.0.1:{p1}")], "{found:?}");
        assert_eq!(*hits.lock().unwrap(), vec!["/v1/models".to_string()], "the proxy was asked nothing beyond the one cheap question");
    }

    /// A secondary port whose model does NOT match anything already found still gets the full
    /// multi-path detection - this must only ever skip a port `rank()` would drop anyway.
    #[test]
    fn a_secondary_port_serving_an_unmatched_model_still_gets_the_full_detection() {
        let (p1, h1) = primary_engine("glm-test");
        let (p2, h2) = primary_engine("a-different-model"); // a real engine of its own, on an unusual port
        let found = scan_ports(&[p1], &[p2]);
        stop(p1);
        stop(p2);
        let _ = h1.join();
        let _ = h2.join();
        let mut urls: Vec<&str> = found.iter().map(|f| f.url.as_str()).collect();
        urls.sort_unstable();
        let mut want = vec![format!("http://127.0.0.1:{p1}"), format!("http://127.0.0.1:{p2}")];
        want.sort();
        assert_eq!(urls, want, "{found:?}");
    }
}
