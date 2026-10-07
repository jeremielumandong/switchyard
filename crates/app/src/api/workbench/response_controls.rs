//! Response-only formatting and document queries; the original response remains intact.
use super::*;
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
};
use gpui_kit::{AnyElement, div};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ResponseFormat {
    #[default]
    Auto,
    Json,
    Xml,
    Html,
    Text,
    Yaml,
    JavaScript,
    Markdown,
    Hex,
    Base64,
    HtmlPreview,
    MarkdownPreview,
}

impl ResponseFormat {
    const ALL: [Self; 12] = [
        Self::Auto,
        Self::Json,
        Self::Xml,
        Self::Html,
        Self::Text,
        Self::Yaml,
        Self::JavaScript,
        Self::Markdown,
        Self::Hex,
        Self::Base64,
        Self::HtmlPreview,
        Self::MarkdownPreview,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Html => "HTML source",
            Self::Text => "Text",
            Self::Yaml => "YAML",
            Self::JavaScript => "JavaScript",
            Self::Markdown => "Markdown source",
            Self::Hex => "Hex bytes",
            Self::Base64 => "Base64 bytes",
            Self::HtmlPreview => "HTML preview",
            Self::MarkdownPreview => "Markdown preview",
        }
    }
}

pub(super) struct ResponseView {
    pub format: ResponseFormat,
    pub wrap: bool,
    pub query: Entity<InputState>,
    pub applied_query: String,
    pub body: Option<pretty::PreparedBody>,
    pub preview: Option<SharedString>,
    pub feedback: Option<String>,
    pub error: Option<String>,
    generation: u64,
    work: Option<gpui_kit::Task<()>>,
    pub rerun_work: Option<gpui_kit::Task<()>>,
    pub rerun_feedback: Option<String>,
}

impl ResponseView {
    pub fn new(window: &mut Window, cx: &mut Context<WorkbenchPanel>) -> Self {
        Self {
            format: ResponseFormat::Auto,
            wrap: false,
            query: cx.new(|cx| {
                InputState::new(window, cx).placeholder("JSONPath $.data[*] or XPath //item")
            }),
            applied_query: String::new(),
            body: None,
            preview: None,
            feedback: None,
            error: None,
            generation: 0,
            work: None,
            rerun_work: None,
            rerun_feedback: None,
        }
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.work = None;
        self.rerun_work = None;
        self.rerun_feedback = None;
        self.body = None;
        self.preview = None;
        self.applied_query.clear();
        self.feedback = None;
        self.error = None;
    }
}

fn format_body(
    content_type: Option<&str>,
    source: &str,
    format: ResponseFormat,
) -> Result<pretty::PreparedBody, String> {
    use base64::Engine;
    use pretty::BodyFormat;
    if format == ResponseFormat::Auto {
        return Ok(pretty::prepare(content_type, source, false));
    }
    let (text, syntax) = match format {
        ResponseFormat::Auto => unreachable!(),
        ResponseFormat::Json => (
            pretty::format_json(source)
                .ok_or("The response is not valid JSON. Choose Text or Raw to inspect it.")?,
            BodyFormat::Json,
        ),
        ResponseFormat::Xml => {
            sxd_document::parser::parse(source).map_err(|error| format!("Invalid XML: {error}"))?;
            (
                pretty::format_xml(source).ok_or("The response could not be formatted as XML.")?,
                BodyFormat::Xml,
            )
        }
        ResponseFormat::Html | ResponseFormat::HtmlPreview => {
            (source.to_string(), BodyFormat::Html)
        }
        ResponseFormat::Text => (source.to_string(), BodyFormat::Text),
        ResponseFormat::Yaml => {
            let value: serde_yaml::Value =
                serde_yaml::from_str(source).map_err(|error| format!("Invalid YAML: {error}"))?;
            (
                serde_yaml::to_string(&value).map_err(|error| error.to_string())?,
                BodyFormat::Yaml,
            )
        }
        ResponseFormat::JavaScript => (source.to_string(), BodyFormat::JavaScript),
        ResponseFormat::Markdown | ResponseFormat::MarkdownPreview => {
            (source.to_string(), BodyFormat::Markdown)
        }
        ResponseFormat::Base64 => (
            base64::engine::general_purpose::STANDARD.encode(source.as_bytes()),
            BodyFormat::Text,
        ),
        ResponseFormat::Hex => (
            source
                .as_bytes()
                .chunks(16)
                .enumerate()
                .map(|(ix, chunk)| {
                    format!(
                        "{:08x}  {}",
                        ix * 16,
                        chunk
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            BodyFormat::Text,
        ),
    };
    let mut prepared = pretty::prepare(Some("text/plain"), "", false);
    prepared.raw = Arc::from(source);
    prepared.presentation = pretty::BodyPresentation::Inline {
        format: syntax,
        text: Arc::from(text),
        spans: Arc::new(Vec::new()),
        line_numbers: None,
    };
    Ok(prepared)
}

impl WorkbenchPanel {
    pub(super) fn rerun_response_tests(&mut self, cx: &mut Context<Self>) {
        use switchyard_api::runtime::send::{
            ordered_script, resolved_script_variables, script_request,
        };
        if self.ux.response_view.rerun_work.is_some() || self.send_state != SendState::Idle {
            return;
        }
        let Some(response) = self
            .response
            .as_ref()
            .map(|response| response.as_ref().clone())
        else {
            return;
        };
        let prepared = (|| -> Result<_, String> {
            let (definition, mut secrets) = self.draft_request(cx)?;
            let environment = self.environment_send_context(cx)?;
            let (environment_variables, entered) =
                draft::parse_session_variables(&environment.source, &environment.scope)?;
            secrets.merge(environment.secrets);
            secrets.merge(entered);
            let data = self
                .workspace_data
                .as_ref()
                .ok_or("Workbench storage is unavailable")?;
            let collection = data.collection(&definition.collection_id);
            let folders = folder_chain(&data.folders, &definition);
            let script = ordered_script(collection, &folders, &definition, true);
            if script.trim().is_empty() {
                return Err(
                    "Add post-response assertions in Scripts before rerunning tests.".into(),
                );
            }
            let globals = data
                .store
                .global_variables(&data.workspace)
                .map_err(|error| error.to_string())?;
            let scopes = transport::ScriptScopes {
                globals: resolved_script_variables(&globals, &secrets),
                environment_name: environment.environment.map(|environment| environment.name),
                environment: resolved_script_variables(&environment_variables, &secrets),
                collection: collection
                    .map(|collection| resolved_script_variables(&collection.variables, &secrets))
                    .unwrap_or_default(),
                local: resolved_script_variables(&definition.variables, &secrets),
                cookies: draft::parse_cookie_pairs(&self.cookies.read(cx).value())?
                    .into_iter()
                    .collect(),
                allow_private_network: definition.settings.allow_private_network,
                ..Default::default()
            };
            Ok((script, scopes, script_request(&definition)))
        })();
        let (script, scopes, request) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.ux.response_view.rerun_feedback = Some(error);
                cx.notify();
                return;
            }
        };
        let transport = self.transport.clone();
        let generation = self.response_body_generation;
        self.ux.response_view.rerun_feedback =
            Some("Running assertions against the current response…".into());
        self.ux.response_view.rerun_work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                let mut response = response;
                switchyard_api::runtime::rerun_response_tests(
                    transport.as_ref(),
                    &script,
                    scopes,
                    request,
                    &mut response,
                )
                .map(|_| response)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.response_body_generation != generation {
                    return;
                }
                panel.ux.response_view.rerun_work = None;
                match result {
                    Ok(response) => {
                        panel.response = Some(Arc::new(response));
                        panel.ux.response_view.rerun_feedback = Some(
                            "Tests refreshed. No request sent; variable changes were not saved."
                                .into(),
                        );
                    }
                    Err(error) => {
                        panel.ux.response_view.rerun_feedback =
                            Some(format!("Test rerun failed: {error}"))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_rerun_tests(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_1()
            .flex_none()
            .child(
                Button::new("workbench-rerun-tests")
                    .outline()
                    .small()
                    .label(if self.ux.response_view.rerun_work.is_some() {
                        "Running tests…"
                    } else {
                        "Rerun tests on this response"
                    })
                    .disabled(
                        self.response.is_none()
                            || self.ux.response_view.rerun_work.is_some()
                            || self.send_state != SendState::Idle,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.rerun_response_tests(cx))),
            )
            .when_some(
                self.ux.response_view.rerun_feedback.clone(),
                |el, feedback| el.child(div().text_xs().child(feedback)),
            )
            .into_any_element()
    }

    pub(super) fn refresh_response_view(&mut self, cx: &mut Context<Self>) {
        self.ux.response_view.generation = self.ux.response_view.generation.wrapping_add(1);
        self.ux.response_view.work = None;
        self.ux.response_view.error = None;
        let Some(response) = self.response.clone() else {
            return;
        };
        let format = self.ux.response_view.format;
        let query = self.ux.response_view.applied_query.clone();
        if format == ResponseFormat::Auto && query.is_empty() {
            self.ux.response_view.body = None;
            self.ux.response_view.preview = None;
            self.ux.response_view.feedback = None;
            self.sync_response_editors(cx);
            cx.notify();
            return;
        }
        if response.binary
            && (!matches!(format, ResponseFormat::Hex | ResponseFormat::Base64)
                || !query.is_empty())
        {
            self.ux.response_view.error = Some(
                "Binary responses support Hex bytes and Base64 bytes. Clear the text filter first."
                    .into(),
            );
            cx.notify();
            return;
        }
        self.ux.response_view.feedback = Some("Preparing response view…".into());
        let generation = self.ux.response_view.generation;
        let response_generation = self.response_body_generation;
        self.ux.response_view.work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                    if response.binary {
                        use base64::Engine;
                        let bytes = base64::engine::general_purpose::STANDARD.decode(&response.body_base64).map_err(|error| format!("Invalid binary response encoding: {error}"))?;
                        let output = if format == ResponseFormat::Base64 { base64::engine::general_purpose::STANDARD.encode(&bytes) } else {
                            bytes.chunks(16).enumerate().map(|(ix, chunk)| format!("{:08x}  {}", ix * 16, chunk.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" "))).collect::<Vec<_>>().join("\n")
                        };
                        return format_body(None, &output, ResponseFormat::Text).map(|body| (body, None, None));
                    }
                    let (source, count) = if query.is_empty() {
                        (response.body.clone(), None)
                    } else {
                        let (text, count) = super::response_query::filter(&response.body, &query)?;
                        (text, Some(count))
                    };
                    let content_type = response
                        .headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
                        .map(|(_, value)| value.as_str());
                    let preview = if matches!(format, ResponseFormat::HtmlPreview | ResponseFormat::MarkdownPreview) {
                        Some(SharedString::from(super::response_preview::sanitize(&source, format == ResponseFormat::MarkdownPreview)?))
                    } else { None };
                    format_body(content_type, &source, format).map(|body| (body, count, preview))
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.response_body_generation != response_generation
                    || panel.ux.response_view.generation != generation
                {
                    return;
                }
                panel.ux.response_view.work = None;
                match result {
                    Ok((body, count, preview)) => {
                        panel.ux.response_view.body = Some(body);
                        panel.ux.response_view.preview = preview;
                        panel.ux.response_view.feedback =
                            count.map(|count| format!("{count} matches · filtered Pretty view"));
                        if panel.ux.response_view.preview.is_some() {
                            panel.ux.response_view.feedback = Some("Static preview · scripts, forms, links, and external media are inactive".into());
                        }
                        panel.sync_response_editors(cx);
                    }
                    Err(error) => {
                        panel.ux.response_view.error = Some(error);
                        panel.ux.response_view.feedback = Some(
                            "Showing the previous view. Raw always contains the original response."
                                .into(),
                        );
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_response_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let handle = cx.entity().downgrade();
        let selected = self.ux.response_view.format;
        let unavailable = self
            .response
            .as_ref()
            .is_none_or(|response| response.binary);
        let wrap = self.ux.response_view.wrap;
        let binary = self
            .response
            .as_ref()
            .is_some_and(|response| response.binary);
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap_1()
            .px_3()
            .py_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("workbench-response-format")
                            .debug_selector(|| "workbench-response-format".into())
                            .outline()
                            .small()
                            .label(format!("Pretty format: {}", selected.label()))
                            .disabled(self.response.is_none())
                            .dropdown_menu(move |mut menu, _, _| {
                                for format in ResponseFormat::ALL {
                                    let handle = handle.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(format.label())
                                            .disabled(
                                                binary
                                                    && !matches!(
                                                        format,
                                                        ResponseFormat::Auto
                                                            | ResponseFormat::Hex
                                                            | ResponseFormat::Base64
                                                    ),
                                            )
                                            .checked(format == selected)
                                            .on_click(move |_, _, cx| {
                                                let _ = handle.update(cx, |panel, cx| {
                                                    panel.ux.response_view.format = format;
                                                    panel.refresh_response_view(cx);
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        Button::new("workbench-response-wrap")
                            .ghost()
                            .small()
                            .label(if wrap { "Wrap: on" } else { "Wrap: off" })
                            .debug_selector(|| "workbench-response-wrap".into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ux.response_view.wrap = !this.ux.response_view.wrap;
                                for editor in
                                    [&this.response_pretty_editor, &this.response_raw_editor]
                                {
                                    editor.update(cx, |editor, cx| {
                                        editor.set_wrap(this.ux.response_view.wrap, window, cx)
                                    });
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        field::bare(&self.ux.response_view.query)
                            .small()
                            .w(gpui_kit::rems(20.))
                            .max_w_full()
                            .disabled(unavailable),
                    )
                    .child(
                        Button::new("workbench-response-filter")
                            .debug_selector(|| "workbench-response-filter".into())
                            .outline()
                            .small()
                            .label("Filter")
                            .disabled(unavailable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ux.response_view.applied_query = this
                                    .ux
                                    .response_view
                                    .query
                                    .read(cx)
                                    .value()
                                    .trim()
                                    .to_string();
                                this.response_tab = ResponseTab::Pretty;
                                // Query results have their own type (XPath node sets
                                // are rendered as JSON arrays), not the input's format.
                                this.ux.response_view.format = ResponseFormat::Auto;
                                this.refresh_response_view(cx);
                            })),
                    )
                    .child(
                        Button::new("workbench-response-clear-filter")
                            .debug_selector(|| "workbench-response-clear-filter".into())
                            .ghost()
                            .small()
                            .label("Clear filter")
                            .disabled(self.ux.response_view.applied_query.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ux.response_view.applied_query.clear();
                                set_input(&this.ux.response_view.query, "", window, cx);
                                this.refresh_response_view(cx);
                            })),
                    ),
            )
            .when_some(self.ux.response_view.feedback.clone(), |el, feedback| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(feedback),
                )
            })
            .when_some(self.ux.response_view.error.clone(), |el, error| {
                el.child(div().text_xs().text_color(cx.theme().danger).child(error))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &pretty::PreparedBody) -> &str {
        match &body.presentation {
            pretty::BodyPresentation::Inline { text, .. }
            | pretty::BodyPresentation::Virtualized { text, .. } => text,
        }
    }
    #[test]
    fn manual_format_overrides_detection_and_reports_invalid_documents() {
        let source = "{\"ok\":true}";
        assert_eq!(
            text(&format_body(None, source, ResponseFormat::Text).unwrap()),
            source
        );
        assert!(text(&format_body(None, source, ResponseFormat::Json).unwrap()).contains('\n'));
        assert!(format_body(None, "{broken", ResponseFormat::Json).is_err());
        assert_eq!(
            text(&format_body(None, "é", ResponseFormat::Base64).unwrap()),
            "w6k="
        );
        assert_eq!(
            text(&format_body(None, "é", ResponseFormat::Hex).unwrap()),
            "00000000  c3 a9"
        );
        assert!(
            text(&format_body(None, "hello: world", ResponseFormat::Yaml).unwrap())
                .contains("hello: world")
        );
    }
}
