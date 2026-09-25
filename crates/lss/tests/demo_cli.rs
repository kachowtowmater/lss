//! v1.2.1: `lss --demo` needs no collector in BOTH paths. v1.2.0 with no TTY (piped, as here: the
//! test harness gives the binary no terminal) printed COLLECTOR UNREACHABLE and exited 2.

use std::process::Command;

/// the binary, pointed at an address where nothing can answer - a demo must never try it
fn lss() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lss"));
    let home = std::env::temp_dir().join(format!("lss-demo-cli-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    c.env("HOME", &home).env("XDG_CONFIG_HOME", home.join(".config")).env("LSS_URL", "http://127.0.0.1:9");
    c
}

#[test]
fn demo_without_a_terminal_prints_the_sample_status_once_and_exits_0() {
    let out = lss().arg("--demo").output().expect("lss runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{text}\nstderr:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.starts_with("DEMO: built-in sample data"), "{text}");
    assert!(!text.contains("UNREACHABLE"), "{text}");
    // the sample status itself, not just the banner
    assert!(text.contains("\nLLM SERVER STATUS ") && text.contains("\nSPEED ") && text.contains("\nSERVE "), "{text}");
}

#[test]
fn demo_with_json_prints_the_sample_status_document_and_exits_0() {
    let out = lss().args(["--demo", "--json"]).output().expect("lss runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert!(v.get("serve").is_some() && v.get("error").is_none(), "{v}");
}

#[test]
fn without_demo_the_same_unreachable_address_is_still_reported() {
    // the control: the address really is unreachable, so the demo test above proves no fetch
    let out = lss().output().expect("lss runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("UNREACHABLE"));
}
