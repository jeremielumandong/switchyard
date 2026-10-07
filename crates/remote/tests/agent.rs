//! SSH agent sign-in on every platform, against an in-process russh server (no sshd):
//! - Unix: a private `ssh-agent` on a temporary socket.
//! - Windows: the OpenSSH agent service (`\\.\pipe\openssh-ssh-agent`, must be running).
//! - Windows: Pageant (must be running).
//!
//! Each test loads a fresh key into the agent through the agent protocol, signs in with
//! it, and removes it again. `ssh-keygen` must be on `PATH`.
//!
//! Run with `cargo test -p switchyard-remote --test agent -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::keys::{PrivateKey, PublicKey, load_secret_key};
use russh::server::Auth;
use secrecy::SecretString;
use switchyard_remote::ssh::{
    HostKeyDecision, HostKeyRequest, InteractiveRequest, KnownHosts, SshAuthMethod, SshManager,
    SshPrompter, SshTarget,
};

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
        _req: InteractiveRequest,
    ) -> BoxFuture<'static, Option<Vec<SecretString>>> {
        Box::pin(async { None })
    }
}

/// Accepts one public key, counting successful sign-ins.
#[derive(Clone)]
struct Server {
    allowed: PublicKey,
    accepted: Arc<AtomicUsize>,
}

impl russh::server::Handler for Server {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if key.key_data() == self.allowed.key_data() {
            self.accepted.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }
}

fn keygen(path: &Path) -> PrivateKey {
    let out = std::process::Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "swy-agent-test",
            "-f",
        ])
        .arg(path)
        .output()
        .expect("ssh-keygen on PATH");
    assert!(out.status.success(), "{out:?}");
    load_secret_key(path, None).unwrap()
}

/// Start the server; returns its port and the sign-in counter.
async fn serve(dir: &Path, allowed: PublicKey) -> (u16, Arc<AtomicUsize>) {
    let host_key = keygen(&dir.join("host"));
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::from_millis(10),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let server = Server {
        allowed,
        accepted: accepted.clone(),
    };
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let (config, server) = (config.clone(), server.clone());
            tokio::spawn(async move {
                if let Ok(session) = russh::server::run_stream(config, sock, server).await {
                    let _ = session.await;
                }
            });
        }
    });
    (port, accepted)
}

fn target(port: u16, agent_socket: Option<&str>) -> SshTarget {
    SshTarget {
        id: format!("agent-{port}"),
        label: "agent".into(),
        address: "127.0.0.1".into(),
        port,
        user: "swy".into(),
        auth: SshAuthMethod::Agent,
        secret: None,
        keepalive: Duration::from_secs(30),
        jump: None,
        agent_socket: agent_socket.map(str::to_owned),
        agent_key: None,
        forward_agent: false,
        forward_x11: false,
        x11_display: None,
    }
}

fn manager(dir: &Path) -> SshManager {
    SshManager::new(
        KnownHosts {
            user_file: None,
            app_file: dir.join("known_hosts"),
        },
        Arc::new(TrustOnce),
    )
}

/// Sign in through `m`; returns the session description (`swy@127.0.0.1 · agent · …`).
async fn sign_in(m: &SshManager, t: &SshTarget, accepted: &AtomicUsize) -> String {
    let conn = m.session(t).await.expect("agent sign-in");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the server saw the agent key"
    );
    conn.description.clone()
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "needs ssh-agent and ssh-keygen"]
async fn ssh_agent_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let mut agent = std::process::Command::new("ssh-agent")
        .args(["-D", "-a"])
        .arg(&sock)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("ssh-agent");
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let key = keygen(&dir.path().join("id"));
    let mut client = russh::keys::agent::client::AgentClient::connect_uds(&sock)
        .await
        .unwrap();
    client.add_identity(&key, &[]).await.unwrap();

    let (port, accepted) = serve(dir.path(), key.public_key().clone()).await;
    let m = manager(dir.path()).with_agent_socket(sock);
    let description = sign_in(&m, &target(port, None), &accepted).await;
    let _ = agent.kill();
    let _ = agent.wait();
    assert!(description.contains("· agent ·"), "{description}");
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "needs the OpenSSH agent service and ssh-keygen"]
async fn windows_openssh_agent_service() {
    let dir = tempfile::tempdir().unwrap();
    let key = keygen(&dir.path().join("id"));
    let mut client =
        russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await
            .expect("the ssh-agent service is running");
    client.add_identity(&key, &[]).await.unwrap();

    let (port, accepted) = serve(dir.path(), key.public_key().clone()).await;
    // No agent on the Host: the default order tries the service pipe first.
    let result = m_sign_in(dir.path(), port, None, &accepted).await;
    let _ = client.remove_identity(key.public_key()).await;
    assert!(result.contains("· agent ·"), "{result}");
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "needs Pageant running and ssh-keygen"]
async fn windows_pageant() {
    let dir = tempfile::tempdir().unwrap();
    let key = keygen(&dir.path().join("id"));
    let mut client = russh::keys::agent::client::AgentClient::connect_pageant()
        .await
        .expect("Pageant is running");
    client.add_identity(&key, &[]).await.unwrap();

    let (port, accepted) = serve(dir.path(), key.public_key().clone()).await;
    // `pageant` in the Host's agent field.
    let result = m_sign_in(dir.path(), port, Some("pageant"), &accepted).await;
    let _ = client.remove_identity(key.public_key()).await;
    assert!(result.contains("· Pageant ·"), "{result}");
}

#[cfg(windows)]
async fn m_sign_in(dir: &Path, port: u16, agent: Option<&str>, accepted: &AtomicUsize) -> String {
    sign_in(&manager(dir), &target(port, agent), accepted).await
}
