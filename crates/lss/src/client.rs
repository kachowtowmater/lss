use lss_core::model::Status;
use std::time::Duration;

/// card #180 gate 6: an error that starts with this is NOT "unreachable" - the collector is
/// running and answered, but has not found an LLM server on its machine yet (it says so with a
/// 503 and the `X-LSS-Waiting` header while it looks). The rest of the error is its own sentence.
pub const WAITING: &str = "collector running, no LLM server found yet: ";

pub fn is_waiting(err: &str) -> bool {
    err.starts_with(WAITING)
}

/// Raw `/status` body. Errors are short, human sentences: they go straight on screen.
pub fn fetch_raw(base: &str) -> Result<String, String> {
    let url = format!("{}/status", base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(Duration::from_secs(6)).build();
    match agent.get(&url).call() {
        Ok(r) => r.into_string().map_err(|e| format!("reading {url}: {e}")),
        Err(ureq::Error::Status(503, r)) if r.header("x-lss-waiting").is_some() => {
            Err(format!("{WAITING}{}", r.into_string().unwrap_or_default().trim()))
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("{url} answered HTTP {code}")),
        Err(e) => {
            let text = e.to_string();
            let reason = text.rsplit(": ").next().unwrap_or(&text);
            Err(format!("cannot reach {url} ({reason})"))
        }
    }
}

pub fn parse(body: &str) -> Result<Status, String> {
    // #51, 2026-09-21 (ship-gate verifier): a field absent because the collector predates it,
    // and the same field present-but-null because it has no data yet, both deserialise to the
    // same `None` - deliberately (an older lss must still read a newer collector's document, and
    // vice versa) but that loses the one distinction page 1 needs to never print a confident
    // sentence about a field that simply is not there. Raw key presence, checked once here
    // before the typed parse throws it away, is the only place that distinction still exists.
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("collector sent something that is not /status JSON: {e}"))?;
    let page1_fields_deployed = raw.get("serve").and_then(|s| s.get("secs_since_last_token")).is_some();
    let mut status: Status = serde_json::from_value(raw).map_err(|e| format!("collector sent something that is not /status JSON: {e}"))?;
    if status.v != lss_core::STATUS_SCHEMA_VERSION {
        return Err(format!("collector speaks /status schema v{}, this lss understands v{} - upgrade lss", status.v, lss_core::STATUS_SCHEMA_VERSION));
    }
    status.serve.page1_fields_deployed = page1_fields_deployed;
    Ok(status)
}

pub fn fetch(base: &str) -> Result<Status, String> {
    parse(&fetch_raw(base)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_other_schema_versions_and_garbage() {
        assert!(parse("{\"v\":2}").unwrap_err().contains("schema v2"));
        assert!(parse("<html>").unwrap_err().contains("not /status JSON"));
        assert!(parse(include_str!("../../../fixtures/status_golden.json")).is_ok());
    }

    /// card #180 gate 6: a collector that answers 503 + X-LSS-Waiting is running and looking for
    /// an engine - that must come back as the WAITING error with its own sentence, never as
    /// "cannot reach" (which told a fresh install the monitor was broken).
    #[test]
    fn a_collector_waiting_for_an_engine_is_not_reported_as_unreachable() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let body = "lss-collector is running but has not found an LLM server on this machine yet";
            let _ = s.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nX-LSS-Waiting: engine\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        });
        let e = fetch(&format!("http://127.0.0.1:{port}")).unwrap_err();
        t.join().unwrap();
        assert!(is_waiting(&e) && e.contains("has not found an LLM server"), "{e}");
        assert!(!e.contains("cannot reach"), "{e}");
    }

    #[test]
    fn unreachable_collector_is_an_error_not_a_panic() {
        let e = fetch("http://127.0.0.1:9").unwrap_err();
        assert!(e.contains("cannot reach http://127.0.0.1:9/status"), "{e}");
    }

    /// #51 ship-gate fix: `page1_fields_deployed` comes from raw JSON key presence, not the
    /// typed value. The current golden fixture already carries the key (re-blessed after #51
    /// stage c), so it is the DEPLOYED case; an old collector's document is built here by
    /// removing the key entirely, the way a document from before that commit actually looks.
    #[test]
    fn page1_fields_deployed_reads_raw_key_presence_not_the_typed_value() {
        let deployed = parse(include_str!("../../../fixtures/status_golden.json")).unwrap();
        assert!(deployed.serve.page1_fields_deployed, "the current golden fixture has the key (even where the value is null): a real, deployed collector with no data yet");

        let mut v: serde_json::Value = serde_json::from_str(include_str!("../../../fixtures/status_golden.json")).unwrap();
        v["serve"].as_object_mut().unwrap().remove("secs_since_last_token"); // an old collector's document: the key is not there at all
        let old = parse(&v.to_string()).unwrap();
        assert!(!old.serve.page1_fields_deployed, "the key is entirely absent: an old collector that predates #51");
        assert_eq!(old.serve.secs_since_last_token, None, "the typed value is None either way - only the raw-presence flag tells the two apart");
    }
}
