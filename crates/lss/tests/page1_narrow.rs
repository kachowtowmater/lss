//! card #227, lss-verifier-4's FAIL: page 1's grid at the owner's narrow sizes.
//!   A - USERS sat on the top row EMPTY at 60 and 120 wide ("N users - widen the pane").
//!   B - at 60x45 values broke mid-word across lines ("modelopt_f" / "p4").
//! Its own file so it does not touch render.rs / dash.rs regions other cards are editing.

use lss::demo;
use lss::ui::{draw, App};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::Terminal;
use std::collections::HashSet;

type Grid = Vec<Vec<String>>;

/// Every screen of page 1 at `w`x`h`, top to bottom (PageDown until the page stops moving).
fn screens(w: u16, h: u16) -> Vec<Grid> {
    let status = demo::status();
    let now = status.generated_at + 3;
    let mut app = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, ..Default::default() };
    let mut out = Vec::new();
    let mut last = usize::MAX;
    for _ in 0..40 {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, &app, now)).unwrap();
        let (first, _, _) = app.page_rows.get();
        if first == last {
            break;
        }
        last = first;
        let buf = t.backend().buffer().clone();
        out.push((0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect());
        app.on_key(KeyCode::PageDown);
    }
    out
}

fn text(g: &Grid) -> String {
    g.iter().map(|r| r.concat() + "\n").collect()
}

/// The interior rows of the box whose top border carries `title`, on the first screen that has it.
fn box_interior(shots: &[Grid], title: &str) -> Option<Vec<String>> {
    for g in shots {
        for (y, row) in g.iter().enumerate() {
            let line = row.concat();
            let Some(byte) = line.find(title) else { continue };
            let tx = line[..byte].chars().count();
            let is = |c: &str, set: &str| set.contains(c) && !c.is_empty();
            let Some(x0) = (0..=tx).rev().find(|&x| is(&row[x], "┌╭")) else { continue };
            let Some(x1) = (tx..row.len()).find(|&x| is(&row[x], "┐╮")) else { continue };
            let mut rows = Vec::new();
            for r in &g[y + 1..] {
                if is(&r[x0], "└╰") {
                    break;
                }
                rows.push(r[x0 + 1..x1].concat());
            }
            return Some(rows);
        }
    }
    None
}

#[test]
fn users_on_page_1_shows_its_users_at_60_and_120_wide() {
    let names: Vec<String> = demo::status().users.rows.iter().map(|r| r.name.chars().take(6).collect()).collect();
    assert!(names.len() >= 2, "the demo has users to show");
    for (w, h) in [(60, 45), (120, 40)] {
        let shots = screens(w, h);
        let inner = box_interior(&shots, "USERS").unwrap_or_else(|| panic!("{w}x{h}: no USERS box\n{}", text(&shots[0])));
        let body = inner.join("\n");
        assert!(!body.contains("widen the pane"), "{w}x{h}: USERS is on the top row, so it must show users, not a hint\n{body}");
        // #255 round 3 (the owner, polish): the USERS caption that used to spell out which
        // columns are hidden at this width ("lane/req24h/prompt/$ hidden...") moved to `?` help -
        // this assertion used to pass at a middling width via THAT sentence mentioning "req24h",
        // not the table's own header, which correctly has no such column there (it genuinely
        // shows "gen"/"share" instead, not req24h, at 120x40's own 2-column width). "req24h" OR
        // "share" - either header says what its own columns are, just for a different tier.
        assert!(body.contains("on now") && (body.contains("req24h") || body.contains("share")), "{w}x{h}: the table says what its columns are\n{body}");
        for n in &names {
            assert!(inner.iter().any(|l| l.contains(n.as_str())), "{w}x{h}: user {n:?} is missing from USERS\n{body}");
        }
    }
}

/// Every whitespace/border-separated word page 1 shows when it is wide enough that nothing wraps.
fn vocabulary() -> HashSet<String> {
    let mut v = HashSet::new();
    for (w, h) in [(240, 80), (200, 60)] {
        for g in screens(w, h) {
            for row in &g {
                for t in row.concat().split(|c: char| c.is_whitespace() || "│┌┐└┘╭╮╰╯─".contains(c)) {
                    if !t.is_empty() {
                        v.insert(t.to_string());
                    }
                }
            }
        }
    }
    v
}

#[test]
fn no_value_on_page_1_is_split_across_lines_at_60x45() {
    let vocab = vocabulary();
    let alnum = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
    let mut splits = Vec::new();
    for (n, g) in screens(60, 45).iter().enumerate() {
        for y in 0..g.len().saturating_sub(1) {
            let (a, b) = (&g[y], &g[y + 1]);
            let bars: Vec<usize> = (0..a.len()).filter(|&x| a[x] == "│" && b[x] == "│").collect();
            for pair in bars.windows(2) {
                let (l, r) = (pair[0] + 1, pair[1]);
                // the last word of this row's cell and the first word of the next row's same cell
                // (a box's interior is padded, so a split word does not touch the border itself)
                let t1 = a[l..r].concat().split_whitespace().next_back().unwrap_or_default().to_string();
                let t2 = b[l..r].concat().split_whitespace().next().unwrap_or_default().to_string();
                if !alnum(t1.chars().last()) || !alnum(t2.chars().next()) {
                    continue;
                }
                let joined = format!("{t1}{t2}");
                if joined.chars().count() >= 4 && vocab.iter().any(|v| v.starts_with(&joined)) {
                    splits.push(format!("screen {} row {y}: {t1:?} | {t2:?} (a split of {joined:?})", n + 1));
                }
            }
        }
    }
    assert!(splits.is_empty(), "values broken mid-word at 60x45:\n{}", splits.join("\n"));
}
