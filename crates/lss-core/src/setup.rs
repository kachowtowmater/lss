//! card #297: the pure half of the setup wizard (`lss-collector setup`, `lss setup`,
//! install.sh). Everything here is text in, text out - no network, no files, no clock - so the
//! parts a stranger actually reads (what an address means, why a connection failed, what gets
//! written into their config) are unit-tested exactly as they will be shown.

use crate::engine::EngineKind;

/// Turn what a person types into the engine's base URL.
///
/// Accepts `8000`, `:8000`, `localhost:8000`, `192.0.2.20:8000`, `gpu-box:11434`,
/// `http://host:port`, `https://host:port` (card #311: the collector speaks TLS since #308), a
/// trailing `/` or `/v1` (people paste their OpenAI "base URL"), and surrounding blanks. No
/// scheme = http. Refuses, with a reason a person can act on: an empty answer, spaces inside,
/// a port that is not 1-65535, and any other scheme.
pub fn normalize_url(input: &str) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("type an address, e.g. localhost:8000 or 192.0.2.20:8000".into());
    }
    if t.chars().any(char::is_whitespace) {
        return Err(format!("'{t}' has a space in it - an address is one word, e.g. 192.0.2.20:8000"));
    }
    let lower = t.to_ascii_lowercase();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("https://") {
        ("https", &t[t.len() - r.len()..])
    } else if let Some(r) = lower.strip_prefix("http://") {
        ("http", &t[t.len() - r.len()..])
    } else if let Some((scheme, _)) = t.split_once("://") {
        return Err(format!("'{scheme}://' is not an address lss can talk to - use http://host:port or https://host:port"));
    } else {
        ("http", t)
    };
    // cut the path: "/", "/v1", "/v1/" (a pasted OpenAI base URL) - any other path is kept, a
    // server mounted under a prefix is real
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    let path = path.strip_suffix("/v1").unwrap_or(path);
    let path = if path == "/v1" { "" } else { path };
    let hostport = hostport.trim_start_matches(':');
    let (host, port) = if hostport.chars().all(|c| c.is_ascii_digit()) {
        ("127.0.0.1", Some(hostport)) // just a port: this machine
    } else if let Some(inner) = hostport.strip_prefix('[') {
        // an IPv6 literal, [::1]:8000
        let (h, after) = inner.split_once(']').ok_or_else(|| format!("'{hostport}': an IPv6 address needs its closing ]"))?;
        let port = after.strip_prefix(':');
        if !after.is_empty() && port.is_none() {
            return Err(format!("'{hostport}': expected :port after the ]"));
        }
        return finish(scheme, &format!("[{h}]"), port, path);
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    finish(scheme, host, port, path)
}

fn finish(scheme: &str, host: &str, port: Option<&str>, path: &str) -> Result<String, String> {
    if host.is_empty() {
        return Err("the address has no host name - e.g. localhost:8000".into());
    }
    if !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':')) {
        return Err(format!("'{host}' is not a host name or IP address"));
    }
    let host = if host.eq_ignore_ascii_case("localhost") { "127.0.0.1" } else { host };
    match port {
        None => Ok(format!("{scheme}://{host}{path}")),
        Some(p) => match p.parse::<u32>() {
            Ok(n) if (1..=65535).contains(&n) => Ok(format!("{scheme}://{host}:{n}{path}")),
            _ => Err(format!("'{p}' is not a port - a port is a number from 1 to 65535, e.g. 8000")),
        },
    }
}

/// Is this address on this machine (as opposed to another host on the network)?
pub fn is_local(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0")
}

/// How one HTTP question to the engine went, as the wizard classifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpOutcome {
    Ok(String),
    /// the server answered with this HTTP status
    Status(u16),
    /// nothing listens there (connection refused)
    Refused,
    /// no answer in time: wrong host, a firewall, or a server too busy to answer
    Timeout,
    /// the host name does not resolve
    NoSuchHost,
    /// anything else, with the transport's own words
    Other(String),
}

impl HttpOutcome {
    /// Classify a transport error message (ureq's, or the OS's) - the words differ between
    /// Linux and macOS, so this matches on what they share.
    pub fn from_transport(msg: &str) -> HttpOutcome {
        let m = msg.to_ascii_lowercase();
        if m.contains("refused") {
            HttpOutcome::Refused
        } else if m.contains("timed out") || m.contains("timeout") || m.contains("would block") {
            HttpOutcome::Timeout
        } else if m.contains("dns") || m.contains("resolve") || m.contains("lookup") || m.contains("not known") || m.contains("nodename") || m.contains("no such host") || m.contains("name or service") {
            HttpOutcome::NoSuchHost
        } else {
            HttpOutcome::Other(msg.chars().take(160).collect())
        }
    }
}

/// One line saying what went wrong asking `path` at `url`, and what to do about it - the
/// "plain English" half of the live test. `key_given` = the wizard sent an API key.
pub fn explain(outcome: &HttpOutcome, url: &str, path: &str, key_given: bool) -> String {
    let local = is_local(url);
    match outcome {
        HttpOutcome::Ok(_) => format!("{url}{path} answered"),
        HttpOutcome::Refused if local => format!("Nothing is listening at {url} (connection refused). Is the engine running, and on this port? Start it, or type the port it really uses."),
        HttpOutcome::Refused => format!("{url} refused the connection. The machine is there but nothing listens on that port - or the engine only listens on its own 127.0.0.1. Start it with --host 0.0.0.0 (llama.cpp/vLLM/SGLang) or OLLAMA_HOST=0.0.0.0 (Ollama), or check the port."),
        HttpOutcome::Timeout if local => format!("{url} did not answer in time. The engine may still be loading its model - wait until it is ready and try again."),
        HttpOutcome::Timeout => format!("{url} did not answer in time. Check the address, that the machine is on, and that no firewall blocks the port."),
        HttpOutcome::NoSuchHost => format!("The host name in {url} is not known on this network. Use its IP address (e.g. 192.0.2.20:8000), or check the spelling."),
        HttpOutcome::Status(401 | 403) if key_given => format!("{url} refused the API key (HTTP {}). Check the key - copy it again from where the engine was started (--api-key / OPENAI_API_KEY).", status_of(outcome)),
        HttpOutcome::Status(401 | 403) => format!("{url} wants an API key (HTTP {}). The engine was started with one (--api-key, or OPENAI_API_KEY): type that key when asked.", status_of(outcome)),
        HttpOutcome::Status(404) => format!("{url} answers, but has no {path} (HTTP 404). This is not an OpenAI-compatible engine at this address, or it is mounted under a path - try the address you would give an OpenAI client, without /v1."),
        HttpOutcome::Status(code) if *code >= 500 => format!("{url} answered {path} with a server error (HTTP {code}). The engine is up but unhealthy - often still loading its model. Try again in a minute."),
        HttpOutcome::Status(code) => format!("{url} answered {path} with HTTP {code}, which is not what an LLM engine answers there. Check the address."),
        HttpOutcome::Other(msg) => format!("Could not talk to {url}: {msg}"),
    }
}

fn status_of(o: &HttpOutcome) -> u16 {
    if let HttpOutcome::Status(c) = o { *c } else { 0 }
}

/// What the live test learned about metrics, and what to do when there are none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsVerdict {
    pub ok: bool,
    pub line: String,
}

/// `has_metrics` = the engine's adapter got numbers from it (running/queued/tokens ...);
/// `missing` = how many of lss's metric fields it does not publish.
pub fn metrics_verdict(kind: EngineKind, has_metrics: bool, missing: usize, total: usize) -> MetricsVerdict {
    if has_metrics {
        let shown = total.saturating_sub(missing);
        let line = if missing == 0 {
            format!("metrics: {} publishes all {total} numbers lss shows", kind.label())
        } else {
            format!("metrics: {} publishes {shown} of the {total} numbers lss shows (the rest read 'n/a' - docs/ENGINES.md says which and why)", kind.label())
        };
        return MetricsVerdict { ok: true, line };
    }
    let fix = match kind {
        EngineKind::Sglang => "SGLang publishes them only when started with --enable-metrics: add it and restart the engine.",
        EngineKind::Vllm => "vLLM serves /metrics by default - something (a proxy? an old version?) hides it at this address. Point lss at the engine itself.",
        EngineKind::LlamaCpp => "llama.cpp publishes them only when llama-server is started with --metrics: add it and restart it (lss reads /slots meanwhile).",
        EngineKind::Tgi => "TGI serves /metrics by default - something in front of it hides it. Point lss at TGI itself.",
        EngineKind::Ollama => "Ollama has no metrics endpoint: lss reads what it can from /api/ps (the model, whether it is busy). That is normal for Ollama.",
        EngineKind::LmStudio => "LM Studio has no metrics endpoint: lss shows what /v1/models and /api/v0/models tell it. That is normal for LM Studio.",
        EngineKind::OpenAi => "This server answers the OpenAI API but publishes no metrics lss knows: lss shows up/down, the model and its own probe's speed.",
    };
    let normal = matches!(kind, EngineKind::Ollama | EngineKind::LmStudio | EngineKind::OpenAi);
    MetricsVerdict { ok: normal, line: format!("metrics: none. {fix}") }
}

/// The engine the person chose, as it will be written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EngineChoice {
    /// `EngineKind::name()` or "auto"
    pub kind: String,
    pub url: String,
    pub api_key: String,
    /// card #321: `Some(false)` = the person chose not to check this https engine's certificate
    /// (written into its `[[engine]]` block as `tls_verify = false`); None = the default check
    pub tls_verify: Option<bool>,
    /// card #321: the server's own certificate or its CA (PEM) the person pointed the wizard at;
    /// written as the top-level `tls_ca_file` ("" = none, and an existing one is left alone)
    pub tls_ca_file: String,
}

/// TOML-quote a string (basic string: backslash and quote escaped, control characters as \u).
pub fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `[[engine]]` block for a choice.
pub fn engine_block(c: &EngineChoice) -> String {
    let mut b = format!("[[engine]]\nkind = {}\nurl = {}\n", toml_str(&c.kind), toml_str(&c.url));
    if !c.api_key.trim().is_empty() {
        b.push_str(&format!("api_key = {}   # a secret: this file is readable by you only (0600)\n", toml_str(c.api_key.trim())));
    }
    if c.tls_verify == Some(false) {
        b.push_str("tls_verify = false   # its certificate is NOT checked (you chose this in lss setup); tls_ca_file is the safer fix\n");
    }
    b
}

/// The top-level `tls_ca_file` line for a choice, if it names one.
fn ca_line(c: Option<&EngineChoice>) -> Option<String> {
    c.map(|c| c.tls_ca_file.trim()).filter(|p| !p.is_empty()).map(|p| format!("tls_ca_file = {}   # the https engine's certificate or its CA (lss setup)", toml_str(p)))
}

/// A new collector.toml. `index` 1 = the first (the only one on most machines).
pub fn new_collector_toml(choice: Option<&EngineChoice>, listen_port: u16, index: usize, state_dir: &str, notify_cmd: &str) -> String {
    let mut t = String::from("# written by lss setup (every key and its default: packaging/collector.toml.example)\n");
    t.push_str(&format!("listen = [\"127.0.0.1:{listen_port}\"]\n"));
    if index != 1 {
        t.push_str(&format!("db_path = {}\n", toml_str(&format!("{}/lss-{index}.db", state_dir.trim_end_matches('/')))));
    }
    if !notify_cmd.is_empty() {
        t.push_str(&format!("alert_cmd = {}   # called as: alert_cmd <severity> <message>; hooks in alert.env next to this file\n", toml_str(notify_cmd)));
    }
    if let Some(l) = ca_line(choice) {
        t.push_str(&l);
        t.push('\n');
    }
    t.push('\n');
    match choice {
        Some(c) => t.push_str(&engine_block(c)),
        None => t.push_str("# engine_url = \"auto\": the collector finds the LLM server by itself (lss-collector --detect)\n"),
    }
    t
}

/// Replace ONLY the engine part of an existing collector.toml: every `[[engine]]` block goes,
/// and so do top-level `engine_url` / `sglang_url` / `engine_kind` keys (they would contradict
/// the new block); one new `[[engine]]` block (or none, for "auto") is appended. Every other
/// line - comments, `listen`, `[rates]`, alert settings - stays exactly as it was. Card #321:
/// a choice that names a `tls_ca_file` replaces the top-level one (it must sit before the first
/// table); a choice without one leaves an existing `tls_ca_file` alone.
pub fn replace_engine(existing: &str, choice: Option<&EngineChoice>) -> String {
    let ca = ca_line(choice);
    let mut out: Vec<&str> = Vec::new();
    let mut in_engine = false;
    let mut top_level = true;
    for line in existing.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if top_level {
                if let Some(l) = &ca {
                    while out.last().is_some_and(|l| l.trim().is_empty()) {
                        out.pop();
                    }
                    out.push(l.as_str());
                    out.push("");
                }
            }
            top_level = false;
            in_engine = t.trim_start_matches('[').trim_start().starts_with("[engine]") || t.replace(' ', "") == "[[engine]]";
            if in_engine {
                continue;
            }
        }
        if in_engine {
            continue;
        }
        if top_level {
            let key = t.split(['=', ' ']).next().unwrap_or("");
            if matches!(key, "engine_url" | "sglang_url" | "engine_kind") && t.contains('=') {
                continue;
            }
            if key == "tls_ca_file" && ca.is_some() && t.contains('=') {
                continue;
            }
        }
        out.push(line);
    }
    if top_level {
        // no table at all: the new tls_ca_file still belongs at the top level
        if let Some(l) = &ca {
            out.push(l.as_str());
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    s.push('\n');
    if let Some(c) = choice {
        s.push('\n');
        s.push_str(&engine_block(c));
    }
    s
}

/// The `listen` port of a collector.toml (the port `lss` must be pointed at), if it names one.
pub fn listen_port(collector_toml: &str) -> Option<u16> {
    let cfg = crate::config::parse_config(collector_toml).ok()?;
    cfg.listen.iter().find_map(|a| a.rsplit(':').next()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_people_type_becomes_a_base_url() {
        for (typed, want) in [
            ("8000", "http://127.0.0.1:8000"),
            (":8000", "http://127.0.0.1:8000"),
            ("localhost:11434", "http://127.0.0.1:11434"),
            ("  192.0.2.20:8000  ", "http://192.0.2.20:8000"),
            ("gpu-box:30000", "http://gpu-box:30000"),
            ("http://198.51.100.5:8080/", "http://198.51.100.5:8080"),
            ("http://198.51.100.5:8080/v1", "http://198.51.100.5:8080"),
            ("http://198.51.100.5:8080/v1/", "http://198.51.100.5:8080"),
            ("HTTP://Box:1234", "http://Box:1234"),
            ("box.local", "http://box.local"),
            ("http://box:80/llm/v1", "http://box:80/llm"),
            ("[::1]:8000", "http://[::1]:8000"),
            // card #311: https is kept, not refused (the collector speaks TLS since #308)
            ("https://gpu-box:8443/v1", "https://gpu-box:8443"),
            ("HTTPS://Box", "https://Box"),
            ("https://[::1]:8443/", "https://[::1]:8443"),
        ] {
            assert_eq!(normalize_url(typed).as_deref(), Ok(want), "typed {typed:?}");
        }
    }

    #[test]
    fn a_bad_address_is_refused_with_a_reason_a_person_can_act_on() {
        for (typed, says) in [
            ("", "type an address"),
            ("   ", "type an address"),
            ("my box:8000", "space"),
            ("host:99999", "not a port"),
            ("host:0", "not a port"),
            ("host:abc", "not a port"),
            ("ftp://box", "not an address"),
            (":", "not a port"),
            ("ho$t:80", "not a host name"),
            ("[::1", "closing ]"),
        ] {
            let err = normalize_url(typed).expect_err(typed);
            assert!(err.contains(says), "{typed:?} -> {err}");
        }
    }

    #[test]
    fn a_certificate_choice_is_written_where_the_collector_reads_it() {
        // card #321: tls_verify = false goes in the engine's own block; tls_ca_file is top-level,
        // so it must land BEFORE the first table, replacing an older one and nothing else
        let c = EngineChoice { kind: "auto".into(), url: "https://192.0.2.20:8443".into(), tls_verify: Some(false), tls_ca_file: "/home/you/box.pem".into(), ..Default::default() };
        let fresh = new_collector_toml(Some(&c), 8099, 1, "/s", "");
        let cfg = crate::config::parse_config(&fresh).unwrap();
        assert_eq!((cfg.tls_ca_file.as_str(), cfg.engines[0].tls_verify, cfg.engine_tls_verify()), ("/home/you/box.pem", Some(false), false), "{fresh}");
        let old = "# mine\nlisten = [\"127.0.0.1:8099\"]\ntls_ca_file = \"/old.pem\"\n\n[[engine]]\nkind = \"auto\"\nurl = \"http://127.0.0.1:1\"\n\n[rates]\npath = \"~/r.toml\"\n";
        let new = replace_engine(old, Some(&c));
        let cfg = crate::config::parse_config(&new).unwrap();
        assert_eq!(cfg.tls_ca_file, "/home/you/box.pem", "{new}");
        assert_eq!(new.lines().filter(|l| l.trim_start().starts_with("tls_ca_file")).count(), 1, "the old one is replaced, not duplicated: {new}");
        assert!(new.find("\ntls_ca_file").unwrap() < new.find("[rates]").unwrap(), "top level, before any table: {new}");
        assert_eq!((cfg.rates.path.as_str(), cfg.engines.len(), cfg.engines[0].tls_verify), ("~/r.toml", 1, Some(false)));
        // a choice with no certificate leaves an existing tls_ca_file alone, and checks by default
        let plain = EngineChoice { kind: "auto".into(), url: "https://192.0.2.20:8443".into(), ..Default::default() };
        let kept = replace_engine(old, Some(&plain));
        let cfg = crate::config::parse_config(&kept).unwrap();
        assert_eq!((cfg.tls_ca_file.as_str(), cfg.engines[0].tls_verify), ("/old.pem", None), "{kept}");
        // no table at all: still top level
        let bare = replace_engine("listen = [\"127.0.0.1:8099\"]\n", Some(&c));
        assert_eq!(crate::config::parse_config(&bare).unwrap().tls_ca_file, "/home/you/box.pem", "{bare}");
    }

    #[test]
    fn local_and_lan_addresses_are_told_apart() {
        assert!(is_local("http://127.0.0.1:8000"));
        assert!(is_local("http://[::1]:8000"));
        assert!(!is_local("http://192.0.2.20:8000"));
        assert!(!is_local("http://gpu-box"));
    }

    #[test]
    fn transport_errors_on_linux_and_macos_classify_the_same() {
        assert_eq!(HttpOutcome::from_transport("Connection refused (os error 111)"), HttpOutcome::Refused);
        assert_eq!(HttpOutcome::from_transport("Connection refused (os error 61)"), HttpOutcome::Refused);
        assert_eq!(HttpOutcome::from_transport("timed out reading response"), HttpOutcome::Timeout);
        assert_eq!(HttpOutcome::from_transport("Dns Failed: resolve dns name 'nohost:80': failed to lookup address information: nodename nor servname provided, or not known"), HttpOutcome::NoSuchHost);
        assert_eq!(HttpOutcome::from_transport("failed to lookup address information: Name or service not known"), HttpOutcome::NoSuchHost);
        assert!(matches!(HttpOutcome::from_transport("broken pipe"), HttpOutcome::Other(_)));
    }

    #[test]
    fn every_failure_is_explained_in_words_with_the_next_step() {
        let lan = "http://192.0.2.20:8000";
        let here = "http://127.0.0.1:8000";
        let cases: Vec<(HttpOutcome, &str, bool, &str)> = vec![
            (HttpOutcome::Refused, here, false, "Is the engine running"),
            (HttpOutcome::Refused, lan, false, "--host 0.0.0.0"),
            (HttpOutcome::Timeout, here, false, "loading its model"),
            (HttpOutcome::Timeout, lan, false, "firewall"),
            (HttpOutcome::NoSuchHost, lan, false, "IP address"),
            (HttpOutcome::Status(401), here, false, "wants an API key"),
            (HttpOutcome::Status(403), here, true, "refused the API key"),
            (HttpOutcome::Status(404), here, false, "not an OpenAI-compatible engine"),
            (HttpOutcome::Status(503), here, false, "still loading"),
            (HttpOutcome::Status(418), here, false, "Check the address"),
        ];
        for (o, url, key, says) in cases {
            let line = explain(&o, url, "/v1/models", key);
            assert!(line.contains(says), "{o:?} {url} key={key}: {line}");
            assert!(line.contains(url), "names the address: {line}");
        }
    }

    #[test]
    fn no_metrics_says_how_to_turn_them_on_per_engine() {
        assert!(metrics_verdict(EngineKind::Sglang, false, 16, 16).line.contains("--enable-metrics"));
        assert!(!metrics_verdict(EngineKind::Sglang, false, 16, 16).ok);
        assert!(metrics_verdict(EngineKind::LlamaCpp, false, 16, 16).line.contains("--metrics"));
        let o = metrics_verdict(EngineKind::Ollama, false, 16, 16);
        assert!(o.ok && o.line.contains("normal for Ollama"), "{o:?}");
        let v = metrics_verdict(EngineKind::Vllm, true, 3, 16);
        assert!(v.ok && v.line.contains("13 of the 16"), "{v:?}");
        assert!(metrics_verdict(EngineKind::Sglang, true, 0, 16).line.contains("all 16"));
    }

    #[test]
    fn a_new_config_parses_and_pins_the_engine_with_its_key() {
        let c = EngineChoice { kind: "vllm".into(), url: "http://192.0.2.20:8000".into(), api_key: "sk-\"odd\\key".into(), ..Default::default() };
        let t = new_collector_toml(Some(&c), 8099, 1, "/h/.local/state/lss", "/h/.local/bin/lss-notify.sh");
        let cfg = crate::config::parse_config(&t).unwrap_or_else(|e| panic!("{e}\n{t}"));
        assert_eq!(cfg.pinned_engine(), Some((Some(EngineKind::Vllm), "http://192.0.2.20:8000".to_string())));
        assert_eq!(cfg.pinned_api_key(), "sk-\"odd\\key");
        assert_eq!(cfg.listen, vec!["127.0.0.1:8099".to_string()]);
        assert_eq!(listen_port(&t), Some(8099));
        // auto: no engine block, and the second collector gets its own database
        let t2 = new_collector_toml(None, 8100, 2, "/s", "");
        let cfg2 = crate::config::parse_config(&t2).unwrap();
        assert!(cfg2.engine_is_auto() && cfg2.db_path == "/s/lss-2.db", "{t2}");
    }

    #[test]
    fn the_key_is_never_printed_whole() {
        let c = EngineChoice { kind: "openai".into(), url: "http://h:1".into(), api_key: "test-hidden-value-7890".into(), ..Default::default() };
        let cfg = crate::config::parse_config(&new_collector_toml(Some(&c), 8099, 1, "/s", "")).unwrap();
        let dumped = serde_json::to_string(&cfg).unwrap();
        assert!(!dumped.contains("hidden-value"), "{dumped}");
        assert!(dumped.contains("...7890"), "{dumped}");
        assert_eq!(crate::config::redact_secret("short"), "(set, hidden)");
        assert_eq!(crate::config::redact_secret(""), "");
    }

    #[test]
    fn re_running_setup_changes_only_the_engine_and_keeps_everything_else() {
        let old = "# my notes\nhost = \"box\"\nengine_url = \"http://127.0.0.1:1\"\nengine_kind = \"sglang\"\nlisten = [\"127.0.0.1:8099\"]\n\n[[engine]]\nkind = \"sglang\"\nurl = \"http://127.0.0.1:30000\"\n\n[rates]\npath = \"~/r.toml\"\n\n[probe]\ninterval_secs = 60\n";
        let c = EngineChoice { kind: "ollama".into(), url: "http://127.0.0.1:11434".into(), api_key: String::new(), ..Default::default() };
        let new = replace_engine(old, Some(&c));
        let cfg = crate::config::parse_config(&new).unwrap_or_else(|e| panic!("{e}\n{new}"));
        assert_eq!(cfg.engines.len(), 1, "{new}");
        assert_eq!(cfg.pinned_engine(), Some((Some(EngineKind::Ollama), "http://127.0.0.1:11434".to_string())));
        assert_eq!((cfg.host.as_str(), cfg.rates.path.as_str(), cfg.probe.interval_secs), ("box", "~/r.toml", 60), "{new}");
        assert!(new.contains("# my notes"));
        assert!(!new.contains("engine_url") && !new.contains("engine_kind"), "{new}");
        // back to auto: no engine at all
        let auto = replace_engine(old, None);
        assert!(crate::config::parse_config(&auto).unwrap().engine_is_auto(), "{auto}");
    }

    #[test]
    fn toml_strings_survive_anything_a_person_pastes() {
        for s in ["plain", "with \"quotes\"", "back\\slash", "tab\there", "unicode é 🔑"] {
            let t = format!("v = {}\n", toml_str(s));
            let parsed: std::collections::BTreeMap<String, String> = toml::from_str(&t).unwrap();
            assert_eq!(parsed["v"], s);
        }
    }
}
