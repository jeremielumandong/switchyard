//! SQLite profile store: profiles, workspace buffers, query history, schema cache, settings,
//! snippets, favorites, terminal macros, runbooks.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{Result, StoreError};
use crate::favorites::{Favorite, kind_from_key, kind_key};
use crate::macros::Macro;
use crate::model::{BufferState, Profile, ProfileId, Workspace};
use crate::runbooks::Runbook;
use crate::snippets::{Snippet, engine_from_key, engine_key};

/// Ordered schema migrations. Never edit an applied migration; append a new one.
pub(crate) const MIGRATIONS: &[&str] = &[
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
    // 2: query plans, stored with their history entry (JSON of `switchyard_plan::Plan`)
    "CREATE TABLE plans (
        history_id INTEGER PRIMARY KEY REFERENCES history(id) ON DELETE CASCADE,
        plan TEXT NOT NULL
    );",
    // 3: user SQL snippets (DBX-4b); built-ins live in code (`snippets::builtin_snippets`)
    "CREATE TABLE snippets (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        prefix TEXT NOT NULL,
        body TEXT NOT NULL,
        engine TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX snippets_prefix_idx ON snippets (prefix);",
    // 4: pinned schema-tree objects (DBX-5e); `kind` is `schema` or an `ObjectKind` name
    "CREATE TABLE favorites (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id TEXT NOT NULL,
        database_name TEXT NOT NULL,
        schema_name TEXT NOT NULL,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        position INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (connection_id, database_name, schema_name, name, kind)
    );",
    // 5: terminal macros (MX-5); `input` is the escaped keystrokes (`macros::escape_input`)
    "CREATE TABLE macros (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        input TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );",
    // 6: runbooks (user request 2026-10-12); `data` is the `Runbook` JSON without secrets
    "CREATE TABLE runbooks (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        data TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );",
    // 7: Host tags (Fleet), indexed for the Explorer's tag filter and fleet selections.
    // The Host's JSON keeps its tags too; `save_profile` keeps both in step. Existing
    // tags are copied over (lower case, trimmed; later saves normalize fully).
    "CREATE TABLE host_tags (
        host_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
        tag TEXT NOT NULL,
        PRIMARY KEY (host_id, tag)
    );
    CREATE INDEX host_tags_tag_idx ON host_tags (tag);
    INSERT OR IGNORE INTO host_tags (host_id, tag)
        SELECT p.id, lower(trim(j.value))
        FROM profiles p, json_each(p.data, '$.tags') j
        WHERE p.kind = 'host' AND j.type = 'text' AND trim(j.value) <> '';",
    // 8: workspaces of the Default side (servers and databases). "All" is created from
    // the old `workspace` setting by `Store::ensure_all_workspace`; existing buffers
    // belong to it. `server_facts` keeps what the Servers view shows beside a profile.
    "CREATE TABLE workspaces (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        sort_order INTEGER NOT NULL DEFAULT 0,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    ALTER TABLE buffers ADD COLUMN workspace_id TEXT NOT NULL DEFAULT 'all';
    CREATE INDEX buffers_workspace_idx ON buffers (workspace_id, position);
    CREATE TABLE server_facts (
        profile_id TEXT PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
        os TEXT,
        last_connected INTEGER
    );",
    // 9: SQL tabs remember the script file they were opened from or saved to.
    "ALTER TABLE buffers ADD COLUMN file TEXT;",
];

/// Replace the `host_tags` rows of `profile` (a Host) with its tags.
fn sync_host_tags(conn: &Connection, profile: &Profile) -> Result<()> {
    let Profile::Host(h) = profile else {
        return Ok(());
    };
    conn.execute("DELETE FROM host_tags WHERE host_id = ?1", [&h.id.0])?;
    for tag in crate::tags::normalize_tags(&h.tags) {
        conn.execute(
            "INSERT OR IGNORE INTO host_tags (host_id, tag) VALUES (?1, ?2)",
            params![h.id.0, tag],
        )?;
    }
    Ok(())
}

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
    /// A query plan is stored with this entry.
    #[serde(default)]
    pub has_plan: bool,
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
    pub(crate) conn: Connection,
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
        store.ensure_all_workspace()?;
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
        let normalized;
        let profile = match profile {
            Profile::Host(h) => {
                let mut h = h.clone();
                h.tags = crate::tags::normalize_tags(&h.tags);
                normalized = Profile::Host(h);
                &normalized
            }
            other => other,
        };
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
        sync_host_tags(&self.conn, profile)?;
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
            .map(|p| match p.without_secret() {
                Profile::Host(mut h) => {
                    h.tags = crate::tags::normalize_tags(&h.tags);
                    Profile::Host(h)
                }
                other => other,
            })
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
            sync_host_tags(&self.conn, p)?;
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

    // ---- host tags ----

    /// Every Host tag with the number of Hosts carrying it, by tag.
    pub fn tags(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag, COUNT(*) FROM host_tags GROUP BY tag ORDER BY tag")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The Hosts carrying `tag` (normalized first), in sidebar order.
    pub fn hosts_with_tag(&self, tag: &str) -> Result<Vec<ProfileId>> {
        let Some(tag) = crate::tags::normalize_tag(tag) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT p.id FROM host_tags t JOIN profiles p ON p.id = t.host_id
             WHERE t.tag = ?1 ORDER BY p.sort_order, p.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([tag], |r| Ok(ProfileId(r.get(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The indexed tags of one Host.
    pub fn host_tags(&self, id: &ProfileId) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM host_tags WHERE host_id = ?1 ORDER BY tag")?;
        let rows = stmt.query_map([&id.0], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- workspace buffers ----

    /// Save (autosave) an editor buffer. A new buffer joins the current workspace; an
    /// existing one stays in its own.
    pub fn save_buffer(&mut self, b: &BufferState, position: i64) -> Result<()> {
        let ws = self.current_workspace_id()?;
        self.write_buffer(b, position, &ws)
    }

    /// Remove a closed buffer.
    pub fn delete_buffer(&mut self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM buffers WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Buffers of the current workspace in tab order.
    pub fn buffers(&self) -> Result<Vec<BufferState>> {
        self.buffers_of(&self.current_workspace_id()?)
    }

    /// The current workspace with its buffers.
    pub fn workspace(&self) -> Result<Workspace> {
        let id = self.current_workspace_id()?;
        self.workspace_by_id(&id)?
            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))
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
                    affected, status, error, tags,
                    EXISTS (SELECT 1 FROM plans p WHERE p.history_id = history.id)
             FROM history WHERE 1 = 1",
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
                has_plan: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Store a plan (JSON) with history entry `history_id`, replacing an earlier one.
    pub fn add_plan(&mut self, history_id: i64, plan_json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO plans (history_id, plan) VALUES (?1, ?2)
             ON CONFLICT (history_id) DO UPDATE SET plan = excluded.plan",
            params![history_id, plan_json],
        )?;
        Ok(())
    }

    /// The plan (JSON) stored with history entry `history_id`.
    pub fn plan(&self, history_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT plan FROM plans WHERE history_id = ?1",
                params![history_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Delete all history.
    pub fn clear_history(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    // ---- snippets ----

    /// The user's snippets (built-ins are not stored), ordered by prefix.
    pub fn snippets(&self) -> Result<Vec<Snippet>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, prefix, body, engine, created_at, updated_at FROM snippets
             ORDER BY prefix COLLATE NOCASE, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            let engine: Option<String> = r.get(4)?;
            Ok(Snippet {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                body: r.get(3)?,
                engine: engine.as_deref().and_then(engine_from_key),
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert or update a user snippet after validation; returns it as stored. An empty
    /// or built-in id saves a new snippet (overriding a built-in by its prefix).
    pub fn save_snippet(&mut self, snippet: &Snippet) -> Result<Snippet> {
        snippet.validate()?;
        let mut s = snippet.clone();
        s.name = s.name.trim().to_owned();
        s.prefix = s.prefix.trim().to_owned();
        let now = now_ms();
        if s.id.is_empty() || s.is_builtin() {
            s.id = crate::random::random_hex(12);
            s.created_at = now;
        }
        s.updated_at = now;
        let created: i64 = self.conn.query_row(
            "INSERT INTO snippets (id, name, prefix, body, engine, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name = ?2, prefix = ?3, body = ?4, engine = ?5,
                 updated_at = ?7
             RETURNING created_at",
            params![
                s.id,
                s.name,
                s.prefix,
                s.body,
                s.engine.map(engine_key),
                if s.created_at == 0 { now } else { s.created_at },
                s.updated_at
            ],
            |r| r.get(0),
        )?;
        s.created_at = created;
        Ok(s)
    }

    /// Delete a user snippet. Returns whether it existed.
    pub fn delete_snippet(&mut self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM snippets WHERE id = ?1", [id])?
            > 0)
    }

    // ---- macros ----

    /// Terminal macros by name.
    pub fn macros(&self) -> Result<Vec<Macro>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, input, created_at, updated_at FROM macros
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            let input: String = r.get(2)?;
            Ok(Macro {
                id: r.get(0)?,
                name: r.get(1)?,
                input: crate::macros::unescape_input(&input),
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert (empty id) or update a macro after validation; returns it as stored.
    pub fn save_macro(&mut self, m: &Macro) -> Result<Macro> {
        m.validate()?;
        let mut m = m.clone();
        m.name = m.name.trim().to_owned();
        let now = now_ms();
        if m.id.is_empty() {
            m.id = crate::random::random_hex(12);
            m.created_at = now;
        }
        m.updated_at = now;
        let created: i64 = self.conn.query_row(
            "INSERT INTO macros (id, name, input, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = ?2, input = ?3, updated_at = ?5
             RETURNING created_at",
            params![
                m.id,
                m.name,
                m.escaped(),
                if m.created_at == 0 { now } else { m.created_at },
                m.updated_at
            ],
            |r| r.get(0),
        )?;
        m.created_at = created;
        Ok(m)
    }

    /// Delete a macro. Returns whether it existed.
    pub fn delete_macro(&mut self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM macros WHERE id = ?1", [id])?
            > 0)
    }

    // ---- runbooks ----

    /// Runbooks by name.
    pub fn runbooks(&self) -> Result<Vec<Runbook>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, data, created_at, updated_at FROM runbooks
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data, created_at, updated_at) = row?;
            let mut rb: Runbook = serde_json::from_str(&data)?;
            rb.id = id;
            rb.created_at = created_at;
            rb.updated_at = updated_at;
            out.push(rb);
        }
        Ok(out)
    }

    /// The runbook with id `id`.
    pub fn runbook(&self, id: &str) -> Result<Option<Runbook>> {
        Ok(self.runbooks()?.into_iter().find(|r| r.id == id))
    }

    /// Insert (empty id) or update a runbook after validation; returns it as stored.
    /// Secret parameters' defaults are never stored (see [`Runbook::sanitized`]).
    pub fn save_runbook(&mut self, rb: &Runbook) -> Result<Runbook> {
        rb.validate()?;
        let mut rb = rb.sanitized();
        rb.name = rb.name.trim().to_owned();
        let now = now_ms();
        if rb.id.is_empty() {
            rb.id = crate::random::random_hex(12);
            rb.created_at = now;
        }
        rb.updated_at = now;
        let data = serde_json::to_string(&rb)?;
        let created: i64 = self.conn.query_row(
            "INSERT INTO runbooks (id, name, data, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = ?2, data = ?3, updated_at = ?5
             RETURNING created_at",
            params![
                rb.id,
                rb.name,
                data,
                if rb.created_at == 0 {
                    now
                } else {
                    rb.created_at
                },
                rb.updated_at
            ],
            |r| r.get(0),
        )?;
        rb.created_at = created;
        Ok(rb)
    }

    /// Delete a runbook. Returns it when it existed (its secrets are the caller's to
    /// remove).
    pub fn delete_runbook(&mut self, id: &str) -> Result<Option<Runbook>> {
        let found = self.runbook(id)?;
        self.conn
            .execute("DELETE FROM runbooks WHERE id = ?1", [id])?;
        Ok(found)
    }

    // ---- favorites ----

    /// Pinned objects in their saved order. Pins of a kind this build does not know
    /// (written by a newer one) are skipped.
    pub fn favorites(&self) -> Result<Vec<Favorite>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, connection_id, database_name, schema_name, name, kind, position
             FROM favorites ORDER BY position, id",
        )?;
        let rows = stmt.query_map([], |r| {
            let kind: String = r.get(5)?;
            let fav = Favorite {
                id: r.get(0)?,
                connection_id: ProfileId(r.get(1)?),
                database: r.get(2)?,
                schema: r.get(3)?,
                name: r.get(4)?,
                kind: None,
                position: r.get(6)?,
            };
            Ok(kind_from_key(&kind).map(|kind| Favorite { kind, ..fav }))
        })?;
        let all = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(all.into_iter().flatten().collect())
    }

    /// Pin an object at the end of the list after validation; pinning it again keeps the
    /// existing pin. Returns the pin as stored.
    pub fn add_favorite(&mut self, fav: &Favorite) -> Result<Favorite> {
        fav.validate()?;
        let mut f = fav.clone();
        if f.kind.is_none() {
            f.name.clear();
        }
        let kind = kind_key(f.kind);
        self.conn.execute(
            "INSERT INTO favorites
                 (connection_id, database_name, schema_name, name, kind, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                 (SELECT COALESCE(MAX(position), 0) + 1 FROM favorites), ?6)
             ON CONFLICT (connection_id, database_name, schema_name, name, kind) DO NOTHING",
            params![
                f.connection_id.0,
                f.database,
                f.schema,
                f.name,
                kind,
                now_ms()
            ],
        )?;
        let (id, position) = self.conn.query_row(
            "SELECT id, position FROM favorites WHERE connection_id = ?1
                 AND database_name = ?2 AND schema_name = ?3 AND name = ?4 AND kind = ?5",
            params![f.connection_id.0, f.database, f.schema, f.name, kind],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        f.id = id;
        f.position = position;
        Ok(f)
    }

    /// Unpin. Returns whether the pin existed.
    pub fn remove_favorite(&mut self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM favorites WHERE id = ?1", [id])?
            > 0)
    }

    /// Put the pins in `ids` order; pins not listed keep their place after them.
    pub fn reorder_favorites(&mut self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let rest: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM favorites ORDER BY position, id")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
                .into_iter()
                .filter(|id| !ids.contains(id))
                .collect()
        };
        for (pos, id) in ids.iter().chain(rest.iter()).enumerate() {
            tx.execute(
                "UPDATE favorites SET position = ?1 WHERE id = ?2",
                params![pos as i64 + 1, id],
            )?;
        }
        tx.commit()?;
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
    use switchyard_db::{Engine, ObjectKind};

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
                has_plan: false,
            })
            .unwrap();
        }
        // A plan stored with the first entry; replaced, read back, flagged in search.
        let first = s.search_history("orders", None, 10).unwrap()[0].id;
        s.add_plan(first, "{\"v\":1}").unwrap();
        s.add_plan(first, "{\"v\":2}").unwrap();
        assert_eq!(s.plan(first).unwrap().as_deref(), Some("{\"v\":2}"));
        assert_eq!(s.plan(first + 100).unwrap(), None);
        let flags: Vec<bool> = s
            .search_history("", None, 10)
            .unwrap()
            .iter()
            .map(|e| e.has_plan)
            .collect();
        assert_eq!(flags, [false, false, true]);
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
                    file: None,
                },
                0,
            )
            .unwrap();
            let mut w = s.workspace().unwrap();
            // "All" keeps its name; the layout is saved.
            w.name = "acme-ops".into();
            w.active_buffer = Some("b1".into());
            s.save_workspace(&w).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let w = s.workspace().unwrap();
        assert_eq!(w.name, "All");
        assert_eq!(w.active_buffer.as_deref(), Some("b1"));
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

    #[test]
    fn snippet_crud() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.snippets().unwrap().is_empty());
        let bad = Snippet::new("n", "two words", "SELECT 1;", None);
        assert!(matches!(
            s.save_snippet(&bad),
            Err(StoreError::Validation(..))
        ));
        let a = s
            .save_snippet(&Snippet::new(
                " Count ",
                "cnt",
                "SELECT COUNT(*) FROM ${1:t};",
                None,
            ))
            .unwrap();
        assert!(!a.id.is_empty() && a.created_at > 0);
        assert_eq!(a.name, "Count");
        let b = s
            .save_snippet(&Snippet::new(
                "Who",
                "who",
                "EXEC sp_who2;",
                Some(Engine::SqlServer),
            ))
            .unwrap();
        let all = s.snippets().unwrap();
        assert_eq!(all, vec![a.clone(), b.clone()]);
        assert_eq!(all[1].engine, Some(Engine::SqlServer));

        // Update keeps id and creation time.
        let mut a2 = a.clone();
        a2.body = "SELECT COUNT(1) FROM ${1:t};".into();
        a2.engine = Some(Engine::Postgres);
        let saved = s.save_snippet(&a2).unwrap();
        assert_eq!(
            (saved.id.as_str(), saved.created_at),
            (a.id.as_str(), a.created_at)
        );
        let got = s.snippets().unwrap();
        assert_eq!(got[0].body, a2.body);
        assert_eq!(got[0].engine, Some(Engine::Postgres));

        // Saving a built-in stores a user copy (an override) under a new id.
        let builtin = crate::snippets::builtin_snippets(Engine::Postgres).remove(0);
        let copy = s.save_snippet(&builtin).unwrap();
        assert!(!copy.is_builtin());
        assert_eq!(s.snippets().unwrap().len(), 3);

        assert!(s.delete_snippet(&b.id).unwrap());
        assert!(!s.delete_snippet(&b.id).unwrap());
        assert_eq!(s.snippets().unwrap().len(), 2);
    }

    #[test]
    fn migrates_v2_store_to_snippets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        {
            // A store as an older build left it: schema version 2, one history row.
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..2] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('theme', '\"light\"')",
                [],
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        // Every later migration runs too (4: favorites).
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(
            s.setting::<String>("theme").unwrap().as_deref(),
            Some("light")
        );
        assert!(s.snippets().unwrap().is_empty());
        s.save_snippet(&Snippet::new("n", "p", "SELECT 1;", None))
            .unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.snippets().unwrap().len(), 1);
    }

    #[test]
    fn favorite_crud_and_order() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.favorites().unwrap().is_empty());
        let c = ProfileId("c1".into());
        let t = s
            .add_favorite(&Favorite::object(
                c.clone(),
                "shop",
                "public",
                "orders",
                ObjectKind::Table,
            ))
            .unwrap();
        assert!(t.id > 0);
        assert_eq!(t.position, 1);
        let sc = s
            .add_favorite(&Favorite::schema(c.clone(), "shop", "sales"))
            .unwrap();
        let role = s
            .add_favorite(&Favorite::object(
                ProfileId("c2".into()),
                "",
                "",
                "app",
                ObjectKind::Role,
            ))
            .unwrap();
        assert_eq!((sc.position, role.position), (2, 3));
        // Pinning the same object again keeps the first pin.
        let again = s
            .add_favorite(&Favorite::object(
                c.clone(),
                "shop",
                "public",
                "orders",
                ObjectKind::Table,
            ))
            .unwrap();
        assert_eq!((again.id, again.position), (t.id, 1));
        // The same name in another database or of another kind is another pin.
        s.add_favorite(&Favorite::object(
            c.clone(),
            "shop",
            "public",
            "orders",
            ObjectKind::View,
        ))
        .unwrap();
        let all = s.favorites().unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], t);
        assert_eq!(all[1].kind, None);
        assert_eq!(all[2].kind, Some(ObjectKind::Role));
        assert!(matches!(
            s.add_favorite(&Favorite::object(
                c,
                "shop",
                "public",
                "",
                ObjectKind::Table
            )),
            Err(StoreError::Validation(..))
        ));

        s.reorder_favorites(&[role.id, t.id]).unwrap();
        let ids: Vec<i64> = s.favorites().unwrap().iter().map(|f| f.id).collect();
        assert_eq!(&ids[..3], &[role.id, t.id, sc.id]);

        assert!(s.remove_favorite(sc.id).unwrap());
        assert!(!s.remove_favorite(sc.id).unwrap());
        assert_eq!(s.favorites().unwrap().len(), 3);
        // A pin of a kind this build does not know is skipped, not an error.
        s.conn
            .execute(
                "INSERT INTO favorites (connection_id, database_name, schema_name, name, kind,
                     position, created_at) VALUES ('c', '', 's', 'x', 'Hologram', 9, 0)",
                [],
            )
            .unwrap();
        assert_eq!(s.favorites().unwrap().len(), 3);
    }

    #[test]
    fn migrates_v3_store_to_favorites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        {
            // A store as DBX-4b left it: schema version 3, one snippet.
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..3] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute(
                "INSERT INTO snippets (id, name, prefix, body, engine, created_at, updated_at)
                 VALUES ('s1', 'n', 'p', 'SELECT 1;', NULL, 1, 1)",
                [],
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(s.snippets().unwrap().len(), 1);
        assert!(s.favorites().unwrap().is_empty());
        s.add_favorite(&Favorite::schema(ProfileId("c".into()), "", "public"))
            .unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.favorites().unwrap().len(), 1);
    }

    #[test]
    fn host_tags_round_trip_and_follow_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        let mut s = Store::open(&path).unwrap();
        let mut web = Host::new("web-1", "10.0.0.1", "deploy");
        web.tags = vec![" Web ".into(), "role:App".into(), "web".into()];
        let mut db = Host::new("db-1", "10.0.0.2", "deploy");
        db.tags = vec!["role:db".into()];
        s.save_profile(&Profile::Host(web.clone())).unwrap();
        s.save_profile(&Profile::Host(db.clone())).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        // Normalized in the profile and in the index.
        let Some(Profile::Host(saved)) = s.profile(&web.id).unwrap() else {
            panic!("host missing");
        };
        assert_eq!(saved.tags, ["web", "role:app"]);
        assert_eq!(s.host_tags(&web.id).unwrap(), ["role:app", "web"]);
        assert_eq!(
            s.tags().unwrap(),
            [
                ("role:app".to_owned(), 1),
                ("role:db".to_owned(), 1),
                ("web".to_owned(), 1)
            ]
        );
        assert_eq!(s.hosts_with_tag("WEB").unwrap(), [web.id.clone()]);
        // Re-saving replaces the tags; deleting removes them.
        let mut changed = saved.clone();
        changed.tags = vec!["role:db".into()];
        s.save_profile(&Profile::Host(changed)).unwrap();
        assert_eq!(
            s.hosts_with_tag("role:db").unwrap(),
            [web.id.clone(), db.id.clone()]
        );
        assert!(s.hosts_with_tag("web").unwrap().is_empty());
        s.delete_profile(&db.id).unwrap();
        assert_eq!(s.tags().unwrap(), [("role:db".to_owned(), 1)]);
        // Imports index their tags too.
        let json = s.export_json().unwrap();
        let mut other = Store::open_in_memory().unwrap();
        other.import_json(&json).unwrap();
        assert_eq!(other.hosts_with_tag("role:db").unwrap(), [web.id]);
    }

    #[test]
    fn migrates_v6_store_tags_into_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        let mut h = Host::new("web", "10.0.0.1", "deploy");
        h.tags = vec!["Web".into(), " role:app ".into(), String::new()];
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..6] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute(
                "INSERT INTO profiles (id, kind, name, sort_order, data, updated_at)
                 VALUES (?1, 'host', 'web', 0, ?2, 0)",
                params![
                    h.id.0,
                    serde_json::to_string(&Profile::Host(h.clone())).unwrap()
                ],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(s.host_tags(&h.id).unwrap(), ["role:app", "web"]);
        assert_eq!(s.hosts_with_tag("web").unwrap(), [h.id]);
    }

    #[test]
    fn macros_are_saved_listed_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        let mut s = Store::open(&path).unwrap();
        let input = b"cd /srv\r\x1b[A\x03\xff".to_vec();
        let saved = s
            .save_macro(&Macro::new(" deploy ", input.clone()))
            .unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(saved.name, "deploy");
        s.save_macro(&Macro::new("a first", b"x".to_vec())).unwrap();
        assert!(s.save_macro(&Macro::new("", b"x".to_vec())).is_err());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let list = s.macros().unwrap();
        assert_eq!(
            list.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["a first", "deploy"]
        );
        assert_eq!(list[1].input, input);
        let mut renamed = list[1].clone();
        renamed.name = "deploy 2".into();
        s.save_macro(&renamed).unwrap();
        assert_eq!(s.macros().unwrap()[1].name, "deploy 2");
        assert!(s.delete_macro(&saved.id).unwrap());
        assert_eq!(s.macros().unwrap().len(), 1);
    }

    #[test]
    fn runbooks_round_trip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        let mut s = Store::open(&path).unwrap();
        let rb = crate::runbooks::tests::sample();
        let saved = s.save_runbook(&rb).unwrap();
        assert!(!saved.id.is_empty());
        assert!(saved.created_at > 0);
        assert_eq!(
            saved.params[1].default, "",
            "secret defaults are never stored"
        );
        assert!(s.save_runbook(&Runbook::new("")).is_err());
        s.save_runbook(&Runbook::new("a first")).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let raw: String = s
            .conn
            .query_row(
                "SELECT data FROM runbooks WHERE id = ?1",
                [&saved.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!raw.contains("hunter2"));
        let list = s.runbooks().unwrap();
        assert_eq!(
            list.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a first", "Fail over replica"]
        );
        let back = &list[1];
        assert_eq!(back.id, saved.id);
        assert_eq!(back.steps, rb.steps);
        assert_eq!(back.params[0], rb.params[0]);
        let mut renamed = back.clone();
        renamed.name = "Fail over 2".into();
        renamed.steps.pop();
        let again = s.save_runbook(&renamed).unwrap();
        assert_eq!(again.created_at, back.created_at);
        let got = s.runbook(&saved.id).unwrap().unwrap();
        assert_eq!(got.name, "Fail over 2");
        assert_eq!(got.steps.len(), 4);
        assert_eq!(s.delete_runbook(&saved.id).unwrap().unwrap().id, saved.id);
        assert!(s.delete_runbook(&saved.id).unwrap().is_none());
        assert_eq!(s.runbooks().unwrap().len(), 1);
    }

    #[test]
    fn migrates_v5_store_to_runbooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.db");
        {
            // A store as MX-5 left it: schema version 5, one macro.
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..5] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 5).unwrap();
            conn.execute(
                "INSERT INTO macros (id, name, input, created_at, updated_at)
                 VALUES ('m1', 'n', 'x', 1, 1)",
                [],
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(s.macros().unwrap().len(), 1);
        assert!(s.runbooks().unwrap().is_empty());
        s.save_runbook(&Runbook::new("rb")).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.runbooks().unwrap().len(), 1);
    }
}
