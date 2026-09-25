//! Prometheus text-format parsing and the SGLang metric extraction.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Series {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Parse the exposition format. Comment lines, blank lines and anything malformed are
/// skipped: a bad line must never take the collector down.
pub fn parse(text: &str) -> Vec<Series> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Series> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let i = line.find(['{', ' '])?;
    let (name, rest) = (&line[..i], &line[i..]);
    if name.is_empty() {
        return None;
    }
    let (labels, value_part) = if let Some(body) = rest.strip_prefix('{') {
        let (labels, consumed) = parse_labels(body)?;
        (labels, &body[consumed..])
    } else {
        (Vec::new(), rest)
    };
    // value, optionally followed by a timestamp
    let value = value_part.split_whitespace().next()?;
    let value = match value {
        "NaN" => f64::NAN,
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        v => v.parse().ok()?,
    };
    Some(Series { name: name.to_string(), labels, value })
}

/// Parses `k="v",k2="v2"}`; returns the labels and how many bytes were consumed, including
/// the closing brace.
fn parse_labels(body: &str) -> Option<(Vec<(String, String)>, usize)> {
    let bytes = body.as_bytes();
    let mut labels = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ') {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'}' {
            return Some((labels, i + 1));
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        let key = body.get(key_start..i)?.trim().to_string();
        i += 1; // '='
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        let mut val = String::new();
        loop {
            let c = *bytes.get(i)?;
            match c {
                b'\\' => {
                    match *bytes.get(i + 1)? {
                        b'n' => val.push('\n'),
                        b'"' => val.push('"'),
                        b'\\' => val.push('\\'),
                        other => {
                            val.push('\\');
                            val.push(other as char);
                        }
                    }
                    i += 2;
                }
                b'"' => {
                    i += 1;
                    break;
                }
                _ => {
                    // copy one full UTF-8 scalar
                    let ch = body.get(i..)?.chars().next()?;
                    val.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        labels.push((key, val));
    }
}

/// The subset of SGLang's `/metrics` the monitor uses. Gauges are read from `tp_rank="0"`
/// only; counters that carry no `tp_rank` label are summed across their label sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeMetrics {
    pub running: f64,
    pub queue: f64,
    pub running_public: f64,
    pub running_trusted: f64,
    pub queue_public: f64,
    pub queue_trusted: f64,
    pub gen_throughput: f64,
    pub token_usage: f64,
    pub full_token_usage: f64,
    pub kv_used_tokens: f64,
    pub max_total_tokens: f64,
    pub cache_hit_rate: f64,
    pub spec_accept_length: f64,
    pub spec_accept_rate: f64,
    pub retracted: f64,
    pub prompt_tokens_total: f64,
    pub generation_tokens_total: f64,
    pub requests_total: f64,
    pub ttft_sum: f64,
    pub ttft_count: f64,
    pub itl_sum: f64,
    pub itl_count: f64,
    pub queue_time_sum: f64,
    pub queue_time_count: f64,
    /// prompt tokens served from the prefix cache (`cached_tokens_total`, all sources)
    pub cached_tokens_total: f64,
    /// the configured context length (`context_len`)
    pub context_len: f64,
    /// prompt tokens the GPUs actually computed (`realtime_tokens_total{mode="prefill_compute"}`):
    /// what "reading speed" is measured on; a prefix-cache hit costs nothing and is not in here
    pub prefill_compute_tokens_total: f64,
    /// KV tokens thrown out of the prefix cache to make room (`evicted_tokens_total`)
    pub evicted_tokens_total: f64,
    /// speculative-decoding verify passes (`spec_verify_calls_total`): generated tokens divided
    /// by this is the EXACT accept length over any window
    pub spec_verify_calls_total: f64,
    /// request-seconds end to end, and of those in the prefill forward pass (time split)
    pub e2e_sum: f64,
    pub prefill_forward_sum: f64,
    /// sum of the context lengths of the requests being written right now (`decode_sum_seq_lens`):
    /// with one request running it IS that conversation's length
    pub decode_sum_seq_lens: f64,
    /// requests whose prompt is being read right now (`num_prefill_inflight_queue_reqs`)
    pub prefill_inflight_reqs: f64,
    /// how long the engine took to start, seconds (`startup_time_seconds`; 0 = not published)
    pub startup_time_s: f64,
    /// which set of COUNTER fields this sample carries (`COUNTERS_V` when it was taken; 0 = a
    /// sample stored before this existed). Samples are stored and diffed later, and a counter
    /// that an older binary did not record reads back as 0: diffing across that would book the
    /// counter's whole life as one step's growth. See `comparable`.
    pub counters_v: u32,
}

/// Bump when a counter (`*_total`, `*_sum`) is added to `ServeMetrics`.
pub const COUNTERS_V: u32 = 2;

impl ServeMetrics {
    /// Prompt tokens the GPUs really READ between `before` and now. The engine's own count when
    /// it has one, else prompt minus cached when it reports the cache. With NEITHER a cache hit
    /// cannot be told from real reading, so nothing is counted: "not measured" is honest, 80 000
    /// tok/s of cache hits is not. A counter that went backwards (engine restart) counts nothing.
    /// Counters of two samples may be diffed only when both carry the same set of them.
    pub fn comparable(&self, before: &ServeMetrics) -> bool {
        self.counters_v == before.counters_v
    }

    pub fn computed_prompt_tokens_since(&self, before: &ServeMetrics) -> f64 {
        if !self.comparable(before) {
            return 0.0;
        }
        let grown = |now: f64, was: f64| if now >= was { now - was } else { 0.0 };
        if self.prefill_compute_tokens_total > 0.0 || before.prefill_compute_tokens_total > 0.0 {
            grown(self.prefill_compute_tokens_total, before.prefill_compute_tokens_total)
        } else if self.cached_tokens_total > 0.0 || before.cached_tokens_total > 0.0 {
            (grown(self.prompt_tokens_total, before.prompt_tokens_total) - grown(self.cached_tokens_total, before.cached_tokens_total)).max(0.0)
        } else {
            0.0
        }
    }

    /// KV-cache utilisation in 0..=1. `token_usage` is authoritative; fall back to the token
    /// counts when the engine reports 0 there but has tokens resident.
    pub fn kv_usage(&self) -> f64 {
        let reported = self.token_usage.max(self.full_token_usage);
        if reported > 0.0 || self.max_total_tokens <= 0.0 {
            reported
        } else {
            (self.kv_used_tokens / self.max_total_tokens).clamp(0.0, 1.0)
        }
    }
}

pub struct Extractor<'a> {
    series: &'a [Series],
    prefix: &'a str,
}

impl<'a> Extractor<'a> {
    pub fn new(series: &'a [Series]) -> Self {
        Self { series, prefix: "sglang:" }
    }

    fn matching(&self, name: &str) -> impl Iterator<Item = &'a Series> + '_ {
        let full = format!("{}{}", self.prefix, name);
        self.series.iter().filter(move |s| {
            s.name == full && s.label("tp_rank").is_none_or(|r| r == "0") && s.value.is_finite()
        })
    }

    /// Whole-engine value. If the engine publishes an explicit total (`priority=""`) use
    /// that; otherwise sum the per-priority series (the counters have no total row).
    pub fn total(&self, name: &str) -> f64 {
        let all: Vec<&Series> = self.matching(name).collect();
        let has_explicit_total = all.iter().any(|s| s.label("priority") == Some(""));
        // `+ 0.0`: the sum of nothing is -0.0 in Rust, and "-0" must never reach a screen or a document
        all.iter()
            .filter(|s| !has_explicit_total || s.label("priority") == Some(""))
            .map(|s| s.value)
            .sum::<f64>()
            + 0.0
    }

    /// Whole-engine value of the series carrying `label="value"` (`mode="decode"`, `stage="…"`).
    pub fn labelled(&self, name: &str, label: &str, value: &str) -> f64 {
        let all: Vec<&Series> = self.matching(name).filter(|s| s.label(label) == Some(value)).collect();
        let has_explicit_total = all.iter().any(|s| s.label("priority") == Some(""));
        all.iter().filter(|s| !has_explicit_total || s.label("priority") == Some("")).map(|s| s.value).sum::<f64>() + 0.0
    }

    /// Value for one priority lane.
    pub fn lane(&self, name: &str, priority: &str) -> f64 {
        self.matching(name).filter(|s| s.label("priority") == Some(priority)).map(|s| s.value).sum::<f64>() + 0.0
    }
}

/// `public_priority` / `trusted_priority` are the label values the gate stamps ("10" / "0").
pub fn extract_serve_metrics(text: &str, public_priority: &str, trusted_priority: &str) -> ServeMetrics {
    extract_serve_metrics_from(&parse(text), public_priority, trusted_priority)
}

/// Same, from a scrape that is already parsed (the collector also reads the histograms from it).
pub fn extract_serve_metrics_from(series: &[Series], public_priority: &str, trusted_priority: &str) -> ServeMetrics {
    let x = Extractor::new(series);
    ServeMetrics {
        running: x.total("num_running_reqs"),
        queue: x.total("num_queue_reqs"),
        running_public: x.lane("num_running_reqs", public_priority),
        running_trusted: x.lane("num_running_reqs", trusted_priority),
        queue_public: x.lane("num_queue_reqs", public_priority),
        queue_trusted: x.lane("num_queue_reqs", trusted_priority),
        gen_throughput: x.total("gen_throughput"),
        token_usage: x.total("token_usage"),
        full_token_usage: x.total("full_token_usage"),
        kv_used_tokens: x.total("kv_used_tokens"),
        max_total_tokens: x.total("max_total_num_tokens"),
        cache_hit_rate: x.total("cache_hit_rate"),
        spec_accept_length: x.total("spec_accept_length"),
        spec_accept_rate: x.total("spec_accept_rate"),
        retracted: x.total("num_retracted_reqs"),
        prompt_tokens_total: x.total("prompt_tokens_total"),
        generation_tokens_total: x.total("generation_tokens_total"),
        requests_total: x.total("num_requests_total"),
        ttft_sum: x.total("time_to_first_token_seconds_sum"),
        ttft_count: x.total("time_to_first_token_seconds_count"),
        itl_sum: x.total("inter_token_latency_seconds_sum"),
        itl_count: x.total("inter_token_latency_seconds_count"),
        queue_time_sum: x.total("queue_time_seconds_sum"),
        queue_time_count: x.total("queue_time_seconds_count"),
        cached_tokens_total: x.total("cached_tokens_total"),
        context_len: x.total("context_len"),
        prefill_compute_tokens_total: x.labelled("realtime_tokens_total", "mode", "prefill_compute"),
        evicted_tokens_total: x.total("evicted_tokens_total"),
        spec_verify_calls_total: x.total("spec_verify_calls_total"),
        e2e_sum: x.total("e2e_request_latency_seconds_sum"),
        prefill_forward_sum: x.labelled("per_stage_req_latency_seconds_sum", "stage", "prefill_forward"),
        decode_sum_seq_lens: x.total("decode_sum_seq_lens"),
        prefill_inflight_reqs: x.total("num_prefill_inflight_queue_reqs"),
        startup_time_s: x.total("startup_time_seconds"),
        counters_v: COUNTERS_V,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = include_str!("../../../fixtures/sglang_metrics.txt");

    #[test]
    fn parses_label_escapes_and_specials() {
        let s = parse("a{x=\"q\\\"uote\",y=\"b\\\\s\"} 1.5\nb 2\nc{z=\"1\"} NaN\n# HELP a x\nbroken{ 3\n");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].label("x"), Some("q\"uote"));
        assert_eq!(s[0].label("y"), Some("b\\s"));
        assert_eq!(s[1].value, 2.0);
        assert!(s[2].value.is_nan());
    }

    #[test]
    fn real_fixture_parses_fully() {
        let series = parse(REAL);
        let data_lines = REAL.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).count();
        assert_eq!(series.len(), data_lines, "every data line of the real scrape must parse");
        assert!(series.len() > 1000);
    }

    #[test]
    fn real_fixture_extracts_expected_values() {
        let m = extract_serve_metrics(REAL, "10", "0");
        // tp_rank="0" only: max_total_num_tokens appears on 4 ranks, must not be summed.
        assert_eq!(m.max_total_tokens, 3_788_160.0);
        assert_eq!(m.running, 0.0);
        assert_eq!(m.queue, 0.0);
        assert_eq!(m.spec_accept_length, 3.1);
        assert_eq!(m.spec_accept_rate, 0.7);
        // counters have no total row: summed across priority + is_streaming label sets
        assert_eq!(m.prompt_tokens_total, 506_827.0 + 195.0 + 15.0);
        assert_eq!(m.generation_tokens_total, 909.0 + 114.0 + 14.0);
        assert_eq!(m.requests_total, 13.0);
        assert_eq!(m.ttft_count, 13.0);
        assert_eq!(m.itl_count, 527.0);
        // queue_time has priority="" on all four ranks: rank 0 only
        assert_eq!(m.queue_time_count, 14.0);
        assert!((m.queue_time_sum - 29.547_549_765).abs() < 1e-6);
        assert_eq!(m.kv_usage(), 0.0);
    }

    #[test]
    fn lanes_split_by_priority_label() {
        let text = "sglang:num_running_reqs{priority=\"\",tp_rank=\"0\"} 5\n\
                    sglang:num_running_reqs{priority=\"0\",tp_rank=\"0\"} 3\n\
                    sglang:num_running_reqs{priority=\"10\",tp_rank=\"0\"} 2\n\
                    sglang:num_running_reqs{priority=\"10\",tp_rank=\"1\"} 99\n";
        let m = extract_serve_metrics(text, "10", "0");
        assert_eq!((m.running, m.running_public, m.running_trusted), (5.0, 2.0, 3.0));
    }

    #[test]
    fn kv_usage_falls_back_to_token_counts() {
        let m = ServeMetrics { kv_used_tokens: 500.0, max_total_tokens: 1000.0, ..Default::default() };
        assert_eq!(m.kv_usage(), 0.5);
    }
}
