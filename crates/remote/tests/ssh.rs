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
        forward_agent: false,
        forward_x11: false,
        x11_display: None,
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

/// A local echo server; returns its port.
async fn echo_server() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn round_trip(s: &mut tokio::net::TcpStream, msg: &[u8]) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    s.write_all(msg).await.unwrap();
    let mut got = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut got))
        .await
        .expect("echo timed out")
        .unwrap();
    got
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn remote_forward_reaches_a_local_service_and_stops() {
    use switchyard_remote::ssh::{ForwardKind, ForwardSpec, Tunnel, TunnelStatus};
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(SshManager::new(
        known(&dir),
        Prompter::new(HostKeyDecision::TrustOnce),
    ));
    let echo = echo_server().await;
    let spec = ForwardSpec::Remote {
        bind_address: "127.0.0.1".into(),
        bind_port: 0,
        host: "127.0.0.1".into(),
        port: echo,
    };
    let t = Tunnel::start(
        7,
        Some("fwd-1".into()),
        m,
        target("rfwd", main_port(), key("id_ed25519")),
        spec,
    )
    .await
    .unwrap();
    let server_port = t.port();
    assert_ne!(server_port, 0, "the server picked a port");
    // The test sshd runs on this machine, so its listening port is reachable here; the
    // bytes go client → sshd → forwarded-tcpip channel → Switchyard → echo server.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", server_port))
        .await
        .unwrap();
    assert_eq!(
        round_trip(&mut s, b"through the server").await,
        b"through the server"
    );
    let info = t.info();
    assert_eq!(info.kind, ForwardKind::Remote);
    assert_eq!(info.forward_id.as_deref(), Some("fwd-1"));
    assert_eq!(info.remote, format!("127.0.0.1:{echo}"));
    assert_eq!(info.status, TunnelStatus::Active);
    assert!(info.bytes_up >= 18 && info.bytes_down >= 18, "{info:?}");
    drop(s);

    t.stop();
    // The server stops listening once the cancel reaches it.
    let mut refused = false;
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", server_port))
            .await
            .is_err()
        {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(refused, "the server still listens after stop");
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn remote_forward_to_a_dead_target_reports_it() {
    use switchyard_remote::ssh::{ForwardSpec, Tunnel, TunnelStatus};
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(SshManager::new(
        known(&dir),
        Prompter::new(HostKeyDecision::TrustOnce),
    ));
    // A port with nothing listening.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let spec = ForwardSpec::Remote {
        bind_address: "127.0.0.1".into(),
        bind_port: 0,
        host: "127.0.0.1".into(),
        port: dead,
    };
    let t = Tunnel::start(
        8,
        None,
        m,
        target("rfwd-dead", main_port(), key("id_ed25519")),
        spec,
    )
    .await
    .unwrap();
    let s = tokio::net::TcpStream::connect(("127.0.0.1", t.port()))
        .await
        .unwrap();
    let mut status = t.info().status;
    for _ in 0..50 {
        if matches!(status, TunnelStatus::Failed(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        status = t.info().status;
    }
    assert!(
        matches!(&status, TunnelStatus::Failed(m) if m.contains(&dead.to_string())),
        "{status:?}"
    );
    drop(s);
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn dynamic_forward_is_a_socks_proxy() {
    use switchyard_remote::ssh::{ForwardKind, ForwardSpec, Tunnel};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(SshManager::new(
        known(&dir),
        Prompter::new(HostKeyDecision::TrustOnce),
    ));
    let echo = echo_server().await;
    let spec = ForwardSpec::Dynamic {
        bind_address: "127.0.0.1".into(),
        bind_port: 0,
    };
    let t = Tunnel::start(
        9,
        None,
        m,
        target("dfwd", main_port(), key("id_ed25519")),
        spec,
    )
    .await
    .unwrap();
    assert_eq!(t.info().kind, ForwardKind::Dynamic);
    assert_eq!(t.info().remote, "SOCKS");

    // SOCKS5 with a domain name, resolved by the server.
    let mut s = tokio::net::TcpStream::connect(t.local()).await.unwrap();
    s.write_all(&[5, 1, 0]).await.unwrap();
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet).await.unwrap();
    assert_eq!(greet, [5, 0]);
    let mut req = vec![5, 1, 0, 3, 9];
    req.extend_from_slice(b"localhost");
    req.extend_from_slice(&echo.to_be_bytes());
    s.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    s.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[1], 0, "SOCKS5 CONNECT succeeded: {rep:?}");
    assert_eq!(round_trip(&mut s, b"socks5").await, b"socks5");

    // SOCKS4 to an IPv4 address.
    let mut s4 = tokio::net::TcpStream::connect(t.local()).await.unwrap();
    let mut req = vec![4, 1];
    req.extend_from_slice(&echo.to_be_bytes());
    req.extend_from_slice(&[127, 0, 0, 1, 0]);
    s4.write_all(&req).await.unwrap();
    let mut rep = [0u8; 8];
    s4.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[1], 0x5A);
    assert_eq!(round_trip(&mut s4, b"socks4").await, b"socks4");

    // A target the server cannot reach is refused, not hung.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut s5 = tokio::net::TcpStream::connect(t.local()).await.unwrap();
    s5.write_all(&[5, 1, 0]).await.unwrap();
    s5.read_exact(&mut greet).await.unwrap();
    let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
    req.extend_from_slice(&dead.to_be_bytes());
    s5.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    s5.read_exact(&mut rep).await.unwrap();
    assert_ne!(rep[1], 0);
    t.stop();
}

/// A private `ssh-agent` holding the test ed25519 key; killed on drop.
struct LocalAgent {
    socket: PathBuf,
    pid: String,
    _dir: tempfile::TempDir,
}

impl LocalAgent {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent.sock");
        let out = std::process::Command::new("ssh-agent")
            .args(["-s", "-a"])
            .arg(&socket)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let pid = text
            .split(';')
            .find_map(|p| p.trim().strip_prefix("SSH_AGENT_PID="))
            .unwrap()
            .to_owned();
        let added = std::process::Command::new("ssh-add")
            .arg(keys().join("id_ed25519"))
            .env("SSH_AUTH_SOCK", &socket)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(added.success());
        Self {
            socket,
            pid,
            _dir: dir,
        }
    }
}

impl Drop for LocalAgent {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").arg(&self.pid).status();
    }
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn forwarded_agent_signs_on_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let agent = LocalAgent::start();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    // From the server, log in to the server again: the user has no private key there, so
    // only the forwarded agent can sign.
    let hop = format!(
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -p {} swy@127.0.0.1 echo signed-by-forwarded-agent",
        main_port()
    );

    let mut off = target("agent-fwd-off", main_port(), key("id_ed25519"));
    off.agent_socket = Some(agent.socket.display().to_string());
    let conn = m.session(&off).await.unwrap();
    let (code, _) = conn.exec(&hop).await.unwrap();
    assert_ne!(code, Some(0), "no forwarding, no signature");

    let mut on = target("agent-fwd-on", main_port(), key("id_ed25519"));
    on.agent_socket = Some(agent.socket.display().to_string());
    on.forward_agent = true;
    let conn = m.session(&on).await.unwrap();
    let (code, out) = conn.exec("ssh-add -l").await.unwrap();
    assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&out));
    assert!(String::from_utf8_lossy(&out).contains("swy-ed25519"));
    let (code, out) = conn.exec(&hop).await.unwrap();
    assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&out));
    assert!(String::from_utf8_lossy(&out).contains("signed-by-forwarded-agent"));
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn x11_reaches_the_local_display_without_the_fake_cookie() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A fake X server on TCP display :37 (port 6037).
    let display = tokio::net::TcpListener::bind("127.0.0.1:6037")
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(None::<(Vec<u8>, Vec<u8>)>));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        let (mut s, _) = display.accept().await.unwrap();
        let mut head = [0u8; 12];
        s.read_exact(&mut head).await.unwrap();
        assert_eq!(head[0], b'l');
        let n = usize::from(u16::from_le_bytes([head[6], head[7]]));
        let d = usize::from(u16::from_le_bytes([head[8], head[9]]));
        let mut name = vec![0u8; n + (4 - n % 4) % 4];
        let mut data = vec![0u8; d + (4 - d % 4) % 4];
        s.read_exact(&mut name).await.unwrap();
        s.read_exact(&mut data).await.unwrap();
        *seen2.lock().unwrap() = Some((name[..n].to_vec(), data[..d].to_vec()));
        s.write_all(b"X11OK").await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let mut t = target("x11-fwd", main_port(), key("id_ed25519"));
    t.forward_x11 = true;
    t.x11_display = Some("127.0.0.1:37".into());
    let conn = m.session(&t).await.unwrap();
    // On the server: an X client using the session's DISPLAY and the (fake) cookie sshd
    // stored for it.
    let client = r#"python3 - <<'PY'
import os, socket, struct, subprocess
d = os.environ["DISPLAY"]
n = int(d.split(":")[1].split(".")[0])
cookie = subprocess.check_output(["xauth", "list", d]).split()[2].decode()
name, data = b"MIT-MAGIC-COOKIE-1", bytes.fromhex(cookie)
pkt = b"l\0" + struct.pack("<HHHHxx", 11, 0, len(name), len(data))
pkt += name + b"\0" * (-len(name) % 4) + data + b"\0" * (-len(data) % 4)
s = socket.create_connection(("127.0.0.1", 6000 + n))
s.sendall(pkt)
print(s.recv(5).decode())
PY"#;
    let (code, out) = conn.exec(client).await.unwrap();
    let out = String::from_utf8_lossy(&out);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("X11OK"), "{out}");
    // No real cookie for this display here, so the fake one was stripped, never passed on.
    assert_eq!(seen.lock().unwrap().clone(), Some((Vec::new(), Vec::new())));
}

#[tokio::test]
#[ignore = "needs ssh servers"]
async fn run_command_splits_streams_caps_and_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let m = SshManager::new(known(&dir), Prompter::new(HostKeyDecision::TrustOnce));
    let t = target("run", main_port(), key("id_ed25519"));
    let conn = m.session(&t).await.unwrap();
    let out = conn
        .run_command(
            "echo out; echo err >&2; exit 3",
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(out.exit_status, Some(3));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
    assert!(!out.truncated && !out.timed_out);

    let out = conn
        .run_command("head -c 5000 /dev/zero", 100, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(out.stdout.len(), 100);
    assert!(out.truncated);

    let out = conn
        .run_command("echo start; sleep 30", 1024, Duration::from_millis(1500))
        .await
        .unwrap();
    assert!(out.timed_out);
    assert_eq!(out.stdout, b"start\n");
    // The session is still usable.
    assert_eq!(whoami(&m, &t).await.unwrap(), t.user);
}
