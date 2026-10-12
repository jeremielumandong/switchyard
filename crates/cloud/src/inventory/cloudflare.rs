//! Cloudflare inventory: R2 buckets and D1 databases of the token's accounts.

use reqwest::Method;
use serde_json::Value as Json;

use super::{CloudResource, DbFlavor, Provider, ResourceKind};
use crate::cloudflare::CloudflareApi;
use crate::error::Result;

/// `(id, name)` of the accounts in a `GET /accounts` reply.
pub fn parse_accounts(json: &Json) -> Vec<(String, String)> {
    json.get("result")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("id")?.as_str()?.to_owned(),
                        x.get("name")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// R2 buckets in a `GET /accounts/{id}/r2/buckets` reply.
pub fn parse_r2(json: &Json, account: &str, account_id: &str) -> Vec<CloudResource> {
    json.pointer("/result/buckets")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let name = b.get("name")?.as_str()?;
            Some(CloudResource {
                provider: Some(Provider::Cloudflare),
                account: account.to_owned(),
                region: b
                    .get("location")
                    .and_then(Json::as_str)
                    .unwrap_or("auto")
                    .to_owned(),
                kind: Some(ResourceKind::Storage),
                rtype: "R2 bucket".into(),
                id: format!("{account_id}/r2/{name}"),
                name: name.to_owned(),
                state: "available".into(),
                host: Some(format!("{account_id}.r2.cloudflarestorage.com")),
                ..CloudResource::default()
            })
        })
        .collect()
}

/// D1 databases in a `GET /accounts/{id}/d1/database` reply.
pub fn parse_d1(json: &Json, account: &str) -> Vec<CloudResource> {
    json.get("result")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?;
            Some(CloudResource {
                provider: Some(Provider::Cloudflare),
                account: account.to_owned(),
                region: d
                    .get("running_in_region")
                    .and_then(Json::as_str)
                    .unwrap_or("global")
                    .to_owned(),
                kind: Some(ResourceKind::Database),
                rtype: "D1 database".into(),
                id: d
                    .get("uuid")
                    .and_then(Json::as_str)
                    .unwrap_or(name)
                    .to_owned(),
                name: name.to_owned(),
                state: "available".into(),
                db: Some(DbFlavor::D1),
                ..CloudResource::default()
            })
        })
        .collect()
}

/// R2 buckets and D1 databases of `account_id` (or of every account the token sees).
pub async fn resources(
    api: &CloudflareApi,
    account_id: Option<&str>,
) -> Result<(Vec<CloudResource>, Vec<String>)> {
    let accounts = match account_id.map(str::trim).filter(|a| !a.is_empty()) {
        Some(id) => vec![(id.to_owned(), id.to_owned())],
        None => parse_accounts(&api.send(api.req(Method::GET, "/accounts")).await?),
    };
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (id, name) in accounts {
        let label = if name.is_empty() { id.clone() } else { name };
        match api
            .send(api.req(Method::GET, format!("/accounts/{id}/r2/buckets")))
            .await
        {
            Ok(j) => out.extend(parse_r2(&j, &label, &id)),
            Err(e) => errors.push(format!("{label}: R2: {e}")),
        }
        match api
            .send(api.req(Method::GET, format!("/accounts/{id}/d1/database")))
            .await
        {
            Ok(j) => out.extend(parse_d1(&j, &label)),
            Err(e) => errors.push(format!("{label}: D1: {e}")),
        }
    }
    Ok((out, errors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_and_databases() {
        let r2: Json = serde_json::from_str(
            r#"{"success":true,"result":{"buckets":[{"name":"assets","creation_date":"2024-01-01T00:00:00Z","location":"WEUR"}]}}"#,
        )
        .unwrap();
        let b = parse_r2(&r2, "Acme", "abc123");
        assert_eq!(b[0].name, "assets");
        assert_eq!(b[0].region, "WEUR");
        let d1: Json = serde_json::from_str(
            r#"{"success":true,"result":[{"uuid":"u-1","name":"prod-db","created_at":"2024-01-01T00:00:00Z","version":"production"}]}"#,
        )
        .unwrap();
        let d = parse_d1(&d1, "Acme");
        assert_eq!(d[0].id, "u-1");
        assert_eq!(d[0].db, Some(DbFlavor::D1));
        let acc: Json =
            serde_json::from_str(r#"{"result":[{"id":"abc123","name":"Acme"}]}"#).unwrap();
        assert_eq!(parse_accounts(&acc), [("abc123".into(), "Acme".into())]);
    }
}
