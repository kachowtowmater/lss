//! The live screen. Pure rendering: `draw` takes the app state and a clock and paints a frame,
//! so every layout and page is testable with ratatui's `TestBackend`.
//!
//! One main view (the OVERVIEW: SERVE, GPUS, LANES, USERS, ADVICE, INCIDENTS, ALERTS as boxes,
//! key `0`) and ten full-screen detail pages: 1 LATENCY, 2 LOAD, 3 GPUS (the Grafana SGLang /
//! NVIDIA DCGM dashboards), 4 USERS, 5 TOKENS, 6 MODEL (who uses it, how much, how good the
//! model is on this hardware), 7 GATEWAY, 8 ALERTS, 9 INCIDENTS (Prometheus / Alertmanager,
//! split in two by card #231), key `a` ADVICE - `0` was already the overview's key, so ADVICE,
//! the tenth page, took the next free one instead of displacing it.
//! The visual language is Terminal Board's - see `widgets`.

pub mod dash;
pub mod fleet;
pub mod overview;
pub mod pages;
pub mod watch;
pub mod widgets;

use crate::data::{range_label, PageData, PageId, DEFAULT_RANGE};
use lss_core::model::Status;
use lss_core::series::RANGES;
use lss_core::timeutil::{fmt_duration, fmt_local};
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use std::cell::{Cell, RefCell};
use widgets::{bold, dim, draw_box, fit, frame, red, spans_len};

pub use widgets::sparkline;

/// The named layouts of the overview, picked from the pane shape on every frame (or pinned
/// with `L`). Same names and thresholds as Terminal Board, plus SMALL for the panes that are
/// too low for a full layout but still deserve boxes (63x11, 63x12, 63x24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// wide, >= 30 rows (126x41): SERVE | GPUS over LANES | INCIDENTS, ALERTS strip below
    HalfH,
    /// 63-100 columns and tall (70x70): the boxes stacked
    HalfV,
    /// wide, < 30 rows (126x22, 94x20, 200x24): three full-height columns, SERVE | GPUS over
    /// ADVICE | LANES over USERS, with INCIDENTS and ALERTS as the row under them
    ThirdH,
    /// narrow and tall (42x73, 50x70, 63x60): the boxes stacked, each as tall as its content,
    /// one compact row per GPU; rows left over become last-hour charts
    ThirdV,
    /// everything else that can hold a box row: SERVE | GPUS, the rest as boxes or 1-line bars
    Small,
    /// minimized (40x8, 50x10, 63x11): ONE status card that answers "is it OK?", the rest as bars
    Focus,
}

impl Shape {
    pub const ALL: [Shape; 6] = [Shape::HalfH, Shape::HalfV, Shape::ThirdH, Shape::ThirdV, Shape::Small, Shape::Focus];

    pub fn name(self) -> &'static str {
        match self {
            Shape::HalfH => "half-h",
            Shape::HalfV => "half-v",
            Shape::ThirdH => "third-h",
            Shape::ThirdV => "third-v",
            Shape::Small => "small",
            Shape::Focus => "focus",
        }
    }

    pub fn from_name(name: &str) -> Option<Shape> {
        Self::ALL.iter().copied().find(|s| s.name() == name)
    }
}

/// Terminal cells are ~2.2x taller than wide, so a pane is "tall" when `cols < rows * 2.2`.
pub fn pick_shape(w: u16, h: u16) -> Shape {
    let tall = f32::from(w) < f32::from(h) * 2.2;
    if h <= 12 || w < 30 {
        // a corner of the screen: there is room for one card, so it is the one that matters
        Shape::Focus
    } else if tall && h >= 30 {
        if w <= 66 {
            Shape::ThirdV
        } else {
            Shape::HalfV
        }
    } else if !tall && h >= 30 && w >= 80 {
        Shape::HalfH
    } else if !tall && ((h >= 16 && w >= 100) || (h >= 18 && w >= 90)) {
        Shape::ThirdH
    } else {
        Shape::Small
    }
}

/// The areas of the overview. Each opens a detail page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Serve,
    Gpus,
    Lanes,
    Users,
    Advice,
    Incidents,
    Alerts,
}

impl Panel {
    /// Top to bottom, and the order in which areas become boxes when rows are short.
    pub const ALL: [Panel; 7] = [Panel::Serve, Panel::Gpus, Panel::Lanes, Panel::Users, Panel::Advice, Panel::Incidents, Panel::Alerts];

    pub fn title(self) -> &'static str {
        match self {
            Panel::Serve => "SERVE",
            Panel::Gpus => "GPUS",
            Panel::Lanes => "LANES",
            Panel::Users => "USERS",
            Panel::Advice => "ADVICE",
            Panel::Incidents => "INCIDENTS",
            Panel::Alerts => "ALERTS",
        }
    }

    /// The page Enter opens from this box (card #222: Tab no longer does - it cycles pages).
    pub fn page(self) -> PageId {
        match self {
            Panel::Serve => PageId::Latency,
            Panel::Gpus => PageId::Gpus,
            Panel::Lanes => PageId::Gateway,
            Panel::Users => PageId::Users,
            Panel::Advice => PageId::Advice,
            // #231: the classic grid's own two boxes now open their own pages, same as the
            // split everywhere else - no more collapsing both into the combined page 8.
            Panel::Alerts => PageId::Alerts,
            Panel::Incidents => PageId::Incidents,
        }
    }
}

pub fn page_colour(page: PageId) -> Color {
    match page {
        PageId::Latency => Color::Blue,
        PageId::Load => Color::Cyan,
        PageId::Gpus => Color::Green,
        PageId::Users => Color::LightBlue,
        PageId::Tokens => Color::LightGreen,
        PageId::Model => Color::LightMagenta,
        // #227 item 5 (the owner: "the amber drift in page_colour") - Yellow is `warn()`, so this
        // page's own border/title read as a standing warning even on a calm day. Blue, the same
        // calm colour LOADOUT already uses (`loadout_colour`) - both are "what is configured"
        // pages, and colour reuse across page1 boxes and detail pages is already how this palette
        // works (Alerts/Incidents already share Magenta).
        PageId::Gateway => Color::Blue,
        PageId::Alerts => Color::Magenta,
        PageId::Incidents => Color::Magenta,
        PageId::Advice => Color::LightCyan,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Page(PageId),
    /// #92: one row per `[[server]]` node - only reachable with `f` when `multi_server()` (a
    /// fleet of one stays exactly as it was: no new key, no new page, per the card's own rule).
    Fleet,
    /// #74: what the sources this owner follows have released, and when (`w`) - always
    /// reachable, honestly empty when `[watch] path` is not configured.
    Watch,
}

/// Something the screen asks the main loop to DO (the screen itself only draws).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `POST /bench` quick: only offered when the collector is on this machine
    StartBench,
    /// `[` / `]`: the screen now looks at another server of `[[server]]`
    SwitchServer,
}

/// One server of the fleet strip: what its collector last said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetEntry {
    pub name: String,
    pub url: String,
    /// None = not asked yet; Some(Err) = its collector cannot be reached
    pub state: Option<Result<FleetState, String>>,
}

/// #92: the FLEET page's own row for one node - a small, deliberately narrow projection of that
/// node's `/status` (never the whole `Status`: this is refetched every 5 s per node, on its own
/// thread, so a slow node never delays the others - see `main.rs`'s `spawn_fleet`). Heterogeneous
/// by design: `not_reported`/`engine` are carried so the FLEET page can print the SAME
/// "n/a (not reported by X)" sentence `ServeStatus::na` already gives every single-node page,
/// never a fabricated 0 for a field an engine (llama.cpp, Ollama, ...) does not publish.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetState {
    pub up: bool,
    /// the collector's own reported host, distinct from the `[[server]]` config's own `name`
    /// (`FleetEntry.name` - what the OWNER chose to call it, shown everywhere else already)
    pub host: String,
    pub engine: String,
    pub model: Option<String>,
    pub decode_tok_s: f64,
    pub prefill_tok_s: Option<f64>,
    pub running: f64,
    pub slots: u32,
    pub kv_usage: f64,
    pub gpu_count: usize,
    pub total_watts: Option<f64>,
    /// #75/#73's live cost figure, straight from that node's OWN rate table - `None` when that
    /// node has no `[rates] path` configured, never a guessed number, and never averaged with a
    /// DIFFERENT node's rate (each node's electricity is its own real number, priced by its own
    /// config - see `fleet.rs`'s totals for why they still sum honestly).
    pub cost_per_hour: Option<f64>,
    /// all-time generated tokens (already on every `/status`) - the one throughput figure that
    /// DOES sum meaningfully across a heterogeneous fleet (unlike a decode tok/s average).
    pub generated_tokens_total: f64,
    pub not_reported: Vec<String>,
}

#[derive(Debug)]
pub struct App {
    pub url: String,
    pub status: Option<Status>,
    /// Some = the last fetch failed; `status` (if any) is stale.
    pub error: Option<String>,
    /// local clock at the last successful fetch
    pub last_ok: Option<i64>,
    /// None = automatic
    pub pinned: Option<Shape>,
    pub show_help: bool,
    /// #247 (lss-verifier-4's FAIL): the `?` overlay is a FIXED box - at 120x40 "Reading it" (the
    /// captions #227/#247 moved off page 1) sits below the cut with no way to reach it. First
    /// visible line of the overlay, same up/down/pgup/pgdn keys as a page; reset to 0 each time
    /// `?` opens it.
    pub help_scroll: usize,
    /// (first line, lines shown, lines in total) of the help overlay, as drawn last frame - same
    /// shape and purpose as `page_rows`, kept separate so opening help never disturbs the scroll
    /// position of the page underneath it.
    pub help_rows: Cell<(usize, usize, usize)>,
    /// card #258: the header row `draw_chrome_bottom` drew THIS frame, redrawn by `draw` once the
    /// page is laid out - the scroll position it carries is only known after the layout.
    pub chrome_top: Cell<Option<Rect>>,
    /// painted fg/bg: false = dark (white on black), true = light
    pub light: bool,
    /// multi-series charts (GPUS, USERS): false = braille dots, true = connected colour lines
    /// (card #48). Default false - the owner opts in with `c`, never opted in for him. Resolved
    /// at startup by `prefs::resolve_chart_lines` ($LSS_CHART > the saved pref > this default).
    pub chart_lines: bool,
    /// the overview drawn: false = the original grid, true = the #51/#73 redesign (cost +
    /// loadout), which `v` toggles. Card #191, 2026-09-22: the DEFAULT is now `true`. The owner
    /// was shown both pages side by side and picked the new one, but that choice lived only in
    /// his own git-ignored `~/.config/lss/ui.json`, so every stranger who installed lss landed
    /// on the page he had rejected. The opt-in era is over: this is the landing page, and `v`
    /// is the way BACK to the classic grid. No env override (never asked for); the saved
    /// `page1` pref still wins over this default, and an absent/empty pref means this default
    /// (see `prefs::Prefs::apply`).
    pub dash: bool,
    pub view: View,
    pub focus: Panel,
    /// index into `lss_core::series::RANGES`
    pub range_idx: usize,
    /// first visible section row of the open page
    pub scroll: usize,
    pub page_data: PageData,
    /// where the overview's areas were drawn last frame (spatial arrow navigation)
    pub rects: RefCell<Vec<(Panel, Rect)>>,
    /// card #179: the key of the reading page 1's worst-now line led with on the last frame, fed
    /// back to `readings::leading` so its hysteresis margin actually applies on screen
    pub worst_key: RefCell<Option<String>>,
    /// (first row, rows shown, rows in total) of the open page, as drawn last frame
    pub page_rows: Cell<(usize, usize, usize)>,
    /// #231: page 1's own box -> page row ranges, as drawn last frame - `[(PageId, y_start,
    /// y_end))]` for the boxes that open a page on Enter (ALERTS, INCIDENTS). Page 1 has no
    /// focus/pin concept (#51), so Enter opens whichever of these the CURRENT SCROLL POSITION
    /// (`page_rows`, same row coordinate space) sits inside, rather than a tracked selection.
    pub dash_page_rows: RefCell<Vec<(PageId, u16, u16)>>,
    /// USERS: how the table is sorted (`s` cycles)
    pub user_sort: lss_core::users::UserSort,
    /// MODEL: the side-by-side comparison is open (enter), against this loadout of the list
    pub compare_open: bool,
    pub compare_sel: usize,
    /// loadouts in the list, as drawn last frame (so the selection can wrap)
    pub compare_len: Cell<usize>,
    /// MODEL: the BENCH prompt is open (`b`)
    pub bench_prompt: bool,
    /// the collector is on this machine, so `b` may start a benchmark; else it shows `bench_command`
    pub bench_local: bool,
    pub bench_command: String,
    /// what the collector answered to the last `b`
    pub bench_message: Option<String>,
    pub action: Option<Action>,
    /// every server of `[[server]]` (empty or one = a single-server setup: nothing extra is drawn)
    pub fleet: Vec<FleetEntry>,
    /// which of them the screen is looking at
    pub server_idx: usize,
    /// #74: per WATCH source name -> the newest item's own date last SEEN (persisted, so a NEW
    /// badge survives closing lss) - updated the moment the page is opened, from whatever was
    /// there before that moment (see `watch_new`, the snapshot of what WAS new at that moment).
    pub watch_seen: std::collections::BTreeMap<String, String>,
    /// source names that were new AT THE MOMENT the WATCH page was last opened - a stable
    /// snapshot for this viewing, not recomputed live against `watch_seen` (which updates
    /// immediately on open) - session-only, never persisted.
    pub watch_new: Vec<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            url: String::new(),
            status: None,
            error: None,
            last_ok: None,
            pinned: None,
            show_help: false,
            help_scroll: 0,
            help_rows: Cell::new((0, 0, 0)),
            chrome_top: Cell::new(None),
            light: false,
            chart_lines: false,
            // #191: the new (cost + loadout) page 1 is what a fresh install lands on.
            dash: true,
            view: View::Overview,
            focus: Panel::Serve,
            range_idx: DEFAULT_RANGE,
            scroll: 0,
            page_data: PageData::default(),
            rects: RefCell::default(),
            worst_key: RefCell::default(),
            page_rows: Cell::new((0, 0, 0)),
            dash_page_rows: RefCell::default(),
            user_sort: Default::default(),
            compare_open: false,
            compare_sel: 1,
            compare_len: Cell::new(0),
            bench_prompt: false,
            bench_local: false,
            bench_command: "lss bench quick".into(),
            bench_message: None,
            action: None,
            fleet: Vec::new(),
            server_idx: 0,
            watch_seen: std::collections::BTreeMap::new(),
            watch_new: Vec::new(),
        }
    }
}

impl App {
    pub fn multi_server(&self) -> bool {
        self.fleet.len() > 1
    }

    /// `[` / `]`: the previous / next server. Everything on screen belongs to the old one, so
    /// it is dropped; the main loop points the fetcher at the new URL.
    pub fn switch_server(&mut self, step: isize) {
        if !self.multi_server() {
            return;
        }
        let n = self.fleet.len() as isize;
        self.server_idx = (self.server_idx as isize + step).rem_euclid(n) as usize;
        self.url = self.fleet[self.server_idx].url.clone();
        self.status = None;
        self.error = None;
        self.last_ok = None;
        self.page_data = PageData::default();
        self.bench_message = None;
        self.action = Some(Action::SwitchServer);
    }

    /// `L`: auto > half-h > half-v > third-h > third-v > small > focus > auto
    pub fn cycle_layout(&mut self) {
        self.pinned = match self.pinned {
            None => Some(Shape::ALL[0]),
            Some(s) => Shape::ALL.iter().position(|x| *x == s).and_then(|i| Shape::ALL.get(i + 1)).copied(),
        };
    }

    pub fn open(&mut self, page: PageId) {
        if self.view != View::Page(page) {
            self.scroll = 0;
            self.compare_open = false;
            self.bench_prompt = false;
        }
        self.view = View::Page(page);
    }

    /// `0` / `esc`: back to the overview, from anywhere.
    fn back_to_overview(&mut self) {
        self.view = View::Overview;
        self.scroll = 0;
    }

    /// Tab: step forward through the 11-stop ring - the overview, then all ten `PageId::ALL`
    /// pages in order, wrapping at ADVICE back to the overview (card #231 correction, the owner
    /// via the orchestrator: "page 0 has no tab thats why tab wasnt working ... So the Tab ring MUST
    /// include the overview: 0 -> 1 -> … -> last -> 0"). `PageId::next()` alone cannot express
    /// this: it only wraps within the 10 pages themselves, and the overview is not a `PageId`.
    pub fn open_next_in_ring(&mut self) {
        match self.view {
            View::Overview => self.open(PageId::ALL[0]),
            View::Page(p) if p == *PageId::ALL.last().unwrap() => self.back_to_overview(),
            View::Page(p) => self.open(p.next()),
            _ => {}
        }
    }

    /// Shift-Tab: the same ring, reversed.
    pub fn open_prev_in_ring(&mut self) {
        match self.view {
            View::Overview => self.open(PageId::ALL[PageId::ALL.len() - 1]),
            View::Page(p) if p == PageId::ALL[0] => self.back_to_overview(),
            View::Page(p) => self.open(PageId::ALL[(p.number() + PageId::ALL.len() - 2) % PageId::ALL.len()]),
            _ => {}
        }
    }

    /// What the fetch thread should be loading: (page, range).
    pub fn wanted(&self) -> Option<(PageId, usize)> {
        match self.view {
            View::Page(p) => Some((p, self.range_idx)),
            View::Overview | View::Fleet | View::Watch => None,
        }
    }

    /// Arrow keys on the overview: the nearest area in that direction, by where they were drawn.
    fn move_focus(&mut self, dx: i32, dy: i32) {
        let rects = self.rects.borrow();
        let Some((_, from)) = rects.iter().find(|(p, _)| *p == self.focus).copied() else {
            if let Some((p, _)) = rects.first() {
                self.focus = *p;
            }
            return;
        };
        let centre = |r: &Rect| (i32::from(r.x) * 2 + i32::from(r.width), i32::from(r.y) * 2 + i32::from(r.height));
        let (fx, fy) = centre(&from);
        let best = rects
            .iter()
            .filter(|(p, _)| *p != self.focus)
            .filter_map(|(p, r)| {
                let (cx, cy) = centre(r);
                let (along, across) = if dx != 0 { ((cx - fx) * dx, (cy - fy).abs()) } else { ((cy - fy) * dy, (cx - fx).abs()) };
                // it must lie in that direction, and overlap the row/column we are moving along
                let overlaps = if dx != 0 { r.y < from.y + from.height && from.y < r.y + r.height } else { r.x < from.x + from.width && from.x < r.x + r.width };
                (along > 0 && overlaps).then_some((along * 4 + across, *p))
            })
            .min_by_key(|(d, _)| *d);
        if let Some((_, p)) = best {
            self.focus = p;
        }
    }

    /// Returns true when the key means "quit".
    pub fn on_key(&mut self, code: KeyCode) -> bool {
        if self.show_help {
            // #247: scrollable, same keys as a page - a closing key still closes it and does
            // nothing else, everything else moves `help_scroll`, clamped against `help_rows`
            // (set by the last `draw_help`, same first/shown/total shape as `page_rows`).
            match code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => self.show_help = false,
                KeyCode::Up => self.help_scroll = self.help_rows.get().0.saturating_sub(1),
                KeyCode::Down => {
                    let (first, _, total) = self.help_rows.get();
                    self.help_scroll = (first + 1).min(total.saturating_sub(1));
                }
                KeyCode::PageUp => {
                    let (first, shown, _) = self.help_rows.get();
                    self.help_scroll = first.saturating_sub(shown.max(1));
                }
                KeyCode::PageDown => {
                    let (first, shown, total) = self.help_rows.get();
                    self.help_scroll = (first + shown.max(1)).min(total.saturating_sub(1));
                }
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = self.help_rows.get().2.saturating_sub(1),
                _ => {}
            }
            return false;
        }
        if self.bench_prompt {
            // the BENCH prompt: y / enter starts it (when it can be started from here), anything
            // that closes a dialog closes it
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter if self.bench_local => {
                    self.action = Some(Action::StartBench);
                    self.bench_prompt = false;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('b') | KeyCode::Char('q') | KeyCode::Enter => self.bench_prompt = false,
                _ => {}
            }
            return false;
        }
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            KeyCode::Char('T') | KeyCode::Char('t') => self.light = !self.light,
            KeyCode::Char('L') | KeyCode::Char('l') => self.cycle_layout(),
            // #48, 2026-09-21: the owner wants to look with his own eyes - a live toggle, not
            // only an env/prefs setting. `main.rs` checks ctrl-c separately before this, so
            // plain `c`/`C` (no modifier) always reaches here.
            KeyCode::Char('c') | KeyCode::Char('C') => self.chart_lines = !self.chart_lines,
            // #51: the redesigned overview (page 1) - global like T/L/c (L, too, only visibly
            // changes the overview but works from anywhere). #191: it is now the DEFAULT, so
            // this key is the way back to the classic grid and back again; the toggle itself is
            // unchanged. Reset scroll: the two overviews are different shapes, an old scroll
            // position from one means nothing in the other.
            KeyCode::Char('v') | KeyCode::Char('V') => {
                self.dash = !self.dash;
                self.scroll = 0;
            }
            KeyCode::Char('[') => self.switch_server(-1),
            KeyCode::Char(']') => self.switch_server(1),
            // #92: only with more than one `[[server]]` - a fleet of one gets no new key, per
            // the card's own rule that it must look exactly like today.
            KeyCode::Char('f') | KeyCode::Char('F') if self.multi_server() => {
                self.view = View::Fleet;
                self.scroll = 0;
            }
            // #74: always reachable (unlike FLEET, WATCH is useful with one server too) - a
            // stable NEW-badge snapshot decided the moment the page opens (see `watch_new`'s own
            // doc comment), then `watch_seen` updates right away so the NEXT open only shows
            // what changed since THIS one.
            KeyCode::Char('w') | KeyCode::Char('W') => {
                if let Some(w) = self.status.as_ref().and_then(|s| s.watch.as_ref()) {
                    self.watch_new = w
                        .sources
                        .iter()
                        .filter(|src| src.newest.as_ref().is_some_and(|item| self.watch_seen.get(&src.name).is_none_or(|seen| item.date.as_str() > seen.as_str())))
                        .map(|src| src.name.clone())
                        .collect();
                    for src in &w.sources {
                        if let Some(item) = &src.newest {
                            self.watch_seen.insert(src.name.clone(), item.date.clone());
                        }
                    }
                } else {
                    self.watch_new.clear();
                }
                self.view = View::Watch;
                self.scroll = 0;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => self.range_idx = (self.range_idx + 1) % RANGES.len(),
            KeyCode::Char('s') | KeyCode::Char('S') if self.view == View::Page(PageId::Users) => self.user_sort = self.user_sort.next(),
            KeyCode::Char('b') | KeyCode::Char('B') if self.view == View::Page(PageId::Model) => {
                self.bench_prompt = true;
                self.bench_message = None;
            }
            KeyCode::Esc if self.compare_open => self.compare_open = false,
            // #92: "drill into a node" - `[`/`]` (unguarded, works from anywhere already) picks
            // which node is current; enter here just leaves FLEET for that node's own overview,
            // reusing every single-server page unchanged, per the card's own rule.
            KeyCode::Enter if self.view == View::Fleet => {
                self.view = View::Overview;
                self.scroll = 0;
            }
            // #231 correction (the orchestrator, reading the owner: "page 0 has no tab thats why tab
            // wasnt working ... Keep 0 = overview"): `0` stays the overview's key, same as
            // `esc`. ADVICE - the tenth page, once 1-9 ran out of digits - gets the next free
            // key, `a`, instead.
            KeyCode::Char('0') => self.back_to_overview(),
            KeyCode::Char('a') | KeyCode::Char('A') => self.open(PageId::Advice),
            KeyCode::Char(c @ '1'..='9') => {
                if let Some(p) = PageId::from_digit(c) {
                    self.open(p);
                }
            }
            KeyCode::Esc => self.back_to_overview(),
            _ => match self.view {
                // #51: the redesigned overview scrolls like a detail page (it is one column of
                // boxed sections, not the old shape-adaptive grid) - same up/down/pgup/pgdn.
                View::Overview if self.dash => {
                    let (first, shown, total) = self.page_rows.get();
                    let last = total.saturating_sub(1);
                    match code {
                        KeyCode::Up => self.scroll = first.saturating_sub(1),
                        // card #270: at the bottom the VIEW stops (the draw clamps to
                        // `total - shown`) but the reading row keeps going down to the last row,
                        // so a box that starts below the last scroll stop (INCIDENTS at 140x41:
                        // rows 25.., max scroll 20) is still reachable for Enter. Nothing on
                        // screen changes; Up moves the view again at once, as before.
                        KeyCode::Down => self.scroll = (self.scroll.max(first) + 1).min(last),
                        KeyCode::PageUp => self.scroll = first.saturating_sub(shown.max(1)),
                        KeyCode::PageDown => self.scroll = (self.scroll.max(first) + shown.max(1)).min(last),
                        KeyCode::Home => self.scroll = 0,
                        KeyCode::End => self.scroll = last,
                        // #222 (the owner: "include the tab feature that lets us go through all
                        // the pages"): page 1 had NO Tab handling at all before this - the key did
                        // nothing. #231 correction: the ring now includes the overview itself
                        // (`open_next_in_ring`/`open_prev_in_ring`), so BackTab from here wraps
                        // to ADVICE, the ring's other neighbour of the overview.
                        KeyCode::Tab => self.open_next_in_ring(),
                        KeyCode::BackTab => self.open_prev_in_ring(),
                        // #231 (the owner: "i still want alerts and incidents on the front page.
                        // but also on the bigger pages"): page 1 has no focus/pin concept (#51),
                        // so Enter opens whichever of ALERTS/INCIDENTS the scroll position (the
                        // top row on screen, same coordinate space `page_rows` already reports)
                        // currently sits inside - the box the reader is actually looking at.
                        // Scrolled to something else (LOADOUT, ELECTRICITY, ...): Enter does
                        // nothing, same as every other key page 1 does not recognise.
                        KeyCode::Enter => {
                            // card #270: the reading row - the top row on screen, or past it when
                            // Down went on at the bottom (see Down); `first` alone never reached a
                            // box starting below the last scroll stop.
                            let row = self.scroll.max(first).min(last) as u16;
                            // the borrow must end before `open` (which needs `&mut self`) starts.
                            let page = self.dash_page_rows.borrow().iter().find(|(_, y0, y1)| *y0 <= row && row < *y1).map(|(p, ..)| *p);
                            if let Some(page) = page {
                                self.open(page);
                            }
                        }
                        _ => {}
                    }
                }
                View::Overview => match code {
                    KeyCode::Left => self.move_focus(-1, 0),
                    KeyCode::Right => self.move_focus(1, 0),
                    KeyCode::Up => self.move_focus(0, -1),
                    KeyCode::Down => self.move_focus(0, 1),
                    // #222: Tab used to open the focused box here, the one place in the whole
                    // screen where it did not mean "next page" - Enter alone keeps that job
                    // (arrows still move the focus first, same as always); Tab/BackTab now enter
                    // the ring exactly as they do from page 1 and every detail page.
                    KeyCode::Enter => self.open(self.focus.page()),
                    KeyCode::Tab => self.open_next_in_ring(),
                    KeyCode::BackTab => self.open_prev_in_ring(),
                    _ => {}
                },
                // #92/#74: FLEET and WATCH are each one flat table, same up/down/pgup/pgdn
                // scroll as a detail page.
                View::Fleet | View::Watch => {
                    let (first, shown, total) = self.page_rows.get();
                    let last = total.saturating_sub(1);
                    match code {
                        KeyCode::Up => self.scroll = first.saturating_sub(1),
                        KeyCode::Down => self.scroll = (first + 1).min(last),
                        KeyCode::PageUp => self.scroll = first.saturating_sub(shown.max(1)),
                        KeyCode::PageDown => self.scroll = (first + shown.max(1)).min(last),
                        KeyCode::Home => self.scroll = 0,
                        KeyCode::End => self.scroll = last,
                        _ => {}
                    }
                }
                View::Page(PageId::Model) if self.compare_open || code == KeyCode::Enter => {
                    // MODEL: enter opens the comparison; up / down pick the loadout to compare with
                    let n = self.compare_len.get().max(1);
                    match code {
                        KeyCode::Enter => self.compare_open = !self.compare_open,
                        KeyCode::Up => self.compare_sel = (self.compare_sel + n - 1) % n,
                        KeyCode::Down => self.compare_sel = (self.compare_sel + 1) % n,
                        KeyCode::Tab | KeyCode::Right => self.open(PageId::Model.next()),
                        KeyCode::BackTab | KeyCode::Left => self.open(PageId::Tokens),
                        _ => {}
                    }
                }
                View::Page(_) => {
                    // move from what is ON SCREEN (the layout never scrolls past the last full screen)
                    let (first, shown, total) = self.page_rows.get();
                    let last = total.saturating_sub(1);
                    match code {
                        KeyCode::Up => self.scroll = first.saturating_sub(1),
                        KeyCode::Down => self.scroll = (first + 1).min(last),
                        KeyCode::PageUp => self.scroll = first.saturating_sub(shown.max(1)),
                        KeyCode::PageDown => self.scroll = (first + shown.max(1)).min(last),
                        KeyCode::Home => self.scroll = 0,
                        KeyCode::End => self.scroll = last,
                        // #231 correction: the ring now includes the overview, so Tab from
                        // ADVICE (the last page) and BackTab from LATENCY (the first) each land
                        // there instead of wrapping straight to the other end.
                        KeyCode::Tab | KeyCode::Right => self.open_next_in_ring(),
                        KeyCode::BackTab | KeyCode::Left => self.open_prev_in_ring(),
                        _ => {}
                    }
                }
            },
        }
        false
    }
}

pub struct Palette {
    pub fg: Color,
    pub bg: Color,
}

/// The whole UI is one foreground colour on one background colour, painted, so the terminal's
/// own theme never shows through.
pub fn palette(light: bool) -> Palette {
    if light {
        Palette { fg: Color::Black, bg: Color::White }
    } else {
        Palette { fg: Color::White, bg: Color::Black }
    }
}

fn base_style(app: &App) -> Style {
    let p = palette(app.light);
    Style::default().fg(p.fg).bg(p.bg)
}

pub fn draw(f: &mut Frame, app: &App, now: i64) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Block::default().style(base_style(app)), area);
    app.rects.borrow_mut().clear();
    match (&app.status, app.view) {
        // FLEET reads app.fleet (each node's own background-fetched state), never the current
        // server's app.status - it must draw even before that first fetch completes.
        (_, View::Fleet) => fleet::draw(f, app, area, now),
        (None, _) => draw_no_data(f, app, area, now),
        (Some(s), View::Overview) if app.dash => dash::draw(f, app, s, area, now),
        (Some(s), View::Overview) => draw_classic_with_key_hints(f, app, s, area, now),
        (Some(s), View::Page(p)) => pages::draw(f, app, s, p, area, now),
        (Some(s), View::Watch) => watch::draw(f, app, s, area, now),
    }
    if let (Some(top), Some(s)) = (app.chrome_top.take(), app.status.as_ref()) {
        f.render_widget(Block::default().style(base_style(app)), top);
        f.render_widget(Paragraph::new(header(app, s, top.width, now)), top);
    }
    if app.bench_prompt {
        draw_bench_prompt(f, app);
    }
    if app.show_help {
        draw_help(f, app);
    }
}

/// `b` on the MODEL page: what a benchmark does, and (when the collector is on this machine)
/// the key that starts it; from anywhere else, the exact command to run.
fn draw_bench_prompt(f: &mut Frame, app: &App) {
    let lines = pages::bench_prompt_lines(app);
    let area = f.area();
    let w = (lines.iter().map(|l| l.width()).max().unwrap_or(40) as u16 + 4).min(area.width.saturating_sub(2));
    let h = (lines.len() as u16 + 2).min(area.height);
    if w < 24 || h < 5 {
        return;
    }
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    f.render_widget(Block::default().style(base_style(app)), rect);
    let block = frame(true, page_colour(PageId::Model)).title(Span::styled(" o BENCH ", bold().fg(page_colour(PageId::Model))));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
}

/// "collector unreachable" wording, shared by every view.
pub fn unreachable_text(app: &App, now: i64) -> Option<String> {
    let err = app.error.as_ref()?;
    let seen = match app.last_ok {
        Some(t) => format!("last seen {} ({} ago)", fmt_local(t, "%H:%M:%S"), fmt_duration(now - t)),
        None => "never reached since lss started".to_string(),
    };
    let stale = if app.status.is_some() { " - showing the last data" } else { "" };
    if let Some(why) = err.strip_prefix(crate::client::WAITING) {
        return Some(format!("NO LLM SERVER YET - {why}"));
    }
    Some(format!("COLLECTOR UNREACHABLE - {seen}{stale} - {err}"))
}

/// No data at all: still a box, red when the collector cannot be reached.
fn draw_no_data(f: &mut Frame, app: &App, area: Rect, now: i64) {
    let problem = unreachable_text(app, now);
    if area.height < 3 || area.width < 12 {
        let text = problem.unwrap_or_else(|| format!("connecting to {} ...", app.url));
        return f.render_widget(Paragraph::new(Line::styled(fit(&text, area.width as usize), if app.error.is_some() { red() } else { dim() })), area);
    }
    let head = Rect { height: 1, ..area };
    let title = if app.multi_server() { format!(" LLM SERVER STATUS · {} · [{}/{}]", app.fleet[app.server_idx.min(app.fleet.len() - 1)].name, app.server_idx + 1, app.fleet.len()) } else { " LLM SERVER STATUS".to_string() };
    f.render_widget(Paragraph::new(Line::styled(fit(&title, area.width as usize), bold())), head);
    let mut body = Rect { y: area.y + 1, height: area.height - 1, ..area };
    if let (Some(line), true) = (fleet_line(app, area.width), body.height >= 5) {
        f.render_widget(Paragraph::new(line), Rect { height: 1, ..body });
        body = Rect { y: body.y + 1, height: body.height - 1, ..body };
    }
    if body.height < 3 {
        return;
    }
    // card #180 gate 6: a collector that is up and still looking for an LLM server is not a
    // broken monitor - its own title, not red, and the hint names the step that fixes it
    let waiting = app.error.as_deref().is_some_and(crate::client::is_waiting);
    let (title, colour) = if waiting { ("NO LLM SERVER YET", Color::Yellow) } else if problem.is_some() { ("COLLECTOR UNREACHABLE", widgets::RED) } else { ("CONNECTING", Color::Blue) };
    let inner = draw_box(f, body, title, vec![], colour, true);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    match problem {
        Some(t) if waiting => {
            lines.extend(widgets::wrap(&t, w).into_iter().map(Line::raw));
            lines.extend(widgets::wrap(&format!("collector: {}  - checking again every 2 s - q quits", app.url), w).into_iter().map(|l| Line::styled(l, dim())));
        }
        Some(t) => {
            lines.extend(widgets::wrap(&t, w).into_iter().map(|l| Line::styled(l, red())));
            lines.extend(widgets::wrap(&format!("collector: {}  (set LSS_URL to change) - retrying every 2 s - q quits", app.url), w).into_iter().map(Line::raw));
            lines.extend(widgets::wrap("on the GPU box: systemctl --user status lss-collector", w).into_iter().map(|l| Line::styled(l, dim())));
        }
        None => lines.push(Line::styled(fit(&format!("connecting to {} ...", app.url), w), dim())),
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// ` LLM SERVER STATUS · gpu-box · model-a · UP 9h42m · restarts today 0      refreshed 14:18:22`
/// On a page the name and the time range lead. Shortened, never cut mid-word, when narrow.
pub fn header(app: &App, s: &Status, width: u16, now: i64) -> Line<'static> {
    let w = width as usize;
    let model = s.serve.model.clone().unwrap_or_else(|| "(no model)".into());
    let state = crate::plain::serve_state(s, now);
    let lead = match app.view {
        View::Overview => "LLM SERVER STATUS".to_string(),
        // card #231 (lss-verifier-4): the KEY, not the ring position. ADVICE is the tenth page
        // and its key is `a`, so number() printed "10 ADVICE" while the tab strip said "a advice"
        // - and a reader who types 1 then 0 gets LATENCY.
        View::Page(p) => format!("{} {} · last {}", p.key_digit(), p.title(), range_label(app.range_idx)),
        View::Fleet => "FLEET".to_string(),
        View::Watch => "WATCH".to_string(),
    };
    let short_lead = match app.view {
        View::Overview => "LSS".to_string(),
        View::Page(p) => format!("{} {} · {}", p.key_digit(), p.title(), range_label(app.range_idx)),
        View::Fleet => "FLEET".to_string(),
        View::Watch => "WATCH".to_string(),
    };
    let restarts = format!("restarts today {}", s.serve.restarts_today);
    // with several servers configured, the name given in lss.toml is what the owner knows it by
    let host = if app.multi_server() { app.fleet[app.server_idx.min(app.fleet.len() - 1)].name.clone() } else { s.host.clone() };
    let mut tail: Vec<Span<'static>> = Vec::new();
    // card #258 (the owner, 15:03: "move the llm server status back up to where you had it
    // before"): this is the ONE chrome row at the top of every page. What used to be a second,
    // loud row (collector unreachable / stalled) rides in it, first and in red, so the top is
    // exactly one line.
    if let Some(err) = app.error.as_ref() {
        let seen = app.last_ok.map_or_else(|| "never reached".to_string(), |t| format!("last seen {} ago", fmt_duration(now - t)));
        let text = if crate::client::is_waiting(err) { "NO LLM SERVER YET".to_string() } else { format!("COLLECTOR UNREACHABLE ({seen})") };
        tail.push(Span::styled(" · ", bold()));
        tail.push(Span::styled(text, red().add_modifier(Modifier::BOLD)));
    } else if now - s.generated_at > 30 {
        tail.push(Span::styled(" · ", bold()));
        tail.push(Span::styled(format!("COLLECTOR STALLED {}", fmt_duration(now - s.generated_at)), red().add_modifier(Modifier::BOLD)));
    }
    if app.multi_server() {
        // the server switcher: which of the configured servers this is
        tail.push(Span::styled(format!(" · [{}/{}]", app.server_idx + 1, app.fleet.len()), bold()));
    }
    // card #291 (the owner, after shelving the redesign: "dont need any of that"): TARGET MISSED,
    // the pinned-layout tell (`[half-h]` etc.) and the chart-lines tell (`[lines]`) are gone from
    // the top status line - a missed target is still on the TARGETS strip/page itself, the pinned
    // layout is still visible in the layout, and `c` still toggles the chart style; none of the
    // three needed a permanent badge here.
    if !s.gate.up {
        tail.push(Span::styled(" · ", bold()));
        tail.push(Span::styled("GATE DOWN", red().add_modifier(Modifier::BOLD)));
    }
    if !s.firing.is_empty() {
        tail.push(Span::styled(" · ", bold()));
        tail.push(Span::styled(format!("{} FIRING", s.firing.len()), red().add_modifier(Modifier::BOLD)));
    }
    // #51 put a `[page1-v2]` tell here while the redesign was an opt-in, by the "show it only
    // when non-default" rule #291 above removed the last other user of. #191 retired the tell
    // outright rather than move it to the classic grid: the redesign IS page 1 now, so a
    // permanent "[v2]" badge on the page every stranger lands on is first-impression noise, and
    // badging the classic grid instead costs header columns at 42 wide (the owner's narrowest
    // pane) to say something the footer already says in words - `v old page 1` on one page,
    // `v new page 1` on the other.
    let state_style = if s.serve.up { bold() } else { red().add_modifier(Modifier::BOLD) };
    // longest first: drop the restarts, then shorten the title (the model says more than the
    // product's name does), then drop the model
    // card #258: the cost per hour rides the header - the one money figure always on screen (#101's
    // rule; it used to ride page 1's "worst now" line, which the owner had removed). It is the
    // first thing kept after the restarts count, and dropped before the model is.
    let cost = s.cost.as_ref().and_then(|c| c.live_usd_per_hour).map(|v| format!("${v:.2}/h"));
    let mut variants: Vec<Vec<&str>> = Vec::new();
    if let Some(c) = &cost {
        variants.extend([vec![&lead, &host, &model, "\0", &restarts, c.as_str()], vec![&lead, &host, &model, "\0", c.as_str()], vec![&short_lead, &host, &model, "\0", c.as_str()]]);
    }
    variants.extend([vec![&lead, &host, &model, "\0", &restarts], vec![&lead, &host, &model, "\0"], vec![&short_lead, &host, &model, "\0"], vec![&lead, &host, "\0"], vec![&short_lead, &host, "\0"], vec![&short_lead, "\0"], vec!["\0"]]);
    let build = |parts: &[&str]| -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(" ")];
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", bold()));
            }
            if *part == "\0" {
                spans.push(Span::styled(state.clone(), state_style));
            } else if cost.as_deref() == Some(*part) {
                spans.push(Span::raw(part.to_string()));
            } else if *part == restarts && s.serve.restarts_today > 0 {
                spans.push(Span::styled(part.to_string(), widgets::warn().add_modifier(Modifier::BOLD)));
            } else {
                spans.push(Span::styled(part.to_string(), bold()));
            }
        }
        spans.extend(tail.iter().cloned());
        spans
    };
    // card #291 (the owner: "dont need any of that"): the scroll position used to ride here too
    // (card #258); it is gone from the top status line now - the bottom key-hint line (`enter
    // open`, `pgup/pgdn page`) and, for FLEET/WATCH, `chrome_tabs`'s own scroll position still
    // say where you are.
    let refreshed = format!("refreshed {} ", fmt_local(app.last_ok.unwrap_or(now), "%H:%M:%S"));
    // the rung that keeps "refreshed" may drop the restarts count only - never the model, and
    // never the cost (#101: a dollar figure on screen with no key pressed)
    let with_model = |v: &Vec<&str>| v.contains(&model.as_str()) && cost.as_deref().is_none_or(|c| v.contains(&c));
    let pick = |reserve: usize, need_model: bool| variants.iter().filter(|v| !need_model || with_model(v)).map(|v| build(v)).find(|spans| spans_len(spans) + reserve <= w);
    let left = pick(refreshed.chars().count() + 2, true).or_else(|| pick(0, false));
    let mut left = left.unwrap_or_else(|| build(&variants[variants.len() - 1]));
    let used = spans_len(&left);
    if used + refreshed.chars().count() + 2 <= w {
        left.push(Span::raw(" ".repeat(w - used - refreshed.chars().count())));
        left.push(Span::styled(refreshed, dim()));
    }
    widgets::fit_line(left, w)
}

/// The header row plus, when the collector is unreachable or stalled, a loud second row.
/// Returns what is left for the body.
pub fn draw_header(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) -> Rect {
    draw_header_lines(f, app, s, area, now, true)
}

/// The minimized overview: its status card IS the header, so only the loud row (collector
/// unreachable / stalled) is drawn, when there is one.
pub fn draw_warning(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) -> Rect {
    draw_header_lines(f, app, s, area, now, false)
}

fn draw_header_lines(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64, title_row: bool) -> Rect {
    let lines = header_lines(app, s, area.width, now, title_row);
    let h = (lines.len() as u16).min(area.height);
    f.render_widget(Paragraph::new(lines), Rect { height: h, ..area });
    Rect { y: area.y + h, height: area.height - h, ..area }
}

/// The header row (optional) plus, when the collector is unreachable or stalled, the loud red row.
fn header_lines(app: &App, s: &Status, width: u16, now: i64, title_row: bool) -> Vec<Line<'static>> {
    let area = Rect { x: 0, y: 0, width, height: 0 };
    if title_row {
        // card #258: one line - the loud unreachable / stalled state rides in the header itself
        return vec![header(app, s, area.width, now)];
    }
    let mut lines = Vec::new();
    if let Some(t) = unreachable_text(app, now) {
        lines.push(Line::styled(fit(&format!(" {t}"), area.width as usize), red().add_modifier(Modifier::BOLD)));
    } else if now - s.generated_at > 30 {
        lines.push(Line::styled(fit(&format!(" COLLECTOR STALLED - its newest sample is {} old", fmt_duration(now - s.generated_at)), area.width as usize), red().add_modifier(Modifier::BOLD)));
    }
    lines
}

/// The frame's chrome, card #258 (the owner, 14:56: "this looks terrible on both bottom and top";
/// 15:01: "just remove all of this" about page 1's "worst now" verdict block; 15:03: "move the llm
/// server status back up to where you had it before"): EXACTLY one line at the top - the header
/// (`header`: the page or LLM SERVER STATUS, host, model, UP, $/h, refreshed) - and one line at
/// the bottom - the tab line (`chrome_tabs`: tabs, scroll position, `? help`). Neither wraps.
/// The key-hint footer is gone (the keys are in `?` help), and so is the verdict line.
///
/// `_extra` (page 1 used to pass its "worst now" lines) is no longer drawn. Below 5 rows the
/// header goes and the tab line stays (#182: the pages must stay visibly reachable). Returns
/// `(body, footer)`: the caller draws `footer(app, ..)` - the tab line - into `footer` AFTER the
/// page, exactly as it drew the key hints there before.
pub fn draw_chrome_bottom(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64, _extra: Vec<Line<'static>>) -> (Rect, Rect) {
    let mut body = area;
    if area.height >= 5 {
        f.render_widget(Paragraph::new(header(app, s, area.width, now)), Rect { height: 1, ..area });
        app.chrome_top.set(Some(Rect { height: 1, ..area }));
        body = Rect { y: area.y + 1, height: area.height - 1, ..area };
    }
    // the bottom row is RESERVED here and filled by the caller after the page is laid out
    // (`footer`/`footer_at` = `chrome_tabs`), so the scroll position it shows is this frame's
    let foot = Rect { y: body.y + body.height.saturating_sub(1), height: body.height.min(1), ..body };
    (Rect { height: body.height - foot.height, ..body }, foot)
}

/// The FLEET strip: every configured server, UP / DOWN and how fast it is writing. Only with
/// more than one `[[server]]`; the one on screen is bold and bracketed.
pub fn fleet_line(app: &App, width: u16) -> Option<Line<'static>> {
    if !app.multi_server() {
        return None;
    }
    let build = |with_speed: bool, with_hint: bool| -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(" FLEET ", bold())];
        for (i, e) in app.fleet.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", dim()));
            }
            let here = i == app.server_idx;
            let name = if here { format!("[{}]", e.name) } else { e.name.clone() };
            spans.push(Span::styled(format!("{name} "), if here { bold() } else { Style::default() }));
            match &e.state {
                Some(Ok(st)) if st.up => {
                    spans.push(Span::styled("UP", widgets::good()));
                    if with_speed {
                        spans.push(Span::raw(format!(" {:.0} tok/s", st.decode_tok_s)));
                    }
                }
                Some(Ok(_)) => spans.push(Span::styled("DOWN", red().add_modifier(Modifier::BOLD))),
                Some(Err(_)) => spans.push(Span::styled("NO MONITOR", red())),
                None => spans.push(Span::styled("...", dim())),
            }
        }
        if with_hint {
            spans.push(Span::styled("   [ ] switch", dim()));
        }
        spans
    };
    let w = width as usize;
    let spans = [build(true, true), build(true, false)].into_iter().find(|v| spans_len(v) <= w).unwrap_or_else(|| build(false, false));
    Some(widgets::fit_line(spans, w))
}

/// The TARGETS strip: how much of the last 24 h met each of the owner's targets. A miss is
/// the only colour on it.
pub fn targets_line(s: &Status, width: u16) -> Option<Line<'static>> {
    let t = &s.targets;
    if !t.measured() {
        return None;
    }
    let share = |r: &lss_core::targets::TargetRow| r.met_pct.map_or_else(|| "-".to_string(), |m| if r.key == "uptime" { format!("{}%", (m * 100.0).round() / 100.0) } else { format!("{m:.0}%") });
    let build = |head: &str, with_goal: bool| -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(format!(" {head} "), bold())];
        for (i, r) in t.rows.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", dim()));
            }
            spans.push(Span::styled(format!("{} ", r.short), dim()));
            let missed = r.ok == Some(false);
            spans.push(Span::styled(share(r), if missed { widgets::warn().add_modifier(Modifier::BOLD) } else { bold() }));
            if missed {
                spans.push(Span::styled(if with_goal { format!(" MISSED (goal {:.0}%)", r.goal_pct) } else { " MISSED".to_string() }, widgets::warn()));
            }
        }
        spans
    };
    let w = width as usize;
    let head = format!("TARGETS {}", t.window);
    let missed = t.missed();
    let short = || -> Vec<Span<'static>> {
        let met = t.rows.iter().filter(|r| r.ok == Some(true)).count();
        let mut v = vec![Span::styled(" TARGETS ", bold()), Span::raw(format!("{met} of {} met", t.rows.len()))];
        if let Some(m) = missed.first() {
            v.push(Span::styled(format!(" · {} {} MISSED", m.short, share(m)), widgets::warn()));
        }
        v
    };
    let spans = [build(&head, true), build(&head, false), build("TARGETS", false)].into_iter().find(|v| spans_len(v) <= w).unwrap_or_else(short);
    Some(widgets::fit_line(spans, w))
}

/// Card #182 (the owner: "i still want those" - the 1-9 pages, which the redesigned page 1 could
/// only be reached from via a footer hint competing for room with the scroll position and, at
/// his own window size, a "not shown at this size" notice). The keys already worked - pressing
/// `3` reached GPUS from anywhere - what was missing was a visible strip naming them, the way a
/// browser or a tmux status line does. `PageId::key_digit()` IS the key that actually reaches
/// each page (checked against `KeyCode::Char('1'..='9' | 'a')`, `ui/mod.rs`); `0` (and `esc`)
/// return to the overview, which is not itself a `PageId` and never was - card #231's split of
/// page 8 gave ADVICE its own key, `a`, rather than displacing `0`, once the orchestrator's correction
/// caught that `0` was never free to reassign (the owner: "page 0 has no tab thats why tab wasnt
/// working").
///
/// Three tiers, tried widest first (the same "build the richest, fall back" shape as
/// `footer_at`/`targets_line` above), and the narrowest ALWAYS renders - card #182 item 2:
/// "it must degrade VISIBLY at narrow widths ... and never vanish silently".
///   1. names   - " 0 overview  1 latency  2 load  3 gpus ... a advice " current bracketed
///   2. numbers - " [0] 1 2 [3] 4 5 6 7 8 9 a " (names dropped, every position still present)
///   3. current - " gpus 3/10 " or " overview " - just enough to say where you are and how many
///      pages there are, however narrow the pane.
///
/// `[bracketed]` marks the current page - `fleet_line`'s own convention for "the one you are on"
/// (`[name]` there too), not colour, so it reads the same in every theme.
pub fn tab_strip(view: View, width: u16) -> Line<'static> {
    let current: Option<PageId> = match view {
        View::Page(p) => Some(p),
        _ => None,
    };
    let w = width as usize;
    let mark = |label: String, here: bool| if here { format!("[{label}]") } else { label };

    let names: Vec<Span<'static>> = std::iter::once(Span::styled(format!(" {} ", mark("0 overview".into(), current.is_none())), if current.is_none() { bold() } else { dim() }))
        .chain(PageId::ALL.iter().map(|p| {
            let here = current == Some(*p);
            Span::styled(format!(" {} ", mark(format!("{} {}", p.key_digit(), p.title().to_lowercase()), here)), if here { bold() } else { dim() })
        }))
        .collect();
    if spans_len(&names) <= w {
        return widgets::fit_line(names, w);
    }

    let nums: Vec<Span<'static>> = std::iter::once(Span::styled(mark(" 0".into(), current.is_none()), if current.is_none() { bold() } else { dim() }))
        .chain(PageId::ALL.iter().map(|p| {
            let here = current == Some(*p);
            Span::styled(mark(format!(" {}", p.key_digit()), here), if here { bold() } else { dim() })
        }))
        .chain(std::iter::once(Span::raw(" ")))
        .collect();
    if spans_len(&nums) <= w {
        return widgets::fit_line(nums, w);
    }

    let (label, n) = current.map_or_else(|| ("overview".to_string(), 0), |p| (p.title().to_lowercase(), p.number()));
    let short = if n > 0 {
        vec![Span::styled(format!(" {label} {n}/{} ", PageId::ALL.len()), bold())]
    } else {
        vec![Span::styled(format!(" {label} "), bold())]
    };
    widgets::fit_line(short, w)
}

/// Card #258 (the owner, pasting the old 5-line bottom block: "this looks terrible on both bottom
/// and top"): the key-hint footer is GONE - the keys live in `?` help (#257). What used to be the
/// footer is the chrome's second line: the tab strip, then (only when the view scrolls) where you
/// are in it, then a dim `? help`. Kept under this name so every view that drew a footer draws
/// this line instead.
pub fn footer(app: &App, width: u16) -> Line<'static> {
    chrome_tabs(app, width)
}

/// See `footer` - the height argument is no longer used by anything.
pub fn footer_at(app: &App, width: u16, _height: u16) -> Line<'static> {
    chrome_tabs(app, width)
}

/// "N-M of T" when the view scrolls (#177: the only thing that says more exists below).
fn scroll_pos(app: &App) -> Option<String> {
    if !(matches!(app.view, View::Page(_) | View::Fleet | View::Watch) || (app.view == View::Overview && app.dash)) {
        return None;
    }
    let (first, shown, total) = app.page_rows.get();
    (total > shown && shown > 0).then(|| format!("{}-{} of {}", first + 1, (first + shown).min(total), total))
}

/// Card #258, chrome line 2: the tab strip with the current page marked, ending in a dim
/// `? help`. The scroll position ("1-34 of 76", #177: the only thing that says more exists below)
/// rides just before it when the view scrolls. The suffix's room is reserved FIRST, so the strip
/// drops to its narrower tiers rather than pushing `? help` off the edge.
pub fn chrome_tabs(app: &App, width: u16) -> Line<'static> {
    let w = width as usize;
    // page 1 and the detail pages carry it in the HEADER (the owner: the labelled strip must fit at
    // 120); FLEET and WATCH have no header row of their own, so theirs stays here
    let pos = if matches!(app.view, View::Fleet | View::Watch) { scroll_pos(app) } else { None };
    let suffix = match &pos {
        Some(p) => format!("{p} \u{b7} ? help "),
        None => "? help ".to_string(),
    };
    // exactly the suffix: the labelled strip needs 113 cols, so at 120 one spare column matters
    let reserve = suffix.chars().count();
    if w <= reserve + 4 {
        return widgets::fit_line(vec![Span::styled(suffix, dim())], w);
    }
    let mut spans = tab_strip(app.view, (w - reserve) as u16).spans;
    let used = spans_len(&spans);
    spans.push(Span::raw(" ".repeat(w.saturating_sub(used + suffix.chars().count()))));
    spans.push(Span::styled(suffix, dim()));
    widgets::fit_line(spans, w)
}

/// Card #291 (the owner, after shelving the R2 redesign: "i like the way lss loads right now" +
/// "the only thing you need to add on the bottom is the short cut keys and buttons"): one line
/// of key hints, directly under the tab strip - wording lifted from `HELP_GROUPS`'s own key
/// column, not restated. Widest-first graduated tiers (the same idiom `header`/`chrome_tabs`
/// already use for their own width degradation): the first tier that fits `width` WHOLE wins,
/// so a narrow pane drops the least-used keys before anything wraps or clips mid-word - the
/// narrowest tier (`? help · q quit`) is short enough for any pane this app ever draws into.
pub fn key_hints_line(width: u16) -> Line<'static> {
    // "up/down", not the arrow glyphs, matching `HELP_GROUPS`'s own wording - and staying inside
    // `every_glyph_is_single_width`'s allowed set, which arrows are not in.
    const TIERS: [&str; 5] = [
        " tab pages \u{b7} up/down scroll \u{b7} pgup/pgdn page \u{b7} enter open \u{b7} r range \u{b7} T theme \u{b7} v layout \u{b7} ? help \u{b7} q quit ",
        " tab pages \u{b7} up/down scroll \u{b7} pgup/pgdn page \u{b7} enter open \u{b7} r range \u{b7} T theme \u{b7} ? help \u{b7} q quit ",
        " tab pages \u{b7} up/down scroll \u{b7} enter open \u{b7} r range \u{b7} ? help \u{b7} q quit ",
        " tab pages \u{b7} up/down scroll \u{b7} ? help \u{b7} q quit ",
        " ? help \u{b7} q quit ",
    ];
    let w = width as usize;
    let text = TIERS.iter().find(|t| t.chars().count() <= w).copied().unwrap_or("");
    Line::styled(text, dim())
}

/// Reserves the LAST row of `area` for `key_hints_line` and draws it there, returning what is
/// left for the caller's own chrome (header/tab-strip/footer), exactly as before - only a
/// genuinely spacious pane gives up a row for it, kept above the height where the overview's own
/// MINIMIZED card view takes over (`h <= 12`, `pick_shape`'s own boundary) so nothing at an
/// already-tuned small size has to give up a row it did not have to spare before.
pub fn reserve_key_hints(f: &mut Frame, area: Rect) -> Rect {
    if area.height <= 12 {
        return area;
    }
    let row = Rect { y: area.y + area.height - 1, height: 1, ..area };
    f.render_widget(Paragraph::new(key_hints_line(area.width)), row);
    Rect { height: area.height - 1, ..area }
}

/// Card #291's key-hint line on the CLASSIC GRID specifically (lss-verifier-2's FAIL on the
/// first cut): unlike the scrolling pages (`dash.rs`, `pages.rs` - one fewer row just means
/// slightly more to scroll, never a change in WHAT is there, and `reserve_key_hints` is fine for
/// those), the classic grid's box layout (`pick_shape`, its strips, each card's own row budget)
/// reacts directly to the height it is given: shrinking it by one row silently dropped the whole
/// TARGETS strip at 94x20, a GPU card's own "clk" row at 200x24, and swapped LANES' two-row
/// chart for a DIFFERENTLY WORDED one-row sparkline ("in-flight budget used - last hour" became
/// "budget 1h") - content changes, not a chart losing one row of resolution, which is the one
/// thing this card allows the hint line to cost.
///
/// So: render the grid TWICE into throwaway backends, once at the real height and once one row
/// short, and only actually show it one row short (with the hint line filling the freed row)
/// when doing so provably changes nothing but chart/border FRAMING - every run of box-drawing or
/// sparkline glyphs is a separator, like whitespace, since a chart's own vertical axis line
/// (`│`) literally repeats once per chart row and so scales with chart height exactly the same
/// way the bars themselves do (measured: a 100-row vs 99-row classic-grid probe otherwise
/// differed ONLY in how many bare `│` axis rows a chart had - three fewer, at three different
/// charts - with every real WORD identical and in the same order). What is left after that
/// split is every readable LABEL and NUMBER, in order - real content, which must match exactly.
/// Otherwise the grid keeps its full height and the hint line is simply not shown at that size,
/// per the owner's own "everything else is perfect" (a page that scrolls has nowhere for the
/// text to silently go; a fixed grid does).
fn draw_classic_with_key_hints(f: &mut Frame, app: &App, s: &Status, area: Rect, now: i64) {
    if area.height <= 12 || area.width == 0 {
        overview::draw(f, app, s, area, now);
        return;
    }
    let probe_text = |h: u16| -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(area.width, h)).unwrap();
        let _ = t.draw(|pf| overview::draw(pf, app, s, pf.area(), now));
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    };
    fn content_words(text: &str) -> Vec<&str> {
        text.split(|c: char| {
            c.is_whitespace()
                || matches!(c, '\u{2500}'..='\u{257f}') // box drawing block (borders, rules, axis lines)
                || matches!(c, '\u{2581}'..='\u{2588}') // sparkline bars
        })
        .filter(|t| !t.is_empty())
        .collect()
    }
    let (full, short) = (probe_text(area.height), probe_text(area.height - 1));
    // the two probes above ran `overview::draw` outside the top-level dispatcher that normally
    // clears `app.rects` first (click hit-testing) - so it is cleared here too, or the real draw
    // below would append onto two throwaway renders' worth of stale/duplicate entries.
    app.rects.borrow_mut().clear();
    if content_words(&full) == content_words(&short) {
        overview::draw(f, app, s, Rect { height: area.height - 1, ..area }, now);
        let row = Rect { y: area.y + area.height - 1, height: 1, ..area };
        f.render_widget(Paragraph::new(key_hints_line(area.width)), row);
    } else {
        overview::draw(f, app, s, area, now);
    }
}

pub const HELP_GROUPS: [(&str, &[(&str, &str)]); 6] = [
    // #182: `v` switches between TWO overview shapes - the redesigned page 1 (default: since
    // #227/#255 a grid of boxes, at most two columns; since #258 the status line on top, the tab
    // strip at the bottom, and no "worst now" verdict line at all; no focus/pin concept) and the classic grid below. `arrows`/`enter`/`L`
    // only move or pin a FOCUSED box, so they apply to the classic grid alone; on page 1 they
    // are real key presses that change nothing you can see (L still moves a hidden pin, arrows
    // do nothing) - said here rather than left for a reader to discover by trying them.
    // #222/#231: Tab/BackTab cycle an 11-stop ring from EITHER overview or any page - the
    // overview itself, then all ten pages, wrapping ADVICE back to the overview and vice versa
    // (the orchestrator's correction: "the Tab ring MUST include the overview"). Enter alone opens the
    // focused box on the classic grid.
    ("Overview", &[("v", "page 1 (default, this one) / the classic grid"), ("on page 1", "a grid of boxes, at most two columns; the status line is on top, the tab strip at the bottom; up/down/pgup/pgdn scroll it"), ("on the classic grid", "arrows move between boxes, enter opens the focused one full screen, L cycles/pins the layout (auto > half-h > ... > focus)"), ("tab / shift-tab", "step through the overview and all ten pages, wrapping"), ("T", "dark / light theme"), ("[ ]", "previous / next server (several [[server]] in lss.toml)")]),
    // #231: 8 ALERTS and 9 INCIDENTS are their own pages now (the combined page 8 split); ADVICE
    // keeps `a` as its key since `0` stays the overview's own key (the orchestrator's correction).
    ("Pages", &[("1 2 3", "LATENCY  LOAD  GPUS"), ("4 5 6", "USERS  TOKENS  MODEL"), ("7 8 9", "GATEWAY  ALERTS  INCIDENTS"), ("a", "ADVICE"), ("tab / shift-tab", "next / previous stop in the ring - the overview then all ten pages, wraps ADVICE <-> overview"), ("r", "time range: 15m > 1h > 6h > 24h > 7d"), ("c", "GPUS/USERS charts: colour lines / braille dots (default dots)"), ("s", "USERS: sort the table"), ("b", "MODEL: benchmark this model (lss bench quick)"), ("enter", "MODEL: compare with another loadout (up/down pick)"), ("up/down pgup/pgdn", "scroll the sections"), ("0 / esc", "back to the overview")]),
    ("Fleet", &[("f", "the fleet view: one row per [[server]] node (only with more than one configured)"), ("[ ]", "pick which node is current, from anywhere"), ("enter", "drill in: that node's own overview, unchanged"), ("esc", "back")]),
    ("Watch", &[("w", "what the sources you follow have released, and when - needs [watch] path configured"), ("NEW", "published since you last opened this page"), ("esc", "back")]),
    // #227 ("alot of wasted space"): page 1's own box captions moved here - an alert/incident's
    // merge rule, and what COST PER 1M TOKENS actually measures - rather than a full sentence in
    // every box, every frame.
    // #247: LANES' own two definitions moved here too (they cost that box 5 wrapped rows at 120
    // wide - the biggest single line-count driver #227's zero-scroll bar did not reach).
    // #255 round 3: USERS' own "on now"/"share/$"/"lane hidden" explanation paragraph moved here
    // too - the table plus its one-line headline is what stays on page 1 now.
    ("Reading it", &[("reading / writing", "prefill = reading your prompt; decode = writing the answer"), ("RED", "a problem, and nothing else"), ("thick border", "the focused box"), ("GPU0*", "excluded from thermal alerts"), ("probes: N", "the monitor's own C1 probes, kept out of LANES"), ("C1", "last VALID idle probe; invalid = it met real traffic"), ("B on a time axis", "a benchmark ran then: load we made, not users"), ("an alert", "a live condition - clears by itself when it does"), ("an incident", "a dated record - never changes once it closes"), ("$/1M tok basis", "uncached prefill+gen tokens - do not sum to a total"), ("held at the door", "the lane's token budget is full - frees itself up"), ("turned away", "refused at the gateway (429/413) - nothing ran"), ("cached (tokens)", "prompt tokens from the cache - part of prompt, not extra"), ("$/kWh unresolved", "may or may not belong on top of the numbers above"), ("USERS on now", "running now; else age since last (\u{2014}=unknown, not idle)"), ("USERS share/$", "% of this table's tokens, last 24h - not true GPU-seconds"), ("USERS narrow tier", "lane/req24h/prompt/$ hidden at this width")]),
    ("Always", &[("?", "this help"), ("q", "quit (ctrl-c too)")]),
];

/// card #257: the help's key column (`   {key:<19} ` - the trailing space is the gap a full
/// 19-character key like "on the classic grid" never had), and the narrowest description column
/// worth keeping beside it - below that the description goes on its own lines under the key.
const HELP_KEY_W: usize = 23;
const HELP_MIN_DESC_W: usize = 16;

/// card #257: `text` in lines of at most `room` columns, broken ONLY between words; a single word
/// wider than `room` is cut with an ellipsis rather than split (page 1's rule, #227). The overlay
/// used to clip each line at its right border mid-word ("see Pa", "default do").
fn help_wrap(text: &str, room: usize) -> Vec<String> {
    let room = room.max(1);
    // a line that fits is kept exactly as written (the Pages rows align on double spaces)
    if text.chars().count() <= room {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let word: String = if word.chars().count() > room { word.chars().take(room - 1).chain(['\u{2026}']).collect() } else { word.to_string() };
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > room {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn draw_help(f: &mut Frame, app: &App) {
    let area = f.area();
    let w = 82.min(area.width.saturating_sub(2));
    // card #257: every line is wrapped to the overlay's own inner width, so nothing is cut at
    // its right border - the descriptions continue under their own column
    let inner_w = w.saturating_sub(2) as usize;
    let beside = inner_w >= HELP_KEY_W + HELP_MIN_DESC_W;
    let mut lines = Vec::new();
    for (group, keys) in HELP_GROUPS {
        lines.push(Line::styled(format!(" {group}"), bold()));
        for (k, d) in keys {
            if beside {
                for (i, part) in help_wrap(d, inner_w - HELP_KEY_W).into_iter().enumerate() {
                    let key = if i == 0 { format!("   {k:<19} ") } else { " ".repeat(HELP_KEY_W) };
                    lines.push(Line::from(vec![Span::styled(key, bold()), Span::raw(part)]));
                }
            } else {
                lines.push(Line::styled(format!("   {k}"), bold()));
                for part in help_wrap(d, inner_w.saturating_sub(5)) {
                    lines.push(Line::raw(format!("     {part}")));
                }
            }
        }
    }
    let total = lines.len();
    // #247: this already clamped the BOX to the terminal height when content overflowed it
    // (`(rows+2).min(area.height)`) - the bug was that the CONTENT was never scrolled to match,
    // so "Reading it" (where the captions #227/#247 moved off page 1 live) fell off the bottom
    // of a shrunken box with no way to reach it. The box sizing is unchanged; the content now
    // scrolls inside it.
    let h = (total as u16 + 2).min(area.height);
    if w < 20 || h < 4 {
        return;
    }
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    f.render_widget(Block::default().style(base_style(app)), rect);
    let scrollable = total > h.saturating_sub(2) as usize;
    let bottom_hint = if scrollable { " esc or ? closes \u{b7} up/down pgup/pgdn scroll " } else { " esc or ? closes " };
    let block = frame(true, palette(app.light).fg).title(Span::styled(" LLM SERVER STATUS keys ", bold())).title_bottom(Line::styled(bottom_hint, bold()));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let capacity = inner.height as usize;
    let last = total.saturating_sub(1);
    let start = app.help_scroll.min(last);
    let raw_shown = capacity.min(total.saturating_sub(start)).max(1);
    let has_more_below = start + raw_shown < total;
    // reserve the overlay's own last line for the "more below" marker, rather than pushing an
    // extra line past `capacity` (which would silently overflow the box by one row)
    let shown = if has_more_below { raw_shown.saturating_sub(1).max(1) } else { raw_shown };
    app.help_rows.set((start, shown, total));
    let mut visible: Vec<Line<'static>> = lines.into_iter().skip(start).take(shown).collect();
    if start + shown < total {
        visible.push(Line::styled(format!(" \u{2193} more below ({}/{} lines) - pgdn", start + shown, total), dim()));
    }
    f.render_widget(Paragraph::new(visible), inner);
}
