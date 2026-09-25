//! card #180 gate 2: `.github/workflows/ci.yml` must stay a VALID workflow. One job-level
//! `if: hashFiles('gate/*') != ''` (5a06b20) made GitHub reject the whole file - every run on main
//! "failed" in 0 s for hours while every agent's local suite was green, because an invalid workflow
//! cannot report its own failure. This test is the check that does not depend on CI running.

use std::path::Path;
use std::process::Command;

fn workflow() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    std::fs::read_to_string(p).expect("ci.yml exists")
}

/// A job-level key sits at exactly 4 spaces under `jobs:`. Expression functions that only exist
/// inside steps (hashFiles, and the step-only contexts) are invalid there.
#[test]
fn no_job_level_condition_uses_a_step_only_function() {
    let wf = workflow();
    let bad: Vec<(usize, &str)> = wf
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with("    if:") && !l.starts_with("     "))
        .filter(|(_, l)| l.contains("hashFiles(") || l.contains("steps.") || l.contains("runner."))
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();
    assert!(bad.is_empty(), "job-level `if:` using a step-only function makes the WHOLE workflow invalid: {bad:?}");
}

/// Where actionlint is installed (a developer's machine), run the real linter too. The build
/// container does not ship it, so this is a no-op there - the structural test above still runs.
#[test]
fn actionlint_passes_where_it_is_installed() {
    let Ok(out) = Command::new("actionlint").arg("-no-color").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml")).output() else {
        eprintln!("actionlint not installed - skipped (the structural test still ran)");
        return;
    };
    assert!(out.status.success(), "actionlint:\n{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// card #302 CI hole: the rust job skips `cargo test` unless its changed-file filter matches. Rust
/// tests READ the gateway directory (gate_deploy_script.rs scans the gateway's gate.py for every
/// env var), so a commit that touches only the gateway must run them: #341 (cff8da5) added
/// MODEL_ALIASES to gate.py, main's CI stayed green
/// because this job skipped, and the next merge went red. The filter is run exactly as CI runs it
/// (`grep -qE` over the changed file names). In an exported copy there is no gate/ at all, and the
/// export strips the alternative, so there the filter must name no gate/ path either.
#[test]
fn a_gate_only_change_runs_the_rust_tests() {
    let wf = workflow();
    let step = wf.find("- name: Did Rust, fixtures, packaging or scripts change?").expect("the rust job's changed-file step");
    let line = wf[step..].lines().find(|l| l.contains("grep -qE '")).expect("its grep -qE filter");
    let start = line.find("grep -qE '").unwrap() + "grep -qE '".len();
    let re = &line[start..start + line[start..].find('\'').unwrap()];
    let triggers = |files: &str| grep_qe(re, files);
    for f in ["crates/lss/src/main.rs", "fixtures/a.json", "packaging/x.example", "scripts/e2e/run.sh", "install.sh", "Cargo.lock", ".github/workflows/ci.yml"] {
        assert!(triggers(f), "{f} must run the rust tests (filter {re})");
    }
    for f in ["docs/ENGINES.md\nREADME.md", "site/ours/index.html"] {
        assert!(!triggers(f), "a docs-only change must not run the rust tests: {f:?} (filter {re})");
    }
    let has_gate = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gate").is_dir();
    if has_gate {
        assert!(triggers("gate/gate.py"), "a gate/-only change must run the rust tests: they read gate/ (filter {re})");
        assert!(triggers("gate/DEPLOY.md\ngate/tests/test_model_aliases.py"), "filter {re}");
    } else {
        assert!(!re.contains("gate/"), "an exported copy has no gate/, so its filter must not name one: {re}");
    }
}

/// `grep -qE <re>` with `input` on stdin, as CI's step runs it: true when a line matched
fn grep_qe(re: &str, input: &str) -> bool {
    use std::io::Write;
    let mut child = Command::new("grep").arg("-qE").arg(re).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().expect("grep runs");
    child.stdin.take().unwrap().write_all(format!("{input}\n").as_bytes()).unwrap();
    child.wait().unwrap().success()
}
