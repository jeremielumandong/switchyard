//! Migration scripts: the statements that make the target schema match the source.
//!
//! Each engine implements [`SchemaScripter`]; [`migration_script`] orders the selected
//! objects' statements so dependencies hold (foreign keys dropped first and added last,
//! tables before views, views before routines) and comments out every statement that
//! loses data, under a warning header. The script is text for the SQL editor: nothing
//! here runs it.

use super::diff::{DiffStatus, Normalizer, ObjectDiff, Paired, SchemaDiff, pair};
use super::model::{CheckDef, FkDef, IndexDef, KeyDef, TableModel};
use super::text::{collapse_ws, semi};
use crate::catalog::{ColumnInfo, ObjectKind};
use crate::dialect::{Dialect, drop_keyword};

/// One statement of a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stmt {
    /// The statement with its terminator, or a `--` comment line.
    pub sql: String,
    /// Loses data (DROP TABLE, DROP COLUMN, a rebuild that leaves columns behind): written
    /// commented out under a warning.
    pub destructive: bool,
}

impl Stmt {
    /// A statement.
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: false,
        }
    }

    /// A statement that loses data.
    pub fn destructive(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: true,
        }
    }

    /// A `--` comment for the reader (something the script cannot do for them).
    pub fn note(text: &str) -> Self {
        Self::new(
            text.lines()
                .map(|l| format!("-- {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn is_comment(&self) -> bool {
        self.sql.lines().all(|l| l.trim_start().starts_with("--"))
    }
}

/// Statements of one table, by phase: `pre` runs before any table changes (dropping
/// foreign keys), `main` in table order, `post` after every table exists (adding
/// foreign keys).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableScript {
    /// Before any table change.
    pub pre: Vec<Stmt>,
    /// The table's own changes.
    pub main: Vec<Stmt>,
    /// After every table change.
    pub post: Vec<Stmt>,
}

/// Engine-specific DDL for schema compare. The provided methods write standard SQL;
/// engines override what they spell differently.
pub trait SchemaScripter: Send + Sync {
    /// The engine's dialect (quoting, script splitting).
    fn dialect(&self) -> &'static dyn Dialect;

    /// Batch separator written after every statement (`GO` for SQL Server, where
    /// `CREATE VIEW` must start a batch).
    fn batch_separator(&self) -> Option<&'static str> {
        None
    }

    /// Lines written before the statements (MySQL: `USE <target>`).
    fn preamble(&self, _target_schema: &str) -> Vec<String> {
        Vec::new()
    }

    /// Whether a table's catalog DDL is the stored `CREATE TABLE` text (SQLite), which
    /// then is compared and reused as is.
    fn stored_table_ddl(&self) -> bool {
        false
    }

    /// Whether a view keeps the tables it reads from being altered (PostgreSQL: "cannot
    /// alter type of a column used by a view"). The script then drops the views that
    /// read an altered table, and every changed view, before the table changes and
    /// creates them again after.
    fn views_block_alter(&self) -> bool {
        false
    }

    /// A definition (view, routine, stored table DDL) as it is compared.
    fn normalize_definition(&self, ddl: &str) -> String {
        collapse_ws(ddl)
    }

    /// A column default as it is compared.
    fn normalize_default(&self, default: &str) -> String {
        collapse_ws(default)
    }

    /// `schema.name`, quoted.
    fn qualified(&self, schema: &str, name: &str) -> String {
        self.dialect().qualified(schema, name)
    }

    /// Quoted identifier.
    fn quote(&self, ident: &str) -> String {
        self.dialect().quote_ident(ident)
    }

    /// Quoted, comma-separated column list.
    fn column_list(&self, cols: &[String]) -> String {
        cols.iter()
            .map(|c| self.quote(c))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A column definition: `name type [DEFAULT …] [NOT NULL]`.
    fn column_def(&self, c: &ColumnInfo) -> String {
        let mut s = format!("{} {}", self.quote(&c.name), c.data_type);
        if let Some(i) = &c.identity {
            s.push(' ');
            s.push_str(i);
        }
        if let Some(d) = &c.default {
            s.push_str(" DEFAULT ");
            s.push_str(d);
        }
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        s
    }

    /// `CONSTRAINT name ` (or nothing for an unnamed constraint).
    fn constraint_prefix(&self, name: &str) -> String {
        if name.is_empty() {
            String::new()
        } else {
            format!("CONSTRAINT {} ", self.quote(name))
        }
    }

    /// `FOREIGN KEY (…) REFERENCES … (…) [ON DELETE …] [ON UPDATE …]` in `schema`.
    fn fk_body(&self, schema: &str, fk: &FkDef) -> String {
        let rs = fk.ref_schema.as_deref().unwrap_or(schema);
        let mut s = format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            self.column_list(&fk.columns),
            self.qualified(rs, &fk.ref_table),
            self.column_list(&fk.ref_columns)
        );
        if let Some(a) = &fk.on_delete {
            s.push_str(&format!(" ON DELETE {a}"));
        }
        if let Some(a) = &fk.on_update {
            s.push_str(&format!(" ON UPDATE {a}"));
        }
        s
    }

    /// `CREATE TABLE` of `t` in `schema`, without foreign keys, then its indexes.
    fn create_table(&self, schema: &str, t: &TableModel, n: &Normalizer<'_>) -> Vec<Stmt> {
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| format!("    {}", self.column_def(c)))
            .collect();
        if let Some(pk) = &t.primary_key {
            lines.push(format!(
                "    {}PRIMARY KEY ({})",
                self.constraint_prefix(&pk.name),
                self.column_list(&pk.columns)
            ));
        }
        for u in &t.uniques {
            lines.push(format!(
                "    {}UNIQUE ({})",
                self.constraint_prefix(&u.name),
                self.column_list(&u.columns)
            ));
        }
        for c in &t.checks {
            lines.push(format!(
                "    {}CHECK {}",
                self.constraint_prefix(&c.name),
                n.retarget(&c.expr)
            ));
        }
        let mut out = vec![Stmt::new(format!(
            "CREATE TABLE {} (\n{}\n);",
            self.qualified(schema, &t.name),
            lines.join(",\n")
        ))];
        for ix in &t.indexes {
            out.push(self.create_index(schema, &t.name, ix, n));
        }
        out
    }

    /// `DROP TABLE`.
    fn drop_table(&self, schema: &str, name: &str) -> Stmt {
        Stmt::destructive(format!("DROP TABLE {};", self.qualified(schema, name)))
    }

    /// Add a column.
    fn add_column(&self, table: &str, c: &ColumnInfo) -> Vec<Stmt> {
        vec![Stmt::new(format!(
            "ALTER TABLE {table} ADD COLUMN {};",
            self.column_def(c)
        ))]
    }

    /// Drop a column (loses its data).
    fn drop_column(&self, table: &str, c: &ColumnInfo) -> Vec<Stmt> {
        vec![Stmt::destructive(format!(
            "ALTER TABLE {table} DROP COLUMN {};",
            self.quote(&c.name)
        ))]
    }

    /// Change column `from` (target) into `to` (source).
    fn alter_column(&self, table: &str, from: &ColumnInfo, to: &ColumnInfo) -> Vec<Stmt>;

    /// Add the primary key.
    fn add_primary_key(&self, table: &str, pk: &KeyDef) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} ADD {}PRIMARY KEY ({});",
            self.constraint_prefix(&pk.name),
            self.column_list(&pk.columns)
        ))
    }

    /// Drop the primary key.
    fn drop_primary_key(&self, table: &str, pk: &KeyDef) -> Stmt {
        self.drop_constraint(table, &pk.name)
    }

    /// `ALTER TABLE … DROP CONSTRAINT`.
    fn drop_constraint(&self, table: &str, name: &str) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} DROP CONSTRAINT {};",
            self.quote(name)
        ))
    }

    /// Add a unique constraint.
    fn add_unique(&self, table: &str, u: &KeyDef) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} ADD {}UNIQUE ({});",
            self.constraint_prefix(&u.name),
            self.column_list(&u.columns)
        ))
    }

    /// Drop a unique constraint.
    fn drop_unique(&self, table: &str, u: &KeyDef) -> Stmt {
        self.drop_constraint(table, &u.name)
    }

    /// Add a check constraint (`expr` already retargeted).
    fn add_check(&self, table: &str, c: &CheckDef, expr: &str) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} ADD {}CHECK {expr};",
            self.constraint_prefix(&c.name)
        ))
    }

    /// Drop a check constraint.
    fn drop_check(&self, table: &str, c: &CheckDef) -> Stmt {
        self.drop_constraint(table, &c.name)
    }

    /// Add a foreign key of a table in `schema`.
    fn add_foreign_key(&self, schema: &str, table: &str, fk: &FkDef) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} ADD {}{};",
            self.constraint_prefix(&fk.name),
            self.fk_body(schema, fk)
        ))
    }

    /// Drop a foreign key.
    fn drop_foreign_key(&self, table: &str, fk: &FkDef) -> Stmt {
        self.drop_constraint(table, &fk.name)
    }

    /// Create a (source) index on `table` in the target `schema`.
    fn create_index(&self, schema: &str, table: &str, ix: &IndexDef, n: &Normalizer<'_>) -> Stmt;

    /// Drop a (target) index of `table` in `schema`.
    fn drop_index(&self, schema: &str, table: &str, ix: &IndexDef) -> Stmt {
        let _ = table;
        Stmt::new(format!("DROP INDEX {};", self.qualified(schema, &ix.name)))
    }

    /// Statements for a table only in the source.
    fn new_table(&self, schema: &str, t: &TableModel, n: &Normalizer<'_>) -> TableScript {
        let q = self.qualified(schema, &t.name);
        TableScript {
            pre: Vec::new(),
            main: self.create_table(schema, t, n),
            post: t
                .foreign_keys
                .iter()
                .map(|fk| self.add_foreign_key(schema, &q, fk))
                .collect(),
        }
    }

    /// Statements that turn the target table `to` into the source table `from`.
    fn alter_table(
        &self,
        schema: &str,
        source: &TableModel,
        target: &TableModel,
        n: &Normalizer<'_>,
    ) -> TableScript {
        generic_alter(self, schema, source, target, n)
    }

    /// Create (source) view `ddl`, already retargeted; `exists` when the target has
    /// another version of it.
    fn create_view(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        ddl: &str,
        exists: bool,
    ) -> Vec<Stmt>;

    /// Drop a view or materialized view.
    fn drop_view(&self, kind: ObjectKind, schema: &str, name: &str) -> Stmt {
        Stmt::new(format!(
            "DROP {} {};",
            drop_keyword(kind),
            self.qualified(schema, name)
        ))
    }

    /// Create (source) routine `ddl`, already retargeted; `exists` when the target has
    /// another version of it.
    fn create_routine(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        signature: Option<&str>,
        ddl: &str,
        exists: bool,
    ) -> Vec<Stmt>;

    /// Drop a routine.
    fn drop_routine(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        signature: Option<&str>,
    ) -> Stmt {
        let _ = signature;
        Stmt::new(format!(
            "DROP {} {};",
            drop_keyword(kind),
            self.qualified(schema, name)
        ))
    }
}

/// The standard `ALTER TABLE` sequence: drop what changed or went away, change columns,
/// then add what is new; foreign keys in the `pre` / `post` phases.
pub fn generic_alter<S: SchemaScripter + ?Sized>(
    s: &S,
    schema: &str,
    a: &TableModel,
    b: &TableModel,
    n: &Normalizer<'_>,
) -> TableScript {
    let q = s.qualified(schema, &b.name);
    let mut out = TableScript::default();

    let fks: Paired = pair(
        &a.foreign_keys,
        &b.foreign_keys,
        |f| &f.name,
        |x, y| n.same_fk(x, y),
    );
    for (i, j) in &fks.both {
        if !n.same_fk(&a.foreign_keys[*i], &b.foreign_keys[*j]) {
            out.pre.push(s.drop_foreign_key(&q, &b.foreign_keys[*j]));
            out.post
                .push(s.add_foreign_key(schema, &q, &a.foreign_keys[*i]));
        }
    }
    for j in &fks.only_target {
        out.pre.push(s.drop_foreign_key(&q, &b.foreign_keys[*j]));
    }
    for i in &fks.only_source {
        out.post
            .push(s.add_foreign_key(schema, &q, &a.foreign_keys[*i]));
    }

    let mut adds: Vec<Stmt> = Vec::new();
    // Checks, uniques, indexes: drop the target's version, add the source's.
    let checks = pair(&a.checks, &b.checks, |c| &c.name, |x, y| n.same_check(x, y));
    for (i, j) in &checks.both {
        if !n.same_check(&a.checks[*i], &b.checks[*j]) {
            out.main.push(s.drop_check(&q, &b.checks[*j]));
            adds.push(s.add_check(&q, &a.checks[*i], &n.retarget(&a.checks[*i].expr)));
        }
    }
    for j in &checks.only_target {
        out.main.push(s.drop_check(&q, &b.checks[*j]));
    }
    for i in &checks.only_source {
        adds.push(s.add_check(&q, &a.checks[*i], &n.retarget(&a.checks[*i].expr)));
    }
    let uniques = pair(
        &a.uniques,
        &b.uniques,
        |u| &u.name,
        |x, y| x.columns == y.columns,
    );
    for (i, j) in &uniques.both {
        if a.uniques[*i].columns != b.uniques[*j].columns {
            out.main.push(s.drop_unique(&q, &b.uniques[*j]));
            adds.push(s.add_unique(&q, &a.uniques[*i]));
        }
    }
    for j in &uniques.only_target {
        out.main.push(s.drop_unique(&q, &b.uniques[*j]));
    }
    for i in &uniques.only_source {
        adds.push(s.add_unique(&q, &a.uniques[*i]));
    }
    let ixs = pair(
        &a.indexes,
        &b.indexes,
        |i| &i.name,
        Normalizer::same_index_shape,
    );
    for (i, j) in &ixs.both {
        let (x, y) = (&a.indexes[*i], &b.indexes[*j]);
        let same = if x.name == y.name {
            n.same_index(x, y)
        } else {
            Normalizer::same_index_shape(x, y)
        };
        if !same {
            out.main.push(s.drop_index(schema, &b.name, y));
            adds.push(s.create_index(schema, &a.name, x, n));
        }
    }
    for j in &ixs.only_target {
        out.main.push(s.drop_index(schema, &b.name, &b.indexes[*j]));
    }
    for i in &ixs.only_source {
        adds.push(s.create_index(schema, &a.name, &a.indexes[*i], n));
    }
    let pk_changed =
        a.primary_key.as_ref().map(|k| &k.columns) != b.primary_key.as_ref().map(|k| &k.columns);
    if pk_changed && let Some(pk) = &b.primary_key {
        out.main.push(s.drop_primary_key(&q, pk));
    }

    // Columns: add, change, then drop.
    for c in &a.columns {
        match b.column(&c.name) {
            None => out.main.extend(s.add_column(&q, c)),
            Some(t) if !n.column_changes(c, t).is_empty() => {
                out.main.extend(s.alter_column(&q, t, c))
            }
            Some(_) => {}
        }
    }
    for c in &b.columns {
        if a.column(&c.name).is_none() {
            out.main.extend(s.drop_column(&q, c));
        }
    }

    if pk_changed && let Some(pk) = &a.primary_key {
        out.main.push(s.add_primary_key(&q, pk));
    }
    out.main.extend(adds);
    out
}

/// A titled group of statements in the script.
type Block = (String, Vec<Stmt>);

/// What [`migration_script`] says about the two sides in its header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptHeader {
    /// Source description (`shop-prod · public`): connection names only.
    pub source: String,
    /// Target description.
    pub target: String,
    /// The target database, when it is not the connection's default: the script
    /// switches to it first (`USE`), or says to where the engine cannot.
    pub database: Option<String>,
}

/// DDL of one diffed object on both sides, as the side-by-side view shows it: tables
/// written by the scripter (so only real differences show), views and routines as the
/// catalog has them, the source retargeted to the target schema.
pub fn display_ddl(diff: &SchemaDiff, o: &ObjectDiff, s: &dyn SchemaScripter) -> (String, String) {
    let n = Normalizer {
        s,
        source_schema: &diff.source_schema,
        target_schema: &diff.target_schema,
    };
    let render = |d: &crate::catalog::ObjectDetail, source: bool| -> String {
        if o.kind == ObjectKind::Table {
            let t = TableModel::from_detail(d);
            let mut stmts = s.create_table(&diff.target_schema, &t, &n);
            let q = s.qualified(&diff.target_schema, &t.name);
            stmts.extend(
                t.foreign_keys
                    .iter()
                    .map(|fk| s.add_foreign_key(&diff.target_schema, &q, fk)),
            );
            let text = stmts
                .into_iter()
                .map(|st| st.sql)
                .collect::<Vec<_>>()
                .join("\n");
            if source { n.retarget(&text) } else { text }
        } else if source {
            n.retarget(&d.ddl)
        } else {
            d.ddl.clone()
        }
    };
    (
        o.source
            .as_ref()
            .map(|d| render(d, true))
            .unwrap_or_default(),
        o.target
            .as_ref()
            .map(|d| render(d, false))
            .unwrap_or_default(),
    )
}

/// The migration script for the objects of `diff` that `include` selects: it makes the
/// target match the source. Statements that lose data are commented out, with a warning
/// at the top saying how many there are.
pub fn migration_script(
    diff: &SchemaDiff,
    include: &dyn Fn(usize) -> bool,
    s: &dyn SchemaScripter,
    header: &ScriptHeader,
) -> String {
    let n = Normalizer {
        s,
        source_schema: &diff.source_schema,
        target_schema: &diff.target_schema,
    };
    let schema = diff.target_schema.as_str();
    let mut fk_drops: Vec<Block> = Vec::new();
    let mut drops: Vec<Block> = Vec::new();
    let mut tables: Vec<Block> = Vec::new();
    let mut fk_adds: Vec<Block> = Vec::new();
    let mut views: Vec<Block> = Vec::new();
    let mut routines: Vec<Block> = Vec::new();

    let selected: Vec<&ObjectDiff> = diff
        .objects
        .iter()
        .enumerate()
        .filter(|(i, _)| include(*i))
        .map(|(_, o)| o)
        .collect();

    // Views to drop before the tables change and create again after (engines where a
    // view blocks ALTER TABLE): every selected changed view, and unchanged views that
    // read a table the script alters.
    let mut view_drops: Vec<Block> = Vec::new();
    let mut rebuilt: Vec<Block> = Vec::new();
    let mut dropped_first: Vec<(ObjectKind, String)> = Vec::new();
    if s.views_block_alter() {
        let altered: Vec<&str> = selected
            .iter()
            .filter(|o| o.kind == ObjectKind::Table && o.status == DiffStatus::Different)
            .map(|o| o.name.as_str())
            .collect();
        for o in &selected {
            if matches!(o.kind, ObjectKind::View | ObjectKind::MaterializedView)
                && o.status == DiffStatus::Different
            {
                dropped_first.push((o.kind, o.name.clone()));
            }
        }
        for v in &diff.unchanged_views {
            if altered.iter().any(|t| mentions(&v.ddl, t)) {
                let (kind, name) = (v.object.kind, v.object.name.clone());
                view_drops.push((
                    format!("{} {name}: reads a table that changes", kind_label(kind)),
                    vec![s.drop_view(kind, schema, &name)],
                ));
                rebuilt.push((
                    format!("{} {name}: unchanged, created again", kind_label(kind)),
                    s.create_view(kind, schema, &name, &n.retarget(&v.ddl), false),
                ));
            }
        }
        for (kind, name) in &dropped_first {
            view_drops.push((
                format!(
                    "{} {name}: different, created again below",
                    kind_label(*kind)
                ),
                vec![s.drop_view(*kind, schema, name)],
            ));
        }
    }

    for o in &selected {
        let title = format!(
            "{} {}: {}",
            kind_label(o.kind),
            o.label(),
            o.status.label().to_lowercase()
        );
        let routine = matches!(o.kind, ObjectKind::Function | ObjectKind::Procedure);
        let view = matches!(o.kind, ObjectKind::View | ObjectKind::MaterializedView);
        match (o.status, &o.source, &o.target) {
            (DiffStatus::OnlyInTarget, _, Some(_)) => {
                let st = if o.kind == ObjectKind::Table {
                    s.drop_table(schema, &o.name)
                } else if view {
                    s.drop_view(o.kind, schema, &o.name)
                } else if routine {
                    s.drop_routine(o.kind, schema, &o.name, o.signature.as_deref())
                } else {
                    continue;
                };
                drops.push((title, vec![st]));
            }
            (DiffStatus::OnlyInSource | DiffStatus::Different, Some(src), tgt) => {
                let exists = tgt.is_some();
                if o.kind == ObjectKind::Table {
                    let a = TableModel::from_detail(src);
                    let ts = match tgt {
                        Some(t) => s.alter_table(schema, &a, &TableModel::from_detail(t), &n),
                        None => s.new_table(schema, &a, &n),
                    };
                    if !ts.pre.is_empty() {
                        fk_drops.push((title.clone(), ts.pre));
                    }
                    if !ts.main.is_empty() {
                        tables.push((title.clone(), ts.main));
                    }
                    if !ts.post.is_empty() {
                        fk_adds.push((title, ts.post));
                    }
                } else if view {
                    let ddl = n.retarget(&src.ddl);
                    let exists = exists && !dropped_first.contains(&(o.kind, o.name.clone()));
                    views.push((title, s.create_view(o.kind, schema, &o.name, &ddl, exists)));
                } else if routine {
                    let ddl = n.retarget(&src.ddl);
                    routines.push((
                        title,
                        s.create_routine(
                            o.kind,
                            schema,
                            &o.name,
                            o.signature.as_deref(),
                            &ddl,
                            exists,
                        ),
                    ));
                }
            }
            _ => {}
        }
    }

    views.extend(rebuilt);
    let sections: [(&str, Vec<Block>); 7] = [
        ("Drop foreign keys that change or go away", fk_drops),
        ("Drop views that are rebuilt", view_drops),
        ("Drop objects only in the target", drops),
        ("Tables", tables),
        ("Foreign keys", fk_adds),
        ("Views", views),
        ("Routines", routines),
    ];
    let destructive = sections
        .iter()
        .flat_map(|(_, b)| b.iter())
        .flat_map(|(_, st)| st.iter())
        .filter(|st| st.destructive)
        .count();

    let mut out = String::new();
    out.push_str("-- Schema migration generated by Switchyard\n");
    out.push_str(&format!("-- Source: {}\n", header.source));
    out.push_str(&format!("-- Target: {}\n", header.target));
    out.push_str("-- Running it makes the target match the source. Nothing has been run:\n");
    out.push_str("-- review it, then run it yourself.\n");
    if destructive > 0 {
        out.push_str("--\n");
        out.push_str(&format!(
            "-- WARNING: {destructive} destructive statement{} (DROP TABLE / DROP COLUMN or a\n",
            if destructive == 1 { "" } else { "s" }
        ));
        out.push_str(
            "-- table rebuild that leaves columns behind) lose data. They are commented\n",
        );
        out.push_str("-- out below; uncomment each one only after checking its data can go.\n");
    }
    out.push('\n');
    let mut pre = Vec::new();
    if let Some(db) = &header.database {
        match s.dialect().use_database(db) {
            Some(sql) => {
                pre.push(semi(&sql));
                if let Some(sep) = s.batch_separator() {
                    pre.push(sep.to_owned());
                }
            }
            None => pre.push(format!(
                "-- Run this script on database {db} (switch the tab's database first)."
            )),
        }
    }
    pre.extend(s.preamble(schema));
    if !pre.is_empty() {
        for l in pre {
            out.push_str(&l);
            out.push('\n');
        }
        out.push('\n');
    }
    if selected.is_empty() {
        out.push_str("-- Nothing selected.\n");
        return out;
    }
    for (section, blocks) in sections {
        if blocks.is_empty() {
            continue;
        }
        out.push_str(&format!("-- ===== {section} =====\n\n"));
        for (title, stmts) in blocks {
            out.push_str(&format!("-- {title}\n"));
            for st in stmts {
                write_stmt(&mut out, &st, s.batch_separator());
            }
            out.push('\n');
        }
    }
    out
}

/// Whether `ddl` names `table` as a whole word (quoted or not), ignoring case.
fn mentions(ddl: &str, table: &str) -> bool {
    let hay = ddl.to_lowercase();
    let needle = table.to_lowercase();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    hay.match_indices(&needle).any(|(at, _)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

fn write_stmt(out: &mut String, st: &Stmt, sep: Option<&str>) {
    if st.destructive {
        out.push_str("-- WARNING: destructive (loses data). Uncomment to run:\n");
        for l in st.sql.lines() {
            out.push_str("-- ");
            out.push_str(l);
            out.push('\n');
        }
        if let Some(sep) = sep {
            out.push_str(&format!("-- {sep}\n"));
        }
        return;
    }
    out.push_str(st.sql.trim_end());
    out.push('\n');
    if let Some(sep) = sep
        && !st.is_comment()
    {
        out.push_str(sep);
        out.push('\n');
    }
}

/// `Table`, `View`, … for block titles.
fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "Table",
        ObjectKind::View => "View",
        ObjectKind::MaterializedView => "Materialized view",
        ObjectKind::Function => "Function",
        ObjectKind::Procedure => "Procedure",
        _ => "Object",
    }
}

/// `ddl` with a terminator, for statements taken from the catalog.
pub(crate) fn catalog_stmt(ddl: &str) -> Stmt {
    Stmt::new(semi(ddl))
}
