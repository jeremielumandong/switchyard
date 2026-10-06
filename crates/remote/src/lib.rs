//! Remote access: SSH sessions shared per Host, the [`RemoteFs`] file-system abstraction
//! (local and SFTP; FTP/FTPS in milestone M4) and `~/.ssh/config` import.

pub mod fs;
pub mod sftp;
pub mod ssh;
pub mod ssh_config;

pub use fs::{EntryKind, FileEntry, FsError, FsReader, FsWriter, LocalFs, RemoteFs};
pub use sftp::SftpFs;
pub use ssh_config::{SshConfigHost, parse_ssh_config};
