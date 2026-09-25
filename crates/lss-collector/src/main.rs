//! lss-collector - the always-on half of LLM SERVER STATUS. Runs on the GPU box as a
//! systemd user service; see README.md and docs/RUNBOOK.md.

mod advice_run;
mod alert;
mod bench_run;
#[cfg(test)]
mod bench_run_tests;
mod cmd;
mod collect;
mod cost_run;
mod cost_setup_run;
mod db;
mod detect;
mod http;
mod loadouts;
mod nvml;
mod probe_run;
mod query;
mod rollup;
mod setup_run;
// card #323: a port where nothing answers, held for the test (shared with tests/)
#[cfg(test)]
#[path = "../tests/common/test_ports.rs"]
mod test_ports;
mod tls;
// card #336: executable test files are written by a child process (Text file busy)
#[cfg(test)]
#[path = "../tests/common/exec_file.rs"]
mod exec_file;
mod tokens_run;
mod watch_run;

use collect::{Poller, find_gpu_source, xid_scan_gate};
use db::{Db, Retention};
use lss_core::config::{expand_home, parse_config, Config};
use lss_core::history::{History, StatusInputs, HISTORY_SECS};
use lss_core::incidents::{IncidentInput, IncidentOp, IncidentTracker, KIND_CONTAINER_RESTART};
use lss_core::model::Thresholds;
use lss_core::probe::ProbeRecord;
use lss_core::rules::{Engine, GpuObs, Observation, RestartObs};
use lss_core::timeutil::local_midnight;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// LOCK ORDER (card #109) - THE RULE FOR THIS BINARY: the long-lived mutexes are acquired in
/// this order and never the reverse:
///
///   **`db` -> `shared` -> `tailscale`**
///
/// A lock may be taken while holding one EARLIER in that list; taking an earlier one while
/// holding a later one is the bug. Simplest way to comply: read what you need, drop the guard,
/// then take the next lock.
///
/// Why an ORDER and not "never nest": the poll loop legitimately holds the `db` guard across a
/// whole cycle and publishes into `shared` inside it, so a no-nesting rule would demand
/// restructuring the loop. What actually had to go was the INVERSION - until card #109 the
/// startup path below held `shared` and locked `db` inside it, the opposite of the poll loop,
/// which is the classic ABBA pair: both orders present in production code, deadlocking the
/// moment the two run at once. The startup path now reads from `db` first. Both this rule and
/// card #79's are enforced by tests, not by memory:
///   - `tests/deadlock_regression.rs::no_statement_locks_the_same_mutex_twice` (card #79)
///     catches two locks on the SAME mutex in one statement.
///   - `tests/deadlock_regression.rs::locks_are_taken_in_the_declared_order` (card #109)
///     catches the pair being taken in opposite orders in two places.
///
/// `std::sync::Mutex` is not reentrant, and a guard temporary created inside an expression
/// lives until the END of the enclosing statement - which is why both failures looked like a
/// hang with no error anywhere.
///
/// card #261: the idle TTFT - the median of the last valid probes' TTFT, once there are enough
/// of them to mean anything.
fn ttft_baseline(db: &Db) -> Option<f64> {
    let v = db.recent_valid_ttft_ms(12).unwrap_or_default();
    (v.len() >= lss_core::probe::C1_TTFT_BASELINE_MIN_PROBES).then(|| lss_core::rules::median(&v))
}

/// What the poll loop publishes for the HTTP and probe threads.
#[derive(Default)]
pub struct Shared {
    pub status_json: String,
    pub metrics_text: String,
    pub last_sample_ts: i64,
    pub serve_up: bool,
    /// card #331: `status.serve.down_reason`, for the bench's "not up" refusal
    pub serve_down_reason: Option<String>,
    pub model: Option<String>,
    pub running: f64,
    pub queue: f64,
    /// filled by the probe thread, drained by the poll loop
    pub probe_results: Vec<ProbeRecord>,
    /// the C1 speed this server usually reaches, as the rule engine has learned it: the probe
    /// thread uses it to recognise an answer that arrived in one burst (added 2026-09-20)
    pub c1_baseline: Option<f64>,
    /// card #261: the median TTFT of recent valid probes - how fast this engine starts answering
    /// when it is idle. The probe thread marks a probe `contended` past 1.5x of it.
    pub c1_ttft_baseline_ms: Option<f64>,
    /// filled by `POST /test-alert`, drained by the poll loop into the rule engine
    pub test_alerts: Vec<String>,
    /// `GET /rules`, pre-rendered like `/status`
    pub rules_json: String,
    /// the 10-minute gate-log bucket still open in the poll loop (for `GET /gateway`)
    pub gate_open: Option<(i64, lss_core::gatelog::LogDelta)>,
    /// `GET /loadouts`, `GET /tokens`, `GET /advice` and `GET /bench`, pre-rendered
    pub loadouts_json: String,
    pub tokens_json: String,
    pub advice_json: String,
    pub bench_json: String,
    /// `lss bench`: is one running, and what its runner and the poll loop tell each other
    pub bench: bench_run::BenchState,
    /// `lss maintenance start|stop "reason"` (#22, 2026-09-21): written by the HTTP thread,
    /// read every poll tick to open/close the `KIND_MAINTENANCE` incident, feed
    /// `Observation::maintenance_active`, and auto-expire a forgotten window
    pub maintenance: lss_core::maintenance::MaintenanceState,
    /// #54, 2026-09-21: `generation_tokens_total`'s growth over the last `[bench] idle_secs` -
    /// what the bench idle gate judges quiet by (replaces the old REQUEST-count check: three
    /// tiny completed requests used to block a bench although the engine generated nothing).
    /// `None` = not enough comparable history yet (`History::counter_window`)
    pub recent_tokens_generated: Option<f64>,
    /// a gateway publishing /gate/health v5.2+: requests in flight of users other than the bench
    pub other_users_inflight: u64,
    /// the newest total GPU power reading, for the bench's energy window
    pub power_w: Option<f64>,
    pub slots: u32,
    pub loadout_id: Option<String>,
}

const USAGE: &str = "lss-collector [--config PATH] [--detect] [--check-config] [--test-alert MESSAGE] [--version]
       lss-collector setup [--help]   the setup wizard: find or type the engine, test it, write the configs
       lss-collector cost-setup [--zip ZIP | --from-ip | --rate USD_PER_KWH | --skip] ...   (see cost-setup --help)
  default config: ~/.config/lss/collector.toml (optional; built-in defaults otherwise)
  --detect               find the LLM server(s) on this machine - SGLang, vLLM, llama.cpp, Ollama,
                         LM Studio, TGI, anything OpenAI-compatible - and print what was found, why,
                         and the [[engine]] block that pins it. Changes nothing. Exit 1 = none found.
  --test-alert MESSAGE   ask the RUNNING collector (POST /test-alert on its 127.0.0.1 listener) to
                         raise one synthetic info alert through the real pipeline: rule engine ->
                         alerts row -> alert_cmd. Prints the row once it exists. Run it on the GPU box.";

/// kv keys for the Xid scan: how far the hot path has read, and a back-fill that is still owed.
const KV_XID_SCAN_UNTIL: &str = "xid_scan_until";
const KV_XID_BACKFILL_FROM: &str = "xid_backfill_from";
/// the first sample this database ever held: uptime % is only claimed from here on
const KV_FIRST_SAMPLE: &str = "first_sample_ts";
/// set once the 1m/10m tiers have been built from the samples an older collector stored
/// v2: the token-count series (`tok_gen` …) joined the tiers; the stored samples are folded in once more
const KV_ROLLUP_BACKFILL: &str = "rollup_backfilled_v2";
const KV_GPU_HEALTH: &str = "gpu_health";
/// the persistent token ledger (`lss_core::tokens::AllTime`): totals that survive serve restarts
const KV_TOKENS_ALL_TIME: &str = "tokens_all_time";

fn main() {
    // card #298: `lss-collector cost-setup ...` - the cost wizard, its own argument set
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("cost-setup") {
        std::process::exit(cost_setup_run::main(&argv[1..]));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut config_path = format!("{home}/.config/lss/collector.toml");
    let mut explicit = false;
    let mut check_only = false;
    let mut detect_only = false;
    let mut test_alert: Option<String> = None;
    // card #297: `lss-collector setup ...` is the setup wizard - its own arguments, its own exit
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("setup") {
        std::process::exit(setup_run::main(&argv[1..]));
    }
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => {
                config_path = args.next().unwrap_or_else(|| die("--config needs a path"));
                explicit = true;
            }
            "--check-config" => check_only = true,
            "--detect" => detect_only = true,
            "--test-alert" => test_alert = Some(args.next().unwrap_or_else(|| die("--test-alert needs a message"))),
            "--version" | "-V" => {
                println!("lss-collector {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other => die(&format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let cfg = match std::fs::read_to_string(&config_path) {
        Ok(text) => parse_config(&text).unwrap_or_else(|e| die(&format!("{config_path}: {e}"))),
        Err(_) if !explicit => Config::default(),
        Err(e) => die(&format!("{config_path}: {e}")),
    };
    lss_core::units::set_temp_units(lss_core::units::TempUnits::parse(&cfg.temp_units));
    // card #297: every request to the pinned engine carries its API key (and nothing else does)
    if let Some((_, url)) = cfg.pinned_engine() {
        collect::set_engine_auth(&url, &cfg.pinned_api_key());
    }
    if check_only {
        println!("{}", toml_dump(&cfg));
        return;
    }
    if detect_only {
        let (text, code) = detect::report(&cfg);
        print!("{text}");
        std::process::exit(code);
    }
    if let Some(msg) = test_alert {
        std::process::exit(send_test_alert(&cfg, &msg));
    }
    let mut cfg = cfg;
    // engine_url = "auto": find the LLM server on this machine (looks until one answers)
    detect::resolve(&mut cfg);
    // card #308: https engine / gateway URLs - one TLS config for every agent, warned about when off
    tls::configure(&cfg, &home);
    run(cfg, &home);
}

/// Client side of `--test-alert`: talks to the running collector, then waits for the row.
fn send_test_alert(cfg: &Config, msg: &str) -> i32 {
    let Some(addr) = cfg.listen.iter().find(|a| a.parse::<std::net::SocketAddr>().is_ok_and(|s| s.ip().is_loopback())) else {
        eprintln!("lss-collector: no 127.0.0.1 address in `listen`: the test-alert endpoint is loopback-only");
        return 2;
    };
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
    let sent_at = unix_now();
    let want = match agent.post(&format!("http://{addr}/test-alert")).send_string(msg) {
        Ok(r) => {
            let body = r.into_string().unwrap_or_default();
            print!("collector accepted it: {body}");
            serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["message"].as_str().map(String::from)).unwrap_or_default()
        }
        Err(ureq::Error::Status(code, r)) => {
            eprintln!("lss-collector: collector refused the test alert: HTTP {code} {}", r.into_string().unwrap_or_default().trim());
            return 1;
        }
        Err(e) => {
            eprintln!("lss-collector: cannot reach the running collector on {addr}: {e}");
            return 2;
        }
    };
    // the poll loop picks it up on its next turn; then the row is in the DB and in /status
    for _ in 0..(cfg.poll_secs.max(1) * 4 + 10) {
        std::thread::sleep(Duration::from_secs(1));
        let Ok(body) = collect::http_get(&agent, &format!("http://{addr}/status")) else { continue };
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let row = v["alerts"].as_array().and_then(|rows| rows.iter().find(|r| r["rule"] == lss_core::rules::RULE_TEST_ALERT && r["message"] == want.as_str() && r["ts"].as_i64().is_some_and(|t| t >= sent_at - 1)));
        if let Some(row) = row {
            println!("alerts row: {row}");
            println!("alert_cmd is running now; `delivered` turns true when a leg gets through (mail to a busy agent is spooled and flips it later). See: lss alerts");
            return 0;
        }
    }
    eprintln!("lss-collector: accepted, but no alerts row showed up in /status - check `journalctl --user -u lss-collector -n 30`");
    1
}

fn die(msg: &str) -> ! {
    eprintln!("lss-collector: {msg}");
    std::process::exit(2);
}

fn toml_dump(cfg: &Config) -> String {
    serde_json::to_string_pretty(cfg).unwrap_or_default()
}

fn run(mut cfg: Config, home: &str) {
    let started_at = unix_now();
    if cfg.host.trim().is_empty() {
        cfg.host = collect::host_name();
    }
    // #36, 2026-09-21: omp's shared config, watched (never written) for a stale default model
    let omp_config_path = (!cfg.rules.omp_config_path.trim().is_empty()).then(|| expand_home(&cfg.rules.omp_config_path, home));
    // #75, 2026-09-22: the OWNER's real electricity rate table - loaded once at startup, same as
    // every other config; a rates.toml edited later needs a restart, same as collector.toml.
    // Missing/unset/unparsable is never fatal and never an error: cost tracking is simply off
    // (the same "unset = nothing shown" convention `electricity_usd_per_kwh` already uses).
    let rates_table: Option<lss_core::rates::RateTable> = (!cfg.rates.path.trim().is_empty())
        .then(|| expand_home(&cfg.rates.path, home))
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
        .and_then(|(p, t)| match lss_core::rates::parse_rates_file(&t) {
            Ok(table) => {
                eprintln!("rates: {p}: {} ({})", table.name, if table.is_flat() { "flat" } else { "time-of-use" });
                Some(table)
            }
            Err(e) => {
                eprintln!("rates: {p}: {e} - cost tracking is off until this is fixed");
                None
            }
        });
    // #74, 2026-09-22: read back whatever a watch-sweep wrote - this collector never fetches
    // anything itself. Re-checked every poll (mtime-cheap), never the fetch loop's concern.
    let mut watcher = watch_run::Watcher::new(if cfg.watch.path.trim().is_empty() { String::new() } else { expand_home(&cfg.watch.path, home) });
    let db_path = expand_home(&cfg.db_path, home);
    if let Some(dir) = std::path::Path::new(&db_path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let db = Db::open(&db_path).unwrap_or_else(|e| die(&format!("{db_path}: {e}")));
    eprintln!("lss-collector {} starting: db={db_path} poll={}s", env!("CARGO_PKG_VERSION"), cfg.poll_secs);

    let mut engine: Engine = load_state(&db, "engine");
    let mut tracker: IncidentTracker = load_state(&db, "tracker");
    let keep = Retention { raw: i64::from(cfg.raw_hours) * 3600, m1: i64::from(cfg.retention_days) * 86_400, m10: i64::from(cfg.rollup_10m_days) * 86_400 };
    let mut history = History::default();
    for s in db.samples_since(started_at - HISTORY_SECS).unwrap_or_default() {
        history.push(s);
    }
    // the rollup buckets that were open when the last run stopped are rebuilt from the stored
    // samples, so a restart costs the tiers nothing
    let mut pipeline = rollup::Pipeline::default();
    let open_from = started_at - started_at.rem_euclid(600);
    pipeline.replay(&db, history.iter().filter(|s| s.ts >= open_from).cloned());
    let first_sample_ts = db.first_sample_ts().ok().flatten().unwrap_or(started_at);
    let first_seen: i64 = match db.kv_get(KV_FIRST_SAMPLE).ok().flatten().and_then(|v| v.parse().ok()) {
        Some(t) => t,
        None => {
            let _ = db.kv_set(KV_FIRST_SAMPLE, &first_sample_ts.to_string());
            first_sample_ts
        }
    };
    // startup sanity check: a stored C1 reading far above the others is a buffered answer
    match db.repair_burst_probes() {
        Ok(bad) => {
            for (ts, v) in bad {
                eprintln!("repair: probe at {ts} stored {v:.0} tok/s - an answer that arrived in one burst, not a speed: marked invalid and taken out of the loadout figures");
            }
        }
        Err(e) => eprintln!("repair: could not check the stored probes: {e}"),
    }
    // one-time correction (card #45): the GPU cold-clock check shipped, then was reverted the
    // same day for invalidating this GPU's routine idle state, not a rare event - undo what it
    // wrongly flagged before it was removed, before the (correct) repair below runs
    match db.undo_cold_clock_false_invalidations() {
        Ok(restored) => {
            for ts in restored {
                eprintln!("repair: probe at {ts} was wrongly invalidated by the reverted cold-clock check - restored to valid");
            }
        }
        Err(e) => eprintln!("repair: could not undo the cold-clock check's false invalidations: {e}"),
    }
    // startup sanity check (card #45): a probe the old idle gate missed - `running` never saw a
    // request that started and finished between two 5 s poll scrapes, but the stored samples'
    // token counter still proves it happened
    match db.repair_probes_the_gauge_missed(cfg.poll_secs as i64) {
        Ok(bad) => {
            for (ts, reason, detail) in bad {
                eprintln!("repair: probe at {ts} was stored VALID but the old idle gate missed it ({reason}): {detail}");
            }
        }
        Err(e) => eprintln!("repair: could not check the stored probes against samples: {e}"),
    }
    // startup sanity check: a token bucket no hardware could have served is an artefact (a
    // counter's whole life booked into one step when it was first recorded): repaired and said
    match db.repair_impossible_token_buckets() {
        Ok(bad) => {
            for (res, ts, sum, spread) in bad {
                eprintln!("repair: an impossible tok_cached bucket (tier {res} s, ts {ts}, {sum:.0} tokens = a counter's whole life booked into one step) was spread back over the earlier buckets it belongs to ({spread:.0} tokens placed)");
            }
        }
        Err(e) => eprintln!("repair: could not check the token history: {e}"),
    }
    // startup sanity check (card #88): a stored prefill peak from before #44 D2's wall-clock fix
    // is a persisted running max that never comes down on its own - cleared, never guessed at.
    match db.repair_impossible_read_peaks() {
        Ok(cleared) => {
            for (id, v) in cleared {
                eprintln!("repair: loadout {id} stored an impossible prefill peak ({v:.0} tok/s, from before #44's wall-clock fix) - cleared, will re-accumulate under the current formula");
            }
        }
        Err(e) => eprintln!("repair: could not check the stored loadouts' prefill peaks: {e}"),
    }
    let owes_backfill = db.kv_get(KV_ROLLUP_BACKFILL).ok().flatten().is_none();
    let mut loadouts = loadouts::Loadouts::load(&db);
    // #105, 2026-09-22 (verifier): once a real [rates] table is configured it is authoritative
    // for cost (an effective date, what it excludes - this old card-#6 path has neither) - see
    // `loadouts::effective_usd_per_kwh`'s own doc comment.
    loadouts.usd_per_kwh = loadouts::effective_usd_per_kwh(cfg.electricity_usd_per_kwh, rates_table.is_some());
    loadouts.min_tok_s_per_user = cfg.targets.min_tok_s_per_user;
    let mut targets_now = lss_core::targets::TargetsStatus::default();
    // the gate log of the last 24 h, merged once a minute: each user's own experience
    let mut user_log_24h = lss_core::gatelog::LogDelta::default();
    let mut user_log_at = 0_i64;
    let mut tokens_at = 0_i64;
    let mut all_time: lss_core::tokens::AllTime = load_state(&db, KV_TOKENS_ALL_TIME);
    if all_time.since == 0 {
        // a new ledger does not start from nothing: what the tiers already hold (up to 90 days)
        // is where "all time" begins, and the fastest minute on record is the first peak
        let total = |m: &str| db.rollup_sum(600, m, 0, started_at + 1).unwrap_or(0.0);
        all_time.since = first_sample_ts;
        (all_time.generated.total, all_time.prompt.total, all_time.cached.total, all_time.requests.total) = (total("tok_gen"), total("tok_prompt"), total("tok_cached"), total("sum_requests"));
        let _ = db.rollup_rows(600, "decode_tok_s", 0, started_at + 1, |ts, a| {
            if a.max > all_time.peak.tok_s {
                all_time.peak = lss_core::tokens::Peak { tok_s: a.max, ts };
            }
        });
        eprintln!("tokens: all-time ledger started from the stored history: {:.0} generated, {:.0} prompt since {first_sample_ts}", all_time.generated.total, all_time.prompt.total);
    }
    let mut all_time_saved_at = 0_i64;
    let mut advice_at = 0_i64;
    let mut advice_top: Vec<lss_core::advice::Finding> = Vec::new();
    let mut own_probes = (0_i64, 0_u64, 0_u64);
    let mut bench_was_active = false;
    let mut bench_generation = 0_u64;
    let mut maintenance_was_active = false;
    // address -> host name, refreshed every 5 minutes on its own thread when switched on
    let tailscale: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
    if cfg.tailscale_lookup {
        let names = tailscale.clone();
        std::thread::spawn(move || loop {
            let fresh = collect::tailscale_names();
            if !fresh.is_empty() {
                *names.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            }
            std::thread::sleep(Duration::from_secs(300));
        });
    }
    let mut gpu_health: Vec<lss_core::gpu::GpuHealth> = load_state(&db, KV_GPU_HEALTH);
    let last_probe = db.last_probe_attempt().ok().flatten();
    // Rows and engine state from before probe validation existed: judge the old rows by their
    // TTFT, then rebuild the baseline (and drop any streak) from valid probes only. Once.
    match db.invalidate_legacy_probes(cfg.rules.c1_max_ttft_s * 1000.0) {
        Ok(n) if n > 0 => eprintln!("probes: {n} stored probe(s) marked invalid (slow TTFT / skipped / failed)"),
        Ok(_) => {}
        Err(e) => eprintln!("probes: legacy validation failed: {e}"),
    }
    if engine.c1_needs_revalidation() {
        let before = engine.c1_baseline(&cfg.rules);
        let valid = db.recent_valid_tok_s(cfg.rules.c1_baseline_probes.max(1)).unwrap_or_default();
        engine.relearn_c1_baseline(&cfg.rules, &valid);
        eprintln!("c1: baseline re-learned from the last {} valid probe(s): {:?} -> {:?}", valid.len(), before, engine.c1_baseline(&cfg.rules));
    }
    // card #261: the startup repairs above can re-judge a probe the rule engine already counted
    // toward its low-decode streak; a streak is only as good as the probes it names
    let dropped = engine.retain_c1_streak(|ts| db.probe_is_valid(ts).unwrap_or(false));
    if !dropped.is_empty() {
        eprintln!("c1: {} probe(s) dropped from the low-decode streak - no longer valid readings: {dropped:?}", dropped.len());
    }
    let mut last_valid_probe = db.last_valid_probe().ok().flatten();

    let mut poller = Poller::new(cfg.clone(), started_at);
    // Where the Xid back-fill starts: a back-fill that never finished, else the point the hot
    // path had reached, else (first start ever) 7 days back. The 7-day read therefore happens
    // ONCE; a restart only re-reads the time the collector was not running.
    let kv_ts = |k: &str| db.kv_get(k).ok().flatten().and_then(|v| v.parse::<i64>().ok());
    let backfill_from = kv_ts(KV_XID_BACKFILL_FROM).or_else(|| kv_ts(KV_XID_SCAN_UNTIL)).unwrap_or(0).max(started_at - 7 * 86_400);
    let _ = db.kv_set(KV_XID_BACKFILL_FROM, &backfill_from.to_string());
    let state_dir = std::path::Path::new(&db_path).parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| ".".into());

    let seed_ttft = ttft_baseline(&db);
    let db = Arc::new(Mutex::new(db));
    let shared = Arc::new(Mutex::new(Shared { c1_ttft_baseline_ms: seed_ttft, ..Shared::default() }));
    let bench_ctx = Arc::new(bench_run::BenchCtx { cfg: cfg.clone(), home: home.to_string(), db: db.clone(), shared: shared.clone(), state_dir: state_dir.clone(), poll: Duration::from_secs(cfg.bench.poll_secs.max(1)) });
    bench_run::recover(&bench_ctx, started_at);
    // card #109: read from `db` FIRST, then take `shared` - never one inside the other. This is
    // the ONE rule for these two mutexes (see LOCK ORDER above): hold at most one at a time.
    // It used to take `shared` and then lock `db` inside that guard, while
    // bench_run_tests.rs held `db` and then took `shared` - the opposite order, which is the
    // shape that deadlocks the moment both run at once. No nesting means no order to get wrong.
    let last_bench_run = db
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .bench_runs(20)
        .unwrap_or_default()
        .iter()
        .find(|r| r.status != "running" && r.profile != "dry-run")
        .map(lss_core::bench::LastRun::of);
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.bench.brief.state = "idle".into();
        // with no external harness the built-in mini bench runs: `lss bench` always works
        s.bench.brief.configured = true;
        s.bench.brief.builtin = cfg.bench.harness.trim().is_empty();
        s.bench.brief.last = last_bench_run;
    }

    for addr in cfg.listen.clone() {
        let shared = shared.clone();
        let source = http::HistorySource { db_path: db_path.clone(), keep, probe_ip: query::probe_ip_of(&cfg.gate_url) };
        let bench = bench_ctx.clone();
        std::thread::spawn(move || http::serve_forever(addr, shared, source, bench));
    }
    if owes_backfill {
        // a database from before the rollup tiers: build them once from the stored samples, on
        // its own connection and thread so the poll loop never waits for it
        let path = db_path.clone();
        std::thread::spawn(move || match Db::open(&path).and_then(|db| rollup::backfill(&db, open_from).and_then(|n| db.kv_set(KV_ROLLUP_BACKFILL, &unix_now().to_string()).map(|()| n))) {
            Ok(n) => eprintln!("rollup backfill: {n} stored sample(s) folded into the 1m/10m tiers"),
            Err(e) => eprintln!("rollup backfill failed: {e} - will be retried on the next start"),
        });
    }
    // card #85 item 2: the Xid backfill shells nvidia-smi + journalctl - pointless (and a
    // stream that never matches) on an AMD, Apple or CPU-only box. Gate it on the same
    // rule the hot-path scan uses; the KV marker is left uncleared so a later NVIDIA setup
    // still gets its backfill on the next start.
    if xid_scan_gate(find_gpu_source()).should_scan() {
        spawn_xid_backfill(db.clone(), backfill_from, None, true);
    }
    let (alert_tx, alert_rx) = mpsc::channel();
    {
        let (cmd, db, dir) = (expand_home(&cfg.alert_cmd, home), db.clone(), state_dir.clone());
        std::thread::spawn(move || alert::run_loop(cmd, dir, alert_rx, db));
    }
    if cfg.probe.enabled {
        let (cfg, shared) = (cfg.clone(), shared.clone());
        std::thread::spawn(move || probe_run::run_loop(cfg, shared, last_probe));
    }

    let mut slots = cfg.slots;
    let mut was_up = false;
    let mut saved_state = (String::new(), String::new());
    let mut last_prune = 0_i64;
    let mut last_flush = started_at;
    let mut last_scan_mark = 0_i64;
    // (gate container start, probes admitted since then): re-counted only when either changes
    let mut probe_share: Option<(i64, u64)> = None;
    let poll = Duration::from_secs(cfg.poll_secs.max(1));

    loop {
        let t0 = Instant::now();
        let now = unix_now();
        let r = poller.poll(now);
        let mut sample = r.sample;

        if sample.serve_up && (!was_up || slots == 0) && cfg.slots == 0 {
            slots = poller.fetch_slots().unwrap_or(slots);
        }
        was_up = sample.serve_up;
        sample.slots = slots;
        if slots > 0 {
            // known after all (the engine's identity, or `slots` in the config)
            sample.not_reported.retain(|k| k != "slots");
        }

        let mut containers = Vec::new();
        if let Some(c) = &sample.serve_ct {
            containers.push(("serve", c.clone()));
        }
        if let Some(c) = &sample.gate_ct {
            containers.push(("gate", c.clone()));
        }
        let ops = tracker.step(&IncidentInput {
            now,
            serve_up: sample.serve_up,
            serve_detail: &r.serve_detail,
            gate_up: sample.gate.is_some() || sample.gate_absent,
            gate_detail: &r.gate_detail,
            containers: &containers,
            xids: &r.xids,
        });

        let (probe_results, test_alerts, bench_active, bench_gen, bench_profile): (Vec<ProbeRecord>, Vec<String>, bool, u64, Option<String>) = {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            let idle_from = now - cfg.bench.idle_secs.max(60) - 60;
            // #54, 2026-09-21: kept for its own bookkeeping (a test checks this list's length) -
            // the idle gate no longer reads it, it judges quiet by the token counter instead
            s.bench.own_requests.retain(|t| *t >= idle_from);
            (std::mem::take(&mut s.probe_results), std::mem::take(&mut s.test_alerts), s.bench.active, s.bench.generation, s.bench.brief.profile.clone())
        };
        let (maintenance_active, maintenance_reason) = {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            // a forgotten `lss maintenance stop` must not mute restart alerts forever
            if lss_core::maintenance::expired(&s.maintenance, now) {
                eprintln!("maintenance: window auto-expired ({})", s.maintenance.reason);
                lss_core::maintenance::stop(&mut s.maintenance, now);
            }
            (s.maintenance.active, s.maintenance.reason.clone())
        };
        if let Some((from, until)) = r.journal_gap {
            eprintln!("kernel journal was unreadable for {}s: streaming the gap for Xids", until - from);
            spawn_xid_backfill(db.clone(), from, Some(until), false);
        }
        let mut restarts = Vec::new();
        let dbl = db.lock().unwrap_or_else(|e| e.into_inner());
        // the bench WINDOW: an incident that is open exactly while a benchmark loads the server,
        // so the charts are annotated and nobody mistakes the load for users
        if bench_active != bench_was_active {
            let res = if bench_active {
                eprintln!("incident OPEN bench: lss bench {}", bench_profile.clone().unwrap_or_default());
                dbl.open_incident(lss_core::incidents::KIND_BENCH, now, &format!("lss bench {} running: the load is the benchmark's; queue and C1 alerts stand down", bench_profile.clone().unwrap_or_default()))
            } else {
                eprintln!("incident CLOSE bench");
                dbl.close_incident(lss_core::incidents::KIND_BENCH, now).map(|_| ())
            };
            if let Err(e) = res {
                eprintln!("db: bench incident write failed: {e}");
            }
            bench_was_active = bench_active;
        }
        // the maintenance WINDOW (#22, 2026-09-21): open exactly while `lss maintenance start`
        // is in effect, so the timeline/charts show it and `eval_restarts` can label the
        // restart it was opened for `info`/"planned" instead of `warn`
        if maintenance_active != maintenance_was_active {
            let res = if maintenance_active {
                eprintln!("incident OPEN maintenance: {maintenance_reason}");
                dbl.open_incident(lss_core::incidents::KIND_MAINTENANCE, now, &format!("planned maintenance: {maintenance_reason}"))
            } else {
                eprintln!("incident CLOSE maintenance");
                dbl.close_incident(lss_core::incidents::KIND_MAINTENANCE, now).map(|_| ())
            };
            if let Err(e) = res {
                eprintln!("db: maintenance incident write failed: {e}");
            }
            maintenance_was_active = maintenance_active;
        }
        if bench_gen != bench_generation {
            bench_generation = bench_gen;
            loadouts.reload_bench(&dbl);
        }
        for op in ops {
            let res = match op {
                IncidentOp::Open { kind, start, detail } => {
                    eprintln!("incident OPEN {kind}: {detail}");
                    dbl.open_incident(kind, start, &detail)
                }
                IncidentOp::Close { kind, end, duration_secs } => {
                    eprintln!("incident CLOSE {kind} after {duration_secs}s");
                    dbl.close_incident(kind, end).map(|_| ())
                }
                IncidentOp::Event { kind, ts, subject, detail, key } => {
                    eprintln!("incident {kind}: {detail}");
                    if kind == KIND_CONTAINER_RESTART {
                        restarts.push(RestartObs { role: subject, detail: detail.clone() });
                    }
                    dbl.event_incident(kind, ts, &detail, key.as_deref()).map(|_| ())
                }
            };
            if let Err(e) = res {
                eprintln!("db: incident write failed: {e}");
            }
        }
        for p in &probe_results {
            if let Err(e) = dbl.insert_probe(p) {
                eprintln!("db: probe write failed: {e}");
            }
            if p.is_reading() {
                let t = ttft_baseline(&dbl);
                shared.lock().unwrap_or_else(|e| e.into_inner()).c1_ttft_baseline_ms = t;
            }
        }

        // #36, 2026-09-21: omp's shared config on THIS box, watched (never written) against
        // what the poll just saw served - a small local file, cheap to read every tick
        let omp_default = omp_config_path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| lss_core::omp::parse_default_model(&t));
        let omp_mismatch = lss_core::omp::mismatch(omp_default.as_deref(), sample.model.as_deref());

        let obs = Observation {
            now,
            serve_up: sample.serve_up,
            gate_up: sample.gate.is_some() || sample.gate_absent,
            queue: sample.metrics.as_ref().map(|m| m.queue),
            trusted_waiters: sample.gate.as_ref().map(|g| g.trusted.waiters),
            // card #78: the composite "big budget with no cache" rule reads the trusted
            // lane's in-flight tokens and the engine's cache hit rate.
            trusted_inflight_tokens: sample.gate.as_ref().map(|g| g.trusted.inflight_tokens),
            // card #135: the LIVE capacity, so the rule's threshold moves with the config
            trusted_budget_tokens: sample.gate.as_ref().and_then(|g| g.trusted.budget_tokens),
            cache_hit_rate: sample.metrics.as_ref().map(|m| m.cache_hit_rate),
            rejected_413: sample.gate.as_ref().map(|g| g.rejected_413()),
            rejected_429: sample.gate.as_ref().map(|g| g.rejected_429()),
            gpus: sample.gpus_ok.then(|| sample.gpus.iter().map(|g| GpuObs { index: g.index, temp_c: g.temp_c, throttle_mask: g.throttle_mask }).collect()),
            xids: r.xids,
            restarts,
            running_roles: containers.iter().filter(|(_, c)| c.running()).map(|(role, _)| (*role).to_string()).collect(),
            probe_tok_s: lss_core::probe::c1_reading(&probe_results),
            probe_ts: probe_results.iter().rev().find(|p| p.is_reading()).map(|p| p.ts),
            test_alerts,
            bench_active,
            probe_interval_secs: cfg.probe.interval_secs,
            maintenance_active,
            omp_mismatch,
            // #108: the gate's own charging health, as it reports it. `None` when
            // /gate/health was unreadable or the gate predates the block - the rule holds its
            // clocks rather than alerting on a missing reading (gate_down owns that).
            gate_shadow: sample.gate.as_ref().and_then(|g| g.shadow.clone()),
            // card #264: this box's own disk (statvfs, cheap) - its / filled to 100% once unnoticed
            disk: disk_reading(&cfg.rules.disk_path),
        };
        for a in engine.evaluate(&cfg.rules, &obs) {
            match dbl.insert_alert(&a) {
                Ok(id) => {
                    let _ = alert_tx.send(alert::Job::Alert { id, severity: a.severity.as_str().to_string(), message: a.message.clone() });
                }
                Err(e) => eprintln!("db: alert write failed: {e} - {}", a.message),
            }
        }
        if cfg.alert_flush_secs > 0 && now - last_flush >= cfg.alert_flush_secs as i64 {
            last_flush = now;
            let _ = alert_tx.send(alert::Job::Flush);
        }
        if r.journal_ok && now - last_scan_mark >= 60 {
            last_scan_mark = now;
            let _ = dbl.kv_set(KV_XID_SCAN_UNTIL, &now.to_string());
        }
        if let Some(p) = probe_results.iter().rev().find(|p| p.is_reading()) {
            last_valid_probe = Some(p.clone());
        }
        let gate_started = sample.gate_ct.as_ref().map(|c| c.started_at).filter(|t| *t > 0);
        if !probe_results.is_empty() || probe_share.map(|(t, _)| t) != gate_started {
            probe_share = gate_started.map(|t| (t, dbl.probes_admitted_since(t).unwrap_or(0)));
        }

        if let Err(e) = dbl.insert_sample(&sample) {
            eprintln!("db: sample write failed: {e}");
        }
        loadouts.on_sample(&dbl, &sample, |name| match name {
            // a serve container: its image and launch arguments ARE the loadout
            n if !n.is_empty() => poller.loadout_inspect(n).map(|i| lss_core::loadout::identity(sample.model.as_deref().unwrap_or_default(), &i.image, &i.args, &i.env)),
            // no container (a plain process, or no docker at all): the engine describes itself
            _ => poller.engine_identity().map(|(kind, e)| lss_core::loadout::identity_from_engine(kind.name(), &e)),
        });
        if let Some(m) = &sample.metrics {
            let epoch = sample.serve_ct.as_ref().map_or(0, |c| c.started_at);
            all_time.observe(now, &lss_core::timeutil::fmt_local(now, "%Y-%m-%d"), epoch, m);
            if now - all_time_saved_at >= 60 {
                all_time_saved_at = now;
                let _ = dbl.kv_set(KV_TOKENS_ALL_TIME, &serde_json::to_string(&all_time).unwrap_or_default());
            }
        }
        if now - own_probes.0 >= 60 {
            // The gateway's per-user counters start again when the gateway restarts, the probe
            // history does not: count only the probes sent since the gateway has known this
            // address. Otherwise EVERY request from the probe's address is taken for a probe and
            // somebody else's traffic (and rejections) on that address disappears from USERS.
            let gate_knows_since = sample.gate.as_ref().and_then(|g| g.users.as_ref()).and_then(|users| users.iter().find(|u| u.lane == "trusted" && Some(u.user.as_str()) == query::probe_user_of(&cfg.gate_url).as_deref()).map(|u| u.first_seen as i64)).unwrap_or(0);
            own_probes = (now, dbl.probes_logged_since((now - 3600).max(gate_knows_since)).unwrap_or(0), dbl.probes_logged_since((now - 86_400).max(gate_knows_since)).unwrap_or(0));
        }
        for p in &probe_results {
            loadouts.on_probe(p);
        }
        for w in pipeline.push(&dbl, &sample, r.hists.as_ref(), true) {
            loadouts.on_window(&w);
        }
        if let Some(h) = r.health {
            let facts = |v: &[lss_core::gpu::GpuHealth]| v.iter().cloned().map(|mut g| { g.ts = 0; g }).collect::<Vec<_>>();
            if facts(&h) != facts(&gpu_health) {
                // written only when something moved (an ECC count, a link width): it is a fact sheet, not a series
                let _ = dbl.kv_set(KV_GPU_HEALTH, &serde_json::to_string(&h).unwrap_or_default());
            }
            gpu_health = h;
        }
        let power_w = { let v: Vec<f64> = sample.gpus.iter().filter_map(|g| g.power_w).collect(); (!v.is_empty()).then(|| v.iter().sum::<f64>()) };
        let gpu_indices: Vec<u32> = sample.gpus.iter().map(|g| g.index).collect();
        history.push(sample);
        // #54, 2026-09-21: what the bench's idle gate asks - has REAL traffic generated tokens
        // in the last N minutes? Request/status counts miss what matters (three tiny COMPLETED
        // requests used to block a bench although the engine had generated nothing for minutes);
        // the SAME counter evidence the probe's own validity check uses (card #45) is the honest
        // signal. The probe's own periodic reading is real growth on this same counter (by
        // default it runs about as often as `idle_secs` itself, so it would otherwise almost
        // always look "busy") - its own known token count is subtracted out, with the same
        // margin the probe's own counter check allows for scrape-timing jitter.
        let idle_from = now - cfg.bench.idle_secs;
        let raw_growth = history.counter_window(now, cfg.bench.idle_secs, |m| m.generation_tokens_total);
        let probe_tokens: f64 = dbl.probes_between(idle_from, now + 1).unwrap_or_default().iter().filter_map(|p| p.tokens).map(f64::from).sum();
        let recent_tokens_generated = lss_core::bench::real_tokens_generated(raw_growth, probe_tokens);

        let state = (serde_json::to_string(&engine).unwrap_or_default(), serde_json::to_string(&tracker).unwrap_or_default());
        if state != saved_state {
            let _ = dbl.kv_set("engine", &state.0);
            let _ = dbl.kv_set("tracker", &state.1);
            saved_state = state;
        }
        if now - last_prune >= 3600 {
            last_prune = now;
            match dbl.prune(now, keep) {
                Ok(n) if n > 0 => eprintln!("retention: pruned {n} rows; database {:.1} MB", dbl.size_bytes().unwrap_or(0) as f64 / 1e6),
                Ok(_) => {}
                Err(e) => eprintln!("retention: {e}"),
            }
        }

        let all_incidents = dbl.incidents_since(now - keep.m10).unwrap_or_default();
        let incidents: Vec<_> = all_incidents.iter().filter(|i| i.start >= now - 7 * 86_400 || i.end.is_none_or(|e| e >= now - 7 * 86_400)).cloned().collect();
        let all_alerts = dbl.recent_alerts(100).unwrap_or_default();
        let alerts: Vec<_> = all_alerts.iter().take(20).cloned().collect();
        let probes = dbl.recent_probes(50).unwrap_or_default();
        let restarts_today = dbl.count_restarts_since(local_midnight(now), &cfg.gate_container).unwrap_or(0);
        let ts_names = tailscale.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let probe_user = query::probe_user_of(&cfg.gate_url);
        let own_traffic = lss_core::users::OwnTraffic { probe_user: probe_user.as_deref(), probes_1h: own_probes.1, probes_24h: own_probes.2 };
        let users_now = lss_core::users::build_users(history.iter().last().and_then(|s| s.gate.as_ref()), &cfg.user_alias, &ts_names, &own_traffic, now);
        // card #23: does the CURRENT engine publish token counters at all? (ServeMetrics has
        // no "tokens" key of its own - the token LEDGER is built from the collector's rollups,
        // which are zero for engines like Ollama that publish no generation_tokens_total.)
        let tokens_not_reported = history.iter().last()
            .and_then(|s| s.metrics.as_ref())
            .is_none_or(|m| m.generation_tokens_total <= 0.0);
        let tokens_json = (now - tokens_at >= 60).then(|| {
            tokens_at = now;
            serde_json::to_string(&query::tokens_doc(&dbl, now, local_midnight(now), first_seen, &all_time, &users_now, query::TokensDocFlags { gate_absent: !cfg.has_gate(), tokens_not_reported })).unwrap_or_else(|_| "{}".into())
        });
        let advice_json = (now - advice_at >= 300).then(|| {
            advice_at = now;
            let doc = advice_run::compute(&dbl, now, &cfg, slots, first_seen, loadouts.current_card().as_ref(), &gpu_indices);
            advice_top = doc.top.clone();
            targets_now = doc.windows.iter().find(|w| w.stats.name == "24h").and_then(|w| w.stats.targets.clone()).unwrap_or_default();
            serde_json::to_string(&doc).unwrap_or_else(|_| "{}".into())
        });
        if now - user_log_at >= 60 {
            user_log_at = now;
            user_log_24h = dbl.gatelog_merged(now - 86_400, now + 1).unwrap_or_default();
            if let Some((_, open)) = pipeline.gate_open_bucket() {
                user_log_24h.merge(open);
            }
        }
        let bench_runs = dbl.bench_runs(30).unwrap_or_default();
        // #75, 2026-09-22: electricity cost from the GPU watts already sampled every poll,
        // priced with the owner's own time-of-use table. None when no rates.toml is configured.
        // Computed here, still inside the `dbl` lock it needs to read stored samples.
        let cost = rates_table.as_ref().map(|table| {
            let midnight = local_midnight(now);
            let today_generated = history.counter_window(now, now - midnight, |m| m.generation_tokens_total);
            let today_prompt = history.counter_window(now, now - midnight, |m| m.prompt_tokens_total);
            // #174: uncached prefill = prompt minus cached, so the real-work basis prices only
            // what the engine actually computed, never a cache hit's near-zero real cost.
            let today_cached = history.counter_window(now, now - midnight, |m| m.cached_tokens_total);
            cost_run::compute(&dbl, table, now, midnight, power_w, cost_run::TodayTokens { generated: today_generated, prompt: today_prompt, cached: today_cached })
        });
        // card #176: the spend-over-time block, from the 10-minute rollup (item 4: never
        // re-derive a month from 5-second samples). Same rate table, same pricing function as
        // the "today" figure above, so the two can never disagree about a day they share.
        let spending = rates_table.as_ref().map(|table| cost_run::spending(&dbl, table, now));
        // #73, 2026-09-22: TOKENS section's hour/day/week/month windows, all from the collector's
        // own rollup tables (#100, 2026-09-22: `hour` used to come from `work_1h`'s raw samples,
        // a different source than day/week/month - that let it read higher than `day` on a
        // low-uptime collector. Same source, same method, for all four now - see tokens_run.rs's
        // own doc comment). Same `dbl` lock as `cost` above.
        let tokens_windows = tokens_run::compute(&dbl, now);
        drop(dbl);
        let watch = watcher.poll();

        let baseline = engine.c1_baseline(&cfg.rules);
        let baseline_source = match (cfg.rules.c1_baseline_tok_s, baseline) {
            (Some(_), _) => "config".to_string(),
            (None, Some(_)) => "learned".to_string(),
            (None, None) => format!("learning {}/{}", engine.c1_baseline_progress(), cfg.rules.c1_baseline_probes),
        };
        // card #79: ONE lock, hoisted OUT of the build_status(...) expression. `std::sync::Mutex`
        // is NOT reentrant, and a MutexGuard temporary created inside a call expression lives
        // until the END of the whole statement - so the two `shared.lock()` calls that used to
        // sit in this argument list (`bench:` and `maintenance:`) self-deadlocked on the FIRST
        // poll. The poll loop then held the mutex forever, so the HTTP thread blocked on it too
        // and /health, /status, /tokens and /metrics never answered at all (card #79's CASE A:
        // http=000 after 20 s). Take both fields under one guard, before the statement.
        let (bench_brief, maintenance_now) = {
            let g = shared.lock().unwrap_or_else(|e| e.into_inner());
            (g.bench.brief.clone(), g.maintenance.clone())
        };
        // card #284: the whole /status assembly in one tested place - lss-core's document, then
        // the series only the collector can fill ($/h priced on each bucket's MEAN power, #283).
        // card #79's rule: the db guard is a NAMED local taken before the statement and dropped
        // right after it - never a temporary living through an argument list (no argument below
        // may take `db` or `shared`; the lock order stays shared -> db, #109).
        let status_db = db.lock().unwrap_or_else(|e| e.into_inner());
        let mut status = query::assemble_status(&status_db, &StatusInputs {
            now,
            host: &cfg.host,
            collector_version: env!("CARGO_PKG_VERSION"),
            collector_started_at: started_at,
            poll_secs: cfg.poll_secs,
            slots,
            history: &history,
            incidents: &incidents,
            alerts: &alerts,
            probes: &probes,
            last_valid_probe: last_valid_probe.as_ref(),
            firing: engine.firing(now),
            serve_down_since: tracker.serve_down_since(),
            gate_down_since: tracker.gate_down_since(),
            restarts_today,
            thermal_exclude: &cfg.rules.thermal_exclude,
            probe_enabled: cfg.probe.enabled,
            probe_interval_s: cfg.probe.interval_secs,
            c1_baseline: baseline,
            c1_baseline_source: baseline_source,
            thresholds: Thresholds::new(&cfg.rules, baseline, cfg.probe.interval_secs),
            probes_admitted_since_gate_start: probe_share.map_or(0, |(_, n)| n),
            latency: Some(pipeline.latency_now()),
            gpu_health: &gpu_health,
            user_aliases: &cfg.user_alias,
            tailscale: &ts_names,
            own_traffic,
            advice_top: advice_top.clone(),
            bench: bench_brief,
            maintenance: maintenance_now,
            loadout: loadouts.brief(),
            prefill_typical: loadouts.prefill_typical(),
            live_decode: loadouts.live_speed(history.latest().and_then(|s| s.metrics.as_ref()).map_or(0.0, |m| m.running)),
            public_priority: cfg.public_priority.clone(),
            trusted_priority: cfg.trusted_priority.clone(),
            targets: targets_now.clone(),
            user_log_24h: Some(&user_log_24h),
            cost,
            spending,
            tokens_hour: tokens_windows.hour,
            tokens_day: tokens_windows.day,
            tokens_week: tokens_windows.week,
            tokens_month: tokens_windows.month,
            tokens_hour_covered_secs: tokens_windows.hour_covered_secs,
            tokens_day_covered_secs: tokens_windows.day_covered_secs,
            tokens_week_covered_secs: tokens_windows.week_covered_secs,
            tokens_month_covered_secs: tokens_windows.month_covered_secs,
            watch,
        }, rates_table.as_ref());
        drop(status_db);
        // card #331: an engine that refuses our key says so, in words, on /status (and so in
        // `lss status` and the bench's refusal) - not just DOWN
        if !status.serve.up {
            status.serve.down_reason = lss_core::engine::auth_refusal_hint(&r.serve_detail, !cfg.engine_api_key.trim().is_empty());
        }
        let rules = lss_core::series::RulesDoc {
            v: lss_core::STATUS_SCHEMA_VERSION,
            generated_at: now,
            rules: engine.rule_states(&cfg.rules, now),
            firing: status.firing.clone(),
            spool_depth: spool_depth(&state_dir),
            alerts: all_alerts,
            incidents: all_incidents.into_iter().take(200).collect(),
            uptime: lss_core::series::uptime(now, first_seen, &status.incidents),
        };
        {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.metrics_text = lss_core::promout::render(&status);
            s.status_json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".into());
            s.c1_baseline = status.c1_baseline();
            s.rules_json = serde_json::to_string(&rules).unwrap_or_else(|_| "{}".into());
            s.gate_open = pipeline.gate_open_bucket().cloned();
            s.loadouts_json = serde_json::to_string(&loadouts.doc(now)).unwrap_or_else(|_| "{}".into());
            if let Some(t) = tokens_json {
                s.tokens_json = t;
            }
            if let Some(a) = advice_json {
                s.advice_json = a;
            }
            s.bench_json = serde_json::to_string(&lss_core::bench::BenchDoc { v: lss_core::STATUS_SCHEMA_VERSION, generated_at: now, brief: s.bench.brief.clone(), runs: bench_runs }).unwrap_or_else(|_| "{}".into());
            s.recent_tokens_generated = recent_tokens_generated;
            s.other_users_inflight = status.users.totals.inflight;
            s.power_w = power_w;
            s.slots = slots;
            s.loadout_id = loadouts.current_id().map(|i| i.id.clone());
            s.last_sample_ts = now;
            s.serve_up = status.serve.up;
            s.serve_down_reason = status.serve.down_reason.clone();
            s.model = status.serve.model.clone();
            s.running = status.serve.running;
            s.queue = status.serve.queue;
        }

        if let Some(rest) = poll.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Streams the kernel journal for Xids in `[from, until)` on its own thread and books them as
/// incidents (never alerts). `startup` = the one owed since the last run: its kv marker is
/// cleared only when the read completed, so a back-fill cut short is redone on the next start.
fn spawn_xid_backfill(db: Arc<Mutex<Db>>, from: i64, until: Option<i64>, startup: bool) {
    std::thread::spawn(move || {
        let events = match collect::backfill_xids(from, until) {
            Ok(events) => events,
            Err(e) => {
                eprintln!("xid backfill since {from}: {e} - will be retried on the next start");
                return;
            }
        };
        let mut scratch = IncidentTracker::default();
        let input = IncidentInput { now: unix_now(), serve_up: true, serve_detail: "", gate_up: true, gate_detail: "", containers: &[], xids: &events };
        let db = db.lock().unwrap_or_else(|e| e.into_inner());
        let mut booked = 0;
        for op in scratch.step(&input) {
            if let IncidentOp::Event { kind, ts, detail, key, .. } = op {
                booked += usize::from(db.event_incident(kind, ts, &detail, key.as_deref()).unwrap_or(false));
            }
        }
        if startup {
            let _ = db.kv_del(KV_XID_BACKFILL_FROM);
        }
        eprintln!("xid backfill: {} report(s) since {from}, {booked} new incident(s)", events.len());
    });
}

/// Agent mail waiting in the alert script's spool (one file per message).
fn spool_depth(state_dir: &std::path::Path) -> Option<u64> {
    let dir = state_dir.join("mail-spool");
    match std::fs::read_dir(&dir) {
        Ok(entries) => Some(entries.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_file())).count() as u64),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

fn load_state<T: serde::de::DeserializeOwned + Default>(db: &Db, key: &str) -> T {
    db.kv_get(key).ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

/// card #264: one statvfs reading of `path` ("" = not watched, None = unreadable). What the disk
/// rules judge; statvfs is a single syscall, so it is read every poll.
// statvfs field types are per-platform (u32 blocks on macOS, u64 on Linux): the `as u64` casts are
// needed on one and "unnecessary" on the other
#[allow(clippy::unnecessary_cast)]
fn disk_reading(path: &str) -> Option<lss_core::rules::DiskObs> {
    if path.trim().is_empty() {
        return None;
    }
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized, owned out-buffer
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frag = st.f_frsize as u64;
    Some(lss_core::rules::DiskObs { path: path.to_string(), total_bytes: st.f_blocks as u64 * frag, avail_bytes: st.f_bavail as u64 * frag })
}

#[cfg(test)]
mod disk_tests {
    #[test]
    fn the_disk_reading_is_this_boxs_real_filesystem_and_nothing_for_an_empty_or_missing_path() {
        // card #264: a real statvfs of / - total and free are real numbers, free never above total
        let d = super::disk_reading("/").expect("/ is always readable");
        assert!(d.total_bytes > 0 && d.avail_bytes <= d.total_bytes, "{d:?}");
        assert!((0.0..=100.0).contains(&d.used_pct()), "{d:?}");
        assert_eq!(super::disk_reading(""), None, "an empty path = not watched");
        assert_eq!(super::disk_reading("/no/such/path/264"), None, "unreadable = no reading (never a recovery)");
    }
}
