//! card #336: a file written by `exec_file::write_exec` can be executed at once, even while other
//! threads of this process are forking - the exact situation of `cargo test`, whose tests are
//! threads of one process. With a plain `std::fs::write` + chmod the same exec fails now and then
//! with ETXTBSY ("Text file busy", os error 26): another thread's child inherited the write fd.

#[path = "common/exec_file.rs"]
mod exec_file;

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn a_script_written_by_write_exec_runs_at_once_while_other_threads_fork() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIPPED: ETXTBSY on a freshly written script is the Linux behaviour this guards");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lss-exec-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    // forkers: keep forking (spawning /bin/true) the whole time, like the other tests do
    let forkers: Vec<_> = (0..8)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("/bin/true").status();
                }
            })
        })
        .collect();
    let (busy, ran) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let writers: Vec<_> = (0..16)
        .map(|t| {
            let (dir, busy, ran) = (dir.clone(), busy.clone(), ran.clone());
            std::thread::spawn(move || {
                for i in 0..40 {
                    let p = dir.join(format!("s-{t}-{i}"));
                    exec_file::write_exec(&p, "#!/bin/sh\nexit 0\n");
                    match Command::new(&p).status() {
                        Ok(s) => {
                            assert!(s.success(), "{} ran but failed: {s}", p.display());
                            ran.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) if e.raw_os_error() == Some(26) => {
                            busy.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => panic!("{}: {e}", p.display()),
                    }
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for f in forkers {
        f.join().unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
    let (busy, ran) = (busy.load(Ordering::Relaxed), ran.load(Ordering::Relaxed));
    assert_eq!(busy, 0, "{busy} of {} freshly written scripts could not be executed: Text file busy (ETXTBSY)", busy + ran);
    assert_eq!(ran, 16 * 40);
}
