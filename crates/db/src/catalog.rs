//! Catalog (schema) introspection types.

use serde::{Deserialize, Serialize};

/// Kind of schema object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKind {
    /// Base table.
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
        }
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
}

impl IntrospectScope {
    /// Whether the answer may be kept in the schema cache.
    pub fn is_cacheable(&self) -> bool {
        !matches!(self, IntrospectScope::Search { .. })
    }
}

/// Kind of a search hit from the `kind` tag every engine's search SQL returns
/// (`table`, `view`, `mview`, `sequence`, `function`, `procedure`, `synonym`, `type`).
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
}

/// An index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
}

/// A table constraint (primary key, unique, check, exclusion).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConstraintInfo {
    /// Name.
    pub name: String,
    /// Kind (`PRIMARY KEY`, `UNIQUE`, `CHECK`, ...).
    pub kind: String,
    /// Definition.
    pub definition: String,
}

/// A foreign key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    /// Name.
    pub name: String,
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced `schema.table`.
    pub references: String,
    /// Referenced columns.
    pub referenced_columns: Vec<String>,
}

/// Full detail of one object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    fn search_is_never_cached() {
        let search = IntrospectScope::Search {
            pattern: "x".into(),
            limit: 10,
            include_system: false,
        };
        assert!(!search.is_cacheable());
        assert!(IntrospectScope::Schemas.is_cacheable());
    }
}
