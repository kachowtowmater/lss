//! #51 built this as a SECOND overview reachable by `v` (`App.dash`, default false: the owner
//! opts in, never opted in for him - same shape as card #48's `c` chart toggle), the old one
//! still one press away. #73, 2026-09-22 (a real product redirect, not a tweak, the owner's own words): "i dont
//! know how much of this will really help me understand loading an llm and watching users and
//! tokens ... how much electric am i spending per hour based on kwh from gpu ... how much did it
//! cost me ... how much am i paying per 1m token based on my electric". The #51 page answered an
//! SRE's questions (is it healthy, is it saturated, who is refusing work); the owner wants an
//! OWNER's questions instead, in this exact order: what is loaded -> what is it costing me right
//! now -> $/1M tokens -> tokens by window -> users' tokens/energy/dollars -> GPUs (he likes them
//! as they are). ADMIT/SPEED/WORK/EVENTS - all "is it healthy" framing - are gone from this page:
//! the card's own DONE list names exactly six things and nothing else, and the owner said
//! plainly he will go to page 8 (ALERTS) for alerts, so this page keeps only ONE line of
//! headline state (up/down, firing count) instead of a boxed EVENTS section. The original
//! overview (`v` off, the default, never opted in for him) is UNCHANGED - this file only touches
//! the page he has to explicitly ask for.
//!
//! SPACING is still a requirement, not polish: ONE grid, ONE label-column width (`LABEL_W`, used
//! by every section), NO boxes nested inside boxes. GPUS and WHO are ONE flat table each.
//!
//! ## Card #191, 2026-09-22: this page is the DEFAULT now, and the accounting of what #73 cut
//!
//! The owner compared both pages and picked this one, but that choice only ever reached his own
//! git-ignored `~/.config/lss/ui.json`; the compiled default was still the classic grid, so every
//! stranger who installed lss landed on the page he had rejected. `App::dash` now defaults to
//! true and `v` is the way back to the classic grid.
//!
//! Making it the default means the content #73 removed can no longer be waved off as "it is only
//! the opt-in page". So, exactly, section by section - what the CLASSIC grid or #51's version of
//! this page had that this page does not:
//!
//! - **#51's SPEED** -> **SERVE** (#175). TTFT p50/p90/p99, prefill, decode total AND per
//!   request, ITL, running/queue, KV, accept, cache hit. `last token` rides LOADOUT's status row
//!   (#124). Only SPEED's TTFT *decomposition* (avg queue-wait + avg prefill-compute) is gone,
//!   and deliberately: `running` carries queue-wait p50/p99, which is the same question answered
//!   with percentiles instead of an average of an average.
//! - **#51's ADMIT** -> **LANES** (#177) for the gateway half, **SERVE**'s `running` row for
//!   the engine half, **LOADOUT**'s `KV free` row (#124) for the KV half. NOT carried over: the
//!   one-word admission VERDICT. That is not a silent loss - it is card **#123**, open, filed
//!   exactly for this; restoring it here would collide with that card's own work.
//! - **#51's WORK** -> **TOKENS**, which #73 widened from 1h/24h/all-time to hour/day/week/month
//!   and #181 extended again. Its `all time` row was the one thing that did NOT survive, and
//!   #191 puts it back (see `tokens`) - the windows structurally cannot answer "since the engine
//!   came up", and the counter needs no rollup history to read.
//! - **#51's EVENTS** -> **ALERTS** + **INCIDENTS** (#177), which is the panel's own rule made
//!   structural: an alert is a live predicate that clears itself, an incident is a dated fact.
//! - **The classic grid's ADVICE box** is the one thing genuinely NOT on this page, and it is a
//!   ruling, not an omission: the #51 panel (Zheng / Kwon / Husain, unanimous) moved the ADVICE
//!   prose to its own page - `9`, reachable from this page's tab strip and its `1-9` keys - and
//!   kept only its one load-bearing NUMBER, the prefix-cache eviction rate, which now sits in
//!   SERVE's `KV` row. Prose that repeats what the numbers beside it already say is what made
//!   the first impression "a bit lacking" in the first place.
//!
//! Card #75 built the pricing engine this page is built on (`lss_core::rates`, `s.cost`); #73
//! adds the rolling-24h figure (`s.cost.last_24h_*`, distinct from `today_*` which is since local
//! midnight - the USERS section needs the SAME window its per-user token table uses) and the
//! TOKENS section's day/week/month windows (`s.tokens_by_window`, from the collector's own
//! rollup tables - `tokens_run.rs` on the collector side).

use super::widgets::{chart_rows, dim, draw_box, fit, fit_line, frame, good, red, render_lines, title_line, warn, ChartOpts, Series};
use super::{footer_at, pick_shape, App, Shape};
use crate::data::PageId;
use lss_core::model::{Status, WorkWindow};
use crate::money::{coverage_note, usd, usd_per_kwh, usd_precise};
use lss_core::timeutil::fmt_duration;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::buffer::Buffer;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

/// Every section's label column is this wide, so a value in one box lines up under one in
/// another even though they are different boxes - "one grid", not several boxes each doing their
/// own thing.
pub const LABEL_W: usize = 16;

/// CARD #196: THE TWO PLACES THIS PAGE IS ALLOWED TO TURN SEVERITY INTO COLOUR.
///
/// #179 gave every reading one score on one scale and one band word. It did not finish the job on
/// screen: a dozen sections still compared a number against a threshold spelled in the renderer
/// and picked red or amber themselves, which is a SECOND scale - the duplication #179 exists to
/// delete, and the reason "the texts looks all off". Every colour decision on page 1 now arrives
/// through one of these two, so a reading's band and the ink it gets can never disagree.
///
/// An INFORMATIONAL reading is never coloured as a fault however high it scores: cost, a chosen
/// power cap and a model id are facts about the setup, and pulse's own note says each of them
/// outscored every real problem on an idle machine.
///
/// THE ONE TABLE, and the one deliberate change of appearance this card makes. Page 1 already had
/// two mappings of the same bands: the verdict line painted `High` AMBER, while the GPU boxes
/// painted it RED. Both cannot be right, and verifier-2 left the choice open on #179 after
/// measuring that a GPU box turned red ~2.6 C BEFORE its alert fired. RED IS RESERVED FOR
/// `Critical` AND `Offline`; `High` and `Watch` are amber. That is the verdict line's own mapping,
/// it keeps "RED only on a problem, and nothing else" literally true, and at a 90 C alert line it
/// moves a GPU box from red-2.6-C-early to amber-2.6-C-early and red 2 C past the line.
fn band_colour(r: &lss_core::readings::Reading) -> Option<Color> {
    use lss_core::readings::Band;
    if r.informational {
        return None;
    }
    match r.band() {
        Band::Critical | Band::Offline => Some(Color::Red),
        Band::High | Band::Watch => Some(Color::Yellow),
        _ => None,
    }
}

fn band_style(r: &lss_core::readings::Reading) -> Style {
    match band_colour(r) {
        Some(Color::Red) => red(),
        Some(_) => warn(),
        None => Style::default(),
    }
}

/// A band rendered BOLD, for a line that has to be read before any number (the verdict line, the
/// status row).
fn band_style_bold(r: &lss_core::readings::Reading) -> Style {
    let s = band_style(r);
    if s == Style::default() {
        s
    } else {
        s.add_modifier(Modifier::BOLD)
    }
}

/// A box's frame colour, from the worst reading inside it, through the SAME table. `calm` is the
/// colour the box wears when nothing in it is a problem - each section keeps its own, so a healthy
/// page does not go monochrome.
fn section_colour(rs: &[lss_core::readings::Reading], calm: Color) -> Color {
    lss_core::readings::rank(rs).into_iter().find_map(band_colour).unwrap_or(calm)
}

/// The label cell: EXACTLY `LABEL_W` columns wide and always ending in at least one space.
///
/// #199: `format!("{label:<LABEL_W$}")` pads a short label but does nothing to a long one, so a
/// label of `LABEL_W` characters or more pushed its own value right and ran straight into it -
/// which is what "month at this rate" (18) did to the ELECTRICITY box's projection figure. A row
/// whose label comes from a READING (`r.label`) is written in lss-core, a crate away from this
/// grid, so "keep every label under 16" is not a rule the renderer can rely on being kept. The
/// cell now holds the grid itself: an over-long label is cut and marked, never allowed to move
/// the value column. `wrap` also identifies a label row by this exact width, so an overrun
/// silently cost the row its hanging indent as well.
fn fit_label(label: &str) -> String {
    if label.chars().count() < LABEL_W {
        return format!("{label:<LABEL_W$}");
    }
    let cut: String = label.chars().take(LABEL_W - 2).collect();
    format!("{:<LABEL_W$}", format!("{}\u{2026}", cut.trim_end()))
}

/// `label` left-padded to `LABEL_W`, then the value. The one row-building primitive every
/// section uses, so the alignment is structural, not a convention to remember.
fn row(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(fit_label(label), dim())];
    spans.extend(value);
    Line::from(spans)
}

fn row_text(label: &str, value: String) -> Line<'static> {
    row(label, vec![Span::raw(value)])
}

/// #114, 2026-09-22 (verifier-3): its OWN line, never appended as a suffix to the value line
/// above it - a narrow pane's truncation must cut into the confident dollar figure first, never
/// silently drop the caveat while the figure it qualifies survives intact (measured: at 63
/// columns the old single-line version truncated to "...(since local midnight) - on…", losing
/// exactly the caveat and keeping the number). Pushes nothing when coverage is full.
fn push_coverage_line(lines: &mut Vec<Line<'static>>, covered_secs: Option<i64>, nominal_secs: i64) {
    let note = coverage_note(covered_secs, nominal_secs);
    if !note.is_empty() {
        // #175: the caveat sits UNDER its value column (an empty label cell), so the grid holds
        lines.push(Line::from(vec![Span::raw(" ".repeat(LABEL_W)), Span::raw(note)]).style(dim()));
    }
}

/// #305 (the owner on v1.1.2: "the bars on the temp doesnt really show me anything ... are we
/// able to make it like the other charts i see in gpus tab 3"): page 1's 1h trends are the GPUS
/// page's own chart widget (`widgets::chart_rows`, the rows `draw_chart` draws) - a y-scale with
/// its top and bottom labelled, a `-1h ... now` time axis and a legend naming the trend - and
/// they honour `c` (dots/lines) like every other chart. `width` is the box's inner width.
#[derive(Clone, Copy)]
struct Trend {
    width: usize,
    lines: bool,
}

/// Rows per trend: 2 plot rows (top and bottom of the scale labelled), the time axis, the legend -
/// the widget's own floor for a chart with axes, because page 1's no-scroll bars (#247/#269/#291)
/// leave no room to spare.
const TREND_H: usize = 4;

impl Trend {
    fn chart(self, label: &str, data: &[Option<f64>], fmt: fn(f64) -> String, (y_min, y_max): (Option<f64>, Option<f64>), peak: bool) -> Vec<Line<'static>> {
        let opts = ChartOpts { fmt, y_min, y_max, x_left: "-1h".into(), markers: vec![], peak };
        chart_rows(self.width, TREND_H, &[Series::new(label, Color::Blue, &hold_samples(data))], &opts, self.lines)
    }
}

/// The widest spacing between two samples `hold_samples` will ever bridge, in slots of the embedded
/// hour's 30 s grid: 4 = 2 minutes. The collector samples every other slot (60 s); anything past
/// 2 minutes with no sample is a real outage and must show as one (card #315).
const MAX_HOLD_SLOTS: usize = 4;

/// #305's verifier FAIL (lss-inst-v1): the embedded hour is a 30 s GRID, but the collector samples
/// less often than that, so a CONTINUOUS hour arrives as `v, None, v, None, ...` and the chart
/// widget drew every empty slot as a gap - claiming missing data that was not missing. Each sample
/// is held across its own time span: a run of empty slots no longer than the series' usual
/// distance between samples (the most common one) takes the sample before it. A longer run - the
/// collector really was down - stays a gap, as does anything before the first sample.
/// card #315: and never a spacing wider than `MAX_HOLD_SLOTS` - a sampling rate is a few grid
/// steps, so two (or twenty) equal 14-minute gaps are outages however often they repeat.
fn hold_samples(data: &[Option<f64>]) -> Vec<Option<f64>> {
    let at: Vec<usize> = data.iter().enumerate().filter(|(_, v)| v.is_some()).map(|(i, _)| i).collect();
    let mut counts = std::collections::BTreeMap::new();
    for d in at.windows(2).map(|w| w[1] - w[0]) {
        *counts.entry(d).or_insert(0usize) += 1;
    }
    // the most common spacing; on a tie the SHORTER one, so a gap is never bridged on a guess.
    // card #309: and only a spacing seen at least TWICE - two samples in the hour give one
    // spacing, seen once, which is not a sampling rate; bridging it painted an outage over.
    let stride = counts.iter().filter(|(d, n)| **n >= 2 && **d <= MAX_HOLD_SLOTS).max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0))).map_or(1, |(d, _)| *d);
    let mut out = data.to_vec();
    for w in at.windows(2) {
        if w[1] - w[0] <= stride {
            for slot in &mut out[w[0] + 1..w[1]] {
                *slot = data[w[0]];
            }
        }
    }
    out
}

struct Section {
    title: String,
    colour: Color,
    lines: Vec<Line<'static>>,
}

/// #118, 2026-09-22 (verifier-2): the SGLang priority label the gate stamps per lane - real
/// values from the collector's own config, not a guess. `""` only when the collector predates
/// this field entirely (an old collector's document has no key for it, and `#[serde(default)]`
/// gives the same empty string a NEW collector would send for an unset value - the two cases
/// read the same on purpose: either way there is nothing real to show).
fn priority_text(s: &Status) -> String {
    match (s.serve.public_priority.as_str(), s.serve.trusted_priority.as_str()) {
        ("", "") => "\u{2014} not published by this collector yet (needs an update)".to_string(),
        (public, trusted) => format!("public {} \u{b7} trusted {}", if public.is_empty() { "\u{2014}" } else { public }, if trusted.is_empty() { "\u{2014}" } else { trusted }),
    }
}

/// LOADOUT (asked twice: "what flags and what things are turned on to load", "what was the
/// loadouts"). `s.serve.model` is already the served id read from `/v1/models` at scrape time -
/// never a client-supplied field. `s.loadout` (`None` = nothing seen serving yet) says so plainly
/// instead of showing zeros for a loadout that may not exist. `priority` (#118) is the real
/// `public_priority`/`trusted_priority` from the collector's own config, published on /status.
fn loadout(s: &Status, now: i64, trend: Trend) -> Section {
    let Some(l) = &s.loadout else {
        let mut lines = vec![row_text("host", s.host.clone()), Line::styled("no loadout recorded yet (the collector has not seen the serve container up)", dim())];
        lines.push(status_line(s));
        return Section { title: "LOADOUT".into(), colour: loadout_colour(s), lines };
    };
    let age_days = ((now - l.first_seen).max(0)) / 86_400;
    let mut lines = vec![
        row_text("host", s.host.clone()),
        row_text("served id", s.serve.model.clone().unwrap_or_else(|| "(no model)".into())),
        row_text("image tag", if l.image_tag.is_empty() { "\u{2014} no tag on this image".into() } else { l.image_tag.clone() }),
        row_text("flags", if !s.serve.page1_fields_deployed { "\u{2014} needs a collector update, not deployed yet".into() } else if l.flags.is_empty() { "\u{2014} none of the tracked flags are set for this loadout".into() } else { l.flags.clone() }),
        row_text("flags age", format!("unchanged {age_days}d")),
        // card #124, panel 2026-09-22 (Kwon/Huyen/Husain - unanimous on this one): the old row
        // showed the POOL SIZE, a capacity fact rather than a state, and the earlier panel had
        // called it "the most misleading number on the page". FREE headroom replaces it in the
        // SAME slot, so the page costs no extra rows. KEPT HONEST: this is free-of-pool from
        // kv_used/kv_max. Kwon's correction is that radix-EVICTABLE tokens are free-on-demand
        // too, so this figure UNDERSTATES real headroom, and the honest KV-short signal is
        // num_retracted_reqs as a rate - neither gauge is published by the collector yet, so
        // this row says what it can prove and no more.
        row_text("KV free", kv_free_text(s)),
    ];
    // #227 item 4 (the owner: "add in some small graphs ... e.g. ... KV use"): the embedded hour
    // `s.series.kv_usage` (already on every /status document) right under the row it trends.
    // #305: drawn by the GPUS page's own chart widget (see `trend`), pinned 0..100%.
    if s.series.kv_usage.iter().any(|v| v.is_some()) {
        lines.extend(trend.chart("KV 1h", &s.series.kv_usage, super::overview::axis_pct01, (Some(0.0), Some(1.0)), false));
    }
    lines.push(row_text("run limit", if s.serve.slots > 0 { format!("{} slots", s.serve.slots) } else { "not known yet".into() }));
    lines.push(row_text("priority", priority_text(s)));
    // uptime + restarts share one row: LOADOUT already has 9 rows above it, and the box
    // height cap (`layout()`, 12 rows = 2 border + 10 content) silently drops anything past
    // the 10th - this keeps `status_line` below always on screen rather than losing a line
    // nobody would notice was missing.
    lines.push(row_text("uptime", format!("{} \u{b7} {} restarts today", uptime_text(s), s.serve.restarts_today)));
    lines.push(status_line(s));
    Section { title: "LOADOUT".into(), colour: loadout_colour(s), lines }
}

/// #196: LOADOUT holds the `status` row, so its frame is the worst of what that row says - an
/// engine that is DOWN must not sit inside a calm blue box. The model id and the launch flags
/// are `informational` readings and contribute nothing, which is the point of the flag: they
/// show, they sort, and they never make the page look like something is wrong.
fn loadout_colour(s: &Status) -> Color {
    let mut rs = lss_core::readings::loadout(s);
    rs.extend(lss_core::readings::serve(s, s.generated_at).into_iter().filter(|r| r.key == "serve.up" || r.key == "serve.silent"));
    rs.extend(lss_core::readings::events(s, s.generated_at).into_iter().filter(|r| r.key.starts_with("alert.")));
    section_colour(&rs, Color::Blue)
}

/// card #124: KV FREE, not the pool size. An em dash (never `0`) when the collector has not
/// published the pool - Husain's rule from the panel: a silent zero is how the inert-charging
/// bug hid for 2,809 requests, so an absent number must LOOK absent.
/// card #180 gate 3: "down" is a statement about the engine, and a clean install against Ollama
/// with no docker showed "uptime down" beside "status up". No uptime reading is not "down".
fn uptime_text(s: &Status) -> String {
    match s.serve.uptime_s {
        Some(u) => fmt_duration(u),
        None if !s.serve.up => "down".to_string(),
        None if s.serve.container.is_none() => "\u{2014} no container to read a start time from (no docker)".to_string(),
        None => "\u{2014} not known yet".to_string(),
    }
}

fn kv_free_text(s: &Status) -> String {
    if let Some(na) = s.serve.na("kv_usage") {
        return na;
    }
    if s.serve.kv_max_tokens <= 0.0 {
        return "\u{2014} not published by this collector yet".to_string();
    }
    let free = (s.serve.kv_max_tokens - s.serve.kv_used_tokens).max(0.0);
    format!(
        "{} tokens free of {} ({:.0}% used)",
        crate::report::big(free),
        crate::report::big(s.serve.kv_max_tokens),
        s.serve.kv_usage * 100.0
    )
}

/// card #124: the ENGINE FRESHNESS STAMP, and the panel's reason for putting it on page 1 rather
/// than the alerts page, in Husain's words: "every number on that page is a lie if the engine
/// emitted nothing for 90s - this isn't defying the page-1 redesign, it's the freshness label
/// his own page needs". Kwon ranked it FIRST of the card's four items by minutes saved on the
/// next incident: monotonic growth here with the GPUs busy IS what Xid-8 looks like from
/// outside, and three hangs in, nothing on any screen showed it.
///
/// Deliberately NOT health framing: no verdict word, dim while the engine is merely idle (nobody
/// is asking it for tokens), coloured only when requests ARE running and nothing is coming out.
/// Card #196: the colour is the `serve.silent` reading's band, not two seconds-thresholds spelled
/// here. That reading already carries the corroboration this row was reaching for by hand - it
/// exists only while requests ARE running, so an idle engine is dim without needing a `busy`
/// test - and its ramp (10 s of jitter to 90 s of hang) is the one the verdict line ranks on.
fn last_token_span(s: &Status, rs: &[lss_core::readings::Reading]) -> Span<'static> {
    let Some(secs) = s.serve.secs_since_last_token else {
        return Span::styled("last token \u{2014}", dim());
    };
    let busy = s.serve.running > 0.0;
    let silent = rs.iter().find(|r| r.key == "serve.silent");
    let style = silent.map_or_else(dim, |r| if band_style(r) == Style::default() { dim() } else { band_style_bold(r) });
    Span::styled(format!("last token {}{}", fmt_duration(secs), if busy { "" } else { " (idle)" }), style)
}

/// The one line of headline state the card asked to keep on the overview ("up/down, firing
/// count") after ADVICE/INCIDENTS/ALERTS/LANES all leave it - the alerts page (`8`) owns the rest.
/// Card #196: `up`, the freshness stamp and the firing count all take their ink from the readings
/// that already judge them (`serve.up`, `serve.silent`, and the worst `alert.*` row), so this line
/// cannot disagree with the verdict line one row above it about how bad the same fact is.
fn status_line(s: &Status) -> Line<'static> {
    let up = s.serve.up;
    let serve_rs = lss_core::readings::serve(s, s.generated_at);
    let up_style = lss_core::readings::rank(&serve_rs)
        .into_iter()
        .find(|r| r.key == "serve.up")
        .map_or_else(good, band_style_bold);
    let events = lss_core::readings::events(s, s.generated_at);
    let worst_alert = lss_core::readings::rank(&events).into_iter().find(|r| r.key.starts_with("alert."));
    let firing = match worst_alert {
        None => Span::styled("no alerts firing", dim()),
        Some(r) => Span::styled(format!("{} firing - see page 8 ALERTS", s.firing.len()), band_style_bold(r)),
    };
    // card #124: the freshness stamp rides this existing line, so it costs no row.
    row("status", vec![
        Span::styled(if up { "up" } else { "down" }, up_style),
        Span::raw(" \u{b7} "),
        last_token_span(s, &serve_rs),
        Span::raw(" \u{b7} "),
        firing,
    ])
}

/// SERVE (#175, the owner on the new page 1: "i feel like alot of the token info is missing. like
/// ttft, prefill, decode, you skipped alot of useful display"). The numbers he reads to know the
/// server is behaving, back BESIDE the cost, not instead of it: TTFT, reading (prefill) and
/// writing (decode) speed, total AND per request, inter-token latency, what is running and
/// waiting, KV, speculative accept and the prefix-cache hit rate. Every one of them either shows a
/// real number or an em dash that says why - never a 0 standing in for "not reported".
/// `vw` is the width the VALUE column has; each row picks the longest wording that fits it, and
/// the wrap in `draw_section` catches anything that still does not.
fn serve(s: &Status, vw: usize, trend: Trend) -> Section {
    let sv = &s.serve;
    let big = crate::report::big;
    let ms = |v: f64| crate::plain::opt_ms(Some(v));
    let pick = |variants: Vec<String>| -> String {
        let last = variants.last().cloned().unwrap_or_default();
        variants.into_iter().find(|v| v.chars().count() <= vw).unwrap_or(last)
    };
    // #196: every colour in this box is a band from these, never a threshold spelled below
    let rs = lss_core::readings::serve(s, s.generated_at);
    let find = |k: &str| rs.iter().find(|r| r.key == k);
    let mut lines = Vec::new();
    if let Some(down) = find("serve.up") {
        lines.push(row("engine", vec![Span::styled("DOWN - the numbers below are its last readings", band_style_bold(down))]));
    }
    let lat = sv.latency.as_ref();
    let window = lat.map_or(600, |l| l.window_s);
    let win = if window % 60 == 0 { format!("{} min", window / 60) } else { format!("{window}s") };

    let ttft = match lat.and_then(|l| l.ttft) {
        Some(t) => pick(vec![format!("p50 {} \u{b7} p90 {} \u{b7} p99 {} (last {win})", ms(t.p50_ms), ms(t.p90_ms), ms(t.p99_ms)), format!("p50 {} \u{b7} p90 {} \u{b7} p99 {}", ms(t.p50_ms), ms(t.p90_ms), ms(t.p99_ms)), format!("{} / {} / {}", ms(t.p50_ms), ms(t.p90_ms), ms(t.p99_ms))]),
        None if sv.is_na("ttft") => sv.na("ttft").unwrap_or_default(),
        None => sv.ttft_avg_ms_10m.map_or_else(|| "\u{2014} no request finished its first token in the last 10 min".to_string(), |v| format!("avg {} (10 min) \u{b7} percentiles need a newer collector", ms(v))),
    };
    lines.push(row_text("TTFT", ttft));

    let prefill = match (sv.prefill_tok_s, sv.prefill_tok_s_typical) {
        _ if sv.is_na("prefill_tok_s") => sv.na("prefill_tok_s").unwrap_or_default(),
        (Some(v), _) => pick(vec![
            format!("{} tok/s total (10 min) \u{b7} per request \u{2014} the engine reports reading speed in aggregate only", big(v)),
            format!("{} tok/s total \u{b7} per request \u{2014} aggregate only", big(v)),
            format!("{} tok/s total", big(v)),
        ]),
        (None, Some(t)) => pick(vec![format!("\u{2014} nothing read in the last 10 min \u{b7} typical for this loadout {} tok/s", big(t)), format!("\u{2014} idle \u{b7} typical {} tok/s", big(t))]),
        (None, None) => format!("\u{2014} {}", sv.no_reading_reason()),
    };
    lines.push(row_text("prefill", prefill));

    let decode = if let Some(na) = sv.na("decode_tok_s") {
        na
    } else if sv.decode_tok_s_from_probe {
        pick(vec![format!("{:.1} tok/s from the monitor's own C1 test - this engine does not report it", sv.decode_tok_s), format!("{:.1} tok/s (C1 test, not from the engine)", sv.decode_tok_s)])
    } else {
        let per = match &sv.live_decode {
            Some(l) if l.per_request_tok_s.is_some() => {
                let p = l.per_request_tok_s.unwrap_or(0.0);
                vec![format!(" \u{b7} {p:.1} tok/s each at {} in flight ({} samples)", l.running, l.samples), format!(" \u{b7} {p:.1} each at {} in flight", l.running), format!(" \u{b7} {p:.1}/req")]
            }
            _ => vec![" \u{b7} per request \u{2014} this concurrency not observed yet".to_string(), " \u{b7} per req \u{2014}".to_string()],
        };
        let head = format!("{:.1} tok/s total", sv.decode_tok_s);
        pick(per.into_iter().map(|p| format!("{head}{p}")).chain([head.clone()]).collect())
    };
    lines.push(row_text("decode", decode));
    // #227 item 4 (the owner: "add in some small graphs ... e.g. ... decode tok/s"): the same
    // embedded hour `s.series.decode_tok_s` the classic overview already draws (overview.rs) -
    // no new fetch, just page 1's own copy of an existing trend. #305: the GPUS page's own chart
    // widget (see `trend`), its axis pinned at 0 like every rate chart on the THROUGHPUT page.
    if s.series.decode_tok_s.iter().any(|v| v.is_some()) {
        lines.extend(trend.chart("decode 1h", &s.series.decode_tok_s, super::overview::axis_number, (Some(0.0), None), false));
    }

    let itl = match lat.and_then(|l| l.itl) {
        Some(h) => pick(vec![format!("p50 {} \u{b7} p90 {} \u{b7} p99 {} between tokens", ms(h.p50_ms), ms(h.p90_ms), ms(h.p99_ms)), format!("p50 {} \u{b7} p90 {} \u{b7} p99 {}", ms(h.p50_ms), ms(h.p90_ms), ms(h.p99_ms))]),
        None if sv.is_na("itl") => sv.na("itl").unwrap_or_default(),
        None => sv.itl_avg_ms_10m.map_or_else(|| "\u{2014} no tokens generated in the last 10 min".to_string(), |v| format!("avg {} (10 min)", ms(v))),
    };
    lines.push(row_text("ITL", itl));

    let running = if let Some(na) = sv.na("running") {
        vec![Span::styled(na, dim())]
    } else {
        let slots = if sv.slots > 0 { format!("{:.0}/{} slots", sv.running, sv.slots) } else { format!("{:.0} (slots not known)", sv.running) };
        // #196: `serve.slots` exists only when the engine's own admission verdict says every slot
        // is busy, and `serve.queue` ramps to the owner's configured queue line - both of them
        // judgements this row used to make again, differently, from `s.thresholds` directly.
        let slot_style = find("serve.slots").map_or_else(Style::default, band_style);
        let q_style = find("serve.queue").map_or_else(Style::default, band_style);
        let wait = lat.and_then(|l| l.queue_time).map(|h| format!(" \u{b7} wait p50 {} \u{b7} p99 {}", ms(h.p50_ms), ms(h.p99_ms)));
        let mut v = vec![Span::styled(slots, slot_style), Span::raw(" \u{b7} queue "), Span::styled(format!("{:.0}", sv.queue), q_style)];
        if let Some(w) = wait.filter(|w| spans_width(&v) + w.chars().count() <= vw) {
            v.push(Span::raw(w));
        }
        v
    };
    lines.push(row("running", running));

    let kv = if let Some(na) = sv.na("kv_usage") {
        na
    } else {
        let evicted = sv.evicted_tok_per_hour_10m.map_or_else(|| "evicted \u{2014} not enough samples yet".to_string(), |e| format!("{}/hour evicted from the prefix cache", big(e)));
        pick(vec![format!("{} used \u{b7} {evicted}", crate::plain::pct(sv.kv_usage)), format!("{} used", crate::plain::pct(sv.kv_usage))])
    };
    lines.push(row_text("KV", kv));

    let accept = if sv.is_na("spec") {
        "n/a - no speculative decoding reported".to_string()
    } else {
        format!("{:.2} tokens per step ({:.0}% of drafts accepted)", sv.spec_accept_length, sv.spec_accept_rate * 100.0)
    };
    lines.push(row_text("accept", pick(vec![accept, format!("{:.2} tok/step", sv.spec_accept_length)])));

    // #196: the JUDGED number leads this row. `serve.cache_hit` now scores the ten-minute
    // token-weighted share (rates, not counters); the engine's own snapshot swings to 0% on one
    // cold batch, so it follows as context rather than fronting a row whose colour and ranking
    // come from the other number. Leading with the figure the page does NOT judge is how a reader
    // ends up arguing with the verdict line.
    let cache = if let Some(na) = sv.na("cache_hit") {
        na
    } else {
        let engine = format!("{} engine-wide, its last prompt read", crate::plain::pct(sv.cache_hit_rate));
        match sv.cached_share_10m {
            Some(v) => pick(vec![
                format!("{} of the last 10 min's prompt tokens \u{b7} {engine}", crate::plain::pct(v)),
                format!("{} last 10 min \u{b7} {} engine-wide", crate::plain::pct(v), crate::plain::pct(sv.cache_hit_rate)),
                format!("{} last 10 min", crate::plain::pct(v)),
            ]),
            None => pick(vec![engine, format!("{} engine-wide", crate::plain::pct(sv.cache_hit_rate))]),
        }
    };
    lines.push(row_text("cache hit", cache));
    Section { title: "SERVE".into(), colour: section_colour(&rs, Color::Blue), lines }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// ELECTRICITY, LIVE (the owner: "how much electric am i spending per hour based on kwh from gpu
/// ... how much did it cost me and how much did it cost me in dollar amount"). All from `s.cost`
/// (card #75's pricing engine) - `None` means cost tracking is simply off (no `[rates] path`
/// configured), never a guessed rate.
fn electricity(s: &Status, now: i64) -> Section {
    let Some(c) = &s.cost else {
        return Section {
            title: "ELECTRICITY".into(),
            colour: Color::Yellow,
            lines: vec![Line::styled("\u{2014} cost tracking is off (no ~/.config/lss/rates.toml - see packaging/rates.toml.example)", dim())],
        };
    };
    let plan = if c.is_flat { format!("{} (flat)", c.rate_name) } else { c.rate_name.clone() };
    let since_midnight = (now - lss_core::timeutil::local_midnight(now)).max(0);
    let mut lines = vec![
        row_text("plan", format!("{plan} \u{b7} effective {}", c.effective_date)),
        row_text("right now", format!("{} \u{b7} {}", usd_per_kwh(c.current_usd_per_kwh), if c.is_flat { "flat".to_string() } else { c.current_period.clone() })),
        row_text("live draw", format!("{}/hour", usd(c.live_usd_per_hour, "no live GPU power reading"))),
        row_text("today", format!("{} \u{b7} {} kWh (since local midnight)", usd(c.today_usd, "no priced samples yet today"), c.today_kwh.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v:.2}")))),
    ];
    // card #298: where the rate came from ("CA avg (EIA 2026-06)"), its own row so a narrow pane's
    // truncation cannot eat it; only when rates.toml states one, so an older file renders as before
    if !c.rate_source.is_empty() {
        lines.insert(1, row_text("rate from", c.rate_source.clone()));
    }
    // #114 (verifier-3): its own line, never appended to the (possibly long) value line above - a
    // narrow pane's truncation must cut into the confident dollar figure before it ever touches
    // this caveat, not the other way around.
    push_coverage_line(&mut lines, c.today_covered_secs, since_midnight);
    lines.push(row_text("last 24h", format!("{} \u{b7} {} kWh (rolling)", usd(c.last_24h_usd, "no priced samples in the last 24h"), c.last_24h_kwh.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v:.2}")))));
    push_coverage_line(&mut lines, c.last_24h_covered_secs, 86_400);
    // card #181: the WEEK and the MONTH. The owner asked for "everyday, every week, every month"
    // (card #176) and page 1 IS the money page - but until now the week and the month existed
    // only in `lss status` and on page 5, so he had to press a key to see the question he
    // actually asks. Rows in THIS box, deliberately not a seventh box: a new section on this page
    // pushes the page's own content off a short pane (card #124, and I hit it again on page 5
    // this hour). Values and labels come from lss_core::readings::cost - #179's rule that a
    // string has ONE owner - and the caveats use this box's own coverage renderer, which every
    // other row here already uses, so the grid stays one grid.
    if let Some(sp) = &s.spending {
        let money = lss_core::readings::cost(s);
        let row_of = |key: &str| money.iter().find(|r| r.key == key).cloned();
        // card #226: + the YEAR to date ("for electric u need to add month a year to date"), and
        // the month row now reads "month to date" - both labels come from the readings
        for (key, win) in [("cost.week", &sp.this_week), ("cost.month", &sp.this_month), ("cost.ytd", &sp.this_year)] {
            if let Some(r) = row_of(key) {
                lines.push(row_text(&r.label, r.value_or_dash().to_string()));
                push_coverage_line(&mut lines, Some(win.covered_secs), win.nominal_secs);
                // a year the stored history does not reach back through says where it starts,
                // under the value column like every other caveat in this box
                if key == "cost.ytd" {
                    let jan1 = format!("{}-01-01", lss_core::timeutil::fmt_local(now, "%Y"));
                    if let Some(first) = sp.year_first_day.as_deref().filter(|f| *f > jan1.as_str()) {
                        lines.push(Line::from(vec![Span::raw(" ".repeat(LABEL_W)), Span::raw(format!("since {first} - the stored history does not reach Jan 1"))]).style(dim()));
                    }
                }
            }
        }
        if let Some(r) = row_of("cost.month_projection") {
            match sp.month_projection_usd {
                Some(_) => lines.push(row_text(&r.label, r.value_or_dash().to_string())),
                // no projection is not a blank row: card #176 always carries the reason it
                // refused, and a refusal a reader cannot see reads as "nothing to say"
                None => lines.push(Line::from(vec![Span::raw(" ".repeat(LABEL_W)), Span::raw(format!("no month projection: {}", sp.projection_note))]).style(dim())),
            }
        }
    }
    let excludes = c.fixed_usd_per_day.map_or_else(|| "no fixed daily charge configured".to_string(), |v| format!("excludes ${v:.2}/day fixed charge"));
    lines.push(Line::styled(format!("delivery+generation only, as of {} - {excludes}", c.effective_date), dim()));
    // #247 (the zero-scroll bar): "may or may not belong on top of the numbers above" moved to
    // `?` help - the figure and the fact that it is unapplied both stay on the box, the longer
    // qualifier does not.
    if let Some(u) = c.unresolved_usd_per_kwh {
        lines.push(Line::styled(format!("+${u:.4}/kWh unresolved - not applied"), dim()));
    }
    // #196: the frame goes through the SAME table as every other box, over the SAME rows this
    // box shows. It comes out Yellow not because it is hardcoded but because every cost reading
    // is `informational` - a fact about the setup, which shows and sorts and can never be scored
    // as a fault. Mutating that flag off turns this box red, which is the guard made visible.
    Section { title: "ELECTRICITY".into(), colour: section_colour(&lss_core::readings::cost(s), Color::Yellow), lines }
}

/// COST PER 1M TOKENS (the owner: "how much am i paying per 1m token based on my electric") - the
/// figure that makes models, quants and slot counts comparable. Same window as ELECTRICITY's
/// "today" row, on purpose - never mixed with a different one.
///
/// card #174 (the owner: "i think the cost per 1m token is off" - it was: the old headline divided
/// ALL today's energy by GENERATED tokens only, and on a live hour this engine did ~10x more
/// real work reading (uncached prefill) than writing, so the number was inflated by roughly that
/// factor). The PRIMARY line is now real work (uncached prefill + generated), on WORK energy only
/// (standby excluded, shown on its own line below) - the basis is named on screen because every
/// choice here is defensible and none is obvious. `generated`/`prompt` stay as secondary detail:
/// real, correctly documented, just not the headline.
fn cost_per_million(s: &Status) -> Section {
    let Some(c) = &s.cost else {
        return Section { title: "COST PER 1M TOKENS".into(), colour: Color::Magenta, lines: vec![Line::styled("\u{2014} cost tracking is off", dim())] };
    };
    // #227 ("alot of wasted space"): both explanatory lines (what "real work" is measured
    // against, and that generated+prompt do not sum to a total) moved to `?` help - this box is
    // the numbers alone now, same shrink every other page-1 box got.
    // #247 (the zero-scroll bar): generated+prompt combined onto one row (like the GPU cards'
    // own temp/power) - the owner's own ask, "compress COST PER 1M".
    let lines = vec![
        row_text("real work", format!("{} per 1M tok", usd_precise(c.today_usd_per_million_real_work_tokens, "no real-work tokens priced yet today"))),
        row_text("gen/prompt", format!("{} \u{b7} {} per 1M tok", usd_precise(c.today_usd_per_million_generated_tokens, "\u{2014}"), usd_precise(c.today_usd_per_million_prompt_tokens, "\u{2014}"))),
        row_text("standby", usd(c.today_standby_usd, "no standby samples yet today")),
        Line::styled(format!("today, as of {}", c.effective_date), dim()),
    ];
    // #196: same table, same guard as ELECTRICITY - money is a fact, not a severity.
    Section { title: "COST PER 1M TOKENS".into(), colour: section_colour(&lss_core::readings::cost(s), Color::Magenta), lines }
}

/// TOKENS: generated / prompt / cached, at hour / day / week / month (`s.tokens_by_window`,
/// card #73 - all four from the collector's own rollup tables since #100). A window that is
/// `None` says WHY (no rollup stored yet for that resolution at all), never a fabricated 0; one
/// that exists but does not yet cover its own nominal length says so too (#102), rather than let
/// "day" and "week" read as identical full windows on a collector that has only been up minutes.
fn tokens_row(lines: &mut Vec<Line<'static>>, label: &str, w: Option<WorkWindow>, covered_secs: Option<i64>, nominal_secs: i64) {
    let big = crate::report::big;
    // #175: one aligned table (columns under the header below), not a sentence per window - the
    // same four numbers now sit in the same four character positions in every row.
    lines.push(row_text(label, w.map_or_else(|| "\u{2014} not enough rollup history yet".to_string(), |w| format!("{:>8}{:>9}{:>10}{:>9}", big(w.prompt), big(w.cached), big(w.generated), big(w.requests)))));
    // #114: its own line - see `push_coverage_line`'s own doc comment for why.
    push_coverage_line(lines, covered_secs, nominal_secs);
}

fn tokens(s: &Status) -> Section {
    let Some(t) = &s.tokens_by_window else {
        return Section { title: "TOKENS".into(), colour: Color::Cyan, lines: vec![Line::styled("\u{2014} needs a collector update, not deployed yet", dim())] };
    };
    // card #180 gate 3: an engine with no token counters (Ollama, LM Studio) rendered a table of
    // zeros here while `lss status` said n/a - a zero is a claim that nothing was served
    if let Some(na) = s.serve.na("tokens") {
        return Section { title: "TOKENS".into(), colour: Color::Cyan, lines: vec![Line::styled(format!("{na}: this engine publishes no token counters, so tokens per hour, day, week and month cannot be counted"), dim())] };
    }
    let mut lines = vec![Line::styled(format!("{:<LABEL_W$}{:>8}{:>9}{:>10}{:>9}", "", "prompt", "cached", "generated", "requests"), dim())];
    tokens_row(&mut lines, "hour", t.hour, t.hour_covered_secs, 3_600);
    tokens_row(&mut lines, "day", t.day, t.day_covered_secs, 86_400);
    tokens_row(&mut lines, "week", t.week, t.week_covered_secs, 7 * 86_400);
    tokens_row(&mut lines, "month", t.month, t.month_covered_secs, 30 * 86_400);
    // card #191, 2026-09-22: ALL TIME is back. #51's WORK box had `last 1h / last 24h / all
    // time`; #73 replaced WORK with this hour/day/week/month table and the all-time row went
    // with it. It is the one figure the windows structurally cannot give - what this box has
    // served since the engine came up - it is the engine's OWN cumulative counter (no rollup
    // history needed, so it is never "not enough history yet"), and it costs one row in a table
    // that already has the four columns it needs. The `na("tokens")` guard above means a 0 here
    // is a counter that really reads 0, never an engine that publishes no counters at all.
    let big = crate::report::big;
    let sv = &s.serve;
    lines.push(row_text("all time", format!("{:>8}{:>9}{:>10}{:>9}", big(sv.prompt_tokens_total), big(sv.cached_tokens_total), big(sv.generation_tokens_total), big(sv.requests_total))));
    // #247 (the zero-scroll bar): the "cached" glossary line moved to `?` help - the same move
    // every other box's own explanatory caption already got in #227 item 3.
    Section { title: "TOKENS".into(), colour: Color::Cyan, lines }
}

/// USERS: one flat table, one row per user (never one box per row) - lane, requests, prompt
/// tokens, generated tokens, who is running now, and (card #73) a dollar figure: this user's
/// share of the last-24h token total x `s.cost.last_24h_usd` - the SAME rolling-24h window the
/// token columns already use, so the split is honest about what it is dividing. `share` was
/// already a token share, not true GPU-seconds (nothing tracks per-user compute time anywhere in
/// this stack - prefill and decode cost very different amounts of GPU-time per token, and
/// batching efficiency varies, so a token share is not a safe stand-in for it); the `$` column
/// inherits that same caveat, stated once in the footer, not per row.
/// Stage (e): the same fallback discipline card #48 built for charts - never draw a table too
/// narrow to be readable; drop the least essential columns first, and say so.
/// card #195, 2026-09-23, the owner: "i want to know who is actually on it now .. the user is good
/// but i would like to see who is currently still on". The 24h columns answered "who used it
/// today"; the `on now` column answers the live question, and the table is ORDERED by it.
const WHO_FULL_W: usize = 78;
const WHO_NARROW_W: usize = 40;
/// card #227: the compact tier (user + on now + req24h) needs a name of at least 4 columns.
const WHO_COMPACT_MIN_W: usize = 21;
/// card #227: the compact tier shows at most this many callers (running first), then "+N more".
const WHO_COMPACT_ROWS: usize = 5;

/// card #195: how long after their last request a caller with nothing in flight still reads as
/// "on". NOT a number picked to look right: it is `users::ACTIVE_WINDOW_SECS`, the same ten
/// minutes the gateway's own `users_active_10m` counts and `build_users` already calls "recent".
/// Reused on purpose - a caller this table dims as quiet can then never be one that the very same
/// screen counts as active, which a freshly-invented 30 s or 5 min threshold would have allowed.
/// Ten minutes also matches the thing being judged: a coding agent or chat client that asked
/// something eight minutes ago is still sitting there and is genuinely "still on"; one that went
/// quiet an hour ago is not.
const WHO_LIVE_SECS: i64 = lss_core::users::ACTIVE_WINDOW_SECS as i64;

/// The `on now` cell for one caller: what they are doing THIS SECOND, in one column.
/// Three states and deliberately no fourth:
///   - `N live` - the gateway's own in-flight count. Read FIRST, because a caller streaming a long
///     answer has requests running AND a `secs_since_last_request` that keeps growing (the gateway
///     stamps `last_seen` when it accounts a request, not per token).
///   - an age - how long since their last request, dim once past `WHO_LIVE_SECS`.
///   - `\u{2014}` - the gateway published no last-seen time for them. UNKNOWN, and it says so: no
///     `0s`, no "idle". A fabricated zero here would be a claim about a caller this screen knows
///     nothing about - the same rule #147's orphan guard and #180's `na()` rows enforce elsewhere.
fn on_now_cell(r: &lss_core::users::UserRow) -> (String, Style) {
    if r.gate.inflight > 0 {
        return (format!("{} live", r.gate.inflight), good().add_modifier(Modifier::BOLD));
    }
    match r.secs_since_last_request {
        None => ("\u{2014}".to_string(), dim()),
        Some(age) if age <= WHO_LIVE_SECS => (fmt_duration(age), Style::default()),
        Some(age) => (fmt_duration(age), dim()),
    }
}

/// Which of the three groups a caller is in, smallest first: running now, seen inside the live
/// window, everyone else (a caller with no last-seen time at all sorts with "everyone else" - it
/// is not a claim that they are quiet, only that nothing here can put them above someone known
/// to be on).
fn who_rank(r: &lss_core::users::UserRow) -> u8 {
    if r.gate.inflight > 0 {
        0
    } else if r.secs_since_last_request.is_some_and(|a| a <= WHO_LIVE_SECS) {
        1
    } else {
        2
    }
}

fn who(s: &Status, width: usize) -> Section {
    if !s.users.available {
        return Section { title: "USERS".into(), colour: Color::LightBlue, lines: vec![Line::styled("no per-user data (needs a v5.2+ gateway)", dim())] };
    }
    if s.users.rows.is_empty() {
        return Section { title: "USERS".into(), colour: Color::LightBlue, lines: vec![Line::styled("nobody has used the server in the last 24 hours", dim())] };
    }
    if width < WHO_COMPACT_MIN_W {
        return Section { title: "USERS".into(), colour: Color::LightBlue, lines: vec![Line::styled(format!("{} users - widen the pane to see the table", s.users.rows.len()), dim())] };
    }
    let big = crate::report::big;
    let total_tok: f64 = s.users.rows.iter().map(|r| r.gate.prompt_tokens_est_24h as f64 + r.gate.completion_tokens_24h as f64).sum::<f64>().max(1.0);
    let last_24h_usd = s.cost.as_ref().and_then(|c| c.last_24h_usd);
    let full = width >= WHO_FULL_W;
    // card #227 (lss-verifier-4's FAIL A): page 1's grid puts USERS in a ~26-38 column cell at 60
    // and 120 wide, under the narrow tier - it used to say only "widen the pane", an empty box on
    // the top row. The COMPACT tier answers the box's own question in that room: who, running
    // now, requests in 24h.
    let compact = width < WHO_NARROW_W;
    // card #195: the question is "who is on NOW", so the answer goes at the TOP. `/status` sends
    // these in-flight-then-24h-volume order, which still lets a caller who has been quiet for
    // twenty hours outrank the one who asked something ninety seconds ago purely on day volume.
    // Running first, then seen-inside-the-live-window (most recent first), then everyone else -
    // and only inside a group does 24h volume decide. Ties break on the name so the order is
    // total: rows must not shuffle between two polls that say the same thing.
    let mut rows: Vec<&lss_core::users::UserRow> = s.users.rows.iter().collect();
    rows.sort_by(|a, b| {
        who_rank(a)
            .cmp(&who_rank(b))
            .then(b.gate.inflight.cmp(&a.gate.inflight))
            .then(a.secs_since_last_request.unwrap_or(i64::MAX).cmp(&b.secs_since_last_request.unwrap_or(i64::MAX)))
            .then(b.gate.requests_24h.cmp(&a.gate.requests_24h))
            .then(a.name.cmp(&b.name))
    });
    // The headline the owner actually asked for, before any table: how many are ON. Counted from
    // the same three groups the rows are sorted into, so the sentence and the column can never
    // disagree. A caller the gateway gave no last-seen time for is counted as UNKNOWN in its own
    // clause rather than quietly folded into "quiet".
    let (mut live, mut recent, mut unknown) = (0usize, 0usize, 0usize);
    for r in &rows {
        match (who_rank(r), r.secs_since_last_request) {
            (0, _) => live += 1,
            (1, _) => recent += 1,
            (_, None) => unknown += 1,
            _ => {}
        }
    }
    // Kept under the table's own 78 columns on purpose: the wording that spelled "unknown" out in
    // full wrapped onto a second line at 140x44, which is how a reader loses the count that
    // matters. The em-dash caption below carries the long explanation instead.
    let mut headline = vec![Span::styled(format!("{live} running{}", if compact { "" } else { " now" }), if live > 0 { good().add_modifier(Modifier::BOLD) } else { dim() })];
    if compact {
        // card #227: the compact cell is SHORT on purpose - page 1's grid packs by height, and a
        // tall USERS box pushes ALERTS/INCIDENTS below the fold (#222's bar)
        headline.push(Span::raw(format!(" \u{b7} {recent} recent \u{b7} {} quiet", rows.len() - live - recent - unknown)));
    } else {
        headline.push(Span::raw(format!(" \u{b7} {recent} more seen in the last 10 min \u{b7} {} quiet", rows.len() - live - recent - unknown)));
    }
    if unknown > 0 {
        headline.push(Span::styled(format!(" \u{b7} {unknown} unknown"), dim()));
    }
    let mut lines = vec![Line::from(headline)];
    let compact_name_w = width.saturating_sub(17).min(12);
    lines.push(if compact {
        Line::styled(format!("{:<compact_name_w$}{:>9}  {:>6}", "user", "on now", "req24h"), dim())
    } else if full {
        Line::styled(format!("{:<14}{:<8}{:>9}  {:>7}  {:>9}  {:>8}  {:>5}  {:>8}", "user", "lane", "on now", "req24h", "prompt", "gen", "share", "$24h"), dim())
    } else {
        Line::styled(format!("{:<12}{:>9}  {:>9}  {:>5}", "user", "on now", "gen", "share"), dim())
    });
    let shown_rows = if compact { rows.len().min(WHO_COMPACT_ROWS) } else { rows.len() };
    for r in &rows[..shown_rows] {
        let tok = r.gate.prompt_tokens_est_24h as f64 + r.gate.completion_tokens_24h as f64;
        let share = tok / total_tok * 100.0;
        let prompt = big(r.gate.prompt_tokens_est_24h as f64);
        let gen = big(r.gate.completion_tokens_24h as f64);
        let (on_now, on_now_style) = on_now_cell(r);
        lines.push(if compact {
            let name = fit(&r.name, compact_name_w);
            Line::from(vec![Span::raw(format!("{name:<compact_name_w$}")), Span::styled(format!("{on_now:>9}"), on_now_style), Span::raw(format!("  {:>6}", r.gate.requests_24h))])
        } else if full {
            let name = fit(&r.name, 14);
            let dollars = last_24h_usd.map_or_else(|| "\u{2014}".to_string(), |total| format!("${:.2}", total * share / 100.0));
            Line::from(vec![
                Span::raw(format!("{name:<14}{:<8}", r.gate.lane)),
                Span::styled(format!("{on_now:>9}"), on_now_style),
                Span::raw(format!("  {:>7}  {prompt:>9}  {gen:>8}  {share:>4.0}%  {dollars:>8}", r.gate.requests_24h)),
            ])
        } else {
            let name = fit(&r.name, 12);
            Line::from(vec![Span::raw(format!("{name:<12}")), Span::styled(format!("{on_now:>9}"), on_now_style), Span::raw(format!("  {gen:>9}  {share:>4.0}%"))])
        });
    }
    if compact {
        // no caption at this width: page 4 (USERS) has the whole table and what `on now` means
        if rows.len() > shown_rows {
            lines.push(Line::styled(format!("+{} more - page 4", rows.len() - shown_rows), dim()));
        }
        return Section { title: "USERS".into(), colour: Color::LightBlue, lines };
    }
    // #255 round 3 (the owner: polish pass): the "on now"/"share/$"/"lane hidden" explanation
    // paragraph moved to `?` help ("Reading it") - page 1 keeps the table plus its one-line
    // headline, same trim #227 already gave every OTHER box's own caption.
    Section { title: "USERS".into(), colour: Color::LightBlue, lines }
}

/// GPUS: the totals across every GPU; each GPU's own numbers are in its own box below this one
/// (#177 - "the gpu section i would like to have those inside boxes to seperate them").
///
/// Card #196: the TP-straggler row is a PROJECTION of `readings::gpus`'s `gpu.skew`, not a spread
/// recomputed here. That reading owns both the number (the laggard's distance from the pack
/// median, not `max - min`) and the sentence saying what it costs, and its corroboration rule is
/// why an idle box no longer shouts about uneven cards. The box's own colour is the worst band in
/// the GPU domain, on the same scale as the verdict line.
/// #255 (the owner on v1.1.0: "bro im looking at the new ones this is terrible layout" / "the gpu
/// boxes are suppose to be together"): every GPU's own card now lives INSIDE this box, nested
/// side by side (2 per row narrow, up to 4 wide - `boxed_grid`, below), like the classic page's
/// own GPUS panel (`fixtures/renders/overview_200x24.txt`). #227 had them as separate lowest-
/// priority items the packer placed wherever was shortest - which is exactly what read as
/// "scattered". The summary (total power, average util) is now this box's FIRST line, not a
/// sibling row - a reader sees the headline before the detail, same order the classic page uses.
fn gpus(s: &Status, width: usize, chart_lines: bool) -> Section {
    if s.gpus.is_empty() {
        return Section { title: "GPUS".into(), colour: Color::LightGreen, lines: vec![Line::styled("no GPU tool on this machine", dim())] };
    }
    let rs = lss_core::readings::gpus(s);
    let total_power: f64 = s.gpus.iter().filter_map(|g| g.sample.power_w).sum();
    let total_cap: f64 = s.gpus.iter().filter_map(|g| g.sample.power_limit_w).sum();
    let avg_util = s.gpus.iter().filter_map(|g| g.sample.util_pct).sum::<f64>() / s.gpus.len().max(1) as f64;
    // #255 round 3 (the owner, polish): the summary is now ONE line - "total 57/1200 W · util 0%
    // · skew 0 pts · * no thermal alerts" - not 2-3 separate rows.
    let power = if total_cap > 0.0 { format!("{total_power:.0}/{total_cap:.0} W") } else { format!("{total_power:.0} W") };
    let mut spans = vec![Span::raw(power), Span::raw(format!(" \u{b7} util {avg_util:.0}%"))];
    if let Some(skew) = rs.iter().find(|r| r.key == "gpu.skew") {
        // #197: the whole phrase is the reading's own sentence ("0 pts behind the pack over
        // 30s") - this takes just its leading "<n> pts" rather than re-authoring a shorter one,
        // so dash.rs is still never a second owner of the words themselves, only of how much of
        // the reading's own sentence fits on a one-line summary. The full sentence is one key
        // away either way (`?` help's "Reading it" no longer carries it - #227 already trimmed
        // it to the bare value here, and #255 shortens the value itself now).
        let short: String = skew.value_or_dash().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        spans.push(Span::raw(" \u{b7} skew "));
        spans.push(Span::styled(short, band_style(skew)));
    }
    if s.gpus.iter().any(|g| g.thermal_excluded) {
        spans.push(Span::styled(" \u{b7} * no thermal alerts", dim()));
    }
    let mut lines = vec![row("total", spans)];
    let per_row = gpu_per_row(s.gpus.len(), width);
    let card_w = (width / per_row).max(1);
    lines.extend(boxed_grid(ranked_gpu_boxes(s, card_w, chart_lines), per_row, width as u16, PAD));
    let colour = section_colour(&rs, Color::LightGreen);
    Section { title: "GPUS".into(), colour, lines }
}

/// Renders `secs` as a GRID of bordered mini-boxes, `per_row` wide, into a scratch buffer and
/// reads the cells back as plain `Line`s - so nested GPU cards look and colour EXACTLY like any
/// other side-by-side box on this page (the same `frame()`/`title_line()` styling `column_buffer`
/// already uses for `Item::Row`), but as INLINE CONTENT of one parent `Section` instead of
/// siblings the packer could scatter to a different column (#255).
fn boxed_grid(secs: Vec<Section>, per_row: usize, width: u16, pad: u16) -> Vec<Line<'static>> {
    let per_row = per_row.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut it = secs.into_iter();
    loop {
        let row: Vec<Section> = it.by_ref().take(per_row).collect();
        if row.is_empty() {
            break;
        }
        let n = row.len().max(1) as u16;
        let (bw, extra) = (width / n, width % n);
        let mut x = 0u16;
        let placed: Vec<(u16, u16, Section, Vec<Line<'static>>)> = row
            .into_iter()
            .enumerate()
            .map(|(i, mut sec)| {
                let w = bw + u16::from((i as u16) < extra);
                let inner_w = w.saturating_sub(2 + 2 * pad) as usize;
                let lines: Vec<Line<'static>> = std::mem::take(&mut sec.lines).into_iter().flat_map(|l| wrap(l, inner_w)).collect();
                let at = x;
                x += w;
                (at, w, sec, lines)
            })
            .collect();
        let h = placed.iter().map(|(_, _, _, l)| l.len() as u16 + 2).max().unwrap_or(0);
        let mut buf = Buffer::empty(Rect { x: 0, y: 0, width, height: h });
        for (x, w, sec, lines) in placed {
            let area = Rect { x, y: 0, width: w, height: h };
            let block = frame(false, sec.colour).title(title_line(&sec.title, vec![], sec.colour, w.saturating_sub(2) as usize));
            let inner = block.inner(area);
            block.render(area, &mut buf);
            let padded = Rect { x: inner.x + pad, width: inner.width.saturating_sub(2 * pad), ..inner };
            Paragraph::new(lines).render(padded, &mut buf);
        }
        for y in 0..h {
            let spans: Vec<Span<'static>> = (0..width).map(|x| { let cell = &buf[(x, y)]; Span::styled(cell.symbol().to_string(), cell.style()) }).collect();
            out.push(Line::from(spans));
        }
    }
    out
}

/// LANES, in words (#177 - the owner: "i guess lanes are important but i have no idea what it
/// means your gonna have to do better explantion for that and probably dont need a chart"). A lane
/// is a door into the model: what each door IS, how many are inside and how many are held at it,
/// how full the door's token budget is, and what happened to the last 10 minutes of callers - in
/// sentences, with the two terms a reader cannot guess (held at the door, turned away) defined
/// once underneath. No chart.
fn lanes(s: &Status) -> Section {
    let title = "LANES".to_string();
    // #227 item 5 (the owner: "the amber drift in ... LANES"): Yellow is `warn()`, so this box
    // read as a standing warning on a calm day. Blue, matching PageId::Gateway's own identity
    // colour (LANES opens that page on Enter) and LOADOUT's established calm colour.
    if s.gate.absent {
        return Section { title, colour: Color::Blue, lines: vec![Line::styled("no gateway in front of this engine - lanes only exist behind one", dim())] };
    }
    let big = crate::report::big;
    let mut lines = Vec::new();
    // #196: this box used to decide, on its own, that any 5xx is red and any 4xx is amber, while
    // `readings::gateway` scored the same two facts on the shared scale - two owners, two answers.
    // The readings are now PER LANE (they were summed, which forced the renderer to judge for
    // itself which lane's number to colour, and cost the headline the ability to say WHICH lane
    // is being refused - a different problem with a different fix for each).
    let all = lss_core::readings::gateway(s);
    let hold_style = all.iter().find(|r| r.key == "gate.waiters").map_or_else(Style::default, band_style);
    for (name, what, l) in [("public", "outside users, each with a key", &s.lanes.public), ("trusted", "your own agents and tools", &s.lanes.trusted)] {
        let lane = |f: &str| all.iter().find(|r| r.key == format!("gate.{name}.{f}"));
        let style = |f: &str| lane(f).map_or_else(Style::default, band_style);
        let held = if l.waiters > 0 { Span::styled(format!("{} held at the door", l.waiters), hold_style) } else { Span::raw("none held at the door") };
        lines.push(row(name, vec![Span::raw(format!("{what} \u{b7} {:.0} running \u{b7} ", l.running)), held]));
        // #247 (the zero-scroll bar): budget shares a row with the 10-min counts now (both are
        // per-lane metrics) instead of its own line - 3 rows per lane down to 2.
        let mut spans = Vec::new();
        if let Some(budget) = l.budget_tokens.filter(|b| *b > 0) {
            let pct = l.inflight_tokens as f64 / budget as f64 * 100.0;
            spans.push(Span::styled(format!("budget {} of {} ({pct:.0}%) \u{b7} ", big(l.inflight_tokens as f64), big(budget as f64)), style("budget")));
        }
        let c = &l.codes_10m;
        let turned = if c.c4xx > 0 { Span::styled(format!("{} turned away", c.c4xx), style("turned")) } else { Span::raw("0 turned away") };
        let failed = if c.c5xx > 0 { Span::styled(format!("{} failed", c.c5xx), style("failed")) } else { Span::raw("0 failed") };
        spans.extend([Span::raw(format!("10m: {} answered \u{b7} ", c.c2xx)), turned, Span::raw(" \u{b7} "), failed]);
        lines.push(row("", spans));
    }
    // #247 (the whole-page zero-scroll bar #227 did not reach): these two definitions were two of
    // the longest lines on page 1 (at 120 wide, LANES wrapped them to 5 rows between them) -
    // moved to `?` help ("Reading it"), the same move #227 item 3 already made for every other
    // box's own explanatory captions.
    Section { title, colour: section_colour(&all, Color::Blue), lines }
}

/// `09-22 13:05` padded to the one label width, so a dated row sits on the same grid as every
/// labelled row (#177: alerts and incidents are back on page 1).
fn when(ts: i64) -> String {
    lss_core::timeutil::fmt_local(ts, "%m-%d %H:%M")
}

/// How many alert / incident rows page 1 shows; page 8 ALERTS / page 9 INCIDENTS have the rest
/// (and each says how many).
const EVENT_ROWS: usize = 4;

/// ALERTS (#177: "i think u can add incidents and alerts back"). What is firing right now first,
/// then the latest few alerts. The merge rule the panel set stays true in the caption: an ALERT is
/// a live condition that clears by itself, an INCIDENT (the next box) is a dated fact.
fn alerts(s: &Status) -> Section {
    // #196: the severity word is turned into a band in ONE place (`readings::alert_score`), so an
    // `info` alert that the verdict line deliberately ranks below LOW is no longer painted amber
    // in the box right beside it - and "firing now" is no longer a boolean that reddens this box
    // for an alert the page does not consider a problem.
    let events = lss_core::readings::events(s, s.generated_at);
    let mut lines = Vec::new();
    lines.push(if s.firing.is_empty() {
        row("firing now", vec![Span::styled("nothing", good())])
    } else {
        let worst = lss_core::readings::rank(&events).into_iter().find(|r| r.key.starts_with("alert."));
        row("firing now", vec![Span::styled(s.firing.join(", "), worst.map_or_else(dim, band_style_bold))])
    });
    for a in s.alerts.iter().take(EVENT_ROWS) {
        // RECOVERED is not a severity - it is the condition having cleared, which is why it keeps
        // its own green rather than going through the band table.
        let sev_style = if a.recovered {
            good()
        } else {
            band_style(&lss_core::readings::Reading::live("alert", "alert", a.severity.clone(), "d.", lss_core::readings::alert_score(&a.severity)))
        };
        // a recovered alert's own message already says RECOVERED; a live one leads with its severity
        let mut v = if a.recovered { vec![] } else { vec![Span::styled(format!("{} ", a.severity), sev_style)] };
        // #198: the marker was amber by hand, which on this box meant "this alert is `info`" -
        // 13 of 13 undelivered alerts here are info and every warn got out - i.e. the second
        // scale #196 deleted from the severity word arriving through the delivery flag instead.
        // Its ink is now the band of that row's OWN reading (amber only when a sink demonstrably
        // works here AND the alert is one the page scores at or above Watch); when that reading
        // is not a fault the marker is still shown, just quietly.
        if !a.delivered {
            let ink = events.iter().find(|r| r.key == format!("events.undelivered.{}", a.id)).map_or_else(Style::default, band_style);
            v.push(Span::styled("[not delivered] ", if ink == Style::default() { dim() } else { ink }));
        }
        v.push(if a.recovered { Span::styled(a.message.clone(), sev_style) } else { Span::raw(a.message.clone()) });
        lines.push(row(&when(a.ts), v));
    }
    if s.alerts.is_empty() {
        lines.push(Line::styled("no alerts recorded", dim()));
    } else if s.alerts.len() > EVENT_ROWS {
        lines.push(Line::styled(format!("{} older on page 8 ALERTS", s.alerts.len() - EVENT_ROWS), dim()));
    }
    // #227 ("alot of wasted space"): the live-vs-dated merge rule moved to `?` help, "Reading it".
    // the frame is the worst of what this box shows: the firing alerts, and the delivery of the
    // rows actually on screen (#198)
    let shown: Vec<String> = s.alerts.iter().take(EVENT_ROWS).map(|a| format!("events.undelivered.{}", a.id)).collect();
    let mut firing: Vec<_> = events.iter().filter(|r| r.key.starts_with("alert.")).cloned().collect();
    firing.extend(events.iter().filter(|r| shown.contains(&r.key)).cloned());
    Section { title: "ALERTS".into(), colour: section_colour(&firing, Color::DarkGray), lines }
}

/// INCIDENTS (#177): what happened, dated - restarts, outages, Xid errors, maintenance. An open
/// one says OPEN and for how long; a closed one says how long it lasted.
fn incidents(s: &Status, now: i64) -> Section {
    // #196: an OPEN incident used to be painted red by this box on its own. An incident is a
    // DATED RECORD that has not been seen to close; whether the condition is still live is what
    // serve.up, gate.up and the firing alerts already say. One left open on a box that has been
    // healthy since would have shown red beside a verdict line saying OK. `events.open` scores it
    // only when a second source agrees the box is unwell now, and this box takes its ink from
    // that - so the two surfaces can no longer contradict each other about the same server.
    let events = lss_core::readings::events(s, now);
    let open_r = events.iter().find(|r| r.key == "events.open");
    let open_style = open_r.map_or_else(Style::default, band_style);
    let mut lines = Vec::new();
    for i in s.incidents.iter().take(EVENT_ROWS) {
        let maintenance = i.kind == lss_core::incidents::KIND_MAINTENANCE;
        let span = match i.end {
            None if maintenance => Span::styled(format!("OPEN {}", fmt_duration(now - i.start)), good()),
            None => Span::styled(format!("OPEN {}", fmt_duration(now - i.start)), open_style),
            Some(e) if e > i.start => Span::raw(format!("lasted {}", fmt_duration(e - i.start))),
            Some(_) => Span::raw(String::new()),
        };
        // #198: the `xid` word was painted amber here on its own, by KIND - ink this box decided,
        // on a dated record inside a seven-day list, which on this box left it lit on 100% of the
        // last day's polls. It now takes the band of that incident's OWN reading, which is
        // coloured while the fault is fresh or while something else says the box is unwell.
        let kind_style = events.iter().find(|r| r.key == format!("events.xid.{}", i.id)).map_or_else(Style::default, band_style);
        let sep = if span.content.is_empty() { "" } else { " " };
        // #247 (the zero-scroll bar): an Xid's full detail (incl. "pid=N, name=..., channel
        // 0x...") wrapped this row to 5-6 lines at a narrow column - page 9 INCIDENTS carries it
        // in full. Page 1's own rule is NEVER an ellipsis cut (`page1_is_one_grid_with_nothing_
        // truncated...`), so this drops the pid/channel clause as a deliberate shorter sentence,
        // not a slice of the longer one - what remains still wraps whole, never cut mid-word.
        let detail = i.detail.split(" pid=").next().unwrap_or(&i.detail);
        lines.push(row(&when(i.start), vec![Span::styled(format!("{}{sep}", i.kind), kind_style), span, Span::styled(format!(" \u{b7} {detail}"), dim())]));
    }
    if s.incidents.is_empty() {
        lines.push(Line::styled("none in the last 7 days", dim()));
    } else if s.incidents.len() > EVENT_ROWS {
        // #231: INCIDENTS split off page 8 onto its own page 9 - this box's own overflow line
        // must point at ITS page, not the ALERTS page it used to share the number with.
        lines.push(Line::styled(format!("{} older on page 9 INCIDENTS", s.incidents.len() - EVENT_ROWS), dim()));
    }
    // #227 ("alot of wasted space"): the live-vs-dated merge rule moved to `?` help, "Reading it".
    // the frame is the worst of what this box actually SHOWS - the open incident and the Xid rows
    // on screen (#198), never an Xid scrolled off onto page 9
    let shown: Vec<String> = s.incidents.iter().take(EVENT_ROWS).map(|i| format!("events.xid.{}", i.id)).collect();
    let mut own: Vec<_> = open_r.into_iter().cloned().collect();
    own.extend(events.iter().filter(|r| shown.contains(&r.key)).cloned());
    Section { title: "INCIDENTS".into(), colour: section_colour(&own, Color::Magenta), lines }
}

/// #177 ("the gpu section i would like to have those inside boxes to seperate them"): one box
/// per GPU. #175's "never a box nested inside another box" rule no longer holds for this one -
/// #255 nests these INSIDE the GPUS box on purpose (see `gpus`/`boxed_grid`).
/// #179 item 4: each box is a PROJECTION of that card's `lss_core::readings` - its colours come
/// from the readings' bands on the one shared scale (the same scale the verdict line uses), not
/// from thresholds spelled here.
/// #247: combined onto 2 rows (temp+power, util+memory) plus throttle only when it is NOT clean
/// (a card running clean is the common case; the exception earns the row, not the routine) -
/// every number page 3's own GPUS detail page still carries in full.
/// #255 ("the gpu boxes are suppose to be together"): now nested 2-4 to a row INSIDE the GPUS
/// box (`boxed_grid`), so each card is much narrower than it used to be as its own top-level box.
/// The old `row()`/`LABEL_W`-prefixed lines ("temp/power    52C/126F \u{b7} 19/300W") have no
/// room to spare at that width and either wrap hard or ellipsis-cut a value. `card_w` is this
/// card's own inner width; below `GPU_CARD_LABEL_W` it drops the label column entirely (bare
/// values, one/two per line - the classic page's own compact GPU cards use none either; the
/// box's own title, "GPU0", already says which GPU a value belongs to).
// #255's FAIL (lss-verifier-4): 24 first, then measured a real ellipsis at 240x34's 4-wide row -
// "0% \u{b7} 93.3/95.6G" (15 chars) needs the label's LABEL_W=16 column PLUS ~16 more; below that
// this exact content wraps or cuts. 36 is what the top-level GPU box always needed for the same
// row before #255 (`GPU_BOX_MIN_W`, since removed - nothing else read it).
const GPU_CARD_LABEL_W: usize = 36;

/// #255 round 3 (the owner: "NEVER 3+1. With 4 GPUs it is 4 across or 2x2, never a lone card on
/// its own row. At 120 cols, make the cards compact enough for 4 across... If that truly cannot
/// fit, use 2x2."): the widest `per_row` that (a) still leaves cards readable at `gpu_box`'s own
/// COMPACT tier (bare values, no label column - `GPU_CARD_MIN_W`, well under the LABELLED tier's
/// own `GPU_CARD_LABEL_W`) and (b) never leaves exactly one card alone on a last row of its own.
/// 4 GPUs at 120 wide: the old `GPU_CARD_LABEL_W`-based fit forced 2x2 outright (3 was the widest
/// that fit a LABELLED card, and 3 always fails the no-lone-card rule for 4 GPUs) - this instead
/// asks "can the compact tier hold 4", which it comfortably can at 120 (~29 a card), so 120 now
/// gets 4 across with short bare-value cards, only falling to 2x2 where even that cannot fit.
const GPU_CARD_MIN_W: usize = 18;
fn gpu_per_row(n_gpus: usize, width: usize) -> usize {
    if n_gpus <= 1 {
        return 1;
    }
    let max_fit = ((width as u16 + 2) / (GPU_CARD_MIN_W as u16 + 2)).clamp(2, 4) as usize;
    for candidate in (2..=max_fit).rev() {
        if n_gpus <= candidate || n_gpus % candidate != 1 {
            return candidate;
        }
    }
    2
}

fn gpu_box(g: &lss_core::model::GpuStatus, rs: &[lss_core::readings::Reading], temp_hist: Option<&[Option<f64>]>, card_w: usize, chart_lines: bool) -> Section {
    let sm = &g.sample;
    let find = |f: &str| rs.iter().find(|r| r.key == format!("gpu.{}.{f}", sm.index));
    let style_of = |r: Option<&lss_core::readings::Reading>| r.map_or_else(Style::default, band_style);
    let temp = sm.temp_c.map_or_else(|| "\u{2014}".into(), lss_core::units::temp_compact);
    let temp_style = style_of(find("temp"));
    let power = match (sm.power_w, sm.power_limit_w) {
        (Some(w), Some(cap)) => format!("{w:.0}/{cap:.0}W"),
        (Some(w), None) => format!("{w:.0}W"),
        _ => "\u{2014}".into(),
    };
    let util = sm.util_pct.map_or_else(|| "\u{2014}".to_string(), |u| format!("{u:.0}%"));
    let gib = |m: Option<f64>| m.map_or_else(|| "\u{2014}".to_string(), |v| format!("{:.1}", v / 1024.0));
    let throttled = find("throttle").filter(|r| r.value.as_deref() != Some("none"));
    let mut lines = if card_w >= GPU_CARD_LABEL_W {
        // room to spare: the labelled rows a top-level GPU box always used
        let mut l = vec![
            row("temp/power", vec![Span::styled(temp, temp_style), Span::raw(format!(" \u{b7} {power}"))]),
            row_text("util/mem", format!("{util} \u{b7} {}/{}G", gib(sm.mem_used_mib), gib(sm.mem_total_mib))),
        ];
        // #196: an informational throttle reason (a CHOSEN power cap) is named in plain ink; a
        // real constraint carries its own band's colour - one table decides which, not this box.
        if let Some(r) = throttled {
            l.push(row("throttle", vec![Span::styled(r.value_or_dash().to_string(), style_of(Some(r)))]));
        }
        l
    } else {
        // no label column: bare values, temp on its own line (it carries the band colour a
        // reader scans for first), power/util combined, mem combined. Whole GiB, not `gib`'s one
        // decimal place - "93/96G" fits a card this narrow outright, "93.3/95.6G" does not and
        // wrap()'s own single-word rule would ellipsis-cut it instead.
        let gib0 = |m: Option<f64>| m.map_or_else(|| "\u{2014}".to_string(), |v| format!("{:.0}", v / 1024.0));
        let mut l = vec![Line::styled(temp, temp_style), Line::raw(format!("{power} \u{b7} {util}")), Line::styled(format!("{}/{}G", gib0(sm.mem_used_mib), gib0(sm.mem_total_mib)), dim())];
        if let Some(r) = throttled {
            l.push(Line::styled(r.value_or_dash().to_string(), style_of(Some(r))));
        }
        l
    };
    // #227 item 4 (the owner: "add in some small graphs ... e.g. GPU temp/power"): this card's
    // OWN embedded hour (`s.series.gpu_temp_c` is one inner series per GPU index). Needs real
    // room to read as a trend rather than noise, so only once the card has it either way - and
    // the labelled tier's own `row()` prefix only when the label tier is actually in use (#255's
    // FAIL, lss-verifier-4: the compact tier called `row()` here regardless, re-adding the
    // LABEL_W-wide column the whole point of the compact tier was to drop).
    // #305 (the owner: "the bars on the temp doesnt really show me anything"): the GPUS page's
    // own chart widget (see `trend`) across the card's whole inner width, both tiers alike, scaled
    // to what the hour spanned and keeping each column's PEAK - the TEMPERATURE chart's own rules.
    if card_w >= 18 {
        if let Some(hist) = temp_hist.filter(|h| h.iter().any(|v| v.is_some())) {
            let trend = Trend { width: card_w.saturating_sub(2 + 2 * PAD as usize), lines: chart_lines };
            lines.extend(trend.chart("temp 1h", hist, super::overview::axis_temp, (None, None), true));
        }
    }
    let colour = section_colour(rs, Color::LightGreen);
    Section { title: format!("GPU{}{}", sm.index, if g.thermal_excluded { "*" } else { "" }), colour, lines }
}

/// #179 item 4: the GPU boxes RANKED worst-first by band on the one scale. Within a band they
/// keep index order, so a box only moves when its band changes - never on a degree's drift.
fn ranked_gpu_boxes(s: &Status, card_w: usize, chart_lines: bool) -> Vec<Section> {
    use lss_core::readings::{gpu_card, worst_band};
    let mut cards: Vec<_> = s.gpus.iter().map(|g| {
        let rs = gpu_card(s, g);
        (worst_band(&rs), g, rs)
    }).collect();
    cards.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.sample.index.cmp(&b.1.sample.index)));
    cards.into_iter().map(|(_, g, rs)| gpu_box(g, &rs, s.series.gpu_temp_c.get(g.sample.index as usize).map(Vec::as_slice), card_w, chart_lines)).collect()
}

/// One box on a row of its own. #255: this used to have a `Row(Vec<Section>)` variant too -
/// several boxes side by side as ONE packer item (the GPU boxes) - but every GPU card now lives
/// INSIDE `gpus()`'s own content (`boxed_grid`) instead of flowing through the packer as its own
/// item, so nothing constructs `Row` any more. Kept as a wrapper (not bare `Section`) since
/// `pack_columns`/`item_height_estimate`/`column_buffer` are written against `Item` and a second
/// variant may well come back the day another box wants its own side-by-side row.
enum Item {
    One(Section),
}


/// #175 ("the texts looks all off"): one blank column inside each box border, both sides, so no
/// value ever touches a border glyph. #227: this used to have a narrower-still special case
/// (`table_pad()`, dropped to 0 at one or two widths to save USERS' full tier) that only ever
/// applied to the ONE column holding USERS in the old two-column design; removed rather than
/// applied to every column of the new grid, which cost OTHER boxes their padding at the same
/// widths (see `layout()`'s own note).
const PAD: u16 = 1;

/// #227 (the owner: "gpu and load out.. then users" - the top row's own order, item 4): every box
/// in the grid EXCEPT GPUS, in priority order. #255 round 2 (the owner, authorized: GPUS gets its
/// OWN full-width row above this grid, never a shared column) - `layout()` renders `gpus()`
/// separately now, so it is not one of these items. `layout()` seeds the first `n` columns with
/// the first `n` entries here (LOADOUT, USERS) so they lead the grid at every width that has room
/// for them; the rest keeps the card's own content order (loadout, electricity, cost per 1M,
/// SERVE, LANES, TOKENS, users, alerts, incidents - USERS pulled forward per item 4, the rest
/// unchanged) and packs by shortest-column-first. `inner_w` is the width INSIDE a box's border
/// and padding - WHO uses it to pick its column tier, SERVE to pick its wordings.
fn boxes(s: &Status, now: i64, inner_w: usize, chart_lines: bool) -> Vec<Item> {
    let vw = inner_w.saturating_sub(LABEL_W);
    let trend = Trend { width: inner_w, lines: chart_lines };
    let items = vec![
        Item::One(loadout(s, now, trend)),
        Item::One(who(s, inner_w)),
        // #222/#231's own hard requirement carries forward: ALERTS/INCIDENTS reachable with NO
        // scroll at 120x40/160x45/200x50/200x30. Right after the item-4 top row, not at the end.
        Item::One(alerts(s)),
        Item::One(incidents(s, now)),
        Item::One(electricity(s, now)),
        Item::One(cost_per_million(s)),
        Item::One(serve(s, vw, trend)),
        Item::One(lanes(s)),
        Item::One(tokens(s)),
    ];
    items
}

/// A cheap stand-in for a box's rendered height, used only to decide WHICH column a box goes
/// into (`pack_columns`) - raw line count, not the exact wrapped height `column_buffer` computes.
/// Good enough to balance a ten-box grid; the real render is the source of truth for what is
/// actually on screen.
/// #247 (the whole-page zero-scroll bar #227 did not reach): the ORIGINAL estimate was raw line
/// count, blind to wrapping - at a narrow column (120 wide / 3 = 40) several boxes' own text
/// wraps 2-3x over, so the greedy packer's height estimate was routinely wrong by that much,
/// piling far more real height into one column than the others while believing them balanced.
/// This wraps for real (the same `wrap()` `column_buffer` itself uses) at the ACTUAL target
/// width, so the greedy decision below matches what will really be drawn.
fn item_height_estimate(item: &Item, width: u16, pad: u16) -> u16 {
    let wrapped_height = |lines: &[Line<'static>], w: u16| -> u16 {
        let inner_w = w.saturating_sub(2 + 2 * pad) as usize;
        lines.iter().map(|l| wrap(l.clone(), inner_w).len() as u16).sum::<u16>() + 2
    };
    let Item::One(sec) = item;
    wrapped_height(&sec.lines, width)
}

/// #227: pack `items` (already in priority order, GPUS/LOADOUT/USERS first) into `n` equal-width
/// columns. The first `n.min(3)` items each seed their own column, so GPUS leads column 0,
/// LOADOUT column 1, USERS column 2, at every width with room for three columns; below that they
/// still lead in the same priority order, just packed into fewer columns. Everything after the
/// seed goes to whichever column is CURRENTLY SHORTEST - the same greedy heuristic any simple
/// masonry layout uses, so the grid stays roughly height-balanced rather than piling into column 0.
fn pack_columns(items: Vec<Item>, n: usize, width: u16, pad: u16) -> Vec<ColumnBuf> {
    let n = n.max(1);
    let mut cols: Vec<Vec<Item>> = (0..n).map(|_| Vec::new()).collect();
    let mut heights = vec![0u16; n];
    let mut it = items.into_iter();
    for (c, h) in cols.iter_mut().zip(heights.iter_mut()).take(n.min(3)) {
        if let Some(item) = it.next() {
            *h += item_height_estimate(&item, width, pad);
            c.push(item);
        }
    }
    for item in it {
        let ci = heights.iter().enumerate().min_by_key(|(_, h)| **h).map(|(i, _)| i).unwrap_or(0);
        heights[ci] += item_height_estimate(&item, width, pad);
        cols[ci].push(item);
    }
    cols.into_iter().map(|c| column_buffer(c, width, pad)).collect()
}

/// #177, the through-line of all the owner's feedback (and of pulse, the bar he pointed at): tell
/// the reader WHAT IS WORST RIGHT NOW AND WHAT IT MEANS, in words, before any number. One line
/// from `crate::constraints` - the same ranked verdict every surface should use - with the rest
/// of the non-OK list named after it, so nothing ranked lower is hidden by the one that leads.
/// Scale and words are #179's (`lss_core::readings`): one scale, one source of band words.
/// The headline for this frame, with last frame's leader as the incumbent (hysteresis), and the
/// new leader remembered for the next frame. Page 1's line and the minimized card share it.
fn frame_headline(app: &App, s: &Status) -> crate::constraints::Headline {
    let incumbent = app.worst_key.borrow().clone();
    let h = crate::constraints::headline_with(s, incumbent.as_deref());
    *app.worst_key.borrow_mut() = h.leader.as_ref().map(|r| r.key.clone());
    h
}

fn worst_line(app: &App, s: &Status) -> Line<'static> {
    let h = frame_headline(app, s);
    let bold = ratatui::style::Style::default().add_modifier(Modifier::BOLD);
    let mut v = match &h.leader {
        None => vec![Span::styled("OK", good()), Span::raw(format!(" \u{b7} {}", h.calm))],
        Some(r) => {
            // #196: the same table every other value on this page goes through. It already
            // matched what this line did; now nothing else can drift away from it.
            let style = if band_style(r) == Style::default() { bold } else { band_style_bold(r) };
            vec![Span::styled(crate::constraints::band_word(r), style), Span::raw(" \u{b7} "), Span::styled(r.label.clone(), bold), Span::raw(format!(" \u{b7} {}", r.detail))]
        }
    };
    if !h.also.is_empty() {
        v.push(Span::styled(format!(" \u{b7} also: {}", h.also.join(", ")), dim()));
    }
    // the meter rides the verdict line, the one line always on screen: #101's rule (a real dollar
    // figure visible with no key pressed, at any size) holds whatever the boxes below have to do.
    // #196: it rides there in PLAIN INK, always, because it is an `informational` reading - the
    // largest dollar figure on the page can share a line with a critical verdict and never be
    // mistaken for the problem.
    if let Some(money) = lss_core::readings::cost(s).into_iter().find(|r| r.key == "cost.live" && r.value.is_some()) {
        debug_assert!(money.informational, "money is a fact, not a severity");
        v.push(Span::styled(format!(" \u{b7} costing {}", money.value_or_dash()), band_style(&money)));
    }
    row("worst now", v)
}

/// At minimized sizes six boxed sections cannot work, and scrolling a one-section-at-a-time list
/// is a regression from a compact card - it shows six STATIC facts and not one live number.
/// Re-ranked for what a glance at a corner of the screen actually asks, in the same owner framing
/// as the rest of this page: is it up, what is it costing right now, how loaded, is anything hot,
/// did anything break. The "say what was hidden" discipline still holds: this is a distinct,
/// named view (title says MINIMIZED), not the full page cropped.
fn minimized(app: &App, s: &Status) -> Vec<Line<'static>> {
    // #196: the corner card is the same page in six lines, so it reads the same list. Its up/down
    // and firing lines used to carry their own two booleans - the one place a reader glances at
    // when they have no room for anything else must not disagree with the full page.
    let up = s.serve.up;
    let serve_rs = lss_core::readings::serve(s, s.generated_at);
    let up_style = serve_rs.iter().find(|r| r.key == "serve.up").map_or_else(good, band_style_bold);
    let events = lss_core::readings::events(s, s.generated_at);
    let worst_alert = lss_core::readings::rank(&events).into_iter().find(|r| r.key.starts_with("alert.")).cloned();
    let cost_line = s.cost.as_ref().map_or_else(
        || "\u{2014} cost tracking off".to_string(),
        |c| format!("{}/hour \u{b7} {} today", usd(c.live_usd_per_hour, "no live reading"), usd(c.today_usd, "not priced yet")),
    );
    let hottest = s.gpus.iter().filter(|g| g.sample.temp_c.is_some()).max_by(|a, b| a.sample.temp_c.unwrap_or(0.0).total_cmp(&b.sample.temp_c.unwrap_or(0.0)));
    let hottest_text = hottest.map_or_else(|| "no GPU data".to_string(), |g| format!("GPU{} {}", g.sample.index, lss_core::units::temp_compact(g.sample.temp_c.unwrap_or(0.0))));
    vec![
        Line::from(vec![Span::styled(if up { "up" } else { "down" }, up_style)]),
        // #177: the one-line verdict, the most useful thing a corner of the screen can say
        {
            let h = frame_headline(app, s);
            Line::raw(h.leader.map_or_else(|| "nothing is constrained".to_string(), |r| format!("{} {}: {}", crate::constraints::band_word(&r), r.label, r.detail)))
        },
        Line::raw(cost_line),
        Line::raw(format!("running {:.0}/{} \u{b7} queue {:.0}", s.serve.running, s.serve.slots, s.serve.queue)),
        Line::raw(format!("hottest {hottest_text}")),
        Line::styled(
            if s.firing.is_empty() { "no alerts firing".to_string() } else { format!("{} firing", s.firing.len()) },
            worst_alert.as_ref().map_or_else(dim, band_style_bold),
        ),
    ]
}

/// #175: a line longer than its box WRAPS instead of ending in `…` - the owner's "no line
/// truncated at a supported size", and the #114 lesson that a cut line cuts the caveat first.
/// A label row (its first span is exactly `LABEL_W` wide) continues UNDER its value column, so
/// the label column stays empty and the grid holds; any other line continues at the left edge.
/// Words break at spaces (so at ` \u{b7} ` and `, ` too); card #227 (lss-verifier-4's FAIL B): a
/// single word wider than the room it has is cut with an ellipsis, never SPLIT across lines -
/// `modelopt_f` / `p4` read as two values, `modelopt…` reads as one value that did not fit.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let total = spans_width(&line.spans);
    if total <= width || width == 0 {
        return vec![line];
    }
    let line_style = line.style;
    let mut spans = line.spans.into_iter();
    let labelled = width > LABEL_W + 8;
    let mut prefix: Vec<Span<'static>> = Vec::new();
    let mut indent = 0;
    let mut rest: Vec<Span<'static>> = Vec::new();
    if let Some(first) = spans.next() {
        if labelled && first.content.chars().count() == LABEL_W {
            indent = LABEL_W;
            prefix.push(first);
        } else {
            rest.push(first);
        }
    }
    rest.extend(spans);
    // words, each keeping its own style; the spaces are their own tokens so styling survives
    let mut toks: Vec<(String, ratatui::style::Style)> = Vec::new();
    for sp in rest {
        let mut word = String::new();
        for c in sp.content.chars() {
            if c == ' ' {
                if !word.is_empty() {
                    toks.push((std::mem::take(&mut word), sp.style));
                }
                toks.push((" ".into(), sp.style));
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            toks.push((word, sp.style));
        }
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur = prefix;
    let mut cur_w = indent;
    let room = width.saturating_sub(indent).max(1);
    for (t, st) in toks {
        let tw = t.chars().count();
        if t == " " {
            if cur_w > indent && cur_w < width {
                cur.push(Span::styled(t, st));
                cur_w += 1;
            }
            continue;
        }
        if cur_w + tw > width && cur_w > indent {
            // drop the trailing space of the finished line
            if cur.last().is_some_and(|sp| sp.content == " ") {
                cur.pop();
            }
            out.push(Line::from(std::mem::take(&mut cur)).style(line_style));
            cur.push(Span::raw(" ".repeat(indent)));
            cur_w = indent;
        }
        let mut chars: Vec<char> = t.chars().collect();
        if chars.len() > room {
            chars.truncate(room - 1);
            chars.push('\u{2026}');
        }
        let tw = chars.len();
        if tw > 0 {
            cur.push(Span::styled(chars.into_iter().collect::<String>(), st));
            cur_w += tw;
        }
    }
    if cur_w > indent {
        out.push(Line::from(cur).style(line_style));
    }
    out
}

/// One column of boxes, drawn top to bottom into its own off-screen buffer of the height it
/// actually needs (every line wrapped to the box first, so the height is the real one - no
/// silent row cap). Returns the buffer and its height.
/// one box placed in a column row: its x, its width, the section, and its lines already wrapped
type Placed = (u16, u16, Section, Vec<Line<'static>>);

/// #231: a box's title, and the row range `[y, y+h)` it occupies in its column's own row space -
/// see `column_buffer`'s doc comment.
type TitleRows = (String, u16, u16);

/// One column: its rendered buffer, its natural height, and every box's `TitleRows`.
type ColumnBuf = (Buffer, u16, Vec<TitleRows>);

/// #231: alongside the rendered buffer, the row range `[y, y+h)` each box's title occupies IN
/// THIS COLUMN'S OWN row space - the same coordinate space `app.scroll` already addresses (both
/// columns scroll together, a row at a time). Lets Enter on page 1 find "which box is the reader
/// looking at right now" without page 1 needing its own focus/pin concept (#51's own rule: it
/// has none) - just a lookup from the one thing it already tracks, the scroll position.
fn column_buffer(items: Vec<Item>, width: u16, pad: u16) -> ColumnBuf {
    // every box: its x, its width, and its lines already wrapped to that width
    let wrapped = |mut sec: Section, w: u16| -> (Section, Vec<Line<'static>>) {
        let inner_w = w.saturating_sub(2 + 2 * pad) as usize;
        let lines: Vec<Line<'static>> = std::mem::take(&mut sec.lines).into_iter().flat_map(|l| wrap(l, inner_w)).collect();
        (sec, lines)
    };
    let rows: Vec<Vec<Placed>> = items
        .into_iter()
        .map(|item| {
            let Item::One(sec) = item;
            let (sec, lines) = wrapped(sec, width);
            vec![(0, width, sec, lines)]
        })
        .collect();
    // a row is as tall as its tallest box, so boxes side by side share top and bottom borders
    let row_h = |row: &Vec<Placed>| row.iter().map(|(_, _, _, l)| l.len() as u16 + 2).max().unwrap_or(0);
    let height: u16 = rows.iter().map(row_h).sum();
    let mut buf = Buffer::empty(Rect { x: 0, y: 0, width, height });
    let mut y = 0;
    let mut ranges = Vec::new();
    for row in rows {
        let h = row_h(&row);
        for (x, w, sec, lines) in row {
            let area = Rect { x, y, width: w, height: h };
            let block = frame(false, sec.colour).title(title_line(&sec.title, vec![], sec.colour, w.saturating_sub(2) as usize));
            let inner = block.inner(area);
            block.render(area, &mut buf);
            let padded = Rect { x: inner.x + pad, width: inner.width.saturating_sub(2 * pad), ..inner };
            Paragraph::new(lines).render(padded, &mut buf);
            ranges.push((sec.title.clone(), y, y + h));
        }
        y += h;
    }
    (buf, height, ranges)
}

/// #227 (the owner: "i liked how you layout out the boxes better in the old version. now its all
/// just one long box" / "alot of wasted space" / "gpu should alwasy be on top as well"): a PACKED
/// GRID, `column_count(body.width)` columns wide, LOADOUT/USERS seeding the first row (see
/// `boxes`/`pack_columns`). Replaces #175's one-or-two-column design - at the owner's own pane
/// widths that design never used more than ONE column at all (`TWO_COLUMNS_MIN_W` was 120), which
/// is exactly what he was pointing at. Every column still scrolls together, a ROW at a time, for
/// whatever does not fit the first screen - #175's own reason still holds: nothing is left out,
/// the footer's "N-M of T" says more is below, and up/down reaches it.
/// #255 round 2 (the owner, authorized): GPUS is no longer one of the columns - it gets its OWN
/// FULL-WIDTH band above the grid (4 GPU cards across when the pane is wide enough, 2x2 when it
/// is not - `gpus()`'s own `per_row` math already does this once it is handed the FULL body
/// width instead of a shared column's half). The band and the grid below it share ONE scroll
/// coordinate space (`app.scroll`/`page_rows`), so "N-M of T" and up/down/pgup/pgdn still address
/// the whole page as a single scrolling column, same as before - the band is just row 0 onward.
fn layout(f: &mut Frame, app: &App, s: &Status, now: i64, body: Rect) {
    let pad = PAD;
    let gpus_inner_w = body.width.saturating_sub(2 + 2 * pad) as usize;
    let (gpus_buf, gpus_h, _) = column_buffer(vec![Item::One(gpus(s, gpus_inner_w, app.chart_lines))], body.width, pad);

    let n = column_count(body.width);
    let col_w = (body.width / n as u16).max(1);
    // #227: `table_pad()` was built for the OLD two-column design, where only the ONE column
    // holding USERS ever called it - it drops the padding column at the one or two widths where
    // that costs USERS its full tier. Applied to EVERY column of the new N-column grid instead,
    // it also strips the padding from whichever OTHER boxes happen to share that column width
    // (measured: SERVE's own rows lost their leading padding space at col_w=80, breaking
    // `"│ label"` - the shape every reader and every test parses rows by). Flat `PAD` everywhere;
    // USERS keeps its own narrow-tier fallback regardless (`WHO_NARROW_W`/`WHO_FULL_W` inside
    // `who()` itself), so nothing is lost, just no longer stolen from boxes it was never about.
    let items = boxes(s, now, col_w.saturating_sub(2 + 2 * pad) as usize, app.chart_lines);
    let cols = pack_columns(items, n, col_w, pad);
    let cols_h = cols.iter().map(|(_, h, _)| *h).max().unwrap_or(0);
    let total = gpus_h + cols_h;
    let start = app.scroll.min(total.saturating_sub(body.height) as usize) as u16;
    let shown = body.height.min(total.saturating_sub(start));
    app.page_rows.set((start as usize, shown as usize, total as usize));
    // #231: ALERTS/INCIDENTS can land in ANY column now (the greedy packer decides), so every
    // column's ranges are scanned, not just the first - offset by `gpus_h` since the grid's own
    // row 0 is now `gpus_h` rows down the shared scroll coordinate space.
    let openable = |title: &str| match title {
        "ALERTS" => Some(PageId::Alerts),
        "INCIDENTS" => Some(PageId::Incidents),
        _ => None,
    };
    *app.dash_page_rows.borrow_mut() = cols
        .iter()
        .flat_map(|(_, _, ranges)| ranges.iter())
        .filter_map(|(title, y0, y1)| openable(title).map(|p| (p, gpus_h + *y0, gpus_h + *y1)))
        .collect();
    let screen = f.buffer_mut();
    // the GPUS band, full width, rows [0, gpus_h) of the shared scroll space
    for dy in 0..shown {
        let sy = start + dy;
        if sy >= gpus_h {
            break;
        }
        for dx in 0..gpus_buf.area.width.min(body.width) {
            screen[(body.x + dx, body.y + dy)] = gpus_buf[(dx, sy)].clone();
        }
    }
    // the N-column grid below it, rows [gpus_h, total) - same per-column early-break as before,
    // just measured from `cy` (the grid's OWN row 0) instead of `sy` directly.
    let mut x0 = body.x;
    for (buf, h, _) in &cols {
        for dy in 0..shown {
            let sy = start + dy;
            if sy < gpus_h {
                continue;
            }
            let cy = sy - gpus_h;
            if cy >= *h {
                break;
            }
            for dx in 0..buf.area.width {
                screen[(x0 + dx, body.y + dy)] = buf[(dx, cy)].clone();
            }
        }
        x0 += col_w;
    }
}

/// #227: at least 2 columns from ~60 wide, more as the pane widens (the owner's own ask - his
/// half-pane is about 60 wide, and the classic grid already packed 2 columns there).
/// #255 (the owner on v1.1.0: "u squeezed 3 boxes in .. most should be two"): the 3RD tier this
/// comment used to defend at 160+ wide is gone - AT MOST 2 columns of boxes at every width, full
/// stop. A #255 measurement note: 3 columns was never really about box COUNT, it was that GPUS
/// used to be one small summary box competing for column space with everything else; now GPUS
/// carries every GPU card itself (see `gpus`) and is naturally the tallest box on the page, so 2
/// wide columns read as deliberate rather than the "empty and wasteful" look a 3rd wide-but-short
/// column gave the smaller boxes.
fn column_count(width: u16) -> usize {
    match width {
        0..=59 => 1,
        _ => 2,
    }
}

pub fn draw(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) {
    let area = super::reserve_key_hints(f, area);
    if area.height < 3 {
        return render_lines(f, area, vec![super::header(app, s, area.width, now)]);
    }
    let focus = pick_shape(area.width, area.height) == Shape::Focus;
    // #177: the verdict, in words, lined up with the boxes' value column (border + pad) - since
    // card #227 item 6 it rides the BOTTOM chrome, directly above the tab strip and the footer, so
    // the boxes start on the first row (the owner: "this stuff should be on the bottom")
    let worst: Vec<Line<'static>> = if focus {
        Vec::new()
    } else {
        wrap(worst_line(app, s), area.width.saturating_sub(2) as usize).into_iter().map(|l| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(l.spans);
            Line::from(spans).style(l.style)
        }).collect()
    };
    let (body, foot) = super::draw_chrome_bottom(f, app, s, area, now, worst);
    if body.height < 1 {
        return;
    }
    if focus {
        let inner = draw_box(f, body, "PAGE 1 (MINIMIZED)", vec![], Color::Blue, false);
        let lines: Vec<Line<'static>> = minimized(app, s).into_iter().map(|l| fit_line(l.spans, inner.width as usize)).collect();
        return render_lines(f, inner, lines);
    }
    layout(f, app, s, now, body);
    f.render_widget(ratatui::widgets::Paragraph::new(footer_at(app, area.width, area.height)), foot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::model::CostStatus;

    /// #175: a label row too long for its box wraps UNDER its value column (the label column
    /// stays empty on the continuation), keeps its words whole, and loses no character.
    #[test]
    fn wrap_continues_a_label_row_under_its_value_column_and_drops_nothing() {
        let l = row_text("flags", "tp 4 \u{b7} ep 4 \u{b7} ctx 1048576 \u{b7} quant modelopt_fp4".into());
        let out = wrap(l, 40);
        assert!(out.len() >= 2, "{out:?}");
        let text = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();
        for cont in &out[1..] {
            let t = text(cont);
            assert!(t.starts_with(&" ".repeat(LABEL_W)) && !t[LABEL_W..].starts_with(' '), "continuation must start exactly at the value column: {t:?}");
        }
        for l in &out {
            assert!(text(l).chars().count() <= 40, "{:?}", text(l));
        }
        let joined: String = out.iter().map(|l| text(l).trim().to_string()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("modelopt_fp4") && joined.contains("1048576"), "no word may be lost or split: {joined}");
        // a line that already fits is returned untouched
        assert_eq!(wrap(row_text("host", "gpu-box".into()), 40).len(), 1);
    }

    #[test]
    fn row_pads_the_label_to_the_one_shared_width() {
        let l = row_text("temp", "68C".into());
        assert_eq!(l.spans[0].content, format!("{:<LABEL_W$}", "temp"));
        assert_eq!(l.spans[1].content, "68C");
    }

    /// #199: the label cell holds the one grid even when the label does not fit it. The row that
    /// found this came from a READING written in another crate ("month at this rate", 18), so the
    /// guarantee has to live here: exactly `LABEL_W` columns, always ending in a space, so a
    /// value can never sit flush against its own label - and `wrap`'s hanging indent, which
    /// recognises a label row by this exact width, keeps working for such a row too.
    #[test]
    fn a_label_too_long_for_the_grid_is_cut_rather_than_pushing_its_value() {
        for label in ["temp", "month projected", &"x".repeat(LABEL_W), "month at this rate", &"y".repeat(60)] {
            let l = row_text(label, "$243.00".into());
            let cell = l.spans[0].content.clone();
            assert_eq!(cell.chars().count(), LABEL_W, "the label cell is always exactly one column wide: {cell:?}");
            assert!(cell.ends_with(' '), "a value must never touch its label: {cell:?}");
            assert_eq!(l.spans[1].content, "$243.00", "and the value itself is untouched");
        }
        // a cut label says it was cut, and the ones page 1 actually uses are never cut
        assert_eq!(row_text("month at this rate", String::new()).spans[0].content, "month at this\u{2026}  ");
        assert_eq!(row_text("month projected", String::new()).spans[0].content, "month projected ");
        // the continuation of an over-long label row still hangs under the value column
        let out = wrap(row_text(&"x".repeat(30), "one two three four five six seven".into()), 40);
        assert!(out.len() >= 2 && out[1].spans[0].content.chars().count() == LABEL_W, "{out:?}");
    }

    /// #114 (verifier-3): a sub-cent figure must show its real digits (never rounded to $0.00),
    /// and precision must NOT jump by 3+ decimals between two neighbouring values either side of
    /// one cent - the exact regression #114 found in #103's first fix (reverting it left every
    /// existing test green, so this is the test that was missing, not a re-statement of one).
    #[test]
    fn usd_precise_shows_real_digits_with_no_cliff_at_one_cent() {
        assert_eq!(usd_precise(Some(0.00031), "x"), "$0.000310", "the real figure the owner asked for must show, not round to $0.00");
        assert_eq!(usd_precise(Some(9.21), "x"), "$9.21", "a normal figure still reads like a normal dollar amount");
        assert_eq!(usd_precise(None, "no data"), "\u{2014} no data");
        // either side of the one-cent cliff: the VALUE must still be recoverable from both, and
        // neither may use wildly more decimals than the other's own order of magnitude needs
        let below = usd_precise(Some(0.00999), "x");
        let above = usd_precise(Some(0.0123), "x");
        assert_eq!(below, "$0.00999");
        assert_eq!(above, "$0.0123", "must show the real value, not the old cliff's rounded $0.01 (19% off)");
        let decimals = |s: &str| s.trim_start_matches('$').split('.').nth(1).map_or(0, str::len);
        assert!(decimals(&above).abs_diff(decimals(&below)) <= 1, "precision must change smoothly across the boundary, not jump by 3 decimals: {below} vs {above}");
    }

    /// #114 (verifier-3): teeth for the whole COST PER 1M TOKENS section, not just the formatter.
    /// This is the test whose ABSENCE let the sub-cent fix regress with the full suite still
    /// green. #227 moved the "not additive" prose line to `?` help (`ui::mod::HELP_GROUPS`) - the
    /// box shrank, the fact itself did not go anywhere unstated.
    #[test]
    fn cost_per_million_shows_sub_cent_digits() {
        let s = Status { cost: Some(CostStatus { today_usd_per_million_prompt_tokens: Some(0.00031), today_usd_per_million_generated_tokens: Some(9.21), ..Default::default() }), ..Status::default() };
        let sec = cost_per_million(&s);
        let all: String = sec.lines.iter().flat_map(|l| l.spans.iter().map(|sp| sp.content.to_string())).collect();
        assert!(all.contains("0.000310") || all.contains("0.00031"), "the sub-cent prompt figure must show its real digits: {all}");
        assert!(all.contains("9.21"), "{all}");
    }

    /// card #174 (the owner: "i think the cost per 1m token is off"): the REAL WORK figure must be
    /// the primary line and it must be its OWN value, never silently aliased to the old
    /// generated-only headline - the exact regression class the arithmetic fix could pass while
    /// the UI kept showing the wrong number, since nothing here asserted on the wired-up value
    /// before this test existed.
    #[test]
    fn cost_per_million_shows_the_real_work_headline_and_standby_separately() {
        let s = Status {
            cost: Some(CostStatus {
                today_usd_per_million_real_work_tokens: Some(0.87),
                today_usd_per_million_generated_tokens: Some(9.21),
                today_standby_usd: Some(0.44),
                ..Default::default()
            }),
            ..Status::default()
        };
        let sec = cost_per_million(&s);
        let all: String = sec.lines.iter().flat_map(|l| l.spans.iter().map(|sp| sp.content.to_string())).collect();
        assert!(all.contains("0.87"), "the real-work figure must be on screen, and distinct from the old generated-only one: {all}");
        assert!(all.contains("real work"), "the primary figure must be labelled, not just present: {all}");
        // card #174 item 3's own "the basis must be named" is satisfied by `? help` since #227
        // moved the prose line there (`ui::mod::HELP_GROUPS`'s "real work / generated / prompt"
        // entry) - this box now names only the VALUE, not the sentence explaining it.
        assert!(all.contains("0.44"), "standby must be shown separately (card #174 item 2): {all}");
        assert!(all.to_lowercase().contains("standby"), "{all}");
        // the two figures must never be the same string - that would mean the wiring silently
        // fell back to the old basis while claiming to be the new one
        assert_ne!(usd_precise(s.cost.as_ref().unwrap().today_usd_per_million_real_work_tokens, ""), usd_precise(s.cost.as_ref().unwrap().today_usd_per_million_generated_tokens, ""));
    }

    /// #118 (verifier-2): real values publish as real values, never held back as an em dash once
    /// the collector actually sends them.
    #[test]
    fn priority_shows_real_values_once_the_collector_publishes_them() {
        let mut s = Status::default();
        assert!(priority_text(&s).contains('\u{2014}'), "nothing published yet: an honest em dash");
        s.serve.public_priority = "10".into();
        s.serve.trusted_priority = "0".into();
        let text = priority_text(&s);
        assert!(text.contains("public 10") && text.contains("trusted 0") && !text.contains('\u{2014}'), "{text}");
    }

    /// #75/#73: a missing rate table is an honest em dash + reason, never a guessed $0.00, and
    /// the whole section says plainly that cost tracking is off rather than showing zeros.
    #[test]
    fn electricity_with_no_rate_table_says_tracking_is_off_not_zero() {
        let s = Status::default();
        let sec = electricity(&s, 1_000_000);
        assert!(sec.lines[0].spans.iter().any(|sp| sp.content.contains("cost tracking is off")), "{:?}", sec.lines);
    }

    /// card #73: `today` (since local midnight) and `last 24h` (rolling) are DIFFERENT windows
    /// and must never be presented as the same number - this pins that both appear, distinctly.
    #[test]
    fn electricity_shows_today_and_last_24h_as_two_distinct_rolling_windows() {
        // a `now` deep enough into the day that "fully covered" is a clean, round number of
        // seconds since local midnight (avoids the test itself depending on the host's own tz)
        let midnight = 1_000_000 - (1_000_000 % 86_400);
        let now = midnight + 3_600 * 5;
        let s = Status { cost: Some(CostStatus { rate_name: "test".into(), effective_date: "2026-01-01".into(), today_usd: Some(1.23), today_kwh: Some(4.0), last_24h_usd: Some(5.67), last_24h_kwh: Some(8.0), today_covered_secs: Some(now - midnight), last_24h_covered_secs: Some(86_400), ..Default::default() }), ..Status::default() };
        let sec = electricity(&s, now);
        let today = sec.lines.iter().find(|l| l.spans[0].content.trim() == "today").expect("a today row always exists");
        let last_24h = sec.lines.iter().find(|l| l.spans[0].content.trim() == "last 24h").expect("a last-24h row always exists");
        assert!(today.spans[1].content.contains("1.23") && today.spans[1].content.contains("since local midnight"), "{}", today.spans[1].content);
        assert!(last_24h.spans[1].content.contains("5.67") && last_24h.spans[1].content.contains("rolling"), "{}", last_24h.spans[1].content);
    }

    /// card #298: a rates.toml that states its source gets a "rate from" row right under "plan";
    /// one that does not gets no row at all (renders exactly as before).
    #[test]
    fn electricity_shows_where_the_rate_came_from_only_when_stated() {
        let mut s = Status { cost: Some(CostStatus { rate_name: "x".into(), effective_date: "2026-06".into(), rate_source: "CA avg (EIA 2026-06)".into(), ..Default::default() }), ..Status::default() };
        let sec = electricity(&s, 1_000_000);
        assert_eq!(sec.lines[1].spans[0].content.trim(), "rate from", "{:?}", sec.lines);
        assert!(sec.lines[1].spans[1].content.contains("CA avg (EIA 2026-06)"));
        s.cost.as_mut().unwrap().rate_source.clear();
        let sec = electricity(&s, 1_000_000);
        assert!(!sec.lines.iter().any(|l| l.spans[0].content.trim() == "rate from"));
    }

    /// #102 (verifier): a collector that has only been up a fraction of "today" must say so, not
    /// present a real but partial number under a confident "(since local midnight)" label.
    #[test]
    fn electricity_today_says_when_coverage_is_partial() {
        let midnight = 1_000_000 - (1_000_000 % 86_400);
        let now = midnight + 3_600 * 5; // 5h into the day
        let s = Status { cost: Some(CostStatus { rate_name: "test".into(), effective_date: "2026-01-01".into(), today_usd: Some(0.02), today_kwh: Some(0.06), today_covered_secs: Some(35 * 60), ..Default::default() }), ..Status::default() };
        let sec = electricity(&s, now);
        // #114: the caveat is now its OWN line (never a suffix a narrow pane could truncate away),
        // right after "today"'s value line - both must be present, but the caveat's presence is
        // no longer conditioned on it fitting on the SAME line as the dollar figure.
        let today_idx = sec.lines.iter().position(|l| l.spans[0].content.trim() == "today").unwrap();
        let today_text: String = sec.lines[today_idx].spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(today_text.contains("$0.02"), "{today_text}");
        let caveat_text: String = sec.lines[today_idx + 1].spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(caveat_text.contains("35m") && caveat_text.contains("not the full window"), "{caveat_text}");
    }

    /// card #181: the owner asked for "everyday, every week, every month". The week and the month
    /// were computed by #176 and then lived only in `lss status` and on page 5 - he had to press
    /// a key to see the question he asks. They are rows in THIS box now, and this pins that they
    /// are projections of the readings (same words) and that a thin window or a refused
    /// projection states its reason instead of showing a number nobody should trust.
    #[test]
    fn electricity_carries_the_week_the_month_and_a_refused_projections_reason() {
        use lss_core::rates::{SpendWindow, SpendingStatus};
        let midnight = 1_000_000 - (1_000_000 % 86_400);
        let now = midnight + 3_600 * 5;
        let s = Status {
            cost: Some(CostStatus { rate_name: "test".into(), effective_date: "2026-01-01".into(), today_usd: Some(0.02), ..Default::default() }),
            spending: Some(SpendingStatus {
                this_week: SpendWindow { usd: Some(48.0), kwh: Some(120.0), covered_secs: 6 * 86_400, nominal_secs: 6 * 86_400 },
                this_month: SpendWindow { usd: Some(6.0), kwh: Some(15.0), covered_secs: 86_400, nominal_secs: 3 * 86_400 },
                month_projection_usd: None,
                projection_note: "too early to project: 3 of 30 days into the month (needs 7)".into(),
                ..Default::default()
            }),
            ..Status::default()
        };
        let sec = electricity(&s, now);
        let text = |i: usize| -> String { sec.lines[i].spans.iter().map(|sp| sp.content.as_ref()).collect() };
        let idx = |label: &str| sec.lines.iter().position(|l| l.spans[0].content.trim() == label).unwrap_or_else(|| panic!("no {label:?} row in:\n{}", (0..sec.lines.len()).map(text).collect::<Vec<_>>().join("\n")));

        let week = idx("this week");
        assert!(text(week).contains("$48.00"), "{}", text(week));
        let month = idx("month to date"); // card #226: relabelled
        assert!(text(month).contains("$6.00"), "{}", text(month));
        // the thin month's caveat is its OWN line under the figure (#114's rule, this box's
        // renderer) - truncation must eat the dollars before it eats the warning
        assert!(text(month + 1).contains("not the full window"), "the thin month must say so: {}", text(month + 1));
        // and the refusal is visible, not a blank
        let all: String = (0..sec.lines.len()).map(text).collect::<Vec<_>>().join("\n");
        assert!(all.contains("no month projection: too early to project"), "{all}");
        assert!(!all.contains("month projected \u{b7}"), "no figure is printed for a refused projection\n{all}");
    }

    /// card #73: a window with no rollup history yet is a stated reason, never a fabricated 0.
    #[test]
    fn tokens_window_with_no_rollup_history_says_so_not_zero() {
        let s = Status {
            tokens_by_window: Some(lss_core::model::TokenWindows { hour: Some(WorkWindow { prompt: 10.0, cached: 2.0, generated: 3.0, requests: 1.0 }), hour_covered_secs: Some(3_600), day: None, day_covered_secs: None, week: None, week_covered_secs: None, month: None, month_covered_secs: None }),
            ..Status::default()
        };
        let sec = tokens(&s);
        let hour = sec.lines.iter().find(|l| l.spans[0].content.trim() == "hour").unwrap();
        assert!(hour.spans[1].content.contains("10") && hour.spans[1].content.contains("3"), "{}", hour.spans[1].content);
        let day = sec.lines.iter().find(|l| l.spans[0].content.trim() == "day").unwrap();
        assert!(day.spans[1].content.contains("not enough rollup history"), "{}", day.spans[1].content);
    }

    /// #102 (verifier): a `day` window that DOES exist but is not yet 95% of its own nominal
    /// 86,400s says so - the exact case where "day" and "week" read as suspiciously identical
    /// numbers on a collector that has only been up 35 minutes.
    #[test]
    fn tokens_window_with_partial_coverage_says_so() {
        let s = Status {
            tokens_by_window: Some(lss_core::model::TokenWindows {
                hour: Some(WorkWindow { prompt: 10.0, cached: 2.0, generated: 3.0, requests: 1.0 }),
                hour_covered_secs: Some(35 * 60),
                day: Some(WorkWindow { prompt: 10.0, cached: 2.0, generated: 3.0, requests: 1.0 }),
                day_covered_secs: Some(35 * 60),
                week: None,
                week_covered_secs: None,
                month: None,
                month_covered_secs: None,
            }),
            ..Status::default()
        };
        let sec = tokens(&s);
        // 35m of coverage is short of BOTH hour's 3,600s and day's 86,400s nominal length (< 95%
        // of either), so both say so - #114: each caveat is now its own line, right after its
        // value line, never a suffix that could be truncated away.
        let day_idx = sec.lines.iter().position(|l| l.spans[0].content.trim() == "day").unwrap();
        let day_caveat: String = sec.lines[day_idx + 1].spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(day_caveat.contains("35m") && day_caveat.contains("not the full window"), "{day_caveat}");
        let hour_idx = sec.lines.iter().position(|l| l.spans[0].content.trim() == "hour").unwrap();
        let hour_caveat: String = sec.lines[hour_idx + 1].spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(hour_caveat.contains("35m") && hour_caveat.contains("not the full window"), "{hour_caveat}");
    }

    #[test]
    fn hold_samples_bridges_the_grid_between_samples_but_never_a_real_outage() {
        let (a, b, c) = (Some(1.0), Some(2.0), Some(3.0));
        // every other slot: continuous, each sample held across its own span
        assert_eq!(hold_samples(&[None, a, None, b, None, c]), vec![None, a, a, b, b, c]);
        // a run longer than the usual spacing is the collector being down: it stays a gap
        assert_eq!(hold_samples(&[a, None, b, None, c, None, None, None, None, a, None, b]), vec![a, a, b, b, c, None, None, None, None, a, a, b]);
        // already continuous, empty, all-None, one sample: unchanged
        assert_eq!(hold_samples(&[a, b, c]), vec![a, b, c]);
        assert_eq!(hold_samples(&[]), Vec::<Option<f64>>::new());
        assert_eq!(hold_samples(&[None, None]), vec![None, None]);
        assert_eq!(hold_samples(&[None, a, None]), vec![None, a, None]);
    }

    /// card #309 (lss-inst-v1's probe on #305): with only TWO samples in the hour there is one
    /// spacing, seen once - that is not a sampling rate, it is a guess. 14.5 minutes of nothing
    /// between them is an outage and must stay a gap.
    #[test]
    fn hold_samples_never_bridges_a_spacing_seen_only_once() {
        let (a, b) = (Some(1.0), Some(2.0));
        let mut data = vec![a];
        data.extend(std::iter::repeat_n(None, 28));
        data.push(b);
        let held = hold_samples(&data);
        assert_eq!(held.iter().filter(|v| v.is_none()).count(), 28, "two samples 14.5 min apart: the 28 slots between stay empty");
        assert_eq!(held, data);
    }

    /// card #315 (lss-inst-1's probe on #309): three samples with two EQUAL 14.5-minute gaps -
    /// the spacing 29 is "seen twice", but it is two outages, not a sampling rate. A spacing
    /// wider than the grid's sampling can ever be must never be bridged, however often it repeats.
    #[test]
    fn hold_samples_never_bridges_repeated_long_outages() {
        let (a, b, c) = (Some(1.0), Some(2.0), Some(3.0));
        let mut data = vec![a];
        data.extend(std::iter::repeat_n(None, 28));
        data.push(b);
        data.extend(std::iter::repeat_n(None, 28));
        data.push(c);
        let held = hold_samples(&data);
        assert_eq!(held.iter().filter(|v| v.is_none()).count(), 56, "both 14.5-min outages stay empty");
        assert_eq!(held, data);
    }

    /// #179 item 4: the GPU boxes are a RANKED projection of the readings - a hot card moves to
    /// the front, an excluded (known-hot) card never does, and a degree of drift inside a band
    /// never reorders them.
    #[test]
    fn gpu_boxes_are_ranked_worst_first_by_band_and_stable_within_one() {
        use lss_core::gpu::GpuSample;
        use lss_core::model::GpuStatus;
        let mut s = Status::default();
        s.thresholds.thermal_temp_c = 90.0;
        s.gpus = (0..4).map(|i| GpuStatus { sample: GpuSample { index: i, temp_c: Some(60.0), ..Default::default() }, ..Default::default() }).collect();
        let order = |s: &Status| ranked_gpu_boxes(s, 24, false).into_iter().map(|b| b.title).collect::<Vec<_>>();
        assert_eq!(order(&s), ["GPU0", "GPU1", "GPU2", "GPU3"], "all fine: index order");
        s.gpus[1].sample.temp_c = Some(61.5); // drift inside the ok band
        assert_eq!(order(&s), ["GPU0", "GPU1", "GPU2", "GPU3"], "drift must not reorder");
        s.gpus[2].sample.temp_c = Some(92.0);
        let boxes = ranked_gpu_boxes(&s, 24, false);
        assert_eq!(boxes[0].title, "GPU2", "the hot card leads");
        assert_eq!(boxes[0].colour, Color::Red);
        assert_eq!(boxes[1].colour, Color::LightGreen);
        s.gpus[3].sample.temp_c = Some(99.0);
        s.gpus[3].thermal_excluded = true;
        assert_eq!(order(&s)[0], "GPU2", "a card with thermal alerts off never jumps the queue");
        assert_eq!(order(&s)[3], "GPU3*");
    }

    /// Card #196: ONE table turns a band into ink, and RED means `Critical` or `Offline` and
    /// nothing else. Page 1 previously held two mappings of the same bands - the verdict line
    /// painted `High` amber while a GPU box painted it red - which is the second scale #179
    /// exists to delete. Both directions are pinned here so neither can drift back.
    #[test]
    fn one_table_turns_a_band_into_ink_and_red_means_critical_or_offline() {
        use lss_core::readings::{Reading, HIGH, MEDIUM};
        let r = |score: f64| Reading::live("k", "l", "v", "d.", score);
        assert_eq!(band_colour(&r(0.0)), None, "ok is plain ink");
        assert_eq!(band_colour(&r(MEDIUM - 0.01)), Some(Color::Yellow), "watch is amber");
        assert_eq!(band_colour(&r(MEDIUM)), Some(Color::Yellow), "HIGH is amber, NOT red - red is for a problem, and nothing else");
        assert_eq!(band_colour(&r(HIGH)), Some(Color::Red), "critical is red");
        assert_eq!(band_colour(&Reading::offline("k", "l", "gone")), Some(Color::Red), "a dead source is red");
        assert_eq!(band_colour(&Reading::no_data("k", "l", "never measured")), None, "no data is not a fault");
        // an informational row is never coloured as a fault, however high it scores: this is the
        // chosen-power-cap case (#179 measured it headlining a healthy box on 19.7% of polls),
        // and before this card the GPU box patched `warn()` over its band anyway.
        assert_eq!(band_colour(&r(1.0).informational()), None, "a fact about the setup is not a fault");
        // and the frame follows the same table, worst-first, with the box's calm colour intact
        assert_eq!(section_colour(&[r(0.0), r(MEDIUM)], Color::LightGreen), Color::Yellow);
        assert_eq!(section_colour(&[r(0.0), r(HIGH)], Color::LightGreen), Color::Red);
        assert_eq!(section_colour(&[r(0.0), r(1.0).informational()], Color::LightGreen), Color::LightGreen, "a big cost number does not turn a box red");
        assert_eq!(section_colour(&[], Color::Yellow), Color::Yellow);
    }

    /// Card #196 item 4: ALERTS and INCIDENTS are projections too, so the two boxes beside the
    /// verdict line cannot disagree with it about the same event. An `info` alert that the line
    /// ranks below LOW must not be amber in the box next to it, and an open incident on an
    /// otherwise healthy box must not be red beside a line saying OK.
    #[test]
    fn the_event_boxes_cannot_contradict_the_verdict_line_about_the_same_event() {
        use lss_core::incidents::Incident;
        use lss_core::model::AlertRow;
        let mut s = Status::default();
        s.serve.up = true;
        s.generated_at = 1_800_000_000;
        s.gate.up = true;

        // an INFO alert firing: the verdict line refuses to headline it, so the box is not red
        let mut info = s.clone();
        info.firing = vec!["c1_stale".into()];
        info.alerts = vec![AlertRow { rule: "c1_stale".into(), severity: "info".into(), message: "C1 could not measure".into(), ts: s.generated_at, ..Default::default() }];
        let sec = alerts(&info);
        assert_eq!(sec.colour, Color::DarkGray, "an info alert does not redden the box the page will not headline");
        let row = sec.lines.iter().find(|l| l.spans[0].content.trim() == "firing now").unwrap();
        assert!(row.spans[1].style != red() && row.spans[1].style != warn(), "{:?}", row.spans[1]);
        // ...and the alert's own listed row is not amber either: the severity word becomes a band
        // in `readings::alert_score` and nowhere else, so a box cannot re-derive it and disagree
        let listed = sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.trim() == "info").expect("the listed alert's severity");
        assert_eq!(listed.style, Style::default(), "an info alert is not a problem in this box either: {listed:?}");

        // a CRITICAL one does both, and its listed row is red
        let mut crit = info.clone();
        crit.firing = vec!["gpu_hot".into()];
        crit.alerts = vec![AlertRow { rule: "gpu_hot".into(), severity: "critical".into(), message: "GPU3 at 94C".into(), ts: s.generated_at, ..Default::default() }];
        let sec = alerts(&crit);
        assert_eq!(sec.colour, Color::Red);
        assert_eq!(sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.trim() == "critical").unwrap().style, red());
        // a WARN one is amber in both places, never red
        let mut w = info.clone();
        w.firing = vec!["gate_waiters".into()];
        w.alerts = vec![AlertRow { rule: "gate_waiters".into(), severity: "warn".into(), message: "queue pressure".into(), ts: s.generated_at, ..Default::default() }];
        let sec = alerts(&w);
        assert_eq!(sec.colour, Color::Yellow);
        assert_eq!(sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.trim() == "warn").unwrap().style, warn());

        // an open incident with nothing else wrong: shown, not painted as a problem
        let mut stale = s.clone();
        stale.incidents = vec![Incident { id: 1, start: s.generated_at - 500_000, end: None, kind: "xid".into(), detail: "d".into() }];
        let sec = incidents(&stale, s.generated_at);
        assert_eq!(sec.colour, Color::Magenta, "an incident nobody closed is not a live fault");
        let open_span = sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.starts_with("OPEN")).expect("the OPEN span");
        assert_eq!(open_span.style, Style::default(), "...and it is not red beside a verdict line saying OK");
        // the same incident while the engine is actually down IS coloured
        let mut down = stale.clone();
        down.serve.up = false;
        let sec = incidents(&down, s.generated_at);
        assert_ne!(sec.colour, Color::Magenta, "corroborated by a second source, it counts");
    }

    /// CARD #198: the last two colours on page 1 that no reading owned - the `[not delivered]`
    /// marker and the word `xid` - were painted by KIND, by this file, on the one page whose
    /// whole point is that severity is decided in one place. Both now take the band of their own
    /// row's reading, so this test is about the INK, not about the scores (those are pinned in
    /// `lss_core::readings`).
    #[test]
    fn the_last_two_hand_painted_ambers_take_their_ink_from_a_reading() {
        use lss_core::incidents::Incident;
        use lss_core::model::AlertRow;
        let mut base = Status::default();
        base.serve.up = true;
        base.gate.up = true;
        base.generated_at = 1_800_000_000;
        let now = base.generated_at;
        let alert = |id: i64, sev: &str, delivered: bool| AlertRow { id, ts: now - 60, rule: format!("r{id}"), severity: sev.into(), message: "something happened".into(), recovered: false, delivered };
        let marker = |sec: &Section| sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.starts_with("[not delivered]")).expect("the marker").style;

        // a sink that demonstrably works here dropped a WARN: amber, and the box says so
        let mut s = base.clone();
        s.alerts = vec![alert(1, "warn", false), alert(2, "warn", true)];
        let sec = alerts(&s);
        assert_eq!(marker(&sec), warn(), "un-paged about a warn alert is the one case this marker is a fault");
        assert_eq!(sec.colour, Color::Yellow);

        // the same flag on an INFO alert is not: on this box all 13 undelivered alerts on record
        // are info, so the old hand-painted amber was a second way of painting `info` amber
        let mut s = base.clone();
        s.alerts = vec![alert(1, "info", false), alert(2, "warn", true)];
        let sec = alerts(&s);
        assert_eq!(marker(&sec), dim(), "shown, quietly - never the amber the verdict line refuses");
        assert_eq!(sec.colour, Color::DarkGray);

        // a box where nothing has ever been handed over has no sink to have failed
        let mut s = base.clone();
        s.alerts = vec![alert(1, "critical", false)];
        let sec = alerts(&s);
        assert_eq!(marker(&sec), dim(), "a missing alert sink is not a delivery failure");
        assert_eq!(sec.colour, Color::DarkGray);

        // INCIDENTS: a fresh Xid is amber; the same record an hour later is not, and the box
        // frame follows the rows it shows
        let xid = |ago: i64| Incident { id: 7, start: now - ago, end: Some(now - ago), kind: "xid".into(), detail: "GPU0 Xid 8".into() };
        let kind_span = |sec: &Section| sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.starts_with("xid")).expect("the kind word").style;
        let mut s = base.clone();
        s.incidents = vec![xid(300)];
        let sec = incidents(&s, now);
        assert_eq!(kind_span(&sec), warn(), "the driver reported a hardware fault five minutes ago");
        assert_eq!(sec.colour, Color::Yellow);
        let mut s = base.clone();
        s.incidents = vec![xid(lss_core::readings::XID_FRESH_SECS + 1)];
        let sec = incidents(&s, now);
        assert_eq!(kind_span(&sec), Style::default(), "an hour on it is a dated record: ink that is always lit says nothing");
        assert_eq!(sec.colour, Color::Magenta);
    }

    /// Card #196 item 3: MONEY IS A FACT, NOT A SEVERITY, and that is now structural rather than
    /// a convention. The money boxes' frames go through the same `section_colour` as every other
    /// box, over the rows they actually show; they come out calm because every `cost.*` and
    /// `loadout.*` reading is `informational`. Pulse's own note on why the flag exists: facts
    /// about the setup outscored every real reading on an idle machine.
    #[test]
    fn the_biggest_dollar_figure_on_the_page_can_neither_colour_a_box_nor_take_the_headline() {
        let mut s = Status::default();
        s.serve.up = true;
        s.generated_at = 1_800_000_000;
        s.cost = Some(CostStatus {
            effective_date: "2026-01-01".into(),
            live_usd_per_hour: Some(9_999.99),
            current_usd_per_kwh: Some(9.5),
            today_usd: Some(99_999.99),
            today_usd_per_million_real_work_tokens: Some(812.5),
            ..Default::default()
        });
        s.loadout = Some(lss_core::model::LoadoutBrief { model: "a-model".into(), flags: "--tp 4".into(), ..Default::default() });
        // every money and loadout reading is informational, so none of them can be scored
        let money = lss_core::readings::cost(&s);
        assert!(!money.is_empty() && money.iter().all(|r| r.informational), "{money:#?}");
        assert!(lss_core::readings::loadout(&s).iter().all(|r| r.informational));
        // ...so the boxes that show them keep their own colour, whatever the figures are
        assert_eq!(electricity(&s, s.generated_at).colour, Color::Yellow);
        assert_eq!(cost_per_million(&s).colour, Color::Magenta);
        assert_eq!(loadout_colour(&s), Color::Blue);
        // ...and the meter on the verdict line rides there in plain ink, never as a problem
        let meter = money.iter().find(|r| r.key == "cost.live").unwrap();
        assert_eq!(band_colour(meter), None);
        assert!(meter.value.as_deref().is_some_and(|v| v.contains("9999.99")), "{meter:?}");

        // TWO LOCKS, and this asserts the second one does the work on its own. Every money row
        // also happens to score 0.0 today, so the `informational` flag is currently belt as well
        // as braces - which would make a test that only reads the flag prove nothing about the
        // page. Give the money box a row that scores CEILING and is informational (what a future
        // "you are over budget" row would look like if someone scored it), and the box must still
        // be calm: the flag, not the score, is what keeps money out of the ink.
        let mut scored = money.clone();
        scored.push(lss_core::readings::Reading::live("cost.alarming", "cost", "$1M/h", "d.", 1.0).informational());
        assert_eq!(section_colour(&scored, Color::Yellow), Color::Yellow, "a scored money row must still not colour its box");
        assert_eq!(lss_core::readings::leading(&scored, None), None, "...nor take the headline");
        assert_eq!(lss_core::readings::leading(&scored, Some("cost.alarming")), None, "...not even as the incumbent");
        // the one thing that DOES colour LOADOUT is the status row it carries: a box saying the
        // engine is down must not be drawn calm
        let mut down = s.clone();
        down.serve.up = false;
        assert_eq!(loadout_colour(&down), Color::Red, "the frame must not contradict its own status row");
    }

    /// Card #196: SERVE and LANES take every colour from the readings that already judge the same
    /// numbers. Before this, LANES decided on its own that any 5xx is red and any 4xx is amber
    /// while `readings::gateway` scored those exact facts on the shared scale, and SERVE compared
    /// the queue against `s.thresholds.queue_reqs` a second time with a different verdict.
    #[test]
    fn serve_and_lanes_take_their_ink_from_the_readings_that_judge_the_same_numbers() {
        let mut s = Status::default();
        s.serve.up = true;
        s.serve.slots = 8;
        s.serve.running = 2.0;
        s.gate.up = true;
        s.thresholds.queue_reqs = 4.0;

        // a calm box colours nothing (#227 item 5: LANES' own colour moved off warn()'s amber)
        let sec = lanes(&s);
        assert_eq!(sec.colour, Color::Blue, "a calm LANES box keeps its own colour");
        assert!(sec.lines.iter().flat_map(|l| &l.spans).all(|sp| sp.style != red()), "nothing red on a calm box");

        // 5xx in the PUBLIC lane colours the public row and the frame - and leaves trusted alone.
        // Six failures in ten minutes scores 0.62 = Band::High, which is AMBER on the one table
        // (the renderer used to paint any 5xx at all red, on its own boolean); a run of them
        // reaches Critical and turns red, which is the scale doing the work instead of a flag.
        let mut bad = s.clone();
        bad.lanes.public.codes_10m.c5xx = 6;
        let sec = lanes(&bad);
        assert_eq!(sec.colour, Color::Yellow);
        let failed = sec.lines.iter().flat_map(|l| &l.spans).find(|sp| sp.content.contains("6 failed")).expect("the public lane's failures");
        assert_eq!(failed.style, warn());
        let clean = sec.lines.iter().find(|l| l.spans.iter().any(|sp| sp.content.contains("0 failed"))).expect("the clean lane's row");
        assert!(clean.spans.iter().all(|sp| sp.style == Style::default() || sp.style == dim()), "the other lane is not painted with it");
        bad.lanes.public.codes_10m.c5xx = 20;
        let sec = lanes(&bad);
        assert_eq!(sec.colour, Color::Red, "a run of failures does reach red");

        // the queue's colour is `serve.queue`'s band, which ramps to the owner's own queue line
        let mut q = s.clone();
        q.serve.queue = 6.0; // past thresholds.queue_reqs (4) * 1.5
        let sec = serve(&q, 60, Trend { width: 60 + LABEL_W, lines: false });
        let row = sec.lines.iter().find(|l| l.spans[0].content.trim() == "running").unwrap();
        let qspan = row.spans.iter().find(|sp| sp.content.trim() == "6").expect("the queue count");
        assert_eq!(qspan.style, red(), "a queue past the owner's line is red: {qspan:?}");
        assert_eq!(sec.colour, Color::Red, "and so is the box");
        // ...and a queue of ONE is not coloured at all. The renderer used to paint any queue > 0
        // amber, on its own boolean; on the one scale a single request waiting for the next
        // answer to finish is what a busy server looks like, and it scores below Watch. Three
        // deep against the same line is amber. That is the scale doing the work.
        let queue_style = |n: f64| {
            let mut q = s.clone();
            q.serve.queue = n;
            let sec = serve(&q, 60, Trend { width: 60 + LABEL_W, lines: false });
            let row = sec.lines.iter().find(|l| l.spans[0].content.trim() == "running").unwrap().clone();
            row.spans.iter().find(|sp| sp.content.trim() == format!("{n:.0}")).unwrap().style
        };
        assert_eq!(queue_style(1.0), Style::default(), "one request waiting is not a fault");
        assert_eq!(queue_style(3.0), warn());
    }

    /// Card #196: GPUS' straggler row and the box's own frame are projections of `gpu.skew`, so
    /// an idle box with wildly uneven cards is neither coloured nor described as a problem - the
    /// measured trap (raw spread p90 92 idle against 54 busy).
    /// #255 round 3 (the owner, polish): the skew value moved from its own "util skew" row into
    /// the ONE-LINE "total" summary ("total 57/1200 W \u{b7} util 0% \u{b7} skew 0 pts \u{b7} ...") -
    /// this now finds the "total" row and picks out the skew value SPAN inside it by shape
    /// (ends in "pts", or is the no-data dash) rather than a whole row of its own.
    #[test]
    fn the_gpus_box_projects_the_skew_reading_and_stays_calm_on_an_idle_box() {
        use lss_core::gpu::GpuSample;
        use lss_core::model::GpuStatus;
        let skew_span = |sec: &Section| -> Span<'static> {
            let row = sec.lines.iter().find(|l| l.spans[0].content.trim() == "total").expect("the total row");
            row.spans.iter().find(|sp| sp.content.ends_with("pts") || sp.content == "\u{2014}").cloned().expect("the skew span")
        };
        let mut s = Status::default();
        s.serve.up = true;
        s.thresholds.thermal_temp_c = 90.0;
        // #197: the SCORED input is the collector's windowed median, not the instantaneous
        // sample beside it - a card that dipped on this one poll is not a straggler
        s.gpus = (0..4)
            .map(|i| GpuStatus {
                sample: GpuSample { index: i, temp_c: Some(60.0), util_pct: Some(if i == 3 { 100.0 } else { 0.0 }), ..Default::default() },
                util_pct_med: Some(if i == 3 { 100.0 } else { 0.0 }),
                ..Default::default()
            })
            .collect();
        let sec = gpus(&s, 80, false);
        let skew = skew_span(&sec);
        assert_eq!(skew.style, Style::default(), "an idle box's uneven cards are not coloured: {skew:?}");
        assert_eq!(sec.colour, Color::LightGreen, "nor is the box");
        // the same cards, with the engine actually running requests and the pack loaded
        let mut busy = s.clone();
        busy.serve.running = 3.0;
        for (i, g) in busy.gpus.iter_mut().enumerate() {
            g.sample.util_pct = Some(if i == 3 { 20.0 } else { 95.0 });
            g.util_pct_med = Some(if i == 3 { 20.0 } else { 95.0 });
        }
        let sec = gpus(&busy, 80, false);
        let skew = skew_span(&sec);
        // #255 round 3: the summary line takes just the reading's leading "<n> pts" now, not its
        // whole sentence ("75 pts behind the pack") - the full sentence is lss_core's own still,
        // this is a prefix of it, not a rewrite (see `gpus()`'s own note on this).
        assert_eq!(skew.content, "75 pts", "the laggard's distance from the pack MEDIAN, not max-min: {}", skew.content);
        assert_eq!(skew.style, red(), "a real straggler in a working group is coloured");
        assert_eq!(sec.colour, Color::Red);

        // #197: a ONE-POLL dip is what this reading used to fire on, and the row must now show
        // the window it judged over rather than a number a single sample could move
        let mut blip = busy.clone();
        for (i, g) in blip.gpus.iter_mut().enumerate() {
            g.sample.util_pct = Some(if i == 3 { 20.0 } else { 95.0 });
            g.util_pct_med = Some(95.0); // every card has been at the pack all window
        }
        let sec = gpus(&blip, 80, false);
        let skew = skew_span(&sec);
        assert_eq!(skew.content, "0 pts", "{}", skew.content);
        assert_eq!(skew.style, Style::default(), "one poll is not a straggler");
        assert_eq!(sec.colour, Color::LightGreen);

        // a collector too old (or too fresh) to publish the median says so; it does NOT fall
        // back to the instantaneous vector, which is the defect wearing a new field name
        let mut old = busy.clone();
        for g in &mut old.gpus {
            g.util_pct_med = None;
        }
        let sec = gpus(&old, 80, false);
        let skew = skew_span(&sec);
        assert_eq!(skew.content, "\u{2014}");
        assert_eq!(sec.colour, Color::LightGreen, "no data is not a fault");
    }
}
