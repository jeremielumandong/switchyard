//! MySQL catalog from `information_schema` and `SHOW CREATE`. A MySQL database is shown
//! as a schema; the system databases (`mysql`, `sys`, the two `*_schema`) are marked
//! as such.

use mysql_async::Conn;
use mysql_async::Value as MyValue;

use super::{decode, simple_rows};
use crate::catalog::{
    CatalogChunk, ColumnInfo, ConstraintInfo, Dependencies, DependencyInfo, ForeignKeyInfo,
    IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, TriggerInfo,
    dependency_kind, like_contains, search_hit, search_kind,
};
use crate::dialect::Dialect as _;
use crate::dialect::mysql::{MySqlDialect, quote_string};
use crate::error::{DbError, Result};
use crate::value::Value;

/// Databases that belong to the server.
const SYSTEM_SCHEMAS: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

fn is_system(schema: &str) -> bool {
    SYSTEM_SCHEMAS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(schema))
}

/// `('information_schema', 'mysql', …)` for `NOT IN` filters.
fn system_list() -> String {
    let quoted: Vec<String> = SYSTEM_SCHEMAS.iter().map(|s| quote_string(s)).collect();
    format!("({})", quoted.join(", "))
}

const SCHEMAS_SQL: &str =
    "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME";

/// Tables (`?` schema). `SYSTEM VERSIONED` is MariaDB's temporal table.
const TABLES_SQL: &str = "SELECT TABLE_NAME, TABLE_ROWS, TABLE_COMMENT \
     FROM information_schema.TABLES \
     WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED') \
     ORDER BY TABLE_NAME";

/// Views (`?` schema), including the server's own in `information_schema`.
const VIEWS_SQL: &str = "SELECT TABLE_NAME, NULL, NULL \
     FROM information_schema.TABLES \
     WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN ('VIEW', 'SYSTEM VIEW') \
     ORDER BY TABLE_NAME";

/// Functions or procedures (`?` schema, `?` FUNCTION / PROCEDURE) with their parameter
/// list.
const ROUTINES_SQL: &str = "SELECT r.ROUTINE_NAME, \
     (SELECT GROUP_CONCAT(CONCAT_WS(' ', p.PARAMETER_MODE, p.PARAMETER_NAME, p.DTD_IDENTIFIER) \
        ORDER BY p.ORDINAL_POSITION SEPARATOR ', ') \
      FROM information_schema.PARAMETERS p \
      WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME \
        AND p.ROUTINE_TYPE = r.ROUTINE_TYPE AND p.ORDINAL_POSITION > 0) \
     FROM information_schema.ROUTINES r \
     WHERE r.ROUTINE_SCHEMA = ? AND r.ROUTINE_TYPE = ? \
     ORDER BY r.ROUTINE_NAME";

/// Users as `'user'@'host'`; needs `SELECT` on `mysql.user`.
const USERS_SQL: &str = "SELECT CONCAT(QUOTE(User), '@', QUOTE(Host)), \
     IF(account_locked = 'Y', 'locked', NULL) FROM mysql.user ORDER BY User, Host";

const USERS_HINT: &str = "Listing users needs SELECT on mysql.user";

/// Columns of one table (`?` schema, `?` table).
const COLUMNS_SQL: &str = "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, \
     IS_NULLABLE, COLUMN_DEFAULT, ORDINAL_POSITION, COLUMN_KEY, COLUMN_COMMENT \
     FROM information_schema.COLUMNS \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION";

/// Index columns of one table (`?` schema, `?` table); functional key parts have no
/// column name.
const INDEXES_SQL: &str = "SELECT INDEX_NAME, NON_UNIQUE, \
     COALESCE(COLUMN_NAME, '(expression)'), INDEX_TYPE, SUB_PART \
     FROM information_schema.STATISTICS \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX";

/// Foreign key column pairs of one table (`?` schema, `?` table) with their actions.
const FOREIGN_KEYS_SQL: &str = "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, \
     k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
     r.UPDATE_RULE, r.DELETE_RULE \
     FROM information_schema.KEY_COLUMN_USAGE k \
     JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
       ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
      AND r.TABLE_NAME = k.TABLE_NAME \
     WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
     ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION";

/// CHECK constraints of one table (`?` schema, `?` table): MySQL 8.0.16+, MariaDB 10.2+.
const CHECKS_SQL: &str = "SELECT c.CONSTRAINT_NAME, c.CHECK_CLAUSE \
     FROM information_schema.CHECK_CONSTRAINTS c \
     JOIN information_schema.TABLE_CONSTRAINTS t \
       ON t.CONSTRAINT_SCHEMA = c.CONSTRAINT_SCHEMA AND t.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
     WHERE t.TABLE_SCHEMA = ? AND t.TABLE_NAME = ? AND t.CONSTRAINT_TYPE = 'CHECK' \
     ORDER BY c.CONSTRAINT_NAME";

/// Triggers on one table (`?` schema, `?` table).
const TRIGGERS_SQL: &str = "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, \
     ACTION_STATEMENT FROM information_schema.TRIGGERS \
     WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY ACTION_ORDER, TRIGGER_NAME";

/// Row estimate, size and comment of one table (`?` schema, `?` table).
const TABLE_STATS_SQL: &str = "SELECT TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH, TABLE_COMMENT, \
     TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

/// Parameters of a routine (`?` schema, `?` name, `?` FUNCTION / PROCEDURE).
const PARAMS_SQL: &str = "SELECT COALESCE(PARAMETER_NAME, ''), \
     CONCAT_WS(' ', PARAMETER_MODE, DTD_IDENTIFIER) FROM information_schema.PARAMETERS \
     WHERE SPECIFIC_SCHEMA = ? AND SPECIFIC_NAME = ? AND ROUTINE_TYPE = ? \
       AND ORDINAL_POSITION > 0 ORDER BY ORDINAL_POSITION";

/// Stored body of a routine (`?` schema, `?` name, `?` type), for users who may not run
/// `SHOW CREATE` on it.
const ROUTINE_BODY_SQL: &str = "SELECT ROUTINE_DEFINITION FROM information_schema.ROUTINES \
     WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?";

/// Tables and views a view reads, and views that read a relation (`?` schema, `?` name
/// twice). `VIEW_TABLE_USAGE` exists from MySQL 8.0.13 (not in MariaDB).
const VIEW_DEPENDENCIES_SQL: &str = "SELECT 'uses', u.TABLE_SCHEMA, u.TABLE_NAME, \
     COALESCE(t.TABLE_TYPE, 'TABLE'), 'view' \
     FROM information_schema.VIEW_TABLE_USAGE u \
     LEFT JOIN information_schema.TABLES t \
       ON t.TABLE_SCHEMA = u.TABLE_SCHEMA AND t.TABLE_NAME = u.TABLE_NAME \
     WHERE u.VIEW_SCHEMA = ? AND u.VIEW_NAME = ? \
     UNION ALL \
     SELECT 'used_by', VIEW_SCHEMA, VIEW_NAME, 'VIEW', 'view' \
     FROM information_schema.VIEW_TABLE_USAGE \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

/// Tables a table references and tables referencing it through foreign keys (`?`
/// schema, `?` name twice).
const FK_DEPENDENCIES_SQL: &str = "SELECT DISTINCT 'uses', REFERENCED_TABLE_SCHEMA, \
     REFERENCED_TABLE_NAME, 'TABLE', 'foreign key' \
     FROM information_schema.KEY_COLUMN_USAGE \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL \
     UNION ALL \
     SELECT DISTINCT 'used_by', TABLE_SCHEMA, TABLE_NAME, 'TABLE', 'foreign key' \
     FROM information_schema.KEY_COLUMN_USAGE \
     WHERE REFERENCED_TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME = ?";

const VIEW_DEPENDENCIES_HINT: &str = "Views' dependencies need MySQL 8.0.13 or later";

/// Global object search (case-insensitive): `?` escaped LIKE pattern (twice), `?` limit.
fn search_sql(include_system: bool) -> String {
    let (tables, routines) = if include_system {
        (String::new(), String::new())
    } else {
        let list = system_list();
        (
            format!(" AND TABLE_SCHEMA NOT IN {list}"),
            format!(" AND ROUTINE_SCHEMA NOT IN {list}"),
        )
    };
    format!(
        "SELECT TABLE_SCHEMA AS s, TABLE_NAME AS n, \
         CASE WHEN TABLE_TYPE IN ('VIEW', 'SYSTEM VIEW') THEN 'view' ELSE 'table' END AS k \
         FROM information_schema.TABLES \
         WHERE LOWER(TABLE_NAME) LIKE LOWER(?) ESCAPE '!'{tables} \
         UNION ALL \
         SELECT ROUTINE_SCHEMA, ROUTINE_NAME, LOWER(ROUTINE_TYPE) \
         FROM information_schema.ROUTINES \
         WHERE LOWER(ROUTINE_NAME) LIKE LOWER(?) ESCAPE '!'{routines} \
         ORDER BY LENGTH(n), n LIMIT ?"
    )
}

/// Every column of every user database, for completion.
fn all_columns_sql() -> String {
    format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, \
         COLUMN_DEFAULT, ORDINAL_POSITION, COLUMN_KEY, COLUMN_COMMENT \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA NOT IN {} \
         ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION",
        system_list()
    )
}

type Row = Vec<MyValue>;

fn text(r: &Row, i: usize) -> String {
    decode::text(r.get(i))
}

fn opt_text(r: &Row, i: usize) -> Option<String> {
    match r.get(i) {
        None | Some(MyValue::NULL) => None,
        v => Some(decode::text(v)),
    }
}

fn int(r: &Row, i: usize) -> Option<i64> {
    match r.get(i).map(decode::to_value) {
        Some(Value::Int(n)) => Some(n),
        Some(Value::Text(s) | Value::Numeric(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn params(values: &[&str]) -> Vec<Value> {
    values
        .iter()
        .map(|v| Value::Text((*v).to_owned()))
        .collect()
}

/// The named schema, or the connection's current database when none is given.
async fn resolve_schema(conn: &mut Conn, schema: &str) -> Result<String> {
    if !schema.is_empty() {
        return Ok(schema.to_owned());
    }
    let rows = simple_rows(conn, "SELECT DATABASE()", Vec::new()).await?;
    rows.first()
        .and_then(|r| opt_text(r, 0))
        .ok_or_else(|| DbError::Unsupported("no database selected".into()))
}

fn column_info(r: &Row) -> ColumnInfo {
    ColumnInfo {
        schema: text(r, 0),
        table: text(r, 1),
        name: text(r, 2),
        data_type: text(r, 3),
        nullable: text(r, 4).eq_ignore_ascii_case("YES"),
        default: opt_text(r, 5),
        ordinal: int(r, 6).unwrap_or_default() as i32,
        is_primary_key: text(r, 7) == "PRI",
        comment: opt_text(r, 8).filter(|c| !c.is_empty()),
    }
}

/// One `QUOTE()`d string at the start of `s` (`'it\'s'`) and the rest after it.
fn unquote(s: &str) -> Option<(String, &str)> {
    let mut chars = s.strip_prefix('\'')?.char_indices();
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '\'' => return Some((out, &s[i + 2..])),
            c => out.push(c),
        }
    }
    None
}

/// `'user'@'host'` as listed by [`USERS_SQL`] back into its parts, so the name is
/// quoted again by us rather than spliced into SQL as given.
fn parse_account(name: &str) -> Option<(String, String)> {
    let (user, rest) = unquote(name)?;
    let (host, rest) = unquote(rest.strip_prefix('@')?)?;
    rest.is_empty().then_some((user, host))
}

fn routine_type(kind: ObjectKind) -> &'static str {
    if kind == ObjectKind::Function {
        "FUNCTION"
    } else {
        "PROCEDURE"
    }
}

/// A referential action, unless it is the default (`NO ACTION` / `RESTRICT`, which MySQL
/// treats the same).
fn fk_action(action: String) -> Option<String> {
    (!action.is_empty()).then_some(action)
}

pub(super) async fn introspect(conn: &mut Conn, scope: IntrospectScope) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => {
            let rows = simple_rows(conn, "SELECT DATABASE()", Vec::new()).await?;
            Ok(CatalogChunk::Databases(
                rows.first()
                    .and_then(|r| opt_text(r, 0))
                    .into_iter()
                    .collect(),
            ))
        }
        IntrospectScope::Schemas => {
            let rows = simple_rows(conn, SCHEMAS_SQL, Vec::new()).await?;
            Ok(CatalogChunk::Schemas(
                rows.iter()
                    .map(|r| {
                        let name = text(r, 0);
                        SchemaInfo {
                            is_system: is_system(&name),
                            name,
                        }
                    })
                    .collect(),
            ))
        }
        IntrospectScope::Objects { schema, kind } => objects(conn, &schema, kind).await,
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let like = like_contains(&pattern, false);
            let mut p = params(&[&like, &like]);
            p.push(Value::Int(i64::from(limit)));
            let rows = simple_rows(conn, &search_sql(include_system), p).await?;
            Ok(CatalogChunk::Objects(
                rows.iter()
                    .filter_map(|r| search_hit(text(r, 0), text(r, 1), &text(r, 2)))
                    .collect(),
            ))
        }
        IntrospectScope::AllColumns => {
            let rows = simple_rows(conn, &all_columns_sql(), Vec::new()).await?;
            Ok(CatalogChunk::AllColumns(
                rows.iter().map(column_info).collect(),
            ))
        }
        IntrospectScope::Detail { name, kind, .. } if kind == ObjectKind::Role => {
            let Some((user, host)) = parse_account(&name) else {
                return Err(DbError::Unsupported(format!("{name} is not a user")));
            };
            let sql = format!(
                "SHOW GRANTS FOR {}@{}",
                quote_string(&user),
                quote_string(&host)
            );
            let rows = simple_rows(conn, &sql, Vec::new()).await?;
            let ddl = rows
                .iter()
                .map(|r| format!("{};", text(r, 0)))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    name,
                    kind,
                    ..ObjectInfo::default()
                },
                ddl,
                ..ObjectDetail::default()
            })))
        }
        IntrospectScope::Detail { schema, name, kind }
        | IntrospectScope::RoutineDefinition {
            schema, name, kind, ..
        } if matches!(kind, ObjectKind::Function | ObjectKind::Procedure) => {
            let schema = resolve_schema(conn, &schema).await?;
            routine_detail(conn, &schema, &name, kind).await
        }
        IntrospectScope::Detail { schema, name, kind } => {
            let schema = resolve_schema(conn, &schema).await?;
            relation_detail(conn, &schema, &name, kind).await
        }
        IntrospectScope::RoutineDefinition { name, .. } => Err(DbError::Unsupported(format!(
            "{name} has no routine definition"
        ))),
        IntrospectScope::Dependencies { schema, name, kind } => {
            let schema = resolve_schema(conn, &schema).await?;
            dependencies(conn, &schema, &name, kind).await
        }
    }
}

async fn objects(conn: &mut Conn, schema: &str, kind: ObjectKind) -> Result<CatalogChunk> {
    if kind == ObjectKind::Role {
        return Ok(match simple_rows(conn, USERS_SQL, Vec::new()).await {
            Ok(rows) => CatalogChunk::Objects(
                rows.iter()
                    .map(|r| ObjectInfo {
                        schema: String::new(),
                        name: text(r, 0),
                        kind,
                        estimated_rows: None,
                        detail: opt_text(r, 1),
                    })
                    .collect(),
            ),
            Err(DbError::Server(_)) => CatalogChunk::Hint(USERS_HINT.into()),
            Err(e) => return Err(e),
        });
    }
    let sql = match kind {
        ObjectKind::Table => TABLES_SQL,
        ObjectKind::View => VIEWS_SQL,
        ObjectKind::Function | ObjectKind::Procedure => ROUTINES_SQL,
        _ => return Ok(CatalogChunk::Objects(Vec::new())),
    };
    let schema = resolve_schema(conn, schema).await?;
    let p = if sql == ROUTINES_SQL {
        params(&[&schema, routine_type(kind)])
    } else {
        params(&[&schema])
    };
    let rows = simple_rows(conn, sql, p).await?;
    Ok(CatalogChunk::Objects(
        rows.iter()
            .map(|r| ObjectInfo {
                schema: schema.clone(),
                name: text(r, 0),
                kind,
                estimated_rows: if kind == ObjectKind::Table {
                    int(r, 1)
                } else {
                    None
                },
                detail: match kind {
                    ObjectKind::Function | ObjectKind::Procedure => {
                        Some(format!("({})", text(r, 1)))
                    }
                    _ => opt_text(r, 2).filter(|c| !c.is_empty()),
                },
            })
            .collect(),
    ))
}

async fn routine_detail(
    conn: &mut Conn,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let ty = routine_type(kind);
    let qualified = MySqlDialect.qualified(schema, name);
    // `SHOW CREATE` leaves the body NULL for users without the right privilege; the
    // information schema then still has it (without the header).
    let ddl = match simple_rows(conn, &format!("SHOW CREATE {ty} {qualified}"), Vec::new()).await {
        Ok(rows) => rows.first().and_then(|r| opt_text(r, 2)),
        Err(DbError::Server(_)) => None,
        Err(e) => return Err(e),
    };
    let ddl = match ddl {
        Some(d) => d,
        None => {
            let rows = simple_rows(conn, ROUTINE_BODY_SQL, params(&[schema, name, ty])).await?;
            let Some(r) = rows.first() else {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            };
            let body = text(r, 0);
            if body.is_empty() {
                "-- The definition is hidden: reading it needs the SHOW_ROUTINE privilege \
                 (or being the routine's definer)."
                    .to_owned()
            } else {
                format!("-- Body only: SHOW CREATE {ty} needs more privileges\n{body}")
            }
        }
    };
    let params = simple_rows(conn, PARAMS_SQL, params(&[schema, name, ty]))
        .await?
        .iter()
        .map(|r| (text(r, 0), text(r, 1)))
        .collect();
    Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
        schema, name, kind, ddl, params,
    ))))
}

async fn relation_detail(
    conn: &mut Conn,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let p = || params(&[schema, name]);
    let stats = simple_rows(conn, TABLE_STATS_SQL, p()).await?;
    let Some(stats) = stats.first() else {
        return Err(DbError::Unsupported(format!("{name} no longer exists")));
    };
    let is_view = text(stats, 3).contains("VIEW");
    let qualified = MySqlDialect.qualified(schema, name);
    let show = if is_view { "VIEW" } else { "TABLE" };
    // System views (information_schema) have no CREATE statement.
    let ddl = match simple_rows(conn, &format!("SHOW CREATE {show} {qualified}"), Vec::new()).await
    {
        Ok(rows) => rows.first().map(|r| text(r, 1)).unwrap_or_default(),
        Err(DbError::Server(_)) => String::new(),
        Err(e) => return Err(e),
    };

    let columns: Vec<ColumnInfo> = simple_rows(conn, COLUMNS_SQL, p())
        .await?
        .iter()
        .map(column_info)
        .collect();

    let mut indexes: Vec<IndexInfo> = Vec::new();
    for r in simple_rows(conn, INDEXES_SQL, p()).await? {
        let iname = text(&r, 0);
        let col = match int(&r, 4) {
            Some(len) => format!("{}({len})", text(&r, 2)),
            None => text(&r, 2),
        };
        match indexes.iter_mut().find(|i| i.name == iname) {
            Some(i) => i.columns.push(col),
            None => indexes.push(IndexInfo {
                is_unique: int(&r, 1) == Some(0),
                is_primary: iname == "PRIMARY",
                method: opt_text(&r, 3),
                definition: String::new(),
                name: iname,
                columns: vec![col],
            }),
        }
    }
    let mut constraints = Vec::new();
    for i in &mut indexes {
        let cols = i
            .columns
            .iter()
            .map(|c| MySqlDialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let using = i
            .method
            .as_deref()
            .map(|m| format!(" USING {m}"))
            .unwrap_or_default();
        i.definition = if i.is_primary {
            format!("PRIMARY KEY ({cols}){using}")
        } else {
            format!(
                "{}INDEX {} ({cols}){using}",
                if i.is_unique { "UNIQUE " } else { "" },
                MySqlDialect.quote_ident(&i.name)
            )
        };
        if i.is_primary || i.is_unique {
            constraints.push(ConstraintInfo {
                name: i.name.clone(),
                kind: if i.is_primary {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                }
                .into(),
                definition: i.definition.clone(),
            });
        }
    }

    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for r in simple_rows(conn, FOREIGN_KEYS_SQL, p()).await? {
        let fname = text(&r, 0);
        match foreign_keys.iter_mut().find(|f| f.name == fname) {
            Some(f) => {
                f.columns.push(text(&r, 1));
                f.referenced_columns.push(text(&r, 4));
            }
            None => foreign_keys.push(ForeignKeyInfo {
                name: fname,
                columns: vec![text(&r, 1)],
                references: format!("{}.{}", text(&r, 2), text(&r, 3)),
                referenced_columns: vec![text(&r, 4)],
                on_update: fk_action(text(&r, 5)),
                on_delete: fk_action(text(&r, 6)),
            }),
        }
    }
    for f in &foreign_keys {
        constraints.push(ConstraintInfo {
            name: f.name.clone(),
            kind: "FOREIGN KEY".into(),
            definition: format!(
                "FOREIGN KEY ({}) REFERENCES {} ({})",
                f.columns.join(", "),
                f.references,
                f.referenced_columns.join(", ")
            ),
        });
    }
    // Older servers have no CHECK_CONSTRAINTS view: no checks to show.
    if let Ok(rows) = simple_rows(conn, CHECKS_SQL, p()).await {
        for r in rows {
            constraints.push(ConstraintInfo {
                name: text(&r, 0),
                kind: "CHECK".into(),
                definition: format!("CHECK ({})", text(&r, 1)),
            });
        }
    }

    let mut triggers = Vec::new();
    let mut trigger_details = Vec::new();
    for r in simple_rows(conn, TRIGGERS_SQL, p()).await? {
        triggers.push(text(&r, 0));
        trigger_details.push(TriggerInfo {
            name: text(&r, 0),
            timing: format!("{} FOR EACH ROW", text(&r, 1)),
            event: text(&r, 2),
            definition: text(&r, 3),
        });
    }

    Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
        object: ObjectInfo {
            schema: schema.to_owned(),
            name: name.to_owned(),
            kind,
            estimated_rows: if is_view { None } else { int(stats, 0) },
            detail: None,
        },
        columns,
        indexes,
        constraints,
        foreign_keys,
        triggers,
        ddl,
        size_bytes: if is_view { None } else { int(stats, 1) },
        comment: opt_text(stats, 2).filter(|c| !c.is_empty() && !is_view),
        trigger_details,
    })))
}

async fn dependencies(
    conn: &mut Conn,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<CatalogChunk> {
    let mut deps = Dependencies::default();
    if !kind.is_relation() {
        return Ok(CatalogChunk::Dependencies(Box::new(deps)));
    }
    let p = params(&[schema, name, schema, name]);
    let mut rows = simple_rows(conn, FK_DEPENDENCIES_SQL, p.clone()).await?;
    match simple_rows(conn, VIEW_DEPENDENCIES_SQL, p).await {
        Ok(more) => rows.extend(more),
        Err(DbError::Server(_)) => deps.hint = Some(VIEW_DEPENDENCIES_HINT.into()),
        Err(e) => return Err(e),
    }
    for r in &rows {
        let type_label = text(r, 3);
        deps.push(
            &text(r, 0),
            DependencyInfo {
                schema: text(r, 1),
                name: text(r, 2),
                kind: dependency_kind(&type_label).or_else(|| search_kind("table")),
                type_label: type_label.to_ascii_lowercase(),
                dependency: text(r, 4),
            },
        );
    }
    Ok(CatalogChunk::Dependencies(Box::new(deps)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_sql_snapshots() {
        insta::assert_snapshot!("mysql_search_sql", search_sql(false));
        insta::assert_snapshot!("mysql_all_columns_sql", all_columns_sql());
        insta::assert_snapshot!(
            "mysql_detail_sql",
            [
                COLUMNS_SQL,
                INDEXES_SQL,
                FOREIGN_KEYS_SQL,
                CHECKS_SQL,
                TRIGGERS_SQL,
                TABLE_STATS_SQL
            ]
            .join("\n\n")
        );
        insta::assert_snapshot!(
            "mysql_dependencies_sql",
            format!("{FK_DEPENDENCIES_SQL}\n\n{VIEW_DEPENDENCIES_SQL}")
        );
    }

    #[test]
    fn accounts_are_parsed_not_spliced() {
        assert_eq!(parse_account("'app'@'%'"), Some(("app".into(), "%".into())));
        assert_eq!(
            parse_account("'o\\'neil'@'10.0.0.1'"),
            Some(("o'neil".into(), "10.0.0.1".into()))
        );
        assert_eq!(parse_account("'a'@'b'; DROP TABLE t"), None);
        assert_eq!(parse_account("root"), None);
    }

    #[test]
    fn system_schemas() {
        assert!(is_system("MySQL"));
        assert!(is_system("performance_schema"));
        assert!(!is_system("shop"));
        assert_eq!(
            system_list(),
            "('information_schema', 'mysql', 'performance_schema', 'sys')"
        );
    }
}
