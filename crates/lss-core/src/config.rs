//! Collector configuration (`~/.config/lss/collector.toml`). Every key is optional; the
//! defaults below are generic: nothing in them names a host, an address or a path of ours.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name reported in /status and as the `host` label in /metrics. Empty = this machine's
    /// host name.
    pub host: String,
    pub poll_secs: u64,
    /// The LLM server. `"auto"` = find it on this machine (`lss-collector --detect` shows how).
    /// Written `engine_url` in config files; `sglang_url` still reads (its first name).
    #[serde(alias = "engine_url")]
    pub sglang_url: String,
    /// `"auto"` = recognise the engine by its fingerprint, else one of
    /// `sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai`.
    pub engine_kind: String,
    /// `[[engine]]` blocks pin what `--detect` would find. THIS collector watches the first;
    /// a second server on the machine gets a collector of its own (see docs/ENGINES.md).
    #[serde(rename = "engine")]
    pub engines: Vec<EngineEntry>,
    /// card #297: the API key of the engine being watched, copied from the first `[[engine]]`
    /// block's `api_key` when the collector starts. Runtime only: never read from, or written
    /// to, a file under this name (the key lives in `[[engine]]`, next to the URL it belongs to).
    #[serde(skip)]
    pub engine_api_key: String,
    /// card #308: check the certificate of an `https://` engine or gateway URL (default true).
    /// `false` accepts ANY certificate - only for a box on your own network with a self-signed
    /// one, and the collector warns at every start. Better: give its CA in `tls_ca_file`.
    /// An `[[engine]]` block's own `tls_verify` wins over this one for THAT engine; `gate_url`
    /// always uses this one (card #316).
    pub tls_verify: bool,
    /// card #308: a PEM file to trust for https URLs: a CA that signs your servers, or a server's
    /// own self-signed certificate (then pinned exactly - `openssl req -x509` makes that kind).
    /// "" = the built-in Mozilla roots plus this machine's own store. "~" = $HOME.
    pub tls_ca_file: String,
    /// the gateway TRUSTED listener: /gate/health, and the C1 probe goes through it.
    /// `""` = no gateway: LANES / USERS / GATEWAY say so and the probe talks to the engine.
    pub gate_url: String,
    /// Container name, or "auto" = whichever container publishes `serve_port`.
    pub serve_container: String,
    pub serve_port: u16,
    pub gate_container: String,
    /// Max concurrent requests, shown as running/slots. 0 = ask SGLang (/get_server_info).
    pub slots: u32,
    pub public_priority: String,
    pub trusted_priority: String,
    pub listen: Vec<String>,
    /// "~" expands to $HOME.
    pub db_path: String,
    /// Raw 5 s samples, in hours.
    pub raw_hours: u32,
    /// The 1-minute tier (rollups + latency histograms), in days.
    pub retention_days: u32,
    /// The 10-minute tier, and with it incidents, alerts, probes and the gate-log rollup, in days.
    pub rollup_10m_days: u32,
    pub alert_cmd: String,
    /// Every this many seconds the collector runs `alert_cmd --flush` so mail spooled while the
    /// seat's agent was busy is retried even when no new alert comes along. 0 = never.
    pub alert_flush_secs: u64,
    /// Look trusted-lane client addresses up in `tailscale status --json` on this host
    /// (every 5 minutes). Off unless switched on; `[[user_alias]]` entries win over it.
    pub tailscale_lookup: bool,
    /// how temperatures are written in alert text and `/rules`: "both" (47°C / 117°F) | "c" | "f"
    pub temp_units: String,
    /// what a kWh costs, for "cost per 1M tokens" on the MODEL page. Unset = no cost is shown.
    pub electricity_usd_per_kwh: Option<f64>,
    /// #75, 2026-09-21: where the OWNER's real, time-of-use-aware rate table lives - see
    /// `rates.rs`'s own doc comment for why this is a file of its own, never a value in this
    /// struct or this repo.
    pub rates: RatesSection,
    /// #74, 2026-09-22: where the watch-sweep's own output file lives - see `watch.rs`'s own doc
    /// comment for why this collector never fetches it itself.
    pub watch: WatchSection,
    pub probe: ProbeConfig,
    pub rules: RulesConfig,
    pub bench: BenchConfig,
    pub advice: AdviceConfig,
    /// `[targets]`: the service levels the owner wants (docs: README "Targets")
    pub targets: crate::targets::TargetsConfig,
    /// `[[user_alias]] ip = "…" name = "…"`: friendly names for trusted-lane client addresses.
    pub user_alias: Vec<crate::users::UserAlias>,
}

/// `[rates]`: cost tracking (card #75). Deliberately just a path - the rate table itself is a
/// SEPARATE file (`rates.rs::RatesFile`), never inline here, so "never commit the real one" is a
/// property of one path a `.gitignore` can name once (`site/*/rates.toml`), not a section buried
/// inside a file that also holds ordinary, non-sensitive settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RatesSection {
    /// "~" expands to $HOME. "" = cost tracking off: no rate table, so no cost figures anywhere
    /// - the same "unset = nothing shown, never a guess" convention as `electricity_usd_per_kwh`.
    pub path: String,
}

impl Default for RatesSection {
    fn default() -> Self {
        Self { path: "~/.config/lss/rates.toml".into() }
    }
}

/// `[watch]`: card #74. A SEPARATE JSON file, written by whatever the owner runs on whatever
/// schedule they choose (`scripts/watch-check.sh` is a generic starting point) - this collector
/// only reads it back and republishes it, the exact same "path, not inline data" shape as
/// `RatesSection` above, for the same reason: the sources an owner follows are their own choice,
/// never this repo's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchSection {
    /// "" = watch tracking off: no file, no WATCH page content anywhere
    pub path: String,
}

impl Default for WatchSection {
    fn default() -> Self {
        Self { path: "~/.config/lss/watch.json".into() }
    }
}

/// One `[[engine]]` block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineEntry {
    /// `sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai`, or `auto`
    pub kind: String,
    pub url: String,
    /// what the screen calls it; "" = the engine's name
    pub name: String,
    /// card #297: an engine started with an API key (`--api-key`, `OPENAI_API_KEY`, a LAN
    /// proxy) answers every request without it with 401. When set, the collector sends
    /// `Authorization: Bearer <api_key>` - to this engine's URL ONLY, never to the gateway or
    /// anywhere else. It is a secret: `lss-collector setup` writes the file 0600, and
    /// `--check-config` prints it redacted.
    #[serde(skip_serializing_if = "String::is_empty", serialize_with = "redacted")]
    pub api_key: String,
    /// card #308: this engine's own certificate check (`https://` only); unset = the top-level
    /// `tls_verify`
    pub tls_verify: Option<bool>,
}

/// A secret, as it may be printed: its last 4 characters at most, never the whole thing.
pub fn redact_secret(secret: &str) -> String {
    let n = secret.chars().count();
    if n == 0 {
        return String::new();
    }
    if n < 12 {
        return "(set, hidden)".to_string();
    }
    let tail: String = secret.chars().skip(n - 4).collect();
    format!("(set, hidden) ...{tail}")
}

fn redacted<S: serde::Serializer>(secret: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&redact_secret(secret))
}

impl Config {
    pub fn has_gate(&self) -> bool {
        !self.gate_url.trim().is_empty()
    }
    /// Where the probe and the bench's own requests go: through the gateway when there is
    /// one (it stamps and accounts for them), else straight to the engine.
    pub fn chat_base(&self) -> &str {
        if self.has_gate() { self.gate_url.trim_end_matches('/') } else { self.sglang_url.trim_end_matches('/') }
    }
    /// The engine URL still has to be found on this machine.
    pub fn engine_is_auto(&self) -> bool {
        self.engines.is_empty() && matches!(self.sglang_url.trim(), "" | "auto")
    }
    /// The API key the first `[[engine]]` block names ("" = none).
    pub fn pinned_api_key(&self) -> String {
        self.engines.first().map(|e| e.api_key.trim().to_string()).unwrap_or_default()
    }
    /// The pinned engine, if the config names one: (kind or None for auto, url).
    pub fn pinned_engine(&self) -> Option<(Option<crate::engine::EngineKind>, String)> {
        if let Some(e) = self.engines.first() {
            return Some((crate::engine::EngineKind::parse(&e.kind), e.url.trim_end_matches('/').to_string()));
        }
        (!self.engine_is_auto()).then(|| (crate::engine::EngineKind::parse(&self.engine_kind), self.sglang_url.trim_end_matches('/').to_string()))
    }
    /// card #308: whether https certificates are checked for the engine this collector watches:
    /// the pinned `[[engine]]` block's own `tls_verify` when it has one, else the top-level key.
    pub fn engine_tls_verify(&self) -> bool {
        self.engines.first().and_then(|e| e.tls_verify).unwrap_or(self.tls_verify)
    }
    /// The port the serve container publishes: `serve_port`, else the port of the engine URL.
    pub fn serve_port_or_url(&self) -> u16 {
        if self.serve_port != 0 {
            return self.serve_port;
        }
        self.sglang_url.rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse().ok()).unwrap_or(0)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            poll_secs: 5,
            sglang_url: "auto".into(),
            engine_kind: "auto".into(),
            engines: Vec::new(),
            engine_api_key: String::new(),
            tls_verify: true,
            tls_ca_file: String::new(),
            gate_url: String::new(),
            serve_container: "auto".into(),
            serve_port: 0,
            gate_container: String::new(),
            slots: 0,
            public_priority: "10".into(),
            trusted_priority: "0".into(),
            listen: vec!["127.0.0.1:8099".into()],
            db_path: "~/.local/state/lss/lss.db".into(),
            raw_hours: 24,
            retention_days: 14,
            rollup_10m_days: 90,
            alert_cmd: "~/bin/lss-alert.sh".into(),
            alert_flush_secs: 300,
            tailscale_lookup: false,
            temp_units: "both".into(),
            electricity_usd_per_kwh: None,
            rates: RatesSection::default(),
            watch: WatchSection::default(),
            probe: ProbeConfig::default(),
            rules: RulesConfig::default(),
            bench: BenchConfig::default(),
            advice: AdviceConfig::default(),
            targets: crate::targets::TargetsConfig::default(),
            user_alias: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeConfig {
    pub enabled: bool,
    pub interval_secs: i64,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub prompt: String,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 300,
            max_tokens: 128,
            timeout_secs: 60,
            prompt: "Write a detailed explanation of how a bicycle stays upright while moving. Use at least 200 words.".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RulesConfig {
    /// Minimum gap between two alerts of the same rule.
    pub cooldown_secs: i64,
    pub serve_down_secs: i64,
    /// Escalates the serve-down alert to severity `page`.
    pub serve_down_page_secs: i64,
    pub gate_down_secs: i64,
    pub queue_reqs: f64,
    pub queue_secs: i64,
    pub waiters_secs: i64,
    pub thermal_temp_c: f64,
    pub thermal_secs: i64,
    /// GPU indices that never raise a thermal alert; they get a once-a-day digest line.
    pub thermal_exclude: Vec<u32>,
    pub gpu_missing_secs: i64,
    /// C1 alert when decode tok/s < c1_ratio x baseline on `c1_consecutive` idle probes.
    pub c1_ratio: f64,
    pub c1_consecutive: u32,
    /// Fixed C1 baseline in tok/s. Unset = median of the first `c1_baseline_probes` probes.
    pub c1_baseline_tok_s: Option<f64>,
    pub c1_baseline_probes: usize,
    /// A probe whose first token took longer than this (seconds) shared the engine with
    /// someone's prefill: it is stored as invalid (`slow_ttft`) and ignored by the C1 rule.
    pub c1_max_ttft_s: f64,
    /// #47, 2026-09-21: on a busy box the stricter probe validation (card #45) can leave C1
    /// without a single VALID reading for hours - an `info` alert says so once the newest valid
    /// reading is older than this many probe intervals, so the silence is never read as health.
    pub c1_stale_probe_intervals: u32,
    pub reject_growth: u64,
    pub reject_window_secs: i64,
    /// Seconds a restarted container must stay up before the "recovered" message.
    pub restart_stable_secs: i64,
    /// #36, 2026-09-21: omp's shared config on THIS box, watched (never written - re-pointing it
    /// is the operator's own job) for `modelRoles.default` naming a model this
    /// provider no longer serves. "~" expands to $HOME. "" = do not watch it (a box with no
    /// omp installed, or where this check is not wanted) - never an error, just silent.
    pub omp_config_path: String,
    /// A mismatch must hold this long before it alerts - the few seconds a swap takes to land
    /// in both the served model and (once run) the sync script must never look like a fault.
    pub omp_mismatch_hold_secs: i64,
    /// #108, 2026-09-22: how long the gate's charge path must be failing before it alerts.
    /// Short on purpose - an inert discount costs our own agents 429s every minute it lasts,
    /// and it is never transient: the exception is structural.
    pub charge_errors_secs: i64,
    /// #108: how long "warm cache, traffic, zero discounts" must hold before it alerts. Longer
    /// than the exception form, because it is inferred from a window rather than observed.
    pub discount_inert_secs: i64,
    /// #108: how many recent admissions the gate must have seen before "0 discounted" means
    /// anything - a quiet box must never read as broken.
    pub discount_min_admissions: u64,
    /// card #78: how long "trusted in-flight > the council ceiling AND cache_hit_rate < 0.9"
    /// must hold before it warns - the state that turns a big budget from harmless to lethal
    /// (both Xid-8 hangs happened with real pending prefill in the millions).
    pub cold_budget_secs: i64,
    /// card #78: the in-flight ceiling the composite rule judges against (the council's 1M).
    pub cold_budget_inflight: u64,
    /// card #78: the cache hit rate below which the budget's tokens are "real" prefill.
    pub cold_budget_hit_rate: f64,
    /// card #264: the filesystem the collector watches on its own box (statvfs). "" = do not
    /// watch. On 2026-09-23 the serve box's / filled to 100% with nothing raising an alert.
    pub disk_path: String,
    /// card #264: `warn` at this % used, `page` at the second - each held `disk_secs`, and
    /// recovered only once it drops `disk_recover_margin_pct` points below its own line.
    pub disk_warn_pct: f64,
    pub disk_page_pct: f64,
    pub disk_secs: i64,
    pub disk_recover_margin_pct: f64,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 1800,
            serve_down_secs: 120,
            serve_down_page_secs: 900,
            gate_down_secs: 120,
            queue_reqs: 24.0,
            queue_secs: 120,
            waiters_secs: 300,
            thermal_temp_c: 90.0,
            thermal_secs: 600,
            thermal_exclude: Vec::new(),
            gpu_missing_secs: 60,
            c1_ratio: 0.8,
            c1_consecutive: 3,
            c1_baseline_tok_s: None,
            c1_baseline_probes: 12,
            c1_max_ttft_s: 3.0,
            c1_stale_probe_intervals: 6,
            reject_growth: 20,
            reject_window_secs: 600,
            restart_stable_secs: 120,
            omp_config_path: "~/.omp/agent/config.yml".into(),
            omp_mismatch_hold_secs: 600,
            charge_errors_secs: 60,
            discount_inert_secs: 600,
            discount_min_admissions: 50,
            // card #78: the 2026-09-22 state that prompted it - 2,956,712 of 3,000,000 in
            // flight (98.6%) with token_usage 0.0. Sustained 120 s so a cold-cache blip that
            // self-corrects never pages.
            cold_budget_secs: 120,
            cold_budget_inflight: 1_000_000,
            cold_budget_hit_rate: 0.9,
            disk_path: "/".into(),
            disk_warn_pct: 90.0,
            disk_page_pct: 97.0,
            disk_secs: 60,
            disk_recover_margin_pct: 2.0,
        }
    }
}

/// `[bench]`: how `lss bench` runs the benchmark harness. The harness is NOT part of this
/// project: `harness` must point at a checkout of `llm_decode_bench.py`
/// (github.com/local-inference-lab/llm-inference-bench).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchConfig {
    /// path of `llm_decode_bench.py`. Empty = not configured: `lss bench` says so and stops.
    pub harness: String,
    /// the interpreter that has the harness's dependencies (httpx, rich)
    pub python: String,
    /// argv put in FRONT of the interpreter, e.g. `["systemd-run", "--user", "--scope",
    /// "--quiet", "--collect"]` so the harness does not run inside the collector's memory-capped
    /// cgroup (a throttled client measures the throttle, not the model)
    pub launcher: Vec<String>,
    /// where the raw harness JSON of every run is kept
    pub results_dir: String,
    /// the server must have been idle this long before a bench may start (without --force)
    pub idle_secs: i64,
    /// the safety poll while a bench runs
    pub poll_secs: u64,
    /// seconds per decode cell
    pub quick_duration: u32,
    pub full_duration: u32,
    /// hard stop per profile, seconds
    pub quick_timeout_secs: u64,
    pub full_timeout_secs: u64,
    pub accuracy_timeout_secs: u64,
    /// size of the long-context retrieval test of the `full` profile, in tokens
    pub needle_tokens: u64,
    /// raw result directories kept on disk (the scorecards are kept in the database regardless)
    pub keep_runs: u32,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            harness: String::new(),
            python: "python3".into(),
            launcher: Vec::new(),
            results_dir: "~/.local/state/lss/bench".into(),
            idle_secs: 300,
            poll_secs: 2,
            quick_duration: 10,
            full_duration: 30,
            quick_timeout_secs: 900,
            full_timeout_secs: 3600,
            accuracy_timeout_secs: 21_600,
            needle_tokens: 250_000,
            keep_runs: 50,
        }
    }
}

/// `[advice]`: the thresholds of the ADVICE rules (docs/ADVICE.md has every rule).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdviceConfig {
    /// % of the time with every slot busy: watch / act
    pub slots_full_watch_pct: f64,
    pub slots_full_act_pct: f64,
    /// % of the time with requests waiting in the queue: watch / act
    pub queue_watch_pct: f64,
    pub queue_act_pct: f64,
    /// KV memory: below `kv_low_pct` at the peak = room for more users; above = watch / act
    pub kv_low_pct: f64,
    pub kv_watch_pct: f64,
    pub kv_act_pct: f64,
    /// prefix-cache hit share below this = watch
    pub cache_low_pct: f64,
    /// remembered text (KV) thrown out to make room, tokens per hour: above = `watch`
    pub evictions_watch_per_hour: f64,
    /// speculative accept length below this = the drafter is not paying for itself
    pub spec_low_accept: f64,
    /// accept length falling by more than this share from 1 user to the busiest level = watch
    pub spec_drop_pct: f64,
    /// the longest real prompt as a % of the configured context: below = context is oversized
    pub context_unused_pct: f64,
    /// rejections (429 / 413) per day: watch / act
    pub rejects_watch_per_day: f64,
    pub rejects_act_per_day: f64,
    /// week-over-week growth of tokens per day: watch above this %
    pub growth_watch_pct: f64,
    /// % of the time a GPU was throttled: watch / act
    pub throttle_watch_pct: f64,
    pub throttle_act_pct: f64,
    /// share of the request time spent reading prompts above this = prefill-bound
    pub prefill_heavy_pct: f64,
}

impl Default for AdviceConfig {
    fn default() -> Self {
        Self {
            slots_full_watch_pct: 5.0,
            slots_full_act_pct: 20.0,
            queue_watch_pct: 2.0,
            queue_act_pct: 10.0,
            kv_low_pct: 30.0,
            kv_watch_pct: 80.0,
            kv_act_pct: 95.0,
            cache_low_pct: 20.0,
            evictions_watch_per_hour: 1_000_000.0,
            spec_low_accept: 1.5,
            spec_drop_pct: 25.0,
            context_unused_pct: 50.0,
            rejects_watch_per_day: 5.0,
            rejects_act_per_day: 50.0,
            growth_watch_pct: 50.0,
            throttle_watch_pct: 1.0,
            throttle_act_pct: 10.0,
            prefill_heavy_pct: 60.0,
        }
    }
}

/// `~/.config/lss/lss.toml`: what the `lss` CLI / screen needs to know. Everything is optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// the collector (`$LSS_URL` and `--url` win over it). Empty = http://127.0.0.1:8099
    pub url: String,
    /// `dark` | `light`: the theme of a first start (the screen remembers `T` after that)
    pub theme: String,
    /// `lss bench` runs on the GPU box. From another machine: the ssh host to run it on.
    /// Empty = print the ssh command instead of running it.
    pub bench_ssh: String,
    /// "both" (47°C / 117°F, the default, also when empty) | "c" | "f"
    pub temp_units: String,
    /// `[[server]] name = "…" url = "…"`: several servers. The screen starts on the first one,
    /// `[` / `]` switch, and a FLEET strip shows them all. None = the single `url` above.
    pub server: Vec<ServerEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerEntry {
    pub name: String,
    /// that server's collector, e.g. `http://gpu-box:8099`
    pub url: String,
    /// the ssh host `lss bench` runs on for this server (else the top-level `bench_ssh`)
    pub bench_ssh: String,
}

/// The servers the screen knows: (name, collector URL). `--url` / `$LSS_URL` mean "this one
/// server, whatever the file says". Entries without a URL are skipped; a missing name is the
/// URL's host. A single-server setup gives exactly one entry and nothing extra is drawn.
pub fn servers(flag: Option<&str>, env: Option<&str>, cfg: &ClientConfig) -> Vec<(String, String)> {
    let clean = |u: &str| u.trim().trim_end_matches('/').to_string();
    let host_of = |u: &str| u.split("://").nth(1).unwrap_or(u).split(['/', ':']).next().unwrap_or("").to_string();
    let explicit = [flag, env].into_iter().flatten().map(str::trim).find(|u| !u.is_empty());
    let listed: Vec<(String, String)> = cfg.server.iter().filter(|s| !s.url.trim().is_empty()).map(|s| (if s.name.trim().is_empty() { host_of(&s.url) } else { s.name.trim().to_string() }, clean(&s.url))).collect();
    match explicit {
        Some(u) => {
            // a listed server keeps its name when it is the one asked for
            let url = clean(u);
            vec![listed.into_iter().find(|(_, l)| *l == url).unwrap_or_else(|| (host_of(&url), url))]
        }
        None if listed.is_empty() => {
            let url = resolve_url(None, None, cfg);
            vec![(host_of(&url), url)]
        }
        None => listed,
    }
}

/// `--server NAME|N` picks one of `servers` (1-based number, or a name, case-insensitive).
pub fn pick_server(servers: &[(String, String)], sel: &str) -> Option<usize> {
    let sel = sel.trim();
    sel.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < servers.len()).or_else(|| servers.iter().position(|(n, _)| n.eq_ignore_ascii_case(sel)))
}

pub const DEFAULT_COLLECTOR_URL: &str = "http://127.0.0.1:8099";

pub fn parse_client_config(text: &str) -> Result<ClientConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Which collector to talk to: `--url`, then `$LSS_URL`, then `lss.toml`, then this machine.
pub fn resolve_url(flag: Option<&str>, env: Option<&str>, cfg: &ClientConfig) -> String {
    [flag, env, Some(cfg.url.as_str())].into_iter().flatten().map(str::trim).find(|u| !u.is_empty()).unwrap_or(DEFAULT_COLLECTOR_URL).trim_end_matches('/').to_string()
}

pub fn parse_config(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Expands a leading `~` using the given home directory.
pub fn expand_home(path: &str, home: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", home.trim_end_matches('/'), rest),
        None if path == "~" => home.to_string(),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_all_defaults() {
        let c = parse_config("").unwrap();
        assert_eq!(c, Config::default());
        assert!(c.rules.thermal_exclude.is_empty(), "no GPU is special in a generic install");
        assert!(c.host.is_empty() && c.listen == vec!["127.0.0.1:8099"] && !c.tailscale_lookup && c.user_alias.is_empty(), "nothing site-specific in the defaults");
        assert_eq!(c.rules.c1_ratio, 0.8, "0.8 is the one default: README, RUNBOOK, the example file and the TUI follow it");
        assert_eq!(c.alert_flush_secs, 300);
    }

    #[test]
    fn partial_override_and_typo_detection() {
        let c = parse_config("poll_secs = 10\n[rules]\nthermal_exclude = [0, 2]\nc1_baseline_tok_s = 190.0\n[[user_alias]]\nip = \"192.0.2.76\"\nname = \"laptop\"\n[[user_alias]]\nip = \"192.0.2.77\"\nname = \"seat\"\n").unwrap();
        assert_eq!((c.user_alias.len(), c.user_alias[1].name.as_str()), (2, "seat"));
        assert_eq!(c.poll_secs, 10);
        assert_eq!(c.rules.thermal_exclude, vec![0, 2]);
        assert_eq!(c.rules.c1_baseline_tok_s, Some(190.0));
        assert_eq!(c.rules.cooldown_secs, 1800);
        assert!(parse_config("pol_secs = 10").is_err(), "unknown keys are rejected, not ignored");
    }

    #[test]
    fn the_shipped_example_config_parses_to_the_defaults() {
        let c = parse_config(include_str!("../../../packaging/collector.toml.example")).unwrap();
        assert_eq!(c, Config::default(), "packaging/collector.toml.example must document the real defaults");
    }

    #[test]
    fn the_client_config_picks_the_collector_flag_then_env_then_file_then_this_machine() {
        let none = ClientConfig::default();
        assert_eq!(resolve_url(None, None, &none), "http://127.0.0.1:8099", "a generic install talks to itself");
        let file = parse_client_config("url = \"http://gpu-box:8099/\"\ntheme = \"dark\"\nbench_ssh = \"gpu-box\"\n").unwrap();
        assert_eq!((resolve_url(None, None, &file).as_str(), file.bench_ssh.as_str()), ("http://gpu-box:8099", "gpu-box"));
        assert_eq!(resolve_url(None, Some("http://env:1"), &file), "http://env:1");
        assert_eq!(resolve_url(Some("http://flag:2"), Some("http://env:1"), &file), "http://flag:2");
        assert_eq!(resolve_url(None, Some("  "), &file), "http://gpu-box:8099", "an empty $LSS_URL is not a URL");
        assert!(parse_client_config("ur = 1").is_err());
        assert_eq!(parse_client_config(include_str!("../../../packaging/lss.toml.example")).unwrap(), ClientConfig::default(), "packaging/lss.toml.example documents the real defaults");
    }

    #[test]
    fn several_servers_and_a_single_server_setup_behaves_as_before() {
        // nothing configured: this machine, one entry
        assert_eq!(servers(None, None, &ClientConfig::default()), vec![("127.0.0.1".to_string(), "http://127.0.0.1:8099".to_string())]);
        // the classic single `url`
        let one = parse_client_config("url = \"http://gpu-box:8099/\"\n").unwrap();
        assert_eq!(servers(None, None, &one), vec![("gpu-box".to_string(), "http://gpu-box:8099".to_string())]);
        // [[server]] entries: in file order, the first is where the screen starts
        let many = parse_client_config("url = \"http://ignored:1\"\n[[server]]\nname = \"big\"\nurl = \"http://192.0.2.10:8099\"\nbench_ssh = \"big\"\n[[server]]\nurl = \"http://spark-1:8099/\"\n[[server]]\nname = \"no url\"\n").unwrap();
        let list = servers(None, None, &many);
        assert_eq!(list, vec![("big".to_string(), "http://192.0.2.10:8099".to_string()), ("spark-1".to_string(), "http://spark-1:8099".to_string())], "an entry without a URL is skipped; a missing name is the host");
        // --url / $LSS_URL = exactly that server, and it keeps its configured name when listed
        assert_eq!(servers(Some("http://192.0.2.10:8099/"), None, &many), vec![("big".to_string(), "http://192.0.2.10:8099".to_string())]);
        assert_eq!(servers(None, Some("http://elsewhere:9"), &many), vec![("elsewhere".to_string(), "http://elsewhere:9".to_string())]);
        assert_eq!((pick_server(&list, "2"), pick_server(&list, "BIG"), pick_server(&list, "0"), pick_server(&list, "3"), pick_server(&list, "nope")), (Some(1), Some(0), None, None, None));
        assert!(parse_client_config("[[server]]\nnam = \"x\"\n").is_err(), "a typo inside [[server]] is an error too");
        assert_eq!(parse_client_config("temp_units = \"f\"\n").unwrap().temp_units, "f");
    }

    #[test]
    fn bench_and_advice_have_generic_defaults() {
        let c = Config::default();
        assert!(c.bench.harness.is_empty() && c.bench.launcher.is_empty(), "no path of ours in the code");
        assert_eq!((c.bench.idle_secs, c.bench.poll_secs), (300, 2));
        let c = parse_config("[bench]\nharness = \"~/bench/llm_decode_bench.py\"\nlauncher = [\"systemd-run\", \"--user\", \"--scope\"]\n[advice]\nkv_low_pct = 25.0\n").unwrap();
        assert_eq!((c.bench.launcher.len(), c.advice.kv_low_pct, c.advice.kv_act_pct), (3, 25.0, 95.0));
        assert!(parse_config("[bench]\nharnes = \"x\"").is_err());
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("~/bin/x", "/home/m"), "/home/m/bin/x");
        assert_eq!(expand_home("/abs", "/home/m"), "/abs");
    }
}
