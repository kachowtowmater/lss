//! #48 ship gate: NOT part of CI, never asserts anything. Renders the real GPUS page from a
//! live collector at the three named sizes (140x44, one of the app's own "1/3-vertical" sizes,
//! one of its own "minimized" sizes) and dumps each cell's glyph + foreground colour as JSON, so
//! a colour-accurate picture can be built OUTSIDE this Mac's install (this never touches
//! ~/.local/bin or install.sh — it only writes JSON files next to the binary).
//!
//! Run against a real collector:
//!   LSS_CAPTURE_URL=http://<collector host>:8099 LSS_CAPTURE_OUT=/w/dist/capture \
//!     cargo test -p lss --test capture_ship_gate -- --ignored --nocapture capture_gpus_page
//!
//! 2026-09-21: the chart style became a runtime field (`App::chart_lines`, card #48's "look
//! with your own eyes" follow-up), not a compile-time const - so this now captures BOTH styles
//! in one run (no more flipping a const and rebuilding twice), one subdirectory each.

use lss::data::{fetch_page, PageCtx, PageId};
use lss::ui::{draw, App};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use std::io::Write;

/// The app's own named sizes (see crates/lss/tests/render.rs OVERVIEW_GOLDEN_SIZES): one
/// explicit size the owner named by hand, one representative of "third-v", one of "minimized".
const SIZES: [(&str, u16, u16); 3] = [("140x44", 140, 44), ("third-v_63x60", 63, 60), ("minimized_50x10", 50, 10)];

fn dump(buf: &Buffer, w: u16, h: u16, path: &std::path::Path) {
    let mut out = String::new();
    out.push_str(&format!("{{\"w\":{w},\"h\":{h},\"rows\":[\n"));
    for y in 0..h {
        out.push('[');
        for x in 0..w {
            let cell = &buf[(x, y)];
            let sym = cell.symbol().replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("[\"{sym}\",\"{:?}\"]", cell.fg));
            if x + 1 < w {
                out.push(',');
            }
        }
        out.push(']');
        if y + 1 < h {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]}\n");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path).unwrap().write_all(out.as_bytes()).unwrap();
}

#[test]
#[ignore = "hits a real collector: LSS_CAPTURE_URL=http://host:8099 LSS_CAPTURE_OUT=dir cargo test -p lss --test capture_ship_gate -- --ignored --nocapture capture_gpus_page"]
fn capture_gpus_page() {
    let url = std::env::var("LSS_CAPTURE_URL").expect("set LSS_CAPTURE_URL=http://<collector host>:8099");
    let out = std::env::var("LSS_CAPTURE_OUT").expect("set LSS_CAPTURE_OUT=<directory to write JSON captures into>");
    let out = std::path::PathBuf::from(out);

    let status = lss::client::fetch(&url).expect("fetch /status from the real collector");
    let now = status.generated_at;
    let ctx = PageCtx::of(&status);
    let page_data = fetch_page(&url, PageId::Gpus, 1, &ctx, now); // range_idx 1 == the default "1h" range
    assert!(page_data.series.is_some(), "no /series data came back for GPUS: {:?}", page_data.error);

    let mut app = App { url: url.clone(), last_ok: Some(now), status: Some(status), ..Default::default() };
    app.open(PageId::Gpus);
    app.page_data = page_data;

    for (dir, lines) in [("lines", true), ("dots", false)] {
        app.chart_lines = lines;
        for (name, w, h) in SIZES {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &app, now)).unwrap();
            let buf = t.backend().buffer().clone();
            dump(&buf, w, h, &out.join(dir).join(format!("gpus_{name}.json")));
            eprintln!("wrote {dir}/{name}");
        }
    }
}
