//! Remote access: SSH sessions shared per Host, the [`RemoteFs`] file-system abstraction
//! (local, SFTP, FTP/FTPS) and `~/.ssh/config` import.

pub mod fs;
pub mod ftp;
mod ftp_tls;
pub mod sftp;
pub mod ssh;
pub mod ssh_config;

pub use fs::{EntryKind, FileEntry, FsError, FsReader, FsWriter, ListPage, LocalFs, RemoteFs};
pub use ftp::{FtpConfig, FtpDataMode, FtpFs, FtpSecurity};
pub use sftp::SftpFs;
pub use ssh_config::{SshConfigHost, parse_ssh_config};
