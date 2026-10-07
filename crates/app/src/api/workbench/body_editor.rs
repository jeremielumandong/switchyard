//! Structured bodies bound to the composer's existing source text.

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, input::InputEvent};
use gpui_kit::{Context, Entity, Render, Subscription, Window, div, prelude::*};

use super::{draft::BodyMode, entries::KvGrid};

/// Keep variables as source text so templates and unfinished edits are lossless.
pub(super) struct GraphQlDraft {
    pub query: String,
    pub variables: String,
    extra: serde_json::Map<String, serde_json::Value>,
}

pub(super) fn decode_graphql(source: &str) -> GraphQlDraft {
    if let Ok(serde_json::Value::Object(mut object)) = serde_json::from_str(source)
        && let Some(query) = object
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    {
        object.remove("query");
        let variables = object
            .remove("variables")
            .map(|value| serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
            .unwrap_or_else(|| "{}".into());
        return GraphQlDraft {
            query,
            variables,
            extra: object,
        };
    }
    // Our envelope writes variables last. Read the valid prefix independently
    // so unquoted {{templates}} and invalid in-progress variables are retained.
    if let Some(source) = source.strip_prefix("{\"query\":") {
        let mut query = serde_json::Deserializer::from_str(source).into_iter::<String>();
        if let Some(Ok(query_value)) = query.next() {
            let mut rest = &source[query.byte_offset()..];
            let mut extra = serde_json::Map::new();
            loop {
                if let Some(variables) = rest
                    .strip_prefix(",\"variables\":")
                    .and_then(|value| value.strip_suffix('}'))
                {
                    return GraphQlDraft {
                        query: query_value,
                        variables: variables.to_string(),
                        extra,
                    };
                }
                let Some(tail) = rest.strip_prefix(',') else {
                    break;
                };
                let mut keys = serde_json::Deserializer::from_str(tail).into_iter::<String>();
                let Some(Ok(key)) = keys.next() else {
                    break;
                };
                let Some(value_source) = tail[keys.byte_offset()..].strip_prefix(':') else {
                    break;
                };
                let mut values = serde_json::Deserializer::from_str(value_source)
                    .into_iter::<serde_json::Value>();
                let Some(Ok(value)) = values.next() else {
                    break;
                };
                extra.insert(key, value);
                rest = &value_source[values.byte_offset()..];
            }
        }
    }
    GraphQlDraft {
        query: source.to_string(),
        variables: "{}".into(),
        extra: Default::default(),
    }
}

pub(super) fn encode_graphql(query: &str, variables: &str) -> String {
    encode_with_extra(query, variables, &Default::default())
}

fn encode_with_extra(
    query: &str,
    variables: &str,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut source = format!("{{\"query\":{}", serde_json::Value::from(query));
    for (key, value) in extra {
        source.push(',');
        source.push_str(&serde_json::Value::from(key.as_str()).to_string());
        source.push(':');
        source.push_str(&value.to_string());
    }
    source.push_str(",\"variables\":");
    source.push_str(variables);
    source.push('}');
    source
}

pub(super) struct BodyEditor {
    source: Entity<TextareaState>,
    mode: BodyMode,
    form: Entity<KvGrid>,
    multipart: Entity<KvGrid>,
    query: Entity<TextareaState>,
    variables: Entity<TextareaState>,
    extra: serde_json::Map<String, serde_json::Value>,
    last_source: Option<String>,
    last_fields: (String, String),
    _subscriptions: Vec<Subscription>,
}

impl BodyEditor {
    pub(super) fn new(
        source: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let form = cx.new(|_| {
            KvGrid::new(
                source.clone(),
                '=',
                "workbench-form-body",
                "URL-encoded form fields. Repeated names are kept in order.",
                ("Field name", "Value"),
                false,
            )
        });
        let multipart = cx.new(|_| KvGrid::new(source.clone(), '=', "workbench-multipart-body", "Text values or file: followed by a path. Choose matching files below before sending.", ("Part name", "Text or file:path"), false));
        let query = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 12)
                .placeholder("query Users { users { id name } }")
        });
        let variables = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("{\"id\": \"{{user_id}}\"}")
        });
        let mut subscriptions = Vec::new();
        for input in [&query, &variables] {
            subscriptions.push(
                cx.subscribe_in(input, window, |this, _, event, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.push_graphql(window, cx);
                    }
                }),
            );
        }
        subscriptions.push(
            cx.subscribe_in(&source, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) && this.mode == BodyMode::GraphQl {
                    this.sync_graphql(window, cx);
                }
            }),
        );
        Self {
            source,
            mode: BodyMode::None,
            form,
            multipart,
            query,
            variables,
            extra: Default::default(),
            last_source: None,
            last_fields: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn set_mode(&mut self, mode: BodyMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.last_source = None;
            cx.notify();
        }
    }

    fn sync_graphql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.source.read(cx).value().to_string();
        if self.last_source.as_deref() == Some(&source) {
            return;
        }
        let draft = decode_graphql(&source);
        self.last_source = Some(source);
        self.last_fields = (draft.query.clone(), draft.variables.clone());
        self.extra = draft.extra;
        self.query
            .update(cx, |input, cx| input.set_value(draft.query, window, cx));
        self.variables
            .update(cx, |input, cx| input.set_value(draft.variables, window, cx));
        cx.notify();
    }

    fn push_graphql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != BodyMode::GraphQl {
            return;
        }
        let query = self.query.read(cx).value().to_string();
        let variables = self.variables.read(cx).value().to_string();
        if self.last_fields == (query.clone(), variables.clone()) {
            return;
        }
        self.last_fields = (query.clone(), variables.clone());
        let source = encode_with_extra(&query, &variables, &self.extra);
        self.last_source = Some(source.clone());
        if self.source.read(cx).value().as_ref() != source {
            self.source
                .update(cx, |input, cx| input.set_value(source, window, cx));
        }
        cx.notify();
    }
}

impl Render for BodyEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.mode {
            BodyMode::Form => self.form.clone().into_any_element(),
            BodyMode::Multipart => self.multipart.clone().into_any_element(),
            BodyMode::GraphQl => {
                self.sync_graphql(window, cx);
                let variables = self.variables.read(cx).value();
                let message = if variables.trim().is_empty() {
                    "Empty variables sends an empty object.".to_string()
                } else if variables.contains("{{") {
                    "Variables support {{templates}}; JSON is validated after substitution."
                        .to_string()
                } else {
                    match serde_json::from_str::<serde_json::Value>(&variables) {
                        Ok(_) => "Variables JSON is valid.".to_string(),
                        Err(error) => {
                            format!("Invalid variables JSON: {error}. Fix it before sending.")
                        }
                    }
                };
                div()
                    .id("workbench-graphql-editor")
                    .debug_selector(|| "workbench-graphql-editor".into())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Query")
                    .child(Textarea::new(&self.query))
                    .child("Variables (JSON)")
                    .child(Textarea::new(&self.variables))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(message),
                    )
                    .into_any_element()
            }
            _ => Textarea::new(&self.source).into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_round_trip_keeps_templates_incomplete_json_and_query_escapes() {
        let query = "query User($id: ID!) { user(id: $id) { name } }\n# \"quoted\"";
        for variables in ["{\"id\": {{$randomInt}}}", "{\"id\":", "", "{\"id\": 3}"] {
            let source = encode_graphql(query, variables);
            let decoded = decode_graphql(&source);
            assert_eq!(decoded.query, query);
            if serde_json::from_str::<serde_json::Value>(variables).is_ok() {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&decoded.variables).unwrap(),
                    serde_json::from_str::<serde_json::Value>(variables).unwrap()
                );
            } else {
                assert_eq!(decoded.variables, variables);
            }
        }
    }

    #[test]
    fn graphql_keeps_raw_queries_and_unknown_envelope_fields() {
        let query = "{ users { id } }";
        assert_eq!(decode_graphql(query).query, query);
        let draft = decode_graphql(
            r#"{"query":"query User { user { id } }","variables":{"id":3},"operationName":"User","extensions":{"key":"value"}}"#,
        );
        let encoded = encode_with_extra(&draft.query, "{\"id\": {{$randomInt}}}", &draft.extra);
        let decoded = decode_graphql(&encoded);
        assert_eq!(decoded.extra, draft.extra);
        assert_eq!(decoded.variables, "{\"id\": {{$randomInt}}}");
        assert_eq!(decoded.query, draft.query);
    }
}
