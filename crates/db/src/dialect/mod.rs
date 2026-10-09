//! SQL dialects: identifier quoting, script splitting, parameters, row limits, literals.

pub mod lexer;
pub mod mongo;
pub mod mysql;
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
    /// Script as CREATE: catalog DDL (`ObjectDetail::ddl`) ready to run, ending with the
    /// terminator the engine's script runner expects.
    fn script_create(&self, _kind: ObjectKind, ddl: &str) -> String {
        terminated(ddl)
    }
    /// Script as DROP + CREATE: [`Self::script_drop`] then [`Self::script_create`],
    /// separated by the engine's batch separator where it needs one.
    fn script_drop_create(&self, kind: ObjectKind, qualified: &str, ddl: &str) -> String {
        format!(
            "{}\n\n{}",
            self.script_drop(kind, qualified),
            self.script_create(kind, ddl)
        )
    }
    /// Script as EXEC: a call of the function or procedure `qualified` with a `NULL` for
    /// each of its input `params` (parameter names; empty for an unnamed one).
    fn script_exec(&self, kind: ObjectKind, qualified: &str, params: &[String]) -> String {
        let args = params
            .iter()
            .map(|p| commented_null(p))
            .collect::<Vec<_>>()
            .join(", ");
        match kind {
            ObjectKind::Procedure => format!("CALL {qualified}({args});"),
            _ => format!("SELECT {qualified}({args});"),
        }
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

    /// Folders the schema explorer shows at the database level, beside the schemas:
    /// objects outside any schema (users and roles, Agent jobs, extensions; DBX-5c).
    fn server_folders(&self) -> &'static [ObjectKind] {
        &[]
    }

    /// Folder label in the schema explorer for objects of `kind` (MongoDB calls tables
    /// collections).
    fn folder_label(&self, kind: ObjectKind) -> &'static str {
        kind.folder_label()
    }

    /// Classify a statement or script without `sqlparser`, for engines whose statements
    /// are not SQL. `None` (the default) parses it as SQL.
    fn classify(&self, _sql: &str) -> Option<crate::guard::Classification> {
        None
    }

    /// Check a data-view filter for engines whose filters are not SQL expressions.
    /// `None` (the default) checks it as one SQL expression.
    fn check_filter(&self, _cond: &str) -> Option<Result<(), String>> {
        None
    }

    /// Whether the engine can list an object's dependencies (DBX-5a).
    fn supports_dependencies(&self) -> bool {
        true
    }

    /// Whether grid edits are document statements matched by `_id`
    /// ([`crate::mongo::edit`]) rather than SQL matched by primary key ([`crate::edit`]).
    fn edits_documents(&self) -> bool {
        false
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

    /// Whether a SQL tab offers a database / schema switcher for this engine.
    fn switches_context(&self) -> bool {
        true
    }

    /// The statement that makes `database` current in an open session, or `None` when
    /// the session must reconnect to it (PostgreSQL binds a connection to one database).
    fn use_database(&self, _database: &str) -> Option<String> {
        None
    }

    /// The statement that makes `schema` the default for unqualified names, or `None`
    /// when the engine cannot switch schemas per session (SQL Server, D1).
    fn use_schema(&self, _schema: &str) -> Option<String> {
        None
    }

    /// One page of `qualified` for the table data view (DBX-3a): `cols` (all when empty),
    /// filtered by `where_` (a condition already validated as one expression), sorted by
    /// `order`, `limit` rows after skipping `offset`. `LIMIT … OFFSET` by default.
    fn select_page(
        &self,
        qualified: &str,
        cols: &[String],
        where_: Option<&str>,
        order: &[SortKey],
        limit: u64,
        offset: u64,
    ) -> String {
        format!(
            "{}\nLIMIT {limit} OFFSET {offset}",
            page_head(self, qualified, cols, where_, order)
        )
    }

    /// `INSERT` of one row (DBX-3b): each column with its literal, or `None` for the column
    /// default (written `DEFAULT`). With no columns, `DEFAULT VALUES`.
    fn insert_row(&self, qualified: &str, values: &[(String, Option<String>)]) -> String {
        if values.is_empty() {
            return format!("INSERT INTO {qualified} DEFAULT VALUES;");
        }
        let names: Vec<String> = values.iter().map(|(c, _)| self.quote_ident(c)).collect();
        let vals: Vec<&str> = values
            .iter()
            .map(|(_, v)| v.as_deref().unwrap_or("DEFAULT"))
            .collect();
        format!(
            "INSERT INTO {qualified} ({}) VALUES ({});",
            names.join(", "),
            vals.join(", ")
        )
    }
}

/// A column of a server-side `ORDER BY`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// Column name (unquoted).
    pub column: String,
    /// Descending instead of ascending.
    pub descending: bool,
}

impl SortKey {
    /// Ascending on `column`.
    pub fn asc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: false,
        }
    }
}

/// `SELECT … FROM … [WHERE …] [ORDER BY …]` of [`Dialect::select_page`], one clause per line.
pub(crate) fn page_head<D: Dialect + ?Sized>(
    d: &D,
    qualified: &str,
    cols: &[String],
    where_: Option<&str>,
    order: &[SortKey],
) -> String {
    let list = if cols.is_empty() {
        "*".to_owned()
    } else {
        cols.iter()
            .map(|c| d.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut sql = format!("SELECT {list}\nFROM {qualified}");
    if let Some(w) = where_.map(str::trim).filter(|w| !w.is_empty()) {
        sql.push_str("\nWHERE ");
        sql.push_str(w);
    }
    if !order.is_empty() {
        sql.push_str("\nORDER BY ");
        sql.push_str(&order_list(d, order));
    }
    sql
}

/// `"a" ASC, "b" DESC`.
pub(crate) fn order_list<D: Dialect + ?Sized>(d: &D, order: &[SortKey]) -> String {
    order
        .iter()
        .map(|k| {
            format!(
                "{} {}",
                d.quote_ident(&k.column),
                if k.descending { "DESC" } else { "ASC" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The dialect for `engine`.
pub fn dialect_for(engine: Engine) -> &'static dyn Dialect {
    match engine {
        Engine::Postgres => &postgres::PostgresDialect,
        Engine::SqlServer => &tsql::TSqlDialect,
        Engine::D1 => &sqlite::SqliteDialect::D1,
        Engine::Sqlite => &sqlite::SqliteDialect::LOCAL,
        Engine::Snowflake => &snowflake::SnowflakeDialect,
        Engine::Oracle => &oracle::OracleDialect,
        Engine::MySql => &mysql::MySqlDialect,
        Engine::MongoDb => &mongo::MongoDialect,
        // Redis has no SQL; nothing parses statements for it, so any dialect will do.
        Engine::Redis => &postgres::PostgresDialect,
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

/// `ddl` trimmed and ending with `;` (on its own line after a trailing `--` comment).
pub(crate) fn terminated(ddl: &str) -> String {
    let ddl = ddl.trim();
    if ddl.is_empty() || ddl.ends_with(';') {
        return ddl.to_owned();
    }
    let last = ddl.lines().last().unwrap_or("");
    if last.contains("--") {
        format!("{ddl}\n;")
    } else {
        format!("{ddl};")
    }
}

/// `NULL`, labelled with the parameter name in a comment when there is one.
pub(crate) fn commented_null(param: &str) -> String {
    if param.is_empty() {
        "NULL".to_owned()
    } else {
        format!("/* {} */ NULL", param.replace("*/", "* /"))
    }
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
        // Read-only kinds (DBX-5c): never offered, spelled for completeness.
        ObjectKind::Role => "ROLE",
        ObjectKind::Job => "JOB",
        ObjectKind::Extension => "EXTENSION",
        ObjectKind::Package => "PACKAGE",
        ObjectKind::Stage => "STAGE",
        ObjectKind::Task => "TASK",
        ObjectKind::Pipe => "PIPE",
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

    /// DBX-5c: schema-level folders per engine and the database-level (server) ones.
    #[test]
    fn folders_per_engine() {
        use ObjectKind as K;
        let server = |e| dialect_for(e).server_folders().to_vec();
        let has = |e, k| dialect_for(e).object_folders().contains(&k);
        assert_eq!(server(Engine::Postgres), [K::Role, K::Extension]);
        assert_eq!(server(Engine::SqlServer), [K::Role, K::Job]);
        assert_eq!(server(Engine::Oracle), [K::Role]);
        assert_eq!(server(Engine::Snowflake), [K::Role]);
        assert!(server(Engine::D1).is_empty());
        assert_eq!(server(Engine::MySql), [K::Role]);
        assert!(has(Engine::Oracle, K::Package));
        for k in [K::Stage, K::Task, K::Pipe] {
            assert!(has(Engine::Snowflake, k));
        }
        for e in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
            Engine::MySql,
        ] {
            let d = dialect_for(e);
            // Server-level kinds never sit under a schema, and schema kinds never at the top.
            assert!(d.object_folders().iter().all(|k| !k.is_server_level()));
            assert!(d.server_folders().iter().all(|k| k.is_server_level()));
            assert_eq!(d.supports_dependencies(), e != Engine::D1);
        }
    }

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
            Engine::MySql,
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

    /// Script as CREATE, DROP + CREATE and EXEC of every dialect, for a table and a
    /// procedure, and calls with and without parameters.
    #[test]
    fn scripts() {
        let table = "CREATE TABLE sales.orders (id int)";
        let proc = "CREATE PROCEDURE sales.archive AS\nBEGIN\n  NULL;\nEND;";
        let commented = "CREATE VIEW sales.v AS SELECT 1 AS x -- one";
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
            Engine::MySql,
        ] {
            let d = dialect_for(engine);
            let name = format!("{engine:?}").to_lowercase();
            let t = d.qualified("sales", "orders");
            let p = d.qualified("sales", "archive");
            insta::assert_snapshot!(
                format!("{name}_script_create"),
                [
                    d.script_create(ObjectKind::Table, table),
                    d.script_create(ObjectKind::Procedure, proc),
                    d.script_create(ObjectKind::View, commented),
                    d.script_create(ObjectKind::Table, "CREATE TABLE t (id int);\n"),
                ]
                .join("\n\n")
            );
            insta::assert_snapshot!(
                format!("{name}_script_drop_create"),
                format!(
                    "{}\n\n{}",
                    d.script_drop_create(ObjectKind::Table, &t, table),
                    d.script_drop_create(ObjectKind::Procedure, &p, proc)
                )
            );
            let at = if engine == Engine::SqlServer { "@" } else { "" };
            let params = vec![format!("{at}cid"), format!("{at}since")];
            insta::assert_snapshot!(
                format!("{name}_script_exec"),
                [
                    d.script_exec(ObjectKind::Procedure, &p, &params),
                    d.script_exec(ObjectKind::Procedure, &p, &[]),
                    d.script_exec(ObjectKind::Function, &p, &params),
                    d.script_exec(ObjectKind::Function, &p, &[String::new()]),
                ]
                .join("\n\n")
            );
        }
    }

    /// One page with every option, and the bare first page, per dialect (DBX-3a).
    #[test]
    fn pages() {
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
            Engine::MySql,
        ] {
            let d = dialect_for(engine);
            let q = d.qualified("sales", "order");
            let order = [
                SortKey {
                    column: "Order Date".into(),
                    descending: true,
                },
                SortKey::asc("id"),
            ];
            let name = format!("{engine:?}").to_lowercase();
            insta::assert_snapshot!(
                format!("{name}_select_page"),
                [
                    d.select_page(
                        &q,
                        &names(&["id", "Order Date"]),
                        Some("total > 100 AND status = 'open'"),
                        &order,
                        500,
                        1000
                    ),
                    d.select_page(&q, &[], None, &[], 100, 0),
                    d.select_page(&q, &[], Some("  "), &[SortKey::asc("id")], 100, 100),
                ]
                .join("\n\n")
            );
        }
    }

    /// One row with a value, a default and a NULL, and a row of defaults only (DBX-3b).
    #[test]
    fn insert_rows() {
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
            Engine::MySql,
        ] {
            let d = dialect_for(engine);
            let q = d.qualified("sales", "order");
            let name = format!("{engine:?}").to_lowercase();
            let row = [
                ("Order Date".to_owned(), Some("'2026-10-08'".to_owned())),
                ("status".to_owned(), None),
                ("note".to_owned(), Some("NULL".to_owned())),
            ];
            insta::assert_snapshot!(
                format!("{name}_insert_row"),
                [d.insert_row(&q, &row), d.insert_row(&q, &[])].join("\n\n")
            );
        }
    }

    #[test]
    fn line_col() {
        assert_eq!(line_col_at_char("ab\ncd", 0), (1, 1));
        assert_eq!(line_col_at_char("ab\ncd", 4), (2, 2));
        assert_eq!(line_of_byte("a\nb\nc", 4), 3);
    }
}

#[cfg(test)]
mod context_tests;
