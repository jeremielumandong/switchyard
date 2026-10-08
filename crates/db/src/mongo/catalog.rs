//! MongoDB catalog: databases are the explorer's schemas, collections its tables and
//! views its views. Fields are not declared, so a collection's "columns" come from a
//! sample of its documents.

use futures::StreamExt;
use mongodb::bson::{Bson, Document, doc};

use super::flatten::{DOCUMENT_COLUMN, Layout, json_text};
use super::{MongoSession, server_error};
use crate::catalog::{
    CatalogChunk, ColumnInfo, Dependencies, IndexInfo, IntrospectScope, ObjectDetail, ObjectInfo,
    ObjectKind, SchemaInfo,
};
use crate::dialect::Dialect;
use crate::dialect::mongo::MongoDialect;
use crate::error::{DbError, Result};

/// Documents sampled to describe a collection's fields.
const DETAIL_SAMPLE: i64 = 200;

/// Documents sampled per collection for completion.
const COMPLETION_SAMPLE: i64 = 20;

/// Collections sampled for completion.
const COMPLETION_COLLECTIONS: usize = 50;

fn is_system_db(name: &str) -> bool {
    matches!(name, "admin" | "local" | "config")
}

async fn cursor_docs(s: &MongoSession, db: &str, command: Document, max: usize) -> Result<Vec<Document>> {
    let mut cursor = s
        .client
        .database(db)
        .run_cursor_command(command)
        .await
        .map_err(|e| server_error(&e))?;
    let mut out = Vec::new();
    while out.len() < max {
        match cursor.next().await {
            Some(Ok(d)) => out.push(d),
            Some(Err(e)) => return Err(server_error(&e)),
            None => break,
        }
    }
    Ok(out)
}

/// Database names; only the current one when the user may not list them.
async fn databases(s: &MongoSession) -> Result<Vec<String>> {
    match s
        .client
        .list_database_names()
        .authorized_databases(true)
        .await
    {
        Ok(mut names) => {
            if !names.contains(&s.database) {
                names.push(s.database.clone());
            }
            names.sort();
            Ok(names)
        }
        Err(e) if matches!(server_error(&e), DbError::Server(_)) => Ok(vec![s.database.clone()]),
        Err(e) => Err(server_error(&e)),
    }
}

/// `(name, type)` of every collection in `db` (`collection`, `view`, `timeseries`).
async fn collections(s: &MongoSession, db: &str, filter: Document) -> Result<Vec<(String, String)>> {
    let docs = cursor_docs(
        s,
        db,
        doc! { "listCollections": 1, "filter": filter, "nameOnly": true, "authorizedCollections": true },
        usize::MAX,
    )
    .await?;
    let mut out: Vec<(String, String)> = docs
        .iter()
        .filter_map(|d| {
            let name = d.get_str("name").ok()?;
            if name.starts_with("system.") {
                return None;
            }
            Some((name.to_owned(), d.get_str("type").unwrap_or("collection").to_owned()))
        })
        .collect();
    out.sort();
    Ok(out)
}

fn kind_of(type_name: &str) -> ObjectKind {
    if type_name == "view" {
        ObjectKind::View
    } else {
        ObjectKind::Table
    }
}

async fn sample(s: &MongoSession, db: &str, name: &str, size: i64) -> Result<Vec<Document>> {
    cursor_docs(
        s,
        db,
        doc! { "aggregate": name, "pipeline": [{ "$sample": { "size": size } }], "cursor": {} },
        size as usize,
    )
    .await
}

/// Fields of `docs` as columns: the flattened paths with their inferred types, nullable
/// when some sampled document lacks the field or holds null.
fn columns(db: &str, table: &str, docs: &[Document]) -> Vec<ColumnInfo> {
    let layout = Layout::infer(docs);
    let batch = layout.batch(docs);
    layout
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name != DOCUMENT_COLUMN)
        .map(|(i, c)| ColumnInfo {
            schema: db.to_owned(),
            table: table.to_owned(),
            name: c.name.clone(),
            data_type: c.type_name.clone(),
            nullable: (0..batch.len()).any(|r| batch.cell(r, i).is_null()),
            default: None,
            ordinal: i as i32 + 1,
            // Not a key the grid may edit by: edits are SQL statements.
            is_primary_key: false,
            comment: None,
        })
        .collect()
}

fn index_info(d: &Document) -> IndexInfo {
    let key = d.get_document("key").cloned().unwrap_or_default();
    let method = key.values().find_map(|v| v.as_str().map(str::to_owned));
    let name = d.get_str("name").unwrap_or_default().to_owned();
    IndexInfo {
        columns: key.keys().cloned().collect(),
        is_unique: d.get_bool("unique").unwrap_or(false) || name == "_id_",
        is_primary: name == "_id_",
        definition: json_text(&Bson::Document(key)),
        method: Some(method.unwrap_or_else(|| "btree".into())),
        name,
    }
}

/// `db.createCollection(…)` with the collection's options, then its indexes.
fn ddl(db: &str, name: &str, info: Option<&Document>, indexes: &[Document]) -> String {
    let d = MongoDialect;
    let target = d.qualified(db, name);
    let mut out = String::new();
    let options = info
        .and_then(|i| i.get_document("options").ok())
        .filter(|o| !o.is_empty());
    let site = format!("db.getSiblingDB({})", d.quote_ident(db));
    match options {
        Some(o) => out.push_str(&format!(
            "{site}.createCollection({}, {})\n",
            d.quote_ident(name),
            json_text(&Bson::Document(o.clone()))
        )),
        None => out.push_str(&format!("{site}.createCollection({})\n", d.quote_ident(name))),
    }
    for ix in indexes {
        let n = ix.get_str("name").unwrap_or_default();
        if n == "_id_" {
            continue;
        }
        let key = ix.get_document("key").cloned().unwrap_or_default();
        let mut opts = ix.clone();
        for k in ["v", "key", "ns"] {
            opts.remove(k);
        }
        out.push_str(&format!(
            "{target}.createIndex({}, {})\n",
            json_text(&Bson::Document(key)),
            json_text(&Bson::Document(opts))
        ));
    }
    out
}

async fn detail(s: &MongoSession, db: &str, name: &str, kind: ObjectKind) -> Result<ObjectDetail> {
    let info = cursor_docs(
        s,
        db,
        doc! { "listCollections": 1, "filter": { "name": name } },
        1,
    )
    .await?
    .into_iter()
    .next();
    let docs = sample(s, db, name, DETAIL_SAMPLE).await?;
    let indexes = if kind == ObjectKind::Table {
        cursor_docs(s, db, doc! { "listIndexes": name }, usize::MAX)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // Size and count need collStats; a missing privilege only leaves them empty.
    let stats = s
        .client
        .database(db)
        .run_command(doc! { "collStats": name })
        .await
        .ok();
    let num = |k: &str| {
        stats.as_ref().and_then(|st| match st.get(k) {
            Some(Bson::Int32(n)) => Some(i64::from(*n)),
            Some(Bson::Int64(n)) => Some(*n),
            Some(Bson::Double(f)) => Some(*f as i64),
            _ => None,
        })
    };
    let comment = info
        .as_ref()
        .and_then(|i| i.get_document("options").ok())
        .and_then(|o| o.get_str("viewOn").ok())
        .map(|on| format!("View on {on}"));
    Ok(ObjectDetail {
        object: ObjectInfo {
            schema: db.to_owned(),
            name: name.to_owned(),
            kind,
            estimated_rows: num("count"),
            detail: None,
        },
        columns: columns(db, name, &docs),
        indexes: indexes.iter().map(index_info).collect(),
        ddl: ddl(db, name, info.as_ref(), &indexes),
        size_bytes: num("totalSize").or_else(|| num("storageSize")),
        comment: comment.or_else(|| {
            Some(format!(
                "Fields from a sample of {} documents",
                docs.len()
            ))
        }),
        ..Default::default()
    })
}

/// Escape regex metacharacters so `text` matches literally.
fn regex_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(super) async fn introspect(s: &MongoSession, scope: IntrospectScope) -> Result<CatalogChunk> {
    match scope {
        IntrospectScope::Databases => Ok(CatalogChunk::Databases(databases(s).await?)),
        IntrospectScope::Schemas => Ok(CatalogChunk::Schemas(
            databases(s)
                .await?
                .into_iter()
                .map(|name| SchemaInfo {
                    is_system: is_system_db(&name),
                    name,
                })
                .collect(),
        )),
        IntrospectScope::Objects { schema, kind } => {
            if !matches!(kind, ObjectKind::Table | ObjectKind::View) {
                return Ok(CatalogChunk::Objects(Vec::new()));
            }
            let objects = collections(s, &schema, Document::new())
                .await?
                .into_iter()
                .filter(|(_, t)| kind_of(t) == kind)
                .map(|(name, t)| ObjectInfo {
                    schema: schema.clone(),
                    name,
                    kind,
                    estimated_rows: None,
                    detail: (t == "timeseries").then(|| "time series".to_owned()),
                })
                .collect();
            Ok(CatalogChunk::Objects(objects))
        }
        IntrospectScope::Detail { schema, name, kind } => Ok(CatalogChunk::Detail(Box::new(
            detail(s, &schema, &name, kind).await?,
        ))),
        IntrospectScope::AllColumns => {
            let db = s.database.clone();
            let mut out = Vec::new();
            for (name, _) in collections(s, &db, Document::new())
                .await?
                .into_iter()
                .take(COMPLETION_COLLECTIONS)
            {
                let docs = sample(s, &db, &name, COMPLETION_SAMPLE).await.unwrap_or_default();
                out.extend(columns(&db, &name, &docs));
            }
            Ok(CatalogChunk::AllColumns(out))
        }
        IntrospectScope::Search {
            pattern,
            limit,
            include_system,
        } => {
            let filter = doc! { "name": { "$regex": regex_literal(&pattern), "$options": "i" } };
            let mut out = Vec::new();
            for db in databases(s).await? {
                if is_system_db(&db) && !include_system {
                    continue;
                }
                let Ok(found) = collections(s, &db, filter.clone()).await else {
                    continue;
                };
                for (name, t) in found {
                    out.push(ObjectInfo {
                        schema: db.clone(),
                        name,
                        kind: kind_of(&t),
                        estimated_rows: None,
                        detail: None,
                    });
                }
            }
            out.sort_by_key(|o| (o.name.len(), o.name.clone()));
            out.truncate(limit as usize);
            Ok(CatalogChunk::Objects(out))
        }
        IntrospectScope::Dependencies { .. } => Ok(CatalogChunk::Dependencies(Box::new(
            Dependencies::hint("MongoDB does not track dependencies between collections."),
        ))),
        IntrospectScope::RoutineDefinition { .. } => Err(DbError::Unsupported(
            "MongoDB has no stored routines".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_recreates_options_and_indexes() {
        let info = doc! { "name": "users", "options": { "validator": { "age": { "$gte": 0 } } } };
        let indexes = vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "email": 1 }, "name": "email_1", "unique": true },
        ];
        insta::assert_snapshot!(ddl("shop", "users", Some(&info), &indexes), @r#"
        db.getSiblingDB("shop").createCollection("users", {"validator":{"age":{"$gte":0}}})
        db.getSiblingDB("shop").getCollection("users").createIndex({"email":1}, {"name":"email_1","unique":true})
        "#);
    }

    #[test]
    fn index_rows() {
        let i = index_info(&doc! { "key": { "body": "text" }, "name": "body_text" });
        assert_eq!(i.method.as_deref(), Some("text"));
        assert_eq!(i.columns, ["body"]);
        assert!(index_info(&doc! { "key": { "_id": 1 }, "name": "_id_" }).is_primary);
    }

    #[test]
    fn search_patterns_are_literal() {
        assert_eq!(regex_literal("a.b*"), "a\\.b\\*");
    }
}
