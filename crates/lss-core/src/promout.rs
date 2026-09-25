//! Prometheus text rendering of the collector's own `/metrics`, derived from the same
//! `Status` document `/status` serves, so the two can never disagree.

use crate::model::Status;
use std::fmt::Write;

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

struct Out {
    buf: String,
    host: String,
}

impl Out {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.buf, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }
    fn sample(&mut self, name: &str, labels: &[(&str, String)], value: f64) {
        let mut l = format!("host=\"{}\"", self.host);
        for (k, v) in labels {
            let _ = write!(l, ",{k}=\"{}\"", esc(v));
        }
        let _ = writeln!(self.buf, "{name}{{{l}}} {value}");
    }
    fn gauge(&mut self, name: &str, help: &str, value: f64) {
        self.family(name, "gauge", help);
        self.sample(name, &[], value);
    }
}

pub fn render(s: &Status) -> String {
    let mut o = Out { buf: String::new(), host: esc(&s.host) };
    let b = |v: bool| if v { 1.0 } else { 0.0 };

    o.gauge("llm_serve_up", "1 when the serve answers /v1/models", b(s.serve.up));
    o.family("llm_serve_info", "gauge", "Served model id and container");
    o.sample("llm_serve_info", &[("model", s.serve.model.clone().unwrap_or_default()), ("container", s.serve.container.clone().unwrap_or_default())], 1.0);
    o.gauge("llm_serve_restart_count", "docker RestartCount of the serve container", s.serve.restart_count as f64);
    o.gauge("llm_serve_restarts_today", "Serve container restarts since local midnight", f64::from(s.serve.restarts_today));
    o.gauge("llm_serve_uptime_seconds", "Seconds since the serve container started (0 when down)", s.serve.uptime_s.unwrap_or(0) as f64);

    // The C1 series are ABSENT until a probe has succeeded: a 0 would read as "decoding at 0 tok/s".
    // `last_ok` is the last VALID probe: one that collided with real traffic never moves these,
    // they keep the last valid value and the age gauge says how old that is.
    if let Some(p) = &s.probe.last_ok {
        if let Some(v) = p.decode_tok_s {
            o.gauge("llm_serve_c1_decode_tokens_per_second", "Single-stream decode rate from the last VALID idle C1 probe", v);
        }
        if let Some(v) = p.ttft_ms {
            o.gauge("llm_serve_c1_ttft_seconds", "Time to first token of the last VALID idle C1 probe", v / 1000.0);
        }
        o.gauge("llm_serve_c1_probe_timestamp_seconds", "Unix time of the last VALID C1 probe", p.ts as f64);
        o.gauge("llm_serve_c1_probe_age_seconds", "Seconds since the last VALID C1 probe (grows while probes are skipped or invalid)", (s.generated_at - p.ts).max(0) as f64);
        o.gauge("llm_serve_c1_probes_invalid_skipped", "Probes since the last valid one that were skipped, invalid or failed", f64::from(s.probe.invalid_skipped));
    }
    if let Some(v) = s.probe.baseline_tok_s {
        o.gauge("llm_serve_c1_baseline_tokens_per_second", "C1 baseline the alert rule compares against", v);
    }

    o.gauge("llm_serve_decode_tokens_per_second", "SGLang gen_throughput", s.serve.decode_tok_s);
    o.gauge("llm_serve_kv_usage_ratio", "KV cache utilisation 0..1", s.serve.kv_usage);
    o.gauge("llm_serve_slots", "Max concurrent requests", f64::from(s.serve.slots));
    o.family("llm_serve_running_requests", "gauge", "Requests being decoded/prefilled");
    o.sample("llm_serve_running_requests", &[("lane", "total".into())], s.serve.running);
    o.sample("llm_serve_running_requests", &[("lane", "public".into())], s.lanes.public.running);
    o.sample("llm_serve_running_requests", &[("lane", "trusted".into())], s.lanes.trusted.running);
    o.family("llm_serve_queued_requests", "gauge", "Requests waiting in the engine queue");
    o.sample("llm_serve_queued_requests", &[("lane", "total".into())], s.serve.queue);
    o.sample("llm_serve_queued_requests", &[("lane", "public".into())], s.lanes.public.queued);
    o.sample("llm_serve_queued_requests", &[("lane", "trusted".into())], s.lanes.trusted.queued);

    o.gauge("llm_gate_up", "1 when the gateway answers /gate/health", b(s.gate.up));
    o.family("llm_gate_waiters", "gauge", "Requests waiting at the gate for token budget");
    o.family("llm_gate_inflight_tokens", "gauge", "Estimated prompt tokens admitted and in flight");
    o.family("llm_gate_rejected_total", "counter", "Gate admission rejections since the gate started");
    for (lane, l) in [("public", &s.lanes.public), ("trusted", &s.lanes.trusted)] {
        o.sample("llm_gate_waiters", &[("lane", lane.into())], l.waiters as f64);
        o.sample("llm_gate_inflight_tokens", &[("lane", lane.into())], l.inflight_tokens as f64);
        o.sample("llm_gate_rejected_total", &[("lane", lane.into()), ("code", "413".into())], l.rejected_413 as f64);
        o.sample("llm_gate_rejected_total", &[("lane", lane.into()), ("code", "429".into())], l.rejected_429 as f64);
    }

    o.family("llm_gpu_temperature_celsius", "gauge", "GPU core temperature");
    o.family("llm_gpu_power_watts", "gauge", "GPU power draw");
    o.family("llm_gpu_utilization_percent", "gauge", "GPU utilisation");
    o.family("llm_gpu_memory_used_mib", "gauge", "GPU memory in use");
    o.family("llm_gpu_throttle_mask", "gauge", "clocks_throttle_reasons.active bitmask");
    o.family("llm_gpu_thermal_throttled", "gauge", "1 when a thermal slowdown bit (0x20|0x40) is set");
    for g in &s.gpus {
        let l = [("gpu", g.sample.index.to_string())];
        if let Some(v) = g.sample.temp_c {
            o.sample("llm_gpu_temperature_celsius", &l, v);
        }
        if let Some(v) = g.sample.power_w {
            o.sample("llm_gpu_power_watts", &l, v);
        }
        if let Some(v) = g.sample.util_pct {
            o.sample("llm_gpu_utilization_percent", &l, v);
        }
        if let Some(v) = g.sample.mem_used_mib {
            o.sample("llm_gpu_memory_used_mib", &l, v);
        }
        o.sample("llm_gpu_throttle_mask", &l, g.sample.throttle_mask as f64);
        o.sample("llm_gpu_thermal_throttled", &l, b(g.sample.thermal_throttled()));
    }

    o.gauge("lss_alerts_firing", "Alert rules currently firing", s.firing.len() as f64);
    o.gauge("lss_incidents_open", "Incidents without an end time", s.incidents.iter().filter(|i| i.end.is_none()).count() as f64);
    o.gauge("lss_last_sample_timestamp_seconds", "Unix time of the newest sample", s.collector.last_sample_ts.unwrap_or(0) as f64);
    o.buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prom::parse;

    #[test]
    fn renders_parseable_text_with_the_contract_series() {
        let status: Status = serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).unwrap();
        let text = render(&status);
        let series = parse(&text);
        let data_lines = text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).count();
        assert_eq!(series.len(), data_lines, "our own output must survive our own parser");
        let get = |name: &str| series.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(get("llm_serve_c1_decode_tokens_per_second").value, 191.7);
        assert_eq!(get("llm_serve_c1_decode_tokens_per_second").label("host"), Some("gpu-box"));
        // the fixture's NEWEST probe (60 s old, 141.4 tok/s) collided with traffic: the series
        // keep the last VALID value, and the age gauge says that one is 120 s old
        assert_eq!(get("llm_serve_c1_probe_age_seconds").value, 120.0);
        assert_eq!(get("llm_serve_c1_probes_invalid_skipped").value, 1.0);
        assert!((get("llm_serve_c1_ttft_seconds").value - 0.0924).abs() < 1e-9);
        assert_eq!(get("llm_serve_up").value, 1.0);
        assert_eq!(get("llm_serve_restart_count").value, 0.0);
        assert_eq!(series.iter().filter(|s| s.name == "llm_gpu_temperature_celsius").count(), 4);
    }

    #[test]
    fn c1_series_absent_until_a_probe_succeeds() {
        let text = render(&Status { host: "h".into(), ..Default::default() });
        assert!(!text.contains("llm_serve_c1_decode_tokens_per_second"));
        assert!(text.contains("llm_serve_up{host=\"h\"} 0"));
    }
}
