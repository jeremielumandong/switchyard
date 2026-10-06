//! `~/.ssh/config` import: Host, HostName, User, Port, IdentityFile, ProxyJump.

use serde::{Deserialize, Serialize};

/// A Host block from an OpenSSH config file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfigHost {
    /// The alias (`Host` pattern without wildcards).
    pub alias: String,
    /// `HostName`, or the alias when absent.
    pub hostname: String,
    /// `User`.
    pub user: Option<String>,
    /// `Port`.
    pub port: Option<u16>,
    /// First `IdentityFile`.
    pub identity_file: Option<String>,
    /// `ProxyJump` chain, outermost first.
    pub proxy_jump: Vec<String>,
}

/// Parse a config file. Wildcard patterns (`*`, `?`, `!`) are skipped, but their options
/// still apply as defaults to later concrete hosts the way OpenSSH applies `Host *`.
pub fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
    struct Block {
        patterns: Vec<String>,
        opts: Vec<(String, String)>,
    }
    let mut blocks: Vec<Block> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.trim(), v.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        let value = value.trim_matches('"').to_owned();
        let key = key.to_ascii_lowercase();
        if key == "host" {
            blocks.push(Block {
                patterns: value.split_whitespace().map(str::to_owned).collect(),
                opts: Vec::new(),
            });
        } else if key == "match" {
            // Match blocks are not imported; swallow their options.
            blocks.push(Block {
                patterns: Vec::new(),
                opts: Vec::new(),
            });
        } else if let Some(b) = blocks.last_mut() {
            b.opts.push((key, value));
        }
    }
    let is_wild = |p: &str| p.contains(['*', '?', '!']);
    let glob = |pat: &str, s: &str| -> bool {
        fn m(p: &[u8], s: &[u8]) -> bool {
            match (p.first(), s.first()) {
                (None, None) => true,
                (Some(b'*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
                (Some(b'?'), Some(_)) => m(&p[1..], &s[1..]),
                (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => m(&p[1..], &s[1..]),
                _ => false,
            }
        }
        m(pat.as_bytes(), s.as_bytes())
    };
    let mut out = Vec::new();
    for b in &blocks {
        for alias in b.patterns.iter().filter(|p| !is_wild(p)) {
            let mut h = SshConfigHost {
                alias: alias.clone(),
                ..Default::default()
            };
            let mut hostname = None;
            // First obtained value wins, in file order, across matching blocks.
            for blk in &blocks {
                let matches =
                    blk.patterns.iter().any(|p| {
                        !p.starts_with('!') && (p == alias || (is_wild(p) && glob(p, alias)))
                    }) && !blk
                        .patterns
                        .iter()
                        .any(|p| p.strip_prefix('!').is_some_and(|n| glob(n, alias)));
                if !matches {
                    continue;
                }
                for (k, v) in &blk.opts {
                    match k.as_str() {
                        "hostname" if hostname.is_none() => hostname = Some(v.clone()),
                        "user" if h.user.is_none() => h.user = Some(v.clone()),
                        "port" if h.port.is_none() => h.port = v.parse().ok(),
                        "identityfile" if h.identity_file.is_none() => {
                            h.identity_file = Some(v.clone())
                        }
                        "proxyjump" if h.proxy_jump.is_empty() && v != "none" => {
                            h.proxy_jump = v.split(',').map(|s| s.trim().to_owned()).collect()
                        }
                        _ => {}
                    }
                }
            }
            h.hostname = hostname.unwrap_or_else(|| alias.clone());
            out.push(h);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_config_snapshot() {
        let cfg = r#"
# Company hosts
Host bastion
    HostName bastion.acme.dev
    User deploy
    IdentityFile ~/.ssh/id_ed25519

Host prod-db-01 prod-db-02
    HostName 10.0.4.12
    ProxyJump bastion
    Port 2222

Host staging-app
  HostName=10.0.8.3
  ProxyJump bastion,jump2

Host *.internal
    User ops

Host reports.internal

Host *
    User fallback
    ServerAliveInterval 30
"#;
        insta::assert_yaml_snapshot!(parse_ssh_config(cfg));
    }
}
