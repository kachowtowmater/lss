//! `scripts/e2e/dist.sh` packs each tarball as THE REF'S OWN release.yml packs it - card #318.
//!
//! It used to tar `lss lss-collector lss-notify.sh` for every ref. release.yml only started packing
//! lss-notify.sh after v1.1.2 (5e4b92f), so an older ref's tarball had a file its real release never
//! had, and a ref with no scripts/lss-notify.sh failed at `install`. Driven for real here, in a
//! scratch git repository with the three shapes history holds (no release.yml; one that tars the
//! two binaries; one that also tars lss-notify.sh), through stub ssh / rsync / scp - no network,
//! no build: the stub scp hands back two fake binaries, and the test reads the tarballs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn exec(p: &Path, text: &str) {
    exec_file::write_exec(p, text);
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-dist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("repo");
        std::fs::create_dir_all(&src).unwrap();
        exec(&src.join("scripts/e2e/dist.sh"), &std::fs::read_to_string(repo().join("scripts/e2e/dist.sh")).unwrap());
        // stubs: ssh / rsync do nothing; scp copies two fake binaries into its last argument
        exec(&dir.join("bin/ssh"), "#!/bin/sh\nexit 0\n");
        exec(&dir.join("bin/rsync"), "#!/bin/sh\nexit 0\n");
        exec(&dir.join("bin/scp"), "#!/bin/sh\nfor last; do :; done\nprintf 'fake lss\\n' > \"$last/lss\"\nprintf 'fake collector\\n' > \"$last/lss-collector\"\n");
        let s = Scratch { dir };
        s.git(&["init", "-q"]);
        s
    }

    fn src(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).current_dir(self.src()).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// stage exactly these paths (never `add -A`), commit, return the short sha
    fn commit(&self, msg: &str, paths: &[&str]) -> String {
        let mut add = vec!["add", "--"];
        add.extend_from_slice(paths);
        self.git(&add);
        self.git(&["commit", "-qm", msg]);
        self.git(&["rev-parse", "--short", "HEAD"])
    }

    fn dist(&self, sha: &str, out: &str) -> Output {
        Command::new("bash")
            .arg(self.src().join("scripts/e2e/dist.sh"))
            .args(["--host", "stub-host", "--ref", sha, "--target", "aarch64-apple-darwin", "--out"])
            .arg(self.dir.join(out))
            .env("PATH", format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env_remove("LSS_E2E_HOST")
            .output()
            .expect("bash runs")
    }

    /// `tar -tvzf` of the tarball: (mode string, name) per entry
    fn contents(&self, out: &str) -> Vec<(String, String)> {
        let t = Command::new("tar").arg("-tvzf").arg(self.dir.join(out).join("lss-aarch64-apple-darwin.tar.gz")).output().expect("tar runs");
        String::from_utf8_lossy(&t.stdout).lines().map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f[0].to_string(), f.last().unwrap().trim_start_matches("./").to_string())
        }).collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const RELEASE_TWO: &str = "jobs:\n  build:\n    steps:\n      - run: |\n          tar -C \"$STAGE\" -czf \"$ASSET\" lss lss-collector\n";
const RELEASE_THREE: &str = "jobs:\n  build:\n    steps:\n      - run: |\n          install -m 0755 scripts/lss-notify.sh \"$STAGE/lss-notify.sh\"\n          tar -C \"$STAGE\" -czf \"$ASSET\" lss lss-collector lss-notify.sh\n";

fn names(c: &[(String, String)]) -> Vec<&str> {
    let mut n: Vec<&str> = c.iter().map(|(_, n)| n.as_str()).collect();
    n.sort();
    n
}

#[test]
fn each_tarball_holds_exactly_what_its_own_refs_release_yml_packs() {
    let s = Scratch::new("layouts");
    // 1. before release.yml existed: the two binaries, and it says why
    write(&s.src().join("README.md"), "x\n");
    let none = s.commit("no release.yml yet", &["README.md", "scripts/e2e/dist.sh"]);
    // 2. a release.yml that tars the two binaries (v1.1.2's shape) - and no notify script in the tree
    write(&s.src().join(".github/workflows/release.yml"), RELEASE_TWO);
    let two = s.commit("release.yml: two binaries", &[".github/workflows/release.yml"]);
    // 3. the notify script, and a release.yml that installs and tars it
    write(&s.src().join("scripts/lss-notify.sh"), "#!/bin/sh\necho notify\n");
    write(&s.src().join(".github/workflows/release.yml"), RELEASE_THREE);
    let three = s.commit("release.yml: + lss-notify.sh", &["scripts/lss-notify.sh", ".github/workflows/release.yml"]);

    let out = s.dist(&none, "d-none");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("has no .github/workflows/release.yml - packing lss + lss-collector"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(names(&s.contents("d-none")), ["lss", "lss-collector"]);

    let out = s.dist(&two, "d-two");
    assert!(out.status.success(), "a ref whose release.yml never tars lss-notify.sh needs none: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(names(&s.contents("d-two")), ["lss", "lss-collector"], "exactly what that ref's release held");

    let out = s.dist(&three, "d-three");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let c = s.contents("d-three");
    assert_eq!(names(&c), ["lss", "lss-collector", "lss-notify.sh"]);
    assert!(c.iter().all(|(mode, _)| mode.starts_with("-rwxr-xr-x")), "0755 as release.yml installs them: {c:?}");
    let sha = std::fs::read_to_string(s.dir.join("d-three/lss-aarch64-apple-darwin.tar.gz.sha256")).unwrap();
    assert!(sha.trim_end().ends_with("  lss-aarch64-apple-darwin.tar.gz"), "{sha}");

    // --layout-only says the same without a host
    let lay = Command::new("bash").arg(s.src().join("scripts/e2e/dist.sh")).args(["--ref", &two, "--layout-only"]).env_remove("LSS_E2E_HOST").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&lay.stdout), "layout: lss build\nlayout: lss-collector build\n");
}

#[test]
fn a_packed_file_with_no_source_is_a_clear_error_not_a_guess() {
    let s = Scratch::new("nosource");
    write(&s.src().join(".github/workflows/release.yml"), "      - run: |\n          tar -C \"$STAGE\" -czf \"$ASSET\" lss lss-collector extra.txt\n");
    let bad = s.commit("tars a file it never installs", &["scripts/e2e/dist.sh", ".github/workflows/release.yml"]);
    let out = s.dist(&bad, "d-bad");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("packs 'extra.txt' but has no 'install -m MODE SRC"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!s.dir.join("d-bad/lss-aarch64-apple-darwin.tar.gz").exists(), "nothing half-packed");
}
