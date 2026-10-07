//! The Workbench → Chat handoff: a prompt built from what the panel already
//! keeps in redacted form.
//!
//! Nothing here reads the editors. The request half is the
//! [`RedactedRequestSnapshot`] the compiler produced for history and snippets
//! — secrets are structurally redacted before it exists — and the response
//! half is the bounded [`transport::Response`] decoded from the same exchange.
//! That is what keeps the spec's security boundary intact: the prompt can only
//! carry what the History tab could already show.
//!
//! The prompt lands in the Chat composer, it is not sent. The reader edits,
//! attaches, picks a model, and presses Enter — the same rule the Library's
//! Run follows, for the same reason: an unwanted turn is not one keystroke
//! away.

use switchyard_api::RedactedRequestSnapshot;

use super::transport;

/// What the reader wants the agent to do with the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistIntent {
    /// Review the request as drafted — no response needed.
    ReviewRequest,
    /// Explain what the response means.
    ExplainResponse,
    /// Work out why the request failed (a 4xx/5xx or a transport error).
    DiagnoseFailure,
    /// Write `pm.test(...)` assertions for the Tests tab.
    WriteTests,
    /// Draft a request body that fits the request's content type and any
    /// schema the headers or URL imply — the Body tab's "Generate from schema".
    GenerateBody,
    /// Produce iteration rows for the Data tab from a prose scenario.
    GenerateData,
    /// Name the `{{variables}}` the selected collection references that the
    /// active environment does not define — the Envs tab's "Fill from spec".
    FillEnvironment,
    /// Review a staged import before it is committed.
    ReviewImport,
}

impl AssistIntent {
    /// The `debug_selector` suffix — `workbench-ask-ai-{slug}`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ReviewRequest => "review",
            Self::ExplainResponse => "explain",
            Self::DiagnoseFailure => "debug",
            Self::WriteTests => "tests",
            Self::GenerateBody => "generate-body",
            Self::GenerateData => "generate-data",
            Self::FillEnvironment => "fill-env",
            Self::ReviewImport => "review-import",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReviewRequest => "Ask AI",
            Self::ExplainResponse => "Explain",
            Self::DiagnoseFailure => "Debug failure",
            Self::WriteTests => "Write tests",
            Self::GenerateBody => "Generate from schema",
            Self::GenerateData => "Generate sample data",
            Self::FillEnvironment => "Fill from spec",
            Self::ReviewImport => "Review with AI",
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::ReviewRequest => {
                "Review this HTTP request from my AgentOps API Workbench. Point out anything \
                 wrong or missing (method, URL, headers, body shape, auth), and suggest \
                 concrete improvements."
            }
            Self::ExplainResponse => {
                "Explain this HTTP exchange from my AgentOps API Workbench: what the response \
                 means, what the notable headers and fields are, and anything I should watch \
                 out for."
            }
            Self::DiagnoseFailure => {
                "This HTTP request from my AgentOps API Workbench failed. Diagnose the most \
                 likely cause from the request and response below and tell me exactly what \
                 to change to make it succeed."
            }
            Self::WriteTests => {
                "Write Postman-style `pm.test(...)` assertions for this HTTP exchange from my \
                 AgentOps API Workbench. Cover the status, the important headers, and the \
                 shape of the body. Return only the script I can paste into the Tests tab."
            }
            Self::GenerateBody => {
                "Generate a realistic request body for this HTTP request from my AgentOps API \
                 Workbench. Match the content type in the headers and any schema the URL or \
                 headers imply; keep `{{variable}}` placeholders where a value should come \
                 from the environment. Return only the body I can paste into the Body tab."
            }
            Self::GenerateData => {
                "Generate iteration data for the Data tab of my AgentOps API Workbench: a JSON \
                 array of objects, one per row, whose keys are the `{{variables}}` this \
                 request reads. Follow the scenario and constraints below exactly and use the \
                 seed so the rows are reproducible. Return only the JSON array."
            }
            Self::FillEnvironment => {
                "My AgentOps API Workbench environment is missing values for the variables \
                 listed below, which the selected collection references. Suggest a value for \
                 each as `KEY=value` lines I can paste into the environment editor; mark \
                 anything that is a credential as `secret:KEY=` with no value."
            }
            Self::ReviewImport => {
                "Review this collection my AgentOps API Workbench parsed from an imported \
                 file before I commit it. Call out destructive operations, missing \
                 authentication, inconsistent naming, and anything the parser may have \
                 misread."
            }
        }
    }

    /// Whether this intent needs a response to talk about.
    pub fn needs_response(self) -> bool {
        matches!(
            self,
            Self::ExplainResponse | Self::DiagnoseFailure | Self::WriteTests
        )
    }

    /// Whether this intent needs the draft compiled into a redacted snapshot.
    /// The environment and import intents talk about the workspace, not a
    /// request, so a blank Compose tab must not block them.
    pub fn needs_request(self) -> bool {
        !matches!(self, Self::FillEnvironment | Self::ReviewImport)
    }
}

/// What a prompt is built from. Every field is already redacted or is
/// workspace metadata (names, keys, counts) — never a resolved secret.
#[derive(Default)]
pub struct AssistContext<'a> {
    pub request: Option<&'a RedactedRequestSnapshot>,
    pub response: Option<&'a transport::Response>,
    pub error: Option<&'a str>,
    /// Extra `### heading` sections appended after the exchange, in order.
    pub sections: Vec<(&'static str, String)>,
}

/// The most of a body the prompt carries. Long enough for any realistic JSON
/// document, short enough that a paginated dump does not bury the question.
pub const MAX_BODY_CHARS: usize = 6_000;

/// Redacted values arrive as this marker from the compiler; a test pins that
/// the prompt keeps it rather than a resolved value.
#[cfg(test)]
pub const REDACTED_MARKER: &str = "<redacted>";

/// Build the composer text for `intent`.
///
/// `response` is the decoded, bounded, redacted response — `None` when the
/// send never produced one (then `error` says why). `request` is always the
/// redacted snapshot, never the editors.
#[cfg(test)]
pub fn prompt(
    intent: AssistIntent,
    request: &RedactedRequestSnapshot,
    response: Option<&transport::Response>,
    error: Option<&str>,
) -> String {
    compose(
        intent,
        &AssistContext {
            request: Some(request),
            response,
            error,
            sections: Vec::new(),
        },
    )
}

/// [`prompt`] with the full context: an optional request, and any extra
/// sections (a scenario, the missing variables, the staged import summary).
pub fn compose(intent: AssistIntent, context: &AssistContext<'_>) -> String {
    let AssistContext {
        request,
        response,
        error,
        sections,
    } = context;
    let (response, error) = (*response, *error);
    let mut out = String::new();
    out.push_str(intent.instruction());
    if let Some(request) = request {
        out.push_str("\n\n### Request\n```http\n");
        out.push_str(&request.method);
        out.push(' ');
        out.push_str(&request.url);
        out.push('\n');
        for (name, value) in &request.headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        if request.body_sensitive {
            out.push_str("\n[request body omitted: marked sensitive]\n");
        } else if request.body_binary {
            out.push_str(&format!(
                "\n[binary request body: {} bytes]\n",
                request.body_bytes
            ));
        } else if !request.body.is_empty() {
            out.push('\n');
            out.push_str(&bounded(&request.body, request.body_bytes));
            out.push('\n');
            if request.body_truncated {
                out.push_str("[request body truncated by the Workbench's retention limit]\n");
            }
        }
        out.push_str("```\n");
    } else {
        out.push('\n');
    }
    let request_url = request.map(|request| request.url.as_str()).unwrap_or("");

    match response {
        Some(response) => {
            out.push_str("\n### Response\n```http\n");
            out.push_str(&format!(
                "{} {} {}\n",
                if response.http_version.is_empty() {
                    "HTTP"
                } else {
                    response.http_version.as_str()
                },
                response.status,
                response.reason
            ));
            for (name, value) in &response.headers {
                out.push_str(name);
                out.push_str(": ");
                out.push_str(value);
                out.push('\n');
            }
            if response.binary {
                out.push_str(&format!(
                    "\n[binary response body: {} bytes]\n",
                    response.received_bytes
                ));
            } else if !response.body.is_empty() {
                out.push('\n');
                out.push_str(&bounded(&response.body, response.received_bytes));
                out.push('\n');
                if response.truncated {
                    out.push_str("[response body truncated by the Workbench's retention limit]\n");
                }
            }
            out.push_str("```\n");
            let mut facts = vec![format!("{} ms", response.duration_ms)];
            if !response.final_url.is_empty() && response.final_url != request_url {
                facts.push(format!("final URL {}", response.final_url));
            }
            if !response.redirects.is_empty() {
                facts.push(format!("{} redirect(s)", response.redirects.len()));
            }
            out.push_str(&format!("\nTiming: {}\n", facts.join(" · ")));
            if !response.test_results.is_empty() {
                out.push_str("\n### Existing test results\n");
                for result in &response.test_results {
                    out.push_str(&format!(
                        "- {} {}{}\n",
                        if result.skipped {
                            "SKIP"
                        } else if result.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        result.name,
                        result
                            .error
                            .as_deref()
                            .map(|error| format!(" — {error}"))
                            .unwrap_or_default()
                    ));
                }
            }
        }
        // A review is about the draft; there is nothing to say about a
        // response that was never asked for.
        None if !intent.needs_response() && error.is_none() => {}
        None => {
            out.push_str("\n### Response\n");
            out.push_str(match error {
                Some(error) if !error.trim().is_empty() => error.trim(),
                _ => "No response was received.",
            });
            out.push('\n');
        }
    }
    if let Some(error) = error.filter(|error| response.is_some() && !error.trim().is_empty()) {
        out.push_str(&format!("\nWorkbench error: {}\n", error.trim()));
    }
    for (heading, body) in sections {
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {heading}\n{body}\n"));
    }
    out
}

/// The first [`MAX_BODY_CHARS`] of `body`, cut on a char boundary, with a note
/// naming what was left out.
fn bounded(body: &str, full_bytes: u64) -> String {
    if body.chars().count() <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let cut = body
        .char_indices()
        .nth(MAX_BODY_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(body.len());
    let shown = &body[..cut];
    let total = if full_bytes > 0 {
        full_bytes
    } else {
        body.len() as u64
    };
    format!("{shown}\n…[truncated: showing {MAX_BODY_CHARS} of {total} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RedactedRequestSnapshot {
        RedactedRequestSnapshot {
            method: "POST".into(),
            url: "https://api.example.test/items".into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {REDACTED_MARKER}")),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: r#"{"name":"widget"}"#.into(),
            replay: None,
            body_bytes: 17,
            body_sensitive: false,
            body_binary: false,
            body_truncated: false,
            body_omitted_reason: None,
        }
    }

    fn response(status: u16, body: &str) -> transport::Response {
        transport::Response {
            console: Vec::new(),
            status,
            reason: if status == 200 { "OK" } else { "Server Error" }.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            set_cookies: Vec::new(),
            cookie_mutations: Vec::new(),
            body: body.into(),
            body_base64: String::new(),
            binary: false,
            final_url: "https://api.example.test/items".into(),
            http_version: "HTTP/1.1".into(),
            received_bytes: body.len() as u64,
            stored_bytes: body.len() as u64,
            full_body_sha256: None,
            timings: Default::default(),
            cookies: Vec::new(),
            duration_ms: 42,
            truncated: false,
            redirects: Vec::new(),
            test_results: Vec::new(),
        }
    }

    #[test]
    fn every_intent_opens_with_its_own_instruction_and_the_request() {
        for intent in [
            AssistIntent::ReviewRequest,
            AssistIntent::ExplainResponse,
            AssistIntent::DiagnoseFailure,
            AssistIntent::WriteTests,
            AssistIntent::GenerateBody,
            AssistIntent::GenerateData,
        ] {
            let text = prompt(intent, &request(), Some(&response(200, "{}")), None);
            assert!(text.starts_with(intent.instruction()), "{intent:?}");
            assert!(text.contains("POST https://api.example.test/items"));
            assert!(text.contains("HTTP/1.1 200 OK"));
            assert!(text.contains("42 ms"));
        }
        assert!(prompt(AssistIntent::WriteTests, &request(), None, None).contains("pm.test"));
    }

    #[test]
    fn redaction_marker_survives_and_no_secret_is_reconstructed() {
        let text = prompt(
            AssistIntent::ExplainResponse,
            &request(),
            Some(&response(200, "{}")),
            None,
        );
        assert!(text.contains(&format!("Authorization: Bearer {REDACTED_MARKER}")));
        assert!(!text.contains("sk-live"));
    }

    #[test]
    fn a_missing_response_reports_the_transport_error() {
        let text = prompt(
            AssistIntent::DiagnoseFailure,
            &request(),
            None,
            Some("connection refused"),
        );
        assert!(text.contains("### Response\nconnection refused"));
        assert!(!text.contains("Workbench error"));
    }

    #[test]
    fn long_bodies_are_bounded_with_a_note() {
        let body = "x".repeat(MAX_BODY_CHARS + 500);
        let text = prompt(
            AssistIntent::ExplainResponse,
            &request(),
            Some(&response(200, &body)),
            None,
        );
        assert!(text.contains(&format!(
            "…[truncated: showing {MAX_BODY_CHARS} of {} bytes]",
            MAX_BODY_CHARS + 500
        )));
        assert!(!text.contains(&"x".repeat(MAX_BODY_CHARS + 1)));
    }

    #[test]
    fn binary_and_sensitive_bodies_become_size_lines() {
        let mut binary = response(200, "\u{0}\u{1}");
        binary.binary = true;
        binary.received_bytes = 2048;
        let mut sensitive = request();
        sensitive.body_sensitive = true;
        let text = prompt(
            AssistIntent::ExplainResponse,
            &sensitive,
            Some(&binary),
            None,
        );
        assert!(text.contains("[request body omitted: marked sensitive]"));
        assert!(text.contains("[binary response body: 2048 bytes]"));
        assert!(!text.contains("widget"));
    }

    #[test]
    fn test_results_and_errors_are_listed_after_the_response() {
        let mut failed = response(500, "{\"error\":\"boom\"}");
        failed.test_results = vec![switchyard_api::TestResult {
            name: "status is 200".into(),
            passed: false,
            skipped: false,
            error: Some("expected 200, got 500".into()),
        }];
        let text = prompt(
            AssistIntent::DiagnoseFailure,
            &request(),
            Some(&failed),
            Some("post-response script threw"),
        );
        assert!(text.contains("- FAIL status is 200 — expected 200, got 500"));
        assert!(text.contains("Workbench error: post-response script threw"));
    }

    #[test]
    fn workspace_intents_need_no_request_and_carry_their_sections() {
        assert!(!AssistIntent::FillEnvironment.needs_request());
        assert!(!AssistIntent::ReviewImport.needs_request());
        assert!(AssistIntent::GenerateBody.needs_request());
        assert!(!AssistIntent::GenerateData.needs_response());
        let text = compose(
            AssistIntent::FillEnvironment,
            &AssistContext {
                sections: vec![
                    ("Environment", "staging · inherits Globals".into()),
                    ("Missing variables", "- service_url\n- api_token".into()),
                    ("Empty", "   ".into()),
                ],
                ..Default::default()
            },
        );
        assert!(text.starts_with(AssistIntent::FillEnvironment.instruction()));
        assert!(!text.contains("### Request"));
        assert!(text.contains("### Environment\nstaging · inherits Globals\n"));
        assert!(text.contains("### Missing variables\n- service_url\n- api_token\n"));
        assert!(!text.contains("### Empty"));

        let text = compose(
            AssistIntent::GenerateData,
            &AssistContext {
                request: Some(&request()),
                sections: vec![("Scenario", "Happy path · 12 rows · seed 4417".into())],
                ..Default::default()
            },
        );
        let request_at = text.find("### Request").unwrap();
        let scenario_at = text.find("### Scenario").unwrap();
        assert!(request_at < scenario_at, "sections follow the exchange");
        assert!(text.contains("seed 4417"));
    }
}
