//! Activity monitor (DBX-5b): list sessions and cancel or end them, on a session the
//! monitor owns. The app's activity tab is the only caller: the MCP server and `swy`
//! never send these commands.
//!
//! Guards, enforced here whatever the UI did:
//! * ids reach SQL only as validated integers (or a Snowflake query id), see
//!   [`switchyard_db::activity`];
//! * the monitor's own session is never a target;
//! * on a Production connection the user must have typed the second confirmation;
//! * every cancel or kill that runs is written to query history (tag `activity`), even
//!   when the connection has history turned off.

use std::sync::Arc;
use std::time::Instant;

use switchyard_db::activity::{self, ActivityAction, SessionTarget};
use switchyard_store::{HistoryEntry, HistoryStatus, ProfileId, now_ms};
use tracing::warn;

use super::Service;
use crate::bus::{Event, RequestId, SessionId};

/// History tag of activity-monitor actions.
pub const HISTORY_TAG: &str = "activity";

impl Service {
    /// [`crate::Command::Activity`].
    pub(super) async fn activity(
        &self,
        session: SessionId,
        connection: ProfileId,
        request: RequestId,
    ) {
        let slot = match self.slot(session) {
            Some(s) => s,
            None => {
                if let Err(e) = self.open_session(session, connection).await {
                    return self.emit(Event::Activity {
                        request,
                        result: Err(e.to_string()),
                    });
                }
                match self.slot(session) {
                    Some(s) => s,
                    None => {
                        return self.emit(Event::Activity {
                            request,
                            result: Err("session is not open".into()),
                        });
                    }
                }
            }
        };
        let engine = slot.connection.engine;
        let mut inner = slot.inner.lock().await;
        let version = inner.session.server_version();
        let result = activity::list(inner.session.as_mut(), engine, &version).await;
        drop(inner);
        self.emit(Event::Activity {
            request,
            result: result.map(Arc::new).map_err(|e| e.to_string()),
        });
    }

    /// [`crate::Command::SessionAction`].
    pub(super) async fn session_action(
        &self,
        session: SessionId,
        request: RequestId,
        action: ActivityAction,
        target: SessionTarget,
        confirmed: bool,
    ) {
        let reply = |result: Result<String, String>| Event::SessionAction { request, result };
        let Some(slot) = self.slot(session) else {
            return self.emit(reply(Err("the monitor's session is not open".into())));
        };
        let conn = slot.connection.clone();
        if conn.environment.is_production() && !confirmed {
            return self.emit(reply(Err(
                "Production: type the session id or KILL to confirm".into(),
            )));
        }
        let statement = activity::action_sql(conn.engine, action, &target)
            .unwrap_or_else(|_| format!("-- {} {}", action.label(), target.label()));
        let started_at = now_ms();
        let t0 = Instant::now();
        let mut inner = slot.inner.lock().await;
        let result = activity::act(inner.session.as_mut(), conn.engine, action, &target).await;
        drop(inner);
        let (status, error) = match &result {
            Ok(_) => (HistoryStatus::Ok, None),
            Err(e) => (HistoryStatus::Error, Some(e.to_string())),
        };
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(conn.id.clone()),
            connection_name: conn.name.clone(),
            sql: format!("-- {}: {}\n{statement}", action.label(), target.label()),
            started_at,
            duration_ms: t0.elapsed().as_millis() as i64,
            rows: None,
            affected: None,
            status,
            error,
            tags: vec![HISTORY_TAG.into()],
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "activity history write failed");
        }
        self.emit(reply(result.map_err(|e| e.to_string())));
    }
}
