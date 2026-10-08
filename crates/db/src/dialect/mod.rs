//! SQL dialects: identifier quoting, script splitting, parameters, row limits, literals.

pub mod lexer;
pub mod oracle;
pub mod postgres;
pub mod snowflake;
pub mod sqlite;
pub mod tsql;

use crate::catalog::ObjectKind;
use crate::error::ErrorPosition;
use crate::value::{self, Engine, Value};

pub use lexer::{Flavor, SegKind, Segment};

/// One executable unit inside a script (a statement, or a T-SQL batch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementSpan {
    /// Start byte of the statement text.
    pub start: usize,
    /// End byte (exclusive), excluding the terminator.
    pub end: usize,
    /// 1-based line number of `start`.
    pub line: u32,
    /// How many times to run it (T-SQL `GO n`).
    pub repeat: u32,
}

impl StatementSpan {
    /// The statement text.
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.start..self.end]
    }
}

/// How placeholders are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamStyle {
    /// `$1`, `$2` (PostgreSQL).
    Dollar,
    /// `:name` (rewritten for the engine).
    Colon,
    /// `@name` (SQL Server).
    At,
}

/// A placeholder found in a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamRef {
    /// Display name (`$1`, `:customer`, `@id`).
    pub name: String,
    /// Style.
    pub style: ParamStyle,
    /// Byte range in the statement.
    pub start: usize,
    /// End byte (exclusive).
    pub end: usize,
}

/// Engine-specific SQL behavior. UI and core code go through this trait instead of
/// branching on the engine.
pub trait Dialect: Send + Sync {
    /// The engine.
    fn engine(&self) -> Engine;
    /// Lexical flavour.
    fn flavor(&self) -> Flavor;
    /// Quote an identifier when needed.
    fn quote_ident(&self, ident: &str) -> String;
    /// `schema.name`, quoted as needed.
    fn qualified(&self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }
    /// Split a script into executable units.
    fn split_script(&self, sql: &str) -> Vec<StatementSpan>;
    /// `SELECT * FROM <qualified> ` limited to `limit` rows.
    fn select_rows(&self, qualified: &str, limit: u64) -> String;
    /// `SELECT` of `cols` (all columns when empty) from `qualified`, limited to `limit`
    /// rows with the engine's own syntax. Column names are quoted here.
    fn select_template(&self, qualified: &str, cols: &[String], limit: u64) -> String;
    /// `INSERT` of `cols` into `qualified`, with `NULL` values to fill in.
    fn insert_template(&self, qualified: &str, cols: &[String]) -> String {
        let (names, values) = if cols.is_empty() {
            ("column1".to_owned(), "NULL".to_owned())
        } else {
            (
                cols.iter()
                    .map(|c| self.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", "),
                vec!["NULL"; cols.len()].join(", "),
            )
        };
        format!("INSERT INTO {qualified} ({names})\nVALUES ({values});")
    }
    /// `UPDATE` setting the non-key `cols` of the row picked by `pk` (every column when
    /// there is no key). Values are `NULL` placeholders, so it changes nothing as written.
    fn update_template(&self, qualified: &str, cols: &[String], pk: &[String]) -> String {
        let set: Vec<&String> = cols.iter().filter(|c| !pk.contains(c)).collect();
        let set: Vec<&String> = if set.is_empty() {
            cols.iter().collect()
        } else {
            set
        };
        let set = if set.is_empty() {
            "  column1 = NULL".to_owned()
        } else {
            set.iter()
                .map(|c| format!("  {} = NULL", self.quote_ident(c)))
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let key = if pk.is_empty() { cols } else { pk };
        format!(
            "UPDATE {qualified}\nSET\n{set}\n{};",
            where_clause(self, key)
        )
    }
    /// `DELETE` of the row picked by `pk`.
    fn delete_template(&self, qualified: &str, pk: &[String]) -> String {
        format!("DELETE FROM {qualified}\n{};", where_clause(self, pk))
    }
    /// `DROP` statement for an object of `kind`.
    fn script_drop(&self, kind: ObjectKind, qualified: &str) -> String {
        format!("DROP {} {qualified};", drop_keyword(kind))
    }
    /// Placeholders in one statement, in order of appearance.
    fn find_params(&self, sql: &str) -> Vec<ParamRef>;
    /// Rewrite named placeholders to the engine's native form. Returns the SQL and the
    /// unique parameter names in binding order.
    fn bind_params(&self, sql: &str) -> (String, Vec<String>);
    /// A `sqlparser` dialect for parsing.
    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect>;
    /// Render a value as a SQL literal.
    fn literal(&self, v: &Value) -> String;
    /// Map a server error position to a 1-based (line, column) within `statement`.
    fn error_line_col(&self, statement: &str, pos: ErrorPosition) -> (u32, u32) {
        match pos {
            ErrorPosition::Offset(chars) => line_col_at_char(statement, chars.saturating_sub(1)),
            ErrorPosition::Line(line) => (line.max(1), 1),
        }
    }
    /// Keywords for highlighting and completion (upper case).
    fn keywords(&self) -> &'static [&'static str];
    /// Common built-in functions (lower case).
    fn functions(&self) -> &'static [&'static str];
    /// The schema unqualified names resolve to by default.
    fn default_schema(&self) -> &'static str;
    /// Object folders the schema explorer shows under a schema, in order.
    fn object_folders(&self) -> &'static [ObjectKind] {
        &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ]
    }

    /// The unit containing byte `offset` (statement at cursor). Falls back to the closest
    /// preceding unit, then the following one.
    fn statement_at(&self, sql: &str, offset: usize) -> Option<StatementSpan> {
        let spans = self.split_script(sql);
        // Inside, or between the end of a statement and its terminator / end of line.
        if let Some(s) = spans.iter().find(|s| s.start <= offset && offset <= s.end) {
            return Some(*s);
        }
        let prev = spans.iter().rev().find(|s| s.end <= offset).copied();
        if let Some(p) = prev {
            // Only take the previous statement if the cursor is on its last line.
            let between = &sql[p.end..offset.min(sql.len())];
            if !between.contains('\n') {
                return Some(p);
            }
        }
        spans.iter().find(|s| s.start >= offset).copied().or(prev)
    }
}

/// The dialect for `engine`.
pub fn dialect_for(engine: Engine) -> &'static dyn Dialect {
    match engine {
        Engine::Postgres => &postgres::PostgresDialect,
        Engine::SqlServer => &tsql::TSqlDialect,
        Engine::D1 => &sqlite::SqliteDialect,
        Engine::Snowflake => &snowflake::SnowflakeDialect,
        Engine::Oracle => &oracle::OracleDialect,
    }
}

/// `WHERE k1 = NULL AND k2 = NULL` over `key`; without a key, a condition that matches
/// nothing so the template is harmless until edited.
fn where_clause<D: Dialect + ?Sized>(d: &D, key: &[String]) -> String {
    if key.is_empty() {
        return "-- No primary key: write the condition\nWHERE 1 = 0".to_owned();
    }
    let conds: Vec<String> = key
        .iter()
        .map(|c| format!("{} = NULL", d.quote_ident(c)))
        .collect();
    format!("WHERE {}", conds.join("\n  AND "))
}

/// The select list of a template: `*`, or one quoted column per line.
pub(crate) fn select_list<D: Dialect + ?Sized>(d: &D, cols: &[String]) -> String {
    if cols.is_empty() {
        return " *".to_owned();
    }
    let cols: Vec<String> = cols
        .iter()
        .map(|c| format!("  {}", d.quote_ident(c)))
        .collect();
    format!("\n{}", cols.join(",\n"))
}

/// The object-type keyword of `DROP <kind>`.
pub(crate) fn drop_keyword(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Type => "TYPE",
        ObjectKind::Synonym => "SYNONYM",
    }
}

/// 1-based (line, column) of the `char_index`-th character (0-based).
pub fn line_col_at_char(text: &str, char_index: u32) -> (u32, u32) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in text.chars().enumerate() {
        if i as u32 == char_index {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// 1-based line number of byte `offset`.
pub fn line_of_byte(text: &str, offset: usize) -> u32 {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
        + 1
}

/// Split on `;` in code, trimming whitespace and dropping units without code.
pub(crate) fn split_on_semicolons(sql: &str, flavor: Flavor) -> Vec<StatementSpan> {
    let segs = lexer::segments(sql, flavor);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut has_code = false;

    let finish = |out: &mut Vec<StatementSpan>, start: usize, end: usize, has_code: bool| {
        if !has_code {
            return;
        }
        let text = &sql[start..end];
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        let s = start + lead;
        let e = end - trail;
        if e > s {
            out.push(StatementSpan {
                start: s,
                end: e,
                line: line_of_byte(sql, s),
                repeat: 1,
            });
        }
    };

    for seg in &segs {
        match seg.kind {
            SegKind::Code => {
                let text = &sql[seg.start..seg.end];
                let mut seg_pos = seg.start;
                for part in text.split_inclusive(';') {
                    let part_end = seg_pos + part.len();
                    let body = part.strip_suffix(';').unwrap_or(part);
                    if !body.trim().is_empty() {
                        has_code = true;
                    }
                    if part.ends_with(';') {
                        finish(&mut out, start, part_end - 1, has_code);
                        start = part_end;
                        has_code = false;
                    }
                    seg_pos = part_end;
                }
            }
            SegKind::Str | SegKind::Ident => {
                has_code = true;
            }
            SegKind::Comment => {}
        }
    }
    finish(&mut out, start, sql.len(), has_code);
    out
}

/// Escape a string literal with doubled single quotes.
pub(crate) fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// Shared literal formatting for date/time values.
pub(crate) fn temporal_text(v: &Value) -> Option<String> {
    let mut s = String::new();
    match v {
        Value::Date(d) => value::write_date(&mut s, *d),
        Value::Time(t) => value::write_time(&mut s, *t),
        Value::Timestamp(t) => value::write_timestamp(&mut s, *t, false),
        Value::TimestampTz(t) => value::write_timestamp(&mut s, *t, true),
        _ => return None,
    }
    Some(s)
}

/// Whether an identifier can be written without quotes (lower-case, not a keyword).
pub(crate) fn is_plain_ident(s: &str, keywords: &[&str], allow_upper: bool) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let ok_first =
        first == '_' || first.is_ascii_lowercase() || (allow_upper && first.is_ascii_uppercase());
    ok_first
        && chars.all(|c| {
            c == '_'
                || c.is_ascii_digit()
                || c.is_ascii_lowercase()
                || (allow_upper && c.is_ascii_uppercase())
        })
        && !keywords.iter().any(|k| k.eq_ignore_ascii_case(s))
}

/// Common SQL keywords shared by both dialects.
pub(crate) const COMMON_KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "NOT",
    "NULL",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "REFERENCES",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [ObjectKind; 8] = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Sequence,
        ObjectKind::Type,
        ObjectKind::Synonym,
    ];

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Every template of every dialect, for a table with a quoted column and a
    /// two-column key, and for one without columns or key.
    #[test]
    fn templates() {
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
        ] {
            let d = dialect_for(engine);
            let q = d.qualified("sales", "order");
            let cols = names(&["id", "line", "Order Date", "total"]);
            let pk = names(&["id", "line"]);
            let name = format!("{engine:?}").to_lowercase();
            insta::assert_snapshot!(
                format!("{name}_select"),
                format!(
                    "{}\n\n{}",
                    d.select_template(&q, &cols, 100),
                    d.select_template(&q, &[], 100)
                )
            );
            insta::assert_snapshot!(
                format!("{name}_insert"),
                format!(
                    "{}\n\n{}",
                    d.insert_template(&q, &cols),
                    d.insert_template(&q, &[])
                )
            );
            insta::assert_snapshot!(
                format!("{name}_update"),
                format!(
                    "{}\n\n{}",
                    d.update_template(&q, &cols, &pk),
                    d.update_template(&q, &cols, &[])
                )
            );
            insta::assert_snapshot!(
                format!("{name}_delete"),
                format!(
                    "{}\n\n{}",
                    d.delete_template(&q, &pk),
                    d.delete_template(&q, &[])
                )
            );
            let drops: Vec<String> = KINDS.iter().map(|k| d.script_drop(*k, &q)).collect();
            insta::assert_snapshot!(format!("{name}_drop"), drops.join("\n"));
        }
    }

    #[test]
    fn line_col() {
        assert_eq!(line_col_at_char("ab\ncd", 0), (1, 1));
        assert_eq!(line_col_at_char("ab\ncd", 4), (2, 2));
        assert_eq!(line_of_byte("a\nb\nc", 4), 3);
    }
}
