//! FTP through core: test a file connection, browse it, upload, resume a partial upload,
//! download. Needs the docker `ftp-plain` service (see `docker/ftp/README.md`).
//! Run with `cargo test -p switchyard-core --test ftp_files -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::remote::{FtpConfig, FtpDataMode, FtpFs, FtpSecurity, RemoteFs};
use switchyard_core::store::{
    EnvironmentLabel, FileConnection, FileProtocol, FtpMode, FtpTls, Profile, ProfileId,
};
use switchyard_core::{
    Command, Core, Event, EventReceiver, FsOp, FsRef, OnConflict, ServiceConfig, TransferError,
};

async fn next<T>(rx: &mut EventReceiver, secs: u64, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if let Some(t) = f(rx.next().await.expect("events")) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn connection(mode: FtpMode) -> FileConnection {
    FileConnection {
        id: ProfileId::new(),
        name: "ftp-box".into(),
        protocol: FileProtocol::Ftp {
            server: "127.0.0.1".into(),
            port: 2120,
            tls: FtpTls::None,
            mode,
            user: "deploy".into(),
        },
        default_path: None,
        environment: EnvironmentLabel::Local,
        folder: None,
        secret: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the docker ftp-plain service"]
async fn browse_transfer_and_resume_over_ftp() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();

    // Test from the editor: the typed password, nothing saved yet.
    h.send(Command::TestFiles {
        request: 1,
        connection: connection(FtpMode::Active),
        secret: Some(SecretString::from("switchyard")),
    });
    let r = next(&mut rx, 15, |e| match e {
        Event::TestResult { request: 1, result } => Some(result),
        _ => None,
    })
    .await;
    assert!(
        r.as_ref().is_ok_and(|s| s.starts_with("Connected · FTP")),
        "{r:?}"
    );

    let conn = connection(FtpMode::Passive);
    let id = conn.id.clone();
    h.send(Command::SaveProfile {
        request: 2,
        profile: Profile::File(conn),
        secret: Some(SecretString::from("switchyard")),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 2, .. }).then_some(())
    })
    .await;
    let fs = FsRef::Conn(id);

    h.send(Command::ListDir {
        request: 3,
        fs: fs.clone(),
        path: None,
    });
    let home = next(&mut rx, 15, |e| match e {
        Event::FsListing {
            request: 3,
            path,
            result,
            ..
        } => {
            result.unwrap();
            Some(path)
        }
        _ => None,
    })
    .await;
    assert_eq!(home, PathBuf::from("/home/deploy"));

    // Upload a local folder.
    let local = tempfile::tempdir().unwrap();
    let name = format!("swy-core-ftp-{}", std::process::id());
    let src = local.path().join(&name);
    std::fs::create_dir_all(src.join("conf")).unwrap();
    std::fs::write(src.join("conf/app.env"), "PORT=1\n").unwrap();
    let blob: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(src.join("blob.bin"), &blob).unwrap();
    h.send(Command::Transfer {
        id: 10,
        from: FsRef::Local,
        path: src.clone(),
        to: fs.clone(),
        dir: Some(home.clone()),
        on_conflict: OnConflict::Ask,
        resume: false,
    });
    let remote = next(&mut rx, 30, |e| match e {
        Event::TransferDone { id: 10, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(remote, home.join(&name));

    // An interrupted upload left a partial file: it is reported, then resumed (APPE).
    let mut cfg = FtpConfig::new(
        "127.0.0.1",
        2120,
        "deploy",
        SecretString::from("switchyard"),
    );
    cfg.security = FtpSecurity::None;
    cfg.mode = FtpDataMode::Passive;
    let direct = FtpFs::connect(cfg, "direct").await.unwrap();
    let single = local.path().join("single.bin");
    std::fs::write(&single, &blob).unwrap();
    let part = home.join("single.bin.swypart");
    direct.write_file(&part, &blob[..1_000_000]).await.unwrap();
    h.send(Command::Transfer {
        id: 11,
        from: FsRef::Local,
        path: single.clone(),
        to: fs.clone(),
        dir: Some(home.clone()),
        on_conflict: OnConflict::Ask,
        resume: false,
    });
    let r = next(&mut rx, 15, |e| match e {
        Event::TransferDone { id: 11, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Err(TransferError::Partial(1_000_000)));
    h.send(Command::Transfer {
        id: 12,
        from: FsRef::Local,
        path: single.clone(),
        to: fs.clone(),
        dir: Some(home.clone()),
        on_conflict: OnConflict::Ask,
        resume: true,
    });
    let mut first = None;
    next(&mut rx, 30, |e| match e {
        Event::TransferProgress { id: 12, done, .. } if done > 0 => {
            first.get_or_insert(done);
            None
        }
        Event::TransferDone { id: 12, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    let first = first.unwrap_or_default();
    assert!(
        first >= 1_000_000,
        "resumed from the partial file ({first})"
    );
    let up = home.join("single.bin");
    assert!(direct.read_file(&up, 10_000_000).await.unwrap() == blob);

    // Download the folder back.
    let back = tempfile::tempdir().unwrap();
    h.send(Command::Transfer {
        id: 13,
        from: fs.clone(),
        path: remote.clone(),
        to: FsRef::Local,
        dir: Some(back.path().to_owned()),
        on_conflict: OnConflict::Ask,
        resume: false,
    });
    next(&mut rx, 30, |e| match e {
        Event::TransferDone { id: 13, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert!(std::fs::read(back.path().join(&name).join("blob.bin")).unwrap() == blob);
    assert_eq!(
        std::fs::read_to_string(back.path().join(&name).join("conf/app.env")).unwrap(),
        "PORT=1\n"
    );

    // Clean up through core (recursive delete).
    for (request, path) in [(20, remote), (21, up)] {
        h.send(Command::FsOp {
            request,
            fs: fs.clone(),
            op: FsOp::Delete(path),
        });
        next(&mut rx, 15, |e| match e {
            Event::FsOpDone {
                request: r, result, ..
            } if r == request => Some(result),
            _ => None,
        })
        .await
        .unwrap();
    }
}
