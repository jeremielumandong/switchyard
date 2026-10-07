//! The API workspace (a Postman-style client): collections, folders and saved requests,
//! environments and variables, the request compiler, Postman / OpenAPI / HAR import and
//! export, cookies, OAuth, snippets, response diffs, collection runs, native HTTP and the
//! `pm.*` script sandbox.
//!
//! Ported from AgentOps's API Workbench (MIT, same owner): `agentops-core::workbench`, plus
//! the agent-service pieces it called over loopback HTTP (`native_routines` sending and
//! OAuth, `workbench_runtime` scripts), which run natively here. No GPUI: the app edits
//! [`SavedRequest`] values, compiles them into an ephemeral [`PreparedRequest`], and the
//! core runtime sends them.

mod compile;
mod cookie_jar;
mod diff;
pub mod http;
mod import_export;
mod model;
mod persistence_safety;
pub mod runtime;
pub mod script;
pub mod secrets;
mod service;
mod snippet;
pub(crate) mod sqlite_journal;
mod store;
mod tls;
pub mod vault;

pub use crate::secrets::{
    MemorySecretStore, SecretRef, SecretScope, SecretStore, SecretStoreError, SecretValue,
    WorkspaceSecretResolver,
};
pub use compile::{
    BASE_URL_VARIABLES, CompileContext, CompileError, SecretResolver, compile_auth_headers,
    compile_redacted_request_with_folder_chain, compile_request, compile_request_with_folder,
    compile_request_with_folder_chain, effective_auth, is_relative_url, join_base_url,
    redact_collection_run, redact_example, redact_exchange, runtime_variable,
};
pub use cookie_jar::{Cookie, CookieJar, CookieJarError, CookieMutation, SameSite};
pub use diff::{DiffEntry, DiffKind, ExchangeDiff, diff_exchanges};
pub use import_export::{
    ImportFormat, ImportOrigin, ImportReferenceResolver, ImportResult, ImportSelection,
    PortableBundle, PreparedCollectionExport, RedactedExportRequest, export_agentops_bundle,
    export_curl, export_postman_collection, export_prepared_agentops_bundle,
    export_prepared_postman_collection, import, import_with_origin,
};
pub use model::*;
pub use persistence_safety::{
    persistence_safe_collection, persistence_safe_environment, persistence_safe_folder,
    persistence_safe_saved_request,
};
pub use snippet::{SnippetLanguage, generate_snippet};
pub use store::{
    DEFAULT_HISTORY_BODY_BYTES, DEFAULT_HISTORY_ENTRIES, DEFAULT_RUN_ENTRIES,
    DEFAULT_RUN_ITEM_RESULTS, DEFAULT_RUN_RESPONSE_BODY_BYTES, HistoryBodyPolicy, HistoryQuery,
    RunStoragePolicy, StoreError, StoreResult, WorkbenchStore, WorkspaceSnapshot,
    validate_import_graph,
};
