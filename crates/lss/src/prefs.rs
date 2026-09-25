//! What the screen remembers between runs: theme, pinned layout, time range.
//! Written ONLY to `$LSS_PREFS` when that is set, else to `~/.config/lss/ui.json`, and never
//! with `--no-save`. Losing the file is harmless; a missing or bad file means the dark theme.
//! Every test, tmux session and verification run must use `LSS_PREFS=<temp file>` or
//! `--no-save`, so trying the `T` key never changes the owner's saved theme.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// `dark` | `light`
    pub theme: String,
    /// `auto` or a layout name (`half-h`, `small`, …)
    pub layout: String,
    /// `15m` … `7d`
    pub range: String,
    /// `lines` | `dots` (`""` from an old file, or unset, means `dots` - card #48: the owner
    /// opts in to line charts, never opted in for him). See `resolve_chart_lines` for how
    /// `$LSS_CHART` overrides this at startup.
    pub chart: String,
    /// `new` | `old`. Card #191, 2026-09-22: `""` (an old file written before `page1` existed,
    /// or an unset value) now means the COMPILED DEFAULT, which is `new` - the owner compared
    /// both pages and chose the cost+loadout one, so only an explicit `"old"` opts back out.
    /// Reading `""` as `old` would have quietly pinned every pre-#191 ui.json to the page he
    /// rejected. No env override here, unlike `chart` - not asked for.
    pub page1: String,
}

/// Where the preferences live: `$LSS_PREFS` wins (a test points it at a temp file), else
/// `~/.config/lss/ui.json`. An empty `$LSS_PREFS` is not a path.
pub fn path_from(env_prefs: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(p) = env_prefs.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(Path::new(home?).join(".config/lss/ui.json"))
}

pub fn default_path() -> Option<PathBuf> {
    path_from(std::env::var_os("LSS_PREFS").as_deref(), std::env::var_os("HOME").as_deref())
}

/// Remembers what was last written and writes only a CHANGE, only to its one path, and not
/// at all with `--no-save`.
#[derive(Debug)]
pub struct Saver {
    path: Option<PathBuf>,
    saved: Prefs,
}

impl Saver {
    /// `no_save` = never write. `current` = what is on screen at start (nothing to write yet).
    pub fn new(path: Option<PathBuf>, no_save: bool, current: Prefs) -> Saver {
        Saver { path: if no_save { None } else { path }, saved: current }
    }

    /// Returns true when the file was written.
    pub fn note(&mut self, now: Prefs) -> bool {
        if now == self.saved {
            return false;
        }
        self.saved = now;
        match &self.path {
            Some(p) => {
                save(p, &self.saved);
                true
            }
            None => false,
        }
    }
}

pub fn load(path: &Path) -> Prefs {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(path: &Path, prefs: &Prefs) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(prefs).unwrap_or_default() + "\n");
}

impl Prefs {
    pub fn of(app: &crate::ui::App) -> Prefs {
        Prefs {
            theme: if app.light { "light" } else { "dark" }.into(),
            layout: app.pinned.map_or("auto", crate::ui::Shape::name).into(),
            range: crate::data::range_label(app.range_idx).into(),
            chart: if app.chart_lines { "lines" } else { "dots" }.into(),
            page1: if app.dash { "new" } else { "old" }.into(),
        }
    }

    pub fn apply(&self, app: &mut crate::ui::App) {
        app.light = self.theme == "light";
        app.pinned = crate::ui::Shape::from_name(&self.layout);
        if let Some(i) = lss_core::series::RANGES.iter().position(|r| *r == self.range) {
            app.range_idx = i;
        }
        // #191: only an explicit "old" opts out; "" (unset, or a file older than this field)
        // falls through to the compiled default, which is the new page.
        app.dash = self.page1 != "old";
        app.chart_lines = self.chart == "lines";
    }
}

/// `$LSS_CHART` (`"lines"` | `"dots"`) wins over the saved `chart` pref, which wins over the
/// default (dots - card #48: this is opt-in, never opted in for the owner). An unrecognised
/// env value (typo, empty) is not a signal either way and falls through to the pref, same as no
/// env at all - a bad env var should never crash or silently mean "off" when the saved pref
/// says otherwise.
pub fn resolve_chart_lines(env: Option<&str>, prefs_chart_lines: bool) -> bool {
    match env {
        Some("lines") => true,
        Some("dots") => false,
        _ => prefs_chart_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_survive_garbage() {
        let dir = std::env::temp_dir().join(format!("lss-prefs-{}", std::process::id()));
        let path = dir.join("ui.json");
        assert_eq!(load(&path), Prefs::default(), "no file = defaults");
        let mut app = crate::ui::App { light: true, pinned: Some(crate::ui::Shape::Small), range_idx: 3, ..Default::default() };
        save(&path, &Prefs::of(&app));
        app = crate::ui::App::default();
        load(&path).apply(&mut app);
        assert!(app.light);
        assert_eq!(app.pinned, Some(crate::ui::Shape::Small));
        assert_eq!(crate::data::range_label(app.range_idx), "24h");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), Prefs::default());
        let mut app = crate::ui::App { light: true, ..Default::default() };
        load(&path).apply(&mut app);
        assert!(!app.light, "a bad prefs file means the dark theme");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prefs_go_to_lss_prefs_when_set_and_nowhere_with_no_save() {
        use std::ffi::OsStr;
        let home = OsStr::new("/home/you");
        assert_eq!(path_from(None, Some(home)), Some(PathBuf::from("/home/you/.config/lss/ui.json")));
        assert_eq!(path_from(Some(OsStr::new("/tmp/t/ui.json")), Some(home)), Some(PathBuf::from("/tmp/t/ui.json")), "$LSS_PREFS wins over the home directory");
        assert_eq!(path_from(Some(OsStr::new("")), Some(home)), Some(PathBuf::from("/home/you/.config/lss/ui.json")), "an empty $LSS_PREFS is not a path");
        assert_eq!(path_from(None, None), None);

        let dir = std::env::temp_dir().join(format!("lss-prefs-iso-{}", std::process::id()));
        let (owner, temp) = (dir.join("owner/ui.json"), dir.join("temp/ui.json"));
        save(&owner, &Prefs { theme: "dark".into(), layout: "auto".into(), range: "1h".into(), chart: "dots".into(), page1: "old".into() });
        let before = std::fs::read_to_string(&owner).unwrap();
        // a test run: LSS_PREFS points at a temp file, and the theme key is pressed
        let mut app = crate::ui::App::default();
        assert!(!app.light, "no prefs at all = dark");
        let mut saver = Saver::new(Some(temp.clone()), false, Prefs::of(&app));
        assert!(!saver.note(Prefs::of(&app)), "nothing changed, nothing written");
        app.on_key(ratatui::crossterm::event::KeyCode::Char('T'));
        assert!(saver.note(Prefs::of(&app)));
        assert_eq!(load(&temp).theme, "light");
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), before, "the owner's file is untouched");
        // --no-save: nothing is written anywhere
        let nowhere = dir.join("never/ui.json");
        let mut saver = Saver::new(Some(nowhere.clone()), true, Prefs::of(&app));
        app.on_key(ratatui::crossterm::event::KeyCode::Char('T'));
        assert!(!saver.note(Prefs::of(&app)));
        assert!(!nowhere.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #48, 2026-09-21: `c` persists like `T`/`L` do, and respects `--no-save` the same way -
    /// this exact bug (a test run writing the owner's real prefs file) already cost a morning.
    #[test]
    fn the_c_key_toggles_chart_lines_and_persists_like_theme_and_layout_do() {
        let dir = std::env::temp_dir().join(format!("lss-prefs-chart-{}", std::process::id()));
        let (owner, temp) = (dir.join("owner/ui.json"), dir.join("temp/ui.json"));
        save(&owner, &Prefs::default());
        let before = std::fs::read_to_string(&owner).unwrap();

        let mut app = crate::ui::App::default();
        assert!(!app.chart_lines, "default is dots - the owner opts in, never opted in for him");
        let mut saver = Saver::new(Some(temp.clone()), false, Prefs::of(&app));
        assert!(!saver.note(Prefs::of(&app)), "nothing changed, nothing written");
        app.on_key(ratatui::crossterm::event::KeyCode::Char('c'));
        assert!(app.chart_lines);
        assert!(saver.note(Prefs::of(&app)));
        assert_eq!(load(&temp).chart, "lines");
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), before, "the owner's file is untouched");
        // press it again: back to dots, and that persists too (not just the first flip)
        app.on_key(ratatui::crossterm::event::KeyCode::Char('c'));
        assert!(!app.chart_lines);
        assert!(saver.note(Prefs::of(&app)));
        assert_eq!(load(&temp).chart, "dots");

        // --no-save: nothing is written anywhere, same as the theme/layout case
        let nowhere = dir.join("never/ui.json");
        let mut saver = Saver::new(Some(nowhere.clone()), true, Prefs::of(&app));
        app.on_key(ratatui::crossterm::event::KeyCode::Char('c'));
        assert!(!saver.note(Prefs::of(&app)));
        assert!(!nowhere.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #51, 2026-09-21: `v` (the redesigned page 1) persists the same way - same shape as `c`.
    /// #191, 2026-09-22: the DEFAULT is now the new (cost + loadout) page, so `v` opts OUT to
    /// the classic grid and it is `"old"` that gets written. Kept, not deleted: a compiled
    /// default worth choosing is worth a test, and this one is the only place that proves the
    /// key still round-trips through the file in both directions.
    #[test]
    fn the_v_key_toggles_dash_and_persists_like_chart_lines_does() {
        let dir = std::env::temp_dir().join(format!("lss-prefs-dash-{}", std::process::id()));
        let (owner, temp) = (dir.join("owner/ui.json"), dir.join("temp/ui.json"));
        save(&owner, &Prefs::default());
        let before = std::fs::read_to_string(&owner).unwrap();

        let mut app = crate::ui::App::default();
        assert!(app.dash, "#191: the compiled default is the NEW (cost + loadout) page 1 - the owner compared both and chose it, so a stranger must land on it too");
        assert_eq!(Prefs::of(&app).page1, "new", "and the default is written as `new`, not as an empty string");
        let mut saver = Saver::new(Some(temp.clone()), false, Prefs::of(&app));
        app.on_key(ratatui::crossterm::event::KeyCode::Char('v'));
        assert!(!app.dash, "v opts out to the classic grid");
        assert!(saver.note(Prefs::of(&app)));
        assert_eq!(load(&temp).page1, "old");
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), before, "the owner's file is untouched");

        // and back: `v` is a toggle, not a one-way door, and the return trip persists too
        app.on_key(ratatui::crossterm::event::KeyCode::Char('v'));
        assert!(app.dash, "v toggles back to the new page 1");
        assert!(saver.note(Prefs::of(&app)));
        assert_eq!(load(&temp).page1, "new");

        let nowhere = dir.join("never/ui.json");
        let mut saver = Saver::new(Some(nowhere.clone()), true, Prefs::of(&app));
        app.on_key(ratatui::crossterm::event::KeyCode::Char('v'));
        assert!(!saver.note(Prefs::of(&app)));
        assert!(!nowhere.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #191: the upgrade path. A `ui.json` written before `page1` existed has no such key, and
    /// `#[serde(default)]` gives it `""` - that must mean "no opinion, use the compiled
    /// default" (the new page), never "old". Read the other way, every existing install would
    /// have been silently pinned to the page the owner rejected, which is the exact bug this
    /// card was filed for, just moved from the source into the config file.
    #[test]
    fn an_unset_page1_means_the_compiled_default_and_only_an_explicit_old_opts_out() {
        let mut app = crate::ui::App::default();

        Prefs { page1: String::new(), ..Prefs::default() }.apply(&mut app);
        assert!(app.dash, "a pre-#191 file with no page1 key lands on the default (new) page");

        Prefs { page1: "old".into(), ..Prefs::default() }.apply(&mut app);
        assert!(!app.dash, "an explicit `old` is the only thing that opts out");

        Prefs { page1: "new".into(), ..Prefs::default() }.apply(&mut app);
        assert!(app.dash, "an explicit `new` stays on the new page");
    }

    #[test]
    fn lss_chart_env_wins_over_the_saved_pref_which_wins_over_the_dots_default() {
        // no env: the pref decides
        assert!(!resolve_chart_lines(None, false), "no env, pref says dots");
        assert!(resolve_chart_lines(None, true), "no env, pref says lines");
        // env set: it wins regardless of what the pref says
        assert!(resolve_chart_lines(Some("lines"), false), "env says lines even though the pref says dots");
        assert!(!resolve_chart_lines(Some("dots"), true), "env says dots even though the pref says lines");
        // an unrecognised env value is not a signal either way: falls through to the pref
        assert!(resolve_chart_lines(Some("bogus"), true));
        assert!(!resolve_chart_lines(Some(""), false));
    }
}
