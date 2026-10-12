//! Google Cloud inventory with the Google Cloud CLI's sign-in (`gcloud auth
//! print-access-token`): Compute Engine instances, Cloud SQL instances, GKE clusters and
//! Cloud Storage buckets of one project. Service-account keys are not read here.

use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use reqwest::Method;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value as Json;

use super::{CloudResource, DbFlavor, Provider, ResourceKind};
use crate::auth::{TokenSource, run_cli};
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client};

/// gcloud tokens live an hour; ask again after this.
const TOKEN_TTL: Duration = Duration::from_secs(45 * 60);
const MAX_PAGES: usize = 20;

/// Access tokens from `gcloud auth print-access-token`.
#[derive(Default)]
pub struct GcloudToken {
    cache: tokio::sync::Mutex<Option<(SecretString, Instant)>>,
}

impl TokenSource for GcloudToken {
    fn token(&self) -> BoxFuture<'_, Result<SecretString>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some((t, at)) = cache.as_ref()
                && at.elapsed() < TOKEN_TTL
            {
                return Ok(t.clone());
            }
            let out = run_cli("gcloud", &["auth", "print-access-token"])
                .await
                .map_err(|e| {
                    CloudError::Auth(format!(
                        "Google Cloud CLI: {e}. Sign in with `gcloud auth login`"
                    ))
                })?;
            let t = String::from_utf8_lossy(&out).trim().to_owned();
            if t.is_empty() {
                return Err(CloudError::Auth("gcloud printed no access token".into()));
            }
            let t = SecretString::from(t);
            *cache = Some((t.clone(), Instant::now()));
            Ok(t)
        })
    }
}

/// gcloud's default project (`gcloud config get-value project`).
pub async fn default_project() -> Result<String> {
    let out = run_cli("gcloud", &["config", "get-value", "project"]).await?;
    let p = String::from_utf8_lossy(&out).trim().to_owned();
    if p.is_empty() || p == "(unset)" {
        return Err(CloudError::Invalid(
            "no Google Cloud project: enter one, or run `gcloud config set project <id>`".into(),
        ));
    }
    Ok(p)
}

fn last(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn labels(v: Option<&Json>) -> Vec<(String, String)> {
    v.and_then(Json::as_object)
        .map(|t| {
            t.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Instances in an `aggregated/instances` reply.
pub fn parse_instances(json: &Json, project: &str) -> Vec<CloudResource> {
    let mut out = Vec::new();
    for scope in json
        .get("items")
        .and_then(Json::as_object)
        .into_iter()
        .flat_map(|m| m.values())
    {
        for i in scope
            .get("instances")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = i.get("name").and_then(Json::as_str) else {
                continue;
            };
            let s = |k: &str| i.get(k).and_then(Json::as_str).unwrap_or_default();
            let mut res = CloudResource {
                provider: Some(Provider::Gcp),
                account: project.to_owned(),
                region: last(s("zone")).to_owned(),
                kind: Some(ResourceKind::Vm),
                rtype: format!("GCE {}", last(s("machineType"))).trim().to_owned(),
                id: i
                    .get("id")
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                    .unwrap_or_default(),
                name: name.to_owned(),
                state: s("status").to_owned(),
                port: Some(22),
                tags: labels(i.get("labels")),
                ..CloudResource::default()
            };
            for n in i
                .get("networkInterfaces")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(ip) = n.get("networkIP").and_then(Json::as_str) {
                    res.private_ips.push(ip.to_owned());
                }
                for a in n
                    .get("accessConfigs")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(ip) = a.get("natIP").and_then(Json::as_str) {
                        res.public_ips.push(ip.to_owned());
                    }
                }
            }
            out.push(res);
        }
    }
    out
}

/// Cloud SQL instances in a `sqladmin … /instances` reply.
pub fn parse_sql(json: &Json, project: &str) -> Vec<CloudResource> {
    json.get("items")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?;
            let version = d
                .get("databaseVersion")
                .and_then(Json::as_str)
                .unwrap_or_default();
            let (db, port) = if version.starts_with("POSTGRES") {
                (Some(DbFlavor::Postgres), 5432)
            } else if version.starts_with("MYSQL") {
                (Some(DbFlavor::MySql), 3306)
            } else if version.starts_with("SQLSERVER") {
                (Some(DbFlavor::SqlServer), 1433)
            } else {
                (None, 0)
            };
            let mut res = CloudResource {
                provider: Some(Provider::Gcp),
                account: project.to_owned(),
                region: d
                    .get("region")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                kind: Some(ResourceKind::Database),
                rtype: format!("Cloud SQL {version}"),
                id: d
                    .get("connectionName")
                    .and_then(Json::as_str)
                    .unwrap_or(name)
                    .to_owned(),
                name: name.to_owned(),
                state: d
                    .get("state")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                db,
                port: (port > 0).then_some(port),
                tags: labels(d.pointer("/settings/userLabels")),
                ..CloudResource::default()
            };
            for a in d
                .get("ipAddresses")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                let Some(ip) = a.get("ipAddress").and_then(Json::as_str) else {
                    continue;
                };
                if a.get("type").and_then(Json::as_str) == Some("PRIVATE") {
                    res.private_ips.push(ip.to_owned());
                } else {
                    res.public_ips.push(ip.to_owned());
                }
            }
            res.host = res.public_ips.first().or(res.private_ips.first()).cloned();
            Some(res)
        })
        .collect()
}

/// GKE clusters in a `locations/-/clusters` reply.
pub fn parse_gke(json: &Json, project: &str) -> Vec<CloudResource> {
    json.get("clusters")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?;
            let s = |k: &str| c.get(k).and_then(Json::as_str).unwrap_or_default();
            Some(CloudResource {
                provider: Some(Provider::Gcp),
                account: project.to_owned(),
                region: s("location").to_owned(),
                kind: Some(ResourceKind::Kubernetes),
                rtype: format!("GKE {}", s("currentMasterVersion"))
                    .trim()
                    .to_owned(),
                id: if s("selfLink").is_empty() {
                    name.to_owned()
                } else {
                    s("selfLink").to_owned()
                },
                name: name.to_owned(),
                state: s("status").to_owned(),
                public_ips: Some(s("endpoint"))
                    .filter(|e| !e.is_empty())
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
                port: Some(443),
                tags: labels(c.get("resourceLabels")),
                ..CloudResource::default()
            })
        })
        .collect()
}

/// Buckets in a `storage/v1/b` reply.
pub fn parse_gcs(json: &Json, project: &str) -> Vec<CloudResource> {
    json.get("items")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let name = b.get("name")?.as_str()?;
            Some(CloudResource {
                provider: Some(Provider::Gcp),
                account: project.to_owned(),
                region: b
                    .get("location")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                kind: Some(ResourceKind::Storage),
                rtype: "Cloud Storage bucket".into(),
                id: format!("gs://{name}"),
                name: name.to_owned(),
                state: "available".into(),
                tags: labels(b.get("labels")),
                ..CloudResource::default()
            })
        })
        .collect()
}

/// Google Cloud REST calls for one project.
pub struct GcpInventory {
    token: Box<dyn TokenSource>,
    http: reqwest::Client,
    project: String,
}

/// Turns one API page into resources (page, project).
type Parser = fn(&Json, &str) -> Vec<CloudResource>;

impl GcpInventory {
    /// A client for `project`.
    pub fn new(token: Box<dyn TokenSource>, project: String) -> Result<Self> {
        Ok(Self {
            token,
            http: client()?,
            project,
        })
    }

    async fn get(&self, url: &str, page: Option<&str>) -> Result<Json> {
        let mut base = url::Url::parse(url).map_err(|e| CloudError::Invalid(e.to_string()))?;
        let pairs: Vec<(String, String)> = base.query_pairs().into_owned().collect();
        base.set_query(None);
        let mut req = Req::new(Method::GET, &base, "");
        for (k, v) in pairs {
            req = req.query(&k, v);
        }
        if let Some(p) = page {
            req = req.query("pageToken", p);
        }
        let t = self.token.token().await?;
        req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
        let resp = Resp::read(req.send(&self.http).await?).await?;
        let json = resp.json().unwrap_or(Json::Null);
        if !resp.ok() {
            let msg = json
                .pointer("/error/message")
                .and_then(Json::as_str)
                .unwrap_or_default();
            return Err(CloudError::api(resp.status, None, msg));
        }
        Ok(json)
    }

    async fn paged(
        &self,
        url: &str,
        parse: fn(&Json, &str) -> Vec<CloudResource>,
    ) -> Result<Vec<CloudResource>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let json = self.get(url, page.as_deref()).await?;
            out.extend(parse(&json, &self.project));
            page = json
                .get("nextPageToken")
                .and_then(Json::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if page.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Every inventoried resource of the project, with the services that failed.
    pub async fn resources(&self) -> (Vec<CloudResource>, Vec<String>) {
        let p = crate::http::uri_encode(&self.project, false);
        let calls: [(&str, String, Parser); 4] = [
            (
                "Compute Engine",
                format!(
                    "https://compute.googleapis.com/compute/v1/projects/{p}/aggregated/instances"
                ),
                parse_instances,
            ),
            (
                "Cloud SQL",
                format!("https://sqladmin.googleapis.com/v1/projects/{p}/instances"),
                parse_sql,
            ),
            (
                "GKE",
                format!("https://container.googleapis.com/v1/projects/{p}/locations/-/clusters"),
                parse_gke,
            ),
            (
                "Cloud Storage",
                format!("https://storage.googleapis.com/storage/v1/b?project={p}"),
                parse_gcs,
            ),
        ];
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for (what, url, parse) in calls {
            match self.paged(&url, parse).await {
                Ok(items) => out.extend(items),
                Err(e) => errors.push(format!("{}: {what}: {e}", self.project)),
            }
        }
        (out, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_sql_gke_and_buckets() {
        let json: Json = serde_json::from_str(
            r#"{"items":{"zones/europe-west1-b":{"instances":[{"id":"123","name":"vm-1","status":"RUNNING",
              "zone":"https://www.googleapis.com/compute/v1/projects/p/zones/europe-west1-b",
              "machineType":"https://www.googleapis.com/compute/v1/projects/p/zones/europe-west1-b/machineTypes/e2-small",
              "networkInterfaces":[{"networkIP":"10.132.0.2","accessConfigs":[{"natIP":"34.1.2.3"}]}],
              "labels":{"env":"dev"}}]},"zones/us-east1-b":{"warning":{"code":"NO_RESULTS_ON_PAGE"}}}}"#,
        )
        .unwrap();
        let vms = parse_instances(&json, "p");
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].rtype, "GCE e2-small");
        assert_eq!(vms[0].region, "europe-west1-b");
        assert_eq!(vms[0].public_ips, ["34.1.2.3"]);
        assert_eq!(vms[0].private_ips, ["10.132.0.2"]);
        let sql: Json = serde_json::from_str(
            r#"{"items":[{"name":"orders","databaseVersion":"POSTGRES_15","state":"RUNNABLE","region":"europe-west1","connectionName":"p:europe-west1:orders","ipAddresses":[{"type":"PRIMARY","ipAddress":"35.1.1.1"},{"type":"PRIVATE","ipAddress":"10.0.0.3"}]}]}"#,
        )
        .unwrap();
        let dbs = parse_sql(&sql, "p");
        assert_eq!(dbs[0].db, Some(DbFlavor::Postgres));
        assert_eq!(dbs[0].host.as_deref(), Some("35.1.1.1"));
        assert_eq!(dbs[0].port, Some(5432));
        assert!(dbs[0].is_up());
        let gke: Json = serde_json::from_str(
            r#"{"clusters":[{"name":"main","location":"europe-west1","status":"RUNNING","endpoint":"34.9.9.9","currentMasterVersion":"1.30.1-gke.1"}]}"#,
        )
        .unwrap();
        assert_eq!(parse_gke(&gke, "p")[0].rtype, "GKE 1.30.1-gke.1");
        let gcs: Json =
            serde_json::from_str(r#"{"items":[{"name":"assets","location":"EU"}]}"#).unwrap();
        assert_eq!(parse_gcs(&gcs, "p")[0].region, "eu");
    }
}
