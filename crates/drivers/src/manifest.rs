//! Component manifests: what each optional native component is, how to find it, and how to
//! install it on each platform. The manifest compiled into the app is trusted; a newer one
//! from the update server is used only when its minisign signature verifies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{DriverError, Result};
use crate::signature;

/// The manifest shipped with this build.
const BUNDLED: &str = include_str!("../manifest.json");

/// A set of components.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version.
    pub schema: u32,
    /// Components.
    pub components: Vec<ComponentSpec>,
}

/// One optional native component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentSpec {
    /// Stable id (`gssapi`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// The feature that needs it.
    pub needed_for: String,
    /// Feature keys that require it (`mssql.integrated_auth`).
    #[serde(default)]
    pub required_by: Vec<String>,
    /// License, shown before download.
    #[serde(default)]
    pub license: Option<License>,
    /// How to find it.
    #[serde(default)]
    pub detect: Detect,
    /// How to get it, per platform.
    #[serde(default)]
    pub platforms: Platforms,
}

/// A component's license.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// Name (`MIT`, `Oracle Technology Network License`).
    pub name: String,
    /// Full text URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Click-through: the user must accept before download.
    #[serde(default)]
    pub accept_required: bool,
}

/// Detection rules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detect {
    /// Shared library file names, any of which will do.
    #[serde(default)]
    pub libraries: Vec<String>,
    /// Environment variables naming a path that must exist (`SSH_AUTH_SOCK`).
    #[serde(default)]
    pub env: Vec<String>,
    /// Well-known paths (`~` expands), e.g. the 1Password agent socket.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Oldest acceptable version (checked where the version is known).
    #[serde(default)]
    pub min_version: Option<String>,
}

/// Install strategy per platform.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platforms {
    /// Windows.
    #[serde(default)]
    pub windows: Option<Platform>,
    /// macOS.
    #[serde(default)]
    pub macos: Option<Platform>,
    /// Linux.
    #[serde(default)]
    pub linux: Option<Platform>,
}

impl Platforms {
    /// The entry for `os`.
    pub fn get(&self, os: Os) -> Option<&Platform> {
        match os {
            Os::Windows => self.windows.as_ref(),
            Os::Macos => self.macos.as_ref(),
            Os::Linux => self.linux.as_ref(),
        }
    }
}

/// How a component arrives on one platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "kebab-case")]
pub enum Platform {
    /// Part of the operating system.
    Builtin,
    /// A system package; needs admin rights.
    Package {
        /// Package name per manager (`apt`, `dnf`, `pacman`, `zypper`, `brew`, `winget`).
        packages: BTreeMap<String, String>,
        /// Approximate size, for the card.
        #[serde(default)]
        size: Option<String>,
    },
    /// A vendor archive unpacked into the app-managed directory (no admin rights).
    Archive {
        /// Version installed.
        version: String,
        /// Download URL.
        url: String,
        /// SHA-256 of the archive, lowercase hex.
        sha256: String,
        /// Size in bytes.
        size: u64,
        /// Folder inside the archive holding the libraries (default: its root).
        #[serde(default)]
        lib_dir: Option<String>,
    },
    /// Steps the user follows; nothing to install.
    Manual {
        /// Instructions, one per line.
        steps: Vec<String>,
    },
}

/// Operating system family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    /// Windows.
    Windows,
    /// macOS.
    Macos,
    /// Linux and other Unix-likes.
    Linux,
}

impl Os {
    /// The OS this build runs on.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

impl Manifest {
    /// The manifest compiled into this build.
    pub fn bundled() -> Self {
        // A broken bundled manifest is a build bug; the app still starts without components.
        Self::parse(BUNDLED.as_bytes()).unwrap_or(Manifest {
            schema: 1,
            components: Vec::new(),
        })
    }

    /// Parse JSON.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| DriverError::Manifest(e.to_string()))?;
        if m.schema != 1 {
            return Err(DriverError::Manifest(format!(
                "schema {} is newer than this build understands",
                m.schema
            )));
        }
        Ok(m)
    }

    /// Parse a downloaded manifest after checking its minisign signature.
    pub fn parse_signed(bytes: &[u8], signature: &str, public_key: &str) -> Result<Self> {
        signature::verify(bytes, signature, public_key)?;
        Self::parse(bytes)
    }

    /// A component by id.
    pub fn component(&self, id: &str) -> Result<&ComponentSpec> {
        self.components
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| DriverError::Unknown(id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn bundled_manifest_parses_and_matches_the_spec_shape() {
        let m = Manifest::parse(BUNDLED.as_bytes()).unwrap();
        let g = m.component("gssapi").unwrap();
        assert_eq!(g.detect.libraries[0], "libgssapi_krb5.so.2");
        assert_eq!(g.platforms.get(Os::Windows), Some(&Platform::Builtin));
        let Some(Platform::Package { packages, .. }) = g.platforms.get(Os::Linux) else {
            panic!("linux installs gssapi as a package");
        };
        assert_eq!(packages["apt"], "libgssapi-krb5-2");
        assert!(m.component("oracle").is_err());
    }

    #[test]
    fn future_schema_is_refused() {
        let err = Manifest::parse(br#"{"schema":2,"components":[]}"#).unwrap_err();
        assert!(err.to_string().contains("newer"));
    }
}
