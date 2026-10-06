//! A file-system trait shared by the local pane, SFTP and FTP.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// File-system errors.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// I/O failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Not supported by this backend.
    #[error("not supported: {0}")]
    Unsupported(&'static str),
}

/// Kind of directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// Directory.
    Dir,
    /// Regular file.
    File,
    /// Symbolic link with its target.
    Symlink(String),
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// File name.
    pub name: String,
    /// Kind.
    pub kind: EntryKind,
    /// Size in bytes (files).
    pub size: u64,
    /// Modification time, ms since epoch.
    pub modified_ms: Option<i64>,
    /// Unix permission bits.
    pub mode: Option<u32>,
}

impl FileEntry {
    /// Whether this is a directory (or a link the UI treats as one).
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir)
    }

    /// Whether the name starts with a dot.
    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    /// `ls -l`-style mode string, e.g. `drwxr-xr-x`.
    pub fn mode_string(&self) -> String {
        let Some(mode) = self.mode else {
            return String::new();
        };
        let mut s = String::with_capacity(10);
        s.push(match self.kind {
            EntryKind::Dir => 'd',
            EntryKind::Symlink(_) => 'l',
            EntryKind::File => '-',
        });
        for shift in [6, 3, 0] {
            let bits = (mode >> shift) & 7;
            s.push(if bits & 4 != 0 { 'r' } else { '-' });
            s.push(if bits & 2 != 0 { 'w' } else { '-' });
            s.push(if bits & 1 != 0 { 'x' } else { '-' });
        }
        s
    }
}

/// Operations every file backend supports.
pub trait RemoteFs: Send + Sync {
    /// Display name of the backend (`Local`, `SFTP`, ...).
    fn name(&self) -> &str;
    /// The starting directory.
    fn home(&self) -> PathBuf;
    /// List a directory, directories first then by name.
    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<FileEntry>, FsError>>;
    /// Create a directory.
    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>>;
    /// Rename or move.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<(), FsError>>;
    /// Delete a file or empty directory.
    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>>;
}

/// The local file system.
#[derive(Clone, Debug, Default)]
pub struct LocalFs;

/// Sort entries: `..`-free, directories first, then case-insensitive by name.
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

impl RemoteFs for LocalFs {
    fn name(&self) -> &str {
        "Local"
    }

    fn home(&self) -> PathBuf {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<FileEntry>, FsError>> {
        Box::pin(async move {
            let mut rd = tokio::fs::read_dir(path).await?;
            let mut out = Vec::new();
            while let Some(e) = rd.next_entry().await? {
                let meta = match tokio::fs::symlink_metadata(e.path()).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let kind = if meta.file_type().is_symlink() {
                    let target = tokio::fs::read_link(e.path())
                        .await
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // Links to directories browse like directories.
                    match tokio::fs::metadata(e.path()).await {
                        Ok(m) if m.is_dir() => EntryKind::Dir,
                        _ => EntryKind::Symlink(target),
                    }
                } else if meta.is_dir() {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    Some(meta.permissions().mode() & 0o777)
                };
                #[cfg(not(unix))]
                let mode = None;
                out.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    size: if kind == EntryKind::Dir {
                        0
                    } else {
                        meta.len()
                    },
                    kind,
                    modified_ms: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64),
                    mode,
                });
            }
            sort_entries(&mut out);
            Ok(out)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move { Ok(tokio::fs::create_dir(path).await?) })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move { Ok(tokio::fs::rename(from, to).await?) })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let meta = tokio::fs::symlink_metadata(path).await?;
            if meta.is_dir() {
                tokio::fs::remove_dir(path).await?;
            } else {
                tokio::fs::remove_file(path).await?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_fs_suite() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs;
        fs.mkdir(&dir.path().join("b_dir")).await.unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        let list = fs.list(dir.path()).await.unwrap();
        let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b_dir", ".hidden", "a.txt"]);
        assert_eq!(list[2].size, 5);
        assert!(list[1].is_hidden());
        fs.rename(&dir.path().join("a.txt"), &dir.path().join("c.txt"))
            .await
            .unwrap();
        fs.delete(&dir.path().join("c.txt")).await.unwrap();
        fs.delete(&dir.path().join("b_dir")).await.unwrap();
        assert_eq!(fs.list(dir.path()).await.unwrap().len(), 1);
    }

    #[test]
    fn mode_string() {
        let e = FileEntry {
            name: "x".into(),
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: None,
            mode: Some(0o755),
        };
        assert_eq!(e.mode_string(), "drwxr-xr-x");
    }
}
