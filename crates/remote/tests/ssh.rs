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
        agent_socket: None,
        agent_key: None,
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

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn tunnel_forwards_counts_and_stops() {
    use switchyard_remote::ssh::{Tunnel, TunnelStatus};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // An echo server the SSH server can reach.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(SshManager::new(
        known(&dir),
        Prompter::new(HostKeyDecision::TrustOnce),
    ));
    let t = Tunnel::open(
        1,
        m.clone(),
        target("tun", main_port(), key("id_ed25519")),
        "127.0.0.1".into(),
        echo_port,
    )
    .await
    .unwrap();
    assert_ne!(t.local().port(), echo_port);

    // Two connections at once share the tunnel.
    let mut a = tokio::net::TcpStream::connect(t.local()).await.unwrap();
    let mut b = tokio::net::TcpStream::connect(t.local()).await.unwrap();
    a.write_all(b"hello through ssh").await.unwrap();
    b.write_all(b"second").await.unwrap();
    let mut buf = [0u8; 64];
    let n = a.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello through ssh");
    let n = b.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"second");

    let info = t.info();
    assert_eq!(info.status, TunnelStatus::Active);
    assert_eq!(info.connections, 2);
    assert_eq!(info.bytes_up, 23);
    assert_eq!(info.bytes_down, 23);
    assert_eq!(info.remote, format!("127.0.0.1:{echo_port}"));

    // Stopping cuts live connections and refuses new ones.
    t.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let n = a.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0, "existing connection closed");
    assert!(tokio::net::TcpStream::connect(t.local()).await.is_err());
    assert_eq!(t.info().status, TunnelStatus::Stopped);
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn sftp_file_system_suite() {
    use switchyard_remote::{EntryKind, RemoteFs, SftpFs};
    use tokio::io::AsyncReadExt as _;

    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let t = target("sftp", main_port(), key("id_ed25519"));
    let conn = m.session(&t).await.unwrap();
    let fs = SftpFs::open(conn.clone(), "sftp").await.unwrap();
    assert_eq!(fs.home(), PathBuf::from(format!("/home/{}", t.user)));

    let root = fs.home().join(format!("swy-sftp-{}", std::process::id()));
    fs.mkdir(&root).await.unwrap();
    fs.mkdir(&root.join("b_dir")).await.unwrap();
    fs.write_file(&root.join("a.txt"), b"hello sftp")
        .await
        .unwrap();
    fs.write_file(&root.join(".hidden"), b"").await.unwrap();
    let list = fs.list(&root).await.unwrap();
    let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["b_dir", ".hidden", "a.txt"]);
    assert_eq!(list[2].size, 10);
    assert!(list[2].modified_ms.is_some());
    assert_eq!(list[0].kind, EntryKind::Dir);

    // Overwrite truncates; stat sees the new size.
    fs.write_file(&root.join("a.txt"), b"hi").await.unwrap();
    assert_eq!(fs.stat(&root.join("a.txt")).await.unwrap().size, 2);
    assert_eq!(
        fs.read_file(&root.join("a.txt"), 1024).await.unwrap(),
        b"hi"
    );
    assert!(
        fs.read_file(&root.join("a.txt"), 1).await.is_err(),
        "size cap"
    );

    // A 3 MB file through the streams.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    fs.write_file(&root.join("big.bin"), &big).await.unwrap();
    let mut back = Vec::new();
    fs.open_read(&root.join("big.bin"))
        .await
        .unwrap()
        .read_to_end(&mut back)
        .await
        .unwrap();
    assert!(back == big, "round trip");

    fs.rename(&root.join("a.txt"), &root.join("c.txt"))
        .await
        .unwrap();
    for f in ["c.txt", ".hidden", "big.bin", "b_dir"] {
        fs.delete(&root.join(f)).await.unwrap();
    }
    assert!(fs.list(&root).await.unwrap().is_empty());
    fs.delete(&root).await.unwrap();
    assert!(fs.stat(&root).await.is_err());

    // Same SSH session as the terminal and tunnels.
    assert!(Arc::ptr_eq(&conn, &m.session(&t).await.unwrap()));
}

/// An agent on its own socket, the way 1Password runs one: the Host names the socket
/// (`IdentityAgent`) and a public key picks which of its keys to offer.
#[tokio::test]
#[ignore = "needs ssh servers and ssh-agent"]
async fn agent_on_its_own_socket_with_a_chosen_key() {
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for k in ["id_rsa", "id_ecdsa", "id_ed25519"] {
        let ok = std::process::Command::new("ssh-add")
            .arg(keys().join(k))
            .env("SSH_AUTH_SOCK", &sock)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));

    let mut t = target("agent-sock", main_port(), SshAuthMethod::Agent);
    t.agent_socket = Some(sock.display().to_string());
    t.agent_key = Some(keys().join("id_ecdsa.pub").display().to_string());
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);
    let conn = m.session(&t).await.unwrap();
    assert!(conn.description.contains("ecdsa"), "{}", conn.description);

    // An IdentityFile pointing at a `.pub` goes through the agent too.
    let mut t = target(
        "agent-pub",
        main_port(),
        SshAuthMethod::PublicKey {
            key_path: keys().join("id_ed25519.pub").display().to_string(),
        },
    );
    t.agent_socket = Some(sock.display().to_string());
    let conn = m.session(&t).await.unwrap();
    assert!(conn.description.contains("agent"), "{}", conn.description);

    // A key the agent does not hold is reported, not silently swapped.
    let mut t = target("agent-missing", main_port(), SshAuthMethod::Agent);
    t.agent_socket = Some(sock.display().to_string());
    let other = dir.path().join("other.pub");
    std::fs::write(
        &other,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl x\n",
    )
    .unwrap();
    t.agent_key = Some(other.display().to_string());
    let Err(err) = m.session(&t).await else {
        panic!("must fail")
    };
    assert!(
        err.to_string().contains("does not hold the chosen key"),
        "{err}"
    );

    let _ = agent.kill();
    let _ = agent.wait();
}
