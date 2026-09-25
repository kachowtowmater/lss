//! `scripts/remote-build.sh`'s SOURCE-RESOLUTION guards, driven for real against a stub
//! rsync/ssh (no network, no real remote host) - card #164, the separable gap #159 left behind.
//!
//! WHY THIS NEEDS TEETH: the script runs `rsync -a --delete "$REPO/" "$HOST:$REMOTE_DIR/"`. If
//! REPO ever resolves wrong, that `--delete` wipes a remote build tree - card #148 already lost
//! a hand-synced worktree fix to exactly that line, which is why #159 added the override in the
//! first place. The guards below are the only thing between a typo and that outcome, and until
//! now nothing pinned them: eight sibling `*_script.rs` tests exist, this script had none.

use std::path::{Path, PathBuf};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// A stub `rsync`/`ssh` on PATH (same shape as gate_deploy_script.rs's stub docker/curl): rsync
/// logs its arguments and succeeds without moving a byte, ssh always succeeds without touching a
/// network. Every guard this file tests fires BEFORE the script's real rsync/ssh lines, so a
/// happy-path test only needs the stubs to not fail the script, not to do anything real.
struct Stubs {
    dir: PathBuf,
}

impl Stubs {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-remote-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // card #202: the stub also records WHICH FILES the source directory held when rsync was
        // called. That is the only way to prove a pinned run syncs an archive of a commit rather
        // than the working tree - the argument alone is just a path.
        write_exec(&dir.join("bin/rsync"), "#!/bin/sh\necho \"$@\" >> \"$LSS_RSYNC_LOG\"\nfor a in \"$@\"; do case \"$a\" in */) if [ -d \"$a\" ]; then ( cd \"$a\" && find . -type f | sort ) >> \"${LSS_RSYNC_LIST:-/dev/null}\"; fi ;; esac; done\nexit 0\n");
        write_exec(&dir.join("bin/ssh"), "#!/bin/sh\necho \"$@\" >> \"$LSS_SSH_LOG\"\nexit 0\n");
        Stubs { dir }
    }

    /// A minimal tree that passes the script's own sanity check (a Cargo.toml and a
    /// scripts/remote-build.sh must exist under it; their content is never read).
    fn fake_source(&self, name: &str) -> PathBuf {
        let src = self.dir.join(name);
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(src.join("scripts/remote-build.sh"), "#!/bin/sh\n").unwrap();
        src
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("bash");
        cmd.arg(repo().join("scripts/remote-build.sh"))
            .args(args)
            .env("PATH", format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("LSS_BUILD_HOST", "stub-host")
            .env("LSS_RSYNC_LOG", self.dir.join("rsync.log"))
            .env("LSS_RSYNC_LIST", self.dir.join("rsync.list"))
            .env("LSS_SSH_LOG", self.dir.join("ssh.log"))
            .env_remove("LSS_BUILD_SRC")
            .env_remove("LSS_BUILD_DIR")
            .env_remove("LSS_BUILD_REF")
            .env_remove("LSS_BUILD_TARGET_VOL");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("bash runs")
    }

    fn rsync_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("rsync.log")).unwrap_or_default()
    }

    fn ssh_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("ssh.log")).unwrap_or_default()
    }

    /// The files the directory handed to rsync actually held, one per line.
    fn synced_files(&self) -> String {
        std::fs::read_to_string(self.dir.join("rsync.list")).unwrap_or_default()
    }

    /// A real one-commit git repository that also holds an UNCOMMITTED file, so a pinned run and
    /// a live run can be told apart by their contents rather than by their arguments.
    fn git_source(&self, name: &str) -> PathBuf {
        let src = self.fake_source(name);
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        std::fs::write(src.join("committed.txt"), "in the commit\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "the pinned tree"]);
        std::fs::write(src.join("uncommitted.txt"), "written after the commit\n").unwrap();
        src
    }
}

impl Drop for Stubs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_exec(path: &Path, body: &str) {
    exec_file::write_exec(path, body);
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn a_positional_source_override_wins_and_reaches_rsync() {
    let s = Stubs::new("positional");
    let src = s.fake_source("src-positional");
    let out = s.run(&["test", src.to_str().unwrap()], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.rsync_log();
    assert!(log.contains(src.to_str().unwrap()), "rsync was not given the positional override as its source:\n{log}");
}

#[test]
fn the_lss_build_src_env_override_is_honoured_with_no_positional_arg() {
    let s = Stubs::new("env");
    let src = s.fake_source("src-env");
    let out = s.run(&["test"], &[("LSS_BUILD_SRC", src.to_str().unwrap())]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.rsync_log().contains(src.to_str().unwrap()), "the env override was not used:\n{}", s.rsync_log());
}

#[test]
fn a_positional_argument_beats_the_env_override_when_both_are_set() {
    // undocumented precedence (verifier-2): a reader would guess the opposite. Pinned here AND
    // stated in the script's own header comment - a test alone does not tell the next reader why.
    let s = Stubs::new("precedence");
    let env_src = s.fake_source("src-env-loses");
    let pos_src = s.fake_source("src-positional-wins");
    let out = s.run(&["test", pos_src.to_str().unwrap()], &[("LSS_BUILD_SRC", env_src.to_str().unwrap())]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.rsync_log();
    assert!(log.contains(pos_src.to_str().unwrap()), "the positional override should have won:\n{log}");
    assert!(!log.contains(env_src.to_str().unwrap()), "the env override's path leaked into the rsync call:\n{log}");
}

#[test]
fn a_positional_path_that_does_not_exist_refuses_before_touching_the_remote() {
    let s = Stubs::new("missing");
    let out = s.run(&["test", "/definitely/does/not/exist/lss-164"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("does not exist"), "{}", text(&out));
    assert!(s.rsync_log().is_empty(), "the remote must never be touched when the override is bad:\n{}", s.rsync_log());
}

#[test]
fn a_real_but_implausible_tree_refuses_before_touching_the_remote() {
    let s = Stubs::new("implausible");
    let dir = s.dir.join("not-a-repo");
    std::fs::create_dir_all(&dir).unwrap();
    // exists, but has neither Cargo.toml nor scripts/remote-build.sh
    let out = s.run(&["test", dir.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("not a plausible source tree"), "{}", text(&out));
    assert!(s.rsync_log().is_empty(), "{}", s.rsync_log());
}

#[test]
fn a_mistyped_command_word_exits_2_instead_of_silently_running_the_default() {
    // "tests" (not "test") is neither a recognised command word nor a flag, so the same loop
    // that reads a real override treats it as a positional SOURCE argument - and since it is
    // not a real directory, the missing-source guard catches it before any default ever runs.
    let s = Stubs::new("mistyped");
    let out = s.run(&["tests"], &[]);
    assert_eq!(out.status.code(), Some(2), "a mistyped command word must not fall through to running 'test' by default:\n{}", text(&out));
    assert!(s.rsync_log().is_empty(), "{}", s.rsync_log());
}

// ---------------------------------------------------------------------------------- card #202
// Two measured failures, one script.
//
// (1) Card #94 gave every worker its own remote SOURCE directory via LSS_BUILD_DIR - and the
//     `docker run` line then mounted `-v lss-target:/w/target` for all of them, so N isolated
//     checkouts shared ONE cargo target directory. The workaround was visible on the build host
//     as a pile of hand-made lss-target-* volumes.
// (2) A verifier syncing from the live shared checkout watched origin/main move b4a4ebd ->
//     cc9a729 MID-RUN: "611 passed", then twenty minutes later one test failing 5/5
//     deterministically on what looked like the same tree. Two trees, and nothing in the output
//     said which was which.

#[test]
fn the_cargo_target_volume_follows_the_build_dir_instead_of_being_shared() {
    let s = Stubs::new("targetvol");
    let src = s.fake_source("src-vol");

    // the DEFAULT build dir keeps the historical volume name, or every existing cache is orphaned
    let out = s.run(&["test", src.to_str().unwrap()], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.ssh_log().contains("-v lss-target:/w/target"), "the default build dir must keep the default cache:\n{}", s.ssh_log());

    // a worker with its own build dir gets its own volume - THE defect this card is about
    let s = Stubs::new("targetvol2");
    let src = s.fake_source("src-vol2");
    let out = s.run(&["test", src.to_str().unwrap()], &[("LSS_BUILD_DIR", "lss-build-worker-7")]);
    assert!(out.status.success(), "{}", text(&out));
    let log = s.ssh_log();
    assert!(log.contains("-v lss-target-lss-build-worker-7:/w/target"),
            "a distinct LSS_BUILD_DIR must get a distinct target volume:\n{log}");
    assert!(!log.contains("-v lss-target:/w/target"), "it must not ALSO mount the shared one:\n{log}");

    // and it stays overridable by name, for a worker that wants to share a warm cache on purpose
    let s = Stubs::new("targetvol3");
    let src = s.fake_source("src-vol3");
    let out = s.run(&["test", src.to_str().unwrap()], &[("LSS_BUILD_DIR", "lss-build-worker-7"), ("LSS_BUILD_TARGET_VOL", "shared-on-purpose")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.ssh_log().contains("-v shared-on-purpose:/w/target"), "{}", s.ssh_log());
}

#[test]
fn a_pinned_ref_verifies_the_commit_and_not_whatever_the_checkout_holds() {
    let s = Stubs::new("pinned");
    let src = s.git_source("src-pinned");

    // the live tree: the uncommitted file is part of what gets verified, and the run SAYS so
    let out = s.run(&["test", src.to_str().unwrap()], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.synced_files().contains("./uncommitted.txt"), "a live run syncs the working tree:\n{}", s.synced_files());
    assert!(text(&out).contains("the LIVE tree") && text(&out).contains("uncommitted path(s)"),
            "a live run must say it is a live run, and how dirty:\n{}", text(&out));

    // the pinned run: the SAME tree, the same argument, and the uncommitted file is not there
    let s = Stubs::new("pinned2");
    let src = s.git_source("src-pinned2");
    let out = s.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let files = s.synced_files();
    assert!(files.contains("./committed.txt"), "the commit's own files must be synced:\n{files}");
    assert!(!files.contains("./uncommitted.txt"),
            "a pinned run must verify the COMMIT, not the working tree that happens to sit on it:\n{files}");
    assert!(text(&out).contains("verifying PINNED "), "a pinned run must name the sha it verified:\n{}", text(&out));

    // ...and it lands in its own remote directory and its own volume, derived from the sha, so
    // two pinned runs at two commits cannot overwrite each other
    let log = s.ssh_log();
    assert!(log.contains("lss-build-") && log.contains("-v lss-target-lss-build-"),
            "a pinned run must not land in the shared build dir or the shared volume:\n{log}");

    // LSS_BUILD_REF is the same thing without an argument, for a wrapper that sets env only
    let s = Stubs::new("pinned3");
    let src = s.git_source("src-pinned3");
    let out = s.run(&["test", src.to_str().unwrap()], &[("LSS_BUILD_REF", "HEAD")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!s.synced_files().contains("./uncommitted.txt"), "{}", s.synced_files());
}

#[test]
fn a_bad_ref_and_a_pinned_bless_are_refused_before_touching_the_remote() {
    let s = Stubs::new("badref");
    let src = s.git_source("src-badref");
    let out = s.run(&["test", src.to_str().unwrap(), "--ref", "no-such-ref-202"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("is not a commit"), "{}", text(&out));
    assert!(s.rsync_log().is_empty(), "the remote must never be touched for a ref that does not resolve:\n{}", s.rsync_log());

    // bless WRITES fixtures back into the source tree; an archive of a past commit has nowhere to
    // write them, so a pinned bless would silently regenerate into a directory about to be deleted
    let s = Stubs::new("blessref");
    let src = s.git_source("src-blessref");
    let out = s.run(&["bless", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert_eq!(out.status.code(), Some(2), "a pinned bless must be refused, not silently discarded: {}", text(&out));
    assert!(text(&out).contains("bless"), "{}", text(&out));
    assert!(s.rsync_log().is_empty(), "{}", s.rsync_log());
}

/// card #238 (the "year_start fails under TZ=America/Los_Angeles on the #226 branch" report):
/// `git archive` stamps every file with the COMMIT's time. A pinned build dir's target volume can
/// hold a binary built LATER from other source (a mutation run of the same sha, measured on the
/// build host) - cargo sees the restored original as OLDER than that artifact, does not rebuild,
/// and runs the stale mutated binary while the tree on disk is clean. Every file a pinned run
/// hands to rsync must therefore be at least as new as the run itself.
#[test]
fn a_pinned_ref_syncs_files_newer_than_any_earlier_build_so_cargo_cannot_run_a_stale_binary() {
    let s = Stubs::new("pin-mtime");
    let src = s.fake_source("old-commit");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&src)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.com")
            // an OLD commit: its archive would carry 2001 timestamps
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z").env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q"]);
    std::fs::write(src.join("src.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "an old pinned tree"]);
    // the marker is "now": anything handed to rsync that is not newer than it would let cargo
    // keep an artifact built after the commit but before this run
    let marker = s.dir.join("marker");
    std::fs::write(&marker, "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let stale = s.dir.join("stale.list");
    write_exec(
        &s.dir.join("bin/rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in */) if [ -d \"$a\" ]; then find \"$a\" -type f ! -newer \"$LSS_MARKER\" >> \"$LSS_STALE\"; fi ;; esac; done\nexit 0\n",
    );
    let out = s.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[("LSS_MARKER", marker.to_str().unwrap()), ("LSS_STALE", stale.to_str().unwrap())]);
    assert!(s.ssh_log().contains("cargo test"), "the pinned run reached the build step:\n{}", text(&out));
    let old = std::fs::read_to_string(&stale).unwrap_or_default();
    assert!(old.trim().is_empty(), "a pinned run synced files with the COMMIT's old mtime - cargo may run a stale binary built after it:\n{old}");
}

/// card #264: a stub build host that really EXECUTES what the script sends it. `ssh` runs the
/// remote command (or the script on its stdin) with `bash`; `rsync` creates the remote dir under a
/// fake $HOME; a fake `docker` keeps volumes as files, fails a build on request, and reports the
/// containers of OTHER live runs (their mounts) from files - so cleanup is judged by what is left
/// on the "host", not by the text of a command.
struct Host {
    dir: PathBuf,
}

impl Host {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-rb-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["bin", "home", "vols"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        write_exec(&dir.join("bin/ssh"), "#!/bin/bash\nshift\ncd \"$HOME\" || exit 1\nexec bash -c \"$*\"\n");
        write_exec(&dir.join("bin/rsync"), "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in *:*/) d=\"${last#*:}\"; mkdir -p \"$HOME/$d\"; touch \"$HOME/$d/synced\" ;; esac\nexit 0\n");
        write_exec(&dir.join("bin/docker"), r#"#!/bin/sh
echo "docker $*" >> "$FAKE/docker.log"
case "$1" in
  run)
    for a in "$@"; do case "$a" in *:/w/target) touch "$FAKE/vols/${a%%:*}" ;; esac; done
    case "$*" in *"cargo test"*) [ -n "$FAKE_FAIL" ] && exit 101 ;; esac
    exit 0 ;;
  ps) [ -f "$FAKE/others" ] && cut -d'|' -f1 "$FAKE/others"; exit 0 ;;
  inspect) for id in "$@"; do grep "^$id|" "$FAKE/others" 2>/dev/null | cut -d'|' -f2-; done; exit 0 ;;
  volume)
    [ "$2" = rm ] || exit 0
    [ -e "$FAKE/vol-rm-fails" ] && exit 1
    [ -e "$FAKE/vols/$3" ] && rm "$FAKE/vols/$3" && exit 0
    exit 1 ;;
esac
exit 0
"#);
        Host { dir }
    }
    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }
    fn vol(&self, name: &str) -> bool {
        self.dir.join("vols").join(name).exists()
    }
    /// another LIVE run's container: `<id>|/<name>|<mount source>|<volume name>|...|`
    fn other_run(&self, id: &str, mounts: &[&str]) {
        let line = format!("{id}|/other-run-{id}{}|\n", mounts.iter().map(|m| format!("|{m}")).collect::<String>());
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("others")).unwrap();
        use std::io::Write;
        f.write_all(line.as_bytes()).unwrap();
    }
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("bash");
        cmd.arg(repo().join("scripts/remote-build.sh"))
            .args(args)
            .env("PATH", format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.home())
            .env("FAKE", &self.dir)
            .env("LSS_BUILD_HOST", "stub-host")
            .env("LSS_TEST_TZ2", "")
            .env_remove("LSS_BUILD_SRC").env_remove("LSS_BUILD_DIR").env_remove("LSS_BUILD_REF")
            .env_remove("LSS_BUILD_TARGET_VOL").env_remove("LSS_BUILD_KEEP");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("bash runs")
    }
    fn docker_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("docker.log")).unwrap_or_default()
    }
    /// the remote dirs a run left under $HOME
    fn dirs(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.home()).unwrap().flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }
    /// a real one-commit repository to pin (its own Cargo.toml / scripts/remote-build.sh)
    fn src(&self) -> PathBuf {
        let src = self.dir.join("src");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(src.join("scripts/remote-build.sh"), "#!/bin/sh\n").unwrap();
        for args in [vec!["init", "-q"], vec!["add", "Cargo.toml", "scripts/remote-build.sh"], vec!["commit", "-qm", "pinned"]] {
            let out = Command::new("git").args(&args).current_dir(&src)
                .env("HOME", self.home())
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
        src
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// card #264 (verifier lss-inst-1, F1 - the gate): two pinned runs sharing ONE explicit
/// LSS_BUILD_DIR; the one that ended first deleted the dir (and tried the volume) under the
/// other's RUNNING container, and that run then failed ('could not execute process ... No such
/// file'). A run must never remove a dir or volume another live container still mounts.
#[test]
fn a_run_never_removes_a_dir_or_volume_another_live_run_is_using() {
    let h = Host::new("inuse");
    let src = h.src();
    let dir = h.home().join("shared-264");
    // the OTHER run is live: its container mounts the shared dir and its derived volume
    h.other_run("aaa111", &[&dir.display().to_string(), "", "/var/lib/docker/volumes/lss-target-shared-264/_data", "lss-target-shared-264"]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(h.dir.join("vols/lss-target-shared-264"), "").unwrap();
    let out = h.run(&["clippy", src.to_str().unwrap(), "--ref", "HEAD"], &[("LSS_BUILD_DIR", "shared-264")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(dir.exists(), "the dir another live run uses must survive:\n{}", text(&out));
    assert!(h.vol("lss-target-shared-264"), "...and its volume:\n{}", text(&out));
    assert!(text(&out).contains("in use") && text(&out).contains("other-run-aaa111"), "and the run says why, naming the container:\n{}", text(&out));
    assert!(!text(&out).contains("remote-build: removed"), "nothing may be reported removed:\n{}", text(&out));
    // only the volume in use (the other run mounts the volume, not this dir): still both kept
    let h = Host::new("inuse-vol");
    let src = h.src();
    h.other_run("bbb222", &["/var/lib/docker/volumes/lss-target-shared-265/_data", "lss-target-shared-265"]);
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[("LSS_BUILD_DIR", "shared-265")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(h.home().join("shared-265").exists() && h.vol("lss-target-shared-265"), "{}", text(&out));
}

/// card #264: every `--ref` run used to leave its remote build dir and its cargo target volume
/// behind (125 of them filled the build host's / to 100% on 2026-09-23). A pinned run removes
/// both when it ends - pass or fail - unless kept (`--keep` / LSS_BUILD_KEEP=1), and reports
/// exactly what it removed (F2: never 'removed' after a failed removal). What it never removes: a
/// live run's dir, the shared `lss-build` / `lss-target`, a volume the caller named, and anything
/// another live container mounts (above). `--keep` keeps the files, not the process (F3).
#[test]
fn a_pinned_run_removes_its_own_build_dir_and_target_volume_afterwards_unless_kept() {
    // 1. pass: its own dir and volume are gone, and it says so
    let h = Host::new("pass");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(h.dirs().is_empty(), "no dir left: {:?}\n{}", h.dirs(), text(&out));
    assert!(std::fs::read_dir(h.dir.join("vols")).unwrap().next().is_none(), "no volume left\n{}", text(&out));
    assert!(text(&out).contains("remote-build: removed stub-host:~/lss-build-") && text(&out).contains("volume lss-target-lss-build-"), "{}", text(&out));
    // 2. a failing build still cleans up, and still fails
    let h = Host::new("fail");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[("FAKE_FAIL", "1")]);
    assert_eq!(out.status.code(), Some(101), "{}", text(&out));
    assert!(h.dirs().is_empty(), "{:?}", h.dirs());
    // 3. F2: the volume will not go - the report says so and never claims it was removed
    let h = Host::new("volfail");
    let src = h.src();
    std::fs::write(h.dir.join("vol-rm-fails"), "").unwrap();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("could not remove volume lss-target-lss-build-"), "{t}");
    assert!(!t.lines().any(|l| l.contains("remote-build: removed") && l.contains("volume")), "never 'removed volume' after a failure:\n{t}");
    assert!(h.dirs().is_empty(), "the dir is still removed: {:?}", h.dirs());
    // 4. --keep and LSS_BUILD_KEEP=1: files kept, the removal command printed, and this run's own
    // container is still stopped (F3 - an interrupted run's container must not keep running)
    for (args, env) in [(vec!["--keep"], vec![]), (vec![], vec![("LSS_BUILD_KEEP", "1")])] {
        let h = Host::new("keep");
        let src = h.src();
        let mut a = vec!["test", src.to_str().unwrap(), "--ref", "HEAD"];
        a.extend(args.iter().copied());
        let out = h.run(&a, &env);
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(h.dirs().len(), 1, "{args:?} {env:?} keeps the dir");
        assert!(std::fs::read_dir(h.dir.join("vols")).unwrap().next().is_some(), "...and the volume");
        assert!(text(&out).contains("kept") && text(&out).contains("docker volume rm"), "{}", text(&out));
        assert!(h.docker_log().lines().any(|l| l.starts_with("docker rm -f lss-run-")), "the run's own container is stopped even with --keep:\n{}", h.docker_log());
    }
    // 5. never removed: a live run; the shared default; a named volume (its dir still goes)
    let h = Host::new("live");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap()], &[("LSS_BUILD_DIR", "lss-build-worker-9")]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(h.dirs(), vec!["lss-build-worker-9".to_string()], "a live run removes nothing");
    let h = Host::new("shared");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[("LSS_BUILD_DIR", "lss-build")]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(h.dirs(), vec!["lss-build".to_string()]);
    assert!(h.vol("lss-target"), "the shared lss-target stays");
    let h = Host::new("named");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[("LSS_BUILD_TARGET_VOL", "shared-on-purpose")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(h.vol("shared-on-purpose"), "a volume the caller named is theirs");
    // 6. build: its dir (dist/) stays, the volume goes, and it says where dist/ is
    let h = Host::new("build");
    let src = h.src();
    let out = h.run(&["build", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(h.dirs().len(), 1, "build keeps its dir");
    assert!(std::fs::read_dir(h.dir.join("vols")).unwrap().next().is_none(), "...and drops its volume");
    assert!(text(&out).contains(&format!("~/{}/dist", h.dirs()[0])), "{}", text(&out));
    // 7. two pinned runs of the same sha never share a dir
    let h = Host::new("twice");
    let src = h.src();
    let keep = [("LSS_BUILD_KEEP", "1")];
    let _ = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &keep);
    let _ = h.run(&["clippy", src.to_str().unwrap(), "--ref", "HEAD"], &keep);
    assert_eq!(h.dirs().len(), 2, "two runs of one sha get two dirs: {:?}", h.dirs());
}

/// card #264 (verifier note): the cleanup's own ssh can fail (the host rebooted, the network
/// dropped). Then nothing is known to be removed, and the run must say what may be left and how
/// to remove it - never stay silent about a GBs-sized volume it may have leaked.
#[test]
fn a_cleanup_that_cannot_reach_the_host_says_what_may_be_left_and_how_to_remove_it() {
    let h = Host::new("sshfail");
    // the build's own ssh calls work; the cleanup (a script on stdin: `bash -s`) cannot connect
    write_exec(&h.dir.join("bin/ssh"), "#!/bin/bash\nshift\ncase \"$*\" in 'bash -s'*) echo 'ssh: connect to host stub-host port 22: Connection refused' >&2; exit 255 ;; esac\ncd \"$HOME\" || exit 1\nexec bash -c \"$*\"\n");
    let src = h.src();
    let out = h.run(&["test", src.to_str().unwrap(), "--ref", "HEAD"], &[]);
    assert!(out.status.success(), "the build itself passed, and a failed cleanup does not change that: {}", text(&out));
    let t = text(&out);
    assert!(t.contains("may be left") && t.contains("docker volume rm lss-target-lss-build-") && t.contains("rm -rf ~/lss-build-"),
            "the run names what may be left and the command that removes it:\n{t}");
    assert!(!t.contains("remote-build: removed"), "nothing is claimed removed:\n{t}");
}
