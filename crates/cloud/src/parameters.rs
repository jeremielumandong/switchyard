//! AWS Systems Manager Parameter Store: `String`, `StringList` and `SecureString`
//! parameters, names often hierarchical (`/app/prod/db-url`).

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value as Json, json};

use crate::aws::{AwsAuth, AwsJson};
use crate::error::Result;
use crate::kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite};

/// Parameter types.
pub const KINDS: [&str; 3] = ["String", "SecureString", "StringList"];

/// A region's parameters.
pub struct ParameterStore {
    api: AwsJson,
}

impl ParameterStore {
    /// Parameters in `region` (`endpoint` overrides the AWS endpoint, for tests).
    pub fn new(
        auth: Arc<AwsAuth>,
        region: &str,
        endpoint: Option<url::Url>,
        read_only: bool,
    ) -> Result<Self> {
        Ok(Self {
            api: AwsJson::new(auth, region, "ssm", "AmazonSSM", endpoint, read_only)?,
        })
    }
}

fn epoch_ms(v: Option<&Json>) -> Option<i64> {
    v.and_then(Json::as_f64).map(|s| (s * 1000.0) as i64)
}

impl KvService for ParameterStore {
    fn caps(&self) -> KvCaps {
        KvCaps {
            kinds: KINDS.to_vec(),
            filter_hint: "Name prefix (/app/prod/)",
            hierarchical: true,
            ..KvCaps::default()
        }
    }

    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        Box::pin(async move {
            let mut body = json!({ "MaxResults": 50 });
            let prefix = q.key.trim();
            if !prefix.is_empty() {
                body["ParameterFilters"] =
                    json!([{ "Key": "Name", "Option": "BeginsWith", "Values": [prefix] }]);
            }
            if let Some(c) = &q.cursor {
                body["NextToken"] = json!(c);
            }
            let json = self.api.call("DescribeParameters", body).await?;
            let items = json
                .get("Parameters")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .map(|p| KvItem {
                            key: p
                                .get("Name")
                                .and_then(Json::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            kind: p.get("Type").and_then(Json::as_str).map(str::to_owned),
                            description: p
                                .get("Description")
                                .and_then(Json::as_str)
                                .map(str::to_owned),
                            modified_ms: epoch_ms(p.get("LastModifiedDate")),
                            etag: p
                                .get("Version")
                                .and_then(Json::as_i64)
                                .map(|v| v.to_string()),
                            ..KvItem::default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(KvPage {
                items,
                next: json
                    .get("NextToken")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            })
        })
    }

    fn get<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async move {
            let json = self
                .api
                .call(
                    "GetParameter",
                    json!({ "Name": key, "WithDecryption": true }),
                )
                .await?;
            let p = json.get("Parameter").cloned().unwrap_or(Json::Null);
            Ok(KvItem {
                key: key.to_owned(),
                value: p.get("Value").and_then(Json::as_str).map(str::to_owned),
                kind: p.get("Type").and_then(Json::as_str).map(str::to_owned),
                modified_ms: epoch_ms(p.get("LastModifiedDate")),
                etag: p
                    .get("Version")
                    .and_then(Json::as_i64)
                    .map(|v| v.to_string()),
                ..KvItem::default()
            })
        })
    }

    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut body = json!({
                "Name": w.key,
                "Value": w.value,
                "Overwrite": !w.create,
            });
            // The type can only be given when creating (or it must match).
            if let Some(k) = w.kind.as_ref().filter(|k| !k.is_empty()) {
                body["Type"] = json!(k);
            } else if w.create {
                body["Type"] = json!("String");
            }
            if let Some(d) = w.description.as_ref().filter(|d| !d.is_empty()) {
                body["Description"] = json!(d);
            }
            self.api.call("PutParameter", body).await?;
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.api
                .call("DeleteParameter", json!({ "Name": key }))
                .await?;
            Ok(())
        })
    }
}
