//! SQLite dialect, used by local SQLite files and Cloudflare D1.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, is_plain_ident, quote_string,
    split_on_semicolons, temporal_text,
};
use crate::catalog::ObjectKind;
use crate::value::{self, Engine, Value};

/// SQLite dialect. One value per engine that speaks it, so [`Dialect::engine`] answers
/// for the right one (D1 has no interactive transactions; a local file does).
#[derive(Clone, Copy, Debug)]
pub struct SqliteDialect {
    engine: Engine,
}

impl SqliteDialect {
    /// Cloudflare D1.
    pub const D1: Self = Self { engine: Engine::D1 };
    /// A local SQLite file.
    pub const LOCAL: Self = Self {
        engine: Engine::Sqlite,
    };
}

const SQLITE_KEYWORDS: &[&str] = &[
    "ABORT",
    "AUTOINCREMENT",
    "BLOB",
    "CONFLICT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "EXPLAIN",
    "FAIL",
    "GLOB",
    "IF",
    "IGNORE",
    "INTEGER",
    "LIMIT",
    "NOTHING",
    "OFFSET",
    "PLAN",
    "PRAGMA",
    "QUERY",
    "REAL",
    "RECURSIVE",
    "REGEXP",
    "REPLACE",
    "RETURNING",
    "ROWID",
    "STRICT",
    "TEMP",
    "TEMPORARY",
    "TEXT",
    "TRIGGER",
    "VACUUM",
    "VIRTUAL",
    "WINDOW",
    "WITHOUT",
];

const SQLITE_FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "coalesce",
    "count",
    "date",
    "datetime",
    "group_concat",
    "hex",
    "ifnull",
    "iif",
    "instr",
    "json",
    "json_array",
    "json_extract",
    "json_group_array",
    "json_object",
    "julianday",
    "last_insert_rowid",
    "length",
    "like",
    "lower",
    "ltrim",
    "max",
    "min",
    "nullif",
    "printf",
    "random",
    "replace",
    "round",
    "rtrim",
    "strftime",
    "substr",
    "sum",
    "time",
    "total",
    "trim",
    "typeof",
    "unixepoch",
    "upper",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS
        .iter()
        .chain(SQLITE_KEYWORDS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `CREATE TRIGGER ... BEGIN stmt; stmt; END` holds semicolons; glue its pieces back
/// together up to the piece that ends with `END`.
fn merge_trigger_bodies(sql: &str, spans: Vec<StatementSpan>) -> Vec<StatementSpan> {
    let mut out: Vec<StatementSpan> = Vec::with_capacity(spans.len());
    let mut open = false;
    for span in spans {
        if open && let Some(last) = out.last_mut() {
            last.end = span.end;
        } else {
            out.push(span);
        }
        let Some(cur) = out.last() else { continue };
        let words: Vec<String> = cur
            .text(sql)
            .split_whitespace()
            .take(3)
            .map(str::to_ascii_uppercase)
            .collect();
        let is_trigger = words.first().is_some_and(|w| w == "CREATE")
            && (words.get(1).is_some_and(|w| w == "TRIGGER")
                || (words
                    .get(1)
                    .is_some_and(|w| w == "TEMP" || w == "TEMPORARY")
                    && words.get(2).is_some_and(|w| w == "TRIGGER")));
        let ends = cur
            .text(sql)
            .trim_end()
            .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("END"));
        open = is_trigger && !ends;
    }
    out
}

impl Dialect for SqliteDialect {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn flavor(&self) -> Flavor {
        Flavor::Sqlite
    }

    fn quote_ident(&self, ident: &str) -> String {
        if is_plain_ident(ident, &KEYWORDS, true) {
            ident.to_owned()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn qualified(&self, schema: &str, name: &str) -> String {
        // `main` is the connected database; `main.` is noise in generated SQL.
        if schema.is_empty() || schema == "main" {
            self.quote_ident(name)
        } else {
            format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        merge_trigger_bodies(sql, split_on_semicolons(sql, Flavor::Sqlite))
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

    /// SQLite drops only tables, views, indexes and triggers.
    fn script_drop(&self, kind: ObjectKind, qualified: &str) -> String {
        match kind {
            ObjectKind::Table | ObjectKind::View => {
                format!("DROP {} {qualified};", super::drop_keyword(kind))
            }
            other => format!("-- SQLite has no DROP {}", super::drop_keyword(other)),
        }
    }

    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let mut out = Vec::new();
        let b = sql.as_bytes();
        let mut anon = 0usize;
        for seg in lexer::segments(sql, Flavor::Sqlite) {
            if seg.kind != SegKind::Code {
                continue;
            }
            let mut i = seg.start;
            while i < seg.end {
                let c = b[i];
                if c == b'?' {
                    let mut j = i + 1;
                    while j < seg.end && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    let name = if j > i + 1 {
                        let n: usize = sql[i + 1..j].parse().unwrap_or(0);
                        anon = anon.max(n);
                        sql[i..j].to_owned()
                    } else {
                        anon += 1;
                        format!("?{anon}")
                    };
                    out.push(ParamRef {
                        name,
                        style: ParamStyle::Dollar,
                        start: i,
                        end: j,
                    });
                    i = j;
                    continue;
                }
                if c == b':'
                    && i + 1 < seg.end
                    && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
                    && (i == 0 || !is_word(b[i - 1]))
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

    /// D1 binds positional parameters only, so every placeholder becomes `?N`.
    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let params = self.find_params(sql);
        let max_pos = params
            .iter()
            .filter(|p| p.style == ParamStyle::Dollar)
            .filter_map(|p| p.name[1..].parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        let mut names: Vec<String> = (1..=max_pos).map(|i| format!("?{i}")).collect();
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for p in &params {
            let ix = match names.iter().position(|n| n == &p.name) {
                Some(ix) => ix,
                None => {
                    names.push(p.name.clone());
                    names.len() - 1
                }
            };
            out.push_str(&sql[last..p.start]);
            out.push('?');
            out.push_str(&(ix + 1).to_string());
            last = p.end;
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::SQLiteDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Other(s) | Value::Json(s) => quote_string(s),
            Value::Bytes(b) => {
                let mut s = String::new();
                value::write_hex(&mut s, b);
                // `write_hex` renders `\x..`; SQLite wants X'..'.
                format!("X'{}'", s.trim_start_matches("\\x"))
            }
            Value::Uuid(_) => quote_string(&v.to_display()),
            Value::Date(_) | Value::Time(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
                quote_string(&temporal_text(v).unwrap_or_default())
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        SQLITE_FUNCTIONS
    }

    fn default_schema(&self) -> &'static str {
        "main"
    }

    fn object_folders(&self) -> &'static [ObjectKind] {
        &[ObjectKind::Table, ObjectKind::View]
    }

    /// SQLite keeps no dependency catalog.
    fn supports_dependencies(&self) -> bool {
        false
    }

    fn switches_context(&self) -> bool {
        // The database is the whole connection; attached ones are reached by `schema.`.
        false
    }

    /// SQLite has no `DEFAULT` in `VALUES`: columns left at their default are omitted.
    fn insert_row(&self, qualified: &str, values: &[(String, Option<String>)]) -> String {
        let set: Vec<(String, Option<String>)> = values
            .iter()
            .filter(|(_, v)| v.is_some())
            .cloned()
            .collect();
        if set.is_empty() {
            return format!("INSERT INTO {qualified} DEFAULT VALUES;");
        }
        let names: Vec<String> = set.iter().map(|(c, _)| self.quote_ident(c)).collect();
        let vals: Vec<&str> = set.iter().filter_map(|(_, v)| v.as_deref()).collect();
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
        SqliteDialect::D1
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
                "select ';' as a; select 2",
                &["select ';' as a", "select 2"],
            ),
            (
                "select [a;b], `c;d` from t; x",
                &["select [a;b], `c;d` from t", "x"],
            ),
            ("/* a /* b */ select 1; c", &["/* a /* b */ select 1", "c"]),
            (
                "create trigger t after insert on a begin select 1; end;",
                &["create trigger t after insert on a begin select 1; end"],
            ),
            (
                "CREATE TEMP TRIGGER t BEGIN UPDATE a SET x=1; DELETE FROM b; END; select 2",
                &[
                    "CREATE TEMP TRIGGER t BEGIN UPDATE a SET x=1; DELETE FROM b; END",
                    "select 2",
                ],
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(&split(sql), want, "script: {sql}");
        }
    }

    #[test]
    fn params_are_rewritten_to_positional() {
        let d = SqliteDialect::D1;
        let names: Vec<_> = d
            .find_params("select ?, ?, ':x' where a = :a and b = ?5")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["?1", "?2", ":a", "?5"]);
        let (sql, names) = d.bind_params("select ?, :a, ?3 where x = :a or y = :b");
        assert_eq!(sql, "select ?1, ?4, ?3 where x = ?4 or y = ?5");
        assert_eq!(names, ["?1", "?2", "?3", ":a", ":b"]);
    }

    #[test]
    fn quoting_and_literals() {
        let d = SqliteDialect::D1;
        assert_eq!(d.quote_ident("Orders"), "Orders");
        assert_eq!(d.quote_ident("order"), "\"order\"");
        assert_eq!(d.quote_ident("a b"), "\"a b\"");
        assert_eq!(d.qualified("main", "users"), "users");
        assert_eq!(d.literal(&Value::Bool(true)), "1");
        assert_eq!(d.literal(&Value::Bytes(vec![0xde, 0xad])), "X'dead'");
        assert_eq!(d.literal(&Value::Text("it's".into())), "'it''s'");
    }
}
