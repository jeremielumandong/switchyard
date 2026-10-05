//! SQLite profile store: profiles, workspace buffers, query history, schema cache, settings.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{Result, StoreError};
use crate::model::{BufferState, Profile, ProfileId, Workspace};

/// Ordered schema migrations. Never edit an applied migration; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "CREATE TABLE profiles (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        sort_order INTEGER NOT NULL DEFAULT 0,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE buffers (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        connection_id TEXT,
        text TEXT NOT NULL,
        cursor INTEGER NOT NULL DEFAULT 0,
        position INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id TEXT,
        connection_name TEXT NOT NULL,
        sql TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL,
        rows INTEGER,
        affected INTEGER,
        status TEXT NOT NULL,
        error TEXT,
        tags TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX history_started_idx ON history (started_at DESC);
    CREATE TABLE schema_cache (
        connection_id TEXT NOT NULL,
        scope TEXT NOT NULL,
        data TEXT NOT NULL,
        cached_at INTEGER NOT NULL,
        PRIMARY KEY (connection_id, scope)
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );",
];

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Outcome of an executed statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryStatus {
    /// Completed.
    Ok,
    /// Failed with an error.
    Error,
    /// Cancelled by the user.
    Cancelled,
}

impl HistoryStatus {
    fn as_str(self) -> &'static str {
        match self {
            HistoryStatus::Ok => "ok",
            HistoryStatus::Error => "error",
            HistoryStatus::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "error" => HistoryStatus::Error,
            "cancelled" => HistoryStatus::Cancelled,
            _ => HistoryStatus::Ok,
        }
    }
}

/// One executed statement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Row id (0 when not yet stored).
    pub id: i64,
    /// Connection id.
    pub connection_id: Option<ProfileId>,
    /// Connection name at the time.
    pub connection_name: String,
    /// Statement text.
    pub sql: String,
    /// Start time, ms since epoch.
    pub started_at: i64,
    /// Duration in ms.
    pub duration_ms: i64,
    /// Rows returned.
    pub rows: Option<i64>,
    /// Rows affected.
    pub affected: Option<i64>,
    /// Outcome.
    pub status: HistoryStatus,
    /// Error message.
    pub error: Option<String>,
    /// Tags (e.g. `agent:claude-code`).
    pub tags: Vec<String>,
}

/// Exported profiles. Never contains secrets or secret references.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileExport {
    /// Format version.
    pub version: u32,
    /// Profiles in sidebar order.
    pub profiles: Vec<Profile>,
}

/// The profile store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (and migrate) the store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > MIGRATIONS.len() as i64 {
            return Err(StoreError::TooNew(version));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
            debug!(version = i + 1, "store migrated");
        }
        Ok(())
    }

    /// Current schema version.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    // ---- profiles ----

    /// All profiles in sidebar order.
    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM profiles ORDER BY sort_order, name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for data in rows {
            out.push(serde_json::from_str(&data?)?);
        }
        Ok(out)
    }

    /// One profile.
    pub fn profile(&self, id: &ProfileId) -> Result<Option<Profile>> {
        let data: Option<String> = self
            .conn
            .query_row("SELECT data FROM profiles WHERE id = ?1", [&id.0], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(match data {
            Some(d) => Some(serde_json::from_str(&d)?),
            None => None,
        })
    }

    /// Insert or update a profile after validation. New profiles go to the end.
    pub fn save_profile(&mut self, profile: &Profile) -> Result<()> {
        profile.validate()?;
        for r in profile.references() {
            if r != profile.id() && self.profile(r)?.is_none() {
                return Err(StoreError::NotFound(format!("referenced profile {r}")));
            }
        }
        let data = serde_json::to_string(profile)?;
        let next_order: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM profiles",
            [],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO profiles (id, kind, name, sort_order, data, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET kind = ?2, name = ?3, data = ?5, updated_at = ?6",
            params![
                profile.id().0,
                profile.kind(),
                profile.name(),
                next_order,
                data,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Delete a profile. Fails if other profiles reference it.
    pub fn delete_profile(&mut self, id: &ProfileId) -> Result<Option<Profile>> {
        let Some(existing) = self.profile(id)? else {
            return Ok(None);
        };
        if let Some(user) = self
            .profiles()?
            .iter()
            .find(|p| p.references().contains(&id))
        {
            return Err(StoreError::InUse(
                existing.name().to_owned(),
                user.name().to_owned(),
            ));
        }
        self.conn
            .execute("DELETE FROM profiles WHERE id = ?1", [&id.0])?;
        self.conn
            .execute("DELETE FROM schema_cache WHERE connection_id = ?1", [&id.0])?;
        Ok(Some(existing))
    }

    /// Set the sidebar order: `ids` first in the given order, others keep relative order.
    pub fn reorder(&mut self, ids: &[ProfileId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE profiles SET sort_order = ?1 WHERE id = ?2",
                params![i as i64, id.0],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Export all profiles without secrets.
    pub fn export(&self) -> Result<ProfileExport> {
        Ok(ProfileExport {
            version: 1,
            profiles: self
                .profiles()?
                .iter()
                .map(Profile::without_secret)
                .collect(),
        })
    }

    /// Export as pretty JSON.
    pub fn export_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.export()?)?)
    }

    /// Import profiles (secrets are never imported). Existing ids are overwritten.
    /// Returns how many profiles were imported.
    pub fn import_json(&mut self, json: &str) -> Result<usize> {
        let export: ProfileExport = serde_json::from_str(json)?;
        // Hosts first so references resolve.
        let mut profiles: Vec<Profile> = export
            .profiles
            .iter()
            .map(Profile::without_secret)
            .collect();
        profiles.sort_by_key(|p| match p {
            Profile::Host(_) => 0,
            _ => 1,
        });
        for p in &profiles {
            p.validate()?;
        }
        let tx_count = profiles.len();
        // Hosts may reference each other (jump hosts); insert all hosts without
        // reference checks, then validate references.
        for p in &profiles {
            let data = serde_json::to_string(p)?;
            self.conn.execute(
                "INSERT INTO profiles (id, kind, name, sort_order, data, updated_at)
                 VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM profiles), ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET kind = ?2, name = ?3, data = ?4, updated_at = ?5",
                params![p.id().0, p.kind(), p.name(), data, now_ms()],
            )?;
        }
        for p in &profiles {
            for r in p.references() {
                if self.profile(r)?.is_none() {
                    return Err(StoreError::NotFound(format!("referenced profile {r}")));
                }
            }
        }
        Ok(tx_count)
    }

    // ---- workspace buffers ----

    /// Save (autosave) an editor buffer.
    pub fn save_buffer(&mut self, b: &BufferState, position: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO buffers (id, title, connection_id, text, cursor, position, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET title = ?2, connection_id = ?3, text = ?4,
                 cursor = ?5, position = ?6, updated_at = ?7",
            params![
                b.id,
                b.title,
                b.connection_id.as_ref().map(|c| c.0.clone()),
                b.text,
                b.cursor as i64,
                position,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Remove a closed buffer.
    pub fn delete_buffer(&mut self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM buffers WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Buffers in tab order.
    pub fn buffers(&self) -> Result<Vec<BufferState>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, connection_id, text, cursor FROM buffers ORDER BY position, updated_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(BufferState {
                id: r.get(0)?,
                title: r.get(1)?,
                connection_id: r.get::<_, Option<String>>(2)?.map(ProfileId),
                text: r.get(3)?,
                cursor: r.get::<_, i64>(4)?.max(0) as usize,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Load the workspace layout (buffers come from [`Store::buffers`]).
    pub fn workspace(&self) -> Result<Workspace> {
        let mut w: Workspace = self.setting("workspace")?.unwrap_or_default();
        w.buffers = self.buffers()?;
        if w.name.is_empty() {
            w.name = "Default".into();
        }
        Ok(w)
    }

    /// Save the workspace layout (not its buffers).
    pub fn save_workspace(&mut self, w: &Workspace) -> Result<()> {
        let mut layout = w.clone();
        layout.buffers.clear();
        self.set_setting("workspace", &layout)
    }

    // ---- settings ----

    /// Read a JSON setting.
    pub fn setting<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(match v {
            Some(v) => Some(serde_json::from_str(&v)?),
            None => None,
        })
    }

    /// Write a JSON setting.
    pub fn set_setting<T: Serialize>(&mut self, key: &str, value: &T) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    // ---- history ----

    /// Record an executed statement. Returns its id.
    pub fn add_history(&mut self, e: &HistoryEntry) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO history (connection_id, connection_name, sql, started_at, duration_ms,
                                  rows, affected, status, error, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                e.connection_id.as_ref().map(|c| c.0.clone()),
                e.connection_name,
                e.sql,
                e.started_at,
                e.duration_ms,
                e.rows,
                e.affected,
                e.status.as_str(),
                e.error,
                e.tags.join(",")
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Search history, newest first. Every whitespace-separated term must match the SQL,
    /// connection name or tags.
    pub fn search_history(
        &self,
        query: &str,
        connection: Option<&ProfileId>,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>> {
        let mut sql = String::from(
            "SELECT id, connection_id, connection_name, sql, started_at, duration_ms, rows,
                    affected, status, error, tags FROM history WHERE 1 = 1",
        );
        let mut args: Vec<String> = Vec::new();
        if let Some(c) = connection {
            args.push(c.0.clone());
            sql.push_str(&format!(" AND connection_id = ?{}", args.len()));
        }
        for term in query.split_whitespace() {
            let escaped = term
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            args.push(format!("%{escaped}%"));
            let n = args.len();
            sql.push_str(&format!(
                " AND (sql LIKE ?{n} ESCAPE '\\' OR connection_name LIKE ?{n} ESCAPE '\\' OR tags LIKE ?{n} ESCAPE '\\')"
            ));
        }
        sql.push_str(&format!(" ORDER BY started_at DESC, id DESC LIMIT {limit}"));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            let tags: String = r.get(10)?;
            Ok(HistoryEntry {
                id: r.get(0)?,
                connection_id: r.get::<_, Option<String>>(1)?.map(ProfileId),
                connection_name: r.get(2)?,
                sql: r.get(3)?,
                started_at: r.get(4)?,
                duration_ms: r.get(5)?,
                rows: r.get(6)?,
                affected: r.get(7)?,
                status: HistoryStatus::parse(&r.get::<_, String>(8)?),
                error: r.get(9)?,
                tags: tags
                    .split(',')
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Delete all history.
    pub fn clear_history(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    // ---- schema cache ----

    /// Store a catalog chunk for a connection and scope key.
    pub fn cache_schema<T: Serialize>(
        &mut self,
        conn: &ProfileId,
        scope: &str,
        data: &T,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO schema_cache (connection_id, scope, data, cached_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(connection_id, scope) DO UPDATE SET data = ?3, cached_at = ?4",
            params![conn.0, scope, serde_json::to_string(data)?, now_ms()],
        )?;
        Ok(())
    }

    /// Read a cached catalog chunk and when it was cached (ms since epoch).
    pub fn cached_schema<T: for<'de> Deserialize<'de>>(
        &self,
        conn: &ProfileId,
        scope: &str,
    ) -> Result<Option<(T, i64)>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT data, cached_at FROM schema_cache WHERE connection_id = ?1 AND scope = ?2",
                params![conn.0, scope],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((d, at)) => Some((serde_json::from_str(&d)?, at)),
            None => None,
        })
    }

    /// Drop the cache for a connection (after DDL or on refresh).
    pub fn invalidate_schema(&mut self, conn: &ProfileId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM schema_cache WHERE connection_id = ?1",
            [&conn.0],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use switchyard_db::Engine;

    use super::*;
    use crate::model::{DbConnection, EnvironmentLabel, Host, SecretRef, SshAuth};

    fn sample() -> (Host, DbConnection) {
        let mut h = Host::new("prod-db-01", "10.0.4.12", "deploy");
        h.environment = EnvironmentLabel::Production;
        h.auth = SshAuth::PublicKey {
            key_path: "~/.ssh/id_ed25519".into(),
        };
        h.secret = Some(SecretRef::for_profile(&h.id, "passphrase"));
        let mut d = DbConnection::new("shop_prod", Engine::Postgres);
        d.user = "app_ro".into();
        d.database = "shop".into();
        d.via_host = Some(h.id.clone());
        d.secret = Some(SecretRef::for_profile(&d.id, "password"));
        (h, d)
    }

    #[test]
    fn crud_and_order() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let (h, d) = sample();
        s.save_profile(&Profile::Host(h.clone())).unwrap();
        s.save_profile(&Profile::Db(d.clone())).unwrap();
        assert_eq!(s.profiles().unwrap().len(), 2);
        // Update keeps position.
        let mut d2 = d.clone();
        d2.name = "shop_prod_ro".into();
        s.save_profile(&Profile::Db(d2.clone())).unwrap();
        let names: Vec<_> = s
            .profiles()
            .unwrap()
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        assert_eq!(names, ["prod-db-01", "shop_prod_ro"]);
        s.reorder(&[d.id.clone(), h.id.clone()]).unwrap();
        let names: Vec<_> = s
            .profiles()
            .unwrap()
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        assert_eq!(names, ["shop_prod_ro", "prod-db-01"]);
        // The Host is in use by the connection.
        assert!(matches!(
            s.delete_profile(&h.id),
            Err(StoreError::InUse(..))
        ));
        assert!(s.delete_profile(&d.id).unwrap().is_some());
        assert!(s.delete_profile(&h.id).unwrap().is_some());
        assert!(s.profiles().unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_and_dangling() {
        let mut s = Store::open_in_memory().unwrap();
        let (_, d) = sample();
        assert!(matches!(
            s.save_profile(&Profile::Db(d)),
            Err(StoreError::NotFound(_))
        ));
        let mut h = Host::new("", "x", "y");
        h.name.clear();
        assert!(matches!(
            s.save_profile(&Profile::Host(h)),
            Err(StoreError::Validation(_))
        ));
    }

    #[test]
    fn export_has_no_secret_material_and_import_restores() {
        let mut s = Store::open_in_memory().unwrap();
        let (h, d) = sample();
        s.save_profile(&Profile::Host(h.clone())).unwrap();
        s.save_profile(&Profile::Db(d.clone())).unwrap();
        let json = s.export_json().unwrap();
        assert!(!json.contains(&format!("{}:passphrase", h.id)), "{json}");
        assert!(!json.contains(&format!("{}:password", d.id)), "{json}");
        assert!(!json.contains("\"secret\": \""), "{json}");

        let mut other = Store::open_in_memory().unwrap();
        assert_eq!(other.import_json(&json).unwrap(), 2);
        let restored = other.profiles().unwrap();
        assert_eq!(restored.len(), 2);
        let Profile::Db(rd) = restored.iter().find(|p| p.kind() == "db").unwrap() else {
            panic!()
        };
        assert_eq!(rd.via_host, Some(h.id.clone()));
        assert!(rd.secret.is_none());
    }

    #[test]
    fn history_search() {
        let mut s = Store::open_in_memory().unwrap();
        let conn = ProfileId("c1".into());
        for (i, sql) in [
            "select * from orders",
            "delete from carts where id = 1",
            "select 100%",
        ]
        .iter()
        .enumerate()
        {
            s.add_history(&HistoryEntry {
                id: 0,
                connection_id: Some(conn.clone()),
                connection_name: "shop_prod".into(),
                sql: (*sql).into(),
                started_at: 1000 + i as i64,
                duration_ms: 5,
                rows: Some(1),
                affected: None,
                status: HistoryStatus::Ok,
                error: None,
                tags: if i == 1 {
                    vec!["agent:claude-code".into()]
                } else {
                    vec![]
                },
            })
            .unwrap();
        }
        assert_eq!(s.search_history("", None, 10).unwrap().len(), 3);
        assert_eq!(
            s.search_history("select", Some(&conn), 10).unwrap()[0].sql,
            "select 100%"
        );
        assert_eq!(s.search_history("100%", None, 10).unwrap().len(), 1);
        assert_eq!(
            s.search_history("agent:claude", None, 10).unwrap()[0].tags,
            ["agent:claude-code"]
        );
        assert!(
            s.search_history("select", Some(&ProfileId("other".into())), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn buffers_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.save_buffer(
                &BufferState {
                    id: "b1".into(),
                    title: "top_customers.sql".into(),
                    connection_id: None,
                    text: "select 1;\nselect 2".into(),
                    cursor: 7,
                },
                0,
            )
            .unwrap();
            let mut w = s.workspace().unwrap();
            w.name = "acme-ops".into();
            w.active_buffer = Some("b1".into());
            s.save_workspace(&w).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let w = s.workspace().unwrap();
        assert_eq!(w.name, "acme-ops");
        assert_eq!(w.buffers.len(), 1);
        assert_eq!(w.buffers[0].text, "select 1;\nselect 2");
        assert_eq!(w.buffers[0].cursor, 7);
    }

    #[test]
    fn schema_cache() {
        let mut s = Store::open_in_memory().unwrap();
        let c = ProfileId("c".into());
        s.cache_schema(&c, "schemas", &vec!["public".to_string()])
            .unwrap();
        let (v, at): (Vec<String>, i64) = s.cached_schema(&c, "schemas").unwrap().unwrap();
        assert_eq!(v, ["public"]);
        assert!(at > 0);
        s.invalidate_schema(&c).unwrap();
        assert!(
            s.cached_schema::<Vec<String>>(&c, "schemas")
                .unwrap()
                .is_none()
        );
    }
}
