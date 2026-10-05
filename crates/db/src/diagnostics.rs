//! Live parse diagnostics and server-error positions.

use sqlparser::parser::Parser;

use crate::dialect::{Dialect, line_col_at_char, line_of_byte};
use crate::error::ServerError;

/// A problem to underline in the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1-based line in the whole buffer.
    pub line: u32,
    /// 1-based column (characters).
    pub column: u32,
    /// Byte offset in the whole buffer where the underline starts.
    pub offset: usize,
    /// Message.
    pub message: String,
}

/// Statements `sqlparser` understands well enough to report errors on. Others (procedural
/// blocks, engine-specific DDL) are skipped to avoid false positives.
const CHECKED_LEADS: &[&str] = &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "VALUES"];

fn leading_keyword(stmt: &str) -> String {
    let mut s = stmt.trim_start();
    // Skip leading comments.
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    s.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Extract `Line: X, Column: Y` from a sqlparser error message.
fn parse_location(msg: &str) -> Option<(u32, u32)> {
    let at = msg.rfind("Line: ")?;
    let rest = &msg[at + 6..];
    let (line, rest) = rest.split_once(',')?;
    let col = rest.trim().strip_prefix("Column: ")?;
    let col: String = col.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((line.trim().parse().ok()?, col.parse().ok()?))
}

fn clean_message(msg: &str) -> String {
    let msg = msg
        .trim_start_matches("sql parser error: ")
        .trim_start_matches("SQL parser error: ");
    match msg.rfind(" at Line: ") {
        Some(i) => msg[..i].to_owned(),
        None => msg.to_owned(),
    }
}

/// Parse every statement in `script` and report syntax errors with buffer positions.
pub fn parse_diagnostics(dialect: &dyn Dialect, script: &str) -> Vec<Diagnostic> {
    let pd = dialect.parser_dialect();
    let mut out = Vec::new();
    for span in dialect.split_script(script) {
        let text = span.text(script);
        if !CHECKED_LEADS.contains(&leading_keyword(text).as_str()) {
            continue;
        }
        if let Err(e) = Parser::parse_sql(pd.as_ref(), text) {
            let msg = e.to_string();
            let (rel_line, col) = parse_location(&msg).unwrap_or((1, 1));
            let line = span.line + rel_line - 1;
            // Byte offset of the reported position.
            let line_start = text
                .split_inclusive('\n')
                .take(rel_line.saturating_sub(1) as usize)
                .map(str::len)
                .sum::<usize>();
            let col_bytes: usize = text[line_start..]
                .chars()
                .take(col.saturating_sub(1) as usize)
                .map(char::len_utf8)
                .sum();
            let offset = span.start + line_start + col_bytes;
            let column = if rel_line == 1 {
                // The statement may not start at column 1 of its line.
                let line_begin = script[..span.start].rfind('\n').map_or(0, |i| i + 1);
                script[line_begin..span.start].chars().count() as u32 + col
            } else {
                col
            };
            out.push(Diagnostic {
                line,
                column,
                offset,
                message: clean_message(&msg),
            });
        }
    }
    out
}

/// Map a server error on a statement that starts at byte `stmt_start` of `script` to an
/// absolute (line, column).
pub fn server_error_location(
    dialect: &dyn Dialect,
    script: &str,
    stmt_start: usize,
    stmt_end: usize,
    err: &ServerError,
) -> Option<(u32, u32)> {
    let pos = err.position?;
    let stmt = &script[stmt_start..stmt_end];
    let (rel_line, rel_col) = dialect.error_line_col(stmt, pos);
    let line = line_of_byte(script, stmt_start) + rel_line - 1;
    let column = if rel_line == 1 {
        let line_begin = script[..stmt_start].rfind('\n').map_or(0, |i| i + 1);
        script[line_begin..stmt_start].chars().count() as u32 + rel_col
    } else {
        rel_col
    };
    Some((line, column))
}

/// 1-based (line, column) of a character index; re-exported for UI use.
pub fn line_col(text: &str, char_index: u32) -> (u32, u32) {
    line_col_at_char(text, char_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::postgres::PostgresDialect;
    use crate::error::ErrorPosition;

    #[test]
    fn reports_parse_error_position() {
        let script = "select 1;\n\nselect a,\n  from t;";
        let d = parse_diagnostics(&PostgresDialect, script);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].line, 4);
        assert!(d[0].column >= 3, "{d:?}");
        assert!(!d[0].message.contains("Line:"));
    }

    #[test]
    fn skips_unchecked_statements() {
        let d = parse_diagnostics(&PostgresDialect, "do $$ begin perform 1; end $$;");
        assert!(d.is_empty());
        assert!(parse_diagnostics(&PostgresDialect, "select 1; select 2").is_empty());
    }

    #[test]
    fn maps_server_offset() {
        let script =
            "-- top\nSELECT c.id\nFROM customers c\nJOIN orders o ON o.customer_ud = c.id;";
        let start = script.find("SELECT").unwrap();
        let end = script.len() - 1;
        let pos = script[start..].find("o.customer_ud").unwrap() as u32 + 1;
        let err = ServerError {
            severity: "ERROR".into(),
            code: Some("42703".into()),
            message: "column o.customer_ud does not exist".into(),
            detail: None,
            hint: None,
            position: Some(ErrorPosition::Offset(pos)),
        };
        assert_eq!(
            server_error_location(&PostgresDialect, script, start, end, &err),
            Some((4, 18))
        );
    }
}
