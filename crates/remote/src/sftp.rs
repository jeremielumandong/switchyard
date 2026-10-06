//! [`RemoteFs`] over SFTP, on a channel of the Host's shared SSH session (no second login).
//!
//! Remote paths are POSIX whatever the local OS; they travel as `Path`s and are converted
//! with forward slashes here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags};

use crate::fs::{EntryKind, FileEntry, FsError, FsReader, FsWriter, RemoteFs, sort_entries};
use crate::ssh::SshConn;

/// An SFTP file system on a Host.
pub struct SftpFs {
    sftp: SftpSession,
    home: PathBuf,
    label: String,
    /// Keeps the SSH session up while the file system is in use.
    _conn: Arc<SshConn>,
}

impl std::fmt::Debug for SftpFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpFs")
            .field("label", &self.label)
            .finish()
    }
}

fn err(e: SftpError) -> FsError {
    FsError::Remote(match e {
        SftpError::Status(s) => {
            let msg = s.error_message.trim();
            if msg.is_empty() {
                format!("{:?}", s.status_code)
            } else {
                msg.to_owned()
            }
        }
        other => other.to_string(),
    })
}

/// A remote path as the server wants it.
pub fn posix(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

fn entry(name: String, attrs: &FileAttributes, kind: EntryKind) -> FileEntry {
    FileEntry {
        name,
        size: if kind == EntryKind::Dir {
            0
        } else {
            attrs.size.unwrap_or(0)
        },
        kind,
        modified_ms: attrs.mtime.map(|t| i64::from(t) * 1000),
        mode: attrs.permissions.map(|m| m & 0o777),
    }
}

fn join(dir: &Path, name: &str) -> PathBuf {
    let d = posix(dir);
    PathBuf::from(if d.ends_with('/') {
        format!("{d}{name}")
    } else {
        format!("{d}/{name}")
    })
}

impl SftpFs {
    /// Start SFTP on `conn`. `label` names the Host in the UI.
    pub async fn open(conn: Arc<SshConn>, label: impl Into<String>) -> Result<Self, FsError> {
        let ch = conn
            .open_sftp()
            .await
            .map_err(|e| FsError::Remote(e.to_string()))?;
        let sftp = SftpSession::new(ch.into_stream()).await.map_err(err)?;
        let home = PathBuf::from(sftp.canonicalize(".").await.map_err(err)?);
        Ok(Self {
            sftp,
            home,
            label: label.into(),
            _conn: conn,
        })
    }

    /// Whether the SSH session behind it is gone.
    pub fn is_closed(&self) -> bool {
        self._conn.is_closed()
    }
}

impl RemoteFs for SftpFs {
    fn name(&self) -> &str {
        &self.label
    }

    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<FileEntry>, FsError>> {
        Box::pin(async move {
            let rd = self.sftp.read_dir(posix(path)).await.map_err(err)?;
            let mut out = Vec::new();
            for e in rd {
                let name = e.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let attrs = e.metadata();
                let ft = attrs.file_type();
                let kind = if ft.is_dir() {
                    EntryKind::Dir
                } else if ft.is_symlink() {
                    // Links to directories browse like directories.
                    let full = join(path, &name);
                    match self.sftp.metadata(posix(&full)).await {
                        Ok(m) if m.file_type().is_dir() => EntryKind::Dir,
                        _ => EntryKind::Symlink(
                            self.sftp.read_link(posix(&full)).await.unwrap_or_default(),
                        ),
                    }
                } else {
                    EntryKind::File
                };
                out.push(entry(name, &attrs, kind));
            }
            sort_entries(&mut out);
            Ok(out)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move { self.sftp.create_dir(posix(path)).await.map_err(err) })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move { self.sftp.rename(posix(from), posix(to)).await.map_err(err) })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let meta = self.sftp.symlink_metadata(posix(path)).await.map_err(err)?;
            if meta.file_type().is_dir() {
                self.sftp.remove_dir(posix(path)).await.map_err(err)
            } else {
                self.sftp.remove_file(posix(path)).await.map_err(err)
            }
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FileEntry, FsError>> {
        Box::pin(async move {
            let meta = self.sftp.metadata(posix(path)).await.map_err(err)?;
            let kind = if meta.file_type().is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            Ok(entry(crate::fs::file_name(path), &meta, kind))
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsReader, FsError>> {
        Box::pin(async move {
            let f = self.sftp.open(posix(path)).await.map_err(err)?;
            Ok(Box::new(f) as FsReader)
        })
    }

    fn open_read_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsReader, FsError>> {
        Box::pin(async move {
            use tokio::io::AsyncSeekExt as _;
            let mut f = self.sftp.open(posix(path)).await.map_err(err)?;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            Ok(Box::new(f) as FsReader)
        })
    }

    fn open_write_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        Box::pin(async move {
            use tokio::io::AsyncSeekExt as _;
            let f = self
                .sftp
                .open_with_flags(posix(path), OpenFlags::CREATE | OpenFlags::WRITE)
                .await
                .map_err(err)?;
            // Drop anything past the resume point (an interrupted write).
            let mut attrs = FileAttributes::empty();
            attrs.size = Some(offset);
            f.set_metadata(attrs).await.map_err(err)?;
            let mut f = f;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            Ok(Box::new(f) as FsWriter)
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        Box::pin(async move {
            let f = self
                .sftp
                .open_with_flags(
                    posix(path),
                    OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                )
                .await
                .map_err(err)?;
            Ok(Box::new(f) as FsWriter)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_posix_paths() {
        assert_eq!(join(Path::new("/home/a"), "x"), PathBuf::from("/home/a/x"));
        assert_eq!(join(Path::new("/"), "etc"), PathBuf::from("/etc"));
    }
}
