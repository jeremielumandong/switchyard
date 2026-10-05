//! Driver Manager (milestone M3): component manifests, detection, install strategies,
//! signature and checksum verification, and runtime loading with `libloading`.
//!
//! Every v1 protocol uses a pure-Rust driver compiled into the app, so nothing here is
//! needed to start. Optional native components only disable the feature that needs them.

use serde::{Deserialize, Serialize};

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

/// Status of an optional native component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComponentStatus {
    /// Found and usable.
    Installed {
        /// Version, if known.
        version: Option<String>,
        /// Where it was found.
        location: String,
    },
    /// Not found.
    Missing,
}

/// An optional native component the Driver Manager tracks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// Manifest id (`gssapi`, `ssh-agent`, ...).
    pub id: String,
    /// Display name.
    pub name: String,
    /// The feature that needs it.
    pub needed_for: String,
    /// Detected status.
    pub status: ComponentStatus,
}

/// Detect the SSH agent through `SSH_AUTH_SOCK` (Unix).
pub fn detect_ssh_agent() -> Component {
    let status = match std::env::var("SSH_AUTH_SOCK") {
        Ok(sock) if std::path::Path::new(&sock).exists() => ComponentStatus::Installed {
            version: None,
            location: "$SSH_AUTH_SOCK".into(),
        },
        _ => ComponentStatus::Missing,
    };
    Component {
        id: "ssh-agent".into(),
        name: "SSH agent".into(),
        needed_for: "Agent-based SSH auth".into(),
        status,
    }
}

/// Look for a shared library in the usual system locations (no loading).
pub fn find_library(names: &[&str]) -> Option<String> {
    let dirs = [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
        "/usr/local/lib",
        "/opt/homebrew/lib",
    ];
    dirs.iter().find_map(|d| {
        names
            .iter()
            .map(|n| std::path::Path::new(d).join(n))
            .find(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// Detect Kerberos / GSSAPI for SQL Server integrated auth.
pub fn detect_gssapi() -> Component {
    let status = if cfg!(any(target_os = "windows", target_os = "macos")) {
        ComponentStatus::Installed {
            version: None,
            location: "built in".into(),
        }
    } else {
        match find_library(&["libgssapi_krb5.so.2", "libgssapi_krb5.so"]) {
            Some(location) => ComponentStatus::Installed {
                version: None,
                location,
            },
            None => ComponentStatus::Missing,
        }
    };
    Component {
        id: "gssapi".into(),
        name: "Kerberos / GSSAPI".into(),
        needed_for: "SQL Server integrated auth".into(),
        status,
    }
}

/// Detect every tracked component.
pub fn detect_all() -> Vec<Component> {
    vec![detect_gssapi(), detect_ssh_agent()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_runs() {
        let all = detect_all();
        assert_eq!(all.len(), 2);
        assert!(find_library(&["definitely-not-a-real-lib.so.99"]).is_none());
    }
}
