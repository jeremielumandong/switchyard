//! FTP / FTPS integration tests against the docker services (`docker/ftp/README.md`):
//!
//! ```text
//! scripts/ftp-test-certs.sh
//! docker compose -f docker/compose.yml up -d ftp ftp-implicit ftp-plain
//! SWITCHYARD_FTP_CA=docker/ftp/ca.pem \
//!   cargo test -p switchyard-remote --test ftp -- --ignored --test-threads 1
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use secrecy::SecretString;
use switchyard_remote::{EntryKind, FtpConfig, FtpDataMode, FtpFs, FtpSecurity, RemoteFs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// `SWITCHYARD_FTP_CA` (relative to the workspace root), or `docker/ftp/ca.pem`.
fn ca() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root
        .join(std::env::var("SWITCHYARD_FTP_CA").unwrap_or_else(|_| "docker/ftp/ca.pem".into()));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("test CA {}: {e}", path.display()))
}

fn config(security: FtpSecurity, mode: FtpDataMode) -> FtpConfig {
    let port = match (security, mode) {
        (FtpSecurity::None, _) => 2120,
        (FtpSecurity::Explicit, _) => 2121,
        (FtpSecurity::Implicit, _) => 2990,
    };
    let mut cfg = FtpConfig::new(
        "localhost",
        port,
        "deploy",
        SecretString::from("switchyard"),
    );
    cfg.security = security;
    cfg.mode = mode;
    if security != FtpSecurity::None {
        cfg.trusted_ca_pem = Some(ca());
    }
    cfg
}

/// Connect, retrying while the container starts.
async fn connect(cfg: FtpConfig) -> FtpFs {
    let mut last = None;
    for _ in 0..30 {
        match FtpFs::connect(cfg.clone(), "ftp").await {
            Ok(fs) => return fs,
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("FTP server on {} not reachable: {last:?}", cfg.port);
}

/// The suite every `RemoteFs` passes (see the local and SFTP suites), plus resume.
async fn suite(fs: &FtpFs, tag: &str) {
    assert_eq!(fs.home(), PathBuf::from("/home/deploy"));
    let root = fs
        .home()
        .join(format!("swy-ftp-{tag}-{}", std::process::id()));
    fs.mkdir(&root).await.unwrap();
    fs.mkdir(&root.join("b_dir")).await.unwrap();
    fs.write_file(&root.join("a.txt"), b"hello ftp!")
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
    let st = fs.stat(&root.join("a.txt")).await.unwrap();
    assert_eq!((st.size, st.kind.clone()), (2, EntryKind::File));
    assert!(st.modified_ms.is_some());
    assert_eq!(
        fs.stat(&root.join("b_dir")).await.unwrap().kind,
        EntryKind::Dir
    );
    assert!(fs.stat(&root.join("missing")).await.is_err());
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

    // Resume: download from an offset (REST + RETR) ...
    let mut tail = Vec::new();
    fs.open_read_from(&root.join("big.bin"), 1_000_000)
        .await
        .unwrap()
        .read_to_end(&mut tail)
        .await
        .unwrap();
    assert!(tail == big[1_000_000..], "download resumed at the offset");
    // ... and an interrupted upload continued from its last byte (APPE).
    let part = root.join("up.bin.swypart");
    let mut w = fs.create(&part).await.unwrap();
    w.write_all(&big[..1_234_567]).await.unwrap();
    w.shutdown().await.unwrap();
    drop(w);
    let have = fs.stat(&part).await.unwrap().size;
    assert_eq!(have, 1_234_567);
    let mut w = fs.open_write_from(&part, have).await.unwrap();
    w.write_all(&big[have as usize..]).await.unwrap();
    w.shutdown().await.unwrap();
    drop(w);
    let mut back = Vec::new();
    fs.open_read(&part)
        .await
        .unwrap()
        .read_to_end(&mut back)
        .await
        .unwrap();
    assert!(back == big, "upload resumed");
    // Several transfers at once (the queue runs 4): each has its own connection.
    let reads = (0..4).map(|_| async {
        let mut v = Vec::new();
        fs.open_read(&root.join("big.bin"))
            .await
            .unwrap()
            .read_to_end(&mut v)
            .await
            .unwrap();
        v.len()
    });
    assert_eq!(futures::future::join_all(reads).await, [3_000_000; 4]);

    fs.rename(&root.join("a.txt"), &root.join("c d.txt"))
        .await
        .unwrap();
    assert!(
        fs.list(&root)
            .await
            .unwrap()
            .iter()
            .any(|e| e.name == "c d.txt"),
        "names with spaces"
    );
    for f in ["c d.txt", ".hidden", "big.bin", "up.bin.swypart", "b_dir"] {
        fs.delete(&root.join(f)).await.unwrap();
    }
    assert!(fs.list(&root).await.unwrap().is_empty());
    fs.delete(&root).await.unwrap();
    assert!(fs.stat(&root).await.is_err());
    assert!(fs.list(Path::new("/nonexistent-dir")).await.is_err());
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn explicit_tls_passive() {
    let fs = connect(config(FtpSecurity::Explicit, FtpDataMode::Passive)).await;
    suite(&fs, "explicit").await;
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn implicit_tls_passive() {
    let fs = connect(config(FtpSecurity::Implicit, FtpDataMode::Passive)).await;
    suite(&fs, "implicit").await;
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn plain_passive() {
    let fs = connect(config(FtpSecurity::None, FtpDataMode::Passive)).await;
    suite(&fs, "plain").await;
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn plain_active() {
    let fs = connect(config(FtpSecurity::None, FtpDataMode::Active)).await;
    suite(&fs, "active").await;
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn untrusted_certificate_is_refused() {
    // Wait for the server first, so the failure below is the certificate.
    drop(connect(config(FtpSecurity::Explicit, FtpDataMode::Passive)).await);
    for security in [FtpSecurity::Explicit, FtpSecurity::Implicit] {
        let mut cfg = config(security, FtpDataMode::Passive);
        cfg.trusted_ca_pem = None;
        let e = FtpFs::connect(cfg, "ftp").await.unwrap_err().to_string();
        assert!(
            e.contains("TLS") && e.contains("certificate"),
            "{security:?}: {e}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the docker ftp services"]
async fn wrong_password_and_plain_login_to_a_tls_only_server() {
    let mut cfg = config(FtpSecurity::Explicit, FtpDataMode::Passive);
    drop(connect(cfg.clone()).await);
    cfg.password = SecretString::from("nope");
    let e = FtpFs::connect(cfg.clone(), "ftp")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("530"), "{e}");
    // The explicit server requires TLS for logins.
    cfg.security = FtpSecurity::None;
    cfg.password = SecretString::from("switchyard");
    assert!(FtpFs::connect(cfg, "ftp").await.is_err());
}
