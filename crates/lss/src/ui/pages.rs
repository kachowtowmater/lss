//! The five full-screen detail pages. Each is a list of boxed SECTIONS (a chart, a distribution,
//! a table); wide panes show two chart sections per row, small panes show fewer sections and
//! scroll (up/down, PgUp/PgDn). The section at the top of the screen is the focused one (thick).
//!
//! What each page replaces:
//!   1 LATENCY  Grafana SGLang: e2e latency + TTFT p50/p90/p99/avg over time, latency heatmaps
//!   2 LOAD     Grafana SGLang: running / queued requests, generation throughput, cache hit rate
//!   3 GPUS     NVIDIA DCGM: temperature, power (+total), SM clock, utilisation, framebuffer used
//!   4 USERS    who is on the server: running now, limits, requests and tokens per user
//!   5 TOKENS   how many tokens were served: windows, all time, per hour, peaks, lengths, per user
//!   6 MODEL    how good this model is on this hardware: speed by users at once, memory, energy,
//!              the bench scorecard against the previous and the best loadout, the comparison
//!   7 GATEWAY  (no Grafana equivalent) the gate's lanes, keys, admission control, client addresses
//!   8 ALERTS   Prometheus rules page + Alertmanager: rule states, alert history
//!   9 INCIDENTS  what happened, dated: restarts, outages, Xid errors, maintenance, uptime %
//!   a ADVICE   plain-English evidence for settings and upgrade decisions (card #231: `0` stays
//!              the overview's own key, so ADVICE - the tenth page - took the next free one)

use super::widgets::{bold, dim, draw_box, draw_chart, draw_heat, draw_hbars, fg, fit, fit_line, gauge, good, red, render_lines, warn, ChartOpts, Marker, Series, MIN_PANEL_W};
use super::{footer, page_colour, App};
use crate::data::{range_label, PageData, PageId};
use lss_core::hist::LATENCY_METRICS;
use lss_core::model::Status;
use lss_core::series::{GatewayRow, HistDoc, SeriesDoc};
use lss_core::timeutil::{fmt_duration, fmt_local};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Two chart sections share a row from this width on.
pub const TWO_COLUMNS: u16 = 100;
/// Series colours: never red (red is for problems, the Terminal Board rule this UI follows).
/// #48, 2026-09-21: this order is chosen to work on BOTH themes (dark and light background) -
/// these eight are the terminal's own named ANSI colours, which every terminal themes with
/// enough contrast against ITS OWN opposite-of-background to stay legible either way (unlike a
/// literal RGB pick, which only this app could get consistently right for both). The same
/// GPU always gets the same colour on every metric and every page: index `k` (this GPU's
/// position among the ones currently reporting) picks `GPU_COLOURS[k % 8]`, nothing per-chart.
/// A 16-colour terminal still tells GPU0..GPU3 apart: Green/Cyan/Magenta/Blue are four distinct
/// low-intensity ANSI hues. Card #223 (the owner: "the colors are off"): never YELLOW either -
/// yellow is `warn()`, the amber band, so a GPU drawn in it read as a warning that was not there
/// (it used to be GPU1's colour on every chart of this page).
const GPU_COLOURS: [Color; 8] = [Color::Green, Color::Cyan, Color::Magenta, Color::Blue, Color::LightGreen, Color::LightCyan, Color::LightMagenta, Color::LightBlue];

/// Card #223, the one-band rule (#196/#198) on this page: a reading's colour comes ONLY from its
/// `lss_core::readings` band - red for Critical/Offline, amber for High/Watch, nothing for an
/// informational reading (a card with thermal alerts off) or an ok one. The same mapping as
/// page 1's `dash::band_colour`, which is not this card's to share out of dash.rs.
fn band_style(r: &lss_core::readings::Reading) -> Style {
    use lss_core::readings::Band;
    if r.informational {
        return Style::default();
    }
    match r.band() {
        Band::Critical | Band::Offline => red(),
        Band::High | Band::Watch => warn(),
        _ => Style::default(),
    }
}

enum Body {
    Chart { series: Vec<Series>, opts: ChartOpts },
    Dist(HistDoc),
    Lines(Vec<Line<'static>>),
}

struct Section {
    title: String,
    detail: Vec<Span<'static>>,
    colour: Color,
    /// may share a row with another half section on a wide pane
    half: bool,
    min_h: u16,
    want_h: u16,
    body: Body,
}

pub fn draw(f: &mut Frame, app: &App, s: &Status, page: PageId, area: Rect, now: i64) {
    let area = super::reserve_key_hints(f, area);
    if area.height < 3 {
        return render_lines(f, area, vec![super::header(app, s, area.width, now)]);
    }
    // card #227 item 6: header + tab strip at the BOTTOM, above the footer; the boxes start on the
    // first row. #182's rule holds there: the tab strip always keeps its row.
    let (body, foot) = super::draw_chrome_bottom(f, app, s, area, now, Vec::new());
    if body.height < 1 {
        return;
    }
    let data = Some(&app.page_data).filter(|d| d.is_for(page, app.range_idx));
    let sections = build(page, app, s, data, now, body);
    layout(f, app, sections, body);
    // after the layout: the footer shows which rows are on screen
    f.render_widget(Paragraph::new(footer(app, area.width)), foot);
}

fn layout(f: &mut Frame, app: &App, sections: Vec<Section>, body: Rect) {
    let two = body.width >= TWO_COLUMNS;
    // rows of section indices
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for (i, sec) in sections.iter().enumerate() {
        match rows.last_mut() {
            Some(last) if two && sec.half && last.len() == 1 && sections[last[0]].half => last.push(i),
            _ => rows.push(vec![i]),
        }
    }
    let min_of = |r: &Vec<usize>| r.iter().map(|i| sections[*i].min_h).max().unwrap_or(3).min(body.height.max(1));
    let want_of = |r: &Vec<usize>| r.iter().map(|i| sections[*i].want_h).max().unwrap_or(3);
    // A row of LISTS is placed at the height it wants (a list is read, not squeezed); a row with
    // a chart in it at its minimum, as charts stay readable when they shrink. Only the last
    // row on screen may be a list shown in part: it says how many lines are hidden.
    let is_list = |r: &Vec<usize>| r.iter().all(|i| matches!(sections[*i].body, Body::Lines(_)));
    let pack_of = |r: &Vec<usize>| if is_list(r) { want_of(r).min(body.height.max(1)) } else { min_of(r) };
    // never scroll past the point where the rest of the page fits: no blank screen at the end
    let mut last_start = rows.len().saturating_sub(1);
    let mut tail = 0;
    for (i, r) in rows.iter().enumerate().rev() {
        tail += pack_of(r);
        if tail > body.height {
            break;
        }
        last_start = i;
    }
    let start = app.scroll.min(last_start);
    let mut shown = 0;
    let mut used = 0;
    let mut partial = false;
    for r in &rows[start..] {
        let need = pack_of(r);
        if used + need <= body.height || shown == 0 {
            used += need.min(body.height);
            shown += 1;
        } else {
            if is_list(r) && used + min_of(r) <= body.height {
                shown += 1;
                partial = true;
            }
            break;
        }
    }
    app.page_rows.set((start, shown, rows.len()));
    let visible = &rows[start..start + shown];
    let mut h: Vec<u16> = visible.iter().enumerate().map(|(k, r)| if partial && k + 1 == shown { min_of(r) } else { pack_of(r) }).collect();
    let mut spare = body.height.saturating_sub(h.iter().sum());
    // first up to what each row wants, then the charts share the rest
    for (k, r) in visible.iter().enumerate() {
        let add = want_of(r).saturating_sub(h[k]).min(spare);
        h[k] += add;
        spare -= add;
    }
    // charts may grow past what they asked for, up to a point where more height shows nothing new
    const CHART_MAX_H: u16 = 16;
    let growable: Vec<usize> = visible.iter().enumerate().filter(|(_, r)| r.iter().any(|i| !matches!(sections[*i].body, Body::Lines(_)))).map(|(k, _)| k).collect();
    let mut progress = true;
    while spare > 0 && progress {
        progress = false;
        for k in &growable {
            if spare > 0 && h[*k] < CHART_MAX_H {
                h[*k] += 1;
                spare -= 1;
                progress = true;
            }
        }
    }
    // whatever is still left belongs to the last box on screen: the page ends where the pane ends
    if let Some(last) = h.last_mut() {
        *last += spare;
    }
    let mut y = body.y;
    let mut sections: Vec<Option<Section>> = sections.into_iter().map(Some).collect();
    for (k, r) in visible.iter().enumerate() {
        let height = h[k].min(body.y + body.height - y);
        let w = body.width / r.len() as u16;
        for (c, idx) in r.iter().enumerate() {
            let rect = Rect { x: body.x + c as u16 * w, y, width: if c == r.len() - 1 { body.width - w * c as u16 } else { w }, height };
            if let Some(sec) = sections[*idx].take() {
                draw_section(f, sec, rect, k == 0 && c == 0, app.chart_lines);
            }
        }
        y += height;
    }
}

fn draw_section(f: &mut Frame, sec: Section, area: Rect, focused: bool, chart_lines: bool) {
    if area.height < 3 || area.width < 8 {
        return render_lines(f, Rect { height: 1.min(area.height), ..area }, vec![fit_line(vec![Span::styled(format!(" {} ", sec.title), bold().fg(sec.colour))], area.width as usize)]);
    }
    let inner = draw_box(f, area, &sec.title, sec.detail, sec.colour, focused);
    match sec.body {
        Body::Chart { series, opts } => draw_chart(f, inner, &series, &opts, chart_lines),
        Body::Dist(doc) => draw_dist(f, inner, &doc, sec.colour),
        Body::Lines(mut lines) => {
            // a list that does not fit says so on its last row: scrolling brings it to the top,
            // where it gets the rows it needs
            let room = inner.height as usize;
            if lines.len() > room && room >= 2 {
                let hidden = lines.len() - (room - 1);
                lines.truncate(room - 1);
                lines.push(Line::styled(fit(&format!("  ... {hidden} more line{}: scroll down", if hidden == 1 { "" } else { "s" }), inner.width as usize), dim()));
            }
            render_lines(f, inner, lines);
        }
    }
}

fn message(title: &str, colour: Color, text: String, problem: bool, width: u16) -> Section {
    let lines: Vec<Line> = super::widgets::wrap(&text, (width as usize).saturating_sub(2)).into_iter().map(|l| Line::styled(l, if problem { red() } else { dim() })).collect();
    let h = lines.len() as u16 + 2;
    Section { title: title.to_string(), detail: vec![], colour, half: false, min_h: h, want_h: h, body: Body::Lines(lines) }
}

fn build(page: PageId, app: &App, s: &Status, data: Option<&PageData>, now: i64, body: Rect) -> Vec<Section> {
    let colour = page_colour(page);
    let Some(d) = data else {
        return vec![message(page.title(), colour, format!("loading the last {} from {} ...", range_label(app.range_idx), app.url), false, body.width)];
    };
    let mut out = Vec::new();
    if let Some(e) = &d.error {
        out.push(message("COLLECTOR", Color::Red, e.clone(), true, body.width));
    }
    match page {
        PageId::Latency => latency(&mut out, d, s, app, body),
        PageId::Load => load(&mut out, d, s, app, body),
        PageId::Gpus => gpus(&mut out, d, s, app, body, now),
        PageId::Gateway => gateway(&mut out, d, s, app, body),
        PageId::Alerts => alerts(&mut out, d, s, body, now),
        PageId::Incidents => incidents(&mut out, d, body, now),
        PageId::Users => users(&mut out, d, s, app, body, now),
        PageId::Tokens => {
            // card #176 item 2, positioned by measurement rather than by preference. Appended
            // LAST it rendered below the fold at ordinary heights - present in the section list,
            // invisible on the screen, which is card #124's failure exactly. Pushed FIRST it
            // displaced the page's own subject: at 63x24 only one box fits, so TOKENS SERVED
            // filled the pane, TOKENS PER HOUR fell off the bottom, and a chart's empty
            // plot rows broke this page's no-blank-rows-at-63x24 rule. Third is the position
            // that costs neither: the page's two headline sections keep the first screen at
            // every size, and the chart is the first thing a scroll reaches.
            // Its real home is the money page, not the token page - card #179 step 4 lands this
            // card on the redesigned page 1, and that is where a reader looking for spending
            // will go. This position is honest in the meantime, not final.
            tokens(&mut out, d, s, body, now);
            let mut chart_out = Vec::new();
            spending_to_date(&mut chart_out, s, body.width);
            spending_chart(&mut chart_out, s, app, f_num);
            let at = out.len().min(2);
            for (i, sec) in chart_out.into_iter().enumerate() {
                out.insert(at + i, sec);
            }
        }
        PageId::Model => model(&mut out, d, app, body, now),
        PageId::Advice => advice(&mut out, d, body),
    }
    if out.is_empty() {
        out.push(message(page.title(), colour, "the collector sent nothing for this page".into(), false, body.width));
    }
    split_tall(out, body.height)
}

// ---------------------------------------------------------------- formatting

/// Milliseconds, compact: `0`, `4.7ms`, `186ms`, `2.8s`, `51s`.
fn f_ms(v: f64) -> String {
    if v.abs() < 0.05 {
        "0".into()
    } else if v < 10.0 {
        format!("{v:.1}ms")
    } else if v < 1000.0 {
        format!("{v:.0}ms")
    } else if v < 10_000.0 {
        format!("{:.1}s", v / 1000.0)
    } else {
        format!("{:.0}s", v / 1000.0)
    }
}
fn f_num(v: f64) -> String {
    if v.abs() >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if v.abs() >= 100_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if v.abs() >= 10_000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if v.abs() >= 100.0 || v.fract().abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}
fn f_pct01(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}
fn f_pct(v: f64) -> String {
    format!("{v:.0}%")
}
fn f_c(v: f64) -> String {
    lss_core::units::temp_compact(v)
}
fn f_w(v: f64) -> String {
    format!("{v:.0}W")
}
fn f_mhz(v: f64) -> String {
    format!("{v:.0}")
}
fn f_gib(v: f64) -> String {
    format!("{:.1}G", v / 1024.0)
}
fn f_x(v: f64) -> String {
    format!("{v:.2}")
}
fn count(n: u64) -> String {
    f_num(n as f64)
}

fn opts(app: &App, fmt: fn(f64) -> String, y_min: Option<f64>, y_max: Option<f64>) -> ChartOpts {
    ChartOpts { fmt, y_min, y_max, x_left: format!("-{}", range_label(app.range_idx)), markers: vec![], peak: false }
}

/// For levels where the spike is the point (queues, temperatures, waiters).
fn peak(mut o: ChartOpts) -> ChartOpts {
    o.peak = true;
    o
}

fn chart(title: &str, detail: Vec<Span<'static>>, colour: Color, series: Vec<Series>, opts: ChartOpts) -> Section {
    Section { title: title.to_string(), detail, colour, half: true, min_h: 7, want_h: 11, body: Body::Chart { series, opts } }
}

/// #115: whether a HALF-width column (`full_width / 2`, minus this app's own box-border and
/// y-axis-gutter overhead) would still fit `n` panels at the renderer's own readable minimum
/// (`MIN_PANEL_W`) - either the single wide row, or the 2-row grid `draw_chart_lines` already
/// falls back to. Mirrors that function's own width arithmetic; kept in sync deliberately rather
/// than exported, since this is a LAYOUT decision (does pairing still leave room) made before a
/// Section exists to render, while `draw_chart_lines` makes the same call at real render time
/// with the real, possibly-narrower area it actually got.
fn multi_panel_fits_halved(n: usize, full_width: u16) -> bool {
    const GUTTER: usize = 7; // a generous, real y-axis label width (e.g. "100.0G ")
    let half_w = (full_width / 2).saturating_sub(2) as usize; // this box's own border
    let side_w = (half_w.saturating_sub(GUTTER + n.saturating_sub(1))) / n.max(1);
    if side_w >= MIN_PANEL_W {
        return true;
    }
    let cols = n.div_ceil(2);
    let grid_w = (half_w.saturating_sub(GUTTER + cols.saturating_sub(1))) / cols.max(1);
    grid_w >= MIN_PANEL_W
}

/// Like `chart()`, for a chart whose series split into per-series small-multiple panels in line
/// mode (GPUS's per-GPU metrics, USERS' per-user/per-lane charts): a half-width box (~62 cols)
/// gave each of 4 GPUs ~14 columns and 2-3 plot rows, so every line collapsed into a square wave
/// of flat runs and vertical jumps instead of a curve. That was a real bug in the LAYOUT, not
/// the line-drawing code, caught by the reviewer on the first ship-gate captures (2026-09-21).
///
/// In line mode, a chart with more than one series that would NOT stay readable at half width
/// gets the row to itself (`half: false`) and a taller box: `min_h` guarantees the renderer's
/// own `MIN_PANEL_H` floor (never a 2-row panel) even under height pressure, and `want_h` gives
/// it the seven-plus plot rows a line needs to have somewhere to go when there is room, with
/// genuine spare height for #115's opportunistic per-panel title.
///
/// #115: at generous width (140 cols: the ship gate's own reproducer, verifier 12:19) forcing
/// EVERY multi-series line chart full-width cost half the page - 4 of 8 GPU boxes, throttle
/// reasons and health falling below the fold with no scroll hint some readers would even notice.
/// `multi_panel_fits_halved` asks the same "would every panel still clear the readable minimum"
/// question `draw_chart_lines` asks at render time, but at half the page width - if yes, this
/// chart stays `half: true` and the page's own two-column pairing (`layout`, `TWO_COLUMNS`)
/// buys the room back exactly the way #101 already does for every other chart on a wide pane.
///
/// Braille mode (one overlaid plot, no panels to split) is untouched: same half-width box as
/// every other chart on the page. `lines` is `app.chart_lines` - a runtime choice now, not a
/// compile-time one (see `draw_chart`). `width` is the PAGE's own body width, not this section's
/// eventual box width - chart_wide is called before layout decides who shares a row with whom.
fn chart_wide(title: &str, detail: Vec<Span<'static>>, colour: Color, series: Vec<Series>, opts: ChartOpts, lines: bool, width: u16) -> Section {
    let mut sec = chart(title, detail, colour, series, opts);
    let n = match &sec.body {
        Body::Chart { series, .. } => series.len(),
        _ => 0,
    };
    if lines && n > 1 {
        let half = multi_panel_fits_halved(n, width);
        sec.half = half;
        if half {
            // #115: a HALVED width is exactly what pushes `draw_chart_lines` from its single
            // wide row into its 2-row grid fallback (grid_w clears MIN_PANEL_W well before
            // side_w does, at half the page width) - and the grid's own bare floor is TWO
            // MIN_PANEL_H rows plus the blank row between them, not one. min_h=9/want_h=12 (the
            // single-row budget) starved the grid down to a 3-row-tall panel and it could never
            // clear its own MIN_PANEL_H(5) floor, so every paired chart silently fell all the
            // way to the "one worst panel" fallback instead - caught by
            // `line_chart_uses_a_distinct_colour_per_series_and_the_legend_matches_the_line` at
            // 126x41 failing exactly this way. floor: border(2) + axis(1) + legend(1) +
            // grid(2*MIN_PANEL_H(5) + 1 blank separator = 11) = 15. target: one MORE row per
            // grid half so #115's opportunistic title has genuine room there too.
            sec.min_h = 15;
            sec.want_h = 17;
        } else {
            sec.min_h = 9; // floor: plot_h >= MIN_PANEL_H (5) + 4 rows of border/axis/legend overhead
            sec.want_h = 12; // target: 7-row panels with genuine spare room for #115's opportunistic title
        }
    }
    sec
}

fn plain(text: String) -> Vec<Span<'static>> {
    vec![Span::raw(text)]
}

fn last_of(doc: &SeriesDoc, token: &str, fmt: fn(f64) -> String) -> String {
    doc.last(token).map_or_else(|| "-".to_string(), fmt)
}

/// Where a moment sits on the chart's time axis, 0..1.
fn at(doc: &SeriesDoc, ts: i64) -> Option<f64> {
    let span = (doc.points as i64 * doc.step_s).max(1);
    let x = (ts - doc.start_ts) as f64 / span as f64;
    (0.0..=1.0).contains(&x).then_some(x)
}

// ---------------------------------------------------------------- 1 LATENCY

const LATENCY_TITLES: [&str; 4] = ["TTFT", "E2E LATENCY", "INTER-TOKEN LATENCY", "QUEUE TIME"];

fn latency(out: &mut Vec<Section>, d: &PageData, s: &Status, app: &App, body: Rect) {
    let colour = page_colour(PageId::Latency);
    for (i, (short, _)) in LATENCY_METRICS.iter().enumerate() {
        // #23, 2026-09-21: an engine that reports none of these (Ollama) used to draw an empty
        // chart and a "no requests" bucket section - indistinguishable from a real engine that
        // is genuinely idle. Say plainly that the number does not exist here, once, instead.
        if s.serve.is_na(short) {
            out.push(message(LATENCY_TITLES[i], colour, s.serve.na(short).unwrap_or_default(), false, body.width));
            continue;
        }
        let hist = d.hists.iter().find(|h| h.metric == *short);
        if let Some(doc) = &d.series {
            let tok = |q: &str| format!("{short}_{q}_ms");
            let series = vec![Series::new("p50", Color::Green, doc.get(&tok("p50"))), Series::new("p90", Color::Cyan, doc.get(&tok("p90"))), Series::new("p99", Color::Magenta, doc.get(&tok("p99")))];
            // "now" = the newest window with requests; "avg" = over the whole range, from the buckets
            let avg = hist.and_then(|h| h.summary).map_or_else(|| last_of(doc, &tok("avg"), f_ms), |s| f_ms(s.avg_ms));
            let detail = plain(format!("avg {avg} · p50 {} · p99 {}", last_of(doc, &tok("p50"), f_ms), last_of(doc, &tok("p99"), f_ms)));
            out.push(chart(LATENCY_TITLES[i], detail, colour, series, peak(opts(app, f_ms, Some(0.0), None))));
        }
        if let Some(h) = hist {
            let n = h.summary.map_or(0.0, |s| s.count);
            let detail = match h.summary {
                // the inter-token histogram counts token gaps, the others count requests
                Some(s) => plain(format!("{} {} · p90 {}", f_num(n), if *short == "itl" { "tokens" } else { "requests" }, f_ms(s.p90_ms))),
                None => plain("no requests".into()),
            };
            out.push(Section { title: format!("{} BUCKETS", LATENCY_TITLES[i]), detail, colour, half: true, min_h: 7, want_h: 11, body: Body::Dist(h.clone()) });
        }
    }
}

/// `<=186ms` for a latency histogram, `<=4.1k` for one that counts tokens.
fn bound_label(doc: &HistDoc, i: usize) -> String {
    let show = |b: f64| if doc.unit == "tokens" { crate::report::big(b) } else { f_ms(b * 1000.0) };
    match doc.le.get(i) {
        Some(b) => format!("<={}", show(*b)),
        None => format!(">{}", doc.le.last().map_or_else(|| "?".into(), |b| show(*b))),
    }
}

/// The histogram buckets of the range as horizontal bars and, when there is room beside them,
/// a heat strip over time (bucket rows, time columns).
fn draw_dist(f: &mut Frame, inner: Rect, doc: &HistDoc, colour: Color) {
    let filled: Vec<usize> = doc.total.iter().enumerate().filter(|(_, c)| **c > 0.0).map(|(i, _)| i).collect();
    let (Some(&lo), Some(&hi)) = (filled.first(), filled.last()) else {
        return render_lines(f, inner, vec![Line::styled("no requests in this range", dim())]);
    };
    // merge neighbouring buckets until they fit the rows; a merged row is labelled by its upper bound
    let rows_max = (inner.height as usize).max(1);
    let span = hi - lo + 1;
    let per = span.div_ceil(rows_max).max(1);
    let groups: Vec<(usize, usize)> = (lo..=hi).step_by(per).map(|a| (a, (a + per - 1).min(hi))).collect();
    let labels: Vec<String> = groups.iter().map(|(_, b)| bound_label(doc, *b)).collect();
    let bars: Vec<(String, f64)> = groups.iter().zip(&labels).map(|((a, b), l)| (l.clone(), doc.total[*a..=*b].iter().sum())).collect();
    let side_by_side = inner.width >= 56 && !doc.columns.is_empty();
    let bars_area = if side_by_side { Rect { width: inner.width / 2, ..inner } } else { inner };
    draw_hbars(f, bars_area, &bars, colour);
    if side_by_side && inner.height >= 3 {
        let heat_area = Rect { x: inner.x + inner.width / 2 + 1, width: inner.width - inner.width / 2 - 1, ..inner };
        let heat_rows = groups.len().min(heat_area.height as usize - 1);
        // heat rows run fastest (bottom) to slowest (top); when there are more groups than rows keep the slowest
        let keep = &groups[groups.len() - heat_rows..];
        let grid: Vec<Vec<f64>> = keep.iter().map(|(a, b)| doc.columns.iter().map(|col| col.get(*a..=(*b).min(col.len().saturating_sub(1))).map_or(0.0, |s| s.iter().sum())).collect()).collect();
        let heat_labels: Vec<String> = labels[labels.len() - heat_rows..].to_vec();
        draw_heat(f, heat_area, &heat_labels, &grid, colour, &format!("-{}", fmt_duration(doc.range_s).trim_end_matches("00m").trim_end_matches("00s")));
    }
}

// ---------------------------------------------------------------- 2 LOAD

fn load(out: &mut Vec<Section>, d: &PageData, s: &Status, app: &App, body: Rect) {
    let Some(doc) = &d.series else { return };
    let colour = page_colour(PageId::Load);
    // #23, 2026-09-21: a chart of a number the engine never reports (Ollama: decode_tok_s,
    // running, queued, kv_usage, spec) used to draw as an empty/flat-zero line - the same
    // picture a genuinely idle engine that DOES report it would draw. Say so plainly instead.
    let na = |key: &str, title: &'static str| s.serve.is_na(key).then(|| message(title, colour, s.serve.na(key).unwrap_or_default(), false, body.width));
    // a benchmark is load WE made: its start sits on the time axis as a `B`, so nobody reads
    // it as real traffic. A maintenance window (#22, 2026-09-21) sits on the same charts as an
    // `M`, in a calm style (not `warn`/`red`) - it is what EXPLAINS a restart's dip, not a
    // problem of its own.
    let bench_marks = |mut o: ChartOpts| {
        o.markers.extend(s.incidents.iter().filter(|i| i.kind == lss_core::incidents::KIND_BENCH).filter_map(|i| at(doc, i.start)).map(|x| Marker { at: x, glyph: 'B', style: warn() }));
        o.markers.extend(s.incidents.iter().filter(|i| i.kind == lss_core::incidents::KIND_MAINTENANCE).filter_map(|i| at(doc, i.start)).map(|x| Marker { at: x, glyph: 'M', style: good() }));
        o
    };
    if let Some(m) = na("decode_tok_s", "THROUGHPUT") {
        out.push(m);
    } else {
        let mut prompt = Series::new("prompt tok/s", Color::Blue, doc.get("prompt_tok_s"));
        prompt.own_scale = true;
        out.push(chart("THROUGHPUT", plain(format!("decode {} tok/s aggregate (all requests together) · prompt {} tok/s", last_of(doc, "decode_tok_s", f_num), last_of(doc, "prompt_tok_s", f_num))), colour, vec![Series::new("decode tok/s (aggregate)", Color::Cyan, doc.get("decode_tok_s")), prompt], bench_marks(opts(app, f_num, Some(0.0), None))));
    }
    // READING (prefill): the other speed of a model. A prompt is read before the first word of
    // the answer is written, so this is what a long prompt waits for.
    let sv = &s.serve;
    let reading_now = match (sv.prefill_tok_s, sv.prefill_tok_s_typical) {
        (Some(v), _) => format!("{} tok/s now", f_num(v)),
        (None, Some(t)) => format!("idle · typical {} tok/s", f_num(t)),
        (None, None) => sv.no_reading_reason().to_string(),
    };
    out.push(chart("READING SPEED (prefill)", plain(format!("{reading_now} · {:.0} being read", sv.prefill_inflight)), colour, vec![Series::new("prompt tok/s read (cache hits not counted)", Color::Blue, doc.get("prefill_tok_s"))], bench_marks(peak(opts(app, f_num, Some(0.0), None)))));
    let total = |token: &str| doc.get(token).iter().flatten().sum::<f64>();
    let (read, cached) = (total("tok_prefill"), total("tok_cached"));
    let pending = sv_pending(s);
    let share = if read + cached > 0.0 { format!("{} from cache · {} read", f_pct01(cached / (read + cached)), f_pct01(read / (read + cached))) } else { "no prompts in this range".to_string() };
    let mut cached_series = Series::new("from cache", Color::Green, doc.get("tok_cached"));
    cached_series.own_scale = true;
    out.push(chart("PROMPT TOKENS", plain(format!("{share} · {} waiting", f_num(pending))), colour, vec![Series::new("read (computed)", Color::Blue, doc.get("tok_prefill")), cached_series], opts(app, f_num, Some(0.0), None)));
    // where the time of a request goes: the share spent reading the prompt, per bucket
    let (pre, e2e) = (doc.get("sum_prefill_s"), doc.get("sum_e2e_s"));
    let split: Vec<Option<f64>> = pre.iter().zip(e2e.iter()).map(|(p, e)| match (p, e) {
        (Some(p), Some(e)) if *e > 0.0 => Some((p / e).clamp(0.0, 1.0)),
        _ => None,
    }).collect();
    let (pre_sum, e2e_sum) = (pre.iter().flatten().sum::<f64>(), e2e.iter().flatten().sum::<f64>());
    let split_detail = if e2e_sum > 0.0 { format!("reading {} · writing + waiting {}", f_pct01((pre_sum / e2e_sum).clamp(0.0, 1.0)), f_pct01((1.0 - pre_sum / e2e_sum).clamp(0.0, 1.0))) } else { "no finished requests in this range".to_string() };
    out.push(chart("WHERE THE TIME GOES", plain(split_detail), colour, vec![Series::new("share of request time spent reading the prompt", Color::Magenta, &split)], opts(app, f_pct01, Some(0.0), Some(1.0))));
    let lanes = |what: &str| vec![Series::new("public", Color::Cyan, doc.get(&format!("{what}_public"))), Series::new("trusted", Color::Green, doc.get(&format!("{what}_trusted")))];
    let slots = f64::from(s.serve.slots.max(1));
    if let Some(m) = na("running", "RUNNING REQUESTS") {
        out.push(m);
    } else {
        out.push(chart("RUNNING REQUESTS", plain(format!("{} {:.0}/{} slots", gauge(s.serve.running / slots, 8), s.serve.running, s.serve.slots)), colour, lanes("running"), bench_marks(peak(opts(app, f_num, Some(0.0), Some(slots))))));
    }
    if let Some(m) = na("queued", "QUEUED REQUESTS") {
        out.push(m);
    } else {
        let queue_detail = if s.serve.queue >= s.thresholds.queue_reqs { vec![Span::styled(format!("{:.0} queued, alert line {:.0}", s.serve.queue, s.thresholds.queue_reqs), red())] } else { plain(format!("{:.0} queued · alert at {:.0}", s.serve.queue, s.thresholds.queue_reqs)) };
        out.push(chart("QUEUED REQUESTS", queue_detail, colour, lanes("queue"), peak(opts(app, f_num, Some(0.0), None))));
    }
    if let Some(m) = na("kv_usage", "KV CACHE") {
        out.push(m);
    } else {
        out.push(chart(
            "KV CACHE",
            plain(format!("usage {} {} · cache hit {}", gauge(s.serve.kv_usage, 8), f_pct01(s.serve.kv_usage), f_pct01(s.serve.cache_hit_rate))),
            colour,
            vec![Series::new("KV usage", Color::Blue, doc.get("kv_usage")), Series::new("cache hit rate", Color::Green, doc.get("cache_hit_rate"))],
            opts(app, f_pct01, Some(0.0), Some(1.0)),
        ));
    }
    if let Some(m) = na("spec", "SPECULATIVE DECODING") {
        out.push(m);
    } else {
        let mut rate = Series::new("accept rate", Color::Cyan, doc.get("spec_accept_rate"));
        rate.own_scale = true;
        out.push(chart("SPECULATIVE DECODING", plain(format!("accept length {:.2} · rate {}", s.serve.spec_accept_length, f_pct01(s.serve.spec_accept_rate))), colour, vec![Series::new("accept length", Color::Magenta, doc.get("spec_accept_length")), rate], opts(app, f_x, None, None)));
    }
    out.push(chart("REQUESTS / MIN", plain(last_of(doc, "req_per_min", f_num)), colour, vec![Series::new("requests/min", Color::Cyan, doc.get("req_per_min"))], opts(app, f_num, Some(0.0), None)));
    // C1: valid probes are the line, invalid ones are marked on the time axis (and plotted apart)
    let mut o = opts(app, f_num, None, None);
    let invalid = doc.get("c1_invalid");
    for (i, v) in invalid.iter().enumerate() {
        if v.is_some() {
            o.markers.push(Marker { at: (i as f64 + 0.5) / invalid.len().max(1) as f64, glyph: 'x', style: warn() });
        }
    }
    let n_valid = doc.get("c1_tok_s").iter().flatten().count();
    let n_invalid = invalid.iter().flatten().count();
    let base = s.c1_baseline().map_or_else(|| "learning".to_string(), |b| format!("{b:.1}"));
    let floor = s.c1_floor().map_or_else(|| "-".to_string(), |b| format!("{b:.1}"));
    out.push(chart(
        "C1 PROBE",
        plain(format!("{n_valid} valid · {n_invalid} invalid x · alert < {floor} of {base}")),
        colour,
        {
            let mut valid = Series::new("valid tok/s", Color::Green, doc.get("c1_tok_s"));
            valid.sparse = true;
            let mut invalid = Series::new("invalid tok/s", Color::Magenta, doc.get("c1_invalid_tok_s"));
            invalid.sparse = true;
            vec![valid, invalid]
        },
        peak(o),
    ));
}

/// Prompt tokens admitted by the gateway whose requests have not finished: what is waiting to
/// be read or being read right now (the gateway's estimate; 0 without a gateway).
fn sv_pending(s: &Status) -> f64 {
    (s.lanes.public.inflight_tokens + s.lanes.trusted.inflight_tokens) as f64
}

// ---------------------------------------------------------------- 3 GPUS

fn gpus(out: &mut Vec<Section>, d: &PageData, s: &Status, app: &App, body: Rect, now: i64) {
    let width = body.width;
    let colour = page_colour(PageId::Gpus);
    // card #180 gate 3, a clean install against a CPU-only Ollama: with no GPU tool at all this
    // page still drew its charts, and their titles folded an EMPTY list into numbers - "max
    // 0C/32F", "total -0 W of -0 W cap". #182's tab strip made the page one keypress away on
    // exactly the machine where it has nothing to say. It says that instead.
    if s.gpus.is_empty() {
        let why = if s.gpu_source == "none" {
            "no GPU tool on this machine: lss reads NVIDIA through NVML or nvidia-smi, AMD through amd-smi or rocm-smi, Apple silicon through ioreg - none of them answered, so there is nothing to chart here"
        } else {
            "no GPU readings yet from this collector"
        };
        out.push(lines_section("GPUS", vec![], colour, vec![Line::styled(why, dim())], false));
        return;
    }
    let idx: Vec<u32> = s.gpus.iter().map(|g| g.sample.index).collect();
    if let Some(doc) = &d.series {
        let per_gpu = |field: &str| -> Vec<Series> { idx.iter().enumerate().map(|(k, i)| Series::new(&format!("GPU{i}"), GPU_COLOURS[k % GPU_COLOURS.len()], doc.get(&format!("gpu{i}_{field}")))).collect() };
        let now_vals = |pick: fn(&lss_core::gpu::GpuSample) -> Option<f64>| -> Vec<f64> { s.gpus.iter().filter_map(|g| pick(&g.sample)).collect() };
        let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
        // Xid incidents sit on every time axis of this page
        let xids: Vec<Marker> = s.incidents.iter().filter(|i| i.kind == "xid").filter_map(|i| at(doc, i.start)).map(|x| Marker { at: x, glyph: 'X', style: red().add_modifier(Modifier::BOLD) }).collect();
        let with_xids = |mut o: ChartOpts| {
            o.markers = xids.iter().map(|m| Marker { at: m.at, glyph: m.glyph, style: m.style }).collect();
            o
        };
        let hot = s.thresholds.thermal_temp_c;
        let temps = now_vals(|g| g.temp_c);
        let t_max = temps.iter().copied().fold(0.0_f64, f64::max);
        let tc = lss_core::units::temp_compact;
        // card #223: the max is coloured by the HOTTEST card's own temp reading band - so a card
        // with thermal alerts off (informational) never paints the title, and amber/red follow
        // the one scale page 1 uses, not a hand-written `t_max >= alert` line
        let hottest = s.gpus.iter().filter(|g| g.sample.temp_c.is_some()).max_by(|a, b| a.sample.temp_c.partial_cmp(&b.sample.temp_c).unwrap_or(std::cmp::Ordering::Equal));
        let t_style = hottest
            .and_then(|g| lss_core::readings::gpu_card(s, g).into_iter().find(|r| r.key == format!("gpu.{}.temp", g.sample.index)))
            .map_or_else(Style::default, |r| band_style(&r));
        // both units make this title long: the hottest GPU and the alert line are what matter
        let mut t_detail = plain(format!("{} max ", gauge(t_max / hot, 8)));
        t_detail.push(Span::styled(tc(t_max), t_style));
        t_detail.push(Span::raw(format!(" · alert at {}", tc(hot))));
        out.push(chart_wide("TEMPERATURE", t_detail, colour, per_gpu("temp_c"), with_xids(peak(opts(app, f_c, None, None))), app.chart_lines, width));
        let power: f64 = now_vals(|g| g.power_w).iter().sum();
        let cap: f64 = now_vals(|g| g.power_limit_w).iter().sum();
        out.push(chart_wide("POWER", plain(format!("total {power:.0} W {} of {cap:.0} W cap", gauge(power / cap.max(1.0), 8))), colour, per_gpu("power_w"), with_xids(opts(app, f_w, Some(0.0), None)), app.chart_lines, width));
        let clk_max = s.gpus.iter().filter_map(|g| g.health.as_ref().and_then(|h| h.clock_max_mhz)).fold(0.0_f64, f64::max);
        let lowest = idx.iter().filter_map(|i| doc.get(&format!("gpu{i}_clock_mhz:min")).iter().flatten().copied().reduce(f64::min)).reduce(f64::min);
        out.push(chart_wide("SM CLOCK MHz", plain(format!("max {} · lowest in range {}", if clk_max > 0.0 { f_mhz(clk_max) } else { "-".into() }, lowest.map_or_else(|| "-".into(), f_mhz))), colour, per_gpu("clock_mhz"), with_xids(opts(app, f_mhz, None, (clk_max > 0.0).then_some(clk_max))), app.chart_lines, width));
        out.push(chart_wide("GPU UTILISATION", plain(format!("avg {:.0}% {}", mean(&now_vals(|g| g.util_pct)), gauge(mean(&now_vals(|g| g.util_pct)) / 100.0, 8))), colour, per_gpu("util_pct"), with_xids(opts(app, f_pct, Some(0.0), Some(100.0))), app.chart_lines, width));
        let total_mem = now_vals(|g| g.mem_total_mib).into_iter().fold(0.0_f64, f64::max);
        out.push(chart_wide("MEMORY USED", plain(format!("of {} per GPU", f_gib(total_mem))), colour, per_gpu("mem_used_mib"), with_xids(opts(app, f_gib, Some(0.0), (total_mem > 0.0).then_some(total_mem))), app.chart_lines, width));
        out.push(chart_wide("MEMORY CONTROLLER UTIL", plain("stands in for DCGM tensor-core util".into()), colour, per_gpu("mem_util_pct"), with_xids(opts(app, f_pct, Some(0.0), Some(100.0))), app.chart_lines, width));
        out.push(throttle_timeline(doc, s, &idx, colour, width, app));
    }
    out.extend(health_table(s, colour, body, now));
}

/// One row per GPU over the range: `·` clean, `p` software power cap (normal under a power
/// limit), `T` thermal slowdown, `H` hardware slowdown / power brake. Xids on the axis.
fn throttle_timeline(doc: &SeriesDoc, s: &Status, idx: &[u32], colour: Color, width: u16, app: &App) -> Section {
    // card #223 (the owner: "the throttle reasoning it all jumbled P not sure what that is"): a
    // power cap is NORMAL under a power limit, and drawing it as a 'p' per time cell turned a card
    // at its cap all day into a solid row of letters. Now: the timeline draws ONLY problems (T
    // thermal, H hw slowdown / power brake) in red, and each card gets a plain-words line saying
    // whether anything is wrong and how much of the range it spent at its power cap.
    let pct = |on: usize, known: usize| (on as f64 * 100.0 / known.max(1) as f64).round() as u32;
    let caption = |i: u32| -> Vec<Span<'static>> {
        let full = |name: &str| doc.get(&format!("gpu{i}_{name}"));
        let (thermal, hw, power) = (full("thr_thermal"), full("thr_hw"), full("thr_power"));
        let n = thermal.len().max(hw.len()).max(power.len());
        let at = |v: &[Option<f64>], c: usize| v.get(c).copied().flatten();
        let known = (0..n).filter(|&c| at(thermal, c).is_some() || at(hw, c).is_some() || at(power, c).is_some()).count();
        let on = |v: &[Option<f64>]| (0..n).filter(|&c| at(v, c).is_some_and(|x| x > 0.0)).count();
        if known == 0 {
            return vec![Span::styled("no throttle data in range", dim())];
        }
        let mut out = Vec::new();
        let problems: Vec<String> = [("thermal slowdown", on(thermal)), ("hw slowdown", on(hw))]
            .into_iter()
            .filter(|(_, k)| *k > 0)
            .map(|(what, k)| format!("{what} {}% of range", pct(k, known)))
            .collect();
        if problems.is_empty() {
            out.push(Span::raw("clean"));
        } else {
            out.push(Span::styled(problems.join(", "), red().add_modifier(Modifier::BOLD)));
        }
        let capped = on(power);
        if capped > 0 {
            out.push(Span::styled(format!(" · power cap {}% of range (normal)", pct(capped, known)), dim()));
        }
        out
    };
    let captions: Vec<(u32, Vec<Span<'static>>)> = idx.iter().map(|&i| (i, caption(i))).collect();
    let cap_w = captions.iter().map(|(_, c)| c.iter().map(|sp| sp.content.chars().count()).sum::<usize>()).max().unwrap_or(0);
    // the caption rides the same row when there is room for a real timeline beside it, else its own
    let inner = (width as usize).saturating_sub(2 + 6);
    let beside = inner >= cap_w + 2 + 24;
    let plot_w = if beside { inner - cap_w - 2 } else { inner }.max(8);
    let mut lines = Vec::new();
    let mut any_bad = false;
    for (i, cap) in captions {
        let col = |name: &str| super::widgets::resample_max(doc.get(&format!("gpu{i}_{name}")), plot_w);
        let (thermal, hw, power) = (col("thr_thermal"), col("thr_hw"), col("thr_power"));
        let mut spans = vec![Span::styled(format!("GPU{i:<2} "), bold())];
        for c in 0..thermal.len().max(power.len()).max(hw.len()) {
            let on = |v: &Vec<Option<f64>>| v.get(c).copied().flatten().is_some_and(|x| x > 0.0);
            let known = [&thermal, &hw, &power].iter().any(|v| v.get(c).copied().flatten().is_some());
            let (ch, st) = if on(&thermal) {
                ('T', red().add_modifier(Modifier::BOLD))
            } else if on(&hw) {
                ('H', red().add_modifier(Modifier::BOLD))
            } else if known {
                ('·', dim()) // clean, or at the power cap: both are fine, neither is drawn
            } else {
                (' ', dim())
            };
            any_bad |= ch == 'T' || ch == 'H';
            match spans.last_mut() {
                Some(last) if last.style == st => last.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), st)),
            }
        }
        if beside {
            let drawn: usize = spans.iter().map(|sp| sp.content.chars().count()).sum();
            spans.push(Span::raw(" ".repeat((6 + plot_w + 2).saturating_sub(drawn))));
            spans.extend(cap);
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(spans));
            let mut own = vec![Span::raw("      ")];
            own.extend(cap);
            lines.push(Line::from(own));
        }
    }
    let mut axis: Vec<char> = "─".repeat(plot_w).chars().collect();
    let left = format!("-{} ", range_label(app.range_idx));
    for (k, ch) in left.chars().enumerate() {
        if let Some(c) = axis.get_mut(k) {
            *c = ch;
        }
    }
    for (k, ch) in " now".chars().enumerate() {
        if let Some(c) = axis.get_mut(plot_w.saturating_sub(4) + k) {
            *c = ch;
        }
    }
    let mut axis_spans = vec![Span::raw("      "), Span::styled(axis.iter().collect::<String>(), dim())];
    let xids: Vec<usize> = s.incidents.iter().filter(|i| i.kind == "xid").filter_map(|i| at(doc, i.start)).map(|x| (x * (plot_w - 1) as f64).round() as usize).collect();
    if !xids.is_empty() {
        let mut cells: Vec<Span> = vec![Span::raw("      ")];
        for (k, ch) in axis.iter().enumerate() {
            cells.push(if xids.contains(&k) { Span::styled("X", red().add_modifier(Modifier::BOLD)) } else { Span::styled(ch.to_string(), dim()) });
        }
        axis_spans = cells;
    }
    lines.push(Line::from(axis_spans));
    // 77 characters: fits the narrowest box this page draws (80 columns, 78 inside the border)
    lines.push(Line::styled("· fine   T thermal   H hw slowdown   X Xid   a power cap is normal, not drawn", dim()));
    // card #223: an Xid in range IS a problem - it used to print uncoloured
    let detail = if any_bad {
        vec![Span::styled("thermal / hw slowdown in range", red())]
    } else if !xids.is_empty() {
        vec![Span::styled(format!("{} Xid in range", xids.len()), red())]
    } else {
        plain("no problem in range".into())
    };
    let h = lines.len() as u16 + 2;
    Section { title: "THROTTLE REASONS".into(), detail, colour, half: false, min_h: h, want_h: h, body: Body::Lines(lines) }
}

/// Cells flowed into lines no wider than `width`, continuation lines indented.
fn flow(first: Span<'static>, cells: Vec<Vec<Span<'static>>>, width: usize) -> Vec<Line<'static>> {
    let indent = first.content.chars().count();
    let mut lines: Vec<Vec<Span<'static>>> = vec![vec![first]];
    let mut used = indent;
    for cell in cells {
        let n: usize = cell.iter().map(|s| s.content.chars().count()).sum();
        let sep = if used > indent { 3 } else { 0 };
        if used + sep + n > width && used > indent {
            lines.push(vec![Span::raw(" ".repeat(indent))]);
            used = indent;
        }
        let line = lines.last_mut().expect("never empty");
        if used > indent {
            line.push(Span::styled(" · ", dim()));
            used += 3;
        }
        line.extend(cell);
        used += n;
    }
    lines.into_iter().map(|l| fit_line(l, width)).collect()
}

fn health_table(s: &Status, colour: Color, body: Rect, now: i64) -> Vec<Section> {
    let w = (body.width as usize).saturating_sub(2);
    let mut groups: Vec<Vec<Line<'static>>> = Vec::new();
    let mut problems = 0;
    let mut read_at = None;
    for g in &s.gpus {
        let Some(h) = &g.health else {
            groups.push(vec![Line::from(vec![Span::styled(format!("GPU{:<2} ", g.sample.index), bold()), Span::styled("no health read yet (the collector reads it once a minute)", dim())])]);
            continue;
        };
        read_at = Some(h.ts);
        let n = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{x:.0}"));
        let yn = |v: Option<bool>| v.map_or("-", |b| if b { "YES" } else { "no" }).to_string();
        let bad_if = |bad: bool, text: String| if bad { Span::styled(text, red().add_modifier(Modifier::BOLD)) } else { Span::raw(text) };
        let pos = |v: Option<f64>| v.is_some_and(|x| x > 0.0);
        problems += h.problems().len();
        let cells = vec![
            vec![Span::styled("pstate ", dim()), Span::raw(h.pstate.clone().unwrap_or_else(|| "-".into()))],
            vec![Span::styled("PCIe gen ", dim()), Span::raw(format!("{}/{}", n(h.pcie_gen), n(h.pcie_gen_max))), Span::styled(" width ", dim()), bad_if(h.pcie_width_degraded(), format!("x{}/x{}", n(h.pcie_width), n(h.pcie_width_max)))],
            vec![Span::styled("ECC corrected ", dim()), Span::raw(n(h.ecc_corrected)), Span::styled(" uncorrected ", dim()), bad_if(pos(h.ecc_uncorrected), n(h.ecc_uncorrected))],
            vec![
                Span::styled("remapped rows corr ", dim()),
                Span::raw(n(h.remap_correctable)),
                Span::styled(" uncorr ", dim()),
                bad_if(pos(h.remap_uncorrectable), n(h.remap_uncorrectable)),
                Span::styled(" pending ", dim()),
                bad_if(h.remap_pending == Some(true), yn(h.remap_pending)),
                Span::styled(" failure ", dim()),
                bad_if(h.remap_failure == Some(true), yn(h.remap_failure)),
            ],
            vec![Span::styled("retired pages sbe ", dim()), Span::raw(n(h.retired_sbe)), Span::styled(" dbe ", dim()), bad_if(pos(h.retired_dbe), n(h.retired_dbe)), Span::styled(" pending ", dim()), bad_if(h.retired_pending == Some(true), yn(h.retired_pending))],
            vec![Span::styled("power limit ", dim()), Span::raw(format!("{} W (max {} W)", n(h.power_enforced_w), n(h.power_max_limit_w)))],
            vec![Span::styled("clock max ", dim()), Span::raw(format!("{} MHz", n(h.clock_max_mhz)))],
            vec![Span::styled("mem temp ", dim()), Span::raw(g.sample.mem_temp_c.map_or_else(|| "n/a on this board".to_string(), lss_core::units::temp_compact))],
        ];
        groups.push(flow(Span::styled(format!("GPU{:<2} ", g.sample.index), bold()), cells, w));
    }
    let mut detail = match read_at {
        Some(t) => plain(format!("read {} ago", fmt_duration((now - t).max(0)))),
        None => vec![],
    };
    if problems > 0 {
        detail.push(Span::styled(format!(" · {problems} PROBLEM(S)"), red()));
    }
    let mut out = table_sections("HEALTH", detail, colour, None, groups, body.height, "");
    if s.gpus.is_empty() {
        out = vec![message("HEALTH", colour, "no GPU reading - nvidia-smi failed or timed out".into(), true, body.width)];
    }
    out
}

// ---------------------------------------------------------------- 4 GATEWAY

fn gateway_header(first: &str, name_w: usize, wide: bool) -> Line<'static> {
    let est = if wide { "  est tok avg/max" } else { "" };
    Line::styled(format!("{first:<name_w$} {:>6} {:>6} {:>5} {:>5} {:>4} {:>4} {:>4} {:>4}{est}", "reqs", "2xx", "4xx", "5xx", "413", "429", "499", "503"), dim())
}

/// One table row; on a narrow pane the gate's token estimate goes on a second, dim line.
fn gateway_row(r: &GatewayRow, name: String, name_w: usize, width: usize, wide: bool) -> Vec<Line<'static>> {
    let est = match (r.est_tokens_avg, r.est_tokens_max) {
        (Some(a), Some(m)) => Some(format!("{}/{} ({}{})", f_num(a), f_num(m), r.est_tokens_n, if wide { " rejected" } else { "" })),
        _ => None,
    };
    // a breakdown that was not recorded is `-`: unknown is not zero
    let c = |v: Option<u64>| v.map_or_else(|| "-".to_string(), count);
    let bad = |v: Option<u64>| if v.is_some_and(|n| n > 0) { red() } else { Style::default() };
    let mut spans = vec![
        Span::styled(format!("{:<name_w$} ", fit(&name, name_w)), bold()),
        Span::raw(format!("{:>6} {:>6} {:>5} ", count(r.requests), c(r.c2xx), c(r.c4xx))),
        Span::styled(format!("{:>5}", c(r.c5xx)), bad(r.c5xx)),
        Span::raw(format!(" {:>4} {:>4} {:>4} ", c(r.s413), c(r.s429), c(r.s499))),
        Span::styled(format!("{:>4}", c(r.s503)), bad(r.s503)),
    ];
    if wide {
        spans.push(Span::raw(format!("  {}", est.unwrap_or_else(|| "-".into()))));
        return vec![fit_line(spans, width)];
    }
    let mut lines = vec![fit_line(spans, width)];
    if let Some(e) = est {
        lines.push(fit_line(vec![Span::styled(format!("{:name_w$} est tokens avg/max {e} rejected", ""), dim())], width));
    }
    lines
}

/// A table cut into screen-sized sections, so every row can be reached by scrolling sections.
fn table_sections(title: &str, detail: Vec<Span<'static>>, colour: Color, header: Option<Line<'static>>, groups: Vec<Vec<Line<'static>>>, avail_h: u16, empty: &str) -> Vec<Section> {
    let head = usize::from(header.is_some());
    let room = (avail_h as usize).saturating_sub(2 + head).max(1);
    if groups.is_empty() {
        let mut lines: Vec<Line> = header.into_iter().collect();
        lines.push(Line::styled(empty.to_string(), dim()));
        let h = lines.len() as u16 + 2;
        return vec![Section { title: title.to_string(), detail, colour, half: false, min_h: h, want_h: h, body: Body::Lines(lines) }];
    }
    let total = groups.len();
    let mut out = Vec::new();
    let mut chunk: Vec<Line> = Vec::new();
    let mut first = 1;
    let mut n_in = 0;
    let flush = |chunk: &mut Vec<Line<'static>>, first: usize, n_in: usize, out: &mut Vec<Section>| {
        if chunk.is_empty() {
            return;
        }
        let mut lines: Vec<Line> = header.clone().into_iter().collect();
        lines.append(chunk);
        let h = lines.len() as u16 + 2;
        let t = if n_in == total { title.to_string() } else { format!("{title} {first}-{} of {total}", first + n_in - 1) };
        out.push(Section { title: t, detail: detail.clone(), colour, half: false, min_h: h.min(avail_h.max(3)), want_h: h, body: Body::Lines(lines) });
    };
    for (i, g) in groups.into_iter().enumerate() {
        if !chunk.is_empty() && chunk.len() + g.len() > room {
            flush(&mut chunk, first, n_in, &mut out);
            first = i + 1;
            n_in = 0;
        }
        chunk.extend(g);
        n_in += 1;
    }
    flush(&mut chunk, first, n_in, &mut out);
    out
}

fn gateway(out: &mut Vec<Section>, d: &PageData, s: &Status, app: &App, body: Rect) {
    let colour = page_colour(PageId::Gateway);
    let w = (body.width as usize).saturating_sub(2);
    // #23, 2026-09-21: with no gateway configured this page fell through to the generic "the
    // collector sent nothing for this page" - true, but not why, and not what the overview and
    // lss status already say in the same situation.
    if s.gate.absent {
        out.push(message("GATEWAY", colour, "no gateway configured: requests go straight to the engine. Lanes, per-user numbers and rejections need one in front of it (gate_url in the collector's config).".into(), false, body.width));
        return;
    }
    if let Some(g) = &d.gateway {
        let name_w = if w >= 90 { 22 } else { 14 };
        let mut detail = if s.gate.up { plain(format!("gate {} · probes: {} excluded", s.gate.version.as_deref().unwrap_or("up"), g.probes_excluded)) } else { vec![Span::styled("GATE DOWN", red())] };
        if !s.gate.upstream_ok && s.gate.up {
            detail.push(Span::styled(" · upstream NOT ok", red()));
        }
        let wide = w >= 90;
        let rows: Vec<Vec<Line>> = g.lanes.iter().map(|r| gateway_row(r, r.name.clone(), name_w, w, wide)).collect();
        out.extend(table_sections("LANES", detail, colour, Some(gateway_header("lane", name_w, wide)), rows, body.height, "no requests in this range"));
        let rows: Vec<Vec<Line>> = g.keys.iter().map(|r| gateway_row(r, format!("{}{}", r.name, if r.lane == "trusted" { " (t)" } else { "" }), name_w, w, wide)).collect();
        // part of the range may predate per-key status codes: say from when the code columns count
        let mut detail = plain(format!("{} · (t) = trusted", g.keys.len()));
        match (g.codes_coverage.as_str(), g.codes_since) {
            ("partial", Some(t)) => detail.push(Span::styled(format!(" · codes since {}", fmt_local(t, if g.range_s > 86_400 { "%m-%d %H:%M" } else { "%H:%M" })), dim())),
            ("none", _) => detail.push(Span::styled(" · codes not recorded in this range", dim())),
            _ => {}
        }
        out.extend(table_sections("KEYS", detail, colour, Some(gateway_header("key", name_w, wide)), rows, body.height, "no keyed requests in this range"));
    }
    if let Some(doc) = &d.series {
        let lanes = |what: &str| vec![Series::new("public", Color::Cyan, doc.get(&format!("{what}_public"))), Series::new("trusted", Color::Green, doc.get(&format!("{what}_trusted")))];
        let (p, t) = (&s.lanes.public, &s.lanes.trusted);
        out.push(chart("IN-FLIGHT TOKENS", plain(format!("public {} · trusted {}", count(p.inflight_tokens), count(t.inflight_tokens))), colour, lanes("inflight"), peak(opts(app, f_num, Some(0.0), None))));
        let waiting = p.waiters + t.waiters;
        out.push(chart("WAITERS", if waiting > 0 { vec![Span::styled(format!("{waiting} waiting for token budget"), warn())] } else { plain("nobody waiting".into()) }, colour, lanes("waiters"), peak(opts(app, f_num, Some(0.0), None))));
    }
    if let Some(g) = &d.gateway {
        let max = g.ips.iter().map(|r| r.requests).max().unwrap_or(1).max(1);
        let bar_w = w.saturating_sub(40).clamp(4, 40);
        // card #228: the bars are DATA ink, so they take the lane's series colour (public Cyan,
        // trusted Green, as on every chart) - not the page's own colour, which on this page is
        // Yellow, warn()'s amber, and made every address read as a warning
        let rows: Vec<Vec<Line>> = g
            .ips
            .iter()
            .map(|r| {
                let n = ((r.requests as f64 / max as f64) * bar_w as f64).round().max(1.0) as usize;
                vec![fit_line(vec![Span::styled(format!("{:<22} ", fit(&r.name, 22)), bold()), Span::raw(format!("{:<8} {:>6} ", r.lane, count(r.requests))), Span::styled("█".repeat(n), fg(if r.lane == "trusted" { Color::Green } else { Color::Cyan }))], w)]
            })
            .collect();
        out.extend(table_sections("TOP CLIENT ADDRESSES", plain("last octet masked · probe excluded".into()), colour, None, rows, body.height, "no addresses logged in this range"));
    }
}

// ---------------------------------------------------------------- 8 ALERTS / 9 INCIDENTS

/// card #124 item 1: the ADMISSION VERDICT, rendered at last. `admission_verdict()` and its 7
/// unit tests have lived in lss-core with ZERO callers - Husain, on the panel: "a pure function
/// with 7 passing tests and zero callers is a liability, not an asset; the test suite is the
/// only consumer". It answers ONE question: when work is not moving, whose fault is it, the
/// gateway's or the engine's. The original panel asked for it because "we already lost hours to
/// a queue that was the gateway's fault while the engine was idle". Page 8, not page 1, because
/// it IS a health rollup and page 1 deliberately no longer does health framing (#73).
fn admission_verdict_parts(s: &Status) -> (String, ratatui::style::Style, String) {
    use lss_core::model::AdmitVerdict;
    let v = s.admission_verdict();
    let label = v.label().to_string();
    match v {
        AdmitVerdict::Ok => (label, good(), "nothing is being held back".to_string()),
        AdmitVerdict::GatewayLimited => (
            label,
            red().add_modifier(Modifier::BOLD),
            format!(
                "{} waiting at the GATEWAY while the engine has room ({:.0} of {} slots) - the queue is ours, not the engine's",
                s.lanes.public.waiters + s.lanes.trusted.waiters, s.serve.running, s.serve.slots
            ),
        ),
        AdmitVerdict::EngineFull => (label, warn(), format!("the ENGINE is full: {:.0} of {} slots running", s.serve.running, s.serve.slots)),
        AdmitVerdict::KvLimited => (label, warn(), format!("KV memory is the limit: {:.0}% of the pool in use", s.serve.kv_usage * 100.0)),
        AdmitVerdict::Down => (label, red().add_modifier(Modifier::BOLD), "the serve is down".to_string()),
    }
}

/// The verdict in the RULES box's header - the LABEL only, because the header is also where the
/// rule counts live and a 60-column terminal truncates the tail (the existing alerts render test
/// caught exactly that: "0 firing" fell off the end). The sentence behind the verdict becomes a
/// body row instead, and ONLY when the verdict is not OK - so a calm page costs nothing and a
/// page where something IS being refused says who is refusing it. Two earlier shapes were worse
/// and both were caught by that same test: its own section ate a short pane and pushed the rule
/// table off screen, and an unconditional body row made the table taller than the pane so the
/// layout dropped it entirely.
fn admission_detail(s: &Status) -> Vec<Span<'static>> {
    let (label, style, _) = admission_verdict_parts(s);
    vec![Span::styled(label, style), Span::raw(" \u{b7} ")]
}

fn admission_verdict_line(s: &Status, w: usize) -> Line<'static> {
    let (label, style, why) = admission_verdict_parts(s);
    fit_line(vec![Span::styled(format!("{label:<16}"), style), Span::raw(why)], w)
}

/// Card #231 (the owner: "alerts and incident should have their pages" / "seperate the alerts
/// and incidents then"): ADMISSION + RULES + ALERT HISTORY, the alert-shaped two-thirds of what
/// page 8 used to carry alone. INCIDENTS (below) is its own page 9 now - both still read the one
/// `/rules` document (`page_paths`), so a fetch failure or an empty document is handled the same
/// way in each rather than shared through one function.
fn alerts(out: &mut Vec<Section>, d: &PageData, s: &Status, body: Rect, now: i64) {
    let w = (body.width as usize).saturating_sub(2);
    // card #124: the verdict comes from /status, the rule table from a separate fetch - so a
    // page that cannot show its rules must still answer "is anything being refused right now".
    let Some(doc) = &d.rules else {
        out.push(lines_section("ADMISSION", vec![], page_colour(PageId::Alerts), vec![admission_verdict_line(s, w)], false));
        return;
    };
    let firing = doc.rules.iter().filter(|r| r.state == "firing").count();
    let pending = doc.rules.iter().filter(|r| r.state == "pending").count();
    let colour = if firing > 0 { Color::Red } else { page_colour(PageId::Alerts) };
    let wide = w >= 110;
    let groups: Vec<Vec<Line>> = doc
        .rules
        .iter()
        .map(|r| {
            let (word, st) = match r.state.as_str() {
                "firing" => ("FIRING ", red().add_modifier(Modifier::BOLD)),
                "pending" => ("pending", warn()),
                // #52: a flapping identity re-labels rather than keeps firing - dim, not red
                // (the measurement is unreliable, not the server: this is not a problem NOW)
                "muted" => ("muted  ", dim()),
                _ => ("ok     ", good()),
            };
            let fired = r.last_fired.map_or_else(|| "never".to_string(), |t| fmt_local(t, "%m-%d %H:%M"));
            let cool = if r.cooldown_remaining_s > 0 { fmt_duration(r.cooldown_remaining_s) } else { "-".to_string() };
            let since = r.pending_since.filter(|_| r.state != "ok").map_or_else(String::new, |t| format!(" for {}", fmt_duration(now - t)));
            if wide {
                vec![fit_line(
                    vec![
                        Span::styled(format!("{word} "), st),
                        Span::styled(format!("{:<26} ", fit(&r.rule, 26)), bold()),
                        Span::raw(format!("{:<24} ", fit(&format!("{}{since}", r.value), 24))),
                        Span::styled(format!("{:<38} ", fit(&r.threshold, 38)), dim()),
                        Span::raw(format!("{fired:<12} {cool}")),
                    ],
                    w,
                )]
            } else {
                vec![
                    fit_line(vec![Span::styled(format!("{word} "), st), Span::styled(format!("{:<24} ", fit(&r.rule, 24)), bold()), Span::raw(format!("{}{since}", r.value))], w),
                    fit_line(vec![Span::styled(format!("        {} · fired {fired} · cooldown {cool}", r.threshold), dim())], w),
                ]
            }
        })
        .collect();
    // card #124: the ADMISSION VERDICT leads this box's header - the question page 8 is opened
    // for, answered before the rule table. When it is NOT OK, the sentence naming whose queue
    // it is goes in as the first body row (one row, only on a day that needs it).
    let mut groups = groups;
    if !matches!(s.admission_verdict(), lss_core::model::AdmitVerdict::Ok) {
        groups.insert(0, vec![admission_verdict_line(s, w)]);
    }
    let mut detail = admission_detail(s);
    detail.push(Span::raw(format!("{} rules \u{b7} ", doc.rules.len())));
    detail.push(if firing > 0 { Span::styled(format!("{firing} FIRING"), red()) } else { Span::raw("0 firing") });
    detail.push(Span::raw(format!(" \u{b7} {pending} pending")));
    let header = wide.then(|| Line::styled(format!("{:<8}{:<27}{:<25}{:<39}{:<13}{}", "state", "rule", "value", "condition", "last fired", "cooldown"), dim()));
    out.extend(table_sections("RULES", detail, colour, header, groups, body.height, "the collector reported no rules"));

    let spool = doc.spool_depth.map_or_else(|| "spool unreadable".to_string(), |n| format!("mail spool {n} waiting"));
    let undelivered = doc.alerts.iter().filter(|a| !a.delivered).count();
    let mut detail = plain(format!("{} · {spool}", doc.alerts.len()));
    if undelivered > 0 {
        detail.push(Span::styled(format!(" · {undelivered} undelivered"), warn()));
    }
    let rows: Vec<Vec<Line>> = doc
        .alerts
        .iter()
        .map(|a| {
            let sev = if a.recovered { good() } else { match a.severity.as_str() { "page" | "hardware" => red(), "warn" => warn(), _ => Style::default() } };
            vec![fit_line(
                vec![
                    Span::styled(fmt_local(a.ts, "%m-%d %H:%M "), dim()),
                    Span::styled(format!("{:<9}", a.severity), sev),
                    if a.delivered { Span::styled("delivered   ", dim()) } else { Span::styled("UNDELIVERED ", warn()) },
                    Span::raw(a.message.clone()),
                ],
                w,
            )]
        })
        .collect();
    out.extend(table_sections("ALERT HISTORY", detail, page_colour(PageId::Alerts), None, rows, body.height, "no alerts recorded"));
}

/// Card #231: the INCIDENTS table that used to be the last section of the combined page 8,
/// split off onto its own page 9 - the reading source is the same `/rules` document `alerts()`
/// reads (`page_paths`: both pages fetch `/rules`), so a document that failed to fetch is
/// reported the same "waiting on it" way rather than a second, differently-worded failure mode.
fn incidents(out: &mut Vec<Section>, d: &PageData, body: Rect, now: i64) {
    let w = (body.width as usize).saturating_sub(2);
    let Some(doc) = &d.rules else {
        out.push(message("INCIDENTS", page_colour(PageId::Incidents), "waiting on /rules ...".to_string(), false, body.width));
        return;
    };
    let pct = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{:.2}%", x * 100.0));
    let open = doc.incidents.iter().filter(|i| i.end.is_none()).count();
    let mut detail = plain(format!("uptime 24h {} · 7d {}", pct(doc.uptime.h24), pct(doc.uptime.d7)));
    if open > 0 {
        detail.push(Span::styled(format!(" · {open} OPEN"), red()));
    }
    let rows: Vec<Vec<Line>> = doc
        .incidents
        .iter()
        .map(|i| {
            let (span, style) = match i.end {
                None => (format!("OPEN {}", fmt_duration(now - i.start)), red()),
                Some(e) if e > i.start => (fmt_duration(e - i.start), Style::default()),
                Some(_) => ("-".to_string(), dim()),
            };
            vec![fit_line(vec![Span::styled(fmt_local(i.start, "%m-%d %H:%M "), dim()), Span::styled(format!("{:<18}", i.kind), if i.kind == "xid" { warn() } else { Style::default() }), Span::styled(format!("{span:>9}  "), style), Span::raw(i.detail.clone())], w)]
        })
        .collect();
    out.extend(table_sections("INCIDENTS", detail, page_colour(PageId::Incidents), None, rows, body.height, "none on record"));
}

// ---------------------------------------------------------------- shared by the newer pages

/// A boxed list of lines that takes exactly the rows it needs.
fn lines_section(title: &str, detail: Vec<Span<'static>>, colour: Color, lines: Vec<Line<'static>>, half: bool) -> Section {
    // it wants the rows it needs; when rows are short it shrinks and says how much is hidden
    // (a list taller than the whole pane is split into continuation sections by `split_tall`)
    let h = lines.len() as u16 + 2;
    Section { title: title.to_string(), detail, colour, half, min_h: h.min(6), want_h: h, body: Body::Lines(lines) }
}

/// A list taller than the pane cannot scroll inside its box, so it becomes several boxes.
fn split_tall(sections: Vec<Section>, avail_h: u16) -> Vec<Section> {
    let room = (avail_h as usize).saturating_sub(2).max(1);
    let mut out = Vec::new();
    for sec in sections {
        match sec.body {
            Body::Lines(lines) if lines.len() > room => {
                let chunks: Vec<Vec<Line<'static>>> = lines.chunks(room).map(<[Line<'static>]>::to_vec).collect();
                let n = chunks.len();
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let h = chunk.len() as u16 + 2;
                    out.push(Section { title: if i == 0 { sec.title.clone() } else { format!("{} ({} of {n})", sec.title, i + 1) }, detail: if i == 0 { sec.detail.clone() } else { vec![] }, colour: sec.colour, half: false, min_h: h.min(6), want_h: h, body: Body::Lines(chunk) });
                }
            }
            body => out.push(Section { body, ..sec }),
        }
    }
    out
}

/// One line of explanation under a technical term: dim, never cut mid-word.
fn explain(text: &str, width: usize) -> Vec<Line<'static>> {
    super::widgets::wrap(text, width.max(10)).into_iter().map(|l| Line::styled(l, dim())).collect()
}

fn kv(label_text: &str, value: String, width: usize) -> Line<'static> {
    fit_line(vec![Span::styled(format!("{label_text:<26}"), dim()), Span::styled(value, bold())], width)
}

fn ago(ts: i64, now: i64) -> String {
    crate::report::ago(ts, now)
}

/// Same rule as `GPU_COLOURS` (card #48): never red, the terminal's own named ANSI colours so
/// both themes stay legible, one colour per user consistent across USERS and TOKENS.
// card #228: never Yellow/LightYellow - that is warn(), the amber band (the same drift #223 fixed in
// GPU_COLOURS); a user drawn in it read as a warning about that user
const USER_COLOURS: [Color; 7] = [Color::LightBlue, Color::Green, Color::Magenta, Color::Cyan, Color::LightGreen, Color::LightMagenta, Color::LightCyan];

// ---------------------------------------------------------------- 4 USERS

fn user_header(wide: bool) -> Line<'static> {
    let text = if wide {
        format!("{:<16} {:<8} {:>3} {:>10}  {:<12} {:>7} {:>11} {:>17} {:>9} {:>9}  {}", "user", "lane", "now", "pk 10m/24h", "limit use", "req/min", "req 1h/24h", "ok/rej/err/closed", "read 24h", "wrote 24h", "last seen")
    } else {
        format!("{:<16} {:<8} {:>3}  {:<12} {:>11}  {}", "user", "lane", "now", "limit use", "req 1h/24h", "last seen")
    };
    Line::styled(text, dim())
}

fn user_row(r: &lss_core::users::UserRow, wide: bool, width: usize, now: i64, quiet: bool) -> Vec<Line<'static>> {
    let g = &r.gate;
    let at_limit = g.conc_limit.is_some_and(|l| l > 0 && g.inflight >= l);
    let limit = match g.conc_limit {
        Some(l) if l > 0 => format!("{} {}/{}", gauge(g.inflight as f64 / l as f64, 6), g.inflight, l),
        _ => format!("{} no limit", g.inflight),
    };
    let name_style = if quiet { dim() } else if g.inflight > 0 { bold() } else { Style::default() };
    let limit_style = if at_limit { warn().add_modifier(Modifier::BOLD) } else { Style::default() };
    let bad = g.errors_24h > 0;
    let out = crate::report::out_tokens(r);
    // each user's own experience, under their row (from the gateway's log of their requests)
    let experience = crate::report::experience_text(r).map(|e| Line::styled(fit(&format!("  {e}"), width), dim()));
    let mut lines = user_row_lines(r, wide, width, now, (name_style, limit_style, bad), (&limit, &out));
    lines.extend(experience);
    lines
}

fn user_row_lines(r: &lss_core::users::UserRow, wide: bool, width: usize, now: i64, (name_style, limit_style, bad): (Style, Style, bool), (limit, out): (&str, &str)) -> Vec<Line<'static>> {
    let g = &r.gate;
    if wide {
        vec![fit_line(
            vec![
                Span::styled(format!("{:<16.16} ", r.name), name_style),
                Span::styled(format!("{:<8} ", g.lane), dim()),
                Span::styled(format!("{:>3} ", g.inflight), if g.inflight > 0 { bold() } else { dim() }),
                Span::raw(format!("{:>10}  ", format!("{}/{}", g.peak_inflight_10m, g.peak_inflight_24h))),
                Span::styled(format!("{limit:<12} "), limit_style),
                Span::raw(format!("{:>7} {:>11} ", g.rpm_now, format!("{}/{}", g.requests_1h, g.requests_24h))),
                Span::styled(format!("{:>17} ", format!("{}/{}/{}/{}", g.ok_24h, g.rejected_24h, g.errors_24h, g.client_closed_24h)), if bad { red() } else { Style::default() }),
                Span::raw(format!("{:>9} {:>9}  ", format!("~{}", crate::report::big(g.prompt_tokens_est_24h as f64)), out)),
                Span::styled(ago(g.last_seen as i64, now), dim()),
            ],
            width,
        )]
    } else {
        vec![
            fit_line(vec![Span::styled(format!("{:<16.16} ", r.name), name_style), Span::styled(format!("{:<8} ", g.lane), dim()), Span::styled(format!("{:>3}  ", g.inflight), if g.inflight > 0 { bold() } else { dim() }), Span::styled(format!("{limit:<12} "), limit_style), Span::raw(format!("{:>11}  ", format!("{}/{}", g.requests_1h, g.requests_24h))), Span::styled(ago(g.last_seen as i64, now), dim())], width),
            fit_line(vec![Span::styled(format!("  peak {}/{} · {} req/min · ok/rej/err/closed ", g.peak_inflight_10m, g.peak_inflight_24h, g.rpm_now), dim()), Span::styled(format!("{}/{}/{}/{}", g.ok_24h, g.rejected_24h, g.errors_24h, g.client_closed_24h), if bad { red() } else { Style::default() }), Span::styled(format!(" · read ~{} · wrote {out}", crate::report::big(g.prompt_tokens_est_24h as f64)), dim())], width),
        ]
    }
}

fn users(out: &mut Vec<Section>, d: &PageData, s: &Status, app: &App, body: Rect, now: i64) {
    let colour = page_colour(PageId::Users);
    let w = (body.width as usize).saturating_sub(2);
    let u = &s.users;
    let slots = s.serve.slots.max(1);
    let mut head = vec![fit_line(vec![Span::styled("slots in use     ", dim()), Span::styled(format!("{} {:.0}/{}", gauge(s.serve.running / f64::from(slots), 10), s.serve.running, s.serve.slots), bold()), Span::styled("   a slot is one request the model is working on", dim())], w)];
    if !u.available {
        // #23, 2026-09-21: this said "needs gateway v5.2" even with NO gateway at all - the
        // overview already tells the two apart (s.gate.absent); this page did not.
        let (title, why) = if s.gate.absent {
            ("no gateway configured", "Who is using them is not known: nothing sits in front of the engine to tell users apart. Requests go straight to it (gate_url in the collector's config adds one).".to_string())
        } else {
            ("needs gateway v5.2", "Who is using them is not known yet: per-user numbers come from a gateway (v5.2 or newer), and this one does not publish them. Until it is upgraded, page 7 GATEWAY has requests per key and per address from the gateway's log.".to_string())
        };
        head.extend(explain(&why, w));
        out.push(lines_section("USERS", plain(title.into()), colour, head, false));
        // what IS known without the gateway's user table: how many requests ran at once, per lane
        if let Some(doc) = &d.series {
            out.push(chart_wide("RUNNING AT ONCE, BY LANE", plain(format!("{:.0} of {} slots now", s.serve.running, s.serve.slots)), colour, vec![Series::new("public", Color::Cyan, doc.get("running_public")), Series::new("trusted", Color::Green, doc.get("running_trusted"))], peak(opts(app, f_num, Some(0.0), Some(f64::from(slots)))), app.chart_lines, body.width));
        }
        return;
    }
    head.insert(0, fit_line(vec![Span::styled("people           ", dim()), Span::styled(format!("{} active now", u.active_now), bold()), Span::raw(format!("  ·  {} in the last 10 min  ·  {} in the last 24 h  ·  {} attempts without a valid key", u.totals.users_active_10m, u.totals.users_24h, u.totals.unauthenticated_24h))], w));
    out.push(lines_section("USERS", plain(format!("{} now · {} in 24h", u.active_now, u.totals.users_24h)), colour, head, false));

    let wide = w >= 122;
    let mut rows = u.rows.clone();
    app.user_sort.apply(&mut rows);
    let mut groups: Vec<Vec<Line<'static>>> = rows.iter().map(|r| user_row(r, wide, w, now, false)).collect();
    // the benchmark and the monitor's own probe: shown apart, dim, in no total
    for r in u.bench.iter().chain(u.probe.iter()) {
        groups.push(user_row(r, wide, w, now, true));
    }
    out.extend(table_sections("WHO", plain(format!("sorted by {} · s sorts · ~ = estimated · lss-bench / lss-probe are not users", app.user_sort.label())), colour, Some(user_header(wide)), groups, body.height, "nobody has used the server in the last 24 hours"));

    if let Some(doc) = &d.series {
        let named: Vec<(String, String)> = u.rows.iter().chain(u.bench.iter()).map(|r| (r.series_id.clone(), r.name.clone())).collect();
        let lines = |prefix: &str| -> Vec<Series> {
            named.iter().enumerate().filter(|(_, (id, _))| doc.series.contains_key(&format!("{prefix}.{id}"))).map(|(i, (id, name))| Series::new(name, USER_COLOURS[i % USER_COLOURS.len()], doc.get(&format!("{prefix}.{id}")))).collect()
        };
        let mut running = lines("user_inflight");
        if running.is_empty() {
            running.push(Series::new("all users", USER_COLOURS[0], doc.get("users_inflight")));
        }
        // same source as the "slots in use" header above (`s.serve.running`), not the series' own
        // last point: those are sampled at different instants and used to disagree (E4, 2026-09-20).
        out.push(chart_wide("RUNNING AT ONCE, PER USER", plain(format!("now {:.0} of {} slots", s.serve.running, s.serve.slots)), colour, running, peak(opts(app, f_num, Some(0.0), Some(f64::from(slots)))), app.chart_lines, body.width));
        let mut rate = lines("user_rpm");
        if rate.is_empty() {
            rate.push(Series::new("active users", USER_COLOURS[0], doc.get("users_active")));
        }
        out.push(chart_wide("REQUESTS / MIN, PER USER", plain(format!("{} active", last_of(doc, "users_active", f_num))), colour, rate, peak(opts(app, f_num, Some(0.0), None)), app.chart_lines, body.width));
    }
}

// ---------------------------------------------------------------- 5 TOKENS

fn hbar(value: f64, top: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cells = if top > 0.0 { (value / top * width as f64).round() as usize } else { 0 };
    let cells = if value > 0.0 { cells.clamp(1, width) } else { 0 };
    format!("{}{}", "█".repeat(cells), " ".repeat(width - cells))
}

/// card #176 item 2: the chart the owner asked for - "some kind of chart that shows spending" -
/// drawn with card #48's connected-line renderer, one series in its own box.
///
/// It lives on the TOKENS page rather than page 1 for a structural reason, not a lazy one: page
/// 1 is a flat grid of label/value rows by card #73's design ("ONE grid, ONE label-column
/// width, no nested boxes") and has no chart body at all, while this page already draws charts
/// and already answers the neighbouring question - how much was SERVED. Tokens and dollars are
/// the two halves of "how much did we use", so they belong on one screen. Page 1's SPENDING
/// rows name this page so nobody has to hunt for it.
/// card #229 (#226's pages.rs half): MONTH TO DATE and YEAR TO DATE on the spending detail, the
/// same two rows page 1 shows. Built from the SAME readings (`cost.month`, `cost.ytd`) - label,
/// value and the one-sentence caveat - so a partial year says "since Jan 1 (the stored history
/// starts <date>)" and is called a floor in exactly page 1's words: one owner for every string.
/// Drawn whenever there is spending at all - unlike the chart, it does not need two days.
fn spending_to_date(out: &mut Vec<Section>, s: &Status, width: u16) {
    if s.spending.is_none() {
        return;
    }
    let rs = lss_core::readings::cost(s);
    let w = (width as usize).saturating_sub(2);
    let mut lines = Vec::new();
    for key in ["cost.month", "cost.ytd"] {
        if let Some(r) = rs.iter().find(|r| r.key == key) {
            lines.push(kv(&r.label, r.value_or_dash().to_string(), w));
            lines.extend(explain(&r.detail, w));
        }
    }
    if !lines.is_empty() {
        out.push(lines_section("SPENDING TO DATE", plain("delivery+generation only".into()), page_colour(PageId::Tokens), lines, false));
    }
}

fn spending_chart(out: &mut Vec<Section>, s: &Status, app: &App, f_num: fn(f64) -> String) {
    let Some(sp) = &s.spending else { return };
    if sp.daily.len() < 2 {
        return;  // a single point is not a trend, and a one-point line reads as a flat one
    }
    let colour = page_colour(PageId::Tokens);
    let points: Vec<Option<f64>> = sp.daily.iter().map(|d| Some(d.usd)).collect();
    let total: f64 = sp.daily.iter().map(|d| d.usd).sum();
    let peak = sp.daily.iter().fold(None::<&lss_core::rates::DaySpend>, |acc, d| match acc {
        Some(b) if b.usd >= d.usd => Some(b),
        _ => Some(d),
    });
    let mut detail = plain(format!(
        "{} days · ${:.2} total · ${:.2}/day average",
        sp.daily.len(),
        total,
        total / sp.daily.len() as f64
    ));
    if let Some(p) = peak {
        detail.push(Span::styled(format!(" · most expensive {} at ${:.2}", p.date, p.usd), dim()));
    }
    // card #176 item 5, SECOND attempt - the first one put this caveat in the detail and
    // lss-verifier-4 killed it with arithmetic: title_line cuts the DETAIL from the right and
    // keeps the title (widgets.rs "cut the detail, keep its closing bracket"), and this box is
    // half: true, so at 200 columns the detail has ~100 and the caveat - the LAST span - was cut
    // COMPLETELY. A caveat that disappears exactly when the box gets narrow is not a caveat, and
    // it was on the box whose whole purpose is that a screenshot of it cannot be read as a bill.
    // So the SCOPE goes in the TITLE, which truncation keeps, and the detail carries only the
    // PRECISION (effective date, the fixed charge) that can be lost without the box lying.
    if let Some(c) = &s.cost {
        // "configured" is load-bearing: an unconfigured charge is not an absent one, and the
        // other four sites all say it (plain.rs, dash.rs). Dropping the word turned "we were not
        // told" into "there is none" - verifier-4 caught that too.
        let excludes = c.fixed_usd_per_day.map_or_else(
            || "no fixed daily charge configured".to_string(),
            |v| format!("excludes ${v:.2}/day fixed charge"));
        detail.push(Span::styled(format!(" \u{b7} as of {} - {excludes}", c.effective_date), dim()));
    }
    out.push(chart(
        "SPENDING PER DAY, DELIVERY+GENERATION ONLY",
        detail,
        colour,
        vec![Series::new("$ per day", Color::Cyan, &points)],
        opts(app, f_num, Some(0.0), None),
    ));
}

fn tokens(out: &mut Vec<Section>, d: &PageData, s: &Status, body: Rect, now: i64) {
    let colour = page_colour(PageId::Tokens);
    let w = (body.width as usize).saturating_sub(2);
    let big = crate::report::big;
    // #23, 2026-09-21: an engine that reports neither counter (Ollama) used to show "0 written,
    // 0 read" for every window - indistinguishable from a real engine nobody has used yet.
    if s.serve.is_na("tokens") {
        out.push(message("TOKENS SERVED", colour, s.serve.na("tokens").unwrap_or_default(), false, body.width));
    } else if let Some(t) = &d.tokens {
        let mut lines = vec![Line::styled(format!("{:<22} {:>10} {:>10} {:>16} {:>10}", "window", "written", "read", "from cache", "requests"), dim())];
        for win in &t.windows {
            let name = match win.name.as_str() {
                "1h" => "last hour".to_string(),
                "24h" => "last 24 hours".to_string(),
                "7d" => "last 7 days".to_string(),
                "today" => "today".to_string(),
                _ => format!("all time ({})", fmt_duration(win.secs)),
            };
            let cache = win.cache_share.map_or_else(|| "-".to_string(), |c| format!("{} ({:.0}%)", big(win.cached), c * 100.0));
            lines.push(fit_line(vec![Span::raw(format!("{name:<22} ")), Span::styled(format!("{:>10} ", big(win.generated)), bold()), Span::raw(format!("{:>10} {cache:>16}  {}", big(win.prompt), crate::report::requests_text(win, now)))], w));
        }
        lines.extend(explain(&format!("Exact, from the engine's own counters. A token is a piece of a word (about 4 characters). written = generated by the model; read = prompt tokens; from cache = prompt tokens it did not have to read again. All time counts since {} and survives restarts of the model server.", fmt_local(t.all_time_since, "%Y-%m-%d")), w));
        out.push(lines_section("TOKENS SERVED", plain(format!("today {} written", t.windows.iter().find(|x| x.name == "today").map_or_else(|| "-".into(), |x| big(x.generated)))), colour, lines, false));
    }
    if let Some(doc) = &d.series {
        let (gen, prompt) = (doc.get("tok_gen:sum"), doc.get("tok_prompt:sum"));
        let top_gen = gen.iter().flatten().copied().fold(0.0, f64::max);
        let top_prompt = prompt.iter().flatten().copied().fold(0.0, f64::max);
        let bar_w = (w.saturating_sub(44) / 2).clamp(6, 40);
        let n = gen.len();
        let mut lines = Vec::new();
        // newest first: when rows are short it is the oldest hours that fall off the bottom
        for i in (n.saturating_sub(24)..n).rev() {
            let ts = doc.start_ts + i as i64 * doc.step_s;
            let (g, p) = (gen.get(i).copied().flatten().unwrap_or(0.0), prompt.get(i).copied().flatten().unwrap_or(0.0));
            lines.push(fit_line(vec![Span::styled(fmt_local(ts, if doc.step_s > 3600 { "%m-%d %Hh " } else { "%H:00 " }), dim()), Span::styled(hbar(g, top_gen, bar_w), fg(Color::LightGreen)), Span::raw(format!(" {:>7} ", big(g))), Span::styled(hbar(p, top_prompt, bar_w), fg(Color::Blue)), Span::raw(format!(" {:>7}", big(p)))], w));
        }
        let unit = if doc.step_s > 3600 { format!("{} HOURS", doc.step_s / 3600) } else { "HOUR".to_string() };
        let h = lines.len() as u16 + 2;
        out.push(Section { title: format!("TOKENS PER {unit}, newest first"), detail: vec![Span::styled("█ written", fg(Color::LightGreen)), Span::raw("  "), Span::styled("█ read", fg(Color::Blue)), Span::styled("  (each on its own scale)", dim())], colour, half: false, min_h: h.min(8), want_h: h, body: Body::Lines(lines) });
    }
    if let Some(t) = &d.tokens {
        let hw = half_width(body);
        let mut lines = Vec::new();
        match &t.peak {
            Some(p) => lines.push(fit_line(vec![Span::styled("fastest ever       ", dim()), Span::styled(format!("{:.0} tok/s", p.tok_s), bold()), Span::raw(format!(" written, all users together, on {}", fmt_local(p.ts, "%Y-%m-%d %H:%M")))], hw)),
            None => lines.push(Line::styled("nothing generated yet", dim())),
        }
        let days: Vec<Option<f64>> = t.peak_by_day.iter().rev().map(|p| Some(p.tok_s)).collect();
        if !days.is_empty() {
            let today = t.peak_by_day.first().map_or(0.0, |p| p.tok_s);
            lines.push(fit_line(vec![Span::styled("peak of each day   ", dim()), Span::styled(super::widgets::sparkline(&days, days.len().min(hw.saturating_sub(40))), fg(colour)), Span::raw(format!("  today {today:.0} tok/s · {} days", days.len()))], hw));
        }
        out.push(lines_section("PEAK SPEED", vec![], colour, lines, true));
        let mut lens = Vec::new();
        for (name, sum) in [("answers", &t.output_len), ("prompts", &t.prompt_len)] {
            match sum {
                Some(x) => lens.push(fit_line(vec![Span::styled(format!("{name:<9}"), dim()), Span::styled(format!("average {}", big(x.avg)), bold()), Span::raw(format!(" · half under {} · 9 in 10 under {} · 99 in 100 under {}", big(x.p50), big(x.p90), big(x.p99)))], hw)),
                None => lens.push(Line::styled(format!("{name:<9}no requests in the last 7 days"), dim())),
            }
        }
        out.push(lines_section("HOW LONG (tokens, 7 days)", vec![], colour, lens, true));
    }
    for h in &d.hists {
        let title = if h.metric == "gen_tokens" { "ANSWER LENGTH" } else { "PROMPT LENGTH" };
        let detail = h.summary_raw.as_ref().map_or_else(Vec::new, |x| plain(format!("average {} tokens · {} requests", big(x.avg), big(x.count))));
        out.push(Section { title: title.to_string(), detail, colour, half: true, min_h: 7, want_h: 11, body: Body::Dist(h.clone()) });
    }
    if let Some(t) = &d.tokens {
        if t.per_user_available {
            let top = t.per_user.iter().map(|u| u.output_24h as f64).fold(0.0, f64::max);
            let groups: Vec<Vec<Line<'static>>> = t.per_user.iter().map(|u| vec![fit_line(vec![Span::raw(format!("{:<18.18} ", u.name)), Span::styled(format!("{:<8} ", u.lane), dim()), Span::styled(hbar(u.output_24h as f64, top, 16), fg(colour)), Span::styled(format!(" {:>8}{}", big(u.output_24h as f64), if u.output_exact { " " } else { "~" }), bold()), Span::raw(format!(" written · ~{:>7} read · {:>5} requests", big(u.prompt_est_24h as f64), u.requests_24h)), Span::styled(u.output_share.map_or_else(String::new, |x| format!(" · {:.0}%", x * 100.0)), dim())], w)]).collect();
            out.extend(table_sections("BY USER (24 h)", plain("~ = estimated by the gateway".into()), colour, None, groups, body.height, "nobody in the last 24 hours"));
        } else {
            out.push(message("BY USER", colour, format!("Who the tokens went to: {}.", crate::report::USERS_NEEDS_GATE), false, body.width));
        }
    }
    let _ = now;
}

// ---------------------------------------------------------------- 6 MODEL

fn delta_span(mark: lss_core::compare::Mark, delta: Option<f64>) -> Span<'static> {
    use lss_core::compare::Mark;
    let text = delta.map_or_else(|| "-".to_string(), |d| format!("{d:+.1}%"));
    match mark {
        Mark::Better => Span::styled(format!("{text:>8} better"), good()),
        Mark::Worse => Span::styled(format!("{text:>8} WORSE "), red()),
        Mark::Same => Span::styled(format!("{text:>8} ~ same"), dim()),
        Mark::NoData => Span::styled(format!("{:>8}       ", "-"), dim()),
        // card #14: both numbers exist, the subtraction does not. Warn-coloured, not dim: this
        // is something the reader has to act on, not something that is merely missing.
        Mark::Incomparable => Span::styled(format!("{:>8} NOT CMP", "-"), warn()),
    }
}

/// One decimal; two for the small numbers where one would hide the difference (tokens per joule).
fn opt1(v: Option<f64>) -> String {
    match v {
        Some(x) if x.abs() < 10.0 && x.fract().abs() > 1e-9 => format!("{x:.2}"),
        Some(x) if x.fract().abs() < 1e-9 && x.abs() < 10.0 => format!("{x:.0}"),
        other => crate::report::opt(other, 1),
    }
}

/// The text width inside a HALF section: two share a row from `TWO_COLUMNS` on.
fn half_width(body: Rect) -> usize {
    let full = (body.width as usize).saturating_sub(2);
    if body.width >= TWO_COLUMNS { (body.width as usize / 2).saturating_sub(2) } else { full }
}

fn model(out: &mut Vec<Section>, d: &PageData, app: &App, body: Rect, now: i64) {
    let colour = page_colour(PageId::Model);
    let w = (body.width as usize).saturating_sub(2);
    let big = crate::report::big;
    let Some(doc) = &d.loadouts else { return };
    app.compare_len.set(doc.loadouts.len());
    let Some(c) = doc.loadouts.iter().find(|c| c.row.current).or(doc.loadouts.first()) else {
        out.push(message("MODEL", colour, "No loadout on record yet: the collector records one as soon as it sees the serve up.".into(), false, body.width));
        return;
    };
    let r = &c.row;
    let list = |sel: Option<usize>| -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(format!("  {:<13} {:<24} {:<16} {:<11} {:<7} {:>7} {:>11} {:>10} {:>8}", "id", "model", "image tag", "first seen", "load", "C1", "max total", "prefill 8k", "accuracy"), dim())];
        for (i, card) in doc.loadouts.iter().enumerate() {
            let h = card.headline();
            let cursor = if sel == Some(i) { "> " } else if card.row.current { "* " } else { "  " };
            let style = if sel == Some(i) { bold().fg(colour) } else if card.row.current { bold() } else { Style::default() };
            // card #14: the bench numbers in this row never travel without the condition they
            // were measured under
            let load = card.speed().map_or("-", |s| s.load_class().word());
            lines.push(fit_line(vec![Span::styled(format!("{cursor}{:<13} {:<24.24} {:<16.16} {:<11} {:<7} {:>7} {:>11} {:>10} {:>8}", card.row.id, card.row.model, card.row.image_tag, fmt_local(card.row.first_seen, "%m-%d %H:%M"), load, opt1(h.c1_tok_s), match (h.max_total_tok_s, h.max_total_at) { (Some(t), Some(n)) => format!("{t:.0} @{n}"), _ => "-".into() }, crate::report::opt(h.prefill_8k_tok_s, 0), h.accuracy.map_or_else(|| "-".to_string(), |a| format!("{:.1}%", a * 100.0))), style)], w));
        }
        lines
    };

    if app.compare_open {
        // the comparison: this loadout against the one picked in the list
        let sel = app.compare_sel.min(doc.loadouts.len().saturating_sub(1));
        let other = &doc.loadouts[sel];
        let mut lines = list(Some(sel));
        lines.extend(explain("* = serving now · > = compared with it · up/down pick another · esc closes · load = what else was on the server while the benchmark ran (quiet / LOADED / ? = not recorded)", w));
        out.push(lines_section("LOADOUTS", plain(format!("{} on record", doc.loadouts.len())), colour, lines, false));
        let cmp = lss_core::compare::compare("this", c, "that", other);
        let mut rows = vec![Line::styled(format!("{:<40} {:>10} {:>10}   {}", "", "this", "that", "this vs that"), dim())];
        let mut last = String::new();
        for row in &cmp.rows {
            if row.category != last {
                last = row.category.clone();
                let title = lss_core::compare::CATEGORIES.iter().find(|(k, _)| *k == last).map_or("", |(_, t)| t);
                rows.push(Line::styled(title.to_uppercase(), bold().fg(colour)));
            }
            let unit = if row.unit.is_empty() { String::new() } else { format!(" ({})", row.unit) };
            rows.push(fit_line(vec![Span::raw(format!("  {:<38} {:>10} {:>10}  ", format!("{}{unit}", row.label), opt1(row.a), opt1(row.b))), delta_span(row.mark, row.delta_pct), Span::styled(if row.source.is_empty() { String::new() } else { format!("  [{}]", row.source) }, dim())], w));
        }
        // the verdict is the answer, so it comes first; the numbers behind it follow (and scroll)
        let mut verdicts: Vec<Line<'static>> = Vec::new();
        // card #14: the reason the benchmark deltas are withheld goes ABOVE the verdicts, because
        // it changes how every one of them must be read
        if let Some(why) = &cmp.load_warning {
            verdicts.push(fit_line(vec![Span::styled("NOT COMPARABLE    ", warn().add_modifier(Modifier::BOLD)), Span::styled(why.clone(), warn())], w));
        }
        verdicts.extend(cmp.verdicts.iter().map(|v| fit_line(vec![Span::styled(format!("{:<18}", v.title), bold()), Span::raw(v.sentence.clone())], w)));
        out.push(lines_section("VERDICT", vec![], colour, verdicts, false));
        out.push(lines_section(&format!("THIS ({}) vs THAT ({})", r.model, other.row.model), plain("within +-3% = run-to-run noise".into()), colour, rows, false));
        return;
    }

    let mut head = vec![
        fit_line(vec![Span::styled(r.model.clone(), bold()), Span::raw(format!(" · {} · loadout {} · first seen {} · {} run(s)", r.image_tag, r.id, fmt_local(r.first_seen, "%Y-%m-%d %H:%M"), r.runs)), if r.current { Span::raw("") } else { Span::styled("  NOT serving now", warn()) }], w),
        Line::styled(fit(&r.flags, w), dim()),
    ];
    head.extend(explain("A loadout is a model plus the image and the launch settings it runs with. Change any of them and lss starts a new scorecard, so two can be compared.", w));
    out.push(lines_section("MODEL", vec![], colour, head, false));

    // BENCH box: the last run, its age, and the key
    let brief = d.bench.as_ref().map(|b| &b.brief);
    let mut bench_lines = Vec::new();
    match brief {
        Some(b) if b.state == "running" => bench_lines.push(fit_line(vec![Span::styled("RUNNING ", warn().add_modifier(Modifier::BOLD)), Span::raw(format!("{} · step {}/{} {} · started {}", b.profile.clone().unwrap_or_default(), b.step_index, b.steps, b.step.clone().unwrap_or_default(), ago(b.started_at.unwrap_or(0), now)))], w)),
        Some(b) if !b.configured => bench_lines.push(Line::styled(fit("not set up: point `[bench] harness` in the collector's config at llm_decode_bench.py", w), dim())),
        Some(b) => match &b.last {
            Some(l) => {
                // card #338: a completed run with a failed check says so, in warn colour
                let ok = l.clean();
                // card #14: the load label sits next to the status, in warn colour unless it is
                // the gold standard - a loaded run must never look like a quiet one at a glance
                let quiet = l.load == lss_core::bench::LoadClass::Quiet;
                bench_lines.push(fit_line(vec![Span::raw(format!("last run: {} on {} · {} · ", l.profile, l.model, ago(l.ended_at, now))), Span::styled(l.verdict(), if ok { good() } else { warn() }), Span::raw(" · "), Span::styled(l.load.word().to_string(), if quiet { dim() } else { warn() }), Span::raw(l.aborted.as_ref().map(|a| format!(" ({a})")).unwrap_or_default())], w));
            }
            None => bench_lines.push(Line::styled(fit("never run on this server", w), dim())),
        },
        None => bench_lines.push(Line::styled(fit("the collector did not answer /bench", w), dim())),
    }
    if let Some(m) = &app.bench_message {
        bench_lines.push(fit_line(vec![Span::styled(m.clone(), bold())], w));
    }
    bench_lines.push(fit_line(vec![Span::styled("b", bold()), Span::styled(" benchmarks this model (quick, ~5 min). It only starts when the server is idle and stops by itself if anyone uses it.", dim())], w));
    if brief.is_some_and(|b| b.builtin) {
        bench_lines.push(Line::styled(fit("No external harness is set up: lss measures it itself (1 / 2 / 4 / 8 at once, two prompt sizes, sanity checks).", w), dim()));
    }
    let dates = format!("quick {} · full {} · accuracy {}", c.last_quick_at.map_or_else(|| "never".into(), |t| ago(t, now)), c.last_full_at.map_or_else(|| "never".into(), |t| ago(t, now)), c.last_accuracy_at.map_or_else(|| "never".into(), |t| ago(t, now)));
    out.push(lines_section("BENCH", plain(dates), colour, bench_lines, false));

    // HEADROOM: the one sentence the owner asks for, then the reasoning behind it
    let mut room: Vec<Line<'static>> = Vec::new();
    match &c.headroom {
        Some(h) => {
            room.extend(super::widgets::wrap(&h.sentence, w.saturating_sub(3)).into_iter().enumerate().map(|(i, part)| Line::from(vec![Span::styled(if i == 0 { "=> " } else { "   " }, bold()), Span::styled(part, bold())])));
            for ceiling in &h.ceilings {
                room.extend(super::widgets::wrap(&ceiling.why, w.saturating_sub(10)).into_iter().enumerate().map(|(i, part)| Line::from(vec![Span::styled(format!("{:<10}", if i == 0 { ceiling.kind.as_str() } else { "" }), dim()), Span::raw(part)])));
            }
        }
        None => room.push(Line::styled(fit("not enough traffic seen yet to estimate", w), dim())),
    }
    out.push(lines_section("HEADROOM", plain("how many more people it can take".into()), colour, room, false));

    // TARGETS: the service the owner asked for, and how often it was met
    if let Some(t) = app.status.as_ref().map(|st| &st.targets).filter(|t| !t.rows.is_empty()) {
        let rows: Vec<Line<'static>> = crate::report::target_lines(t).into_iter().map(|(text, missed)| if missed { fit_line(vec![Span::raw(text), Span::styled("  MISSED", warn().add_modifier(Modifier::BOLD))], w) } else { Line::raw(fit(&text, w)) }).collect();
        let mut rows = rows;
        if let Some(note) = t.rows.iter().find_map(lss_core::targets::first_word_note) {
            rows.extend(explain(note, w));
        }
        let missed = t.missed().len();
        out.push(lines_section("TARGETS", if missed > 0 { vec![Span::styled(format!("{missed} missed · last {}", t.window), warn())] } else { plain(format!("all met · last {}", t.window)) }, colour, rows, false));
    }

    // writing speed
    let curve = crate::report::merged_curve(c);
    let mut lines = vec![
        kv("one user, server idle", format!("{} tok/s   (best {}, {} probes)", opt1(r.c1_tok_s), opt1(r.c1_best_tok_s), r.c1_probes), w),
        kv("real requests", format!("half at {} tok/s or faster · 9 in 10 at {}", opt1(r.decode_p50_tok_s), opt1(r.decode_p90_tok_s)), w),
        Line::styled(format!("{:<16} {:>16} {:>14} {:>18} {:>9}  {}", "users at once", "speed each gets", "total tok/s", "time to first word", "samples", "from"), dim()),
    ];
    let top_total = curve.iter().filter_map(|x| x.tok_s).fold(0.0, f64::max);
    for row in &curve {
        let from_bench = row.source == "bench";
        lines.push(fit_line(vec![Span::raw(format!("{:<16} {:>16} {:>14} {:>18} {:>9}  ", row.running, opt1(row.per_request_tok_s), opt1(row.tok_s), crate::report::ms(row.ttft_ms), row.samples)), Span::styled(format!("{:<6}", row.source), if from_bench { bold().fg(colour) } else { dim() }), Span::styled(hbar(row.tok_s.unwrap_or(0.0), top_total, 14.min(w.saturating_sub(88))), fg(colour))], w));
    }
    lines.push(fit_line(vec![Span::styled("=> ", bold()), Span::styled(crate::report::saturation_sentence(c), bold())], w));
    lines.push(kv("long conversations", format!("one user: {}", crate::report::long_context_text(c)), w));
    lines.extend(explain("Why three speeds for one user: alone = lss's own short test question on an idle server; 1 at once = real requests while they are written, which the guessing shortcut speeds up more on real text; \"real requests\" is timed inside the engine between tokens.", w));
    lines.extend(explain("Decode is how fast the answer is written once it has started. live = measured from real traffic, grouped by how many requests were running; bench = measured by lss bench. \"real requests\" is timed inside the engine between tokens (no network, no waiting), so it reads higher than what one user sees.", w));
    // card #14: every `bench` row above came from one scorecard - say under what conditions it
    // was taken, right where the rows are read
    if let Some(b) = c.speed() {
        lines.push(fit_line(vec![Span::styled(format!("{:<26}", "bench rows measured"), dim()), Span::styled(b.load_sentence(), if b.load_class() == lss_core::bench::LoadClass::Quiet { dim() } else { warn() })], w));
    }
    out.push(lines_section("WRITING SPEED (decode)", plain(format!("{} tok/s alone", opt1(c.headline().c1_tok_s))), colour, lines, false));

    // reading speed
    let mut lines = vec![kv("prompt tokens per second", format!("{} · peak {}", crate::report::opt(r.prefill_tok_s, 0), crate::report::opt(r.prefill_peak_tok_s, 0)), w), kv("time to first word", format!("half under {} · 9 in 10 under {}", crate::report::ms(r.ttft_p50_ms), crate::report::ms(r.ttft_p90_ms)), w)];
    if let Some(b) = c.speed() {
        for p in &b.prefill {
            lines.push(kv(&format!("bench: {}-token prompt", big(p.tokens as f64)), format!("{:.0} tok/s · first word after {}", p.tok_s, crate::report::ms(Some(p.ttft_ms))), w));
        }
    }
    lines.extend(explain("\"peak\" is the busiest one-minute window, not the whole life of the loadout.", w));
    let sized: Vec<String> = r.ttft_by_size.iter().map(|x| format!("{} {}", x.bucket, if x.n > 0 { crate::report::ms(x.avg_ms) } else { "-".into() })).collect();
    let any_sized = r.ttft_by_size.iter().any(|x| x.n > 0);
    lines.push(fit_line(vec![Span::styled(format!("{:<26}", "first word by prompt size"), dim()), if any_sized { Span::raw(sized.join("  ·  ")) } else { Span::styled("needs a v5.2 gateway (it logs the prompt size of every request)", dim()) }], w));
    lines.extend(explain("Prefill is the model reading the prompt before it writes the first word. Long prompts wait longer; cached text is not read again.", w));
    out.push(lines_section("READING SPEED (prefill)", vec![], colour, lines, false));

    let hw = half_width(body);
    // shortcuts
    let mut lines = vec![kv("speculative decoding", format!("accepts {} tokens per step ({} of its guesses)", crate::report::opt(r.spec_accept_length, 2), r.spec_accept_rate.map_or_else(|| "-".to_string(), f_pct01)), hw)];
    lines.extend(explain("A small helper guesses several tokens ahead and the model checks them in one go. Above 1 is free speed; read from the server, never from a client.", hw));
    lines.push(kv("prefix cache", format!("{} of prompt tokens came from cache", r.cache_hit_share.map_or_else(|| "-".to_string(), crate::report::share_pct)), hw));
    lines.extend(explain("Text it has already read (a system prompt, earlier turns) is not read again.", hw));
    out.push(lines_section("SHORTCUTS WORKING?", vec![], colour, lines, true));

    let mut lines = vec![kv("capacity", format!("{} tokens", big(r.kv_capacity_tokens)), hw), kv("peak use", format!("{} {}", gauge(r.kv_peak, 8), f_pct01(r.kv_peak)), hw), kv("context per request", format!("{} tokens", big(r.context_len)), hw), kv("longest real prompt", format!("{}{}", r.prompt_max_tokens.map_or_else(|| "-".to_string(), big), if r.prompt_max_is_bucket_edge { " (at most)" } else { "" }), hw)];
    lines.extend(explain("The KV cache is the model's working memory for the conversations in progress.", hw));
    out.push(lines_section("MEMORY", vec![], colour, lines, true));

    let mut lines = vec![kv("tokens per joule", crate::report::opt(r.tokens_per_joule, 3), hw), kv("energy per 1M tokens", format!("{} Wh", crate::report::opt(r.wh_per_mtok, 0)), hw), kv("average while serving", format!("{} W", crate::report::opt(r.avg_watts_serving, 0)), hw)];
    lines.push(kv("energy per day", format!("{} kWh", crate::report::opt(r.kwh_per_day, 2)), hw));
    match crate::report::cost_text(r) {
        Some(cost) => lines.push(kv("cost", cost, hw)),
        // #105: also None whenever a real [rates] table is configured (that engine owns cost
        // now - see page 1 ELECTRICITY) - the message covers both causes honestly, never implies
        // the flat setting is the only or current way to see a cost figure.
        None => lines.push(Line::styled(fit("cost: not shown here - set electricity_usd_per_kwh, or see page 1 ELECTRICITY if [rates] is configured", hw), dim())),
    }
    lines.extend(explain("GPU power, integrated while at least one request was running.", hw));
    out.push(lines_section("EFFICIENCY", vec![], colour, lines, true));

    let share = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |x| format!("{:.2}%", x * 100.0));
    let bad = r.error_rate.is_some_and(|e| e >= 0.01);
    let lines = vec![
        kv("up", format!("{}% of {:.1} h observed", crate::report::opt(r.uptime_pct, 2), r.hours_observed), hw),
        kv("restarts", r.runs.saturating_sub(1).to_string(), hw),
        fit_line(vec![Span::styled(format!("{:<26}", "errors (5xx)"), dim()), Span::styled(share(r.error_rate), if bad { red() } else { bold() })], hw),
        kv("turned away", format!("too busy (429) {} · unavailable (503) {}", share(r.rate_429), share(r.rate_503)), hw),
        kv("cold start", crate::report::cold_start_text(r), hw),
    ];
    out.push(lines_section("RELIABILITY", vec![], colour, lines, true));

    // scorecard against the previous and the best loadout
    let previous = lss_core::compare::resolve("previous", &doc.loadouts).ok().filter(|p| p.row.id != r.id);
    let best = lss_core::compare::resolve("best", &doc.loadouts).ok().filter(|b| b.row.id != r.id && Some(&b.row.id) != previous.map(|p| &p.row.id));
    if previous.is_some() || best.is_some() {
        let cp = previous.map(|p| lss_core::compare::compare("this", c, "previous", p));
        let cb = best.map(|b| lss_core::compare::compare("this", c, "best", b));
        let mut rows = vec![Line::styled(format!("{:<34} {:>9}   {:<26} {:<26}", "", "this", previous.map_or_else(String::new, |p| format!("vs previous ({:.14})", p.row.model)), best.map_or_else(String::new, |b| format!("vs best ({:.14})", b.row.model))), dim())];
        let base = cp.as_ref().or(cb.as_ref()).map(|x| x.rows.clone()).unwrap_or_default();
        for (i, row) in base.iter().enumerate() {
            let pick = |cmp: &Option<lss_core::compare::Comparison>| -> Span<'static> { cmp.as_ref().and_then(|x| x.rows.get(i)).filter(|x| x.label == row.label).map_or_else(|| Span::raw(format!("{:<10}", "")), |x| Span::raw(format!("{:>9} ", opt1(x.b)))) };
            let mark = |cmp: &Option<lss_core::compare::Comparison>| -> Span<'static> { cmp.as_ref().and_then(|x| x.rows.get(i)).filter(|x| x.label == row.label).map_or_else(|| Span::raw(format!("{:<16}", "")), |x| delta_span(x.mark, x.delta_pct)) };
            let unit = if row.unit.is_empty() { String::new() } else { format!(" ({})", row.unit) };
            rows.push(fit_line(vec![Span::raw(format!("{:<34} {:>9}   ", format!("{}{unit}", row.label), opt1(row.a))), pick(&cp), mark(&cp), Span::raw(" "), pick(&cb), mark(&cb)], w));
        }
        out.push(lines_section("SCORECARD", plain("+-3% = noise".into()), colour, rows, false));
    }
    let mut lines = list(None);
    lines.extend(explain("* = serving now · enter opens the side-by-side comparison", w));
    out.push(lines_section("LOADOUTS", plain(format!("{} on record · enter compares", doc.loadouts.len())), colour, lines, false));
}

/// The BENCH prompt over the MODEL page.
pub fn bench_prompt_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(" Benchmark this model now?", bold()), Line::raw("")];
    lines.push(Line::raw(" quick: ~5 min. Warm-up, writing speed at 1, 2, 4 and 8"));
    lines.push(Line::raw(" users, reading speed at 8k and 64k, three sanity checks."));
    lines.push(Line::raw(""));
    lines.push(Line::styled(" It only starts when the server has been idle, and it", dim()));
    lines.push(Line::styled(" stops by itself the moment anyone else uses the server.", dim()));
    lines.push(Line::raw(""));
    if app.bench_local {
        lines.push(Line::from(vec![Span::styled(" y", bold()), Span::raw(" start it    "), Span::styled("esc", bold()), Span::raw(" not now")]));
    } else {
        lines.push(Line::raw(" It runs on the GPU box. From a shell:"));
        lines.push(Line::styled(format!("   {}", app.bench_command), bold()));
        lines.push(Line::from(vec![Span::styled(" esc", bold()), Span::raw(" closes")]));
    }
    lines
}

// ---------------------------------------------------------------- a ADVICE

fn advice(out: &mut Vec<Section>, d: &PageData, body: Rect) {
    let colour = page_colour(PageId::Advice);
    let w = (body.width as usize).saturating_sub(2);
    let Some(doc) = &d.advice else { return };
    if doc.windows.is_empty() {
        out.push(message("ADVICE", colour, "Nothing yet: the collector works the advice out every 5 minutes once it has data.".into(), false, body.width));
        return;
    }
    let intro = explain("Evidence for settings and upgrade decisions. ACT = it is costing you now · WATCH = look again · FINE = measured, nothing to do. lss never changes a setting: these are reasons, the decision is yours. Every rule and threshold: docs/ADVICE.md.", w);
    out.push(lines_section("ADVICE", plain(format!("{} windows", doc.windows.len())), colour, intro, false));
    // the week first: it is the headline
    let order = ["7d", "24h", "30d"];
    for name in order {
        let Some(win) = doc.windows.iter().find(|x| x.stats.name == name) else { continue };
        let title = match name {
            "7d" => "THE WEEK",
            "24h" => "THE LAST 24 HOURS",
            _ => "THE LAST 30 DAYS",
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        for f in &win.findings {
            let tag = format!("{:<5} ", f.severity.as_str().to_uppercase());
            for (i, part) in super::widgets::wrap(&f.sentence, w.saturating_sub(tag.len()).max(10)).into_iter().enumerate() {
                let lead = if i == 0 { Span::styled(tag.clone(), super::overview::severity_colour(f.severity)) } else { Span::raw(" ".repeat(tag.len())) };
                lines.push(fit_line(vec![lead, Span::raw(part)], w));
            }
            if !f.points_at.is_empty() {
                lines.push(fit_line(vec![Span::raw(" ".repeat(tag.len())), Span::styled(format!("points at: {}", f.points_at), dim())], w));
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled("not enough data in this window to say anything yet", dim()));
        }
        let excluded: Vec<String> = win.stats.throttle.iter().filter(|g| g.excluded).map(|g| format!("GPU{} {:.1}%", g.index, g.pct)).collect();
        if !excluded.is_empty() {
            lines.push(Line::styled(fit(&format!("throttle time of GPUs left out of the advice on purpose: {}", excluded.join(", ")), w), dim()));
        }
        let acts = win.findings.iter().filter(|f| f.severity == lss_core::advice::Severity::Act).count();
        let watches = win.findings.iter().filter(|f| f.severity == lss_core::advice::Severity::Watch).count();
        let detail = vec![Span::raw(format!("{} of data · ", fmt_duration(win.stats.covered_secs))), if acts > 0 { Span::styled(format!("{acts} act"), red()) } else { Span::raw("0 act") }, Span::raw(format!(" · {watches} watch"))];
        out.push(lines_section(title, detail, colour, lines, false));
    }
}
