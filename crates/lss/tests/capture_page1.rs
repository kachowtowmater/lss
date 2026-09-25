//! #175 captures: the `v` page 1 (dash.rs) rendered from the DEMO status at the sizes the card
//! names, written as plain text so they can be pasted onto the card and diffed by the next person.
//! NOT part of CI, asserts nothing:
//!   LSS_CAPTURE_OUT=dir cargo test -p lss --test capture_page1 -- --ignored --nocapture

use lss::demo;
use lss::ui::{draw, App};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// 80x24 / 126x22 / 150x46 are the card's own; the owner's half-height layout is `Shape::HalfH`
/// (wide, 30+ rows) - two representatives of it, since his exact cell count is not recorded.
pub const SIZES: [(u16, u16); 5] = [(80, 24), (126, 22), (150, 46), (200, 32), (240, 34)];

/// Every screen of page 1 at `w`x`h`, top to bottom (PageDown until the page stops moving), each
/// under a `--- screen N ---` line, so a capture shows the whole page, not just its first screen.
pub fn page1_text(w: u16, h: u16) -> String {
    let status = demo::status();
    let now = status.generated_at + 3;
    let mut app = App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), dash: true, ..Default::default() };
    let mut out = String::new();
    let mut last = usize::MAX;
    for n in 1..=20 {
        let text = screen(&app, w, h, now);
        let (first, _, _) = app.page_rows.get();
        if first == last {
            break;
        }
        last = first;
        out.push_str(&format!("--- screen {n} ---\n{text}"));
        app.on_key(ratatui::crossterm::event::KeyCode::PageDown);
    }
    out
}

fn screen(app: &App, w: u16, h: u16, now: i64) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, app, now)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

#[test]
#[ignore = "writes capture files: LSS_CAPTURE_OUT=dir cargo test -p lss --test capture_page1 -- --ignored --nocapture"]
fn capture_page1() {
    let out = std::path::PathBuf::from(std::env::var("LSS_CAPTURE_OUT").expect("set LSS_CAPTURE_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    for (w, h) in SIZES {
        std::fs::write(out.join(format!("page1_{w}x{h}.txt")), page1_text(w, h)).unwrap();
    }
}
