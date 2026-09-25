//! "Measure the LLM": one LOADOUT = (model id, container image, a sha256 of the normalised
//! launch args + the serving-related environment), taken from `docker inspect` of the serve
//! container, and everything the collector can say about how that loadout performed,
//! accumulated sample by sample so two loadouts (a model swap, or one flag changed)
//! can be compared side by side after a swap. A restart of the SAME configuration is the same
//! loadout: it keeps accumulating, and the restart is counted.
//!
//! Pure: samples, probes and closed histogram windows go in; a scorecard row comes out.

use crate::gatelog::TimeAgg;
use crate::hist::{ClosedWindow, HistAccum};
use crate::model::Sample;
use crate::probe::ProbeRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// `docker inspect --format` for the serve container: image, entrypoint path, args and env as JSON.
pub const LOADOUT_INSPECT_FORMAT: &str = "{{.Config.Image}}\t{{.Path}}\t{{json .Args}}\t{{json .Config.Env}}";
/// A gap between two samples longer than this is not integrated over (the collector was down).
pub const MAX_STEP_SECS: i64 = 30;
/// The concurrency curve never grows past this many levels, whatever the engine reports.
pub const CURVE_MAX_LEVELS: usize = 64;
/// Run-to-run noise: two speeds closer than this are "the same" (the owner's runbook, +-3 %).
pub const NOISE: f64 = 0.03;
/// The longest-prompt figure from the engine's histogram needs this many requests that were not
/// the monitor's own probes.
pub const MIN_REAL_PROMPTS: f64 = 20.0;
/// A concurrency level needs this many 5 s samples (30 s) before its average means anything.
pub const MIN_LEVEL_SAMPLES: u64 = 6;
/// How many C1 readings the median is taken over.
pub const C1_SAMPLES_KEPT: usize = 200;
/// What a LIVE level needs before it may say where speed stops growing: ten minutes of samples
/// (120 x 5 s) AND a real share of the serving time. Two dozen samples of a second request
/// reading a long prompt are not a throughput ceiling (2026-09-20: 22 samples at "2 at once"
/// told a healthy server it was full). A benchmark cell always counts: it was measured on purpose.
pub const MIN_SATURATION_SAMPLES: u64 = 120;
pub const MIN_SATURATION_SHARE: f64 = 0.05;
/// Prompt tokens read within one poll step from which the step counts as "reading": about half
/// a second of prefill on a fast box, enough to stall the requests being written.
pub const READING_STEP_TOKENS: f64 = 2_000.0;

/// What was read from `docker inspect`: (image, entrypoint + args, env).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inspected {
    pub image: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutIdentity {
    /// 12 hex characters of sha256(model, image, config hash): what `lss compare` takes
    pub id: String,
    pub model: String,
    pub image: String,
    /// sha256 of the normalised launch args (ORDER KEPT) + the serving-related environment
    /// (sorted). The args and the environment themselves are never stored (they can carry an
    /// API key); `flags` keeps the few that describe a loadout.
    pub args_hash: String,
    pub flags: String,
}

impl LoadoutIdentity {
    /// `model-a-serve:1.0` -> `1.0`; an image without a tag is shown whole.
    pub fn image_tag(&self) -> String {
        image_tag(&self.image)
    }
}

pub fn image_tag(image: &str) -> String {
    match image.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') && !tag.is_empty() => tag.to_string(),
        _ => image.rsplit('/').next().unwrap_or(image).to_string(),
    }
}

/// From a `LOADOUT_INSPECT_FORMAT` line. The env field is optional (an older format string).
pub fn parse_loadout_inspect(text: &str) -> Option<Inspected> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut f = line.splitn(4, '\t');
    let image = f.next()?.trim().to_string();
    let path = f.next()?.trim().to_string();
    let args: Vec<String> = serde_json::from_str(f.next()?.trim()).ok()?;
    let env: Vec<String> = f.next().and_then(|e| serde_json::from_str(e.trim()).ok()).unwrap_or_default();
    let mut all = vec![path];
    all.extend(args);
    (!image.is_empty()).then_some(Inspected { image, args: all, env })
}

/// Every arg with its inner whitespace runs collapsed, joined by single spaces. The ORDER is
/// kept: `--a 1 --b 2` and `--b 2 --a 1` are different launch lines (later flags can override
/// earlier ones), so they are different loadouts.
pub fn normalise_args(args: &[String]) -> String {
    args.iter().map(|a| a.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The environment that changes how a model is served: engine, kernel, NCCL and CUDA switches.
/// Image plumbing (`PATH`, `NV_*` package versions, locale) is not, and neither is anything
/// that looks like a credential: rotating a key must not look like a new loadout.
pub fn relevant_env(env: &[String]) -> Vec<String> {
    const PREFIXES: [&str; 12] = ["SGLANG_", "SGL_", "VLLM_", "NCCL_", "TORCH_", "PYTORCH_", "FLASHINFER_", "TRITON_", "CUDA_VISIBLE", "CUDA_DEVICE", "NVIDIA_VISIBLE", "OMP_NUM"];
    const SECRET: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    let mut out: Vec<String> = env
        .iter()
        .filter_map(|e| e.split_once('='))
        .filter(|(name, _)| PREFIXES.iter().any(|p| name.starts_with(p)) && !SECRET.iter().any(|w| name.to_ascii_uppercase().contains(w)))
        .map(|(name, value)| format!("{name}={}", value.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 over the normalised args, a separator, and the relevant environment.
pub fn config_hash(args: &[String], env: &[String]) -> String {
    sha256_hex(&format!("{}\n--env--\n{}", normalise_args(args), relevant_env(env).join("\n")))
}

/// The flags worth showing next to a loadout. Anything else (paths, names, keys) stays out.
pub fn safe_flags(args: &[String]) -> String {
    const SHOW: [(&str, &str); 17] = [
        ("--tp-size", "tp"), ("--tp", "tp"), ("--ep-size", "ep"), ("--context-length", "ctx"), ("--quantization", "quant"), ("--kv-cache-dtype", "kv"), ("--max-running-requests", "slots"),
        ("--mem-fraction-static", "mem"), ("--chunked-prefill-size", "chunk"), ("--speculative-algorithm", "spec"), ("--speculative-num-steps", "steps"), ("--speculative-eagle-topk", "topk"),
        ("--speculative-num-draft-tokens", "draft"), ("--attention-backend", "attn"), ("--cmdline-unreadable", "cmdline?"), ("--cmdline", "cmdline"),
        ("--engine-detail", "model"),
    ];
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (args[i].as_str(), None),
        };
        if let Some((_, short)) = SHOW.iter().find(|(f, _)| *f == flag) {
            let value = inline.or_else(|| args.get(i + 1).filter(|v| !v.starts_with("--")).cloned());
            if let Some(v) = value {
                out.push(format!("{short} {}", v.split_whitespace().collect::<Vec<_>>().join(" ")));
            } else {
                out.push(short.to_string()); // a valueless marker such as `cmdline?`
            }
        }
        i += 1;
    }
    out.join(" · ")
}

pub fn identity(model: &str, image: &str, args: &[String], env: &[String]) -> LoadoutIdentity {
    let args_hash = config_hash(args, env);
    let id = sha256_hex(&format!("{model}\n{image}\n{args_hash}"))[..12].to_string();
    LoadoutIdentity { id, model: model.to_string(), image: image.to_string(), args_hash, flags: safe_flags(args) }
}

/// The identity of an engine that is NOT a container: a plain process (the normal way to run
/// Ollama, LM Studio and llama-server), or a machine with no docker at all. The facts the engine
/// reports about itself take the place of the image and the launch arguments, so MODEL,
/// `lss loadouts` and the bench scorecards work there too (2026-09-20).
pub fn identity_from_engine(kind: &str, engine: &crate::engine::EngineIdentity) -> LoadoutIdentity {
    let model = engine.model.clone().unwrap_or_else(|| "(unknown model)".into());
    // "ollama:0.34.2" stands where "image:tag" does, so the screen's "image tag" column shows
    // the engine version; a version change is a new loadout
    let image = match &engine.version {
        Some(v) if !v.is_empty() => format!("{kind}:{v}"),
        _ => kind.to_string(),
    };
    let mut facts: Vec<String> = Vec::new();
    if let Some(c) = engine.context_len.filter(|c| *c > 0.0) {
        facts.push(format!("--context-length={c:.0}"));
    }
    if let Some(n) = engine.slots.filter(|n| *n > 0) {
        facts.push(format!("--max-running-requests={n}"));
    }
    // card #180 gate 3, a clean install against a real Ollama: the engine's own model detail
    // ("494.03M · Q4_K_M · 77% on the GPU") went in as `--quantization`, so the screen read
    // "quant 494.03M" - a parameter count labelled as a quantisation - and the GPU SHARE, which
    // is live placement rather than configuration, was hashed into the loadout id: the same model
    // re-loaded with a slightly different offload became a new loadout. It is its own fact now,
    // shown as "model", without the placement.
    let detail: Vec<&str> = engine.detail.split(" \u{b7} ").map(str::trim).filter(|d| !d.is_empty() && !d.ends_with("on the GPU")).collect();
    if !detail.is_empty() {
        facts.push(format!("--engine-detail={}", detail.join(" \u{b7} ")));
    }
    // #53: launch flags the engine does not publish. A readable command line makes two runs
    // that differ only in such flags DIFFERENT loadouts; where it cannot be read (macOS, a
    // remote engine, a missing process), say so - the hashed fact marks the loadout as
    // incomplete rather than silently merging what may be two different configurations.
    //
    // #71 (verifier-2): the raw line used to go straight into `facts` as `--cmdline=<whole
    // line>`, and `safe_flags` below shows `--cmdline`'s value verbatim (it is the one entry in
    // its SHOW list that was never meant to be a real, safely-valued flag) - so an --api-key, an
    // absolute model path or a --host value reached `.flags`, the DISPLAYED and PERSISTED field,
    // on every plain-process engine (llama-server, Ollama, LM Studio; never our own dockerized
    // SGLang, which is exactly why this sat unnoticed). A 12-hex digest gives the same
    // differentiation power (two different lines still hash to two different facts, so
    // `args_hash`/`id` still tell the loadouts apart) with none of the content.
    if engine.cmdline.is_empty() {
        facts.push("--cmdline-unreadable".into());
    } else {
        facts.push(format!("--cmdline={}", &sha256_hex(&engine.cmdline)[..12]));
    }
    identity(&model, &image, &facts, &[])
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CurveCell {
    pub samples: u64,
    pub tok_s_sum: f64,
    /// time to first token of the requests that started while this many were running
    pub ttft_sum_s: f64,
    pub ttft_n: f64,
    /// speculative-decoding accept length seen at this level
    pub spec_len_sum: f64,
    pub spec_n: u64,
}

/// Throughput vs concurrency, from the live samples: every sample with N requests running adds
/// its aggregate decode tok/s to level N. Built passively: nobody has to run a benchmark.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Curve {
    pub cells: Vec<CurveCell>,
}

impl Curve {
    fn cell(&mut self, running: f64) -> Option<&mut CurveCell> {
        if !running.is_finite() || running < 0.5 {
            return None;
        }
        let level = (running.round() as usize).clamp(1, CURVE_MAX_LEVELS);
        if self.cells.len() < level {
            self.cells.resize(level, CurveCell::default());
        }
        self.cells.get_mut(level - 1)
    }

    pub fn observe(&mut self, running: f64, tok_s: f64) {
        if !tok_s.is_finite() {
            return;
        }
        if let Some(c) = self.cell(running) {
            c.samples += 1;
            c.tok_s_sum += tok_s.max(0.0);
        }
    }

    /// `ttft_sum_s` / `ttft_n`: growth of the engine's TTFT histogram `_sum` / `_count` since
    /// the sample before; `spec_len`: the accept-length gauge (0 = no speculative decoding).
    pub fn observe_extra(&mut self, running: f64, ttft_sum_s: f64, ttft_n: f64, spec_len: f64) {
        if let Some(c) = self.cell(running) {
            if ttft_n > 0.0 && ttft_sum_s.is_finite() && ttft_sum_s >= 0.0 {
                c.ttft_sum_s += ttft_sum_s;
                c.ttft_n += ttft_n;
            }
            if spec_len.is_finite() && spec_len > 0.0 {
                c.spec_len_sum += spec_len;
                c.spec_n += 1;
            }
        }
    }

    /// Rows 1..=`slots` (and further if more than that was ever seen running).
    pub fn rows(&self, slots: usize) -> Vec<CurveRow> {
        let top = slots.max(self.cells.iter().rposition(|c| c.samples > 0).map_or(0, |i| i + 1)).clamp(1, CURVE_MAX_LEVELS);
        (1..=top)
            .map(|n| {
                let c = self.cells.get(n - 1).copied().unwrap_or_default();
                let tok_s = (c.samples > 0).then(|| round1(c.tok_s_sum / c.samples as f64));
                CurveRow {
                    running: n as u64,
                    samples: c.samples,
                    tok_s,
                    per_request_tok_s: tok_s.map(|t| round1(t / n as f64)),
                    ttft_ms: (c.ttft_n > 0.0).then(|| round1(c.ttft_sum_s / c.ttft_n * 1000.0)),
                    spec_accept_length: (c.spec_n > 0).then(|| (c.spec_len_sum / c.spec_n as f64 * 100.0).round() / 100.0),
                    source: "live".into(),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CurveRow {
    /// requests running at once ("users at once")
    pub running: u64,
    pub samples: u64,
    /// mean TOTAL decode tok/s at that concurrency; null = never seen
    pub tok_s: Option<f64>,
    /// the speed each of them gets
    pub per_request_tok_s: Option<f64>,
    /// time to the first word
    pub ttft_ms: Option<f64>,
    pub spec_accept_length: Option<f64>,
    /// `live` = measured passively from real traffic, `bench` = from `lss bench`
    pub source: String,
}

/// Where adding users stops adding total speed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Saturation {
    /// fewer than two concurrency levels with enough samples
    NotEnough,
    /// every level above `users` is within noise of (or below) the total speed at `users`.
    /// `from_bench` = a benchmark measured the levels; otherwise this is what the traffic of the
    /// last day happened to show, which the sentence says (added 2026-09-20).
    After {
        users: u64,
        total_tok_s: f64,
        #[serde(default)]
        from_bench: bool,
    },
    /// the highest level seen is still clearly faster than the ones below it
    StillGrowing { users: u64, total_tok_s: f64 },
}

impl Saturation {
    pub fn sentence(&self) -> String {
        match self {
            Saturation::NotEnough => "not enough traffic at different concurrency levels yet to say where total speed stops growing".into(),
            Saturation::After { users, total_tok_s, from_bench } => format!(
                "total speed stops growing after {users} user{} (about {total_tok_s:.0} tok/s in total){}",
                if *users == 1 { "" } else { "s" },
                if *from_bench { "" } else { ", in the traffic of the last day" }
            ),
            Saturation::StillGrowing { users, total_tok_s } => format!("total speed was still growing at {users} users, the most measured ({total_tok_s:.0} tok/s in total)"),
        }
    }
}

/// Where total speed stops growing, from a whole curve: the levels that qualify as evidence
/// decide it, but EVERY level that was observed at all - and the best total ever recorded -
/// can VETO it. A ceiling the same screen contradicts is worse than no ceiling: on 2026-09-20
/// levels 1 and 2 qualified (228.6 and 212.9 tok/s) while the table beside them showed 233 at
/// 3 and 289 at 4, and TOKENS showed a peak of 553 - and lss told the owner the server was full.
pub fn saturation_of(curve: &[CurveRow], peak_tok_s: Option<f64>) -> Saturation {
    let claim = saturation(&curve_points(curve));
    let Saturation::After { users, total_tok_s, .. } = claim else { return claim };
    let ceiling = total_tok_s * (1.0 + NOISE);
    // anything faster ABOVE that level, however few samples it has, says the ceiling is not one
    let beaten_by_a_level = curve.iter().filter(|r| r.running > users).filter_map(|r| r.tok_s).any(|t| t > ceiling);
    let beaten_by_peak = peak_tok_s.is_some_and(|p| p > ceiling);
    if beaten_by_a_level || beaten_by_peak {
        return Saturation::NotEnough;
    }
    let (_, from_bench) = curve_evidence(curve);
    Saturation::After { users, total_tok_s, from_bench }
}

/// `rows`: (users at once, total tok/s) of the levels that were measured. The saturation point
/// is the FIRST level that no higher level beats by more than the noise band.
pub fn saturation(rows: &[(u64, f64)]) -> Saturation {
    let mut pts: Vec<(u64, f64)> = rows.iter().copied().filter(|(n, t)| *n > 0 && t.is_finite() && *t > 0.0).collect();
    pts.sort_by_key(|p| p.0);
    pts.dedup_by_key(|p| p.0);
    if pts.len() < 2 {
        return Saturation::NotEnough;
    }
    for (i, (n, total)) in pts.iter().enumerate().take(pts.len() - 1) {
        if pts[i + 1..].iter().all(|(_, later)| *later <= total * (1.0 + NOISE)) {
            return Saturation::After { users: *n, total_tok_s: round1(*total), from_bench: false };
        }
    }
    let (n, total) = pts[pts.len() - 1];
    Saturation::StillGrowing { users: n, total_tok_s: round1(total) }
}

/// Is this level evidence for a capacity statement? (See `MIN_SATURATION_SAMPLES`.)
pub fn qualifies(row: &CurveRow, live_samples: u64) -> bool {
    row.source == "bench" || (row.samples >= MIN_SATURATION_SAMPLES && row.samples as f64 >= live_samples as f64 * MIN_SATURATION_SHARE)
}

/// All live samples of a curve: what a level's share is a share of.
pub fn live_samples(rows: &[CurveRow]) -> u64 {
    rows.iter().map(|r| r.samples).sum()
}

/// The levels of a curve that qualify as evidence, ready for `saturation`.
pub fn curve_points(rows: &[CurveRow]) -> Vec<(u64, f64)> {
    let total = live_samples(rows);
    rows.iter().filter(|r| qualifies(r, total)).filter_map(|r| r.tok_s.map(|t| (r.running, t))).collect()
}

/// How many levels qualify, and whether any of them is a benchmark cell: a claim from live
/// traffic alone needs at least three levels before it is worth a warning.
pub fn curve_evidence(rows: &[CurveRow]) -> (u64, bool) {
    let total = live_samples(rows);
    let good: Vec<&CurveRow> = rows.iter().filter(|r| qualifies(r, total) && r.tok_s.is_some()).collect();
    (good.len() as u64, good.iter().any(|r| r.source == "bench"))
}

/// Everything accumulated for one loadout. Stored as JSON; every field is additive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutAcc {
    /// container starts seen with this configuration (1 = never restarted)
    pub runs: u64,
    /// the container start time of the newest run
    pub last_started_at: i64,
    pub samples: u64,
    pub up_samples: u64,
    /// seconds integrated (gaps longer than `MAX_STEP_SECS` are not)
    pub secs: f64,
    /// of those, with at least one request running
    pub serving_secs: f64,
    pub joules: f64,
    pub serving_joules: f64,
    pub gen_tokens: f64,
    pub prompt_tokens: f64,
    pub cached_tokens: f64,
    pub requests: f64,
    /// seconds and COMPUTED prompt tokens of the intervals in which the GPUs were reading prompts
    /// (superseded by `read_*`: a 5 s step is far longer than most reads, so this pair read low)
    pub prefill_secs: f64,
    pub prefill_tokens: f64,
    /// READING SPEED: prompt tokens the GPUs computed, and the request-seconds the engine spent in
    /// its prefill forward pass for them (`per_stage_req_latency_seconds{stage="prefill_forward"}`)
    pub read_tokens: f64,
    pub read_secs: f64,
    /// the best one-minute reading speed, and the minute being filled: (start, tokens, seconds)
    pub read_peak_tok_s: f64,
    pub read_window: (i64, f64, f64),
    /// decode tok/s with ONE request running, by how long its conversation already was
    pub ctx_speed: BTreeMap<String, CurveCell>,
    /// container start -> first answer, seconds, newest last (only starts the collector watched)
    pub cold_starts: Vec<f64>,
    /// what the engine says its own start took (`startup_time_seconds`), newest run
    pub engine_startup_s: f64,
    pub curve: Curve,
    pub spec_len_sum: f64,
    pub spec_rate_sum: f64,
    pub spec_samples: u64,
    /// generated tokens and verify passes while speculative decoding was on: their ratio is the
    /// exact accept length
    pub spec_gen_tokens: f64,
    pub spec_verify_calls: f64,
    pub kv_peak: f64,
    pub kv_capacity_tokens: f64,
    pub context_len: f64,
    /// the most requests the engine allows at once (max slots), when known
    pub slots: u64,
    pub evicted_tokens: f64,
    /// samples in which the engine had retracted (paused and re-queued) a request for lack of KV
    pub retracted_samples: u64,
    /// request-seconds end to end, and of those spent in the prefill forward pass
    pub e2e_secs: f64,
    pub prefill_forward_secs: f64,
    pub c1_probes: u64,
    pub c1_sum: f64,
    pub c1_best: f64,
    /// the valid readings themselves, newest last, capped at `C1_SAMPLES_KEPT`: "one user alone"
    /// is their MEDIAN, which one odd reading cannot move (added 2026-09-20)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub c1_samples: Vec<f64>,
    pub ttft: HistAccum,
    pub itl: HistAccum,
    pub prompt_len: HistAccum,
    pub gen_len: HistAccum,
    pub gate_requests: u64,
    pub gate_5xx: u64,
    pub gate_429: u64,
    pub gate_503: u64,
    pub gate_499: u64,
    pub ttfb_by_size: BTreeMap<String, TimeAgg>,
    /// the largest answered prompt the gate logged (a gateway publishing /gate/health v5.2+), in estimated tokens
    pub prompt_max_tokens: f64,
    pub last_ts: i64,
}

impl LoadoutAcc {
    /// Another container start with this configuration (the first one included).
    pub fn note_run(&mut self, started_at: i64) {
        if started_at != self.last_started_at || self.runs == 0 {
            self.runs += 1;
            self.last_started_at = started_at;
        }
    }

    /// The collector watched this start from "container started" to "first answer".
    pub fn note_cold_start(&mut self, secs: f64) {
        if secs.is_finite() && secs > 0.0 {
            self.cold_starts.push((secs * 10.0).round() / 10.0);
            let extra = self.cold_starts.len().saturating_sub(COLD_STARTS_KEPT);
            self.cold_starts.drain(..extra);
        }
    }

    /// One 5 s sample. `prev` = the sample before it (None for the first one of a run).
    pub fn observe(&mut self, prev: Option<&Sample>, cur: &Sample) {
        self.samples += 1;
        self.up_samples += u64::from(cur.serve_up);
        self.last_ts = cur.ts;
        let dt = prev.map_or(0, |p| cur.ts - p.ts);
        let dt = if (1..=MAX_STEP_SECS).contains(&dt) { dt as f64 } else { 0.0 };
        let power = |s: &Sample| -> Option<f64> {
            let v: Vec<f64> = s.gpus.iter().filter_map(|g| g.power_w).collect();
            (!v.is_empty()).then(|| v.iter().sum())
        };
        let running = cur.metrics.as_ref().map_or(0.0, |m| m.running);
        let serving = running >= 1.0;
        if dt > 0.0 {
            self.secs += dt;
            if serving {
                self.serving_secs += dt;
            }
            // trapezoid between the two readings; one missing reading = the other one
            if let Some(w) = match (prev.and_then(power), power(cur)) {
                (Some(a), Some(b)) => Some((a + b) / 2.0),
                (a, b) => a.or(b),
            } {
                self.joules += w * dt;
                if serving {
                    self.serving_joules += w * dt;
                }
            }
        }
        for lane in [&cur.log.public, &cur.log.trusted] {
            self.gate_requests += lane.requests;
            for (code, n) in &lane.by_status {
                match code {
                    429 => self.gate_429 += n,
                    499 => self.gate_499 += n,
                    503 => self.gate_503 += n,
                    _ => {}
                }
                if code / 100 == 5 {
                    self.gate_5xx += n;
                }
            }
            for (bucket, t) in &lane.ttfb_by_size {
                self.ttfb_by_size.entry(bucket.clone()).or_default().merge(t);
            }
            self.prompt_max_tokens = self.prompt_max_tokens.max(lane.prompt_max);
        }
        let Some(m) = &cur.metrics else { return };
        // The concurrency curve is about WRITING capacity. A step in which a prompt was being
        // read measures something else: reading stalls the writers, and the dip is not a ceiling.
        // "Being read" = a prompt in the prefill queue now, or a real amount read during the step
        // (`READING_STEP_TOKENS`: a short chat prompt does not stall anyone). Without a previous
        // sample it cannot be told, and one sample is not worth a guess.
        let reading = m.prefill_inflight_reqs > 0.0 || prev.and_then(|p| p.metrics.as_ref()).is_none_or(|pm| m.computed_prompt_tokens_since(pm) >= READING_STEP_TOKENS);
        if !reading {
            self.curve.observe(m.running, m.gen_throughput);
        }
        if m.running.round() == 1.0 && m.gen_throughput > 0.0 && m.decode_sum_seq_lens > 0.0 {
            // one request being written: `decode_sum_seq_lens` is the length of its conversation
            let c = self.ctx_speed.entry(ctx_bucket(m.decode_sum_seq_lens).to_string()).or_default();
            c.samples += 1;
            c.tok_s_sum += m.gen_throughput;
        }
        if m.startup_time_s > 0.0 {
            self.engine_startup_s = m.startup_time_s;
        }
        self.kv_peak = self.kv_peak.max(m.kv_usage());
        self.kv_capacity_tokens = self.kv_capacity_tokens.max(m.max_total_tokens);
        self.context_len = self.context_len.max(m.context_len);
        self.retracted_samples += u64::from(m.retracted > 0.0);
        if serving && m.spec_accept_length > 0.0 {
            self.spec_len_sum += m.spec_accept_length;
            self.spec_rate_sum += m.spec_accept_rate;
            self.spec_samples += 1;
        }
        let mut ttft = (0.0, 0.0);
        if let (true, Some(pm)) = (dt > 0.0, prev.and_then(|p| p.metrics.as_ref())) {
            // a counter that went backwards = the engine restarted: nothing to add for this step;
            // nor across samples that carry different sets of counters (see `ServeMetrics::comparable`)
            let same = m.comparable(pm);
            let grown = |now: f64, before: f64| if same && now >= before { now - before } else { 0.0 };
            let generated = grown(m.generation_tokens_total, pm.generation_tokens_total);
            self.gen_tokens += generated;
            let prompt = grown(m.prompt_tokens_total, pm.prompt_tokens_total);
            self.prompt_tokens += prompt;
            let cached = grown(m.cached_tokens_total, pm.cached_tokens_total);
            self.cached_tokens += cached;
            self.requests += grown(m.requests_total, pm.requests_total);
            self.evicted_tokens += grown(m.evicted_tokens_total, pm.evicted_tokens_total);
            self.e2e_secs += grown(m.e2e_sum, pm.e2e_sum);
            self.prefill_forward_secs += grown(m.prefill_forward_sum, pm.prefill_forward_sum);
            let verify = grown(m.spec_verify_calls_total, pm.spec_verify_calls_total);
            if verify > 0.0 {
                self.spec_verify_calls += verify;
                self.spec_gen_tokens += generated;
            }
            // What the GPUs really read (never cache hits), against the request-seconds the engine
            // spent reading. The tokens are counted while a prompt is read and the seconds land
            // when its read ends, so the pair is only a speed over a window, never per step.
            let computed = m.computed_prompt_tokens_since(pm);
            let read_secs = grown(m.prefill_forward_sum, pm.prefill_forward_sum);
            if computed > 0.0 {
                // the fallback for an engine without the prefill timer: the 5 s steps in which
                // something was read (reads low: most reads are far shorter than a step)
                self.prefill_secs += dt;
                self.prefill_tokens += computed;
            }
            if m.prefill_forward_sum > 0.0 || pm.prefill_forward_sum > 0.0 {
                self.read_tokens += computed;
                self.read_secs += read_secs;
                if cur.ts - self.read_window.0 >= READ_PEAK_WINDOW_SECS {
                    let (start, tokens, _) = self.read_window;
                    // #44 D2, 2026-09-21: this used to divide by the WINDOW's request-seconds
                    // (many requests reading at once sum their own seconds past the window's own
                    // wall-clock length), which could report a peak many times the typical figure
                    // for the exact same hardware - live, 242,702 against a typical 3,052 with 8
                    // slots (80x). Wall-clock seconds elapsed cannot be inflated by concurrency:
                    // this is the same "prompt tokens per second" the typical figure already is,
                    // just the busiest minute rather than the whole life of the loadout. (the
                    // 3rd tuple slot, request-seconds, is kept only for the stored shape's own
                    // backward compatibility - a collector restart must still parse an older row.)
                    let wall_secs = (cur.ts - start) as f64;
                    if tokens >= READ_PEAK_MIN_TOKENS && wall_secs >= READ_PEAK_MIN_SECS {
                        self.read_peak_tok_s = self.read_peak_tok_s.max(tokens / wall_secs);
                    }
                    self.read_window = (cur.ts, 0.0, 0.0);
                }
                self.read_window.1 += computed;
                self.read_window.2 += read_secs;
            }
            ttft = (grown(m.ttft_sum, pm.ttft_sum), grown(m.ttft_count, pm.ttft_count));
        }
        self.curve.observe_extra(m.running, ttft.0, ttft.1, if serving { m.spec_accept_length } else { 0.0 });
    }

    /// A finished C1 probe: only a VALID idle reading counts.
    pub fn observe_probe(&mut self, p: &ProbeRecord) {
        if let (true, Some(v)) = (p.is_reading(), p.decode_tok_s) {
            self.c1_probes += 1;
            self.c1_sum += v;
            self.c1_best = self.c1_best.max(v);
            self.c1_samples.push(v);
            let over = self.c1_samples.len().saturating_sub(C1_SAMPLES_KEPT);
            self.c1_samples.drain(..over);
        }
    }

    /// Rebuild the C1 figures from the probes that are still considered readings (used by the
    /// startup repair after a stored probe has been re-judged).
    pub fn rebuild_c1(&mut self, probes: &[ProbeRecord]) {
        let readings: Vec<f64> = probes.iter().filter(|p| p.is_reading()).filter_map(|p| p.decode_tok_s).collect();
        self.c1_probes = readings.len() as u64;
        self.c1_sum = readings.iter().sum();
        self.c1_best = readings.iter().copied().fold(0.0, f64::max);
        let from = readings.len().saturating_sub(C1_SAMPLES_KEPT);
        self.c1_samples = readings[from..].to_vec();
    }

    /// A closed 1-minute histogram window (bucket deltas).
    pub fn observe_window(&mut self, w: &ClosedWindow) {
        if w.res != 60 {
            return;
        }
        self.ttft.add(&w.accs[0]);
        self.itl.add(&w.accs[2]);
        self.prompt_len.add(&w.accs[4]);
        self.gen_len.add(&w.accs[5]);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SizeRow {
    pub bucket: String,
    pub n: u64,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// One loadout's PASSIVE scorecard: what real traffic showed. `null` = not measured (yet),
/// never zero. The bench scorecards (`crate::bench::Scorecard`) sit next to it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutRow {
    pub id: String,
    pub model: String,
    pub image: String,
    pub image_tag: String,
    pub args_hash: String,
    pub flags: String,
    pub first_seen: i64,
    pub last_seen: i64,
    /// the container start of the newest run, and how many runs there were
    pub started_at: i64,
    pub runs: u64,
    /// the loadout serving right now
    pub current: bool,
    pub hours_observed: f64,
    pub uptime_pct: Option<f64>,
    // decode
    pub c1_tok_s: Option<f64>,
    pub c1_best_tok_s: Option<f64>,
    pub c1_probes: u64,
    /// observed per-request decode speed = 1 / inter-token latency: at the median token, and
    /// at the p90 token (9 tokens in 10 came at least this fast)
    pub decode_p50_tok_s: Option<f64>,
    pub decode_p90_tok_s: Option<f64>,
    pub slots: u64,
    pub curve: Vec<CurveRow>,
    pub saturation: Option<Saturation>,
    /// the best total tok/s of the curve and the concurrency it was seen at
    pub peak_tok_s: Option<f64>,
    pub peak_at_running: Option<u64>,
    /// LONG-CONVERSATION SPEED, passive: decode tok/s with one request running, by how much
    /// was already in its conversation (added 2026-09-20)
    pub long_context: Vec<CtxRow>,
    // prefill
    /// prompt tokens read per second WHILE READING (computed tokens / prefill request-seconds)
    pub prefill_tok_s: Option<f64>,
    /// the best minute (added 2026-09-20)
    pub prefill_peak_tok_s: Option<f64>,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p90_ms: Option<f64>,
    pub ttft_by_size: Vec<SizeRow>,
    // request shape
    pub avg_prompt_tokens: Option<f64>,
    pub avg_gen_tokens: Option<f64>,
    pub prompt_p50_tokens: Option<f64>,
    pub prompt_p95_tokens: Option<f64>,
    /// the longest real prompt: exact-ish from the gate (>= v5.2), else the upper edge of the
    /// highest occupied bucket of the engine's prompt-length histogram
    pub prompt_max_tokens: Option<f64>,
    pub prompt_max_is_bucket_edge: bool,
    // speculative decoding, cache, KV
    pub spec_accept_length: Option<f64>,
    pub spec_accept_rate: Option<f64>,
    pub cache_hit_share: Option<f64>,
    pub kv_peak: f64,
    pub kv_capacity_tokens: f64,
    pub context_len: f64,
    pub evicted_tokens: f64,
    pub retracted_pct: Option<f64>,
    /// share of the request-seconds spent reading the prompt (the rest is writing)
    pub prefill_time_share: Option<f64>,
    // volume
    pub gen_tokens: f64,
    pub prompt_tokens: f64,
    pub cached_tokens: f64,
    pub requests: f64,
    // efficiency
    pub tokens_per_joule: Option<f64>,
    pub wh_per_mtok: Option<f64>,
    /// the same with the idle hours' energy charged to the tokens too
    pub wh_per_mtok_incl_idle: Option<f64>,
    pub avg_watts_serving: Option<f64>,
    pub avg_watts: Option<f64>,
    /// energy per day at the average draw, idle hours included (added 2026-09-20)
    pub kwh_per_day: Option<f64>,
    /// money, only with `electricity_usd_per_kwh` configured (added 2026-09-20): per 1M generated
    /// tokens while serving, the same with the idle hours charged too, and per day
    pub usd_per_mtok: Option<f64>,
    pub usd_per_mtok_incl_idle: Option<f64>,
    pub usd_per_day: Option<f64>,
    /// COLD START, seconds from container start to the first answer: the newest, the mean of
    /// those watched, how many were watched; and what the engine says its own start took
    pub cold_start_s: Option<f64>,
    pub cold_start_avg_s: Option<f64>,
    pub cold_starts_seen: u64,
    pub engine_startup_s: Option<f64>,
    // reliability (gate's view of the requests)
    pub gate_requests: u64,
    pub error_rate: Option<f64>,
    pub rate_429: Option<f64>,
    pub rate_503: Option<f64>,
}

/// One row of the long-conversation table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CtxRow {
    /// `0` | `16k` | `64k` | `128k` | `250k`
    pub bucket: String,
    pub context_tokens: u64,
    pub tok_s: Option<f64>,
    pub samples: u64,
    /// `live` | `bench`
    pub source: String,
}

impl LoadoutRow {
    /// Money needs a price: `electricity_usd_per_kwh` in the collector's config. Unset = no cost shown.
    pub fn apply_price(&mut self, usd_per_kwh: Option<f64>) {
        let Some(price) = usd_per_kwh.filter(|p| p.is_finite() && *p > 0.0) else { return };
        let money = |wh: f64| ((wh / 1000.0 * price) * 10_000.0).round() / 10_000.0;
        self.usd_per_mtok = self.wh_per_mtok.map(money);
        self.usd_per_mtok_incl_idle = self.wh_per_mtok_incl_idle.map(money);
        self.usd_per_day = self.kwh_per_day.map(|k| ((k * price) * 100.0).round() / 100.0);
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn ratio(a: f64, b: f64) -> Option<f64> {
    (b > 0.0).then(|| a / b)
}

/// Long-conversation speed: how many tokens are already in the conversation while ONE request
/// is being written. (label, the context it stands for, upper edge of the bucket)
pub const CTX_BUCKETS: [(&str, u64, f64); 5] = [("0", 0, 4_096.0), ("16k", 16_384, 40_000.0), ("64k", 65_536, 98_000.0), ("128k", 131_072, 190_000.0), ("250k", 250_000, f64::INFINITY)];

pub fn ctx_bucket(context_tokens: f64) -> &'static str {
    CTX_BUCKETS.iter().find(|(_, _, upper)| context_tokens < *upper).map_or("250k", |(name, _, _)| name)
}

/// A reading-speed window must hold at least this much before it can be a peak.
const READ_PEAK_MIN_TOKENS: f64 = 4_000.0;
const READ_PEAK_MIN_SECS: f64 = 1.0;
const READ_PEAK_WINDOW_SECS: i64 = 60;
/// cold starts kept per loadout
pub const COLD_STARTS_KEPT: usize = 10;

/// The upper edge of the highest bucket that holds anything (None for the +Inf bucket).
fn top_bucket_edge(h: &HistAccum) -> Option<f64> {
    let i = h.counts.iter().rposition(|c| *c > 0.0)?;
    h.le.get(i).copied()
}

pub fn row(id: &LoadoutIdentity, acc: &LoadoutAcc, first_seen: i64, current: bool) -> LoadoutRow {
    let itl = |q: f64| acc.itl.quantile(q).filter(|s| *s > 0.0).map(|s| round1(1.0 / s));
    let curve = acc.curve.rows(acc.slots as usize);
    // the best total speed SEEN is a fact about the past, not a capacity claim: a level needs
    // only enough samples to be a level (`MIN_LEVEL_SAMPLES`), not the evidence `saturation` needs
    let peak = curve.iter().filter(|r| r.samples >= MIN_LEVEL_SAMPLES).filter_map(|r| r.tok_s.map(|t| (r.running, t))).max_by(|a, b| a.1.total_cmp(&b.1));
    let share = |n: u64| ratio(n as f64, acc.gate_requests as f64).map(round4);
    let exact_accept = ratio(acc.spec_gen_tokens, acc.spec_verify_calls).filter(|v| *v >= 1.0);
    // The engine's prompt-length histogram also counts the monitor's own small probes: until
    // real requests clearly outnumber them, its top bucket says nothing about real prompts.
    let real_prompts = acc.prompt_len.count - acc.c1_probes as f64;
    let (prompt_max, edge) = if acc.prompt_max_tokens > 0.0 {
        (Some(acc.prompt_max_tokens), false)
    } else if real_prompts >= MIN_REAL_PROMPTS {
        (top_bucket_edge(&acc.prompt_len), true)
    } else {
        (None, false)
    };
    LoadoutRow {
        id: id.id.clone(),
        model: id.model.clone(),
        image: id.image.clone(),
        image_tag: id.image_tag(),
        args_hash: id.args_hash.clone(),
        flags: id.flags.clone(),
        first_seen,
        last_seen: acc.last_ts,
        started_at: acc.last_started_at,
        runs: acc.runs,
        current,
        hours_observed: round4(acc.secs / 3600.0),
        uptime_pct: ratio(acc.up_samples as f64, acc.samples as f64).map(|v| round4(v * 100.0)),
        // the MEDIAN reading, not the mean: one buffered answer must not move "one user alone"
        c1_tok_s: if acc.c1_samples.is_empty() { ratio(acc.c1_sum, acc.c1_probes as f64).map(round1) } else { Some(round1(crate::rules::median(&acc.c1_samples))) },
        c1_best_tok_s: (acc.c1_probes > 0).then_some(round1(acc.c1_best)),
        c1_probes: acc.c1_probes,
        decode_p50_tok_s: itl(0.5),
        decode_p90_tok_s: itl(0.9),
        slots: acc.slots,
        saturation: Some(saturation_of(&curve, peak.map(|p| p.1))),
        peak_tok_s: peak.map(|p| p.1),
        peak_at_running: peak.map(|p| p.0),
        curve,
        long_context: CTX_BUCKETS
            .iter()
            .map(|(b, ctx, _)| {
                let c = acc.ctx_speed.get(*b).copied().unwrap_or_default();
                CtxRow { bucket: (*b).to_string(), context_tokens: *ctx, tok_s: (c.samples >= MIN_LEVEL_SAMPLES).then(|| round1(c.tok_s_sum / c.samples as f64)), samples: c.samples, source: "live".into() }
            })
            .collect(),
        prefill_tok_s: if acc.read_secs > 0.0 { ratio(acc.read_tokens, acc.read_secs).filter(|_| acc.read_tokens >= READ_PEAK_MIN_TOKENS).map(round1) } else { ratio(acc.prefill_tokens, acc.prefill_secs).map(round1) },
        prefill_peak_tok_s: (acc.read_peak_tok_s > 0.0).then(|| round1(acc.read_peak_tok_s)),
        ttft_p50_ms: acc.ttft.quantile(0.5).map(|s| round1(s * 1000.0)),
        ttft_p90_ms: acc.ttft.quantile(0.9).map(|s| round1(s * 1000.0)),
        ttft_by_size: crate::gatelog::SIZE_BUCKETS
            .iter()
            .map(|(b, _)| {
                let t = acc.ttfb_by_size.get(*b).copied().unwrap_or_default();
                SizeRow { bucket: (*b).to_string(), n: t.n, avg_ms: t.avg_ms().map(round1), max_ms: (t.n > 0).then_some(round1(t.max_ms)) }
            })
            .collect(),
        avg_prompt_tokens: acc.prompt_len.summary_raw().map(|s| round1(s.avg)),
        avg_gen_tokens: acc.gen_len.summary_raw().map(|s| round1(s.avg)),
        prompt_p50_tokens: acc.prompt_len.quantile(0.5).map(round1),
        prompt_p95_tokens: acc.prompt_len.quantile(0.95).map(round1),
        prompt_max_tokens: prompt_max,
        prompt_max_is_bucket_edge: edge && prompt_max.is_some(),
        spec_accept_length: exact_accept.or_else(|| ratio(acc.spec_len_sum, acc.spec_samples as f64)).map(|v| (v * 100.0).round() / 100.0),
        spec_accept_rate: ratio(acc.spec_rate_sum, acc.spec_samples as f64).map(round4),
        cache_hit_share: ratio(acc.cached_tokens, acc.prompt_tokens).map(|v| round4(v.min(1.0))),
        kv_peak: round4(acc.kv_peak),
        kv_capacity_tokens: acc.kv_capacity_tokens,
        context_len: acc.context_len,
        evicted_tokens: acc.evicted_tokens,
        retracted_pct: ratio(acc.retracted_samples as f64, acc.samples as f64).map(|v| round4(v * 100.0)),
        prefill_time_share: ratio(acc.prefill_forward_secs, acc.e2e_secs).map(|v| round4(v.min(1.0))),
        gen_tokens: acc.gen_tokens,
        prompt_tokens: acc.prompt_tokens,
        cached_tokens: acc.cached_tokens,
        requests: acc.requests,
        tokens_per_joule: ratio(acc.gen_tokens, acc.serving_joules).filter(|_| acc.gen_tokens > 0.0).map(round4),
        wh_per_mtok: ratio(acc.serving_joules / 3600.0, acc.gen_tokens / 1e6).map(round1),
        wh_per_mtok_incl_idle: ratio(acc.joules / 3600.0, acc.gen_tokens / 1e6).map(round1),
        avg_watts_serving: ratio(acc.serving_joules, acc.serving_secs).map(round1),
        avg_watts: ratio(acc.joules, acc.secs).map(round1),
        kwh_per_day: ratio(acc.joules, acc.secs).map(|w| round4(w * 24.0 / 1000.0)),
        usd_per_mtok: None,
        usd_per_mtok_incl_idle: None,
        usd_per_day: None,
        cold_start_s: acc.cold_starts.last().copied(),
        cold_start_avg_s: (!acc.cold_starts.is_empty()).then(|| round1(acc.cold_starts.iter().sum::<f64>() / acc.cold_starts.len() as f64)),
        cold_starts_seen: acc.cold_starts.len() as u64,
        engine_startup_s: (acc.engine_startup_s > 0.0).then(|| round1(acc.engine_startup_s)),
        gate_requests: acc.gate_requests,
        error_rate: share(acc.gate_5xx),
        rate_429: share(acc.gate_429),
        rate_503: share(acc.gate_503),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::GpuSample;
    use crate::prom::ServeMetrics;

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn sample(ts: i64, running: f64, tok_s: f64, gen_total: f64, watts_per_gpu: f64) -> Sample {
        Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics { running, gen_throughput: tok_s, generation_tokens_total: gen_total, prompt_tokens_total: gen_total * 10.0, cached_tokens_total: gen_total * 4.0, max_total_tokens: 3_788_160.0, context_len: 1_048_576.0, token_usage: running / 10.0, spec_accept_length: 3.0, spec_accept_rate: 0.7, ..Default::default() }),
            gpus_ok: true,
            gpus: (0..4).map(|i| GpuSample { index: i, power_w: Some(watts_per_gpu), ..Default::default() }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn identity_is_a_stable_sha256_and_a_changed_arg_or_env_is_a_new_loadout() {
        let a = strs(&["python3", "-m", "sglang.launch_server", "--tp-size", "4", "--context-length", "1048576"]);
        let spaced = strs(&["python3", " -m ", "sglang.launch_server", "--tp-size", "4 ", "--context-length", " 1048576"]);
        let reordered = strs(&["python3", "-m", "sglang.launch_server", "--context-length", "1048576", "--tp-size", "4"]);
        let env = strs(&["PATH=/usr/bin", "SGLANG_OPT_USE_TOPK_V2=1", "NCCL_P2P_LEVEL=SYS", "NV_CUDNN_VERSION=9.1", "LANG=C"]);
        assert_eq!(config_hash(&a, &env), config_hash(&spaced, &env), "whitespace is not a difference");
        assert_ne!(config_hash(&a, &env), config_hash(&reordered, &env), "the ORDER is: a later flag can override an earlier one");
        assert_ne!(config_hash(&a, &env), config_hash(&strs(&["python3", "-m", "sglang.launch_server", "--tp-size", "8", "--context-length", "1048576"]), &env), "a changed arg");
        assert_eq!(normalise_args(&strs(&["a  b", "", "\tc\n"])), "a b c");
        assert_eq!(config_hash(&a, &env).len(), 64);
        // the environment: serving switches count, image plumbing and credentials do not, order does not
        assert_eq!(relevant_env(&env), strs(&["NCCL_P2P_LEVEL=SYS", "SGLANG_OPT_USE_TOPK_V2=1"]));
        let mut shuffled = env.clone();
        shuffled.reverse();
        shuffled.push("PATH=/somewhere/else".into());
        shuffled.push("SGLANG_API_KEY=sk-rotated".into());
        assert_eq!(config_hash(&a, &env), config_hash(&a, &shuffled));
        assert_ne!(config_hash(&a, &env), config_hash(&a, &strs(&["SGLANG_OPT_USE_TOPK_V2=0", "NCCL_P2P_LEVEL=SYS"])), "a changed serving switch");
        let x = identity("model-a", "img:1", &a, &env);
        assert_eq!((x.id.len(), &x.id), (12, &identity("model-a", "img:1", &spaced, &shuffled).id), "the same configuration, whenever it is started, is the same loadout");
        assert_ne!(x.id, identity("model-b", "img:1", &a, &env).id);
        assert_ne!(x.id, identity("model-a", "img:2", &a, &env).id);
        assert_ne!(x.id, identity("model-a", "img:1", &reordered, &env).id);
        assert_eq!((image_tag("repo/model-a-serve:1.0"), image_tag("localhost:5000/serve"), image_tag("serve")), ("1.0".to_string(), "serve".to_string(), "serve".to_string()));
    }

    #[test]
    fn sha256_matches_the_published_test_vector() {
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn docker_inspect_line_and_the_flags_that_are_safe_to_show() {
        let line = "sglang:1.0\tpython3\t[\"-m\",\"sglang.launch_server\",\"--model-path\",\"/model\",\"--api-key\",\"sk-very-secret\",\"--tp-size\",\"4\",\"--context-length=1048576\",\"--quantization\",\"modelopt_fp4\",\"--max-running-requests\",\"8\",\"--disable-shared-experts-fusion\"]\t[\"PATH=/bin\",\"SGLANG_X=1\",\"HF_TOKEN=hf_secret\"]\n";
        let got = parse_loadout_inspect(line).unwrap();
        assert_eq!((got.image.as_str(), got.args[0].as_str(), got.args.len(), got.env.len()), ("sglang:1.0", "python3", 15, 3));
        let id = identity("m", &got.image, &got.args, &got.env);
        assert_eq!(id.flags, "tp 4 · ctx 1048576 · quant modelopt_fp4 · slots 8");
        let stored = serde_json::to_string(&id).unwrap();
        assert!(!stored.contains("secret") && !stored.contains("/model"), "neither the args nor the env are ever stored: {stored}");
        // the older three-field format still parses (no env)
        assert_eq!(parse_loadout_inspect("img\tpython3\t[\"-m\"]").map(|i| (i.args.len(), i.env.len())), Some((2, 0)));
        assert!(parse_loadout_inspect("").is_none() && parse_loadout_inspect("img\tpython3\tnot json").is_none());
    }

    #[test]
    fn the_concurrency_curve_buckets_samples_by_running_requests() {
        let mut c = Curve::default();
        for (running, tok_s) in [(1.0, 190.0), (1.0, 210.0), (2.0, 360.0), (4.0, 640.0), (4.0, 600.0), (4.0, 620.0), (8.0, 900.0), (11.0, 1000.0), (0.0, 5.0), (0.4, 5.0), (f64::NAN, 1.0)] {
            c.observe(running, tok_s);
        }
        c.observe_extra(4.0, 1.2, 3.0, 3.5);
        c.observe_extra(4.0, 0.6, 1.0, 2.5);
        c.observe_extra(0.0, 9.0, 9.0, 9.0);
        let rows = c.rows(8);
        assert_eq!(rows.len(), 11, "rows 1..=slots, and on to the highest level ever seen");
        assert_eq!((rows[0].samples, rows[0].tok_s, rows[0].per_request_tok_s), (2, Some(200.0), Some(200.0)));
        assert_eq!((rows[1].samples, rows[1].tok_s, rows[1].per_request_tok_s), (1, Some(360.0), Some(180.0)));
        assert_eq!((rows[3].samples, rows[3].tok_s, rows[3].per_request_tok_s), (3, Some(620.0), Some(155.0)));
        assert_eq!((rows[3].ttft_ms, rows[3].spec_accept_length), (Some(450.0), Some(3.0)), "1.8 s over 4 first tokens; accept length by level");
        assert_eq!((rows[2].samples, rows[2].tok_s, rows[2].ttft_ms), (0, None, None), "a level never seen is null, not zero");
        assert_eq!((rows[7].running, rows[7].samples, rows[10].running, rows[10].tok_s), (8, 1, 11, Some(1000.0)));
        assert_eq!(rows.iter().map(|r| r.samples).sum::<u64>(), 8, "idle samples are not on the curve");
        assert!(rows.iter().all(|r| r.source == "live"));
        assert_eq!(Curve::default().rows(4).len(), 4);
        // only levels with enough samples count as measured
        assert!(curve_points(&rows).is_empty());
    }

    /// 2026-09-20, LIVE: levels 1 and 2 qualified (1348 and 191 samples) and said "stops growing
    /// after 1 user, 228.6 tok/s" - while the same table showed 233 at 3 and 289 at 4, `/loadouts`
    /// recorded a peak of 289.4, and the TOKENS page a peak of 553 tok/s. A ceiling the rest of
    /// the screen contradicts is worse than no ceiling at all.
    #[test]
    fn a_ceiling_the_same_screen_contradicts_is_never_claimed() {
        let curve = |rows: &[(u64, u64, f64)]| -> Vec<CurveRow> { rows.iter().map(|(n, samples, total)| CurveRow { running: *n, samples: *samples, tok_s: Some(*total), per_request_tok_s: Some(total / *n as f64), source: "live".into(), ..Default::default() }).collect() };
        let live = curve(&[(1, 1348, 228.6), (2, 191, 212.9), (3, 58, 233.1), (4, 11, 289.4)]);
        // the qualified levels alone would say "after 1"
        assert_eq!(saturation(&curve_points(&live)), Saturation::After { users: 1, total_tok_s: 228.6, from_bench: false });
        // ... and the levels above it, and the recorded peak, each veto it on their own
        assert_eq!(saturation_of(&live, Some(289.4)), Saturation::NotEnough);
        assert_eq!(saturation_of(&live, None), Saturation::NotEnough, "level 4 alone is enough to veto");
        assert_eq!(saturation_of(&live[..2], Some(289.4)), Saturation::NotEnough, "the peak alone is enough to veto");
        // nothing above it and no faster peak: the ceiling stands, and says it is from live traffic
        assert_eq!(saturation_of(&live[..2], Some(228.6)), Saturation::After { users: 1, total_tok_s: 228.6, from_bench: false });
        // a benchmark measured it: the ceiling stands and says so
        let benched: Vec<CurveRow> = curve(&[(1, 0, 228.6), (2, 0, 212.9), (3, 0, 220.0), (4, 0, 225.0)]).into_iter().map(|mut r| { r.source = "bench".into(); r }).collect();
        assert_eq!(saturation_of(&benched, Some(228.6)), Saturation::After { users: 1, total_tok_s: 228.6, from_bench: true });
    }

    /// E2, 2026-09-20: an engine started as a plain process (the normal way to run Ollama, LM
    /// Studio, llama-server) never got a loadout, so MODEL, `lss loadouts` and every bench
    /// scorecard stayed empty - on exactly the machines lss is meant to be easy on.
    #[test]
    fn an_engine_that_is_not_a_container_still_has_an_identity() {
        use crate::engine::EngineIdentity;
        let ollama = EngineIdentity { model: Some("mistral:latest".into()), version: Some("0.34.2".into()), context_len: Some(8192.0), slots: None, detail: "7.2B · Q4_0".into(), cmdline: String::new() };
        let id = identity_from_engine("ollama", &ollama);
        assert_eq!((id.model.as_str(), id.image.as_str(), id.image_tag().as_str()), ("mistral:latest", "ollama:0.34.2", "0.34.2"));
        assert!(id.flags.contains("ctx 8192") && id.flags.contains("model 7.2B \u{b7} Q4_0") && !id.flags.contains("quant"), "{}", id.flags);
        // card #180 gate 3: where the model sits (GPU share) is live placement, not configuration -
        // shown nowhere in the identity, and a different offload is the SAME loadout
        let offloaded = EngineIdentity { detail: "7.2B · Q4_0 · 77% on the GPU".into(), ..ollama.clone() };
        let on_gpu = EngineIdentity { detail: "7.2B · Q4_0 · 100% on the GPU".into(), ..ollama.clone() };
        assert_eq!(identity_from_engine("ollama", &offloaded).id, identity_from_engine("ollama", &on_gpu).id);
        assert!(!identity_from_engine("ollama", &offloaded).flags.contains("GPU"), "{}", identity_from_engine("ollama", &offloaded).flags);
        assert_eq!(id.id.len(), 12);
        // the same engine, same model, same version = the same loadout across restarts
        assert_eq!(identity_from_engine("ollama", &ollama).id, id.id);
        // a new model, a new engine version or a new context length is a NEW loadout
        let other_model = EngineIdentity { model: Some("llama3:8b".into()), ..ollama.clone() };
        let upgraded = EngineIdentity { version: Some("0.35.0".into()), ..ollama.clone() };
        let re_served = EngineIdentity { context_len: Some(32_768.0), ..ollama.clone() };
        for other in [other_model, upgraded, re_served] {
            assert_ne!(identity_from_engine("ollama", &other).id, id.id);
        }
        // an engine that says almost nothing still gets a usable row (its flags now carry the
        // #53 `cmdline?` marker: no readable launch line, and the loadout says so)
        let bare = EngineIdentity { model: Some("some-model".into()), ..Default::default() };
        let bare = identity_from_engine("openai", &bare);
        assert_eq!((bare.model.as_str(), bare.image.as_str(), bare.flags.as_str()), ("some-model", "openai", "cmdline?"));
        assert_eq!(bare.id.len(), 12);
        // #53: the launch line the engine never publishes. Where it is readable it folds into
        // the identity: same model + version, different flags = two loadouts. Where it is not,
        // the loadout SAYS so (`cmdline?`) instead of silently merging the two runs.
        // #71: `.flags` shows a digest of the line, never the line itself (see
        // `an_unreadable_cmdline_leak_never_reaches_flags` for the security-focused version of
        // this same fact - this test only pins that a digest, not the raw text, is what shows).
        let read = EngineIdentity { model: Some("llama3:8b".into()), version: Some("b4700".into()), cmdline: "llama-server -m model.gguf --n-gpu-layers 999 --flash-attn 1".into(), ..Default::default() };
        let a = identity_from_engine("llamacpp", &read);
        assert!(a.flags.contains("cmdline ") && !a.flags.contains("llama-server"), "{}", a.flags);
        let different_flags = EngineIdentity { cmdline: "llama-server -m model.gguf --n-gpu-layers 0 --flash-attn 0".into(), ..read.clone() };
        let b = identity_from_engine("llamacpp", &different_flags);
        assert_ne!(a.id, b.id, "same model + version, different flags = two loadouts");
        // nothing changed but a re-run: the same loadout
        assert_eq!(identity_from_engine("llamacpp", &read).id, a.id);
        // unreadable: still an identity, and it is marked, never silently merged with a readable run
        let unreadable = EngineIdentity { model: Some("llama3:8b".into()), version: Some("b4700".into()), ..Default::default() };
        let c = identity_from_engine("llamacpp", &unreadable);
        assert!(c.flags.contains("cmdline?"), "{}", c.flags);
        assert_ne!(c.id, a.id, "an unreadable cmdline must not collapse into a readable run's loadout");
        let d = identity_from_engine("llamacpp", &EngineIdentity { ..unreadable });
        assert_eq!(c.id, d.id, "two unreadable runs share the engine-facts identity, as before");
    }

    /// card #71 (verifier-2): the exact reproducer from the card, run against the fixed code -
    /// an --api-key value, an absolute model path and a --host value must never reach `.flags`,
    /// which is displayed on page 1 (`ui/dash.rs`), the loadouts table (`ui/pages.rs`), `lss
    /// loadouts` (`report.rs`) and the collector's `/status` JSON (`LoadoutBrief`), and
    /// PERSISTED to the loadout DB - not one screen among several, every one of them.
    #[test]
    fn an_unreadable_cmdline_leak_never_reaches_flags() {
        use crate::engine::EngineIdentity;
        // assembled at run time, not a literal: a real-looking /home/<name>/... path here would
        // make the privacy scan trip on this file's own tracked source (card #15's re-check).
        let secret_key = "sk-SECRET";
        let private_path = format!("/{}/{}/models/private.gguf", "home", "someone");
        let private_host = "myhost.internal";
        let engine = EngineIdentity {
            model: Some("m".into()),
            version: Some("v".into()),
            cmdline: format!("llama-server --api-key {secret_key} -m {private_path} --host {private_host}"),
            ..Default::default()
        };
        let id = identity_from_engine("llama.cpp", &engine);
        for secret in [secret_key, private_path.as_str(), private_host, "someone", "myhost"] {
            assert!(!id.flags.contains(secret), "leaked into .flags: {secret:?} in {:?}", id.flags);
        }
        // still gives a usable, safe short form - not just an empty string with the leak scrubbed
        assert!(id.flags.contains("cmdline "), "{}", id.flags);
        // and still genuinely differentiates: a different secret line is still a different loadout
        let different = EngineIdentity { cmdline: format!("llama-server --api-key sk-OTHER -m {private_path} --host {private_host}"), ..engine.clone() };
        assert_ne!(identity_from_engine("llama.cpp", &different).id, id.id);
    }

    #[test]
    fn the_saturation_point_is_the_first_level_nothing_above_beats_by_more_than_noise() {
        // grows to 4 users, then flat (within 3 %), then falls
        assert_eq!(saturation(&[(1, 190.0), (2, 360.0), (4, 640.0), (6, 650.0), (8, 600.0)]), Saturation::After { users: 4, total_tok_s: 640.0, from_bench: false });
        // 3.1 % more at 8 than at 4 is growth, 2.9 % is noise
        assert_eq!(saturation(&[(4, 1000.0), (8, 1031.0)]), Saturation::StillGrowing { users: 8, total_tok_s: 1031.0 });
        assert_eq!(saturation(&[(4, 1000.0), (8, 1029.0)]), Saturation::After { users: 4, total_tok_s: 1000.0, from_bench: false });
        assert_eq!(saturation(&[(1, 190.0), (2, 360.0), (4, 640.0), (8, 1100.0)]), Saturation::StillGrowing { users: 8, total_tok_s: 1100.0 });
        // order and duplicates do not matter; one level (or none, or junk) is not a curve
        assert_eq!(saturation(&[(8, 600.0), (1, 190.0), (4, 640.0), (4, 640.0)]), Saturation::After { users: 4, total_tok_s: 640.0, from_bench: false });
        assert_eq!(saturation(&[(1, 190.0)]), Saturation::NotEnough);
        assert_eq!(saturation(&[(1, f64::NAN), (0, 5.0), (2, 0.0)]), Saturation::NotEnough);
        assert_eq!(saturation(&[(1, 200.0), (2, 190.0)]), Saturation::After { users: 1, total_tok_s: 200.0, from_bench: false });
        assert_eq!(Saturation::After { users: 4, total_tok_s: 640.0, from_bench: true }.sentence(), "total speed stops growing after 4 users (about 640 tok/s in total)");
        // a claim from live traffic says so: it is what today happened to show, not a measurement
        assert!(Saturation::After { users: 4, total_tok_s: 640.0, from_bench: false }.sentence().ends_with(", in the traffic of the last day"));
        assert!(Saturation::NotEnough.sentence().starts_with("not enough traffic"));
        // from a live curve: a level needs ten minutes of samples AND a real share of the
        // traffic before it is believed
        let mut c = Curve::default();
        for _ in 0..MIN_SATURATION_SAMPLES {
            c.observe(1.0, 200.0);
            c.observe(2.0, 380.0);
            c.observe(4.0, 385.0);
        }
        for _ in 0..MIN_LEVEL_SAMPLES * 2 {
            c.observe(8.0, 5000.0); // a dozen lucky samples: shown in the table, never evidence
        }
        assert_eq!(saturation(&curve_points(&c.rows(8))), Saturation::After { users: 2, total_tok_s: 380.0, from_bench: false });
        assert_eq!(serde_json::to_value(Saturation::After { users: 2, total_tok_s: 380.0, from_bench: false }).unwrap()["kind"], "after");
    }

    #[test]
    fn tokens_per_joule_integrates_power_over_a_fake_clock() {
        // 1000 s at 4 x 150 W = 600 W with one request running and 100 tok/s: 100 000 tokens for 600 kJ
        let mut acc = LoadoutAcc::default();
        let mut prev: Option<Sample> = None;
        for k in 0..=200 {
            let s = sample(10_000 + k * 5, 1.0, 100.0, (k * 500) as f64, 150.0);
            acc.observe(prev.as_ref(), &s);
            prev = Some(s);
        }
        // then 1000 s idle at 4 x 25 W = 100 W: 100 kJ more, no tokens
        for k in 1..=200 {
            let s = sample(11_000 + k * 5, 0.0, 0.0, 100_000.0, 25.0);
            acc.observe(prev.as_ref(), &s);
            prev = Some(s);
        }
        assert_eq!((acc.secs, acc.serving_secs, acc.gen_tokens), (2000.0, 1000.0, 100_000.0));
        // the first idle interval is a trapezoid from 600 W down to 100 W
        assert!((acc.serving_joules - 600_000.0).abs() < 1e-6, "{}", acc.serving_joules);
        assert!((acc.joules - (600_000.0 + 100_000.0 + 1250.0)).abs() < 1e-6, "{}", acc.joules);
        let r = row(&LoadoutIdentity::default(), &acc, 10_000, true);
        assert_eq!(r.tokens_per_joule, Some(0.1667));
        assert_eq!(r.wh_per_mtok, Some(1666.7), "600 kJ = 166.67 Wh for 0.1 M tokens");
        assert_eq!(r.wh_per_mtok_incl_idle, Some(1947.9));
        assert_eq!((r.avg_watts_serving, r.avg_watts), (Some(600.0), Some(350.6)));
        assert_eq!((r.cache_hit_share, r.kv_peak, r.context_len, r.spec_accept_length), (Some(0.4), 0.1, 1_048_576.0, Some(3.0)));
        // reading speed counts what the GPUs computed: 5000 prompt - 2000 cached per 5 s step
        assert_eq!((r.prefill_tok_s, r.uptime_pct), (Some(600.0), Some(100.0)));

        // a gap (the collector was down for an hour) is not integrated over, and an engine
        // restart (counters back to zero) adds nothing
        let mut gap = LoadoutAcc::default();
        gap.observe(None, &sample(0, 1.0, 100.0, 1000.0, 150.0));
        gap.observe(Some(&sample(0, 1.0, 100.0, 1000.0, 150.0)), &sample(3600, 1.0, 100.0, 50.0, 150.0));
        assert_eq!((gap.secs, gap.joules, gap.gen_tokens, gap.samples), (0.0, 0.0, 0.0, 2));
        // nothing measured = null, not zero and not a division by zero
        let empty = row(&LoadoutIdentity::default(), &LoadoutAcc::default(), 0, false);
        assert_eq!((empty.tokens_per_joule, empty.wh_per_mtok, empty.c1_tok_s, empty.decode_p50_tok_s, empty.uptime_pct, empty.error_rate, empty.prompt_max_tokens), (None, None, None, None, None, None, None));
        assert_eq!(empty.saturation, Some(Saturation::NotEnough));
    }

    #[test]
    fn engine_counters_give_the_exact_accept_length_the_time_split_and_evictions() {
        let mk = |ts: i64, gen: f64, verify: f64, e2e: f64, prefill: f64, evicted: f64, computed: f64, retracted: f64| Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics { running: 2.0, gen_throughput: 300.0, generation_tokens_total: gen, prompt_tokens_total: computed * 50.0, spec_verify_calls_total: verify, e2e_sum: e2e, prefill_forward_sum: prefill, evicted_tokens_total: evicted, prefill_compute_tokens_total: computed, retracted, spec_accept_length: 9.9, ..Default::default() }),
            ..Default::default()
        };
        let a = mk(100, 1000.0, 400.0, 50.0, 10.0, 0.0, 8000.0, 0.0);
        let b = mk(105, 2500.0, 900.0, 60.0, 12.5, 4096.0, 28_000.0, 1.0);
        let mut acc = LoadoutAcc::default();
        acc.observe(None, &a);
        acc.observe(Some(&a), &b);
        let r = row(&LoadoutIdentity::default(), &acc, 0, true);
        assert_eq!(r.spec_accept_length, Some(3.0), "1500 tokens over 500 verify passes, not the 9.9 the gauge showed");
        assert_eq!((r.prefill_time_share, r.evicted_tokens, r.retracted_pct), (Some(0.25), 4096.0, Some(50.0)));
        assert_eq!(r.prefill_tok_s, Some(8000.0), "20 000 computed prompt tokens in the 2.5 request-seconds the engine spent reading, not in the 5 s step");
        // a sample stored by an older collector has neither the computed nor the cached counter:
        // cache hits cannot be told from reading, so reading speed is NOT MEASURED, not 80 000 tok/s
        let old = |ts: i64, prompt: f64| Sample { ts, serve_up: true, metrics: Some(ServeMetrics { running: 1.0, prompt_tokens_total: prompt, ..Default::default() }), ..Default::default() };
        let mut legacy = LoadoutAcc::default();
        legacy.observe(None, &old(0, 1_000_000.0));
        legacy.observe(Some(&old(0, 1_000_000.0)), &old(5, 1_400_000.0));
        assert_eq!((legacy.prompt_tokens, row(&LoadoutIdentity::default(), &legacy, 0, true).prefill_tok_s), (400_000.0, None));
    }

    #[test]
    fn reading_peak_long_conversations_cold_starts_and_money() {
        let mk = |ts: i64, computed: f64, prefill: f64, running: f64, ctx: f64, tok_s: f64| Sample {
            ts,
            serve_up: true,
            metrics: Some(ServeMetrics { running, gen_throughput: tok_s, prefill_compute_tokens_total: computed, prefill_forward_sum: prefill, decode_sum_seq_lens: ctx, startup_time_s: 412.0, ..Default::default() }),
            gpus: vec![GpuSample { index: 0, power_w: Some(1000.0), ..Default::default() }],
            ..Default::default()
        };
        let mut acc = LoadoutAcc::default();
        let mut prev: Option<Sample> = None;
        // minute 1: 60 000 tokens in 10 request-seconds of reading (the concurrency-inflated
        // figure #44 stopped using), 60 real wall-clock seconds (the peak now: 1000 tok/s);
        // minute 2: 30 000 in the same 60 s (500 tok/s, not the best); then quiet
        let (mut computed, mut secs) = (0.0, 100.0);
        for k in 0..40 {
            let ts = 1_000 + k * 5;
            if k > 0 && k <= 12 {
                computed += 5_000.0;
                secs += 10.0 / 12.0;
            } else if k > 12 && k <= 24 {
                computed += 2_500.0;
                secs += 10.0 / 12.0;
            }
            // one request being written the whole time, 70k tokens into its conversation
            let s = mk(ts, computed, secs, 1.0, 70_000.0, 120.0);
            acc.observe(prev.as_ref(), &s);
            prev = Some(s);
        }
        let mut r = row(&LoadoutIdentity::default(), &acc, 0, true);
        assert_eq!(r.prefill_tok_s, Some(4500.0), "90 000 tokens in 20 request-seconds of reading");
        assert_eq!(r.prefill_peak_tok_s, Some(1000.0), "the best minute, per wall-clock second - never the request-seconds figure (would be 6000, 80x-style inflation)");
        let ctx: Vec<(&str, Option<f64>, u64)> = r.long_context.iter().map(|c| (c.bucket.as_str(), c.tok_s, c.samples)).collect();
        assert_eq!(ctx, [("0", None, 0), ("16k", None, 0), ("64k", Some(120.0), 40), ("128k", None, 0), ("250k", None, 0)]);
        assert_eq!((ctx_bucket(0.0), ctx_bucket(20_000.0), ctx_bucket(130_000.0), ctx_bucket(900_000.0)), ("0", "16k", "128k", "250k"));
        assert_eq!(r.engine_startup_s, Some(412.0));
        // cold starts: the newest, the mean, and only the last ten are kept
        assert_eq!((r.cold_start_s, r.cold_starts_seen), (None, 0));
        for s in [300.0, 0.0, f64::NAN, 420.0] {
            acc.note_cold_start(s);
        }
        let r2 = row(&LoadoutIdentity::default(), &acc, 0, true);
        assert_eq!((r2.cold_start_s, r2.cold_start_avg_s, r2.cold_starts_seen), (Some(420.0), Some(360.0), 2));
        for _ in 0..20 {
            acc.note_cold_start(100.0);
        }
        assert_eq!(acc.cold_starts.len(), COLD_STARTS_KEPT);
        // money: 1000 W all day = 24 kWh; nothing without a price
        assert_eq!((r.kwh_per_day, r.usd_per_day, r.usd_per_mtok), (Some(24.0), None, None));
        r.wh_per_mtok = Some(177.0);
        r.wh_per_mtok_incl_idle = Some(400.0);
        r.apply_price(None);
        assert_eq!(r.usd_per_mtok, None, "no price configured = no cost shown");
        r.apply_price(Some(0.30));
        assert_eq!((r.usd_per_mtok, r.usd_per_mtok_incl_idle, r.usd_per_day), (Some(0.0531), Some(0.12), Some(7.2)));
    }

    #[test]
    fn accumulation_survives_a_collector_restart_and_a_serve_restart() {
        let id = identity("model-a", "img:1", &strs(&["python3", "--tp-size", "4"]), &[]);
        // two users writing; their prompts come from the prefix cache (a step in which a real
        // amount of prompt is READ stays out of the concurrency curve)
        let samples: Vec<Sample> = (0..120)
            .map(|k| {
                let mut s = sample(5_000 + k * 5, 2.0, 380.0, (k * 1900) as f64, 200.0);
                if let Some(m) = s.metrics.as_mut() {
                    m.cached_tokens_total = m.prompt_tokens_total;
                }
                s
            })
            .collect();
        // one collector run over everything
        let mut whole = LoadoutAcc::default();
        whole.note_run(4_990);
        for (i, s) in samples.iter().enumerate() {
            whole.observe(i.checked_sub(1).map(|p| &samples[p]), s);
        }
        // the same, with the collector restarted in the middle: the state goes through its
        // stored JSON form and the first sample after the restart has no `prev`
        let mut first = LoadoutAcc::default();
        first.note_run(4_990);
        for (i, s) in samples[..60].iter().enumerate() {
            first.observe(i.checked_sub(1).map(|p| &samples[p]), s);
        }
        let stored = serde_json::to_string(&first).unwrap();
        let mut second: LoadoutAcc = serde_json::from_str(&stored).unwrap();
        assert_eq!(second, first);
        second.note_run(4_990); // the same container run seen again: not another run
        for (i, s) in samples[60..].iter().enumerate() {
            second.observe(i.checked_sub(1).map(|p| &samples[60 + p]), s);
        }
        assert_eq!((second.samples, second.runs), (whole.samples, 1));
        // the first sample after the collector restart has no previous sample, so it cannot be
        // told whether a prompt was being read: it stays out of the concurrency curve
        assert_eq!((second.curve.cells[1].samples + 1, second.curve.cells[0].samples), (whole.curve.cells[1].samples, whole.curve.cells[0].samples));
        // exactly one 5 s interval (the restart itself) is not integrated
        assert_eq!(whole.secs - second.secs, 5.0);
        assert_eq!(whole.gen_tokens - second.gen_tokens, 1900.0);
        let (a, b) = (row(&id, &whole, 5_000, true), row(&id, &second, 5_000, true));
        assert_eq!((a.id.as_str(), a.peak_tok_s, a.peak_at_running), (b.id.as_str(), Some(380.0), Some(2)));
        assert_eq!(a.tokens_per_joule, b.tokens_per_joule);
        // the SERVE restarts with the same configuration: the same loadout, one more run, and
        // the engine's counters starting again from zero add nothing negative
        second.note_run(9_000);
        let after: Vec<Sample> = (0..10).map(|k| sample(9_100 + k * 5, 2.0, 380.0, (k * 1900) as f64, 200.0)).collect();
        let tokens_before = second.gen_tokens;
        for (i, s) in after.iter().enumerate() {
            second.observe(if i == 0 { samples.last() } else { Some(&after[i - 1]) }, s);
        }
        assert_eq!((second.runs, second.last_started_at), (2, 9_000));
        assert_eq!(second.gen_tokens - tokens_before, 9.0 * 1900.0, "the step across the restart is a gap, the rest accumulates");
        // an older stored state (fewer fields) still loads
        let old: LoadoutAcc = serde_json::from_str("{\"samples\":3,\"joules\":10.5}").unwrap();
        assert_eq!((old.samples, old.joules, old.curve.rows(8).len()), (3, 10.5, 8));
    }

    #[test]
    fn probes_windows_and_the_gate_log_feed_the_scorecard() {
        let mut acc = LoadoutAcc::default();
        let ok = ProbeRecord { ts: 1, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(190.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
        let mut bad = ok.clone();
        bad.decode_tok_s = Some(90.0);
        bad.invalidate("contended", "x".into());
        acc.observe_probe(&ok);
        acc.observe_probe(&ProbeRecord { decode_tok_s: Some(210.0), ..ok.clone() });
        acc.observe_probe(&bad);
        let itl = HistAccum { le: vec![0.004, 0.008, 0.016], counts: vec![0.0, 900.0, 100.0, 0.0], sum: 6.0, count: 1000.0 };
        let sizes = HistAccum { le: vec![1000.0, 10_000.0], counts: vec![5.0, 5.0, 0.0], sum: 30_000.0, count: 10.0 };
        let mut accs: [HistAccum; 6] = Default::default();
        accs[2] = itl;
        accs[4] = sizes;
        acc.observe_window(&ClosedWindow { res: 60, ts: 0, accs: accs.clone() });
        acc.observe_window(&ClosedWindow { res: 600, ts: 0, accs }); // the 10-minute copy must not double it
        let r = row(&LoadoutIdentity::default(), &acc, 0, false);
        assert_eq!((r.prompt_max_tokens, r.prompt_max_is_bucket_edge), (None, false), "10 requests, 2 of them our own probes: too few to say how long real prompts get");
        let mut busy = acc.clone();
        busy.prompt_len.count = 500.0;
        let r = row(&LoadoutIdentity::default(), &busy, 0, false);
        assert_eq!((r.prompt_max_tokens, r.prompt_max_is_bucket_edge), (Some(10_000.0), true), "without the gate's number: the edge of the highest occupied bucket");
        let (log, _) = crate::gatelog::ingest(include_str!("../../../fixtures/gate_log_v52.txt"), None);
        acc.observe(None, &Sample { ts: 5, serve_up: true, log, ..Default::default() });
        let r = row(&LoadoutIdentity::default(), &acc, 0, false);
        assert_eq!((r.c1_tok_s, r.c1_best_tok_s, r.c1_probes), (Some(200.0), Some(210.0), 2), "an invalid probe is not a measurement");
        // median token gap 6.22 ms -> 160.7 tok/s; the p90 token gap is 8 ms -> 125 tok/s
        assert_eq!((r.decode_p50_tok_s, r.decode_p90_tok_s), (Some(160.7), Some(125.0)));
        assert_eq!(r.avg_prompt_tokens, Some(3000.0));
        assert_eq!((r.gate_requests, r.rate_429, r.error_rate), (6, Some(0.1667), Some(0.0)));
        assert!(r.prompt_max_tokens.unwrap() >= 128_000.0 && !r.prompt_max_is_bucket_edge, "{:?}", r.prompt_max_tokens);
        let by: Vec<(&str, u64, Option<f64>)> = r.ttft_by_size.iter().map(|s| (s.bucket.as_str(), s.n, s.avg_ms)).collect();
        assert_eq!(by, vec![("<1k", 1, Some(190.0)), ("1-8k", 2, Some(800.0)), ("8-32k", 0, None), ("32-128k", 1, Some(9800.0)), ("128k+", 1, Some(71_000.0))]);
    }

    /// #37, 2026-09-20: c1_tok_s is the MEDIAN of the recent readings, not the mean - one buffered
    /// answer (fast because it was served from cache, or just a lucky idle moment) must not move
    /// "what one user alone gets". The outlier here is a real, VALID, non-burst probe (nothing for
    /// `observe_probe`'s own reading check to reject): this test is about the statistic picking it
    /// out, not about a validity rule catching it first. Proved to have teeth by hand: flipping
    /// `row()`'s `c1_tok_s` back to `ratio(acc.c1_sum, acc.c1_probes as f64)` fails this assertion
    /// (it publishes 1152.0, the mean, not 190.0).
    #[test]
    fn c1_tok_s_is_the_median_one_buffered_reading_does_not_move_it() {
        let mut acc = LoadoutAcc::default();
        let ok = ProbeRecord { ts: 1, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(190.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None, unverified: false };
        for v in [190.0, 189.0, 191.0, 190.0, 5000.0] {
            acc.observe_probe(&ProbeRecord { decode_tok_s: Some(v), ..ok.clone() });
        }
        let r = row(&LoadoutIdentity::default(), &acc, 0, false);
        assert_eq!(r.c1_probes, 5, "all five are valid readings: the outlier is not a burst, contention or slow-TTFT case");
        assert_eq!(r.c1_tok_s, Some(190.0), "median of [189,190,190,191,5000] is 190 - the mean (1152.0) must never be what the owner sees as \"one user alone\"");
        assert_eq!(r.c1_best_tok_s, Some(5000.0), "best is still the max: a genuine fast reading is worth keeping, just not as the headline number");
    }
}
