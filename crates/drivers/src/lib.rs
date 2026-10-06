//! Driver Manager: component manifests (bundled, or downloaded and minisign-verified),
//! detection, install strategies (system package, signed archive, file, manual steps) and
//! runtime loading with `libloading`.
//!
//! Every v1 protocol uses a pure-Rust driver compiled into the app, so nothing here is
//! needed to start. Optional native components only disable the feature that needs them.

pub mod archive;
pub mod detect;
pub mod error;
pub mod install;
pub mod manifest;
pub mod registry;
pub mod signature;
pub mod version;

use serde::{Deserialize, Serialize};

pub use detect::{ComponentStatus, Source};
pub use error::{DriverError, Result};
pub use install::{InstallPlan, InstallProgress};
pub use manifest::{License, Manifest};
pub use registry::{Component, Registry};

/// A built-in (compiled-in) driver, shown on Settings → Drivers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinDriver {
    /// Protocol name.
    pub protocol: &'static str,
    /// Crate implementing it.
    pub implementation: &'static str,
}

/// Drivers compiled into every build.
pub const BUILTIN_DRIVERS: &[BuiltinDriver] = &[
    BuiltinDriver {
        protocol: "PostgreSQL",
        implementation: "tokio-postgres",
    },
    BuiltinDriver {
        protocol: "SQL Server",
        implementation: "tiberius",
    },
    BuiltinDriver {
        protocol: "Cloudflare D1",
        implementation: "reqwest",
    },
    BuiltinDriver {
        protocol: "SSH",
        implementation: "russh",
    },
    BuiltinDriver {
        protocol: "SFTP",
        implementation: "russh-sftp",
    },
    BuiltinDriver {
        protocol: "FTP/FTPS",
        implementation: "suppaftp",
    },
];
