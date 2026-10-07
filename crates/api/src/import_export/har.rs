use super::*;

pub(super) fn collect_credentials(value: &Value, output: &mut Vec<String>) {
    for entry in value
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for source in [entry.get("request"), entry.get("response")]
            .into_iter()
            .flatten()
        {
            for cookie in source
                .get("cookies")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(value) = cookie.get("value").and_then(Value::as_str) {
                    push_credential(output, value);
                }
            }
        }
        if let Some(url) = entry
            .pointer("/request/url")
            .and_then(Value::as_str)
            .and_then(|s| url::Url::parse(s).ok())
        {
            if let Some(password) = url.password() {
                push_credential(output, password);
            }
            for (key, value) in url.query_pairs() {
                if sensitive_credential_field(&key) {
                    push_credential(output, &value);
                }
            }
        }
    }
}

/// Import captures as editable requests. Responses and browser metadata are not retained.
pub(super) fn import_har(workspace: &WorkspaceId, value: Value) -> Result<ImportResult, String> {
    let entries = value
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .ok_or("HAR must contain a log.entries array")?;
    if entries.len() > MAX_IMPORTED_REQUESTS {
        return Err(format!("HAR exceeds {MAX_IMPORTED_REQUESTS} requests"));
    }
    let collection = new_collection(workspace, "Imported HAR", "");
    let mut requests = Vec::with_capacity(entries.len());
    let mut warnings = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let owner = format!("HAR entry {}", index + 1);
        let source = entry
            .get("request")
            .filter(|v| v.is_object())
            .ok_or_else(|| format!("{owner} must contain a request object"))?;
        let method = required_string(source, "method", &owner)?;
        let address = required_string(source, "url", &owner)?;
        let parsed = url::Url::parse(address).map_err(|_| format!("{owner} has an invalid URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(format!("{owner} URL must use HTTP or HTTPS"));
        }
        let mut request = empty_request(
            &collection.id,
            format!("{method} {}", parsed.path()),
            method,
            address,
        )?;
        request.sort_key = index as i64;
        request.headers = rows(source, "headers", &owner)?;
        // These belong to the transport and must be recomputed for a replay.
        request.headers.retain(|row| !transport_header(&row.key));
        let query = rows(source, "queryString", &owner)?;
        // HAR repeats the URL query in queryString. Keep the original URL spelling
        // and append only occurrences absent from it, including repeated values.
        let mut represented = BTreeMap::<(String, String), usize>::new();
        for (key, value) in parsed.query_pairs() {
            *represented
                .entry((key.into_owned(), value.into_owned()))
                .or_default() += 1;
        }
        for row in query {
            if let Some(count) = represented
                .get_mut(&(row.key.clone(), row.value.clone()))
                .filter(|count| **count > 0)
            {
                *count -= 1;
            } else {
                request.params.push(row);
            }
        }
        if let Some(post) = source.get("postData") {
            if !post.is_object() {
                return Err(format!("{owner} postData must be an object"));
            }
            request.body = body(post, &owner, &mut warnings)?;
            if matches!(request.body, Body::Multipart { .. }) {
                // The new multipart encoder owns the boundary.
                request
                    .headers
                    .retain(|row| !row.key.eq_ignore_ascii_case("content-type"));
            } else if !matches!(request.body, Body::None)
                && !request
                    .headers
                    .iter()
                    .any(|row| row.key.eq_ignore_ascii_case("content-type"))
            {
                request.headers.push(KeyValueRow::enabled(
                    "Content-Type",
                    required_string(post, "mimeType", &owner)?,
                ));
            }
        }
        requests.push(request);
    }
    Ok(ImportResult {
        format: ImportFormat::Har,
        origin: None,
        collection,
        folders: Vec::new(),
        requests,
        environments: Vec::new(),
        examples: Vec::new(),
        warnings,
    })
}

fn required_string<'a>(value: &'a Value, key: &str, owner: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{owner} requires a nonempty {key} string"))
}

fn rows(value: &Value, key: &str, owner: &str) -> Result<Vec<KeyValueRow>, String> {
    let Some(value) = value.get(key) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| format!("{owner} {key} must be an array"))?
        .iter()
        .map(|row| {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{owner} {key} row requires a name string"))?;
            let value = row
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{owner} {key} row requires a value string"))?;
            Ok(KeyValueRow::enabled(name, value))
        })
        .collect()
}

fn transport_header(name: &str) -> bool {
    name.starts_with(':')
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "accept-encoding"
                | "keep-alive"
                | "proxy-connection"
                | "te"
                | "trailer"
                | "upgrade"
        )
}

fn body(post: &Value, owner: &str, warnings: &mut Vec<String>) -> Result<Body, String> {
    let mime = required_string(post, "mimeType", owner)?;
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    let text = match post.get("text") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| format!("{owner} postData.text must be a string"))?,
        ),
        None => None,
    };
    if mime == "multipart/form-data" {
        let mut output = Vec::new();
        if let Some(params) = post.get("params") {
            for param in params
                .as_array()
                .ok_or_else(|| format!("{owner} postData.params must be an array"))?
            {
                let name = required_string(param, "name", owner)?;
                if param.get("fileName").is_some() {
                    warnings.push(format!(
                        "{owner}: omitted a multipart file; select the file again before sending."
                    ));
                    continue;
                }
                let value = param
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{owner} multipart field requires a value string"))?;
                output.push(MultipartRow {
                    id: RowId::new(),
                    key: name.into(),
                    value: MultipartValue::Text(value.into()),
                    enabled: true,
                    description: String::new(),
                });
            }
        } else {
            warnings.push(format!(
                "{owner}: multipart capture has no structured fields; body omitted."
            ));
        }
        return Ok(Body::Multipart { rows: output });
    }
    if mime == "application/x-www-form-urlencoded" {
        let rows = if let Some(text) =
            text.filter(|text| !text.is_empty() || post.get("params").is_none())
        {
            url::form_urlencoded::parse(text.as_bytes())
                .map(|(k, v)| KeyValueRow::enabled(k, v))
                .collect()
        } else {
            rows(post, "params", owner)?
        };
        return Ok(Body::UrlEncoded { rows });
    }
    let Some(text) = text else {
        warnings.push(format!(
            "{owner}: captured body contents are unavailable; body omitted."
        ));
        return Ok(Body::None);
    };
    let media_type = if mime == "application/json" || mime.ends_with("+json") {
        RawBodyKind::Json
    } else if mime.ends_with("/xml") || mime.ends_with("+xml") {
        RawBodyKind::Xml
    } else {
        RawBodyKind::Text
    };
    Ok(Body::Raw {
        media_type,
        text: text.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(request: Value) -> Value {
        json!({"log":{"version":"1.2","entries":[{"request":request}]}})
    }
    fn parse(value: Value) -> Result<ImportResult, String> {
        import(
            &WorkspaceId::new("har-tests").unwrap(),
            &serde_json::to_vec(&value).unwrap(),
        )
    }

    #[test]
    fn har_preserves_repeated_rows_and_static_requests_without_duplicating_query() {
        let result = parse(capture(json!({"method":"GET","url":"https://example.test/app.js?tag=a&tag=a",
            "queryString":[{"name":"tag","value":"a"},{"name":"tag","value":"a"},{"name":"tag","value":"b"}],
            "headers":[{"name":"X-Repeat","value":"one"},{"name":"X-Repeat","value":"two"},{"name":"Host","value":"old.test"},{"name":":authority","value":"old.test"}]}))).unwrap();
        assert_eq!(result.format, ImportFormat::Har);
        let request = &result.requests[0];
        assert_eq!(request.url, "https://example.test/app.js?tag=a&tag=a");
        assert_eq!(request.params.len(), 1);
        assert_eq!(request.params[0].value, "b");
        assert_eq!(request.headers.len(), 2);
        assert_eq!(request.headers[1].value, "two");
    }

    #[test]
    fn har_imports_form_fields_without_turning_captured_filenames_into_local_reads() {
        let result = parse(capture(json!({"method":"POST","url":"https://example.test/upload",
            "headers":[{"name":"Content-Type","value":"multipart/form-data; boundary=old"}],
            "postData":{"mimeType":"multipart/form-data","params":[{"name":"label","value":"one"},{"name":"label","value":"two"},{"name":"file","fileName":"C:/private/key","value":"omitted"}]}}))).unwrap();
        assert!(result.requests[0].headers.is_empty());
        let Body::Multipart { rows } = &result.requests[0].body else {
            panic!("multipart expected")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(result.warnings.len(), 1);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("C:/private")
        );
        let result = parse(capture(json!({"method":"POST","url":"https://example.test/form", "postData":{"mimeType":"application/x-www-form-urlencoded","params":[{"name":"x","value":"1"},{"name":"x","value":"2"}]}}))).unwrap();
        let Body::UrlEncoded { rows } = &result.requests[0].body else {
            panic!("form expected")
        };
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn har_redacts_credentials_from_copies_in_safe_fields() {
        let result = parse(capture(json!({"method":"POST","url":"https://example.test/?copy=har-secret-938",
            "headers":[{"name":"Authorization","value":"Bearer har-secret-938"},{"name":"X-Copy","value":"har-secret-938"}],
            "queryString":[{"name":"api_key","value":"har-query-secret"}],
            "cookies":[{"name":"sid","value":"opaque-session-8492"}],
            "postData":{"mimeType":"application/json","text":"{\"copy\":\"har-query-secret\",\"sessionCopy\":\"opaque-session-8492\"}"}}))).unwrap();
        let text = serde_json::to_string(&result).unwrap();
        assert!(!text.contains("har-secret-938"), "{text}");
        assert!(!text.contains("har-query-secret"), "{text}");
        assert!(!text.contains("opaque-session-8492"), "{text}");
    }

    #[test]
    fn har_preserves_raw_media_types_and_request_order() {
        let entries: Vec<Value> = [("application/vnd.example+json", "{\"x\":1}"), ("application/xml", "<x/>"), ("text/csv", "a,b\n1,2")].into_iter().map(|(mime, text)| json!({"request":{"method":"POST","url":"https://example.test/", "postData":{"mimeType":mime,"text":text}}})).collect();
        let result = parse(json!({"log":{"entries":entries}})).unwrap();
        for (index, (request, expected)) in result
            .requests
            .iter()
            .zip([RawBodyKind::Json, RawBodyKind::Xml, RawBodyKind::Text])
            .enumerate()
        {
            assert_eq!(request.sort_key, index as i64);
            let Body::Raw { media_type, text } = &request.body else {
                panic!("raw body expected")
            };
            assert_eq!(*media_type, expected);
            assert!(!text.is_empty());
            assert_eq!(
                request.headers[0].value,
                [
                    "application/vnd.example+json",
                    "application/xml",
                    "text/csv"
                ][index]
            );
        }
        let result = parse(capture(json!({"method":"POST","url":"https://example.test/form", "postData":{"mimeType":"application/x-www-form-urlencoded","text":"","params":[{"name":"x","value":"1"}]}}))).unwrap();
        let Body::UrlEncoded { rows } = &result.requests[0].body else {
            panic!("form expected")
        };
        assert_eq!(rows[0].value, "1");
    }

    #[test]
    fn har_rejects_malformed_and_over_limit_captures() {
        for value in [
            json!({"log":{}}),
            capture(json!({"method":"GET"})),
            capture(json!({"method":"GET","url":"file:///private"})),
            capture(json!({"method":"GET","url":"https://example.test","headers":{}})),
        ] {
            assert!(parse(value).is_err());
        }
        let value = json!({"log":{"entries":vec![json!({}); MAX_IMPORTED_REQUESTS + 1]}});
        assert!(parse(value).unwrap_err().contains("exceeds"));
    }
}
