//! Snowflake catalog from the current database's `INFORMATION_SCHEMA` and `GET_DDL`.

use super::SnowflakeSession;
use crate::catalog::{
    CatalogChunk, ColumnInfo, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo,
    like_contains, search_hit,
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

/// The query listing the objects of `kind` in a schema (bound to `?`), or `None` for a
/// kind Snowflake's explorer does not show.
pub(crate) fn objects_sql(kind: ObjectKind) -> Option<String> {
    let filter = match kind {
        ObjectKind::Table => "TABLE_TYPE IN ('BASE TABLE', 'TEMPORARY TABLE', 'EXTERNAL TABLE')",
        ObjectKind::View => "TABLE_TYPE = 'VIEW'",
        ObjectKind::MaterializedView => "TABLE_TYPE = 'MATERIALIZED VIEW'",
        _ => return None,
    };
    Some(format!(
        "SELECT TABLE_NAME, ROW_COUNT, COMMENT FROM INFORMATION_SCHEMA.TABLES \
         WHERE TABLE_SCHEMA = ? AND {filter} ORDER BY TABLE_NAME"
    ))
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

/// Global object search over the current database's `INFORMATION_SCHEMA`: every `?` is
/// the same escaped LIKE pattern. `limit` is a number, never user text.
fn search_sql(limit: u32, include_system: bool) -> String {
    let user = if include_system {
        ""
    } else {
        " AND TABLE_SCHEMA <> 'INFORMATION_SCHEMA'"
    };
    format!(
        "SELECT SCHEMA_NAME, OBJECT_NAME, KIND FROM ( \
           SELECT TABLE_SCHEMA AS SCHEMA_NAME, TABLE_NAME AS OBJECT_NAME, \
                  CASE TABLE_TYPE WHEN 'VIEW' THEN 'view' WHEN 'MATERIALIZED VIEW' THEN 'mview' \
                       ELSE 'table' END AS KIND \
           FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME ILIKE ? ESCAPE '!'{user} \
           UNION ALL \
           SELECT FUNCTION_SCHEMA, FUNCTION_NAME, 'function' \
           FROM INFORMATION_SCHEMA.FUNCTIONS WHERE FUNCTION_NAME ILIKE ? ESCAPE '!' \
           UNION ALL \
           SELECT PROCEDURE_SCHEMA, PROCEDURE_NAME, 'procedure' \
           FROM INFORMATION_SCHEMA.PROCEDURES WHERE PROCEDURE_NAME ILIKE ? ESCAPE '!' \
         ) GROUP BY SCHEMA_NAME, OBJECT_NAME, KIND \
         ORDER BY LENGTH(OBJECT_NAME), OBJECT_NAME, SCHEMA_NAME LIMIT {limit}"
    )
}

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
            let Some(sql) = objects_sql(kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
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
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let like = Value::Text(like_contains(&pattern, false));
            let r = Rows::of(
                s,
                &search_sql(limit, include_system),
                &[like.clone(), like.clone(), like],
            )
            .await?;
            Ok(CatalogChunk::Objects(
                r.rows
                    .iter()
                    .filter_map(|row| {
                        search_hit(
                            r.text(row, "SCHEMA_NAME"),
                            r.text(row, "OBJECT_NAME"),
                            &r.text(row, "KIND"),
                        )
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
            // `GET_DDL('VIEW', ...)` also covers materialized views.
            let object_type = match kind {
                ObjectKind::View | ObjectKind::MaterializedView => "VIEW",
                _ => "TABLE",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_sql_per_kind() {
        let all: Vec<String> = [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
        ]
        .into_iter()
        .map(|k| format!("{k:?}: {}", objects_sql(k).unwrap_or_else(|| "-".into())))
        .collect();
        insta::assert_snapshot!("snowflake_objects_sql", all.join("\n"));
    }

    #[test]
    fn search_sql_snapshot() {
        insta::assert_snapshot!("snowflake_search_sql", search_sql(200, false));
    }
}
