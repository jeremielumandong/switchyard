//! Named secrets (`{{vault.name}}`): credentials kept once and referenced by name from the
//! API workbench and from connection settings. A named secret's value is either in
//! Switchyard's keychain or fallback vault ([`NamedSecretSource::Local`]), read from
//! 1Password with the `op` CLI ([`NamedSecretSource::OnePassword`]), or read from a
//! cloud secret store when it is used ([`NamedSecretSource::Cloud`]: Azure Key Vault, AWS
//! Secrets Manager or Parameter Store, through a saved cloud connection).
//!
//! The catalog holds names, descriptions and where each value lives. Never values.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{ProfileId, SecretRef, ValidationError};
use crate::store::Store;

/// Settings key of the catalog.
const SETTING: &str = "named_secrets";

/// Prefix of a profile [`SecretRef`] that points at a named secret instead of a keychain
/// item. Profile keychain keys are `<profile id>:<purpose>`, so the two never collide.
pub const NAMED_REF_PREFIX: &str = "vault:";

/// Where a named secret's value lives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NamedSecretSource {
    /// Switchyard's keychain or fallback vault.
    #[default]
    Local,
    /// A secret in a cloud secret store, read through a saved cloud connection each time
    /// it is used (cached briefly in memory, never written to disk).
    Cloud {
        /// The cloud connection (Key Vault, Secrets Manager or Parameter Store).
        connection: ProfileId,
        /// The secret's name in that store.
        key: String,
        /// Field to take when the secret is a JSON object (Secrets Manager key/value
        /// secrets); `None` uses the whole value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
    },
    /// An item field in 1Password, read with the `op` CLI (unlocked by the 1Password
    /// desktop app, or a service account token in the environment).
    OnePassword {
        /// Secret reference: `op://<vault>/<item>/[<section>/]<field>`.
        reference: String,
        /// Account (sign-in address or id) when several are signed in.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

/// A named secret in the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedSecret {
    /// Name used in `{{vault.name}}`.
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: String,
    /// Where the value lives.
    #[serde(default)]
    pub source: NamedSecretSource,
}

impl NamedSecret {
    /// The reference to type in a field: `{{vault.name}}`.
    pub fn expression(&self) -> String {
        expression(&self.name)
    }

    /// Check the fields a user can edit.
    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        if !valid_name(&self.name) {
            return Err(ValidationError {
                field: "name",
                message: "Names need 1–128 letters, numbers, dots, underscores or hyphens".into(),
            });
        }
        if let NamedSecretSource::Cloud { key, field, .. } = &self.source {
            if key.trim().is_empty() {
                return Err(ValidationError {
                    field: "key",
                    message: "Enter the secret's name in the cloud store".into(),
                });
            }
            if field.as_deref().is_some_and(|f| f.trim().is_empty()) {
                return Err(ValidationError {
                    field: "field",
                    message: "Leave the JSON field empty to use the whole value".into(),
                });
            }
        }
        if let NamedSecretSource::OnePassword { reference, account } = &self.source {
            if !valid_op_reference(reference) {
                return Err(ValidationError {
                    field: "reference",
                    message: "Use a 1Password secret reference: op://vault/item/field".into(),
                });
            }
            if account.as_deref().is_some_and(|a| a.trim().is_empty()) {
                return Err(ValidationError {
                    field: "account",
                    message: "Leave the account empty to use the default one".into(),
                });
            }
        }
        Ok(())
    }
}

/// Whether `reference` is a 1Password secret reference (`op://vault/item/field`, with an
/// optional section): no blank parts, no whitespace at the ends, nothing that `op` could
/// take as an option.
pub fn valid_op_reference(reference: &str) -> bool {
    let Some(path) = reference.strip_prefix("op://") else {
        return false;
    };
    let parts: Vec<&str> = path.split('/').collect();
    (3..=4).contains(&parts.len())
        && parts.iter().all(|p| !p.trim().is_empty())
        && reference.trim() == reference
        && !reference.contains(['\n', '\r', '\0'])
}

/// Whether `name` can be used in `{{vault.name}}` (the API workbench's rule).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// `{{vault.name}}`.
pub fn expression(name: &str) -> String {
    format!("{{{{vault.{name}}}}}")
}

/// The name in a value that is exactly one reference (`{{vault.name}}`, spaces allowed
/// around it and the name). Anything else is a literal value.
pub fn parse_expression(value: &str) -> Option<&str> {
    let name = value
        .trim()
        .strip_prefix("{{")?
        .strip_suffix("}}")?
        .trim()
        .strip_prefix("vault.")?
        .trim();
    valid_name(name).then_some(name)
}

impl SecretRef {
    /// A profile secret that is the named secret `name`.
    pub fn named(name: &str) -> Self {
        Self(format!("{NAMED_REF_PREFIX}{name}"))
    }

    /// The named secret this points at, if it is not a keychain item.
    pub fn named_secret(&self) -> Option<&str> {
        self.0.strip_prefix(NAMED_REF_PREFIX)
    }

    /// The keychain item of a local named secret (shared with the API workbench).
    pub fn for_named(name: &str) -> Self {
        Self(format!("api-vault:{name}"))
    }
}

impl Store {
    /// The named-secret catalog, by name.
    pub fn named_secrets(&self) -> Result<Vec<NamedSecret>> {
        let mut list: Vec<NamedSecret> = self.setting(SETTING)?.unwrap_or_default();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    /// One named secret.
    pub fn named_secret(&self, name: &str) -> Result<Option<NamedSecret>> {
        Ok(self.named_secrets()?.into_iter().find(|s| s.name == name))
    }

    /// Add or replace a named secret. `previous` is its old name when it was renamed.
    pub fn save_named_secret(
        &mut self,
        secret: &NamedSecret,
        previous: Option<&str>,
    ) -> Result<()> {
        secret.validate()?;
        let mut list = self.named_secrets()?;
        list.retain(|s| s.name != secret.name && Some(s.name.as_str()) != previous);
        list.push(secret.clone());
        self.set_setting(SETTING, &list)
    }

    /// Remove a named secret. Returns it when it existed.
    pub fn delete_named_secret(&mut self, name: &str) -> Result<Option<NamedSecret>> {
        let mut list = self.named_secrets()?;
        let found = list
            .iter()
            .position(|s| s.name == name)
            .map(|i| list.remove(i));
        if found.is_some() {
            self.set_setting(SETTING, &list)?;
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud(name: &str) -> NamedSecret {
        NamedSecret {
            name: name.into(),
            description: "orders API".into(),
            source: NamedSecretSource::Cloud {
                connection: ProfileId("kv1".into()),
                key: "orders-api-key".into(),
                field: None,
            },
        }
    }

    #[test]
    fn parses_only_whole_references() {
        assert_eq!(parse_expression("{{vault.db-pass}}"), Some("db-pass"));
        assert_eq!(parse_expression("  {{ vault.a.b_c }} "), Some("a.b_c"));
        for literal in [
            "hunter2",
            "{{vault.}}",
            "{{vault.a b}}",
            "x{{vault.a}}",
            "{{vault.a}}x",
            "{{env.a}}",
        ] {
            assert_eq!(parse_expression(literal), None, "{literal}");
        }
    }

    #[test]
    fn named_refs_are_not_keychain_keys() {
        let r = SecretRef::named("db-pass");
        assert_eq!(r.named_secret(), Some("db-pass"));
        let profile = SecretRef::for_profile(&ProfileId("abc".into()), "password");
        assert_eq!(profile.named_secret(), None);
    }

    #[test]
    fn catalog_round_trip_rename_and_delete() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.named_secrets().unwrap().is_empty());
        s.save_named_secret(&cloud("b"), None).unwrap();
        s.save_named_secret(
            &NamedSecret {
                name: "a".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let names: Vec<_> = s
            .named_secrets()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(s.named_secret("b").unwrap(), Some(cloud("b")));

        s.save_named_secret(&cloud("c"), Some("b")).unwrap();
        let names: Vec<_> = s
            .named_secrets()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["a", "c"]);

        assert!(s.delete_named_secret("a").unwrap().is_some());
        assert!(s.delete_named_secret("a").unwrap().is_none());
        assert_eq!(s.named_secrets().unwrap().len(), 1);
    }

    #[test]
    fn one_password_references() {
        for ok in [
            "op://Private/Orders DB/password",
            "op://dev/api/credentials/token",
        ] {
            assert!(valid_op_reference(ok), "{ok}");
        }
        for bad in [
            "op://vault/item",
            "op://vault//field",
            "op://a/b/c/d/e",
            "vault/item/field",
            " op://v/i/f",
            "op://v/i/f\n",
        ] {
            assert!(!valid_op_reference(bad), "{bad:?}");
        }
        let s = NamedSecret {
            name: "db".into(),
            description: String::new(),
            source: NamedSecretSource::OnePassword {
                reference: "op://v/i".into(),
                account: None,
            },
        };
        assert_eq!(s.validate().unwrap_err().field, "reference");
    }

    #[test]
    fn rejects_bad_names_and_empty_keys() {
        let mut s = Store::open_in_memory().unwrap();
        let bad = NamedSecret {
            name: "has space".into(),
            ..Default::default()
        };
        assert!(s.save_named_secret(&bad, None).is_err());
        let mut no_key = cloud("x");
        if let NamedSecretSource::Cloud { key, .. } = &mut no_key.source {
            key.clear();
        }
        assert_eq!(no_key.validate().unwrap_err().field, "key");
    }
}
