//! `scripts/hooks/pre-commit-privacy`, driven for real in a throw-away git repo (card #148):
//! a file that is staged and then deleted from the WORKING TREE must still block the commit,
//! because the hook reads the INDEX everywhere else (`git show ":$f"`) - the old line tested the
//! working tree, so the private word sailed through inside the index.
//!
//! Hermetic: builds its own repo with the real hook and the committed word list; needs `git` and
//! `bash` on PATH and touches nothing in this checkout.

use std::path::{Path, PathBuf};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;
use std::process::Command;

/// A throw-away repo with the real hook installed and the committed fallback word list present,
/// so the hook scans exactly what a fresh clone would.
fn scratch_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lss-hook-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts/hooks")).unwrap();
    std::fs::create_dir_all(dir.join("packaging")).unwrap();
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    std::fs::copy(repo().join("scripts/hooks/pre-commit-privacy"), dir.join("scripts/hooks/pre-commit-privacy")).unwrap();
    // the hook shells out to privacy-check.sh: a real clone has it, so the fixture must too
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    std::fs::copy(repo().join("packaging/privacy-words.example"), dir.join("packaging/privacy-words.example")).unwrap();
    // install it the way its own header says: a real `pre-commit` in .git/hooks
    let hook = dir.join(".git/hooks/pre-commit");
    exec_file::write_exec(&hook, "#!/bin/sh\nexec bash \"$(git rev-parse --show-toplevel)/scripts/hooks/pre-commit-privacy\"\n");
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(&dir).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "--no-verify", "-m", "seed"]);
    dir
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// `git commit` in the scratch repo; returns (exit code, output). The hook runs for real.
fn commit(dir: &Path, message: &str) -> (i32, String) {
    let out = Command::new("git").args(["commit", "-m", message]).current_dir(dir).output().expect("git runs");
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn a_staged_then_deleted_file_still_blocks_the_commit() {
    // the word from the committed example list, assembled at run time so this file is not itself
    // a hit when the repo's own export scans it
    let word = ["zebra", "host", "77"].concat();
    let dir = scratch_repo("staged-deleted");

    // CONTROL: a plain staged file with the private word blocks (the hook works at all)
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    let live = dir.join("docs/live.md");
    std::fs::write(&live, format!("{word}\n")).unwrap();
    let git_add = |p: &str| {
        let out = Command::new("git").args(["add", p]).current_dir(&dir).output().expect("git runs");
        assert!(out.status.success(), "git add {p}");
    };
    git_add("docs/live.md");
    let (code, text) = commit(&dir, "control");
    assert_ne!(code, 0, "the control must be blocked:\n{text}");
    assert!(text.contains(&word) || text.to_lowercase().contains("private"), "the block must name the hit:\n{text}");
    // UNSTAGE the control file (the commit was refused, so there is nothing to reset to): the
    // index must be clean before the bypass case, or its own staged word would mask the mutation
    // (the working-tree test would still find live.md and block for the WRONG reason).
    let _ = Command::new("git").args(["reset", "-q", "HEAD", "--", "docs/live.md"]).current_dir(&dir).output();
    std::fs::remove_file(&live).unwrap();

    // BYPASS reproducer (card #148): stage it, then DELETE it from the working tree. The index
    // still holds the word, so the commit must still be blocked.
    let ghost = dir.join("docs/ghost.md");
    std::fs::write(&ghost, format!("{word}\n")).unwrap();
    git_add("docs/ghost.md");
    std::fs::remove_file(&ghost).unwrap(); // gone from the tree, still in the index
    // assert the precondition: the index really does still carry it
    let show = Command::new("git").args(["show", ":docs/ghost.md"]).current_dir(&dir).output().expect("git runs");
    assert!(show.status.success() && String::from_utf8_lossy(&show.stdout).contains(&word), "precondition: the index must still hold the word");

    let (code, text) = commit(&dir, "bypass");
    assert_ne!(code, 0, "a staged-then-deleted file with a private word MUST be blocked (card #148):\n{text}");
    assert!(text.contains(&word) || text.to_lowercase().contains("private"), "the block must name the hit:\n{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scripts_install_privacy_hook_actually_wires_up_the_block() {
    // card #166: the hook FILE always existed, but nothing in a fresh clone ever ran it - no
    // README line, no installer step, `pre-commit-privacy` mentioned in exactly two files (the
    // hook itself and its own logic test). This proves the DOCUMENTED install step - the real
    // script, not a hand-written .git/hooks/pre-commit like scratch_repo() above - actually
    // wires the hook into a repo that starts with NONE installed.
    let word = ["zebra", "host", "77"].concat();
    let dir = std::env::temp_dir().join(format!("lss-hook-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts/hooks")).unwrap();
    std::fs::create_dir_all(dir.join("packaging")).unwrap();
    std::fs::copy(repo().join("scripts/hooks/pre-commit-privacy"), dir.join("scripts/hooks/pre-commit-privacy")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    std::fs::copy(repo().join("scripts/install-privacy-hook.sh"), dir.join("scripts/install-privacy-hook.sh")).unwrap();
    std::fs::copy(repo().join("packaging/privacy-words.example"), dir.join("packaging/privacy-words.example")).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(&dir).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "--no-verify", "-m", "seed"]);

    // precondition: NOTHING is installed yet - a fresh clone's actual starting state
    assert!(!dir.join(".git/hooks/pre-commit").exists(), "a fresh clone must start with no hook installed");
    std::fs::write(dir.join("docs.md"), format!("{word}\n")).unwrap();
    git(&["add", "docs.md"]);
    let (code, _) = commit(&dir, "unprotected");
    assert_eq!(code, 0, "before install, an ordinary commit is NOT blocked (this is the gap #166 exists to close)");
    git(&["reset", "-q", "--hard", "HEAD~1"]); // undo the (correctly, at this point) unblocked commit

    // run the documented install step
    let install = Command::new("bash").arg(dir.join("scripts/install-privacy-hook.sh")).current_dir(&dir).output().expect("bash runs");
    assert!(install.status.success(), "scripts/install-privacy-hook.sh failed:\n{}{}",
        String::from_utf8_lossy(&install.stdout), String::from_utf8_lossy(&install.stderr));
    assert!(dir.join(".git/hooks/pre-commit").exists(), "the install step must create .git/hooks/pre-commit");

    // now the same commit must be blocked
    std::fs::write(dir.join("docs.md"), format!("{word}\n")).unwrap();
    git(&["add", "docs.md"]);
    let (code, text) = commit(&dir, "protected");
    assert_ne!(code, 0, "after scripts/install-privacy-hook.sh, the same commit must now be blocked:\n{text}");
    assert!(text.contains(&word) || text.to_lowercase().contains("private"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_clean_staged_set_commits() {
    // the hook must not block an ordinary commit: no private word anywhere
    let dir = scratch_repo("clean");
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/ok.md"), "an ordinary sentence with no private word\n").unwrap();
    let out = Command::new("git").args(["add", "docs/ok.md"]).current_dir(&dir).output().expect("git runs");
    assert!(out.status.success());
    let (code, text) = commit(&dir, "clean");
    assert_eq!(code, 0, "a clean commit must go through:\n{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_commit_from_a_worktree_is_scanned_with_the_main_checkouts_word_list() {
    // card #180 gate 4, THE REAL EVENT: every worker commits from a git worktree; a worktree has
    // no .privacy-words (it is git-ignored, so it lives in the main checkout only), the hook fell
    // back to the generic committed list, and the owner's name shipped in eight comments. The
    // private word here is invented and assembled at run time, like the one above.
    let word = ["quokka", "seat", "42"].concat();
    let main = scratch_repo("worktree-main");
    std::fs::write(main.join(".privacy-words"), format!("{word}\n")).unwrap();
    let wt = main.with_file_name(format!("lss-hook-worktree-wt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&wt);
    let out = Command::new("git").args(["worktree", "add", "-q", "-b", "w", wt.to_str().unwrap()]).current_dir(&main).output().expect("git runs");
    assert!(out.status.success(), "worktree add: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!wt.join(".privacy-words").exists(), "precondition: the worktree has no list of its own");

    std::fs::create_dir_all(wt.join("docs")).unwrap();
    std::fs::write(wt.join("docs/note.md"), format!("asked by {word}\n")).unwrap();
    let out = Command::new("git").args(["add", "docs/note.md"]).current_dir(&wt).output().expect("git runs");
    assert!(out.status.success());
    let (code, text) = commit(&wt, "from a worktree");
    assert_ne!(code, 0, "a private word in the main checkout's list must block a worktree commit:\n{text}");
    assert!(text.contains("the main checkout's list"), "the hook must say which list it used:\n{text}");

    // and a clean change from the same worktree still commits
    let _ = Command::new("git").args(["reset", "-q", "HEAD", "--", "docs/note.md"]).current_dir(&wt).output();
    std::fs::write(wt.join("docs/note.md"), "nothing private here\n").unwrap();
    let _ = Command::new("git").args(["add", "docs/note.md"]).current_dir(&wt).output();
    let (code, text) = commit(&wt, "clean from a worktree");
    assert_eq!(code, 0, "{text}");
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&main);
}
