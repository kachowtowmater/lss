//! card #297: `lss-collector setup` (also `lss setup`, and what install.sh runs) - the setup
//! wizard that connects lss to ANY local inference server:
//!
//!   welcome -> find (what answered, and how sure) -> choose or type the engine + address + key
//!   -> a LIVE test (models, engine, metrics, plain-English errors) -> electricity cost
//!   (`lss-collector cost-setup`, card #298) -> write the configs (never replacing a file
//!   without asking; a backup is kept) -> restart the background service -> "type lss".
//!
//! Re-runnable: run it again to point lss at another engine; only the engine part of an
//! existing collector.toml changes. Answers come from the terminal (`/dev/tty`, so it works
//! under `curl ... | bash`), else stdin; `--yes` asks nothing. The text a person reads is built
//! in `lss_core::setup` (unit-tested there); this file is the I/O around it, driven in tests by
//! scripted answers against a fake engine.

use crate::detect;
use lss_core::config::{parse_config, Config};
use lss_core::engine::{self, EngineKind, Found, METRIC_FIELDS};
use lss_core::setup::{self as s, EngineChoice, HttpOutcome};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const USAGE: &str = "lss-collector setup [options]   (also: lss setup)
  The setup wizard: finds the LLM server (or asks for its address), tests the connection live,
  sets the electricity rate, writes ~/.config/lss/{collector,lss}.toml and restarts the
  background collector. Run it again any time to change the engine.
  --url ADDR          (or --engine-url) the engine's address: localhost:8000, 192.0.2.20:8000,
                      http://host:port or https://host:port
                      (skips the scan)
  --kind KIND         sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai | auto (default)
  --api-key KEY       the engine's API key, if it was started with one (or $LSS_API_KEY)
  --all               several engines found: watch every one (one collector each)
  --zip ZIP | --rate USD_PER_KWH | --from-ip | --skip-cost
                      the electricity step without questions (--from-ip = one lookup of this
                      machine's public IP, with your consent given by the flag)
  --yes               ask nothing: --url, else the engine already configured, else the first
                      found; keep existing files
  --force             replace existing config files without asking (a .bak copy is kept)
  --config-dir DIR    default $XDG_CONFIG_HOME/lss or ~/.config/lss
  --prefix DIR        where lss-notify.sh is installed (default ~/.local/bin)
  --no-service        do not restart the background collector (install.sh sets it up itself)
Exit: 0 done, 1 could not write, or a --zip/--rate/--from-ip given was refused or failed
      (e.g. --rate 2.5: cents or dollars? - nothing written), 2 bad arguments, 3 stopped by you
      (nothing written).";

/// The first collector's own port; a second engine's collector gets the next one.
pub const BASE_PORT: u16 = 8099;

#[derive(Debug, Default, Clone)]
pub struct Opts {
    pub url: Option<String>,
    pub kind: Option<String>,
    pub api_key: Option<String>,
    pub all: bool,
    pub cost: Vec<String>,
    pub yes: bool,
    pub force: bool,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub prefix: PathBuf,
    pub no_service: bool,
}

pub fn parse_args(args: &[String], home: &str) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut o = Opts {
        config_dir: PathBuf::from(env("XDG_CONFIG_HOME").unwrap_or_else(|| format!("{home}/.config"))).join("lss"),
        state_dir: PathBuf::from(env("XDG_STATE_HOME").unwrap_or_else(|| format!("{home}/.local/state"))).join("lss"),
        prefix: PathBuf::from(format!("{home}/.local/bin")),
        api_key: env("LSS_API_KEY"),
        ..Default::default()
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--url" | "--engine-url" => o.url = Some(val("--url")?),
            "--kind" => {
                let k = val("--kind")?;
                if k != "auto" && EngineKind::parse(&k).is_none() {
                    return Err(format!("--kind '{k}': one of sglang, vllm, llamacpp, ollama, lmstudio, tgi, openai, auto"));
                }
                o.kind = Some(k);
            }
            "--api-key" => o.api_key = Some(val("--api-key")?),
            "--all" => o.all = true,
            "--zip" => o.cost = vec!["--zip".into(), val("--zip")?],
            "--rate" => o.cost = vec!["--rate".into(), val("--rate")?],
            "--from-ip" => o.cost = vec!["--from-ip".into()],
            "--skip-cost" => o.cost = vec!["--skip".into()],
            "--yes" | "-y" => o.yes = true,
            "--force" => o.force = true,
            "--config-dir" => o.config_dir = PathBuf::from(val("--config-dir")?),
            "--prefix" => o.prefix = PathBuf::from(val("--prefix")?),
            "--no-service" => o.no_service = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
}

// ------------------------------------------------------------------ talking to the person

/// Where answers come from and where the questions go.
pub struct Io<'a> {
    input: Box<dyn BufRead + 'a>,
    out: Box<dyn Write + 'a>,
    /// false = never wait for an answer: every question takes its default
    pub interactive: bool,
    /// true = the input is a real terminal (typing a key can be hidden)
    tty: bool,
}

impl<'a> Io<'a> {
    #[cfg(test)]
    pub fn new(input: Box<dyn BufRead + 'a>, out: Box<dyn Write + 'a>, interactive: bool) -> Self {
        Io { input, out, interactive, tty: false }
    }

    /// The terminal, even under `curl | bash` (stdin is the script there): `/dev/tty`, else
    /// stdin when it is a terminal, else nothing to ask - every question takes its default.
    pub fn terminal(yes: bool) -> Io<'static> {
        let out: Box<dyn Write> = Box::new(std::io::stdout());
        if yes {
            return Io { input: Box::new(std::io::empty()), out, interactive: false, tty: false };
        }
        if let Ok(f) = std::fs::File::open("/dev/tty") {
            return Io { input: Box::new(std::io::BufReader::new(f)), out, interactive: true, tty: true };
        }
        // SAFETY: isatty only reads the file descriptor's state
        let stdin_tty = unsafe { libc::isatty(0) } == 1;
        // stdin that is NOT a terminal is never read for answers: under `curl ... | bash` it is
        // the rest of the install script itself
        Io { input: Box::new(std::io::BufReader::new(std::io::stdin())), out, interactive: stdin_tty, tty: stdin_tty }
    }

    pub fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }

    /// Ask; Enter (or no terminal, or the end of the input) = `default`.
    pub fn ask(&mut self, question: &str, default: &str) -> String {
        if default.is_empty() {
            let _ = write!(self.out, "{question} ");
        } else {
            let _ = write!(self.out, "{question} [{default}] ");
        }
        if !self.interactive {
            self.say(if default.is_empty() { "(no answer: none)" } else { default });
            return default.to_string();
        }
        let _ = self.out.flush();
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // the input ended: every later question takes its default too
                self.interactive = false;
                self.say("");
                default.to_string()
            }
            Ok(_) => {
                let t = line.trim();
                if t.is_empty() { default.to_string() } else { t.to_string() }
            }
        }
    }

    pub fn yes_no(&mut self, question: &str, default_yes: bool) -> bool {
        if !self.interactive {
            // #320 (1): never "[y/N] y/N" - say what was taken, in words
            self.say(&format!("{question} (no terminal: {})", if default_yes { "yes" } else { "no" }));
            return default_yes;
        }
        let a = self.ask(question, if default_yes { "Y/n" } else { "y/N" });
        match a.to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default_yes,
        }
    }

    /// A secret: not echoed when the input is a real terminal.
    pub fn ask_secret(&mut self, question: &str) -> String {
        let hide = self.tty && self.interactive;
        if hide {
            stty(&["-echo"]);
        }
        let a = self.ask(question, "");
        if hide {
            stty(&["echo"]);
            self.say("");
        }
        a
    }
}

fn stty(args: &[&str]) {
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        let _ = std::process::Command::new("stty").args(args).stdin(tty).status();
    }
}

// ------------------------------------------------------------------ the live test

/// One GET against the engine, classified.
pub fn get(url: &str, path: &str, key: &str, tls: &crate::tls::TlsOptions, timeout: Duration) -> HttpOutcome {
    // card #311: the collector's TLS agent, so an https:// engine is tested the way it is watched;
    // card #321: with the certificate choice made in this wizard (tls_ca_file / tls_verify)
    let builder = if *tls == crate::tls::TlsOptions::default() { crate::tls::agent() } else { crate::tls::agent_builder(tls) };
    let agent = builder.timeout_connect(Duration::from_secs(3)).timeout(timeout).build();
    let mut req = agent.get(&format!("{}{path}", url.trim_end_matches('/')));
    if !key.trim().is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", key.trim()));
    }
    match req.call() {
        Ok(r) => r.into_string().map_or_else(|e| HttpOutcome::Other(format!("read: {e}")), HttpOutcome::Ok),
        Err(ureq::Error::Status(code, _)) => HttpOutcome::Status(code),
        // a certificate problem is said in words, with both fixes (tls_ca_file / tls_verify)
        Err(e) => crate::tls::explain_cert_error(&e.to_string()).map_or_else(|| HttpOutcome::from_transport(&e.to_string()), HttpOutcome::Other),
    }
}

/// What the live test found.
#[derive(Debug, Clone)]
pub struct TestReport {
    /// Ok(model ids) or the failure of `/v1/models`
    pub models: Result<Vec<String>, HttpOutcome>,
    /// the engine as recognised (or as chosen, when it is not recognisable)
    pub kind: Option<EngineKind>,
    pub why: String,
    pub metrics: Option<s::MetricsVerdict>,
    /// card #326: false = nothing was read from the server (refused, timed out, a certificate it
    /// would not accept), so `kind` is only what was ASKED for (`--kind`) - never "[ok]"
    pub kind_confirmed: bool,
}

impl TestReport {
    pub fn connected(&self) -> bool {
        self.models.is_ok()
    }
}

/// Ask the engine what lss will ask it: its models, what it is, and its metrics.
pub fn live_test(url: &str, kind: Option<EngineKind>, key: &str, tls: &crate::tls::TlsOptions) -> TestReport {
    let timeout = Duration::from_secs(8);
    let fetch = |path: &str| match get(url, path, key, tls, timeout) {
        HttpOutcome::Ok(body) => Ok(body),
        HttpOutcome::Status(c) => Err(format!("HTTP {c}")),
        other => Err(format!("{other:?}")),
    };
    let models = match get(url, "/v1/models", key, tls, timeout) {
        HttpOutcome::Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => match v["data"].as_array() {
                Some(list) => Ok(list.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()),
                None => Err(HttpOutcome::Other("/v1/models answered, but not with a model list - not an OpenAI-compatible engine?".into())),
            },
            Err(_) => Err(HttpOutcome::Other("/v1/models answered with something that is not JSON - is this the engine's port, or a web page?".into())),
        },
        // Ollama before its OpenAI layer, and a few servers, have no /v1/models: recognised below
        other => Err(other),
    };
    // a connection that failed outright needs no more questions
    if matches!(models, Err(HttpOutcome::Refused | HttpOutcome::Timeout | HttpOutcome::NoSuchHost | HttpOutcome::Status(401 | 403))) {
        return TestReport { models, kind, why: String::new(), metrics: None, kind_confirmed: false };
    }
    // card #311 (verifier F3): a refused certificate means NOTHING was read - no metrics verdict
    // (it used to blame "a proxy" for metrics it never got to ask for)
    if matches!(&models, Err(HttpOutcome::Other(m)) if m.starts_with("TLS:")) {
        return TestReport { models, kind, why: String::new(), metrics: None, kind_confirmed: false };
    }
    let seen = engine::detect(&fetch);
    let recognised = seen.is_some();
    let (kind, why) = match (kind, seen) {
        (Some(k), Some((seen_k, why))) if seen_k == k => (Some(k), why),
        // #320 (5): the generic adapter's own reason ("... lss has no adapter for") contradicts a
        // named engine on the same line - say what was actually seen
        (Some(k), Some((EngineKind::OpenAi, _))) => (Some(k), format!("as you chose - it answers the OpenAI API, but none of {}'s own endpoints answered here; the metrics line says what lss can read", k.label())),
        (Some(k), Some((seen_k, _))) => (Some(k), format!("you chose {}, but it looks like {} - lss will read it as {}", k.label(), seen_k.label(), k.label())),
        (Some(k), None) => (Some(k), "as you chose (it has no fingerprint lss can check)".into()),
        (None, Some((k, why))) => (Some(k), why),
        (None, None) => (None, String::new()),
    };
    let metrics = kind.map(|k| {
        let scrape = engine::adapter_for(k, "", "").scrape(&fetch);
        let missing = scrape.not_reported().len();
        s::metrics_verdict(k, scrape.metrics.is_some(), missing, METRIC_FIELDS.len())
    });
    // an engine with no /v1/models but a native API that answered (old Ollama) is still connected
    let models = match (models, kind) {
        (Err(HttpOutcome::Status(404)), Some(k)) if k != EngineKind::OpenAi => {
            let m = engine::adapter_for(k, "", "").identity(&fetch).model;
            Ok(m.into_iter().collect())
        }
        (m, _) => m,
    };
    // card #325 (4): the engine gave no model list - a "metrics: none, a proxy?" verdict would
    // blame something for metrics that could not be judged. A positive verdict still stands.
    let metrics = if models.is_err() { metrics.filter(|m| m.ok) } else { metrics };
    // card #326: a kind is confirmed by what was READ - a model list, or a fingerprint
    let kind_confirmed = models.is_ok() || recognised;
    TestReport { models, kind, why, metrics, kind_confirmed }
}

fn print_report(io: &mut Io, url: &str, key: &str, r: &TestReport) {
    match &r.models {
        Ok(list) if list.is_empty() => io.say("   [ok] it answers - no model is loaded right now (lss shows it as soon as one is)"),
        Ok(list) => {
            let more = if list.len() > 3 { format!(" (+{} more)", list.len() - 3) } else { String::new() };
            io.say(&format!("   [ok] models: {}{more}", list.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        }
        Err(o) => io.say(&format!("   [!!] {}", s::explain(o, url, "/v1/models", !key.trim().is_empty()))),
    }
    if let (Some(k), false) = (r.kind, r.kind_confirmed) {
        // card #326: '[ok] engine: vLLM' after a refused certificate claimed a check that never ran
        io.say(&format!("   [--] engine: {} as you chose - not confirmed: nothing could be read from it yet", k.label()));
    } else if let Some(k) = r.kind {
        io.say(&format!("   [ok] engine: {}{}", k.label(), if r.why.is_empty() { String::new() } else { format!(" ({})", r.why) }));
    } else if r.connected() {
        io.say("   [--] engine: not recognised - lss reads it as a generic OpenAI-compatible server");
    }
    if let Some(m) = &r.metrics {
        io.say(&format!("   [{}] {}", if m.ok { "ok" } else { "!!" }, m.line));
    } else if !r.connected() {
        // card #325 (4): say it was not checked, rather than nothing (or a guess)
        io.say("   [--] metrics: not checked - nothing usable was read from the engine yet");
    }
}

// ------------------------------------------------------------------ what the wizard needs from outside

/// (address, the engine if known, API key) -> what the live test found
/// (the certificate choice, card #321) -> what the live test found
pub type TestFn<'a> = Box<dyn Fn(&str, Option<EngineKind>, &str, &crate::tls::TlsOptions) -> TestReport + 'a>;
/// (args for `cost-setup`, the rates.toml path, the terminal) -> its exit code
pub type CostFn<'a> = Box<dyn Fn(&[String], &Path, &mut Io) -> i32 + 'a>;

/// The parts of the wizard that touch the machine, swappable in tests.
pub struct Env<'a> {
    pub scan: Box<dyn Fn() -> (Vec<Found>, Vec<u16>) + 'a>,
    pub test: TestFn<'a>,
    /// run the cost step: (args for `cost-setup`, the rates.toml path) -> exit code
    pub cost: CostFn<'a>,
    /// restart the background collector(s); returns what to tell the person
    pub service: Box<dyn Fn(usize) -> Vec<String> + 'a>,
    pub now: i64,
}

impl Env<'static> {
    pub fn real(skip_ports: Vec<u16>, o: &Opts) -> Env<'static> {
        let (state, config) = (o.state_dir.clone(), o.config_dir.clone());
        Env {
            scan: Box::new(move || detect::scan(&skip_ports)),
            test: Box::new(live_test),
            cost: Box::new(run_cost_setup),
            service: Box::new(move |n| restart_service(n, &state, &config)),
            now: crate::unix_now(),
        }
    }
}

// ------------------------------------------------------------------ the wizard

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Stopped,
    WriteFailed,
    /// card #337: a --zip/--rate/--from-ip that was GIVEN was refused or failed - stopped at
    /// step 3, nothing written (going on would end 'Done' with cost tracking silently off)
    CostRefused,
}

impl Outcome {
    pub fn code(self) -> i32 {
        match self {
            Outcome::Done => 0,
            Outcome::WriteFailed | Outcome::CostRefused => 1,
            Outcome::Stopped => 3,
        }
    }
}

enum Pick {
    One(EngineChoice),
    All(Vec<EngineChoice>),
    Auto,
    Quit,
}

pub fn run(o: &Opts, io: &mut Io, env: &Env) -> Outcome {
    io.say("");
    io.say("LLM SERVER STATUS - setup");
    io.say("  1 find your LLM server   2 test the connection   3 electricity cost");
    io.say("  4 write the settings     5 start watching");
    io.say(&format!("  Nothing is written before step 3 (the electricity rate), and nothing that exists is replaced without asking. Settings: {}", o.config_dir.display()));
    if io.interactive {
        io.say("  Enter takes the answer in [brackets]. Ctrl-C stops at any time.");
    }

    // ---------------------------------------------------------- 1 + 2: find, choose, test
    let picked = match choose(o, io, env) {
        Pick::Quit => {
            io.say("Stopped. Nothing was written.");
            return Outcome::Stopped;
        }
        p => p,
    };
    let engines: Vec<Option<EngineChoice>> = match picked {
        Pick::One(c) => vec![Some(c)],
        Pick::All(list) => list.into_iter().map(Some).collect(),
        Pick::Auto => vec![None],
        Pick::Quit => unreachable!(),
    };

    // ---------------------------------------------------------- 3: cost
    io.say("");
    io.say("== 3/5  Electricity cost (optional)");
    let collector_path = o.config_dir.join("collector.toml");
    let existing = std::fs::read_to_string(&collector_path).ok();
    let home = std::env::var("HOME").unwrap_or_default();
    let rates_path = existing
        .as_deref()
        .and_then(|t| parse_config(t).ok())
        .map(|c| c.rates.path)
        .filter(|p| !p.trim().is_empty())
        .map(|p| PathBuf::from(lss_core::config::expand_home(&p, &home)))
        .unwrap_or_else(|| o.config_dir.join("rates.toml"));
    let mut cost_args: Vec<String> = o.cost.clone();
    let has_price = cost_args.iter().any(|a| matches!(a.as_str(), "--zip" | "--rate" | "--from-ip"));
    if cost_args.iter().any(|a| a == "--skip") && rates_path.exists() {
        io.say(&format!("   kept {} (it exists; --skip-cost changes nothing)", rates_path.display()));
    } else if rates_path.exists() && !o.force && !has_price {
        io.say(&format!("   kept {} (it exists)", rates_path.display()));
        if io.yes_no("   Change the electricity rate?", false) {
            cost_args.push("--force".into());
            (env.cost)(&cost_args, &rates_path, io);
        }
    } else {
        // a price given on the command line for an existing file: cost-setup itself asks before
        // replacing it (and keeps it when there is no one to ask), unless --force
        if o.force {
            cost_args.push("--force".into());
        }
        if !io.interactive && cost_args.iter().all(|a| a == "--force") {
            cost_args.insert(0, "--skip".into());
        }
        let code = (env.cost)(&cost_args, &rates_path, io);
        // card #337: a price flag the person GAVE that cost-setup refused (1: an ambiguous --rate,
        // a ZIP it does not cover, --from-ip with no network; 2: not a price) stops here, before
        // anything is written - exit 1, not 'Done' with cost tracking off. 3 (an existing
        // rates.toml kept) is not a failure.
        if has_price && matches!(code, 1 | 2) {
            io.say("");
            io.say(&format!("Stopped: `{}` did not set a price (see above), so cost tracking would be OFF. Nothing was written.", o.cost.join(" ")));
            io.say("   Run it again with a price that works (like --rate 0.31, or --rate 2.5c for cents),");
            io.say("   or with --skip-cost to set up without cost tracking (lss setup adds it later).");
            return Outcome::CostRefused;
        }
    }

    // ---------------------------------------------------------- 4: write
    io.say("");
    io.say(&format!("== 4/5  The settings in {}", o.config_dir.display()));
    if let Err(e) = std::fs::create_dir_all(&o.config_dir) {
        io.say(&format!("   could not create {}: {e}", o.config_dir.display()));
        return Outcome::WriteFailed;
    }
    // alert_cmd names the dispatcher install.sh installs - only when it is really there (a
    // command that does not exist is the same as no alerts, card #81)
    let notify = Some(o.prefix.join("lss-notify.sh")).filter(|p| p.is_file()).map(|p| p.display().to_string()).unwrap_or_default();
    let state = o.state_dir.display().to_string();
    let rates_line = format!("\n[rates]\npath = {}\n", s::toml_str(&rates_path.display().to_string()));
    let mut ports: Vec<(String, u16)> = Vec::new();
    for (i, choice) in engines.iter().enumerate() {
        let idx = i + 1;
        let path = o.config_dir.join(if idx == 1 { "collector.toml".to_string() } else { format!("collector-{idx}.toml") });
        let old = std::fs::read_to_string(&path).ok();
        let fresh_port = BASE_PORT + i as u16;
        let new_text = match &old {
            Some(t) => s::replace_engine(t, choice.as_ref()),
            None => {
                let mut t = s::new_collector_toml(choice.as_ref(), fresh_port, idx, &state, &notify);
                t.push_str(&rates_line);
                t
            }
        };
        if let Err(e) = parse_config(&new_text) {
            io.say(&format!("   not written: the new {} would not parse ({e}) - this is a bug in lss, please report it", path.display()));
            return Outcome::WriteFailed;
        }
        let secret = choice.as_ref().is_some_and(|c| !c.api_key.trim().is_empty());
        match write_config(io, o, &path, old.as_deref(), &new_text, secret, env.now, &describe_engine(choice.as_ref())) {
            Ok(_) => {}
            Err(e) => {
                io.say(&format!("   could not write {}: {e}", path.display()));
                return Outcome::WriteFailed;
            }
        }
        // the port lss must be pointed at: the file's own listen, whichever version was kept
        let on_disk = std::fs::read_to_string(&path).unwrap_or(new_text);
        let port = s::listen_port(&on_disk).unwrap_or(fresh_port);
        let name = choice.as_ref().map_or_else(|| "this machine".to_string(), |c| EngineKind::parse(&c.kind).map_or("engine", EngineKind::name).to_string());
        ports.push((name, port));
    }
    let lss_toml = if ports.len() == 1 {
        format!("# written by lss setup\nurl = \"http://127.0.0.1:{}\"\n", ports[0].1)
    } else {
        let mut t = String::from("# written by lss setup: one [[server]] per LLM server on this machine ([ and ] switch)\n");
        for (name, port) in &ports {
            t.push_str(&format!("\n[[server]]\nname = {}\nurl = \"http://127.0.0.1:{port}\"\n", s::toml_str(name)));
        }
        t
    };
    let lss_path = o.config_dir.join("lss.toml");
    let old_lss = std::fs::read_to_string(&lss_path).ok();
    if let Err(e) = write_config(io, o, &lss_path, old_lss.as_deref(), &lss_toml, false, env.now, "which collector the screen talks to") {
        io.say(&format!("   could not write {}: {e}", lss_path.display()));
        return Outcome::WriteFailed;
    }

    // ---------------------------------------------------------- 5: service
    io.say("");
    io.say("== 5/5  Watching");
    if o.no_service {
        io.say("   the installer sets up the background collector next");
    } else {
        for line in (env.service)(engines.len()) {
            io.say(&format!("   {line}"));
        }
        io.say("");
        io.say("Done. Type:  lss            the live screen (? shows the keys, q quits)");
        io.say("             lss status     the same as one page of text");
        io.say("             lss setup      run this again to change the engine");
    }
    Outcome::Done
}

fn describe_engine(c: Option<&EngineChoice>) -> String {
    match c {
        Some(c) => format!("{} at {}{}", EngineKind::parse(&c.kind).map_or("auto-recognised engine", EngineKind::label), c.url, if c.api_key.trim().is_empty() { "" } else { " (with an API key)" }),
        None => "auto: the collector finds the engine on this machine".into(),
    }
}

/// Write `path` unless it exists with other content and the person does not want it replaced.
/// Returns true when the file now holds `text`.
#[allow(clippy::too_many_arguments)]
fn write_config(io: &mut Io, o: &Opts, path: &Path, old: Option<&str>, text: &str, secret: bool, now: i64, what: &str) -> std::io::Result<bool> {
    if let Some(old) = old {
        if old == text {
            io.say(&format!("   unchanged {}", path.display()));
            if secret {
                chmod_600(path)?;
            }
            return Ok(true);
        }
        io.say(&format!("   {} exists. The new one: {what}; every other setting in it stays.", path.display()));
        let replace = o.force || (!o.yes && io.interactive && io.yes_no("   Replace it (the old one is kept as a .bak copy)?", false));
        if !replace && !o.force && !o.yes && !io.interactive {
            io.say("   Replace it? (no terminal: keeping it)");
        }
        if !replace {
            io.say(&format!("   kept {} as it is{}", path.display(), if o.yes { " (--force replaces it)" } else { "" }));
            return Ok(false);
        }
        let bak = path.with_extension(format!("toml.bak-{now}"));
        std::fs::copy(path, &bak)?;
        io.say(&format!("   the old one is in {}", bak.display()));
    }
    // write next to it, then swap: never a half-written config
    let tmp = path.with_extension("toml.new");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        if secret {
            chmod_600(&tmp)?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    io.say(&format!("   wrote {}{}", path.display(), if secret { " (readable by you only: it holds the API key)" } else { "" }));
    Ok(true)
}

fn chmod_600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Steps 1 and 2: what to watch, tested live.
/// The engine the existing collector.toml pins: (url, kind, api key) - what a re-run offers first.
pub fn configured_engine(config_dir: &Path) -> Option<(String, Option<EngineKind>, String)> {
    let cfg = parse_config(&std::fs::read_to_string(config_dir.join("collector.toml")).ok()?).ok()?;
    let (kind, url) = cfg.pinned_engine()?;
    Some((url, kind, cfg.pinned_api_key()))
}

fn choose(o: &Opts, io: &mut Io, env: &Env) -> Pick {
    let current = configured_engine(&o.config_dir);
    let kind_opt = o.kind.as_deref().filter(|k| *k != "auto").and_then(EngineKind::parse);
    let mut key = o.api_key.clone().unwrap_or_default();
    io.say("");
    io.say("== 1/5  Finding your LLM server");
    let mut url: Option<String> = None;
    if let Some(given) = &o.url {
        match s::normalize_url(given) {
            Ok(u) => {
                io.say(&format!("   the address you gave: {u}"));
                url = Some(u);
            }
            Err(e) => {
                io.say(&format!("   --url {given}: {e}"));
                if !io.interactive {
                    return Pick::Quit;
                }
            }
        }
    }
    // `kind` = what the person chose (written into the config); `hint` = what the scan saw
    // (only used to test). A server found on this machine is pinned by its ADDRESS with
    // kind = "auto": swapping the engine behind the same port later needs no edit.
    let mut kind = kind_opt;
    let mut hint: Option<EngineKind> = None;
    if url.is_none() {
        io.say("   asking the usual ports on this machine (SGLang, vLLM, llama.cpp, Ollama, LM Studio, TGI, any OpenAI-compatible server) ...");
        let (found, tried) = (env.scan)();
        if found.is_empty() {
            io.say(&format!("   nothing answered on this machine (asked ports {}).", short_ports(&tried)));
            io.say("   If the engine runs on another machine, or on an unusual port, type its address.");
            let a = match &current {
                // #320 (2): a re-run offers what is configured now
                Some((cur, _, _)) => io.ask("   Engine address - URL or host:port (Enter keeps the one configured now):", cur),
                None if io.interactive => io.ask("   Engine address - URL or host:port (e.g. localhost:8000 or 192.0.2.20:8000), or Enter to let the collector keep looking:", ""),
                None => String::new(),
            };
            if a.is_empty() {
                io.say("   auto: the collector looks every 15 s and picks the engine up as soon as it runs");
                return Pick::Auto;
            }
            let u = ask_address(io, &a);
            if current.as_ref().is_none_or(|(cur, _, _)| *cur != u) {
                kind = ask_kind(io, kind);
                if key.is_empty() {
                    key = io.ask_secret("   API key, if the engine was started with one (Enter for none):");
                }
            }
            url = Some(u);
        } else {
            for (i, f) in found.iter().enumerate() {
                let (sure, how) = if f.kind == EngineKind::OpenAi { ("likely", "answers the OpenAI API; engine not recognised") } else { ("sure", f.why.as_str()) };
                let model = f.identity.model.clone().unwrap_or_else(|| engine::NO_MODEL.to_string());
                io.say(&format!("   {}) {} at {} - {model}  [{sure}: {how}]", i + 1, f.kind.label(), f.url));
            }
            let many = found.len() > 1;
            if o.all && many {
                io.say("   --all: watching every one of them, one collector each");
                return all_of(&found, io, env);
            }
            // #320 (2): the configured engine is the default - its number when it was found, else
            // its own choice "c"
            let cur_in_found = current.as_ref().and_then(|(cur, _, _)| found.iter().position(|f| f.url == *cur));
            let cur_elsewhere = current.as_ref().filter(|_| cur_in_found.is_none()).map(|(cur, _, _)| cur.clone());
            if let Some(cur) = &cur_elsewhere {
                io.say(&format!("   c) the one configured now: {cur}"));
            }
            let default_pick = cur_in_found.map(|i| (i + 1).to_string()).or_else(|| cur_elsewhere.as_ref().map(|_| "c".to_string())).unwrap_or_else(|| "1".into());
            let choices = if many { format!("1-{}, a = all of them, o = another address, q = quit", found.len()) } else { "1, o = another address, q = quit".to_string() };
            loop {
                let a = io.ask(&format!("   Which one should lss watch? ({choices}{})", if cur_elsewhere.is_some() { ", c = the configured one" } else { "" }), &default_pick).to_ascii_lowercase();
                match a.as_str() {
                    "q" | "quit" => return Pick::Quit,
                    "c" if cur_elsewhere.is_some() => {
                        url = cur_elsewhere.clone();
                        break;
                    }
                    "a" | "all" if many => return all_of(&found, io, env),
                    "o" | "other" => {
                        let typed = io.ask("   Engine address - URL or host:port (e.g. localhost:8000 or 192.0.2.20:8000):", "");
                        url = Some(ask_address(io, &typed));
                        kind = ask_kind(io, kind);
                        if key.is_empty() {
                            key = io.ask_secret("   API key, if the engine was started with one (Enter for none):");
                        }
                        break;
                    }
                    n => match n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| found.get(i)) {
                        Some(f) => {
                            url = Some(f.url.clone());
                            hint = Some(f.kind);
                            break;
                        }
                        None => io.say(&format!("   '{a}' is not one of the choices")),
                    },
                }
            }
        }
    }
    let mut url = url.expect("an address was chosen above");
    // the configured engine keeps its own kind and key unless new ones were given (#320 (2): a
    // re-run must never silently drop the API key)
    if let Some((cur, cur_kind, cur_key)) = &current {
        if *cur == url {
            kind = kind.or(*cur_kind);
            if key.is_empty() {
                key = cur_key.clone();
            }
        }
    }
    // card #321 / #311 F2: the certificate settings already in collector.toml are used for the
    // test (a re-run after setting tls_ca_file or tls_verify = false must connect), and what the
    // person decides here is written back
    let mut tls = saved_tls(o, &url);
    if tls != crate::tls::TlsOptions::default() {
        io.say(&format!("   (from collector.toml: {})", tls_words(&tls)));
    }
    let mut ca_chosen = false;
    let choice = |kind: Option<EngineKind>, url: String, key: String, tls: &crate::tls::TlsOptions, ca_chosen: bool| EngineChoice {
        kind: kind.map_or("auto", EngineKind::name).into(),
        url,
        api_key: key,
        tls_verify: if tls.verify { None } else { Some(false) },
        // only a file chosen NOW is written; one already in collector.toml stays as it is
        tls_ca_file: if ca_chosen { tls.ca_file.clone() } else { String::new() },
    };

    // ---------------------------------------------------------- 2: the live test
    loop {
        io.say("");
        io.say(&format!("== 2/5  Testing {url}"));
        let r = (env.test)(&url, kind.or(hint), &key, &tls);
        print_report(io, &url, &key, &r);
        if r.connected() {
            return Pick::One(choice(kind, url, key, &tls, ca_chosen));
        }
        if !io.interactive {
            io.say("   keeping this address anyway: the collector keeps trying, and lss shows the engine DOWN until it answers");
            return Pick::One(choice(kind, url, key, &tls, ca_chosen));
        }
        // card #321: a certificate this machine does not trust - offer the two fixes #308 made
        if matches!(&r.models, Err(HttpOutcome::Other(m)) if m.starts_with("TLS:")) {
            io.say("   The engine answers over https, but this machine does not trust its certificate (usual for a box that made its own).");
            io.say("   [c] use a CA or certificate file: the path of the server's own certificate, or of the CA that signed it (PEM) - saved as tls_ca_file");
            io.say("   [i] trust this box insecurely: do not check its certificate (tls_verify = false for this engine) - anyone between you and it could pretend to be it");
            match io.ask("   c, i, or r = try again, e = change the address or key, k = keep it anyway, q = quit", "c").to_ascii_lowercase().as_str() {
                "c" => {
                    let typed = io.ask("   Path to the certificate (PEM):", "");
                    if typed.is_empty() {
                        continue;
                    }
                    let path = lss_core::config::expand_home(&typed, &std::env::var("HOME").unwrap_or_default());
                    match crate::tls::root_store(&path) {
                        Ok(_) => {
                            io.say(&format!("   -> tls_ca_file = {path}"));
                            tls.ca_file = path;
                            ca_chosen = true;
                        }
                        Err(e) => io.say(&format!("   [!!] {e}")),
                    }
                    continue;
                }
                "i" => {
                    io.say("   -> tls_verify = false for this engine: its certificate will NOT be checked (WARNING: the collector repeats this at every start)");
                    tls.verify = false;
                    continue;
                }
                "q" | "quit" => return Pick::Quit,
                "k" | "keep" => return Pick::One(choice(kind, url, key, &tls, ca_chosen)),
                "e" | "edit" => {
                    let typed = io.ask("   Engine address:", &url);
                    url = ask_address(io, &typed);
                    kind = ask_kind(io, kind);
                    continue;
                }
                _ => continue,
            }
        }
        if matches!(r.models, Err(HttpOutcome::Status(401 | 403))) {
            let k = io.ask_secret("   API key (Enter to go back to the choices):");
            if !k.is_empty() {
                key = k;
                continue;
            }
        }
        match io.ask("   r = try again, e = change the address or key, k = keep it anyway (the collector keeps trying), q = quit", "r").to_ascii_lowercase().as_str() {
            "q" | "quit" => return Pick::Quit,
            "k" | "keep" => return Pick::One(choice(kind, url, key, &tls, ca_chosen)),
            "e" | "edit" => {
                let typed = io.ask("   Engine address:", &url);
                url = ask_address(io, &typed);
                kind = ask_kind(io, kind);
                let k = io.ask_secret("   API key (Enter keeps the current one, '-' for none):");
                if k == "-" {
                    key.clear();
                } else if !k.is_empty() {
                    key = k;
                }
            }
            _ => {}
        }
    }
}

/// The certificate settings collector.toml already holds for `url` (card #311 F2): its
/// `tls_ca_file` always, and `tls_verify = false` only when it belongs to THIS engine (a re-run
/// that points lss at another engine must not inherit "do not check" silently).
fn saved_tls(o: &Opts, url: &str) -> crate::tls::TlsOptions {
    let mut t = crate::tls::TlsOptions::default();
    let Some(cfg) = std::fs::read_to_string(o.config_dir.join("collector.toml")).ok().and_then(|s| parse_config(&s).ok()) else {
        return t;
    };
    let ca = cfg.tls_ca_file.trim();
    if !ca.is_empty() {
        t.ca_file = lss_core::config::expand_home(ca, &std::env::var("HOME").unwrap_or_default());
    }
    let same = cfg.engines.first().filter(|e| e.url.trim_end_matches('/') == url.trim_end_matches('/'));
    t.verify = match same {
        Some(e) => e.tls_verify.unwrap_or(cfg.tls_verify),
        None => cfg.tls_verify,
    };
    t
}

fn tls_words(t: &crate::tls::TlsOptions) -> String {
    let mut w = Vec::new();
    if !t.ca_file.is_empty() {
        w.push(format!("tls_ca_file = {}", t.ca_file));
    }
    if !t.verify {
        w.push("tls_verify = false (the certificate is NOT checked)".to_string());
    }
    w.join(", ")
}

/// "All of them": each is tested and reported once (no retry menu - they all answered the scan
/// a moment ago), and each gets a collector of its own.
fn all_of(found: &[Found], io: &mut Io, env: &Env) -> Pick {
    let mut list = Vec::new();
    for f in found {
        io.say("");
        io.say(&format!("== 2/5  Testing {}", f.url));
        let r = (env.test)(&f.url, Some(f.kind), "", &crate::tls::TlsOptions::default());
        print_report(io, &f.url, "", &r);
        list.push(EngineChoice { kind: "auto".into(), url: f.url.clone(), ..Default::default() });
    }
    Pick::All(list)
}

/// Keep asking until the address is one lss can use.
fn ask_address(io: &mut Io, first: &str) -> String {
    let mut typed = first.to_string();
    loop {
        match s::normalize_url(&typed) {
            Ok(u) => {
                io.say(&format!("   -> {u}"));
                return u;
            }
            Err(e) => {
                io.say(&format!("   {e}"));
                if !io.interactive {
                    // no one to ask again: the collector's own default, looking on this machine
                    return "http://127.0.0.1:8000".into();
                }
                typed = io.ask("   Engine address:", "");
            }
        }
    }
}

const KIND_MENU: [(&str, Option<EngineKind>); 8] = [
    ("not sure - recognise it", None),
    ("SGLang", Some(EngineKind::Sglang)),
    ("vLLM", Some(EngineKind::Vllm)),
    ("llama.cpp (llama-server)", Some(EngineKind::LlamaCpp)),
    ("Ollama", Some(EngineKind::Ollama)),
    ("LM Studio", Some(EngineKind::LmStudio)),
    ("TGI (text-generation-inference)", Some(EngineKind::Tgi)),
    ("another OpenAI-compatible server", Some(EngineKind::OpenAi)),
];

fn ask_kind(io: &mut Io, current: Option<EngineKind>) -> Option<EngineKind> {
    if current.is_some() && !io.interactive {
        return current;
    }
    let menu: Vec<String> = KIND_MENU.iter().enumerate().map(|(i, (name, _))| format!("{i} {name}")).collect();
    io.say(&format!("   Which engine is it?  {}", menu.join(" · ")));
    let default = current.and_then(|k| KIND_MENU.iter().position(|(_, m)| *m == Some(k))).unwrap_or(0).to_string();
    loop {
        let a = io.ask("   Engine:", &default);
        if let Some(k) = a.parse::<usize>().ok().and_then(|i| KIND_MENU.get(i)) {
            return k.1;
        }
        if let Some(k) = EngineKind::parse(&a) {
            return Some(k);
        }
        io.say(&format!("   '{a}': type a number from 0 to {}", KIND_MENU.len() - 1));
    }
}

fn short_ports(p: &[u16]) -> String {
    let mut v: Vec<String> = p.iter().take(12).map(u16::to_string).collect();
    if p.len() > 12 {
        v.push(format!("+{} more", p.len() - 12));
    }
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

// ------------------------------------------------------------------ the real cost + service steps

/// Card #298's cost wizard, run as `<this binary> cost-setup ...`. A build without it (a
/// version skew while both cards land) gets the one question it can always answer: a $/kWh.
fn run_cost_setup(args: &[String], rates: &Path, io: &mut Io) -> i32 {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lss-collector"));
    let has_cost = std::process::Command::new(&exe).args(["cost-setup", "--help"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    if has_cost {
        // #298's contract: --out PATH, one of --zip/--from-ip/--rate/--skip, --force to replace;
        // no mode flag + a terminal = its own menu. Nobody to ask = skipped, never invented.
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("cost-setup").arg("--out").arg(rates).args(args);
        let has_mode = args.iter().any(|a| matches!(a.as_str(), "--zip" | "--rate" | "--skip" | "--from-ip"));
        if !io.interactive && !has_mode {
            cmd.arg("--skip");
        }
        return cmd.status().map_or(1, |s| s.code().unwrap_or(1));
    }
    manual_rate(args, rates, io)
}

/// The fallback cost step: a flat $/kWh typed (or given with --rate), or nothing.
pub fn manual_rate(args: &[String], rates: &Path, io: &mut Io) -> i32 {
    let given = args.windows(2).find(|w| w[0] == "--rate").map(|w| w[1].clone());
    if args.iter().any(|a| a == "--skip") && given.is_none() {
        io.say("   skipped: no cost is shown. Run lss setup again to add one.");
        return 3;
    }
    if rates.exists() && !args.iter().any(|a| a == "--force") {
        io.say(&format!("   kept {} (it exists)", rates.display()));
        return 0;
    }
    let answer = match given {
        Some(r) => r,
        None => io.ask("   Your electricity price in $/kWh (from your bill, e.g. 0.31), or Enter to skip:", ""),
    };
    if answer.is_empty() {
        io.say("   skipped: no cost is shown. Run lss setup again to add one.");
        return 3;
    }
    match answer.trim_start_matches('$').parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 && v < 10.0 => {
            let text = format!("# written by lss setup from the price you typed. This file is yours - never commit it.\nname = \"flat (typed in lss setup)\"\neffective_date = \"{}\"\n\n[plan]\nkind = \"flat\"\nusd_per_kwh = {v}\n", chrono::Local::now().format("%Y-%m-%d"));
            if let Some(dir) = rates.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(rates, text) {
                Ok(()) => {
                    io.say(&format!("   wrote {} (flat ${v}/kWh)", rates.display()));
                    0
                }
                Err(e) => {
                    io.say(&format!("   could not write {}: {e}", rates.display()));
                    1
                }
            }
        }
        _ => {
            io.say(&format!("   '{answer}' is not a price per kWh (a number like 0.31): skipped - run lss setup again to add one"));
            2
        }
    }
}

/// Restart the background collector(s) so the new settings take effect.
fn restart_service(n: usize, state_dir: &Path, config_dir: &Path) -> Vec<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lss-collector"));
    let home = std::env::var("HOME").unwrap_or_default();
    let quiet = |prog: &str, args: &[&str]| std::process::Command::new(prog).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    let mut lines = Vec::new();
    let mut restarted = 0;
    for i in 1..=n.max(1) {
        let unit = if i == 1 { "lss-collector".to_string() } else { format!("lss-collector-{i}") };
        let label = if i == 1 { "ai.lss.collector".to_string() } else { format!("ai.lss.collector-{i}") };
        let unit_file = format!("{home}/.config/systemd/user/{unit}.service");
        let plist = format!("{home}/Library/LaunchAgents/{label}.plist");
        if Path::new(&unit_file).exists() && quiet("systemctl", &["--user", "show-environment"]) {
            if quiet("systemctl", &["--user", "restart", &unit]) {
                lines.push(format!("restarted {unit} (systemd --user)"));
                restarted += 1;
            } else {
                lines.push(format!("could not restart {unit}: journalctl --user -u {unit} -n 30 says why"));
            }
        } else if Path::new(&plist).exists() {
            // SAFETY: getuid cannot fail
            let uid = unsafe { libc::getuid() };
            if quiet("launchctl", &["kickstart", "-k", &format!("gui/{uid}/{label}")]) {
                lines.push(format!("restarted {label} (launchd)"));
                restarted += 1;
            } else {
                lines.push(format!("could not restart {label}: launchctl load {plist}"));
            }
        } else if let Some(line) = restart_pidfile(i, state_dir, config_dir, &exe) {
            // #320 (3): no service manager here, but install.sh started one (its pid file)
            lines.push(line);
            restarted += 1;
        } else {
            lines.push(format!("no background service for collector {i} yet: the installer sets one up (curl ... install.sh | bash), or start it now:  lss-collector{} &", if i == 1 { String::new() } else { format!(" --config ~/.config/lss/collector-{i}.toml") }));
        }
    }
    if restarted > 0 {
        lines.push("the collector reads the new settings now; the screen fills within ~15 s".into());
    }
    lines
}

/// A collector install.sh started with nohup (no systemd/launchd here) and recorded in
/// `<state>/lss-collector[-N].pid`: stopped and started again with the new settings, the new pid
/// recorded. None = no such collector is running.
pub fn restart_pidfile(i: usize, state_dir: &Path, config_dir: &Path, exe: &Path) -> Option<String> {
    let name = if i == 1 { "lss-collector".to_string() } else { format!("lss-collector-{i}") };
    let pidfile = state_dir.join(format!("{name}.pid"));
    let old: i32 = std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()?;
    let alive = |pid: i32| pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
    // SAFETY: kill(pid, 0) only asks whether the process exists
    if !alive(old) {
        return None;
    }
    // only ever a collector: a recycled pid belonging to something else is left alone
    let args = std::process::Command::new("ps").args(["-p", &old.to_string(), "-o", "args="]).output().ok()?;
    if !String::from_utf8_lossy(&args.stdout).contains("lss-collector") {
        return None;
    }
    // SAFETY: SIGTERM to the collector the installer started (checked just above)
    unsafe { libc::kill(old, libc::SIGTERM) };
    for _ in 0..50 {
        if !alive(old) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let config = config_dir.join(if i == 1 { "collector.toml".to_string() } else { format!("collector-{i}.toml") });
    let log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir.join(format!("{name}.log"))).ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config").arg(&config).stdin(std::process::Stdio::null()).stdout(log.try_clone().ok()?).stderr(log);
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid in the child before exec: the collector outlives this wizard's terminal
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = cmd.spawn().ok()?;
    let _ = std::fs::write(&pidfile, format!("{}\n", child.id()));
    Some(format!("restarted {name} (pid {old} -> {}; started by the installer - no service manager here)", child.id()))
}

/// The entry point for `lss-collector setup ARGS`.
pub fn main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let o = match parse_args(args, &home) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("lss-collector setup: {e}\n{USAGE}");
            return 2;
        }
    };
    // never knock on our own collectors' ports while scanning
    let skip: Vec<u16> = std::fs::read_to_string(o.config_dir.join("collector.toml")).ok().and_then(|t| parse_config(&t).ok()).map(|c: Config| detect::own_ports(&c)).unwrap_or_else(|| vec![BASE_PORT]);
    let mut io = Io::terminal(o.yes);
    if !o.yes && !io.interactive {
        io.say("(no terminal to ask: taking every default, as with --yes)");
    }
    run(&o, &mut io, &Env::real(skip, &o)).code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A fake engine: `/v1/models` (optionally behind a key) and a SGLang-shaped `/metrics`.
    struct Fake {
        port: u16,
        handle: Option<std::thread::JoinHandle<()>>,
        auth_seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl Fake {
        fn start(key: Option<&'static str>, metrics: bool) -> Fake {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let port = server.server_addr().to_ip().unwrap().port();
            let auth_seen = Arc::new(Mutex::new(Vec::new()));
            let seen = auth_seen.clone();
            let handle = std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    let path = req.url().to_string();
                    if path == "/stop" {
                        let _ = req.respond(tiny_http::Response::from_string("bye"));
                        return;
                    }
                    let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                    seen.lock().unwrap().push(auth.clone());
                    if let Some(k) = key {
                        if auth.as_deref() != Some(&format!("Bearer {k}")) {
                            let _ = req.respond(tiny_http::Response::from_string("{\"error\":\"no key\"}").with_status_code(401));
                            continue;
                        }
                    }
                    let (code, body) = match path.as_str() {
                        "/v1/models" => (200, "{\"data\":[{\"id\":\"fake-model-7b\"}]}".to_string()),
                        "/metrics" if metrics => (200, "sglang:num_running_reqs{} 0.0\nsglang:num_queue_reqs{} 0.0\nsglang:generation_tokens_total{} 5\n".to_string()),
                        _ => (404, "not found".to_string()),
                    };
                    let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
                }
            });
            Fake { port, handle: Some(handle), auth_seen }
        }
        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = ureq::get(&format!("http://127.0.0.1:{}/stop", self.port)).call();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }


    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lss-setup-{name}-{}-{}", std::process::id(), free_port()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(dir: &Path) -> Opts {
        Opts { config_dir: dir.join("cfg"), state_dir: dir.join("state"), prefix: dir.join("bin"), ..Default::default() }
    }

    /// Run the wizard with scripted answers; returns (outcome, everything it printed, cost calls).
    fn drive(o: &Opts, answers: &str, found: Vec<Found>) -> (Outcome, String, Vec<Vec<String>>) {
        drive_with(o, answers, found, Box::new(manual_rate))
    }

    /// `drive` with the cost step replaced (cost-setup's own exit codes: 1 refused/failed, 3 kept)
    fn drive_with(o: &Opts, answers: &str, found: Vec<Found>, cost: CostFn<'static>) -> (Outcome, String, Vec<Vec<String>>) {
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct W(Arc<Mutex<Vec<u8>>>);
        impl Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let cost_calls = Arc::new(Mutex::new(Vec::new()));
        let cc = cost_calls.clone();
        let env = Env {
            scan: Box::new(move || (found.clone(), vec![8000, 30000])),
            test: Box::new(live_test),
            cost: Box::new(move |args: &[String], rates: &Path, io: &mut Io| {
                cc.lock().unwrap().push(args.to_vec());
                cost(args, rates, io)
            }),
            service: Box::new(|_| vec!["(test: no service)".into()]),
            now: 1_700_000_000,
        };
        let interactive = !o.yes;
        let mut io = Io::new(Box::new(std::io::Cursor::new(answers.as_bytes().to_vec())), Box::new(W(out.clone())), interactive);
        let outcome = run(o, &mut io, &env);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let calls = cost_calls.lock().unwrap().clone();
        (outcome, text, calls)
    }

    fn found(kind: EngineKind, url: &str) -> Found {
        Found { kind, url: url.into(), why: "/metrics carries `sglang:` series".into(), identity: engine::EngineIdentity { model: Some("fake-model-7b".into()), ..Default::default() } }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    #[test]
    fn detect_confirm_test_cost_write_the_happy_path() {
        let eng = Fake::start(None, true);
        let d = tmp("happy");
        let o = opts(&d);
        // 1 = the engine found, then the price
        let (outcome, text, _) = drive(&o, "1\n0.31\n", vec![found(EngineKind::Sglang, &eng.url())]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("[sure: /metrics carries `sglang:` series]"), "shows how sure it is: {text}");
        assert!(text.contains("[ok] models: fake-model-7b"), "{text}");
        assert!(text.contains("[ok] engine: SGLang"), "{text}");
        assert!(text.contains("[ok] metrics: SGLang publishes"), "{text}");
        let cfg = parse_config(&read(&o.config_dir.join("collector.toml"))).unwrap();
        assert_eq!(cfg.pinned_engine(), Some((None, eng.url())), "found here: pinned by address, the engine recognised when it answers");
        assert_eq!(cfg.rates.path, o.config_dir.join("rates.toml").display().to_string(), "the collector reads the rates the wizard wrote");
        assert!(read(&o.config_dir.join("rates.toml")).contains("usd_per_kwh = 0.31"));
        assert_eq!(read(&o.config_dir.join("lss.toml")), "# written by lss setup\nurl = \"http://127.0.0.1:8099\"\n");
        assert!(text.contains("Done. Type:  lss"), "{text}");
    }

    #[test]
    fn a_lan_address_with_a_key_the_401_is_explained_then_the_key_is_asked_and_it_works() {
        let eng = Fake::start(Some("test-lan-value-0001"), true);
        let d = tmp("key");
        let o = opts(&d);
        // nothing found here -> type the address, engine 2 = vLLM? no: 0 = recognise it, no key at first,
        // the test says 401 -> the key -> connected; then skip the cost
        let answers = format!("{}\n0\n\ntest-lan-value-0001\n\n", eng.url().trim_start_matches("http://"));
        let (outcome, text, _) = drive(&o, &answers, vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("wants an API key (HTTP 401)"), "{text}");
        assert!(text.contains("[ok] models: fake-model-7b"), "{text}");
        let path = o.config_dir.join("collector.toml");
        let cfg = parse_config(&read(&path)).unwrap();
        assert_eq!(cfg.pinned_api_key(), "test-lan-value-0001");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "the key is readable by its owner only");
        assert!(!text.contains("test-lan-value-0001"), "the key is never printed: {text}");
        assert!(eng.auth_seen.lock().unwrap().iter().any(|a| a.as_deref() == Some("Bearer test-lan-value-0001")));
        assert!(!o.config_dir.join("rates.toml").exists(), "cost skipped: nothing invented");
    }

    #[test]
    fn a_cost_flag_that_is_refused_or_fails_stops_setup_before_anything_is_written() {
        // card #337: `--yes --rate 2.5` (refused by #332 as ambiguous), a ZIP it does not cover,
        // --from-ip with no network: cost-setup exits 1 (a bad price 2) - setup used to go on,
        // write collector.toml + lss.toml, print 'Done' and exit 0 with cost tracking OFF.
        let eng = Fake::start(None, true);
        for (flag, val, code, why) in [("--rate", "2.5", 1, "--rate 2.5: reads as 2.5 cents. If that is right, pass --rate 2.5c"), ("--zip", "00000", 1, "ZIP 00000 is not in the table"), ("--from-ip", "", 1, "no network"), ("--rate", "abc", 2, "'abc' is not a price")] {
            let d = tmp("cost-refused");
            let mut o = opts(&d);
            (o.yes, o.no_service, o.url) = (true, true, Some(eng.url()));
            o.cost = if val.is_empty() { vec![flag.into()] } else { vec![flag.into(), val.into()] };
            let (outcome, text, calls) = drive_with(&o, "", vec![], Box::new(move |_: &[String], _: &Path, io: &mut Io| {
                io.say(&format!("cost-setup: {why}"));
                code
            }));
            assert_eq!(calls.len(), 1, "{flag}: the cost step ran: {text}");
            assert_eq!((outcome == Outcome::Done, outcome.code()), (false, 1), "{flag} {val}: a refused/failed cost flag must stop setup with exit 1: {text}");
            for f in ["collector.toml", "lss.toml", "rates.toml"] {
                assert!(!o.config_dir.join(f).exists(), "{flag}: {f} written although the cost flag failed: {text}");
            }
            assert!(!text.contains("Done."), "{flag}: {text}");
            assert!(!text.contains("== 4/5"), "{flag}: stops at step 3: {text}");
            assert!(text.contains(why) && text.contains("Nothing was written") && text.contains("--skip-cost"), "{flag}: {text}");
            assert!(text.contains(&format!("{flag}{}{val}", if val.is_empty() { "" } else { " " })), "names the flag: {text}");
        }
        // an existing rates.toml KEPT (cost-setup exit 3, nobody to ask) is not a failure; nor is --skip-cost
        for (args, code) in [(vec!["--rate".to_string(), "0.31".into()], 3), (vec!["--skip".to_string()], 0)] {
            let d = tmp("cost-kept");
            let mut o = opts(&d);
            (o.yes, o.no_service, o.url, o.cost) = (true, true, Some(eng.url()), args.clone());
            let (outcome, text, _) = drive_with(&o, "", vec![], Box::new(move |_: &[String], _: &Path, _: &mut Io| code));
            assert_eq!(outcome, Outcome::Done, "{args:?}: {text}");
            assert!(o.config_dir.join("collector.toml").exists(), "{text}");
        }
    }

    #[test]
    fn the_help_says_yes_keeps_the_configured_engine() {
        // card #325 (1): #320 made a re-run with --yes keep the configured engine
        let at = USAGE.find("  --yes").unwrap();
        let text = USAGE[at..].split_whitespace().take(16).collect::<Vec<_>>().join(" ");
        assert!(text.contains("--url, else the engine already configured, else the first found; keep existing files"), "{text}");
    }

    #[test]
    fn an_engine_that_answers_nothing_usable_gets_no_metrics_verdict_and_no_ok() {
        // card #325 (4): every path answers HTTP 500 - nothing was read. It used to print
        // '[ok] engine: vLLM (as you chose ...)' and 'metrics: none. vLLM serves /metrics by default
        // - something (a proxy? ...)' about an engine it never heard from.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let _ = req.respond(tiny_http::Response::from_string("broken").with_status_code(500));
            }
        });
        let d = tmp("broken-500");
        let o = Opts { url: Some(url), kind: Some("vllm".into()), yes: true, no_service: true, ..opts(&d) };
        let (_, text, _) = drive(&o, "", vec![]);
        assert!(!text.contains("proxy") && !text.contains("metrics: none"), "no verdict about metrics it never read: {text}");
        assert!(text.contains("[--] metrics: not checked"), "{text}");
        assert!(!text.contains("[ok] engine") && text.contains("[--] engine: vLLM as you chose - not confirmed"), "{text}");
    }

    #[test]
    fn a_refused_engine_is_never_reported_ok_as_the_kind_it_was_given() {
        // card #326 (rv-311, black-box on dd18a2b): `setup --url https://... --kind vllm` against a
        // box whose certificate is refused printed '[ok] engine: vLLM' - the kind came from
        // --kind, nothing had been read. It must say the kind is the one given, unconfirmed.
        let p = crate::tls::tests::pki();
        let url = crate::tls::tests::tls_engine(&p, 64);
        let d = tmp("tls-kind");
        let o = Opts { url: Some(url.clone()), kind: Some("vllm".into()), ..opts(&d) };
        // [certificate refused] i = trust it insecurely (so the run ends), then skip the cost
        let (outcome, text, _) = drive(&o, "i\n\n", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        let refused = &text[..text.find("[c] use a CA").expect("the certificate choices are offered")];
        assert!(refused.contains("TLS: the server's certificate was not accepted"), "{refused}");
        assert!(!refused.contains("[ok] engine"), "nothing was read, so no engine is 'ok': {refused}");
        assert!(refused.contains("[--] engine: vLLM as you chose - not confirmed"), "{refused}");
        // after the retest connects, the engine line is a real one again
        let after = &text[text.find("[c] use a CA").unwrap()..];
        assert!(after.contains("[ok] engine: vLLM"), "{after}");
        // and a plain refusal (nothing listening) says the same
        let r = live_test("http://127.0.0.1:9", Some(EngineKind::Vllm), "", &crate::tls::TlsOptions::default());
        assert!(!r.connected() && !r.kind_confirmed && r.kind == Some(EngineKind::Vllm));
    }

    #[test]
    fn an_https_engine_with_its_own_certificate_is_trusted_by_its_pem_or_by_not_checking() {
        // card #321: a LAN box behind https with a certificate it made itself. The first test
        // fails ON THE CERTIFICATE, the wizard offers the two fixes #308 built, and each one is
        // tested live again and written where the collector reads it.
        let p = crate::tls::tests::pki();

        // c = the path of its CA (PEM) -> retest connects -> top-level tls_ca_file
        let url = crate::tls::tests::tls_engine(&p, 64);
        let d = tmp("tls-ca");
        let o = opts(&d);
        let pem = d.join("box-ca.pem");
        std::fs::write(&pem, &p.ca_pem).unwrap();
        // address, 0 = recognise it, no key, [certificate refused] c, a bad path first, c, the PEM, skip the cost
        let answers = format!("{url}\n0\n\nc\n{}\nc\n{}\n\n", d.join("nope.pem").display(), pem.display());
        let (outcome, text, _) = drive(&o, &answers, vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("TLS: the server's certificate was not accepted"), "the refusal is said in words: {text}");
        assert!(text.contains("[c] use a CA or certificate file") && text.contains("[i] trust this box insecurely"), "both fixes are offered: {text}");
        assert!(text.contains("[!!] tls_ca_file"), "a path that is not a PEM is refused and asked again: {text}");
        let refused = &text[text.find("== 2/5").unwrap()..text.find("[c] use a CA").unwrap()];
        assert!(!refused.contains("metrics: none") && !refused.contains("proxy") && refused.contains("[--] metrics: not checked"), "#311 F3 / #325: a refused certificate read no metrics - no verdict, and it says they were not checked: {refused}");
        assert!(text.contains("[ok] models: tls-fake"), "the retest connects: {text}");
        let cfg = parse_config(&read(&o.config_dir.join("collector.toml"))).unwrap();
        assert_eq!(cfg.tls_ca_file, pem.display().to_string());
        assert_eq!((cfg.engines[0].url.as_str(), cfg.engines[0].tls_verify), (url.as_str(), None), "https kept, certificate still checked");

        // i = trust it insecurely -> retest connects -> tls_verify = false in the engine's own block
        let url = crate::tls::tests::tls_engine(&p, 64);
        let d = tmp("tls-v");
        let o = opts(&d);
        let (outcome, text, _) = drive(&o, &format!("{url}\n0\n\ni\n\n"), vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("will NOT be checked") && text.contains("[ok] models: tls-fake"), "{text}");
        let written = read(&o.config_dir.join("collector.toml"));
        let cfg = parse_config(&written).unwrap();
        assert_eq!((cfg.engines[0].tls_verify, cfg.engine_tls_verify(), cfg.tls_ca_file.as_str()), (Some(false), false, ""), "{written}");
    }

    #[test]
    fn a_re_run_uses_the_certificate_settings_already_in_collector_toml() {
        // card #311 F2 (verifier): with tls_ca_file (or tls_verify = false) already in
        // collector.toml, `lss setup --url https://...` still said "certificate was not
        // accepted" - the wizard ran before the collector's TLS was configured and ignored both.
        let p = crate::tls::tests::pki();
        let url = crate::tls::tests::tls_engine(&p, 64);
        let d = tmp("tls-rerun");
        let mut o = opts(&d);
        std::fs::create_dir_all(&o.config_dir).unwrap();
        let pem = d.join("box-ca.pem");
        std::fs::write(&pem, &p.ca_pem).unwrap();
        let path = o.config_dir.join("collector.toml");
        std::fs::write(&path, format!("listen = [\"127.0.0.1:8099\"]\ntls_ca_file = \"{}\"\n\n[[engine]]\nkind = \"auto\"\nurl = \"{url}\"\n", pem.display())).unwrap();
        o.url = Some(url.clone());
        o.yes = true;
        o.force = true;
        o.cost = vec!["--skip".into()];
        let (outcome, text, _) = drive(&o, "", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("(from collector.toml: tls_ca_file = "), "says which saved setting it used: {text}");
        assert!(text.contains("[ok] models: tls-fake") && !text.contains("not accepted"), "the saved CA is used for the test: {text}");
        let cfg = parse_config(&read(&path)).unwrap();
        assert_eq!((cfg.tls_ca_file, cfg.engines[0].url.clone()), (pem.display().to_string(), url.clone()), "the saved tls_ca_file is kept");

        // tls_verify = false for THIS engine is honoured and kept; for another engine it is not inherited
        let url2 = crate::tls::tests::tls_engine(&p, 64);
        std::fs::write(&path, format!("listen = [\"127.0.0.1:8099\"]\n\n[[engine]]\nkind = \"auto\"\nurl = \"{url2}\"\ntls_verify = false\n")).unwrap();
        o.url = Some(url2.clone());
        let (outcome, text, _) = drive(&o, "", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("[ok] models: tls-fake"), "{text}");
        assert_eq!(parse_config(&read(&path)).unwrap().engines[0].tls_verify, Some(false), "still this engine's choice");
        let url3 = crate::tls::tests::tls_engine(&p, 64);
        o.url = Some(url3);
        let (_, text, _) = drive(&o, "", vec![]);
        assert!(text.contains("not accepted"), "another engine does not inherit 'do not check': {text}");
    }

    #[test]
    fn nothing_listening_is_explained_and_q_writes_nothing() {
        let d = tmp("refused");
        let o = opts(&d);
        let held = crate::test_ports::refusing_port();
        let port = held.port;
        let (outcome, text, _) = drive(&o, &format!("localhost:{port}\n2\n\nq\n"), vec![]);
        assert_eq!(outcome, Outcome::Stopped, "{text}");
        assert!(text.contains("Nothing is listening at http://127.0.0.1:"), "{text}");
        assert!(!o.config_dir.exists(), "stopped before step 4: nothing written");
    }

    #[test]
    fn an_engine_without_metrics_says_how_to_turn_them_on() {
        let eng = Fake::start(None, false);
        let d = tmp("nometrics");
        let mut o = opts(&d);
        o.url = Some(eng.url());
        o.kind = Some("sglang".into());
        o.cost = vec!["--skip".into()];
        let (outcome, text, _) = drive(&o, "", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("[!!] metrics: none. SGLang publishes them only when started with --enable-metrics"), "{text}");
    }

    #[test]
    fn a_404_is_explained_and_a_bad_address_is_asked_again() {
        // something that answers http, but is no LLM engine: every path 404s
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let h = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let stop = req.url() == "/stop";
                let _ = req.respond(tiny_http::Response::from_string("nope").with_status_code(404));
                if stop {
                    return;
                }
            }
        });
        let d = tmp("404");
        let o = opts(&d);
        let (outcome, text, _) = drive(&o, &format!("my host:1\n127.0.0.1:{port}\n7\n\nq\n"), vec![]);
        let _ = ureq::get(&format!("http://127.0.0.1:{port}/stop")).call();
        let _ = h.join();
        assert_eq!(outcome, Outcome::Stopped, "{text}");
        assert!(text.contains("has a space in it"), "{text}");
        assert!(text.contains("has no /v1/models (HTTP 404)"), "{text}");
    }

    #[test]
    fn yes_writes_the_first_engine_found_and_never_replaces_a_file() {
        let eng = Fake::start(None, true);
        let d = tmp("yes");
        let mut o = opts(&d);
        o.yes = true;
        let (outcome, text, cost) = drive(&o, "", vec![found(EngineKind::Sglang, &eng.url())]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert_eq!(cost, vec![vec!["--skip".to_string()]], "--yes with no cost flag: the cost step is skipped, never invented");
        let path = o.config_dir.join("collector.toml");
        let first = read(&path);
        // run again against another engine: with --yes and no --url the CONFIGURED one stays the
        // choice (#320); pointed at another one, --yes still keeps the existing file
        let eng2 = Fake::start(None, true);
        let (_, same, _) = drive(&o, "", vec![found(EngineKind::Sglang, &eng2.url())]);
        assert!(same.contains(&format!("c) the one configured now: {}", eng.url())) && same.contains("unchanged"), "{same}");
        o.url = Some(eng2.url());
        let (_, text2, _) = drive(&o, "", vec![found(EngineKind::Sglang, &eng2.url())]);
        assert_eq!(read(&path), first, "{text2}");
        assert!(text2.contains("kept") && text2.contains("--force replaces it"), "{text2}");
        // with --force it is replaced, and the old one is kept as a backup
        o.force = true;
        let (_, text3, _) = drive(&o, "", vec![found(EngineKind::Sglang, &eng2.url())]);
        assert!(read(&path).contains(&eng2.url()), "{text3}");
        assert_eq!(read(&path.with_extension("toml.bak-1700000000")), first);
    }

    #[test]
    fn re_running_interactively_asks_before_replacing_and_keeps_the_other_settings() {
        let eng = Fake::start(None, true);
        let d = tmp("rerun");
        let o = opts(&d);
        std::fs::create_dir_all(&o.config_dir).unwrap();
        let path = o.config_dir.join("collector.toml");
        let rates = o.config_dir.join("my-rates.toml");
        std::fs::write(&path, format!("host = \"my-box\"\nlisten = [\"127.0.0.1:9100\"]\n\n[[engine]]\nkind = \"ollama\"\nurl = \"http://127.0.0.1:11434\"\n\n[rates]\npath = \"{}\"\n", rates.display())).unwrap();
        // "n": keep it
        let (_, text, _) = drive(&o, &format!("o\n{}\n1\n\n\nn\n", eng.url()), vec![found(EngineKind::Ollama, "http://127.0.0.1:11434")]);
        assert!(read(&path).contains("11434"), "{text}");
        assert!(text.contains("Replace it"), "{text}");
        // "y": replaced; host and listen stay, lss.toml points at the file's own port
        let (_, text, _) = drive(&o, &format!("o\n{}\n1\n\n\ny\n", eng.url()), vec![found(EngineKind::Ollama, "http://127.0.0.1:11434")]);
        let cfg = parse_config(&read(&path)).unwrap();
        assert_eq!((cfg.host.as_str(), cfg.listen.clone()), ("my-box", vec!["127.0.0.1:9100".to_string()]), "{text}");
        assert_eq!(cfg.pinned_engine(), Some((Some(EngineKind::Sglang), eng.url())));
        assert!(read(&o.config_dir.join("lss.toml")).contains("127.0.0.1:9100"));
    }

    #[test]
    fn several_engines_all_gets_one_collector_each() {
        let e1 = Fake::start(None, true);
        let e2 = Fake::start(None, true);
        let d = tmp("all");
        let o = opts(&d);
        let (outcome, text, _) = drive(&o, "a\n\n", vec![found(EngineKind::Sglang, &e1.url()), found(EngineKind::Vllm, &e2.url())]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        let c2 = parse_config(&read(&o.config_dir.join("collector-2.toml"))).unwrap();
        assert_eq!((c2.listen.clone(), c2.pinned_engine()), (vec!["127.0.0.1:8100".to_string()], Some((None, e2.url()))));
        let lss = lss_core::config::parse_client_config(&read(&o.config_dir.join("lss.toml"))).unwrap();
        assert_eq!(lss.server.iter().map(|s| s.url.as_str()).collect::<Vec<_>>(), vec!["http://127.0.0.1:8099", "http://127.0.0.1:8100"]);
    }

    #[test]
    fn nothing_found_and_enter_means_auto() {
        let d = tmp("auto");
        let o = opts(&d);
        let (outcome, text, _) = drive(&o, "\n\n", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(parse_config(&read(&o.config_dir.join("collector.toml"))).unwrap().engine_is_auto());
        assert!(text.contains("nothing answered on this machine (asked ports 8000, 30000)"), "{text}");
    }

    #[test]
    fn the_input_ending_early_takes_the_defaults_instead_of_looping() {
        let d = tmp("eof");
        let o = opts(&d);
        let held = crate::test_ports::refusing_port();
        let port = held.port;
        // an address that refuses, then the input ends: kept anyway, no endless retry
        let (outcome, text, _) = drive(&o, &format!("127.0.0.1:{port}\n"), vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains("keeping this address anyway"), "{text}");
    }

    #[test]
    fn arguments_parse_and_bad_ones_are_refused() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "/h");
        let o = a(&["--url", "box:8000", "--kind", "vllm", "--zip", "94103", "--yes", "--no-service"]).unwrap();
        assert_eq!((o.url.as_deref(), o.kind.as_deref(), o.cost.clone(), o.yes, o.no_service), (Some("box:8000"), Some("vllm"), vec!["--zip".to_string(), "94103".to_string()], true, true));
        assert!(a(&["--kind", "gpt"]).unwrap_err().contains("--kind 'gpt'"));
        assert!(a(&["--url"]).unwrap_err().contains("needs a value"));
        assert!(a(&["--bogus"]).unwrap_err().contains("unknown argument"));
    }

    /// The collector's own requests (poll, probe, bench, detect all go through `authed`): the key
    /// reaches the engine it belongs to and no other server.
    #[test]
    fn the_collector_sends_the_key_to_its_engine_and_to_nothing_else() {
        let eng = Fake::start(Some("test-engine-value-42"), true);
        let other = Fake::start(None, true);
        crate::collect::set_engine_auth(&eng.url(), "test-engine-value-42");
        let agent = ureq::AgentBuilder::new().build();
        assert!(crate::collect::http_get(&agent, &format!("{}/v1/models", eng.url())).is_ok_and(|b| b.contains("fake-model-7b")), "the engine accepted the collector's request");
        assert!(crate::collect::http_get(&agent, &format!("{}/v1/models", other.url())).is_ok());
        assert_eq!(*other.auth_seen.lock().unwrap(), vec![None], "another server never sees the key");
        let fetch = detect::fetcher(&eng.url(), Duration::from_secs(2));
        assert!(fetch("/metrics").is_ok(), "--detect of the pinned engine carries it too");
    }

    #[test]
    fn the_engine_key_goes_to_the_engine_only() {
        assert!(crate::collect::is_under("http://h:8000/v1/models", "http://h:8000"));
        assert!(crate::collect::is_under("http://h:8000", "http://h:8000/"));
        assert!(!crate::collect::is_under("http://h:80001/v1/models", "http://h:8000"), "a longer port is another server");
        assert!(!crate::collect::is_under("http://gate:8096/v1/chat/completions", "http://h:8000"));
        assert!(!crate::collect::is_under("http://h:8000/x", ""));
    }

    // ---------------------------------------------------------- card #320: polish from a stranger run
    fn pin(o: &Opts, url: &str, key: &str) {
        std::fs::create_dir_all(&o.config_dir).unwrap();
        let key_line = if key.is_empty() { String::new() } else { format!("api_key = \"{key}\"\n") };
        std::fs::write(o.config_dir.join("collector.toml"), format!("listen = [\"127.0.0.1:8099\"]\n\n[[engine]]\nkind = \"sglang\"\nurl = \"{url}\"\n{key_line}")).unwrap();
    }

    #[test]
    fn with_no_terminal_a_replace_question_says_it_keeps_the_file_in_words() {
        let eng = Fake::start(None, true);
        let d = tmp("320-1");
        let mut o = opts(&d);
        pin(&o, "http://127.0.0.1:9", "");
        o.url = Some(eng.url());
        o.cost = vec!["--skip".into()];
        // not --yes, but no terminal: Io is non-interactive
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct W(Arc<Mutex<Vec<u8>>>);
        impl Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(b); Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let env = Env { scan: Box::new(|| (vec![], vec![])), test: Box::new(live_test), cost: Box::new(manual_rate), service: Box::new(|_| vec![]), now: 1 };
        let mut io = Io::new(Box::new(std::io::empty()), Box::new(W(out.clone())), false);
        assert_eq!(run(&o, &mut io, &env), Outcome::Done);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert!(text.contains("Replace it? (no terminal: keeping it)"), "{text}");
        assert!(!text.contains("y/N] y/N") && !text.contains("[y/N] y/N"), "{text}");
        assert!(read(&o.config_dir.join("collector.toml")).contains("127.0.0.1:9"), "kept");
    }

    #[test]
    fn a_re_run_offers_the_configured_engine_and_keeps_its_api_key() {
        let eng = Fake::start(Some("test-keep-value-320"), true);
        let d = tmp("320-2");
        let o = opts(&d);
        pin(&o, &eng.url(), "test-keep-value-320");
        // nothing found: Enter keeps the configured address; the key is NOT asked again and NOT lost
        let (outcome, text, _) = drive(&o, "\n\n", vec![]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains(&format!("(Enter keeps the one configured now): [{}]", eng.url())), "{text}");
        assert!(!text.contains("Enter to let the collector keep looking"), "{text}");
        assert!(text.contains("[ok] models: fake-model-7b"), "the stored key was used for the test: {text}");
        let cfg = parse_config(&read(&o.config_dir.join("collector.toml"))).unwrap();
        assert_eq!((cfg.pinned_api_key(), cfg.pinned_engine()), ("test-keep-value-320".to_string(), Some((Some(EngineKind::Sglang), eng.url()))));
        // something else found: the configured one is choice "c", and the default
        let (outcome, text, _) = drive(&o, "\n\n", vec![found(EngineKind::Ollama, "http://127.0.0.1:11434")]);
        assert_eq!(outcome, Outcome::Done, "{text}");
        assert!(text.contains(&format!("c) the one configured now: {}", eng.url())) && text.contains("c = the configured one) [c]"), "{text}");
        assert_eq!(parse_config(&read(&o.config_dir.join("collector.toml"))).unwrap().pinned_api_key(), "test-keep-value-320");
    }

    #[test]
    fn the_header_says_writing_starts_at_the_electricity_step() {
        let d = tmp("320-4");
        let (_, text, _) = drive(&opts(&d), "\n\n", vec![]);
        assert!(text.contains("Nothing is written before step 3 (the electricity rate)"), "{text}");
        assert!(!text.contains("before step 4"), "{text}");
    }

    #[test]
    fn a_named_engine_that_only_shows_the_openai_api_is_described_without_contradiction() {
        // /v1/models only: no vLLM /metrics
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let h = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let stop = req.url() == "/stop";
                let models = req.url() == "/v1/models";
                let body = if models { "{\"data\":[{\"id\":\"m\"}]}" } else { "no" };
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(if models { 200 } else { 404 }));
                if stop {
                    return;
                }
            }
        });
        let r = live_test(&format!("http://127.0.0.1:{port}"), Some(EngineKind::Vllm), "", &crate::tls::TlsOptions::default());
        let _ = ureq::get(&format!("http://127.0.0.1:{port}/stop")).call();
        let _ = h.join();
        assert_eq!(r.kind, Some(EngineKind::Vllm));
        assert!(!r.why.contains("no adapter"), "{}", r.why);
        assert!(r.why.contains("as you chose") && r.why.contains("vLLM"), "{}", r.why);
    }

    #[test]
    fn an_installer_started_collector_is_restarted_not_reported_missing() {
        let d = tmp("320-3");
        let (state, config) = (d.join("state"), d.join("cfg"));
        std::fs::create_dir_all(&state).unwrap();
        // a stand-in collector: a script NAMED lss-collector that just waits
        let exe = d.join("lss-collector");
        // (a loop, not `exec sleep`: exec would rename the process to "sleep", and a pid whose
        // command line is not a collector is - rightly - never touched)
        crate::exec_file::write_exec(&exe, "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n");
        let old = std::process::Command::new(&exe).args(["--config", "x"]).spawn();
        let old = match old { Ok(c) => c, Err(e) => panic!("{e}") };
        std::fs::write(state.join("lss-collector.pid"), old.id().to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let line = restart_pidfile(1, &state, &config, &exe).expect("the running collector is found and restarted");
        let mut old = old;
        let _ = old.wait(); // reaped: it was stopped
        let new: i32 = read(&state.join("lss-collector.pid")).trim().parse().unwrap();
        assert!(line.starts_with(&format!("restarted lss-collector (pid {} -> {new}", old.id())), "{line}");
        assert_ne!(new as u32, old.id());
        // SAFETY: the stand-in this test started
        assert_eq!(unsafe { libc::kill(new, 0) }, 0, "the new one runs");
        unsafe { libc::kill(new, libc::SIGKILL) };
        // a live pid that is NOT a collector is never touched
        let mut other = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(state.join("lss-collector.pid"), other.id().to_string()).unwrap();
        assert!(restart_pidfile(1, &state, &config, &exe).is_none());
        assert_eq!(unsafe { libc::kill(other.id() as i32, 0) }, 0, "left alone");
        let _ = other.kill();
        let _ = other.wait();
        // no pid file / a dead pid: nothing to restart, and the caller says "no background service"
        std::fs::write(state.join("lss-collector.pid"), "999999").unwrap();
        assert!(restart_pidfile(1, &state, &config, &exe).is_none());
    }
}
