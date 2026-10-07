use super::{RedactedRequestSnapshot, export_curl};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetLanguage {
    Curl,
    RawHttp,
    RustReqwest,
    JavaScriptFetch,
    TypeScriptFetch,
    PythonRequests,
    PowerShell,
    CSharpHttpClient,
}

pub fn generate_snippet(language: SnippetLanguage, request: &RedactedRequestSnapshot) -> String {
    match language {
        SnippetLanguage::Curl => export_curl(request),
        SnippetLanguage::RawHttp => raw_http(request),
        SnippetLanguage::RustReqwest => rust_reqwest(request),
        SnippetLanguage::JavaScriptFetch | SnippetLanguage::TypeScriptFetch => fetch(request),
        SnippetLanguage::PythonRequests => python(request),
        SnippetLanguage::PowerShell => powershell(request),
        SnippetLanguage::CSharpHttpClient => csharp(request),
    }
}

fn raw_http(request: &RedactedRequestSnapshot) -> String {
    let parsed = url::Url::parse(&request.url).ok();
    let path = parsed
        .as_ref()
        .map(|url| {
            let mut path = url.path().to_string();
            if let Some(query) = url.query() {
                path.push('?');
                path.push_str(query);
            }
            path
        })
        .unwrap_or_else(|| request.url.clone());
    let host = parsed
        .and_then(|url| {
            let mut host = url.host().map(|host| host.to_string())?;
            if let Some(port) = url.port() {
                host.push(':');
                host.push_str(&port.to_string());
            }
            Some(host)
        })
        .unwrap_or_default();
    let mut output = format!("{} {path} HTTP/1.1\r\n", request.method);
    if !request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        output.push_str(&format!("Host: {host}\r\n"));
    }
    for (name, value) in &request.headers {
        output.push_str(&format!("{name}: {value}\r\n"));
    }
    output.push_str("\r\n");
    output.push_str(&request.body);
    output
}

fn rust_reqwest(request: &RedactedRequestSnapshot) -> String {
    let mut output = format!(
        "let client = reqwest::Client::new();\nlet response = client\n    .request(reqwest::Method::from_bytes(b\"{}\")?, \"{}\")",
        rust_escape(&request.method),
        rust_escape(&request.url)
    );
    for (name, value) in &request.headers {
        output.push_str(&format!(
            "\n    .header(\"{}\", \"{}\")",
            rust_escape(name),
            rust_escape(value)
        ));
    }
    if !request.body.is_empty() {
        output.push_str(&format!("\n    .body(\"{}\")", rust_escape(&request.body)));
    }
    output.push_str("\n    .send()\n    .await?;");
    output
}

fn fetch(request: &RedactedRequestSnapshot) -> String {
    let mut headers = String::from("const headers = new Headers();\n");
    for (name, value) in &request.headers {
        headers.push_str(&format!(
            "headers.append({}, {});\n",
            json_str(name),
            json_str(value)
        ));
    }
    let body = if request.body.is_empty() {
        String::new()
    } else {
        format!(",\n  body: {}", json_str(&request.body))
    };
    format!(
        "{headers}\nconst response = await fetch({}, {{\n  method: {},\n  headers{body}\n}});",
        json_str(&request.url),
        json_str(&request.method)
    )
}

fn python(request: &RedactedRequestSnapshot) -> String {
    if has_duplicate_header_names(&request.headers) {
        let header_rows =
            serde_json::to_string_pretty(&request.headers).unwrap_or_else(|_| "[]".into());
        let body = if request.body.is_empty() {
            "None".into()
        } else {
            format!("{}.encode('utf-8')", json_str(&request.body))
        };
        return format!(
            "import http.client\nfrom urllib.parse import urlsplit\n\ntarget = urlsplit({})\nconnection_type = http.client.HTTPSConnection if target.scheme == \"https\" else http.client.HTTPConnection\nconnection = connection_type(target.hostname, target.port)\npath = target.path or \"/\"\nif target.query:\n    path += \"?\" + target.query\nbody = {body}\nheader_rows = {header_rows}\nheader_names = {{name.lower() for name, _ in header_rows}}\nconnection.putrequest({}, path)\nfor name, value in header_rows:\n    connection.putheader(name, value)\nif body is not None and \"content-length\" not in header_names and \"transfer-encoding\" not in header_names:\n    connection.putheader(\"Content-Length\", str(len(body)))\nconnection.endheaders(body)\nresponse = connection.getresponse()",
            json_str(&request.url),
            json_str(&request.method),
        );
    }
    let header_rows =
        serde_json::to_string_pretty(&request.headers).unwrap_or_else(|_| "[]".into());
    let data = if request.body.is_empty() {
        String::new()
    } else {
        format!(",\n    data={}", json_str(&request.body))
    };
    format!(
        "import requests\n\nheader_rows = {header_rows}\nheaders = {{}}\nfor name, value in header_rows:\n    headers[name] = f\"{{headers[name]}}, {{value}}\" if name in headers else value\n\nresponse = requests.request(\n    {},\n    {},\n    headers=headers{data}\n)",
        json_str(&request.method),
        json_str(&request.url)
    )
}

fn powershell(request: &RedactedRequestSnapshot) -> String {
    let mut output = format!(
        "$client = [System.Net.Http.HttpClient]::new()\n$request = [System.Net.Http.HttpRequestMessage]::new(\n    [System.Net.Http.HttpMethod]::new('{}'),\n    '{}')",
        ps_escape(&request.method),
        ps_escape(&request.url)
    );
    // `StringContent` would stamp `text/plain; charset=utf-8` on the request,
    // and a Content-Type row from the snapshot then *appends* to it rather
    // than replacing it: measured on the wire as
    // `Content-Type: text/plain; charset=utf-8, application/json`, which no
    // server reads as JSON. `ByteArrayContent` carries no type of its own, so
    // the snapshot's row is the only one. Same reasoning as [`csharp`].
    if !request.body.is_empty() {
        output.push_str(&format!(
            "\n$request.Content = [System.Net.Http.ByteArrayContent]::new([System.Text.Encoding]::UTF8.GetBytes('{}'))",
            ps_escape(&request.body)
        ));
    }
    // The fallback cannot be gated on there being a body: .NET refuses every
    // content header on `$request.Headers` and drops it silently, so a
    // bodyless Content-Type needs an empty content to hang on.
    for (index, (name, value)) in request.headers.iter().enumerate() {
        let name = ps_escape(name);
        let value = ps_escape(value);
        output.push_str(&format!(
            "\n$added{index} = $request.Headers.TryAddWithoutValidation('{name}', '{value}')\nif (-not $added{index}) {{\n    if ($null -eq $request.Content) {{ $request.Content = [System.Net.Http.ByteArrayContent]::new([byte[]]::new(0)) }}\n    [void]$request.Content.Headers.TryAddWithoutValidation('{name}', '{value}')\n}}"
        ));
    }
    output.push_str("\n$response = $client.SendAsync($request).GetAwaiter().GetResult()");
    output
}

fn csharp(request: &RedactedRequestSnapshot) -> String {
    let requires = if request.body.is_empty() {
        "System, System.Net.Http"
    } else {
        "System, System.Net.Http, System.Text"
    };
    let mut output = format!(
        "// Requires: {requires}\nusing var client = new HttpClient();\nusing var request = new HttpRequestMessage(new HttpMethod(\"{}\"), \"{}\");",
        cs_escape(&request.method),
        cs_escape(&request.url)
    );
    // `ByteArrayContent` carries no Content-Type of its own, so a Content-Type
    // header in the snapshot lands as the only one rather than appending to
    // `StringContent`'s implicit `text/plain; charset=utf-8`.
    if !request.body.is_empty() {
        output.push_str(&format!(
            "\nrequest.Content = new ByteArrayContent(Encoding.UTF8.GetBytes(\"{}\"));",
            cs_escape(&request.body)
        ));
    }
    // Every header row takes the same shape, whether or not the request has a
    // body. `HttpRequestMessage.Headers` **refuses** each content header —
    // Content-Type and its family — and drops it without a word, so a refused
    // row falls back to the content's own collection, creating an empty body
    // to hang it on when the request has none. Measured against .NET 10: a
    // bodyless request whose only Content-Type went to `request.Headers`
    // reaches the wire with no Content-Type at all.
    for (name, value) in &request.headers {
        let name = cs_escape(name);
        let value = cs_escape(value);
        output.push_str(&format!(
            "\nif (!request.Headers.TryAddWithoutValidation(\"{name}\", \"{value}\"))\n{{\n    request.Content ??= new ByteArrayContent(Array.Empty<byte>());\n    request.Content.Headers.TryAddWithoutValidation(\"{name}\", \"{value}\");\n}}"
        ));
    }
    output.push_str("\nusing var response = await client.SendAsync(request);");
    output
}

fn cs_escape(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn has_duplicate_header_names(headers: &[(String, String)]) -> bool {
    let mut names = std::collections::BTreeSet::new();
    headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .any(|name| !names.insert(name))
}

fn rust_escape(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn ps_escape(input: &str) -> String {
    input.replace('\'', "''")
}

/// A JSON string literal (`Value`'s `Display` cannot fail).
fn json_str(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RedactedRequestSnapshot {
        RedactedRequestSnapshot {
            method: "POST".into(),
            url: "https://example.test/items?q=one".into(),
            headers: vec![("Authorization".into(), "<redacted>".into())],
            body: "{\"name\":\"O'Reilly\"}".into(),
            ..RedactedRequestSnapshot::default()
        }
    }

    #[test]
    fn every_advertised_generator_is_nonempty_and_redacted() {
        for language in [
            SnippetLanguage::Curl,
            SnippetLanguage::RawHttp,
            SnippetLanguage::RustReqwest,
            SnippetLanguage::JavaScriptFetch,
            SnippetLanguage::TypeScriptFetch,
            SnippetLanguage::PythonRequests,
            SnippetLanguage::PowerShell,
            SnippetLanguage::CSharpHttpClient,
        ] {
            let generated = generate_snippet(language, &request());
            assert!(!generated.is_empty());
            assert!(generated.contains("<redacted>"));
            assert!(!generated.contains("actual-secret"));
        }
    }

    #[test]
    fn generators_do_not_collapse_duplicate_header_rows() {
        let mut request = request();
        request.headers = vec![
            ("X-Tag".into(), "alpha-header-value".into()),
            ("X-Tag".into(), "beta-header-value".into()),
        ];

        for language in [
            SnippetLanguage::Curl,
            SnippetLanguage::RawHttp,
            SnippetLanguage::RustReqwest,
            SnippetLanguage::JavaScriptFetch,
            SnippetLanguage::TypeScriptFetch,
            SnippetLanguage::PythonRequests,
            SnippetLanguage::PowerShell,
            SnippetLanguage::CSharpHttpClient,
        ] {
            let generated = generate_snippet(language, &request);
            assert!(
                generated.contains("alpha-header-value"),
                "{language:?}: {generated}"
            );
            assert!(
                generated.contains("beta-header-value"),
                "{language:?}: {generated}"
            );
        }
        let python = generate_snippet(SnippetLanguage::PythonRequests, &request);
        assert!(python.contains("connection.putheader(name, value)"));
        assert!(!python.contains("headers = {}"));

        let powershell = generate_snippet(SnippetLanguage::PowerShell, &request);
        assert_eq!(
            powershell
                .matches("$request.Headers.TryAddWithoutValidation('X-Tag'")
                .count(),
            2
        );
        assert!(!powershell.contains("-Headers @{"));
        assert!(!powershell.contains("StringContent"));

        let csharp = generate_snippet(SnippetLanguage::CSharpHttpClient, &request);
        assert_eq!(
            csharp
                .matches("!request.Headers.TryAddWithoutValidation(\"X-Tag\"")
                .count(),
            2
        );
        assert_eq!(
            csharp
                .matches("request.Content.Headers.TryAddWithoutValidation(\"X-Tag\"")
                .count(),
            2
        );
        assert!(!csharp.contains("new StringContent("));
    }

    /// The two generators that drive .NET's `HttpRequestMessage` — C# and
    /// PowerShell — share a trap that no other target has. Measured against
    /// .NET 10 on 2026-09-02, on captured wire bytes:
    ///
    /// - A bodyless request whose `Content-Type` is offered to
    ///   `HttpRequestMessage.Headers` is refused and **silently dropped**; the
    ///   request reached the socket with no Content-Type at all. So the
    ///   fallback onto the content's own header collection cannot be reserved
    ///   for requests that have a body — the snippet has to materialise an
    ///   empty body to hang the header on.
    /// - `StringContent` stamps `text/plain; charset=utf-8` on the content,
    ///   and the snapshot's own Content-Type then *appends* to it rather than
    ///   replacing it: `Content-Type: text/plain; charset=utf-8,
    ///   application/json`. `ByteArrayContent` carries no type of its own.
    #[test]
    fn dotnet_content_headers_survive_a_request_with_no_body() {
        let request = RedactedRequestSnapshot {
            method: "DELETE".into(),
            url: "https://example.test/items/7".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: String::new(),
            ..RedactedRequestSnapshot::default()
        };

        let csharp = generate_snippet(SnippetLanguage::CSharpHttpClient, &request);
        assert!(
            csharp.contains("request.Content ??= new ByteArrayContent(Array.Empty<byte>());"),
            "{csharp}"
        );
        assert!(
            csharp.contains("request.Content.Headers.TryAddWithoutValidation(\"Content-Type\""),
            "{csharp}"
        );
        // Nothing to encode, so naming System.Text would send the reader after
        // a using directive the snippet does not need.
        assert!(!csharp.contains("System.Text"), "{csharp}");

        let powershell = generate_snippet(SnippetLanguage::PowerShell, &request);
        assert!(
            powershell.contains(
                "$request.Content = [System.Net.Http.ByteArrayContent]::new([byte[]]::new(0))"
            ),
            "{powershell}"
        );
        assert!(
            powershell.contains("$request.Content.Headers.TryAddWithoutValidation('Content-Type'"),
            "{powershell}"
        );
    }
}
