use super::Exchange;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Changed,
    Indeterminate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub kind: DiffKind,
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct ExchangeDiff {
    pub entries: Vec<DiffEntry>,
}

pub fn diff_exchanges(before: &Exchange, after: &Exchange) -> ExchangeDiff {
    let mut entries = Vec::new();
    match (&before.response, &after.response) {
        (Some(before), Some(after)) => {
            changed(&mut entries, "status", before.status, after.status);
            changed(&mut entries, "reason", &before.reason, &after.reason);
            changed(
                &mut entries,
                "duration_ms",
                before.duration_ms,
                after.duration_ms,
            );
            changed(
                &mut entries,
                "final_url",
                &before.final_url,
                &after.final_url,
            );
            changed(
                &mut entries,
                "http_version",
                &before.http_version,
                &after.http_version,
            );
            changed(&mut entries, "truncated", before.truncated, after.truncated);
            changed(
                &mut entries,
                "received_bytes",
                before.received_bytes,
                after.received_bytes,
            );
            changed(
                &mut entries,
                "stored_bytes",
                before.stored_bytes,
                after.stored_bytes,
            );
            changed(&mut entries, "sensitive", before.sensitive, after.sensitive);
            diff_serializable(
                &mut entries,
                "body_omitted_reason",
                &before.body_omitted_reason,
                &after.body_omitted_reason,
            );
            if before.full_body_sha256 != after.full_body_sha256 {
                entries.push(DiffEntry {
                    kind: DiffKind::Changed,
                    path: "body.full_sha256".into(),
                    before: before.full_body_sha256.clone(),
                    after: after.full_body_sha256.clone(),
                });
            } else if before.full_body_sha256.is_none() && (before.truncated || after.truncated) {
                entries.push(DiffEntry {
                    kind: DiffKind::Indeterminate,
                    path: "body.identity".into(),
                    before: before
                        .truncated
                        .then(|| "unknown beyond retained prefix".into()),
                    after: after
                        .truncated
                        .then(|| "unknown beyond retained prefix".into()),
                });
            }
            diff_serializable(
                &mut entries,
                "redirects",
                &before.redirects,
                &after.redirects,
            );
            diff_serializable(&mut entries, "timings", &before.timings, &after.timings);
            diff_serializable(&mut entries, "cookies", &before.cookies, &after.cookies);
            diff_serializable(
                &mut entries,
                "tests",
                &before.test_results,
                &after.test_results,
            );
            diff_headers(&mut entries, &before.headers, &after.headers);
            let before_body = base64::engine::general_purpose::STANDARD
                .decode(&before.body_base64)
                .unwrap_or_default();
            let after_body = base64::engine::general_purpose::STANDARD
                .decode(&after.body_base64)
                .unwrap_or_default();
            diff_body(
                &mut entries,
                &before_body,
                &after_body,
                response_body_is_binary(before) || response_body_is_binary(after),
            );
        }
        (None, Some(_)) => entries.push(DiffEntry {
            kind: DiffKind::Added,
            path: "response".into(),
            before: None,
            after: Some("response received".into()),
        }),
        (Some(_), None) => entries.push(DiffEntry {
            kind: DiffKind::Removed,
            path: "response".into(),
            before: Some("response received".into()),
            after: None,
        }),
        (None, None) => {}
    }
    if before.error != after.error {
        entries.push(DiffEntry {
            kind: DiffKind::Changed,
            path: "error".into(),
            before: before.error.clone(),
            after: after.error.clone(),
        });
    }
    ExchangeDiff { entries }
}

fn diff_serializable<T: Serialize>(
    entries: &mut Vec<DiffEntry>,
    path: &str,
    before: &T,
    after: &T,
) {
    let before = serde_json::to_value(before).unwrap_or(Value::Null);
    let after = serde_json::to_value(after).unwrap_or(Value::Null);
    diff_json(entries, path, Some(&before), Some(&after));
}

fn changed<T: ToString + PartialEq>(entries: &mut Vec<DiffEntry>, path: &str, before: T, after: T) {
    if before != after {
        entries.push(DiffEntry {
            kind: DiffKind::Changed,
            path: path.into(),
            before: Some(before.to_string()),
            after: Some(after.to_string()),
        });
    }
}

fn diff_headers(
    entries: &mut Vec<DiffEntry>,
    before: &[(String, String)],
    after: &[(String, String)],
) {
    let names: BTreeSet<String> = before
        .iter()
        .chain(after)
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    for name in names {
        let before_values = header_values(before, &name);
        let after_values = header_values(after, &name);
        if before_values != after_values {
            entries.push(DiffEntry {
                kind: match (before_values.is_empty(), after_values.is_empty()) {
                    (true, false) => DiffKind::Added,
                    (false, true) => DiffKind::Removed,
                    _ => DiffKind::Changed,
                },
                path: format!("headers.{name}"),
                before: (!before_values.is_empty()).then(|| before_values.join(", ")),
                after: (!after_values.is_empty()).then(|| after_values.join(", ")),
            });
        }
    }
}

fn header_values<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

fn response_body_is_binary(response: &super::ResponseSnapshot) -> bool {
    header_values(&response.headers, "content-type")
        .into_iter()
        .any(|content_type| {
            let content_type = content_type.to_ascii_lowercase();
            content_type.starts_with("image/")
                || content_type.starts_with("audio/")
                || content_type.starts_with("video/")
                || [
                    "application/octet-stream",
                    "application/pdf",
                    "application/zip",
                    "application/gzip",
                ]
                .iter()
                .any(|binary| content_type.starts_with(binary))
        })
}

fn diff_body(entries: &mut Vec<DiffEntry>, before: &[u8], after: &[u8], force_binary: bool) {
    if before == after {
        return;
    }
    if force_binary {
        diff_binary(entries, before, after);
        return;
    }
    if let (Ok(before_json), Ok(after_json)) = (
        serde_json::from_slice::<Value>(before),
        serde_json::from_slice::<Value>(after),
    ) {
        diff_json(entries, "body", Some(&before_json), Some(&after_json));
        return;
    }
    match (std::str::from_utf8(before), std::str::from_utf8(after)) {
        (Ok(before), Ok(after)) => {
            let before_lines: Vec<&str> = before.lines().collect();
            let after_lines: Vec<&str> = after.lines().collect();
            let max = before_lines.len().max(after_lines.len());
            for index in 0..max {
                if before_lines.get(index) != after_lines.get(index) {
                    entries.push(DiffEntry {
                        kind: match (before_lines.get(index), after_lines.get(index)) {
                            (None, Some(_)) => DiffKind::Added,
                            (Some(_), None) => DiffKind::Removed,
                            _ => DiffKind::Changed,
                        },
                        path: format!("body.line[{}]", index + 1),
                        before: before_lines.get(index).map(|value| (*value).into()),
                        after: after_lines.get(index).map(|value| (*value).into()),
                    });
                }
            }
        }
        _ => diff_binary(entries, before, after),
    }
}

fn diff_binary(entries: &mut Vec<DiffEntry>, before: &[u8], after: &[u8]) {
    changed(entries, "body.binary.bytes", before.len(), after.len());
    entries.push(DiffEntry {
        kind: DiffKind::Changed,
        path: "body.binary.sha256".into(),
        before: Some(format!("sha256:{}", hex::encode(Sha256::digest(before)))),
        after: Some(format!("sha256:{}", hex::encode(Sha256::digest(after)))),
    });
}

fn diff_json(
    entries: &mut Vec<DiffEntry>,
    path: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) {
    match (before, after) {
        (Some(Value::Object(before)), Some(Value::Object(after))) => {
            let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            for key in keys {
                diff_json(
                    entries,
                    &format!("{path}.{key}"),
                    before.get(key),
                    after.get(key),
                );
            }
        }
        (Some(Value::Array(before)), Some(Value::Array(after))) => {
            for index in 0..before.len().max(after.len()) {
                diff_json(
                    entries,
                    &format!("{path}[{index}]"),
                    before.get(index),
                    after.get(index),
                );
            }
        }
        (Some(before), Some(after)) if before != after => entries.push(DiffEntry {
            kind: DiffKind::Changed,
            path: path.into(),
            before: Some(before.to_string()),
            after: Some(after.to_string()),
        }),
        (None, Some(after)) => entries.push(DiffEntry {
            kind: DiffKind::Added,
            path: path.into(),
            before: None,
            after: Some(after.to_string()),
        }),
        (Some(before), None) => entries.push(DiffEntry {
            kind: DiffKind::Removed,
            path: path.into(),
            before: Some(before.to_string()),
            after: None,
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExchangeId, RedactedRequestSnapshot, RedirectSnapshot, ResponseCookie, ResponseSnapshot,
        ResponseTimings, TestResult, WorkspaceId,
    };

    fn exchange(status: u16, json: &str) -> Exchange {
        Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: ExchangeId::new(),
            workspace_id: WorkspaceId::new("project").unwrap(),
            request_id: None,
            request: RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://example.test".into(),
                headers: Vec::new(),
                body: String::new(),
                ..RedactedRequestSnapshot::default()
            },
            response: Some(ResponseSnapshot {
                status,
                reason: String::new(),
                headers: vec![("content-type".into(), "application/json".into())],
                body_base64: base64::engine::general_purpose::STANDARD.encode(json),
                duration_ms: 10,
                truncated: false,
                ..ResponseSnapshot::default()
            }),
            error: None,
            started_at: 0,
            completed_at: 1,
        }
    }

    #[test]
    fn reports_semantic_json_paths_instead_of_whole_body() {
        let diff = diff_exchanges(
            &exchange(200, r#"{"items":[{"id":1,"name":"one"}]}"#),
            &exchange(201, r#"{"items":[{"id":1,"name":"two"}],"next":true}"#),
        );
        assert!(diff.entries.iter().any(|entry| entry.path == "status"));
        assert!(
            diff.entries
                .iter()
                .any(|entry| entry.path == "body.items[0].name")
        );
        assert!(diff.entries.iter().any(|entry| entry.path == "body.next"));
    }

    #[test]
    fn reports_response_transport_metadata_cookies_and_tests() {
        let before = exchange(200, "ok");
        let mut after = before.clone();
        let response = after.response.as_mut().unwrap();
        response.final_url = "https://example.test/final".into();
        response.http_version = "HTTP/2".into();
        response.redirects = vec![RedirectSnapshot {
            status: 302,
            from_url: "https://example.test".into(),
            to_url: "https://example.test/final".into(),
            method: "GET".into(),
        }];
        response.truncated = true;
        response.received_bytes = 4096;
        response.stored_bytes = 1024;
        response.body_omitted_reason = Some("policy".into());
        response.timings = ResponseTimings {
            dns_ms: Some(1),
            connect_ms: Some(2),
            tls_ms: Some(3),
            first_byte_ms: Some(4),
            download_ms: Some(5),
        };
        response.cookies = vec![ResponseCookie {
            name: "sid".into(),
            domain: "example.test".into(),
            secure: true,
            ..ResponseCookie::default()
        }];
        response.test_results = vec![TestResult {
            name: "status is 200".into(),
            passed: true,
            skipped: false,
            error: None,
        }];

        let paths: BTreeSet<String> = diff_exchanges(&before, &after)
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        for expected in [
            "final_url",
            "http_version",
            "redirects[0]",
            "truncated",
            "received_bytes",
            "stored_bytes",
            "body_omitted_reason",
            "timings.dns_ms",
            "cookies[0]",
            "tests[0]",
        ] {
            assert!(
                paths
                    .iter()
                    .any(|path| path == expected || path.starts_with(&format!("{expected}."))),
                "missing semantic diff for {expected}: {paths:?}"
            );
        }
    }

    #[test]
    fn binary_body_diff_includes_length_and_sha256() {
        let mut before = exchange(200, "ignored");
        let mut after = before.clone();
        before.response.as_mut().unwrap().headers =
            vec![("content-type".into(), "application/octet-stream".into())];
        after.response.as_mut().unwrap().headers =
            before.response.as_ref().unwrap().headers.clone();
        before.response.as_mut().unwrap().body_base64 =
            base64::engine::general_purpose::STANDARD.encode(b"AB");
        after.response.as_mut().unwrap().body_base64 =
            base64::engine::general_purpose::STANDARD.encode(b"ABC");

        let diff = diff_exchanges(&before, &after);
        let hash = diff
            .entries
            .iter()
            .find(|entry| entry.path == "body.binary.sha256")
            .expect("binary bodies should be identified by content hash");
        assert!(hash.before.as_deref().unwrap().starts_with("sha256:"));
        assert!(hash.after.as_deref().unwrap().starts_with("sha256:"));
        assert_ne!(hash.before, hash.after);
        assert!(
            diff.entries
                .iter()
                .any(|entry| entry.path == "body.binary.bytes")
        );
    }

    #[test]
    fn omitted_bodies_are_distinguished_by_their_full_payload_digest() {
        let mut before = exchange(200, "");
        let mut after = before.clone();
        let before_response = before.response.as_mut().unwrap();
        before_response.body_base64.clear();
        before_response.body_omitted_reason = Some("binary".into());
        before_response.full_body_sha256 = Some("a".repeat(64));
        let after_response = after.response.as_mut().unwrap();
        after_response.body_base64.clear();
        after_response.body_omitted_reason = Some("binary".into());
        after_response.full_body_sha256 = Some("b".repeat(64));

        let diff = diff_exchanges(&before, &after);
        let digest = diff
            .entries
            .iter()
            .find(|entry| entry.path == "body.full_sha256")
            .expect("different omitted payloads must not compare equal");
        assert_eq!(
            digest.before.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            digest.after.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn matching_truncated_prefixes_have_indeterminate_payload_identity() {
        let mut before = exchange(200, "same retained prefix");
        let mut after = before.clone();
        for response in [
            before.response.as_mut().unwrap(),
            after.response.as_mut().unwrap(),
        ] {
            response.truncated = true;
            response.received_bytes = response.stored_bytes.saturating_add(1);
            response.full_body_sha256 = None;
        }

        let diff = diff_exchanges(&before, &after);
        let identity = diff
            .entries
            .iter()
            .find(|entry| entry.path == "body.identity")
            .expect("unread suffixes must not compare as equal payloads");
        assert_eq!(identity.kind, DiffKind::Indeterminate);
        assert_eq!(
            identity.before.as_deref(),
            Some("unknown beyond retained prefix")
        );
        assert_eq!(
            identity.after.as_deref(),
            Some("unknown beyond retained prefix")
        );
    }
}
