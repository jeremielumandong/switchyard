//! Snowflake catalog from the current database's `INFORMATION_SCHEMA` and `GET_DDL`.

use super::SnowflakeSession;
use crate::catalog::{
    CatalogChunk, ColumnInfo, Dependencies, DependencyInfo, IntrospectScope, ObjectDetail,
    ObjectInfo, ObjectKind, SchemaInfo, dependency_kind, like_contains, search_hit,
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
        ObjectKind::Stage => {
            return Some(
                "SELECT STAGE_NAME AS TABLE_NAME, NULL AS ROW_COUNT, \
                 LOWER(STAGE_TYPE) || COALESCE(' · ' || STAGE_URL, '') AS COMMENT \
                 FROM INFORMATION_SCHEMA.STAGES WHERE STAGE_SCHEMA = ? ORDER BY STAGE_NAME"
                    .into(),
            );
        }
        ObjectKind::Pipe => {
            return Some(
                "SELECT PIPE_NAME AS TABLE_NAME, NULL AS ROW_COUNT, \
                 CASE WHEN IS_AUTOINGEST_ENABLED = 'YES' THEN 'auto-ingest' ELSE 'manual' END \
                 || COALESCE(' · ' || COMMENT, '') AS COMMENT \
                 FROM INFORMATION_SCHEMA.PIPES WHERE PIPE_SCHEMA = ? ORDER BY PIPE_NAME"
                    .into(),
            );
        }
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
        comment: r.get(row, "COMMENT").map(str::to_owned),
    }
}

const COLUMNS: &str = "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, \
                       COLUMN_DEFAULT, ORDINAL_POSITION, COMMENT FROM INFORMATION_SCHEMA.COLUMNS";

/// Size, row count and comment of one table or view (`?` schema, `?` name). Views
/// have NULL bytes and rows.
const TABLE_PROPS: &str = "SELECT BYTES, ROW_COUNT, COMMENT FROM INFORMATION_SCHEMA.TABLES \
                           WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

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
           UNION ALL \
           SELECT STAGE_SCHEMA, STAGE_NAME, 'stage' \
           FROM INFORMATION_SCHEMA.STAGES WHERE STAGE_NAME ILIKE ? ESCAPE '!' \
           UNION ALL \
           SELECT PIPE_SCHEMA, PIPE_NAME, 'pipe' \
           FROM INFORMATION_SCHEMA.PIPES WHERE PIPE_NAME ILIKE ? ESCAPE '!' \
         ) GROUP BY SCHEMA_NAME, OBJECT_NAME, KIND \
         ORDER BY LENGTH(OBJECT_NAME), OBJECT_NAME, SCHEMA_NAME LIMIT {limit}"
    )
}

/// Tasks of one schema (DBX-5c; Snowflake has no `INFORMATION_SCHEMA` view of them).
fn tasks_sql(schema: &str) -> String {
    format!("SHOW TASKS IN SCHEMA \"{}\"", schema.replace('"', "\"\""))
}

/// A task's tree line: `started · USING CRON 0 3 * * * UTC`.
fn task_summary(state: &str, schedule: Option<&str>, predecessors: Option<&str>) -> String {
    let mut parts = vec![state.to_ascii_lowercase()];
    if let Some(s) = schedule.filter(|s| !s.is_empty()) {
        parts.push(s.to_owned());
    } else if predecessors.is_some_and(|p| !p.is_empty() && p != "[]") {
        parts.push("after predecessors".into());
    }
    parts.join(" · ")
}

/// `SHOW ROLES` / `SHOW USERS` narrowed to one name (`LIKE` is a pattern; callers keep
/// only the exact name).
fn show_like(what: &str, name: &str) -> String {
    format!("SHOW {what} LIKE '{}'", name.replace('\'', "''"))
}

/// One stage's description from `INFORMATION_SCHEMA.STAGES` (`?` schema, `?` name).
const STAGE_SQL: &str = "SELECT STAGE_TYPE, STAGE_URL, STAGE_REGION, STORAGE_INTEGRATION, \
     STAGE_OWNER, COMMENT, CREATED FROM INFORMATION_SCHEMA.STAGES \
     WHERE STAGE_SCHEMA = ? AND STAGE_NAME = ?";

/// Dependencies (DBX-5a) from `SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES`: `?` schema
/// and `?` name twice. Objects in another database show as `DB.SCHEMA` (`OTHER_DB`).
const DEPENDENCIES_SQL: &str = "SELECT 'uses' AS DIRECTION, \
       REFERENCED_DATABASE <> CURRENT_DATABASE() AS OTHER_DB, \
       IFF(REFERENCED_DATABASE = CURRENT_DATABASE(), REFERENCED_SCHEMA, \
           REFERENCED_DATABASE || '.' || REFERENCED_SCHEMA) AS SCHEMA_NAME, \
       REFERENCED_OBJECT_NAME AS OBJECT_NAME, REFERENCED_OBJECT_DOMAIN AS DOMAIN, \
       DEPENDENCY_TYPE \
     FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES \
     WHERE REFERENCING_DATABASE = CURRENT_DATABASE() AND REFERENCING_SCHEMA = ? \
       AND REFERENCING_OBJECT_NAME = ? \
     UNION ALL \
     SELECT 'used_by', REFERENCING_DATABASE <> CURRENT_DATABASE(), \
       IFF(REFERENCING_DATABASE = CURRENT_DATABASE(), REFERENCING_SCHEMA, \
           REFERENCING_DATABASE || '.' || REFERENCING_SCHEMA), \
       REFERENCING_OBJECT_NAME, REFERENCING_OBJECT_DOMAIN, DEPENDENCY_TYPE \
     FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES \
     WHERE REFERENCED_DATABASE = CURRENT_DATABASE() AND REFERENCED_SCHEMA = ? \
       AND REFERENCED_OBJECT_NAME = ? \
     ORDER BY 1 DESC, 5, 3, 4";

/// Shown with every Snowflake dependency answer.
const DEPENDENCIES_LATENCY: &str =
    "From SNOWFLAKE.ACCOUNT_USAGE, which lags up to 3 hours: recent changes may be missing";

/// Shown when `ACCOUNT_USAGE` cannot be read.
const DEPENDENCIES_PRIVILEGE: &str = "Dependencies need IMPORTED PRIVILEGES on the SNOWFLAKE \
     database (SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES)";

/// `-- column: value` lines of one `SHOW` / `INFORMATION_SCHEMA` row (empty values left out).
fn described(title: &str, r: &Rows, row: &[Option<String>]) -> String {
    let mut out = format!("-- {title}\n");
    for (name, v) in r.names.iter().zip(row) {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty() && *v != "null") {
            out.push_str(&format!(
                "-- {}: {}\n",
                name.to_ascii_lowercase(),
                v.replace('\n', " ")
            ));
        }
    }
    out
}

/// Users and roles: `SHOW ROLES`, then `SHOW USERS` when permitted.
async fn roles(s: &SnowflakeSession) -> Result<Vec<ObjectInfo>> {
    let r = Rows::of(s, "SHOW ROLES", &[]).await?;
    let mut out: Vec<ObjectInfo> = r
        .rows
        .iter()
        .map(|row| ObjectInfo {
            schema: String::new(),
            name: r.text(row, "name"),
            kind: ObjectKind::Role,
            estimated_rows: None,
            detail: Some(match r.get(row, "comment").filter(|c| !c.is_empty()) {
                Some(c) => format!("role · {c}"),
                None => "role".into(),
            }),
        })
        .collect();
    // SHOW USERS needs MANAGE GRANTS (or ownership); without it only roles show.
    if let Ok(u) = Rows::of(s, "SHOW USERS", &[]).await {
        out.extend(u.rows.iter().map(|row| ObjectInfo {
            schema: String::new(),
            name: u.text(row, "name"),
            kind: ObjectKind::Role,
            estimated_rows: None,
            detail: Some(if u.get(row, "disabled") == Some("true") {
                "user · disabled".into()
            } else {
                "user".into()
            }),
        }));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// `Detail` of a role, stage, task or pipe (DBX-5c): `GET_DDL` for tasks and pipes, the
/// `SHOW` / `INFORMATION_SCHEMA` row as text for the others.
async fn admin_detail(
    s: &SnowflakeSession,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let gone = || DbError::Unsupported(format!("{name} no longer exists"));
    let ddl = match kind {
        ObjectKind::Task | ObjectKind::Pipe => {
            let ty = if kind == ObjectKind::Task {
                "TASK"
            } else {
                "PIPE"
            };
            let d = Rows::of(
                s,
                "SELECT GET_DDL(?, ?) AS DDL",
                &[Value::Text(ty.into()), Value::Text(quoted(schema, name))],
            )
            .await?;
            d.rows
                .first()
                .map(|row| d.text(row, "DDL"))
                .ok_or_else(gone)?
        }
        ObjectKind::Stage => {
            let r = Rows::of(
                s,
                STAGE_SQL,
                &[Value::Text(schema.into()), Value::Text(name.into())],
            )
            .await?;
            let row = r.rows.first().ok_or_else(gone)?;
            described(&format!("Stage {}", quoted(schema, name)), &r, row)
        }
        _ => {
            let mut found = None;
            for (what, title) in [("ROLES", "Role"), ("USERS", "User")] {
                let Ok(r) = Rows::of(s, &show_like(what, name), &[]).await else {
                    continue;
                };
                if let Some(row) = r.rows.iter().find(|row| r.text(row, "name") == name) {
                    found = Some(described(&format!("{title} {name}"), &r, row));
                    break;
                }
            }
            found.ok_or_else(gone)?
        }
    };
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
        object: ObjectInfo {
            schema: if kind.is_server_level() {
                String::new()
            } else {
                schema.to_owned()
            },
            name: name.to_owned(),
            kind,
            estimated_rows: None,
            detail: None,
        },
        ddl,
        ..ObjectDetail::default()
    })))
}

/// [`IntrospectScope::Dependencies`]: always with a hint (latency, or the privilege).
async fn dependencies(s: &SnowflakeSession, schema: &str, name: &str) -> CatalogChunk {
    let (schema, name) = (Value::Text(schema.into()), Value::Text(name.into()));
    let params = [schema.clone(), name.clone(), schema, name];
    let deps = match Rows::of(s, DEPENDENCIES_SQL, &params).await {
        Ok(r) => {
            let mut deps = Dependencies::hint(DEPENDENCIES_LATENCY);
            for row in &r.rows {
                let domain = r.text(row, "DOMAIN");
                let other_db = r.get(row, "OTHER_DB") == Some("true");
                deps.push(
                    &r.text(row, "DIRECTION"),
                    DependencyInfo {
                        schema: r.text(row, "SCHEMA_NAME"),
                        name: r.text(row, "OBJECT_NAME"),
                        kind: (!other_db).then(|| dependency_kind(&domain)).flatten(),
                        type_label: domain.to_ascii_lowercase(),
                        dependency: r.text(row, "DEPENDENCY_TYPE").to_ascii_lowercase(),
                    },
                );
            }
            deps
        }
        Err(e) => {
            tracing::debug!(error = %e, "account usage unavailable");
            Dependencies::hint(DEPENDENCIES_PRIVILEGE)
        }
    };
    CatalogChunk::Dependencies(Box::new(deps))
}

/// `"schema"."name"`, quoted exactly as stored.
fn quoted(schema: &str, name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema.replace('"', "\"\""),
        name.replace('"', "\"\"")
    )
}

/// Script as CREATE / EXEC: the argument signature `(A NUMBER, B VARCHAR)` of a function
/// or procedure (bound to `?` schema, `?` name; first overload).
fn routine_signature_sql(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Procedure => {
            "SELECT ARGUMENT_SIGNATURE FROM INFORMATION_SCHEMA.PROCEDURES \
             WHERE PROCEDURE_SCHEMA = ? AND PROCEDURE_NAME = ? ORDER BY ARGUMENT_SIGNATURE LIMIT 1"
        }
        _ => {
            "SELECT ARGUMENT_SIGNATURE FROM INFORMATION_SCHEMA.FUNCTIONS \
             WHERE FUNCTION_SCHEMA = ? AND FUNCTION_NAME = ? ORDER BY ARGUMENT_SIGNATURE LIMIT 1"
        }
    }
}

/// `(A NUMBER, B VARCHAR)` as `[("A", "NUMBER"), ("B", "VARCHAR")]`; commas inside a
/// type's parentheses (`NUMBER(38,0)`) do not split.
fn parse_signature(signature: &str) -> Vec<(String, String)> {
    let inner = signature
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or("");
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once(char::is_whitespace) {
            Some((name, ty)) => (name.to_owned(), ty.trim().to_owned()),
            None => (String::new(), p.to_owned()),
        })
        .collect()
}

/// [`IntrospectScope::RoutineDefinition`]: the signature, then `GET_DDL` of
/// `"schema"."name"(types)`.
async fn routine_definition(
    s: &SnowflakeSession,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let sig = Rows::of(
        s,
        routine_signature_sql(kind),
        &[Value::Text(schema.into()), Value::Text(name.into())],
    )
    .await?;
    let Some(row) = sig.rows.first() else {
        return Err(DbError::Unsupported(format!("{name} no longer exists")));
    };
    let params = parse_signature(&sig.text(row, "ARGUMENT_SIGNATURE"));
    let types: Vec<&str> = params.iter().map(|(_, t)| t.as_str()).collect();
    let object_type = if kind == ObjectKind::Procedure {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    let d = Rows::of(
        s,
        "SELECT GET_DDL(?, ?) AS DDL",
        &[
            Value::Text(object_type.into()),
            Value::Text(format!("{}({})", quoted(schema, name), types.join(", "))),
        ],
    )
    .await?;
    let ddl = d
        .rows
        .first()
        .map(|row| d.text(row, "DDL"))
        .unwrap_or_default();
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema, name, kind, ddl, params,
    ))))
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
        IntrospectScope::Objects {
            kind: ObjectKind::Role,
            ..
        } => Ok(CatalogChunk::Objects(roles(s).await?)),
        IntrospectScope::Objects {
            schema,
            kind: ObjectKind::Task,
        } => {
            let r = Rows::of(s, &tasks_sql(&schema), &[]).await?;
            Ok(CatalogChunk::Objects(
                r.rows
                    .iter()
                    .map(|row| ObjectInfo {
                        schema: schema.clone(),
                        name: r.text(row, "name"),
                        kind: ObjectKind::Task,
                        estimated_rows: None,
                        detail: Some(task_summary(
                            &r.text(row, "state"),
                            r.get(row, "schedule"),
                            r.get(row, "predecessors"),
                        )),
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Detail { schema, name, kind } if kind.is_admin() => {
            admin_detail(s, &schema, &name, kind).await
        }
        IntrospectScope::Dependencies { schema, name, .. } => {
            Ok(dependencies(s, &schema, &name).await)
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
                &[like.clone(), like.clone(), like.clone(), like.clone(), like],
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
        IntrospectScope::RoutineDefinition {
            schema, name, kind, ..
        } => routine_definition(s, &schema, &name, kind).await,
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
            // Extras: an error leaves them unknown.
            let props = Rows::of(
                s,
                TABLE_PROPS,
                &[Value::Text(schema.clone()), Value::Text(name.clone())],
            )
            .await
            .ok();
            let prop = |col: &str| {
                props
                    .as_ref()
                    .and_then(|p| p.rows.first().and_then(|row| p.get(row, col)))
                    .map(str::to_owned)
            };
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    estimated_rows: prop("ROW_COUNT").and_then(|v| v.parse().ok()),
                    schema,
                    name,
                    kind,
                    detail: None,
                },
                columns,
                ddl,
                size_bytes: prop("BYTES").and_then(|v| v.parse().ok()),
                comment: prop("COMMENT").filter(|c| !c.is_empty()),
                ..ObjectDetail::default()
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
    fn admin_sql_snapshots() {
        let objects: Vec<String> = [ObjectKind::Stage, ObjectKind::Pipe]
            .into_iter()
            .map(|k| format!("{k:?}: {}", objects_sql(k).unwrap_or_default()))
            .collect();
        insta::assert_snapshot!(
            "snowflake_admin_sql",
            [
                objects.join("\n"),
                tasks_sql("ETL \"x\""),
                show_like("ROLES", "o'brien"),
                STAGE_SQL.into(),
            ]
            .join("\n")
        );
        insta::assert_snapshot!("snowflake_dependencies_sql", DEPENDENCIES_SQL);
    }

    #[test]
    fn task_lines() {
        assert_eq!(
            task_summary("started", Some("USING CRON 0 3 * * * UTC"), None),
            "started · USING CRON 0 3 * * * UTC"
        );
        assert_eq!(
            task_summary("suspended", None, Some("[\"DB.S.ROOT\"]")),
            "suspended · after predecessors"
        );
        assert_eq!(task_summary("STARTED", Some(""), Some("[]")), "started");
    }

    #[test]
    fn search_sql_snapshot() {
        insta::assert_snapshot!("snowflake_search_sql", search_sql(200, false));
    }

    #[test]
    fn routine_signature_sql_snapshot() {
        insta::assert_snapshot!(
            "snowflake_routine_signature_sql",
            format!(
                "{}\n{}",
                routine_signature_sql(ObjectKind::Function),
                routine_signature_sql(ObjectKind::Procedure)
            )
        );
    }

    #[test]
    fn signatures_split_on_top_level_commas() {
        assert_eq!(
            parse_signature("(A NUMBER(38,0), B VARCHAR)"),
            [
                ("A".to_owned(), "NUMBER(38,0)".to_owned()),
                ("B".to_owned(), "VARCHAR".to_owned())
            ]
        );
        assert!(parse_signature("()").is_empty());
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn detail_sql_snapshots() {
        insta::assert_snapshot!("snowflake_columns_sql", COLUMNS);
        insta::assert_snapshot!("snowflake_table_props_sql", TABLE_PROPS);
    }
}
