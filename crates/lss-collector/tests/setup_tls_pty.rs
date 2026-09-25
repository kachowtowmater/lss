//! Cards #311 + #321: `lss-collector setup` against a self-signed https engine, driven through a
//! REAL pseudo-terminal (util-linux `script`), the way a person meets it - not scripted stdin.
//! At the certificate refusal the wizard must OFFER [c] (a certificate file -> tls_ca_file) and
//! [i] (trust it insecurely -> tls_verify = false for this engine), end connected either way, and
//! a re-run must use what collector.toml now says instead of failing on the certificate again.
//!
//! Linux only: macOS `script` takes different arguments, and this suite runs on the Linux build
//! host and in CI.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A self-signed certificate for 127.0.0.1 (what `openssl req -x509` makes), fresh per run.
fn self_signed() -> (String, rustls::pki_types::CertificateDer<'static>, rustls::pki_types::PrivateKeyDer<'static>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.subject_alt_names.push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), cert.der().clone(), rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()))
}

/// An https fake engine: every request gets a one-model /v1/models body. Returns its base URL.
fn tls_engine(cert: rustls::pki_types::CertificateDer<'static>, key: rustls::pki_types::PrivateKeyDer<'static>) -> String {
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let server = Arc::new(server);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut tcp) = stream else { continue };
            let server = server.clone();
            std::thread::spawn(move || {
                let mut conn = rustls::ServerConnection::new(server).unwrap();
                let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
                let mut buf = [0u8; 4096];
                if tls.read(&mut buf).is_err() {
                    return; // the client refused the certificate
                }
                let body = r#"{"object":"list","data":[{"id":"tls-pty-fake","object":"model"}]}"#;
                let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    format!("https://127.0.0.1:{port}")
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lss-pty-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The wizard in a pty: its output collected as it comes, answers typed when a prompt appears.
struct Pty {
    child: Child,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Pty {
    fn start(home: &Path, args: &[&str]) -> Pty {
        let bin = env!("CARGO_BIN_EXE_lss-collector");
        let cmd = std::iter::once(bin.to_string()).chain(args.iter().map(|a| format!("'{a}'"))).collect::<Vec<_>>().join(" ");
        let mut child = Command::new("script")
            .args(["-qfec", &cmd, "/dev/null"])
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env("TERM", "dumb")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("util-linux `script` runs (it gives the wizard a real terminal)");
        let out = Arc::new(Mutex::new(Vec::new()));
        for mut src in [Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>, Box::new(child.stderr.take().unwrap())] {
            let out = out.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = src.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    out.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
        }
        Pty { child, out }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    /// Wait until `needle` has been printed (after `from` bytes), then type `answer` + Enter.
    fn answer(&mut self, needle: &str, answer: &str) {
        let from = self.text().len();
        self.wait_for_after(needle, from.saturating_sub(4096));
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(format!("{answer}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }

    fn wait_for_after(&self, needle: &str, from: usize) {
        let until = Instant::now() + Duration::from_secs(40);
        while Instant::now() < until {
            let t = self.text();
            if t.get(from..).unwrap_or(&t).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the wizard never printed {needle:?}; it printed:\n{}", self.text());
    }

    /// The wizard's exit code, once it has finished.
    fn finish(mut self) -> (i32, String) {
        drop(self.child.stdin.take());
        let until = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(200));
                return (status.code().unwrap_or(-1), self.text());
            }
            if Instant::now() > until {
                let _ = self.child.kill();
                panic!("the wizard did not finish; it printed:\n{}", self.text());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn args<'a>(cfg: &'a str, url: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["setup", "--url", url, "--kind", "vllm", "--config-dir", cfg, "--skip-cost", "--no-service"];
    a.extend_from_slice(extra);
    a
}

#[test]
fn c_trusts_the_servers_own_certificate_and_a_re_run_connects_with_it() {
    let (pem, cert, key) = self_signed();
    let url = tls_engine(cert, key);
    let d = tmp("c");
    let cfg = d.join("cfg");
    let pem_path = d.join("box.pem");
    std::fs::write(&pem_path, &pem).unwrap();
    let cfg_s = cfg.display().to_string();

    let mut w = Pty::start(&d, &args(&cfg_s, &url, &[]));
    w.answer("c, i, or r = try again", "c");
    w.answer("Path to the certificate (PEM):", &pem_path.display().to_string());
    let (code, text) = w.finish();
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("certificate was not accepted"), "first the refusal, in words:\n{text}");
    assert!(text.contains("[c] use a CA or certificate file") && text.contains("[i] trust this box insecurely"), "both choices are OFFERED:\n{text}");
    // #311 F3: the refused test read nothing, so it gives no metrics verdict (it used to blame "a
    // proxy"); the verdict after the retest is about a real answer and may say anything
    let refused = &text[text.find("== 2/5").unwrap()..text.find("[c] use a CA").unwrap()];
    // card #325 (4): ...and it SAYS the metrics were not checked, rather than staying silent
    assert!(!refused.contains("metrics: none") && !refused.contains("proxy") && refused.contains("[--] metrics: not checked"), "#311 F3 / #325: no metrics verdict from a refused certificate, only 'not checked':\n{refused}");
    assert!(text.contains("[ok] models: tls-pty-fake"), "and then it is connected:\n{text}");
    let written = std::fs::read_to_string(cfg.join("collector.toml")).unwrap();
    let parsed = lss_core::config::parse_config(&written).unwrap();
    assert_eq!((parsed.tls_ca_file.as_str(), parsed.engines[0].url.as_str(), parsed.engines[0].tls_verify), (pem_path.to_str().unwrap(), url.as_str(), None), "{written}");

    // #311 F2: the re-run reads tls_ca_file from collector.toml and connects with no question
    let w = Pty::start(&d, &args(&cfg_s, &url, &["--force"]));
    let (code, text) = w.finish();
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("(from collector.toml: tls_ca_file = "), "{text}");
    assert!(text.contains("[ok] models: tls-pty-fake") && !text.contains("not accepted"), "the re-run connects with the saved certificate:\n{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn i_trusts_the_box_insecurely_with_a_warning_and_a_re_run_connects() {
    let (_pem, cert, key) = self_signed();
    let url = tls_engine(cert, key);
    let d = tmp("i");
    let cfg = d.join("cfg");
    let cfg_s = cfg.display().to_string();

    let mut w = Pty::start(&d, &args(&cfg_s, &url, &[]));
    w.answer("c, i, or r = try again", "i");
    let (code, text) = w.finish();
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("will NOT be checked (WARNING"), "the insecure choice is warned about:\n{text}");
    assert!(text.contains("[ok] models: tls-pty-fake"), "{text}");
    let written = std::fs::read_to_string(cfg.join("collector.toml")).unwrap();
    let parsed = lss_core::config::parse_config(&written).unwrap();
    assert_eq!((parsed.engines[0].tls_verify, parsed.tls_ca_file.as_str()), (Some(false), ""), "{written}");

    let w = Pty::start(&d, &args(&cfg_s, &url, &["--force"]));
    let (code, text) = w.finish();
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("(from collector.toml: tls_verify = false"), "{text}");
    assert!(text.contains("[ok] models: tls-pty-fake") && !text.contains("not accepted"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}
