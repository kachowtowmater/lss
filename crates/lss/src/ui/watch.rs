//! Card #74: what the sources this owner follows for serving recipes have published, and when
//! (`w`, always reachable - unlike FLEET this is useful with a single server too). The owner:
//! "then ill know okay someone posted an update for the model I'm running, another posted about
//! a different one ... last updated 2 hours ago. stuff like this is more useful than seeing
//! advice, incidents and alerts."
//!
//! Reads `s.watch` (`lss_core::watch::WatchStatus`) - the collector's own copy of whatever a
//! SEPARATE watch-sweep wrote to `[watch] path`; this page never fetches anything itself and
//! never suggests switching a loadout - it only reports. `None` says plainly that no
//! `[watch] path` is configured, never an empty table standing in for "off".
//!
//! The two hard constraints this page renders honestly, not just carries the data for:
//!  - a bare social/hypothesis claim (`has_receipt: false`) is labelled UNVERIFIED, in its own
//!    colour, never presented the same way as a receipted item.
//!  - `last_checked` ("checked Nh ago") and a source's newest item's own `date` are two
//!    different facts and are never on the same line pretending to be one.
//!
//! NEW badges (`app.watch_new`, a stable snapshot decided the moment the page was opened - see
//! its own doc comment on `App`) mark what changed since the last time this page was opened.

use super::widgets::{bold, dim, draw_box, fit_line, good, render_lines, warn};
use super::{footer, App};
use lss_core::model::Status;
use lss_core::timeutil::fmt_duration;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

fn source_lines(src: &lss_core::watch::WatchSource, is_new: bool, now: i64) -> Vec<Line<'static>> {
    let checked = src.last_checked.map_or_else(|| "never checked".to_string(), |t| format!("checked {} ago", fmt_duration((now - t).max(0))));
    let mut head = vec![Span::styled(format!("{:<20}", src.name), bold())];
    if is_new {
        head.push(Span::styled("NEW  ", good().add_modifier(Modifier::BOLD)));
    }
    head.push(Span::styled(src.covers.clone(), dim()));
    let mut lines = vec![Line::from(head), Line::styled(format!("  {checked}"), dim())];
    match &src.newest {
        None if src.last_checked.is_none() => lines.push(Line::styled("  \u{2014} never checked yet", dim())),
        None => lines.push(Line::styled("  \u{2014} checked, nothing published yet", dim())),
        Some(item) => {
            if item.has_receipt {
                lines.push(Line::raw(format!("  {}  {}", item.date, item.summary)));
            } else {
                lines.push(Line::from(vec![Span::raw(format!("  {}  ", item.date)), Span::styled("UNVERIFIED", warn().add_modifier(Modifier::BOLD)), Span::raw(format!(" - {}", item.summary))]));
            }
            if !item.url.is_empty() {
                lines.push(Line::styled(format!("  {}", item.url), dim()));
            }
        }
    }
    lines.push(Line::raw(String::new()));
    lines
}

fn content(s: &Status, app: &App, now: i64) -> Vec<Line<'static>> {
    let Some(w) = &s.watch else {
        return vec![
            Line::styled("\u{2014} watch tracking is off (no [watch] path configured - see packaging/watch.json.example)", dim()),
            Line::styled("this page only ever REPORTS what a source published - it never picks or switches a loadout, that stays a human decision", dim()),
        ];
    };
    if w.sources.is_empty() {
        return vec![Line::styled("[watch] path is configured, but the file lists no sources yet", dim())];
    }
    let mut lines = Vec::new();
    for src in &w.sources {
        lines.extend(source_lines(src, app.watch_new.contains(&src.name), now));
    }
    lines.push(Line::styled("REPORTS ONLY - switching a loadout needs an observed problem or a measured A/B, always a human decision, never this page", dim()));
    lines
}

pub fn draw(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) {
    let area = super::reserve_key_hints(f, area);
    if area.height < 3 {
        return render_lines(f, area, vec![Line::styled(" WATCH ", dim())]);
    }
    let top = vec![Line::styled(" LLM SERVER STATUS \u{b7} WATCH ", ratatui::style::Style::default().add_modifier(Modifier::BOLD))];
    let top_h = (top.len() as u16).min(area.height);
    render_lines(f, Rect { height: top_h, ..area }, top);
    let body = Rect { y: area.y + top_h, height: area.height.saturating_sub(top_h + 1), ..area };
    if body.height < 3 {
        return;
    }
    let lines = content(s, app, now);
    let total = lines.len();
    let last = total.saturating_sub(1);
    let start = app.scroll.min(last);
    let shown = (body.height as usize).saturating_sub(2).min(total - start).max(1);
    app.page_rows.set((start, shown, total));
    let inner = draw_box(f, body, "WATCH", vec![], Color::Blue, false);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(start).take(shown).map(|l| fit_line(l.spans, inner.width as usize)).collect();
    render_lines(f, inner, visible);
    let foot = Rect { y: area.y + area.height - 1, height: 1, ..area };
    f.render_widget(Paragraph::new(footer(app, area.width)), foot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::watch::{WatchItem, WatchSource};

    fn src(name: &str, has_receipt: bool, date: &str) -> lss_core::watch::WatchSource {
        WatchSource { name: name.into(), covers: "model loadouts".into(), last_checked: Some(1_000), newest: Some(WatchItem { date: date.into(), summary: "swapped the chat template for a fixed one from upstream".into(), url: "https://example.com/r/1".into(), has_receipt }) }
    }

    #[test]
    fn no_watch_path_says_tracking_is_off_never_an_empty_table() {
        let s = Status::default();
        let app = App::default();
        let lines = content(&s, &app, 1_000);
        let text: String = lines[0].spans.iter().map(|sp| sp.content.as_ref()).collect::<Vec<_>>().join("");
        assert!(text.contains("watch tracking is off"), "{text}");
    }

    /// the card's own hard constraint: a bare social claim reads UNVERIFIED, in its own colour,
    /// never presented the same way as a receipted item.
    #[test]
    fn a_source_with_no_receipt_reads_unverified_a_receipted_one_does_not() {
        let s = Status { watch: Some(lss_core::watch::WatchStatus { sources: vec![src("example-recipes", true, "2026-09-20"), src("a social account", false, "2026-09-19")] }), ..Status::default() };
        let app = App::default();
        let lines = content(&s, &app, 2_000);
        let all: String = lines.iter().flat_map(|l| l.spans.iter().map(|sp| sp.content.to_string())).collect();
        assert!(all.contains("UNVERIFIED"), "{all}");
        let sero_line = lines.iter().find(|l| l.spans.iter().any(|sp| sp.content.contains("2026-09-20"))).unwrap();
        let sero_text: String = sero_line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(!sero_text.contains("UNVERIFIED"), "a receipted item must never read UNVERIFIED: {sero_text}");
    }

    /// NEW is a stable snapshot from `app.watch_new`, decided at open time - not recomputed live.
    #[test]
    fn new_badge_follows_app_watch_new_not_a_live_recompute() {
        let s = Status { watch: Some(lss_core::watch::WatchStatus { sources: vec![src("example-recipes", true, "2026-09-20")] }), ..Status::default() };
        let mut app = App::default();
        let without = content(&s, &app, 2_000);
        assert!(!without.iter().any(|l| l.spans.iter().any(|sp| sp.content.contains("NEW"))), "not new until app.watch_new says so");
        app.watch_new = vec!["example-recipes".into()];
        let with = content(&s, &app, 2_000);
        assert!(with.iter().any(|l| l.spans.iter().any(|sp| sp.content.contains("NEW"))), "NEW once app.watch_new names it");
    }

    /// `last_checked` (the CHECK's own age) and a source's `newest.date` (the item's own date)
    /// must never collapse into the same sentence.
    #[test]
    fn checked_age_and_item_date_are_two_distinct_lines() {
        let s = Status { watch: Some(lss_core::watch::WatchStatus { sources: vec![src("example-recipes", true, "2026-09-20")] }), ..Status::default() };
        let app = App::default();
        let lines = content(&s, &app, 1_000 + 7_200);
        let checked_line = lines.iter().find(|l| l.spans.iter().any(|sp| sp.content.contains("checked"))).unwrap();
        let checked_text: String = checked_line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(checked_text.contains("2h") && !checked_text.contains("2026-09-20"), "{checked_text}");
        let item_line = lines.iter().find(|l| l.spans.iter().any(|sp| sp.content.contains("2026-09-20"))).unwrap();
        let item_text: String = item_line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(!item_text.contains("checked"), "{item_text}");
    }
}
