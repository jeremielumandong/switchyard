//! Retained, selectable, read-only response documents. The input owns selection and scrolling;
//! response formatting remains on the workbench's background executor.

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme,
    input::{Editor, EditorState},
};
use gpui_kit::{AnyElement, App, Context, Entity, Render, SharedString, Window, div, prelude::*};

use super::{
    WorkbenchPanel,
    pretty::{BodyFormat, BodyPresentation, PreparedBody},
};

pub(super) struct ResponseEditor {
    pretty: bool,
    wrap: bool,
    source: Option<(Arc<str>, BodyFormat)>,
    input: Option<Entity<EditorState>>,
}

impl ResponseEditor {
    pub(super) fn new(pretty: bool) -> Self {
        Self {
            pretty,
            wrap: false,
            source: None,
            input: None,
        }
    }

    pub(super) fn set_body(&mut self, body: Option<&PreparedBody>, cx: &mut Context<Self>) {
        self.source = body.map(|body| {
            if self.pretty {
                match &body.presentation {
                    BodyPresentation::Inline { text, format, .. }
                    | BodyPresentation::Virtualized { text, format, .. } => (text.clone(), *format),
                }
            } else {
                (body.raw.clone(), BodyFormat::Text)
            }
        });
        // A new response starts a new document. Switching Pretty/Raw keeps the
        // two entities, their selection, and their scroll positions intact.
        self.input = None;
        cx.notify();
    }

    pub(super) fn set_wrap(&mut self, wrap: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = wrap;
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| input.set_soft_wrap(wrap, window, cx));
        }
        cx.notify();
    }
}

impl Render for ResponseEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((source, format)) = &self.source else {
            return div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Preparing response off the frame thread…")
                .into_any_element();
        };
        if self.input.is_none() {
            let language = match format {
                BodyFormat::Json => "json",
                BodyFormat::Xml => "html",
                BodyFormat::Text => "text",
                BodyFormat::Yaml => "yaml",
                BodyFormat::JavaScript => "javascript",
                BodyFormat::Markdown => "markdown",
                BodyFormat::Html => "html",
            };
            let line_numbers = self.pretty && source.contains('\n');
            let pretty = self.pretty;
            let source = source.clone();
            let wrap = self.wrap;
            // default_value seeds the rope without editing or recording undo.
            // The editor shapes visible lines and caches syntax highlights.
            self.input = Some(cx.new(|cx| {
                let input = EditorState::new(window, cx);
                let input = if pretty {
                    input.language(language).line_number(line_numbers)
                } else {
                    // Raw does not need a syntax parser or line-number gutter.
                    input.searchable(true)
                };
                input
                    .soft_wrap(wrap)
                    .placeholder("(empty response)")
                    .default_value(SharedString::from(source))
            }));
        }
        let pretty = self.pretty;
        let Some(input) = self.input.as_ref() else {
            return div().into_any_element();
        };
        div()
            .id("workbench-response-editor")
            .debug_selector(move || {
                if pretty {
                    "workbench-response-body"
                } else {
                    "workbench-response-raw-body"
                }
                .into()
            })
            .size_full()
            .min_w_0()
            .min_h_0()
            .font_family(crate::api::compat::fonts::mono(cx))
            .child(
                Editor::new(input)
                    // Read-only, not disabled: a disabled input swallows every
                    // mouse-down, so the scrollbar could not be dragged and text
                    // could not be selected or copied.
                    .readonly(true)
                    .appearance(false)
                    .h_full()
                    .w_full(),
            )
            .into_any_element()
    }
}

impl WorkbenchPanel {
    pub(super) fn render_response_editor(&self, pretty: bool, _: &App) -> AnyElement {
        if pretty {
            self.response_pretty_editor.clone().into_any_element()
        } else {
            self.response_raw_editor.clone().into_any_element()
        }
    }

    pub(super) fn sync_response_editors(&self, cx: &mut Context<Self>) {
        self.response_pretty_editor.update(cx, |editor, cx| {
            editor.set_body(
                self.ux
                    .response_view
                    .body
                    .as_ref()
                    .or(self.response_body.as_ref()),
                cx,
            )
        });
        self.response_raw_editor.update(cx, |editor, cx| {
            editor.set_body(self.response_body.as_ref(), cx)
        });
    }
}
