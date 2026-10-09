//! MySQL dialect (also MariaDB). A MySQL "schema" is a database: the explorer lists
//! databases as schemas, and switching schemas is `USE`.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, is_plain_ident, line_of_byte,
};
use crate::catalog::ObjectKind;
use crate::value::{self, Engine, Value};

/// MySQL dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct MySqlDialect;

const MYSQL_KEYWORDS: &[&str] = &[
    "AUTO_INCREMENT",
    "BIGINT",
    "BINARY",
    "BLOB",
    "BOOLEAN",
    "CALL",
    "CHANGE",
    "CHARSET",
    "COLLATE",
    "DATABASE",
    "DATABASES",
    "DATE",
    "DATETIME",
    "DECIMAL",
    "DELIMITER",
    "DESCRIBE",
    "DO",
    "DUPLICATE",
    "ENGINE",
    "ENUM",
    "EXPLAIN",
    "FORCE",
    "FOR",
    "HANDLER",
    "IF",
    "IGNORE",
    "INT",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "KILL",
    "LIMIT",
    "LOCK",
    "LONGTEXT",
    "MODIFY",
    "OFFSET",
    "OPTIMIZE",
    "PROCESSLIST",
    "RECURSIVE",
    "REGEXP",
    "RENAME",
    "REPLACE",
    "RETURNS",
    "SCHEMA",
    "SHOW",
    "SIGNED",
    "STATUS",
    "STRAIGHT_JOIN",
    "TEMPORARY",
    "TEXT",
    "TIMESTAMP",
    "TINYINT",
    "TRIGGER",
    "UNLOCK",
    "UNSIGNED",
    "USE",
    "USING",
    "VARCHAR",
    "VARIABLES",
    "WARNINGS",
    "WINDOW",
    "ZEROFILL",
];

const MYSQL_FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "cast",
    "char_length",
    "coalesce",
    "concat",
    "concat_ws",
    "convert",
    "count",
    "curdate",
    "current_timestamp",
    "date_add",
    "date_format",
    "date_sub",
    "datediff",
    "dense_rank",
    "found_rows",
    "from_unixtime",
    "greatest",
    "group_concat",
    "ifnull",
    "instr",
    "json_arrayagg",
    "json_extract",
    "json_object",
    "json_objectagg",
    "json_unquote",
    "lag",
    "last_insert_id",
    "lead",
    "least",
    "length",
    "lower",
    "max",
    "min",
    "now",
    "nullif",
    "rank",
    "regexp_replace",
    "replace",
    "round",
    "row_number",
    "sleep",
    "str_to_date",
    "substring",
    "substring_index",
    "sum",
    "timestampdiff",
    "trim",
    "unix_timestamp",
    "upper",
    "uuid",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS
        .iter()
        .chain(MYSQL_KEYWORDS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A string literal with MySQL's default escaping: `'` doubled and `\` escaped (the
/// server reads backslash escapes unless `NO_BACKSLASH_ESCAPES` is set, and `\\` means
/// a backslash either way only without it, so doubling is the portable choice).
pub(crate) fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The `DELIMITER <token>` client command on the line starting at `at`, if any: the
/// new delimiter and the end of the line.
fn delimiter_command(sql: &str, at: usize) -> Option<(String, usize)> {
    let eol = sql[at..].find('\n').map_or(sql.len(), |p| at + p);
    let line = sql[at..eol].trim();
    let (word, rest) = line.split_at(line.find(char::is_whitespace)?);
    if !word.eq_ignore_ascii_case("DELIMITER") {
        return None;
    }
    let token = rest.split_whitespace().next()?;
    Some((token.to_owned(), eol))
}

/// Split on the current delimiter (`;` until a `DELIMITER` line changes it), the way the
/// `mysql` client and MySQL Workbench do. `DELIMITER` lines are not sent to the server.
fn split_with_delimiters(sql: &str) -> Vec<StatementSpan> {
    let segs = lexer::segments(sql, Flavor::MySql);
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut delim = ";".to_owned();
    let mut start = 0usize;
    let mut has_code = false;
    // Bytes up to here were consumed by a `DELIMITER` line.
    let mut skip = 0usize;
    // Nothing but whitespace so far on the current line.
    let mut line_clean = true;

    let finish = |out: &mut Vec<StatementSpan>, start: usize, end: usize, has_code: bool| {
        if !has_code || end <= start {
            return;
        }
        let text = &sql[start..end];
        let s = start + (text.len() - text.trim_start().len());
        let e = end - (text.len() - text.trim_end().len());
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
        if seg.end <= skip {
            continue;
        }
        match seg.kind {
            SegKind::Code => {
                let mut j = seg.start.max(skip);
                while j < seg.end {
                    if b[j] == b'\n' {
                        line_clean = true;
                        j += 1;
                        continue;
                    }
                    if !has_code
                        && line_clean
                        && !b[j].is_ascii_whitespace()
                        && let Some((token, eol)) = delimiter_command(sql, j)
                    {
                        delim = token;
                        start = eol;
                        skip = eol;
                        j = eol;
                        continue;
                    }
                    if sql[j..seg.end].starts_with(delim.as_str()) {
                        finish(&mut out, start, j, has_code);
                        j += delim.len();
                        start = j;
                        has_code = false;
                        line_clean = false;
                        continue;
                    }
                    if !b[j].is_ascii_whitespace() {
                        has_code = true;
                        line_clean = false;
                    }
                    j += 1;
                }
            }
            SegKind::Str | SegKind::Ident | SegKind::Comment => {
                line_clean = false;
                if seg.kind != SegKind::Comment {
                    has_code = true;
                }
            }
        }
    }
    finish(&mut out, start, sql.len(), has_code);
    out
}

impl Dialect for MySqlDialect {
    fn engine(&self) -> Engine {
        Engine::MySql
    }

    fn flavor(&self) -> Flavor {
        Flavor::MySql
    }

    fn quote_ident(&self, ident: &str) -> String {
        // MySQL keeps the case of names as written, so upper case needs no quotes.
        if is_plain_ident(ident, &KEYWORDS, true) {
            ident.to_owned()
        } else {
            format!("`{}`", ident.replace('`', "``"))
        }
    }

    fn qualified(&self, schema: &str, name: &str) -> String {
        if schema.is_empty() {
            self.quote_ident(name)
        } else {
            format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        split_with_delimiters(sql)
    }

    fn select_rows(&self, qualified: &str, limit: u64) -> String {
        format!("SELECT * FROM {qualified} LIMIT {limit}")
    }

    fn select_template(&self, qualified: &str, cols: &[String], limit: u64) -> String {
        format!(
            "SELECT{}\nFROM {qualified}\nLIMIT {limit};",
            super::select_list(self, cols)
        )
    }

    /// MySQL drops tables, views and routines; MariaDB also sequences.
    fn script_drop(&self, kind: ObjectKind, qualified: &str) -> String {
        match kind {
            ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Sequence => {
                format!("DROP {} {qualified};", super::drop_keyword(kind))
            }
            other => format!("-- MySQL has no DROP {}", super::drop_keyword(other)),
        }
    }

    /// Routine bodies hold `;`, so they run between `DELIMITER $$` lines.
    fn script_create(&self, kind: ObjectKind, ddl: &str) -> String {
        match kind {
            ObjectKind::Function | ObjectKind::Procedure => {
                let body = ddl.trim().trim_end_matches(';').trim_end();
                format!("DELIMITER $$\n{body}\n$$\nDELIMITER ;")
            }
            _ => super::terminated(ddl),
        }
    }

    /// `?` placeholders (numbered `?1`, `?2` … by position) and `:name`.
    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let mut out = Vec::new();
        let b = sql.as_bytes();
        let mut anon = 0usize;
        for seg in lexer::segments(sql, Flavor::MySql) {
            if seg.kind != SegKind::Code {
                continue;
            }
            let mut i = seg.start;
            while i < seg.end {
                let c = b[i];
                if c == b'?' {
                    anon += 1;
                    out.push(ParamRef {
                        name: format!("?{anon}"),
                        style: ParamStyle::Dollar,
                        start: i,
                        end: i + 1,
                    });
                    i += 1;
                    continue;
                }
                if c == b':'
                    && i + 1 < seg.end
                    && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
                    && (i == 0 || !(is_word(b[i - 1]) || b[i - 1] == b':'))
                {
                    let mut j = i + 1;
                    while j < seg.end && is_word(b[j]) {
                        j += 1;
                    }
                    out.push(ParamRef {
                        name: sql[i..j].to_owned(),
                        style: ParamStyle::Colon,
                        start: i,
                        end: j,
                    });
                    i = j;
                    continue;
                }
                i += 1;
            }
        }
        out
    }

    /// MySQL binds by position only: every placeholder becomes `?`, and the names come
    /// back once per occurrence (a `:name` used twice is bound twice).
    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let params = self.find_params(sql);
        let mut out = String::with_capacity(sql.len());
        let mut names = Vec::with_capacity(params.len());
        let mut last = 0;
        for p in params {
            out.push_str(&sql[last..p.start]);
            out.push('?');
            last = p.end;
            names.push(p.name);
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::MySqlDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        let mut s = String::new();
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Other(s) => quote_string(s),
            Value::Json(s) => format!("CAST({} AS JSON)", quote_string(s)),
            Value::Bytes(b) => {
                value::write_hex(&mut s, b);
                // `write_hex` renders `\x..`; MySQL wants X'..'.
                format!("X'{}'", s.trim_start_matches("\\x"))
            }
            Value::Uuid(_) => quote_string(&v.to_display()),
            Value::Date(d) => {
                value::write_date(&mut s, *d);
                format!("DATE '{s}'")
            }
            Value::Time(t) => {
                value::write_time(&mut s, *t);
                format!("TIME '{s}'")
            }
            // MySQL has no zoned type; UTC values are written as plain timestamps.
            Value::Timestamp(t) | Value::TimestampTz(t) => {
                value::write_timestamp(&mut s, *t, false);
                format!("TIMESTAMP '{s}'")
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        MYSQL_FUNCTIONS
    }

    /// No fixed default: unqualified names resolve in the connection's database.
    fn default_schema(&self) -> &'static str {
        ""
    }

    fn object_folders(&self) -> &'static [ObjectKind] {
        &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Function,
            ObjectKind::Procedure,
        ]
    }

    fn server_folders(&self) -> &'static [ObjectKind] {
        &[ObjectKind::Role]
    }

    fn use_schema(&self, schema: &str) -> Option<String> {
        Some(format!("USE `{}`", schema.replace('`', "``")))
    }

    fn insert_row(&self, qualified: &str, values: &[(String, Option<String>)]) -> String {
        if values.is_empty() {
            // MySQL has no `DEFAULT VALUES`.
            return format!("INSERT INTO {qualified} () VALUES ();");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<&str> {
        MySqlDialect
            .split_script(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn split_table() {
        let cases: &[(&str, &[&str])] = &[
            ("select 1; select 2;", &["select 1", "select 2"]),
            (
                "select ';', \"a;b\", `c;d` from t; x",
                &["select ';', \"a;b\", `c;d` from t", "x"],
            ),
            (
                "select 'it\\'s;'; # c;\nselect 2",
                &["select 'it\\'s;'", "# c;\nselect 2"],
            ),
            ("-- only a comment;\n", &[]),
            (
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nCALL p();",
                &[
                    "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                    "CALL p()",
                ],
            ),
            (
                "delimiter //\nselect 1//\n  select ';'//\ndelimiter ;\nselect 3;",
                &["select 1", "select ';'", "select 3"],
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(&split(sql), want, "script: {sql}");
        }
        let sql = "select 1;\n\nDELIMITER $$\nselect 2$$";
        let spans = MySqlDialect.split_script(sql);
        assert_eq!(spans[1].line, 4);
    }

    #[test]
    fn params_become_positional() {
        let d = MySqlDialect;
        let names: Vec<_> = d
            .find_params("select ?, '?', :a, x::y, `:b` where c = ?")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["?1", ":a", "?2"]);
        let (sql, names) = d.bind_params("select :a, ? where x = :a");
        assert_eq!(sql, "select ?, ? where x = ?");
        assert_eq!(names, [":a", "?1", ":a"]);
    }

    #[test]
    fn quoting_and_literals() {
        let d = MySqlDialect;
        assert_eq!(d.quote_ident("Orders"), "Orders");
        assert_eq!(d.quote_ident("order"), "`order`");
        assert_eq!(d.quote_ident("a`b"), "`a``b`");
        assert_eq!(d.qualified("shop", "users"), "shop.users");
        assert_eq!(d.qualified("", "users"), "users");
        assert_eq!(
            d.literal(&Value::Text("it's \\ ok".into())),
            "'it''s \\\\ ok'"
        );
        assert_eq!(d.literal(&Value::Bytes(vec![0xde, 0xad])), "X'dead'");
        assert_eq!(d.literal(&Value::Bool(true)), "TRUE");
        assert_eq!(
            d.literal(&Value::TimestampTz(1_759_651_200_000_000)),
            "TIMESTAMP '2025-10-05 08:00:00'"
        );
        assert_eq!(d.use_schema("my`db").as_deref(), Some("USE `my``db`"));
    }
}
