use lss::data::{self, PageData, PageId, DEFAULT_RANGE};
use lss::ui::{self, App};
use lss::{client, plain, prefs, report};
use lss_core::model::Status;
use lss_core::series::RANGES;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io::IsTerminal;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str = "lss - LLM SERVER STATUS: terminal monitor for a self-hosted LLM server
Usage: lss [COMMAND] [--range 1h] [--json] [--url URL]     setup: connect + test any LLM server
  (none)      live screen in a terminal; one plain-text status when piped
  status      one-shot status (--json = the collector's /status document)
  users       who is on the server: running now, limits, requests and tokens per user
  tokens      tokens served: today / 1h / 24h / 7d / all time, peaks, lengths, per user
  model       how good this model is on this hardware: speed by users at once, memory, energy
  loadouts    every model + settings ever served, with its benchmark headline numbers
  compare A B two loadouts side by side (A, B: current | previous | best | id | model name)
  bench P     benchmark: quick (~5 min) | full (~30 min) | accuracy | dry-run | status | cancel
  maintenance start \"reason\"|stop|status   planned window: its restart shows 'planned', not warn
  advice      plain-English evidence for settings and upgrade decisions
  latency     TTFT / e2e / inter-token / queue time: p50 p90 p99 + histogram buckets
  load        tok/s, running + queued per lane, KV, cache hit, spec accept, req/min, C1
  gpus        per-GPU temp, power, clock, util, memory, throttle + link / ECC / remap health
  gateway     per-lane and per-key requests and status codes, in-flight tokens, client IPs
  rules       every alert rule: state, history  (also: incidents|alerts|probe)
  events      incidents: what happened, dated - restarts, outages, Xid, uptime %
Options   (-h help · -V version)
  --range R   15m | 1h | 6h | 24h | 7d (default 1h); for advice 24h | 7d | 30d (default 7d)
  --json      machine-readable output (docs/STATUS-JSON.md, schema \"v\":1)
  --url URL   collector (else $LSS_URL, else ~/.config/lss/lss.toml, else http://127.0.0.1:8099)
  --server S  with [[server]] entries in lss.toml: which one (its name or number; default the first)
  --force --note --dataset --no-wait --minutes   bench flags; maintenance auto-expire (default 60m)
  --under-load  bench w/ other traffic, recorded    --accuracy  compare: paired accuracy test too
  --demo      the live screen on built-in sample data, no collector needed
  --no-save   never saves theme/layout/range/chart ($LSS_PREFS, else ~/.config/lss/ui.json)
Keys (live screen)   arrows move   enter open   1-9,a pages   r range   s sort   b bench   esc back
                     T theme   L layout   c chart   v page1   w watch   [ ] server   ? help   q quit
Exit codes   0 serve up   1 serve DOWN / refused   2 collector unreachable / bad usage";

/// What to print when the collector answered without echoing `under_load` back (card #14).
const UNDER_LOAD_IGNORED: &str = "lss: this collector is OLDER than --under-load and ignored it. It did not skip the idle gate, and it will abort the run at the first request from anyone else - which is exactly what --under-load exists to prevent. Install a matching lss-collector on the server and restart it, then run this again. (`lss` and `lss-collector` are two binaries: updating one does not update the other.)";

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `~/.config/lss/lss.toml`. A missing file is the defaults; a broken one is said, then ignored.
fn client_config() -> lss_core::config::ClientConfig {
    let Some(home) = std::env::var_os("HOME") else { return Default::default() };
    let path = std::path::Path::new(&home).join(".config/lss/lss.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => lss_core::config::parse_client_config(&text).unwrap_or_else(|e| {
            eprintln!("lss: ignoring {}: {e}", path.display());
            Default::default()
        }),
        Err(_) => Default::default(),
    }
}

#[derive(Default)]
struct Args {
    cmd: Option<String>,
    /// the words after the command: `compare A B`, `bench quick`
    words: Vec<String>,
    json: bool,
    demo: bool,
    no_save: bool,
    range: Option<String>,
    url: Option<String>,
    server: Option<String>,
    force: bool,
    under_load: bool,
    no_wait: bool,
    accuracy: bool,
    note: String,
    dataset: String,
    minutes: u32,
}

// #231: "events" is PageId::Incidents's own CLI word (`lss events`) - the top-level one-shot
// `lss incidents` report already owned "incidents", so the new detail page needed a different
// word (see data.rs's `PageId::command` doc comment).
const COMMANDS: [&str; 18] = ["status", "incidents", "alerts", "probe", "latency", "load", "gpus", "gateway", "rules", "events", "users", "tokens", "model", "loadouts", "compare", "bench", "advice", "maintenance"];

/// card #297: `lss setup ...` is `lss-collector setup ...` - the wizard lives with the collector
/// (it writes the collector's config and restarts it). The one next to this program first, so
/// a copy in ~/.local/bin never runs some other version's wizard; else whatever PATH finds.
fn setup(args: &[String]) -> ! {
    let sibling = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("lss-collector"))).filter(|p| p.is_file());
    let program = sibling.unwrap_or_else(|| std::path::PathBuf::from("lss-collector"));
    let err = std::os::unix::process::CommandExt::exec(std::process::Command::new(&program).arg("setup").args(args));
    eprintln!("lss setup: cannot run {}: {err}. It is installed next to lss by install.sh.", program.display());
    std::process::exit(2);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("setup") {
        setup(&argv[1..]);
    }
    let mut a = Args::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                println!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("lss {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--json" => a.json = true,
            "--demo" => a.demo = true,
            "--no-save" => a.no_save = true,
            "--force" => a.force = true,
            "--under-load" => a.under_load = true,
            "--no-wait" => a.no_wait = true,
            "--accuracy" => a.accuracy = true,
            "--url" => a.url = Some(args.next().unwrap_or_else(|| usage("--url needs a value"))),
            "--server" => a.server = Some(args.next().unwrap_or_else(|| usage("--server needs a name or a number"))),
            "--note" => a.note = args.next().unwrap_or_else(|| usage("--note needs a value")),
            "--dataset" => a.dataset = args.next().unwrap_or_else(|| usage("--dataset needs a value")),
            "--minutes" => {
                let v = args.next().unwrap_or_else(|| usage("--minutes needs a value"));
                a.minutes = v.parse().unwrap_or_else(|_| usage(&format!("--minutes '{v}' is not a whole number")));
            }
            "--range" => {
                let r = args.next().unwrap_or_else(|| usage("--range needs a value"));
                if lss_core::series::parse_range(&r).is_none() {
                    usage(&format!("--range '{r}' is not a duration: try 15m, 1h, 6h, 24h, 7d"));
                }
                a.range = Some(r);
            }
            word if a.cmd.is_none() && COMMANDS.contains(&word) => a.cmd = Some(arg),
            word if matches!(a.cmd.as_deref(), Some("compare" | "bench" | "maintenance")) && !word.starts_with('-') => a.words.push(arg),
            other => usage(&format!("unknown argument '{other}'")),
        }
    }
    let cfg = client_config();
    lss_core::units::set_temp_units(lss_core::units::TempUnits::parse(&cfg.temp_units));
    let env_url = std::env::var("LSS_URL").ok();
    let servers = lss_core::config::servers(a.url.as_deref(), env_url.as_deref(), &cfg);
    let server_idx = match a.server.as_deref() {
        Some(sel) => lss_core::config::pick_server(&servers, sel).unwrap_or_else(|| usage(&format!("--server '{sel}': not one of {}", servers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")))),
        None => 0,
    };
    let url = servers[server_idx].1.clone();
    // `lss bench` from here runs over ssh: a server's own `bench_ssh` wins over the top-level one
    let mut cfg = cfg;
    if let Some(own) = cfg.server.iter().find(|s| s.url.trim().trim_end_matches('/') == url && !s.bench_ssh.trim().is_empty()).map(|s| s.bench_ssh.clone()) {
        cfg.bench_ssh = own;
    }
    let code = match a.cmd.as_deref() {
        None if !a.json && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() => match run_tui(servers, server_idx, a.demo, a.no_save, &cfg) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("lss: terminal error: {e}");
                2
            }
        },
        None => one_shot("status", a.json, &url),
        Some("users") => users_shot(a.json, &url),
        Some("loadouts") => doc_shot(a.json, &url, "/loadouts", |body| Ok(report::loadouts(&parse_doc(body)?, now()))),
        Some("advice") => {
            let window = a.range.clone().unwrap_or_else(|| "7d".into());
            doc_shot(a.json, &url, "/advice", move |body| Ok(report::advice(&parse_doc(body)?, &window)))
        }
        Some("tokens") => doc_shot(a.json, &url, "/tokens", |body| Ok(report::tokens(&parse_doc(body)?, now()))),
        Some("model") => model_shot(a.json, &url),
        Some("compare") => compare_shot(&a, &url, &cfg),
        Some("bench") => bench_shot(&a, &url, &cfg),
        Some("maintenance") => maintenance_shot(&a, &url),
        Some(c) => match PageId::from_command(c) {
            Some(page) => page_shot(page, a.range.as_deref().unwrap_or(RANGES[DEFAULT_RANGE]), a.json, &url),
            None => one_shot(c, a.json, &url),
        },
    };
    std::process::exit(code);
}

fn usage(msg: &str) -> ! {
    eprintln!("lss: {msg}\n{HELP}");
    std::process::exit(2);
}

fn parse_doc<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| format!("the collector sent something this lss does not understand ({e}): upgrade lss or lss-collector"))
}

/// One collector document: verbatim with `--json`, else rendered. Exit 0 / 2.
fn doc_shot(json: bool, url: &str, path: &str, render: impl FnOnce(&str) -> Result<String, String>) -> i32 {
    let body = match data::fetch_raw(url, path) {
        Ok(b) => b,
        Err(e) => return unreachable(json, &e),
    };
    if json {
        println!("{}", body.trim_end());
        return 0;
    }
    match render(&body) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => unreachable(false, &e),
    }
}

fn users_shot(json: bool, url: &str) -> i32 {
    let raw = match client::fetch_raw(url) {
        Ok(r) => r,
        Err(e) => return unreachable(json, &e),
    };
    let status = match client::parse(&raw) {
        Ok(s) => s,
        Err(e) => return unreachable(json, &e),
    };
    if json {
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        println!("{}", serde_json::json!({"v": v["v"], "users": v["users"], "slots": v["serve"]["slots"], "running": v["serve"]["running"]}));
    } else {
        print!("{}", report::users(&status, now()));
    }
    i32::from(!status.serve.up)
}

fn model_shot(json: bool, url: &str) -> i32 {
    let loadouts = match data::fetch_raw(url, "/loadouts") {
        Ok(b) => b,
        Err(e) => return unreachable(json, &e),
    };
    let bench = data::fetch_raw(url, "/bench").ok();
    if json {
        let l: serde_json::Value = serde_json::from_str(&loadouts).unwrap_or_default();
        let current = l["loadouts"].as_array().and_then(|a| a.iter().find(|c| c["current"] == true).or(a.first())).cloned().unwrap_or_default();
        let b: serde_json::Value = bench.as_deref().and_then(|b| serde_json::from_str(b).ok()).unwrap_or_default();
        println!("{}", serde_json::json!({"v": 1, "model": current, "bench": b["brief"]}));
        return 0;
    }
    match parse_doc(&loadouts) {
        Ok(cards) => {
            let bench: Option<lss_core::bench::BenchDoc> = bench.as_deref().and_then(|b| serde_json::from_str(b).ok());
            // the targets live in /status; an older collector has none, and that is fine
            let targets: Option<lss_core::targets::TargetsStatus> = data::fetch_raw(url, "/status").ok().and_then(|b| serde_json::from_str::<lss_core::model::Status>(&b).ok()).map(|st| st.targets);
            print!("{}", report::model(&cards, bench.as_ref(), targets.as_ref(), now()));
            0
        }
        Err(e) => unreachable(false, &e),
    }
}

fn compare_shot(a: &Args, url: &str, cfg: &lss_core::config::ClientConfig) -> i32 {
    let [sel_a, sel_b] = a.words.as_slice() else { usage("compare needs two loadouts: lss compare current previous   (or best, an id from `lss loadouts`, a model name)") };
    let cards: lss_core::compare::LoadoutsDoc = match data::fetch_doc(url, "/loadouts") {
        Ok(d) => d,
        Err(e) => return unreachable(a.json, &e),
    };
    let pick = |sel: &str| lss_core::compare::resolve(sel, &cards.loadouts);
    let (ca, cb) = match (pick(sel_a), pick(sel_b)) {
        (Ok(x), Ok(y)) => (x, y),
        (Err(e), _) | (_, Err(e)) => {
            if a.json {
                println!("{}", serde_json::json!({"v": 1, "error": "no_such_loadout", "detail": e}));
            } else {
                println!("lss compare: {e}");
            }
            return 1;
        }
    };
    let cmp = lss_core::compare::compare(sel_a, ca, sel_b, cb);
    let mut paired: Option<serde_json::Value> = None;
    let mut paired_note = String::new();
    if a.accuracy {
        // the harness's own paired test (McNemar) on the two stored result files, per dataset both have
        let file = |c: &lss_core::compare::LoadoutCard, set: &str| c.accuracy.iter().flat_map(|s| s.accuracy.iter()).find(|x| x.dataset == set).map(|x| x.file.clone());
        let set = if a.dataset.is_empty() { "gsm8k" } else { &a.dataset };
        match (file(ca, set), file(cb, set)) {
            (Some(fa), Some(fb)) => {
                if !is_local(url) {
                    paired_note = format!("the paired accuracy test reads result files on the GPU box. Run it there:\n  {}", ssh_line(url, cfg, &["compare", sel_a, sel_b, "--accuracy", "--dataset", set]));
                } else {
                    match data::post_json(url, "/bench/compare-accuracy", &serde_json::json!({"a": fa, "b": fb})) {
                        Ok((200, body)) => paired = serde_json::from_str(&body).ok(),
                        Ok((_, body)) => paired_note = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or(body),
                        Err(e) => paired_note = e,
                    }
                }
            }
            _ => paired_note = format!("no stored `{set}` accuracy result on both sides: run `lss bench accuracy` on each loadout first (long: pick a quiet window)"),
        }
    }
    if a.json {
        let mut v = serde_json::to_value(&cmp).unwrap_or_default();
        if a.accuracy {
            v["paired_accuracy"] = paired.unwrap_or_else(|| serde_json::json!({"ok": false, "message": paired_note}));
        }
        println!("{v}");
    } else {
        print!("{}", report::comparison(&cmp));
        if a.accuracy {
            println!("\nPAIRED ACCURACY TEST (the harness's own, on the same questions)");
            match paired {
                Some(p) => println!("{}", p["text"].as_str().unwrap_or("").trim_end()),
                None => println!("  {paired_note}"),
            }
        }
    }
    0
}

/// Is the collector on this machine? (`POST /bench` is only accepted on 127.0.0.1.)
fn is_local(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h).trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn url_host(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
    host.rsplit_once(':').map_or(host, |(h, _)| h).to_string()
}

fn ssh_line(url: &str, cfg: &lss_core::config::ClientConfig, words: &[&str]) -> String {
    let host = if cfg.bench_ssh.trim().is_empty() { url_host(url) } else { cfg.bench_ssh.trim().to_string() };
    let quoted: Vec<String> = words.iter().map(|w| if w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:".contains(c)) { (*w).to_string() } else { format!("'{}'", w.replace('\'', "'\\''")) }).collect();
    format!("ssh {host} lss {}", quoted.join(" "))
}

fn bench_shot(a: &Args, url: &str, cfg: &lss_core::config::ClientConfig) -> i32 {
    let word = a.words.first().map_or("status", String::as_str);
    if word == "status" {
        return doc_shot(a.json, url, "/bench", |body| Ok(report::bench_status(&parse_doc(body)?, now())));
    }
    if word != "cancel" && lss_core::bench::Profile::parse(word).is_none() {
        usage(&format!("bench '{word}': use quick, full, accuracy, dry-run, status or cancel"));
    }
    if !is_local(url) {
        // a benchmark is started ON the GPU box: the collector refuses it from anywhere else
        let mut words: Vec<&str> = vec!["bench", word];
        if a.force {
            words.push("--force");
        }
        if a.under_load {
            words.push("--under-load");
        }
        if !a.note.is_empty() {
            words.extend(["--note", a.note.as_str()]);
        }
        if !a.dataset.is_empty() {
            words.extend(["--dataset", a.dataset.as_str()]);
        }
        if a.no_wait {
            words.push("--no-wait");
        }
        if a.json {
            words.push("--json");
        }
        let line = ssh_line(url, cfg, &words);
        if cfg.bench_ssh.trim().is_empty() {
            println!("A benchmark runs on the GPU box itself. Run this (or set `bench_ssh = \"<host>\"` in ~/.config/lss/lss.toml and lss will do it for you):\n  {line}");
            return 2;
        }
        eprintln!("lss: running on the GPU box: {line}");
        let mut argv: Vec<&str> = vec![cfg.bench_ssh.trim(), "lss"];
        argv.extend(words);
        return match std::process::Command::new("ssh").args(&argv).status() {
            Ok(s) => s.code().unwrap_or(2),
            Err(e) => {
                eprintln!("lss: cannot run ssh: {e}");
                2
            }
        };
    }
    let (path, body) = if word == "cancel" { ("/bench/cancel", serde_json::json!({})) } else { ("/bench", serde_json::json!({"profile": word, "force": a.force, "under_load": a.under_load, "note": a.note, "dataset": a.dataset})) };
    let (code, text) = match data::post_json(url, path, &body) {
        Ok(r) => r,
        Err(e) => return unreachable(a.json, &e),
    };
    let answer: lss_core::bench::BenchStarted = serde_json::from_str(&text).unwrap_or_else(|_| lss_core::bench::BenchStarted { message: format!("the collector answered HTTP {code}: {}", text.trim()), ..Default::default() });
    let (out, err, exit) = bench_start_result(a.json, word, a.under_load, code, &text, &answer);
    if !out.is_empty() {
        println!("{out}");
    }
    if !err.is_empty() {
        eprintln!("{err}");
    }
    if let Some(exit) = exit {
        return exit;
    }
    if !answer.ok || word == "cancel" || a.no_wait {
        return i32::from(!answer.ok);
    }
    println!("following it (ctrl-c stops watching, not the benchmark; `lss bench cancel` stops the benchmark)");
    let mut last_step = String::new();
    // card #329: a /bench document older than the run says idle and lists nothing - "not yet"
    let mut waiting_since: Option<i64> = None;
    let mut waited = 0_i64;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let doc: lss_core::bench::BenchDoc = match data::fetch_doc(url, "/bench") {
            Ok(d) => d,
            Err(e) => return unreachable(false, &e),
        };
        match lss_core::bench::follow_step(&doc, answer.run_id, waiting_since, waited) {
            lss_core::bench::Follow::Running(step) => {
                waiting_since = None;
                waited = 0;
                if step != last_step {
                    println!("  {}  {step}", lss_core::timeutil::fmt_local(now(), "%H:%M:%S"));
                    last_step = step;
                }
            }
            lss_core::bench::Follow::Waiting => {
                waiting_since.get_or_insert(doc.generated_at);
                waited += 2;
            }
            lss_core::bench::Follow::Finished(run) => {
                print!("{}", report::scorecard(run));
                if run.complete() && run.profile != "dry-run" {
                    println!("next: lss compare current previous");
                }
                return i32::from(!run.complete());
            }
            lss_core::bench::Follow::Lost => {
                eprintln!("lss: the collector accepted run {} but does not list it{}: see `lss bench status`", answer.run_id.map_or_else(|| "?".into(), |i| i.to_string()), if answer.run_id.is_none() { " (it is older than lss and gives no run id)" } else { "" });
                return 1;
            }
        }
    }
}

/// `lss maintenance start "reason" [--minutes N] | stop | status` (card #22): a planned window
/// so a deliberate restart is labelled, not read as an incident. `start`/`stop` POST straight to
/// the collector - like `lss bench`, they only take effect on the box itself (127.0.0.1); unlike
/// `lss bench` there is no ssh-relay convenience here, since a maintenance toggle is a much
/// smaller ask than driving a benchmark remotely and the collector's own refusal message already
/// says exactly what to do.
fn maintenance_shot(a: &Args, url: &str) -> i32 {
    let word = a.words.first().map_or("status", String::as_str);
    match word {
        "status" => doc_shot(a.json, url, "/status", |body| Ok(report::maintenance_status(&parse_doc(body)?, now()))),
        "start" => {
            let reason = a.words.get(1).map(String::as_str).unwrap_or("");
            if reason.trim().is_empty() {
                usage("maintenance start needs a reason: lss maintenance start \"reason\" [--minutes N]");
            }
            let body = serde_json::json!({"reason": reason, "minutes": a.minutes});
            maintenance_post(url, "/maintenance/start", &body, a.json)
        }
        "stop" => maintenance_post(url, "/maintenance/stop", &serde_json::json!({}), a.json),
        other => usage(&format!("maintenance '{other}': use start, stop or status")),
    }
}

fn maintenance_post(url: &str, path: &str, body: &serde_json::Value, json: bool) -> i32 {
    let (code, text) = match data::post_json(url, path, body) {
        Ok(r) => r,
        Err(e) => return unreachable(json, &e),
    };
    let reply: lss_core::maintenance::MaintenanceReply = serde_json::from_str(&text).unwrap_or_else(|_| lss_core::maintenance::MaintenanceReply { ok: false, message: format!("the collector answered HTTP {code}: {}", text.trim()), expires_at: 0 });
    if json {
        println!("{}", text.trim_end());
    } else {
        println!("{}", reply.message);
    }
    i32::from(!reply.ok)
}

fn one_shot(cmd: &str, json: bool, url: &str) -> i32 {
    let raw = match client::fetch_raw(url) {
        Ok(r) => r,
        Err(e) => return unreachable(json, &e),
    };
    let status = match client::parse(&raw) {
        Ok(s) => s,
        Err(e) => return unreachable(json, &e),
    };
    let t = now();
    if json {
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let out = match cmd {
            "incidents" => serde_json::json!({"v": v["v"], "incidents": v["incidents"]}),
            "alerts" => serde_json::json!({"v": v["v"], "firing": v["firing"], "alerts": v["alerts"]}),
            "probe" => serde_json::json!({"v": v["v"], "probe": v["probe"]}),
            _ => v,
        };
        println!("{out}");
    } else {
        print!("{}", match cmd {
            "incidents" => plain::incidents(&status, t),
            "alerts" => plain::alerts(&status),
            "probe" => plain::probe(&status, t),
            _ => plain::status(&status, t),
        });
    }
    i32::from(!status.serve.up)
}

/// `lss latency|load|gpus|gateway|rules`: the same data a detail page draws, as text or JSON.
/// Never needs a terminal. Exit code as for `status`.
fn page_shot(page: PageId, range: &str, json: bool, url: &str) -> i32 {
    let status = match client::fetch(url) {
        Ok(s) => s,
        Err(e) => return unreachable(json, &e),
    };
    let ctx = data::PageCtx::of(&status);
    if json {
        // the collector's own documents, verbatim, keyed by what they are
        let mut out = serde_json::json!({"v": 1, "page": page.command(), "range": range});
        for path in data::page_paths(page, range, &ctx) {
            let body = match data::fetch_raw(url, &path) {
                Ok(b) => b,
                Err(e) => return unreachable(true, &e),
            };
            let doc: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let key = path.trim_start_matches('/').split('?').next().unwrap_or("doc").to_string();
            match (key.as_str(), &mut out[&key]) {
                ("hist", serde_json::Value::Array(list)) => list.push(doc),
                ("hist", slot) => *slot = serde_json::json!([doc]),
                (_, slot) => *slot = doc,
            }
        }
        if page == PageId::Gpus {
            out["gpus"] = serde_json::to_value(&status.gpus).unwrap_or_default();
        }
        println!("{out}");
    } else {
        let d = match RANGES.iter().position(|r| *r == range) {
            Some(idx) => data::fetch_page(url, page, idx, &ctx, now()),
            None => {
                // a range the screen does not cycle through: ask for exactly it
                let mut d = PageData { page: Some(page), ..Default::default() };
                for path in data::page_paths(page, range, &ctx) {
                    if let Err(e) = d.take(&path, data::fetch_raw(url, &path)) {
                        d.error.get_or_insert(e);
                    }
                }
                d
            }
        };
        if d.is_empty() {
            return unreachable(false, d.error.as_deref().unwrap_or("the collector sent nothing"));
        }
        print!("{}", plain::page(&d, page, range, now()));
        if page == PageId::Gpus {
            print!("{}", plain::gpu_health(&status));
        }
    }
    i32::from(!status.serve.up)
}

/// What `lss bench <profile>` does with the collector's answer to a START request: what goes to
/// stdout, what goes to stderr, and whether this is the end of the command (`Some(exit)`) or the
/// run is to be followed (`None`).
///
/// Card #208, 2026-09-23 - THE ERROR WAS UNREACHABLE FOR THE ONE CALLER THAT NEEDED IT. The
/// version-skew check card #14 added sat BELOW a `--json` early return that printed the body and
/// returned, so `lss bench quick --under-load --json` against a collector too old to know the flag
/// exited 0 and said nothing. A script is exactly the caller that would pass `--json` and exactly
/// the caller least able to notice a flag silently dropped on the floor. The check now runs FIRST,
/// before anything is printed, so there is one ordering for both surfaces instead of two.
///
/// ON THE `--json` PATH IT IS AN ERROR OBJECT, not the sentence. The sentence would land in a
/// caller's `json.loads` as a parse failure, which is a second silent failure dressed as a first
/// one. The envelope is the one every other machine-readable error in this binary already uses
/// (`unreachable` below): `v` / `error` / `detail`, on STDOUT, with a stable `error` code a caller
/// can branch on without matching prose. The collector's own answer is carried WHOLE under
/// `answer`, so a script that wanted `run_id` still has it and nothing is traded away for the
/// error; and it is ONE document, because two JSON documents on stdout parse as neither.
///
/// Card #214 (verifier-9 on #208): A REPLY THAT IS NOT JSON AT ALL - a proxy's error page, a 502
/// from something in front of the collector - is reported as exactly that, FIRST. It used to fall
/// into the placeholder answer, which carries no `under_load` echo, so with `--under-load` it was
/// diagnosed as "collector too old" (a wrong fix to go chasing), and under `--json` its raw text
/// was dropped to `null` - or, without the flag, printed as if it were the JSON answer. Now: the
/// error envelope with a stable code, the HTTP status and a body excerpt; exit 2 on both surfaces.
fn bench_start_result(json: bool, word: &str, asked_under_load: bool, code: u16, text: &str, answer: &lss_core::bench::BenchStarted) -> (String, String, Option<i32>) {
    if !serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| v.is_object()) {
        let excerpt: String = text.trim().chars().take(BODY_EXCERPT_CHARS).collect();
        if json {
            let doc = serde_json::json!({"v": 1, "error": "collector_reply_not_json", "detail": format!("the {word} request got HTTP {code} with a body that is not a bench answer - a proxy or error page between lss and the collector, not the collector itself"), "http_status": code, "body_excerpt": excerpt});
            return (doc.to_string(), String::new(), Some(2));
        }
        let one_line = match excerpt.split_whitespace().collect::<Vec<_>>().join(" ") { l if l.is_empty() => "an empty body".to_string(), l => l };
        return (String::new(), format!("lss: the collector did not answer the {word} request with JSON (HTTP {code}) - something between lss and the collector replied instead: {one_line}"), Some(2));
    }
    // card #14, 2026-09-23: the collector echoes back what it understood. It did not understand
    // this, so it is older than the flag and silently dropped it - say so HERE, rather than let
    // the next thing that happens (a refusal telling you to --force, then a run that dies at the
    // first foreign request) be the only clue. Found the hard way: `lss` was installed on the
    // box and `lss-collector` was not, and the run looked like the flag was broken.
    if asked_under_load && word != "cancel" && !answer.under_load {
        if json {
            let body = serde_json::from_str::<serde_json::Value>(text).unwrap_or(serde_json::Value::Null);
            return (serde_json::json!({"v": 1, "error": "collector_too_old_for_under_load", "detail": UNDER_LOAD_IGNORED, "answer": body}).to_string(), String::new(), Some(2));
        }
        return (answer.message.clone(), UNDER_LOAD_IGNORED.to_string(), Some(2));
    }
    if json {
        return (text.trim_end().to_string(), String::new(), Some(i32::from(!answer.ok)));
    }
    (answer.message.clone(), String::new(), None)
}

/// card #214: how much of a non-JSON reply is carried back - enough to recognise a proxy's error
/// page, not a whole HTML document.
const BODY_EXCERPT_CHARS: usize = 400;

fn unreachable(json: bool, err: &str) -> i32 {
    // card #180 gate 6: the collector is running and says it has no LLM server to watch yet -
    // that is "nothing is serving" (exit 1), not "the monitor is broken" (exit 2)
    if let Some(why) = err.strip_prefix(lss::client::WAITING) {
        if json {
            println!("{}", serde_json::json!({"v": 1, "error": "no_engine_yet", "detail": why}));
        } else {
            println!("NO LLM SERVER YET: {why}");
        }
        return 1;
    }
    if json {
        println!("{}", serde_json::json!({"v": 1, "error": "collector_unreachable", "detail": err}));
    } else {
        println!("COLLECTOR UNREACHABLE: {err}");
        println!("the serve may be fine; the monitor is not. On the GPU box: systemctl --user status lss-collector");
    }
    2
}

/// Every message says which collector it came from: after `[` / `]` the answers of the server
/// that was on screen before are dropped, not drawn.
enum Msg {
    Status(String, Box<Result<Status, String>>),
    Page(String, Box<PageData>),
    Fleet(usize, Result<ui::FleetState, String>),
}

/// The FLEET strip: every configured server's `/status`, every 5 s, each on its own thread so
/// one unreachable box never delays the others.
fn spawn_fleet(servers: &[(String, String)], tx: &mpsc::Sender<Msg>) {
    for (i, (_, url)) in servers.iter().enumerate() {
        let (url, tx) = (url.clone(), tx.clone());
        std::thread::spawn(move || loop {
            let r = client::fetch(&url).map(|s| ui::FleetState {
                up: s.serve.up,
                host: s.host,
                engine: s.serve.engine,
                decode_tok_s: s.serve.decode_tok_s,
                prefill_tok_s: s.serve.prefill_tok_s,
                running: s.serve.running,
                slots: s.serve.slots,
                kv_usage: s.serve.kv_usage,
                gpu_count: s.gpus.len(),
                total_watts: (!s.gpus.is_empty()).then(|| s.gpus.iter().filter_map(|g| g.sample.power_w).sum()),
                cost_per_hour: s.cost.and_then(|c| c.live_usd_per_hour),
                generated_tokens_total: s.serve.generation_tokens_total,
                not_reported: s.serve.not_reported,
                model: s.serve.model,
            });
            if tx.send(Msg::Fleet(i, r)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(5));
        });
    }
}

/// What the screen is looking at, shared with the fetch thread: (page, range index).
type Wanted = Arc<Mutex<Option<(PageId, usize)>>>;

/// Fetching happens off the UI thread: a hung network never freezes the keys. `/status` every
/// 2 s; the open page's history every 5 s, and at once when the page or the range changes.
fn spawn_fetcher(current: Arc<Mutex<String>>, wanted: Wanted, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let mut ctx = data::PageCtx::default();
        let mut last_status: Option<Instant> = None;
        let mut last_page: Option<(Instant, (PageId, usize))> = None;
        let mut url = String::new();
        loop {
            let now_url = current.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if now_url != url {
                // another server: nothing fetched so far applies to it
                (url, ctx, last_status, last_page) = (now_url, data::PageCtx::default(), None, None);
            }
            if last_status.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                let r = client::fetch(&url);
                if let Ok(s) = &r {
                    ctx = data::PageCtx::of(s);
                }
                last_status = Some(Instant::now());
                if tx.send(Msg::Status(url.clone(), Box::new(r))).is_err() {
                    return;
                }
            }
            let want = *wanted.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(w) = want {
                let due = last_page.is_none_or(|(t, was)| was != w || t.elapsed() >= Duration::from_secs(5));
                if due {
                    let d = data::fetch_page(&url, w.0, w.1, &ctx, now());
                    last_page = Some((Instant::now(), w));
                    if tx.send(Msg::Page(url.clone(), Box::new(d))).is_err() {
                        return;
                    }
                }
            } else {
                last_page = None;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    });
}

fn run_tui(servers: Vec<(String, String)>, server_idx: usize, demo: bool, no_save: bool, cfg: &lss_core::config::ClientConfig) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let wanted: Wanted = Arc::default();
    let url = servers[server_idx].1.clone();
    let current = Arc::new(Mutex::new(url.clone()));
    if !demo {
        if servers.len() > 1 {
            spawn_fleet(&servers, &tx);
        }
        spawn_fetcher(current.clone(), wanted.clone(), tx);
    }
    let prefs_path = prefs::default_path();
    // #92: --demo shows a small synthetic fleet too (never a live fetch - `spawn_fleet` is
    // already skipped above when `demo`), so `f` has something real to demo without a collector.
    let fleet: Vec<ui::FleetEntry> = if demo { lss::demo::fleet(&lss::demo::status()) } else { servers.iter().map(|(name, url)| ui::FleetEntry { name: name.clone(), url: url.clone(), state: None }).collect() };
    let mut app = App { url: url.clone(), bench_local: is_local(&url), bench_command: ssh_line(&url, cfg, &["bench", "quick"]), light: cfg.theme == "light", fleet, server_idx, ..Default::default() };
    if let Some(p) = prefs_path.as_ref().filter(|p| p.exists()) {
        // what the person chose with `T` wins over the config's first-start theme
        prefs::load(p).apply(&mut app);
    }
    // #48, 2026-09-21: $LSS_CHART wins over the saved pref, which wins over the dots default -
    // this never touches theme/layout/range, only the one field it names.
    app.chart_lines = prefs::resolve_chart_lines(std::env::var("LSS_CHART").ok().as_deref(), app.chart_lines);
    let mut saver = prefs::Saver::new(prefs_path.clone(), no_save, prefs::Prefs::of(&app));
    let mut terminal = ratatui::init(); // installs a panic hook that restores the terminal
    let result = (|| -> std::io::Result<()> {
        loop {
            if demo {
                let s = lss::demo::status();
                let t = s.generated_at + 3;
                if let Some((page, range)) = app.wanted().filter(|(p, r)| !app.page_data.is_for(*p, *r)) {
                    app.page_data = lss::demo::page(&s, page, range, t);
                }
                app.last_ok = Some(t);
                app.status = Some(s);
            }
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Fleet(i, r) => {
                        if let Some(e) = app.fleet.get_mut(i) {
                            e.state = Some(r);
                        }
                    }
                    Msg::Status(from, _) | Msg::Page(from, _) if from != app.url => {}
                    Msg::Status(_, r) => match *r {
                        Ok(s) => {
                            app.status = Some(s);
                            app.error = None;
                            app.last_ok = Some(now());
                        }
                        Err(e) => app.error = Some(e),
                    },
                    Msg::Page(_, d) => app.page_data = *d,
                }
            }
            let clock = if demo { app.last_ok.unwrap_or_else(now) } else { now() };
            terminal.draw(|f| ui::draw(f, &app, clock))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        return Ok(());
                    }
                    if app.on_key(k.code) {
                        return Ok(());
                    }
                    let action = app.action.take();
                    if let Some(ui::Action::SwitchServer) = action {
                        *current.lock().unwrap_or_else(|e| e.into_inner()) = app.url.clone();
                        app.bench_local = is_local(&app.url);
                        let mut per_server = cfg.clone();
                        if let Some(own) = cfg.server.iter().find(|s| s.url.trim().trim_end_matches('/') == app.url && !s.bench_ssh.trim().is_empty()) {
                            per_server.bench_ssh = own.bench_ssh.clone();
                        }
                        app.bench_command = ssh_line(&app.url, &per_server, &["bench", "quick"]);
                    }
                    if let Some(ui::Action::StartBench) = action {
                        // only offered when the collector is on this machine (it refuses anyone else)
                        app.bench_message = Some(match data::post_json(&app.url, "/bench", &serde_json::json!({"profile": "quick", "note": "started from the MODEL page"})) {
                            Ok((_, body)) => serde_json::from_str::<lss_core::bench::BenchStarted>(&body).map(|b| b.message).unwrap_or(body),
                            Err(e) => e,
                        });
                    }
                    *wanted.lock().unwrap_or_else(|e| e.into_inner()) = app.wanted();
                    saver.note(prefs::Prefs::of(&app));
                }
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn help_stays_short_and_names_every_command() {
        assert!(super::HELP.lines().count() <= 30, "lss --help must fit a small pane: {} lines", super::HELP.lines().count());
        for cmd in super::COMMANDS.iter().copied().chain(["--range", "--json", "--force", "--note", "--accuracy", "quick", "full", "accuracy", "dry-run", "1-9"]) {
            assert!(super::HELP.contains(cmd), "{cmd} missing from --help");
        }
        assert!(super::HELP.lines().all(|l| l.chars().count() <= 100), "{:?}", super::HELP.lines().map(|l| l.chars().count()).max());
        assert!(super::HELP.contains("else http://127.0.0.1:8099"), "the default collector is this machine, and no other address is in the help");
        assert_eq!(super::HELP.matches("http://").count(), 1);
    }

    #[test]
    fn a_benchmark_is_only_started_locally_and_the_ssh_line_is_exact() {
        use super::{is_local, ssh_line, url_host};
        assert!(is_local("http://127.0.0.1:8099") && is_local("http://localhost:8099/") && is_local("http://[::1]:8099"));
        assert!(!is_local("http://gpu-box:8099") && !is_local("http://192.0.2.32:8099"));
        assert_eq!(url_host("http://gpu-box:8099/"), "gpu-box");
        let none = lss_core::config::ClientConfig::default();
        assert_eq!(ssh_line("http://192.0.2.32:8099", &none, &["bench", "quick", "--note", "after the swap"]), "ssh 192.0.2.32 lss bench quick --note 'after the swap'");
        let named = lss_core::config::ClientConfig { bench_ssh: "gpu".into(), ..Default::default() };
        assert_eq!(ssh_line("http://192.0.2.32:8099", &named, &["bench", "full", "--force"]), "ssh gpu lss bench full --force");
        assert_eq!(ssh_line("http://h:1", &none, &["bench", "quick", "--note", "it's new"]), "ssh h lss bench quick --note 'it'\\''s new'");
    }

    /// Card #208, 2026-09-23, found by verifier-8 while verifying card #14's fix: the skew error
    /// was printed BELOW the `--json` early return, so the surface a script uses never reached it.
    /// `--json` exited 0, in silence, on a run that was NOT measuring what was asked for.
    #[test]
    fn a_reply_that_is_not_json_is_reported_as_what_it_is_and_keeps_its_text() {
        use super::bench_start_result;
        // what a proxy in front of the collector sends when the collector is not there
        let page = "<html><head><title>502 Bad Gateway</title></head>\n<body><center><h1>502 Bad Gateway</h1></center></body></html>";
        let answer = lss_core::bench::BenchStarted { message: format!("the collector answered HTTP 502: {page}"), ..Default::default() };
        for under_load in [true, false] {
            // --json: ONE error document with a stable code and the raw text, never null, never
            // the page itself posing as the JSON answer, and never "collector too old"
            let (out, err, exit) = bench_start_result(true, "quick", under_load, 502, page, &answer);
            assert_eq!(exit, Some(2), "still a non-zero exit: {out}");
            let doc: serde_json::Value = serde_json::from_str(&out).expect("a --json caller gets JSON");
            assert_eq!(doc["error"], "collector_reply_not_json", "{out}");
            assert_eq!(doc["http_status"], 502);
            assert!(doc["body_excerpt"].as_str().unwrap().contains("502 Bad Gateway"), "the raw text is carried: {out}");
            assert!(!out.contains("too_old") && err.is_empty(), "{out}");
            // human: named for what it is, on stderr, exit 2
            let (out, err, exit) = bench_start_result(false, "quick", under_load, 502, page, &answer);
            assert_eq!(exit, Some(2));
            assert!(out.is_empty() && err.contains("did not answer the quick request with JSON (HTTP 502)") && err.contains("502 Bad Gateway"), "{err}");
            assert!(!err.contains("too old") && !err.contains("under-load"), "not a version skew: {err}");
        }
        // a huge page is carried as an excerpt, not whole
        let big = format!("<html>{}</html>", "x".repeat(10_000));
        let doc: serde_json::Value = serde_json::from_str(&bench_start_result(true, "quick", false, 500, &big, &answer).0).unwrap();
        assert!(doc["body_excerpt"].as_str().unwrap().chars().count() <= super::BODY_EXCERPT_CHARS);
    }

    #[test]
    fn an_empty_200_reply_is_not_json_either() {
        // card #237 (lss-verifier on #214, mutation C): an EMPTY body with HTTP 200 - what a
        // proxy or a half-closed keep-alive in front of the collector can hand back - carries
        // no under_load echo either, so it must be claimed by the not-JSON branch and never
        // fall through to "collector too old". Whitespace-only is the same reply.
        use super::bench_start_result;
        let answer = lss_core::bench::BenchStarted { message: "the collector answered HTTP 200: ".into(), ..Default::default() };
        for body in ["", "\r\n", "   \n"] {
            for under_load in [true, false] {
                let (out, err, exit) = bench_start_result(true, "quick", under_load, 200, body, &answer);
                assert_eq!(exit, Some(2), "{body:?} under_load={under_load}: {out}");
                let doc: serde_json::Value = serde_json::from_str(&out).expect("a --json caller gets JSON");
                assert_eq!(doc["error"], "collector_reply_not_json", "{body:?} under_load={under_load}: {out}");
                assert_eq!(doc["http_status"], 200);
                assert_eq!(doc["body_excerpt"], "", "an empty reply has an empty excerpt, not a placeholder");
                assert!(err.is_empty() && !out.contains("too_old"), "{out}");
                let (out, err, exit) = bench_start_result(false, "quick", under_load, 200, body, &answer);
                assert_eq!(exit, Some(2));
                assert!(out.is_empty() && err.contains("did not answer the quick request with JSON (HTTP 200)"), "{body:?}: {err}");
                assert!(err.ends_with("replied instead: an empty body") && !err.contains("too old"), "{err}");
            }
        }
    }

    #[test]
    fn a_collector_too_old_for_under_load_is_an_error_on_the_json_surface_too() {
        use super::{bench_start_result, UNDER_LOAD_IGNORED};
        // an OLD collector: it accepted the run and echoed no `under_load` at all, which is
        // indistinguishable, in the body, from an answer to a request that never carried it
        let old = r#"{"v":1,"ok":true,"run_id":7,"message":"started"}"#;
        let answer: lss_core::bench::BenchStarted = serde_json::from_str(old).unwrap();
        assert!(answer.ok && !answer.under_load, "the premise: accepted, with no echo");

        let (out, err, exit) = bench_start_result(true, "quick", true, 200, old, &answer);
        assert_eq!(exit, Some(2), "--json must not exit 0 on a flag the collector dropped: {out}");
        let doc: serde_json::Value = serde_json::from_str(&out).expect("a --json caller gets JSON, never a bare sentence");
        assert_eq!(doc["error"], "collector_too_old_for_under_load", "a stable code to branch on, not prose to match");
        assert_eq!(doc["detail"], UNDER_LOAD_IGNORED, "the human sentence survives, inside the envelope");
        assert_eq!((doc["v"].as_i64(), doc["answer"]["run_id"].as_i64()), (Some(1), Some(7)), "the collector's whole answer is carried, so nothing is traded away for the error");
        assert!(err.is_empty() && out.lines().count() == 1, "one document on stdout: two would parse as neither");

        // the same skew on the human surface: unchanged - the message, then the named error
        let (out, err, exit) = bench_start_result(false, "quick", true, 200, old, &answer);
        assert_eq!((out.as_str(), err.as_str(), exit), ("started", UNDER_LOAD_IGNORED, Some(2)));

        // a REFUSAL from an old collector skews too, and that is where it bites first: the
        // operator is about to be told to --force by a gate the flag was meant to skip
        let refused = r#"{"v":1,"ok":false,"run_id":null,"message":"the server is busy"}"#;
        let no: lss_core::bench::BenchStarted = serde_json::from_str(refused).unwrap();
        assert_eq!(bench_start_result(true, "quick", true, 200, refused, &no).2, Some(2));

        // AND THE CONTROL, or the assertions above would pass just as well with the flag error
        // fired on every run: a collector that DOES echo the flag is passed through untouched,
        // body and exit code, on both surfaces
        let fresh = r#"{"v":1,"ok":true,"run_id":7,"message":"started","under_load":true}"#;
        let yes: lss_core::bench::BenchStarted = serde_json::from_str(fresh).unwrap();
        assert_eq!(bench_start_result(true, "quick", true, 200, fresh, &yes), (fresh.to_string(), String::new(), Some(0)));
        assert_eq!(bench_start_result(false, "quick", true, 200, fresh, &yes), ("started".to_string(), String::new(), None), "a human run is followed, not returned from");
        // ... and a run that never asked for the flag is never accused of a skew
        assert_eq!(bench_start_result(true, "quick", false, 200, old, &answer), (old.to_string(), String::new(), Some(0)));
        // ... nor is `bench cancel`, which does not carry the flag at all
        assert_eq!(bench_start_result(true, "cancel", true, 200, old, &answer).2, Some(0));
        // a refused run still exits 1 rather than 0 when there is no skew to report
        assert_eq!(bench_start_result(true, "quick", false, 200, refused, &no), (refused.to_string(), String::new(), Some(1)));
    }
}
