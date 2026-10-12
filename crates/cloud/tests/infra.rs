//! Infra tools against moto (`scripts/moto-server.sh`): EC2 / RDS / ELBv2 / EKS / S3
//! inventory, CloudWatch Logs, and Terraform state versions in a versioned S3 bucket.
//!
//! moto does not check SigV4 signatures, so the test seeds data with plain requests that
//! only name the service in their `Authorization` header.
//!
//! `cargo test -p switchyard-cloud --test infra -- --ignored --test-threads 1`
//! (`SWITCHYARD_S3_ENDPOINT` points at moto when it is not on 127.0.0.1:5000).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use secrecy::SecretString;
use serde_json::{Value as Json, json};
use switchyard_cloud::aws::{AwsAuth, AwsSource};
use switchyard_cloud::inventory::aws::AwsInventory;
use switchyard_cloud::inventory::{DbFlavor, ResourceKind};
use switchyard_cloud::logs::CloudWatchLogs;
use switchyard_cloud::s3::{S3Config, S3Fs};
use switchyard_cloud::sigv4::AwsCredentials;
use switchyard_cloud::tfstate;

fn endpoint() -> url::Url {
    url::Url::parse(
        &std::env::var("SWITCHYARD_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:5000".into()),
    )
    .unwrap()
}

fn auth() -> Arc<AwsAuth> {
    Arc::new(AwsAuth::new(AwsSource::Keys(AwsCredentials {
        access_key_id: "testing".into(),
        secret_access_key: SecretString::from("testing".to_owned()),
        session_token: None,
        expires_ms: None,
    })))
}

fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// An `Authorization` header naming `service` (moto routes on it; it checks nothing else).
fn fake_auth(service: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential=testing/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
    )
}

async fn query(service: &str, params: &[(&str, &str)]) -> String {
    let body = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = http()
        .post(endpoint())
        .header("authorization", fake_auth(service))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "{service} {params:?}: {status} {text}");
    text
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn logs_call(action: &str, body: Json) -> Json {
    let resp = http()
        .post(endpoint())
        .header("authorization", fake_auth("logs"))
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amz-target", format!("Logs_20140328.{action}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "{action}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Json::Null)
}

#[tokio::test]
#[ignore = "needs moto_server (scripts/moto-server.sh)"]
async fn ec2_and_rds_show_in_the_inventory() {
    let images = query(
        "ec2",
        &[("Action", "DescribeImages"), ("Version", "2016-11-15")],
    )
    .await;
    let ami = images
        .split("<imageId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("moto lists images")
        .to_owned();
    query(
        "ec2",
        &[
            ("Action", "RunInstances"),
            ("Version", "2016-11-15"),
            ("ImageId", &ami),
            ("MinCount", "1"),
            ("MaxCount", "1"),
            ("InstanceType", "t3.micro"),
            ("TagSpecification.1.ResourceType", "instance"),
            ("TagSpecification.1.Tag.1.Key", "Name"),
            ("TagSpecification.1.Tag.1.Value", "infra-web"),
        ],
    )
    .await;
    query(
        "rds",
        &[
            ("Action", "CreateDBInstance"),
            ("Version", "2014-10-31"),
            ("DBInstanceIdentifier", "infra-orders"),
            ("Engine", "postgres"),
            ("DBInstanceClass", "db.t3.micro"),
            ("MasterUsername", "app"),
            ("MasterUserPassword", "not-a-real-password"),
            ("AllocatedStorage", "20"),
        ],
    )
    .await;

    let inv = AwsInventory::new(auth(), "moto".into(), Some(endpoint())).unwrap();
    assert_eq!(inv.account_id().await.unwrap(), "123456789012");
    let vms = inv.instances("us-east-1").await.unwrap();
    let web = vms
        .iter()
        .find(|r| r.name == "infra-web")
        .expect("the instance is listed");
    assert_eq!(web.kind, Some(ResourceKind::Vm));
    assert_eq!(web.rtype, "EC2 t3.micro");
    assert!(!web.private_ips.is_empty());
    let dbs = inv.databases("us-east-1").await.unwrap();
    let db = dbs.iter().find(|r| r.name == "infra-orders").unwrap();
    assert_eq!(db.db, Some(DbFlavor::Postgres));
    assert_eq!(db.user.as_deref(), Some("app"));
    assert!(db.host.is_some());
    // Empty but answered.
    inv.load_balancers("us-east-1").await.unwrap();
    inv.clusters("us-east-1").await.unwrap();
    inv.buckets().await.unwrap();
}

#[tokio::test]
#[ignore = "needs moto_server (scripts/moto-server.sh)"]
async fn cloudwatch_groups_and_events() {
    let group = "/switchyard/infra-test";
    let _ = logs_call("CreateLogGroup", json!({ "logGroupName": group })).await;
    logs_call(
        "CreateLogStream",
        json!({ "logGroupName": group, "logStreamName": "s1" }),
    )
    .await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    logs_call(
        "PutLogEvents",
        json!({
            "logGroupName": group,
            "logStreamName": "s1",
            "logEvents": [
                { "timestamp": now - 2000, "message": "INFO started" },
                { "timestamp": now - 1000, "message": "ERROR boom" }
            ]
        }),
    )
    .await;
    let logs = CloudWatchLogs::new(auth(), "us-east-1", Some(endpoint())).unwrap();
    let groups = logs.groups("/switchyard/").await.unwrap();
    assert!(groups.iter().any(|g| g.name == group));
    let page = logs
        .filter(group, "", now - 60_000, None, None)
        .await
        .unwrap();
    let msgs: Vec<&str> = page.events.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(msgs, ["INFO started", "ERROR boom"]);
    assert_eq!(page.events[0].stream, "s1");
}

#[tokio::test]
#[ignore = "needs moto_server (scripts/moto-server.sh)"]
async fn state_versions_in_a_versioned_bucket() {
    let bucket = "switchyard-tfstate-test";
    let base = endpoint();
    let c = http();
    let put = |path: String, body: Vec<u8>| {
        let url = base.join(&path).unwrap();
        c.put(url)
            .header("authorization", fake_auth("s3"))
            .body(body)
            .send()
    };
    put(format!("/{bucket}"), Vec::new()).await.unwrap();
    let v = put(
        format!("/{bucket}?versioning"),
        br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#.to_vec(),
    )
    .await
    .unwrap();
    assert!(v.status().is_success(), "{}", v.text().await.unwrap());
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/tfstate/");
    for f in ["serial-7.tfstate", "serial-9.tfstate"] {
        let body = std::fs::read(format!("{dir}{f}")).unwrap();
        let r = put(format!("/{bucket}/env/prod.tfstate"), body)
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    let fs = S3Fs::new(
        S3Config {
            name: "moto".into(),
            endpoint: Some(endpoint()),
            region: "us-east-1".into(),
            home: None,
            read_only: true,
            service: "S3",
        },
        auth(),
    )
    .unwrap();
    let versions = fs
        .object_versions(bucket, "env/prod.tfstate")
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert!(versions[0].latest);
    let current = tfstate::parse(
        &fs.get_object(bucket, "env/prod.tfstate", None)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(current.serial, 9);
    let older = tfstate::parse(
        &fs.get_object(bucket, "env/prod.tfstate", Some(&versions[1].version_id))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(older.serial, 7);
    let d = tfstate::diff(&older, &current);
    assert_eq!(d.added, ["aws_s3_bucket.logs"]);
    assert_eq!(d.removed, ["aws_instance.web[1]"]);
}
