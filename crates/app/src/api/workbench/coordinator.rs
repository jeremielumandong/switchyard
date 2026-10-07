//! Blocking Workbench persistence commands live in the GPUI-free core
//! runtime; the panel only re-exports them.

pub use switchyard_api::runtime::workspace::{
    StorageCommand, TerminalCommand, execute, persist_terminal,
};
