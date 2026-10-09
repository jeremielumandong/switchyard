//! Platform directories, with portable mode.

use std::path::PathBuf;

use directories::ProjectDirs;

/// Marker file next to the executable that enables portable mode.
pub const PORTABLE_MARKER: &str = "switchyard.portable";

/// Where Switchyard keeps its files.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// Config directory (profile store, vault).
    pub config: PathBuf,
    /// Data directory (drivers, logs).
    pub data: PathBuf,
    /// Cache directory.
    pub cache: PathBuf,
    /// Whether portable mode is active.
    pub portable: bool,
}

impl AppPaths {
    /// Resolve paths: portable mode if the marker sits next to the binary, otherwise the
    /// platform's conventional directories.
    pub fn resolve() -> Option<Self> {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            && dir.join(PORTABLE_MARKER).exists()
        {
            return Some(Self::under(dir.join("switchyard-data"), true));
        }
        if let Ok(dir) = std::env::var("SWITCHYARD_HOME") {
            return Some(Self::under(PathBuf::from(dir), false));
        }
        let dirs = ProjectDirs::from("dev", "Switchyard", "Switchyard")?;
        Some(Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
            portable: false,
        })
    }

    /// All directories under one root.
    pub fn under(root: PathBuf, portable: bool) -> Self {
        Self {
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            portable,
        }
    }

    /// The profile store file.
    pub fn store_file(&self) -> PathBuf {
        self.config.join("switchyard.db")
    }

    /// The fallback vault file.
    pub fn vault_file(&self) -> PathBuf {
        self.config.join("vault.json")
    }

    /// App-managed driver directory.
    pub fn drivers_dir(&self) -> PathBuf {
        self.data.join("drivers")
    }

    /// The app's log files (`switchyard.log` and rotated copies).
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join("logs")
    }
}
