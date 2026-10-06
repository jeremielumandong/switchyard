//! T-SQL (SQL Server) dialect: `GO` batches, `TOP`, bracket quoting, `@name` parameters.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, is_plain_ident, line_of_byte,
    quote_string, temporal_text,
};
use crate::value::{self, Engine, Value};

/// SQL Server dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct TSqlDialect;

const TSQL_KEYWORDS: &[&str] = &[
    "APPLY",
    "BIGINT",
    "BIT",
    "CATCH",
    "DATETIME2",
    "DECLARE",
    "EXEC",
    "EXECUTE",
    "FETCH",
    "GO",
    "IDENTITY",
    "IF",
    "MERGE",
    "NOCOUNT",
    "NVARCHAR",
    "OFFSET",
    "OUTPUT",
    "PRINT",
    "RAISERROR",
    "RETURN",
    "ROWS",
    "TOP",
    "TRAN",
    "TRANSACTION",
    "TRY",
    "UNIQUEIDENTIFIER",
    "USE",
    "VARCHAR",
    "WAITFOR",
    "WHILE",
    "NEXT",
    "ONLY",
    "PIVOT",
    "UNPIVOT",
    "THROW",
    "INT",
    "DATE",
    "TIME",
];

const TSQL_FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "cast",
    "ceiling",
    "charindex",
    "coalesce",
    "concat",
    "convert",
    "count",
    "dateadd",
    "datediff",
    "datename",
    "datepart",
    "format",
    "getdate",
    "getutcdate",
    "isnull",
    "json_value",
    "left",
    "len",
    "lower",
    "ltrim",
    "max",
    "min",
    "newid",
    "nullif",
    "replace",
    "right",
    "round",
    "row_number",
    "rtrim",
    "string_agg",
    "substring",
    "sum",
    "sysdatetime",
    "trim",
    "upper",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS
        .iter()
        .chain(TSQL_KEYWORDS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

/// Parse a line as a `GO [count]` separator.
fn go_separator(line: &str) -> Option<u32> {
    let t = line.trim();
    let rest = t
        .get(..2)
        .filter(|g| g.eq_ignore_ascii_case("go"))
        .map(|_| &t[2..])?;
    let rest = rest.trim();
    // Allow a trailing line comment: `GO -- next`.
    let rest = rest.split("--").next().unwrap_or("").trim();
    if rest.is_empty() {
        Some(1)
    } else {
        rest.parse::<u32>().ok().filter(|n| *n > 0)
    }
}

impl Dialect for TSqlDialect {
    fn engine(&self) -> Engine {
        Engine::SqlServer
    }

    fn flavor(&self) -> Flavor {
        Flavor::TSql
    }

    fn quote_ident(&self, ident: &str) -> String {
        if is_plain_ident(ident, &KEYWORDS, true) {
            ident.to_owned()
        } else {
            format!("[{}]", ident.replace(']', "]]"))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        let segs = lexer::segments(sql, Flavor::TSql);
        let mut out = Vec::new();
        let mut batch_start = 0usize;
        let mut line_start = 0usize;
        let push = |out: &mut Vec<StatementSpan>, start: usize, end: usize, repeat: u32| {
            let text = &sql[start..end];
            let s = start + (text.len() - text.trim_start().len());
            let e = end - (text.len() - text.trim_end().len());
            let has_code = segs.iter().any(|seg| {
                seg.start < e
                    && seg.end > s
                    && match seg.kind {
                        SegKind::Code => !sql[seg.start.max(s)..seg.end.min(e)].trim().is_empty(),
                        SegKind::Str | SegKind::Ident => true,
                        SegKind::Comment => false,
                    }
            });
            if e > s && has_code {
                out.push(StatementSpan {
                    start: s,
                    end: e,
                    line: line_of_byte(sql, s),
                    repeat,
                });
            }
        };
        for line in sql.split_inclusive('\n') {
            let line_end = line_start + line.len();
            if lexer::in_code(&segs, line_start + (line.len() - line.trim_start().len()))
                && let Some(repeat) = go_separator(line)
            {
                push(&mut out, batch_start, line_start, repeat);
                batch_start = line_end;
            }
            line_start = line_end;
        }
        push(&mut out, batch_start, sql.len(), 1);
        out
    }

    fn select_rows(&self, qualified: &str, limit: u64) -> String {
        format!("SELECT TOP ({limit}) * FROM {qualified}")
    }

    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let segs = lexer::segments(sql, Flavor::TSql);
        let b = sql.as_bytes();
        let mut found = Vec::new();
        let mut declared: Vec<String> = Vec::new();
        for seg in segs.iter().filter(|s| s.kind == SegKind::Code) {
            let mut i = seg.start;
            while i < seg.end {
                if b[i] == b'@' && i + 1 < seg.end && b[i + 1] == b'@' {
                    i += 2;
                    while i < seg.end && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    continue;
                }
                if b[i] == b'@'
                    && i + 1 < seg.end
                    && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
                {
                    let mut j = i + 1;
                    while j < seg.end && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    let before = sql[seg.start..i].trim_end();
                    let name = sql[i..j].to_owned();
                    let upper = before.to_ascii_uppercase();
                    let in_declare_list = upper.ends_with(',')
                        && upper.rfind("DECLARE").is_some_and(|d| {
                            upper.rfind(';').is_none_or(|s| d > s)
                                && upper.rfind("SELECT").is_none_or(|s| d > s)
                        });
                    if upper.ends_with("DECLARE") || in_declare_list {
                        declared.push(name.to_ascii_lowercase());
                    } else {
                        found.push(ParamRef {
                            name,
                            style: ParamStyle::At,
                            start: i,
                            end: j,
                        });
                    }
                    i = j;
                    continue;
                }
                i += 1;
            }
        }
        found.retain(|p| !declared.contains(&p.name.to_ascii_lowercase()));
        found
    }

    /// Parameters become `@P1`, `@P2`, … (positional, as the driver binds them); a name
    /// used twice maps to the same position. Declared variables are left alone.
    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let mut names: Vec<String> = Vec::new();
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for p in self.find_params(sql) {
            let ix = match names.iter().position(|n| n.eq_ignore_ascii_case(&p.name)) {
                Some(ix) => ix,
                None => {
                    names.push(p.name.clone());
                    names.len() - 1
                }
            };
            out.push_str(&sql[last..p.start]);
            out.push_str(&format!("@P{}", ix + 1));
            last = p.end;
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::MsSqlDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Json(s) | Value::Other(s) => format!("N{}", quote_string(s)),
            Value::Bytes(b) => {
                let mut s = String::new();
                value::write_hex(&mut s, b);
                format!("0x{}", &s[2..])
            }
            Value::Uuid(_) => format!("'{}'", v.to_display()),
            Value::Date(_) | Value::Time(_) | Value::Timestamp(_) => {
                format!("'{}'", temporal_text(v).unwrap_or_default())
            }
            Value::TimestampTz(_) => {
                let t = temporal_text(v).unwrap_or_default();
                format!("'{}:00'", t)
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        TSQL_FUNCTIONS
    }

    fn object_folders(&self) -> &'static [crate::catalog::ObjectKind] {
        use crate::catalog::ObjectKind as K;
        &[
            K::Table,
            K::View,
            K::Procedure,
            K::Function,
            K::Sequence,
            K::Synonym,
            K::Type,
        ]
    }

    fn default_schema(&self) -> &'static str {
        "dbo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<(&str, u32)> {
        TSqlDialect
            .split_script(sql)
            .iter()
            .map(|s| (s.text(sql), s.repeat))
            .collect()
    }

    #[test]
    fn go_batches() {
        let cases: &[(&str, &[(&str, u32)])] = &[
            (
                "select 1\nGO\nselect 2",
                &[("select 1", 1), ("select 2", 1)],
            ),
            ("select 1; select 2", &[("select 1; select 2", 1)]),
            (
                "select 1\n  go  \nselect 2\nGO 3\n",
                &[("select 1", 1), ("select 2", 3)],
            ),
            ("select 'GO\nGO'\nGO", &[("select 'GO\nGO'", 1)]),
            ("/*\nGO\n*/ select 1", &[("/*\nGO\n*/ select 1", 1)]),
            ("select gone\nGOTO x", &[("select gone\nGOTO x", 1)]),
            ("GO\nGO\n", &[]),
            (
                "set showplan_xml on\ngo -- separate\nselect 1",
                &[("set showplan_xml on", 1), ("select 1", 1)],
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(&split(sql), want, "script: {sql:?}");
        }
    }

    #[test]
    fn params_skip_declared_and_globals() {
        let d = TSqlDialect;
        let p = d.find_params("declare @x int = @limit; select @@rowcount, @x, @name, '@no'");
        let names: Vec<_> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["@limit", "@name"]);
        let (sql, names) =
            d.bind_params("declare @x int = @limit; select @x, @Name, @limit, @@rowcount");
        assert_eq!(sql, "declare @x int = @P1; select @x, @P2, @P1, @@rowcount");
        assert_eq!(names, ["@limit", "@Name"]);
    }

    #[test]
    fn quoting_and_top() {
        let d = TSqlDialect;
        assert_eq!(d.quote_ident("Orders"), "Orders");
        assert_eq!(d.quote_ident("order"), "[order]");
        assert_eq!(d.quote_ident("a]b"), "[a]]b]");
        assert_eq!(
            d.select_rows("dbo.Orders", 100),
            "SELECT TOP (100) * FROM dbo.Orders"
        );
        assert_eq!(d.literal(&Value::Text("x".into())), "N'x'");
        assert_eq!(d.literal(&Value::Bytes(vec![1, 255])), "0x01ff");
    }
}
