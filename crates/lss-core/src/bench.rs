//! `lss bench`: the repeatable benchmark to run on every NEW model. This module is the pure
//! half: what to run (a plan of harness invocations per profile), when it is safe to run and
//! when to stop, and how a harness result file becomes a normalised SCORECARD. The collector
//! owns the other half (processes, clocks, HTTP).
//!
//! The decode and prefill measurements are NOT reimplemented here: they come from
//! `llm_decode_bench.py` (github.com/local-inference-lab/llm-inference-bench), which `lss bench`
//! wraps. Three small things it does not do are done by the collector itself, through the
//! gateway's trusted port with `X-LSS-Bench: 1`: the sanity checks, and the long-context needle.

use crate::config::BenchConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The header every request the collector sends for a bench carries; a gateway publishing /gate/health v5.2+ files
/// such requests under the user `lss-bench`.
pub const BENCH_HEADER: &str = "X-LSS-Bench";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// ~5 min: warm-up, decode matrix at 1,2,4,8 x context 0 and 16k, prefill 8k + 64k, 3 sanity checks
    Quick,
    /// ~30 min: quick + prefill 128k, the needle at 3 depths, longer cells, server-side
    /// acceptance, KV capacity and tokens per joule over the bench window
    Full,
    /// a pinned accuracy dataset (gsm8k by default): long, run it in a quiet window
    Accuracy,
    /// everything except load: idle gate, lock, incident window, launcher + interpreter + harness
    /// (`--help` only). Proves the setup without sending the model one token.
    DryRun,
}

impl Profile {
    pub fn parse(word: &str) -> Option<Profile> {
        match word {
            "quick" => Some(Profile::Quick),
            "full" => Some(Profile::Full),
            "accuracy" => Some(Profile::Accuracy),
            "dry-run" | "dryrun" | "dry_run" => Some(Profile::DryRun),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Quick => "quick",
            Profile::Full => "full",
            Profile::Accuracy => "accuracy",
            Profile::DryRun => "dry-run",
        }
    }

    pub fn timeout_secs(self, cfg: &BenchConfig) -> u64 {
        match self {
            Profile::Quick => cfg.quick_timeout_secs,
            Profile::Full => cfg.full_timeout_secs,
            Profile::Accuracy => cfg.accuracy_timeout_secs,
            Profile::DryRun => 60,
        }
    }
}

pub const DATASETS: [&str; 3] = ["gsm8k", "mmlu-pro", "gpqa-diamond"];

/// `POST /bench` body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchRequest {
    /// `quick` | `full` | `accuracy` | `dry-run`
    pub profile: String,
    /// start although the server is not idle (the abort-on-traffic watch still runs)
    pub force: bool,
    /// card #14: measure WITH whatever else the box is doing, and say so. Implies `force` (the
    /// idle gate is not a wall here) and stands the abort-on-traffic watch down, because a run
    /// that kills itself the moment somebody else sends a request cannot measure under load.
    /// The scorecard records what the traffic actually was, sampled at every poll.
    pub under_load: bool,
    pub note: String,
    /// accuracy only: `gsm8k` (default) | `mmlu-pro` | `gpqa-diamond`
    pub dataset: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Warmup,
    Decode,
    Prefill,
    Accuracy,
    DryRun,
}

/// One thing the runner does, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// one harness invocation. `args` come after `<python> <harness>`; `output` is a file name
    /// inside the run directory; `max_concurrency` is the most requests this step itself puts
    /// on the engine (what the abort watch allows).
    Harness { name: String, role: Role, args: Vec<String>, output: String, max_concurrency: u32 },
    /// arithmetic, forced tool call, JSON output: through the gateway's trusted port
    Sanity,
    /// needle in a haystack of ~`tokens` tokens at these depths (percent into the prompt)
    Needle { tokens: u64, depths: Vec<u32> },
    /// GARBLED-OUTPUT check: `prompts` English writing tasks of `max_tokens` each, through the
    /// gateway; every answer is scanned for U+FFFD and for CJK text nobody asked for
    Garble { prompts: u32, max_tokens: u32 },
    /// The BUILT-IN mini bench (no external harness): lss's own requests over the OpenAI chat
    /// API, which every engine speaks.
    Mini { name: String, kind: MiniKind },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MiniKind {
    /// one short request nobody measures: the first request after a start is never typical
    Warmup,
    /// `concurrency` identical writing requests at once, `max_tokens` each
    Decode { concurrency: u32, max_tokens: u32 },
    /// one request with a prompt of ~`tokens` tokens and a one-token answer: reading speed
    Prefill { tokens: u64 },
}

impl Step {
    pub fn name(&self) -> String {
        match self {
            Step::Harness { name, .. } => name.clone(),
            Step::Sanity => "sanity checks".into(),
            Step::Needle { .. } => "long-context needle".into(),
            Step::Garble { .. } => "garbled-output check".into(),
            Step::Mini { name, .. } => name.clone(),
        }
    }

    pub fn max_concurrency(&self) -> u32 {
        match self {
            Step::Harness { max_concurrency, .. } => *max_concurrency,
            Step::Sanity | Step::Needle { .. } | Step::Garble { .. } => 1,
            // the mini bench's requests are lss's OWN (counted as such by the watch while they
            // are in flight), so the step itself tolerates nothing extra on the engine
            Step::Mini { .. } => 0,
        }
    }
}

/// `http://127.0.0.1:8090` -> (`127.0.0.1`, Some(8090)); a URL with a path or https is passed
/// whole as `--host` (the harness accepts a full URL).
pub fn split_host_port(url: &str) -> (String, Option<u16>) {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("http://") {
        if !rest.contains('/') {
            if let Some((host, port)) = rest.rsplit_once(':') {
                if let Ok(p) = port.parse::<u16>() {
                    return (host.to_string(), Some(p));
                }
            }
            return (rest.to_string(), None);
        }
    }
    (trimmed.to_string(), None)
}

/// The harness output of the `full` profile's long-conversation step.
pub const LONG_DECODE_FILE: &str = "decode-long.json";

/// 1, 2, 4, 8 … capped at the engine's slots (the last level IS the cap when it is not a power of two).
pub fn concurrency_levels(slots: u32) -> Vec<u32> {
    let cap = if slots == 0 { 8 } else { slots.min(64) };
    let mut out: Vec<u32> = [1, 2, 4, 8, 16, 32, 64].into_iter().filter(|n| *n <= cap.min(8)).collect();
    if cap < 8 && !out.contains(&cap) {
        out.push(cap);
    }
    out
}

fn join(levels: &[u32]) -> String {
    levels.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

/// What `profile` runs, in order. `target` is the ENGINE's URL (`sglang_url`): the harness
/// cannot set request headers, so it is pointed straight at the engine and the abort watch
/// relies on the engine's running count and the gateway's view (see `abort_reason`).
/// The plan when no external harness is configured: a stranger gets a scorecard without
/// installing anything. Writing speed at 1 / 2 / 4 / 8 users at once (capped at the slots), two
/// prompt sizes for reading speed, and the sanity checks. `accuracy` has no built-in form.
pub fn plan_builtin(profile: Profile, slots: u32) -> Vec<Step> {
    let (max_tokens, prompts): (u32, [u64; 2]) = match profile {
        Profile::Full => (256, [2_000, 8_000]),
        _ => (128, [1_000, 4_000]),
    };
    match profile {
        Profile::DryRun | Profile::Accuracy => Vec::new(),
        Profile::Quick | Profile::Full => {
            let mut steps = vec![Step::Mini { name: "warm-up".into(), kind: MiniKind::Warmup }];
            steps.extend(concurrency_levels(slots).into_iter().map(|n| Step::Mini { name: format!("writing speed, {n} at once"), kind: MiniKind::Decode { concurrency: n, max_tokens } }));
            steps.extend(prompts.into_iter().map(|t| Step::Mini { name: format!("reading speed, a {t}-token prompt"), kind: MiniKind::Prefill { tokens: t } }));
            steps.push(Step::Sanity);
            steps
        }
    }
}

/// A writing task long enough that the answer never ends before `max_tokens`.
pub fn mini_decode_body(model: &str, max_tokens: u32) -> Value {
    serde_json::json!({"model": model, "stream": false, "temperature": 0.7, "max_tokens": max_tokens, "messages": [{"role": "user", "content": "Write a long, detailed story about a lighthouse keeper and the sea. Do not stop early."}]})
}

/// A prompt of about `tokens` tokens and a one-token answer: the time is the reading time.
pub fn mini_prefill_body(model: &str, tokens: u64) -> Value {
    // every run reads NEW text (a nonce first): a prefix cache must not answer for the GPU
    let nonce = format!("Document {}-{tokens}. ", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    serde_json::json!({"model": model, "stream": false, "temperature": 0.0, "max_tokens": 1, "messages": [{"role": "user", "content": format!("{nonce}{}\n\nReply with the single word OK.", needle_prompt(tokens, 50))}]})
}

/// One decode cell from the answers of `concurrency` simultaneous requests: (HTTP code,
/// response, seconds). Speeds come from `usage.completion_tokens`, which every OpenAI-compatible
/// server returns; a server that does not is counted by words, marked by `errors` staying 0.
pub fn grade_mini_decode(concurrency: u32, results: &[(u16, Value, f64)]) -> DecodeCell {
    let ok: Vec<(f64, f64)> = results
        .iter()
        .filter(|(code, _, secs)| *code == 200 && *secs > 0.0)
        .map(|(_, v, secs)| {
            let tokens = v["usage"]["completion_tokens"].as_f64().filter(|t| *t > 0.0).unwrap_or_else(|| (answer_text(v).split_whitespace().count() as f64 * 1.3).round());
            (tokens, *secs)
        })
        .collect();
    let errors = (results.len() - ok.len()) as u64;
    if ok.is_empty() {
        return DecodeCell { concurrency, errors, ..Default::default() };
    }
    let r1 = |v: f64| (v * 10.0).round() / 10.0;
    let longest = ok.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    DecodeCell { concurrency, context: 0, per_user_tok_s: r1(ok.iter().map(|(t, s)| t / s).sum::<f64>() / ok.len() as f64), total_tok_s: r1(ok.iter().map(|(t, _)| *t).sum::<f64>() / longest), ttft_ms: None, errors, capacity_limited: false }
}

/// One prefill cell: the prompt's real token count (`usage.prompt_tokens`) over the time the
/// one-token answer took.
pub fn grade_mini_prefill(asked_tokens: u64, code: u16, response: &Value, secs: f64) -> Option<PrefillCell> {
    if code != 200 || secs <= 0.0 {
        return None;
    }
    let tokens = response["usage"]["prompt_tokens"].as_u64().filter(|t| *t > 0).unwrap_or(asked_tokens);
    Some(PrefillCell { tokens, tok_s: (tokens as f64 / secs).round(), ttft_ms: (secs * 1000.0).round() })
}

pub fn plan(profile: Profile, cfg: &BenchConfig, model: &str, slots: u32, target: &str, dataset: &str) -> Vec<Step> {
    let (host, port) = split_host_port(target);
    let base = |extra: &[&str], output: &str| -> Vec<String> {
        let mut a = vec!["--host".to_string(), host.clone()];
        if let Some(p) = port {
            a.extend(["--port".to_string(), p.to_string()]);
        }
        a.extend(["--model".to_string(), model.to_string(), "--display-mode".to_string(), "plain".to_string()]);
        a.extend(extra.iter().map(|s| s.to_string()));
        a.extend(["--output".to_string(), output.to_string()]);
        a
    };
    let levels = concurrency_levels(slots);
    let top = levels.iter().copied().max().unwrap_or(1);
    let harness = |name: &str, role: Role, extra: &[&str], output: &str, max_concurrency: u32| Step::Harness { name: name.into(), role, args: base(extra, output), output: output.into(), max_concurrency };
    let warmup = || {
        vec![
            harness("warm-up (decode)", Role::Warmup, &["--concurrency", "1", "--contexts", "0", "--duration", "5", "--skip-prefill", "--no-hw-monitor"], "warmup-decode.json", 1),
            harness("warm-up (prefill)", Role::Warmup, &["--prefill-only", "--prefill-contexts", "8k", "--prefill-duration", "3", "--no-hw-monitor"], "warmup-prefill.json", 1),
        ]
    };
    match profile {
        Profile::DryRun => vec![Step::Harness { name: "harness --help (no load)".into(), role: Role::DryRun, args: vec!["--help".into()], output: String::new(), max_concurrency: 0 }],
        Profile::Quick => {
            let mut steps = warmup();
            steps.push(harness("decode matrix", Role::Decode, &["--concurrency", &join(&levels), "--contexts", "0,16384", "--duration", &cfg.quick_duration.to_string(), "--skip-prefill"], "decode.json", top));
            steps.push(harness("prefill 8k, 64k", Role::Prefill, &["--prefill-only", "--prefill-contexts", "8k,64k"], "prefill.json", 1));
            steps.push(Step::Sanity);
            steps
        }
        Profile::Full => {
            let mut steps = warmup();
            steps.push(harness("decode matrix", Role::Decode, &["--concurrency", &join(&levels), "--contexts", "0,16384", "--duration", &cfg.full_duration.to_string(), "--skip-prefill"], "decode.json", top));
            // LONG CONVERSATIONS: one user whose conversation already holds 64k / 128k tokens
            // (the 0 and 16k cells are in the matrix above). Its own file: a partial matrix
            // never hides these, and the other way round.
            steps.push(harness("long conversations 64k, 128k", Role::Decode, &["--concurrency", "1", "--contexts", "65536,131072", "--duration", &cfg.full_duration.to_string(), "--skip-prefill"], LONG_DECODE_FILE, 1));
            steps.push(harness("prefill 8k, 64k, 128k", Role::Prefill, &["--prefill-only", "--prefill-contexts", "8k,64k,128k"], "prefill.json", 1));
            steps.push(Step::Sanity);
            steps.push(Step::Garble { prompts: GARBLE_PROMPTS.len() as u32, max_tokens: 1_500 });
            steps.push(Step::Needle { tokens: cfg.needle_tokens, depths: vec![10, 50, 90] });
            steps
        }
        Profile::Accuracy => {
            let set = if DATASETS.contains(&dataset) { dataset } else { DATASETS[0] };
            vec![harness(&format!("accuracy: {set}"), Role::Accuracy, &["--test-profile", set], &format!("accuracy-{set}.json"), top.max(16))]
        }
    }
}

/// argv of one harness step: `[launcher…] python harness args…`.
pub fn command_line(cfg: &BenchConfig, harness_path: &str, args: &[String]) -> Vec<String> {
    let mut words = vec![cfg.python.clone(), harness_path.to_string()];
    words.extend(args.iter().cloned());
    with_launcher(&cfg.launcher, words)
}

/// card #312 (verifier FAIL 2): `ssh host cmd args…` does NOT pass argv through - it joins the
/// words with spaces and the REMOTE SHELL splits them again, so a word holding spaces or shell
/// characters (the key shim is full of `;()[]'`) broke: measured `bash: -c: line 1: syntax error
/// near unexpected token '('`. After an ssh launcher, every word lss adds is shell-quoted, so
/// the remote shell hands the program exactly these words. A word of only safe characters stays
/// as it is: a plain step over ssh is byte-for-byte what it always was. Launchers that pass argv
/// through (systemd-run, `docker exec -i`) get the words unchanged.
pub fn with_launcher(launcher: &[String], words: Vec<String>) -> Vec<String> {
    let joins = launcher_joins(launcher);
    let mut argv: Vec<String> = launcher.to_vec();
    argv.extend(words.into_iter().map(|w| if joins { remote_word(&w) } else { w }));
    argv
}

/// card #333: one word for the REMOTE shell. A path written `~/…` (the `[bench] harness`, kept
/// unexpanded over ssh) means the REMOTE user's home, so the tilde stays OUTSIDE the quotes -
/// `~/'my bench/b.py'` - where the remote shell expands it; everything else is `shell_quote`d.
pub fn remote_word(word: &str) -> String {
    match word.strip_prefix("~/") {
        Some(rest) if !rest.is_empty() => format!("~/{}", shell_quote(rest)),
        _ => shell_quote(word),
    }
}

/// card #333: the `[bench] harness` path as it is handed to the launcher. Over ssh, `~/…` is left
/// for the REMOTE shell (the remote user's home, not the collector's); otherwise `~` is expanded
/// here with `home`, as it always was.
pub fn harness_word(launcher: &[String], raw: &str, home: &str) -> String {
    let raw = raw.trim();
    if launcher_joins(launcher) && raw.starts_with("~/") {
        raw.to_string()
    } else {
        crate::config::expand_home(raw, home)
    }
}

/// An ssh launcher: its argv is re-joined by the REMOTE shell (see `with_launcher`), and the
/// command runs on another machine, in that shell's cwd - the remote user's HOME.
pub fn launcher_joins(launcher: &[String]) -> bool {
    launcher.iter().any(|a| std::path::Path::new(a).file_name().is_some_and(|n| n == "ssh"))
}

/// card #330: where a run's harness steps work on the far side of an ssh launcher -
/// `.cache/lss-bench/<run>`, RELATIVE, so under the remote user's HOME whoever that is (the same
/// absolute path as lss's own run dir would need the same user and a shared filesystem). One
/// directory per run, so a step that writes nothing can never hand back an older run's file.
/// The name is reduced to safe characters and is never empty, "." or "..": `remote_cleanup`
/// removes this directory.
/// card #340: `token` (bench_run: random per run) is appended, `<run>-<token>`: the run name alone
/// is only unique within ONE collector - two collectors benching as the same remote user both
/// have a run 1, shared its directory, and the first to finish removed it under the other. Only
/// its ASCII letters and digits are kept; an empty token adds nothing.
pub fn remote_run_dir(run_name: &str, token: &str) -> String {
    let safe: String = run_name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    let safe = if safe.is_empty() || safe.chars().all(|c| c == '.') { "run".to_string() } else { safe };
    let token: String = token.chars().filter(char::is_ascii_alphanumeric).collect();
    if token.is_empty() {
        format!(".cache/lss-bench/{safe}")
    } else {
        format!(".cache/lss-bench/{safe}-{token}")
    }
}

/// card #330: a step's argv, run inside `rdir` on the remote side: `[ssh…] mkdir -p R && cd R &&
/// <the step's words>`. The `&&` are for the remote shell and go in unquoted; R is quoted. Not an
/// ssh launcher = the argv unchanged (the step already runs in lss's run directory).
pub fn in_remote_dir(launcher: &[String], argv: Vec<String>, rdir: &str) -> Vec<String> {
    if !launcher_joins(launcher) || argv.len() < launcher.len() {
        return argv;
    }
    let q = shell_quote(rdir);
    let mut out = launcher.to_vec();
    out.extend(["mkdir".to_string(), "-p".into(), q.clone(), "&&".into(), "cd".into(), q, "&&".into()]);
    out.extend(argv.into_iter().skip(launcher.len()));
    out
}

/// card #330: the argv that prints one result file of `rdir` on stdout, through the same launcher
/// (`[ssh…] cat R/decode.json`): it reaches the box exactly as the step did, whatever ssh options,
/// port or user the launcher names - scp would need all of those parsed back out of it.
pub fn remote_fetch(launcher: &[String], rdir: &str, file: &str) -> Vec<String> {
    let mut out = launcher.to_vec();
    out.extend(["cat".to_string(), shell_quote(&format!("{rdir}/{file}"))]);
    out
}

/// card #330: removes the run's remote directory once its files are back.
pub fn remote_cleanup(launcher: &[String], rdir: &str) -> Vec<String> {
    let mut out = launcher.to_vec();
    out.extend(["rm".to_string(), "-rf".into(), "--".into(), shell_quote(rdir)]);
    out
}

/// card #339: what the scorecard's harness commit says when the harness runs over an ssh launcher
/// and its checkout's commit could not be read on the far side (no git there, not a checkout, ssh
/// failed) - said, never left blank.
pub const REMOTE_COMMIT_UNKNOWN: &str = "unknown (remote)";

/// card #339: over an ssh launcher the harness checkout is on the OTHER machine, so its commit is
/// asked there, through the same launcher: `[ssh…] git -C <dir of the harness word> rev-parse
/// --short HEAD`. `harness_word` is what the steps get (bench::harness_word), so `~/…` is the
/// REMOTE user's home: its directory goes out as `~/<quoted rest>`, or a bare `~` for the home
/// itself; a word with no `/` is relative to where ssh starts (the remote HOME) - `.`.
pub fn remote_commit_argv(launcher: &[String], harness_word: &str) -> Vec<String> {
    let dir = match harness_word.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((d, _)) => d.to_string(),
        None => ".".to_string(),
    };
    let dir = if dir == "~" { dir } else { remote_word(&dir) };
    let mut out = launcher.to_vec();
    out.extend(["git".to_string(), "-C".into(), dir, "rev-parse".into(), "--short".into(), "HEAD".into()]);
    out
}

/// card #339: the harness commit from `remote_commit_argv`'s stdout (None = it failed): the last
/// non-empty line when it is a short sha (4-40 hex digits), else `REMOTE_COMMIT_UNKNOWN`.
pub fn remote_commit(stdout: Option<&str>) -> String {
    let last = stdout.and_then(|o| o.lines().map(str::trim).rfind(|l| !l.is_empty())).unwrap_or("");
    if (4..=40).contains(&last.len()) && last.chars().all(|c| c.is_ascii_hexdigit()) {
        last.to_string()
    } else {
        REMOTE_COMMIT_UNKNOWN.to_string()
    }
}

/// One word for a POSIX shell: as it is when it holds only safe characters, else single-quoted.
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// card #312: how lss launches one harness step: the argv, and what to write to its stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessLaunch {
    pub argv: Vec<String>,
    /// Some = write this to the child's stdin, then close it (the key, one line); None = no stdin
    pub stdin: Option<String>,
}

/// Runs the harness with `--api-key <the line on stdin>` appended INSIDE Python, so the key is
/// never on the OS command line (`ps`, /proc/PID/cmdline) and never in the environment. The rest of
/// stdin stays for the harness (its self-update prompt reads EOF, as with no stdin at all).
/// `runpy` runs the harness exactly as `python harness.py args` would (`__main__`, same argv[0]).
pub const KEY_SHIM: &str = "import runpy,sys; k=sys.stdin.readline().strip(); sys.argv=sys.argv[1:]+(['--api-key',k] if k else []); runpy.run_path(sys.argv[0], run_name='__main__')";

/// card #312: the engine's API key (`[[engine]] api_key`) for the harness. `llm_decode_bench.py`
/// takes it only as `--api-key` and sends it as `Authorization: Bearer` - the same header the
/// collector's own requests carry. The verifier measured the first version (the key appended to
/// argv) sitting in /proc/PID/cmdline for the whole run; so a keyed step is launched as
/// `[launcher…] python -c KEY_SHIM harness args…` with the key on STDIN (which also crosses an
/// ssh / `docker exec -i` launcher, where an environment variable would not). Only steps that talk
/// to the engine get it - every harness step's `--host` is the ENGINE, never the gateway - and
/// the `--help` dry run gets none. No key = the plain command line, no stdin, as before.
pub fn harness_launch(cfg: &BenchConfig, harness_path: &str, args: &[String], role: Role, key: &str) -> HarnessLaunch {
    let key = key.trim();
    if key.is_empty() || role == Role::DryRun {
        return HarnessLaunch { argv: command_line(cfg, harness_path, args), stdin: None };
    }
    let mut words = vec![cfg.python.clone(), "-c".to_string(), KEY_SHIM.to_string(), harness_path.to_string()];
    words.extend(args.iter().cloned());
    HarnessLaunch { argv: with_launcher(&cfg.launcher, words), stdin: Some(format!("{key}\n")) }
}

/// card #312: the real harness does NOT fail on a refused key: with no (or a wrong) key it prints
/// `WARNING: Only OpenAI /v1 endpoints appear reachable (HTTP 401 on /metrics)`, measures what it
/// can and exits 0 ('Done.'). A run that was refused must not be reported as a measurement, so
/// its log is read for the engine's refusal. Returns the offending line.
pub fn harness_refused(log: &str) -> Option<String> {
    log.lines().find(|l| l.contains("HTTP 401") || l.contains("HTTP 403")).map(|l| l.trim().chars().take(200).collect())
}

/// card #312 (verifier FAIL 1): the hint lss gives when the ENGINE refuses its own preflight
/// request (`GET /v1/models`, then a 1-token chat when that is open) - asked BEFORE the external
/// harness starts, because the real harness does not say "401" when it is refused (it prints
/// "SGLang metrics are disabled", ERROR cells, "Done." and exits 0). None = not a refusal: any
/// other answer (or none) is left to the harness and the sanity checks to report. The key itself
/// is never part of the text.
pub fn preflight_refusal(path: &str, status: u16, key_configured: bool) -> Option<String> {
    if status != 401 && status != 403 {
        return None;
    }
    Some(if key_configured {
        format!("the engine refused the configured key (HTTP {status} on {path}) before the benchmark started - check api_key in the engine's [[engine]] block of collector.toml (`lss setup` tests it), then restart the collector")
    } else {
        format!("the engine wants an API key (HTTP {status} on {path}) and none is configured - set api_key in the engine's [[engine]] block of collector.toml (`lss setup` asks for it), then restart the collector")
    })
}

/// `argv` for a log line or an error message: the value after `--api-key` (or of
/// `--api-key=...`) replaced by `config::redact_secret` of it.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut hide_next = false;
    for a in argv {
        if hide_next {
            out.push(crate::config::redact_secret(a));
            hide_next = false;
        } else if a == "--api-key" {
            out.push(a.clone());
            hide_next = true;
        } else if let Some(v) = a.strip_prefix("--api-key=") {
            out.push(format!("--api-key={}", crate::config::redact_secret(v)));
        } else {
            out.push(a.clone());
        }
    }
    out
}

// ------------------------------------------------------------------ safety

/// What the safety poll sees every 2 s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SafetyObs {
    /// engine totals; None = the engine's metrics could not be read
    pub running: Option<f64>,
    pub queue: Option<f64>,
    /// requests running / queued carrying the PUBLIC lane's priority label. The bench has no key
    /// for that lane and never sends anything down it, so any of these is somebody else's -
    /// whatever the bench itself is doing at the time.
    pub gateway_public: Option<f64>,
    /// requests running / queued carrying the TRUSTED lane's priority label.
    ///
    /// Card #207, 2026-09-23: this is NOT a bench-free number, and the comment that used to sit
    /// here ("the harness goes straight to the engine, so these are never its own") was the bug.
    /// SGLang stamps `priority="0"` on a request that asked for no priority at all, and `"0"` is
    /// exactly this box's TRUSTED lane (`trusted_priority`), so the harness's own
    /// direct-to-engine load lands in this counter. Measured: one 1-token direct POST to the
    /// engine moved `num_requests_total{priority="0"}` 32 -> 33. It must therefore have the
    /// bench's own in-flight subtracted, like the engine total, before it means "other people".
    pub gateway_trusted: Option<f64>,
    /// a gateway publishing /gate/health v5.2+: requests in flight of users other than `lss-bench`; None = older gate
    pub other_users_inflight: Option<u64>,
    /// the step's own gateway requests in flight right now (sanity / needle): 0 or 1
    pub own_gateway_inflight: u64,
    /// card #212: which lane's counter the bench's OWN priority-less requests land in - read
    /// from the lane config (`own_lane_for`), never assumed. #207 hard-wired "trusted", which is
    /// only true where the trusted lane is the engine's default priority "0".
    pub own_lane: OwnLane,
}

/// card #212: the priority label SGLang stamps on a request that asked for none - the harness
/// sends none, so this is where the bench's own direct-to-engine load is counted.
pub const ENGINE_DEFAULT_PRIORITY: &str = "0";

/// Which lane counter holds the bench's own priority-less requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OwnLane {
    /// the trusted lane carries the engine's default priority (this project's own setup, and
    /// the default config) - the historical case #207 fixed
    #[default]
    Trusted,
    /// the PUBLIC lane carries the default priority: the bench's own load is in the public
    /// counter, and subtracting it from trusted instead (#207's assumption) aborts every run
    Public,
    /// neither lane carries it: the bench's own load is in NEITHER lane counter, so both are
    /// foreign whole (the engine-total backstop still bounds everything)
    Neither,
}

/// card #212: read the lane that holds the engine's default priority from the lane config.
pub fn own_lane_for(public_priority: &str, trusted_priority: &str) -> OwnLane {
    if trusted_priority == ENGINE_DEFAULT_PRIORITY {
        OwnLane::Trusted
    } else if public_priority == ENGINE_DEFAULT_PRIORITY {
        OwnLane::Public
    } else {
        OwnLane::Neither
    }
}

/// Everything the BENCH itself can have on the engine at this instant: the harness's direct load
/// (`step_max_concurrency` is an upper bound - it may not have ramped up yet) plus its own
/// gateway request. An upper bound on its own load, and so a LOWER bound on everybody else's.
fn bench_inflight(o: &SafetyObs, step_max_concurrency: u32) -> f64 {
    f64::from(step_max_concurrency) + o.own_gateway_inflight as f64
}

/// Why a running bench must stop NOW, or None.
///
/// Card #207, 2026-09-23 - THE BENCH USED TO SHOOT ITSELF. This function had three independent
/// branches and fell through the first one whenever the authoritative source answered ZERO, so a
/// gateway user table saying "nobody but the bench is here" was overruled two lines later by a
/// lane counter that contains the bench's own requests (see `SafetyObs::gateway_trusted`). Every
/// plain `lss bench quick` died at its first poll, in about two seconds, and reported real
/// traffic that was not there.
///
/// It now reads ONE number - the best one available, chosen by exactly the source precedence
/// `foreign_now` already used for the scorecard (user table > lanes > engine total). One number
/// from one source, so a strong source can no longer be contradicted by a weaker one.
///
/// The engine-total check below is kept as a BACKSTOP rather than a fallback: it is the only way
/// to see somebody who went straight to the engine and so never entered the gateway's user table
/// at all, and it cannot misfire, because what it compares against is the bench's own upper bound.
pub fn abort_reason(o: &SafetyObs, step_max_concurrency: u32) -> Option<String> {
    if let Some((n, source)) = foreign_now(o, step_max_concurrency).filter(|(n, s)| *n >= 0.5 && *s != LOAD_SRC_ENGINE) {
        return Some(match source {
            LOAD_SRC_USERS => format!("real traffic: {n:.0} request(s) in flight from a user that is not the bench (gateway user table)"),
            _ => format!("real traffic: {n:.0} request(s) came in through the gateway while the bench was running"),
        });
    }
    // the engine total is never the precedence WINNER: it is this backstop, which says the same
    // thing in the terms a reader can act on (what was running, against what the bench launched).
    if let Some(r) = o.running {
        let allowed = bench_inflight(o, step_max_concurrency);
        if r > allowed + 0.5 {
            return Some(format!("real traffic: {r:.0} requests running, the bench launched at most {allowed:.0}"));
        }
    }
    None
}

/// #54, 2026-09-21: `raw_growth` (`History::counter_window` on `generation_tokens_total` over
/// the idle window) still includes the collector's OWN periodic probe, which by default runs
/// about as often as the idle window itself - left in, the idle gate would almost always read
/// "busy". `probe_tokens` is the known sum of every probe's own `tokens` field inside that same
/// window (valid or not - an invalidated probe still generated its own real tokens on the
/// engine, it is still not a user), subtracted out with the same
/// `probe::COUNTER_MARGIN_TOKENS` slack the probe's own counter check allows for scrape-timing
/// jitter. Never negative - a probe running exactly the whole window must not manufacture a
/// negative "real" number. `None` propagates: not enough history is not enough history.
pub fn real_tokens_generated(raw_growth: Option<f64>, probe_tokens: f64) -> Option<f64> {
    raw_growth.map(|grown| (grown - probe_tokens - crate::probe::COUNTER_MARGIN_TOKENS).max(0.0))
}

/// What the idle gate looks at before a bench may start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleObs {
    pub serve_up: bool,
    pub running: f64,
    pub queue: f64,
    /// #54, 2026-09-21: `generation_tokens_total`'s growth over the last `idle_secs` - the SAME
    /// evidence the probe's own validity check uses (card #45), because request/status counts
    /// miss what matters: three tiny completed requests used to flip the old `recent_requests >
    /// 0` check although the engine had generated nothing for minutes. `None` = fewer than two
    /// comparable samples in the window yet (the collector or the serve just (re)started) -
    /// treated as NOT shown idle, the same conservative default `History::counter_window`
    /// itself documents; this resolves on its own within `idle_secs`, it is never stuck.
    pub tokens_generated: Option<f64>,
    pub idle_secs: i64,
    /// a gateway publishing /gate/health v5.2+: non-bench requests in flight
    pub other_users_inflight: u64,
}

/// What the idle gate is asking for, in one clause, so every refusal ends the same way.
const IDLE_WAYS_OUT: &str = "Try again when it is quiet, pass --force to start anyway, or pass --under-load to measure WITH the traffic and have the scorecard say so";

/// Ok, or why not (a sentence the CLI prints). `running`/`queue` are the cheap, instant
/// pre-filter (no history needed); the token counter is what actually proves quiet, exactly as
/// `probe::validate`'s counter check proves a probe ran alone.
///
/// Card #14, 2026-09-23 - THE GATE IS A LABEL, NOT A WALL. It stays exactly as strict about what
/// it calls quiet, and a run that passes it is still the gold standard. What changed is what a
/// refusal means: four attempts over three days were refused here, and a three-day survey of the
/// engine's own token counter found the longest flat run in ANY hour was 5m00s with most hours far
/// below - on a box that is also the fleet's only serve, "wait for `idle_secs` of silence" is a
/// requirement at or above the ceiling the machine ever offers. So `skip` (--force or
/// --under-load) is a first-class way through, and everything the gate knows is recorded on the
/// scorecard instead (`Scorecard::background`), where a reader can weigh it.
pub fn idle_gate(o: &IdleObs, skip: bool) -> Result<(), String> {
    if !o.serve_up {
        return Err("the serve is not up: nothing to benchmark".into());
    }
    if skip {
        return Ok(());
    }
    if o.running >= 1.0 || o.queue >= 1.0 || o.other_users_inflight > 0 {
        return Err(format!("the server is busy ({:.0} running, {:.0} queued): a benchmark now would slow real users down and measure their traffic too. {IDLE_WAYS_OUT}", o.running.max(o.other_users_inflight as f64), o.queue));
    }
    let mins = (o.idle_secs + 59) / 60;
    match o.tokens_generated {
        None => Err(format!("not enough history yet to prove the engine has been quiet for {mins} quiet minute(s): try again shortly, or pass --force")),
        Some(tok) if tok > 0.0 => {
            Err(format!("the engine generated {tok:.0} token(s) in the last {mins} min: waiting for {mins} quiet minute(s) protects whoever is using it. {IDLE_WAYS_OUT}"))
        }
        Some(_) => Ok(()),
    }
}

// ------------------------------------------------------------- what else was running

/// Where the count of OTHER people's requests came from, best evidence first. Only a GATEWAY
/// source can prove a run had the engine to itself: the engine's own total cannot be told apart
/// from the bench's own requests except by subtracting what the bench THINKS it launched, which
/// is an upper bound on its own load and so a LOWER bound on everybody else's - fine for
/// "there was traffic", worthless for "there was none".
pub const LOAD_SRC_USERS: &str = "gateway user table";
pub const LOAD_SRC_LANES: &str = "gateway lanes";
pub const LOAD_SRC_ENGINE: &str = "engine total less the bench's own";
pub const LOAD_SRC_NONE: &str = "not measured";

/// Higher = better evidence. 0 = nothing was measurable; below 2 = cannot prove quiet.
fn source_rank(source: &str) -> u8 {
    match source {
        LOAD_SRC_USERS => 3,
        LOAD_SRC_LANES => 2,
        LOAD_SRC_ENGINE => 1,
        _ => 0,
    }
}

/// Other people's requests as ONE safety poll saw them, and how well that poll could see them.
/// Never negative: a harness that has not yet ramped to its full concurrency would otherwise
/// manufacture "minus two other users".
pub fn foreign_now(o: &SafetyObs, step_max_concurrency: u32) -> Option<(f64, &'static str)> {
    if let Some(n) = o.other_users_inflight {
        return Some((n as f64, LOAD_SRC_USERS));
    }
    let own = bench_inflight(o, step_max_concurrency);
    // card #207: the two lanes are NOT equally innocent - the one that carries the engine's
    // default priority also holds the bench's own priority-less direct requests, and is worth
    // only what is left after the bench's own upper bound comes off it; the other is foreign by
    // construction. Card #212: WHICH lane that is comes from the config (`own_lane`), not from
    // #207's assumption that it is always the trusted one.
    if let (Some(public), Some(trusted)) = (o.gateway_public, o.gateway_trusted) {
        let n = match o.own_lane {
            OwnLane::Trusted => public + (trusted - own).max(0.0),
            OwnLane::Public => (public - own).max(0.0) + trusted,
            OwnLane::Neither => public + trusted,
        };
        return Some((n, LOAD_SRC_LANES));
    }
    o.running.map(|r| ((r - own).max(0.0), LOAD_SRC_ENGINE))
}

/// Accumulates what every safety poll saw, so the scorecard can state the background load ACROSS
/// the run instead of a single reading taken before it started (which says nothing about the
/// four minutes that followed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadSampler {
    samples: u64,
    polls_with_traffic: u64,
    foreign_sum: f64,
    foreign_max: f64,
    foreign_samples: u64,
    queue_sum: f64,
    queue_max: f64,
    /// the WEAKEST source any poll had to fall back to (a gateway that went away mid-run must
    /// downgrade the whole run's claim, not be averaged away)
    weakest: Option<&'static str>,
}

impl LoadSampler {
    pub fn push(&mut self, o: &SafetyObs, step_max_concurrency: u32) {
        self.samples += 1;
        let seen = foreign_now(o, step_max_concurrency);
        let source = seen.map_or(LOAD_SRC_NONE, |(_, s)| s);
        if self.weakest.is_none_or(|w| source_rank(source) < source_rank(w)) {
            self.weakest = Some(source);
        }
        if let Some((n, _)) = seen {
            self.foreign_samples += 1;
            self.foreign_sum += n;
            self.foreign_max = self.foreign_max.max(n);
            self.polls_with_traffic += u64::from(n >= 0.5);
        }
        if let Some(q) = o.queue {
            self.queue_sum += q;
            self.queue_max = self.queue_max.max(q);
        }
    }

    /// The scorecard's block, or None when no poll ever ran (a refusal, a dry run).
    pub fn finish(&self, engine_tok_s: Option<f64>) -> Option<BackgroundLoad> {
        if self.samples == 0 {
            return None;
        }
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let have = self.foreign_samples > 0;
        Some(BackgroundLoad {
            samples: self.samples,
            polls_with_traffic: self.polls_with_traffic,
            concurrent_avg: have.then(|| r2(self.foreign_sum / self.foreign_samples as f64)),
            concurrent_max: have.then_some(r2(self.foreign_max)),
            source: self.weakest.unwrap_or(LOAD_SRC_NONE).to_string(),
            queue_avg: r2(self.queue_sum / self.samples as f64),
            queue_max: r2(self.queue_max),
            engine_tok_s: engine_tok_s.map(|v| (v * 10.0).round() / 10.0),
        })
    }
}

/// What else the server was doing while the benchmark ran, sampled at EVERY safety poll rather
/// than read once at the start. This is the fact a reader needs before any other number on the
/// scorecard means anything: the same loadout measured alone and measured with two other people
/// on it gives two different, both-correct answers.
///
/// KNOWN LIMIT, stated rather than hidden: these are instantaneous gauges read every
/// `[bench] poll_secs` (2 s by default). A request shorter than that interval can begin and end
/// between two polls and be counted by neither - `samples` is published so a reader can see the
/// denominator, and `LOAD_SRC_ENGINE` never counts as proof of quiet for exactly this reason.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundLoad {
    /// safety polls that contributed
    pub samples: u64,
    /// polls that saw at least one request that was not the bench's
    pub polls_with_traffic: u64,
    /// other people's requests in flight: the mean over the run, and the worst poll. `null` =
    /// no poll could tell the bench's own requests from anyone else's.
    pub concurrent_avg: Option<f64>,
    pub concurrent_max: Option<f64>,
    /// how `concurrent_*` was counted, at its WEAKEST over the run (one of the `LOAD_SRC_*`)
    pub source: String,
    /// the ENGINE's queue depth, mean and peak - everything queued, the bench's own included
    pub queue_avg: f64,
    pub queue_max: f64,
    /// the ENGINE's whole output over the bench window, tok/s, the bench's own generation
    /// INCLUDED (its counter does not separate them). It is the denominator that gives a
    /// foreign-request count a size, not a measure of the foreign load on its own.
    pub engine_tok_s: Option<f64>,
}

/// How a scorecard's numbers must be read. Three states, never two: a run whose background load
/// was never recorded is not a quiet run, it is an unknown one, and saying so is the whole point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadClass {
    /// nothing but the benchmark touched the server for the whole run: the gold standard
    Quiet,
    /// other traffic shared the engine: every number here is what the model gave WITH it
    Loaded,
    /// nobody recorded it - a run from before this was measured, or no gateway to separate the
    /// bench's own requests from everyone else's. Never to be read as quiet.
    #[default]
    Unknown,
}

impl LoadClass {
    /// The short marker for a table column.
    pub fn word(self) -> &'static str {
        match self {
            LoadClass::Quiet => "quiet",
            LoadClass::Loaded => "LOADED",
            LoadClass::Unknown => "?",
        }
    }

    pub fn sentence(self) -> &'static str {
        match self {
            LoadClass::Quiet => "measured on a quiet server: nothing else was running",
            LoadClass::Loaded => "MEASURED UNDER LOAD: other traffic shared the server the whole time",
            LoadClass::Unknown => "what else the server was doing was NOT recorded: do not read this as quiet",
        }
    }
}

impl BackgroundLoad {
    pub fn class(&self) -> LoadClass {
        let rank = source_rank(&self.source);
        if self.samples == 0 || rank == 0 {
            return LoadClass::Unknown;
        }
        if rank < 2 {
            // the engine's own total: it can show traffic, it can never prove the absence of it
            return if self.polls_with_traffic > 0 { LoadClass::Loaded } else { LoadClass::Unknown };
        }
        if self.polls_with_traffic == 0 && self.concurrent_max.is_some_and(|m| m < 0.5) {
            LoadClass::Quiet
        } else {
            LoadClass::Loaded
        }
    }

    /// One line for a person: what was on the server, how it was counted, and out of how many looks.
    pub fn sentence(&self) -> String {
        let engine = self.engine_tok_s.map_or_else(String::new, |t| format!(" · the engine wrote {t:.0} tok/s in all (the bench's own output included)"));
        match (self.class(), self.concurrent_avg, self.concurrent_max) {
            (LoadClass::Quiet, _, _) => format!("quiet: no request but the benchmark's own in any of {} looks, 2 s apart ({}){engine}", self.samples, self.source),
            (_, Some(avg), Some(max)) => format!(
                "UNDER LOAD: {avg:.2} other request(s) in flight on average, {max:.0} at the worst, on {} of {} looks ({}) · engine queue {:.1} avg / {:.0} peak{engine}",
                self.polls_with_traffic, self.samples, self.source, self.queue_avg, self.queue_max
            ),
            _ => format!("not recorded: nothing here could tell the benchmark's own requests apart from anyone else's in {} looks ({}){engine}", self.samples, self.source),
        }
    }
}

/// The share of the engine ONE benchmark request had while `foreign` other requests shared it:
/// fair sharing between equal streams, 1/(1+foreign).
fn engine_share(foreign: f64) -> f64 {
    1.0 / (1.0 + foreign.max(0.0))
}

/// Why two scorecards must NOT be compared, or None when they may be.
///
/// WHERE THE RULE COMES FROM - "materially different" has to be derived, not picked. The
/// comparison already declares a run-to-run noise band (`loadout::NOISE`, +-3 %) and refuses to
/// call anything inside it a difference. The honest bar for the LOAD is the same band applied to
/// the cause instead of the effect: two runs are comparable only when the share of the engine one
/// benchmark request had cannot differ by more than that band. Under fair sharing that share is
/// 1/(1+n) for n other requests in flight, so the test is
///     |1/(1+na) - 1/(1+nb)| / max(sa, sb) <= NOISE.
/// At NOISE = 3 % this tolerates a difference of about 0.03 of one continuously-busy foreign
/// request - three percent of one extra user - and nothing beyond it. It therefore makes QUIET vs
/// LOADED incomparable in every case worth the name: nothing against even one steady other user
/// is a 50 % difference in share, not a 3 % one. And a run whose load was never recorded is
/// comparable with nothing at all, because an unmeasured difference is not a small one.
pub fn load_gap(a: &Scorecard, b: &Scorecard) -> Option<String> {
    let unknown = |s: &Scorecard, which: &str| format!("the {which} run did not record what else the server was doing while it ran, so a difference in the numbers cannot be told apart from a difference in the traffic ({})", s.load_class().sentence());
    if a.load_class() == LoadClass::Unknown {
        return Some(unknown(a, "first"));
    }
    if b.load_class() == LoadClass::Unknown {
        return Some(unknown(b, "second"));
    }
    let (Some(ba), Some(bb)) = (a.background.as_ref(), b.background.as_ref()) else {
        return Some(unknown(a, "first"));
    };
    let (Some(na), Some(nb)) = (ba.concurrent_avg, bb.concurrent_avg) else {
        return Some(unknown(a, "first"));
    };
    let (sa, sb) = (engine_share(na), engine_share(nb));
    let gap = (sa - sb).abs() / sa.max(sb);
    (gap > crate::loadout::NOISE + 1e-12).then(|| {
        format!(
            "measured under different background load: {na:.2} other request(s) in flight against {nb:.2}, so one run had about {:.0}% more of the engine to itself than the other - past the +-{:.0}% band a difference has to clear to count at all. The gap in the numbers is at least as likely to be the traffic as the loadout",
            gap * 100.0,
            crate::loadout::NOISE * 100.0
        )
    })
}

// ------------------------------------------------------------------ garbled output

/// English-only writing tasks: any CJK in the answers is degeneration, not content.
pub const GARBLE_PROMPTS: [&str; 6] = [
    "Write a detailed, practical guide to keeping a sourdough starter alive. Plain English prose only, about 900 words.",
    "Explain how a four-stroke petrol engine works to a curious teenager. Plain English prose only, about 900 words.",
    "Write a short story about a lighthouse keeper who finds a message in a bottle. Plain English prose only, about 900 words.",
    "Write a Python module with a documented class that implements an LRU cache, then explain every method in English.",
    "Describe the water cycle, then the carbon cycle, then compare them. Plain English prose only, about 900 words.",
    "Write a product manual for an imaginary programmable coffee machine, with numbered steps. Plain English only.",
];

pub fn garble_requests(model: &str, prompts: u32, max_tokens: u32) -> Vec<Value> {
    GARBLE_PROMPTS.iter().take(prompts as usize).map(|p| serde_json::json!({"model": model, "messages": [{"role": "user", "content": p}], "temperature": 0.7, "max_tokens": max_tokens, "stream": false, "user": crate::users::BENCH_USER})).collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Garbled {
    /// characters of model output scanned
    pub chars: u64,
    /// U+FFFD: bytes that were not valid text (a broken detokeniser, a cut multi-byte character)
    pub replacement_chars: u64,
    /// CJK / kana / hangul characters in answers to English-only tasks
    pub cjk_chars: u64,
    /// (replacement + CJK) per 100 000 characters; 0 is what a healthy model gives
    pub per_100k: f64,
}

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF66..=0xFF9F | 0x20000..=0x2FA1F)
}

/// Scan what the model wrote. Nothing scanned = None: no output is not "0 garbled".
pub fn scan_garbled<S: AsRef<str>>(texts: &[S]) -> Option<Garbled> {
    let mut g = Garbled::default();
    for t in texts {
        for c in t.as_ref().chars() {
            g.chars += 1;
            g.replacement_chars += u64::from(c == '\u{FFFD}');
            g.cjk_chars += u64::from(is_cjk(c));
        }
    }
    if g.chars == 0 {
        return None;
    }
    g.per_100k = (((g.replacement_chars + g.cjk_chars) as f64 / g.chars as f64 * 100_000.0) * 10.0).round() / 10.0;
    Some(g)
}

// ------------------------------------------------------------------ scorecard

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecodeCell {
    /// users at once
    pub concurrency: u32,
    /// tokens already in the conversation
    pub context: u64,
    /// the speed each user gets, tok/s
    pub per_user_tok_s: f64,
    pub total_tok_s: f64,
    /// time to the first word
    pub ttft_ms: Option<f64>,
    pub errors: u64,
    /// the harness marked the cell as limited by KV capacity, not by speed
    pub capacity_limited: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefillCell {
    /// prompt size in tokens
    pub tokens: u64,
    pub tok_s: f64,
    pub ttft_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedleResult {
    /// how far into the prompt the needle sat, percent
    pub depth_pct: u32,
    /// what the engine counted the prompt as (0 = it never answered)
    pub prompt_tokens: u64,
    pub pass: bool,
    pub detail: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Accuracy {
    pub dataset: String,
    /// 0..=1
    pub score: f64,
    pub n: u64,
    pub correct: u64,
    pub wilson95_low: Option<f64>,
    pub wilson95_high: Option<f64>,
    /// the raw harness file: what `lss compare A B --accuracy` hands to the harness
    pub file: String,
}

/// One bench run, normalised. `null` / empty = that part did not run; never a made-up zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scorecard {
    pub run_id: i64,
    pub loadout_id: String,
    pub model: String,
    pub profile: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_s: i64,
    /// card #334: the same, in milliseconds (`duration_s` alone printed a 0.4 s run as "took 0s").
    /// `None` = a scorecard written before this field existed.
    pub duration_ms: Option<i64>,
    /// `ok` | `aborted` | `failed` | `timeout` - whether the MEASUREMENT completed. Whether its
    /// checks passed is `verdict()`: a run can complete and still fail a sanity check.
    pub status: String,
    /// why it stopped early (`aborted: real traffic …`, the harness's error, `timeout after …`)
    pub aborted: Option<String>,
    /// the idle gate was skipped (`--force` or `--under-load`)
    pub forced: bool,
    /// the run deliberately measured with other traffic on the box: the abort-on-traffic watch
    /// was stood down for it (card #14). What the traffic WAS is `background`, not this flag -
    /// an under-load run on a box that happened to stay quiet is still quiet, and says so.
    pub under_load: bool,
    /// what else the server was doing, sampled across the whole run. `null` = not recorded
    /// (a run from before card #14, or nothing that could separate the bench from everyone
    /// else) - which is NOT the same as quiet, and `LoadClass` keeps the two apart.
    pub background: Option<BackgroundLoad>,
    pub note: String,
    pub harness_version: String,
    pub harness_commit: String,
    /// where the harness was pointed and why
    pub target: String,
    pub decode: Vec<DecodeCell>,
    pub prefill: Vec<PrefillCell>,
    pub sanity: Vec<Check>,
    pub needle: Vec<NeedleResult>,
    /// speculative decoding, from the SERVER's counters across the bench window
    pub accept_length: Option<f64>,
    pub accept_rate: Option<f64>,
    /// KV capacity in tokens (`max_total_num_tokens`)
    pub kv_tokens: Option<f64>,
    pub tokens_per_joule: Option<f64>,
    pub wh_per_mtok: Option<f64>,
    pub avg_watts: Option<f64>,
    /// GARBLED OUTPUT (full profile): U+FFFD and unasked-for CJK per 100k characters written
    pub garbled: Option<Garbled>,
    pub accuracy: Vec<Accuracy>,
    /// the directory holding the raw harness JSON of this run
    pub raw_dir: String,
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The harness's own version, from any of its result files.
pub fn harness_version(doc: &Value) -> Option<String> {
    doc["metadata"]["version"].as_str().or_else(|| doc["startup_diagnostics"]["version"].as_str()).map(str::to_string)
}

/// The decode cells of a `benchmark_results.json` (`results[]`).
pub fn decode_cells(doc: &Value) -> Vec<DecodeCell> {
    let mut cells: Vec<DecodeCell> = doc["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let concurrency = r["concurrency"].as_u64()? as u32;
            let total = num(&r["aggregate_tps"])?;
            if concurrency == 0 || total <= 0.0 {
                return None;
            }
            let per_user = num(&r["per_request_avg_tps"]).filter(|v| *v > 0.0).unwrap_or(total / f64::from(concurrency));
            Some(DecodeCell {
                concurrency,
                context: r["context_tokens"].as_u64().unwrap_or(0),
                per_user_tok_s: round1(per_user),
                total_tok_s: round1(total),
                ttft_ms: num(&r["ttft_p50"]).or_else(|| num(&r["ttft_avg"])).filter(|v| *v > 0.0).map(|s| round1(s * 1000.0)),
                errors: r["num_errors"].as_u64().unwrap_or(0),
                capacity_limited: r["capacity_limited"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    cells.sort_by_key(|c| (c.context, c.concurrency));
    cells
}

/// The prefill cells (`prefill{"8192": {…}}`), smallest first.
pub fn prefill_cells(doc: &Value) -> Vec<PrefillCell> {
    let mut cells: Vec<PrefillCell> = doc["prefill"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(size, p)| {
            let tok_s = num(&p["tok_per_sec"]).filter(|v| *v > 0.0)?;
            let tokens = p["prompt_tokens"].as_u64().filter(|t| *t > 0).or_else(|| size.parse().ok())?;
            Some(PrefillCell { tokens, tok_s: round1(tok_s), ttft_ms: round1(num(&p["ttft_seconds"]).unwrap_or(0.0) * 1000.0) })
        })
        .collect();
    cells.sort_by_key(|c| c.tokens);
    cells
}

/// The `accuracy` block of a dataset-profile result.
pub fn accuracy_of(doc: &Value, dataset: &str, file: &str) -> Option<Accuracy> {
    let a = &doc["accuracy"];
    let n = a["scored"].as_u64().filter(|n| *n > 0)?;
    Some(Accuracy { dataset: dataset.to_string(), score: (num(&a["accuracy"])? * 10_000.0).round() / 10_000.0, n, correct: a["correct"].as_u64().unwrap_or(0), wilson95_low: num(&a["wilson95_low"]), wilson95_high: num(&a["wilson95_high"]), file: file.to_string() })
}

/// The numbers a table row shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Headline {
    /// one user alone, context 0: tok/s
    pub c1_tok_s: Option<f64>,
    pub ttft_c1_ms: Option<f64>,
    /// the best total tok/s (context 0) and the concurrency it was reached at
    pub max_total_tok_s: Option<f64>,
    pub max_total_at: Option<u32>,
    pub prefill_8k_tok_s: Option<f64>,
    pub accuracy: Option<f64>,
    pub accuracy_dataset: Option<String>,
}

impl Scorecard {
    pub fn cell(&self, concurrency: u32, context: u64) -> Option<&DecodeCell> {
        self.decode.iter().find(|c| c.concurrency == concurrency && c.context == context)
    }

    /// The prefill cell nearest `tokens` (the harness lands a few tokens off the round number).
    pub fn prefill_near(&self, tokens: u64) -> Option<&PrefillCell> {
        self.prefill.iter().find(|p| p.tokens.abs_diff(tokens) * 20 <= tokens)
    }

    pub fn headline(&self) -> Headline {
        let best = self.decode.iter().filter(|c| c.context == 0).max_by(|a, b| a.total_tok_s.total_cmp(&b.total_tok_s));
        let acc = self.accuracy.first();
        Headline {
            c1_tok_s: self.cell(1, 0).map(|c| c.per_user_tok_s),
            ttft_c1_ms: self.cell(1, 0).and_then(|c| c.ttft_ms),
            max_total_tok_s: best.map(|c| c.total_tok_s),
            max_total_at: best.map(|c| c.concurrency),
            prefill_8k_tok_s: self.prefill_near(8192).map(|p| p.tok_s),
            accuracy: acc.map(|a| a.score),
            accuracy_dataset: acc.map(|a| a.dataset.clone()),
        }
    }

    /// Context-0 decode cells as rows of the concurrency table (`source: bench`).
    pub fn curve_rows(&self) -> Vec<crate::loadout::CurveRow> {
        self.decode
            .iter()
            .filter(|c| c.context == 0)
            .map(|c| crate::loadout::CurveRow { running: u64::from(c.concurrency), samples: 1, tok_s: Some(c.total_tok_s), per_request_tok_s: Some(c.per_user_tok_s), ttft_ms: c.ttft_ms, spec_accept_length: None, source: "bench".into() })
            .collect()
    }

    pub fn complete(&self) -> bool {
        self.status == "ok"
    }

    /// card #334: the checks this run FAILED - sanity checks and needle depths - in words.
    pub fn failed_checks(&self) -> Vec<String> {
        let mut out: Vec<String> = self.sanity.iter().filter(|c| !c.pass).map(|c| c.name.clone()).collect();
        out.extend(self.needle.iter().filter(|n| !n.pass).map(|n| format!("needle at {}%", n.depth_pct)));
        out
    }

    /// card #334: the headline verdict. `OK` only when the run completed AND every check passed;
    /// a completed run with a failed check says so on the same line instead of a bare OK.
    pub fn verdict(&self) -> String {
        run_verdict(&self.status, &self.failed_checks(), true)
    }

    /// card #334: how long it took, to a tenth of a second under 10 s ("took 0s" read as broken).
    pub fn took(&self) -> String {
        match self.duration_ms {
            Some(ms) if ms < 10_000 => format!("{:.1}s", ms as f64 / 1000.0),
            _ => crate::timeutil::fmt_duration(self.duration_s),
        }
    }

    /// How these numbers must be read: on a quiet server, under load, or unrecorded. A scorecard
    /// with no `background` block answers `Unknown`, never `Quiet` - the one mistake this whole
    /// mechanism exists to prevent is a loaded measurement being read as a clean one.
    pub fn load_class(&self) -> LoadClass {
        self.background.as_ref().map_or(LoadClass::Unknown, BackgroundLoad::class)
    }

    /// The one line every screen that shows this scorecard prints next to it.
    pub fn load_sentence(&self) -> String {
        self.background.as_ref().map_or_else(|| LoadClass::Unknown.sentence().to_string(), BackgroundLoad::sentence)
    }
}

/// card #334/#338: one wording for "the run completed but a check failed", shared by the CLI
/// headline (`OK, BUT 1 CHECK FAILED: json output`) and `/status` bench.last / the UI's BENCH box
/// (`ok, but 1 check failed: json output`). A run that did not complete answers its status.
fn run_verdict(status: &str, failed: &[String], upper: bool) -> String {
    if status != "ok" || failed.is_empty() {
        return if upper { status.to_uppercase() } else { status.to_string() };
    }
    let n = failed.len();
    let s = if n == 1 { "" } else { "s" };
    let head = format!("ok, but {n} check{s} failed");
    format!("{}: {}", if upper { head.to_uppercase() } else { head }, failed.join(", "))
}

/// The live concurrency table with the bench's rows laid over it: where the bench measured a
/// level, its row wins (and says `bench`); the other levels stay `live`.
pub fn merge_curve(live: &[crate::loadout::CurveRow], bench: Option<&Scorecard>) -> Vec<crate::loadout::CurveRow> {
    let over = bench.map(Scorecard::curve_rows).unwrap_or_default();
    let mut out: Vec<crate::loadout::CurveRow> = live
        .iter()
        .map(|l| match over.iter().find(|b| b.running == l.running) {
            Some(b) => crate::loadout::CurveRow { samples: l.samples, spec_accept_length: l.spec_accept_length, ..b.clone() },
            None => l.clone(),
        })
        .collect();
    for b in over {
        if !out.iter().any(|r| r.running == b.running) {
            out.push(b);
        }
    }
    out.sort_by_key(|r| r.running);
    out
}

// ------------------------------------------------------------------ sanity checks + needle

pub const SANITY_ARITHMETIC: &str = "arithmetic";
pub const SANITY_TOOL: &str = "tool call";
pub const SANITY_JSON: &str = "json output";

/// The three fixed request bodies (OpenAI chat format), in order.
pub fn sanity_requests(model: &str) -> Vec<(&'static str, Value)> {
    let chat = |messages: Value, extra: Value| {
        let mut body = serde_json::json!({"model": model, "messages": messages, "temperature": 0, "max_tokens": 2048, "stream": false, "user": crate::users::BENCH_USER});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        body
    };
    vec![
        (SANITY_ARITHMETIC, chat(serde_json::json!([{"role": "user", "content": "What is 17 * 23? Reply with the number only."}]), serde_json::json!({}))),
        (
            SANITY_TOOL,
            chat(
                serde_json::json!([{"role": "user", "content": "What is the weather in Paris right now? Use the tool."}]),
                serde_json::json!({
                    "tools": [{"type": "function", "function": {"name": "get_weather", "description": "Current weather for a city", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}],
                    "tool_choice": {"type": "function", "function": {"name": "get_weather"}}
                }),
            ),
        ),
        (SANITY_JSON, chat(serde_json::json!([{"role": "user", "content": "Return a JSON object with exactly two keys: \"colour\" (a string) and \"count\" (the number 3). JSON only."}]), serde_json::json!({"response_format": {"type": "json_object"}}))),
    ]
}

/// The answer text of a chat completion, with any `<think>…</think>` block removed.
pub fn answer_text(response: &Value) -> String {
    let raw = response["choices"][0]["message"]["content"].as_str().unwrap_or("");
    match raw.rfind("</think>") {
        Some(i) => raw[i + "</think>".len()..].trim().to_string(),
        None => raw.trim().to_string(),
    }
}

fn excerpt(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = one_line.chars().take(80).collect();
    if cut.len() < one_line.len() { format!("{cut}...") } else { cut }
}

/// Pass / fail of one sanity check from the gateway's answer.
pub fn grade_sanity(name: &str, http_status: u16, response: &Value) -> Check {
    let fail = |detail: String| Check { name: name.to_string(), pass: false, detail };
    if !(200..300).contains(&http_status) {
        return fail(format!("HTTP {http_status}: {}", excerpt(&response.to_string())));
    }
    let text = answer_text(response);
    match name {
        SANITY_ARITHMETIC => {
            let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits == "391" { Check { name: name.into(), pass: true, detail: "17 * 23 = 391".into() } } else { fail(format!("expected 391, got: {}", excerpt(&text))) }
        }
        SANITY_TOOL => {
            let call = &response["choices"][0]["message"]["tool_calls"][0]["function"];
            let args: Option<Value> = call["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok()).or_else(|| call["arguments"].as_object().map(|o| Value::Object(o.clone())));
            match (call["name"].as_str(), args) {
                (Some("get_weather"), Some(a)) if a["city"].as_str().is_some_and(|c| !c.is_empty()) => Check { name: name.into(), pass: true, detail: format!("get_weather(city={})", a["city"].as_str().unwrap_or("")) },
                (Some(other), _) => fail(format!("called {other}, or its arguments are not JSON with a city")),
                (None, _) => fail(format!("no tool call in the answer: {}", excerpt(&text))),
            }
        }
        _ => {
            let body = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
            match serde_json::from_str::<Value>(body) {
                Ok(Value::Object(o)) if !o.is_empty() => Check { name: name.into(), pass: true, detail: format!("parses: {}", excerpt(body)) },
                Ok(_) => fail(format!("parses, but is not a JSON object: {}", excerpt(body))),
                Err(e) => fail(format!("does not parse as JSON ({e}): {}", excerpt(body))),
            }
        }
    }
}

/// The secret the needle test hides. Fixed, so a run is repeatable.
pub const NEEDLE_CODE: &str = "7421-ORCHID-88";

/// A prompt of roughly `tokens` tokens with one sentence that matters at `depth_pct` percent.
/// The filler is plain numbered English (about 4.6 characters per token on the tokenizers we have
/// met; the engine's own `prompt_tokens` is what the scorecard reports).
pub fn needle_prompt(tokens: u64, depth_pct: u32) -> String {
    const FILLER: [&str; 6] = [
        "The harbour ledger records that the tide table was copied by hand each morning before the boats left.",
        "A baker on the corner kept a list of every loaf sold, and the list was filed by week in a tin box.",
        "The railway clerk noted the arrival of the evening train and wrote the delay in minutes beside it.",
        "In the orchard the pickers counted crates at noon and again at dusk, and the two counts rarely agreed.",
        "The museum catalogue gives the height of each vase in centimetres and the year it entered the collection.",
        "Every lighthouse keeper on the coast sent a monthly report about oil, glass, weather and visiting ships.",
    ];
    let needle = format!("IMPORTANT: the secret access code for the archive room is {NEEDLE_CODE}. Remember it.");
    let target_chars = (tokens as f64 * 4.6) as usize;
    let needle_at = target_chars * depth_pct.min(100) as usize / 100;
    let mut out = String::with_capacity(target_chars + 512);
    out.push_str("Below is a long archive of notes. One line in it states a secret access code. Read everything, then answer the question at the end.\n\n");
    let mut placed = false;
    let mut i = 0usize;
    while out.len() < target_chars {
        if !placed && out.len() >= needle_at {
            out.push_str(&needle);
            out.push('\n');
            placed = true;
        }
        out.push_str(&format!("Note {}: {}\n", i + 1, FILLER[i % FILLER.len()]));
        i += 1;
    }
    if !placed {
        out.push_str(&needle);
        out.push('\n');
    }
    out.push_str("\nQuestion: what is the secret access code for the archive room? Answer with the code only.");
    out
}

pub fn grade_needle(depth_pct: u32, http_status: u16, response: &Value, secs: f64) -> NeedleResult {
    let prompt_tokens = response["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let text = answer_text(response);
    let (pass, detail) = if !(200..300).contains(&http_status) {
        (false, format!("HTTP {http_status}: {}", excerpt(&response.to_string())))
    } else if text.contains(NEEDLE_CODE) {
        (true, format!("found the code at {depth_pct}% depth"))
    } else {
        (false, format!("did not return the code: {}", excerpt(&text)))
    };
    NeedleResult { depth_pct, prompt_tokens, pass, detail, secs: round1(secs) }
}

/// Speculative-decoding accept length over a window, from the SERVER's counters: generated
/// tokens per verify pass. None when speculative decoding is off (no verify passes).
pub fn accept_length(gen_tokens_delta: f64, verify_calls_delta: f64) -> Option<f64> {
    (verify_calls_delta > 0.0 && gen_tokens_delta > 0.0).then(|| (gen_tokens_delta / verify_calls_delta * 100.0).round() / 100.0).filter(|v| *v >= 1.0)
}

// ------------------------------------------------------------------ documents

/// `status.bench`: is one running, and what was the last one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchBrief {
    /// `idle` | `running`
    pub state: String,
    pub profile: Option<String>,
    pub started_at: Option<i64>,
    /// the step in progress (`decode matrix`, `sanity checks` …) and its position
    pub step: Option<String>,
    pub step_index: u32,
    pub steps: u32,
    /// the newest finished run
    pub last: Option<LastRun>,
    /// false = `lss bench` cannot run here (an older collector without a harness)
    pub configured: bool,
    /// no external harness is configured: `quick` and `full` run lss's built-in mini bench
    /// (added 2026-09-20)
    pub builtin: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastRun {
    pub run_id: i64,
    pub profile: String,
    pub ended_at: i64,
    pub status: String,
    pub aborted: Option<String>,
    pub model: String,
    pub headline: Headline,
    /// `quiet` | `loaded` | `unknown`: the headline numbers travel with the condition they were
    /// measured under, so a client that only reads `status.bench.last` cannot mistake one for
    /// the other either.
    pub load: LoadClass,
    /// card #338: the checks this run FAILED (sanity checks, `needle at N%`), in words; empty when
    /// all passed or none ran. `status` stays `ok` for a completed run, so a reader that only
    /// looked at it saw `ok` over a failed check - `verdict()` puts the two together.
    pub failed_checks: Vec<String>,
}

impl LastRun {
    pub fn of(s: &Scorecard) -> LastRun {
        LastRun { run_id: s.run_id, profile: s.profile.clone(), ended_at: s.ended_at, status: s.status.clone(), aborted: s.aborted.clone(), model: s.model.clone(), headline: s.headline(), load: s.load_class(), failed_checks: s.failed_checks() }
    }

    /// card #338: `ok`, `ok, but 1 check failed: json output`, or the status of a run that did
    /// not complete - the lower-case twin of the CLI's `Scorecard::verdict()`.
    pub fn verdict(&self) -> String {
        run_verdict(&self.status, &self.failed_checks, false)
    }

    /// `ok` AND every check passed: the only reading drawn in the calm colour.
    pub fn clean(&self) -> bool {
        self.status == "ok" && self.failed_checks.is_empty()
    }
}

/// `GET /bench`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchDoc {
    pub v: u32,
    pub generated_at: i64,
    pub brief: BenchBrief,
    /// newest first
    pub runs: Vec<Scorecard>,
}

/// card #329: what `lss bench <profile>` (following a run it just started) should do with one
/// `/bench` document. The collector re-renders that document on its poll tick, not on request, so
/// right after a start - and right after the run ends - a document can be OLDER than what happened:
/// it says `idle` and does not list the run yet. That is "not yet", never "done": the old follow
/// loop returned exit 0 there, with the run still going and no scorecard shown (measured twice).
#[derive(Debug, PartialEq)]
pub enum Follow<'a> {
    /// the run is going: show this step line
    Running(String),
    /// the run is listed: show its scorecard; exit 0 only if it is complete
    Finished(&'a Scorecard),
    /// not running and not listed, in a document not rendered since we began waiting: ask again
    Waiting,
    /// a document rendered after we began waiting still does not list it (or `FOLLOW_GRACE_SECS`
    /// passed): say so and exit nonzero
    Lost,
}

/// How long `Waiting` may last at most, whatever the documents say (a poll tick is seconds).
pub const FOLLOW_GRACE_SECS: i64 = 120;

/// `waiting_since` = when (on the collector's clock, `generated_at`) the follower first saw the
/// run neither running nor listed; `now` = the follower's own clock.
pub fn follow_step<'a>(doc: &'a BenchDoc, run_id: Option<i64>, waiting_since: Option<i64>, waited_secs: i64) -> Follow<'a> {
    if doc.brief.state == "running" {
        return Follow::Running(format!("step {}/{}: {}", doc.brief.step_index, doc.brief.steps, doc.brief.step.clone().unwrap_or_default()));
    }
    if let Some(run) = doc.runs.iter().find(|r| Some(r.run_id) == run_id) {
        return Follow::Finished(run);
    }
    let refreshed_since = waiting_since.is_some_and(|since| doc.generated_at > since);
    if refreshed_since || waited_secs >= FOLLOW_GRACE_SECS {
        Follow::Lost
    } else {
        Follow::Waiting
    }
}

/// The answer to `POST /bench`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchStarted {
    pub v: u32,
    pub ok: bool,
    pub run_id: Option<i64>,
    /// what to tell the person: started, or why not
    pub message: String,
    /// card #14, 2026-09-23: what the COLLECTOR understood `under_load` to be - echoed back on
    /// the acceptance AND on the refusal. `BenchRequest` is `#[serde(default)]` with no
    /// `deny_unknown_fields`, so a collector older than the flag drops it on the floor and
    /// answers exactly as if it had never been asked: the caller then gets "pass --force",
    /// passes it, and the run dies at the first foreign request - which is the failure the flag
    /// exists to remove. An old collector omits this field, it deserialises `false`, and `lss`
    /// can say so instead of leaving the operator to infer a version skew from a dead run.
    pub under_load: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../../../fixtures/benchmark_results_sample.json");

    fn cfg() -> BenchConfig {
        BenchConfig { harness: "/opt/bench/llm_decode_bench.py".into(), ..Default::default() }
    }

    #[test]
    fn the_engine_key_goes_to_engine_steps_on_stdin_never_on_the_command_line() {
        // card #312 (verifier FAIL on the argv version)
        let cfg = BenchConfig { python: "python3".into(), launcher: vec!["ssh".into(), "gpu-box".into()], ..Default::default() };
        let args: Vec<String> = ["--host", "127.0.0.1", "--port", "8000"].map(String::from).to_vec();
        let key = "test-bench-value-4242";
        let keyed = harness_launch(&cfg, "/h/bench.py", &args, Role::Decode, key);
        assert!(!keyed.argv.iter().any(|a| a.contains(key)), "the key is never on the command line: {:?}", keyed.argv);
        assert_eq!(keyed.stdin.as_deref(), Some("test-bench-value-4242\n"));
        assert_eq!(&keyed.argv[..6], ["ssh".to_string(), "gpu-box".into(), "python3".into(), "-c".into(), shell_quote(KEY_SHIM), "/h/bench.py".into()], "launcher first, then the shim (quoted for ssh's remote shell) runs the harness");
        assert_eq!(&keyed.argv[6..], &args[..]);
        let plain = HarnessLaunch { argv: command_line(&cfg, "/h/bench.py", &args), stdin: None };
        assert_eq!(harness_launch(&cfg, "/h/bench.py", &args, Role::DryRun, key), plain, "the --help dry run talks to nothing: no key");
        assert_eq!(harness_launch(&cfg, "/h/bench.py", &args, Role::Prefill, "  "), plain, "no key: the plain command, no stdin");
        // anything printed still goes through redact_argv
        let shown = redact_argv(&[format!("--api-key={key}"), "--api-key".into(), key.into()]).join(" ");
        assert!(!shown.contains(key), "{shown}");
    }

    #[test]
    fn a_preflight_refusal_names_the_key_setting_and_nothing_else_is_one() {
        let none = preflight_refusal("/v1/models", 401, false).unwrap();
        assert!(none.contains("HTTP 401 on /v1/models") && none.contains("none is configured") && none.contains("api_key") && none.contains("[[engine]]"), "{none}");
        let wrong = preflight_refusal("/v1/chat/completions", 403, true).unwrap();
        assert!(wrong.contains("HTTP 403 on /v1/chat/completions") && wrong.contains("refused the configured key") && wrong.contains("api_key"), "{wrong}");
        for code in [200, 404, 429, 500, 502] {
            assert_eq!(preflight_refusal("/v1/models", code, true), None, "{code} is not a key problem");
        }
    }

    #[test]
    fn over_ssh_every_word_reaches_the_remote_program_as_it_was() {
        // card #312 FAIL 2: ssh joins argv with spaces and the remote shell splits it again
        let ssh = BenchConfig { python: "python3".into(), launcher: vec!["ssh".into(), "-o".into(), "BatchMode=yes".into(), "gpu-box".into()], ..Default::default() };
        let plain_args: Vec<String> = ["--host", "127.0.0.1", "--port", "8000", "--model", "org/model-7b", "--output", "decode.json"].map(String::from).to_vec();
        // a plain step is byte-for-byte what it was before (no quoting of safe words)
        let mut before: Vec<String> = ssh.launcher.clone();
        before.extend(["python3".to_string(), "/h/bench.py".to_string()]);
        before.extend(plain_args.iter().cloned());
        assert_eq!(command_line(&ssh, "/h/bench.py", &plain_args), before);
        // systemd-run passes argv through: never quoted, even the shim
        let scope = BenchConfig { python: "python3".into(), launcher: vec!["/usr/bin/systemd-run".into(), "--user".into(), "--scope".into()], ..Default::default() };
        assert!(harness_launch(&scope, "/h/bench.py", &plain_args, Role::Decode, "k").argv.contains(&KEY_SHIM.to_string()));
        // what the remote shell does with the joined line: `sh -c "<words joined by spaces>"`
        if std::process::Command::new("python3").arg("-c").arg("1").output().is_ok_and(|o| o.status.success()) {
            let dir = std::env::temp_dir().join(format!("lss-ssh-join-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let probe = dir.join("probe dir").join("bench.py"); // a path with a space, too
            std::fs::create_dir_all(probe.parent().unwrap()).unwrap();
            std::fs::write(&probe, "import sys, json\nprint(json.dumps(sys.argv[1:]))\n").unwrap();
            let odd: Vec<String> = ["--prompt", "it's (a) test; $HOME `x`"].map(String::from).to_vec();
            let launch = harness_launch(&ssh, &probe.display().to_string(), &odd, Role::Decode, "test-ssh-value-77");
            let remote_line = launch.argv[ssh.launcher.len()..].join(" ");
            use std::io::Write;
            let mut child = std::process::Command::new("sh").arg("-c").arg(&remote_line).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(launch.stdin.as_deref().unwrap().as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            let got: Vec<String> = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)));
            assert_eq!(got, ["--prompt", "it's (a) test; $HOME `x`", "--api-key", "test-ssh-value-77"], "the remote program got every word intact, and the key from stdin");
        } else {
            eprintln!("SKIPPED the shell round-trip: no python3 here");
        }
        assert_eq!(shell_quote("plain-word_1.2:3"), "plain-word_1.2:3");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_refusal_in_the_harness_log_is_found_even_when_it_exits_0() {
        let real = "Checking server...\nWARNING: Only OpenAI /v1 endpoints appear reachable (HTTP 401 on /metrics)\nDone.\n";
        assert_eq!(harness_refused(real).as_deref(), Some("WARNING: Only OpenAI /v1 endpoints appear reachable (HTTP 401 on /metrics)"));
        assert_eq!(harness_refused("decode 1: 86.8 tok/s\nDone.\n"), None);
    }

    #[test]
    fn a_completed_run_with_a_failed_check_is_not_a_bare_ok_and_short_runs_keep_their_tenths() {
        // card #334 (rv-331 on #329): 'BENCH RUN 1 quick ... OK' above 'SANITY json output FAIL',
        // and 'took 0s' for a run that took 0.4 s
        let pass = |n: &str| Check { name: n.into(), pass: true, detail: String::new() };
        let mut s = Scorecard { status: "ok".into(), duration_s: 0, duration_ms: Some(380), sanity: vec![pass("arithmetic"), pass("tool call"), Check { name: "json output".into(), pass: false, detail: "not JSON".into() }], ..Default::default() };
        assert_eq!(s.verdict(), "OK, BUT 1 CHECK FAILED: json output");
        assert_eq!(s.took(), "0.4s");
        s.needle = vec![NeedleResult { depth_pct: 50, pass: false, ..Default::default() }];
        assert_eq!(s.verdict(), "OK, BUT 2 CHECKS FAILED: json output, needle at 50%");
        s.sanity[2].pass = true;
        s.needle.clear();
        assert_eq!(s.verdict(), "OK", "every check passed: a plain OK");
        s.status = "aborted".into();
        assert_eq!(s.verdict(), "ABORTED", "a run that did not complete says so, checks or not");
        // long runs keep the usual format; a scorecard from before duration_ms still prints
        let long = Scorecard { duration_s: 312, duration_ms: Some(312_400), ..Default::default() };
        assert_eq!(long.took(), crate::timeutil::fmt_duration(312));
        let old = Scorecard { duration_s: 4, duration_ms: None, ..Default::default() };
        assert_eq!(old.took(), crate::timeutil::fmt_duration(4));
    }

    #[test]
    fn status_bench_last_carries_the_failed_checks_and_says_them_like_the_cli() {
        // card #338: /status bench.last said a bare `ok` for the run whose CLI headline said
        // `OK, BUT 1 CHECK FAILED: json output`
        let pass = |n: &str| Check { name: n.into(), pass: true, detail: String::new() };
        let mut s = Scorecard { status: "ok".into(), sanity: vec![pass("arithmetic"), Check { name: "json output".into(), pass: false, detail: "not JSON".into() }], ..Default::default() };
        let l = LastRun::of(&s);
        assert_eq!(l.failed_checks, ["json output"]);
        assert_eq!(l.verdict(), "ok, but 1 check failed: json output");
        assert!(!l.clean());
        assert_eq!(s.verdict(), l.verdict().to_uppercase().replace("JSON OUTPUT", "json output"), "one wording on both surfaces");
        s.needle = vec![NeedleResult { depth_pct: 50, pass: false, ..Default::default() }];
        assert_eq!(LastRun::of(&s).verdict(), "ok, but 2 checks failed: json output, needle at 50%");
        s.sanity[1].pass = true;
        s.needle.clear();
        let l = LastRun::of(&s);
        assert_eq!((l.verdict().as_str(), l.clean()), ("ok", true));
        s.status = "aborted".into();
        s.sanity[1].pass = false;
        assert_eq!(LastRun::of(&s).verdict(), "aborted", "a run that did not complete says its status");
        // an older collector's bench.last has no failed_checks: it reads as before
        let old: LastRun = serde_json::from_str(r#"{"run_id":1,"status":"ok"}"#).unwrap();
        assert_eq!((old.verdict().as_str(), old.clean()), ("ok", true));
    }

    #[test]
    fn the_quick_plan_wraps_the_harness_and_never_reimplements_it() {
        let steps = plan(Profile::Quick, &cfg(), "model-a", 8, "http://127.0.0.1:8090", "");
        let names: Vec<String> = steps.iter().map(Step::name).collect();
        assert_eq!(names, ["warm-up (decode)", "warm-up (prefill)", "decode matrix", "prefill 8k, 64k", "sanity checks"]);
        let Step::Harness { args, output, max_concurrency, .. } = &steps[2] else { panic!() };
        assert_eq!(args.join(" "), "--host 127.0.0.1 --port 8090 --model model-a --display-mode plain --concurrency 1,2,4,8 --contexts 0,16384 --duration 10 --skip-prefill --output decode.json");
        assert_eq!((output.as_str(), *max_concurrency), ("decode.json", 8));
        let Step::Harness { args, .. } = &steps[3] else { panic!() };
        assert!(args.join(" ").contains("--prefill-only --prefill-contexts 8k,64k"));
        // the warm-up comes first, so the first prefill of a fresh container is never the one measured
        assert!(matches!(&steps[0], Step::Harness { role: Role::Warmup, .. }) && matches!(&steps[1], Step::Harness { role: Role::Warmup, .. }));
        // the matrix is capped at the engine's slots
        let Step::Harness { args, max_concurrency, .. } = &plan(Profile::Quick, &cfg(), "m", 3, "http://127.0.0.1:8090", "")[2] else { panic!() };
        assert!(args.join(" ").contains("--concurrency 1,2,3 ") && *max_concurrency == 3);
        assert_eq!((concurrency_levels(0), concurrency_levels(4), concurrency_levels(6), concurrency_levels(32)), (vec![1, 2, 4, 8], vec![1, 2, 4], vec![1, 2, 4, 6], vec![1, 2, 4, 8]));
    }

    #[test]
    fn full_adds_128k_and_the_needle_and_accuracy_runs_a_pinned_dataset() {
        let steps = plan(Profile::Full, &cfg(), "m", 8, "http://127.0.0.1:8090", "");
        assert!(steps.iter().any(|s| matches!(s, Step::Harness { args, .. } if args.join(" ").contains("--prefill-contexts 8k,64k,128k"))));
        assert!(steps.iter().any(|s| matches!(s, Step::Harness { args, .. } if args.join(" ").contains("--duration 30"))));
        assert_eq!(steps.last(), Some(&Step::Needle { tokens: 250_000, depths: vec![10, 50, 90] }));
        assert!(steps.iter().any(|s| matches!(s, Step::Harness { args, output, max_concurrency, .. } if args.join(" ").contains("--concurrency 1 --contexts 65536,131072") && output == LONG_DECODE_FILE && *max_concurrency == 1)), "decode with 64k and 128k already in the conversation");
        assert!(steps.contains(&Step::Garble { prompts: 6, max_tokens: 1_500 }));
        // no harness: the built-in plan is a warm-up, the concurrency sweep capped at the slots,
        // two prompt sizes and the sanity checks
        let mini = plan_builtin(Profile::Quick, 4);
        let names: Vec<String> = mini.iter().map(Step::name).collect();
        assert_eq!(names, ["warm-up", "writing speed, 1 at once", "writing speed, 2 at once", "writing speed, 4 at once", "reading speed, a 1000-token prompt", "reading speed, a 4000-token prompt", "sanity checks"]);
        assert_eq!(mini.iter().filter(|s| matches!(s, Step::Mini { .. })).map(Step::max_concurrency).max(), Some(0), "its requests are counted as our own while in flight: nothing else is tolerated");
        assert_eq!(plan_builtin(Profile::Full, 0).iter().filter(|s| matches!(s, Step::Mini { kind: MiniKind::Decode { .. }, .. })).count(), 4, "unknown slots: 1, 2, 4, 8");
        assert!(plan_builtin(Profile::Accuracy, 8).is_empty(), "accuracy needs the harness's datasets");
        let answer = |tokens: u64| serde_json::json!({"choices": [{"message": {"content": "word ".repeat(10)}}], "usage": {"completion_tokens": tokens, "prompt_tokens": 1024}});
        let cell = grade_mini_decode(2, &[(200, answer(128), 4.0), (200, answer(128), 5.0), (500, Value::Null, 0.1)]);
        assert_eq!((cell.per_user_tok_s, cell.total_tok_s, cell.errors, cell.ttft_ms), (28.8, 51.2, 1, None), "each user: mean of 32 and 25.6; together: 256 tokens in the 5 s the slower one took");
        assert_eq!(grade_mini_decode(1, &[(200, serde_json::json!({"choices": [{"message": {"content": "one two three four five six seven eight nine ten"}}]}), 1.0)]).per_user_tok_s, 13.0, "no usage block: counted by words");
        assert_eq!(grade_mini_prefill(1000, 200, &answer(1), 0.5), Some(PrefillCell { tokens: 1024, tok_s: 2048.0, ttft_ms: 500.0 }));
        assert_eq!(grade_mini_prefill(1000, 429, &Value::Null, 0.5), None);
        let a = mini_prefill_body("m", 1000)["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(a.len() > 3000 && a.ends_with("Reply with the single word OK."), "{}", a.len());
        assert!(!plan(Profile::Quick, &cfg(), "m", 8, "http://127.0.0.1:8090", "").iter().any(|s| matches!(s, Step::Garble { .. } | Step::Needle { .. })), "quick stays quick");
        // garbled output: U+FFFD and unasked-for CJK, per 100k characters
        let clean = "All good here. ".repeat(1000);
        assert_eq!(scan_garbled(&[clean.as_str()]).map(|g| (g.chars, g.per_100k)), Some((15_000, 0.0)));
        let broken = format!("{clean}{r}{r} 然后我们 カタカナ 한국어", r = '\u{FFFD}');
        let g = scan_garbled(&[broken.as_str()]).unwrap();
        assert_eq!((g.replacement_chars, g.cjk_chars), (2, 11));
        assert!((g.per_100k - 86.6).abs() < 0.2, "{g:?}");
        assert_eq!(scan_garbled::<&str>(&[]), None, "nothing scanned is not a clean bill");
        assert!(garble_requests("m", 6, 1_500).iter().all(|r| r["user"] == "lss-bench" && r["max_tokens"] == 1_500));
        let acc = plan(Profile::Accuracy, &cfg(), "m", 8, "http://127.0.0.1:8090", "");
        assert!(matches!(&acc[0], Step::Harness { args, output, .. } if args.join(" ").contains("--test-profile gsm8k") && output == "accuracy-gsm8k.json"), "gsm8k by default");
        let acc = plan(Profile::Accuracy, &cfg(), "m", 8, "http://127.0.0.1:8090", "mmlu-pro");
        assert!(matches!(&acc[0], Step::Harness { args, .. } if args.join(" ").contains("--test-profile mmlu-pro")));
        assert!(matches!(&plan(Profile::Accuracy, &cfg(), "m", 8, "x", "rm -rf")[0], Step::Harness { args, .. } if args.join(" ").contains("gsm8k")), "only the pinned datasets");
        // the dry run sends the model nothing
        assert_eq!(plan(Profile::DryRun, &cfg(), "m", 8, "http://127.0.0.1:8090", ""), vec![Step::Harness { name: "harness --help (no load)".into(), role: Role::DryRun, args: vec!["--help".into()], output: String::new(), max_concurrency: 0 }]);
        assert_eq!((Profile::parse("quick"), Profile::parse("dry-run"), Profile::parse("nope")), (Some(Profile::Quick), Some(Profile::DryRun), None));
        assert!(Profile::Quick.timeout_secs(&cfg()) < Profile::Full.timeout_secs(&cfg()) && Profile::Full.timeout_secs(&cfg()) < Profile::Accuracy.timeout_secs(&cfg()));
    }

    #[test]
    fn over_ssh_a_step_runs_in_its_own_remote_directory_and_its_result_is_fetched_through_the_launcher() {
        // card #330: ssh runs the words in the REMOTE shell's cwd (the remote HOME)
        let ssh: Vec<String> = ["ssh", "-p", "2222", "u@gpu-box"].map(String::from).to_vec();
        let rd = remote_run_dir("1-quick-model-a", "");
        assert_eq!(rd, ".cache/lss-bench/1-quick-model-a", "relative: under the remote user's HOME, whoever that is");
        let step = with_launcher(&ssh, ["python3", "/h/bench.py", "--output", "decode.json"].map(String::from).to_vec());
        assert_eq!(in_remote_dir(&ssh, step, &rd).join(" "), "ssh -p 2222 u@gpu-box mkdir -p .cache/lss-bench/1-quick-model-a && cd .cache/lss-bench/1-quick-model-a && python3 /h/bench.py --output decode.json");
        assert_eq!(remote_fetch(&ssh, &rd, "decode.json").join(" "), "ssh -p 2222 u@gpu-box cat .cache/lss-bench/1-quick-model-a/decode.json", "the same launcher, options and all");
        assert_eq!(remote_cleanup(&ssh, &rd).join(" "), "ssh -p 2222 u@gpu-box rm -rf -- .cache/lss-bench/1-quick-model-a");
        // a name the remote shell could misread is reduced to safe characters; never empty, "." or ".."
        assert_eq!(remote_run_dir("3-full-org/model x;rm", ""), ".cache/lss-bench/3-full-org_model_x_rm");
        for bad in ["", ".", "..", "/"] {
            for token in ["", "9f3c", "..", "/"] {
                let d = remote_run_dir(bad, token);
                assert!(d.starts_with(".cache/lss-bench/") && !d.ends_with("/.") && !d.ends_with("/..") && !d.ends_with("bench/") && !d[17..].contains('/'), "{bad:?} {token:?} -> {d}");
            }
        }
        // card #340: the per-run token makes the same run name on two collectors two directories
        assert_eq!(remote_run_dir("1-quick-model-a", "0a1b2c3d4e5f6789"), ".cache/lss-bench/1-quick-model-a-0a1b2c3d4e5f6789");
        assert_ne!(remote_run_dir("1-quick-model-a", "0a1b"), remote_run_dir("1-quick-model-a", "0a1c"));
        assert_eq!(remote_run_dir("1-quick-m", "a;b c/d"), ".cache/lss-bench/1-quick-m-abcd", "only letters and digits of the token");
        // not ssh (systemd-run, docker exec -i): argv unchanged, the step runs in lss's run dir
        let scope: Vec<String> = ["systemd-run", "--user", "--scope"].map(String::from).to_vec();
        let plain = with_launcher(&scope, ["python3", "b.py"].map(String::from).to_vec());
        assert_eq!(in_remote_dir(&scope, plain.clone(), &rd), plain);
        assert!(!launcher_joins(&scope) && launcher_joins(&ssh) && launcher_joins(&["/usr/bin/ssh".to_string(), "h".into()]));
    }

    #[test]
    fn over_ssh_a_tilde_harness_is_left_for_the_remote_shell_to_expand() {
        // card #333: `~` over ssh is the REMOTE user's home; the collector's own home is wrong there
        let ssh: Vec<String> = ["ssh", "u@gpu-box"].map(String::from).to_vec();
        assert_eq!(harness_word(&ssh, " ~/bench/llm_decode_bench.py ", "/var/lib/me"), "~/bench/llm_decode_bench.py");
        assert_eq!(harness_word(&[], "~/bench/b.py", "/var/lib/me"), "/var/lib/me/bench/b.py", "no launcher: expanded here, as always");
        assert_eq!(harness_word(&["systemd-run".to_string(), "--user".into()], "~/b.py", "/var/lib/me"), "/var/lib/me/b.py", "a local launcher: expanded here");
        assert_eq!(harness_word(&ssh, "/opt/b.py", "/var/lib/me"), "/opt/b.py");
        // the tilde stays OUTSIDE the quotes, so the remote shell expands it
        let argv = command_line(&BenchConfig { python: "python3".into(), launcher: ssh.clone(), ..Default::default() }, "~/my bench/b.py", &["--output".to_string(), "decode.json".into()]);
        assert_eq!(argv.join(" "), "ssh u@gpu-box python3 ~/'my bench/b.py' --output decode.json");
        assert_eq!(remote_word("~/bench/b.py"), "~/bench/b.py");
        assert_eq!(remote_word("~"), "'~'", "a bare ~ is not a path lss hands over: quoted like any word");
        assert_eq!(remote_word("~user/x"), "'~user/x'", "only ~/ is the remote HOME");
        // and the shell really expands it that way
        if let Ok(o) = std::process::Command::new("sh").arg("-c").arg(format!("HOME=/srv/far; echo {}", remote_word("~/my bench/b.py"))).output() {
            assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "/srv/far/my bench/b.py");
        }
    }

    #[test]
    fn over_ssh_the_harness_commit_is_asked_of_the_remote_checkout() {
        // card #339: the checkout is on the far side - `git -C <its dir>` goes through the launcher
        let ssh: Vec<String> = ["ssh", "u@gpu-box"].map(String::from).to_vec();
        assert_eq!(remote_commit_argv(&ssh, "~/bench/llm_decode_bench.py").join(" "), "ssh u@gpu-box git -C ~/bench rev-parse --short HEAD");
        assert_eq!(remote_commit_argv(&ssh, "~/my bench/b.py").join(" "), "ssh u@gpu-box git -C ~/'my bench' rev-parse --short HEAD");
        assert_eq!(remote_commit_argv(&ssh, "~/b.py").join(" "), "ssh u@gpu-box git -C ~ rev-parse --short HEAD", "the remote HOME itself: a bare ~, unquoted");
        assert_eq!(remote_commit_argv(&ssh, "/opt/bench/b.py").join(" "), "ssh u@gpu-box git -C /opt/bench rev-parse --short HEAD");
        assert_eq!(remote_commit_argv(&ssh, "/b.py").join(" "), "ssh u@gpu-box git -C / rev-parse --short HEAD");
        assert_eq!(remote_commit_argv(&ssh, "b.py").join(" "), "ssh u@gpu-box git -C . rev-parse --short HEAD", "relative = the remote HOME, where ssh starts");
        // the answer: a short sha, or said to be unknown - never an empty field
        assert_eq!(remote_commit(Some("86cf05c\n")), "86cf05c");
        assert_eq!(remote_commit(Some("a banner line\n86cf05c\n")), "86cf05c", "the last line is git's");
        for bad in [None, Some(""), Some("HEAD\n"), Some("fatal: not a git repository"), Some("zz12345")] {
            assert_eq!(remote_commit(bad), REMOTE_COMMIT_UNKNOWN, "{bad:?}");
        }
        assert_eq!(REMOTE_COMMIT_UNKNOWN, "unknown (remote)");
    }

    #[test]
    fn the_command_line_puts_the_launcher_in_front() {
        let mut c = cfg();
        assert_eq!(command_line(&c, "/opt/b.py", &["--help".into()]), ["python3", "/opt/b.py", "--help"]);
        c.launcher = vec!["systemd-run".into(), "--user".into(), "--scope".into()];
        c.python = "/opt/venv/bin/python".into();
        assert_eq!(command_line(&c, "/opt/b.py", &["--help".into()]).join(" "), "systemd-run --user --scope /opt/venv/bin/python /opt/b.py --help");
        assert_eq!((split_host_port("http://127.0.0.1:8090/"), split_host_port("http://gpu"), split_host_port("https://ai.example.com/v1")), (("127.0.0.1".into(), Some(8090)), ("gpu".into(), None), ("https://ai.example.com/v1".into(), None)));
    }

    #[test]
    fn the_idle_gate_refuses_a_busy_or_recently_used_server() {
        let quiet = IdleObs { serve_up: true, idle_secs: 300, tokens_generated: Some(0.0), ..Default::default() };
        assert_eq!(idle_gate(&quiet, false), Ok(()));
        let busy = IdleObs { running: 2.0, ..quiet.clone() };
        assert!(idle_gate(&busy, false).unwrap_err().contains("busy (2 running, 0 queued)"));
        assert!(idle_gate(&IdleObs { queue: 1.0, ..quiet.clone() }, false).is_err());
        assert!(idle_gate(&IdleObs { other_users_inflight: 1, ..quiet.clone() }, false).is_err());
        // #54, 2026-09-21: three tiny COMPLETED requests must not block it if the counter is
        // flat - only real generation does
        let recent = IdleObs { tokens_generated: Some(1_240.0), ..quiet.clone() };
        assert!(idle_gate(&recent, false).unwrap_err().contains("1240 token(s) in the last 5 min"), "{:?}", idle_gate(&recent, false));
        // not enough history to PROVE it quiet (just (re)started) is treated as not idle, not as
        // a free pass - it self-resolves the moment two comparable samples exist
        let unknown = IdleObs { tokens_generated: None, ..quiet.clone() };
        assert!(idle_gate(&unknown, false).unwrap_err().contains("not enough history"), "{:?}", idle_gate(&unknown, false));
        // --force starts anyway (the abort watch still runs), but never on a serve that is down
        assert_eq!((idle_gate(&busy, true), idle_gate(&recent, true), idle_gate(&unknown, true)), (Ok(()), Ok(()), Ok(())));
        assert!(idle_gate(&IdleObs { serve_up: false, ..quiet }, true).is_err());
        // card #14: a refusal is a LABEL now, not a dead end - it has to name the way through it
        // it did not used to name. Four attempts on a box that is also the fleet's only serve
        // were refused here with nothing to do about it but wait for a quiet window the machine
        // never gives.
        for refusal in [idle_gate(&busy, false), idle_gate(&recent, false)] {
            assert!(refusal.unwrap_err().contains("--under-load"), "the refusal must say how to measure anyway");
        }
    }

    /// card #14. The scorecard has to be able to say what else was on the server while it was
    /// taken - and, just as importantly, to refuse to say "quiet" when nothing could tell the
    /// bench's own requests apart from anybody else's.
    #[test]
    fn the_background_load_is_sampled_across_the_run_and_never_guesses_quiet() {
        // the decode matrix at 8: 8 of ours on the engine, a v5.2 gate showing nobody else
        let alone = SafetyObs { running: Some(8.0), queue: Some(0.0), gateway_public: Some(0.0), gateway_trusted: Some(8.0), other_users_inflight: Some(0), own_gateway_inflight: 0, own_lane: OwnLane::Trusted };
        let mut s = LoadSampler::default();
        for _ in 0..30 {
            s.push(&alone, 8);
        }
        let quiet = s.finish(Some(412.0)).unwrap();
        assert_eq!((quiet.samples, quiet.polls_with_traffic, quiet.concurrent_avg, quiet.concurrent_max), (30, 0, Some(0.0), Some(0.0)));
        assert_eq!((quiet.class(), quiet.engine_tok_s), (LoadClass::Quiet, Some(412.0)));
        assert!(quiet.sentence().starts_with("quiet:") && quiet.sentence().contains("30 looks"), "{}", quiet.sentence());

        // the same run with two other users on it for a third of the polls: LOADED, and the
        // average is over the whole run, not over the busy part of it
        let mut s = LoadSampler::default();
        for i in 0..30 {
            s.push(&SafetyObs { other_users_inflight: Some(if i % 3 == 0 { 2 } else { 0 }), queue: Some(if i % 3 == 0 { 3.0 } else { 0.0 }), ..alone.clone() }, 8);
        }
        let loaded = s.finish(Some(690.0)).unwrap();
        assert_eq!((loaded.polls_with_traffic, loaded.concurrent_avg, loaded.concurrent_max, loaded.queue_max), (10, Some(0.67), Some(2.0), 3.0));
        assert_eq!(loaded.class(), LoadClass::Loaded);
        assert!(loaded.sentence().starts_with("UNDER LOAD:") && loaded.sentence().contains("10 of 30 looks"), "{}", loaded.sentence());

        // NO gateway: the engine's own total is all there is. It can show traffic...
        let blind = SafetyObs { running: Some(9.0), queue: Some(0.0), gateway_public: None, gateway_trusted: None, other_users_inflight: None, own_gateway_inflight: 0, own_lane: OwnLane::Trusted };
        let mut s = LoadSampler::default();
        s.push(&blind, 8);
        assert_eq!(s.finish(None).unwrap().class(), LoadClass::Loaded);
        // ...but it can NEVER prove the absence of it: a request shorter than the 2 s poll
        // begins and ends between two looks, so "8 running, 8 of them mine" is not "I was alone"
        let mut s = LoadSampler::default();
        for _ in 0..30 {
            s.push(&SafetyObs { running: Some(8.0), ..blind.clone() }, 8);
        }
        let unprovable = s.finish(None).unwrap();
        assert_eq!((unprovable.polls_with_traffic, unprovable.class()), (0, LoadClass::Unknown));
        assert_eq!(unprovable.source, LOAD_SRC_ENGINE);
        // a gateway that goes away mid-run downgrades the whole run's claim, it is not averaged out
        let mut s = LoadSampler::default();
        s.push(&alone, 8);
        s.push(&SafetyObs { running: Some(8.0), ..blind.clone() }, 8);
        assert_eq!((s.finish(None).unwrap().source.as_str(), s.finish(None).unwrap().class()), (LOAD_SRC_ENGINE, LoadClass::Unknown));
        // nothing readable at all, and no poll at all
        let mut s = LoadSampler::default();
        s.push(&SafetyObs::default(), 1);
        assert_eq!(s.finish(None).unwrap().class(), LoadClass::Unknown);
        assert_eq!(LoadSampler::default().finish(None), None);
        // a harness that has not ramped up yet must not manufacture negative foreign traffic
        assert_eq!(foreign_now(&SafetyObs { running: Some(2.0), ..blind }, 8).unwrap().0, 0.0);
        // and a scorecard with no block at all answers Unknown, never Quiet
        assert_eq!(Scorecard::default().load_class(), LoadClass::Unknown);
        assert!(Scorecard::default().load_sentence().contains("do not read this as quiet"));
    }

    /// card #14, 2026-09-23, bought by a live failure: `lss` was updated on the server and
    /// `lss-collector` was not, so `--under-load` went over the wire to a collector that had
    /// never heard of it. `BenchRequest` is `#[serde(default)]` with no `deny_unknown_fields`,
    /// so the flag was dropped in silence and the answer was indistinguishable from one to a
    /// request that never carried it - the operator got "pass --force", passed it, and watched
    /// the run die at the first foreign request. The ECHO is what makes that skew visible.
    #[test]
    fn the_answer_says_what_the_collector_understood_so_a_version_skew_cannot_be_silent() {
        // an unknown field is ignored, which is the whole hazard: a request is never refused for
        // carrying a flag the collector is too old to know
        let req: BenchRequest = serde_json::from_str(r#"{"profile":"quick","under_load":true,"invented_later":1}"#).unwrap();
        assert!(req.under_load && req.profile == "quick");
        // an OLD collector's answer has no `under_load` at all: it must read false
        let old: BenchStarted = serde_json::from_str(r#"{"v":1,"ok":false,"run_id":null,"message":"the server is busy"}"#).unwrap();
        assert!(!old.under_load, "no echo means the collector did not understand the flag");
        let fresh: BenchStarted = serde_json::from_str(r#"{"v":1,"ok":true,"run_id":2,"message":"started","under_load":true}"#).unwrap();
        assert!(fresh.under_load);
    }

    /// #54, 2026-09-21: a probe running roughly as often as the idle window itself must not, on
    /// its own, look like real traffic and permanently block the bench.
    #[test]
    fn real_tokens_generated_nets_out_the_probes_own_footprint_and_never_goes_negative() {
        assert_eq!(real_tokens_generated(None, 128.0), None, "not enough history stays not enough history");
        // exactly one probe's worth of growth, nothing else: nets to zero, not negative
        assert_eq!(real_tokens_generated(Some(128.0), 128.0), Some(0.0));
        // a probe plus real traffic: the probe's share is netted out, real traffic is not
        assert_eq!(real_tokens_generated(Some(1_368.0), 128.0), Some(1_232.0));
        // two probes inside the window (a short idle_secs, or a probe near each edge)
        assert_eq!(real_tokens_generated(Some(256.0), 256.0), Some(0.0));
        // never negative even if probe accounting slightly overshoots the raw growth
        assert_eq!(real_tokens_generated(Some(100.0), 128.0), Some(0.0));
    }

    /// Card #207, 2026-09-23. The decode matrix at 8 puts 8 requests straight on the engine, and
    /// SGLang files every one of them under `priority="0"` - the TRUSTED lane - because they
    /// carry no priority of their own. The old watch read that lane as "came in through the
    /// gateway", so every plain run aborted on its own traffic at its first poll. The shape of
    /// this test is the fix: `gateway_trusted` is the bench's OWN load in the cases that must
    /// survive, and the run survives them.
    /// card #212: the bench's own priority-less load is subtracted from whichever lane the
    /// CONFIG says carries the engine's default priority "0" - on a box whose PUBLIC lane is 0,
    /// that is the public counter, and #207's hard-wired "trusted" aborted every run there.
    #[test]
    fn the_benchs_own_load_is_subtracted_from_the_lane_that_carries_priority_0() {
        assert_eq!(own_lane_for("10", "0"), OwnLane::Trusted, "this project's own setup");
        assert_eq!(own_lane_for("0", "10"), OwnLane::Public);
        assert_eq!(own_lane_for("5", "10"), OwnLane::Neither);
        // one bench request, in flight, sitting in the priority-0 lane
        let base = SafetyObs { running: Some(1.0), queue: Some(0.0), gateway_public: Some(0.0), gateway_trusted: Some(0.0), other_users_inflight: None, own_gateway_inflight: 0, own_lane: OwnLane::Trusted };
        let trusted_is_0 = SafetyObs { gateway_trusted: Some(1.0), ..base.clone() };
        assert_eq!(foreign_now(&trusted_is_0, 1).unwrap().0, 0.0);
        let public_is_0 = SafetyObs { gateway_public: Some(1.0), own_lane: OwnLane::Public, ..base.clone() };
        assert_eq!(foreign_now(&public_is_0, 1).unwrap().0, 0.0, "public lane = 0: the bench's own request is not foreign");
        assert_eq!(abort_reason(&public_is_0, 1), None);
        // the same counter under #207's old assumption WOULD have convicted the bench
        assert_eq!(foreign_now(&SafetyObs { own_lane: OwnLane::Trusted, ..public_is_0.clone() }, 1).unwrap().0, 1.0);
        // teeth: one more than the bench could have launched is still somebody else
        let one_more = SafetyObs { gateway_public: Some(2.0), running: Some(2.0), ..public_is_0.clone() };
        assert!(abort_reason(&one_more, 1).is_some());
        // neither lane is 0: the bench is in neither counter, so both lanes count whole
        let neither = SafetyObs { gateway_public: Some(1.0), gateway_trusted: Some(1.0), own_lane: OwnLane::Neither, ..base };
        assert_eq!(foreign_now(&neither, 1).unwrap().0, 2.0);
    }

    #[test]
    fn the_abort_watch_tells_real_traffic_from_the_bench() {
        // the decode matrix at 8: all 8 of the bench's own, in the trusted lane, gate says nobody else
        let own = SafetyObs { running: Some(8.0), queue: Some(0.0), gateway_public: Some(0.0), gateway_trusted: Some(8.0), other_users_inflight: Some(0), own_gateway_inflight: 0, own_lane: OwnLane::Trusted };
        assert_eq!(abort_reason(&own, 8), None, "#207: the bench's own direct load is not foreign traffic");
        // and the authoritative source is not overruled by a weaker one: a user table saying
        // ZERO ends the question, which is the precedence `foreign_now` always had
        assert_eq!(foreign_now(&own, 8), Some((0.0, LOAD_SRC_USERS)));
        // a gateway publishing /gate/health v5.2+: a user that is not lss-bench has a request in flight
        assert!(abort_reason(&SafetyObs { other_users_inflight: Some(1), ..own.clone() }, 8).unwrap().contains("not the bench"));
        // v5.1 (no user table), the bench's own 8 still in the trusted lane: still not traffic
        let v51 = SafetyObs { other_users_inflight: None, ..own.clone() };
        assert_eq!(abort_reason(&v51, 8), None);
        // ... a NINTH request in that lane is somebody else, and is caught
        assert!(abort_reason(&SafetyObs { gateway_trusted: Some(9.0), running: Some(9.0), ..v51.clone() }, 8).unwrap().contains("through the gateway"));
        // ... and one in the PUBLIC lane is foreign whatever the bench is doing: the bench has no
        // key for that lane, so it is never hidden behind the bench's own eight
        assert!(abort_reason(&SafetyObs { gateway_public: Some(1.0), running: Some(9.0), ..v51.clone() }, 8).unwrap().contains("through the gateway"));
        // when the lanes cannot be read either, more running than the bench launched is the last
        // evidence left - and the only evidence that can see somebody who bypassed the gateway
        let blind = SafetyObs { running: Some(9.0), gateway_public: None, gateway_trusted: None, other_users_inflight: None, ..own.clone() };
        assert!(abort_reason(&blind, 8).unwrap().contains("9 requests running, the bench launched at most 8"));
        assert_eq!(abort_reason(&SafetyObs { running: Some(8.0), ..blind.clone() }, 8), None);
        // that backstop stays armed even when a gateway source answered zero: a stranger who
        // posts straight to the engine never enters the user table at all
        assert!(abort_reason(&SafetyObs { running: Some(9.0), gateway_trusted: Some(8.0), ..own.clone() }, 8).unwrap().contains("the bench launched at most 8"));
        // the bench's OWN gateway request (a sanity check) is not foreign traffic
        let sanity = SafetyObs { running: Some(1.0), gateway_public: Some(0.0), gateway_trusted: Some(1.0), other_users_inflight: Some(0), own_gateway_inflight: 1, queue: Some(0.0), own_lane: OwnLane::Trusted };
        assert_eq!(abort_reason(&sanity, 1), None);
        assert!(abort_reason(&SafetyObs { running: Some(3.0), gateway_trusted: Some(3.0), other_users_inflight: None, ..sanity }, 1).is_some());
        // an unreadable poll is not evidence of traffic
        assert_eq!(abort_reason(&SafetyObs::default(), 1), None);
    }

    #[test]
    fn a_real_harness_result_becomes_a_scorecard() {
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(harness_version(&doc).as_deref(), Some("0.4.29"));
        let decode = decode_cells(&doc);
        assert_eq!(decode.len(), 2);
        assert_eq!((decode[0].concurrency, decode[0].context, decode[0].per_user_tok_s, decode[0].total_tok_s, decode[0].ttft_ms), (1, 0, 86.8, 86.8, Some(143.3)));
        assert_eq!((decode[1].concurrency, decode[1].total_tok_s), (4, 361.8));
        assert!((decode[1].per_user_tok_s * 4.0 - decode[1].total_tok_s).abs() < 5.0, "per user x users is about the total: {:?}", decode[1]);
        let prefill = prefill_cells(&doc);
        assert_eq!(prefill.iter().map(|p| (p.tokens, p.tok_s)).collect::<Vec<_>>()[..2], [(8198, 5791.0), (64509, 6551.0)]);
        assert_eq!(prefill[0].ttft_ms, 1416.0);
        let card = Scorecard { decode, prefill, status: "ok".into(), ..Default::default() };
        let h = card.headline();
        assert_eq!((h.c1_tok_s, h.ttft_c1_ms, h.max_total_tok_s, h.max_total_at, h.prefill_8k_tok_s, h.accuracy), (Some(86.8), Some(143.3), Some(361.8), Some(4), Some(5791.0), None));
        assert_eq!(card.prefill_near(65_536).map(|p| p.tok_s), Some(6551.0));
        // garbage in, nothing out: never a made-up zero
        let junk: Value = serde_json::json!({"results": [{"concurrency": 0}, {"aggregate_tps": 5}], "prefill": {"x": {"tok_per_sec": 0}}});
        assert!(decode_cells(&junk).is_empty() && prefill_cells(&junk).is_empty() && harness_version(&junk).is_none());
        assert_eq!(Scorecard::default().headline(), Headline::default());
    }

    #[test]
    fn accuracy_comes_from_the_dataset_profile_report() {
        let doc = serde_json::json!({"accuracy": {"items_total": 1319, "scored": 1319, "correct": 1247, "accuracy": 0.945413, "wilson95_low": 0.9318, "wilson95_high": 0.9564}});
        let a = accuracy_of(&doc, "gsm8k", "/runs/7/accuracy-gsm8k.json").unwrap();
        assert_eq!((a.dataset.as_str(), a.score, a.n, a.correct), ("gsm8k", 0.9454, 1319, 1247));
        assert!(accuracy_of(&serde_json::json!({"accuracy": null}), "gsm8k", "").is_none());
        assert!(accuracy_of(&serde_json::json!({"accuracy": {"scored": 0}}), "gsm8k", "").is_none());
    }

    #[test]
    fn bench_rows_override_the_live_curve_and_say_so() {
        let mut live = crate::loadout::Curve::default();
        for _ in 0..10 {
            live.observe(1.0, 180.0);
            live.observe(2.0, 300.0);
        }
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        let card = Scorecard { decode: decode_cells(&doc), ..Default::default() };
        let merged = merge_curve(&live.rows(4), Some(&card));
        let view: Vec<(u64, &str, Option<f64>)> = merged.iter().map(|r| (r.running, r.source.as_str(), r.tok_s)).collect();
        assert_eq!(view, vec![(1, "bench", Some(86.8)), (2, "live", Some(300.0)), (3, "live", None), (4, "bench", Some(361.8))]);
        assert_eq!(merged[0].samples, 10, "the live sample count stays visible");
        assert_eq!(merge_curve(&live.rows(2), None).iter().filter(|r| r.source == "bench").count(), 0);
    }

    #[test]
    fn the_three_sanity_checks_are_graded_from_the_answer() {
        let reqs = sanity_requests("model-a");
        assert_eq!(reqs.iter().map(|r| r.0).collect::<Vec<_>>(), [SANITY_ARITHMETIC, SANITY_TOOL, SANITY_JSON]);
        assert!(reqs.iter().all(|(_, b)| b["model"] == "model-a" && b["user"] == "lss-bench" && b["stream"] == false));
        assert_eq!(reqs[1].1["tool_choice"]["function"]["name"], "get_weather");
        assert_eq!(reqs[2].1["response_format"]["type"], "json_object");
        let say = |text: &str| serde_json::json!({"choices": [{"message": {"content": text}}]});
        assert!(grade_sanity(SANITY_ARITHMETIC, 200, &say("391")).pass);
        assert!(grade_sanity(SANITY_ARITHMETIC, 200, &say("<think>17*23 = 340 + 51</think>\n391.")).pass, "a reasoning block is not the answer");
        assert!(!grade_sanity(SANITY_ARITHMETIC, 200, &say("The answer is 401")).pass);
        assert!(grade_sanity(SANITY_ARITHMETIC, 503, &say("391")).detail.starts_with("HTTP 503"));
        let tool = serde_json::json!({"choices": [{"message": {"content": null, "tool_calls": [{"function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}}]}}]});
        assert_eq!(grade_sanity(SANITY_TOOL, 200, &tool).detail, "get_weather(city=Paris)");
        assert!(!grade_sanity(SANITY_TOOL, 200, &say("It is sunny in Paris.")).pass);
        let bad_args = serde_json::json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "get_weather", "arguments": "{city: Paris"}}]}}]});
        assert!(!grade_sanity(SANITY_TOOL, 200, &bad_args).pass);
        assert!(grade_sanity(SANITY_JSON, 200, &say("{\"colour\": \"red\", \"count\": 3}")).pass);
        assert!(grade_sanity(SANITY_JSON, 200, &say("```json\n{\"colour\": \"red\", \"count\": 3}\n```")).pass);
        assert!(!grade_sanity(SANITY_JSON, 200, &say("Sure! Here is the JSON: {colour: red}")).pass);
        assert!(!grade_sanity(SANITY_JSON, 200, &say("[1, 2]")).pass);
    }

    #[test]
    fn the_needle_sits_where_it_is_asked_to_and_is_graded() {
        let p = needle_prompt(2_000, 50);
        let at = p.find(NEEDLE_CODE).unwrap() as f64 / p.len() as f64;
        assert!((0.4..0.6).contains(&at), "needle at {at}");
        assert!((8_000..11_000).contains(&p.len()), "{} chars for ~2000 tokens", p.len());
        assert!(needle_prompt(2_000, 10).find(NEEDLE_CODE).unwrap() < needle_prompt(2_000, 90).find(NEEDLE_CODE).unwrap());
        assert_eq!(needle_prompt(500, 100).matches(NEEDLE_CODE).count(), 1, "exactly one needle, also at the very end");
        assert_eq!(needle_prompt(2_000, 50), needle_prompt(2_000, 50), "repeatable");
        let ok = serde_json::json!({"choices": [{"message": {"content": "The code is 7421-ORCHID-88."}}], "usage": {"prompt_tokens": 251_344}});
        let r = grade_needle(50, 200, &ok, 41.27);
        assert_eq!((r.pass, r.prompt_tokens, r.depth_pct, r.secs), (true, 251_344, 50, 41.3));
        assert!(!grade_needle(10, 200, &serde_json::json!({"choices": [{"message": {"content": "I could not find it."}}]}), 1.0).pass);
        assert!(grade_needle(90, 413, &serde_json::json!({"error": "too large"}), 0.1).detail.starts_with("HTTP 413"));
    }

    #[test]
    fn the_accept_length_comes_from_the_servers_counters() {
        assert_eq!(accept_length(58_800.0, 10_000.0), Some(5.88));
        assert_eq!(accept_length(100.0, 0.0), None, "no verify passes = speculative decoding is off");
        assert_eq!(accept_length(0.0, 50.0), None);
    }

    // ---------------------------------------------------------- card #329
    fn doc(state: &str, generated_at: i64, runs: &[i64]) -> BenchDoc {
        BenchDoc {
            v: 1,
            generated_at,
            brief: BenchBrief { state: state.into(), step: Some("decode matrix".into()), step_index: 3, steps: 5, ..Default::default() },
            runs: runs.iter().map(|&id| Scorecard { run_id: id, status: "ok".into(), ..Default::default() }).collect(),
        }
    }

    #[test]
    fn a_document_older_than_the_run_is_not_yet_never_done() {
        // just started: the document still says idle and lists nothing - the old loop exited 0 here
        assert_eq!(follow_step(&doc("idle", 100, &[]), Some(7), None, 0), Follow::Waiting);
        assert_eq!(follow_step(&doc("idle", 100, &[6]), Some(7), Some(100), 2), Follow::Waiting, "an older run listed is not ours");
        assert_eq!(follow_step(&doc("running", 105, &[]), Some(7), Some(100), 4), Follow::Running("step 3/5: decode matrix".into()));
        // ended, not re-rendered yet: still waiting; re-rendered and listed: finished
        assert_eq!(follow_step(&doc("idle", 200, &[6]), Some(7), Some(200), 1), Follow::Waiting);
        let listed = doc("idle", 205, &[7, 6]);
        assert!(matches!(follow_step(&listed, Some(7), Some(200), 5), Follow::Finished(r) if r.run_id == 7));
        // a newer document that still does not list it, or the grace spent: lost, not success
        assert_eq!(follow_step(&doc("idle", 206, &[6]), Some(7), Some(200), 6), Follow::Lost);
        assert_eq!(follow_step(&doc("idle", 200, &[6]), Some(7), Some(200), FOLLOW_GRACE_SECS), Follow::Lost);
    }
}
