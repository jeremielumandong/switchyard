//! Schema-aware completion: resolves tables and aliases referenced by the statement at the
//! cursor and ranks keywords, tables, columns and functions for a typed prefix.

use std::collections::HashMap;

use crate::catalog::ColumnInfo;
use crate::dialect::Dialect;
use crate::dialect::lexer::{self, SegKind};

/// A table reference in a statement (`schema.table AS alias`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    /// Schema, when qualified.
    pub schema: Option<String>,
    /// Table name.
    pub name: String,
    /// Alias, when given.
    pub alias: Option<String>,
}

/// What a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CandidateKind {
    /// A column of a table in the statement.
    Column,
    /// A table or view.
    Table,
    /// A schema.
    Schema,
    /// A function.
    Function,
    /// A keyword.
    Keyword,
}

/// A completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Text shown.
    pub label: String,
    /// Text inserted (replaces the typed prefix).
    pub insert: String,
    /// Kind.
    pub kind: CandidateKind,
    /// Detail (column type, table schema).
    pub detail: String,
}

/// Columns indexed for completion.
#[derive(Clone, Debug, Default)]
pub struct CatalogIndex {
    /// Columns per (schema, table), in ordinal order.
    pub columns: HashMap<(String, String), Vec<(String, String)>>,
    /// How often each object name was used recently (higher ranks first).
    pub recent: HashMap<String, u32>,
    /// The session's current schema; unqualified names resolve there first.
    pub preferred_schema: Option<String>,
}

impl CatalogIndex {
    /// Build from `AllColumns` introspection output.
    pub fn from_columns(cols: &[ColumnInfo]) -> Self {
        let mut columns: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
        for c in cols {
            columns
                .entry((c.schema.clone(), c.table.clone()))
                .or_default()
                .push((c.name.clone(), c.data_type.clone()));
        }
        Self {
            columns,
            recent: HashMap::new(),
            preferred_schema: None,
        }
    }

    fn tables_named(
        &self,
        name: &str,
    ) -> impl Iterator<Item = (&(String, String), &Vec<(String, String)>)> {
        self.columns
            .iter()
            .filter(move |((_, t), _)| t.eq_ignore_ascii_case(name))
    }

    /// Columns of a table, optionally schema-qualified (prefers `public`/`dbo`).
    pub fn columns_of(&self, schema: Option<&str>, name: &str) -> Vec<(String, String)> {
        let mut found: Vec<_> = self
            .tables_named(name)
            .filter(|((s, _), _)| schema.is_none_or(|q| s.eq_ignore_ascii_case(q)))
            .collect();
        let preferred = self.preferred_schema.as_deref();
        found.sort_by_key(|((s, _), _)| {
            (
                !preferred.is_some_and(|p| s.eq_ignore_ascii_case(p)),
                !matches!(s.as_str(), "public" | "dbo"),
            )
        });
        found.first().map(|(_, c)| (*c).clone()).unwrap_or_default()
    }

    /// Record use of an object name (ranking).
    pub fn touch(&mut self, name: &str) {
        *self.recent.entry(name.to_ascii_lowercase()).or_default() += 1;
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') || t.starts_with('[') && t.ends_with(']'))
        && t.len() >= 2
    {
        t[1..t.len() - 1].to_owned()
    } else {
        t.to_owned()
    }
}

/// Tokens: identifiers (plain or quoted), keywords and punctuation, skipping strings and
/// comments.
fn tokens(dialect: &dyn Dialect, sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in lexer::segments(sql, dialect.flavor()) {
        let text = &sql[seg.start..seg.end];
        match seg.kind {
            SegKind::Ident => out.push(text.to_owned()),
            SegKind::Str => out.push("'".into()),
            SegKind::Comment => {}
            SegKind::Code => {
                let mut cur = String::new();
                for ch in text.chars() {
                    if ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '@' || ch == '#' {
                        cur.push(ch);
                    } else {
                        if !cur.is_empty() {
                            out.push(std::mem::take(&mut cur));
                        }
                        if !ch.is_whitespace() {
                            out.push(ch.to_string());
                        }
                    }
                }
                if !cur.is_empty() {
                    out.push(cur);
                }
            }
        }
    }
    out
}

const NOT_ALIAS: &[&str] = &[
    "WHERE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "FULL",
    "CROSS",
    "OUTER",
    "ON",
    "USING",
    "GROUP",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "WINDOW",
    "FETCH",
    "FOR",
    "NATURAL",
    "LATERAL",
    "RETURNING",
    "WITH",
];

/// Table references in a statement: `FROM a [AS] x, b y JOIN c z`, `UPDATE t`, `INTO t`.
pub fn table_refs(dialect: &dyn Dialect, sql: &str) -> Vec<TableRef> {
    let toks = tokens(dialect, sql);
    let mut out = Vec::new();
    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let mut i = 0;
    let is_ident = |t: &str| {
        t.starts_with('"')
            || t.starts_with('[')
            || t.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
    };
    while i < toks.len() {
        let starts_list = matches!(upper[i].as_str(), "FROM" | "JOIN" | "UPDATE" | "INTO");
        if !starts_list {
            i += 1;
            continue;
        }
        let in_from = upper[i] == "FROM";
        i += 1;
        loop {
            if i >= toks.len() || toks[i] == "(" || !is_ident(&toks[i]) {
                break;
            }
            let mut name = unquote(&toks[i]);
            let mut schema = None;
            i += 1;
            while i + 1 < toks.len() && toks[i] == "." && is_ident(&toks[i + 1]) {
                schema = Some(name);
                name = unquote(&toks[i + 1]);
                i += 2;
            }
            let mut alias = None;
            if i < toks.len() && upper[i] == "AS" {
                i += 1;
            }
            if i < toks.len() && is_ident(&toks[i]) && !NOT_ALIAS.contains(&upper[i].as_str()) {
                alias = Some(unquote(&toks[i]));
                i += 1;
            }
            out.push(TableRef {
                schema,
                name,
                alias,
            });
            if in_from && i < toks.len() && toks[i] == "," {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// The table an identifier under the cursor refers to (peek table, F12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeekTarget {
    /// Schema, when qualified in the identifier or its `FROM` reference.
    pub schema: Option<String>,
    /// Table name.
    pub table: String,
}

/// The dotted identifier chain around byte `offset` as unquoted parts, using the lexer so
/// quoted identifiers may contain spaces and dots. `None` in strings and comments.
fn chain_at(dialect: &dyn Dialect, script: &str, offset: usize) -> Option<Vec<String>> {
    #[derive(PartialEq)]
    enum Tok {
        Ident,
        Dot,
        Other,
    }
    let mut toks: Vec<(usize, usize, Tok)> = Vec::new();
    for seg in lexer::segments(script, dialect.flavor()) {
        match seg.kind {
            SegKind::Ident => toks.push((seg.start, seg.end, Tok::Ident)),
            SegKind::Str | SegKind::Comment => toks.push((seg.start, seg.end, Tok::Other)),
            SegKind::Code => {
                let text = &script[seg.start..seg.end];
                let mut word: Option<usize> = None;
                for (i, ch) in text.char_indices() {
                    let at = seg.start + i;
                    if ch.is_alphanumeric() || matches!(ch, '_' | '$' | '#') {
                        word.get_or_insert(at);
                        continue;
                    }
                    if let Some(w) = word.take() {
                        toks.push((w, at, Tok::Ident));
                    }
                    if ch == '.' {
                        toks.push((at, at + 1, Tok::Dot));
                    } else if !ch.is_whitespace() {
                        toks.push((at, at + ch.len_utf8(), Tok::Other));
                    }
                }
                if let Some(w) = word {
                    toks.push((w, seg.end, Tok::Ident));
                }
            }
        }
    }
    // The identifier containing the cursor, or ending right at it.
    let ix = toks
        .iter()
        .position(|(s, e, k)| *k == Tok::Ident && *s <= offset && offset <= *e)
        .or_else(|| {
            // On a dot: take the identifier after it.
            toks.iter()
                .position(|(s, e, k)| *k == Tok::Dot && *s <= offset && offset < *e)
                .map(|d| d + 1)
                .filter(|&i| toks.get(i).is_some_and(|t| t.2 == Tok::Ident))
        })?;
    let adjacent = |a: &(usize, usize, Tok), b: &(usize, usize, Tok)| a.1 == b.0;
    let mut first = ix;
    while first >= 2
        && toks[first - 1].2 == Tok::Dot
        && toks[first - 2].2 == Tok::Ident
        && adjacent(&toks[first - 2], &toks[first - 1])
        && adjacent(&toks[first - 1], &toks[first])
    {
        first -= 2;
    }
    let mut last = ix;
    while last + 2 < toks.len()
        && toks[last + 1].2 == Tok::Dot
        && toks[last + 2].2 == Tok::Ident
        && adjacent(&toks[last], &toks[last + 1])
        && adjacent(&toks[last + 1], &toks[last + 2])
    {
        last += 2;
    }
    let parts: Vec<String> = toks[first..=last]
        .iter()
        .filter(|t| t.2 == Tok::Ident)
        .map(|t| unquote(&script[t.0..t.1]))
        .collect();
    let lead = parts.first()?.chars().next()?;
    (!lead.is_ascii_digit() && parts.iter().all(|p| !p.is_empty())).then_some(parts)
}

/// Resolve the identifier at byte `offset` to a table: a table name, `schema.table`, an
/// alias from the statement's `FROM` list, or `alias.column` / `table.column` (the table).
pub fn peek_target(dialect: &dyn Dialect, script: &str, offset: usize) -> Option<PeekTarget> {
    let offset = offset.min(script.len());
    let parts = chain_at(dialect, script, offset)?;
    let stmt = dialect
        .statement_at(script, offset)
        .map(|s| &script[s.start..s.end])
        .unwrap_or(script);
    let refs = table_refs(dialect, stmt);
    let by_alias = |name: &str| {
        refs.iter()
            .find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(name))
            })
            .or_else(|| {
                refs.iter()
                    .find(|r| r.alias.is_none() && r.name.eq_ignore_ascii_case(name))
            })
            .map(|r| PeekTarget {
                schema: r.schema.clone(),
                table: r.name.clone(),
            })
    };
    let schema_of = |schema: &str, table: &str| {
        // A qualified table keeps its schema; a FROM reference adds nothing.
        PeekTarget {
            schema: Some(schema.to_owned()),
            table: table.to_owned(),
        }
    };
    match parts.as_slice() {
        [name] => Some(by_alias(name).unwrap_or(PeekTarget {
            schema: None,
            table: name.clone(),
        })),
        [a, b] => {
            // `alias.column` or `table.column` peeks the table; otherwise `schema.table`.
            let qualifier = by_alias(a).filter(|_| {
                refs.iter().all(|r| {
                    !r.schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(a))
                })
            });
            Some(qualifier.unwrap_or_else(|| schema_of(a, b)))
        }
        [a, b, c] => {
            // `schema.table.column`, else `database.schema.table`.
            let is_ref = refs.iter().any(|r| {
                r.name.eq_ignore_ascii_case(b)
                    && r.schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(a))
            });
            Some(if is_ref {
                schema_of(a, b)
            } else {
                schema_of(b, c)
            })
        }
        [.., s, t] => Some(schema_of(s, t)),
        [] => None,
    }
}

/// Candidates for completing at byte `offset` in `script`, where `typed` is the text typed
/// since completion started (it may begin with `.` after a qualifier).
pub fn complete(
    dialect: &dyn Dialect,
    index: &CatalogIndex,
    script: &str,
    offset: usize,
    typed: &str,
) -> Vec<Candidate> {
    let offset = offset.min(script.len());
    // `typed` may carry its qualifier (`o.st`) or start at the dot (`.st`).
    let (qualifier, prefix): (Option<String>, &str) = match typed.rfind('.') {
        Some(i) => {
            let q = typed[..i].trim();
            let q = if q.is_empty() {
                // Read the identifier right before the dot from the script.
                let before = &script[..offset.saturating_sub(typed.len() - i)];
                let q: String = before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '"' | '[' | ']'))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                q
            } else {
                q.to_owned()
            };
            (Some(q), typed[i + 1..].trim())
        }
        None => (None, typed.trim()),
    };
    // What precedes the candidate in the inserted text (it replaces all of `typed`).
    let lead = match typed.rfind('.') {
        Some(i) => typed[..=i].to_owned(),
        None => String::new(),
    };
    let stmt = dialect
        .statement_at(script, offset)
        .map(|s| &script[s.start..s.end.max(offset).min(script.len())])
        .unwrap_or(script);
    let refs = table_refs(dialect, stmt);
    let mut out: Vec<Candidate> = Vec::new();
    let matches = |label: &str| label.to_lowercase().starts_with(&prefix.to_lowercase());

    if let Some(qual) = qualifier {
        let qual = unquote(&qual);
        // Alias or table name → columns.
        let target = refs.iter().find(|r| {
            r.alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(&qual))
                || r.name.eq_ignore_ascii_case(&qual)
        });
        let cols = match target {
            Some(r) => index.columns_of(r.schema.as_deref(), &r.name),
            None => index.columns_of(None, &qual),
        };
        for (name, ty) in cols {
            if matches(&name) {
                out.push(Candidate {
                    insert: format!("{lead}{}", dialect.quote_ident(&name)),
                    label: name,
                    kind: CandidateKind::Column,
                    detail: ty,
                });
            }
        }
        // Schema name → its tables.
        if out.is_empty() {
            let mut tables: Vec<&String> = index
                .columns
                .keys()
                .filter(|(s, _)| s.eq_ignore_ascii_case(&qual))
                .map(|(_, t)| t)
                .collect();
            tables.sort();
            for t in tables {
                if matches(t) {
                    out.push(Candidate {
                        insert: format!("{lead}{}", dialect.quote_ident(t)),
                        label: t.clone(),
                        kind: CandidateKind::Table,
                        detail: qual.clone(),
                    });
                }
            }
        }
        return out;
    }

    if prefix.is_empty() {
        return out;
    }
    // Columns of tables in this statement.
    let mut seen = std::collections::HashSet::new();
    for r in &refs {
        for (name, ty) in index.columns_of(r.schema.as_deref(), &r.name) {
            if matches(&name) && seen.insert(name.clone()) {
                out.push(Candidate {
                    insert: dialect.quote_ident(&name),
                    label: name,
                    kind: CandidateKind::Column,
                    detail: format!(
                        "{ty} · {}",
                        r.alias.clone().unwrap_or_else(|| r.name.clone())
                    ),
                });
            }
        }
    }
    // Tables and schemas.
    let mut tables: Vec<&(String, String)> =
        index.columns.keys().filter(|(_, t)| matches(t)).collect();
    tables.sort_by(|a, b| {
        let ra = index
            .recent
            .get(&a.1.to_ascii_lowercase())
            .copied()
            .unwrap_or(0);
        let rb = index
            .recent
            .get(&b.1.to_ascii_lowercase())
            .copied()
            .unwrap_or(0);
        rb.cmp(&ra).then_with(|| a.1.cmp(&b.1))
    });
    for (s, t) in tables {
        out.push(Candidate {
            insert: dialect.quote_ident(t),
            label: t.clone(),
            kind: CandidateKind::Table,
            detail: s.clone(),
        });
    }
    let mut schemas: Vec<&String> = index.columns.keys().map(|(s, _)| s).collect();
    schemas.sort();
    schemas.dedup();
    for s in schemas.into_iter().filter(|s| matches(s)) {
        out.push(Candidate {
            insert: dialect.quote_ident(s),
            label: s.clone(),
            kind: CandidateKind::Schema,
            detail: "schema".into(),
        });
    }
    for f in dialect.functions() {
        if matches(f) {
            out.push(Candidate {
                insert: format!("{f}("),
                label: format!("{f}()"),
                kind: CandidateKind::Function,
                detail: "function".into(),
            });
        }
    }
    let upper_prefix = prefix.chars().next().is_some_and(char::is_uppercase);
    for k in dialect.keywords() {
        if matches(k) {
            let text = if upper_prefix {
                (*k).to_owned()
            } else {
                k.to_lowercase()
            };
            out.push(Candidate {
                insert: text.clone(),
                label: text,
                kind: CandidateKind::Keyword,
                detail: "keyword".into(),
            });
        }
    }
    out.truncate(80);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::postgres::PostgresDialect;

    fn index() -> CatalogIndex {
        let col = |s: &str, t: &str, n: &str, ty: &str| ColumnInfo {
            schema: s.into(),
            table: t.into(),
            name: n.into(),
            data_type: ty.into(),
            nullable: true,
            default: None,
            ordinal: 0,
            is_primary_key: false,
            comment: None,
        };
        CatalogIndex::from_columns(&[
            col("public", "orders", "id", "bigint"),
            col("public", "orders", "status", "text"),
            col("public", "orders", "store_id", "integer"),
            col("public", "orders", "customer_id", "bigint"),
            col("public", "customers", "id", "bigint"),
            col("public", "customers", "segment", "text"),
            col("analytics", "orders", "day", "date"),
        ])
    }

    #[test]
    fn resolves_aliases_and_joins() {
        let d = PostgresDialect;
        let refs = table_refs(
            &d,
            "SELECT * FROM customers c JOIN public.orders AS o ON o.customer_id = c.id, payments WHERE 1=1",
        );
        assert_eq!(
            refs,
            vec![
                TableRef {
                    schema: None,
                    name: "customers".into(),
                    alias: Some("c".into())
                },
                TableRef {
                    schema: Some("public".into()),
                    name: "orders".into(),
                    alias: Some("o".into())
                },
            ]
        );
        let refs = table_refs(&d, "select * from a x, b where x.id = b.id");
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[1],
            TableRef {
                schema: None,
                name: "b".into(),
                alias: None
            }
        );
        assert_eq!(
            table_refs(&d, "update orders set status = 'x'")[0].name,
            "orders"
        );
    }

    #[test]
    fn columns_for_alias_in_multi_join() {
        let d = PostgresDialect;
        let sql = "SELECT c.id FROM customers c JOIN orders o ON o.customer_id = c.id WHERE o.st";
        let got = complete(&d, &index(), sql, sql.len(), ".st");
        let labels: Vec<_> = got.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["status", "store_id"]);
        assert_eq!(got[0].insert, ".status");
        // The editor may hand over the qualifier too.
        let got = complete(&d, &index(), sql, sql.len(), "o.st");
        assert_eq!(got[0].insert, "o.status");
        let sql2 = "SELECT c. FROM customers c JOIN orders o ON true";
        let at = sql2.find("c.").unwrap() + 2;
        let got = complete(&d, &index(), sql2, at, ".");
        let labels: Vec<_> = got.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["id", "segment"]);
    }

    #[test]
    fn prefix_ranks_statement_columns_first() {
        let d = PostgresDialect;
        let sql = "select seg from customers";
        let at = sql.find("seg").unwrap() + 3;
        let got = complete(&d, &index(), sql, at, "seg");
        assert_eq!(got[0].kind, CandidateKind::Column);
        assert_eq!(got[0].label, "segment");
        let got = complete(&d, &index(), "select * from ord", 17, "ord");
        assert!(
            got.iter()
                .any(|c| c.kind == CandidateKind::Table && c.label == "orders")
        );
        let got = complete(&d, &index(), "SEL", 3, "SEL");
        assert_eq!(
            got.iter()
                .find(|c| c.kind == CandidateKind::Keyword)
                .unwrap()
                .insert,
            "SELECT"
        );
    }

    fn peek_at(sql: &str, marker: &str) -> Option<PeekTarget> {
        // The cursor sits one character into the first occurrence of `marker`.
        let at = sql.find(marker).unwrap() + 1;
        peek_target(&PostgresDialect, sql, at)
    }

    fn target(schema: Option<&str>, table: &str) -> Option<PeekTarget> {
        Some(PeekTarget {
            schema: schema.map(str::to_owned),
            table: table.to_owned(),
        })
    }

    #[test]
    fn peek_resolves_identifier_under_cursor() {
        let sql = "SELECT o.status, c.segment FROM public.orders o JOIN customers c ON true";
        // A bare table name.
        assert_eq!(peek_at(sql, "customers"), target(None, "customers"));
        // schema.table, with the cursor on either part.
        assert_eq!(peek_at(sql, "public."), target(Some("public"), "orders"));
        assert_eq!(peek_at(sql, "orders o"), target(Some("public"), "orders"));
        // An alias, alone or as a column qualifier (cursor on the alias or the column).
        assert_eq!(peek_at(sql, "o JOIN"), target(Some("public"), "orders"));
        assert_eq!(peek_at(sql, "o.status"), target(Some("public"), "orders"));
        assert_eq!(peek_at(sql, "status"), target(Some("public"), "orders"));
        assert_eq!(peek_at(sql, "segment"), target(None, "customers"));
        // A table name used as a qualifier.
        let sql = "select customers.id from customers";
        assert_eq!(peek_at(sql, "customers.id"), target(None, "customers"));
        // Quoted parts and three-part names.
        let sql = r#"select * from "Sales"."Big Orders""#;
        assert_eq!(peek_at(sql, "Big"), target(Some("Sales"), "Big Orders"));
        let sql = "select x.y.z from shop.public.orders";
        assert_eq!(peek_at(sql, "orders"), target(Some("public"), "orders"));
        // Strings, comments, numbers and whitespace resolve to nothing.
        assert_eq!(peek_at("select 'orders'", "orders"), None);
        assert_eq!(peek_at("select 1 -- orders", "orders"), None);
        assert_eq!(peek_at("select 42", "42"), None);
        assert_eq!(peek_target(&PostgresDialect, "select  1", 7), None);
    }

    #[test]
    fn schema_qualifier_lists_tables() {
        let d = PostgresDialect;
        let got = complete(&d, &index(), "select * from analytics.", 24, ".");
        assert_eq!(
            got.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["orders"]
        );
    }
}
