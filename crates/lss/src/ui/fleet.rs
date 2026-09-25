//! #92: the FLEET view (`f`, only reachable with more than one `[[server]]` - a fleet of one
//! gets no new key, per the card's own rule that it must look exactly like today; the header's
//! own compact FLEET STRIP, `fleet_line`, is unchanged and still shows on every page regardless).
//! The owner: "if our network or tailscale cluster has more than one node .. i would like to
//! see that information in llm server status too".
//!
//! One row per configured node (`app.fleet`, refreshed every 5s by its own background thread per
//! node - `main.rs`'s `spawn_fleet` - never a live fetch from this draw call). Three honesty
//! rules this page exists to prove:
//!   1. a node this collector cannot reach at all (`Err`) says so and drops out of every total -
//!      it never reads as a silent 0, and the totals line says how many nodes it could not count.
//!   2. a node whose collector answers but whose SERVE is down still contributes its real
//!      hardware numbers (idle GPUs still draw power and still cost money) - "down" only zeroes
//!      what is genuinely zero right now (decode speed), never the things that are still true.
//!   3. totals that sum honestly ARE summed (watts, dollars/hour, all-time generated tokens);
//!      the one that would NOT - a single "decode tok/s" averaged across different models on
//!      different silicon - is refused outright, with the sentence saying why, never blended.
//!
//! Heterogeneous by design (a Spark runs a different model on different hardware than the GPU
//! box): a column an engine does not publish reads `n/a (not reported by X)`, the exact sentence
//! every single-node page already gives that same gap (`ServeStatus::na`).

use super::widgets::{dim, draw_box, fit, fit_line, good, red, render_lines};
use super::{footer, App, FleetEntry, FleetState};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const FULL_W: usize = 100;

fn engine_label(engine: &str) -> String {
    lss_core::engine::EngineKind::parse(engine).map_or_else(|| if engine.is_empty() { "\u{2014}".to_string() } else { engine.to_string() }, |k| k.label().to_string())
}

/// `n/a (not reported by X)` - the exact sentence `ServeStatus::na` gives every single-node page
/// for the same gap, rebuilt here from the small slice `FleetState` actually carries.
fn na(not_reported: &[String], engine: &str, key: &str) -> Option<String> {
    not_reported.iter().any(|k| k == key).then(|| format!("n/a (not reported by {})", engine_label(engine)))
}

/// #107, 2026-09-22 (verifier): every numeric cell after `model` gets an explicit 2-space
/// separator BEFORE the next one - a value that overflows its own nominal width (a real
/// `"2.1k tok/s"` is 10 chars, wider than a `{:>9}` column) must never eat the gap that keeps it
/// apart from its neighbour. `format!` never truncates an over-width value, so the separator
/// always survives even when a column runs long - the ONE thing that guarantees two numbers on
/// the same row can never visually fuse into one.
fn header_row(full: bool) -> Line<'static> {
    if full {
        Line::styled(format!("{:<14}{:<10}{:<21}{:<6}{:>9}  {:>9}  {:>6}  {:>5}  {:>8}  {:>8}", "node", "engine", "model", "up", "decode", "prefill", "kv", "gpus", "watts", "$/hr"), dim())
    } else {
        Line::styled(format!("{:<14}{:<6}{:>9}  {:>8}", "node", "up", "decode", "$/hr"), dim())
    }
}

/// One row. `None` on `e.state` = the background fetch has not answered even once yet (freshly
/// started) - distinct from `Some(Err(_)))` (asked, and it failed) so "still connecting" never
/// reads as "this node is down".
fn row(e: &FleetEntry, full: bool) -> Line<'static> {
    let big = crate::report::big;
    let name = fit(&e.name, 14);
    let Some(result) = &e.state else {
        return Line::styled(format!("{name:<14}connecting..."), dim());
    };
    let st = match result {
        Ok(st) => st,
        // #107 item 2: no pre-truncation here - `content()`'s own `fit_line` call already
        // truncates every row to the REAL box width at render time; a second, hardcoded cutoff
        // here (the old code cut at 70 chars always, even in a 150-wide box) was strictly worse
        // and is exactly what cut the reason off mid-word ("Connection r…") with room to spare.
        //
        // #107 item 2 (verifier-3): the em dash used to sit in the field right after `name`
        // unconditionally - the narrow header's SECOND column genuinely is `up` (6 wide), so the
        // dash landed there by accident and looked right, but the full header's second column is
        // `engine` (10 wide): the SAME dash at the SAME offset landed under ENGINE, reading as
        // "engine unknown" rather than "node down". Pad through engine+model FIRST at full width
        // so the dash always lands in the `up` column - the one honesty rule 1's own row exists
        // to answer ("is this node up") - at every tier, not just the one that happened to align.
        Err(msg) => {
            let dash = "\u{2014}";
            return if full {
                Line::styled(format!("{name:<14}{:<10}{:<21}{dash:<6}unreachable - {msg}", "", ""), red())
            } else {
                Line::styled(format!("{name:<14}{dash:<6}unreachable - {msg}"), red())
            };
        }
    };
    let up_text = if st.up { "up" } else { "down" };
    let up_style = if st.up { good() } else { red().add_modifier(Modifier::BOLD) };
    // padded to the SAME 6-column width the header gives "up" - a bare "up"/"down" span with no
    // padding of its own is what let decode's `{:>9}` bleed left and collide with it.
    let up = Span::styled(format!("{up_text:<6}"), up_style);
    let model = fit(&st.model.clone().unwrap_or_else(|| "(no model)".into()), 21);
    let decode = format!("{} tok/s", big(st.decode_tok_s));
    if full {
        let prefill = st.prefill_tok_s.map_or_else(|| na(&st.not_reported, &st.engine, "prefill_tok_s").unwrap_or_else(|| "\u{2014}".into()), |v| format!("{} tok/s", big(v)));
        let kv = na(&st.not_reported, &st.engine, "kv_usage").unwrap_or_else(|| format!("{:.0}%", st.kv_usage * 100.0));
        let watts = st.total_watts.map_or_else(|| "\u{2014}".to_string(), |w| format!("{w:.0}W"));
        let cost = st.cost_per_hour.map_or_else(|| "\u{2014}".to_string(), |c| format!("${c:.2}"));
        let mut spans = vec![Span::raw(format!("{name:<14}{:<10}{model:<21}", engine_label(&st.engine)))];
        spans.push(up);
        spans.push(Span::raw(format!("{decode:>9}  {prefill:>9}  {kv:>6}  {:>5}  {watts:>8}  {cost:>8}", st.gpu_count)));
        Line::from(spans)
    } else {
        let cost = st.cost_per_hour.map_or_else(|| "\u{2014}".to_string(), |c| format!("${c:.2}"));
        let mut spans = vec![Span::raw(format!("{name:<14}"))];
        spans.push(up);
        spans.push(Span::raw(format!("{decode:>9}  {cost:>8}")));
        Line::from(spans)
    }
}

/// Honesty rules 1 and 3 from the module doc comment, in one place: which nodes actually count
/// (reachable, `Ok`), what sums across them, and the one figure refused outright.
fn totals(fleet: &[FleetEntry], full: bool) -> Vec<Line<'static>> {
    let big = crate::report::big;
    let reachable: Vec<&FleetState> = fleet.iter().filter_map(|e| e.state.as_ref().and_then(|r| r.as_ref().ok())).collect();
    let excluded = fleet.len() - reachable.len();
    let up_count = reachable.iter().filter(|s| s.up).count();
    let mut lines = vec![Line::styled(format!("{up_count} of {} up{}", fleet.len(), if excluded > 0 { format!(" \u{b7} {excluded} unreachable, excluded from every total below") } else { String::new() }), dim())];
    let watts: Vec<f64> = reachable.iter().filter_map(|s| s.total_watts).collect();
    let watts_line = if watts.is_empty() {
        "total power: \u{2014} no node reports GPU watts".to_string()
    } else {
        format!("total power: {:.0}W (across {} of {} reachable nodes reporting it)", watts.iter().sum::<f64>(), watts.len(), reachable.len())
    };
    let costs: Vec<f64> = reachable.iter().filter_map(|s| s.cost_per_hour).collect();
    let cost_line = if costs.is_empty() {
        "total live cost: \u{2014} no node has electricity cost tracking configured (card #75)".to_string()
    } else {
        format!("total live cost: ${:.2}/hour (across {} of {} reachable nodes with cost tracking on)", costs.iter().sum::<f64>(), costs.len(), reachable.len())
    };
    let tokens_total: f64 = reachable.iter().map(|s| s.generated_tokens_total).sum();
    let tokens_line = format!("generated, all time, summed across the fleet: {}", big(tokens_total));
    lines.push(Line::raw(watts_line));
    lines.push(Line::raw(cost_line));
    lines.push(Line::raw(tokens_line));
    lines.push(Line::styled("no fleet-wide \"decode tok/s\" figure: a single number averaged across different models on different hardware would not mean anything - see each node's own row", dim()));
    if !full {
        lines.push(Line::styled("engine/model/prefill/kv/gpus/watts columns hidden at this width", dim()));
    }
    lines
}

fn content(app: &App, width: usize) -> Vec<Line<'static>> {
    let full = width >= FULL_W;
    let mut lines = vec![header_row(full)];
    lines.extend(app.fleet.iter().map(|e| row(e, full)));
    lines.push(Line::raw(""));
    lines.extend(totals(&app.fleet, full));
    lines
}

pub fn draw(f: &mut Frame, app: &App, area: Rect, _now: i64) {
    let area = super::reserve_key_hints(f, area);
    if area.height < 3 {
        return render_lines(f, area, vec![Line::styled(" FLEET ", dim())]);
    }
    let mut top = vec![Line::styled(" LLM SERVER STATUS \u{b7} FLEET ", ratatui::style::Style::default().add_modifier(Modifier::BOLD))];
    if let Some(l) = super::fleet_line(app, area.width) {
        top.push(l);
    }
    let top_h = (top.len() as u16).min(area.height);
    render_lines(f, Rect { height: top_h, ..area }, top);
    let body = Rect { y: area.y + top_h, height: area.height.saturating_sub(top_h + 1), ..area };
    if body.height < 3 {
        return;
    }
    let lines = content(app, body.width.saturating_sub(2) as usize);
    let total = lines.len();
    let last = total.saturating_sub(1);
    let start = app.scroll.min(last);
    let shown = (body.height as usize).saturating_sub(2).min(total - start).max(1);
    app.page_rows.set((start, shown, total));
    let inner = draw_box(f, body, "FLEET", vec![], Color::Blue, false);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(start).take(shown).map(|l| fit_line(l.spans, inner.width as usize)).collect();
    render_lines(f, inner, visible);
    let foot = Rect { y: area.y + area.height - 1, height: 1, ..area };
    f.render_widget(Paragraph::new(footer(app, area.width)), foot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::FleetState;

    fn up_node(name: &str) -> FleetEntry {
        FleetEntry {
            name: name.into(),
            url: format!("http://{name}:8099"),
            state: Some(Ok(FleetState {
                up: true,
                host: name.into(),
                engine: "sglang".into(),
                model: Some("model-a".into()),
                decode_tok_s: 210.0,
                prefill_tok_s: Some(4_000.0),
                running: 2.0,
                slots: 8,
                kv_usage: 0.31,
                gpu_count: 4,
                total_watts: Some(900.0),
                cost_per_hour: Some(0.42),
                generated_tokens_total: 1_000_000.0,
                not_reported: Vec::new(),
            })),
        }
    }

    /// honesty rule 1: an unreachable node says so in its own row and is EXCLUDED from every
    /// total, and the totals line says how many were excluded - never a silent 0, never a
    /// fleet total that quietly pretends the missing node does not exist.
    #[test]
    fn an_unreachable_node_says_so_and_is_excluded_from_every_total() {
        let fleet = vec![up_node("gpu-box"), FleetEntry { name: "spark-b".into(), url: "http://spark-b:8099".into(), state: Some(Err("connection refused".into())) }];
        let row_text = row(&fleet[1], true);
        let text: String = row_text.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("unreachable") && text.contains("connection refused"), "{text}");

        let t = totals(&fleet, true);
        let up_line: String = t[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(up_line.contains("1 of 2 up") && up_line.contains("1 unreachable, excluded"), "{up_line}");
        let watts_line: String = t[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(watts_line.contains("900W") && watts_line.contains("1 of 1 reachable"), "the unreachable node must not be counted in the denominator either\n{watts_line}");
    }

    /// honesty rule 2: a reachable node whose SERVE is down still contributes its real hardware
    /// numbers (idle GPUs still draw power and still cost money) - down only zeroes what really
    /// is zero right now.
    #[test]
    fn a_down_but_reachable_node_still_counts_its_watts_and_cost() {
        let mut e = up_node("spark-a");
        if let Some(Ok(st)) = e.state.as_mut() {
            st.up = false;
            st.decode_tok_s = 0.0;
        }
        let t = totals(std::slice::from_ref(&e), true);
        let watts_line: String = t[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(watts_line.contains("900W"), "a down-but-reachable node's GPUs still cost money: {watts_line}");
        let row_text = row(&e, true);
        let text: String = row_text.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("down"), "{text}");
    }

    /// honesty rule 3: watts/cost/tokens sum; decode tok/s across different models never does -
    /// the refusal sentence is always on screen, never silently omitted.
    #[test]
    fn totals_sum_what_is_meaningful_and_refuse_the_decode_average() {
        let fleet = vec![up_node("gpu-box"), up_node("spark-a")];
        let t = totals(&fleet, true);
        let cost_line: String = t[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(cost_line.contains("$0.84/hour"), "{cost_line}");
        let tokens_line: String = t[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(tokens_line.contains("2.00M") || tokens_line.contains("2M"), "{tokens_line}");
        let refusal: String = t[4].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(refusal.to_lowercase().contains("not mean anything"), "{refusal}");
    }

    /// heterogeneous by design: a column the engine does not publish reads the SAME
    /// "n/a (not reported by X)" sentence a single-node page already gives, never a blank.
    #[test]
    fn a_column_the_engine_does_not_publish_reads_na_with_the_engine_named() {
        let mut e = up_node("laptop");
        if let Some(Ok(st)) = e.state.as_mut() {
            st.engine = "ollama".into();
            st.not_reported = vec!["kv_usage".into(), "prefill_tok_s".into()];
        }
        let text: String = row(&e, true).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("n/a (not reported by Ollama)"), "{text}");
    }

    #[test]
    fn connecting_reads_distinctly_from_down_or_unreachable() {
        let e = FleetEntry { name: "spark-b".into(), url: "http://spark-b:8099".into(), state: None };
        let text: String = row(&e, true).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("connecting"), "{text}");
        assert!(!text.to_lowercase().contains("down") && !text.to_lowercase().contains("unreachable"), "{text}");
    }
}
