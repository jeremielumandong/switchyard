//! Azure inventory: one Azure Resource Graph query over every subscription the sign-in can
//! read (virtual machines with their network interfaces and public IPs, databases, caches,
//! load balancers, application gateways, AKS clusters, storage accounts), and the
//! subscriptions' names. Also lists Log Analytics workspaces for the log viewer.

use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Method;
use secrecy::ExposeSecret as _;
use serde_json::{Value as Json, json};

use super::{CloudResource, DbFlavor, Provider, ResourceKind};
use crate::auth::TokenSource;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client};

/// The Azure Resource Manager scope (Resource Graph lives there).
pub const ARM_SCOPE: &str = "https://management.azure.com/.default";
const ARM: &str = "https://management.azure.com";
const API_VERSION: &str = "2022-10-01";
/// Rows per page (the service maximum).
const PAGE: u32 = 1000;
/// Most pages read.
const MAX_PAGES: usize = 20;

/// The inventory query: the resource types shown, plus the network interfaces and public
/// IP addresses that give virtual machines and load balancers their addresses.
pub const INVENTORY_QUERY: &str = "Resources \
| where type in~ ('microsoft.compute/virtualmachines', 'microsoft.network/networkinterfaces', \
'microsoft.network/publicipaddresses', 'microsoft.sql/servers', \
'microsoft.dbforpostgresql/flexibleservers', 'microsoft.dbformysql/flexibleservers', \
'microsoft.documentdb/databaseaccounts', 'microsoft.cache/redis', \
'microsoft.network/loadbalancers', 'microsoft.network/applicationgateways', \
'microsoft.containerservice/managedclusters', 'microsoft.storage/storageaccounts') \
| project id, name, type, location, resourceGroup, subscriptionId, tags, properties, kind";

const SUBSCRIPTIONS_QUERY: &str = "ResourceContainers \
| where type =~ 'microsoft.resources/subscriptions' | project subscriptionId, name";

const WORKSPACES_QUERY: &str = "Resources \
| where type =~ 'microsoft.operationalinsights/workspaces' \
| project name, location, resourceGroup, subscriptionId, customerId = tostring(properties.customerId)";

/// A Log Analytics workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaWorkspace {
    /// Name.
    pub name: String,
    /// Workspace (customer) id used by the query API.
    pub customer_id: String,
    /// Location.
    pub location: String,
    /// Resource group.
    pub resource_group: String,
}

/// Azure Resource Graph through Azure Resource Manager.
pub struct AzureInventory {
    token: Arc<dyn TokenSource>,
    http: reqwest::Client,
    base: url::Url,
    /// Shown before the subscription name (the connection's name).
    label: String,
}

fn s<'a>(v: &'a Json, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr)
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
}

fn lower_id(v: &Json, ptr: &str) -> Option<String> {
    s(v, ptr).map(str::to_ascii_lowercase)
}

/// Resources from Resource Graph rows (`data` of the reply, object-array format), with
/// subscription ids replaced by `subscriptions` names.
pub fn parse_resources(
    rows: &[Json],
    subscriptions: &HashMap<String, String>,
    label: &str,
) -> Vec<CloudResource> {
    // Public IP id → address; NIC id → (private IPs, public IPs).
    let mut pips: HashMap<String, String> = HashMap::new();
    for r in rows {
        if r.get("type")
            .and_then(Json::as_str)
            .map(str::to_ascii_lowercase)
            .as_deref()
            == Some("microsoft.network/publicipaddresses")
            && let (Some(id), Some(ip)) = (lower_id(r, "/id"), s(r, "/properties/ipAddress"))
        {
            pips.insert(id, ip.to_owned());
        }
    }
    let mut nics: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for r in rows {
        if r.get("type")
            .and_then(Json::as_str)
            .map(str::to_ascii_lowercase)
            .as_deref()
            != Some("microsoft.network/networkinterfaces")
        {
            continue;
        }
        let Some(id) = lower_id(r, "/id") else {
            continue;
        };
        let entry = nics.entry(id).or_default();
        for c in r
            .pointer("/properties/ipConfigurations")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(ip) = s(c, "/properties/privateIPAddress") {
                entry.0.push(ip.to_owned());
            }
            if let Some(pip) = lower_id(c, "/properties/publicIPAddress/id")
                && let Some(ip) = pips.get(&pip)
            {
                entry.1.push(ip.clone());
            }
        }
    }
    let mut out = Vec::new();
    for r in rows {
        let ty = r
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let sub = s(r, "/subscriptionId").unwrap_or_default();
        let sub_name = subscriptions.get(sub).map_or(sub, String::as_str);
        let account = if label.is_empty() {
            sub_name.to_owned()
        } else {
            format!("{label} · {sub_name}")
        };
        let mut res = CloudResource {
            provider: Some(Provider::Azure),
            account,
            region: s(r, "/location").unwrap_or_default().to_owned(),
            id: s(r, "/id").unwrap_or_default().to_owned(),
            name: s(r, "/name").unwrap_or_default().to_owned(),
            tags: r
                .get("tags")
                .and_then(Json::as_object)
                .map(|t| {
                    t.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            ..CloudResource::default()
        };
        let state = |ptrs: &[&str]| {
            ptrs.iter()
                .find_map(|p| s(r, p))
                .unwrap_or_default()
                .to_owned()
        };
        match ty.as_str() {
            "microsoft.compute/virtualmachines" => {
                res.kind = Some(ResourceKind::Vm);
                res.rtype = format!(
                    "VM {}",
                    s(r, "/properties/hardwareProfile/vmSize").unwrap_or_default()
                )
                .trim()
                .to_owned();
                res.state = state(&[
                    "/properties/extended/instanceView/powerState/displayStatus",
                    "/properties/extended/instanceView/powerState/code",
                    "/properties/provisioningState",
                ]);
                res.user = s(r, "/properties/osProfile/adminUsername").map(str::to_owned);
                let windows = s(r, "/properties/storageProfile/osDisk/osType")
                    .is_some_and(|o| o.eq_ignore_ascii_case("windows"));
                res.port = Some(if windows { 3389 } else { 22 });
                for n in r
                    .pointer("/properties/networkProfile/networkInterfaces")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some((private, public)) = lower_id(n, "/id").and_then(|id| nics.get(&id))
                    {
                        res.private_ips.extend(private.iter().cloned());
                        res.public_ips.extend(public.iter().cloned());
                    }
                }
            }
            "microsoft.sql/servers"
            | "microsoft.dbforpostgresql/flexibleservers"
            | "microsoft.dbformysql/flexibleservers" => {
                res.kind = Some(ResourceKind::Database);
                let (label, db, port) = match ty.as_str() {
                    "microsoft.sql/servers" => ("Azure SQL server", DbFlavor::SqlServer, 1433),
                    "microsoft.dbforpostgresql/flexibleservers" => {
                        ("PostgreSQL flexible server", DbFlavor::Postgres, 5432)
                    }
                    _ => ("MySQL flexible server", DbFlavor::MySql, 3306),
                };
                res.rtype = match s(r, "/properties/version") {
                    Some(v) => format!("{label} {v}"),
                    None => label.to_owned(),
                };
                res.db = Some(db);
                res.port = Some(port);
                res.host = s(r, "/properties/fullyQualifiedDomainName").map(str::to_owned);
                res.user = s(r, "/properties/administratorLogin").map(str::to_owned);
                res.state = state(&["/properties/state", "/properties/provisioningState"]);
            }
            "microsoft.cache/redis" => {
                res.kind = Some(ResourceKind::Database);
                res.rtype = "Azure Cache for Redis".into();
                res.db = Some(DbFlavor::Redis);
                res.host = s(r, "/properties/hostName").map(str::to_owned);
                res.port = r
                    .pointer("/properties/sslPort")
                    .and_then(Json::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .or(Some(6380));
                res.state = state(&["/properties/provisioningState"]);
            }
            "microsoft.documentdb/databaseaccounts" => {
                res.kind = Some(ResourceKind::Database);
                let mongo = s(r, "/kind").is_some_and(|k| k.eq_ignore_ascii_case("MongoDB"));
                res.rtype = if mongo {
                    "Cosmos DB (MongoDB)".into()
                } else {
                    "Cosmos DB".into()
                };
                res.db = mongo.then_some(DbFlavor::MongoDb);
                res.host = s(r, "/properties/documentEndpoint")
                    .and_then(|u| url::Url::parse(u).ok())
                    .and_then(|u| u.host_str().map(str::to_owned));
                res.state = state(&["/properties/provisioningState"]);
            }
            "microsoft.network/loadbalancers" | "microsoft.network/applicationgateways" => {
                res.kind = Some(ResourceKind::LoadBalancer);
                res.rtype = if ty.ends_with("loadbalancers") {
                    "Load balancer".into()
                } else {
                    "Application gateway".into()
                };
                res.state = state(&[
                    "/properties/operationalState",
                    "/properties/provisioningState",
                ]);
                for f in r
                    .pointer("/properties/frontendIPConfigurations")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(ip) = s(f, "/properties/privateIPAddress") {
                        res.private_ips.push(ip.to_owned());
                    }
                    if let Some(ip) =
                        lower_id(f, "/properties/publicIPAddress/id").and_then(|p| pips.get(&p))
                    {
                        res.public_ips.push(ip.clone());
                    }
                }
            }
            "microsoft.containerservice/managedclusters" => {
                res.kind = Some(ResourceKind::Kubernetes);
                res.rtype = format!(
                    "AKS {}",
                    s(r, "/properties/kubernetesVersion").unwrap_or_default()
                )
                .trim()
                .to_owned();
                res.host = s(r, "/properties/fqdn")
                    .or_else(|| s(r, "/properties/privateFQDN"))
                    .map(str::to_owned);
                res.port = Some(443);
                res.state = state(&[
                    "/properties/powerState/code",
                    "/properties/provisioningState",
                ]);
            }
            "microsoft.storage/storageaccounts" => {
                res.kind = Some(ResourceKind::Storage);
                res.rtype = format!("Storage account {}", s(r, "/kind").unwrap_or_default())
                    .trim()
                    .to_owned();
                res.host = s(r, "/properties/primaryEndpoints/blob")
                    .and_then(|u| url::Url::parse(u).ok())
                    .and_then(|u| u.host_str().map(str::to_owned));
                res.state = state(&[
                    "/properties/statusOfPrimary",
                    "/properties/provisioningState",
                ]);
            }
            _ => continue,
        }
        out.push(res);
    }
    out
}

/// Subscription id → name from a subscriptions query's rows.
pub fn parse_subscriptions(rows: &[Json]) -> HashMap<String, String> {
    rows.iter()
        .filter_map(|r| {
            Some((
                s(r, "/subscriptionId")?.to_owned(),
                s(r, "/name")?.to_owned(),
            ))
        })
        .collect()
}

/// Workspaces from a workspaces query's rows.
pub fn parse_workspaces(rows: &[Json]) -> Vec<LaWorkspace> {
    let mut out: Vec<LaWorkspace> = rows
        .iter()
        .filter_map(|r| {
            Some(LaWorkspace {
                name: s(r, "/name")?.to_owned(),
                customer_id: s(r, "/customerId")?.to_owned(),
                location: s(r, "/location").unwrap_or_default().to_owned(),
                resource_group: s(r, "/resourceGroup").unwrap_or_default().to_owned(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

impl AzureInventory {
    /// A client with ARM tokens from `token`; `label` prefixes the account column.
    pub fn new(token: Arc<dyn TokenSource>, label: String, base: Option<url::Url>) -> Result<Self> {
        let base = match base {
            Some(b) => b,
            None => url::Url::parse(ARM).map_err(|e| CloudError::Invalid(e.to_string()))?,
        };
        Ok(Self {
            token,
            http: client()?,
            base,
            label,
        })
    }

    /// Every row of a Resource Graph query (following `$skipToken`).
    pub async fn query(&self, kql: &str) -> Result<Vec<Json>> {
        let mut rows = Vec::new();
        let mut skip: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut options = json!({ "$top": PAGE, "resultFormat": "objectArray" });
            if let Some(t) = &skip {
                options["$skipToken"] = Json::String(t.clone());
            }
            let body = json!({ "query": kql, "options": options });
            let mut req = Req::new(
                Method::POST,
                &self.base,
                "/providers/Microsoft.ResourceGraph/resources",
            )
            .query("api-version", API_VERSION)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).map_err(|e| CloudError::Invalid(e.to_string()))?);
            let t = self.token.token().await?;
            req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
            let resp = Resp::read(req.send(&self.http).await?).await?;
            let json = resp.json().unwrap_or(Json::Null);
            if !resp.ok() {
                let code = s(&json, "/error/code").map(str::to_owned);
                let msg = s(&json, "/error/message").unwrap_or_default().to_owned();
                return Err(CloudError::api(resp.status, code, msg));
            }
            if let Some(Json::Array(data)) = json.get("data") {
                rows.extend(data.iter().cloned());
            }
            skip = s(&json, "/$skipToken").map(str::to_owned);
            if skip.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    /// Every inventoried resource the sign-in can see.
    pub async fn resources(&self) -> Result<Vec<CloudResource>> {
        let subs = self
            .query(SUBSCRIPTIONS_QUERY)
            .await
            .map(|rows| parse_subscriptions(&rows))
            .unwrap_or_default();
        let rows = self.query(INVENTORY_QUERY).await?;
        Ok(parse_resources(&rows, &subs, &self.label))
    }

    /// Log Analytics workspaces the sign-in can see.
    pub async fn log_workspaces(&self) -> Result<Vec<LaWorkspace>> {
        Ok(parse_workspaces(&self.query(WORKSPACES_QUERY).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_graph_rows() {
        let path = format!(
            "{}/testdata/inventory/azure-resource-graph.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let json: Json = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let rows = json["data"].as_array().unwrap();
        let subs = parse_subscriptions(&[serde_json::json!({
            "subscriptionId": "sub-1", "name": "Production"
        })]);
        let items = parse_resources(rows, &subs, "corp");
        let names: Vec<&str> = items.iter().map(|r| r.name.as_str()).collect();
        // NICs and public IPs only feed addresses.
        assert_eq!(names, ["vm-web", "pg-orders", "aks-main", "stacme"]);
        let vm = &items[0];
        assert_eq!(vm.account, "corp · Production");
        assert_eq!(vm.kind, Some(ResourceKind::Vm));
        assert_eq!(vm.rtype, "VM Standard_B2s");
        assert_eq!(vm.state, "VM running");
        assert_eq!(vm.user.as_deref(), Some("azureuser"));
        assert_eq!(vm.private_ips, ["10.1.0.4"]);
        assert_eq!(vm.public_ips, ["20.1.2.3"]);
        assert_eq!(vm.address(), Some("20.1.2.3"));
        assert!(vm.is_up());
        let pg = &items[1];
        assert_eq!(pg.db, Some(DbFlavor::Postgres));
        assert_eq!(
            pg.host.as_deref(),
            Some("pg-orders.postgres.database.azure.com")
        );
        assert_eq!(pg.port, Some(5432));
        assert_eq!(pg.user.as_deref(), Some("pgadmin"));
        assert_eq!(pg.rtype, "PostgreSQL flexible server 16");
        let aks = &items[2];
        assert_eq!(aks.rtype, "AKS 1.30.3");
        assert_eq!(aks.state, "Running");
        assert_eq!(
            items[3].host.as_deref(),
            Some("stacme.blob.core.windows.net")
        );
        assert_eq!(items[3].tags, [("env".to_owned(), "prod".to_owned())]);
    }

    #[test]
    fn workspaces() {
        let w = parse_workspaces(&[serde_json::json!({
            "name": "logs-prod", "location": "westeurope", "resourceGroup": "rg",
            "subscriptionId": "s", "customerId": "0000-1111"
        })]);
        assert_eq!(w[0].customer_id, "0000-1111");
    }
}
