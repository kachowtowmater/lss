//! card #257: the `?` help overlay. It said page 1 was "one scrolling column" (untrue since #227),
//! and it CLIPPED every line longer than the box at its right border, mid-word ("see Pa", "ful",
//! "default do"). Its own file: the page-1 layout in dash.rs / render.rs is being changed by
//! another card at the same time.

use lss::demo;
use lss::ui::{draw, App, HELP_GROUPS};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::Terminal;

/// Every line of the overlay's content, in order, read off the screen one window at a time
/// (scrolling by exactly what each frame showed, so no line is read twice or skipped).
fn help_content(w: u16, h: u16) -> Vec<String> {
    let status = demo::status();
    let now = status.generated_at + 3;
    let mut app = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, ..Default::default() };
    app.on_key(KeyCode::Char('?'));
    assert!(app.show_help);
    let mut rows = Vec::new();
    for _ in 0..50 {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, &app, now)).unwrap();
        let buf = t.backend().buffer().clone();
        let (start, shown, total) = app.help_rows.get();
        // the overlay's own rect, as draw_help computes it
        let bw = 82.min(w.saturating_sub(2));
        let bh = (total as u16 + 2).min(h);
        let (x, y) = ((w - bw) / 2, (h - bh) / 2);
        for yy in y + 1..y + 1 + shown as u16 {
            rows.push((x + 1..x + bw - 1).map(|xx| buf[(xx, yy)].symbol().to_string()).collect::<String>());
        }
        if start + shown >= total {
            return rows;
        }
        app.help_scroll = start + shown;
    }
    panic!("{w}x{h}: the help never reached its end");
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_help_line_is_whole_at_120x40_and_60x45() {
    for (w, h) in [(120u16, 40u16), (60, 45)] {
        let rows = help_content(w, h);
        let all = squash(&rows.join(" "));
        for (group, keys) in HELP_GROUPS {
            assert!(all.contains(group), "{w}x{h}: group {group:?} missing");
            for (k, d) in keys {
                // wrapped lines continue under their own column, so with the indentation squashed
                // the entry reads back whole - a clipped line cannot
                let entry = squash(&format!("{k} {d}"));
                assert!(all.contains(&entry), "{w}x{h}: {entry:?} is not whole in the overlay (clipped or cut):\n{}", rows.join("\n"));
            }
        }
        assert!(!all.contains('\u{2026}'), "{w}x{h}: no help word is so long it needs an ellipsis at this size:\n{}", rows.join("\n"));
    }
}

#[test]
fn help_describes_the_real_page_1() {
    let all = squash(&help_content(120, 40).join(" "));
    assert!(!all.contains("one scrolling column"), "page 1 has not been one scrolling column since #227");
    assert!(all.contains("a grid of boxes, at most two columns") && all.contains("the status line is on top, the tab strip at the bottom"), "{all}");
}

/// card #267 (a merge-only defect in batch 3): #257 wrote the help text while page 1 still had a
/// "worst now" verdict at the bottom, #258 removed it, and the help went on describing it. What
/// `?` says about page 1's chrome is checked against page 1 AS DRAWN, at the same size.
#[test]
fn help_and_the_drawn_chrome_agree() {
    let help = squash(&help_content(120, 40).join(" "));
    let status = demo::status();
    let now = status.generated_at + 3;
    let app = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, ..Default::default() };
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| draw(f, &app, now)).unwrap();
    let buf = t.backend().buffer().clone();
    let rows: Vec<String> = (0..40).map(|y| (0..120).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect();
    // "the status line is on top": row 0 is the status line
    assert!(help.contains("the status line is on top") && rows[0].starts_with(" LLM SERVER STATUS"), "help: status line on top; drawn row 0: {}", rows[0]);
    // "the tab strip at the bottom": card #291 put the key-hint line under it, so at this
    // (spacious, `reserve_key_hints`'s own `h > 12` floor) size the tab strip is the second-last
    // row, not the last one.
    assert!(help.contains("the tab strip at the bottom") && rows[38].contains("overview") && rows[38].trim_end().ends_with("? help"), "help: tab strip at the bottom; drawn second-last row: {}", rows[38]);
    // and neither says there is a verdict line: the help never mentions one, the page never draws one
    assert!(!help.contains("verdict"), "the help describes a verdict line page 1 no longer has: {help}");
    assert!(!rows.iter().any(|r| r.contains("worst now")), "page 1 draws a verdict line again");
}
