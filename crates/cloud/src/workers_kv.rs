//! Cloudflare Workers KV: namespaces, keys and values through the Cloudflare API.

use std::sync::Arc;

use futures::future::BoxFuture;
use reqwest::Method;
use serde_json::Value as Json;

use crate::cloudflare::CloudflareApi;
use crate::error::{CloudError, Result};
use crate::http::uri_encode;
use crate::kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite};

/// An account's Workers KV.
pub struct WorkersKv {
    api: Arc<CloudflareApi>,
    account: String,
    read_only: bool,
}

impl WorkersKv {
    /// Workers KV of `account` (its 32-character id).
    pub fn new(api: Arc<CloudflareApi>, account: &str, read_only: bool) -> Result<Self> {
        let account = account.trim().to_owned();
        if account.is_empty() {
            return Err(CloudError::Invalid(
                "the Cloudflare account id is required".into(),
            ));
        }
        Ok(Self {
            api,
            account,
            read_only,
        })
    }

    fn ns_path(&self, scope: Option<&str>) -> Result<String> {
        let ns = scope
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CloudError::Invalid("choose a namespace first".into()))?;
        Ok(format!(
            "/accounts/{}/storage/kv/namespaces/{}",
            uri_encode(&self.account, false),
            uri_encode(ns, false)
        ))
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            Err(CloudError::ReadOnly)
        } else {
            Ok(())
        }
    }
}

impl KvService for WorkersKv {
    fn caps(&self) -> KvCaps {
        KvCaps {
            scope_label: Some("Namespace"),
            filter_hint: "Key prefix",
            ..KvCaps::default()
        }
    }

    fn scopes(&self) -> BoxFuture<'_, Result<Vec<(String, String)>>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for page in 1..=50 {
                let json = self
                    .api
                    .send(
                        self.api
                            .req(
                                Method::GET,
                                format!(
                                    "/accounts/{}/storage/kv/namespaces",
                                    uri_encode(&self.account, false)
                                ),
                            )
                            .query("per_page", "100")
                            .query("page", page.to_string()),
                    )
                    .await?;
                let list = json
                    .get("result")
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                let n = list.len();
                out.extend(list.iter().filter_map(|n| {
                    Some((
                        n.get("id")?.as_str()?.to_owned(),
                        n.get("title")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    ))
                }));
                if n < 100 {
                    break;
                }
            }
            out.sort_by_key(|(_, t)| t.to_lowercase());
            Ok(out)
        })
    }

    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        Box::pin(async move {
            let mut req = self
                .api
                .req(
                    Method::GET,
                    format!("{}/keys", self.ns_path(q.scope.as_deref())?),
                )
                .query("limit", "1000");
            if !q.key.trim().is_empty() {
                req = req.query("prefix", q.key.trim());
            }
            if let Some(c) = &q.cursor {
                req = req.query("cursor", c.clone());
            }
            let json = self.api.send(req).await?;
            let items =
                json.get("result")
                    .and_then(Json::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|k| KvItem {
                                key: k
                                    .get("name")
                                    .and_then(Json::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                kind: k.get("expiration").and_then(Json::as_i64).map(|e| {
                                    format!("expires {}", crate::time::display_ms(e * 1000))
                                }),
                                description: k
                                    .get("metadata")
                                    .filter(|m| !m.is_null())
                                    .map(Json::to_string),
                                ..KvItem::default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            Ok(KvPage {
                items,
                next: json
                    .pointer("/result_info/cursor")
                    .and_then(Json::as_str)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned),
            })
        })
    }

    fn get<'a>(
        &'a self,
        scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async move {
            let resp = self
                .api
                .send_raw(self.api.req(
                    Method::GET,
                    format!("{}/values/{}", self.ns_path(scope)?, uri_encode(key, false)),
                ))
                .await?;
            let value = match String::from_utf8(resp.body) {
                Ok(s) => s,
                Err(e) => format!("(binary value, {} bytes: not shown)", e.into_bytes().len()),
            };
            Ok(KvItem {
                key: key.to_owned(),
                value: Some(value),
                ..KvItem::default()
            })
        })
    }

    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.writable()?;
            let path = format!(
                "{}/values/{}",
                self.ns_path(w.scope.as_deref())?,
                uri_encode(&w.key, false)
            );
            if w.create {
                let exists = self
                    .api
                    .send_raw(self.api.req(Method::GET, path.clone()))
                    .await;
                if exists.is_ok() {
                    return Err(CloudError::api(
                        409,
                        None,
                        format!("{} already exists", w.key),
                    ));
                }
            }
            self.api
                .send(
                    self.api
                        .req(Method::PUT, path)
                        .header("content-type", "text/plain; charset=utf-8")
                        .body(w.value.clone().into_bytes()),
                )
                .await?;
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.writable()?;
            self.api
                .send(self.api.req(
                    Method::DELETE,
                    format!("{}/values/{}", self.ns_path(scope)?, uri_encode(key, false)),
                ))
                .await?;
            Ok(())
        })
    }
}
