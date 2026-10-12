//! Network checks: DNS lookups, TLS certificates, TCP reachability and HTTP(S) requests,
//! run from this computer or from an SSH Host.
//!
//! - Locally: A / AAAA through the system resolver (what this computer sees, VPN and
//!   `/etc/hosts` included); MX, TXT, NS and CNAME through DNS over HTTPS
//!   (`cloudflare-dns.com`'s JSON API) since no approved crate speaks DNS. TLS through
//!   rustls with a verifier that records the chain and the verification result instead of
//!   failing; TCP with tokio; HTTP with reqwest, following redirects one hop at a time.
//! - On a Host: `getent ahosts` / `dig` / `host`, `openssl s_client`, `nc` (or bash's
//!   `/dev/tcp`) and `curl -w`, run over the Host's shared SSH session; their output is
//!   parsed here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use switchyard_store::ProfileId;

use super::x509::{CertInfo, parse_cert};

/// How long one network step may take.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Certificates expiring sooner than this many days are flagged.
pub const EXPIRY_WARN_DAYS: i64 = 30;
/// DNS over HTTPS resolver for record types the system resolver cannot answer.
pub const DOH_URL: &str = "https://cloudflare-dns.com/dns-query";

/// What a check does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    /// DNS lookup.
    Dns,
    /// TLS certificate of `host:port`.
    Tls,
    /// TCP connect to `host:port`.
    Tcp,
    /// HTTP(S) request to a URL.
    Http,
}

impl CheckKind {
    /// Every kind.
    pub const ALL: [CheckKind; 4] = [
        CheckKind::Dns,
        CheckKind::Tls,
        CheckKind::Tcp,
        CheckKind::Http,
    ];

    /// Label.
    pub fn label(self) -> &'static str {
        match self {
            CheckKind::Dns => "DNS",
            CheckKind::Tls => "TLS",
            CheckKind::Tcp => "TCP",
            CheckKind::Http => "HTTP",
        }
    }

    /// Example target for the input's placeholder.
    pub fn placeholder(self) -> &'static str {
        match self {
            CheckKind::Dns => "example.com",
            CheckKind::Tls => "example.com:443",
            CheckKind::Tcp => "db.internal:5432",
            CheckKind::Http => "https://example.com/health",
        }
    }
}

/// DNS record types offered.
pub const DNS_TYPES: [&str; 7] = ["A/AAAA", "A", "AAAA", "CNAME", "MX", "TXT", "NS"];

/// One saved check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetCheck {
    /// Kind.
    pub kind: CheckKind,
    /// Host name, `host:port` or URL.
    pub target: String,
    /// DNS record type (`A/AAAA`, `MX`…).
    #[serde(default)]
    pub record: Option<String>,
    /// Run on this SSH Host instead of this computer.
    #[serde(default)]
    pub via: Option<ProfileId>,
}

/// A named list of checks (Settings key [`CHECK_LISTS_SETTING`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckList {
    /// Name.
    pub name: String,
    /// Checks in order.
    pub checks: Vec<NetCheck>,
}

/// Where saved check lists live.
pub const CHECK_LISTS_SETTING: &str = "infra.check_lists";

/// Overall verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Fine.
    Ok,
    /// Works, but something needs attention (certificate expiring, HTTP 4xx…).
    Warn,
    /// Failed.
    Fail,
}

/// A check's result, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    /// Verdict.
    pub verdict: Verdict,
    /// One line.
    pub summary: String,
    /// Label / value lines.
    pub details: Vec<(String, String)>,
    /// How long it took.
    pub elapsed_ms: u64,
    /// Days until the leaf certificate expires (TLS).
    pub expires_in_days: Option<i64>,
}

impl CheckResult {
    fn fail(summary: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Fail,
            summary: summary.into(),
            details: Vec::new(),
            elapsed_ms: 0,
            expires_in_days: None,
        }
    }
}

/// `host` and `port` of `host:port`, `[v6]:port` or a bare host (`default` port).
pub fn split_host_port(target: &str, default: u16) -> Result<(String, u16), String> {
    let t = target.trim();
    let t = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let t = t.split('/').next().unwrap_or(t);
    if t.is_empty() {
        return Err("enter a host name".into());
    }
    if let Some(rest) = t.strip_prefix('[') {
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| "an IPv6 address needs a closing ]".to_owned())?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("bad port {p:?}"))?,
            None => default,
        };
        return Ok((h.to_owned(), port));
    }
    match t.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Ok((
            h.to_owned(),
            p.parse().map_err(|_| format!("bad port {p:?}"))?,
        )),
        _ => Ok((t.to_owned(), default)),
    }
}

/// Whether `s` is safe as a host name or IP (letters, digits, `.-_:`), so nothing odd
/// reaches a resolver or a command line.
pub fn valid_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

/// POSIX single-quoting for a command line.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------- DNS

/// One DNS answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsRecord {
    /// Record type.
    pub rtype: String,
    /// Value (address, target, text).
    pub value: String,
    /// TTL in seconds, when known.
    pub ttl: Option<u32>,
}

/// A DNS lookup's answers and where they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsReport {
    /// Name asked for.
    pub name: String,
    /// `system resolver`, `DNS over HTTPS (cloudflare-dns.com)`, `dig on web-1`.
    pub resolver: String,
    /// Answers.
    pub records: Vec<DnsRecord>,
    /// DNS status when not NOERROR (`NXDOMAIN`).
    pub status: Option<String>,
}

fn rtype_name(code: u64) -> String {
    match code {
        1 => "A".into(),
        2 => "NS".into(),
        5 => "CNAME".into(),
        6 => "SOA".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        28 => "AAAA".into(),
        other => format!("TYPE{other}"),
    }
}

/// Answers of a DNS-over-HTTPS JSON reply (`application/dns-json`).
pub fn parse_doh(json: &serde_json::Value) -> (Vec<DnsRecord>, Option<String>) {
    let status = match json.get("Status").and_then(serde_json::Value::as_u64) {
        Some(0) | None => None,
        Some(2) => Some("SERVFAIL".to_owned()),
        Some(3) => Some("NXDOMAIN".to_owned()),
        Some(5) => Some("REFUSED".to_owned()),
        Some(n) => Some(format!("status {n}")),
    };
    let records = json
        .get("Answer")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(DnsRecord {
                rtype: rtype_name(a.get("type")?.as_u64()?),
                value: a.get("data")?.as_str()?.trim_matches('"').to_owned(),
                ttl: a
                    .get("TTL")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|t| u32::try_from(t).ok()),
            })
        })
        .collect();
    (records, status)
}

/// Answers in `dig +noall +answer` output (`name TTL IN TYPE value`), or `host -t` output.
pub fn parse_dig(out: &str) -> Vec<DnsRecord> {
    let mut v = Vec::new();
    for line in out.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 5 && f[2] == "IN" {
            v.push(DnsRecord {
                rtype: f[3].to_owned(),
                value: f[4..].join(" ").trim_matches('"').to_owned(),
                ttl: f[1].parse().ok(),
            });
        } else if let Some((_, addr)) = line.split_once(" has address ") {
            v.push(DnsRecord {
                rtype: "A".into(),
                value: addr.trim().to_owned(),
                ttl: None,
            });
        } else if let Some((_, addr)) = line.split_once(" has IPv6 address ") {
            v.push(DnsRecord {
                rtype: "AAAA".into(),
                value: addr.trim().to_owned(),
                ttl: None,
            });
        } else if let Some((_, rest)) = line.split_once(" mail is handled by ") {
            v.push(DnsRecord {
                rtype: "MX".into(),
                value: rest.trim().to_owned(),
                ttl: None,
            });
        } else if let Some((_, rest)) = line.split_once(" is an alias for ") {
            v.push(DnsRecord {
                rtype: "CNAME".into(),
                value: rest.trim().to_owned(),
                ttl: None,
            });
        } else if let Some((_, rest)) = line.split_once(" descriptive text ") {
            v.push(DnsRecord {
                rtype: "TXT".into(),
                value: rest.trim().trim_matches('"').to_owned(),
                ttl: None,
            });
        } else if let Some((_, rest)) = line.split_once(" name server ") {
            v.push(DnsRecord {
                rtype: "NS".into(),
                value: rest.trim().to_owned(),
                ttl: None,
            });
        }
    }
    v
}

/// Addresses in `getent ahosts` output (one per address, deduplicated).
pub fn parse_getent(out: &str) -> Vec<DnsRecord> {
    let mut v: Vec<DnsRecord> = Vec::new();
    for line in out.lines() {
        let Some(ip) = line.split_whitespace().next() else {
            continue;
        };
        let Ok(addr) = ip.parse::<std::net::IpAddr>() else {
            continue;
        };
        if v.iter().any(|r| r.value == ip) {
            continue;
        }
        v.push(DnsRecord {
            rtype: if addr.is_ipv4() { "A" } else { "AAAA" }.into(),
            value: ip.to_owned(),
            ttl: None,
        });
    }
    v
}

/// A DNS report as a result.
pub fn format_dns(r: &DnsReport) -> CheckResult {
    let details: Vec<(String, String)> = r
        .records
        .iter()
        .map(|rec| {
            let ttl = rec.ttl.map(|t| format!("  (TTL {t}s)")).unwrap_or_default();
            (rec.rtype.clone(), format!("{}{ttl}", rec.value))
        })
        .chain(std::iter::once(("Resolver".to_owned(), r.resolver.clone())))
        .collect();
    let (verdict, summary) = match (&r.status, r.records.len()) {
        (Some(s), _) => (Verdict::Fail, format!("{}: {s}", r.name)),
        (None, 0) => (Verdict::Warn, format!("{}: no records", r.name)),
        (None, n) => {
            let first: Vec<&str> = r.records.iter().take(3).map(|x| x.value.as_str()).collect();
            let more = if n > 3 {
                format!(" +{}", n - 3)
            } else {
                String::new()
            };
            (
                Verdict::Ok,
                format!("{} → {}{more}", r.name, first.join(", ")),
            )
        }
    };
    CheckResult {
        verdict,
        summary,
        details,
        elapsed_ms: 0,
        expires_in_days: None,
    }
}

// ---------------------------------------------------------------------- TLS

/// What the TLS check learned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsReport {
    /// `host:port`.
    pub target: String,
    /// `TLSv1.3`.
    pub protocol: Option<String>,
    /// Cipher suite.
    pub cipher: Option<String>,
    /// Chain as sent, leaf first.
    pub chain: Vec<CertInfo>,
    /// `None` when verification passed, else why it failed.
    pub verify_error: Option<String>,
    /// The handshake itself failed.
    pub handshake_error: Option<String>,
}

/// Whole days from `now_ms` until `until_ms` (negative once past).
pub fn days_until(until_ms: i64, now_ms: i64) -> i64 {
    (until_ms - now_ms).div_euclid(86_400_000)
}

fn date(ms: i64) -> String {
    switchyard_cloud::display_ms(ms)
        .trim_end_matches(" UTC")
        .get(0..10)
        .unwrap_or_default()
        .to_owned()
}

/// A TLS report as a result, judged at `now_ms`.
pub fn format_tls(r: &TlsReport, now_ms: i64) -> CheckResult {
    if let Some(e) = &r.handshake_error {
        return CheckResult::fail(format!("{}: {e}", r.target));
    }
    let Some(leaf) = r.chain.first() else {
        return CheckResult::fail(format!("{}: the server sent no certificate", r.target));
    };
    let days = leaf.not_after_ms.map(|t| days_until(t, now_ms));
    let mut details = vec![
        ("Subject".to_owned(), leaf.subject.clone()),
        ("Issuer".to_owned(), leaf.issuer.clone()),
        ("Names".to_owned(), leaf.sans.join(", ")),
    ];
    if let (Some(a), Some(b)) = (leaf.not_before_ms, leaf.not_after_ms) {
        details.push(("Valid".to_owned(), format!("{} → {}", date(a), date(b))));
    }
    if let Some(p) = &r.protocol {
        details.push(("Protocol".to_owned(), p.clone()));
    }
    if let Some(c) = &r.cipher {
        details.push(("Cipher".to_owned(), c.clone()));
    }
    for (i, c) in r.chain.iter().enumerate().skip(1) {
        details.push((
            format!("Chain {i}"),
            format!("{} (by {})", c.subject, c.issuer),
        ));
    }
    details.push((
        "Verification".to_owned(),
        r.verify_error.clone().unwrap_or_else(|| "OK".to_owned()),
    ));
    let expiry = match days {
        Some(d) if d < 0 => format!("expired {} day{} ago", -d, if d == -1 { "" } else { "s" }),
        Some(d) => format!("expires in {d} day{}", if d == 1 { "" } else { "s" }),
        None => "no expiry date".to_owned(),
    };
    let verdict = if r.verify_error.is_some() || days.is_some_and(|d| d < 0) {
        Verdict::Fail
    } else if days.is_some_and(|d| d < EXPIRY_WARN_DAYS) {
        Verdict::Warn
    } else {
        Verdict::Ok
    };
    let mut summary = format!("{} · {expiry}", leaf.subject.trim_start_matches("CN="));
    if let Some(e) = &r.verify_error {
        summary.push_str(&format!(" · not trusted: {e}"));
    }
    CheckResult {
        verdict,
        summary,
        details,
        elapsed_ms: 0,
        expires_in_days: days,
    }
}

/// What `openssl s_client -showcerts` printed: protocol, cipher, chain and verify result.
pub fn parse_s_client(out: &str, target: &str) -> TlsReport {
    let mut r = TlsReport {
        target: target.to_owned(),
        ..TlsReport::default()
    };
    for line in out.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Protocol") {
            if let Some(v) = rest.split(':').nth(1) {
                r.protocol = Some(v.trim().to_owned());
            }
        } else if let Some(rest) = line
            .strip_prefix("Cipher    :")
            .or_else(|| line.strip_prefix("Cipher :"))
        {
            r.cipher = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("New, ") {
            // `New, TLSv1.3, Cipher is TLS_AES_256_GCM_SHA384`
            let mut parts = rest.split(", Cipher is ");
            if let (Some(p), Some(c)) = (parts.next(), parts.next()) {
                r.protocol.get_or_insert_with(|| p.trim().to_owned());
                r.cipher.get_or_insert_with(|| c.trim().to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("Verify return code:") {
            let rest = rest.trim();
            if !rest.starts_with("0 ") {
                r.verify_error = Some(rest.to_owned());
            }
        }
    }
    r.chain = <rustls::pki_types::CertificateDer<'_> as rustls::pki_types::pem::PemObject>::pem_slice_iter(out.as_bytes())
        .filter_map(Result::ok)
        .filter_map(|c| parse_cert(&c))
        .collect();
    if r.chain.is_empty() {
        let err = out
            .lines()
            .find(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("errno") || l.contains("error") || l.contains("refused")
            })
            .unwrap_or("no certificate (is it a TLS port?)");
        r.handshake_error = Some(err.trim().to_owned());
    }
    r
}

/// Records the presented chain and the verification result, and accepts the server either
/// way so the handshake completes and the certificate can be shown.
#[derive(Debug)]
struct Recorder {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    seen: Mutex<(Vec<Vec<u8>>, Option<String>)>,
}

impl rustls::client::danger::ServerCertVerifier for Recorder {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let verdict = self
            .inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            .err()
            .map(|e| e.to_string());
        let mut chain = vec![end_entity.to_vec()];
        chain.extend(intermediates.iter().map(|c| c.to_vec()));
        if let Ok(mut s) = self.seen.lock() {
            *s = (chain, verdict);
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Connect to `host:port` and report the certificate chain, protocol and whether the
/// chain verifies against the system's roots. Nothing is sent after the handshake.
pub async fn tls_local(host: &str, port: u16) -> TlsReport {
    let target = format!("{host}:{port}");
    let mut report = TlsReport {
        target: target.clone(),
        ..TlsReport::default()
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let inner = match rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    {
        Ok(v) => v,
        Err(e) => {
            report.handshake_error = Some(format!("no trusted roots: {e}"));
            return report;
        }
    };
    let recorder = Arc::new(Recorder {
        inner,
        seen: Mutex::new((Vec::new(), None)),
    });
    let config = match rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
    {
        Ok(b) => b
            .dangerous()
            .with_custom_certificate_verifier(recorder.clone())
            .with_no_client_auth(),
        Err(e) => {
            report.handshake_error = Some(e.to_string());
            return report;
        }
    };
    let name = match rustls::pki_types::ServerName::try_from(host.to_owned()) {
        Ok(n) => n,
        Err(e) => {
            report.handshake_error = Some(format!("bad host name: {e}"));
            return report;
        }
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let run = async {
        let tcp = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("handshake: {e}"))
    };
    match tokio::time::timeout(STEP_TIMEOUT, run).await {
        Err(_) => report.handshake_error = Some("timed out".into()),
        Ok(Err(e)) => report.handshake_error = Some(e),
        Ok(Ok(stream)) => {
            let (_, conn) = stream.get_ref();
            report.protocol = conn.protocol_version().map(|v| {
                format!("{v:?}")
                    .replace("TLSv1_3", "TLSv1.3")
                    .replace("TLSv1_2", "TLSv1.2")
            });
            report.cipher = conn
                .negotiated_cipher_suite()
                .map(|c| format!("{:?}", c.suite()));
        }
    }
    if let Ok(s) = recorder.seen.lock() {
        report.chain = s.0.iter().filter_map(|d| parse_cert(d)).collect();
        report.verify_error = s.1.clone();
    }
    // A failed handshake after the certificate arrived still shows it.
    if !report.chain.is_empty()
        && report
            .handshake_error
            .as_deref()
            .is_some_and(|e| e.starts_with("handshake"))
    {
        report
            .verify_error
            .get_or_insert(report.handshake_error.take().unwrap_or_default());
    }
    report
}

// ---------------------------------------------------------------------- TCP

/// A TCP connect attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpReport {
    /// `host:port`.
    pub target: String,
    /// Address connected to.
    pub address: Option<String>,
    /// Connect time, or why it failed.
    pub result: Result<u64, String>,
}

/// A TCP report as a result.
pub fn format_tcp(r: &TcpReport) -> CheckResult {
    match &r.result {
        Ok(ms) => CheckResult {
            verdict: Verdict::Ok,
            summary: format!("{} open · {ms} ms", r.target),
            details: r
                .address
                .iter()
                .map(|a| ("Address".to_owned(), a.clone()))
                .chain(std::iter::once(("Connect".to_owned(), format!("{ms} ms"))))
                .collect(),
            elapsed_ms: *ms,
            expires_in_days: None,
        },
        Err(e) => CheckResult::fail(format!("{}: {e}", r.target)),
    }
}

/// Connect to `host:port` from here.
pub async fn tcp_local(host: &str, port: u16) -> TcpReport {
    let target = format!("{host}:{port}");
    let started = Instant::now();
    match tokio::time::timeout(STEP_TIMEOUT, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(s)) => TcpReport {
            target,
            address: s.peer_addr().ok().map(|a| a.to_string()),
            result: Ok(started.elapsed().as_millis() as u64),
        },
        Ok(Err(e)) => TcpReport {
            target,
            address: None,
            result: Err(e.to_string()),
        },
        Err(_) => TcpReport {
            target,
            address: None,
            result: Err(format!("no answer in {} s", STEP_TIMEOUT.as_secs())),
        },
    }
}

/// The shell command testing `host:port` on a Host; prints `SWY_TCP <status> <ms>`.
pub fn tcp_command(host: &str, port: u16) -> String {
    let h = shell_quote(host);
    format!(
        "s=$(date +%s%N); if command -v nc >/dev/null 2>&1; then nc -z -w 5 {h} {port}; \
         else timeout 5 bash -c 'exec 3<>/dev/tcp/'{h}'/{port}'; fi 2>&1; r=$?; \
         e=$(date +%s%N); echo \"SWY_TCP $r $(( (e - s) / 1000000 ))\""
    )
}

/// The result of [`tcp_command`]'s output.
pub fn parse_tcp_output(out: &str, target: &str) -> TcpReport {
    let marker = out.lines().find_map(|l| l.trim().strip_prefix("SWY_TCP "));
    let result = match marker.map(|m| m.split_whitespace().collect::<Vec<_>>()) {
        Some(f) if f.first() == Some(&"0") => {
            Ok(f.get(1).and_then(|m| m.parse().ok()).unwrap_or(0))
        }
        Some(_) => {
            let msg = out
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with("SWY_TCP"))
                .unwrap_or("closed or filtered");
            Err(msg.to_owned())
        }
        None => Err("the Host printed no result".to_owned()),
    };
    TcpReport {
        target: target.to_owned(),
        address: None,
        result,
    }
}

// ---------------------------------------------------------------------- HTTP

/// One request of an HTTP check (redirects make several).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpHop {
    /// URL requested.
    pub url: String,
    /// Status code.
    pub status: u16,
    /// Time to the response headers, ms.
    pub ms: u64,
}

/// An HTTP check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpReport {
    /// URL asked for.
    pub url: String,
    /// Each request, redirects first.
    pub hops: Vec<HttpHop>,
    /// Headers of the last response.
    pub headers: Vec<(String, String)>,
    /// Total time, ms.
    pub total_ms: u64,
    /// Timings from curl (`DNS 3 ms · connect 10 ms…`), when run on a Host.
    pub timings: Option<String>,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// Headers shown in the result (the rest are counted).
const SHOWN_HEADERS: [&str; 8] = [
    "server",
    "content-type",
    "content-length",
    "cache-control",
    "strict-transport-security",
    "location",
    "x-powered-by",
    "via",
];

/// An HTTP report as a result.
pub fn format_http(r: &HttpReport) -> CheckResult {
    if let Some(e) = &r.error
        && r.hops.is_empty()
    {
        return CheckResult::fail(format!("{}: {e}", r.url));
    }
    let Some(last) = r.hops.last() else {
        return CheckResult::fail(format!("{}: no response", r.url));
    };
    let mut details: Vec<(String, String)> = r
        .hops
        .iter()
        .enumerate()
        .map(|(i, h)| {
            (
                format!("#{}", i + 1),
                format!("{} {} · {} ms", h.status, h.url, h.ms),
            )
        })
        .collect();
    if let Some(t) = &r.timings {
        details.push(("Timing".to_owned(), t.clone()));
    }
    for (k, v) in &r.headers {
        if SHOWN_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
            details.push((k.clone(), v.clone()));
        }
    }
    if let Some(e) = &r.error {
        details.push(("Error".to_owned(), e.clone()));
    }
    let redirects = r.hops.len() - 1;
    let verdict = match last.status {
        200..=399 if r.error.is_none() => Verdict::Ok,
        400..=499 => Verdict::Warn,
        _ => Verdict::Fail,
    };
    let mut summary = format!("{} · {} ms", last.status, r.total_ms);
    if redirects > 0 {
        summary.push_str(&format!(
            " · {redirects} redirect{} → {}",
            if redirects == 1 { "" } else { "s" },
            last.url
        ));
    }
    CheckResult {
        verdict,
        summary,
        details,
        elapsed_ms: r.total_ms,
        expires_in_days: None,
    }
}

/// Request `url` from here, following up to 10 redirects one at a time.
pub async fn http_local(url: &str) -> HttpReport {
    let mut report = HttpReport {
        url: url.to_owned(),
        ..HttpReport::default()
    };
    let client = switchyard_db::tls::client_config()
        .map_err(|e| e.to_string())
        .and_then(|tls| {
            reqwest::Client::builder()
                .tls_backend_preconfigured(tls)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(STEP_TIMEOUT)
                .timeout(Duration::from_secs(30))
                .user_agent(format!("Switchyard/{} (check)", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| e.to_string())
        });
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            report.error = Some(e);
            return report;
        }
    };
    let started = Instant::now();
    let mut current = match reqwest::Url::parse(url.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        Ok(_) => {
            report.error = Some("only http:// and https:// URLs".into());
            return report;
        }
        Err(e) => {
            report.error = Some(format!("bad URL: {e}"));
            return report;
        }
    };
    for _ in 0..=10 {
        let hop_start = Instant::now();
        let resp = match client.get(current.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                let mut cur: &dyn std::error::Error = &e;
                while let Some(next) = cur.source() {
                    cur = next;
                }
                report.error = Some(cur.to_string());
                break;
            }
        };
        let status = resp.status().as_u16();
        report.hops.push(HttpHop {
            url: current.to_string(),
            status,
            ms: hop_start.elapsed().as_millis() as u64,
        });
        report.headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("(binary)").to_owned()))
            .collect();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .and_then(|l| current.join(l).ok());
        match (status, location) {
            (300..=399, Some(next)) => current = next,
            _ => break,
        }
    }
    if report.hops.len() > 10 {
        report.error = Some("more than 10 redirects".into());
    }
    report.total_ms = started.elapsed().as_millis() as u64;
    report
}

/// The `curl` command for an HTTP check on a Host: response headers of every hop, then
/// `SWY_HTTP <code> <dns> <connect> <tls> <ttfb> <total> <redirects> <url>`.
pub fn http_command(url: &str) -> String {
    format!(
        "curl -sS -o /dev/null -D - -L --max-redirs 10 --max-time 30 \
         -w 'SWY_HTTP %{{http_code}} %{{time_namelookup}} %{{time_connect}} %{{time_appconnect}} \
         %{{time_starttransfer}} %{{time_total}} %{{num_redirects}} %{{url_effective}}\\n' -- {} 2>&1",
        shell_quote(url.trim())
    )
}

fn secs_ms(s: &str) -> u64 {
    s.parse::<f64>()
        .map(|v| (v * 1000.0).round() as u64)
        .unwrap_or(0)
}

/// The result of [`http_command`]'s output.
pub fn parse_curl(out: &str, url: &str) -> HttpReport {
    let mut r = HttpReport {
        url: url.to_owned(),
        ..HttpReport::default()
    };
    let mut blocks: Vec<(u16, Vec<(String, String)>)> = Vec::new();
    let mut marker = None;
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(m) = line.strip_prefix("SWY_HTTP ") {
            marker = Some(m.to_owned());
        } else if line.starts_with("HTTP/") {
            let code = line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            blocks.push((code, Vec::new()));
        } else if let Some((k, v)) = line.split_once(':')
            && let Some(b) = blocks.last_mut()
            && !k.contains(' ')
        {
            b.1.push((k.trim().to_owned(), v.trim().to_owned()));
        } else if line.starts_with("curl:") {
            r.error = Some(line.trim_start_matches("curl:").trim().to_owned());
        }
    }
    // 1xx interim responses are not hops.
    blocks.retain(|(c, _)| *c >= 200);
    let mut hop_url = url.trim().to_owned();
    let next_url = |headers: &[(String, String)], base: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .and_then(|(_, v)| {
                reqwest::Url::parse(base)
                    .ok()
                    .and_then(|b| b.join(v).ok())
                    .map(|u| u.to_string())
            })
    };
    for (code, headers) in &blocks {
        r.hops.push(HttpHop {
            url: hop_url.clone(),
            status: *code,
            ms: 0,
        });
        if let Some(n) = next_url(headers, &hop_url) {
            hop_url = n;
        }
    }
    if let Some(last) = blocks.last() {
        r.headers = last.1.clone();
    }
    if let Some(m) = marker {
        let f: Vec<&str> = m.split_whitespace().collect();
        if f.len() >= 8 {
            let (dns, conn, tls, ttfb, total) = (
                secs_ms(f[1]),
                secs_ms(f[2]),
                secs_ms(f[3]),
                secs_ms(f[4]),
                secs_ms(f[5]),
            );
            r.total_ms = total;
            let mut t = format!("DNS {dns} ms · connect {conn} ms");
            if tls > 0 {
                t.push_str(&format!(" · TLS {tls} ms"));
            }
            t.push_str(&format!(" · first byte {ttfb} ms"));
            r.timings = Some(t);
            if let Some(last) = r.hops.last_mut() {
                last.url = f[7].to_owned();
                last.ms = ttfb;
            }
            if f[0] == "000" && r.error.is_none() {
                r.error = Some("no response".into());
            }
        }
    }
    r
}

// ---------------------------------------------------------------------- SSH DNS

/// The shell command for a DNS lookup on a Host.
pub fn dns_command(name: &str, record: &str) -> String {
    let n = shell_quote(name);
    match record {
        "A/AAAA" | "" => format!("getent ahosts {n} 2>&1 || echo 'SWY_DNS none'"),
        t => {
            let t = shell_quote(t);
            format!(
                "if command -v dig >/dev/null 2>&1; then dig +noall +answer -t {t} {n}; \
                 elif command -v host >/dev/null 2>&1; then host -t {t} {n}; \
                 else echo 'SWY_DNS no dig or host on this Host'; fi 2>&1"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_2026_06_01: i64 = 1_780_272_000_000;

    #[test]
    fn host_port_and_quoting() {
        assert_eq!(
            split_host_port("a.b:8443", 443).unwrap(),
            ("a.b".into(), 8443)
        );
        assert_eq!(
            split_host_port("https://a.b/x", 443).unwrap(),
            ("a.b".into(), 443)
        );
        assert_eq!(
            split_host_port("[::1]:5432", 1).unwrap(),
            ("::1".into(), 5432)
        );
        assert_eq!(split_host_port("::1", 22).unwrap(), ("::1".into(), 22));
        assert!(split_host_port("a:b", 1).is_err());
        assert!(valid_host("db-1.internal"));
        assert!(!valid_host("a;rm -rf"));
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn dns_answers() {
        let (recs, status) = parse_doh(&serde_json::json!({
            "Status": 0,
            "Answer": [
                {"name": "example.com.", "type": 15, "TTL": 300, "data": "10 mx1.example.com."},
                {"name": "example.com.", "type": 16, "TTL": 60, "data": "\"v=spf1 -all\""}
            ]
        }));
        assert!(status.is_none());
        assert_eq!(recs[0].rtype, "MX");
        assert_eq!(recs[1].value, "v=spf1 -all");
        let (_, status) = parse_doh(&serde_json::json!({"Status": 3}));
        assert_eq!(status.as_deref(), Some("NXDOMAIN"));
        let dig = parse_dig(
            "example.com.\t\t300\tIN\tMX\t10 mx1.example.com.\nwww.example.com. 60 IN CNAME example.com.\n",
        );
        assert_eq!(dig.len(), 2);
        assert_eq!(dig[0].ttl, Some(300));
        assert_eq!(dig[1].rtype, "CNAME");
        let host = parse_dig("example.com mail is handled by 10 mx1.example.com.\n");
        assert_eq!(host[0].value, "10 mx1.example.com.");
        let ge = parse_getent(
            "93.184.215.14   STREAM example.com\n93.184.215.14   DGRAM\n2606:2800:21f:cb07:6820:80da:af6b:8b2c STREAM\n",
        );
        assert_eq!(ge.len(), 2);
        assert_eq!(ge[1].rtype, "AAAA");
        let report = DnsReport {
            name: "example.com".into(),
            resolver: "system resolver".into(),
            records: ge,
            status: None,
        };
        let r = format_dns(&report);
        assert_eq!(r.verdict, Verdict::Ok);
        assert_eq!(
            r.summary,
            "example.com → 93.184.215.14, 2606:2800:21f:cb07:6820:80da:af6b:8b2c"
        );
        let empty = format_dns(&DnsReport {
            records: vec![],
            ..report
        });
        assert_eq!(empty.verdict, Verdict::Warn);
    }

    #[test]
    fn tls_from_s_client() {
        let pem = super::super::x509::tests::PEM;
        let out = format!(
            "CONNECTED(00000003)\n---\nCertificate chain\n 0 s:CN = example.test\n{pem}---\n\
             New, TLSv1.3, Cipher is TLS_AES_256_GCM_SHA384\n\
             Verify return code: 18 (self-signed certificate)\n"
        );
        let r = parse_s_client(&out, "example.test:443");
        assert_eq!(r.protocol.as_deref(), Some("TLSv1.3"));
        assert_eq!(r.cipher.as_deref(), Some("TLS_AES_256_GCM_SHA384"));
        assert_eq!(r.chain.len(), 1);
        assert_eq!(
            r.verify_error.as_deref(),
            Some("18 (self-signed certificate)")
        );
        let res = format_tls(&r, NOW_2026_06_01);
        assert_eq!(res.verdict, Verdict::Fail);
        assert_eq!(res.expires_in_days, Some(214));
        assert!(
            res.summary
                .starts_with("example.test, O=Switchyard Test · expires in 214 days")
        );
        assert!(
            res.details
                .contains(&("Valid".into(), "2026-01-01 → 2027-01-01".into()))
        );
        // Trusted and close to expiry: a warning.
        let ok = TlsReport {
            verify_error: None,
            ..r.clone()
        };
        let soon = format_tls(&ok, 1_797_552_000_000);
        assert_eq!(soon.verdict, Verdict::Warn);
        assert_eq!(soon.expires_in_days, Some(14));
        let expired = format_tls(&ok, 1_800_000_000_000);
        assert_eq!(expired.verdict, Verdict::Fail);
        assert!(
            expired.summary.contains("expired 15 days ago"),
            "{}",
            expired.summary
        );
        let refused = parse_s_client("connect: Connection refused\nconnect:errno=111\n", "x:443");
        assert!(format_tls(&refused, 0).summary.contains("refused"));
    }

    #[test]
    fn tcp_results() {
        let ok = parse_tcp_output("SWY_TCP 0 12\n", "db:5432");
        assert_eq!(ok.result, Ok(12));
        assert_eq!(format_tcp(&ok).summary, "db:5432 open · 12 ms");
        let bad = parse_tcp_output(
            "nc: connect to db port 5432 (tcp) failed: Connection refused\nSWY_TCP 1 3\n",
            "db:5432",
        );
        assert_eq!(format_tcp(&bad).verdict, Verdict::Fail);
        assert!(format_tcp(&bad).summary.contains("Connection refused"));
        assert!(tcp_command("db'x", 1).contains(r"'db'\''x'"));
    }

    #[test]
    fn http_from_curl() {
        let out = "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/\r\nServer: nginx\r\n\r\n\
                   HTTP/2 200\r\ncontent-type: text/html\r\nserver: ECS\r\n\r\n\
                   SWY_HTTP 200 0.004 0.020 0.051 0.120 0.130 1 https://example.com/\n";
        let r = parse_curl(out, "http://example.com");
        assert_eq!(r.hops.len(), 2);
        assert_eq!(r.hops[0].status, 301);
        assert_eq!(r.hops[1].url, "https://example.com/");
        assert_eq!(r.total_ms, 130);
        let res = format_http(&r);
        assert_eq!(res.verdict, Verdict::Ok);
        assert_eq!(
            res.summary,
            "200 · 130 ms · 1 redirect → https://example.com/"
        );
        assert!(
            res.details
                .contains(&("content-type".into(), "text/html".into()))
        );
        assert!(
            res.details
                .iter()
                .any(|(k, v)| k == "Timing" && v.contains("TLS 51 ms"))
        );
        let fail = parse_curl(
            "curl: (6) Could not resolve host: nope.invalid\nSWY_HTTP 000 0 0 0 0 0.01 0 http://nope.invalid/\n",
            "http://nope.invalid",
        );
        assert_eq!(format_http(&fail).verdict, Verdict::Fail);
        assert!(
            format_http(&fail)
                .summary
                .contains("Could not resolve host")
        );
        let nf = HttpReport {
            url: "u".into(),
            hops: vec![HttpHop {
                url: "u".into(),
                status: 404,
                ms: 5,
            }],
            total_ms: 5,
            ..HttpReport::default()
        };
        assert_eq!(format_http(&nf).verdict, Verdict::Warn);
    }

    #[test]
    fn check_lists_round_trip() {
        let l = vec![CheckList {
            name: "prod".into(),
            checks: vec![NetCheck {
                kind: CheckKind::Tls,
                target: "example.com:443".into(),
                record: None,
                via: None,
            }],
        }];
        let json = serde_json::to_value(&l).unwrap();
        assert_eq!(json[0]["checks"][0]["kind"], "tls");
        let back: Vec<CheckList> = serde_json::from_value(json).unwrap();
        assert_eq!(back, l);
    }
}
