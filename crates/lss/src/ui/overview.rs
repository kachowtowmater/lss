//! The OVERVIEW: SERVE (blue), GPUS (green), LANES (yellow), USERS (light blue), ADVICE (light
//! cyan), INCIDENTS (magenta) and ALERTS (red when something fires, grey otherwise), laid out
//! for the pane it is given. Nothing is
//! ever silently dropped: an area that cannot be a box becomes a 1-line bar (`GPUS … tab >`)
//! that still takes focus and opens its page.

use super::widgets::{bar_line, bold, dim, draw_box, fg, fit, fit_line, frame, gauge, good, red, render_lines, sparkline, sparkline_to, warn};
use super::{draw_header, footer, pick_shape, App, Panel, Shape};
use crate::plain::{gib, num, opt_ms, pct, top_keys};
use lss_core::advice::Severity;
use lss_core::model::{GpuStatus, LaneStatus, Status};
use lss_core::timeutil::{fmt_duration, fmt_local};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub fn panel_colour(p: Panel, s: &Status) -> Color {
    match p {
        Panel::Serve => Color::Blue,
        Panel::Gpus => Color::Green,
        Panel::Lanes => Color::Yellow,
        Panel::Users => Color::LightBlue,
        Panel::Advice => Color::LightCyan,
        Panel::Incidents => Color::Magenta,
        Panel::Alerts if !s.firing.is_empty() => Color::Red,
        Panel::Alerts => Color::DarkGray,
    }
}

/// How an area is shown this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Boxed(Rect),
    Bar(Rect),
    /// ONE bar for this area and every area after it in `MINIMIZED_BARS` (the pane has no row
    /// left for a bar each): nothing is silently dropped
    More(Rect, usize),
}

struct Ctx<'a> {
    app: &'a App,
    s: &'a Status,
    now: i64,
    /// one card style per render: when any card has to carry its first line in its border,
    /// they all do
    dense: bool,
    /// ALERTS has a slot of its own this frame (else the INCIDENTS bar carries it)
    alerts_drawn: bool,
    /// text rows inside the SERVE box: under four, the C1 reading moves into its title
    serve_rows: u16,
    /// the GPU cards are too narrow for `52C/126F`: they show `52/126` and the title says `C/F`
    gpu_bare: bool,
    /// the minimized overview: SERVE is drawn as the one status card, and there is no header row
    minimized: bool,
}

/// The bars under the minimized status card, most useful first (the card already carries the
/// hottest GPU and the alerts).
const MINIMIZED_BARS: [Panel; 6] = [Panel::Users, Panel::Gpus, Panel::Lanes, Panel::Advice, Panel::Incidents, Panel::Alerts];

pub fn draw(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) {
    let shape = app.pinned.unwrap_or_else(|| pick_shape(area.width, area.height));
    if area.height < 3 {
        // not even one box fits: the header is the screen
        return render_lines(f, area, vec![super::header(app, s, area.width, now)]);
    }
    let minimized = shape == Shape::Focus;
    // minimized: the status card IS the header (state, model and host are in its border)
    let mut body = if minimized { super::draw_warning(f, app, s, area, now) } else { draw_header(f, app, s, area, now) };
    // one-line strips under the header: the FLEET (several servers) and the owner's TARGETS.
    // They take a row each, so the smallest panes keep their boxes instead (a missed target is
    // in the header there, and the server switcher always is).
    let strips: Vec<Line<'static>> = [super::fleet_line(app, area.width).filter(|_| area.height >= 12 && !minimized), super::targets_line(s, area.width).filter(|_| area.height >= 25 || (area.height >= 20 && matches!(shape, Shape::ThirdH | Shape::ThirdV | Shape::HalfV | Shape::HalfH)))].into_iter().flatten().collect();
    for line in strips {
        if body.height < 6 {
            break;
        }
        f.render_widget(Paragraph::new(line), Rect { height: 1, ..body });
        body = Rect { y: body.y + 1, height: body.height - 1, ..body };
    }
    if body.height < 2 {
        return;
    }
    let foot = Rect { y: body.y + body.height - 1, height: 1, ..body };
    f.render_widget(Paragraph::new(footer(app, area.width)), foot);
    let body = Rect { height: body.height - 1, ..body };
    // measuring comes before drawing: the plan asks every area how many rows it has to say
    let measure = Ctx { app, s, now, dense: false, alerts_drawn: true, serve_rows: 99, gpu_bare: false, minimized };
    let mut slots = plan(shape, body, &measure);
    if s.gpu_source == "none" {
        hide_panel(&mut slots, Panel::Gpus);
    }
    let n = s.gpus.len();
    let card_rects: Vec<(Panel, Rect)> = slots.iter().filter_map(|(p, slot)| if let Slot::Boxed(r) = slot { Some((*p, *r)) } else { None }).collect();
    let dense = card_rects.iter().any(|(p, r)| match p {
        Panel::Gpus => !gpu_rows_mode(inner_of(*r), n) && gpu_grid(inner_of(*r), n, false).is_none_or(|g| g.dense),
        Panel::Lanes => !lane_lines_mode(inner_of(*r)) && lane_grid(inner_of(*r), false).is_none_or(|g| g.dense),
        _ => false,
    });
    let alerts_drawn = slots.iter().any(|(p, slot)| *p == Panel::Alerts && !matches!(slot, Slot::More(..)));
    let serve_rows = slots.iter().find_map(|(p, slot)| match (p, slot) {
        (Panel::Serve, Slot::Boxed(r)) => Some(r.height.saturating_sub(2)),
        _ => None,
    });
    let gpu_bare = card_rects.iter().any(|(p, r)| *p == Panel::Gpus && !gpu_rows_mode(inner_of(*r), n) && gpu_grid(inner_of(*r), n, dense).is_some_and(|g| g.w.saturating_sub(2) < 8));
    let cx = Ctx { app, s, now, dense, alerts_drawn, serve_rows: serve_rows.unwrap_or(0), gpu_bare, minimized };
    for (panel, slot) in &slots {
        match slot {
            Slot::Boxed(r) => {
                app.rects.borrow_mut().push((*panel, *r));
                draw_panel(f, &cx, *panel, *r);
            }
            Slot::Bar(r) => {
                app.rects.borrow_mut().push((*panel, *r));
                f.render_widget(Paragraph::new(bar(&cx, *panel, r.width as usize)), *r);
            }
            Slot::More(r, from) => {
                app.rects.borrow_mut().push((*panel, *r));
                f.render_widget(Paragraph::new(more_bar(&cx, &MINIMIZED_BARS[*from..], r.width as usize)), *r);
            }
        }
    }
}

/// Take an area off the screen and give its room to the neighbour that shares its whole edge
/// (the box to its left, else the one under it, else the one above it). Bars close up.
fn hide_panel(slots: &mut Vec<(Panel, Slot)>, gone: Panel) {
    let Some(at) = slots.iter().position(|(p, _)| *p == gone) else { return };
    let (_, slot) = slots.remove(at);
    match slot {
        Slot::Boxed(r) => {
            let boxes = slots.iter_mut().filter_map(|(_, s)| if let Slot::Boxed(b) = s { Some(b) } else { None });
            let mut left = None;
            let mut under = None;
            let mut above = None;
            for b in boxes {
                if b.y == r.y && b.height == r.height && b.x + b.width == r.x {
                    left = Some(b);
                } else if b.x == r.x && b.width == r.width && b.y == r.y + r.height {
                    under = Some(b);
                } else if b.x == r.x && b.width == r.width && b.y + b.height == r.y {
                    above = Some(b);
                }
            }
            if let Some(b) = left {
                b.width += r.width;
            } else if let Some(b) = under {
                b.y = r.y;
                b.height += r.height;
            } else if let Some(b) = above {
                b.height += r.height;
            }
        }
        Slot::Bar(r) | Slot::More(r, _) => {
            // the bars above it move down one row, and the box(es) over them grow by that row
            let mut top = r.y;
            for (_, s) in slots.iter_mut() {
                if let Slot::Bar(b) | Slot::More(b, _) = s {
                    if b.y < r.y && b.x < r.x + r.width && r.x < b.x + b.width {
                        top = top.min(b.y);
                        b.y += 1;
                    }
                }
            }
            for (_, s) in slots.iter_mut() {
                if let Slot::Boxed(b) = s {
                    if b.y + b.height == top && b.x < r.x + r.width && r.x < b.x + b.width {
                        b.height += 1;
                    }
                }
            }
        }
    }
}

fn inner_of(r: Rect) -> Rect {
    Rect { x: r.x + 1, y: r.y + 1, width: r.width.saturating_sub(2), height: r.height.saturating_sub(2) }
}

/// GPUS as one compact row per GPU (`GPU0* 52C/126F ▁▂▃ 19W 0%`) instead of cards: when the
/// cards would be too narrow to say anything, and every GPU gets its row.
fn gpu_rows_mode(inner: Rect, n: usize) -> bool {
    n > 0 && inner.width >= 22 && inner.width / (n as u16) < 14 && inner.height as usize >= n
}

/// LANES as plain lines instead of two cards: under this width the cards clip their own titles.
fn lane_lines_mode(inner: Rect) -> bool {
    inner.width < 56 && inner.height >= 4
}

/// (fewest, all) rows of text an area has to say at this inner width. `all` is exact, so a
/// box is as tall as its content and never carries blank rows.
fn rows_wanted(cx: &Ctx, p: Panel, iw: u16) -> (u16, u16) {
    let s = cx.s;
    let tall = |h: u16| Rect { x: 0, y: 0, width: iw, height: h };
    match p {
        Panel::Serve => (2, serve_lines(cx, iw as usize, 99).len() as u16),
        Panel::Gpus => {
            let n = s.gpus.len().max(1) as u16;
            if gpu_rows_mode(tall(99), n as usize) {
                (n, n + 1)
            } else {
                // cards in one row when they fit at >= 7 columns each, else two rows; a full
                // card is 7 rows (name, temperature, power, memory, throttle), plus the totals
                let per_row = if iw / n >= 7 { n } else { n.div_ceil(2) };
                let rows = n.div_ceil(per_row.max(1));
                (3 * rows, 7 * rows + 1)
            }
        }
        Panel::Lanes if s.gate.absent => (1, 2 + u16::from(iw < 64) + u16::from(iw < 40)),
        Panel::Lanes => {
            if lane_lines_mode(tall(99)) {
                (3, 6)
            } else {
                (3, if iw >= 54 { 11 } else { 9 })
            }
        }
        Panel::Users => (2, users_lines(cx, tall(99)).len() as u16),
        Panel::Advice => (1, advice_lines(cx, tall(99)).len() as u16),
        Panel::Incidents => (1, incident_lines(cx, tall(8)).len() as u16),
        Panel::Alerts => (1, alert_lines(cx, tall(6 + u16::from(!s.firing.is_empty()))).len() as u16),
    }
}

/// Stack `panels` top to bottom in `area`: everything starts as a 1-line bar, and areas are
/// promoted to boxes in order while their fewest rows fit; what is left over grows the boxes to
/// exactly their content. Rows still left go to the areas in `stretch` (area, most rows), which
/// turn them into a last-hour chart, then to the longer lists, then to the first of `stretch`:
/// never to blank space.
fn stack(area: Rect, panels: &[Panel], cx: &Ctx, stretch: &[(Panel, u16)]) -> Vec<(Panel, Slot)> {
    let iw = area.width.saturating_sub(2);
    let sizes: Vec<(u16, u16)> = panels.iter().map(|p| rows_wanted(cx, *p, iw)).map(|(min, all)| (min + 2, all.max(min) + 2)).collect();
    let mut h: Vec<u16> = vec![1; panels.len()];
    let mut boxed = vec![false; panels.len()];
    let mut used: u16 = panels.len() as u16;
    for (i, (fewest, _)) in sizes.iter().enumerate() {
        if used - 1 + fewest <= area.height {
            used = used - 1 + fewest;
            h[i] = *fewest;
            boxed[i] = true;
        }
    }
    let mut spare = area.height.saturating_sub(used);
    // one row at a time, round the boxes, until each has what it wants
    let mut progress = true;
    while spare > 0 && progress {
        progress = false;
        for i in 0..panels.len() {
            if boxed[i] && spare > 0 && h[i] < sizes[i].1 {
                h[i] += 1;
                spare -= 1;
                progress = true;
            }
        }
    }
    let at = |p: Panel| panels.iter().position(|x| *x == p).filter(|i| boxed[*i]);
    for (p, most) in stretch {
        if let Some(i) = at(*p) {
            let take = spare.min(*most);
            h[i] += take;
            spare -= take;
        }
    }
    // lists longer than their usual cut
    for (p, len, cut) in [(Panel::Incidents, cx.s.incidents.len(), 8usize), (Panel::Alerts, cx.s.alerts.len(), 6)] {
        if let Some(i) = at(p) {
            let take = spare.min(len.saturating_sub(cut) as u16);
            h[i] += take;
            spare -= take;
        }
    }
    if spare > 0 {
        if let Some(i) = stretch.iter().find_map(|(p, _)| at(*p)).or_else(|| boxed.iter().rposition(|b| *b)) {
            h[i] += spare;
        }
    }
    let mut y = area.y;
    let mut out = Vec::new();
    for (i, p) in panels.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let r = Rect { x: area.x, y, width: area.width, height: h[i].min(area.y + area.height - y) };
        out.push((*p, if boxed[i] { Slot::Boxed(r) } else { Slot::Bar(r) }));
        y += r.height;
    }
    out
}

fn split_h(area: Rect, parts: u16) -> Vec<Rect> {
    let w = area.width / parts;
    (0..parts).map(|i| Rect { x: area.x + i * w, width: if i == parts - 1 { area.width - w * (parts - 1) } else { w }, ..area }).collect()
}

fn plan(shape: Shape, body: Rect, cx: &Ctx) -> Vec<(Panel, Slot)> {
    let s = cx.s;
    match shape {
        Shape::HalfH if body.height >= 24 && body.width >= 60 => {
            // three rows of two boxes + the ALERTS strip:
            //   SERVE | GPUS  /  LANES | USERS  /  ADVICE | INCIDENTS  /  ALERTS
            let alerts_h = (3 + s.alerts.len().clamp(1, 4) as u16).min(body.height / 5).max(3);
            let grid_h = body.height - alerts_h;
            let low_h = (grid_h * 3 / 10).clamp(5, 10);
            let top_h = ((grid_h - low_h) / 2).clamp(8, 15);
            let mid_h = grid_h - low_h - top_h;
            let top = split_h(Rect { height: top_h, ..body }, 2);
            let mid = split_h(Rect { y: body.y + top_h, height: mid_h, ..body }, 2);
            let low = split_h(Rect { y: body.y + top_h + mid_h, height: low_h, ..body }, 2);
            let strip = Rect { y: body.y + grid_h, height: alerts_h, ..body };
            vec![
                (Panel::Serve, Slot::Boxed(top[0])),
                (Panel::Gpus, Slot::Boxed(top[1])),
                (Panel::Lanes, Slot::Boxed(mid[0])),
                (Panel::Users, Slot::Boxed(mid[1])),
                (Panel::Advice, Slot::Boxed(low[0])),
                (Panel::Incidents, Slot::Boxed(low[1])),
                (Panel::Alerts, Slot::Boxed(strip)),
            ]
        }
        Shape::ThirdH if body.height >= 12 && body.width >= 90 => {
            // wide and short: three columns that use the full height,
            //   SERVE | GPUS over ADVICE | LANES over USERS
            // and INCIDENTS | ALERTS as the row under them: bars, or two short boxes when the
            // pane has the rows. Each column gives its spare rows to a last-hour chart.
            let half_iw = (body.width / 2).saturating_sub(2);
            let lists = rows_wanted(cx, Panel::Incidents, half_iw).1.max(rows_wanted(cx, Panel::Alerts, half_iw).1);
            let bottom_h: u16 = if body.height >= 22 { (lists + 2).min(body.height - 17).max(3) } else if body.width >= 120 { 1 } else { 2 };
            let cols = split_h(Rect { height: body.height - bottom_h, ..body }, 3);
            let bottom = Rect { y: body.y + body.height - bottom_h, height: bottom_h, ..body };
            let mut out = vec![(Panel::Serve, Slot::Boxed(cols[0]))];
            out.extend(stack(cols[1], &[Panel::Gpus, Panel::Advice], cx, &[(Panel::Gpus, u16::MAX)]));
            out.extend(stack(cols[2], &[Panel::Lanes, Panel::Users], cx, &[(Panel::Lanes, u16::MAX)]));
            if bottom_h == 2 {
                out.push((Panel::Incidents, Slot::Bar(Rect { height: 1, ..bottom })));
                out.push((Panel::Alerts, Slot::Bar(Rect { y: bottom.y + 1, height: 1, ..bottom })));
            } else {
                let halves = split_h(bottom, 2);
                let slot = |r: Rect| if bottom_h >= 3 { Slot::Boxed(r) } else { Slot::Bar(r) };
                out.push((Panel::Incidents, slot(halves[0])));
                out.push((Panel::Alerts, slot(halves[1])));
            }
            out
        }
        // narrow and tall: every box as tall as its content, the rest of the pane as charts
        Shape::HalfV | Shape::ThirdV if body.height >= 12 => stack(body, &Panel::ALL, cx, &[(Panel::Serve, 8), (Panel::Gpus, 6), (Panel::Lanes, 4)]),
        Shape::Focus => focus_plan(body),
        // SMALL, and any pinned layout the pane is too small for
        _ if body.width >= 40 && body.height >= 7 => {
            // a row of two boxes, SERVE | GPUS, then the rest stacked under it
            let all = [Panel::Lanes, Panel::Users, Panel::Advice, Panel::Incidents, Panel::Alerts];
            let rest: &[Panel] = if body.height < 5 + all.len() as u16 { &all[..4] } else { &all };
            let row_min = 4;
            let row_want = (if body.height >= 26 { 10 } else { 8 }).min(body.height.saturating_sub(3)).max(row_min);
            // give the row what it wants only if the rest can still be bars
            let row_h = row_want.min(body.height.saturating_sub(rest.len() as u16)).max(row_min);
            let below = Rect { y: body.y + row_h, height: body.height.saturating_sub(row_h), ..body };
            let under = stack(below, rest, cx, &[(Panel::Lanes, 4)]);
            let mut row = split_h(Rect { height: row_h, ..body }, 2);
            // a temperature in both units needs 6 columns inside a GPU card (`52/126`): when the
            // even split leaves the cards a column or two short, GPUS takes them from SERVE
            let n = s.gpus.len().max(1) as u16;
            let need = n * 8 + 2;
            if row[1].width < need && need - row[1].width <= 3 && row[0].width > 24 {
                let take = need - row[1].width;
                row[0].width -= take;
                row[1].x -= take;
                row[1].width += take;
            }
            let mut out = vec![(Panel::Serve, Slot::Boxed(row[0])), (Panel::Gpus, Slot::Boxed(row[1]))];
            out.extend(under);
            out
        }
        _ => focus_plan(body),
    }
}

/// MINIMIZED: ONE status card (SERVE), everything else a bar under it. With a row for five
/// bars ALERTS rides in INCIDENTS; with fewer, the last row is ONE `MORE` bar for the areas
/// that have no row of their own, so every area is still on screen.
fn focus_plan(body: Rect) -> Vec<(Panel, Slot)> {
    let all = MINIMIZED_BARS.len() as u16;
    if body.height < 3 {
        return vec![(Panel::Serve, Slot::Bar(Rect { height: 1.min(body.height), ..body }))];
    }
    // the card wants three rows of text (5 with its border); a taller pane grows the card
    let card_h = if body.height >= 5 + all {
        body.height - all
    } else if body.height >= 6 {
        5
    } else {
        body.height
    };
    let rows = body.height - card_h;
    let mut out = vec![(Panel::Serve, Slot::Boxed(Rect { height: card_h, ..body }))];
    let bar_at = |i: u16| Rect { y: body.y + card_h + i, height: 1, ..body };
    // one bar each, except that five rows hold six areas (INCIDENTS carries ALERTS)
    let own = if rows >= all - 1 { rows.min(all) } else { rows.saturating_sub(1) };
    for (i, p) in MINIMIZED_BARS.iter().take(own as usize).enumerate() {
        out.push((*p, Slot::Bar(bar_at(i as u16))));
    }
    if rows > own {
        out.push((MINIMIZED_BARS[own as usize], Slot::More(bar_at(own), own as usize)));
    }
    out
}

/// `412`, `3.1k`: a chart's axis label.
pub(super) fn axis_number(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else {
        format!("{v:.0}")
    }
}

/// #69: a 0.0..1.0 fraction on a chart's axis, as a percent (`budget_used_frac`).
pub(super) fn axis_pct01(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

/// A temperature on a chart's axis, in the first configured unit (the caption has both).
pub(super) fn axis_temp(c: f64) -> String {
    if lss_core::units::temp_units() == lss_core::units::TempUnits::F {
        format!("{:.0}F", lss_core::units::c_to_f(c))
    } else {
        format!("{c:.0}C")
    }
}

/// The rows a box has left under its text become a chart of the last hour: a caption, then
/// blocks against an axis that names the top and the bottom of the scale, so no row of a box
/// is ever blank (one row: a sparkline behind a short caption).
fn fill_chart(f: &mut Frame, rect: Rect, (caption, short): (&str, &str), data: &[Option<f64>], (lo, hi): (f64, Option<f64>), axis: fn(f64) -> String, colour: Color) {
    if rect.height == 0 || rect.width < 8 {
        return;
    }
    let w = rect.width as usize;
    let seen = data.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
    let top = hi.unwrap_or(if seen.is_finite() { seen } else { lo + 1.0 }).max(lo + 1e-9);
    if rect.height == 1 {
        let lead = format!("{short} ");
        let room = w.saturating_sub(lead.chars().count());
        let line = super::widgets::block_chart(data, room, 1, lo, Some(top)).into_iter().next().unwrap_or_default();
        return render_lines(f, rect, vec![Line::from(vec![label(&lead), Span::styled(line, fg(colour))])]);
    }
    let rows = rect.height as usize - 1;
    let (top_label, low_label) = (axis(top), axis(lo));
    let gutter = top_label.chars().count().max(low_label.chars().count());
    let plot = super::widgets::block_chart(data, w.saturating_sub(gutter + 1), rows, lo, Some(top));
    let mut lines = vec![Line::styled(fit(caption, w), dim())];
    for (r, row) in plot.into_iter().enumerate() {
        let (text, tick) = if r == 0 { (top_label.as_str(), '┤') } else if r + 1 == rows { (low_label.as_str(), '┤') } else { ("", '│') };
        lines.push(Line::from(vec![Span::styled(format!("{text:>gutter$}{tick}"), dim()), Span::styled(row, fg(colour))]));
    }
    render_lines(f, rect, lines);
}

fn draw_panel(f: &mut Frame, cx: &Ctx, p: Panel, area: Rect) {
    if area.height < 3 || area.width < 6 {
        return f.render_widget(Paragraph::new(bar(cx, p, area.width as usize)), Rect { height: 1.min(area.height), ..area });
    }
    let focused = cx.app.focus == p;
    let colour = panel_colour(p, cx.s);
    if p == Panel::Serve && cx.minimized {
        return draw_status_card(f, cx, area);
    }
    let inner = draw_box(f, area, p.title(), detail(cx, p), colour, focused);
    let s = cx.s;
    let under = |used: u16| Rect { y: inner.y + used.min(inner.height), height: inner.height.saturating_sub(used), ..inner };
    match p {
        Panel::Serve => {
            let lines = serve_lines(cx, inner.width as usize, inner.height as usize);
            let used = lines.len() as u16;
            render_lines(f, inner, lines);
            // also while the serve is down: the last hour shows when it stopped writing
            fill_chart(f, under(used), ("writing tok/s · last hour", "write 1h"), &s.series.decode_tok_s, (0.0, None), axis_number, Color::Blue);
        }
        Panel::Gpus => {
            let bottom = draw_gpus(f, cx, inner);
            // the hottest GPU of every moment; full height = the temperature that alerts
            let n = s.series.gpu_temp_c.iter().map(Vec::len).max().unwrap_or(0);
            let hottest: Vec<Option<f64>> = (0..n).map(|i| s.series.gpu_temp_c.iter().filter_map(|g| g.get(i).copied().flatten()).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))).collect();
            // scaled to what the hour really spanned: the shape of the hour is the point here
            // (the rows above say how hot each GPU is now, and RED says when that is too hot)
            let (low, high) = hottest.iter().flatten().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
            let (low, high) = if low.is_finite() { ((low - 2.0).floor(), high.ceil().max(low + 1.0)) } else { (20.0, 90.0) };
            let long = format!("hottest GPU · last hour · peak {}", lss_core::units::temp_compact(high));
            let caption = [long.as_str(), "hottest GPU · last hour"].into_iter().find(|c| c.chars().count() <= inner.width as usize).unwrap_or("hottest GPU 1h");
            fill_chart(f, under(bottom.saturating_sub(inner.y)), (caption, "temp 1h"), &hottest, (low, Some(high)), axis_temp, Color::Green);
        }
        Panel::Lanes => {
            let bottom = draw_lanes(f, cx, inner);
            let rest = under(bottom.saturating_sub(inner.y));
            // #69 (the owner, 2026-09-22): "what the hell does this graph do in lanes ... i dont
            // even know what this tells me that is useful." He was right - the OLD chart here
            // graphed gateway QUEUE, which is flat at zero almost always because requests
            // essentially never queue at the gateway: they are either admitted straight through
            // or HELD on the trusted lane's in-flight token budget, a different quantity that
            // was not graphed at all. `budget_used_frac` is that quantity - it is what actually
            // varies and drives a decision (see the budget filling BEFORE the rejections start,
            // not just an instantaneous "88%" that is gone the moment you look away).
            let waiters: Vec<Option<f64>> = s.series.waiters_public.iter().zip(s.series.waiters_trusted.iter().chain(std::iter::repeat(&None))).map(|(a, b)| match (a, b) {
                (None, None) => None,
                _ => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
            }).collect();
            let has_budget = s.series.budget_used_frac.iter().any(Option::is_some);
            let budget_moved = s.series.budget_used_frac.iter().flatten().any(|v| *v > 0.0);
            if has_budget && budget_moved {
                // waiters get their own row when there is room for both to stay readable; a
                // short box gives the more informative of the two (the budget itself, per the
                // card's own emphasis) the whole box rather than squeezing both unreadable.
                let (budget_area, waiters_area) = if rest.height >= 6 {
                    let h = rest.height / 2;
                    (Rect { height: h, ..rest }, Some(Rect { y: rest.y + h, height: rest.height - h, ..rest }))
                } else {
                    (rest, None)
                };
                let cap = if inner.width >= 36 { "in-flight budget used · last hour" } else { "budget used · last hour" };
                fill_chart(f, budget_area, (cap, "budget 1h"), &s.series.budget_used_frac, (0.0, Some(1.0)), axis_pct01, Color::Yellow);
                if let Some(wa) = waiters_area {
                    fill_chart(f, wa, ("waiters · last hour", "waiters 1h"), &waiters, (0.0, None), axis_number, Color::Yellow);
                }
            } else if has_budget {
                // #69 item 2: a series that never moved all hour is worse DRAWN than SAID - a
                // flat line looks like information; a sentence says plainly that nothing did.
                render_lines(f, rest, vec![Line::styled("budget never touched this hour", dim())]);
            } else {
                // no gate, or a gate too old to publish `budget_tokens` (card #31): the same
                // fallback this box used before #69 existed - how busy the slots were.
                let slots = f64::from(s.serve.slots.max(1));
                fill_chart(f, rest, (if inner.width >= 36 { "no budget history · requests running, last hour" } else { "running · last hour" }, "running 1h"), &s.series.running, (0.0, Some(slots)), axis_number, Color::Yellow);
            }
        }
        Panel::Users => render_lines(f, inner, users_lines(cx, inner)),
        Panel::Advice => render_lines(f, inner, advice_lines(cx, inner)),
        Panel::Incidents => render_lines(f, inner, incident_lines(cx, inner)),
        Panel::Alerts => render_lines(f, inner, alert_lines(cx, inner)),
    }
}

/// The flags the header would carry, for the status card's border: what is wrong, loudest last.
fn status_flags(s: &Status) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    if s.targets.rows.iter().any(|r| r.ok == Some(false)) {
        v.push(Span::styled(" · TARGET MISSED", warn()));
    }
    if !s.gate.up {
        v.push(Span::styled(" · GATE DOWN", red()));
    }
    if !s.firing.is_empty() {
        v.push(Span::styled(format!(" · {} FIRING", s.firing.len()), red()));
    }
    v
}

/// MINIMIZED: the one card that answers "is it OK?" from a corner of the screen. The border
/// says UP / DOWN, the model and the host; inside are the two speeds, the slots and the queue,
/// the hottest GPU in both units and the alerts. A taller card adds the rest of SERVE.
fn draw_status_card(f: &mut Frame, cx: &Ctx, area: Rect) {
    let s = cx.s;
    let sv = &s.serve;
    let focused = cx.app.focus == Panel::Serve;
    let colour = if sv.up { Color::Blue } else { Color::Red };
    let host = if cx.app.multi_server() { cx.app.fleet[cx.app.server_idx.min(cx.app.fleet.len() - 1)].name.clone() } else { s.host.clone() };
    // the border: the state and for how long (`UP 36m54s`), then the model, then the host
    let state = crate::plain::serve_state(s, cx.now);
    let model = sv.model.clone().unwrap_or_else(|| "(no model)".into());
    let flags = status_flags(s);
    let room = (area.width as usize).saturating_sub(state.chars().count() + 9 + spans_width(&flags));
    let mut text = String::new();
    for part in [model, host] {
        let next = if text.is_empty() { part } else { format!("{text} · {part}") };
        if next.chars().count() > room {
            break;
        }
        text = next;
    }
    let mut detail = vec![Span::raw(text)];
    if detail[0].content.is_empty() {
        // no room for the model: the flags stand alone (` · ` is their separator after text)
        detail.clear();
        detail.extend(flags.into_iter().enumerate().map(|(i, f)| if i == 0 { Span::styled(f.content.trim_start_matches(" · ").to_string(), f.style) } else { f }));
    } else {
        detail.extend(flags);
    }
    let inner = draw_box(f, area, &state, detail, colour, focused);
    let (w, rows) = (inner.width as usize, inner.height as usize);
    let b = bold();
    let spark = fg(Color::Blue);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 1. the two speeds of the model, each with its last hour when there is room
    if sv.up {
        let (reading, typical) = match (sv.prefill_tok_s, sv.prefill_tok_s_typical) {
            (Some(v), _) => (format!("{v:.0}"), false),
            (None, Some(t)) => (format!("{t:.0}"), true),
            (None, None) => ("-".to_string(), false),
        };
        let read_top = s.series.prefill_tok_s.iter().flatten().copied().fold(100.0_f64, f64::max);
        let half = |lead: &str, value: String, note: &str, chart: String| vec![label(lead), Span::styled(value, b), label(note), Span::styled(chart, spark)];
        let build = |chart_w: usize, words: bool| -> Vec<Span<'static>> {
            let (wl, rl) = if words { ("writing ", "reading ") } else { ("write ", "read ") };
            let mut v = half(wl, if sv.is_na("decode_tok_s") { "n/a".to_string() } else { format!("{:.1} tok/s", sv.decode_tok_s) }, if chart_w > 0 { " " } else { "" }, sparkline(&s.series.decode_tok_s, chart_w));
            v.push(Span::raw("   "));
            v.extend(half(rl, format!("{reading} tok/s"), if typical { " typical " } else if chart_w > 0 { " " } else { "" }, sparkline_to(&s.series.prefill_tok_s, chart_w, Some(read_top))));
            v
        };
        let plain = spans_width(&build(0, true));
        let chart_w = (w.saturating_sub(plain + 2) / 2).min(16);
        lines.push(first_fit(
            vec![
                if chart_w >= 4 { build(chart_w, true) } else { build(0, true) },
                build(0, false),
                vec![label("write "), Span::styled(format!("{:.0}", sv.decode_tok_s), b), label(" · read "), Span::styled(reading.clone(), b), label(" tok/s")],
            ],
            w,
        ));
    } else {
        let since = sv.down_since.map_or_else(String::new, |t| format!(" for {}", fmt_duration(cx.now - t)));
        let loud = red().add_modifier(Modifier::BOLD);
        lines.push(first_fit(vec![vec![Span::styled(format!("SERVE IS DOWN{since} - /v1/models not answering"), loud)], vec![Span::styled(format!("SERVE IS DOWN{since}"), loud)], vec![Span::styled("SERVE DOWN", loud)]], w));
    }

    // 2. the slots, the queue, the memory, the wait for the first word
    let full = sv.slots > 0 && sv.running >= f64::from(sv.slots);
    let run_style = if full { warn().add_modifier(Modifier::BOLD) } else { b };
    let queue_style = (if sv.queue >= s.thresholds.queue_reqs { red() } else if sv.queue > 0.0 { warn() } else { Style::default() }).add_modifier(Modifier::BOLD);
    let kv_style = (if sv.kv_usage >= 0.95 { warn() } else { Style::default() }).add_modifier(Modifier::BOLD);
    let ttft = sv.latency.as_ref().and_then(|l| l.ttft).map(|t| ms(t.p50_ms));
    let load = |g: usize, queue_word: &'static str, kv: bool, first_word: bool| -> Vec<Span<'static>> {
        let run = if sv.is_na("running") {
            "n/a".to_string()
        } else if g > 0 && sv.slots > 0 {
            format!("{} {:.0}/{}", gauge(sv.running / f64::from(sv.slots.max(1)), g), sv.running, sv.slots)
        } else if sv.slots > 0 {
            format!("{:.0}/{}", sv.running, sv.slots)
        } else {
            format!("{:.0}", sv.running)
        };
        let mut v = vec![label("running "), Span::styled(run, run_style), label(queue_word), Span::styled(format!("{:.0}", sv.queue), queue_style)];
        if kv {
            v.extend([label(" · memory (KV) "), Span::styled(if sv.is_na("kv_usage") { "n/a".to_string() } else { pct(sv.kv_usage) }, kv_style)]);
        }
        if let (true, Some(t)) = (first_word, &ttft) {
            v.extend([label(" · first word "), Span::styled(t.clone(), b)]);
        }
        v
    };
    let load_line = first_fit(vec![load(8, " · queue ", true, true), load(8, " · queue ", true, false), load(0, " · queue ", true, false), load(0, " · queue ", false, false)], w);

    // 3. the hottest GPU in both units, and the alerts
    let hot = s.thresholds.thermal_temp_c;
    let hottest = s.gpus.iter().filter(|g| g.sample.temp_c.is_some()).max_by(|a, b| a.sample.temp_c.unwrap_or(0.0).total_cmp(&b.sample.temp_c.unwrap_or(0.0)));
    let power: f64 = s.gpus.iter().filter_map(|g| g.sample.power_w).sum();
    let alerts = |long: bool| -> Span<'static> {
        if s.firing.is_empty() {
            Span::styled(if long { "alerts none firing" } else { "alerts 0" }, dim())
        } else {
            Span::styled(format!("{} ALERT{} FIRING", s.firing.len(), if s.firing.len() == 1 { "" } else { "S" }), red().add_modifier(Modifier::BOLD))
        }
    };
    let gpu = |style: lss_core::units::TempStyle, lead: &'static str, watts: bool| -> Vec<Span<'static>> {
        match hottest {
            Some(g) => {
                let tstyle = if g.thermal_excluded { Style::default() } else { temp_style(g.sample.temp_c, hot) };
                let mut v = vec![label(lead), Span::raw(format!("GPU{} ", g.sample.index)), Span::styled(lss_core::units::temp_opt(g.sample.temp_c, style), tstyle.add_modifier(Modifier::BOLD))];
                if watts {
                    v.push(Span::raw(format!(" · {power:.0} W")));
                }
                if s.gpus.iter().any(gpu_problem(s)) {
                    v.push(Span::styled(" THERMAL", red()));
                }
                v
            }
            None if s.gpu_source == "none" => vec![label(lead), Span::styled("GPU stats unavailable", dim())],
            None if s.gpus.is_empty() => vec![label(lead), Span::styled("no GPU reading", red())],
            None => vec![label(lead), Span::styled(format!("GPU {}% busy", s.gpus.iter().filter_map(|g| g.sample.util_pct).fold(0.0_f64, f64::max).round()), b)],
        }
    };
    let with_alerts = |mut v: Vec<Span<'static>>, long: bool| -> Vec<Span<'static>> {
        v.push(label(" · "));
        v.push(alerts(long));
        v
    };
    use lss_core::units::TempStyle::{Compact, Full};
    let heat_line = first_fit(vec![with_alerts(gpu(Full, "hottest ", true), true), with_alerts(gpu(Full, "hottest ", false), true), with_alerts(gpu(Compact, "hottest ", false), false), with_alerts(gpu(Compact, "hot ", false), false)], w);

    if rows >= 3 {
        lines.push(load_line);
        lines.push(heat_line);
    } else if rows == 2 {
        // two rows: the load and the heat share the second one
        let mut v = load(0, " · q ", false, false);
        v.push(label(" · "));
        v.extend(gpu(Compact, "hot ", false));
        v.push(label(" · "));
        v.push(alerts(false));
        lines.push(first_fit(vec![v, load(0, " · queue ", false, false)], w));
    }
    if rows > 3 && sv.up {
        // a taller card: the rest of SERVE, after the rows this card already said in its own words
        let skip = if w >= 44 { 4 } else { 3 };
        lines.extend(serve_lines(cx, w, 99).into_iter().skip(skip));
    }
    lines.truncate(rows);
    let used = lines.len() as u16;
    render_lines(f, inner, lines);
    fill_chart(f, Rect { y: inner.y + used.min(inner.height), height: inner.height.saturating_sub(used), ..inner }, ("writing tok/s · last hour", "write 1h"), &s.series.decode_tok_s, (0.0, None), axis_number, Color::Blue);
}

/// One bar for the areas that have no row of their own in the minimized overview. Whole
/// pieces only: what does not fit is left out, never cut mid-word (the card above already
/// carries the hottest GPU, so GPUS is the first to go).
fn more_bar(cx: &Ctx, panels: &[Panel], width: usize) -> Line<'static> {
    let s = cx.s;
    let piece = |p: Panel| -> Option<Span<'static>> {
        Some(match p {
            Panel::Serve | Panel::Gpus => return None,
            Panel::Users if s.gate.absent => return None,
            Panel::Users => Span::raw(if s.users.available { format!("{} users now", s.users.active_now) } else { format!("slots {:.0}/{}", s.serve.running, s.serve.slots) }),
            Panel::Lanes => {
                if s.gate.up {
                    Span::raw(format!("queue {:.0}", s.lanes.public.queued + s.lanes.trusted.queued))
                } else {
                    Span::styled("GATE DOWN", red())
                }
            }
            Panel::Advice => match worst(s) {
                Some(sev) => Span::styled(format!("advice {}", sev.as_str().to_uppercase()), severity_colour(sev)),
                None => Span::styled("advice -", dim()),
            },
            Panel::Incidents => {
                let open = s.incidents.iter().filter(|i| i.end.is_none()).count();
                if open > 0 {
                    Span::styled(format!("{open} incident(s) OPEN"), red())
                } else {
                    Span::raw(format!("{} incidents", s.incidents.len()))
                }
            }
            Panel::Alerts => {
                if s.firing.is_empty() {
                    Span::raw("alerts 0")
                } else {
                    Span::styled(format!("{} FIRING", s.firing.len()), red())
                }
            }
        })
    };
    let room = width.saturating_sub("MORE".len() + 2 + 8);
    let mut body: Vec<Span<'static>> = Vec::new();
    for span in panels.iter().filter_map(|p| piece(*p)) {
        let sep = if body.is_empty() { 0 } else { 3 };
        if spans_width(&body) + sep + span.content.chars().count() > room {
            continue;
        }
        if sep > 0 {
            body.push(Span::styled(" · ", dim()));
        }
        body.push(span);
    }
    bar_line("MORE", body, panels.contains(&cx.app.focus), width)
}

/// The `(detail)` of a box title. Problems are red, the rest takes the box colour.
fn detail(cx: &Ctx, p: Panel) -> Vec<Span<'static>> {
    let s = cx.s;
    match p {
        Panel::Serve => {
            let state = crate::plain::serve_state(s, cx.now);
            // the box is too short for its C1 line: the reading takes the title (the header
            // already says how long the serve has been up)
            if let (true, Some(v)) = (s.serve.up && (1..4).contains(&cx.serve_rows), s.probe.last_ok.as_ref().and_then(|p| p.decode_tok_s)) {
                let low = s.c1_floor().is_some_and(|f| v < f);
                return vec![Span::styled(format!("C1 {v:.1} tok/s"), if low { red() } else { Style::default() })];
            }
            vec![if s.serve.up { Span::raw(state) } else { Span::styled(state, red()) }]
        }
        Panel::Gpus => {
            if s.gpus.is_empty() {
                return vec![Span::styled("no reading", red())];
            }
            let power: f64 = s.gpus.iter().filter_map(|g| g.sample.power_w).sum();
            let mut d = vec![Span::raw(format!("{} · {power:.0} W", s.gpus.len()))];
            if cx.gpu_bare {
                // the narrowest cards carry bare numbers (`52/126`): the title says what they are
                d.push(Span::styled(format!(" · {}", lss_core::units::unit_caption()), dim()));
            }
            if s.gpus.iter().any(gpu_problem(s)) {
                d.push(Span::styled(" · THERMAL", red()));
            }
            d
        }
        Panel::Lanes => {
            if s.gate.absent {
                vec![Span::styled("no gateway", dim())]
            } else if s.gate.up {
                vec![Span::raw(format!("gate {}", s.gate.version.as_deref().unwrap_or("up")))]
            } else {
                vec![Span::styled("GATE DOWN", red())]
            }
        }
        Panel::Users => {
            if s.users.available {
                vec![Span::raw(format!("{} now · {} in 24h", s.users.active_now, s.users.totals.users_24h))]
            } else if s.gate.absent {
                vec![Span::styled("no gateway", dim())]
            } else {
                vec![Span::styled("needs gate v5.2", dim())]
            }
        }
        Panel::Advice => match worst(s) {
            Some(Severity::Act) => vec![Span::styled("act", red())],
            Some(Severity::Watch) => vec![Span::styled("watch", warn())],
            Some(Severity::Fine) => vec![Span::raw("all fine")],
            None => vec![Span::styled("collecting", dim())],
        },
        Panel::Incidents => {
            let open = s.incidents.iter().filter(|i| i.end.is_none()).count();
            let mut d = vec![Span::raw(format!("{} in 7d", s.incidents.len()))];
            if open > 0 {
                d.push(Span::styled(format!(" · {open} OPEN"), red()));
            }
            d
        }
        Panel::Alerts => {
            if s.firing.is_empty() {
                vec![Span::raw("none firing")]
            } else {
                vec![Span::styled(format!("{} FIRING", s.firing.len()), red())]
            }
        }
    }
}

fn gpu_problem(s: &Status) -> impl Fn(&GpuStatus) -> bool + '_ {
    move |g| !g.thermal_excluded && (g.sample.thermal_throttled() || g.sample.temp_c.is_some_and(|t| t >= s.thresholds.thermal_temp_c)) || g.health.as_ref().is_some_and(|h| !h.problems().is_empty())
}

fn temp_style(t: Option<f64>, hot: f64) -> Style {
    match t {
        Some(t) if t >= hot => red(),
        Some(t) if t >= hot - 10.0 => warn(),
        _ => Style::default(),
    }
}

/// One-line form of an area, for the bars.
fn bar(cx: &Ctx, p: Panel, width: usize) -> Line<'static> {
    let s = cx.s;
    let focused = cx.app.focus == p;
    let body: Vec<Span<'static>> = match p {
        Panel::Serve => {
            if s.serve.up {
                let read = s.serve.prefill_tok_s.or(s.serve.prefill_tok_s_typical).map_or_else(|| "-".to_string(), |v| format!("{v:.0}"));
                let long = format!("write {:.0} · read {read} tok/s · run {:.0}/{} · q {:.0} · KV {}", s.serve.decode_tok_s, s.serve.running, s.serve.slots, s.serve.queue, pct(s.serve.kv_usage));
                let short = format!("write {:.0} · read {read} tok/s · run {:.0}/{}", s.serve.decode_tok_s, s.serve.running, s.serve.slots);
                vec![Span::raw(if long.chars().count() + p.title().len() + 10 <= width { long } else { short })]
            } else {
                vec![Span::styled(crate::plain::serve_state(s, cx.now), red())]
            }
        }
        Panel::Gpus => {
            if s.gpus.is_empty() {
                vec![Span::styled("no reading - nvidia-smi failed", red())]
            } else {
                let hot = s.thresholds.thermal_temp_c;
                let power: f64 = s.gpus.iter().filter_map(|g| g.sample.power_w).sum();
                let util = s.gpus.iter().filter_map(|g| g.sample.util_pct).sum::<f64>() / s.gpus.len() as f64;
                let units = lss_core::units::temp_units();
                // every GPU in each configured unit (`52/42/47/46C 126/108/117/115F`), then the tail
                let build = |with_util: bool| -> Vec<Span<'static>> {
                    let mut v = Vec::new();
                    let list = |v: &mut Vec<Span<'static>>, to_unit: fn(f64) -> f64, letter: &str| {
                        for (i, g) in s.gpus.iter().enumerate() {
                            if i > 0 {
                                v.push(Span::raw("/"));
                            }
                            v.push(Span::styled(num(g.sample.temp_c.map(to_unit), ""), if g.thermal_excluded { Style::default() } else { temp_style(g.sample.temp_c, hot) }));
                        }
                        v.push(Span::raw(letter.to_string()));
                    };
                    if units != lss_core::units::TempUnits::F {
                        list(&mut v, |c| c, "C");
                    }
                    if units == lss_core::units::TempUnits::Both {
                        v.push(Span::raw(" "));
                    }
                    if units != lss_core::units::TempUnits::C {
                        list(&mut v, lss_core::units::c_to_f, "F");
                    }
                    v.push(Span::raw(if with_util { format!(" · {power:.0} W · util {util:.0}%") } else { format!(" · {power:.0} W") }));
                    if s.gpus.iter().any(gpu_problem(s)) {
                        v.push(Span::styled(" · THERMAL", red()));
                    }
                    v
                };
                // the hottest GPU alone when even the short list does not fit: both units, always
                let hottest = || -> Vec<Span<'static>> {
                    let max = s.gpus.iter().filter_map(|g| g.sample.temp_c).fold(f64::NEG_INFINITY, f64::max);
                    let mut v = vec![Span::raw("max "), Span::styled(lss_core::units::temp_opt(max.is_finite().then_some(max), lss_core::units::TempStyle::Compact), temp_style(max.is_finite().then_some(max), hot)), Span::raw(format!(" · {power:.0} W"))];
                    if s.gpus.iter().any(gpu_problem(s)) {
                        v.push(Span::styled(" · THERMAL", red()));
                    }
                    v
                };
                let room = width.saturating_sub(p.title().len() + 2 + 8);
                [build(true), build(false)].into_iter().find(|v| spans_width(v) <= room).unwrap_or_else(hottest)
            }
        }
        Panel::Lanes if s.gate.absent => vec![Span::styled("no gateway configured", dim())],
        Panel::Lanes => {
            let five = s.lanes.public.codes_10m.c5xx + s.lanes.trusted.codes_10m.c5xx;
            let build = |waiters: bool, excluded: bool| -> Vec<Span<'static>> {
                let lane = |l: &LaneStatus| if waiters || l.waiters > 0 { format!("{:.0}r {:.0}q {}w", l.running, l.queued, l.waiters) } else { format!("{:.0}r {:.0}q", l.running, l.queued) };
                let mut v = vec![];
                if !s.gate.up {
                    v.push(Span::styled("GATE DOWN · ", red()));
                }
                v.push(Span::raw(format!("public {} · trusted {}", lane(&s.lanes.public), lane(&s.lanes.trusted))));
                if five > 0 {
                    v.push(Span::styled(format!(" · 5xx {five}"), red()));
                }
                v.push(Span::styled(format!(" · probes: {}{}", s.lanes.trusted.probes.admitted, if excluded { " excluded" } else { "" }), dim()));
                v
            };
            // the wording that fits the bar; the probes count is the last thing to go
            let room = width.saturating_sub(p.title().len() + 2 + 8);
            [build(true, true), build(false, true), build(true, false), build(false, false)].into_iter().find(|v| spans_width(v) <= room).unwrap_or_else(|| {
                // not even the short form: the probes count goes, the lanes stay whole
                let mut v = build(false, false);
                v.pop();
                v
            })
        }
        Panel::Users => {
            if s.users.available {
                let room = width.saturating_sub(p.title().len() + 2 + 8);
                let u = &s.users;
                let top = u.rows.first().filter(|r| r.gate.inflight > 0).map(|r| format!(" · {} {}", r.name, r.gate.inflight)).unwrap_or_default();
                let texts = [
                    format!("{} now · {} in 10m · {} in 24h · slots {:.0}/{}{top}", u.active_now, u.totals.users_active_10m, u.totals.users_24h, s.serve.running, s.serve.slots),
                    format!("{} now · {} in 10m · {} in 24h · slots {:.0}/{}", u.active_now, u.totals.users_active_10m, u.totals.users_24h, s.serve.running, s.serve.slots),
                    format!("{} now · {} in 24h · slots {:.0}/{}", u.active_now, u.totals.users_24h, s.serve.running, s.serve.slots),
                    format!("{} now · slots {:.0}/{}", u.active_now, s.serve.running, s.serve.slots),
                ];
                let n = texts.len();
                vec![Span::raw(texts.into_iter().enumerate().find(|(i, t)| t.chars().count() <= room || i + 1 == n).map(|(_, t)| t).unwrap_or_default())]
            } else if s.gate.absent {
                vec![Span::styled("no gateway configured: nothing tells users apart", dim())]
            } else {
                vec![Span::raw(format!("slots {:.0}/{}", s.serve.running, s.serve.slots)), Span::styled(" · per-user numbers need gate v5.2", dim())]
            }
        }
        Panel::Advice => match s.advice_top.first() {
            Some(f) => vec![Span::styled(format!("{} ", f.severity.as_str().to_uppercase()), severity_colour(f.severity)), Span::raw(f.sentence.clone())],
            None => vec![Span::styled("collecting evidence", dim())],
        },
        Panel::Incidents => {
            let open = s.incidents.iter().filter(|i| i.end.is_none()).count();
            let mut v = vec![Span::raw(format!("{} in 7d", s.incidents.len()))];
            if open > 0 {
                v.push(Span::styled(format!(" · {open} OPEN"), red()));
            }
            if !cx.alerts_drawn {
                // no row left for ALERTS: it rides here
                v.push(if s.firing.is_empty() { Span::raw(" · alerts: none firing") } else { Span::styled(format!(" · FIRING {}", s.firing.join(", ")), red()) });
            } else if let Some(i) = s.incidents.first() {
                v.push(Span::raw(format!(" · last {} {}", fmt_local(i.start, "%m-%d %H:%M"), i.kind)));
            }
            v
        }
        Panel::Alerts => {
            let mut v = if s.firing.is_empty() { vec![Span::raw("none firing")] } else { vec![Span::styled(format!("FIRING {}", s.firing.join(", ")), red())] };
            if let Some(a) = s.alerts.first() {
                v.push(Span::raw(format!(" · last {} {}", fmt_local(a.ts, "%m-%d %H:%M"), a.rule)));
            }
            v
        }
    };
    bar_line(p.title(), body, focused, width)
}

fn label(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), dim())
}

/// Milliseconds, compact: `183ms`, `2.0s`.
fn ms(v: f64) -> String {
    if v < 10.0 {
        format!("{v:.1}ms")
    } else if v < 1000.0 {
        format!("{v:.0}ms")
    } else {
        format!("{:.1}s", v / 1000.0)
    }
}

/// The first of `variants` that fits `w`; the last one, cut, when none does.
fn first_fit(variants: Vec<Vec<Span<'static>>>, w: usize) -> Line<'static> {
    let n = variants.len();
    for (i, v) in variants.into_iter().enumerate() {
        if spans_width(&v) <= w || i + 1 == n {
            return fit_line(v, w);
        }
    }
    Line::default()
}

/// SERVE: big numbers with small gauges and sparklines. `rows` decides how much of it shows;
/// the order below is the order of importance. Every line has a wide and a narrow wording, so
/// the key numbers are never cut off at 63 columns.
fn serve_lines(cx: &Ctx, w: usize, rows: usize) -> Vec<Line<'static>> {
    let s = cx.s;
    let sv = &s.serve;
    let mut out: Vec<Line<'static>> = Vec::new();
    if !sv.up {
        let since = sv.down_since.map_or_else(String::new, |t| format!(" for {}", fmt_duration(cx.now - t)));
        let loud = red().add_modifier(Modifier::BOLD);
        out.push(first_fit(vec![vec![Span::styled(format!("SERVE IS DOWN{since} - /v1/models not answering"), loud)], vec![Span::styled(format!("SERVE IS DOWN{since}"), loud)], vec![Span::styled("SERVE IS DOWN", loud)]], w));
    }
    let wide = w >= 44;
    let pad = |name: &str| if wide { format!("{name:<9}") } else { format!("{name} ") };
    let spark = fg(Color::Blue);
    let b = bold();
    let queue_style = if sv.queue >= s.thresholds.queue_reqs { red() } else if sv.queue > 0.0 { warn() } else { Style::default() };
    let full = sv.slots > 0 && sv.running >= f64::from(sv.slots);
    let kv_style = if sv.kv_usage >= 0.95 { warn() } else { Style::default() };

    // WRITING (decode) tok/s + sparkline of the last hour, then READING (prefill) tok/s: the two
    // speeds of a model, always both. Reading is bursty: while nothing is being read the
    // loadout's typical figure stands in, marked as such.
    let speed_pad = |name: &str, what: &str, short: &str| if wide { format!("{:<18}", format!("{name} ({what})")) } else { format!("{short:<6}") };
    let lead = speed_pad("writing", "decode", "write");
    if let Some(na) = sv.na("decode_tok_s") {
        // the engine publishes no speed and no token counter: lss's own probe (C1, below) measures it
        out.push(first_fit(vec![vec![label(&lead), Span::styled(na.clone(), dim()), label(" · see C1")], vec![label(&lead), Span::styled(na, dim())], vec![label(&lead), Span::styled("n/a", dim())]], w));
    } else if sv.decode_tok_s_from_probe {
        // this engine publishes no speed: the number is the monitor's own test, and says so
        let head = format!("{:.1} tok/s ", sv.decode_tok_s);
        out.push(first_fit(
            vec![
                vec![label(&lead), Span::styled(head.clone(), b), label("from the monitor's own test (C1): this engine does not report it")],
                vec![label(&lead), Span::styled(head.clone(), b), label("from the C1 test: not reported by the engine")],
                vec![label(&lead), Span::styled(head, b), label("(C1)")],
            ],
            w,
        ));
    } else {
        let head = if wide { format!("{:>7.1} tok/s  ", sv.decode_tok_s) } else { format!("{:.1} tok/s ", sv.decode_tok_s) };
        let room = w.saturating_sub(lead.chars().count() + head.chars().count());
        out.push(Line::from(vec![label(&lead), Span::styled(head, b), Span::styled(sparkline(&s.series.decode_tok_s, room), spark)]));
    }
    let lead = speed_pad("reading", "prefill", "read");
    // the same columns as the writing row: label, number, then the last hour as a sparkline.
    // While nothing is being read the loadout's typical figure stands in, marked as such.
    // against a floor of 100 tok/s: the monitor's own tiny probe prompts must read as an idle
    // hour, not as a full-height wiggle
    let read_top = s.series.prefill_tok_s.iter().flatten().copied().fold(100.0_f64, f64::max);
    let read_spark = |used: usize| -> Span<'static> { Span::styled(sparkline_to(&s.series.prefill_tok_s, w.saturating_sub(used), Some(read_top)), spark) };
    let read = match (sv.prefill_tok_s, sv.prefill_tok_s_typical) {
        (Some(v), _) => {
            let head = if wide { format!("{v:>7.0} tok/s  ") } else { format!("{v:.0} tok/s ") };
            let used = lead.chars().count() + head.chars().count();
            vec![vec![label(&lead), Span::styled(head.clone(), b), read_spark(used)], vec![label(&lead), Span::styled(head, b)]]
        }
        (None, Some(t)) => {
            let head = if wide { format!("{t:>7.0} tok/s  ") } else { format!("{t:.0} tok/s ") };
            let note = if wide { "typical  " } else { "typ " };
            let used = lead.chars().count() + head.chars().count() + note.chars().count();
            vec![vec![label(&lead), Span::styled(head.clone(), b), label(note), read_spark(used)], vec![label(&lead), Span::styled(head.clone(), b), label(note)], vec![label(&lead), Span::styled(head, b)]]
        }
        (None, None) if sv.is_na("prefill_tok_s") => vec![vec![label(&lead), Span::styled(sv.na("prefill_tok_s").unwrap_or_default(), dim())], vec![label(&lead), Span::styled("n/a", dim())]],
        (None, None) => vec![vec![label(&lead), Span::styled(sv.no_reading_reason(), dim())], vec![label(&lead), Span::styled("-", dim())]],
    };
    out.push(first_fit(read, w));

    // three rows only (63x11): slots, queue and memory share the last one
    if !wide && rows <= 3 {
        out.push(first_fit(
            vec![
                vec![label("run "), Span::styled(if sv.is_na("running") { "n/a".to_string() } else { format!("{:.0}/{}", sv.running, sv.slots) }, if full { warn().add_modifier(Modifier::BOLD) } else { b }), label("  q "), Span::styled(if sv.is_na("queued") { "n/a".to_string() } else { format!("{:.0}", sv.queue) }, queue_style.add_modifier(Modifier::BOLD)), label("  KV "), Span::styled(if sv.is_na("kv_usage") { "n/a".to_string() } else { pct(sv.kv_usage) }, kv_style.add_modifier(Modifier::BOLD))],
                vec![label("run "), Span::styled(format!("{:.0}/{}", sv.running, sv.slots), b), label(" q "), Span::styled(format!("{:.0}", sv.queue), queue_style)],
            ],
            w,
        ));
        out.truncate(rows.max(1));
        return out;
    }

    // running / slots gauge + queue
    let run_na = sv.na("running");
    let run = if run_na.is_some() {
        "n/a".to_string()
    } else if sv.slots > 0 { format!("{} {:.0}/{}", gauge(sv.running / f64::from(sv.slots.max(1)), 8), sv.running, sv.slots) } else { format!("{:.0} (slots n/a)", sv.running) };
    let run_style = if full { warn().add_modifier(Modifier::BOLD) } else { b };
    let mut l = vec![label(&pad(if wide { "running" } else { "run" })), Span::styled(run, run_style), label(if wide { "   queue " } else { "  q " }), Span::styled(format!("{:.0}", sv.queue), queue_style.add_modifier(Modifier::BOLD))];
    let left = w.saturating_sub(spans_width(&l) + 2);
    if let Some(na) = &run_na {
        // no running / queued count from this engine: say so once, in words
        l = vec![label(&pad(if wide { "running" } else { "run" })), Span::styled(na.clone(), dim())];
    } else if left >= 6 {
        l.push(Span::raw("  "));
        l.push(Span::styled(sparkline_to(&s.series.running, left, Some(f64::from(sv.slots.max(1)))), spark));
    }
    out.push(first_fit(vec![l, vec![label("run "), Span::styled(if run_na.is_some() { "n/a".to_string() } else { format!("{:.0}", sv.running) }, b)]], w));

    // KV gauge (+ sparkline when wide); on a narrow box TTFT shares the line
    let lat = sv.latency.as_ref();
    let ttft = lat.and_then(|x| x.ttft);
    let ttft_spans = |short: bool| -> Vec<Span<'static>> {
        match ttft {
            Some(t) if short => vec![label("TTFT "), Span::styled(format!("{}/{}", ms(t.p50_ms), ms(t.p99_ms)), b)],
            Some(t) => vec![label(&pad("TTFT")), label("p50 "), Span::styled(ms(t.p50_ms), b), label(" · p90 "), Span::styled(ms(t.p90_ms), b), label(" · p99 "), Span::styled(ms(t.p99_ms), b), label(" (10 min)")],
            None if sv.is_na("ttft") => vec![label(&pad("TTFT")), Span::styled(if short { "n/a".to_string() } else { format!("{} · C1 measures it", sv.na("ttft").unwrap_or_default()) }, dim())],
            None => vec![label(&pad("TTFT")), label("avg "), Span::styled(opt_ms(sv.ttft_avg_ms_10m), b), label(" (10 min)")],
        }
    };
    if wide {
        let kv_text = sv.na("kv_usage").unwrap_or_else(|| format!("{} {}", gauge(sv.kv_usage, 8), pct(sv.kv_usage)));
        let mut l = vec![label(&pad("KV")), Span::styled(kv_text, if sv.is_na("kv_usage") { dim() } else { kv_style.add_modifier(Modifier::BOLD) })];
        let left = w.saturating_sub(spans_width(&l) + 3);
        // #69 item 3 (the panel, on the ship-gate audit this card asked for): "a sparkline on
        // KV% at 8.4% says nothing" - scaled against the full 0-100% domain, real movement
        // within a low, narrow band (5%-12%, say) barely registers as one bar of difference. A
        // sparkline that CANNOT show its own hour's real range is exactly the "chart of nothing"
        // item 2 names: below 10 points of spread, the gauge+percentage already said the number.
        let kv_varies = { let vals: Vec<f64> = s.series.kv_usage.iter().flatten().copied().collect(); vals.iter().copied().fold(f64::MIN, f64::max) - vals.iter().copied().fold(f64::MAX, f64::min) >= 0.10 };
        if left >= 6 && !sv.is_na("kv_usage") && kv_varies {
            l.push(Span::raw("   "));
            l.push(Span::styled(sparkline_to(&s.series.kv_usage, left, Some(1.0)), spark));
        }
        out.push(fit_line(l, w));
        out.push(fit_line(ttft_spans(false), w));
    } else {
        let kv = |g: usize| -> Vec<Span<'static>> {
            let mut v = vec![label("KV "), Span::styled(if sv.is_na("kv_usage") { "n/a".to_string() } else if g > 0 { format!("{} {}", gauge(sv.kv_usage, g), pct(sv.kv_usage)) } else { pct(sv.kv_usage) }, kv_style.add_modifier(Modifier::BOLD)), Span::raw("  ")];
            v.extend(ttft_spans(true));
            v
        };
        out.push(first_fit(vec![kv(6), kv(4), kv(0)], w));
    }

    // C1 probe
    let c1 = match &s.probe.last_ok {
        Some(p) => {
            let v = p.decode_tok_s.unwrap_or(0.0);
            let low = s.c1_floor().is_some_and(|floor| v < floor);
            let age = cx.now - p.ts;
            // #47, 2026-09-21: an hours-old number next to a live clock, coloured no differently
            // from a fresh one, is the same lie in the other direction as an unproved reading
            // shown as proved - `c1_stale_secs` is the SAME threshold the rule alerts on, not a
            // separate hardcoded guess, so the screen and the alert never disagree.
            let stale = age >= s.thresholds.c1_stale_secs;
            let head = vec![label(&pad(if wide { "C1 probe" } else { "C1" })), Span::styled(format!("{v:.1} tok/s"), if low { warn().add_modifier(Modifier::BOLD) } else { b })];
            let age_style = if stale { warn().add_modifier(Modifier::BOLD) } else { Style::default() };
            let stale_note = if stale { " STALE - too busy to measure" } else { "" };
            let skipped = s.probe.invalid_skipped;
            let mut long = head.clone();
            long.extend([label(" · TTFT "), Span::raw(opt_ms(p.ttft_ms)), label(" · "), Span::styled(format!("{} ago{stale_note}", fmt_duration(age)), age_style)]);
            let mut mid = head.clone();
            mid.extend([label(" · "), Span::styled(format!("{} ago{stale_note}", fmt_duration(age)), age_style)]);
            let mut short = head;
            short.extend([Span::raw(" "), Span::styled(format!("{}{}", fmt_duration(age), if stale { " STALE" } else { "" }), age_style)]);
            if skipped > 0 {
                long.push(Span::styled(format!(" ({skipped} invalid skipped)"), dim()));
                mid.push(Span::styled(format!(" ({skipped} invalid)"), dim()));
                short.push(Span::styled(format!(" ({skipped} inv)"), dim()));
            }
            let bare = short.iter().take(4).cloned().collect::<Vec<_>>();
            first_fit(vec![long, mid, short, bare], w)
        }
        None => fit_line(vec![label(&pad("C1")), Span::styled(format!("no valid result yet ({})", s.probe.baseline_source), dim())], w),
    };
    out.push(c1);

    // #50, 2026-09-21: real-traffic decode speed, bucketed by concurrency - never rendered as a
    // number comparable to C1 (a different concurrency level, a different measuring method), so
    // it always carries its own "live" label and the concurrency + sample count it was seen at.
    if let Some(live) = &s.serve.live_decode {
        let per_req = live.per_request_tok_s.unwrap_or(0.0);
        let n = live.running;
        let samples = live.samples;
        let long = vec![
            label(&pad("live")),
            Span::styled(format!("{per_req:.1} tok/s/req"), b),
            label(&format!(" at {n} in flight ({samples} samples, real traffic, not comparable to C1)")),
        ];
        let short = vec![label(&pad("live")), Span::styled(format!("{per_req:.1} tok/s/req"), b), label(&format!(" @{n} ({samples}s)"))];
        out.push(first_fit(vec![long, short], w));
    }

    // accept + cache hit
    let accept = if sv.is_na("spec") { "n/a".to_string() } else { format!("{:.2}", sv.spec_accept_length) };
    let cache = if sv.is_na("cache_hit") { "n/a".to_string() } else { pct(sv.cache_hit_rate) };
    out.push(first_fit(
        vec![
            vec![label(&pad("accept")), Span::styled(if sv.is_na("spec") { accept.clone() } else { format!("{accept} ({:.0}%)", sv.spec_accept_rate * 100.0) }, b), label(" · cache hit "), Span::styled(cache.clone(), b)],
            vec![label("accept "), Span::styled(accept, b), label("  cache "), Span::styled(cache, b)],
        ],
        w,
    ));

    // the other latencies
    if let Some(l) = lat {
        let p50 = |h: Option<lss_core::hist::HistSummary>| h.map_or_else(|| "-".to_string(), |x| ms(x.p50_ms));
        let p99 = |h: Option<lss_core::hist::HistSummary>| h.map_or_else(|| "-".to_string(), |x| ms(x.p99_ms));
        out.push(first_fit(
            vec![
                vec![label(&pad("ITL")), label("p50 "), Span::raw(p50(l.itl)), label(" · p99 "), Span::raw(p99(l.itl)), label("   e2e p50 "), Span::raw(p50(l.e2e)), label(" · p99 "), Span::raw(p99(l.e2e))],
                vec![label("ITL "), Span::raw(p50(l.itl)), label("  e2e "), Span::raw(format!("{}/{}", p50(l.e2e), p99(l.e2e)))],
            ],
            w,
        ));
        out.push(first_fit(vec![vec![label(&pad("queue")), label("wait p50 "), Span::raw(p50(l.queue_time)), label(" · p99 "), Span::raw(p99(l.queue_time))], vec![label("q wait "), Span::raw(format!("{}/{}", p50(l.queue_time), p99(l.queue_time)))]], w));
    }

    // what the C1 rule compares against
    let t = &s.thresholds;
    let base = s.c1_baseline().map_or_else(|| "-".into(), |v| format!("{v:.1}"));
    out.push(first_fit(
        match s.c1_floor() {
            Some(floor) => vec![
                vec![Span::styled(format!("C1 alert < {floor:.1} tok/s = {:.0}% of baseline {base} ({}), {} in a row", t.c1_ratio * 100.0, s.probe.baseline_source, t.c1_consecutive), dim())],
                vec![Span::styled(format!("C1 alert < {floor:.1} tok/s ({:.0}% of {base})", t.c1_ratio * 100.0), dim())],
                vec![Span::styled(format!("C1 alert < {floor:.1}"), dim())],
            ],
            None => vec![vec![Span::styled(format!("C1 alert: {}", s.probe.baseline_source), dim())]],
        },
        w,
    ));

    // lifetime counters of this engine run
    let big = |v: f64| if v >= 1e6 { format!("{:.1}M", v / 1e6) } else if v >= 1e3 { format!("{:.1}k", v / 1e3) } else { format!("{v:.0}") };
    out.push(first_fit(
        vec![
            vec![label(&pad("tokens")), label("prompt "), Span::raw(big(sv.prompt_tokens_total)), label(" · generated "), Span::raw(big(sv.generation_tokens_total)), label(" · requests "), Span::raw(big(sv.requests_total))],
            vec![label("tok in "), Span::raw(big(sv.prompt_tokens_total)), label(" out "), Span::raw(big(sv.generation_tokens_total))],
        ],
        w,
    ));
    out.truncate(rows.max(1));
    out
}

/// How a card grid is cut: columns x rows of cards, each `w` x `h`, dense = first line in the
/// border (3-row cards).
struct Grid {
    cols: usize,
    w: u16,
    h: u16,
    dense: bool,
}

/// Picks the card arrangement that shows the most: prefers full cards (>= 4 rows), falls back
/// to dense 3-row cards, returns None when not even those fit.
fn card_grid(inner: Rect, n: usize, min_w: u16, force_dense: bool, reserve_rows: u16) -> Option<Grid> {
    if n == 0 {
        return None;
    }
    let avail_h = inner.height.saturating_sub(reserve_rows);
    let mut best: Option<(u32, Grid)> = None;
    for cols in (1..=n).rev() {
        let rows = n.div_ceil(cols) as u16;
        let w = inner.width / cols as u16;
        let h = (avail_h / rows.max(1)).min(7);
        if w < min_w || h < 3 {
            continue;
        }
        let dense = force_dense || h < 4;
        let lines = u32::from(h) - 2 + u32::from(dense);
        let score = lines.min(5) * u32::from(w.min(30)) + if dense { 0 } else { 8 };
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, Grid { cols, w, h, dense }));
        }
    }
    best.map(|(_, g)| g)
}

fn gpu_grid(inner: Rect, n: usize, force_dense: bool) -> Option<Grid> {
    // one spare row under the cards for the totals line when there is room
    card_grid(inner, n, 7, force_dense, u16::from(inner.height >= 4))
}

fn lane_grid(inner: Rect, force_dense: bool) -> Option<Grid> {
    // a tall box: the two cards stacked, each the full width, with room for every counter
    if inner.height >= 15 && inner.width >= 50 {
        return Some(Grid { cols: 1, w: inner.width, h: if force_dense { 6 } else { 7 }, dense: force_dense });
    }
    card_grid(inner, 2, 16, force_dense, 1)
}

struct Card {
    title: Vec<Span<'static>>,
    lines: Vec<Line<'static>>,
    colour: Color,
}

fn draw_cards(f: &mut Frame, inner: Rect, grid: &Grid, cards: Vec<Card>, fg_plain: Color) -> u16 {
    let mut bottom = inner.y;
    // dense titles are padded (` GPU0 `) only when every card has the room: one look per render
    let pad_titles = cards.iter().all(|c| grid.w.saturating_sub(2) as usize >= spans_width(&c.title) + 2);
    // columns left over by the division go to the first cards, one each
    let extra = inner.width.saturating_sub(grid.w * grid.cols as u16);
    for (i, card) in cards.into_iter().enumerate() {
        let (col, row) = ((i % grid.cols) as u16, (i / grid.cols) as u16);
        let rect = Rect { x: inner.x + col * grid.w + col.min(extra), y: inner.y + row * grid.h, width: grid.w + u16::from(col < extra), height: grid.h };
        if rect.y + rect.height > inner.y + inner.height {
            break;
        }
        bottom = bottom.max(rect.y + rect.height);
        let mut block = frame(false, card.colour);
        let mut body = card.lines;
        let text_w = rect.width.saturating_sub(2) as usize;
        if grid.dense {
            // the first line rides in the top border; its text stays plain fg
            let pad = if pad_titles { " " } else { "" };
            let mut t = vec![Span::raw(pad.to_string())];
            t.extend(card.title.into_iter().map(|mut s| {
                if s.style.fg.is_none() {
                    s.style = s.style.fg(fg_plain);
                }
                s
            }));
            t.push(Span::raw(pad.to_string()));
            block = block.title(fit_line(t, text_w));
        } else {
            body.insert(0, fit_line(card.title, text_w));
        }
        body.truncate(rect.height.saturating_sub(2) as usize);
        f.render_widget(Paragraph::new(body).block(block), rect);
    }
    bottom
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// The totals under the GPUs: power against the cap, and the average load.
fn gpu_totals(s: &Status, w: usize) -> Line<'static> {
    let power: f64 = s.gpus.iter().filter_map(|g| g.sample.power_w).sum();
    let cap: f64 = s.gpus.iter().filter_map(|g| g.sample.power_limit_w).sum();
    let util = s.gpus.iter().filter_map(|g| g.sample.util_pct).sum::<f64>() / s.gpus.len().max(1) as f64;
    let mut spans = vec![label("total "), Span::raw(format!("{power:.0}/{cap:.0} W")), label("  util "), Span::raw(format!("{util:.0}%"))];
    let note = "  * thermal alerts off";
    if s.gpus.iter().any(|g| g.thermal_excluded) && spans_width(&spans) + note.len() <= w {
        spans.push(label(note));
    }
    fit_line(spans, w)
}

/// One compact row per GPU, in columns: name, temperature in both units, the last hour of that
/// temperature as a mini chart, power, load, memory. The chart takes what the columns leave.
fn gpu_rows(cx: &Ctx, w: usize) -> Vec<Line<'static>> {
    let s = cx.s;
    let hot = s.thresholds.thermal_temp_c;
    let style = if w >= 40 { lss_core::units::TempStyle::Full } else { lss_core::units::TempStyle::Compact };
    let temp_text = |g: &GpuStatus| lss_core::units::temp_opt(g.sample.temp_c, style).replace(" / ", "/");
    let name_w = s.gpus.iter().map(|g| format!("GPU{}{}", g.sample.index, if g.thermal_excluded { "*" } else { "" }).len()).max().unwrap_or(4);
    let temp_w = s.gpus.iter().map(|g| temp_text(g).chars().count()).max().unwrap_or(1);
    let with_mem = w >= 36;
    let tail_w = 5 + 5 + if with_mem { 7 } else { 0 };
    let chart_w = w.saturating_sub(name_w + 1 + temp_w + 1 + tail_w);
    s.gpus
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let x = &g.sample;
            let thermal = x.thermal_throttled() || x.throttle_mask & lss_core::gpu::THROTTLE_HW_SLOWDOWN != 0;
            let tstyle = if g.thermal_excluded { Style::default() } else { temp_style(x.temp_c, hot) };
            let mut spans = vec![
                Span::styled(format!("{:<name_w$} ", format!("GPU{}{}", x.index, if g.thermal_excluded { "*" } else { "" })), if thermal { red().add_modifier(Modifier::BOLD) } else { bold() }),
                Span::styled(format!("{:<temp_w$} ", temp_text(g)), tstyle.add_modifier(Modifier::BOLD)),
            ];
            if chart_w >= 3 {
                let chart = s.series.gpu_temp_c.get(i).map(|t| super::widgets::block_chart(t, chart_w, 1, 20.0, Some(hot.max(21.0))).into_iter().next().unwrap_or_default()).unwrap_or_default();
                spans.push(Span::styled(format!("{chart:<chart_w$}"), fg(Color::Green)));
            }
            spans.push(Span::raw(format!("{:>5}{:>5}", num(x.power_w, "W"), num(x.util_pct, "%"))));
            if with_mem {
                spans.push(Span::styled(format!("{:>7}", format!("{}G", gib(x.mem_used_mib))), dim()));
            }
            fit_line(spans, w)
        })
        .collect()
}

/// Returns the first row under what was drawn (the rest of the box is the caller's to fill).
fn draw_gpus(f: &mut Frame, cx: &Ctx, inner: Rect) -> u16 {
    let s = cx.s;
    if s.gpus.is_empty() {
        render_lines(f, inner, vec![Line::styled("no GPU reading - nvidia-smi failed or timed out", red())]);
        return inner.y + 1.min(inner.height);
    }
    let hot = s.thresholds.thermal_temp_c;
    if gpu_rows_mode(inner, s.gpus.len()) {
        let mut lines = gpu_rows(cx, inner.width as usize);
        if (inner.height as usize) > lines.len() {
            lines.push(gpu_totals(s, inner.width as usize));
        }
        let used = lines.len() as u16;
        render_lines(f, inner, lines);
        return inner.y + used;
    }
    let Some(grid) = gpu_grid(inner, s.gpus.len(), cx.dense) else {
        // too small for cards: one line per GPU
        let lines: Vec<Line<'static>> = s.gpus.iter().map(|g| fit_line(vec![Span::styled(format!("GPU{} ", g.sample.index), bold()), Span::styled(lss_core::units::temp_opt(g.sample.temp_c, lss_core::units::TempStyle::Compact), temp_style(g.sample.temp_c, hot)), Span::raw(format!(" {} {}", num(g.sample.power_w, "W"), num(g.sample.util_pct, "%")))], inner.width as usize)).collect();
        let used = (lines.len() as u16).min(inner.height);
        render_lines(f, inner, lines);
        return inner.y + used;
    };
    let cw = grid.w.saturating_sub(2) as usize;
    let problem = gpu_problem(s);
    let cards: Vec<Card> = s
        .gpus
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let x = &g.sample;
            let thermal = x.thermal_throttled() || x.throttle_mask & lss_core::gpu::THROTTLE_HW_SLOWDOWN != 0;
            let star = if g.thermal_excluded { "*" } else { "" };
            let tstyle = if g.thermal_excluded { Style::default() } else { temp_style(x.temp_c, hot) };
            let throttle = Span::styled(crate::plain::throttle_label(&g.throttle), if thermal { red() } else { dim() });
            let temps = s.series.gpu_temp_c.get(i).map(|t| sparkline(t, cw.saturating_sub(8))).unwrap_or_default();
            let (title, lines) = if cw >= 24 {
                (
                    vec![Span::styled(format!("GPU{}{star}", x.index), bold()), Span::raw(" "), Span::styled(lss_core::units::temp_opt(x.temp_c, lss_core::units::TempStyle::Compact), tstyle.add_modifier(Modifier::BOLD)), Span::raw(format!("  {}/{}", num(x.power_w, ""), num(x.power_limit_w, "W")))],
                    vec![
                        Line::from(vec![label("util "), Span::raw(format!("{} {}", gauge(x.util_pct.unwrap_or(0.0) / 100.0, 6), num(x.util_pct, "%")))]),
                        Line::from(vec![label("mem  "), Span::raw(format!("{}/{} GiB", gib(x.mem_used_mib), gib(x.mem_total_mib)))]),
                        Line::from(vec![label("clk  "), Span::raw(format!("{}  ", num(x.clock_mhz, " MHz"))), throttle.clone()]),
                        Line::from(vec![label("temp 1h "), Span::styled(temps, fg(Color::Green))]),
                    ],
                )
            } else if cw >= 12 {
                (
                    vec![Span::styled(format!("GPU{}{star}", x.index), bold())],
                    vec![
                        Line::from(vec![Span::styled(lss_core::units::temp_fit(x.temp_c, cw), tstyle.add_modifier(Modifier::BOLD))]),
                        Line::raw(format!("{}/{} · {}", num(x.power_w, ""), num(x.power_limit_w, "W"), num(x.util_pct, "%"))),
                        Line::raw(format!("{} GiB", gib(x.mem_used_mib))),
                        Line::from(vec![throttle.clone()]),
                    ],
                )
            } else {
                (
                    vec![Span::styled(format!("GPU{}{star}", x.index), bold())],
                    vec![Line::from(vec![Span::styled(lss_core::units::temp_fit(x.temp_c, cw), tstyle)]), Line::raw(num(x.power_w, "W")), Line::raw(num(x.util_pct, "%")), Line::raw(format!("{}G", gib(x.mem_used_mib)))],
                )
            };
            // a dense narrow card has room for one thing in its border: keep the name there, the temperature inside
            Card { title, lines, colour: if problem(g) { Color::Red } else { Color::Green } }
        })
        .collect();
    let bottom = draw_cards(f, inner, &grid, cards, super::palette(cx.app.light).fg);
    if bottom < inner.y + inner.height {
        render_lines(f, Rect { y: bottom, height: 1, ..inner }, vec![gpu_totals(s, inner.width as usize)]);
        return bottom + 1;
    }
    bottom
}

/// #31, 2026-09-21: the in-flight budget (and how close it is) was already enforced but never
/// shown - a lane with no in-flight budget (public) still gets a plain token count.
fn inflight_frag(l: &LaneStatus) -> String {
    match l.budget_tokens {
        Some(b) if b > 0 => format!("{} of {} tok ({:.0}%)", l.inflight_tokens, b, l.inflight_tokens as f64 / b as f64 * 100.0),
        _ => format!("{} tok", l.inflight_tokens),
    }
}

/// `other` = the lane in the neighbouring card: both cards use the same wording.
fn lane_card(name: &str, l: &LaneStatus, other: &LaneStatus, cw: usize) -> Card {
    let five = if l.codes_10m.c5xx > 0 { red() } else { Style::default() };
    let wait = if l.waiters > 0 { warn() } else { Style::default() };
    let title = vec![Span::styled(name.to_string(), bold()), Span::raw(format!(" run {:.0} · q {:.0}", l.running, l.queued))];
    let lines = if cw >= 50 {
        vec![
            Line::from(vec![label("waiters "), Span::styled(l.waiters.to_string(), wait), label("   inflight "), Span::raw(inflight_frag(l)), label("   admitted "), Span::raw(l.admitted.to_string())]),
            Line::from(vec![label("10 min   2xx "), Span::raw(l.codes_10m.c2xx.to_string()), label("   4xx "), Span::raw(l.codes_10m.c4xx.to_string()), label("   5xx "), Span::styled(l.codes_10m.c5xx.to_string(), five), label("   requests "), Span::raw(l.requests_10m.to_string())]),
            Line::from(vec![label("rejected 413 "), Span::raw(l.rejected_413.to_string()), label("   429 "), Span::raw(l.rejected_429.to_string()), label("   client closed "), Span::raw(l.client_closed.to_string()), label("   upstream down "), Span::styled(l.upstream_down.to_string(), if l.upstream_down > 0 { warn() } else { Style::default() })]),
            Line::from(vec![label("keys 60m "), Span::raw(fit(&top_keys(l), cw.saturating_sub(9)))]),
        ]
    } else if cw >= 26 {
        vec![
            Line::from(vec![label("waiters "), Span::styled(l.waiters.to_string(), wait), label("  inflight "), Span::raw(inflight_frag(l))]),
            Line::from(vec![label("10m  2xx "), Span::raw(l.codes_10m.c2xx.to_string()), label("  4xx "), Span::raw(l.codes_10m.c4xx.to_string()), label("  5xx "), Span::styled(l.codes_10m.c5xx.to_string(), five)]),
            Line::from(vec![label("keys "), Span::raw(fit(&top_keys(l), cw.saturating_sub(5)))]),
            Line::from(vec![label("rejected 413 "), Span::raw(l.rejected_413.to_string()), label("  429 "), Span::raw(l.rejected_429.to_string())]),
        ]
    } else {
        let codes = |x: &LaneStatus, head: &'static str, sep: &'static str| -> Vec<Span<'static>> {
            let five = if x.codes_10m.c5xx > 0 { red() } else { Style::default() };
            vec![label(head), label("2xx"), Span::raw(format!("{sep}{} ", x.codes_10m.c2xx)), label("4xx"), Span::raw(format!("{sep}{} ", x.codes_10m.c4xx)), label("5xx"), Span::styled(format!("{sep}{}", x.codes_10m.c5xx), five)]
        };
        let forms = [("10m ", " "), ("", " "), ("", "")];
        let pick = forms.iter().position(|(h, sep)| spans_width(&codes(l, h, sep)) <= cw && spans_width(&codes(other, h, sep)) <= cw).unwrap_or(forms.len() - 1);
        vec![
            first_fit(vec![vec![label("waiters "), Span::styled(l.waiters.to_string(), wait), label(" · infl "), Span::raw(l.inflight_tokens.to_string())], vec![label("w "), Span::styled(l.waiters.to_string(), wait), label(" infl "), Span::raw(l.inflight_tokens.to_string())]], cw),
            fit_line(codes(l, forms[pick].0, forms[pick].1), cw),
            Line::raw(fit(&top_keys(l), cw)),
        ]
    };
    Card { title, lines, colour: Color::Yellow }
}

/// LANES as plain lines (a narrow box): per lane what is running / queued / waiting and the
/// last ten minutes of status codes, then the gate and the probes. When rows are short the
/// least important lines go first; the lanes themselves always stay.
fn lane_lines(cx: &Ctx, w: usize, rows: usize) -> Vec<Line<'static>> {
    let s = cx.s;
    // with fewer than five rows the status codes have no line of their own: they ride here
    let inline_codes = rows < 5;
    let head = |name: &str, l: &LaneStatus| {
        let wait = if l.waiters > 0 { warn() } else { Style::default() };
        let mut variants = Vec::new();
        if inline_codes {
            let five = if l.codes_10m.c5xx > 0 { red() } else { Style::default() };
            variants.push(vec![Span::styled(format!("{name:<8} "), bold()), label("running "), Span::raw(format!("{:.0}", l.running)), label(" · queued "), Span::raw(format!("{:.0}", l.queued)), label(" · 10 min 2xx "), Span::raw(l.codes_10m.c2xx.to_string()), label(" 4xx "), Span::raw(l.codes_10m.c4xx.to_string()), label(" 5xx "), Span::styled(l.codes_10m.c5xx.to_string(), five)]);
        }
        variants.extend(
            vec![
                vec![Span::styled(format!("{name:<8} "), bold()), label("running "), Span::raw(format!("{:.0}", l.running)), label(" · queued "), Span::raw(format!("{:.0}", l.queued)), label(" · waiting "), Span::styled(l.waiters.to_string(), wait)],
                vec![Span::styled(format!("{name:<8} "), bold()), label("run "), Span::raw(format!("{:.0}", l.running)), label(" · queue "), Span::raw(format!("{:.0}", l.queued)), label(" · wait "), Span::styled(l.waiters.to_string(), wait)],
                vec![Span::styled(format!("{name:<8} "), bold()), Span::raw(format!("run {:.0} q {:.0} w {}", l.running, l.queued, l.waiters))],
            ],
        );
        first_fit(variants, w)
    };
    let codes = |l: &LaneStatus| {
        let five = if l.codes_10m.c5xx > 0 { red() } else { Style::default() };
        let build = |lead: &'static str, gap: &'static str| vec![label(lead), Span::raw(l.codes_10m.c2xx.to_string()), label(&format!("{gap}4xx ")), Span::raw(l.codes_10m.c4xx.to_string()), label(&format!("{gap}5xx ")), Span::styled(l.codes_10m.c5xx.to_string(), five)];
        first_fit(vec![build("         10 min  2xx ", "  "), build("  10 min 2xx ", " "), build("  2xx ", " ")], w)
    };
    let (p, t) = (&s.lanes.public, &s.lanes.trusted);
    let gate = if s.gate.up {
        let build = |word: &'static str| vec![label("gate "), Span::styled(format!("UP {}", s.gate.version.as_deref().unwrap_or("")), good()), label(word), Span::raw((p.rejected_413 + t.rejected_413).to_string()), label(" · 429 "), Span::raw((p.rejected_429 + t.rejected_429).to_string())];
        (4, first_fit(vec![build(" · rejected 413 "), build(" · 413 ")], w))
    } else {
        let since = s.gate.down_since.map_or_else(String::new, |at| format!(" since {}", fmt_local(at, "%H:%M:%S")));
        (1, Line::styled(fit(&format!("GATE DOWN{since} - the public endpoint is dark"), w), red()))
    };
    let probes_long = crate::plain::probes_label(&s.lanes.trusted);
    let probes = Line::styled(if probes_long.chars().count() <= w { probes_long } else { fit(&format!("probes: {} excluded", s.lanes.trusted.probes.admitted), w) }, dim());
    let all: Vec<(u8, Line<'static>)> = vec![(0, head("public", p)), (3, codes(p)), (0, head("trusted", t)), (3, codes(t)), gate, (2, probes)];
    let mut keep: Vec<u8> = all.iter().map(|(k, _)| *k).collect();
    keep.sort_unstable();
    keep.truncate(rows);
    let cut = keep.last().copied().unwrap_or(0);
    let mut room = rows;
    all.into_iter()
        .filter(|(k, _)| {
            let take = *k <= cut && room > 0;
            room -= usize::from(take);
            take
        })
        .map(|(_, l)| l)
        .collect()
}

/// Returns the first row under what was drawn (the rest of the box is the caller's to fill).
fn draw_lanes(f: &mut Frame, cx: &Ctx, inner: Rect) -> u16 {
    let s = cx.s;
    if s.gate.absent {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for part in ["no gateway configured: requests go straight to the engine.", "Lanes, per-user numbers and rejections need one in front of it (gate_url in the collector's config)."] {
            lines.extend(super::widgets::wrap(part, inner.width as usize).into_iter().map(|l| Line::styled(l, dim())));
        }
        lines.truncate(inner.height as usize);
        let used = lines.len() as u16;
        render_lines(f, inner, lines);
        return inner.y + used;
    }
    let grid = if lane_lines_mode(inner) { None } else { lane_grid(inner, cx.dense) };
    let Some(grid) = grid else {
        // narrow, or too low for cards: plain lines
        let lines = lane_lines(cx, inner.width as usize, inner.height as usize);
        let used = lines.len() as u16;
        render_lines(f, inner, lines);
        return inner.y + used;
    };
    let probes = Line::styled(fit(&format!("{} · codes = last 10 min", crate::plain::probes_label(&s.lanes.trusted)), inner.width as usize), dim());
    let cw = grid.w.saturating_sub(2) as usize;
    let cards = vec![lane_card("public", &s.lanes.public, &s.lanes.trusted, cw), lane_card("trusted", &s.lanes.trusted, &s.lanes.public, cw)];
    let bottom = draw_cards(f, inner, &grid, cards, super::palette(cx.app.light).fg);
    // what goes under the cards, by importance: (priority, line). The probes line always shows.
    let w = inner.width as usize;
    let mut tail: Vec<(u8, Line<'static>)> = Vec::new();
    if !s.gate.up {
        let since = s.gate.down_since.map_or_else(String::new, |t| format!(" since {}", fmt_local(t, "%H:%M:%S")));
        tail.push((1, Line::styled(fit(&format!("GATE DOWN{since} - the public endpoint is dark"), w), red())));
    } else {
        let (p, t) = (&s.lanes.public, &s.lanes.trusted);
        tail.push((2, fit_line(vec![
            label("gate "), Span::styled(format!("UP {}", s.gate.version.as_deref().unwrap_or("")), good()),
            label("  rejected 413 "), Span::raw((p.rejected_413 + t.rejected_413).to_string()),
            label("  429 "), Span::raw((p.rejected_429 + t.rejected_429).to_string()),
            label("  client closed "), Span::raw((p.client_closed + t.client_closed).to_string()),
            label("  upstream down "), Span::raw((p.upstream_down + t.upstream_down).to_string()),
        ], w)));
    }
    tail.push((0, probes));
    if cw < 26 {
        // the narrow cards had no room for their keys
        tail.push((3, fit_line(vec![label("keys 60m public  "), Span::raw(top_keys(&s.lanes.public))], w)));
        tail.push((4, fit_line(vec![label("keys 60m trusted "), Span::raw(top_keys(&s.lanes.trusted))], w)));
    }
    let room = (inner.y + inner.height).saturating_sub(bottom) as usize;
    let mut keep: Vec<u8> = tail.iter().map(|(p, _)| *p).collect();
    keep.sort_unstable();
    keep.truncate(room);
    let lines: Vec<Line> = tail.into_iter().filter(|(p, _)| keep.contains(p)).map(|(_, l)| l).collect();
    let used = lines.len() as u16;
    render_lines(f, Rect { y: bottom, height: room as u16, ..inner }, lines);
    bottom + used
}

/// The most pressing severity among the overview's advice.
fn worst(s: &Status) -> Option<Severity> {
    s.advice_top.iter().map(|f| f.severity).max()
}

/// ACT is a problem (red), WATCH is a warning, FINE is plain.
pub fn severity_colour(sev: Severity) -> Style {
    match sev {
        Severity::Act => red().add_modifier(Modifier::BOLD),
        Severity::Watch => warn().add_modifier(Modifier::BOLD),
        Severity::Fine => good(),
    }
}

/// USERS: who is on the server right now, the slots they take, and the three busiest.
fn users_lines(cx: &Ctx, inner: Rect) -> Vec<Line<'static>> {
    let s = cx.s;
    let w = inner.width as usize;
    let u = &s.users;
    let slots = s.serve.slots.max(1);
    let full = s.serve.running >= f64::from(slots);
    let slots_style = if full { warn().add_modifier(Modifier::BOLD) } else { bold() };
    let slots_text = format!("{} {:.0}/{}", gauge(s.serve.running / f64::from(slots), 8), s.serve.running, s.serve.slots);
    if s.serve.slots == 0 || s.serve.is_na("running") {
        // how many run at once is not known: say so instead of drawing a gauge of nothing
        let why = s.serve.na("running").unwrap_or_else(|| "n/a (set `slots` in the collector's config)".to_string());
        let mut lines = vec![first_fit(vec![vec![label("slots    "), Span::styled(why, dim())], vec![label("slots "), Span::styled("n/a", dim())]], w)];
        lines.push(fit_line(vec![Span::styled(if s.gate.absent { "no gateway configured: nothing tells users apart" } else { "who is using them: needs gateway v5.2" }, dim())], w));
        return lines;
    }
    let gauge_line = first_fit(vec![vec![label("slots    "), Span::styled(slots_text.clone(), slots_style), label("  in use now")], vec![label("slots "), Span::styled(slots_text.clone(), slots_style), label(" in use")], vec![label("slots "), Span::styled(slots_text, slots_style)]], w);
    if s.gate.absent {
        return vec![gauge_line, fit_line(vec![Span::styled("no gateway configured: nothing tells users apart", dim())], w)];
    }
    if !u.available {
        return vec![gauge_line, fit_line(vec![Span::styled("who is using them: needs gateway v5.2", dim())], w), fit_line(vec![Span::styled("until then the GATEWAY page has requests per key", dim())], w)];
    }
    let mut lines = vec![first_fit(
        vec![
            vec![label("active   "), Span::styled(format!("{} now", u.active_now), bold()), Span::raw(format!(" · {} in 10 min · {} in 24 h", u.totals.users_active_10m, u.totals.users_24h))],
            vec![Span::styled(format!("{} now", u.active_now), bold()), Span::raw(format!(" · {} 10m · {} 24h", u.totals.users_active_10m, u.totals.users_24h))],
        ],
        w,
    )];
    let rows = inner.height as usize;
    if rows >= 3 {
        lines.push(gauge_line);
    }
    let room = rows.saturating_sub(lines.len());
    for r in u.rows.iter().take(room.min(3)) {
        let running = if r.gate.inflight > 0 { Span::styled(format!("{:>2} running", r.gate.inflight), bold()) } else { Span::styled(" idle     ".to_string(), dim()) };
        let requests = Span::styled(format!("  {} req/24h", r.gate.requests_24h), dim());
        lines.push(first_fit(vec![vec![Span::raw(format!("{:<14.14} ", r.name)), running.clone(), requests.clone()], vec![Span::raw(format!("{:<9.9} ", r.name)), running.clone(), requests], vec![Span::raw(format!("{:<9.9} ", r.name)), running]], w));
    }
    if u.rows.is_empty() && room > 0 {
        lines.push(Line::styled("nobody in the last 24 hours", dim()));
    }
    lines
}

/// ADVICE: the two most pressing findings, wrapped, severity first.
fn advice_lines(cx: &Ctx, inner: Rect) -> Vec<Line<'static>> {
    let w = inner.width as usize;
    let rows = inner.height as usize;
    let top = &cx.s.advice_top;
    if top.is_empty() {
        return vec![Line::styled(fit("collecting evidence: the first advice appears within 5 minutes", w), dim())];
    }
    // the first finding takes what it needs; every later one is still promised a line
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (k, f) in top.iter().enumerate() {
        let later = top.len() - k - 1;
        let budget = rows.saturating_sub(lines.len()).saturating_sub(later).max(usize::from(lines.len() < rows));
        let tag = format!("{:<5} ", f.severity.as_str().to_uppercase());
        let mut wrapped = super::widgets::wrap(&f.sentence, w.saturating_sub(tag.len()).max(8));
        if wrapped.len() > budget && budget > 0 {
            // the sentence goes on: say so, instead of stopping as if it had ended
            wrapped.truncate(budget);
            let last = wrapped.pop().unwrap_or_default();
            wrapped.push(fit(&format!("{last} …"), w.saturating_sub(tag.len()).max(8)));
        }
        for (i, part) in wrapped.into_iter().take(budget).enumerate() {
            let lead = if i == 0 { Span::styled(tag.clone(), severity_colour(f.severity)) } else { Span::raw(" ".repeat(tag.len())) };
            lines.push(fit_line(vec![lead, Span::raw(part)], w));
        }
    }
    lines.truncate(rows);
    lines
}

fn incident_lines(cx: &Ctx, inner: Rect) -> Vec<Line<'static>> {
    let w = inner.width as usize;
    let mut lines: Vec<Line> = cx
        .s
        .incidents
        .iter()
        .take(inner.height as usize)
        .map(|i| {
            let is_maintenance = i.kind == lss_core::incidents::KIND_MAINTENANCE;
            let (span, style) = match i.end {
                // a maintenance window OPEN is deliberate, not an outage in progress - the same
                // red as a real one would be exactly the "looks like a crash" card #22 exists to
                // avoid
                None if is_maintenance => (format!("OPEN {}", fmt_duration(cx.now - i.start)), good()),
                None => (format!("OPEN {}", fmt_duration(cx.now - i.start)), red()),
                Some(e) if e > i.start => (fmt_duration(e - i.start), Style::default()),
                Some(_) => ("-".to_string(), dim()),
            };
            let kind_style = if i.kind == "xid" { warn() } else if is_maintenance { good() } else { Style::default() };
            if w < 80 {
                // a narrow box: no padded columns, so the words get the room
                let mut v = vec![Span::styled(fmt_local(i.start, "%m-%d %H:%M "), dim()), Span::styled(i.kind.clone(), kind_style)];
                if span != "-" {
                    v.push(Span::styled(format!(" {span}"), style));
                }
                v.push(Span::styled(format!("  {}", i.detail), dim()));
                return fit_line(v, w);
            }
            fit_line(vec![Span::styled(fmt_local(i.start, "%m-%d %H:%M "), dim()), Span::styled(format!("{:<18}", i.kind), kind_style), Span::styled(format!("{span:>9}  "), style), Span::raw(i.detail.clone())], w)
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::styled("none in the last 7 days", dim()));
    }
    lines
}

fn severity_style(sev: &str, recovered: bool) -> Style {
    if recovered {
        return good();
    }
    match sev {
        "page" | "hardware" => red(),
        "warn" => warn(),
        _ => Style::default(),
    }
}

fn alert_lines(cx: &Ctx, inner: Rect) -> Vec<Line<'static>> {
    let s = cx.s;
    let w = inner.width as usize;
    let mut lines = Vec::new();
    if !s.firing.is_empty() {
        lines.push(fit_line(vec![Span::styled(format!("FIRING {}", s.firing.join(", ")), red().add_modifier(Modifier::BOLD))], w));
    }
    for a in s.alerts.iter().take((inner.height as usize).saturating_sub(lines.len())) {
        let severity = if w < 60 { format!("{} ", a.severity) } else { format!("{:<9}", a.severity) };
        let mut spans = vec![Span::styled(fmt_local(a.ts, "%m-%d %H:%M "), dim()), Span::styled(severity, severity_style(&a.severity, a.recovered))];
        if !a.delivered {
            // before the message: the tail of a long line is the first thing a narrow pane cuts
            spans.push(Span::styled("[undelivered] ", warn()));
        }
        spans.push(Span::raw(a.message.clone()));
        lines.push(fit_line(spans, w));
    }
    if s.alerts.is_empty() {
        lines.push(Line::styled("no alerts recorded", dim()));
    }
    lines
}
