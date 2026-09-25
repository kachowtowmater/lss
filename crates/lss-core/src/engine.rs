//! ENGINE ADAPTERS: whatever serves the model on this machine, behind one trait.
//!
//! An adapter knows three things about its engine: how to recognise it (`detect`), what it is
//! serving (`identity`) and how to read its numbers (`scrape`). Every number is an `Option`:
//! an engine that does not publish something leaves it `None`, the screen then says
//! `n/a (not reported by <engine>)`, and nothing ever shows a made-up zero.
//!
//! No I/O in here: an adapter is handed a `Fetch` (GET a path, get the body), so every parser is
//! tested against real fixture text (`fixtures/engines/`). The metric names are recorded, with
//! the URL each was confirmed from, in `docs/ENGINES.md`.

use crate::hist::{extract_histogram_named, extract_histogram_where, extract_latency, HistSet, HIST_METRICS};
use crate::prom::{self, Series, ServeMetrics};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Sglang,
    Vllm,
    LlamaCpp,
    Ollama,
    LmStudio,
    Tgi,
    /// anything that answers `/v1/models`: numbers come from lss's own probe
    OpenAi,
}

impl EngineKind {
    pub const ALL: [EngineKind; 7] = [EngineKind::Sglang, EngineKind::Vllm, EngineKind::Tgi, EngineKind::LlamaCpp, EngineKind::Ollama, EngineKind::LmStudio, EngineKind::OpenAi];

    /// the config / JSON name
    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Sglang => "sglang",
            EngineKind::Vllm => "vllm",
            EngineKind::LlamaCpp => "llamacpp",
            EngineKind::Ollama => "ollama",
            EngineKind::LmStudio => "lmstudio",
            EngineKind::Tgi => "tgi",
            EngineKind::OpenAi => "openai",
        }
    }

    /// what people call it
    pub fn label(self) -> &'static str {
        match self {
            EngineKind::Sglang => "SGLang",
            EngineKind::Vllm => "vLLM",
            EngineKind::LlamaCpp => "llama.cpp",
            EngineKind::Ollama => "Ollama",
            EngineKind::LmStudio => "LM Studio",
            EngineKind::Tgi => "TGI",
            EngineKind::OpenAi => "this server",
        }
    }

    pub fn parse(text: &str) -> Option<EngineKind> {
        let t = text.trim().to_ascii_lowercase().replace(['.', '-', '_', ' '], "");
        Some(match t.as_str() {
            "sglang" => EngineKind::Sglang,
            "vllm" => EngineKind::Vllm,
            "llamacpp" | "llamaserver" | "llama" => EngineKind::LlamaCpp,
            "ollama" => EngineKind::Ollama,
            "lmstudio" => EngineKind::LmStudio,
            "tgi" | "textgenerationinference" => EngineKind::Tgi,
            "openai" | "openaicompatible" | "generic" => EngineKind::OpenAi,
            _ => return None,
        })
    }

    /// the port the engine listens on when nobody chose another
    pub fn default_ports(self) -> &'static [u16] {
        match self {
            EngineKind::Sglang => &[30000],
            EngineKind::Vllm => &[8000],
            EngineKind::LlamaCpp => &[8080],
            EngineKind::Ollama => &[11434],
            EngineKind::LmStudio => &[1234],
            EngineKind::Tgi => &[3000, 80],
            EngineKind::OpenAi => &[],
        }
    }

    /// Does the engine answer the OpenAI chat API (what the probe and the mini bench speak)?
    /// All of them do; Ollama and TGI under `/v1` next to their native API.
    pub fn openai_chat_path(self) -> &'static str {
        "/v1/chat/completions"
    }
}

/// The numbers every engine is asked for. `None` = the engine does not report it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineMetrics {
    pub running: Option<f64>,
    pub queued: Option<f64>,
    /// 0..=1
    pub kv_usage: Option<f64>,
    pub kv_capacity_tokens: Option<f64>,
    /// tokens per second RIGHT NOW as the engine itself reports it (most only publish counters:
    /// the collector then works the speed out from two scrapes)
    pub decode_tok_s: Option<f64>,
    /// all prompt tokens, cache hits included
    pub prompt_tokens_total: Option<f64>,
    /// prompt tokens that came from the prefix cache
    pub cached_tokens_total: Option<f64>,
    /// prompt tokens the hardware really read
    pub computed_prompt_tokens_total: Option<f64>,
    pub generation_tokens_total: Option<f64>,
    pub requests_total: Option<f64>,
    /// 0..=1 over the engine's life
    pub cache_hit_rate: Option<f64>,
    pub spec_accept_length: Option<f64>,
    pub spec_accept_rate: Option<f64>,
    /// requests pushed out of the batch to make room (vLLM preemptions, SGLang retractions)
    pub preemptions_total: Option<f64>,
    /// seconds of request time spent reading prompts / in total (the time split)
    pub prefill_seconds_total: Option<f64>,
    pub request_seconds_total: Option<f64>,
    pub context_len: Option<f64>,
    pub slots: Option<f64>,
}

/// `(key, what the screen calls it)` for every field: the keys travel in `/status`
/// (`serve.not_reported`) so the screen can say WHICH numbers an engine does not publish.
pub const METRIC_FIELDS: [(&str, &str); 16] = [
    ("running", "requests running"),
    ("queued", "requests queued"),
    ("kv_usage", "memory (KV) in use"),
    ("kv_capacity", "memory (KV) capacity"),
    ("decode_tok_s", "writing speed"),
    ("prefill_tok_s", "reading speed"),
    ("tokens", "token counters"),
    // card #180 gate 3: Ollama and llama.cpp publish no finished-request counter, and the
    // stored series drew requests-per-minute as a flat 0 while real requests were served
    ("requests", "request counter"),
    ("cache_hit", "prefix-cache hits"),
    ("spec", "speculative decoding"),
    ("ttft", "time to first word"),
    ("itl", "time between tokens"),
    ("e2e", "request duration"),
    ("queue_time", "time waiting for a slot"),
    ("slots", "how many requests run at once"),
    // card #279: WITHOUT this key, an engine that publishes no preemption counter gets 0 from
    // `to_serve_metrics`, and a rate built on that 0 publishes "0 preempted per hour" - which
    // reads as "nothing was pushed out", the exact opposite of "we cannot tell". The same
    // silent-zero shape as #180 gate 3's flat-0 request series, one field over.
    ("preemptions", "requests pushed out to make room"),
];

impl EngineMetrics {
    /// From SGLang's rich metrics (everything it publishes is reported).
    pub fn from_sglang(m: &ServeMetrics) -> EngineMetrics {
        EngineMetrics {
            running: Some(m.running),
            queued: Some(m.queue),
            kv_usage: Some(m.kv_usage()),
            kv_capacity_tokens: Some(m.max_total_tokens),
            decode_tok_s: Some(m.gen_throughput),
            prompt_tokens_total: Some(m.prompt_tokens_total),
            cached_tokens_total: Some(m.cached_tokens_total),
            computed_prompt_tokens_total: Some(m.prefill_compute_tokens_total),
            generation_tokens_total: Some(m.generation_tokens_total),
            requests_total: Some(m.requests_total),
            cache_hit_rate: Some(m.cache_hit_rate),
            spec_accept_length: Some(m.spec_accept_length),
            spec_accept_rate: Some(m.spec_accept_rate),
            preemptions_total: Some(m.retracted),
            prefill_seconds_total: Some(m.prefill_forward_sum),
            request_seconds_total: Some(m.e2e_sum),
            context_len: Some(m.context_len),
            slots: None,
        }
    }

    /// Into the struct the rest of lss works with. A number that is not reported becomes 0
    /// THERE, which is why `not_reported` travels next to it: the screen asks that first.
    pub fn to_serve_metrics(&self) -> ServeMetrics {
        let v = |x: Option<f64>| x.filter(|n| n.is_finite()).unwrap_or(0.0);
        ServeMetrics {
            running: v(self.running),
            queue: v(self.queued),
            token_usage: v(self.kv_usage),
            max_total_tokens: v(self.kv_capacity_tokens),
            gen_throughput: v(self.decode_tok_s),
            prompt_tokens_total: v(self.prompt_tokens_total),
            cached_tokens_total: v(self.cached_tokens_total),
            prefill_compute_tokens_total: v(self.computed_prompt_tokens_total),
            generation_tokens_total: v(self.generation_tokens_total),
            requests_total: v(self.requests_total),
            cache_hit_rate: v(self.cache_hit_rate),
            spec_accept_length: v(self.spec_accept_length),
            spec_accept_rate: v(self.spec_accept_rate),
            retracted: v(self.preemptions_total),
            prefill_forward_sum: v(self.prefill_seconds_total),
            e2e_sum: v(self.request_seconds_total),
            context_len: v(self.context_len),
            counters_v: prom::COUNTERS_V,
            ..Default::default()
        }
    }

    /// The keys of `METRIC_FIELDS` this scrape has no number for.
    pub fn not_reported(&self, hists: Option<&HistSet>) -> Vec<String> {
        let hist = |name: &str| HIST_METRICS.iter().position(|(n, _)| *n == name).and_then(|i| hists.and_then(|h| h[i].as_ref())).is_some();
        let has = |key: &str| match key {
            "running" => self.running.is_some(),
            "queued" => self.queued.is_some(),
            "kv_usage" => self.kv_usage.is_some(),
            "kv_capacity" => self.kv_capacity_tokens.is_some(),
            // a generation counter is enough: the collector turns two scrapes into a speed
            "decode_tok_s" => self.decode_tok_s.is_some() || self.generation_tokens_total.is_some(),
            "prefill_tok_s" => self.computed_prompt_tokens_total.is_some() || (self.prompt_tokens_total.is_some() && self.cached_tokens_total.is_some()),
            "tokens" => self.generation_tokens_total.is_some() && self.prompt_tokens_total.is_some(),
            "requests" => self.requests_total.is_some(),
            "cache_hit" => self.cache_hit_rate.is_some() || self.cached_tokens_total.is_some(),
            "spec" => self.spec_accept_length.is_some(),
            "slots" => self.slots.is_some(),
            "preemptions" => self.preemptions_total.is_some(),
            h => hist(h),
        };
        METRIC_FIELDS.iter().map(|(k, _)| *k).filter(|k| !has(k)).map(String::from).collect()
    }
}

/// What is being served, as far as the engine says.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineIdentity {
    pub model: Option<String>,
    pub version: Option<String>,
    pub context_len: Option<f64>,
    pub slots: Option<u32>,
    /// quantisation, parameter size, file: whatever the engine volunteers, for the MODEL page
    pub detail: String,
    /// The engine process's own launch line when it is readable (`/proc/<pid>/cmdline` of the
    /// process listening on the engine port, Linux only; empty elsewhere). Card #53: two runs
    /// that differ only in launch flags the engine never publishes (llama-server's
    /// --n-gpu-layers, --flash-attn, batch size) must not collapse into one loadout - where the
    /// command line is readable it is folded into the identity hash, where it is not the
    /// loadout says so (`cmdline: unreadable`) instead of silently merging them.
    pub cmdline: String,
}

pub struct Scrape {
    /// Ok(model id) = the server answers. `Ok("(no model loaded)")` is still UP.
    pub model: Result<String, String>,
    pub metrics: Option<EngineMetrics>,
    /// SGLang only: everything it publishes (lanes by priority, stage times, ...)
    pub rich: Option<ServeMetrics>,
    pub hists: Option<HistSet>,
}

impl Scrape {
    pub fn serve_metrics(&self) -> Option<ServeMetrics> {
        self.rich.clone().or_else(|| self.metrics.as_ref().map(EngineMetrics::to_serve_metrics))
    }
    pub fn not_reported(&self) -> Vec<String> {
        self.metrics.clone().unwrap_or_default().not_reported(self.hists.as_ref())
    }
}

/// GET `path` on the server under inspection.
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<String, String>;

pub trait EngineAdapter: Send + Sync {
    fn kind(&self) -> EngineKind;
    /// Some(why) when the server behind `fetch` is this engine.
    fn detect(&self, fetch: Fetch) -> Option<String>;
    fn identity(&self, fetch: Fetch) -> EngineIdentity;
    fn scrape(&self, fetch: Fetch) -> Scrape;
}

/// Most specific first: an engine with its own fingerprint wins over "answers /v1/models".
pub fn adapters(public_priority: &str, trusted_priority: &str) -> Vec<Box<dyn EngineAdapter>> {
    vec![
        Box::new(Sglang { public_priority: public_priority.to_string(), trusted_priority: trusted_priority.to_string() }),
        Box::new(Vllm),
        Box::new(Tgi),
        Box::new(LlamaCpp),
        Box::new(Ollama),
        Box::new(LmStudio),
        Box::new(OpenAi),
    ]
}

pub fn adapter_for(kind: EngineKind, public_priority: &str, trusted_priority: &str) -> Box<dyn EngineAdapter> {
    adapters(public_priority, trusted_priority).into_iter().find(|a| a.kind() == kind).expect("every kind has an adapter")
}

/// A model this engine could serve RIGHT NOW, even with nothing loaded: what the C1 probe asks
/// for. Ollama loads on demand and reports "(no model loaded)" until the first request, so a
/// monitor that waits for a loaded model would never measure anything on it (2026-09-20).
pub fn servable_model(kind: EngineKind, fetch: Fetch) -> Option<String> {
    let loaded = adapter_for(kind, "", "").identity(fetch).model.filter(|m| m != NO_MODEL);
    if loaded.is_some() {
        return loaded;
    }
    match kind {
        EngineKind::Ollama => {
            let tags = json(fetch, "/api/tags")?;
            let first = tags["models"].as_array()?.first()?;
            first["name"].as_str().or_else(|| first["model"].as_str()).map(String::from)
        }
        _ => openai_model(fetch).ok().filter(|m| m != NO_MODEL),
    }
}

/// At most this many DISTINCT paths are asked per port: a port that is not an LLM server, or a
/// gateway that logs each miss against its caller, pays a small, fixed price.
/// (E3, 2026-09-20: the old cap only stopped a port that answered NOTHING - once one path
/// succeeded, every adapter still probed its own fallbacks, 8 distinct paths in practice,
/// which showed up as 20 requests and rejections in our own gateway's 24h user table.)
pub const MAX_DETECT_PATHS: usize = 4;

/// Which engine answers behind `fetch`, and the evidence. Exactly `MAX_DETECT_PATHS` real
/// requests are made, in a fixed priority order, `/v1/models` always first (it alone tells a
/// gateway or proxy in front of an already-found engine from a real second server, via `rank`);
/// a path past the budget behaves exactly like one the server does not answer, so an adapter
/// that needs it simply does not match, never panics or reports a false positive.
///
/// The order matters: every real engine but Ollama and LM Studio can be told apart from
/// `/v1/models` or `/metrics` alone (checked by each adapter's own `.detect()`, reusing the
/// cache), so the third and fourth requests are reserved for whichever of those two the first
/// two answers make plausible - `/api/version` then, only if IT answers, `/api/tags` (Ollama);
/// otherwise `/api/v0/models` (LM Studio). A budget spent on a fallback path no adapter ends up
/// needing (`/get_server_info`, `/props`, `/info`) is fine: those adapters have already matched
/// or failed by the time it is asked, from paths already in this fixed set.
pub fn detect(fetch: Fetch) -> Option<(EngineKind, String)> {
    let seen: std::cell::RefCell<std::collections::HashMap<String, Result<String, String>>> = Default::default();
    let asked = std::cell::Cell::new(0usize);
    let once = |path: &str| -> Result<String, String> {
        if let Some(hit) = seen.borrow().get(path) {
            return hit.clone();
        }
        if asked.get() >= MAX_DETECT_PATHS {
            return Err("lss: path budget spent for this port".to_string());
        }
        asked.set(asked.get() + 1);
        let got = fetch(path);
        seen.borrow_mut().insert(path.to_string(), got.clone());
        got
    };
    let _ = once("/v1/models");
    let _ = once("/metrics");
    // #25, 2026-09-22, found against a REAL LM Studio server (an assembled fixture never has
    // this shape): LM Studio answers EVERY path with HTTP 200, including a JSON *error* body for
    // one it does not implement - `/api/version` there is `200 {"error":"Unexpected endpoint..."}`.
    // `.is_ok()` alone (HTTP succeeded) took that as "plausibly Ollama" and spent the 4th path on
    // `/api/tags` (which LM Studio ALSO 200s with the same error shape, and Ollama::detect never
    // sees) - `/api/v0/models` was never asked, so LM Studio silently fell through to the generic
    // OpenAI adapter. This must agree with `Ollama::detect`'s own, already-correct reasoning
    // ("/api/version alone is not enough - other servers answer it"): only a REAL version string
    // earns the 4th path for Ollama's own check.
    if json(&once, "/api/version").and_then(|v| v["version"].as_str().map(String::from)).is_some() {
        let _ = once("/api/tags");
    } else {
        let _ = once("/api/v0/models");
    }
    adapters("", "").into_iter().find_map(|a| a.detect(&once).map(|why| (a.kind(), why)))
}

// ------------------------------------------------------------------ helpers

fn json(fetch: Fetch, path: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&fetch(path).ok()?).ok()
}

/// The first model of an OpenAI `/v1/models` list.
/// Just the model id from `/v1/models`, alone: a port's real engine and any proxy in front of
/// it answer this identically, which is what lets a proxy be recognised from this one cheap
/// question (card #44 D1) rather than the full multi-path `detect()`.
pub fn openai_model(fetch: Fetch) -> Result<String, String> {
    let body = fetch("/v1/models")?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "bad JSON from /v1/models".to_string())?;
    match v["data"].as_array() {
        Some(a) if a.is_empty() => Ok(NO_MODEL.to_string()),
        Some(a) => a[0]["id"].as_str().map(String::from).ok_or_else(|| "no model id in /v1/models".to_string()),
        None => Err("no model list in /v1/models".to_string()),
    }
}

pub const NO_MODEL: &str = "(no model loaded)";

/// card #331: the words for an engine that REFUSES the collector (HTTP 401/403 in `detail`, the
/// scrape's own error, e.g. "/v1/models: HTTP 401"). Anyone who connects their own keyed server
/// hits this, and "DOWN" alone sends them looking for a crash. None = not a refusal.
pub fn auth_refusal_hint(detail: &str, key_configured: bool) -> Option<String> {
    let code = ["401", "403"].into_iter().find(|c| detail.contains(&format!("HTTP {c}")))?;
    Some(if key_configured {
        format!("the engine refuses the configured API key (HTTP {code}) - check api_key in its [[engine]] block of collector.toml (`lss setup` tests it), then restart the collector")
    } else {
        format!("the engine wants an API key (HTTP {code}) - set api_key in its [[engine]] block of collector.toml (`lss setup` asks for it), then restart the collector")
    })
}

struct Prom<'a> {
    series: &'a [Series],
}

impl Prom<'_> {
    /// Sum over every label set (engines, models, finish reasons); None when the family is absent.
    fn sum(&self, name: &str) -> Option<f64> {
        let mut rows = self.series.iter().filter(|s| s.name == name && s.value.is_finite()).peekable();
        rows.peek()?;
        Some(rows.map(|s| s.value).sum::<f64>() + 0.0)
    }
    fn first_of(&self, names: &[&str]) -> Option<f64> {
        names.iter().find_map(|n| self.sum(n))
    }
    fn label_of(&self, name: &str, label: &str) -> Option<String> {
        self.series.iter().find(|s| s.name == name).and_then(|s| s.label(label)).map(String::from)
    }
}

fn ratio(part: Option<f64>, whole: Option<f64>) -> Option<f64> {
    match (part, whole) {
        (Some(p), Some(w)) if w > 0.0 => Some((p / w).clamp(0.0, 1.0)),
        _ => None,
    }
}

// ------------------------------------------------------------------ SGLang

pub struct Sglang {
    pub public_priority: String,
    pub trusted_priority: String,
}

impl EngineAdapter for Sglang {
    fn kind(&self) -> EngineKind {
        EngineKind::Sglang
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        fetch("/metrics").ok().filter(|t| t.contains("sglang:")).map(|_| "/metrics carries `sglang:` series".to_string()).or_else(|| json(fetch, "/get_server_info").filter(|v| v.get("max_running_requests").is_some()).map(|_| "/get_server_info answers (start it with --enable-metrics for the full picture)".to_string()))
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let info = json(fetch, "/get_server_info").unwrap_or_default();
        EngineIdentity {
            model: openai_model(fetch).ok(),
            version: info["version"].as_str().map(String::from),
            context_len: info["context_length"].as_f64().or_else(|| info["max_total_num_tokens"].as_f64()),
            slots: info["max_running_requests"].as_u64().map(|n| n as u32),
            detail: info["quantization"].as_str().map(|q| format!("quant {q}")).unwrap_or_default(),
            cmdline: String::new(),
        }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let model = openai_model(fetch);
        let parsed = fetch("/metrics").ok().filter(|t| t.contains("sglang:")).map(|t| prom::parse(&t));
        let rich = parsed.as_ref().map(|series| prom::extract_serve_metrics_from(series, &self.public_priority, &self.trusted_priority));
        Scrape { model, metrics: rich.as_ref().map(EngineMetrics::from_sglang), hists: parsed.as_ref().map(|s| extract_latency(s)), rich }
    }
}

// ------------------------------------------------------------------ vLLM

pub struct Vllm;

impl Vllm {
    pub fn parse_metrics(text: &str) -> (EngineMetrics, HistSet) {
        let series = prom::parse(text);
        let p = Prom { series: &series };
        // renamed twice: gpu_cache_usage_perc -> kv_cache_usage_perc (v0.9.2; old name gone in v0.12),
        // gpu_prefix_cache_* -> prefix_cache_* on the same timeline
        let hits = p.first_of(&["vllm:prefix_cache_hits_total", "vllm:gpu_prefix_cache_hits_total"]);
        let queries = p.first_of(&["vllm:prefix_cache_queries_total", "vllm:gpu_prefix_cache_queries_total"]);
        let prompt = p.sum("vllm:prompt_tokens_total");
        let cached = p.sum("vllm:prompt_tokens_cached_total").or(hits);
        let drafts = p.sum("vllm:spec_decode_num_drafts_total").filter(|d| *d > 0.0);
        let accepted = p.sum("vllm:spec_decode_num_accepted_tokens_total");
        let draft_tokens = p.sum("vllm:spec_decode_num_draft_tokens_total");
        // KV capacity = blocks x block size, from the labels of the cache-config info series
        let capacity = match (p.label_of("vllm:cache_config_info", "num_gpu_blocks").and_then(|v| v.parse::<f64>().ok()), p.label_of("vllm:cache_config_info", "block_size").and_then(|v| v.parse::<f64>().ok())) {
            (Some(blocks), Some(size)) => Some(blocks * size),
            _ => None,
        };
        let m = EngineMetrics {
            running: p.sum("vllm:num_requests_running"),
            queued: p.sum("vllm:num_requests_waiting"),
            kv_usage: p.first_of(&["vllm:kv_cache_usage_perc", "vllm:gpu_cache_usage_perc"]),
            kv_capacity_tokens: capacity,
            // V0 published a gauge; V1 only counters
            decode_tok_s: p.sum("vllm:avg_generation_throughput_toks_per_s"),
            prompt_tokens_total: prompt,
            cached_tokens_total: cached,
            computed_prompt_tokens_total: match (prompt, cached) {
                (Some(a), Some(c)) => Some((a - c).max(0.0)),
                _ => None,
            },
            generation_tokens_total: p.sum("vllm:generation_tokens_total"),
            requests_total: p.sum("vllm:request_success_total"),
            cache_hit_rate: ratio(hits, queries),
            // one verify pass emits the accepted draft tokens plus one of its own
            spec_accept_length: drafts.zip(accepted).map(|(d, a)| 1.0 + a / d),
            spec_accept_rate: ratio(accepted, draft_tokens),
            preemptions_total: p.sum("vllm:num_preemptions_total"),
            prefill_seconds_total: p.sum("vllm:request_prefill_time_seconds_sum"),
            request_seconds_total: p.sum("vllm:e2e_request_latency_seconds_sum"),
            context_len: None,
            slots: None,
        };
        let mut hists: HistSet = Default::default();
        let families: [&[&str]; 6] = [
            &["vllm:time_to_first_token_seconds"],
            &["vllm:e2e_request_latency_seconds"],
            // time_per_output_token_seconds -> inter_token_latency_seconds (v0.10.2; old name gone in v0.15)
            &["vllm:inter_token_latency_seconds", "vllm:time_per_output_token_seconds"],
            &["vllm:request_queue_time_seconds"],
            &["vllm:request_prompt_tokens"],
            &["vllm:request_generation_tokens"],
        ];
        for (i, names) in families.iter().enumerate() {
            hists[i] = names.iter().find_map(|n| extract_histogram_named(&series, n));
        }
        (m, hists)
    }
}

impl EngineAdapter for Vllm {
    fn kind(&self) -> EngineKind {
        EngineKind::Vllm
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        fetch("/metrics").ok().filter(|t| t.contains("vllm:")).map(|_| "/metrics carries `vllm:` series".to_string())
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let models = json(fetch, "/v1/models").unwrap_or_default();
        EngineIdentity {
            model: openai_model(fetch).ok(),
            version: json(fetch, "/version").and_then(|v| v["version"].as_str().map(String::from)),
            context_len: models["data"][0]["max_model_len"].as_f64(),
            slots: None,
            detail: String::new(),
            cmdline: String::new(),
        }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let model = openai_model(fetch);
        let parsed = fetch("/metrics").ok().filter(|t| t.contains("vllm:")).map(|t| Vllm::parse_metrics(&t));
        let (metrics, hists) = parsed.map_or((None, None), |(m, h)| (Some(m), Some(h)));
        Scrape { model, metrics, rich: None, hists }
    }
}

// ------------------------------------------------------------------ TGI

pub struct Tgi;

impl Tgi {
    pub fn parse_metrics(text: &str) -> (EngineMetrics, HistSet) {
        let series = prom::parse(text);
        let p = Prom { series: &series };
        // TGI's counters carry no `_total`; its token counts only exist as histogram sums
        let m = EngineMetrics {
            running: p.sum("tgi_batch_current_size"),
            queued: p.sum("tgi_queue_size"),
            prompt_tokens_total: p.sum("tgi_request_input_length_sum"),
            generation_tokens_total: p.sum("tgi_request_generated_tokens_sum"),
            requests_total: p.sum("tgi_request_success"),
            request_seconds_total: p.sum("tgi_request_duration_sum"),
            ..Default::default()
        };
        let mut hists: HistSet = Default::default();
        // no time-to-first-token histogram; the mean time per token stands in for the inter-token time
        hists[1] = extract_histogram_named(&series, "tgi_request_duration");
        hists[2] = extract_histogram_named(&series, "tgi_request_mean_time_per_token_duration");
        hists[3] = extract_histogram_named(&series, "tgi_request_queue_duration");
        hists[4] = extract_histogram_named(&series, "tgi_request_input_length");
        hists[5] = extract_histogram_named(&series, "tgi_request_generated_tokens");
        // kept for the docs and the tests: the per-batch prefill / decode split
        let _ = extract_histogram_where(&series, "tgi_batch_inference_duration", Some(("method", "prefill")));
        (m, hists)
    }
}

impl EngineAdapter for Tgi {
    fn kind(&self) -> EngineKind {
        EngineKind::Tgi
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        json(fetch, "/info").filter(|v| v["router"].as_str().is_some_and(|r| r.contains("text-generation"))).map(|_| "/info says `text-generation-router`".to_string()).or_else(|| fetch("/metrics").ok().filter(|t| t.contains("tgi_request_count") || t.contains("tgi_queue_size")).map(|_| "/metrics carries `tgi_` series".to_string()))
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let info = json(fetch, "/info").unwrap_or_default();
        EngineIdentity {
            model: info["model_id"].as_str().map(String::from),
            version: info["version"].as_str().map(String::from),
            context_len: info["max_total_tokens"].as_f64(),
            slots: info["max_concurrent_requests"].as_u64().map(|n| n as u32),
            detail: info["max_input_tokens"].as_u64().map(|n| format!("max input {n} tokens")).unwrap_or_default(),
            cmdline: String::new(),
        }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let info = json(fetch, "/info");
        let model = match &info {
            Some(v) => Ok(v["model_id"].as_str().unwrap_or(NO_MODEL).to_string()),
            None => openai_model(fetch),
        };
        let parsed = fetch("/metrics").ok().filter(|t| t.contains("tgi_")).map(|t| Tgi::parse_metrics(&t));
        let (mut metrics, hists) = parsed.map_or((None, None), |(m, h)| (Some(m), Some(h)));
        if let (Some(m), Some(v)) = (metrics.as_mut(), &info) {
            m.context_len = v["max_total_tokens"].as_f64();
            m.slots = v["max_concurrent_requests"].as_f64();
        }
        Scrape { model, metrics, rich: None, hists }
    }
}

// ------------------------------------------------------------------ llama.cpp server

pub struct LlamaCpp;

impl LlamaCpp {
    /// `/metrics` exists only when the server was started with `--metrics`.
    pub fn parse_metrics(text: &str) -> EngineMetrics {
        let series = prom::parse(text);
        let p = Prom { series: &series };
        // since 2025 `prompt_tokens_total` EXCLUDES cached tokens, which have their own counter
        let computed = p.sum("llamacpp:prompt_tokens_total");
        let cached = p.sum("llamacpp:prompt_tokens_cached_total");
        let drafts = p.sum("llamacpp:spec_decode_num_drafts_total").filter(|d| *d > 0.0);
        let accepted = p.sum("llamacpp:spec_decode_num_accepted_tokens_total");
        let prompt_s = p.sum("llamacpp:prompt_seconds_total");
        let predict_s = p.sum("llamacpp:tokens_predicted_seconds_total");
        EngineMetrics {
            running: p.sum("llamacpp:requests_processing"),
            queued: p.sum("llamacpp:requests_deferred"),
            // `kv_cache_usage_ratio` was removed from llama.cpp: only an old build still has it
            kv_usage: p.sum("llamacpp:kv_cache_usage_ratio"),
            prompt_tokens_total: computed.map(|c| c + cached.unwrap_or(0.0)),
            cached_tokens_total: cached,
            computed_prompt_tokens_total: computed,
            generation_tokens_total: p.sum("llamacpp:tokens_predicted_total"),
            cache_hit_rate: ratio(cached, computed.map(|c| c + cached.unwrap_or(0.0))),
            spec_accept_length: drafts.zip(accepted).map(|(d, a)| 1.0 + a / d),
            spec_accept_rate: ratio(accepted, p.sum("llamacpp:spec_decode_num_draft_tokens_total")),
            prefill_seconds_total: prompt_s,
            request_seconds_total: prompt_s.zip(predict_s).map(|(a, b)| a + b),
            ..Default::default()
        }
    }

    /// `/slots`: how many are busy (`is_processing`), when `/metrics` is off.
    pub fn busy_slots(slots_json: &str) -> Option<(f64, f64)> {
        let v: serde_json::Value = serde_json::from_str(slots_json).ok()?;
        let a = v.as_array()?;
        let busy = a.iter().filter(|s| s["is_processing"].as_bool().unwrap_or(false) || s["state"].as_i64().is_some_and(|n| n != 0)).count();
        Some((busy as f64, a.len() as f64))
    }
}

impl EngineAdapter for LlamaCpp {
    fn kind(&self) -> EngineKind {
        EngineKind::LlamaCpp
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        if fetch("/v1/models").ok().is_some_and(|b| b.contains("\"llamacpp\"")) {
            return Some("/v1/models says owned_by `llamacpp`".to_string());
        }
        if fetch("/metrics").ok().is_some_and(|t| t.contains("llamacpp:")) {
            return Some("/metrics carries `llamacpp:` series".to_string());
        }
        json(fetch, "/props").filter(|v| v.get("default_generation_settings").is_some()).map(|_| "/props has `default_generation_settings`".to_string())
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let props = json(fetch, "/props").unwrap_or_default();
        let file = props["model_path"].as_str().map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
        EngineIdentity {
            model: props["model_alias"].as_str().filter(|a| !a.is_empty()).map(String::from).or_else(|| openai_model(fetch).ok()).or_else(|| file.clone()),
            version: props["build_info"].as_str().map(String::from),
            context_len: props["default_generation_settings"]["n_ctx"].as_f64(),
            slots: props["total_slots"].as_u64().map(|n| n as u32),
            detail: file.unwrap_or_default(),
            cmdline: String::new(),
        }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let model = openai_model(fetch).or_else(|e| if fetch("/health").is_ok() { Ok(NO_MODEL.to_string()) } else { Err(e) });
        let mut metrics = fetch("/metrics").ok().filter(|t| t.contains("llamacpp:")).map(|t| LlamaCpp::parse_metrics(&t));
        if model.is_ok() {
            let m = metrics.get_or_insert_with(EngineMetrics::default);
            if let Some((busy, total)) = fetch("/slots").ok().and_then(|b| LlamaCpp::busy_slots(&b)) {
                m.running.get_or_insert(busy);
                m.slots = Some(total);
            }
            if let Some(props) = json(fetch, "/props") {
                m.context_len = props["default_generation_settings"]["n_ctx"].as_f64();
                m.slots = m.slots.or_else(|| props["total_slots"].as_f64());
            }
        }
        Scrape { model, metrics, rich: None, hists: None }
    }
}

// ------------------------------------------------------------------ Ollama

pub struct Ollama;

impl Ollama {
    /// `/api/ps`: what is loaded now. (name, context length, detail)
    pub fn loaded(ps_json: &str) -> Vec<(String, Option<f64>, String)> {
        let v: serde_json::Value = serde_json::from_str(ps_json).unwrap_or_default();
        v["models"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(|m| {
            let name = m["name"].as_str().or_else(|| m["model"].as_str())?.to_string();
            let d = &m["details"];
            let mut detail: Vec<String> = [d["parameter_size"].as_str(), d["quantization_level"].as_str()].into_iter().flatten().map(String::from).collect();
            if let (Some(size), Some(vram)) = (m["size"].as_f64(), m["size_vram"].as_f64()) {
                if size > 0.0 {
                    detail.push(format!("{:.0}% on the GPU", (vram / size * 100.0).clamp(0.0, 100.0)));
                }
            }
            Some((name, m["context_length"].as_f64(), detail.join(" · ")))
        }).collect()
    }

    /// Speeds from the timing fields of a non-streaming `/api/generate` or `/api/chat` answer
    /// (all durations are nanoseconds): (prompt tok/s, generation tok/s).
    pub fn speeds(response_json: &str) -> (Option<f64>, Option<f64>) {
        let v: serde_json::Value = serde_json::from_str(response_json).unwrap_or_default();
        let rate = |count: &str, nanos: &str| match (v[count].as_f64(), v[nanos].as_f64()) {
            (Some(c), Some(ns)) if ns > 0.0 => Some(c / (ns / 1e9)),
            _ => None,
        };
        (rate("prompt_eval_count", "prompt_eval_duration"), rate("eval_count", "eval_duration"))
    }
}

impl EngineAdapter for Ollama {
    fn kind(&self) -> EngineKind {
        EngineKind::Ollama
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        // `/api/version` alone is not enough (other servers answer it): `/api/tags` must list models the Ollama way
        let version = json(fetch, "/api/version").and_then(|v| v["version"].as_str().map(String::from))?;
        json(fetch, "/api/tags").filter(|v| v["models"].is_array()).map(|_| format!("/api/version says {version} and /api/tags lists models"))
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let loaded = fetch("/api/ps").map(|b| Ollama::loaded(&b)).unwrap_or_default();
        let first = loaded.first().cloned();
        EngineIdentity {
            model: first.as_ref().map(|(n, _, _)| n.clone()),
            version: json(fetch, "/api/version").and_then(|v| v["version"].as_str().map(String::from)),
            context_len: first.as_ref().and_then(|(_, c, _)| *c),
            slots: None,
            detail: first.map(|(_, _, d)| d).unwrap_or_default(),
            cmdline: String::new(),
        }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        // Ollama has no metrics endpoint at all: what is loaded comes from /api/ps, the speeds
        // from lss's own probe
        let model = fetch("/api/ps").map(|b| Ollama::loaded(&b)).map(|l| l.first().map_or_else(|| NO_MODEL.to_string(), |(n, _, _)| n.clone()));
        let metrics = model.as_ref().ok().map(|_| EngineMetrics { context_len: fetch("/api/ps").ok().and_then(|b| Ollama::loaded(&b).first().and_then(|(_, c, _)| *c)), ..Default::default() });
        Scrape { model, metrics, rich: None, hists: None }
    }
}

// ------------------------------------------------------------------ LM Studio, generic OpenAI

pub struct LmStudio;

impl LmStudio {
    /// `/api/v0/models`: the LOADED model (the OpenAI list has every downloaded one).
    pub fn loaded(models_json: &str) -> Option<(String, Option<f64>, String)> {
        let v: serde_json::Value = serde_json::from_str(models_json).ok()?;
        let m = v["data"].as_array()?.iter().find(|m| m["state"].as_str() == Some("loaded") && m["type"].as_str() != Some("embeddings"))?;
        let detail: Vec<String> = [m["arch"].as_str(), m["quantization"].as_str(), m["compatibility_type"].as_str()].into_iter().flatten().map(String::from).collect();
        Some((m["id"].as_str()?.to_string(), m["loaded_context_length"].as_f64().or_else(|| m["max_context_length"].as_f64()), detail.join(" · ")))
    }
}

impl EngineAdapter for LmStudio {
    fn kind(&self) -> EngineKind {
        EngineKind::LmStudio
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        json(fetch, "/api/v0/models").filter(|v| v["data"].is_array()).map(|_| "/api/v0/models answers (LM Studio's own API)".to_string())
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        let loaded = fetch("/api/v0/models").ok().and_then(|b| LmStudio::loaded(&b));
        EngineIdentity { model: loaded.as_ref().map(|(n, _, _)| n.clone()), version: None, context_len: loaded.as_ref().and_then(|(_, c, _)| *c), slots: None, detail: loaded.map(|(_, _, d)| d).unwrap_or_default(), cmdline: String::new() }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let body = fetch("/api/v0/models");
        let loaded = body.as_ref().ok().and_then(|b| LmStudio::loaded(b));
        let model = match (&body, &loaded) {
            (Ok(_), Some((name, _, _))) => Ok(name.clone()),
            (Ok(_), None) => Ok(NO_MODEL.to_string()),
            (Err(_), _) => openai_model(fetch),
        };
        let metrics = model.as_ref().ok().map(|_| EngineMetrics { context_len: loaded.and_then(|(_, c, _)| c), ..Default::default() });
        Scrape { model, metrics, rich: None, hists: None }
    }
}

pub struct OpenAi;

impl EngineAdapter for OpenAi {
    fn kind(&self) -> EngineKind {
        EngineKind::OpenAi
    }
    fn detect(&self, fetch: Fetch) -> Option<String> {
        openai_model(fetch).ok().map(|m| format!("/v1/models answers ({m}): an OpenAI-compatible server lss has no adapter for"))
    }
    fn identity(&self, fetch: Fetch) -> EngineIdentity {
        EngineIdentity { model: openai_model(fetch).ok(), ..Default::default() }
    }
    fn scrape(&self, fetch: Fetch) -> Scrape {
        let model = openai_model(fetch);
        let metrics = model.as_ref().ok().map(|_| EngineMetrics::default());
        Scrape { model, metrics, rich: None, hists: None }
    }
}

// ------------------------------------------------------------------ detection over a machine

/// The ports worth knocking on when nothing is configured: every engine's default, then the
/// ones people pick for OpenAI-compatible servers.
pub const COMMON_PORTS: [u16; 11] = [30000, 8000, 8080, 11434, 1234, 3000, 8090, 8001, 5000, 9000, 80];

/// Ports from `ss -ltnH` / `netstat -an` / `lsof -iTCP -sTCP:LISTEN -nP` output: every local
/// listening TCP port, whatever the tool's column layout.
pub fn listening_ports(text: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = text
        .lines()
        .filter(|l| l.contains("LISTEN"))
        .filter_map(|l| {
            // the local address is the first token that ends in `:<port>` or `.<port>` (BSD netstat)
            l.split_whitespace().find_map(|tok| {
                let tok = tok.trim_end_matches("(LISTEN)");
                let (host, port) = tok.rsplit_once(':').or_else(|| tok.rsplit_once('.'))?;
                let port: u16 = port.parse().ok()?;
                (host.is_empty() || host == "*" || host.starts_with('[') || host.contains('.') || host.contains(':') || host == "localhost").then_some(port)
            })
        })
        .filter(|p| *p >= 80)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Host ports from `docker ps --format '{{.Ports}}'`: `0.0.0.0:8090->30000/tcp, :::8090->30000/tcp`.
pub fn docker_published_ports(text: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = text.split([',', '\n']).filter_map(|part| part.split("->").next().filter(|_| part.contains("->"))).filter_map(|host| host.trim().rsplit(':').next().and_then(|p| p.parse().ok())).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// One server found on the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub kind: EngineKind,
    pub url: String,
    pub why: String,
    pub identity: EngineIdentity,
}

/// What `--detect` prints, and what a `[[engine]]` block for it looks like.
pub fn describe(found: &[Found], tried: &[u16]) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    if found.is_empty() {
        let _ = writeln!(o, "No LLM server found on this machine.");
        let _ = writeln!(o, "  tried ports: {}", tried.iter().map(u16::to_string).collect::<Vec<_>>().join(", "));
        let _ = writeln!(o, "  Start your server first, or tell lss where it is in collector.toml:");
        let _ = writeln!(o, "    [[engine]]\n    kind = \"openai\"            # sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai\n    url = \"http://127.0.0.1:8000\"");
        return o;
    }
    let _ = writeln!(o, "Found {} LLM server{} on this machine:", found.len(), if found.len() == 1 { "" } else { "s" });
    for f in found {
        let id = &f.identity;
        let _ = writeln!(o, "\n  {} at {}", f.kind.label(), f.url);
        let _ = writeln!(o, "    why      {}", f.why);
        let _ = writeln!(o, "    model    {}{}", id.model.as_deref().unwrap_or(NO_MODEL), if id.detail.is_empty() { String::new() } else { format!("  ({})", id.detail) });
        if let Some(v) = &id.version {
            let _ = writeln!(o, "    version  {v}");
        }
        if let Some(c) = id.context_len {
            let _ = writeln!(o, "    context  {c:.0} tokens{}", id.slots.map(|s| format!(" · {s} requests at once")).unwrap_or_default());
        }
        let _ = writeln!(o, "    config   [[engine]]\n             kind = \"{}\"\n             url = \"{}\"", f.kind.name(), f.url);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/engines/");
    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{DIR}{name}")).unwrap_or_else(|e| panic!("{name}: {e}"))
    }
    /// A fake server: the paths it answers, from fixture files.
    fn server(routes: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, String> {
        let routes: Vec<(String, String)> = routes.iter().map(|(p, f)| (p.to_string(), fixture(f))).collect();
        move |path: &str| routes.iter().find(|(p, _)| p == path).map(|(_, b)| b.clone()).ok_or_else(|| format!("404 {path}"))
    }
    fn hist<'a>(h: &'a HistSet, name: &str) -> Option<&'a crate::hist::HistSnapshot> {
        h[HIST_METRICS.iter().position(|(n, _)| *n == name).unwrap()].as_ref()
    }

    #[test]
    fn vllm_metrics_with_the_current_and_the_old_names() {
        // #25, 2026-09-22: a LIVE capture (replacing an assembled-from-source fixture) - a real
        // vllm/vllm-openai-cpu:latest v0.30.0, CPU-only, facebook/opt-125m, 3 real /v1/completions
        // requests through it, captured mid-idle right after. The capture, in full, so it can be
        // redone without the card: docker run --shm-size=4g vllm/vllm-openai-cpu:latest --model
        // facebook/opt-125m --dtype bfloat16 --port 8000, no host ports, no GPU.
        let (m, h) = Vllm::parse_metrics(&fixture("vllm_metrics.txt"));
        assert_eq!((m.running, m.queued, m.kv_usage), (Some(0.0), Some(0.0), Some(0.0)), "captured idle, right after the 3 requests finished");
        assert_eq!((m.prompt_tokens_total, m.cached_tokens_total, m.computed_prompt_tokens_total, m.generation_tokens_total), (Some(15.0), Some(0.0), Some(15.0), Some(63.0)));
        assert_eq!(m.requests_total, Some(3.0), "every finished_reason counts: 2 stop + 1 length");
        assert_eq!(m.cache_hit_rate, Some(0.0), "0 prefix-cache hits of 15 queries - a fresh model, nothing to hit yet");
        assert_eq!((m.spec_accept_length, m.spec_accept_rate), (None, None), "opt-125m has no speculative decoding: no spec_decode_* series at all, not a zero");
        assert_eq!((m.preemptions_total, m.decode_tok_s, m.slots), (Some(0.0), None, None), "V1 has no throughput gauge: the collector diffs the counter");
        assert_eq!(m.kv_capacity_tokens, Some(29_056.0), "cache_config_info: num_gpu_blocks=227 x block_size=128");
        for name in ["ttft", "e2e", "itl", "queue_time", "prompt_tokens", "gen_tokens"] {
            let snap = hist(&h, name).unwrap_or_else(|| panic!("{name} histogram"));
            assert!(snap.count > 0.0 && snap.cum.last() == Some(&snap.count), "{name}: +Inf equals the count");
        }
        assert_eq!(hist(&h, "ttft").unwrap().count, 3.0);
        assert_eq!(hist(&h, "itl").unwrap().count, 60.0, "60 inter-token gaps across the 3 requests' 63 generated tokens");
        assert!(m.not_reported(Some(&h)).iter().all(|k| k == "slots" || k == "spec"), "{:?}", m.not_reported(Some(&h)));
        // a server older than v0.9.2 / v0.10.2 still reads
        let old = "vllm:gpu_cache_usage_perc{model_name=\"m\"} 0.5\nvllm:gpu_prefix_cache_hits_total{model_name=\"m\"} 10\nvllm:gpu_prefix_cache_queries_total{model_name=\"m\"} 40\nvllm:time_per_output_token_seconds_bucket{le=\"0.1\",model_name=\"m\"} 7\nvllm:time_per_output_token_seconds_bucket{le=\"+Inf\",model_name=\"m\"} 9\nvllm:time_per_output_token_seconds_count{model_name=\"m\"} 9\nvllm:time_per_output_token_seconds_sum{model_name=\"m\"} 1.5\nvllm:avg_generation_throughput_toks_per_s{model_name=\"m\"} 88.5\n";
        let (m, h) = Vllm::parse_metrics(old);
        assert_eq!((m.kv_usage, m.cache_hit_rate, m.decode_tok_s, hist(&h, "itl").map(|s| s.count)), (Some(0.5), Some(0.25), Some(88.5), Some(9.0)));
    }

    #[test]
    fn llamacpp_metrics_props_and_slots() {
        let m = LlamaCpp::parse_metrics(&fixture("llamacpp_metrics.txt"));
        assert_eq!((m.running, m.queued, m.generation_tokens_total), (Some(1.0), Some(0.0), Some(20_977.0)));
        // prompt_tokens_total excludes the cached ones in current llama.cpp: lss's total is both
        assert_eq!((m.computed_prompt_tokens_total, m.cached_tokens_total, m.prompt_tokens_total), (Some(48_213.0), Some(131_072.0), Some(179_285.0)));
        assert_eq!(m.kv_usage, None, "llama.cpp removed kv_cache_usage_ratio: not reported, never 0");
        assert_eq!(m.spec_accept_length, None, "no drafts = no accept length");
        assert!((m.request_seconds_total.unwrap() - 574.389).abs() < 0.001);
        assert_eq!(LlamaCpp::busy_slots(&fixture("llamacpp_slots.json")), Some((1.0, 2.0)));
        let srv = server(&[("/v1/models", "lmstudio_v1_models.json"), ("/props", "llamacpp_props.json"), ("/slots", "llamacpp_slots.json"), ("/metrics", "llamacpp_metrics.txt"), ("/health", "llamacpp_health.json")]);
        let id = LlamaCpp.identity(&srv);
        assert_eq!((id.context_len, id.slots, id.detail.as_str()), (Some(8192.0), Some(2), "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf"));
        let s = LlamaCpp.scrape(&srv);
        let nr = s.not_reported();
        assert!(nr.contains(&"kv_usage".to_string()) && nr.contains(&"ttft".to_string()) && !nr.contains(&"running".to_string()) && !nr.contains(&"slots".to_string()) && !nr.contains(&"prefill_tok_s".to_string()), "{nr:?}");
        // started without --metrics: /slots still says how busy it is
        let bare = server(&[("/v1/models", "lmstudio_v1_models.json"), ("/props", "llamacpp_props.json"), ("/slots", "llamacpp_slots.json")]);
        let m = LlamaCpp.scrape(&bare).metrics.unwrap();
        assert_eq!((m.running, m.slots, m.generation_tokens_total), (Some(1.0), Some(2.0), None));
    }

    #[test]
    fn tgi_metrics_and_info() {
        let (m, h) = Tgi::parse_metrics(&fixture("tgi_metrics.txt"));
        assert_eq!((m.running, m.queued, m.requests_total), (Some(6.0), Some(2.0), Some(412.0)));
        assert!(m.generation_tokens_total.is_some_and(|t| t > 0.0) && m.prompt_tokens_total.is_some_and(|t| t > 0.0));
        assert!(hist(&h, "ttft").is_none() && hist(&h, "e2e").is_some() && hist(&h, "queue_time").is_some() && hist(&h, "itl").is_some());
        assert_eq!(hist(&h, "e2e").unwrap().count, 412.0);
        let srv = server(&[("/info", "tgi_info.json"), ("/metrics", "tgi_metrics.txt")]);
        let s = Tgi.scrape(&srv);
        assert_eq!(s.model.as_deref(), Ok("meta-llama/Llama-3.1-8B-Instruct"));
        let m = s.metrics.as_ref().unwrap();
        assert_eq!((m.context_len, m.slots), (Some(8192.0), Some(128.0)));
        assert!(s.not_reported().contains(&"kv_usage".to_string()) && s.not_reported().contains(&"ttft".to_string()));
    }

    /// Ollama loads a model when the first request arrives. Until then it says nothing is
    /// loaded - and the C1 probe, which waits for a model, never ran: no writing speed, ever.
    #[test]
    fn a_model_to_probe_is_found_even_when_the_engine_has_none_loaded() {
        let idle = |path: &str| match path {
            "/api/ps" => Ok("{\"models\":[]}".to_string()),
            "/api/tags" => Ok(fixture("ollama_tags.json")),
            "/api/version" => Ok(fixture("ollama_version.json")),
            _ => Err("404".to_string()),
        };
        assert_eq!(Ollama.scrape(&idle).model.as_deref(), Ok(NO_MODEL), "nothing is loaded, and that is what the screen says");
        assert!(servable_model(EngineKind::Ollama, &idle).is_some_and(|m| !m.is_empty() && m != NO_MODEL), "but there is something to probe with");
        // once something IS loaded, that is what gets probed
        let busy = server(&[("/api/ps", "ollama_ps.json"), ("/api/tags", "ollama_tags.json")]);
        assert_eq!(servable_model(EngineKind::Ollama, &busy).as_deref(), Some("mistral:latest"));
        // any OpenAI-compatible server: the first model it lists
        let generic = server(&[("/v1/models", "lmstudio_v1_models.json")]);
        assert_eq!(servable_model(EngineKind::OpenAi, &generic).as_deref(), Some("qwen2.5-0.5b-instruct"));
        let nothing = |_: &str| Err("connection refused".to_string());
        assert_eq!(servable_model(EngineKind::OpenAi, &nothing), None);
    }

    #[test]
    fn ollama_and_lm_studio_have_no_metrics_only_what_is_loaded() {
        let loaded = Ollama::loaded(&fixture("ollama_ps.json"));
        assert_eq!(loaded, vec![("mistral:latest".to_string(), Some(8192.0), "7.2B · Q4_0 · 100% on the GPU".to_string())]);
        let (read, write) = Ollama::speeds(&fixture("ollama_generate_response.json"));
        assert!(read.is_some_and(|v| v > 0.0) && write.is_some_and(|v| v > 0.0), "nanosecond durations: {read:?} {write:?}");
        let srv = server(&[("/api/version", "ollama_version.json"), ("/api/tags", "ollama_tags.json"), ("/api/ps", "ollama_ps.json")]);
        let s = Ollama.scrape(&srv);
        assert_eq!(s.model.as_deref(), Ok("mistral:latest"));
        let nr = s.not_reported();
        assert!(["running", "kv_usage", "decode_tok_s", "ttft", "tokens"].iter().all(|k| nr.contains(&k.to_string())), "everything but the identity is lss's own probe: {nr:?}");
        assert_eq!(s.serve_metrics().unwrap().running, 0.0, "0 in the old struct - which is why not_reported travels with it");
        // nothing loaded is still UP
        let idle = |path: &str| match path {
            "/api/ps" => Ok("{\"models\":[]}".to_string()),
            _ => Err("404".to_string()),
        };
        assert_eq!(Ollama.scrape(&idle).model.as_deref(), Ok(NO_MODEL));

        // LM Studio: /v1/models lists every downloaded model; its own API says which is loaded
        // #25, 2026-09-22: LIVE capture (llmster 0.0.25-1, headless, no display) - real
        // qwen2.5-0.5b-instruct loaded beside an unloaded embedding model (correctly skipped:
        // `type != "embeddings"`), loaded_context_length present (8192, the server's own default
        // for this model, not its 32768 max)
        assert_eq!(LmStudio::loaded(&fixture("lmstudio_api_v0_models.json")), Some(("qwen2.5-0.5b-instruct".to_string(), Some(8192.0), "qwen2 · Q8_0 · gguf".to_string())));
    }

    #[test]
    fn every_engine_is_recognised_by_its_own_fingerprint_and_nothing_else() {
        let sglang = |path: &str| match path {
            "/metrics" => Ok("sglang:num_running_reqs{tp_rank=\"0\"} 1\n".to_string()),
            "/v1/models" => Ok("{\"data\":[{\"id\":\"glm\"}]}".to_string()),
            _ => Err("404".to_string()),
        };
        let vllm = server(&[("/metrics", "vllm_metrics.txt"), ("/v1/models", "lmstudio_v1_models.json")]);
        let tgi = server(&[("/info", "tgi_info.json"), ("/metrics", "tgi_metrics.txt")]);
        let llama = |path: &str| match path {
            "/v1/models" => Ok("{\"object\":\"list\",\"data\":[{\"id\":\"m.gguf\",\"object\":\"model\",\"owned_by\":\"llamacpp\"}]}".to_string()),
            _ => Err("404".to_string()),
        };
        let ollama = server(&[("/api/version", "ollama_version.json"), ("/api/tags", "ollama_tags.json"), ("/api/ps", "ollama_ps.json"), ("/v1/models", "lmstudio_v1_models.json")]);
        let lmstudio = server(&[("/api/v0/models", "lmstudio_api_v0_models.json"), ("/v1/models", "lmstudio_v1_models.json")]);
        let generic = server(&[("/v1/models", "lmstudio_v1_models.json")]);
        let nothing = |_: &str| Err("connection refused".to_string());
        let kind = |f: Fetch| detect(f).map(|(k, _)| k);
        assert_eq!(kind(&sglang), Some(EngineKind::Sglang));
        assert_eq!(kind(&vllm), Some(EngineKind::Vllm));
        assert_eq!(kind(&tgi), Some(EngineKind::Tgi));
        assert_eq!(kind(&llama), Some(EngineKind::LlamaCpp));
        assert_eq!(kind(&ollama), Some(EngineKind::Ollama));
        assert_eq!(kind(&lmstudio), Some(EngineKind::LmStudio));
        assert_eq!(kind(&generic), Some(EngineKind::OpenAi));
        assert_eq!(kind(&nothing), None);
        // #25, 2026-09-22: a REAL LM Studio server (llmster 0.0.25-1, headless) answers /api/version
        // and /api/tags with 200 + a JSON *error* body, not a 404 - the exact shape that made the
        // path-budget logic waste the 4th path on Ollama's check instead of LM Studio's own
        let real_lmstudio = |path: &str| match path {
            "/v1/models" => Ok("{\"object\":\"list\",\"data\":[{\"id\":\"qwen2.5-0.5b-instruct\",\"object\":\"model\",\"owned_by\":\"organization_owner\"}]}".to_string()),
            "/api/version" | "/api/tags" => Ok("{\"error\":\"Unexpected endpoint or method. (GET /api/version)\"}".to_string()),
            "/api/v0/models" => Ok(fixture("lmstudio_api_v0_models.json")),
            _ => Err("404".to_string()),
        };
        assert_eq!(kind(&real_lmstudio), Some(EngineKind::LmStudio), "a 200-with-an-error-body must not be mistaken for Ollama's real /api/version");
        // E3, 2026-09-20: a port that is not an LLM server used to see 8 distinct paths once ONE
        // discriminator answered (our own gateway got 20 requests + rejections from a zero-config
        // scan). Now: /v1/models is always asked first, at most MAX_DETECT_PATHS distinct real
        // requests are ever made, an engine is never asked the same path twice, and a path past
        // the budget is left alone entirely - it never even reaches the underlying `fetch`.
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let closed = |path: &str| {
            asked.borrow_mut().push(path.to_string());
            Err::<String, String>("HTTP 404".to_string())
        };
        assert_eq!(detect(&closed).map(|(k, _)| k), None);
        // the exact fixed order for a port that answers nothing: /v1/models, /metrics, then
        // /api/version (fails) -> /api/v0/models (LM Studio's turn, since Ollama's did not apply)
        assert_eq!(*asked.borrow(), ["/v1/models", "/metrics", "/api/version", "/api/v0/models"]);
        asked.borrow_mut().clear();
        let counting_vllm = |path: &str| {
            asked.borrow_mut().push(path.to_string());
            vllm(path)
        };
        assert_eq!(detect(&counting_vllm).map(|(k, _)| k), Some(EngineKind::Vllm));
        assert!(asked.borrow().len() <= MAX_DETECT_PATHS, "{:?}", asked.borrow());
        let mut paths = asked.borrow().clone();
        let n = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), n, "a path was asked twice: {:?}", asked.borrow());
        // a web server that answers 200 with HTML to everything is not an LLM server
        let html = |_: &str| Ok("<html>hello</html>".to_string());
        assert_eq!(kind(&html), None);
        for k in EngineKind::ALL {
            assert_eq!(EngineKind::parse(k.name()), Some(k));
            assert_eq!(adapter_for(k, "", "").kind(), k);
        }
        assert_eq!((EngineKind::parse("llama.cpp"), EngineKind::parse("LM Studio"), EngineKind::parse("nope")), (Some(EngineKind::LlamaCpp), Some(EngineKind::LmStudio), None));
    }

    #[test]
    fn listening_ports_from_ss_netstat_and_lsof_and_docker() {
        let ss = "LISTEN 0 4096 0.0.0.0:8090 0.0.0.0:*\nLISTEN 0 128 127.0.0.1:11434 0.0.0.0:*\nLISTEN 0 4096 [::]:22 [::]:*\nLISTEN 0 511 *:8000 *:*\n";
        assert_eq!(listening_ports(ss), vec![8000, 8090, 11434]);
        let netstat = "tcp4       0      0  127.0.0.1.1234         *.*                    LISTEN\ntcp46      0      0  *.8080                 *.*                    LISTEN\ntcp4 0 0 192.0.2.10.55000 192.0.2.11.443 ESTABLISHED\n";
        assert_eq!(listening_ports(netstat), vec![1234, 8080]);
        let lsof = "ollama   812 you    3u  IPv4 0x1  0t0  TCP 127.0.0.1:11434 (LISTEN)\n";
        assert_eq!(listening_ports(lsof), vec![11434]);
        assert_eq!(docker_published_ports("0.0.0.0:8090->30000/tcp, :::8090->30000/tcp\n127.0.0.1:3000->80/tcp\n5432/tcp\n"), vec![3000, 8090]);
        let text = describe(&[Found { kind: EngineKind::Ollama, url: "http://127.0.0.1:11434".into(), why: "/api/version says 0.5.1".into(), identity: EngineIdentity { model: Some("mistral:latest".into()), context_len: Some(8192.0), ..Default::default() } }], &[]);
        assert!(text.contains("Ollama at http://127.0.0.1:11434") && text.contains("kind = \"ollama\"") && text.contains("context  8192 tokens"), "{text}");
        assert!(describe(&[], &[8000, 8080]).contains("No LLM server found") && describe(&[], &[8000]).contains("[[engine]]"));
    }

    #[test]
    fn a_refused_key_is_named_and_nothing_else_is_called_a_key_problem() {
        let none = auth_refusal_hint("/v1/models: HTTP 401", false).unwrap();
        assert!(none.contains("wants an API key (HTTP 401)") && none.contains("api_key") && none.contains("[[engine]]"), "{none}");
        let wrong = auth_refusal_hint("/v1/models: HTTP 403", true).unwrap();
        assert!(wrong.contains("refuses the configured API key (HTTP 403)"), "{wrong}");
        for other in ["/v1/models: HTTP 404", "/v1/models: HTTP 500", "/v1/models: Connection refused", "", "timeout after 4011 ms"] {
            assert_eq!(auth_refusal_hint(other, true), None, "{other}");
        }
    }
}
