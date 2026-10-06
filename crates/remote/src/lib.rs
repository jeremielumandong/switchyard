//! Remote access: SSH sessions shared per Host, the [`RemoteFs`] file-system abstraction
//! (local now; SFTP and FTP/FTPS in milestone M4) and `~/.ssh/config` import.

pub mod fs;
pub mod ssh;
pub mod ssh_config;

pub use fs::{EntryKind, FileEntry, FsError, LocalFs, RemoteFs};
pub use ssh_config::{SshConfigHost, parse_ssh_config};
