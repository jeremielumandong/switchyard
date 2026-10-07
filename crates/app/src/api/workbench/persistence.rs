//! Durable project-scoped Workbench state lives in the GPUI-free core
//! runtime; the panel only re-exports it.

pub use switchyard_api::runtime::workspace::WorkspaceData;
