//! Clickable links: find the URL under a column of a line of terminal text.

const SCHEMES: &[&str] = &["https://", "http://", "file://", "ssh://", "ftp://"];

fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '"' | '\'' | '<' | '>' | '`' | '|' | '{' | '}' | '\\' | '^'
        )
}

/// URLs in `line` as `(start column, end column exclusive, url)`. Columns count chars.
pub fn find_urls(line: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().take(8).collect();
        if let Some(scheme) = SCHEMES.iter().find(|s| rest.starts_with(**s)) {
            let mut j = i + scheme.chars().count();
            while j < chars.len() && is_url_char(chars[j]) {
                j += 1;
            }
            // Trailing punctuation usually belongs to the sentence, and an unbalanced
            // closing parenthesis to the surrounding text.
            while j > i {
                let c = chars[j - 1];
                let opens = chars[i..j].iter().filter(|c| **c == '(').count();
                let closes = chars[i..j].iter().filter(|c| **c == ')').count();
                if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ']')
                    || (c == ')' && closes > opens)
                {
                    j -= 1;
                } else {
                    break;
                }
            }
            if j > i + scheme.chars().count() {
                out.push((i, j, chars[i..j].iter().collect()));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// The URL covering column `col` of `line`, if any.
pub fn url_at(line: &str, col: usize) -> Option<String> {
    find_urls(line)
        .into_iter()
        .find(|(s, e, _)| (*s..*e).contains(&col))
        .map(|(_, _, u)| u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls() {
        let line = "see https://example.com/a?b=1, or (http://x.dev/p) now.";
        let urls: Vec<_> = find_urls(line).into_iter().map(|u| u.2).collect();
        assert_eq!(urls, ["https://example.com/a?b=1", "http://x.dev/p"]);
        assert_eq!(
            url_at(line, 10).as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(url_at(line, 2), None);
        assert_eq!(
            url_at("wiki https://en.wikipedia.org/wiki/Rust_(language) ok", 8).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        assert!(find_urls("https:// nothing").is_empty());
    }
}
