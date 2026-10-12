//! The diff model: which objects are only in the source, only in the target, or
//! different, and what differs in each.
//!
//! Objects are matched by kind and name (routines also by signature). Inside a table,
//! constraints and indexes are matched by name first and then by what they do, since
//! servers generate different names for unnamed ones (`PK__orders__3213E83F…`).
//! Definitions taken from the source are retargeted to the target schema before they
//! are compared, so two schemas on one server compare equal when only the schema name
//! inside their DDL differs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::SchemaSnapshot;
use super::model::{CheckDef, FkDef, IndexDef, KeyDef, TableModel};
use super::script::SchemaScripter;
use super::text::{collapse_ws, first_statement, retarget};
use crate::catalog::{ColumnInfo, ObjectDetail, ObjectKind};
use crate::value::Engine;

/// Where an object differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DiffStatus {
    /// In the source only: the script creates it.
    OnlyInSource,
    /// In the target only: the script drops it.
    OnlyInTarget,
    /// In both, with differences: the script alters or replaces it.
    Different,
}

impl DiffStatus {
    /// Group heading in the result list.
    pub fn label(self) -> &'static str {
        match self {
            DiffStatus::OnlyInSource => "Only in source",
            DiffStatus::OnlyInTarget => "Only in target",
            DiffStatus::Different => "Different",
        }
    }
}

/// What part of an object a [`Change`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Part {
    /// A column.
    Column,
    /// The primary key.
    PrimaryKey,
    /// A unique constraint.
    Unique,
    /// A check constraint.
    Check,
    /// A foreign key.
    ForeignKey,
    /// An index.
    Index,
    /// The whole definition (views, routines, SQLite tables).
    Definition,
}

impl Part {
    /// Lower-case label.
    pub fn label(self) -> &'static str {
        match self {
            Part::Column => "column",
            Part::PrimaryKey => "primary key",
            Part::Unique => "unique constraint",
            Part::Check => "check constraint",
            Part::ForeignKey => "foreign key",
            Part::Index => "index",
            Part::Definition => "definition",
        }
    }
}

/// Added, dropped or changed (seen from the target: "added" means the script adds it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    /// In the source only.
    Added,
    /// In the target only.
    Dropped,
    /// In both, different.
    Changed,
}

/// One difference inside an object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Added, dropped or changed.
    pub kind: ChangeKind,
    /// What part.
    pub part: Part,
    /// Its name (column, constraint, index); empty for the definition.
    pub name: String,
    /// What changed (`type integer → bigint`), for changed parts.
    pub detail: String,
}

impl Change {
    /// One line for the result view: `+ column email`, `~ column qty: type int → bigint`.
    pub fn label(&self) -> String {
        let sign = match self.kind {
            ChangeKind::Added => "+",
            ChangeKind::Dropped => "−",
            ChangeKind::Changed => "~",
        };
        let mut s = format!("{sign} {}", self.part.label());
        if !self.name.is_empty() {
            s.push(' ');
            s.push_str(&self.name);
        }
        if !self.detail.is_empty() {
            s.push_str(": ");
            s.push_str(&self.detail);
        }
        s
    }
}

/// One object that differs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectDiff {
    /// Kind.
    pub kind: ObjectKind,
    /// Name.
    pub name: String,
    /// Routine signature (`(id integer)`), when the engine lists one.
    pub signature: Option<String>,
    /// Where it differs.
    pub status: DiffStatus,
    /// The differences (empty unless [`DiffStatus::Different`]).
    pub changes: Vec<Change>,
    /// Source detail.
    pub source: Option<ObjectDetail>,
    /// Target detail.
    pub target: Option<ObjectDetail>,
}

impl ObjectDiff {
    /// `name` or `name(signature)`.
    pub fn label(&self) -> String {
        match &self.signature {
            Some(s) if !s.is_empty() => format!("{}{s}", self.name),
            _ => self.name.clone(),
        }
    }

    /// Whether applying it loses data: dropping a table or one of its columns.
    pub fn is_destructive(&self) -> bool {
        match self.status {
            DiffStatus::OnlyInTarget => self.kind == ObjectKind::Table,
            DiffStatus::Different => self
                .changes
                .iter()
                .any(|c| c.part == Part::Column && c.kind == ChangeKind::Dropped),
            DiffStatus::OnlyInSource => false,
        }
    }
}

/// The result of comparing two schemas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaDiff {
    /// Engine of both sides.
    pub engine: Engine,
    /// Source schema.
    pub source_schema: String,
    /// Target schema.
    pub target_schema: String,
    /// Differing objects: grouped by status, then by kind and name.
    pub objects: Vec<ObjectDiff>,
    /// Objects equal on both sides.
    pub identical: usize,
    /// Views equal on both sides (source detail): rebuilt by the script when they
    /// depend on a table it alters and the engine refuses that
    /// ([`SchemaScripter::views_block_alter`]).
    pub unchanged_views: Vec<ObjectDetail>,
}

/// Compares parts of the two sides with the source's schema name retargeted.
pub struct Normalizer<'a> {
    /// The engine's scripter (definition and default normalization).
    pub s: &'a dyn SchemaScripter,
    /// Source schema.
    pub source_schema: &'a str,
    /// Target schema.
    pub target_schema: &'a str,
}

impl Normalizer<'_> {
    /// Source text as it would read in the target schema.
    pub fn retarget(&self, text: &str) -> String {
        let d = self.s.dialect();
        retarget(text, d.flavor(), d, self.source_schema, self.target_schema)
    }

    fn src(&self, text: &str) -> String {
        self.s.normalize_definition(&self.retarget(text))
    }

    fn tgt(&self, text: &str) -> String {
        self.s.normalize_definition(text)
    }

    fn src_default(&self, d: &Option<String>) -> Option<String> {
        d.as_deref()
            .map(|d| self.s.normalize_default(&self.retarget(d)))
    }

    fn tgt_default(&self, d: &Option<String>) -> Option<String> {
        d.as_deref().map(|d| self.s.normalize_default(d))
    }

    /// What differs between a source and a target column (empty when equal).
    pub fn column_changes(&self, a: &ColumnInfo, b: &ColumnInfo) -> Vec<String> {
        let mut out = Vec::new();
        let ty = |t: &str| collapse_ws(t).to_ascii_lowercase();
        if ty(&a.data_type) != ty(&b.data_type) {
            out.push(format!("type {} → {}", b.data_type, a.data_type));
        }
        if a.nullable != b.nullable {
            let n = |x: bool| if x { "NULL" } else { "NOT NULL" };
            out.push(format!("{} → {}", n(b.nullable), n(a.nullable)));
        }
        if self.src_default(&a.default) != self.tgt_default(&b.default) {
            let show = |d: &Option<String>| d.clone().unwrap_or_else(|| "none".into());
            out.push(format!(
                "default {} → {}",
                show(&b.default),
                show(&a.default)
            ));
        }
        let ident = |i: &Option<String>| i.as_deref().map(|s| collapse_ws(s).to_ascii_uppercase());
        if ident(&a.identity) != ident(&b.identity) {
            let show = |d: &Option<String>| d.clone().unwrap_or_else(|| "none".into());
            out.push(format!(
                "identity {} → {}",
                show(&b.identity),
                show(&a.identity)
            ));
        }
        out
    }

    /// Same check condition.
    pub fn same_check(&self, a: &CheckDef, b: &CheckDef) -> bool {
        self.src(&a.expr) == self.tgt(&b.expr)
    }

    /// Same foreign key (whatever its name).
    pub fn same_fk(&self, a: &FkDef, b: &FkDef) -> bool {
        // A reference to the source schema itself reads as one to the target's.
        let rs = |s: &Option<String>, own: &str| s.clone().filter(|s| s != own);
        a.columns == b.columns
            && rs(&a.ref_schema, self.source_schema) == rs(&b.ref_schema, self.target_schema)
            && a.ref_table == b.ref_table
            && a.ref_columns == b.ref_columns
            && a.on_delete == b.on_delete
            && a.on_update == b.on_update
    }

    /// Same index shape (whatever its name).
    pub fn same_index_shape(a: &IndexDef, b: &IndexDef) -> bool {
        let m = |m: &Option<String>| m.as_deref().map(str::to_ascii_lowercase);
        a.columns == b.columns && a.unique == b.unique && m(&a.method) == m(&b.method)
    }

    /// Same index, definition included (for two indexes with one name).
    pub fn same_index(&self, a: &IndexDef, b: &IndexDef) -> bool {
        Self::same_index_shape(a, b) && self.src(&a.definition) == self.tgt(&b.definition)
    }
}

/// Matching of two lists: pairs (source index, target index) and the leftovers.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Paired {
    /// Matched by name or by shape.
    pub both: Vec<(usize, usize)>,
    /// Source only.
    pub only_source: Vec<usize>,
    /// Target only.
    pub only_target: Vec<usize>,
}

/// Pair `src` with `tgt` by name, then the rest by `same`.
pub(crate) fn pair<T>(
    src: &[T],
    tgt: &[T],
    name: impl Fn(&T) -> &str,
    same: impl Fn(&T, &T) -> bool,
) -> Paired {
    let mut used = vec![false; tgt.len()];
    let mut out = Paired::default();
    let mut unmatched = Vec::new();
    for (i, a) in src.iter().enumerate() {
        let n = name(a);
        match tgt
            .iter()
            .enumerate()
            .find(|(j, b)| !used[*j] && !n.is_empty() && name(b) == n)
        {
            Some((j, _)) => {
                used[j] = true;
                out.both.push((i, j));
            }
            None => unmatched.push(i),
        }
    }
    for i in unmatched {
        match tgt
            .iter()
            .enumerate()
            .find(|(j, b)| !used[*j] && same(&src[i], b))
        {
            Some((j, _)) => {
                used[j] = true;
                out.both.push((i, j));
            }
            None => out.only_source.push(i),
        }
    }
    out.only_target = (0..tgt.len()).filter(|j| !used[*j]).collect();
    out
}

fn change(kind: ChangeKind, part: Part, name: &str, detail: String) -> Change {
    Change {
        kind,
        part,
        name: name.to_owned(),
        detail,
    }
}

/// Added / dropped / changed entries of one list.
fn list_changes<T>(
    out: &mut Vec<Change>,
    part: Part,
    src: &[T],
    tgt: &[T],
    name: impl Fn(&T) -> &str + Copy,
    same: impl Fn(&T, &T) -> bool + Copy,
    describe: impl Fn(&T) -> String,
) {
    let p = pair(src, tgt, name, same);
    for (i, j) in &p.both {
        if !same(&src[*i], &tgt[*j]) {
            out.push(change(
                ChangeKind::Changed,
                part,
                name(&src[*i]),
                format!("{} → {}", describe(&tgt[*j]), describe(&src[*i])),
            ));
        }
    }
    for i in p.only_source {
        out.push(change(
            ChangeKind::Added,
            part,
            name(&src[i]),
            String::new(),
        ));
    }
    for j in p.only_target {
        out.push(change(
            ChangeKind::Dropped,
            part,
            name(&tgt[j]),
            String::new(),
        ));
    }
}

/// The differences between two versions of one table.
pub(crate) fn table_changes(n: &Normalizer<'_>, a: &TableModel, b: &TableModel) -> Vec<Change> {
    let mut out = Vec::new();
    for c in &a.columns {
        match b.column(&c.name) {
            None => out.push(change(
                ChangeKind::Added,
                Part::Column,
                &c.name,
                String::new(),
            )),
            Some(t) => {
                let what = n.column_changes(c, t);
                if !what.is_empty() {
                    out.push(change(
                        ChangeKind::Changed,
                        Part::Column,
                        &c.name,
                        what.join(", "),
                    ));
                }
            }
        }
    }
    for c in &b.columns {
        if a.column(&c.name).is_none() {
            out.push(change(
                ChangeKind::Dropped,
                Part::Column,
                &c.name,
                String::new(),
            ));
        }
    }
    let cols = |k: &KeyDef| format!("({})", k.columns.join(", "));
    match (&a.primary_key, &b.primary_key) {
        (Some(x), Some(y)) if x.columns != y.columns => out.push(change(
            ChangeKind::Changed,
            Part::PrimaryKey,
            &x.name,
            format!("{} → {}", cols(y), cols(x)),
        )),
        (Some(x), None) => out.push(change(
            ChangeKind::Added,
            Part::PrimaryKey,
            &x.name,
            cols(x),
        )),
        (None, Some(y)) => out.push(change(
            ChangeKind::Dropped,
            Part::PrimaryKey,
            &y.name,
            cols(y),
        )),
        _ => {}
    }
    list_changes(
        &mut out,
        Part::Unique,
        &a.uniques,
        &b.uniques,
        |k| k.name.as_str(),
        |x, y| x.columns == y.columns,
        cols,
    );
    list_changes(
        &mut out,
        Part::Check,
        &a.checks,
        &b.checks,
        |c| c.name.as_str(),
        |x, y| n.same_check(x, y),
        |c| c.expr.clone(),
    );
    list_changes(
        &mut out,
        Part::ForeignKey,
        &a.foreign_keys,
        &b.foreign_keys,
        |f| f.name.as_str(),
        |x, y| n.same_fk(x, y),
        |f| {
            let mut s = format!(
                "({}) → {}({})",
                f.columns.join(", "),
                f.ref_table,
                f.ref_columns.join(", ")
            );
            if let Some(a) = &f.on_delete {
                s.push_str(&format!(" ON DELETE {a}"));
            }
            if let Some(a) = &f.on_update {
                s.push_str(&format!(" ON UPDATE {a}"));
            }
            format!("[{s}]")
        },
    );
    // Indexes: matched by name, then by shape; one name with another definition
    // (a WHERE clause, INCLUDE columns) is a change.
    let p = pair(
        &a.indexes,
        &b.indexes,
        |i| i.name.as_str(),
        Normalizer::same_index_shape,
    );
    for (i, j) in &p.both {
        let (x, y) = (&a.indexes[*i], &b.indexes[*j]);
        let same = if x.name == y.name {
            n.same_index(x, y)
        } else {
            Normalizer::same_index_shape(x, y)
        };
        if !same {
            out.push(change(
                ChangeKind::Changed,
                Part::Index,
                &x.name,
                format!("({}) → ({})", y.columns.join(", "), x.columns.join(", ")),
            ));
        }
    }
    for i in p.only_source {
        out.push(change(
            ChangeKind::Added,
            Part::Index,
            &a.indexes[i].name,
            String::new(),
        ));
    }
    for j in p.only_target {
        out.push(change(
            ChangeKind::Dropped,
            Part::Index,
            &b.indexes[j].name,
            String::new(),
        ));
    }
    // Engines whose table DDL is stored text (SQLite) also differ by what the parts
    // above do not cover (CHECKs, collations, AUTOINCREMENT).
    if out.is_empty() && n.s.stored_table_ddl() {
        let d = n.s.dialect();
        let x = n.src(&first_statement(&a.ddl, d));
        let y = n.tgt(&first_statement(&b.ddl, d));
        if x != y {
            out.push(change(
                ChangeKind::Changed,
                Part::Definition,
                "",
                "CREATE TABLE text differs".into(),
            ));
        }
    }
    out
}

/// Key of an object across the two sides.
fn key(d: &ObjectDetail) -> (ObjectKind, String, Option<String>) {
    let sig = match d.object.kind {
        ObjectKind::Function | ObjectKind::Procedure => d.object.detail.clone(),
        _ => None,
    };
    (d.object.kind, d.object.name.clone(), sig)
}

/// Compare `source` with `target`, using `s` (the engine's scripter) to normalize
/// definitions.
pub fn compare(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    s: &dyn SchemaScripter,
) -> SchemaDiff {
    let n = Normalizer {
        s,
        source_schema: &source.schema,
        target_schema: &target.schema,
    };
    let targets: HashMap<_, &ObjectDetail> = target.objects.iter().map(|d| (key(d), d)).collect();
    let mut objects = Vec::new();
    let mut identical = 0;
    let mut unchanged_views = Vec::new();
    for a in &source.objects {
        let k = key(a);
        let Some(b) = targets.get(&k) else {
            objects.push(ObjectDiff {
                kind: k.0,
                name: k.1,
                signature: k.2,
                status: DiffStatus::OnlyInSource,
                changes: Vec::new(),
                source: Some(a.clone()),
                target: None,
            });
            continue;
        };
        let changes = if k.0 == ObjectKind::Table {
            table_changes(&n, &TableModel::from_detail(a), &TableModel::from_detail(b))
        } else if n.src(&a.ddl) != n.tgt(&b.ddl) {
            vec![change(
                ChangeKind::Changed,
                Part::Definition,
                "",
                String::new(),
            )]
        } else {
            Vec::new()
        };
        if changes.is_empty() {
            identical += 1;
            if matches!(k.0, ObjectKind::View | ObjectKind::MaterializedView) {
                unchanged_views.push(a.clone());
            }
            continue;
        }
        objects.push(ObjectDiff {
            kind: k.0,
            name: k.1,
            signature: k.2,
            status: DiffStatus::Different,
            changes,
            source: Some(a.clone()),
            target: Some((*b).clone()),
        });
    }
    let sources: std::collections::HashSet<_> = source.objects.iter().map(key).collect();
    for b in &target.objects {
        let k = key(b);
        if !sources.contains(&k) {
            objects.push(ObjectDiff {
                kind: k.0,
                name: k.1,
                signature: k.2,
                status: DiffStatus::OnlyInTarget,
                changes: Vec::new(),
                source: None,
                target: Some(b.clone()),
            });
        }
    }
    objects.sort_by(|x, y| {
        (x.status, x.kind, &x.name, &x.signature).cmp(&(y.status, y.kind, &y.name, &y.signature))
    });
    SchemaDiff {
        engine: source.engine,
        source_schema: source.schema.clone(),
        target_schema: target.schema.clone(),
        objects,
        identical,
        unchanged_views,
    }
}
