//! PostgreSQL catalog queries. Each scope is one round trip so tree levels load lazily.

use tokio_postgres::Client;

use super::map_error;
use crate::catalog::{
    CatalogChunk, ColumnInfo, ConstraintInfo, ForeignKeyInfo, IndexInfo, IntrospectScope,
    ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, TriggerInfo, like_contains, search_hit,
};
use crate::dialect::{Dialect, postgres::PostgresDialect};
use crate::error::Result;

/// Schemas, user schemas first.
pub const SCHEMAS_SQL: &str = "\
SELECT n.nspname::text,
       (n.nspname LIKE 'pg\\_%' OR n.nspname = 'information_schema') AS is_system
FROM pg_catalog.pg_namespace n
WHERE n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'
ORDER BY is_system, n.nspname";

/// Relations of one relkind set in a schema, with estimated rows.
/// Extension members (e.g. `pg_stat_statements`) are left out; they belong to the extension.
pub const RELATIONS_SQL: &str = "\
SELECT c.relname::text,
       CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::int8 END AS est_rows
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relkind = ANY($2::text::\"char\"[])
  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                  WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = c.oid
                    AND d.deptype = 'e')
ORDER BY c.relname";

/// Functions or procedures in a schema.
pub const ROUTINES_SQL: &str = "\
SELECT p.proname::text, pg_catalog.pg_get_function_identity_arguments(p.oid) AS args
FROM pg_catalog.pg_proc p
JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = $1 AND p.prokind = $2::text::\"char\"
  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                  WHERE d.classid = 'pg_catalog.pg_proc'::regclass AND d.objid = p.oid
                    AND d.deptype = 'e')
ORDER BY p.proname, args";

/// User-defined types in a schema (excluding table row types and arrays).
pub const TYPES_SQL: &str = "\
SELECT t.typname::text
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = $1
  AND t.typtype IN ('c', 'e', 'd', 'r', 'm', 'b')
  AND (t.typrelid = 0
       OR (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid) = 'c')
  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_type el
                  WHERE el.oid = t.typelem AND el.typarray = t.oid)
  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                  WHERE d.classid = 'pg_catalog.pg_type'::regclass AND d.objid = t.oid
                    AND d.deptype = 'e')
ORDER BY 1";

/// Columns of one relation.
pub const COLUMNS_SQL: &str = "\
SELECT n.nspname::text, c.relname::text, a.attname::text,
       pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
       NOT a.attnotnull AS nullable,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr,
       a.attnum::int4,
       EXISTS (SELECT 1 FROM pg_catalog.pg_index i
               WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)) AS is_pk,
       pg_catalog.col_description(c.oid, a.attnum) AS comment
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
WHERE a.attnum > 0 AND NOT a.attisdropped
  AND ($1::text IS NULL OR n.nspname = $1)
  AND ($2::text IS NULL OR c.relname = $2)
  AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
ORDER BY n.nspname, c.relname, a.attnum";

/// Indexes of one table.
pub const INDEXES_SQL: &str = "\
SELECT ic.relname::text, i.indisunique, i.indisprimary, pg_catalog.pg_get_indexdef(i.indexrelid),
       ARRAY(SELECT pg_catalog.pg_get_indexdef(i.indexrelid, k, true)
             FROM generate_subscripts(i.indkey, 1) AS k ORDER BY k)::text[],
       am.amname::text
FROM pg_catalog.pg_index i
JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
LEFT JOIN pg_catalog.pg_am am ON am.oid = ic.relam
JOIN pg_catalog.pg_class c ON c.oid = i.indrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = $2
ORDER BY i.indisprimary DESC, ic.relname";

/// Constraints of one table.
pub const CONSTRAINTS_SQL: &str = "\
SELECT con.conname::text, con.contype::text, pg_catalog.pg_get_constraintdef(con.oid, true),
       ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n, o)
             JOIN pg_catalog.pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n
             ORDER BY k.o)::text[],
       fn.nspname::text, fc.relname::text,
       ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n, o)
             JOIN pg_catalog.pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n
             ORDER BY k.o)::text[],
       con.confdeltype::text, con.confupdtype::text
FROM pg_catalog.pg_constraint con
JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_class fc ON fc.oid = con.confrelid
LEFT JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace
WHERE n.nspname = $1 AND c.relname = $2
ORDER BY con.contype, con.conname";

/// Triggers of one table or view: name, `tgtype` bits and definition.
pub const TRIGGERS_SQL: &str = "\
SELECT t.tgname::text, t.tgtype::int4, pg_catalog.pg_get_triggerdef(t.oid, true)
FROM pg_catalog.pg_trigger t
JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal
ORDER BY 1";

/// Size on disk (table, indexes and TOAST), comment and estimated rows of one relation.
/// Run on its own so a failure (a relation dropped meanwhile) only loses these values.
pub const RELATION_PROPS_SQL: &str = "\
SELECT pg_catalog.pg_total_relation_size(c.oid),
       pg_catalog.obj_description(c.oid, 'pg_class'),
       CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::int8 END
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = $2";

/// Definition of a view or materialized view.
pub const VIEW_DEF_SQL: &str = "\
SELECT pg_catalog.pg_get_viewdef(c.oid, true)
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = $2";

/// Definition of a function or procedure.
pub const ROUTINE_DEF_SQL: &str = "\
SELECT pg_catalog.pg_get_functiondef(p.oid)
FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = $1 AND p.proname = $2
ORDER BY p.oid LIMIT 1";

/// Script as CREATE / EXEC: the definition and input parameters of one routine
/// (`$3` = its identity arguments, or NULL for the first overload). One row per input
/// parameter; a routine without any gives one row with NULL parameter columns.
pub const ROUTINE_DEFINITION_SQL: &str = "\
WITH r AS (
  SELECT p.oid, p.proallargtypes, p.proargtypes, p.proargnames, p.proargmodes
  FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
  WHERE n.nspname = $1 AND p.proname = $2
    AND ($3::text IS NULL OR pg_catalog.pg_get_function_identity_arguments(p.oid) = $3)
  ORDER BY p.oid LIMIT 1
)
SELECT pg_catalog.pg_get_functiondef(r.oid), a.name::text,
       pg_catalog.format_type(a.type, NULL)
FROM r
LEFT JOIN LATERAL unnest(COALESCE(r.proallargtypes, r.proargtypes::oid[]), r.proargnames,
                         r.proargmodes::text[]) WITH ORDINALITY AS a(type, name, mode, n)
       ON COALESCE(a.mode, 'i') IN ('i', 'b', 'v')
ORDER BY a.n";

/// Global object search: relations, sequences and routines whose name contains `$1`
/// (an escaped LIKE pattern), shortest names first. `$3` includes system schemas.
pub const SEARCH_SQL: &str = "\
SELECT s.schema_name, s.object_name, s.kind FROM (
  SELECT n.nspname::text AS schema_name, c.relname::text AS object_name,
         CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'mview' WHEN 'S' THEN 'sequence'
                        ELSE 'table' END AS kind
  FROM pg_catalog.pg_class c
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') AND c.relname ILIKE $1 ESCAPE '!'
  UNION ALL
  SELECT n.nspname::text, p.proname::text,
         CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END
  FROM pg_catalog.pg_proc p
  JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
  WHERE p.prokind IN ('f', 'p') AND p.proname ILIKE $1 ESCAPE '!'
) s
WHERE s.schema_name NOT LIKE 'pg\\_toast%' AND s.schema_name NOT LIKE 'pg\\_temp%'
  AND ($3::bool OR s.schema_name NOT IN ('pg_catalog', 'information_schema'))
GROUP BY s.schema_name, s.object_name, s.kind
ORDER BY length(s.object_name), s.object_name, s.schema_name
LIMIT $2::int8";

/// Timing and events of a trigger from its `pg_trigger.tgtype` bits
/// (row 1, before 2, insert 4, delete 8, update 16, truncate 32, instead 64).
pub fn trigger_timing(tgtype: i32) -> (String, String) {
    let when = if tgtype & 64 != 0 {
        "INSTEAD OF"
    } else if tgtype & 2 != 0 {
        "BEFORE"
    } else {
        "AFTER"
    };
    let level = if tgtype & 1 != 0 {
        "FOR EACH ROW"
    } else {
        "FOR EACH STATEMENT"
    };
    let events: Vec<&str> = [
        (4, "INSERT"),
        (16, "UPDATE"),
        (8, "DELETE"),
        (32, "TRUNCATE"),
    ]
    .into_iter()
    .filter(|(bit, _)| tgtype & bit != 0)
    .map(|(_, e)| e)
    .collect();
    (format!("{when} {level}"), events.join(" OR "))
}

/// A foreign key action from `pg_constraint.confdeltype` / `confupdtype`.
pub fn fk_action(code: &str) -> Option<String> {
    Some(
        match code {
            "a" => "NO ACTION",
            "r" => "RESTRICT",
            "c" => "CASCADE",
            "n" => "SET NULL",
            "d" => "SET DEFAULT",
            _ => return None,
        }
        .into(),
    )
}

fn relkinds(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "{r,p,f}",
        ObjectKind::View => "{v}",
        ObjectKind::MaterializedView => "{m}",
        ObjectKind::Sequence => "{S}",
        _ => "{}",
    }
}

/// Generate CREATE TABLE DDL from catalog detail.
pub fn table_ddl(
    schema: &str,
    name: &str,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    let d = PostgresDialect;
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut s = format!("    {} {}", d.quote_ident(&c.name), c.data_type);
            if let Some(def) = &c.default {
                s.push_str(&format!(" DEFAULT {def}"));
            }
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    for con in constraints {
        lines.push(format!(
            "    CONSTRAINT {} {}",
            d.quote_ident(&con.name),
            con.definition
        ));
    }
    let mut ddl = format!(
        "CREATE TABLE {} (\n{}\n);\n",
        d.qualified(schema, name),
        lines.join(",\n")
    );
    let constraint_names: Vec<&str> = constraints.iter().map(|c| c.name.as_str()).collect();
    for ix in indexes
        .iter()
        .filter(|i| !constraint_names.contains(&i.name.as_str()))
    {
        ddl.push_str(&ix.definition);
        ddl.push_str(";\n");
    }
    ddl
}

/// Load one catalog scope.
pub async fn introspect(client: &Client, scope: IntrospectScope) -> Result<CatalogChunk> {
    let err = |e| map_error(e, false);
    match scope {
        IntrospectScope::Databases => {
            let rows = client
                .query(
                    "SELECT datname::text FROM pg_catalog.pg_database \
                     WHERE NOT datistemplate AND datallowconn ORDER BY 1",
                    &[],
                )
                .await
                .map_err(err)?;
            Ok(CatalogChunk::Databases(
                rows.iter().map(|r| r.get(0)).collect(),
            ))
        }
        IntrospectScope::Schemas => {
            let rows = client.query(SCHEMAS_SQL, &[]).await.map_err(err)?;
            Ok(CatalogChunk::Schemas(
                rows.iter()
                    .map(|r| SchemaInfo {
                        name: r.get(0),
                        is_system: r.get(1),
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Objects { schema, kind } => {
            let objects = match kind {
                ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::Sequence => {
                    let rows = client
                        .query(RELATIONS_SQL, &[&schema, &relkinds(kind)])
                        .await
                        .map_err(err)?;
                    rows.iter()
                        .map(|r| ObjectInfo {
                            schema: schema.clone(),
                            name: r.get(0),
                            kind,
                            estimated_rows: if kind == ObjectKind::Sequence {
                                None
                            } else {
                                r.get(1)
                            },
                            detail: None,
                        })
                        .collect()
                }
                ObjectKind::Function | ObjectKind::Procedure => {
                    let prokind = if kind == ObjectKind::Function {
                        "f"
                    } else {
                        "p"
                    };
                    let rows = client
                        .query(ROUTINES_SQL, &[&schema, &prokind])
                        .await
                        .map_err(err)?;
                    rows.iter()
                        .map(|r| ObjectInfo {
                            schema: schema.clone(),
                            name: r.get(0),
                            kind,
                            estimated_rows: None,
                            detail: Some(format!("({})", r.get::<_, String>(1))),
                        })
                        .collect()
                }
                ObjectKind::Type => {
                    let rows = client.query(TYPES_SQL, &[&schema]).await.map_err(err)?;
                    rows.iter()
                        .map(|r| ObjectInfo {
                            schema: schema.clone(),
                            name: r.get(0),
                            kind,
                            estimated_rows: None,
                            detail: None,
                        })
                        .collect()
                }
                ObjectKind::Synonym => Vec::new(),
            };
            Ok(CatalogChunk::Objects(objects))
        }
        IntrospectScope::Detail { schema, name, kind } => {
            let columns = load_columns(client, Some(&schema), Some(&name)).await?;
            let mut detail = ObjectDetail {
                object: ObjectInfo {
                    schema: schema.clone(),
                    name: name.clone(),
                    kind,
                    estimated_rows: None,
                    detail: None,
                },
                columns,
                ..ObjectDetail::default()
            };
            if kind.is_relation() {
                // Optional extras: an error here never fails the whole detail.
                if let Ok(Some(r)) = client
                    .query_opt(RELATION_PROPS_SQL, &[&schema, &name])
                    .await
                {
                    detail.size_bytes = if kind == ObjectKind::View {
                        None
                    } else {
                        r.get(0)
                    };
                    detail.comment = r.get(1);
                    detail.object.estimated_rows = if kind == ObjectKind::View {
                        None
                    } else {
                        r.get(2)
                    };
                }
            }
            if matches!(kind, ObjectKind::Table | ObjectKind::View) {
                let rows = client
                    .query(TRIGGERS_SQL, &[&schema, &name])
                    .await
                    .map_err(err)?;
                for r in &rows {
                    let (timing, event) = trigger_timing(r.get(1));
                    detail.triggers.push(r.get(0));
                    detail.trigger_details.push(TriggerInfo {
                        name: r.get(0),
                        timing,
                        event,
                        definition: r.get::<_, Option<String>>(2).unwrap_or_default(),
                    });
                }
            }
            let d = PostgresDialect;
            match kind {
                ObjectKind::Table => {
                    let rows = client
                        .query(INDEXES_SQL, &[&schema, &name])
                        .await
                        .map_err(err)?;
                    detail.indexes = rows
                        .iter()
                        .map(|r| IndexInfo {
                            name: r.get(0),
                            is_unique: r.get(1),
                            is_primary: r.get(2),
                            definition: r.get(3),
                            columns: r.get(4),
                            method: r.get(5),
                        })
                        .collect();
                    let rows = client
                        .query(CONSTRAINTS_SQL, &[&schema, &name])
                        .await
                        .map_err(err)?;
                    for r in &rows {
                        let contype: String = r.get(1);
                        if contype == "f" {
                            let fs: Option<String> = r.get(4);
                            let ft: Option<String> = r.get(5);
                            detail.foreign_keys.push(ForeignKeyInfo {
                                name: r.get(0),
                                columns: r.get(3),
                                references: format!(
                                    "{}.{}",
                                    fs.unwrap_or_default(),
                                    ft.unwrap_or_default()
                                ),
                                referenced_columns: r.get(6),
                                on_delete: fk_action(r.get::<_, &str>(7)),
                                on_update: fk_action(r.get::<_, &str>(8)),
                            });
                        }
                        detail.constraints.push(ConstraintInfo {
                            name: r.get(0),
                            kind: match contype.as_str() {
                                "p" => "PRIMARY KEY",
                                "u" => "UNIQUE",
                                "c" => "CHECK",
                                "f" => "FOREIGN KEY",
                                "x" => "EXCLUDE",
                                _ => "CONSTRAINT",
                            }
                            .into(),
                            definition: r.get(2),
                        });
                    }
                    detail.ddl = table_ddl(
                        &schema,
                        &name,
                        &detail.columns,
                        &detail.constraints,
                        &detail.indexes,
                    );
                }
                ObjectKind::View | ObjectKind::MaterializedView => {
                    let row = client
                        .query_opt(VIEW_DEF_SQL, &[&schema, &name])
                        .await
                        .map_err(err)?;
                    let def: String = row.map(|r| r.get(0)).unwrap_or_default();
                    let mat = if kind == ObjectKind::MaterializedView {
                        "MATERIALIZED "
                    } else {
                        ""
                    };
                    let create = if mat.is_empty() {
                        "CREATE OR REPLACE"
                    } else {
                        "CREATE"
                    };
                    detail.ddl = format!(
                        "{create} {mat}VIEW {} AS\n{}",
                        d.qualified(&schema, &name),
                        def.trim_end()
                    );
                }
                ObjectKind::Function | ObjectKind::Procedure => {
                    let row = client
                        .query_opt(ROUTINE_DEF_SQL, &[&schema, &name])
                        .await
                        .map_err(err)?;
                    detail.ddl = row.map(|r| r.get(0)).unwrap_or_default();
                }
                _ => {}
            }
            Ok(CatalogChunk::Detail(Box::new(detail)))
        }
        IntrospectScope::RoutineDefinition {
            schema,
            name,
            kind,
            signature,
        } => routine_definition(client, &schema, &name, kind, signature.as_deref()).await,
        IntrospectScope::AllColumns => Ok(CatalogChunk::AllColumns(
            load_columns(client, None, None).await?,
        )),
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let like = like_contains(&pattern, false);
            let rows = client
                .query(SEARCH_SQL, &[&like, &i64::from(limit), &include_system])
                .await
                .map_err(err)?;
            Ok(CatalogChunk::Objects(
                rows.iter()
                    .filter_map(|r| search_hit(r.get(0), r.get(1), r.get::<_, &str>(2)))
                    .collect(),
            ))
        }
    }
}

/// The identity arguments inside a tree signature `(…)`.
fn identity_args(signature: Option<&str>) -> Option<&str> {
    signature?.trim().strip_prefix('(')?.strip_suffix(')')
}

/// [`IntrospectScope::RoutineDefinition`]: `pg_get_functiondef` and the input parameters.
async fn routine_definition(
    client: &Client,
    schema: &str,
    name: &str,
    kind: ObjectKind,
    signature: Option<&str>,
) -> Result<CatalogChunk> {
    let args = identity_args(signature);
    let rows = client
        .query(ROUTINE_DEFINITION_SQL, &[&schema, &name, &args])
        .await
        .map_err(|e| map_error(e, false))?;
    let Some(first) = rows.first() else {
        return Err(crate::error::DbError::Unsupported(format!(
            "{schema}.{name} no longer exists"
        )));
    };
    let ddl: Option<String> = first.get(0);
    let params = rows
        .iter()
        .filter_map(|r| {
            let ty: Option<String> = r.get(2);
            ty.map(|ty| (r.get::<_, Option<String>>(1).unwrap_or_default(), ty))
        })
        .collect();
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema,
        name,
        kind,
        ddl.unwrap_or_default(),
        params,
    ))))
}

async fn load_columns(
    client: &Client,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<ColumnInfo>> {
    let rows = client
        .query(COLUMNS_SQL, &[&schema, &table])
        .await
        .map_err(|e| map_error(e, false))?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: r.get(0),
            table: r.get(1),
            name: r.get(2),
            data_type: r.get(3),
            nullable: r.get(4),
            default: r.get(5),
            ordinal: r.get(6),
            is_primary_key: r.get(7),
            comment: r.get(8),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_sql_snapshot() {
        insta::assert_snapshot!("pg_search_sql", SEARCH_SQL);
    }

    #[test]
    fn routine_definition_sql_snapshot() {
        insta::assert_snapshot!("pg_routine_definition_sql", ROUTINE_DEFINITION_SQL);
    }

    #[test]
    fn identity_args_from_the_tree_signature() {
        assert_eq!(identity_args(Some("(cid bigint)")), Some("cid bigint"));
        assert_eq!(identity_args(Some("()")), Some(""));
        assert_eq!(identity_args(None), None);
        assert_eq!(identity_args(Some("SQL_SCALAR_FUNCTION")), None);
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn detail_sql_snapshots() {
        insta::assert_snapshot!("pg_columns_sql", COLUMNS_SQL);
        insta::assert_snapshot!("pg_indexes_sql", INDEXES_SQL);
        insta::assert_snapshot!("pg_constraints_sql", CONSTRAINTS_SQL);
        insta::assert_snapshot!("pg_triggers_sql", TRIGGERS_SQL);
        insta::assert_snapshot!("pg_relation_props_sql", RELATION_PROPS_SQL);
    }

    #[test]
    fn trigger_bits_decode() {
        // BEFORE INSERT OR UPDATE FOR EACH ROW
        assert_eq!(
            trigger_timing(1 | 2 | 4 | 16),
            ("BEFORE FOR EACH ROW".into(), "INSERT OR UPDATE".into())
        );
        assert_eq!(
            trigger_timing(8 | 32),
            (
                "AFTER FOR EACH STATEMENT".into(),
                "DELETE OR TRUNCATE".into()
            )
        );
        assert_eq!(trigger_timing(1 | 64 | 4).0, "INSTEAD OF FOR EACH ROW");
    }

    #[test]
    fn fk_actions_decode() {
        assert_eq!(fk_action("c").as_deref(), Some("CASCADE"));
        assert_eq!(fk_action("n").as_deref(), Some("SET NULL"));
        assert_eq!(fk_action("a").as_deref(), Some("NO ACTION"));
        assert_eq!(fk_action(" "), None);
    }
}
