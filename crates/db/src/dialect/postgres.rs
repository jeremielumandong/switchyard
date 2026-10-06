//! PostgreSQL dialect.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, is_plain_ident, quote_string,
    split_on_semicolons, temporal_text,
};
use crate::value::{self, Engine, Value};

/// PostgreSQL dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostgresDialect;

const PG_KEYWORDS: &[&str] = &[
    "ANALYZE",
    "ARRAY",
    "BOTH",
    "CASCADE",
    "COALESCE",
    "COPY",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DO",
    "EXCLUDED",
    "EXPLAIN",
    "FETCH",
    "FILTER",
    "FIRST",
    "FOR",
    "ILIKE",
    "INTERVAL",
    "LATERAL",
    "LIMIT",
    "MATERIALIZED",
    "NULLS",
    "OFFSET",
    "ONLY",
    "RECURSIVE",
    "RETURNING",
    "SCHEMA",
    "SEQUENCE",
    "SIMILAR",
    "TEMP",
    "TEMPORARY",
    "TYPE",
    "USING",
    "VACUUM",
    "WINDOW",
    "CONFLICT",
    "NOTHING",
    "LAST",
    "ROWS",
    "RANGE",
    "PRECEDING",
    "FOLLOWING",
    "UNBOUNDED",
    "CURRENT",
    "ROW",
    "EXTENSION",
    "TRIGGER",
    "RETURNS",
    "LANGUAGE",
    "REPLACE",
    "IF",
    "BOOLEAN",
    "INTEGER",
    "BIGINT",
    "TEXT",
    "NUMERIC",
    "VARCHAR",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "DATE",
    "TIME",
    "JSONB",
    "JSON",
    "UUID",
    "BYTEA",
    "SERIAL",
    "BIGSERIAL",
];

const PG_FUNCTIONS: &[&str] = &[
    "abs",
    "array_agg",
    "avg",
    "coalesce",
    "count",
    "date_trunc",
    "extract",
    "format",
    "generate_series",
    "greatest",
    "json_agg",
    "jsonb_build_object",
    "jsonb_agg",
    "least",
    "length",
    "lower",
    "max",
    "min",
    "now",
    "nullif",
    "row_number",
    "rank",
    "dense_rank",
    "lag",
    "lead",
    "round",
    "split_part",
    "starts_with",
    "string_agg",
    "substring",
    "sum",
    "to_char",
    "to_timestamp",
    "trim",
    "upper",
    "pg_sleep",
    "current_setting",
    "gen_random_uuid",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS.iter().chain(PG_KEYWORDS).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
});

impl Dialect for PostgresDialect {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn flavor(&self) -> Flavor {
        Flavor::Postgres
    }

    fn quote_ident(&self, ident: &str) -> String {
        if is_plain_ident(ident, &KEYWORDS, false) {
            ident.to_owned()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        split_on_semicolons(sql, Flavor::Postgres)
    }

    fn select_rows(&self, qualified: &str, limit: u64) -> String {
        format!("SELECT * FROM {qualified} LIMIT {limit}")
    }

    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let mut out = Vec::new();
        for seg in lexer::segments(sql, Flavor::Postgres) {
            if seg.kind != SegKind::Code {
                continue;
            }
            let b = sql.as_bytes();
            let mut i = seg.start;
            while i < seg.end {
                let c = b[i];
                if c == b'$' && i + 1 < seg.end && b[i + 1].is_ascii_digit() {
                    let mut j = i + 1;
                    while j < seg.end && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    out.push(ParamRef {
                        name: sql[i..j].to_owned(),
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
                    && (i == 0 || b[i - 1] != b':')
                    && (i == 0
                        || !(b[i - 1].is_ascii_alphanumeric()
                            || b[i - 1] == b'_'
                            || b[i - 1] == b']'))
                {
                    let mut j = i + 1;
                    while j < seg.end && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
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
                if c == b':' && i + 1 < seg.end && b[i + 1] == b':' {
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
        out
    }

    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let params = self.find_params(sql);
        let max_dollar = params
            .iter()
            .filter(|p| p.style == ParamStyle::Dollar)
            .filter_map(|p| p.name[1..].parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        let mut names: Vec<String> = (1..=max_dollar).map(|i| format!("${i}")).collect();
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for p in &params {
            if p.style != ParamStyle::Colon {
                continue;
            }
            let ix = match names.iter().position(|n| n == &p.name) {
                Some(ix) => ix,
                None => {
                    names.push(p.name.clone());
                    names.len() - 1
                }
            };
            out.push_str(&sql[last..p.start]);
            out.push('$');
            out.push_str(&(ix + 1).to_string());
            last = p.end;
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::PostgreSqlDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "true" } else { "false" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(f) => format!("'{}'::float8", Value::Float(*f).to_display()),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Other(s) => quote_string(s),
            Value::Json(s) => format!("{}::jsonb", quote_string(s)),
            Value::Bytes(b) => {
                let mut s = String::new();
                value::write_hex(&mut s, b);
                format!("'{s}'::bytea")
            }
            Value::Uuid(_) => format!("'{}'::uuid", v.to_display()),
            Value::Date(_) => format!("DATE '{}'", temporal_text(v).unwrap_or_default()),
            Value::Time(_) => format!("TIME '{}'", temporal_text(v).unwrap_or_default()),
            Value::Timestamp(_) => format!("TIMESTAMP '{}'", temporal_text(v).unwrap_or_default()),
            Value::TimestampTz(_) => {
                format!("TIMESTAMPTZ '{}'", temporal_text(v).unwrap_or_default())
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        PG_FUNCTIONS
    }

    fn default_schema(&self) -> &'static str {
        "public"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<&str> {
        PostgresDialect
            .split_script(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn split_table() {
        let cases: &[(&str, &[&str])] = &[
            ("select 1; select 2;", &["select 1", "select 2"]),
            ("select 1", &["select 1"]),
            ("  ;; select 1 ;  ", &["select 1"]),
            (
                "select ';' as a; select 2",
                &["select ';' as a", "select 2"],
            ),
            ("select \"a;b\" from t; x", &["select \"a;b\" from t", "x"]),
            ("-- c; c\nselect 1; /* ; */", &["-- c; c\nselect 1"]),
            (
                "create function f() returns int as $$ select 1; $$ language sql; select f();",
                &[
                    "create function f() returns int as $$ select 1; $$ language sql",
                    "select f()",
                ],
            ),
            (
                "do $body$ begin raise notice 'x;'; end $body$; select 1",
                &["do $body$ begin raise notice 'x;'; end $body$", "select 1"],
            ),
            (
                "select E'it\\'s;' ; select 2",
                &["select E'it\\'s;'", "select 2"],
            ),
            ("select $1, a$b; select 2", &["select $1, a$b", "select 2"]),
            ("/* only a comment */", &[]),
            (
                "select 1 -- trailing; comment",
                &["select 1 -- trailing; comment"],
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(&split(sql), want, "script: {sql}");
        }
    }

    #[test]
    fn statement_at_cursor() {
        let sql = "select 1;\n\nselect 2\nfrom t;\nselect 3;";
        let d = PostgresDialect;
        let at = |off| d.statement_at(sql, off).map(|s| s.text(sql));
        assert_eq!(at(3), Some("select 1"));
        assert_eq!(at(9), Some("select 1"), "right after the semicolon");
        assert_eq!(
            at(10),
            Some("select 2\nfrom t"),
            "blank line picks the next one"
        );
        assert_eq!(at(sql.find("from").unwrap()), Some("select 2\nfrom t"));
        assert_eq!(at(sql.len()), Some("select 3"));
    }

    #[test]
    fn line_numbers() {
        let sql = "select 1;\n\nselect 2;";
        let spans = PostgresDialect.split_script(sql);
        assert_eq!(spans[0].line, 1);
        assert_eq!(spans[1].line, 3);
    }

    #[test]
    fn params() {
        let d = PostgresDialect;
        let p = d.find_params("select :a, $2, x::text, ':b', $1 from t where y = :a");
        let names: Vec<_> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [":a", "$2", "$1", ":a"]);
        let (sql, names) = d.bind_params("select :a, $2 where y = :a or z = :b");
        assert_eq!(sql, "select $3, $2 where y = $3 or z = $4");
        assert_eq!(names, ["$1", "$2", ":a", ":b"]);
    }

    #[test]
    fn quoting_and_literals() {
        let d = PostgresDialect;
        assert_eq!(d.quote_ident("orders"), "orders");
        assert_eq!(d.quote_ident("Orders"), "\"Orders\"");
        assert_eq!(d.quote_ident("select"), "\"select\"");
        assert_eq!(d.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.literal(&Value::Text("it's".into())), "'it''s'");
        assert_eq!(d.literal(&Value::Date(0)), "DATE '1970-01-01'");
        assert_eq!(
            d.select_rows("public.orders", 100),
            "SELECT * FROM public.orders LIMIT 100"
        );
    }
}
