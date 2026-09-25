//! The visual vocabulary, shared with Terminal Board: every area is a box whose title sits IN
//! its top border (`┌ o TITLE (detail) ───┐`), colours are the terminal's named ANSI colours (so
//! a 16-colour terminal still shows 8 distinct hues, never one shade smeared into another, and
//! both a dark and a light theme keep contrast - see `GPU_COLOURS`/`USER_COLOURS` in pages.rs),
//! the focused box is THICK, body text is plain, RED means a problem and nothing else, and every
//! glyph is single-width (box drawing, `▁▂▃▄▅▆▇█`, braille).
//!
//! #48, 2026-09-21: multi-series charts (`draw_chart`) can draw CONNECTED LINES in box-drawing
//! glyphs (`─│╭╮╰╯`), one small panel PER SERIES (small multiples: the owner picked this over
//! several lines overlaid in one box because lines never cross) - instead of braille, which can
//! only carry one colour per cell and merges overlaid series into an unreadable smear. Which one
//! draws is a runtime choice (`App::chart_lines`, the `c` key, default dots - see `draw_chart`'s
//! doc comment), not a switch in this file. The braille renderer (`chart_rows_braille`) stays in
//! the tree either way. Single-series sparklines (`sparkline`) are untouched - see that
//! function's own doc comment for why.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

pub const RED: Color = Color::Red;

pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn red() -> Style {
    Style::default().fg(RED)
}

pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

pub fn good() -> Style {
    Style::default().fg(Color::Green)
}

pub fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

/// Cut to `width` characters; a cut string ends in `…`.
pub fn fit(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = width - 1;
    // never cut inside a number or a quantity (`2m0…`, `12.…`): the whole token goes instead,
    // because half a number reads as a different number
    let (last, next) = (chars[keep.saturating_sub(1)], chars[keep]);
    if keep > 0 && (last.is_ascii_digit() || next.is_ascii_digit()) && !last.is_whitespace() && !next.is_whitespace() {
        let token_start = chars[..keep].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
        if token_start > 0 {
            keep = token_start;
        }
    }
    let mut out: String = chars[..keep].iter().collect::<String>().trim_end().to_string();
    out.push('…');
    out
}

/// Word wrap to `width` columns (a word longer than the line is cut).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word: String = word.to_string();
        loop {
            let used = cur.chars().count();
            let need = word.chars().count() + usize::from(used > 0);
            if used + need <= width {
                if used > 0 {
                    cur.push(' ');
                }
                cur.push_str(&word);
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            // a single word wider than the line
            let head: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(head);
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

pub fn spans_len(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Cut a line of spans to `width` characters (the span that is cut gets the ellipsis).
pub fn fit_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut left = width;
    for s in spans {
        if left == 0 {
            break;
        }
        let n = s.content.chars().count();
        if n <= left {
            left -= n;
            out.push(s);
        } else {
            out.push(Span::styled(fit(&s.content, left), s.style));
            left = 0;
        }
    }
    Line::from(out)
}

/// A frame in `colour`, thick when focused.
pub fn frame(thick: bool, colour: Color) -> Block<'static> {
    Block::default().borders(Borders::ALL).border_type(if thick { BorderType::Thick } else { BorderType::Plain }).border_style(Style::default().fg(colour))
}

/// ` o TITLE (detail) ` for the top border: the dot and the title take the box's colour.
pub fn title_line(title: &str, detail: Vec<Span<'static>>, colour: Color, room: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" "), Span::styled("o", fg(colour)), Span::styled(format!(" {title}"), bold().fg(colour))];
    if !detail.is_empty() {
        spans.push(Span::styled(" (", bold().fg(colour)));
        for mut d in detail {
            if d.style.fg.is_none() {
                d.style = d.style.fg(colour);
            }
            d.style = d.style.add_modifier(Modifier::BOLD);
            spans.push(d);
        }
        spans.push(Span::styled(")", bold().fg(colour)));
    }
    spans.push(Span::raw(" "));
    if spans_len(&spans) > room {
        let bare = vec![Span::raw(" "), Span::styled("o", fg(colour)), Span::styled(format!(" {title} "), bold().fg(colour))];
        let bare_len = spans_len(&bare);
        if room >= bare_len + 8 {
            // cut the detail, keep its closing bracket: `o TITLE (detail…) `
            let mut cut = fit_line(spans, room - 2).spans;
            cut.push(Span::styled(")", bold().fg(colour)));
            cut.push(Span::raw(" "));
            spans = cut;
        } else if room >= bare_len {
            spans = bare;
        } else {
            spans = vec![Span::styled(fit(title, room), bold().fg(colour))];
        }
    }
    Line::from(spans)
}

/// Draws the box and returns its inside.
pub fn draw_box(f: &mut Frame, area: Rect, title: &str, detail: Vec<Span<'static>>, colour: Color, focused: bool) -> Rect {
    let block = frame(focused, colour).title(title_line(title, detail, colour, area.width.saturating_sub(2) as usize));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// `[##------]`
pub fn gauge(frac: f64, width: usize) -> String {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let mut filled = (frac * width as f64).round() as usize;
    if frac > 0.0 && filled == 0 {
        filled = 1; // something is running: never draw it as nothing
    }
    format!("[{}{}]", "#".repeat(filled.min(width)), "-".repeat(width - filled.min(width)))
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Text sparkline, newest value on the right. More points than columns: each column is the max
/// of its slice (a spike must stay visible); gaps are spaces.
///
/// #48, 2026-09-21 (card requirement 6): this is NOT braille and is unchanged by that card - it
/// is one bar-glyph column per value, ONE series, no colour and no line to overlay or merge. It
/// is used for the overview's compact per-metric readouts (`writing 412.5 tok/s  ▁▁▁██▁`) and
/// small unfocused panes, where there is nothing to confuse: a single self-contained shape at a
/// size too small for an axis. The problem card #48 fixes - several series losing their colour
/// when overlaid - cannot happen here because there is only ever one thing being drawn.
pub fn sparkline(data: &[Option<f64>], width: usize) -> String {
    sparkline_to(data, width, None)
}

/// Same, against a fixed top (`Some(1.0)` for a 0..1 ratio, the slot count for running
/// requests): an idle hour must look idle, not like a full-height wiggle.
pub fn sparkline_to(data: &[Option<f64>], width: usize, top: Option<f64>) -> String {
    if width == 0 || data.is_empty() {
        return String::new();
    }
    let cols = resample_max(data, width.min(data.len()));
    let seen = cols.iter().flatten().copied().fold(0.0_f64, f64::max);
    let max = top.map_or(seen, |t| t.max(seen));
    cols.iter()
        .map(|v| match v {
            None => ' ',
            Some(_) if max <= 0.0 => BARS[0],
            Some(x) => BARS[(((x / max) * 7.0).round() as usize).min(7)],
        })
        .collect()
}

/// A chart made of block glyphs, `rows` tall: what fills the rows a box has left over, so a
/// tall pane shows the last hour instead of blank space. `lo..hi` is the scale (`hi` None = the
/// largest value seen); a bucket with no data is a gap; a bucket with data always shows at least
/// the lowest block, so "zero" and "no data" never look the same. Top row first.
pub fn block_chart(data: &[Option<f64>], width: usize, rows: usize, lo: f64, hi: Option<f64>) -> Vec<String> {
    if width == 0 || rows == 0 || data.is_empty() {
        return Vec::new();
    }
    const GLYPHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let cols = resample_max(data, width.min(data.len()));
    let seen = cols.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
    let top = hi.unwrap_or(seen).max(lo + f64::EPSILON);
    let steps = rows * 8;
    let levels: Vec<Option<usize>> = cols.iter().map(|c| c.map(|v| ((((v - lo) / (top - lo)).clamp(0.0, 1.0) * steps as f64).round() as usize).max(1))).collect();
    (0..rows)
        .map(|r| {
            let base = (rows - 1 - r) * 8;
            levels.iter().map(|l| l.map_or(' ', |l| GLYPHS[l.saturating_sub(base).min(8)])).collect()
        })
        .collect()
}

/// `data` squeezed into `width` columns, each the max of its slice.
pub fn resample_max(data: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    if width == 0 || data.is_empty() {
        return Vec::new();
    }
    if data.len() <= width {
        return data.to_vec();
    }
    (0..width)
        .map(|c| {
            let a = c * data.len() / width;
            let b = ((c + 1) * data.len() / width).max(a + 1).min(data.len());
            data[a..b].iter().flatten().copied().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v))))
        })
        .collect()
}

/// A 1-line bar for a panel that does not fit: `GPUS 52/55/61/58C · 742W   enter >`. The focused
/// bar is reversed.
pub fn bar_line(label: &str, mut body: Vec<Span<'static>>, focused: bool, width: usize) -> Line<'static> {
    let base = if focused { bold().add_modifier(Modifier::REVERSED) } else { Style::default() };
    // #222: this used to say "tab >", which was true when Tab opened the focused box (arrows
    // move focus, Tab/Enter both opened it). Tab is now the global page-cycling key everywhere,
    // including here - Enter alone opens a focused bar, so the hint has to say the key that
    // still does that, not the one that now does something else.
    let hint = "   enter >";
    let mut spans = vec![Span::styled(format!(" {label} "), bold().patch(base))];
    for s in &mut body {
        s.style = s.style.patch(base);
    }
    let room = width.saturating_sub(label.chars().count() + 2 + hint.len());
    spans.extend(fit_line(body, room).spans);
    spans.push(Span::styled(hint, dim().patch(base)));
    fit_line(spans, width)
}

pub fn render_lines(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// One series of a chart.
pub struct Series {
    pub label: String,
    pub colour: Color,
    pub data: Vec<Option<f64>>,
    /// scaled on its own min..max instead of the chart's axis (its legend entry says so)
    pub own_scale: bool,
    /// sparse events (one probe every few minutes): join them across the gaps, and draw each
    /// one fat enough to be seen
    pub sparse: bool,
}

impl Series {
    pub fn new(label: &str, colour: Color, data: &[Option<f64>]) -> Self {
        Self { label: label.to_string(), colour, data: data.to_vec(), own_scale: false, sparse: false }
    }
    pub fn last(&self) -> Option<f64> {
        self.data.iter().rev().flatten().next().copied()
    }
}

/// A mark on the time axis (an Xid, an invalid probe).
pub struct Marker {
    /// 0..1 along the time axis
    pub at: f64,
    pub glyph: char,
    pub style: Style,
}

pub struct ChartOpts {
    pub fmt: fn(f64) -> String,
    /// pin the bottom of the axis (0 for rates and counts)
    pub y_min: Option<f64>,
    pub y_max: Option<f64>,
    /// left end of the time axis, e.g. `-6h`
    pub x_left: String,
    pub markers: Vec<Marker>,
    /// more points than dot columns: keep each column's PEAK (queues, temperatures) instead of
    /// its mean (rates)
    pub peak: bool,
}

/// `data` squeezed into `width` columns, each the mean of its slice.
pub fn resample_mean(data: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    if width == 0 || data.len() <= width {
        return data.to_vec();
    }
    (0..width)
        .map(|c| {
            let a = c * data.len() / width;
            let b = ((c + 1) * data.len() / width).max(a + 1).min(data.len());
            let v: Vec<f64> = data[a..b].iter().flatten().copied().collect();
            (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
        })
        .collect()
}

struct Canvas {
    w: usize,
    h: usize,
    bits: Vec<u8>,
    colour: Vec<Option<Color>>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, bits: vec![0; w * h], colour: vec![None; w * h] }
    }

    /// (x, y) in DOT coordinates: 2 per cell across, 4 per cell down.
    fn dot(&mut self, x: i64, y: i64, c: Color) {
        if x < 0 || y < 0 || x >= (self.w * 2) as i64 || y >= (self.h * 4) as i64 {
            return;
        }
        let (cx, cy) = ((x / 2) as usize, (y / 4) as usize);
        let bit = match (x % 2, y % 4) {
            (0, 0) => 0x01,
            (0, 1) => 0x02,
            (0, 2) => 0x04,
            (1, 0) => 0x08,
            (1, 1) => 0x10,
            (1, 2) => 0x20,
            (0, 3) => 0x40,
            _ => 0x80,
        };
        self.bits[cy * self.w + cx] |= bit;
        self.colour[cy * self.w + cx] = Some(c);
    }

    fn segment(&mut self, a: (i64, i64), b: (i64, i64), c: Color) {
        let (mut x, mut y) = a;
        let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
        let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.dot(x, y, c);
            if (x, y) == b {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

fn bounds(data: &[Option<f64>]) -> Option<(f64, f64)> {
    let mut it = data.iter().flatten().copied().filter(|v| v.is_finite());
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
}

/// #48, 2026-09-21: a braille cell carries one colour, so overlaid series merged into a
/// single-coloured smear (TEMPERATURE with 4 GPUs was unreadable). Switch to connected
/// box-drawing lines, one small panel PER SERIES (small multiples - lines never cross).
/// 2026-09-21, later the same day: the owner wants to look at both with his own eyes, so this
/// is a RUNTIME choice now (`App::chart_lines`, precedence: `$LSS_CHART` beats the saved
/// `chart` pref, which beats the default - see `prefs::resolve_chart_lines`), not a
/// compile-time const. The default is dots: the owner opts in with the `c` key, never opted in
/// for him. The braille renderer stays in the tree either way, exactly for this: a size or a
/// chart the line renderer is not clearly better at can still be switched back to braille,
/// without deleting anything.
///
/// The chart a `Body::Chart` section draws: dispatches to the line renderer or the braille one
/// depending on `lines`. Both share the same y-axis (max/mid/min, shared scale across series
/// that are not `own_scale`), the same time axis (`-6h … now`, markers on it) and the same
/// legend line.
pub fn draw_chart(f: &mut Frame, area: Rect, series: &[Series], opts: &ChartOpts, lines: bool) {
    render_lines(f, area, chart_rows(area.width as usize, area.height as usize, series, opts, lines));
}

/// The rows `draw_chart` would draw into a `width` x `height` area, as plain `Line`s - for a page
/// that composes its boxes as lines rather than frame areas (page 1's boxes, #305: the owner asked
/// for its 1h trends "like the other charts i see in gpus tab 3", so they are THIS widget, not a
/// look-alike). Empty when the area is too small to draw anything.
pub fn chart_rows(width: usize, height: usize, series: &[Series], opts: &ChartOpts, lines: bool) -> Vec<Line<'static>> {
    if lines { chart_rows_lines(width, height, series, opts) } else { chart_rows_braille(width, height, series, opts) }
}

/// Braille line chart with a y axis (max / mid / min), a time axis (`-6h … now`, markers on it)
/// and a legend line carrying each series' newest value. Degrades to sparklines when the area
/// is too small for an axis. Kept undeleted (card #48's ship gate: a size or chart the line
/// renderer is not clearly better at keeps this - see `draw_chart`'s `lines` parameter).
fn chart_rows_braille(w: usize, height: usize, series: &[Series], opts: &ChartOpts) -> Vec<Line<'static>> {
    if w < 8 || height == 0 {
        return Vec::new();
    }
    let legend = legend_line(series, opts, w);
    if series.iter().all(|s| s.data.iter().all(Option::is_none)) {
        let mut lines = vec![Line::styled("no data in this range yet", dim())];
        if height >= 2 {
            lines.push(legend);
        }
        return lines;
    }
    if height < 4 {
        // no room for axes: one sparkline per series as far as the rows go, legend last
        let mut lines: Vec<Line> = series.iter().take((height).saturating_sub(1).max(1)).map(|s| Line::styled(sparkline(&s.data, w), fg(s.colour))).collect();
        if height >= 2 {
            lines.push(legend);
        }
        return lines;
    }
    // shared axis: everything that is not on its own scale
    let shared: Vec<Option<f64>> = series.iter().filter(|s| !s.own_scale).flat_map(|s| s.data.iter().copied()).collect();
    let (mut lo, mut hi) = bounds(&shared).unwrap_or((0.0, 1.0));
    if let Some(m) = opts.y_min {
        lo = lo.min(m);
    }
    if let Some(m) = opts.y_max {
        hi = hi.max(m);
    }
    if hi - lo < 1e-9 {
        hi = lo + if lo.abs() < 1e-9 { 1.0 } else { lo.abs() * 0.1 };
    }
    let labels = [(opts.fmt)(hi), (opts.fmt)((hi + lo) / 2.0), (opts.fmt)(lo)];
    let gutter = labels.iter().map(|l| l.chars().count()).max().unwrap_or(1) + 1;
    let plot_w = w.saturating_sub(gutter);
    let plot_h = height - 2;
    if plot_w < 4 {
        return vec![legend];
    }
    let mut canvas = Canvas::new(plot_w, plot_h);
    let (dots_w, dots_h) = ((plot_w * 2) as f64, (plot_h * 4) as f64);
    for s in series {
        let (slo, shi) = if s.own_scale {
            let (a, b) = bounds(&s.data).unwrap_or((0.0, 1.0));
            if b - a < 1e-9 { (a - 0.5, a + 0.5) } else { (a, b) }
        } else {
            (lo, hi)
        };
        // one value per dot column at most: a 5 s series over an hour would otherwise paint a band
        let data = if opts.peak { resample_max(&s.data, plot_w * 2) } else { resample_mean(&s.data, plot_w * 2) };
        let n = data.len().max(2);
        let at = |i: usize, v: f64| -> (i64, i64) {
            let x = (i as f64 / (n - 1) as f64 * (dots_w - 1.0)).round() as i64;
            let y = ((1.0 - ((v - slo) / (shi - slo)).clamp(0.0, 1.0)) * (dots_h - 1.0)).round() as i64;
            (x, y)
        };
        let mut prev: Option<(i64, i64)> = None;
        for (i, v) in data.iter().enumerate() {
            match v.filter(|v| v.is_finite()) {
                Some(v) => {
                    let p = at(i, v);
                    canvas.segment(prev.unwrap_or(p), p, s.colour);
                    if s.sparse {
                        for (dx, dy) in [(1, 0), (0, 1), (1, 1), (-1, 0), (0, -1)] {
                            canvas.dot(p.0 + dx, p.1 + dy, s.colour);
                        }
                    }
                    prev = Some(p);
                }
                None if s.sparse => {}
                None => prev = None,
            }
        }
    }
    let mut lines: Vec<Line> = Vec::with_capacity(height);
    for row in 0..plot_h {
        let label = if row == 0 {
            &labels[0]
        } else if row == plot_h - 1 {
            &labels[2]
        } else if row == plot_h / 2 && plot_h >= 5 {
            &labels[1]
        } else {
            ""
        };
        let mut spans = vec![Span::styled(format!("{label:>width$} ", width = gutter - 1), dim())];
        let mut run = String::new();
        let mut run_colour: Option<Color> = None;
        for col in 0..plot_w {
            let i = row * plot_w + col;
            let ch = if canvas.bits[i] == 0 { ' ' } else { char::from_u32(0x2800 + u32::from(canvas.bits[i])).unwrap_or(' ') };
            let c = if canvas.bits[i] == 0 { run_colour } else { canvas.colour[i] };
            if c != run_colour && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_colour.map_or_else(Style::default, fg)));
            }
            run_colour = c;
            run.push(ch);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_colour.map_or_else(Style::default, fg)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(time_axis(gutter, plot_w, opts));
    lines.push(legend);
    lines
}

/// Below this many columns, or below `MIN_PANEL_H` plot rows, a panel is not worth drawing
/// (card #48, and the 2026-09-21 geometry fix after the first ship-gate captures showed a
/// GPUS metric split 4 ways at ~14 cols x 2-3 rows: every line collapsed into a square wave
/// of flat runs and vertical jumps, not a curve - a panel needs room to actually curve).
/// A 2-row panel must never be drawn: `MIN_PANEL_H` alone rules that out, at any width.
pub(crate) const MIN_PANEL_W: usize = 24;
pub(crate) const MIN_PANEL_H: usize = 5;

/// Connected box-drawing lines, ONE PANEL PER SERIES (small multiples). `series.len() <= 1`
/// draws a single full-width panel - there is nothing to confuse, but the PRIMARY ask (a
/// continuous line, not dots) still applies. More than one series splits into panels so lines
/// never cross: side by side while each panel is at least `MIN_PANEL_W` columns wide and the
/// box gives at least `MIN_PANEL_H` plot rows, else a 2-row grid at the same minimums (so a
/// row is itself at least `MIN_PANEL_H` tall - a 2-row panel is a square wave, not a curve,
/// and must never be drawn), else ONE panel for the series reading worst (highest current
/// value) plus a plain sentence naming what the others read. This geometry floor is
/// page-agnostic: the page layer (pages.rs) gives a multi-series chart a full-width, taller
/// box in line mode so this usually doesn't even bite, but the floor holds regardless of what
/// height or width it is handed.
///
/// Every panel shares ONE y-scale (computed exactly as the braille renderer's, over every series
/// that is not `own_scale`) - four panels at four different scales would look comparable and
/// lie. Y labels are drawn on the FIRST (leftmost) panel only; the time axis is drawn once,
/// under the whole box, never once per panel.
///
/// OVERLAP RULE (card #48 item 4): moving to one panel per series removes the old problem by
/// construction - two series can never land in the same cell, because they are never in the same
/// panel. The only thing that can still land in one column is this ONE series' own value: that
/// is resolved before drawing, by `resample_mean`/`resample_max` reducing however many source
/// points fall in a column to its single value - not a rendering-time collision.
fn chart_rows_lines(w: usize, height: usize, series: &[Series], opts: &ChartOpts) -> Vec<Line<'static>> {
    if w < 8 || height == 0 {
        return Vec::new();
    }
    let legend = legend_line(series, opts, w);
    if series.is_empty() || series.iter().all(|s| s.data.iter().all(Option::is_none)) {
        let mut lines = vec![Line::styled("no data in this range yet", dim())];
        if height >= 2 {
            lines.push(legend);
        }
        return lines;
    }
    if height < 4 {
        // no room for axes: one sparkline per series as far as the rows go, legend last (single
        // series or not - at this height there is no room for a panel border either)
        let mut lines: Vec<Line> = series.iter().take((height).saturating_sub(1).max(1)).map(|s| Line::styled(sparkline(&s.data, w), fg(s.colour))).collect();
        if height >= 2 {
            lines.push(legend);
        }
        return lines;
    }
    let shared: Vec<Option<f64>> = series.iter().filter(|s| !s.own_scale).flat_map(|s| s.data.iter().copied()).collect();
    let (mut lo, mut hi) = bounds(&shared).unwrap_or((0.0, 1.0));
    if let Some(m) = opts.y_min {
        lo = lo.min(m);
    }
    if let Some(m) = opts.y_max {
        hi = hi.max(m);
    }
    if hi - lo < 1e-9 {
        hi = lo + if lo.abs() < 1e-9 { 1.0 } else { lo.abs() * 0.1 };
    }
    let labels = [(opts.fmt)(hi), (opts.fmt)((hi + lo) / 2.0), (opts.fmt)(lo)];
    let gutter = labels.iter().map(|l| l.chars().count()).max().unwrap_or(1) + 1;
    let plot_h = (height).saturating_sub(2); // 1 row time axis, 1 row legend
    if plot_h < 2 || w.saturating_sub(gutter) < 4 {
        return vec![legend];
    }
    let n = series.len();
    let build_row = |idxs: &[usize], panel_w: usize, panel_h: usize, titled: bool| -> Vec<Line<'static>> {
        let panels: Vec<Vec<Line<'static>>> = idxs
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                let g = if k == 0 { gutter } else { 0 };
                let mut rows = if titled { vec![panel_title_line(&series[i], panel_w, g)] } else { Vec::new() };
                rows.extend(panel_rows(&series[i], panel_w, panel_h, (lo, hi), opts, g, &labels));
                rows
            })
            .collect();
        hcat(&panels)
    };
    // #115: a title row is OPPORTUNISTIC - shown only when the row already has spare plot height
    // beyond `MIN_PANEL_H`, never by shrinking a box's existing guaranteed floor. A box sized
    // for the OLD (title-less) minimum must render exactly as it always did; this only spends
    // room a box already had going unused. `panel_h` here is what draw_row's per-row budget was
    // BEFORE any title is taken out of it.
    let titled_split = |panel_h: usize| -> (bool, usize) { if panel_h > MIN_PANEL_H { (true, panel_h - 1) } else { (false, panel_h) } };
    // #115: the legend must list only what is actually drawn - the "worst panel + a plain
    // sentence" fallback below draws ONE panel, and the shared legend used to list every series
    // regardless, with a colour swatch implying a line that was never on screen. `drawn` tracks
    // the indices that actually got a panel this call, in the order they were drawn.
    let mut drawn: Vec<usize> = (0..n).collect();
    let mut lines: Vec<Line<'static>> = if n <= 1 {
        build_row(&[0], w - gutter, plot_h, false)
    } else {
        let side_w = (w.saturating_sub(gutter + n - 1)) / n;
        if side_w >= MIN_PANEL_W && plot_h >= MIN_PANEL_H {
            let (titled, panel_h) = titled_split(plot_h);
            build_row(&(0..n).collect::<Vec<_>>(), side_w, panel_h, titled)
        } else {
            let cols = n.div_ceil(2);
            let grid_w = (w.saturating_sub(gutter + cols.saturating_sub(1))) / cols.max(1);
            let row_h = plot_h.saturating_sub(1) / 2;
            if grid_w >= MIN_PANEL_W && row_h >= MIN_PANEL_H {
                let (titled, panel_h) = titled_split(row_h);
                let (r1, r2): (Vec<usize>, Vec<usize>) = ((0..cols).collect(), (cols..n).collect());
                let mut out = build_row(&r1, grid_w, panel_h, titled);
                out.push(Line::raw(String::new()));
                if !r2.is_empty() {
                    out.extend(build_row(&r2, grid_w, panel_h, titled));
                }
                out
            } else {
                // never squeeze four unreadable panels in: one panel for the series reading
                // worst (highest current value), the rest as a plain sentence
                let worst = series.iter().enumerate().max_by(|a, b| a.1.last().unwrap_or(f64::MIN).total_cmp(&b.1.last().unwrap_or(f64::MIN))).map_or(0, |(i, _)| i);
                drawn = vec![worst];
                let (titled, panel_h) = titled_split(plot_h.saturating_sub(1).max(1));
                let mut out = build_row(&[worst], w - gutter, panel_h, titled);
                let others: Vec<String> = series.iter().enumerate().filter(|(i, _)| *i != worst).map(|(_, s)| format!("{} {}", s.label, s.last().map_or_else(|| "-".to_string(), opts.fmt))).collect();
                out.push(fit_line(vec![Span::styled(if others.is_empty() { String::new() } else { format!("also: {}", others.join(" · ")) }, dim())], w));
                out
            }
        }
    };
    // the axis aligns with where the FIRST panel's own plot columns start (after its y-gutter),
    // not the box's left edge - otherwise "-1h" sits under the y-axis labels, not the chart
    lines.push(time_axis(gutter, w - gutter, opts));
    lines.push(legend_line(drawn.iter().map(|&i| &series[i]), opts, w));
    lines
}

/// Side by side, row by row (shorter panels pad with blank rows). #115: a plain space between
/// panels let four side-by-side GPU lines "read as one hour changing colour three times"
/// (verifier, #48 ship gate at 140x44) - a boundary a reader had to INFER from a colour change,
/// never something they could actually see. A dim vertical rule between every adjacent pair
/// gives every row (the new per-panel title row included) an actual drawn boundary.
fn hcat(panels: &[Vec<Line<'static>>]) -> Vec<Line<'static>> {
    let h = panels.iter().map(Vec::len).max().unwrap_or(0);
    (0..h)
        .map(|r| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, p) in panels.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled("\u{2502}", dim()));
                }
                if let Some(line) = p.get(r) {
                    spans.extend(line.spans.iter().cloned());
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// #115: one row, in the series' own colour, above its panel - so each of the small multiples
/// says what it is before a reader ever looks at the line inside it, instead of the panels
/// reading as one continuous hour that happens to change colour. Aligned exactly like the plot
/// rows below it (`y_gutter` leading blanks on the first panel only), so the title sits directly
/// over its own panel's columns, not the box's left edge.
fn panel_title_line(s: &Series, panel_w: usize, y_gutter: usize) -> Line<'static> {
    let mut spans = Vec::new();
    if y_gutter > 0 {
        spans.push(Span::raw(" ".repeat(y_gutter)));
    }
    let label: String = s.label.chars().take(panel_w).collect();
    spans.push(Span::styled(format!("{label:<panel_w$}"), fg(s.colour).add_modifier(Modifier::BOLD)));
    Line::from(spans)
}

/// One series' connected line: `panel_h` rows of `panel_w` columns, `y_gutter` leading columns
/// reserved for the shared-scale labels (0 = no labels - every panel but the first). CONTINUITY
/// is the acceptance test (card #48): between two adjacent real points the line is unbroken - a
/// vertical run of `│` with `╭ ╮ ╰ ╯` at the turns, flat runs as `───`, never a gap and never a
/// floating character. A gap in the DATA (the collector was down) is left BLANK and breaks the
/// line rather than joining across it - a missing reading must never look like a flat one.
fn panel_rows(s: &Series, panel_w: usize, panel_h: usize, (lo, hi): (f64, f64), opts: &ChartOpts, y_gutter: usize, labels: &[String; 3]) -> Vec<Line<'static>> {
    if panel_w == 0 || panel_h == 0 {
        return Vec::new();
    }
    let (slo, shi) = if s.own_scale {
        let (a, b) = bounds(&s.data).unwrap_or((0.0, 1.0));
        if b - a < 1e-9 { (a - 0.5, a + 0.5) } else { (a, b) }
    } else {
        (lo, hi)
    };
    let data = if opts.peak { resample_max(&s.data, panel_w) } else { resample_mean(&s.data, panel_w) };
    let row_of = |v: f64| -> usize { ((1.0 - ((v - slo) / (shi - slo).max(1e-9)).clamp(0.0, 1.0)) * (panel_h - 1) as f64).round() as usize };
    let mut grid: Vec<Vec<Option<char>>> = vec![vec![None; panel_w]; panel_h];
    let mut prev: Option<(usize, usize)> = None; // (col, row) of the last REAL point
    for (col, v) in data.iter().enumerate() {
        match v.filter(|v| v.is_finite()) {
            None => prev = None, // a data gap: blank here, and the next real point starts fresh
            Some(v) => {
                let row = row_of(v);
                match prev {
                    None => grid[row][col] = Some('─'),
                    Some((_, prow)) if prow == row => grid[row][col] = Some('─'),
                    Some((_, prow)) if row < prow => {
                        // the value went UP: the incoming flat run is at `prow` (bottom of this
                        // step) and turns up-and-left (╯); the outgoing run leaves at `row` (top),
                        // turning down-and-right (╭); everything strictly between is a vertical run
                        grid[prow][col] = Some('╯');
                        grid[row][col] = Some('╭');
                        for line in &mut grid[(row + 1)..prow] {
                            line[col] = Some('│');
                        }
                    }
                    Some((_, prow)) => {
                        // the value went DOWN: incoming at `prow` (top) turns down-and-left (╮);
                        // outgoing at `row` (bottom) turns up-and-right (╰)
                        grid[prow][col] = Some('╮');
                        grid[row][col] = Some('╰');
                        for line in &mut grid[(prow + 1)..row] {
                            line[col] = Some('│');
                        }
                    }
                }
                prev = Some((col, row));
            }
        }
    }
    (0..panel_h)
        .map(|r| {
            let label = if r == 0 { labels[0].as_str() } else if r == panel_h - 1 { labels[2].as_str() } else if r == panel_h / 2 && panel_h >= 5 { labels[1].as_str() } else { "" };
            let mut spans = Vec::new();
            if y_gutter > 0 {
                spans.push(Span::styled(format!("{label:>width$} ", width = y_gutter.saturating_sub(1)), dim()));
            }
            let line: String = grid[r].iter().map(|c| c.unwrap_or(' ')).collect();
            spans.push(Span::styled(line, fg(s.colour)));
            Line::from(spans)
        })
        .collect()
}

/// `     -6h ─────────── -3h ─────────── now`, with the markers drawn over it.
fn time_axis(gutter: usize, plot_w: usize, opts: &ChartOpts) -> Line<'static> {
    let mut cells: Vec<(char, Style)> = vec![('─', dim()); plot_w];
    for m in &opts.markers {
        let col = ((m.at.clamp(0.0, 1.0)) * (plot_w.saturating_sub(1)) as f64).round() as usize;
        if let Some(c) = cells.get_mut(col) {
            *c = (m.glyph, m.style);
        }
    }
    // the two labels are written last: a marker never eats the scale
    let right = " now";
    let mut put = |start: usize, text: &str| {
        for (k, ch) in text.chars().enumerate() {
            if let Some(c) = cells.get_mut(start + k) {
                *c = (ch, dim());
            }
        }
    };
    put(0, &format!("{} ", opts.x_left));
    put(plot_w.saturating_sub(right.len()), right);
    let mut spans = vec![Span::raw(" ".repeat(gutter))];
    for (ch, st) in cells {
        match spans.last_mut() {
            Some(last) if last.style == st => last.content.to_mut().push(ch),
            _ => spans.push(Span::styled(ch.to_string(), st)),
        }
    }
    Line::from(spans)
}

/// `- p50 210 ms  - p90 800 ms  - p99 1.4 s`: the dash takes the series colour. Takes anything
/// iterable of `&Series` so a caller can pass a FILTERED subset (#115: only what was actually
/// drawn, see `chart_rows_lines`'s `drawn`) as easily as the whole slice.
fn legend_line<'a>(series: impl IntoIterator<Item = &'a Series>, opts: &ChartOpts, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for s in series {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("━ ", fg(s.colour)));
        let value = s.last().map_or_else(|| "-".to_string(), opts.fmt);
        let own = if s.own_scale { " (own scale)" } else { "" };
        spans.push(Span::raw(format!("{} {value}{own}", s.label)));
    }
    fit_line(spans, width)
}

/// Horizontal bars, one row per bucket: `<=200 ms ████████ 61 (61%)`.
pub fn draw_hbars(f: &mut Frame, area: Rect, rows: &[(String, f64)], colour: Color) {
    if area.height == 0 || area.width < 10 {
        return;
    }
    let total: f64 = rows.iter().map(|(_, v)| v).sum();
    let max = rows.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
    let label_w = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    let tail_w = 12;
    let bar_w = (area.width as usize).saturating_sub(label_w + 1 + tail_w).max(1);
    let lines: Vec<Line> = rows
        .iter()
        .take(area.height as usize)
        .map(|(label, v)| {
            let mut n = if max > 0.0 { (v / max * bar_w as f64).round() as usize } else { 0 };
            if *v > 0.0 && n == 0 {
                n = 1;
            }
            let share = if total > 0.0 { v / total * 100.0 } else { 0.0 };
            Line::from(vec![
                Span::styled(format!("{label:>label_w$} "), dim()),
                Span::styled("█".repeat(n), fg(colour)),
                Span::raw(format!("{} {v:.0} ({share:.0}%)", " ".repeat(bar_w - n))),
            ])
        })
        .collect();
    render_lines(f, area, lines);
}

/// Heat strip: time runs left to right, one row per bucket (slowest on top), the block height
/// in a cell = how many requests fell there.
pub fn draw_heat(f: &mut Frame, area: Rect, labels: &[String], grid: &[Vec<f64>], colour: Color, x_left: &str) {
    if area.height < 2 || area.width < 12 || labels.is_empty() {
        return;
    }
    let label_w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let plot_w = (area.width as usize).saturating_sub(label_w + 1);
    let rows = labels.len().min(area.height as usize - 1);
    // grid[bucket][column] -> squeeze the columns to the plot width (sum of each slice)
    let squeeze = |row: &Vec<f64>| -> Vec<f64> {
        if row.len() <= plot_w {
            return row.clone();
        }
        (0..plot_w).map(|c| row[c * row.len() / plot_w..((c + 1) * row.len() / plot_w).max(c * row.len() / plot_w + 1).min(row.len())].iter().sum()).collect()
    };
    let squeezed: Vec<Vec<f64>> = grid.iter().map(squeeze).collect();
    let max = squeezed.iter().flatten().copied().fold(0.0_f64, f64::max);
    let mut lines = Vec::new();
    for b in (0..rows).rev() {
        let cells: String = squeezed.get(b).map(|row| row.iter().map(|v| if *v <= 0.0 || max <= 0.0 { ' ' } else { BARS[(((v / max) * 7.0).round() as usize).min(7)] }).collect()).unwrap_or_default();
        lines.push(Line::from(vec![Span::styled(format!("{:>label_w$} ", labels[b]), dim()), Span::styled(cells, fg(colour))]));
    }
    let used = squeezed.first().map_or(plot_w, Vec::len).max(8);
    let mut axis = format!("{x_left} ");
    let pad = used.saturating_sub(axis.chars().count() + 4);
    axis.push_str(&"─".repeat(pad));
    axis.push_str(" now");
    lines.push(Line::from(vec![Span::raw(" ".repeat(label_w + 1)), Span::styled(axis, dim())]));
    render_lines(f, area, lines);
}
