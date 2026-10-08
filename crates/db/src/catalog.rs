//! Catalog (schema) introspection types.

use serde::{Deserialize, Serialize};

/// Kind of schema object.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum ObjectKind {
    /// Base table.
    #[default]
    Table,
    /// View.
    View,
    /// Materialized view.
    MaterializedView,
    /// Function.
    Function,
    /// Procedure.
    Procedure,
    /// Sequence.
    Sequence,
    /// User-defined type.
    Type,
    /// Synonym (SQL Server).
    Synonym,
    /// Login user or role (DBX-5c). Server level: listed with an empty schema.
    Role,
    /// SQL Server Agent job (DBX-5c). Server level.
    Job,
    /// PostgreSQL extension (DBX-5c). Database level, outside any schema.
    Extension,
    /// Oracle package: specification and body as one node (DBX-5c).
    Package,
    /// Snowflake stage (DBX-5c).
    Stage,
    /// Snowflake task (DBX-5c).
    Task,
    /// Snowflake pipe (DBX-5c).
    Pipe,
}

impl ObjectKind {
    /// Folder label in the schema explorer.
    pub fn folder_label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized views",
            ObjectKind::Function => "Functions",
            ObjectKind::Procedure => "Procedures",
            ObjectKind::Sequence => "Sequences",
            ObjectKind::Type => "Types",
            ObjectKind::Synonym => "Synonyms",
            ObjectKind::Role => "Users & roles",
            ObjectKind::Job => "SQL Agent jobs",
            ObjectKind::Extension => "Extensions",
            ObjectKind::Package => "Packages",
            ObjectKind::Stage => "Stages",
            ObjectKind::Task => "Tasks",
            ObjectKind::Pipe => "Pipes",
        }
    }

    /// One-letter icon used in the tree.
    pub fn icon(self) -> &'static str {
        match self {
            ObjectKind::Table => "T",
            ObjectKind::View => "V",
            ObjectKind::MaterializedView => "M",
            ObjectKind::Function => "ƒ",
            ObjectKind::Procedure => "P",
            ObjectKind::Sequence => "#",
            ObjectKind::Type => "τ",
            ObjectKind::Synonym => "≈",
            ObjectKind::Role => "R",
            ObjectKind::Job => "J",
            ObjectKind::Extension => "E",
            ObjectKind::Package => "K",
            ObjectKind::Stage => "St",
            ObjectKind::Task => "Tk",
            ObjectKind::Pipe => "Pi",
        }
    }

    /// Lives outside any schema (users and roles, Agent jobs, extensions): listed with
    /// an empty schema in a folder at the database level of the tree.
    pub fn is_server_level(self) -> bool {
        matches!(
            self,
            ObjectKind::Role | ObjectKind::Job | ObjectKind::Extension
        )
    }

    /// Whether [`IntrospectScope::Dependencies`] means anything for this kind.
    pub fn has_dependencies(self) -> bool {
        !self.is_server_level()
    }

    /// Whether `Script as CREATE` is offered: the old kinds always, the DBX-5c kinds
    /// only where the engine can produce a `CREATE` statement (not jobs or stages, whose
    /// "DDL" is a description).
    pub fn scripts_create(self) -> bool {
        !matches!(self, ObjectKind::Job | ObjectKind::Stage)
    }

    /// The DBX-5c kinds: browsed read-only (no DROP, no data, no templates), their
    /// [`ObjectInfo::detail`] is a description (role attributes, job status, version)
    /// rather than a signature.
    pub fn is_admin(self) -> bool {
        matches!(
            self,
            ObjectKind::Role
                | ObjectKind::Job
                | ObjectKind::Extension
                | ObjectKind::Package
                | ObjectKind::Stage
                | ObjectKind::Task
                | ObjectKind::Pipe
        )
    }

    /// Whether rows can be selected from this object.
    pub fn is_relation(self) -> bool {
        matches!(
            self,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
        )
    }
}

/// What to introspect. Each scope is loaded lazily when its tree node expands.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IntrospectScope {
    /// Databases on the server.
    Databases,
    /// Schemas in the connected database.
    Schemas,
    /// Objects of one kind in a schema.
    Objects {
        /// Schema name.
        schema: String,
        /// Object kind.
        kind: ObjectKind,
    },
    /// Full detail for one object.
    Detail {
        /// Schema name.
        schema: String,
        /// Object name.
        name: String,
        /// Object kind.
        kind: ObjectKind,
    },
    /// Definition and parameters of one function or procedure (Script as CREATE / EXEC).
    /// Answered with [`CatalogChunk::Detail`]: `ddl` holds the `CREATE` text and `columns`
    /// the input parameters in order (`name` may be empty for an unnamed one, `data_type`
    /// is the type); the other lists are empty.
    RoutineDefinition {
        /// Schema name.
        schema: String,
        /// Routine name.
        name: String,
        /// [`ObjectKind::Function`] or [`ObjectKind::Procedure`].
        kind: ObjectKind,
        /// The routine's [`ObjectInfo::detail`] from the tree, if any. PostgreSQL reads it
        /// as the identity arguments `(…)` to pick one overload; other engines ignore it.
        signature: Option<String>,
    },
    /// Every relation and column in the database (completion and fuzzy search).
    AllColumns,
    /// Objects whose name contains `pattern` (case-insensitive), across every schema of the
    /// connected database. Answered with [`CatalogChunk::Objects`]; never cached.
    Search {
        /// Text to look for; matched literally (LIKE wildcards are escaped).
        pattern: String,
        /// Most rows to return.
        limit: u32,
        /// Also search system schemas (`pg_catalog`, `sys`, Oracle-maintained users).
        include_system: bool,
    },
    /// What one object uses and what uses it (DBX-5a). Answered with
    /// [`CatalogChunk::Dependencies`]; a missing privilege or an unavailable source gives
    /// a [`Dependencies::hint`], not an error. Never cached (other objects' DDL changes it).
    Dependencies {
        /// Schema name.
        schema: String,
        /// Object name.
        name: String,
        /// Object kind.
        kind: ObjectKind,
    },
}

/// Version of the schema cache's keys. Bumped when a cached answer could be misread by
/// a newer build (DBX-5c added object kinds and folders): older entries are no longer
/// found and reload from the server.
pub const CATALOG_CACHE_VERSION: u32 = 2;

impl IntrospectScope {
    /// Whether the answer may be kept in the schema cache.
    pub fn is_cacheable(&self) -> bool {
        !matches!(
            self,
            IntrospectScope::Search { .. } | IntrospectScope::Dependencies { .. }
        )
    }

    /// Key of this scope in the schema cache: the scope as JSON behind
    /// [`CATALOG_CACHE_VERSION`].
    pub fn cache_key(&self) -> String {
        format!(
            "v{CATALOG_CACHE_VERSION};{}",
            serde_json::to_string(self).unwrap_or_default()
        )
    }
}

/// Kind of a search hit from the `kind` tag every engine's search SQL returns
/// (`table`, `view`, `mview`, `sequence`, `function`, `procedure`, `synonym`, `type`,
/// `package`, `extension`, `role`, `stage`, `task`, `pipe`).
pub fn search_kind(tag: &str) -> Option<ObjectKind> {
    Some(match tag.trim() {
        "table" => ObjectKind::Table,
        "view" => ObjectKind::View,
        "mview" => ObjectKind::MaterializedView,
        "sequence" => ObjectKind::Sequence,
        "function" => ObjectKind::Function,
        "procedure" => ObjectKind::Procedure,
        "synonym" => ObjectKind::Synonym,
        "type" => ObjectKind::Type,
        "package" => ObjectKind::Package,
        "extension" => ObjectKind::Extension,
        "role" => ObjectKind::Role,
        "stage" => ObjectKind::Stage,
        "task" => ObjectKind::Task,
        "pipe" => ObjectKind::Pipe,
        _ => return None,
    })
}

/// Kind of a dependency from the engine's object type text (`TABLE`, `PACKAGE BODY`,
/// `MATERIALIZED VIEW`, SQL Server `USER_TABLE`, …) or a search tag. `None` for a type
/// the explorer cannot open (an index, a trigger, a column).
pub fn dependency_kind(type_text: &str) -> Option<ObjectKind> {
    let t = type_text.trim().to_ascii_uppercase().replace('_', " ");
    Some(match t.as_str() {
        "TABLE" | "BASE TABLE" | "USER TABLE" | "EXTERNAL TABLE" | "TEMPORARY TABLE"
        | "DYNAMIC TABLE" | "ICEBERG TABLE" | "HYBRID TABLE" => ObjectKind::Table,
        "VIEW" | "SECURE VIEW" => ObjectKind::View,
        "MATERIALIZED VIEW" | "MVIEW" => ObjectKind::MaterializedView,
        "FUNCTION"
        | "SQL SCALAR FUNCTION"
        | "SQL INLINE TABLE VALUED FUNCTION"
        | "SQL TABLE VALUED FUNCTION"
        | "CLR SCALAR FUNCTION"
        | "CLR TABLE VALUED FUNCTION"
        | "EXTERNAL FUNCTION" => ObjectKind::Function,
        "PROCEDURE" | "SQL STORED PROCEDURE" | "CLR STORED PROCEDURE" => ObjectKind::Procedure,
        "SEQUENCE" | "SEQUENCE OBJECT" => ObjectKind::Sequence,
        "SYNONYM" => ObjectKind::Synonym,
        "TYPE" | "TYPE BODY" => ObjectKind::Type,
        "PACKAGE" | "PACKAGE BODY" => ObjectKind::Package,
        "STAGE" => ObjectKind::Stage,
        "TASK" => ObjectKind::Task,
        "PIPE" => ObjectKind::Pipe,
        _ => return None,
    })
}

/// A search hit as an [`ObjectInfo`], or `None` for an unknown kind tag.
pub fn search_hit(schema: String, name: String, tag: &str) -> Option<ObjectInfo> {
    Some(ObjectInfo {
        schema,
        name,
        kind: search_kind(tag)?,
        estimated_rows: None,
        detail: None,
    })
}

/// Escape character used by every search `LIKE … ESCAPE '!'` clause. `!` needs no
/// quoting in any engine's string literals, unlike a backslash.
pub const LIKE_ESCAPE: char = '!';

/// A `%…%` LIKE pattern that matches `text` literally: `%`, `_` and the escape
/// character itself are escaped with [`LIKE_ESCAPE`]. `brackets` also escapes `[`,
/// a wildcard only in SQL Server (Oracle rejects escaping anything else).
pub fn like_contains(text: &str, brackets: bool) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        if c == '%' || c == '_' || c == LIKE_ESCAPE || (brackets && c == '[') {
            out.push(LIKE_ESCAPE);
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// A schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaInfo {
    /// Name.
    pub name: String,
    /// Whether this is a system schema.
    pub is_system: bool,
}

/// A schema object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: ObjectKind,
    /// Estimated row count for relations.
    pub estimated_rows: Option<i64>,
    /// Signature or extra description (functions).
    pub detail: Option<String>,
}

/// A column of a relation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// Schema.
    pub schema: String,
    /// Relation name.
    pub table: String,
    /// Column name.
    pub name: String,
    /// Formatted type.
    pub data_type: String,
    /// Whether NULL is allowed.
    pub nullable: bool,
    /// Default expression.
    pub default: Option<String>,
    /// 1-based position.
    pub ordinal: i32,
    /// Part of the primary key.
    pub is_primary_key: bool,
    /// Column comment (`COMMENT ON COLUMN`, `MS_Description`), when the engine has one.
    #[serde(default)]
    pub comment: Option<String>,
}

/// An index.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexInfo {
    /// Name.
    pub name: String,
    /// Indexed columns or expressions.
    pub columns: Vec<String>,
    /// Unique index.
    pub is_unique: bool,
    /// Backs the primary key.
    pub is_primary: bool,
    /// Full definition.
    pub definition: String,
    /// Access method or index type (`btree`, `clustered`, `bitmap`), when known.
    #[serde(default)]
    pub method: Option<String>,
}

/// A table constraint (primary key, unique, check, exclusion).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConstraintInfo {
    /// Name.
    pub name: String,
    /// Kind (`PRIMARY KEY`, `UNIQUE`, `CHECK`, ...).
    pub kind: String,
    /// Definition.
    pub definition: String,
}

/// A foreign key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    /// Name.
    pub name: String,
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced `schema.table`.
    pub references: String,
    /// Referenced columns.
    pub referenced_columns: Vec<String>,
    /// Referential action on delete (`CASCADE`, `SET NULL`, `NO ACTION`, …), when known.
    #[serde(default)]
    pub on_delete: Option<String>,
    /// Referential action on update, when the engine has one.
    #[serde(default)]
    pub on_update: Option<String>,
}

/// A trigger and its source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerInfo {
    /// Name.
    pub name: String,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`, with `FOR EACH ROW` where the engine says so.
    pub timing: String,
    /// Firing events joined with ` OR ` (`INSERT OR UPDATE`).
    pub event: String,
    /// Full definition (`CREATE TRIGGER …`); empty when the server would not show it.
    pub definition: String,
}

/// Full detail of one object. Fields after `ddl` were added later and are optional, so
/// cached details from older versions still load.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectDetail {
    /// The object.
    pub object: ObjectInfo,
    /// Columns (relations only).
    pub columns: Vec<ColumnInfo>,
    /// Indexes.
    pub indexes: Vec<IndexInfo>,
    /// Constraints.
    pub constraints: Vec<ConstraintInfo>,
    /// Foreign keys.
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Trigger names.
    pub triggers: Vec<String>,
    /// Generated DDL.
    pub ddl: String,
    /// Total size on disk in bytes (table, indexes and TOAST/LOB data where the engine
    /// counts them); `None` when unknown or not permitted.
    #[serde(default)]
    pub size_bytes: Option<i64>,
    /// Object comment (`COMMENT ON`, `MS_Description`, `ALL_TAB_COMMENTS`).
    #[serde(default)]
    pub comment: Option<String>,
    /// Triggers with timing, events and definition (same order as `triggers`).
    #[serde(default)]
    pub trigger_details: Vec<TriggerInfo>,
}

impl ObjectDetail {
    /// The answer to [`IntrospectScope::RoutineDefinition`]: the routine's `CREATE` text
    /// in `ddl` and its input parameters as `columns`, everything else empty.
    pub fn routine(
        schema: &str,
        name: &str,
        kind: ObjectKind,
        ddl: String,
        params: Vec<(String, String)>,
    ) -> Self {
        let columns = params
            .into_iter()
            .enumerate()
            .map(|(i, (param, data_type))| ColumnInfo {
                schema: schema.to_owned(),
                table: name.to_owned(),
                name: param,
                data_type,
                nullable: true,
                default: None,
                ordinal: i as i32 + 1,
                is_primary_key: false,
                ..Default::default()
            })
            .collect();
        ObjectDetail {
            object: ObjectInfo {
                schema: schema.to_owned(),
                name: name.to_owned(),
                kind,
                estimated_rows: None,
                detail: None,
            },
            columns,
            indexes: Vec::new(),
            constraints: Vec::new(),
            foreign_keys: Vec::new(),
            triggers: Vec::new(),
            ddl,
            ..Default::default()
        }
    }
}

/// One object on either side of a dependency (DBX-5a).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DependencyInfo {
    /// Schema (owner); `db.schema` for an object in another database.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Kind, when the explorer can open it (`None` for an index, a trigger, another
    /// database's object).
    pub kind: Option<ObjectKind>,
    /// The engine's own type text (`view`, `PACKAGE BODY`, `SQL_STORED_PROCEDURE`).
    pub type_label: String,
    /// How the two are linked (`query`, `foreign key orders_customer_id_fkey`, `HARD`,
    /// `schema-bound`).
    pub dependency: String,
}

/// The answer to [`IntrospectScope::Dependencies`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Dependencies {
    /// Objects this one uses (depends on).
    pub uses: Vec<DependencyInfo>,
    /// Objects that use (depend on) this one.
    pub used_by: Vec<DependencyInfo>,
    /// A note shown above the lists: why they may be empty or incomplete (a missing
    /// privilege, Snowflake's `ACCOUNT_USAGE` latency).
    pub hint: Option<String>,
}

impl Dependencies {
    /// No rows, only `hint`.
    pub fn hint(hint: impl Into<String>) -> Self {
        Dependencies {
            hint: Some(hint.into()),
            ..Dependencies::default()
        }
    }

    /// Add one row from a `uses` / `used_by` direction tag; any other tag is ignored.
    /// Duplicate rows (same direction, object and link) are dropped.
    pub fn push(&mut self, direction: &str, dep: DependencyInfo) {
        let list = match direction.trim() {
            "uses" => &mut self.uses,
            "used_by" => &mut self.used_by,
            _ => return,
        };
        if !list.contains(&dep) {
            list.push(dep);
        }
    }
}

/// The answer to an [`IntrospectScope`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CatalogChunk {
    /// Database names.
    Databases(Vec<String>),
    /// Schemas.
    Schemas(Vec<SchemaInfo>),
    /// Objects.
    Objects(Vec<ObjectInfo>),
    /// Object detail.
    Detail(Box<ObjectDetail>),
    /// All columns of all relations.
    AllColumns(Vec<ColumnInfo>),
    /// Uses / used-by lists of one object (DBX-5a).
    Dependencies(Box<Dependencies>),
    /// The scope could not be read for a reason the user can act on (a missing
    /// privilege such as msdb access for Agent jobs): shown as a hint, never cached.
    Hint(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_contains_escapes_wildcards() {
        assert_eq!(like_contains("order", false), "%order%");
        assert_eq!(like_contains("a_b%c", false), "%a!_b!%c%");
        assert_eq!(like_contains("hey!", false), "%hey!!%");
        assert_eq!(like_contains("[x]", false), "%[x]%");
        assert_eq!(like_contains("[x]", true), "%![x]%");
        assert_eq!(like_contains("it's", false), "%it's%");
    }

    #[test]
    fn search_kind_tags() {
        assert_eq!(search_kind("mview"), Some(ObjectKind::MaterializedView));
        assert_eq!(search_kind("procedure "), Some(ObjectKind::Procedure));
        assert_eq!(search_kind("index"), None);
    }

    #[test]
    fn dependency_kinds_from_engine_type_text() {
        assert_eq!(dependency_kind("PACKAGE BODY"), Some(ObjectKind::Package));
        assert_eq!(dependency_kind("USER_TABLE"), Some(ObjectKind::Table));
        assert_eq!(
            dependency_kind("SQL_STORED_PROCEDURE"),
            Some(ObjectKind::Procedure)
        );
        assert_eq!(dependency_kind("mview"), Some(ObjectKind::MaterializedView));
        assert_eq!(dependency_kind("INDEX"), None);
        assert_eq!(dependency_kind("SQL_TRIGGER"), None);
    }

    #[test]
    fn dependencies_split_by_direction_without_duplicates() {
        let dep = |n: &str| DependencyInfo {
            schema: "s".into(),
            name: n.into(),
            ..DependencyInfo::default()
        };
        let mut d = Dependencies::default();
        d.push("uses", dep("a"));
        d.push("uses", dep("a"));
        d.push("used_by", dep("b"));
        d.push("other", dep("c"));
        assert_eq!(d.uses, [dep("a")]);
        assert_eq!(d.used_by, [dep("b")]);
        assert_eq!(Dependencies::hint("x").hint.as_deref(), Some("x"));
    }

    #[test]
    fn new_kinds_are_additive_and_server_level_ones_are_marked() {
        // Serde names of the old kinds are unchanged (cached chunks keep loading).
        assert_eq!(
            serde_json::to_string(&ObjectKind::Synonym).unwrap(),
            "\"Synonym\""
        );
        assert_eq!(
            serde_json::from_str::<ObjectKind>("\"Package\"").unwrap(),
            ObjectKind::Package
        );
        for k in [ObjectKind::Role, ObjectKind::Job, ObjectKind::Extension] {
            assert!(k.is_server_level() && k.is_admin() && !k.has_dependencies());
        }
        for k in [
            ObjectKind::Package,
            ObjectKind::Stage,
            ObjectKind::Task,
            ObjectKind::Pipe,
        ] {
            assert!(!k.is_server_level() && k.is_admin() && k.has_dependencies());
        }
        assert!(!ObjectKind::Table.is_admin());
        assert!(!ObjectKind::Job.scripts_create() && ObjectKind::Package.scripts_create());
    }

    #[test]
    fn cache_keys_carry_the_version() {
        let scope = IntrospectScope::Objects {
            schema: "public".into(),
            kind: ObjectKind::Table,
        };
        let json = serde_json::to_string(&scope).unwrap();
        let key = scope.cache_key();
        // An entry written under the old (unversioned) key is never read back.
        assert_ne!(key, json);
        assert_eq!(key, format!("v{CATALOG_CACHE_VERSION};{json}"));
        const { assert!(CATALOG_CACHE_VERSION >= 2) };
    }

    #[test]
    fn search_is_never_cached() {
        let search = IntrospectScope::Search {
            pattern: "x".into(),
            limit: 10,
            include_system: false,
        };
        assert!(!search.is_cacheable());
        assert!(IntrospectScope::Schemas.is_cacheable());
        let deps = IntrospectScope::Dependencies {
            schema: "s".into(),
            name: "t".into(),
            kind: ObjectKind::Table,
        };
        assert!(!deps.is_cacheable());
    }
}
