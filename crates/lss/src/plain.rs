//! Plain-text output for agents, pipes and logs. No colour, no box drawing, stable labels.

use crate::money::{coverage_note, usd, usd_per_kwh};
use lss_core::model::{LaneStatus, Status};
use lss_core::timeutil::{fmt_duration, fmt_local};
use std::fmt::Write;

pub fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

pub fn opt_ms(v: Option<f64>) -> String {
    match v {
        Some(ms) if ms >= 10_000.0 => format!("{:.1} s", ms / 1000.0),
        Some(ms) if ms >= 100.0 => format!("{ms:.0} ms"),
        Some(ms) => format!("{ms:.1} ms"),
        None => "-".into(),
    }
}

pub fn num(v: Option<f64>, unit: &str) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{x:.0}{unit}"))
}

pub fn gib(mib: Option<f64>) -> String {
    mib.map_or_else(|| "-".to_string(), |m| format!("{:.1}", m / 1024.0))
}

pub fn serve_state(s: &Status, now: i64) -> String {
    if s.serve.up {
        // live, from the container's start time: the clock keeps ticking between refreshes
        let up = s.serve.started_at.map(|t| now - t).or(s.serve.uptime_s);
        // card #180 gate 3: "UP -" read like a glitch on an engine with no docker; say UP alone
        up.map_or_else(|| "UP".to_string(), |u| format!("UP {}", fmt_duration(u)))
    } else {
        match s.serve.down_since {
            Some(t) => format!("DOWN {}", fmt_duration(now - t)),
            None => "DOWN".to_string(),
        }
    }
}

pub fn throttle_label(flags: &[String]) -> String {
    if flags.is_empty() { "ok".into() } else { flags.join("+") }
}

pub fn top_keys(l: &LaneStatus) -> String {
    if l.top_keys_60m.is_empty() {
        return "-".into();
    }
    l.top_keys_60m.iter().map(|k| format!("{} {}", k.key, k.count)).collect::<Vec<_>>().join(", ")
}

/// `(n invalid skipped)`: probes since the C1 reading shown that were NOT used - they collided
/// with real traffic (`slow_ttft`, `contended`), found the engine busy, or failed.
pub fn invalid_label(s: &Status) -> String {
    format!("({} invalid skipped)", s.probe.invalid_skipped)
}

/// `probes: N` - the collector's own C1 probe requests, which are NOT in the lane numbers.
/// N counts since the gate started (the life of its `admitted` counter).
pub fn probes_label(trusted: &LaneStatus) -> String {
    let p = &trusted.probes;
    format!("probes: {} excluded ({} in 10 min)", p.admitted, p.requests_10m)
}

pub fn incident_line(i: &lss_core::incidents::Incident, now: i64) -> String {
    let span = match i.end {
        None => format!("OPEN {}", fmt_duration(now - i.start)),
        Some(e) if e > i.start => fmt_duration(e - i.start),
        Some(_) => "-".to_string(),
    };
    format!("{}  {:<17} {:>9}  {}", fmt_local(i.start, "%m-%d %H:%M"), i.kind, span, i.detail)
}

pub fn alert_line(a: &lss_core::model::AlertRow) -> String {
    let tail = if a.delivered { "" } else { "  [not delivered]" };
    format!("{}  {:<8} {}{}", fmt_local(a.ts, "%m-%d %H:%M"), a.severity, a.message, tail)
}

/// card #173: the COST block for `lss status`. The owner's words were "i dont see any of the
/// things we talked about in terms of pricing" - the money existed only in the TUI's
/// ELECTRICITY panel, so the plain CLI (what a stranger reads, and what a script parses) showed
/// none of it.
///
/// It carries THE SAME CAVEATS as the panel, and it does so by using the same formatters from
/// `crate::money` rather than by re-wording them here: a dollar figure that does not exist yet
/// is an em dash WITH ITS REASON and never a fabricated $0.00; a partly-covered window says how
/// much data is actually behind the number; the delivery+generation scope and the excluded fixed
/// daily charge are stated; and an unresolved per-kWh adder is shown as unresolved and NOT
/// applied. Coverage caveats sit on their own continuation lines, as card #114 required on the
/// panel - the figure and its qualifier must not share a line that something can truncate.
fn cost_block(o: &mut String, s: &Status, now: i64) {
    let Some(c) = &s.cost else {
        // never zeros: "off" and "$0.00" are different facts, and only one of them is true here
        let _ = writeln!(o, "COST     cost tracking is off (no ~/.config/lss/rates.toml - see packaging/rates.toml.example)");
        return;
    };
    let plan = if c.is_flat { format!("{} (flat)", c.rate_name) } else { c.rate_name.clone() };
    // card #298: where the rate came from ("rate: CA avg (EIA 2026-06)"), when rates.toml says
    let from = if c.rate_source.is_empty() { String::new() } else { format!(" \u{b7} rate: {}", c.rate_source) };
    let _ = writeln!(o, "COST     plan {plan} \u{b7} effective {}{from}", c.effective_date);
    let _ = writeln!(o, "         right now {} \u{b7} {}  \u{b7}  live draw {}/hour",
        usd_per_kwh(c.current_usd_per_kwh),
        if c.is_flat { "flat".to_string() } else { c.current_period.clone() },
        usd(c.live_usd_per_hour, "no live GPU power reading"));
    let since_midnight = (now - lss_core::timeutil::local_midnight(now)).max(0);
    let _ = writeln!(o, "         today {} \u{b7} {} kWh (since local midnight)",
        usd(c.today_usd, "no priced samples yet today"),
        c.today_kwh.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v:.2}")));
    let note = coverage_note(c.today_covered_secs, since_midnight);
    if !note.is_empty() {
        let _ = writeln!(o, "           {note}");
    }
    let _ = writeln!(o, "         last 24h {} \u{b7} {} kWh (rolling)",
        usd(c.last_24h_usd, "no priced samples in the last 24h"),
        c.last_24h_kwh.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v:.2}")));
    let note = coverage_note(c.last_24h_covered_secs, 86_400);
    if !note.is_empty() {
        let _ = writeln!(o, "           {note}");
    }
    let excludes = c.fixed_usd_per_day.map_or_else(
        || "no fixed daily charge configured".to_string(),
        |v| format!("excludes ${v:.2}/day fixed charge"));
    let _ = writeln!(o, "         delivery+generation only, as of {} - {excludes}", c.effective_date);
    if let Some(u) = c.unresolved_usd_per_kwh {
        let _ = writeln!(o, "         +${u:.4}/kWh unresolved (may or may not belong on top of the numbers above - not applied either way)");
    }
}

/// card #176: SPENDING over time. The owner: "how much we are spending everyday, every week,
/// every month or some kind of chart that shows spending."
///
/// Every window says what it covers, because a month that is three days old is a three-day
/// figure and must read like one (item 1). The projection appears only when the month is both
/// long enough and covered enough to support it, and when it does not, the line says WHY rather
/// than going quiet (item 3) - `lss_core::rates::spending` makes that call, this only prints it.
fn spending_block(o: &mut String, s: &Status) {
    let Some(sp) = &s.spending else { return };  // cost tracking off: the COST block already said so
    let w = |label: &str, win: &lss_core::rates::SpendWindow| {
        let money = usd(win.usd, "nothing priced");
        let kwh = win.kwh.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v:.2} kWh"));
        let note = coverage_note(Some(win.covered_secs), win.nominal_secs);
        let tail = if note.is_empty() { String::new() } else { format!("  ({note})") };
        format!("{label} {money} \u{b7} {kwh}{tail}")
    };
    let _ = writeln!(o, "SPENDING {}", w("today", &sp.today));
    let _ = writeln!(o, "         {}", w("yesterday", &sp.yesterday));
    let _ = writeln!(o, "         {}", w("this week", &sp.this_week));
    let _ = writeln!(o, "         {}", w("last 7 days", &sp.last_7d));
    // card #226: "month to date" (it always was since the 1st), and the YEAR to date
    let _ = writeln!(o, "         {}", w("month to date", &sp.this_month));
    let _ = writeln!(o, "         {}", w("last 30 days", &sp.last_30d));
    let _ = writeln!(o, "         {}", w("year to date", &sp.this_year));
    let jan1 = format!("{}-01-01", lss_core::timeutil::fmt_local(s.generated_at, "%Y"));
    if let Some(first) = sp.year_first_day.as_deref().filter(|f| *f > jan1.as_str()) {
        let _ = writeln!(o, "           since {first} - the stored history does not reach Jan 1");
    }
    match sp.month_projection_usd {
        Some(p) => {
            let _ = writeln!(o, "         at this rate this month lands near ${p:.2} ({})", sp.projection_note);
        }
        None => {
            let _ = writeln!(o, "         no month projection: {}", sp.projection_note);
        }
    }
    // card #176 item 5: every figure above carries the SAME caveats the COST block states, and
    // says so where the figures are - a reader who scrolls to SPENDING must not have to remember
    // a line from another block to know what these dollars exclude.
    if let Some(c) = &s.cost {
        let excludes = c.fixed_usd_per_day.map_or_else(
            || "no fixed daily charge configured".to_string(),
            |v| format!("excludes ${v:.2}/day fixed charge"));
        let _ = writeln!(o, "         priced on {} \u{b7} delivery+generation only, as of {} - {excludes}", c.rate_name, c.effective_date);
    }
    // the daily series is what the chart draws; in text, say it exists and how much of it there is
    if !sp.daily.is_empty() {
        let first = sp.daily.first().map(|d| d.date.clone()).unwrap_or_default();
        let last = sp.daily.last().map(|d| d.date.clone()).unwrap_or_default();
        let _ = writeln!(o, "         {} day(s) of daily history, {first} to {last} (page 5 TOKENS charts it)", sp.daily.len());
    }
}

pub fn status(s: &Status, now: i64) -> String {
    let mut o = String::new();
    let sv = &s.serve;
    let _ = writeln!(o, "LLM SERVER STATUS  {}  {}  {}  restarts today {}  {}",
        s.host, sv.model.as_deref().unwrap_or("(no model)"), serve_state(s, now), sv.restarts_today, fmt_local(now, "%H:%M:%S"));
    // card #331: the engine told us why it is down (a refused API key): say it on line 2
    if let Some(why) = sv.down_reason.as_deref().filter(|_| !sv.up) {
        let _ = writeln!(o, "WHY DOWN {why}");
    }
    let age = now - s.generated_at;
    if age > 30 {
        let _ = writeln!(o, "WARNING  collector data is {} old - its poll loop may be stuck", fmt_duration(age));
    }
    let reading = match (sv.prefill_tok_s, sv.prefill_tok_s_typical) {
        (Some(v), _) => format!("{v:.0} tok/s"),
        (None, Some(t)) => format!("{t:.0} tok/s typical (nothing being read now)"),
        (None, None) => format!("- ({})", sv.no_reading_reason()),
    };
    let cached = sv.cached_share_10m.map_or_else(String::new, |c| format!("  {:.0}% of prompts from cache (10 min)", c * 100.0));
    // a number the engine does not publish is `n/a`, never 0
    let or_na = |key: &str, text: String| if sv.is_na(key) { "n/a".to_string() } else { text };
    let reading = if sv.is_na("prefill_tok_s") { "n/a".to_string() } else { reading };
    let from_probe = if sv.decode_tok_s_from_probe { "  (from the monitor's own C1 test: this engine does not report a speed)" } else { "" };
    let _ = writeln!(o, "SPEED    writing (decode) {}  reading (prefill) {reading}{cached}{from_probe}", or_na("decode_tok_s", format!("{:.1} tok/s", sv.decode_tok_s)));
    // "running" is one fragment, not two placeholders glued with a slash: `n/a/?` read as
    // garbage (E4, 2026-09-20). Unreported = one "n/a"; reported but slots unknown = "N/?".
    let running_frag = if sv.is_na("running") { "n/a".to_string() } else { format!("{:.0}/{}", sv.running, if sv.slots > 0 { sv.slots.to_string() } else { "?".to_string() }) };
    // D1 area, 2026-09-20: SERVE repeated the same decode figure as SPEED without the caveat
    // that it is the monitor's own C1 probe, not the engine's - the two lines must agree.
    let _ = writeln!(o, "SERVE    decode {}{}  running {running_frag}  queue {}  KV {}  TTFT {}  ITL {}  accept {}  cache hit {}",
        or_na("decode_tok_s", format!("{:.1} tok/s", sv.decode_tok_s)), from_probe, or_na("queued", format!("{:.0}", sv.queue)), or_na("kv_usage", pct(sv.kv_usage)),
        or_na("ttft", opt_ms(sv.ttft_avg_ms_10m)), or_na("itl", opt_ms(sv.itl_avg_ms_10m)), or_na("spec", format!("{:.2}", sv.spec_accept_length)), or_na("cache_hit", pct(sv.cache_hit_rate)));
    if !sv.not_reported.is_empty() {
        let names: Vec<&str> = sv.not_reported.iter().filter_map(|k| lss_core::engine::METRIC_FIELDS.iter().find(|(key, _)| key == k).map(|(_, label)| *label)).collect();
        let _ = writeln!(o, "         n/a = not reported by {}: {}. lss's own probe (C1) measures speed and time to first word.", sv.engine_label(), names.join(", "));
    }
    cost_block(&mut o, s, now);
    spending_block(&mut o, s);
    match &s.probe.last_ok {
        Some(p) => {
            if p.unverified {
                // #44 D3, 2026-09-21: probe_run.rs already prefixes UNVERIFIED_NOTE onto
                // `detail` when it stores an unverified reading - prepending it again here
                // printed "could not rule out other traffic: could not rule out other traffic:
                // ...". `detail` is the whole sentence already.
                let _ = writeln!(o, "         {}", p.detail);
            }
            let t = &s.thresholds;
            // #47, 2026-09-21: an hours-old number sitting quietly next to a live clock is the
            // same lie in the other direction as an unproved one presented as proved - say so.
            if now - p.ts >= t.c1_stale_secs {
                let _ = writeln!(o, "         C1 STALE: no valid reading in {} - the server has been too busy to measure", fmt_duration(now - p.ts));
            }
            let _ = writeln!(o, "C1       {} tok/s  TTFT {}  {} ago  baseline {} ({})  alert below {} ({:.0}% x{})  {}",
                p.decode_tok_s.map_or_else(|| "-".to_string(), |v| format!("{v:.1}")), opt_ms(p.ttft_ms), fmt_duration(now - p.ts),
                s.c1_baseline().map_or_else(|| "-".to_string(), |v| format!("{v:.1}")), s.probe.baseline_source,
                s.c1_floor().map_or_else(|| "-".to_string(), |v| format!("{v:.1}")), t.c1_ratio * 100.0, t.c1_consecutive, invalid_label(s));
        }
        None => {
            let _ = writeln!(o, "C1       no valid probe yet ({})  {}", s.probe.baseline_source, invalid_label(s));
        }
    }
    for g in &s.gpus {
        let x = &g.sample;
        let _ = writeln!(o, "GPU{}     {}  {}/{}  {}  util {}  mem {}/{} GiB  fan {}  {}{}",
            x.index, lss_core::units::temp_opt(x.temp_c, lss_core::units::TempStyle::Full), num(x.power_w, ""), num(x.power_limit_w, "W"), num(x.clock_mhz, "MHz"), num(x.util_pct, "%"),
            gib(x.mem_used_mib), gib(x.mem_total_mib), num(x.fan_pct, "%"), throttle_label(&g.throttle),
            if g.thermal_excluded { "  (thermal alerts off)" } else { "" });
    }
    if s.gpus.is_empty() {
        if s.gpu_source == "none" {
            let _ = writeln!(o, "GPUS     stats unavailable: no nvidia-smi, amd-smi, rocm-smi or (macOS) ioreg on this machine");
        } else {
            let _ = writeln!(o, "GPUS     no reading ({} failed)", match s.gpu_source.as_str() { "amd" => "amd-smi / rocm-smi", "apple" => "ioreg", _ => "nvidia-smi" });
        }
    }
    if s.gate.absent {
        let _ = writeln!(o, "GATE     no gateway configured: requests go straight to the engine (lanes, users and rejections need one)");
    } else {
        for (name, l) in [("public", &s.lanes.public), ("trusted", &s.lanes.trusted)] {
            // #31, 2026-09-21: the in-flight budget (and how close it is) was already enforced
            // but never shown - a lane with no in-flight budget (public) still gets a plain
            // token count, never an invented "of 0".
            let inflight = match l.budget_tokens {
                Some(b) if b > 0 => format!("{} of {} tok budget ({:.0}%)", l.inflight_tokens, b, l.inflight_tokens as f64 / b as f64 * 100.0),
                _ => format!("{} tok", l.inflight_tokens),
            };
            let _ = writeln!(o, "LANE     {name:<8} running {:.0}  queued {:.0}  inflight {inflight}  waiters {}  10m: 2xx {} 4xx {} 5xx {}  keys: {}",
                l.running, l.queued, l.waiters, l.codes_10m.c2xx, l.codes_10m.c4xx, l.codes_10m.c5xx, top_keys(l));
        }
        let _ = writeln!(o, "LANE     {}  (the collector's own C1 probes; trusted lane, since the gate started)", probes_label(&s.lanes.trusted));
        let g = &s.gate;
        let _ = writeln!(o, "GATE     {} {}  rejected 413: {}  429: {}",
            if g.up { "UP" } else { "DOWN" }, g.version.as_deref().unwrap_or("-"),
            s.lanes.public.rejected_413 + s.lanes.trusted.rejected_413, s.lanes.public.rejected_429 + s.lanes.trusted.rejected_429);
    }
    let _ = writeln!(o, "FIRING   {}", if s.firing.is_empty() { "none".to_string() } else { s.firing.join(", ") });
    let _ = writeln!(o, "INCIDENTS (7 days): {}", s.incidents.len());
    for i in s.incidents.iter().take(5) {
        let _ = writeln!(o, "  {}", incident_line(i, now));
    }
    let _ = writeln!(o, "ALERTS (last {}):", s.alerts.len().min(5));
    for a in s.alerts.iter().take(5) {
        let _ = writeln!(o, "  {}", alert_line(a));
    }
    o
}

pub fn incidents(s: &Status, now: i64) -> String {
    if s.incidents.is_empty() {
        return "no incidents in the last 7 days\n".into();
    }
    s.incidents.iter().map(|i| incident_line(i, now) + "\n").collect()
}

pub fn alerts(s: &Status) -> String {
    let mut o = format!("firing: {}\n", if s.firing.is_empty() { "none".to_string() } else { s.firing.join(", ") });
    if s.alerts.is_empty() {
        o.push_str("no alerts recorded\n");
    }
    for a in &s.alerts {
        o.push_str(&alert_line(a));
        o.push('\n');
    }
    o
}

pub fn probe(s: &Status, now: i64) -> String {
    let p = &s.probe;
    let t = &s.thresholds;
    let mut o = format!("C1 probe: {}  every {}  baseline {} ({})  alert below {} = {:.0}% of baseline on {} VALID probes in a row\nvalid = engine idle before, TTFT <= {} s, nobody else admitted or running during it; invalid probes are kept here and used nowhere  {}\n",
        if p.enabled { "enabled" } else { "DISABLED" }, fmt_duration(p.interval_s),
        s.c1_baseline().map_or_else(|| "-".to_string(), |v| format!("{v:.1} tok/s")), p.baseline_source,
        s.c1_floor().map_or_else(|| "-".to_string(), |v| format!("{v:.1} tok/s")), t.c1_ratio * 100.0, t.c1_consecutive, t.c1_max_ttft_s, invalid_label(s));
    for r in &p.history {
        let verdict = match (&r.invalid_reason, r.valid) {
            (_, true) => "valid".to_string(),
            (Some(why), false) => format!("INVALID {why}"),
            (None, false) => "INVALID".to_string(),
        };
        let _ = writeln!(o, "{}  {:<12} {:>7}  TTFT {:>8}  {:>9} ago  {:<19} {}",
            fmt_local(r.ts, "%m-%d %H:%M"), r.status, r.decode_tok_s.map_or_else(|| "-".to_string(), |v| format!("{v:.1}")),
            opt_ms(r.ttft_ms), fmt_duration(now - r.ts), verdict, r.detail);
    }
    let o: String = o.lines().map(|l| format!("{}\n", l.trim_end())).collect();
    o
}

/// The slow health facts of every GPU (`lss gpus`): link, ECC, remapped rows, retired pages.
pub fn gpu_health(s: &Status) -> String {
    let mut o = String::from("HEALTH\n");
    let f = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{x:.0}"));
    let b = |v: Option<bool>| v.map_or("-", |x| if x { "YES" } else { "no" });
    for g in &s.gpus {
        match &g.health {
            Some(h) => {
                let problems = h.problems();
                let _ = writeln!(o, "  GPU{}  pstate {}  pcie gen {}/{} width x{}/x{}  ecc corrected {} uncorrected {}  remapped corr {} uncorr {} pending {} failure {}  retired sbe {} dbe {} pending {}  power limit {}W max {}W  clock max {}MHz  mem temp {}  {}",
                    g.sample.index, h.pstate.as_deref().unwrap_or("-"), f(h.pcie_gen), f(h.pcie_gen_max), f(h.pcie_width), f(h.pcie_width_max), f(h.ecc_corrected), f(h.ecc_uncorrected),
                    f(h.remap_correctable), f(h.remap_uncorrectable), b(h.remap_pending), b(h.remap_failure), f(h.retired_sbe), f(h.retired_dbe), b(h.retired_pending),
                    f(h.power_enforced_w), f(h.power_max_limit_w), f(h.clock_max_mhz), lss_core::units::temp_opt(g.sample.mem_temp_c, lss_core::units::TempStyle::Full),
                    if problems.is_empty() { "OK".to_string() } else { format!("PROBLEM: {}", problems.join("; ")) });
            }
            None => {
                let _ = writeln!(o, "  GPU{}  no health read yet", g.sample.index);
            }
        }
    }
    o
}

/// min / avg / max / newest of one series, for the agents' tables.
fn stats(data: &[Option<f64>]) -> Option<(f64, f64, f64, f64)> {
    let v: Vec<f64> = data.iter().flatten().copied().filter(|x| x.is_finite()).collect();
    let last = *v.last()?;
    let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
    Some((lo, v.iter().sum::<f64>() / v.len() as f64, hi, last))
}

fn n(v: f64) -> String {
    if v.abs() >= 1000.0 || v.fract().abs() < 1e-9 { format!("{v:.0}") } else if v.abs() >= 10.0 { format!("{v:.1}") } else { format!("{v:.3}") }
}

/// Plain-text form of a detail page (`lss latency|load|gpus|gateway|rules`): one line per
/// series with `now min avg max` over the range, then the page's tables. Stable labels, no
/// colour, no box drawing.
pub fn page(d: &crate::data::PageData, page: crate::data::PageId, range: &str, now: i64) -> String {
    let mut o = String::new();
    if matches!(page, crate::data::PageId::Alerts | crate::data::PageId::Incidents) {
        // rule states are "now"; the histories below carry their own dates (card #231: same for
        // the split-off INCIDENTS page - it never used the range selector either)
        let _ = writeln!(o, "{}", page.title());
    } else {
        let _ = writeln!(o, "{}  range {range}", page.title());
    }
    if let Some(e) = &d.error {
        let _ = writeln!(o, "WARNING  {e}");
    }
    if let Some(doc) = &d.series {
        let _ = writeln!(o, "SERIES   {} points, step {}, tier {}, from {}", doc.points, fmt_duration(doc.step_s), doc.tier, fmt_local(doc.start_ts, "%m-%d %H:%M"));
        let _ = writeln!(o, "  {:<26} {:>10} {:>10} {:>10} {:>10} {:>7}", "metric", "now", "min", "avg", "max", "points");
        for (name, data) in &doc.series {
            match stats(data) {
                Some((lo, avg, hi, last)) => {
                    // a temperature shows in the configured unit(s): the series is Celsius, and a
                    // `_temp_f` row carries the same numbers in Fahrenheit
                    let units = lss_core::units::temp_units();
                    let is_temp = name.ends_with("_temp_c");
                    if !is_temp || units != lss_core::units::TempUnits::F {
                        let _ = writeln!(o, "  {name:<26} {:>10} {:>10} {:>10} {:>10} {:>7}", n(last), n(lo), n(avg), n(hi), data.iter().flatten().count());
                    }
                    if is_temp && units != lss_core::units::TempUnits::C {
                        let f = lss_core::units::c_to_f;
                        let _ = writeln!(o, "  {:<26} {:>10} {:>10} {:>10} {:>10} {:>7}", name.replace("_temp_c", "_temp_f"), n(f(last)), n(f(lo)), n(f(avg)), n(f(hi)), data.iter().flatten().count());
                    }
                }
                None => {
                    let _ = writeln!(o, "  {name:<26} {:>10} {:>10} {:>10} {:>10} {:>7}", "-", "-", "-", "-", 0);
                }
            }
        }
        if !doc.unknown.is_empty() {
            let _ = writeln!(o, "  never recorded: {}", doc.unknown.join(", "));
        }
    }
    for h in &d.hists {
        match h.summary {
            Some(s) => {
                let _ = writeln!(o, "HIST     {:<11} {:.0} observations  p50 {}  p90 {}  p99 {}  avg {}", h.metric, s.count, opt_ms(Some(s.p50_ms)), opt_ms(Some(s.p90_ms)), opt_ms(Some(s.p99_ms)), opt_ms(Some(s.avg_ms)));
                for (i, c) in h.total.iter().enumerate().filter(|(_, c)| **c > 0.0) {
                    let bound = h.le.get(i).map_or_else(|| "+Inf".to_string(), |b| format!("{b}"));
                    let _ = writeln!(o, "  le {bound:<8} {c:>8.0}  {:>5.1}%", c / s.count.max(1.0) * 100.0);
                }
            }
            None => {
                let _ = writeln!(o, "HIST     {:<11} no requests in this range", h.metric);
            }
        }
    }
    if let Some(g) = &d.gateway {
        let _ = writeln!(o, "GATEWAY  probes excluded {}", g.probes_excluded);
        match (g.codes_coverage.as_str(), g.codes_since) {
            ("partial", Some(t)) => {
                let _ = writeln!(o, "  NOTE     per-key codes since {}: the code columns of key rows count only from then (`coded` of `reqs`); - = not recorded", fmt_local(t, "%m-%d %H:%M"));
            }
            ("none", _) => {
                let _ = writeln!(o, "  NOTE     per-key codes were not recorded in this range: - = not recorded");
            }
            _ => {}
        }
        let _ = writeln!(o, "  {:<8} {:<22} {:>7} {:>7} {:>7} {:>6} {:>6} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10}", "kind", "name", "reqs", "coded", "2xx", "4xx", "5xx", "413", "429", "499", "503", "est_avg", "est_max");
        let row = |o: &mut String, kind: &str, r: &lss_core::series::GatewayRow| {
            let e = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{x:.0}"));
            let c = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |x| x.to_string());
            let _ = writeln!(o, "  {kind:<8} {:<22} {:>7} {:>7} {:>7} {:>6} {:>6} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10}", r.name, r.requests, r.coded_requests, c(r.c2xx), c(r.c4xx), c(r.c5xx), c(r.s413), c(r.s429), c(r.s499), c(r.s503), e(r.est_tokens_avg), e(r.est_tokens_max));
        };
        for r in &g.lanes {
            row(&mut o, "lane", r);
        }
        for r in &g.keys {
            row(&mut o, &format!("key/{}", &r.lane[..1]), r);
        }
        for r in &g.ips {
            let _ = writeln!(o, "  ip       {:<22} {:>7}  {}", r.name, r.requests, r.lane);
        }
    }
    // #231: ALERTS (page 8) and INCIDENTS (page 9) split - both still fetch the one /rules
    // document (`page_paths`), so `d.rules` is populated for either, but each page's plain-text
    // form now only prints ITS OWN half, matching the split TUI pages exactly.
    if let Some(r) = &d.rules {
        let pct = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{:.3}%", x * 100.0));
        if page == crate::data::PageId::Alerts {
            let _ = writeln!(o, "FIRING   {}", if r.firing.is_empty() { "none".to_string() } else { r.firing.join(", ") });
            let _ = writeln!(o, "SPOOL    {}", r.spool_depth.map_or_else(|| "unreadable".to_string(), |d| format!("{d} waiting")));
            let _ = writeln!(o, "RULES    {}", r.rules.len());
            for x in &r.rules {
                let fired = x.last_fired.map_or_else(|| "never".to_string(), |t| fmt_local(t, "%m-%d %H:%M"));
                let cool = if x.cooldown_remaining_s > 0 { fmt_duration(x.cooldown_remaining_s) } else { "-".into() };
                let since = x.pending_since.filter(|_| x.state != "ok").map_or_else(String::new, |t| format!(" for {}", fmt_duration(now - t)));
                let _ = writeln!(o, "  {:<8} {:<26} {:<9} value {}{since}  |  {}  |  fired {fired}  cooldown {cool}", x.state, x.rule, x.severity, x.value, x.threshold);
            }
            let _ = writeln!(o, "ALERTS   last {}", r.alerts.len());
            for a in &r.alerts {
                let _ = writeln!(o, "  {}", alert_line(a));
            }
        }
        if page == crate::data::PageId::Incidents {
            let _ = writeln!(o, "UPTIME   24h {}  7d {}  (since {})", pct(r.uptime.h24), pct(r.uptime.d7), fmt_local(r.uptime.since, "%m-%d %H:%M"));
            let _ = writeln!(o, "INCIDENTS {}", r.incidents.len());
            for i in &r.incidents {
                let _ = writeln!(o, "  {}", incident_line(i, now));
            }
        }
    }
    o.lines().map(|l| format!("{}\n", l.trim_end())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Status {
        serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).unwrap()
    }

    /// card #173: the owner said "i dont see any of the things we talked about in terms of
    /// pricing" - the money existed only in the TUI. These pin that `lss status` carries it AND
    /// carries the same caveats, because a dollar figure without its scope is the confidently
    /// wrong number the whole cost feature was built to avoid.
    #[test]
    fn status_text_carries_the_cost_block_with_the_tui_caveats() {
        let s = golden();
        let text = status(&s, s.generated_at);
        for needle in [
            "COST     plan ",
            "effective ",
            "right now $",
            "/kWh",
            "live draw ",
            "/hour",
            "today ",
            "(since local midnight)",
            "last 24h ",
            "(rolling)",
            "delivery+generation only, as of ",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        // the fixed daily charge is STATED as excluded, or stated as not configured - never
        // silently folded in
        assert!(
            text.contains("excludes $") || text.contains("no fixed daily charge configured"),
            "the fixed-charge scope must be stated:\n{text}"
        );
    }

    /// card #298: the rate's provenance rides on the COST header line when rates.toml states it,
    /// and an older file with no `source` renders exactly as before (no invented label).
    #[test]
    fn cost_block_names_where_the_rate_came_from() {
        let mut s = Status { cost: Some(lss_core::model::CostStatus { rate_name: "California average residential (EIA)".into(), effective_date: "2026-06".into(), is_flat: true, rate_source: "CA avg (EIA 2026-06)".into(), ..Default::default() }), ..Status::default() };
        let mut o = String::new();
        cost_block(&mut o, &s, 1_000_000);
        assert!(o.lines().next().unwrap().contains("rate: CA avg (EIA 2026-06)"), "{o}");
        s.cost.as_mut().unwrap().rate_source.clear();
        let mut o = String::new();
        cost_block(&mut o, &s, 1_000_000);
        assert!(!o.contains("rate:"), "{o}");
    }

    #[test]
    fn cost_tracking_off_says_so_and_never_shows_zero_dollars() {
        // "off" and "$0.00" are different facts and only one of them is true when there is no
        // rates.toml. This is the TUI's own rule (electricity_with_no_rate_table_says_tracking_
        // is_off_not_zero) applied to the plain surface.
        let mut s = golden();
        s.cost = None;
        let text = status(&s, s.generated_at);
        assert!(text.contains("COST     cost tracking is off"), "{text}");
        assert!(!text.contains("$0.00"), "a missing rate table must not read as zero money:\n{text}");
    }

    #[test]
    fn a_partly_covered_window_says_how_much_data_is_behind_the_figure() {
        // card #102's rule, on this surface: "today $0.02 (since local midnight)" at 03:16 on a
        // collector that started at 02:41 reads as three hours of data when it is 35 minutes.
        let mut s = golden();
        if let Some(c) = s.cost.as_mut() {
            c.today_usd = Some(0.02);
            c.today_covered_secs = Some(35 * 60);
            c.last_24h_covered_secs = Some(35 * 60);
        }
        // a `now` well into the day, so the window's nominal length is much larger than 35 min
        let now = s.generated_at - (s.generated_at % 86_400) + 12 * 3600;
        let text = status(&s, now);
        assert!(text.contains("only 35m00s of data so far, not the full window"), "{text}");
        // and the caveat is on its OWN line, never appended to the figure (card #114)
        let line = text.lines().find(|l| l.contains("today $0.02")).expect("a today line");
        assert!(!line.contains("only 35m"), "the caveat must not share the figure's line: {line:?}");
    }

    #[test]
    fn the_json_surface_carries_the_same_cost_fields_the_text_block_prints() {
        // card #173 item 3. `lss status --json` prints the collector's /status document
        // VERBATIM (main.rs one_shot: the `_ =>` arm passes the parsed body straight through),
        // so what the json exposes is exactly what /status carries - this pins that the
        // document really does carry every field the COST block reads, so the two surfaces can
        // never disagree about what exists. A scraper reading the json and a human reading the
        // text are looking at the same numbers.
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).unwrap();
        let cost = raw.get("cost").expect("/status carries a cost block");
        for key in [
            "rate_name",
            "effective_date",
            "is_flat",
            "current_usd_per_kwh",
            "current_period",
            "live_usd_per_hour",
            "today_usd",
            "today_kwh",
            "today_covered_secs",
            "last_24h_usd",
            "last_24h_kwh",
            "last_24h_covered_secs",
            "fixed_usd_per_day",
            "unresolved_usd_per_kwh",
        ] {
            assert!(cost.get(key).is_some(), "/status's cost block is missing {key:?} - the text block reads it");
        }
    }

    /// card #176: the owner asked "how much we are spending everyday, every week, every month".
    /// These pin that each window appears AND that it cannot silently pretend to be longer than
    /// it is - which is the whole reason the block carries coverage at all.
    #[test]
    fn spending_shows_every_window_the_owner_asked_for() {
        let s = golden();
        let text = status(&s, s.generated_at);
        for needle in ["SPENDING today", "yesterday", "this week", "last 7 days", "month to date", "last 30 days", "year to date"] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(text.contains("day(s) of daily history"), "the chart's input is named:\n{text}");
    }

    #[test]
    fn a_three_day_old_month_never_reads_as_a_month_and_refuses_to_project() {
        // his exact concern, item 1: "a month that is 3 days old says so rather than rendering a
        // third of a month as if it were a month"
        let mut s = golden();
        if let Some(sp) = s.spending.as_mut() {
            sp.this_month = lss_core::rates::SpendWindow {
                usd: Some(6.0),
                kwh: Some(15.0),
                covered_secs: 3 * 86_400,
                nominal_secs: 30 * 86_400,   // as if the window claimed a whole month
            };
            sp.month_projection_usd = None;
            sp.projection_note = "too early to project: 3 of 30 days into the month (needs 7)".into();
        }
        let text = status(&s, s.generated_at);
        let line = text.lines().find(|l| l.contains("month to date")).expect("a month-to-date line");
        assert!(line.contains("only 3d") || line.contains("of data so far"), "the short month must say so: {line:?}");
        assert!(text.contains("no month projection: too early to project"), "{text}");
        assert!(text.contains("3 of 30 days"), "and say how short it is:\n{text}");
    }

    #[test]
    fn a_projection_is_shown_only_as_a_projection() {
        let s = golden();   // the golden has a projection
        let text = status(&s, s.generated_at);
        assert!(text.contains("at this rate this month lands near $"), "{text}");
        assert!(text.contains("projected from"), "it must say what it extrapolated from:\n{text}");
    }

    /// card #176 item 5: "every figure carries the same caveats the ELECTRICITY box does". A
    /// reader who scrolls to SPENDING sees only SPENDING - so the exclusions live there too, not
    /// only up in COST. Never the rate VALUE, which is private (card #172): the plan name and the
    /// effective date, exactly what the COST block's own tail line prints.
    #[test]
    fn spending_states_what_its_dollars_exclude_where_the_dollars_are() {
        let s = golden();
        let text = status(&s, s.generated_at);
        let block: String = text
            .lines()
            .skip_while(|l| !l.starts_with("SPENDING"))
            .take_while(|l| l.starts_with("SPENDING") || l.starts_with("         "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(block.contains("delivery+generation only"), "the SPENDING block itself must carry the caveat:\n{block}");
        assert!(block.contains("fixed charge") || block.contains("no fixed daily charge"), "{block}");
        assert!(block.contains("as of "), "and the rate table's effective date:\n{block}");
    }

    #[test]
    fn cost_tracking_off_means_no_spending_block_at_all() {
        // not a block of zeros and not a block of em dashes: the COST line already says tracking
        // is off, and repeating it six times would be noise
        let mut s = golden();
        s.spending = None;
        let text = status(&s, s.generated_at);
        assert!(!text.contains("SPENDING"), "{text}");
        assert!(text.contains("COST     cost tracking is off") || text.contains("COST     plan"), "{text}");
    }

    #[test]
    fn status_text_has_every_section_and_no_escape_codes() {
        let s = golden();
        let text = status(&s, s.generated_at);
        for needle in ["LLM SERVER STATUS  gpu-box  model-a  UP 36m51s", "SERVE ", "C1       191.7 tok/s", "GPU0 ", "GPU3 ", "LANE     public", "LANE     trusted", "GATE     UP v5", "FIRING   none", "INCIDENTS (7 days): 4", "ALERTS"] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(!text.contains('\u{1b}'), "plain output must carry no ANSI escapes");
        assert!(text.contains("key-a 14"));
        assert!(text.contains("LANE     probes: 3 excluded (2 in 10 min)"), "{text}");
        assert!(text.contains("(no key) 15"), "the trusted lane's key count has the probes taken out:\n{text}");
        // the newest probe in the fixture (141.4 tok/s, TTFT 26.5 s) collided with traffic: not shown as THE reading
        assert!(text.contains("C1       191.7 tok/s  TTFT 92.4 ms  2m00s ago  baseline 190.5 (learned)  alert below 152.4 (80% x3)  (1 invalid skipped)"), "{text}");
        assert!(text.contains("(thermal alerts off)"));
    }

    #[test]
    fn down_and_stale_are_loud() {
        let mut s = golden();
        s.serve.up = false;
        s.serve.down_since = Some(s.generated_at - 300);
        let text = status(&s, s.generated_at + 120);
        assert!(text.contains("DOWN 7m00s"), "{text}");
        assert!(text.contains("WARNING  collector data is 2m00s old"));
        assert!(!text.contains("WHY DOWN"), "no reason known: none invented");
        // card #331: a refused key is named on line 2
        s.serve.down_reason = lss_core::engine::auth_refusal_hint("/v1/models: HTTP 401", false);
        let text = status(&s, s.generated_at + 120);
        let line2 = text.lines().nth(1).unwrap_or_default();
        assert!(line2.starts_with("WHY DOWN the engine wants an API key (HTTP 401) - set api_key in its [[engine]] block"), "{text}");
        // and never on an UP document, whatever a stale field says
        s.serve.up = true;
        assert!(!status(&s, s.generated_at + 120).contains("WHY DOWN"));
    }

    /// #47, 2026-09-21: an hours-old C1 number sitting next to a live clock, unflagged, is the
    /// same lie in the other direction as an unproved reading shown as proved - status must say
    /// #44 D3, 2026-09-21: probe_run.rs already writes `detail` as "could not rule out other
    /// traffic: <why>" for an unverified reading (see `UNVERIFIED_NOTE`'s own doc comment) - the
    /// caveat line must not prefix it a second time.
    #[test]
    fn the_unverified_caveat_is_not_printed_twice() {
        let mut s = golden();
        let mut p = s.probe.last_ok.clone().unwrap();
        p.unverified = true;
        p.detail = format!("{}: the engine does not report how many requests are running", lss_core::probe::UNVERIFIED_NOTE);
        s.probe.last_ok = Some(p);
        let text = status(&s, s.generated_at);
        assert_eq!(text.matches("could not rule out other traffic").count(), 1, "{text}");
        assert!(text.contains("could not rule out other traffic: the engine does not report how many requests are running"), "{text}");
    }

    /// so plainly, using the SAME threshold the rule engine alerts on (`c1_stale_secs`).
    #[test]
    fn c1_says_when_it_has_not_measured_in_a_while() {
        let mut s = golden();
        s.thresholds.c1_stale_secs = 600; // a clean, explicit value for this test
        let mut p = s.probe.last_ok.clone().unwrap();
        p.ts = s.generated_at - 599; // just under: quiet
        s.probe.last_ok = Some(p.clone());
        let text = status(&s, s.generated_at);
        assert!(!text.contains("C1 STALE"), "{text}");
        p.ts = s.generated_at - 601; // just over: says so
        s.probe.last_ok = Some(p);
        let text = status(&s, s.generated_at);
        assert!(text.contains("C1 STALE: no valid reading in 10m01s - the server has been too busy to measure"), "{text}");
    }

    /// #31, 2026-09-21: the in-flight budget was already enforced but never shown. Trusted
    /// publishes one (a real "of BUDGET" share); public has none at all (only a per-request
    /// cap, no in-flight ceiling) and must never show an invented "of 0".
    #[test]
    fn lane_shows_inflight_as_a_share_of_its_budget_when_one_is_published() {
        let mut s = golden();
        s.lanes.trusted.inflight_tokens = 48_000;
        s.lanes.trusted.budget_tokens = Some(600_000);
        s.lanes.public.budget_tokens = None;
        let text = status(&s, s.generated_at);
        assert!(text.contains("inflight 48000 of 600000 tok budget (8%)"), "{text}");
        let public_line = text.lines().find(|l| l.starts_with("LANE     public")).unwrap();
        assert!(public_line.contains(&format!("inflight {} tok", s.lanes.public.inflight_tokens)) && !public_line.contains("of"), "{public_line}");
    }

    #[test]
    fn every_page_has_a_plain_text_form_for_agents() {
        use crate::data::PageId;
        let s = golden();
        let now = s.generated_at;
        for p in PageId::ALL {
            let text = page(&crate::demo::page(&s, p, 1, now), p, "1h", now);
            let head = if matches!(p, PageId::Alerts | PageId::Incidents) { format!("{}\n", p.title()) } else { format!("{}  range 1h\n", p.title()) };
            assert!(text.starts_with(&head), "{text}");
            assert!(!text.contains('\u{1b}') && !text.contains('│'), "no escapes, no box drawing");
            assert!(text.lines().all(|l| l == l.trim_end()));
        }
        let lat = page(&crate::demo::page(&s, PageId::Latency, 1, now), PageId::Latency, "1h", now);
        for needle in ["SERIES   60 points, step 1m00s, tier 1m", "ttft_p99_ms", "HIST     ttft", "HIST     queue_time", "  le 0.2 "] {
            assert!(lat.contains(needle), "missing {needle:?} in:\n{lat}");
        }
        let health = gpu_health(&s);
        assert!(health.contains("GPU0  pstate P1  pcie gen 5/5 width x16/x16  ecc corrected 0 uncorrected 0") && health.contains("OK"), "{health}");
        let gw = page(&crate::demo::page(&s, PageId::Gateway, 1, now), PageId::Gateway, "1h", now);
        assert!(gw.contains("lane     public") && gw.contains("key/p    key-a") && gw.contains("  ip       203.0.113.x"), "{gw}");
        // part of the range without per-key codes: a note, `-` for what is unknown, and the coded share
        let mut partial = crate::demo::page(&s, PageId::Gateway, 3, now);
        partial.gateway = Some(crate::demo::gateway_partial(3, now));
        let gw = page(&partial, PageId::Gateway, "24h", now);
        assert!(gw.contains("  NOTE     per-key codes since "), "{gw}");
        let row = |name: &str| gw.lines().find(|l| l.contains(name)).unwrap().split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(row("key-b")[2..11], ["8", "0", "-", "-", "-", "-", "-", "-", "-"].map(String::from), "{gw}");
        assert_eq!(row("key-a")[2..5], ["1920", "14", "8"].map(String::from), "{gw}");
        assert!(row("203.0.113.x").len() == 4, "address rows carry no code columns");
        let mut firing = golden();
        firing.firing = vec!["thermal_temp:gpu2".into()];
        // #231: ALERTS and INCIDENTS are separate pages now, each only carrying its own half.
        let rules = page(&crate::demo::page(&firing, PageId::Alerts, 1, now), PageId::Alerts, "1h", now);
        for needle in ["FIRING   thermal_temp:gpu2", "SPOOL    2 waiting", "  firing   thermal_temp:gpu2", "  pending  queue_pressure", "ALERTS   last 2"] {
            assert!(rules.contains(needle), "missing {needle:?} in:\n{rules}");
        }
        assert!(!rules.contains("UPTIME") && !rules.contains("INCIDENTS"), "ALERTS must not carry INCIDENTS' half:\n{rules}");
        let events = page(&crate::demo::page(&firing, PageId::Incidents, 1, now), PageId::Incidents, "1h", now);
        for needle in ["UPTIME   24h", "INCIDENTS 4"] {
            assert!(events.contains(needle), "missing {needle:?} in:\n{events}");
        }
        assert!(!events.contains("FIRING") && !events.contains("SPOOL") && !events.contains("RULES"), "INCIDENTS must not carry ALERTS' half:\n{events}");
    }

    #[test]
    fn sub_commands() {
        let s = golden();
        let now = s.generated_at;
        let inc = incidents(&s, now);
        assert_eq!(inc.lines().count(), 4);
        assert!(inc.contains("serve_down"));
        assert!(inc.contains("10m50s"));
        assert!(alerts(&s).starts_with("firing: none\n"));
        let p = probe(&s, now);
        assert!(p.contains("baseline 190.5 tok/s (learned)  alert below 152.4 tok/s = 80% of baseline on 3 VALID probes in a row"), "{p}");
        assert!(p.contains("TTFT <= 3 s") && p.contains("(1 invalid skipped)"), "{p}");
        assert!(p.contains("141.4  TTFT   26.5 s      1m00s ago  INVALID slow_ttft"), "{p}");
        assert!(p.contains("INVALID busy_before"), "{p}");
        assert!(p.contains("191.7  TTFT  92.4 ms      2m00s ago  valid"), "{p}");
        assert!(p.contains("skipped_busy"));
    }
}
