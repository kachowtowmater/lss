//! `lss bench`, the collector's half: it runs the harness (`llm_decode_bench.py`) as a child
//! process, watches the server every 2 s while it runs, and turns what it wrote into a scorecard.
//!
//! SAFETY, in the order it is enforced:
//!   1. idle gate: refuse to start unless the server has been idle (`bench::idle_gate`), or
//!      `--force` / `--under-load`;
//!   2. one bench at a time: an in-process flag AND a lock file (a second collector, a stale run);
//!   3. abort on real traffic: every `poll`, `bench::abort_reason`; the harness's whole process
//!      group is killed and the run is recorded `aborted: real traffic …`. `--under-load` stands
//!      this down ON PURPOSE (card #14) - a run that kills itself the moment somebody else sends
//!      a request cannot measure under load - and every poll's reading is recorded on the
//!      scorecard's `background` block instead, so the numbers carry the conditions they were
//!      taken in;
//!   4. a hard timeout per profile;
//!   5. the harness never updates itself: its stdin is /dev/null, so its "upgrade and restart?"
//!      prompt reads EOF and it carries on with the version on disk (recorded in the scorecard);
//!   6. if the collector dies, the harness dies with it (PR_SET_PDEATHSIG), and the next start
//!      closes the run as aborted.
//!
//! WHERE THE LOAD GOES: the harness can only set an `Authorization` header, not `X-LSS-Bench`,
//! so it is pointed straight at the ENGINE (`sglang_url`), bypassing the gateway. That is what
//! makes the abort watch sharp: while the harness runs, ANY request the engine labels with a
//! gateway lane's priority is somebody else's. The sanity checks and the needle are the
//! collector's own requests and DO go through the gateway's trusted port with `X-LSS-Bench: 1`.

use crate::db::Db;
use crate::Shared;
use lss_core::bench::{self, BenchBrief, BenchRequest, BenchStarted, LastRun, LoadSampler, NeedleResult, Profile, Role, SafetyObs, Scorecard, Step};
use lss_core::config::{expand_home, Config};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the poll loop and the HTTP threads share about the bench.
#[derive(Default)]
pub struct BenchState {
    pub brief: BenchBrief,
    /// a bench is running: the incident window is open, the C1 probe is paused, the queue / C1
    /// rules stand down
    pub active: bool,
    pub cancel: bool,
    /// bumped whenever a run starts or finishes (the poll loop re-reads the runs)
    pub generation: u64,
    /// when the bench's own gateway requests were sent (so they are not counted as real traffic
    /// by the NEXT idle gate)
    pub own_requests: Vec<i64>,
}

pub struct BenchCtx {
    pub cfg: Config,
    pub home: String,
    pub db: Arc<Mutex<Db>>,
    pub shared: Arc<Mutex<Shared>>,
    /// where the lock file lives (the database's directory)
    pub state_dir: PathBuf,
    /// the safety poll (`[bench] poll_secs`; tests use milliseconds)
    pub poll: Duration,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum StepEnd {
    Done,
    Aborted(String),
    Timeout,
    Failed(String),
}

/// `[bench] harness` resolved and checked: Ok(path) or the sentence to show the person.
pub fn harness_path(cfg: &Config, home: &str) -> Result<String, String> {
    let raw = cfg.bench.harness.trim();
    if raw.is_empty() {
        return Err("the benchmark harness is not set up. `lss bench` wraps llm_decode_bench.py (github.com/local-inference-lab/llm-inference-bench): clone it on this machine and set its path as `[bench] harness = \"…\"` in the collector's config, then restart the collector".into());
    }
    let path = expand_home(raw, home);
    if !Path::new(&path).is_file() {
        return Err(format!("the benchmark harness is not at {path} (`[bench] harness` in the collector's config): clone github.com/local-inference-lab/llm-inference-bench there, or fix the path"));
    }
    Ok(path)
}

/// card #333: `[bench] harness` for a RUN, which goes through `[bench] launcher`. Over ssh the
/// harness is on the far side: `~/…` is left for the REMOTE shell (bench::harness_word) and the
/// file is looked for THERE (`[ssh…] test -f`), never on this machine. Otherwise `harness_path`.
pub fn run_harness_path(cfg: &Config, home: &str) -> Result<String, String> {
    let launcher = &cfg.bench.launcher;
    if !bench::launcher_joins(launcher) || cfg.bench.harness.trim().is_empty() {
        return harness_path(cfg, home);
    }
    let word = bench::harness_word(launcher, &cfg.bench.harness, home);
    let mut argv = launcher.clone();
    // the answer is a word on stdout, not ssh's exit code: ssh's own warnings share stderr, and
    // its own failure (255) must not read as "the file is not there"
    argv.extend(["test".to_string(), "-f".into(), bench::remote_word(&word), "&&".into(), "echo".into(), "lss-harness-found".into(), "||".into(), "echo".into(), "lss-harness-missing".into()]);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    match crate::cmd::run(&crate::cmd::Guard::default(), &argv[0], &args, Duration::from_secs(30)) {
        Ok(o) if o.stdout.contains("lss-harness-found") => Ok(word),
        Ok(o) if !o.stdout.contains("lss-harness-missing") => Err(format!("could not check the benchmark harness over the ssh launcher ({}): {}", launcher.join(" "), o.stderr.trim().lines().last().unwrap_or("no answer"))),
        Ok(_) => Err(format!("the benchmark harness is not at {word} on the remote side of the ssh launcher ({}; `~` there is the REMOTE user's home): clone github.com/local-inference-lab/llm-inference-bench there, or fix `[bench] harness`", launcher.join(" "))),
        Err(e) => Err(format!("could not check the benchmark harness over the ssh launcher: {e}")),
    }
}

// ------------------------------------------------------------------ the lock

pub(crate) fn lock_path(ctx: &BenchCtx) -> PathBuf {
    ctx.state_dir.join("bench.lock")
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists. EPERM means it exists but belongs
    // to someone else: that is still a live holder of the lock.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Takes the lock file, or says who holds it. A lock left by a process that no longer exists is
/// taken over.
pub(crate) fn take_lock(ctx: &BenchCtx, profile: &str, now: i64) -> Result<(), String> {
    let path = lock_path(ctx);
    let _ = std::fs::create_dir_all(&ctx.state_dir);
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "{}", serde_json::json!({"pid": std::process::id(), "profile": profile, "started_at": now}));
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let held: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                let pid = held["pid"].as_i64().unwrap_or(0) as i32;
                if pid_alive(pid) {
                    return Err(format!("a benchmark is already running ({}, started {}s ago, pid {pid}): one at a time. `lss bench status` shows it, `lss bench cancel` stops it", held["profile"].as_str().unwrap_or("?"), now - held["started_at"].as_i64().unwrap_or(now)));
                }
                eprintln!("bench: taking over a stale lock left by pid {pid}");
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => return Err(format!("cannot create the bench lock {}: {e}", path.display())),
        }
    }
    Err("cannot take the bench lock".into())
}

pub(crate) fn release_lock(ctx: &BenchCtx) {
    let _ = std::fs::remove_file(lock_path(ctx));
}

/// At collector start: a lock or a `running` row left behind by a collector that died mid-bench.
pub fn recover(ctx: &BenchCtx, now: i64) {
    let path = lock_path(ctx);
    if path.exists() {
        let held: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let pid = held["pid"].as_i64().unwrap_or(0) as i32;
        if !pid_alive(pid) || pid == std::process::id() as i32 {
            let _ = std::fs::remove_file(&path);
        }
    }
    let db = lock(&ctx.db);
    match db.bench_close_stale(now, "aborted: the collector restarted while the benchmark was running") {
        Ok(n) if n > 0 => eprintln!("bench: {n} run(s) left unfinished by the last collector closed as aborted"),
        _ => {}
    }
    let _ = db.close_incident(lss_core::incidents::KIND_BENCH, now);
}

// ------------------------------------------------------------------ starting

/// `POST /bench`: check, lock, record, and start the run on its own thread.
pub fn request_start(ctx: &Arc<BenchCtx>, req: &BenchRequest) -> (u16, BenchStarted) {
    // `under_load` is echoed on the REFUSAL too: that is where a version skew first bites (an
    // older collector cannot refuse differently, so the caller needs the echo to tell the two
    // apart) - card #14, 2026-09-23.
    let refuse = |code: u16, message: String| (code, BenchStarted { v: lss_core::STATUS_SCHEMA_VERSION, ok: false, run_id: None, message, under_load: req.under_load });
    let Some(profile) = Profile::parse(if req.profile.is_empty() { "quick" } else { &req.profile }) else {
        return refuse(400, format!("unknown profile `{}`: use quick, full, accuracy or dry-run", req.profile));
    };
    if !req.dataset.is_empty() && !bench::DATASETS.contains(&req.dataset.as_str()) {
        return refuse(400, format!("unknown dataset `{}`: use {}", req.dataset, bench::DATASETS.join(", ")));
    }
    // no external harness is fine: the built-in mini bench runs instead (except `accuracy`,
    // which needs the harness's datasets, and `dry-run`, which shows the harness's own help)
    let harness = match run_harness_path(&ctx.cfg, &ctx.home) {
        Ok(p) => Some(p),
        // a path that is SET but wrong is a mistake to fix, not a reason to measure differently
        Err(e) if !ctx.cfg.bench.harness.trim().is_empty() || matches!(profile, Profile::Accuracy | Profile::DryRun) => return refuse(409, e),
        Err(_) => None,
    };
    let now = crate::unix_now();
    let (idle, model, slots, loadout_id) = {
        let s = lock(&ctx.shared);
        if s.bench.active {
            return refuse(409, format!("a benchmark is already running ({}): one at a time. `lss bench status` shows it, `lss bench cancel` stops it", s.bench.brief.profile.clone().unwrap_or_default()));
        }
        (bench::IdleObs { serve_up: s.serve_up, running: s.running, queue: s.queue, tokens_generated: s.recent_tokens_generated, idle_secs: ctx.cfg.bench.idle_secs, other_users_inflight: s.other_users_inflight }, s.model.clone(), s.slots, s.loadout_id.clone())
    };
    // card #14: --under-load is a deliberate "measure it with the traffic on it", so it passes
    // the gate the same way --force does. What it does NOT do is hide that fact: the scorecard
    // carries `under_load` and the sampled `background` block, and every screen prints it.
    if let Err(mut why) = bench::idle_gate(&idle, req.force || req.under_load) {
        // card #331: a serve that is down BECAUSE it refuses our key says so here too
        if !idle.serve_up {
            if let Some(reason) = lock(&ctx.shared).serve_down_reason.clone() {
                why = format!("{why} - {reason}");
            }
        }
        return refuse(409, why);
    }
    let Some(model) = model else { return refuse(409, "the serve has no model loaded: nothing to benchmark".into()) };
    if let Err(why) = take_lock(ctx, profile.name(), now) {
        return refuse(409, why);
    }
    let steps = match &harness {
        Some(_) => bench::plan(profile, &ctx.cfg.bench, &model, slots, &ctx.cfg.sglang_url, &req.dataset),
        None => bench::plan_builtin(profile, slots),
    };
    let harness = harness.unwrap_or_default();
    let mut card = Scorecard {
        loadout_id: loadout_id.unwrap_or_default(),
        model: model.clone(),
        profile: profile.name().into(),
        started_at: now,
        status: "running".into(),
        forced: req.force || req.under_load,
        under_load: req.under_load,
        note: req.note.chars().filter(|c| !c.is_control()).take(200).collect(),
        target: {
            // card #84: name only what actually ran, with separators, and no gateway clause
            // when there is no gateway (a stranger's scorecard read 'the gateway's trusted
            // port ()' - a URL that does not exist).
            let mut parts: Vec<String> = Vec::new();
            if harness.is_empty() {
                parts.push(format!("built-in mini bench (no external harness configured): lss's own requests to {}/v1/chat/completions with X-LSS-Bench: 1", ctx.cfg.chat_base()));
            } else {
                parts.push(format!("engine direct ({}): the harness cannot send X-LSS-Bench, so it bypasses the gateway", ctx.cfg.sglang_url));
            }
            if ctx.cfg.has_gate() {
                parts.push(format!("sanity checks and the needle go through the gateway's trusted port ({}) with X-LSS-Bench: 1", ctx.cfg.gate_url));
            } else {
                parts.push("sanity checks and the needle go straight to the engine (no gateway configured), with X-LSS-Bench: 1".to_string());
            }
            parts.join(" · ")
        },
        ..Default::default()
    };
    let run_id = match lock(&ctx.db).bench_start(&card.loadout_id, &card.profile, now, &card) {
        Ok(id) => id,
        Err(e) => {
            release_lock(ctx);
            return refuse(500, format!("cannot record the run: {e}"));
        }
    };
    card.run_id = run_id;
    {
        let mut s = lock(&ctx.shared);
        s.bench.active = true;
        s.bench.cancel = false;
        s.bench.generation += 1;
        s.bench.brief.state = "running".into();
        s.bench.brief.profile = Some(profile.name().into());
        s.bench.brief.started_at = Some(now);
        s.bench.brief.step = None;
        s.bench.brief.step_index = 0;
        s.bench.brief.steps = steps.len() as u32;
    }
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let finished = run(&ctx2, profile, steps, card, &harness);
        finish(&ctx2, finished);
    });
    let watch_note = if req.under_load {
        "It runs to the end WITH whatever else is on the server, and the scorecard records what that was."
    } else {
        "It stops by itself if anyone else uses the server."
    };
    (202, BenchStarted { v: lss_core::STATUS_SCHEMA_VERSION, ok: true, run_id: Some(run_id), message: format!("benchmark `{}` started on {model} (run {run_id}). {watch_note}", profile.name()), under_load: req.under_load })
}

/// `POST /bench/cancel`
pub fn request_cancel(ctx: &BenchCtx) -> (u16, BenchStarted) {
    let mut s = lock(&ctx.shared);
    let (ok, message) = if s.bench.active {
        s.bench.cancel = true;
        (true, "stopping the benchmark: the harness is being killed".to_string())
    } else {
        (false, "no benchmark is running".to_string())
    };
    (if ok { 202 } else { 409 }, BenchStarted { v: lss_core::STATUS_SCHEMA_VERSION, ok, run_id: None, message, under_load: false })
}

fn finish(ctx: &BenchCtx, card: Scorecard) {
    if let Err(e) = lock(&ctx.db).bench_finish(card.run_id, &card) {
        eprintln!("bench: cannot store run {}: {e}", card.run_id);
    }
    eprintln!("bench: run {} ({}) {} in {}s{}", card.run_id, card.profile, card.status, card.duration_s, card.aborted.as_ref().map(|a| format!(" - {a}")).unwrap_or_default());
    prune_raw(ctx);
    release_lock(ctx);
    let mut s = lock(&ctx.shared);
    s.bench.active = false;
    s.bench.cancel = false;
    s.bench.generation += 1;
    s.bench.brief.state = "idle".into();
    s.bench.brief.profile = None;
    s.bench.brief.started_at = None;
    s.bench.brief.step = None;
    if card.profile != Profile::DryRun.name() || !card.complete() {
        s.bench.brief.last = Some(LastRun::of(&card));
    }
}

/// Keeps the newest `keep_runs` raw result directories.
fn prune_raw(ctx: &BenchCtx) {
    let root = PathBuf::from(expand_home(&ctx.cfg.bench.results_dir, &ctx.home));
    let mut dirs: Vec<(i64, PathBuf)> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.split('-').next()?.parse::<i64>().ok()?, e.path())))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, old) in dirs.into_iter().skip(ctx.cfg.bench.keep_runs.max(1) as usize) {
        let _ = std::fs::remove_dir_all(old);
    }
}

// ------------------------------------------------------------------ running

struct Watch<'a> {
    ctx: &'a BenchCtx,
    agent: ureq::Agent,
    /// card #316: the gateway's own agent (`tls_verify` is per URL)
    gate_agent: ureq::Agent,
    deadline: Instant,
    /// card #14: the run was asked to measure WITH other traffic. The watch still looks at
    /// everything, every poll - it just records what it sees instead of killing the run for it.
    under_load: bool,
    /// what every poll saw of everybody else: the scorecard's `background` block
    load: LoadSampler,
    /// the collector's own requests: in flight, just ended, or abandoned client-side
    own: OwnLoad,
    // the bench window, from the SERVER's counters
    first: Option<(f64, f64)>,
    last: Option<(f64, f64)>,
    kv_tokens: f64,
    joules: f64,
    energy_secs: f64,
    last_power_at: Option<Instant>,
}

impl Watch<'_> {
    fn get(&self, url: &str) -> Option<String> {
        // card #316: the gateway's URLs go through the gateway's TLS setting
        let agent = if crate::tls::target_of(url, self.ctx.cfg.gate_url.trim().trim_end_matches('/')) == crate::tls::Target::Gate { &self.gate_agent } else { &self.agent };
        crate::collect::authed(agent.get(url)).call().ok()?.into_string().ok()
    }

    /// One safety poll: what the engine and the gateway show right now.
    fn observe(&mut self) -> SafetyObs {
        let cfg = &self.ctx.cfg;
        // whatever the engine: its adapter says how many requests run (None = it does not say,
        // and then only the gateway - if there is one - can show somebody else's traffic)
        let base = cfg.sglang_url.trim_end_matches('/').to_string();
        let metrics = {
            let fetch = |path: &str| self.get(&format!("{base}{path}")).ok_or_else(|| "unreachable".to_string());
            lss_core::engine::EngineKind::parse(&cfg.engine_kind).or_else(|| lss_core::engine::detect(&fetch).map(|(k, _)| k)).and_then(|kind| {
                let scrape = lss_core::engine::adapter_for(kind, &cfg.public_priority, &cfg.trusted_priority).scrape(&fetch);
                let reports_load = scrape.metrics.as_ref().is_some_and(|m| m.running.is_some());
                scrape.serve_metrics().filter(|_| reports_load)
            })
        };
        let gate = if cfg.has_gate() { self.get(&format!("{}/gate/health", cfg.gate_url.trim_end_matches('/'))).and_then(|t| lss_core::gate::parse_gate_health(&t)) } else { None };
        if let Some(m) = &metrics {
            let point = (m.generation_tokens_total, m.spec_verify_calls_total);
            self.first.get_or_insert(point);
            self.last = Some(point);
            self.kv_tokens = self.kv_tokens.max(m.max_total_tokens);
        }
        // energy: the poll loop's newest GPU power reading, integrated at this cadence
        let power = lock(&self.ctx.shared).power_w;
        let now = Instant::now();
        if let (Some(w), Some(at)) = (power, self.last_power_at) {
            let dt = now.duration_since(at).as_secs_f64().min(30.0);
            self.joules += w * dt;
            self.energy_secs += dt;
        }
        self.last_power_at = Some(now);
        let own = self.own.count(now, metrics.as_ref().map(|m| m.running));
        SafetyObs {
            running: metrics.as_ref().map(|m| m.running),
            queue: metrics.as_ref().map(|m| m.queue),
            // card #207: kept as TWO numbers, not one sum. The trusted lane's counter also holds
            // the harness's own direct-to-engine requests (SGLang labels a priority-less request
            // `priority="0"`, which is the trusted lane); the public lane's never does. Summed,
            // the innocent half was dragged down by the guilty half and every plain run aborted
            // on its own traffic.
            gateway_public: metrics.as_ref().map(|m| m.running_public + m.queue_public),
            gateway_trusted: metrics.as_ref().map(|m| m.running_trusted + m.queue_trusted),
            other_users_inflight: gate.as_ref().and_then(|g| g.users.as_ref()).map(|users| users.iter().filter(|u| !(u.lane == "trusted" && u.user == lss_core::users::BENCH_USER)).map(|u| u.inflight).sum()),
            own_gateway_inflight: own,
            // card #212: from the lane config - which counter the harness's own load lands in
            own_lane: bench::own_lane_for(&cfg.public_priority, &cfg.trusted_priority),
        }
    }

    /// Some(why) = stop now.
    fn must_stop(&mut self, max_concurrency: u32) -> Option<StepEnd> {
        if lock(&self.ctx.shared).bench.cancel {
            return Some(StepEnd::Aborted("cancelled by the operator".into()));
        }
        if Instant::now() >= self.deadline {
            return Some(StepEnd::Timeout);
        }
        let obs = self.observe();
        // EVERY poll is recorded, whichever mode this is: a quiet run's scorecard has to be able
        // to say "nothing else was here, and here is how many times I looked" just as loudly as
        // a loaded one says the opposite.
        self.load.push(&obs, max_concurrency);
        if self.under_load {
            return None;
        }
        bench::abort_reason(&obs, max_concurrency).map(|why| StepEnd::Aborted(format!("aborted: {why}")))
    }

    /// After a harness step its streams may still be winding down on the engine: wait for it to
    /// go quiet, so the next (smaller) step is not mistaken for foreign traffic.
    fn settle(&mut self, allowed: u32) -> Option<StepEnd> {
        let until = Instant::now() + Duration::from_secs(60).min(self.ctx.poll * 30);
        loop {
            if let Some(end) = self.must_stop(allowed) {
                return Some(end);
            }
            // under load the engine never goes quiet, and nothing here can be mistaken for
            // foreign traffic because nothing aborts: waiting the full minute after every step
            // would only add minutes of nothing to the run.
            if self.under_load {
                return None;
            }
            let quiet = self.get(&format!("{}/metrics", self.ctx.cfg.sglang_url.trim_end_matches('/'))).map(|t| lss_core::prom::extract_serve_metrics(&t, &self.ctx.cfg.public_priority, &self.ctx.cfg.trusted_priority)).is_none_or(|m| m.running < 0.5 && m.queue < 0.5);
            if quiet || Instant::now() >= until {
                return None;
            }
            std::thread::sleep(self.ctx.poll);
        }
    }
}

fn kill_group(pid: u32) {
    // SAFETY: plain syscalls on a process group we created; a stale id at worst returns ESRCH.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// card #312: ask the ENGINE, with the key the harness will be given, whether it will be let in:
/// `GET /v1/models`, and when that is open a 1-token chat request (an engine may guard only its
/// inference routes). Err = the hint (`bench::preflight_refusal`); any other answer is not a key
/// problem and is left to the harness. The key is sent, never logged or returned.
pub(crate) fn preflight(ctx: &BenchCtx, model: &str) -> Result<(), String> {
    let base = ctx.cfg.sglang_url.trim_end_matches('/');
    let key = ctx.cfg.engine_api_key.trim();
    let agent = crate::tls::agent_for(base).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(60)).build();
    let prepare = |r: ureq::Request| {
        let r = r.set("User-Agent", concat!("lss-preflight/", env!("CARGO_PKG_VERSION"))).set(bench::BENCH_HEADER, "1");
        if key.is_empty() { r } else { r.set("Authorization", &format!("Bearer {key}")) }
    };
    let status = |res: Result<ureq::Response, ureq::Error>| match res {
        Ok(r) => Some(r.status()),
        Err(ureq::Error::Status(code, _)) => Some(code),
        Err(_) => None,
    };
    let models = status(prepare(agent.get(&format!("{base}/v1/models"))).call());
    if let Some(hint) = models.and_then(|c| bench::preflight_refusal("/v1/models", c, !key.is_empty())) {
        return Err(hint);
    }
    if models == Some(200) {
        let body = serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1, "temperature": 0, "stream": false, "user": lss_core::users::BENCH_USER});
        let chat = status(prepare(agent.post(&format!("{base}/v1/chat/completions"))).set("Content-Type", "application/json").send_string(&body.to_string()));
        if let Some(hint) = chat.and_then(|c| bench::preflight_refusal("/v1/chat/completions", c, !key.is_empty())) {
            return Err(hint);
        }
    }
    Ok(())
}

/// card #330: one result file from the far side of an ssh launcher into the local run directory:
/// `[launcher…] cat R/FILE`, stdout streamed into `FILE.part` (a result file can be large - no
/// in-memory cap), renamed into place only when the copy succeeded; ssh's own complaints go to
/// harness.log. A copy that has not finished in 5 minutes is killed.
fn fetch_remote(launcher: &[String], rdir: &str, file: &str, dir: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let argv = bench::remote_fetch(launcher, rdir, file);
    let (dest, part) = (dir.join(file), dir.join(format!("{file}.part")));
    let why = |e: String| format!("the harness ran over the ssh launcher, but its result {file} could not be copied back: {e} (see harness.log in {})", dir.display());
    let out = std::fs::File::create(&part).map_err(|e| why(format!("{}: {e}", part.display())))?;
    let err = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("harness.log")).map_err(|e| why(e.to_string()))?;
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(out).stderr(err).process_group(0).spawn().map_err(|e| why(format!("cannot start `{}`: {e}", argv[0])))?;
    let until = std::time::Instant::now() + Duration::from_secs(300);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                kill_group(child.id());
                let _ = child.wait();
                let _ = std::fs::remove_file(&part);
                return Err(why("the copy did not finish in 5 minutes".into()));
            }
            Err(e) => return Err(why(e.to_string())),
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(why(format!("`cat` on the far side exited with {status}")));
    }
    std::fs::rename(&part, &dest).map_err(|e| why(e.to_string()))
}

/// card #340: 16 hex digits from /dev/urandom for a run's remote directory, so two collectors
/// (each with its own run 1) benching as the same remote user never share one. Without
/// /dev/urandom: the clock's nanoseconds, the pid and a counter - still distinct per run here.
fn run_token() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok() {
        return b.iter().map(|x| format!("{x:02x}")).collect();
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    format!("{:016x}", nanos ^ (u64::from(std::process::id()) << 32) ^ SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// One harness invocation, watched. Its output goes to `harness.log` in the run directory.
fn run_harness(watch: &mut Watch, dir: &Path, argv: &[String], stdin: Option<&str>, max_concurrency: u32) -> StepEnd {
    use std::os::unix::process::CommandExt;
    let log = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("harness.log")) {
        Ok(f) => f,
        Err(e) => return StepEnd::Failed(format!("cannot write {}/harness.log: {e}", dir.display())),
    };
    let err_log = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return StepEnd::Failed(format!("harness.log: {e}")),
    };
    let mut cmd = Command::new(&argv[0]);
    // stdin: /dev/null (the harness's self-update prompt reads EOF), or card #312's key line then EOF
    cmd.args(&argv[1..]).current_dir(dir).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(log).stderr(err_log).env("NO_COLOR", "1").env("TERM", "dumb").env("PYTHONUNBUFFERED", "1").process_group(0);
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and touches no memory of ours. If the collector dies,
    // the harness gets SIGTERM instead of loading the server unsupervised.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return StepEnd::Failed(format!("cannot start `{}`: {e}", bench::redact_argv(&argv[..argv.len().min(3)]).join(" "))),
    };
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        // one short line: fits the pipe buffer, so this never blocks; dropping `pipe` closes it
        let _ = pipe.write_all(text.as_bytes());
    }
    let pid = child.id();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return StepEnd::Done,
            Ok(Some(status)) => return StepEnd::Failed(format!("the harness exited with {status}: see harness.log in {}", dir.display())),
            Ok(None) => {}
            Err(e) => return StepEnd::Failed(format!("waiting for the harness: {e}")),
        }
        if let Some(end) = watch.must_stop(max_concurrency) {
            kill_group(pid);
            let _ = child.wait();
            return end;
        }
        std::thread::sleep(watch.ctx.poll);
    }
}

/// One of the collector's own requests through the gateway's trusted port, watched while it runs.
/// Returns (HTTP status, body as JSON, seconds) or the reason the step must end.
fn gateway_request(watch: &mut Watch, url: String, body: serde_json::Value, timeout: Duration) -> Result<(u16, serde_json::Value, f64), StepEnd> {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let agent = crate::tls::agent_for(&url).timeout_connect(Duration::from_secs(5)).timeout(timeout).build();
        let result = crate::collect::authed(agent.post(&url)).set(bench::BENCH_HEADER, "1").set("Content-Type", "application/json").set("User-Agent", concat!("lss-bench/", env!("CARGO_PKG_VERSION"))).send_string(&body.to_string());
        let (code, text, gave_up) = match result {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default(), false),
            Err(ureq::Error::Status(code, r)) => {
                let mut text = String::new();
                let _ = r.into_reader().take(4096).read_to_string(&mut text);
                (code, text, false)
            }
            Err(e) => (0, serde_json::json!({"error": e.to_string()}).to_string(), client_timed_out(&e)),
        };
        let _ = tx.send((code, text, gave_up));
    });
    watch.own.start(1);
    lock(&watch.ctx.shared).bench.own_requests.push(crate::unix_now());
    let mut gave_up = false;
    let out = loop {
        match rx.recv_timeout(watch.ctx.poll) {
            Ok((code, text, timed)) => {
                gave_up = timed;
                break Ok((code, serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)), started.elapsed().as_secs_f64()));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(end) = watch.must_stop(1) {
                    // the request cannot be recalled; it is one request and it ends by itself
                    break Err(end);
                }
            }
            Err(_) => break Ok((0, serde_json::json!({"error": "the request thread died"}), started.elapsed().as_secs_f64())),
        }
    };
    // card #213: a request that gave up CLIENT-side (status 0 at its own timeout) is still
    // running on the engine - it stays the bench's until the engine shows it gone
    let abandoned = matches!(&out, Ok((code, _, secs)) if abandoned(*code, gave_up, *secs, timeout));
    watch.own.finish(Instant::now(), watch.ctx.poll, u64::from(abandoned));
    out
}

/// `bodies.len()` requests at once (the built-in mini bench's concurrency sweep), watched like
/// every other bench request: real traffic aborts the step.
fn parallel_requests(watch: &mut Watch, url: &str, bodies: Vec<serde_json::Value>, timeout: Duration) -> Result<Vec<(u16, serde_json::Value, f64)>, StepEnd> {
    let n = bodies.len();
    let (tx, rx) = std::sync::mpsc::channel();
    for body in bodies {
        let (tx, url) = (tx.clone(), url.to_string());
        std::thread::spawn(move || {
            let started = Instant::now();
            let agent = crate::tls::agent_for(&url).timeout_connect(Duration::from_secs(5)).timeout(timeout).build();
            let result = crate::collect::authed(agent.post(&url)).set(bench::BENCH_HEADER, "1").set("Content-Type", "application/json").set("User-Agent", concat!("lss-bench/", env!("CARGO_PKG_VERSION"))).send_string(&body.to_string());
            let (code, text, gave_up) = match result {
                Ok(r) => (r.status(), r.into_string().unwrap_or_default(), false),
                Err(ureq::Error::Status(code, _)) => (code, String::new(), false),
                Err(e) => (0, serde_json::json!({"error": e.to_string()}).to_string(), client_timed_out(&e)),
            };
            let _ = tx.send(((code, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null), started.elapsed().as_secs_f64()), gave_up));
        });
        lock(&watch.ctx.shared).bench.own_requests.push(crate::unix_now());
    }
    drop(tx);
    watch.own.start(n as u64);
    let mut out = Vec::with_capacity(n);
    let end = loop {
        match rx.recv_timeout(watch.ctx.poll) {
            Ok(r) => {
                out.push(r);
                if out.len() == n {
                    break Ok(out);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(stop) = watch.must_stop(0) {
                    break Err(stop);
                }
            }
            Err(_) => break Ok(out),
        }
    };
    // card #213: each request that timed out on the client is still running on the engine
    let abandoned = match &end {
        Ok(rs) => rs.iter().filter(|((code, _, secs), gave_up)| abandoned(*code, *gave_up, *secs, timeout)).count() as u64,
        Err(_) => 0,
    };
    watch.own.finish(Instant::now(), watch.ctx.poll, abandoned);
    end.map(|rs| rs.into_iter().map(|(r, _)| r).collect())
}

/// card #213: a request that came back with no HTTP status at (or within a second of) its own
/// client timeout gave up on the CLIENT - the engine is still running it.
pub(crate) fn timed_out(secs: f64, timeout: Duration) -> bool {
    secs + 1.0 >= timeout.as_secs_f64()
}

/// card #236: whether a request is ABANDONED - still running on the engine after the client
/// gave up. Status 0 covers every transport error, and elapsed time alone cannot tell "ureq's
/// deadline fired" from "the connection was reset half a second before it would have" - the
/// second one is a request the engine is NOT running, and booking it as the bench's hides that
/// much real traffic from the abort watch. So it needs both: the error really was the client's
/// timeout (`client_timed_out`), AND it came at the timeout, not at the 5 s connect timeout.
pub(crate) fn abandoned(code: u16, client_timed_out: bool, secs: f64, timeout: Duration) -> bool {
    code == 0 && client_timed_out && timed_out(secs, timeout)
}

/// card #236: ureq 2 reports its read/overall deadline as a transport error whose cause is an
/// `io::Error` of kind `TimedOut` (it normalises the platform's `WouldBlock` to that, stream.rs).
/// A refused connection, a reset, an early close or a DNS failure carries some other cause.
pub(crate) fn client_timed_out(e: &ureq::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(c) = cause {
        if c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut) {
            return true;
        }
        cause = c.source();
    }
    false
}

/// The bench's OWN requests as the abort watch must count them (card #213). Three kinds:
/// in flight now; just finished (one-poll grace, so a response racing the next scrape is not a
/// stranger); and ABANDONED - timed out on the client but still running on the engine. Before
/// this, an abandoned request fell out of the count the moment the client gave up, and the next
/// poll read the engine still serving it as somebody else's traffic and killed the run.
/// Abandoned requests stay the bench's until the engine's own running total proves they finished
/// (it drops to what the bench still has in flight); they can never hide more foreign traffic
/// than the number of requests the bench itself abandoned.
#[derive(Debug, Clone)]
pub(crate) struct OwnLoad {
    inflight: u64,
    grace_until: Instant,
    abandoned: u64,
}

impl OwnLoad {
    pub(crate) fn new(now: Instant) -> Self {
        OwnLoad { inflight: 0, grace_until: now, abandoned: 0 }
    }

    pub(crate) fn start(&mut self, n: u64) {
        self.inflight = n;
    }

    /// The requests returned; `abandoned` of them gave up client-side and are still running.
    pub(crate) fn finish(&mut self, now: Instant, poll: Duration, abandoned: u64) {
        self.inflight = 0;
        self.grace_until = now + poll * 2;
        self.abandoned += abandoned;
    }

    /// How many requests on the engine right now are the bench's own. `engine_running` settles
    /// the abandoned ones: once the engine runs no more than the bench still has in flight, they
    /// are finished (or cancelled) and stop counting.
    pub(crate) fn count(&mut self, now: Instant, engine_running: Option<f64>) -> u64 {
        if engine_running.is_some_and(|r| r <= self.inflight as f64 + 0.5) {
            self.abandoned = 0;
        }
        let live = if self.inflight > 0 { self.inflight } else { u64::from(now < self.grace_until) };
        live + self.abandoned
    }
}

fn run(ctx: &BenchCtx, profile: Profile, steps: Vec<Step>, mut card: Scorecard, harness: &str) -> Scorecard {
    let started = Instant::now();
    let model_dir: String = card.model.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).take(40).collect();
    let dir = PathBuf::from(expand_home(&ctx.cfg.bench.results_dir, &ctx.home)).join(format!("{}-{}-{model_dir}", card.run_id, profile.name()));
    card.raw_dir = dir.display().to_string();
    let mut end = StepEnd::Done;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        end = StepEnd::Failed(format!("cannot create {}: {e}", dir.display()));
    }
    let mut watch = Watch {
        ctx,
        agent: crate::tls::agent().timeout_connect(Duration::from_secs(2)).timeout(Duration::from_secs(4)).build(),
        gate_agent: crate::tls::gate_agent().timeout_connect(Duration::from_secs(2)).timeout(Duration::from_secs(4)).build(),
        deadline: started + Duration::from_secs(profile.timeout_secs(&ctx.cfg.bench)),
        under_load: card.under_load,
        load: LoadSampler::default(),
        own: OwnLoad::new(started),
        first: None,
        last: None,
        kv_tokens: 0.0,
        joules: 0.0,
        energy_secs: 0.0,
        last_power_at: None,
    };
    if bench::launcher_joins(&ctx.cfg.bench.launcher) {
        // card #339: over ssh the checkout is on the far side - asked there, through the same
        // launcher; a local `git -C` of that path was always empty. Unreadable = said so.
        let argv = bench::remote_commit_argv(&ctx.cfg.bench.launcher, harness);
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let out = crate::cmd::run(&crate::cmd::Guard::default(), &argv[0], &args, Duration::from_secs(30)).ok().filter(|o| o.success);
        card.harness_commit = bench::remote_commit(out.as_ref().map(|o| o.stdout.as_str()));
    } else if let Some(commit) = Path::new(harness).parent().and_then(|d| crate::cmd::run(&crate::cmd::Guard::default(), "git", &["-C", &d.display().to_string(), "rev-parse", "--short", "HEAD"], Duration::from_secs(5)).ok()).filter(|o| o.success) {
        card.harness_commit = commit.stdout.trim().to_string();
    }
    // card #312 (verifier FAIL 1): the engine is asked, WITH the key the harness will get, before
    // any step that talks to it - a refused key must stop the run with a hint, and the real
    // harness does not reliably say so itself
    if matches!(end, StepEnd::Done) && steps.iter().any(|s| matches!(s, Step::Harness { role, .. } if *role != Role::DryRun)) {
        if let Err(hint) = preflight(ctx, &card.model) {
            eprintln!("bench: run {}: {hint}", card.run_id);
            end = StepEnd::Failed(hint);
        }
    }
    let chat_url = format!("{}/v1/chat/completions", ctx.cfg.chat_base());
    let mut dry_run_help = String::new();
    // card #330: over an ssh launcher the steps run on the far side, in a per-run directory there;
    // each result file is copied back through the same launcher, and the directory removed after
    // card #340: plus a random token - the run name is unique only within this collector
    let remote_dir = bench::launcher_joins(&ctx.cfg.bench.launcher).then(|| bench::remote_run_dir(&dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), &run_token()));
    for (i, step) in steps.iter().enumerate() {
        if !matches!(end, StepEnd::Done) {
            break;
        }
        {
            let mut s = lock(&ctx.shared);
            s.bench.brief.step = Some(step.name());
            s.bench.brief.step_index = i as u32 + 1;
        }
        eprintln!("bench: run {} step {}/{}: {}", card.run_id, i + 1, steps.len(), step.name());
        match step {
            Step::Harness { args, role, max_concurrency, output, .. } => {
                // card #312: a keyed engine needs its key on the harness's own requests too - on
                // STDIN, never on the command line (bench::harness_launch)
                let launch = bench::harness_launch(&ctx.cfg.bench, harness, args, *role, &ctx.cfg.engine_api_key);
                let argv = match &remote_dir {
                    Some(rd) => bench::in_remote_dir(&ctx.cfg.bench.launcher, launch.argv, rd),
                    None => launch.argv,
                };
                end = run_harness(&mut watch, &dir, &argv, launch.stdin.as_deref(), *max_concurrency);
                // card #330: the result is on the other machine - bring it back (also after a
                // stopped step: a partial file still shows its finished cells)
                if let (Some(rd), false) = (&remote_dir, output.is_empty()) {
                    if let Err(e) = fetch_remote(&ctx.cfg.bench.launcher, rd, output, &dir) {
                        if matches!(end, StepEnd::Done) {
                            end = StepEnd::Failed(e);
                        }
                    }
                }
                // ...and the harness does not fail on a refused key (it exits 0): its log says so
                if matches!(end, StepEnd::Done) && *role != Role::DryRun {
                    if let Some(line) = bench::harness_refused(&std::fs::read_to_string(dir.join("harness.log")).unwrap_or_default()) {
                        let hint = if ctx.cfg.engine_api_key.trim().is_empty() { "this engine wants an API key: set api_key in its [[engine]] block (lss-collector setup)" } else { "the [[engine]] api_key was refused: check it" };
                        end = StepEnd::Failed(format!("the engine refused the harness ({line}) - {hint}"));
                    }
                }
                if *role == Role::DryRun {
                    dry_run_help = std::fs::read_to_string(dir.join("harness.log")).unwrap_or_default();
                } else if matches!(end, StepEnd::Done) {
                    if let Some(stop) = watch.settle(*max_concurrency) {
                        end = stop;
                    }
                }
            }
            Step::Mini { kind, .. } => match kind {
                bench::MiniKind::Warmup => {
                    if let Err(stop) = gateway_request(&mut watch, chat_url.clone(), bench::mini_decode_body(&card.model, 16), Duration::from_secs(300)) {
                        end = stop;
                    }
                }
                bench::MiniKind::Decode { concurrency, max_tokens } => match parallel_requests(&mut watch, &chat_url, (0..*concurrency).map(|_| bench::mini_decode_body(&card.model, *max_tokens)).collect(), Duration::from_secs(600)) {
                    Ok(results) => {
                        card.decode.push(bench::grade_mini_decode(*concurrency, &results));
                        if let Some(stop) = watch.settle(0) {
                            end = stop;
                        }
                    }
                    Err(stop) => end = stop,
                },
                bench::MiniKind::Prefill { tokens } => match gateway_request(&mut watch, chat_url.clone(), bench::mini_prefill_body(&card.model, *tokens), Duration::from_secs(600)) {
                    Ok((code, response, secs)) => card.prefill.extend(bench::grade_mini_prefill(*tokens, code, &response, secs)),
                    Err(stop) => end = stop,
                },
            },
            Step::Sanity => {
                for (name, body) in bench::sanity_requests(&card.model) {
                    match gateway_request(&mut watch, chat_url.clone(), body, Duration::from_secs(120)) {
                        Ok((code, response, _)) => card.sanity.push(bench::grade_sanity(name, code, &response)),
                        Err(stop) => {
                            end = stop;
                            break;
                        }
                    }
                }
            }
            Step::Garble { prompts, max_tokens } => {
                let mut texts: Vec<String> = Vec::new();
                for body in bench::garble_requests(&card.model, *prompts, *max_tokens) {
                    match gateway_request(&mut watch, chat_url.clone(), body, Duration::from_secs(300)) {
                        Ok((200, response, _)) => texts.push(bench::answer_text(&response)),
                        Ok(_) => {}
                        Err(stop) => {
                            end = stop;
                            break;
                        }
                    }
                }
                // what was written before an abort still counts
                card.garbled = bench::scan_garbled(&texts);
                let _ = std::fs::write(dir.join("garble-outputs.json"), serde_json::to_string(&texts).unwrap_or_default());
            }
            Step::Needle { tokens, depths } => {
                for depth in depths {
                    let body = serde_json::json!({"model": card.model, "messages": [{"role": "user", "content": bench::needle_prompt(*tokens, *depth)}], "temperature": 0, "max_tokens": 512, "stream": false, "user": lss_core::users::BENCH_USER});
                    let mut got = gateway_request(&mut watch, chat_url.clone(), body.clone(), Duration::from_secs(900));
                    let mut via = "";
                    if let Ok((code, _, _)) = &got {
                        if matches!(code, 413 | 429 | 400) {
                            // the gateway's own limit is not the model's: ask the engine directly, and say so
                            via = " (the gateway refused a prompt this large; sent straight to the engine)";
                            got = gateway_request(&mut watch, format!("{}/v1/chat/completions", ctx.cfg.sglang_url.trim_end_matches('/')), body, Duration::from_secs(900));
                        }
                    }
                    match got {
                        Ok((code, response, secs)) => {
                            let mut r: NeedleResult = bench::grade_needle(*depth, code, &response, secs);
                            r.detail.push_str(via);
                            card.needle.push(r);
                        }
                        Err(stop) => {
                            end = stop;
                            break;
                        }
                    }
                }
            }
        }
    }

    if let Some(rd) = &remote_dir {
        let argv = bench::remote_cleanup(&ctx.cfg.bench.launcher, rd);
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        if let Err(e) = crate::cmd::run(&crate::cmd::Guard::default(), &argv[0], &args, Duration::from_secs(60)).and_then(|o| if o.success { Ok(()) } else { Err(o.stderr) }) {
            eprintln!("bench: run {}: could not remove the remote run directory {rd}: {}", card.run_id, e.trim());
        }
    }
    // what the harness wrote, whatever happened: a partial run still shows its finished cells
    let read = |name: &str| -> Option<serde_json::Value> { serde_json::from_str(&std::fs::read_to_string(dir.join(name)).ok()?).ok() };
    if let Some(doc) = read("decode.json") {
        card.decode = bench::decode_cells(&doc);
        card.harness_version = bench::harness_version(&doc).unwrap_or_default();
    }
    if let Some(doc) = read(bench::LONG_DECODE_FILE) {
        // the long-conversation cells (one user, 64k / 128k already read) join the matrix
        card.decode.extend(bench::decode_cells(&doc));
        card.decode.sort_by_key(|c| (c.context, c.concurrency));
        card.decode.dedup_by_key(|c| (c.context, c.concurrency));
    }
    if let Some(doc) = read("prefill.json") {
        card.prefill = bench::prefill_cells(&doc);
        if card.harness_version.is_empty() {
            card.harness_version = bench::harness_version(&doc).unwrap_or_default();
        }
    }
    for set in bench::DATASETS {
        let file = format!("accuracy-{set}.json");
        if let Some(a) = read(&file).and_then(|doc| bench::accuracy_of(&doc, set, &dir.join(&file).display().to_string())) {
            card.accuracy.push(a);
        }
    }
    // card #14: what else was on the box for the whole run, from the SAME safety polls that
    // decide whether to abort - one collection path, not a second one invented for the report.
    let engine_tok_s = match (watch.first, watch.last) {
        (Some(first), Some(last)) => {
            let secs = started.elapsed().as_secs_f64();
            (secs > 0.0).then(|| (last.0 - first.0).max(0.0) / secs)
        }
        _ => None,
    };
    card.background = watch.load.finish(engine_tok_s);
    if let (Some(first), Some(last)) = (watch.first, watch.last) {
        let generated = (last.0 - first.0).max(0.0);
        card.accept_length = bench::accept_length(generated, (last.1 - first.1).max(0.0));
        if watch.joules > 0.0 && generated > 0.0 && profile != Profile::DryRun {
            card.tokens_per_joule = Some((generated / watch.joules * 10_000.0).round() / 10_000.0);
            card.wh_per_mtok = Some((watch.joules / 3600.0 / (generated / 1e6) * 10.0).round() / 10.0);
            card.avg_watts = Some((watch.joules / watch.energy_secs.max(1e-9) * 10.0).round() / 10.0);
        }
    }
    if watch.kv_tokens > 0.0 {
        card.kv_tokens = Some(watch.kv_tokens);
    }
    match end {
        StepEnd::Done => {
            let nothing = profile != Profile::DryRun && card.decode.is_empty() && card.prefill.is_empty() && card.accuracy.is_empty();
            if nothing {
                card.status = "failed".into();
                card.aborted = Some(format!("the harness finished but wrote no result this version of lss understands: see {}", dir.display()));
            } else if profile == Profile::DryRun && !dry_run_help.contains("--concurrency") {
                card.status = "failed".into();
                card.aborted = Some(format!("`{} {harness} --help` did not print the harness's usage: wrong interpreter, or its dependencies (httpx, rich) are missing. See {}/harness.log", ctx.cfg.bench.python, dir.display()));
            } else {
                card.status = "ok".into();
            }
        }
        StepEnd::Aborted(why) => {
            card.status = "aborted".into();
            card.aborted = Some(why);
        }
        StepEnd::Timeout => {
            card.status = "timeout".into();
            card.aborted = Some(format!("timeout after {}s (the `{}` profile's hard limit): the harness was killed", profile.timeout_secs(&ctx.cfg.bench), profile.name()));
        }
        StepEnd::Failed(why) => {
            card.status = "failed".into();
            card.aborted = Some(why);
        }
    }
    card.ended_at = crate::unix_now();
    card.duration_s = started.elapsed().as_secs() as i64;
    card.duration_ms = Some(started.elapsed().as_millis() as i64);
    card
}

// ------------------------------------------------------------------ accuracy A/B

/// `POST /bench/compare-accuracy` `{a, b}`: the harness's own paired comparison (McNemar) of two
/// stored accuracy result files. No load: it only reads the two files.
pub fn compare_accuracy(ctx: &BenchCtx, a: &str, b: &str) -> (u16, String) {
    let bad = |code: u16, msg: String| (code, format!("{}\n", serde_json::json!({"v": 1, "ok": false, "message": msg})));
    let harness = match harness_path(&ctx.cfg, &ctx.home) {
        Ok(p) => p,
        Err(e) => return bad(409, e),
    };
    let root = PathBuf::from(expand_home(&ctx.cfg.bench.results_dir, &ctx.home));
    let root = root.canonicalize().unwrap_or(root);
    // only files this collector's benchmarks wrote
    let inside = |p: &str| Path::new(p).canonicalize().ok().filter(|c| c.starts_with(&root) && c.is_file());
    let (Some(a), Some(b)) = (inside(a), inside(b)) else {
        return bad(400, format!("both files must be accuracy results under {}", root.display()));
    };
    let out = root.join(format!("compare-{}.json", crate::unix_now()));
    let (a, b, o) = (a.display().to_string(), b.display().to_string(), out.display().to_string());
    let mut argv: Vec<String> = vec![ctx.cfg.bench.python.clone(), harness];
    argv.extend(["--compare-baseline", &b, "--compare-candidate", &a, "--display-mode", "plain", "--output", &o].map(String::from));
    let refs: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let run = crate::cmd::run_env(&crate::cmd::Guard::default(), &argv[0], &refs, &[("NO_COLOR", "1"), ("TERM", "dumb")], Duration::from_secs(120));
    let comparison: serde_json::Value = std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    match run {
        Ok(r) if r.success => (200, format!("{}\n", serde_json::json!({"v": 1, "ok": true, "text": r.stdout, "comparison": comparison["comparison"]}))),
        Ok(r) => bad(502, format!("the harness could not compare the two files: {}", r.stderr.lines().last().unwrap_or("no output"))),
        Err(e) => bad(502, e),
    }
}
