//! Snowflake catalog from the current database's `INFORMATION_SCHEMA` and `GET_DDL`.

use super::SnowflakeSession;
use crate::catalog::{
    CatalogChunk, ColumnInfo, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo,
};
use crate::error::{DbError, Result};
use crate::value::Value;

/// Rows of one query, with columns found by name.
struct Rows {
    names: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    async fn of(s: &SnowflakeSession, sql: &str, params: &[Value]) -> Result<Self> {
        let (names, rows) = s.rows(sql, params).await?;
        Ok(Self { names, rows })
    }

    fn get<'a>(&self, row: &'a [Option<String>], name: &str) -> Option<&'a str> {
        let i = self
            .names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))?;
        row.get(i)?.as_deref()
    }

    fn text(&self, row: &[Option<String>], name: &str) -> String {
        self.get(row, name).unwrap_or_default().to_owned()
    }
}

fn table_types(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => {
            Some("TABLE_TYPE IN ('BASE TABLE', 'TEMPORARY TABLE', 'EXTERNAL TABLE')")
        }
        ObjectKind::View => Some("TABLE_TYPE IN ('VIEW', 'MATERIALIZED VIEW')"),
        _ => None,
    }
}

fn column_info(r: &Rows, row: &[Option<String>]) -> ColumnInfo {
    ColumnInfo {
        schema: r.text(row, "TABLE_SCHEMA"),
        table: r.text(row, "TABLE_NAME"),
        name: r.text(row, "COLUMN_NAME"),
        data_type: r.text(row, "DATA_TYPE"),
        nullable: r.get(row, "IS_NULLABLE") != Some("NO"),
        default: r.get(row, "COLUMN_DEFAULT").map(str::to_owned),
        ordinal: r
            .get(row, "ORDINAL_POSITION")
            .and_then(|v| v.parse().ok())
            .unwrap_or_default(),
        is_primary_key: false,
    }
}

const COLUMNS: &str = "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, \
                       COLUMN_DEFAULT, ORDINAL_POSITION FROM INFORMATION_SCHEMA.COLUMNS";

/// `"schema"."name"`, quoted exactly as stored.
fn quoted(schema: &str, name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema.replace('"', "\"\""),
        name.replace('"', "\"\"")
    )
}

pub(super) async fn introspect(
    s: &SnowflakeSession,
    scope: IntrospectScope,
) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => {
            let r = Rows::of(s, "SHOW DATABASES", &[]).await?;
            Ok(CatalogChunk::Databases(
                r.rows.iter().map(|row| r.text(row, "name")).collect(),
            ))
        }
        IntrospectScope::Schemas => {
            let r = Rows::of(
                s,
                "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA ORDER BY SCHEMA_NAME",
                &[],
            )
            .await?;
            Ok(CatalogChunk::Schemas(
                r.rows
                    .iter()
                    .map(|row| {
                        let name = r.text(row, "SCHEMA_NAME");
                        SchemaInfo {
                            is_system: name == "INFORMATION_SCHEMA",
                            name,
                        }
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Objects { schema, kind } => {
            let Some(filter) = table_types(kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let sql = format!(
                "SELECT TABLE_NAME, ROW_COUNT, COMMENT FROM INFORMATION_SCHEMA.TABLES \
                 WHERE TABLE_SCHEMA = ? AND {filter} ORDER BY TABLE_NAME"
            );
            let r = Rows::of(s, &sql, &[Value::Text(schema.clone())]).await?;
            Ok(CatalogChunk::Objects(
                r.rows
                    .iter()
                    .map(|row| ObjectInfo {
                        schema: schema.clone(),
                        name: r.text(row, "TABLE_NAME"),
                        kind,
                        estimated_rows: r.get(row, "ROW_COUNT").and_then(|v| v.parse().ok()),
                        detail: r.get(row, "COMMENT").map(str::to_owned),
                    })
                    .collect(),
            ))
        }
        IntrospectScope::AllColumns => {
            let sql = format!(
                "{COLUMNS} WHERE TABLE_SCHEMA <> 'INFORMATION_SCHEMA' \
                 ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"
            );
            let r = Rows::of(s, &sql, &[]).await?;
            Ok(CatalogChunk::AllColumns(
                r.rows.iter().map(|row| column_info(&r, row)).collect(),
            ))
        }
        IntrospectScope::Detail { schema, name, kind } => {
            let sql = format!(
                "{COLUMNS} WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION"
            );
            let r = Rows::of(
                s,
                &sql,
                &[Value::Text(schema.clone()), Value::Text(name.clone())],
            )
            .await?;
            if r.rows.is_empty() {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            }
            let columns = r.rows.iter().map(|row| column_info(&r, row)).collect();
            let object_type = if kind == ObjectKind::View {
                "VIEW"
            } else {
                "TABLE"
            };
            let ddl = Rows::of(
                s,
                "SELECT GET_DDL(?, ?) AS DDL",
                &[
                    Value::Text(object_type.into()),
                    Value::Text(quoted(&schema, &name)),
                ],
            )
            .await
            .ok()
            .and_then(|d| d.rows.first().map(|row| d.text(row, "DDL")))
            .unwrap_or_default();
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    schema,
                    name,
                    kind,
                    estimated_rows: None,
                    detail: None,
                },
                columns,
                indexes: Vec::new(),
                constraints: Vec::new(),
                foreign_keys: Vec::new(),
                triggers: Vec::new(),
                ddl,
            })))
        }
    }
}
