//! card #308: https engine and gateway URLs.
//!
//! ureq ships with its own TLS setup, but it trusts EITHER the bundled Mozilla roots OR the
//! machine's own store (`native-certs`), never both. Each choice breaks somebody. With the
//! machine's store only, a static binary in a minimal container that has no CA bundle fails
//! every https request. With the bundled roots only, a company or home CA that the owner already
//! installed on the machine is ignored. So the collector builds its own client config: the
//! Mozilla roots, plus the machine's store when it can be read, plus `tls_ca_file` when one is
//! configured. That last one is the right answer for a LAN box that signs its own certificate.
//! `tls_verify = false` is the blunt one: it accepts any certificate. It is off by default and
//! the collector warns about it at every start.
//!
//! Everything is rustls on the *ring* backend: pure Rust plus ring's own assembly, no OpenSSL,
//! so the static musl release builds stay static.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::{Arc, OnceLock};

/// How the collector checks an https server's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// `false` = accept any certificate (`tls_verify = false`)
    pub verify: bool,
    /// extra CA certificates (PEM), already expanded; "" = none
    pub ca_file: String,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self { verify: true, ca_file: String::new() }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The certificates in `ca_file` ("" = none).
fn file_certs(ca_file: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    if ca_file.trim().is_empty() {
        return Ok(Vec::new());
    }
    let pem = std::fs::read(ca_file).map_err(|e| format!("tls_ca_file {ca_file}: {e}"))?;
    let certs = pem_certs(&pem).map_err(|e| format!("tls_ca_file {ca_file}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("tls_ca_file {ca_file}: no PEM certificate in it"));
    }
    Ok(certs)
}

/// The trusted roots: Mozilla's, this machine's store when it can be read, and `ca_file`.
/// Returns the store and what was loaded from where (for the start-up log line).
pub fn root_store(ca_file: &str) -> Result<(RootCertStore, String), String> {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mozilla = roots.len();
    // the machine's own store: failing to read it is not an error (a scratch container has
    // none), it just means the Mozilla roots are all there is
    let native = match rustls_native_certs::load_native_certs() {
        Ok(certs) => roots.add_parsable_certificates(certs).0,
        Err(_) => 0,
    };
    let mut from_file = 0;
    let certs = file_certs(ca_file)?;
    if !certs.is_empty() {
        // a self-signed SERVER certificate is not a usable CA (webpki: CaUsedAsEndEntity), yet it
        // is exactly what `openssl req -x509` makes - it still counts, as an exact pin (below)
        from_file = certs.len();
        roots.add_parsable_certificates(certs);
    }
    let summary = format!("{mozilla} Mozilla roots + {native} from this machine's store{}", if from_file > 0 { format!(" + {from_file} from {ca_file} (a CA, or the server's own certificate pinned exactly)") } else { String::new() });
    Ok((roots, summary))
}

/// PEM "CERTIFICATE" blocks out of a file, without pulling in another crate for it.
fn pem_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>().map_err(|e| format!("not a PEM certificate file ({e:?})"))
}

/// The rustls client config for these options.
pub fn client_config(opts: &TlsOptions) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
    let config = if opts.verify {
        let (roots, _) = root_store(&opts.ca_file)?;
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build().map_err(|e| e.to_string())?;
        let pins = file_certs(&opts.ca_file)?;
        builder.dangerous().with_custom_certificate_verifier(Arc::new(RootsOrPinned { webpki, pins })).with_no_client_auth()
    } else {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider()))).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// A ureq agent builder that uses these options for https. A config that cannot be built (a
/// missing or unreadable `tls_ca_file`) is reported and falls back to the default roots, so a
/// typo there costs https to that one box, never the whole collector.
pub fn agent_builder(opts: &TlsOptions) -> ureq::AgentBuilder {
    match client_config(opts) {
        Ok(cfg) => ureq::AgentBuilder::new().tls_config(cfg),
        Err(e) => {
            eprintln!("lss-collector: TLS: {e} - using the built-in roots only");
            ureq::AgentBuilder::new().tls_config(client_config(&TlsOptions::default()).expect("the default TLS config always builds"))
        }
    }
}

/// Which server a connection is for. card #316: `tls_verify` applies PER URL - an `[[engine]]`
/// block's own `tls_verify = false` turns the check off for that engine only, never for
/// `gate_url` (#308 had one config for both, so it silently covered the gateway too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Engine,
    Gate,
}

/// The collector's TLS configs, built ONCE at start from collector.toml (`configure`): one for the
/// engine, one for the gateway. Building one reads the machine's certificate store, which must not
/// happen once per bench request.
static ENGINE_TLS: OnceLock<Arc<ClientConfig>> = OnceLock::new();
static GATE_TLS: OnceLock<Arc<ClientConfig>> = OnceLock::new();
/// `gate_url` without its trailing '/', for `agent_for` ("" = no gateway)
static GATE_BASE: OnceLock<String> = OnceLock::new();

/// The options collector.toml asks for, for one target: `tls_ca_file` ("~" expanded) for both;
/// `tls_verify` = the engine's own (its `[[engine]]` block wins over the top-level key) for the
/// engine, the top-level key alone for the gateway.
pub fn options(cfg: &lss_core::config::Config, home: &str, target: Target) -> TlsOptions {
    let ca = cfg.tls_ca_file.trim();
    let verify = match target {
        Target::Engine => cfg.engine_tls_verify(),
        Target::Gate => cfg.tls_verify,
    };
    TlsOptions { verify, ca_file: if ca.is_empty() { String::new() } else { lss_core::config::expand_home(ca, home) } }
}

/// Build and keep the collector's TLS configs; the log lines for any https URL go to stderr, each
/// with the setting that really applies to THAT url.
pub fn configure(cfg: &lss_core::config::Config, home: &str) {
    for (url, target, slot) in [(&cfg.sglang_url, Target::Engine, &ENGINE_TLS), (&cfg.gate_url, Target::Gate, &GATE_TLS)] {
        let opts = options(cfg, home, target);
        if let Some(note) = startup_note(url, &opts) {
            eprintln!("{note}");
        }
        let config = client_config(&opts).unwrap_or_else(|e| {
            eprintln!("lss-collector: TLS: {e} - using the built-in roots only");
            client_config(&TlsOptions::default()).expect("the default TLS config always builds")
        });
        let _ = slot.set(config);
    }
    let _ = GATE_BASE.set(cfg.gate_url.trim().trim_end_matches('/').to_string());
}

/// An agent builder for the ENGINE: the configured TLS settings (the defaults until `configure`
/// has run - tests, and `--detect`, which only knocks on local ports).
pub fn agent() -> ureq::AgentBuilder {
    agent_to(Target::Engine)
}

/// An agent builder for the GATEWAY (`gate_url`): the top-level `tls_verify`, never an
/// `[[engine]]` block's.
pub fn gate_agent() -> ureq::AgentBuilder {
    agent_to(Target::Gate)
}

fn agent_to(target: Target) -> ureq::AgentBuilder {
    let slot = match target {
        Target::Engine => &ENGINE_TLS,
        Target::Gate => &GATE_TLS,
    };
    match slot.get() {
        Some(cfg) => ureq::AgentBuilder::new().tls_config(cfg.clone()),
        None => agent_builder(&TlsOptions::default()),
    }
}

/// The agent builder for whichever server `url` is on: the gateway's when it starts with
/// `gate_url`, else the engine's.
pub fn agent_for(url: &str) -> ureq::AgentBuilder {
    agent_to(target_of(url, GATE_BASE.get().map_or("", String::as_str)))
}

/// `url` is the gateway's when it is `gate_base` or a path under it.
pub fn target_of(url: &str, gate_base: &str) -> Target {
    let rest = if gate_base.is_empty() { None } else { url.strip_prefix(gate_base) };
    match rest {
        Some(r) if r.is_empty() || r.starts_with('/') || r.starts_with('?') => Target::Gate,
        _ => Target::Engine,
    }
}

/// The one line the collector logs at start for an https URL (and the warning for verify=false).
pub fn startup_note(url: &str, opts: &TlsOptions) -> Option<String> {
    if !url.trim_start().to_ascii_lowercase().starts_with("https://") {
        return None;
    }
    Some(if !opts.verify {
        format!("lss-collector: WARNING - tls_verify = false: the certificate of {url} is NOT checked. Anyone who can intercept this connection can impersonate the server. Use tls_ca_file with the server's CA instead where you can.")
    } else {
        match root_store(&opts.ca_file) {
            Ok((_, summary)) => format!("lss-collector: https to {url}: certificates checked against {summary}"),
            Err(e) => format!("lss-collector: https to {url}: {e} - checking against the built-in roots only"),
        }
    })
}

/// Turns a ureq transport error about a certificate into a sentence that says what to do.
/// None = not a certificate problem.
pub fn explain_cert_error(err: &str) -> Option<String> {
    let lower = err.to_ascii_lowercase();
    if !(lower.contains("certificate") || lower.contains("unknownissuer") || lower.contains("handshake")) {
        return None;
    }
    let raw = err.rsplit(": ").next().unwrap_or(err).trim();
    // rustls' reasons, in words (the raw one stays in brackets for a search engine)
    let what = if raw.contains("PinMismatch") {
        // card #316: a wrong pin used to read "it is self-signed"
        "the certificate does not match the pinned one in tls_ca_file (nor is it signed by a CA in that file)"
    } else if raw.contains("SelfSigned") || raw.contains("CaUsedAsEndEntity") || (raw.contains("UnknownIssuer") && lower.contains("self")) {
        "it is self-signed"
    } else if raw.contains("UnknownIssuer") {
        "it is signed by a CA this machine does not trust"
    } else if raw.contains("BadSignature") {
        // same subject as a certificate you trust, different key: another box, or a re-issued one
        "it is not the certificate (or from the CA) in tls_ca_file"
    } else if raw.contains("Expired") {
        "it has expired"
    } else if raw.contains("NotValidYet") {
        "it is not valid yet (check this machine's clock)"
    } else if raw.contains("NotValidForName") {
        "it was not issued for this host name or address"
    } else {
        "it did not pass the check"
    };
    Some(format!("TLS: the server's certificate was not accepted - {what} ({raw}). For a box that signs its own: tls_ca_file = its CA or its own certificate, or tls_verify = false"))
}

/// The normal check (a chain to a trusted root, the right name, in date), and failing that, the
/// server's certificate is accepted when it is BYTE FOR BYTE one listed in `tls_ca_file`: the owner
/// pinned that exact certificate. That is how a self-signed LAN box is trusted without trusting
/// everyone (`tls_verify = false`). A pin is the owner's explicit say-so: its name and dates are
/// not re-checked, just as an ssh host key's are not.
#[derive(Debug)]
struct RootsOrPinned {
    webpki: Arc<WebPkiServerVerifier>,
    pins: Vec<CertificateDer<'static>>,
}

impl ServerCertVerifier for RootsOrPinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        match self.webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Ok(ok) => Ok(ok),
            Err(_) if self.pins.iter().any(|p| p.as_ref() == end_entity.as_ref()) => Ok(ServerCertVerified::assertion()),
            // card #316: "who signed it" failures get a reason of their own. With a tls_ca_file the
            // owner told us what to expect, so the answer is "not the one you pinned" (#308 said
            // "self-signed" for a wrong pin); without one, a certificate that verifies against
            // ITSELF is self-signed, and says so (rustls calls both UnknownIssuer).
            Err(rustls::Error::InvalidCertificate(ce)) if is_issuer_failure(&ce) => {
                let why = if !self.pins.is_empty() {
                    CertCheck::PinMismatch
                } else if is_self_signed(end_entity, server_name, now) {
                    CertCheck::SelfSigned
                } else {
                    return Err(rustls::Error::InvalidCertificate(ce));
                };
                Err(rustls::Error::InvalidCertificate(rustls::CertificateError::Other(rustls::OtherError(Arc::new(why)))))
            }
            Err(e) => Err(e),
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// card #316: the two reasons a certificate is refused that rustls cannot name on its own. Their
/// names ride the error text ("... Other(OtherError(PinMismatch))") into `explain_cert_error`.
#[derive(Debug)]
enum CertCheck {
    /// a tls_ca_file is configured, and this certificate is neither in it nor signed by a CA in it
    PinMismatch,
    /// no tls_ca_file, and the certificate is its own issuer
    SelfSigned,
}

impl std::fmt::Display for CertCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for CertCheck {}

/// The failures that mean "nobody we trust signed this": a wrong or unknown issuer, a signature
/// that does not check out, or a self-signed certificate marked CA:TRUE.
fn is_issuer_failure(e: &rustls::CertificateError) -> bool {
    use rustls::CertificateError as C;
    match e {
        C::UnknownIssuer | C::BadSignature => true,
        C::Other(o) => format!("{o:?}").contains("CaUsedAsEndEntity"),
        _ => false,
    }
}

/// Whether `cert` is its own issuer: it verifies with itself as the only trusted root. Any
/// failure other than "unknown issuer" / "bad signature" (a wrong name, an expired date, a CA
/// flag) still means the chain closed on itself.
fn is_self_signed(cert: &CertificateDer<'_>, name: &ServerName<'_>, now: UnixTime) -> bool {
    let mut roots = RootCertStore::empty();
    if roots.add(cert.clone().into_owned()).is_err() {
        return false;
    }
    let Ok(v) = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build() else {
        return false;
    };
    !matches!(v.verify_server_cert(cert, &[], name, &[], now), Err(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer | rustls::CertificateError::BadSignature)))
}

/// `tls_verify = false`: accepts any certificate, but still checks the handshake signatures, so
/// the connection is at least with whoever holds the key of the certificate it was shown.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(&self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _server_name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A CA, and a server certificate for 127.0.0.1 signed by it - made fresh for each test run,
    /// so no private key is ever committed.
    pub(crate) struct Pki {
        pub(crate) ca_pem: String,
        cert_der: CertificateDer<'static>,
        key_der: rustls::pki_types::PrivateKeyDer<'static>,
    }

    pub(crate) fn pki() -> Pki {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name.push(rcgen::DnType::CommonName, "lss test CA");
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let mut leaf = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        leaf.subject_alt_names.push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
        let cert = leaf.signed_by(&key, &ca_cert, &ca_key).unwrap();
        Pki {
            ca_pem: ca_cert.pem(),
            cert_der: cert.der().clone(),
            key_der: rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        }
    }

    /// A TLS fake engine on 127.0.0.1: answers every request with a one-model /v1/models body.
    /// Serves `n` connections, then stops. Returns its https base URL.
    pub(crate) fn tls_engine(p: &Pki, n: usize) -> String {
        let server = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![p.cert_der.clone()], p.key_der.clone_key())
            .unwrap();
        let server = Arc::new(server);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(n) {
                let Ok(mut tcp) = stream else { continue };
                let mut conn = rustls::ServerConnection::new(server.clone()).unwrap();
                let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
                let mut buf = [0u8; 4096];
                if tls.read(&mut buf).is_err() {
                    continue; // the client refused our certificate: that is the test
                }
                let body = r#"{"object":"list","data":[{"id":"tls-fake","object":"model"}]}"#;
                let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        format!("https://127.0.0.1:{port}")
    }

    fn get(opts: &TlsOptions, url: &str) -> Result<String, String> {
        let agent = agent_builder(opts).timeout(std::time::Duration::from_secs(10)).build();
        crate::collect::http_get(&agent, &format!("{url}/v1/models"))
    }

    #[test]
    fn a_self_signed_engine_is_refused_by_default_with_a_sentence_that_says_what_to_do() {
        let p = pki();
        let url = tls_engine(&p, 1);
        let err = get(&TlsOptions::default(), &url).expect_err("an unknown CA must not be trusted by default");
        assert!(err.starts_with("TLS: the server's certificate was not accepted - it is signed by a CA this machine does not trust"), "{err}");
        assert!(err.contains("tls_ca_file") && err.contains("tls_verify = false"), "the error names both fixes: {err}");
    }

    #[test]
    fn the_setup_wizards_live_test_speaks_tls_and_explains_a_certificate_it_does_not_trust() {
        // card #311: `lss setup` used to refuse https:// outright; now it tests the address with
        // the collector's own TLS agent, and a certificate it cannot trust is said in words
        let p = pki();
        let url = tls_engine(&p, 1);
        assert_eq!(lss_core::setup::normalize_url(&url).as_deref(), Ok(url.as_str()), "the wizard keeps https");
        match crate::setup_run::get(&url, "/v1/models", "", &TlsOptions::default(), std::time::Duration::from_secs(10)) {
            lss_core::setup::HttpOutcome::Other(msg) => {
                assert!(msg.contains("certificate was not accepted") && msg.contains("tls_ca_file"), "{msg}");
            }
            other => panic!("a TLS engine with an untrusted CA must fail on its certificate: {other:?}"),
        }
    }

    #[test]
    fn tls_verify_false_accepts_the_self_signed_engine() {
        let p = pki();
        let url = tls_engine(&p, 1);
        let body = get(&TlsOptions { verify: false, ca_file: String::new() }, &url).expect("tls_verify = false accepts any certificate");
        assert!(body.contains("tls-fake"), "{body}");
    }

    #[test]
    fn tls_ca_file_trusts_the_lan_ca_and_keeps_checking_everything_else() {
        let p = pki();
        let dir = std::env::temp_dir().join(format!("lss-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("lan-ca.pem");
        std::fs::write(&ca, &p.ca_pem).unwrap();
        let opts = TlsOptions { verify: true, ca_file: ca.to_string_lossy().into_owned() };
        let url = tls_engine(&p, 1);
        let body = get(&opts, &url).expect("a server signed by the configured CA is trusted, VERIFIED");
        assert!(body.contains("tls-fake"), "{body}");
        // ...and that CA is not a blanket pass: another CA's server is still refused
        let other = pki();
        let url2 = tls_engine(&other, 1);
        let err = get(&opts, &url2).expect_err("a certificate from a CA that is not configured is still refused");
        assert!(err.starts_with("TLS:"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single self-signed server certificate, the way `openssl req -x509` makes one (`is_ca`
    /// true: openssl marks it CA:TRUE) or a plain one (`is_ca` false).
    fn self_signed(is_ca: bool) -> Pki {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        p.subject_alt_names.push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
        if is_ca {
            p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        }
        let cert = p.self_signed(&key).unwrap();
        Pki { ca_pem: cert.pem(), cert_der: cert.der().clone(), key_der: rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()) }
    }

    #[test]
    fn tls_ca_file_holding_the_servers_own_self_signed_certificate_pins_exactly_that_one() {
        // card #308, found by the release-binary e2e: `openssl req -x509` makes ONE self-signed
        // certificate marked CA:TRUE; pointed at it, the plain root check refused it
        // (CaUsedAsEndEntity) - the most common home setup could not be trusted without
        // tls_verify = false. It is now accepted as an exact pin, for both shapes.
        for is_ca in [true, false] {
            let p = self_signed(is_ca);
            let dir = std::env::temp_dir().join(format!("lss-tls-pin-{}-{is_ca}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("server.pem");
            std::fs::write(&file, &p.ca_pem).unwrap();
            let opts = TlsOptions { verify: true, ca_file: file.to_string_lossy().into_owned() };
            let body = get(&opts, &tls_engine(&p, 1)).unwrap_or_else(|e| panic!("is_ca={is_ca}: the pinned certificate must be accepted: {e}"));
            assert!(body.contains("tls-fake"), "{body}");
            // the pin is EXACT: another self-signed server with the same names is refused
            let other = self_signed(is_ca);
            let err = get(&opts, &tls_engine(&other, 1)).expect_err("a different certificate is not pinned");
            assert!(err.starts_with("TLS: the server's certificate was not accepted"), "is_ca={is_ca}: {err}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_bad_tls_ca_file_is_a_clear_error_and_the_default_roots_still_work() {
        assert!(root_store("/nonexistent/ca.pem").unwrap_err().contains("tls_ca_file /nonexistent/ca.pem"));
        let dir = std::env::temp_dir().join(format!("lss-tls-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("empty.pem"), "not a certificate\n").unwrap();
        let e = root_store(&dir.join("empty.pem").to_string_lossy()).unwrap_err();
        assert!(e.contains("no PEM certificate"), "{e}");
        let (roots, summary) = root_store("").unwrap();
        assert!(roots.len() > 100, "the Mozilla roots are always there, even with no system store: {summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_notes_warn_for_verify_off_and_say_nothing_for_plain_http() {
        assert_eq!(startup_note("http://127.0.0.1:30000", &TlsOptions::default()), None);
        let w = startup_note("https://192.0.2.10:8000", &TlsOptions { verify: false, ca_file: String::new() }).unwrap();
        assert!(w.contains("WARNING") && w.contains("NOT checked"), "{w}");
        let ok = startup_note("https://192.0.2.10:8000", &TlsOptions::default()).unwrap();
        assert!(ok.contains("Mozilla roots"), "{ok}");
    }

    /// card #316 (#308's verifier): `tls_verify` is PER URL. An `[[engine]]` block's own
    /// `tls_verify = false` turns the check off for that engine only - the gateway keeps the
    /// top-level setting (it used to share the engine's one config, warned but surprising).
    #[test]
    fn an_engine_blocks_tls_verify_false_never_covers_the_gateway() {
        let cfg = lss_core::config::Config {
            sglang_url: "https://192.0.2.10:8000".into(),
            gate_url: "https://192.0.2.10:8443".into(),
            engines: vec![lss_core::config::EngineEntry { url: "https://192.0.2.10:8000".into(), tls_verify: Some(false), ..Default::default() }],
            ..Default::default()
        };
        assert!(!options(&cfg, "/h", Target::Engine).verify, "the engine block says false");
        assert!(options(&cfg, "/h", Target::Gate).verify, "the gateway keeps the top-level true");
        assert!(startup_note(&cfg.sglang_url, &options(&cfg, "/h", Target::Engine)).unwrap().contains("NOT checked"));
        assert!(!startup_note(&cfg.gate_url, &options(&cfg, "/h", Target::Gate)).unwrap().contains("NOT checked"), "no warning claims the gateway is unchecked");
        // the top-level false still covers both (unless an engine block says otherwise)
        let all_off = lss_core::config::Config { tls_verify: false, ..cfg.clone() };
        assert!(!options(&all_off, "/h", Target::Gate).verify);
        // which URLs are the gateway's
        assert_eq!(target_of("https://192.0.2.10:8443/gate/health", "https://192.0.2.10:8443"), Target::Gate);
        assert_eq!(target_of("https://192.0.2.10:8443", "https://192.0.2.10:8443"), Target::Gate);
        assert_eq!(target_of("https://192.0.2.10:84430/x", "https://192.0.2.10:8443"), Target::Engine, "a longer port is another server");
        assert_eq!(target_of("https://192.0.2.10:8000/metrics", "https://192.0.2.10:8443"), Target::Engine);
        assert_eq!(target_of("https://192.0.2.10:8000/metrics", ""), Target::Engine, "no gateway: everything is the engine's");
    }

    /// card #316: a wrong PIN said "it is self-signed"; now a pin mismatch and a genuinely
    /// self-signed certificate each say what they are.
    #[test]
    fn a_wrong_pin_and_a_self_signed_certificate_get_different_messages() {
        let dir = std::env::temp_dir().join(format!("lss-tls-316-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for is_ca in [false, true] {
            // no tls_ca_file, a real self-signed server: "self-signed"
            let server = self_signed(is_ca);
            let err = get(&TlsOptions::default(), &tls_engine(&server, 1)).expect_err("self-signed is refused by default");
            assert!(err.starts_with("TLS: the server's certificate was not accepted - it is self-signed"), "is_ca={is_ca}: {err}");
            // tls_ca_file pins ANOTHER certificate: "does not match the pinned one"
            let pinned = self_signed(is_ca);
            let file = dir.join(format!("pinned-{is_ca}.pem"));
            std::fs::write(&file, &pinned.ca_pem).unwrap();
            let opts = TlsOptions { verify: true, ca_file: file.to_string_lossy().into_owned() };
            let err = get(&opts, &tls_engine(&server, 1)).expect_err("a certificate that is not the pinned one is refused");
            assert!(err.contains("does not match the pinned one in tls_ca_file"), "is_ca={is_ca}: {err}");
            assert!(!err.contains("it is self-signed"), "is_ca={is_ca}: a pin mismatch is not reported as self-signed: {err}");
        }
        // a server from an unknown CA, no tls_ca_file: still "a CA this machine does not trust"
        let err = get(&TlsOptions::default(), &tls_engine(&pki(), 1)).unwrap_err();
        assert!(err.contains("signed by a CA this machine does not trust"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_certificate_failures_are_explained_as_tls() {
        assert!(explain_cert_error("https://x/v1/models: Connection Failed: tls connection init failed: invalid peer certificate: UnknownIssuer").unwrap().contains("not trust (UnknownIssuer)"));
        assert!(explain_cert_error("x: invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))").unwrap().contains("it is self-signed"));
        assert!(explain_cert_error("x: invalid peer certificate: BadSignature").unwrap().contains("not the certificate (or from the CA) in tls_ca_file"));
        assert_eq!(explain_cert_error("http://x/v1/models: Connection Failed: Connect error: connection refused"), None);
    }
}
