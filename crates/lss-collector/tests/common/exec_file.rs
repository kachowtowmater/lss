//! card #336: make a file that will be EXECUTED without this process ever holding a write fd to it.
//!
//! The flake: a test wrote a script with `std::fs::write` and ran it at once, and on Linux the
//! exec failed now and then with `Text file busy (os error 26)`. Cargo runs tests as threads of
//! ONE process; while this thread held the script open for writing, another test thread forked
//! (to spawn anything) and its child inherited that open fd. Until that child reached its own
//! exec (the fd is close-on-exec), the kernel saw the script as open for writing, and exec()
//! of it is refused with ETXTBSY. A temp file + rename does not help (the inode is the same, and
//! it is the inode that is busy), and a sleep only makes it rarer.
//!
//! So the bytes are written by a short-lived CHILD process (`sh -c 'cat > file'`): the only fd
//! ever open for writing on the file lives in that child, which no other test thread's fork can
//! inherit. By the time `wait()` returns, it is closed, and the file can be executed.
#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `body` to `path` (parents created) and make it executable (0755).
pub fn write_exec(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "write_exec"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("/bin/sh runs");
    child.stdin.take().expect("piped stdin").write_all(body.as_bytes()).unwrap();
    // stdin is dropped above: cat sees EOF, writes, and exits
    let status = child.wait().unwrap();
    assert!(status.success(), "writing {} failed: {status}", path.display());
}

/// Copy `from` to `to` (parents created) and make it executable (0755), written by `cp` in a child.
pub fn copy_exec(from: &Path, to: &Path) {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let status = Command::new("/bin/sh")
        .args(["-c", "cp \"$1\" \"$2\" && chmod 755 \"$2\"", "copy_exec"])
        .arg(from)
        .arg(to)
        .status()
        .expect("/bin/sh runs");
    assert!(status.success(), "copying {} to {} failed: {status}", from.display(), to.display());
}
