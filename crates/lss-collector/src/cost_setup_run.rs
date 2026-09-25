//! card #298: `lss-collector cost-setup` - the cost wizard. Asks how to price electricity, then
//! writes a flat `rates.toml` the collector already reads, stamped with where the rate came from.
//! The numbers and the file text are decided in `lss_core::cost_setup` (pure, tested there); this
//! file is only the asking, the one optional network call, and the write.
//!
//!   lss-collector cost-setup                      interactive (reads /dev/tty, so `curl | bash` works);
//!                                                 its menu also takes a typed time-of-use plan (card #313)
//!   lss-collector cost-setup --zip 94103          state average for that ZIP, no network
//!   lss-collector cost-setup --from-ip            the flag IS the consent: one HTTPS GET to ipinfo.io
//!   lss-collector cost-setup --rate 0.31          your own $/kWh (31c and 31 also read as cents)
//!   lss-collector cost-setup --skip               write nothing; cost tracking stays off
//!   ... [--out PATH] [--force] [--dry-run]
//!
//! Exit: 0 written (or skipped) · 1 lookup failed (bad/uncovered ZIP, not in the US, no network)
//! · 2 usage (bad flags, no terminal to ask on) · 3 an existing rates.toml was kept.

use lss_core::config::expand_home;
use lss_core::cost_setup::{self as cs, IpLocation, RateOrigin, ZipLookup, IP_SERVICE_NAME, IP_SERVICE_URL};
use lss_core::cost_tables::{EIA_MONTH, EIA_RELEASED};
use std::io::{BufRead, Write};

pub const USAGE: &str = "lss-collector cost-setup [--zip ZIP | --from-ip | --rate USD_PER_KWH | --skip] [--out PATH] [--force] [--dry-run]
  How lss prices the electricity your GPUs use. With no choice flag it asks on the terminal.
  --zip ZIP        your state's average residential rate (EIA Electric Power Monthly, embedded table; no network)
  --from-ip        look your state up from your public IP: ONE HTTPS request to ipinfo.io (this flag is your consent)
  --rate RATE      your own rate in $/kWh, e.g. 0.31 (31c or 31 read as cents) - from your bill, the most accurate
  --skip           write nothing; cost tracking stays off (an existing rates.toml is left alone)
  --out PATH       where to write (default ~/.config/lss/rates.toml, the collector's default [rates] path)
  --force          replace an existing file without asking
  --dry-run        print the file instead of writing it
  exit: 0 written/skipped, 1 lookup failed or --rate refused (a bare 2.5: cents or dollars?), 2 usage,
        3 existing file kept";

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Ask,
    Zip(String),
    FromIp,
    Rate(String),
    Skip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Opts {
    pub mode: Mode,
    pub out: String,
    pub force: bool,
    pub dry_run: bool,
}

pub fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { mode: Mode::Ask, out: "~/.config/lss/rates.toml".into(), force: false, dry_run: false };
    let mut chosen = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--zip" => { o.mode = Mode::Zip(val("--zip")?); chosen += 1 }
            "--from-ip" => { o.mode = Mode::FromIp; chosen += 1 }
            "--rate" => { o.mode = Mode::Rate(val("--rate")?); chosen += 1 }
            "--skip" => { o.mode = Mode::Skip; chosen += 1 }
            "--out" => o.out = val("--out")?,
            "--force" => o.force = true,
            "--dry-run" => o.dry_run = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if chosen > 1 {
        return Err("choose ONE of --zip, --from-ip, --rate, --skip".into());
    }
    Ok(o)
}

/// Everything the wizard touches outside itself - a real terminal and network in `main`, a
/// script and a canned answer in the tests.
pub trait Io {
    /// print a line (or a prompt with no newline) to the person
    fn say(&mut self, s: &str);
    /// read one answer; `None` = end of input (the person pressed Ctrl-D, or the script ran out)
    fn ask(&mut self, prompt: &str) -> Option<String>;
    /// the single HTTPS GET to the geolocation service: the body, or why it failed
    fn fetch_ip_location(&mut self) -> Result<String, String>;
    fn today(&self) -> String;
    fn file_exists(&self, path: &str) -> Option<String>;
    fn write_file(&mut self, path: &str, text: &str) -> Result<(), String>;
}

pub fn run(opts: &Opts, io: &mut dyn Io) -> i32 {
    let path = expand_home(&opts.out, &std::env::var("HOME").unwrap_or_default());
    let interactive = opts.mode == Mode::Ask;
    let picked = match &opts.mode {
        Mode::Skip => return skip(io, &path),
        Mode::Ask => match ask_menu(io) {
            Ok(Some(Choice::Tou(plan))) => return write_tou(opts, io, &path, &plan),
            Ok(Some(Choice::Flat(v, o))) => Ok(Some((v, o))),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        },
        Mode::Zip(z) => from_zip(z, io),
        Mode::FromIp => from_ip(io, false),
        // card #332: with no one to ask, a bare number read as a sub-5-cent rate is refused -
        // "2.5c" (cents, said) or "0.025" (dollars, said) both pass
        Mode::Rate(r) => match cs::cents_doubt(r) {
            Some(d) => Err((1, format!("--rate {}: {d}. If that is right, pass --rate {}c; otherwise pass the dollars, like --rate 0.31", r.trim(), r.trim()))),
            None => cs::parse_rate(r).map(|v| Some((v, RateOrigin::Manual))).map_err(|e| (1, e)),
        },
    };
    let (usd, origin) = match picked {
        Ok(Some(p)) => p,
        Ok(None) => return skip(io, &path),
        Err((code, msg)) => {
            io.say(&format!("cost-setup: {msg}\n"));
            return code;
        }
    };
    let today = io.today();
    let text = cs::render_rates_toml(usd, &origin, &today, &format!("lss-collector cost-setup {}", env!("CARGO_PKG_VERSION")));
    let label = cs::source_label(&origin, &today);
    if opts.dry_run {
        io.say(&text);
        io.say(&format!("(dry run - nothing written; would write {path})\n"));
        return 0;
    }
    if let Some(existing) = io.file_exists(&path) {
        if !opts.force {
            let replace = interactive
                && yes_no(io, &format!("{path} already exists ({existing}). Replace it? [y/N] "), false);
            if !replace {
                io.say(&format!("kept the existing {path} - nothing changed (use --force to replace it)\n"));
                return 3;
            }
        }
    }
    if let Err(e) = io.write_file(&path, &text) {
        io.say(&format!("cost-setup: cannot write {path}: {e}\n"));
        return 1;
    }
    io.say(&format!("rate: {label} = ${} /kWh -> {path}\n", trim_rate(usd)));
    io.say("restart the collector to use it (it reads rates.toml at start).\n");
    0
}

fn skip(io: &mut dyn Io, path: &str) -> i32 {
    match io.file_exists(path) {
        Some(existing) => io.say(&format!("skipped - the existing {path} ({existing}) is left as it is\n")),
        None => io.say("skipped - cost tracking stays off. Run `lss-collector cost-setup` any time to turn it on.\n"),
    }
    0
}

type Picked = Result<Option<(f64, RateOrigin)>, (i32, String)>;

/// What the menu picked: a flat rate (ZIP, IP, typed) or a typed time-of-use plan (card #313).
enum Choice {
    Flat(f64, RateOrigin),
    Tou(cs::TouPlan),
}

fn ask_menu(io: &mut dyn Io) -> Result<Option<Choice>, (i32, String)> {
    let flat = |p: Picked| p.map(|o| o.map(|(v, origin)| Choice::Flat(v, origin)));
    io.say("\nElectricity cost - how should lss price the power your GPUs draw?\n");
    io.say(&format!("  1) my ZIP code       - my state's average home rate (U.S. EIA, {EIA_MONTH}); nothing is sent anywhere\n"));
    io.say(&format!("  2) look it up        - from my internet address, via ONE request to {IP_SERVICE_NAME} (asks first)\n"));
    io.say("  3) type my own rate  - $ per kWh from my electricity bill (the most accurate)\n");
    io.say("  4) skip              - no cost figures for now\n");
    io.say("  5) time-of-use plan  - type your bill's peak and off-peak prices and hours\n");
    for _ in 0..5 {
        let Some(a) = io.ask("Choose 1-5 [1] ") else { return Ok(None) };
        match a.trim() {
            "" | "1" => return flat(ask_zip(io)),
            "2" => return flat(from_ip(io, true)),
            "3" => return flat(ask_rate(io).map(|v| v.map(|r| (r, RateOrigin::Manual)))),
            "4" | "s" | "skip" => return Ok(None),
            "5" | "tou" => return ask_tou(io).map(|p| p.map(Choice::Tou)),
            other => io.say(&format!("'{other}' is not one of 1-5.\n")),
        }
    }
    Err((2, "no valid choice after 5 tries".into()))
}

/// Non-interactive `--zip`: one answer, no retries.
fn from_zip(zip: &str, io: &mut dyn Io) -> Picked {
    match cs::lookup_zip(zip) {
        ZipLookup::State { code, usd_per_kwh } => {
            io.say(&format!("ZIP {} -> {}: {:.2}c/kWh average residential (EIA, {EIA_MONTH})\n", zip.trim(), cs::place_name(code).unwrap_or(code), usd_per_kwh * 100.0));
            Ok(Some((usd_per_kwh, RateOrigin::StateAverage { code, how: format!("ZIP {}", zip.trim()) })))
        }
        other => Err((1, zip_problem(&other) + " - pass --rate with the $/kWh from your bill instead")),
    }
}

fn zip_problem(z: &ZipLookup) -> String {
    match z {
        ZipLookup::NotCovered { code } => format!("{} is not in EIA's state table, so there is no average to use", cs::place_name(code).unwrap_or(code)),
        ZipLookup::UnknownPrefix { prefix } => format!("no US ZIP code starts with {prefix}"),
        ZipLookup::Invalid { reason } => reason.clone(),
        ZipLookup::State { .. } => String::new(),
    }
}

fn ask_zip(io: &mut dyn Io) -> Picked {
    for _ in 0..3 {
        let Some(a) = io.ask("Your 5-digit ZIP code: ") else { return Ok(None) };
        match cs::lookup_zip(&a) {
            ZipLookup::State { code, usd_per_kwh } => {
                let q = format!("ZIP {} is in {}: average home rate {:.2}c/kWh (U.S. EIA, {EIA_MONTH}, published {EIA_RELEASED}).\nUse it? [Y/n] ",
                    a.trim(), cs::place_name(code).unwrap_or(code), usd_per_kwh * 100.0);
                if yes_no(io, &q, true) {
                    return Ok(Some((usd_per_kwh, RateOrigin::StateAverage { code, how: format!("ZIP {}", a.trim()) })));
                }
                return ask_rate(io).map(|v| v.map(|r| (r, RateOrigin::Manual)));
            }
            ZipLookup::NotCovered { .. } => {
                io.say(&format!("{}. Type the rate from your bill instead.\n", zip_problem(&cs::lookup_zip(&a))));
                return ask_rate(io).map(|v| v.map(|r| (r, RateOrigin::Manual)));
            }
            other => io.say(&format!("{}. Try again.\n", zip_problem(&other))),
        }
    }
    io.say("No ZIP code matched after 3 tries - type the rate from your bill instead.\n");
    ask_rate(io).map(|v| v.map(|r| (r, RateOrigin::Manual)))
}

/// card #332: true = use what was typed. A bare number read as a sub-5-cent rate ("2.5" = 2.5
/// cents = $0.025/kWh) is said back in words and asked about; No = type it again.
fn cents_confirmed(io: &mut dyn Io, typed: &str) -> bool {
    match cs::cents_doubt(typed) {
        None => true,
        Some(d) => {
            io.say(&format!("Are you sure? {d}.\n"));
            yes_no(io, "Use it anyway? [y/N] ", false)
        }
    }
}

fn ask_rate(io: &mut dyn Io) -> Result<Option<f64>, (i32, String)> {
    for _ in 0..3 {
        let Some(a) = io.ask("Your rate in $ per kWh (e.g. 0.31; 31c also works): ") else { return Ok(None) };
        match cs::parse_rate(&a) {
            Ok(_) if !cents_confirmed(io, &a) => continue,
            Ok(v) => return Ok(Some(v)),
            Err(e) => io.say(&format!("{e}.\n")),
        }
    }
    Err((1, "no usable rate after 3 tries - cost tracking stays off".into()))
}

/// `interactive` = the menu path: ask consent first, and fall back to the ZIP question on any
/// failure. `--from-ip` (non-interactive) was the consent, and a failure is exit 1.
fn from_ip(io: &mut dyn Io, interactive: bool) -> Picked {
    if interactive {
        io.say(&format!("This sends ONE request to {IP_SERVICE_URL}. {IP_SERVICE_NAME} sees your public IP address and answers with\n"));
        io.say("an approximate location (city, state, country). Nothing else is sent, and lss keeps only the state.\n");
        if !yes_no(io, &format!("Contact {IP_SERVICE_NAME} now? [y/N] "), false) {
            io.say("Not contacted. Your ZIP code works without the network:\n");
            return ask_zip(io);
        }
    }
    let located = io.fetch_ip_location().and_then(|body| cs::parse_ipinfo(&body));
    let fallback = |io: &mut dyn Io, why: String| -> Picked {
        if interactive {
            io.say(&format!("{why}. Enter your ZIP code instead:\n"));
            ask_zip(io)
        } else {
            Err((1, format!("{why} - pass --zip or --rate instead")))
        }
    };
    match located {
        Ok(IpLocation::UsState { code, usd_per_kwh, city }) => {
            let place = cs::place_name(code).unwrap_or(code);
            let where_ = if city.is_empty() { place.to_string() } else { format!("{city}, {place}") };
            if interactive {
                let q = format!("{IP_SERVICE_NAME} places you in {where_}: average home rate {:.2}c/kWh (U.S. EIA, {EIA_MONTH}).\nUse it? [Y/n] ", usd_per_kwh * 100.0);
                if !yes_no(io, &q, true) {
                    return ask_zip(io);
                }
            } else {
                io.say(&format!("{IP_SERVICE_NAME}: {where_} -> {:.2}c/kWh average residential (EIA, {EIA_MONTH})\n", usd_per_kwh * 100.0));
            }
            Ok(Some((usd_per_kwh, RateOrigin::StateAverage { code, how: format!("an IP lookup via {IP_SERVICE_NAME}") })))
        }
        Ok(IpLocation::NotUs { country }) => {
            let why = format!("{IP_SERVICE_NAME} places you outside the US ({country}); the built-in table covers US states only");
            if interactive {
                io.say(&format!("{why}. Type the rate from your bill:\n"));
                ask_rate(io).map(|v| v.map(|r| (r, RateOrigin::Manual)))
            } else {
                Err((1, format!("{why} - pass --rate instead")))
            }
        }
        Ok(IpLocation::UsNotCovered { region }) => fallback(io, format!("{IP_SERVICE_NAME} places you in '{region}', which EIA's state table does not cover")),
        Err(e) => fallback(io, format!("the location lookup failed ({e})")),
    }
}

/// card #313: the time-of-use questions the old installer asked, one at a time, each checked.
fn ask_tou(io: &mut dyn Io) -> Result<Option<cs::TouPlan>, (i32, String)> {
    io.say("\nA time-of-use plan: one peak window on weekdays, its own price on weekends and holidays in\n");
    io.say("the same hours, off-peak the rest - optionally different in summer and winter. Take the numbers\n");
    io.say("from your bill or your utility's tariff page. (More periods, holidays: edit rates.toml afterwards.)\n");
    let price = |io: &mut dyn Io, q: &str, default: Option<f64>| -> Result<Option<f64>, (i32, String)> {
        for _ in 0..3 {
            let prompt = match default { Some(d) => format!("{q} [{}] ", trim_rate(d)), None => format!("{q}: ") };
            let Some(a) = io.ask(&prompt) else { return Ok(None) };
            if a.trim().is_empty() {
                if let Some(d) = default {
                    return Ok(Some(d));
                }
            }
            match cs::parse_rate(&a) {
                Ok(_) if !cents_confirmed(io, &a) => continue,
                Ok(v) => return Ok(Some(v)),
                Err(e) => io.say(&format!("{e}.\n")),
            }
        }
        Err((1, "no usable price after 3 tries - nothing written".into()))
    };
    let number = |io: &mut dyn Io, q: &str, default: u32, range: std::ops::RangeInclusive<u32>| -> Result<Option<u32>, (i32, String)> {
        for _ in 0..3 {
            let Some(a) = io.ask(&format!("{q} [{default}] ")) else { return Ok(None) };
            let a = a.trim();
            if a.is_empty() {
                return Ok(Some(default));
            }
            match a.parse::<u32>() {
                Ok(n) if range.contains(&n) => return Ok(Some(n)),
                _ => io.say(&format!("'{a}' is not a whole number from {} to {}.\n", range.start(), range.end())),
            }
        }
        Err((1, "no usable number after 3 tries - nothing written".into()))
    };
    macro_rules! got {
        ($e:expr) => {
            match $e? {
                Some(v) => v,
                None => return Ok(None),
            }
        };
    }
    for _ in 0..3 {
        let on_peak = got!(price(io, "On-peak price, $ per kWh (e.g. 0.52)", None));
        let peak_start = got!(number(io, "On-peak starts at (local hour, 0-23)", 16, 0..=23));
        let peak_end = got!(number(io, "On-peak ends at (local hour, 1-24)", 21, 1..=24));
        let off_peak = got!(price(io, "Off-peak price, $ per kWh (the other hours)", None));
        let weekend_peak = got!(price(io, "Weekend/holiday price in those same hours, $ per kWh", Some(on_peak)));
        let seasons = if yes_no(io, "Does the plan change between summer and winter? [y/N] ", false) {
            let summer_start = got!(number(io, "Summer starts in month (1-12)", 6, 1..=12));
            let summer_end = got!(number(io, "Winter starts in month (1-12)", 10, 1..=12));
            let winter_peak = got!(price(io, "Winter on-peak price, $ per kWh (the same hours, EVERY day in winter - weekends too)", Some(on_peak)));
            Some(cs::Seasons { summer_start, summer_end, winter_peak })
        } else {
            None
        };
        let fixed_usd_per_day = got!(price_or_zero(io));
        let plan = cs::TouPlan { on_peak, peak_start, peak_end, off_peak, weekend_peak, seasons, fixed_usd_per_day };
        match cs::check_tou(&plan) {
            Ok(()) => {
                // card #325: usable, but maybe a slip - ask once, default no = type it again
                let doubts = cs::tou_doubts(&plan);
                if doubts.is_empty() {
                    return Ok(Some(plan));
                }
                for d in &doubts {
                    io.say(&format!("Are you sure? {d}.\n"));
                }
                if yes_no(io, "Use these numbers anyway? [y/N] ", false) {
                    return Ok(Some(plan));
                }
                io.say("Once more:\n");
            }
            Err(e) => io.say(&format!("That plan cannot be used: {e}. Once more:\n")),
        }
    }
    Err((1, "no usable time-of-use plan after 3 tries - nothing written".into()))
}

/// The fixed daily charge: 0 (the default) or a dollar amount.
fn price_or_zero(io: &mut dyn Io) -> Result<Option<f64>, (i32, String)> {
    for _ in 0..3 {
        let Some(a) = io.ask("Fixed daily charge, $ per day (0 if none or not sure) [0] ") else { return Ok(None) };
        let a = a.trim().trim_start_matches('$');
        if a.is_empty() {
            return Ok(Some(0.0));
        }
        match a.parse::<f64>() {
            Ok(v) if v.is_finite() && (0.0..100.0).contains(&v) => return Ok(Some(v)),
            _ => io.say(&format!("'{a}' is not a dollar amount per day (e.g. 0.25).\n")),
        }
    }
    Err((1, "no usable daily charge after 3 tries - nothing written".into()))
}

fn write_tou(opts: &Opts, io: &mut dyn Io, path: &str, plan: &cs::TouPlan) -> i32 {
    let today = io.today();
    let text = cs::render_tou_toml(plan, &today, &format!("lss-collector cost-setup {}", env!("CARGO_PKG_VERSION")));
    if opts.dry_run {
        io.say(&text);
        io.say(&format!("(dry run - nothing written; would write {path})\n"));
        return 0;
    }
    if let Some(existing) = io.file_exists(path) {
        if !opts.force && !yes_no(io, &format!("{path} already exists ({existing}). Replace it? [y/N] "), false) {
            io.say(&format!("kept the existing {path} - nothing changed (use --force to replace it)\n"));
            return 3;
        }
    }
    if let Err(e) = io.write_file(path, &text) {
        io.say(&format!("cost-setup: cannot write {path}: {e}\n"));
        return 1;
    }
    io.say(&format!("rate: {} = on-peak ${} /kWh {}-{}h weekdays, off-peak ${} /kWh -> {path}\n", cs::tou_source_label(&today), trim_rate(plan.on_peak), plan.peak_start, plan.peak_end, trim_rate(plan.off_peak)));
    io.say("restart the collector to use it (it reads rates.toml at start).\n");
    0
}

fn yes_no(io: &mut dyn Io, q: &str, default: bool) -> bool {
    match io.ask(q).map(|a| a.trim().to_ascii_lowercase()) {
        Some(a) if a.is_empty() => default,
        Some(a) => a.starts_with('y'),
        None => false,
    }
}

fn trim_rate(v: f64) -> String {
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The real terminal + network + filesystem.
struct RealIo {
    tty: Option<(std::io::BufReader<std::fs::File>, std::fs::File)>,
}

impl Io for RealIo {
    fn say(&mut self, s: &str) {
        match &mut self.tty {
            Some((_, w)) => { let _ = w.write_all(s.as_bytes()); let _ = w.flush(); }
            None => { print!("{s}"); let _ = std::io::stdout().flush(); }
        }
    }
    fn ask(&mut self, prompt: &str) -> Option<String> {
        self.say(prompt);
        let (r, _) = self.tty.as_mut()?;
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        }
    }
    fn fetch_ip_location(&mut self) -> Result<String, String> {
        // curl, not a TLS stack in this binary: the collector otherwise never makes an outbound
        // HTTPS call, and this one only runs when a person asked for it. --proto =https refuses
        // any downgrade; 10 s cap so a dead network falls back to the ZIP question quickly.
        let out = std::process::Command::new("curl")
            .args(["-fsS", "--proto", "=https", "--max-time", "10", "-H", "Accept: application/json", IP_SERVICE_URL])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if err.is_empty() { format!("curl exited {}", out.status) } else { err });
        }
        String::from_utf8(out.stdout).map_err(|_| "the answer was not text".into())
    }
    fn today(&self) -> String {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    }
    fn file_exists(&self, path: &str) -> Option<String> {
        describe_existing(path)
    }
    fn write_file(&mut self, path: &str, text: &str) -> Result<(), String> {
        let p = std::path::Path::new(path);
        if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // write-then-rename: a crash mid-write never leaves the collector a half file
        let tmp = p.with_extension("toml.tmp");
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0600: a rate implies where you live - keep it to this user
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, p).map_err(|e| e.to_string())
    }
}

/// Whether something is already at `path`, and a short description of it for the "replace?"
/// question. EXISTENCE comes from `symlink_metadata`, never from reading the file: card #298's
/// verifier found that `read_to_string(..).ok()?` turned every read error - a root-owned 0600 file
/// left by a sudo run, a non-UTF-8 file, a directory - into "nothing there", so the --force guard
/// was skipped and the rename REPLACED the file with exit 0. Anything at the path counts
/// (a dangling symlink too); only then is it read, for the label.
fn describe_existing(path: &str) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_dir() {
        return Some("a directory, not a rate table".into());
    }
    Some(match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => "a file you cannot read (permission denied)".into(),
        Err(e) => format!("a file that cannot be read: {e}"),
        Ok(bytes) => match String::from_utf8(bytes).ok().map(|s| lss_core::rates::parse_rates_file(&s)) {
            Some(Ok(t)) => format!("\"{}\"", t.name),
            _ => "not a readable rate table".into(),
        },
    })
}

/// `lss-collector cost-setup ...` - returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("lss-collector cost-setup: {e}\n{USAGE}");
            return 2;
        }
    };
    // Questions go to /dev/tty, not stdin: under `curl ... | bash` stdin is the script itself.
    let tty = std::fs::File::open("/dev/tty").ok().and_then(|r| std::fs::OpenOptions::new().write(true).open("/dev/tty").ok().map(|w| (std::io::BufReader::new(r), w)));
    let needs_tty = opts.mode == Mode::Ask;
    if needs_tty && tty.is_none() {
        eprintln!("lss-collector cost-setup: no terminal to ask on - pass --zip, --from-ip, --rate or --skip\n{USAGE}");
        return 2;
    }
    // a non-interactive run never needs the terminal (it may still answer "replace?" -> no)
    let mut io = RealIo { tty: if needs_tty { tty } else { None } };
    run(&opts, &mut io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    struct Script {
        answers: VecDeque<String>,
        said: String,
        ip: Result<String, String>,
        ip_calls: u32,
        files: HashMap<String, String>,
    }

    impl Script {
        fn new(answers: &[&str]) -> Self {
            Script { answers: answers.iter().map(|s| s.to_string()).collect(), said: String::new(), ip: Err("offline".into()), ip_calls: 0, files: HashMap::new() }
        }
        fn written(&self) -> Option<&String> {
            self.files.get("/cfg/rates.toml")
        }
    }

    impl Io for Script {
        fn say(&mut self, s: &str) { self.said.push_str(s) }
        fn ask(&mut self, p: &str) -> Option<String> { self.said.push_str(p); self.answers.pop_front() }
        fn fetch_ip_location(&mut self) -> Result<String, String> { self.ip_calls += 1; self.ip.clone() }
        fn today(&self) -> String { "2026-09-24".into() }
        fn file_exists(&self, p: &str) -> Option<String> { self.files.get(p).map(|_| "\"old\"".into()) }
        fn write_file(&mut self, p: &str, t: &str) -> Result<(), String> { self.files.insert(p.into(), t.into()); Ok(()) }
    }

    fn opts(mode: Mode) -> Opts {
        Opts { mode, out: "/cfg/rates.toml".into(), force: false, dry_run: false }
    }

    fn parsed(io: &Script) -> lss_core::rates::RateTable {
        lss_core::rates::parse_rates_file(io.written().expect("a file was written")).expect("the collector reads it")
    }

    #[test]
    fn args_parse_and_conflicts_are_refused() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&["--zip", "02134"]).unwrap().mode, Mode::Zip("02134".into()));
        assert_eq!(a(&[]).unwrap().mode, Mode::Ask);
        assert!(a(&["--zip", "1", "--rate", "0.2"]).is_err());
        assert!(a(&["--zip"]).is_err());
        assert!(a(&["--bogus"]).is_err());
        let o = a(&["--rate", "0.2", "--out", "/x", "--force", "--dry-run"]).unwrap();
        assert!(o.force && o.dry_run && o.out == "/x");
    }

    #[test]
    fn zip_flag_writes_the_state_average_with_its_source() {
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Zip("02134".into())), &mut io), 0, "{}", io.said);
        let t = parsed(&io);
        assert_eq!(t.source.as_deref(), Some("MA avg (EIA 2026-06)"));
        assert_eq!(t.price_for(lss_core::rates::DayContext { hour: 1, is_summer: false, is_weekend_or_holiday: false }).unwrap().0, 0.2961);
        assert_eq!(io.ip_calls, 0, "a ZIP never touches the network");
        assert!(io.said.contains("rate: MA avg (EIA 2026-06)"), "{}", io.said);
    }

    #[test]
    fn zip_flag_failures_exit_1_and_write_nothing() {
        for z in ["00901", "abc", "00012", "9410"] {
            let mut io = Script::new(&[]);
            assert_eq!(run(&opts(Mode::Zip(z.into())), &mut io), 1, "{z}: {}", io.said);
            assert!(io.written().is_none());
        }
    }

    #[test]
    fn rate_flag_and_skip_flag() {
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Rate("31c".into())), &mut io), 0);
        let t = parsed(&io);
        assert_eq!(t.source.as_deref(), Some("entered by hand (2026-09-24)"));
        assert_eq!(t.price_for(lss_core::rates::DayContext { hour: 1, is_summer: false, is_weekend_or_holiday: false }).unwrap().0, 0.31);
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Rate("$40".into())), &mut io), 1);
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Skip), &mut io), 0);
        assert!(io.written().is_none());
    }

    #[test]
    fn existing_file_is_kept_unless_forced_or_confirmed() {
        let mut io = Script::new(&[]);
        io.files.insert("/cfg/rates.toml".into(), "mine".into());
        assert_eq!(run(&opts(Mode::Rate("0.2".into())), &mut io), 3);
        assert_eq!(io.written().unwrap(), "mine");
        let mut o = opts(Mode::Rate("0.2".into()));
        o.force = true;
        assert_eq!(run(&o, &mut io), 0);
        assert_ne!(io.written().unwrap(), "mine");
        // interactive: asked, default is NO
        let mut io = Script::new(&["3", "0.2", ""]);
        io.files.insert("/cfg/rates.toml".into(), "mine".into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 3);
        assert_eq!(io.written().unwrap(), "mine");
        let mut io = Script::new(&["3", "0.2", "y"]);
        io.files.insert("/cfg/rates.toml".into(), "mine".into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0);
        assert_ne!(io.written().unwrap(), "mine");
    }

    #[test]
    fn dry_run_prints_and_writes_nothing() {
        let mut o = opts(Mode::Zip("94103".into()));
        o.dry_run = true;
        let mut io = Script::new(&[]);
        assert_eq!(run(&o, &mut io), 0);
        assert!(io.written().is_none());
        assert!(io.said.contains("kind = \"flat\"") && io.said.contains("usd_per_kwh = 0.3474"), "{}", io.said);
    }

    #[test]
    fn interactive_zip_path_retries_bad_input_then_accepts() {
        let mut io = Script::new(&["", "abc", "00012", "94103", ""]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("not a US ZIP code") && io.said.contains("no US ZIP code starts with 000"), "{}", io.said);
        assert_eq!(parsed(&io).source.as_deref(), Some("CA avg (EIA 2026-06)"));
        assert_eq!(io.ip_calls, 0);
    }

    #[test]
    fn interactive_territory_zip_goes_to_manual_rate() {
        let mut io = Script::new(&["1", "00901", "0.27"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("Puerto Rico is not in EIA's state table"));
        assert_eq!(parsed(&io).source.as_deref(), Some("entered by hand (2026-09-24)"));
    }

    #[test]
    fn declining_the_average_asks_for_a_rate() {
        let mut io = Script::new(&["1", "10001", "n", "0.25"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0);
        assert_eq!(parsed(&io).source.as_deref(), Some("entered by hand (2026-09-24)"));
    }

    #[test]
    fn ip_lookup_needs_consent_and_declining_sends_nothing() {
        let mut io = Script::new(&["2", "", "60601", ""]); // Enter at consent = NO (default)
        io.ip = Ok(r#"{"region":"California","country":"US"}"#.into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert_eq!(io.ip_calls, 0, "no consent = no request");
        assert!(io.said.contains("ipinfo.io sees your public IP address"), "the prompt names the service and what it sees");
        assert_eq!(parsed(&io).source.as_deref(), Some("IL avg (EIA 2026-06)"));
    }

    #[test]
    fn ip_lookup_with_consent_makes_one_call_and_uses_the_state() {
        let mut io = Script::new(&["2", "y", ""]);
        io.ip = Ok(r#"{"city":"Austin","region":"Texas","country":"US","postal":"78701"}"#.into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert_eq!(io.ip_calls, 1);
        assert!(io.said.contains("Austin, Texas"));
        let t = parsed(&io);
        assert_eq!(t.source.as_deref(), Some("TX avg (EIA 2026-06)"));
        assert!(io.written().unwrap().contains("IP lookup via ipinfo.io"));
    }

    #[test]
    fn ip_lookup_network_failure_falls_back_to_the_zip_question() {
        let mut io = Script::new(&["2", "y", "98101", ""]);
        io.ip = Err("Could not resolve host: ipinfo.io".into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("lookup failed") && io.said.contains("Enter your ZIP code instead"), "{}", io.said);
        assert_eq!(parsed(&io).source.as_deref(), Some("WA avg (EIA 2026-06)"));
    }

    #[test]
    fn ip_lookup_outside_the_us_asks_for_a_rate() {
        let mut io = Script::new(&["2", "y", "0.40"]);
        io.ip = Ok(r#"{"city":"Berlin","region":"Berlin","country":"DE"}"#.into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("outside the US (DE)"));
        assert_eq!(parsed(&io).source.as_deref(), Some("entered by hand (2026-09-24)"));
    }

    #[test]
    fn from_ip_flag_is_consent_and_its_failure_is_exit_1() {
        let mut io = Script::new(&[]);
        io.ip = Ok(r#"{"region":"New York","country":"US"}"#.into());
        assert_eq!(run(&opts(Mode::FromIp), &mut io), 0);
        assert_eq!(io.ip_calls, 1);
        assert_eq!(parsed(&io).source.as_deref(), Some("NY avg (EIA 2026-06)"));
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::FromIp), &mut io), 1);
        assert!(io.written().is_none());
        let mut io = Script::new(&[]);
        io.ip = Ok(r#"{"country":"CA","region":"Ontario"}"#.into());
        assert_eq!(run(&opts(Mode::FromIp), &mut io), 1);
    }

    #[test]
    fn skip_and_end_of_input_write_nothing() {
        let mut io = Script::new(&["4"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0);
        assert!(io.written().is_none() && io.said.contains("cost tracking stays off"));
        let mut io = Script::new(&[]); // Ctrl-D at the menu
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0);
        assert!(io.written().is_none());
    }

    /// card #298 verifier FAIL (lss-inst-v1): an existing rates.toml the wizard cannot READ used to
    /// count as absent, so a run without --force replaced it and exited 0. The real filesystem is
    /// needed here (the Script Io above cannot be unreadable), so this drives RealIo - with
    /// Mode::Rate only, which never reaches the network.
    #[test]
    fn an_existing_file_it_cannot_read_is_still_kept_without_force() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lss-cost-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run_at = |p: &std::path::Path| {
            let o = Opts { mode: Mode::Rate("0.2".into()), out: p.to_string_lossy().into_owned(), force: false, dry_run: false };
            run(&o, &mut RealIo { tty: None })
        };
        // 1. not UTF-8 (the verifier's exact bytes)
        let bad = dir.join("non-utf8.toml");
        let bytes = [0xffu8, 0xfe, 0x00, 0x41];
        std::fs::write(&bad, bytes).unwrap();
        assert_eq!(run_at(&bad), 3, "a non-UTF-8 rates.toml is an existing file: kept, exit 3");
        assert_eq!(std::fs::read(&bad).unwrap(), bytes, "and byte-identical");
        // 2. no read permission (as root the read succeeds anyway - the file must be kept either way)
        let locked = dir.join("locked.toml");
        std::fs::write(&locked, "name = \"mine\"\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(run_at(&locked), 3, "an unreadable rates.toml is an existing file: kept, exit 3");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&locked).unwrap(), "name = \"mine\"\n", "and byte-identical");
        // 3. a directory: unreadable as a file even for root, so this arm is exercised everywhere
        let d = dir.join("adir.toml");
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(run_at(&d), 3, "a directory at the path is kept, exit 3");
        assert!(d.is_dir());
        // skip() must not claim cost tracking is off when something is at the path
        assert_eq!(describe_existing(bad.to_str().unwrap()).as_deref(), Some("not a readable rate table"));
        assert_eq!(describe_existing(dir.join("absent.toml").to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------- card #313
    #[test]
    fn menu_choice_5_takes_a_typed_time_of_use_plan_and_writes_one_the_collector_prices() {
        // on-peak 0.52, 16-21 (Enter = defaults), off-peak 31c, weekend = Enter (same as on-peak),
        // seasons yes: Jun (Enter) .. Oct (Enter), winter peak 0.45; fixed 0.25
        let mut io = Script::new(&["5", "0.52", "", "", "31c", "", "y", "", "", "0.45", "0.25"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("5) time-of-use plan"), "{}", io.said);
        let t = parsed(&io);
        assert!(!t.is_flat());
        assert_eq!(t.fixed_usd_per_day, Some(0.25));
        assert_eq!(t.source.as_deref(), Some("time-of-use, entered by hand (2026-09-24)"));
        let at = |hour, summer, weekend| t.price_for(lss_core::rates::DayContext { hour, is_summer: summer, is_weekend_or_holiday: weekend }).map(|(v, _)| v);
        assert_eq!((at(17, true, false), at(10, true, false), at(17, true, true), at(17, false, false)), (Some(0.52), Some(0.31), Some(0.52), Some(0.45)));
        assert!(io.said.contains("rate: time-of-use, entered by hand (2026-09-24) = on-peak $0.52 /kWh 16-21h weekdays, off-peak $0.31 /kWh"), "{}", io.said);
    }

    #[test]
    fn a_tou_plan_with_bad_numbers_is_asked_again_and_an_ended_input_writes_nothing() {
        // a price of 0, then 0.5; hour 25 (refused) then 17; end 16 (before the start: the whole plan
        // is refused in words and asked once more) ...
        let mut io = Script::new(&["5", "0", "0.5", "25", "17", "16", "0.2", "", "", "", "0.5", "17", "22", "0.2", "", "", ""]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("is not a whole number from 0 to 23"), "{}", io.said);
        assert!(io.said.contains("across midnight is not supported"), "{}", io.said);
        let t = parsed(&io);
        assert_eq!(t.price_for(lss_core::rates::DayContext { hour: 21, is_summer: false, is_weekend_or_holiday: false }).map(|(v, _)| v), Some(0.5), "no seasons: winter uses the same plan");
        // the input ends half way: nothing is written, cost stays off
        let mut io = Script::new(&["5", "0.52", "16"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0);
        assert!(io.written().is_none(), "{}", io.said);
    }

    #[test]
    fn a_tou_plan_that_looks_like_a_slip_is_asked_about_and_typed_again_on_no() {
        // card #325: off-peak 0.60 above on-peak 0.52 -> "Are you sure?" -> n -> typed again; then
        // the fixed charge 21 (the verification's slip) -> "Are you sure?" -> y keeps it
        let mut io = Script::new(&["5", "0.52", "", "", "0.60", "", "", "0.25", "n", "0.52", "", "", "0.31", "", "", "21", "y"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("Are you sure? off-peak ($0.60/kWh) is dearer than on-peak ($0.52/kWh)"), "{}", io.said);
        assert!(io.said.contains("Are you sure? a fixed charge of $21 a day"), "{}", io.said);
        let t = parsed(&io);
        assert_eq!(t.price_for(lss_core::rates::DayContext { hour: 10, is_summer: true, is_weekend_or_holiday: false }).map(|(v, _)| v), Some(0.31), "the second, corrected plan was written");
        assert_eq!(t.fixed_usd_per_day, Some(21.0), "'y' keeps a number that is unusual but deliberate");
    }

    #[test]
    fn the_winter_question_says_it_covers_every_day_of_winter() {
        // card #325: render writes the winter periods with day_kind = "any", so the weekend price is
        // a SUMMER price - the question has to say the winter price applies on weekends too
        let mut io = Script::new(&["5", "0.52", "", "", "0.31", "0.40", "y", "", "", "0.45", ""]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("Winter on-peak price, $ per kWh (the same hours, EVERY day in winter - weekends too)"), "{}", io.said);
        let t = parsed(&io);
        assert_eq!(t.price_for(lss_core::rates::DayContext { hour: 17, is_summer: false, is_weekend_or_holiday: true }).map(|(v, _)| v), Some(0.45), "what the question says is what gets priced");
    }

    #[test]
    fn a_typed_tou_plan_never_replaces_an_existing_file_unasked() {
        let mut io = Script::new(&["5", "0.52", "", "", "0.31", "", "", "", "n"]);
        io.files.insert("/cfg/rates.toml".into(), "old".into());
        assert_eq!(run(&opts(Mode::Ask), &mut io), 3, "{}", io.said);
        assert_eq!(io.written().map(String::as_str), Some("old"));
    }

    fn price_at(io: &Script, hour: u32) -> Option<f64> {
        parsed(io).price_for(lss_core::rates::DayContext { hour, is_summer: true, is_weekend_or_holiday: false }).map(|(v, _)| v)
    }

    #[test]
    fn a_flat_rate_typed_as_a_bare_small_number_is_said_back_as_cents_and_asked_about() {
        // card #332: menu 3, "2.5" -> "Are you sure? '2.5' reads as 2.5 cents = $0.025/kWh" -> n -> typed again
        let mut io = Script::new(&["3", "2.5", "n", "0.25"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("Are you sure? '2.5' reads as 2.5 cents = $0.025/kWh, below almost any home rate"), "{}", io.said);
        assert_eq!(price_at(&io, 1), Some(0.25), "the retyped rate was written");
        // y keeps it: 2.5 cents is odd, not impossible
        let mut io = Script::new(&["3", "2.5", "y"]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert_eq!(price_at(&io, 1), Some(0.025));
        // said explicitly (2.5c), or an ordinary bare number of cents (31): no question
        for (typed, want) in [("2.5c", 0.025), ("31", 0.31), ("0.31", 0.31)] {
            let mut io = Script::new(&["3", typed]);
            assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{typed}: {}", io.said);
            assert!(!io.said.contains("Are you sure?"), "{typed}: {}", io.said);
            assert_eq!(price_at(&io, 1), Some(want), "{typed}");
        }
    }

    #[test]
    fn a_tou_price_typed_as_a_bare_small_number_is_said_back_as_cents_and_asked_about() {
        // card #332 (rv lss-inst-v2 on #325): on-peak "2.5" -> asked -> n -> 0.52 typed again
        let mut io = Script::new(&["5", "2.5", "n", "0.52", "", "", "0.31", "", "", ""]);
        assert_eq!(run(&opts(Mode::Ask), &mut io), 0, "{}", io.said);
        assert!(io.said.contains("Are you sure? '2.5' reads as 2.5 cents = $0.025/kWh"), "{}", io.said);
        assert_eq!(price_at(&io, 17), Some(0.52), "on-peak is the retyped 0.52");
        assert_eq!(price_at(&io, 10), Some(0.31));
    }

    #[test]
    fn the_rate_flag_refuses_a_bare_small_number_it_would_read_as_cents() {
        // card #332: nobody to ask -> refused, with the reading and both ways to say it
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Rate("2.5".into())), &mut io), 1, "{}", io.said);
        assert!(io.written().is_none());
        assert!(io.said.contains("--rate 2.5: '2.5' reads as 2.5 cents = $0.025/kWh") && io.said.contains("pass --rate 2.5c"), "{}", io.said);
        let mut io = Script::new(&[]);
        assert_eq!(run(&opts(Mode::Rate("2.5c".into())), &mut io), 0, "{}", io.said);
        assert_eq!(price_at(&io, 1), Some(0.025));
    }
}
