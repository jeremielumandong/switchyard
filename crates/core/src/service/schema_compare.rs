//! Schema compare: each side of the compare tab has a session of its own, opened on the
//! first request. These commands only read the catalog; the migration script is built
//! in the app and opened in a SQL editor, never run from here. The app is the only
//! caller (the MCP server and `swy` never send these).

use std::sync::Arc;

use switchyard_db::compare;
use switchyard_db::{CatalogChunk, IntrospectScope};
use switchyard_store::ProfileId;
use tracing::info;

use super::{Service, SessionSlot};
use crate::bus::{CompareCatalog, Event, RequestId, SessionId};
use crate::error::{CoreError, Result};

impl Service {
    /// The side's session: open (on `connection`) and switched to `database`.
    async fn compare_session(
        &self,
        session: SessionId,
        connection: ProfileId,
        database: Option<String>,
    ) -> Result<Arc<SessionSlot>> {
        if self.slot(session).is_none() {
            self.open_session(session, connection).await?;
        }
        let slot = self
            .slot(session)
            .ok_or_else(|| CoreError::NotFound("session".into()))?;
        let current = super::lock(&slot.context)
            .database
            .clone()
            .unwrap_or_else(|| slot.connection.database.clone());
        if let Some(db) = database.filter(|d| !d.is_empty() && *d != current) {
            self.set_session_context(session, Some(db), None).await?;
        }
        self.slot(session)
            .ok_or_else(|| CoreError::NotFound("session".into()))
    }

    /// [`crate::Command::CompareCatalog`].
    pub(super) async fn compare_catalog(
        &self,
        session: SessionId,
        connection: ProfileId,
        request: RequestId,
        database: Option<String>,
    ) {
        let result = self
            .compare_catalog_inner(session, connection, database)
            .await
            .map_err(|e| e.to_string());
        self.emit(Event::CompareCatalog { request, result });
    }

    async fn compare_catalog_inner(
        &self,
        session: SessionId,
        connection: ProfileId,
        database: Option<String>,
    ) -> Result<CompareCatalog> {
        let slot = self.compare_session(session, connection, database).await?;
        let engine = slot.connection.engine;
        if switchyard_db::dialect_for(engine)
            .schema_scripter()
            .is_none()
        {
            return Err(CoreError::Unsupported(compare::unsupported_message(engine)));
        }
        let current = super::lock(&slot.context)
            .database
            .clone()
            .or_else(|| Some(slot.connection.database.clone()).filter(|d| !d.is_empty()));
        let mut inner = slot.inner.lock().await;
        // Some servers refuse the database list (permissions): the current one will do.
        let databases = match inner.session.introspect(IntrospectScope::Databases).await {
            Ok(CatalogChunk::Databases(list)) => list,
            _ => current.iter().cloned().collect(),
        };
        let schemas = match inner.session.introspect(IntrospectScope::Schemas).await? {
            CatalogChunk::Schemas(list) => list
                .into_iter()
                .filter(|s| !s.is_system)
                .map(|s| s.name)
                .collect(),
            _ => Vec::new(),
        };
        Ok(CompareCatalog {
            databases,
            database: current,
            schemas,
        })
    }

    /// [`crate::Command::CompareSnapshot`].
    pub(super) async fn compare_snapshot(
        &self,
        session: SessionId,
        connection: ProfileId,
        request: RequestId,
        database: Option<String>,
        schema: String,
    ) {
        let result = async {
            let slot = self
                .compare_session(session, connection, database.clone())
                .await?;
            let engine = slot.connection.engine;
            info!(session, request, "schema compare snapshot");
            let mut inner = slot.inner.lock().await;
            let snapshot = compare::load_snapshot(
                inner.session.as_mut(),
                engine,
                database,
                &schema,
                |done, total| {
                    self.emit(Event::CompareProgress {
                        request,
                        done,
                        total,
                    });
                    // Closing the side's session cancels the read.
                    self.slot(session).is_some()
                },
            )
            .await?;
            Ok::<_, CoreError>(Arc::new(snapshot))
        }
        .await
        .map_err(|e| e.to_string());
        self.emit(Event::CompareSnapshot { request, result });
    }
}
