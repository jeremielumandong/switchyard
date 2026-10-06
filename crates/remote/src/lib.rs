//! Remote access: the [`RemoteFs`] file-system abstraction (local now; SFTP and FTP/FTPS in
//! milestone M4), `~/.ssh/config` import, and later SSH sessions and tunnels (M2).

pub mod fs;
pub mod ssh_config;

pub use fs::{EntryKind, FileEntry, FsError, LocalFs, RemoteFs};
pub use ssh_config::{SshConfigHost, parse_ssh_config};
