//! Statement delimiting the way DBeaver's "Smart" mode does it: inside a unit that
//! `split_script` returns, a blank line also ends a statement when what follows starts a
//! new one and what precedes is complete. So two queries separated by an empty line run
//! one at a time with Ctrl/Cmd+Enter even without semicolons.
//!
//! Units with procedural code (`DECLARE`, `BEGIN … END` blocks, `IF` / `WHILE`, routine
//! and trigger bodies) are left whole, because their statements share variables and
//! blank lines inside them are common. T-SQL units (whole `GO` batches) are also split on
//! top-level `;` under the same rule.

use super::lexer::{self, Flavor, SegKind, Segment};
use super::{StatementSpan, with_lines};

/// Words that start a statement: a blank line before one of these may end the previous
/// statement.
const STARTERS: &[&str] = &[
    "ALTER",
    "ANALYZE",
    "ANALYSE",
    "BACKUP",
    "BEGIN",
    "CALL",
    "CHECKPOINT",
    "CLUSTER",
    "COMMENT",
    "COMMIT",
    "COPY",
    "CREATE",
    "DBCC",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISCARD",
    "DO",
    "DROP",
    "EXEC",
    "EXECUTE",
    "EXPLAIN",
    "GRANT",
    "INSERT",
    "KILL",
    "LISTEN",
    "LOCK",
    "MERGE",
    "NOTIFY",
    "OPTIMIZE",
    "PRAGMA",
    "PREPARE",
    "PRINT",
    "REFRESH",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESET",
    "RESTORE",
    "REVOKE",
    "ROLLBACK",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "SHOW",
    "START",
    "TRUNCATE",
    "UNDROP",
    "UNLISTEN",
    "UNLOCK",
    "UPDATE",
    "UPSERT",
    "USE",
    "VACUUM",
    "VALUES",
    "WITH",
];

/// Last words that leave a statement unfinished: no split after them.
const CONTINUES: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "AS",
    "BETWEEN",
    "BY",
    "CASE",
    "DISTINCT",
    "ELSE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FROM",
    "HAVING",
    "IN",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LIKE",
    "MINUS",
    "NOT",
    "ON",
    "OR",
    "SELECT",
    "SET",
    "THEN",
    "UNION",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
];

/// Split each unit of `units` (from `split_script`) further at smart blank lines (and,
/// when `semicolons`, at top-level `;`).
pub(crate) fn refine(
    sql: &str,
    flavor: Flavor,
    units: Vec<StatementSpan>,
    semicolons: bool,
) -> Vec<StatementSpan> {
    if flavor == Flavor::JavaScript || units.is_empty() {
        return units;
    }
    let segs = lexer::segments(sql, flavor);
    let mut out = Vec::with_capacity(units.len());
    for unit in units {
        if unit.repeat > 1 {
            // `GO n` repeats the whole batch; splitting it would reorder statements.
            out.push(unit);
            continue;
        }
        let first = segs.partition_point(|s| s.end <= unit.start);
        let unit_segs: Vec<Segment> = segs[first..]
            .iter()
            .take_while(|s| s.start < unit.end)
            .map(|s| Segment {
                kind: s.kind,
                start: s.start.max(unit.start),
                end: s.end.min(unit.end),
            })
            .collect();
        split_unit(sql, &unit, &unit_segs, semicolons, &mut out);
    }
    with_lines(sql, out)
}

/// A word of code: its upper-cased text, where it starts, and whether it is the first
/// word on its line.
struct Word {
    text: String,
    start: usize,
    line_start: bool,
    /// Parenthesis depth.
    depth: i32,
}

/// Code words of a unit, with the top-level cut points found along the way.
struct Scan {
    words: Vec<Word>,
    /// Top-level cut candidates.
    cuts: Vec<Cut>,
}

/// A place a unit may be split.
struct Cut {
    /// Byte offset: the second newline of a blank line, or a `;`.
    at: usize,
    /// A blank line (else a `;`).
    blank: bool,
    /// The last code byte before it that is not whitespace.
    last: u8,
}

fn scan(sql: &str, segs: &[Segment], semicolons: bool) -> Scan {
    let b = sql.as_bytes();
    let mut words = Vec::new();
    let mut cuts = Vec::new();
    let mut depth = 0i32;
    // Only whitespace seen since the last newline (and that newline was in code).
    let mut line_clean = true;
    // A newline in code that began a line holding nothing so far: a blank line follows
    // if the next newline comes while the line is still clean.
    let mut last_newline: Option<usize> = None;
    let mut last = 0u8;
    for seg in segs {
        if seg.kind != SegKind::Code {
            line_clean = false;
            last_newline = None;
            if seg.kind != SegKind::Comment {
                // A string or quoted name ends the statement as well as a word does.
                last = b'\'';
            }
            continue;
        }
        let mut i = seg.start;
        while i < seg.end {
            let c = b[i];
            match c {
                b'\n' => {
                    if line_clean && depth == 0 && last_newline.is_some() {
                        cuts.push(Cut {
                            at: i,
                            blank: true,
                            last,
                        });
                    }
                    last_newline = Some(i);
                    line_clean = true;
                    i += 1;
                }
                b' ' | b'\t' | b'\r' => i += 1,
                b'(' => {
                    depth += 1;
                    line_clean = false;
                    last = c;
                    i += 1;
                }
                b')' => {
                    depth -= 1;
                    line_clean = false;
                    last = c;
                    i += 1;
                }
                b';' => {
                    if semicolons && depth == 0 {
                        cuts.push(Cut {
                            at: i,
                            blank: false,
                            last,
                        });
                    }
                    line_clean = false;
                    last = c;
                    i += 1;
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let start = i;
                    while i < seg.end && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    words.push(Word {
                        text: sql[start..i].to_ascii_uppercase(),
                        start,
                        line_start: line_clean,
                        depth,
                    });
                    line_clean = false;
                    last = b'a';
                }
                _ => {
                    line_clean = false;
                    last = c;
                    i += 1;
                }
            }
        }
    }
    Scan { words, cuts }
}

/// Whether the unit holds procedural code that must run as one piece.
fn is_procedural(words: &[Word]) -> bool {
    let w = |i: usize| words.get(i).map(|w| w.text.as_str());
    for (i, word) in words.iter().enumerate() {
        match word.text.as_str() {
            "DECLARE" | "LOOP" | "ELSIF" | "GOTO" => return true,
            "BEGIN" => {
                if !matches!(
                    w(i + 1),
                    Some("TRAN" | "TRANSACTION" | "WORK" | "DISTRIBUTED" | "ISOLATION") | None
                ) {
                    return true;
                }
            }
            "IF" | "WHILE" if word.line_start => return true,
            "FUNCTION" | "PROCEDURE" | "PROC" | "TRIGGER" | "PACKAGE" | "EVENT" => {
                // CREATE [OR REPLACE | OR ALTER] [DEFINER = …] FUNCTION …
                let lead = words[..i].iter().rev().take(6);
                if lead
                    .into_iter()
                    .any(|w| w.text == "CREATE" || w.text == "ALTER")
                {
                    return true;
                }
            }
            "DO" if i == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Whether the statement made of `words` is complete enough to end at a blank line.
fn is_complete(words: &[Word], last_byte: u8) -> bool {
    // Ends in a comma, an operator or an opening parenthesis.
    if matches!(
        last_byte,
        b',' | b'(' | b'=' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'|' | b'&' | b'.' | b'%'
    ) {
        return false;
    }
    let Some(last) = words.last() else {
        // Only comments so far: they drop out on their own.
        return true;
    };
    let before = words.len().checked_sub(2).map(|i| words[i].text.as_str());
    // `last_byte` is `a` after a word (`select 2` ends in a number, not in SELECT).
    if last_byte == b'a'
        && CONTINUES.contains(&last.text.as_str())
        && !(last.text == "ALL"
            && !matches!(before, Some("UNION" | "INTERSECT" | "EXCEPT" | "MINUS")))
    {
        return false;
    }
    // Words of the statement itself, not of its subqueries.
    let has = |kw: &[&str]| {
        words
            .iter()
            .skip(1)
            .any(|w| w.depth == words[0].depth && kw.contains(&w.text.as_str()))
    };
    match words[0].text.as_str() {
        "INSERT" | "UPSERT" | "REPLACE" => has(&["VALUES", "SELECT", "DEFAULT", "SET", "TABLE"]),
        "UPDATE" => has(&["SET"]),
        "MERGE" => has(&["WHEN"]),
        "WITH" => has(&["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES"]),
        _ => true,
    }
}

fn split_unit(
    sql: &str,
    unit: &StatementSpan,
    segs: &[Segment],
    semicolons: bool,
    out: &mut Vec<StatementSpan>,
) {
    let Scan { words, cuts } = scan(sql, segs, semicolons);
    if cuts.is_empty() || is_procedural(&words) {
        out.push(*unit);
        return;
    }
    let mut from = unit.start;
    // Words of the statement being built: words[w0..].
    let mut w0 = 0usize;
    for Cut { at, blank, last } in cuts {
        let w1 = words.partition_point(|w| w.start < at);
        if blank {
            // Only comments after the blank line: split them off (they drop out).
            if let Some(next) = words.get(w1) {
                if !STARTERS.contains(&next.text.as_str()) {
                    continue;
                }
                // `WITH (NOLOCK)` and other table / index options.
                if next.text == "WITH" && sql[next.start + 4..].trim_start().starts_with('(') {
                    continue;
                }
            }
            if !is_complete(&words[w0..w1], last) {
                continue;
            }
        }
        // A T-SQL statement keeps its `;`: MERGE must end with one.
        let end = if blank { at } else { at + 1 };
        push_trimmed(sql, segs, from, end, out);
        from = end;
        w0 = w1;
    }
    push_trimmed(sql, segs, from, unit.end, out);
}

/// Push `sql[from..to]` trimmed, unless it holds no code (only comments or whitespace).
fn push_trimmed(sql: &str, segs: &[Segment], from: usize, to: usize, out: &mut Vec<StatementSpan>) {
    let text = &sql[from..to];
    let s = from + (text.len() - text.trim_start().len());
    let e = to - (text.len() - text.trim_end().len());
    if e <= s {
        return;
    }
    let first = segs.partition_point(|seg| seg.end <= s);
    let has_code = segs[first..]
        .iter()
        .take_while(|seg| seg.start < e)
        .any(|seg| match seg.kind {
            SegKind::Code => !sql[seg.start.max(s)..seg.end.min(e)].trim().is_empty(),
            SegKind::Str | SegKind::Ident => true,
            SegKind::Comment => false,
        });
    if has_code {
        out.push(StatementSpan {
            start: s,
            end: e,
            line: 0,
            repeat: 1,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::dialect::{
        Dialect, mongo::MongoDialect, postgres::PostgresDialect, tsql::TSqlDialect,
    };

    fn pg(sql: &str) -> Vec<&str> {
        PostgresDialect
            .split_statements(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    fn ms(sql: &str) -> Vec<&str> {
        TSqlDialect
            .split_statements(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn blank_line_ends_a_statement() {
        assert_eq!(
            pg("select 1\nfrom a\n\nselect 2\n\n\nupdate t set x = 1"),
            ["select 1\nfrom a", "select 2", "update t set x = 1"]
        );
        // Semicolons still split, and blank lines between them change nothing.
        assert_eq!(pg("select 1;\n\nselect 2;"), ["select 1", "select 2"]);
        // Whitespace-only lines count as blank.
        assert_eq!(pg("select 1\n  \t\nselect 2"), ["select 1", "select 2"]);
    }

    #[test]
    fn blank_line_inside_a_statement_is_kept() {
        for sql in [
            "select a\n\nfrom t\n\nwhere x = 1",
            "select 1\nunion all\n\nselect 2",
            "insert into t (a, b)\n\nselect 1, 2",
            "insert into t\n\nvalues (1)",
            "update t\n\nset a = 1",
            "with x as (select 1)\n\nselect * from x",
            "select *\nfrom (\n\nselect 1\n\n) s",
            "create view v as\n\nselect 1",
            "select * from t\n\nwith (nolock)",
            "select 'a\n\nselect' as s",
            "select 1 /* x\n\nselect 2 */ + 1",
        ] {
            assert_eq!(pg(sql), [sql], "{sql:?}");
        }
    }

    #[test]
    fn procedural_units_stay_whole() {
        for sql in [
            "create function f() returns int as $$\nbegin\n\n  select 1;\nend $$ language plpgsql",
            "do $$\nbegin\n\nperform 1;\nend $$",
        ] {
            assert_eq!(pg(sql).len(), 1, "{sql:?}");
        }
        for sql in [
            "declare @a int = 1\n\nselect @a",
            "if object_id('t') is not null\n\ndrop table t",
            "create procedure p as\nbegin\n  select 1;\n\n  select 2;\nend",
        ] {
            assert_eq!(ms(sql), [sql], "{sql:?}");
        }
    }

    #[test]
    fn tsql_batches_split_like_dbeaver() {
        assert_eq!(
            ms("select 1;\nselect 2\n\nselect 3\nGO\nselect 4"),
            ["select 1;", "select 2", "select 3", "select 4"]
        );
        // MERGE must keep its terminator.
        assert_eq!(
            ms("merge t using s on t.id = s.id when matched then delete;\nselect 1"),
            [
                "merge t using s on t.id = s.id when matched then delete;",
                "select 1"
            ]
        );
        // BEGIN TRAN is not a block.
        assert_eq!(
            ms("begin tran\n\nupdate t set a = 1\n\ncommit"),
            ["begin tran", "update t set a = 1", "commit"]
        );
        // `GO 3` repeats its batch: kept whole.
        assert_eq!(
            ms("insert into t values (1)\n\nselect 1\nGO 3"),
            ["insert into t values (1)\n\nselect 1"]
        );
    }

    #[test]
    fn comment_only_parts_are_dropped_and_lines_kept() {
        let sql = "-- header\n\nselect 1\n\n-- trailing note";
        let spans = PostgresDialect.split_statements(sql);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text(sql), "select 1");
        assert_eq!(spans[0].line, 3);
    }

    #[test]
    fn cursor_picks_the_blank_line_delimited_statement() {
        let sql = "select 1\nfrom a\n\nselect 2\nfrom b";
        let at = |off| PostgresDialect.statement_at(sql, off).map(|s| s.text(sql));
        assert_eq!(at(3), Some("select 1\nfrom a"));
        assert_eq!(at(sql.len()), Some("select 2\nfrom b"));
    }

    #[test]
    fn large_scripts_split_in_linear_time() {
        // 20,000 statements (~1 MB): splitting used to count lines and scan segments from
        // the top for every statement.
        let mut sql = String::new();
        for i in 0..20_000 {
            sql.push_str(&format!("insert into t (a, b) values ({i}, 'row {i}');\n"));
        }
        let t = std::time::Instant::now();
        let spans = PostgresDialect.split_statements(&sql);
        let ms_spans = TSqlDialect.split_statements(&sql);
        assert_eq!((spans.len(), ms_spans.len()), (20_000, 20_000));
        assert_eq!(spans[19_999].line, 20_000);
        assert_eq!(ms_spans[19_999].line, 20_000);
        // Generous even for an unoptimised build; quadratic took minutes.
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn mongo_is_unchanged() {
        let sql = "show dbs\n\ndb.a.find()";
        assert_eq!(
            MongoDialect.split_statements(sql),
            MongoDialect.split_script(sql)
        );
    }
}
