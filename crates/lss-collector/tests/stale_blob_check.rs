//! `scripts/stale-blob-check.sh`, driven for real against purpose-built git repositories, plus
//! the one real commit that made it necessary - card #201.
//!
//! WHY THIS NEEDS TEETH: on 2026-09-22 a single commit set two files' blobs back to versions they
//! had held before their most recent change, through a bad `git stash` resolution in a working
//! tree shared by eight agents. It left a test RED ON MAIN for about 29 hours and undid a fix so
//! that test_admission_integration.py ran in 121.59s instead of 1.00s. Nothing noticed either.
//! A guard against that has to be proven to FIRE - the negative control below runs it against the
//! actual commit, 9e5f317 - and proven not to fire on ordinary work, or it would be turned off
//! within a week.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn script() -> PathBuf {
    repo().join("scripts/stale-blob-check.sh")
}

/// A throwaway git repository whose history is written one commit at a time.
struct Hist {
    dir: PathBuf,
}

impl Hist {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-stale-blob-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = Hist { dir };
        h.git(&["init", "-q"]);
        h
    }

    fn git(&self, args: &[&str]) -> Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out
    }

    /// Write `body` to `file` and commit it with `message`.
    fn commit(&self, file: &str, body: &str, message: &str) {
        std::fs::write(self.dir.join(file), body).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", message]);
    }

    /// Run the check against this repository.
    fn check(&self, args: &[&str]) -> Output {
        Command::new("bash")
            .arg(script())
            .args(args)
            .current_dir(&self.dir)
            .output()
            .expect("bash runs")
    }
}

impl Drop for Hist {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn a_commit_that_sets_a_file_back_to_a_version_it_already_left_behind_is_refused() {
    let h = Hist::new("resurrect");
    h.commit("f.txt", "one\n", "v1");
    h.commit("f.txt", "two\n", "v2");
    h.commit("f.txt", "three\n", "v3");
    // the defect: back to v1, saying nothing about it
    h.commit("f.txt", "one\n", "an ordinary-looking commit");

    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(1), "a silent resurrection must fail: {}", text(&out));
    let t = text(&out);
    assert!(t.contains("RESURRECTED BLOB"), "{t}");
    assert!(t.contains("f.txt"), "the finding must name the file: {t}");
    assert!(t.contains("moved the file BACKWARDS"), "{t}");
    assert!(t.contains("Blob-Revert:"), "it must say how to declare a deliberate one: {t}");
}

#[test]
fn a_declared_revert_is_allowed_and_an_undeclared_one_is_not() {
    // The decision this card asked for, made explicit: restoring an old version ON PURPOSE is a
    // normal thing to do and is indistinguishable from the defect by content alone. The
    // difference is whether anyone MEANT it, so the commit has to say so - and only then.
    let h = Hist::new("declared");
    h.commit("f.txt", "one\n", "v1");
    h.commit("f.txt", "two\n", "v2");

    // (a) git revert writes its own marker, and that is accepted: the tool wrote it, the act was
    //     deliberate, and nobody should have to annotate a revert by hand.
    let head = String::from_utf8_lossy(&h.git(&["rev-parse", "HEAD"]).stdout).trim().to_string();
    h.git(&["revert", "--no-edit", &head]);
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(0), "git revert must be allowed: {}", text(&out));
    assert!(String::from_utf8_lossy(&h.git(&["log", "-1", "--format=%B"]).stdout).contains("This reverts commit"));

    // (b) a hand-made restore with the declaration
    h.commit("f.txt", "two\n", "back to v2\n\nBlob-Revert: v1 turned out to be the broken one");
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(0), "a declared hand revert must be allowed: {}", text(&out));

    // (c) the same restore WITHOUT the declaration - the card-#201 shape
    h.commit("f.txt", "one\n", "tidy up");
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(1), "an undeclared restore must still fail: {}", text(&out));

    // (d) and the declaration needs a REASON, so that writing it is a decision rather than a
    //     reflex incantation
    h.commit("f.txt", "two\n", "quiet\n\nBlob-Revert:");
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(1), "an empty Blob-Revert: must not buy a pass: {}", text(&out));
}

#[test]
fn ordinary_work_a_deletion_and_a_merge_are_not_findings() {
    let h = Hist::new("ordinary");
    h.commit("f.txt", "one\n", "v1");
    h.commit("f.txt", "two\n", "v2");
    h.commit("g.txt", "new file\n", "add g");
    h.commit("f.txt", "three\n", "v3");
    let out = h.check(&[&format!("{}..HEAD", first_commit(&h))]);
    assert_eq!(out.status.code(), Some(0), "ordinary forward work must be clean: {}", text(&out));

    // a deletion restores nothing
    h.git(&["rm", "-q", "g.txt"]);
    h.git(&["commit", "-qm", "drop g"]);
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(0), "a deletion is not a resurrection: {}", text(&out));

    // a MERGE legitimately reintroduces the other side's blobs, so it is skipped outright - a
    // branch that kept the older version of a file is not a stash accident.
    h.git(&["checkout", "-q", "-b", "side", "HEAD~3"]);
    h.commit("side.txt", "from the side\n", "side work");
    h.git(&["checkout", "-q", "-"]);
    h.git(&["merge", "--no-ff", "-m", "merge side", "side"]);
    let out = h.check(&["HEAD"]);
    assert_eq!(out.status.code(), Some(0), "a merge must not be judged against one parent: {}", text(&out));
    assert!(text(&out).contains("0 commit(s) checked"), "a merge should be skipped, not examined: {}", text(&out));
}

fn first_commit(h: &Hist) -> String {
    let out = h.git(&["rev-list", "--max-parents=0", "HEAD"]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_bad_argument_is_refused_rather_than_silently_checking_nothing() {
    let h = Hist::new("args");
    h.commit("f.txt", "one\n", "v1");
    let out = h.check(&["no-such-ref-201"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("is not a commit"), "{}", text(&out));
}

/// THE NEGATIVE CONTROL THE CARD REQUIRES (item 2): the real commit, in the real repository.
/// A guard that has never fired on the thing it was built for has proven nothing.
///
/// Skipped rather than failed when the commit is not reachable, because this same suite runs
/// inside the PUBLIC EXPORT, which carries no .git at all (and the gate/ files below are dropped
/// from it besides). The skip is loud in the test's own output.
#[test]
fn the_real_2026_09_22_resurrection_is_caught() {
    let known = "9e5f317";
    let reachable = Command::new("git")
        .args(["cat-file", "-e", &format!("{known}^{{commit}}")])
        .current_dir(repo())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !reachable {
        eprintln!("skipping: {known} is not reachable here (no .git, or a shallow clone) - the synthetic cases above still pin the behaviour");
        return;
    }

    let out = Command::new("bash").arg(script()).arg(known).current_dir(repo()).output().expect("bash runs");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "the commit that caused card #201 must be caught: {t}");
    // both files named in the card - two test files under the excluded `gate/` tree - proven at
    // blob level there: each one's blob at 9e5f317 is byte-identical to its blob at the parent of
    // the commit that had last removed it (d5f3363^ and d15ec6d^ respectively).
    assert!(t.contains("gate/tests/test_shadow.py"), "{t}");
    assert!(t.contains("gate/tests/conftest.py"), "{t}");
    assert!(t.contains("2 resurrected blob(s)"), "both files, and only those two: {t}");

    // ...and the commit right before it, which changed the same files legitimately, is clean -
    // or this test would pass on a check that flagged everything.
    let out = Command::new("bash").arg(script()).arg("c7f8bf4").current_dir(repo()).output().expect("bash runs");
    assert_eq!(out.status.code(), Some(0), "9e5f317's own parent must be clean: {}", text(&out));
}
