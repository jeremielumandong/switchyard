//! Redis sessions: the key browser and console ([`Command::RedisOpen`] and friends).

use std::sync::Arc;
use std::time::Instant;

use switchyard_db::redis::{self, CommandClass, KeyEdit, RedisClient, browse, command};
use switchyard_store::{
    DbConnection, EnvironmentLabel, HistoryEntry, HistoryStatus, Profile, ProfileId, now_ms,
};
use tracing::warn;

use super::{Service, endpoint_of, lock};
use crate::bus::{Event, RedisInfo, RedisOutcome, RequestId, SessionId};
use crate::error::{CoreError, Result};

/// Keys per `SCAN` page (a hint to the server; pages vary).
const SCAN_COUNT: u32 = 500;
/// Elements read per collection value (strings read 64 × as many bytes).
const VALUE_LIMIT: usize = 1_000;

/// An open Redis session.
pub(super) struct RedisSlot {
    connection: DbConnection,
    client: tokio::sync::Mutex<RedisClient>,
    /// Keeps the SSH tunnel open while the session uses it.
    tunnel: Option<Arc<switchyard_remote::ssh::Tunnel>>,
}

impl Service {
    /// Connect to a Redis profile (also used by Test connection).
    pub(super) async fn redis_connect(
        &self,
        c: &DbConnection,
        secret: Option<secrecy::SecretString>,
    ) -> Result<(RedisClient, Option<Arc<switchyard_remote::ssh::Tunnel>>)> {
        let cfg = self.db_config(c, secret).await?;
        let tunnel = self.endpoint(c).await?;
        let client =
            RedisClient::connect(&cfg, tunnel.as_deref().map(endpoint_of).as_ref()).await?;
        Ok((client, tunnel))
    }

    pub(super) async fn redis_open(&self, session: SessionId, id: ProfileId) -> Result<RedisInfo> {
        let profile = self.with_store(move |s| s.profile(&id)).await?;
        let Some(Profile::Db(conn)) = profile else {
            return Err(CoreError::NotFound("connection".into()));
        };
        let (mut client, tunnel) = self.redis_connect(&conn, None).await?;
        let keys = browse::dbsize(&mut client).await.unwrap_or(0);
        let info = RedisInfo {
            version: client.server_version().to_owned(),
            db: client.db(),
            keys,
            read_only: client.read_only(),
        };
        lock(&self.redis).insert(
            session,
            Arc::new(RedisSlot {
                connection: conn,
                client: tokio::sync::Mutex::new(client),
                tunnel,
            }),
        );
        Ok(info)
    }

    /// Drop the Redis sessions that go through tunnel `id` (it was stopped).
    pub(super) fn end_redis_on_tunnel(&self, id: u64) -> Vec<SessionId> {
        let mut slots = lock(&self.redis);
        let ended: Vec<SessionId> = slots
            .iter()
            .filter(|(_, s)| s.tunnel.as_ref().is_some_and(|t| t.id() == id))
            .map(|(s, _)| *s)
            .collect();
        for s in &ended {
            slots.remove(s);
        }
        ended
    }

    fn redis_slot(&self, session: SessionId) -> Result<Arc<RedisSlot>> {
        lock(&self.redis)
            .get(&session)
            .cloned()
            .ok_or_else(|| CoreError::NotFound("Redis session".into()))
    }

    /// The session's client, reconnected first when a previous command broke it (timeout,
    /// dropped connection).
    async fn redis_client<'a>(
        &self,
        slot: &'a RedisSlot,
    ) -> Result<tokio::sync::MutexGuard<'a, RedisClient>> {
        let mut guard = slot.client.lock().await;
        if guard.is_broken() {
            let cfg = self.db_config(&slot.connection, None).await?;
            let endpoint = slot.tunnel.as_deref().map(endpoint_of);
            *guard = RedisClient::connect(&cfg, endpoint.as_ref()).await?;
        }
        Ok(guard)
    }

    pub(super) async fn redis_scan(
        &self,
        session: SessionId,
        request: RequestId,
        pattern: String,
        cursor: u64,
    ) {
        let result = async {
            let slot = self.redis_slot(session)?;
            let mut c = self.redis_client(&slot).await?;
            Ok::<_, CoreError>(browse::scan(&mut c, cursor, &pattern, SCAN_COUNT).await?)
        }
        .await;
        self.emit(Event::RedisKeys {
            session,
            request,
            result: result.map_err(|e| e.to_string()),
        });
    }

    pub(super) async fn redis_load(&self, session: SessionId, request: RequestId, key: Vec<u8>) {
        let result = async {
            let slot = self.redis_slot(session)?;
            let mut c = self.redis_client(&slot).await?;
            Ok::<_, CoreError>(Arc::new(browse::load(&mut c, &key, VALUE_LIMIT).await?))
        }
        .await;
        self.emit(Event::RedisKey {
            session,
            request,
            result: result.map_err(|e| e.to_string()),
        });
    }

    pub(super) async fn redis_edit(
        &self,
        session: SessionId,
        request: RequestId,
        key: Vec<u8>,
        edit: KeyEdit,
        create: bool,
    ) {
        let result = async {
            let slot = self.redis_slot(session)?;
            let mut c = self.redis_client(&slot).await?;
            browse::edit(&mut c, &key, &edit, create).await?;
            Ok::<_, CoreError>(edit.done().to_owned())
        }
        .await;
        let key = match &edit {
            KeyEdit::Rename(to) if result.is_ok() => to.clone(),
            _ => key,
        };
        self.emit(Event::RedisEdited {
            session,
            request,
            key,
            result: result.map_err(|e| e.to_string()),
        });
    }

    pub(super) async fn redis_run(
        &self,
        session: SessionId,
        request: RequestId,
        line: String,
        confirmed: bool,
    ) {
        let outcome = match self.redis_run_inner(session, &line, confirmed).await {
            Ok(o) => o,
            Err(e) => RedisOutcome::Failed(e.to_string()),
        };
        self.emit(Event::RedisReply {
            session,
            request,
            outcome,
        });
    }

    async fn redis_run_inner(
        &self,
        session: SessionId,
        line: &str,
        confirmed: bool,
    ) -> Result<RedisOutcome> {
        let args = command::split(line)?;
        let slot = self.redis_slot(session)?;
        let conn = &slot.connection;
        // KEYS is a read (read-only profiles may run it) but blocks the server while it
        // walks every key, so Production asks first.
        let blocks_server = command::name(&args) == "KEYS";
        if conn.environment == EnvironmentLabel::Production && !confirmed && blocks_server {
            return Ok(RedisOutcome::NeedsConfirmation {
                reason: format!(
                    "KEYS blocks {} while it walks every key; on a Production connection the \
                     key list (SCAN) is safer.",
                    conn.name
                ),
            });
        }
        if command::classify(&args) == CommandClass::Destructive
            && conn.environment == EnvironmentLabel::Production
            && !confirmed
        {
            return Ok(RedisOutcome::NeedsConfirmation {
                reason: format!(
                    "{} can remove data or change the server on {}, a Production connection.",
                    command::name(&args),
                    conn.name
                ),
            });
        }
        let started_at = now_ms();
        let started = Instant::now();
        let reply = {
            let mut c = self.redis_client(&slot).await?;
            redis::run_console(&mut c, &args).await
        };
        let ms = started.elapsed().as_millis() as u64;
        let (status, error) = match &reply {
            Ok(redis::Reply::Error(e)) => (HistoryStatus::Error, Some(e.clone())),
            Ok(_) => (HistoryStatus::Ok, None),
            Err(e) => (HistoryStatus::Error, Some(e.to_string())),
        };
        if conn.history_enabled {
            let entry = HistoryEntry {
                id: 0,
                connection_id: Some(conn.id.clone()),
                connection_name: conn.name.clone(),
                sql: command::redacted(&args),
                started_at,
                duration_ms: ms as i64,
                rows: None,
                affected: None,
                status,
                error,
                tags: Vec::new(),
                has_plan: false,
            };
            if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
                warn!(error = %e, "redis history write failed");
            }
        }
        let reply = reply?;
        Ok(RedisOutcome::Output {
            error: matches!(reply, redis::Reply::Error(_)),
            text: redis::format_reply(&reply),
            ms,
        })
    }
}
