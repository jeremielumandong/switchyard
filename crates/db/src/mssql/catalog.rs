//! SQL Server catalog from the `sys` views of the current database.

use tiberius::Query;

use super::{TdsClient, decode, map_error, simple_rows};
use crate::catalog::{
    CatalogChunk, ColumnInfo, ConstraintInfo, ForeignKeyInfo, IndexInfo, IntrospectScope,
    ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, like_contains, search_hit,
};
use crate::error::{DbError, Result};
use crate::value::Value;

/// `N'…'` with quotes doubled.
fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn text(row: &[Value], i: usize) -> String {
    match row.get(i) {
        Some(Value::Text(s) | Value::Other(s) | Value::Numeric(s)) => s.clone(),
        Some(Value::Int(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn int(row: &[Value], i: usize) -> i64 {
    match row.get(i) {
        Some(Value::Int(n)) => *n,
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

fn opt_text(row: &[Value], i: usize) -> Option<String> {
    match row.get(i) {
        None | Some(Value::Null) => None,
        _ => Some(text(row, i)),
    }
}

/// Schemas that hold the engine's own objects.
const SYSTEM_SCHEMAS: &str = "('sys','INFORMATION_SCHEMA','guest','db_owner','db_accessadmin',\
'db_securityadmin','db_ddladmin','db_backupoperator','db_datareader','db_datawriter',\
'db_denydatareader','db_denydatawriter')";

/// A formatted type: `nvarchar(200)`, `decimal(10,2)`, `varbinary(max)`.
const TYPE_EXPR: &str = "t.name + CASE \
    WHEN t.name IN ('varchar','char','varbinary','binary') \
        THEN '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')' \
    WHEN t.name IN ('nvarchar','nchar') \
        THEN '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
    WHEN t.name IN ('decimal','numeric') \
        THEN '(' + CAST(c.precision AS varchar(5)) + ',' + CAST(c.scale AS varchar(5)) + ')' \
    WHEN t.name IN ('datetime2','time','datetimeoffset') AND c.scale <> 7 \
        THEN '(' + CAST(c.scale AS varchar(5)) + ')' \
    ELSE '' END";

fn objects_sql(schema: &str, kind: ObjectKind) -> Option<String> {
    let s = lit(schema);
    Some(match kind {
        ObjectKind::Table => format!(
            "SELECT o.name, (SELECT SUM(p.rows) FROM sys.partitions p \
                             WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)), NULL \
             FROM sys.tables o WHERE SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::View => format!(
            "SELECT o.name, NULL, NULL FROM sys.views o \
             WHERE SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::Procedure => format!(
            "SELECT o.name, NULL, NULL FROM sys.procedures o \
             WHERE SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::Function => format!(
            "SELECT o.name, NULL, o.type_desc FROM sys.objects o \
             WHERE o.type IN ('FN','IF','TF','FS','FT') AND SCHEMA_NAME(o.schema_id) = {s} \
             ORDER BY o.name"
        ),
        ObjectKind::Sequence => format!(
            "SELECT o.name, NULL, NULL FROM sys.sequences o \
             WHERE SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::Synonym => format!(
            "SELECT o.name, NULL, o.base_object_name FROM sys.synonyms o \
             WHERE SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::Type => format!(
            "SELECT o.name, NULL, NULL FROM sys.types o \
             WHERE o.is_user_defined = 1 AND SCHEMA_NAME(o.schema_id) = {s} ORDER BY o.name"
        ),
        ObjectKind::MaterializedView => return None,
    })
}

/// Global object search over `sys.objects`: `@P1` is an escaped, lower-cased LIKE
/// pattern, `@P2` the row cap, `@P3` = 1 includes system schemas and shipped objects.
fn search_sql() -> String {
    format!(
        "SELECT TOP (@P2) s.name, o.name, \
                CASE o.type WHEN 'U' THEN 'table' WHEN 'V' THEN 'view' \
                            WHEN 'P' THEN 'procedure' WHEN 'PC' THEN 'procedure' \
                            WHEN 'SO' THEN 'sequence' WHEN 'SN' THEN 'synonym' \
                            ELSE 'function' END \
         FROM sys.objects o \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE o.type IN ('U','V','P','PC','FN','IF','TF','FS','FT','SO','SN') \
           AND LOWER(o.name) LIKE @P1 ESCAPE '!' \
           AND (@P3 = 1 OR (o.is_ms_shipped = 0 AND s.name NOT IN {SYSTEM_SCHEMAS})) \
         ORDER BY LEN(o.name), o.name, s.name"
    )
}

/// Run one parameterized query and collect its first result set.
async fn param_rows(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mut q = Query::new(sql);
    for p in params {
        decode::bind(&mut q, p);
    }
    let stream = q.query(client).await.map_err(map_error)?;
    let rows = stream.into_first_result().await.map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| r.cells().map(|(_, d)| decode::to_value(d)).collect())
        .collect())
}

fn columns_sql(filter: &str) -> String {
    format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, c.name, {TYPE_EXPR}, c.is_nullable, \
                OBJECT_DEFINITION(c.default_object_id), c.column_id, \
                CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i \
                    JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                    WHERE i.object_id = o.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id) \
                THEN 1 ELSE 0 END, \
                c.is_identity \
         FROM sys.columns c \
         JOIN sys.objects o ON o.object_id = c.object_id \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         WHERE o.type IN ('U','V') AND {filter} \
         ORDER BY SCHEMA_NAME(o.schema_id), o.name, c.column_id"
    )
}

fn column_from(row: &[Value]) -> ColumnInfo {
    ColumnInfo {
        schema: text(row, 0),
        table: text(row, 1),
        name: text(row, 2),
        data_type: text(row, 3),
        nullable: int(row, 4) != 0,
        default: opt_text(row, 5),
        ordinal: int(row, 6) as i32,
        is_primary_key: int(row, 7) != 0,
    }
}

fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// `CREATE TABLE` text from the catalog (the server has no built-in generator).
fn table_ddl(schema: &str, name: &str, cols: &[(ColumnInfo, bool)], pk: &[String]) -> String {
    let mut lines: Vec<String> = cols
        .iter()
        .map(|(c, identity)| {
            let mut l = format!("    {} {}", quote(&c.name), c.data_type);
            if *identity {
                l.push_str(" IDENTITY");
            }
            l.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(d) = &c.default {
                l.push_str(" DEFAULT ");
                l.push_str(d);
            }
            l
        })
        .collect();
    if !pk.is_empty() {
        lines.push(format!(
            "    PRIMARY KEY ({})",
            pk.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
        ));
    }
    format!(
        "CREATE TABLE {}.{} (\n{}\n);",
        quote(schema),
        quote(name),
        lines.join(",\n")
    )
}

pub(super) async fn introspect(
    client: &mut TdsClient,
    scope: IntrospectScope,
) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => {
            let rows = simple_rows(
                client,
                "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name",
            )
            .await?;
            Ok(CatalogChunk::Databases(
                rows.iter().map(|r| text(r, 0)).collect(),
            ))
        }
        IntrospectScope::Schemas => {
            let rows = simple_rows(
                client,
                &format!(
                    "SELECT s.name, CASE WHEN s.name IN {SYSTEM_SCHEMAS} THEN 1 ELSE 0 END \
                     FROM sys.schemas s \
                     WHERE s.name IN ('dbo','sys','INFORMATION_SCHEMA') \
                        OR EXISTS (SELECT 1 FROM sys.objects o WHERE o.schema_id = s.schema_id) \
                        OR s.principal_id = DATABASE_PRINCIPAL_ID() \
                     ORDER BY CASE WHEN s.name = 'dbo' THEN 0 ELSE 1 END, s.name"
                ),
            )
            .await?;
            Ok(CatalogChunk::Schemas(
                rows.iter()
                    .map(|r| SchemaInfo {
                        name: text(r, 0),
                        is_system: int(r, 1) != 0,
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Objects { schema, kind } => {
            let Some(sql) = objects_sql(&schema, kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let rows = simple_rows(client, &sql).await?;
            Ok(CatalogChunk::Objects(
                rows.iter()
                    .map(|r| ObjectInfo {
                        schema: schema.clone(),
                        name: text(r, 0),
                        kind,
                        estimated_rows: matches!(
                            r.get(1),
                            Some(Value::Int(_)) | Some(Value::Numeric(_))
                        )
                        .then(|| text(r, 1).parse().unwrap_or_default()),
                        detail: opt_text(r, 2),
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let like = like_contains(&pattern.to_lowercase(), true);
            let rows = param_rows(
                client,
                &search_sql(),
                &[
                    Value::Text(like),
                    Value::Int(i64::from(limit)),
                    Value::Int(i64::from(include_system)),
                ],
            )
            .await?;
            Ok(CatalogChunk::Objects(
                rows.iter()
                    .filter_map(|r| search_hit(text(r, 0), text(r, 1), &text(r, 2)))
                    .collect(),
            ))
        }
        IntrospectScope::AllColumns => {
            let rows = simple_rows(client, &columns_sql("1 = 1")).await?;
            Ok(CatalogChunk::AllColumns(
                rows.iter().map(|r| column_from(r)).collect(),
            ))
        }
        IntrospectScope::Detail { schema, name, kind } => {
            let full = format!("{}.{}", quote(&schema), quote(&name));
            let id = format!("OBJECT_ID({})", lit(&full));
            let rows = simple_rows(client, &format!("SELECT {id}")).await?;
            if matches!(
                rows.first().and_then(|r| r.first()),
                None | Some(Value::Null)
            ) {
                return Err(DbError::Unsupported(format!("{full} no longer exists")));
            }
            let col_rows =
                simple_rows(client, &columns_sql(&format!("o.object_id = {id}"))).await?;
            let columns: Vec<(ColumnInfo, bool)> = col_rows
                .iter()
                .map(|r| (column_from(r), int(r, 8) != 0))
                .collect();

            let idx_rows = simple_rows(
                client,
                &format!(
                    "SELECT i.name, i.is_unique, i.is_primary_key, i.type_desc, c.name, ic.is_descending_key \
                     FROM sys.indexes i \
                     JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE i.object_id = {id} AND i.name IS NOT NULL AND ic.is_included_column = 0 \
                     ORDER BY i.name, ic.key_ordinal"
                ),
            )
            .await?;
            let mut indexes: Vec<IndexInfo> = Vec::new();
            for r in &idx_rows {
                let iname = text(r, 0);
                let col = if int(r, 5) != 0 {
                    format!("{} DESC", text(r, 4))
                } else {
                    text(r, 4)
                };
                match indexes.iter_mut().find(|i| i.name == iname) {
                    Some(i) => i.columns.push(col),
                    None => indexes.push(IndexInfo {
                        is_unique: int(r, 1) != 0,
                        is_primary: int(r, 2) != 0,
                        definition: text(r, 3).to_ascii_lowercase().replace('_', " "),
                        name: iname,
                        columns: vec![col],
                    }),
                }
            }
            for i in &mut indexes {
                i.definition = format!(
                    "{}{} INDEX {} ON {full} ({})",
                    if i.is_unique { "UNIQUE " } else { "" },
                    i.definition.to_ascii_uppercase(),
                    quote(&i.name),
                    i.columns.join(", ")
                );
            }

            let fk_rows = simple_rows(
                client,
                &format!(
                    "SELECT fk.name, pc.name, SCHEMA_NAME(rt.schema_id) + '.' + rt.name, rc.name \
                     FROM sys.foreign_keys fk \
                     JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                     JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id \
                     JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                     WHERE fk.parent_object_id = {id} ORDER BY fk.name, fkc.constraint_column_id"
                ),
            )
            .await?;
            let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
            for r in &fk_rows {
                let fname = text(r, 0);
                match foreign_keys.iter_mut().find(|f| f.name == fname) {
                    Some(f) => {
                        f.columns.push(text(r, 1));
                        f.referenced_columns.push(text(r, 3));
                    }
                    None => foreign_keys.push(ForeignKeyInfo {
                        name: fname,
                        columns: vec![text(r, 1)],
                        references: text(r, 2),
                        referenced_columns: vec![text(r, 3)],
                    }),
                }
            }

            let check_rows = simple_rows(
                client,
                &format!(
                    "SELECT name, 'CHECK', definition FROM sys.check_constraints WHERE parent_object_id = {id} \
                     UNION ALL SELECT name, 'DEFAULT', definition FROM sys.default_constraints WHERE parent_object_id = {id} \
                     ORDER BY 1"
                ),
            )
            .await?;
            let constraints = check_rows
                .iter()
                .map(|r| ConstraintInfo {
                    name: text(r, 0),
                    kind: text(r, 1),
                    definition: text(r, 2),
                })
                .collect();

            let trig_rows = simple_rows(
                client,
                &format!("SELECT name FROM sys.triggers WHERE parent_id = {id} ORDER BY name"),
            )
            .await?;
            let triggers = trig_rows.iter().map(|r| text(r, 0)).collect();

            let ddl = if kind == ObjectKind::Table {
                let pk: Vec<String> = indexes
                    .iter()
                    .find(|i| i.is_primary)
                    .map(|i| i.columns.clone())
                    .unwrap_or_default();
                let mut ddl = table_ddl(&schema, &name, &columns, &pk);
                for i in indexes.iter().filter(|i| !i.is_primary) {
                    ddl.push('\n');
                    ddl.push_str(&i.definition);
                    ddl.push(';');
                }
                ddl
            } else {
                let rows = simple_rows(client, &format!("SELECT OBJECT_DEFINITION({id})")).await?;
                rows.first().map(|r| text(r, 0)).unwrap_or_default()
            };

            let estimated_rows = if kind == ObjectKind::Table {
                simple_rows(
                    client,
                    &format!(
                        "SELECT SUM(rows) FROM sys.partitions WHERE object_id = {id} AND index_id IN (0, 1)"
                    ),
                )
                .await?
                .first()
                .map(|r| int(r, 0))
            } else {
                None
            };

            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    schema,
                    name,
                    kind,
                    estimated_rows,
                    detail: None,
                },
                columns: columns.into_iter().map(|(c, _)| c).collect(),
                indexes,
                constraints,
                foreign_keys,
                triggers,
                ddl,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_sql_snapshot() {
        insta::assert_snapshot!("mssql_search_sql", search_sql());
    }
}
