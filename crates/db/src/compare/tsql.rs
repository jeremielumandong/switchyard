//! SQL Server migration DDL. Every statement is followed by `GO`: `CREATE VIEW` and
//! `CREATE PROCEDURE` must start a batch, and the default-constraint lookups declare
//! variables that belong to one batch.

use super::diff::Normalizer;
use super::model::IndexDef;
use super::script::{SchemaScripter, Stmt};
use super::text::{collapse_ws, replace_create, semi};
use crate::catalog::{ColumnInfo, ObjectKind};
use crate::dialect::tsql::TSqlDialect;
use crate::dialect::{Dialect, Flavor};

/// SQL Server schema compare scripts.
#[derive(Clone, Copy, Debug, Default)]
pub struct TSqlScripter;

/// `N'…'` literal.
fn nlit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

impl TSqlScripter {
    /// Drop the default constraint of `column` (its name is server-generated, so it is
    /// looked up when the script runs).
    fn drop_default(&self, table: &str, column: &str) -> String {
        format!(
            "DECLARE @df sysname = (SELECT d.name FROM sys.default_constraints d \
             JOIN sys.columns c ON c.default_object_id = d.object_id \
             WHERE d.parent_object_id = OBJECT_ID({}) AND c.name = {});\n\
             IF @df IS NOT NULL EXEC(N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@df));",
            nlit(table),
            nlit(column),
            table.replace('\'', "''")
        )
    }

    /// The column's type and nullability (`int NOT NULL`).
    fn type_null(c: &ColumnInfo) -> String {
        format!(
            "{} {}",
            c.data_type,
            if c.nullable { "NULL" } else { "NOT NULL" }
        )
    }

    /// `[col] DESC` from the catalog's `col DESC`.
    fn index_column(&self, c: &str) -> String {
        match c.strip_suffix(" DESC") {
            Some(name) => format!("{} DESC", self.quote(name)),
            None => self.quote(c),
        }
    }
}

impl SchemaScripter for TSqlScripter {
    fn dialect(&self) -> &'static dyn Dialect {
        &TSqlDialect
    }

    fn batch_separator(&self) -> Option<&'static str> {
        Some("GO")
    }

    fn normalize_default(&self, default: &str) -> String {
        // The server stores `((0))`; one added by hand may read `(0)`.
        let mut d = collapse_ws(default);
        while d.starts_with("((") && d.ends_with("))") {
            d = d[1..d.len() - 1].to_owned();
        }
        d
    }

    fn column_def(&self, c: &ColumnInfo) -> String {
        let mut s = format!("{} {}", self.quote(&c.name), c.data_type);
        if let Some(i) = &c.identity {
            s.push(' ');
            s.push_str(i);
        }
        s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if let Some(d) = &c.default {
            s.push_str(" DEFAULT ");
            s.push_str(d);
        }
        s
    }

    fn add_column(&self, table: &str, c: &ColumnInfo) -> Vec<Stmt> {
        vec![Stmt::new(format!(
            "ALTER TABLE {table} ADD {};",
            self.column_def(c)
        ))]
    }

    fn drop_column(&self, table: &str, c: &ColumnInfo) -> Vec<Stmt> {
        let mut sql = String::new();
        if c.default.is_some() {
            sql.push_str(&self.drop_default(table, &c.name));
            sql.push('\n');
        }
        sql.push_str(&format!(
            "ALTER TABLE {table} DROP COLUMN {};",
            self.quote(&c.name)
        ));
        vec![Stmt::destructive(sql)]
    }

    fn alter_column(&self, table: &str, from: &ColumnInfo, to: &ColumnInfo) -> Vec<Stmt> {
        let col = self.quote(&to.name);
        let mut out = Vec::new();
        let ident = |c: &ColumnInfo| c.identity.as_deref().map(collapse_ws);
        if ident(from) != ident(to) {
            out.push(Stmt::note(&format!(
                "{}: IDENTITY cannot be added, removed or reseeded with ALTER COLUMN;\n\
                 rebuild the table (or use a new column) to get: {}",
                to.name,
                self.column_def(to)
            )));
        }
        let type_changed = collapse_ws(&from.data_type).to_lowercase()
            != collapse_ws(&to.data_type).to_lowercase();
        // A bound default blocks a type change: drop it and add it back after.
        let default_changed = type_changed
            || self.normalize_default(from.default.as_deref().unwrap_or(""))
                != self.normalize_default(to.default.as_deref().unwrap_or(""));
        if default_changed && from.default.is_some() {
            out.push(Stmt::new(self.drop_default(table, &to.name)));
        }
        if type_changed || from.nullable != to.nullable {
            out.push(Stmt::new(format!(
                "ALTER TABLE {table} ALTER COLUMN {col} {};",
                Self::type_null(to)
            )));
        }
        if default_changed && let Some(d) = &to.default {
            out.push(Stmt::new(format!(
                "ALTER TABLE {table} ADD DEFAULT {d} FOR {col};"
            )));
        }
        out
    }

    fn create_index(&self, schema: &str, table: &str, ix: &IndexDef, _n: &Normalizer<'_>) -> Stmt {
        let kind = match ix.method.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("clustered") => "CLUSTERED ",
            Some("nonclustered") => "NONCLUSTERED ",
            _ => "",
        };
        let cols = ix
            .columns
            .iter()
            .map(|c| self.index_column(c))
            .collect::<Vec<_>>()
            .join(", ");
        Stmt::new(format!(
            "CREATE {}{kind}INDEX {} ON {} ({cols});",
            if ix.unique { "UNIQUE " } else { "" },
            self.quote(&ix.name),
            self.qualified(schema, table)
        ))
    }

    fn drop_index(&self, schema: &str, table: &str, ix: &IndexDef) -> Stmt {
        Stmt::new(format!(
            "DROP INDEX {} ON {};",
            self.quote(&ix.name),
            self.qualified(schema, table)
        ))
    }

    fn create_view(
        &self,
        _kind: ObjectKind,
        _schema: &str,
        _name: &str,
        ddl: &str,
        _exists: bool,
    ) -> Vec<Stmt> {
        // SQL Server 2016 SP1 and later.
        vec![Stmt::new(semi(&replace_create(
            ddl.trim(),
            Flavor::TSql,
            "CREATE OR ALTER",
        )))]
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
        // A routine body may end without `;` and must not get one after `END` of some
        // statements; the batch separator ends it.
        vec![Stmt::new(
            replace_create(ddl.trim(), Flavor::TSql, "CREATE OR ALTER")
                .trim_end()
                .to_owned(),
        )]
    }
}
