//! Cloud resource inventory: virtual machines, managed databases, load balancers,
//! Kubernetes clusters and storage across AWS, Azure, Cloudflare and Google Cloud, in one
//! model ([`CloudResource`]). Read-only list calls only.
//!
//! - AWS ([`aws`]): EC2 `DescribeInstances`, RDS `DescribeDBInstances`, ELBv2
//!   `DescribeLoadBalancers`, EKS `ListClusters` / `DescribeCluster`, S3 `ListBuckets`,
//!   signed with SigV4 per region.
//! - Azure ([`azure`]): one Resource Graph query over every subscription the sign-in sees.
//! - Cloudflare ([`cloudflare`]): R2 buckets and D1 databases per account.
//! - Google Cloud ([`gcp`]): Compute Engine, Cloud SQL and GKE with a token from
//!   `gcloud auth print-access-token`.

pub mod aws;
pub mod azure;
pub mod cloudflare;
pub mod gcp;

/// Which cloud a resource is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    /// Amazon Web Services.
    Aws,
    /// Microsoft Azure.
    Azure,
    /// Cloudflare.
    Cloudflare,
    /// Google Cloud.
    Gcp,
}

impl Provider {
    /// Short name for the table.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Aws => "AWS",
            Provider::Azure => "Azure",
            Provider::Cloudflare => "Cloudflare",
            Provider::Gcp => "GCP",
        }
    }
}

/// What a resource is, broadly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceKind {
    /// A virtual machine / instance.
    Vm,
    /// A managed database.
    Database,
    /// A load balancer or application gateway.
    LoadBalancer,
    /// A Kubernetes cluster.
    Kubernetes,
    /// Object storage (bucket, storage account).
    Storage,
}

impl ResourceKind {
    /// Every kind, in table order.
    pub const ALL: [ResourceKind; 5] = [
        ResourceKind::Vm,
        ResourceKind::Database,
        ResourceKind::LoadBalancer,
        ResourceKind::Kubernetes,
        ResourceKind::Storage,
    ];

    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            ResourceKind::Vm => "VM",
            ResourceKind::Database => "Database",
            ResourceKind::LoadBalancer => "Load balancer",
            ResourceKind::Kubernetes => "Kubernetes",
            ResourceKind::Storage => "Storage",
        }
    }
}

/// The database engine of a managed database, when Switchyard can connect to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DbFlavor {
    /// PostgreSQL (and Aurora PostgreSQL).
    Postgres,
    /// MySQL, MariaDB, Aurora MySQL.
    MySql,
    /// SQL Server / Azure SQL.
    SqlServer,
    /// Oracle.
    Oracle,
    /// Redis-compatible caches.
    Redis,
    /// MongoDB-compatible (DocumentDB, Cosmos DB for MongoDB).
    MongoDb,
    /// Cloudflare D1.
    D1,
}

/// One resource.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CloudResource {
    /// Provider.
    pub provider: Option<Provider>,
    /// Account: AWS profile / connection and account id, Azure subscription, Cloudflare
    /// account, GCP project.
    pub account: String,
    /// Region or location (`global` for global services).
    pub region: String,
    /// Broad kind.
    pub kind: Option<ResourceKind>,
    /// The provider's type (`EC2 t3.micro`, `RDS postgres 16.3`, `Microsoft.Compute/virtualMachines`).
    pub rtype: String,
    /// Provider id (instance id, ARN, Azure resource id).
    pub id: String,
    /// Name (the `Name` tag for EC2).
    pub name: String,
    /// State as the provider reports it (`running`, `available`, `Succeeded`).
    pub state: String,
    /// Public addresses.
    pub public_ips: Vec<String>,
    /// Private addresses.
    pub private_ips: Vec<String>,
    /// DNS name or endpoint host (RDS endpoint, LB DNS name, AKS FQDN).
    pub host: Option<String>,
    /// Port of the endpoint, when known.
    pub port: Option<u16>,
    /// Database engine Switchyard can connect to.
    pub db: Option<DbFlavor>,
    /// Login user hint (Azure VM admin user, RDS master user).
    pub user: Option<String>,
    /// Tags.
    pub tags: Vec<(String, String)>,
}

impl CloudResource {
    /// Where to connect: a public IP, else the host name, else a private IP.
    pub fn address(&self) -> Option<&str> {
        self.public_ips
            .first()
            .or(self.host.as_ref())
            .or(self.private_ips.first())
            .map(String::as_str)
    }

    /// Every address, public first, joined for the table.
    pub fn ips_text(&self) -> String {
        self.public_ips
            .iter()
            .chain(self.private_ips.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `k=v, k=v`.
    pub fn tags_text(&self) -> String {
        self.tags
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether the state reads as running / available.
    pub fn is_up(&self) -> bool {
        let s = self.state.to_ascii_lowercase();
        [
            "running",
            "available",
            "active",
            "succeeded",
            "ready",
            "runnable",
            "online",
        ]
        .iter()
        .any(|k| s.contains(k))
            && !s.contains("deallocat")
            && !s.contains("stopp")
    }

    /// Matches every whitespace-separated word of `query` (case-insensitive) somewhere in
    /// its provider, account, region, kind, type, name, id, state, addresses or tags.
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!(
            "{} {} {} {} {} {} {} {} {} {} {}",
            self.provider.map_or("", Provider::label),
            self.account,
            self.region,
            self.kind.map_or("", ResourceKind::label),
            self.rtype,
            self.name,
            self.id,
            self.state,
            self.ips_text(),
            self.host.as_deref().unwrap_or_default(),
            self.tags_text()
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|w| hay.contains(&w.to_lowercase()))
    }
}

/// The engine of a database engine name (`aurora-postgresql`, `sqlserver-se`, `mariadb`).
pub fn flavor_of(engine: &str) -> Option<DbFlavor> {
    let e = engine.to_ascii_lowercase();
    Some(if e.contains("postgres") {
        DbFlavor::Postgres
    } else if e.contains("mysql") || e.contains("mariadb") {
        DbFlavor::MySql
    } else if e.contains("sqlserver") || e.contains("sql server") || e == "mssql" {
        DbFlavor::SqlServer
    } else if e.contains("oracle") {
        DbFlavor::Oracle
    } else if e.contains("redis") || e.contains("valkey") {
        DbFlavor::Redis
    } else if e.contains("docdb") || e.contains("mongo") {
        DbFlavor::MongoDb
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_matching() {
        let r = CloudResource {
            provider: Some(Provider::Aws),
            account: "prod (123)".into(),
            region: "eu-west-1".into(),
            kind: Some(ResourceKind::Vm),
            rtype: "EC2 t3.micro".into(),
            id: "i-1".into(),
            name: "web".into(),
            state: "running".into(),
            public_ips: vec![],
            private_ips: vec!["10.0.0.5".into()],
            tags: vec![("Env".into(), "prod".into())],
            ..CloudResource::default()
        };
        assert_eq!(r.address(), Some("10.0.0.5"));
        assert!(r.matches("aws web"));
        assert!(r.matches("env=prod"));
        assert!(!r.matches("azure"));
        assert!(r.is_up());
        assert_eq!(flavor_of("aurora-postgresql"), Some(DbFlavor::Postgres));
        assert_eq!(flavor_of("sqlserver-ex"), Some(DbFlavor::SqlServer));
        assert_eq!(flavor_of("neptune"), None);
    }
}
