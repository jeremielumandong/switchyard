//! Cloud log sources, read-only: AWS CloudWatch Logs (log groups, `FilterLogEvents`) and
//! Azure Monitor Log Analytics (KQL through the query API). The log viewer in `core`
//! polls these for a live tail.

use std::sync::Arc;

use reqwest::Method;
use secrecy::ExposeSecret as _;
use serde_json::{Value as Json, json};

use crate::auth::TokenSource;
use crate::aws::{AwsAuth, AwsJson};
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client};

/// The Log Analytics query API's scope.
pub const LOG_ANALYTICS_SCOPE: &str = "https://api.loganalytics.io/.default";
const LOG_ANALYTICS: &str = "https://api.loganalytics.io";

/// A CloudWatch log group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogGroup {
    /// Name (`/aws/lambda/x`).
    pub name: String,
    /// Stored bytes.
    pub stored_bytes: u64,
    /// Retention in days (`None`: never expire).
    pub retention_days: Option<u64>,
}

/// One CloudWatch log event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CwEvent {
    /// Event id (unique within the group).
    pub id: String,
    /// When it happened, ms since the epoch.
    pub ts_ms: i64,
    /// Log stream.
    pub stream: String,
    /// Message.
    pub message: String,
}

/// A page of [`CwEvent`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CwPage {
    /// Events, oldest first.
    pub events: Vec<CwEvent>,
    /// Token for the next page.
    pub next: Option<String>,
}

/// Groups in a `DescribeLogGroups` reply, and its `nextToken`.
pub fn parse_log_groups(json: &Json) -> (Vec<LogGroup>, Option<String>) {
    let groups = json
        .get("logGroups")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| {
            Some(LogGroup {
                name: g.get("logGroupName")?.as_str()?.to_owned(),
                stored_bytes: g.get("storedBytes").and_then(Json::as_u64).unwrap_or(0),
                retention_days: g.get("retentionInDays").and_then(Json::as_u64),
            })
        })
        .collect();
    (groups, next_token(json))
}

fn next_token(json: &Json) -> Option<String> {
    json.get("nextToken")
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Events in a `FilterLogEvents` reply.
pub fn parse_events(json: &Json) -> CwPage {
    let mut events: Vec<CwEvent> = json
        .get("events")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(CwEvent {
                id: e
                    .get("eventId")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                ts_ms: e.get("timestamp").and_then(Json::as_i64)?,
                stream: e
                    .get("logStreamName")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                message: e
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .trim_end_matches(['\n', '\r'])
                    .to_owned(),
            })
        })
        .collect();
    events.sort_by_key(|e| e.ts_ms);
    CwPage {
        events,
        next: next_token(json),
    }
}

/// CloudWatch Logs for one account and region.
pub struct CloudWatchLogs {
    api: AwsJson,
}

impl CloudWatchLogs {
    /// A client; `endpoint` overrides `https://logs.<region>.amazonaws.com` (tests).
    pub fn new(auth: Arc<AwsAuth>, region: &str, endpoint: Option<url::Url>) -> Result<Self> {
        Ok(Self {
            api: AwsJson::new(auth, region, "logs", "Logs_20140328", endpoint, true)?,
        })
    }

    /// Log groups whose names start with `prefix` (up to 1,000).
    pub async fn groups(&self, prefix: &str) -> Result<Vec<LogGroup>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..20 {
            let mut body = json!({ "limit": 50 });
            if !prefix.trim().is_empty() {
                body["logGroupNamePrefix"] = Json::String(prefix.trim().to_owned());
            }
            if let Some(t) = &token {
                body["nextToken"] = Json::String(t.clone());
            }
            let (groups, next) = parse_log_groups(&self.api.call("DescribeLogGroups", body).await?);
            out.extend(groups);
            token = next;
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// One page of events in `group` from `start_ms` (inclusive) matching `pattern`
    /// (CloudWatch filter syntax; empty: every event).
    pub async fn filter(
        &self,
        group: &str,
        pattern: &str,
        start_ms: i64,
        end_ms: Option<i64>,
        token: Option<&str>,
    ) -> Result<CwPage> {
        let mut body = json!({
            "logGroupName": group,
            "startTime": start_ms,
            "limit": 1000,
        });
        if !pattern.trim().is_empty() {
            body["filterPattern"] = Json::String(pattern.trim().to_owned());
        }
        if let Some(e) = end_ms {
            body["endTime"] = json!(e);
        }
        if let Some(t) = token {
            body["nextToken"] = Json::String(t.to_owned());
        }
        Ok(parse_events(&self.api.call("FilterLogEvents", body).await?))
    }
}

/// A Log Analytics result table: column names and rows of values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaTable {
    /// Column names.
    pub columns: Vec<String>,
    /// Rows.
    pub rows: Vec<Vec<Json>>,
}

impl LaTable {
    /// Index of the first column named one of `names` (case-insensitive).
    pub fn column(&self, names: &[&str]) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n)))
    }
}

/// The first table of a query reply.
pub fn parse_la(json: &Json) -> Result<LaTable> {
    if let Some(e) = json.get("error") {
        let msg = e
            .pointer("/innererror/innererror/message")
            .or_else(|| e.pointer("/innererror/message"))
            .or_else(|| e.get("message"))
            .and_then(Json::as_str)
            .unwrap_or("the query failed");
        return Err(CloudError::api(400, None, msg));
    }
    let Some(t) = json.pointer("/tables/0") else {
        return Ok(LaTable::default());
    };
    Ok(LaTable {
        columns: t
            .get("columns")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("name").and_then(Json::as_str).map(str::to_owned))
            .collect(),
        rows: t
            .get("rows")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_array().cloned())
            .collect(),
    })
}

/// The Log Analytics query API.
pub struct LogAnalytics {
    token: Arc<dyn TokenSource>,
    http: reqwest::Client,
    base: url::Url,
}

impl LogAnalytics {
    /// A client with tokens for [`LOG_ANALYTICS_SCOPE`]; `base` overrides the API (tests).
    pub fn new(token: Arc<dyn TokenSource>, base: Option<url::Url>) -> Result<Self> {
        let base = match base {
            Some(b) => b,
            None => {
                url::Url::parse(LOG_ANALYTICS).map_err(|e| CloudError::Invalid(e.to_string()))?
            }
        };
        Ok(Self {
            token,
            http: client()?,
            base,
        })
    }

    /// Run `kql` in workspace `workspace_id` over `timespan` (ISO 8601: `PT1H`, or
    /// `start/end`).
    pub async fn query(&self, workspace_id: &str, kql: &str, timespan: &str) -> Result<LaTable> {
        let body = json!({ "query": kql, "timespan": timespan });
        let mut req = Req::new(
            Method::POST,
            &self.base,
            format!(
                "/v1/workspaces/{}/query",
                crate::http::uri_encode(workspace_id.trim(), false)
            ),
        )
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| CloudError::Invalid(e.to_string()))?);
        let t = self.token.token().await?;
        req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
        let resp = Resp::read(req.send(&self.http).await?).await?;
        let json = resp.json().unwrap_or(Json::Null);
        if !resp.ok() {
            let msg = json
                .pointer("/error/innererror/message")
                .or_else(|| json.pointer("/error/message"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            return Err(CloudError::api(resp.status, None, msg));
        }
        parse_la(&json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudwatch_replies() {
        let (g, next) = parse_log_groups(
            &serde_json::from_str(
                r#"{"logGroups":[{"logGroupName":"/aws/lambda/api","storedBytes":1024,"retentionInDays":14},{"logGroupName":"app"}],"nextToken":"t2"}"#,
            )
            .unwrap(),
        );
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].retention_days, Some(14));
        assert_eq!(g[1].retention_days, None);
        assert_eq!(next.as_deref(), Some("t2"));
        let page = parse_events(
            &serde_json::from_str(
                r#"{"events":[{"logStreamName":"s1","timestamp":2000,"message":"second\n","ingestionTime":2001,"eventId":"e2"},{"logStreamName":"s1","timestamp":1000,"message":"first","eventId":"e1"}],"searchedLogStreams":[]}"#,
            )
            .unwrap(),
        );
        assert_eq!(page.events[0].id, "e1");
        assert_eq!(page.events[1].message, "second");
        assert!(page.next.is_none());
    }

    #[test]
    fn log_analytics_reply() {
        let t = parse_la(
            &serde_json::from_str(
                r#"{"tables":[{"name":"PrimaryResult","columns":[{"name":"TimeGenerated","type":"datetime"},{"name":"Message","type":"string"}],"rows":[["2026-10-12T10:00:00Z","hello"]]}]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(t.column(&["timegenerated"]), Some(0));
        assert_eq!(t.rows[0][1], "hello");
        let e = parse_la(
            &serde_json::from_str(
                r#"{"error":{"message":"The request had some invalid properties","code":"BadArgumentError","innererror":{"code":"SyntaxError","message":"Query could not be parsed at 'x'"}}}"#,
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("could not be parsed"));
    }
}
