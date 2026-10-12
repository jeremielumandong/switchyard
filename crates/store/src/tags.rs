//! Host tags (Fleet): free-form labels next to folders, such as `web`, `role:db`,
//! `env:prod` or `region:eu-west-1`. A `key:value` tag groups under its key in the
//! Explorer; any other tag is a group of its own.
//!
//! Tags are lower case, without spaces (a space becomes `-`) and at most
//! [`MAX_TAG_LEN`] characters, so `Web`, ` web ` and `WEB` are one tag. The Host's JSON
//! keeps them (exports carry them); the `host_tags` table indexes them for filtering.

/// Longest tag, in characters.
pub const MAX_TAG_LEN: usize = 64;

/// One tag as stored: trimmed, lower case, inner whitespace as `-`, control characters
/// dropped, cut at [`MAX_TAG_LEN`]. `None` when nothing is left.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut gap = false;
    for c in raw.trim().chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if c.is_control() || c == ',' {
            continue;
        }
        // `role: db` is `role:db`; other gaps become `-`.
        if gap && !out.is_empty() && !out.ends_with([':', '=']) && !matches!(c, ':' | '=') {
            out.push('-');
        }
        gap = false;
        out.extend(c.to_lowercase());
    }
    let out: String = out.chars().take(MAX_TAG_LEN).collect();
    (!out.is_empty()).then_some(out)
}

/// Normalize a list of tags, keeping the first occurrence of each.
pub fn normalize_tags<I, S>(tags: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        if let Some(t) = normalize_tag(t.as_ref())
            && !out.contains(&t)
        {
            out.push(t);
        }
    }
    out
}

/// Tags typed in one field: separated by commas or spaces.
pub fn parse_tags(text: &str) -> Vec<String> {
    normalize_tags(text.split(|c: char| c == ',' || c.is_whitespace()))
}

/// Tags as typed in a field (`a, b, c`).
pub fn format_tags(tags: &[String]) -> String {
    tags.join(", ")
}

/// The key and value of a `key:value` (or `key=value`) tag.
pub fn tag_key_value(tag: &str) -> Option<(&str, &str)> {
    let at = tag.find([':', '='])?;
    let (k, v) = (&tag[..at], &tag[at + 1..]);
    (!k.is_empty() && !v.is_empty()).then_some((k, v))
}

/// Whether `tags` holds `tag` (compared normalized).
pub fn has_tag(tags: &[String], tag: &str) -> bool {
    normalize_tag(tag).is_some_and(|t| tags.iter().any(|x| normalize_tag(x).as_ref() == Some(&t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_normalized() {
        assert_eq!(normalize_tag("  Web  ").as_deref(), Some("web"));
        assert_eq!(normalize_tag("Role: DB").as_deref(), Some("role:db"));
        assert_eq!(normalize_tag("eu west\t1").as_deref(), Some("eu-west-1"));
        assert_eq!(normalize_tag(" \n "), None);
        assert_eq!(normalize_tag(&"x".repeat(100)).map(|t| t.len()), Some(64));
        assert_eq!(
            normalize_tags(["web", "WEB", " db", "", "role:web"]),
            ["web", "db", "role:web"]
        );
        assert_eq!(
            parse_tags("web, role:db  env:prod,,"),
            ["web", "role:db", "env:prod"]
        );
        assert_eq!(format_tags(&parse_tags("a b")), "a, b");
    }

    #[test]
    fn key_value_tags() {
        assert_eq!(tag_key_value("role:web"), Some(("role", "web")));
        assert_eq!(tag_key_value("env=prod"), Some(("env", "prod")));
        assert_eq!(tag_key_value("web"), None);
        assert_eq!(tag_key_value(":x"), None);
        assert!(has_tag(&["role:web".into()], "Role:Web"));
        assert!(!has_tag(&["role:web".into()], "web"));
    }
}
