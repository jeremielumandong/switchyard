//! Text helpers for DDL that comes from the server: schema retargeting, whitespace
//! normalization and `CREATE` header rewrites. They only touch code segments (never
//! strings or comments), using the dialect lexer.

use crate::dialect::{Dialect, Flavor, SegKind, lexer};

/// Whitespace runs collapsed to one space, trimmed, a trailing `;` dropped: what two
/// definitions are compared by.
pub(crate) fn collapse_ws(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_end_matches(';').trim_end().to_owned()
}

/// The text of a quoted identifier segment without its quotes (`"a""b"` → `a"b`).
fn unquote(seg: &str) -> String {
    let mut chars = seg.chars();
    let (open, close) = match chars.next() {
        Some('"') => ('"', '"'),
        Some('[') => ('[', ']'),
        Some('`') => ('`', '`'),
        _ => return seg.to_owned(),
    };
    let inner = &seg[open.len_utf8()..];
    let inner = inner.strip_suffix(close).unwrap_or(inner);
    let doubled = format!("{close}{close}");
    inner.replace(&doubled, &close.to_string())
}

/// `sql` with every `from.` schema qualifier (quoted or bare, in code only) written as
/// `to.` quoted by `d`. Bare words compare case-insensitively, quoted ones exactly.
/// Unchanged when the names are equal.
pub(crate) fn retarget(sql: &str, flavor: Flavor, d: &dyn Dialect, from: &str, to: &str) -> String {
    if from == to || from.is_empty() {
        return sql.to_owned();
    }
    let target = d.quote_ident(to);
    let segs = lexer::segments(sql, flavor);
    let mut out = String::with_capacity(sql.len());
    for seg in &segs {
        let text = &sql[seg.start..seg.end];
        let next_is_dot = sql[seg.end..].starts_with('.');
        let prev_is_dot = seg.start > 0 && sql.as_bytes()[seg.start - 1] == b'.';
        match seg.kind {
            SegKind::Ident if next_is_dot && !prev_is_dot && unquote(text) == from => {
                out.push_str(&target)
            }
            SegKind::Code => out.push_str(&retarget_code(text, from, &target)),
            // A dollar-quoted body (`$$ … $$`, `$fn$ … $fn$`) is code too.
            SegKind::Str if text.starts_with('$') => match dollar_body(text) {
                Some((open, body, close)) => {
                    out.push_str(open);
                    out.push_str(&retarget(body, flavor, d, from, to));
                    out.push_str(close);
                }
                None => out.push_str(text),
            },
            _ => out.push_str(text),
        }
    }
    out
}

/// `$tag$`, body, `$tag$` of a dollar-quoted string.
fn dollar_body(text: &str) -> Option<(&str, &str, &str)> {
    let tag_end = text[1..].find('$')? + 2;
    let tag = &text[..tag_end];
    if text.len() < 2 * tag_end || !text.ends_with(tag) {
        return None;
    }
    Some((tag, &text[tag_end..text.len() - tag_end], tag))
}

/// Replace bare `from.` words inside one code segment.
fn retarget_code(code: &str, from: &str, target: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '#' || c == '@';
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    let bytes = code.as_bytes();
    while i < code.len() {
        let c = code[i..].chars().next().unwrap_or(' ');
        if is_word(c) {
            let start = i;
            while i < code.len() {
                let ch = code[i..].chars().next().unwrap_or(' ');
                if !is_word(ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            let word = &code[start..i];
            let prev_dot = start > 0 && bytes[start - 1] == b'.';
            if !prev_dot && bytes.get(i) == Some(&b'.') && word.eq_ignore_ascii_case(from) {
                out.push_str(target);
            } else {
                out.push_str(word);
            }
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Byte range of the object name in a `CREATE … <TABLE|VIEW|INDEX|…> [IF NOT EXISTS]
/// name` header: one or two (dotted) name parts, quoted or bare. `None` when the text
/// does not start that way.
fn create_name_span(sql: &str, keywords: &[&str]) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    let skip_ws_comments = |mut p: usize| -> usize {
        loop {
            let rest = &sql[p..];
            let trimmed = rest.trim_start();
            p += rest.len() - trimmed.len();
            if trimmed.starts_with("--") {
                p += trimmed.find('\n').map_or(trimmed.len(), |n| n + 1);
            } else if trimmed.starts_with("/*") {
                p += trimmed.find("*/").map_or(trimmed.len(), |n| n + 2);
            } else {
                return p;
            }
        }
    };
    let word_at = |p: usize| -> (usize, &str) {
        let rest = &sql[p..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        (p + end, &rest[..end])
    };
    pos = skip_ws_comments(pos);
    let (end, w) = word_at(pos);
    if !w.eq_ignore_ascii_case("CREATE") {
        return None;
    }
    pos = end;
    // Modifiers until the object keyword.
    loop {
        pos = skip_ws_comments(pos);
        let (end, w) = word_at(pos);
        if w.is_empty() {
            return None;
        }
        pos = end;
        if keywords.iter().any(|k| k.eq_ignore_ascii_case(w)) {
            break;
        }
    }
    pos = skip_ws_comments(pos);
    // IF NOT EXISTS
    let (end, w) = word_at(pos);
    if w.eq_ignore_ascii_case("IF") {
        pos = end;
        for _ in 0..2 {
            pos = skip_ws_comments(pos);
            pos = word_at(pos).0;
        }
        pos = skip_ws_comments(pos);
    }
    let start = pos;
    let mut p = pos;
    for part in 0..2 {
        let rest = &sql[p..];
        let close = match rest.chars().next()? {
            '"' => Some('"'),
            '[' => Some(']'),
            '`' => Some('`'),
            _ => None,
        };
        match close {
            Some(close) => {
                let mut q = p + 1;
                loop {
                    let r = &sql[q..];
                    let at = r.find(close)?;
                    q += at + close.len_utf8();
                    if sql[q..].starts_with(close) && close != ']' {
                        q += close.len_utf8();
                        continue;
                    }
                    break;
                }
                p = q;
            }
            None => {
                let (end, w) = word_at(p);
                if w.is_empty() {
                    return None;
                }
                p = end;
            }
        }
        if part == 0 && sql[p..].starts_with('.') {
            p += 1;
        } else {
            break;
        }
    }
    Some((start, p))
}

/// `sql` (a `CREATE … <keyword> name …` statement) with its object name replaced by
/// `qualified`; unchanged when the header is not recognized.
pub(crate) fn rename_created(sql: &str, keywords: &[&str], qualified: &str) -> String {
    match create_name_span(sql, keywords) {
        Some((s, e)) => format!("{}{qualified}{}", &sql[..s], &sql[e..]),
        None => sql.to_owned(),
    }
}

/// `sql` with its first code word `CREATE` replaced by `with` (`CREATE OR ALTER`).
pub(crate) fn replace_create(sql: &str, flavor: Flavor, with: &str) -> String {
    for seg in lexer::segments(sql, flavor) {
        if seg.kind != SegKind::Code {
            continue;
        }
        let code = &sql[seg.start..seg.end];
        let lead = code.len() - code.trim_start().len();
        let rest = &code[lead..];
        if rest.is_empty() {
            continue;
        }
        if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("CREATE") {
            let at = seg.start + lead;
            return format!("{}{with}{}", &sql[..at], &sql[at + 6..]);
        }
        return sql.to_owned();
    }
    sql.to_owned()
}

/// The first statement of a script (catalog DDL that appends indexes and triggers).
pub(crate) fn first_statement(sql: &str, d: &dyn Dialect) -> String {
    d.split_script(sql)
        .first()
        .map_or_else(|| sql.trim().to_owned(), |s| s.text(sql).trim().to_owned())
}

/// `sql` ending with `;`.
pub(crate) fn semi(sql: &str) -> String {
    crate::dialect::terminated(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::postgres::PostgresDialect;
    use crate::dialect::tsql::TSqlDialect;

    #[test]
    fn retarget_touches_qualifiers_in_code_only() {
        let sql =
            "SELECT a.x, 'src.y' FROM src.t a JOIN \"src\".u ON true -- src.z\nJOIN other.src_t";
        let out = retarget(sql, Flavor::Postgres, &PostgresDialect, "src", "dst");
        assert_eq!(
            out,
            "SELECT a.x, 'src.y' FROM dst.t a JOIN dst.u ON true -- src.z\nJOIN other.src_t"
        );
        let body = "CREATE FUNCTION src.f() RETURNS int AS $f$ SELECT count(*) FROM src.t $f$";
        assert_eq!(
            retarget(body, Flavor::Postgres, &PostgresDialect, "src", "dst"),
            "CREATE FUNCTION dst.f() RETURNS int AS $f$ SELECT count(*) FROM dst.t $f$"
        );
        let out = retarget(
            "[Src].[t], src.u, x.src.v",
            Flavor::TSql,
            &TSqlDialect,
            "Src",
            "New Schema",
        );
        assert_eq!(out, "[New Schema].[t], [New Schema].u, x.src.v");
    }

    #[test]
    fn create_headers_are_renamed() {
        assert_eq!(
            rename_created(
                "CREATE TABLE \"t x\" (a int)",
                &["TABLE"],
                "\"main\".\"_new\""
            ),
            "CREATE TABLE \"main\".\"_new\" (a int)"
        );
        assert_eq!(
            rename_created(
                "create unique index if not exists main.ix on t(a)",
                &["INDEX"],
                "aux.ix"
            ),
            "create unique index if not exists aux.ix on t(a)"
        );
        assert_eq!(
            rename_created("CREATE TEMP VIEW [v] AS SELECT 1", &["VIEW"], "v2"),
            "CREATE TEMP VIEW v2 AS SELECT 1"
        );
        assert_eq!(rename_created("SELECT 1", &["TABLE"], "x"), "SELECT 1");
    }

    #[test]
    fn create_is_replaced_after_comments() {
        assert_eq!(
            replace_create(
                "-- note\nCREATE VIEW v AS SELECT 1",
                Flavor::TSql,
                "CREATE OR ALTER"
            ),
            "-- note\nCREATE OR ALTER VIEW v AS SELECT 1"
        );
        assert_eq!(collapse_ws("  a\n  b ;"), "a b");
    }
}
