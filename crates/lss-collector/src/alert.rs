//! Alert sink: runs `alert_cmd <severity> <message>` on a worker thread so a slow ssh to
//! the seat never delays a poll. The script prints `delivered=1` when a leg got through NOW;
//! mail it could not hand over is spooled by the script, retried on every later call and by
//! the `Flush` job, and the ids that got through late come back in `mail-delivered.ids`.

use crate::cmd::{self, Guard};
use crate::db::Db;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum Job {
    Alert { id: i64, severity: String, message: String },
    /// `alert_cmd --flush`: retry spooled mail although there is no new alert
    Flush,
}

pub fn run_loop(alert_cmd: String, state_dir: PathBuf, rx: Receiver<Job>, db: Arc<Mutex<Db>>) {
    let state = state_dir.to_string_lossy().into_owned();
    for job in rx {
        // a fresh guard per job: one stuck delivery must not block the next alert
        let guard = Guard::default();
        match job {
            Job::Alert { id, severity, message } => {
                let id_text = id.to_string();
                let env = [("LSS_STATE_DIR", state.as_str()), ("LSS_ALERT_ID", id_text.as_str())];
                let (delivered, summary) = match cmd::run_env(&guard, &alert_cmd, &[&severity, &message], &env, Duration::from_secs(150)) {
                    Ok(o) => {
                        if !o.success {
                            eprintln!("alert sink exited non-zero: {}", o.stderr.trim());
                        }
                        (o.stdout.contains("delivered=1"), last_line(&o.stdout))
                    }
                    Err(e) => {
                        eprintln!("alert sink failed: {e}");
                        (false, String::new())
                    }
                };
                eprintln!("alert #{id} [{severity}] delivered={delivered} {message}  ({summary})");
                if delivered {
                    mark(&db, id);
                }
            }
            Job::Flush => match cmd::run_env(&guard, &alert_cmd, &["--flush"], &[("LSS_STATE_DIR", state.as_str())], Duration::from_secs(150)) {
                Ok(o) => {
                    let summary = last_line(&o.stdout);
                    // silent while there is nothing spooled: this runs every few minutes
                    if !summary.contains("flushed=0 spool=0") {
                        eprintln!("alert flush: {summary}");
                    }
                }
                Err(e) => eprintln!("alert flush failed: {e}"),
            },
        }
        for id in take_late_deliveries(&state_dir) {
            eprintln!("alert #{id} delivered late from the mail spool");
            mark(&db, id);
        }
    }
}

fn mark(db: &Arc<Mutex<Db>>, id: i64) {
    let db = db.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = db.mark_delivered(id) {
        eprintln!("alert {id}: mark delivered: {e}");
    }
}

fn last_line(stdout: &str) -> String {
    stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

/// Alert ids whose spooled mail has been handed over since the last look. The file is renamed
/// before it is read so an id appended meanwhile is never lost.
pub fn take_late_deliveries(state_dir: &Path) -> Vec<i64> {
    let (file, taken) = (state_dir.join("mail-delivered.ids"), state_dir.join("mail-delivered.ids.taken"));
    if std::fs::rename(&file, &taken).is_err() {
        return Vec::new();
    }
    let ids = std::fs::read_to_string(&taken).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect();
    let _ = std::fs::remove_file(&taken);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_deliveries_are_read_once() {
        let dir = std::env::temp_dir().join(format!("lss-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(take_late_deliveries(&dir).is_empty());
        std::fs::write(dir.join("mail-delivered.ids"), "41\n-\n\n107\n").unwrap();
        assert_eq!(take_late_deliveries(&dir), vec![41, 107]);
        assert!(take_late_deliveries(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worker_runs_the_sink_marks_delivery_and_flushes() {
        let dir = std::env::temp_dir().join(format!("lss-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = dir.join("sink.sh");
        // records its arguments and environment; "delivers" page alerts only; a flush hands over id 1 late
        crate::exec_file::write_exec(&sink, "#!/bin/sh\necho \"$LSS_ALERT_ID|$1|$2\" >>\"$LSS_STATE_DIR/calls\"\nif [ \"$1\" = --flush ]; then echo 1 >>\"$LSS_STATE_DIR/mail-delivered.ids\"; echo 'lss-alert: flush flushed=1 spool=0'; exit 0; fi\n[ \"$1\" = page ] && echo 'lss-alert: mail=OK banner=OK delivered=1 spool=0' || echo 'lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=1'\n");

        let db = Arc::new(Mutex::new(Db::memory()));
        let ev = |sev| lss_core::rules::AlertEvent { ts: 1, rule: "test_alert".into(), severity: sev, message: "m".into(), recovered: false };
        let (a, b) = {
            let d = db.lock().unwrap();
            (d.insert_alert(&ev(lss_core::rules::Severity::Info)).unwrap(), d.insert_alert(&ev(lss_core::rules::Severity::Page)).unwrap())
        };
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Job::Alert { id: a, severity: "info".into(), message: "it's spooled".into() }).unwrap();
        tx.send(Job::Alert { id: b, severity: "page".into(), message: "paged".into() }).unwrap();
        drop(tx);
        run_loop(sink.to_string_lossy().into_owned(), dir.clone(), rx, db.clone());
        let delivered = |id: i64| db.lock().unwrap().recent_alerts(20).unwrap().into_iter().find(|r| r.id == id).unwrap().delivered;
        assert!(!delivered(a), "spooled is not delivered");
        assert!(delivered(b));

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Job::Flush).unwrap();
        drop(tx);
        run_loop(sink.to_string_lossy().into_owned(), dir.clone(), rx, db.clone());
        assert!(delivered(a), "the flush reported alert 1 as handed over: the row follows");
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        assert_eq!(calls, format!("{a}|info|it's spooled\n{b}|page|paged\n|--flush|\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
