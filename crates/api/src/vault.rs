//! Named credentials (`{{vault.name}}`) shared by every API workspace. Only references
//! are stored; values live in Switchyard's keychain or vault through [`crate::SecretStore`].

use crate::secrets::{SecretRef, VAULT_REFERENCE_PREFIX};
mod resolve;
pub use resolve::{contains_reference, resolve_template_with};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultEntry {
    pub name: String,
    pub description: String,
}

impl VaultEntry {
    pub fn expression(&self) -> String {
        format!("{{{{vault.{}}}}}", self.name)
    }
}

pub fn vault_secret_reference(name: &str) -> Result<SecretRef, String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err("Vault names need 1–128 letters, numbers, dots, underscores or hyphens".into());
    }
    SecretRef::new(format!("{VAULT_REFERENCE_PREFIX}{name}"))
}

/// Recognizes an entire auth field containing a named reference.
pub fn parse_vault_expression(value: &str) -> Result<Option<SecretRef>, String> {
    let Some(name) = value.trim().strip_prefix("{{vault.") else {
        return Ok(None);
    };
    let name = name
        .strip_suffix("}}")
        .ok_or("Vault references use {{vault.name}}")?;
    vault_secret_reference(name.trim()).map(Some)
}

pub fn vault_reference_expression(reference: &SecretRef) -> Option<String> {
    let name = reference.as_str().strip_prefix(VAULT_REFERENCE_PREFIX)?;
    vault_secret_reference(name).ok()?;
    Some(format!("{{{{vault.{name}}}}}"))
}
