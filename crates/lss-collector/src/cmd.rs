//! Subprocess execution with a hard timeout. A wedged GPU can leave `nvidia-smi` stuck in
//! uninterruptible sleep; the monitor must keep polling through exactly that situation, so
//! no external command is ever waited on without a deadline, and a command that is still
//! stuck from the previous poll is not started a second time.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Upper bound on captured stdout. The first version of the Xid back-fill read three million
/// kernel-journal lines into one String and hit the unit's MemoryMax; nothing may do that again.
const MAX_CAPTURE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct Guard(Arc<AtomicBool>);

pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// SIGKILL to the child's whole process group: `sh -c "journalctl … | grep …"` must not leave
/// a journalctl behind when only the shell is killed.
fn kill_group(pid: u32) {
    // SAFETY: plain syscall, no memory involved; a stale pid at worst returns ESRCH.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

pub fn run(guard: &Guard, program: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput, String> {
    run_env(guard, program, args, &[], timeout)
}

pub fn run_env(guard: &Guard, program: &str, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Result<CmdOutput, String> {
    if guard.0.swap(true, Ordering::SeqCst) {
        return Err(format!("{program}: previous invocation is still stuck"));
    }
    let mut child = match Command::new(program)
        .args(args)
        .envs(env.iter().copied())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            guard.0.store(false, Ordering::SeqCst);
            return Err(format!("{program}: {e}"));
        }
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let flag = guard.0.clone();
    std::thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        // stderr is drained on its own thread so neither pipe can fill up and block the child
        let stderr = child.stderr.take();
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut s);
            }
            s
        });
        if let Some(mut o) = child.stdout.take() {
            let mut bytes = Vec::new();
            let _ = (&mut o).take(MAX_CAPTURE).read_to_end(&mut bytes);
            // keep draining so the child can finish, but never hold more than the cap
            let _ = std::io::copy(&mut o, &mut std::io::sink());
            out = String::from_utf8_lossy(&bytes).into_owned();
        }
        if let Ok(s) = err_thread.join() {
            err = s;
        }
        let success = child.wait().map(|s| s.success()).unwrap_or(false);
        flag.store(false, Ordering::SeqCst);
        let _ = tx.send(CmdOutput { stdout: out, stderr: err, success });
    });
    match rx.recv_timeout(timeout) {
        Ok(o) => Ok(o),
        Err(_) => {
            // best effort; a process in D state ignores it and the guard stays set until it dies
            kill_group(pid);
            Err(format!("{program}: timed out after {}s", timeout.as_secs()))
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct StreamStats {
    pub lines: usize,
    /// true when `max_lines` was reached and the rest of the output was not read
    pub truncated: bool,
}

/// Runs a command and hands its stdout to `on_line` ONE LINE AT A TIME, on the calling thread.
/// Nothing is accumulated here: memory is one line, however much the command prints. Stops
/// after `max_lines`; the whole process group is killed at the deadline or on truncation.
pub fn stream_lines(program: &str, args: &[&str], timeout: Duration, max_lines: usize, mut on_line: impl FnMut(&str)) -> Result<StreamStats, String> {
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let pid = child.id();
    let timed_out = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let flag = timed_out.clone();
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            flag.store(true, Ordering::SeqCst);
            kill_group(pid);
        }
    });
    let mut stats = StreamStats { lines: 0, truncated: false };
    if let Some(out) = child.stdout.take() {
        let mut reader = BufReader::new(out);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if stats.lines >= max_lines {
                stats.truncated = true;
                kill_group(pid);
                break;
            }
            stats.lines += 1;
            on_line(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']));
        }
    }
    let _ = child.wait();
    let _ = done_tx.send(());
    let _ = watchdog.join();
    if timed_out.load(Ordering::SeqCst) {
        return Err(format!("{program}: timed out after {}s ({} lines read)", timeout.as_secs(), stats.lines));
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_status() {
        let g = Guard::default();
        let o = run(&g, "sh", &["-c", "echo out; echo err >&2; exit 3"], Duration::from_secs(5)).unwrap();
        assert_eq!(o.stdout, "out\n");
        assert_eq!(o.stderr, "err\n");
        assert!(!o.success);
        assert!(run(&g, "sh", &["-c", "true"], Duration::from_secs(5)).unwrap().success, "guard is released");
    }

    #[test]
    fn a_hung_command_times_out_and_is_not_started_twice() {
        let g = Guard::default();
        let t0 = std::time::Instant::now();
        let e = run(&g, "sleep", &["30"], Duration::from_millis(200)).err().unwrap();
        assert!(e.contains("timed out"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        // killed promptly → guard released shortly after
        std::thread::sleep(Duration::from_millis(300));
        assert!(run(&g, "true", &[], Duration::from_secs(5)).is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_timeout_kills_the_whole_pipeline_not_just_the_shell() {
        let g = Guard::default();
        let pid = std::process::id();
        // argv[0] of the sleep carries a marker we can look for in /proc afterwards
        let script = format!("(exec -a lss-test-marker-{pid} sleep 41) | cat");
        let e = run(&g, "bash", &["-c", &script], Duration::from_millis(300)).err().unwrap();
        assert!(e.contains("timed out"), "{e}");
        std::thread::sleep(Duration::from_millis(300));
        // the [r] keeps this grep from finding its own command line
        let find = format!("grep -l 'lss-test-marke[r]-{pid}' /proc/[0-9]*/cmdline 2>/dev/null | wc -l");
        let left = run(&Guard::default(), "sh", &["-c", &find], Duration::from_secs(5)).unwrap();
        assert_eq!(left.stdout.trim(), "0", "the sleep behind the pipe must be gone too");
    }

    #[test]
    fn environment_is_passed_to_the_child() {
        let g = Guard::default();
        let o = run_env(&g, "sh", &["-c", "echo $LSS_ALERT_ID"], &[("LSS_ALERT_ID", "77")], Duration::from_secs(5)).unwrap();
        assert_eq!(o.stdout, "77\n");
    }

    #[test]
    fn stream_lines_sees_every_line_without_holding_them() {
        let mut sum = 0_u64;
        let stats = stream_lines("sh", &["-c", "seq 1 200000"], Duration::from_secs(30), usize::MAX, |l| sum += l.parse::<u64>().unwrap()).unwrap();
        assert_eq!(stats, StreamStats { lines: 200_000, truncated: false });
        assert_eq!(sum, 200_000 * 200_001 / 2);
    }

    #[test]
    fn stream_lines_stops_at_the_cap_and_at_the_deadline() {
        let mut seen = Vec::new();
        let stats = stream_lines("sh", &["-c", "yes xid"], Duration::from_secs(30), 5, |l| seen.push(l.to_string())).unwrap();
        assert_eq!(stats, StreamStats { lines: 5, truncated: true });
        assert_eq!(seen, vec!["xid"; 5]);
        let t0 = std::time::Instant::now();
        let e = stream_lines("sh", &["-c", "echo one; sleep 30 | cat"], Duration::from_millis(300), 100, |_| {}).unwrap_err();
        assert!(e.contains("timed out") && e.contains("1 lines read"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(stream_lines("/nonexistent/lss-nope", &[], Duration::from_secs(1), 1, |_| {}).is_err());
    }

    #[test]
    fn missing_binary_is_an_error_not_a_panic() {
        let g = Guard::default();
        assert!(run(&g, "/nonexistent/lss-nope", &[], Duration::from_secs(1)).is_err());
        assert!(run(&g, "true", &[], Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn large_output_does_not_deadlock() {
        let g = Guard::default();
        let o = run(&g, "sh", &["-c", "head -c 2000000 /dev/zero | tr '\\0' 'x'"], Duration::from_secs(20)).unwrap();
        assert_eq!(o.stdout.len(), 2_000_000);
    }

    #[test]
    fn capture_is_capped_but_the_child_still_completes() {
        let g = Guard::default();
        let o = run(&g, "sh", &["-c", "head -c 20000000 /dev/zero | tr '\\0' 'x'"], Duration::from_secs(30)).unwrap();
        assert_eq!(o.stdout.len() as u64, MAX_CAPTURE);
        assert!(o.success);
    }
}
