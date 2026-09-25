//! `scripts/install-collector.sh`, driven for REAL in a throw-away HOME (card #246): the script
//! itself runs; only the machinery around it is stood in - `remote-build.sh` (nothing to build),
//! `ssh` (runs the heredoc with a local `bash -s`), `loginctl`, `systemctl`, `curl`, `sleep`.
//! Nothing touches a real host. Card #246: an install over existing binaries must leave a
//! `.prev` (and a dated `.prev-YYYYMMDD`) copy of what it replaced, and print the rollback.

use std::path::{Path, PathBuf};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn write_exec(path: &Path, body: &str) {
    exec_file::write_exec(path, body);
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-instcoll-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // the script, beside a remote-build.sh that has nothing to do
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::copy(repo().join("scripts/install-collector.sh"), dir.join("scripts/install-collector.sh")).unwrap();
        write_exec(&dir.join("scripts/remote-build.sh"), "#!/bin/bash\nexit 0\n");
        // stand-ins on PATH: ssh HOST bash -s  ->  bash -s, here
        for (p, body) in [
            ("ssh", "#!/bin/bash\nshift\nexec \"$@\"\n"),
            ("loginctl", "#!/bin/sh\necho Linger=yes\n"),
            ("systemctl", "#!/bin/sh\nexit 0\n"),
            ("curl", "#!/bin/sh\necho '{\"ok\":true}'\n"),
            ("sleep", "#!/bin/sh\nexit 0\n"),
        ] {
            write_exec(&dir.join("stub").join(p), body);
        }
        // the "remote" build tree the heredoc cds into: ~/lss-build
        let tree = dir.join("home/lss-build");
        for f in ["scripts/lss-alert.sh", "scripts/watch-check.sh"] {
            write_exec(&tree.join(f), "#!/bin/sh\nexit 0\n");
        }
        for f in ["collector.toml.example", "lss-collector.service", "lss-watch.service", "lss-watch.timer"] {
            std::fs::create_dir_all(tree.join("packaging")).unwrap();
            std::fs::write(tree.join("packaging").join(f), "# stand-in\n").unwrap();
        }
        Sandbox { dir }
    }

    /// Put `version` in the build output (what the install will copy in).
    fn build(&self, version: &str) {
        let dist = self.dir.join("home/lss-build/dist");
        write_exec(&dist.join("lss-collector"), &format!("#!/bin/sh\n# {version} collector\nexit 0\n"));
        write_exec(&dist.join("lss"), &format!("#!/bin/sh\n# {version} lss\nexit 0\n"));
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.dir.join("home/.local/bin").join(name)
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.bin(name)).ok()
    }

    fn install(&self) -> (i32, String) {
        let path = format!("{}:{}", self.dir.join("stub").display(), std::env::var("PATH").unwrap_or_default());
        let out = Command::new("bash")
            .arg(self.dir.join("scripts/install-collector.sh"))
            .env("LSS_BUILD_HOST", "testhost")
            .env("USER", "tester")
            .env_remove("LSS_BUILD_DIR")
            .env("HOME", self.dir.join("home"))
            .env("PATH", path)
            .output()
            .expect("bash runs");
        (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }
}

fn today() -> String {
    let out = Command::new("date").arg("+%Y%m%d").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn an_install_over_existing_binaries_backs_them_up_and_prints_the_rollback() {
    let sb = Sandbox::new("upgrade");
    // v1 is installed
    sb.build("v1");
    let (code, out) = sb.install();
    assert_eq!(code, 0, "{out}");
    assert!(sb.read("lss-collector").unwrap().contains("v1 collector"));
    // upgrade to v2: v1 must survive as .prev and as the dated copy
    sb.build("v2");
    let (code, out) = sb.install();
    assert_eq!(code, 0, "{out}");
    assert!(sb.read("lss-collector").unwrap().contains("v2 collector"), "the new binary is installed");
    let day = today();
    for (name, v) in [("lss-collector", "v1 collector"), ("lss", "v1 lss")] {
        assert!(sb.read(&format!("{name}.prev")).is_some_and(|b| b.contains(v)), "{name}.prev must hold what was replaced:\n{out}");
        assert!(sb.read(&format!("{name}.prev-{day}")).is_some_and(|b| b.contains(v)), "{name}.prev-{day} must hold what was replaced:\n{out}");
    }
    assert!(out.contains("ROLLBACK") && out.contains("ssh testhost") && out.contains("lss-collector.prev ~/.local/bin/lss-collector"), "the rollback command is printed:\n{out}");

    // a SECOND install the same day: .prev moves on to v2, the dated copy keeps the day's first (v1)
    sb.build("v3");
    let (code, out) = sb.install();
    assert_eq!(code, 0, "{out}");
    assert!(sb.read("lss-collector.prev").unwrap().contains("v2 collector"), ".prev is the version right before THIS install");
    assert!(sb.read(&format!("lss-collector.prev-{day}")).unwrap().contains("v1 collector"), "the dated copy is what ran before the day's FIRST install");
    let _ = std::fs::remove_dir_all(&sb.dir);
}

#[test]
fn a_first_install_makes_no_backup_and_says_there_is_no_rollback() {
    let sb = Sandbox::new("first");
    sb.build("v1");
    let (code, out) = sb.install();
    assert_eq!(code, 0, "{out}");
    assert!(sb.read("lss-collector.prev").is_none() && sb.read("lss.prev").is_none(), "nothing to back up on a first install");
    assert!(out.contains("no rollback") && !out.contains("ROLLBACK"), "{out}");
    let _ = std::fs::remove_dir_all(&sb.dir);
}
