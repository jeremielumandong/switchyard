//! Credentials that come from outside this crate, and running the clouds' own CLIs.

use std::time::Duration;

use futures::future::BoxFuture;
use secrecy::SecretString;

use crate::error::{CloudError, Result};

/// A bearer token for one resource, from wherever the connection signs in (Microsoft
/// Entra through `core`, the Azure CLI, a saved API token).
pub trait TokenSource: Send + Sync {
    /// A token valid for at least a few minutes.
    fn token(&self) -> BoxFuture<'_, Result<SecretString>>;
}

/// A token that never changes (a saved API token).
pub struct StaticToken(pub SecretString);

impl TokenSource for StaticToken {
    fn token(&self) -> BoxFuture<'_, Result<SecretString>> {
        let t = self.0.clone();
        Box::pin(async move { Ok(t) })
    }
}

/// Longest a cloud CLI may take to print credentials.
const CLI_TIMEOUT: Duration = Duration::from_secs(90);

/// Run a cloud CLI (`aws`, `az`) and return its standard output. On Windows the CLIs are
/// batch files, so they run through `cmd /C`.
pub(crate) async fn run_cli(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(program);
        c
    } else {
        tokio::process::Command::new(program)
    };
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    run(cmd, program).await
}

/// Run a shell command line (an AWS `credential_process`).
pub(crate) async fn run_shell(line: &str) -> Result<Vec<u8>> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(line);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line);
        c
    };
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    run(cmd, "credential_process").await
}

async fn run(mut cmd: tokio::process::Command, what: &str) -> Result<Vec<u8>> {
    let out = tokio::time::timeout(CLI_TIMEOUT, cmd.output())
        .await
        .map_err(|_| CloudError::Auth(format!("{what} did not answer in time")))?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CloudError::Auth(format!("{what} is not installed or not on PATH"))
            } else {
                CloudError::Auth(format!("could not run {what}: {e}"))
            }
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        // CLIs print secrets only on stdout; stderr is safe to show.
        let tail: String = err.lines().rev().take(3).collect::<Vec<_>>().join(" · ");
        return Err(CloudError::Auth(if tail.is_empty() {
            format!("{what} failed ({})", out.status)
        } else {
            format!("{what} failed: {tail}")
        }));
    }
    Ok(out.stdout)
}
