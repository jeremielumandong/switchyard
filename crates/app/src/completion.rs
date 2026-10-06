//! SQL completion for the editor: adapts `switchyard_db::complete` to gpui-component's
//! `CompletionProvider`.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::RopeExt as _;
use gpui_kit::component::input::{CompletionProvider, Rope};
use gpui_kit::{App, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    TextEdit,
};
use switchyard_core::db::complete::{CandidateKind, CatalogIndex, complete};
use switchyard_core::db::{Engine, dialect_for};

/// Completion state shared between a SQL tab and its editor.
#[derive(Default)]
pub struct CompletionState {
    /// Columns and tables of the bound connection.
    pub index: CatalogIndex,
    /// Engine of the bound connection.
    pub engine: Option<Engine>,
}

/// The provider installed on each SQL editor.
pub struct SqlCompletion {
    /// Shared state, updated when the catalog loads.
    pub state: Rc<RefCell<CompletionState>>,
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let typed = trigger.trigger_character.unwrap_or_default();
        tracing::debug!(offset, typed = %typed, "completion requested");
        let state = self.state.borrow();
        let dialect = dialect_for(state.engine.unwrap_or(Engine::Postgres));
        let script = text.to_string();
        // Replace exactly what was typed since completion started.
        let start = offset.saturating_sub(typed.len());
        let range = lsp_types::Range::new(
            text.offset_to_position(start),
            text.offset_to_position(offset),
        );
        let items: Vec<CompletionItem> = complete(dialect, &state.index, &script, offset, &typed)
            .into_iter()
            .map(|c| CompletionItem {
                label: c.label,
                kind: Some(match c.kind {
                    CandidateKind::Column => CompletionItemKind::FIELD,
                    CandidateKind::Table => CompletionItemKind::CLASS,
                    CandidateKind::Schema => CompletionItemKind::MODULE,
                    CandidateKind::Function => CompletionItemKind::FUNCTION,
                    CandidateKind::Keyword => CompletionItemKind::KEYWORD,
                }),
                detail: Some(c.detail),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, c.insert))),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        let mut chars = new_text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => c == '.' || c.is_alphanumeric() || c == '_',
            _ => false,
        }
    }
}
