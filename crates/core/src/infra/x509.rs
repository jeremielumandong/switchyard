//! Just enough DER to show a certificate: subject and issuer common name / organization,
//! validity and subject alternative names. Nothing here verifies anything; rustls does
//! that (or `openssl s_client` on a Host).

/// What the TLS check shows of one certificate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CertInfo {
    /// Subject (`CN=…, O=…`).
    pub subject: String,
    /// Issuer (`CN=…, O=…`).
    pub issuer: String,
    /// DNS names and IP addresses it is valid for.
    pub sans: Vec<String>,
    /// Valid from, ms since the epoch.
    pub not_before_ms: Option<i64>,
    /// Valid until, ms since the epoch.
    pub not_after_ms: Option<i64>,
    /// Serial number, hex.
    pub serial: String,
}

/// One TLV: tag and contents.
struct Tlv<'a> {
    tag: u8,
    body: &'a [u8],
}

/// Read one TLV from the front of `b`, returning it and the rest.
fn tlv(b: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)?;
    let (len, hdr) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let body = b.get(hdr..hdr.checked_add(len)?)?;
    Some((Tlv { tag, body }, &b[hdr + len..]))
}

/// Every TLV inside `b`.
fn children(mut b: &[u8]) -> Vec<Tlv<'_>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let Some((t, rest)) = tlv(b) else { break };
        out.push(t);
        b = rest;
    }
    out
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_O: &[u8] = &[0x55, 0x04, 0x0a];
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];

/// `CN=…, O=…` of a Name.
fn name(b: &[u8]) -> String {
    let mut cn = None;
    let mut o = None;
    for set in children(b) {
        for atv in children(set.body) {
            let parts = children(atv.body);
            let (Some(oid), Some(val)) = (parts.first(), parts.get(1)) else {
                continue;
            };
            let text = String::from_utf8_lossy(val.body).into_owned();
            if oid.body == OID_CN {
                cn = Some(text);
            } else if oid.body == OID_O {
                o = Some(text);
            }
        }
    }
    match (cn, o) {
        (Some(c), Some(o)) => format!("CN={c}, O={o}"),
        (Some(c), None) => format!("CN={c}"),
        (None, Some(o)) => format!("O={o}"),
        (None, None) => String::new(),
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`) as ms.
fn time(t: &Tlv<'_>) -> Option<i64> {
    let s = std::str::from_utf8(t.body).ok()?;
    let (y, rest) = match t.tag {
        0x17 => {
            let yy: i64 = s.get(0..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let n = |a: usize| rest.get(a..a + 2).and_then(|x| x.parse::<i64>().ok());
    let (mo, d, h, mi) = (n(0)?, n(2)?, n(4)?, n(6)?);
    let sec = n(8).unwrap_or(0);
    let days = days_from_civil(y, u32::try_from(mo).ok()?, u32::try_from(d).ok()?);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000)
}

fn sans(ext: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    // extnValue OCTET STRING → GeneralNames SEQUENCE.
    let Some((seq, _)) = tlv(ext) else {
        return out;
    };
    for gn in children(seq.body) {
        match gn.tag {
            0x82 => out.push(String::from_utf8_lossy(gn.body).into_owned()),
            0x87 if gn.body.len() == 4 => out.push(
                gn.body
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join("."),
            ),
            0x87 if gn.body.len() == 16 => {
                let mut a = [0u8; 16];
                a.copy_from_slice(gn.body);
                out.push(std::net::Ipv6Addr::from(a).to_string());
            }
            _ => {}
        }
    }
    out
}

/// Parse a DER certificate (`None` when it is not one).
pub fn parse_cert(der: &[u8]) -> Option<CertInfo> {
    let (cert, _) = tlv(der)?;
    let parts = children(cert.body);
    let tbs = children(parts.first()?.body);
    let mut i = 0;
    // Optional [0] version.
    if tbs.first()?.tag == 0xa0 {
        i += 1;
    }
    let serial = tbs.get(i)?;
    let issuer = tbs.get(i + 2)?;
    let validity = children(tbs.get(i + 3)?.body);
    let subject = tbs.get(i + 4)?;
    let mut info = CertInfo {
        subject: name(subject.body),
        issuer: name(issuer.body),
        sans: Vec::new(),
        not_before_ms: validity.first().and_then(time),
        not_after_ms: validity.get(1).and_then(time),
        serial: serial.body.iter().map(|b| format!("{b:02x}")).collect(),
    };
    for t in &tbs[i + 5..] {
        if t.tag != 0xa3 {
            continue;
        }
        let Some((exts, _)) = tlv(t.body) else {
            continue;
        };
        for ext in children(exts.body) {
            let f = children(ext.body);
            if f.first().is_some_and(|o| o.body == OID_SAN)
                && let Some(v) = f.iter().rev().find(|x| x.tag == 0x04)
            {
                info.sans = sans(v.body);
            }
        }
    }
    Some(info)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A self-signed certificate for `example.test` / `www.example.test` / 127.0.0.1,
    /// valid 2026-01-01 to 2027-01-01 (made with `openssl req -x509`).
    pub(crate) const PEM: &str = include_str!("testdata/example.test.pem");

    pub(crate) fn der() -> Vec<u8> {
        <rustls::pki_types::CertificateDer<'_> as rustls::pki_types::pem::PemObject>::pem_slice_iter(PEM.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .to_vec()
    }

    #[test]
    fn reads_names_dates_and_sans() {
        let c = parse_cert(&der()).unwrap();
        assert_eq!(c.subject, "CN=example.test, O=Switchyard Test");
        assert_eq!(c.issuer, "CN=example.test, O=Switchyard Test");
        assert_eq!(c.sans, ["example.test", "www.example.test", "127.0.0.1"]);
        assert_eq!(c.not_before_ms, Some(1_767_225_600_000));
        assert_eq!(c.not_after_ms, Some(1_798_761_600_000));
        assert!(parse_cert(b"\x30\x03\x02\x01").is_none());
    }
}
