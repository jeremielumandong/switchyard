//! The API workspace's secrets, kept in Switchyard's keychain or fallback vault.
//!
//! `switchyard-api` stores only references; this adapter resolves them through the same
//! secret backend the connection profiles use. Named vault references
//! (`{{vault.name}}`) share one application-wide key; every other reference is scoped to
//! its API workspace.

use std::sync::Arc;

use secrecy::{ExposeSecret as _, SecretString};
use switchyard_api::{SecretRef, SecretScope, SecretStore, SecretStoreError, SecretValue};
use switchyard_store::SecretStore as Backend;
use switchyard_store::model::SecretRef as Key;

use crate::error::CoreError;
use crate::service::NamedSecrets;

/// Prefix of named vault references (`switchyard_api::secrets`).
const VAULT_PREFIX: &str = "switchyard.vault.";
/// Largest value accepted (as AgentOps did).
const MAX_BYTES: usize = 1024 * 1024;

/// [`SecretStore`] over the app's secret backend.
pub struct ApiSecrets {
    backend: Arc<dyn Backend>,
    /// Reads named secrets from the secret vault (a cloud store or the keychain); without
    /// it they are keychain items only.
    named: Option<NamedSecrets>,
}

impl ApiSecrets {
    /// Wrap the app's secret backend.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            named: None,
        }
    }

    /// Read named references through the secret vault, so `{{vault.name}}` can come from
    /// Azure Key Vault, AWS Secrets Manager or Parameter Store.
    pub(crate) fn with_named(mut self, named: NamedSecrets) -> Self {
        self.named = Some(named);
        self
    }

    fn key(scope: &SecretScope, reference: &SecretRef) -> Key {
        match reference.as_str().strip_prefix(VAULT_PREFIX) {
            Some(name) => Key(format!("api-vault:{name}")),
            None => Key(format!("api:{}:{}", scope.as_str(), reference.as_str())),
        }
    }
}

impl SecretStore for ApiSecrets {
    fn set_secret(
        &self,
        workspace: &SecretScope,
        reference: &SecretRef,
        value: SecretValue,
    ) -> Result<(), SecretStoreError> {
        if value.expose_secret().len() > MAX_BYTES {
            return Err(SecretStoreError::TooLarge);
        }
        let value = SecretString::from(value.expose_secret().to_owned());
        self.backend
            .set(&Self::key(workspace, reference), &value)
            .map_err(|_| SecretStoreError::BackendUnavailable)
    }

    fn get_secret(
        &self,
        workspace: &SecretScope,
        reference: &SecretRef,
    ) -> Result<SecretValue, SecretStoreError> {
        if let (Some(named), Some(name)) =
            (&self.named, reference.as_str().strip_prefix(VAULT_PREFIX))
        {
            return match named.resolve_blocking(name) {
                Ok(v) => Ok(SecretValue::new(v.expose_secret())),
                Err(CoreError::NotFound(_)) => Err(SecretStoreError::Missing),
                Err(e) => {
                    tracing::warn!(name, error = %e, "named secret unavailable");
                    Err(SecretStoreError::BackendUnavailable)
                }
            };
        }
        match self.backend.get(&Self::key(workspace, reference)) {
            Ok(Some(v)) => Ok(SecretValue::new(v.expose_secret())),
            Ok(None) => Err(SecretStoreError::Missing),
            Err(_) => Err(SecretStoreError::BackendUnavailable),
        }
    }

    fn delete_secret(
        &self,
        workspace: &SecretScope,
        reference: &SecretRef,
    ) -> Result<(), SecretStoreError> {
        self.backend
            .delete(&Self::key(workspace, reference))
            .map_err(|_| SecretStoreError::BackendUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_store::secrets::MemoryStore;

    #[test]
    fn scoped_and_named_references() {
        let s = ApiSecrets::new(Arc::new(MemoryStore::default()));
        let (a, b) = (
            SecretScope::new("a").unwrap(),
            SecretScope::new("b").unwrap(),
        );
        let local = SecretRef::new("wbsec-1").unwrap();
        s.set_secret(&a, &local, SecretValue::new("x")).unwrap();
        assert_eq!(s.get_secret(&a, &local).unwrap().expose_secret(), "x");
        assert_eq!(s.get_secret(&b, &local), Err(SecretStoreError::Missing));
        let named = switchyard_api::vault::vault_secret_reference("token").unwrap();
        s.set_secret(&a, &named, SecretValue::new("t")).unwrap();
        assert_eq!(s.get_secret(&b, &named).unwrap().expose_secret(), "t");
        s.delete_secret(&b, &named).unwrap();
        assert_eq!(s.get_secret(&a, &named), Err(SecretStoreError::Missing));
    }
}
