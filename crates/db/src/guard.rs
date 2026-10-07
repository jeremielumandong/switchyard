//! Statement classification with `sqlparser`: destructive statements for Production
//! confirmation, and read-only checks for read-only connections and agent tools.

use sqlparser::ast::{ObjectType, Query, SetExpr, Statement};
use sqlparser::parser::Parser;

use crate::dialect::Dialect;

/// Why a statement needs confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestructiveKind {
    /// `DROP <object>`.
    Drop {
        /// Object type (`TABLE`, `VIEW`, ...).
        object_type: String,
    },
    /// `TRUNCATE`.
    Truncate,
    /// `DELETE` without `WHERE`.
    DeleteWithoutWhere,
    /// `UPDATE` without `WHERE`.
    UpdateWithoutWhere,
}

impl DestructiveKind {
    /// Short label (`no WHERE`, `DROP TABLE`, ...).
    pub fn label(&self) -> String {
        match self {
            DestructiveKind::Drop { object_type } => format!("DROP {object_type}"),
            DestructiveKind::Truncate => "TRUNCATE".into(),
            DestructiveKind::DeleteWithoutWhere | DestructiveKind::UpdateWithoutWhere => {
                "no WHERE".into()
            }
        }
    }
}

/// A destructive statement found in a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destructive {
    /// What makes it destructive.
    pub kind: DestructiveKind,
    /// Affected objects, as written.
    pub objects: Vec<String>,
}

impl Destructive {
    /// Headline for the confirmation dialog.
    pub fn headline(&self) -> String {
        let obj = self.objects.first().map(String::as_str).unwrap_or("object");
        let short = obj.rsplit('.').next().unwrap_or(obj);
        match &self.kind {
            DestructiveKind::Drop { object_type } => {
                format!("Drop {} {short}?", object_type.to_lowercase())
            }
            DestructiveKind::Truncate => format!("Remove every row in {short}?"),
            DestructiveKind::DeleteWithoutWhere => format!("Delete every row in {short}?"),
            DestructiveKind::UpdateWithoutWhere => format!("Update every row in {short}?"),
        }
    }

    /// Explanation line for the confirmation dialog.
    pub fn explanation(&self) -> &'static str {
        match self.kind {
            DestructiveKind::Drop { .. } => "The script drops a database object.",
            DestructiveKind::Truncate => "The script truncates a table.",
            DestructiveKind::DeleteWithoutWhere => {
                "The script contains a DELETE without a WHERE clause."
            }
            DestructiveKind::UpdateWithoutWhere => {
                "The script contains an UPDATE without a WHERE clause."
            }
        }
    }
}

/// Result of classifying a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classification {
    /// Parsed; read-only (SELECT/WITH, EXPLAIN without ANALYZE, SHOW).
    ReadOnly,
    /// Parsed; writes or DDL. Destructive findings listed.
    Write(Vec<Destructive>),
    /// Could not be parsed; callers must treat it as a write.
    Unparsed(String),
}

impl Classification {
    /// Whether this is known to be read-only.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Classification::ReadOnly)
    }

    /// Destructive findings, if any.
    pub fn destructive(&self) -> &[Destructive] {
        match self {
            Classification::Write(d) => d,
            _ => &[],
        }
    }
}

fn names(n: &[impl std::fmt::Display]) -> Vec<String> {
    n.iter().map(ToString::to_string).collect()
}

fn object_type_name(t: &ObjectType) -> String {
    t.to_string().to_uppercase()
}

fn set_expr_read_only(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(sel) => sel.into.is_none(),
        SetExpr::Query(q) => query_read_only(q),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_read_only(left) && set_expr_read_only(right)
        }
        SetExpr::Values(_) | SetExpr::Table(_) => true,
        _ => false,
    }
}

fn query_read_only(q: &Query) -> bool {
    set_expr_read_only(&q.body)
        && q.with
            .as_ref()
            .is_none_or(|w| w.cte_tables.iter().all(|cte| query_read_only(&cte.query)))
}

fn statement_is_read_only(stmt: &Statement) -> bool {
    match stmt {
        Statement::Query(q) => query_read_only(q),
        Statement::Explain {
            analyze, statement, ..
        } => !*analyze || statement_is_read_only(statement),
        Statement::ExplainTable { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowColumns { .. } => true,
        _ => false,
    }
}

fn destructive_of(stmt: &Statement) -> Option<Destructive> {
    match stmt {
        Statement::Drop {
            object_type,
            names: n,
            ..
        } => Some(Destructive {
            kind: DestructiveKind::Drop {
                object_type: object_type_name(object_type),
            },
            objects: names(n),
        }),
        Statement::Truncate(t) => Some(Destructive {
            kind: DestructiveKind::Truncate,
            objects: t.table_names.iter().map(|t| t.name.to_string()).collect(),
        }),
        Statement::Delete(d) if d.selection.is_none() && d.using.is_none() => {
            let from = match &d.from {
                sqlparser::ast::FromTable::WithFromKeyword(t)
                | sqlparser::ast::FromTable::WithoutKeyword(t) => t,
            };
            let mut objects = names(&d.tables);
            objects.extend(from.iter().map(|t| t.relation.to_string()));
            Some(Destructive {
                kind: DestructiveKind::DeleteWithoutWhere,
                objects,
            })
        }
        Statement::Update(u) if u.selection.is_none() && u.from.is_none() => Some(Destructive {
            kind: DestructiveKind::UpdateWithoutWhere,
            objects: vec![u.table.relation.to_string()],
        }),
        _ => None,
    }
}

/// Classify one statement or batch. Multiple statements are combined: read-only only if
/// every statement is.
pub fn classify(dialect: &dyn Dialect, sql: &str) -> Classification {
    let pd = dialect.parser_dialect();
    match Parser::parse_sql(pd.as_ref(), sql) {
        Ok(stmts) if stmts.is_empty() => Classification::ReadOnly,
        Ok(stmts) => {
            if stmts.iter().all(statement_is_read_only) {
                Classification::ReadOnly
            } else {
                Classification::Write(stmts.iter().filter_map(destructive_of).collect())
            }
        }
        Err(e) => Classification::Unparsed(e.to_string()),
    }
}

/// Destructive statements across a whole script (each unit parsed separately so one
/// unparseable statement does not hide the others).
pub fn destructive_in_script(dialect: &dyn Dialect, script: &str) -> Vec<(usize, Destructive)> {
    let mut out = Vec::new();
    for span in dialect.split_script(script) {
        if let Classification::Write(found) = classify(dialect, span.text(script)) {
            out.extend(found.into_iter().map(|d| (span.start, d)));
        }
    }
    out
}

/// Whether `sql` is a single read-only query (SELECT or WITH ... SELECT). Used by agent
/// tools, which accept nothing else.
pub fn is_single_select(dialect: &dyn Dialect, sql: &str) -> bool {
    let pd = dialect.parser_dialect();
    match Parser::parse_sql(pd.as_ref(), sql) {
        Ok(stmts) => {
            stmts.len() == 1
                && matches!(&stmts[0], Statement::Query(_))
                && statement_is_read_only(&stmts[0])
        }
        Err(_) => false,
    }
}

/// Whether `sql` is one statement an agent may ask an estimated plan for: a query or DML
/// (INSERT, UPDATE, DELETE, MERGE). Never DDL, never several statements.
pub fn is_single_plannable(dialect: &dyn Dialect, sql: &str) -> bool {
    let pd = dialect.parser_dialect();
    match Parser::parse_sql(pd.as_ref(), sql) {
        Ok(stmts) => {
            stmts.len() == 1
                && matches!(
                    &stmts[0],
                    Statement::Query(_)
                        | Statement::Insert(_)
                        | Statement::Update(_)
                        | Statement::Delete(_)
                        | Statement::Merge { .. }
                )
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{postgres::PostgresDialect, tsql::TSqlDialect};

    fn kinds(sql: &str) -> Vec<DestructiveKind> {
        classify(&PostgresDialect, sql)
            .destructive()
            .iter()
            .map(|d| d.kind.clone())
            .collect()
    }

    #[test]
    fn detects_destructive() {
        assert_eq!(
            kinds("DROP TABLE orders"),
            [DestructiveKind::Drop {
                object_type: "TABLE".into()
            }]
        );
        assert_eq!(
            kinds("TRUNCATE abandoned_carts"),
            [DestructiveKind::Truncate]
        );
        assert_eq!(
            kinds("DELETE FROM abandoned_carts"),
            [DestructiveKind::DeleteWithoutWhere]
        );
        assert_eq!(
            kinds("UPDATE customers SET segment = 'vip'"),
            [DestructiveKind::UpdateWithoutWhere]
        );
        assert!(kinds("DELETE FROM carts WHERE id = 1").is_empty());
        assert!(kinds("UPDATE customers SET segment = 'vip' WHERE id = 3").is_empty());
        assert!(kinds("INSERT INTO t VALUES (1)").is_empty());
        // Comments and strings do not fool the parser the way a regex would.
        assert!(kinds("SELECT 'DELETE FROM x' -- DROP TABLE y").is_empty());
    }

    #[test]
    fn delete_headline() {
        let c = classify(&PostgresDialect, "DELETE FROM public.abandoned_carts");
        let d = &c.destructive()[0];
        assert_eq!(d.objects, ["public.abandoned_carts"]);
        assert_eq!(d.headline(), "Delete every row in abandoned_carts?");
    }

    #[test]
    fn read_only() {
        assert!(classify(&PostgresDialect, "select 1").is_read_only());
        assert!(classify(&PostgresDialect, "with a as (select 1) select * from a").is_read_only());
        assert!(classify(&PostgresDialect, "explain select 1").is_read_only());
        assert!(!classify(&PostgresDialect, "explain analyze delete from t").is_read_only());
        assert!(!classify(&PostgresDialect, "insert into t values (1)").is_read_only());
        assert!(
            !classify(
                &PostgresDialect,
                "with d as (delete from t returning *) select * from d"
            )
            .is_read_only()
        );
        assert!(matches!(
            classify(&PostgresDialect, "selec 1"),
            Classification::Unparsed(_)
        ));
    }

    #[test]
    fn script_and_single_select() {
        let found =
            destructive_in_script(&PostgresDialect, "select 1;\nDELETE FROM a;\ndrop view v;");
        assert_eq!(found.len(), 2);
        assert!(is_single_select(&PostgresDialect, "SELECT * FROM t"));
        assert!(!is_single_select(&PostgresDialect, "SELECT 1; SELECT 2"));
        assert!(!is_single_select(&PostgresDialect, "DELETE FROM t"));
        assert!(!is_single_select(&TSqlDialect, "SELECT * INTO t2 FROM t"));
        assert_eq!(
            destructive_in_script(&TSqlDialect, "delete from dbo.x\nGO\nselect 1").len(),
            1
        );
    }

    #[test]
    fn plannable_is_one_query_or_dml() {
        let d = &PostgresDialect;
        assert!(is_single_plannable(d, "select 1"));
        assert!(is_single_plannable(d, "update t set a = 1 where id = 2"));
        assert!(is_single_plannable(d, "delete from t"));
        assert!(!is_single_plannable(d, "drop table t"));
        assert!(!is_single_plannable(d, "create index on t (a)"));
        assert!(!is_single_plannable(d, "select 1; drop table t"));
        assert!(!is_single_plannable(d, "explain analyze delete from t"));
    }
}
