//! Oracle catalog from the `ALL_*` dictionary views (what the user can see) and
//! `DBMS_METADATA.GET_DDL`. Schemas are users.

use oracle::Connection;
use oracle::sql_type::ToSql;

use super::ora_error;
use crate::catalog::{
    CatalogChunk, ColumnInfo, Dependencies, DependencyInfo, ForeignKeyInfo, IndexInfo,
    IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, TriggerInfo,
    dependency_kind, like_contains, search_hit,
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
        comment: None,
    }
}

/// Indexes of `:1.:2`, one row per column: name, uniqueness, column, backs-the-PK
/// count and index type.
const DETAIL_INDEXES: &str = "SELECT i.INDEX_NAME, i.UNIQUENESS, ic.COLUMN_NAME, \
       (SELECT COUNT(*) FROM ALL_CONSTRAINTS c WHERE c.OWNER = i.TABLE_OWNER \
          AND c.INDEX_NAME = i.INDEX_NAME AND c.CONSTRAINT_TYPE = 'P'), \
       i.INDEX_TYPE \
     FROM ALL_INDEXES i JOIN ALL_IND_COLUMNS ic \
       ON ic.INDEX_OWNER = i.OWNER AND ic.INDEX_NAME = i.INDEX_NAME \
     WHERE i.TABLE_OWNER = :1 AND i.TABLE_NAME = :2 \
     ORDER BY i.INDEX_NAME, ic.COLUMN_POSITION";

/// Foreign keys of `:1.:2`, one row per column pair, with the delete rule (Oracle has
/// no update rule).
const DETAIL_FOREIGN_KEYS: &str = "SELECT c.CONSTRAINT_NAME, cc.COLUMN_NAME, \
       r.OWNER || '.' || r.TABLE_NAME, rc.COLUMN_NAME, c.DELETE_RULE \
     FROM ALL_CONSTRAINTS c \
     JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
     JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
     JOIN ALL_CONS_COLUMNS rc ON rc.OWNER = r.OWNER AND rc.CONSTRAINT_NAME = r.CONSTRAINT_NAME \
       AND rc.POSITION = cc.POSITION \
     WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.CONSTRAINT_TYPE = 'R' \
     ORDER BY c.CONSTRAINT_NAME, cc.POSITION";

/// Triggers of `:1.:2` with type, events and source (`TRIGGER_BODY` is a LONG).
const DETAIL_TRIGGERS: &str = "SELECT TRIGGER_NAME, TRIGGER_TYPE, TRIGGERING_EVENT, \
       DESCRIPTION, TRIGGER_BODY \
     FROM ALL_TRIGGERS WHERE TABLE_OWNER = :1 AND TABLE_NAME = :2 ORDER BY 1";

/// Table (or view) comment of `:1.:2`.
const DETAIL_COMMENT: &str =
    "SELECT COMMENTS FROM ALL_TAB_COMMENTS WHERE OWNER = :1 AND TABLE_NAME = :2";

/// Column comments of `:1.:2`.
const DETAIL_COLUMN_COMMENTS: &str = "SELECT COLUMN_NAME, COMMENTS FROM ALL_COL_COMMENTS \
     WHERE OWNER = :1 AND TABLE_NAME = :2 AND COMMENTS IS NOT NULL";

/// Statistics row count of `:1.:2` (NULL until statistics are gathered).
const DETAIL_NUM_ROWS: &str =
    "SELECT NUM_ROWS FROM ALL_TABLES WHERE OWNER = :1 AND TABLE_NAME = :2";

/// Bytes of the table and its index segments (bind owner, table, owner, table: SQL
/// binds by position, so a repeated `:1` would need its own value). `DBA_SEGMENTS` needs a DBA grant; the
/// `USER_SEGMENTS` form only works for the session user's own tables. Oracle has no
/// `ALL_SEGMENTS`.
fn detail_size_sql(dba: bool) -> String {
    let (view, owner) = if dba {
        ("DBA_SEGMENTS", "OWNER = :1 AND ")
    } else {
        ("USER_SEGMENTS", "USER = :1 AND ")
    };
    format!(
        "SELECT SUM(BYTES) FROM {view} WHERE {owner}(SEGMENT_NAME = :2 \
           OR SEGMENT_NAME IN (SELECT INDEX_NAME FROM ALL_INDEXES \
                               WHERE TABLE_OWNER = :3 AND TABLE_NAME = :4))"
    )
}

/// `CREATE OR REPLACE TRIGGER` text from `ALL_TRIGGERS.DESCRIPTION` (the header after
/// the keyword) and `TRIGGER_BODY`.
fn trigger_source(description: &str, body: &str) -> String {
    let head = description.trim_end();
    if head.is_empty() {
        return body.trim_end().to_owned();
    }
    format!("CREATE OR REPLACE TRIGGER {head}\n{}", body.trim_end())
}

/// Global object search over `ALL_OBJECTS`: `:1` is an escaped LIKE pattern, `:2` = 1
/// includes Oracle-maintained users and PUBLIC, `:3` caps the rows. `maintained` uses
/// `ALL_USERS.ORACLE_MAINTAINED` (12c+); without it only SYS and SYSTEM count as system.
fn search_sql(maintained: bool) -> String {
    let system_users = if maintained {
        "SELECT USERNAME FROM ALL_USERS WHERE ORACLE_MAINTAINED = 'Y'"
    } else {
        "SELECT 'SYS' FROM DUAL UNION ALL SELECT 'SYSTEM' FROM DUAL"
    };
    format!(
        "SELECT OWNER, OBJECT_NAME, KIND FROM ( \
           SELECT o.OWNER, o.OBJECT_NAME, \
                  CASE o.OBJECT_TYPE WHEN 'TABLE' THEN 'table' WHEN 'VIEW' THEN 'view' \
                       WHEN 'MATERIALIZED VIEW' THEN 'mview' WHEN 'SEQUENCE' THEN 'sequence' \
                       WHEN 'SYNONYM' THEN 'synonym' WHEN 'PROCEDURE' THEN 'procedure' \
                       WHEN 'PACKAGE' THEN 'package' ELSE 'function' END AS KIND \
           FROM ALL_OBJECTS o \
           WHERE o.OBJECT_TYPE IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW', 'SEQUENCE', \
                                   'SYNONYM', 'PROCEDURE', 'FUNCTION', 'PACKAGE') \
             AND UPPER(o.OBJECT_NAME) LIKE UPPER(:1) ESCAPE '!' \
             AND o.OBJECT_NAME NOT LIKE 'BIN$%' \
             AND NOT (o.OBJECT_TYPE = 'TABLE' AND EXISTS (SELECT 1 FROM ALL_MVIEWS m \
                      WHERE m.OWNER = o.OWNER AND m.MVIEW_NAME = o.OBJECT_NAME)) \
             AND (:2 = 1 OR (o.OWNER <> 'PUBLIC' AND o.OWNER NOT IN ({system_users}))) \
           ORDER BY LENGTH(o.OBJECT_NAME), o.OBJECT_NAME, o.OWNER \
         ) WHERE ROWNUM <= :3"
    )
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

/// Script as CREATE: `DBMS_METADATA.GET_DDL(type, name, owner)`.
const ROUTINE_DDL_SQL: &str = "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM DUAL";

/// Fallback when `GET_DDL` is refused (another user's routine without
/// `SELECT_CATALOG_ROLE`): the source lines the user can see (`:1` owner, `:2` name,
/// `:3` type), which start at `PROCEDURE` / `FUNCTION`.
const ROUTINE_SOURCE_SQL: &str =
    "SELECT TEXT FROM ALL_SOURCE WHERE OWNER = :1 AND NAME = :2 AND TYPE = :3 ORDER BY LINE";

/// Script as EXEC: the input parameters of a standalone routine (`:1` owner, `:2` name).
/// Position 0 is a function's return value; a routine without parameters has one row
/// with a NULL name.
const ROUTINE_PARAMS_SQL: &str = "SELECT ARGUMENT_NAME, DATA_TYPE FROM ALL_ARGUMENTS \
     WHERE OWNER = :1 AND OBJECT_NAME = :2 AND PACKAGE_NAME IS NULL AND DATA_LEVEL = 0 \
       AND POSITION > 0 AND ARGUMENT_NAME IS NOT NULL AND IN_OUT IN ('IN', 'IN/OUT') \
     ORDER BY POSITION";

/// Packages of `:1` (DBX-5c): name and `valid · spec + body`.
const PACKAGES_SQL: &str = "SELECT o.OBJECT_NAME, LOWER(o.STATUS) || CASE WHEN EXISTS ( \
       SELECT 1 FROM ALL_OBJECTS b WHERE b.OWNER = o.OWNER AND b.OBJECT_NAME = o.OBJECT_NAME \
         AND b.OBJECT_TYPE = 'PACKAGE BODY') THEN ' · spec + body' ELSE ' · spec only' END \
     FROM ALL_OBJECTS o WHERE o.OWNER = :1 AND o.OBJECT_TYPE = 'PACKAGE' \
     ORDER BY o.OBJECT_NAME";

/// Users (DBX-5c), Oracle-maintained ones left out (12c+; `USERS_SQL_PRE12C` lists all).
const USERS_SQL: &str = "SELECT USERNAME, 'user · created ' || TO_CHAR(CREATED, 'YYYY-MM-DD') \
     FROM ALL_USERS WHERE ORACLE_MAINTAINED = 'N' ORDER BY USERNAME";

/// [`USERS_SQL`] before 12c (no `ORACLE_MAINTAINED`).
const USERS_SQL_PRE12C: &str = "SELECT USERNAME, 'user · created ' || TO_CHAR(CREATED, 'YYYY-MM-DD') \
     FROM ALL_USERS ORDER BY USERNAME";

/// Every role, with a DBA grant (`DBA_ROLES`, user-created ones).
const ROLES_SQL_DBA: &str =
    "SELECT ROLE, 'role' FROM DBA_ROLES WHERE ORACLE_MAINTAINED = 'N' ORDER BY ROLE";

/// Without `DBA_ROLES`: the roles granted to the session user.
const ROLES_SQL_GRANTED: &str =
    "SELECT GRANTED_ROLE, 'role · granted to you' FROM USER_ROLE_PRIVS ORDER BY 1";

/// One user (`:1`): name and creation time; no row for a role.
const USER_SQL: &str = "SELECT USERNAME, TO_CHAR(CREATED, 'YYYY-MM-DD HH24:MI:SS') \
     FROM ALL_USERS WHERE USERNAME = :1";

/// `DBMS_METADATA.GET_DDL(:1, :2)` of a user or role (needs `SELECT_CATALOG_ROLE` for
/// anyone but yourself).
const PRINCIPAL_DDL_SQL: &str = "SELECT DBMS_METADATA.GET_DDL(:1, :2) FROM DUAL";

/// Dependencies (DBX-5a) from `ALL_DEPENDENCIES` (`:1`–`:4` owner, name and the two
/// type texts of the object, `:5`–`:8` again for the other direction) and foreign keys
/// (`:9`/`:10`, `:11`/`:12` owner and table). `SYS` and `PUBLIC` references (STANDARD,
/// DUAL) are left out. Columns: direction, owner, name, type, link, database link.
const DEPENDENCIES_SQL: &str = "SELECT 'uses', d.REFERENCED_OWNER, d.REFERENCED_NAME, \
       d.REFERENCED_TYPE, d.DEPENDENCY_TYPE, d.REFERENCED_LINK_NAME \
     FROM ALL_DEPENDENCIES d \
     WHERE d.OWNER = :1 AND d.NAME = :2 AND d.TYPE IN (:3, :4) \
       AND d.REFERENCED_OWNER NOT IN ('SYS', 'PUBLIC') \
       AND NOT (d.REFERENCED_OWNER = d.OWNER AND d.REFERENCED_NAME = d.NAME) \
     UNION ALL \
     SELECT 'used_by', d.OWNER, d.NAME, d.TYPE, d.DEPENDENCY_TYPE, NULL \
     FROM ALL_DEPENDENCIES d \
     WHERE d.REFERENCED_OWNER = :5 AND d.REFERENCED_NAME = :6 AND d.REFERENCED_TYPE IN (:7, :8) \
       AND NOT (d.REFERENCED_OWNER = d.OWNER AND d.REFERENCED_NAME = d.NAME) \
     UNION ALL \
     SELECT 'uses', r.OWNER, r.TABLE_NAME, 'TABLE', 'foreign key ' || c.CONSTRAINT_NAME, NULL \
     FROM ALL_CONSTRAINTS c \
     JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
     WHERE c.OWNER = :9 AND c.TABLE_NAME = :10 AND c.CONSTRAINT_TYPE = 'R' \
       AND NOT (r.OWNER = c.OWNER AND r.TABLE_NAME = c.TABLE_NAME) \
     UNION ALL \
     SELECT 'used_by', c.OWNER, c.TABLE_NAME, 'TABLE', 'foreign key ' || c.CONSTRAINT_NAME, NULL \
     FROM ALL_CONSTRAINTS c \
     JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
     WHERE r.OWNER = :11 AND r.TABLE_NAME = :12 AND c.CONSTRAINT_TYPE = 'R' \
       AND NOT (r.OWNER = c.OWNER AND r.TABLE_NAME = c.TABLE_NAME) \
     ORDER BY 1 DESC, 4, 2, 3";

/// The two `ALL_DEPENDENCIES.TYPE` texts of a kind (a package's spec and body), or
/// `None` for a kind Oracle does not track.
fn dependency_types(kind: ObjectKind) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        ObjectKind::Table => ("TABLE", "TABLE"),
        ObjectKind::View => ("VIEW", "VIEW"),
        ObjectKind::MaterializedView => ("MATERIALIZED VIEW", "TABLE"),
        ObjectKind::Function => ("FUNCTION", "FUNCTION"),
        ObjectKind::Procedure => ("PROCEDURE", "PROCEDURE"),
        ObjectKind::Package => ("PACKAGE", "PACKAGE BODY"),
        ObjectKind::Sequence => ("SEQUENCE", "SEQUENCE"),
        ObjectKind::Synonym => ("SYNONYM", "SYNONYM"),
        ObjectKind::Type => ("TYPE", "TYPE BODY"),
        _ => return None,
    })
}

/// [`IntrospectScope::Dependencies`]; any error (privileges) is a hint.
fn dependencies(conn: &Connection, schema: &str, name: &str, kind: ObjectKind) -> CatalogChunk {
    let Some((t1, t2)) = dependency_types(kind) else {
        return CatalogChunk::Dependencies(Box::default());
    };
    let params: [&dyn ToSql; 12] = [
        &schema, &name, &t1, &t2, &schema, &name, &t1, &t2, &schema, &name, &schema, &name,
    ];
    let deps = match rows(conn, DEPENDENCIES_SQL, &params) {
        Ok(r) => {
            let mut deps = Dependencies::default();
            for row in &r {
                let link = row.get(5).cloned().flatten();
                let type_label = text(row, 3);
                deps.push(
                    &text(row, 0),
                    DependencyInfo {
                        schema: match &link {
                            Some(l) => format!("{}@{l}", text(row, 1)),
                            None => text(row, 1),
                        },
                        name: text(row, 2),
                        kind: link
                            .is_none()
                            .then(|| dependency_kind(&type_label))
                            .flatten(),
                        type_label: type_label.to_ascii_lowercase(),
                        dependency: text(row, 4).to_ascii_lowercase(),
                    },
                );
            }
            deps
        }
        Err(e) => Dependencies::hint(format!("Dependencies unavailable: {e}")),
    };
    CatalogChunk::Dependencies(Box::new(deps))
}

/// Users, then roles (every role with `DBA_ROLES`, else the ones granted to you), by name.
fn roles(conn: &Connection) -> Result<Vec<ObjectInfo>> {
    let mut all = rows(conn, USERS_SQL, &[]).or_else(|_| rows(conn, USERS_SQL_PRE12C, &[]))?;
    all.extend(
        rows(conn, ROLES_SQL_DBA, &[])
            .or_else(|_| rows(conn, ROLES_SQL_GRANTED, &[]))
            .unwrap_or_default(),
    );
    let mut out: Vec<ObjectInfo> = all
        .iter()
        .map(|r| ObjectInfo {
            schema: String::new(),
            name: text(r, 0),
            kind: ObjectKind::Role,
            estimated_rows: None,
            detail: r.get(1).cloned().flatten(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// `Detail` of a user or role: `GET_DDL`, else what the dictionary shows as text.
fn role_detail(conn: &Connection, name: &str) -> Result<CatalogChunk> {
    let user = rows(conn, USER_SQL, &[&name])?.into_iter().next();
    let ddl_type = if user.is_some() { "USER" } else { "ROLE" };
    let ddl = rows(conn, PRINCIPAL_DDL_SQL, &[&ddl_type, &name])
        .ok()
        .and_then(|r| r.first().map(|row| text(row, 0).trim().to_owned()))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| match &user {
            Some(u) => format!(
                "-- User {name}, created {}\n-- DBMS_METADATA.GET_DDL needs SELECT_CATALOG_ROLE\n",
                text(u, 1)
            ),
            None => format!("CREATE ROLE \"{}\";", name.replace('"', "\"\"")),
        });
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
        object: ObjectInfo {
            schema: String::new(),
            name: name.to_owned(),
            kind: ObjectKind::Role,
            estimated_rows: None,
            detail: None,
        },
        ddl,
        ..ObjectDetail::default()
    })))
}

/// A package's specification then body (DBX-5c): `GET_DDL('PACKAGE_SPEC' / 'PACKAGE_BODY')`,
/// else `ALL_SOURCE`, joined by a `/` line so the script runs as two PL/SQL units.
fn package_definition(conn: &Connection, schema: &str, name: &str) -> Result<CatalogChunk> {
    let get = |ty: &str| {
        rows(conn, ROUTINE_DDL_SQL, &[&ty, &name, &schema])
            .ok()
            .and_then(|r| r.first().map(|row| text(row, 0).trim().to_owned()))
            .filter(|d| !d.is_empty())
    };
    let source = |ty: &str| -> Result<Option<String>> {
        let lines = rows(conn, ROUTINE_SOURCE_SQL, &[&schema, &name, &ty])?;
        let body: String = lines.iter().map(|r| text(r, 0)).collect();
        Ok((!body.trim().is_empty()).then(|| format!("CREATE OR REPLACE {}", body.trim())))
    };
    let spec = match get("PACKAGE_SPEC") {
        Some(s) => Some(s),
        None => source("PACKAGE")?,
    };
    let Some(spec) = spec else {
        return Err(DbError::Unsupported(format!("{name} no longer exists")));
    };
    let body = match get("PACKAGE_BODY") {
        Some(b) => Some(b),
        None => source("PACKAGE BODY").ok().flatten(),
    };
    let ddl = match body {
        Some(b) => format!("{spec}\n/\n\n{b}"),
        None => spec,
    };
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema,
        name,
        ObjectKind::Package,
        ddl,
        Vec::new(),
    ))))
}

/// [`IntrospectScope::RoutineDefinition`]: `GET_DDL`, else `ALL_SOURCE`.
fn routine_definition(
    conn: &Connection,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let ddl_kind = ddl_type(kind);
    let ddl = rows(conn, ROUTINE_DDL_SQL, &[&ddl_kind, &name, &schema])
        .ok()
        .and_then(|r| r.first().map(|row| text(row, 0).trim().to_owned()))
        .filter(|d| !d.is_empty());
    let ddl = match ddl {
        Some(d) => d,
        None => {
            let lines = rows(conn, ROUTINE_SOURCE_SQL, &[&schema, &name, &ddl_kind])?;
            if lines.is_empty() {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            }
            let body: String = lines.iter().map(|r| text(r, 0)).collect();
            format!("CREATE OR REPLACE {}", body.trim())
        }
    };
    let params = rows(conn, ROUTINE_PARAMS_SQL, &[&schema, &name])?
        .iter()
        .map(|r| (text(r, 0), text(r, 1)))
        .collect();
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema, name, kind, ddl, params,
    ))))
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
        IntrospectScope::Objects {
            kind: ObjectKind::Role,
            ..
        } => Ok(CatalogChunk::Objects(roles(conn)?)),
        IntrospectScope::Objects {
            schema,
            kind: ObjectKind::Package,
        } => {
            let r = rows(conn, PACKAGES_SQL, &[&schema])?;
            Ok(CatalogChunk::Objects(
                r.iter()
                    .map(|row| ObjectInfo {
                        schema: schema.clone(),
                        name: text(row, 0),
                        kind: ObjectKind::Package,
                        estimated_rows: None,
                        detail: row.get(1).cloned().flatten(),
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Detail {
            name,
            kind: ObjectKind::Role,
            ..
        } => role_detail(conn, &name),
        IntrospectScope::Detail {
            schema,
            name,
            kind: ObjectKind::Package,
        }
        | IntrospectScope::RoutineDefinition {
            schema,
            name,
            kind: ObjectKind::Package,
            ..
        } => package_definition(conn, &schema, &name),
        IntrospectScope::Dependencies { schema, name, kind } => {
            Ok(dependencies(conn, &schema, &name, kind))
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
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let like = like_contains(&pattern, false);
            let (system, limit) = (i64::from(include_system), i64::from(limit));
            let params: [&dyn ToSql; 3] = [&like, &system, &limit];
            let r = rows(conn, &search_sql(true), &params)
                .or_else(|_| rows(conn, &search_sql(false), &params))?;
            Ok(CatalogChunk::Objects(
                r.iter()
                    .filter_map(|row| search_hit(text(row, 0), text(row, 1), &text(row, 2)))
                    .collect(),
            ))
        }
        IntrospectScope::RoutineDefinition {
            schema, name, kind, ..
        } => routine_definition(conn, &schema, &name, kind),
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
            for row in rows(conn, DETAIL_INDEXES, &[&schema, &name])? {
                let iname = text(&row, 0);
                let col = text(&row, 2);
                match indexes.iter_mut().find(|i| i.name == iname) {
                    Some(i) => i.columns.push(col),
                    None => indexes.push(IndexInfo {
                        is_unique: text(&row, 1) == "UNIQUE",
                        is_primary: text(&row, 3) != "0",
                        definition: String::new(),
                        method: row
                            .get(4)
                            .cloned()
                            .flatten()
                            .map(|m| m.to_ascii_lowercase()),
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
            for row in rows(conn, DETAIL_FOREIGN_KEYS, &[&schema, &name])? {
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
                        on_delete: row.get(4).cloned().flatten(),
                        on_update: None,
                    }),
                }
            }
            let trigger_details: Vec<TriggerInfo> = rows(conn, DETAIL_TRIGGERS, &[&schema, &name])?
                .iter()
                .map(|r| TriggerInfo {
                    name: text(r, 0),
                    timing: text(r, 1),
                    event: text(r, 2),
                    definition: trigger_source(&text(r, 3), &text(r, 4)),
                })
                .collect();
            let triggers: Vec<String> = trigger_details.iter().map(|t| t.name.clone()).collect();
            // Comments, statistics and segment sizes are extras: any error leaves them unknown.
            let mut columns = columns;
            if let Ok(r) = rows(conn, DETAIL_COLUMN_COMMENTS, &[&schema, &name]) {
                for row in r {
                    let col = text(&row, 0);
                    if let Some(c) = columns.iter_mut().find(|c| c.name == col) {
                        c.comment = row.get(1).cloned().flatten();
                    }
                }
            }
            let first = |sql: &str, params: &[&dyn ToSql]| {
                rows(conn, sql, params)
                    .ok()
                    .and_then(|r| r.first().and_then(|row| row.first().cloned().flatten()))
            };
            let key: [&dyn ToSql; 2] = [&schema, &name];
            let key2: [&dyn ToSql; 4] = [&schema, &name, &schema, &name];
            let comment = first(DETAIL_COMMENT, &key).filter(|c| !c.is_empty());
            let (estimated_rows, size_bytes) =
                if matches!(kind, ObjectKind::Table | ObjectKind::MaterializedView) {
                    (
                        first(DETAIL_NUM_ROWS, &key).and_then(|n| n.parse().ok()),
                        first(&detail_size_sql(true), &key2)
                            .or_else(|| first(&detail_size_sql(false), &key2))
                            .and_then(|n| n.parse().ok()),
                    )
                } else {
                    (None, None)
                };
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
                    estimated_rows,
                    detail: None,
                },
                columns,
                indexes,
                constraints: Vec::new(),
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
        insta::assert_snapshot!("oracle_search_sql", search_sql(true));
        insta::assert_snapshot!("oracle_search_sql_pre12c", search_sql(false));
    }

    #[test]
    fn admin_sql_snapshots() {
        insta::assert_snapshot!(
            "oracle_admin_sql",
            [
                PACKAGES_SQL,
                USERS_SQL,
                USERS_SQL_PRE12C,
                ROLES_SQL_DBA,
                ROLES_SQL_GRANTED,
                USER_SQL,
                PRINCIPAL_DDL_SQL,
            ]
            .join("\n")
        );
        insta::assert_snapshot!("oracle_dependencies_sql", DEPENDENCIES_SQL);
    }

    #[test]
    fn dependency_types_per_kind() {
        assert_eq!(
            dependency_types(ObjectKind::Package),
            Some(("PACKAGE", "PACKAGE BODY"))
        );
        assert_eq!(
            dependency_types(ObjectKind::Table),
            Some(("TABLE", "TABLE"))
        );
        assert_eq!(dependency_types(ObjectKind::Role), None);
    }

    #[test]
    fn routine_sql_snapshot() {
        insta::assert_snapshot!(
            "oracle_routine_sql",
            [ROUTINE_DDL_SQL, ROUTINE_SOURCE_SQL, ROUTINE_PARAMS_SQL].join("\n")
        );
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn detail_sql_snapshots() {
        insta::assert_snapshot!("oracle_indexes_sql", DETAIL_INDEXES);
        insta::assert_snapshot!("oracle_foreign_keys_sql", DETAIL_FOREIGN_KEYS);
        insta::assert_snapshot!("oracle_triggers_sql", DETAIL_TRIGGERS);
        insta::assert_snapshot!("oracle_comment_sql", DETAIL_COMMENT);
        insta::assert_snapshot!("oracle_column_comments_sql", DETAIL_COLUMN_COMMENTS);
        insta::assert_snapshot!("oracle_num_rows_sql", DETAIL_NUM_ROWS);
        insta::assert_snapshot!("oracle_size_sql_dba", detail_size_sql(true));
        insta::assert_snapshot!("oracle_size_sql_user", detail_size_sql(false));
    }

    #[test]
    fn trigger_source_joins_header_and_body() {
        assert_eq!(
            trigger_source(
                "t_bi\nBEFORE INSERT ON t\nFOR EACH ROW\n",
                "BEGIN NULL; END;\n"
            ),
            "CREATE OR REPLACE TRIGGER t_bi\nBEFORE INSERT ON t\nFOR EACH ROW\nBEGIN NULL; END;"
        );
        assert_eq!(trigger_source("", "BEGIN NULL; END;"), "BEGIN NULL; END;");
    }
}
