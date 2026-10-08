//! A small lexical scanner that separates code from strings, quoted identifiers and
//! comments. Splitting, parameter detection and highlighting all build on it, so they
//! agree on what is "inside a string".

/// Lexical flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// PostgreSQL: `E'..'` escapes, `"ident"`, `$tag$` bodies, nested block comments.
    Postgres,
    /// T-SQL: `N'..'`, `"ident"`, `[ident]`, nested block comments.
    TSql,
    /// SQLite (Cloudflare D1): `"ident"`, `[ident]`, `` `ident` ``, flat block comments.
    Sqlite,
    /// Snowflake: `'..'` with backslash escapes, `"ident"`, `$$` bodies, `//` comments,
    /// flat block comments.
    Snowflake,
    /// Oracle: `"ident"`, `q'[..]'` alternative quoting, flat block comments.
    Oracle,
    /// MySQL / MariaDB: `'..'` and `".."` strings with backslash escapes, `` `ident` ``,
    /// `#` comments, `-- ` comments only before whitespace, flat block comments.
    MySql,
}

/// Kind of a lexical segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    /// Ordinary SQL text.
    Code,
    /// String literal (including dollar-quoted bodies).
    Str,
    /// Quoted identifier.
    Ident,
    /// `-- ...` or `/* ... */`.
    Comment,
}

/// A byte range of one kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Kind.
    pub kind: SegKind,
    /// Start byte.
    pub start: usize,
    /// End byte (exclusive).
    pub end: usize,
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Split `sql` into segments. Unterminated strings or comments run to the end.
pub fn segments(sql: &str, flavor: Flavor) -> Vec<Segment> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out: Vec<Segment> = Vec::new();
    let mut code_start = 0;
    let mut i = 0;

    let push = |out: &mut Vec<Segment>, kind, start, end| {
        if end > start {
            out.push(Segment { kind, start, end });
        }
    };

    while i < n {
        let c = b[i];
        let next = if i + 1 < n { b[i + 1] } else { 0 };
        // Start of a non-code segment?
        let (kind, end) = if (c == b'-'
            && next == b'-'
            && (flavor != Flavor::MySql
                || i + 2 >= n
                || b[i + 2].is_ascii_whitespace()
                || b[i + 2].is_ascii_control()))
            || (c == b'/' && next == b'/' && flavor == Flavor::Snowflake)
            || (c == b'#' && flavor == Flavor::MySql)
        {
            let end = sql[i..].find('\n').map_or(n, |p| i + p);
            (SegKind::Comment, end)
        } else if c == b'/' && next == b'*' {
            let mut depth = 0usize;
            let mut j = i;
            let mut end = n;
            while j < n {
                if b[j] == b'/'
                    && j + 1 < n
                    && b[j + 1] == b'*'
                    && (depth == 0
                        || !matches!(
                            flavor,
                            Flavor::Sqlite | Flavor::Snowflake | Flavor::Oracle | Flavor::MySql
                        ))
                {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && j + 1 < n && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        end = j;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (SegKind::Comment, end)
        } else if c == b'\''
            && flavor == Flavor::Oracle
            && i > 0
            && matches!(b[i - 1], b'q' | b'Q')
            && (i < 2 || !is_ident_char(b[i - 2]) || matches!(b[i - 2], b'n' | b'N'))
            && i + 1 < n
        {
            // q'<open> ... <close>'  (brackets pair up; any other character closes itself)
            let open = b[i + 1];
            let close = match open {
                b'[' => b']',
                b'{' => b'}',
                b'(' => b')',
                b'<' => b'>',
                other => other,
            };
            let body = i + 2;
            let mut end = n;
            let mut j = body;
            while j + 1 < n {
                if b[j] == close && b[j + 1] == b'\'' {
                    end = j + 2;
                    break;
                }
                j += 1;
            }
            (SegKind::Str, end)
        } else if c == b'\'' || (c == b'"' && flavor == Flavor::MySql) {
            let backslash = matches!(flavor, Flavor::Snowflake | Flavor::MySql)
                || (flavor == Flavor::Postgres
                    && i > 0
                    && (b[i - 1] == b'E' || b[i - 1] == b'e')
                    && (i < 2 || !is_ident_char(b[i - 2])));
            let mut j = i + 1;
            let mut end = n;
            while j < n {
                if backslash && b[j] == b'\\' {
                    j += 2;
                } else if b[j] == c {
                    if j + 1 < n && b[j + 1] == c {
                        j += 2;
                    } else {
                        end = j + 1;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (SegKind::Str, end.min(n))
        } else if c == b'"' {
            let mut j = i + 1;
            let mut end = n;
            while j < n {
                if b[j] == b'"' {
                    if j + 1 < n && b[j + 1] == b'"' {
                        j += 2;
                    } else {
                        end = j + 1;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (SegKind::Ident, end)
        } else if c == b'`' && matches!(flavor, Flavor::Sqlite | Flavor::MySql) {
            let mut j = i + 1;
            let mut end = n;
            while j < n {
                if b[j] == b'`' {
                    if j + 1 < n && b[j + 1] == b'`' {
                        j += 2;
                    } else {
                        end = j + 1;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (SegKind::Ident, end)
        } else if c == b'[' && matches!(flavor, Flavor::TSql | Flavor::Sqlite) {
            let mut j = i + 1;
            let mut end = n;
            while j < n {
                if b[j] == b']' {
                    if j + 1 < n && b[j + 1] == b']' {
                        j += 2;
                    } else {
                        end = j + 1;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (SegKind::Ident, end)
        } else if c == b'$' && next == b'$' && flavor == Flavor::Snowflake {
            // $$ ... $$ (Snowflake has no tags)
            let body = i + 2;
            let end = sql[body..].find("$$").map_or(n, |p| body + p + 2);
            (SegKind::Str, end)
        } else if c == b'$'
            && flavor == Flavor::Postgres
            && (i == 0 || !is_ident_char(b[i - 1]))
            && !next.is_ascii_digit()
        {
            // $tag$ ... $tag$
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j < n && b[j] == b'$' {
                let tag = &sql[i..=j];
                let body = j + 1;
                let end = sql[body..].find(tag).map_or(n, |p| body + p + tag.len());
                (SegKind::Str, end)
            } else {
                i += 1;
                continue;
            }
        } else {
            i += 1;
            continue;
        };
        push(&mut out, SegKind::Code, code_start, i);
        push(&mut out, kind, i, end);
        i = end;
        code_start = end;
    }
    push(&mut out, SegKind::Code, code_start, n);
    out
}

/// Whether byte offset `at` lies in a code segment.
pub fn in_code(segs: &[Segment], at: usize) -> bool {
    segs.iter()
        .find(|s| s.start <= at && at < s.end)
        .is_none_or(|s| s.kind == SegKind::Code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str, f: Flavor) -> Vec<(SegKind, &str)> {
        segments(sql, f)
            .into_iter()
            .map(|s| (s.kind, &sql[s.start..s.end]))
            .collect()
    }

    #[test]
    fn postgres_segments() {
        let sql = "select 'it''s', E'a\\'b', $fn$ x; $fn$, \"a\"\"b\" -- c;\n/* /* n */ */ $1";
        let k = kinds(sql, Flavor::Postgres);
        assert!(k.contains(&(SegKind::Str, "'it''s'")));
        assert!(k.contains(&(SegKind::Str, "'a\\'b'")));
        assert!(k.contains(&(SegKind::Str, "$fn$ x; $fn$")));
        assert!(k.contains(&(SegKind::Ident, "\"a\"\"b\"")));
        assert!(k.contains(&(SegKind::Comment, "-- c;")));
        assert!(k.contains(&(SegKind::Comment, "/* /* n */ */")));
        assert_eq!(k.last(), Some(&(SegKind::Code, " $1")));
    }

    #[test]
    fn tsql_brackets() {
        let k = kinds("select [a]]b], N'x' from t", Flavor::TSql);
        assert!(k.contains(&(SegKind::Ident, "[a]]b]")));
        assert!(k.contains(&(SegKind::Str, "'x'")));
    }

    #[test]
    fn snowflake_segments() {
        let sql = "select 'a\\'b;', $$ x; y $$, \"c\" // d;\n/* e */ 1";
        let k = kinds(sql, Flavor::Snowflake);
        assert!(k.contains(&(SegKind::Str, "'a\\'b;'")));
        assert!(k.contains(&(SegKind::Str, "$$ x; y $$")));
        assert!(k.contains(&(SegKind::Ident, "\"c\"")));
        assert!(k.contains(&(SegKind::Comment, "// d;")));
        assert!(k.contains(&(SegKind::Comment, "/* e */")));
    }

    #[test]
    fn oracle_q_quotes() {
        let k = kinds("select q'[it's; ok]', nq'{x}' from dual", Flavor::Oracle);
        assert!(k.contains(&(SegKind::Str, "'[it's; ok]'")));
        assert!(k.contains(&(SegKind::Str, "'{x}'")));
    }

    #[test]
    fn mysql_segments() {
        let sql = "select 'a\\'b;', \"c\\\"d;\", `e``f` # g;\n-- h;\n--1 /* i */ 2";
        let k = kinds(sql, Flavor::MySql);
        assert!(k.contains(&(SegKind::Str, "'a\\'b;'")));
        assert!(k.contains(&(SegKind::Str, "\"c\\\"d;\"")));
        assert!(k.contains(&(SegKind::Ident, "`e``f`")));
        assert!(k.contains(&(SegKind::Comment, "# g;")));
        assert!(k.contains(&(SegKind::Comment, "-- h;")));
        // `--1` is minus minus one, not a comment.
        assert!(k.contains(&(SegKind::Code, "\n--1 ")));
        assert!(k.contains(&(SegKind::Comment, "/* i */")));
    }

    #[test]
    fn unterminated_runs_to_end() {
        let k = kinds("select 'abc", Flavor::Postgres);
        assert_eq!(k.last(), Some(&(SegKind::Str, "'abc")));
    }
}
