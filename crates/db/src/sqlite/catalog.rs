//! Catalog of a local SQLite file from `sqlite_schema` and the table-valued `pragma_*`
//! functions. Each attached database is a schema (`main` first); SQLite's own
//! `sqlite_*` tables are hidden unless system objects are asked for.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row, params};

use super::map_error;
use crate::catalog::{
    CatalogChunk, ColumnInfo, ForeignKeyInfo, IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo,
    ObjectKind, SchemaInfo, TriggerInfo, like_contains, search_hit,
};
use crate::d1::catalog::trigger_timing;
use crate::error::{DbError, Result};

const USER_OBJECTS: &str = "name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

/// `"schema"`, quoted for use before `.sqlite_schema`.
fn quote(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

fn db_err(e: rusqlite::Error, sql: &str) -> DbError {
    map_error(e, sql, 0)
}

fn query<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    mut f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql).map_err(|e| db_err(e, sql))?;
    let rows = stmt
        .query_map(params, |r| f(r))
        .map_err(|e| db_err(e, sql))?;
    rows.collect::<rusqlite::Result<Vec<T>>>()
        .map_err(|e| db_err(e, sql))
}

/// Text of a cell whatever its storage class; empty for NULL.
fn text(r: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    Ok(match r.get_ref(i)? {
        ValueRef::Null => String::new(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
    })
}

fn opt_text(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<String>> {
    Ok(match r.get_ref(i)? {
        ValueRef::Null => None,
        _ => Some(text(r, i)?),
    })
}

/// Attached databases, `main` first; `temp` only when it holds something.
fn schemas(conn: &Connection) -> Result<Vec<String>> {
    let names = query(
        conn,
        "SELECT name FROM pragma_database_list ORDER BY seq",
        [],
        |r| r.get::<_, String>(0),
    )?;
    let temp_used = query(conn, "SELECT count(*) FROM temp.sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })?
    .first()
    .is_some_and(|n| *n > 0);
    Ok(names
        .into_iter()
        .filter(|n| n != "temp" || temp_used)
        .collect())
}

fn type_filter(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => Some("table"),
        ObjectKind::View => Some("view"),
        _ => None,
    }
}

/// `pragma_table_info` columns of `table` in `schema`.
fn columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
    query(
        conn,
        "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1, ?2) ORDER BY cid",
        params![table, schema],
        |r| {
            Ok(ColumnInfo {
                schema: schema.to_owned(),
                table: table.to_owned(),
                name: text(r, 1)?,
                data_type: text(r, 2)?,
                nullable: r.get::<_, i64>(3)? == 0,
                default: opt_text(r, 4)?,
                ordinal: r.get::<_, i32>(0)? + 1,
                is_primary_key: r.get::<_, i64>(5)? > 0,
                comment: None,
            })
        },
    )
}

fn object(schema: &str, name: String, kind: ObjectKind) -> ObjectInfo {
    ObjectInfo {
        schema: schema.to_owned(),
        name,
        kind,
        estimated_rows: None,
        detail: None,
    }
}

pub(super) fn introspect(conn: &mut Connection, scope: IntrospectScope) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => Ok(CatalogChunk::Databases(vec!["main".into()])),
        IntrospectScope::Schemas => Ok(CatalogChunk::Schemas(
            schemas(conn)?
                .into_iter()
                .map(|name| SchemaInfo {
                    name,
                    is_system: false,
                })
                .collect(),
        )),
        IntrospectScope::Objects { schema, kind } => {
            let Some(ty) = type_filter(kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let sql = format!(
                "SELECT name FROM {}.sqlite_schema WHERE type = ?1 AND {USER_OBJECTS} ORDER BY name",
                quote(&schema)
            );
            let names = query(conn, &sql, [ty], |r| r.get::<_, String>(0))?;
            Ok(CatalogChunk::Objects(
                names
                    .into_iter()
                    .map(|n| object(&schema, n, kind))
                    .collect(),
            ))
        }
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let user = if include_system {
                String::new()
            } else {
                format!(" AND {USER_OBJECTS}")
            };
            let pattern = like_contains(&pattern, false);
            let mut out = Vec::new();
            for schema in schemas(conn)? {
                // SQLite's LIKE ignores ASCII case.
                let sql = format!(
                    "SELECT name, type FROM {}.sqlite_schema \
                     WHERE type IN ('table', 'view') AND name LIKE ?1 ESCAPE '!'{user} \
                     ORDER BY length(name), name LIMIT ?2",
                    quote(&schema)
                );
                let hits = query(conn, &sql, params![pattern, limit], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                out.extend(
                    hits.into_iter()
                        .filter_map(|(name, ty)| search_hit(schema.clone(), name, &ty)),
                );
            }
            out.truncate(limit as usize);
            Ok(CatalogChunk::Objects(out))
        }
        IntrospectScope::RoutineDefinition {
            schema, name, kind, ..
        } => {
            // SQLite has no stored routines; answer with whatever the schema holds.
            let ddl = definition(conn, &schema, &name)?
                .ok_or_else(|| DbError::Unsupported(format!("{name} no longer exists")))?;
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
                &schema,
                &name,
                kind,
                ddl,
                Vec::new(),
            ))))
        }
        // The explorer hides dependencies for SQLite (`Dialect::supports_dependencies`).
        IntrospectScope::Dependencies { .. } => Err(DbError::Unsupported(
            "SQLite keeps no dependency catalog".into(),
        )),
        IntrospectScope::AllColumns => {
            let mut out = Vec::new();
            for schema in schemas(conn)? {
                let sql = format!(
                    "SELECT name FROM {}.sqlite_schema \
                     WHERE type IN ('table', 'view') AND {USER_OBJECTS} ORDER BY name",
                    quote(&schema)
                );
                for table in query(conn, &sql, [], |r| r.get::<_, String>(0))? {
                    out.extend(columns(conn, &schema, &table)?);
                }
            }
            Ok(CatalogChunk::AllColumns(out))
        }
        IntrospectScope::Detail { schema, name, kind } => {
            detail(conn, &schema, &name, kind).map(|d| CatalogChunk::Detail(Box::new(d)))
        }
    }
}

/// The stored `CREATE` text of `name`.
fn definition(conn: &Connection, schema: &str, name: &str) -> Result<Option<String>> {
    let sql = format!(
        "SELECT sql FROM {}.sqlite_schema WHERE name = ?1",
        quote(schema)
    );
    Ok(query(conn, &sql, [name], |r| text(r, 0))?
        .into_iter()
        .next())
}

fn detail(conn: &Connection, schema: &str, name: &str, kind: ObjectKind) -> Result<ObjectDetail> {
    let mut ddl = definition(conn, schema, name)?
        .ok_or_else(|| DbError::Unsupported(format!("{name} no longer exists")))?;
    let columns = columns(conn, schema, name)?;

    let master = quote(schema);
    let index_sql = format!(
        "SELECT il.name, il.\"unique\", il.origin, ii.name, \
         (SELECT sql FROM {master}.sqlite_schema WHERE name = il.name) \
         FROM pragma_index_list(?1, ?2) il JOIN pragma_index_info(il.name, ?2) ii \
         ORDER BY il.name, ii.seqno"
    );
    let rows = query(conn, &index_sql, params![name, schema], |r| {
        Ok((
            text(r, 0)?,
            r.get::<_, i64>(1)? != 0,
            text(r, 2)?,
            text(r, 3)?,
            text(r, 4)?,
        ))
    })?;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for (iname, unique, origin, col, def) in rows {
        match indexes.iter_mut().find(|i| i.name == iname) {
            Some(i) => i.columns.push(col),
            None => {
                if !def.is_empty() {
                    ddl.push_str(";\n");
                    ddl.push_str(&def);
                }
                indexes.push(IndexInfo {
                    is_unique: unique,
                    is_primary: origin == "pk",
                    definition: if def.is_empty() {
                        format!("automatic index for {origin}")
                    } else {
                        def
                    },
                    method: None,
                    name: iname,
                    columns: vec![col],
                });
            }
        }
    }

    let fk_rows = query(
        conn,
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
         FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        params![name, schema],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                text(r, 1)?,
                text(r, 2)?,
                text(r, 3)?,
                text(r, 4)?,
                text(r, 5)?,
            ))
        },
    )?;
    let mut foreign_keys: Vec<(i64, ForeignKeyInfo)> = Vec::new();
    for (id, table, from, to, on_update, on_delete) in fk_rows {
        match foreign_keys.iter_mut().find(|(i, _)| *i == id) {
            Some((_, f)) => {
                f.columns.push(from);
                f.referenced_columns.push(to);
            }
            None => foreign_keys.push((
                id,
                ForeignKeyInfo {
                    name: format!("fk_{name}_{id}"),
                    columns: vec![from],
                    references: format!("{schema}.{table}"),
                    referenced_columns: vec![to],
                    on_delete: (!on_delete.is_empty()).then_some(on_delete),
                    on_update: (!on_update.is_empty()).then_some(on_update),
                },
            )),
        }
    }

    let trigger_sql = format!(
        "SELECT name, sql FROM {master}.sqlite_schema \
         WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name"
    );
    let mut triggers = Vec::new();
    let mut trigger_details = Vec::new();
    for (tname, sql) in query(conn, &trigger_sql, [name], |r| {
        Ok((text(r, 0)?, text(r, 1)?))
    })? {
        let (timing, event) = trigger_timing(&sql);
        ddl.push_str(";\n");
        ddl.push_str(&sql);
        triggers.push(tname.clone());
        trigger_details.push(TriggerInfo {
            name: tname,
            timing,
            event,
            definition: sql,
        });
    }
    if !ddl.is_empty() {
        ddl.push(';');
    }

    Ok(ObjectDetail {
        object: object(schema, name.to_owned(), kind),
        columns,
        indexes,
        constraints: Vec::new(),
        foreign_keys: foreign_keys.into_iter().map(|(_, f)| f).collect(),
        triggers,
        ddl,
        size_bytes: None,
        comment: None,
        trigger_details,
    })
}
