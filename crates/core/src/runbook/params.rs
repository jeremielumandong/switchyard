//! `{{param}}` substitution in runbook step text.
//!
//! A placeholder is `{{name}}` or `{{name:modifier}}` (spaces inside the braces are
//! allowed). Anything else in double braces (`{{ 1 + 1 }}`, `{{}}`) is left as it is.
//!
//! Plain text (HTTP, shell, notes) gets the value as is; the `quote` modifier wraps it in
//! single quotes for a POSIX shell. SQL is resolved per dialect ([`render_sql`]): a value
//! in code becomes a string literal from the dialect (`raw` inserts it unquoted, `ident`
//! as a quoted identifier), a value inside a string literal or quoted identifier is
//! escaped for it, and a secret value is never written into the text at all: it is bound
//! as a statement parameter, so the SQL kept in history holds only a placeholder.
//!
//! [`Reveal::Masked`] renders secrets as [`MASK`] for display and history.

use std::collections::HashMap;

use secrecy::{ExposeSecret as _, SecretString};
use switchyard_db::dialect::{Flavor, SegKind, lexer};
use switchyard_db::{Dialect, Engine, Value};
use switchyard_store::Runbook;

/// What a masked secret shows as.
pub const MASK: &str = "••••••";

/// A parameter value for one run.
#[derive(Clone)]
pub enum ParamValue {
    /// A plain value.
    Plain(String),
    /// A secret value (never displayed, logged or stored in plain text).
    Secret(SecretString),
}

impl std::fmt::Debug for ParamValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamValue::Plain(s) => f.debug_tuple("Plain").field(s).finish(),
            ParamValue::Secret(_) => f.write_str("Secret(..)"),
        }
    }
}

impl ParamValue {
    fn expose(&self) -> &str {
        match self {
            ParamValue::Plain(s) => s,
            ParamValue::Secret(s) => s.expose_secret(),
        }
    }

    fn is_secret(&self) -> bool {
        matches!(self, ParamValue::Secret(_))
    }
}

/// The values of a run, by parameter name.
#[derive(Clone, Debug, Default)]
pub struct ParamValues(HashMap<String, ParamValue>);

impl ParamValues {
    /// No values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a plain value.
    pub fn plain(mut self, name: &str, value: &str) -> Self {
        self.0
            .insert(name.to_owned(), ParamValue::Plain(value.to_owned()));
        self
    }

    /// Set a secret value.
    pub fn secret(mut self, name: &str, value: SecretString) -> Self {
        self.0.insert(name.to_owned(), ParamValue::Secret(value));
        self
    }

    /// Set a value.
    pub fn insert(&mut self, name: &str, value: ParamValue) {
        self.0.insert(name.to_owned(), value);
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.0.get(name)
    }

    /// Whether `name` has a value.
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Plain values from `rb`'s defaults for every parameter not set yet. Secret
    /// parameters are left for the caller (their defaults live in the keychain).
    pub fn with_defaults(mut self, rb: &Runbook) -> Self {
        for p in &rb.params {
            if !p.secret && !self.0.contains_key(&p.name) {
                self.0
                    .insert(p.name.clone(), ParamValue::Plain(p.default.clone()));
            }
        }
        self
    }

    /// Every parameter name with a value.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.keys().cloned().collect();
        v.sort();
        v
    }

    /// The value of `name` as text (secrets exposed: execution only).
    pub(crate) fn expose(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(ParamValue::expose)
    }

    /// The secret values as plain strings, to redact from output.
    pub fn secret_texts(&self) -> Vec<String> {
        self.0
            .values()
            .filter(|v| v.is_secret())
            .map(|v| v.expose().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// How secret values render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// As [`MASK`] (display, history).
    Masked,
    /// The real value (execution only).
    Secrets,
}

/// Why text could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    /// `{{name}}` names no parameter of the runbook.
    #[error("unknown parameter {{{{{0}}}}}")]
    Unknown(String),
    /// A modifier other than `raw`, `ident` or `quote`.
    #[error("unknown modifier {1:?} in {{{{{0}}}}}")]
    Modifier(String, String),
    /// A secret where it would end up in plain text.
    #[error("the secret parameter {{{{{0}}}}} can only be used {1}")]
    SecretHere(String, &'static str),
    /// A statement with a secret also has placeholders of its own.
    #[error("a statement with a secret parameter cannot also use its own placeholders ({0})")]
    MixedPlaceholders(String),
}

/// One `{{…}}` placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// Start byte of `{{`.
    pub start: usize,
    /// End byte after `}}`.
    pub end: usize,
    /// Parameter name.
    pub name: String,
    /// Modifier after `:` (lowercase), if any.
    pub modifier: Option<String>,
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// The placeholders in `text`, in order.
pub fn placeholders(text: &str) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("{{") {
        let start = from + rel;
        let Some(close) = text[start + 2..].find("}}") else {
            break;
        };
        let inner = &text[start + 2..start + 2 + close];
        let end = start + 2 + close + 2;
        let (name, modifier) = match inner.split_once(':') {
            Some((n, m)) => (n.trim(), Some(m.trim().to_ascii_lowercase())),
            None => (inner.trim(), None),
        };
        if is_name(name) && !inner.contains('{') {
            out.push(Placeholder {
                start,
                end,
                name: name.to_owned(),
                modifier,
            });
            from = end;
        } else {
            from = start + 2;
        }
    }
    out
}

/// Names used in `text` (unique, in order of first use).
pub fn names_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in placeholders(text) {
        if !out.contains(&p.name) {
            out.push(p.name);
        }
    }
    out
}

fn value_of<'a>(values: &'a ParamValues, p: &Placeholder) -> Result<&'a ParamValue, ParamError> {
    values
        .get(&p.name)
        .ok_or_else(|| ParamError::Unknown(p.name.clone()))
}

/// `s` in single quotes for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Substitute placeholders in plain text (HTTP, shell, notes).
pub fn render_text(text: &str, values: &ParamValues, reveal: Reveal) -> Result<String, ParamError> {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for p in placeholders(text) {
        let v = value_of(values, &p)?;
        let shown = if v.is_secret() && reveal == Reveal::Masked {
            MASK
        } else {
            v.expose()
        };
        out.push_str(&text[last..p.start]);
        match p.modifier.as_deref() {
            None | Some("raw") => out.push_str(shown),
            Some("quote") => out.push_str(&shell_quote(shown)),
            Some(m) => return Err(ParamError::Modifier(p.name.clone(), m.to_owned())),
        }
        last = p.end;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// One statement ready to run.
#[derive(Clone, Debug)]
pub struct ResolvedStatement {
    /// SQL sent to the server (and kept in history): no secret values in it.
    pub sql: String,
    /// Bound values (secret parameters), in binding order.
    pub params: Vec<Value>,
}

/// A SQL step resolved for one dialect.
#[derive(Clone, Debug)]
pub struct ResolvedSql {
    /// The whole text as it will run, secrets as their placeholders.
    pub display: String,
    /// Statements in order.
    pub statements: Vec<ResolvedStatement>,
    /// How many secret values are bound.
    pub secrets_bound: usize,
}

/// Prefix of the placeholders that carry secret values.
const SECRET_PREFIX: &str = "rb_secret_";

fn secret_placeholder(engine: Engine, name: &str) -> String {
    match engine {
        Engine::SqlServer => format!("@{SECRET_PREFIX}{name}"),
        _ => format!(":{SECRET_PREFIX}{name}"),
    }
}

/// Escape `v` for the inside of a string literal of `flavor`.
fn escape_in_string(v: &str, flavor: Flavor) -> String {
    let v = v.replace('\'', "''");
    match flavor {
        Flavor::MySql | Flavor::Snowflake => v.replace('\\', "\\\\"),
        Flavor::JavaScript => v.replace('\\', "\\\\").replace('"', "\\\""),
        _ => v,
    }
}

/// Resolve a SQL step: substitute plain values per dialect, bind secrets, split the
/// script into statements.
pub fn render_sql(
    dialect: &dyn Dialect,
    text: &str,
    values: &ParamValues,
) -> Result<ResolvedSql, ParamError> {
    let engine = dialect.engine();
    let flavor = dialect.flavor();
    let segs = lexer::segments(text, flavor);
    let kind_at = |pos: usize| {
        segs.iter()
            .find(|s| s.start <= pos && pos < s.end)
            .map_or(SegKind::Code, |s| s.kind)
    };
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut secrets: Vec<(String, Value)> = Vec::new();
    for p in placeholders(text) {
        let v = value_of(values, &p)?;
        out.push_str(&text[last..p.start]);
        last = p.end;
        let kind = kind_at(p.start);
        if v.is_secret() {
            if kind != SegKind::Code || p.modifier.is_some() {
                return Err(ParamError::SecretHere(
                    p.name.clone(),
                    "as a plain value in SQL code (it is bound as a parameter)",
                ));
            }
            if engine == Engine::MongoDb || !engine.is_sql() {
                return Err(ParamError::SecretHere(
                    p.name.clone(),
                    "in SQL engines that bind parameters",
                ));
            }
            let key = format!("{SECRET_PREFIX}{}", p.name);
            if !secrets.iter().any(|(k, _)| *k == key) {
                secrets.push((key, Value::Text(v.expose().to_owned())));
            }
            out.push_str(&secret_placeholder(engine, &p.name));
            continue;
        }
        let s = v.expose();
        let piece = match (kind, p.modifier.as_deref()) {
            (SegKind::Code, None) => dialect.literal(&Value::Text(s.to_owned())),
            (SegKind::Code, Some("raw")) => s.to_owned(),
            (SegKind::Code, Some("ident")) => dialect.quote_ident(s),
            (SegKind::Str, None | Some("raw")) => escape_in_string(s, flavor),
            (SegKind::Ident, None | Some("raw")) => s.replace('"', "\"\"").replace(']', "]]"),
            (SegKind::Comment, None | Some("raw")) => {
                s.replace(['\n', '\r'], " ").replace("*/", "* /")
            }
            (_, Some(m)) => return Err(ParamError::Modifier(p.name.clone(), m.to_owned())),
        };
        out.push_str(&piece);
    }
    out.push_str(&text[last..]);

    let mut statements = Vec::new();
    for span in dialect.split_script(&out) {
        let st = span.text(&out).trim();
        if st.is_empty() {
            continue;
        }
        let ours: Vec<&(String, Value)> = secrets
            .iter()
            .filter(|(k, _)| st.contains(&secret_placeholder(engine, &k[SECRET_PREFIX.len()..])))
            .collect();
        if ours.is_empty() {
            for _ in 0..span.repeat.max(1) {
                statements.push(ResolvedStatement {
                    sql: st.to_owned(),
                    params: Vec::new(),
                });
            }
            continue;
        }
        let (bound, names) = dialect.bind_params(st);
        let mut params = Vec::with_capacity(names.len());
        for n in &names {
            let key = n.trim_start_matches([':', '@', '$']);
            match ours.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
                Some((_, v)) => params.push(v.clone()),
                None => return Err(ParamError::MixedPlaceholders(n.clone())),
            }
        }
        for _ in 0..span.repeat.max(1) {
            statements.push(ResolvedStatement {
                sql: bound.clone(),
                params: params.clone(),
            });
        }
    }
    let display = if secrets.is_empty() {
        out
    } else {
        statements
            .iter()
            .map(|s| s.sql.as_str())
            .collect::<Vec<_>>()
            .join(";\n")
    };
    Ok(ResolvedSql {
        display,
        statements,
        secrets_bound: secrets.len(),
    })
}

/// Unknown names used in `text` (for the editor's warnings).
pub fn unknown_names(text: &str, rb: &Runbook) -> Vec<String> {
    names_in(text)
        .into_iter()
        .filter(|n| rb.param(n).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use switchyard_db::dialect_for;

    fn values() -> ParamValues {
        ParamValues::new()
            .plain("region", "eu-west-1")
            .plain("name", "O'Brien")
            .plain("table", "Order Items")
            .secret("token", SecretString::from("s3cr3t"))
    }

    #[test]
    fn finds_placeholders() {
        let p = placeholders("a {{x}} b {{ y : Raw }} {{1x}} {{}} {{a b}} {{z");
        assert_eq!(
            p.iter()
                .map(|p| (p.name.as_str(), p.modifier.as_deref()))
                .collect::<Vec<_>>(),
            [("x", None), ("y", Some("raw"))]
        );
        assert_eq!(names_in("{{a}} {{b}} {{a}}"), ["a", "b"]);
        assert_eq!(p[0].start, 2);
        assert_eq!(p[0].end, 7);
    }

    #[test]
    fn plain_text_masks_secrets() {
        let v = values();
        let t = "curl -H 'X: {{token}}' {{region}} {{name:quote}}";
        assert_eq!(
            render_text(t, &v, Reveal::Masked).unwrap(),
            format!("curl -H 'X: {MASK}' eu-west-1 'O'\\''Brien'")
        );
        assert_eq!(
            render_text(t, &v, Reveal::Secrets).unwrap(),
            "curl -H 'X: s3cr3t' eu-west-1 'O'\\''Brien'"
        );
        assert_eq!(
            render_text("{{nope}}", &v, Reveal::Masked),
            Err(ParamError::Unknown("nope".into()))
        );
        assert!(matches!(
            render_text("{{region:upper}}", &v, Reveal::Masked),
            Err(ParamError::Modifier(..))
        ));
        assert_eq!(
            render_text("{{ not a param }}", &v, Reveal::Masked).unwrap(),
            "{{ not a param }}"
        );
    }

    #[test]
    fn sql_quotes_per_dialect() {
        let v = values();
        let pg = dialect_for(Engine::Postgres);
        let r = render_sql(
            pg,
            "SELECT * FROM {{table:ident}} WHERE n = {{name}} AND r = '{{region}}-x' LIMIT {{region:raw}}",
            &v,
        )
        .unwrap();
        assert_eq!(
            r.display,
            "SELECT * FROM \"Order Items\" WHERE n = 'O''Brien' AND r = 'eu-west-1-x' LIMIT eu-west-1"
        );
        assert_eq!(r.statements.len(), 1);
        assert!(r.statements[0].params.is_empty());

        let ms = dialect_for(Engine::SqlServer);
        let r = render_sql(ms, "SELECT {{table:ident}}, {{name}}", &v).unwrap();
        assert_eq!(r.display, "SELECT [Order Items], N'O''Brien'");

        let my = dialect_for(Engine::MySql);
        let r = render_sql(
            my,
            "SELECT '{{x}}'",
            &ParamValues::new().plain("x", r"a\'b"),
        )
        .unwrap();
        assert_eq!(r.display, r"SELECT 'a\\''b'");
    }

    #[test]
    fn sql_injection_attempt_stays_a_literal() {
        let v = ParamValues::new().plain("id", "1; DROP TABLE users; --");
        let pg = dialect_for(Engine::Postgres);
        let r = render_sql(pg, "SELECT * FROM t WHERE id = {{id}};", &v).unwrap();
        assert_eq!(r.statements.len(), 1, "no second statement appears");
        assert_eq!(
            r.statements[0].sql,
            "SELECT * FROM t WHERE id = '1; DROP TABLE users; --'"
        );
    }

    #[test]
    fn secrets_are_bound_not_inlined() {
        let v = values();
        let pg = dialect_for(Engine::Postgres);
        let r = render_sql(
            pg,
            "SELECT 1 WHERE {{token}} = {{token}} AND r = {{region}};\nSELECT 2;",
            &v,
        )
        .unwrap();
        assert_eq!(r.secrets_bound, 1);
        assert_eq!(r.statements.len(), 2);
        assert_eq!(
            r.statements[0].sql,
            "SELECT 1 WHERE $1 = $1 AND r = 'eu-west-1'"
        );
        assert_eq!(r.statements[0].params, [Value::Text("s3cr3t".into())]);
        assert!(r.statements[1].params.is_empty());
        assert!(!r.display.contains("s3cr3t"));

        let ms = dialect_for(Engine::SqlServer);
        let r = render_sql(ms, "SELECT {{token}}", &v).unwrap();
        assert!(!r.statements[0].sql.contains("s3cr3t"));
        assert_eq!(r.statements[0].params.len(), 1);

        assert!(matches!(
            render_sql(pg, "SELECT '{{token}}'", &v),
            Err(ParamError::SecretHere(..))
        ));
        assert!(matches!(
            render_sql(pg, "SELECT {{token}}, $1", &v),
            Err(ParamError::MixedPlaceholders(_))
        ));
        assert!(matches!(
            render_sql(dialect_for(Engine::MongoDb), "db.x.find({{token}})", &v),
            Err(ParamError::SecretHere(..))
        ));
    }

    #[test]
    fn defaults_fill_plain_params_only() {
        let mut rb = Runbook::new("x");
        rb.params = vec![
            switchyard_store::RunbookParam::new("a", "1"),
            switchyard_store::RunbookParam::secret("s"),
        ];
        let v = ParamValues::new().plain("a", "2").with_defaults(&rb);
        assert!(matches!(v.get("a"), Some(ParamValue::Plain(s)) if s == "2"));
        assert!(!v.contains("s"));
        let v = ParamValues::new().with_defaults(&rb);
        assert!(matches!(v.get("a"), Some(ParamValue::Plain(s)) if s == "1"));
        assert_eq!(unknown_names("{{a}} {{b}}", &rb), ["b"]);
        let v = ParamValues::new().secret("s", SecretString::from("x"));
        assert_eq!(v.secret_texts(), ["x"]);
        assert_eq!(format!("{:?}", v.get("s").unwrap()), "Secret(..)");
    }
}
