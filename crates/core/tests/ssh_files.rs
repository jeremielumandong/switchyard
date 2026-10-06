//! SFTP through core: browse, upload a folder, download, edit with the conflict check.
//! Needs the SSH test servers (see `crates/remote/tests/ssh.rs`).
//! Run with `cargo test -p switchyard-core --test ssh_files -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use switchyard_core::remote::ssh::HostKeyDecision;
use switchyard_core::store::{Host, Profile, SshAuth};
use switchyard_core::{
    Command, Core, Event, EventReceiver, FsOp, FsRef, OnConflict, PromptAnswer, SaveError,
    ServiceConfig, TransferError,
};

async fn next<T>(rx: &mut EventReceiver, secs: u64, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match rx.next().await.expect("events") {
                // Trust the test server's key whenever asked.
                Event::HostKeyPrompt { .. } => {}
                ev => {
                    if let Some(t) = f(ev) {
                        return t;
                    }
                }
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn keys() -> String {
    std::env::var("SWITCHYARD_SSH_KEYS").unwrap_or_else(|_| "/tmp/switchyard-ssh".into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ssh servers"]
async fn browse_transfer_and_edit_over_sftp() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut host = Host::new("files-box", "127.0.0.1", "swy");
    host.port = 2222;
    host.auth = SshAuth::PublicKey {
        key_path: format!("{}/id_ed25519", keys()),
    };
    let host_id = host.id.clone();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Host(host),
        secret: None,
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;
    let fs = FsRef::Host(host_id.clone());

    // First SFTP use logs in (host key prompt answered here).
    h.send(Command::ListDir {
        request: 2,
        fs: fs.clone(),
        path: None,
    });
    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::HostKeyPrompt { request, .. } = rx.next().await.unwrap() {
                return request;
            }
        }
    })
    .await
    .unwrap();
    h.send(Command::AnswerPrompt {
        request,
        answer: PromptAnswer::HostKey(HostKeyDecision::TrustOnce),
    });
    let home = next(&mut rx, 10, |e| match e {
        Event::FsListing {
            request: 2,
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
    assert_eq!(home, PathBuf::from("/home/swy"));

    // Upload a local folder.
    let local = tempfile::tempdir().unwrap();
    let name = format!("swy-up-{}", std::process::id());
    let src = local.path().join(&name);
    std::fs::create_dir_all(src.join("conf")).unwrap();
    std::fs::write(src.join("conf/app.env"), "PORT=1\n").unwrap();
    std::fs::write(src.join("blob.bin"), vec![3u8; 2_500_000]).unwrap();
    h.send(Command::Transfer {
        id: 10,
        from: FsRef::Local,
        path: src.clone(),
        to: fs.clone(),
        dir: Some(home.clone()),
        on_conflict: OnConflict::Ask,
    });
    let mut progress = 0;
    let remote = next(&mut rx, 20, |e| match e {
        Event::TransferProgress { id: 10, .. } => {
            progress += 1;
            None
        }
        Event::TransferDone { id: 10, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert!(progress >= 2, "progress events");
    assert_eq!(remote, home.join(&name));

    // Again: the target exists.
    h.send(Command::Transfer {
        id: 11,
        from: FsRef::Local,
        path: src.clone(),
        to: fs.clone(),
        dir: Some(home.clone()),
        on_conflict: OnConflict::Ask,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::TransferDone { id: 11, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Err(TransferError::Exists(name.clone())));

    // Download one file back.
    let back = tempfile::tempdir().unwrap();
    h.send(Command::Transfer {
        id: 12,
        from: fs.clone(),
        path: remote.join("blob.bin"),
        to: FsRef::Local,
        dir: Some(back.path().to_owned()),
        on_conflict: OnConflict::Ask,
    });
    let got = next(&mut rx, 20, |e| match e {
        Event::TransferDone { id: 12, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(std::fs::read(got).unwrap().len(), 2_500_000);

    // Edit: read, save, then a change made elsewhere is a conflict.
    let env_path = remote.join("conf/app.env");
    h.send(Command::ReadTextFile {
        request: 20,
        fs: fs.clone(),
        path: env_path.clone(),
    });
    let opened = next(&mut rx, 10, |e| match e {
        Event::TextFileRead {
            request: 20,
            result,
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(opened.content, "PORT=1\n");
    h.send(Command::WriteTextFile {
        request: 21,
        fs: fs.clone(),
        path: env_path.clone(),
        content: "PORT=2\n".into(),
        expect_modified: opened.modified_ms,
        force: false,
    });
    let saved = next(&mut rx, 10, |e| match e {
        Event::TextFileSaved {
            request: 21,
            result,
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    // Someone else changes it a second later (SFTP times are whole seconds).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.send(Command::WriteTextFile {
        request: 23,
        fs: fs.clone(),
        path: env_path.clone(),
        content: "PORT=9\n".into(),
        expect_modified: None,
        force: true,
    });
    next(&mut rx, 10, |e| match e {
        Event::TextFileSaved {
            request: 23,
            result,
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    h.send(Command::WriteTextFile {
        request: 22,
        fs: fs.clone(),
        path: env_path.clone(),
        content: "PORT=3\n".into(),
        expect_modified: saved,
        force: false,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::TextFileSaved {
            request: 22,
            result,
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(matches!(r, Err(SaveError::Conflict(_))), "{r:?}");
    assert_ne!(saved, None);
    h.send(Command::ReadTextFile {
        request: 24,
        fs: fs.clone(),
        path: env_path.clone(),
    });
    let now = next(&mut rx, 10, |e| match e {
        Event::TextFileRead {
            request: 24,
            result,
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert_eq!(now.content, "PORT=9\n", "the conflicting save wrote nothing");

    // Clean up with a recursive delete.
    h.send(Command::FsOp {
        request: 30,
        fs: fs.clone(),
        op: FsOp::Delete(remote.clone()),
    });
    next(&mut rx, 10, |e| match e {
        Event::FsOpDone {
            request: 30,
            result,
            ..
        } => {
            result.unwrap();
            Some(())
        }
        _ => None,
    })
    .await;
    h.send(Command::ListDir {
        request: 31,
        fs: fs.clone(),
        path: Some(home.clone()),
    });
    let list = next(&mut rx, 10, |e| match e {
        Event::FsListing {
            request: 31,
            result,
            ..
        } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    assert!(list.iter().all(|e| e.name != name), "deleted");
}
