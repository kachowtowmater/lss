//! card #258, then card #291. #258: the owner, pasting the old five-line bottom block (header, a
//! two-line "worst now" sentence with its "also:" list and the cost, the tab strip, a key-hint
//! footer): "this looks terrible on both bottom and top lol"; then "just remove all of this" (the
//! verdict block); then "move the llm server status back up to where you had it before". #291,
//! after shelving the R2 redesign entirely: "the only thing you need to add on the bottom is the
//! short cut keys and buttons" - so the key-hint line is back, directly under the tab strip, and
//! the redesign's other #258-era removals (the verdict line, its "also:" list) stay gone. FINAL
//! chrome, a genuinely spacious pane (`reserve_key_hints`'s own `h > 12` floor), every page: ONE
//! line at the top (LLM SERVER STATUS or the page · host · model · UP · $/h · refreshed), and TWO
//! at the bottom - the tab strip ending in a dim `? help`, then the key-hint line under it. Its
//! own file: page 1's box layout is another card's.
//!
//! `LSS_CHROME_OUT=<dir>` writes the rendered chrome lines to files (for pasting onto the card).

use lss::data::PageId;
use lss::demo;
use lss::ui::{draw, App, View};
use lss_core::model::Status;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app_on(status: Status, view: View) -> (App, i64) {
    let now = status.generated_at + 3;
    // lss-verifier-2 on #258: the SAME three-server fleet `lss --demo` runs with, so the header
    // under test carries the " · [1/3]" tag the real one does (6 columns a fleet-less fixture
    // never measured)
    let fleet = demo::fleet(&status);
    let mut a = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, fleet, ..Default::default() };
    assert!(a.multi_server(), "the fixture is the demo's fleet");
    if let View::Page(p) = view {
        a.range_idx = 1;
        a.open(p);
        a.page_data = demo::page(a.status.as_ref().unwrap(), p, 1, now);
    }
    (a, now)
}

fn rows(a: &App, w: u16, h: u16, now: i64) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, a, now)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
}

fn views() -> Vec<View> {
    std::iter::once(View::Overview).chain(PageId::ALL.iter().map(|p| View::Page(*p))).collect()
}

/// What used to be chrome and must not come back anywhere on screen: page 1's verdict line and
/// its "also:" list (#258), and the scroll position on the bottom row (#291 - it never rode
/// there; #258 put it in the header, #291 dropped it from the header too, so it is checked as
/// gone from the WHOLE screen by `refreshed_survives_at_120_cols_with_a_fleet_on_every_view`'s
/// sibling assertions below instead of listed here, since "of" is too common a word to hunt for
/// across the whole render safely).
const GONE: [&str; 3] = ["worst now", "also:", "esc back"];

#[test]
fn the_chrome_is_one_line_at_the_top_and_two_at_the_bottom_of_every_page() {
    let status = demo::status();
    let out = std::env::var_os("LSS_CHROME_OUT");
    for view in views() {
        for (w, h) in [(60u16, 40u16), (120, 40), (200, 50)] {
            let (a, now) = app_on(status.clone(), view);
            let r = rows(&a, w, h, now);
            let n = r.len();
            let at = format!("{view:?} {w}x{h}:\n{}\n...\n{}", r[..3].join("\n"), r[n - 3..].join("\n"));
            // card #291: a spacious pane (every size here is, `reserve_key_hints`'s own `h > 12`
            // floor) now carries TWO bottom rows - the tab strip, then the key-hint line under it.
            let (top, tabs, hints) = (&r[0], &r[n - 2], &r[n - 1]);
            // top: the header - the page (or the product on page 1), host, UP
            let lead = match view {
                View::Page(p) => format!(" {} {}", p.key_digit(), p.title()),
                _ => " LLM SERVER STATUS".to_string(),
            };
            let short = match view {
                View::Page(p) => format!(" {} {}", p.key_digit(), p.title()),
                _ => " LSS".to_string(),
            };
            assert!(top.starts_with(&lead) || top.starts_with(&short), "the header leads the top row\n{at}");
            assert!(top.contains(" UP ") || top.contains(" DOWN"), "{at}");
            if w >= 120 {
                assert!(top.contains(&status.host) && top.contains("/h") && top.contains("refreshed "), "host, $/h and refreshed on the header\n{at}");
            }
            assert!(!top.contains(" of "), "card #291: the scroll position no longer rides the header\n{at}");
            // second row: the tab strip, ending in a dim "? help" - LABELLED from 120 cols (the
            // owner: "[0 overview] 1 latency 2 load ...")
            assert!(tabs.trim_end().ends_with("? help"), "{at}");
            if w >= 120 {
                assert!(tabs.contains("overview") && tabs.contains("1 latency") && tabs.contains("a advice"), "the labelled strip at {w} cols\n{at}");
                assert!(!tabs.contains(" of "), "the scroll position is not on the tab-strip row\n{at}");
            }
            // last row: card #291's key-hint line, wording lifted from `?` help's own key column
            assert!(hints.contains("? help") && hints.contains("quit"), "the key-hint line under the tab strip\n{at}");
            assert!(!hints.trim_end().ends_with("? help"), "the key-hint line is its own row, not another copy of the tab strip\n{at}");
            // exactly one header row and exactly two bottom rows: the second row and the
            // third-from-last are the page, not chrome
            assert!(!r[1].contains("LLM SERVER STATUS") && !r[1].contains("refreshed ") && !r[1].contains("COLLECTOR"), "a second header row\n{at}");
            assert!(!r[n - 3].contains("? help") && !r[n - 3].contains("refreshed ") && !r[n - 3].contains("quit"), "a third bottom row\n{at}");
            for gone in GONE {
                assert!(!r.iter().any(|l| l.contains(gone)), "{gone:?} is back on screen\n{at}");
            }
            if let (Some(dir), View::Overview | View::Page(PageId::Gpus)) = (&out, view) {
                let name = format!("{}-{w}.txt", if view == View::Overview { "page1" } else { "gpus" });
                std::fs::write(std::path::Path::new(dir).join(name), format!("{}\n...\n{}\n{}\n", top.trim_end(), tabs.trim_end(), hints.trim_end())).unwrap();
            }
        }
    }
}

/// lss-verifier-2's FAIL on 51eacc6, pinned on its own: at 120 cols with the demo's fleet,
/// "refreshed" is on the header of EVERY view - "restarts today" gives way first.
#[test]
fn refreshed_survives_at_120_cols_with_a_fleet_on_every_view() {
    let status = demo::status();
    for view in views() {
        let (a, now) = app_on(status.clone(), view);
        let top = rows(&a, 120, 40, now)[0].clone();
        assert!(top.contains("[1/3]") && top.contains("refreshed "), "{view:?} 120x40: restarts must give way before refreshed does: {top}");
    }
}

#[test]
fn an_unreachable_collector_rides_in_the_one_header_line() {
    let (mut a, now) = app_on(demo::status(), View::Overview);
    a.error = Some("connection refused".into());
    let r = rows(&a, 200, 40, now);
    assert!(r[0].contains("COLLECTOR UNREACHABLE (last seen"), "{}", r[..2].join("\n"));
    assert!(!r[1].contains("COLLECTOR"), "folded into the header, not a row of its own\n{}", r[..2].join("\n"));
}
