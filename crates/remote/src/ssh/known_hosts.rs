//! Host key verification against OpenSSH `known_hosts` files.
//!
//! Two files are consulted: the user's `~/.ssh/known_hosts` (read only; Switchyard never
//! writes it) and Switchyard's own file, where keys the user trusts are added. Unlike a
//! strict parser, lines that cannot be read (unknown key types, markers) are skipped one
//! by one, so one odd line cannot hide a changed key.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey, parse_public_key_base64};
use sha1::Sha1;

use crate::ssh::SshError;

/// Outcome of checking a server key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// A stored key matches.
    Known,
    /// No key of this algorithm is stored for the host.
    Unknown,
    /// A different key of the same algorithm is stored: possible interception.
    Changed {
        /// Fingerprint of the stored key.
        stored: String,
        /// File and line that holds it.
        location: String,
    },
    /// The host is marked `@revoked` for this key.
    Revoked,
}

/// The two files consulted.
#[derive(Clone, Debug)]
pub struct KnownHosts {
    /// The user's OpenSSH file (read only). `None` skips it (tests).
    pub user_file: Option<PathBuf>,
    /// Switchyard's own file (read and appended).
    pub app_file: PathBuf,
}

/// `SHA256:…` fingerprint of a key.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The `known_hosts` host field for a host and port.
fn host_entry(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

fn pattern_matches(entry: &str, pattern: &str) -> bool {
    // `*` and `?` wildcards, as OpenSSH allows them in unhashed patterns.
    fn wild(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => wild(&p[1..], s) || (!s.is_empty() && wild(p, &s[1..])),
            (Some(b'?'), Some(_)) => wild(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => wild(&p[1..], &s[1..]),
            _ => false,
        }
    }
    if let Some(hashed) = pattern.strip_prefix("|1|") {
        let mut parts = hashed.split('|');
        let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(salt), Ok(hash)) = (
            BASE64.decode(salt.as_bytes()),
            BASE64.decode(hash.as_bytes()),
        ) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
            return false;
        };
        mac.update(entry.as_bytes());
        return mac.verify_slice(&hash).is_ok();
    }
    wild(pattern.as_bytes(), entry.as_bytes())
}

/// Whether a comma-separated host list matches, honoring `!negated` patterns.
fn hosts_match(entry: &str, list: &str) -> bool {
    let mut matched = false;
    for p in list.split(',') {
        if let Some(neg) = p.strip_prefix('!') {
            if pattern_matches(entry, neg) {
                return false;
            }
        } else if pattern_matches(entry, p) {
            matched = true;
        }
    }
    matched
}

struct Line {
    number: usize,
    revoked: bool,
    key: PublicKey,
}

fn matching_lines(path: &Path, entry: &str) -> Vec<Line> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut first = fields.next().unwrap_or_default();
        let mut revoked = false;
        if first.starts_with('@') {
            match first {
                "@revoked" => revoked = true,
                // @cert-authority lines describe CAs, not host keys.
                _ => continue,
            }
            first = fields.next().unwrap_or_default();
        }
        let (Some(_algo), Some(b64)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !hosts_match(entry, first) {
            continue;
        }
        if let Ok(key) = parse_public_key_base64(b64) {
            out.push(Line {
                number: i + 1,
                revoked,
                key,
            });
        }
    }
    out
}

impl KnownHosts {
    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.user_file.iter().chain(std::iter::once(&self.app_file))
    }

    /// Check `key` for `host:port`. The address the user typed and, when different, the
    /// resolved IP are both checked by the caller.
    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let entry = host_entry(host, port);
        // A key the user explicitly trusted in Switchyard wins over an older entry in
        // their own file (that is how "replace stored key" works without editing it).
        if matching_lines(&self.app_file, &entry)
            .iter()
            .any(|l| !l.revoked && l.key.key_data() == key.key_data())
        {
            return HostKeyStatus::Known;
        }
        let mut known = false;
        for file in self.files() {
            for line in matching_lines(file, &entry) {
                let same_algo = line.key.algorithm() == key.algorithm();
                let same_key = line.key.key_data() == key.key_data();
                if line.revoked && same_key {
                    return HostKeyStatus::Revoked;
                }
                if line.revoked {
                    continue;
                }
                if same_key {
                    known = true;
                } else if same_algo {
                    return HostKeyStatus::Changed {
                        stored: fingerprint(&line.key),
                        location: format!("{}:{}", file.display(), line.number),
                    };
                }
            }
        }
        if known {
            HostKeyStatus::Known
        } else {
            HostKeyStatus::Unknown
        }
    }

    /// Trust `key` for `host:port` from now on (appends to Switchyard's file).
    pub fn trust(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), SshError> {
        if let Some(dir) = self.app_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| SshError::Io(e.to_string()))?;
        }
        let openssh = key.to_openssh().map_err(|e| SshError::Io(e.to_string()))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.app_file)
            .map_err(|e| SshError::Io(e.to_string()))?;
        // Keep only "algorithm base64" (drop any comment).
        let mut parts = openssh.split_whitespace();
        let (algo, b64) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        writeln!(f, "{} {algo} {b64}", host_entry(host, port))
            .map_err(|e| SshError::Io(e.to_string()))
    }

    /// Replace the key stored in Switchyard's file for `host:port` (after a confirmed
    /// rebuild). Lines in the user's own file are never touched.
    pub fn replace(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), SshError> {
        let entry = host_entry(host, port);
        if let Ok(text) = std::fs::read_to_string(&self.app_file) {
            let kept: Vec<&str> = text
                .lines()
                .filter(|l| {
                    let first = l.split_whitespace().next().unwrap_or_default();
                    !hosts_match(&entry, first)
                })
                .collect();
            let mut body = kept.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            std::fs::write(&self.app_file, body).map_err(|e| SshError::Io(e.to_string()))?;
        }
        self.trust(host, port, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJbqpzfWlUVlVlDiqty/YbNbN/FB08djdOD5Fh00QTf0";
    const ED_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAYUhAj9rEnudntRTasvH4O9GRXi5pQ4MXUj8OdSVQlO";

    fn key(b64: &str) -> PublicKey {
        parse_public_key_base64(b64).expect("key")
    }

    fn setup(user: &str) -> (tempfile::TempDir, KnownHosts) {
        let dir = tempfile::tempdir().expect("dir");
        let user_file = dir.path().join("known_hosts");
        std::fs::write(&user_file, user).expect("write");
        let kh = KnownHosts {
            user_file: Some(user_file),
            app_file: dir.path().join("switchyard_known_hosts"),
        };
        (dir, kh)
    }

    #[test]
    fn known_unknown_and_changed() {
        let (_d, kh) = setup(&format!(
            "# comment\nsk-weird-type-nobody-knows AAAA bogus\nprod.acme.dev,10.0.4.12 ssh-ed25519 {ED_A}\n[db.acme.dev]:2222 ssh-ed25519 {ED_A}\n"
        ));
        assert_eq!(
            kh.check("prod.acme.dev", 22, &key(ED_A)),
            HostKeyStatus::Known
        );
        assert_eq!(kh.check("10.0.4.12", 22, &key(ED_A)), HostKeyStatus::Known);
        assert_eq!(
            kh.check("db.acme.dev", 2222, &key(ED_A)),
            HostKeyStatus::Known
        );
        assert_eq!(
            kh.check("db.acme.dev", 22, &key(ED_A)),
            HostKeyStatus::Unknown
        );
        assert_eq!(kh.check("other", 22, &key(ED_A)), HostKeyStatus::Unknown);
        match kh.check("prod.acme.dev", 22, &key(ED_B)) {
            HostKeyStatus::Changed { stored, location } => {
                assert_eq!(stored, fingerprint(&key(ED_A)));
                assert!(location.ends_with("known_hosts:3"), "{location}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hashed_wildcard_and_negated_hosts() {
        // `ssh-keygen -H` style entry for "hashed.example" with salt "salt".
        let mut mac = Hmac::<Sha1>::new_from_slice(b"salt").expect("mac");
        mac.update(b"hashed.example");
        let hash = BASE64.encode(&mac.finalize().into_bytes());
        let (_d, kh) = setup(&format!(
            "|1|{}|{hash} ssh-ed25519 {ED_A}\n*.corp,!bad.corp ssh-ed25519 {ED_A}\n",
            BASE64.encode(b"salt")
        ));
        assert_eq!(
            kh.check("hashed.example", 22, &key(ED_A)),
            HostKeyStatus::Known
        );
        assert_eq!(kh.check("db.corp", 22, &key(ED_A)), HostKeyStatus::Known);
        assert_eq!(kh.check("bad.corp", 22, &key(ED_A)), HostKeyStatus::Unknown);
    }

    #[test]
    fn app_file_trust_overrides_the_user_file() {
        let (_d, kh) = setup(&format!("prod ssh-ed25519 {ED_A}\n"));
        assert!(matches!(
            kh.check("prod", 22, &key(ED_B)),
            HostKeyStatus::Changed { .. }
        ));
        kh.replace("prod", 22, &key(ED_B)).expect("replace");
        assert_eq!(kh.check("prod", 22, &key(ED_B)), HostKeyStatus::Known);
    }

    #[test]
    fn revoked_keys_are_refused() {
        let (_d, kh) = setup(&format!("@revoked * ssh-ed25519 {ED_A}\n"));
        assert_eq!(kh.check("any", 22, &key(ED_A)), HostKeyStatus::Revoked);
    }

    #[test]
    fn trust_and_replace_use_the_app_file_only() {
        let (_d, kh) = setup("");
        kh.trust("new.host", 2200, &key(ED_A)).expect("trust");
        assert_eq!(kh.check("new.host", 2200, &key(ED_A)), HostKeyStatus::Known);
        assert!(matches!(
            kh.check("new.host", 2200, &key(ED_B)),
            HostKeyStatus::Changed { .. }
        ));
        kh.replace("new.host", 2200, &key(ED_B)).expect("replace");
        assert_eq!(kh.check("new.host", 2200, &key(ED_B)), HostKeyStatus::Known);
        let user = std::fs::read_to_string(kh.user_file.as_ref().expect("user")).expect("read");
        assert!(user.is_empty(), "user file untouched");
    }
}
