//! SQLite migration DDL. `ALTER TABLE` can only add a plain column (and rename), so any
//! other table change is the documented rebuild: create the new table under a temporary
//! name from the source's stored `CREATE TABLE`, copy the common columns, drop the old
//! table, rename, then recreate indexes and triggers
//! (<https://www.sqlite.org/lang_altertable.html#otheralter>).

use super::diff::{Normalizer, table_changes};
use super::model::{IndexDef, TableModel};
use super::script::{SchemaScripter, Stmt, TableScript};
use super::text::{first_statement, rename_created, semi};
use crate::catalog::{ColumnInfo, ObjectKind};
use crate::compare::diff::{ChangeKind, Part};
use crate::dialect::Dialect;
use crate::dialect::sqlite::SqliteDialect;

/// SQLite schema compare scripts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteScripter;

/// Prefix of the temporary table of a rebuild.
const REBUILD_PREFIX: &str = "_swy_new_";

impl SqliteScripter {
    /// The source's stored `CREATE TABLE`, renamed to `qualified`.
    fn create_as(&self, t: &TableModel, qualified: &str) -> String {
        let create = first_statement(&t.ddl, self.dialect());
        semi(&rename_created(&create, &["TABLE"], qualified))
    }

    /// The source's stored `CREATE INDEX`, created in `schema`.
    fn index_sql(&self, schema: &str, ix: &IndexDef) -> String {
        let q = if schema.is_empty() || schema == "main" {
            self.quote(&ix.name)
        } else {
            self.qualified(schema, &ix.name)
        };
        semi(&rename_created(&ix.definition, &["INDEX"], &q))
    }

    /// The table, qualified only outside `main` (SQLite's default schema).
    fn table_name(&self, schema: &str, name: &str) -> String {
        self.qualified(if schema.is_empty() { "main" } else { schema }, name)
    }

    /// Source triggers (from the stored DDL after the table and its indexes).
    fn triggers(&self, t: &TableModel) -> Vec<String> {
        let d = self.dialect();
        d.split_script(&t.ddl)
            .iter()
            .map(|s| s.text(&t.ddl).trim().to_owned())
            .filter(|s| {
                s.split_whitespace()
                    .take(4)
                    .any(|w| w.eq_ignore_ascii_case("TRIGGER"))
            })
            .map(|s| semi(&s))
            .collect()
    }
}

impl SchemaScripter for SqliteScripter {
    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect::LOCAL
    }

    fn stored_table_ddl(&self) -> bool {
        true
    }

    fn qualified(&self, schema: &str, name: &str) -> String {
        self.dialect().qualified(schema, name)
    }

    fn alter_column(&self, _table: &str, _from: &ColumnInfo, _to: &ColumnInfo) -> Vec<Stmt> {
        // Never reached: `alter_table` rebuilds instead.
        Vec::new()
    }

    fn create_table(&self, schema: &str, t: &TableModel, _n: &Normalizer<'_>) -> Vec<Stmt> {
        let mut out = vec![Stmt::new(
            self.create_as(t, &self.table_name(schema, &t.name)),
        )];
        out.extend(
            t.indexes
                .iter()
                .map(|ix| Stmt::new(self.index_sql(schema, ix))),
        );
        out
    }

    fn new_table(&self, schema: &str, t: &TableModel, n: &Normalizer<'_>) -> TableScript {
        // Foreign keys are part of SQLite's CREATE TABLE.
        let mut main = self.create_table(schema, t, n);
        main.extend(self.triggers(t).into_iter().map(Stmt::new));
        TableScript {
            main,
            ..TableScript::default()
        }
    }

    fn create_index(&self, schema: &str, _table: &str, ix: &IndexDef, _n: &Normalizer<'_>) -> Stmt {
        Stmt::new(self.index_sql(schema, ix))
    }

    fn alter_table(
        &self,
        schema: &str,
        a: &TableModel,
        b: &TableModel,
        n: &Normalizer<'_>,
    ) -> TableScript {
        let changes = table_changes(n, a, b);
        let q = self.table_name(schema, &b.name);
        // Only new plain columns and index changes: ALTER TABLE … ADD COLUMN will do.
        let simple = changes.iter().all(|c| match (c.part, c.kind) {
            (Part::Index, _) => true,
            (Part::Column, ChangeKind::Added) => a
                .column(&c.name)
                .is_some_and(|col| !col.is_primary_key && (col.nullable || col.default.is_some())),
            _ => false,
        });
        if simple {
            let mut main = Vec::new();
            for c in &changes {
                match (c.part, c.kind) {
                    (Part::Index, ChangeKind::Dropped | ChangeKind::Changed) => {
                        if let Some(ix) = b.indexes.iter().find(|i| i.name == c.name) {
                            main.push(self.drop_index(schema, &b.name, ix));
                        }
                    }
                    (Part::Column, ChangeKind::Added) => {
                        if let Some(col) = a.column(&c.name) {
                            main.extend(self.add_column(&q, col));
                        }
                    }
                    _ => {}
                }
            }
            for c in &changes {
                if c.part == Part::Index
                    && matches!(c.kind, ChangeKind::Added | ChangeKind::Changed)
                    && let Some(ix) = a.indexes.iter().find(|i| i.name == c.name)
                {
                    main.push(self.create_index(schema, &a.name, ix, n));
                }
            }
            return TableScript {
                main,
                ..TableScript::default()
            };
        }

        let tmp_name = format!("{REBUILD_PREFIX}{}", a.name);
        let tmp = self.table_name(schema, &tmp_name);
        let common: Vec<String> = a
            .columns
            .iter()
            .filter(|c| b.column(&c.name).is_some())
            .map(|c| self.quote(&c.name))
            .collect();
        let lost: Vec<&str> = b
            .columns
            .iter()
            .filter(|c| a.column(&c.name).is_none())
            .map(|c| c.name.as_str())
            .collect();
        let why: Vec<String> = changes.iter().map(|c| c.label()).collect();
        let mut sql = vec![format!(
            "-- SQLite cannot ALTER this; rebuilding {} ({}).",
            b.name,
            why.join("; ")
        )];
        if !lost.is_empty() {
            sql.push(format!(
                "-- Columns not copied (their data is lost): {}",
                lost.join(", ")
            ));
        }
        sql.push("PRAGMA foreign_keys = OFF;".into());
        sql.push("BEGIN;".into());
        sql.push(self.create_as(a, &tmp));
        if !common.is_empty() {
            let cols = common.join(", ");
            sql.push(format!(
                "INSERT INTO {tmp} ({cols})\nSELECT {cols} FROM {q};"
            ));
        }
        sql.push(format!("DROP TABLE {q};"));
        sql.push(format!(
            "ALTER TABLE {tmp} RENAME TO {};",
            self.quote(&a.name)
        ));
        for ix in &a.indexes {
            sql.push(self.index_sql(schema, ix));
        }
        sql.extend(self.triggers(a));
        sql.push(format!(
            "PRAGMA {}foreign_key_check;",
            schema_prefix(self, schema)
        ));
        sql.push("COMMIT;".into());
        sql.push("PRAGMA foreign_keys = ON;".into());
        let sql = sql.join("\n");
        TableScript {
            main: vec![if lost.is_empty() {
                Stmt::new(sql)
            } else {
                Stmt::destructive(sql)
            }],
            ..TableScript::default()
        }
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
        if exists {
            out.push(Stmt::new(format!(
                "DROP VIEW IF EXISTS {};",
                self.table_name(schema, name)
            )));
        }
        let _ = kind;
        out.push(Stmt::new(semi(&rename_created(
            ddl.trim(),
            &["VIEW"],
            &self.table_name(schema, name),
        ))));
        out
    }

    fn create_routine(
        &self,
        _kind: ObjectKind,
        _schema: &str,
        name: &str,
        _signature: Option<&str>,
        _ddl: &str,
        _exists: bool,
    ) -> Vec<Stmt> {
        vec![Stmt::note(&format!(
            "{name}: SQLite has no stored routines"
        ))]
    }
}

/// `"schema".` for a `PRAGMA` outside `main`.
fn schema_prefix(s: &SqliteScripter, schema: &str) -> String {
    if schema.is_empty() || schema == "main" {
        String::new()
    } else {
        format!("{}.", s.quote(schema))
    }
}
