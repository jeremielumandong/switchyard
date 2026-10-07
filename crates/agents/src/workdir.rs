//! The run's private, empty working directory (never a user project).

use std::hash::{BuildHasher as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const PREFIX: &str = "switchyard-agent-";
/// Directories a crashed run left behind are removed after this long.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// A directory only the user can read, removed with everything in it on drop.
#[derive(Debug)]
pub(crate) struct WorkDir {
    path: PathBuf,
}

/// A name nobody else is likely to pick. Not a secret: the directory is private and
/// created exclusively.
fn unique_suffix() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{}-{:016x}", std::process::id(), h.finish())
}

impl WorkDir {
    /// A new directory under `root` (created owner-only if missing).
    pub(crate) fn create(root: &Path) -> std::io::Result<Self> {
        create_private_dir(root, true)?;
        remove_stale(root);
        loop {
            let path = root.join(format!("{PREFIX}{}", unique_suffix()));
            match create_private_dir(&path, false) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "could not remove the agent run directory");
        }
    }
}

fn create_private_dir(path: &Path, existing_ok: bool) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        b.mode(0o700);
    }
    match b.create(path) {
        Err(e) if existing_ok && e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        r => r,
    }
}

/// Remove run directories older than [`STALE_AFTER`] (left by a crash).
fn remove_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        // `symlink_metadata`: never follow a link out of the root.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if meta.is_dir() && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Where run directories go by default.
pub(crate) fn default_root() -> PathBuf {
    std::env::temp_dir().join("switchyard-agents")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_removed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let a = WorkDir::create(root.path()).unwrap();
        let b = WorkDir::create(root.path()).unwrap();
        assert_ne!(a.path(), b.path());
        assert_eq!(std::fs::read_dir(a.path()).unwrap().count(), 0, "empty");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = a.path().metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::write(a.path().join("f"), "x").unwrap();
        let p = a.path().to_path_buf();
        drop(a);
        assert!(!p.exists());
        assert!(b.path().exists());
    }
}
