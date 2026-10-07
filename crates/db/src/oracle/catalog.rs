//! Oracle catalog from the `ALL_*` dictionary views (what the user can see) and
//! `DBMS_METADATA.GET_DDL`. Schemas are users.

use oracle::Connection;
use oracle::sql_type::ToSql;

use super::ora_error;
use crate::catalog::{
    CatalogChunk, ColumnInfo, ForeignKeyInfo, IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo,
    ObjectKind, SchemaInfo,
};
use crate::error::{DbError, Result};

/// Rows of a small query as optional strings.
fn rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Vec<Option<String>>>> {
    let rs = conn.query(sql, params).map_err(ora_error)?;
    let mut out = Vec::new();
    for row in rs {
        let row = row.map_err(ora_error)?;
        let n = row.sql_values().len();
        out.push(
            (0..n)
                .map(|i| row.get::<usize, Option<String>>(i).ok().flatten())
                .collect(),
        );
    }
    Ok(out)
}

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// `(view, name column, extra filter)` listing one kind.
fn listing(kind: ObjectKind) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match kind {
        ObjectKind::Table => (
            "ALL_TABLES",
            "TABLE_NAME",
            "AND NESTED = 'NO' AND SECONDARY = 'N'",
        ),
        ObjectKind::View => ("ALL_VIEWS", "VIEW_NAME", ""),
        ObjectKind::MaterializedView => ("ALL_MVIEWS", "MVIEW_NAME", ""),
        ObjectKind::Sequence => ("ALL_SEQUENCES", "SEQUENCE_NAME", ""),
        ObjectKind::Synonym => ("ALL_SYNONYMS", "SYNONYM_NAME", ""),
        ObjectKind::Procedure => (
            "ALL_OBJECTS",
            "OBJECT_NAME",
            "AND OBJECT_TYPE = 'PROCEDURE'",
        ),
        ObjectKind::Function => ("ALL_OBJECTS", "OBJECT_NAME", "AND OBJECT_TYPE = 'FUNCTION'"),
        _ => return None,
    })
}

fn owner_column(view: &str) -> &'static str {
    if view == "ALL_SEQUENCES" {
        "SEQUENCE_OWNER"
    } else {
        "OWNER"
    }
}

const COLUMNS: &str = "SELECT OWNER, TABLE_NAME, COLUMN_NAME, \
    DATA_TYPE || CASE \
      WHEN DATA_TYPE IN ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW') THEN '(' || CHAR_LENGTH || ')' \
      WHEN DATA_TYPE = 'NUMBER' AND DATA_PRECISION IS NOT NULL \
        THEN '(' || DATA_PRECISION || CASE WHEN DATA_SCALE > 0 THEN ',' || DATA_SCALE END || ')' \
    END, NULLABLE, DATA_DEFAULT, COLUMN_ID FROM ALL_TAB_COLUMNS";

fn column(row: &[Option<String>], pk: &[String]) -> ColumnInfo {
    let name = text(row, 2);
    ColumnInfo {
        schema: text(row, 0),
        table: text(row, 1),
        is_primary_key: pk.contains(&name),
        name,
        data_type: text(row, 3),
        nullable: text(row, 4) != "N",
        default: row
            .get(5)
            .cloned()
            .flatten()
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty()),
        ordinal: text(row, 6).parse().unwrap_or_default(),
    }
}

fn ddl_type(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Synonym => "SYNONYM",
        _ => "TABLE",
    }
}

pub(super) fn introspect(conn: &Connection, scope: IntrospectScope) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => {
            let r = rows(
                conn,
                "SELECT SYS_CONTEXT('USERENV', 'DB_NAME') FROM DUAL",
                &[],
            )?;
            Ok(CatalogChunk::Databases(
                r.first().map(|row| vec![text(row, 0)]).unwrap_or_default(),
            ))
        }
        IntrospectScope::Schemas => {
            // ORACLE_MAINTAINED exists from 12c; older servers list every user as ordinary.
            let r = rows(
                conn,
                "SELECT USERNAME, ORACLE_MAINTAINED FROM ALL_USERS ORDER BY USERNAME",
                &[],
            )
            .or_else(|_| {
                rows(
                    conn,
                    "SELECT USERNAME, 'N' FROM ALL_USERS ORDER BY USERNAME",
                    &[],
                )
            })?;
            Ok(CatalogChunk::Schemas(
                r.iter()
                    .map(|row| SchemaInfo {
                        name: text(row, 0),
                        is_system: text(row, 1) == "Y",
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Objects { schema, kind } => {
            let Some((view, name_col, filter)) = listing(kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let rows_col = if view == "ALL_TABLES" {
                "NUM_ROWS"
            } else {
                "NULL"
            };
            let sql = format!(
                "SELECT {name_col}, {rows_col} FROM {view} WHERE {} = :1 {filter} ORDER BY {name_col}",
                owner_column(view)
            );
            let r = rows(conn, &sql, &[&schema])?;
            Ok(CatalogChunk::Objects(
                r.iter()
                    .map(|row| ObjectInfo {
                        schema: schema.clone(),
                        name: text(row, 0),
                        kind,
                        estimated_rows: row.get(1).cloned().flatten().and_then(|n| n.parse().ok()),
                        detail: None,
                    })
                    .collect(),
            ))
        }
        IntrospectScope::AllColumns => {
            let sql = format!(
                "{COLUMNS} WHERE OWNER = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') \
                 ORDER BY TABLE_NAME, COLUMN_ID"
            );
            let r = rows(conn, &sql, &[])?;
            Ok(CatalogChunk::AllColumns(
                r.iter().map(|row| column(row, &[])).collect(),
            ))
        }
        IntrospectScope::Detail { schema, name, kind } => {
            let pk: Vec<String> = rows(
                conn,
                "SELECT cc.COLUMN_NAME FROM ALL_CONSTRAINTS c \
                 JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
                 WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.CONSTRAINT_TYPE = 'P' ORDER BY cc.POSITION",
                &[&schema, &name],
            )?
            .iter()
            .map(|r| text(r, 0))
            .collect();
            let columns: Vec<ColumnInfo> = rows(
                conn,
                &format!("{COLUMNS} WHERE OWNER = :1 AND TABLE_NAME = :2 ORDER BY COLUMN_ID"),
                &[&schema, &name],
            )?
            .iter()
            .map(|row| column(row, &pk))
            .collect();
            let mut indexes: Vec<IndexInfo> = Vec::new();
            for row in rows(
                conn,
                "SELECT i.INDEX_NAME, i.UNIQUENESS, ic.COLUMN_NAME, \
                   (SELECT COUNT(*) FROM ALL_CONSTRAINTS c WHERE c.OWNER = i.TABLE_OWNER \
                      AND c.INDEX_NAME = i.INDEX_NAME AND c.CONSTRAINT_TYPE = 'P') \
                 FROM ALL_INDEXES i JOIN ALL_IND_COLUMNS ic \
                   ON ic.INDEX_OWNER = i.OWNER AND ic.INDEX_NAME = i.INDEX_NAME \
                 WHERE i.TABLE_OWNER = :1 AND i.TABLE_NAME = :2 \
                 ORDER BY i.INDEX_NAME, ic.COLUMN_POSITION",
                &[&schema, &name],
            )? {
                let iname = text(&row, 0);
                let col = text(&row, 2);
                match indexes.iter_mut().find(|i| i.name == iname) {
                    Some(i) => i.columns.push(col),
                    None => indexes.push(IndexInfo {
                        is_unique: text(&row, 1) == "UNIQUE",
                        is_primary: text(&row, 3) != "0",
                        definition: String::new(),
                        name: iname,
                        columns: vec![col],
                    }),
                }
            }
            for i in &mut indexes {
                i.definition = format!(
                    "{}INDEX {} ON {} ({})",
                    if i.is_unique { "UNIQUE " } else { "" },
                    i.name,
                    name,
                    i.columns.join(", ")
                );
            }
            let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
            for row in rows(
                conn,
                "SELECT c.CONSTRAINT_NAME, cc.COLUMN_NAME, r.OWNER || '.' || r.TABLE_NAME, rc.COLUMN_NAME \
                 FROM ALL_CONSTRAINTS c \
                 JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
                 JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
                 JOIN ALL_CONS_COLUMNS rc ON rc.OWNER = r.OWNER AND rc.CONSTRAINT_NAME = r.CONSTRAINT_NAME \
                   AND rc.POSITION = cc.POSITION \
                 WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.CONSTRAINT_TYPE = 'R' \
                 ORDER BY c.CONSTRAINT_NAME, cc.POSITION",
                &[&schema, &name],
            )? {
                let fname = text(&row, 0);
                match foreign_keys.iter_mut().find(|f| f.name == fname) {
                    Some(f) => {
                        f.columns.push(text(&row, 1));
                        f.referenced_columns.push(text(&row, 3));
                    }
                    None => foreign_keys.push(ForeignKeyInfo {
                        name: fname,
                        columns: vec![text(&row, 1)],
                        references: text(&row, 2),
                        referenced_columns: vec![text(&row, 3)],
                    }),
                }
            }
            let triggers: Vec<String> = rows(
                conn,
                "SELECT TRIGGER_NAME FROM ALL_TRIGGERS WHERE TABLE_OWNER = :1 AND TABLE_NAME = :2 ORDER BY 1",
                &[&schema, &name],
            )?
            .iter()
            .map(|r| text(r, 0))
            .collect();
            let ddl_kind = ddl_type(kind);
            let ddl = rows(
                conn,
                "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM DUAL",
                &[&ddl_kind, &name, &schema],
            )
            .ok()
            .and_then(|r| r.first().map(|row| text(row, 0).trim().to_owned()))
            .unwrap_or_default();
            if columns.is_empty() && ddl.is_empty() {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            }
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    schema,
                    name,
                    kind,
                    estimated_rows: None,
                    detail: None,
                },
                columns,
                indexes,
                constraints: Vec::new(),
                foreign_keys,
                triggers,
                ddl,
            })))
        }
    }
}
