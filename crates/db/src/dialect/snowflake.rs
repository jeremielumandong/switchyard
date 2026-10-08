//! Snowflake dialect.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, split_on_semicolons,
    temporal_text,
};
use crate::catalog::ObjectKind;
use crate::value::{self, Engine, Value};

/// Snowflake dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct SnowflakeDialect;

const SNOWFLAKE_KEYWORDS: &[&str] = &[
    "ARRAY",
    "BINARY",
    "CLONE",
    "CLUSTER",
    "COPY",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "EXPLAIN",
    "FLATTEN",
    "ILIKE",
    "LATERAL",
    "LIMIT",
    "MERGE",
    "MINUS",
    "OBJECT",
    "OFFSET",
    "PIVOT",
    "QUALIFY",
    "REGEXP",
    "RLIKE",
    "SAMPLE",
    "SCHEMA",
    "SHOW",
    "STAGE",
    "TABLESAMPLE",
    "TOP",
    "UNPIVOT",
    "USE",
    "VARIANT",
    "WAREHOUSE",
];

const SNOWFLAKE_FUNCTIONS: &[&str] = &[
    "array_agg",
    "array_construct",
    "array_size",
    "avg",
    "coalesce",
    "concat",
    "count",
    "current_database",
    "current_role",
    "current_schema",
    "current_warehouse",
    "date_trunc",
    "dateadd",
    "datediff",
    "decode",
    "flatten",
    "get_path",
    "iff",
    "ifnull",
    "listagg",
    "lower",
    "max",
    "min",
    "nullif",
    "nvl",
    "object_construct",
    "parse_json",
    "regexp_substr",
    "round",
    "split_part",
    "sum",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_varchar",
    "trim",
    "try_cast",
    "try_parse_json",
    "upper",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS
        .iter()
        .chain(SNOWFLAKE_KEYWORDS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Unquoted Snowflake identifiers fold to upper case, so only upper-case names round-trip
/// without quotes.
fn is_plain(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_uppercase())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_digit() || c.is_ascii_uppercase())
        && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(ident))
}

/// A string literal: quotes doubled, backslashes escaped (Snowflake strings treat `\` as
/// an escape character).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

impl Dialect for SnowflakeDialect {
    fn engine(&self) -> Engine {
        Engine::Snowflake
    }

    fn flavor(&self) -> Flavor {
        Flavor::Snowflake
    }

    fn quote_ident(&self, ident: &str) -> String {
        if is_plain(ident) {
            ident.to_owned()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        split_on_semicolons(sql, Flavor::Snowflake)
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

    /// `?` placeholders, `:1` positions and `:name` parameters. `::` casts and
    /// `col:path` lookups into semi-structured data are not parameters.
    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let mut out = Vec::new();
        let b = sql.as_bytes();
        let mut anon = 0usize;
        for seg in lexer::segments(sql, Flavor::Snowflake) {
            if seg.kind != SegKind::Code {
                continue;
            }
            let mut i = seg.start;
            while i < seg.end {
                let c = b[i];
                if c == b'?' {
                    anon += 1;
                    out.push(ParamRef {
                        name: format!(":{anon}"),
                        style: ParamStyle::Dollar,
                        start: i,
                        end: i + 1,
                    });
                    i += 1;
                    continue;
                }
                let prev_ok = i == 0 || !(is_word(b[i - 1]) || b[i - 1] == b':');
                if c == b':' && i + 1 < seg.end && b[i + 1] != b':' && prev_ok {
                    let next = b[i + 1];
                    let mut j = i + 1;
                    if next.is_ascii_digit() {
                        while j < seg.end && b[j].is_ascii_digit() {
                            j += 1;
                        }
                        let n: usize = sql[i + 1..j].parse().unwrap_or(0);
                        anon = anon.max(n);
                        out.push(ParamRef {
                            name: sql[i..j].to_owned(),
                            style: ParamStyle::Dollar,
                            start: i,
                            end: j,
                        });
                        i = j;
                        continue;
                    }
                    if next.is_ascii_alphabetic() || next == b'_' {
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
                }
                i += 1;
            }
        }
        out
    }

    /// The SQL API binds by position (`:1`, `:2`), so every placeholder becomes `:N`.
    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let params = self.find_params(sql);
        let max_pos = params
            .iter()
            .filter(|p| p.style == ParamStyle::Dollar)
            .filter_map(|p| p.name[1..].parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        let mut names: Vec<String> = (1..=max_pos).map(|i| format!(":{i}")).collect();
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
            out.push(':');
            out.push_str(&(ix + 1).to_string());
            last = p.end;
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::SnowflakeDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Other(s) => quote(s),
            Value::Json(s) => format!("PARSE_JSON({})", quote(s)),
            Value::Bytes(b) => {
                let mut s = String::new();
                value::write_hex(&mut s, b);
                format!("X'{}'", s.trim_start_matches("\\x"))
            }
            Value::Uuid(_) => quote(&v.to_display()),
            Value::Date(_) | Value::Time(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
                quote(&temporal_text(v).unwrap_or_default())
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        SNOWFLAKE_FUNCTIONS
    }

    fn default_schema(&self) -> &'static str {
        "PUBLIC"
    }

    fn object_folders(&self) -> &'static [ObjectKind] {
        &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<&str> {
        SnowflakeDialect
            .split_script(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn split_respects_dollar_bodies_and_escapes() {
        let sql = "create function f() returns int as $$ 1; $$; select 'a\\';b'; select 2";
        assert_eq!(
            split(sql),
            [
                "create function f() returns int as $$ 1; $$",
                "select 'a\\';b'",
                "select 2"
            ]
        );
    }

    #[test]
    fn params_skip_casts_and_paths() {
        let d = SnowflakeDialect;
        let names: Vec<_> = d
            .find_params("select v:name, x::int, ?, :2, :cust from t where a = ?")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, [":1", ":2", ":cust", ":3"]);
        let (sql, names) = d.bind_params("select :a, ?, :a");
        assert_eq!(sql, "select :2, :1, :2");
        assert_eq!(names, [":1", ":a"]);
    }

    #[test]
    fn quoting_and_literals() {
        let d = SnowflakeDialect;
        assert_eq!(d.quote_ident("ORDERS"), "ORDERS");
        assert_eq!(d.quote_ident("orders"), "\"orders\"");
        assert_eq!(d.quote_ident("SELECT"), "\"SELECT\"");
        assert_eq!(d.literal(&Value::Text("it's \\".into())), "'it''s \\\\'");
        assert_eq!(d.literal(&Value::Bytes(vec![0xab])), "X'ab'");
        assert_eq!(d.literal(&Value::Bool(true)), "TRUE");
    }
}
