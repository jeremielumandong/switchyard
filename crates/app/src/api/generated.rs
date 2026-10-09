//! Requests the assistant writes: the ```http blocks of an answer, parsed into
//! something the Workbench can open as a new request.
//!
//! The format is the one the Workbench's own prompts use for the request they
//! send (see `workbench::assist`): a request line, header lines, a blank line,
//! then the body. `# name` comment lines before the request line name it.
//!
//! ```http
//! # Create a user
//! POST {{baseUrl}}/users
//! Content-Type: application/json
//!
//! {"name": "Ada"}
//! ```

use switchyard_api::{Body, HttpMethod, KeyValueRow, RawBodyKind};

/// A request read from an answer, ready to open in the Workbench.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedRequest {
    /// A `# name` comment, else `METHOD path`.
    pub name: String,
    /// The method, validated as an HTTP token.
    pub method: HttpMethod,
    /// The URL as written, `{{variables}}` kept.
    pub url: String,
    /// Header lines in order.
    pub headers: Vec<(String, String)>,
    /// Everything after the blank line, trimmed; empty for none.
    pub body: String,
}

impl GeneratedRequest {
    /// The header rows the Workbench stores.
    pub fn header_rows(&self) -> Vec<KeyValueRow> {
        self.headers
            .iter()
            .map(|(name, value)| KeyValueRow::enabled(name, value))
            .collect()
    }

    /// The body as a raw body whose kind follows `Content-Type`.
    pub fn body(&self) -> Body {
        if self.body.is_empty() {
            return Body::None;
        }
        let content_type = self
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_ascii_lowercase())
            .unwrap_or_default();
        let media_type = if content_type.contains("json")
            || (content_type.is_empty() && looks_like_json(&self.body))
        {
            RawBodyKind::Json
        } else if content_type.contains("xml") {
            RawBodyKind::Xml
        } else {
            RawBodyKind::Text
        };
        Body::Raw {
            media_type,
            text: self.body.clone(),
        }
    }
}

fn looks_like_json(body: &str) -> bool {
    let body = body.trim();
    (body.starts_with('{') && body.ends_with('}')) || (body.starts_with('[') && body.ends_with(']'))
}

/// The fence labels read as a request.
fn is_http_fence(lang: &str) -> bool {
    matches!(lang, "http" | "https" | "rest" | "restclient")
}

/// Every ```http block of `text` that parses as a request, in order, without
/// duplicates. Other blocks (a response, a shell command, JSON) are skipped.
pub fn http_requests(text: &str) -> Vec<GeneratedRequest> {
    let mut out: Vec<GeneratedRequest> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(lang) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        let lang = lang.trim().to_ascii_lowercase();
        let mut block = Vec::new();
        for line in lines.by_ref() {
            if line.trim_start().starts_with("```") {
                break;
            }
            block.push(line);
        }
        if !is_http_fence(&lang) {
            continue;
        }
        if let Some(request) = parse_request(&block)
            && !out.contains(&request)
        {
            out.push(request);
        }
    }
    out
}

/// One block: comments, the request line, headers, a blank line, the body.
fn parse_request(block: &[&str]) -> Option<GeneratedRequest> {
    let mut name = None;
    let mut rest = block.iter().copied();
    let request_line = loop {
        let line = rest.next()?.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#').or_else(|| line.strip_prefix("//")) {
            let comment = comment.trim_start_matches('#').trim();
            if name.is_none() && !comment.is_empty() {
                name = Some(comment.to_owned());
            }
            continue;
        }
        break line;
    };
    let mut words = request_line.split_whitespace();
    let method = words.next()?;
    // `HTTP/1.1 200 OK` is a response, and lowercase words are prose.
    if !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let method = HttpMethod::new(method).ok()?;
    let url = words.next()?.to_owned();
    match words.next() {
        None => {}
        Some(version) if version.starts_with("HTTP/") && words.next().is_none() => {}
        Some(_) => return None,
    }
    let mut headers = Vec::new();
    let mut body = Vec::new();
    let mut in_body = false;
    for line in rest {
        if in_body {
            body.push(line);
        } else if line.trim().is_empty() {
            in_body = true;
        } else if let Some((name, value)) = line.split_once(':')
            && !name.trim().is_empty()
            && !name.trim().contains(char::is_whitespace)
        {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        } else {
            // No blank line before the body: the rest is the body.
            in_body = true;
            body.push(line);
        }
    }
    let name = name.unwrap_or_else(|| format!("{} {}", method.as_str(), path_of(&url)));
    Some(GeneratedRequest {
        name,
        method,
        url,
        headers,
        body: body.join("\n").trim().to_owned(),
    })
}

/// `https://host/a/b?x` → `/a/b`; a URL that starts with a variable keeps what
/// follows it.
fn path_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let after_host = if let Some(rest) = after_scheme.strip_prefix("{{") {
        rest.split_once("}}").map_or(after_scheme, |(_, rest)| rest)
    } else if url.contains("://") {
        after_scheme
            .find('/')
            .map_or("/", |index| &after_scheme[index..])
    } else {
        after_scheme
    };
    let path = after_host.split(['?', '#']).next().unwrap_or_default();
    if path.is_empty() { "/" } else { path }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_request_with_name_headers_and_body() {
        let answer = "Here it is:\n\n```http\n# Create a user\nPOST {{baseUrl}}/users HTTP/1.1\nContent-Type: application/json\nAuthorization: Bearer {{token}}\n\n{\n  \"name\": \"Ada\"\n}\n```\nSend it.";
        let found = http_requests(answer);
        assert_eq!(found.len(), 1);
        let r = &found[0];
        assert_eq!(r.name, "Create a user");
        assert_eq!(r.method.as_str(), "POST");
        assert_eq!(r.url, "{{baseUrl}}/users");
        assert_eq!(
            r.headers,
            [
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), "Bearer {{token}}".to_owned()),
            ]
        );
        assert_eq!(r.body, "{\n  \"name\": \"Ada\"\n}");
        assert!(matches!(
            r.body(),
            Body::Raw {
                media_type: RawBodyKind::Json,
                ..
            }
        ));
    }

    #[test]
    fn names_an_unnamed_request_by_method_and_path() {
        let found = http_requests("```http\nGET https://api.example.test/v1/items?limit=5\n```");
        assert_eq!(found[0].name, "GET /v1/items");
        assert_eq!(found[0].body(), Body::None);
        let found = http_requests("```rest\nDELETE {{base}}/items/{{id}}\n```");
        assert_eq!(found[0].name, "DELETE /items/{{id}}");
        let found = http_requests("```http\nGET https://example.test\n```");
        assert_eq!(found[0].name, "GET /");
    }

    #[test]
    fn skips_responses_prose_and_other_languages() {
        let answer = "```http\nHTTP/1.1 500 Internal Server Error\ncontent-type: text/plain\n\nboom\n```\n\
            ```bash\ncurl -X POST https://x.test\n```\n\
            ```json\n{\"a\": 1}\n```\n\
            ```http\nsend this to the server\n```\n\
            ```http\nGET https://x.test extra words\n```";
        assert!(http_requests(answer).is_empty());
    }

    #[test]
    fn keeps_every_distinct_request_once() {
        let block = "```http\nGET https://x.test/a\n```\n";
        let answer = format!(
            "{block}{block}```HTTP\nPUT https://x.test/b\nContent-Type: application/xml\n\n<a/>\n```"
        );
        let found = http_requests(&answer);
        assert_eq!(
            found.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            ["GET", "PUT"]
        );
        assert!(matches!(
            found[1].body(),
            Body::Raw {
                media_type: RawBodyKind::Xml,
                ..
            }
        ));
    }

    #[test]
    fn a_body_without_a_blank_line_is_still_the_body() {
        let found = http_requests(
            "```http\nPOST https://x.test/form\nContent-Type: text/plain\nhello world\n```",
        );
        assert_eq!(found[0].headers.len(), 1);
        assert_eq!(found[0].body, "hello world");
        assert!(matches!(
            found[0].body(),
            Body::Raw {
                media_type: RawBodyKind::Text,
                ..
            }
        ));
        // A JSON body with no Content-Type is still JSON.
        let found = http_requests("```http\nPATCH https://x.test/a\n\n[1, 2]\n```");
        assert!(matches!(
            found[0].body(),
            Body::Raw {
                media_type: RawBodyKind::Json,
                ..
            }
        ));
    }

    #[test]
    fn an_unclosed_block_still_parses() {
        let found = http_requests("Streaming…\n```http\nGET https://x.test/live");
        assert_eq!(found[0].url, "https://x.test/live");
    }
}
