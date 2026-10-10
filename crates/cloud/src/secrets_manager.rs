//! AWS Secrets Manager. Deleting schedules deletion after a 7-day recovery window.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value as Json, json};

use crate::aws::{AwsAuth, AwsJson};
use crate::error::{CloudError, Result};
use crate::kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite};

/// A region's secrets.
pub struct SecretsManager {
    api: AwsJson,
}

impl SecretsManager {
    /// Secrets in `region` (`endpoint` overrides the AWS endpoint, for tests).
    pub fn new(
        auth: Arc<AwsAuth>,
        region: &str,
        endpoint: Option<url::Url>,
        read_only: bool,
    ) -> Result<Self> {
        Ok(Self {
            api: AwsJson::new(
                auth,
                region,
                "secretsmanager",
                "secretsmanager",
                endpoint,
                read_only,
            )?,
        })
    }
}

/// A fresh `ClientRequestToken` (a UUID, as the AWS SDKs send) naming the new version.
fn request_token() -> String {
    use ring::rand::{SecureRandom as _, SystemRandom};
    let mut b = [0u8; 16];
    // A failure leaves zeros; the token only has to differ from the previous version's.
    let _ = SystemRandom::new().fill(&mut b);
    let h = data_encoding::HEXLOWER.encode(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn epoch_ms(v: Option<&Json>) -> Option<i64> {
    v.and_then(Json::as_f64).map(|s| (s * 1000.0) as i64)
}

fn tags(v: Option<&Json>) -> std::collections::BTreeMap<String, String> {
    v.and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some((
                        t.get("Key")?.as_str()?.to_owned(),
                        t.get("Value")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl KvService for SecretsManager {
    fn caps(&self) -> KvCaps {
        KvCaps {
            tags: true,
            secret_values: true,
            filter_hint: "Name prefix",
            hierarchical: true,
            ..KvCaps::default()
        }
    }

    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        Box::pin(async move {
            let mut body = json!({ "MaxResults": 100, "SortOrder": "asc" });
            if !q.key.trim().is_empty() {
                body["Filters"] = json!([{ "Key": "name", "Values": [q.key.trim()] }]);
            }
            if let Some(c) = &q.cursor {
                body["NextToken"] = json!(c);
            }
            let json = self.api.call("ListSecrets", body).await?;
            let items = json
                .get("SecretList")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .map(|s| KvItem {
                            key: s
                                .get("Name")
                                .and_then(Json::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            description: s
                                .get("Description")
                                .and_then(Json::as_str)
                                .map(str::to_owned),
                            modified_ms: epoch_ms(s.get("LastChangedDate")),
                            tags: tags(s.get("Tags")),
                            kind: s
                                .get("DeletedDate")
                                .map(|_| "scheduled for deletion".to_owned()),
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
            let meta = self
                .api
                .call("DescribeSecret", json!({ "SecretId": key }))
                .await?;
            let value = match self
                .api
                .call("GetSecretValue", json!({ "SecretId": key }))
                .await
            {
                Ok(v) => match v.get("SecretString").and_then(Json::as_str) {
                    Some(s) => Some(s.to_owned()),
                    None if v.get("SecretBinary").is_some() => {
                        Some("(binary secret: not shown)".to_owned())
                    }
                    None => None,
                },
                // A secret created without a value yet.
                Err(e) if e.is_not_found() => None,
                Err(e) => return Err(e),
            };
            Ok(KvItem {
                key: key.to_owned(),
                value,
                description: meta
                    .get("Description")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
                modified_ms: epoch_ms(meta.get("LastChangedDate")),
                tags: tags(meta.get("Tags")),
                ..KvItem::default()
            })
        })
    }

    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if w.create {
                let mut body = json!({
                    "Name": w.key,
                    "SecretString": w.value,
                    "ClientRequestToken": request_token(),
                });
                if let Some(d) = w.description.as_ref().filter(|d| !d.is_empty()) {
                    body["Description"] = json!(d);
                }
                if !w.tags.is_empty() {
                    body["Tags"] = Json::Array(
                        w.tags
                            .iter()
                            .map(|(k, v)| json!({ "Key": k, "Value": v }))
                            .collect(),
                    );
                }
                self.api.call("CreateSecret", body).await?;
                return Ok(());
            }
            self.api
                .call(
                    "PutSecretValue",
                    json!({
                        "SecretId": w.key,
                        "SecretString": w.value,
                        "ClientRequestToken": request_token(),
                    }),
                )
                .await?;
            if let Some(d) = &w.description {
                self.api
                    .call(
                        "UpdateSecret",
                        json!({
                            "SecretId": w.key,
                            "Description": d,
                            "ClientRequestToken": request_token(),
                        }),
                    )
                    .await?;
            }
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
                .call(
                    "DeleteSecret",
                    json!({ "SecretId": key, "RecoveryWindowInDays": 7 }),
                )
                .await
                .map_err(|e| match e {
                    CloudError::Api {
                        status: 400,
                        code,
                        message,
                    } if code.as_deref() == Some("InvalidRequestException") => CloudError::Api {
                        status: 400,
                        code,
                        message: format!("{message} (already scheduled for deletion?)"),
                    },
                    e => e,
                })?;
            Ok(())
        })
    }
}
