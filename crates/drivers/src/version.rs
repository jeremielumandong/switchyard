//! Loose dotted version comparison (`21.13.0.0`, `1.20-1`).

use std::cmp::Ordering;

fn parts(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(u64::MAX))
        .collect()
}

/// Compare two versions numerically, part by part; missing parts count as 0.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// The first dotted version in a program's `--version` output (`2.1.292 (Claude Code)`,
/// `codex-cli 0.160.1`).
pub fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|t| t.trim_matches('.'))
        .find(|t| t.contains('.') && t.split('.').all(|p| !p.is_empty()))
        .map(str::to_owned)
}

/// Whether `found` satisfies `minimum`.
pub fn at_least(found: &str, minimum: &str) -> bool {
    compare(found, minimum) != Ordering::Less
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_versions() {
        assert_eq!(
            parse_version("2.1.292 (Claude Code)").as_deref(),
            Some("2.1.292")
        );
        assert_eq!(
            parse_version("codex-cli 0.160.1\n").as_deref(),
            Some("0.160.1")
        );
        assert_eq!(parse_version("0.63.0").as_deref(), Some("0.63.0"));
        assert_eq!(parse_version("v1.2"), Some("1.2".into()));
        assert_eq!(parse_version("no version"), None);
        assert_eq!(parse_version("build 42"), None);
    }

    #[test]
    fn ordering() {
        assert!(at_least("21.13.0.0", "21.1"));
        assert!(at_least("2.0", "2"));
        assert!(!at_least("1.9.9", "2.0"));
        assert!(at_least("10.0", "9.9"));
        assert_eq!(compare("1.20-1", "1.20"), Ordering::Greater);
    }
}
