//! Schema compare: read two schemas ([`SchemaSnapshot`]), diff them ([`diff::compare`])
//! and write the migration script that makes the target match the source
//! ([`script::migration_script`]).
//!
//! Script generation is per engine behind [`SchemaScripter`], reached through
//! [`crate::Dialect::schema_scripter`]; engines without one cannot be compared. Nothing
//! here runs a script: the app opens it in a SQL editor for the user to review.

pub mod diff;
pub mod model;
mod mysql;
mod postgres;
pub mod script;
mod sqlite;
mod text;
mod tsql;

use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogChunk, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind};
use crate::driver::DbSession;
use crate::error::{DbError, Result};
use crate::value::Engine;

pub use diff::{Change, ChangeKind, DiffStatus, Normalizer, ObjectDiff, Part, SchemaDiff, compare};
pub use model::{CheckDef, FkDef, IndexDef, KeyDef, TableModel};
pub use mysql::MySqlScripter;
pub use postgres::PostgresScripter;
pub use script::{SchemaScripter, ScriptHeader, Stmt, TableScript, display_ddl, migration_script};
pub use sqlite::SqliteScripter;
pub use tsql::TSqlScripter;

/// Object kinds schema compare reads, in the order they are listed. Only those the
/// engine's explorer shows ([`crate::Dialect::object_folders`]) are read.
pub const COMPARED_KINDS: [ObjectKind; 5] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Function,
    ObjectKind::Procedure,
];

/// One side of a compare: every compared object of one schema, with its detail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// Engine the snapshot was read from.
    pub engine: Engine,
    /// Database, when the user picked one (else the connection's default).
    pub database: Option<String>,
    /// Schema name.
    pub schema: String,
    /// Objects with their detail. Routines keep their signature in
    /// [`ObjectInfo::detail`] (PostgreSQL overloads).
    pub objects: Vec<ObjectDetail>,
    /// Objects whose detail could not be read: (kind, name, reason). Shown as a warning;
    /// they are left out of the diff.
    pub skipped: Vec<(ObjectKind, String, String)>,
}

/// The message for an engine without a [`SchemaScripter`].
pub fn unsupported_message(engine: Engine) -> String {
    format!(
        "Schema compare isn't supported for {} yet",
        engine.display_name()
    )
}

/// Read `schema` through `session`: the object lists, then each object's detail.
/// `progress(done, total)` is called before each detail; returning `false` stops with
/// [`DbError::Cancelled`]. An object whose detail fails is recorded in
/// [`SchemaSnapshot::skipped`] unless the session itself is gone.
pub async fn load_snapshot(
    session: &mut dyn DbSession,
    engine: Engine,
    database: Option<String>,
    schema: &str,
    mut progress: impl FnMut(usize, usize) -> bool + Send,
) -> Result<SchemaSnapshot> {
    let dialect = crate::dialect_for(engine);
    if dialect.schema_scripter().is_none() {
        return Err(DbError::Unsupported(unsupported_message(engine)));
    }
    let folders = dialect.object_folders();
    let mut listed: Vec<ObjectInfo> = Vec::new();
    for kind in COMPARED_KINDS.into_iter().filter(|k| folders.contains(k)) {
        if !progress(0, 0) {
            return Err(DbError::Cancelled);
        }
        let chunk = session
            .introspect(IntrospectScope::Objects {
                schema: schema.to_owned(),
                kind,
            })
            .await?;
        if let CatalogChunk::Objects(objects) = chunk {
            listed.extend(objects);
        }
    }
    let total = listed.len();
    let mut snapshot = SchemaSnapshot {
        engine,
        database,
        schema: schema.to_owned(),
        objects: Vec::with_capacity(total),
        skipped: Vec::new(),
    };
    for (done, info) in listed.into_iter().enumerate() {
        if !progress(done, total) {
            return Err(DbError::Cancelled);
        }
        let scope = match info.kind {
            ObjectKind::Function | ObjectKind::Procedure => IntrospectScope::RoutineDefinition {
                schema: schema.to_owned(),
                name: info.name.clone(),
                kind: info.kind,
                signature: info.detail.clone(),
            },
            kind => IntrospectScope::Detail {
                schema: schema.to_owned(),
                name: info.name.clone(),
                kind,
            },
        };
        match session.introspect(scope).await {
            Ok(CatalogChunk::Detail(mut detail)) => {
                // Keep the listing's identity: the routine signature, the schema asked for.
                detail.object.schema = schema.to_owned();
                detail.object.name = info.name.clone();
                detail.object.kind = info.kind;
                if matches!(info.kind, ObjectKind::Function | ObjectKind::Procedure) {
                    detail.object.detail = info.detail.clone();
                }
                snapshot.objects.push(*detail);
            }
            Ok(CatalogChunk::Hint(hint)) => snapshot.skipped.push((info.kind, info.name, hint)),
            Ok(_) => snapshot
                .skipped
                .push((info.kind, info.name, "no detail returned".into())),
            Err(e @ (DbError::Closed | DbError::Cancelled | DbError::Connect(_))) => {
                return Err(e);
            }
            Err(e) => snapshot.skipped.push((info.kind, info.name, e.to_string())),
        }
    }
    progress(total, total);
    Ok(snapshot)
}

#[cfg(test)]
mod tests;
