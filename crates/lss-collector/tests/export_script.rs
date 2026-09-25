//! `scripts/export-public.sh` + `scripts/privacy-check.sh`, driven for real: what is left out,
//! that a PLANTED private word (or home path, or address) fails the export and removes the copy,
//! and that in the real tree nothing of ours is left to find.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A dotted address from its four numbers.
fn ip(parts: [u8; 4]) -> String {
    parts.map(|n| n.to_string()).join(".")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

struct Tree {
    dir: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lss-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = Tree { dir };
        // the PLANTED WORD is assembled at run time, never a literal in this file: in a fresh
        // clone the fallback word list is the committed example, and a literal here would make
        // the export scan its own test source and fail (card #15 re-check at HEAD).
        let planted = ["zebra", "host", "77"].concat();
        // a small product tree with a site of its own
        t.write("src/README.md", "A monitor. The collector listens on 127.0.0.1:8099; examples use 192.0.2.10 and you@example.com.\n");
        t.write("src/crates/x/src/lib.rs", "pub fn f() {}\n");
        t.write("src/gate/gate.py", "print('gateway')\n");
        t.write("src/gate/DEPLOY.md", &format!("how OUR gateway is deployed on {planted}\n"));
        t.write("src/site/ours/collector.toml", &format!("host = \"{planted}\"\nlisten = [\"{}:8099\"]\n", ip([10, 20, 30, 40])));
        t.write("src/target/debug/junk", &format!("{planted}\n"));
        t.write("words", &format!("# our private words\n{planted}\n"));
        std::fs::create_dir_all(t.dir.join("src/scripts")).unwrap();
        std::fs::copy(repo().join("scripts/privacy-check.sh"), t.dir.join("src/scripts/privacy-check.sh")).unwrap();
        t
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Runs the REAL export script over this tree: (exit code, stdout + stderr).
    fn export(&self, dest: &str) -> (i32, String) {
        let out = Command::new("bash")
            .arg(repo().join("scripts/export-public.sh"))
            .arg(self.dir.join(dest))
            .env("LSS_EXPORT_SOURCE", self.dir.join("src"))
            .env("PRIVACY_WORDS", self.dir.join("words"))
            .output()
            .expect("bash runs");
        (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_export_leaves_the_site_out_and_a_planted_private_word_fails_it() {
    let t = Tree::new("planted");
    let (code, out) = t.export("out");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("privacy check clean"), "{out}");
    let has = |rel: &str| t.dir.join("out").join(rel).exists();
    assert!(has("README.md") && has("crates/x/src/lib.rs") && has("scripts/privacy-check.sh"));
    assert!(!has("site") && !has("gate") && !has("target"), "site/ours, gate/ and build output are never exported");

    // a private word planted in an exported file: the export fails, names the line, removes the copy
    let planted = ["zebra", "host", "77"].concat();
    t.write("src/docs/NOTES.md", &format!("line one\nssh {planted} and restart it\n"));
    let (code, out) = t.export("out2");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains(&format!("./docs/NOTES.md:2:ssh {planted}")) && out.contains("FAILED the privacy check"), "{out}");
    assert!(!t.dir.join("out2").exists(), "a tree that did not pass is not left lying around");
    std::fs::remove_file(t.dir.join("src/docs/NOTES.md")).unwrap();

    // the generic patterns need no word list: a home path, a real address, an e-mail, a key
    // (assembled at run time, so that THIS file holds nothing the check would flag)
    let home = ["/Us", "ers/realname"].concat();
    let mail = format!("someone{}realcompany.io", '@');
    let token = format!("gh{}_abcdefgh12345678", 'p');
    let planted: [(&str, String, String); 5] = [
        ("src/a.md", format!("see {home}/notes\n"), home.clone()),
        ("src/b.md", format!("the box is {}\n", ip([10, 20, 30, 40])), ip([10, 20, 30, 40])),
        ("src/c.md", format!("on the VPN it is {}\n", ip([100, 64, 17, 9])), ip([100, 64, 17, 9])),
        ("src/d.md", format!("mail {mail}\n"), mail.clone()),
        ("src/e.md", format!("token {token}\n"), token.clone()),
    ];
    for (file, text, needle) in &planted {
        t.write(file, text);
        let (code, out) = t.export("out3");
        assert_eq!(code, 1, "{needle}: {out}");
        assert!(out.contains(needle.as_str()), "{needle}: {out}");
        std::fs::remove_file(t.dir.join(file)).unwrap();
    }
    // and what is NOT private passes: loopback, the documentation ranges, example.com, /home/you
    t.write("src/ok.md", "127.0.0.1 0.0.0.0 192.0.2.7 198.51.100.1 203.0.113.9 you@example.com /home/you/.config uses: actions/checkout@v4\n");
    assert_eq!(t.export("out4").0, 0);

    // it never writes into a directory that already holds something
    t.write("full/keep.txt", "mine\n");
    let (code, out) = t.export("full");
    assert_eq!(code, 2, "{out}");
    assert!(t.dir.join("full/keep.txt").exists());
}

#[test]
fn a_direct_run_with_no_word_list_still_reports_real_hits_instead_of_crashing() {
    // card #67: privacy-check.sh run DIRECTLY (not through export-public.sh, which always
    // supplies a words file) in a tree that has the committed packaging/privacy-words.example
    // but no real .privacy-words - exactly a fresh clone. The old guard on line 56 only spoke
    // up when BOTH files were missing, so here (example present, real one absent) it fell
    // through to `<"$WORDS_FILE"` with nothing there, crashing under `set -e` and discarding
    // every generic-pattern hit with it (verifier-3, verifier-2 both reproduced this and moved
    // the card back to TODO). The fix fires the guard on WORDS_FILE alone.
    let dir = std::env::temp_dir().join(format!("lss-privacy-direct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join("packaging")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    // the committed fallback list must exist here too - that presence is exactly what made the
    // old guard go quiet instead of firing.
    std::fs::copy(repo().join("packaging/privacy-words.example"), dir.join("packaging/privacy-words.example")).unwrap();
    // a real hit that needs NO word list: a plain (non-documentation) IPv4 address.
    let needle = ip([10, 20, 30, 40]);
    std::fs::write(dir.join("notes.md"), format!("the box is at {needle}\n")).unwrap();
    // no .privacy-words anywhere, and PRIVACY_WORDS unset: this is the direct-run, no-list case.

    let out = Command::new("bash")
        .arg(dir.join("scripts/privacy-check.sh"))
        .arg(&dir)
        .env_remove("PRIVACY_WORDS")
        .output()
        .expect("bash runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains(&needle), "the real generic-pattern hit must still be reported, not swallowed:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("no .privacy-words found"), "the operator must be told only generic patterns ran:\nstderr:\n{stderr}");
    // and it must not have crashed on the way there: no bash redirection-error noise on stderr
    assert!(!stderr.contains("No such file or directory"), "stderr:\n{stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn watch_check_with_an_empty_or_comment_only_source_list_writes_empty_sources_not_a_crash() {
    // card #116 + #141: /bin/bash 3.2.57 (macOS's shipped bash) throws "unbound variable" on
    // `"${entries[@]}"` under `set -u` when `entries` is an EMPTY array - and an empty, or
    // all-comments, watch-sources.txt is exactly the honest first-run state before anyone has
    // configured a source. That crashed the whole sweep with no output file at all, despite the
    // script's own comment two lines above claiming to have already dodged this trap.
    //
    // WHICH BASH THIS PROVES ANYTHING ON (card #141, item 2): this test shells out to whatever
    // `bash` is first on PATH - in the suite that runs in CI that is GNU bash 5.2, where an empty
    // `"${entries[@]}"` under `set -u` is an ordinary NO-OP (measured on 5.2.21, `BASH_COMPAT=32`
    // included: rc 0). So the `Some(0)` assertion below, ON ITS OWN, cannot fail on a 5.2 box and
    // is NOT what protects the guard. What protects it is the `_first="${entries[0]}"` line that
    // card #141 added to scripts/watch-check.sh's else-branch: a DIRECT subscript under `set -u`
    // traps on 5.2 AND 3.2 alike, so deleting the empty-list guard now crashes here too. Teeth
    // re-proven by hand on GNU bash 5.2.21: guard present -> "wrote 0 sources", rc 0; guard
    // replaced by `if false` -> "entries[0]: unbound variable", rc 1.
    let dir = std::env::temp_dir().join(format!("lss-watch-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sources = dir.join("sources.txt");
    std::fs::write(&sources, "# nothing configured yet\n\n").unwrap();
    let out = dir.join("watch.json");

    let result = Command::new("bash")
        .arg(repo().join("scripts/watch-check.sh"))
        .env("WATCH_SOURCES", &sources)
        .env("WATCH_OUT", &out)
        .output()
        .expect("bash runs");
    let text = format!("{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    // card #132/#149: this test assumed `jq` is installed, so it FAILED on any clean machine -
    // including the container a stranger builds our published copy in, where it was one of the
    // reds that made the exported tree's own suite non-green. `jq` is an optional tool for one
    // script, not a condition for the suite: without it the honest behaviour is a clear refusal
    // naming the missing tool, and the empty-list guard this test is about is asserted where jq
    // exists.
    let has_jq = Command::new("sh").arg("-c").arg("command -v jq").output().map(|o| o.status.success()).unwrap_or(false);
    if !has_jq {
        assert!(text.contains("jq is required"), "without jq the sweep must say so plainly, not crash:\n{text}");
        assert!(!text.contains("unbound variable"), "even the no-jq path must not trip over an empty list:\n{text}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert_eq!(result.status.code(), Some(0), "an empty source list must not crash the sweep:\n{text}");
    assert!(!text.contains("unbound variable"), "{text}");
    let written = std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("watch.json was never written: {e}\n{text}"));
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid json");
    assert_eq!(parsed["sources"].as_array().map(|a| a.len()), Some(0), "{written}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_e2e_gates_red_evidence_is_left_out_and_the_gate_itself_ships() {
    // card #324: scripts/e2e/RED-on-<sha>.txt is the saved output of the acceptance gate failing
    // BEFORE the work - evidence for the development record, not something a stranger runs. Any
    // such file is left out, whatever its sha; the gate's own files beside it all ship, and a
    // file that merely has RED in its name elsewhere is not touched.
    let t = Tree::new("red-evidence");
    t.write("src/scripts/e2e/run.sh", "#!/bin/sh\necho gate\n");
    t.write("src/scripts/e2e/README.md", "The gate.\n");
    t.write("src/scripts/e2e/RED-on-5e4b92f.txt", "FAIL  S2.A1 ...\n");
    t.write("src/scripts/e2e/RED-on-0123abc.txt", "FAIL  S3.C1 ...\n");
    t.write("src/docs/RED-notes.txt", "a doc that happens to be called RED\n");
    let (code, out) = t.export("out");
    assert_eq!(code, 0, "{out}");
    let has = |rel: &str| t.dir.join("out").join(rel).exists();
    assert!(!has("scripts/e2e/RED-on-5e4b92f.txt") && !has("scripts/e2e/RED-on-0123abc.txt"), "the RED evidence must not ship");
    assert!(has("scripts/e2e/run.sh") && has("scripts/e2e/README.md"), "the gate itself ships");
    assert!(has("docs/RED-notes.txt"), "only scripts/e2e/RED-*.txt is left out");
}

#[test]
fn in_the_real_tree_nothing_of_ours_is_left_to_find() {
    // site/ and gate/ are excluded from the export, so the real tree must pass the check
    // outright: a success here is the card-15 contract (the public export fails closed on
    // nothing outside the exclusions, and the exclusions hold).
    let dest = std::env::temp_dir().join(format!("lss-export-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    // card #149: LSS_EXPORT_ALLOW_NO_WORDS is set because this test runs wherever it lives -
    // INCLUDING inside the exported tree, which by design has no .privacy-words (it is
    // git-ignored). Card #144 made a missing word list a refusal (exit 4), which is right for a
    // human about to publish and wrong for a test asserting what the tree CONTAINS. The refusal
    // policy itself is covered by `a_missing_private_word_list_refuses_to_export` below.
    let out = Command::new("bash")
        .arg(repo().join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output()
        .expect("bash runs");
    let hits = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "the real-tree export must pass the privacy check:\n{hits}");
    assert!(!dest.join("site/ours").exists() && !dest.join("gate").exists());
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn the_exported_ci_workflow_names_no_gate_job_or_path() {
    // card #158: the export drops gate/ entirely, but the shipped .github/workflows/ci.yml kept
    // running a 'gate pytest' job (working-directory: gate) and gate/** path filters anyway.
    // Card #143 already guards the job to SKIP when gate/ is absent, which stops it failing -
    // but the file should not still name a directory the copy does not ship, so
    // scripts/export-public.sh now strips the job and its path filters from the exported copy.
    let dest = std::env::temp_dir().join(format!("lss-export-ci-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let out = Command::new("bash")
        .arg(repo().join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output()
        .expect("bash runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    let ci = std::fs::read_to_string(dest.join(".github/workflows/ci.yml")).expect("exported ci.yml exists");
    assert!(!ci.contains("gate:"), "the exported workflow still declares a gate job:\n{ci}");
    assert!(!ci.contains("working-directory: gate"), "the exported workflow still cd's into gate/:\n{ci}");
    // card #160: a substring check for the one literal "gate/**" filter missed any differently
    // shaped filter under the same list ("gate/tests/**", a different glob, different quoting).
    // Scan every NON-COMMENT line for a bare "gate/" instead - comments are allowed to name the
    // exclusion (disclosure, same as the word list naming what it forbids), real YAML content is
    // not.
    let bad: Vec<&str> = ci.lines().filter(|l| !l.trim_start().starts_with('#') && l.contains("gate/")).collect();
    assert!(bad.is_empty(), "the exported workflow still references gate/ outside a comment:\n{}", bad.join("\n"));
    // the two named jobs must still be exactly rust + exported-tree - nothing silently dropped
    assert!(ci.contains("  rust:") && ci.contains("  exported-tree:"), "the transform must not touch the jobs it keeps:\n{ci}");

    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn the_ci_strip_catches_a_gate_path_filter_shaped_differently_than_the_literal_case() {
    // card #160's own mutation: the first strip matched only the exact source line "gate/**", so
    // a source ci.yml written with a MORE SPECIFIC filter under the same list - "gate/tests/**" -
    // would pass both the old strip and the old test untouched. Build a synthetic source (same
    // pattern as a_missing_private_word_list_refuses_to_export) whose ci.yml carries exactly
    // that shape, export it for real, and assert the result carries no gate/ reference.
    //
    // card #165 (verifier-2): #160's fix made the double quote optional but matched nothing
    // else, so a SINGLE-quoted filter - equally legal YAML - slipped through untouched (measured
    // on a real export). Added here rather than as a third test: same mutation class (a
    // differently-shaped path-filter item), one synthetic source proves both quote styles at
    // once.
    let src = std::env::temp_dir().join(format!("lss-export-ci-mutate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(src.join("scripts")).unwrap();
    std::fs::create_dir_all(src.join("packaging")).unwrap();
    std::fs::create_dir_all(src.join(".github/workflows")).unwrap();
    std::fs::copy(repo().join("scripts/export-public.sh"), src.join("scripts/export-public.sh")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), src.join("scripts/privacy-check.sh")).unwrap();
    std::fs::copy(repo().join("packaging/privacy-words.example"), src.join("packaging/privacy-words.example")).unwrap();
    std::fs::write(src.join(".github/workflows/ci.yml"), "\
on:\n  push:\n    paths:\n      - \"crates/**\"\n      - \"gate/tests/**\"\n      - 'gate/singlequoted/**'\n\njobs:\n  rust:\n    steps:\n      - run: echo hi\n  gate:\n    if: hashFiles('gate/*') != ''\n    steps:\n      - working-directory: gate\n        run: pytest -q\n").unwrap();

    let dest = src.with_extension("out");
    let _ = std::fs::remove_dir_all(&dest);
    let out = Command::new("bash")
        .arg(src.join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_SOURCE", &src)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output()
        .expect("bash runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    let ci = std::fs::read_to_string(dest.join(".github/workflows/ci.yml")).expect("exported ci.yml exists");
    assert!(!ci.contains("gate/tests/**"), "the differently-shaped filter survived the strip:\n{ci}");
    assert!(!ci.contains("gate/singlequoted"), "the single-quoted filter survived the strip:\n{ci}");
    assert!(!ci.contains("gate:"), "the gate job survived the strip:\n{ci}");
    assert!(ci.contains("rust:"), "the strip must not touch the job it keeps:\n{ci}");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn the_exported_docs_never_point_a_reader_at_a_path_the_export_does_not_carry() {
    // card #83: gate/ and site/ are excluded from the export, so a reader of the EXPORTED tree
    // must never be sent to a file under either. The RUNBOOK is followed mid-incident, and
    // "rollback is in gate/DEPLOY.md" is a dead end for anyone who got the public copy.
    //
    // SCOPE, card #200 (this replaces the markdown-only scope this test carried until now, which
    // PR #9 had already widened before it was superseded): EVERY TEXT FILE THE EXPORT SHIPS, not
    // only the .md. A reader follows a shipped `.sh` header, a `.toml` example and a `.txt` just
    // as literally as a RUNBOOK, and until now a dangling path in any of them was invisible to
    // this ruler. Measured on the real export while widening it: 162 files ship, and 7 of them are
    // .md - the ruler was looking at 4% of what it is supposed to guard.
    //
    // What is NOT scanned, and why each is a category rather than a convenience:
    //   * DATA formats (.json .lock .csv .plist) and PATTERN files (.gitignore). A path in these
    //     is a value or a glob that some program consumes - never a sentence a reader follows.
    //
    // card #205: .rs IS scanned now - the debt this test recorded at #200 is paid. Rust source
    // was the one shipped text the ruler could not see, and it was hiding the single most
    // user-facing dangling path in the product: the `omp_default_mismatch` ALERT TEXT told the
    // reader to "run scripts/omp-sync-default-model.sh", a script the export drops. A monitor
    // that answers a stranger's alarm with the name of a file they do not have is lying to them,
    // and no amount of doc hygiene elsewhere covers for it. The message now states the general
    // truth instead ("re-point omp's default at the served id"), and this ruler is what stops it
    // coming back.
    //
    // .rs is scanned with ONE narrowing, and it is a category, not a list of files:
    //   * in `src/`, the WHOLE LINE is read - a string literal there can reach the user, and the
    //     alert message above is exactly such a literal. That is the case this scope exists for.
    //   * in a `tests/` directory, only COMMENT text is read. An integration test's string
    //     literals are inputs to a synthetic fixture tree it builds under a temp dir:
    //     `dir.join("docs/ghost.md")` CREATES a file in a scratch repo, it does not point anyone
    //     into this one, and `assert!(!dest.join("site/ours").exists())` is an assertion that a
    //     path is ABSENT. Reading those as pointers is what produced 47 of the 56 hits measured
    //     while turning .rs on - every hit outside src/ came from a `tests/` file, and the great
    //     majority of those were literals of exactly that kind. Comments in a test are prose and are
    //     still read - crates/lss/tests/render.rs pointed at a template name through one.
    //     KNOWN COST: a genuinely dangling path written in a test's string literal is not caught.
    //     The one that mattered lived in src/, which is read whole.
    //
    // Scripts do refer to their own repo paths internally (and to host-side paths like
    // $HOME/the gateway/keys, which merely contain the letters "gate/"); that is handled by the
    // same question everything else is judged on - does the export CARRY the path - plus the two
    // shape rules below (a glob is not a pointer; a regex-escaped path is the path it escapes).
    //
    // The two forms that are NOT a dangling path and stay allowed:
    //   * an HTTP route - the gateway's own `/gate/health`, a URL path on the gateway, always
    //     preceded by a slash;
    //   * prose that NAMES the excluded directory to disclose the exclusion, written without a
    //     trailing token ("all of `gate/`, whose tests…").
    //
    // card #190: this ruler used to hardcode `["gate", "site"]`, so it could only ever catch TWO
    // of the export's exclusions - and the export excludes SEVEN paths, five of them under
    // scripts/ and crates/. It proved it: docs/MODEL-SWAP.md, docs/RUNBOOK.md and
    // docs/OMP-DEFAULT-MODEL.md all shipped pointing at scripts/omp-sync-default-model.sh, which
    // the export drops, and this test stayed green through every one of them. The question is not
    // "does this doc mention gate/ or site/" but "does this doc send a reader to a path THIS
    // EXPORT DOES NOT CARRY", so ask exactly that: derive the roots from the source tree instead
    // of listing them, and check each referenced path against the exported tree itself. A path
    // that IS carried passes whatever directory it lives in; a path that is not fails the same
    // way whatever directory it lives in.
    use std::fs;
    let dest = std::env::temp_dir().join(format!("lss-export-refs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dest);
    let out = Command::new("bash")
        .arg(repo().join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")  // card #149: see the note in the test above
        .output()
        .expect("bash runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    // Every directory a repo path can START with: the source tree's own top-level directories,
    // plus the two that are excluded so completely they may not exist here at all (gate/ does,
    // site/ does not - and "site/ours/..." must still be caught, which is card #83's original
    // case). target/ and dist/ are build output nobody links to in prose.
    let mut roots: Vec<String> = vec!["gate".into(), "site".into()];
    for entry in fs::read_dir(repo()).unwrap().flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "target" || name == "dist" || name == ".git" {
            continue;
        }
        if !roots.contains(&name) {
            roots.push(name);
        }
    }

    // Not scanned - see SCOPE above. Extensions, so a new file of a known data shape is covered
    // the day it lands rather than the day someone remembers to add it.
    let skip_ext = ["json", "lock", "csv", "plist"];
    let skip_name = [".gitignore"];

    // THE ONLY FILE-LEVEL EXEMPTION, and it is four files with one thing in common: each one IS
    // the export's exclusion machinery, or the check that the machinery worked. They name the
    // dropped paths BECAUSE they drop them - `excluded()` is a list of those very paths, and CI's
    // "for gone in ..." step exists to assert they are absent. A ruler that fires on the code
    // that implements the rule is measuring itself. Nothing else may be added here without the
    // same property: it must be a file whose JOB is naming what the export removes.
    //
    // KNOWN COST, stated rather than hidden: this is a whole-file skip, so a genuinely dangling
    // path written in one of these four would not be caught. They are four small files that no
    // reader follows as instructions, which is why the trade is acceptable here and is not
    // offered to anything else.
    let exempt_files = [
        "scripts/export-public.sh",        // excluded() IS the list of dropped paths
        "scripts/privacy-check.sh",        // names site/ours/rates.toml (a file the USER makes) and docs/LICENSE as a path that must NOT be used
        "scripts/hooks/pre-commit-privacy", // its comment restates the excluded set to explain the hook's scope
        ".github/workflows/ci.yml",        // the export job's `for gone in ...` asserts each dropped path is absent
        // card #205, and it has exactly the property demanded above - it is "the check that the
        // machinery worked". THIS FILE holds its own copy of `excluded()` (the `for gone in` list
        // further down), asserts each dropped path is absent from the export, and its comments
        // name those paths BECAUSE it drops them. Turning .rs on made the ruler read the ruler:
        // 28 of the 56 hits were this file reciting the very list it exists to enforce. A test
        // that fires on itself measures nothing.
        "crates/lss-collector/tests/export_script.rs",
    ];

    let mut bad: Vec<String> = Vec::new();
    let mut stack = vec![dest.clone()];
    let mut docs: Vec<std::path::PathBuf> = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let ext = p.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if skip_ext.contains(&ext.as_str()) || skip_name.contains(&name.as_str()) {
                continue;
            }
            let rel = p.strip_prefix(&dest).unwrap_or(&p).to_string_lossy().to_string();
            if exempt_files.contains(&rel.as_str()) {
                continue;
            }
            docs.push(p);
        }
    }
    docs.sort();
    for p in &docs {
        // a file that is not UTF-8 text carries no sentences: read_to_string fails, nothing to scan
        let Ok(text) = fs::read_to_string(p) else { continue };
        // card #205, see SCOPE: in a Rust integration test only the comments are prose. Keyed on
        // the directory, which is what makes a file an integration test in cargo's own terms.
        let rel_file = p.strip_prefix(&dest).unwrap_or(p).to_string_lossy().to_string();
        let comments_only = rel_file.ends_with(".rs") && rel_file.contains("/tests/");
        for (i, full_line) in text.lines().enumerate() {
            let line = if comments_only {
                match full_line.find("//") {
                    Some(c) => &full_line[c..],
                    None => continue,
                }
            } else {
                full_line
            };
            // card #190: docs/ENGINES.md cites where in ANOTHER PROJECT's repository each
            // engine's metric names come from, and three of those citations are `docs/...` -
            // vLLM's, TGI's and Ollama's own, which this repository obviously does not carry and
            // is not claiming to. A citation of upstream source is not a reader being sent
            // anywhere in THIS tree. The allowance is deliberately keyed on an explicit marker
            // the READER sees too ("Sources (upstream <project> repo):"), not on a filename or a
            // bare "Sources:" - so it cannot quietly swallow a real dangling path written in
            // ordinary prose, and a doc that wants the exemption has to say why on the page.
            // card #200 (verifier note on #193): the exemption above used to skip the WHOLE LINE,
            // so anything written on a line that opens with that marker was waved through - a
            // genuinely dangling path could ride along on a deliberately mislabelled line.
            // Tightened to the shape the citations actually have: only a BACKTICKED path, and
            // only after the marker's own `):`, is a citation. `- Sources (upstream vLLM repo):
            // see docs/GONE.md` is still checked, because that path is neither.
            let cite_from = if line.trim_start().starts_with("- Sources (upstream ") {
                line.find("):").map(|x| x + 2)
            } else {
                None
            };
            for root in &roots {
                let needle = format!("{root}/");
                let mut from = 0usize;
                while let Some(at) = line[from..].find(&needle) {
                    let abs = from + at;
                    from = abs + needle.len();
                    // an HTTP route (`/gate/health`) is preceded by a slash: not a file path.
                    // A word character before it means this is the TAIL of a longer path (or of
                    // a longer word) - the outer occurrence is judged on its own.
                    if abs > 0 {
                        let b = line.as_bytes()[abs - 1];
                        if b == b'/' || b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' {
                            continue;
                        }
                        // a backticked citation after the "Sources (upstream <project> repo):"
                        // marker - another project's own file, not a promise about this tree
                        if cite_from.is_some_and(|c| abs > c) && b == b'`' {
                            continue;
                        }
                    }
                    // the rest of the path, slashes included, so a deep path is checked whole.
                    // A backslash is taken along because a path written INSIDE A REGEX escapes its
                    // dots (`\.github/workflows/ci\.yml` in ci.yml's own path filter); dropping the
                    // escapes turns it back into the path it denotes, which then resolves normally.
                    // Without this it truncated at the first `\` and "did not exist" every time.
                    let raw: String = line[from..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-' || *c == '/' || *c == '\\')
                        .collect();
                    // A GLOB IS NOT A POINTER. "gate/tests/**" in a CI path filter, "site/ours/*"
                    // in an exclusion case-arm: these are patterns a program matches with, and no
                    // reader is being sent to them. Judged on the character the path run stops at,
                    // so it cannot be confused with a real file whose name merely contains a star.
                    if line[from + raw.len()..].starts_with('*') {
                        continue;
                    }
                    let tok = raw.replace('\\', "");
                    // sentence punctuation is not part of the path ("...see docs/RUNBOOK.md.")
                    let tok = tok.trim_end_matches(['.', '-', '_', '/']);
                    if tok.is_empty() {
                        continue; // a bare "gate/" in prose is the disclosure, not a pointer
                    }
                    let rel = format!("{root}/{tok}");
                    if dest.join(&rel).exists() {
                        continue; // the export carries it: following this reference works
                    }
                    bad.push(format!("{}:{}: {rel}", p.strip_prefix(&dest).unwrap_or(p).display(), i + 1));
                }
            }
        }
    }
    // The ruler must be shown to have LOOKED at the widened set, or a future edit that quietly
    // narrows it back to .md leaves this test green while guarding 4% of the tree again - which
    // is precisely the state card #200 found it in.
    let scanned: Vec<String> = docs.iter().map(|p| p.strip_prefix(&dest).unwrap_or(p).to_string_lossy().to_string()).collect();
    for ext in [".sh", ".toml", ".txt", ".md", ".yml", ".rs"] {
        assert!(scanned.iter().any(|s| s.ends_with(ext)),
            "the dangling-reference scan covered no {ext} file, so it is not checking what it claims to (card #200). Scanned {} files.", scanned.len());
    }
    // card #205: .rs alone is not enough - a tests/ .rs is read for comments only, so a scan that
    // saw ONLY those would leave the alert-message class (a string literal in src/) unguarded.
    assert!(scanned.iter().any(|s| s.ends_with(".rs") && !s.contains("/tests/")),
        "no src/ .rs file was scanned: the class this scope exists for is unguarded (card #205).");
    assert!(scanned.len() > 50, "only {} files were scanned: the export ships far more than that", scanned.len());

    assert!(bad.is_empty(),
        "the exported files still send readers to paths the export does not carry:\n  {}", bad.join("\n  "));
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn the_exported_tree_names_no_product_name_and_no_gateway_port() {
    // card #163 item 6: the panel's rule (no product name anywhere, no example pointed at our
    // real gateway ports, gateway support documented as optional) needs a test or the next
    // contributor undoes it with nothing going red - proven twice already this session (#160,
    // #165: a fix with no guarding test regressed within the hour). It also pins the sed-damage
    // class verifier-3 found: a blind find-and-replace that leaves grammar like "a the gateway"
    // or "docker logs -t the gateway" is WORSE than the name it replaced, because a reader
    // pastes it and gets an error rather than a working command.
    let dest = std::env::temp_dir().join(format!("lss-export-genericgw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let out = Command::new("bash")
        .arg(repo().join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output()
        .expect("bash runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    let mut bad: Vec<String> = Vec::new();
    let mut stack = vec![dest.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(&dest).unwrap_or(&p).display().to_string();
            // this test file's own source contains the search literals below (it has to, to
            // search for them) - it is not the defect class this ruler exists to catch. The
            // COMPILED BINARY of this suite ships under the same name, so exclude by filename
            // in the export too (card #144 item 1's fresh-clone grep found the .rodata of the
            // test binary carrying the literal).
            if rel.ends_with("export_script.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            // card #144: the forbidden name is ASSEMBLED, never written as a literal. This file
            // ships in the export too, so spelling it here left exactly one hit for the next
            // auditor's `grep -r <name>` over a fresh export - a ruler that trips its own rule
            // and costs somebody ten minutes working out that the only offender is the test.
            // The self-skip above stays (the compiled binary's .rodata is a separate path), and
            // now the source carries nothing to find either.
            let forbidden = [["model", "-gate"].concat(), ["model", "_gate"].concat()];
            let lower = text.to_lowercase();
            if forbidden.iter().any(|f| lower.contains(f.as_str())) {
                bad.push(format!("{rel}: still names the product"));
            }
            for shape in ["a the gateway", "name=the gateway", "://the gateway", "docker logs -t the gateway", "the the gateway"] {
                if text.contains(shape) {
                    bad.push(format!("{rel}: sed-damage shape {shape:?}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "the exported tree still names the product or carries sed-damage:\n  {}", bad.join("\n  "));

    // reader-facing gateway PORTS: an example must not point at our real ones - a synthetic port
    // in a unit test module is not a claim about what a stranger has, so only the files a reader
    // actually reads or edits are checked here. Item 3 fixed FIVE reader-facing files; verifier-2
    // planted ":8096" back into an upcheck script and it PASSED - the exact two files a
    // contributor editing an upcheck script would touch must be guarded, not just the three docs.
    for rel in [
        "README.md",
        "docs/RUNBOOK.md",
        "packaging/collector.toml.example",
        "scripts/lss-upcheck.sh",
        "scripts/install-upcheck.sh",
    ] {
        let p = dest.join(rel);
        if !p.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("8096") && !text.contains("8097"), "{rel} still points an example at our gateway's real port:\n{text}");
    }
    assert!(!dest.join("scripts/omp-sync-default-model.sh").exists(), "omp-sync-default-model.sh must not ship (card #163 item 1: it rewrites OUR omp config against OUR gateway port)");

    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn the_gateways_own_tooling_is_not_in_the_public_copy() {
    // card #180 gate 2, ported from the unmerged fix/143 branch. Card #149 was a PUBLIC BLOCKER
    // of exactly this shape - the export shipped the gateway's deploy script and a test that read
    // a gateway source file, so a stranger's first `cargo test` failed 8 of 10 - and nothing in
    // this suite asserted the fix. The exclusion lived only in export-public.sh's own case list,
    // where a later edit could drop it silently, which is how scripts/shadow-analysis.py (a
    // reader of the GATEWAY's shadow log) was still shipping today, hours after #149 closed.
    // A red suite on a first clone is the strongest signal a project is unmaintained.
    let dest = std::env::temp_dir().join(format!("lss-export-gatetooling-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    // the REAL tree, like in_the_real_tree_nothing_of_ours_is_left_to_find - a synthetic tree
    // could not catch a real script the exclusion list forgot, which is the whole defect here.
    // ALLOW_NO_WORDS for that test's stated reason: this suite also runs INSIDE the export.
    let out = Command::new("bash")
        .arg(repo().join("scripts/export-public.sh"))
        .arg(&dest)
        .env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output()
        .expect("bash runs");
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{log}");
    for gone in [
        "gate",
        "site/ours",
        "scripts/gate-deploy.sh",
        "packaging/gate-env.conf.example",
        "scripts/shadow-analysis.py",
        "scripts/omp-sync-default-model.sh",
        "crates/lss-collector/tests/gate_deploy_script.rs",
        // ...and the AGENT-WORKFLOW half of the same class: tooling that exists because several
        // AI workers share one checkout is a fact about how this is DEVELOPED, not the product.
        "scripts/worker-checkout.sh",
        "crates/lss-collector/tests/worker_checkout_script.rs",
        // card #302: publishing tooling - how the public copy itself was published
        "scripts/publish-public.sh",
        // card #324: the e2e gate's RED-first evidence (a transcript of our runs)
        "scripts/e2e/RED-on-5e4b92f.txt",
    ] {
        assert!(!dest.join(gone).exists(), "{gone} drives something the public copy does not ship, so it must not be in it");
    }
    // ...and the exclusion is SURGICAL: the product's own scripts still ship, or this test would
    // pass just as well on an export that shipped nothing at all.
    for kept in ["install.sh", "scripts/export-public.sh", "scripts/privacy-check.sh", "scripts/install-collector.sh"] {
        assert!(dest.join(kept).exists(), "{kept} is the product's own and must ship");
    }
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_missing_private_word_list_refuses_to_export() {
    // card #144's policy, pinned here because card #149 made two tests opt out of it: the tree
    // anyone would publish FROM is a fresh clone, .privacy-words is git-ignored so a fresh clone
    // has none, and the fallback example list cannot contain our host names. Warning and
    // proceeding meant the one case that mattered was the one case the scan was blind in -
    // measured: a fresh-clone export wrote 155 files and reported "clean" while the result
    // carried our host name in 4 files and two model ids we run in 34 more.
    let src = std::env::temp_dir().join(format!("lss-export-nowords-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(src.join("scripts")).unwrap();
    std::fs::create_dir_all(src.join("packaging")).unwrap();
    std::fs::copy(repo().join("scripts/export-public.sh"), src.join("scripts/export-public.sh")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), src.join("scripts/privacy-check.sh")).unwrap();
    std::fs::copy(repo().join("packaging/privacy-words.example"), src.join("packaging/privacy-words.example")).unwrap();
    std::fs::write(src.join("README.md"), "nothing private here\n").unwrap();
    // the destination must not sit INSIDE the source tree - the script refuses that, correctly
    let dest = src.with_extension("out");
    let _ = std::fs::remove_dir_all(&dest);

    let refused = Command::new("bash").arg(src.join("scripts/export-public.sh")).arg(&dest)
        .env("LSS_EXPORT_SOURCE", &src).env_remove("LSS_EXPORT_ALLOW_NO_WORDS")
        .output().expect("bash runs");
    let text = format!("{}{}", String::from_utf8_lossy(&refused.stdout), String::from_utf8_lossy(&refused.stderr));
    assert_eq!(refused.status.code(), Some(4), "a missing word list must REFUSE, not warn and proceed:\n{text}");
    assert!(text.contains("REFUSING"), "{text}");
    assert!(!dest.exists(), "nothing may be written when the check cannot see our names");

    // the override is deliberate and loud, and it does let the export through
    let allowed = Command::new("bash").arg(src.join("scripts/export-public.sh")).arg(&dest)
        .env("LSS_EXPORT_SOURCE", &src).env("LSS_EXPORT_ALLOW_NO_WORDS", "1")
        .output().expect("bash runs");
    let atext = format!("{}{}", String::from_utf8_lossy(&allowed.stdout), String::from_utf8_lossy(&allowed.stderr));
    assert_eq!(allowed.status.code(), Some(0), "{atext}");
    assert!(atext.contains("are NOT"), "the override must still say what is unchecked:\n{atext}");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn the_exported_word_list_example_is_cut_at_the_marker_and_the_cut_is_checked() {
    // card #144 item 7 (lss-builder-3, verifying 7b2e59b): packaging/privacy-words.example is
    // exempt from the scan because it is a word list - and it WAS our word list, so every export
    // shipped our host names, model ids, agent names, account, domain and utility verbatim while
    // saying "clean". The export now cuts it at the marker; this pins the cut AND its check.
    // The private entries are read from the committed file at run time, never written here.
    // The exported tree runs this suite too (card #149), and its example was already cut - there
    // is no seeded block left to test. gate/ is never exported, so it marks the private repo.
    if !repo().join("gate").is_dir() {
        return;
    }
    let real = std::fs::read_to_string(repo().join("packaging/privacy-words.example")).unwrap();
    let cut = "# ==== export-public: CUT HERE ====";
    let (above, below) = real.split_once(&format!("{cut}\n")).expect("the committed example carries the cut marker");
    let entry = |l: &str| {
        let l = l.split('#').next().unwrap_or("").trim().to_string();
        (!l.is_empty()).then_some(l)
    };
    let private: Vec<String> = below.lines().filter_map(entry).collect();
    assert!(private.len() > 5, "expected the seeded private block below the marker, got {private:?}");
    // The part above the marker SHIPS and is exempt from every word scan (it is a word list), so
    // nothing but the three made-up samples may live there - a private word added above the
    // marker is indistinguishable from a sample to any scan, and has to fail here instead.
    let samples: Vec<String> = above.lines().filter_map(entry).collect();
    let expected = [["zebra", "host", "77"].concat(), ["my", "user"].concat(), r"\bproj-\w+".to_string()];
    assert_eq!(samples, expected, "only the made-up samples may sit above the marker: that part of the file is published");

    let t = Tree::new("example-cut");
    t.write("src/packaging/privacy-words.example", &real);
    t.write("words", &format!("{}\n", private.join("\n")));
    let (code, out) = t.export("out");
    assert_eq!(code, 0, "{out}");
    let shipped = std::fs::read_to_string(t.dir.join("out/packaging/privacy-words.example")).unwrap();
    assert_eq!(shipped, above, "the exported example must be exactly the part above the marker");
    let kept: Vec<String> = shipped.lines().filter_map(entry).collect();
    for p in &private {
        assert!(!kept.contains(p), "private entry {p:?} survived the cut");
    }

    // MUTATION: no marker -> the full list would ship -> the export must FAIL and remove the copy.
    t.write("src/packaging/privacy-words.example", &real.replace(&format!("{cut}\n"), ""));
    let (code, out) = t.export("out2");
    assert_eq!(code, 1, "an uncut example must fail the export:\n{out}");
    assert!(out.contains("still names private words"), "{out}");
    assert!(!t.dir.join("out2").exists(), "a tree that did not pass is not left lying around");
}

// ------------------------------------------- card #172: the privacy net now looks at NUMBERS

/// A throwaway tree with a FABRICATED tariff, used to prove the numeric net without putting a
/// single real value in this repository. The numbers below are invented for the test.
fn tariff_sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lss-tariff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join("fixtures")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    std::fs::write(
        dir.join("rates.toml"),
        "name = \"Invented Utility TOU\"\neffective_date = \"2027-03-14\"\n         fixed_usd_per_day = 0.913\nunresolved_usd_per_kwh = 0.00731\n         [[plan.periods]]\nusd_per_kwh = 0.41827\n",
    )
    .unwrap();
    dir
}

fn privacy_run(dir: &Path) -> (i32, String, String) {
    let out = Command::new("bash")
        .arg(dir.join("scripts/privacy-check.sh"))
        .arg(".")
        .current_dir(dir)
        .env("LSS_RATES", dir.join("rates.toml"))
        .env("HOME", dir)  // so a real ~/.config/lss/rates.toml can never leak into the test
        .output()
        .expect("bash runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_planted_tariff_fingerprint_is_refused() {
    // card #172 item 2. THE REAL EVENT: a fixture labelled "synthetic" carried the owner's exact
    // effective date, his daily charge and his off-peak rate, and every check said clean -
    // because the word net cannot see a number. Here the same shape, with invented values.
    let dir = tariff_sandbox("planted");
    std::fs::write(
        dir.join("fixtures/looks_synthetic.json"),
        "{\"rate_name\": \"Test Utility (synthetic)\", \"effective_date\": \"2027-03-14\", \"fixed_usd_per_day\": 0.91}\n",
    )
    .unwrap();
    let (code, out, err) = privacy_run(&dir);
    assert_eq!(code, 1, "a date plus a daily charge is a fingerprint:\n{out}{err}");
    assert!(out.contains("looks_synthetic.json"), "{out}");
    assert!(out.contains("2027-03-14"), "the refusal must name what it matched:\n{out}");
    assert!(err.contains("tariff numbers in force"), "and which source is in force:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_numeric_net_scans_the_privacy_script_itself() {
    // card #144 item 7. THE REAL EVENT: the word net must exempt privacy-check.sh (it names the
    // patterns it forbids), and the numeric net inherited that exemption - so a comment in the
    // script explaining significant figures carried the owner's real unresolved rate at full
    // precision AND its 2-sig-fig rounding, and every export said clean. Same shape, invented values.
    let dir = tariff_sandbox("self");
    let script = dir.join("scripts/privacy-check.sh");
    let mut body = std::fs::read_to_string(&script).unwrap();
    body.push_str("\n# worked example: printf of 0.00731 vs two significant figures, 0.0073\n");
    std::fs::write(&script, body).unwrap();
    let (code, out, err) = privacy_run(&dir);
    assert_eq!(code, 1, "a real tariff value inside the privacy script must fail the check\nstdout: {out}\nstderr: {err}");
    assert!(out.contains("privacy-check.sh"), "the hit must name the script: {out}");
    // and the unmodified script, scanned by itself, carries no tariff value of its own
    let clean = tariff_sandbox("self-clean");
    let (code, out, err) = privacy_run(&clean);
    assert_eq!(code, 0, "the shipped script must not trip its own numeric net\nstdout: {out}\nstderr: {err}");
}

#[test]
fn one_exact_tariff_value_alone_is_refused() {
    // The shape of EVERY real incident so far, and the shape a two-values-per-file rule cannot
    // see: ONE exact value, in a COMMENT explaining why that value must never be used. It has
    // happened five times - the owner's unresolved rate in privacy-check.sh itself, his
    // off-peak rate in a rounding example, his daily charge and adder in a test helper, his
    // effective date as a doc "e.g.", and the justification comment for THIS VERY RULE, which
    // spelled two of his values while explaining that they must not be spelled. A
    // full-precision tariff value does not occur by accident, so one is enough.
    let dir = tariff_sandbox("lone");
    std::fs::write(
        dir.join("fixtures/a_comment.txt"),
        "# the unresolved adder is 0.00731 per kWh - never use the real one\n",
    )
    .unwrap();
    let (code, out, _err) = privacy_run(&dir);
    assert_eq!(code, 1, "one exact value alone must be refused:\n{out}");
    assert!(out.contains("a_comment.txt") && out.contains("0.00731"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_canonical_fakes_and_a_lone_rounded_number_still_pass() {
    // card #172 item 3, the false-positive edge the card named: 0.50 / 0.0100 / 2026-01-01 are
    // the canonical FAKE values (card #129) and must never be patterns, and ONE rounded number
    // on its own is ordinary test data - measured on this repo, a two-rounded-matches rule
    // flagged loadout.rs for carrying 0.008 / 0.01 / 0.25, which is noise.
    let dir = tariff_sandbox("fakes");
    std::fs::write(
        dir.join("fixtures/fake.json"),
        "{\"effective_date\": \"2026-01-01\", \"fixed_usd_per_day\": 0.50, \"usd_per_kwh\": 0.0100}\n",
    )
    .unwrap();
    std::fs::write(dir.join("fixtures/one_rounded.txt"), "a threshold of 0.91 in ordinary test data\n").unwrap();
    // card #172b: a LONGER number that merely CONTAINS a tariff rounding as a prefix is not a
    // tariff. This is a real case: fixtures/benchmark_results_sample.json carries a TTFT
    // percentile whose leading digits equal a tariff's 3-significant-figure rounding, and a
    // substring match counted it as that tariff.
    // Two such values in one file would have been "refused" - and a check that cries wolf on a
    // benchmark fixture is a check everybody learns to ignore.
    std::fs::write(
        dir.join("fixtures/longer_numbers.json"),
        "{\"ttft_p90\": 0.4182799113, \"other\": 0.9137412}\n",
    )
    .unwrap();
    let (code, out, err) = privacy_run(&dir);
    assert_eq!(code, 0, "the fakes and a lone rounding must pass:\n{out}{err}");
    assert!(out.is_empty(), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_rates_file_fails_open_and_says_so_out_loud() {
    // card #172 item 4: a machine with no tariff has nothing to protect and must not start
    // failing - but "clean" must never quietly mean "checked nothing" (card #145's lesson).
    let dir = tariff_sandbox("noopen");
    std::fs::remove_file(dir.join("rates.toml")).unwrap();
    std::fs::write(dir.join("fixtures/whatever.json"), "{\"effective_date\": \"2027-03-14\"}\n").unwrap();
    let out = Command::new("bash")
        .arg(dir.join("scripts/privacy-check.sh"))
        .arg(".")
        .current_dir(&dir)
        .env("HOME", &dir)
        .env_remove("LSS_RATES")
        .output()
        .expect("bash runs");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "no tariff = fail open");
    assert!(err.contains("NOT checked"), "and it must say so:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}


#[test]
fn a_licence_may_name_its_copyright_holder_and_nothing_else_may() {
    // card #188. Adding an MIT LICENSE turned main RED in five of this file's own tests and was
    // refused twice by the pre-commit hook, every time on the same line: the licence's
    // `Copyright (c) <year> <holder>`. The word net knows the owner's handle is private data and
    // cannot tell publication from a leak; the commit went in with SKIP_PRIVACY_SCAN=1, which is
    // the habit a check must never teach. The carve-out is the narrowest shape that can carry a
    // licence, so this test is mostly its NEGATIVE controls - an exemption is worth having only
    // if everything one character away from it still fails.
    //
    // The names here are INVENTED and the word list is this test's own: spelling the real handle
    // would plant it in a committed source file, which is the thing being defended against. The
    // first draft of this test did exactly that and was caught by the check it was testing.
    let dir = std::env::temp_dir().join(format!("lss-privacy-licence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    let holder = "inventedholder";
    let boxname = "inventedbox";
    // the list is named `.privacy-words` because the check excludes that name from its own scan;
    // under any other name the list's own entries are hits (how the first draft failed).
    std::fs::write(dir.join(".privacy-words"), format!("{holder}\n{boxname}\n")).unwrap();

    let run = |path: &str, body: &str| -> (Option<i32>, String) {
        for stale in ["LICENSE", "LICENSE.md", "notes.md"] {
            let _ = std::fs::remove_file(dir.join(stale));
        }
        std::fs::write(dir.join(path), body).unwrap();
        let out = Command::new("bash")
            .arg(dir.join("scripts/privacy-check.sh"))
            .arg(&dir)
            .env_remove("PRIVACY_WORDS")
            .env("LSS_RATES", dir.join("no-such-rates.toml"))
            .output()
            .expect("bash runs");
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
    };

    // the one exemption: a real licence passes.
    let (code, out) = run("LICENSE", &format!("MIT License\n\nCopyright (c) 2026 {holder}\n\nPermission is hereby granted.\n"));
    assert_eq!(code, Some(0), "an MIT LICENSE must pass its own repository's privacy check:\n{out}");

    // ... including when the word list holds only a PREFIX of the holder, which is the ordinary
    // case: a list names the stem of a handle, not every string built on it. The first version of
    // this carve-out required the line to end with the matched word itself, so it passed on the
    // build host (whose fallback list happens to carry the full handle) while the export stayed
    // RED on the seat a release is cut from - green everywhere except where it is used.
    std::fs::write(dir.join(".privacy-words"), format!("{}\n{boxname}\n", &holder[..8])).unwrap();
    let (code, out) = run("LICENSE", &format!("MIT License\n\nCopyright (c) 2026 {holder}\n"));
    assert_eq!(code, Some(0), "a word list holding a PREFIX of the holder must still let the licence through:\n{out}");
    let (code, out) = run("notes.md", &format!("we run as {holder}\n"));
    assert_eq!(code, Some(1), "and that prefix must still catch the holder outside the licence:\n{out}");
    std::fs::write(dir.join(".privacy-words"), format!("{holder}\n{boxname}\n")).unwrap();

    // ... and every near miss still fails.
    let cases: [(&str, &str, String); 5] = [
        ("the holder named anywhere else in the licence", "LICENSE",
         format!("MIT License\n\nCopyright (c) 2026 {holder}\n\nWrite to {holder} for terms.\n")),
        ("anything appended after the holder", "LICENSE",
         format!("MIT License\n\nCopyright (c) 2026 {holder} of {boxname}\n")),
        ("a notice past the top of the file", "LICENSE",
         format!("{}Copyright (c) 2026 {holder}\n", "filler\n".repeat(12))),
        ("a file that is not ./LICENSE", "LICENSE.md",
         format!("MIT License\n\nCopyright (c) 2026 {holder}\n")),
        ("the holder in an ordinary file", "notes.md",
         format!("we run as {holder}\n")),
    ];
    for (what, path, body) in cases {
        let (code, out) = run(path, &body);
        assert_eq!(code, Some(1), "{what}: must still be refused\n{out}");
    }

    // the GENERIC net is not filtered by the carve-out: a licence needs a name, never an address.
    let addr = ip([10, 20, 30, 41]);
    let (code, out) = run("LICENSE", &format!("MIT License\n\nCopyright (c) 2026 {holder}\n\nserver {addr}\n"));
    assert_eq!(code, Some(1), "an address inside a LICENSE must still be refused\n{out}");
    assert!(out.contains(&addr), "and it must be the address that is reported:\n{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// card #244: A MISSING SOURCE LIST IS A CLEAN NO-OP, not a failed unit every hour.
///
/// Measured on 2026-09-23 (lss-builder-3, card #243): install-collector.sh enabled
/// `lss-watch.timer` on the collector host for the first time, and its first run exited 1 with
/// "watch-check: ~/.config/lss/watch-sources.txt does not exist" - on a machine where nobody had
/// configured a source yet. It pages nobody, so it is not an incident; it is a unit that fails
/// every hour and trains a reader to ignore red. Card #116 had already settled this question for
/// the EMPTY list ("the honest first-run state": write `{"sources": []}`, exit 0) and #116's own
/// note said the script was safe to run unconditionally - which was true only if the file existed.
///
/// A missing file is the same fact one step earlier, so it takes the same path, and writing the
/// empty document also ends the collector's companion complaint ("watch.json: No such file").
#[test]
fn watch_check_with_no_source_list_at_all_is_a_clean_no_op() {
    let dir = std::env::temp_dir().join(format!("lss-watch-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // deliberately NOT created: this is the first-run state
    let sources = dir.join("does-not-exist.txt");
    let out = dir.join("watch.json");
    assert!(!sources.exists());

    let result = Command::new("bash")
        .arg(repo().join("scripts/watch-check.sh"))
        .env("WATCH_SOURCES", &sources)
        .env("WATCH_OUT", &out)
        .output()
        .expect("bash runs");
    let text = format!("{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));

    // the no-jq path is the same carve-out card #132/#149 made for the test above: a missing
    // optional tool must refuse clearly, and this card's guard is asserted where jq exists.
    let has_jq = Command::new("sh").arg("-c").arg("command -v jq").output().map(|o| o.status.success()).unwrap_or(false);
    if !has_jq {
        assert!(text.contains("jq is required"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    assert_eq!(result.status.code(), Some(0),
               "a missing source list must be a no-op, not a failed unit every hour:\n{text}");
    assert!(text.contains("no sources configured"),
            "and it must SAY nothing is configured, so the timer's log reads as information:\n{text}");
    assert!(text.contains(&sources.display().to_string()),
            "naming the file a reader has to create:\n{text}");
    // the empty document is written, which is also what stops the collector logging
    // "watch.json: No such file" beside the failing unit
    let written = std::fs::read_to_string(&out)
        .unwrap_or_else(|e| panic!("watch.json was never written: {e}\n{text}"));
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid json");
    assert_eq!(parsed["sources"].as_array().map(|a| a.len()), Some(0), "{written}");

    // and a real list still works afterwards - the no-op must not have become the only path
    std::fs::write(&sources, "# a comment\n").unwrap();
    let again = Command::new("bash")
        .arg(repo().join("scripts/watch-check.sh"))
        .env("WATCH_SOURCES", &sources)
        .env("WATCH_OUT", &out)
        .output()
        .expect("bash runs");
    assert_eq!(again.status.code(), Some(0), "{}", String::from_utf8_lossy(&again.stderr));

    let _ = std::fs::remove_dir_all(&dir);
}

/// card #300: a bare tree holding only privacy-check.sh plus the COMMITTED word list, which is what
/// CI and a fresh clone scan with (neither has a .privacy-words).
fn words_sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lss-words-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join("crates/lss/src")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), dir.join("scripts/privacy-check.sh")).unwrap();
    dir
}

fn words_run(dir: &Path) -> (i32, String) {
    let out = Command::new("bash")
        .arg(dir.join("scripts/privacy-check.sh"))
        .arg(".")
        .current_dir(dir)
        .env("PRIVACY_WORDS", repo().join("packaging/privacy-words.example"))
        .env("HOME", dir) // no real rates file can leak in
        .output()
        .expect("bash runs");
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// A word list NEXT TO a sandbox, never inside it: inside, the list itself would be scanned and hit.
fn words_path(dir: &Path) -> PathBuf {
    dir.with_extension("words")
}

/// `words_run`, with a word list and (optionally) a public slug of the test's own choosing - so a
/// test can prove a rule with NEUTRAL stand-ins instead of spelling (or assembling) real ones.
fn words_run_with(dir: &Path, words: &Path, slug: Option<&str>) -> (i32, String) {
    let mut cmd = Command::new("bash");
    cmd.arg(dir.join("scripts/privacy-check.sh")).arg(".").current_dir(dir).env("PRIVACY_WORDS", words).env("HOME", dir);
    if let Some(slug) = slug {
        cmd.env("LSS_PUBLIC_SLUG", slug);
    }
    let out = cmd.output().expect("bash runs");
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// A word list entry as text a file could contain: `\b` dropped, `\.` unescaped. `None` for an
/// entry that is a real pattern (classes, repeats, alternatives) rather than a spelled word.
fn plantable(entry: &str) -> Option<String> {
    let text = entry.replace(r"\b", "").replace(r"\.", ".");
    text.chars().all(|c| c.is_ascii_alphanumeric() || " ._:-/".contains(c)).then_some(text)
}

#[test]
fn the_committed_word_list_catches_every_class_of_owner_data_the_card_names() {
    // card #300. THE REAL EVENT: a stricter scan of a fresh export found the owner's own Qwen
    // Flash loadout and its serve image tag in the shipped demo data while CI said clean - the
    // committed list named only a longer variant of the id. So EVERY private entry of the
    // committed list is planted ALONE into a shipped-looking source file and must fail the check
    // CI actually runs.
    // card #307: the entries are read from the list itself at run time. This file used to plant
    // eight real values assembled from pieces, which kept the scan quiet and published all eight
    // in the public copy anyway (a joined-literals scan now fails that).
    let text = std::fs::read_to_string(repo().join("packaging/privacy-words.example")).unwrap();
    let entry = |l: &str| {
        let l = l.split('#').next().unwrap_or("").trim().to_string();
        (!l.is_empty()).then_some(l)
    };
    let cut = "# ==== export-public: CUT HERE ====";
    if let Some((_samples, below)) = text.split_once(cut) {
        let private: Vec<String> = below.lines().filter_map(entry).collect();
        let plants: Vec<(String, String)> = private.iter().filter_map(|e| plantable(e).map(|p| (e.clone(), p))).collect();
        assert!(plants.len() >= 20, "expected the private block to spell at least 20 words to plant, got {} of {}", plants.len(), private.len());
        for (entry, value) in &plants {
            let dir = words_sandbox("classes");
            std::fs::write(dir.join("crates/lss/src/demo.rs"), format!("let m = \"{value} said hi\";\n")).unwrap();
            let (code, out) = words_run(&dir);
            assert_eq!(code, 1, "the list entry {entry:?}, planted as {value:?} in a shipped file, must fail the committed-list check:\n{out}");
            assert!(out.contains("./crates/lss/src/demo.rs:1:"), "{entry:?}: the hit must name the file and line:\n{out}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    } else {
        // the EXPORTED copy: its list was cut at the marker, so it must carry the samples only -
        // there is no private entry left to plant, and none may be left to find
        let kept: Vec<String> = text.lines().filter_map(entry).collect();
        assert_eq!(kept.len(), 3, "an exported (cut) word list holds the three made-up samples only, got {kept:?}");
    }
    // and the neutral demo values that replaced them pass
    let dir = words_sandbox("neutral");
    std::fs::write(dir.join("crates/lss/src/demo.rs"), "let m = (\"model-c\", \"model-c-serve:1.0\", \"zeta-7b\", \"qwen2.5-0.5b-instruct\", \"deepseek-r1:latest\");\n").unwrap();
    let (code, out) = words_run(&dir);
    assert_eq!(code, 0, "neutral example values and public model ids must pass:\n{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_private_word_in_a_file_name_fails_even_when_the_contents_are_clean() {
    // card #300: the scan used to read CONTENTS only, so a capture saved under a host's name
    // shipped that name in every listing while its body passed.
    // card #307: a neutral stand-in word and a word list of the test's own - the rule, not our names.
    let dir = words_sandbox("filename");
    std::fs::write(words_path(&dir), "gpuhostalpha\n").unwrap();
    let name = "crates/lss/src/gpuhostalpha_metrics.txt";
    std::fs::write(dir.join(name), "gpu_util 0.5\n").unwrap();
    let (code, out) = words_run_with(&dir, &words_path(&dir), None);
    assert_eq!(code, 1, "a private word in a path must fail:\n{out}");
    assert!(out.contains(&format!("./{name}: private word in the FILE NAME")), "{out}");
    std::fs::remove_file(dir.join(name)).unwrap();
    assert_eq!(words_run_with(&dir, &words_path(&dir), None).0, 0, "and with the file gone the tree is clean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gitignore_keeps_credentials_databases_site_data_and_filled_in_configs_out() {
    // card #300: every packaging/*.example gets copied and filled with real hosts, keys and rates;
    // a copy left in the checkout must be one the next `git add -A` cannot pick up. Checked with
    // git itself against the repository's real .gitignore, in a scratch repository (the test
    // also runs in the exported copy, which has no .git).
    let dir = std::env::temp_dir().join(format!("lss-gitignore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(repo().join(".gitignore"), dir.join(".gitignore")).unwrap();
    let init = Command::new("git").args(["init", "-q"]).current_dir(&dir).output().expect("git runs");
    assert!(init.status.success());
    let ignored = |p: &str| {
        Command::new("git").args(["check-ignore", "-q", "--no-index", p]).current_dir(&dir).status().unwrap().success()
    };
    for p in [
        "site/ours/collector.toml", "site/mybox/rates.toml", ".privacy-words", "keys/k", "keys.json", "server.pem", "tls.key",
        "id_ed25519", ".netrc", "secrets/token", "lss.db", "state.sqlite3", ".env", "alert.env", "collector.toml",
        "packaging/lss.toml", "rates.toml", "gate-env.conf", "watch.json", "watch-sources.txt", "ui.json", "cfg.local.toml",
        "target/debug/lss", ".claude/settings.local.json",
    ] {
        assert!(ignored(p), "{p} must be git-ignored");
    }
    // ...and the tracked templates and the example site stay trackable
    for p in [
        "packaging/collector.toml.example", "packaging/rates.toml.example", "packaging/alert.env.example",
        "packaging/watch.json.example", "packaging/privacy-words.example", "site/example/README.md", "Cargo.toml",
    ] {
        assert!(!ignored(p), "{p} is a tracked template and must NOT be ignored");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_exact_public_repo_slug_in_a_url_or_repo_default_is_publishable() {
    // card #304: the one-line install must name github.com/<owner>/lss, and the owner's handle is
    // a private word. Exempt the exact published slug in exactly two shapes; everything else that
    // carries the handle still fails.
    // card #307: proven with a NEUTRAL owner and host (LSS_PUBLIC_SLUG + a word list of the test's
    // own) - the rule is the same for any handle, and this file no longer spells ours in pieces.
    let handle = "exampleowner";
    let slug = format!("{handle}/lss");
    let pass = [
        format!("curl -fsSL https://github.com/{handle}/lss/releases/latest/download/install.sh | bash"),
        format!("DEFAULT_REPO=\"{handle}/lss\""),
        format!("LSS_REPO={handle}/lss ./install.sh"),
        format!("see https://github.com/{handle}/lss"),
    ];
    let fail = [
        format!("https://github.com/{handle}/llm-server-status"),
        format!("the owner is {handle}"),
        format!("https://github.com/{handle}/lssx"),
        format!("https://github.com/{handle}/lss.git"),
        format!("read {handle}/lss for more"),
        format!("https://github.com/{handle}/lss by {handle}"),
        format!("https://github.com/{handle}/lss on gpuhostalpha"),
    ];
    for (line, want) in pass.iter().map(|l| (l, 0)).chain(fail.iter().map(|l| (l, 1))) {
        let dir = words_sandbox("slug");
        std::fs::write(words_path(&dir), format!("{handle}\ngpuhostalpha\n")).unwrap();
        std::fs::write(dir.join("crates/lss/src/demo.rs"), format!("{line}\n")).unwrap();
        let (code, out) = words_run_with(&dir, &words_path(&dir), Some(&slug));
        assert_eq!(code, want, "{line:?} must {} the check:\n{out}", if want == 0 { "pass" } else { "fail" });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_private_word_split_across_string_literals_is_still_found() {
    // card #307. THE REAL EVENT: tests here assembled real names from pieces - ["x", "y"].concat()
    // - so that "this file holds no literal hit" stayed true; the scan was quiet and the public
    // copy carried three people's names, the owner's name and domain, a host name and the handle.
    // The joined-literals pass puts the pieces back together. Its first version read *.rs only,
    // and the verifier found a host name split in a SHELL COMMENT of privacy-check.sh itself -
    // so every text file is read, in every quoting style. The pieces are written into SANDBOX
    // files at run time (this file holds the neutral word whole, where it is harmless).
    let word = "gpuhostalpha";
    let (a, b) = word.split_at(5);
    let cases: Vec<(&str, String, i32)> = vec![
        ("crates/lss/src/demo.rs", format!("let h = [\"{a}\", \"{b}\"].concat();"), 1),
        ("crates/lss/src/demo.rs", format!("let h = [\n    \"{a}\",\n    \"{b}\",\n].concat();"), 1),
        ("crates/lss/src/demo.rs", format!("const H: &str = concat!(\"{a}\", \"{b}\");"), 1),
        // the verifier's case: an example in a COMMENT of a shell script
        ("scripts/tool.sh", format!("#!/bin/sh\n# e.g. [\"{a}\", \"{b}\"].concat() in Rust\necho ok"), 1),
        ("scripts/tool.sh", format!("#!/bin/sh\nhost='{a}''{b}'\n"), 1),
        ("scripts/tool.py", format!("host = \"\".join(['{a}', '{b}'])\n"), 1),
        ("docs/NOTES.md", format!("Deploy with `(\"{a}\", \"{b}\")` joined.\n"), 1),
        (".github/workflows/x.yml", format!("hosts: [\n  '{a}',\n  '{b}'\n]\n"), 1),
        // controls: pieces that do not spell the word, or that are not joined, pass
        ("crates/lss/src/demo.rs", format!("let h = [\"{a}\", \"-other\"].concat(); // {b}"), 0),
        ("scripts/tool.sh", format!("#!/bin/sh\necho '{a}' then '{b}'\n"), 0),
    ];
    for (path, content, code) in cases {
        let dir = words_sandbox("split");
        std::fs::write(words_path(&dir), format!("{word}\n")).unwrap();
        std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(path), format!("{content}\n")).unwrap();
        let (got, out) = words_run_with(&dir, &words_path(&dir), None);
        assert_eq!(got, code, "{path}: {content:?}:\n{out}");
        if code == 1 {
            assert!(out.contains(&format!("./{path}:")) && out.contains("gpuhostalpha") && out.contains("joined from split literals"), "the hit names the file and the joined word:\n{out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_git_worktrees_dot_git_pointer_file_is_not_scanned() {
    // card #293. THE REAL EVENT: in a git WORKTREE, `.git` is not a directory but a one-line
    // FILE, `gitdir: <absolute path of the main clone>/.git/worktrees/<name>`. The check left out
    // `./.git/*` only, so that file was scanned, and its absolute host path failed every export
    // test run from a worktree - while the same tree was clean from the main clone. Here the
    // main clone lives under a directory named after a planted private word (assembled at run
    // time, never a literal here), so the pointer file carries the word on any machine.
    let planted = ["zebra", "host", "77"].concat();
    let root = std::env::temp_dir().join(format!("lss-wt-{planted}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let main = root.join("main");
    std::fs::create_dir_all(main.join("scripts")).unwrap();
    std::fs::copy(repo().join("scripts/privacy-check.sh"), main.join("scripts/privacy-check.sh")).unwrap();
    std::fs::write(main.join("README.md"), "A monitor. Examples use 192.0.2.10.\n").unwrap();
    std::fs::write(root.join("words"), format!("{planted}\n")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "tree"]);
    git(&main, &["worktree", "add", "-q", "../wt"]);
    let wt = root.join("wt");
    let pointer = std::fs::read_to_string(wt.join(".git")).expect("a worktree's .git is a file");
    assert!(pointer.contains(&planted), "the fixture must carry the word in the pointer file, or this test proves nothing: {pointer}");
    let check = |dir: &Path| {
        let out = Command::new("bash")
            .arg(dir.join("scripts/privacy-check.sh"))
            .arg(".")
            .current_dir(dir)
            .env("PRIVACY_WORDS", root.join("words"))
            .env("HOME", &root) // no real ~/.config/lss/rates.toml in the test
            .output()
            .expect("bash runs");
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let (code, out) = check(&wt);
    assert_eq!(code, 0, "from a worktree, the .git pointer FILE must be left out like the .git directory is:\n{out}");
    assert!(!out.contains("./.git"), "{out}");
    // the main clone (`.git` a directory) stays clean too, and a REAL hit is still found from the
    // worktree: leaving `.git` out must not blind the check to the tree itself
    assert_eq!(check(&main).0, 0);
    std::fs::write(wt.join("notes.md"), format!("deployed on {planted}\n")).unwrap();
    let (code, out) = check(&wt);
    assert_eq!(code, 1, "a planted word in a real file must still fail:\n{out}");
    assert!(out.contains("./notes.md"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_private_word_split_by_plus_chars_bytes_escapes_or_yaml_lists_is_still_found() {
    // card #310 (found verifying #307): the joined pass caught [..]/(..) lists and adjacent
    // quotes, but a word could still ship split by `+`, as a char or byte array, behind \x
    // escapes, as literals on separate lines inside parens, or as a YAML block list - each one
    // below passed at c49a93e (rc 0). The pieces are built at run time (neutral word, as above).
    let word = "gpuhostalpha";
    let (a, b) = word.split_at(5);
    let chars = word.chars().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
    let bytes = word.bytes().map(|c| c.to_string()).collect::<Vec<_>>().join(", ");
    let hex = word.bytes().map(|c| format!("0x{c:02x}")).collect::<Vec<_>>().join(", ");
    let escaped = format!("\\x{:02x}{}", word.as_bytes()[0], &word[1..]);
    let cases: Vec<(&str, String, i32)> = vec![
        ("crates/lss/src/demo.rs", format!("let h = \"{a}\".to_string() + \"{b}\";"), 1),
        ("crates/lss/src/demo.rs", format!("let h = String::from(\"{a}\") + \"{b}\";"), 1),
        ("crates/lss/src/demo.rs", format!("let h: String = [{chars}].iter().collect();"), 1),
        ("crates/lss/src/demo.rs", format!("let h = [&b\"{a}\"[..], b\"{b}\"].concat();"), 1),
        ("crates/lss/src/demo.rs", format!("const H: [u8; {}] = [{bytes}];", word.len()), 1),
        ("crates/lss/src/demo.rs", format!("const H: [u8; {}] = [{hex}];", word.len()), 1),
        ("crates/lss/src/demo.rs", format!("const H: &str = \"{escaped}\";"), 1),
        ("scripts/tool.py", format!("host = \"{a}\" + \"{b}\"\n"), 1),
        ("scripts/tool.py", format!("host = (\"{a}\"\n        \"{b}\")\n"), 1),
        ("docs/hosts.yml", format!("hosts:\n  - \"{a}\"\n  - \"{b}\"\n"), 1),
        // controls: a number array that spells nothing, and pieces joined by a WORD, pass
        ("crates/lss/src/demo.rs", "const H: [u8; 4] = [1, 2, 3, 250];".to_string(), 0),
        ("scripts/tool.py", format!("print(\"{a}\" if x else \"{b}\")\n"), 0),
    ];
    for (path, content, code) in cases {
        let dir = words_sandbox("split310");
        std::fs::write(words_path(&dir), format!("{word}\n")).unwrap();
        std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(path), format!("{content}\n")).unwrap();
        let (got, out) = words_run_with(&dir, &words_path(&dir), None);
        assert_eq!(got, code, "{path}: {content:?}:\n{out}");
        if code == 1 {
            assert!(out.contains(&format!("./{path}:")) && out.contains(word) && out.contains("joined from split literals"), "the hit names the file and the joined word:\n{out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_private_word_split_by_shell_quote_gluing_octal_raw_strings_or_chr_is_still_found() {
    // card #317 (found verifying #310): six more shapes left the check at rc 0. In sh a bare word
    // glued to a quoted one with NO space between IS one word (h=a"b", h="a"b, h=$'\x61'b); an
    // octal escape spells a letter ("\147..."); a Rust raw string with #s (r#"a"#) is a literal
    // too; and chr(N) + "..." builds the word in Python. The pieces are built at run time from
    // the same neutral word as the tests above.
    let word = "gpuhostalpha";
    let (a, b) = word.split_at(5);
    let first = word.as_bytes()[0];
    let rest = &word[1..];
    let cases: Vec<(&str, String, i32)> = vec![
        ("scripts/tool.sh", format!("#!/bin/sh\nh={a}\"{b}\"\n"), 1),
        ("scripts/tool.sh", format!("#!/bin/sh\nh=\"{a}\"{b}\n"), 1),
        ("scripts/tool.sh", format!("#!/bin/bash\nh=$'\\x{first:02x}'{rest}\n"), 1),
        ("scripts/tool.py", format!("host = \"\\{first:03o}{rest}\"\n"), 1),
        ("crates/lss/src/demo.rs", format!("let h = r#\"{a}\"#.to_string() + \"{b}\";"), 1),
        ("scripts/tool.py", format!("host = chr({first}) + \"{rest}\"\n"), 1),
        // controls: a space between a bare word and a quoted one is two shell words; two
        // statements are two words; chr() joined by a bare word joins nothing
        ("scripts/tool.sh", format!("#!/bin/sh\necho \"{a}\" {b}\n"), 0),
        ("scripts/tool.sh", format!("#!/bin/sh\nx={a}; y=\"{b}\"\n"), 0),
        ("scripts/tool.py", format!("print(chr({first}) if x else \"{rest}\")\n"), 0),
    ];
    for (path, content, code) in cases {
        let dir = words_sandbox("split317");
        std::fs::write(words_path(&dir), format!("{word}\n")).unwrap();
        std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(path), format!("{content}\n")).unwrap();
        let (got, out) = words_run_with(&dir, &words_path(&dir), None);
        assert_eq!(got, code, "{path}: {content:?}:\n{out}");
        if code == 1 {
            assert!(out.contains(&format!("./{path}:")) && out.contains(word) && out.contains("joined from split literals"), "the hit names the file and the joined word:\n{out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
