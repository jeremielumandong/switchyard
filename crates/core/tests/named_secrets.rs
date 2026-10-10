//! Named secrets (`{{vault.name}}`) through core: the catalog commands, connection settings
//! that reference one, the API workbench's secret store, and (ignored, needs moto) a secret
//! linked to AWS Secrets Manager.
//! Run the moto test with `cargo test -p switchyard-core --test named_secrets -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::api::{SecretScope, SecretStoreError};
use switchyard_core::cloud::KvWrite;
use switchyard_core::db::Engine;
use switchyard_core::store::named_secrets::{NamedSecret, NamedSecretSource};
use switchyard_core::store::{
    CloudAuth, CloudConnection, CloudService, DbConnection, Profile, ProfileId, SecretRef,
};
use switchyard_core::{
    CloudEdit, Command, Core, Event, EventReceiver, RuntimeHandle, ServiceConfig,
};

async fn next<T>(rx: &mut EventReceiver, secs: u64, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match rx.next().await.expect("events") {
                Event::Error { context, message } => panic!("{context}: {message}"),
                e => {
                    if let Some(t) = f(e) {
                        return t;
                    }
                }
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

async fn list(rx: &mut EventReceiver) -> Vec<NamedSecret> {
    next(rx, 5, |e| match e {
        Event::NamedSecrets(l) => Some(l),
        _ => None,
    })
    .await
}

async fn test(
    h: &RuntimeHandle,
    rx: &mut EventReceiver,
    request: u64,
    name: &str,
) -> Result<String, String> {
    h.send(Command::TestNamedSecret {
        request,
        name: name.into(),
    });
    next(rx, 30, |e| match e {
        Event::NamedSecretTested {
            request: r, result, ..
        } if r == request => Some(result),
        _ => None,
    })
    .await
}

/// What the API workbench reads for `{{vault.name}}` (on the blocking pool, as it does).
async fn workbench_read(h: &RuntimeHandle, name: &str) -> Result<String, SecretStoreError> {
    let store = h.api_secrets();
    let reference = switchyard_core::api::vault::vault_secret_reference(name).unwrap();
    tokio::task::spawn_blocking(move || {
        store
            .get_secret(&SecretScope::default_workspace(), &reference)
            .map(|v| v.expose_secret().to_owned())
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn local_named_secret_round_trip() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    h.send(Command::LoadNamedSecrets);
    assert!(list(&mut rx).await.is_empty());

    h.send(Command::SaveNamedSecret {
        request: 1,
        secret: NamedSecret {
            name: "orders-key".into(),
            description: "Orders API".into(),
            source: NamedSecretSource::Local,
        },
        value: Some(SecretString::from("s3cr3t")),
        previous: None,
    });
    let l = list(&mut rx).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].expression(), "{{vault.orders-key}}");

    let r = test(&h, &mut rx, 2, "orders-key").await;
    assert_eq!(r.unwrap(), "Read 6 characters from the keychain");
    assert_eq!(workbench_read(&h, "orders-key").await.unwrap(), "s3cr3t");

    // Renaming keeps the value under the new name.
    h.send(Command::SaveNamedSecret {
        request: 3,
        secret: NamedSecret {
            name: "orders".into(),
            ..l[0].clone()
        },
        value: None,
        previous: Some("orders-key".into()),
    });
    let l = list(&mut rx).await;
    assert_eq!(l.len(), 1);
    assert_eq!(workbench_read(&h, "orders").await.unwrap(), "s3cr3t");
    assert_eq!(
        workbench_read(&h, "orders-key").await.unwrap_err(),
        SecretStoreError::Missing
    );

    h.send(Command::DeleteNamedSecret {
        name: "orders".into(),
    });
    assert!(list(&mut rx).await.is_empty());
    assert!(test(&h, &mut rx, 4, "orders").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_unreachable_cloud_links() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    h.send(Command::SaveNamedSecret {
        request: 1,
        secret: NamedSecret {
            name: "bad name".into(),
            ..NamedSecret::default()
        },
        value: Some(SecretString::from("x")),
        previous: None,
    });
    let (field, _) = next(&mut rx, 5, |e| match e {
        Event::NamedSecretError {
            request: 1,
            field,
            message,
        } => Some((field, message)),
        _ => None,
    })
    .await;
    assert_eq!(field, Some("name"));

    h.send(Command::SaveNamedSecret {
        request: 2,
        secret: NamedSecret {
            name: "gone".into(),
            description: String::new(),
            source: NamedSecretSource::Cloud {
                connection: ProfileId("missing".into()),
                key: "k".into(),
                field: None,
            },
        },
        value: None,
        previous: None,
    });
    list(&mut rx).await;
    let e = test(&h, &mut rx, 3, "gone").await.unwrap_err();
    assert!(e.contains("Cloud connection missing"), "{e}");
    assert_eq!(
        workbench_read(&h, "gone").await.unwrap_err(),
        SecretStoreError::BackendUnavailable
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_password_can_be_a_named_secret() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let d = DbConnection::new("orders", Engine::Postgres);
    let id = d.id.clone();
    // A password stored first is replaced by the reference.
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Db(d.clone()),
        secret: Some(SecretString::from("typed-password")),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;
    let stored = SecretRef::for_profile(&id, "password");
    let mut d = d;
    d.secret = Some(stored);
    h.send(Command::SaveProfile {
        request: 2,
        profile: Profile::Db(d),
        secret: Some(SecretString::from(" {{vault.orders-db}} ")),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 2, .. }).then_some(())
    })
    .await;
    h.send(Command::LoadProfiles);
    let saved = next(&mut rx, 5, |e| match e {
        Event::Profiles(l) => l.into_iter().find(|p| p.id() == &id),
        _ => None,
    })
    .await;
    assert_eq!(saved.secret(), Some(&SecretRef::named("orders-db")));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn secret_linked_to_secrets_manager() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut c = CloudConnection::new("moto secrets", CloudService::SecretsManager);
    c.auth = CloudAuth::AccessKey;
    c.user = "testing".into();
    c.region = "us-east-1".into();
    c.endpoint =
        std::env::var("SWITCHYARD_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:5000".into());
    let id = c.id.clone();
    // The connection's own key is itself a local named secret.
    h.send(Command::SaveNamedSecret {
        request: 1,
        secret: NamedSecret {
            name: "aws-key".into(),
            ..NamedSecret::default()
        },
        value: Some(SecretString::from("testing")),
        previous: None,
    });
    list(&mut rx).await;
    h.send(Command::SaveProfile {
        request: 2,
        profile: Profile::Cloud(c),
        secret: Some(SecretString::from("{{vault.aws-key}}")),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 2, .. }).then_some(())
    })
    .await;

    let key = format!(
        "swy-named-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
            % 1_000_000_000
    );
    h.send(Command::CloudOpen {
        session: 7,
        connection: id.clone(),
    });
    next(&mut rx, 15, |e| match e {
        Event::CloudOpened { session: 7, result } => Some(result.unwrap()),
        _ => None,
    })
    .await;
    h.send(Command::CloudEdit {
        session: 7,
        request: 3,
        edit: CloudEdit::Put(KvWrite {
            key: key.clone(),
            value: r#"{"username":"app","password":"from-the-cloud"}"#.into(),
            create: true,
            ..KvWrite::default()
        }),
    });
    let r = next(&mut rx, 15, |e| match e {
        Event::CloudEdited {
            request: 3, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.is_ok(), "{r:?}");

    h.send(Command::SaveNamedSecret {
        request: 4,
        secret: NamedSecret {
            name: "orders-db".into(),
            description: String::new(),
            source: NamedSecretSource::Cloud {
                connection: id,
                key,
                field: Some("password".into()),
            },
        },
        value: None,
        previous: None,
    });
    list(&mut rx).await;
    let r = test(&h, &mut rx, 5, "orders-db").await;
    assert_eq!(r.unwrap(), "Read 14 characters from moto secrets");
    assert_eq!(
        workbench_read(&h, "orders-db").await.unwrap(),
        "from-the-cloud"
    );
}
