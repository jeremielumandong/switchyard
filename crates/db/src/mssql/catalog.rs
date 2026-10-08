//! SQL Server catalog from the `sys` views of the current database.

use tiberius::Query;

use super::{TdsClient, decode, map_error, simple_rows};
use crate::catalog::{
    CatalogChunk, ColumnInfo, ConstraintInfo, Dependencies, DependencyInfo, ForeignKeyInfo,
    IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, TriggerInfo,
    dependency_kind, like_contains, search_hit,
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
        ObjectKind::Role => ROLES_SQL.to_owned(),
        _ => return None,
    })
}

/// Users and roles (DBX-5c): the database's principals (with their login and default
/// schema), then the server logins mapped to none of them. `sys.server_principals` shows
/// only what the user may see (their own login without `VIEW ANY DEFINITION`).
const ROLES_SQL: &str = "SELECT p.name COLLATE DATABASE_DEFAULT, NULL, \
       LOWER(REPLACE(p.type_desc, '_', ' ')) COLLATE DATABASE_DEFAULT \
       + CASE WHEN sp.name IS NOT NULL \
              THEN ' · login ' + sp.name COLLATE DATABASE_DEFAULT ELSE '' END \
       + CASE WHEN p.default_schema_name IS NOT NULL \
              THEN ' · schema ' + p.default_schema_name COLLATE DATABASE_DEFAULT ELSE '' END \
     FROM sys.database_principals p \
     LEFT JOIN sys.server_principals sp ON sp.sid = p.sid \
     WHERE p.type IN ('S','U','G','R','E','X','C','K') AND p.is_fixed_role = 0 \
       AND p.name NOT IN ('sys','INFORMATION_SCHEMA','guest','public') \
     UNION ALL \
     SELECT sp.name COLLATE DATABASE_DEFAULT, NULL, \
       'login · ' + LOWER(REPLACE(sp.type_desc, '_', ' ')) COLLATE DATABASE_DEFAULT \
       + CASE WHEN sp.is_disabled = 1 THEN ' · disabled' ELSE '' END \
     FROM sys.server_principals sp \
     WHERE sp.type IN ('S','U','G','E','X') AND sp.name NOT LIKE '##%' \
       AND NOT EXISTS (SELECT 1 FROM sys.database_principals p \
                       WHERE p.sid = sp.sid \
                          OR p.name = sp.name COLLATE DATABASE_DEFAULT) \
     ORDER BY 1";

/// One database principal (`@P1` name): type, type text, default schema, login.
const PRINCIPAL_SQL: &str = "SELECT p.type, p.type_desc, p.default_schema_name, \
       sp.name COLLATE DATABASE_DEFAULT \
     FROM sys.database_principals p LEFT JOIN sys.server_principals sp ON sp.sid = p.sid \
     WHERE p.name = @P1";

/// The database roles `@P1` is a member of.
const PRINCIPAL_ROLES_SQL: &str = "SELECT r.name FROM sys.database_role_members m \
     JOIN sys.database_principals r ON r.principal_id = m.role_principal_id \
     JOIN sys.database_principals u ON u.principal_id = m.member_principal_id \
     WHERE u.name = @P1 ORDER BY 1";

/// One server login (`@P1` name): type, type text, disabled, default database.
const LOGIN_SQL: &str = "SELECT sp.type, sp.type_desc, sp.is_disabled, sp.default_database_name \
     FROM sys.server_principals sp WHERE sp.name = @P1";

/// SQL Agent jobs (DBX-5c): enabled flag, last outcome (`sysjobhistory` step 0), next
/// scheduled run and schedule count. Needs msdb access (`SQLAgentReaderRole`).
const JOBS_SQL: &str = "SELECT j.name, j.enabled, h.run_status, h.run_date, h.run_time, \
       (SELECT MIN(CAST(s.next_run_date AS bigint) * 1000000 + s.next_run_time) \
        FROM msdb.dbo.sysjobschedules s WHERE s.job_id = j.job_id AND s.next_run_date > 0), \
       (SELECT COUNT(*) FROM msdb.dbo.sysjobschedules s WHERE s.job_id = j.job_id) \
     FROM msdb.dbo.sysjobs j \
     OUTER APPLY (SELECT TOP 1 jh.run_status, jh.run_date, jh.run_time \
                  FROM msdb.dbo.sysjobhistory jh \
                  WHERE jh.job_id = j.job_id AND jh.step_id = 0 \
                  ORDER BY jh.instance_id DESC) h \
     ORDER BY j.name";

/// One job (`@P1` name) with its steps, one row per step.
const JOB_DETAIL_SQL: &str = "SELECT j.enabled, j.description, SUSER_SNAME(j.owner_sid), \
       c.name, s.step_id, s.step_name, s.subsystem, s.database_name, s.command \
     FROM msdb.dbo.sysjobs j \
     LEFT JOIN msdb.dbo.syscategories c ON c.category_id = j.category_id \
     LEFT JOIN msdb.dbo.sysjobsteps s ON s.job_id = j.job_id \
     WHERE j.name = @P1 ORDER BY s.step_id";

/// The schedules of one job (`@P1` name): name and enabled flag.
const JOB_SCHEDULES_SQL: &str = "SELECT sc.name, sc.enabled FROM msdb.dbo.sysjobs j \
     JOIN msdb.dbo.sysjobschedules js ON js.job_id = j.job_id \
     JOIN msdb.dbo.sysschedules sc ON sc.schedule_id = js.schedule_id \
     WHERE j.name = @P1 ORDER BY sc.name";

/// The hint shown instead of the jobs when msdb cannot be read.
const JOBS_HINT: &str = "SQL Agent jobs need read access to msdb (SQLAgentReaderRole)";

/// Dependencies (DBX-5a) of the object `id`: `sys.sql_expression_dependencies` in both
/// directions plus foreign keys. Columns: direction, schema, name, type, link, database
/// of a cross-database reference.
fn dependencies_sql(id: &str) -> String {
    format!(
        "SELECT 'uses', COALESCE(d.referenced_schema_name, SCHEMA_NAME(o.schema_id), ''), \
                d.referenced_entity_name, COALESCE(o.type_desc, d.referenced_class_desc), \
                CASE WHEN d.is_schema_bound_reference = 1 THEN 'schema-bound' \
                     ELSE 'reference' END, d.referenced_database_name \
         FROM sys.sql_expression_dependencies d \
         LEFT JOIN sys.objects o ON o.object_id = d.referenced_id \
         WHERE d.referencing_id = {id} \
         UNION ALL \
         SELECT 'used_by', SCHEMA_NAME(o.schema_id), o.name, o.type_desc, \
                CASE WHEN d.is_schema_bound_reference = 1 THEN 'schema-bound' \
                     ELSE 'reference' END, NULL \
         FROM sys.sql_expression_dependencies d \
         JOIN sys.objects o ON o.object_id = d.referencing_id \
         WHERE d.referenced_id = {id} \
         UNION ALL \
         SELECT 'uses', SCHEMA_NAME(t.schema_id), t.name, t.type_desc, 'foreign key ' + fk.name, NULL \
         FROM sys.foreign_keys fk JOIN sys.objects t ON t.object_id = fk.referenced_object_id \
         WHERE fk.parent_object_id = {id} AND fk.referenced_object_id <> {id} \
         UNION ALL \
         SELECT 'used_by', SCHEMA_NAME(t.schema_id), t.name, t.type_desc, 'foreign key ' + fk.name, NULL \
         FROM sys.foreign_keys fk JOIN sys.objects t ON t.object_id = fk.parent_object_id \
         WHERE fk.referenced_object_id = {id} AND fk.parent_object_id <> {id} \
         ORDER BY 1 DESC, 4, 2, 3"
    )
}

/// `2026-10-01 03:00` from Agent's `yyyymmdd` date and `hhmmss` time integers.
fn agent_time(date: i64, time: i64) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        date / 10000,
        date / 100 % 100,
        date % 100,
        time / 10000,
        time / 100 % 100
    )
}

/// A job's tree line: `enabled · last run succeeded 2026-10-01 03:00 · next 2026-10-09
/// 03:00` (`status` is `sysjobhistory.run_status`, `next` is `yyyymmddhhmmss`).
fn job_summary(
    enabled: bool,
    status: Option<i64>,
    run: Option<(i64, i64)>,
    next: Option<i64>,
    schedules: i64,
) -> String {
    let mut parts = vec![if enabled { "enabled" } else { "disabled" }.to_owned()];
    match status {
        Some(s) => {
            let word = match s {
                0 => "failed",
                1 => "succeeded",
                2 => "retrying",
                3 => "canceled",
                4 => "in progress",
                _ => "unknown",
            };
            let at = run.map(|(d, t)| format!(" {}", agent_time(d, t)));
            parts.push(format!("last run {word}{}", at.unwrap_or_default()));
        }
        None => parts.push("never run".into()),
    }
    match next {
        Some(n) if n > 0 => {
            parts.push(format!("next {}", agent_time(n / 1_000_000, n % 1_000_000)))
        }
        _ if schedules == 0 => parts.push("no schedule".into()),
        _ => {}
    }
    parts.join(" · ")
}

/// `CREATE USER` / `CREATE ROLE` text of a database principal and its role memberships
/// (`kind` is `sys.database_principals.type`).
fn principal_ddl(
    name: &str,
    kind: &str,
    type_desc: &str,
    default_schema: Option<&str>,
    login: Option<&str>,
    roles: &[String],
) -> String {
    let who = quote(name);
    let schema = default_schema
        .map(|s| format!(" WITH DEFAULT_SCHEMA = {}", quote(s)))
        .unwrap_or_default();
    let mut ddl = format!("-- {type_desc}\n");
    ddl.push_str(&match kind.trim() {
        "R" => format!("CREATE ROLE {who};"),
        "A" => format!("CREATE APPLICATION ROLE {who} WITH PASSWORD = N'<password>';"),
        "E" | "X" => format!("CREATE USER {who} FROM EXTERNAL PROVIDER{schema};"),
        _ => match login {
            Some(l) => format!("CREATE USER {who} FOR LOGIN {}{schema};", quote(l)),
            None => format!("CREATE USER {who} WITHOUT LOGIN{schema};"),
        },
    });
    ddl.push('\n');
    for r in roles {
        ddl.push_str(&format!("ALTER ROLE {} ADD MEMBER {who};\n", quote(r)));
    }
    ddl
}

/// `CREATE LOGIN` text of a server login (the password is never scripted).
fn login_ddl(name: &str, kind: &str, type_desc: &str, disabled: bool, db: Option<&str>) -> String {
    let who = quote(name);
    let db = db
        .map(|d| format!("DEFAULT_DATABASE = {}", quote(d)))
        .unwrap_or_default();
    let mut ddl = format!(
        "-- Login ({type_desc}){}\n",
        if disabled { ", disabled" } else { "" }
    );
    ddl.push_str(&match kind.trim() {
        "U" | "G" => {
            let with = if db.is_empty() {
                String::new()
            } else {
                format!(" WITH {db}")
            };
            format!("CREATE LOGIN {who} FROM WINDOWS{with};")
        }
        "E" | "X" => format!("CREATE LOGIN {who} FROM EXTERNAL PROVIDER;"),
        _ => {
            let db = if db.is_empty() {
                String::new()
            } else {
                format!(", {db}")
            };
            format!("CREATE LOGIN {who} WITH PASSWORD = N'<password>'{db};")
        }
    });
    ddl.push('\n');
    if disabled {
        ddl.push_str(&format!("ALTER LOGIN {who} DISABLE;\n"));
    }
    ddl
}

/// [`IntrospectScope::Objects`] for Agent jobs, or a hint without msdb access.
async fn jobs(client: &mut TdsClient) -> Result<CatalogChunk> {
    let rows = match simple_rows(client, JOBS_SQL).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(error = %e, "msdb jobs unavailable");
            return Ok(CatalogChunk::Hint(JOBS_HINT.into()));
        }
    };
    let opt =
        |r: &[Value], i: usize| (!matches!(r.get(i), None | Some(Value::Null))).then(|| int(r, i));
    Ok(CatalogChunk::Objects(
        rows.iter()
            .map(|r| ObjectInfo {
                schema: String::new(),
                name: text(r, 0),
                kind: ObjectKind::Job,
                estimated_rows: None,
                detail: Some(job_summary(
                    int(r, 1) != 0,
                    opt(r, 2),
                    opt(r, 3).zip(opt(r, 4)),
                    opt(r, 5),
                    int(r, 6),
                )),
            })
            .collect(),
    ))
}

/// `Detail` of a user, role, login or Agent job (DBX-5c): its text in `ddl`.
async fn admin_detail(
    client: &mut TdsClient,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let key = [Value::Text(name.to_owned())];
    let ddl = if kind == ObjectKind::Job {
        let rows = match param_rows(client, JOB_DETAIL_SQL, &key).await {
            Ok(rows) => rows,
            Err(_) => return Ok(CatalogChunk::Hint(JOBS_HINT.into())),
        };
        let Some(first) = rows.first() else {
            return Err(DbError::Unsupported(format!("{name} no longer exists")));
        };
        let mut ddl = format!(
            "-- SQL Agent job {}\n-- {}\n",
            quote(name),
            if int(first, 0) != 0 {
                "Enabled"
            } else {
                "Disabled"
            }
        );
        for (label, i) in [("Owner", 2), ("Category", 3), ("Description", 1)] {
            if let Some(v) = opt_text(first, i).filter(|v| !v.is_empty()) {
                ddl.push_str(&format!("-- {label}: {}\n", v.replace('\n', "\n--   ")));
            }
        }
        let schedules = param_rows(client, JOB_SCHEDULES_SQL, &key)
            .await
            .unwrap_or_default();
        for s in &schedules {
            let off = if int(s, 1) != 0 { "" } else { " (disabled)" };
            ddl.push_str(&format!("-- Schedule: {}{off}\n", text(s, 0)));
        }
        for r in rows.iter().filter(|r| opt_text(r, 4).is_some()) {
            ddl.push_str(&format!(
                "\n-- Step {}: {} ({}{})\n{}\n",
                text(r, 4),
                text(r, 5),
                text(r, 6),
                opt_text(r, 7)
                    .map(|d| format!(", database {d}"))
                    .unwrap_or_default(),
                text(r, 8).trim_end()
            ));
        }
        ddl
    } else if let Some(p) = param_rows(client, PRINCIPAL_SQL, &key).await?.first() {
        let roles: Vec<String> = param_rows(client, PRINCIPAL_ROLES_SQL, &key)
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| text(r, 0))
            .collect();
        principal_ddl(
            name,
            &text(p, 0),
            &text(p, 1),
            opt_text(p, 2).as_deref(),
            opt_text(p, 3).as_deref(),
            &roles,
        )
    } else if let Some(l) = param_rows(client, LOGIN_SQL, &key).await?.first() {
        login_ddl(
            name,
            &text(l, 0),
            &text(l, 1),
            int(l, 2) != 0,
            opt_text(l, 3).as_deref(),
        )
    } else {
        return Err(DbError::Unsupported(format!("{name} no longer exists")));
    };
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
        object: ObjectInfo {
            schema: String::new(),
            name: name.to_owned(),
            kind,
            estimated_rows: None,
            detail: None,
        },
        ddl,
        ..ObjectDetail::default()
    })))
}

/// [`IntrospectScope::Dependencies`]; a failed read (no `VIEW DEFINITION`) is a hint.
async fn dependencies(client: &mut TdsClient, schema: &str, name: &str) -> Result<CatalogChunk> {
    let full = format!("{}.{}", quote(schema), quote(name));
    let id = format!("OBJECT_ID({})", lit(&full));
    let rows = match simple_rows(client, &dependencies_sql(&id)).await {
        Ok(rows) => rows,
        Err(e) => {
            return Ok(CatalogChunk::Dependencies(Box::new(Dependencies::hint(
                format!("Dependencies unavailable (VIEW DEFINITION needed): {e}"),
            ))));
        }
    };
    let mut deps = Dependencies::default();
    for r in &rows {
        let type_label = text(r, 3);
        let other_db = opt_text(r, 5).filter(|d| !d.is_empty());
        let schema = match &other_db {
            Some(db) => format!("{db}.{}", text(r, 1)),
            None => text(r, 1),
        };
        deps.push(
            &text(r, 0),
            DependencyInfo {
                schema,
                name: text(r, 2),
                kind: other_db
                    .is_none()
                    .then(|| dependency_kind(&type_label))
                    .flatten(),
                type_label: type_label.to_ascii_lowercase().replace('_', " "),
                dependency: text(r, 4),
            },
        );
    }
    Ok(CatalogChunk::Dependencies(Box::new(deps)))
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

/// Script as CREATE / EXEC: a routine's definition (`OBJECT_DEFINITION`, NULL when the
/// module is encrypted) and its parameters (`@name`, type), `id` being an
/// `OBJECT_ID(…)` expression.
fn routine_sql(id: &str) -> (String, String) {
    (
        format!("SELECT {id}, OBJECT_DEFINITION({id})"),
        format!(
            "SELECT p.name, TYPE_NAME(p.user_type_id) FROM sys.parameters p \
             WHERE p.object_id = {id} AND p.parameter_id > 0 ORDER BY p.parameter_id"
        ),
    )
}

/// [`IntrospectScope::RoutineDefinition`].
async fn routine_definition(
    client: &mut TdsClient,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let full = format!("{}.{}", quote(schema), quote(name));
    let (def_sql, params_sql) = routine_sql(&format!("OBJECT_ID({})", lit(&full)));
    let rows = simple_rows(client, &def_sql).await?;
    let Some(row) = rows
        .first()
        .filter(|r| !matches!(r.first(), None | Some(Value::Null)))
    else {
        return Err(DbError::Unsupported(format!("{full} no longer exists")));
    };
    let ddl = text(row, 1);
    let params = simple_rows(client, &params_sql)
        .await?
        .iter()
        .map(|r| (text(r, 0), text(r, 1)))
        .collect();
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema, name, kind, ddl, params,
    ))))
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
                c.is_identity, \
                CAST((SELECT ep.value FROM sys.extended_properties ep \
                      WHERE ep.class = 1 AND ep.major_id = c.object_id \
                        AND ep.minor_id = c.column_id AND ep.name = 'MS_Description') \
                     AS nvarchar(max)) \
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
        comment: opt_text(row, 9),
    }
}

/// Indexes of the object `id` (an `OBJECT_ID(…)` expression), one row per key column.
fn detail_indexes_sql(id: &str) -> String {
    format!(
        "SELECT i.name, i.is_unique, i.is_primary_key, i.type_desc, c.name, ic.is_descending_key \
         FROM sys.indexes i \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE i.object_id = {id} AND i.name IS NOT NULL AND ic.is_included_column = 0 \
         ORDER BY i.name, ic.key_ordinal"
    )
}

/// Foreign keys of the object `id`, one row per column pair, with their actions.
fn detail_foreign_keys_sql(id: &str) -> String {
    format!(
        "SELECT fk.name, pc.name, SCHEMA_NAME(rt.schema_id) + '.' + rt.name, rc.name, \
                fk.delete_referential_action_desc, fk.update_referential_action_desc \
         FROM sys.foreign_keys fk \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id \
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE fk.parent_object_id = {id} ORDER BY fk.name, fkc.constraint_column_id"
    )
}

/// Triggers of the object `id`: name, definition, INSTEAD OF flag and events.
fn detail_triggers_sql(id: &str) -> String {
    format!(
        "SELECT t.name, OBJECT_DEFINITION(t.object_id), t.is_instead_of_trigger, \
                OBJECTPROPERTY(t.object_id, 'ExecIsInsertTrigger'), \
                OBJECTPROPERTY(t.object_id, 'ExecIsUpdateTrigger'), \
                OBJECTPROPERTY(t.object_id, 'ExecIsDeleteTrigger') \
         FROM sys.triggers t WHERE t.parent_id = {id} ORDER BY t.name"
    )
}

/// Reserved bytes and rows of the object `id`. Needs `VIEW DATABASE STATE`; callers
/// treat an error as unknown.
fn detail_size_sql(id: &str) -> String {
    format!(
        "SELECT SUM(ps.reserved_page_count) * 8192, \
                SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.row_count ELSE 0 END) \
         FROM sys.dm_db_partition_stats ps WHERE ps.object_id = {id}"
    )
}

/// The `MS_Description` extended property of the object `id`.
fn detail_comment_sql(id: &str) -> String {
    format!(
        "SELECT CAST(ep.value AS nvarchar(max)) FROM sys.extended_properties ep \
         WHERE ep.class = 1 AND ep.major_id = {id} AND ep.minor_id = 0 \
           AND ep.name = 'MS_Description'"
    )
}

/// `NO_ACTION` → `NO ACTION`.
fn action_label(desc: &str) -> Option<String> {
    (!desc.is_empty()).then(|| desc.replace('_', " "))
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
        IntrospectScope::Objects {
            kind: ObjectKind::Job,
            ..
        } => jobs(client).await,
        IntrospectScope::Detail { name, kind, .. }
            if matches!(kind, ObjectKind::Role | ObjectKind::Job) =>
        {
            admin_detail(client, &name, kind).await
        }
        IntrospectScope::Dependencies { schema, name, .. } => {
            dependencies(client, &schema, &name).await
        }
        IntrospectScope::Objects { schema, kind } => {
            let Some(sql) = objects_sql(&schema, kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let rows = simple_rows(client, &sql).await?;
            let schema = if kind.is_server_level() {
                String::new()
            } else {
                schema
            };
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
        IntrospectScope::RoutineDefinition {
            schema, name, kind, ..
        } => routine_definition(client, &schema, &name, kind).await,
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

            let idx_rows = simple_rows(client, &detail_indexes_sql(&id)).await?;
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
                        method: Some(text(r, 3).to_ascii_lowercase().replace('_', " ")),
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

            let fk_rows = simple_rows(client, &detail_foreign_keys_sql(&id)).await?;
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
                        on_delete: action_label(&text(r, 4)),
                        on_update: action_label(&text(r, 5)),
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

            let trig_rows = simple_rows(client, &detail_triggers_sql(&id)).await?;
            let triggers = trig_rows.iter().map(|r| text(r, 0)).collect();
            let trigger_details = trig_rows
                .iter()
                .map(|r| {
                    let events: Vec<&str> = [(3, "INSERT"), (4, "UPDATE"), (5, "DELETE")]
                        .into_iter()
                        .filter(|(i, _)| int(r, *i) != 0)
                        .map(|(_, e)| e)
                        .collect();
                    TriggerInfo {
                        name: text(r, 0),
                        timing: if int(r, 2) != 0 {
                            "INSTEAD OF"
                        } else {
                            "AFTER"
                        }
                        .into(),
                        event: events.join(" OR "),
                        definition: text(r, 1),
                    }
                })
                .collect();

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

            // Size and rows need VIEW DATABASE STATE; without it they stay unknown and the
            // row estimate falls back to sys.partitions.
            let (size_bytes, stats_rows) = match simple_rows(client, &detail_size_sql(&id)).await {
                Ok(rows) => rows.first().map_or((None, None), |r| {
                    let n = |i| (!matches!(r.get(i), None | Some(Value::Null))).then(|| int(r, i));
                    (n(0), n(1))
                }),
                Err(e) => {
                    tracing::debug!(error = %e, "partition stats unavailable");
                    (None, None)
                }
            };
            let comment = simple_rows(client, &detail_comment_sql(&id))
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| opt_text(r, 0)));
            let estimated_rows = if kind != ObjectKind::Table {
                None
            } else if stats_rows.is_some() {
                stats_rows
            } else {
                simple_rows(
                    client,
                    &format!(
                        "SELECT SUM(rows) FROM sys.partitions WHERE object_id = {id} AND index_id IN (0, 1)"
                    ),
                )
                .await?
                .first()
                .map(|r| int(r, 0))
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
                size_bytes,
                comment,
                trigger_details,
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

    #[test]
    fn admin_sql_snapshots() {
        insta::assert_snapshot!(
            "mssql_admin_sql",
            [
                ROLES_SQL,
                PRINCIPAL_SQL,
                PRINCIPAL_ROLES_SQL,
                LOGIN_SQL,
                JOBS_SQL,
                JOB_DETAIL_SQL,
                JOB_SCHEDULES_SQL,
            ]
            .join("\n")
        );
        insta::assert_snapshot!(
            "mssql_dependencies_sql",
            dependencies_sql("OBJECT_ID(N'[dbo].[orders]')")
        );
    }

    #[test]
    fn job_lines() {
        assert_eq!(
            job_summary(
                true,
                Some(1),
                Some((20261001, 30000)),
                Some(20_261_009_030_000),
                1
            ),
            "enabled · last run succeeded 2026-10-01 03:00 · next 2026-10-09 03:00"
        );
        assert_eq!(
            job_summary(false, None, None, None, 0),
            "disabled · never run · no schedule"
        );
        assert_eq!(
            job_summary(true, Some(0), Some((20260102, 235959)), None, 2),
            "enabled · last run failed 2026-01-02 23:59"
        );
    }

    #[test]
    fn principal_and_login_text() {
        insta::assert_snapshot!(
            "mssql_principal_ddl",
            [
                principal_ddl(
                    "app",
                    "S",
                    "SQL_USER",
                    Some("dbo"),
                    Some("app_login"),
                    &["db_datareader".into()]
                ),
                principal_ddl("readers", "R", "DATABASE_ROLE", None, None, &[]),
                principal_ddl("orphan", "S", "SQL_USER", None, None, &[]),
                login_ddl("sa", "S", "SQL_LOGIN", true, Some("master")),
                login_ddl("DOM\\ann", "U", "WINDOWS_LOGIN", false, None),
            ]
            .join("\n")
        );
    }

    #[test]
    fn routine_sql_snapshot() {
        let (def, params) = routine_sql("OBJECT_ID(N'[dbo].[usp_orders]')");
        insta::assert_snapshot!("mssql_routine_sql", format!("{def}\n{params}"));
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn detail_sql_snapshots() {
        let id = "OBJECT_ID(N'[dbo].[t]')";
        insta::assert_snapshot!(
            "mssql_columns_sql",
            columns_sql(&format!("o.object_id = {id}"))
        );
        insta::assert_snapshot!("mssql_indexes_sql", detail_indexes_sql(id));
        insta::assert_snapshot!("mssql_foreign_keys_sql", detail_foreign_keys_sql(id));
        insta::assert_snapshot!("mssql_triggers_sql", detail_triggers_sql(id));
        insta::assert_snapshot!("mssql_size_sql", detail_size_sql(id));
        insta::assert_snapshot!("mssql_comment_sql", detail_comment_sql(id));
    }

    #[test]
    fn referential_actions_read_as_words() {
        assert_eq!(action_label("SET_NULL").as_deref(), Some("SET NULL"));
        assert_eq!(action_label("CASCADE").as_deref(), Some("CASCADE"));
        assert_eq!(action_label(""), None);
    }
}
