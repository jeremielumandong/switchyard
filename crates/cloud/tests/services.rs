//! Object storage and key / value tools against local stand-ins:
//!
//! - Azure Blob: Azurite (`docker compose -f docker/compose.yml up -d azurite`), which
//!   checks Shared Key signatures.
//! - S3, Secrets Manager, Parameter Store: moto (`moto_server -p 5000`, see
//!   `scripts/moto-server.sh`). moto does not check SigV4 signatures; the signer has unit
//!   tests against AWS's published vectors.
//!
//! `cargo test -p switchyard-cloud --test services -- --ignored --test-threads 1`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::SecretString;
use switchyard_cloud::aws::{AwsAuth, AwsSource};
use switchyard_cloud::azure::{AzureAuth, storage_connection_string};
use switchyard_cloud::blob::{BlobConfig, BlobFs};
use switchyard_cloud::kv::{KvQuery, KvService, KvWrite};
use switchyard_cloud::s3::{S3Config, S3Fs};
use switchyard_cloud::sigv4::AwsCredentials;
use switchyard_remote::RemoteFs;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn s3_endpoint() -> url::Url {
    url::Url::parse(
        &std::env::var("SWITCHYARD_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:5000".into()),
    )
    .unwrap()
}

fn aws_auth() -> Arc<AwsAuth> {
    Arc::new(AwsAuth::new(AwsSource::Keys(AwsCredentials {
        access_key_id: "testing".into(),
        secret_access_key: SecretString::from("testing".to_owned()),
        session_token: None,
        expires_ms: None,
    })))
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    format!("{prefix}-{}", n % 1_000_000_000)
}

/// The same suite for every object store: `root` is a new bucket / container path.
async fn fs_suite(fs: &dyn RemoteFs, root: &Path) {
    let p = |s: &str| PathBuf::from(format!("{}/{s}", root.display()));
    assert!(
        fs.stat(root).await.is_err(),
        "the bucket should not exist yet"
    );
    fs.mkdir(root).await.unwrap();
    assert!(fs.stat(root).await.unwrap().is_dir());

    fs.mkdir(&p("docs")).await.unwrap();
    fs.write_file(&p("docs/a b.txt"), b"hello world")
        .await
        .unwrap();
    fs.write_file(&p("top.json"), b"{}").await.unwrap();

    let names: Vec<_> = fs
        .list(root)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name.clone(), e.is_dir()))
        .collect();
    assert_eq!(names, [("docs".into(), true), ("top.json".into(), false)]);
    let docs = fs.list(&p("docs")).await.unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(docs[0].name, "a b.txt");
    assert_eq!(docs[0].size, 11);
    assert!(docs[0].modified_ms.is_some());

    let st = fs.stat(&p("docs/a b.txt")).await.unwrap();
    assert_eq!(st.size, 11);
    assert!(!st.is_dir());
    assert!(fs.stat(&p("docs")).await.unwrap().is_dir());
    assert!(fs.stat(&p("missing.txt")).await.is_err());
    assert_eq!(
        fs.read_file(&p("docs/a b.txt"), 100).await.unwrap(),
        b"hello world"
    );

    // Ranged read (resuming a download).
    let mut r = fs.open_read_from(&p("docs/a b.txt"), 6).await.unwrap();
    let mut rest = String::new();
    r.read_to_string(&mut rest).await.unwrap();
    assert_eq!(rest, "world");

    // A streamed upload large enough for a multipart / block upload.
    let big: Vec<u8> = (0..(17 * 1024 * 1024 + 3))
        .map(|i| (i % 253) as u8)
        .collect();
    let mut w = fs.create(&p("docs/big.bin")).await.unwrap();
    for c in big.chunks(300_000) {
        w.write_all(c).await.unwrap();
    }
    w.shutdown().await.unwrap();
    assert_eq!(
        fs.stat(&p("docs/big.bin")).await.unwrap().size,
        big.len() as u64
    );
    let mut back = Vec::new();
    fs.open_read(&p("docs/big.bin"))
        .await
        .unwrap()
        .read_to_end(&mut back)
        .await
        .unwrap();
    assert!(back == big, "the downloaded object differs");

    // A cancelled upload (writer dropped without shutdown) leaves nothing.
    let mut w = fs.create(&p("docs/cancelled.bin")).await.unwrap();
    w.write_all(b"partial").await.unwrap();
    drop(w);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(fs.stat(&p("docs/cancelled.bin")).await.is_err());

    fs.rename(&p("top.json"), &p("docs/moved.json"))
        .await
        .unwrap();
    assert!(fs.stat(&p("top.json")).await.is_err());
    assert_eq!(
        fs.read_file(&p("docs/moved.json"), 10).await.unwrap(),
        b"{}"
    );
    assert!(
        fs.rename(&p("docs"), &p("docs2")).await.is_err(),
        "folders do not rename"
    );

    for f in ["docs/a b.txt", "docs/big.bin", "docs/moved.json", "docs"] {
        fs.delete(&p(f)).await.unwrap();
    }
    assert!(fs.list(root).await.unwrap().is_empty());
    fs.delete(root).await.unwrap();
}

#[tokio::test]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn s3_file_system() {
    let fs = S3Fs::new(
        S3Config {
            name: "moto".into(),
            endpoint: Some(s3_endpoint()),
            region: "us-east-1".into(),
            home: None,
            read_only: false,
            service: "S3",
        },
        aws_auth(),
    )
    .unwrap();
    let bucket = unique("swy");
    fs_suite(&fs, Path::new(&format!("/{bucket}"))).await;
    let buckets = fs.list(Path::new("/")).await.unwrap();
    assert!(!buckets.iter().any(|b| b.name == bucket));
}

#[tokio::test]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn s3_read_only_refuses_writes() {
    let fs = S3Fs::new(
        S3Config {
            name: "moto".into(),
            endpoint: Some(s3_endpoint()),
            region: "us-east-1".into(),
            home: None,
            read_only: true,
            service: "S3",
        },
        aws_auth(),
    )
    .unwrap();
    let err = fs
        .mkdir(Path::new(&format!("/{}", unique("ro"))))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    fs.list(Path::new("/")).await.unwrap();
}

fn azurite() -> String {
    std::env::var("SWITCHYARD_AZURITE")
        .unwrap_or_else(|_| "http://127.0.0.1:10000/devstoreaccount1".into())
}

#[tokio::test]
#[ignore = "needs Azurite on 127.0.0.1:10000"]
async fn blob_file_system_shared_key() {
    let cs = SecretString::from(format!(
        "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;\
         AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;\
         BlobEndpoint={};",
        azurite()
    ));
    let (endpoint, _, auth) = storage_connection_string(&cs).unwrap();
    let fs = BlobFs::new(
        BlobConfig {
            name: "azurite".into(),
            endpoint,
            home: None,
            read_only: false,
        },
        auth,
    )
    .unwrap();
    fs_suite(&fs, Path::new(&format!("/{}", unique("swy")))).await;
}

#[tokio::test]
#[ignore = "needs Azurite on 127.0.0.1:10000"]
async fn blob_listing_pages_and_filters_by_prefix() {
    let cs = SecretString::from(format!(
        "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;\
         AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;\
         BlobEndpoint={};",
        azurite()
    ));
    let (endpoint, _, auth) = storage_connection_string(&cs).unwrap();
    let fs = BlobFs::new(
        BlobConfig {
            name: "azurite".into(),
            endpoint,
            home: None,
            read_only: false,
        },
        auth,
    )
    .unwrap();
    let container = unique("page");
    let root = format!("/{container}");
    fs.mkdir(Path::new(&root)).await.unwrap();
    fs.mkdir(Path::new(&format!("{root}/dir/sub")))
        .await
        .unwrap();
    for name in ["a1.txt", "a2.txt", "a3.txt", "b1.txt", "b2.txt"] {
        let path = format!("{root}/dir/{name}");
        let mut w = fs.create(Path::new(&path)).await.unwrap();
        w.write_all(b"x").await.unwrap();
        w.shutdown().await.unwrap();
    }
    let dir = format!("{root}/dir");
    let dir = Path::new(&dir);

    // Two at a time until the cursor runs out: every entry once.
    let mut names = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = fs.list_page(dir, "", cursor.as_deref(), 2).await.unwrap();
        assert!(page.entries.len() <= 2);
        names.extend(page.entries.into_iter().map(|e| e.name));
        pages += 1;
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    names.sort();
    assert_eq!(
        names,
        ["a1.txt", "a2.txt", "a3.txt", "b1.txt", "b2.txt", "sub"]
    );
    assert!(pages >= 3, "{pages} pages");

    let page = fs.list_page(dir, "b", None, 100).await.unwrap();
    let found: Vec<_> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(found, ["b1.txt", "b2.txt"]);
    assert_eq!(page.next, None);

    // Containers page and filter the same way.
    let page = fs
        .list_page(Path::new("/"), &container, None, 5)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, container);

    for name in ["a1.txt", "a2.txt", "a3.txt", "b1.txt", "b2.txt", "sub"] {
        fs.delete(&dir.join(name)).await.unwrap();
    }
    fs.delete(Path::new(&root)).await.unwrap();
}

#[tokio::test]
#[ignore = "needs Azurite on 127.0.0.1:10000"]
async fn blob_wrong_key_is_refused() {
    let fs = BlobFs::new(
        BlobConfig {
            name: "azurite".into(),
            endpoint: url::Url::parse(&azurite()).unwrap(),
            home: None,
            read_only: false,
        },
        AzureAuth::SharedKey {
            account: "devstoreaccount1".into(),
            key: SecretString::from("d3Jvbmc=".to_owned()),
        },
    )
    .unwrap();
    let err = fs.list(Path::new("/")).await.unwrap_err().to_string();
    assert!(err.contains("refused"), "{err}");
}

#[tokio::test]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn secrets_manager() {
    let sm = switchyard_cloud::secrets_manager::SecretsManager::new(
        aws_auth(),
        "us-east-1",
        Some(s3_endpoint()),
        false,
    )
    .unwrap();
    let name = unique("swy/db");
    let mut w = KvWrite {
        key: name.clone(),
        value: "s3cret".into(),
        description: Some("test".into()),
        create: true,
        ..KvWrite::default()
    };
    sm.put(&w).await.unwrap();
    let page = sm
        .list(&KvQuery {
            key: name.clone(),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1, "{page:?}");
    assert_eq!(page.items[0].value, None, "lists never carry secret values");
    assert_eq!(
        sm.get(None, &name, None).await.unwrap().value.as_deref(),
        Some("s3cret")
    );
    w.create = false;
    w.value = "rotated".into();
    sm.put(&w).await.unwrap();
    assert_eq!(
        sm.get(None, &name, None).await.unwrap().value.as_deref(),
        Some("rotated")
    );
    sm.delete(None, &name, None).await.unwrap();
}

#[tokio::test]
#[ignore = "needs moto_server on 127.0.0.1:5000"]
async fn parameter_store() {
    let ps = switchyard_cloud::parameters::ParameterStore::new(
        aws_auth(),
        "us-east-1",
        Some(s3_endpoint()),
        false,
    )
    .unwrap();
    let prefix = format!("/{}/", unique("swy"));
    let key = format!("{prefix}db-url");
    let mut w = KvWrite {
        key: key.clone(),
        value: "postgres://x".into(),
        kind: Some("SecureString".into()),
        create: true,
        ..KvWrite::default()
    };
    ps.put(&w).await.unwrap();
    assert!(ps.put(&w).await.is_err(), "create refuses an existing name");
    let page = ps
        .list(&KvQuery {
            key: prefix.clone(),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].kind.as_deref(), Some("SecureString"));
    w.create = false;
    w.kind = None;
    w.value = "postgres://y".into();
    ps.put(&w).await.unwrap();
    let item = ps.get(None, &key, None).await.unwrap();
    assert_eq!(item.value.as_deref(), Some("postgres://y"));
    ps.delete(None, &key, None).await.unwrap();
    assert!(ps.get(None, &key, None).await.unwrap_err().is_not_found());
}

fn app_config_connection_string() -> SecretString {
    SecretString::from(std::env::var("SWITCHYARD_APPCONFIG").unwrap_or_else(|_| {
        "Endpoint=http://127.0.0.1:8483;Id=emulator;Secret=c2VjcmV0c2VjcmV0c2VjcmV0".into()
    }))
}

#[tokio::test]
#[ignore = "needs the App Configuration emulator on 127.0.0.1:8483"]
async fn app_configuration() {
    use switchyard_cloud::appconfig::{self, AppConfig};
    let (endpoint, auth) =
        appconfig::from_connection_string(&app_config_connection_string()).unwrap();
    let ac = AppConfig::new(endpoint, auth).unwrap();
    let app = unique("swy");
    let key = format!("{app}:db:host");
    let mut w = KvWrite {
        key: key.clone(),
        label: Some("prod".into()),
        value: "db.internal".into(),
        content_type: Some("text/plain".into()),
        tags: [("team".to_owned(), "web".to_owned())].into(),
        create: true,
        ..KvWrite::default()
    };
    ac.put(&w).await.unwrap();
    assert!(
        ac.put(&w).await.is_err(),
        "create refuses an existing key and label"
    );
    // The same key without a label is a different setting.
    ac.put(&KvWrite {
        label: None,
        value: "localhost".into(),
        ..w.clone()
    })
    .await
    .unwrap();

    let all = ac
        .list(&KvQuery {
            key: format!("{app}:"),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(all.items.len(), 2, "{all:?}");
    let prod = ac
        .list(&KvQuery {
            key: format!("{app}:"),
            label: "prod".into(),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(prod.items.len(), 1);
    assert_eq!(prod.items[0].value.as_deref(), Some("db.internal"));
    assert_eq!(prod.items[0].tags["team"], "web");
    let unlabeled = ac
        .list(&KvQuery {
            key: format!("{app}:"),
            label: "\\0".into(),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(unlabeled.items.len(), 1);
    assert_eq!(unlabeled.items[0].label, None);

    // Optimistic concurrency: an update with a stale etag is refused.
    let item = ac.get(None, &key, Some("prod")).await.unwrap();
    w.create = false;
    w.etag = item.etag.clone();
    w.value = "db2.internal".into();
    ac.put(&w).await.unwrap();
    w.value = "db3.internal".into();
    assert!(ac.put(&w).await.is_err(), "stale etag");

    // Locked settings refuse changes until unlocked.
    ac.set_locked(&key, Some("prod"), true).await.unwrap();
    assert_eq!(
        ac.get(None, &key, Some("prod")).await.unwrap().locked,
        Some(true)
    );
    w.etag = None;
    assert!(ac.put(&w).await.is_err(), "locked");
    ac.set_locked(&key, Some("prod"), false).await.unwrap();
    ac.put(&w).await.unwrap();

    // Feature flags.
    let flag_id = format!("{app}-beta");
    let (fkey, fvalue) = appconfig::new_flag(&flag_id, false, "New checkout");
    ac.put(&KvWrite {
        key: fkey.clone(),
        value: fvalue,
        content_type: Some(appconfig::FEATURE_FLAG_CONTENT_TYPE.into()),
        create: true,
        ..KvWrite::default()
    })
    .await
    .unwrap();
    let flag = ac.get(None, &fkey, None).await.unwrap();
    let on = appconfig::update_flag(flag.value.as_deref().unwrap(), true, None).unwrap();
    ac.put(&KvWrite {
        key: fkey.clone(),
        value: on,
        content_type: flag.content_type.clone(),
        etag: flag.etag.clone(),
        ..KvWrite::default()
    })
    .await
    .unwrap();
    let flags = ac
        .list(&KvQuery {
            key: format!("{}{flag_id}", appconfig::FEATURE_FLAG_PREFIX),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    let parsed = appconfig::parse_flag(&fkey, flags.items[0].value.as_deref().unwrap()).unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.description, "New checkout");

    // Revisions: newest first, one per change.
    let revs = ac.revisions(&key, Some("prod")).await.unwrap();
    assert!(revs.len() >= 3, "{revs:?}");
    assert_eq!(revs[0].value.as_deref(), Some("db3.internal"));
    assert!(
        revs.iter()
            .any(|r| r.value.as_deref() == Some("db.internal"))
    );
    let unlabeled_revs = ac.revisions(&key, None).await.unwrap();
    assert!(unlabeled_revs.iter().all(|r| r.label.is_none()));
    assert!(ac.labels().await.unwrap().contains(&Some("prod".into())));

    // Import from a file's text, then export it back.
    use switchyard_cloud::kv_file::{self, KvFormat};
    let text = format!("{{\"{app}\": {{\"import\": {{\"a\": \"1\", \"b\": 2}}}}}}");
    let writes = kv_file::import(&text, KvFormat::Json, Some("imported")).unwrap();
    for w in &writes {
        ac.put(w).await.unwrap();
    }
    let imported = ac
        .list(&KvQuery {
            key: format!("{app}:import:"),
            label: "imported".into(),
            ..KvQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(imported.items.len(), 2);
    let exported = kv_file::export(&imported.items, KvFormat::Env);
    assert!(
        exported.contains(&format!("{app}__import__b=2")),
        "{exported}"
    );
    for i in &imported.items {
        ac.delete(None, &i.key, i.label.as_deref()).await.unwrap();
    }

    ac.delete(None, &key, Some("prod")).await.unwrap();
    ac.delete(None, &key, None).await.unwrap();
    ac.delete(None, &fkey, None).await.unwrap();
    assert!(ac.get(None, &key, None).await.unwrap_err().is_not_found());
}
