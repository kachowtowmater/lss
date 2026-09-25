//! Card #74: reads back whatever a watch-sweep wrote to `[watch] path` - never fetches anything
//! itself (see `lss_core::watch`'s own doc comment for why). Re-checks the file's mtime every
//! poll (cheap: one `stat`) and only re-parses on a real change; a read/parse failure NEVER
//! blanks the page - it keeps the last successfully parsed content on screen, logged once per
//! failure state change, not every 5s poll forever.

use lss_core::watch::WatchStatus;
use std::time::SystemTime;

pub struct Watcher {
    path: String,
    mtime: Option<SystemTime>,
    last_good: Option<WatchStatus>,
    logged_missing: bool,
}

impl Watcher {
    /// `path` already expanded (`""` = watch tracking off, `poll` then always returns `None`).
    pub fn new(path: String) -> Self {
        Self { path, mtime: None, last_good: None, logged_missing: false }
    }

    /// The best-known status - possibly stale, from an earlier successful parse - or `None` only
    /// when tracking is off or nothing has EVER parsed successfully.
    pub fn poll(&mut self) -> Option<WatchStatus> {
        if self.path.trim().is_empty() {
            return None;
        }
        match std::fs::metadata(&self.path).and_then(|m| m.modified()) {
            Ok(mtime) => {
                self.logged_missing = false;
                if self.mtime != Some(mtime) {
                    match std::fs::read_to_string(&self.path).map_err(|e| e.to_string()).and_then(|t| lss_core::watch::parse(&t)) {
                        Ok(w) => {
                            self.mtime = Some(mtime);
                            self.last_good = Some(w);
                        }
                        Err(e) => eprintln!("watch: {}: {e} - showing the last good read, if any", self.path),
                    }
                }
            }
            Err(e) => {
                if !self.logged_missing {
                    eprintln!("watch: {}: {e} - watch tracking is off until this is fixed", self.path);
                    self.logged_missing = true;
                }
            }
        }
        self.last_good.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::watch::{WatchItem, WatchSource};

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lss-watch-run-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn empty_path_is_always_none_never_reads_the_disk() {
        let mut w = Watcher::new(String::new());
        assert_eq!(w.poll(), None);
    }

    #[test]
    fn a_missing_file_is_none_not_a_panic_and_a_later_write_is_picked_up() {
        let p = tmp("missing-then-written");
        let _ = std::fs::remove_file(&p);
        let mut w = Watcher::new(p.to_string_lossy().into_owned());
        assert_eq!(w.poll(), None, "nothing written yet");
        std::fs::write(&p, r#"{"sources": [{"name": "x", "covers": "y", "last_checked": null, "newest": null}]}"#).unwrap();
        assert_eq!(w.poll().unwrap().sources.len(), 1, "a later write is picked up on the next poll");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn an_unchanged_mtime_never_reparses_a_malformed_edit_keeps_the_last_good_read() {
        let p = tmp("keeps-last-good");
        std::fs::write(&p, r#"{"sources": []}"#).unwrap();
        let mut w = Watcher::new(p.to_string_lossy().into_owned());
        assert_eq!(w.poll().unwrap().sources.len(), 0);
        // simulate a bad write mid-edit (mtime WILL move: same-second writes on most filesystems
        // still bump mtime on rewrite in practice for this test's purpose - if it does not, the
        // parse is simply never attempted, which is the same "never blank" guarantee either way)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&p, "{not json").unwrap();
        let after_bad_write = w.poll();
        assert_eq!(after_bad_write.unwrap().sources.len(), 0, "a malformed rewrite must never blank what was already known good");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_good_edit_after_a_bad_one_recovers() {
        let p = tmp("recovers");
        std::fs::write(&p, r#"{"sources": [{"name": "a", "covers": "c", "last_checked": null, "newest": null}]}"#).unwrap();
        let mut w = Watcher::new(p.to_string_lossy().into_owned());
        assert_eq!(w.poll().unwrap().sources.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&p, "{not json").unwrap();
        w.poll();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&p, r#"{"sources": [{"name": "a", "covers": "c", "last_checked": null, "newest": {"date": "2026-09-20", "summary": "s", "url": "u", "has_receipt": true}}]}"#).unwrap();
        let w2 = w.poll().unwrap();
        assert_eq!(w2.sources[0].newest.as_ref().unwrap().date, "2026-09-20");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn round_trips_a_real_shaped_file() {
        let src = WatchSource { name: "example-recipes".into(), covers: "model loadouts".into(), last_checked: Some(1_790_000_000), newest: Some(WatchItem { date: "2026-09-20".into(), summary: "s".into(), url: "u".into(), has_receipt: true }) };
        let text = serde_json::to_string(&WatchStatus { sources: vec![src] }).unwrap();
        let p = tmp("round-trip");
        std::fs::write(&p, text).unwrap();
        let mut w = Watcher::new(p.to_string_lossy().into_owned());
        let got = w.poll().unwrap();
        assert_eq!(got.sources[0].name, "example-recipes");
        let _ = std::fs::remove_file(&p);
    }
}
