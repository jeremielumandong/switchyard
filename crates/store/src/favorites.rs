//! Pinned schema-tree objects ("Favorites", DBX-5e).
//!
//! A pin names an object by connection, database, schema, name and kind, so it survives
//! a reload of the tree and can outlive the object itself (the explorer then shows it as
//! missing; pins are never removed automatically).

use serde::{Deserialize, Serialize};
use switchyard_db::ObjectKind;

use crate::model::{ProfileId, ValidationError};

/// A pinned schema, relation, routine or server-level object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    /// Row id; 0 before it is stored.
    pub id: i64,
    /// Connection profile.
    pub connection_id: ProfileId,
    /// Database the object lives in (the profile's database when pinned).
    pub database: String,
    /// Schema; empty for a server-level object (users and roles, jobs, extensions).
    pub schema: String,
    /// Object name; empty for a pinned schema.
    pub name: String,
    /// Object kind; `None` pins the schema itself.
    pub kind: Option<ObjectKind>,
    /// Position in the Favorites list (ascending).
    pub position: i64,
}

impl Favorite {
    /// A pin of an object (not stored yet).
    pub fn object(
        connection_id: ProfileId,
        database: &str,
        schema: &str,
        name: &str,
        kind: ObjectKind,
    ) -> Self {
        Self {
            id: 0,
            connection_id,
            database: database.to_owned(),
            schema: schema.to_owned(),
            name: name.to_owned(),
            kind: Some(kind),
            position: 0,
        }
    }

    /// A pin of a schema (not stored yet).
    pub fn schema(connection_id: ProfileId, database: &str, schema: &str) -> Self {
        Self {
            id: 0,
            connection_id,
            database: database.to_owned(),
            schema: schema.to_owned(),
            name: String::new(),
            kind: None,
            position: 0,
        }
    }

    /// Whether `self` and `other` pin the same thing (ids and positions aside).
    pub fn same_target(&self, other: &Favorite) -> bool {
        self.connection_id == other.connection_id
            && self.database == other.database
            && self.schema == other.schema
            && self.name == other.name
            && self.kind == other.kind
    }

    /// Check the pin before storing it.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let invalid = |field: &'static str, message: &str| ValidationError {
            field,
            message: message.to_owned(),
        };
        if self.connection_id.0.is_empty() {
            return Err(invalid("connection", "a pin needs a connection"));
        }
        match self.kind {
            None if self.schema.trim().is_empty() => {
                Err(invalid("schema", "a pinned schema needs its name"))
            }
            Some(_) if self.name.trim().is_empty() => {
                Err(invalid("name", "a pinned object needs its name"))
            }
            _ => Ok(()),
        }
    }
}

/// The stored text of a pin's kind: `schema`, or the object kind's name (`Table`, …).
pub(crate) fn kind_key(kind: Option<ObjectKind>) -> String {
    match kind {
        None => "schema".to_owned(),
        Some(k) => serde_json::to_value(k)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{k:?}")),
    }
}

/// The kind of a stored pin; `None` inside for a schema, outer `None` when unknown (a
/// newer build's kind).
pub(crate) fn kind_from_key(key: &str) -> Option<Option<ObjectKind>> {
    if key == "schema" {
        return Some(None);
    }
    serde_json::from_value(serde_json::Value::String(key.to_owned()))
        .ok()
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_key() {
        for k in [
            None,
            Some(ObjectKind::Table),
            Some(ObjectKind::MaterializedView),
            Some(ObjectKind::Role),
            Some(ObjectKind::Pipe),
        ] {
            assert_eq!(kind_from_key(&kind_key(k)), Some(k));
        }
        assert_eq!(kind_key(Some(ObjectKind::Table)), "Table");
        assert_eq!(kind_from_key("Hologram"), None);
    }

    #[test]
    fn validation() {
        let c = ProfileId("c".into());
        assert!(
            Favorite::schema(c.clone(), "db", "public")
                .validate()
                .is_ok()
        );
        assert!(Favorite::schema(c.clone(), "db", " ").validate().is_err());
        let t = Favorite::object(c.clone(), "db", "public", "t", ObjectKind::Table);
        assert!(t.validate().is_ok());
        // A server-level object has no schema.
        let r = Favorite::object(c.clone(), "db", "", "app", ObjectKind::Role);
        assert!(r.validate().is_ok());
        assert!(
            Favorite::object(c, "db", "public", "", ObjectKind::View)
                .validate()
                .is_err()
        );
        assert!(
            Favorite::schema(ProfileId(String::new()), "db", "s")
                .validate()
                .is_err()
        );
    }
}
