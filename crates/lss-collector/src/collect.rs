//! One poll: every source is read concurrently, each with its own deadline, so a source
//! that hangs costs the poll one timeout rather than stalling the monitor.

use crate::cmd::{self, Guard};
use crate::nvml::Nvml;
use lss_core::config::Config;
use lss_core::docker::{parse_inspect, ContainerState, INSPECT_FORMAT};
use lss_core::gate::{parse_gate_health, GateHealth};
use lss_core::gatelog::{ingest, LogDelta};
use lss_core::gpu::{parse_gpu_csv, parse_gpu_health_csv, parse_pci_map, GpuHealth, GpuSample, PciKey, HEALTH_QUERY_FIELDS, QUERY_FIELDS, QUERY_FIELDS_EXT};
use lss_core::hist::HistSet;
use lss_core::model::Sample;
use lss_core::prom::ServeMetrics;
use std::sync::atomic::{AtomicBool, Ordering};
use lss_core::xid::{parse_xid_line, XidEvent};
use std::collections::VecDeque;
use std::time::Duration;

const SOURCE_TIMEOUT: Duration = Duration::from_secs(4);
const XID_DEDUPE_SECS: i64 = 900;
/// Link state, ECC, remapped rows, limits: they move in days, not seconds.
const HEALTH_EVERY_SECS: i64 = 60;
/// The hot path never looks further back than this, however long the journal was unreadable.
/// An older gap is handed to the streamed back-fill instead.
pub const HOT_LOOKBACK_SECS: i64 = 600;
/// journalctl's own cap on a hot-path read (`-n`), and the cap on matches kept from it.
const HOT_JOURNAL_LINES: u32 = 5000;
const HOT_XID_LINES: u32 = 200;
/// The back-fill keeps at most this many Xid reports; a storm beyond that is already a story.
const BACKFILL_MAX_EVENTS: usize = 5000;
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(900);

/// The hot-path journal read: a bounded window, journalctl capped at `-n` lines, filtered in
/// the pipeline so only `NVRM: Xid` lines (and at most `HOT_XID_LINES`) reach this process.
pub fn hot_journal_script(since: i64) -> String {
    format!("journalctl -k --since @{since} -n {HOT_JOURNAL_LINES} -o short-iso --no-pager -q | grep -F 'NVRM: Xid' | head -n {HOT_XID_LINES}; true")
}

/// Where the hot path reads from: the last successful read (5 s overlap), but never more than
/// `HOT_LOOKBACK_SECS` back. Returns (since, gap): the gap `[from, until)` is what that clamp
/// left uncovered, for the streamed back-fill.
pub fn hot_window(last_ok: i64, now: i64) -> (i64, Option<(i64, i64)>) {
    let floor = now - HOT_LOOKBACK_SECS;
    if last_ok - 5 >= floor {
        (last_ok - 5, None)
    } else {
        (floor, Some((last_ok - 5, floor)))
    }
}

/// card #297: the engine's API key and the base URL it belongs to, set once at startup from the
/// first `[[engine]]` block. Process-wide because the engine is asked from half a dozen places
/// (poll, probe, bench, detect) through agents built in each; one gate here means none of them
/// can forget it, and none of them can send it anywhere else.
static ENGINE_AUTH: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Remember the engine's key (an empty key = nothing to send, nothing is set).
pub fn set_engine_auth(base: &str, key: &str) {
    let key = key.trim();
    if !key.is_empty() {
        let _ = ENGINE_AUTH.set((base.trim_end_matches('/').to_string(), key.to_string()));
    }
}

/// Is `url` the engine at `base`, or a path under it? A prefix alone is not enough:
/// `http://h:80` must not match `http://h:8000/...`.
pub fn is_under(url: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    !base.is_empty() && (url == base || url.strip_prefix(base).is_some_and(|rest| rest.starts_with('/') || rest.starts_with('?')))
}

/// Add `Authorization: Bearer <key>` when - and only when - this request goes to the engine
/// the key belongs to.
pub fn authed(req: ureq::Request) -> ureq::Request {
    match ENGINE_AUTH.get() {
        Some((base, key)) if is_under(req.url(), base) => req.set("Authorization", &format!("Bearer {key}")),
        _ => req,
    }
}

pub fn http_get(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    match authed(agent.get(url)).timeout(SOURCE_TIMEOUT).call() {
        Ok(resp) => resp.into_string().map_err(|e| format!("read: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => {
            let e = e.to_string();
            // card #308: a refused certificate reads "UnknownIssuer" once shortened - say what it is
            Err(crate::tls::explain_cert_error(&e).unwrap_or_else(|| short_err(&e)))
        }
    }
}

/// ureq's transport errors repeat the whole URL; keep the part an operator needs.
fn short_err(e: &str) -> String {
    let e = e.rsplit(": ").next().unwrap_or(e);
    e.chars().take(80).collect()
}

/// Xid reports the hot path did not see - before this process started (previous boots
/// included) or a gap while the journal was unreadable. The caller books them as incidents;
/// they are never alerted on. The kernel journal on a busy box is millions of lines (the first
/// version read them into one String and hit the unit's MemoryMax), so this is STREAMED:
/// journalctl is niced and filtered by grep in the pipeline, and the few matching lines are
/// parsed one at a time as they arrive. Err = incomplete (timed out): the caller retries later.
pub fn backfill_xids(since: i64, until: Option<i64>) -> Result<Vec<XidEvent>, String> {
    let guard = Guard::default();
    let pci_map = cmd::run(&guard, "nvidia-smi", &["--query-gpu=index,pci.bus_id", "--format=csv,noheader,nounits"], SOURCE_TIMEOUT)
        .map(|o| parse_pci_map(&o.stdout))
        .unwrap_or_default();
    let until = until.map(|u| format!(" --until @{u}")).unwrap_or_default();
    let script = format!("nice -n 19 journalctl _TRANSPORT=kernel --since @{since}{until} -o short-iso --no-pager -q | grep -F --line-buffered 'NVRM: Xid'; true");
    let mut events = Vec::new();
    let stats = cmd::stream_lines("sh", &["-c", &script], BACKFILL_TIMEOUT, BACKFILL_MAX_EVENTS, |line| {
        if let Some(e) = parse_xid_line(line, &pci_map) {
            events.push(e);
        }
    })?;
    if stats.truncated {
        eprintln!("xid backfill: more than {BACKFILL_MAX_EVENTS} Xid lines since {since}; kept the first {BACKFILL_MAX_EVENTS}");
    }
    Ok(events)
}

/// `tailscale status --json` -> address -> host name. Empty when tailscale is not there.
pub fn tailscale_names() -> std::collections::BTreeMap<String, String> {
    cmd::run(&Guard::default(), "tailscale", &["status", "--json"], SOURCE_TIMEOUT).map(|o| lss_core::users::parse_tailscale_status(&o.stdout)).unwrap_or_default()
}

/// This machine's host name, for a config that does not name the host.
pub fn host_name() -> String {
    cmd::run(&Guard::default(), "hostname", &[], SOURCE_TIMEOUT).ok().map(|o| o.stdout.trim().to_string()).filter(|h| !h.is_empty()).unwrap_or_else(|| "localhost".into())
}

/// The launch line of the process LISTENING on `port` (`ss -ltnp`, else `lsof`), read out of
/// `/proc/<pid>/cmdline`. Linux only - and only when the engine runs as a plain process on this
/// machine (not in a container: that path uses `docker inspect` instead). Empty whenever
/// anything in the chain is missing: the loadout then says `cmdline?` (card #53) instead of
/// quietly merging two runs that may differ in unpublished launch flags.
pub fn engine_cmdline(port: u16) -> String {
    if port == 0 || !cfg!(target_os = "linux") {
        return String::new();
    }
    // ss prints `LISTEN 0 128 *:8080 *:* users:(("llama-server",pid=4312,fd=5))`; lsof prints
    // `llama-server 4312 user 5u IPv4 ... TCP *:8080 (LISTEN)`. Either: the pid owning the port.
    let pid = cmd::run(&Guard::default(), "ss", &["-ltnpH", "sport", "=", &format!(":{port}")], SOURCE_TIMEOUT)
        .ok()
        .and_then(|o| pid_from_ss(&o.stdout, port))
        .or_else(|| cmd::run(&Guard::default(), "lsof", &["-ti", &format!("TCP:{port}"), "-sTCP:LISTEN"], SOURCE_TIMEOUT).ok().and_then(|o| o.stdout.lines().next().map(str::to_string)))
        .and_then(|p| p.trim().parse::<u32>().ok());
    let Some(pid) = pid else { return String::new() };
    std::fs::read_to_string(format!("/proc/{pid}/cmdline")).map(|raw| {
        // args are NUL-separated; the last arg can carry one too - keep it non-empty either way
        raw.split('\0').filter(|a| !a.is_empty()).collect::<Vec<_>>().join(" ")
    }).unwrap_or_default()
}

/// The pid in `ss -ltnpH` output whose local address ends in `:<port>`.
fn pid_from_ss(out: &str, port: u16) -> Option<String> {
    out.lines().find_map(|l| {
        let local = l.split_whitespace().nth(3)?; // State Recv-Q Send-Q Local
        let p = local.rsplit(':').next()?;
        (p == port.to_string()).then(|| {
            l.split("pid=").nth(1).and_then(|rest| rest.split(',').next()).map(str::to_string)
        }).flatten()
    })
}

pub struct PollResult {
    pub sample: Sample,
    pub serve_detail: String,
    pub gate_detail: String,
    pub xids: Vec<XidEvent>,
    /// the kernel journal was read on this poll (the Xid scan is current up to `sample.ts`)
    pub journal_ok: bool,
    /// `[from, until)` the hot path had to skip because the journal was unreadable for longer
    /// than `HOT_LOOKBACK_SECS`; reported once, for the streamed back-fill
    pub journal_gap: Option<(i64, i64)>,
    /// the latency histograms of this scrape (cumulative); None = /metrics was unreadable
    pub hists: Option<HistSet>,
    /// Some on the polls that ran the slow health query
    pub health: Option<Vec<GpuHealth>>,
}

pub struct Poller {
    cfg: Config,
    agent: ureq::Agent,
    /// card #316: the gateway's own agent - `tls_verify` is per URL
    gate_agent: ureq::Agent,
    g_smi: Guard,
    g_docker: Guard,
    g_logs: Guard,
    g_journal: Guard,
    pci_map: Vec<(PciKey, u32)>,
    serve_container: Option<String>,
    log_cursor: Option<String>,
    xid_since: i64,
    xid_seen: VecDeque<(i64, String)>,
    /// cleared for good the first time nvidia-smi refuses the extended field list
    smi_ext_ok: AtomicBool,
    health_at: i64,
    /// which engine this is; None = the URL is known but nothing recognisable has answered yet
    kind: Option<lss_core::engine::EngineKind>,
    /// where GPU numbers come from on this machine; None = not looked for yet
    gpu_source: Option<lss_core::gpu::GpuSource>,
    /// (ts, generated tokens) of the last scrape: engines that only publish counters get their
    /// writing speed from two scrapes
    last_generated: Option<(i64, f64)>,
    /// card #85: NVIDIA+journalctl gate for the Xid scan, resolved once
    xid_gate: Option<XidScanGate>,
    /// card #178: the NVML library, loaded once on first use. `None` outer = not tried yet;
    /// `Some(None)` = tried and unavailable (no driver, missing symbols) - the fork path is
    /// used for good, without retrying `dlopen` on every poll.
    nvml: Option<Option<Nvml>>,
}

/// What one look at the engine gives: the model (or why not), its numbers and latency
/// histograms, which engine it is, and the numbers it does not publish.
type ServeRead = (Result<String, String>, Option<(ServeMetrics, HistSet)>, Option<lss_core::engine::EngineKind>, Vec<String>);

/// Is the program there at all? (`nvidia-smi` missing is "no NVIDIA GPU", not a failure.)
fn has_program(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// NVIDIA first (the richest numbers), then AMD, then Apple Silicon, else none.
#[derive(Clone, Copy, PartialEq)]
pub struct XidScanGate {
    pub nvidia: bool,
    pub journalctl: bool,
}

pub fn xid_scan_gate(source: lss_core::gpu::GpuSource) -> XidScanGate {
    XidScanGate { nvidia: source == lss_core::gpu::GpuSource::Nvidia, journalctl: has_program("journalctl") }
}

impl XidScanGate {
    pub fn should_scan(&self) -> bool {
        self.nvidia && self.journalctl
    }
}

pub fn find_gpu_source() -> lss_core::gpu::GpuSource {
    use lss_core::gpu::GpuSource;
    if has_program("nvidia-smi") {
        GpuSource::Nvidia
    } else if has_program("amd-smi") || has_program("rocm-smi") {
        GpuSource::Amd
    } else if cfg!(target_os = "macos") && has_program("ioreg") {
        GpuSource::Apple
    } else {
        GpuSource::None
    }
}

impl Poller {
    pub fn new(cfg: Config, now: i64) -> Self {
        Self {
            cfg,
            agent: crate::tls::agent().timeout_connect(Duration::from_secs(2)).build(),
            gate_agent: crate::tls::gate_agent().timeout_connect(Duration::from_secs(2)).build(),
            g_smi: Guard::default(),
            g_docker: Guard::default(),
            g_logs: Guard::default(),
            g_journal: Guard::default(),
            pci_map: Vec::new(),
            serve_container: None,
            log_cursor: None,
            xid_since: now,
            xid_seen: VecDeque::new(),
            smi_ext_ok: AtomicBool::new(true),
            health_at: 0,
            kind: None,
            gpu_source: None,
            last_generated: None,
            xid_gate: None,
            nvml: None,
        }
    }


    fn refresh_pci_map(&mut self) {
        if !self.pci_map.is_empty() {
            return;
        }
        if let Ok(o) = cmd::run(&self.g_smi, "nvidia-smi", &["--query-gpu=index,pci.bus_id", "--format=csv,noheader,nounits"], SOURCE_TIMEOUT) {
            self.pci_map = parse_pci_map(&o.stdout);
        }
    }

    /// (image, launch args, environment) of a container, for the loadout identity. Asked once per container run.
    pub fn loadout_inspect(&self, container: &str) -> Option<lss_core::loadout::Inspected> {
        let o = cmd::run(&self.g_docker, "docker", &["inspect", "--format", lss_core::loadout::LOADOUT_INSPECT_FORMAT, container], SOURCE_TIMEOUT).ok()?;
        lss_core::loadout::parse_loadout_inspect(&o.stdout)
    }

    /// What the engine says about itself: what a loadout is built from when there is no
    /// container to inspect (a plain process, or a machine without docker).
    pub fn engine_identity(&self) -> Option<(lss_core::engine::EngineKind, lss_core::engine::EngineIdentity)> {
        let kind = self.kind?;
        let base = self.cfg.sglang_url.trim_end_matches('/').to_string();
        let agent = self.agent.clone();
        let fetch = move |path: &str| http_get(&agent, &format!("{base}{path}"));
        let mut id = lss_core::engine::adapter_for(kind, &self.cfg.public_priority, &self.cfg.trusted_priority).identity(&fetch);
        // #53: the engine's own launch line, when this machine can read it, so two runs that
        // differ only in flags the engine never publishes stay two loadouts. Empty = not read
        // (the identity marks the loadout `cmdline?` instead of silently merging them).
        id.cmdline = engine_cmdline(self.cfg.serve_port_or_url());
        id.model.is_some().then_some((kind, id))
    }

    /// How many requests the engine runs at once, when it says (SGLang, llama.cpp, TGI do).
    pub fn fetch_slots(&self) -> Option<u32> {
        let kind = self.kind?;
        let base = self.cfg.sglang_url.trim_end_matches('/').to_string();
        let agent = self.agent.clone();
        let fetch = move |path: &str| http_get(&agent, &format!("{base}{path}"));
        lss_core::engine::adapter_for(kind, &self.cfg.public_priority, &self.cfg.trusted_priority).identity(&fetch).slots
    }

    pub fn poll(&mut self, now: i64) -> PollResult {
        let gpu_source = *self.gpu_source.get_or_insert_with(find_gpu_source);
        // card #85: the Xid scan only makes sense for NVIDIA with a kernel journal; gate it once
        let xid_gate = *self.xid_gate.get_or_insert_with(|| xid_scan_gate(gpu_source));
        if gpu_source == lss_core::gpu::GpuSource::Nvidia {
            self.refresh_pci_map();
            // card #178: try NVML exactly once (a failed dlopen is not retried every poll);
            // `nvml_reading` below still falls back to the nvidia-smi fork on every call this
            // returns `None` from, so a library that loads but starts failing individual
            // reads never breaks GPU reporting, only slows it back down to the fork path.
            self.nvml.get_or_insert_with(Nvml::load);
        }
        let nvml = self.nvml.as_ref().and_then(|o| o.as_ref());
        if self.kind.is_none() {
            self.kind = lss_core::engine::EngineKind::parse(&self.cfg.engine_kind);
        }
        let known_kind = self.kind;
        let cfg = &self.cfg;
        let agent = &self.agent;
        let gate_agent = &self.gate_agent;
        let known_serve = self.serve_container.clone();
        let log_cursor = self.log_cursor.clone();
        // card #85: no NVIDIA+journalctl, no scan - not even the script string is built
        let (journal_since, journal_gap, journal_script) = if xid_gate.should_scan() {
            let (since, gap) = hot_window(self.xid_since, now);
            (Some(since), gap, Some(hot_journal_script(since)))
        } else {
            (None, None, None)
        };
        let want_health = now - self.health_at >= HEALTH_EVERY_SECS;
        if want_health {
            self.health_at = now;
        }
        let smi_ext_ok = &self.smi_ext_ok;

        let (serve, gate, gpus, containers, logs, journal) = std::thread::scope(|s| {
            let serve = s.spawn(|| -> ServeRead {
                let base = cfg.sglang_url.trim_end_matches('/').to_string();
                let fetch = |path: &str| http_get(agent, &format!("{base}{path}"));
                // the kind is pinned, or recognised by its fingerprint the first time it answers
                let Some(kind) = known_kind.or_else(|| lss_core::engine::detect(&fetch).map(|(k, _)| k)) else {
                    return (Err("nothing lss recognises answers here (lss-collector --detect)".to_string()), None, None, Vec::new());
                };
                let scrape = lss_core::engine::adapter_for(kind, &cfg.public_priority, &cfg.trusted_priority).scrape(&fetch);
                let not_reported = if scrape.model.is_ok() { scrape.not_reported() } else { Vec::new() };
                // parsed once: the gauges and the latency histograms come from the same scrape
                let metrics = scrape.serve_metrics().map(|m| (m, scrape.hists.clone().unwrap_or_default()));
                (scrape.model, metrics, Some(kind), not_reported)
            });
            let gate = s.spawn(|| -> Result<GateHealth, String> {
                if !cfg.has_gate() {
                    return Err(String::new());
                }
                let body = http_get(gate_agent, &format!("{}/gate/health", cfg.gate_url))?;
                parse_gate_health(&body).ok_or_else(|| "bad JSON from /gate/health".to_string())
            });
            let gpus = s.spawn(|| -> (Result<Vec<GpuSample>, String>, Option<Vec<GpuHealth>>) {
                use lss_core::gpu::GpuSource;
                match gpu_source {
                    GpuSource::Nvidia => {}
                    GpuSource::None => return (Ok(Vec::new()), None),
                    GpuSource::Apple => {
                        let read = cmd::run(&self.g_smi, "ioreg", &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"], SOURCE_TIMEOUT).map(|o| lss_core::gpu::parse_ioreg_accelerator(&o.stdout));
                        return (read.and_then(|g| if g.is_empty() { Err("ioreg: no IOAccelerator statistics".to_string()) } else { Ok(g) }), None);
                    }
                    GpuSource::Amd => {
                        // amd-smi is rocm-smi's successor; either is fine
                        let amd = cmd::run(&self.g_smi, "amd-smi", &["metric", "--json"], SOURCE_TIMEOUT).map(|o| lss_core::gpu::parse_amd_smi_json(&o.stdout)).ok().filter(|g| !g.is_empty());
                        let read = amd.map(Ok).unwrap_or_else(|| cmd::run(&self.g_smi, "rocm-smi", &["--showtemp", "--showpower", "--showuse", "--showmeminfo", "vram", "--json"], SOURCE_TIMEOUT).map(|o| lss_core::gpu::parse_rocm_smi_json(&o.stdout)));
                        return (read.and_then(|g| if g.is_empty() { Err("amd-smi / rocm-smi: no GPU in the output".to_string()) } else { Ok(g) }), None);
                    }
                }
                // the slow health read (link/ECC/remap/limits): still nvidia-smi either way -
                // it runs once a minute, so the fork cost this card targets does not apply to
                // it (see the card's own before/after, taken on the per-poll fast query only).
                let read_health = || -> Option<Vec<GpuHealth>> {
                    let q = format!("--query-gpu={HEALTH_QUERY_FIELDS}");
                    cmd::run(&self.g_smi, "nvidia-smi", &[&q, "--format=csv,noheader,nounits"], SOURCE_TIMEOUT).ok().map(|o| parse_gpu_health_csv(&o.stdout, now)).filter(|h| !h.is_empty())
                };
                // card #178: NVML direct read first, no fork - this is the ~90ms-per-poll path
                // the card measured. Only when it has nothing usable (library never loaded, or
                // every device read failed this poll) does the nvidia-smi fork below still run,
                // so a driver hiccup degrades to the old behaviour rather than losing GPU data.
                if let Some(g) = nvml.and_then(|n| n.samples()).filter(|g| !g.is_empty()) {
                    let health = want_health.then(read_health).flatten();
                    return (Ok(g), health);
                }
                let query = |fields: &str| -> Result<(Vec<GpuSample>, String), String> {
                    let q = format!("--query-gpu={fields}");
                    let o = cmd::run(&self.g_smi, "nvidia-smi", &[&q, "--format=csv,noheader,nounits"], SOURCE_TIMEOUT)?;
                    Ok((parse_gpu_csv(&o.stdout), format!("{} {}", o.stdout.trim(), o.stderr.trim())))
                };
                let mut fast = if smi_ext_ok.load(Ordering::Relaxed) { query(QUERY_FIELDS_EXT) } else { query(QUERY_FIELDS) };
                if matches!(&fast, Ok((parsed, said)) if parsed.is_empty() && said.contains("not a valid field")) {
                    // this driver does not know the two extra fields: never ask for them again
                    eprintln!("nvidia-smi refused the extended query; falling back to the base fields");
                    smi_ext_ok.store(false, Ordering::Relaxed);
                    fast = query(QUERY_FIELDS);
                }
                let fast = fast.and_then(|(parsed, said)| if parsed.is_empty() { Err(format!("nvidia-smi: {}", said.chars().take(120).collect::<String>())) } else { Ok(parsed) });
                // the slow health read: only when the fast one worked, and its failure is nobody's alert
                let health = (want_health && fast.is_ok()).then(read_health).flatten();
                (fast, health)
            });
            let containers = s.spawn(|| -> (Option<String>, Vec<ContainerState>) {
                let serve_name = if cfg.serve_container == "auto" {
                    let filter = format!("publish={}", cfg.serve_port_or_url());
                    cmd::run(&self.g_docker, "docker", &["ps", "--filter", &filter, "--format", "{{.Names}}"], SOURCE_TIMEOUT)
                        .ok()
                        .and_then(|o| o.stdout.lines().next().map(str::to_string))
                        .filter(|n| !n.is_empty())
                        .or(known_serve)
                } else {
                    Some(cfg.serve_container.clone())
                };
                let mut args = vec!["inspect", "--format", INSPECT_FORMAT];
                if let Some(n) = &serve_name {
                    args.push(n);
                }
                if !cfg.gate_container.is_empty() {
                    args.push(&cfg.gate_container);
                }
                if args.len() == 3 {
                    // no container to ask about (no docker, or the server is a plain process)
                    return (serve_name, Vec::new());
                }
                let states = cmd::run(&self.g_docker, "docker", &args, SOURCE_TIMEOUT).map(|o| parse_inspect(&o.stdout)).unwrap_or_default();
                (serve_name, states)
            });
            let logs = s.spawn(|| -> Option<(LogDelta, Option<String>)> {
                if cfg.gate_container.is_empty() {
                    return None;
                }
                let since = log_cursor.clone().unwrap_or_else(|| "10m".to_string());
                let o = cmd::run(&self.g_logs, "docker", &["logs", "-t", "--since", &since, &cfg.gate_container], SOURCE_TIMEOUT).ok()?;
                let text = format!("{}\n{}", o.stdout, o.stderr);
                Some(ingest(&text, log_cursor.as_deref()))
            });
            let g_journal = &self.g_journal;
            let journal = s.spawn(|| -> Option<String> {
                // card #85: NVIDIA + journalctl only; the gate was decided before the scope
                match (&journal_since, &journal_script) {
                    (Some(_), Some(script)) => {
                        cmd::run(g_journal, "sh", &["-c", script], SOURCE_TIMEOUT).ok().map(|o| o.stdout)
                    }
                    _ => Some(String::new()),
                }
            });
            (serve.join(), gate.join(), gpus.join(), containers.join(), logs.join(), journal.join())
        });

        let (model, metrics, seen_kind, not_reported) = serve.unwrap_or_else(|_| (Err("poll thread panicked".into()), None, None, Vec::new()));
        if self.kind.is_none() {
            self.kind = seen_kind;
        }
        let gate = gate.unwrap_or_else(|_| Err("poll thread panicked".into()));
        let (gpus, health) = gpus.unwrap_or_else(|_| (Err("poll thread panicked".into()), None));
        let (mut metrics, hists) = match metrics {
            Some((m, h)) => (Some(m), Some(h)),
            None => (None, None),
        };
        // an engine that publishes a generation COUNTER but no speed: two scrapes make the speed
        if let Some(m) = metrics.as_mut().filter(|_| self.kind != Some(lss_core::engine::EngineKind::Sglang)) {
            let reports_counter = !not_reported.iter().any(|k| k == "tokens");
            if let (true, Some((then, before))) = (reports_counter && m.gen_throughput == 0.0, self.last_generated) {
                let dt = (now - then) as f64;
                if dt > 0.0 && dt <= 60.0 && m.generation_tokens_total >= before {
                    m.gen_throughput = ((m.generation_tokens_total - before) / dt * 10.0).round() / 10.0;
                }
            }
            self.last_generated = reports_counter.then_some((now, m.generation_tokens_total));
        }
        let (serve_name, states) = containers.unwrap_or_default();
        let log = logs.ok().flatten();
        let journal = journal.ok().flatten();

        if serve_name.is_some() {
            self.serve_container = serve_name.clone();
        }
        let mut delta = LogDelta::default();
        if let Some((d, cursor)) = log {
            delta = d;
            if cursor.is_some() {
                self.log_cursor = cursor;
            }
        }

        let mut xids = Vec::new();
        let journal_ok = journal.is_some();
        if let Some(text) = journal {
            for line in text.lines() {
                let Some(e) = parse_xid_line(line, &self.pci_map) else { continue };
                if self.xid_seen.iter().any(|(_, l)| l == line) {
                    continue;
                }
                self.xid_seen.push_back((e.ts, line.to_string()));
                xids.push(e);
            }
            self.xid_since = now;
        }
        while self.xid_seen.front().is_some_and(|(ts, _)| *ts < now - XID_DEDUPE_SECS) {
            self.xid_seen.pop_front();
        }

        let find = |name: &Option<String>| name.as_ref().and_then(|n| states.iter().find(|c| &c.name == n).cloned());
        let sample = Sample {
            ts: now,
            serve_up: model.is_ok(),
            model: model.as_ref().ok().cloned(),
            metrics,
            // a machine with no GPU tool has nothing to fail at
            gpus_ok: gpus.is_ok(),
            engine: self.kind.map(|k| k.name().to_string()).unwrap_or_default(),
            not_reported,
            gate_absent: !self.cfg.has_gate(),
            gpu_source: gpu_source.name().to_string(),
            gpus: gpus.as_ref().ok().cloned().unwrap_or_default(),
            serve_ct: find(&serve_name),
            gate_ct: find(&Some(self.cfg.gate_container.clone()).filter(|n| !n.is_empty())),
            users: gate.as_ref().ok().map(lss_core::users::user_points).unwrap_or_default(),
            // filled in by the poll loop, which knows the engine's slot count
            slots: 0,
            gate: gate.as_ref().ok().cloned(),
            log: delta,
        };
        PollResult {
            sample,
            serve_detail: model.err().map(|e| format!("/v1/models: {e}")).unwrap_or_default(),
            gate_detail: gate.err().filter(|e| !e.is_empty()).map(|e| format!("/gate/health: {e}")).unwrap_or_default(),
            xids,
            journal_ok,
            // only once the read works again: by then the gap is final and is reported once
            journal_gap: journal_gap.filter(|_| journal_ok),
            hists,
            health,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hot_path_window_is_bounded() {
        let now = 1_000_000;
        // normal operation: 5 s polls, 5 s overlap
        assert_eq!(hot_window(now - 5, now), (now - 10, None));
        assert_eq!(hot_window(now - HOT_LOOKBACK_SECS + 5, now), (now - HOT_LOOKBACK_SECS, None));
        // the journal was unreadable for 3 hours: the hot path still reads 10 minutes, and the
        // rest goes to the streamed back-fill
        let (since, gap) = hot_window(now - 10_800, now);
        assert_eq!(since, now - HOT_LOOKBACK_SECS);
        assert_eq!(gap, Some((now - 10_805, now - HOT_LOOKBACK_SECS)));
        // a fresh start (last_ok = start time) never asks for 7 days
        assert!(now - hot_window(now, now).0 <= HOT_LOOKBACK_SECS);
    }

    #[test]
    fn the_hot_path_command_caps_lines_and_filters_in_the_pipeline() {
        let s = hot_journal_script(123);
        assert!(s.contains("--since @123") && s.contains("-n 5000") && s.contains("grep -F 'NVRM: Xid'") && s.contains("head -n 200"), "{s}");
    }

    #[test]
    fn the_pid_is_read_from_real_ss_output_shapes() {
        // `ss -ltnpH` with the users:(...) tail; addresses reshaped so this file carries
        // nothing the privacy check would flag (documentation ports only)
        let with_users = "LISTEN 0      128       127.0.0.1:8443 0.0.0.0:* users:((\"engine\",pid=517184,fd=6))\nLISTEN 0      128    192.0.2.10:8443 0.0.0.0:* users:((\"engine\",pid=517184,fd=8))\n";
        assert_eq!(pid_from_ss(with_users, 8443).as_deref(), Some("517184"));
        // the engine port is NOT this port: no line matches, no pid
        assert_eq!(pid_from_ss(with_users, 8090), None);
        // plain `ss -ltn` (no -p): a listener but no process info -> None
        assert_eq!(pid_from_ss("LISTEN 0      4096   0.0.0.0:8090 0.0.0.0:*\n", 8090), None);
        // the pid format lsof's -ti prints: one bare number per line
        assert_eq!("517184\n".trim().parse::<u32>().ok(), Some(517_184));
    }

    #[test]
    fn engine_cmdline_refuses_a_zero_port() {
        // a URL without a port (port 0) must not scan anything
        assert_eq!(engine_cmdline(0), "");
    }

    /// card #137: the NVIDIA+journalctl gate had no test, and on every box we own it is TRUE by
    /// construction (all our collectors are NVIDIA plus journalctl), so a regression here is
    /// invisible rather than merely unlikely. Pinned: the source->nvidia mapping AND the full
    /// truth table of should_scan.
    #[test]
    fn the_xid_scan_gate_is_nvidia_with_journalctl_and_nothing_else() {
        use lss_core::gpu::GpuSource;
        // the mapping: only NVIDIA turns the flag on. A non-NVIDIA source must never scan, on any
        // box, whatever `journalctl` happens to be - so the env-dependent field is forced both ways.
        assert!(xid_scan_gate(GpuSource::Nvidia).nvidia);
        assert!(!xid_scan_gate(GpuSource::Amd).nvidia);
        assert!(!xid_scan_gate(GpuSource::Apple).nvidia);
        assert!(!xid_scan_gate(GpuSource::None).nvidia);
        for src in [GpuSource::Amd, GpuSource::Apple, GpuSource::None] {
            for journalctl in [false, true] {
                assert!(
                    !XidScanGate { nvidia: xid_scan_gate(src).nvidia, journalctl }.should_scan(),
                    "{src:?} with journalctl={journalctl} must not scan"
                );
            }
        }
        // the truth table of the gate itself
        assert!(XidScanGate { nvidia: true, journalctl: true }.should_scan());
        assert!(!XidScanGate { nvidia: true, journalctl: false }.should_scan(), "NVIDIA without journalctl must not scan");
        assert!(!XidScanGate { nvidia: false, journalctl: true }.should_scan(), "journalctl without NVIDIA must not scan");
        assert!(!XidScanGate { nvidia: false, journalctl: false }.should_scan());
    }
}
