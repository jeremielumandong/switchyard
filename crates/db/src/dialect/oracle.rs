//! Oracle dialect: SQL statements end at `;`, PL/SQL units (`BEGIN`, `DECLARE`,
//! `CREATE PROCEDURE | FUNCTION | PACKAGE | TRIGGER | TYPE`) run to a `/` line.

use super::lexer::{self, Flavor, SegKind};
use super::{
    COMMON_KEYWORDS, Dialect, ParamRef, ParamStyle, StatementSpan, line_of_byte,
    split_on_semicolons, temporal_text,
};
use crate::catalog::ObjectKind;
use crate::value::{self, Engine, Value};

/// Oracle dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct OracleDialect;

const ORACLE_KEYWORDS: &[&str] = &[
    "CONNECT",
    "DECLARE",
    "DUAL",
    "EXCEPTION",
    "FETCH",
    "FIRST",
    "LEVEL",
    "LOOP",
    "MERGE",
    "MINUS",
    "NEXT",
    "NOCOPY",
    "NUMBER",
    "OFFSET",
    "PACKAGE",
    "PRAGMA",
    "PRIOR",
    "RAISE",
    "ROWID",
    "ROWNUM",
    "ROWS",
    "START",
    "SYNONYM",
    "SYSDATE",
    "TRIGGER",
    "TYPE",
    "VARCHAR2",
];

const ORACLE_FUNCTIONS: &[&str] = &[
    "add_months",
    "avg",
    "coalesce",
    "count",
    "decode",
    "extract",
    "greatest",
    "initcap",
    "instr",
    "json_value",
    "last_day",
    "least",
    "length",
    "listagg",
    "lower",
    "lpad",
    "max",
    "min",
    "months_between",
    "nvl",
    "nvl2",
    "regexp_like",
    "regexp_replace",
    "regexp_substr",
    "replace",
    "round",
    "rpad",
    "substr",
    "sum",
    "sys_guid",
    "systimestamp",
    "to_char",
    "to_date",
    "to_number",
    "to_timestamp",
    "trim",
    "trunc",
    "upper",
];

static KEYWORDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    let mut v: Vec<&str> = COMMON_KEYWORDS
        .iter()
        .chain(ORACLE_KEYWORDS)
        .copied()
        .collect();
    v.sort_unstable();
    v.dedup();
    v
});

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#'
}

/// Unquoted names fold to upper case, so only upper-case names round-trip unquoted.
fn is_plain(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase()
        && chars
            .all(|c| matches!(c, '_' | '$' | '#') || c.is_ascii_digit() || c.is_ascii_uppercase())
        && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(ident))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The first words of `text`, upper-cased, skipping comments and whitespace.
fn leading_words(text: &str, n: usize) -> Vec<String> {
    let mut code = String::new();
    for seg in lexer::segments(text, Flavor::Oracle) {
        match seg.kind {
            SegKind::Comment => code.push(' '),
            _ => code.push_str(&text[seg.start..seg.end]),
        }
        if code.split_whitespace().count() > n {
            break;
        }
    }
    code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Whether a unit starting with `text` is PL/SQL (runs to `/`, keeps its `;`).
pub fn is_plsql(text: &str) -> bool {
    let w = leading_words(text, 6);
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["BEGIN" | "DECLARE", ..] => true,
        ["CREATE", rest @ ..] => {
            let mut rest = rest;
            if let ["OR", "REPLACE", tail @ ..] = rest {
                rest = tail;
            }
            if let ["EDITIONABLE" | "NONEDITIONABLE", tail @ ..] = rest {
                rest = tail;
            }
            matches!(
                rest.first(),
                Some(&("PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"))
            )
        }
        _ => false,
    }
}

/// Byte ranges of lines holding only `/`, outside strings and comments.
fn slash_lines(sql: &str) -> Vec<(usize, usize)> {
    let segs = lexer::segments(sql, Flavor::Oracle);
    let mut out = Vec::new();
    let mut start = 0;
    for line in sql.split_inclusive('\n') {
        let end = start + line.len();
        if line.trim() == "/" {
            let slash = start + line.find('/').unwrap_or(0);
            if lexer::in_code(&segs, slash) {
                out.push((start, end));
            }
        }
        start = end;
    }
    out
}

fn trimmed_span(sql: &str, start: usize, end: usize) -> Option<StatementSpan> {
    let text = &sql[start..end];
    let s = start + (text.len() - text.trim_start().len());
    let e = end - (text.len() - text.trim_end().len());
    (e > s).then(|| StatementSpan {
        start: s,
        end: e,
        line: line_of_byte(sql, s),
        repeat: 1,
    })
}

/// Split one `/`-delimited chunk: SQL statements on `;` until a PL/SQL unit starts, which
/// takes the rest of the chunk.
fn split_chunk(sql: &str, from: usize, to: usize, out: &mut Vec<StatementSpan>) {
    let chunk = &sql[from..to];
    for span in split_on_semicolons(chunk, Flavor::Oracle) {
        let start = from + span.start;
        if is_plsql(&sql[start..to]) {
            if let Some(s) = trimmed_span(sql, start, to) {
                out.push(s);
            }
            return;
        }
        out.push(StatementSpan {
            start,
            end: from + span.end,
            line: line_of_byte(sql, start),
            repeat: 1,
        });
    }
}

impl Dialect for OracleDialect {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }

    fn flavor(&self) -> Flavor {
        Flavor::Oracle
    }

    fn quote_ident(&self, ident: &str) -> String {
        if is_plain(ident) {
            ident.to_owned()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        let mut out = Vec::new();
        let mut from = 0;
        for (start, end) in slash_lines(sql) {
            split_chunk(sql, from, start, &mut out);
            from = end;
        }
        split_chunk(sql, from, sql.len(), &mut out);
        out
    }

    fn select_rows(&self, qualified: &str, limit: u64) -> String {
        format!("SELECT * FROM {qualified} FETCH FIRST {limit} ROWS ONLY")
    }

    fn select_template(&self, qualified: &str, cols: &[String], limit: u64) -> String {
        format!(
            "SELECT{}\nFROM {qualified}\nFETCH FIRST {limit} ROWS ONLY;",
            super::select_list(self, cols)
        )
    }

    /// PL/SQL units (`GET_DDL` of a procedure, function, type) run to a `/` line;
    /// everything else ends at `;`.
    fn script_create(&self, _kind: ObjectKind, ddl: &str) -> String {
        let ddl = ddl.trim();
        if is_plsql(ddl) {
            format!("{ddl}\n/")
        } else {
            super::terminated(ddl)
        }
    }

    /// Procedures run in an anonymous block, functions from `DUAL`; named notation.
    fn script_exec(&self, kind: ObjectKind, qualified: &str, params: &[String]) -> String {
        let args = params
            .iter()
            .map(|p| {
                if p.is_empty() {
                    "NULL".to_owned()
                } else {
                    format!("{} => NULL", self.quote_ident(p))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let call = if args.is_empty() {
            qualified.to_owned()
        } else {
            format!("{qualified}({args})")
        };
        match kind {
            ObjectKind::Procedure => format!("BEGIN\n  {call};\nEND;\n/"),
            _ => format!("SELECT {call} FROM DUAL;"),
        }
    }

    /// `:name` and `:1` placeholders; `:=` assignments and trigger `:NEW.x` / `:OLD.x`
    /// references are not parameters.
    fn find_params(&self, sql: &str) -> Vec<ParamRef> {
        let mut out = Vec::new();
        let b = sql.as_bytes();
        for seg in lexer::segments(sql, Flavor::Oracle) {
            if seg.kind != SegKind::Code {
                continue;
            }
            let mut i = seg.start;
            while i < seg.end {
                if b[i] != b':' || i + 1 >= seg.end || (i > 0 && is_word(b[i - 1])) {
                    i += 1;
                    continue;
                }
                let next = b[i + 1];
                if !(next.is_ascii_alphanumeric() || next == b'_') {
                    i += 1;
                    continue;
                }
                let mut j = i + 1;
                while j < seg.end && is_word(b[j]) {
                    j += 1;
                }
                if j < seg.end && b[j] == b'.' {
                    // :NEW.col / :OLD.col inside a trigger
                    i = j;
                    continue;
                }
                out.push(ParamRef {
                    name: sql[i..j].to_owned(),
                    style: if next.is_ascii_digit() {
                        ParamStyle::Dollar
                    } else {
                        ParamStyle::Colon
                    },
                    start: i,
                    end: j,
                });
                i = j;
            }
        }
        out
    }

    /// Placeholders become `:p1`, `:p2`, bound by name, so a repeated name takes one
    /// value.
    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        let params = self.find_params(sql);
        let mut names: Vec<String> = Vec::new();
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for p in &params {
            let key = p.name.to_ascii_uppercase();
            let ix = match names.iter().position(|n| n.eq_ignore_ascii_case(&key)) {
                Some(ix) => ix,
                None => {
                    names.push(p.name.clone());
                    names.len() - 1
                }
            };
            out.push_str(&sql[last..p.start]);
            out.push_str(&format!(":p{}", ix + 1));
            last = p.end;
        }
        out.push_str(&sql[last..]);
        (out, names)
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        Box::new(sqlparser::dialect::OracleDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Numeric(s) => s.clone(),
            Value::Text(s) | Value::Other(s) | Value::Json(s) => quote(s),
            Value::Bytes(b) => {
                let mut s = String::new();
                value::write_hex(&mut s, b);
                format!("HEXTORAW('{}')", s.trim_start_matches("\\x"))
            }
            Value::Uuid(_) => quote(&v.to_display()),
            Value::Date(_) => format!("DATE {}", quote(&temporal_text(v).unwrap_or_default())),
            Value::Timestamp(_) | Value::TimestampTz(_) => {
                format!("TIMESTAMP {}", quote(&temporal_text(v).unwrap_or_default()))
            }
            Value::Time(_) => quote(&temporal_text(v).unwrap_or_default()),
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &KEYWORDS
    }

    fn functions(&self) -> &'static [&'static str] {
        ORACLE_FUNCTIONS
    }

    fn default_schema(&self) -> &'static str {
        ""
    }

    fn object_folders(&self) -> &'static [ObjectKind] {
        &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Package,
            ObjectKind::Sequence,
            ObjectKind::Synonym,
        ]
    }

    fn server_folders(&self) -> &'static [ObjectKind] {
        &[ObjectKind::Role]
    }

    fn use_schema(&self, schema: &str) -> Option<String> {
        Some(format!(
            "ALTER SESSION SET CURRENT_SCHEMA = {}",
            self.quote_ident(schema)
        ))
    }

    /// Oracle 12c+ `OFFSET … FETCH` (no `ORDER BY` needed).
    fn select_page(
        &self,
        qualified: &str,
        cols: &[String],
        where_: Option<&str>,
        order: &[super::SortKey],
        limit: u64,
        offset: u64,
    ) -> String {
        format!(
            "{}\nOFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY",
            super::page_head(self, qualified, cols, where_, order)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<&str> {
        OracleDialect
            .split_script(sql)
            .iter()
            .map(|s| s.text(sql))
            .collect()
    }

    #[test]
    fn sql_ends_at_semicolon_plsql_at_slash() {
        let sql = "select 1 from dual;\nselect q'[a;b]' from dual;\n\
                   create or replace procedure p as\nbegin\n  null;\nend;\n/\n\
                   begin\n  p;\nend;\n/\nselect 2 from dual";
        assert_eq!(
            split(sql),
            [
                "select 1 from dual",
                "select q'[a;b]' from dual",
                "create or replace procedure p as\nbegin\n  null;\nend;",
                "begin\n  p;\nend;",
                "select 2 from dual",
            ]
        );
    }

    #[test]
    fn plsql_without_slash_runs_to_the_end() {
        assert_eq!(
            split("select 1 from dual; declare x number; begin x := 1; end;"),
            ["select 1 from dual", "declare x number; begin x := 1; end;"]
        );
    }

    #[test]
    fn slash_inside_a_string_or_expression_is_not_a_separator() {
        assert_eq!(
            split("select 4\n/\n2 from dual"),
            ["select 4", "2 from dual"],
            "a lone / line always separates (SQL*Plus does the same)"
        );
        assert_eq!(
            split("select '\n/\n' from dual"),
            ["select '\n/\n' from dual"]
        );
    }

    #[test]
    fn params_skip_assignments_and_trigger_rows() {
        let d = OracleDialect;
        let names: Vec<_> = d
            .find_params("begin :new.x := :val; select :1 into v from t where a = :Val; end;")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, [":val", ":1", ":Val"]);
        let (sql, names) = d.bind_params("select :a, :b, :A from dual");
        assert_eq!(sql, "select :p1, :p2, :p1 from dual");
        assert_eq!(names, [":a", ":b"]);
    }

    #[test]
    fn quoting_and_literals() {
        let d = OracleDialect;
        assert_eq!(d.quote_ident("EMP"), "EMP");
        assert_eq!(d.quote_ident("emp"), "\"emp\"");
        assert_eq!(
            d.select_rows("HR.EMP", 10),
            "SELECT * FROM HR.EMP FETCH FIRST 10 ROWS ONLY"
        );
        assert_eq!(d.literal(&Value::Bytes(vec![0xab])), "HEXTORAW('ab')");
        assert_eq!(d.literal(&Value::Date(0)), "DATE '1970-01-01'");
    }
}
