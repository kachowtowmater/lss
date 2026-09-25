//! Plain-text reports for the commands added with the USERS / TOKENS / MODEL / ADVICE pages:
//! `lss users | tokens | model | loadouts | compare | advice | bench`. Written for a person who
//! is not an engineer: plain labels, and one line of explanation under each technical term.
//! No colour, no box drawing, stable labels (agents read these too; `--json` is the contract).

use lss_core::advice::{AdviceDoc, Finding};
use lss_core::bench::{BenchDoc, Scorecard};
use lss_core::compare::{Comparison, LoadoutCard, LoadoutsDoc, Mark, CATEGORIES};
use lss_core::loadout::CurveRow;
use lss_core::model::Status;
use lss_core::timeutil::{fmt_duration, fmt_local};
use lss_core::tokens::TokensDoc;
use lss_core::users::UserRow;
use std::fmt::Write;

/// `1.43M`, `772M`, `41k`, `3.1k`, `640`.
pub fn big(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e8 {
        format!("{:.0}M", v / 1e6)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

pub fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{x:.digits$}"))
}

pub fn ago(ts: i64, now: i64) -> String {
    if ts <= 0 {
        "-".into()
    } else {
        format!("{} ago", fmt_duration((now - ts).max(0)))
    }
}

pub fn ms(v: Option<f64>) -> String {
    match v {
        Some(x) if x >= 10_000.0 => format!("{:.0} s", x / 1000.0),
        Some(x) if x >= 1000.0 => format!("{:.1} s", x / 1000.0),
        Some(x) => format!("{x:.0} ms"),
        None => "-".into(),
    }
}

/// `2/4` with its limit, or just the number when the lane has no limit.
pub fn limit_use(r: &UserRow) -> String {
    match r.gate.conc_limit {
        Some(l) => format!("{}/{}", r.gate.inflight, l),
        None => format!("{}/-", r.gate.inflight),
    }
}

pub fn out_tokens(r: &UserRow) -> String {
    format!("{}{}", if r.gate.completion_tokens_exact { "" } else { "~" }, big(r.gate.completion_tokens_24h as f64))
}

pub const USERS_NEEDS_GATE: &str = "per-user numbers need a gateway v5.2 or newer (this one does not publish them yet)";
pub const NO_GATEWAY: &str = "no gateway configured: per-user numbers, lanes and the gateway log need one (see the collector's gate_url)";

/// What using the server is like FOR THIS USER, from the gateway's log of their own requests:
/// how long they wait for the first word and how fast their answers are written. None until
/// the gateway logs it (v5.2) and they have had an answer.
pub fn experience_text(r: &UserRow) -> Option<String> {
    let e = r.experience.as_ref()?;
    Some(format!("their experience: first word half under {}, 95% under {} · answers written at {} tok/s (median request {}) · {} answered in 24h", ms(e.ttft_p50_ms), ms(e.ttft_p95_ms), opt(e.tok_s, 1), opt(e.tok_s_p50, 1), e.requests))
}

pub fn users(s: &Status, now: i64) -> String {
    let mut o = String::new();
    let u = &s.users;
    if s.gate.absent {
        // card #23: NO gateway at all is a different fact from "a gateway exists but is old"
        let _ = writeln!(o, "USERS    {NO_GATEWAY}");
        return o;
    }
    if !u.available {
        let _ = writeln!(o, "USERS    {USERS_NEEDS_GATE}");
        let _ = writeln!(o, "         until then: `lss gateway` has requests per key and per address from the gateway's log");
        return o;
    }
    let _ = writeln!(o, "USERS    active now {}  last 10 min {}  last 24 h {}  slots in use {:.0}/{}  no-key attempts 24h {}", u.active_now, u.totals.users_active_10m, u.totals.users_24h, s.serve.running, s.serve.slots, u.totals.unauthenticated_24h);
    let _ = writeln!(o, "         a \"slot\" is one request the model is working on; {} can run at once", s.serve.slots);
    let _ = writeln!(o, "{:<22} {:<8} {:>5} {:>9} {:>6} {:>7} {:>11}  {:>19} {:>10} {:>10}  last seen", "user", "lane", "now", "peak10m/24h", "limit", "req/min", "req 1h/24h", "ok/rej/err/closed", "prompt 24h", "output 24h");
    for r in &u.rows {
        let g = &r.gate;
        let _ = writeln!(o, "{:<22} {:<8} {:>5} {:>11} {:>6} {:>7} {:>11}  {:>19} {:>10} {:>10}  {}", clip(&r.name, 22), g.lane, g.inflight, format!("{}/{}", g.peak_inflight_10m, g.peak_inflight_24h), limit_use(r), g.rpm_now, format!("{}/{}", g.requests_1h, g.requests_24h), format!("{}/{}/{}/{}", g.ok_24h, g.rejected_24h, g.errors_24h, g.client_closed_24h), format!("~{}", big(g.prompt_tokens_est_24h as f64)), out_tokens(r), ago(g.last_seen as i64, now));
        if let Some(e) = experience_text(r) {
            let _ = writeln!(o, "  {e}");
        }
    }
    if u.rows.is_empty() {
        let _ = writeln!(o, "(nobody has used the server in the last 24 hours)");
    }
    for (label, row) in [("BENCH", &u.bench), ("PROBE", &u.probe)] {
        if let Some(r) = row {
            let _ = writeln!(o, "{label:<8} {} - {} request(s) in 24 h, {} now; shown apart, never counted as a user", r.name, r.gate.requests_24h, r.gate.inflight);
        }
    }
    let _ = writeln!(o, "         prompt tokens are the gateway's estimate (~); output tokens are exact unless marked ~");
    o
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width.saturating_sub(1)).collect::<String>() + "~"
    }
}

/// A 0..=1 share as a percentage that never rounds UP to a false 100 %: 0.995 is `99.5%`, not
/// `100%` ("100% from cache" next to a non-zero reading speed reads as a contradiction).
pub fn share_pct(v: f64) -> String {
    let p = v * 100.0;
    if (99.5..100.0).contains(&p) {
        format!("{:.1}%", (p * 10.0).floor() / 10.0)
    } else if p > 0.0 && p < 0.5 {
        format!("{:.1}%", (p * 10.0).ceil() / 10.0)
    } else {
        format!("{p:.0}%")
    }
}

/// `1.4k`, or `1.4k (since 19:00)` when the request counter is younger than the row's window:
/// a figure never pretends to cover more than it does.
pub fn requests_text(w: &lss_core::tokens::TokenWindow, now: i64) -> String {
    match w.requests_since {
        Some(ts) => format!("{} (since {})", big(w.requests), fmt_local(ts, if now - ts < 86_400 { "%H:%M" } else { "%m-%d %H:%M" })),
        None => big(w.requests),
    }
}

pub fn tokens(d: &TokensDoc, now: i64) -> String {
    let mut o = String::new();
    // card #23: an engine that publishes no token counters (Ollama, LM Studio) is not "zero
    // tokens served" - say n/a instead of a table of zeros. NO gateway configured is a
    // different fact (the ledger still works, from the engine's own counters).
    if d.tokens_not_reported {
        // card #180 gate 3: it used to say the table "would be made-up zeros" and then print it
        let _ = writeln!(o, "TOKENS   n/a (not reported by the engine: no token counters) - token totals, peaks and the all-time ledger cannot be counted here");
    } else {
        let _ = writeln!(o, "TOKENS   how many tokens the server handled (exact, from the engine's own counters)");
        let _ = writeln!(o, "         a token is a piece of a word: about 4 characters of English");
        let _ = writeln!(o, "{:<20} {:>10} {:>10} {:>16}  requests", "window", "written", "read", "from cache");
        for w in &d.windows {
            let name = match w.name.as_str() {
                "all" => format!("all time ({})", fmt_duration(w.secs)),
                n => n.to_string(),
            };
            let _ = writeln!(o, "{name:<20} {:>10} {:>10} {:>16}  {}", big(w.generated), big(w.prompt), w.cache_share.map_or_else(|| "-".to_string(), |c| format!("{} ({})", big(w.cached), share_pct(c))), requests_text(w, now));
        }
        let _ = writeln!(o, "         written = generated by the model; read = prompt tokens; from cache = prompt tokens it did not have to read again");
        let _ = writeln!(o, "         all time counts since {} and survives restarts of the model server", fmt_local(d.all_time_since, "%Y-%m-%d %H:%M"));
        match &d.peak {
            Some(p) => {
                let _ = writeln!(o, "PEAK     {:.0} tok/s written, all users together, on {}", p.tok_s, fmt_local(p.ts, "%Y-%m-%d %H:%M"));
            }
            None => {
                let _ = writeln!(o, "PEAK     nothing generated yet");
            }
        }
        for p in d.peak_by_day.iter().take(7) {
            let _ = writeln!(o, "  {}  {:>6.0} tok/s  at {}", p.day, p.tok_s, fmt_local(p.ts, "%H:%M"));
        }
    }
    for (label, s) in [("ANSWERS", &d.output_len), ("PROMPTS", &d.prompt_len)] {
        match s {
            Some(s) => {
                let _ = writeln!(o, "{label:<8} average {} tokens  ·  half are under {}  ·  9 in 10 under {}  ·  99 in 100 under {}  ({} requests, 7 days)", big(s.avg), big(s.p50), big(s.p90), big(s.p99), big(s.count));
            }
            None => {
                let _ = writeln!(o, "{label:<8} no requests in the last 7 days");
            }
        }
    }
    if d.per_user_available {
        let _ = writeln!(o, "BY USER  (24 h; prompt is the gateway's estimate)");
        for u in &d.per_user {
            let _ = writeln!(o, "  {:<22} {:<8} written {:>8}{}  read ~{:>8}  {:>5} requests  {}", clip(&u.name, 22), u.lane, big(u.output_24h as f64), if u.output_exact { " " } else { "~" }, big(u.prompt_est_24h as f64), u.requests_24h, u.output_share.map_or_else(String::new, |s| format!("{:.0}% of all output", s * 100.0)));
        }
        if d.per_user.is_empty() {
            let _ = writeln!(o, "  (nobody in the last 24 hours)");
        }
    } else if d.gate_absent {
        // card #23: NO gateway at all is a different fact from "a gateway exists but is old"
        let _ = writeln!(o, "BY USER  {NO_GATEWAY}");
    } else {
        let _ = writeln!(o, "BY USER  {USERS_NEEDS_GATE}");
    }
    let _ = now;
    o
}

fn curve_line(r: &CurveRow) -> String {
    format!("  {:>2} at once   each {:>7}   total {:>7}   first word {:>8}   {:>6} samples   {}", r.running, opt(r.per_request_tok_s, 1), opt(r.tok_s, 1), ms(r.ttft_ms), r.samples, r.source)
}

/// The concurrency table of a card: live rows, overridden by the bench where it measured.
pub fn merged_curve(c: &LoadoutCard) -> Vec<CurveRow> {
    lss_core::bench::merge_curve(&c.row.curve, c.speed())
}

pub fn saturation_sentence(c: &LoadoutCard) -> String {
    let sat = lss_core::loadout::saturation_of(&merged_curve(c), c.row.peak_tok_s);
    let mut text = sat.sentence();
    // a ceiling from live traffic says WHAT it was measured on: the same server reads a 470k-token
    // prompt very differently from a 2k one, and the owner cannot see that in a tok/s number
    if let (lss_core::loadout::Saturation::After { from_bench: false, .. }, Some(p50)) = (&sat, c.row.prompt_p50_tokens) {
        text.push_str(&format!(" (typical prompt {} tokens)", big(p50)));
    }
    text
}

pub fn bench_line(doc: Option<&BenchDoc>, now: i64) -> String {
    let Some(b) = doc.map(|d| &d.brief) else { return "BENCH    (the collector did not answer /bench)".into() };
    if b.state == "running" {
        return format!("BENCH    RUNNING {} · step {}/{} {} · started {}", b.profile.as_deref().unwrap_or("?"), b.step_index, b.steps, b.step.as_deref().unwrap_or(""), ago(b.started_at.unwrap_or(0), now));
    }
    if !b.configured {
        return "BENCH    not set up: point `[bench] harness` in the collector's config at llm_decode_bench.py".into();
    }
    let own = if b.builtin { " (built-in mini bench: no external harness is set up)" } else { "" };
    match &b.last {
        Some(l) => format!("BENCH    last run: {} {} {} on {} [{}]{} - `lss bench quick` runs a new one (~5 min on an idle server; `--under-load` measures with the traffic on it and says so)", l.profile, l.verdict(), ago(l.ended_at, now), l.model, l.load.word(), l.aborted.as_ref().map(|a| format!(" ({a})")).unwrap_or_default()),
        None => format!("BENCH    never run on this server - `lss bench quick` measures a new model in ~5 min on an idle server, `lss bench quick --under-load` with the traffic on it{own}"),
    }
}

/// `0: 191.0 · 16k: 168.1 (bench) · 64k: -`: one user's writing speed by how much is already
/// in the conversation. Only the buckets with a number.
pub fn long_context_text(c: &LoadoutCard) -> String {
    let rows: Vec<String> = c.long_context().iter().filter_map(|r| r.tok_s.map(|v| format!("{} in: {v:.0} tok/s ({})", if r.context_tokens == 0 { "nothing".to_string() } else { r.bucket.clone() }, r.source))).collect();
    if rows.is_empty() {
        "not measured yet (lss bench full measures 0 / 16k / 64k / 128k)".into()
    } else {
        rows.join("  ·  ")
    }
}

/// The money line: only with `electricity_usd_per_kwh` configured.
pub fn cost_text(r: &lss_core::loadout::LoadoutRow) -> Option<String> {
    let per_mtok = r.usd_per_mtok?;
    let mut t = format!("${per_mtok:.4} per 1M tokens written");
    if let Some(idle) = r.usd_per_mtok_incl_idle {
        t.push_str(&format!(" (${idle:.4} counting idle time)"));
    }
    if let Some(day) = r.usd_per_day {
        t.push_str(&format!("  ·  ${day:.2} per day"));
    }
    Some(t)
}

pub fn cold_start_text(r: &lss_core::loadout::LoadoutRow) -> String {
    match (r.cold_start_s, r.cold_start_avg_s) {
        (Some(last), Some(avg)) => format!("{} last time · {} on average ({} seen)", fmt_duration(last.round() as i64), fmt_duration(avg.round() as i64), r.cold_starts_seen),
        (Some(last), None) => fmt_duration(last.round() as i64),
        _ => "not seen yet (measured the next time the container starts)".into(),
    }
}

/// `first word within 5 s: 96% of requests (goal 95%)  MISSED`
pub fn target_lines(t: &lss_core::targets::TargetsStatus) -> Vec<(String, bool)> {
    t.rows
        .iter()
        .map(|r| {
            let met = r.met_pct.map_or_else(|| "not measured yet".to_string(), |m| if r.key == "uptime" { format!("{}%", (m * 100.0).round() / 100.0) } else { format!("{m:.0}%") });
            let goal = if r.key == "uptime" { format!("{}%", r.goal_pct) } else { format!("{:.0}%", r.goal_pct) };
            (format!("{:<36} {met} of {} (goal {goal})", r.label, r.basis), r.ok == Some(false))
        })
        .collect()
}

pub fn model(cards: &LoadoutsDoc, bench: Option<&BenchDoc>, targets: Option<&lss_core::targets::TargetsStatus>, now: i64) -> String {
    let mut o = String::new();
    let Some(c) = cards.loadouts.iter().find(|c| c.row.current).or(cards.loadouts.first()) else {
        return "MODEL    no loadout on record yet: the collector records one as soon as it sees the serve up\n".into();
    };
    let r = &c.row;
    let _ = writeln!(o, "MODEL    {} · {} · loadout {} · first seen {} · {} run(s){}", r.model, r.image_tag, r.id, fmt_local(r.first_seen, "%Y-%m-%d %H:%M"), r.runs, if r.current { "" } else { "  (NOT serving now: the last one seen)" });
    if !r.flags.is_empty() {
        let _ = writeln!(o, "         {}", r.flags);
    }
    let _ = writeln!(o, "{}", bench_line(bench, now));

    let _ = writeln!(o, "\nHEADROOM: how many more people it can take");
    match &c.headroom {
        Some(h) => {
            let _ = writeln!(o, "  => {}", h.sentence);
            for ceiling in &h.ceilings {
                let _ = writeln!(o, "     {:<8} {}", ceiling.kind, ceiling.why);
            }
        }
        None => {
            let _ = writeln!(o, "  not enough traffic seen yet to estimate");
        }
    }
    if let Some(t) = targets.filter(|t| !t.rows.is_empty()) {
        let _ = writeln!(o, "\nTARGETS (last {}): the service you asked for, and how often it was met", t.window);
        for (line, missed) in target_lines(t) {
            let _ = writeln!(o, "  {line}{}", if missed { "  MISSED" } else { "" });
        }
        if let Some(note) = t.rows.iter().find_map(lss_core::targets::first_word_note) {
            let _ = writeln!(o, "    {note}");
        }
    }

    let _ = writeln!(o, "\nWRITING SPEED (decode): how fast it produces an answer once it has started");
    let _ = writeln!(o, "  one user alone, server idle   {} tok/s (best {}, {} probes)", opt(r.c1_tok_s, 1), opt(r.c1_best_tok_s, 1), r.c1_probes);
    let _ = writeln!(o, "  real requests                 half run at {} tok/s or faster, 9 in 10 at {} or faster", opt(r.decode_p50_tok_s, 1), opt(r.decode_p90_tok_s, 1));
    let _ = writeln!(o, "  users at once -> speed each gets · total · time to first word   (live = real traffic, bench = lss bench)");
    for row in merged_curve(c) {
        let _ = writeln!(o, "{}", curve_line(&row));
    }
    // card #14: the `bench` rows above all come from one scorecard - state the conditions it was
    // taken under next to them, not only on the scorecard nobody opens
    if let Some(b) = c.speed() {
        let _ = writeln!(o, "  bench rows measured           {}", b.load_sentence());
    }
    let _ = writeln!(o, "  => {}", saturation_sentence(c));
    let _ = writeln!(o, "  why three speeds for \"one user\": alone = lss's own short test question on an idle server; 1 at once = real");
    let _ = writeln!(o, "  requests measured while being written, which the guessing shortcut (speculative decoding) speeds up more on");
    let _ = writeln!(o, "  real text; \"half of real requests\" is timed inside the engine, between tokens, with no network and no waiting");
    let _ = writeln!(o, "  long conversations (one user) {}", long_context_text(c));

    let _ = writeln!(o, "\nREADING SPEED (prefill): how fast it reads a prompt before it starts to answer");
    let _ = writeln!(o, "  prompt tokens read per second {} (peak {})   time to first word: half under {}, 9 in 10 under {}", opt(r.prefill_tok_s, 0), opt(r.prefill_peak_tok_s, 0), ms(r.ttft_p50_ms), ms(r.ttft_p90_ms));
    // #44 D2, 2026-09-21: "peak" is now the same tokens-per-wall-clock-second the plain figure
    // is, just the busiest one-minute window rather than the whole life of the loadout - it
    // used to divide by request-seconds instead (many concurrent reads sum their own seconds
    // past the window's real length), which could inflate it far past anything physically
    // plausible (live: 80x the plain figure with 8 slots).
    let _ = writeln!(o, "  (peak is the busiest one-minute window, not the whole life of the loadout)");
    if let Some(b) = c.speed() {
        for p in &b.prefill {
            let _ = writeln!(o, "  bench: a {:>5}-token prompt   {:>7.0} tok/s   first word after {}", big(p.tokens as f64), p.tok_s, ms(Some(p.ttft_ms)));
        }
    }
    let sized: Vec<String> = r.ttft_by_size.iter().filter(|s| s.n > 0).map(|s| format!("{} {} (n={})", s.bucket, ms(s.avg_ms), s.n)).collect();
    let _ = writeln!(o, "  first word by prompt size     {}", if sized.is_empty() { "needs a gateway v5.2 (it logs prompt size and time to first byte per request)".to_string() } else { sized.join("  ·  ") });

    let _ = writeln!(o, "\nSHORTCUTS WORKING?");
    let _ = writeln!(o, "  speculative decoding          accepts {} tokens per step ({} of its guesses)", opt(r.spec_accept_length, 2), r.spec_accept_rate.map_or_else(|| "-".to_string(), |v| format!("{:.0}%", v * 100.0)));
    let _ = writeln!(o, "    a small helper guesses several tokens ahead and the model checks them in one go: above 1 = free speed");
    let _ = writeln!(o, "  prefix cache                  {} of prompt tokens came from cache", r.cache_hit_share.map_or_else(|| "-".to_string(), share_pct));
    let _ = writeln!(o, "    text it has already read (a system prompt, earlier turns) is not read again");

    let _ = writeln!(o, "\nMEMORY (KV cache): the model's working memory for the conversations in progress");
    let _ = writeln!(o, "  capacity {} tokens  ·  peak use {:.0}%  ·  context per request {} tokens  ·  longest real prompt {}{}", big(r.kv_capacity_tokens), r.kv_peak * 100.0, big(r.context_len), r.prompt_max_tokens.map_or_else(|| "-".to_string(), big), if r.prompt_max_is_bucket_edge { " (at most)" } else { "" });

    let _ = writeln!(o, "\nEFFICIENCY");
    let _ = writeln!(o, "  {} tokens per joule  ·  {} Wh per 1M tokens written  ·  {} W on average while serving", opt(r.tokens_per_joule, 3), opt(r.wh_per_mtok, 0), opt(r.avg_watts_serving, 0));
    // #105: also None whenever a real [rates] table is configured (that engine owns cost now -
    // see page 1 ELECTRICITY) - covers both causes honestly.
    let _ = writeln!(o, "  {} kWh per day{}", opt(r.kwh_per_day, 2), cost_text(r).map_or_else(|| "  ·  cost: not shown here - set electricity_usd_per_kwh, or see page 1 ELECTRICITY if [rates] is configured".to_string(), |c| format!("  ·  {c}")));

    let _ = writeln!(o, "\nRELIABILITY");
    let _ = writeln!(o, "  up {}% of {:.1} h observed  ·  {} restart(s)  ·  errors {}  ·  too busy (429) {}  ·  unavailable (503) {}", opt(r.uptime_pct, 2), r.hours_observed, r.runs.saturating_sub(1), share(r.error_rate), share(r.rate_429), share(r.rate_503));
    let _ = writeln!(o, "  cold start (container start -> ready)  {}", cold_start_text(r));

    let previous = lss_core::compare::resolve("previous", &cards.loadouts).ok().filter(|p| p.row.id != r.id);
    let best = lss_core::compare::resolve("best", &cards.loadouts).ok().filter(|b| b.row.id != r.id);
    for (name, other) in [("previous", previous), ("best", best)] {
        if let Some(other) = other {
            let cmp = lss_core::compare::compare("this", c, name, other);
            let _ = writeln!(o, "\nVS {} ({} · {})", name.to_uppercase(), other.row.model, other.row.image_tag);
            if let Some(why) = &cmp.load_warning {
                let _ = writeln!(o, "  ! the two benchmarks are NOT comparable: {why}");
            }
            for v in &cmp.verdicts {
                let _ = writeln!(o, "  {:<17} {}", v.title, v.sentence);
            }
        }
    }
    o
}

fn share(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{:.2}%", x * 100.0))
}

fn date(ts: Option<i64>) -> String {
    ts.filter(|t| *t > 0).map_or_else(|| "-".to_string(), |t| fmt_local(t, "%m-%d"))
}

pub fn loadouts(d: &LoadoutsDoc, now: i64) -> String {
    if d.loadouts.is_empty() {
        return "no loadout on record yet: the collector records one as soon as it sees the serve up\n".into();
    }
    let mut o = String::new();
    let _ = writeln!(o, "LOADOUTS  newest first  (a loadout = model + image + launch settings; `lss compare A B` puts two side by side)");
    let _ = writeln!(o, "{:<2}{:<13} {:<24} {:<16} {:<11} {:>8}  {:<17} {:<7}  {:>7} {:>12} {:>10} {:>9} {:>8}", "", "id", "model", "image tag", "first seen", "observed", "bench q/f/acc", "load", "C1", "max total", "prefill 8k", "TTFT C1", "accuracy");
    for c in &d.loadouts {
        let h = c.headline();
        // card #14: the condition the bench numbers were taken in travels WITH them, in the same
        // row. A reader who never opens the scorecard must still not read a loaded run as a quiet
        // one; `-` means this row has no bench numbers at all, `?` means they exist and nobody
        // recorded what else was running.
        let load = c.speed().map_or("-", |s| s.load_class().word());
        let _ = writeln!(o, "{:<2}{:<13} {:<24} {:<16} {:<11} {:>8}  {:<17} {:<7}  {:>7} {:>12} {:>10} {:>9} {:>8}", if c.row.current { "*" } else { "" }, c.row.id, clip(&c.row.model, 24), clip(&c.row.image_tag, 16), fmt_local(c.row.first_seen, "%m-%d %H:%M"), format!("{:.1}h", c.row.hours_observed), format!("{}/{}/{}", date(c.last_quick_at), date(c.last_full_at), date(c.last_accuracy_at)), load, opt(h.c1_tok_s, 1), match (h.max_total_tok_s, h.max_total_at) { (Some(t), Some(n)) => format!("{t:.0} @{n}"), _ => "-".into() }, opt(h.prefill_8k_tok_s, 0), ms(h.ttft_c1_ms), h.accuracy.map_or_else(|| "-".to_string(), |a| format!("{:.1}%", a * 100.0)));
    }
    let _ = writeln!(o, "* = serving now  ·  C1 = tok/s for one user alone  ·  max total = best tok/s for everybody together @ how many users");
    let _ = writeln!(o, "load = what else was on the server while the benchmark ran: quiet = nothing (the gold standard) · LOADED = other traffic shared it, the numbers are what the model gave WITH it · ? = not recorded, never read it as quiet · - = no benchmark yet");
    let _ = writeln!(o, "numbers come from `lss bench` where it ran, otherwise from real traffic");
    let _ = now;
    o
}

pub fn mark_text(m: Mark) -> &'static str {
    match m {
        Mark::Same => "~ same (noise)",
        Mark::Better => "A better",
        Mark::Worse => "A WORSE",
        Mark::NoData => "",
        Mark::Incomparable => "NOT COMPARABLE",
    }
}

fn side_line(tag: &str, s: &lss_core::compare::Side) -> String {
    format!("  {tag}  {:<9} {} · {} · id {} · {}", s.selector, s.model, s.image_tag, s.id, match (&s.bench_profile, s.bench_at) { (Some(p), Some(at)) => format!("bench {p} {} · {}", fmt_local(at, "%m-%d %H:%M"), s.bench_load_detail), _ => "no bench yet (live numbers only)".to_string() })
}

pub fn comparison(c: &Comparison) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "COMPARE  {} (A)  vs  {} (B)      differences within +-{:.0}% are run-to-run noise", c.a.selector, c.b.selector, c.noise_pct);
    let _ = writeln!(o, "{}\n{}", side_line("A", &c.a), side_line("B", &c.b));
    if let Some(why) = &c.load_warning {
        let _ = writeln!(o, "\n  ! THE BENCHMARK NUMBERS ARE NOT COMPARABLE: {why}.\n    They are still shown; the difference between them is not, because it would not mean what it looks like. Rows measured from real traffic are unaffected.");
    }
    for (key, title) in CATEGORIES {
        let rows: Vec<_> = c.rows.iter().filter(|r| r.category == key).collect();
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n  {:<40} {:>10} {:>10} {:>9}", title.to_uppercase(), "A", "B", "A vs B");
        for r in rows {
            let unit = if r.unit.is_empty() { String::new() } else { format!(" ({})", r.unit) };
            let delta = r.delta_pct.map_or_else(|| "-".to_string(), |d| format!("{d:+.1}%"));
            let _ = writeln!(o, "    {:<38} {:>10} {:>10} {:>9}  {:<15}{}", format!("{}{unit}", r.label), opt(r.a, 1), opt(r.b, 1), delta, mark_text(r.mark), if r.source.is_empty() { String::new() } else { format!(" [{}]", r.source) });
        }
    }
    let _ = writeln!(o, "\nVERDICT");
    for v in &c.verdicts {
        let _ = writeln!(o, "  {:<17} {}", v.title, v.sentence);
    }
    o
}

pub fn finding_line(f: &Finding) -> String {
    format!("  {:<5}  {}{}", f.severity.as_str().to_uppercase(), f.sentence, if f.points_at.is_empty() { String::new() } else { format!("  [{}]", f.points_at) })
}

pub fn advice(d: &AdviceDoc, window: &str) -> String {
    let mut o = String::new();
    let Some(w) = d.windows.iter().find(|w| w.stats.name == window).or(d.windows.first()) else {
        return "ADVICE   nothing yet: the collector works it out every 5 minutes once it has data\n".into();
    };
    let span = match w.stats.name.as_str() {
        "24h" => "the last 24 hours",
        "7d" => "the last 7 days",
        _ => "the last 30 days",
    };
    let _ = writeln!(o, "ADVICE   evidence from {span} ({} of data). ACT = costing you now · WATCH = look again · FINE = measured, nothing to do", fmt_duration(w.stats.covered_secs));
    let _ = writeln!(o, "         lss never changes a setting: these are reasons, the decision is yours. Every rule: docs/ADVICE.md");
    if w.findings.is_empty() {
        let _ = writeln!(o, "  (not enough data in this window to say anything yet)");
    }
    for f in &w.findings {
        let _ = writeln!(o, "{}", finding_line(f));
    }
    let excluded: Vec<String> = w.stats.throttle.iter().filter(|g| g.excluded).map(|g| format!("GPU{} {:.1}%", g.index, g.pct)).collect();
    if !excluded.is_empty() {
        let _ = writeln!(o, "         (throttle time of GPUs left out of the advice on purpose: {})", excluded.join(", "));
    }
    o
}

pub fn scorecard(s: &Scorecard) -> String {
    let mut o = String::new();
    // card #334: the verdict counts the checks (a bare OK over a failed check was a lie), and a
    // short run keeps its tenths of a second
    let _ = writeln!(o, "BENCH RUN {}  {}  {}  {}  took {}{}", s.run_id, s.profile, s.model, s.verdict(), s.took(), s.aborted.as_ref().map(|a| format!("\n  {a}")).unwrap_or_default());
    // card #14: FIRST, before any number - what the server was doing while these were taken.
    // Always printed, including when the answer is "nobody recorded it": silence about the
    // conditions reads as "it was quiet", and that is the one reading that must never be free.
    let _ = writeln!(o, "  BACKGROUND      {}", s.load_sentence());
    if !s.note.is_empty() {
        let _ = writeln!(o, "  note: {}", s.note);
    }
    if !s.decode.is_empty() {
        let _ = writeln!(o, "  WRITING SPEED   users at once · context -> each (tok/s) · total (tok/s) · first word");
        for c in &s.decode {
            let _ = writeln!(o, "    {:>2} · {:>6}   {:>7.1}   {:>7.1}   {}", c.concurrency, big(c.context as f64), c.per_user_tok_s, c.total_tok_s, ms(c.ttft_ms));
        }
    }
    for p in &s.prefill {
        let _ = writeln!(o, "  READING SPEED   a {:>5}-token prompt: {:.0} tok/s, first word after {}", big(p.tokens as f64), p.tok_s, ms(Some(p.ttft_ms)));
    }
    for c in &s.sanity {
        let _ = writeln!(o, "  SANITY          {:<12} {}  {}", c.name, if c.pass { "pass" } else { "FAIL" }, c.detail);
    }
    for n in &s.needle {
        let _ = writeln!(o, "  NEEDLE          {:>2}% deep in {} tokens: {}  ({:.0} s) {}", n.depth_pct, big(n.prompt_tokens as f64), if n.pass { "found" } else { "NOT FOUND" }, n.secs, n.detail);
    }
    for a in &s.accuracy {
        let _ = writeln!(o, "  ACCURACY        {}: {:.1}% ({} of {})", a.dataset, a.score * 100.0, a.correct, a.n);
    }
    if let Some(g) = &s.garbled {
        let _ = writeln!(o, "  GARBLED OUTPUT  {} per 100k characters ({} broken characters, {} stray CJK in {} written) - 0 is what a healthy model gives", opt(Some(g.per_100k), 1), g.replacement_chars, g.cjk_chars, big(g.chars as f64));
    }
    let _ = writeln!(o, "  SERVER SIDE     accept length {} · KV capacity {} tokens · {} tok/J · {} Wh per 1M tokens", opt(s.accept_length, 2), s.kv_tokens.map_or_else(|| "-".to_string(), big), opt(s.tokens_per_joule, 3), opt(s.wh_per_mtok, 0));
    let _ = writeln!(o, "  harness {} {} · raw results in {}", if s.harness_version.is_empty() { "?" } else { &s.harness_version }, s.harness_commit, s.raw_dir);
    o
}

pub fn bench_status(d: &BenchDoc, now: i64) -> String {
    let mut o = bench_line(Some(d), now) + "\n";
    for r in d.runs.iter().take(8) {
        let h = r.headline();
        let _ = writeln!(o, "  run {:<4} {:<9} {:<8} {}  {:<22} {:<7} C1 {:>6} · max total {:>6} · prefill 8k {:>6}{}", r.run_id, r.profile, r.status, fmt_local(r.started_at, "%m-%d %H:%M"), clip(&r.model, 22), r.load_class().word(), opt(h.c1_tok_s, 1), opt(h.max_total_tok_s, 0), opt(h.prefill_8k_tok_s, 0), r.aborted.as_ref().map(|a| format!("  ({a})")).unwrap_or_default());
    }
    let _ = writeln!(o, "  quiet = nothing else was on the server · LOADED = measured with other traffic on it · ? = not recorded (never read as quiet)");
    o
}

/// `lss maintenance status` (card #22, 2026-09-21): plain-English state of the planned window.
pub fn maintenance_status(s: &Status, now: i64) -> String {
    let m = &s.maintenance;
    if !m.active {
        return "no maintenance window is open\n".to_string();
    }
    format!(
        "maintenance window OPEN: {}\n  started {} ({} ago) - auto-expires {} unless stopped first (lss maintenance stop)\n",
        m.reason,
        fmt_local(m.started_at, "%m-%d %H:%M"),
        fmt_duration(now - m.started_at),
        if m.expires_at > now { format!("in {}", fmt_duration(m.expires_at - now)) } else { "any moment now".into() },
    )
}

#[cfg(test)]
mod tests {

    /// card #180 gate 3: `lss tokens` on an engine with no token counters said the table "would
    /// be made-up zeros" and then printed it. It now prints the n/a line and no table.
    #[test]
    fn the_scorecard_headline_counts_a_failed_check_and_keeps_tenths_of_a_second() {
        // card #334: what `lss bench quick` prints first
        use lss_core::bench::{Check, Scorecard};
        let s = Scorecard { run_id: 1, profile: "quick".into(), model: "m".into(), status: "ok".into(), duration_s: 0, duration_ms: Some(412), sanity: vec![Check { name: "json output".into(), pass: false, detail: "not JSON".into() }], ..Default::default() };
        let first = scorecard(&s).lines().next().unwrap().to_string();
        assert_eq!(first, "BENCH RUN 1  quick  m  OK, BUT 1 CHECK FAILED: json output  took 0.4s");
    }

    #[test]
    fn the_bench_line_says_a_failed_check_like_the_scorecard_headline() {
        // card #338: `lss` printed `BENCH    last run: quick ok ...` over a failed json check
        use lss_core::bench::{BenchBrief, BenchDoc, Check, LastRun, Scorecard};
        let now = 1_790_000_000;
        let s = Scorecard { run_id: 1, profile: "quick".into(), model: "m".into(), status: "ok".into(), ended_at: now - 60, sanity: vec![Check { name: "json output".into(), pass: false, detail: "not JSON".into() }], ..Default::default() };
        let doc = BenchDoc { brief: BenchBrief { state: "idle".into(), configured: true, last: Some(LastRun::of(&s)), ..Default::default() }, ..Default::default() };
        let line = bench_line(Some(&doc), now);
        assert!(line.starts_with("BENCH    last run: quick ok, but 1 check failed: json output "), "{line}");
    }

    #[test]
    fn tokens_with_no_engine_counters_prints_no_table_of_zeros() {
        let mut d = crate::demo::tokens(1_790_000_000);
        d.tokens_not_reported = true;
        let out = tokens(&d, 1_790_000_000);
        assert!(out.contains("n/a (not reported by the engine"), "{out}");
        // the zero table's header, its all-time row and the peak (all from the engine's counters)
        assert!(!out.contains("from cache") && !out.contains("all time (") && !out.contains("PEAK"), "{out}");
    }
    use super::*;

    /// card #14: the one mistake this must never allow is a measurement taken under load being
    /// read as a clean one. Checked on the RENDERED text of every surface a scorecard reaches,
    /// not on the struct: a field nobody prints protects nobody.
    #[test]
    fn a_loaded_run_can_never_be_mistaken_for_a_quiet_one_on_any_screen() {
        use lss_core::bench::{BackgroundLoad, LoadClass, LOAD_SRC_USERS};
        let now = 1_790_000_000;
        let mut cards = crate::demo::loadouts(now);
        let loaded = BackgroundLoad { samples: 150, polls_with_traffic: 150, concurrent_avg: Some(1.8), concurrent_max: Some(4.0), source: LOAD_SRC_USERS.into(), queue_avg: 0.4, queue_max: 3.0, engine_tok_s: Some(690.0) };
        if let Some(q) = cards.loadouts[0].quick.as_mut() {
            q.background = Some(loaded.clone());
            q.under_load = true;
        }
        cards.loadouts[0].full = None;
        assert_eq!(cards.loadouts[0].speed().unwrap().load_class(), LoadClass::Loaded);

        let l = loadouts(&cards, now);
        let serving = |text: &str| text.lines().find(|l| l.starts_with('*')).expect("the serving loadout's row").to_string();
        assert!(serving(&l).contains("LOADED") && !serving(&l).contains("quiet"), "the row shouts it:\n{l}");
        assert!(l.contains("LOADED = other traffic shared it"), "and the table says what the word means:\n{l}");
        let card = scorecard(cards.loadouts[0].quick.as_ref().unwrap());
        assert!(card.contains("BACKGROUND      UNDER LOAD: 1.80 other request(s)") && card.contains("150 of 150 looks"), "{card}");

        // and the comparison refuses to subtract it from the quiet run next to it
        let c = lss_core::compare::compare("current", &cards.loadouts[0], "previous", &cards.loadouts[1]);
        let text = comparison(&c);
        assert!(text.contains("THE BENCHMARK NUMBERS ARE NOT COMPARABLE"), "{text}");
        assert!(text.contains("NOT COMPARABLE"), "every bench row is marked:\n{text}");
        assert!(text.contains("UNDER LOAD:") && text.contains("quiet:"), "each side states its own condition:\n{text}");
        let speed = c.verdicts.iter().find(|v| v.category == "speed_alone").unwrap();
        assert!(speed.sentence.starts_with("cannot be compared:"), "{}", speed.sentence);

        // a scorecard from before this existed reads as UNKNOWN, never as quiet
        let mut old = cards.clone();
        old.loadouts[0].quick.as_mut().unwrap().background = None;
        let text = loadouts(&old, now);
        assert!(serving(&text).contains(" ?") && !serving(&text).contains("quiet"), "an unrecorded run is not a quiet one:\n{text}");
        assert!(scorecard(old.loadouts[0].quick.as_ref().unwrap()).contains("NOT recorded: do not read this as quiet"));
    }

    #[test]
    fn maintenance_status_reads_plainly_open_or_closed() {
        let mut s = Status::default();
        assert_eq!(maintenance_status(&s, 1_000), "no maintenance window is open\n");
        s.maintenance = lss_core::maintenance::MaintenanceState { active: true, reason: "gate v5.2 swap".into(), started_at: 1_000, expires_at: 1_000 + 900 };
        let text = maintenance_status(&s, 1_300);
        assert!(text.contains("gate v5.2 swap") && text.contains("5m00s ago") && text.contains("in 10m00s"), "{text}");
    }

    #[test]
    fn numbers_read_like_a_person_would_say_them() {
        assert_eq!([640.0, 3_100.0, 41_000.0, 1_430_000.0, 772_018_048.0, 3.2e9].map(big), ["640", "3.1k", "41k", "1.43M", "772M", "3.20B"]);
        assert_eq!((ms(Some(143.3)), ms(Some(2_460.0)), ms(Some(71_000.0)), ms(None)), ("143 ms".into(), "2.5 s".into(), "71 s".into(), "-".into()));
        assert_eq!((opt(Some(5.876), 2), opt(None, 1), ago(0, 50), ago(10, 100)), ("5.88".into(), "-".into(), "-".into(), "1m30s ago".into()));
        // a share never rounds up to a false 100 % (or down to a false 0 %)
        assert_eq!([0.9951, 0.9999, 1.0, 0.452, 0.004, 0.0].map(share_pct), ["99.5%", "99.9%", "100%", "45%", "0.4%", "0%"]);
    }

    #[test]
    fn every_report_renders_the_demo_data_and_explains_its_terms() {
        let s = crate::demo::status();
        let now = s.generated_at;
        let u = users(&s, now);
        assert!(u.contains("active now") && u.contains("acme") && u.contains("lss-bench") && u.contains("never counted as a user"), "{u}");
        assert!(u.contains("their experience: first word half under") && u.contains("answers written at") && u.contains("answered in 24h"), "each user's own TTFT and speed, WITH the window label (card #72):\n{u}");
        let t = tokens(&crate::demo::tokens(now), now);
        assert!(t.contains("all time") && t.contains("a token is a piece of a word") && t.contains("BY USER") && t.contains("PEAK"), "{t}");
        let cards = crate::demo::loadouts(now);
        let m = model(&cards, Some(&crate::demo::bench(now)), Some(&s.targets), now);
        for heading in ["HEADROOM: how many more people it can take", "more simultaneous users", "TARGETS (last 24h)", "MISSED", "long conversations (one user)", "cold start (container start -> ready)", "kWh per day", "WRITING SPEED (decode)", "READING SPEED (prefill)", "SHORTCUTS WORKING?", "MEMORY (KV cache)", "EFFICIENCY", "RELIABILITY", "VS PREVIOUS", "total speed stops growing after"] {
            assert!(m.contains(heading), "`{heading}` missing from:\n{m}");
        }
        assert!(m.contains(" bench") && m.contains(" live"), "the source of every concurrency row is marked:\n{m}");
        let l = loadouts(&cards, now);
        assert!(l.lines().count() >= 5 && l.contains("* = serving now"), "{l}");
        // card #14: the condition the bench numbers were taken under is IN the table, with a key
        assert!(l.contains("load") && l.contains("quiet") && l.contains("never read it as quiet"), "{l}");
        let c = lss_core::compare::compare("current", &cards.loadouts[0], "previous", &cards.loadouts[1]);
        let text = comparison(&c);
        assert!(text.contains("~ same (noise)") && text.contains("A better") && text.contains("A WORSE") && text.contains("VERDICT"), "{text}");
        assert_eq!(text.lines().filter(|l| CATEGORIES.iter().any(|(_, t)| l.trim_start().starts_with(t))).count(), CATEGORIES.len(), "one verdict line per category:\n{text}");
        let a = advice(&crate::demo::advice(now), "7d");
        assert!(a.contains("ACT") && a.contains("FINE") && a.contains("lss never changes a setting"), "{a}");
        assert!(advice(&AdviceDoc::default(), "7d").contains("nothing yet"));
        let b = bench_status(&crate::demo::bench(now), now);
        assert!(b.contains("last run: quick ok"), "{b}");
        assert!(scorecard(&crate::demo::bench(now).runs[0]).contains("SANITY"));
        assert!(scorecard(&crate::demo::bench(now).runs[0]).contains("BACKGROUND      quiet:"), "{}", scorecard(&crate::demo::bench(now).runs[0]));
        assert!(b.contains("quiet"), "the run list marks what each run was measured under:\n{b}");
        let full = crate::demo::bench(now).runs.into_iter().find(|r| r.garbled.is_some()).expect("the demo has a full run");
        assert!(scorecard(&full).contains("GARBLED OUTPUT  0.0 per 100k characters"), "{}", scorecard(&full));
        // a gate older than v5.2: said plainly, never a table of zeros
        let mut old = s.clone();
        old.users = Default::default();
        assert!(users(&old, now).contains("gateway v5.2") && !users(&old, now).contains("active now"));
    }
}

