//! Cloud connections through core: Test connection, browse and transfer to S3 (moto), and
//! the key / value tool on App Configuration (its emulator). See
//! `crates/cloud/tests/services.rs` for the services.
//! Run with `cargo test -p switchyard-core --test cloud -- --ignored --test-threads 1`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::cloud::{KvQuery, KvWrite};
use switchyard_core::store::{CloudAuth, CloudConnection, CloudService, Profile};
use switchyard_core::{
    CloudEdit, Command, Core, Event, EventReceiver, FsOp, FsRef, OnConflict, ServiceConfig,
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

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    format!("{prefix}-{}", n % 1_000_000_000)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn s3_browse_and_transfer() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut c = CloudConnection::new("moto", CloudService::S3);
    c.auth = CloudAuth::AccessKey;
    c.user = "testing".into();
    c.endpoint =
        std::env::var("SWITCHYARD_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:5000".into());
    let bucket = unique("swy-core");

    // Test from the editor with the typed secret.
    h.send(Command::TestCloud {
        request: 1,
        connection: c.clone(),
        secret: Some(SecretString::from("testing")),
    });
    let r = next(&mut rx, 20, |e| match e {
        Event::TestResult { request: 1, result } => Some(result),
        _ => None,
    })
    .await;
    assert!(
        r.as_ref().is_ok_and(|s| s.starts_with("Connected")),
        "{r:?}"
    );

    let id = c.id.clone();
    h.send(Command::SaveProfile {
        request: 2,
        profile: Profile::Cloud(c),
        secret: Some(SecretString::from("testing")),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 2, .. }).then_some(())
    })
    .await;
    let fs = FsRef::Conn(id);

    h.send(Command::FsOp {
        request: 3,
        fs: fs.clone(),
        op: FsOp::Mkdir(PathBuf::from(format!("/{bucket}"))),
    });
    next(&mut rx, 15, |e| match e {
        Event::FsOpDone {
            request: 3, result, ..
        } => Some(result),
        _ => None,
    })
    .await
    .unwrap();

    // Upload a local folder: object storage writes each file directly (no .swypart).
    let local = tempfile::tempdir().unwrap();
    let src = local.path().join("site");
    std::fs::create_dir_all(src.join("css")).unwrap();
    std::fs::write(src.join("index.html"), "<h1>hi</h1>").unwrap();
    let blob: Vec<u8> = (0..9_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("css/big.bin"), &blob).unwrap();
    let dir = PathBuf::from(format!("/{bucket}"));
    h.send(Command::Transfer {
        id: 10,
        from: FsRef::Local,
        path: src.clone(),
        to: fs.clone(),
        dir: Some(dir.clone()),
        on_conflict: OnConflict::Ask,
        resume: false,
    });
    let remote = next(&mut rx, 60, |e| match e {
        Event::TransferDone { id: 10, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(remote, PathBuf::from(format!("/{bucket}/site")));

    h.send(Command::ListDir {
        request: 4,
        fs: fs.clone(),
        path: Some(remote.join("css")),
    });
    let listing = next(&mut rx, 15, |e| match e {
        Event::FsListing {
            request: 4, result, ..
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    let names: Vec<_> = listing.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["big.bin"], "no leftover part files");
    assert_eq!(listing[0].size, blob.len() as u64);

    // Download it back.
    let down = tempfile::tempdir().unwrap();
    h.send(Command::Transfer {
        id: 11,
        from: fs.clone(),
        path: remote.join("css/big.bin"),
        to: FsRef::Local,
        dir: Some(down.path().to_owned()),
        on_conflict: OnConflict::Ask,
        resume: false,
    });
    let local_copy = next(&mut rx, 60, |e| match e {
        Event::TransferDone { id: 11, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert!(std::fs::read(local_copy).unwrap() == blob);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the App Configuration emulator on 127.0.0.1:8483"]
async fn app_configuration_tool() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut c = CloudConnection::new("settings", CloudService::AppConfig);
    c.auth = CloudAuth::ConnectionString;
    let id = c.id.clone();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Cloud(c),
        secret: Some(SecretString::from(
            std::env::var("SWITCHYARD_APPCONFIG").unwrap_or_else(|_| {
                "Endpoint=http://127.0.0.1:8483;Id=emulator;Secret=c2VjcmV0c2VjcmV0c2VjcmV0".into()
            }),
        )),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;
    h.send(Command::CloudOpen {
        session: 7,
        connection: id,
    });
    let info = next(&mut rx, 15, |e| match e {
        Event::CloudOpened { session: 7, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert!(info.caps.labels && info.caps.feature_flags);

    let key = format!("{}:greeting", unique("core"));
    h.send(Command::CloudEdit {
        session: 7,
        request: 2,
        edit: CloudEdit::Put(KvWrite {
            key: key.clone(),
            label: Some("dev".into()),
            value: "hello".into(),
            create: true,
            ..KvWrite::default()
        }),
    });
    let r = next(&mut rx, 15, |e| match e {
        Event::CloudEdited {
            request: 2, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r.unwrap(), format!("Created {key}"));

    h.send(Command::CloudList {
        session: 7,
        request: 3,
        query: KvQuery {
            key: key.clone(),
            ..KvQuery::default()
        },
    });
    let page = next(&mut rx, 15, |e| match e {
        Event::CloudItems {
            request: 3, result, ..
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].label.as_deref(), Some("dev"));

    h.send(Command::CloudEdit {
        session: 7,
        request: 4,
        edit: CloudEdit::Delete {
            scope: None,
            key: key.clone(),
            label: Some("dev".into()),
        },
    });
    let r = next(&mut rx, 15, |e| match e {
        Event::CloudEdited {
            request: 4, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.is_ok(), "{r:?}");
    h.send(Command::CloseSession { session: 7 });
}
