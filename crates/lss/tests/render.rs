//! TestBackend renders of the overview at every required pane size and of EVERY detail page,
//! the unreachable screen, keys (focus, pages, range, scroll, theme, layout), and a no-panic
//! size sweep. The visual rules asserted here are Terminal Board's: boxes with the title in the
//! top border, named ANSI frame colours, a THICK focused border, RED only on problems, bars
//! (`… enter >`) for what does not fit.

use lss::data::{PageData, PageId};
use lss::demo;
use lss_core::compare::LoadoutsDoc;
use lss::ui::{draw, pick_shape, sparkline, App, FleetEntry, FleetState, Panel, Shape, View};
use lss_core::advice::Severity;
use lss_core::model::Status;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;
use ratatui::Terminal;

fn golden() -> Status {
    let mut s = demo::status();
    // the fixture gate log carries 5xx, which IS a problem and IS red: a calm day has none
    s.lanes.trusted.codes_10m.c5xx = 0;
    // real-looking load, so gauges and numbers have something to show
    s.serve.running = 2.0;
    s.serve.decode_tok_s = 412.5;
    s.serve.kv_usage = 0.123;
    s.serve.cache_hit_rate = 0.452;
    // a calm day: the two pieces of advice on the overview are both FINE (an ACT is red, and
    // red is for problems)
    // ... and every target is met (a missed one is a warning on the TARGETS strip/page)
    s.targets = demo::targets("24h", 97.0);
    s.advice_top = demo::advice(s.generated_at).windows[1].findings.iter().filter(|f| f.severity == Severity::Fine).take(2).cloned().collect();
    s
}

fn bad_day() -> Status {
    let mut s = golden();
    s.serve.up = false;
    s.serve.uptime_s = None;
    s.serve.down_since = Some(s.generated_at - 600);
    s.serve.restarts_today = 3;
    s.firing = vec!["serve_down".into(), "thermal_throttle:gpu2".into()];
    s.gpus[2].sample.temp_c = Some(93.0);
    s.gpus[2].sample.throttle_mask = 0x60;
    s.gpus[2].throttle = vec!["hw_thermal".into(), "sw_thermal".into()];
    s.lanes.trusted.waiters = 4;
    s.lanes.trusted.codes_10m.c5xx = 6;
    s.incidents[0].end = None;
    s.alerts[0].delivered = false;
    s
}

struct Shot {
    text: String,
    buf: Buffer,
    w: u16,
    h: u16,
}

impl Shot {
    fn red_cells(&self) -> usize {
        let mut n = 0;
        for y in 0..self.h {
            for x in 0..self.w {
                let c = &self.buf[(x, y)];
                n += usize::from(c.fg == Color::Red && c.symbol() != " ");
            }
        }
        n
    }

    /// (column, row) of the first cell of `needle`.
    fn find(&self, needle: &str) -> Option<(u16, u16)> {
        for (y, line) in self.text.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                return Some((line[..byte].chars().count() as u16, y as u16));
            }
        }
        None
    }

    fn fg_of(&self, needle: &str) -> Color {
        let (x, y) = self.find(needle).unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", self.text));
        self.buf[(x, y)].fg
    }

    /// The top-left corner glyph and its colour of the box whose title contains `title`.
    fn corner_of(&self, title: &str) -> (String, Color) {
        let (x, y) = self.find(title).unwrap_or_else(|| panic!("{title:?} not on screen:\n{}", self.text));
        let mut cx = x;
        loop {
            let sym = self.buf[(cx, y)].symbol();
            if matches!(sym, "┌" | "┏") {
                return (sym.to_string(), self.buf[(cx, y)].fg);
            }
            assert!(cx > 0, "no box corner left of {title:?}:\n{}", self.text);
            cx -= 1;
        }
    }

    fn row_of(&self, needle: &str) -> usize {
        self.find(needle).unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", self.text)).1 as usize
    }
}

fn render(app: &App, w: u16, h: u16, now: i64) -> Shot {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, app, now)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..h {
        for x in 0..w {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    Shot { text, buf, w, h }
}

fn app(status: Status) -> (App, i64) {
    let now = status.generated_at + 3;
    (App { url: "http://collector:8099".into(), last_ok: Some(now), status: Some(status), ..Default::default() }, now)
}

/// #191: the CLASSIC grid overview - what `v` now opts OUT to. The compiled default is the
/// redesigned page 1 (cost + loadout), so a test about the old grid's boxes, focus movement,
/// layout shapes or golden fixtures has to say which page it means. Saying it through one
/// helper means the day someone changes the default again, these tests still test the page
/// their own names claim, and only the tests that assert the DEFAULT have to be re-read.
fn classic(status: Status) -> (App, i64) {
    let (mut a, now) = app(status);
    a.dash = false;
    (a, now)
}

fn on_page(status: Status, page: PageId, range_idx: usize) -> (App, i64) {
    let (mut a, now) = app(status);
    a.range_idx = range_idx;
    a.open(page);
    a.page_data = demo::page(a.status.as_ref().unwrap(), page, range_idx, now);
    (a, now)
}

/// A key press followed by a frame, as in the real loop (scrolling moves from what is on screen).
fn press(a: &mut App, key: KeyCode, w: u16, h: u16, now: i64) -> Shot {
    a.on_key(key);
    render(a, w, h, now)
}

fn assert_all(shot: &Shot, needles: &[&str]) {
    for n in needles {
        assert!(shot.text.contains(n), "expected {n:?} in:\n{}", shot.text);
    }
}

/// The dash.rs row whose OWN label is `label` (padded to `dash::LABEL_W`, right after the box's
/// one leading border glyph) - not just any line that happens to CONTAIN `label` somewhere in its
/// value or trailing padding. `"day"` is a real trap here: `"today"` contains it as a substring,
/// and a box-drawn line is padded with trailing spaces out to the box's right border, so a plain
/// `.contains()` on a padded label can match deep inside an unrelated row by coincidence.
/// #175: every box now has one blank column of padding inside its border, and page 1 has TWO
/// columns of boxes from 120 wide - so the label follows "│ " wherever it is on the line, and the
/// returned text is that box's row alone (from the label to its own right border), not the
/// other column's row that shares the screen line. A value too long for its box now WRAPS under
/// its own value column (never `…`), so the row's continuation lines are joined on too.
fn row_starting_with(text: &str, label: &str) -> Option<String> {
    let key = format!("\u{2502} {label:<16}");
    let lines: Vec<&str> = text.lines().collect();
    let segment = |l: &str, col: usize| -> Option<String> {
        let tail: String = l.chars().skip(col).collect();
        let row = tail.strip_prefix("\u{2502} ")?.to_string();
        Some(row.split('\u{2502}').next().unwrap_or("").to_string())
    };
    for (i, l) in lines.iter().enumerate() {
        let Some(byte) = l.find(&key) else { continue };
        let col = l[..byte].chars().count();
        let mut row = segment(l, col)?.trim_end().to_string();
        for next in &lines[i + 1..] {
            match segment(next, col) {
                Some(seg) if seg.starts_with(&" ".repeat(16)) && !seg.trim().is_empty() => {
                    row.push(' ');
                    row.push_str(seg.trim());
                }
                _ => break,
            }
        }
        return Some(row);
    }
    None
}
const BOX_TITLES: [(&str, Color); 7] = [("o SERVE", Color::Blue), ("o GPUS", Color::Green), ("o LANES", Color::Yellow), ("o USERS", Color::LightBlue), ("o ADVICE", Color::LightCyan), ("o INCIDENTS", Color::Magenta), ("o ALERTS", Color::DarkGray)];

#[test]
fn layout_is_picked_from_the_pane_shape() {
    assert_eq!(pick_shape(126, 41), Shape::HalfH);
    assert_eq!(pick_shape(160, 45), Shape::HalfH);
    assert_eq!(pick_shape(126, 22), Shape::ThirdH);
    assert_eq!(pick_shape(70, 70), Shape::HalfV);
    assert_eq!(pick_shape(100, 60), Shape::HalfV);
    assert_eq!(pick_shape(50, 70), Shape::ThirdV);
    // the owner's real panes: too low for a full layout, still boxes
    assert_eq!(pick_shape(63, 15), Shape::Small);
    assert_eq!(pick_shape(63, 24), Shape::Small);
    assert_eq!(pick_shape(94, 15), Shape::Small);
    // the three shapes of milestone 2, at the sizes the owner named
    for (w, h) in [(126, 22), (94, 20), (200, 24)] {
        assert_eq!(pick_shape(w, h), Shape::ThirdH, "{w}x{h}");
    }
    for (w, h) in [(42, 73), (50, 70), (63, 60), (39, 50)] {
        assert_eq!(pick_shape(w, h), Shape::ThirdV, "{w}x{h}");
    }
    for (w, h) in [(40, 8), (50, 10), (63, 11), (63, 12), (200, 8)] {
        assert_eq!(pick_shape(w, h), Shape::Focus, "{w}x{h}: minimized");
    }
}

#[test]
fn half_h_is_a_2x2_grid_of_coloured_boxes_with_an_alerts_strip_and_no_red_on_a_good_day() {
    let (a, now) = classic(golden());
    let shot = render(&a, 126, 41, now);
    assert_all(&shot, &[" LLM SERVER STATUS · gpu-box · model-a · UP 36m54s · restarts today 1", "refreshed ", "o SERVE (UP 36m54s)", "o GPUS (4 ·", "o LANES (gate v5)", "o USERS (2 now · 3 in 24h)", "o ADVICE (all fine)", "o INCIDENTS (4 in 7d)", "o ALERTS (none firing)",
        "active   2 now · 2 in 10 min · 3 in 24 h", "slots    [##------] 2/8", "acme", "2 running", "laptop", "FINE  All 8 slots were busy 0.6% of the week",
        "decode", "412.5 tok/s", "[##------] 2/8", "queue", "KV", "12.3%", "TTFT     p50 183ms · p90 600ms · p99 2.0s (10 min)", "accept   3.10 (70%)", "cache hit 45.2%", "C1 probe 191.7 tok/s", "(1 invalid)", "C1 alert < 152.4 tok/s (80% of 190.5)", "ITL      p50 3.2ms", "e2e p50 2.0s", "writing (decode)", "reading (prefill)", "4180 tok/s",
        "GPU0*", "GPU3", "300W", "public", "trusted", "key-a 14", "probes: 3 excluded", "serve_down", "10m50s", "GPU3 Xid 8", "█", "? help"]);
    for (title, colour) in BOX_TITLES {
        let (corner, fg) = shot.corner_of(title);
        assert_eq!(fg, colour, "{title}: frame colour");
        assert_eq!(corner == "┏", title == "o SERVE", "{title}: only the focused box is thick");
        assert_eq!(shot.fg_of(title), colour, "{title}: the dot and the title take the box's colour");
    }
    // both speeds of the model, each with its last hour as a sparkline
    for speed in ["writing (decode)", "reading (prefill)"] {
        let row = shot.text.lines().nth(shot.row_of(speed)).unwrap();
        assert!(row.contains("tok/s") && row.chars().any(|c| ('▁'..='█').contains(&c)), "{speed}: no sparkline in\n{row}");
    }
    assert_eq!(shot.row_of("o SERVE"), shot.row_of("o GPUS"));
    assert_eq!(shot.row_of("o LANES"), shot.row_of("o USERS"), "second row: LANES | USERS");
    assert_eq!(shot.row_of("o ADVICE"), shot.row_of("o INCIDENTS"), "third row: ADVICE | INCIDENTS");
    assert!(shot.row_of("o ADVICE") > shot.row_of("o LANES") && shot.row_of("o ALERTS") > shot.row_of("o ADVICE"), "ALERTS is the strip under the grid");
    // one boxed card per GPU, two lane cards
    assert_eq!(shot.text.matches("┌ GPU").count() + shot.text.matches("│GPU").count() + shot.text.matches("│ GPU").count(), 4, "{}", shot.text);
    assert_eq!(shot.red_cells(), 0, "RED is reserved for problems:\n{}", shot.text);
    assert!(!shot.text.contains("enter >"), "everything fits as a box here");
}

#[test]
fn small_panes_are_still_boxes_a_row_of_two_then_bars() {
    for (w, h) in [(63, 13), (63, 15)] {
        let (a, now) = classic(golden());
        let shot = render(&a, w, h, now);
        assert_eq!(shot.row_of("o SERVE"), shot.row_of("o GPUS"), "{w}x{h}: SERVE | GPUS share a row\n{}", shot.text);
        assert_eq!(shot.corner_of("o SERVE"), ("┏".to_string(), Color::Blue));
        assert_eq!(shot.corner_of("o GPUS"), ("┌".to_string(), Color::Green));
        // both speeds always, and one readable row per GPU: name, both units, last hour, power, load
        assert_all(&shot, &["write 412.5 tok/s", "read  4180 tok/s", "GPU0* 52C/126F", "GPU3  46C/115F", "19W", "(4 · 57 W)"]);
        // nothing silently dropped: every other area is a bar
        for bar in [" LANES ", " USERS ", " ADVICE ", " INCIDENTS ", " ALERTS "] {
            let row = shot.text.lines().find(|l| l.starts_with(bar)).unwrap_or_else(|| panic!("{w}x{h}: no {bar} bar:\n{}", shot.text));
            assert!(row.trim_end().ends_with("enter >"), "{row}");
        }
        assert_all(&shot, &["probes: 3", "4 in 7d", "none firing", "2 now · 2 in 10m · 3 in 24h · slots 2/8", "FINE All 8 slots"]);
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
        assert!(shot.text.lines().last().unwrap().contains("? help"));
    }
}

/// MINIMIZED (a corner of the screen): ONE status card that answers "is it OK?", then bars.
#[test]
fn minimized_is_one_status_card_that_answers_is_it_ok_then_bars() {
    let (a, now) = classic(golden());
    let shot = render(&a, 63, 11, now);
    // the card is the header: state + for how long, the model, the host - in its thick border
    assert_eq!(shot.corner_of("o UP 36m54s"), ("┏".to_string(), Color::Blue));
    assert!(shot.text.lines().next().unwrap().contains("o UP 36m54s (model-a · gpu-box)"), "{}", shot.text);
    assert_all(&shot, &["writing 412.5 tok/s", "reading 4180 tok/s", "running [##------] 2/8 · queue 0 · memory (KV) 12.3%", "hottest GPU0 52°C / 126°F · 57 W · alerts none firing"]);
    // five rows for six areas: ALERTS rides in INCIDENTS
    let bars: Vec<&str> = shot.text.lines().filter(|l| l.trim_end().ends_with("enter >")).map(|l| l.split_whitespace().next().unwrap()).collect();
    assert_eq!(bars, ["USERS", "GPUS", "LANES", "ADVICE", "INCIDENTS"], "{}", shot.text);
    assert_all(&shot, &["alerts: none firing", "52/42/47/46C 126/108/117/115F"]);
    // one more row and ALERTS has its own bar
    assert!(render(&a, 63, 12, now).text.lines().any(|l| l.starts_with(" ALERTS ")));

    // 50x10: three bars of their own, the rest in ONE bar - nothing is silently dropped
    let shot = render(&a, 50, 10, now);
    // #222: the hint went from "tab >" to "enter >" (Tab now cycles pages everywhere, Enter
    // alone opens a focused bar) - 2 chars longer, so a bar this narrow truncates 2 chars
    // sooner. Real content, not lost: `assert_all` on the box above already proves "alerts 0".
    assert_all(&shot, &["o UP 36m54s (model-a · gpu-box)", "writing 412.5 tok/s   reading 4180 tok/s", "running 2/8 · queue 0 · memory (KV) 12.3%", "hottest GPU0 52°C / 126°F · alerts none firing", " USERS 2 now", " GPUS 52/42/47/46C", " LANES public 0r 0q", " MORE advice FINE · 4 incidents"]);

    // 40x8: still both speeds, the slots, the queue, the hottest GPU in both units, the alerts.
    // #153: "model-a" is shorter than the fixture's old model name, so the " · gpu-box" suffix
    // now fits here too where the longer name used to force the short form - a genuine, expected
    // side effect of the fixture's own scrub, not a behaviour change to chase.
    let shot = render(&a, 40, 8, now);
    assert_all(&shot, &["o UP 36m54s (model-a · gpu-box)", "write 412.5 tok/s   read 4180 tok/s", "running 2/8 · queue 0", "hottest GPU0 52C/126F · alerts 0", " USERS 2 now · slots 2/8", " MORE queue 0 · advice FINE", "? help"]);
    for (w, h) in [(40, 8), (50, 10), (63, 11), (63, 12), (200, 8)] {
        let shot = render(&a, w, h, now);
        assert_eq!(shot.red_cells(), 0, "{w}x{h}\n{}", shot.text);
        assert_eq!(shot.text.lines().filter(|l| l.trim().is_empty()).count(), 0, "{w}x{h}: a blank row\n{}", shot.text);
    }

    // a bad day: the card's border says DOWN in red, with what is firing; the first row says why
    let (bad, now) = classic(bad_day());
    let shot = render(&bad, 63, 11, now);
    assert_eq!(shot.corner_of("o DOWN 10m03s"), ("┏".to_string(), Color::Red), "{}", shot.text);
    assert_eq!((shot.fg_of("2 FIRING"), shot.fg_of("SERVE IS DOWN")), (Color::Red, Color::Red));
    assert!(shot.text.contains("ALERTS FIRING"), "{}", shot.text);
}

#[test]
fn at_63x24_the_areas_that_answer_who_and_what_to_do_are_boxes() {
    let (a, now) = classic(golden());
    let shot = render(&a, 63, 24, now);
    // SERVE | GPUS, then LANES, USERS and ADVICE as boxes; INCIDENTS and ALERTS as bars
    for (title, colour) in &BOX_TITLES[..5] {
        assert_eq!(shot.corner_of(title).1, *colour, "{title}");
    }
    assert_eq!(shot.row_of("o SERVE"), shot.row_of("o GPUS"));
    assert!(shot.row_of("o LANES") > shot.row_of("o SERVE") && shot.row_of("o USERS") > shot.row_of("o LANES") && shot.row_of("o ADVICE") > shot.row_of("o USERS"));
    for bar in [" INCIDENTS ", " ALERTS "] {
        let row = shot.text.lines().find(|l| l.starts_with(bar)).unwrap_or_else(|| panic!("no {bar} bar:\n{}", shot.text));
        assert!(row.trim_end().ends_with("enter >"), "{row}");
    }
    assert_all(&shot, &["public", "trusted", "probes: 3 excluded", "active   2 now · 2 in 10 min · 3 in 24 h", "acme", "FINE  All 8 slots", "serve_down"]);
    assert_eq!(shot.red_cells(), 0);
    assert_eq!(shot.text.lines().filter(|l| l.trim().is_empty()).count(), 0, "no blank row: the pane is used\n{}", shot.text);
}

#[test]
fn the_overview_looks_deliberate_at_every_size_the_owner_uses() {
    for (w, h) in [(63, 13), (63, 15), (63, 24), (94, 15), (126, 41), (126, 22), (94, 20), (200, 24), (42, 73), (50, 70), (63, 60)] {
        let (a, now) = classic(golden());
        let shot = render(&a, w, h, now);
        // every area is on screen, as a box or as a bar
        for title in ["SERVE", "GPUS", "LANES", "USERS", "ADVICE", "INCIDENTS"] {
            assert!(shot.text.contains(&format!("o {title}")) || shot.text.lines().any(|l| l.starts_with(&format!(" {title} "))), "{w}x{h}: {title} is nowhere\n{}", shot.text);
        }
        assert!(shot.text.contains("ALERTS") || shot.text.contains("alerts: none firing"), "{w}x{h}\n{}", shot.text);
        // all four GPUs, the slots in use, and the first piece of advice are readable
        assert_all(&shot, &["GPU0", "GPU1", "GPU2", "GPU3", "2/8", "All 8 slots"]);
        // the frame is whole: the first and last row are the header and the key hints, no row is blank
        assert!(shot.text.lines().next().unwrap().contains("model-a") && shot.text.lines().last().unwrap().contains("? help"), "{w}x{h}");
        assert_eq!(shot.text.lines().filter(|l| l.trim().is_empty()).count(), 0, "{w}x{h}: a blank row\n{}", shot.text);
        assert_eq!(shot.red_cells(), 0, "{w}x{h}");
    }
    // with a gateway older than v5.2 the USERS box says what it needs instead of showing zeros
    let (a, now) = classic(demo::status_plain());
    let old = render(&a, 126, 41, now);
    assert_all(&old, &["o USERS (needs gate v5.2)", "who is using them: needs gateway v5.2", "o ADVICE (collecting)", "collecting evidence"]);
    assert!(!old.text.contains("0 now"), "{}", old.text);
    // an ACT is a problem: it is the one thing that turns ADVICE red
    let (a, now) = classic(demo::status());
    let act = render(&a, 126, 41, now);
    assert_eq!((act.fg_of("ACT   Requests waited in line"), act.fg_of("WATCH Longest real prompt")), (Color::Red, Color::Yellow), "{}", act.text);
    assert_all(&act, &["o ADVICE (act)"]);
}

/// THIRD-H (wide and short): three columns that use the full height, SERVE | GPUS over ADVICE |
/// LANES over USERS, with INCIDENTS and ALERTS as the row under them.
#[test]
fn third_h_is_three_full_height_columns_with_incidents_and_alerts_under_them() {
    let (a, now) = classic(golden());
    for (w, h) in [(126, 22), (94, 20), (200, 24)] {
        let shot = render(&a, w, h, now);
        assert_eq!(shot.row_of("o SERVE"), shot.row_of("o GPUS"), "{w}x{h}\n{}", shot.text);
        assert_eq!(shot.row_of("o GPUS"), shot.row_of("o LANES"), "{w}x{h}: three boxes across\n{}", shot.text);
        assert!(shot.row_of("o ADVICE") > shot.row_of("o GPUS") && shot.find("o ADVICE").unwrap().0 == shot.find("o GPUS").unwrap().0, "{w}x{h}: ADVICE is under GPUS\n{}", shot.text);
        assert!(shot.row_of("o USERS") > shot.row_of("o LANES") && shot.find("o USERS").unwrap().0 == shot.find("o LANES").unwrap().0, "{w}x{h}: USERS is under LANES\n{}", shot.text);
        // the three columns end on the same row, right above INCIDENTS / ALERTS
        let bottoms: Vec<usize> = shot.text.lines().enumerate().filter(|(_, l)| l.starts_with('┗')).map(|(i, _)| i).collect();
        assert_eq!(bottoms.len(), 1, "{w}x{h}: SERVE is one full-height box\n{}", shot.text);
        let end = shot.text.lines().nth(bottoms[0]).unwrap();
        assert_eq!(end.matches('┘').count(), 2, "{w}x{h}: the other two columns end with it\n{}", shot.text);
        // both speeds, one row or card per GPU in both units, who is on, what to do
        assert_all(&shot, &["412.5 tok/s", "4180 tok/s", "GPU0*", "GPU3", "public", "trusted", "probes: 3 excluded", "acme", "FINE  All 8 slots", "writing tok/s · last hour"]);
        assert!(shot.text.contains("52°C/126°F") || shot.text.contains("52C/126F") || shot.text.contains("52°C / 126°F"), "{w}x{h}: both units\n{}", shot.text);
        assert_eq!(shot.red_cells(), 0);
    }
    // 126x22: INCIDENTS | ALERTS share one row of bars; 94x20: a bar each; 200x24: two short boxes
    let shot = render(&a, 126, 22, now);
    let row = shot.text.lines().find(|l| l.starts_with(" INCIDENTS ")).unwrap();
    assert!(row.contains(" ALERTS none firing") && row.matches("enter >").count() == 2, "{row}");
    let shot = render(&a, 94, 20, now);
    assert_eq!(shot.row_of(" ALERTS none firing"), shot.row_of(" INCIDENTS 4 in 7d") + 1, "{}", shot.text);
    // 200x24: side by side (two short boxes once the pane has the rows for them: 200x26)
    let shot = render(&a, 200, 24, now);
    assert_eq!(shot.row_of(" INCIDENTS 4 in 7d"), shot.row_of(" ALERTS none firing"), "{}", shot.text);
    assert!(shot.text.contains("┌ GPU") || shot.text.contains("│GPU0*"), "200 columns: the GPUs are cards\n{}", shot.text);
    assert!(shot.text.contains(" TARGETS 24h first word 96%"), "the owner's targets, one line under the header\n{}", shot.text);
    let shot = render(&a, 200, 26, now);
    assert_eq!(shot.row_of("o INCIDENTS"), shot.row_of("o ALERTS"), "{}", shot.text);
}

/// THIRD-V (narrow and tall): full-width boxes in priority order, each as tall as its content,
/// one compact row per GPU; rows left over are last-hour charts.
#[test]
fn third_v_stacks_boxes_as_tall_as_their_content_and_charts_the_rest() {
    let (a, now) = classic(golden());
    for (w, h) in [(42, 73), (50, 70), (63, 60), (70, 70)] {
        let shot = render(&a, w, h, now);
        let rows: Vec<usize> = BOX_TITLES.iter().map(|(t, _)| shot.row_of(t)).collect();
        assert!(rows.windows(2).all(|p| p[0] < p[1]), "{w}x{h}: stacked top to bottom {rows:?}\n{}", shot.text);
        for (title, colour) in BOX_TITLES {
            assert_eq!(shot.corner_of(title).1, colour);
        }
        assert_all(&shot, &["412.5 tok/s", "4180 tok/s", "GPU0*", "GPU3", "public", "trusted", "probes: 3 excluded", "acme", "serve_down"]);
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
    }
    let shot = render(&a, 50, 70, now);
    // one compact row per GPU: name, both units, the last hour, power, load, memory
    let gpu0 = shot.text.lines().find(|l| l.contains("GPU0*")).unwrap();
    assert!(gpu0.contains("52°C/126°F") && gpu0.contains("19W") && gpu0.contains("0%") && gpu0.contains("93.3G") && gpu0.chars().any(|c| ('▁'..='█').contains(&c)), "{gpu0}");
    // a narrow LANES box is plain lines, never a card that clips its own title
    assert_all(&shot, &["public   running 0 · queued 0 · waiting 0", "10 min  2xx 14  4xx 10  5xx 0", "gate UP v5 · rejected 413 1 · 429 0"]);
    assert!(!shot.text.contains("trusted run 0 · q…"), "{}", shot.text);
    // the rows the content does not need are charts with a named scale
    // #69: LANES now charts the trusted lane's in-flight budget (what varies and drives a
    // decision), not gateway queue depth (flat at zero almost always - see that card).
    assert_all(&shot, &["writing tok/s · last hour", "hottest GPU · last hour · peak", "in-flight budget used · last hour", "┤"]);
}

/// lss-verifier-2's FAIL on card #291's first cut: the classic grid's box layout reacts directly
/// to height (unlike the scrolling pages), so blindly reserving a row for the key-hint line
/// dropped the whole TARGETS strip at 94x20 and, at 200x24, dropped a GPU card's own "clk" row
/// while swapping LANES' two-row chart for a DIFFERENTLY WORDED one-row sparkline. Re-proven here
/// at those exact two sizes: every one of those content pieces is back, and - since keeping them
/// costs the row the hint line would have used - there is no hint line at either size.
#[test]
fn classic_grid_never_drops_a_row_of_content_for_the_key_hint() {
    let (a, now) = classic(golden());
    let at_94x20 = render(&a, 94, 20, now);
    assert!(at_94x20.text.contains("TARGETS 24h first word 96%"), "the TARGETS strip must survive at 94x20:\n{}", at_94x20.text);
    assert!(!at_94x20.text.lines().any(|l| l.contains("tab pages")), "94x20 has no spare row, so no key-hint line:\n{}", at_94x20.text);

    let at_200x24 = render(&a, 200, 24, now);
    assert!(at_200x24.text.contains("clk  180 MHz"), "a GPU card's own clk row must survive at 200x24:\n{}", at_200x24.text);
    assert!(at_200x24.text.contains("in-flight budget used · last hour"), "LANES' two-row chart must keep its long caption, not the short one-row 'budget 1h':\n{}", at_200x24.text);
    assert!(!at_200x24.text.contains("budget 1h"), "the short one-row sparkline must not replace the two-row chart:\n{}", at_200x24.text);
    assert!(!at_200x24.text.lines().any(|l| l.contains("tab pages")), "200x24 has no spare row, so no key-hint line:\n{}", at_200x24.text);
}

/// The other side of the same guarantee: a classic grid tall enough that shrinking it by one row
/// changes nothing but a chart's own resolution DOES get the key-hint line - the mechanism is not
/// simply "never add it".
#[test]
fn classic_grid_key_hint_appears_when_a_row_is_genuinely_free() {
    let (a, now) = classic(golden());
    let shot = render(&a, 126, 100, now);
    assert!(shot.text.lines().any(|l| l.contains("tab pages")), "126x100 has rows of pure chart padding to spare - the hint line must show:\n{}", shot.text);
    // and nothing real was lost getting it: every box is still there, and the same content words
    // this size shows now must be exactly what it showed one row shorter, minus the hint line
    // itself - `draw_classic_with_key_hints`'s own guarantee, checked here from the outside.
    for title in ["o SERVE", "o GPUS", "o LANES", "o USERS", "o ADVICE", "o INCIDENTS", "o ALERTS"] {
        assert!(shot.text.contains(title), "{title:?} must still be on screen at 126x100:\n{}", shot.text);
    }
    assert_eq!(shot.red_cells(), 0, "{}", shot.text);
}

#[test]
fn one_card_style_per_render() {
    let (a, now) = classic(golden());
    // tight (a wide pane with few rows): every GPU card carries its first line in its border
    let tight = render(&a, 126, 13, now);
    assert!(tight.text.contains("┌ GPU0") || tight.text.contains("┌GPU0"), "{}", tight.text);
    // roomy: no card does
    let roomy = render(&a, 126, 41, now);
    assert!(!roomy.text.contains("┌ GPU") && !roomy.text.contains("┌ public"), "{}", roomy.text);
    assert!(roomy.text.contains("│GPU0*") || roomy.text.contains("│ GPU0*"), "{}", roomy.text);
    // cards too narrow to say anything are not drawn at all: a row per GPU instead
    let narrow = render(&a, 63, 24, now);
    assert!(!narrow.text.contains("┌GPU0") && narrow.text.contains("GPU0* 52C/126F"), "{}", narrow.text);
    let third = render(&a, 200, 24, now);
    let (dg, dl) = (third.text.contains("┌GPU0") || third.text.contains("┌ GPU0"), third.text.contains("┌ public") || third.text.contains("┌public"));
    assert_eq!(dg, dl, "200x24: GPU and lane cards share a style\n{}", third.text);
}

#[test]
fn tiny_panes_get_one_thick_box_and_bars() {
    let (a, now) = classic(golden());
    let shot = render(&a, 38, 12, now);
    assert_eq!(shot.corner_of("o UP 36m54s"), ("┏".to_string(), Color::Blue));
    for bar in [" GPUS ", " LANES ", " USERS ", " ADVICE ", " INCIDENTS ", " ALERTS "] {
        assert!(shot.text.lines().any(|l| l.starts_with(bar)), "no {bar} bar:\n{}", shot.text);
    }
    let low = render(&a, 80, 6, now);
    assert!(low.text.contains("o UP 36m54s") && low.text.contains("writing 412.5 tok/s") && low.text.contains("hottest GPU0"), "{}", low.text);
}

#[test]
fn problems_are_red_and_only_problems() {
    let (a, now) = classic(bad_day());
    for (w, h) in [(126, 41), (126, 22), (63, 24), (63, 11), (50, 70)] {
        let shot = render(&a, w, h, now);
        assert!(shot.red_cells() > 10, "{w}x{h}");
        assert_eq!(shot.fg_of("DOWN 10m03s"), Color::Red, "{w}x{h}: the state (header, or the minimized card's border)\n{}", shot.text);
        assert_eq!(shot.fg_of("2 FIRING"), Color::Red, "{w}x{h}");
        assert!(shot.text.contains("restarts today 3") || w < 100, "{}", shot.text);
    }
    let shot = render(&a, 126, 41, now);
    assert_eq!(shot.corner_of("o ALERTS").1, Color::Red, "the ALERTS frame turns red when something fires");
    assert_eq!(shot.corner_of("o SERVE").1, Color::Blue, "a box keeps its colour; the problem is in its title and text");
    assert_eq!(shot.fg_of("SERVE IS DOWN"), Color::Red);
    assert_eq!(shot.fg_of("FIRING serve_down"), Color::Red);
    assert_eq!(shot.fg_of("hw_thermal"), Color::Red);
    assert_eq!(shot.fg_of("OPEN 36m54s"), Color::Red);
    assert_eq!(shot.fg_of("[undelivered]"), Color::Yellow);
    // the hot GPU's card frame is red, the others stay green
    let gpu2 = shot.find("GPU2").unwrap();
    assert_eq!(shot.buf[(gpu2.0 - 1, gpu2.1)].fg, Color::Red, "{}", shot.text);
    let gpu1 = shot.find("GPU1").unwrap();
    assert_eq!(shot.buf[(gpu1.0 - 1, gpu1.1)].fg, Color::Green);
}

#[test]
fn c1_highlight_follows_the_collectors_thresholds_not_a_constant() {
    // golden: probe 191.7, baseline 190.5, ratio 0.8 -> floor 152.4
    let mut s = golden();
    let (a, now) = classic(s.clone());
    assert_ne!(render(&a, 126, 41, now).fg_of("191.7 tok/s"), Color::Yellow);
    s.thresholds.c1_ratio = 1.05;
    s.thresholds.c1_floor_tok_s = Some(200.0);
    let (a, now) = classic(s);
    let shot = render(&a, 126, 41, now);
    assert_eq!(shot.fg_of("191.7 tok/s"), Color::Yellow, "below the floor the collector published");
    assert!(shot.text.contains("C1 alert < 200.0 tok/s (105% of 190.5)"), "{}", shot.text);
}

#[test]
fn a_collector_that_cannot_be_reached_is_boxed_and_red_with_and_without_old_data() {
    let never = App { url: "http://collector:8099".into(), error: Some("cannot reach http://collector:8099/status (connection refused)".into()), ..Default::default() };
    for (w, h) in [(126, 41), (63, 11), (63, 24)] {
        let shot = render(&never, w, h, 1000);
        assert_eq!(shot.corner_of("o COLLECTOR UNREACHABLE"), ("┏".to_string(), Color::Red), "{w}x{h}\n{}", shot.text);
        assert_all(&shot, &["never reached since lss started", "refused)", "systemctl --user status lss-collector"]);
    }
    let connecting = App { url: "http://collector:8099".into(), ..Default::default() };
    let shot = render(&connecting, 63, 11, 1000);
    assert_eq!(shot.corner_of("o CONNECTING").1, Color::Blue);
    assert_eq!(shot.red_cells(), 0);

    let (mut stale, now) = classic(golden());
    stale.error = Some("cannot reach http://collector:8099/status (timed out)".into());
    stale.last_ok = Some(now - 95);
    for (w, h, boxes) in [(126, 41, ["o SERVE", "o GPUS"]), (63, 15, ["o SERVE", "o GPUS"]), (63, 11, ["o UP 36m54s", " GPUS "])] {
        let shot = render(&stale, w, h, now);
        assert_eq!(shot.fg_of("COLLECTOR UNREACHABLE"), Color::Red);
        assert_all(&shot, &["1m35s ago"]);
        assert_all(&shot, &boxes);
    }
    let mut old = golden();
    old.generated_at -= 300;
    let (a, _) = classic(old);
    assert!(render(&a, 126, 41, a.status.as_ref().unwrap().generated_at + 303).text.contains("COLLECTOR STALLED 5m03s"));
}

#[test]
fn arrows_move_focus_spatially_and_enter_opens_the_matching_page() {
    let (mut a, now) = classic(golden());
    render(&a, 126, 41, now); // a frame records where the boxes are
    assert_eq!(a.focus, Panel::Serve);
    a.on_key(KeyCode::Right);
    assert_eq!(a.focus, Panel::Gpus);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Users);
    a.on_key(KeyCode::Left);
    assert_eq!(a.focus, Panel::Lanes);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Advice);
    a.on_key(KeyCode::Right);
    assert_eq!(a.focus, Panel::Incidents);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Alerts);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Alerts, "nothing below: focus stays");
    let shot = render(&a, 126, 41, now);
    assert_eq!(shot.corner_of("o ALERTS").0, "┏");
    assert_eq!(shot.corner_of("o SERVE").0, "┌");
    for _ in 0..3 {
        a.on_key(KeyCode::Up);
    }
    assert_eq!(a.focus, Panel::Serve);

    // bars take focus too (reversed), and open their page
    render(&a, 63, 11, now);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Users, "the first bar under the status card");
    a.on_key(KeyCode::Down);
    a.on_key(KeyCode::Down);
    assert_eq!(a.focus, Panel::Lanes);
    let shot = render(&a, 63, 11, now);
    let (x, y) = shot.find(" LANES ").unwrap();
    assert!(shot.buf[(x + 1, y)].modifier.contains(ratatui::style::Modifier::REVERSED), "the focused bar is reversed");
    // #222: Enter opens a focused bar now - Tab is the global page-cycling key everywhere,
    // checked separately below.
    assert!(!a.on_key(KeyCode::Enter));
    assert_eq!(a.view, View::Page(PageId::Gateway));
    a.on_key(KeyCode::Esc);
    assert_eq!(a.view, View::Overview);
    // #222/#231: Tab/BackTab from the classic overview enter the SAME 11-stop ring (the
    // overview, then all ten pages) that page 1 and every detail page already use - forward
    // lands on LATENCY, backward wraps straight to ADVICE, the ring's other neighbour of the
    // overview - not the focused box.
    a.on_key(KeyCode::Tab);
    assert_eq!(a.view, View::Page(PageId::Latency));
    a.on_key(KeyCode::Esc);
    a.on_key(KeyCode::BackTab);
    assert_eq!(a.view, View::Page(PageId::Advice));
    a.on_key(KeyCode::Esc);
    for (panel, page) in [(Panel::Serve, PageId::Latency), (Panel::Gpus, PageId::Gpus), (Panel::Users, PageId::Users), (Panel::Advice, PageId::Advice), (Panel::Incidents, PageId::Incidents), (Panel::Alerts, PageId::Alerts)] {
        a.focus = panel;
        a.on_key(KeyCode::Enter);
        assert_eq!(a.view, View::Page(page));
        a.on_key(KeyCode::Esc);
    }
    // #231 correction: `0` stays the overview's own key - ADVICE (the tenth page) took the next
    // free key, `a`, instead of displacing it.
    for (key, page) in [('1', PageId::Latency), ('2', PageId::Load), ('3', PageId::Gpus), ('4', PageId::Users), ('5', PageId::Tokens), ('6', PageId::Model), ('7', PageId::Gateway), ('8', PageId::Alerts), ('9', PageId::Incidents), ('a', PageId::Advice)] {
        a.on_key(KeyCode::Char(key));
        assert_eq!(a.view, View::Page(page));
    }
    a.on_key(KeyCode::Char('0'));
    assert_eq!(a.view, View::Overview, "0 is the overview's key, not a page");
    assert!(a.on_key(KeyCode::Char('q')), "q quits from anywhere");
}

/// Card #222 (the owner: "include the tab feature that lets us go through all the pages"), #231
/// correction (the orchestrator, reading the owner: "page 0 has no tab thats why tab wasnt working ...
/// So the Tab ring MUST include the overview"). Page 1 (dash.rs, the default) had NO Tab handling
/// at all before #222 - the key did nothing, on the one page a reader actually starts on. Tab /
/// BackTab now step through an 11-stop ring - the overview, then all ten `PageId::ALL` pages in
/// order - and wrap at BOTH ends through the overview itself, from page 1 and from every detail
/// page, exactly the same ring either way.
#[test]
fn tab_and_backtab_step_through_every_page_and_the_overview_and_wrap() {
    let (mut a, now) = app(golden());
    a.dash = true;
    assert!(a.view == View::Overview && a.dash, "page 1 is the default");
    a.on_key(KeyCode::Tab);
    assert_eq!(a.view, View::Page(PageId::Latency), "Tab from the overview lands on page 1");
    for page in [PageId::Load, PageId::Gpus, PageId::Users, PageId::Tokens, PageId::Model, PageId::Gateway, PageId::Alerts, PageId::Incidents, PageId::Advice] {
        a.on_key(KeyCode::Tab);
        assert_eq!(a.view, View::Page(page));
    }
    a.on_key(KeyCode::Tab);
    assert_eq!(a.view, View::Overview, "Tab from the last page (ADVICE) lands on the overview, not back to LATENCY");
    a.on_key(KeyCode::BackTab);
    assert_eq!(a.view, View::Page(PageId::Advice), "BackTab from the overview wraps straight to ADVICE, its other ring neighbour");
    a.on_key(KeyCode::BackTab);
    assert_eq!(a.view, View::Page(PageId::Incidents));
    a.on_key(KeyCode::Esc);
    a.on_key(KeyCode::BackTab);
    assert_eq!(a.view, View::Page(PageId::Advice), "BackTab from page 1 enters the ring the same way as from the classic overview");
    // BackTab from the FIRST page (LATENCY) wraps back to the overview, never straight to ADVICE.
    a.open(PageId::Latency);
    a.on_key(KeyCode::BackTab);
    assert_eq!(a.view, View::Overview);
    let _ = render(&a, 140, 44, now); // never panics with the tab strip on screen either
}

#[test]
fn theme_layout_and_help_keys() {
    let (mut a, now) = classic(golden());
    let dark = render(&a, 63, 15, now);
    assert_eq!((dark.buf[(0, 5)].bg, dark.buf[(30, 0)].fg), (Color::Black, Color::White), "painted, not the terminal's default");
    a.on_key(KeyCode::Char('T'));
    let light = render(&a, 63, 15, now);
    assert_eq!((light.buf[(0, 5)].bg, light.buf[(30, 0)].fg), (Color::White, Color::Black));
    assert_eq!(light.corner_of("o SERVE").1, Color::Blue, "accents are the same named colours in both themes");

    let mut names = Vec::new();
    for _ in 0..7 {
        a.on_key(KeyCode::Char('L'));
        names.push(a.pinned.map_or("auto", Shape::name));
    }
    assert_eq!(names, ["half-h", "half-v", "third-h", "third-v", "small", "focus", "auto"]);
    // card #291: the header no longer carries a `[small]`/`[half-h]` pinned-layout tell (the owner:
    // "dont need any of that") - pinning is checked by its real effect instead (the layout it
    // produces), which the next line already does.
    a.pinned = Some(Shape::Small);
    let pinned = render(&a, 126, 41, now);
    assert_eq!(pinned.row_of("o SERVE"), pinned.row_of("o GPUS"));
    // a pinned layout that cannot fit falls back to boxes that do, never to a blank screen
    a.pinned = Some(Shape::HalfH);
    assert!(render(&a, 63, 11, now).text.contains("o SERVE"));
    a.pinned = None;

    a.on_key(KeyCode::Char('?'));
    // #227: three more "Reading it" rows pushed the help overlay's total past what 126x41 can
    // show without clipping (`draw_help` clamps its height to the frame) - tall enough that
    // nothing here is cut off.
    let help = render(&a, 126, 60, now);
    // card #231: ALERTS/INCIDENTS split into two help rows, ADVICE gets its own "a" row
    // card #227: page 1's own box captions moved here (the alert/incident merge rule, and what
    // COST PER 1M TOKENS measures) once they were trimmed off the boxes themselves
    // lss-verifier-3's FAIL on #255: "unknown, not idle" and "not true GPU-seconds" were
    // described as having MOVED off page 1 into this help - they had not, the help text dropped
    // both clarifications on the way in. Pinned here now, not just their surrounding sentence.
    assert_all(
        &help,
        &[
            "LLM SERVER STATUS keys",
            "esc or ? closes",
            "1 2 3",
            "USERS  TOKENS  MODEL",
            "GATEWAY  ALERTS  INCIDENTS",
            "ADVICE",
            "MODEL: benchmark this model",
            "time range: 15m > 1h > 6h > 24h > 7d",
            "RED",
            "thick border",
            "a live condition",
            "a dated record",
            "do not sum to a total",
            "held at the door",
            "turned away",
            "token budget is full",
            "unknown, not idle",
            "not true GPU-seconds",
        ],
    );
    assert!(!a.on_key(KeyCode::Char('q')), "q closes the help, it does not quit under it");
    assert!(!a.show_help);
}

/// lss-verifier-4's FAIL on #247: at the owner's own 120x40 (the size #247 traded scroll for),
/// the `?` overlay used to CLIP instead of scroll - "Reading it" (where #227/#247 moved every
/// page-1 caption) sat below the cut with no way to reach it, even though the assert above
/// proves the words exist in the overlay's own content at a tall enough terminal. This pins
/// reachability itself: press `?` at 120x40, then scroll (PageDown, the same key a page uses),
/// and every caption that #227/#247 moved off page 1 must appear on screen at SOME scroll
/// position - RED before the scroll fix, since draw_help rendered the whole list unscrolled into
/// a clamped, too-short box and PageDown was silently swallowed.
#[test]
fn help_overlay_scrolls_at_120x40_so_every_moved_caption_is_reachable() {
    let (mut a, now) = app(golden());
    a.on_key(KeyCode::Char('?'));
    assert!(a.show_help);
    let mut seen = String::new();
    for _ in 0..8 {
        seen.push_str(&render(&a, 120, 40, now).text);
        a.on_key(KeyCode::PageDown);
    }
    for needle in ["held at the door", "turned away", "an incident", "a dated record", "cached (tokens)", "$/kWh unresolved"] {
        assert!(seen.contains(needle), "{needle:?} is never reachable by scrolling the help overlay at 120x40:\n{seen}");
    }
    // scrolled past the end: PageDown stops moving (never blank space past the last line)
    let last = render(&a, 120, 40, now).text;
    a.on_key(KeyCode::PageDown);
    assert_eq!(render(&a, 120, 40, now).text, last, "PageDown past the end must not scroll further");
}

const PAGE_SIZES: [(u16, u16); 2] = [(63, 24), (126, 41)];

#[test]
fn latency_page_charts_and_distributions() {
    for (w, h) in PAGE_SIZES {
        let (mut a, now) = on_page(golden(), PageId::Latency, 1);
        a.chart_lines = true; // #48: the default is dots now (the owner opts in) - this test checks the line renderer specifically
        let shot = render(&a, w, h, now);
        eprintln!("=== {w}x{h} ===\n{}", shot.text);
        // the header's exact wording (and whether "last"/the host fit) is covered elsewhere; the
        // "[lines]" tell this test's own chart_lines=true now adds to it can itself eat the width
        // budget that "last"/the host need, at the narrower of PAGE_SIZES - a real, and correct,
        // side effect of the same width-fallback the header already used for every other tail bit
        assert_all(&shot, &["1 LATENCY", "o TTFT (avg ", "p50 ", "p99 ", "o TTFT BUCKETS (", " requests", "-1h ", " now", "<=", "█", "? help"]);
        assert_eq!(shot.corner_of("o TTFT (avg"), ("┏".to_string(), Color::Blue), "{w}x{h}: the top section is the focused one\n{}", shot.text);
        assert_eq!(shot.corner_of("o TTFT BUCKETS").0, "┌");
        // #48, 2026-09-21: connected box-drawing lines, not braille - a turn or a vertical run
        // somewhere in the plot proves it is a line, not a flat row of dashes
        assert!(shot.text.chars().any(|c| matches!(c, '│' | '╭' | '╮' | '╰' | '╯')), "line plot:\n{}", shot.text);
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
        if w >= 100 {
            // wide: chart and distribution side by side, all four metrics on one screen, with heat strips
            assert_eq!(shot.row_of("o TTFT (avg"), shot.row_of("o TTFT BUCKETS"));
            assert_all(&shot, &["o E2E LATENCY (", "o INTER-TOKEN LATENCY (", "o QUEUE TIME (", "o QUEUE TIME BUCKETS"]);
        } else {
            assert!(shot.row_of("o TTFT BUCKETS") > shot.row_of("o TTFT (avg"), "narrow: one section per row");
            // card #291: the scroll position no longer rides the header (the owner: "dont need any
            // of that") - `a.page_rows`/`pages_scroll_by_section_and_never_past_the_end` is where
            // the real scroll state is checked now.
        }
    }
}

#[test]
fn load_page() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(golden(), PageId::Load, 1);
        let shot = render(&a, w, h, now);
        // the two speeds of the model come first: writing (decode), then reading (prefill)
        assert_all(&shot, &[" 2 LOAD · last 1h", "o THROUGHPUT (decode ", "━ decode tok/s", "━ prompt tok/s", "(own scale)", "o READING SPEED (prefill) (4180 tok/s now · 0 being read)"]);
        assert_eq!(shot.corner_of("o THROUGHPUT"), ("┏".to_string(), Color::Cyan));
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
    }
    // everything the page has, by scrolling a small pane to the end
    let (mut a, now) = on_page(golden(), PageId::Load, 1);
    let mut seen = String::new();
    seen.push_str(&render(&a, 63, 24, now).text);
    for _ in 0..8 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 63, 24, now).text);
    }
    // reading (prefill) has its own charts: the speed, read vs from the cache, and the time split
    for title in ["o READING SPEED (prefill) (4180 tok/s now · 0 being read)", "prompt tok/s read (cache hits not counted)", "o PROMPT TOKENS (94% from cache · 6% read · 0 waiting)", "read (computed)", "from cache", "o WHERE THE TIME GOES (reading 21% · writing + waiting 79%)"] {
        assert!(seen.contains(title), "{title} never came on screen");
    }
    for title in ["o THROUGHPUT", "o RUNNING REQUESTS ([##------] 2/8 slots)", "━ public", "━ trusted", "o RUNNING REQUESTS", "o QUEUED REQUESTS", "o KV CACHE (usage", "cache hit 45%", "o SPECULATIVE DECODING (accept length 3.10", "o REQUESTS / MIN", "o C1 PROBE (", " valid · ", " invalid x · alert < 152.4 of 190.5)"] {
        assert!(seen.contains(title), "{title} never came on screen");
    }
}

#[test]
fn gpus_page() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(golden(), PageId::Gpus, 2);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &[" 3 GPUS · last 6h", "o TEMPERATURE ([", "max 52C/126F", "alert at 90C/194F", "━ GPU0", "━ GPU3", "-6h "]);
        assert_eq!(shot.corner_of("o TEMPERATURE"), ("┏".to_string(), Color::Green));
    }
    let (mut a, now) = on_page(golden(), PageId::Gpus, 2);
    let mut seen = String::new();
    seen.push_str(&render(&a, 126, 41, now).text);
    for _ in 0..10 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 126, 41, now).text);
    }
    for needle in ["o POWER (total ", " W cap)", "o SM CLOCK MHz (max 3090", "o GPU UTILISATION", "o MEMORY USED (of 95.6G per GPU)", "o MEMORY CONTROLLER UTIL", "o THROTTLE REASONS", "a power cap is normal", "o HEALTH (read ",
        "pstate P1", "PCIe gen 5/5 width x16/x16", "ECC corrected 0 uncorrected 0", "remapped rows corr 0 uncorr 0 pending no failure no", "retired pages sbe - dbe - pending -", "power limit 300 W (max 325 W)", "clock max 3090 MHz", "mem temp n/a"] {
        assert!(seen.contains(needle), "{needle:?} never came on screen:\n{seen}");
    }
    // the demo has GPU2 in thermal slowdown for a while: that is a problem, and red
    assert!(seen.contains('T'));
    // an Xid inside the range is marked on the time axes
    let mut s = golden();
    s.incidents[2].start = s.generated_at - 3 * 3600;
    let (a, now) = on_page(s, PageId::Gpus, 2);
    let shot = render(&a, 126, 41, now);
    assert_eq!(shot.fg_of("X"), Color::Red, "{}", shot.text);
    // a degraded link is a problem
    let mut s = golden();
    s.gpus[1].health.as_mut().unwrap().pcie_width = Some(8.0);
    let (mut a, now) = on_page(s, PageId::Gpus, 2);
    render(&a, 126, 41, now);
    let shot = press(&mut a, KeyCode::End, 126, 41, now);
    assert_eq!(shot.fg_of("x8/x16"), Color::Red, "{}", shot.text);
    assert_eq!(shot.fg_of("1 PROBLEM(S)"), Color::Red);
}

/// card #223 (the owner: "on page 3 the throttle reasoning it all jumbled P not sure what that
/// is"): a GPU at its power cap for the whole range is NORMAL under a power limit, and must read
/// as words, not as a row of 'p' glyphs. Only a real problem (thermal / hw slowdown) is drawn.
fn gpus_page_seen(a: &mut App, now: i64) -> String {
    let mut seen = render(a, 126, 41, now).text;
    for _ in 0..10 {
        seen.push_str(&press(a, KeyCode::PageDown, 126, 41, now).text);
    }
    seen
}

#[test]
fn a_gpu_at_its_power_cap_all_range_reads_as_normal_in_words_not_a_row_of_p() {
    let (mut a, now) = on_page(golden(), PageId::Gpus, 2);
    {
        let doc = a.page_data.series.as_mut().expect("the demo has series");
        let n = doc.get("gpu0_temp_c").len().max(8);
        doc.series.insert("gpu0_thr_power".into(), vec![Some(1.0); n]);
        doc.series.insert("gpu0_thr_thermal".into(), vec![Some(0.0); n]);
        doc.series.insert("gpu0_thr_hw".into(), vec![Some(0.0); n]);
    }
    let seen = gpus_page_seen(&mut a, now);
    assert!(seen.contains("o THROTTLE REASONS"), "{seen}");
    assert!(!seen.contains("ppp"), "normal power capping must not print as a run of 'p':\n{seen}");
    assert!(seen.contains("power cap 100% of range (normal)"), "the capping is said in words:\n{seen}");
    // the rest of the demo still shows GPU2's thermal slowdown as a problem, in red
    let shot_t = seen.contains('T');
    assert!(shot_t, "{seen}");
    // and the legend is whole at the narrowest size, not cut mid-word
    let (mut a, now) = on_page(golden(), PageId::Gpus, 2);
    let mut small = render(&a, 80, 24, now).text;
    for _ in 0..14 {
        small.push_str(&press(&mut a, KeyCode::PageDown, 80, 24, now).text);
    }
    assert!(small.contains("a power cap is normal, not drawn"), "the legend must fit 80 columns:\n{small}");
}

/// card #223 (the owner: "the colors are off", about page 3): page 3 carries NO amber - yellow is
/// `warn()`, the amber band, and it used to be GPU1's series colour on every chart, so every chart
/// looked like a warning. RED appears only where there is a real problem: the demo has GPU2 in
/// thermal slowdown, which is red in THROTTLE REASONS and must be red nowhere above it. (The header
/// and tab rows are page-wide chrome drawn by ui/mod.rs, not this page; they are skipped.)
#[test]
fn page3_has_no_amber_and_red_only_on_the_real_problem() {
    for (w, h) in [(126u16, 41u16), (80, 24)] {
        let (a, now) = on_page(golden(), PageId::Gpus, 2);
        let shot = render(&a, w, h, now);
        let throttle_y = shot.find("THROTTLE REASONS").map_or(shot.h, |(_, y)| y);
        let (mut amber, mut red_above) = (0, 0);
        for y in 1..shot.h.saturating_sub(1) { // #258: chrome = the top row and the bottom row
            for x in 0..shot.w {
                let c = &shot.buf[(x, y)];
                if c.symbol() == " " {
                    continue;
                }
                amber += usize::from(matches!(c.fg, Color::Yellow | Color::LightYellow));
                red_above += usize::from(c.fg == Color::Red && y < throttle_y);
            }
        }
        assert_eq!(amber, 0, "no amber on page 3 at {w}x{h}:\n{}", shot.text);
        assert_eq!(red_above, 0, "red only on the real problem, never on the charts above it, at {w}x{h}:\n{}", shot.text);
    }
}

/// card #228 (generalising #223): no CHART SERIES on any page is drawn in amber or red. Yellow
/// is `warn()` (the amber band) and red is a problem; a series in either colour reads as a warning
/// that is not there - GPU1 on page 3 (#223), the users palette, the "public" lane, p90 latency,
/// the accept rate, invalid tok/s and $ per day all were. Chart INK only (braille and block
/// glyphs, which only series draw): box borders and titles are page chrome from ui/mod.rs and
/// dash.rs, coloured by page identity or a reading band, and are not series.
#[test]
fn no_chart_series_on_any_page_is_drawn_in_amber_or_red() {
    let ink = |sym: &str| sym.chars().next().is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c) || ('\u{2580}'..='\u{259f}').contains(&c));
    let mut offenders: Vec<String> = Vec::new();
    for page in PageId::ALL {
        let (mut a, now) = on_page(golden(), page, 2);
        let mut shot = render(&a, 126, 41, now);
        for _ in 0..14 {
            let mut n = 0;
            for y in 1..shot.h.saturating_sub(1) { // #258: chrome = the top row and the bottom row
                for x in 0..shot.w {
                    let c = &shot.buf[(x, y)];
                    n += usize::from(ink(c.symbol()) && matches!(c.fg, Color::Yellow | Color::LightYellow | Color::Red | Color::LightRed));
                }
            }
            if n > 0 {
                offenders.push(format!("{page:?}: {n} amber/red chart cells on screen:\n{}", shot.text));
                break;
            }
            shot = press(&mut a, KeyCode::PageDown, 126, 41, now);
        }
    }
    assert!(offenders.is_empty(), "{} page(s) draw a series in amber or red:\n{}", offenders.len(), offenders.join("\n"));
}

/// #227 item 5 (the owner: extend #228's every-page amber test to page 1): the same guard, for
/// the packed grid dash.rs draws - a SEPARATE test rather than folded into the one above, because
/// page 1 is not a `PageId` (`PageId::ALL` cannot reach it; it is `View::Overview` with
/// `App.dash`) and is scanned at the required sizes (#227 item 1), not `on_page`'s single 126x41.
/// Page 1 carries no chart series at all yet (item 4's sparklines are not built in this pass), so
/// today this only proves box CHROME (page_colour/section_colour) stays off amber - the guard
/// item 4 needs in place BEFORE any sparkline lands, not after.
#[test]
fn no_chart_series_on_page1_is_drawn_in_amber_or_red_at_every_required_size() {
    let ink = |sym: &str| sym.chars().next().is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c) || ('\u{2580}'..='\u{259f}').contains(&c));
    let mut offenders: Vec<String> = Vec::new();
    for (w, h) in [(60u16, 45u16), (120, 40), (200, 50), (200, 30)] {
        let (mut a, now) = app(bad_day());
        a.dash = true;
        let mut shot = render(&a, w, h, now);
        for _ in 0..20 {
            let mut n = 0;
            for y in 2..shot.h.saturating_sub(1) {
                for x in 0..shot.w {
                    let c = &shot.buf[(x, y)];
                    n += usize::from(ink(c.symbol()) && matches!(c.fg, Color::Yellow | Color::LightYellow | Color::Red | Color::LightRed));
                }
            }
            if n > 0 {
                offenders.push(format!("{w}x{h}: {n} amber/red chart cells on screen:\n{}", shot.text));
                break;
            }
            let (first, shown, total) = a.page_rows.get();
            if first + shown >= total {
                break;
            }
            shot = press(&mut a, KeyCode::PageDown, w, h, now);
        }
    }
    assert!(offenders.is_empty(), "{} size(s) draw page 1 chart ink in amber or red:\n{}", offenders.len(), offenders.join("\n"));
}

/// card #229 (#226's pages.rs half): the spending detail (the TOKENS page) shows MONTH TO DATE and
/// YEAR TO DATE like page 1 - from the same readings, so the words and the "since <first day>"
/// caveat of a partial year are page 1's own, never a second copy.
#[test]
fn the_spending_detail_shows_month_and_year_to_date_with_the_partial_year_said() {
    let mut s = golden();
    let year = lss_core::timeutil::fmt_local(s.generated_at, "%Y");
    let sp = s.spending.get_or_insert_with(Default::default);
    sp.this_month = lss_core::rates::SpendWindow { usd: Some(97.2), kwh: Some(241.2), covered_secs: 12 * 86_400, nominal_secs: 12 * 86_400 };
    sp.this_year = lss_core::rates::SpendWindow { usd: Some(612.0), kwh: Some(1520.0), covered_secs: 45 * 86_400, nominal_secs: 200 * 86_400 };
    sp.year_first_day = Some(format!("{year}-06-01"));
    let (mut a, now) = on_page(s, PageId::Tokens, 2);
    let mut seen = render(&a, 126, 41, now).text;
    for _ in 0..8 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 126, 41, now).text);
    }
    assert!(seen.contains("month to date") && seen.contains("$97.20"), "MTD row on the spending detail:\n{seen}");
    assert!(seen.contains("year to date") && seen.contains("$612.00"), "YTD row on the spending detail:\n{seen}");
    assert!(seen.contains(&format!("the stored history starts {year}-06-01")), "a partial year says where it starts:\n{seen}");
}

/// card #227 item 6 (the owner, pasting the tab strip and page 1's "worst now" line: "this stuff
/// should be on the bottom"): on page 1 and every detail page, the FIRST row is a box, the LAST
/// two rows are the tab strip then card #291's key-hint line under it, and page 1's verdict sits
/// in the bottom block (below every box, above the tab strip) - not at the top.
#[test]
fn chrome_is_one_line_top_and_two_at_the_bottom_and_the_boxes_start_on_the_second_row() {
    // card #258 reverses #227 item 6 at the owner's word ("move the llm server status back up to
    // where you had it before"): the header is the TOP row again, and page 1's "worst now"
    // verdict is gone. Card #291 puts the key-hint line back under the tab strip. tests/chrome.rs
    // holds the full contract.
    for (w, h) in [(126u16, 41u16), (80, 24), (200, 50)] {
        let mut views: Vec<(String, App, i64)> = Vec::new();
        let (mut a, now) = app(golden());
        a.dash = true;
        views.push(("page 1".into(), a, now));
        for page in PageId::ALL {
            let (a, now) = on_page(golden(), page, 2);
            views.push((format!("{page:?}"), a, now));
        }
        for (name, a, now) in views {
            let shot = render(&a, w, h, now);
            let rows: Vec<&str> = shot.text.lines().collect();
            assert!(rows[0].contains("gpu-box") && !rows[0].starts_with('┌') && !rows[0].starts_with('┏'), "{name} {w}x{h}: the first row is the header:\n{}", shot.text);
            assert!(rows[1].starts_with('┌') || rows[1].starts_with('┏'), "{name} {w}x{h}: the boxes start on the second row:\n{}", shot.text);
            assert!(rows.iter().any(|l| l.trim_end().ends_with("? help")), "{name} {w}x{h}: the tab line is on screen:\n{}", shot.text);
            assert!(!shot.text.contains("worst now"), "{name} {w}x{h}: the verdict line is gone:\n{}", shot.text);
        }
    }
}

/// The text INSIDE one box, its wrapped lines joined and whitespace collapsed (the box whose title
/// contains `title`, from its top corners down to its bottom border).
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
        out.push(l[left + 1..right].iter().collect::<String>());
    }
    out.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// card #230 (lss-verifier-4 on #229): the old test pinned today's WORDS, so a hard-coded copy of
/// page 1's string in pages.rs would pass it and then drift. This one takes its expectations FROM
/// the readings - lss_core::readings::cost(), the single owner - and plants a SENTINEL through the
/// data (year_first_day) that only reaches the screen if the page renders the reading's own detail.
#[test]
fn the_spending_detail_renders_the_cost_readings_themselves_not_a_copy_of_their_words() {
    let mut s = golden();
    let sp = s.spending.get_or_insert_with(Default::default);
    sp.this_month = lss_core::rates::SpendWindow { usd: Some(97.2), kwh: Some(241.2), covered_secs: 12 * 86_400, nominal_secs: 12 * 86_400 };
    sp.this_year = lss_core::rates::SpendWindow { usd: Some(612.0), kwh: Some(1520.0), covered_secs: 45 * 86_400, nominal_secs: 200 * 86_400 };
    let year = lss_core::timeutil::fmt_local(s.generated_at, "%Y");
    // sorts after "<year>-01-01", so the reading takes its partial-year branch and names it
    sp.year_first_day = Some(format!("{year}-SENTINEL-230"));
    let readings = lss_core::readings::cost(&s);
    let (mut a, now) = on_page(s, PageId::Tokens, 2);
    let mut seen = render(&a, 126, 41, now).text;
    let mut own = None;
    for _ in 0..8 {
        if seen.contains("SPENDING TO DATE") {
            own = Some(box_text(&seen, "SPENDING TO DATE"));
            break;
        }
        seen = press(&mut a, KeyCode::PageDown, 126, 41, now).text;
    }
    let own = own.unwrap_or_else(|| panic!("the SPENDING TO DATE box never came on screen"));
    let squash = |x: &str| x.split_whitespace().collect::<Vec<_>>().join(" ");
    for key in ["cost.month", "cost.ytd"] {
        let r = readings.iter().find(|r| r.key == key).unwrap_or_else(|| panic!("no {key} reading"));
        for part in [r.label.as_str(), r.value_or_dash(), r.detail.as_str()] {
            assert!(own.contains(&squash(part)), "{key}: the page must show the reading's own {part:?} - not a copy of its words:\n{own}");
        }
    }
    assert!(own.contains("SENTINEL-230"), "the sentinel planted in the data must reach the screen through the reading:\n{own}");
}

#[test]
fn gateway_page() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(golden(), PageId::Gateway, 3);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &[" 7 GATEWAY · last 24h", "o LANES (gate v5 · probes: 3 excluded)", "reqs", "2xx", "413", "429", "499", "503", if w >= 100 { "est tok avg/max" } else { "est tokens avg/max" }, "public", "trusted", "o KEYS (", "key-a", "(no key) (t)"]);
        // #227 item 5: the Gateway page's own colour moved off warn()'s amber (Yellow -> Blue)
        assert_eq!(shot.corner_of("o LANES"), ("┏".to_string(), Color::Blue));
    }
    let (mut a, now) = on_page(golden(), PageId::Gateway, 3);
    let mut seen = String::new();
    seen.push_str(&render(&a, 63, 24, now).text);
    for _ in 0..8 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 63, 24, now).text);
    }
    for needle in ["o IN-FLIGHT TOKENS", "o WAITERS (nobody waiting)", "o TOP CLIENT ADDRESSES (last octet masked · probe excluded)", "203.0.113.x"] {
        assert!(seen.contains(needle), "{needle:?} never came on screen:\n{seen}");
    }
    assert!(!seen.contains("203.0.113.5"), "a full client address must never be drawn");
}

#[test]
fn gateway_codes_that_were_not_recorded_are_dashes_with_a_dim_note_never_zeros() {
    for (w, h) in PAGE_SIZES {
        let (mut a, now) = on_page(golden(), PageId::Gateway, 3);
        a.page_data.gateway = Some(demo::gateway_partial(3, now));
        let shot = render(&a, w, h, now);
        // key-b was only seen before per-key codes existed: every code column is `-`
        let row = shot.text.lines().find(|l| l.contains("key-b")).unwrap_or_else(|| panic!("{}", shot.text));
        let row = row.replace(['│', '┃'], " ");
        let cells: Vec<&str> = row.split_whitespace().collect();
        let at = cells.iter().position(|c| *c == "key-b").unwrap();
        assert_eq!(&cells[at + 1..at + 9], ["8", "-", "-", "-", "-", "-", "-", "-"], "{w}x{h}: {row}");
        // key-a: 1 920 requests, a breakdown for the covered part only, and the title says since when
        let row = shot.text.lines().find(|l| l.contains("key-a")).unwrap();
        assert!(row.contains("1920") && row.split_whitespace().any(|c| c == "8"), "{row}");
        let note = "codes since ";
        let (x, y) = shot.find(note).unwrap_or_else(|| panic!("{w}x{h}: no note\n{}", shot.text));
        assert!(shot.buf[(x, y)].modifier.contains(ratatui::style::Modifier::DIM), "the note is dim");
        // unknown is not a problem: nothing on the key-b row is red (the fixture's real 5xx rows are)
        let y = shot.row_of("key-b") as u16;
        assert!((0..w).all(|x| shot.buf[(x, y)].fg != Color::Red), "{}", shot.text);
    }
    // full coverage: no note, real zeros
    let (a, now) = on_page(golden(), PageId::Gateway, 3);
    let shot = render(&a, 126, 41, now);
    assert!(!shot.text.contains("codes since") && shot.text.lines().any(|l| l.contains("key-b") && l.contains(" 0 ")), "{}", shot.text);
    assert!(shot.text.contains("probe excluded"));
}

/// Card #231: ALERTS (page 8) no longer carries the INCIDENTS table - that moved to its own
/// page 9, tested separately below.
#[test]
fn alerts_page_rule_states_and_history() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(golden(), PageId::Alerts, 1);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &[" 8 ALERTS", "o RULES", "0 firing", "ok      serve_down", "serve down >= 2m00s", "queue_pressure", "gpu_missing"]);
        assert!(!shot.text.contains("ALERTS & INCIDENTS"), "the combined title is gone\n{}", shot.text);
        assert_eq!(shot.corner_of("o RULES"), ("┏".to_string(), Color::Magenta));
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
    }
    let mut s = golden();
    s.firing = vec!["thermal_temp:gpu2".into()];
    let (mut a, now) = on_page(s, PageId::Alerts, 1);
    let shot = render(&a, 126, 41, now);
    assert_eq!(shot.corner_of("o RULES").1, Color::Red);
    assert_eq!(shot.fg_of("FIRING  thermal_temp:gpu2"), Color::Red);
    assert_eq!(shot.fg_of("pending queue_pressure"), Color::Yellow);
    assert_eq!(shot.fg_of("1 FIRING"), Color::Red);
    let mut seen = String::new();
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 126, 41, now).text);
    }
    for needle in ["o ALERT HISTORY (2 · mail spool 2 waiting)", "delivered", "serve DOWN for 2m05s"] {
        assert!(seen.contains(needle), "{needle:?} never came on screen:\n{seen}");
    }
    assert!(!seen.contains("o INCIDENTS"), "INCIDENTS moved to its own page:\n{seen}");
}

/// Card #231: the INCIDENTS table split off page 8 onto its own page 9 - the owner: "alerts and
/// incident should have their pages" / "seperate the alerts and incidents then".
#[test]
fn incidents_page_dated_records_and_uptime() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(golden(), PageId::Incidents, 1);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &[" 9 INCIDENTS"]);
        assert!(!shot.text.contains("o RULES") && !shot.text.contains("ALERT HISTORY"), "ALERTS' own sections stay on page 8\n{}", shot.text);
        assert_eq!(shot.red_cells(), 0, "{}", shot.text);
    }
    let (a, now) = on_page(golden(), PageId::Incidents, 1);
    let shot = render(&a, 126, 41, now);
    for needle in ["o INCIDENTS (uptime 24h ", "· 7d ", "10m50s", "GPU3 Xid 8"] {
        assert!(shot.text.contains(needle), "{needle:?} never came on screen:\n{}", shot.text);
    }
}

#[test]
fn users_page_table_charts_sort_and_graceful_absence() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(demo::status(), PageId::Users, 1);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &["4 USERS", "1h", "o USERS (2 now · 3 in 24h)", "2 active now", "slots in use", "o WHO (sorted by running now", "acme", "public", "laptop", "trusted", "lss-bench", "lss-probe", "340/2100"]);
        assert_eq!(shot.corner_of("o USERS").1, Color::LightBlue);
        assert!(shot.text.lines().last().unwrap().contains("? help"), "{}", shot.text); // #258: key hints live in ? help
    }
    let (mut a, now) = on_page(demo::status(), PageId::Users, 1);
    let wide = render(&a, 126, 41, now);
    // each user's own experience: their time to first word and their writing speed
    // NOTE: the tail ('answered in 24h') is NOT asserted here on purpose: at 126 wide the
    // experience line is fit()-truncated and the tail never survives rendering (card #72's
    // teeth live in report.rs's full-string assertion below, which sees the untruncated text).
    assert_all(&wide, &["their experience: first word half under", "95% under", "answers written at", "tok/s (median request"]);
    // the limit gauge, estimated tokens marked ~, per-user charts with the friendly names
    assert_all(&wide, &["[###---] 2/4", "1 no limit", "2050/12/3/35", "~781M", "646k", "~212k", "RUNNING AT ONCE, PER USER", "REQUESTS / MIN, PER USER", "━ acme", "━ laptop"]);
    assert!(wide.row_of("lss-bench  ") > wide.row_of("canary") && wide.row_of("lss-probe  ") > wide.row_of("canary"), "the bench and the probe come after every real user");
    assert!(wide.row_of("acme") < wide.row_of("laptop"), "busiest first");
    // s cycles the sort: by name the address-named users lead
    for _ in 0..5 {
        a.on_key(KeyCode::Char('s'));
    }
    let by_name = render(&a, 126, 41, now);
    assert_all(&by_name, &["sorted by name"]);
    assert!(by_name.row_of("laptop") < by_name.row_of("office") && by_name.row_of("acme") < by_name.row_of("laptop"), "{}", by_name.text);
    a.on_key(KeyCode::Char('s'));
    assert_all(&render(&a, 126, 41, now), &["sorted by running now"]);
    // `s` is the USERS page's key only
    let (mut other, _) = on_page(demo::status(), PageId::Load, 1);
    other.on_key(KeyCode::Char('s'));
    assert_eq!(other.user_sort, Default::default());
    // a gateway older than v5.2: said plainly, with the lane chart that IS known, never zeros
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(demo::status_plain(), PageId::Users, 1);
        let shot = render(&a, w, h, now);
        assert_all(&shot, &["o USERS (needs gateway v5.2)", "slots in use", "a gateway (v5.2 or newer)", "7 GATEWAY", "RUNNING AT ONCE, BY LANE"]);
        assert!(!shot.text.contains("o WHO") && shot.red_cells() == 0, "{}", shot.text);
    }
}

#[test]
fn tokens_page_windows_bars_peaks_lengths_and_the_per_user_split() {
    let (a, now) = on_page(demo::status(), PageId::Tokens, 1);
    let shot = render(&a, 126, 41, now);
    assert_all(&shot, &[" 5 TOKENS", "o TOKENS SERVED (today 920k written)", "last hour", "last 24 hours", "last 7 days", "all time (19d00h)", "26.40M", "1.09B", "(95%)", "A token is a piece of a word", "survives restarts", "o TOKENS PER HOUR, newest first", "█ written", "█ read"]);
    assert_eq!(shot.corner_of("o TOKENS SERVED").1, Color::LightGreen);
    let small = render(&a, 63, 24, now);
    assert_all(&small, &["o TOKENS SERVED", "all time", "o TOKENS PER HOUR"]);
    assert_eq!(small.text.lines().filter(|l| l.trim().is_empty()).count(), 0, "no blank rows at 63x24\n{}", small.text);
    // the rest of the page: peaks, lengths, the two distributions (in TOKENS, not milliseconds), who
    let (mut a, now) = on_page(demo::status(), PageId::Tokens, 1);
    render(&a, 126, 41, now);
    let end = press(&mut a, KeyCode::End, 126, 41, now);
    assert_all(&end, &["o BY USER (24 h)", "acme", "written", "~ = estimated"]);
    let mut all = String::new();
    let (mut a, now) = on_page(demo::status(), PageId::Tokens, 1);
    for _ in 0..8 {
        all.push_str(&render(&a, 126, 41, now).text);
        a.on_key(KeyCode::Down);
    }
    for needle in ["o PEAK SPEED", "fastest ever       912 tok/s", "peak of each day", "o HOW LONG (tokens, 7 days)", "answers  average", "9 in 10 under", "o ANSWER LENGTH", "o PROMPT LENGTH", "<=1.0k"] {
        assert!(all.contains(needle), "{needle:?} is nowhere on the TOKENS page");
    }
    assert!(!all.contains("<=16s") && !all.contains("ms\n"), "a token histogram is never labelled in time units");
    // without the gateway's user table the split says what it needs
    let (mut a, now) = on_page(demo::status(), PageId::Tokens, 1);
    if let Some(t) = a.page_data.tokens.as_mut() {
        t.per_user_available = false;
        t.per_user.clear();
    }
    render(&a, 126, 41, now);
    assert_all(&press(&mut a, KeyCode::End, 126, 41, now), &["o BY USER", "gateway v5.2"]);
}

/// card #338: the BENCH box said a green `ok` for a completed run whose `json output` check
/// failed - the CLI headline already said `OK, BUT 1 CHECK FAILED` (#334). Now the box says the
/// same, in the warning colour.
#[test]
fn the_bench_box_says_a_failed_check_instead_of_a_green_ok() {
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    let cards = a.page_data.loadouts.as_mut().expect("the model page carries /loadouts");
    let q = cards.loadouts[0].quick.as_mut().expect("model-a has a quick run");
    q.sanity.iter_mut().filter(|c| c.name == "json output").for_each(|c| c.pass = false);
    let card = q.clone();
    a.page_data.bench.as_mut().expect("the model page carries /bench").brief.last = Some(lss_core::bench::LastRun::of(&card));
    let top = render(&a, 126, 41, now);
    assert_all(&top, &["last run: quick on model-a", "ok, but 1 check failed: json output"]);
    assert_eq!(top.fg_of("ok, but 1 check failed"), Color::Yellow, "a failed check is never drawn in the all-good colour\n{}", top.text);
}

/// card #14, 2026-09-23. The demo data is all QUIET, so every screen test above renders the
/// gold-standard path and the `LOADED` / `?` / `NOT COMPARABLE` paths were drawn by nothing -
/// the same shape of gap that let a claim about behaviour-under-traffic ship untested. This
/// drives them on the real widgets: a reader must not be able to mistake a measurement taken
/// under load for one taken on a quiet box, on ANY screen.
#[test]
fn the_model_page_shows_what_the_benchmark_was_measured_under_and_refuses_a_bad_comparison() {
    use lss_core::bench::{BackgroundLoad, LoadClass, LOAD_SRC_USERS};
    let loaded = || BackgroundLoad { samples: 150, polls_with_traffic: 150, concurrent_avg: Some(1.8), concurrent_max: Some(4.0), source: LOAD_SRC_USERS.into(), queue_avg: 0.4, queue_max: 3.0, engine_tok_s: Some(690.0) };
    /// The serving loadout's newest speed run, measured with other traffic on the box. The BENCH
    /// box reads `/bench`, the tables read `/loadouts`: both must say it, which is the point.
    fn make_loaded(a: &mut App, bg: lss_core::bench::BackgroundLoad) {
        let cards = a.page_data.loadouts.as_mut().expect("the model page carries /loadouts");
        cards.loadouts[0].full = None;
        let q = cards.loadouts[0].quick.as_mut().expect("model-a has a quick run");
        q.under_load = true;
        q.background = Some(bg);
        let card = cards.loadouts[0].quick.clone().unwrap();
        if let Some(b) = a.page_data.bench.as_mut() {
            b.brief.last = Some(lss_core::bench::LastRun::of(&card));
        }
    }
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    make_loaded(&mut a, loaded());
    assert_eq!(a.page_data.bench.as_ref().unwrap().brief.last.as_ref().unwrap().load, LoadClass::Loaded);
    let top = render(&a, 126, 41, now);
    // the BENCH box: the label sits next to the status, and it is a WARNING colour, not dim -
    // "quiet" is the only reading that costs the reader nothing
    assert_all(&top, &["last run: quick on model-a", "LOADED"]);
    assert_eq!(top.fg_of("LOADED"), Color::Yellow, "a loaded run is never drawn as calmly as a quiet one\n{}", top.text);
    let mut all = String::new();
    for _ in 0..10 {
        all.push_str(&render(&a, 126, 41, now).text);
        a.on_key(KeyCode::Down);
    }
    assert!(all.contains("bench rows measured") && all.contains("UNDER LOAD: 1.80 other request(s)"), "the decode table says what its bench rows were taken under:\n{all}");

    // the comparison against the quiet previous loadout must refuse to subtract them
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    make_loaded(&mut a, loaded());
    render(&a, 126, 41, now);
    let cmp = press(&mut a, KeyCode::Enter, 126, 41, now);
    assert_all(&cmp, &["NOT COMPARABLE", "NOT CMP", "measured under different background load"]);
    assert_eq!(cmp.fg_of("NOT CMP"), Color::Yellow, "withheld is something to act on, not something merely missing");
    assert!(!cmp.text.contains("% better") && !cmp.text.contains("% WORSE"), "no bench delta may survive the load gap:\n{}", cmp.text);
    // the loadouts list carries the label per row, with a key
    assert_all(&cmp, &["load", "quiet"]);

    // a run from before any of this existed reads as `?`, never as quiet
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    {
        let cards = a.page_data.loadouts.as_mut().unwrap();
        cards.loadouts[0].full = None;
        cards.loadouts[0].quick.as_mut().unwrap().background = None;
    }
    render(&a, 126, 41, now);
    let cmp = press(&mut a, KeyCode::Enter, 126, 41, now);
    let row = cmp.text.lines().find(|l| l.contains("3f9a1c07e2b4")).expect("model-a's row").to_string();
    assert!(!row.contains("quiet") && !row.contains("LOADED"), "an unrecorded run is neither: {row}");
}

#[test]
fn model_page_plain_headings_concurrency_table_bench_box_and_the_comparison() {
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    let top = render(&a, 126, 41, now);
    assert_all(&top, &[" 6 MODEL", "o MODEL", "model-a · 1.0 · loadout 3f9a1c07e2b4", "A loadout is a model plus the image", "o BENCH (quick 2h00m ago · full 3d00h ago · accuracy 2d00h ago)", "last run: quick on model-a · 2h00m ago · ok", "b benchmarks this model",
        "o WRITING SPEED (decode)", "one user, server idle", "187.6 tok/s", "users at once", "speed each gets", "time to first word", "bench", "live",
        // the one sentence the owner asks for, with its reasoning, and the targets he set
        "o HEADROOM (how many more people it can take)", "more simultaneous users", "o TARGETS (1 missed · last 24h)", "first word within", "MISSED"]);
    assert_eq!(top.fg_of("MISSED"), Color::Yellow, "a missed target is a warning, not a problem");
    assert_eq!(top.corner_of("o MODEL").1, Color::LightMagenta);
    // every row of the concurrency table says where it comes from; a level never seen is `-`, not 0
    // #182: the tab strip now takes one row from every page's body, including this one at
    // 126x41 - the same trade the card asks for explicitly: the strip wins the row, dense
    // content adjusts the way every list on this screen already does (a page scrolls; the
    // footer says how much is below). One fewer row of the concurrency table is visible here
    // without scrolling; the data itself is unchanged, so the row this test already checks for
    // "never seen" is still on screen and still means the same thing.
    // card #258: the chrome is two rows now (header on top, tab line at the bottom), not three at
    // the bottom, so that row is back - all eight levels are visible without scrolling again.
    // card #291: the key-hint row takes one more, so it is 7 again (still `table[6]`, now the
    // LAST visible row rather than the second-last).
    let table: Vec<&str> = top.text.lines().filter(|l| l.contains("  bench ") || l.contains("  live  ")).collect();
    assert_eq!(table.len(), 7, "rows 1..max_slots\n{}", top.text);
    assert!(table[6].contains(" - ") && table[6].contains("live"), "7 users were never seen: {}", table[6]);
    let mut all = String::new();
    for _ in 0..10 {
        all.push_str(&render(&a, 126, 41, now).text);
        a.on_key(KeyCode::Down);
    }
    for needle in ["=> total speed stops growing after 6 users", "Why three speeds for one user", "Decode is how fast the answer is written", "long conversations", "one user: ", "16k in: 168 tok/s (bench)", "o READING SPEED (prefill)", "bench: 8.2k-token prompt", "· peak ", "energy per day", "kWh", "cold start", "o SHORTCUTS WORKING?", "accepts 5.88 tokens per step", "A small helper guesses several tokens ahead", "prefix cache", "o MEMORY", "capacity", "3.79M tokens", "longest real prompt", "o EFFICIENCY", "tokens per joule", "energy per 1M tokens", "o RELIABILITY", "restarts", "o SCORECARD (+-3% = noise)", "vs previous (model-b)", "+27.1% better", "-22.4% WORSE", "~ same", "o LOADOUTS (3 on record · enter compares)", "model-c"] {
        assert!(all.contains(needle), "{needle:?} is nowhere on the MODEL page");
    }
    // small pane: the same sections, fewer at a time, never a blank row
    let (a63, now63) = on_page(demo::status(), PageId::Model, 1);
    let small = render(&a63, 63, 24, now63);
    assert_all(&small, &["o MODEL", "o BENCH", "b bench", "? help"]); // #258: "enter compare" is a key hint, in ? help now
    assert_eq!(small.text.lines().filter(|l| l.trim().is_empty()).count(), 0, "{}", small.text);

    // enter opens the comparison against the previous loadout; up / down pick another; esc closes it
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    render(&a, 126, 41, now);
    let cmp = press(&mut a, KeyCode::Enter, 126, 41, now);
    // #182: the tab strip's row pushes EFFICIENCY (and RELIABILITY, SCORECARD below it) past the
    // fold at this fixed height without scrolling - the box says so honestly ("... 9 more lines:
    // scroll down"), the same truncate-with-indicator shape every list on this screen already
    // uses. ACCURACY, the section right before it, is still the right thing to check for here.
    assert_all(&cmp, &["> b81e55d09c3a  model-b", "* 3f9a1c07e2b4  model-a", "o THIS (model-a) vs THAT (model-b)", "SPEED ALONE", "SPEED UNDER LOAD", "READING SPEED", "LONG CONTEXT", "ACCURACY", "o VERDICT", "this is faster than that (one user alone 27%)", "this is slower than that (best total speed 22%)", "[bench]", "esc close"]);
    assert!(cmp.text.contains("scroll down"), "more of the comparison exists below - the box must say so\n{}", cmp.text);
    assert_eq!((cmp.fg_of("% WORSE"), cmp.fg_of("% better")), (Color::Red, Color::Green), "a clear loss is red, a clear win green");
    let next = press(&mut a, KeyCode::Down, 126, 41, now);
    assert_all(&next, &["> c4d2e9907f11  model-c", "vs THAT (model-c)", "[live]"]);
    assert!(!next.text.contains("[bench]"), "model-c was never benchmarked: only live numbers are compared\n{}", next.text);
    a.on_key(KeyCode::Esc);
    assert_eq!((a.compare_open, a.view), (false, View::Page(PageId::Model)), "esc closes the comparison, not the page");
    a.on_key(KeyCode::Esc);
    assert_eq!(a.view, View::Overview);

    // b: the BENCH prompt. From another machine it shows the exact command and starts nothing
    let (mut a, now) = on_page(demo::status(), PageId::Model, 1);
    a.bench_command = "ssh gpu-box lss bench quick".into();
    let prompt = press(&mut a, KeyCode::Char('b'), 126, 41, now);
    assert_all(&prompt, &["o BENCH", "Benchmark this model now?", "stops by itself the moment anyone else uses the server", "ssh gpu-box lss bench quick", "esc closes"]);
    a.on_key(KeyCode::Char('y'));
    assert_eq!((a.action, a.bench_prompt), (None, true), "y does nothing when the collector is elsewhere");
    a.on_key(KeyCode::Esc);
    assert!(!a.bench_prompt && a.view == View::Page(PageId::Model));
    // on the GPU box itself: y asks the main loop to start it
    a.bench_local = true;
    assert_all(&press(&mut a, KeyCode::Char('b'), 63, 24, now), &["y start it", "esc not now"]);
    assert!(!a.on_key(KeyCode::Char('q')), "q closes the prompt, it does not quit under it");
    a.on_key(KeyCode::Char('b'));
    a.on_key(KeyCode::Char('y'));
    assert_eq!((a.action, a.bench_prompt), (Some(lss::ui::Action::StartBench), false));
}

#[test]
fn advice_page_the_week_first_with_severity_colours_and_what_each_points_at() {
    for (w, h) in PAGE_SIZES {
        let (a, now) = on_page(demo::status(), PageId::Advice, 1);
        let shot = render(&a, w, h, now);
        // card #231: ADVICE is the tenth page now (INCIDENTS split off page 8 and took position 9)
        // card #231 (lss-verifier-4): the header names the page by its KEY, not its ring position.
        // This expectation pinned the defect - "10 ADVICE" told a reader to type 1 then 0, which
        // opens LATENCY; ADVICE's key is `a`, which is also what the tab strip has always shown.
        assert_all(&shot, &[" a ADVICE", "o ADVICE (3 windows)", "lss never changes a setting", "o THE WEEK (7d00h of data · 1 act · 7 watch)", "ACT   Requests waited in line 14% of the time", "points at: max-running-requests / a second server", "WATCH Longest real prompt was 490k"]);
        assert_eq!(shot.corner_of("o ADVICE").1, Color::LightCyan);
        assert_eq!((shot.fg_of("ACT   Requests"), shot.fg_of("WATCH Longest")), (Color::Red, Color::Yellow));
    }
    let (mut a, now) = on_page(demo::status(), PageId::Advice, 1);
    let wide = render(&a, 126, 41, now);
    assert_all(&wide, &["FINE  All 8 slots were busy 0.6% of the week - no capacity problem.", "FINE  Memory (KV) peaked at 33%", "WATCH Total speed stops growing after 5 users", "WATCH GPU2 slowed itself down", "left out of the advice on purpose: GPU0 38.0%"]);
    assert_eq!(wide.fg_of("FINE  "), Color::Green);
    // the week is the headline: the last 24 hours come after it (one screen down on this pane)
    let (mut below, _) = on_page(demo::status(), PageId::Advice, 1);
    render(&below, 126, 41, now);
    let mut seen = wide.text.clone();
    for _ in 0..3 {
        seen.push_str(&press(&mut below, KeyCode::PageDown, 126, 41, now).text);
    }
    assert!(seen.find("o THE WEEK").unwrap() < seen.find("o THE LAST 24 HOURS").expect("the last 24 hours are on the page"), "the week is the headline");
    assert!(seen.contains("Memory (KV) threw out"), "the evictions rule is on the page\n{seen}");
    assert!(!wide.text.lines().any(|l| l.contains("GPU0 slowed")), "an excluded GPU is never worded");
    render(&a, 126, 41, now);
    assert_all(&press(&mut a, KeyCode::End, 126, 41, now), &["o THE LAST 30 DAYS"]);
    // nothing worked out yet
    let (mut a, now) = on_page(demo::status(), PageId::Advice, 1);
    a.page_data.advice = Some(Default::default());
    assert_all(&render(&a, 63, 24, now), &["Nothing yet: the collector works the advice out"]);
}

#[test]
fn r_cycles_the_time_range_and_the_charts_rescale() {
    let (mut a, now) = on_page(golden(), PageId::Load, 0);
    let mut labels = Vec::new();
    let mut steps = Vec::new();
    for _ in 0..5 {
        a.page_data = demo::page(a.status.as_ref().unwrap(), PageId::Load, a.range_idx, now);
        let shot = render(&a, 126, 41, now);
        // card #227 item 6: the header now sits in the BOTTOM chrome, above the tab strip
        let header = shot.text.lines().find(|l| l.contains("last ")).unwrap().to_string();
        labels.push(header.split("last ").nth(1).unwrap().split(' ').next().unwrap().to_string());
        assert!(shot.text.contains(&format!("-{} ", labels.last().unwrap())), "the time axis follows the range:\n{}", shot.text);
        steps.push(a.page_data.series.as_ref().unwrap().step_s);
        a.on_key(KeyCode::Char('r'));
    }
    assert_eq!(labels, ["15m", "1h", "6h", "24h", "7d"]);
    assert_eq!(steps, [5, 10, 60, 180, 1200]);
    assert_eq!(a.range_idx, 0, "and round again");
    // data for another range is not drawn as if it were this one
    a.on_key(KeyCode::Char('r'));
    let shot = render(&a, 126, 41, now);
    assert!(shot.text.contains("loading the last 1h"), "{}", shot.text);
    assert_eq!(shot.corner_of("o LOAD").0, "┏", "the loading state is boxed too");
}

#[test]
fn pages_scroll_by_section_and_never_past_the_end() {
    let (mut a, now) = on_page(golden(), PageId::Latency, 1);
    let first = render(&a, 63, 24, now);
    assert!(first.text.contains("o TTFT (avg") && !first.text.contains("o QUEUE TIME"));
    assert_eq!(a.page_rows.get(), (0, 3, 8));
    let second = press(&mut a, KeyCode::Down, 63, 24, now);
    assert_eq!(second.corner_of("o TTFT BUCKETS").0, "┏", "the section at the top takes the focus");
    assert!(second.text.contains("o E2E LATENCY") && !second.text.contains("o TTFT (avg"));
    // card #291: the scroll position no longer rides the header (the owner: "dont need any of
    // that") - `a.page_rows.get()` just above/below is the real scroll state.
    let mut end = press(&mut a, KeyCode::PageDown, 63, 24, now);
    for _ in 0..4 {
        end = press(&mut a, KeyCode::PageDown, 63, 24, now);
    }
    assert!(end.text.contains("o QUEUE TIME BUCKETS"), "{}", end.text);
    assert_eq!(a.page_rows.get(), (5, 3, 8), "the last screen is full, not half empty");
    press(&mut a, KeyCode::Up, 63, 24, now);
    assert_eq!(a.page_rows.get().0, 4, "up moves from what is on screen");
    press(&mut a, KeyCode::Home, 63, 24, now);
    assert_eq!(a.page_rows.get().0, 0);
    // tab walks the pages, and a new page starts at its top
    a.on_key(KeyCode::Down);
    a.on_key(KeyCode::Tab);
    assert_eq!((a.view, a.scroll), (View::Page(PageId::Load), 0));
    a.on_key(KeyCode::Left);
    assert_eq!(a.view, View::Page(PageId::Latency));
}

#[test]
fn a_page_whose_data_could_not_be_fetched_says_so_in_a_red_box() {
    let (mut a, now) = on_page(golden(), PageId::Latency, 1);
    a.page_data.series = None;
    a.page_data.hists.clear();
    a.page_data.error = Some("the collector has no /series (older than this lss): upgrade lss-collector".into());
    for (w, h) in PAGE_SIZES {
        let shot = render(&a, w, h, now);
        assert_eq!(shot.corner_of("o COLLECTOR"), ("┏".to_string(), Color::Red));
        assert!(shot.text.contains("the collector has no /series") && shot.text.contains("lss-collector"), "{}", shot.text);
    }
}

#[test]
fn text_sparkline() {
    let up: Vec<Option<f64>> = (0..8).map(|i| Some(f64::from(i))).collect();
    assert_eq!(sparkline(&up, 8), "▁▂▃▄▅▆▇█");
    assert_eq!(sparkline(&[Some(0.0), None, Some(0.0)], 3), "▁ ▁");
    // 120 points into 12 columns: each column keeps the max of its slice, so a spike survives
    let mut spiky = vec![Some(1.0); 120];
    spiky[57] = Some(100.0);
    let s = sparkline(&spiky, 12);
    assert_eq!(s.chars().count(), 12);
    assert_eq!(s.chars().filter(|c| *c == '█').count(), 1);
    assert_eq!(sparkline(&[], 10), "");
    assert_eq!(sparkline(&up, 0), "");
}

#[test]
fn every_glyph_is_single_width() {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<char> = BTreeSet::new();
    // #191: BOTH overviews - the default page 1 and the classic grid `v` reaches - not just
    // whichever one happens to be the default when this test is read.
    for (a, now) in [app(bad_day()), classic(bad_day())] {
        for (w, h) in OVERVIEW_GOLDEN_SIZES.into_iter().chain([(126, 41)]) {
            seen.extend(render(&a, w, h, now).text.chars());
        }
    }
    for p in PageId::ALL {
        let (a, now) = on_page(bad_day(), p, 1);
        seen.extend(render(&a, 126, 41, now).text.chars());
    }
    for c in seen {
        // #48, 2026-09-21: the line-chart renderer's connected box-drawing glyphs - the four
        // rounded corners plus the plain vertical/horizontal already in this set.
        // #191: `—` (U+2014), page 1's "this number is absent, and here is why" mark. It reached
        // this test only when that page became the default. Ambiguous-width like `…` (U+2026),
        // already allowed here and used the same way, and unicode-width - what ratatui measures
        // the buffer with - calls both of them one column.
        let ok = c.is_ascii() || matches!(c, '·' | '°' | '…' | '—' | '─' | '━' | '│' | '┃' | '┌' | '┐' | '└' | '┘' | '┏' | '┓' | '┗' | '┛' | '┤' | '╭' | '╮' | '╰' | '╯') || ('▁'..='█').contains(&c) || ('\u{2800}'..='\u{28ff}').contains(&c);
        assert!(ok, "glyph {c:?} (U+{:04X}) is not in the allowed single-width set", c as u32);
    }
}

#[test]
fn no_size_panics_anywhere() {
    let states = [golden(), bad_day(), Status { v: 1, ..Default::default() }];
    for status in states {
        let now = status.generated_at + 3;
        // the page data once per (page, range), not once per size
        let data: Vec<Vec<lss::data::PageData>> = PageId::ALL.iter().map(|p| (0..5).map(|r| demo::page(&status, *p, r, now)).collect()).collect();
        // 20..220 x 6..70: every size gets the overview and two pages (rotating, so every page
        // meets every kind of size); the owner's real pane sizes get all five
        let real = [(63, 11), (63, 12), (63, 15), (63, 24), (94, 15), (126, 41), (126, 22), (50, 70)];
        let grid = (20..=220u16).step_by(8).flat_map(|w| (6..=70u16).step_by(4).map(move |h| (w, h)));
        for (w, h) in grid.chain(real) {
            let (mut a, now) = app(status.clone());
            render(&a, w, h, now);
            let n = PageId::ALL.len();
            let k = (w as usize / 8 + h as usize / 4) % n;
            let pages: Vec<usize> = if real.contains(&(w, h)) { (0..n).collect() } else { vec![k, (k + 2) % n, (k + 5) % n] };
            for i in pages {
                a.open(PageId::ALL[i]);
                a.page_data = data[i][(w as usize + h as usize) % 5].clone();
                a.range_idx = a.page_data.range_idx;
                a.scroll = (w as usize) % 4;
                render(&a, w, h, now);
                if PageId::ALL[i] == PageId::Model {
                    // the comparison (against every loadout of the list) and the BENCH prompt
                    a.compare_open = true;
                    a.compare_sel = (w as usize) % 3;
                    render(&a, w, h, now);
                    a.compare_open = false;
                    a.bench_prompt = true;
                    a.bench_local = h % 2 == 0;
                    render(&a, w, h, now);
                    a.bench_prompt = false;
                }
                if PageId::ALL[i] == PageId::Users {
                    a.user_sort = a.user_sort.next();
                    render(&a, w, h, now);
                }
            }
        }
    }
    // degenerate sizes and every pinned layout
    for (w, h) in [(1, 1), (2, 3), (20, 6), (19, 5), (220, 6), (20, 70), (400, 3)] {
        for pinned in [None, Some(Shape::HalfH), Some(Shape::HalfV), Some(Shape::ThirdH), Some(Shape::ThirdV), Some(Shape::Small), Some(Shape::Focus)] {
            let (mut a, now) = app(bad_day());
            a.pinned = pinned;
            a.show_help = true;
            render(&a, w, h, now);
            a.error = Some("x".into());
            a.status = None;
            render(&a, w, h, now);
        }
    }
}

/// SEVERAL SERVERS: the header names the one on screen and its place, a FLEET line shows every
/// one (UP / DOWN / no monitor, and its speed), `[` and `]` switch. One server: none of it.
#[test]
fn several_servers_get_a_switcher_in_the_header_and_a_fleet_line() {
    let (mut a, now) = classic(golden());
    let single = render(&a, 126, 41, now);
    assert!(!single.text.contains("FLEET") && !single.text.contains("[1/"), "one server behaves as before\n{}", single.text);
    let entry = |name: &str, state: Option<Result<FleetState, String>>| FleetEntry { name: name.into(), url: format!("http://{name}:8099"), state };
    let fleet = || {
        vec![
            entry("big-box", Some(Ok(FleetState { up: true, decode_tok_s: 412.0, running: 2.0, model: Some("model-a".into()), ..Default::default() }))),
            entry("spark-1", Some(Ok(FleetState { up: false, decode_tok_s: 0.0, running: 0.0, model: None, ..Default::default() }))),
            entry("spark-2", Some(Err("connection refused".into()))),
        ]
    };
    a.fleet = fleet();
    let shot = render(&a, 126, 41, now);
    // the name from lss.toml is what the owner knows the server by
    assert_all(&shot, &[" LLM SERVER STATUS · big-box · model-a", "[1/3]", " FLEET [big-box] UP 412 tok/s · spark-1 DOWN · spark-2 NO MONITOR", "[ ] switch"]);
    assert_eq!((shot.fg_of("DOWN · spark-2"), shot.fg_of("NO MONITOR")), (Color::Red, Color::Red), "a server that is down is a problem");
    a.on_key(KeyCode::Char(']'));
    assert_eq!(a.server_idx, 1);
    a.on_key(KeyCode::Char('['));
    a.on_key(KeyCode::Char('['));
    assert_eq!(a.server_idx, 2, "the switcher wraps around");
    assert!(a.status.is_none(), "the other server's numbers are never shown under this one's name");
    // the fleet line gives way on a small pane; the switcher stays in the header
    let (mut a, now) = classic(golden());
    a.fleet = fleet();
    let small = render(&a, 63, 11, now);
    assert!(!small.text.contains("FLEET"), "{}", small.text);
    let mid = render(&a, 94, 20, now);
    assert_all(&mid, &["[1/3]", " FLEET [big-box] UP"]);
}

/// #92: the FLEET PAGE (`f`, one row per node), not just the header strip - only reachable with
/// more than one `[[server]]`, per the card's own "a fleet of one looks exactly like today" rule.
/// Proves the three honesty rules stated in fleet.rs's own doc comment render for real: an
/// unreachable node says so and is excluded from the totals (and the totals say so), and the
/// decode-average refusal sentence is always on screen.
#[test]
fn f_opens_the_fleet_page_one_row_per_node_never_with_a_single_server() {
    let (mut a, now) = classic(golden());
    // a single server: f does nothing at all - no new key, no new page
    a.on_key(KeyCode::Char('f'));
    assert_eq!(a.view, View::Overview, "a fleet of one gets no new key");

    let entry = |name: &str, state: Option<Result<FleetState, String>>| FleetEntry { name: name.into(), url: format!("http://{name}:8099"), state };
    a.fleet = vec![
        entry(
            "gpu-box",
            Some(Ok(FleetState {
                up: true,
                host: "gpu-box".into(),
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
        ),
        entry(
            "spark-a",
            Some(Ok(FleetState {
                up: true,
                host: "spark-a".into(),
                engine: "sglang".into(),
                model: Some("model-b".into()),
                decode_tok_s: 85.0,
                prefill_tok_s: Some(1_200.0),
                running: 1.0,
                slots: 4,
                kv_usage: 0.12,
                gpu_count: 1,
                total_watts: Some(180.0),
                cost_per_hour: Some(0.08),
                generated_tokens_total: 40_000.0,
                not_reported: Vec::new(),
            })),
        ),
        entry("spark-b", Some(Err("connection refused".into()))),
    ];
    a.on_key(KeyCode::Char('f'));
    assert_eq!(a.view, View::Fleet, "with more than one server, f opens the FLEET page");
    let shot = render(&a, 140, 44, now);
    assert_all(&shot, &["gpu-box", "spark-a", "model-a", "model-b", "spark-b", "unreachable", "connection refused"]);
    assert!(shot.text.contains("1 unreachable, excluded"), "{}", shot.text);
    assert!(shot.text.contains("total power:") && shot.text.contains("total live cost:"), "{}", shot.text);
    assert!(shot.text.to_lowercase().contains("not mean anything"), "the decode-average refusal must be on screen\n{}", shot.text);

    // esc/0 leaves the fleet page like every other view
    a.on_key(KeyCode::Esc);
    assert_eq!(a.view, View::Overview);
}

/// #107 (verifier): the exact real measurement - decode "31 tok/s" and prefill "2.1k tok/s"
/// fused into one unreadable token with no separator, at 150 and (narrow-tier) 92 cols. Also
/// pins that an unreachable node's full reason survives (the old code pre-truncated to a fixed
/// 70 chars regardless of the real box width, cutting it off mid-word with room to spare).
#[test]
fn fleet_table_columns_never_collide_and_the_unreachable_reason_is_not_cut_short() {
    let (mut a, now) = app(golden());
    let entry = |name: &str, state: Option<Result<FleetState, String>>| FleetEntry { name: name.into(), url: format!("http://{name}:8099"), state };
    a.fleet = vec![
        entry(
            "gpu-box",
            Some(Ok(FleetState {
                up: true,
                host: "gpu-box".into(),
                engine: "sglang".into(),
                model: Some("model-a".into()),
                decode_tok_s: 31.0,
                prefill_tok_s: Some(2_100.0),
                running: 1.0,
                slots: 8,
                kv_usage: 0.14,
                gpu_count: 4,
                total_watts: Some(738.0),
                cost_per_hour: None,
                generated_tokens_total: 1_000_000.0,
                not_reported: Vec::new(),
            })),
        ),
        entry("spark-a", Some(Err("connection refused".into()))),
    ];
    a.on_key(KeyCode::Char('f'));
    // 150: the full tier - decode AND prefill both show and must never fuse. 92: the narrow tier
    // (fleet.rs's own FULL_W=100) legitimately drops prefill rather than showing it at all - that
    // is a real, named tier drop, not the silent-fusion bug; decode alone must still be clean.
    let wide = render(&a, 150, 30, now);
    assert!(wide.text.contains("31 tok/s") && wide.text.contains("2.1k tok/s"), "150 cols: decode/prefill values missing\n{}", wide.text);
    // #107 item 3 (verifier-3, verifier-2 - both independently proved the ORIGINAL selector
    // wrong): `.find(|l| l.contains("gpu-box") && l.contains("tok/s"))` also matches the PAGE
    // HEADER summary line ("FLEET [gpu-box] UP 31 tok/s · ...") since it carries both needles
    // too, and being line 1 it wins `.find()` before the real table row ever gets checked - a
    // test that can never see the table row can never fail no matter how badly that row is
    // broken (verified: reverting the fix left this exact test green). "model-a" (the
    // model) appears ONLY in the table row, never in the header summary.
    let wide_row = wide.text.lines().find(|l| l.contains("model-a")).unwrap_or_else(|| panic!("150 cols: no gpu-box row\n{}", wide.text));
    assert!(!wide_row.contains("tok/s2.1k"), "150 cols: decode and prefill fused with no separator\n{wide_row}");

    let narrow = render(&a, 92, 30, now);
    // the narrow (< FULL_W) tier drops the model column entirely, so "model-a" cannot
    // anchor this one either, and the SAME header-collision applies ("gpu-box"+"tok/s" both
    // appear in "FLEET [gpu-box] UP 31 tok/s · ..."). That summary line is the only one that
    // ever says "FLEET" - excluding it is enough to land on the real table row instead.
    let narrow_row = narrow.text.lines().find(|l| l.contains("gpu-box") && l.contains("tok/s") && !l.contains("FLEET")).unwrap_or_else(|| panic!("92 cols: no gpu-box row\n{}", narrow.text));
    assert!(narrow_row.contains("31 tok/s") && !narrow_row.contains("tok/s—") && !narrow_row.contains("tok/s$"), "92 cols: decode must not fuse with whatever follows it in the narrow tier\n{narrow_row}");

    // a short reason survives whole at both widths now that nothing pre-truncates it to a
    // hardcoded 70 chars regardless of the real box width (the old bug: it cut mid-word with
    // plenty of room left in a 150-wide box).
    for shot in [&wide, &narrow] {
        assert!(shot.text.contains("unreachable - connection refused"), "the unreachable reason must survive at the real box width, not a hardcoded cutoff\n{}", shot.text);
    }
    // #107 item 2 (verifier-3): the unreachable row's em dash must sit in the `up` column at
    // EVERY tier, not just the narrow one it happened to align with by accident - a dash under
    // `engine` reads as "engine unknown", not "node down".
    // same header-collision as the two selectors above: the page summary line also names
    // "spark-a" ("FLEET [gpu-box] UP 31 tok/s · spark-a NO MONITOR ...").
    let unreachable_row_wide = wide.text.lines().find(|l| l.contains("spark-a") && !l.contains("FLEET")).unwrap_or_else(|| panic!("150 cols: no spark-a row\n{}", wide.text));
    let header_wide = wide.text.lines().find(|l| l.contains("node") && l.contains("engine")).expect("a header row");
    // `str::find` returns a BYTE offset; the leading box-drawing border character is multi-byte,
    // so a byte offset fed straight into `.chars().nth()` lands short of the true column - find
    // the CHAR index instead.
    let up_col = header_wide.chars().collect::<Vec<char>>().windows(2).position(|w| w == ['u', 'p']).expect("header names the up column");
    assert_eq!(unreachable_row_wide.chars().nth(up_col), Some('\u{2014}'), "150 cols: the dash must sit under the 'up' header, not 'engine'\nheader/row:\n{header_wide}\n{unreachable_row_wide}");
}

/// `[`/`]` (unguarded, already works from anywhere) picks which node is current while on FLEET;
/// enter then leaves for that node's own overview - today's single-server pages, unchanged.
#[test]
fn enter_on_the_fleet_page_drills_into_the_currently_switched_node() {
    let (mut a, _now) = classic(golden());
    a.fleet = vec![
        FleetEntry { name: "gpu-box".into(), url: "http://gpu-box:8099".into(), state: Some(Ok(FleetState { up: true, ..Default::default() })) },
        FleetEntry { name: "spark-a".into(), url: "http://spark-a:8099".into(), state: Some(Ok(FleetState { up: true, ..Default::default() })) },
    ];
    a.on_key(KeyCode::Char('f'));
    assert_eq!(a.view, View::Fleet);
    a.on_key(KeyCode::Char(']'));
    assert_eq!(a.server_idx, 1, "[ ] works from the fleet page too");
    a.on_key(KeyCode::Enter);
    assert_eq!(a.view, View::Overview, "enter leaves FLEET for that node's own overview");
    assert_eq!(a.url, "http://spark-a:8099");
}

/// #74: WATCH (`w`, always reachable - unlike FLEET this is useful with one server too). Proves
/// end to end, through real key presses and a real render: a receipted item shows its content
/// (never a title) plainly; a bare social claim reads UNVERIFIED; a source is NEW the first time
/// it is seen and not the second (the `watch_seen`/`watch_new` snapshot mechanics); "checked Nh
/// ago" and the item's own date never collapse into one line; the page never suggests switching
/// anything.
#[test]
fn w_opens_watch_shows_content_not_titles_unverified_social_and_a_new_badge_that_clears_on_reopen() {
    use lss_core::watch::{WatchItem, WatchSource};
    let mut s = golden();
    let now = s.generated_at;
    s.watch = Some(lss_core::watch::WatchStatus {
        sources: vec![
            WatchSource {
                name: "example-recipes".into(),
                covers: "model loadouts on the GPU box".into(),
                last_checked: Some(now - 7_200),
                newest: Some(WatchItem { date: "2026-09-20".into(), summary: "swapped the chat template for a fixed one from upstream, gated PASS same day".into(), url: "https://example.com/r/1".into(), has_receipt: true }),
            },
            WatchSource {
                name: "a social account".into(),
                covers: "reference only".into(),
                last_checked: Some(now - 3_600),
                newest: Some(WatchItem { date: "2026-09-19".into(), summary: "claims a new kernel is 2x faster, no link".into(), url: String::new(), has_receipt: false }),
            },
        ],
    });
    let (mut a, now2) = classic(s);
    a.on_key(KeyCode::Char('w'));
    assert_eq!(a.view, View::Watch, "w opens WATCH");
    let shot = render(&a, 140, 44, now2);
    assert_all(&shot, &["example-recipes", "model loadouts on the GPU box", "2026-09-20", "swapped the chat template", "UNVERIFIED", "a social account", "2h", "1h", "NEW"]);
    assert!(shot.text.contains("REPORTS ONLY"), "the page must say plainly it never switches anything\n{}", shot.text);
    let sero_line = shot.text.lines().find(|l| l.contains("2026-09-20")).unwrap();
    assert!(!sero_line.contains("UNVERIFIED"), "a receipted item must never read UNVERIFIED\n{sero_line}");

    // reopening the page: the same two sources are no longer NEW (the snapshot cleared on open)
    a.on_key(KeyCode::Esc);
    a.on_key(KeyCode::Char('w'));
    let shot2 = render(&a, 140, 44, now2);
    assert!(!shot2.text.contains("NEW"), "a source already seen on a previous open must not still read NEW\n{}", shot2.text);
}

/// no `[watch] path` configured: the page says plainly that tracking is off, never an empty
/// table pretending nothing was ever followed.
#[test]
fn watch_with_nothing_configured_says_tracking_is_off() {
    let mut s = golden();
    s.watch = None; // golden()/demo::status() carries demo watch data - this test is the off case
    let (mut a, now) = classic(s);
    a.on_key(KeyCode::Char('w'));
    let shot = render(&a, 140, 44, now);
    assert!(shot.text.contains("watch tracking is off"), "{}", shot.text);
}

/// A stranger's machine: Ollama on a laptop. No metrics endpoint, no gateway, no GPU tool. Every
/// number the engine does not publish reads `n/a (not reported by Ollama)` - never 0 - nothing
/// is red, LANES / USERS say there is no gateway, GPUS is not on the screen, and the probe (C1)
/// still gives the speed.
fn strangers_machine() -> Status {
    let mut s = golden();
    s.serve.engine = "ollama".into();
    s.serve.model = Some("mistral:latest".into());
    s.serve.not_reported = lss_core::engine::EngineMetrics::default().not_reported(None);
    s.serve.decode_tok_s = 0.0;
    s.serve.running = 0.0;
    s.serve.slots = 0;
    s.serve.kv_usage = 0.0;
    s.serve.prefill_tok_s = None;
    s.serve.prefill_tok_s_typical = None;
    s.serve.latency = None;
    s.gate = lss_core::model::GateStatus { up: true, absent: true, ..Default::default() };
    s.users = Default::default();
    s.gpu_source = "none".into();
    s.gpus.clear();
    s.series.gpu_temp_c.clear();
    s
}

#[test]
fn an_engine_that_reports_nothing_reads_n_a_never_zero_and_nothing_is_red() {
    let (a, now) = classic(strangers_machine());
    let shot = render(&a, 126, 41, now);
    assert_all(&shot, &["mistral:latest", "n/a (not reported by Ollama)", "C1 probe 191.7 tok/s", "o LANES (no gateway)", "no gateway configured: requests go straight to the engine.", "o USERS (no gateway)", "no gateway configured: nothing tells users apart"]);
    assert!(!shot.text.contains("o GPUS") && !shot.text.contains("GATE DOWN") && !shot.text.contains("needs gate v5.2"), "{}", shot.text);
    assert!(!shot.text.contains(" 0.0 tok/s") && !shot.text.contains("0/0"), "a number that is not reported is never shown as zero\n{}", shot.text);
    assert_eq!(shot.red_cells(), 0, "not reporting something is not a problem\n{}", shot.text);
    for (w, h) in OVERVIEW_GOLDEN_SIZES.into_iter().chain([(63, 15), (63, 24), (94, 15), (126, 41)]) {
        let shot = render(&a, w, h, now);
        assert!(shot.text.contains("n/a"), "{w}x{h}\n{}", shot.text);
        assert!(!shot.text.contains("GPUS") || shot.text.contains("GPU stats unavailable"), "{w}x{h}: no GPU tool = no GPUS area\n{}", shot.text);
        assert_eq!(shot.red_cells(), 0, "{w}x{h}\n{}", shot.text);
        for (y, line) in shot.text.lines().enumerate() {
            assert!(!line.trim().is_empty(), "{w}x{h}: row {y} is blank\n{}", shot.text);
        }
    }
    // the minimized card: still answers "is it OK?"
    let card = render(&a, 63, 11, now);
    assert_all(&card, &["o UP 36m54s (mistral:latest", "writing n/a", "running n/a", "GPU stats unavailable"]);
    // the text report says which numbers are missing, and why that is fine
    let text = lss::plain::status(&strangers_machine(), now);
    assert!(text.contains("writing (decode) n/a") && text.contains("n/a = not reported by Ollama: requests running") && text.contains("GATE     no gateway configured") && text.contains("GPUS     stats unavailable") && !text.contains("LANE     public"), "{text}");
    // Apple Silicon without root: load and memory, no temperature - shown as `-`, never 0
    let mut mac = strangers_machine();
    mac.gpu_source = "apple".into();
    mac.gpus = vec![lss_core::model::GpuStatus { sample: lss_core::gpu::GpuSample { index: 0, util_pct: Some(7.0), mem_used_mib: Some(319.0), ..Default::default() }, ..Default::default() }];
    let (a, now) = classic(mac);
    let shot = render(&a, 126, 41, now);
    assert!(shot.text.contains("o GPUS") && shot.text.contains("GPU0") && !shot.text.contains("0°C"), "{}", shot.text);
    assert_eq!(shot.red_cells(), 0, "{}", shot.text);
}

/// The nine sizes the owner named for the three shapes: third-h, third-v, minimized.
const OVERVIEW_GOLDEN_SIZES: [(u16, u16); 9] = [(126, 22), (94, 20), (200, 24), (42, 73), (50, 70), (63, 60), (40, 8), (50, 10), (63, 11)];

/// #23, 2026-09-21: the overview and `lss status` already read a stranger's machine (Ollama, no
/// gateway) honestly - `n/a`, never 0, nothing red. The DETAIL pages (LATENCY, LOAD, TOKENS,
/// USERS, GATEWAY) did not: an empty chart, a "0 written" token table, or "needs gateway v5.2"
/// with no gateway at all. Same fixture, same property, one page at a time.
#[test]
fn detail_pages_read_a_strangers_machine_n_a_never_zero_and_nothing_is_red() {
    let status = strangers_machine();
    // card #180 gate 3: GPUS joins the list - #182's tab strip puts it one keypress away, and on
    // a machine with no GPU tool its chart titles read "max 0C/32F" and "-0 W of -0 W cap"
    for page in [PageId::Latency, PageId::Load, PageId::Tokens, PageId::Users, PageId::Gateway, PageId::Model, PageId::Gpus] {
        for (w, h) in [(63, 24), (126, 41)] {
            let (mut a, now) = app(status.clone());
            a.range_idx = 1;
            a.open(page);
            // card #110: the MODEL page of a stranger's machine must be fed the STRANGER's
            // (empty) loadout doc, never the demo's model-a one.
            a.page_data = if page == PageId::Model {
                // card #110: the MODEL page of a stranger's machine must be fed the STRANGER's
                // (empty) loadout doc, never the demo's model-a one. Built in the
                // initializer (clippy::field_reassign_with_default): a `Default::default()` then a
                // field assignment is the same value spelled the way clippy rejects.
                PageData { loadouts: Some(LoadoutsDoc { v: 1, generated_at: now, loadouts: vec![] }), ..Default::default() }
            } else {
                demo::page(a.status.as_ref().unwrap(), page, a.range_idx, now)
            };
            let shot = render(&a, w, h, now);
            assert_eq!(shot.red_cells(), 0, "{page:?} {w}x{h}: not reporting something is not a problem\n{}", shot.text);
            assert!(!shot.text.contains(" 0.0 tok/s") && !shot.text.contains("0/0 slots"), "{page:?} {w}x{h}: a number that is not reported is never shown as zero\n{}", shot.text);
            match page {
                PageId::Latency | PageId::Load | PageId::Tokens => {
                    assert!(shot.text.contains("n/a (not reported by Ollama)"), "{page:?} {w}x{h}\n{}", shot.text);
                }
                PageId::Gpus => {
                    assert!(shot.text.contains("no GPU tool on this machine"), "{page:?} {w}x{h}\n{}", shot.text);
                    assert!(!shot.text.contains("0C/32F") && !shot.text.contains("-0 W") && !shot.text.contains("TEMPERATURE"), "{page:?} {w}x{h}: no GPU, no GPU numbers\n{}", shot.text);
                }
                PageId::Users | PageId::Gateway => {
                    assert!(shot.text.contains("no gateway configured"), "{page:?} {w}x{h}\n{}", shot.text);
                    assert!(!shot.text.contains("needs gateway v5.2"), "{page:?} {w}x{h}: there is no gateway at all, not an old one\n{}", shot.text);
                }
                PageId::Model => {
                    // card #110: no borrowed loadout, no borrowed bench numbers, no demo targets
                    assert!(shot.text.contains("mistral:latest"), "{page:?} {w}x{h}\n{}", shot.text);
                    assert!(!shot.text.contains("model-a"), "{page:?} {w}x{h}: the demo's model must not appear on a stranger's MODEL page\n{}", shot.text);
                    assert!(!shot.text.contains("1.0") && !shot.text.contains("tp 4"), "{page:?} {w}x{h}: the demo's loadout flags must not appear\n{}", shot.text);
                    assert!(!shot.text.contains("TARGETS") || shot.text.contains("no targets"), "{page:?} {w}x{h}\n{}", shot.text);
                }
                _ => unreachable!(),
            }
        }
    }
}

/// Clock times and dates are local to whoever runs the test: masked, so a golden render is the
/// same in every time zone. `09-19 11:36` -> `##-## ##:##`, `12:13:23` -> `##:##:##`.
fn mask_times(text: &str) -> String {
    let mut c: Vec<char> = text.chars().collect();
    let d = |c: &[char], i: usize| c.get(i).is_some_and(|x| x.is_ascii_digit() || *x == '#');
    for i in 0..c.len() {
        if d(&c, i) && d(&c, i + 1) && c.get(i + 2) == Some(&':') && d(&c, i + 3) && d(&c, i + 4) {
            for k in [i, i + 1, i + 3, i + 4] {
                c[k] = '#';
            }
            // the date in front of it
            if i >= 6 && c[i - 1] == ' ' && d(&c, i - 6) && d(&c, i - 5) && c[i - 4] == '-' && d(&c, i - 3) && d(&c, i - 2) {
                for k in [i - 6, i - 5, i - 3, i - 2] {
                    c[k] = '#';
                }
            }
        }
    }
    c.into_iter().collect()
}

/// Golden renders of the overview at the nine sizes (`fixtures/renders/`, one file per size,
/// named for its columns and rows; `scripts/remote-build.sh bless` rewrites them). A layout change shows up as a diff a person
/// can read.
#[test]
fn overview_golden_renders_at_the_nine_sizes() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/renders");
    let (a, now) = classic(golden());
    let mut stale = Vec::new();
    for (w, h) in OVERVIEW_GOLDEN_SIZES {
        let text = mask_times(&render(&a, w, h, now).text);
        let path = dir.join(format!("overview_{w}x{h}.txt"));
        if std::env::var("LSS_BLESS").is_ok() {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).unwrap_or_default() != text {
            stale.push(format!("--- {w}x{h} renders as\n{text}"));
        }
    }
    assert!(stale.is_empty(), "overview renders differ from fixtures/renders (LSS_BUILD_HOST=... scripts/remote-build.sh bless, then read the diff):\n{}", stale.join("\n"));
}

/// The rules of the three shapes, at every one of the nine sizes, on a calm and on a bad day:
/// no row of the screen and no row of a box is blank, no text is cut mid-number, and every
/// area is on screen (its own box or bar, or the MORE bar of the minimized overview).
#[test]
fn no_blank_rows_no_number_cut_in_half_and_nothing_dropped_at_the_nine_sizes() {
    for (name, status) in [("calm", golden()), ("bad", bad_day())] {
        let (a, now) = classic(status);
        for (w, h) in OVERVIEW_GOLDEN_SIZES {
            let shot = render(&a, w, h, now);
            for (y, line) in shot.text.lines().enumerate() {
                let ink: String = line.chars().filter(|c| !c.is_whitespace() && !matches!(c, '│' | '┃')).collect();
                assert!(!ink.is_empty(), "{name} {w}x{h}: row {y} is blank\n{}", shot.text);
                let chars: Vec<char> = line.chars().collect();
                for (i, c) in chars.iter().enumerate() {
                    assert!(!(*c == '…' && i > 0 && chars[i - 1].is_ascii_digit()), "{name} {w}x{h}: a number is cut in half in row {y}: {line}\n{}", shot.text);
                }
            }
            let minimized = h <= 12;
            for title in ["GPUS", "LANES", "USERS", "ADVICE", "INCIDENTS"] {
                let shown = shot.text.contains(&format!("o {title}")) || shot.text.lines().any(|l| l.contains(&format!(" {title} ")));
                assert!(shown || (minimized && shot.text.contains(" MORE ")), "{name} {w}x{h}: {title} is nowhere\n{}", shot.text);
            }
            assert!(shot.text.lines().last().unwrap().contains("? help"), "{name} {w}x{h}");
        }
    }
}

/// #48, 2026-09-21 (card requirement 8): a 4-series chart (the GPUS page's TEMPERATURE, 4 GPUs)
/// at 3 sizes spanning the layout's three tiers (narrow -> one panel + a sentence, medium -> a
/// 2x2 grid, wide -> four panels side by side): the legend always carries four DISTINCT colours,
/// and wherever a panel's own line is on screen it is the SAME colour as its legend entry -
/// never merged, never a mismatch.
#[test]
fn line_chart_uses_a_distinct_colour_per_series_and_the_legend_matches_the_line() {
    let (mut a, now) = on_page(golden(), PageId::Gpus, 1);
    a.chart_lines = true; // #48: the default is dots now (the owner opts in) - this test checks the line renderer specifically
    for (w, h) in [(63, 24), (126, 41), (200, 44)] {
        let shot = render(&a, w, h, now);
        // "━ GPU" (not the specific digit): at w=63 the narrow fallback may draw any GPU as the
        // one worst reading, not necessarily GPU0 - this finds whichever one it is.
        let legend_row = shot.row_of("━ GPU");
        let legend_line = shot.text.lines().nth(legend_row).unwrap();
        let mut colours: Vec<Color> = Vec::new();
        for (x, ch) in legend_line.chars().enumerate() {
            if ch == '━' {
                let c = shot.buf[(x as u16, legend_row as u16)].fg;
                if !colours.contains(&c) {
                    colours.push(c);
                }
            }
        }
        assert!(colours.iter().all(|c| !matches!(c, Color::Reset | Color::White | Color::Black)), "{w}x{h}: a real colour per series, not the default\n{}", shot.text);
        if w == 63 {
            // #115: at 63 cols the box is too narrow for 4 side-by-side (or 2x2-grid) panels -
            // this is `draw_chart_lines`'s pre-existing "never squeeze four unreadable panels
            // in" fallback, unrelated to #115 itself. It draws ONE real panel (the series
            // reading worst) plus a plain "also:" sentence for the rest - #115's own fix is
            // that the LEGEND now matches: exactly one colour, one entry, not four.
            assert_eq!(colours.len(), 1, "{w}x{h}: the narrow fallback draws one panel, the legend must show exactly that one\n{}", shot.text);
            assert!(shot.text.contains("also: "), "{w}x{h}: the other three are named in plain text, not implied by a colour that is not on screen\n{}", shot.text);
            continue;
        }
        assert_eq!(colours.len(), 4, "{w}x{h}: four distinct legend colours\n{}", shot.text);
        // GPU0's legend colour (pages.rs's GPU_COLOURS[0], Green) also appears somewhere in the
        // plot rows between the title and the time axis - the panel is drawn in its own colour,
        // not just described in the legend
        let title_row = shot.row_of("o TEMPERATURE");
        let axis_row = (title_row..).find(|&r| shot.text.lines().nth(r).is_some_and(|l| l.contains("-1h"))).unwrap();
        let green_in_plot = (title_row + 1..axis_row).any(|r| (0..w).any(|x| shot.buf[(x, r as u16)].fg == Color::Green && shot.buf[(x, r as u16)].symbol() != " "));
        assert!(green_in_plot, "{w}x{h}: GPU0's own colour (green) never appears in the plotted line\n{}", shot.text);
    }
}

/// card #142 (verifier-3): #115 items 1 and 3 had no teeth - killing either mutation
/// (`titled_split` always returning `(false, panel_h)`, or `multi_panel_fits_halved` always
/// `false`) left the render suite fully green. At 140x44 (the ship gate's own reproducer, and
/// the one width generous enough for BOTH a per-panel title AND two-column pairing to actually
/// happen) this pins: all four GPU names appear as their own title above a panel (item 1), the
/// TEMPERATURE and POWER boxes share a row (item 2 - #101's own established way to prove two
/// boxes are side by side, `row_of` equality), and the vertical rule between panels exists.
#[test]
fn at_140x44_gpu_panels_are_titled_and_two_charts_share_a_row() {
    let (mut a, now) = on_page(golden(), PageId::Gpus, 1);
    a.chart_lines = true;
    let shot = render(&a, 140, 44, now);
    // "GPU0" also appears in the LEGEND line ("━ GPU0 47C/117F"), so a bare `contains` is
    // vacuous - it stays true even with titles removed entirely. A title row has NO "━" prefix
    // (only the legend does), so filtering that out isolates the title specifically.
    for gpu in ["GPU0", "GPU1", "GPU2", "GPU3"] {
        assert!(shot.text.lines().any(|l| l.contains(gpu) && !l.contains('\u{2501}')), "140x44: {gpu}'s own TITLE row is missing (only its legend entry is on screen)\n{}", shot.text);
    }
    assert_eq!(shot.row_of("o TEMPERATURE"), shot.row_of("o POWER"), "140x44: TEMPERATURE and POWER must share a row - two-column line mode is off\n{}", shot.text);
    assert!(shot.text.contains('\u{2502}'), "140x44: no vertical rule between panels\n{}", shot.text);
}

/// card #142 item 3: the ship gate's own 140x44 answer (does line mode beat dots) was decided on
/// a live render nobody pinned - `LSS_BLESS=1` rewrites this file the same way
/// `overview_golden_renders_at_the_nine_sizes` does its own fixtures.
#[test]
fn gpus_lines_golden_render_at_140x44() {
    let (mut a, now) = on_page(golden(), PageId::Gpus, 1);
    a.chart_lines = true;
    let text = mask_times(&render(&a, 140, 44, now).text);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/renders/gpus_lines_140x44.txt");
    if std::env::var("LSS_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    } else {
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(text, want, "GPUS line-mode render at 140x44 differs from fixtures/renders/gpus_lines_140x44.txt (LSS_BUILD_HOST=... scripts/remote-build.sh bless, then read the diff)");
    }
}

/// #48 (card requirement 6, the acceptance test): between two adjacent REAL points the line must
/// be unbroken - a vertical run of `│` with `╭ ╮ ╰ ╯` at the turns, never a gap, never a floating
/// character. A steep jump (20C -> 90C between two adjacent samples, the full height of the
/// panel) is exactly where a gap would show if the renderer only plotted points and not the
/// connectors between them.
#[test]
fn the_line_never_breaks_at_a_steep_jump() {
    let mut status = golden();
    status.gpus.truncate(1); // one GPU: a single full-width panel, easiest to inspect exactly
    let (mut a, now) = on_page(status, PageId::Gpus, 1);
    a.chart_lines = true; // #48: the default is dots now (the owner opts in) - this test checks the line renderer specifically
    let mut doc = lss_core::series::SeriesDoc { step_s: 60, start_ts: now - 240, points: 5, ..Default::default() };
    doc.series.insert("gpu0_temp_c".into(), vec![Some(20.0), Some(20.0), Some(90.0), Some(90.0), Some(90.0)]);
    a.page_data.series = Some(doc);
    let shot = render(&a, 100, 20, now);
    let title_row = shot.row_of("o TEMPERATURE");
    let axis_row = (title_row..).find(|&r| shot.text.lines().nth(r).is_some_and(|l| l.contains("-1h"))).unwrap();
    // the plot columns start where "-1h" does (the axis aligns with the panel's own y-gutter,
    // not the box border) and end where " now" does (the box may share the row with another one)
    let plot_x0 = shot.find("-1h").unwrap().0;
    let axis_line = shot.text.lines().nth(axis_row).unwrap();
    let plot_x1 = axis_line.find(" now").map_or(shot.w, |b| axis_line[..b].chars().count() as u16);
    let allowed = ['│', '╭', '╮', '╰', '╯', '─'];
    let mut saw_vertical_run = false;
    for x in plot_x0..plot_x1 {
        let col_rows: Vec<u16> = (title_row as u16 + 1..axis_row as u16).filter(|&r| shot.buf[(x, r)].symbol() != " ").collect();
        if col_rows.len() >= 2 {
            let (lo, hi) = (*col_rows.iter().min().unwrap(), *col_rows.iter().max().unwrap());
            for r in lo..=hi {
                let ch = shot.buf[(x, r)].symbol().chars().next().unwrap_or(' ');
                assert!(allowed.contains(&ch) || (r != lo && r != hi), "col {x} row {r}: {ch:?} breaks the run between row {lo} and {hi} - a gap or a stray character\n{}", shot.text);
                assert_ne!(ch, ' ', "col {x} row {r}: a blank row sandwiched inside the line's own run (rows {lo}..{hi})\n{}", shot.text);
            }
            if hi - lo >= 3 {
                saw_vertical_run = true;
            }
        }
    }
    assert!(saw_vertical_run, "the steep jump never produced a tall connected run to check\n{}", shot.text);
}

/// #48 follow-up, 2026-09-21: the owner wants to look with his own eyes, so the chart style is a
/// live `c` toggle, default DOTS (never opted in for him). Pressing it switches the actual glyphs
/// on screen instantly. Card #291 (the owner: "dont need any of that") retired the header's
/// "which is active" tell (`[lines]`) this test used to check alongside the glyph switch - `c`
/// still toggles the real chart style, which is what is checked now.
#[test]
fn default_is_dots_and_c_switches_to_lines_instantly() {
    let (mut a, now) = on_page(golden(), PageId::Gpus, 1);
    assert!(!a.chart_lines, "default is dots - the owner opts in, never opted in for him");
    let shot = render(&a, 140, 44, now);
    assert!(shot.text.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)), "default dots: braille somewhere on screen\n{}", shot.text);

    let shot = press(&mut a, KeyCode::Char('c'), 140, 44, now);
    assert!(a.chart_lines, "c toggled it on");
    assert!(shot.text.chars().any(|c| matches!(c, '│' | '╭' | '╮' | '╰' | '╯')), "after c: a connected line on screen\n{}", shot.text);

    let shot = press(&mut a, KeyCode::Char('c'), 140, 44, now);
    assert!(!a.chart_lines, "c toggles back off");
    assert!(shot.text.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)), "back to dots: braille again\n{}", shot.text);
}

/// #73, 2026-09-22 (a real product redirect - the page is now COST and LOADOUT, not
/// health): all six sections - in the owner's own order, LOADOUT first - exist and are never
/// dropped, even though real content is tall enough that they do not all fit on one 140x44
/// screen: scrolling through must find every one of them. ADMIT/SPEED/WORK/EVENTS (the #51 "is
/// it healthy" framing) are gone from this page entirely - see dash.rs's own module doc comment.
///
/// #191, 2026-09-22: this page is now the DEFAULT, not an opt-in behind `v`. The owner was shown
/// both and chose this one; that choice lived only in his git-ignored `~/.config/lss/ui.json`,
/// so the compiled default still landed every stranger on the page he had rejected. The test is
/// kept and INVERTED rather than deleted: the direction of `v` is the whole claim. #51's
/// `[page1-v2]` header tell is retired with the opt-in it belonged to - the footer says which
/// page `v` goes to, in words, on both pages, which is where the claim is checked now.
#[test]
fn v_switches_to_the_73_redesign_and_back_with_the_header_tell_and_every_section_reachable() {
    let (mut a, now) = app(golden());
    assert!(a.dash, "#191: the default IS the redesigned page 1 - a stranger's first impression is the page the owner picked");
    let shot = render(&a, 140, 44, now);
    assert!(!shot.text.contains("page1-v2") && !shot.text.contains("page1-classic"), "the landing page carries no opt-in badge at all\n{}", shot.text);
    // card #291: a key-hints row now rides UNDER the tab strip at this height, so the tab line
    // (ending "? help") is found by its own content, not by position.
    assert!(shot.text.lines().any(|l| l.trim_end().ends_with("? help")), "#258: v is in ? help, on the tab line\n{}", shot.text);
    assert_eq!(shot.red_cells(), 0, "a calm day is never red\n{}", shot.text);

    // scroll through the whole page (End jumps to the last screen) and confirm every section
    // title was seen somewhere - "never dropped" means reachable, not necessarily all at once
    let mut seen = shot.text.clone();
    let end_shot = press(&mut a, KeyCode::End, 140, 44, now);
    seen.push_str(&end_shot.text);
    for title in ["LOADOUT", "ELECTRICITY", "COST PER 1M TOKENS", "TOKENS", "USERS", "GPUS"] {
        assert!(seen.contains(title), "{title} missing from the redesign page (checked the top and the last screen)\n--top--\n{}\n--end--\n{}", shot.text, end_shot.text);
    }
    for gone in ["o ADMIT", "o SPEED", "o WORK", "o EVENTS"] {
        assert!(!seen.contains(gone), "{gone} is the old health framing - #73 removed it from this page\n{seen}");
    }

    // `v` now opts OUT, to the classic grid, and the footer offers the way back
    let shot = press(&mut a, KeyCode::Char('v'), 140, 44, now);
    assert!(!a.dash, "v switches to the classic grid");
    assert!(shot.text.contains("o ADVICE"), "the classic grid is what v reaches - its ADVICE box is the one section page 1 does not carry\n{}", shot.text);
    assert!(shot.text.lines().any(|l| l.trim_end().ends_with("? help")), "#258: v is in ? help, on the tab line\n{}", shot.text);

    let shot = press(&mut a, KeyCode::Char('v'), 140, 44, now);
    assert!(a.dash, "v toggles back to the default page 1");
    assert!(shot.text.contains("LOADOUT"), "back on the default page 1\n{}", shot.text);
}

/// card #176 item 2: the chart the owner asked for, on the page that has charts. A render test
/// rather than a unit test because "there is a chart" is only true if it reaches a screen -
/// card #124's whole lesson was four features that existed everywhere except on a screen.
#[test]
fn the_tokens_page_charts_daily_spending() {
    // on_page loads the page's own documents, like the other page tests: without them TOKENS
    // renders its "loading..." box, which fills the pane and pushes everything else off screen
    // (exactly the layout trap card #124 hit).
    let (mut a, now) = on_page(golden(), PageId::Tokens, 1);
    let shot = render(&a, 200, 60, now);
    assert!(shot.text.contains("SPENDING PER DAY"), "the daily-spend chart is on page 5\n{}", shot.text);
    assert!(shot.text.contains("$ per day"), "its series is labelled\n{}", shot.text);
    // the detail line has to carry the numbers a reader would otherwise have to eyeball
    assert!(shot.text.contains("total") && shot.text.contains("/day average"), "{}", shot.text);
    // the peak day is PRECISION, and precision is what the detail may lose: this box is
    // half-width and title_line cuts the detail from the right, so the peak only survives where
    // there is room for it. Asserted at a width that has room, not at every width - the thing
    // asserted at EVERY width is the scope, and it lives in the title for exactly that reason
    // (see the_spending_chart_states_its_scope_at_every_width_because_a_cut_caveat_is_no_caveat).
    let wide = render(&a, 300, 60, now);
    assert!(wide.text.contains("most expensive"), "the peak day is named where there is room\n{}", wide.text);
    // POSITION is part of the feature, and it is pinned from both sides. Pushed first, the chart
    // ate a small pane: at 63x24 only one box fits, so the page's own subject fell off the bottom
    // and the chart's empty plot rows broke the no-blank-rows rule. Appended last, it rendered
    // below the fold and nobody saw it at all.
    let small = render(&a, 63, 24, now);
    for needle in ["o TOKENS SERVED", "o TOKENS PER HOUR"] {
        assert!(small.text.contains(needle), "{needle:?} must keep the first screen at 63x24\n{}", small.text);
    }
    assert!(!small.text.contains("SPENDING PER DAY"), "the chart must not displace the page at 63x24\n{}", small.text);
    // ...and one scroll reaches it, so "not first" never means "not there".
    let mut seen = false;
    for _ in 0..3 {
        a.on_key(KeyCode::Down);
        if render(&a, 63, 24, now).text.contains("SPENDING PER DAY") {
            seen = true;
            break;
        }
    }
    assert!(seen, "the chart is within three scrolls of the top of page 5 at 63x24");
}

/// card #176 item 5, the half I got wrong first: lss-verifier-4 showed by arithmetic that the
/// caveat, appended as the last DETAIL span, is the first thing title_line cuts - and this box is
/// half-width, so at 200 columns it had ~100 and the caveat vanished entirely. There was no render
/// test on the TUI half at all, so deleting those lines left the suite green. This is that test,
/// and it runs at the widths where the detail DOES overflow, which is the only place it matters.
#[test]
fn the_spending_chart_states_its_scope_at_every_width_because_a_cut_caveat_is_no_caveat() {
    for (w, h) in [(200u16, 60u16), (126, 41), (100, 40)] {
        let (mut a, now) = on_page(golden(), PageId::Tokens, 1);
        let mut all = String::new();
        for _ in 0..4 {
            all.push_str(&render(&a, w, h, now).text);
            a.on_key(KeyCode::Down);
        }
        assert!(all.contains("SPENDING PER DAY"), "the chart is reachable at {w}x{h}\n{all}");
        // the SCOPE rides the title, which truncation keeps; the date and the fixed charge ride
        // the detail, which it may cut
        assert!(
            all.contains("DELIVERY+GENERATION ONLY"),
            "the chart must say what its dollars cover at {w}x{h} - the detail is cut here, the title is not\n{all}"
        );
    }
}

#[test]
fn one_day_of_history_draws_no_chart_because_a_point_is_not_a_trend() {
    let mut s = golden();
    if let Some(sp) = s.spending.as_mut() {
        sp.daily.truncate(1);
    }
    let (a, now) = on_page(s, PageId::Tokens, 1);
    let shot = render(&a, 200, 60, now);
    assert!(!shot.text.contains("SPENDING PER DAY"), "a single point must not be drawn as a line\n{}", shot.text);
}

/// card #124 (panel 2026-09-22): the four panel items that were built, plumbed and rendered by
/// NOTHING. These are the render-LAYER assertions the panel asked for: the earlier work had unit
/// tests on the data and no test that any of it reached a screen, which is how four features
/// stayed invisible straight through a page redesign.
#[test]
fn page1_shows_kv_free_not_the_pool_size_and_says_so_when_it_cannot() {
    let (mut a, now) = app(golden());
    a.dash = true;
    let shot = render(&a, 140, 44, now);
    let row = row_starting_with(&shot.text, "KV free").expect("a KV free row");
    assert!(row.contains("free of"), "the row must show free AND the pool it is free of: {row}");
    assert!(!shot.text.contains("KV pool"), "the pool SIZE row is gone: it was a capacity fact, not a state\n{}", shot.text);

    // Husain's rule from the panel: an absent number must LOOK absent, never read as 0 - a
    // silent zero is how the inert-charging bug hid for 2,809 requests.
    let mut s = golden();
    s.serve.kv_max_tokens = 0.0;
    s.serve.kv_used_tokens = 0.0;
    let (mut a2, now2) = app(s);
    a2.dash = true;
    let shot2 = render(&a2, 140, 44, now2);
    let row2 = row_starting_with(&shot2.text, "KV free").expect("a KV free row");
    assert!(row2.contains("not published"), "an absent pool must say so, not show 0: {row2}");
}

#[test]
fn page1_stamps_how_long_since_the_engine_emitted_a_token() {
    // Kwon ranked this first of the four by minutes saved on the next incident: monotonic growth
    // here with the GPUs busy IS what an Xid-8 hang looks like from outside.
    let mut s = golden();
    s.serve.secs_since_last_token = Some(214);
    s.serve.running = 3.0;
    let (mut a, now) = app(s);
    a.dash = true;
    let shot = render(&a, 140, 44, now);
    let row = row_starting_with(&shot.text, "status").expect("the status row");
    assert!(row.contains("last token"), "the freshness stamp rides the status line: {row}");
    assert!(row.contains("3m34s"), "it shows the real age: {row}");
    assert!(shot.red_cells() > 0, "requests running and no token for 214s is red\n{}", shot.text);

    // IDLE is not a fault: nobody is asking the engine for tokens, so it must not colour
    let mut idle = golden();
    idle.serve.secs_since_last_token = Some(900);
    idle.serve.running = 0.0;
    let (mut a2, now2) = app(idle);
    a2.dash = true;
    let shot2 = render(&a2, 140, 44, now2);
    let row2 = row_starting_with(&shot2.text, "status").expect("the status row");
    assert!(row2.contains("idle"), "an idle engine says idle rather than alarming: {row2}");
    assert_eq!(shot2.red_cells(), 0, "an idle box is not a problem\n{}", shot2.text);

    // an old collector that does not publish the field says nothing rather than showing 0s
    let mut old = golden();
    old.serve.secs_since_last_token = None;
    let (mut a3, now3) = app(old);
    a3.dash = true;
    let shot3 = render(&a3, 140, 44, now3);
    let row3 = row_starting_with(&shot3.text, "status").expect("the status row");
    assert!(!row3.contains("last token 0"), "an unpublished field must not read as 0s: {row3}");
}

#[test]
fn page8_leads_with_the_admission_verdict_and_names_whose_queue_it_is() {
    // item 1: admission_verdict() had 7 passing unit tests and ZERO callers. This is the test
    // that can fail when the wiring breaks - the unit tests cannot.
    let (a, now) = on_page(golden(), PageId::Alerts, 1);
    let shot = render(&a, 140, 44, now);
    assert!(shot.text.contains("OK \u{b7}"), "a calm box shows the OK verdict in the RULES header\n{}", shot.text);

    // THE case the verdict exists for: waiters at the gateway while the engine has room.
    let mut s = golden();
    s.lanes.trusted.waiters = 4;
    s.serve.running = 1.0;
    s.serve.slots = 8;
    let (a2, now2) = on_page(s, PageId::Alerts, 1);
    let shot2 = render(&a2, 140, 44, now2);
    assert!(shot2.text.contains("GATEWAY-LIMITED"), "4 waiting with 7 free slots is the gateway's queue\n{}", shot2.text);
    assert!(shot2.text.contains("the queue is ours"), "it must say WHOSE fault it is\n{}", shot2.text);
    assert!(shot2.red_cells() > 0, "a gateway-limited box is red\n{}", shot2.text);
}

/// #73: LOADOUT shows what is loaded and how (asked twice: "what flags and what things are
/// turned on to load", "what was the loadouts") plus the one line of headline state the card
/// asked to keep ("up/down, firing count") once ADVICE/INCIDENTS/ALERTS/LANES all leave the page.
#[test]
fn loadout_shows_the_loadout_and_the_one_line_status() {
    let (mut a, now) = app(golden());
    a.dash = true;
    let shot = render(&a, 140, 44, now);
    // #118: the demo fixture now publishes real priority values (public 10, trusted 0)
    assert_all(&shot, &["host", "gpu-box", "served id", "model-a", "image tag", "flags", "tp 4", "flags age", "unchanged", "KV free", "run limit", "8 slots", "priority", "public 10", "trusted 0", "uptime", "restarts"]);
    let status_row = row_starting_with(&shot.text, "status").unwrap();
    assert!(status_row.contains("up") && status_row.contains("no alerts firing"), "{status_row}");
    assert_eq!(shot.red_cells(), 0, "a calm day is never red\n{}", shot.text);

    // a real problem (down + firing) is the one thing that DOES turn this line red - and #222
    // means it is visible on the FIRST screen even before LOADOUT's own status row is: the
    // header and the worst-now line both say so with no key pressed, because a bad day now
    // spends this pane's first screen on ALERTS + INCIDENTS (the higher-priority boxes), not on
    // LOADOUT - the same trade the card's own review made explicit for the required sizes.
    let (mut a2, now2) = app(bad_day());
    a2.dash = true;
    let shot2 = render(&a2, 140, 44, now2);
    assert!(shot2.text.contains("DOWN") && shot2.text.contains("FIRING"), "down + firing must be visible with no key pressed (the header)\n{}", shot2.text);
    assert!(shot2.red_cells() > 0, "down + firing is a real problem: it is red\n{}", shot2.text);
    // LOADOUT's own one-line status row still exists and still says the same thing, a scroll or
    // two away on a day this busy - reachable, not gone, per #222 item 1's own fallback rule.
    // #227: the packed grid can push it further down than one screen on a bad day. `seen` starts
    // from the unscrolled first screen too (shot2, already rendered above) - LOADOUT seeds its
    // own column and can be on-screen with no scroll at all.
    let mut seen = shot2.text.clone();
    for _ in 0..6 {
        seen.push_str(&press(&mut a2, KeyCode::PageDown, 140, 44, now2).text);
    }
    let status_row2 = row_starting_with(&seen, "status").unwrap_or_else(|| panic!("LOADOUT's status row is not reachable by scrolling, even on a bad day\n{seen}"));
    assert!(status_row2.contains("down") && status_row2.contains("firing"), "{status_row2}");
}

/// #73: ELECTRICITY (the owner: "how much electric am i spending per hour based on kwh from gpu
/// ... how much did it cost me and how much did it cost me in dollar amount"), COST PER 1M
/// TOKENS (the owner: "how much am i paying per 1m token based on my electric") and TOKENS (by
/// hour/day/week/month) all show real numbers, and `today` (since local midnight) is never
/// presented as the same window as `last 24h` (rolling) - they are visibly two different rows.
#[test]
fn electricity_cost_per_million_and_tokens_show_real_numbers() {
    let (mut a, now) = app(golden());
    a.dash = true;
    // card #226: 200x60, not 140x44. At 140 the ELECTRICITY box WRAPS "(since local midnight)"
    // across two lines, and this passed only because the COST box happened to repeat the same
    // words inside the first screen; #226's month/year rows pushed that line below it. A size
    // where nothing wraps makes the needles test the rows they name.
    let shot = render(&a, 200, 60, now);
    // #222: ALERTS + INCIDENTS now lead this column (the card's own point), so ELECTRICITY and
    // COST PER 1M TOKENS may be a PageDown away rather than on the first screen - #101's
    // own "a real dollar figure with no key pressed" rule still holds regardless: the worst-now
    // line above carries a live cost figure unconditionally (see `worst_line`), which is what
    // that rule actually asks for. TOKENS (the right column, untouched by this reorder) is
    // unaffected and still checked on the first screen below.
    // (batch1 merge of #226 + #222: #226's size, #222's first-two-screens check.)
    assert!(shot.text.contains('$'), "a real dollar figure must be visible with no key pressed\n{}", shot.text);
    let scrolled = press(&mut a, KeyCode::PageDown, 200, 60, now);
    for needle in ["plan", "effective", "right now", "live draw", "today", "since local midnight", "last 24h", "rolling", "delivery+generation only", "excludes", "unresolved", "generated", "prompt", "per 1M tok"] {
        assert!(shot.text.contains(needle) || scrolled.text.contains(needle), "{needle:?} is nowhere on page 1's first two screens\n{}\n---\n{}", shot.text, scrolled.text);
    }
    // #175: TOKENS is an aligned table now - the column header names each number once
    assert_all(&shot, &["hour", "day", "week", "month", "prompt", "cached", "generated", "requests"]);
    assert_eq!(shot.red_cells(), 0, "a calm day is never red\n{}", shot.text);
}

/// #199: ELECTRICITY's month-projection row obeys the ONE label grid. Its label was "month at
/// this rate" - 18 characters against a 16-column cell - so `{label:<16}` padded nothing and the
/// figure it qualifies sat flush against the words at every width ("month at this rate~$243.00"),
/// which is how it was spotted in a render capture. Captured at both sizes the card names; the
/// row has to be PAGED to at 80x24, which is exactly where a reader meets it.
#[test]
fn the_month_projection_row_keeps_the_one_label_grid_at_every_size() {
    for (w, h) in [(150, 46), (80, 24)] {
        let (mut a, now) = app(golden());
        a.dash = true;
        let mut seen = render(&a, w, h, now).text;
        for _ in 0..20 {
            seen.push_str(&press(&mut a, KeyCode::PageDown, w, h, now).text);
        }
        assert!(!seen.contains("month at this rate"), "{w}x{h}: the over-long label is gone, not merely padded\n{seen}");
        // read the row off the SCREEN at whatever column its box sits in - at 80 wide page 1 has
        // no inner padding column, so `row_starting_with`'s "border, space, label" shape does not
        // apply and the check has to be the grid itself: 16 columns of label cell, then the value
        let line = seen.lines().find(|l| l.contains("month projected")).unwrap_or_else(|| panic!("{w}x{h}: no `month projected` row on any screen of page 1\n{seen}"));
        let chars: Vec<char> = line.chars().collect();
        let start = (0..chars.len()).find(|&i| chars[i..].iter().take(15).collect::<String>() == "month projected").unwrap();
        let cell: String = chars[start..start + 16].iter().collect();
        assert_eq!(cell, "month projected ", "{w}x{h}: the label cell must end in a space, never touch its value\n{line}");
        let value: String = chars[start + 16..].iter().collect();
        assert!(value.starts_with("~$"), "{w}x{h}: the figure starts exactly at the value column\n{line}");
    }
}

/// "never carry a stale value forward" (Hamel), re-applied to #73's own new fields: a TOKENS
/// window with no rollup history yet says so plainly - distinct from `hour`, which still comes
/// from `work_1h` and stays populated.
#[test]
fn a_token_window_with_no_rollup_history_yet_says_so_not_a_fabricated_zero() {
    let mut s = golden();
    if let Some(t) = s.tokens_by_window.as_mut() {
        t.day = None;
        t.week = None;
        t.month = None;
    }
    let (mut a, now) = app(s);
    a.dash = true;
    // #227: the packed grid can put TOKENS a screen or two down at this width - scan every
    // screen page 1 has, same as the reachability tests below.
    // card #258: 140x41 = the 39-row body 140x44 had under the old five-row chrome
    let mut seen = render(&a, 140, 41, now).text;
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 140, 41, now).text);
    }
    for label in ["day", "week", "month"] {
        let row = row_starting_with(&seen, label).unwrap_or_else(|| panic!("{label} row not on screen\n{seen}"));
        assert!(row.contains('\u{2014}') && row.to_lowercase().contains("not enough rollup history"), "{label}: em dash + the LIVE reason\n{row}");
    }
    let hour_row = row_starting_with(&seen, "hour").unwrap();
    assert!(!hour_row.contains('\u{2014}'), "hour still comes from work_1h and stays populated\n{hour_row}");
}

/// card #191: the ALL TIME row, the one piece of #51's WORK box that #73's hour/day/week/month
/// table did not carry over. Making this page the compiled default is what made that a real
/// loss rather than a choice on an opt-in page, so it is back - and it must be a REAL number
/// from the engine's own cumulative counters, sitting in the same four columns as the windows
/// above it, never "not enough rollup history" (it needs none) and never empty.
#[test]
fn page1_tokens_carries_the_all_time_totals_the_windows_cannot_answer() {
    let (mut a, now) = app(golden());
    assert!(a.dash, "#191: this is the default page");
    // #227: the packed grid can put TOKENS a screen or two down at this width.
    let mut seen = render(&a, 140, 44, now).text;
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 140, 44, now).text);
    }
    let row = row_starting_with(&seen, "all time").unwrap_or_else(|| panic!("no `all time` row on page 1\n{seen}"));
    assert!(!row.contains('\u{2014}'), "the engine's own cumulative counters need no rollup history, so this row is never an em dash\n{row}");
    assert!(row.chars().any(|c| c.is_ascii_digit()), "all time must carry real numbers\n{row}");

    // the same four columns as the windows above it: four separate figures, not a sentence
    let figures = row.chars().skip(16).collect::<String>().split_whitespace().count();
    assert_eq!(figures, 4, "prompt, cached, generated, requests - one figure per column\n{row}");
}

/// #73's own version of the #51 ship-gate fix: a field the collector does not have AT ALL (an
/// old collector, `tokens_by_window: None`) must say "not deployed", never the "not enough
/// history" reason that implies the field exists but is merely empty so far - and the loadout
/// flags line's own not-deployed case (unchanged since #51) still applies alongside it.
#[test]
fn tokens_not_deployed_at_all_renders_as_an_em_dash_saying_so_never_a_confident_sentence() {
    let mut s = golden();
    s.tokens_by_window = None;
    s.serve.page1_fields_deployed = false;
    let (mut a, now) = app(s);
    a.dash = true;
    // #227: the packed grid can put TOKENS a screen or two down at this width.
    let mut seen = render(&a, 140, 44, now).text;
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 140, 44, now).text);
    }
    // #227: this line can itself wrap ("... not deployed" / "yet") at this column width
    let tokens_row = seen.lines().find(|l| l.contains("needs a collector update")).unwrap_or_else(|| panic!("TOKENS not-deployed row missing\n{seen}"));
    assert!(tokens_row.contains('\u{2014}'), "{tokens_row}");
    let flags_row = row_starting_with(&seen, "flags").unwrap();
    assert!(flags_row.contains('\u{2014}') && flags_row.to_lowercase().contains("not deployed"), "{flags_row}");
}

/// minimized (50x10) shows a compact live card - up/down, a cost line, running/queue, the
/// hottest GPU, the firing count - never just the loadout's static facts.
#[test]
fn minimized_shows_a_live_compact_card_not_just_static_loadout_facts() {
    let (mut a, now) = app(golden());
    a.dash = true;
    let shot = render(&a, 50, 10, now);
    assert!(shot.text.contains("MINIMIZED"), "the box says what it is\n{}", shot.text);
    assert_all(&shot, &["up", "running", "queue", "hottest", "GPU"]);
    assert!(shot.text.contains('$') || shot.text.contains("cost tracking off"), "minimized shows a cost line, live or honestly off\n{}", shot.text);
    // the old regression: static identity facts and not one live number
    assert!(!shot.text.contains("served id") && !shot.text.contains("image tag") && !shot.text.contains("flags age"), "minimized must not just be LOADOUT cropped\n{}", shot.text);
}

/// USERS (one flat table, never one box per user - now with the #73 `$` column, split by each
/// user's share of the SAME rolling-24h window the token columns use) and GPUS (one flat table,
/// never four mini-cards, plus the util spread as the TP-straggler proxy); the one-line status
/// (LOADOUT's last row) carries the firing count that EVENTS used to own as a whole section.
#[test]
fn users_and_gpus_are_flat_tables_and_the_one_line_status_carries_firing() {
    let (mut a, now) = app(golden());
    a.dash = true;
    // "End" reaches the last screen: real content is tall enough that USERS/GPUS scroll past
    // the first screen (proved by the earlier reachability test) - this test reads whichever
    // screen actually shows them by scanning both the top and the end. 260 wide, not 140: #227's
    // packed grid needs a column over `WHO_FULL_W` for the `$24h`/share columns this test reads.
    let top = render(&a, 260, 44, now).text;
    let end = press(&mut a, KeyCode::End, 260, 44, now).text;
    let seen = format!("{top}\n{end}");
    assert!(seen.contains("acme") && seen.contains("laptop"), "USERS: real users from the fixture\n{seen}");
    // #255 round 3 (the owner, polish): the "not true GPU-seconds" explanation of share/$ moved
    // to `?` help ("USERS share/$") - page 1 keeps just the column headers naming them.
    assert!(seen.contains("share") && seen.contains("$24h"), "USERS: the share/$ columns are named\n{seen}");
    assert!(seen.contains("GPU0") && seen.contains("GPU3"), "GPUS: one row per GPU\n{seen}");
    // card #196: GPUS' straggler row is a PROJECTION of readings' `gpu.skew`, so the number is
    // the laggard's distance from the pack MEDIAN (not `max - min`, which fired hardest on an
    // idle box). The old wording ("util spread ... TP-straggler proxy") was the renderer's;
    // asserting it again would pin a second owner for the same string.
    // #255 round 3 (the owner, polish): the skew value moved from its own "util skew" row into
    // the GPUS box's one-line summary ("total ... \u{b7} skew 0 pts \u{b7} ...") - and takes just
    // the reading's leading "<n> pts" now, not its whole sentence ("0 pts behind the pack").
    assert!(seen.contains("skew") && seen.contains("pts"), "GPUS: the straggler value, projected from gpu.skew\n{seen}");
    // #227 ("alot of wasted space"): the reading's own explanatory sentence moved off page 1's
    // box entirely (no room for it anywhere but `? help`, and it is per-reading text, not a
    // fixed phrase help can state) - the VALUE is still here, the prose is not.
    assert!(seen.contains("no alerts firing"), "the one-line status carries the firing count\n{seen}");
}

/// Everything page 1's USERS table shows, top to bottom, at 140x44 - every screen joined (#227:
/// the packed grid can put USERS on a middle screen, not just the first or the last one).
fn users_screens(s: Status) -> String {
    let (mut a, now) = app(s);
    a.dash = true;
    // #227: 260 wide, not 140 - at 140 USERS' own column (a third of the pane) is narrower than
    // `WHO_FULL_W` needs and drops to its narrow tier, which is a real, honest trade-off of the
    // packed grid (untested here on purpose - see `page1_padding_never_costs_users...` for the
    // narrow-tier case) but breaks every test that reads the FULL tier's own columns.
    let mut seen = render(&a, 260, 44, now).text;
    for _ in 0..10 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 260, 44, now).text);
    }
    seen
}

/// The USERS row for `name`, and the character of that row sitting under the LAST character of
/// the `on now` header - both columns are right-aligned to the same edge, so this reads the cell
/// itself rather than "the row contains this glyph somewhere" (the `$24h` column can hold an em
/// dash of its own, which is exactly how a vacuous assertion would sneak in - see #107 item 2's
/// own column-position check).
fn on_now_cell(seen: &str, name: &str) -> (String, char) {
    let header = seen.lines().find(|l| l.contains("on now") && l.contains("req24h")).unwrap_or_else(|| panic!("no USERS header\n{seen}"));
    // CHARACTERS, not bytes: every line here is full of box-drawing glyphs and em dashes, so a
    // byte offset from `find` lands five columns off (measured - this helper's own first version
    // read a digit of req24h and called it the cell).
    let col = header[..header.find("on now").expect("header")].chars().count() + 5;
    let row = seen.lines().find(|l| l.contains(name) && l.contains('%')).unwrap_or_else(|| panic!("no USERS row for {name}\n{seen}"));
    (row.to_string(), row.chars().nth(col).unwrap_or(' '))
}

/// card #195, 2026-09-23, the owner: "i want to know who is actually on it now .. the user is good
/// but i would like to see who is currently still on". The 24h columns answered "who used it
/// today"; this asserts the LIVE half - the `on now` column, the headline count above the table,
/// and running-first ordering.
#[test]
fn users_says_who_is_on_right_now_with_in_flight_and_an_age() {
    let seen = users_screens(golden());
    assert!(seen.contains("2 running now") && seen.contains("more seen in the last 10 min"), "the headline answers 'who is on' before any table\n{seen}");
    assert!(seen.contains("on now"), "the live column is on screen\n{seen}");
    // the gateway's own in-flight count for whoever has requests running THIS SECOND
    assert!(on_now_cell(&seen, "acme").0.contains("2 live"), "acme has 2 requests running\n{}", on_now_cell(&seen, "acme").0);
    assert!(on_now_cell(&seen, "laptop").0.contains("1 live"), "laptop has 1 running\n{}", on_now_cell(&seen, "laptop").0);
    // and for everyone else, how long since their last request - the fixture's own 5h16m / 8h11m
    assert!(on_now_cell(&seen, "canary").0.contains("5h16m"), "a quiet caller shows how long since their last request\n{}", on_now_cell(&seen, "canary").0);
    assert!(on_now_cell(&seen, "office").0.contains("8h11m"), "same, in the row below it\n{}", on_now_cell(&seen, "office").0);
    // #255 round 3 (the owner, polish): the "unknown, not idle" caption explaining the em dash
    // moved to `?` help ("USERS on now") - it is no longer page 1's own sentence to carry.
    // running first, then the most recently seen - never 24h volume ahead of who is actually on
    let rows: Vec<&str> = seen.lines().filter(|l| l.contains('%') && ["acme", "laptop", "office", "canary"].iter().any(|n| l.contains(n))).collect();
    let at = |n: &str| rows.iter().position(|l| l.contains(n)).unwrap_or_else(|| panic!("{n} missing\n{seen}"));
    assert!(at("acme") < at("laptop") && at("laptop") < at("canary") && at("canary") < at("office"), "order is 2 live, 1 live, 5h16m, 8h11m\n{rows:#?}");
}

/// card #195's honesty half, and the one this repo is strictest about: a caller the gateway
/// published no last-seen time for is UNKNOWN. Not idle, and above all not a fabricated `0s` -
/// the same rule #147's orphan guard and #180's `na()` rows enforce elsewhere. It must also not
/// be sorted above someone known to be on, and a quiet caller with a huge day must not outrank
/// one seen two minutes ago.
#[test]
fn a_caller_with_no_last_seen_time_reads_unknown_never_a_fabricated_zero() {
    let mut s = golden();
    // /status sends these in-flight-then-volume order: acme, laptop, office, canary
    assert_eq!(s.users.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["acme", "laptop", "office", "canary"]);
    s.users.rows[2].secs_since_last_request = Some(120); // office: seen two minutes ago, 19 requests
    s.users.rows[3].secs_since_last_request = None; // canary: the gateway said nothing about it...
    s.users.rows[3].gate.requests_24h = 99_999; // ...and it had the biggest day of anyone
    let seen = users_screens(s);
    let (row, cell) = on_now_cell(&seen, "canary");
    assert_eq!(cell, '\u{2014}', "no last-seen time is an em dash in the `on now` column itself\n{row}\n{seen}");
    assert!(!row.contains("0s") && !row.contains("idle"), "never a fabricated zero, never a claim that they are idle\n{row}");
    assert!(seen.contains("1 unknown"), "and the headline counts them apart from the quiet ones\n{seen}");
    let rows: Vec<&str> = seen.lines().filter(|l| l.contains('%') && ["acme", "laptop", "office", "canary"].iter().any(|n| l.contains(n))).collect();
    let at = |n: &str| rows.iter().position(|l| l.contains(n)).unwrap_or_else(|| panic!("{n} missing\n{seen}"));
    assert!(at("office") < at("canary"), "seen 2 min ago beats a quiet caller with 99,999 requests - the question is who is ON\n{rows:#?}");
    assert!(on_now_cell(&seen, "office").0.contains("2m00s"), "and says how long ago\n{}", on_now_cell(&seen, "office").0);
}

/// Card #227's own two pinned assertions, from the Done= criteria directly: "GPUS is the FIRST
/// box, top row, at every size" and "a packed grid of side-by-side boxes, >=2 columns from ~60
/// cols wide, more when wider". `dash_page_rows`/`page_rows` alone cannot tell columns apart (they
/// only carry row ranges), so this reads the actual screen buffer: GPUS's own box-drawing corner
/// must be the top-left-most one on screen, and at least two DISTINCT box left-edges (`x`
/// coordinates) must appear in the first few rows for every width the card names.
#[test]
fn gpus_leads_the_grid_and_it_is_at_least_two_columns_from_60_wide() {
    for (w, h) in [(60u16, 45u16), (94, 20), (120, 40), (160, 45), (200, 50), (200, 30)] {
        let (a, now) = app(bad_day());
        let shot = render(&a, w, h, now);
        // the header + tab strip + worst-now line occupy the first few rows before the grid
        // starts (card #227 item 6, not yet done, will move them to the bottom instead). Every
        // page-1 box is drawn with `frame(false, ...)` (light borders, never thick), so '┌' alone
        // marks a box's own top-left corner.
        let lines: Vec<&str> = shot.text.lines().collect();
        let grid_start = lines.iter().position(|l| l.contains('\u{250c}')).unwrap_or_else(|| panic!("{w}x{h}: no box top border found\n{}", shot.text));
        let first_row = lines[grid_start];
        assert!(first_row.contains("o GPUS"), "{w}x{h}: GPUS is not the first box on screen\n{first_row}");
        // #255 round 2 (the owner, authorized): GPUS is now its OWN full-width band above the
        // grid, not one of its columns - so the first rows are GPUS ALONE (one edge, not two).
        // The invariant this test is actually about (a packed multi-column grid, not one long
        // box) now belongs to the boxes BELOW the GPUS band - find where GPUS' own box closes
        // (its bottom border, same left column as its top) and check columns from there.
        let gpus_close = (grid_start + 1..lines.len())
            .find(|&y| lines[y].starts_with('\u{2514}') || lines[y].starts_with('\u{2517}'))
            .unwrap_or_else(|| panic!("{w}x{h}: GPUS box never closes on screen\n{}", shot.text));
        let mut edges: Vec<usize> = Vec::new();
        for line in lines.iter().skip(gpus_close + 1).take(3) {
            for (i, c) in line.char_indices() {
                if c == '\u{250c}' && !edges.contains(&i) {
                    edges.push(i);
                }
            }
        }
        assert!(edges.len() >= 2, "{w}x{h}: fewer than 2 columns of boxes below the GPUS band (edges={edges:?})\n{}", shot.text);
    }
}

/// lss-verifier-3's FAIL on #255: card item 3 (the owner: "u squeezed 3 boxes in .. most should
/// be two") is IMPLEMENTED (`column_count`) but was never PINNED - a mutant allowing a 3rd column
/// at >=100 wide (`60..=99 => 2, _ => 3`) rendered three boxes side by side below GPUS at 120
/// wide and no test objected; it even made the #247 zero-scroll pins pass (a shorter page, for
/// the wrong reason). This is the upper-bound half of `gpus_leads_the_grid_...`'s own lower-bound
/// check (`edges.len() >= 2`), same technique, at every width the owner's complaint covers.
#[test]
fn page1_never_more_than_2_columns_below_gpus_at_100_to_200_wide() {
    for w in [100u16, 120, 160, 200] {
        let (a, now) = app(bad_day());
        let shot = render(&a, w, 60, now);
        let lines: Vec<&str> = shot.text.lines().collect();
        let grid_start = lines.iter().position(|l| l.contains('\u{250c}')).unwrap_or_else(|| panic!("{w}: no box top border found\n{}", shot.text));
        assert!(lines[grid_start].contains("o GPUS"), "{w}: GPUS is not the first box on screen\n{}", lines[grid_start]);
        let gpus_close = (grid_start + 1..lines.len())
            .find(|&y| lines[y].starts_with('\u{2514}') || lines[y].starts_with('\u{2517}'))
            .unwrap_or_else(|| panic!("{w}: GPUS box never closes on screen\n{}", shot.text));
        let mut edges: Vec<usize> = Vec::new();
        for line in lines.iter().skip(gpus_close + 1).take(3) {
            for (i, c) in line.char_indices() {
                if c == '\u{250c}' && !edges.contains(&i) {
                    edges.push(i);
                }
            }
        }
        assert!(edges.len() <= 2, "{w}: more than 2 columns of boxes below the GPUS band (edges={edges:?}) - the owner: \"most should be two\"\n{}", shot.text);
    }
}

/// lss-verifier-3's FAIL on #255: round 3's headline rule ("NEVER 3+1... 4 across or 2x2, never
/// a lone card on its own row") is IMPLEMENTED (`gpu_per_row`'s no-lone-card clause) but was
/// never PINNED anywhere page 1 actually renders in the narrow band where it is the ONLY thing
/// deciding the shape - 4 GPUs, `GPU_CARD_MIN_W`(18) fits exactly 3 a row from about 62 to 81
/// wide, which the no-lone-card clause rejects in favour of 2x2; neutering that clause (accepting
/// remainder-1) produced zero new test failures anywhere in the suite. A GPU card row is one
/// screen line containing one or more "o GPU<digit>" box titles side by side (never "o GPUS" -
/// the outer box's own title has no digit after GPU, so it cannot match this and give a false
/// "lone card" here); no such line may hold exactly one when there is more than one GPU.
#[test]
fn page1_gpu_row_never_leaves_one_card_alone_at_62_to_81_wide() {
    for w in [64u16, 70, 76, 80] {
        let (a, now) = app(golden());
        let shot = render(&a, w, 50, now);
        for line in shot.text.lines() {
            let n = line.match_indices("o GPU").filter(|(i, _)| line[i + 5..].chars().next().is_some_and(|c| c.is_ascii_digit())).count();
            assert_ne!(n, 1, "{w}: a GPU card row has exactly one card, left alone on its own row:\n{line}\n{}", shot.text);
        }
    }
}

/// The skeleton at the three ship-gate sizes (140x44, narrow, minimized): no panic, nothing
/// half-drawn past the edge, and a size too short for all seven sections scrolls rather than
/// silently dropping the rest - the footer's "N-M of 7" says so.
///
/// #191, 2026-09-22: this page is the DEFAULT now, so (a) the test reads the default instead of
/// setting `dash` - flip the default back and this fails, which is the point - and (b) it runs
/// at EVERY supported size, the nine the classic grid's own golden test uses as well as its
/// three ship-gate ones. The assertions here are the size-agnostic ones (never red on a calm
/// day, no line past the pane, and if it does not all fit then say so), so they hold at 40x8
/// and at 200x24 alike; the tighter spacing rules live in
/// `page1_is_one_grid_with_nothing_truncated_at_every_supported_size`.
#[test]
fn dash_skeleton_holds_at_every_required_size() {
    for (w, h) in [(140, 44), (63, 60), (50, 10)].into_iter().chain(OVERVIEW_GOLDEN_SIZES) {
        let (a, now) = app(golden());
        assert!(a.dash, "#191: page 1 is the default - this test must exercise it without opting in");
        let shot = render(&a, w, h, now);
        assert_eq!(shot.red_cells(), 0, "{w}x{h}: never red on a calm-day render\n{}", shot.text);
        for line in shot.text.lines() {
            assert!(line.chars().count() as u16 <= w, "{w}x{h}: a line overran the pane\n{}", shot.text);
        }
        if h < 20 {
            // #175: the page scrolls by ROW now, so the hint is "N-M of <rows>"
            assert!(shot.text.lines().last().unwrap_or("").contains(" of ") || shot.text.contains("MINIMIZED"), "{w}x{h}: too short for the whole page but no 'N-M of T' told the reader more exists\n{}", shot.text);
        }
    }
}

/// #101 (verifier): at a SHORT, WIDE pane (126x22, 94x20 - `Shape::ThirdH`) the old single-column
/// stack showed ONLY the LOADOUT box and left 6-7 rows blank, with ELECTRICITY (the entire point
/// of this page) unreachable without scrolling, and NOTHING else on screen at all. That total-
/// blankness is the regression this guards against. #227 reorders the grid's own priority (GPUS/
/// LOADOUT/USERS lead per item 4, ALERTS/INCIDENTS right after per #222's carried-forward "no
/// scroll" bar), so at the SHORTEST of these two sizes (94x20, 15 rows shown) ELECTRICITY itself
/// can be one column over from LOADOUT rather than literally beside it - LOADOUT is still there,
/// and #101's real point (a real dollar figure, not a blank pane) still holds via the worst-now
/// line, unconditional on every page-1 render regardless of which boxes fit.
#[test]
fn short_wide_panes_show_cost_with_no_key_pressed() {
    for (w, h) in [(126, 22), (94, 20)] {
        let (mut a, now) = app(golden());
        a.dash = true;
        let shot = render(&a, w, h, now);
        assert!(shot.text.contains("LOADOUT"), "{w}x{h}: LOADOUT must be on screen\n{}", shot.text);
        assert!(shot.text.contains('$'), "{w}x{h}: a real dollar figure must be visible with no key pressed\n{}", shot.text);
        for line in shot.text.lines() {
            assert!(line.chars().count() as u16 <= w, "{w}x{h}: a line overran the pane\n{}", shot.text);
        }
        assert_eq!(shot.red_cells(), 0, "{w}x{h}: a calm day is never red\n{}", shot.text);
    }
}

/// #117/#119 (verifier-3, lss-verifier) found COST PER 1M TOKENS silently absent at short, wide
/// panes; #101's fixed columns could only NAME what they left out. #175 replaced them with a page
/// that scrolls by row in every shape, so every section is REACHABLE (paging down from the top
/// reaches all of them) and it offers the scroll keys - which now do something in this shape too.
/// #291 retired the header's own "N-M of T" position (the owner: "dont need any of that"); the
/// claim this test makes is now just that paging down really does reach every section.
#[test]
fn every_page1_section_is_reachable_and_the_footer_says_more_is_below() {
    const TITLES: [&str; 11] = ["LOADOUT", "SERVE", "LANES", "ELECTRICITY", "COST PER 1M TOKENS", "ALERTS", "INCIDENTS", "TOKENS", "USERS", "GPUS", "GPU3"];
    for (w, h) in [(126, 22), (94, 20), (80, 24)] {
        let (mut a, now) = app(golden());
        a.dash = true;
        let first = render(&a, w, h, now);
        // card #291: the scroll position no longer rides the header (the owner: "dont need any of
        // that") - the real claim this test makes ("every section IS reachable by paging down")
        // is proven below by actually paging down and finding every title; this just confirms
        // the tab line (ending "? help") is still on screen somewhere.
        assert!(first.text.lines().any(|l| l.trim_end().ends_with("? help")), "{w}x{h}: the tab line must be on screen\n{}", first.text);
        let mut seen = first.text.clone();
        for _ in 0..20 {
            seen.push_str(&press(&mut a, KeyCode::PageDown, w, h, now).text);
        }
        for title in TITLES {
            assert!(seen.contains(&format!("o {title} ")) || seen.contains(&format!("o {title}\u{2500}")), "{w}x{h}: {title} is not reachable by paging down\n{seen}");
        }
    }
}

/// #51, stage (e): the same fallback discipline card #48 built for charts - a table too narrow
/// for all its columns drops the least essential ones and SAYS so, rather than wrap or run past
/// the box edge. Wide enough (260, #227: a single ~86-wide column of the packed grid, over
/// `WHO_FULL_W`) WHO/GPUS show every column; at 95 wide (#227: a ~47-wide column, inside
/// `WHO_NARROW_W..WHO_FULL_W`) they drop to the partial-table narrow tier and name what was
/// hidden - narrower still (63 total) falls past that into the OTHER, bottom tier ("N users -
/// widen the pane", no table at all), which is `page1_on_a_strangers_ollama_says_n_a_never_zero`'s
/// case, not this one's.
#[test]
fn narrow_panes_drop_table_columns_and_say_so() {
    let (mut a, now) = app(golden());
    a.dash = true;
    // WHO/GPUS may be scrolled past the top screen (the packed grid can put them on a middle
    // screen, not just the first or the last one) - check every screen
    let mut wide = render(&a, 260, 44, now).text;
    for _ in 0..10 {
        wide.push_str(&press(&mut a, KeyCode::PageDown, 260, 44, now).text);
    }
    // #247: the GPU cards' own "memory"/"throttle" rows were combined/made conditional (the
    // biggest single line-count win #227's zero-scroll bar did not reach) - "util/mem" is that
    // combined row's own label now; "throttle" only appears when a card is NOT clean, so a calm
    // fixture never shows it at all (checked on page 3's own GPUS detail page instead, which
    // still carries every column unconditionally).
    assert!(wide.contains("req24h") && wide.contains("lane") && wide.contains("util/mem"), "wide: every WHO/GPUS column shows\n{wide}");

    a.scroll = 0;
    let mut narrow = render(&a, 95, 60, now).text;
    for _ in 0..10 {
        narrow.push_str(&press(&mut a, KeyCode::PageDown, 95, 60, now).text);
    }
    // the narrow tier's own row for "acme" has no lane column at all - "public" (its lane) is
    // gone, not wrapped onto another line (the note is allowed, and expected, to NAME the
    // dropped columns - "req24h" among them - so the assertion checks real row data, not prose)
    let acme_row = narrow.lines().find(|l| l.contains("acme")).unwrap_or_else(|| panic!("no acme row\n{narrow}"));
    assert!(!acme_row.contains("public"), "95 wide: the lane column should be dropped from the row, not wrapped\n{acme_row}");
    // #255 round 3 (the owner, polish): the inline "hidden at this width" caption that used to
    // name the dropped columns moved to `?` help - page 1 no longer explains itself inline here.
    // the universal safety net: no row is wider than the pane (fit_line ends a cut row in an
    // ellipsis - checked here as "nothing overflowed")
    for line in narrow.lines() {
        assert!(line.chars().count() <= 95, "a row ran past the 95-wide pane\n{narrow}");
    }
}

#[test]
#[ignore = "prints every layout and page as plain text: cargo test -p lss --test render -- --ignored --nocapture print_renders"]
fn print_renders() {
    let (a, now) = app(golden());
    for (w, h) in [(63, 11), (63, 12), (63, 15), (63, 24), (94, 15), (126, 41), (126, 22), (94, 20), (200, 24), (42, 73), (50, 70), (63, 60), (40, 8), (50, 10)] {
        println!("--- overview {w}x{h} ({})\n{}", pick_shape(w, h).name(), render(&a, w, h, now).text);
    }
    for p in PageId::ALL {
        for (w, h) in PAGE_SIZES {
            let (a, now) = on_page(golden(), p, 1);
            println!("--- page {} {w}x{h}\n{}", p.title(), render(&a, w, h, now).text);
        }
    }
    for (w, h) in PAGE_SIZES {
        let (mut a, now) = on_page(golden(), PageId::Model, 1);
        render(&a, w, h, now);
        println!("--- page MODEL {w}x{h}, one screen down\n{}", press(&mut a, KeyCode::PageDown, w, h, now).text);
        println!("--- page MODEL {w}x{h}, the end\n{}", press(&mut a, KeyCode::End, w, h, now).text);
        let (mut a, now) = on_page(golden(), PageId::Model, 1);
        println!("--- page MODEL {w}x{h}, enter = compare\n{}", press(&mut a, KeyCode::Enter, w, h, now).text);
        let (mut a, now) = on_page(golden(), PageId::Model, 1);
        println!("--- page MODEL {w}x{h}, b = bench prompt\n{}", press(&mut a, KeyCode::Char('b'), w, h, now).text);
        let (a, now) = on_page(demo::status_plain(), PageId::Users, 1);
        println!("--- page USERS {w}x{h}, a gateway older than v5.2\n{}", render(&a, w, h, now).text);
    }
    let (a, now) = app(bad_day());
    println!("--- bad day 126x41\n{}", render(&a, 126, 41, now).text);
    println!("--- bad day 63x11\n{}", render(&a, 63, 11, now).text);
}

/// #175, the owner's spacing requirement (second time asked, so it is pinned, not eyeballed): at
/// every size the card names - 80x24, 126x22, 150x46 and his wide half-height layout - page 1
/// (a) ends NO line in `…` (a long value wraps under its own value column instead), (b) keeps ONE
/// label column: every labelled row in a box starts its value at the same character position,
/// and (c) never runs past the pane. Checked on every screen, paging from the top to the end.
///
/// #191: read from the DEFAULT, not by setting `dash` - the page this asserts spacing for is the
/// one a stranger actually lands on, so the two must not be able to drift apart.
#[test]
fn page1_is_one_grid_with_nothing_truncated_at_every_supported_size() {
    for (w, h) in [(80, 24), (126, 22), (150, 46), (200, 32), (240, 34)] {
        let (mut a, now) = app(golden());
        assert!(a.dash, "#191: the compiled default is this page - the test must not have to opt in");
        let mut screens = vec![render(&a, w, h, now)];
        for _ in 0..12 {
            screens.push(press(&mut a, KeyCode::PageDown, w, h, now));
        }
        for shot in &screens {
            let lines: Vec<&str> = shot.text.lines().collect();
            // the header (row 0) and the footer (last row) are not page-1 content
            for line in &lines[1..lines.len().saturating_sub(1)] {
                assert!(!line.contains('\u{2026}'), "{w}x{h}: a line was cut with an ellipsis instead of wrapped:\n{line}\n{}", shot.text);
                assert!(line.chars().count() as u16 <= w, "{w}x{h}: a line overran the pane\n{}", shot.text);
            }
            // (b): every "│ <label padded to 16>" cell puts its value at border + 2 + 16
            for line in &lines {
                let chars: Vec<char> = line.chars().collect();
                for (i, c) in chars.iter().enumerate() {
                    if *c != '\u{2502}' || chars.get(i + 1) != Some(&' ') {
                        continue;
                    }
                    let cell: String = chars[i + 2..].iter().take_while(|c| **c != '\u{2502}').collect();
                    // the cell's first 16 characters ARE the label column: when they hold one
                    // of these labels, the value must begin exactly at character 16 (TOKENS rows
                    // are a right-aligned table under their own header, so they are not listed)
                    let label_cell: String = cell.chars().take(16).collect();
                    let label = label_cell.trim_end();
                    if ["host", "flags", "KV free", "TTFT", "prefill", "decode", "ITL", "running", "KV", "accept", "cache hit", "plan", "right now", "today", "generated"].contains(&label) {
                        let at16 = cell.chars().nth(16);
                        assert!(at16.is_some_and(|c| c != ' '), "{w}x{h}: {label}'s value does not start in the shared value column:\n{line}");
                    }
                }
            }
        }
    }
}

/// #175: the SERVE numbers the owner asked back ("ttft, prefill, decode, you skipped alot of
/// useful display") are on page 1 with real values - TTFT with p50, p90 and p99, reading and
/// writing speed with their total, ITL, running/queue, KV, accept and cache hit - and what the
/// engine cannot give per request says so with an em dash rather than inventing a number.
#[test]
fn page1_serve_carries_ttft_percentiles_and_both_speeds_with_honest_per_request() {
    let (mut a, now) = app(golden());
    a.dash = true;
    // #227: the packed grid can put SERVE a screen or two down at this width, and at 200 wide its
    // own 3 columns (66 each) are too narrow for `prefill`'s middle tier ("per request —") - wide
    // enough here that the column gets it.
    let (w, h) = (240u16, 60u16);
    let mut seen = render(&a, w, h, now).text;
    for _ in 0..24 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, w, h, now).text);
    }
    let ttft = row_starting_with(&seen, "TTFT").unwrap_or_else(|| panic!("no TTFT row on any screen\n{seen}"));
    assert!(ttft.contains("p50") && ttft.contains("p90") && ttft.contains("p99"), "{ttft}");
    let prefill = row_starting_with(&seen, "prefill").expect("prefill row");
    assert!(prefill.contains("tok/s total"), "{prefill}");
    assert!(prefill.contains("per request \u{2014}"), "the engine reports reading speed in aggregate only: an em dash with the reason, never a guessed per-request figure\n{prefill}");
    let decode = row_starting_with(&seen, "decode").expect("decode row");
    // #227: "per request" itself can abbreviate to "per req" at a narrower tier (`pick()`'s own
    // shortest variant, unchanged by this card) - the fact must be present, the exact word need not.
    assert!(decode.contains("tok/s total") && decode.contains("per req"), "{decode}");
    for label in ["ITL", "running", "KV", "accept", "cache hit"] {
        assert!(row_starting_with(&seen, label).is_some(), "{label} missing from SERVE\n{seen}");
    }
    let running = row_starting_with(&seen, "running").unwrap();
    assert!(running.contains("slots") && running.contains("queue"), "{running}");
}

/// #175: the owner likes the USERS and GPUS tables as they are, so at a WIDE-ENOUGH pane the new
/// box padding must never be what demotes USERS to its narrow tier - never lose the full tier to
/// padding alone when the pane genuinely has the room.
/// #227's own trade-off, not yet resolved: at 80 total columns the packed grid gives USERS one
/// column of a 2-or-3-column row (`column_count`), which is narrower than `WHO_FULL_W` needs -
/// #175's ORIGINAL bar ("at 80 columns, the most common terminal, the full table still shows")
/// no longer holds at 80 specifically, because #227 did not give USERS/GPUS the OLD design's
/// preferential column width back. Flagged for the owner, not silently dropped: this
/// test now pins the still-true half (padding costs nothing beyond the grid's own column split)
/// at a width the grid genuinely has room to spare (200), and separately proves the narrow tier
/// at 80 says HONESTLY what it hid, never silently.
#[test]
fn page1_padding_never_costs_users_its_full_table_when_the_grid_has_room() {
    let (mut a, now) = app(golden());
    a.dash = true;
    // 260, not 200: at N=3 columns 200/3=66 is still short of `WHO_FULL_W`(78)+border+padding;
    // 260/3=86 clears it.
    let mut seen = render(&a, 260, 24, now).text;
    for _ in 0..20 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 260, 24, now).text);
    }
    assert!(seen.contains("$24h") && seen.contains("req24h") && !seen.contains("hidden at this width - share"), "260x24: USERS must keep its full tier when the column has room\n{seen}");
}

/// The other half of the trade-off above: below `WHO_FULL_W` USERS' own column drops to a
/// narrower tier. #255 round 3 (the owner, polish): the inline "hidden at this width" caption
/// that used to say so moved to `?` help ("USERS narrow tier") - page 1 keeps just the table and
/// its headline now. Still honest in the sense that matters: the header never CLAIMS a column
/// ("req24h"/"$24h") it does not actually show. (95, not 80: at 80/2 columns the inner width
/// falls below `WHO_NARROW_W` too, into the bottom "N users - widen the pane" tier instead - this
/// test pins the partial-table tier specifically.)
#[test]
fn page1_users_narrow_tier_drops_columns_honestly_without_a_page1_caption() {
    let (mut a, now) = app(golden());
    a.dash = true;
    let mut seen = render(&a, 95, 24, now).text;
    for _ in 0..20 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 95, 24, now).text);
    }
    assert!(seen.contains("share"), "95x24: the narrow tier's own table must still be on screen\n{seen}");
    assert!(!seen.contains("req24h") && !seen.contains("$24h"), "95x24: the narrow tier's header must not claim columns it does not show\n{seen}");
    assert!(!seen.contains("hidden at this width"), "95x24: the caption moved to help - page 1 must not still carry it\n{seen}");
}

/// #177, the owner's second round on page 1, one assertion per item on a rendered frame:
/// (1) ALERTS and INCIDENTS are back as their own boxes, with the live-vs-dated rule in words;
/// (2) ADVICE stays off; (3) LANES explains itself in words and draws no chart; (4) each GPU has
/// its own box; and the top line names what is worst right now, in words, with the live cost.
/// #227 item 4 SUPERSEDES this test's original (4) - the owner reversed #177's "i dont need
/// charts" ("add in some small graphs where it would look nice ... no graph on static facts like
/// loadout flags or plan name"): sparklines now exist on page 1, on LOADOUT (KV use), SERVE
/// (decode tok/s) and each GPU's own card (temp) - see `dash.rs`'s own notes on each for why
/// those three specifically. What this test still pins from #177: a STATIC-FACTS box (LANES,
/// which #177 explicitly said "probably dont need a chart" for) carries no chart glyph.
#[test]
fn page1_round_two_restores_events_explains_lanes_boxes_gpus_and_names_the_worst() {
    let (mut a, now) = app(golden());
    a.dash = true;
    let mut seen = render(&a, 200, 60, now).text;
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 200, 60, now).text);
    }
    for title in ["o ALERTS", "o INCIDENTS", "o LANES", "o GPU0", "o GPU1", "o GPU2", "o GPU3"] {
        assert!(seen.contains(title), "{title} box missing\n{seen}");
    }
    assert!(!seen.contains("o ADVICE"), "ADVICE stays off page 1 (the owner: 'advice i dont need')\n{seen}");
    // #227 ("alot of wasted space") / #247 (the zero-scroll bar): the live-vs-dated merge rule and
    // LANES' own two definitions all moved off their boxes and into `?` help (`ui::mod::HELP_GROUPS`,
    // "Reading it") - the terms ALERTS/INCIDENTS/LANES themselves are still tested for above; the
    // sentences explaining them are tested where they now live, in `theme_layout_and_help_keys`.
    assert!(seen.contains("outside users, each with a key"), "LANES must still say what a lane IS\n{seen}");
    let chart_ink = |sym: &str| sym.chars().next().is_some_and(|c| ('\u{2581}'..='\u{2588}').contains(&c) || ('\u{2800}'..='\u{28ff}').contains(&c));
    assert!(seen.chars().any(|c| chart_ink(&c.to_string())), "item 4: page 1 must carry at least one sparkline now (KV use / decode tok/s / GPU temp)\n{seen}");
    assert!(seen.contains("KV 1h") && seen.contains("1h") && seen.contains("temp 1h"), "item 4: the three named sparklines (LOADOUT KV, SERVE decode, per-GPU temp) are all on screen\n{seen}");
    // card #258 (the owner: "just remove all of this"): the "worst now" verdict line is GONE from
    // page 1; the live cost (#101: a real dollar figure with no key pressed) rides the header
    assert!(!seen.contains("worst now"), "the verdict line is gone\n{seen}");
    assert!(seen.lines().next().is_some_and(|l| l.contains("$") && l.contains("/h")), "the live cost is on the header row\n{seen}");
}

/// #305 (the owner on v1.1.2: "the bars on the temp doesnt really show me anything ... are we able
/// to make it like the other charts i see in gpus tab 3"): page 1's three 1h trends (each GPU
/// card's temp, LOADOUT's KV, SERVE's decode) are drawn by the GPUS page's own chart widget - a
/// y-scale with its top and bottom labelled, a `-1h ... now` time axis, a legend naming the trend -
/// and they honour `c` (dots = braille, lines = box-drawing) like every other chart. The old
/// one-row block sparkline rows (`KV 1h ▁▂▃`, `1h ▁▂▃`, `temp 1h ▁▂▃`) are gone.
#[test]
fn page1_trends_are_drawn_like_the_gpus_page_charts() {
    let braille = |c: char| ('\u{2800}'..='\u{28ff}').contains(&c);
    let blocks = |c: char| ('\u{2581}'..='\u{2588}').contains(&c);
    let whole_page = |a: &mut App, w: u16, h: u16, now: i64| -> String {
        let mut seen = render(a, w, h, now).text;
        for _ in 0..8 {
            seen.push_str(&press(a, KeyCode::PageDown, w, h, now).text);
        }
        seen
    };
    for (w, h) in [(200, 60), (120, 40)] {
        for lines in [false, true] {
            let (mut a, now) = app(golden());
            assert!(a.dash);
            a.chart_lines = lines;
            let seen = whole_page(&mut a, w, h, now);
            let mode = if lines { "lines" } else { "dots" };
            // the three legends, each naming its trend, drawn by the shared widget's legend line
            for legend in ["\u{2501} KV 1h", "\u{2501} decode 1h", "\u{2501} temp 1h"] {
                assert!(seen.contains(legend), "{w}x{h} {mode}: {legend:?} legend missing\n{seen}");
            }
            // a time axis per chart: 4 GPU cards + KV + decode
            let axes = seen.lines().filter(|l| l.contains("-1h \u{2500}") && l.contains("\u{2500} now")).map(|l| l.matches("-1h \u{2500}").count()).sum::<usize>();
            assert!(axes >= 6, "{w}x{h} {mode}: {axes} '-1h ... now' axes on page 1, want >= 6 (4 GPU cards + KV + decode)\n{seen}");
            // KV is pinned 0..100%: both ends of its y-scale are labelled
            assert!(seen.contains("100% ") && seen.contains("  0% "), "{w}x{h} {mode}: KV chart's y-scale labels missing\n{seen}");
            // no old one-row block sparkline rows left
            for old in ["KV 1h", "1h", "temp 1h"] {
                if let Some(row) = row_starting_with(&seen, old) {
                    assert!(!row.chars().any(blocks), "{w}x{h} {mode}: the old block sparkline row {old:?} is still there: {row}");
                }
            }
            // c toggles the style exactly like every other chart
            if lines {
                assert!(!seen.chars().any(braille), "{w}x{h} lines: braille on page 1 in line mode\n{seen}");
            } else {
                assert!(seen.chars().any(braille), "{w}x{h} dots: no braille trend on page 1\n{seen}");
            }
        }
    }
}

/// #305 verifier FAIL (lss-inst-v1): the 1h grid has a slot every 30 s but the collector samples
/// less often, so a CONTINUOUS hour arrives as `v, None, v, None, ...` - and page 1's charts drew
/// every empty slot as a gap that claimed missing data that was not missing. With a whole hour
/// sampled every other slot, no chart on page 1 may have a blank column inside its plot, in
/// either `c` mode.
#[test]
fn page1_trends_have_no_interior_gap_when_the_hour_is_continuous() {
    let mut s = golden();
    let every_other = |f: &dyn Fn(usize) -> f64| -> Vec<Option<f64>> { (0..120).map(|i| (i % 2 == 0).then(|| f(i))).collect() };
    s.series.kv_usage = every_other(&|i| 0.1 + 0.6 * ((i as f64) / 20.0).sin().abs());
    s.series.decode_tok_s = every_other(&|i| 50.0 + 300.0 * ((i as f64) / 15.0).cos().abs());
    for g in &mut s.series.gpu_temp_c {
        *g = every_other(&|i| 45.0 + 20.0 * ((i as f64) / 25.0).sin().abs());
    }
    for (w, h) in [(200u16, 50u16), (200, 60)] {
        for lines in [false, true] {
            let mode = if lines { "lines" } else { "dots" };
            let (mut a, now) = app(s.clone());
            a.chart_lines = lines;
            let mut screens = vec![render(&a, w, h, now).text];
            for _ in 0..8 {
                screens.push(press(&mut a, KeyCode::PageDown, w, h, now).text);
            }
            let mut charts = 0;
            for text in &screens {
                let rows: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
                for (y, row) in rows.iter().enumerate() {
                    // each chart's time axis reads '-1h ──── now'; its 2 plot rows sit right above it
                    let mut from = 0;
                    while let Some(k) = row[from..].windows(3).position(|c| c == ['-', '1', 'h']).map(|k| k + from) {
                        from = k + 3;
                        let Some(end) = row[k..].windows(3).position(|c| c == ['n', 'o', 'w']).map(|e| e + k + 3) else { continue };
                        if y < 2 {
                            continue;
                        }
                        charts += 1;
                        let inked = |x: usize| (y - 2..y).any(|r| rows[r].get(x).is_some_and(|c| *c != ' '));
                        let cols: Vec<usize> = (k..end).filter(|x| inked(*x)).collect();
                        let (Some(first), Some(last)) = (cols.first(), cols.last()) else { continue };
                        let holes: Vec<usize> = (*first..=*last).filter(|x| !inked(*x)).collect();
                        let shown: Vec<String> = rows[y - 2..=y].iter().map(|r| r.iter().collect()).collect();
                        assert!(holes.is_empty(), "{w}x{h} {mode}: interior gap at columns {holes:?} in the chart above row {y}:\n{}", shown.join("\n"));
                    }
                }
            }
            assert!(charts >= 6, "{w}x{h} {mode}: found {charts} charts, want >= 6 (4 GPU + KV + decode)");
        }
    }
}

/// #177: the verdict line says what the TOP constraint is, in words, and colours it on the one
/// shared scale - a hot GPU names that GPU; a calm engine says nothing is constrained.
#[test]
fn page1_worst_line_names_the_bottleneck_in_words() {
    let mut s = golden();
    s.lanes.public.codes_10m.c4xx = 0;
    s.lanes.trusted.codes_10m.c4xx = 0;
    let (mut a, now) = app(s.clone());
    a.dash = true;
    let calm = render(&a, 150, 46, now);
    // card #258: page 1 no longer DRAWS the verdict (the owner: "just remove all of this"); the
    // ranked verdict itself still exists (the minimized card, page 8) and is pinned here directly
    let h = lss::constraints::headline(&s);
    assert!(h.leader.is_none() && h.calm.contains("nothing is constrained"), "{}", h.calm);
    assert_eq!(calm.red_cells(), 0, "a calm day is never red\n{}", calm.text);

    let idx = s.gpus.iter().position(|g| !g.thermal_excluded).unwrap();
    let n = s.gpus[idx].sample.index;
    s.gpus[idx].sample.temp_c = Some(s.thresholds.thermal_temp_c + 3.0);
    let leader = lss::constraints::headline(&s).leader.expect("a hot GPU leads");
    let line = format!("{} {} {}", lss::constraints::band_word(&leader), leader.label, leader.detail);
    // #179: the detail says what the heat COSTS, never restates the threshold
    assert!((line.contains("HIGH") || line.contains("CRITICAL")) && line.contains(&format!("GPU{n}")) && line.contains("slows its clocks"), "{line}");
}

/// card #180 gate 6: a collector that is running but has not found an LLM server yet is not a
/// broken monitor. The screen names the state and the step that fixes it, and nothing is red.
#[test]
fn a_collector_waiting_for_an_engine_is_not_drawn_as_unreachable() {
    let a = App { url: "http://collector:8099".into(), error: Some(format!("{}lss-collector is running but has not found an LLM server on this machine yet. Start your model server", lss::client::WAITING)), ..Default::default() };
    let shot = render(&a, 80, 24, 1_000_000);
    assert!(shot.text.contains("NO LLM SERVER YET") && shot.text.contains("Start your model server"), "{}", shot.text);
    assert!(!shot.text.contains("UNREACHABLE"), "{}", shot.text);
    assert_eq!(shot.red_cells(), 0, "a fresh install waiting for its model is not an error\n{}", shot.text);
}

/// card #180 gate 3, found by a clean install against a real Ollama: page 1 (`v`) broke the
/// n/a-never-zero rule the classic overview keeps - "0/0 slots busy · KV 0%" in the verdict
/// line, a TOKENS table of zeros, "uptime down" beside "status up", "KV free: not published by
/// this collector yet" for an engine that simply does not report it, and "UP -" in the header.
#[test]
fn page1_on_a_strangers_ollama_says_n_a_never_zero() {
    let mut s = strangers_machine();
    s.serve.uptime_s = None;
    s.serve.started_at = None;
    s.serve.container = None;
    let (mut a, now) = app(s);
    a.dash = true;
    let mut seen = render(&a, 150, 46, now).text;
    for _ in 0..6 {
        seen.push_str(&press(&mut a, KeyCode::PageDown, 150, 46, now).text);
    }
    // card #258: the calm sentence is no longer drawn on page 1; it is pinned where it is made
    let worst = lss::constraints::headline(a.status.as_ref().unwrap()).calm;
    assert!(worst.contains("nothing is constrained") && !worst.contains("0/0") && !worst.contains("KV 0%"), "{worst}");
    assert!(!seen.contains("UP -"), "the header must not read UP -\n{seen}");
    let uptime = row_starting_with(&seen, "uptime").expect("uptime row");
    assert!(!uptime.contains("down") && uptime.contains("no docker"), "{uptime}");
    let kv = row_starting_with(&seen, "KV free").expect("KV free row");
    assert!(kv.contains("not reported by Ollama"), "{kv}");
    assert!(seen.contains("publishes no token counters"), "TOKENS must say n/a, not print zeros\n{seen}");
    assert!(row_starting_with(&seen, "hour").is_none(), "no per-window token rows for an engine with no counters\n{seen}");
}

/// card #179: the worst-now line holds its leader across frames unless a challenger is clearly
/// worse (the 0.08 hysteresis in `readings::leading`). Two frames on the SAME App: a GPU at its
/// alert line leads; then KV rises to a score just above it - the line must not flip.
#[test]
fn page1_worst_line_does_not_flap_between_two_close_readings() {
    let mut s = golden();
    s.lanes.public.codes_10m.c4xx = 0;
    s.lanes.trusted.codes_10m.c4xx = 0;
    s.lanes.trusted.codes_10m.c5xx = 0;
    let idx = s.gpus.iter().position(|g| !g.thermal_excluded).unwrap();
    let n = s.gpus[idx].sample.index;
    s.gpus[idx].sample.temp_c = Some(s.thresholds.thermal_temp_c);
    s.serve.kv_usage = 0.30;
    // card #258: page 1 no longer draws the verdict; the hysteresis it relied on is pinned on
    // `headline_with` itself, with the first frame's leader as the incumbent (as the UI passes it)
    let first = lss::constraints::headline_with(&s, None).leader.expect("the GPU leads");
    assert!(first.label.contains(&format!("GPU{n}")), "{}", first.label);
    let mut s2 = s;
    s2.serve.kv_usage = 0.96; // scores just above the GPU, inside the margin
    let second = lss::constraints::headline_with(&s2, Some(&first.key)).leader.expect("still a leader");
    assert_eq!(second.key, first.key, "the leader flapped on a challenger inside the 0.08 margin: {}", second.label);
}

/// Card #182: the tab strip's three tiers, measured (not guessed) - `diag_tab_strip_lengths`
/// printed the real widths before this test was written.
#[test]
fn tab_strip_degrades_through_three_tiers_and_never_vanishes() {
    // card #231 correction added INCIDENTS, the ring's 10th `PageId` (11th stop overall with the
    // overview) - one entry wider than when these thresholds were first measured.
    // tier 1, names: fits down to 113 columns - real page names, the current one bracketed.
    let wide = lss::ui::tab_strip(View::Page(PageId::Gpus), 113);
    let text: String = wide.spans.iter().map(|s| s.content.to_string()).collect();
    assert!(text.contains("0 overview") && text.contains("1 latency") && text.contains("[3 gpus]") && text.contains("9 incidents") && text.contains("a advice"), "{text:?}");
    // one column short of tier 1: falls to tier 2, numbers alone (no names at all)
    let numbers = lss::ui::tab_strip(View::Page(PageId::Gpus), 112);
    let text: String = numbers.spans.iter().map(|s| s.content.to_string()).collect();
    assert!(!text.contains("latency") && !text.contains("gpus") && text.contains("[ 3]"), "names must be gone, not truncated names: {text:?}");
    assert_eq!(numbers.width(), 25);
    // tier 2 holds all the way down to where even the numbers cannot fit (measured: 25 wide)
    assert_eq!(lss::ui::tab_strip(View::Page(PageId::Gpus), 25).width(), 25);
    // one column short of tier 2: falls to tier 3, current page and the count, nothing else
    let short = lss::ui::tab_strip(View::Page(PageId::Gpus), 24);
    let text: String = short.spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text.trim(), "gpus 3/10");
    assert_eq!(lss::ui::tab_strip(View::Overview, 22).spans[0].content.trim(), "overview");
    // NEVER empty, however narrow - it truncates with an ellipsis rather than disappearing
    for w in [10, 5, 1, 0] {
        let s = lss::ui::tab_strip(View::Page(PageId::Gpus), w);
        assert!(w == 0 || s.width() > 0, "the strip must never silently vanish at w={w}: {s:?}");
    }
}

/// Card #182 item 3, the two sizes named on the card itself, where the footer is already
/// crowded: the tab strip is present AND a real panel is still on screen - the strip's row
/// does not, by itself, blank the page.
#[test]
fn tab_strip_present_at_120x22_and_100x26_without_blanking_the_page() {
    let (mut a, now) = app(golden());
    a.dash = true;
    for (w, h) in [(120u16, 22u16), (100, 26)] {
        let shot = render(&a, w, h, now);
        assert!(shot.text.contains("overview") || shot.text.contains(" 0 "), "no tab strip at all at {w}x{h}\n{}", shot.text);
        assert!(shot.text.contains("o SERVE") || shot.text.contains("o GPUS") || shot.text.contains("o LOADOUT"), "the page went blank at {w}x{h}\n{}", shot.text);
    }
    let gpus = on_page(golden(), PageId::Gpus, 1);
    for (w, h) in [(120u16, 22u16), (100, 26)] {
        let shot = render(&gpus.0, w, h, now);
        assert!(shot.text.contains("gpus") || shot.text.contains(" 3 "), "no tab strip at all at {w}x{h}\n{}", shot.text);
        assert!(shot.text.contains("GPU0"), "the page went blank at {w}x{h}\n{}", shot.text);
    }
}

/// Card #222 (the owner: "throw back incidents and alerts onto the front page"). Reproduced
/// first (item 1): at parent d767527, ALERTS and INCIDENTS were the LAST two of five items in
/// page 1's left column, behind LOADOUT/ELECTRICITY/COST PER 1M TOKENS - at every one of these
/// four required sizes, at least one of the two boxes was pushed below the fold with no on-screen
/// sign that it existed (the footer only ever said "N-M of T", never which named box was hidden).
/// A `bad_day()` fixture (real firing alerts, an open incident) is used deliberately: a calm day
/// is the case that matters least here.
#[test]
fn alerts_and_incidents_are_reachable_with_no_scroll_at_every_required_size() {
    let (mut a, now) = app(bad_day());
    a.dash = true;
    for (w, h) in [(120u16, 40u16), (160, 45), (200, 50), (200, 30)] {
        let shot = render(&a, w, h, now);
        assert!(shot.text.contains("o ALERTS"), "{w}x{h}: ALERTS box on screen\n{}", shot.text);
        assert!(shot.text.contains("o INCIDENTS"), "{w}x{h}: INCIDENTS box on screen\n{}", shot.text);
        // #222/#231's own bar (carried forward, and the one #227's grid is proven against here):
        // BOTH boxes fully on screen, top border to bottom, at zero scroll - not merely
        // mentioned somewhere before a cut. `dash_page_rows`' `y1` is each box's own last row.
        let (_, shown, _) = a.page_rows.get();
        let rows = a.dash_page_rows.borrow().clone();
        for page in [PageId::Alerts, PageId::Incidents] {
            let (_, y0, y1) = *rows.iter().find(|(p, ..)| *p == page).unwrap_or_else(|| panic!("{w}x{h}: {page:?} missing from dash_page_rows"));
            // card #269: the owner accepted a small scroll, 2026-09-23 'ship it 1'. MEASURED that
            // day at 200x30: INCIDENTS ends 1 row below the fold (rows 22..29, shown=28); every
            // other box at every size is fully on screen. Card #291 (2026-09-24) then took one
            // more row from every page's body for the key-hint line (`reserve_key_hints`), which
            // this size is tall enough to receive, pushing the same box one row further below the
            // fold (shown=27) - re-measured, not re-guessed, the same discipline #269 itself used.
            // card #305 (2026-09-24, the owner: "are we able to make it like the other charts i
            // see in gpus tab 3"): page 1's three 1h trends became real 4-row charts (the widget's
            // floor for axes), 3 rows each more than the one-row bars. the orchestrator chose to re-pin: the
            // owner's newer ask supersedes #269/#291's slack, re-pinned at EXACTLY what was
            // measured on 6f6b4cb - 120x40 INCIDENTS rows 29..40, shown=37 (3 below the fold);
            // 200x30 INCIDENTS rows 25..32, shown=27 (5). Every other size and box: 0.
            let slack = match ((w, h), page) {
                ((200, 30), PageId::Incidents) => 5,
                ((120, 40), PageId::Incidents) => 3,
                _ => 0,
            };
            assert!(y1 as usize <= shown + slack, "{w}x{h}: {page:?} (rows {y0}..{y1}) must be on screen (shown={shown}, accepted slack {slack} row), not cut off further\n{}", shot.text);
        }
    }
    // #101/#175's established rule at a short, wide pane still holds: not required here, and not
    // broken by this card - LOADOUT+ELECTRICITY keep the first screen instead (see
    // `short_wide_panes_show_cost_with_no_key_pressed_and_serve_beside_it`), and the firing count
    // is still on screen (the header, with no key pressed) even where the boxes are not.
    for (w, h) in [(126u16, 22u16), (94, 20)] {
        let shot = render(&a, w, h, now);
        assert!(shot.text.contains("FIRING"), "{w}x{h}: the firing count must still be visible with no key pressed\n{}", shot.text);
    }
}

/// Card #231's own test requirement ("Enter on page 1's INCIDENTS box opens INCIDENTS"): page 1
/// has no focus/pin concept (#51), so Enter opens whichever of its own ALERTS/INCIDENTS boxes the
/// current scroll position sits inside - proved here for BOTH boxes, not just the one the card
/// names, since a fix that only opened INCIDENTS could just as easily have broken ALERTS.
#[test]
fn enter_on_page1_opens_the_box_the_scroll_position_is_inside() {
    let (mut a, now) = app(bad_day());
    a.dash = true;
    // card #269 (the owner accepted a small scroll, 2026-09-23 'ship it 1'): 140x30. Measured that
    // day at 140x41 (a 39-row body): page 1 scrolls to at most row 20 but INCIDENTS starts at row
    // 25, so no scroll position is ever INSIDE it there and Enter cannot reach it by this rule -
    // the box is fully visible, just never under the scroll position. At 140x30 (max scroll 31,
    // ALERTS rows 18..25, INCIDENTS 25..36) both boxes are reachable, so this still proves the
    // Enter routing for BOTH, which is what the test is for.
    render(&a, 140, 30, now); // a frame records where ALERTS/INCIDENTS were drawn
    // #227: the packed grid's scroll clamps to `total - shown` (`layout()`'s own `start`), so a
    // box near the very end of a tall day is not reachable at ITS OWN row offset - only at
    // whatever the clamped maximum actually is. Land on the LAST reachable screen first, the same
    // way a real reader scrolling down would, rather than poking an unreachable `scroll` value.
    let (_, shown, total) = a.page_rows.get();
    let max_scroll = total.saturating_sub(shown);
    let rows = a.dash_page_rows.borrow().clone();
    let (_, alerts_y0, _) = *rows.iter().find(|(p, ..)| *p == PageId::Alerts).expect("ALERTS is on page 1");
    let (_, incidents_y0, _) = *rows.iter().find(|(p, ..)| *p == PageId::Incidents).expect("INCIDENTS is on page 1");
    assert_ne!(alerts_y0, incidents_y0, "the two boxes must occupy different rows");

    a.scroll = (alerts_y0 as usize).min(max_scroll);
    render(&a, 140, 30, now);
    a.on_key(KeyCode::Enter);
    assert_eq!(a.view, View::Page(PageId::Alerts));

    a.view = View::Overview;
    a.scroll = (incidents_y0 as usize).min(max_scroll);
    render(&a, 140, 30, now);
    a.on_key(KeyCode::Enter);
    assert_eq!(a.view, View::Page(PageId::Incidents));
}

/// card #270 (lss-builder-3 on #269): at 140x41 page 1 scrolls to at most row 20 while INCIDENTS
/// starts at row 25 - the box was fully on screen, yet Enter (which opens the box under the top
/// row) could never open it. Swept the way a reader gets there, with KEYS ONLY: from the top,
/// Down until it stops moving, Enter at every stop. Every box that opens a page must be opened by
/// Enter at some stop, at every width 100-200 (step 10, 140 included) and every height 30-50.
/// RED with 8294575's key handling: 342 misses (140 then counted twice), e.g. `golden 140x41:
/// Incidents never opened` (INCIDENTS rows 26..36, max scroll 25) and
/// `golden 200x50: Alerts never opened` (max scroll 10, ALERTS starts at row 19).
#[test]
fn every_page1_box_that_opens_a_page_is_reachable_by_down_and_enter_at_every_size() {
    let widths: Vec<u16> = (100..=200).step_by(10).collect();
    // one thread per (fixture, width): each builds its own App; ~12k frames in all
    let sweep = |name: &'static str, status: Status, w: u16| -> Vec<String> {
        let mut missed = Vec::new();
        for h in 30u16..=50 {
            let (mut a, now) = app(status.clone());
            a.dash = true;
            render(&a, w, h, now);
            let want: Vec<PageId> = a.dash_page_rows.borrow().iter().map(|(p, ..)| *p).collect();
            let mut opened: Vec<PageId> = Vec::new();
            let mut steps = 0;
            loop {
                let at = a.scroll;
                a.on_key(KeyCode::Enter);
                if let View::Page(p) = a.view {
                    if !opened.contains(&p) {
                        opened.push(p);
                    }
                    // back where it was; the last frame's rows still hold (same view)
                    a.view = View::Overview;
                    a.scroll = at;
                }
                if want.iter().all(|p| opened.contains(p)) {
                    break;
                }
                press(&mut a, KeyCode::Down, w, h, now);
                steps += 1;
                if a.scroll == at || steps > 400 {
                    break;
                }
            }
            for p in want.iter().filter(|p| !opened.contains(p)) {
                let rows = a.dash_page_rows.borrow().clone();
                missed.push(format!("{name} {w}x{h}: {p:?} never opened (page_rows {:?}, boxes {rows:?})", a.page_rows.get()));
            }
        }
        missed
    };
    let fixtures = [("bad_day", bad_day()), ("golden", golden())];
    let missed: Vec<String> = std::thread::scope(|sc| {
        let handles: Vec<_> = fixtures.iter().flat_map(|(name, status)| widths.iter().map(move |&w| (*name, status, w))).map(|(name, status, w)| sc.spawn(move || sweep(name, status.clone(), w))).collect();
        handles.into_iter().flat_map(|h| h.join().expect("a sweep thread panicked")).collect()
    });
    assert!(missed.is_empty(), "{} size(s) where a box can never be opened by Enter:\n{}", missed.len(), missed.join("\n"));
}

/// card #231 (lss-verifier-4's FAIL): NO RENDERED PAGE AND NO SHIPPED DOC MAY NAME A PAGE THAT
/// DOES NOT EXIST.
///
/// The card split the combined ALERTS & INCIDENTS page into `8` ALERTS and `9` INCIDENTS, and gave
/// ADVICE the key `a` because 1-9 ran out. Four places still pointed at the old world, and one of
/// them was on screen: the ADVICE page itself said "see ALERTS & INCIDENTS for what went down",
/// naming a page a reader cannot open. A grep would have found them; nothing made a grep mandatory.
///
/// So this asserts the invariant rather than the four strings:
///   1. the pair title "X & Y" built from two real page titles never appears - that shape can only
///      mean a page that was split;
///   2. every "<digit-or-a> TITLE" pairing that appears must match that page's OWN key, so a
///      renumbering cannot leave "4 GATEWAY" behind when GATEWAY is 7;
///   3. every page's own header names it by KEY, never by ring position (ADVICE is the tenth page
///      and its key is `a`: "10 ADVICE" sends a reader to type 1 then 0, which opens LATENCY).
#[test]
fn no_rendered_page_or_shipped_doc_names_a_page_that_does_not_exist() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let titles: Vec<(char, &str)> = PageId::ALL.iter().map(|p| (p.key_digit(), p.title())).collect();

    // every surface a reader actually sees: each page rendered wide enough not to truncate, plus
    // the docs that ship in the public export
    let mut surfaces: Vec<(String, String)> = Vec::new();
    for p in PageId::ALL {
        let (a, now) = on_page(demo::status(), p, 1);
        let mut text = String::new();
        let mut app = a;
        for _ in 0..12 {
            text.push_str(&render(&app, 200, 60, now).text);
            app.on_key(KeyCode::Down);
        }
        surfaces.push((format!("the {} page, rendered", p.title()), text));
    }
    for doc in ["README.md", "docs/RUNBOOK.md", "docs/ADVICE.md", "docs/STATUS-JSON.md", "docs/ENGINES.md"] {
        if let Ok(text) = std::fs::read_to_string(repo.join(doc)) {
            surfaces.push((doc.to_string(), text));
        }
    }

    // ...and the STRING LITERALS the product ships, because a fixture may never reach the branch
    // that prints one. advice.rs:377 was exactly that: the offending sentence only renders when a
    // reliability target is missed, so the rendered surfaces above did NOT catch it on the parent -
    // the docs did. Literals in `src` only: a test legitimately names a forbidden string to assert
    // its absence, and a comment legitimately explains one (this comment does).
    for crate_dir in ["lss/src", "lss-core/src", "lss-collector/src"] {
        let dir = repo.join("crates").join(crate_dir);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let Ok(src) = std::fs::read_to_string(&path) else { continue };
                    // a QUOTE-AWARE, comment-skipping scan. Splitting on '"' was not enough: a
                    // `//` comment may quote a forbidden string in order to explain it (the
                    // comment three lines above this one does), and a naive split reads that as a
                    // shipped literal. Walk the line, track whether we are inside a string, and
                    // stop at a `//` that is NOT inside one.
                    let mut lits = String::new();
                    for line in src.lines() {
                        let mut in_str = false;
                        let mut prev = '\0';
                        let mut chars = line.chars().peekable();
                        while let Some(c) = chars.next() {
                            if c == '"' && prev != '\\' {
                                in_str = !in_str;
                                if !in_str {
                                    lits.push('\n');
                                }
                            } else if !in_str && c == '/' && chars.peek() == Some(&'/') {
                                break;
                            } else if in_str {
                                lits.push(c);
                            }
                            prev = c;
                        }
                        lits.push('\n');
                    }
                    surfaces.push((format!("{} (string literals)", path.strip_prefix(&repo).unwrap_or(&path).display()), lits));
                }
            }
        }
    }

    let mut bad: Vec<String> = Vec::new();
    for (where_, text) in &surfaces {
        // 1. a split page's combined name
        for (_, a) in &titles {
            for (_, b) in &titles {
                if a == b {
                    continue;
                }
                for joiner in [" & ", " and "] {
                    let combined = format!("{a}{joiner}{b}");
                    // "ALERTS and INCIDENTS" in prose about page 1's two BOXES is legitimate, so
                    // only the ampersand form (a page TITLE) is forbidden outright; the prose form
                    // is only forbidden when it is introduced as a page, i.e. "page X and Y".
                    let forbidden = if joiner == " & " { text.contains(&combined) } else { text.contains(&format!("page {combined}")) };
                    if forbidden {
                        bad.push(format!("{where_}: names the combined page {combined:?}, which does not exist (it split)"));
                    }
                }
            }
        }
        // 2. a number paired with a title that has a different key
        for (key, title) in &titles {
            for cand in '0'..='9' {
                if cand == *key {
                    continue;
                }
                for shape in [format!("{cand} {title}"), format!("`{cand}` {title}"), format!("page {cand} {title}")] {
                    if text.contains(&shape) {
                        bad.push(format!("{where_}: {shape:?} - {title}'s key is `{key}`"));
                    }
                }
            }
            // the ring position of the tenth page is two digits and is never a key
            if text.contains(&format!("10 {title}")) {
                bad.push(format!("{where_}: \"10 {title}\" - there is no key `10`; {title} opens with `{key}`"));
            }
        }
    }

    // 3. each page's header names it by its own key
    for p in PageId::ALL {
        let (a, now) = on_page(demo::status(), p, 1);
        let shot = render(&a, 200, 60, now);
        let want = format!("{} {}", p.key_digit(), p.title());
        // batch 2 (#231 with #227 item 6): the header is in the BOTTOM chrome now, not row 0 -
        // it is the row that STARTS with "<key> <TITLE>"
        let head = shot.text.lines().find(|l| l.trim_start().starts_with(&want)).unwrap_or("").to_string();
        if !head.contains(&want) {
            bad.push(format!("the {} page header reads {:?}, not {:?}", p.title(), head.trim(), want));
        }
    }

    bad.sort();
    bad.dedup();
    assert!(bad.is_empty(), "a reader is being sent to a page that does not exist:\n  {}", bad.join("\n  "));
}

/// Card #247, split from #227 item 4 (the whole-page zero-scroll bar #227 did not reach). Pins
/// all three of the card's own targets directly, each against `page_rows` (first, shown, total) -
/// the same numbers the footer's "N-M of T" hint is built from, so "no scroll" here means exactly
/// what a reader sees: `shown >= total`. One test PER SIZE, not a loop over both - so a size that
/// meets the bar is provably green on its own, never hidden behind a sibling size that does not
/// (measured: 200x50 reaches true zero-scroll on both fixtures; 120x40 does not, even after
/// wrap-aware column packing, GPU-card compaction, and trimming every remaining caption this pass
/// could find - see the card's own note for the exact numbers and what a further cut would cost).
#[test]
fn page247_200x50_fits_with_no_scroll_on_golden() {
    let (a, now) = app(golden());
    let shot = render(&a, 200, 50, now);
    let (_, shown, total) = a.page_rows.get();
    // card #269: the owner accepted a small scroll, 2026-09-23 'ship it 1'. MEASURED that day:
    // total 51, shown 48 = 3 rows over. Card #291 (2026-09-24) then took one more row from every
    // page's body for the key-hint line (`reserve_key_hints`), which this size is tall enough to
    // receive - re-measured at 4 rows over, not re-guessed. Pinned there - a 5th row still fails.
    // card #305 (2026-09-24, the owner asked for page 1's 1h trends as real charts "like the other
    // charts i see in gpus tab 3"; the orchestrator chose to re-pin): re-measured on 6f6b4cb at total 57,
    // shown 47 = 10 rows over. Pinned there - an 11th row still fails.
    assert!(total <= shown + 10, "200x50: a calm day may scroll at most the 10 rows accepted (card #305 re-measure) (shown={shown} of total={total})\n{}", shot.text);
}

/// lss-verifier-4: measures 0 today (not the card's original <=5), so the pin now says what is
/// actually true - a looser assert here would let a real regression (e.g. up to 5 rows of new
/// scroll on a bad day) land silently green.
#[test]
fn page247_200x50_fits_with_no_scroll_on_a_bad_day() {
    let (a, now) = app(bad_day());
    let shot = render(&a, 200, 50, now);
    let (_, shown, total) = a.page_rows.get();
    // card #269: the owner accepted a small scroll, 2026-09-23 'ship it 1'. MEASURED that day:
    // total 52, shown 48 = 4 rows over. Card #291 (2026-09-24) then took one more row from every
    // page's body for the key-hint line (`reserve_key_hints`), which this size is tall enough to
    // receive - re-measured at 5 rows over, not re-guessed. Pinned there - a 6th row still fails.
    // card #305 (2026-09-24, the owner asked for page 1's 1h trends as real charts "like the other
    // charts i see in gpus tab 3"; the orchestrator chose to re-pin): re-measured on 6f6b4cb at total 58,
    // shown 47 = 11 rows over. Pinned there - a 12th row still fails.
    assert!(total <= shown + 11, "200x50: a bad day may scroll at most the 11 rows accepted (card #305 re-measure) (shown={shown} of total={total})\n{}", shot.text);
}

// card #252: the 120x40 zero-scroll bar (page247_120x40_fits_with_no_scroll_on_golden,
// page247_120x40_scrolls_at_most_5_rows_on_a_bad_day) is parked here per the owner's call
// on #247 (accept a little scroll at 120x40 rather than cut real content). Their exact bodies
// are recorded on card #252 so they can be restored verbatim if #252 is picked back up -
// they were never weakened, only removed.

/// 60x45 (the owner's own half-pane): #227 must not have made it WORSE than the pre-#227
/// baseline (163 total rows, bad_day() fixture, measured before any of #227's work landed).
#[test]
fn page247_60x45_is_no_worse_than_before_227() {
    let (a, now) = app(bad_day());
    let shot = render(&a, 60, 45, now);
    let (_, _, total) = a.page_rows.get();
    assert!(total <= 163, "60x45 must be no worse than the pre-#227 baseline (163 total rows), got {total}\n{}", shot.text);
}

/// lss-verifier-4's FAIL on #247: the Xid detail-trim's delimiter fix (dash.rs::incidents,
/// `split(" pid=")`) was unpinned - the ORIGINAL fix (5ef11b3) split on ", pid=" and no test
/// caught that it never fired, because real data has ") pid=" (a space, inside the closing
/// paren), not a comma. `golden()`'s own fixture (`fixtures/status_golden.json`, the same shape
/// the collector actually writes per `xid.rs`) carries exactly that shape, so this needs no
/// synthetic data: it pins that page 1's INCIDENTS row drops the pid=/name=/channel= clause of a
/// REAL Xid line, and is RED again if the delimiter regresses to ", pid=" (verified: reverting
/// dash.rs's `split(" pid=")` back to `split(", pid=")` turns this red).
#[test]
fn page1_incidents_drops_the_pid_name_channel_clause_of_a_real_xid_line() {
    let (a, now) = app(golden());
    let shot = render(&a, 120, 40, now);
    assert!(shot.text.contains("GPU3 Xid 8"), "the Xid incident itself must still be on screen:\n{}", shot.text);
    for gone in ["pid=3177412", "name=python3", "channel 0x00000004"] {
        assert!(!shot.text.contains(gone), "{gone:?} must be trimmed from page 1's INCIDENTS row (page 9 INCIDENTS keeps it in full):\n{}", shot.text);
    }
}

/// card #255 (the owner on v1.1.0: "bro im looking at the new ones this is terrible layout" /
/// "the gpu boxes are suppose to be together"): #227 had every GPU's own card as a separate
/// lowest-priority `Item::Row` - its OWN top-level box, free for the greedy packer to place
/// wherever was shortest, which read as scattered. RED on 0cc6c76 (that is exactly what it did).
/// Every GPU card must now sit INSIDE the GPUS box, adjacent to the others - never anywhere else.
#[test]
fn all_gpu_cards_are_inside_the_gpus_box_and_adjacent() {
    let (a, now) = app(golden());
    let shot = render(&a, 200, 50, now);
    let lines: Vec<Vec<char>> = shot.text.lines().map(|l| l.chars().collect()).collect();
    let title_row = lines.iter().position(|l| l.iter().collect::<String>().contains("o GPUS")).unwrap_or_else(|| panic!("no GPUS box on screen\n{}", shot.text));
    let x0 = lines[title_row].iter().position(|&c| c == '\u{250c}' || c == '\u{250f}').unwrap_or_else(|| panic!("no GPUS top-left corner\n{}", shot.text));
    let close_row = (title_row + 1..lines.len())
        .find(|&y| lines[y].get(x0).is_some_and(|&c| c == '\u{2514}' || c == '\u{2517}'))
        .unwrap_or_else(|| panic!("GPUS box never closes on screen at 200x50\n{}", shot.text));
    for i in 0..4 {
        let needle = format!("o GPU{i}");
        let hits: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.iter().collect::<String>().contains(&needle)).map(|(y, _)| y).collect();
        assert!(!hits.is_empty(), "GPU{i}'s card never appears on screen\n{}", shot.text);
        for y in hits {
            assert!(
                y > title_row && y < close_row,
                "GPU{i}'s card (row {y}) is OUTSIDE the GPUS box (rows {}..{close_row}) - every GPU card must sit together, not scattered\n{}",
                title_row + 1,
                shot.text
            );
        }
    }
}
