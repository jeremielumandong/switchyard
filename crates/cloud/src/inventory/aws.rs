//! AWS inventory over the query APIs (EC2, RDS, ELBv2, STS: form-encoded POST, XML
//! replies), EKS's REST API and S3's bucket list, all signed with SigV4.

use std::sync::Arc;
use std::time::SystemTime;

use reqwest::Method;
use serde_json::Value as Json;
use tracing::debug;

use super::{CloudResource, DbFlavor, Provider, ResourceKind, flavor_of};
use crate::aws::AwsAuth;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, query_string};
use crate::sigv4::sign;
use crate::xml::{self, Node};

/// Most pages read per API and region.
const MAX_PAGES: usize = 20;
/// Most EKS clusters described per region.
const MAX_CLUSTERS: usize = 50;

/// AWS clients for one account (credentials) and any region.
pub struct AwsInventory {
    auth: Arc<AwsAuth>,
    http: reqwest::Client,
    /// One endpoint for every service (moto in tests); `None` for AWS.
    endpoint: Option<url::Url>,
    /// Shown in the Account column.
    account: String,
}

/// The first `<Error>` element of an error reply (`ErrorResponse/Error` or
/// `Response/Errors/Error`).
fn find_error(n: &Node) -> Option<&Node> {
    if n.name == "Error" {
        return Some(n);
    }
    n.children.iter().find_map(find_error)
}

fn query_error(status: u16, body: &[u8]) -> CloudError {
    match xml::parse(body).ok().as_ref().and_then(find_error) {
        Some(e) => CloudError::api(
            status,
            e.text_of("Code").map(str::to_owned),
            e.text_of("Message").unwrap_or_default(),
        ),
        None => CloudError::api(status, None, ""),
    }
}

fn tags(set: Option<&Node>, item: &str, k: &str, v: &str) -> Vec<(String, String)> {
    set.map(|s| {
        s.all(item)
            .filter_map(|t| {
                Some((
                    t.text_of(k)?.to_owned(),
                    t.text_of(v).unwrap_or_default().to_owned(),
                ))
            })
            .collect()
    })
    .unwrap_or_default()
}

fn name_tag(tags: &[(String, String)]) -> Option<String> {
    tags.iter()
        .find(|(k, _)| k == "Name")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// Instances in a `DescribeInstances` reply, and its `nextToken`.
pub(crate) fn parse_ec2(
    root: &Node,
    account: &str,
    region: &str,
) -> (Vec<CloudResource>, Option<String>) {
    let mut out = Vec::new();
    for res in root
        .child("reservationSet")
        .into_iter()
        .flat_map(|s| s.all("item"))
    {
        for i in res
            .child("instancesSet")
            .into_iter()
            .flat_map(|s| s.all("item"))
        {
            let Some(id) = i.text_of("instanceId") else {
                continue;
            };
            let tags = tags(i.child("tagSet"), "item", "key", "value");
            let platform = i
                .text_of("platformDetails")
                .or_else(|| i.text_of("platform"))
                .unwrap_or("Linux/UNIX");
            let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_owned);
            out.push(CloudResource {
                provider: Some(Provider::Aws),
                account: account.to_owned(),
                region: i
                    .path(&["placement", "availabilityZone"])
                    .filter(|z| !z.is_empty())
                    .unwrap_or(region)
                    .to_owned(),
                kind: Some(ResourceKind::Vm),
                rtype: format!("EC2 {}", i.text_of("instanceType").unwrap_or_default())
                    .trim()
                    .to_owned(),
                id: id.to_owned(),
                name: name_tag(&tags).unwrap_or_else(|| id.to_owned()),
                state: i
                    .path(&["instanceState", "name"])
                    .unwrap_or_default()
                    .to_owned(),
                public_ips: nonempty(i.text_of("ipAddress")).into_iter().collect(),
                private_ips: nonempty(i.text_of("privateIpAddress"))
                    .into_iter()
                    .collect(),
                host: nonempty(i.text_of("dnsName")),
                port: Some(if platform.contains("Windows") {
                    3389
                } else {
                    22
                }),
                db: None,
                user: (!platform.contains("Windows")).then(|| "ec2-user".to_owned()),
                tags,
            });
        }
    }
    let next = root
        .text_of("nextToken")
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    (out, next)
}

/// Databases in a `DescribeDBInstances` reply, and its `Marker`.
pub(crate) fn parse_rds(
    root: &Node,
    account: &str,
    region: &str,
) -> (Vec<CloudResource>, Option<String>) {
    let result = root.child("DescribeDBInstancesResult").unwrap_or(root);
    let mut out = Vec::new();
    for d in result
        .child("DBInstances")
        .into_iter()
        .flat_map(|s| s.all("DBInstance"))
    {
        let Some(name) = d.text_of("DBInstanceIdentifier") else {
            continue;
        };
        let engine = d.text_of("Engine").unwrap_or_default();
        out.push(CloudResource {
            provider: Some(Provider::Aws),
            account: account.to_owned(),
            region: d
                .text_of("AvailabilityZone")
                .filter(|z| !z.is_empty())
                .unwrap_or(region)
                .to_owned(),
            kind: Some(ResourceKind::Database),
            rtype: format!(
                "RDS {engine} {}",
                d.text_of("EngineVersion").unwrap_or_default()
            )
            .trim()
            .to_owned(),
            id: d.text_of("DBInstanceArn").unwrap_or(name).to_owned(),
            name: name.to_owned(),
            state: d.text_of("DBInstanceStatus").unwrap_or_default().to_owned(),
            public_ips: Vec::new(),
            private_ips: Vec::new(),
            host: d
                .path(&["Endpoint", "Address"])
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
            port: d.path(&["Endpoint", "Port"]).and_then(|p| p.parse().ok()),
            db: flavor_of(engine),
            user: d
                .text_of("MasterUsername")
                .filter(|u| !u.is_empty())
                .map(str::to_owned),
            tags: tags(d.child("TagList"), "Tag", "Key", "Value"),
        });
    }
    let next = result
        .text_of("Marker")
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    (out, next)
}

/// Load balancers in a `DescribeLoadBalancers` (ELBv2) reply, and its `NextMarker`.
pub(crate) fn parse_elbv2(
    root: &Node,
    account: &str,
    region: &str,
) -> (Vec<CloudResource>, Option<String>) {
    let result = root.child("DescribeLoadBalancersResult").unwrap_or(root);
    let mut out = Vec::new();
    for lb in result
        .child("LoadBalancers")
        .into_iter()
        .flat_map(|s| s.all("member"))
    {
        let Some(name) = lb.text_of("LoadBalancerName") else {
            continue;
        };
        let kind = lb.text_of("Type").unwrap_or("application");
        let short = match kind {
            "application" => "ALB",
            "network" => "NLB",
            "gateway" => "GWLB",
            other => other,
        };
        out.push(CloudResource {
            provider: Some(Provider::Aws),
            account: account.to_owned(),
            region: region.to_owned(),
            kind: Some(ResourceKind::LoadBalancer),
            rtype: format!(
                "{short} ({})",
                lb.text_of("Scheme").unwrap_or("internet-facing")
            ),
            id: lb.text_of("LoadBalancerArn").unwrap_or(name).to_owned(),
            name: name.to_owned(),
            state: lb.path(&["State", "Code"]).unwrap_or_default().to_owned(),
            host: lb
                .text_of("DNSName")
                .filter(|d| !d.is_empty())
                .map(str::to_owned),
            port: None,
            ..CloudResource::default()
        });
    }
    let next = result
        .text_of("NextMarker")
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    (out, next)
}

/// One cluster of an EKS `DescribeCluster` reply.
pub fn parse_eks_cluster(json: &Json, account: &str, region: &str) -> Option<CloudResource> {
    let c = json.get("cluster")?;
    let name = c.get("name")?.as_str()?;
    let s = |k: &str| c.get(k).and_then(Json::as_str).unwrap_or_default();
    let host = url::Url::parse(s("endpoint"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    Some(CloudResource {
        provider: Some(Provider::Aws),
        account: account.to_owned(),
        region: region.to_owned(),
        kind: Some(ResourceKind::Kubernetes),
        rtype: format!("EKS {}", s("version")).trim().to_owned(),
        id: if s("arn").is_empty() {
            name.to_owned()
        } else {
            s("arn").to_owned()
        },
        name: name.to_owned(),
        state: s("status").to_owned(),
        host,
        port: Some(443),
        tags: c
            .get("tags")
            .and_then(Json::as_object)
            .map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        ..CloudResource::default()
    })
}

/// Buckets in a `ListBuckets` reply.
pub(crate) fn parse_buckets(root: &Node, account: &str) -> Vec<CloudResource> {
    root.child("Buckets")
        .into_iter()
        .flat_map(|b| b.all("Bucket"))
        .filter_map(|b| {
            let name = b.text_of("Name")?;
            Some(CloudResource {
                provider: Some(Provider::Aws),
                account: account.to_owned(),
                region: b
                    .text_of("BucketRegion")
                    .filter(|r| !r.is_empty())
                    .unwrap_or("global")
                    .to_owned(),
                kind: Some(ResourceKind::Storage),
                rtype: "S3 bucket".into(),
                id: format!("arn:aws:s3:::{name}"),
                name: name.to_owned(),
                state: "available".into(),
                ..CloudResource::default()
            })
        })
        .collect()
}

impl AwsInventory {
    /// Clients signing with `auth`; `account` labels every row; `endpoint` replaces every
    /// service's endpoint (tests).
    pub fn new(auth: Arc<AwsAuth>, account: String, endpoint: Option<url::Url>) -> Result<Self> {
        Ok(Self {
            auth,
            http: client()?,
            endpoint,
            account,
        })
    }

    /// The same clients labelling rows `account` (once the account id is known).
    pub fn with_account(mut self, account: String) -> Self {
        self.account = account;
        self
    }

    /// The account label shown in rows.
    pub fn account(&self) -> &str {
        &self.account
    }

    fn base(&self, service: &str, region: &str) -> Result<url::Url> {
        if let Some(e) = &self.endpoint {
            return Ok(e.clone());
        }
        let host = match service {
            "sts" => "sts.amazonaws.com".to_owned(),
            "s3" if region == "us-east-1" => "s3.amazonaws.com".to_owned(),
            "elasticloadbalancing" => format!("elasticloadbalancing.{region}.amazonaws.com"),
            s => format!("{s}.{region}.amazonaws.com"),
        };
        url::Url::parse(&format!("https://{host}")).map_err(|e| CloudError::Invalid(e.to_string()))
    }

    async fn send(&self, mut req: Req, service: &str, region: &str) -> Result<Resp> {
        let creds = self.auth.credentials().await?;
        sign(
            &mut req,
            &creds,
            region,
            service,
            SystemTime::now(),
            service == "s3",
        );
        let resp = Resp::read(req.send(&self.http).await?).await?;
        if resp.status == 403 || resp.status == 401 {
            self.auth.reset().await;
        }
        Ok(resp)
    }

    /// A query API call (form POST, XML reply).
    async fn query(&self, service: &str, region: &str, params: &[(&str, &str)]) -> Result<Node> {
        let pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let req = Req::new(Method::POST, &self.base(service, region)?, "/")
            .header(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )
            .body(query_string(&pairs).into_bytes());
        let resp = self.send(req, service, region).await?;
        if !resp.ok() {
            return Err(query_error(resp.status, &resp.body));
        }
        xml::parse(&resp.body)
    }

    /// The account id the credentials belong to (STS `GetCallerIdentity`).
    pub async fn account_id(&self) -> Result<String> {
        let root = self
            .query(
                "sts",
                "us-east-1",
                &[("Action", "GetCallerIdentity"), ("Version", "2011-06-15")],
            )
            .await?;
        root.child("GetCallerIdentityResult")
            .and_then(|r| r.text_of("Account"))
            .map(str::to_owned)
            .ok_or_else(|| CloudError::Invalid("STS returned no account".into()))
    }

    /// Regions enabled for the account (EC2 `DescribeRegions`, asked in `region`).
    pub async fn regions(&self, region: &str) -> Result<Vec<String>> {
        let root = self
            .query(
                "ec2",
                region,
                &[("Action", "DescribeRegions"), ("Version", "2016-11-15")],
            )
            .await?;
        let mut out: Vec<String> = root
            .child("regionInfo")
            .into_iter()
            .flat_map(|r| r.all("item"))
            .filter_map(|i| i.text_of("regionName").map(str::to_owned))
            .collect();
        out.sort();
        Ok(out)
    }

    /// EC2 instances in `region`.
    pub async fn instances(&self, region: &str) -> Result<Vec<CloudResource>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut p = vec![("Action", "DescribeInstances"), ("Version", "2016-11-15")];
            if let Some(t) = &token {
                p.push(("NextToken", t.as_str()));
            }
            let root = self.query("ec2", region, &p).await?;
            let (items, next) = parse_ec2(&root, &self.account, region);
            out.extend(items);
            token = next;
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// RDS instances in `region`.
    pub async fn databases(&self, region: &str) -> Result<Vec<CloudResource>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut p = vec![("Action", "DescribeDBInstances"), ("Version", "2014-10-31")];
            if let Some(m) = &marker {
                p.push(("Marker", m.as_str()));
            }
            let root = self.query("rds", region, &p).await?;
            let (items, next) = parse_rds(&root, &self.account, region);
            out.extend(items);
            marker = next;
            if marker.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Application, network and gateway load balancers in `region`.
    pub async fn load_balancers(&self, region: &str) -> Result<Vec<CloudResource>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut p = vec![
                ("Action", "DescribeLoadBalancers"),
                ("Version", "2015-12-01"),
            ];
            if let Some(m) = &marker {
                p.push(("Marker", m.as_str()));
            }
            let root = self.query("elasticloadbalancing", region, &p).await?;
            let (items, next) = parse_elbv2(&root, &self.account, region);
            out.extend(items);
            marker = next;
            if marker.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn rest_json(&self, service: &str, region: &str, path: &str) -> Result<Json> {
        let req = Req::new(Method::GET, &self.base(service, region)?, path);
        let resp = self.send(req, service, region).await?;
        let json = resp.json().unwrap_or(Json::Null);
        if !resp.ok() {
            let msg = json
                .get("message")
                .or_else(|| json.get("Message"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            return Err(CloudError::api(resp.status, None, msg));
        }
        Ok(json)
    }

    /// EKS clusters in `region`.
    pub async fn clusters(&self, region: &str) -> Result<Vec<CloudResource>> {
        let list = self.rest_json("eks", region, "/clusters").await?;
        let names: Vec<String> = list
            .get("clusters")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Json::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        for name in names.into_iter().take(MAX_CLUSTERS) {
            let path = format!("/clusters/{}", crate::http::uri_encode(&name, false));
            match self.rest_json("eks", region, &path).await {
                Ok(json) => out.extend(parse_eks_cluster(&json, &self.account, region)),
                Err(e) => {
                    debug!(cluster = %name, error = %e, "eks describe failed");
                    out.push(CloudResource {
                        provider: Some(Provider::Aws),
                        account: self.account.clone(),
                        region: region.to_owned(),
                        kind: Some(ResourceKind::Kubernetes),
                        rtype: "EKS".into(),
                        id: name.clone(),
                        name,
                        ..CloudResource::default()
                    });
                }
            }
        }
        Ok(out)
    }

    /// S3 buckets (global).
    pub async fn buckets(&self) -> Result<Vec<CloudResource>> {
        let req = Req::new(Method::GET, &self.base("s3", "us-east-1")?, "/");
        let resp = self.send(req, "s3", "us-east-1").await?;
        if !resp.ok() {
            return Err(query_error(resp.status, &resp.body));
        }
        Ok(parse_buckets(&xml::parse(&resp.body)?, &self.account))
    }
}

/// Whether `r` is a database Switchyard opens over SQL.
pub fn connectable(r: &CloudResource) -> bool {
    matches!(
        r.db,
        Some(
            DbFlavor::Postgres
                | DbFlavor::MySql
                | DbFlavor::SqlServer
                | DbFlavor::Oracle
                | DbFlavor::Redis
                | DbFlavor::MongoDb
        )
    ) && r.host.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Node {
        let path = format!("{}/testdata/inventory/{name}", env!("CARGO_MANIFEST_DIR"));
        xml::parse(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn ec2_instances() {
        let (items, next) = parse_ec2(&fixture("ec2-describe-instances.xml"), "prod", "eu-west-1");
        assert_eq!(next.as_deref(), Some("tok-2"));
        assert_eq!(items.len(), 2);
        let web = &items[0];
        assert_eq!(web.id, "i-0abc");
        assert_eq!(web.name, "web-1");
        assert_eq!(web.state, "running");
        assert_eq!(web.region, "eu-west-1a");
        assert_eq!(web.rtype, "EC2 t3.micro");
        assert_eq!(web.public_ips, ["54.1.2.3"]);
        assert_eq!(web.private_ips, ["10.0.1.10"]);
        assert_eq!(web.address(), Some("54.1.2.3"));
        assert_eq!(web.user.as_deref(), Some("ec2-user"));
        assert_eq!(web.port, Some(22));
        assert!(web.tags.contains(&("Env".into(), "prod".into())));
        let win = &items[1];
        assert_eq!(win.name, "i-0def");
        assert_eq!(win.state, "stopped");
        assert!(win.public_ips.is_empty());
        assert_eq!(win.port, Some(3389));
        assert!(win.user.is_none());
        assert!(!win.is_up());
    }

    #[test]
    fn rds_instances() {
        let (items, next) = parse_rds(
            &fixture("rds-describe-db-instances.xml"),
            "prod",
            "eu-west-1",
        );
        assert!(next.is_none());
        assert_eq!(items.len(), 1);
        let db = &items[0];
        assert_eq!(db.name, "orders");
        assert_eq!(db.db, Some(DbFlavor::Postgres));
        assert_eq!(
            db.host.as_deref(),
            Some("orders.abc.eu-west-1.rds.amazonaws.com")
        );
        assert_eq!(db.port, Some(5432));
        assert_eq!(db.user.as_deref(), Some("app"));
        assert_eq!(db.rtype, "RDS postgres 16.3");
        assert!(connectable(db));
    }

    #[test]
    fn load_balancers() {
        let (items, _) = parse_elbv2(
            &fixture("elbv2-describe-load-balancers.xml"),
            "prod",
            "eu-west-1",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rtype, "ALB (internet-facing)");
        assert_eq!(items[0].state, "active");
        assert_eq!(
            items[0].host.as_deref(),
            Some("web-123.eu-west-1.elb.amazonaws.com")
        );
    }

    #[test]
    fn eks_and_buckets() {
        let json: Json = serde_json::from_str(
            r#"{"cluster":{"name":"main","arn":"arn:aws:eks:eu-west-1:1:cluster/main","status":"ACTIVE","version":"1.30","endpoint":"https://ABC.gr7.eu-west-1.eks.amazonaws.com","tags":{"team":"core"}}}"#,
        )
        .unwrap();
        let c = parse_eks_cluster(&json, "prod", "eu-west-1").unwrap();
        assert_eq!(c.rtype, "EKS 1.30");
        assert_eq!(
            c.host.as_deref(),
            Some("abc.gr7.eu-west-1.eks.amazonaws.com")
        );
        assert_eq!(c.tags, [("team".to_owned(), "core".to_owned())]);
        assert!(c.is_up());
        let b = parse_buckets(
            &xml::parse(br#"<ListAllMyBucketsResult><Buckets><Bucket><Name>logs</Name><CreationDate>2024-01-01T00:00:00.000Z</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>"#).unwrap(),
            "prod",
        );
        assert_eq!(b[0].name, "logs");
        assert_eq!(b[0].kind, Some(ResourceKind::Storage));
    }

    #[test]
    fn query_errors() {
        let e = query_error(
            403,
            br#"<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized</Message></Error></Errors></Response>"#,
        );
        assert!(e.to_string().contains("UnauthorizedOperation"));
        let e = query_error(
            400,
            br#"<ErrorResponse><Error><Code>Throttling</Code><Message>Rate exceeded</Message></Error></ErrorResponse>"#,
        );
        assert!(e.to_string().contains("Rate exceeded"));
    }
}
