//! `scripts/hooks/pre-push-scope`, driven against real throwaway git repositories (card #169).
//!
//! WHY THIS EXISTS, and why it is tested against real repos rather than mocked: on 2026-09-22
//! two different agents each landed a commit that REVERTED work they never touched, because
//! both branches were based on an older `main` and both were squashed or pushed relative to the
//! moved remote. One took out 50 files and 1826 lines including an entire fixture scrub; the
//! other silently removed an admission-safety cap. Both were recovered by somebody noticing
//! afterwards. A control that depends on noticing is not a control.
//!
//! The two tests below ARE those two cases, reproduced in miniature.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    /// An "upstream" repo plus a clone of it, with the hook script copied in.
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-push-scope-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("up")).unwrap();
        let s = Sandbox { dir };
        s.git(&["init", "-q", "--bare", "up"], &s.dir);
        std::fs::create_dir_all(s.dir.join("work")).unwrap();
        s.git(&["init", "-q", "."], &s.work());
        s.git(&["remote", "add", "origin", s.dir.join("up").to_str().unwrap()], &s.work());
        std::fs::create_dir_all(s.work().join("scripts/hooks")).unwrap();
        std::fs::copy(repo_root().join("scripts/hooks/pre-push-scope"), s.work().join("scripts/hooks/pre-push-scope")).unwrap();
        s.write("shared.txt", "line one\n");
        s.write("mine.txt", "mine\n");
        s.commit("base");
        s.git(&["push", "-q", "origin", "HEAD:refs/heads/main"], &s.work());
        // card #169: point the bare repo's HEAD at main. Git's own default branch name differs
        // by version (this seat's git says main, the Linux container's says master), so without
        // this a clone of the bare repo lands on an unborn `master` and every later push from it
        // is a non-fast-forward. That is what made the first version of these tests fail in the
        // container and pass here - found only because the test now asserts its OWN git calls.
        s.git(&["--git-dir", s.dir.join("up").to_str().unwrap(), "symbolic-ref", "HEAD", "refs/heads/main"], &s.dir);
        s.git(&["branch", "--set-upstream-to=origin/main"], &s.work());
        s
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn git(&self, args: &[&str], cwd: &Path) -> Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        // card #169: ASSERT the sandbox's own git commands succeed. The first version ignored
        // them, so when a sandbox step silently failed the hook looked like it had passed and I
        // went looking for a bug in the hook instead of in the test.
        assert!(
            out.status.success(),
            "sandbox git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn write(&self, rel: &str, body: &str) {
        std::fs::write(self.work().join(rel), body).unwrap();
    }

    fn commit(&self, msg: &str) {
        self.git(&["add", "-A"], &self.work());
        self.git(&["commit", "-q", "-m", msg], &self.work());
    }

    /// Land a commit on the remote's main from a SEPARATE clone - i.e. somebody else's work
    /// arriving while our branch was elsewhere.
    fn someone_else_lands(&self, rel: &str, body: &str, msg: &str) {
        let other = self.dir.join("other");
        if !other.exists() {
            self.git(&["clone", "-q", "-b", "main", self.dir.join("up").to_str().unwrap(), other.to_str().unwrap()], &self.dir);
        }
        std::fs::write(other.join(rel), body).unwrap();
        self.git(&["add", "-A"], &other);
        self.git(&["commit", "-q", "-m", msg], &other);
        self.git(&["push", "-q", "origin", "HEAD:main"], &other);
    }

    /// Run the hook exactly as git would, and return its combined output + exit code.
    fn hook(&self) -> (i32, String) {
        let out = Command::new("bash")
            .arg(self.work().join("scripts/hooks/pre-push-scope"))
            .current_dir(self.work())
            .env("LSS_PUSH_TARGET", "origin/main")
            .output()
            .expect("bash runs");
        (
            out.status.code().unwrap_or(-1),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_rebased_branch_passes() {
    let s = Sandbox::new("ok");
    s.someone_else_lands("shared.txt", "line one\ntheir new line\n", "their work");
    s.write("mine.txt", "mine, edited\n");
    s.commit("my work");
    s.git(&["fetch", "-q", "origin"], &s.work());
    s.git(&["rebase", "-q", "origin/main"], &s.work());
    let (code, text) = s.hook();
    assert_eq!(code, 0, "a branch that contains origin/main must push freely:\n{text}");
}

#[test]
fn a_stale_branch_that_would_undo_someone_elses_file_is_refused() {
    // CASE 1, reproduced: my own card-#149 push. The branch never touched shared.txt, but
    // pushing it from an older base would have reverted the line somebody else added.
    let s = Sandbox::new("stale");
    s.someone_else_lands("shared.txt", "line one\ntheir new line\n", "their work");
    s.write("mine.txt", "mine, edited\n");
    s.commit("my work, from a stale base");
    s.git(&["fetch", "-q", "origin"], &s.work());
    // deliberately NOT rebased
    let (code, text) = s.hook();
    assert_eq!(code, 1, "a stale branch must be refused:\n{text}");
    assert!(text.contains("REFUSING"), "{text}");
    assert!(text.contains("shared.txt"), "it must NAME the file it would undo:\n{text}");
    assert!(text.contains("git rebase"), "and say how to fix it:\n{text}");
    assert!(!text.contains("mine.txt"), "a file the branch DID edit is not a revert:\n{text}");
}

#[test]
fn the_silent_case_is_refused_too_a_deleted_line_in_a_file_nobody_renamed() {
    // CASE 2, reproduced: card #163 removing card #155's hard cap. The dangerous version is a
    // DELETION inside an existing file - silent, and invisible in a file listing.
    let s = Sandbox::new("silent");
    s.someone_else_lands("shared.txt", "line one\nHARD_CAP = 250000\n", "add the cap");
    s.write("mine.txt", "unrelated\n");
    s.commit("my unrelated work");
    s.git(&["fetch", "-q", "origin"], &s.work());
    let (code, text) = s.hook();
    assert_eq!(code, 1, "the silent deletion case must be refused too:\n{text}");
    // the file is named even though the loss is a single line inside it
    assert!(text.contains("shared.txt"), "{text}");
}

#[test]
fn the_bypass_is_explicit_and_says_so() {
    let s = Sandbox::new("bypass");
    s.someone_else_lands("shared.txt", "line one\ntheirs\n", "their work");
    // content must actually DIFFER from the base, or `git commit` refuses an empty commit and
    // the sandbox - not the hook - is what fails (the strict git assertion caught this)
    s.write("mine.txt", "mine, bypassed\n");
    s.commit("mine");
    s.git(&["fetch", "-q", "origin"], &s.work());
    let out = Command::new("bash")
        .arg(s.work().join("scripts/hooks/pre-push-scope"))
        .current_dir(s.work())
        .env("LSS_PUSH_TARGET", "origin/main")
        .env("SKIP_PUSH_SCOPE", "1")
        .output()
        .expect("bash runs");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "the bypass must work:\n{text}");
    assert!(text.contains("SKIPPED"), "and it must be loud, never silent:\n{text}");
}

#[test]
fn the_installer_wires_the_hook_into_this_checkouts_own_hooks_path() {
    let s = Sandbox::new("install");
    std::fs::copy(repo_root().join("scripts/install-push-hook.sh"), s.work().join("scripts/install-push-hook.sh")).unwrap();
    let out = Command::new("bash")
        .arg(s.work().join("scripts/install-push-hook.sh"))
        .current_dir(s.work())
        .output()
        .expect("bash runs");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    let hooks = String::from_utf8_lossy(
        &Command::new("git").args(["rev-parse", "--git-path", "hooks"]).current_dir(s.work()).output().unwrap().stdout,
    )
    .trim()
    .to_string();
    let installed = s.work().join(&hooks).join("pre-push");
    assert!(installed.exists(), "no pre-push hook at {}: {text}", installed.display());
    let body = std::fs::read_to_string(&installed).unwrap();
    assert!(body.contains("pre-push-scope"), "{body}");
}

#[test]
fn an_installed_wrapper_is_a_clean_no_op_in_a_repo_without_the_script() {
    // card #169, found by installing my own hook on this seat and reading where it landed:
    // git here uses a GLOBAL core.hooksPath, so `install-push-hook.sh` writes a wrapper that
    // every repository on the machine runs. The original wrapper was
    //     [ -x scripts/hooks/pre-push-scope ] && exec bash .../pre-push-scope
    // whose exit status in a repo WITHOUT that script is 1 - so a global install of it would
    // have blocked every push in every other checkout on the box, including the notes repo.
    // Both installers now write an `if ... fi; exit 0` wrapper. This test is the guard.
    for (installer, hook) in [
        ("scripts/install-push-hook.sh", "pre-push"),
        ("scripts/install-privacy-hook.sh", "pre-commit"),
    ] {
        let dir = std::env::temp_dir().join(format!("lss-wrapper-{hook}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        let sb = Sandbox { dir: dir.clone() };
        sb.git(&["init", "-q", "."], &dir);
        std::fs::copy(repo_root().join(installer), dir.join(installer)).unwrap();
        let out = Command::new("bash").arg(dir.join(installer)).current_dir(&dir).output().expect("bash runs");
        assert!(out.status.success(), "{installer}: {}", String::from_utf8_lossy(&out.stderr));
        let hooks = String::from_utf8_lossy(
            &Command::new("git").args(["rev-parse", "--git-path", "hooks"]).current_dir(&dir).output().unwrap().stdout,
        ).trim().to_string();
        let wrapper = dir.join(&hooks).join(hook);
        assert!(wrapper.exists(), "{installer} wrote no {hook}");
        // the repo has NO scripts/hooks/... script, which is every other repository on a
        // machine with a global hooksPath: the wrapper must exit 0, not 1.
        let run = Command::new("sh").arg(&wrapper).current_dir(&dir).output().expect("sh runs");
        assert_eq!(
            run.status.code(),
            Some(0),
            "{hook} wrapper must no-op cleanly where the script is absent, else it blocks every              repo on a machine with a global core.hooksPath: {}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

