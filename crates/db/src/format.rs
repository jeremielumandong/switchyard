//! A small, conservative SQL formatter: upper-cases keywords and starts major clauses on
//! their own line. Strings, quoted identifiers and comments are never touched.

use crate::dialect::lexer::{self, Flavor, SegKind};

/// Keywords that begin a new line at the current indentation.
const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION ALL",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "VALUES",
    "SET",
    "RETURNING",
    "LEFT JOIN",
    "RIGHT JOIN",
    "INNER JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "JOIN",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "WITH",
];

/// Keywords indented one level under their clause.
const SUB_CLAUSES: &[&str] = &["AND", "OR", "ON"];

/// Keywords upper-cased in place.
const UPPER: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "all",
    "except",
    "intersect",
    "values",
    "set",
    "returning",
    "left",
    "right",
    "inner",
    "full",
    "cross",
    "outer",
    "join",
    "insert",
    "into",
    "update",
    "delete",
    "with",
    "and",
    "or",
    "on",
    "as",
    "not",
    "null",
    "is",
    "in",
    "like",
    "ilike",
    "between",
    "case",
    "when",
    "then",
    "else",
    "end",
    "distinct",
    "asc",
    "desc",
    "nulls",
    "first",
    "last",
    "interval",
    "exists",
    "top",
    "create",
    "table",
    "view",
    "index",
    "drop",
    "alter",
    "primary",
    "key",
    "references",
    "default",
    "true",
    "false",
];

fn word_at(code: &str, i: usize) -> &str {
    let rest = &code[i..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Format a script. Statements are separated by a blank line.
pub fn format_sql(sql: &str, flavor: Flavor) -> String {
    let segs = lexer::segments(sql, flavor);
    let mut out = String::with_capacity(sql.len() + 64);
    let mut depth: usize = 0;
    let mut paren_select: Vec<bool> = Vec::new();
    let mut at_line_start = true;

    let newline = |out: &mut String, indent: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&"    ".repeat(indent));
    };

    for seg in &segs {
        let text = &sql[seg.start..seg.end];
        if seg.kind != SegKind::Code {
            if seg.kind == SegKind::Comment && text.starts_with("--") {
                if !at_line_start && !out.ends_with(' ') && !out.ends_with('\n') {
                    out.push(' ');
                }
                out.push_str(text);
                out.push('\n');
                out.push_str(&"    ".repeat(depth));
                at_line_start = true;
            } else {
                out.push_str(text);
                at_line_start = false;
            }
            continue;
        }
        // Collapse whitespace in code, then scan words.
        let collapsed: String = {
            let mut s = String::with_capacity(text.len());
            let mut ws = false;
            for ch in text.chars() {
                if ch.is_whitespace() {
                    ws = true;
                } else {
                    if ws && (!s.is_empty() || (!out.is_empty() && !at_line_start)) {
                        s.push(' ');
                    }
                    ws = false;
                    s.push(ch);
                }
            }
            if ws && !s.is_empty() {
                s.push(' ');
            }
            s
        };
        let b = collapsed.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i] as char;
            if c.is_ascii_alphabetic() || c == '_' {
                let w = word_at(&collapsed, i);
                let prev_ident = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                if !prev_ident {
                    // Two-word clauses first.
                    let two = {
                        let after = i + w.len();
                        if after < b.len() && b[after] == b' ' {
                            let w2 = word_at(&collapsed, after + 1);
                            format!("{} {}", w.to_ascii_uppercase(), w2.to_ascii_uppercase())
                        } else {
                            String::new()
                        }
                    };
                    if CLAUSES.contains(&two.as_str()) {
                        newline(&mut out, depth);
                        out.push_str(&two);
                        i += two.len();
                        at_line_start = false;
                        continue;
                    }
                    let upper = w.to_ascii_uppercase();
                    if CLAUSES.contains(&upper.as_str()) {
                        newline(&mut out, depth);
                        out.push_str(&upper);
                        i += w.len();
                        at_line_start = false;
                        continue;
                    }
                    if SUB_CLAUSES.contains(&upper.as_str()) && depth == 0 {
                        newline(&mut out, depth + 1);
                        out.push_str(&upper);
                        i += w.len();
                        at_line_start = false;
                        continue;
                    }
                    if UPPER.contains(&w.to_ascii_lowercase().as_str()) {
                        out.push_str(&upper);
                    } else {
                        out.push_str(w);
                    }
                    i += w.len();
                    at_line_start = false;
                    continue;
                }
            }
            match c {
                '(' => {
                    let opens_select = collapsed[i + 1..]
                        .trim_start()
                        .to_ascii_uppercase()
                        .starts_with("SELECT");
                    paren_select.push(opens_select);
                    out.push('(');
                    if opens_select {
                        depth += 1;
                    }
                }
                ')' => {
                    if paren_select.pop() == Some(true) {
                        depth = depth.saturating_sub(1);
                        newline(&mut out, depth);
                    }
                    out.push(')');
                }
                ';' => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str(";\n\n");
                    depth = 0;
                    paren_select.clear();
                    at_line_start = true;
                    i += 1;
                    while i < b.len() && b[i] == b' ' {
                        i += 1;
                    }
                    continue;
                }
                ' ' if at_line_start || out.ends_with('\n') || out.ends_with(' ') => {}
                _ => out.push(c),
            }
            at_line_start = false;
            i += 1;
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_clauses_and_keeps_strings() {
        let f = format_sql(
            "select c.id, 'from x where' as s from customers c join orders o on o.customer_id = c.id where a = 1 and b = 'it''s' order by 1 limit 5;",
            Flavor::Postgres,
        );
        assert_eq!(
            f,
            "SELECT c.id, 'from x where' AS s\nFROM customers c\nJOIN orders o\n    ON o.customer_id = c.id\nWHERE a = 1\n    AND b = 'it''s'\nORDER BY 1\nLIMIT 5;"
        );
    }

    #[test]
    fn keeps_comments_and_subqueries() {
        let f = format_sql("-- top\nselect * from (select 1) t", Flavor::Postgres);
        assert!(f.starts_with("-- top\nSELECT *\nFROM ("), "{f}");
        assert!(f.contains("SELECT 1"), "{f}");
    }

    #[test]
    fn idempotent() {
        let once = format_sql("select a from t where x = 1 and y = 2;", Flavor::Postgres);
        assert_eq!(format_sql(&once, Flavor::Postgres), once);
    }
}
