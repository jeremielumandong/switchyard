//! D1 catalog from `sqlite_master` and the table-valued `pragma_*` functions. D1 has one
//! schema, shown as `main`; Cloudflare's internal `_cf_*` tables are hidden.

use serde_json::Value as Json;

use super::wire::{RawResult, Statement};
use super::{D1Session, json_text};
use crate::catalog::{
    CatalogChunk, ColumnInfo, ForeignKeyInfo, IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo,
    ObjectKind, SchemaInfo, TriggerInfo, like_contains, search_hit,
};
use crate::error::{DbError, Result};

const SCHEMA: &str = "main";

const USER_OBJECTS: &str =
    "m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND m.name NOT LIKE '\\_cf\\_%' ESCAPE '\\'";

/// Global object search over `sqlite_master` (SQLite's LIKE ignores ASCII case):
/// `?1` is an escaped LIKE pattern, `?2` the row cap.
fn search_sql(include_system: bool) -> String {
    let user = if include_system {
        String::new()
    } else {
        format!(" AND {USER_OBJECTS}")
    };
    format!(
        "SELECT m.name, m.type FROM sqlite_master m \
         WHERE m.type IN ('table', 'view') AND m.name LIKE ?1 ESCAPE '!'{user} \
         ORDER BY length(m.name), m.name LIMIT ?2"
    )
}

/// Script as CREATE for [`IntrospectScope::RoutineDefinition`]: the stored `CREATE` text.
const ROUTINE_DEFINITION_SQL: &str = "SELECT sql FROM sqlite_master WHERE name = ?1";

/// Foreign keys of `?1`, one row per column pair, with their actions.
const DETAIL_FOREIGN_KEYS: &str = "SELECT id, \"table\" AS ref, \"from\" AS col, \"to\" AS refcol, \
     on_update, on_delete FROM pragma_foreign_key_list(?1) ORDER BY id, seq";

/// Triggers on `?1` with their `CREATE TRIGGER` text.
const DETAIL_TRIGGERS: &str =
    "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name";

/// Timing and event of a SQLite trigger from its `CREATE TRIGGER` text (D1 keeps no
/// separate columns for them). Defaults to SQLite's own default, `BEFORE`.
pub(crate) fn trigger_timing(sql: &str) -> (String, String) {
    let words: Vec<String> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    let mut timing = "BEFORE";
    let mut event = String::new();
    for (i, w) in words.iter().enumerate() {
        match w.as_str() {
            "BEFORE" => timing = "BEFORE",
            "AFTER" => timing = "AFTER",
            "INSTEAD" => timing = "INSTEAD OF",
            "INSERT" | "UPDATE" | "DELETE" => {
                event = w.clone();
                break;
            }
            "ON" if i > 0 => break,
            _ => {}
        }
    }
    let row = words.windows(3).any(|w| w == ["FOR", "EACH", "ROW"]);
    let timing = if row {
        format!("{timing} FOR EACH ROW")
    } else {
        timing.to_owned()
    };
    (timing, event)
}

/// A pragma referential action (`NO ACTION`, `CASCADE`, …); `None` when empty.
fn fk_action(action: String) -> Option<String> {
    (!action.is_empty()).then_some(action)
}

/// Column lookup by name in one result set.
struct Table<'a> {
    columns: &'a [String],
    rows: &'a [Vec<Json>],
}

impl<'a> Table<'a> {
    fn of(r: Option<&'a RawResult>) -> Self {
        match r.and_then(|r| r.results.as_ref()) {
            Some(x) => Self {
                columns: &x.columns,
                rows: &x.rows,
            },
            None => Self {
                columns: &[],
                rows: &[],
            },
        }
    }

    fn get(&self, row: &'a [Json], name: &str) -> &'a Json {
        self.columns
            .iter()
            .position(|c| c == name)
            .and_then(|i| row.get(i))
            .unwrap_or(&Json::Null)
    }

    fn text(&self, row: &'a [Json], name: &str) -> String {
        json_text(self.get(row, name))
    }

    fn int(&self, row: &'a [Json], name: &str) -> i64 {
        match self.get(row, name) {
            Json::Number(n) => n.as_i64().unwrap_or_default(),
            Json::String(s) => s.parse().unwrap_or_default(),
            Json::Bool(b) => i64::from(*b),
            _ => 0,
        }
    }
}

fn column_info(t: &Table<'_>, row: &[Json], table: &str) -> ColumnInfo {
    let default = t.get(row, "dflt_value");
    ColumnInfo {
        schema: SCHEMA.into(),
        table: table.to_owned(),
        name: t.text(row, "name"),
        data_type: t.text(row, "type"),
        nullable: t.int(row, "notnull") == 0,
        default: (!default.is_null()).then(|| json_text(default)),
        ordinal: t.int(row, "cid") as i32 + 1,
        is_primary_key: t.int(row, "pk") > 0,
        comment: None,
    }
}

fn type_filter(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => Some("table"),
        ObjectKind::View => Some("view"),
        _ => None,
    }
}

pub(super) async fn introspect(s: &D1Session, scope: IntrospectScope) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => Ok(CatalogChunk::Databases(vec![SCHEMA.into()])),
        IntrospectScope::Schemas => Ok(CatalogChunk::Schemas(vec![SchemaInfo {
            name: SCHEMA.into(),
            is_system: false,
        }])),
        IntrospectScope::Objects { kind, .. } => {
            let Some(ty) = type_filter(kind) else {
                return Ok(CatalogChunk::Objects(Vec::new()));
            };
            let sql = format!(
                "SELECT m.name FROM sqlite_master m WHERE m.type = ?1 AND {USER_OBJECTS} ORDER BY m.name"
            );
            let results = s.query(&sql, vec![Json::from(ty)]).await?;
            let t = Table::of(results.first());
            Ok(CatalogChunk::Objects(
                t.rows
                    .iter()
                    .map(|row| ObjectInfo {
                        schema: SCHEMA.into(),
                        name: t.text(row, "name"),
                        kind,
                        estimated_rows: None,
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
            let params = vec![
                Json::from(like_contains(&pattern, false)),
                Json::from(limit),
            ];
            let results = s.query(&search_sql(include_system), params).await?;
            let t = Table::of(results.first());
            Ok(CatalogChunk::Objects(
                t.rows
                    .iter()
                    .filter_map(|row| {
                        search_hit(SCHEMA.into(), t.text(row, "name"), &t.text(row, "type"))
                    })
                    .collect(),
            ))
        }
        IntrospectScope::RoutineDefinition { name, kind, .. } => {
            // D1 has no routines; this answers with whatever `sqlite_master` holds.
            let results = s
                .query(ROUTINE_DEFINITION_SQL, vec![Json::from(name.as_str())])
                .await?;
            let t = Table::of(results.first());
            let Some(row) = t.rows.first() else {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            };
            let ddl = t.text(row, "sql");
            Ok(CatalogChunk::Detail(Box::new(ObjectDetail::routine(
                SCHEMA,
                &name,
                kind,
                ddl,
                Vec::new(),
            ))))
        }
        // The explorer hides dependencies for D1 (`Dialect::supports_dependencies`).
        IntrospectScope::Dependencies { .. } => Err(DbError::Unsupported(
            "D1 keeps no dependency catalog".into(),
        )),
        IntrospectScope::AllColumns => {
            let sql = format!(
                "SELECT m.name AS tbl, p.cid, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk \
                 FROM sqlite_master m JOIN pragma_table_info(m.name) p \
                 WHERE m.type IN ('table', 'view') AND {USER_OBJECTS} ORDER BY m.name, p.cid"
            );
            let results = s.query(&sql, Vec::new()).await?;
            let t = Table::of(results.first());
            Ok(CatalogChunk::AllColumns(
                t.rows
                    .iter()
                    .map(|row| column_info(&t, row, &t.text(row, "tbl")))
                    .collect(),
            ))
        }
        IntrospectScope::Detail { name, kind, .. } => {
            let p = || vec![Json::from(name.as_str())];
            let results = s
                .batch(vec![
                    Statement {
                        sql: "SELECT type, sql FROM sqlite_master WHERE name = ?1",
                        params: p(),
                    },
                    Statement {
                        sql: "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid",
                        params: p(),
                    },
                    Statement {
                        sql: "SELECT il.name AS idx, il.\"unique\" AS uniq, il.origin, ii.name AS col, \
                              (SELECT sql FROM sqlite_master WHERE name = il.name) AS def \
                              FROM pragma_index_list(?1) il JOIN pragma_index_info(il.name) ii \
                              ORDER BY il.name, ii.seqno",
                        params: p(),
                    },
                    Statement {
                        sql: DETAIL_FOREIGN_KEYS,
                        params: p(),
                    },
                    Statement {
                        sql: DETAIL_TRIGGERS,
                        params: p(),
                    },
                ])
                .await?;
            let master = Table::of(results.first());
            let Some(obj) = master.rows.first() else {
                return Err(DbError::Unsupported(format!("{name} no longer exists")));
            };
            let mut ddl = master.text(obj, "sql");

            let cols = Table::of(results.get(1));
            let columns: Vec<ColumnInfo> = cols
                .rows
                .iter()
                .map(|row| column_info(&cols, row, &name))
                .collect();

            let idx = Table::of(results.get(2));
            let mut indexes: Vec<IndexInfo> = Vec::new();
            for row in idx.rows {
                let iname = idx.text(row, "idx");
                let col = idx.text(row, "col");
                match indexes.iter_mut().find(|i| i.name == iname) {
                    Some(i) => i.columns.push(col),
                    None => {
                        let def = idx.text(row, "def");
                        if !def.is_empty() {
                            ddl.push_str(";\n");
                            ddl.push_str(&def);
                        }
                        indexes.push(IndexInfo {
                            is_unique: idx.int(row, "uniq") != 0,
                            is_primary: idx.text(row, "origin") == "pk",
                            definition: if def.is_empty() {
                                format!("automatic index for {}", idx.text(row, "origin"))
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

            let fk = Table::of(results.get(3));
            let mut foreign_keys: Vec<(i64, ForeignKeyInfo)> = Vec::new();
            for row in fk.rows {
                let id = fk.int(row, "id");
                match foreign_keys.iter_mut().find(|(i, _)| *i == id) {
                    Some((_, f)) => {
                        f.columns.push(fk.text(row, "col"));
                        f.referenced_columns.push(fk.text(row, "refcol"));
                    }
                    None => foreign_keys.push((
                        id,
                        ForeignKeyInfo {
                            name: format!("fk_{name}_{id}"),
                            columns: vec![fk.text(row, "col")],
                            references: format!("{SCHEMA}.{}", fk.text(row, "ref")),
                            referenced_columns: vec![fk.text(row, "refcol")],
                            on_delete: fk_action(fk.text(row, "on_delete")),
                            on_update: fk_action(fk.text(row, "on_update")),
                        },
                    )),
                }
            }

            let trig = Table::of(results.get(4));
            let mut triggers = Vec::new();
            let mut trigger_details = Vec::new();
            for row in trig.rows {
                let sql = trig.text(row, "sql");
                let (timing, event) = trigger_timing(&sql);
                triggers.push(trig.text(row, "name"));
                trigger_details.push(TriggerInfo {
                    name: trig.text(row, "name"),
                    timing,
                    event,
                    definition: sql.clone(),
                });
                ddl.push_str(";\n");
                ddl.push_str(&sql);
            }
            if !ddl.is_empty() {
                ddl.push(';');
            }

            Ok(CatalogChunk::Detail(Box::new(ObjectDetail {
                object: ObjectInfo {
                    schema: SCHEMA.into(),
                    name: name.clone(),
                    kind,
                    estimated_rows: None,
                    detail: None,
                },
                columns,
                indexes,
                constraints: Vec::new(),
                foreign_keys: foreign_keys.into_iter().map(|(_, f)| f).collect(),
                triggers,
                ddl,
                // D1 reports no sizes or comments.
                size_bytes: None,
                comment: None,
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
        insta::assert_snapshot!("d1_search_sql", search_sql(false));
    }

    #[test]
    fn routine_definition_sql_snapshot() {
        insta::assert_snapshot!("d1_routine_definition_sql", ROUTINE_DEFINITION_SQL);
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn detail_sql_snapshots() {
        insta::assert_snapshot!("d1_foreign_keys_sql", DETAIL_FOREIGN_KEYS);
        insta::assert_snapshot!("d1_triggers_sql", DETAIL_TRIGGERS);
    }

    #[test]
    fn trigger_timing_reads_the_header() {
        assert_eq!(
            trigger_timing("CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN SELECT 1; END"),
            ("AFTER".into(), "INSERT".into())
        );
        assert_eq!(
            trigger_timing(
                "CREATE TRIGGER v_io INSTEAD OF UPDATE OF a ON v FOR EACH ROW BEGIN SELECT 1; END"
            ),
            ("INSTEAD OF FOR EACH ROW".into(), "UPDATE".into())
        );
        assert_eq!(
            trigger_timing("create trigger x delete on t begin select 1; end").0,
            "BEFORE"
        );
    }
}
