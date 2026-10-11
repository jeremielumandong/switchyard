//! One shape for the key / value tools: App Configuration settings, Key Vault secrets,
//! Secrets Manager secrets, Parameter Store parameters and Workers KV keys.

use std::collections::BTreeMap;

use futures::future::BoxFuture;

use crate::error::{CloudError, Result};

/// What a service supports, so the UI shows only the fields that apply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KvCaps {
    /// Settings carry a label (App Configuration).
    pub labels: bool,
    /// Items have a content type.
    pub content_type: bool,
    /// Items can be locked read-only (App Configuration).
    pub locks: bool,
    /// Feature flags live among the settings (App Configuration).
    pub feature_flags: bool,
    /// Items carry tags.
    pub tags: bool,
    /// Items can be disabled (Key Vault).
    pub enabled: bool,
    /// Values are secrets: hidden until asked for, never in lists.
    pub secret_values: bool,
    /// Values come with the list (no extra request per item).
    pub values_in_list: bool,
    /// Item types to choose from on create (Parameter Store `String`, `SecureString`).
    pub kinds: Vec<&'static str>,
    /// Name of the container level (`Namespace` for Workers KV), when there is one.
    pub scope_label: Option<&'static str>,
    /// How the key filter matches (`Key prefix`, `Key filter (* wildcard)`).
    pub filter_hint: &'static str,
    /// Names may contain `/` and are shown as a path (Parameter Store).
    pub hierarchical: bool,
    /// Earlier values of an item can be listed and restored (App Configuration revisions,
    /// Key Vault versions).
    pub history: bool,
    /// Items have activation and expiry dates (Key Vault).
    pub dates: bool,
    /// Deleted items can be listed, recovered and purged (Key Vault soft-delete).
    pub recoverable: bool,
}

/// One item. `value` is set when it was loaded.
#[derive(Clone, Default, PartialEq)]
pub struct KvItem {
    /// Key or name.
    pub key: String,
    /// Label (App Configuration; `None` is the null label).
    pub label: Option<String>,
    /// Value, when loaded.
    pub value: Option<String>,
    /// Content type.
    pub content_type: Option<String>,
    /// Enabled (Key Vault).
    pub enabled: Option<bool>,
    /// Locked (App Configuration).
    pub locked: Option<bool>,
    /// Last change, ms since the epoch.
    pub modified_ms: Option<i64>,
    /// Version tag for optimistic concurrency.
    pub etag: Option<String>,
    /// Tags.
    pub tags: BTreeMap<String, String>,
    /// Type (Parameter Store `SecureString`), description, or expiry: short extra text.
    pub kind: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Version id (Key Vault).
    pub version: Option<String>,
    /// Not usable before, ms since the epoch (Key Vault).
    pub not_before_ms: Option<i64>,
    /// Expiry, ms since the epoch (Key Vault).
    pub expires_ms: Option<i64>,
}

impl std::fmt::Debug for KvItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values can be secrets: never printed.
        f.debug_struct("KvItem")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("value", &self.value.as_ref().map(|_| "…"))
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A list request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KvQuery {
    /// Namespace (Workers KV).
    pub scope: Option<String>,
    /// Key prefix or filter (empty: everything).
    pub key: String,
    /// Label filter (App Configuration; empty: any label, `\0`: no label).
    pub label: String,
    /// Continuation from the previous page.
    pub cursor: Option<String>,
    /// List deleted items instead (services with [`KvCaps::recoverable`]).
    pub deleted: bool,
}

/// One page of items.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KvPage {
    /// Items.
    pub items: Vec<KvItem>,
    /// Pass back as [`KvQuery::cursor`] for more.
    pub next: Option<String>,
}

/// A create or update.
#[derive(Clone, Default, PartialEq)]
pub struct KvWrite {
    /// Namespace (Workers KV).
    pub scope: Option<String>,
    /// Key.
    pub key: String,
    /// Label.
    pub label: Option<String>,
    /// New value.
    pub value: String,
    /// Content type.
    pub content_type: Option<String>,
    /// Tags.
    pub tags: BTreeMap<String, String>,
    /// Enabled (Key Vault).
    pub enabled: Option<bool>,
    /// Type (Parameter Store).
    pub kind: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Must not exist yet.
    pub create: bool,
    /// Must still be this version (`None`: overwrite).
    pub etag: Option<String>,
    /// Not usable before, ms since the epoch (Key Vault; `None`: no date).
    pub not_before_ms: Option<i64>,
    /// Expiry, ms since the epoch (Key Vault; `None`: no date).
    pub expires_ms: Option<i64>,
}

impl std::fmt::Debug for KvWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvWrite")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("create", &self.create)
            .finish_non_exhaustive()
    }
}

/// A key / value service.
pub trait KvService: Send + Sync {
    /// What it supports.
    fn caps(&self) -> KvCaps;
    /// Namespaces (id, title), for services that have them.
    fn scopes(&self) -> BoxFuture<'_, Result<Vec<(String, String)>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// One page of items.
    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>>;
    /// One item with its value.
    fn get<'a>(
        &'a self,
        scope: Option<&'a str>,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>>;
    /// Create or update.
    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>>;
    /// Delete (Key Vault and Secrets Manager keep deleted secrets recoverable).
    fn delete<'a>(
        &'a self,
        scope: Option<&'a str>,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>>;
    /// Lock or unlock (App Configuration).
    fn set_locked<'a>(
        &'a self,
        _key: &'a str,
        _label: Option<&'a str>,
        _locked: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::Unsupported("locks")) })
    }
    /// Earlier values of one item, newest first (App Configuration revisions).
    fn revisions<'a>(
        &'a self,
        _key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<KvItem>>> {
        Box::pin(async { Err(CloudError::Unsupported("history")) })
    }
    /// Labels in use (`None` is the null label), for services that have labels.
    fn labels(&self) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// One earlier version of an item with its value (an id from [`Self::revisions`]),
    /// for services whose revisions list no values.
    fn get_version<'a>(
        &'a self,
        _key: &'a str,
        _version: &'a str,
    ) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async { Err(CloudError::Unsupported("versions")) })
    }
    /// Bring a deleted item back.
    fn recover<'a>(&'a self, _key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::Unsupported("recovery")) })
    }
    /// Delete a deleted item for good.
    fn purge<'a>(&'a self, _key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::Unsupported("purge")) })
    }
}

/// Refuses every change; reads pass through. For read-only connections.
pub struct ReadOnlyKv(pub Box<dyn KvService>);

impl KvService for ReadOnlyKv {
    fn caps(&self) -> KvCaps {
        self.0.caps()
    }
    fn scopes(&self) -> BoxFuture<'_, Result<Vec<(String, String)>>> {
        self.0.scopes()
    }
    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        self.0.list(q)
    }
    fn get<'a>(
        &'a self,
        scope: Option<&'a str>,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>> {
        self.0.get(scope, key, label)
    }
    fn put<'a>(&'a self, _: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::ReadOnly) })
    }
    fn delete<'a>(
        &'a self,
        _: Option<&'a str>,
        _: &'a str,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::ReadOnly) })
    }
    fn set_locked<'a>(
        &'a self,
        _: &'a str,
        _: Option<&'a str>,
        _: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::ReadOnly) })
    }
    fn revisions<'a>(
        &'a self,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<KvItem>>> {
        self.0.revisions(key, label)
    }
    fn labels(&self) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        self.0.labels()
    }
    fn get_version<'a>(&'a self, key: &'a str, version: &'a str) -> BoxFuture<'a, Result<KvItem>> {
        self.0.get_version(key, version)
    }
    fn recover<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::ReadOnly) })
    }
    fn purge<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(CloudError::ReadOnly) })
    }
}

/// An error message from a JSON error body: `message`, `Message`, `error.message`,
/// `errors[0].message`, `detail` or `title`.
pub(crate) fn json_message(json: &serde_json::Value) -> Option<String> {
    use serde_json::Value as J;
    let s = |v: Option<&J>| v.and_then(J::as_str).map(str::to_owned);
    s(json.get("message"))
        .or_else(|| s(json.get("Message")))
        .or_else(|| s(json.pointer("/error/message")))
        .or_else(|| s(json.pointer("/errors/0/message")))
        .or_else(|| s(json.get("detail")))
        .or_else(|| s(json.get("title")))
}
