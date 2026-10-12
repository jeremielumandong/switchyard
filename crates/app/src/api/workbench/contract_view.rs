//! The response pane's Contract tab: the shape of this request's JSON
//! responses learned from its saved examples and History
//! ([`switchyard_api::contract`]), and how the shown response drifts from it.
//! Nothing has to be written first; the contract is what the API has
//! returned before, and it copies out as JSON Schema.

use base64::Engine;
use gpui_kit::{AnyElement, div, prelude::*};
use switchyard_api::contract::{Drift, FieldSummary, Shape};

use super::view::{Col, cell, header_cells, heading, note_card, table, table_row, tinted_cell};
use super::*;

/// Earlier responses learned from (the most recent ones).
const MAX_SAMPLES: usize = 30;
/// Larger bodies are skipped: they are slow to parse and rarely the norm.
const MAX_SAMPLE_BYTES: usize = 2 * 1024 * 1024;
/// Rows of the learned field table.
const MAX_FIELD_ROWS: usize = 300;

/// What the Contract tab shows for the current response.
#[derive(Clone, Debug, Default)]
pub(super) struct ContractView {
    pub status: u16,
    /// Earlier responses the contract was learned from.
    pub samples: usize,
    /// Earlier responses with exactly this body (not learned from).
    pub identical: usize,
    pub drifts: Vec<Drift>,
    pub fields: Vec<FieldSummary>,
    pub schema: String,
    /// Why there is no contract to compare with, when there is none.
    pub note: Option<String>,
}

/// Learn from `earlier` bodies and check `current` (all raw bytes).
pub(super) fn analyze(status: u16, current: &[u8], earlier: &[Vec<u8>]) -> ContractView {
    let mut view = ContractView {
        status,
        ..ContractView::default()
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(current) else {
        view.note = Some("This response is not JSON, so it has no contract to check.".into());
        return view;
    };
    let mut learned = Vec::new();
    for body in earlier {
        if body.as_slice() == current {
            view.identical += 1;
        } else if let Ok(sample) = serde_json::from_slice::<serde_json::Value>(body) {
            learned.push(sample);
        }
    }
    view.samples = learned.len();
    if learned.is_empty() {
        view.note = Some(if view.identical > 0 {
            format!(
                "Identical to {} earlier {status} response{}; send again after the API \
                 changes, or save examples, to see drift.",
                view.identical,
                if view.identical == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "No earlier {status} JSON responses for this request yet. Each send and \
                 saved example teaches the contract; drift shows from the next one on."
            )
        });
        let shape = Shape::of(&value);
        view.fields = shape.fields();
        view.schema = serde_json::to_string_pretty(&shape.json_schema()).unwrap_or_default();
        return view;
    }
    let shape = Shape::learn(learned.iter());
    view.drifts = shape.check(&value);
    view.fields = shape.fields();
    view.schema = serde_json::to_string_pretty(&shape.json_schema()).unwrap_or_default();
    view
}

impl WorkbenchPanel {
    /// Recompute the Contract tab for the shown response off the frame thread.
    pub(super) fn refresh_contract(&mut self, cx: &mut Context<Self>) {
        self.contract = None;
        self._contract_work = None;
        let (Some(response), Some(request_id)) =
            (self.response.clone(), self.current_request_id.clone())
        else {
            return;
        };
        if response.binary || response.truncated {
            self.contract = Some(ContractView {
                status: response.status,
                note: Some("Only complete JSON responses have a contract.".into()),
                ..ContractView::default()
            });
            return;
        }
        let status = response.status;
        let decode = |encoded: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .ok()
        };
        let mut earlier: Vec<String> = Vec::new();
        for entry in &self.history {
            if earlier.len() >= MAX_SAMPLES {
                break;
            }
            let exchange = &entry.exchange;
            if exchange.request_id.as_ref() != Some(&request_id) {
                continue;
            }
            if let Some(snapshot) = exchange.response.as_ref().filter(|snapshot| {
                snapshot.status == status
                    && !snapshot.truncated
                    && snapshot.body_base64.len() <= MAX_SAMPLE_BYTES * 4 / 3 + 4
            }) {
                earlier.push(snapshot.body_base64.clone());
            }
        }
        if let Some(data) = self.workspace_data.as_ref() {
            for example in data.examples.iter().filter(|example| {
                example.request_id == request_id
                    && example.response.status == status
                    && !example.response.truncated
            }) {
                earlier.push(example.response.body_base64.clone());
            }
        }
        let generation = self.response_body_generation;
        self._contract_work = Some(cx.spawn(async move |this, cx| {
            let view = crate::api::compat::blocking(move || {
                let earlier: Vec<Vec<u8>> = earlier.iter().filter_map(|b| decode(b)).collect();
                analyze(status, response.body.as_bytes(), &earlier)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.response_body_generation == generation {
                    panel.contract = Some(view);
                    panel._contract_work = None;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn render_contract(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let Some(view) = self.contract.as_ref() else {
            let text = if self.current_request_id.is_none() {
                "Save the request: its contract is learned from its earlier responses."
            } else {
                "Learning the contract…"
            };
            return div()
                .text_size(text::S11)
                .text_color(palette::text_tertiary(cx))
                .child(text)
                .into_any_element();
        };
        let breaking = view.drifts.iter().filter(|d| d.kind.is_breaking()).count();
        let added = view.drifts.len() - breaking;
        let summary = match &view.note {
            Some(note) => note_card(palette::text_tertiary(cx), Some("info"), note.clone(), cx),
            None if view.drifts.is_empty() => note_card(
                colors.success,
                Some("check"),
                format!(
                    "Matches the contract learned from {} earlier {} response{}.",
                    view.samples,
                    view.status,
                    if view.samples == 1 { "" } else { "s" }
                ),
                cx,
            ),
            None => note_card(
                if breaking > 0 {
                    colors.danger
                } else {
                    colors.info
                },
                Some("alert"),
                format!(
                    "{breaking} breaking change{} and {added} new field{} against {} earlier {} response{}.",
                    if breaking == 1 { "" } else { "s" },
                    if added == 1 { "" } else { "s" },
                    view.samples,
                    view.status,
                    if view.samples == 1 { "" } else { "s" }
                ),
                cx,
            ),
        };
        let drift_cols = [Col::Px(96.), Col::Flex, Col::Flex];
        let drifts = (!view.drifts.is_empty()).then(|| {
            table(cx)
                .debug_selector(|| "workbench-contract-drifts".into())
                .child(table_row(
                    &drift_cols,
                    header_cells(&["CHANGE", "PATH", "DETAIL"]),
                    true,
                    cx,
                ))
                .children(view.drifts.iter().map(|drift| {
                    use switchyard_api::contract::DriftKind;
                    let (label, tint, detail) = match &drift.kind {
                        DriftKind::Removed { was } => {
                            ("removed", colors.danger, format!("was {was}"))
                        }
                        DriftKind::TypeChanged { was, now } => {
                            ("type", colors.danger, format!("{was} → {now}"))
                        }
                        DriftKind::Added { now } => ("new", colors.info, now.clone()),
                    };
                    table_row(
                        &drift_cols,
                        vec![
                            tinted_cell(label, tint),
                            cell(drift.path.clone()),
                            cell(detail),
                        ],
                        false,
                        cx,
                    )
                }))
        });
        let field_cols = [Col::Flex, Col::Px(180.), Col::Px(80.)];
        let fields = table(cx)
            .debug_selector(|| "workbench-contract-fields".into())
            .child(table_row(
                &field_cols,
                header_cells(&["PATH", "TYPE", "REQUIRED"]),
                true,
                cx,
            ))
            .children(view.fields.iter().take(MAX_FIELD_ROWS).map(|field| {
                table_row(
                    &field_cols,
                    vec![
                        cell(field.path.clone()),
                        cell(field.types.clone()),
                        if field.required {
                            tinted_cell("✓", colors.success)
                        } else {
                            tinted_cell("optional", palette::text_tertiary(cx))
                        },
                    ],
                    false,
                    cx,
                )
            }));
        div()
            .flex()
            .flex_col()
            .gap(space::SP_3)
            .child(summary)
            .children(drifts)
            .when(!view.fields.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::SP_2)
                        .child(heading(
                            format!("Learned shape · {} fields", view.fields.len()),
                            colors.muted_foreground,
                        ))
                        .child(div().flex_1())
                        .child(self.copy_chip(
                            "workbench-contract-copy-schema",
                            "Copy JSON Schema",
                            !view.schema.is_empty(),
                            |panel, _| {
                                panel
                                    .contract
                                    .as_ref()
                                    .map(|view| view.schema.clone())
                                    .unwrap_or_default()
                            },
                            cx,
                        )),
                )
                .child(fields)
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::analyze;

    #[test]
    fn drift_is_measured_against_earlier_different_bodies() {
        let earlier = vec![
            br#"{"id":1,"name":"a"}"#.to_vec(),
            br#"{"id":2,"name":"b"}"#.to_vec(),
            br#"{"id":3}"#.to_vec(),
        ];
        let view = analyze(200, br#"{"id":"4","name":"c"}"#, &earlier);
        assert_eq!(view.samples, 3);
        assert_eq!(view.drifts.len(), 1, "{:?}", view.drifts);
        assert_eq!(
            view.drifts[0].describe(),
            "$.id: type changed from integer to string"
        );
        assert!(view.schema.contains("\"required\""));
    }

    #[test]
    fn identical_and_missing_history_explain_themselves() {
        let body = br#"{"ok":true}"#;
        let view = analyze(200, body, &[body.to_vec()]);
        assert_eq!(view.identical, 1);
        assert!(
            view.note
                .unwrap()
                .starts_with("Identical to 1 earlier 200 response")
        );
        let view = analyze(201, body, &[]);
        assert!(view.note.unwrap().contains("No earlier 201"));
        assert!(
            !view.fields.is_empty(),
            "the shape of this response still shows"
        );
        assert!(
            analyze(200, b"<html/>", &[])
                .note
                .unwrap()
                .contains("not JSON")
        );
    }
}
