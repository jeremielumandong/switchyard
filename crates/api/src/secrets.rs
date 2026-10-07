//! Secure storage for API workspace credentials.
//!
//! Durable records contain only [`SecretRef`] values. The app resolves them through
//! Switchyard's keychain or fallback vault (core implements [`SecretStore`]); they never
//! fall back to plaintext files or SQLite.

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};
use std::{collections::HashMap, fmt};
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) const VAULT_REFERENCE_PREFIX: &str = "switchyard.vault.";
const APPLICATION_VAULT_SCOPE: &str = "switchyard.application-vault";

/// A project or other stable consumer scope. Named vault references use the
/// application scope so every project resolves the same named credential.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretScope(String);

impl SecretScope {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("secret scope cannot be empty".into());
        }
        Ok(Self(trimmed.replace('\\', "/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The scope used when no project names one.
    pub fn default_workspace() -> Self {
        Self("default".to_owned())
    }
}

fn effective_scope<'a>(scope: &'a SecretScope, reference: &SecretRef) -> &'a str {
    if reference.as_str().starts_with(VAULT_REFERENCE_PREFIX) {
        APPLICATION_VAULT_SCOPE
    } else {
        scope.as_str()
    }
}

impl fmt::Display for SecretScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
            return Err("secret reference must be a non-empty opaque token".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A fresh reference `<prefix>-<uuid>`: valid by construction (no whitespace).
    pub fn generated(prefix: &str) -> Self {
        Self(format!("{prefix}-{}", Uuid::new_v4()))
    }
}

pub trait SecretResolver {
    fn resolve(&self, reference: &SecretRef) -> Result<String, String>;
}

use SecretScope as WorkspaceId;

/// An owned secret that clears its allocation when dropped.
///
/// This type deliberately implements neither `Display` nor serialization. Its
/// `Debug` output is always redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Explicitly borrows the secret for the shortest practical lifetime.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretValue {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Non-sensitive failures exposed by a secret backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretStoreError {
    /// The reference does not exist in its effective project or application scope.
    Missing,
    /// The operating-system vault could not complete the operation.
    BackendUnavailable,
    /// The credential exceeds the supported secure-storage size.
    TooLarge,
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("secret is unavailable"),
            Self::BackendUnavailable => formatter.write_str("secure secret storage is unavailable"),
            Self::TooLarge => {
                formatter.write_str("credential exceeds the secure storage limit (1 MiB)")
            }
        }
    }
}

impl std::error::Error for SecretStoreError {}

/// Scoped secret persistence. Named `agentops.vault.*` references always use
/// the application vault scope; other references remain local to their scope.
///
/// Implementations are fail-closed: backend failures are returned and must not
/// be replaced with plaintext persistence or an empty value.
pub trait SecretStore: Send + Sync {
    fn create_secret(
        &self,
        workspace: &WorkspaceId,
        value: SecretValue,
    ) -> Result<SecretRef, SecretStoreError> {
        let reference = SecretRef::generated("wbsec");
        self.set_secret(workspace, &reference, value)?;
        Ok(reference)
    }

    /// Creates or replaces a reference in its effective scope.
    fn set_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
        value: SecretValue,
    ) -> Result<(), SecretStoreError>;

    fn get_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
    ) -> Result<SecretValue, SecretStoreError>;

    /// Deletes a secret. Deleting an already-missing reference is successful.
    fn delete_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
    ) -> Result<(), SecretStoreError>;

    /// Reads the previous project-scoped vault location during catalog migration.
    /// Custom backends without legacy values can keep the default implementation.
    fn get_legacy_vault_secret(
        &self,
        _scope: &SecretScope,
        _reference: &SecretRef,
    ) -> Result<SecretValue, SecretStoreError> {
        Err(SecretStoreError::Missing)
    }

    /// Removes a migrated project-scoped value after its new catalog commits.
    fn delete_legacy_vault_secret(
        &self,
        _scope: &SecretScope,
        _reference: &SecretRef,
    ) -> Result<(), SecretStoreError> {
        Ok(())
    }
}

/// Deterministic, process-local backend for tests and ephemeral previews.
///
/// It is public so application tests can inject it; the app uses core's keychain store.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<(String, String), SecretValue>>,
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemorySecretStore")
            .field("entry_count", &lock(&self.values).len())
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(workspace: &WorkspaceId, reference: &SecretRef) -> (String, String) {
        (
            effective_scope(workspace, reference).to_owned(),
            reference.as_str().to_owned(),
        )
    }
}

impl SecretStore for MemorySecretStore {
    fn set_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
        value: SecretValue,
    ) -> Result<(), SecretStoreError> {
        lock(&self.values).insert(Self::key(workspace, reference), value);
        Ok(())
    }

    fn get_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
    ) -> Result<SecretValue, SecretStoreError> {
        lock(&self.values)
            .get(&Self::key(workspace, reference))
            .cloned()
            .ok_or(SecretStoreError::Missing)
    }

    fn delete_secret(
        &self,
        workspace: &WorkspaceId,
        reference: &SecretRef,
    ) -> Result<(), SecretStoreError> {
        lock(&self.values).remove(&Self::key(workspace, reference));
        Ok(())
    }

    fn get_legacy_vault_secret(
        &self,
        scope: &SecretScope,
        reference: &SecretRef,
    ) -> Result<SecretValue, SecretStoreError> {
        lock(&self.values)
            .get(&(scope.as_str().to_owned(), reference.as_str().to_owned()))
            .cloned()
            .ok_or(SecretStoreError::Missing)
    }

    fn delete_legacy_vault_secret(
        &self,
        scope: &SecretScope,
        reference: &SecretRef,
    ) -> Result<(), SecretStoreError> {
        lock(&self.values).remove(&(scope.as_str().to_owned(), reference.as_str().to_owned()));
        Ok(())
    }
}

/// Adapts a scoped store to the existing request compiler contract.
pub struct WorkspaceSecretResolver<'a> {
    workspace: &'a WorkspaceId,
    store: &'a dyn SecretStore,
}

impl<'a> WorkspaceSecretResolver<'a> {
    pub fn new(workspace: &'a WorkspaceId, store: &'a dyn SecretStore) -> Self {
        Self { workspace, store }
    }
}

impl SecretResolver for WorkspaceSecretResolver<'_> {
    fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
        self.store
            .get_secret(self.workspace, reference)
            .map(|value| value.expose_secret().to_owned())
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_vault_create_update_and_delete_are_shared_between_projects() {
        let store = MemorySecretStore::new();
        let alpha = SecretScope::new("alpha").unwrap();
        let beta = SecretScope::new("beta").unwrap();
        let named = crate::vault::vault_secret_reference("token").unwrap();
        store
            .set_secret(&alpha, &named, SecretValue::new("first"))
            .unwrap();
        assert_eq!(
            WorkspaceSecretResolver::new(&beta, &store)
                .resolve(&named)
                .unwrap(),
            "first"
        );
        store
            .set_secret(&beta, &named, SecretValue::new("second"))
            .unwrap();
        assert_eq!(
            WorkspaceSecretResolver::new(&alpha, &store)
                .resolve(&named)
                .unwrap(),
            "second"
        );
        store.delete_secret(&beta, &named).unwrap();
        assert_eq!(
            store.get_secret(&alpha, &named),
            Err(SecretStoreError::Missing)
        );
    }

    #[test]
    fn secret_value_never_exposes_its_contents_through_debug() {
        let value = SecretValue::new("correct horse battery staple");
        let rendered = format!("{value:?}");
        assert_eq!(rendered, "SecretValue(<redacted>)");
        assert!(!rendered.contains(value.expose_secret()));
    }

    #[test]
    fn memory_store_is_workspace_scoped_and_delete_is_idempotent() {
        let store = MemorySecretStore::new();
        let alpha = WorkspaceId::new("alpha").unwrap();
        let beta = WorkspaceId::new("beta").unwrap();
        let reference = store
            .create_secret(&alpha, SecretValue::new("alpha-value"))
            .unwrap();

        assert_eq!(
            store
                .get_secret(&alpha, &reference)
                .unwrap()
                .expose_secret(),
            "alpha-value"
        );
        assert_eq!(
            store.get_secret(&beta, &reference),
            Err(SecretStoreError::Missing)
        );
        store.delete_secret(&alpha, &reference).unwrap();
        store.delete_secret(&alpha, &reference).unwrap();
        assert_eq!(
            store.get_secret(&alpha, &reference),
            Err(SecretStoreError::Missing)
        );
    }

    #[test]
    fn set_creates_and_replaces_a_specific_scoped_reference() {
        let store = MemorySecretStore::new();
        let workspace = WorkspaceId::new("project").unwrap();
        let unknown = SecretRef::new("unknown").unwrap();

        assert_eq!(
            store.set_secret(&workspace, &unknown, SecretValue::new("first")),
            Ok(())
        );
        assert_eq!(
            store
                .get_secret(&workspace, &unknown)
                .unwrap()
                .expose_secret(),
            "first"
        );
        store
            .set_secret(&workspace, &unknown, SecretValue::new("second"))
            .unwrap();
        assert_eq!(
            store
                .get_secret(&workspace, &unknown)
                .unwrap()
                .expose_secret(),
            "second"
        );
    }

    #[test]
    fn compiler_adapter_fails_closed_for_missing_secrets() {
        let store = MemorySecretStore::new();
        let workspace = WorkspaceId::new("project").unwrap();
        let resolver = WorkspaceSecretResolver::new(&workspace, &store);

        assert_eq!(
            resolver.resolve(&SecretRef::new("missing").unwrap()),
            Err("secret is unavailable".to_owned())
        );
    }
}
