//! SSH integration tests. They need OpenSSH servers: the docker `openssh` service, or
//! local `sshd`s, described by environment variables:
//!
//! - `SWITCHYARD_SSH_HOST` (default `127.0.0.1`), `SWITCHYARD_SSH_PORT` (2222)
//! - `SWITCHYARD_SSH_JUMP_PORTS` (`2223,2224`): two more servers for a two-hop chain
//! - `SWITCHYARD_SSH_USER` (`swy`), `SWITCHYARD_SSH_PASSWORD` (`swypass`)
//! - `SWITCHYARD_SSH_KEYS`: directory with `id_ed25519`, `id_ecdsa`, `id_rsa` and
//!   `id_ed25519_enc` (passphrase `keypass`), all in the user's `authorized_keys`
//!
//! Run with `cargo test -p switchyard-remote --test ssh -- --ignored --test-threads 1`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::ChannelMsg;
use secrecy::SecretString;
use switchyard_remote::ssh::{
    HostKeyDecision, HostKeyRequest, InteractiveRequest, KnownHosts, SshAuthMethod, SshError,
    SshManager, SshPrompter, SshTarget,
};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn keys() -> PathBuf {
    PathBuf::from(env("SWITCHYARD_SSH_KEYS", "/tmp/switchyard-ssh"))
}

struct Prompter {
    decision: HostKeyDecision,
    host_prompts: AtomicUsize,
    interactive: Mutex<Vec<InteractiveRequest>>,
    secret: Option<String>,
}

impl Prompter {
    fn new(decision: HostKeyDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            host_prompts: AtomicUsize::new(0),
            interactive: Mutex::default(),
            secret: None,
        })
    }
}

impl SshPrompter for Prompter {
    fn host_key(&self, _req: HostKeyRequest) -> BoxFuture<'static, HostKeyDecision> {
        self.host_prompts.fetch_add(1, Ordering::SeqCst);
        let d = self.decision;
        Box::pin(async move { d })
    }

    fn secret(&self, _host: String, _prompt: String) -> BoxFuture<'static, Option<SecretString>> {
        let s = self.secret.clone().map(SecretString::from);
        Box::pin(async move { s })
    }

    fn interactive(
        &self,
        req: InteractiveRequest,
    ) -> BoxFuture<'static, Option<Vec<SecretString>>> {
        let n = req.prompts.len();
        self.interactive.lock().unwrap().push(req);
        let pw = env("SWITCHYARD_SSH_PASSWORD", "swypass");
        Box::pin(async move { Some(vec![SecretString::from(pw); n]) })
    }
}

fn target(id: &str, port: u16, auth: SshAuthMethod) -> SshTarget {
    SshTarget {
        id: id.into(),
        label: id.into(),
        address: env("SWITCHYARD_SSH_HOST", "127.0.0.1"),
        port,
        user: env("SWITCHYARD_SSH_USER", "swy"),
        auth,
        secret: None,
        keepalive: Duration::from_secs(30),
        jump: None,
    }
}

fn main_port() -> u16 {
    env("SWITCHYARD_SSH_PORT", "2222").parse().unwrap()
}

fn jump_ports() -> Vec<u16> {
    env("SWITCHYARD_SSH_JUMP_PORTS", "2223,2224")
        .split(',')
        .map(|p| p.trim().parse().unwrap())
        .collect()
}

fn known(dir: &tempfile::TempDir) -> KnownHosts {
    KnownHosts {
        user_file: None,
        app_file: dir.path().join("known_hosts"),
    }
}

fn key(name: &str) -> SshAuthMethod {
    SshAuthMethod::PublicKey {
        key_path: keys().join(name).display().to_string(),
    }
}

async fn whoami(m: &SshManager, t: &SshTarget) -> Result<String, SshError> {
    let conn = m.session(t).await?;
    let (code, out) = conn.exec("whoami").await?;
    assert_eq!(code, Some(0));
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn password_auth() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let mut t = target("pw", main_port(), SshAuthMethod::Password);
    t.secret = Some(SecretString::from(env(
        "SWITCHYARD_SSH_PASSWORD",
        "swypass",
    )));
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);

    let mut bad = target("pw-bad", main_port(), SshAuthMethod::Password);
    bad.secret = Some(SecretString::from("wrong".to_owned()));
    assert!(matches!(whoami(&m, &bad).await, Err(SshError::Auth { .. })));
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn public_key_auth_for_each_key_type() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustAndSave));
    for (name, kind) in [
        ("id_ed25519", "ed25519"),
        ("id_ecdsa", "ecdsa"),
        ("id_rsa", "rsa"),
    ] {
        let t = target(name, main_port(), key(name));
        assert_eq!(whoami(&m, &t).await.unwrap(), t.user, "{name}");
        let conn = m.session(&t).await.unwrap();
        assert!(conn.description.contains(kind), "{}", conn.description);
    }
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn encrypted_key_uses_stored_or_prompted_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let mut t = target("enc", main_port(), key("id_ed25519_enc"));
    t.secret = Some(SecretString::from("keypass".to_owned()));
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);

    let prompter = Arc::new(Prompter {
        decision: HostKeyDecision::TrustOnce,
        host_prompts: AtomicUsize::new(0),
        interactive: Mutex::default(),
        secret: Some("keypass".into()),
    });
    let m = SshManager::new(known(&dir), prompter);
    let t = target("enc-prompt", main_port(), key("id_ed25519_enc"));
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);

    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let t = target("enc-cancel", main_port(), key("id_ed25519_enc"));
    assert!(matches!(whoami(&m, &t).await, Err(SshError::Cancelled)));
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn keyboard_interactive_auth() {
    let dir = tempfile::tempdir().unwrap();
    let prompter = Prompter::new(HostKeyDecision::TrustOnce);
    let m = SshManager::new(known(&dir), prompter.clone());
    let t = target("kbd", main_port(), SshAuthMethod::KeyboardInteractive);
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);
    let asked = prompter.interactive.lock().unwrap();
    assert!(!asked.is_empty());
    assert!(
        asked[0].prompts.iter().all(|(_, echo)| !echo),
        "password prompts do not echo"
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "needs ssh servers"]
async fn agent_auth() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let mut agent = std::process::Command::new("ssh-agent")
        .args(["-D", "-a"])
        .arg(&sock)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("ssh-agent");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let added = std::process::Command::new("ssh-add")
        .arg(keys().join("id_ecdsa"))
        .env("SSH_AUTH_SOCK", &sock)
        .output()
        .expect("ssh-add");
    assert!(added.status.success(), "{added:?}");
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce))
        .with_agent_socket(sock);
    let t = target("agent", main_port(), SshAuthMethod::Agent);
    let r = whoami(&m, &t).await;
    let _ = agent.kill();
    let _ = agent.wait();
    assert_eq!(r.unwrap(), t.user);
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn host_keys_known_unknown_and_changed() {
    let dir = tempfile::tempdir().unwrap();
    // Unknown + reject: no connection, nothing stored.
    let reject = Prompter::new(HostKeyDecision::Reject);
    let m = SshManager::new(known(&dir), reject.clone());
    let t = target("hk", main_port(), key("id_ed25519"));
    assert!(matches!(
        whoami(&m, &t).await,
        Err(SshError::HostKeyRejected(_))
    ));
    assert_eq!(reject.host_prompts.load(Ordering::SeqCst), 1);
    assert!(!dir.path().join("known_hosts").exists());

    // Unknown + trust and save: prompted once, then known.
    let save = Prompter::new(HostKeyDecision::TrustAndSave);
    let m = SshManager::new(known(&dir), save.clone());
    whoami(&m, &t).await.unwrap();
    let m2 = SshManager::new(known(&dir), save.clone());
    whoami(&m2, &t).await.unwrap();
    assert_eq!(
        save.host_prompts.load(Ordering::SeqCst),
        1,
        "second connect is silent"
    );

    // Changed: store another server's key for this host; the connection is blocked.
    let jump_known = dir.path().join("jump_known");
    let m3 = SshManager::new(
        KnownHosts {
            user_file: None,
            app_file: jump_known.clone(),
        },
        Prompter::new(HostKeyDecision::TrustAndSave),
    );
    whoami(&m3, &target("j", jump_ports()[0], key("id_ed25519")))
        .await
        .unwrap();
    let other_key = std::fs::read_to_string(&jump_known).unwrap();
    let other_key = other_key
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    let stored = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
    let host_field = stored.split_whitespace().next().unwrap().to_owned();
    std::fs::write(
        dir.path().join("known_hosts"),
        format!("{host_field} {other_key}\n"),
    )
    .unwrap();
    let prompts = Prompter::new(HostKeyDecision::TrustAndSave);
    let m = SshManager::new(known(&dir), prompts.clone());
    match whoami(&m, &t).await {
        Err(SshError::HostKeyChanged {
            stored, received, ..
        }) => assert_ne!(stored, received),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        prompts.host_prompts.load(Ordering::SeqCst),
        0,
        "no prompt on a changed key"
    );
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn two_hop_jump_chain() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let ports = jump_ports();
    let first = target("bastion", main_port(), key("id_ed25519"));
    let mut second = target("inner", ports[0], key("id_ecdsa"));
    second.jump = Some(Box::new(first));
    let mut last = target("db-host", ports[1], key("id_rsa"));
    last.jump = Some(Box::new(second));
    let conn = m.session(&last).await.unwrap();
    let (_, out) = conn.exec("echo $SSH_CONNECTION").await.unwrap();
    // The last hop sees the connection coming from the server itself, not from us.
    let seen = String::from_utf8_lossy(&out);
    // SSH_CONNECTION = client ip, client port, server ip, server port.
    assert_eq!(
        seen.split_whitespace().nth(3),
        Some(ports[1].to_string().as_str()),
        "{seen}"
    );
    assert!(
        conn.description.contains("via inner"),
        "{}",
        conn.description
    );
    assert!(m.is_connected("bastion") && m.is_connected("inner"));
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn sessions_are_shared_per_host() {
    let dir = tempfile::tempdir().unwrap();
    let prompter = Prompter::new(HostKeyDecision::TrustOnce);
    let m = SshManager::new(known(&dir), prompter.clone());
    let t = target("shared", main_port(), key("id_ed25519"));
    let (a, b) = tokio::join!(m.session(&t), m.session(&t));
    assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
    assert_eq!(prompter.host_prompts.load(Ordering::SeqCst), 1, "one login");
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn interactive_shell() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let conn = m
        .session(&target("shell", main_port(), key("id_ed25519")))
        .await
        .unwrap();
    let mut ch = conn.open_shell(80, 24).await.unwrap();
    ch.data(&b"stty size; echo marker-$((40+2)); exit 7\n"[..])
        .await
        .unwrap();
    let mut out = Vec::new();
    let mut code = None;
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("24 80"), "{text}");
    assert!(text.contains("marker-42"), "{text}");
    assert_eq!(code, Some(7));
}
