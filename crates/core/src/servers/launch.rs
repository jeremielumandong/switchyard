//! Starting a remote desktop client from a [`LaunchPlan`]: files go into a private
//! temp folder (0700, files 0600) that is removed afterwards, the password is written to
//! the client's stdin (or its environment), and the client can be stopped.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use secrecy::ExposeSecret as _;
use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::rdp::LaunchPlan;

/// How long the files of a detached launch (`open`) stay for the app to read them.
pub const DETACHED_GRACE: Duration = Duration::from_secs(30);

/// Why a launch failed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The temp folder or a file could not be written.
    #[error("could not prepare the session files: {0}")]
    Files(std::io::Error),
    /// The client did not start.
    #[error("could not start {0}: {1}")]
    Spawn(String, std::io::Error),
    /// The client exited with an error at once.
    #[error("{0} exited with status {1}")]
    Exit(String, i32),
}

/// A private folder for one launch, removed on drop.
pub struct PrivateDir(PathBuf);

impl PrivateDir {
    /// Create `switchyard-desktop-<random>` in the system temp folder, readable only by
    /// this user.
    pub fn create() -> std::io::Result<Self> {
        let name = format!(
            "switchyard-desktop-{}",
            switchyard_store::random::random_hex(8)
        );
        let path = std::env::temp_dir().join(name);
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            b.mode(0o700);
        }
        b.create(&path)?;
        Ok(Self(path))
    }

    /// The folder.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write a file readable only by this user.
    pub fn write(&self, name: &str, content: &str) -> std::io::Result<PathBuf> {
        let path = self.0.join(name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        std::io::Write::write_all(&mut f, content.as_bytes())?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            warn!(error = %e, "could not remove a desktop session folder");
        }
    }
}

/// Run the client until it exits or `cancel` fires. `started` is called once it runs.
/// The folder is removed when this returns (a detached launch waits [`DETACHED_GRACE`]
/// first, so the app it handed the file to can read it).
pub async fn run(
    plan: LaunchPlan,
    dir: PrivateDir,
    cancel: CancellationToken,
    started: impl FnOnce(),
) -> Result<(), LaunchError> {
    for (name, content) in &plan.files {
        dir.write(name, content).map_err(LaunchError::Files)?;
    }
    let name = plan.client.name().to_owned();
    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args)
        .stdin(if plan.stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, v) in &plan.env {
        cmd.env(k, v.expose_secret());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| LaunchError::Spawn(name.clone(), e))?;
    if let Some(mut stdin) = child.stdin.take() {
        for line in &plan.stdin {
            let mut bytes = line.expose_secret().as_bytes().to_vec();
            bytes.push(b'\n');
            if let Err(e) = stdin.write_all(&bytes).await {
                warn!(error = %e, "client closed its input early");
                break;
            }
            bytes.fill(0);
        }
        drop(stdin);
    }
    started();
    let status = tokio::select! {
        s = child.wait() => s.ok(),
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            None
        }
    };
    if plan.detached {
        tokio::select! {
            _ = tokio::time::sleep(DETACHED_GRACE) => {}
            _ = cancel.cancelled() => {}
        }
    }
    drop(dir);
    match status {
        Some(s) if !s.success() && plan.detached => {
            Err(LaunchError::Exit(name, s.code().unwrap_or(-1)))
        }
        _ => Ok(()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::servers::rdp::Client;
    use secrecy::SecretString;

    #[test]
    fn private_folder_is_closed_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = PrivateDir::create().unwrap();
        let path = dir.path().to_owned();
        let f = dir.write("session.rdp", "full address:s:x\r\n").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(dir);
        assert!(!path.exists());
    }

    /// Launches a stub client script that records its arguments and stdin.
    #[tokio::test]
    #[ignore = "starts a process; run with --ignored"]
    async fn launches_a_stub_client() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = tempfile::tempdir().unwrap();
        let out = work.path().join("out.txt");
        let script = work.path().join("xfreerdp3");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"args: $*\" > '{o}'\nread pw\necho \"stdin: $pw\" >> '{o}'\nfor a in \"$@\"; do case \"$a\" in *.rdp) cat \"$a\" >> '{o}';; esac; done\n",
                o = out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut server =
            switchyard_store::DesktopServer::new("dc", switchyard_store::DesktopProtocol::Rdp);
        server.address = "dc.example".into();
        let dir = PrivateDir::create().unwrap();
        let folder = dir.path().to_owned();
        let plan = crate::servers::rdp::plan(
            &server,
            Client::FreeRdp { v3: true },
            &script,
            &crate::servers::rdp::Endpoint {
                host: "dc.example".into(),
                port: 3389,
            },
            &crate::servers::rdp::Login {
                user: "admin".into(),
                domain: None,
                password: Some(SecretString::from("pw-123")),
            },
            None,
            dir.path(),
        );
        let mut started = false;
        run(plan, dir, CancellationToken::new(), || started = true)
            .await
            .unwrap();
        assert!(started);
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("args: /v:dc.example:3389 /u:admin"));
        assert!(!text.lines().next().unwrap().contains("pw-123"));
        assert!(text.contains("stdin: pw-123"));
        assert!(!folder.exists(), "the private folder is removed");
    }
}
