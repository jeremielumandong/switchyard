//! Fold regions for the SQL editor, from the dialect's own lexer and statement splitter
//! (strings, comments, dollar quotes and `GO` respected): statements, parenthesised blocks
//! and block comments that span at least three lines.
//!
//! gpui-kit derives folds from tree-sitter edits incrementally, but for SQL its candidates
//! went missing while typing, so the SQL tab supplies them (see `docs/DECISIONS.md`).

use gpui_kit::base::input::FoldRange;
use switchyard_core::db::Dialect;
use switchyard_core::db::dialect::SegKind;

/// Line (0-based) of each byte offset, by binary search over line starts.
struct Lines(Vec<usize>);

impl Lines {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self(starts)
    }

    fn of(&self, offset: usize) -> usize {
        self.0.partition_point(|&s| s <= offset).saturating_sub(1)
    }
}

/// Fold regions as (first line, last line), sorted, one per first line (the longest).
pub fn sql_fold_lines(dialect: &dyn Dialect, text: &str) -> Vec<(usize, usize)> {
    let lines = Lines::new(text);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut add = |start: usize, end: usize| {
        if end >= start + 2 {
            out.push((start, end));
        }
    };
    let segments = switchyard_core::db::dialect::lexer::segments(text, dialect.flavor());
    // Where real code starts at or after `at`: past whitespace and comments.
    let code_from = |at: usize| -> usize {
        segments
            .iter()
            .filter(|g| g.end > at && g.kind != SegKind::Comment)
            .find_map(|g| {
                let from = g.start.max(at);
                let rest = &text[from..g.end];
                let skip = rest.len() - rest.trim_start().len();
                (skip < rest.len()).then_some(from + skip)
            })
            .unwrap_or(at)
    };
    for s in dialect.split_script(text) {
        let start = code_from(s.start).min(s.end);
        let end = start + text[start..s.end].trim_end().len();
        if end > start {
            add(lines.of(start), lines.of(end - 1));
        }
    }
    let mut open: Vec<usize> = Vec::new();
    for seg in segments.iter().copied() {
        match seg.kind {
            SegKind::Code => {
                for (i, ch) in text[seg.start..seg.end].char_indices() {
                    match ch {
                        '(' => open.push(seg.start + i),
                        ')' => {
                            if let Some(o) = open.pop() {
                                add(lines.of(o), lines.of(seg.start + i));
                            }
                        }
                        _ => {}
                    }
                }
            }
            SegKind::Comment if text[seg.start..seg.end].starts_with("/*") => {
                add(lines.of(seg.start), lines.of(seg.end.saturating_sub(1)));
            }
            _ => {}
        }
    }
    // One region per first line: the longest.
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    out.dedup_by_key(|r| r.0);
    out
}

/// [`sql_fold_lines`] as editor fold candidates.
pub fn sql_folds(dialect: &dyn Dialect, text: &str) -> Vec<FoldRange> {
    sql_fold_lines(dialect, text)
        .into_iter()
        .map(|(s, e)| FoldRange::new(s, e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::dialect::{postgres::PostgresDialect, tsql::TSqlDialect};

    #[test]
    fn statements_parens_and_comments() {
        let sql = "select\n  a,\n  b\nfrom t;\n\
                   with x as (\n  select 1\n  from y\n)\nselect * from x;\n\
                   /* one\n two\n three */\n\
                   select 1;\n";
        assert_eq!(
            sql_fold_lines(&PostgresDialect, sql),
            [(0, 3), (4, 8), (9, 11)],
            "a statement, a CTE statement (its body starts on the same line), a comment"
        );
    }

    #[test]
    fn strings_and_short_blocks_do_not_fold() {
        let sql = "select '(\n\n\n' as s;\nselect (1,\n2);\n";
        // The string spans lines but is one statement of 4 lines; the parens inside the
        // string are ignored, and the 2-line tuple is too short.
        assert_eq!(sql_fold_lines(&PostgresDialect, sql), [(0, 3)]);
    }

    #[test]
    fn tsql_batches() {
        let sql = "create procedure p as\nbegin\n  select 1\nend\nGO\nselect 2\n";
        assert_eq!(sql_fold_lines(&TSqlDialect, sql), [(0, 3)]);
    }

    #[test]
    fn line_lookup() {
        let l = Lines::new("ab\ncd\n\nef");
        assert_eq!(
            [l.of(0), l.of(2), l.of(3), l.of(6), l.of(7)],
            [0, 0, 1, 2, 3]
        );
    }
}
