//! `install.sh`, driven for real in a throw-away HOME with stand-in programs: what it writes for
//! one server, for several, that it never overwrites a config, that a dry run changes nothing,
//! and that uninstall removes the programs and leaves the data.

use std::path::{Path, PathBuf};
use std::process::Command;

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

struct Home {
    dir: PathBuf,
}

impl Home {
    /// `detect`: what the stand-in `lss-collector --detect` prints.
    fn new(name: &str, detect: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("stub")).unwrap();
        // an lss-collector from BEFORE the setup wizard (card #297): `setup` is an unknown
        // argument to it, exactly as to the real v1.1.x - install.sh must fall back, not break
        let collector = format!("#!/bin/sh\ncase \"$*\" in setup*) echo \"lss-collector: unknown argument setup\" >&2; exit 2;; *--detect*) cat <<'EOF'\n{detect}\nEOF\n;; esac\n");
        for (program, body) in [("lss", "#!/bin/sh\necho lss\n".to_string()), ("lss-collector", collector)] {
            let path = dir.join("stub").join(program);
            exec_file::write_exec(&path, &body);
        }
        Home { dir }
    }

    fn install(&self, extra: &[&str]) -> String {
        let out = Command::new("bash")
            .arg(repo().join("install.sh"))
            .args(["--binary-dir", self.dir.join("stub").to_str().unwrap(), "--prefix", self.dir.join("bin").to_str().unwrap()])
            .args(extra)
            .env("HOME", &self.dir)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env("LSS_TTY", "/nonexistent")
            .output()
            .expect("bash runs");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "install.sh {extra:?} failed:\n{text}");
        text
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.dir.join(rel).exists()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const ONE: &str = "Found 1 LLM server on this machine:\n\n  Ollama at http://127.0.0.1:11434\n    why      /api/version says 0.5.1\n    config   [[engine]]\n             kind = \"ollama\"\n             url = \"http://127.0.0.1:11434\"";
const TWO: &str = "Found 2 LLM servers on this machine:\n\n  Ollama at http://127.0.0.1:11434\n    config   [[engine]]\n             kind = \"ollama\"\n             url = \"http://127.0.0.1:11434\"\n\n  vLLM at http://127.0.0.1:8000\n    config   [[engine]]\n             kind = \"vllm\"\n             url = \"http://127.0.0.1:8000\"";

#[test]
fn one_server_installs_the_programs_and_a_config_that_finds_the_engine_by_itself() {
    let h = Home::new("one", ONE);
    let said = h.install(&["--yes", "--no-service"]);
    assert!(h.exists("bin/lss") && h.exists("bin/lss-collector"), "{said}");
    assert!(said.contains("Ollama at http://127.0.0.1:11434") && said.contains("Type:   lss"), "{said}");
    let cfg = h.read(".config/lss/collector.toml");
    // one server: stay on auto, so swapping the engine later needs no edit
    assert!(cfg.contains("listen = [\"127.0.0.1:8099\"]") && cfg.contains("engine_url = \"auto\"") && !cfg.contains("\n[[engine]]"), "{cfg}");
    lss_core::config::parse_config(&cfg).expect("the config install.sh writes is one the collector accepts");
    let client = h.read(".config/lss/lss.toml");
    assert!(client.contains("url = \"http://127.0.0.1:8099\""), "{client}");
    lss_core::config::parse_client_config(&client).expect("and so is lss.toml");

    // a second run never overwrites what the person may have edited
    std::fs::write(h.dir.join(".config/lss/collector.toml"), "# mine\nlisten = [\"127.0.0.1:9000\"]\n").unwrap();
    let again = h.install(&["--yes", "--no-service"]);
    assert!(again.contains("kept ") && h.read(".config/lss/collector.toml").starts_with("# mine"), "{again}");

    // card #320 (6): with no terminal and no --yes, nothing is removed (and it says why)
    let out = Command::new("bash").arg(repo().join("install.sh")).args(["--uninstall", "--prefix", h.dir.join("bin").to_str().unwrap()]).env("HOME", &h.dir).env("LSS_TTY", "/nonexistent").output().unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success() && said.contains("nothing was removed") && said.contains("--uninstall --yes"), "{said}");
    assert!(h.exists("bin/lss") && h.exists("bin/lss-collector"), "{said}");
    // uninstall: the programs go, the configs stay
    h.install(&["--uninstall", "--yes"]);
    assert!(!h.exists("bin/lss") && !h.exists("bin/lss-collector") && h.exists(".config/lss/collector.toml"));
}

#[test]
fn several_servers_get_a_collector_each_and_a_server_entry_each() {
    let h = Home::new("two", TWO);
    h.install(&["--yes", "--no-service"]);
    let first = h.read(".config/lss/collector.toml");
    let second = h.read(".config/lss/collector-2.toml");
    assert!(first.contains("kind = \"ollama\"") && first.contains("127.0.0.1:8099") && !first.contains("db_path"), "{first}");
    assert!(second.contains("kind = \"vllm\"") && second.contains("url = \"http://127.0.0.1:8000\"") && second.contains("listen = [\"127.0.0.1:8100\"]") && second.contains("lss-2.db"), "{second}");
    for cfg in [&first, &second] {
        let parsed = lss_core::config::parse_config(cfg).expect("accepted by the collector");
        assert_eq!(parsed.engines.len(), 1);
    }
    let client = lss_core::config::parse_client_config(&h.read(".config/lss/lss.toml")).expect("lss.toml parses");
    assert_eq!(client.server.iter().map(|s| (s.name.as_str(), s.url.as_str())).collect::<Vec<_>>(), vec![("ollama", "http://127.0.0.1:8099"), ("vllm", "http://127.0.0.1:8100")]);
}

#[test]
fn a_dry_run_changes_nothing_and_no_server_found_is_not_an_error() {
    let h = Home::new("dry", "No LLM server found on this machine.");
    let said = h.install(&["--dry-run"]);
    assert!(said.contains("would run:") && said.contains("would run: lss-collector setup"), "{said}");
    assert!(!h.exists("bin") && !h.exists(".config"), "a dry run wrote something");
    let said = h.install(&["--yes", "--no-service"]);
    assert!(said.contains("None answers right now") && said.contains("keeps looking every 15 seconds"), "{said}");
    assert!(h.read(".config/lss/collector.toml").contains("engine_url = \"auto\""));
}

#[test]
fn the_alert_dispatcher_the_config_names_is_actually_installed() {
    // card #81: a stranger's alerts went nowhere. install.sh wrote a config with no alert_cmd, so
    // the collector fell back to its built-in default (~/bin/lss-alert.sh) - a file install.sh
    // NEVER installed - and scripts/lss-alert.sh, the only dispatcher in the repo, needs our
    // fleet's agent CLI + seat. The fix: install a generic dispatcher AND point alert_cmd at it.
    let h = Home::new("alert", ONE);
    let said = h.install(&["--yes", "--no-service"]);
    assert!(h.exists("bin/lss-notify.sh"), "the dispatcher the config names must exist after install: {said}");
    let cfg = h.read(".config/lss/collector.toml");
    let parsed = lss_core::config::parse_config(&cfg).expect("accepted by the collector");
    // the named program must be the one just installed, resolved to this home
    assert!(parsed.alert_cmd.ends_with("/lss-notify.sh"), "alert_cmd must name the shipped dispatcher: {}", parsed.alert_cmd);
    assert!(!parsed.alert_cmd.contains("lss-alert.sh"),
            "alert_cmd must NOT name the fleet-only dispatcher, which install.sh does not install: {}", parsed.alert_cmd);

    // and the installed dispatcher runs at all here: `<alert_cmd> <severity> <message>` with no
    // hook configured must still exit 0 (the collector's alert must never break the collector)
    let script = h.dir.join("bin/lss-notify.sh");
    let out = std::process::Command::new("bash")
        .arg(&script).args(["warn", "install check"])
        .env("HOME", &h.dir).env("LSS_STATE_DIR", h.dir.join("state"))
        .env("LSS_ALERT_ENV", h.dir.join("no-such.env")).env("LSS_NOTIFY_NO_BANNER", "1")
        .output().expect("bash runs");
    assert!(out.status.success(), "the installed dispatcher exited non-zero: {out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("lss-notify:"), "it must answer in the collector's parseable shape: {out:?}");

    // uninstall removes it with the programs
    h.install(&["--uninstall", "--yes"]);
    assert!(!h.exists("bin/lss-notify.sh"), "uninstall left the dispatcher behind");
}

/// Drive the real installer in a throw-away clone whose only `origin` is `origin`, and return the
/// release URL it says it would download. `--dry-run` downloads nothing; a stub `curl` on PATH
/// keeps the answer the same on a machine that has no curl of its own.
fn release_url_for_origin(origin: &str) -> String {
    let safe = origin.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let dir = std::env::temp_dir().join(format!("lss-origin-{}-{safe}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("stub")).unwrap();
    std::fs::copy(repo().join("install.sh"), dir.join("install.sh")).unwrap();
    // a CLONE of this project (v1.2.1: only inside our own checkout does the origin name the repo)
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::create_dir_all(dir.join("crates/lss-collector")).unwrap();
    let curl = dir.join("stub/curl");
    exec_file::write_exec(&curl, "#!/bin/sh\nexit 1\n");
    for args in [vec!["init", "-q", "."], vec!["remote", "add", "origin", origin]] {
        let out = Command::new("git").args(&args).current_dir(&dir).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    let out = Command::new("bash")
        .arg(dir.join("install.sh"))
        .args(["--dry-run", "--no-service"])
        .env("HOME", dir.join("home"))
        .env("PATH", format!("{}:{}", dir.join("stub").display(), std::env::var("PATH").unwrap_or_default()))
        .env_remove("LSS_REPO")
        .env_remove("LSS_RELEASE_BASE")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("LSS_TTY", "/nonexistent")
        .output()
        .expect("bash runs");
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "install.sh --dry-run failed for origin {origin}:\n{said}");
    let _ = std::fs::remove_dir_all(&dir);
    said.lines()
        .find_map(|l| l.trim().strip_prefix("would download "))
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_string())
        .unwrap_or_else(|| panic!("install.sh named no release URL for origin {origin} (unknown OS/ARCH here?):\n{said}"))
}

#[test]
fn the_release_url_is_right_for_every_origin_shape_the_host_hands_out() {
    // card #189: the installer derives the download URL from this checkout's own origin. The URL
    // the host offers by default ends in `.git`, and a naive strip produced
    // `https://github.com/OWNER/NAME.git/releases/latest/download/...` - a 404 on exactly the
    // path that exists for the person with no Rust toolchain. The scp-style SSH remote was worse:
    // it has nothing to cut at, so the whole remote was pasted into the middle of the URL.
    const WANT: &str = "https://github.com/acme/widget/releases/latest/download/lss-";
    // the scp form is assembled, not written out: it reads as an e-mail address to the privacy
    // net, which scans this file like any other (scripts/privacy-check.sh).
    let scp = concat!("git", "@", "github.com:acme/widget.git");
    for origin in ["https://github.com/acme/widget.git", "https://github.com/acme/widget", scp, concat!("ssh://git", "@", "github.com/acme/widget.git")] {
        let url = release_url_for_origin(origin);
        assert!(url.starts_with(WANT) && url.ends_with(".tar.gz"), "origin {origin} gave the wrong release URL:\n  want {WANT}<target>.tar.gz\n  got  {url}");
    }
}

// ------------------------------------------------------------------ card #297 / #299: the real
// lss-collector (its setup wizard), a fake engine, a local release server, curl | bash, a pty.

/// A fake OpenAI-compatible engine with SGLang-shaped metrics, optionally behind an API key.
struct Engine {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    fn start(key: Option<&'static str>) -> Engine {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                if req.url() == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                if key.is_some_and(|k| auth.as_deref() != Some(format!("Bearer {k}").as_str())) {
                    let _ = req.respond(tiny_http::Response::from_string("{}").with_status_code(401));
                    continue;
                }
                let (code, body) = match req.url() {
                    "/v1/models" => (200, "{\"data\":[{\"id\":\"fake-model-7b\"}]}"),
                    "/metrics" => (200, "sglang:num_running_reqs{} 0.0\nsglang:num_queue_reqs{} 0.0\nsglang:generation_tokens_total{} 5\n"),
                    _ => (404, "no"),
                };
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
            }
        });
        Engine { port, handle: Some(handle) }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = ureq::get(&format!("http://127.0.0.1:{}/stop", self.port)).call();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Serves `files` (name -> bytes) under `/latest/download/`, like a GitHub release; anything
/// else is a 404.
struct ReleaseHost {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ReleaseHost {
    fn start(files: Vec<(String, Vec<u8>)>) -> ReleaseHost {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                if req.url() == "/stop" {
                    let _ = req.respond(tiny_http::Response::from_string("bye"));
                    return;
                }
                let hit = files.iter().find(|(name, _)| req.url() == format!("/latest/download/{name}"));
                let _ = match hit {
                    Some((_, bytes)) => req.respond(tiny_http::Response::from_data(bytes.clone())),
                    None => req.respond(tiny_http::Response::from_string("Not Found").with_status_code(404)),
                };
            }
        });
        ReleaseHost { port, handle: Some(handle) }
    }
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for ReleaseHost {
    fn drop(&mut self) {
        let _ = ureq::get(&format!("http://127.0.0.1:{}/stop", self.port)).call();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// install.sh as release.yml publishes it: its DEFAULT_REPO line re-stamped with the repo.
fn stamped(repo_slug: &str) -> String {
    let src = std::fs::read_to_string(repo().join("install.sh")).unwrap();
    let line = src.lines().find(|l| l.starts_with("DEFAULT_REPO=\"")).expect("install.sh has a DEFAULT_REPO line for release.yml to stamp");
    src.replace(line, &format!("DEFAULT_REPO=\"{repo_slug}\""))
}

/// The release triple install.sh asks for on THIS machine (None = no release exists for it).
fn triple() -> Option<&'static str> {
    let os = String::from_utf8(Command::new("uname").arg("-s").output().unwrap().stdout).unwrap();
    let arch = String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout).unwrap();
    match (os.trim(), arch.trim()) {
        ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-musl"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// A scratch HOME plus a directory holding the REAL lss-collector (built by this test run), a
/// stand-in lss and the real lss-notify.sh - what a release tarball holds.
struct Real {
    dir: PathBuf,
}

impl Real {
    fn new(name: &str) -> Real {
        let dir = std::env::temp_dir().join(format!("lss-real-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("dist");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        exec_file::copy_exec(Path::new(env!("CARGO_BIN_EXE_lss-collector")), &bin.join("lss-collector"));
        exec_file::write_exec(&bin.join("lss"), "#!/bin/sh\necho lss\n");
        exec_file::copy_exec(&repo().join("scripts/lss-notify.sh"), &bin.join("lss-notify.sh"));
        Real { dir }
    }
    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }
    /// The release tarball + its checksum line, as release.yml builds them.
    fn tarball(&self) -> (Vec<u8>, String) {
        let out = self.dir.join("asset.tar.gz");
        let st = Command::new("tar").arg("-C").arg(self.dir.join("dist")).arg("-czf").arg(&out).args(["lss", "lss-collector", "lss-notify.sh"]).status().unwrap();
        assert!(st.success());
        let bytes = std::fs::read(&out).unwrap();
        let sum = Command::new("sh").arg("-c").arg("(sha256sum \"$1\" 2>/dev/null || shasum -a 256 \"$1\") | cut -d' ' -f1").arg("sh").arg(&out).output().unwrap();
        (bytes, String::from_utf8(sum.stdout).unwrap().trim().to_string())
    }
    /// install.sh with `stdin` as its standard input (Some(script) = piped, like curl | bash).
    fn bash(&self, args: &[&str], envs: &[(&str, &str)], piped: bool) -> (bool, String) {
        let mut cmd = Command::new("bash");
        if piped {
            cmd.args(["-s", "--"]);
        } else {
            cmd.arg(repo().join("install.sh"));
        }
        cmd.args(args)
            .current_dir(&self.dir) // never the repository: piped, there is no checkout
            .env("HOME", self.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("LSS_REPO")
            .env_remove("LSS_RELEASE_BASE")
            .env_remove("LSS_API_KEY")
            .env("LSS_TTY", "/nonexistent")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("bash runs");
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            if piped {
                stdin.write_all(stamped("acme/public-lss").as_bytes()).unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.home().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_wizard_with_yes_pins_the_engine_writes_the_rate_and_names_the_dispatcher() {
    let eng = Engine::start(None);
    let r = Real::new("yes");
    let dist = r.dir.join("dist");
    let (ok, said) = r.bash(&["--binary-dir", dist.to_str().unwrap(), "--yes", "--no-service", "--url", &eng.url(), "--rate", "0.2"], &[], false);
    assert!(ok, "{said}");
    assert!(said.contains("[ok] models: fake-model-7b") && said.contains("[ok] engine: SGLang"), "the live test ran and said so:\n{said}");
    let cfg = lss_core::config::parse_config(&r.read(".config/lss/collector.toml")).unwrap();
    assert_eq!(cfg.pinned_engine(), Some((None, eng.url())), "the engine the person gave, recognised when it answers");
    assert!(cfg.alert_cmd.ends_with("/.local/bin/lss-notify.sh") && r.home().join(".local/bin/lss-notify.sh").exists(), "{}", cfg.alert_cmd);
    let rates = lss_core::rates::parse_rates_file(&r.read(".config/lss/rates.toml")).expect("the rate file the collector reads");
    assert!(rates.is_flat(), "{rates:?}");
    assert!(r.read(".config/lss/rates.toml").contains("0.2"));
    assert_eq!(lss_core::config::parse_client_config(&r.read(".config/lss/lss.toml")).unwrap().url, "http://127.0.0.1:8099");
    assert!(said.contains("Type:   lss"), "{said}");
}

#[test]
fn piped_from_curl_it_downloads_checks_the_sha256_and_installs_with_no_checkout() {
    let Some(triple) = triple() else {
        eprintln!("SKIPPED: no release triple for this OS/ARCH - install.sh has nothing to download here");
        return;
    };
    let eng = Engine::start(None);
    let r = Real::new("piped");
    let (tar, sum) = r.tarball();
    let asset = format!("lss-{triple}.tar.gz");
    let good = ReleaseHost::start(vec![(asset.clone(), tar.clone()), (format!("{asset}.sha256"), format!("{sum}  {asset}\n").into_bytes())]);
    let (ok, said) = r.bash(&["--yes", "--no-service", "--url", &eng.url(), "--skip-cost"], &[("LSS_RELEASE_BASE", &good.base())], true);
    assert!(ok, "{said}");
    assert!(said.contains(&format!("sha256 matches the published checksum ({sum})")), "{said}");
    for program in ["lss", "lss-collector", "lss-notify.sh"] {
        assert!(r.home().join(".local/bin").join(program).exists(), "{program} not installed:\n{said}");
    }
    assert!(r.read(".config/lss/collector.toml").contains(&eng.url()));
    assert!(!r.home().join(".config/lss/rates.toml").exists(), "--skip-cost: no rate invented");

    // a tampered download is never installed
    let r2 = Real::new("tampered");
    let bad = ReleaseHost::start(vec![(asset.clone(), tar.clone()), (format!("{asset}.sha256"), format!("{}  {asset}\n", "0".repeat(64)).into_bytes())]);
    let (ok, said) = r2.bash(&["--yes", "--no-service", "--skip-cost"], &[("LSS_RELEASE_BASE", &bad.base())], true);
    assert!(!ok && said.contains("does NOT match its published checksum"), "{said}");
    assert!(!r2.home().join(".local/bin/lss-collector").exists(), "{said}");

    // no checksum published: not installed unchecked
    let r3 = Real::new("nosum");
    let nosum = ReleaseHost::start(vec![(asset.clone(), tar)]);
    let (ok, said) = r3.bash(&["--yes", "--no-service"], &[("LSS_RELEASE_BASE", &nosum.base())], true);
    assert!(!ok && said.contains("has no checksum file"), "{said}");

    // no such release: says so, and how to point elsewhere
    let r4 = Real::new("404");
    let empty = ReleaseHost::start(vec![]);
    let (ok, said) = r4.bash(&["--yes", "--no-service"], &[("LSS_RELEASE_BASE", &empty.base())], true);
    assert!(!ok && said.contains("no such release") && said.contains("--repo OWNER/NAME"), "{said}");
    assert!(said.contains("Nothing was installed"), "{said}");
}

#[test]
fn piped_with_no_repo_named_it_downloads_from_the_repository_that_published_it() {
    let r = Real::new("default-repo");
    // a curl that is never really called in a dry run, so this holds on a machine without one
    let stub = r.dir.join("stubbin");
    std::fs::create_dir_all(&stub).unwrap();
    exec_file::write_exec(&stub.join("curl"), "#!/bin/sh\nexit 1\n");
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap_or_default());
    let (ok, said) = r.bash(&["--dry-run", "--no-service"], &[("PATH", &path)], true);
    assert!(ok, "{said}");
    if triple().is_some() {
        assert!(said.contains("would download https://github.com/acme/public-lss/releases/latest/download/lss-"), "{said}");
    }
    let (ok, said) = r.bash(&["--dry-run", "--no-service"], &[("PATH", &path), ("LSS_REPO", "acme/other")], true);
    assert!(ok && (triple().is_none() || said.contains("https://github.com/acme/other/releases/latest/download/lss-")), "{said}");
    assert!(!r.home().join(".local").exists() && !r.home().join(".config").exists(), "a dry run wrote something");
    // the release workflow re-stamps exactly that line
    let wf = std::fs::read_to_string(repo().join(".github/workflows/release.yml")).unwrap();
    assert!(wf.contains("s#^DEFAULT_REPO=") && wf.contains("dist/install.sh"), "release.yml publishes a stamped install.sh");
    // the source installer, piped straight from the repository, defaults to the public repository
    let out = Command::new("bash").args(["-c", "bash -s -- --dry-run --no-service < \"$1\"", "sh"]).arg(repo().join("install.sh")).current_dir(&r.dir).env("HOME", r.home()).env("PATH", &path).env_remove("LSS_REPO").env_remove("LSS_RELEASE_BASE").output().unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{said}");
    if triple().is_some() {
        assert!(said.contains("would download https://github.com/kachowtowmater/lss/releases/latest/download/lss-"), "{said}");
    }
}

/// Drive a command on a real pseudo-terminal: wait for each prompt, then type the answer -
/// exactly what a person at the keyboard does. Returns (exit code, transcript).
fn pty(cmd: &str, script: &[(&str, &str)], envs: &[(&str, String)]) -> (i32, String) {
    // card #319: every way out of this driver ends the child. It used to end in a bare
    // os.waitpid(pid, 0) - an installer that never exited (stuck at a prompt no step answers)
    // hung the whole test run, measured > 9 minutes, instead of failing. Now the final wait has a
    // deadline (LSS_TEST_PTY_FINAL_SECS, default 120 s after the last answer), and on ANY timeout
    // the child's whole process group is killed and reaped, the transcript is printed, and a
    // <<TIMED OUT ...>> line says which wait ran out.
    const DRIVER: &str = r#"
import json, os, pty, select, signal, sys, time
steps = json.loads(sys.argv[1]); cmd = sys.argv[2]
FINAL = float(os.environ.get("LSS_TEST_PTY_FINAL_SECS", "120"))
pid, fd = pty.fork()
if pid == 0:
    os.execvp("bash", ["bash", "-c", cmd])
buf = b""
status = None
def pump(deadline):
    global buf
    r, _, _ = select.select([fd], [], [], max(0.0, deadline - time.time()))
    if not r: return False
    try: chunk = os.read(fd, 4096)
    except OSError: return None
    if not chunk: return None
    buf += chunk; return True
def reap(block):
    global status
    if status is None:
        done, st = os.waitpid(pid, 0 if block else os.WNOHANG)
        if done: status = st
    return status is not None
def finish(code, note):
    # the child is a session leader (pty.fork), so its process group is everything it started:
    # bash, the installer, the wizard - none of it may outlive the test
    if not reap(False):
        try: os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError: pass
        reap(True)
    sys.stdout.write(buf.decode("utf-8", "replace"))
    if note: sys.stdout.write("\n" + note + "\n")
    sys.exit(code)
seen = 0
for want, answer in steps:
    deadline = time.time() + 60
    while buf.find(want.encode(), seen) < 0:
        if time.time() > deadline or pump(deadline) is None:
            finish(99, "<<TIMED OUT waiting for %r>>" % want)
    seen = buf.find(want.encode(), seen) + len(want)
    time.sleep(0.2)
    os.write(fd, answer.encode())
deadline = time.time() + FINAL
while not reap(False):
    if time.time() >= deadline:
        finish(97, "<<TIMED OUT: the command had not exited %.0f s after the last answer - killed>>" % FINAL)
    if pump(min(deadline, time.time() + 0.5)) is None:
        time.sleep(0.05)  # the pty is closed; the child is exiting - keep reaping
while pump(time.time() + 0.2): pass  # whatever it printed last
finish(os.WEXITSTATUS(status) if os.WIFEXITED(status) else 98, "")
"#;
    let steps = serde_json::to_string(&script.iter().map(|(w, a)| vec![w.to_string(), a.to_string()]).collect::<Vec<_>>()).unwrap();
    let mut c = Command::new("python3");
    c.args(["-c", DRIVER, &steps, cmd]).env_remove("LSS_TTY").env_remove("LSS_REPO").env_remove("LSS_API_KEY").env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME");
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().expect("python3 runs");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).to_string())
}

#[test]
fn the_pty_driver_kills_a_command_that_never_exits_and_fails_instead_of_hanging() {
    // card #319: the driver's final wait used to be a bare waitpid - a stuck installer hung the
    // test run for good. Here the command never exits on its own: it must be killed (its pid
    // gone), the run must FAIL with the TIMED OUT line and the transcript, and it must take the
    // configured seconds, not forever.
    if Command::new("python3").arg("-c").arg("import pty").output().map_or(true, |o| !o.status.success()) {
        eprintln!("SKIPPED: no python3 with pty here");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lss-pty-deadline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("pid");
    let started = std::time::Instant::now();
    let (code, said) = pty(&format!("echo started; echo $$ > {}; sleep 600", pidfile.display()), &[], &[("LSS_TEST_PTY_FINAL_SECS", "3".to_string())]);
    let took = started.elapsed();
    assert_eq!(code, 97, "{said}");
    assert!(said.contains("started") && said.contains("<<TIMED OUT: the command had not exited 3 s after the last answer - killed>>"), "{said}");
    assert!(took < std::time::Duration::from_secs(30), "took {took:?}");
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the stuck command (pid {pid}) must be gone, not left running");
    // ...and a prompt that never comes: the per-step wait kills the child the same way
    let (code, said) = pty(&format!("echo $$ > {}; sleep 600", pidfile.display()), &[("a prompt that never comes", "x\n")], &[]);
    assert_eq!(code, 99, "{said}");
    assert!(said.contains("<<TIMED OUT waiting for 'a prompt that never comes'>>"), "{said}");
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "pid {pid} must be gone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_person_at_a_terminal_answers_the_wizard_while_the_script_itself_comes_down_a_pipe() {
    if Command::new("python3").arg("-c").arg("import pty").output().map_or(true, |o| !o.status.success()) {
        eprintln!("SKIPPED: no python3 with pty here - this test needs a pseudo-terminal driver");
        return;
    }
    // an engine on the LAN that wants a key; the person types the address, is told the key is
    // needed, and types it (hidden). install.sh is PIPED in (curl | bash): every
    // answer has to come from the terminal, none from stdin.
    let eng = Engine::start(Some("test-pty-value-99"));
    let r = Real::new("pty");
    let dist = r.dir.join("dist");
    let cmd = format!(
        "cat {} | bash -s -- --binary-dir {} --no-service --kind sglang --url {} --rate 0.27",
        repo().join("install.sh").display(),
        dist.display(),
        eng.url()
    );
    let (code, said) = pty(
        &cmd,
        &[
            ("Install them into", "\n"),
            ("wants an API key (HTTP 401)", ""),
            ("API key (Enter to go back to the choices):", "test-pty-value-99\n"),
        ],
        &[("HOME", r.home().display().to_string())],
    );
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("[ok] models: fake-model-7b"), "{said}");
    assert!(!said.contains("test-pty-value-99"), "the key was typed hidden and is never printed:\n{said}");
    let cfg = lss_core::config::parse_config(&r.read(".config/lss/collector.toml")).unwrap();
    assert_eq!((cfg.pinned_engine(), cfg.pinned_api_key()), (Some((Some(lss_core::engine::EngineKind::Sglang), eng.url())), "test-pty-value-99".to_string()));
    assert!(r.read(".config/lss/rates.toml").contains("0.27"), "{said}");
    assert!(said.contains("Type:   lss"), "{said}");

    // re-run as `lss setup`: an existing collector.toml is only replaced when the person says so
    let (code, said) = pty(
        &format!("{} setup --url {} --kind vllm --api-key test-pty-value-99 --skip-cost", r.home().join(".local/bin/lss-collector").display(), eng.url()),
        &[("Replace it", "n\n")],
        &[("HOME", r.home().display().to_string())],
    );
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("kept ") && r.read(".config/lss/collector.toml").contains("kind = \"sglang\""), "{said}");
}

#[test]
fn with_no_terminal_the_wizard_never_takes_answers_from_a_pipe() {
    // Under `curl ... | bash` on a machine with no terminal (CI, a cron job), stdin is the rest of
    // the install script. The wizard must take its defaults, not read "answers" out of the pipe.
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let r = Real::new("notty");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lss-collector"));
    cmd.args(["setup", "--no-service", "--skip-cost"])
        .env("HOME", r.home())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LSS_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // SAFETY: setsid in the child before exec only detaches it from any controlling terminal
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"q\nq\nq\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "a 'q' read from the pipe would have stopped it (exit 3):\n{said}");
    assert!(said.contains("no terminal to ask"), "{said}");
    assert!(r.home().join(".config/lss/collector.toml").exists(), "{said}");
}

/// card #328: is `pid` a live process? A zombie (state Z) is gone: it holds no port any more, and
/// only waits to be reaped by whoever adopted it.
fn alive(pid: u32) -> bool {
    Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().is_ok_and(|o| {
        let st = String::from_utf8_lossy(&o.stdout).trim().to_string();
        !st.is_empty() && !st.starts_with('Z')
    })
}

/// Start `argv0 sleep 600` detached (not our child), returning its pid.
fn detached(argv0: &str) -> u32 {
    let out = Command::new("bash").arg("-c").arg(format!("nohup bash -c 'exec -a {argv0} sleep 600' >/dev/null 2>&1 & echo $!")).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Start `sleep 600` detached, showing `argv` (spaces allowed) as its command line.
fn detached_as(argv: &str) -> u32 {
    let out = Command::new("bash").arg("-c").arg("nohup bash -c 'exec -a \"$1\" sleep 600' _ \"$1\" >/dev/null 2>&1 & echo $!").arg("_").arg(argv).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[test]
fn uninstall_stops_a_collector_started_by_hand_and_leaves_one_that_is_not_this_installs() {
    // v1.2.1 (tb lss #302): --uninstall left a hand-started lss-collector (no pid file) running
    // with /health up. It now stops one that is certainly this install's - its --config in the
    // install's config dir, or no --config and the install's own binary - and leaves any other
    // lss-collector running with its pid and how to stop it. A process that merely MENTIONS
    // lss-collector (tail -f of its log) is not touched or mentioned.
    let r = Real::new("handstart");
    let home = r.home();
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let with_config = detached_as(&format!("{}/lss-collector --config {}/.config/lss/collector.toml", bin.display(), home.display()));
    let plain = detached_as(&format!("{}/lss-collector", bin.display()));
    let other = detached_as(&format!("{}/elsewhere/lss-collector --config {}/elsewhere/collector.toml", r.dir.display(), r.dir.display()));
    let bystander = detached_as(&format!("tail -f {}/.local/state/lss/lss-collector.log", home.display()));
    for p in [with_config, plain, other, bystander] {
        assert!(alive(p), "stand-in {p} runs");
    }
    let out = Command::new("bash").arg(repo().join("install.sh")).args(["--uninstall", "--yes"]).env("HOME", &home).env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME").env("LSS_TTY", "/nonexistent").output().unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let (a, b, c, d) = (alive(with_config), alive(plain), alive(other), alive(bystander));
    for p in [with_config, plain, other, bystander] {
        let _ = Command::new("kill").arg(p.to_string()).status();
    }
    assert!(out.status.success(), "{said}");
    assert!(!a && said.contains(&format!("stopped a collector started by hand (pid {with_config}:")), "its --config is this install's: stopped\n{said}");
    assert!(!b && said.contains(&format!("stopped a collector started by hand (pid {plain}:")), "this install's binary, default config: stopped\n{said}");
    assert!(c && said.contains(&format!("left pid {other} alone")) && said.contains(&format!("kill {other}")), "another install's collector keeps running, and the reader is told how to stop it\n{said}");
    assert!(d && !said.contains(&bystander.to_string()), "a process that only mentions lss-collector is none of our business\n{said}");
}

#[test]
fn a_service_manager_taking_over_first_stops_the_collector_the_nohup_fallback_started() {
    // card #328: install.sh on a box with no user session started the collector with nohup and a
    // pid file (#296 / #320). Run again once systemd --user (or launchd) is there, it enabled the
    // unit while the old nohup collector still held :8099 - two collectors, the new one failing to
    // bind. Now the pid-file collector is stopped (and its pid file removed) BEFORE the unit starts;
    // a pid file naming something that is not an lss-collector (a reused pid) is left alone.
    let r = Real::new("takeover");
    let state = r.home().join(".local/state/lss");
    std::fs::create_dir_all(&state).unwrap();
    let old = detached("lss-collector-old");
    let stranger = detached("some-other-program");
    assert!(alive(old) && alive(stranger), "the two stand-ins are running");
    std::fs::write(state.join("lss-collector.pid"), format!("{old}\n")).unwrap();
    std::fs::write(state.join("lss-collector-2.pid"), format!("{stranger}\n")).unwrap();
    // the service managers: each call logged, and at the moment a unit / agent is started,
    // whether the old collector was still alive then (the ORDER is the point of the card)
    let stub = r.dir.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    let log = r.dir.join("service.log");
    let check = format!("if ps -o stat= -p {old} 2>/dev/null | grep -qv '^Z'; then echo 'OLD COLLECTOR STILL ALIVE' >> {log}; fi", log = log.display());
    exec_file::write_exec(&stub.join("systemctl"), &format!("#!/bin/sh\necho \"systemctl $*\" >> {log}\ncase \"$*\" in *enable*) {check} ;; esac\nexit 0\n", log = log.display()));
    exec_file::write_exec(&stub.join("launchctl"), &format!("#!/bin/sh\necho \"launchctl $*\" >> {log}\ncase \"$*\" in load*) {check} ;; esac\nexit 0\n", log = log.display()));
    exec_file::write_exec(&stub.join("loginctl"), "#!/bin/sh\necho Linger=yes\n");
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap_or_default());
    let eng = Engine::start(None);
    let dist = r.dir.join("dist");
    let (ok, said) = r.bash(&["--binary-dir", dist.to_str().unwrap(), "--yes", "--url", &eng.url(), "--rate", "0.2"], &[("PATH", &path)], false);
    let service_log = std::fs::read_to_string(&log).unwrap_or_default();
    let stranger_survived = alive(stranger);
    let _ = Command::new("kill").arg(stranger.to_string()).status();
    let _ = Command::new("kill").arg(old.to_string()).status();
    assert!(ok, "{said}");
    assert!(service_log.contains("systemctl --user enable --now lss-collector") || service_log.contains("launchctl load"), "the service path was taken: {service_log}\n{said}");
    assert!(!service_log.contains("OLD COLLECTOR STILL ALIVE"), "the unit started while the nohup collector still ran:\n{service_log}\n{said}");
    assert!(said.contains(&format!("stopped the collector started earlier in the background (pid {old})")), "{said}");
    assert!(said.contains(&format!("left pid {stranger} alone")) && stranger_survived, "a pid file naming another program leaves that program running: {said}");
    assert!(!state.join("lss-collector.pid").exists() && !state.join("lss-collector-2.pid").exists(), "both pid files are gone");
    assert!(!alive(old), "the old collector (pid {old}) is gone");
}

#[test]
fn with_no_user_session_the_printed_restart_line_really_restarts_it_with_its_log_and_pid_file() {
    // card #301 (re-verification FAIL): after a reboot the README and the installer said to run
    // '~/.local/bin/lss-collector --config ... &' - no nohup, no log, and the pid file kept naming
    // the pre-reboot process, so --uninstall and the next install (#328) could not find it. The
    // line printed now is the fallback's own start; here it is EXECUTED, as a person would.
    if !cfg!(target_os = "linux") {
        eprintln!("SKIPPED: the no-session (nohup) branch is the Linux one; macOS always has launchd");
        return;
    }
    let r = Real::new("nosession");
    // a systemctl with no user session to talk to: the installer must take the nohup branch
    let stub = r.dir.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    exec_file::write_exec(&stub.join("systemctl"), "#!/bin/sh\necho 'Failed to connect to bus: No medium found' >&2\nexit 1\n");
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap_or_default());
    let eng = Engine::start(None);
    let dist = r.dir.join("dist");
    let (ok, said) = r.bash(&["--binary-dir", dist.to_str().unwrap(), "--yes", "--url", &eng.url(), "--rate", "0.2"], &[("PATH", &path)], false);
    let state = r.home().join(".local/state/lss");
    let pidfile = state.join("lss-collector.pid");
    let first: u32 = std::fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
    let kill = |pid: u32| {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    };
    assert!(ok, "{said}");
    assert!(said.contains("no systemd --user session and no launchd here"), "the nohup branch was taken: {said}");
    assert!(first > 0 && alive(first), "the fallback started the collector and wrote its pid: {said}");
    // the printed restart line: the SAME start - nohup, the log, a fresh pid file
    let home = r.home().display().to_string();
    let want = format!("nohup {home}/.local/bin/lss-collector --config {home}/.config/lss/collector.toml >>{home}/.local/state/lss/lss-collector.log 2>&1 & echo $! > {home}/.local/state/lss/lss-collector.pid");
    let line = said.lines().skip_while(|l| !l.contains("after a restart, start it again with:")).nth(1).unwrap_or("").trim().to_string();
    assert_eq!(line, want, "{said}");
    // "reboot": the collector is gone, the pid file still names it; run the printed line
    kill(first);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let log_before = std::fs::metadata(state.join("lss-collector.log")).map(|m| m.len()).unwrap_or(0);
    let st = Command::new("bash").arg("-c").arg(&line).env("HOME", r.home()).status().unwrap();
    assert!(st.success());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let second: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let args = Command::new("ps").args(["-o", "args=", "-p", &second.to_string()]).output().unwrap();
    let args = String::from_utf8_lossy(&args.stdout).to_string();
    kill(second);
    assert_ne!(second, first, "the pid file names the NEW collector, not the pre-reboot one");
    assert!(args.contains("lss-collector"), "...and that pid is the collector: {args:?}");
    assert!(std::fs::metadata(state.join("lss-collector.log")).map(|m| m.len()).unwrap_or(0) > log_before, "it writes to the log");
    // the README tells a person the same line
    let readme = std::fs::read_to_string(repo().join("README.md")).unwrap();
    assert!(readme.contains("`nohup ~/.local/bin/lss-collector --config ~/.config/lss/collector.toml >>~/.local/state/lss/lss-collector.log 2>&1 & echo $! > ~/.local/state/lss/lss-collector.pid`"), "README step 3 gives the same start line");
    assert!(!readme.contains("`~/.local/bin/lss-collector --config ~/.config/lss/collector.toml &`"), "the old line is gone from the README");
}

#[test]
fn the_nohup_restart_leaves_an_unrelated_process_named_by_the_pid_file_alone() {
    // card #335: with no service manager, install.sh restarts its OWN background collector from
    // lss-collector.pid. A pid file can outlive its process and the pid be reused by something
    // else; before #328 that something was killed unchecked (measured by rv lss-inst-v2: an
    // unrelated `sleep` gone). Only a live lss-collector may be stopped - anything else is left
    // running and said so - and the new collector gets a fresh pid file of its own.
    if !cfg!(target_os = "linux") {
        eprintln!("SKIPPED: the no-session (nohup) branch is the Linux one; macOS always has launchd");
        return;
    }
    let r = Real::new("nohup-stranger");
    let state = r.home().join(".local/state/lss");
    std::fs::create_dir_all(&state).unwrap();
    let stranger = detached("some-other-program");
    assert!(alive(stranger), "the stand-in is running");
    std::fs::write(state.join("lss-collector.pid"), format!("{stranger}\n")).unwrap();
    // a systemctl with no user session to talk to: the installer takes the nohup branch
    let stub = r.dir.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    exec_file::write_exec(&stub.join("systemctl"), "#!/bin/sh\necho 'Failed to connect to bus: No medium found' >&2\nexit 1\n");
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap_or_default());
    let eng = Engine::start(None);
    let dist = r.dir.join("dist");
    let (ok, said) = r.bash(&["--binary-dir", dist.to_str().unwrap(), "--yes", "--url", &eng.url(), "--rate", "0.2"], &[("PATH", &path)], false);
    let stranger_survived = alive(stranger);
    let new: u32 = std::fs::read_to_string(state.join("lss-collector.pid")).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
    let new_args = Command::new("ps").args(["-o", "args=", "-p", &new.to_string()]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    for pid in [stranger, new] {
        if pid != 0 {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
    assert!(ok, "{said}");
    assert!(said.contains("no systemd --user session and no launchd here"), "the nohup branch was taken: {said}");
    assert!(stranger_survived, "pid {stranger} is not an lss-collector - the restart must not kill it:\n{said}");
    assert!(said.contains(&format!("left pid {stranger} alone")), "and it says so: {said}");
    assert!(new != 0 && new != stranger, "a fresh pid file names the NEW collector (got {new}):\n{said}");
    assert!(new_args.contains("lss-collector"), "pid {new} is the new lss-collector: {new_args:?}");
}

/// v1.2.1 (tb lss #302, found by lss-inst-v1): the README's download-then-run form -
/// `curl -fsSLo install.sh <release URL>; bash install.sh` - failed with 'No release binary fits
/// this machine': the stamped DEFAULT_REPO was used only when the script was PIPED. Now any
/// install.sh that is not inside a source checkout downloads from the repository that published
/// it, exactly like the piped form; a clone still goes by its own origin (or builds). Driven for
/// real against a local release server: a stub curl maps the stamped repo's GitHub release URL
/// onto it, so the default-repo path itself is what is exercised (no LSS_RELEASE_BASE).
#[test]
fn a_downloaded_install_sh_installs_from_its_own_release_like_the_piped_form_and_a_clone_keeps_its_origin() {
    let Some(triple) = triple() else {
        eprintln!("SKIPPED: no release triple for this OS/ARCH - install.sh has nothing to download here");
        return;
    };
    let real_curl = String::from_utf8(Command::new("sh").args(["-c", "command -v curl"]).output().unwrap().stdout).unwrap().trim().to_string();
    if real_curl.is_empty() {
        eprintln!("SKIPPED: no curl on this machine");
        return;
    }
    let eng = Engine::start(None);
    let probe = Real::new("dl-probe");
    let (tar, sum) = probe.tarball();
    let asset = format!("lss-{triple}.tar.gz");
    let host = ReleaseHost::start(vec![(asset.clone(), tar), (format!("{asset}.sha256"), format!("{sum}  {asset}\n").into_bytes())]);
    // a curl that sends github.com/acme/public-lss/releases/... (the stamped repo) to the local host
    let stub_curl = |r: &Real| -> String {
        let stub = r.dir.join("stubbin");
        std::fs::create_dir_all(&stub).unwrap();
        exec_file::write_exec(&stub.join("curl"), &format!("#!/bin/bash\nargs=()\nfor a in \"$@\"; do args+=(\"${{a/https:\\/\\/github.com\\/acme\\/public-lss\\/releases/{}}}\"); done\nexec {real_curl} \"${{args[@]}}\"\n", host.base()));
        format!("{}:{}", stub.display(), std::env::var("PATH").unwrap_or_default())
    };
    let run_file = |r: &Real, script: &Path, args: &[&str], path: &str| -> (bool, String) {
        let out = Command::new("bash").arg(script).args(args).current_dir(script.parent().unwrap())
            .env("HOME", r.home()).env("PATH", path).env("LSS_TTY", "/nonexistent")
            .env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME").env_remove("LSS_REPO").env_remove("LSS_RELEASE_BASE").env_remove("LSS_API_KEY")
            .stdin(std::process::Stdio::null()).output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let installed = |r: &Real| ["lss", "lss-collector", "lss-notify.sh"].iter().all(|p| r.home().join(".local/bin").join(p).exists());

    // 1. piped (curl ... | bash): from the stamped repo's release
    let r = Real::new("dl-piped");
    let path = stub_curl(&r);
    let (ok, said) = r.bash(&["--yes", "--no-service", "--url", &eng.url(), "--skip-cost"], &[("PATH", &path)], true);
    assert!(ok && said.contains(&format!("sha256 matches the published checksum ({sum})")) && installed(&r), "piped:\n{said}");

    // 2. downloaded (curl -fsSLo install.sh URL; bash install.sh): the SAME release, the same check
    let r = Real::new("dl-file");
    let path = stub_curl(&r);
    let file = r.dir.join("downloads/install.sh");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, stamped("acme/public-lss")).unwrap();
    let (ok, said) = run_file(&r, &file, &["--yes", "--no-service", "--url", &eng.url(), "--skip-cost"], &path);
    assert!(ok, "a downloaded install.sh must install like the piped one:\n{said}");
    assert!(said.contains(&format!("sha256 matches the published checksum ({sum})")) && installed(&r), "{said}");
    // ...and --repo still overrides the stamped one
    let (_, said) = run_file(&r, &file, &["--dry-run", "--no-service", "--repo", "acme/other"], &path);
    assert!(said.contains("https://github.com/acme/other/releases/latest/download/lss-"), "{said}");

    // 3. inside a clone (a source checkout): its OWN origin decides, never the stamped default
    let r = Real::new("dl-clone");
    let path = stub_curl(&r);
    let clone = r.dir.join("clone");
    std::fs::create_dir_all(&clone).unwrap();
    std::fs::write(clone.join("install.sh"), stamped("acme/public-lss")).unwrap();
    std::fs::write(clone.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::create_dir_all(clone.join("crates/lss-collector")).unwrap(); // THIS project's checkout
    let git = |args: &[&str]| assert!(Command::new("git").args(args).current_dir(&clone).output().unwrap().status.success(), "git {args:?}");
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "https://github.com/acme/clone-lss.git"]);
    let (_, said) = run_file(&r, &clone.join("install.sh"), &["--dry-run", "--no-service"], &path);
    assert!(said.contains("https://github.com/acme/clone-lss/releases/latest/download/lss-"), "a clone downloads from its origin:\n{said}");
    assert!(!said.contains("acme/public-lss"), "a clone never falls back to the stamped repo:\n{said}");
    // a clone whose origin is not on GitHub builds from source (or says it cannot) - still not the stamped repo
    git(&["remote", "set-url", "origin", "https://example.com/somewhere/else.git"]);
    let (_, said) = run_file(&r, &clone.join("install.sh"), &["--dry-run", "--no-service"], &path);
    assert!(!said.contains("acme/public-lss"), "{said}");

    // 4. (lss-inst-v1's edge on 3c2abbd) a downloaded install.sh inside ANOTHER project's git folder
    // - a home folder under dotfiles version control, origin someone/myproj, no Cargo.toml - is not
    // our checkout: it installs from the repository that published it, never someone/myproj's
    let r = Real::new("dl-dotfiles");
    let path = stub_curl(&r);
    let dotfiles = r.dir.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::write(dotfiles.join("install.sh"), stamped("acme/public-lss")).unwrap();
    let dgit = |args: &[&str]| assert!(Command::new("git").args(args).current_dir(&dotfiles).output().unwrap().status.success(), "git {args:?}");
    dgit(&["init", "-q"]);
    dgit(&["remote", "add", "origin", "https://github.com/someone/myproj.git"]);
    let (ok, said) = run_file(&r, &dotfiles.join("install.sh"), &["--yes", "--no-service", "--url", &eng.url(), "--skip-cost"], &path);
    assert!(!said.contains("someone/myproj"), "another project's origin must not pick the release:\n{said}");
    assert!(ok && said.contains(&format!("sha256 matches the published checksum ({sum})")) && installed(&r), "it installs from the stamped repo's release:\n{said}");

    // 5. (lss-inst-v1: mutant 'any Cargo.toml is our checkout' survived) a downloaded install.sh
    // inside ANOTHER Rust project - a Cargo.toml, but not our crates - is not our checkout either:
    // it installs from the repository that published it, and never tries to build that project
    let r = Real::new("dl-other-rust");
    let path = stub_curl(&r);
    let other = r.dir.join("my-rust-project");
    std::fs::create_dir_all(other.join("src")).unwrap();
    std::fs::write(other.join("Cargo.toml"), "[package]\nname = \"my-rust-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(other.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(other.join("install.sh"), stamped("acme/public-lss")).unwrap();
    let (_, said) = run_file(&r, &other.join("install.sh"), &["--dry-run", "--no-service"], &path);
    assert!(said.contains("would download https://github.com/acme/public-lss/releases/latest/download/lss-"), "another project's Cargo.toml is not our checkout:\n{said}");
    let (ok, said) = run_file(&r, &other.join("install.sh"), &["--yes", "--no-service", "--url", &eng.url(), "--skip-cost"], &path);
    assert!(ok && said.contains(&format!("sha256 matches the published checksum ({sum})")) && installed(&r), "it installs from the stamped repo's release:\n{said}");
    assert!(!said.contains("cargo build") && !other.join("target").exists(), "and never builds that project:\n{said}");
}
