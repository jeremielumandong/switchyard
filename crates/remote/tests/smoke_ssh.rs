//! Smoke test for the docker compose `ssh` service (M0-2): a password login, a command,
//! and an SFTP round trip on the shared session. When `docker/ssh/id_ed25519` exists (see
//! `docker/ssh/README.md`) the key login is checked too.
//!
//! ```text
//! docker compose -f docker/compose.yml up -d ssh
//! cargo test -p switchyard-remote --test smoke_ssh -- --ignored
//! ```
//!
//! `SWITCHYARD_SMOKE_SSH_PORT` (2222) overrides the port. The fuller suite (`tests/ssh.rs`)
//! runs against local `sshd`s from `scripts/ssh-test-servers.sh`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use secrecy::SecretString;
use switchyard_remote::ssh::{
    HostKeyDecision, HostKeyRequest, InteractiveRequest, KnownHosts, SshAuthMethod, SshError,
    SshManager, SshPrompter, SshTarget,
};
use switchyard_remote::{RemoteFs, SftpFs};

const USER: &str = "deploy";
const PASSWORD: &str = "switchyard";

struct TrustOnce;

impl SshPrompter for TrustOnce {
    fn host_key(&self, _req: HostKeyRequest) -> BoxFuture<'static, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::TrustOnce })
    }

    fn secret(&self, _host: String, _prompt: String) -> BoxFuture<'static, Option<SecretString>> {
        Box::pin(async { None })
    }

    fn interactive(
        &self,
        req: InteractiveRequest,
    ) -> BoxFuture<'static, Option<Vec<SecretString>>> {
        let n = req.prompts.len();
        Box::pin(async move { Some(vec![SecretString::from(PASSWORD.to_owned()); n]) })
    }
}

fn target(id: &str, auth: SshAuthMethod, secret: Option<&str>) -> SshTarget {
    SshTarget {
        id: id.into(),
        label: id.into(),
        address: "127.0.0.1".into(),
        port: std::env::var("SWITCHYARD_SMOKE_SSH_PORT")
            .map(|p| p.parse().unwrap())
            .unwrap_or(2222),
        user: USER.into(),
        auth,
        secret: secret.map(|s| SecretString::from(s.to_owned())),
        keepalive: Duration::from_secs(30),
        jump: None,
        agent_socket: None,
        agent_key: None,
        forward_agent: false,
        forward_x11: false,
        x11_display: None,
    }
}

fn manager(dir: &tempfile::TempDir) -> SshManager {
    let known = KnownHosts {
        user_file: None,
        app_file: dir.path().join("known_hosts"),
    };
    SshManager::new(known, Arc::new(TrustOnce))
}

#[tokio::test]
#[ignore = "needs the docker compose ssh service"]
async fn compose_ssh_server_logs_in_runs_a_command_and_serves_sftp() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(&dir);
    let t = target("smoke-pw", SshAuthMethod::Password, Some(PASSWORD));

    // The container generates its host keys and starts sshd after `up -d` returns.
    let deadline = Instant::now() + Duration::from_secs(90);
    let conn = loop {
        match m.session(&t).await {
            Ok(c) => break c,
            Err(e @ SshError::Auth { .. }) => panic!("password login refused: {e}"),
            Err(e) if Instant::now() > deadline => panic!("ssh service not reachable: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    };
    let (code, out) = conn.exec("whoami").await.unwrap();
    assert_eq!(code, Some(0));
    assert_eq!(String::from_utf8_lossy(&out).trim(), USER);

    let fs = SftpFs::open(conn.clone(), "smoke").await.unwrap();
    let file = fs
        .home()
        .join(format!("swy-smoke-{}.txt", std::process::id()));
    fs.write_file(&file, b"smoke").await.unwrap();
    assert_eq!(fs.read_file(&file, 1024).await.unwrap(), b"smoke");
    assert!(
        fs.list(&fs.home())
            .await
            .unwrap()
            .iter()
            .any(|e| Some(e.name.as_str()) == file.file_name().and_then(|n| n.to_str()))
    );
    fs.delete(&file).await.unwrap();

    let key = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker/ssh/id_ed25519"
    ));
    if key.exists() {
        let t = target(
            "smoke-key",
            SshAuthMethod::PublicKey {
                key_path: key.display().to_string(),
            },
            None,
        );
        let conn = m.session(&t).await.expect("key login");
        let (code, _) = conn.exec("true").await.unwrap();
        assert_eq!(code, Some(0));
    }
}
