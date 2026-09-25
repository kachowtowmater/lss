//! card #253: `scripts/release-notes.sh`, driven for real against throw-away git repos - no
//! GitHub, no pushed tag. The "runner" clone is put into exactly the state actions/checkout
//! leaves on a tag push (`git fetch --no-tags origin +<sha>:refs/tags/<tag>` after fetching
//! every tag), which is what made v1.0.0 and v1.1.0 publish the commit message as their notes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    out
}

fn kind(dir: &Path, tag: &str) -> String {
    String::from_utf8_lossy(&git(dir, &["cat-file", "-t", &format!("refs/tags/{tag}")]).stdout).trim().to_string()
}

const NOTES: &str = "lss 9.9.9 - the release notes\n\nWHAT CHANGED\n- a line a reader wants";

struct Repos {
    dir: PathBuf,
}

impl Repos {
    /// origin: one commit (message "release: bump"), an ANNOTATED tag v9.9.9 carrying NOTES and a
    /// LIGHTWEIGHT tag v9.9.8. runner: a clone of it.
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-release-notes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let origin = dir.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        std::fs::write(origin.join("f"), "x").unwrap();
        git(&origin, &["add", "f"]);
        git(&origin, &["commit", "-q", "-m", "release: bump"]);
        git(&origin, &["tag", "-a", "v9.9.9", "-m", NOTES]);
        git(&origin, &["tag", "v9.9.8"]);
        git(&dir, &["clone", "-q", origin.to_str().unwrap(), "runner"]);
        Repos { dir }
    }

    fn runner(&self) -> PathBuf {
        self.dir.join("runner")
    }

    /// What actions/checkout does after fetching the tags: the tag ref re-pointed at the COMMIT.
    fn clobber_like_actions_checkout(&self, tag: &str) {
        let sha = String::from_utf8_lossy(&git(&self.runner(), &["rev-parse", &format!("{tag}^{{commit}}")]).stdout).trim().to_string();
        git(&self.runner(), &["fetch", "-q", "--no-tags", "origin", &format!("+{sha}:refs/tags/{tag}")]);
    }

    fn notes(&self, tag: &str) -> Output {
        Command::new("bash")
            .arg(repo().join("scripts/release-notes.sh"))
            .arg(tag)
            .current_dir(self.runner())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("bash runs")
    }
}

impl Drop for Repos {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_annotated_tag_is_restored_after_checkout_made_it_lightweight_and_its_message_is_the_notes() {
    let r = Repos::new("restored");
    r.clobber_like_actions_checkout("v9.9.9");
    assert_eq!(kind(&r.runner(), "v9.9.9"), "commit", "the reproduction: this is the state the release job saw on v1.1.0");
    let out = r.notes("v9.9.9");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), NOTES, "the notes are the TAG's message, not the commit's");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("release: bump"));
    assert_eq!(kind(&r.runner(), "v9.9.9"), "tag", "the tag object is back");
}

#[test]
fn a_lightweight_tag_fails_loudly_and_prints_no_notes() {
    let r = Repos::new("lightweight");
    let out = r.notes("v9.9.8");
    assert_eq!(out.status.code(), Some(1), "a release from a lightweight tag must stop the run");
    assert!(out.stdout.is_empty(), "nothing a release could publish: {:?}", String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an ANNOTATED tag") && err.contains("git tag -a v9.9.8"), "{err}");
}

#[test]
fn a_tag_the_remote_does_not_have_fails() {
    let r = Repos::new("missing");
    let out = r.notes("v0.0.0-nope");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not fetch refs/tags/v0.0.0-nope"));
}

/// card #254 (lss-verifier-4 on #253): a SIGNED tag's raw message ends in its signature block.
/// SSH signing, so the test needs only ssh-keygen, not a GPG keyring.
#[test]
fn a_signed_tag_gives_its_notes_without_the_signature_block() {
    let r = Repos::new("signed");
    let origin = r.dir.join("origin");
    let key = r.dir.join("key");
    let kg = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "t", "-f"]).arg(&key).output().expect("ssh-keygen runs");
    assert!(kg.status.success(), "{}", String::from_utf8_lossy(&kg.stderr));
    let signing_key = format!("user.signingkey={}", key.with_extension("pub").display());
    git(&origin, &["-c", "gpg.format=ssh", "-c", &signing_key, "tag", "-s", "v9.9.7", "-m", NOTES]);
    let raw = String::from_utf8_lossy(&git(&origin, &["tag", "-l", "--format=%(contents)", "v9.9.7"]).stdout).to_string();
    assert!(raw.contains("-----BEGIN SSH SIGNATURE-----"), "the reproduction: the raw message carries the signature\n{raw}");
    let out = r.notes("v9.9.7");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let notes = String::from_utf8_lossy(&out.stdout);
    assert_eq!(notes.trim_end(), NOTES, "exactly the message, not its signature");
    assert!(!notes.contains("SIGNATURE"), "{notes}");
}

#[test]
fn release_yml_publishes_the_checked_notes_and_never_notes_from_tag() {
    let yml = std::fs::read_to_string(repo().join(".github/workflows/release.yml")).unwrap();
    let code: Vec<&str> = yml.lines().filter(|l| !l.trim_start().starts_with('#')).collect();
    let at = |needle: &str| code.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("release.yml has no {needle:?}"));
    assert!(at("scripts/release-notes.sh") < at("gh release create"), "the tag is checked BEFORE anything is published");
    assert!(code.iter().any(|l| l.contains("--notes-file \"$RUNNER_TEMP/notes.md\"")), "the release body is the checked notes");
    assert!(!code.iter().any(|l| l.contains("--notes-from-tag")), "--notes-from-tag falls back to the commit message and exits 0");
    // card #254: the tag name only ever reaches a shell through env (a tag name may hold `$(`)
    for l in code.iter().filter(|l| l.contains("github.ref_name")) {
        assert_eq!(l.trim(), "TAG: ${{ github.ref_name }}", "github.ref_name expanded outside env: {l}");
    }
    assert!(code.iter().any(|l| l.contains("release-notes.sh \"$TAG\"")) && code.iter().any(|l| l.contains("gh release create \"$TAG\"")));
}
