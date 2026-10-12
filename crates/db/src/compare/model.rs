//! A table normalized for comparison: the catalog's [`ObjectDetail`] lists overlap per
//! engine (PostgreSQL and MySQL list a unique constraint both as a constraint and as an
//! index, MySQL lists foreign keys as constraints), so they are sorted into one key,
//! unique constraints, checks, foreign keys and plain indexes here.

use serde::{Deserialize, Serialize};

use crate::catalog::{ColumnInfo, ObjectDetail};

/// A primary key or unique constraint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyDef {
    /// Constraint name (server-generated names differ between databases).
    pub name: String,
    /// Columns in key order.
    pub columns: Vec<String>,
}

/// A check constraint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDef {
    /// Constraint name.
    pub name: String,
    /// The condition, parenthesized (`(qty > 0)`), without the `CHECK` keyword.
    pub expr: String,
}

/// A foreign key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FkDef {
    /// Constraint name.
    pub name: String,
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced schema, or `None` when it is the table's own schema (so two schemas
    /// compare equal when each references its own copy).
    pub ref_schema: Option<String>,
    /// Referenced table.
    pub ref_table: String,
    /// Referenced columns.
    pub ref_columns: Vec<String>,
    /// `ON DELETE` action; `None` for the default (`NO ACTION`).
    pub on_delete: Option<String>,
    /// `ON UPDATE` action; `None` for the default.
    pub on_update: Option<String>,
}

/// A plain index (not backing a primary key or constraint).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    /// Index name.
    pub name: String,
    /// Key columns or expressions as the catalog lists them.
    pub columns: Vec<String>,
    /// Unique index.
    pub unique: bool,
    /// Access method or type (`btree`, `nonclustered`, `FULLTEXT`).
    pub method: Option<String>,
    /// The catalog's definition (`CREATE INDEX …` for PostgreSQL and SQLite).
    pub definition: String,
}

/// One table, normalized.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TableModel {
    /// Schema the table was read from.
    pub schema: String,
    /// Table name.
    pub name: String,
    /// Columns in order.
    pub columns: Vec<ColumnInfo>,
    /// Primary key.
    pub primary_key: Option<KeyDef>,
    /// Unique constraints.
    pub uniques: Vec<KeyDef>,
    /// Check constraints.
    pub checks: Vec<CheckDef>,
    /// Foreign keys.
    pub foreign_keys: Vec<FkDef>,
    /// Plain indexes.
    pub indexes: Vec<IndexDef>,
    /// The catalog's DDL (SQLite: the stored `CREATE TABLE` text, then indexes).
    pub ddl: String,
}

/// `NO ACTION` is the default referential action everywhere: compare it as absent.
fn action(a: &Option<String>) -> Option<String> {
    a.as_deref()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty() && s != "NO ACTION")
}

/// `CHECK (x > 0)` or `(x > 0)` → `(x > 0)`.
fn check_expr(definition: &str) -> String {
    let d = definition.trim();
    let d = if d.len() >= 5 && d[..5].eq_ignore_ascii_case("CHECK") {
        d[5..].trim_start()
    } else {
        d
    };
    let d = d.trim_end_matches(" NOT VALID").trim();
    if d.starts_with('(') && d.ends_with(')') {
        d.to_owned()
    } else {
        format!("({d})")
    }
}

impl TableModel {
    /// Normalize one table's catalog detail.
    pub fn from_detail(d: &ObjectDetail) -> Self {
        let schema = d.object.schema.clone();
        let mut columns = d.columns.clone();
        columns.sort_by_key(|c| c.ordinal);

        let pk_index = d.indexes.iter().find(|i| i.is_primary);
        let primary_key = match pk_index {
            Some(i) => Some(KeyDef {
                name: i.name.clone(),
                columns: i.columns.clone(),
            }),
            None => {
                let cols: Vec<String> = columns
                    .iter()
                    .filter(|c| c.is_primary_key)
                    .map(|c| c.name.clone())
                    .collect();
                let name = d
                    .constraints
                    .iter()
                    .find(|c| c.kind == "PRIMARY KEY")
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                (!cols.is_empty()).then_some(KeyDef {
                    name,
                    columns: cols,
                })
            }
        };

        let uniques: Vec<KeyDef> = d
            .constraints
            .iter()
            .filter(|c| c.kind == "UNIQUE")
            .map(|c| KeyDef {
                name: c.name.clone(),
                columns: d
                    .indexes
                    .iter()
                    .find(|i| i.name == c.name)
                    .map(|i| i.columns.clone())
                    .unwrap_or_default(),
            })
            .collect();

        let checks = d
            .constraints
            .iter()
            .filter(|c| c.kind == "CHECK")
            .map(|c| CheckDef {
                name: c.name.clone(),
                expr: check_expr(&c.definition),
            })
            .collect();

        let foreign_keys = d
            .foreign_keys
            .iter()
            .map(|f| {
                let (rs, rt) = match f.references.split_once('.') {
                    Some((s, t)) => (s.to_owned(), t.to_owned()),
                    None => (String::new(), f.references.clone()),
                };
                FkDef {
                    name: f.name.clone(),
                    columns: f.columns.clone(),
                    ref_schema: (!rs.is_empty() && rs != schema).then_some(rs),
                    ref_table: rt,
                    ref_columns: f.referenced_columns.clone(),
                    on_delete: action(&f.on_delete),
                    on_update: action(&f.on_update),
                }
            })
            .collect();

        // Indexes behind a key or constraint are scripted with it, and SQLite's
        // automatic ones come with the table.
        let owned = |name: &str| {
            d.constraints.iter().any(|c| c.name == name)
                || d.foreign_keys.iter().any(|f| f.name == name)
        };
        let indexes = d
            .indexes
            .iter()
            .filter(|i| !i.is_primary && !owned(&i.name))
            .filter(|i| !i.definition.starts_with("automatic index"))
            .map(|i| IndexDef {
                name: i.name.clone(),
                columns: i.columns.clone(),
                unique: i.is_unique,
                method: i.method.clone(),
                definition: i.definition.clone(),
            })
            .collect();

        TableModel {
            schema,
            name: d.object.name.clone(),
            columns,
            primary_key,
            uniques,
            checks,
            foreign_keys,
            indexes,
            ddl: d.ddl.clone(),
        }
    }

    /// The column called `name`.
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ConstraintInfo, ForeignKeyInfo, IndexInfo, ObjectInfo};

    #[test]
    fn overlapping_catalog_lists_are_sorted_out() {
        let d = ObjectDetail {
            object: ObjectInfo {
                schema: "app".into(),
                name: "orders".into(),
                ..ObjectInfo::default()
            },
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    ordinal: 1,
                    is_primary_key: true,
                    ..ColumnInfo::default()
                },
                ColumnInfo {
                    name: "code".into(),
                    ordinal: 2,
                    ..ColumnInfo::default()
                },
            ],
            indexes: vec![
                IndexInfo {
                    name: "orders_pkey".into(),
                    columns: vec!["id".into()],
                    is_unique: true,
                    is_primary: true,
                    ..IndexInfo::default()
                },
                IndexInfo {
                    name: "orders_code_key".into(),
                    columns: vec!["code".into()],
                    is_unique: true,
                    ..IndexInfo::default()
                },
                IndexInfo {
                    name: "orders_customer_fk".into(),
                    columns: vec!["customer_id".into()],
                    ..IndexInfo::default()
                },
                IndexInfo {
                    name: "orders_code_idx".into(),
                    columns: vec!["code".into()],
                    ..IndexInfo::default()
                },
            ],
            constraints: vec![
                ConstraintInfo {
                    name: "orders_code_key".into(),
                    kind: "UNIQUE".into(),
                    definition: "UNIQUE (code)".into(),
                },
                ConstraintInfo {
                    name: "orders_code_check".into(),
                    kind: "CHECK".into(),
                    definition: "CHECK (code <> '')".into(),
                },
            ],
            foreign_keys: vec![
                ForeignKeyInfo {
                    name: "orders_customer_fk".into(),
                    columns: vec!["customer_id".into()],
                    references: "app.customers".into(),
                    referenced_columns: vec!["id".into()],
                    on_delete: Some("CASCADE".into()),
                    on_update: Some("NO ACTION".into()),
                },
                ForeignKeyInfo {
                    name: "orders_region_fk".into(),
                    references: "ref.regions".into(),
                    ..ForeignKeyInfo::default()
                },
            ],
            ..ObjectDetail::default()
        };
        let t = TableModel::from_detail(&d);
        assert_eq!(
            t.primary_key,
            Some(KeyDef {
                name: "orders_pkey".into(),
                columns: vec!["id".into()]
            })
        );
        assert_eq!(t.uniques.len(), 1);
        assert_eq!(t.uniques[0].columns, ["code"]);
        assert_eq!(t.checks[0].expr, "(code <> '')");
        assert_eq!(t.foreign_keys[0].ref_schema, None);
        assert_eq!(t.foreign_keys[0].on_update, None);
        assert_eq!(t.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(t.foreign_keys[1].ref_schema.as_deref(), Some("ref"));
        let names: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["orders_code_idx"]);
    }

    #[test]
    fn checks_are_parenthesized_once() {
        assert_eq!(check_expr("CHECK ((a > 0))"), "((a > 0))");
        assert_eq!(check_expr("([a]>(0))"), "([a]>(0))");
        assert_eq!(check_expr("a > 0"), "(a > 0)");
        assert_eq!(check_expr("CHECK (a > 0) NOT VALID"), "(a > 0)");
    }
}
