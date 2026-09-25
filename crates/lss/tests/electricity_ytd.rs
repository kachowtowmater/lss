//! card #226 (the owner: "for electric u need to add month a year to date"): page 1's
//! ELECTRICITY box shows "month to date" and a "year to date" row, and a year the stored history
//! does not reach back through says where it starts instead of reading as a whole year.
//! Its own file so it does not touch the render.rs / dash.rs regions other cards are editing.

use lss::demo;
use lss::ui::{draw, App};
use lss_core::rates::SpendWindow;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn page1(status: lss_core::model::Status, w: u16, h: u16) -> String {
    let now = status.generated_at + 3;
    let app = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, ..Default::default() };
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, &app, now)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
}

fn with_year(first_day: Option<String>, win: SpendWindow) -> lss_core::model::Status {
    let mut s = demo::status();
    let sp = s.spending.get_or_insert_with(Default::default);
    sp.this_year = win;
    sp.year_first_day = first_day;
    s
}

#[test]
fn page1_electricity_has_month_to_date_and_year_to_date() {
    let s = with_year(None, SpendWindow { usd: Some(412.5), kwh: Some(1031.0), covered_secs: 86_400, nominal_secs: 86_400 });
    let text = page1(s, 200, 70);
    assert!(text.contains("month to date"), "the month row says what it is:\n{text}");
    assert!(!text.contains("this month "), "the old label is gone from page 1:\n{text}");
    let row = text.lines().find(|l| l.contains("year to date")).unwrap_or_else(|| panic!("no year-to-date row:\n{text}"));
    assert!(row.contains("$412.50") && row.contains("1031.00 kWh"), "{row}");
}

/// The text INSIDE one box, its wrapped lines joined: batch 2 (integration of #226 with #227) -
/// #227's packed grid makes ELECTRICITY one column of three at 200 wide, so a caveat sentence
/// wraps inside the box and a whole-screen-line `contains` no longer sees it; the lines between
/// belong to the neighbouring columns. This reads only the box's own column.
fn box_text(text: &str, title: &str) -> String {
    let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let (top, at) = lines.iter().enumerate().find_map(|(y, l)| {
        let s: String = l.iter().collect();
        s.find(title).map(|b| (y, s[..b].chars().count()))
    }).unwrap_or_else(|| panic!("{title} not on screen:\n{text}"));
    let left = (0..=at).rev().find(|&x| matches!(lines[top][x], '┌' | '┏')).expect("the box's left corner");
    let right = (at..lines[top].len()).find(|&x| matches!(lines[top][x], '┐' | '┓')).expect("the box's right corner");
    let mut out = Vec::new();
    for l in &lines[top + 1..] {
        if l.len() <= right || matches!(l[left], '└' | '┗') {
            break;
        }
        out.push(l[left + 1..right].iter().collect::<String>().trim().to_string());
    }
    out.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn page1_year_to_date_names_where_a_short_history_starts() {
    let s = with_year(None, SpendWindow::default());
    let year = lss_core::timeutil::fmt_local(s.generated_at, "%Y");
    let s = with_year(Some(format!("{year}-06-01")), SpendWindow { usd: Some(50.0), kwh: Some(120.0), covered_secs: 40 * 86_400, nominal_secs: 200 * 86_400 });
    let text = page1(s, 200, 70);
    let own = box_text(&text, "o ELECTRICITY");
    assert!(own.contains(&format!("since {year}-06-01 - the stored history does not reach Jan 1")), "{own}\n{text}");
    assert!(own.contains("not the full window"), "the covered/nominal caveat is there too:\n{own}");
}

#[test]
fn page1_year_to_date_with_nothing_priced_is_a_dash_not_zero() {
    let s = with_year(None, SpendWindow { usd: None, kwh: None, covered_secs: 0, nominal_secs: 200 * 86_400 });
    let text = page1(s, 200, 70);
    let line = text.lines().find(|l| l.contains("year to date")).unwrap_or_else(|| panic!("the row must still be there:\n{text}"));
    // only the ELECTRICITY box's own cell: since #222 put ALERTS/INCIDENTS first, this screen
    // line also carries the users table beside it, whose "$0.00" is a real (zero) user spend and
    // says nothing about this row (batch1 integration of #226 with #222)
    let at = line.find("year to date").unwrap();
    let row = &line[at..line[at..].find('\u{2502}').map_or(line.len(), |e| at + e)];
    assert!(row.contains('\u{2014}') && !row.contains("$0.00"), "{row}");
}
