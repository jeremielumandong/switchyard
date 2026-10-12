//! PostgreSQL migration DDL.

use super::diff::Normalizer;
use super::model::IndexDef;
use super::script::{SchemaScripter, Stmt, catalog_stmt};
use super::text::{collapse_ws, semi};
use crate::catalog::{ColumnInfo, ObjectKind};
use crate::dialect::Dialect;
use crate::dialect::postgres::PostgresDialect;

/// PostgreSQL schema compare scripts.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostgresScripter;

/// `serial` spelling of an integer column whose default is a sequence's `nextval`, so a
/// new table gets its own sequence.
fn serial_type(c: &ColumnInfo) -> Option<&'static str> {
    let d = c.default.as_deref()?;
    if !d.trim_start().starts_with("nextval(") {
        return None;
    }
    match c.data_type.as_str() {
        "integer" => Some("serial"),
        "bigint" => Some("bigserial"),
        "smallint" => Some("smallserial"),
        _ => None,
    }
}

impl SchemaScripter for PostgresScripter {
    fn dialect(&self) -> &'static dyn Dialect {
        &PostgresDialect
    }

    fn views_block_alter(&self) -> bool {
        true
    }

    fn normalize_default(&self, default: &str) -> String {
        // Each schema has its own sequence behind a serial column.
        if default.trim_start().starts_with("nextval(") {
            "nextval(<sequence>)".into()
        } else {
            collapse_ws(default)
        }
    }

    fn column_def(&self, c: &ColumnInfo) -> String {
        let name = self.quote(&c.name);
        if let Some(serial) = serial_type(c) {
            let nn = if c.nullable { "" } else { " NOT NULL" };
            return format!("{name} {serial}{nn}");
        }
        let mut s = format!("{name} {}", c.data_type);
        match (&c.identity, &c.default) {
            (Some(i), _) => {
                s.push(' ');
                s.push_str(i);
            }
            (None, Some(d)) => {
                s.push_str(" DEFAULT ");
                s.push_str(d);
            }
            (None, None) => {}
        }
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        s
    }

    fn alter_column(&self, table: &str, from: &ColumnInfo, to: &ColumnInfo) -> Vec<Stmt> {
        let col = self.quote(&to.name);
        let alter =
            |what: String| Stmt::new(format!("ALTER TABLE {table} ALTER COLUMN {col} {what};"));
        let mut out = Vec::new();
        let generated =
            |c: &ColumnInfo| c.identity.as_deref().is_some_and(|i| i.contains(" STORED"));
        if generated(from) || generated(to) {
            if from.identity != to.identity {
                out.push(Stmt::note(&format!(
                    "{}: a generation expression cannot be altered; drop the column and add it\n\
                     again as: {}",
                    to.name,
                    self.column_def(to)
                )));
            }
        } else if from.identity != to.identity {
            match (&from.identity, &to.identity) {
                (Some(_), None) => out.push(alter("DROP IDENTITY IF EXISTS".into())),
                (None, Some(i)) => {
                    if from.default.is_some() {
                        out.push(alter("DROP DEFAULT".into()));
                    }
                    out.push(alter(format!("ADD {i}")));
                }
                (Some(_), Some(i)) => {
                    let how = if i.contains("ALWAYS") {
                        "ALWAYS"
                    } else {
                        "BY DEFAULT"
                    };
                    out.push(alter(format!("SET GENERATED {how}")));
                }
                (None, None) => {}
            }
        }
        if collapse_ws(&from.data_type).to_lowercase() != collapse_ws(&to.data_type).to_lowercase()
        {
            out.push(alter(format!(
                "TYPE {ty} USING {col}::{ty}",
                ty = to.data_type
            )));
        }
        if self.normalize_default(from.default.as_deref().unwrap_or(""))
            != self.normalize_default(to.default.as_deref().unwrap_or(""))
            && to.identity.is_none()
        {
            out.push(match &to.default {
                Some(d) => alter(format!("SET DEFAULT {d}")),
                None => alter("DROP DEFAULT".into()),
            });
        }
        if from.nullable != to.nullable {
            out.push(alter(if to.nullable {
                "DROP NOT NULL".into()
            } else {
                "SET NOT NULL".into()
            }));
        }
        out
    }

    fn create_index(&self, schema: &str, table: &str, ix: &IndexDef, n: &Normalizer<'_>) -> Stmt {
        if ix
            .definition
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE")
        {
            // pg_get_indexdef: exact (method, INCLUDE, WHERE, opclasses).
            return catalog_stmt(&n.retarget(&ix.definition));
        }
        Stmt::new(format!(
            "CREATE {}INDEX {} ON {} ({});",
            if ix.unique { "UNIQUE " } else { "" },
            self.quote(&ix.name),
            self.qualified(schema, table),
            ix.columns.join(", ")
        ))
    }

    fn create_view(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        ddl: &str,
        exists: bool,
    ) -> Vec<Stmt> {
        let mut out = Vec::new();
        if exists && kind == ObjectKind::MaterializedView {
            out.push(self.drop_view(kind, schema, name));
        }
        // The catalog writes `CREATE OR REPLACE VIEW` (views) / `CREATE MATERIALIZED VIEW`.
        out.push(Stmt::new(semi(ddl)));
        out
    }

    fn create_routine(
        &self,
        _kind: ObjectKind,
        _schema: &str,
        _name: &str,
        _signature: Option<&str>,
        ddl: &str,
        _exists: bool,
    ) -> Vec<Stmt> {
        // pg_get_functiondef writes `CREATE OR REPLACE`.
        vec![Stmt::new(semi(ddl))]
    }

    fn drop_routine(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        signature: Option<&str>,
    ) -> Stmt {
        let kw = if kind == ObjectKind::Procedure {
            "PROCEDURE"
        } else {
            "FUNCTION"
        };
        Stmt::new(format!(
            "DROP {kw} {}{};",
            self.qualified(schema, name),
            signature.unwrap_or("")
        ))
    }
}
